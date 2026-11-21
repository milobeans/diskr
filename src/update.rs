//! Update the executable the user invoked through its original installation channel.

use crate::manifest::bounded_text;
use crate::packages::{Manager, ManagerEnvironment};
use anyhow::{ensure, Context, Result};
use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const FORMULA: &str = "milobeans/diskr/diskr";
const RELEASE: &str =
    "https://github.com/milobeans/diskr/releases/latest/download/distribution.json";
const SYSTEM_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

fn version(input: &str) -> Result<[u64; 3]> {
    let mut parts = input.split('.');
    let mut result = [0; 3];
    for number in &mut result {
        let part = parts.next().unwrap_or("");
        ensure!(
            !part.is_empty()
                && part.bytes().all(|c| c.is_ascii_digit())
                && (part == "0" || !part.starts_with('0')),
            "invalid stable release version"
        );
        *number = part.parse().context("invalid stable release version")?;
    }
    ensure!(parts.next().is_none(), "invalid stable release version");
    Ok(result)
}

fn latest_version() -> Result<String> {
    let output = Command::new("/usr/bin/curl")
        .args([
            "-q",
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--connect-timeout",
            "10",
            "--max-time",
            "30",
            "--max-filesize",
            "65536",
            RELEASE,
        ])
        .current_dir("/")
        .stderr(Stdio::inherit())
        .output()
        .context("check the latest Diskr release")?;
    ensure!(
        output.status.success(),
        "could not check the latest Diskr release ({})",
        output.status
    );
    #[derive(serde::Deserialize)]
    struct Distribution {
        version: String,
    }
    Ok(serde_json::from_slice::<Distribution>(&output.stdout)
        .context("invalid release metadata")?
        .version)
}

#[derive(Debug, PartialEq)]
enum Installation {
    Homebrew(PathBuf),
    Cargo(PathBuf),
    NativePrefix(PathBuf),
    Standalone,
}

impl Installation {
    fn detect(exe: &Path) -> Result<Self> {
        if exe.file_name().is_none_or(|name| name != "diskr") {
            return Ok(Self::Standalone);
        }
        let Some(bin) = exe.parent().filter(|dir| dir.ends_with("bin")) else {
            return Ok(Self::Standalone);
        };
        let prefix = bin.parent().context("missing installation prefix")?;
        // A keg has the layout Cellar/diskr/VERSION/bin/diskr. The running
        // executable may have been invoked through the opt or bin symlink.
        if let Some(cellar) = prefix
            .parent()
            .and_then(Path::parent)
            .filter(|p| p.ends_with("Cellar"))
        {
            let receipt = bounded_text(&prefix.join("INSTALL_RECEIPT.json"))
                .map_err(anyhow::Error::msg)
                .context("read Homebrew install receipt")?;
            let receipt: serde_json::Value = serde_json::from_str(&receipt)?;
            ensure!(
                prefix.parent().is_some_and(|p| p.ends_with("diskr"))
                    && receipt["source"]["tap"] == "milobeans/diskr",
                "this executable belongs to another Homebrew formula; update it with its own tap"
            );
            return Ok(Self::Homebrew(
                cellar
                    .parent()
                    .context("missing Homebrew prefix")?
                    .to_owned(),
            ));
        }
        let registry = prefix.join(".crates.toml");
        if registry.try_exists()? {
            let text = bounded_text(&registry)
                .map_err(anyhow::Error::msg)
                .context("read Cargo install records")?;
            let records: toml::Table = text.parse().context("invalid Cargo install records")?;
            let packages = records
                .get("v1")
                .and_then(toml::Value::as_table)
                .context("unknown Cargo install record format")?;
            for (id, bins) in packages {
                if bins
                    .as_array()
                    .is_some_and(|names| names.iter().any(|n| n.as_str() == Some("diskr")))
                {
                    ensure!(
                        id.starts_with("diskr "),
                        "another Cargo package owns this executable"
                    );
                    return Ok(Self::Cargo(prefix.to_owned()));
                }
            }
        }
        if prefix
            .join("share/diskr")
            .join(env!("CARGO_PKG_VERSION"))
            .join("LICENSE")
            .is_file()
        {
            return Ok(Self::NativePrefix(prefix.to_owned()));
        }
        Ok(Self::Standalone)
    }

    fn install(
        &self,
        exe: &Path,
        latest: &str,
        manager_command: impl Fn(Manager, Option<&Path>) -> Result<Command, String>,
    ) -> Result<PathBuf> {
        match self {
            Self::Homebrew(prefix) => {
                let bin = prefix.join("bin");
                let brew =
                    || manager_command(Manager::Brew, Some(&bin)).map_err(anyhow::Error::msg);
                run_command(brew()?.arg("update"), "refresh the Homebrew tap")?;
                run_command(
                    brew()?.args(["upgrade", FORMULA]),
                    "upgrade Diskr with Homebrew",
                )?;
                Ok(prefix.join("opt/diskr/bin/diskr"))
            }
            Self::Cargo(root) => {
                let mut cargo =
                    manager_command(Manager::Cargo, None).map_err(anyhow::Error::msg)?;
                run_command(
                    cargo
                        .args([
                            "install",
                            "--locked",
                            "--registry",
                            "crates-io",
                            "--version",
                            &format!("={latest}"),
                            "--root",
                        ])
                        .arg(root)
                        .arg("diskr"),
                    "install Diskr from crates.io",
                )?;
                Ok(exe.to_owned())
            }
            Self::NativePrefix(prefix) => {
                native_install(prefix, latest)?;
                Ok(exe.to_owned())
            }
            Self::Standalone => {
                let parent = exe.parent().context("missing executable directory")?;
                let stage = tempfile::Builder::new()
                    .prefix(".diskr-update-")
                    .tempdir_in(parent)
                    .context("create staging directory beside the current executable")?;
                native_install(stage.path(), latest)?;
                fs::rename(stage.path().join("bin/diskr"), exe)
                    .context("replace the active executable")?;
                Ok(exe.to_owned())
            }
        }
    }
}

fn run_command(command: &mut Command, action: &str) -> Result<()> {
    let status = command.status().with_context(|| action.to_string())?;
    ensure!(status.success(), "could not {action} ({status})");
    Ok(())
}

fn native_install(prefix: &Path, latest: &str) -> Result<()> {
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            include_str!("../scripts/install.sh"),
            "diskr-installer",
            "--version",
            latest,
            "--prefix",
        ])
        .arg(prefix)
        .current_dir("/")
        .env("PATH", SYSTEM_PATH)
        .env_remove("ENV")
        .env_remove("BASH_ENV")
        .env_remove("CDPATH")
        .stdout(Stdio::null());
    run_command(&mut command, "install the checksum-verified native release")
}

pub(crate) fn run() -> Result<String> {
    let current = env!("CARGO_PKG_VERSION");
    let latest = latest_version()?;
    match version(current)?.cmp(&version(&latest)?) {
        Ordering::Equal => return Ok(format!("diskr {current} is up to date.")),
        Ordering::Greater => {
            return Ok(format!(
                "diskr {current} is newer than the latest release ({latest})."
            ))
        }
        Ordering::Less => {}
    }
    let exe = std::env::current_exe()?
        .canonicalize()
        .context("locate the active Diskr executable")?;
    crate::state::with_exclusive_lock(&crate::state::state_dir().join("self-update"), || {
        let on_disk = installed_version(&exe)?;
        if version(&on_disk)? >= version(&latest)? {
            return Ok(format!("diskr {on_disk} is already installed."));
        }
        let install = Installation::detect(&exe)?;
        eprintln!("Updating diskr {on_disk} to {latest}…");
        let installed = install.install(&exe, &latest, ManagerEnvironment::for_update)?;
        let actual = installed_version(&installed)?;
        ensure!(version(&actual)? >= version(&latest)?,
            "the installation still reports diskr {actual}; expected {latest}. If the Homebrew tap is still updating, retry shortly.");
        Ok(format!("Updated diskr {on_disk} to {actual}."))
    })
}

fn installed_version(exe: &Path) -> Result<String> {
    let result = Command::new(exe)
        .arg("--version")
        .current_dir("/")
        .output()
        .with_context(|| format!("verify installed executable {}", exe.display()))?;
    ensure!(
        result.status.success(),
        "installed executable failed its version check"
    );
    let output = std::str::from_utf8(&result.stdout)?.trim();
    Ok(output
        .strip_prefix("diskr ")
        .context("unexpected installed executable version output")?
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_compared_numerically_and_only_stable_releases_are_accepted() {
        assert!(version("0.10.0").unwrap() > version("0.9.99").unwrap());
        for invalid in ["", "1", "0.1", "1.0.0-rc.1", "1.02.0", "+1.2.3", "1.2.3.4"] {
            assert!(version(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn homebrew_refreshes_the_matching_tap_before_upgrading() {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("calls");
        let brew = Installation::Homebrew(temp.path().into());
        let exe = temp.path().join("Cellar/diskr/0.1.0/bin/diskr");
        for fail in [false, true] {
            let result = brew.install(&exe, "0.2.0", |manager, directory| {
                assert_eq!(manager, Manager::Brew);
                assert_eq!(directory.unwrap(), temp.path().join("bin"));
                let mut command = Command::new("/bin/sh");
                command
                    .args(["-c", r#"echo "$@" >> "$LOG"; exit "$STATUS""#, "brew"])
                    .env("LOG", &log)
                    .env("STATUS", if fail { "1" } else { "0" });
                Ok(command)
            });
            let calls = fs::read_to_string(&log).unwrap();
            if fail {
                assert!(result.is_err());
                assert_eq!(calls, "update\n");
            } else {
                assert_eq!(result.unwrap(), temp.path().join("opt/diskr/bin/diskr"));
                assert_eq!(calls, "update\nupgrade milobeans/diskr/diskr\n");
            }
            fs::remove_file(&log).unwrap();
        }
    }

    #[test]
    fn identifies_the_owner_of_the_invoked_executable() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let exe = root.join("bin/diskr");
        assert_eq!(
            Installation::detect(&exe).unwrap(),
            Installation::Standalone
        );
        let notices = root.join("share/diskr").join(env!("CARGO_PKG_VERSION"));
        fs::create_dir_all(&notices).unwrap();
        fs::write(notices.join("LICENSE"), "").unwrap();
        assert_eq!(
            Installation::detect(&exe).unwrap(),
            Installation::NativePrefix(root.into())
        );
        let record = root.join(".crates.toml");
        fs::write(
            &record,
            "[v1]\n\"diskr 0.1.0 (registry+example)\" = [\"diskr\"]",
        )
        .unwrap();
        assert_eq!(
            Installation::detect(&exe).unwrap(),
            Installation::Cargo(root.into())
        );
        fs::write(
            &record,
            "[v1]\n\"other 1.0.0 (registry+example)\" = [\"diskr\"]",
        )
        .unwrap();
        assert!(Installation::detect(&exe).is_err());
        let keg = root.join("Cellar/diskr/0.1.0");
        fs::create_dir_all(&keg).unwrap();
        let receipt = keg.join("INSTALL_RECEIPT.json");
        fs::write(&receipt, r#"{"source":{"tap":"milobeans/diskr"}}"#).unwrap();
        assert_eq!(
            Installation::detect(&keg.join("bin/diskr")).unwrap(),
            Installation::Homebrew(root.into())
        );
        fs::write(receipt, r#"{"source":{"tap":"another/tap"}}"#).unwrap();
        assert!(Installation::detect(&keg.join("bin/diskr")).is_err());
    }
}
