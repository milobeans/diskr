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

use crate::app::{human, size_sort_key, App, Focus, PkgView};
use crate::bulkstat::SizeInfo;

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

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(root[1]);

    let packages_visible =
        app.focus == Focus::Packages || app.packages_loaded || app.packages_loading;
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
    draw_disks(f, side[0], app);
    draw_packages(f, side[1], app);

    draw_status(f, root[2], app);
    draw_help(f, root[3]);

    if app.confirming_delete {
        draw_confirm(f, app);
    }
    if app.confirming_uninstall {
        draw_uninstall_confirm(f, app);
    }
    if app.pkg_detail {
        draw_pkg_detail(f, app);
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let path = truncate_start(
        &app.cwd.display().to_string(),
        area.width.saturating_sub(30) as usize,
    );
    let text = Line::from(vec![
        Span::styled("diskr", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" · "),
        Span::styled(path, Style::default().fg(Color::Cyan)),
        Span::raw(" · "),
        Span::styled(
            format!("{} items", app.entries.len()),
            Style::default().fg(Color::White),
        ),
        Span::raw(" · "),
        Span::styled(
            format!("sort {}", app.sort.label()),
            Style::default().fg(Color::Gray),
        ),
        Span::raw(" · "),
        Span::styled(
            format!("hidden {}", if app.show_hidden { "on" } else { "off" }),
            Style::default().fg(Color::Gray),
        ),
    ]);
    f.render_widget(Paragraph::new(text), area);
}

fn draw_files(f: &mut Frame, area: Rect, app: &mut App) {
    app.files_area = area;
    let border_color = if app.focus == Focus::Files {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    let (name_width, size_width) = file_columns(area.width.saturating_sub(2));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("files ({})", app.entries.len()))
        .border_style(Style::default().fg(border_color));
    let visible_count = app.visible_entry_count();
    if visible_count == 0 {
        let message = if app.entries.is_empty() {
            "empty directory"
        } else {
            "no matching entries"
        };
        let empty = Paragraph::new(message)
            .block(block)
            .alignment(Alignment::Center);
        f.render_widget(empty, area);
        return;
    }
    let max_rows = area.height.saturating_sub(2).max(1) as usize;
    let (offset, end) =
        file_window_bounds(app.selected, visible_count, app.file_list_offset, max_rows);
    app.file_list_offset = offset;

    let items: Vec<ListItem> = (offset..end)
        .filter_map(|visible_index| app.visible_entry(visible_index))
        .map(|e| {
            let size_str = match (e.is_dir, e.size, e.scanning) {
                (true, _, true) => format!("{} scanning…", spinner_char()),
                (true, Some(size), _) => human(size_sort_key(size)),
                (true, None, _) => String::from("—"),
                (false, Some(size), _) if e.is_symlink => human(size_sort_key(size)),
                (false, None, _) if e.is_symlink => String::from("link"),
                (false, Some(size), _) => human(size_sort_key(size)),
                (false, None, _) => String::from("?"),
            };
            let icon = if e.is_dir {
                "▸ "
            } else if e.is_symlink {
                "↪ "
            } else {
                "  "
            };
            let name_style = if e.is_dir {
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let mut spans = vec![
                Span::styled(icon, Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!(
                        "{:<width$}",
                        truncate(&e.name, name_width),
                        width = name_width
                    ),
                    name_style,
                ),
            ];
            if size_width > 0 {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    format!("{size_str:>size_width$}"),
                    Style::default().fg(Color::Green),
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

fn draw_disks(f: &mut Frame, area: Rect, app: &App) {
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

fn draw_packages(f: &mut Frame, area: Rect, app: &App) {
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
            Span::styled(" [removable]", Style::default().fg(Color::Magenta))
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
                        format!("{} ", spinner_char()),
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

    let item_count = app.pkg_item_count();
    let inner_width = area.width.saturating_sub(2);

    if item_count == 0 {
        let message = if app.pkg_search_mode && !app.pkg_search_query.is_empty() {
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

    let items: Vec<ListItem> = (0..item_count)
        .filter_map(|visible_i| {
            let real_i = app.pkg_visible_index(visible_i)?;
            match app.pkg_view {
                PkgView::SystemManagers => {
                    let (package, manager) = app.flat_packages().get(real_i)?;
                    let size = package
                        .size
                        .map(|s| human(s.allocated))
                        .unwrap_or_else(|| String::from("?"));
                    let removable = app
                        .dep_graph
                        .as_ref()
                        .map(|g| g.is_removable(*manager, &package.name));
                    Some(package_line_with_version(
                        manager.label(),
                        &package.name,
                        &package.version,
                        &size,
                        removable,
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
    removable: Option<bool>,
    inner_width: u16,
) -> ListItem<'static> {
    let (name_width, size_width) = package_columns(inner_width);
    let ver_display = if version.is_empty() {
        String::new()
    } else {
        format!("@{version}")
    };
    let dep_indicator = match removable {
        Some(true) => "  ",
        Some(false) => "* ",
        None => "  ",
    };
    let mgr_label = format!("{manager} ");
    let name_budget = name_width
        .saturating_sub(mgr_label.len())
        .saturating_sub(dep_indicator.len());
    let name_ver = format!("{name}{ver_display}");
    let name_truncated = truncate(&name_ver, name_budget);
    let padded_name = format!("{name_truncated:<width$}", width = name_budget);

    let dep_style = match removable {
        Some(false) => Style::default().fg(Color::DarkGray),
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
    let mut spans = selection_status(app);
    if !app.status.is_empty() {
        spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(&app.status, Style::default().fg(Color::Gray)));
    }
    let text = Line::from(spans);
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
        key("Pg/Home"),
        label(" jump"),
        sep(),
        key("←→/hl"),
        label(" pane/view"),
        sep(),
        key("/"),
        label(" search"),
        sep(),
        key("⏎"),
        label(" open"),
        sep(),
        key("⌫"),
        label(" up"),
        sep(),
        key("Space"),
        label(" preview"),
        sep(),
        key("f"),
        label(" finder"),
        sep(),
        key("O"),
        label(" open"),
        sep(),
        key("r"),
        label(" refresh"),
        sep(),
        key("o"),
        label(" sort"),
        sep(),
        key("p"),
        label(" packages"),
        sep(),
        key("."),
        label(" hidden"),
        sep(),
        key("d"),
        label(" trash"),
        sep(),
        key("i"),
        label(" info"),
        sep(),
        key("u"),
        label(" unused"),
        sep(),
        key("x"),
        label(" uninstall"),
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
    let area = centered_rect(60, 20, 40, 6, f.area());
    f.render_widget(Clear, area);
    let name = app.pending_delete_name();
    let body = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("Move to Trash: {name}"),
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

    lines.push(Line::from(""));

    match dep_info {
        Some(info) => {
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
                String::from("none (removable)")
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
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn truncate_start(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let len = s.chars().count();
    if len <= max {
        return s.to_string();
    }
    if max == 1 {
        return String::from("…");
    }
    let tail: String = s.chars().skip(len.saturating_sub(max - 1)).collect();
    format!("…{tail}")
}

fn selection_status(app: &App) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    match app.focus {
        Focus::Files => match app.entries.get(app.selected) {
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
                    .map(size_detail)
                    .unwrap_or_else(|| String::from("—"));
                spans.push(Span::styled(size, Style::default().fg(Color::Green)));
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
                let label = if disk.name.is_empty() {
                    disk.mount.display().to_string()
                } else {
                    disk.name.clone()
                };
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
        return (side_height, 0);
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

fn disk_label(disk: &crate::app::DiskInfo) -> String {
    if disk.name.is_empty() {
        disk.mount.display().to_string()
    } else {
        format!("{}  {}", disk.name, disk.mount.display())
    }
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

fn file_columns(inner_width: u16) -> (usize, usize) {
    const ICON_WIDTH: usize = 2;
    const GAP_WIDTH: usize = 2;
    const MIN_NAME_WIDTH: usize = 8;
    const MIN_SIZE_WIDTH: usize = 4;
    const PREFERRED_SIZE_WIDTH: usize = 12;

    let inner_width = inner_width as usize;
    let bare_name_width = inner_width.saturating_sub(ICON_WIDTH).max(1);
    let room_for_size = inner_width.saturating_sub(ICON_WIDTH + GAP_WIDTH + MIN_NAME_WIDTH);
    if room_for_size < MIN_SIZE_WIDTH {
        return (bare_name_width, 0);
    }

    let size_width = room_for_size.min(PREFERRED_SIZE_WIDTH);
    let name_width = inner_width
        .saturating_sub(ICON_WIDTH + GAP_WIDTH + size_width)
        .max(1);
    (name_width, size_width)
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
    fn file_columns_hide_size_on_narrow_widths() {
        assert_eq!(file_columns(10), (8, 0));
    }

    #[test]
    fn file_columns_keep_size_when_space_allows() {
        assert_eq!(file_columns(30), (14, 12));
    }

    #[test]
    fn package_columns_hide_size_when_narrow() {
        assert_eq!(package_columns(12), (12, 0));
    }

    #[test]
    fn side_panel_keeps_room_for_packages() {
        assert_eq!(side_panel_heights(21, 12, true), (14, 7));
    }

    #[test]
    fn disk_window_tracks_selected_disk() {
        assert_eq!(disk_window_bounds(0, 12, 3), (0, 3));
        assert_eq!(disk_window_bounds(8, 12, 3), (7, 10));
        assert_eq!(disk_window_bounds(11, 12, 3), (9, 12));
    }

    #[test]
    fn file_window_bounds_scrolls_to_keep_selection_visible() {
        assert_eq!(file_window_bounds(0, 100, 0, 10), (0, 10));
        assert_eq!(file_window_bounds(9, 100, 0, 10), (0, 10));
        assert_eq!(file_window_bounds(10, 100, 0, 10), (1, 11));
        assert_eq!(file_window_bounds(50, 100, 40, 10), (41, 51));
    }
}

fn spinner_char() -> char {
    let spinners = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let index = ((ms / 80) % spinners.len() as u128) as usize;
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
