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
