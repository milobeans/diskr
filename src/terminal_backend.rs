use std::io::{self, Write};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    execute, queue,
    style::{
        Attribute, Color as TermColor, Print, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal::{self, Clear},
};
use ratatui_core::{
    backend::{Backend, ClearType, WindowSize},
    buffer::Cell,
    layout::{Position, Size},
    style::{Color, Modifier},
};

use crate::terminal_text;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Theme {
    Dark,
    Light,
    HighContrast,
    Monochrome,
}

#[derive(Clone, Copy, Debug)]
pub struct Appearance {
    theme: Theme,
    pub reduced_motion: bool,
    pub ascii: bool,
}

impl Appearance {
    fn from_settings(theme: &str, no_color: bool, reduced_motion: bool, ascii: bool) -> Self {
        Self {
            theme: if no_color {
                Theme::Monochrome
            } else {
                match theme {
                    "light" => Theme::Light,
                    "high-contrast" => Theme::HighContrast,
                    "mono" | "monochrome" => Theme::Monochrome,
                    _ => Theme::Dark,
                }
            },
            reduced_motion,
            ascii,
        }
    }

    pub fn configured() -> &'static Self {
        static APPEARANCE: std::sync::OnceLock<Appearance> = std::sync::OnceLock::new();
        APPEARANCE.get_or_init(|| {
            Self::from_settings(
                &std::env::var("DISKR_THEME").unwrap_or_default(),
                std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()),
                std::env::var("DISKR_REDUCED_MOTION").is_ok_and(|value| value == "1"),
                std::env::var("DISKR_ASCII").is_ok_and(|value| value == "1"),
            )
        })
    }

    fn color(self, color: Color, background: bool) -> Color {
        if self.theme == Theme::Monochrome {
            return Color::Reset;
        }
        if background {
            return match (self.theme, color) {
                (Theme::Light, Color::Reset) => Color::White,
                (Theme::Dark | Theme::HighContrast, Color::Reset) => Color::Black,
                (Theme::Light, Color::DarkGray) => Color::Indexed(253),
                (Theme::Dark, Color::DarkGray) => Color::Indexed(235),
                (Theme::HighContrast, Color::DarkGray) => Color::Black,
                _ => color,
            };
        }
        match (self.theme, color) {
            (Theme::Light, Color::Reset) => Color::Black,
            (Theme::Dark | Theme::HighContrast, Color::Reset) => Color::White,
            (Theme::Light, Color::White | Color::Gray) => Color::Black,
            (Theme::Light, Color::DarkGray) => Color::Indexed(238),
            (Theme::Light, Color::Yellow) => Color::Indexed(130),
            (Theme::Light, Color::Green) => Color::Indexed(28),
            (Theme::Light, Color::Cyan) => Color::Indexed(23),
            (Theme::HighContrast, Color::DarkGray | Color::Gray) => Color::White,
            (Theme::HighContrast, Color::Blue | Color::Cyan) => Color::LightCyan,
            (Theme::HighContrast, Color::Red) => Color::LightRed,
            (Theme::HighContrast, Color::Green) => Color::LightGreen,
            (Theme::HighContrast, Color::Yellow) => Color::LightYellow,
            (Theme::Dark, Color::Blue) => Color::LightBlue,
            (Theme::Dark, Color::DarkGray) => Color::Indexed(246),
            _ => color,
        }
    }
}

pub struct CrosstermBackend<W: Write> {
    writer: W,
    appearance: Appearance,
}

impl<W: Write> CrosstermBackend<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            appearance: *Appearance::configured(),
        }
    }
}

impl<W: Write> Write for CrosstermBackend<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

impl<W: Write> Backend for CrosstermBackend<W> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut fg = Color::Reset;
        let mut bg = Color::Reset;
        let mut modifier = Modifier::empty();
        let mut last_pos: Option<Position> = None;

        for (x, y, cell) in content {
            if !matches!(last_pos, Some(pos) if x == pos.x + 1 && y == pos.y) {
                queue!(self.writer, MoveTo(x, y))?;
            }
            last_pos = Some(Position { x, y });

            if cell.modifier != modifier {
                queue_modifier(&mut self.writer, cell.modifier)?;
                modifier = cell.modifier;
                fg = Color::Reset;
                bg = Color::Reset;
            }
            let cell_fg = self.appearance.color(cell.fg, false);
            let cell_bg = self.appearance.color(cell.bg, true);
            if cell_fg != fg {
                queue!(self.writer, SetForegroundColor(to_term_color(cell_fg)))?;
                fg = cell_fg;
            }
            if cell_bg != bg {
                queue!(self.writer, SetBackgroundColor(to_term_color(cell_bg)))?;
                bg = cell_bg;
            }
            queue!(
                self.writer,
                Print(terminal_text::sanitize_cell(cell.symbol()))
            )?;
        }

        queue!(
            self.writer,
            SetForegroundColor(TermColor::Reset),
            SetBackgroundColor(TermColor::Reset),
            SetAttribute(Attribute::Reset),
        )
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        for _ in 0..n {
            queue!(self.writer, Print("\n"))?;
        }
        self.writer.flush()
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        execute!(self.writer, Hide)
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        execute!(self.writer, Show)
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        crossterm::cursor::position()
            .map(|(x, y)| Position { x, y })
            .map_err(io::Error::other)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let Position { x, y } = position.into();
        execute!(self.writer, MoveTo(x, y))
    }

    fn clear(&mut self) -> io::Result<()> {
        self.clear_region(ClearType::All)
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        execute!(
            self.writer,
            Clear(match clear_type {
                ClearType::All => terminal::ClearType::All,
                ClearType::AfterCursor => terminal::ClearType::FromCursorDown,
                ClearType::BeforeCursor => terminal::ClearType::FromCursorUp,
                ClearType::CurrentLine => terminal::ClearType::CurrentLine,
                ClearType::UntilNewLine => terminal::ClearType::UntilNewLine,
            })
        )
    }

    fn size(&self) -> io::Result<Size> {
        let (width, height) = terminal::size()?;
        Ok(Size { width, height })
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        let crossterm::terminal::WindowSize {
            columns,
            rows,
            width,
            height,
        } = terminal::window_size()?;

        Ok(WindowSize {
            columns_rows: Size {
                width: columns,
                height: rows,
            },
            pixels: Size { width, height },
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

fn to_term_color(color: Color) -> TermColor {
    match color {
        Color::Reset => TermColor::Reset,
        Color::Black => TermColor::Black,
        Color::Red => TermColor::DarkRed,
        Color::Green => TermColor::DarkGreen,
        Color::Yellow => TermColor::DarkYellow,
        Color::Blue => TermColor::DarkBlue,
        Color::Magenta => TermColor::DarkMagenta,
        Color::Cyan => TermColor::DarkCyan,
        Color::Gray => TermColor::Grey,
        Color::DarkGray => TermColor::DarkGrey,
        Color::LightRed => TermColor::Red,
        Color::LightGreen => TermColor::Green,
        Color::LightYellow => TermColor::Yellow,
        Color::LightBlue => TermColor::Blue,
        Color::LightMagenta => TermColor::Magenta,
        Color::LightCyan => TermColor::Cyan,
        Color::White => TermColor::White,
        Color::Indexed(value) => TermColor::AnsiValue(value),
        Color::Rgb(r, g, b) => TermColor::Rgb { r, g, b },
    }
}

fn queue_modifier<W: Write>(writer: &mut W, modifier: Modifier) -> io::Result<()> {
    queue!(writer, SetAttribute(Attribute::Reset))?;

    if modifier.contains(Modifier::REVERSED) {
        queue!(writer, SetAttribute(Attribute::Reverse))?;
    }
    if modifier.contains(Modifier::BOLD) {
        queue!(writer, SetAttribute(Attribute::Bold))?;
    }
    if modifier.contains(Modifier::ITALIC) {
        queue!(writer, SetAttribute(Attribute::Italic))?;
    }
    if modifier.contains(Modifier::UNDERLINED) {
        queue!(writer, SetAttribute(Attribute::Underlined))?;
    }
    if modifier.contains(Modifier::DIM) {
        queue!(writer, SetAttribute(Attribute::Dim))?;
    }
    if modifier.contains(Modifier::CROSSED_OUT) {
        queue!(writer, SetAttribute(Attribute::CrossedOut))?;
    }
    if modifier.contains(Modifier::SLOW_BLINK) {
        queue!(writer, SetAttribute(Attribute::SlowBlink))?;
    }
    if modifier.contains(Modifier::RAPID_BLINK) {
        queue!(writer, SetAttribute(Attribute::RapidBlink))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palettes_map_metadata_highlights_and_no_color_consistently() {
        let dark = Appearance::from_settings("dark", false, false, false);
        let light = Appearance::from_settings("light", false, false, false);
        let contrast = Appearance::from_settings("high-contrast", false, false, false);
        assert_eq!(dark.color(Color::DarkGray, false), Color::Indexed(246));
        assert_eq!(light.color(Color::DarkGray, false), Color::Indexed(238));
        assert_eq!(light.color(Color::White, false), Color::Black);
        assert_eq!(light.color(Color::DarkGray, true), Color::Indexed(253));
        assert_eq!(contrast.color(Color::DarkGray, false), Color::White);
        assert_eq!(contrast.color(Color::DarkGray, true), Color::Black);
        for setting in ["dark", "light", "high-contrast", "mono"] {
            let appearance = Appearance::from_settings(setting, true, true, true);
            for color in [Color::Red, Color::Gray, Color::Blue, Color::White] {
                assert_eq!(appearance.color(color, false), Color::Reset);
                assert_eq!(appearance.color(color, true), Color::Reset);
            }
            assert!(appearance.reduced_motion && appearance.ascii);
        }
    }

    #[test]
    fn monochrome_backend_preserves_unicode_data_and_redundant_text() {
        let mut backend = CrosstermBackend {
            writer: Vec::new(),
            appearance: Appearance::from_settings("mono", false, true, true),
        };
        let mut cell = Cell::default();
        cell.set_symbol("界▶failed");
        cell.fg = Color::Red;
        cell.bg = Color::DarkGray;
        cell.modifier = Modifier::BOLD;
        backend.draw(std::iter::once((0, 0, &cell))).unwrap();
        let output = String::from_utf8(backend.writer).unwrap();
        assert!(output.contains("界▶failed"));
        assert!(output.contains("[1m"));
        assert!(!output.contains("38;"));
        assert!(!output.contains("48;"));
    }
}
