//! Persisted scan baselines and diffing — "what grew since the last scan?".
//!
//! `save` records the immediate children of a path (each sized recursively via
//! [`crate::bulkstat`]) together with a timestamp, keyed by absolute path in a
//! single JSON file. `diff` re-scans the path now and compares against the saved
//! baseline. Diffing is kept pure ([`diff_records`]) so it can be tested without
//! touching the filesystem.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    bulkstat::{self, SizeInfo},
    state,
};

pub(crate) const HISTORY_MAX_RECORDS: usize = 512;

/// One immediate child of a scanned directory, with its recursive size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildSize {
    pub name: String,
    pub is_dir: bool,
    pub size: SizeInfo,
    /// Directories under this child that could not be read while sizing it;
    /// when non-zero the recorded size is a lower bound.
    pub inaccessible: u32,
    pub skipped_mounts: u32,
}

/// A saved scan of a directory's immediate children at a point in time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanRecord {
    pub path: PathBuf,
    /// Seconds since the Unix epoch when the baseline was captured.
    pub timestamp: u64,
    pub generation: u64,
    /// Immediate entries that could not be enumerated or inspected.
    pub enumeration_errors: u32,
    pub children: Vec<ChildSize>,
}

impl ScanRecord {
    pub fn skipped_mounts(&self) -> u32 {
        self.children
            .iter()
            .fold(0u32, |acc, c| acc.saturating_add(c.skipped_mounts))
    }
    pub fn total(&self) -> SizeInfo {
        let mut total = SizeInfo::default();
        for child in &self.children {
            total.logical = total.logical.saturating_add(child.size.logical);
            total.allocated = total.allocated.saturating_add(child.size.allocated);
        }
        total
    }

    /// Total directories that were unreadable across all children; non-zero
    /// means the recorded sizes are lower bounds.
    pub fn inaccessible(&self) -> u32 {
        self.children
            .iter()
            .fold(self.enumeration_errors, |acc, child| {
                acc.saturating_add(child.inaccessible)
            })
    }
}

/// A single child's change between two scans. Either side may be `None` when the
/// child only exists in one of the scans (added or removed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildChange {
    pub name: String,
    pub before: Option<SizeInfo>,
    pub after: Option<SizeInfo>,
    pub before_inaccessible: u32,
    pub after_inaccessible: u32,
    pub before_skipped_mounts: u32,
    pub after_skipped_mounts: u32,
    pub before_is_dir: Option<bool>,
    pub after_is_dir: Option<bool>,
}

impl ChildChange {
    pub fn complete(&self) -> bool {
        self.before_inaccessible == 0
            && self.after_inaccessible == 0
            && self.before_skipped_mounts == 0
            && self.after_skipped_mounts == 0
    }
    pub fn delta_allocated(&self) -> i128 {
        i128::from(self.after.map(|s| s.allocated).unwrap_or(0))
            - i128::from(self.before.map(|s| s.allocated).unwrap_or(0))
    }

    pub fn delta_logical(&self) -> i128 {
        i128::from(self.after.map(|s| s.logical).unwrap_or(0))
            - i128::from(self.before.map(|s| s.logical).unwrap_or(0))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffReport {
    pub path: PathBuf,
    pub baseline_timestamp: u64,
    pub current_timestamp: u64,
    pub before_total: SizeInfo,
    pub after_total: SizeInfo,
    /// Unreadable-directory counts in the baseline and the current scan; either
    /// being non-zero means the totals and deltas are lower bounds.
    pub baseline_inaccessible: u32,
    pub current_inaccessible: u32,
    pub baseline_skipped_mounts: u32,
    pub current_skipped_mounts: u32,
    /// Children whose size changed, plus additions and removals, sorted by the
    /// magnitude of the allocated-size delta (largest movers first).
    pub changes: Vec<ChildChange>,
}

impl DiffReport {
    pub fn complete(&self) -> bool {
        self.baseline_inaccessible == 0
            && self.current_inaccessible == 0
            && self.baseline_skipped_mounts == 0
            && self.current_skipped_mounts == 0
    }
    pub fn total_delta_allocated(&self) -> i128 {
        i128::from(self.after_total.allocated) - i128::from(self.before_total.allocated)
    }

    pub fn total_delta_logical(&self) -> i128 {
        i128::from(self.after_total.logical) - i128::from(self.before_total.logical)
    }
}

/// Scan `path` and persist the result as the new baseline, returning the record.
pub fn save(path: &Path) -> Result<ScanRecord> {
    validate_dir(path)?;
    let record = scan_record(path)?;
    if !store_record(&record)? {
        bail!("a newer baseline already exists; this scan was not saved");
    }
    Ok(record)
}

/// Compare a fresh scan of `path` against the saved baseline.
pub fn diff(path: &Path) -> Result<DiffReport> {
    validate_dir(path)?;
    let Some(baseline) = load_record_for_path(path)? else {
        bail!(
            "no saved baseline for {}; run `diskr --save {}` first",
            path.display(),
            path.display()
        );
    };
    diff_from_record(&baseline, path)
}

/// Compare a fresh scan of `path` against an already-loaded baseline.
pub fn diff_from_record(baseline: &ScanRecord, path: &Path) -> Result<DiffReport> {
    validate_dir(path)?;
    let current = scan_record(path)?;
    Ok(diff_records(baseline, &current))
}

/// Like [`diff_from_record`], but the background scan aborts early when
/// `cancellation` fires, returning `Ok(None)`. Used by the TUI so a diff worker
/// for a directory the user already navigated away from stops walking instead
/// of running to completion only to be discarded.
pub fn diff_from_record_with_cancellation(
    baseline: &ScanRecord,
    path: &Path,
    cancellation: &bulkstat::ScanCancellation,
) -> Result<Option<DiffReport>> {
    validate_dir(path)?;
    let Some(current) = scan_record_with_cancellation(path, cancellation)? else {
        return Ok(None);
    };
    Ok(Some(diff_records(baseline, &current)))
}

/// Pure diff of two scan records. `before`/`after` need not be sorted.
pub fn diff_records(before: &ScanRecord, after: &ScanRecord) -> DiffReport {
    let mut changes: Vec<ChildChange> = Vec::new();
    let before_by_name: HashMap<&str, &ChildSize> = before
        .children
        .iter()
        .map(|child| (child.name.as_str(), child))
        .collect();
    let after_names: std::collections::HashSet<&str> = after
        .children
        .iter()
        .map(|child| child.name.as_str())
        .collect();

    for child in &after.children {
        let prior = before_by_name.get(child.name.as_str()).copied();
        let change = ChildChange {
            name: child.name.clone(),
            before: prior.map(|c| c.size),
            after: Some(child.size),
            before_inaccessible: prior
                .map(|c| c.inaccessible)
                .unwrap_or(before.enumeration_errors),
            after_inaccessible: child.inaccessible,
            before_skipped_mounts: prior.map(|c| c.skipped_mounts).unwrap_or(0),
            after_skipped_mounts: child.skipped_mounts,
            before_is_dir: prior.map(|c| c.is_dir),
            after_is_dir: Some(child.is_dir),
        };
        if prior.is_none()
            || change.before_is_dir != change.after_is_dir
            || change.before_inaccessible != change.after_inaccessible
            || change.before_skipped_mounts != change.after_skipped_mounts
            || change.delta_allocated() != 0
            || change.delta_logical() != 0
        {
            changes.push(change);
        }
    }

    // Removed children: present before, absent now.
    for child in &before.children {
        if !after_names.contains(child.name.as_str()) {
            changes.push(ChildChange {
                name: child.name.clone(),
                before: Some(child.size),
                after: None,
                before_inaccessible: child.inaccessible,
                after_inaccessible: after.enumeration_errors,
                before_skipped_mounts: child.skipped_mounts,
                after_skipped_mounts: 0,
                before_is_dir: Some(child.is_dir),
                after_is_dir: None,
            });
        }
    }

    changes.sort_by(|a, b| {
        b.delta_allocated()
            .abs()
            .cmp(&a.delta_allocated().abs())
            .then(a.name.cmp(&b.name))
    });

    DiffReport {
        path: after.path.clone(),
        baseline_timestamp: before.timestamp,
        current_timestamp: after.timestamp,
        before_total: before.total(),
        after_total: after.total(),
        baseline_inaccessible: before.inaccessible(),
        current_inaccessible: after.inaccessible(),
        baseline_skipped_mounts: before.skipped_mounts(),
        current_skipped_mounts: after.skipped_mounts(),
        changes,
    }
}

fn validate_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("path does not exist: {}", path.display());
    }
    if !path.is_dir() {
        bail!("path is not a directory: {}", path.display());
    }
    Ok(())
}

pub(crate) fn scan_record(path: &Path) -> Result<ScanRecord> {
    let never = bulkstat::ScanCancellation::never();
    // `never()` cannot fire, so the cancellable scan always yields `Some`.
    Ok(
        scan_record_with_cancellation(path, &never)?.unwrap_or_else(|| ScanRecord {
            path: path.to_path_buf(),
            timestamp: now_secs(),
            generation: state::next_epoch(),
            enumeration_errors: 0,
            children: Vec::new(),
        }),
    )
}

pub(crate) fn scan_record_with_cancellation(
    path: &Path,
    cancellation: &bulkstat::ScanCancellation,
) -> Result<Option<ScanRecord>> {
    validate_dir(path)?;
    if cancellation.is_cancelled() {
        return Ok(None);
    }
    let timestamp = now_secs();
    let generation = state::next_epoch();
    let canonical = path
        .canonicalize()
        .with_context(|| format!("resolve {}", path.display()))?;
    let scope = bulkstat::ScanScope::new(&canonical)?;
    let mut children = Vec::new();
    let mut enumeration_errors = 0u32;
    let read =
        std::fs::read_dir(&canonical).with_context(|| format!("read {}", canonical.display()))?;
    let mut read: Vec<_> = read.collect();
    read.sort_by_key(|entry| entry.as_ref().ok().map(|e| e.file_name()));
    for entry in read {
        if cancellation.is_cancelled() {
            return Ok(None);
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                enumeration_errors = enumeration_errors.saturating_add(1);
                continue;
            }
        };
        let name = state::encode_path(Path::new(&entry.file_name()));
        let meta = match std::fs::symlink_metadata(entry.path()) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                enumeration_errors = enumeration_errors.saturating_add(1);
                continue;
            }
        };
        let file_type = meta.file_type();
        if file_type.is_symlink() {
            continue;
        }
        let (is_dir, size, inaccessible, skipped_mounts) = if !scope.accepts(&entry.path(), &meta) {
            (file_type.is_dir(), SizeInfo::default(), 0, 1)
        } else if file_type.is_dir() {
            let Some(scan) =
                bulkstat::scan_dir_in_scope(&entry.path(), 0, cancellation, Some(&scope))
            else {
                return Ok(None);
            };
            (true, scan.size, scan.inaccessible, scan.skipped_mounts)
        } else if file_type.is_file() {
            (false, scope.file_size(&meta), 0, 0)
        } else {
            continue;
        };
        children.push(ChildSize {
            name,
            is_dir,
            size,
            inaccessible,
            skipped_mounts,
        });
    }
    children.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Some(ScanRecord {
        path: canonical,
        timestamp,
        generation,
        enumeration_errors,
        children,
    }))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn history_file() -> PathBuf {
    state::state_dir().join("history.json")
}

fn load_history_from_path(path: &Path) -> Result<serde_json::Map<String, serde_json::Value>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(serde_json::Map::new()),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parse {} (delete it to reset history)", path.display()))?;
    match value {
        serde_json::Value::Object(mut map) => {
            if let Some(version) = map.remove("version") {
                if version.as_u64() != Some(1) {
                    bail!("unsupported history schema version");
                }
                return map
                    .remove("records")
                    .and_then(|v| v.as_object().cloned())
                    .context("history schema missing records");
            }
            Ok(map)
        }
        _ => bail!(
            "unexpected history format in {} (delete it to reset history)",
            path.display()
        ),
    }
}

pub fn load_records() -> Result<HashMap<PathBuf, ScanRecord>> {
    load_records_from_path(&history_file())
}

/// Like [`load_records`], but converts a load failure (a corrupt or
/// unreadable `history.json`) into a user-facing warning string instead of
/// silently presenting "no baselines", mirroring how the size cache surfaces
/// its own load errors.
pub fn load_records_with_warning() -> (HashMap<PathBuf, ScanRecord>, Option<String>) {
    match load_records_report(&history_file()) {
        Ok((records, rejected)) => (
            records,
            (rejected > 0).then(|| {
                format!(
                    "history ignored {rejected} invalid baseline records; valid records retained"
                )
            }),
        ),
        Err(err) => (HashMap::new(), Some(format!("history load failed: {err}"))),
    }
}

fn load_records_from_path(path: &Path) -> Result<HashMap<PathBuf, ScanRecord>> {
    Ok(load_records_report(path)?.0)
}

fn load_records_report(path: &Path) -> Result<(HashMap<PathBuf, ScanRecord>, usize)> {
    let history = load_history_from_path(path)?;
    let mut records = HashMap::new();
    let mut rejected = 0;
    for (path, value) in history {
        let parsed = (|| -> Result<(PathBuf, ScanRecord)> {
            let path = state::decode_path(&path).context("invalid baseline path encoding")?;
            if !path.is_absolute() {
                bail!("baseline path is not absolute");
            }
            let record = record_from_json(&path, &value)?;
            Ok((path, record))
        })();
        match parsed {
            Ok((path, record)) => {
                records.insert(path, record);
            }
            Err(_) => {
                rejected += 1;
            }
        }
    }
    prune_records(&mut records);
    Ok((records, rejected))
}

/// Load the saved baseline for a path if one exists.
pub fn load_record_for_path(path: &Path) -> Result<Option<ScanRecord>> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("resolve {}", path.display()))?;
    let history = load_records()?;
    Ok(history.get(&canonical).cloned())
}

pub(crate) fn store_record(record: &ScanRecord) -> Result<bool> {
    let dir = state::state_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = history_file();
    store_record_to_path(&path, record)
}

pub(crate) fn store_record_to_path(path: &Path, record: &ScanRecord) -> Result<bool> {
    store_record_guarded(path, record, &bulkstat::ScanCancellation::never())
}

pub(crate) fn store_record_cancellable(
    record: &ScanRecord,
    cancellation: &bulkstat::ScanCancellation,
    override_path: Option<&Path>,
) -> Result<bool> {
    store_record_guarded(
        override_path.unwrap_or(&history_file()),
        record,
        cancellation,
    )
}

fn store_record_guarded(
    path: &Path,
    record: &ScanRecord,
    cancellation: &bulkstat::ScanCancellation,
) -> Result<bool> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    state::with_exclusive_lock(path, || {
        if cancellation.is_cancelled() {
            return Ok(false);
        }
        let mut records = match load_records_report(path) {
            Ok((records, 0)) => records,
            Ok((records, _)) => {
                state::backup_corrupt_state(path)?;
                records
            }
            Err(_) => {
                state::backup_corrupt_state(path)?;
                HashMap::new()
            }
        };
        if records.get(&record.path).is_some_and(|old| {
            (old.timestamp, old.generation) > (record.timestamp, record.generation)
        }) {
            return Ok(false);
        }
        records.insert(record.path.clone(), record.clone());
        prune_records(&mut records);
        store_records_to_path(path, &records)?;
        Ok(true)
    })
}

pub(crate) fn prune_records(records: &mut HashMap<PathBuf, ScanRecord>) -> bool {
    if records.len() <= HISTORY_MAX_RECORDS {
        return false;
    }
    let mut ordered: Vec<_> = records.drain().collect();
    ordered.sort_by(|(path_a, a), (path_b, b)| {
        b.timestamp
            .cmp(&a.timestamp)
            .then_with(|| path_a.cmp(path_b))
    });
    ordered.truncate(HISTORY_MAX_RECORDS);
    records.extend(ordered);
    true
}

fn store_records_to_path(path: &Path, records: &HashMap<PathBuf, ScanRecord>) -> Result<()> {
    let mut ordered: Vec<_> = records.iter().collect();
    ordered.sort_by(|(path_a, a), (path_b, b)| {
        b.timestamp
            .cmp(&a.timestamp)
            .then_with(|| path_a.cmp(path_b))
    });
    let mut history = serde_json::Map::with_capacity(ordered.len());
    for (path, record) in ordered {
        history.insert(state::encode_path(path), record_to_json(record));
    }
    let text =
        serde_json::to_string_pretty(&serde_json::json!({"version": 1, "records": history}))?;
    state::atomic_write(path, &text)
}

fn record_to_json(record: &ScanRecord) -> serde_json::Value {
    let children: Vec<serde_json::Value> = record
        .children
        .iter()
        .map(|child| {
            serde_json::json!({
                "name": child.name,
                "is_dir": child.is_dir,
                "logical": child.size.logical,
                "allocated": child.size.allocated,
                "inaccessible": child.inaccessible,
                "skipped_mounts": child.skipped_mounts,
            })
        })
        .collect();
    serde_json::json!({
        "path": state::encode_path(&record.path),
        "timestamp": record.timestamp,
        "generation": record.generation,
        "enumeration_errors": record.enumeration_errors,
        "children": children,
    })
}

fn record_from_json(path: &Path, value: &serde_json::Value) -> Result<ScanRecord> {
    let timestamp = value
        .get("timestamp")
        .and_then(|v| v.as_u64())
        .context("baseline missing valid timestamp")?;
    let children = value
        .get("children")
        .and_then(|v| v.as_array())
        .context("baseline missing children")?
        .iter()
        .map(|item| -> Result<ChildSize> {
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .context("child missing name")?
                .to_string();
            if name.is_empty()
                || state::decode_path(&name).is_none_or(|p| {
                    p.components().count() != 1
                        || !matches!(p.components().next(), Some(std::path::Component::Normal(_)))
                })
            {
                bail!("invalid baseline child name");
            }
            let is_dir = item
                .get("is_dir")
                .and_then(|v| v.as_bool())
                .context("child missing type")?;
            let logical = item
                .get("logical")
                .and_then(|v| v.as_u64())
                .context("child missing logical bytes")?;
            let allocated = item
                .get("allocated")
                .and_then(|v| v.as_u64())
                .context("child missing allocated bytes")?;
            let inaccessible = optional_count(item, "inaccessible")?;
            let skipped_mounts = optional_count(item, "skipped_mounts")?;
            Ok(ChildSize {
                name,
                is_dir,
                size: SizeInfo::new(logical, allocated),
                inaccessible,
                skipped_mounts,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut names = std::collections::HashSet::new();
    if children.iter().any(|c| !names.insert(&c.name)) {
        bail!("duplicate baseline child name");
    }
    Ok(ScanRecord {
        path: path.to_path_buf(),
        timestamp,
        generation: match value.get("generation") {
            None => 0,
            Some(v) => v.as_u64().context("invalid baseline generation")?,
        },
        enumeration_errors: optional_count(value, "enumeration_errors")?,
        children,
    })
}

fn optional_count(value: &serde_json::Value, key: &str) -> Result<u32> {
    match value.get(key) {
        None => Ok(0),
        Some(v) => v
            .as_u64()
            .and_then(|n| n.try_into().ok())
            .with_context(|| format!("invalid {key}")),
    }
}

const LIBRARY_MAX_BYTES: u64 = 16 * 1024 * 1024;
const LIBRARY_MAX_BASELINES: usize = 2048;
const LIBRARY_MAX_PROFILES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedBaseline {
    pub machine: String,
    pub pinned: bool,
    pub record: ScanRecord,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanProfile {
    pub machine: String,
    pub root: PathBuf,
    pub scope: crate::packages::InventoryScope,
}

#[derive(Default)]
struct BaselineLibrary {
    machine: String,
    baselines: std::collections::BTreeMap<String, NamedBaseline>,
    profiles: std::collections::BTreeMap<String, ScanProfile>,
}

pub enum LibraryAction {
    List,
    Pin {
        name: String,
        pinned: bool,
    },
    RemoveProfile {
        name: String,
    },
    Prune {
        keep: usize,
    },
    Export(PathBuf),
    Import(PathBuf),
    SaveProfile {
        name: String,
        root: PathBuf,
        scope: crate::packages::InventoryScope,
    },
}

fn library_file() -> PathBuf {
    state::state_dir().join("baseline-library.json")
}

pub fn validate_library_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 80
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        bail!("names require 1-80 ASCII letters, digits, dots, underscores or hyphens");
    }
    Ok(())
}

fn machine_id(value: &serde_json::Value) -> Result<String> {
    let machine = value.as_str().context("library missing machine identity")?;
    if machine.is_empty()
        || machine.len() > 128
        || !machine
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
    {
        bail!("invalid library machine identity");
    }
    Ok(machine.to_owned())
}

fn library_root(value: &serde_json::Value) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let root = value
        .as_str()
        .and_then(state::decode_path)
        .context("invalid library root identity")?;
    if !root.is_absolute() || root.as_os_str().as_bytes().contains(&0) {
        bail!("library root must be an absolute Unix path");
    }
    Ok(root)
}

fn library_from_json(value: &serde_json::Value) -> Result<BaselineLibrary> {
    if value["version"].as_u64() != Some(1) {
        bail!("unsupported baseline library version");
    }
    let mut library = BaselineLibrary {
        machine: machine_id(&value["machine"])?,
        ..Default::default()
    };
    let baselines = value["baselines"]
        .as_object()
        .context("library missing baselines")?;
    let profiles = value["profiles"]
        .as_object()
        .context("library missing profiles")?;
    if baselines.len() > LIBRARY_MAX_BASELINES || profiles.len() > LIBRARY_MAX_PROFILES {
        bail!("baseline library capacity exceeded");
    }
    for (name, item) in baselines {
        validate_library_name(name)?;
        let root = library_root(&item["record"]["path"])?;
        library.baselines.insert(
            name.clone(),
            NamedBaseline {
                machine: machine_id(&item["machine"])?,
                pinned: item["pinned"]
                    .as_bool()
                    .context("baseline missing pinned state")?,
                record: record_from_json(&root, &item["record"])?,
            },
        );
    }
    for (name, item) in profiles {
        validate_library_name(name)?;
        let settings = &item["scope"];
        let depth = settings["max_depth"]
            .as_u64()
            .filter(|depth| *depth <= 64)
            .context("profile depth must be 0-64")? as usize;
        let mut managers = Vec::new();
        for key in settings["managers"]
            .as_array()
            .context("profile missing managers")?
        {
            let manager = key
                .as_str()
                .and_then(crate::packages::Manager::from_key)
                .context("unknown profile manager")?;
            if managers.contains(&manager) {
                bail!("duplicate profile manager");
            }
            managers.push(manager);
        }
        let mut excludes = Vec::new();
        for value in settings["exclude_identities"]
            .as_array()
            .context("profile missing exclusions")?
        {
            let excluded = value
                .as_str()
                .and_then(state::decode_path)
                .context("invalid profile exclusion identity")?;
            if excluded.as_os_str().is_empty()
                || excluded.as_os_str().as_bytes().contains(&0)
                || excluded.components().any(|component| {
                    matches!(
                        component,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                })
            {
                bail!("profile exclusions must remain root-relative without ..");
            }
            excludes.push(excluded);
        }
        library.profiles.insert(
            name.clone(),
            ScanProfile {
                machine: machine_id(&item["machine"])?,
                root: library_root(&item["root_identity"])?,
                scope: crate::packages::InventoryScope {
                    managers,
                    projects: settings["projects"]
                        .as_bool()
                        .context("profile missing projects state")?,
                    depth,
                    excludes,
                },
            },
        );
    }
    Ok(library)
}

fn library_json(library: &BaselineLibrary) -> serde_json::Value {
    let baselines: serde_json::Map<_,_> = library.baselines.iter().map(|(name, baseline)|
        (name.clone(), serde_json::json!({"machine":baseline.machine,"pinned":baseline.pinned,"record":record_to_json(&baseline.record)}))).collect();
    let profiles: serde_json::Map<_,_> = library.profiles.iter().map(|(name, profile)|
        (name.clone(), serde_json::json!({"machine":profile.machine,"root_identity":state::encode_path(&profile.root),"scope":profile.scope.json()}))).collect();
    serde_json::json!({"version":1,"machine":library.machine,"baselines":baselines,"profiles":profiles})
}

fn read_library(path: &Path) -> Result<BaselineLibrary> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BaselineLibrary::default())
        }
        Err(error) => return Err(error).context("open baseline library"),
    };
    let mut bytes = Vec::new();
    file.take(LIBRARY_MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIBRARY_MAX_BYTES {
        bail!("baseline library exceeds 16MiB limit");
    }
    library_from_json(&serde_json::from_slice(&bytes).context("parse baseline library")?)
}

fn update_library<R>(
    path: &Path,
    update: impl FnOnce(&mut BaselineLibrary) -> Result<R>,
) -> Result<R> {
    use std::io::Read;
    state::with_exclusive_lock(path, || {
        let mut library = read_library(path)?;
        if library.machine.is_empty() {
            let mut random = [0u8; 16];
            std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
            library.machine = random.iter().map(|byte| format!("{byte:02x}")).collect();
        }
        let result = update(&mut library)?;
        if library.baselines.len() > LIBRARY_MAX_BASELINES
            || library.profiles.len() > LIBRARY_MAX_PROFILES
        {
            bail!("baseline library full; explicitly prune captures or remove obsolete profiles before adding records");
        }
        let text = serde_json::to_string_pretty(&library_json(&library))?;
        if text.len() as u64 > LIBRARY_MAX_BYTES {
            bail!("baseline library exceeds 16MiB limit; explicitly prune or export first");
        }
        state::atomic_write(path, &text)?;
        Ok(result)
    })
}

pub fn save_named_baseline(name: &str, record: &ScanRecord) -> Result<()> {
    save_named_baseline_to(&library_file(), name, record)
}

fn save_named_baseline_to(path: &Path, name: &str, record: &ScanRecord) -> Result<()> {
    validate_library_name(name)?;
    update_library(path, |library| {
        if library.baselines.contains_key(name) {
            bail!("baseline name already exists; choose a new name");
        }
        library.baselines.insert(
            name.to_owned(),
            NamedBaseline {
                machine: library.machine.clone(),
                pinned: false,
                record: record.clone(),
            },
        );
        Ok(())
    })
}

pub fn named_baseline(name: &str) -> Result<(NamedBaseline, bool)> {
    validate_library_name(name)?;
    let mut library = read_library(&library_file())?;
    let baseline = library
        .baselines
        .remove(name)
        .context("named baseline not found")?;
    let foreign = baseline.machine != library.machine;
    Ok((baseline, foreign))
}

pub fn scan_profile(name: &str) -> Result<ScanProfile> {
    validate_library_name(name)?;
    let mut library = read_library(&library_file())?;
    let profile = library
        .profiles
        .remove(name)
        .context("scan profile not found")?;
    if profile.machine != library.machine {
        bail!("imported profile belongs to another machine; save a new local profile with an explicit root and scope before running it");
    }
    Ok(profile)
}

pub fn scan_profiles() -> Result<Vec<(String, ScanProfile, bool)>> {
    let library = read_library(&library_file())?;
    Ok(library
        .profiles
        .into_iter()
        .map(|(name, profile)| {
            let foreign = profile.machine != library.machine;
            (name, profile, foreign)
        })
        .collect())
}

fn write_new_export(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        state::next_epoch()
    ));
    let temporary = PathBuf::from(temporary);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _cleanup = Cleanup(temporary.clone());
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    std::fs::hard_link(&temporary, path).context("export destination must not already exist")?;
    Ok(())
}

fn prune_names(library: &BaselineLibrary, keep: usize) -> Vec<String> {
    let mut candidates: Vec<_> = library
        .baselines
        .iter()
        .filter(|(_, baseline)| !baseline.pinned)
        .collect();
    candidates.sort_by(|(name_a, a), (name_b, b)| {
        b.record
            .timestamp
            .cmp(&a.record.timestamp)
            .then(b.record.generation.cmp(&a.record.generation))
            .then(name_a.cmp(name_b))
    });
    candidates
        .into_iter()
        .skip(keep)
        .map(|(name, _)| name.clone())
        .collect()
}

pub fn manage_library(action: LibraryAction, confirmed: bool) -> Result<serde_json::Value> {
    manage_library_at(&library_file(), action, confirmed)
}

fn manage_library_at(
    path: &Path,
    action: LibraryAction,
    confirmed: bool,
) -> Result<serde_json::Value> {
    match action {
        LibraryAction::List => {
            let library = read_library(path)?;
            let (latest, history_warning) = load_records_with_warning();
            let mut latest: Vec<_> = latest.into_values().collect();
            latest.sort_by(|left, right| left.path.cmp(&right.path));
            Ok(
                serde_json::json!({"library":path.to_string_lossy(),"local_machine":library.machine,
                "history_warning":history_warning,"latest_roots":latest.iter().map(|record| serde_json::json!({
                    "root":record.path.to_string_lossy(),"root_identity":state::encode_path(&record.path),"timestamp":record.timestamp,
                    "logical":record.total().logical,"allocated":record.total().allocated})).collect::<Vec<_>>(),
                "latest_root_retention":HISTORY_MAX_RECORDS,"named_retention":"explicit prune only; pinned records protected",
                "baseline_capacity":LIBRARY_MAX_BASELINES,"profile_capacity":LIBRARY_MAX_PROFILES,
                "baselines":library.baselines.iter().map(|(name, baseline)| serde_json::json!({"name":name,"machine":baseline.machine,
                    "pinned":baseline.pinned,"root":baseline.record.path.to_string_lossy(),"root_identity":state::encode_path(&baseline.record.path),
                    "timestamp":baseline.record.timestamp,"logical":baseline.record.total().logical,"allocated":baseline.record.total().allocated,
                    "complete":baseline.record.inaccessible()==0 && baseline.record.skipped_mounts()==0})).collect::<Vec<_>>(),
                "profiles":library.profiles.iter().map(|(name, profile)| serde_json::json!({"name":name,"machine":profile.machine,
                    "root":profile.root.to_string_lossy(),"root_identity":state::encode_path(&profile.root),"scope":profile.scope.json()})).collect::<Vec<_>>() }),
            )
        }
        LibraryAction::Pin { name, pinned } => {
            validate_library_name(&name)?;
            update_library(path, |library| {
                library
                    .baselines
                    .get_mut(&name)
                    .context("named baseline not found")?
                    .pinned = pinned;
                Ok(serde_json::json!({"name":name,"pinned":pinned}))
            })
        }
        LibraryAction::RemoveProfile { name } if !confirmed => {
            validate_library_name(&name)?;
            let library = read_library(path)?;
            let profile = library
                .profiles
                .get(&name)
                .context("scan profile not found")?;
            Ok(
                serde_json::json!({"dry_run":true,"would_remove_profile":name,"root_identity":state::encode_path(&profile.root),"note":"rerun with --yes; a recoverable backup is written first"}),
            )
        }
        LibraryAction::RemoveProfile { name } => {
            validate_library_name(&name)?;
            update_library(path, |library| {
                if !library.profiles.contains_key(&name) {
                    bail!("scan profile not found");
                }
                let backup = path.with_file_name(format!(
                    "baseline-library.profile-backup-{}.json",
                    state::next_epoch()
                ));
                write_new_export(
                    &backup,
                    &serde_json::to_string_pretty(&library_json(library))?,
                )?;
                library.profiles.remove(&name);
                Ok(
                    serde_json::json!({"dry_run":false,"removed_profile":name,"backup":backup.to_string_lossy()}),
                )
            })
        }
        LibraryAction::Prune { keep } if !confirmed => {
            let library = read_library(path)?;
            Ok(
                serde_json::json!({"dry_run":true,"keep_unpinned":keep,"would_remove":prune_names(&library,keep),"note":"all pinned baselines retained; rerun with --yes to apply"}),
            )
        }
        LibraryAction::Prune { keep } => update_library(path, |library| {
            let names = prune_names(library, keep);
            let backup = if names.is_empty() {
                None
            } else {
                let backup = path.with_file_name(format!(
                    "baseline-library.prune-backup-{}.json",
                    state::next_epoch()
                ));
                write_new_export(
                    &backup,
                    &serde_json::to_string_pretty(&library_json(library))?,
                )?;
                for name in &names {
                    library.baselines.remove(name);
                }
                Some(backup)
            };
            Ok(
                serde_json::json!({"dry_run":false,"removed":names,"backup":backup.map(|path| path.to_string_lossy().into_owned()),"pinned_retained":true}),
            )
        }),
        LibraryAction::Export(destination) => {
            let library = read_library(path)?;
            if library.machine.is_empty() {
                bail!("baseline library is empty; nothing to export");
            }
            write_new_export(
                &destination,
                &serde_json::to_string_pretty(&library_json(&library))?,
            )?;
            Ok(
                serde_json::json!({"exported":destination.to_string_lossy(),"baselines":library.baselines.len(),"profiles":library.profiles.len()}),
            )
        }
        LibraryAction::Import(source) => {
            if !source.is_file() {
                bail!("import source must be a library file");
            }
            let incoming = read_library(&source)?;
            update_library(path, |library| {
                for (name, baseline) in &incoming.baselines {
                    if library
                        .baselines
                        .get(name)
                        .is_some_and(|existing| existing != baseline)
                    {
                        bail!("conflicting imported baseline {name}; no changes applied");
                    }
                }
                for (name, profile) in &incoming.profiles {
                    if library
                        .profiles
                        .get(name)
                        .is_some_and(|existing| existing != profile)
                    {
                        bail!("conflicting imported profile {name}; no changes applied");
                    }
                }
                library.baselines.extend(incoming.baselines);
                library.profiles.extend(incoming.profiles);
                Ok(
                    serde_json::json!({"imported":source.to_string_lossy(),"baselines":library.baselines.len(),"profiles":library.profiles.len(),
                    "identity":"source machine and exact root retained; latest-root baselines unchanged"}),
                )
            })
        }
        LibraryAction::SaveProfile { name, root, scope } => {
            validate_library_name(&name)?;
            let root = root.canonicalize().context("resolve profile root")?;
            if !root.is_dir() {
                bail!("profile root must be a directory");
            }
            update_library(path, |library| {
                if library.profiles.contains_key(&name) {
                    bail!("profile name already exists; choose a new name");
                }
                let value = serde_json::json!({"saved_profile":name,"root":root.to_string_lossy(),"root_identity":state::encode_path(&root),"scope":scope.json()});
                library.profiles.insert(
                    name,
                    ScanProfile {
                        machine: library.machine.clone(),
                        root,
                        scope,
                    },
                );
                Ok(value)
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_library_import_and_invalid_profiles_preserve_the_existing_catalog() {
        let root = state::state_dir().join(format!("library-bounds-{}", state::next_epoch()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("library.json");
        save_named_baseline_to(&path, "retained", &record(Vec::new())).unwrap();
        let before = std::fs::read(&path).unwrap();
        let oversized = root.join("oversized.json");
        std::fs::File::create(&oversized)
            .unwrap()
            .set_len(LIBRARY_MAX_BYTES + 1)
            .unwrap();
        assert!(manage_library_at(&path, LibraryAction::Import(oversized), false).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let mut value = library_json(&read_library(&path).unwrap());
        let too_many: serde_json::Map<_, _> = (0..=LIBRARY_MAX_BASELINES)
            .map(|index| {
                (
                    format!("capture-{index}"),
                    value["baselines"]["retained"].clone(),
                )
            })
            .collect();
        value["baselines"] = serde_json::Value::Object(too_many);
        assert!(library_from_json(&value).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn named_library_pins_prunes_with_backup_and_imports_without_retargeting() {
        let root = state::state_dir().join(format!("library-lifecycle-{}", state::next_epoch()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("library.json");
        for (name, timestamp) in [("protected", 1), ("older", 2), ("newest", 3)] {
            let mut record = record(vec![child("file", timestamp)]);
            record.timestamp = timestamp;
            save_named_baseline_to(&path, name, &record).unwrap();
        }
        manage_library_at(
            &path,
            LibraryAction::Pin {
                name: String::from("protected"),
                pinned: true,
            },
            false,
        )
        .unwrap();
        let before = std::fs::read(&path).unwrap();
        let preview = manage_library_at(&path, LibraryAction::Prune { keep: 1 }, false).unwrap();
        assert_eq!(preview["would_remove"], serde_json::json!(["older"]));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let result = manage_library_at(&path, LibraryAction::Prune { keep: 1 }, true).unwrap();
        let backup = Path::new(result["backup"].as_str().unwrap());
        assert_eq!(read_library(backup).unwrap().baselines.len(), 3);
        let library = read_library(&path).unwrap();
        assert_eq!(library.baselines.len(), 2);
        assert!(library.baselines["protected"].pinned);
        assert!(save_named_baseline_to(&path, "protected", &record(Vec::new())).is_err());
        let destination = root.join("export.json");
        manage_library_at(&path, LibraryAction::Export(destination.clone()), false).unwrap();
        assert!(
            manage_library_at(&path, LibraryAction::Export(destination.clone()), false).is_err()
        );
        let imported = root.join("imported.json");
        manage_library_at(&imported, LibraryAction::Import(destination.clone()), false).unwrap();
        let restored = read_library(&imported).unwrap();
        assert_ne!(restored.machine, library.machine);
        assert_eq!(restored.baselines, library.baselines);
        manage_library_at(&imported, LibraryAction::Import(destination.clone()), false).unwrap();
        let conflict = root.join("conflict.json");
        save_named_baseline_to(&conflict, "protected", &record(Vec::new())).unwrap();
        let before = std::fs::read(&conflict).unwrap();
        assert!(manage_library_at(&conflict, LibraryAction::Import(destination), false).is_err());
        assert_eq!(std::fs::read(&conflict).unwrap(), before);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn library_round_trip_preserves_raw_roots_and_exclusion_identities_and_rejects_invalid_data() {
        use std::os::unix::ffi::OsStringExt;
        let raw = PathBuf::from(std::ffi::OsString::from_vec(b"/fixture/raw-\xff".to_vec()));
        let excluded = PathBuf::from(std::ffi::OsString::from_vec(b"raw-\xfe".to_vec()));
        let mut library = BaselineLibrary {
            machine: String::from("local-machine"),
            ..Default::default()
        };
        library.baselines.insert(
            String::from("raw"),
            NamedBaseline {
                machine: library.machine.clone(),
                pinned: true,
                record: ScanRecord {
                    path: raw.clone(),
                    timestamp: 1,
                    generation: 2,
                    enumeration_errors: 0,
                    children: Vec::new(),
                },
            },
        );
        library.profiles.insert(
            String::from("work"),
            ScanProfile {
                machine: library.machine.clone(),
                root: raw,
                scope: crate::packages::InventoryScope {
                    excludes: vec![excluded],
                    ..Default::default()
                },
            },
        );
        let value = library_json(&library);
        let restored = library_from_json(&value).unwrap();
        assert_eq!(restored.baselines, library.baselines);
        assert_eq!(restored.profiles, library.profiles);
        for mutate in [0, 1, 2, 3] {
            let mut malformed = value.clone();
            match mutate {
                0 => malformed["version"] = serde_json::json!(99),
                1 => {
                    malformed["baselines"]["raw"]["record"]["path"] = serde_json::json!("relative")
                }
                2 => malformed["profiles"]["work"]["scope"]["max_depth"] = serde_json::json!(65),
                _ => {
                    malformed["profiles"]["work"]["scope"]["exclude_identities"] =
                        serde_json::json!(["../outside"])
                }
            }
            assert!(library_from_json(&malformed).is_err());
        }
        assert!(validate_library_name("../outside").is_err());
        assert!(validate_library_name("bad\nname").is_err());
    }

    #[test]
    fn concurrent_named_writers_merge_under_the_library_lock() {
        let root = state::state_dir().join(format!("library-concurrent-{}", state::next_epoch()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("library.json");
        std::thread::scope(|scope| {
            for index in 0..12 {
                let path = &path;
                scope.spawn(move || {
                    save_named_baseline_to(path, &format!("capture-{index}"), &record(Vec::new()))
                        .unwrap()
                });
            }
        });
        assert_eq!(read_library(&path).unwrap().baselines.len(), 12);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn baseline_children_share_root_unique_hardlink_accounting() {
        let root = state::state_dir().join("hardlinked-baseline");
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::write(root.join("a/data"), vec![1u8; 8192]).unwrap();
        std::fs::hard_link(root.join("a/data"), root.join("b/link")).unwrap();
        let record = scan_record(&root).unwrap();
        assert_eq!(record.total(), bulkstat::scan_dir(&root, 0).size);
    }

    #[test]
    fn stale_same_path_save_is_rejected_inside_the_lock() {
        let path = state::state_dir().join("ordered-history.json");
        let mut newest = record(vec![]);
        newest.timestamp = 20;
        assert!(store_record_to_path(&path, &newest).unwrap());
        let mut older = newest.clone();
        older.timestamp = 10;
        assert!(!store_record_to_path(&path, &older).unwrap());
        assert_eq!(
            load_records_from_path(&path).unwrap().get(&newest.path),
            Some(&newest)
        );
    }

    #[test]
    fn diff_keeps_empty_additions_type_changes_and_per_child_uncertainty() {
        let mut unknown = child("uncertain", 12);
        unknown.inaccessible = 1;
        let before = record(vec![unknown, child("type", 0)]);
        let mut file = child("type", 0);
        file.is_dir = false;
        let after = record(vec![child("empty", 0), child("uncertain", 12), file]);
        let diff = diff_records(&before, &after);
        assert_eq!(diff.changes.len(), 3);
        assert!(diff
            .changes
            .iter()
            .any(|c| c.name == "empty" && c.before.is_none()));
        assert!(diff.changes.iter().any(|c| c.name == "uncertain"
            && c.before_inaccessible == 1
            && c.after_inaccessible == 0));
        assert!(diff
            .changes
            .iter()
            .any(|c| c.name == "type" && c.before_is_dir != c.after_is_dir));
    }

    #[test]
    fn malformed_history_records_are_not_zero_baselines() {
        for value in [
            serde_json::json!({}),
            serde_json::json!({"timestamp":1,"children":[{"name":"a","is_dir":false}]}),
            serde_json::json!({"timestamp":1,"children":[],"enumeration_errors":-1}),
        ] {
            assert!(record_from_json(Path::new("/tmp/test"), &value).is_err());
        }
    }

    #[test]
    fn history_retains_valid_records_and_backs_up_corrupt_entries() {
        let path = state::state_dir().join("mixed-history.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let valid = record(vec![]);
        let original = serde_json::json!({"/tmp/example": record_to_json(&valid), "/tmp/bad": {"timestamp": 10}}).to_string();
        std::fs::write(&path, &original).unwrap();
        let (loaded, rejected) = load_records_report(&path).unwrap();
        assert_eq!(rejected, 1);
        assert_eq!(loaded.len(), 1);
        assert!(store_record_to_path(&path, &valid).unwrap());
        let backups: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".corrupt-"))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(
            std::fs::read_to_string(backups[0].path()).unwrap(),
            original
        );
        assert_eq!(load_records_report(&path).unwrap().1, 0);
    }

    #[test]
    fn cancelled_history_store_does_not_write() {
        let path = state::state_dir().join("cancelled.json");
        let generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(2));
        let cancellation = bulkstat::ScanCancellation::new(generation, 1);
        assert!(!store_record_cancellable(&record(vec![]), &cancellation, Some(&path)).unwrap());
        assert!(!path.exists());
    }

    fn child(name: &str, allocated: u64) -> ChildSize {
        ChildSize {
            name: name.to_string(),
            is_dir: true,
            size: SizeInfo::new(allocated, allocated),
            inaccessible: 0,
            skipped_mounts: 0,
        }
    }

    fn record(children: Vec<ChildSize>) -> ScanRecord {
        ScanRecord {
            path: PathBuf::from("/tmp/example"),
            timestamp: 1000,
            generation: 0,
            enumeration_errors: 0,
            children,
        }
    }

    #[test]
    fn diff_detects_growth_addition_and_removal() {
        let before = record(vec![
            child("steady", 100),
            child("shrinks", 500),
            child("gone", 200),
        ]);
        let mut after = record(vec![
            child("steady", 100),
            child("shrinks", 300),
            child("grows", 900),
        ]);
        after.timestamp = 2000;

        let diff = diff_records(&before, &after);

        // "steady" is unchanged and should be absent.
        assert!(diff.changes.iter().all(|c| c.name != "steady"));
        // Largest mover first: "grows" (+900) over "shrinks" (-200) and "gone" (-200).
        assert_eq!(diff.changes[0].name, "grows");
        assert_eq!(diff.changes[0].before, None);
        assert_eq!(diff.changes[0].delta_allocated(), 900);

        let removed = diff
            .changes
            .iter()
            .find(|c| c.name == "gone")
            .expect("removed child present");
        assert_eq!(removed.after, None);
        assert_eq!(removed.delta_allocated(), -200);

        assert_eq!(diff.before_total.allocated, 800);
        assert_eq!(diff.after_total.allocated, 1300);
        assert_eq!(diff.total_delta_allocated(), 500);
        assert_eq!(diff.baseline_timestamp, 1000);
        assert_eq!(diff.current_timestamp, 2000);
    }

    #[test]
    fn record_round_trips_through_json() {
        let original = ScanRecord {
            path: PathBuf::from("/tmp/example"),
            timestamp: 4242,
            generation: 0,
            enumeration_errors: 2,
            children: vec![
                ChildSize {
                    name: "dir".to_string(),
                    is_dir: true,
                    size: SizeInfo::new(10, 20),
                    inaccessible: 3,
                    skipped_mounts: 0,
                },
                ChildSize {
                    name: "file".to_string(),
                    is_dir: false,
                    size: SizeInfo::new(30, 40),
                    inaccessible: 0,
                    skipped_mounts: 0,
                },
            ],
        };

        let json = record_to_json(&original);
        let restored = record_from_json(&original.path, &json).unwrap();
        assert_eq!(restored, original);
    }

    #[test]
    fn cancelled_diff_returns_none_without_scanning() {
        let dir = std::env::temp_dir().join(format!(
            "diskr_history_cancel_{}_{}",
            std::process::id(),
            now_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let baseline = ScanRecord {
            path: dir.clone(),
            timestamp: 0,
            generation: 0,
            enumeration_errors: 0,
            children: Vec::new(),
        };
        // A token whose generation has already moved past its expected value is
        // cancelled, so the diff aborts without walking.
        let generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(2));
        let cancellation = bulkstat::ScanCancellation::new(generation, 1);

        let result = diff_from_record_with_cancellation(&baseline, &dir, &cancellation).unwrap();
        assert!(result.is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_history_file_surfaces_as_error() {
        // load_records_with_warning relies on the load path returning Err so a
        // corrupt history.json becomes a visible status instead of "no
        // baselines".
        let path = std::env::temp_dir().join(format!(
            "diskr_history_corrupt_{}_{}.json",
            std::process::id(),
            now_secs()
        ));
        std::fs::write(&path, "{ not valid json").unwrap();

        assert!(load_records_from_path(&path).is_err());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn old_format_child_without_inaccessible_defaults_to_zero() {
        let value = serde_json::json!({
            "path": "/tmp/example",
            "timestamp": 1,
            "children": [
                {"name": "dir", "is_dir": true, "logical": 10, "allocated": 20}
            ],
        });
        let record = record_from_json(Path::new("/tmp/example"), &value).unwrap();
        assert_eq!(record.children[0].inaccessible, 0);
        assert_eq!(record.inaccessible(), 0);
    }

    #[test]
    fn diff_reports_inaccessible_totals() {
        let mut before = record(vec![child("a", 100)]);
        before.children[0].inaccessible = 2;
        let mut after = record(vec![child("a", 150)]);
        after.children[0].inaccessible = 0;

        let diff = diff_records(&before, &after);
        assert_eq!(diff.baseline_inaccessible, 2);
        assert_eq!(diff.current_inaccessible, 0);
    }

    #[test]
    fn total_sums_children() {
        let rec = record(vec![child("a", 100), child("b", 250)]);
        assert_eq!(rec.total().allocated, 350);
    }

    #[test]
    fn load_records_uses_json_keys_for_paths() {
        let path = std::env::temp_dir().join(format!(
            "diskr_history_{}_{}.json",
            std::process::id(),
            now_secs()
        ));
        std::fs::write(
            &path,
            r#"{
  "/tmp/first": {"path":"/wrong","timestamp":1,"children":[]},
  "/tmp/second": {"timestamp":2,"children":[]}
}"#,
        )
        .unwrap();

        let records = load_records_from_path(&path).unwrap();

        assert_eq!(
            records.get(Path::new("/tmp/first")).unwrap().path,
            PathBuf::from("/tmp/first")
        );
        assert_eq!(
            records.get(Path::new("/tmp/second")).unwrap().path,
            PathBuf::from("/tmp/second")
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn load_records_prunes_older_entries() {
        let path = std::env::temp_dir().join(format!(
            "diskr_history_prune_{}_{}.json",
            std::process::id(),
            now_secs()
        ));
        let mut history = serde_json::Map::new();
        for idx in 0..(HISTORY_MAX_RECORDS + 2) {
            let path = PathBuf::from(format!("/tmp/history-{idx}"));
            history.insert(
                path.to_string_lossy().into_owned(),
                record_to_json(&ScanRecord {
                    path,
                    timestamp: idx as u64,
                    generation: 0,
                    enumeration_errors: 0,
                    children: Vec::new(),
                }),
            );
        }
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&serde_json::Value::Object(history)).unwrap(),
        )
        .unwrap();

        let records = load_records_from_path(&path).unwrap();
        assert_eq!(records.len(), HISTORY_MAX_RECORDS);
        assert!(!records.contains_key(Path::new("/tmp/history-0")));
        assert!(records.contains_key(Path::new(&format!(
            "/tmp/history-{}",
            HISTORY_MAX_RECORDS + 1
        ))));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn concurrent_history_writers_keep_both_baselines() {
        let dir = std::env::temp_dir().join(format!(
            "diskr_history_merge_{}_{}",
            std::process::id(),
            now_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.json");
        let spawn = |name: &'static str, timestamp| {
            let path = path.clone();
            std::thread::spawn(move || {
                store_record_to_path(
                    &path,
                    &ScanRecord {
                        path: PathBuf::from(format!("/tmp/{name}")),
                        timestamp,
                        generation: 0,
                        enumeration_errors: 0,
                        children: Vec::new(),
                    },
                )
                .unwrap();
            })
        };
        let first = spawn("first", 1);
        let second = spawn("second", 2);
        first.join().unwrap();
        second.join().unwrap();

        let loaded = load_records_from_path(&path).unwrap();
        assert!(loaded.contains_key(Path::new("/tmp/first")));
        assert!(loaded.contains_key(Path::new("/tmp/second")));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
