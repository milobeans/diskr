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
fn reports_share_a_versioned_contract_and_keep_filters_separate_from_totals() {
    use std::os::unix::fs::PermissionsExt;
    let root = fixture("report_contract");
    let scan = root.join("scan");
    let tools = root.join("tools");
    std::fs::create_dir_all(scan.join("target")).unwrap();
    std::fs::create_dir_all(&tools).unwrap();
    std::fs::write(scan.join("small"), b"x").unwrap();
    std::fs::write(scan.join("large"), vec![0u8; 32768]).unwrap();
    std::fs::write(scan.join("target/output"), vec![0u8; 32768]).unwrap();
    std::fs::write(
        scan.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='1.0.0'\n",
    )
    .unwrap();
    for (name, content) in [
        ("tmutil", "#!/bin/sh\n[ \"$1\" = listlocalsnapshots ] && [ -d \"$2\" ] || exit 42\nprintf 'Snapshots for fixture:\\n'\n"),
        ("diskutil", "#!/bin/sh\n[ \"$1\" = info ] && [ \"$2\" = -plist ] && [ -d \"$3\" ] || exit 42\nprintf '<plist><dict><key>APFSContainerReference</key><string>fixture</string><key>APFSContainerSize</key><integer>100000</integer><key>APFSContainerFree</key><integer>40000</integer></dict></plist>'\n"),
    ] {
        let executable = tools.join(name);
        std::fs::write(&executable, content).unwrap();
        std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let run = |arguments: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_diskr"))
            .args(arguments)
            .arg(&scan)
            .env("HOME", root.join("home"))
            .env("PATH", &tools)
            .output()
            .unwrap()
    };
    for (kind, arguments) in [
        ("top", vec!["--top", "10", "--json"]),
        ("reclaim", vec!["--reclaim", "--json"]),
        ("save", vec!["--save", "--json"]),
        ("diff", vec!["--diff", "--json"]),
        ("space", vec!["--space", "--json"]),
        ("packages", vec!["--packages", "--json"]),
    ] {
        let output = run(&arguments);
        assert!(
            output.status.success(),
            "{kind}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty(), "{kind}");
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["schema_version"], 1, "{kind}");
        assert_eq!(report["report"], kind);
        assert_eq!(
            report["root"],
            scan.canonicalize().unwrap().to_string_lossy().as_ref()
        );
        assert_eq!(report["scope"]["root"], report["root"]);
        assert!(report["root_identity"].is_string());
        assert!(report["coverage"]["complete"].is_boolean());
        assert!(report["warnings"].is_array());
        assert!(report["timing"]["elapsed_ms"].is_u64());
        assert!(report["numeric_semantics"]["signed_deltas"].is_string());
        if kind == "save" {
            std::fs::write(scan.join("added"), b"new").unwrap();
        }
        if kind == "diff" {
            report["total_delta_allocated"]
                .as_str()
                .unwrap()
                .parse::<i128>()
                .unwrap();
            assert!(report["changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|change| change["name"] == "added"));
        }
    }
    let full: serde_json::Value =
        serde_json::from_slice(&run(&["--top", "10", "--json"]).stdout).unwrap();
    let filtered: serde_json::Value =
        serde_json::from_slice(&run(&["--top", "10", "--json", "--min-size", "8192B"]).stdout)
            .unwrap();
    assert_eq!(full["total_allocated"], filtered["total_allocated"]);
    assert!(filtered["filtered_out"].as_u64().unwrap() > 0);
    assert!(filtered["files"]
        .as_array()
        .unwrap()
        .iter()
        .all(|file| file["allocated"].as_u64().unwrap() >= 8192));
    let safe: serde_json::Value =
        serde_json::from_slice(&run(&["--reclaim", "--json", "--class", "safe"]).stdout).unwrap();
    assert!(safe["findings"]
        .as_array()
        .unwrap()
        .iter()
        .all(|finding| finding["class"] == "safe"));
    assert!(safe["filtered_out"].as_u64().unwrap() > 0);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn strict_reports_emit_valid_json_before_exiting_two_for_unknown_data() {
    let root = fixture("strict_report");
    let scan = root.join("scan");
    let tools = root.join("tools");
    std::fs::create_dir_all(&scan).unwrap();
    std::fs::create_dir_all(&tools).unwrap();
    std::fs::write(scan.join("Cargo.toml"), "[").unwrap();
    for strict in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_diskr"));
        command.args(["--packages", "--json", "--min-size", "1GiB"]);
        if strict {
            command.arg("--strict");
        }
        let output = command
            .arg(&scan)
            .env("HOME", root.join("home"))
            .env("PATH", &tools)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(if strict { 2 } else { 0 }),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["coverage"]["complete"], false);
        assert!(!report["warnings"].as_array().unwrap().is_empty());
        assert_eq!(report["project_dependencies"].as_array().unwrap().len(), 1);
        assert_eq!(
            report["project_dependencies"][0]["dep_count"],
            serde_json::Value::Null
        );
        assert_eq!(output.stderr.is_empty(), !strict);
    }
    use std::os::fd::{FromRawFd, OwnedFd};
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    assert_eq!(unsafe { libc::close(fds[0]) }, 0);
    let writer = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    let output = Command::new(env!("CARGO_BIN_EXE_diskr"))
        .args(["--packages", "--json", "--strict"])
        .arg(&scan)
        .env("HOME", root.join("home"))
        .env("PATH", &tools)
        .stdout(Stdio::from(writer))
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    std::fs::remove_dir_all(root).unwrap();
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
    std::fs::write(root.join("Cargo.toml"), "[").unwrap();
    let cargo = tools.join("cargo");
    std::fs::write(&cargo, "#!/bin/sh\n[ \"$1\" = install ] && [ \"$2\" = --list ] && [ \"$3\" = --root ] && [ \"$4\" = \"$CARGO_INSTALL_ROOT\" ] || exit 42\nprintf 'example v1.0.0:\\n    example\\n'\n").unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();

    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_diskr"))
            .args(args)
            .arg(&root)
            .env("PATH", &tools)
            .env("HOME", root.join("home"))
            .env("CARGO_HOME", root.join("cargo-home"))
            .env("CARGO_INSTALL_ROOT", &install)
            .output()
            .unwrap()
    };
    let output = run(&["--packages", "--json"]);
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
    let project = &report["project_dependencies"][0];
    assert!(project["dep_count"].is_null(), "{report:#}");
    assert_eq!(project["known_dep_count"], 0);
    assert_eq!(project["dependency_count_complete"], false);
    assert!(!project["dependency_warnings"]
        .as_array()
        .unwrap()
        .is_empty());
    let filtered = run(&["--packages", "--json", "--min-size", "1GiB"]);
    assert!(filtered.status.success());
    let report: serde_json::Value = serde_json::from_slice(&filtered.stdout).unwrap();
    let cargo = report["system_packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|manager| manager["manager"] == "cargo")
        .unwrap();
    assert_eq!(cargo["packages"].as_array().unwrap().len(), 1);
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
