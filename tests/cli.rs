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
fn baseline_library_and_profiles_preserve_identity_retention_and_latest_history() {
    let root = fixture("baseline_library");
    let scan = root.join("scan");
    let other = root.join("other");
    let home = root.join("home");
    let imported_home = root.join("imported-home");
    let tools = root.join("tools");
    for path in [&scan, &other, &home, &imported_home, &tools] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::write(scan.join("file"), b"before").unwrap();
    let run = |home: &PathBuf, arguments: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_diskr"))
            .args(arguments)
            .env("HOME", home)
            .env("PATH", &tools)
            .env("CARGO_HOME", root.join("cargo"))
            .output()
            .unwrap()
    };
    let checked = |home: &PathBuf, arguments: &[&str]| {
        let output = run(home, arguments);
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let scan_text = scan.to_str().unwrap();
    checked(&home, &["--save", "--json", scan_text]);
    let latest_path = home.join("Library/Application Support/diskr/history.json");
    let latest = std::fs::read(&latest_path).unwrap();
    checked(
        &home,
        &["--save", "--baseline", "before", "--json", scan_text],
    );
    std::fs::write(scan.join("added"), b"after").unwrap();
    checked(
        &home,
        &["--save", "--baseline", "after", "--json", scan_text],
    );
    let diff = checked(&home, &["--diff", "--baseline", "before", "--json"]);
    assert_eq!(diff["baseline_origin"]["name"], "before");
    assert!(diff["changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|change| change["name"] == "added"));
    assert_eq!(std::fs::read(&latest_path).unwrap(), latest);
    assert!(!run(
        &home,
        &["--diff", "--baseline", "before", other.to_str().unwrap()]
    )
    .status
    .success());
    checked(&home, &["--baselines", "pin", "before", "--json"]);
    checked(
        &home,
        &[
            "--save-profile",
            "work",
            "--projects-only",
            "--max-depth",
            "2",
            "--json",
            scan_text,
        ],
    );
    let profile = checked(&home, &["--packages", "--profile", "work", "--json"]);
    assert_eq!(
        profile["root"],
        scan.canonicalize().unwrap().to_str().unwrap()
    );
    assert_eq!(profile["system_packages"], serde_json::json!([]));
    let export = root.join("export.json");
    checked(
        &home,
        &["--baselines", "export", export.to_str().unwrap(), "--json"],
    );
    assert!(
        !run(&home, &["--baselines", "export", export.to_str().unwrap()])
            .status
            .success()
    );
    let preview = checked(&home, &["--baselines", "prune", "0", "--json"]);
    assert_eq!(preview["would_remove"], serde_json::json!(["after"]));
    let pruned = checked(&home, &["--baselines", "prune", "0", "--yes", "--json"]);
    assert_eq!(pruned["removed"], serde_json::json!(["after"]));
    assert!(std::path::Path::new(pruned["backup"].as_str().unwrap()).is_file());
    checked(
        &imported_home,
        &["--baselines", "import", export.to_str().unwrap(), "--json"],
    );
    let listed = checked(&imported_home, &["--baselines", "list", "--json"]);
    assert_eq!(listed["baselines"].as_array().unwrap().len(), 2);
    assert!(listed["latest_roots"].as_array().unwrap().is_empty());
    assert!(listed["baselines"]
        .as_array()
        .unwrap()
        .iter()
        .all(|baseline| baseline["machine"] != listed["local_machine"]));
    assert!(!run(&imported_home, &["--packages", "--profile", "work"])
        .status
        .success());
    let foreign = run(
        &imported_home,
        &["--diff", "--baseline", "before", "--strict", "--json"],
    );
    assert_eq!(foreign.status.code(), Some(2));
    let foreign: serde_json::Value = serde_json::from_slice(&foreign.stdout).unwrap();
    assert_eq!(foreign["baseline_origin"]["foreign_machine"], true);
    assert_eq!(foreign["coverage"]["complete"], false);
    let preview = checked(&home, &["--baselines", "remove-profile", "work", "--json"]);
    assert_eq!(preview["dry_run"], true);
    assert_eq!(
        checked(&home, &["--baselines", "list", "--json"])["profiles"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let removed = checked(
        &home,
        &["--baselines", "remove-profile", "work", "--yes", "--json"],
    );
    let backup: serde_json::Value =
        serde_json::from_slice(&std::fs::read(removed["backup"].as_str().unwrap()).unwrap())
            .unwrap();
    assert!(backup["profiles"]["work"].is_object());
    assert_eq!(
        checked(&home, &["--baselines", "list", "--json"])["profiles"],
        serde_json::json!([])
    );
    assert_eq!(std::fs::read(&latest_path).unwrap(), latest);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn native_tui_restores_terminal_after_filter_refresh_modal_and_quit() {
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::time::{Duration, Instant};
    let root = fixture("native_tui");
    let scan = root.join("scan");
    let home = root.join("home");
    let tools = root.join("tools");
    for dir in [&scan, &home, &tools] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let target = scan.join("needle.txt");
    std::fs::write(&target, b"preserve this fixture").unwrap();
    std::fs::write(scan.join("other.txt"), b"other").unwrap();
    let idle_seconds = std::env::var("DISKR_BENCH_IDLE_SECONDS")
        .ok()
        .map(|value| value.parse::<u64>().unwrap())
        .unwrap_or(0);
    assert!(idle_seconds <= 60);
    if idle_seconds > 0 {
        for index in 0..10_000 {
            std::fs::write(scan.join(format!("wide-{index:05}")), b"").unwrap();
        }
    }
    let (mut master_fd, mut slave_fd) = (-1, -1);
    let mut dimensions = libc::winsize {
        ws_row: 24,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dimensions,
            )
        },
        0
    );
    let mut master = unsafe { std::fs::File::from_raw_fd(master_fd) };
    let slave = unsafe { std::fs::File::from_raw_fd(slave_fd) };
    let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
    assert_eq!(
        unsafe { libc::tcgetattr(slave.as_raw_fd(), original.as_mut_ptr()) },
        0
    );
    let original = unsafe { original.assume_init() };
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let startup_started = Instant::now();
    let child = Command::new(env!("CARGO_BIN_EXE_diskr"))
        .arg(&scan)
        .env("HOME", &home)
        .env("CARGO_HOME", root.join("cargo"))
        .env("PATH", &tools)
        .env("TERM", "xterm-256color")
        .env("DISKR_ASCII", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave.try_clone().unwrap()))
        .spawn()
        .unwrap();
    let mut child = ChildGuard(child);
    let mut output = Vec::new();
    fn emitted_text(bytes: &[u8]) -> String {
        let mut visible = Vec::new();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'[') {
                index += 2;
                while index < bytes.len() && !(0x40..=0x7e).contains(&bytes[index]) {
                    index += 1;
                }
            } else {
                visible.push(bytes[index]);
            }
            index += 1;
        }
        String::from_utf8_lossy(&visible)
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect()
    }
    let wait_for =
        |master: &mut std::fs::File, output: &mut Vec<u8>, needle: &str, offset: usize| {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut bytes = [0u8; 16_384];
            loop {
                match master.read(&mut bytes) {
                    Ok(count) => output.extend_from_slice(&bytes[..count]),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => panic!("PTY read: {error}"),
                }
                if emitted_text(&output[offset..]).contains(&emitted_text(needle.as_bytes())) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "waiting for {needle}: {}",
                    String::from_utf8_lossy(&output[output.len().saturating_sub(4096)..])
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        };
    wait_for(&mut master, &mut output, "loading state for", 0);
    let first_paint = startup_started.elapsed();
    wait_for(&mut master, &mut output, "needle.txt", 0);
    let first_row = startup_started.elapsed();
    let offset = output.len();
    master.write_all(b"/needle").unwrap();
    wait_for(&mut master, &mut output, "Enter keep", offset);
    master.write_all(b"\r").unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let offset = output.len();
    master.write_all(b"r").unwrap();
    wait_for(&mut master, &mut output, "needle.txt", offset);
    let offset = output.len();
    master.write_all(b"d").unwrap();
    wait_for(&mut master, &mut output, "Move to Trash:", offset);
    master.write_all(b"n").unwrap();
    let offset = output.len();
    master.write_all(b"?").unwrap();
    wait_for(&mut master, &mut output, "keyboard help", offset);
    let offset = output.len();
    master.write_all(b"\x1b").unwrap();
    wait_for(&mut master, &mut output, "all visible sizes fresh", offset);
    if idle_seconds > 0 {
        let offset = output.len();
        master.write_all(b"\x1b").unwrap();
        wait_for(&mut master, &mut output, "other.txt", offset);
        fn usage(pid: u32) -> libc::rusage_info_v2 {
            let mut usage: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe {
                    libc::proc_pid_rusage(
                        pid as libc::pid_t,
                        libc::RUSAGE_INFO_V2,
                        (&mut usage as *mut libc::rusage_info_v2).cast(),
                    )
                },
                0
            );
            usage
        }
        let before = usage(child.0.id());
        let started = Instant::now();
        std::thread::sleep(Duration::from_secs(idle_seconds));
        let after = usage(child.0.id());
        let cpu = after
            .ri_user_time
            .saturating_add(after.ri_system_time)
            .saturating_sub(before.ri_user_time.saturating_add(before.ri_system_time));
        #[repr(C)]
        struct Timebase {
            numer: u32,
            denom: u32,
        }
        unsafe extern "C" {
            #[link_name = "mach_timebase_info"]
            fn read_timebase(info: *mut Timebase) -> libc::c_int;
        }
        let mut timebase = Timebase { numer: 0, denom: 0 };
        assert_eq!(unsafe { read_timebase(&mut timebase) }, 0);
        assert_ne!(timebase.denom, 0);
        let cpu_ns = u128::from(cpu) * u128::from(timebase.numer) / u128::from(timebase.denom);
        eprintln!("native_10002_files: first_paint_us={} first_row_us={} idle_seconds={:.3} idle_cpu_ns={} rss_bytes={} cpu_percent={:.3}",
            first_paint.as_micros(),first_row.as_micros(),started.elapsed().as_secs_f64(),cpu_ns,after.ri_resident_size,
            cpu_ns as f64 / started.elapsed().as_nanos() as f64 * 100.0);
    }
    master.write_all(b"q").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "TUI failed to quit: {}",
            String::from_utf8_lossy(&output[output.len().saturating_sub(4096)..])
        );
        let mut bytes = [0u8; 16_384];
        if let Ok(count) = master.read(&mut bytes) {
            output.extend_from_slice(&bytes[..count]);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut restored = std::mem::MaybeUninit::<libc::termios>::uninit();
    assert_eq!(
        unsafe { libc::tcgetattr(slave.as_raw_fd(), restored.as_mut_ptr()) },
        0
    );
    let restored = unsafe { restored.assume_init() };
    assert_eq!(
        restored.c_lflag & (libc::ICANON | libc::ECHO),
        original.c_lflag & (libc::ICANON | libc::ECHO)
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"preserve this fixture");
    drop(child);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn package_scope_controls_do_not_run_unselected_managers() {
    use std::os::unix::fs::PermissionsExt;
    let root = fixture("inventory_scope");
    let scan = root.join("scan");
    let tools = root.join("tools");
    std::fs::create_dir_all(&tools).unwrap();
    for relative in ["", "keep", "keep/deep", "archive"] {
        let dir = scan.join(relative);
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='1.0.0'\n",
        )
        .unwrap();
    }
    for name in ["brew", "npm", "pip3", "python3", "cargo", "bun"] {
        let executable = tools.join(name);
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf invoked >> \"$HOME/invoked\"\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let run = |arguments: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_diskr"))
            .args(["--packages", "--json"])
            .args(arguments)
            .arg(&scan)
            .env("PATH", &tools)
            .env("HOME", &home)
            .env("CARGO_HOME", root.join("cargo"))
            .output()
            .unwrap()
    };
    let output = run(&[
        "--projects-only",
        "--max-depth",
        "1",
        "--exclude",
        "archive",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["project_dependencies"].as_array().unwrap().len(), 2);
    assert_eq!(report["system_packages"], serde_json::json!([]));
    assert_eq!(
        report["scope"]["inventory"]["managers"],
        serde_json::json!([])
    );
    assert_eq!(report["project_scope"]["depth_limited"], 1);
    assert!(!home.join("invoked").exists());
    let output = run(&["--manager", "cargo", "--no-projects"]);
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["system_packages"].as_array().unwrap().len(), 1);
    assert_eq!(report["system_packages"][0]["manager"], "cargo");
    assert_eq!(report["project_dependencies"], serde_json::json!([]));
    assert!(home.join("invoked").exists());
    std::fs::remove_dir_all(root).unwrap();
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
