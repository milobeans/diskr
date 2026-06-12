use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

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
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
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
