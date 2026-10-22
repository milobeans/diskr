use anyhow::{bail, Context, Result};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::packages;

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
    pub apfs_error: Option<String>,
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
    let local_snapshots = list_local_snapshots(&stat.mount);
    let (apfs_container, apfs_error) = if stat.fs_type == "apfs" {
        match apfs_container_for_mount(&stat.mount) {
            Ok(container) => (Some(container), None),
            Err(error) => (None, Some(error.to_string())),
        }
    } else {
        (None, None)
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
        apfs_error,
    })
}

pub fn thin_local_snapshots(path: &Path, bytes: u64) -> Result<ThinResult> {
    if bytes == 0 {
        bail!("snapshot thin amount must be greater than zero");
    }
    let mut command = Command::new("tmutil");
    command
        .arg("thinlocalsnapshots")
        .arg(path)
        .arg(bytes.to_string())
        .arg("4");
    let output = packages::run_process(command, Duration::from_secs(300));
    let stdout = output.stdout.trim().to_string();
    let stderr = output.stderr.trim().to_string();
    if output.timed_out {
        bail!("tmutil thinlocalsnapshots timed out; check snapshot state before retrying");
    }
    if !output.success {
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

    let multiplier: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        "t" | "tb" | "tib" => 1024_u64.pow(4),
        "p" | "pb" | "pib" => 1024_u64.pow(5),
        _ => bail!("unknown size unit: {unit}"),
    };

    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    let whole = if whole.is_empty() {
        0
    } else {
        whole
            .parse::<u64>()
            .with_context(|| format!("invalid size: {input}"))?
    };
    if fraction.len() > 18 {
        bail!("size has more than 18 fractional digits");
    }
    let whole_bytes = whole.checked_mul(multiplier).context("size is too large")?;
    let fractional_bytes = if fraction.is_empty() {
        0
    } else {
        let numerator = fraction
            .parse::<u64>()
            .with_context(|| format!("invalid size: {input}"))?;
        let denominator = 10_u128.pow(fraction.len() as u32);
        let scaled = u128::from(numerator) * u128::from(multiplier);
        ((scaled + denominator / 2) / denominator) as u64
    };
    let bytes = whole_bytes
        .checked_add(fractional_bytes)
        .context("size is too large")?;
    if bytes == 0 {
        bail!("size must be at least one byte");
    }
    Ok(bytes)
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

pub(crate) fn list_local_snapshots(path: &Path) -> SnapshotListing {
    let mut command = Command::new("tmutil");
    command.arg("listlocalsnapshots").arg(path);
    let output = packages::run_process(command, Duration::from_secs(15));
    match output {
        output if output.success => SnapshotListing {
            names: parse_tmutil_snapshot_names(&output.stdout),
            error: None,
        },
        output => SnapshotListing {
            names: Vec::new(),
            error: Some(if output.timed_out {
                String::from("tmutil timed out; retry snapshot discovery")
            } else if output.stderr.trim().is_empty() {
                output.stdout.trim().to_string()
            } else {
                output.stderr.trim().to_string()
            }),
        },
    }
}

pub(crate) fn apfs_container_for_mount(mount: &Path) -> Result<ApfsContainer> {
    let mut command = Command::new("diskutil");
    command.arg("info").arg("-plist").arg(mount);
    let output = packages::run_process(command, Duration::from_secs(15));
    if output.timed_out {
        bail!("diskutil info timed out; retry APFS container discovery");
    }
    if !output.success {
        bail!("diskutil info failed: {}", output.stderr.trim());
    }

    parse_apfs_container_plist(output.stdout.as_bytes())
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

fn parse_apfs_container_plist(plist: &[u8]) -> Result<ApfsContainer> {
    let plist = std::str::from_utf8(plist).context("diskutil plist was not utf-8")?;
    let reference = extract_plist_string(plist, "APFSContainerReference")
        .context("diskutil plist lacks APFSContainerReference")?;
    let size = extract_plist_u64(plist, "APFSContainerSize")?
        .context("diskutil plist lacks APFSContainerSize")?;
    let free = extract_plist_u64(plist, "APFSContainerFree")?
        .context("diskutil plist lacks APFSContainerFree")?;

    Ok(ApfsContainer {
        reference,
        size,
        free,
    })
}

fn extract_plist_string(plist: &str, key: &str) -> Option<String> {
    extract_plist_scalar(plist, key, "string").map(ToOwned::to_owned)
}

fn extract_plist_u64(plist: &str, key: &str) -> Result<Option<u64>> {
    let Some(raw) = extract_plist_scalar(plist, key, "integer") else {
        return Ok(None);
    };
    raw.parse::<u64>()
        .with_context(|| format!("invalid integer value for plist key {key}"))
        .map(Some)
}

fn extract_plist_scalar<'a>(plist: &'a str, key: &str, tag: &str) -> Option<&'a str> {
    let key_marker = format!("<key>{key}</key>");
    let start = plist.find(&key_marker)? + key_marker.len();
    let tail = &plist[start..];
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let value_start = tail.find(&open)? + open.len();
    let tail = &tail[value_start..];
    let value_end = tail.find(&close)?;
    Some(&tail[..value_end])
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
    fn parse_byte_size_is_exact_at_u64_boundary() {
        assert_eq!(parse_byte_size("18446744073709551615").unwrap(), u64::MAX);
        assert!(parse_byte_size("18446744073709551616").is_err());
        assert!(parse_byte_size("18014398509481984KiB").is_err());
    }

    #[test]
    fn parse_byte_size_rounds_fractions_without_floating_point() {
        assert_eq!(parse_byte_size("0.5B").unwrap(), 1);
        assert_eq!(parse_byte_size("1.5KiB").unwrap(), 1536);
        assert_eq!(parse_byte_size("0.0009765625KiB").unwrap(), 1);
        assert!(parse_byte_size("0.1B").is_err());
        assert!(parse_byte_size("1.1234567890123456789KiB").is_err());
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

    #[test]
    fn parse_apfs_container_plist_extracts_required_fields() {
        let plist = br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>APFSContainerFree</key>
    <integer>123</integer>
    <key>APFSContainerReference</key>
    <string>disk3</string>
    <key>APFSContainerSize</key>
    <integer>456</integer>
</dict>
</plist>
"#;

        let container = parse_apfs_container_plist(plist).unwrap();
        assert_eq!(container.reference, "disk3");
        assert_eq!(container.free, 123);
        assert_eq!(container.size, 456);
    }

    #[test]
    fn parse_apfs_container_plist_reports_missing_fields() {
        let plist = br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>FilesystemType</key>
    <string>apfs</string>
</dict>
</plist>
"#;

        assert!(parse_apfs_container_plist(plist)
            .unwrap_err()
            .to_string()
            .contains("APFSContainerReference"));
    }

    #[test]
    fn parse_apfs_container_plist_rejects_invalid_integer_fields() {
        let plist = br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>APFSContainerFree</key>
    <integer>nope</integer>
    <key>APFSContainerReference</key>
    <string>disk3</string>
    <key>APFSContainerSize</key>
    <integer>456</integer>
</dict>
</plist>
"#;

        assert!(parse_apfs_container_plist(plist).is_err());
    }
}
