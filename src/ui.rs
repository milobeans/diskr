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
    format_elapsed, format_full_timestamp, format_modified_time, human, size_sort_key, ActiveModal,
    App, Focus, ListingState, PkgView, SortMode,
};
use crate::bulkstat::SizeInfo;
use crate::keymap::{self, KeySection};
use crate::packages::{DepEvidence, PackageUseStatus};
use crate::reclaim::Reclaimability;
use crate::terminal_text;

fn decoration<'a>(unicode: &'a str, ascii: &'a str) -> &'a str {
    if crate::terminal_backend::Appearance::configured().ascii {
        ascii
    } else {
        unicode
    }
}

fn themed_block<'a>() -> Block<'a> {
    block_with_ascii(crate::terminal_backend::Appearance::configured().ascii)
}

fn block_with_ascii<'a>(ascii: bool) -> Block<'a> {
    let block = Block::default();
    if !ascii {
        return block;
    }
    block.border_set(ratatui_core::symbols::border::Set {
        top_left: "+",
        top_right: "+",
        bottom_left: "+",
        bottom_right: "+",
        vertical_left: "|",
        vertical_right: "|",
        horizontal_top: "-",
        horizontal_bottom: "-",
    })
}

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
    let disk_expanded = app.focus == Focus::Disks;
    let (disk_height, package_height) = if disk_expanded {
        (disk_height, package_height)
    } else {
        let height = (app.disks.len() as u16)
            .saturating_add(2)
            .max(3)
            .min(body[1].height);
        (height, body[1].height.saturating_sub(height))
    };

    let side = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(disk_height),
            Constraint::Length(package_height),
        ])
        .split(body[1]);

    if app.top_files_visible() {
        draw_top_files(f, app);
    } else if app.full_width {
        match app.focus {
            Focus::Files => draw_files(f, root[1], app),
            Focus::Disks => draw_disks(f, root[1], app, true),
            Focus::Packages => draw_packages(f, root[1], app),
            Focus::Reclaim => {}
        }
    } else {
        draw_files(f, body[0], app);
        draw_disks(f, side[0], app, disk_expanded);
        draw_packages(f, side[1], app);
    }

    draw_status(f, root[2], app);
    draw_help(f, root[3], app.focus);

    if app.focus == Focus::Reclaim && !app.reclaim_paths_open() {
        draw_reclaim_panel(f, app);
    }
    match app.active_modal() {
        ActiveModal::Help => draw_help_overlay(f, app),
        ActiveModal::Locations => draw_locations(f, app),
        ActiveModal::Delete => draw_confirm(f, app),
        ActiveModal::Uninstall => draw_uninstall_confirm(f, app),
        ActiveModal::EmptyTrash => draw_empty_trash_confirm(f, app),
        ActiveModal::TopFiles => {}
        ActiveModal::ReclaimPaths => draw_reclaim_paths(f, app),
        ActiveModal::DiskInfo => draw_disk_info_modal(f, app),
        ActiveModal::Package => match app.pkg_view {
            PkgView::SystemManagers => draw_pkg_detail(f, app),
            PkgView::ProjectDeps => draw_project_dep_detail(f, app),
        },
        ActiveModal::FileInfo => draw_file_info(f, app),
        ActiveModal::Input => draw_input_overlay(f, app),
        ActiveModal::None => {}
    }
}

fn draw_input_overlay(f: &mut Frame, app: &App) {
    let area = centered_rect(50, 20, 40, 6, f.area());
    let block = themed_block()
        .title(app.input_prompt.as_str())
        .borders(Borders::ALL);
    let text = vec![
        Line::from(""),
        Line::from(
            app.input_cursor
                .viewport(&app.input_buffer, area.width.saturating_sub(2) as usize),
        ),
    ];
    let paragraph = Paragraph::new(text).block(block).wrap(Wrap { trim: false });
    f.render_widget(Clear, area);
    f.render_widget(paragraph, area);
}

fn draw_locations(f: &mut Frame, app: &App) {
    let area = centered_rect(84, 80, 64, 20, f.area());
    f.render_widget(Clear, area);
    let block = themed_block()
        .title(" go to folder / package profile ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let chooser = &app.locations;
    let message = if let Some(error) = &chooser.error {
        terminal_text::sanitize(error).into_owned()
    } else if chooser.loading {
        String::from("Loading saved roots and profiles...")
    } else {
        String::from("Folders browse files; profiles apply their saved inventory scope.")
    };
    if let Some(input) = &chooser.path_input {
        let footer_height = u16::from(inner.height >= 2);
        let label_height = u16::from(inner.height >= 3);
        let message_height = inner
            .height
            .saturating_sub(footer_height + label_height + 1)
            .min(2);
        let editor = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(message_height),
                Constraint::Length(label_height),
                Constraint::Min(1),
                Constraint::Length(footer_height),
            ])
            .split(inner);
        f.render_widget(
            Paragraph::new(message).wrap(Wrap { trim: false }),
            editor[0],
        );
        f.render_widget(
            Paragraph::new("Folder path (absolute, relative, or ~/):"),
            editor[1],
        );
        f.render_widget(
            Paragraph::new(chooser.cursor.viewport(input, usize::from(editor[2].width))),
            editor[2],
        );
        f.render_widget(Paragraph::new("Enter go · Esc back"), editor[3]);
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(7),
            Constraint::Length(2),
        ])
        .split(inner);
    f.render_widget(
        Paragraph::new(message).wrap(Wrap { trim: false }),
        chunks[0],
    );
    let visible = usize::from(chunks[1].height).max(1);
    let selected = chooser
        .selected
        .min(chooser.entries.len().saturating_sub(1));
    let offset = selected.saturating_sub(visible.saturating_sub(1));
    let items = chooser
        .entries
        .iter()
        .skip(offset)
        .take(visible)
        .map(|entry| {
            let kind = if entry.foreign {
                "[foreign] "
            } else if entry.profile.is_some() {
                "[profile] "
            } else {
                ""
            };
            ListItem::new(truncate(
                &format!("{kind}{} · {}", entry.label, entry.path.display()),
                usize::from(chunks[1].width.saturating_sub(2)),
            ))
        })
        .collect::<Vec<_>>();
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(decoration("▶ ", "> "));
    let mut state = ListState::default();
    if !chooser.entries.is_empty() {
        state.select(Some(selected.saturating_sub(offset)));
    }
    f.render_stateful_widget(list, chunks[1], &mut state);
    if let Some(entry) = chooser.entries.get(selected) {
        let mut details = vec![
            Line::from(truncate_start(
                &entry.path.display().to_string(),
                usize::from(chunks[2].width),
            )),
            Line::from(truncate(
                &format!("Origin: {}", entry.origin),
                usize::from(chunks[2].width),
            )),
        ];
        if entry.foreign {
            details.push(Line::from(Span::styled(
                "Unavailable: recreate this profile locally.",
                Style::default().fg(Color::Yellow),
            )));
        }
        if let Some(scope) = &entry.scope {
            let managers = if scope.managers.is_empty() {
                String::from("none")
            } else {
                scope
                    .managers
                    .iter()
                    .map(|manager| manager.key())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            details.push(Line::from(format!("Managers: {managers}")));
            details.push(Line::from(format!(
                "Projects {} · depth {} · {} exclusions",
                if scope.projects { "on" } else { "off" },
                scope.depth,
                scope.excludes.len()
            )));
        }
        f.render_widget(
            Paragraph::new(details).wrap(Wrap { trim: false }),
            chunks[2],
        );
    }
    f.render_widget(
        Paragraph::new(vec![
            Line::from("↑↓ select · Enter go · / path"),
            Line::from("Esc/g close"),
        ]),
        chunks[3],
    );
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![Span::styled(
        "diskr",
        Style::default().add_modifier(Modifier::BOLD),
    )];
    if app.fda_limited {
        spans.push(Span::styled(
            " · limited access",
            Style::default().fg(Color::Yellow),
        ));
    }
    if app.cwd_enumeration_errors > 0 {
        spans.push(Span::styled(
            format!(" · {} entry errors", app.cwd_enumeration_errors),
            Style::default().fg(Color::Yellow),
        ));
    }
    let mut used = spans.iter().map(Span::width).sum::<usize>();
    let width = usize::from(area.width);
    if app.fda_limited && width.saturating_sub(used) >= 20 {
        spans.push(Span::styled(
            " · ? help",
            Style::default().fg(Color::Yellow),
        ));
        used += 9;
    }
    let path = truncate_start(
        &app.cwd.display().to_string(),
        width.saturating_sub(used + 3),
    );
    if !path.is_empty() {
        used += display_width(&path) + 3;
        spans.push(Span::raw(" · "));
        spans.push(Span::styled(path, Style::default().fg(Color::Cyan)));
    }
    let mut context = Vec::new();
    if app.show_hidden {
        context.push((String::from("hidden on"), Color::Gray));
    }
    if let Some(baseline) = app.history_baseline_status() {
        context.push((baseline, Color::DarkGray));
    }
    if let Some(delta) = app.history_delta_status() {
        context.push((delta, Color::Green));
    }
    for (text, color) in context {
        let needed = display_width(&text) + 3;
        if used + needed <= width {
            spans.push(Span::styled(
                format!(" · {text}"),
                Style::default().fg(color),
            ));
            used += needed;
        }
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
    let content_width = area.width.saturating_sub(4);
    let visible_count = app.visible_entry_count();
    if visible_count == 0 {
        let block = themed_block()
            .borders(Borders::ALL)
            .title("files · 0 items")
            .border_style(Style::default().fg(border_color));
        let message = match &app.listing_state {
            ListingState::Loading => app.listing_progress(),
            ListingState::Cancelled => {
                String::from("listing cancelled · shown entries are partial · r retry")
            }
            ListingState::Unreadable(message) => format!("{message} · r retry · Backspace parent"),
            ListingState::Ready if app.cwd_enumeration_errors > 0 => format!(
                "no visible entries · {} entries unreadable · r retry",
                app.cwd_enumeration_errors
            ),
            ListingState::Ready if app.entries.is_empty() && app.hidden_entry_count > 0 => {
                String::from("no visible files · . shows hidden")
            }
            ListingState::Ready if app.entries.is_empty() => String::from("empty directory"),
            ListingState::Ready => String::from("no matching entries"),
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
    let (max_visible_size, total_visible_size, sized_count) = app.file_aggregates();
    let block = themed_block()
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
                    format!("{} scanning…", scan_frame()),
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
                    decoration("✓▸", "*>")
                } else if e.is_symlink {
                    decoration("✓↪", "*@")
                } else {
                    decoration("✓ ", "* ")
                }
            } else if e.is_dir {
                decoration("▸ ", "> ")
            } else if e.is_symlink {
                decoration("↪ ", "@ ")
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
            if content_width < 3 {
                return ListItem::new(truncate(icon, content_width as usize));
            }
            let mut growth = if e.scanning {
                None
            } else {
                e.size_delta.and_then(growth_badge)
            };
            let mut chip = app.reclaim_class(&e.path).map(|class| {
                let (name, color) = reclaim_chip(class);
                (format!(" [{name}]"), Style::default().fg(color))
            });
            let max_suffix = content_width.saturating_sub(3) as usize;
            let growth_width = growth.as_ref().map_or(0, |(text, _)| display_width(text));
            let chip_width = chip.as_ref().map_or(0, |(text, _)| display_width(text));
            if growth_width + chip_width > max_suffix {
                chip = None;
            }
            if growth_width > max_suffix {
                growth = None;
            }
            let suffix_width = growth.as_ref().map_or(0, |(text, _)| display_width(text))
                + chip.as_ref().map_or(0, |(text, _)| display_width(text));
            let (name_width, size_width, modified_width, bar_width) =
                file_columns_for_row(content_width, suffix_width, show_modified);
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
                spans.push(Span::styled(
                    pad_left_truncated(&size_str, size_width),
                    size_style,
                ));
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
                    pad_left_truncated(&modified, modified_width),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            if let Some((badge, badge_style)) = growth {
                spans.push(Span::styled(badge, badge_style));
            }
            if let Some((badge, badge_style)) = chip {
                spans.push(Span::styled(badge, badge_style));
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
        .highlight_symbol(decoration("▶ ", "> "));

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
    let block = themed_block()
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
            .block(themed_block().title(label))
            .gauge_style(gauge_style)
            .percent(pct.min(100));
        f.render_widget(gauge, rows[visible_i]);
    }
}

fn draw_reclaim_panel(f: &mut Frame, app: &mut App) {
    let area = if app.full_width {
        browser_area(f)
    } else {
        centered_rect(70, 70, 72, 22, f.area())
    };
    f.render_widget(Clear, area);

    if app.reclaim_loading {
        app.reclaim_page_rows = area.height.saturating_sub(2).max(1) as usize;
        let inner = themed_block().borders(Borders::ALL).title(" reclaim ");
        let body = Paragraph::new(vec![
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    format!("{} ", scan_frame()),
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
            let inner = themed_block().borders(Borders::ALL).title(" reclaim ");
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
        let inner = themed_block().borders(Borders::ALL).title(" reclaim ");
        let body = Paragraph::new(format!(
            "no cleanup candidates found within scope\n{}",
            report.scope.summary()
        ))
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
            themed_block()
                .borders(Borders::ALL)
                .title(format!(
                    " cleanup {} · {} candidate disk bytes ",
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
        .highlight_symbol(decoration("▶ ", "> "));

    let mut state = ListState::default();
    state.select(Some(app.selected_reclaim.min(item_count.saturating_sub(1))));
    f.render_stateful_widget(list, list_area, &mut state);

    if let Some(finding) = app.selected_reclaim_finding() {
        let size_line = format!(
            "{} · {} {}",
            if finding.size.allocated == finding.size.logical {
                human(finding.size.allocated)
            } else {
                format!(
                    "{} / {}",
                    human(finding.size.allocated),
                    human(finding.size.logical)
                )
            },
            finding.count,
            if finding.count == 1 { "path" } else { "paths" }
        );
        let mut lines = vec![
            Line::from(""),
            Line::from(Span::styled(size_line, Style::default().fg(Color::Green))),
            Line::from(Span::styled(
                finding.note.clone(),
                Style::default().fg(Color::Gray),
            )),
            Line::from("Allocated candidates are not guaranteed freed space; Trash retains bytes."),
            Line::from(report.scope.summary()),
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
        let block = themed_block()
            .borders(Borders::ALL)
            .title(" reclaim finding ")
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = block.inner(detail_area);
        f.render_widget(block, detail_area);
        let regions = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), regions[0]);
        f.render_widget(
            Paragraph::new("Enter paths · d Trash · Esc close"),
            regions[1],
        );
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
/// overlays so their placement and chrome stay identical.
fn modal_list_regions(f: &mut Frame, title: &str) -> Option<(Rect, Rect)> {
    let area = centered_rect(70, 70, 74, 20, f.area());
    list_regions(f, title, area, 2)
}

fn browser_area(f: &Frame) -> Rect {
    let area = f.area();
    Rect::new(
        area.x,
        area.y.saturating_add(1).min(area.bottom()),
        area.width,
        area.height.saturating_sub(3),
    )
}

fn list_regions(f: &mut Frame, title: &str, area: Rect, footer_rows: u16) -> Option<(Rect, Rect)> {
    f.render_widget(Clear, area);
    let block = themed_block()
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
        .constraints([Constraint::Min(1), Constraint::Length(footer_rows)])
        .split(inner);
    Some((chunks[0], chunks[1]))
}

/// Centered single-message overlay (empty/loading states) sharing the list
/// modals' placement.
fn draw_modal_message(f: &mut Frame, title: &str, lines: Vec<Line<'static>>) {
    let area = centered_rect(70, 70, 74, 20, f.area());
    f.render_widget(Clear, area);
    let block = themed_block()
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
        .highlight_symbol(decoration("▶ ", "> "));
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

fn draw_top_files_message(f: &mut Frame, lines: Vec<Line<'static>>) {
    let area = browser_area(f);
    f.render_widget(
        Paragraph::new(lines)
            .block(
                themed_block()
                    .borders(Borders::ALL)
                    .title(" top files · t/Esc return "),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_top_files(f: &mut Frame, app: &mut App) {
    if app.top_files_loading() {
        let mut lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    format!("{} ", scan_frame()),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled("scanning top files…", Style::default().fg(Color::White)),
            ]),
        ];
        if let Some(path) = app.top_files_path() {
            lines.push(Line::from(format!("path: {}", path.display())));
        }
        draw_top_files_message(f, lines);
        return;
    }

    let file_count = app.top_files_count();
    if file_count == 0 {
        let message = if app
            .top_files_scan()
            .is_some_and(|scan| scan.inaccessible > 0 || scan.skipped_mounts > 0)
        {
            "no regular files observed · scan incomplete (unreadable paths or skipped mounts)"
        } else if app.top_files_scan().is_some() {
            "no regular files found"
        } else {
            "no scan in progress"
        };
        draw_top_files_message(f, vec![Line::from(message)]);
        return;
    }

    let title = app
        .top_files_path()
        .map(|path| format!(" top files · {} ", path.display()))
        .unwrap_or_else(|| String::from(" top files "));
    let Some((list_area, footer_area)) = list_regions(f, &title, browser_area(f), 3) else {
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
                        &file
                            .path
                            .strip_prefix(app.top_files_path().unwrap_or(&app.cwd))
                            .unwrap_or(&file.path)
                            .display()
                            .to_string(),
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
            Span::styled(
                format!(
                    "{}total: {total} · {} unreadable · {} mounts skipped",
                    if scan.inaccessible > 0 || scan.skipped_mounts > 0 {
                        "≥ "
                    } else {
                        ""
                    },
                    scan.inaccessible,
                    scan.skipped_mounts
                ),
                Style::default().fg(Color::Green),
            ),
            Span::styled(
                format!(" · {}-{} of {}", offset + 1, end, file_count),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(truncate_start(
            &app.selected_top_file_path()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            footer_area.width as usize,
        )),
        Line::from("↑↓ select · Enter/f Finder · O open · d Trash · t/Esc return"),
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

/// Standard centered placement for the record-like detail modals (disk, file,
/// package, project-deps info). Clears a consistently-sized area and returns it,
/// so the four modals share one placement instead of each rolling its own
/// dimensions.
fn detail_modal_area(f: &mut Frame) -> Rect {
    let area = centered_rect(78, 70, 64, 20, f.area());
    f.render_widget(Clear, area);
    area
}

/// Render a record modal's bordered, wrapped detail block of `lines` into a
/// [`detail_modal_area`] region with the shared chrome.
fn render_detail_modal(
    f: &mut Frame,
    area: Rect,
    title: &str,
    border: Color,
    lines: Vec<Line<'_>>,
    footer: Line<'_>,
    scroll: usize,
) {
    let block = themed_block()
        .borders(Borders::ALL)
        .title(title.to_string())
        .border_style(Style::default().fg(border));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let content_height = inner.height.saturating_sub(2);
    let content_area = Rect {
        height: content_height,
        ..inner
    };
    let footer_area = Rect {
        y: inner.y.saturating_add(content_height),
        height: inner.height.saturating_sub(content_height),
        ..inner
    };
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let total = paragraph.line_count(content_area.width);
    let visible = content_area.height as usize;
    let offset = scroll.min(total.saturating_sub(visible));
    f.render_widget(
        paragraph.scroll((u16::try_from(offset).unwrap_or(u16::MAX), 0)),
        content_area,
    );
    let first = if total == 0 { 0 } else { offset + 1 };
    let last = (offset + visible).min(total);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!("↑/↓ PgUp/PgDn scroll · {first}-{last} of {total}"),
                Style::default().fg(Color::DarkGray),
            )),
            footer,
        ]),
        footer_area,
    );
}

fn draw_disk_info_modal(f: &mut Frame, app: &App) {
    let area = detail_modal_area(f);

    if app.disk_info_loading() {
        render_detail_modal(
            f,
            area,
            " disk info ",
            Color::DarkGray,
            vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled(
                        format!("{} ", scan_frame()),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled("loading disk details…", Style::default().fg(Color::White)),
                ]),
            ],
            Line::from("Esc close"),
            0,
        );
        return;
    }

    let Some(report) = app.disk_info_report.as_ref() else {
        render_detail_modal(
            f,
            area,
            " disk info ",
            Color::DarkGray,
            vec![Line::from("no disk details available")],
            Line::from("Esc close"),
            0,
        );
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
    if let Some(error) = &report.apfs_error {
        lines.push(Line::from(vec![
            Span::styled("apfs container ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                terminal_text::sanitize(&error.to_string()).into_owned(),
                Style::default().fg(Color::Red),
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
                for name in names {
                    lines.push(Line::from(Span::styled(
                        truncate(name, area.width.saturating_sub(6) as usize),
                        Style::default().fg(Color::Gray),
                    )));
                }
            }
        }
    }
    if app.disk_snapshots_loading() {
        lines.push(Line::from("refreshing snapshot list…"));
    }
    if app.disk_apfs_loading() {
        lines.push(Line::from("refreshing APFS container…"));
    }
    render_detail_modal(
        f,
        area,
        " disk details ",
        Color::DarkGray,
        lines,
        Line::from("s refresh snapshots · a refresh APFS · Esc close"),
        app.detail_scroll,
    );
}

fn draw_file_info(f: &mut Frame, app: &App) {
    let Some(info) = app.file_info.as_ref() else {
        let area = detail_modal_area(f);
        render_detail_modal(
            f,
            area,
            " file info ",
            Color::DarkGray,
            vec![Line::from("Loading file info…"), Line::from("Esc cancels")],
            Line::from("Esc close"),
            0,
        );
        return;
    };

    let area = detail_modal_area(f);

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
                format!(
                    "{}{count} direct children",
                    if info.direct_item_errors > 0 {
                        "≥"
                    } else {
                        ""
                    }
                ),
                Style::default().fg(Color::White),
            ),
        ]));
    }
    if info.direct_item_errors > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "  {} directory entries could not be read; item count is a lower bound.",
                info.direct_item_errors
            ),
            Style::default().fg(Color::Yellow),
        )));
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
            "  f",
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

    let footer = lines.pop().unwrap_or_default();
    lines.pop();
    render_detail_modal(
        f,
        area,
        " file info ",
        Color::Cyan,
        lines,
        footer,
        app.detail_scroll,
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

    let (system_total, system_partial, project_total, project_partial) = app.package_aggregates();
    let total_str = if app.packages_loaded {
        match app.pkg_view {
            PkgView::SystemManagers => {
                let total = system_total;
                let partial = system_partial;
                if total > 0 || partial || !app.flat_packages().is_empty() {
                    format!(
                        " · {}{} known",
                        if partial { "≥" } else { "" },
                        human(total)
                    )
                } else {
                    String::new()
                }
            }
            PkgView::ProjectDeps => {
                let total = project_total;
                let partial = project_partial;
                if total > 0 || partial || !app.project_deps.is_empty() {
                    format!(
                        " · {}{} known",
                        if partial { "≥" } else { "" },
                        human(total)
                    )
                } else {
                    String::new()
                }
            }
        }
    } else {
        String::new()
    };

    let mut title = Line::from(vec![
        Span::raw(match &app.inventory.profile {
            Some(profile) => format!(" packages · profile {} ─", truncate(profile, 16)),
            None => String::from(" packages ─"),
        }),
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
        Span::styled(total_str.clone(), Style::default().fg(Color::Green)),
        if app.package_scan_partial {
            Span::styled(" [partial]", Style::default().fg(Color::Yellow))
        } else {
            Span::raw("")
        },
        if app.pkg_show_unused {
            Span::styled(" [dependency leaves]", Style::default().fg(Color::Magenta))
        } else {
            Span::raw("")
        },
    ]);

    let title_width = usize::from(area.width.saturating_sub(2));
    let mut bottom_title = None;
    if title.width() > title_width {
        let total = total_str.trim_start_matches(" · ");
        let view_budget = title_width
            .saturating_sub(display_width(total))
            .saturating_sub(if total.is_empty() { 2 } else { 5 });
        let view = truncate(
            if app.pkg_view == PkgView::SystemManagers {
                "[ System ]"
            } else {
                "[ Projects ]"
            },
            view_budget,
        );
        let separator = if view.is_empty() || total.is_empty() {
            ""
        } else {
            " · "
        };
        title = Line::from(vec![
            Span::raw(" "),
            Span::styled(view, Style::default().fg(Color::Yellow)),
            Span::raw(separator),
            Span::styled(
                truncate(total, title_width.saturating_sub(2)),
                Style::default().fg(Color::Green),
            ),
            Span::raw(" "),
        ]);
        let mut metadata = Vec::new();
        if app.package_scan_partial {
            metadata.push(String::from("partial"));
        }
        if app.pkg_show_unused {
            metadata.push(String::from("leaves"));
        }
        let metadata_width = title_width.saturating_sub(2);
        let profile_marker = app.inventory.profile.as_ref().map(|_| "profile".to_owned());
        let required: Vec<_> = metadata.iter().cloned().chain(profile_marker).collect();
        let separator = if display_width(&required.join(" · ")) > metadata_width {
            "·"
        } else {
            " · "
        };
        if let Some(profile) = &app.inventory.profile {
            let name_budget = metadata_width
                .saturating_sub(display_width(&metadata.join(separator)))
                .saturating_sub(if metadata.is_empty() {
                    0
                } else {
                    display_width(separator)
                })
                .saturating_sub("profile".len());
            metadata.push(if name_budget > 1 {
                format!("profile {}", truncate(profile, name_budget - 1))
            } else {
                String::from("profile")
            });
        }
        if !metadata.is_empty() {
            bottom_title = Some(Line::from(Span::styled(
                format!(" {} ", truncate(&metadata.join(separator), metadata_width)),
                Style::default().fg(if app.package_scan_partial {
                    Color::Yellow
                } else {
                    Color::DarkGray
                }),
            )));
        }
    }
    let mut block = themed_block()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(border_color));
    if let Some(title) = bottom_title {
        block = block.title_bottom(title);
    }

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
                        format!("{} ", scan_frame()),
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
    // The List reserves two columns for its selection symbol inside the border.
    let inner_width = area.width.saturating_sub(4);

    if item_count == 0 {
        let message = if app.pkg_filter_active() {
            "no matching packages"
        } else {
            match app.pkg_view {
                PkgView::SystemManagers => "no supported packages found",
                PkgView::ProjectDeps => "no project manifests found",
            }
        };
        let message = if app.packages_loading {
            app.package_progress()
        } else if app.package_scan_partial {
            format!("{message}\ninventory incomplete; p to scan again")
        } else if app.pkg_view == PkgView::ProjectDeps && !app.pkg_filter_active() {
            format!("{message}\n{}", app.project_scope.summary())
        } else {
            message.to_string()
        };
        let empty = Paragraph::new(message)
            .block(block)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true });
        f.render_widget(empty, area);
        return;
    }

    let selected = app.selected_pkg.min(item_count.saturating_sub(1));
    let offset = selected.saturating_sub(area.height.saturating_sub(3) as usize);
    let items: Vec<ListItem> = visible_indices
        .iter()
        .skip(offset)
        .take(area.height.saturating_sub(2) as usize)
        .copied()
        .filter_map(|real_i| match app.pkg_view {
            PkgView::SystemManagers => {
                let (package, manager) = app.flat_packages().get(real_i)?;
                let size = package
                    .size
                    .map(|s| {
                        format!(
                            "{}{}",
                            if package.coverage.is_partial() {
                                "≥"
                            } else {
                                ""
                            },
                            human(s.allocated)
                        )
                    })
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
                    .map(|s| {
                        format!(
                            "{}{}",
                            if dep.coverage.is_partial() { "≥" } else { "" },
                            human(s.allocated)
                        )
                    })
                    .unwrap_or_else(|| String::from("—"));
                let label = format!(
                    "{} · {} deps · {}",
                    dep.manager_label,
                    dep.dep_count,
                    dep.path
                        .strip_prefix(&app.cwd)
                        .unwrap_or(&dep.path)
                        .display()
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
        .highlight_symbol(decoration("▶ ", "> "));

    let mut state = ListState::default();
    state.select(Some(selected.saturating_sub(offset)));
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
    let dep_display = truncate(dep_indicator, name_width);
    let mgr_label = truncate(
        &format!("{manager} "),
        name_width.saturating_sub(display_width(&dep_display)),
    );
    let name_budget = name_width
        .saturating_sub(display_width(&dep_display))
        .saturating_sub(display_width(&mgr_label));
    let name_ver = format!("{name}{ver_display}");
    let padded_name = pad_truncated(&name_ver, name_budget);

    let dep_style = match use_status {
        PackageUseStatus::Untracked => Style::default().fg(Color::Yellow),
        _ => Style::default().fg(Color::DarkGray),
    };
    let mut spans = vec![
        Span::styled(dep_display, dep_style),
        Span::styled(mgr_label, Style::default().fg(Color::DarkGray)),
        Span::styled(padded_name, Style::default().fg(Color::White)),
    ];
    if size_width > 0 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            pad_left_truncated(size, size_width),
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
        pad_truncated(label, name_width),
        Style::default().fg(Color::White),
    )];
    if size_width > 0 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            pad_left_truncated(size, size_width),
            Style::default().fg(Color::Green),
        ));
    }
    ListItem::new(Line::from(spans))
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    if app.search_mode {
        let query = format!(
            "/{}",
            app.search_cursor
                .viewport(&app.search_query, area.width.saturating_sub(9) as usize)
        );
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
        let query = format!(
            "/{}",
            app.pkg_search_cursor.viewport(
                &app.pkg_search_query,
                area.width.saturating_sub(18) as usize
            )
        );
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
    if !app.status.is_empty() {
        let important = app.status.contains("failed") || app.status.starts_with("cannot read");
        let mut spans = vec![Span::styled(
            truncate(&app.status, area.width as usize),
            Style::default().fg(if important {
                Color::Yellow
            } else {
                Color::White
            }),
        )];
        if app.focus == Focus::Files && app.search_filter_active() {
            spans.push(Span::raw(format!(" · /{} · Esc clear", app.search_query)));
        } else if app.focus == Focus::Packages && app.pkg_filter_active() {
            spans.push(Span::raw(format!(
                " · /{} · Esc clear",
                app.pkg_search_query
            )));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
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

fn draw_help_overlay(f: &mut Frame, app: &mut App) {
    let area = centered_rect(84, 80, 64, 22, f.area());
    f.render_widget(Clear, area);

    let block = themed_block()
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

    let (total_lines, page_height) = if app.fda_limited {
        let sections: Vec<&KeySection> = keymap::HELP_SECTIONS.iter().collect();
        let mut lines = access_help_lines();
        lines.push(Line::from(""));
        lines.extend(help_lines(&sections));
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let total = paragraph.line_count(sections_area.width);
        let page = usize::from(sections_area.height);
        app.help_scroll = app.help_scroll.min(total.saturating_sub(page));
        let scroll = u16::try_from(app.help_scroll).unwrap_or(u16::MAX);
        f.render_widget(paragraph.scroll((scroll, 0)), sections_area);
        (total, page)
    } else if sections_area.width >= 76 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(sections_area);
        let (left_sections, right_sections) =
            balanced_help_columns(keymap::HELP_SECTIONS, columns[0].width, columns[1].width);
        let left = help_paragraph(&left_sections);
        let right = help_paragraph(&right_sections);
        let left_lines = help_rendered_lines(&left_sections, columns[0].width);
        let right_lines = help_rendered_lines(&right_sections, columns[1].width);
        let total = left_lines.max(right_lines);
        let page = usize::from(sections_area.height);
        let max_scroll = total.saturating_sub(page);
        app.help_scroll = app.help_scroll.min(max_scroll);
        let scroll = u16::try_from(app.help_scroll).unwrap_or(u16::MAX);
        f.render_widget(left.scroll((scroll, 0)), columns[0]);
        f.render_widget(right.scroll((scroll, 0)), columns[1]);
        (total, page)
    } else {
        let sections: Vec<&KeySection> = keymap::HELP_SECTIONS.iter().collect();
        let paragraph = help_paragraph(&sections);
        let total = help_rendered_lines(&sections, sections_area.width);
        let page = usize::from(sections_area.height);
        let max_scroll = total.saturating_sub(page);
        app.help_scroll = app.help_scroll.min(max_scroll);
        let scroll = u16::try_from(app.help_scroll).unwrap_or(u16::MAX);
        f.render_widget(paragraph.scroll((scroll, 0)), sections_area);
        (total, page)
    };

    let marker = |m: &'static str| Span::styled(m, Style::default().fg(Color::Yellow));
    let note = |s: &'static str| Span::styled(s, Style::default().fg(Color::Gray));
    let visible_start = app.help_scroll.saturating_add(1).min(total_lines.max(1));
    let visible_end = app.help_scroll.saturating_add(page_height).min(total_lines);
    let legend = Line::from(vec![
        Span::styled(
            format!("↑↓ scroll {visible_start}-{visible_end}/{total_lines}  "),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled("markers  ", Style::default().fg(Color::DarkGray)),
        marker("~"),
        note(" cached  "),
        marker("≥"),
        note(" lower bound  "),
        marker("*"),
        note(" mounts skipped  "),
        Span::styled("↑↓", Style::default().fg(Color::Green)),
        note(" since session baseline"),
    ]);
    f.render_widget(Paragraph::new(legend), chunks[1]);
}

fn balanced_help_columns(
    sections: &[KeySection],
    left_width: u16,
    right_width: u16,
) -> (Vec<&KeySection>, Vec<&KeySection>) {
    let mut best: Option<(usize, Vec<&KeySection>, Vec<&KeySection>)> = None;
    for mask in 1usize..(1usize << sections.len()).saturating_sub(1) {
        let mut left = Vec::new();
        let mut right = Vec::new();
        for (index, section) in sections.iter().enumerate() {
            if mask & (1 << index) != 0 {
                left.push(section);
            } else {
                right.push(section);
            }
        }
        let difference = help_rendered_lines(&left, left_width)
            .abs_diff(help_rendered_lines(&right, right_width));
        if best
            .as_ref()
            .map(|(best_difference, _, _)| difference < *best_difference)
            .unwrap_or(true)
        {
            best = Some((difference, left, right));
        }
    }
    best.map(|(_, left, right)| (left, right))
        .unwrap_or_else(|| (sections.iter().collect(), Vec::new()))
}

fn help_rendered_lines(sections: &[&KeySection], width: u16) -> usize {
    let width = usize::from(width).max(1);
    let key_width = sections
        .iter()
        .flat_map(|section| section.bindings.iter())
        .map(|binding| terminal_text::display_width(binding.key))
        .max()
        .unwrap_or(1)
        .min(16);
    sections
        .iter()
        .enumerate()
        .map(|(section_index, section)| {
            let gap = usize::from(section_index > 0);
            let heading = terminal_text::display_width(section.title).div_ceil(width);
            let bindings = section
                .bindings
                .iter()
                .map(|binding| {
                    (2 + key_width + 2 + terminal_text::display_width(binding.action))
                        .div_ceil(width)
                        .max(1)
                })
                .sum::<usize>();
            gap + heading.max(1) + bindings
        })
        .sum()
}

fn access_help_lines() -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            "Limited disk access",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("macOS blocks some protected folders. Their sizes remain lower bounds."),
        Line::from("Optional: allow Full Disk Access for your terminal host."),
        Line::from("System Settings > Privacy & Security > Full Disk Access."),
        Line::from("Enable the app hosting this terminal: Terminal, iTerm, or your editor."),
        Line::from("Restart that host if macOS requests it, then press r to refresh."),
        Line::from("You can keep limited access; review lower-bound and error markers."),
    ]
}

fn help_paragraph(sections: &[&KeySection]) -> Paragraph<'static> {
    Paragraph::new(help_lines(sections)).wrap(Wrap { trim: false })
}

fn help_lines(sections: &[&KeySection]) -> Vec<Line<'static>> {
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

    lines
}

fn render_confirm_modal(f: &mut Frame, area: Rect, title: &str, body: Vec<Line<'_>>) {
    f.render_widget(Clear, area);
    let block = themed_block()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::Red));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let body_area = Rect {
        height: inner.height.saturating_sub(1),
        ..inner
    };
    let footer_area = Rect {
        y: inner.y.saturating_add(body_area.height),
        height: inner.height.saturating_sub(body_area.height),
        ..inner
    };
    f.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }), body_area);
    f.render_widget(
        Paragraph::new("y confirm · n cancel").alignment(Alignment::Center),
        footer_area,
    );
}

fn draw_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(60, 20, 40, 7, f.area());
    let text_width = area.width.saturating_sub(4) as usize;
    let mut body = vec![
        Line::from("Move to Trash:"),
        Line::from(Span::styled(
            truncate(app.pending_delete_name(), text_width),
            Style::default().add_modifier(Modifier::BOLD),
        )),
    ];
    if let Some(path) = app.pending_delete_path() {
        body.push(Line::from(format!(
            "Path: {}",
            truncate_start(&path.display().to_string(), text_width.saturating_sub(6))
        )));
    }
    if let Some(summary) = app.pending_batch_summary() {
        body.push(Line::from(Span::styled(
            truncate(&summary, text_width),
            Style::default().fg(Color::Gray),
        )));
    }
    render_confirm_modal(f, area, "confirm", body);
}

fn draw_empty_trash_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(60, 20, 48, 8, f.area());
    let text_width = area.width.saturating_sub(4) as usize;
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
        Line::from(Span::styled(
            truncate(&format!("Empty Trash{size_note}?"), text_width),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            truncate_start(&path_note, text_width),
            Style::default().fg(Color::Gray),
        )),
        Line::from(Span::styled(
            "already-discarded items are removed permanently",
            Style::default().fg(Color::Gray),
        )),
    ];
    render_confirm_modal(f, area, "empty trash", body);
}

fn draw_uninstall_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(60, 20, 44, 8, f.area());
    let text_width = area.width.saturating_sub(4) as usize;
    let name = app.pending_uninstall_name();
    let mut body = vec![
        Line::from("Uninstall package:"),
        Line::from(Span::styled(
            truncate(name, text_width),
            Style::default().add_modifier(Modifier::BOLD),
        )),
    ];
    if display_width(name) > text_width {
        body.push(Line::from(Span::styled(
            truncate_start(name, text_width),
            Style::default().fg(Color::Gray),
        )));
    }
    if let Some(root) = app.pending_uninstall_root() {
        body.push(Line::from(format!(
            "Root: {}",
            truncate_start(&root.display().to_string(), text_width.saturating_sub(6))
        )));
    }
    render_confirm_modal(f, area, "uninstall", body);
}

fn inventory_scope_lines(app: &App) -> Vec<Line<'static>> {
    let Some(profile) = &app.inventory.profile else {
        return Vec::new();
    };
    let scope = &app.inventory.scope;
    let managers = if scope.managers.is_empty() {
        String::from("none")
    } else {
        scope
            .managers
            .iter()
            .map(|manager| manager.key())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let root = app.inventory.root.as_ref().unwrap_or(&app.cwd);
    let mut lines = vec![
        Line::from(format!("  Profile: {}", terminal_text::sanitize(profile))),
        Line::from(format!(
            "  Scan root: {}",
            terminal_text::sanitize(&root.display().to_string())
        )),
        Line::from(format!("  Managers: {managers}")),
        Line::from(format!(
            "  Projects: {} · depth {}",
            if scope.projects { "on" } else { "off" },
            scope.depth
        )),
    ];
    if scope.excludes.is_empty() {
        lines.push(Line::from("  Exclusions: none"));
    } else {
        for path in &scope.excludes {
            lines.push(Line::from(format!(
                "  Exclude: {}",
                terminal_text::sanitize(&path.display().to_string())
            )));
        }
    }
    lines.push(Line::from(""));
    lines
}

fn draw_pkg_detail(f: &mut Frame, app: &App) {
    let Some((pkg, manager, dep_info)) = app.selected_pkg_detail() else {
        return;
    };

    let area = detail_modal_area(f);

    let mut lines = Vec::new();
    lines.push(Line::from(""));
    lines.extend(inventory_scope_lines(app));

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
    lines.push(Line::from(format!(
        "  Footprint: {}",
        pkg.coverage.summary()
    )));
    if let Some(report) = app
        .pkg_reports
        .iter()
        .find(|report| report.manager == manager)
    {
        lines.push(Line::from(format!("  Inventory: {}", report.state.label())));
        if let Some(environment) = &report.environment {
            lines.push(Line::from(format!(
                "  Manager: {}",
                terminal_text::sanitize(&environment.executable.display().to_string())
            )));
            for root in &environment.package_roots {
                lines.push(Line::from(format!(
                    "  Package root: {}",
                    terminal_text::sanitize(&root.display().to_string())
                )));
            }
        }
        lines.push(Line::from(format!(
            "  Manager footprints: {} unknown · {} partial · {} scoped",
            report.unknown_sizes(),
            report.partial_sizes(),
            report.limited_sizes()
        )));
        if let Some(warning) = &report.warning {
            lines.push(Line::from(Span::styled(
                format!("  Warning: {}", terminal_text::sanitize(warning)),
                Style::default().fg(Color::Yellow),
            )));
        }
    }
    if let Some(warning) = app
        .dep_graph
        .as_ref()
        .and_then(|graph| graph.warning_for(manager))
    {
        lines.push(Line::from(Span::styled(
            format!("  Dependency graph: {}", terminal_text::sanitize(warning)),
            Style::default().fg(Color::Yellow),
        )));
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
        Some(info) if info.evidence == DepEvidence::PartialGraph => {
            lines.push(Line::from(Span::styled(
                "  Usage: incomplete dependency graph; leaf status is unknown",
                Style::default().fg(Color::Yellow),
            )));
            lines.push(Line::from(format!(
                "  Known dependencies: {}",
                info.dependencies.join(", ")
            )));
            lines.push(Line::from(format!(
                "  Known dependents: {}",
                info.dependents.join(", ")
            )));
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
                        format!("  {} ", scan_frame()),
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
            "  x / d",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" uninstall", Style::default().fg(Color::Gray)),
        Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "Esc",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" close", Style::default().fg(Color::Gray)),
    ]));

    let footer = lines.pop().unwrap_or_default();
    lines.pop();
    render_detail_modal(
        f,
        area,
        " package info ",
        Color::Cyan,
        lines,
        footer,
        app.detail_scroll,
    );
}

fn draw_project_dep_detail(f: &mut Frame, app: &App) {
    let Some(dep) = app.selected_project_dep_detail() else {
        return;
    };

    let area = detail_modal_area(f);

    let mut lines = Vec::new();
    lines.push(Line::from(""));
    lines.extend(inventory_scope_lines(app));

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
    for warning in &dep.dep_count.warnings {
        lines.push(Line::from(Span::styled(
            format!("  {}", terminal_text::sanitize(warning)),
            Style::default().fg(Color::Yellow),
        )));
    }

    if let Some(size) = dep.deps_size {
        lines.push(Line::from(vec![
            Span::styled("  Deps size: ", Style::default().fg(Color::DarkGray)),
            Span::styled(size_detail(size), Style::default().fg(Color::Green)),
        ]));
    }
    lines.push(Line::from(format!(
        "  Footprint: {}",
        dep.coverage.summary()
    )));

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

    let footer = lines.pop().unwrap_or_default();
    lines.pop();
    render_detail_modal(
        f,
        area,
        " project deps ",
        Color::Cyan,
        lines,
        footer,
        app.detail_scroll,
    );
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
    terminal_text::truncate_end(s, max)
}

fn truncate_start(s: &str, max: usize) -> String {
    terminal_text::truncate_start(s, max)
}

fn pad_truncated(s: &str, width: usize) -> String {
    let mut out = truncate(s, width);
    let padding = width.saturating_sub(display_width(&out));
    out.extend(std::iter::repeat_n(' ', padding));
    out
}

fn pad_left_truncated(s: &str, width: usize) -> String {
    let out = truncate_start(s, width);
    format!(
        "{}{}",
        " ".repeat(width.saturating_sub(display_width(&out))),
        out
    )
}

fn display_width(s: &str) -> usize {
    terminal_text::display_width(s)
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
                    format!("{} ", scan_frame()),
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
                    format!("{} ", scan_frame()),
                    Style::default().fg(Color::Cyan),
                ));
                spans.push(Span::styled(
                    app.package_progress(),
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
                                .map(|size| {
                                    format!(
                                        "{}{}",
                                        if package.coverage.is_partial() {
                                            "≥"
                                        } else {
                                            ""
                                        },
                                        size_detail(size)
                                    )
                                })
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
                            .map(|size| {
                                format!(
                                    "{}{}",
                                    if dep.coverage.is_partial() { "≥" } else { "" },
                                    size_detail(size)
                                )
                            })
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
    if matches!(
        app.listing_state,
        ListingState::Loading | ListingState::Cancelled
    ) {
        title.push_str(" · partial");
    }
    if app.cwd_enumeration_errors > 0 {
        title.push_str(&format!(" · {} unreadable", app.cwd_enumeration_errors));
    }
    if total_size > 0 {
        title.push_str(&format!(" · {} rows*", human(total_size)));
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

/// A trailing "↑/↓ size" badge relative to the session baseline.
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

fn file_columns_for_row(
    content_width: u16,
    suffix_width: usize,
    show_modified: bool,
) -> (usize, usize, usize, usize) {
    let available = content_width.saturating_sub(suffix_width as u16);
    let (_, mut size, mut modified, mut bar) = file_columns(available, show_modified);
    loop {
        let fixed = 2
            + if size > 0 { 2 + size } else { 0 }
            + if bar > 0 { 2 + bar + 5 } else { 0 }
            + if modified > 0 { 2 + modified } else { 0 };
        let name = (available as usize).saturating_sub(fixed);
        if name >= 8 || size == 0 && modified == 0 && bar == 0 {
            return (name, size, modified, bar);
        }
        if bar > 0 {
            bar = 0;
        } else if modified > 0 {
            modified = 0;
        } else {
            size = 0;
        }
    }
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
        decoration("█", "#").repeat(filled),
        decoration("░", "-").repeat(bar_width.saturating_sub(filled)),
    );
    (bar_text, format!("{percent_of_sum:>3}%"), style)
}

#[allow(clippy::items_after_test_module)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_footers_keep_primary_actions_and_help() {
        use ratatui_core::{backend::TestBackend, terminal::Terminal};
        for width in [40, 80, 120] {
            for focus in [Focus::Files, Focus::Packages, Focus::Reclaim, Focus::Disks] {
                let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
                terminal
                    .draw(|frame| draw_help(frame, frame.area(), focus))
                    .unwrap();
                let text = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(text.contains("? help"), "{width} {focus:?}: {text}");
                assert!(!text.contains("pane/view"));
                match focus {
                    Focus::Files => {
                        for action in ["S scan", ". hidden", "t largest"] {
                            assert!(text.contains(action), "{width}: {text}");
                        }
                        if width >= 80 {
                            assert!(text.contains("g go"));
                            assert!(text.contains("r refresh"));
                        }
                    }
                    Focus::Packages => {
                        assert!(text.contains("u leaves"));
                        assert!(text.contains("p view"));
                        assert!(text.contains("x uninstall"));
                    }
                    Focus::Reclaim => assert!(text.contains("R rescan")),
                    Focus::Disks => assert!(text.contains("open")),
                }
            }
        }
    }

    #[test]
    fn access_warning_survives_long_paths_history_and_entry_errors() {
        use ratatui_core::{backend::TestBackend, terminal::Terminal};
        let root = std::env::temp_dir().join(format!("diskr_access_header_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut app = App::new(root.clone()).unwrap();
        app.fda_limited = true;
        app.cwd_enumeration_errors = 7;
        app.cwd = root.join("long-folder-name/".repeat(20));
        app.history_baseline = Some(crate::history::ScanRecord {
            path: app.cwd.clone(),
            timestamp: 1,
            generation: 1,
            enumeration_errors: 0,
            children: vec![],
        });
        for width in [40, 80, 120] {
            let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
            terminal
                .draw(|frame| draw_header(frame, frame.area(), &app))
                .unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(text.contains("limited access"), "{width}: {text}");
            assert!(text.contains("7 entry errors"), "{width}: {text}");
            if width >= 80 {
                assert!(text.contains("? help"), "{width}: {text}");
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn limited_access_help_explains_optional_host_permission_and_refresh() {
        use ratatui_core::{backend::TestBackend, terminal::Terminal};
        let root = std::env::temp_dir().join(format!("diskr_access_help_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut app = App::new(root.clone()).unwrap();
        app.fda_limited = true;
        app.open_help();
        for width in [80, 120] {
            app.help_scroll = 0;
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal
                .draw(|frame| draw_help_overlay(frame, &mut app))
                .unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            for instruction in [
                "Limited disk access",
                "Full Disk Access",
                "hosting this terminal",
                "r to refresh",
            ] {
                assert!(text.contains(instruction), "{width}: {text}");
            }
            app.help_scroll = usize::MAX;
            terminal
                .draw(|frame| draw_help_overlay(frame, &mut app))
                .unwrap();
            assert!(app.help_scroll > 0);
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(text.contains("Detail drawers"));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn location_chooser_keeps_foreign_scope_and_path_actions_visible() {
        use ratatui_core::{backend::TestBackend, terminal::Terminal};
        let root =
            std::env::temp_dir().join(format!("diskr_locations_render_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut app = App::new(root.clone()).unwrap();
        app.locations.open = true;
        app.locations.entries = vec![crate::locations::LocationEntry {
            label: String::from("work"),
            path: root.join("wide-folder/".repeat(10)),
            profile: Some(String::from("work")),
            origin: String::from("imported from remote-machine"),
            foreign: true,
            scope: Some(crate::packages::InventoryScope::default()),
        }];
        for width in [40, 80, 120] {
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal.draw(|frame| draw_locations(frame, &app)).unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            for expected in [
                "[foreign]",
                "Origin:",
                "Unavailable:",
                "Managers:",
                "Projects on",
                "Enter go",
                "Esc/g close",
            ] {
                assert!(text.contains(expected), "{width}: {text}");
            }
        }
        app.locations.path_input = Some(String::from("~/some/folder/target"));
        app.locations
            .cursor
            .end(app.locations.path_input.as_ref().unwrap());
        app.locations.error = Some(String::from("Folder is unavailable"));
        let mut terminal = Terminal::new(TestBackend::new(40, 24)).unwrap();
        terminal.draw(|frame| draw_locations(frame, &app)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for expected in ["Folder is unavailable", "target|", "Enter go", "Esc back"] {
            assert!(text.contains(expected), "{text}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn location_path_editor_retains_text_and_caret_in_short_terminals() {
        let root = std::env::temp_dir().join(format!(
            "diskr_location_editor_{}",
            crate::state::next_epoch()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut app = App::new(root.clone()).unwrap();
        app.locations.open = true;
        let input = format!("/{}VISIBLE_INPUT", "long folder/".repeat(16));
        app.locations.cursor.end(&input);
        app.locations.path_input = Some(input);
        app.locations.error = Some(String::from("Folder is unavailable"));
        for width in [40, 80, 120] {
            for height in [6, 8, 14, 24] {
                let text = render_app(&mut app, width, height);
                for expected in ["VISIBLE_INPUT|", "Enter go", "Esc back"] {
                    assert!(text.contains(expected), "{width}x{height}: {text}");
                }
                if height >= 8 {
                    assert!(
                        text.contains("Folder is unavailable"),
                        "{width}x{height}: {text}"
                    );
                }
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn active_profile_exposes_exact_inventory_scope_in_details() {
        let root =
            std::env::temp_dir().join(format!("diskr_profile_scope_render_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut app = App::new(root.clone()).unwrap();
        app.inventory.profile = Some(String::from("work"));
        app.inventory.root = Some(root.clone());
        app.inventory.scope = crate::packages::InventoryScope {
            managers: vec![crate::packages::Manager::Cargo],
            projects: true,
            depth: 3,
            excludes: vec![std::path::PathBuf::from("archive/legacy")],
        };
        let text = inventory_scope_lines(&app)
            .into_iter()
            .flat_map(|line| line.spans.into_iter().map(|span| span.content.into_owned()))
            .collect::<String>();
        for expected in [
            "Profile: work",
            "Scan root:",
            "Managers: cargo",
            "Projects: on",
            "depth 3",
            "Exclude: archive/legacy",
        ] {
            assert!(text.contains(expected), "{text}");
        }
        app.focus = Focus::Packages;
        app.full_width = true;
        assert!(render_app(&mut app, 80, 24).contains("profile work"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ascii_borders_leave_unicode_data_intact() {
        use ratatui_core::{backend::TestBackend, terminal::Terminal};
        let mut terminal = Terminal::new(TestBackend::new(30, 5)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new("界▶ filename")
                        .block(block_with_ascii(true).borders(Borders::ALL)),
                    frame.area(),
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.cell((0, 0)).unwrap().symbol(), "+");
        assert_eq!(buffer.cell((0, 1)).unwrap().symbol(), "|");
        let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("界 ▶ filename"));
        assert!(!text.contains('┌'));
    }

    #[test]
    fn full_width_views_hide_inactive_panes_and_top_files_fill_the_browser() {
        use ratatui_core::{backend::TestBackend, terminal::Terminal};
        let root = std::env::temp_dir().join(format!("diskr_full_width_{}", std::process::id()));
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("nested/payload"), b"fixture").unwrap();
        let mut app = App::new(root.clone()).unwrap();
        app.full_width = true;
        for width in [40, 80, 120] {
            app.focus = Focus::Files;
            render_app(&mut app, width, 24);
            assert_eq!(app.files_area.width, width);
            app.focus = Focus::Packages;
            let frame = render_app(&mut app, width, 24);
            assert!(frame.contains("packages"));
            assert!(!frame.contains("files ·"));
            app.focus = Focus::Disks;
            let frame = render_app(&mut app, width, 24);
            assert!(frame.contains("disks"));
            assert!(!frame.contains("files ·"));
        }
        app.focus = Focus::Files;
        app.open_top_files_for_path(root.clone());
        for _ in 0..200 {
            app.drain_scan_results();
            if !app.top_files_loading() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(!app.top_files_loading());
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.cell((0, 1)).unwrap().symbol(), "┌");
        assert_eq!(buffer.cell((119, 1)).unwrap().symbol(), "┐");
        let frame: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(frame.contains("nested/payload"));
        assert!(frame.contains("t/Esc return"));
        app.close_top_files();
        assert!(!app.top_files_visible());
        std::fs::remove_dir_all(root).unwrap();
    }

    struct MeasuredAllocator;
    thread_local! {
        static ALLOCATION_COUNT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    }
    fn record_allocation() {
        let _ = ALLOCATION_COUNT.try_with(|count| {
            if let Some(value) = count.get() {
                count.set(Some(value + 1));
            }
        });
    }
    unsafe impl std::alloc::GlobalAlloc for MeasuredAllocator {
        unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
            record_allocation();
            unsafe { std::alloc::System.alloc(layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
            record_allocation();
            unsafe { std::alloc::System.alloc_zeroed(layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, size: usize) -> *mut u8 {
            record_allocation();
            unsafe { std::alloc::System.realloc(ptr, layout, size) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
            unsafe { std::alloc::System.dealloc(ptr, layout) }
        }
    }
    #[global_allocator]
    static ALLOCATOR: MeasuredAllocator = MeasuredAllocator;

    fn measured<T>(operation: impl FnOnce() -> T) -> (T, std::time::Duration, usize) {
        ALLOCATION_COUNT.with(|count| count.set(Some(0)));
        let start = std::time::Instant::now();
        let result = operation();
        let elapsed = start.elapsed();
        let allocations = ALLOCATION_COUNT.with(|count| count.replace(None).unwrap());
        (result, elapsed, allocations)
    }

    fn package_fixture(app: &mut App, count: usize) {
        app.pkg_reports = vec![crate::packages::ManagerReport {
            manager: crate::packages::Manager::Cargo,
            packages: (0..count)
                .map(|index| crate::packages::Package {
                    name: format!("pkg-{index:05}"),
                    version: String::from("1.0"),
                    size: Some(SizeInfo::new(100, 100)),
                    path: None,
                    metadata_path: None,
                    coverage: Default::default(),
                })
                .collect(),
            total_size: SizeInfo::new(count as u64 * 100, count as u64 * 100),
            available: true,
            warning: None,
            state: crate::packages::ManagerScanState::Complete,
            environment: None,
        }];
        app.packages_loaded = true;
        app.focus = Focus::Packages;
        app.rebuild_flat_packages();
    }

    #[test]
    fn compact_package_titles_keep_totals_and_scope_evidence() {
        use ratatui_core::{backend::TestBackend, terminal::Terminal};
        let root = std::env::temp_dir().join(format!(
            "diskr_compact_packages_{}",
            crate::state::next_epoch()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut app = App::new(root.clone()).unwrap();
        package_fixture(&mut app, 1);
        let size = SizeInfo::new((69 << 30) / 10, (69 << 30) / 10);
        app.pkg_reports[0].packages[0].size = Some(size);
        app.pkg_reports[0].total_size = size;
        app.project_deps = vec![crate::packages::ProjectDeps {
            path: root.clone(),
            manager_label: String::from("cargo"),
            manifest: String::from("Cargo.toml"),
            dep_count: 1usize.into(),
            deps_size: Some(size),
            deps_dir: Some(root.join("target")),
            coverage: Default::default(),
        }];
        for partial in [false, true] {
            app.package_scan_partial = partial;
            app.pkg_show_unused = partial;
            app.inventory.profile = partial.then(|| "long-saved-project-profile".to_owned());
            app.pkg_reports[0].warning = partial.then(|| "partial fixture".to_owned());
            app.project_dep_errors = u32::from(partial);
            for view in [PkgView::SystemManagers, PkgView::ProjectDeps] {
                app.pkg_view = view;
                app.rebuild_flat_packages();
                for width in [28, 44, 80] {
                    let mut terminal = Terminal::new(TestBackend::new(width, 12)).unwrap();
                    terminal
                        .draw(|frame| draw_packages(frame, frame.area(), &mut app))
                        .unwrap();
                    let cells = &terminal.backend().buffer().content;
                    let title: String = cells
                        .iter()
                        .take(usize::from(width))
                        .map(|cell| cell.symbol())
                        .collect();
                    let text: String = cells.iter().map(|cell| cell.symbol()).collect();
                    assert!(title.contains("6.9 GiB known"), "{width}: {title}");
                    if partial {
                        assert!(title.contains('≥'), "{width}: {title}");
                        assert!(text.contains("partial"), "{width}: {text}");
                        assert!(text.contains("profile"), "{width}: {text}");
                    }
                    if width >= 44 {
                        assert!(
                            title.contains(if view == PkgView::SystemManagers {
                                "[ System ]"
                            } else {
                                "[ Projects ]"
                            }),
                            "{width}: {title}"
                        );
                    }
                }
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn package_viewport_keeps_last_selection_visible_and_bounds_allocations() {
        use ratatui_core::{backend::TestBackend, terminal::Terminal};
        let root = std::env::temp_dir().join(format!("diskr_virtual_rows_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut app = App::new(root.clone()).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        package_fixture(&mut app, 100);
        terminal
            .draw(|frame| draw_packages(frame, frame.area(), &mut app))
            .unwrap();
        let (_, _, small_allocations) = measured(|| {
            terminal
                .draw(|frame| draw_packages(frame, frame.area(), &mut app))
                .unwrap()
        });
        package_fixture(&mut app, 10_000);
        app.selected_pkg = 9_999;
        terminal
            .draw(|frame| draw_packages(frame, frame.area(), &mut app))
            .unwrap();
        let (_, _, large_allocations) = measured(|| {
            terminal
                .draw(|frame| draw_packages(frame, frame.area(), &mut app))
                .unwrap()
        });
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("pkg-09999"));
        assert!(!text.contains("pkg-00000"));
        assert!(
            large_allocations <= small_allocations + 20,
            "small {small_allocations}, large {large_allocations}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "manual rendering benchmark; run optimized with --nocapture"]
    fn benchmark_package_viewport_frames() {
        use ratatui_core::{backend::TestBackend, terminal::Terminal};
        let root =
            std::env::temp_dir().join(format!("diskr_frame_benchmark_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("file"), b"fixture").unwrap();
        let mut app = App::new(root.clone()).unwrap();
        for count in [100, 10_000] {
            package_fixture(&mut app, count);
            let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            let mut durations = Vec::new();
            let mut allocations = Vec::new();
            for index in 0..200 {
                app.selected_pkg = index * (count - 1) / 199;
                let (_, duration, allocated) =
                    measured(|| terminal.draw(|frame| draw(frame, &mut app)).unwrap());
                durations.push(duration.as_micros());
                allocations.push(allocated);
            }
            durations.sort_unstable();
            allocations.sort_unstable();
            eprintln!(
                "packages={count} frames=200 frame_p50_us={} frame_p95_us={} allocations_p95={}",
                durations[100], durations[190], allocations[190]
            );
        }
        app.focus = Focus::Files;
        let entry = app.entries[0].clone();
        app.entries = vec![entry; 100_000];
        app.apply_sort();
        let (_, cold, cold_allocations) = measured(|| app.file_aggregates());
        let mut durations = Vec::new();
        let mut allocations = 0;
        for _ in 0..200 {
            let (_, elapsed, allocated) = measured(|| std::hint::black_box(app.file_aggregates()));
            durations.push(elapsed.as_nanos());
            allocations += allocated;
        }
        durations.sort_unstable();
        eprintln!("files=100000 aggregate_cold_us={} cold_allocations={cold_allocations} cached_p95_ns={} cached_allocations={allocations}", cold.as_micros(), durations[190]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn long_inputs_keep_the_caret_visible_in_real_frames() {
        let root =
            std::env::temp_dir().join(format!("diskr_input_viewport_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut app = App::new(root.clone()).unwrap();
        for width in [40, 80, 120] {
            app.request_mkdir();
            for ch in "界e\u{301}".repeat(100).chars() {
                app.input_push(ch);
            }
            assert!(render_app(&mut app, width, 24).contains('|'));
            app.exit_input_mode();
            app.enter_search();
            for ch in "界e\u{301}".repeat(100).chars() {
                app.search_push(ch);
            }
            assert!(render_app(&mut app, width, 24).contains('|'));
            app.clear_search();
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn detail_modal_scrolls_all_rows_without_losing_action_footer() {
        use ratatui_core::{backend::TestBackend, terminal::Terminal};

        for (width, height) in [(80, 16), (80, 24), (120, 30)] {
            for (scroll, expected, hidden) in [
                (0, "snapshot-01", "snapshot-30"),
                (usize::MAX, "snapshot-30", "snapshot-01"),
            ] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| {
                        let area = detail_modal_area(frame);
                        let lines = (1..=30)
                            .map(|index| Line::from(format!("snapshot-{index:02}")))
                            .collect();
                        render_detail_modal(
                            frame,
                            area,
                            " disk details ",
                            Color::DarkGray,
                            lines,
                            Line::from("Esc close · retry discovery"),
                            scroll,
                        );
                    })
                    .unwrap();
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect();
                assert!(text.contains(expected), "{width}x{height} scroll {scroll}");
                assert!(!text.contains(hidden), "{width}x{height} scroll {scroll}");
                assert!(text.contains("Esc close · retry discovery"));
                assert!(text.contains("of 30"));
            }
        }
    }

    #[test]
    fn disk_detail_exposes_snapshot_beyond_old_six_row_cap() {
        use ratatui_core::{backend::TestBackend, terminal::Terminal};

        let root = std::env::temp_dir().join(format!(
            "diskr_snapshot_detail_render_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut app = App::new(root.clone()).unwrap();
        app.disk_info_report = Some(crate::space::SpaceReport {
            path: root.clone(),
            mount: root.clone(),
            device: String::from("disk-test"),
            fs_type: String::from("apfs"),
            total: 100,
            used: 40,
            free: 60,
            available: 60,
            local_snapshots: crate::space::SnapshotListing {
                names: (1..=30)
                    .map(|index| format!("snapshot-{index:02}"))
                    .collect(),
                error: None,
            },
            apfs_container: None,
            apfs_error: None,
        });
        app.detail_scroll = usize::MAX;
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        terminal
            .draw(|frame| draw_disk_info_modal(frame, &app))
            .unwrap();
        let frame: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(frame.contains("snapshot-30"));
        assert!(frame.contains("Esc close"));
        assert!(frame.contains("of "));
        std::fs::remove_dir_all(root).unwrap();
    }

    fn render_app(app: &mut App, width: u16, height: u16) -> String {
        use ratatui_core::{backend::TestBackend, terminal::Terminal};
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn filtered_views_keep_action_failures_visible() {
        let root =
            std::env::temp_dir().join(format!("diskr_feedback_render_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("name"), b"x").unwrap();
        let mut app = App::new(root.clone()).unwrap();
        app.enter_search();
        app.search_push('n');
        app.keep_search();
        for focus in [Focus::Files, Focus::Packages] {
            app.focus = focus;
            if focus == Focus::Packages {
                app.enter_pkg_search();
                app.pkg_search_push('n');
                app.keep_pkg_search();
            }
            app.status = String::from("Trash failed: permission denied");
            for width in [40, 80, 120] {
                let frame = render_app(&mut app, width, 24);
                assert!(frame.contains("Trash failed: permission denied"));
                assert_eq!(app.status, "Trash failed: permission denied");
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn confirmation_rendering_matches_keyboard_modal_priority() {
        let root =
            std::env::temp_dir().join(format!("diskr_modal_priority_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("name"), b"x").unwrap();
        let mut app = App::new(root.clone()).unwrap();
        app.request_delete();
        app.file_info_open = true;
        let frame = render_app(&mut app, 120, 40);
        assert_eq!(app.active_modal(), ActiveModal::Delete);
        assert!(frame.contains("Move to Trash"));
        assert!(!frame.contains("Loading file info"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn long_delete_confirmation_keeps_target_tail_and_actions_visible() {
        let root = std::env::temp_dir().join(format!("diskr_long_confirm_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let name = format!("{}-tail.txt", "long".repeat(30));
        std::fs::write(root.join(&name), b"x").unwrap();
        let mut app = App::new(root.clone()).unwrap();
        app.request_delete();
        for (width, height) in [(40, 16), (80, 24), (120, 30)] {
            let frame = render_app(&mut app, width, height);
            assert!(frame.contains("Move to Trash:"));
            assert!(frame.contains("-tail.txt"));
            assert!(frame.contains("y confirm · n cancel"));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn empty_views_explain_hidden_entries_and_read_failures() {
        let root = std::env::temp_dir().join(format!("diskr_empty_render_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".hidden"), b"x").unwrap();
        let mut app = App::new(root.clone()).unwrap();
        assert!(render_app(&mut app, 80, 24).contains("no visible files"));
        app.cwd_enumeration_errors = 2;
        assert!(render_app(&mut app, 80, 24).contains("2 entries unreadable"));
        app.cwd_enumeration_errors = 0;
        app.listing_state = ListingState::Cancelled;
        assert!(render_app(&mut app, 80, 24).contains("listing cancelled"));
        app.listing_state = ListingState::Unreadable(String::from("cannot read: denied"));
        assert!(render_app(&mut app, 80, 24).contains("cannot read: denied"));
        std::fs::remove_dir_all(root).unwrap();
    }

    fn render_help(width: u16, height: u16, scroll: usize) -> (String, usize) {
        use ratatui_core::{backend::TestBackend, terminal::Terminal};

        let root = std::env::temp_dir().join(format!(
            "diskr_help_render_{}_{}_{}",
            std::process::id(),
            width,
            height
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut app = App::new(root.clone()).unwrap();
        app.open_help();
        app.help_scroll = scroll;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_help_overlay(frame, &mut app))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        let offset = app.help_scroll;
        std::fs::remove_dir_all(root).unwrap();
        (text, offset)
    }

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
    fn help_overlay_scrolls_to_every_section_at_80x24() {
        let (first_page, _) = render_help(80, 24, 0);
        assert!(first_page.contains("keyboard help"));
        assert!(first_page.contains("Global"));

        let (last_page, offset) = render_help(80, 24, usize::MAX);
        assert!(offset > 0, "short help overlay should require scrolling");
        assert!(last_page.contains("Reclaim / Reports"));
        assert!(last_page.contains("scroll"));
    }

    #[test]
    fn help_columns_balance_by_rendered_content() {
        let (left_sections, right_sections) = balanced_help_columns(keymap::HELP_SECTIONS, 48, 48);
        let left = help_rendered_lines(&left_sections, 48);
        let right = help_rendered_lines(&right_sections, 48);
        assert!(
            left.abs_diff(right) <= 8,
            "unbalanced help columns: {left}/{right}"
        );
        let (wide, _) = render_help(120, 30, 0);
        assert!(wide.contains("Files"));
        assert!(
            (0..=30).any(|scroll| render_help(120, 30, scroll).0.contains("Packages")),
            "Packages section should remain reachable after adding more help bindings"
        );
    }

    #[test]
    fn scan_frame_holds_then_advances_and_wraps() {
        // Each frame holds for one interval, advances at the boundary, and the
        // ten-frame beam wraps back to the first frame.
        assert_eq!(scan_frame_at(0), SCAN_FRAMES[0]);
        assert_eq!(scan_frame_at(SCAN_FRAME_MS - 1), SCAN_FRAMES[0]);
        assert_eq!(scan_frame_at(SCAN_FRAME_MS), SCAN_FRAMES[1]);
        assert_eq!(scan_frame_at(SCAN_FRAME_MS * 9), SCAN_FRAMES[9]);
        assert_eq!(scan_frame_at(SCAN_FRAME_MS * 10), SCAN_FRAMES[0]);
    }

    #[test]
    fn scan_frames_are_four_columns_wide() {
        // Every frame must be exactly four display columns so it can prefix a
        // label (or fill the size cell) without shifting alignment.
        for frame in SCAN_FRAMES {
            assert_eq!(display_width(frame), 4, "frame {frame:?} is not 4 columns");
        }
    }

    #[test]
    fn reclaim_detail_uses_singular_and_plural_path_labels() {
        let root = std::env::temp_dir().join(format!(
            "diskr_reclaim_path_label_{}",
            crate::state::next_epoch()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut app = App::new(root.clone()).unwrap();
        app.focus = Focus::Reclaim;
        for count in [1, 2] {
            app.reclaim_report = Some(crate::reclaim::ReclaimReport {
                root: root.clone(),
                findings: vec![crate::reclaim::Finding {
                    label: String::from("Fixture cache"),
                    class: Reclaimability::Safe,
                    note: String::from("Fixture note"),
                    size: SizeInfo::new(100, 100),
                    inaccessible: 0,
                    skipped_mounts: 0,
                    count,
                    paths: vec![root.join("cache"); count],
                    rollup: false,
                }],
                total: SizeInfo::new(100, 100),
                ..Default::default()
            });
            let text = render_app(&mut app, 100, 30);
            assert!(
                text.contains(if count == 1 {
                    "100 B · 1 path"
                } else {
                    "100 B · 2 paths"
                }),
                "{text}"
            );
            assert!(!text.contains("1 paths"), "{text}");
        }
        std::fs::remove_dir_all(root).unwrap();
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
    fn final_file_columns_include_badges_and_bar_percentage() {
        for width in [3_u16, 20, 40, 80, 120] {
            for suffix in [0_usize, 8, 16] {
                for show_modified in [false, true] {
                    let suffix = suffix.min(width.saturating_sub(3) as usize);
                    let (name, size, modified, bar) =
                        file_columns_for_row(width, suffix, show_modified);
                    let used = 2
                        + name
                        + if size > 0 { 2 + size } else { 0 }
                        + if modified > 0 { 2 + modified } else { 0 }
                        + if bar > 0 { 2 + bar + 5 } else { 0 }
                        + suffix;
                    assert!(used <= width as usize, "width {width}, suffix {suffix}");
                }
            }
        }
    }

    #[test]
    fn file_row_keeps_growth_chip_and_percent_at_common_widths() {
        use ratatui_core::{backend::TestBackend, terminal::Terminal};
        let root = std::env::temp_dir().join(format!(
            "diskr_file_row_budget_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        std::fs::create_dir_all(
            root.join(
                "very-long-directory-name-with-unicode-漢字-and-more-than-one-screen-of-text",
            ),
        )
        .unwrap();
        std::fs::write(root.join("package.json"), b"{}").unwrap();
        let mut app = App::new(root.clone()).unwrap();
        let row_index = app
            .entries
            .iter()
            .position(|entry| entry.name == "node_modules")
            .unwrap();
        app.entries[row_index].size = Some(crate::bulkstat::SizeInfo::new(1_000_000, 1_000_000));
        app.entries[row_index].inaccessible = 1;
        app.entries[row_index].skipped_mounts = 1;
        app.entries[row_index].size_delta = Some(500_000);
        app.entries[row_index].scanning = false;
        app.selected = row_index;
        app.toggle_mark();
        assert!(app.reclaim_class(&app.entries[row_index].path).is_some());
        let long_row_index = app
            .entries
            .iter()
            .position(|entry| entry.name.starts_with("very-long-directory"))
            .unwrap();
        app.entries[long_row_index].size = Some(crate::bulkstat::SizeInfo::new(500_000, 500_000));
        app.entries[long_row_index].size_delta = Some(300_000);
        app.entries[long_row_index].scanning = false;
        for (width, height) in [(80_u16, 24_u16), (120, 30)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| draw_files(frame, Rect::new(0, 0, width, height), &mut app))
                .unwrap();
            let get_row = |index: usize| -> String {
                (0..width)
                    .map(|x| {
                        terminal
                            .backend()
                            .buffer()
                            .cell((x, index as u16 + 1))
                            .unwrap()
                            .symbol()
                    })
                    .collect()
            };
            let row = get_row(row_index);
            assert!(row.contains("[build]"), "{row}");
            assert!(row.contains('↑'), "{row}");
            assert!(row.contains('%'), "{row}");
            assert!(row.ends_with('│'), "{row}");
            let long_row = get_row(long_row_index);
            assert!(long_row.contains("very-long-directory"), "{long_row}");
            assert!(long_row.contains('↑'), "{long_row}");
            assert!(long_row.contains('%'), "{long_row}");
            assert!(long_row.ends_with('│'), "{long_row}");
        }
        std::fs::remove_dir_all(root).unwrap();
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
    fn package_rows_fit_cell_budget_with_wide_unicode() {
        for width in [8_u16, 20, 41] {
            let (name_width, size_width) = package_columns(width);
            for label in ["漢字長い名前", "e\u{301}cole", "📦🧩large-package"] {
                let package = package_line_with_version(
                    "brew (cask)",
                    label,
                    "2.0",
                    "1023.9 GiB",
                    PackageUseStatus::Untracked,
                    width,
                );
                assert!(package.width() <= width as usize, "{label} at {width}");
                let project = package_line(label, "1023.9 GiB", name_width, size_width);
                assert!(project.width() <= width as usize, "{label} at {width}");
            }
        }
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

/// Frames of the "scan" spinner preset from the rattles spinner library
/// (github.com/vyfor/rattles, dual MIT/Apache-2.0): a four-cell braille beam
/// that sweeps left to right. Each frame is exactly four display columns wide
/// (braille blanks, U+2800, are one column each), so a frame stands in for a
/// short label prefix without disturbing column alignment.
const SCAN_FRAMES: [&str; 10] = [
    "⠀⠀⠀⠀",
    "⡇⠀⠀⠀",
    "⣿⠀⠀⠀",
    "⢸⡇⠀⠀",
    "⠀⣿⠀⠀",
    "⠀⢸⡇⠀",
    "⠀⠀⣿⠀",
    "⠀⠀⢸⡇",
    "⠀⠀⠀⣿",
    "⠀⠀⠀⢸",
];

/// Milliseconds per scan-spinner frame, matching the rattles "scan" preset.
const SCAN_FRAME_MS: u128 = 70;

/// Pick the scan-spinner frame for an elapsed-millisecond clock value. Kept
/// pure so the cadence and frame order can be tested without the wall clock.
fn scan_frame_at(ms: u128) -> &'static str {
    SCAN_FRAMES[((ms / SCAN_FRAME_MS) % SCAN_FRAMES.len() as u128) as usize]
}

fn scan_frame() -> &'static str {
    let appearance = crate::terminal_backend::Appearance::configured();
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    scan_frame_for_preferences(ms, appearance.reduced_motion, appearance.ascii)
}

fn scan_frame_for_preferences(ms: u128, reduced_motion: bool, ascii: bool) -> &'static str {
    if reduced_motion || ascii {
        "scan"
    } else {
        scan_frame_at(ms)
    }
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
