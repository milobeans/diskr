use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn fixture(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("diskr_cli_{name}_{}_{}", std::process::id(), nanos))
}

#[test]
fn package_json_retains_custom_manager_root_and_footprint_scope() {
    use std::os::unix::fs::PermissionsExt;

    let root = fixture("package_environment");
    let tools = root.join("tools");
    let install = root.join("custom install");
    std::fs::create_dir_all(&tools).unwrap();
    std::fs::create_dir_all(install.join("bin")).unwrap();
    std::fs::write(install.join("bin/example"), b"fixture-tool").unwrap();
    let cargo = tools.join("cargo");
    std::fs::write(&cargo, "#!/bin/sh\n[ \"$1\" = install ] && [ \"$2\" = --list ] && [ \"$3\" = --root ] && [ \"$4\" = \"$CARGO_INSTALL_ROOT\" ] || exit 42\nprintf 'example v1.0.0:\\n    example\\n'\n").unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_diskr"))
        .args(["--packages", "--json"])
        .arg(&root)
        .env("PATH", &tools)
        .env("HOME", root.join("home"))
        .env("CARGO_HOME", root.join("cargo-home"))
        .env("CARGO_INSTALL_ROOT", &install)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let cargo = report["system_packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|manager| manager["manager"] == "cargo")
        .unwrap();
    assert_eq!(cargo["scan_state"], "complete");
    assert_eq!(
        cargo["environment"]["install_root"],
        install.to_string_lossy().as_ref()
    );
    assert_eq!(cargo["packages"][0]["logical"], 12);
    assert_eq!(
        cargo["packages"][0]["footprint"]["basis"],
        "known installed binaries"
    );
    assert_eq!(cargo["size_coverage"]["scope_limited"], 1);
    assert_eq!(cargo["size_coverage"]["unknown"], 0);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn human_report_escapes_terminal_controls_in_filenames() {
    let root = fixture("terminal_controls");
    std::fs::create_dir_all(&root).unwrap();
    let malicious = root.join("fake\u{1b}[31m-red\nline.txt");
    std::fs::write(&malicious, vec![0_u8; 4096]).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_diskr"))
        .args(["--top", "1"])
        .arg(&root)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "status: {:?}, stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.stdout.contains(&0x1b));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains("-red\nline.txt"));
    assert!(text.contains("\\x1b[31m-red\\nline.txt"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn snapshot_size_overflow_is_rejected() {
    let root = fixture("overflow");
    std::fs::create_dir_all(&root).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_diskr"))
        .args(["--thin-snapshots", "18446744073709551616"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("too large"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn closed_stdout_pipe_does_not_turn_help_into_a_panic() {
    use std::os::fd::{FromRawFd, OwnedFd};

    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    assert_eq!(unsafe { libc::close(fds[0]) }, 0);
    let writer = unsafe { OwnedFd::from_raw_fd(fds[1]) };

    let output = Command::new(env!("CARGO_BIN_EXE_diskr"))
        .arg("--help")
        .stdout(Stdio::from(writer))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "status: {:?}, stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
}
