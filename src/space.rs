use anyhow::{bail, Context, Result};
use plist::Value;
use std::ffi::CString;
use std::io::Cursor;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug)]
pub struct SpaceReport {
    pub path: PathBuf,
    pub mount: PathBuf,
    pub device: String,
    pub fs_type: String,
    pub total: u64,
    pub used: u64,
    pub free: u64,
    pub available: u64,
    pub local_snapshots: SnapshotListing,
    pub apfs_container: Option<ApfsContainer>,
}

impl SpaceReport {
    pub fn unavailable_free(&self) -> u64 {
        self.free.saturating_sub(self.available)
    }
}

#[derive(Clone, Debug, Default)]
pub struct SnapshotListing {
    pub names: Vec<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ApfsContainer {
    pub reference: String,
    pub size: u64,
    pub free: u64,
}

#[derive(Clone, Debug)]
pub struct ThinResult {
    pub path: PathBuf,
    pub requested_bytes: u64,
    pub stdout: String,
    pub stderr: String,
}

pub fn report_for_path(path: &Path) -> Result<SpaceReport> {
    if !path.exists() {
        bail!("path does not exist: {}", path.display());
    }

    let stat = statfs_for_path(path)?;
    let local_snapshots = list_local_snapshots(path);
    let apfs_container = if stat.fs_type == "apfs" {
        apfs_container_for_mount(&stat.mount).ok().flatten()
    } else {
        None
    };

    Ok(SpaceReport {
        path: path.to_path_buf(),
        mount: stat.mount,
        device: stat.device,
        fs_type: stat.fs_type,
        total: stat.total,
        used: stat.used,
        free: stat.free,
        available: stat.available,
        local_snapshots,
        apfs_container,
    })
}

pub fn thin_local_snapshots(path: &Path, bytes: u64) -> Result<ThinResult> {
    if bytes == 0 {
        bail!("snapshot thin amount must be greater than zero");
    }
    let output = Command::new("tmutil")
        .arg("thinlocalsnapshots")
        .arg(path)
        .arg(bytes.to_string())
        .arg("4")
        .output()
        .context("run tmutil thinlocalsnapshots")?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        let message = if stderr.is_empty() { &stdout } else { &stderr };
        bail!("tmutil thinlocalsnapshots failed: {message}");
    }

    Ok(ThinResult {
        path: path.to_path_buf(),
        requested_bytes: bytes,
        stdout,
        stderr,
    })
}

pub fn parse_byte_size(input: &str) -> Result<u64> {
    let input = input.trim();
    if input.is_empty() {
        bail!("size cannot be empty");
    }

    let split_at = input
        .find(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .unwrap_or(input.len());
    let (number, unit) = input.split_at(split_at);
    if number.is_empty() || number == "." || number.matches('.').count() > 1 {
        bail!("invalid size: {input}");
    }

    let value = number
        .parse::<f64>()
        .with_context(|| format!("invalid size: {input}"))?;
    if !value.is_finite() || value <= 0.0 {
        bail!("size must be greater than zero");
    }

    let multiplier = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        "p" | "pb" | "pib" => 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => bail!("unknown size unit: {unit}"),
    };
    let bytes = value * multiplier;
    if bytes > u64::MAX as f64 {
        bail!("size is too large");
    }
    Ok(bytes.round() as u64)
}

struct StatfsInfo {
    mount: PathBuf,
    device: String,
    fs_type: String,
    total: u64,
    used: u64,
    free: u64,
    available: u64,
}

fn statfs_for_path(path: &Path) -> Result<StatfsInfo> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("path contains interior NUL: {}", path.display()))?;
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    let rc = unsafe { libc::statfs(c_path.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("statfs {}", path.display()));
    }
    let stat = unsafe { stat.assume_init() };
    let block_size = u64::from(stat.f_bsize);
    let total = stat.f_blocks.saturating_mul(block_size);
    let free = stat.f_bfree.saturating_mul(block_size);
    let available = stat.f_bavail.saturating_mul(block_size);

    Ok(StatfsInfo {
        mount: PathBuf::from(c_char_array_to_string(&stat.f_mntonname)),
        device: c_char_array_to_string(&stat.f_mntfromname),
        fs_type: c_char_array_to_string(&stat.f_fstypename),
        total,
        used: total.saturating_sub(free),
        free,
        available,
    })
}

fn list_local_snapshots(path: &Path) -> SnapshotListing {
    match Command::new("tmutil")
        .arg("listlocalsnapshots")
        .arg(path)
        .output()
    {
        Ok(output) if output.status.success() => SnapshotListing {
            names: parse_tmutil_snapshot_names(&String::from_utf8_lossy(&output.stdout)),
            error: None,
        },
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            SnapshotListing {
                names: Vec::new(),
                error: Some(if stderr.is_empty() { stdout } else { stderr }),
            }
        }
        Err(err) => SnapshotListing {
            names: Vec::new(),
            error: Some(err.to_string()),
        },
    }
}

fn apfs_container_for_mount(mount: &Path) -> Result<Option<ApfsContainer>> {
    let output = Command::new("diskutil")
        .arg("info")
        .arg("-plist")
        .arg(mount)
        .output()
        .context("run diskutil info -plist")?;
    if !output.status.success() {
        return Ok(None);
    }

    let value = Value::from_reader(Cursor::new(output.stdout)).context("parse diskutil plist")?;
    let Some(dict) = value.as_dictionary() else {
        return Ok(None);
    };

    let Some(reference) = dict
        .get("APFSContainerReference")
        .and_then(Value::as_string)
    else {
        return Ok(None);
    };
    let Some(size) = dict
        .get("APFSContainerSize")
        .and_then(Value::as_unsigned_integer)
    else {
        return Ok(None);
    };
    let Some(free) = dict
        .get("APFSContainerFree")
        .and_then(Value::as_unsigned_integer)
    else {
        return Ok(None);
    };

    Ok(Some(ApfsContainer {
        reference: reference.to_string(),
        size,
        free,
    }))
}

fn parse_tmutil_snapshot_names(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with("Snapshots for "))
        .map(ToOwned::to_owned)
        .collect()
}

fn c_char_array_to_string(chars: &[libc::c_char]) -> String {
    if chars.is_empty() || chars[0] == 0 {
        return String::new();
    }
    unsafe { std::ffi::CStr::from_ptr(chars.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_byte_size_accepts_binary_units() {
        assert_eq!(parse_byte_size("1024").unwrap(), 1024);
        assert_eq!(parse_byte_size("1K").unwrap(), 1024);
        assert_eq!(parse_byte_size("1.5G").unwrap(), 1_610_612_736);
        assert_eq!(parse_byte_size("2 TiB").unwrap(), 2_199_023_255_552);
    }

    #[test]
    fn parse_byte_size_rejects_invalid_values() {
        assert!(parse_byte_size("").is_err());
        assert!(parse_byte_size("0").is_err());
        assert!(parse_byte_size("12XB").is_err());
        assert!(parse_byte_size("..1G").is_err());
    }

    #[test]
    fn parses_tmutil_snapshot_names() {
        let output = "\
Snapshots for volume group containing disk /:
com.apple.TimeMachine.2026-06-08-120000.local
com.apple.os.update-MSUPrepareUpdate
";

        assert_eq!(
            parse_tmutil_snapshot_names(output),
            vec![
                "com.apple.TimeMachine.2026-06-08-120000.local",
                "com.apple.os.update-MSUPrepareUpdate"
            ]
        );
    }
}
