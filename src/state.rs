use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::bulkstat::SizeInfo;

pub(crate) const SIZE_CACHE_MAX_ENTRIES: usize = 50_000;

const SIZE_CACHE_VERSION: u64 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CachedSize {
    pub path: PathBuf,
    pub size: SizeInfo,
    pub inaccessible: u32,
    pub scanned_at: u64,
}

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

pub(crate) fn store_size_cache(entries: &[CachedSize]) -> Result<()> {
    let path = size_cache_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    store_size_cache_to_path(&path, entries)
}

fn load_size_cache_from_path(path: &Path) -> Result<Vec<CachedSize>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parse {} (delete it to reset cache)", path.display()))?;
    let version = value.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
    if version != SIZE_CACHE_VERSION {
        bail!(
            "unexpected size-cache version in {} (delete it to reset cache)",
            path.display()
        );
    }
    let entries = value
        .get("entries")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("missing entries in {}", path.display()))?;
    Ok(entries.iter().filter_map(cached_size_from_json).collect())
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

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp.{}.{}", std::process::id(), nanos));
    let tmp = PathBuf::from(tmp);

    let write = (|| -> Result<()> {
        let mut file =
            std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
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
    Ok(())
}

fn store_size_cache_to_path(path: &Path, entries: &[CachedSize]) -> Result<()> {
    let entries: Vec<serde_json::Value> = entries
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
    let value = serde_json::json!({
        "version": SIZE_CACHE_VERSION,
        "entries": entries,
    });
    let text = serde_json::to_string_pretty(&value)?;
    atomic_write(path, &text)
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
    fn size_cache_round_trips_schema_v1() {
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
