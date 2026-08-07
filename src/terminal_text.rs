use std::borrow::Cow;
use std::fmt;
use std::path::Path;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

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
