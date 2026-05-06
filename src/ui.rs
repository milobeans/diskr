use ratatui_core::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    terminal::Frame,
    text::{Line, Span},
};
use ratatui_widgets::{
    block::Block,
    borders::Borders,
    clear::Clear,
    gauge::Gauge,
    list::{List, ListItem, ListState},
    paragraph::{Paragraph, Wrap},
};

use crate::app::{human, App, Focus};

pub fn draw(f: &mut Frame, app: &App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(f.area());

    draw_header(f, root[0], app);

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(root[1]);

    draw_files(f, body[0], app);
    draw_disks(f, body[1], app);

    draw_status(f, root[2], app);
    draw_help(f, root[3]);

    if app.confirming_delete {
        draw_confirm(f, app);
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let text = Line::from(vec![
        Span::styled("diskr ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("· "),
        Span::styled(
            app.cwd.display().to_string(),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw(format!(
            "   [sort: {}, hidden: {}]",
            app.sort.label(),
            if app.show_hidden { "on" } else { "off" }
        )),
    ]);
    f.render_widget(Paragraph::new(text), area);
}

fn draw_files(f: &mut Frame, area: Rect, app: &App) {
    let border_color = if app.focus == Focus::Files {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    let items: Vec<ListItem> = app
        .entries
        .iter()
        .map(|e| {
            let size_str = match (e.is_dir, e.size, e.scanning) {
                (true, _, true) => String::from("scanning…"),
                (true, Some(n), _) => human(n),
                (true, None, _) => String::from("—"),
                (false, Some(n), _) => human(n),
                (false, None, _) => String::from("?"),
            };
            let icon = if e.is_dir { "▸ " } else { "  " };
            let name_style = if e.is_dir {
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let line = Line::from(vec![
                Span::styled(icon, Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{:<40}", truncate(&e.name, 40)), name_style),
                Span::raw("  "),
                Span::styled(
                    format!("{:>12}", size_str),
                    Style::default().fg(Color::Green),
                ),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("files")
                .border_style(Style::default().fg(border_color)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    let mut state = ListState::default();
    state.select(Some(app.selected));
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_disks(f: &mut Frame, area: Rect, app: &App) {
    let border_color = if app.focus == Focus::Disks {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title("disks")
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.disks.is_empty() {
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            app.disks
                .iter()
                .map(|_| Constraint::Length(4))
                .collect::<Vec<_>>(),
        )
        .split(inner);

    for (i, d) in app.disks.iter().enumerate() {
        if i >= rows.len() {
            break;
        }
        let selected = app.focus == Focus::Disks && i == app.selected_disk;
        let used = d.total.saturating_sub(d.available);
        let pct = if d.total > 0 {
            (used as f64 / d.total as f64 * 100.0) as u16
        } else {
            0
        };
        let disk_name = if d.name.is_empty() {
            d.mount.display().to_string()
        } else {
            format!("{}  {}", d.name, d.mount.display())
        };
        let label = if selected {
            format!("> {}  {} / {}", disk_name, human(used), human(d.total))
        } else {
            format!("{}  {} / {}", disk_name, human(used), human(d.total))
        };
        let color = if pct > 90 {
            Color::Red
        } else if pct > 75 {
            Color::Yellow
        } else {
            Color::Green
        };
        let mut gauge_style = Style::default().fg(color);
        if selected {
            gauge_style = gauge_style.add_modifier(Modifier::BOLD);
        }
        let gauge = Gauge::default()
            .block(Block::default().title(label))
            .gauge_style(gauge_style)
            .percent(pct.min(100));
        f.render_widget(gauge, rows[i]);
    }
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let text = Line::from(Span::styled(&app.status, Style::default().fg(Color::White)));
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), area);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let key = |k: &'static str| {
        Span::styled(
            k,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    };
    let label = |s: &'static str| Span::styled(s, Style::default().fg(Color::Gray));
    let sep = || Span::styled(" · ", Style::default().fg(Color::DarkGray));

    let text = Line::from(vec![
        key("↑↓/jk"),
        label(" move"),
        sep(),
        key("⏎"),
        label(" open"),
        sep(),
        key("⌫"),
        label(" up"),
        sep(),
        key("r"),
        label(" reload"),
        sep(),
        key("o"),
        label(" sort"),
        sep(),
        key("."),
        label(" hidden"),
        sep(),
        key("d"),
        label(" trash"),
        sep(),
        key("Tab"),
        label(" pane"),
        sep(),
        key("q"),
        label(" quit"),
    ]);
    f.render_widget(Paragraph::new(text), area);
}

fn draw_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(60, 20, f.area());
    f.render_widget(Clear, area);
    let name = app.pending_delete_name();
    let body = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("Move to Trash: {}", name),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("press  y  to confirm   ·   n  to cancel"),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .title("confirm")
        .border_style(Style::default().fg(Color::Red));
    f.render_widget(
        Paragraph::new(body)
            .block(block)
            .alignment(Alignment::Center),
        area,
    );
}

fn centered_rect(px: u16, py: u16, area: Rect) -> Rect {
    let popup = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - py) / 2),
            Constraint::Percentage(py),
            Constraint::Percentage((100 - py) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - px) / 2),
            Constraint::Percentage(px),
            Constraint::Percentage((100 - px) / 2),
        ])
        .split(popup[1])[1]
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
