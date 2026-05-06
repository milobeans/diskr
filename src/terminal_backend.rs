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

pub struct CrosstermBackend<W: Write> {
    writer: W,
}

impl<W: Write> CrosstermBackend<W> {
    pub const fn new(writer: W) -> Self {
        Self { writer }
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
            if cell.fg != fg {
                queue!(self.writer, SetForegroundColor(to_term_color(cell.fg)))?;
                fg = cell.fg;
            }
            if cell.bg != bg {
                queue!(self.writer, SetBackgroundColor(to_term_color(cell.bg)))?;
                bg = cell.bg;
            }
            queue!(self.writer, Print(cell.symbol()))?;
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
