use anyhow::Result;
use crossterm::event::KeyCode;

use crate::app::{ActiveModal, App};
use crate::terminal_text::InputEdit;

pub fn handle_overlay_key(app: &mut App, code: KeyCode) -> Result<Option<bool>> {
    let handled = match app.active_modal() {
        ActiveModal::Help => match code {
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                app.close_help();
                true
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.scroll_help(1);
                true
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.scroll_help(-1);
                true
            }
            KeyCode::PageDown => {
                app.scroll_help(10);
                true
            }
            KeyCode::PageUp => {
                app.scroll_help(-10);
                true
            }
            KeyCode::Home => {
                app.help_scroll = 0;
                true
            }
            KeyCode::End => {
                app.help_scroll = usize::MAX;
                true
            }
            _ => false,
        },
        ActiveModal::Locations => handle_locations_key(app, code),
        ActiveModal::Delete => match code {
            KeyCode::Char('y') => {
                app.confirm_delete()?;
                true
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                app.cancel_delete();
                true
            }
            _ => false,
        },
        ActiveModal::Uninstall => match code {
            KeyCode::Char('y') => {
                app.confirm_uninstall();
                true
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                app.cancel_uninstall();
                true
            }
            _ => false,
        },
        ActiveModal::EmptyTrash => match code {
            KeyCode::Char('y') => {
                app.confirm_empty_trash();
                true
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                app.cancel_empty_trash();
                true
            }
            _ => false,
        },
        _ => return Ok(None),
    };
    Ok(Some(handled))
}

fn handle_locations_key(app: &mut App, code: KeyCode) -> bool {
    if app.locations.path_input.is_some() {
        let edit = match code {
            KeyCode::Esc => {
                app.locations.cancel();
                return true;
            }
            KeyCode::Enter => {
                app.locations.activate(&app.cwd, app.home.as_deref());
                return true;
            }
            KeyCode::Left => InputEdit::Left,
            KeyCode::Right => InputEdit::Right,
            KeyCode::Home => InputEdit::Home,
            KeyCode::End => InputEdit::End,
            KeyCode::Backspace => InputEdit::Backspace,
            KeyCode::Delete => InputEdit::Delete,
            KeyCode::Char(ch) if ch != '\0' => InputEdit::Insert(ch),
            _ => return false,
        };
        app.locations.edit_path(edit);
        return true;
    }
    match code {
        KeyCode::Esc | KeyCode::Char('g') | KeyCode::Char('q') => app.locations.close(),
        KeyCode::Down | KeyCode::Char('j') => app.locations.move_selection(1),
        KeyCode::Up | KeyCode::Char('k') => app.locations.move_selection(-1),
        KeyCode::PageDown => app.locations.move_selection(10),
        KeyCode::PageUp => app.locations.move_selection(-10),
        KeyCode::Home => app.locations.set_selection(0),
        KeyCode::End => app.locations.set_selection(usize::MAX),
        KeyCode::Char('/') => app.locations.enter_path(&app.cwd),
        KeyCode::Enter => app.locations.activate(&app.cwd, app.home.as_deref()),
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chooser_text_and_modal_keys_are_confined_to_the_active_view() {
        let root =
            std::env::temp_dir().join(format!("diskr-chooser-keys-{}", crate::state::next_epoch()));
        std::fs::create_dir(&root).unwrap();
        let mut app = App::new(root.clone()).unwrap();
        app.locations.open = true;
        app.locations.path_input = Some(String::new());
        assert_eq!(
            handle_overlay_key(&mut app, KeyCode::Char('g')).unwrap(),
            Some(true)
        );
        assert_eq!(app.locations.path_input.as_deref(), Some("g"));
        assert_eq!(
            handle_overlay_key(&mut app, KeyCode::Esc).unwrap(),
            Some(true)
        );
        assert!(app.locations.open);
        assert!(app.locations.path_input.is_none());
        handle_overlay_key(&mut app, KeyCode::Char('g')).unwrap();
        assert!(!app.locations.open);
        assert_eq!(
            handle_overlay_key(&mut app, KeyCode::Char('y')).unwrap(),
            None
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
