mod app;
mod bulkstat;
mod fs_ops;
mod scanner;
mod terminal_backend;
mod ui;

use anyhow::{bail, Result};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui_core::terminal::Terminal;
use std::{
    ffi::OsString,
    io::{self, IsTerminal},
    path::PathBuf,
    time::Duration,
};

use app::{App, Focus};
use terminal_backend::CrosstermBackend;

fn main() -> Result<()> {
    match parse_args(std::env::args_os().skip(1))? {
        CliAction::Help => {
            print_help();
            Ok(())
        }
        CliAction::Version => {
            println!("diskr {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        CliAction::Run(start) => run_app(start),
    }
}

fn run_app(start: PathBuf) -> Result<()> {
    if !start.exists() {
        bail!("path does not exist: {}", start.display());
    }
    if !start.is_dir() {
        bail!("path is not a directory: {}", start.display());
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("diskr requires an interactive terminal");
    }

    let mut app = App::new(start)?;

    let _terminal_guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let res = run(&mut terminal, &mut app);
    let cursor_res = terminal.show_cursor();

    res?;
    cursor_res?;
    Ok(())
}

enum CliAction {
    Run(PathBuf),
    Help,
    Version,
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<CliAction> {
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Ok(CliAction::Run(dirs_home()));
    };

    match first.to_string_lossy().as_ref() {
        "-h" | "--help" => Ok(CliAction::Help),
        "-V" | "--version" => Ok(CliAction::Version),
        "--" => {
            let Some(path) = args.next() else {
                bail!("usage: diskr [PATH]");
            };
            if args.next().is_some() {
                bail!("usage: diskr [PATH]");
            }
            Ok(CliAction::Run(PathBuf::from(path)))
        }
        _ => {
            if args.next().is_some() {
                bail!("usage: diskr [PATH]");
            }
            Ok(CliAction::Run(PathBuf::from(first)))
        }
    }
}

fn print_help() {
    println!(
        "\
diskr {}

Lightweight terminal file explorer and disk/storage manager for macOS.

Usage:
  diskr [PATH]
  diskr -- PATH

Keys:
  Up/Down, j/k    Move selection
  Enter           Open selected directory or disk
  Backspace       Go to parent directory
  r               Rescan directory sizes
  o               Cycle sort mode
  .               Toggle hidden files
  d               Move selected item to Trash
  Tab             Switch files/disks pane
  q, Esc          Quit
",
        env!("CARGO_PKG_VERSION")
    );
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(err) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
            let _ = disable_raw_mode();
            return Err(err.into());
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
    }
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/"))
}

fn run<B>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()>
where
    B: ratatui_core::backend::Backend<Error = io::Error>,
{
    let mut needs_draw = true;

    loop {
        if app.drain_scan_results() {
            needs_draw = true;
        }

        if needs_draw {
            terminal.draw(|f| ui::draw(f, app))?;
            needs_draw = false;
        }

        let timeout = if app.has_pending_scan_work() {
            Duration::from_millis(50)
        } else {
            Duration::from_secs(1)
        };

        if event::poll(timeout)? {
            match event::read()? {
                Event::Resize(_, _) => {
                    needs_draw = true;
                }
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    if app.confirming_delete {
                        let handled = match key.code {
                            KeyCode::Char('y') => {
                                app.confirm_delete()?;
                                true
                            }
                            KeyCode::Char('n') | KeyCode::Esc => {
                                app.cancel_delete();
                                true
                            }
                            _ => false,
                        };
                        if handled {
                            needs_draw = true;
                        }
                        continue;
                    }
                    let handled = match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Down | KeyCode::Char('j') => {
                            app.move_cursor(1);
                            true
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            app.move_cursor(-1);
                            true
                        }
                        KeyCode::Enter => {
                            app.enter()?;
                            true
                        }
                        KeyCode::Backspace => {
                            app.go_up()?;
                            true
                        }
                        KeyCode::Char('r') => {
                            app.force_rescan();
                            true
                        }
                        KeyCode::Char('d') => {
                            app.request_delete();
                            true
                        }
                        KeyCode::Char('o') => {
                            app.cycle_sort();
                            true
                        }
                        KeyCode::Char('.') => {
                            app.toggle_hidden()?;
                            true
                        }
                        KeyCode::Tab => {
                            app.focus = match app.focus {
                                Focus::Files => Focus::Disks,
                                Focus::Disks => Focus::Files,
                            };
                            true
                        }
                        _ => false,
                    };
                    if handled {
                        needs_draw = true;
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(parts: &[&str]) -> Result<CliAction> {
        parse_args(parts.iter().map(OsString::from))
    }

    #[test]
    fn defaults_to_home_without_args() {
        assert!(matches!(parse(&[]).unwrap(), CliAction::Run(_)));
    }

    #[test]
    fn accepts_one_path() {
        let action = parse(&["/tmp"]).unwrap();
        assert!(matches!(action, CliAction::Run(path) if path == std::path::Path::new("/tmp")));
    }

    #[test]
    fn accepts_dash_prefixed_path_after_separator() {
        let action = parse(&["--", "-cache"]).unwrap();
        assert!(matches!(action, CliAction::Run(path) if path == std::path::Path::new("-cache")));
    }

    #[test]
    fn separator_requires_path() {
        let err = parse(&["--"]).err().unwrap();
        assert!(err.to_string().contains("usage: diskr [PATH]"));
    }

    #[test]
    fn parses_help_and_version_flags() {
        assert!(matches!(parse(&["--help"]).unwrap(), CliAction::Help));
        assert!(matches!(parse(&["-h"]).unwrap(), CliAction::Help));
        assert!(matches!(parse(&["--version"]).unwrap(), CliAction::Version));
        assert!(matches!(parse(&["-V"]).unwrap(), CliAction::Version));
    }

    #[test]
    fn rejects_extra_args() {
        assert!(parse(&["/tmp", "/var"]).is_err());
        assert!(parse(&["--", "/tmp", "/var"]).is_err());
    }
}
