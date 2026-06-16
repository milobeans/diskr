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
use std::time::{SystemTime, UNIX_EPOCH};

use crate::app::{
    format_elapsed, format_full_timestamp, format_modified_time, human, size_sort_key, App, Focus,
    InputMode, PkgView, SortMode,
};
use crate::bulkstat::SizeInfo;
use crate::keymap::{self, KeySection};
use crate::packages::{DepEvidence, PackageUseStatus};
use crate::reclaim::Reclaimability;

pub fn draw(f: &mut Frame, app: &mut App) {
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

    let packages_visible =
        app.focus == Focus::Packages || app.packages_loaded || app.packages_loading;
    // Collapse the side column to a compact one-line-per-disk summary while
    // browsing files, giving the files pane the reclaimed width. It expands
    // back to full gauges when Disks/Packages is focused or packages load.
    let side_expanded = app.focus == Focus::Disks || packages_visible;
    let (files_pct, side_pct) = if side_expanded { (62, 38) } else { (74, 26) };
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(files_pct),
            Constraint::Percentage(side_pct),
        ])
        .split(root[1]);

    let (disk_height, package_height) =
        side_panel_heights(body[1].height, app.disks.len(), packages_visible);

    let side = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(disk_height),
            Constraint::Length(package_height),
        ])
        .split(body[1]);

    draw_files(f, body[0], app);
    draw_disks(f, side[0], app, side_expanded);
    draw_packages(f, side[1], app);

    draw_status(f, root[2], app);
    draw_help(f, root[3], app.focus);

    if app.focus == Focus::Reclaim && !app.reclaim_paths_open() {
        draw_reclaim_panel(f, app);
    }
    if app.top_files_open() {
        draw_top_files(f, app);
    }
    if app.reclaim_paths_open() {
        draw_reclaim_paths(f, app);
    }
    if app.disk_info_open() {
        draw_disk_info_modal(f, app);
    }
    if app.confirming_delete {
        draw_confirm(f, app);
    }
    if app.confirming_uninstall {
        draw_uninstall_confirm(f, app);
    }
    if app.confirming_empty_trash {
        draw_empty_trash_confirm(f, app);
    }
    if app.pkg_detail {
        match app.pkg_view {
            PkgView::SystemManagers => draw_pkg_detail(f, app),
            PkgView::ProjectDeps => draw_project_dep_detail(f, app),
        }
    }
    if app.file_info_open {
        draw_file_info(f, app);
    }
    if app.input_mode != InputMode::None {
        draw_input_overlay(f, app);
    }
    if app.show_help {
        draw_help_overlay(f);
    }
}

fn draw_input_overlay(f: &mut Frame, app: &App) {
    let area = centered_rect(50, 20, 40, 6, f.area());
    let block = Block::default()
        .title(app.input_prompt.as_str())
        .borders(Borders::ALL);
    let text = vec![Line::from(""), Line::from(format!("{}▏", app.input_buffer))];
    let paragraph = Paragraph::new(text).block(block).wrap(Wrap { trim: false });
    f.render_widget(Clear, area);
    f.render_widget(paragraph, area);
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let path = truncate_start(
        &app.cwd.display().to_string(),
        area.width.saturating_sub(30) as usize,
    );
    // Only surface non-default state in the header (the sort mode lives in the
    // files-pane title, and "hidden off" is the default). This keeps the line
    // about the location, not about settings that rarely change.
    let mut spans = vec![
        Span::styled("diskr", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" · "),
        Span::styled(path, Style::default().fg(Color::Cyan)),
    ];
    if app.show_hidden {
        spans.push(Span::styled(
            " · hidden on",
            Style::default().fg(Color::Gray),
        ));
    }
    if let Some(baseline) = app.history_baseline_status() {
        spans.push(Span::styled(
            format!(" · {baseline}"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if let Some(delta) = app.history_delta_status() {
        spans.push(Span::styled(
            format!(" · {delta}"),
            Style::default().fg(Color::Green),
        ));
    }
    if app.fda_limited {
        spans.push(Span::styled(
            " · limited access (no Full Disk Access)",
            Style::default().fg(Color::Yellow),
        ));
    }
    let text = Line::from(spans);
    f.render_widget(Paragraph::new(text), area);
}

fn draw_files(f: &mut Frame, area: Rect, app: &mut App) {
    app.files_area = area;
    let border_color = if app.focus == Focus::Files {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    // Show the modified column when sorting by mtime, or whenever the pane is
    // wide enough to spare the room.
    const MODIFIED_AUTO_WIDTH: u16 = 100;
    let show_modified = app.sort == SortMode::Modified || area.width >= MODIFIED_AUTO_WIDTH;
    let (name_width, size_width, modified_width, bar_width) =
        file_columns(area.width.saturating_sub(2), show_modified);
    let visible_count = app.visible_entry_count();
    if visible_count == 0 {
        let block = Block::default()
            .borders(Borders::ALL)
            .title("files · 0 items")
            .border_style(Style::default().fg(border_color));
        let message = if app.entries.is_empty() {
            "empty directory"
        } else {
            "no matching entries"
        };
        f.render_widget(
            Paragraph::new(message)
                .block(block)
                .alignment(Alignment::Center),
            area,
        );
        return;
    }
    let max_rows = area.height.saturating_sub(2).max(1) as usize;
    let (offset, end) =
        file_window_bounds(app.selected, visible_count, app.file_list_offset, max_rows);
    app.file_list_offset = offset;
    let max_visible_size = (0..visible_count)
        .filter_map(|i| app.visible_entry(i))
        .filter_map(|entry| entry.size.map(size_sort_key))
        .max()
        .unwrap_or(0);
    let total_visible_size: u64 = (0..visible_count)
        .filter_map(|i| app.visible_entry(i))
        .filter_map(|entry| entry.size.map(size_sort_key))
        .sum();
    let sized_count = (0..visible_count)
        .filter_map(|i| app.visible_entry(i))
        .filter(|entry| entry.size.is_some())
        .count();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(files_pane_title(
            app,
            visible_count,
            total_visible_size,
            sized_count,
        ))
        .border_style(Style::default().fg(border_color));
    let visible_entries: Vec<&crate::app::Entry> = (offset..end)
        .filter_map(|visible_index| app.visible_entry(visible_index))
        .collect();

    let items: Vec<ListItem> = visible_entries
        .into_iter()
        .map(|e| {
            let (size_str, size_style) = match (e.is_dir, e.size, e.scanning) {
                (true, _, true) => (
                    format!("{} scanning…", spinner_char()),
                    Style::default().fg(Color::Cyan),
                ),
                (true, Some(size), _) => (
                    size_with_markers(
                        size_sort_key(size),
                        e.inaccessible,
                        e.skipped_mounts,
                        e.size_stale,
                    ),
                    if e.size_stale {
                        Style::default().fg(Color::DarkGray)
                    } else {
                        size_magnitude_style(size_sort_key(size))
                    },
                ),
                (true, None, _) => (String::from("—"), Style::default().fg(Color::DarkGray)),
                (false, Some(size), _) if e.is_symlink => (
                    human(size_sort_key(size)),
                    size_magnitude_style(size_sort_key(size)),
                ),
                (false, None, _) if e.is_symlink => {
                    (String::from("link"), Style::default().fg(Color::DarkGray))
                }
                (false, Some(size), _) => (
                    human(size_sort_key(size)),
                    size_magnitude_style(size_sort_key(size)),
                ),
                (false, None, _) => (String::from("?"), Style::default().fg(Color::DarkGray)),
            };
            // Marked rows keep their type glyph next to the check so the icon
            // column still tells files from directories.
            let marked = app.is_marked(&e.path);
            let icon = if marked {
                if e.is_dir {
                    "✓▸"
                } else if e.is_symlink {
                    "✓↪"
                } else {
                    "✓ "
                }
            } else if e.is_dir {
                "▸ "
            } else if e.is_symlink {
                "↪ "
            } else {
                "  "
            };
            let icon_style = if marked {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let name_style = if e.is_dir {
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let mut spans = vec![
                Span::styled(icon, icon_style),
                Span::styled(pad_truncated(&e.name, name_width), name_style),
            ];
            let (bar_text, bar_percent, bar_style) = file_size_bar(
                e.size.map(size_sort_key),
                max_visible_size,
                total_visible_size,
                bar_width,
            );
            if size_width > 0 {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(format!("{size_str:>size_width$}"), size_style));
            }
            if bar_width > 0 {
                spans.push(Span::raw("  "));
                if e.scanning {
                    spans.push(Span::raw(" ".repeat(bar_width)));
                    spans.push(Span::raw(" "));
                    spans.push(Span::raw("    "));
                } else {
                    spans.push(Span::styled(bar_text, bar_style));
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(
                        bar_percent,
                        Style::default().fg(Color::DarkGray),
                    ));
                }
            }
            if show_modified && modified_width > 0 {
                let modified = format_modified_time(e.modified);
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    format!("{modified:>modified_width$}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            // Growth badge for directories that changed since their last scan.
            if !e.scanning {
                if let Some((badge, badge_style)) = e.size_delta.and_then(growth_badge) {
                    spans.push(Span::styled(badge, badge_style));
                }
            }
            // Reclaim class chip for rows the reclaim engine recognizes
            // (node_modules, target, ~/Library/Caches, …) — name/path only,
            // no I/O on the render path.
            if let Some(class) =
                crate::reclaim::classify_by_name_path(&e.name, &e.path, app.home.as_deref())
            {
                let (chip, chip_color) = reclaim_chip(class);
                spans.push(Span::styled(
                    format!(" [{chip}]"),
                    Style::default().fg(chip_color),
                ));
            }
            let line = Line::from(spans);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    let mut state = ListState::default();
    state.select(Some(app.selected.saturating_sub(offset)));
    f.render_stateful_widget(list, area, &mut state);
}

fn file_window_bounds(
    selected: usize,
    total_items: usize,
    current_offset: usize,
    max_rows: usize,
) -> (usize, usize) {
    if total_items == 0 {
        return (0, 0);
    }

    let max_rows = max_rows.max(1);
    let max_offset = total_items.saturating_sub(max_rows);
    let mut offset = current_offset.min(max_offset);
    let selected = selected.min(total_items.saturating_sub(1));

    if selected < offset {
        offset = selected;
    } else if selected >= offset + max_rows {
        offset = selected + 1 - max_rows;
    }

    let end = (offset + max_rows).min(total_items);
    (offset, end)
}

fn draw_disks(f: &mut Frame, area: Rect, app: &mut App, expanded: bool) {
    app.disk_page_rows = 1;
    if area.height == 0 || area.width == 0 {
        return;
    }
    let border_color = if app.focus == Focus::Disks {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("disks ({})", app.disks.len()))
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.disks.is_empty() {
        f.render_widget(
            Paragraph::new("no disks detected").alignment(Alignment::Center),
            inner,
        );
        return;
    }
    if !expanded {
        // Collapsed: one compact line per disk so the side column stays out of
        // the files pane's way while browsing.
        let max_rows = inner.height.max(1) as usize;
        app.disk_page_rows = max_rows;
        let (offset, end) = disk_window_bounds(app.selected_disk, app.disks.len(), max_rows);
        let lines: Vec<Line> = app.disks[offset..end]
            .iter()
            .enumerate()
            .map(|(i, disk)| {
                let selected = app.focus == Focus::Disks && offset + i == app.selected_disk;
                compact_disk_line(disk, selected, inner.width as usize)
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
        return;
    }
    if inner.height < 4 {
        let selected = app
            .disks
            .get(app.selected_disk)
            .map(disk_label)
            .unwrap_or_else(|| String::from("no disks"));
        f.render_widget(
            Paragraph::new(truncate(&selected, inner.width as usize)).alignment(Alignment::Center),
            inner,
        );
        return;
    }
    let max_rows = (inner.height / 4).max(1) as usize;
    app.disk_page_rows = max_rows;
    let (offset, end) = disk_window_bounds(app.selected_disk, app.disks.len(), max_rows);
    let visible_disks = &app.disks[offset..end];
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            visible_disks
                .iter()
                .map(|_| Constraint::Length(4))
                .collect::<Vec<_>>(),
        )
        .split(inner);

    for (visible_i, d) in visible_disks.iter().enumerate() {
        if visible_i >= rows.len() {
            break;
        }
        let disk_index = offset + visible_i;
        let selected = app.focus == Focus::Disks && disk_index == app.selected_disk;
        let used = d.total.saturating_sub(d.available);
        let pct = if d.total > 0 {
            (used as f64 / d.total as f64 * 100.0) as u16
        } else {
            0
        };
        let disk_name = disk_label(d);
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
        f.render_widget(gauge, rows[visible_i]);
    }
}

fn draw_reclaim_panel(f: &mut Frame, app: &mut App) {
    let area = centered_rect(70, 70, 72, 22, f.area());
    f.render_widget(Clear, area);

    if app.reclaim_loading {
        app.reclaim_page_rows = area.height.saturating_sub(2).max(1) as usize;
        let inner = Block::default().borders(Borders::ALL).title(" reclaim ");
        let body = Paragraph::new(vec![
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    format!("{} ", spinner_char()),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(
                    "scanning reclaim recommendations…",
                    Style::default().fg(Color::White),
                ),
            ]),
            Line::from(""),
            Line::from("press Enter on a finding to view recoverable paths"),
        ])
        .block(inner)
        .alignment(Alignment::Center);
        f.render_widget(body, area);
        return;
    }

    let report = match app.reclaim_report.as_ref() {
        Some(report) => report,
        None => {
            app.reclaim_page_rows = area.height.saturating_sub(2).max(1) as usize;
            let inner = Block::default().borders(Borders::ALL).title(" reclaim ");
            let body = Paragraph::new("press R to scan reclaim recommendations")
                .block(inner)
                .alignment(Alignment::Center);
            f.render_widget(body, area);
            return;
        }
    };

    let item_count = report.findings.len();
    if item_count == 0 {
        app.reclaim_page_rows = area.height.saturating_sub(2).max(1) as usize;
        let inner = Block::default().borders(Borders::ALL).title(" reclaim ");
        let body = Paragraph::new("no reclaim candidates found")
            .block(inner)
            .alignment(Alignment::Center);
        f.render_widget(body, area);
        return;
    }

    let (list_area, detail_area) = reclaim_panel_layout(area);
    app.reclaim_page_rows = list_area.height.saturating_sub(2).max(1) as usize;

    let items: Vec<ListItem> = report
        .findings
        .iter()
        .map(|finding| {
            let class_style = match finding.class {
                Reclaimability::Safe => Style::default().fg(Color::Green),
                Reclaimability::Regenerable => Style::default().fg(Color::Yellow),
                Reclaimability::Risky => Style::default().fg(Color::Red),
            };
            let mut spans = Vec::new();
            let size = if finding.size.allocated == finding.size.logical {
                format!("{} ", human(finding.size.allocated))
            } else {
                format!(
                    "{} / {} ",
                    human(finding.size.allocated),
                    human(finding.size.logical)
                )
            };
            let size = if finding.inaccessible > 0 {
                format!("≥{size}")
            } else {
                size
            };
            let size = if finding.skipped_mounts > 0 {
                format!("{size}*")
            } else {
                size
            };
            spans.push(Span::styled(
                format!("{size:>16}"),
                Style::default().fg(Color::Green),
            ));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                format!("{:>11}", finding.class.label()),
                class_style,
            ));
            spans.push(Span::raw("  "));
            // Roll-up rows are shown for context; their bytes are already
            // counted inside child findings and excluded from the total.
            let label_text = if finding.rollup {
                let available = list_area.width.saturating_sub(52) as usize;
                let base = truncate(&finding.label, available.saturating_sub(10));
                format!("{base} [subtotal]")
            } else {
                truncate(&finding.label, list_area.width.saturating_sub(52) as usize)
            };
            spans.push(Span::styled(label_text, Style::default().fg(Color::White)));
            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(
                    " reclaim {} · {} ",
                    finding_count_label(item_count),
                    human(report.total.allocated)
                ))
                .title_style(Style::default().fg(Color::DarkGray)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    let mut state = ListState::default();
    state.select(Some(app.selected_reclaim.min(item_count.saturating_sub(1))));
    f.render_stateful_widget(list, list_area, &mut state);

    if let Some(finding) = app.selected_reclaim_finding() {
        let size_line = format!(
            "{} · {} paths",
            if finding.size.allocated == finding.size.logical {
                human(finding.size.allocated)
            } else {
                format!(
                    "{} / {}",
                    human(finding.size.allocated),
                    human(finding.size.logical)
                )
            },
            finding.count
        );
        let mut lines = vec![
            Line::from(""),
            Line::from(Span::styled(size_line, Style::default().fg(Color::Green))),
            Line::from(Span::styled(
                finding.note.clone(),
                Style::default().fg(Color::Gray),
            )),
        ];
        if finding.inaccessible > 0 {
            lines.push(Line::from(Span::styled(
                format!(
                    "{} unreadable directories; size is a lower bound",
                    finding.inaccessible
                ),
                Style::default().fg(Color::Yellow),
            )));
        }
        if finding.skipped_mounts > 0 {
            lines.push(Line::from(Span::styled(
                format!("{} mounted volumes skipped", finding.skipped_mounts),
                Style::default().fg(Color::Yellow),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(
            "enter: open paths  ·  d: delete path  ·  esc: close paths",
        ));
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" reclaim finding ")
            .border_style(Style::default().fg(Color::DarkGray));
        f.render_widget(Paragraph::new(lines).block(block), detail_area);
    }
}

fn reclaim_panel_layout(area: Rect) -> (Rect, Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(7), Constraint::Length(9)])
        .split(area);
    (chunks[0], chunks[1])
}

fn finding_count_label(count: usize) -> String {
    if count == 1 {
        String::from("1 finding")
    } else {
        format!("{count} findings")
    }
}

/// Shared footer hint for the selectable list modals so they read identically.
const MODAL_LIST_HINT: &str = "f/enter: reveal  ·  O: open  ·  d: trash  ·  esc: close";

/// Standard centered, bordered list-modal shell. Clears the area, draws the
/// block, and returns the `(list_area, footer_area)` for the caller to fill, or
/// `None` when there is no room. Shared by the top-files and reclaim-paths
/// overlays so their placement and chrome stay identical (issue #77).
fn modal_list_regions(f: &mut Frame, title: &str) -> Option<(Rect, Rect)> {
    let area = centered_rect(70, 70, 74, 20, f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title.to_string())
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return None;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .split(inner);
    Some((chunks[0], chunks[1]))
}

/// Centered single-message overlay (empty/loading states) sharing the list
/// modals' placement.
fn draw_modal_message(f: &mut Frame, title: &str, lines: Vec<Line<'static>>) {
    let area = centered_rect(70, 70, 74, 20, f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title.to_string());
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .alignment(Alignment::Center),
        area,
    );
}

/// Render a selectable list and its two-line footer into pre-computed regions
/// with the modals' shared highlight style.
fn render_modal_list(
    f: &mut Frame,
    list_area: Rect,
    footer_area: Rect,
    items: Vec<ListItem<'static>>,
    selected_in_window: usize,
    footer: Vec<Line<'static>>,
) {
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    let mut state = ListState::default();
    state.select(Some(selected_in_window));
    f.render_stateful_widget(list, list_area, &mut state);
    f.render_widget(Paragraph::new(footer), footer_area);
}

fn draw_reclaim_paths(f: &mut Frame, app: &mut App) {
    let item_count = app.reclaim_paths_count();
    if item_count == 0 {
        draw_modal_message(
            f,
            " reclaim paths ",
            vec![Line::from("no reclaim paths found")],
        );
        return;
    }
    let Some((list_area, footer_area)) = modal_list_regions(f, " reclaim paths ") else {
        return;
    };
    let max_rows = list_area.height.max(1) as usize;
    let (offset, end) = app.reclaim_paths_window_bounds(max_rows);
    let selected = app.reclaim_paths_selected();
    let Some(finding) = app.selected_reclaim_finding() else {
        return;
    };
    let items: Vec<ListItem> = finding
        .paths
        .iter()
        .enumerate()
        .skip(offset)
        .take(end.saturating_sub(offset))
        .map(|(idx, path)| {
            let mut spans = vec![
                Span::styled(
                    format!("{:>2}. ", idx + 1),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    truncate(
                        &path.display().to_string(),
                        list_area.width.saturating_sub(8) as usize,
                    ),
                    Style::default().fg(Color::White),
                ),
            ];
            if idx == selected {
                spans.push(Span::styled(
                    "  (current)",
                    Style::default().fg(Color::DarkGray),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let footer = vec![
        Line::from(vec![
            Span::styled(
                format!(
                    "{} ({}) · {}",
                    finding.label,
                    finding.class.label(),
                    finding.count
                ),
                Style::default().fg(Color::Gray),
            ),
            Span::styled(
                format!("  ·  {}-{} of {}", offset + 1, end, item_count),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(MODAL_LIST_HINT),
    ];
    render_modal_list(
        f,
        list_area,
        footer_area,
        items,
        selected.saturating_sub(offset),
        footer,
    );
}

fn draw_top_files(f: &mut Frame, app: &mut App) {
    if app.top_files_loading() {
        let mut lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    format!("{} ", spinner_char()),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled("scanning top files…", Style::default().fg(Color::White)),
            ]),
        ];
        if let Some(path) = app.top_files_path() {
            lines.push(Line::from(format!("path: {}", path.display())));
        }
        draw_modal_message(f, " top files ", lines);
        return;
    }

    let file_count = app.top_files_count();
    if file_count == 0 {
        let message = if app.top_files_scan().is_some() {
            "no regular files found"
        } else {
            "no scan in progress"
        };
        draw_modal_message(f, " top files ", vec![Line::from(message)]);
        return;
    }

    let title = app
        .top_files_path()
        .map(|path| format!(" top files · {} ", path.display()))
        .unwrap_or_else(|| String::from(" top files "));
    let Some((list_area, footer_area)) = modal_list_regions(f, &title) else {
        return;
    };
    let max_rows = list_area.height.max(1) as usize;
    let (offset, end) = app.top_files_window_bounds(max_rows);
    let selected = app.top_files_selected();
    let Some(scan) = app.top_files_scan() else {
        return;
    };
    let items: Vec<ListItem> = scan
        .largest_files
        .iter()
        .enumerate()
        .skip(offset)
        .take(end.saturating_sub(offset))
        .map(|(idx, file)| {
            let size = if file.size.allocated == file.size.logical {
                human(file.size.allocated)
            } else {
                format!(
                    "{} / {}",
                    human(file.size.allocated),
                    human(file.size.logical)
                )
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:>2}. ", idx + 1),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    truncate(
                        &file.path.display().to_string(),
                        list_area.width.saturating_sub(34) as usize,
                    ),
                    Style::default().fg(Color::White),
                ),
                Span::raw("  "),
                Span::styled(format!("{size:>20}"), Style::default().fg(Color::Green)),
            ]))
        })
        .collect();
    let total = if scan.size.allocated == scan.size.logical {
        human(scan.size.allocated)
    } else {
        format!(
            "{} / {}",
            human(scan.size.allocated),
            human(scan.size.logical)
        )
    };
    let footer = vec![
        Line::from(vec![
            Span::styled(format!("total: {total}"), Style::default().fg(Color::Green)),
            Span::styled(
                format!(" · {}-{} of {}", offset + 1, end, file_count),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(MODAL_LIST_HINT),
    ];
    render_modal_list(
        f,
        list_area,
        footer_area,
        items,
        selected.saturating_sub(offset),
        footer,
    );
}

fn draw_disk_info_modal(f: &mut Frame, app: &App) {
    let area = centered_rect(65, 62, 60, 18, f.area());
    f.render_widget(Clear, area);

    if app.disk_info_loading() {
        let block = Block::default().borders(Borders::ALL).title(" disk info ");
        let body = Paragraph::new(vec![
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    format!("{} ", spinner_char()),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled("loading disk details…", Style::default().fg(Color::White)),
            ]),
        ])
        .block(block)
        .alignment(Alignment::Center);
        f.render_widget(body, area);
        return;
    }

    let Some(report) = app.disk_info_report.as_ref() else {
        let block = Block::default().borders(Borders::ALL).title(" disk info ");
        let body = Paragraph::new("no disk details available")
            .block(block)
            .alignment(Alignment::Center);
        f.render_widget(body, area);
        return;
    };

    let used = report.used;
    let gap = report.unavailable_free();
    let mut lines = Vec::new();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("mount ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!(
                "{}  ({}, {})",
                report.mount.display(),
                report.fs_type,
                report.device
            ),
            Style::default().fg(Color::White),
        ),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("total ", Style::default().fg(Color::DarkGray)),
        Span::styled(human(report.total), Style::default().fg(Color::Green)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("used ", Style::default().fg(Color::DarkGray)),
        Span::styled(human(used), Style::default().fg(Color::Green)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("free ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!(
                "{}  (available: {})",
                human(report.free),
                human(report.available)
            ),
            Style::default().fg(Color::Green),
        ),
    ]));
    if gap > 0 {
        lines.push(Line::from(vec![
            Span::styled("free-not-available ", Style::default().fg(Color::DarkGray)),
            Span::styled(human(gap), Style::default().fg(Color::Yellow)),
        ]));
    }
    lines.push(Line::from(""));
    if let Some(container) = &report.apfs_container {
        lines.push(Line::from(vec![
            Span::styled("apfs container ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!(
                    "{} free of {}",
                    human(container.free),
                    human(container.size)
                ),
                Style::default().fg(Color::Green),
            ),
        ]));
    }
    match report.local_snapshots.error.as_deref() {
        Some(err) => {
            lines.push(Line::from(vec![
                Span::styled("snapshots ", Style::default().fg(Color::DarkGray)),
                Span::styled(err, Style::default().fg(Color::Red)),
            ]));
        }
        _ => {
            let names = &report.local_snapshots.names;
            if names.is_empty() {
                lines.push(Line::from(Span::styled(
                    "snapshots: none",
                    Style::default().fg(Color::DarkGray),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    format!("snapshots: {}", names.len()),
                    Style::default().fg(Color::DarkGray),
                )));
                for name in names.iter().take(6) {
                    lines.push(Line::from(Span::styled(
                        truncate(name, area.width.saturating_sub(6) as usize),
                        Style::default().fg(Color::Gray),
                    )));
                }
            }
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from("esc closes this panel"));

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" disk details ");
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_file_info(f: &mut Frame, app: &App) {
    let Some(info) = app.file_info.as_ref() else {
        return;
    };

    let area = centered_rect(78, 70, 64, 20, f.area());
    f.render_widget(Clear, area);

    let mut lines = Vec::new();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(
            truncate(&info.name, area.width.saturating_sub(6) as usize),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" · {}", info.kind),
            Style::default().fg(Color::Gray),
        ),
    ]));
    lines.push(info_path_line("  Path: ", &info.path, area, 10));

    if let Some(size) = info.size {
        lines.push(Line::from(vec![
            Span::styled(
                if info.kind == "directory" {
                    "  Recursive size: "
                } else {
                    "  Size: "
                },
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                size_detail(size),
                if info.size_stale {
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default().fg(Color::Green)
                },
            ),
        ]));
        if size.logical != size.allocated {
            lines.push(Line::from(Span::styled(
                if size.allocated < size.logical {
                    "  Apparent and allocated size differ; APFS clones, sparse files, or compression can make allocated smaller."
                } else {
                    "  Apparent and allocated size differ; filesystem block allocation can round allocated size up."
                },
                Style::default().fg(Color::Gray),
            )));
        }
    } else if info.kind == "directory" {
        lines.push(Line::from(vec![
            Span::styled("  Recursive size: ", Style::default().fg(Color::DarkGray)),
            Span::styled("not scanned yet", Style::default().fg(Color::Gray)),
        ]));
    }

    if let Some(count) = info.direct_items {
        lines.push(Line::from(vec![
            Span::styled("  Items: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{count} direct children"),
                Style::default().fg(Color::White),
            ),
        ]));
    }
    if info.inaccessible > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "  {} unreadable directories were skipped; recursive size is a lower bound.",
                info.inaccessible
            ),
            Style::default().fg(Color::Yellow),
        )));
    }

    lines.push(Line::from(""));
    lines.push(file_info_meta_line("  Created: ", info.created));
    lines.push(file_info_meta_line("  Modified: ", info.modified));
    lines.push(file_info_meta_line("  Accessed: ", info.accessed));
    lines.push(Line::from(vec![
        Span::styled("  Owner: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{}:{}", info.owner, info.group),
            Style::default().fg(Color::White),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  Permissions: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{} ({})", info.permissions_octal, info.permissions_symbolic),
            Style::default().fg(Color::White),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  Hard links: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            info.hard_links.to_string(),
            Style::default().fg(Color::White),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  Extended attrs: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            match info.xattr_count {
                Some(count) if info.has_quarantine_xattr => {
                    format!("{count} (includes com.apple.quarantine)")
                }
                Some(count) => count.to_string(),
                None => String::from("unavailable"),
            },
            if info.has_quarantine_xattr {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::White)
            },
        ),
    ]));

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(
            "  Space",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Quick Look", Style::default().fg(Color::Gray)),
        Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "f",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Finder", Style::default().fg(Color::Gray)),
        Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "O",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" open", Style::default().fg(Color::Gray)),
        Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "d",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" trash", Style::default().fg(Color::Gray)),
        Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "Esc",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" close", Style::default().fg(Color::Gray)),
    ]));

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" file info ")
        .border_style(Style::default().fg(Color::Cyan));
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn info_path_line(
    label: &'static str,
    path: &std::path::Path,
    area: Rect,
    reserved: u16,
) -> Line<'static> {
    let max_path_len = area.width.saturating_sub(reserved) as usize;
    Line::from(vec![
        Span::styled(label, Style::default().fg(Color::DarkGray)),
        Span::styled(
            truncate_start(&path.display().to_string(), max_path_len),
            Style::default().fg(Color::Gray),
        ),
    ])
}

fn file_info_meta_line(label: &'static str, value: Option<SystemTime>) -> Line<'static> {
    Line::from(vec![
        Span::styled(label, Style::default().fg(Color::DarkGray)),
        Span::styled(
            format_full_timestamp(value),
            Style::default().fg(Color::White),
        ),
    ])
}

fn draw_packages(f: &mut Frame, area: Rect, app: &mut App) {
    app.package_page_rows = area.height.saturating_sub(2).max(1) as usize;
    if area.height == 0 || area.width == 0 {
        return;
    }
    let border_color = if app.focus == Focus::Packages {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    let (sys_style, proj_style) = if app.pkg_view == PkgView::SystemManagers {
        (
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Yellow),
            Style::default().fg(Color::DarkGray),
        )
    } else {
        (
            Style::default().fg(Color::DarkGray),
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Yellow),
        )
    };

    let total_str = if app.packages_loaded {
        match app.pkg_view {
            PkgView::SystemManagers => {
                let total = app.total_pkg_size();
                if total > 0 {
                    format!(" · {}", human(total))
                } else {
                    String::new()
                }
            }
            PkgView::ProjectDeps => {
                let total = app.total_project_deps_size();
                if total > 0 {
                    format!(" · {}", human(total))
                } else {
                    String::new()
                }
            }
        }
    } else {
        String::new()
    };

    let title = Line::from(vec![
        Span::raw(" packages ─"),
        Span::styled(
            if app.pkg_view == PkgView::SystemManagers {
                "[ System ]"
            } else {
                " System "
            },
            sys_style,
        ),
        Span::raw("─"),
        Span::styled(
            if app.pkg_view == PkgView::ProjectDeps {
                "[ Projects ]"
            } else {
                " Projects "
            },
            proj_style,
        ),
        Span::styled(total_str, Style::default().fg(Color::Green)),
        if app.pkg_show_unused {
            Span::styled(" [dependency leaves]", Style::default().fg(Color::Magenta))
        } else {
            Span::raw("")
        },
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(border_color));

    if !app.packages_loaded {
        let block_inner = block.inner(area);
        f.render_widget(block, area);

        if app.packages_loading {
            if block_inner.height >= 6 {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Min(1),
                        Constraint::Length(1), // Title
                        Constraint::Length(1), // Activity bar
                        Constraint::Length(1), // Subtitle
                        Constraint::Min(1),
                    ])
                    .split(block_inner);

                let title = Line::from(vec![
                    Span::styled(
                        format!("{} ", big_spinner_char()),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled("scanning package data…", Style::default().fg(Color::White)),
                ]);
                f.render_widget(
                    Paragraph::new(title).alignment(Alignment::Center),
                    chunks[1],
                );

                let act_bar = activity_bar(block_inner.width as usize);
                let bar_line =
                    Line::from(Span::styled(act_bar, Style::default().fg(Color::DarkGray)));
                f.render_widget(
                    Paragraph::new(bar_line).alignment(Alignment::Center),
                    chunks[2],
                );

                let sub = Line::from(Span::styled(
                    "checking package managers and project dependencies",
                    Style::default().fg(Color::DarkGray),
                ));
                f.render_widget(Paragraph::new(sub).alignment(Alignment::Center), chunks[3]);
            } else {
                let message = Line::from(vec![
                    Span::styled(
                        format!("{} ", spinner_char()),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled("scanning…", Style::default().fg(Color::White)),
                ]);
                let text = Paragraph::new(message)
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: true });
                f.render_widget(text, block_inner);
            }
        } else {
            let message = "press p to scan packages";
            let text = Paragraph::new(message)
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true });
            f.render_widget(text, block_inner);
        }
        return;
    }

    let visible_indices = app.pkg_visible_indices();
    let item_count = visible_indices.len();
    let inner_width = area.width.saturating_sub(2);

    if item_count == 0 {
        let message = if app.pkg_filter_active() {
            "no matching packages"
        } else {
            match app.pkg_view {
                PkgView::SystemManagers => "no supported packages found",
                PkgView::ProjectDeps => "no project manifests found",
            }
        };
        let empty = Paragraph::new(message)
            .block(block)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true });
        f.render_widget(empty, area);
        return;
    }

    let items: Vec<ListItem> = visible_indices
        .iter()
        .copied()
        .filter_map(|real_i| match app.pkg_view {
            PkgView::SystemManagers => {
                let (package, manager) = app.flat_packages().get(real_i)?;
                let size = package
                    .size
                    .map(|s| human(s.allocated))
                    .unwrap_or_else(|| String::from("?"));
                let use_status = app
                    .dep_graph
                    .as_ref()
                    .map(|g| g.use_status(*manager, &package.name))
                    .unwrap_or(PackageUseStatus::Untracked);
                Some(package_line_with_version(
                    manager.label(),
                    &package.name,
                    &package.version,
                    &size,
                    use_status,
                    inner_width,
                ))
            }
            PkgView::ProjectDeps => {
                let dep = app.project_deps.get(real_i)?;
                let size = dep
                    .deps_size
                    .map(|s| human(s.allocated))
                    .unwrap_or_else(|| String::from("—"));
                let label = format!(
                    "{} · {} deps · {}",
                    dep.manager_label,
                    dep.dep_count,
                    dep.path.display()
                );
                let (name_width, size_width) = package_columns(inner_width);
                Some(package_line(&label, &size, name_width, size_width))
            }
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    let mut state = ListState::default();
    state.select(Some(app.selected_pkg.min(item_count.saturating_sub(1))));
    f.render_stateful_widget(list, area, &mut state);
}

fn package_line_with_version(
    manager: &str,
    name: &str,
    version: &str,
    size: &str,
    use_status: PackageUseStatus,
    inner_width: u16,
) -> ListItem<'static> {
    let (name_width, size_width) = package_columns(inner_width);
    let ver_display = if version.is_empty() {
        String::new()
    } else {
        format!("@{version}")
    };
    let dep_indicator = match use_status {
        PackageUseStatus::RequiredByDependents => "* ",
        PackageUseStatus::DependencyLeaf => "  ",
        PackageUseStatus::Untracked => "? ",
    };
    let mgr_label = format!("{manager} ");
    let name_budget = name_width
        .saturating_sub(mgr_label.len())
        .saturating_sub(dep_indicator.len());
    let name_ver = format!("{name}{ver_display}");
    let name_truncated = truncate(&name_ver, name_budget);
    let padded_name = format!("{name_truncated:<width$}", width = name_budget);

    let dep_style = match use_status {
        PackageUseStatus::Untracked => Style::default().fg(Color::Yellow),
        _ => Style::default().fg(Color::DarkGray),
    };
    let mut spans = vec![
        Span::styled(dep_indicator, dep_style),
        Span::styled(mgr_label, Style::default().fg(Color::DarkGray)),
        Span::styled(padded_name, Style::default().fg(Color::White)),
    ];
    if size_width > 0 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("{size:>size_width$}"),
            Style::default().fg(Color::Green),
        ));
    }
    ListItem::new(Line::from(spans))
}

fn package_line(
    label: &str,
    size: &str,
    name_width: usize,
    size_width: usize,
) -> ListItem<'static> {
    let mut spans = vec![Span::styled(
        format!(
            "{:<width$}",
            truncate(label, name_width),
            width = name_width
        ),
        Style::default().fg(Color::White),
    )];
    if size_width > 0 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("{size:>size_width$}"),
            Style::default().fg(Color::Green),
        ));
    }
    ListItem::new(Line::from(spans))
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    if app.search_mode {
        let query = if app.search_query.is_empty() {
            String::from("/")
        } else {
            format!("/{}", app.search_query)
        };
        let count = app.visible_entry_count();
        let text = Line::from(vec![
            Span::styled("search ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                query,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" · {count} matches · Enter keep · Esc clear"),
                Style::default().fg(Color::Gray),
            ),
        ]);
        f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), area);
        return;
    }
    if app.pkg_search_mode {
        let query = if app.pkg_search_query.is_empty() {
            String::from("/")
        } else {
            format!("/{}", app.pkg_search_query)
        };
        let count = app.pkg_item_count();
        let text = Line::from(vec![
            Span::styled("filter packages ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                query,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" · {count} matches · Enter keep · Esc clear"),
                Style::default().fg(Color::Gray),
            ),
        ]);
        f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), area);
        return;
    }
    if app.focus == Focus::Files && app.search_filter_active() {
        let count = app.visible_entry_count();
        let text = Line::from(vec![
            Span::styled("filter ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("/{}", app.search_query),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" · {count} matches · / edit · Esc clear"),
                Style::default().fg(Color::Gray),
            ),
        ]);
        f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), area);
        return;
    }
    if app.focus == Focus::Packages && app.pkg_filter_active() {
        let count = app.pkg_item_count();
        let text = Line::from(vec![
            Span::styled("package filter ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("/{}", app.pkg_search_query),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" · {count} matches · / edit · Esc clear"),
                Style::default().fg(Color::Gray),
            ),
        ]);
        f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), area);
        return;
    }
    let mut spans = selection_status(app);
    if !app.status.is_empty() {
        spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(&app.status, Style::default().fg(Color::Gray)));
    }
    let text = Line::from(spans);
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), area);
}

fn draw_help(f: &mut Frame, area: Rect, focus: Focus) {
    // Destructive keys (trash, empty trash, uninstall) get a red key cap so they
    // read differently from navigation.
    let key = |k: &'static str| {
        let color = if keymap::is_destructive_key(k) {
            Color::Red
        } else {
            Color::Yellow
        };
        Span::styled(k, Style::default().fg(color).add_modifier(Modifier::BOLD))
    };
    let label = |s: &'static str| Span::styled(s, Style::default().fg(Color::Gray));
    let sep = || Span::styled(" · ", Style::default().fg(Color::DarkGray));

    let total = area.width as usize;
    // Reserve room for the trailing "? help" so it always survives truncation.
    let help_width = " · ? help".chars().count();
    let budget = total.saturating_sub(help_width);

    let mut spans = Vec::new();
    let mut used = 0usize;
    for binding in keymap::footer_bindings_for(focus) {
        let piece = binding.key.chars().count() + 1 + binding.action.chars().count();
        let sep_width = if spans.is_empty() { 0 } else { 3 };
        if used + sep_width + piece > budget {
            break;
        }
        if !spans.is_empty() {
            spans.push(sep());
        }
        spans.push(key(binding.key));
        spans.push(label(" "));
        spans.push(label(binding.action));
        used += sep_width + piece;
    }
    if !spans.is_empty() {
        spans.push(sep());
    }
    spans.push(key("?"));
    spans.push(label(" "));
    spans.push(label("help"));

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_help_overlay(f: &mut Frame) {
    let area = centered_rect(84, 80, 64, 22, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" keyboard help ")
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Reserve the bottom row for the size-marker legend, which is otherwise
    // undiscoverable.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let sections_area = chunks[0];

    if sections_area.width >= 76 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(sections_area);
        let split = keymap::HELP_SECTIONS.len().div_ceil(2);
        draw_help_column(f, columns[0], &keymap::HELP_SECTIONS[..split]);
        draw_help_column(f, columns[1], &keymap::HELP_SECTIONS[split..]);
    } else {
        draw_help_column(f, sections_area, keymap::HELP_SECTIONS);
    }

    let marker = |m: &'static str| Span::styled(m, Style::default().fg(Color::Yellow));
    let note = |s: &'static str| Span::styled(s, Style::default().fg(Color::Gray));
    let legend = Line::from(vec![
        Span::styled("markers  ", Style::default().fg(Color::DarkGray)),
        marker("~"),
        note(" cached  "),
        marker("≥"),
        note(" lower bound  "),
        marker("*"),
        note(" mounts skipped  "),
        Span::styled("↑↓", Style::default().fg(Color::Green)),
        note(" grew/shrank"),
    ]);
    f.render_widget(Paragraph::new(legend), chunks[1]);
}

fn draw_help_column(f: &mut Frame, area: Rect, sections: &[KeySection]) {
    let key_width = sections
        .iter()
        .flat_map(|section| section.bindings.iter())
        .map(|binding| binding.key.chars().count())
        .max()
        .unwrap_or(1)
        .min(16);
    let mut lines = Vec::new();

    for (section_idx, section) in sections.iter().enumerate() {
        if section_idx > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            section.title,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        for binding in section.bindings {
            let key_color = if keymap::is_destructive_key(binding.key) {
                Color::Red
            } else {
                Color::Yellow
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    pad_key(binding.key, key_width),
                    Style::default().fg(key_color).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(binding.action, Style::default().fg(Color::Gray)),
            ]));
        }
    }

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn draw_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(60, 20, 40, 7, f.area());
    f.render_widget(Clear, area);
    let name = app.pending_delete_name();
    let mut body = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("Move to Trash: {name}"),
            Style::default().add_modifier(Modifier::BOLD),
        )),
    ];
    if let Some(summary) = app.pending_batch_summary() {
        body.push(Line::from(Span::styled(
            summary,
            Style::default().fg(Color::Gray),
        )));
    }
    body.push(Line::from(""));
    body.push(Line::from("press  y  to confirm   ·   n  to cancel"));
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

fn draw_empty_trash_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(60, 20, 48, 8, f.area());
    f.render_widget(Clear, area);
    // request_empty_trash only arms when the report lists Trash, so the
    // finding is normally present; the fallback covers a report swapped out
    // by a reclaim re-scan while the confirmation is open.
    let (path_note, size_note) = match app.reclaim_trash_finding() {
        Some(finding) => (
            finding
                .paths
                .first()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            format!(" (~{})", crate::app::human(finding.size.allocated)),
        ),
        None => (String::new(), String::new()),
    };
    let body = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("Empty Trash{size_note}?"),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(path_note, Style::default().fg(Color::Gray))),
        Line::from(Span::styled(
            "already-discarded items are removed permanently",
            Style::default().fg(Color::Gray),
        )),
        Line::from(""),
        Line::from("press  y  to confirm   ·   n  to cancel"),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .title("empty trash")
        .border_style(Style::default().fg(Color::Red));
    f.render_widget(
        Paragraph::new(body)
            .block(block)
            .alignment(Alignment::Center),
        area,
    );
}

fn draw_uninstall_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(60, 20, 44, 6, f.area());
    f.render_widget(Clear, area);
    let name = app.pending_uninstall_name();
    let body = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("Uninstall: {name}"),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("press  y  to confirm   ·   n  to cancel"),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .title("uninstall")
        .border_style(Style::default().fg(Color::Red));
    f.render_widget(
        Paragraph::new(body)
            .block(block)
            .alignment(Alignment::Center),
        area,
    );
}

fn draw_pkg_detail(f: &mut Frame, app: &App) {
    let Some((pkg, manager, dep_info)) = app.selected_pkg_detail() else {
        return;
    };

    let area = centered_rect(75, 60, 50, 14, f.area());
    f.render_widget(Clear, area);

    let mut lines = Vec::new();
    lines.push(Line::from(""));

    lines.push(Line::from(vec![
        Span::styled(
            format!("{} ", manager.label()),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            &pkg.name,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        if pkg.version.is_empty() {
            Span::raw("")
        } else {
            Span::styled(
                format!("@{}", pkg.version),
                Style::default().fg(Color::Gray),
            )
        },
    ]));

    if let Some(size) = pkg.size {
        lines.push(Line::from(vec![
            Span::styled("  Size: ", Style::default().fg(Color::DarkGray)),
            Span::styled(size_detail(size), Style::default().fg(Color::Green)),
        ]));
    }
    if let Some(path) = &pkg.path {
        let max_path_len = area.width.saturating_sub(10) as usize;
        lines.push(Line::from(vec![
            Span::styled("  Path: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                truncate(&path.display().to_string(), max_path_len),
                Style::default().fg(Color::Gray),
            ),
        ]));
    }
    if let Some(path) = &pkg.metadata_path {
        let max_path_len = area.width.saturating_sub(14) as usize;
        lines.push(Line::from(vec![
            Span::styled("  Metadata: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                truncate(&path.display().to_string(), max_path_len),
                Style::default().fg(Color::Gray),
            ),
        ]));
    }

    lines.push(Line::from(""));

    match dep_info {
        Some(info) if info.evidence == DepEvidence::ManagerGraph => {
            if manager.is_global_leaf_manager()
                && info.dependencies.is_empty()
                && info.dependents.is_empty()
            {
                lines.push(Line::from(Span::styled(
                    "  Usage: global install - no package-manager dependents",
                    Style::default().fg(Color::Green),
                )));
                lines.push(Line::from(Span::styled(
                    "  Treat as a leaf unless you use it manually or from scripts.",
                    Style::default().fg(Color::Gray),
                )));
            } else {
                let dep_text = if info.dependencies.is_empty() {
                    String::from("none")
                } else {
                    let max_len = area.width.saturating_sub(18) as usize;
                    let joined = info.dependencies.join(", ");
                    truncate(&joined, max_len)
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  Dependencies ({}): ", info.dependencies.len()),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(dep_text, Style::default().fg(Color::White)),
                ]));

                let rev_text = if info.dependents.is_empty() {
                    String::from("none in manager graph")
                } else {
                    let max_len = area.width.saturating_sub(14) as usize;
                    let joined = info.dependents.join(", ");
                    truncate(&joined, max_len)
                };
                let rev_style = if info.dependents.is_empty() {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::Yellow)
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  Used by ({}): ", info.dependents.len()),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(rev_text, rev_style),
                ]));
            }
        }
        Some(_) => {
            lines.push(Line::from(Span::styled(
                "  Usage: not dependency-tracked by this package manager",
                Style::default().fg(Color::Yellow),
            )));
            lines.push(Line::from(Span::styled(
                "  This can still be a runtime, app, environment, CLI, or manually used tool.",
                Style::default().fg(Color::Gray),
            )));
        }
        None => {
            if app.deps_loading {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {} ", spinner_char()),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(
                        "scanning dependency tree…",
                        Style::default().fg(Color::Gray),
                    ),
                ]));
            } else {
                lines.push(Line::from(Span::styled(
                    "  dependency info not available",
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(
            "  x",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" uninstall", Style::default().fg(Color::Gray)),
        Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "d",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" trash dir", Style::default().fg(Color::Gray)),
        Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "Esc",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" close", Style::default().fg(Color::Gray)),
    ]));

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" package info ")
        .border_style(Style::default().fg(Color::Cyan));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_project_dep_detail(f: &mut Frame, app: &App) {
    let Some(dep) = app.selected_project_dep_detail() else {
        return;
    };

    let area = centered_rect(75, 60, 50, 12, f.area());
    f.render_widget(Clear, area);

    let mut lines = Vec::new();
    lines.push(Line::from(""));

    lines.push(Line::from(vec![
        Span::styled(
            format!("{} ", dep.manager_label),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            dep.path.display().to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ]));

    lines.push(Line::from(vec![
        Span::styled("  Manifest: ", Style::default().fg(Color::DarkGray)),
        Span::styled(dep.manifest.as_str(), Style::default().fg(Color::Gray)),
    ]));

    lines.push(Line::from(vec![
        Span::styled("  Dependencies: ", Style::default().fg(Color::DarkGray)),
        Span::styled(dep.dep_count.to_string(), Style::default().fg(Color::White)),
    ]));

    if let Some(size) = dep.deps_size {
        lines.push(Line::from(vec![
            Span::styled("  Deps size: ", Style::default().fg(Color::DarkGray)),
            Span::styled(size_detail(size), Style::default().fg(Color::Green)),
        ]));
    }

    if let Some(deps_dir) = &dep.deps_dir {
        let max_path_len = area.width.saturating_sub(14) as usize;
        lines.push(Line::from(vec![
            Span::styled("  Deps dir: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                truncate(&deps_dir.display().to_string(), max_path_len),
                Style::default().fg(Color::Gray),
            ),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(
            "  d",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" trash deps dir", Style::default().fg(Color::Gray)),
        Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "Esc",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" close", Style::default().fg(Color::Gray)),
    ]));

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" project deps ")
        .border_style(Style::default().fg(Color::Cyan));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn centered_rect(px: u16, py: u16, min_width: u16, min_height: u16, area: Rect) -> Rect {
    let width = area
        .width
        .saturating_mul(px)
        .saturating_div(100)
        .max(min_width)
        .min(area.width);
    let height = area
        .height
        .saturating_mul(py)
        .saturating_div(100)
        .max(min_height)
        .min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if display_width(s) <= max {
        s.to_string()
    } else {
        let mut out = String::new();
        let mut width = 0;
        let limit = max.saturating_sub(1);
        for ch in s.chars() {
            let ch_width = char_display_width(ch);
            if width + ch_width > limit {
                break;
            }
            out.push(ch);
            width += ch_width;
        }
        out.push('…');
        out
    }
}

fn truncate_start(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if display_width(s) <= max {
        return s.to_string();
    }
    if max == 1 {
        return String::from("…");
    }
    let mut tail = Vec::new();
    let mut width = 0;
    let limit = max.saturating_sub(1);
    for ch in s.chars().rev() {
        let ch_width = char_display_width(ch);
        if width + ch_width > limit {
            break;
        }
        tail.push(ch);
        width += ch_width;
    }
    let tail: String = tail.into_iter().rev().collect();
    format!("…{tail}")
}

fn pad_truncated(s: &str, width: usize) -> String {
    let mut out = truncate(s, width);
    let padding = width.saturating_sub(display_width(&out));
    out.extend(std::iter::repeat_n(' ', padding));
    out
}

fn display_width(s: &str) -> usize {
    s.chars().map(char_display_width).sum()
}

fn char_display_width(ch: char) -> usize {
    if ch.is_control()
        || matches!(
            ch,
            '\u{0300}'..='\u{036F}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
        )
    {
        return 0;
    }

    if matches!(
        ch,
        '\u{1100}'..='\u{115F}'
            | '\u{2329}'..='\u{232A}'
            | '\u{2E80}'..='\u{A4CF}'
            | '\u{AC00}'..='\u{D7A3}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{FE10}'..='\u{FE19}'
            | '\u{FE30}'..='\u{FE6F}'
            | '\u{FF00}'..='\u{FF60}'
            | '\u{FFE0}'..='\u{FFE6}'
            | '\u{1F300}'..='\u{1FAFF}'
    ) {
        2
    } else {
        1
    }
}

fn pad_key(s: &str, width: usize) -> String {
    let mut out = s.to_string();
    let len = s.chars().count();
    for _ in len..width {
        out.push(' ');
    }
    out
}

fn selection_status(app: &App) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    match app.focus {
        Focus::Files => match app.visible_entry(app.selected) {
            Some(entry) if entry.is_dir && entry.scanning => {
                spans.push(Span::styled(
                    format!("{} ", spinner_char()),
                    Style::default().fg(Color::Cyan),
                ));
                spans.push(Span::styled("dir ", Style::default().fg(Color::DarkGray)));
                spans.push(Span::styled(
                    truncate(&entry.name, 28),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(
                    " · scanning size",
                    Style::default().fg(Color::Gray),
                ));
                spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
                spans.push(Span::styled(
                    format_modified_time(entry.modified),
                    Style::default().fg(Color::Gray),
                ));
            }
            Some(entry) if entry.is_dir => {
                spans.push(Span::styled("dir ", Style::default().fg(Color::DarkGray)));
                spans.push(Span::styled(
                    truncate(&entry.name, 28),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
                let size = entry
                    .size
                    .map(|size| size_detail_with_cache_marker(size, entry.size_stale))
                    .unwrap_or_else(|| String::from("—"));
                let size_style = if entry.size_stale {
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default().fg(Color::Green)
                };
                spans.push(Span::styled(size, size_style));
                if let Some(status) = stale_cache_status(entry) {
                    spans.push(Span::styled(
                        format!(" · {status}"),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                if entry.inaccessible > 0 {
                    spans.push(Span::styled(
                        format!(
                            " · {} unreadable dirs; size is a lower bound",
                            entry.inaccessible
                        ),
                        Style::default().fg(Color::Yellow),
                    ));
                }
                if entry.skipped_mounts > 0 {
                    spans.push(Span::styled(
                        format!(" · {} mounted volumes skipped", entry.skipped_mounts),
                        Style::default().fg(Color::Yellow),
                    ));
                }
                spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
                spans.push(Span::styled(
                    format_modified_time(entry.modified),
                    Style::default().fg(Color::Gray),
                ));
            }
            Some(entry) if entry.is_symlink => {
                spans.push(Span::styled("link ", Style::default().fg(Color::DarkGray)));
                spans.push(Span::styled(
                    truncate(&entry.name, 28),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
                let size = entry
                    .size
                    .map(size_detail)
                    .unwrap_or_else(|| String::from("symlink"));
                spans.push(Span::styled(size, Style::default().fg(Color::Green)));
                spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
                spans.push(Span::styled(
                    format_modified_time(entry.modified),
                    Style::default().fg(Color::Gray),
                ));
            }
            Some(entry) => {
                spans.push(Span::styled("file ", Style::default().fg(Color::DarkGray)));
                spans.push(Span::styled(
                    truncate(&entry.name, 28),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
                let size = entry
                    .size
                    .map(size_detail)
                    .unwrap_or_else(|| String::from("?"));
                spans.push(Span::styled(size, Style::default().fg(Color::Green)));
                spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
                spans.push(Span::styled(
                    format_modified_time(entry.modified),
                    Style::default().fg(Color::Gray),
                ));
            }
            None => {
                spans.push(Span::styled(
                    "no files in view",
                    Style::default().fg(Color::Gray),
                ));
            }
        },
        Focus::Disks => match app.disks.get(app.selected_disk) {
            Some(disk) => {
                let free = human(disk.available);
                let total = human(disk.total);
                let label = disk_label(disk);
                spans.push(Span::styled("disk ", Style::default().fg(Color::DarkGray)));
                spans.push(Span::styled(
                    truncate(&label, 28),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(
                    format!(" · {free} free of {total}"),
                    Style::default().fg(Color::Gray),
                ));
            }
            None => {
                spans.push(Span::styled(
                    "no disks available",
                    Style::default().fg(Color::Gray),
                ));
            }
        },
        Focus::Reclaim => {
            if let Some((name, path)) = app.selected_reclaim_path() {
                spans.push(Span::styled(
                    "reclaim ",
                    Style::default().fg(Color::DarkGray),
                ));
                spans.push(Span::styled(
                    truncate(&name, 28),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
                spans.push(Span::styled(
                    truncate(path.to_string_lossy().as_ref(), 34),
                    Style::default().fg(Color::Gray),
                ));
            } else {
                spans.push(Span::styled(
                    "no reclaim path selected",
                    Style::default().fg(Color::Gray),
                ));
            }
        }
        Focus::Packages => {
            if app.packages_loading {
                spans.push(Span::styled(
                    format!("{} ", spinner_char()),
                    Style::default().fg(Color::Cyan),
                ));
                spans.push(Span::styled(
                    "scanning packages",
                    Style::default().fg(Color::White),
                ));
                return spans;
            }
            if !app.packages_loaded {
                spans.push(Span::styled(
                    "packages not scanned",
                    Style::default().fg(Color::Gray),
                ));
                return spans;
            }
            match app.pkg_view {
                PkgView::SystemManagers => {
                    let packages = app.flat_packages();
                    let real_idx = app
                        .pkg_visible_index(app.selected_pkg)
                        .unwrap_or(usize::MAX);
                    match packages.get(real_idx) {
                        Some((package, manager)) => {
                            let size = package
                                .size
                                .map(size_detail)
                                .unwrap_or_else(|| String::from("?"));
                            spans.push(Span::styled(
                                format!("{} package ", manager.label()),
                                Style::default().fg(Color::DarkGray),
                            ));
                            spans.push(Span::styled(
                                truncate(&package.name, 28),
                                Style::default()
                                    .fg(Color::White)
                                    .add_modifier(Modifier::BOLD),
                            ));
                            spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
                            spans.push(Span::styled(size, Style::default().fg(Color::Green)));
                        }
                        None => {
                            spans.push(Span::styled(
                                "no packages in view",
                                Style::default().fg(Color::Gray),
                            ));
                        }
                    }
                }
                PkgView::ProjectDeps => match app
                    .pkg_visible_index(app.selected_pkg)
                    .and_then(|i| app.project_deps.get(i))
                {
                    Some(dep) => {
                        let size = dep
                            .deps_size
                            .map(size_detail)
                            .unwrap_or_else(|| String::from("—"));
                        spans.push(Span::styled(
                            format!("{} project · {} deps · ", dep.manager_label, dep.dep_count),
                            Style::default().fg(Color::Gray),
                        ));
                        spans.push(Span::styled(size, Style::default().fg(Color::Green)));
                    }
                    None => {
                        spans.push(Span::styled(
                            "no project dependencies in view",
                            Style::default().fg(Color::Gray),
                        ));
                    }
                },
            }
        }
    }
    spans
}

fn side_panel_heights(side_height: u16, disk_count: usize, packages_visible: bool) -> (u16, u16) {
    if side_height == 0 {
        return (0, 0);
    }
    if side_height <= 8 {
        if !packages_visible {
            return (side_height, 0);
        }
        // Short side column with packages focused/loaded: split so the
        // packages pane is never zero-height (Tab/p could otherwise focus an
        // invisible pane). Reserve up to 5 rows for packages without starving
        // disks below 3.
        let package_height = side_height.saturating_sub(3).clamp(1, 5);
        let disk_height = side_height.saturating_sub(package_height);
        return (disk_height, package_height);
    }

    let desired_disk_height = if disk_count == 0 {
        3
    } else {
        (disk_count as u16).saturating_mul(4).saturating_add(2)
    };
    let min_package_height = if packages_visible { 7 } else { 5 };
    let reserved_package_height = min_package_height.min(side_height.saturating_sub(6));
    let max_disk_height = side_height.saturating_sub(reserved_package_height);
    let disk_height = desired_disk_height.min(max_disk_height).max(3);
    (disk_height, side_height.saturating_sub(disk_height))
}

fn disk_window_bounds(selected: usize, total_items: usize, max_rows: usize) -> (usize, usize) {
    if total_items == 0 {
        return (0, 0);
    }
    let max_rows = max_rows.max(1);
    let selected = selected.min(total_items.saturating_sub(1));
    let half = max_rows / 2;
    let max_offset = total_items.saturating_sub(max_rows);
    let offset = selected.saturating_sub(half).min(max_offset);
    let end = (offset + max_rows).min(total_items);
    (offset, end)
}

/// One-line summary of a disk for the collapsed side column: volume, usage
/// percent, and free space, colored by fullness and truncated to fit.
fn compact_disk_line(disk: &crate::app::DiskInfo, selected: bool, width: usize) -> Line<'static> {
    let used = disk.total.saturating_sub(disk.available);
    let pct = if disk.total > 0 {
        (used as f64 / disk.total as f64 * 100.0) as u16
    } else {
        0
    };
    let color = if pct > 90 {
        Color::Red
    } else if pct > 75 {
        Color::Yellow
    } else {
        Color::Green
    };
    let marker = if selected { "▸ " } else { "  " };
    let text = format!(
        "{marker}{}  {pct}%  {} free",
        volume_label(&disk.mount),
        human(disk.available)
    );
    let mut style = Style::default().fg(color);
    if selected {
        style = style.add_modifier(Modifier::BOLD);
    }
    Line::from(Span::styled(truncate(&text, width), style))
}

fn disk_label(disk: &crate::app::DiskInfo) -> String {
    let volume = volume_label(&disk.mount);
    if disk.name.is_empty() {
        format!("{volume}  {}", disk.mount.display())
    } else {
        format!("{volume}  {} ({})", disk.mount.display(), disk.name)
    }
}

fn volume_label(mount: &std::path::Path) -> String {
    mount
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| String::from("Root volume"))
}

fn size_detail(size: SizeInfo) -> String {
    if size.allocated == size.logical {
        return human(size.logical);
    }
    format!(
        "{} disk · {} apparent",
        human(size.allocated),
        human(size.logical)
    )
}

fn size_detail_with_cache_marker(size: SizeInfo, stale: bool) -> String {
    let detail = size_detail(size);
    if stale {
        format!("~{detail}")
    } else {
        detail
    }
}

/// Compact summary for the files-pane border title: item count, total size,
/// scan coverage, the active sort mode, and any marked-items total. Replaces
/// the old `files (N)` title and the sort/hidden text that used to clutter the
/// app header.
fn files_pane_title(app: &App, visible_count: usize, total_size: u64, sized: usize) -> String {
    let mut title = if app.search_filter_active() {
        format!("files · {visible_count}/{} items", app.entries.len())
    } else {
        format!("files · {visible_count} items")
    };
    if total_size > 0 {
        title.push_str(&format!(" · {}", human(total_size)));
    }
    if sized < visible_count {
        title.push_str(&format!(" · {sized}/{visible_count} sized"));
    }
    title.push_str(&format!(" · sort {}", app.sort.label()));
    if let Some((count, marked_total, lower_bound)) = app.marked_summary() {
        if marked_total > 0 {
            let prefix = if lower_bound { "≥" } else { "" };
            title.push_str(&format!(
                " · {count} marked {prefix}{}",
                human(marked_total)
            ));
        } else {
            title.push_str(&format!(" · {count} marked"));
        }
    }
    title
}

/// Chip text and color for a reclaim classification: safe caches are green,
/// regenerable build output amber, risky-to-delete red.
fn reclaim_chip(class: Reclaimability) -> (&'static str, Color) {
    match class {
        Reclaimability::Safe => ("cache", Color::Green),
        Reclaimability::Regenerable => ("build", Color::Yellow),
        Reclaimability::Risky => ("keep", Color::Red),
    }
}

/// Color sizes by magnitude so the eye lands on the big directories: dim for
/// sub-MiB, default green for the MiB range, bold amber for GiB and up.
fn size_magnitude_style(bytes: u64) -> Style {
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else if bytes >= MIB {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

/// A trailing "↑/↓ size" badge for a directory whose size changed since the
/// last scan, so the eye goes to what grew rather than the familiar top-N.
fn growth_badge(delta: i64) -> Option<(String, Style)> {
    if delta == 0 {
        return None;
    }
    let magnitude = delta.unsigned_abs();
    if delta > 0 {
        Some((
            format!(" ↑{}", human(magnitude)),
            Style::default().fg(Color::Green),
        ))
    } else {
        Some((
            format!(" ↓{}", human(magnitude)),
            Style::default().fg(Color::Blue),
        ))
    }
}

fn size_with_markers(bytes: u64, inaccessible: u32, skipped_mounts: u32, stale: bool) -> String {
    let mut label = String::new();
    if stale {
        label.push('~');
    }
    if inaccessible > 0 {
        label.push('≥');
    }
    label.push_str(&human(bytes));
    if skipped_mounts > 0 {
        label.push('*');
    }
    label
}

fn stale_cache_status(entry: &crate::app::Entry) -> Option<String> {
    if !entry.size_stale {
        return None;
    }
    let scanned_at = entry.cached_at?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let age = now.saturating_sub(scanned_at);
    Some(format!(
        "cached {}; press r to refresh",
        format_elapsed(age)
    ))
}

fn file_columns(inner_width: u16, show_modified: bool) -> (usize, usize, usize, usize) {
    const ICON_WIDTH: usize = 2;
    const GAP_WIDTH: usize = 2;
    const MIN_NAME_WIDTH: usize = 8;
    const MIN_SIZE_WIDTH: usize = 4;
    const PREFERRED_SIZE_WIDTH: usize = 12;
    const MIN_MODIFIED_WIDTH: usize = 4;
    const PREFERRED_MODIFIED_WIDTH: usize = 8;
    const MIN_BAR_WIDTH: usize = 8;
    const MAX_BAR_WIDTH: usize = 14;
    const PREFERRED_BAR_WIDTH: usize = 10;
    const BAR_THRESHOLD_WIDTH: usize = 55;

    let inner_width = inner_width as usize;
    let content_width = inner_width.saturating_sub(ICON_WIDTH);
    let show_bar = inner_width >= BAR_THRESHOLD_WIDTH;
    let preferred_bar_width = if show_bar {
        PREFERRED_BAR_WIDTH.clamp(MIN_BAR_WIDTH, MAX_BAR_WIDTH)
    } else {
        0
    };

    if !show_modified {
        let bare_name_width = content_width.max(1);
        let room_for_size = content_width.saturating_sub(MIN_NAME_WIDTH);
        if room_for_size < MIN_SIZE_WIDTH {
            return (bare_name_width, 0, 0, 0);
        }

        if preferred_bar_width > 0 {
            let minimum_width =
                MIN_NAME_WIDTH + GAP_WIDTH + MIN_SIZE_WIDTH + GAP_WIDTH + MIN_BAR_WIDTH;
            if room_for_size >= minimum_width.saturating_sub(MIN_NAME_WIDTH) {
                let mut size_width = PREFERRED_SIZE_WIDTH.min(room_for_size);
                let mut bar_width = preferred_bar_width;
                let mut required = MIN_NAME_WIDTH + GAP_WIDTH + size_width + GAP_WIDTH + bar_width;
                if required > content_width {
                    let mut overflow = required - content_width;
                    let shrink_bar = (bar_width.saturating_sub(MIN_BAR_WIDTH)).min(overflow);
                    bar_width -= shrink_bar;
                    overflow -= shrink_bar;
                    size_width = size_width.saturating_sub(overflow).max(MIN_SIZE_WIDTH);
                    required = MIN_NAME_WIDTH + GAP_WIDTH + size_width + GAP_WIDTH + bar_width;
                }
                if required <= content_width {
                    let name_width = content_width
                        .saturating_sub(GAP_WIDTH + size_width + GAP_WIDTH + bar_width)
                        .max(1);
                    return (name_width, size_width, 0, bar_width);
                }
            }
        }

        let size_width = room_for_size.min(PREFERRED_SIZE_WIDTH);
        let name_width = content_width.saturating_sub(GAP_WIDTH + size_width).max(1);
        return (name_width, size_width, 0, 0);
    }

    let room_for_name_and_meta = content_width;
    let min_for_modified_only = MIN_NAME_WIDTH + GAP_WIDTH + MIN_MODIFIED_WIDTH;
    if room_for_name_and_meta < min_for_modified_only {
        return (content_width.max(1), 0, 0, 0);
    }

    let min_for_size_and_modified =
        MIN_NAME_WIDTH + GAP_WIDTH + MIN_SIZE_WIDTH + GAP_WIDTH + MIN_MODIFIED_WIDTH;
    let mut size_width;
    let mut modified_width;
    if room_for_name_and_meta >= min_for_size_and_modified && inner_width >= 50 {
        size_width = PREFERRED_SIZE_WIDTH;
        modified_width = PREFERRED_MODIFIED_WIDTH;
        let mut required = MIN_NAME_WIDTH + GAP_WIDTH + size_width + GAP_WIDTH + modified_width;
        if required > room_for_name_and_meta {
            let mut overflow = required - room_for_name_and_meta;
            let shrink_modified = modified_width
                .saturating_sub(MIN_MODIFIED_WIDTH)
                .min(overflow);
            modified_width -= shrink_modified;
            overflow -= shrink_modified;
            size_width = size_width.saturating_sub(overflow).max(MIN_SIZE_WIDTH);
            required = MIN_NAME_WIDTH + GAP_WIDTH + size_width + GAP_WIDTH + modified_width;
        }
        if required <= room_for_name_and_meta {
            let name_width = content_width
                .saturating_sub(GAP_WIDTH + size_width + GAP_WIDTH + modified_width)
                .max(1);
            if preferred_bar_width == 0 {
                return (name_width, size_width, modified_width, 0);
            }

            let mut bar_width = preferred_bar_width;
            let mut required = MIN_NAME_WIDTH
                + GAP_WIDTH
                + size_width
                + GAP_WIDTH
                + modified_width
                + GAP_WIDTH
                + bar_width;
            if required > room_for_name_and_meta {
                let mut overflow = required - room_for_name_and_meta;
                let shrink_bar = (bar_width.saturating_sub(MIN_BAR_WIDTH)).min(overflow);
                bar_width -= shrink_bar;
                overflow -= shrink_bar;
                let shrink_modified =
                    (modified_width.saturating_sub(MIN_MODIFIED_WIDTH)).min(overflow);
                modified_width -= shrink_modified;
                overflow -= shrink_modified;
                size_width = size_width.saturating_sub(overflow).max(MIN_SIZE_WIDTH);
                required = MIN_NAME_WIDTH
                    + GAP_WIDTH
                    + size_width
                    + GAP_WIDTH
                    + modified_width
                    + GAP_WIDTH
                    + bar_width;
            }

            if required <= room_for_name_and_meta {
                let name_width = content_width
                    .saturating_sub(
                        GAP_WIDTH + size_width + GAP_WIDTH + modified_width + GAP_WIDTH + bar_width,
                    )
                    .max(1);
                return (name_width, size_width, modified_width, bar_width);
            }
        }
    }

    let modified_width = room_for_name_and_meta
        .saturating_sub(MIN_NAME_WIDTH + GAP_WIDTH)
        .clamp(MIN_MODIFIED_WIDTH, PREFERRED_MODIFIED_WIDTH);
    let name_width = content_width
        .saturating_sub(GAP_WIDTH + modified_width)
        .max(1);
    let bar_width = if preferred_bar_width == 0 {
        0
    } else {
        let mut bar_width = preferred_bar_width;
        let mut required = MIN_NAME_WIDTH + GAP_WIDTH + modified_width + GAP_WIDTH + bar_width;
        if required > room_for_name_and_meta {
            let overflow = required - room_for_name_and_meta;
            bar_width = bar_width.saturating_sub(overflow).max(MIN_BAR_WIDTH);
            required = MIN_NAME_WIDTH + GAP_WIDTH + modified_width + GAP_WIDTH + bar_width;
        }
        if required <= room_for_name_and_meta {
            bar_width
        } else {
            0
        }
    };
    if bar_width == 0 {
        return (name_width, 0, modified_width, 0);
    }
    (name_width, 0, modified_width, bar_width)
}

fn package_columns(inner_width: u16) -> (usize, usize) {
    const GAP_WIDTH: usize = 2;
    const MIN_NAME_WIDTH: usize = 10;
    const MIN_SIZE_WIDTH: usize = 4;
    const PREFERRED_SIZE_WIDTH: usize = 9;

    let inner_width = inner_width as usize;
    let room_for_size = inner_width.saturating_sub(GAP_WIDTH + MIN_NAME_WIDTH);
    if room_for_size < MIN_SIZE_WIDTH {
        return (inner_width.max(1), 0);
    }

    let size_width = room_for_size.min(PREFERRED_SIZE_WIDTH);
    let name_width = inner_width.saturating_sub(GAP_WIDTH + size_width).max(1);
    (name_width, size_width)
}

fn file_size_bar(
    size: Option<u64>,
    max_visible_size: u64,
    total_visible_size: u64,
    bar_width: usize,
) -> (String, String, Style) {
    if bar_width == 0 {
        return (String::new(), String::new(), Style::default());
    }

    let Some(size) = size else {
        let blank = format!("{: <bar_width$}", " ");
        return (
            blank,
            String::from(" --%"),
            Style::default().fg(Color::DarkGray),
        );
    };

    if max_visible_size == 0 {
        let blank = format!("{: <bar_width$}", " ");
        return (
            blank,
            format!("{:>3}%", 0),
            Style::default().fg(Color::DarkGray),
        );
    }

    let percent_of_max = if size >= max_visible_size {
        100
    } else {
        rounded_percent(size, max_visible_size)
    };
    let percent_of_sum = rounded_percent(size, total_visible_size);
    let filled = if size == 0 {
        0
    } else {
        let numerator = size.saturating_mul(u64::try_from(bar_width).unwrap_or(0));
        if max_visible_size == 0 || numerator == 0 {
            0
        } else {
            numerator
                .saturating_div(max_visible_size)
                .max(1)
                .min(u64::try_from(bar_width).unwrap_or(u64::MAX))
        }
    };
    let filled = usize::try_from(filled).unwrap_or(0).min(bar_width);

    let style = if percent_of_max >= 50 {
        Style::default().fg(Color::Red)
    } else if percent_of_max >= 25 {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::White)
    };
    let bar_text = format!(
        "{}{}",
        "█".repeat(filled),
        "░".repeat(bar_width.saturating_sub(filled)),
    );
    (bar_text, format!("{percent_of_sum:>3}%"), style)
}

#[allow(clippy::items_after_test_module)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_preserves_short_names() {
        assert_eq!(truncate("Downloads", 40), "Downloads");
    }

    #[test]
    fn truncate_keeps_unicode_boundary() {
        assert_eq!(truncate("résumé.txt", 7), "résumé…");
    }

    #[test]
    fn truncate_uses_terminal_display_width() {
        let truncated = truncate("漢字abc", 5);
        assert_eq!(truncated, "漢字…");
        assert_eq!(display_width(&truncated), 5);
    }

    #[test]
    fn pad_truncated_pads_by_terminal_display_width() {
        let padded = pad_truncated("漢字a", 6);
        assert_eq!(display_width(&padded), 6);
        assert_eq!(padded, "漢字a ");
    }

    #[test]
    fn truncate_start_keeps_path_tail() {
        assert_eq!(truncate_start("/Users/milo/Downloads", 10), "…Downloads");
    }

    #[test]
    fn centered_rect_uses_expected_percent_area() {
        let area = Rect::new(0, 0, 100, 40);
        let got = centered_rect(60, 20, 40, 6, area);
        assert_eq!(got, Rect::new(20, 16, 60, 8));
    }

    #[test]
    fn centered_rect_respects_minimum_size() {
        let area = Rect::new(0, 0, 80, 24);
        let got = centered_rect(60, 20, 40, 6, area);
        assert_eq!(got.height, 6);
    }

    #[test]
    fn reclaim_panel_layout_keeps_detail_below_list() {
        let area = Rect::new(15, 4, 70, 28);
        let (list, detail) = reclaim_panel_layout(area);

        assert_eq!(list.x, area.x);
        assert_eq!(detail.x, area.x);
        assert_eq!(list.width, area.width);
        assert_eq!(detail.width, area.width);
        assert_eq!(list.y + list.height, detail.y);
        assert_eq!(detail.height, 9);
        assert!(list.height >= 7);
    }

    #[test]
    fn file_columns_hide_size_on_narrow_widths() {
        assert_eq!(file_columns(10, false), (8, 0, 0, 0));
    }

    #[test]
    fn file_columns_keep_size_when_space_allows() {
        assert_eq!(file_columns(30, false), (14, 12, 0, 0));
    }

    #[test]
    fn file_columns_show_modified_in_wide_view() {
        let (name_width, size_width, modified_width, bar_width) = file_columns(60, true);
        assert!(size_width > 0);
        assert!(modified_width > 0);
        assert!(bar_width > 0);
        // Account for ICON_WIDTH (2) + GAP_WIDTH (2) * 3 = 8
        assert_eq!(name_width + size_width + modified_width + bar_width + 8, 60);
        assert_eq!(bar_width, 10);
    }

    #[test]
    fn file_columns_swap_modified_when_space_is_limited() {
        let (name_width, size_width, modified_width, bar_width) = file_columns(35, true);
        assert!(size_width == 0);
        assert_eq!(name_width + modified_width + 4, 35);
        assert!(modified_width >= 4);
        assert_eq!(bar_width, 0);
    }

    #[test]
    fn file_columns_show_bar_at_wide_width() {
        let (name_width, size_width, modified_width, bar_width) = file_columns(90, false);
        assert_eq!(modified_width, 0);
        assert_eq!(bar_width, 10);
        // Account for ICON_WIDTH (2) + GAP_WIDTH (2) * 2 = 6
        assert_eq!(name_width + size_width + bar_width + 6, 90);
    }

    #[test]
    fn file_size_bar_renders_scaled_blocks() {
        let (bar, percent, _style) = file_size_bar(Some(50), 100, 200, 10);
        // bar.len() returns bytes (█ is 3 bytes), use chars().count() for visual width
        assert_eq!(bar.chars().count(), 10);
        assert_eq!(percent, " 25%");
    }

    #[test]
    fn file_size_bar_handles_unknown_size() {
        let (bar, percent, _style) = file_size_bar(None, 100, 200, 10);
        assert_eq!(bar.len(), 10);
        assert_eq!(percent, " --%");
    }

    #[test]
    fn package_columns_hide_size_when_narrow() {
        assert_eq!(package_columns(12), (12, 0));
    }

    #[test]
    fn growth_badge_shows_direction_and_skips_zero() {
        let (up, _) = growth_badge(4096).unwrap();
        assert!(up.contains('↑'));
        let (down, _) = growth_badge(-4096).unwrap();
        assert!(down.contains('↓'));
        assert!(growth_badge(0).is_none());
    }

    #[test]
    fn size_magnitude_style_grades_by_size() {
        let kb = size_magnitude_style(500);
        let mb = size_magnitude_style(5 * 1024 * 1024);
        let gb = size_magnitude_style(5 * 1024 * 1024 * 1024);
        assert_ne!(kb, mb);
        assert_ne!(mb, gb);
    }

    #[test]
    fn side_panel_keeps_room_for_packages() {
        assert_eq!(side_panel_heights(21, 12, true), (14, 7));
    }

    #[test]
    fn side_panel_gives_packages_rows_on_short_terminal_when_visible() {
        // Packages not visible: the short side column is all disks.
        assert_eq!(side_panel_heights(8, 2, false), (8, 0));
        // Packages focused/loaded on the same short column: both panes get
        // rows so a focused packages pane is never zero-height.
        let (disk, pkg) = side_panel_heights(8, 2, true);
        assert!(pkg > 0, "packages pane starved on short terminal");
        assert!(disk > 0, "disks pane starved on short terminal");
        assert_eq!(disk + pkg, 8);
    }

    #[test]
    fn disk_window_tracks_selected_disk() {
        assert_eq!(disk_window_bounds(0, 12, 3), (0, 3));
        assert_eq!(disk_window_bounds(8, 12, 3), (7, 10));
        assert_eq!(disk_window_bounds(11, 12, 3), (9, 12));
    }

    #[test]
    fn compact_disk_line_truncates_to_width() {
        let disk = crate::app::DiskInfo {
            name: String::new(),
            mount: std::path::PathBuf::from("/Volumes/Data"),
            total: 1000,
            available: 280,
        };
        let line = compact_disk_line(&disk, false, 18);
        assert!(line.width() <= 18, "compact disk line overflowed: {line:?}");
        let selected = compact_disk_line(&disk, true, 40);
        assert!(selected.width() <= 40);
    }

    #[test]
    fn disk_label_prefers_mount_component_over_device_node() {
        let disk = crate::app::DiskInfo {
            name: String::from("/dev/disk3s1"),
            mount: std::path::PathBuf::from("/Volumes/Projects"),
            total: 100,
            available: 50,
        };

        assert_eq!(
            disk_label(&disk),
            "Projects  /Volumes/Projects (/dev/disk3s1)"
        );
    }

    #[test]
    fn file_window_bounds_scrolls_to_keep_selection_visible() {
        assert_eq!(file_window_bounds(0, 100, 0, 10), (0, 10));
        assert_eq!(file_window_bounds(9, 100, 0, 10), (0, 10));
        assert_eq!(file_window_bounds(10, 100, 0, 10), (1, 11));
        assert_eq!(file_window_bounds(50, 100, 40, 10), (41, 51));
    }
}

fn rounded_percent(value: u64, total: u64) -> u64 {
    value
        .saturating_mul(100)
        .saturating_add(total / 2)
        .checked_div(total)
        .unwrap_or(0)
        .min(100)
}

fn spinner_char() -> char {
    let spinners = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let index = ((ms / 60) % spinners.len() as u128) as usize;
    spinners[index]
}

fn big_spinner_char() -> char {
    let spinners = ['⣾', '⣽', '⣻', '⢿', '⡿', '⣟', '⣯', '⣷'];
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let index = ((ms / 120) % spinners.len() as u128) as usize;
    spinners[index]
}

fn activity_bar(width: usize) -> String {
    let bar_width = 20.min(width.saturating_sub(10)).max(10);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    let period = 2000; // 2 seconds back and forth
    let pos_t = (ms % period) as f64 / period as f64; // 0.0 to 1.0
    let pos = if pos_t < 0.5 {
        pos_t * 2.0
    } else {
        2.0 - pos_t * 2.0
    };

    let active_pos = (pos * (bar_width - 1) as f64).round() as usize;

    let mut bar = vec!['·'; bar_width];
    bar[active_pos] = '●';

    format!(
        "  {}  ",
        bar.into_iter()
            .map(|c| format!("{c} "))
            .collect::<String>()
            .trim_end()
    )
}
