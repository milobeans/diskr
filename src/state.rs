use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::bulkstat::SizeInfo;

pub(crate) const SIZE_CACHE_MAX_ENTRIES: usize = 50_000;

const SIZE_CACHE_VERSION: u64 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CachedSize {
    pub path: PathBuf,
    pub size: SizeInfo,
    pub inaccessible: u32,
    pub scanned_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CacheInvalidation {
    pub path: PathBuf,
    pub invalidated_at: u64,
}

#[derive(Default)]
struct SizeCacheState {
    entries: Vec<CachedSize>,
    invalidations: Vec<CacheInvalidation>,
}

#[cfg(test)]
pub(crate) fn state_dir() -> PathBuf {
    static SESSION: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    SESSION
        .get_or_init(|| {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            std::env::temp_dir().join(format!("diskr-tests-{}-{stamp}", std::process::id()))
        })
        .join(std::thread::current().name().unwrap_or("worker"))
}

#[cfg(not(test))]
pub(crate) fn state_dir() -> PathBuf {
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Library/Application Support/diskr")
}

fn size_cache_file() -> PathBuf {
    state_dir().join("size-cache.json")
}

pub(crate) fn load_size_cache() -> Result<Vec<CachedSize>> {
    load_size_cache_from_path(&size_cache_file())
}

pub(crate) fn store_size_cache(
    entries: &[CachedSize],
    invalidations: &[CacheInvalidation],
) -> Result<()> {
    let path = size_cache_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    store_size_cache_merged_to_path(&path, entries, invalidations)
}

fn store_size_cache_merged_to_path(
    path: &Path,
    entries: &[CachedSize],
    invalidations: &[CacheInvalidation],
) -> Result<()> {
    with_exclusive_lock(path, || {
        let current = load_size_cache_state_from_path(path)?;
        let mut merged: HashMap<PathBuf, CachedSize> = current
            .entries
            .into_iter()
            .map(|entry| (entry.path.clone(), entry))
            .collect();
        let mut tombstones: HashMap<PathBuf, u64> = current
            .invalidations
            .into_iter()
            .map(|entry| (entry.path, entry.invalidated_at))
            .collect();
        for invalidation in invalidations {
            let invalidated_at = tombstones
                .entry(invalidation.path.clone())
                .or_insert(invalidation.invalidated_at);
            *invalidated_at = (*invalidated_at).max(invalidation.invalidated_at);
            if merged
                .get(&invalidation.path)
                .is_some_and(|entry| entry.scanned_at <= *invalidated_at)
            {
                merged.remove(&invalidation.path);
            }
        }
        for entry in entries {
            if tombstones
                .get(&entry.path)
                .is_some_and(|invalidated_at| entry.scanned_at < *invalidated_at)
            {
                continue;
            }
            match merged.get(&entry.path) {
                Some(existing) if existing.scanned_at > entry.scanned_at => {}
                _ => {
                    merged.insert(entry.path.clone(), entry.clone());
                    tombstones.remove(&entry.path);
                }
            }
        }
        let mut merged: Vec<CachedSize> = merged.into_values().collect();
        merged.sort_by(|a, b| {
            b.scanned_at
                .cmp(&a.scanned_at)
                .then_with(|| a.path.cmp(&b.path))
        });
        merged.truncate(SIZE_CACHE_MAX_ENTRIES);
        let mut invalidations: Vec<CacheInvalidation> = tombstones
            .into_iter()
            .map(|(path, invalidated_at)| CacheInvalidation {
                path,
                invalidated_at,
            })
            .collect();
        invalidations.sort_by(|a, b| {
            b.invalidated_at
                .cmp(&a.invalidated_at)
                .then_with(|| a.path.cmp(&b.path))
        });
        invalidations.truncate(SIZE_CACHE_MAX_ENTRIES);
        store_size_cache_state_to_path(
            path,
            &SizeCacheState {
                entries: merged,
                invalidations,
            },
        )
    })
}

fn load_size_cache_from_path(path: &Path) -> Result<Vec<CachedSize>> {
    Ok(load_size_cache_state_from_path(path)?.entries)
}

fn load_size_cache_state_from_path(path: &Path) -> Result<SizeCacheState> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SizeCacheState::default());
        }
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parse {} (delete it to reset cache)", path.display()))?;
    let version = value.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
    if !matches!(version, 1 | SIZE_CACHE_VERSION) {
        bail!(
            "unexpected size-cache version in {} (delete it to reset cache)",
            path.display()
        );
    }
    let entries = value
        .get("entries")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("missing entries in {}", path.display()))?;
    let entries = entries.iter().filter_map(cached_size_from_json).collect();
    let invalidations = value
        .get("invalidations")
        .and_then(|v| v.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(cache_invalidation_from_json)
                .collect()
        })
        .unwrap_or_default();
    Ok(SizeCacheState {
        entries,
        invalidations,
    })
}

/// Write `contents` to `path` atomically: stream to a uniquely-named temp file
/// in the same directory, fsync it, then `rename(2)` over the target. A crash or
/// full disk leaves the previous file intact instead of a half-written one, and
/// the pid+timestamp suffix keeps concurrent writers from clobbering each
/// other's temp file.
pub(crate) fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;

    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }

    let (tmp, mut file) = (0_u8..20)
        .find_map(|attempt| {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let mut tmp = path.as_os_str().to_owned();
            tmp.push(format!(".tmp.{}.{}.{}", std::process::id(), nanos, attempt));
            let tmp = PathBuf::from(tmp);
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
            {
                Ok(file) => Some(Ok((tmp, file))),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(err) => Some(Err(err).with_context(|| format!("create {}", tmp.display()))),
            }
        })
        .transpose()?
        .context("could not allocate a unique state temporary file")?;

    let write = (|| -> Result<()> {
        file.write_all(contents.as_bytes())
            .with_context(|| format!("write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", tmp.display()))?;
        Ok(())
    })();
    if let Err(err) = write {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }

    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("replace {}", path.display()));
    }
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        File::open(dir)
            .with_context(|| format!("open {} for sync", dir.display()))?
            .sync_all()
            .with_context(|| format!("sync {}", dir.display()))?;
    }
    Ok(())
}

/// Serialize read-modify-write updates across diskr processes. The lock is a
/// sibling file so it remains stable while the data file itself is renamed.
pub(crate) fn with_exclusive_lock<T>(
    path: &Path,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(".lock");
    let lock_path = PathBuf::from(lock_path);
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .with_context(|| format!("open lock {}", lock_path.display()))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("lock {}", lock_path.display()));
    }
    struct Unlock(File);
    impl Drop for Unlock {
        fn drop(&mut self) {
            unsafe {
                libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
    let _unlock = Unlock(lock);
    operation()
}

#[cfg(test)]
fn store_size_cache_to_path(path: &Path, entries: &[CachedSize]) -> Result<()> {
    store_size_cache_state_to_path(
        path,
        &SizeCacheState {
            entries: entries.to_vec(),
            invalidations: Vec::new(),
        },
    )
}

fn store_size_cache_state_to_path(path: &Path, state: &SizeCacheState) -> Result<()> {
    let entries: Vec<serde_json::Value> = state
        .entries
        .iter()
        .map(|entry| {
            serde_json::json!({
                "path": entry.path.to_string_lossy(),
                "logical": entry.size.logical,
                "allocated": entry.size.allocated,
                "inaccessible": entry.inaccessible,
                "scanned_at": entry.scanned_at,
            })
        })
        .collect();
    let invalidations: Vec<serde_json::Value> = state
        .invalidations
        .iter()
        .map(|entry| {
            serde_json::json!({
                "path": entry.path.to_string_lossy(),
                "invalidated_at": entry.invalidated_at,
            })
        })
        .collect();
    let value = serde_json::json!({
        "version": SIZE_CACHE_VERSION,
        "entries": entries,
        "invalidations": invalidations,
    });
    let text = serde_json::to_string_pretty(&value)?;
    atomic_write(path, &text)
}

fn cache_invalidation_from_json(value: &serde_json::Value) -> Option<CacheInvalidation> {
    let path = value.get("path")?.as_str()?;
    if path.is_empty() {
        return None;
    }
    Some(CacheInvalidation {
        path: PathBuf::from(path),
        invalidated_at: value.get("invalidated_at")?.as_u64()?,
    })
}

fn cached_size_from_json(value: &serde_json::Value) -> Option<CachedSize> {
    let path = value.get("path")?.as_str()?;
    if path.is_empty() {
        return None;
    }
    let logical = value.get("logical")?.as_u64()?;
    let allocated = value.get("allocated")?.as_u64()?;
    let scanned_at = value.get("scanned_at")?.as_u64()?;
    let inaccessible = value
        .get("inaccessible")
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0);

    Some(CachedSize {
        path: PathBuf::from(path),
        size: SizeInfo::new(logical, allocated),
        inaccessible,
        scanned_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn size_cache_round_trips_schema_v2() {
        let path = temp_file("round_trip");
        let entries = vec![CachedSize {
            path: PathBuf::from("/tmp/example"),
            size: SizeInfo::new(123, 456),
            inaccessible: 2,
            scanned_at: 42,
        }];

        store_size_cache_to_path(&path, &entries).unwrap();
        let loaded = load_size_cache_from_path(&path).unwrap();

        assert_eq!(loaded, entries);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn size_cache_skips_malformed_entries() {
        let path = temp_file("malformed");
        std::fs::write(
            &path,
            r#"{"version":1,"entries":[{"path":"/tmp/a","logical":1,"allocated":2,"scanned_at":3},{"path":""}]}"#,
        )
        .unwrap();

        let loaded = load_size_cache_from_path(&path).unwrap();

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].path, PathBuf::from("/tmp/a"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp_files() {
        let dir = temp_file("atomic_dir");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.json");

        atomic_write(&path, "first").unwrap();
        atomic_write(&path, "second").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![String::from("data.json")],
            "temp file left behind: {names:?}"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_cache_writers_merge_without_lost_updates() {
        let dir = temp_file("cache_merge");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cache.json");
        let writer = |name: &'static str, scanned_at| {
            let path = path.clone();
            std::thread::spawn(move || {
                store_size_cache_merged_to_path(
                    &path,
                    &[CachedSize {
                        path: PathBuf::from(format!("/tmp/{name}")),
                        size: SizeInfo::new(scanned_at, scanned_at),
                        inaccessible: 0,
                        scanned_at,
                    }],
                    &[],
                )
                .unwrap();
            })
        };
        let first = writer("first", 1);
        let second = writer("second", 2);
        first.join().unwrap();
        second.join().unwrap();

        let loaded = load_size_cache_from_path(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.iter().any(|entry| entry.path.ends_with("first")));
        assert!(loaded.iter().any(|entry| entry.path.ends_with("second")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cache_invalidation_prevents_a_stale_writer_from_resurrecting_an_entry() {
        let dir = temp_file("cache_tombstone");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cache.json");
        let cached = CachedSize {
            path: PathBuf::from("/tmp/removed"),
            size: SizeInfo::new(10, 10),
            inaccessible: 0,
            scanned_at: 10,
        };
        store_size_cache_merged_to_path(&path, std::slice::from_ref(&cached), &[]).unwrap();
        store_size_cache_merged_to_path(
            &path,
            &[],
            &[CacheInvalidation {
                path: cached.path.clone(),
                invalidated_at: 20,
            }],
        )
        .unwrap();

        store_size_cache_merged_to_path(&path, &[cached], &[]).unwrap();

        assert!(load_size_cache_from_path(&path).unwrap().is_empty());
        let state = load_size_cache_state_from_path(&path).unwrap();
        assert_eq!(state.invalidations.len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn temp_file(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "diskr_state_{name}_{}_{}.json",
            std::process::id(),
            nanos
        ))
    }
}
