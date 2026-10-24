use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) struct CommandResult {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) success: bool,
    pub(crate) timed_out: bool,
}

const MAX_COMMAND_OUTPUT: usize = 16 * 1024 * 1024;

pub(crate) fn run_process(command: Command, timeout: Duration) -> CommandResult {
    run_process_cancellable(command, timeout, || false)
}

pub(crate) fn run_process_cancellable(
    mut command: Command,
    timeout: Duration,
    cancelled: impl Fn() -> bool,
) -> CommandResult {
    if cancelled() {
        return CommandResult {
            stdout: String::new(),
            stderr: String::from("command cancelled"),
            success: false,
            timed_out: false,
        };
    }
    command.process_group(0);
    let mut child = match command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return CommandResult {
                stdout: String::new(),
                stderr: e.to_string(),
                success: false,
                timed_out: false,
            }
        }
    };

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    if let Err(err) = stdout_pipe
        .as_ref()
        .map(set_nonblocking)
        .transpose()
        .and_then(|_| stderr_pipe.as_ref().map(set_nonblocking).transpose())
    {
        terminate_command(&mut child);
        reap_command(child);
        return CommandResult {
            stdout: String::new(),
            stderr: err.to_string(),
            success: false,
            timed_out: false,
        };
    }

    let deadline = Instant::now() + timeout;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut status = None;
    let mut failed = None;
    let mut timed_out = false;
    loop {
        if cancelled() {
            failed = Some(String::from("command cancelled"));
            break;
        }
        if let Err(err) = drain_pipe(&mut stdout_pipe, &mut stdout)
            .and_then(|_| drain_pipe(&mut stderr_pipe, &mut stderr))
        {
            failed = Some(err.to_string());
            break;
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(result) => status = result,
                Err(err) => {
                    failed = Some(err.to_string());
                    break;
                }
            }
        }
        if status.is_some() && stdout_pipe.is_none() && stderr_pipe.is_none() {
            break;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    if timed_out || failed.is_some() {
        // Descendants may inherit either pipe after their direct parent exits.
        terminate_command(&mut child);
    }
    if status.is_none() {
        reap_command(child);
    }
    let command_failed = failed.is_some();
    let mut stderr = String::from_utf8_lossy(&stderr).into_owned();
    if let Some(err) = failed {
        stderr = if stderr.is_empty() {
            err
        } else {
            format!("{err}\n{stderr}")
        };
    }
    CommandResult {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr,
        success: !timed_out && status.is_some_and(|s| s.success()) && !command_failed,
        timed_out,
    }
}

fn set_nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn drain_pipe<R: io::Read>(pipe: &mut Option<R>, output: &mut Vec<u8>) -> io::Result<()> {
    let Some(reader) = pipe.as_mut() else {
        return Ok(());
    };
    let mut buf = [0_u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                *pipe = None;
                return Ok(());
            }
            Ok(n) => {
                if output.len().saturating_add(n) > MAX_COMMAND_OUTPUT {
                    return Err(io::Error::other("command output exceeds 16 MiB"));
                }
                output.extend_from_slice(&buf[..n]);
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
}

fn reap_command(mut child: std::process::Child) {
    thread::spawn(move || {
        let _ = child.wait();
    });
}

fn terminate_command(child: &mut std::process::Child) {
    let pid = child.id() as libc::pid_t;
    if pid <= 0 {
        let _ = child.kill();
        return;
    }

    if unsafe { libc::killpg(pid, libc::SIGKILL) } != 0 {
        let _ = child.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_command(cmd: &str, args: &[&str]) -> CommandResult {
        run_command_with_timeout(cmd, args, Duration::from_secs(10))
    }

    fn run_command_with_timeout(cmd: &str, args: &[&str], timeout: Duration) -> CommandResult {
        let mut command = Command::new(cmd);
        command.args(args);
        run_process(command, timeout)
    }

    #[test]
    fn run_command_captures_stdout_on_success() {
        let result = run_command("echo", &["hello"]);
        assert!(result.success);
        assert!(!result.timed_out);
        assert_eq!(result.stdout.trim(), "hello");
    }

    #[test]
    fn run_command_captures_stderr_on_failure() {
        let result = run_command("ls", &["/nonexistent_path_diskr_test_49"]);
        assert!(!result.success);
        assert!(!result.timed_out);
        assert!(!result.stderr.is_empty());
    }

    #[test]
    fn run_command_times_out_on_slow_process() {
        let result = run_command_with_timeout("sleep", &["60"], Duration::from_millis(200));
        assert!(result.timed_out);
        assert!(!result.success);
    }

    #[test]
    fn run_command_timeout_kills_grandchildren_holding_stdout_open() {
        use std::sync::mpsc;

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = run_command_with_timeout(
                "sh",
                &["-c", "sleep 5 & wait"],
                Duration::from_millis(200),
            );
            tx.send(result).unwrap();
        });

        let result = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("timed-out command should return even when a grandchild inherits stdout");
        assert!(result.timed_out);
        assert!(!result.success);
    }

    #[test]
    fn run_command_deadline_includes_pipe_drain_after_parent_exits() {
        let started = Instant::now();
        let result = run_command_with_timeout(
            "sh",
            &["-c", "sleep 5 & printf 'parent exited\\n'"],
            Duration::from_millis(200),
        );
        assert!(result.timed_out);
        assert!(!result.success);
        assert!(result.stdout.contains("parent exited"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn drain_pipe_enforces_output_bound_at_exact_boundary() {
        let mut within_limit = Some(io::Cursor::new(vec![b'x'; MAX_COMMAND_OUTPUT]));
        let mut output = Vec::new();
        drain_pipe(&mut within_limit, &mut output).unwrap();
        assert!(within_limit.is_none());
        assert_eq!(output.len(), MAX_COMMAND_OUTPUT);

        let mut over_limit = Some(io::Cursor::new(vec![b'x'; MAX_COMMAND_OUTPUT + 1]));
        let mut output = Vec::new();
        let error = drain_pipe(&mut over_limit, &mut output).unwrap_err();
        assert_eq!(error.to_string(), "command output exceeds 16 MiB");
        assert!(output.len() <= MAX_COMMAND_OUTPUT);
    }

    #[test]
    fn run_command_reports_missing_command() {
        let result = run_command("diskr_nonexistent_cmd_49", &[]);
        assert!(!result.success);
        assert!(!result.timed_out);
        assert!(!result.stderr.is_empty());
    }
}
