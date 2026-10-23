use std::borrow::Cow;
use std::fmt;
use std::path::Path;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug)]
pub enum InputEdit {
    Left,
    Right,
    Home,
    End,
    Backspace,
    Delete,
    Insert(char),
}

#[derive(Clone, Debug, Default)]
pub struct TextCursor {
    byte: usize,
}

impl TextCursor {
    pub fn end(&mut self, text: &str) {
        self.byte = text.len();
    }

    fn position(&self, text: &str) -> usize {
        text.grapheme_indices(true)
            .map(|(index, _)| index)
            .chain(std::iter::once(text.len()))
            .take_while(|&index| index <= self.byte.min(text.len()))
            .last()
            .unwrap_or(0)
    }

    pub fn edit(&mut self, text: &mut String, edit: InputEdit) {
        let position = self.position(text);
        let previous = text[..position]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(index, _)| index);
        let next = text[position..]
            .graphemes(true)
            .next()
            .map_or(position, |part| position + part.len());
        self.byte = position;
        match edit {
            InputEdit::Left => self.byte = previous,
            InputEdit::Right => self.byte = next,
            InputEdit::Home => self.byte = 0,
            InputEdit::End => self.end(text),
            InputEdit::Backspace => {
                text.replace_range(previous..position, "");
                self.byte = previous;
            }
            InputEdit::Delete => {
                text.replace_range(position..next, "");
            }
            InputEdit::Insert(ch) if !ch.is_control() => {
                text.insert(position, ch);
                let inserted_end = position + ch.len_utf8();
                self.byte = text
                    .grapheme_indices(true)
                    .map(|(index, _)| index)
                    .find(|&index| index >= inserted_end)
                    .unwrap_or(text.len());
            }
            InputEdit::Insert(_) => {}
        }
    }

    pub fn viewport(&self, text: &str, columns: usize) -> String {
        if columns == 0 {
            return String::new();
        }
        let position = self.position(text);
        let mut start = position;
        let mut width = 0;
        for (index, grapheme) in text[..position].grapheme_indices(true).rev() {
            let next_width = UnicodeWidthStr::width(sanitize(grapheme).as_ref());
            if width + next_width > columns.saturating_sub(2) {
                break;
            }
            width += next_width;
            start = index;
        }
        let mut result = String::new();
        if start > 0 && columns > 1 {
            result.push('…');
        }
        result.push_str(&sanitize(&text[start..position]));
        result.push('|');
        let mut used = UnicodeWidthStr::width(result.as_str());
        for grapheme in text[position..].graphemes(true) {
            let safe = sanitize(grapheme);
            let next_width = UnicodeWidthStr::width(safe.as_ref());
            if used + next_width > columns {
                break;
            }
            result.push_str(&safe);
            used += next_width;
        }
        result
    }
}

/// Make untrusted text inert before it reaches a terminal. Newlines, tabs,
/// escape bytes, DEL, and C1 controls are rendered visibly instead of being
/// interpreted by a terminal emulator.
pub fn sanitize(text: &str) -> Cow<'_, str> {
    if !text.chars().any(is_terminal_control) {
        return Cow::Borrowed(text);
    }

    let mut safe = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\n' => safe.push_str("\\n"),
            '\r' => safe.push_str("\\r"),
            '\t' => safe.push_str("\\t"),
            '\u{1b}' => safe.push_str("\\x1b"),
            _ if is_terminal_control(ch) => {
                use std::fmt::Write as _;
                let _ = write!(safe, "\\u{{{:x}}}", ch as u32);
            }
            _ => safe.push(ch),
        }
    }
    Cow::Owned(safe)
}

/// Buffer cells already have fixed terminal geometry, so replace each control
/// with one printable column instead of expanding it to a visible escape.
pub fn sanitize_cell(text: &str) -> Cow<'_, str> {
    if !text.chars().any(is_terminal_control) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.chars()
            .map(|ch| if is_terminal_control(ch) { '�' } else { ch })
            .collect(),
    )
}

fn is_terminal_control(ch: char) -> bool {
    ch.is_control() || ('\u{80}'..='\u{9f}').contains(&ch)
}

pub struct SafePath<'a>(pub &'a Path);

impl fmt::Display for SafePath<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&sanitize(&self.0.to_string_lossy()))
    }
}

pub fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

pub fn truncate_end(text: &str, max_width: usize) -> String {
    let text = sanitize(text);
    if display_width(&text) <= max_width {
        return text.into_owned();
    }
    if max_width == 0 {
        return String::new();
    }

    let content_width = max_width.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let width = display_width(grapheme);
        if used + width > content_width {
            break;
        }
        out.push_str(grapheme);
        used += width;
    }
    out.push('…');
    out
}

pub fn truncate_start(text: &str, max_width: usize) -> String {
    let text = sanitize(text);
    if display_width(&text) <= max_width {
        return text.into_owned();
    }
    if max_width == 0 {
        return String::new();
    }

    let content_width = max_width.saturating_sub(1);
    let graphemes: Vec<&str> = text.graphemes(true).collect();
    let mut kept = Vec::new();
    let mut used = 0;
    for grapheme in graphemes.into_iter().rev() {
        let width = display_width(grapheme);
        if used + width > content_width {
            break;
        }
        kept.push(grapheme);
        used += width;
    }
    kept.reverse();
    let mut out = String::from("…");
    for grapheme in kept {
        out.push_str(grapheme);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_edits_whole_graphemes_and_handles_insertion_at_boundaries() {
        let mut text = String::from("a\u{1f44d}\u{1f3fd}e\u{301}界");
        let mut cursor = TextCursor::default();
        cursor.end(&text);
        cursor.edit(&mut text, InputEdit::Left);
        cursor.edit(&mut text, InputEdit::Backspace);
        assert_eq!(text, "a\u{1f44d}\u{1f3fd}界");
        cursor.edit(&mut text, InputEdit::Left);
        cursor.edit(&mut text, InputEdit::Delete);
        assert_eq!(text, "a界");
        cursor.edit(&mut text, InputEdit::Home);
        cursor.edit(&mut text, InputEdit::Insert('e'));
        cursor.edit(&mut text, InputEdit::Insert('\u{301}'));
        cursor.edit(&mut text, InputEdit::Backspace);
        assert_eq!(text, "a界");
        cursor.edit(&mut text, InputEdit::End);
        cursor.edit(&mut text, InputEdit::Insert('!'));
        assert_eq!(text, "a界!");
    }

    #[test]
    fn input_viewport_keeps_caret_inside_cell_budget_and_sanitizes_controls() {
        let text = format!("{}e\u{301}\u{1f44d}\u{1f3fd}\u{1b}[31m", "界".repeat(100));
        let mut cursor = TextCursor::default();
        cursor.end(&text);
        for width in 0..45 {
            let rendered = cursor.viewport(&text, width);
            assert!(
                display_width(&rendered) <= width,
                "width {width}: {rendered}"
            );
            assert!(!rendered.contains('\u{1b}'));
            assert_eq!(rendered.contains('|'), width > 0);
        }
        cursor.edit(&mut text.clone(), InputEdit::Home);
        assert!(cursor.viewport(&text, 6).starts_with('|'));
    }

    #[test]
    fn controls_are_visible_and_inert() {
        assert_eq!(
            sanitize("a\u{1b}[31m\nb\t\u{85}"),
            "a\\x1b[31m\\nb\\t\\u{85}"
        );
        assert_eq!(sanitize_cell("a\u{1b}\nb"), "a��b");
    }

    #[test]
    fn truncation_preserves_graphemes_and_width() {
        assert_eq!(truncate_end("👍🏽abc", 4), "👍🏽a…");
        assert_eq!(display_width(&truncate_end("👍🏽abc", 4)), 4);
        assert_eq!(truncate_end("e\u{301}xyz", 3), "e\u{301}x…");
        assert_eq!(truncate_start("/a/👍🏽/file", 7), "…/file");
    }
}
