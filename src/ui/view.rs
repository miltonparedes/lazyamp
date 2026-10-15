//! Rendering. Every draw also records clickable areas in `App::hits`.

use super::{
    amp_version_label, display_path, flag_value_display, App, ConfirmAction, DirPicker, FlagField,
    HitMap, ListHit, LogKind, Overlay, Pane, PickerPurpose, TextInput,
};
use crate::amp::Runner;
use crate::update;
use crossterm::event::KeyCode;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
};
use ratatui::Frame;
use std::path::Path;

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;
const DIM: Color = Color::Gray;
const GOOD: Color = Color::Green;
const WARN: Color = Color::Yellow;
const BAD: Color = Color::Red;
const SELECTED_BG: Color = Color::Indexed(237);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// One footer hint or dialog button: key label, action label, key it sends.
type Hint = (&'static str, &'static str, Option<KeyCode>);

pub(super) fn draw(frame: &mut Frame, app: &App) {
    *app.hits.borrow_mut() = HitMap::default();
    let [header, main, status, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(6),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, header, app);
    draw_main(frame, main, app);
    draw_status(frame, status, app);
    draw_hints(frame, footer, app, &footer_hints(app));

    let area = frame.area();
    match app.overlay {
        Overlay::None => {}
        Overlay::Help => draw_help(frame, area, app),
        Overlay::Log => draw_log(frame, area, app),
        Overlay::Flags => draw_flags(frame, area, app),
        Overlay::EditField => {
            draw_flags(frame, area, app);
            draw_edit(frame, area, app);
        }
        Overlay::Picker => {
            if let Some(picker) = &app.picker {
                draw_picker(frame, area, app, picker);
            }
        }
        Overlay::Confirm => {
            if let Some(action) = &app.confirm {
                draw_confirm(frame, area, app, action);
            }
        }
    }
}

fn spinner(app: &App) -> &'static str {
    let frame = app.started.elapsed().as_millis() / 100;
    SPINNER[frame as usize % SPINNER.len()]
}

fn muted(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(MUTED))
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let mut left = vec![
        Span::styled(
            " lazyamp",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        muted(format!(" {}", update::current_version())),
    ];
    if !app.amp_version.is_empty() {
        left.push(muted(format!(
            "  ·  amp {}",
            amp_version_label(&app.amp_version)
        )));
    }
    frame.render_widget(Paragraph::new(Line::from(left)), area);

    let right = if let Some(tag) = &app.latest_release {
        let text = format!("↑ lazyamp {tag} available · U to update ");
        let width = Span::raw(&text).width() as u16;
        let button = Rect {
            x: area.right().saturating_sub(width),
            width: width.min(area.width),
            ..area
        };
        app.hits
            .borrow_mut()
            .buttons
            .push((button, KeyCode::Char('U')));
        Span::styled(text, Style::default().fg(WARN))
    } else if app.refreshing {
        muted(format!("{} ", spinner(app)))
    } else {
        return;
    };
    frame.render_widget(
        Paragraph::new(Line::from(right)).alignment(Alignment::Right),
        area,
    );
}

fn draw_main(frame: &mut Frame, area: Rect, app: &App) {
    let (list, detail) = if area.width < 72 {
        let [a, b] = Layout::vertical([Constraint::Percentage(40), Constraint::Min(6)]).areas(area);
        (a, b)
    } else {
        let width = pct(area.width, 36).clamp(30, 48);
        let [a, b] =
            Layout::horizontal([Constraint::Length(width), Constraint::Min(30)]).areas(area);
        (a, b)
    };
    draw_runners(frame, list, app);
    draw_detail(frame, detail, app);
}

fn pane_block(title: String, focused: bool) -> Block<'static> {
    let (border, title_style) = if focused {
        (
            Style::default().fg(ACCENT),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )
    } else {
        (Style::default().fg(MUTED), Style::default().fg(DIM))
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(Span::styled(title, title_style))
}

fn highlight(focused: bool) -> Style {
    let style = Style::default().bg(SELECTED_BG);
    if focused {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

/// Status dot for a runner: connected, outdated binary, or only seen locally.
fn runner_dot(runner: &Runner) -> Span<'static> {
    if runner.amp_outdated {
        Span::styled("●", Style::default().fg(WARN))
    } else if runner.from_amp_list {
        Span::styled("●", Style::default().fg(GOOD))
    } else {
        Span::styled("○", Style::default().fg(MUTED))
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn draw_runners(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.pane == Pane::Runners && app.overlay == Overlay::None;
    let title = match app.runners.len() {
        0 => " Runners ".to_string(),
        n => format!(" Runners {n} "),
    };
    let block = pane_block(title, focused);
    let inner = block.inner(area);
    if app.runners.is_empty() {
        let text = if app.refreshing {
            vec![Line::from(""), Line::from(muted("Looking for runners…"))]
        } else {
            vec![
                Line::from(""),
                Line::from("No runners yet"),
                Line::from(""),
                Line::from(vec![
                    muted("Press "),
                    Span::styled(
                        "s",
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    ),
                    muted(" to start one"),
                ]),
            ]
        };
        frame.render_widget(
            Paragraph::new(text)
                .alignment(Alignment::Center)
                .block(block),
            area,
        );
        return;
    }

    // Room after the highlight symbol, keeping one column off the border.
    let width = usize::from(inner.width.saturating_sub(3));
    let items: Vec<ListItem> = app
        .runners
        .iter()
        .map(|runner| {
            let name = runner.display_id();
            let meta = runner_meta(runner, width.saturating_sub(2), Span::raw(name).width());
            let meta_w = Span::raw(&meta).width();
            let name = truncate(name, width.saturating_sub(meta_w + 3).max(4));
            let pad = width.saturating_sub(2 + Span::raw(&name).width() + meta_w);
            ListItem::new(Line::from(vec![
                runner_dot(runner),
                Span::raw(format!(" {name}")),
                Span::raw(" ".repeat(pad)),
                muted(meta),
            ]))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(app.selected_runner));
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .highlight_style(highlight(focused))
            .highlight_symbol(if focused { "› " } else { "  " }),
        area,
        &mut state,
    );
    app.hits.borrow_mut().runners = ListHit {
        area: inner,
        offset: state.offset(),
        len: app.runners.len(),
    };
}

/// Right-hand row details, most important first; parts that would squeeze the
/// name below 10 columns are dropped.
fn runner_meta(runner: &Runner, room: usize, name_w: usize) -> String {
    let mut parts = Vec::new();
    if runner.amp_outdated {
        parts.push("↻".to_string());
    }
    if let Some(n) = runner.threads.filter(|n| *n > 0) {
        parts.push(format!("{n} running"));
    }
    parts.push(plural(runner.dirs.len(), "dir", "dirs"));
    let budget = room.saturating_sub(name_w.min(10) + 1);
    let mut shown: Vec<String> = Vec::new();
    for part in parts {
        let candidate = std::iter::once(part.clone())
            .chain(shown.iter().cloned())
            .collect::<Vec<_>>()
            .join(" · ");
        if Span::raw(&candidate).width() > budget {
            break;
        }
        shown.insert(0, part);
    }
    shown.join(" · ")
}

fn draw_detail(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.pane == Pane::Dirs && app.overlay == Overlay::None;
    let Some(runner) = app.current_runner() else {
        let block = pane_block(" Details ".into(), focused);
        frame.render_widget(
            Paragraph::new(muted(
                "Select a runner to see its directories and activity.",
            ))
            .wrap(Wrap { trim: true })
            .block(block),
            area,
        );
        return;
    };
    let block = pane_block(format!(" {} ", runner.display_id()), focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let inner = Rect {
        x: inner.x + 1,
        width: inner.width.saturating_sub(2),
        ..inner
    };

    let mut summary = vec![Line::from(runner_summary(runner))];
    if runner.amp_outdated {
        summary.push(Line::from(Span::styled(
            "↻ amp was updated; this runner keeps the old version until it restarts. r restarts it now.",
            Style::default().fg(WARN),
        )));
    }
    let summary_h = summary
        .iter()
        .map(|l| (l.width() as u16).div_ceil(inner.width.max(1)).max(1))
        .sum::<u16>();

    let dirs = &runner.dirs;
    let avail = inner.height.saturating_sub(summary_h + 4);
    let dirs_h = (dirs.len().max(1) as u16).min((avail / 2).max(3));
    let [summary_area, _, dirs_head, dirs_area, _, activity_head, activity_area] =
        Layout::vertical([
            Constraint::Length(summary_h),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(dirs_h),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(inner);

    frame.render_widget(
        Paragraph::new(summary).wrap(Wrap { trim: true }),
        summary_area,
    );

    frame.render_widget(
        Paragraph::new(section_heading(
            "Directories",
            &plural(dirs.len(), "served", "served"),
        )),
        dirs_head,
    );
    if dirs.is_empty() {
        frame.render_widget(
            Paragraph::new(muted("None reported. Press a to add one.")),
            dirs_area,
        );
    } else {
        let width = usize::from(dirs_area.width.saturating_sub(4));
        let items: Vec<ListItem> = dirs
            .iter()
            .map(|dir| {
                let primary = runner.cwd.as_deref() == Some(dir.as_path());
                let mark = if primary {
                    Span::styled("★ ", Style::default().fg(WARN))
                } else {
                    Span::raw("  ")
                };
                ListItem::new(Line::from(vec![
                    mark,
                    Span::raw(truncate_start(&display_path(dir), width)),
                ]))
            })
            .collect();
        let mut state = ListState::default().with_selected(focused.then_some(app.selected_dir));
        frame.render_stateful_widget(
            List::new(items)
                .highlight_style(highlight(true))
                .highlight_symbol("› "),
            dirs_area,
            &mut state,
        );
        app.hits.borrow_mut().dirs = ListHit {
            area: dirs_area,
            offset: state.offset(),
            len: dirs.len(),
        };
    }

    let log_name = app
        .runner_log
        .path
        .as_deref()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(section_heading("Activity", &log_name)),
        activity_head,
    );
    let height = usize::from(activity_area.height);
    let width = usize::from(activity_area.width);
    let lines: Vec<Line> = if runner.log_path.is_none() {
        vec![Line::from(muted(
            "No log: lazyamp captures output only for runners it started.",
        ))]
    } else if app.runner_log.lines.is_empty() {
        vec![Line::from(muted("Log is empty."))]
    } else {
        let skip = app.runner_log.lines.len().saturating_sub(height);
        app.runner_log.lines[skip..]
            .iter()
            .map(|line| activity_line(line, width))
            .collect()
    };
    frame.render_widget(Paragraph::new(lines), activity_area);
}

fn runner_summary(runner: &Runner) -> Vec<Span<'static>> {
    let mut spans = vec![runner_dot(runner), Span::raw(" ")];
    if runner.from_amp_list {
        spans.push(Span::raw("Connected"));
    } else {
        spans.push(Span::raw("Local process"));
        spans.push(muted(" (not in amp runner list)".replace(' ', "\u{a0}")));
    }
    if let Some(n) = runner.threads {
        spans.push(muted("  ·  "));
        spans.push(Span::raw(plural(
            n as usize,
            "thread running",
            "threads running",
        )));
    }
    if let Some(pid) = runner.pid {
        spans.push(muted(format!("  ·  pid\u{a0}{pid}")));
    }
    spans
}

fn section_heading(title: &str, detail: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            title.to_string(),
            Style::default().fg(DIM).add_modifier(Modifier::BOLD),
        ),
        muted(format!("  {detail}")),
    ])
}

/// Amp log line, with timestamps dimmed and thread URLs shortened to their ID.
fn activity_line(line: &str, width: usize) -> Line<'static> {
    let line = &line.replace("https://ampcode.com/threads/", "");
    if line.starts_with("T-") && !line.contains(' ') {
        return Line::from(muted(truncate(&format!("  ↳ {line}"), width)));
    }
    if line.starts_with("--- lazyamp") {
        return Line::from(muted(truncate(line.trim_matches('-').trim(), width)));
    }
    let is_time = |s: &str| s.len() == 8 && s.chars().all(|c| c.is_ascii_digit() || c == ':');
    match line.split_once(' ') {
        Some((time, rest)) if is_time(time) => Line::from(vec![
            muted(format!("{} ", &time[..5])),
            Span::raw(truncate(rest, width.saturating_sub(6))),
        ]),
        _ => Line::from(Span::styled(
            truncate(line, width),
            Style::default().fg(DIM),
        )),
    }
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let width = usize::from(area.width.saturating_sub(4));
    let line = if let Some(label) = &app.busy {
        Line::from(Span::styled(
            format!(" {} {}…", spinner(app), truncate(label, width)),
            Style::default().fg(WARN),
        ))
    } else if let Some(toast) = &app.toast {
        let (icon, color) = match toast.kind {
            LogKind::Ok => ("✓", GOOD),
            LogKind::Error => ("✗", BAD),
            LogKind::Info => ("•", DIM),
        };
        Line::from(Span::styled(
            format!(" {icon} {}", truncate(&toast.text, width)),
            Style::default().fg(color),
        ))
    } else {
        return;
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn footer_hints(app: &App) -> Vec<Hint> {
    let enter = Some(KeyCode::Enter);
    let esc = Some(KeyCode::Esc);
    let key = |c| Some(KeyCode::Char(c));
    match app.overlay {
        Overlay::None => match app.pane {
            Pane::Runners => vec![
                ("s", "start", key('s')),
                ("x", "stop", key('x')),
                ("r", "restart", key('r')),
                ("a", "add dir", key('a')),
                ("f", "flags", key('f')),
                ("u", "update amp", key('u')),
                ("L", "log", key('L')),
                ("?", "help", key('?')),
                ("q", "quit", key('q')),
            ],
            Pane::Dirs => vec![
                ("a", "add dir", key('a')),
                ("d", "remove", key('d')),
                ("⏎", "full path", enter),
                ("tab", "runners", Some(KeyCode::Tab)),
                ("?", "help", key('?')),
                ("q", "quit", key('q')),
            ],
        },
        Overlay::Picker => vec![
            ("type", "search", None),
            ("↑↓", "move", None),
            ("←→", "browse", None),
            ("tab", "open folder", Some(KeyCode::Tab)),
            ("⏎", "select", enter),
            ("esc", "close", esc),
        ],
        Overlay::Flags => vec![
            ("↑↓", "move", None),
            ("⏎", "change", enter),
            ("s", "save", key('s')),
            ("esc", "cancel", esc),
        ],
        Overlay::EditField => vec![("⏎", "done", enter), ("esc", "cancel", esc)],
        Overlay::Confirm => vec![("y", "confirm", key('y')), ("n", "cancel", key('n'))],
        Overlay::Help | Overlay::Log => vec![("↑↓", "scroll", None), ("esc", "close", esc)],
    }
}

/// Render hints left to right, dropping any that do not fit; record buttons.
fn draw_hints(frame: &mut Frame, area: Rect, app: &App, hints: &[Hint]) {
    let mut spans = vec![Span::raw(" ")];
    let mut x = area.x + 1;
    let mut hits = app.hits.borrow_mut();
    for (key, label, code) in hints {
        let width = (Span::raw(*key).width() + 1 + Span::raw(*label).width()) as u16;
        if x + width > area.right() {
            break;
        }
        spans.push(Span::styled(
            *key,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        spans.push(muted(format!(" {label}")));
        spans.push(Span::raw("   "));
        if let Some(code) = code {
            hits.buttons.push((
                Rect {
                    x,
                    width,
                    height: 1,
                    ..area
                },
                *code,
            ));
        }
        x += width + 3;
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Centered dialog frame. Returns its inner area.
fn popup(
    frame: &mut Frame,
    area: Rect,
    width: u16,
    height: u16,
    title: &str,
    color: Color,
) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    let rect = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    Rect {
        x: inner.x + 1,
        width: inner.width.saturating_sub(2),
        ..inner
    }
}

fn set_popup(app: &App, rect: Rect) {
    app.hits.borrow_mut().popup = Some(rect);
}

fn pct(total: u16, percent: u16) -> u16 {
    (u32::from(total) * u32::from(percent) / 100) as u16
}

fn help_lines() -> Vec<Line<'static>> {
    let section = |t: &'static str| {
        Line::from(Span::styled(
            t,
            Style::default().fg(DIM).add_modifier(Modifier::BOLD),
        ))
    };
    let row = |k: &'static str, d: &'static str| {
        Line::from(vec![
            Span::styled(
                format!("  {k:<10}"),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(d),
        ])
    };
    vec![
        section("Runners"),
        row("s", "start a runner in a folder you pick"),
        row("x", "stop the selected runner"),
        row("r", "restart it with the settings it started with"),
        row("f", "default flags for new runners"),
        Line::from(""),
        section("Directories"),
        row("a", "add a directory to the selected runner"),
        row("d", "stop serving the selected directory"),
        row("⏎", "show its full path"),
        Line::from(""),
        section("Updates"),
        row("u", "update the Amp CLI; runners pick it up on restart ↻"),
        row("U", "update lazyamp itself; runners keep running"),
        Line::from(""),
        section("Moving around"),
        row("↑↓ jk", "move"),
        row("tab ←→", "switch between runners and directories"),
        row("mouse", "click to select or press a hint; scroll to move"),
        row("g", "refresh now (also refreshes on its own)"),
        row("L", "message history"),
        row("c", "show the config file path"),
        row("q", "quit"),
        Line::from(""),
        section("Folder picker"),
        row("type", "search folders, recent and zoxide paths"),
        row("←→ tab", "browse up / into folders"),
        Line::from(""),
        section("Status"),
        Line::from(vec![
            Span::styled("  ●", Style::default().fg(GOOD)),
            Span::raw(" connected  "),
            Span::styled("●", Style::default().fg(WARN)),
            Span::raw(" restart pending ↻  "),
            Span::styled("○", Style::default().fg(MUTED)),
            Span::raw(" local process only"),
        ]),
        row("★", "primary directory (where the runner started)"),
    ]
}

fn draw_help(frame: &mut Frame, area: Rect, app: &App) {
    let lines = help_lines();
    let height = lines.len() as u16 + 2;
    let inner = popup(frame, area, 68, height, "Help", ACCENT);
    set_popup(app, outer_of(inner));
    frame.render_widget(Paragraph::new(lines).scroll((app.help_scroll, 0)), inner);
}

/// Outer rect of a popup from its padded inner area.
fn outer_of(inner: Rect) -> Rect {
    Rect {
        x: inner.x.saturating_sub(2),
        y: inner.y.saturating_sub(1),
        width: inner.width + 4,
        height: inner.height + 2,
    }
}

fn draw_log(frame: &mut Frame, area: Rect, app: &App) {
    let inner = popup(
        frame,
        area,
        pct(area.width, 86),
        pct(area.height, 80),
        "Messages",
        ACCENT,
    );
    set_popup(app, outer_of(inner));
    let height = usize::from(inner.height);
    let end = app.log.len().saturating_sub(app.log_scroll);
    let start = end.saturating_sub(height);
    let lines: Vec<Line> = app
        .log
        .iter()
        .skip(start)
        .take(end - start)
        .map(|l| {
            let color = match l.kind {
                LogKind::Info => DIM,
                LogKind::Ok => GOOD,
                LogKind::Error => BAD,
            };
            Line::from(vec![
                muted(format!("{}  ", l.time)),
                Span::styled(l.text.clone(), Style::default().fg(color)),
            ])
        })
        .collect();
    let body = if lines.is_empty() {
        Paragraph::new(muted("No messages yet."))
    } else {
        Paragraph::new(lines).wrap(Wrap { trim: false })
    };
    frame.render_widget(body, inner);
}

fn draw_flags(frame: &mut Frame, area: Rect, app: &App) {
    let rows = FlagField::ALL.len() as u16;
    let inner = popup(
        frame,
        area,
        70,
        rows + 5,
        "Defaults for new runners",
        ACCENT,
    );
    set_popup(app, outer_of(inner));
    let [list_area, _, note] = Layout::vertical([
        Constraint::Length(rows),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    let width = usize::from(list_area.width.saturating_sub(2));
    let items: Vec<ListItem> = FlagField::ALL
        .iter()
        .map(|field| {
            let value = flag_value_display(&app.flags, *field);
            let flag = field.label();
            let left = format!("{:<24}{}", field.name(), value);
            let pad = width.saturating_sub(Span::raw(&left).width() + flag.len());
            let unset = value.starts_with('(');
            ListItem::new(Line::from(vec![
                Span::raw(format!("{:<24}", field.name())),
                Span::styled(
                    value,
                    Style::default().fg(if unset { MUTED } else { Color::Reset }),
                ),
                Span::raw(" ".repeat(pad)),
                muted(flag),
            ]))
        })
        .collect();
    let selected = FlagField::ALL.iter().position(|f| *f == app.flag_field);
    let mut state = ListState::default().with_selected(selected);
    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(highlight(true))
            .highlight_symbol("› "),
        list_area,
        &mut state,
    );
    app.hits.borrow_mut().flags = ListHit {
        area: list_area,
        offset: state.offset(),
        len: FlagField::ALL.len(),
    };
    frame.render_widget(
        Paragraph::new(muted(format!(
            "Saved to {}",
            display_path(&app.config_path)
        ))),
        note,
    );
}

fn draw_edit(frame: &mut Frame, area: Rect, app: &App) {
    let inner = popup(frame, area, 64, 3, app.flag_field.name(), WARN);
    set_popup(app, outer_of(inner));
    draw_input(frame, inner, &app.edit, "empty = Amp default");
}

/// Text field with a `›` prompt and the terminal cursor at the edit point.
fn draw_input(frame: &mut Frame, area: Rect, input: &TextInput, placeholder: &str) {
    let room = usize::from(area.width.saturating_sub(3));
    let (start, cursor_x) = input_window(input, room);
    let body = if input.text.is_empty() {
        muted(placeholder.to_string())
    } else {
        Span::raw(input.text[start..].to_string())
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("› ", Style::default().fg(ACCENT)),
            body,
        ])),
        area,
    );
    if area.width > 2 {
        frame.set_cursor_position((area.x + 2 + cursor_x as u16, area.y));
    }
}

/// First visible byte of `input` and the cursor's column within the visible
/// text, scrolling just enough to keep the cursor inside `room` columns.
/// Measured in display width, so wide (CJK) characters stay aligned.
fn input_window(input: &TextInput, room: usize) -> (usize, usize) {
    let before = &input.text[..input.cursor];
    let start = before
        .char_indices()
        .map(|(i, _)| i)
        .find(|&i| Span::raw(&before[i..]).width() <= room)
        .unwrap_or(before.len());
    (start, Span::raw(&before[start..]).width())
}

fn draw_picker(frame: &mut Frame, area: Rect, app: &App, picker: &DirPicker) {
    let title = match &picker.purpose {
        PickerPurpose::Start => "Start a runner · choose its working directory".to_string(),
        PickerPurpose::AddDir(runner) => format!("Add a directory to {}", runner.display_id()),
    };
    let inner = popup(
        frame,
        area,
        pct(area.width, 80).max(60),
        pct(area.height, 80).max(12),
        &title,
        ACCENT,
    );
    set_popup(app, outer_of(inner));
    let [input_area, _, list_area, _, where_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    draw_input(
        frame,
        input_area,
        &picker.input,
        "type to search · ↑↓ move · ←→ browse folders",
    );
    if picker.searching {
        frame.render_widget(
            Paragraph::new(muted(format!("{} searching", spinner(app))))
                .alignment(Alignment::Right),
            input_area,
        );
    }

    if picker.entries.is_empty() {
        frame.render_widget(
            Paragraph::new(muted(if picker.searching {
                ""
            } else {
                "No matches. Press ⏎ to use the typed path."
            })),
            list_area,
        );
    } else {
        let width = usize::from(list_area.width.saturating_sub(12));
        let items: Vec<ListItem> = picker
            .entries
            .iter()
            .map(|e| {
                ListItem::new(Line::from(vec![
                    muted(format!("{:<9}", e.tag)),
                    Span::raw(truncate_start(&e.text, width)),
                ]))
            })
            .collect();
        let cursor = picker.cursor.min(picker.entries.len() - 1);
        let mut state = ListState::default().with_selected(Some(cursor));
        frame.render_stateful_widget(
            List::new(items)
                .highlight_style(highlight(true))
                .highlight_symbol("› "),
            list_area,
            &mut state,
        );
        app.hits.borrow_mut().picker = ListHit {
            area: list_area,
            offset: state.offset(),
            len: picker.entries.len(),
        };
    }
    frame.render_widget(
        Paragraph::new(muted(format!(
            "in {}",
            truncate_start(
                &display_path(&picker.browse_root),
                usize::from(where_area.width.saturating_sub(3))
            )
        ))),
        where_area,
    );
}

fn confirm_text(app: &App, action: &ConfirmAction) -> (&'static str, Color, String) {
    match action {
        ConfirmAction::Stop(runner) => (
            "Stop runner",
            BAD,
            format!(
                "Stop {}? Threads running on it will be interrupted.",
                runner.display_id()
            ),
        ),
        ConfirmAction::Restart(runner) => (
            "Restart runner",
            WARN,
            format!(
                "Restart {} with the settings it was started with? Threads running on it will be interrupted.{}",
                runner.display_id(),
                if runner.amp_outdated {
                    " It comes back on the updated amp."
                } else {
                    ""
                }
            ),
        ),
        ConfirmAction::RemoveDir {
            runner,
            path,
            restart: None,
        } => (
            "Remove directory",
            WARN,
            format!(
                "Stop serving {} from {}? No files are deleted.",
                display_path(path),
                runner.display_id()
            ),
        ),
        ConfirmAction::RemoveDir {
            runner,
            path,
            restart: Some(spec),
        } => (
            "Change primary directory",
            BAD,
            format!(
                "{} is where {} was started. Removing it restarts the runner in {} and interrupts running threads. Other directories and flags are kept. No files are deleted.",
                display_path(path),
                runner.display_id(),
                display_path(&spec.cwd),
            ),
        ),
        ConfirmAction::SelfUpdate => (
            "Update lazyamp",
            ACCENT,
            format!(
                "Install {} over {}? Amp runners keep running.",
                app.latest_release
                    .as_deref()
                    .map_or("the latest release".to_string(), |t| format!("lazyamp {t}")),
                app.exe
                    .as_deref()
                    .map_or("this binary".to_string(), display_path),
            ),
        ),
        ConfirmAction::QuitAfterUpdate(tag) => (
            "lazyamp updated",
            ACCENT,
            format!(
                "lazyamp {tag} is installed. Quit now and start it again to use it? Amp runners keep running."
            ),
        ),
    }
}

fn draw_confirm(frame: &mut Frame, area: Rect, app: &App, action: &ConfirmAction) {
    let (title, color, text) = confirm_text(app, action);
    let width = 64u16.min(area.width.saturating_sub(4));
    let text_lines = (Span::raw(&text).width() as u16).div_ceil(width.saturating_sub(6).max(1));
    let inner = popup(frame, area, width, text_lines + 6, title, color);
    set_popup(app, outer_of(inner));
    let [body, _, buttons] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), body);

    let confirm = " y  Confirm ";
    let cancel = " n  Cancel ";
    let confirm_w = Span::raw(confirm).width() as u16;
    let cancel_w = Span::raw(cancel).width() as u16;
    let x = buttons.right().saturating_sub(confirm_w + cancel_w + 2);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(cancel, Style::default().fg(DIM).bg(SELECTED_BG)),
            Span::raw("  "),
            Span::styled(
                confirm,
                Style::default()
                    .fg(Color::Black)
                    .bg(color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]))
        .alignment(Alignment::Right),
        buttons,
    );
    let mut hits = app.hits.borrow_mut();
    hits.buttons.push((
        Rect {
            x,
            width: cancel_w,
            ..buttons
        },
        KeyCode::Char('n'),
    ));
    hits.buttons.push((
        Rect {
            x: x + cancel_w + 2,
            width: confirm_w,
            ..buttons
        },
        KeyCode::Char('y'),
    ));
}

/// Cut `s` to `max` columns, ending with `…`.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Cut `s` to `max` columns from the left, keeping the end (paths end in the
/// most specific part).
fn truncate_start(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let tail: String = s.chars().skip(count + 1 - max.max(1)).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_window_keeps_wide_characters_aligned_with_the_cursor() {
        // Five 2-column characters, cursor at the end, 7 columns of room:
        // only three fit (6 columns), cursor right after them.
        let input = TextInput::new("界界界界界");
        let (start, x) = input_window(&input, 7);
        assert_eq!(&input.text[start..], "界界界");
        assert_eq!(x, 6);
        // Fits entirely: no scroll.
        let input = TextInput::new("abc");
        assert_eq!(input_window(&input, 7), (0, 3));
        // Cursor at the start of a long line: no scroll either.
        let mut input = TextInput::new("界界界界界");
        input.cursor = 0;
        assert_eq!(input_window(&input, 7), (0, 0));
    }

    #[test]
    fn pct_does_not_overflow_on_wide_terminals() {
        assert_eq!(pct(820, 80), 656);
        assert_eq!(pct(u16::MAX, 100), u16::MAX);
    }

    #[test]
    fn truncate_start_keeps_the_end_of_paths() {
        assert_eq!(truncate_start("~/a/very/long/path", 8), "…ng/path");
        assert_eq!(truncate_start("~/short", 8), "~/short");
        assert_eq!(truncate("box-runner", 5), "box-…");
    }
}
