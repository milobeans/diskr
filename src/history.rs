//! Persisted scan baselines and diffing — "what grew since the last scan?".
//!
//! `save` records the immediate children of a path (each sized recursively via
//! [`crate::bulkstat`]) together with a timestamp, keyed by absolute path in a
//! single JSON file. `diff` re-scans the path now and compares against the saved
//! baseline. Diffing is kept pure ([`diff_records`]) so it can be tested without
//! touching the filesystem.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::bulkstat::{self, SizeInfo};

/// One immediate child of a scanned directory, with its recursive size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildSize {
    pub name: String,
    pub is_dir: bool,
    pub size: SizeInfo,
}

/// A saved scan of a directory's immediate children at a point in time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanRecord {
    pub path: PathBuf,
    /// Seconds since the Unix epoch when the baseline was captured.
    pub timestamp: u64,
    pub children: Vec<ChildSize>,
}

impl ScanRecord {
    pub fn total(&self) -> SizeInfo {
        let mut total = SizeInfo::default();
        for child in &self.children {
            total.logical = total.logical.saturating_add(child.size.logical);
            total.allocated = total.allocated.saturating_add(child.size.allocated);
        }
        total
    }
}

/// A single child's change between two scans. Either side may be `None` when the
/// child only exists in one of the scans (added or removed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildChange {
    pub name: String,
    pub before: Option<SizeInfo>,
    pub after: Option<SizeInfo>,
}

impl ChildChange {
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
    /// Children whose size changed, plus additions and removals, sorted by the
    /// magnitude of the allocated-size delta (largest movers first).
    pub changes: Vec<ChildChange>,
}

impl DiffReport {
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
    store_record(&record)?;
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
    let current = scan_record(path)?;
    Ok(diff_records(&baseline, &current))
}

/// Pure diff of two scan records. `before`/`after` need not be sorted.
pub fn diff_records(before: &ScanRecord, after: &ScanRecord) -> DiffReport {
    let mut changes: Vec<ChildChange> = Vec::new();

    for child in &after.children {
        let prior = before
            .children
            .iter()
            .find(|c| c.name == child.name)
            .map(|c| c.size);
        let change = ChildChange {
            name: child.name.clone(),
            before: prior,
            after: Some(child.size),
        };
        if change.delta_allocated() != 0 || change.delta_logical() != 0 {
            changes.push(change);
        }
    }

    // Removed children: present before, absent now.
    for child in &before.children {
        if !after.children.iter().any(|c| c.name == child.name) {
            changes.push(ChildChange {
                name: child.name.clone(),
                before: Some(child.size),
                after: None,
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

fn scan_record(path: &Path) -> Result<ScanRecord> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("resolve {}", path.display()))?;
    let mut children = Vec::new();
    let read =
        std::fs::read_dir(&canonical).with_context(|| format!("read {}", canonical.display()))?;
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let meta = match std::fs::symlink_metadata(entry.path()) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        let file_type = meta.file_type();
        if file_type.is_symlink() {
            continue;
        }
        let (is_dir, size) = if file_type.is_dir() {
            (true, bulkstat::scan_dir(&entry.path(), 0).size)
        } else if file_type.is_file() {
            use std::os::unix::fs::MetadataExt;
            (
                false,
                SizeInfo::new(meta.len(), meta.blocks().saturating_mul(512)),
            )
        } else {
            continue;
        };
        children.push(ChildSize { name, is_dir, size });
    }
    children.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(ScanRecord {
        path: canonical,
        timestamp: now_secs(),
        children,
    })
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn state_dir() -> PathBuf {
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Library/Application Support/diskr")
}

fn history_file() -> PathBuf {
    state_dir().join("history.json")
}

fn load_history() -> Result<serde_json::Map<String, serde_json::Value>> {
    let path = history_file();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(serde_json::Map::new()),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parse {} (delete it to reset history)", path.display()))?;
    match value {
        serde_json::Value::Object(map) => Ok(map),
        _ => bail!(
            "unexpected history format in {} (delete it to reset history)",
            path.display()
        ),
    }
}

/// Load the saved baseline for a path if one exists.
pub fn load_record_for_path(path: &Path) -> Result<Option<ScanRecord>> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("resolve {}", path.display()))?;
    let history = load_history()?;
    let key = canonical.to_string_lossy().into_owned();
    let Some(value) = history.get(&key) else {
        return Ok(None);
    };
    Ok(Some(record_from_json(&canonical, value)))
}

fn store_record(record: &ScanRecord) -> Result<()> {
    let dir = state_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let mut history = load_history()?;
    history.insert(
        record.path.to_string_lossy().into_owned(),
        record_to_json(record),
    );
    let text = serde_json::to_string_pretty(&serde_json::Value::Object(history))?;
    let path = history_file();
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
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
            })
        })
        .collect();
    serde_json::json!({
        "path": record.path.to_string_lossy(),
        "timestamp": record.timestamp,
        "children": children,
    })
}

fn record_from_json(path: &Path, value: &serde_json::Value) -> ScanRecord {
    let timestamp = value.get("timestamp").and_then(|v| v.as_u64()).unwrap_or(0);
    let children = value
        .get("children")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let name = item.get("name")?.as_str()?.to_string();
                    let is_dir = item
                        .get("is_dir")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let logical = item.get("logical").and_then(|v| v.as_u64()).unwrap_or(0);
                    let allocated = item.get("allocated").and_then(|v| v.as_u64()).unwrap_or(0);
                    Some(ChildSize {
                        name,
                        is_dir,
                        size: SizeInfo::new(logical, allocated),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    ScanRecord {
        path: path.to_path_buf(),
        timestamp,
        children,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn child(name: &str, allocated: u64) -> ChildSize {
        ChildSize {
            name: name.to_string(),
            is_dir: true,
            size: SizeInfo::new(allocated, allocated),
        }
    }

    fn record(children: Vec<ChildSize>) -> ScanRecord {
        ScanRecord {
            path: PathBuf::from("/tmp/example"),
            timestamp: 1000,
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
            children: vec![
                ChildSize {
                    name: "dir".to_string(),
                    is_dir: true,
                    size: SizeInfo::new(10, 20),
                },
                ChildSize {
                    name: "file".to_string(),
                    is_dir: false,
                    size: SizeInfo::new(30, 40),
                },
            ],
        };

        let json = record_to_json(&original);
        let restored = record_from_json(&original.path, &json);
        assert_eq!(restored, original);
    }

    #[test]
    fn total_sums_children() {
        let rec = record(vec![child("a", 100), child("b", 250)]);
        assert_eq!(rec.total().allocated, 350);
    }
}
