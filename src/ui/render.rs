//! Drawing. Three panes, a grouped column message layout, and the overlays.

use chrono::{DateTime, Datelike, Local};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};

use super::theme::Theme;
use super::{App, Mode, Overlay, markdown};
use crate::config;
use crate::model::*;

const SIDEBAR_WIDTH: u16 = 22;
const MEMBERS_WIDTH: u16 = 18;
const TIME_WIDTH: usize = 5;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [body, status] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
    let [sidebar, center, members] = Layout::horizontal([
        Constraint::Length(SIDEBAR_WIDTH),
        Constraint::Min(24),
        Constraint::Length(MEMBERS_WIDTH),
    ])
    .areas(body);

    draw_sidebar(frame, app, sidebar);
    draw_center(frame, app, center);
    draw_members(frame, app, members);
    draw_status(frame, app, status);

    match &app.overlay {
        Some(Overlay::Switcher(_)) => draw_switcher(frame, app),
        Some(Overlay::Profile(user)) => draw_profile(frame, app, user),
        Some(Overlay::Help) => draw_help(frame, app),
        None => {}
    }
}

// ---------------------------------------------------------------------------
// sidebar
// ---------------------------------------------------------------------------

fn draw_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let block = Block::new()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(theme.border(false)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    let section = |lines: &mut Vec<Line>, title: &str| {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(Line::from(Span::styled(
            format!(" {title}"),
            Style::default()
                .fg(theme.dim())
                .add_modifier(Modifier::BOLD),
        )));
    };

    let channels: Vec<usize> = app
        .convs
        .iter()
        .enumerate()
        .filter(|(_, c)| c.kind == ConvKind::Channel)
        .map(|(i, _)| i)
        .collect();
    let directs: Vec<usize> = app
        .convs
        .iter()
        .enumerate()
        .filter(|(_, c)| c.kind != ConvKind::Channel)
        .map(|(i, _)| i)
        .collect();

    if !channels.is_empty() {
        section(&mut lines, "CHANNELS");
        for index in channels {
            lines.push(sidebar_row(app, index, inner.width));
        }
    }
    if !directs.is_empty() {
        section(&mut lines, "DIRECT");
        for index in directs {
            lines.push(sidebar_row(app, index, inner.width));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn sidebar_row(app: &App, index: usize, width: u16) -> Line<'static> {
    let theme = &app.theme;
    let conv = &app.convs[index];
    let selected = index == app.active;
    let label = truncate(app.label(conv.id), width.saturating_sub(2) as usize);
    let style = if selected {
        Style::default()
            .fg(theme.accent())
            .bg(theme.selection())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.fg())
    };
    let padding = (width as usize).saturating_sub(label.chars().count() + 1);
    Line::from(vec![
        Span::styled(" ", style),
        Span::styled(label, style),
        Span::styled(" ".repeat(padding), style),
    ])
}

// ---------------------------------------------------------------------------
// centre: header, messages, input
// ---------------------------------------------------------------------------

fn draw_center(frame: &mut Frame, app: &mut App, area: Rect) {
    let [header, messages, input] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .areas(area);

    draw_header(frame, app, header);
    draw_messages(frame, app, messages);
    draw_input(frame, app, input);
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let mut spans = vec![Span::raw(" ")];
    match app.active_conv() {
        Some(conv) => {
            spans.push(Span::styled(
                app.label(conv.id).to_string(),
                Style::default()
                    .fg(theme.fg())
                    .add_modifier(Modifier::BOLD),
            ));
            if !conv.topic.is_empty() {
                spans.push(Span::styled(
                    format!("  \u{b7}  {}", conv.topic),
                    Style::default().fg(theme.dim()),
                ));
            }
        }
        None => spans.push(Span::styled(
            "no conversations",
            Style::default().fg(theme.dim()),
        )),
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_messages(frame: &mut Frame, app: &mut App, area: Rect) {
    let lines = build_messages(app, area.width as usize);
    app.total_lines = lines.len();
    app.view_height = area.height as usize;

    let height = area.height as usize;
    let start = lines.len().saturating_sub(height + app.scroll);
    let end = (start + height).min(lines.len());
    let mut visible: Vec<Line> = lines[start..end].to_vec();
    // A short conversation sits on the input box, not under the header.
    while visible.len() < height {
        visible.insert(0, Line::default());
    }
    frame.render_widget(Paragraph::new(visible), area);
}

/// The column layout: time and author in fixed columns, a vertical rule, then
/// the wrapped body. Consecutive messages from one author share a header until
/// the gap grows or the day turns over.
fn build_messages(app: &App, width: usize) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let name_width = app
        .messages
        .iter()
        .map(|m| app.user_name(m.author).chars().count())
        .max()
        .unwrap_or(6)
        .clamp(6, 14);
    let gutter = TIME_WIDTH + 2 + name_width + 3;
    let text_width = width.saturating_sub(gutter).max(16);
    let rule_style = Style::default().fg(theme.border(false));
    let dim = Style::default().fg(theme.dim());

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut previous: Option<&Message> = None;

    for message in &app.messages {
        let when = local_time(message.created_at);
        let new_day = previous.is_none_or(|p| local_time(p.created_at).date_naive() != when.date_naive());
        if new_day {
            lines.push(day_separator(&when, width, theme));
        }
        let grouped = !new_day
            && previous.is_some_and(|p| {
                p.author == message.author
                    && message.created_at - p.created_at <= config::GROUP_GAP_MILLIS
            });

        let body = markdown::render(&message.body, text_width, theme);
        for (index, body_line) in body.into_iter().enumerate() {
            let mut spans: Vec<Span<'static>> = Vec::new();
            if index == 0 && !grouped {
                spans.push(Span::styled(when.format("%H:%M").to_string(), dim));
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    pad(&app.user_name(message.author), name_width),
                    Style::default()
                        .fg(app.user_color(message.author))
                        .add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::raw(" ".repeat(TIME_WIDTH + 2 + name_width)));
            }
            spans.push(Span::styled(" \u{2502} ", rule_style));
            spans.extend(body_line.spans);
            if index == 0 && message.edited_at.is_some() {
                spans.push(Span::styled("  (edited)", dim));
            }
            lines.push(Line::from(spans));
        }
        previous = Some(message);
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  nothing here yet -- press i and say something",
            dim,
        )));
    }
    lines
}

fn day_separator(when: &DateTime<Local>, width: usize, theme: &Theme) -> Line<'static> {
    let today = Local::now().date_naive();
    let label = if when.date_naive() == today {
        "today".to_string()
    } else if (today - when.date_naive()).num_days() == 1 {
        "yesterday".to_string()
    } else if when.year() == today.year() {
        when.format("%A, %-d %B").to_string()
    } else {
        when.format("%-d %B %Y").to_string()
    };
    let style = Style::default().fg(theme.dim());
    let rule = Style::default().fg(theme.border(false));
    let side = width.saturating_sub(label.chars().count() + 4) / 2;
    Line::from(vec![
        Span::styled("\u{2500}".repeat(side), rule),
        Span::styled(format!("  {label}  "), style),
        Span::styled("\u{2500}".repeat(side), rule),
    ])
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let block = Block::new()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(theme.border(app.mode == Mode::Insert)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let (prompt, content, prompt_style) = match app.mode {
        Mode::Command => (
            ":",
            app.command.clone(),
            Style::default().fg(theme.color(theme.palette.warn)),
        ),
        Mode::Insert => (
            "\u{203a} ",
            app.input.clone(),
            Style::default().fg(theme.color(theme.palette.ok)),
        ),
        Mode::Normal => (
            "\u{203a} ",
            app.input.clone(),
            Style::default().fg(theme.dim()),
        ),
    };
    let text_style = if app.input.is_empty() && app.mode == Mode::Normal {
        Style::default().fg(theme.dim())
    } else {
        Style::default().fg(theme.fg())
    };
    let shown = if app.mode == Mode::Normal && app.input.is_empty() {
        "press i to write".to_string()
    } else {
        content
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(prompt, prompt_style),
            Span::styled(shown, text_style),
        ])),
        inner,
    );

    // A real caret, so the terminal's own cursor shows where typing lands.
    if app.overlay.is_none() {
        let offset = match app.mode {
            Mode::Insert => 1 + 2 + app.cursor,
            Mode::Command => 1 + 1 + app.command.chars().count(),
            Mode::Normal => return,
        };
        let x = inner.x + (offset as u16).min(inner.width.saturating_sub(1));
        frame.set_cursor_position((x, inner.y));
    }
}

// ---------------------------------------------------------------------------
// members
// ---------------------------------------------------------------------------

fn draw_members(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let block = Block::new()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(theme.border(false)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    let mut listed: Vec<UserId> = Vec::new();

    // Hoisted roles get their own section, highest priority first.
    let mut hoisted: Vec<Role> = Vec::new();
    for id in &app.members {
        if let Some(user) = app.user(*id) {
            for role in &user.roles {
                if role.hoisted && !hoisted.iter().any(|r| r.id == role.id) {
                    hoisted.push(role.clone());
                }
            }
        }
    }
    hoisted.sort_by(|a, b| b.priority.cmp(&a.priority));

    for role in &hoisted {
        let section: Vec<UserId> = app
            .members
            .iter()
            .copied()
            .filter(|id| {
                app.user(*id).is_some_and(|u| {
                    u.online
                        && u.roles
                            .iter()
                            .find(|r| r.hoisted)
                            .is_some_and(|r| r.id == role.id)
                })
            })
            .collect();
        if section.is_empty() {
            continue;
        }
        push_section(&mut lines, &role.name.to_uppercase(), section.len(), theme);
        for id in section {
            lines.push(member_row(app, id));
            listed.push(id);
        }
    }

    let online: Vec<UserId> = app
        .members
        .iter()
        .copied()
        .filter(|id| !listed.contains(id) && app.user(*id).is_some_and(|u| u.online))
        .collect();
    if !online.is_empty() {
        push_section(&mut lines, "ONLINE", online.len(), theme);
        for id in online {
            lines.push(member_row(app, id));
            listed.push(id);
        }
    }

    let offline: Vec<UserId> = app
        .members
        .iter()
        .copied()
        .filter(|id| !listed.contains(id))
        .collect();
    if !offline.is_empty() {
        push_section(&mut lines, "OFFLINE", offline.len(), theme);
        for id in offline {
            lines.push(member_row(app, id));
        }
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

fn push_section(lines: &mut Vec<Line<'static>>, title: &str, count: usize, theme: &Theme) {
    if !lines.is_empty() {
        lines.push(Line::default());
    }
    lines.push(Line::from(Span::styled(
        format!(" {title} \u{2014} {count}"),
        Style::default()
            .fg(theme.dim())
            .add_modifier(Modifier::BOLD),
    )));
}

fn member_row(app: &App, id: UserId) -> Line<'static> {
    let online = app.user(id).is_some_and(|u| u.online);
    let name = app.user_name(id);
    let style = if online {
        Style::default().fg(app.user_color(id))
    } else {
        Style::default().fg(app.theme.dim())
    };
    Line::from(vec![
        Span::raw(" "),
        Span::styled(if online { "\u{25cf} " } else { "\u{25cb} " }, style),
        Span::styled(truncate(&name, MEMBERS_WIDTH as usize - 4), style),
    ])
}

// ---------------------------------------------------------------------------
// status line
// ---------------------------------------------------------------------------

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let (label, color) = match app.mode {
        Mode::Normal => (" NORMAL ", theme.accent()),
        Mode::Insert => (" INSERT ", theme.color(theme.palette.ok)),
        Mode::Command => (" COMMAND ", theme.color(theme.palette.warn)),
    };
    let mut spans = vec![Span::styled(
        label,
        Style::default()
            .fg(theme.color(0x1b1d23))
            .bg(color)
            .add_modifier(Modifier::BOLD),
    )];
    let online = app
        .members
        .iter()
        .filter(|id| app.user(**id).is_some_and(|u| u.online))
        .count();
    spans.push(Span::styled(
        format!(
            "  {}  \u{b7}  {online} online",
            app.active_conv()
                .map(|c| app.label(c.id).to_string())
                .unwrap_or_default()
        ),
        Style::default().fg(theme.dim()),
    ));
    if let Some(status) = &app.status {
        spans.push(Span::styled(
            format!("  \u{b7}  {status}"),
            Style::default().fg(theme.color(theme.palette.warn)),
        ));
    } else if app.scroll > 0 {
        spans.push(Span::styled(
            format!(
                "  \u{b7}  scrolled {} line{}",
                app.scroll,
                if app.scroll == 1 { "" } else { "s" }
            ),
            Style::default().fg(theme.dim()),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ---------------------------------------------------------------------------
// overlays
// ---------------------------------------------------------------------------

fn draw_switcher(frame: &mut Frame, app: &App) {
    let Some(Overlay::Switcher(state)) = &app.overlay else {
        return;
    };
    let theme = &app.theme;
    let area = centered(frame.area(), 54, 16);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent()))
        .padding(Padding::horizontal(1))
        .title(" jump to ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [query_area, list_area] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(inner);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("/", Style::default().fg(theme.accent())),
            Span::styled(state.query.clone(), Style::default().fg(theme.fg())),
        ])),
        query_area,
    );

    let mut lines: Vec<Line> = Vec::new();
    for (row, (index, positions)) in state.matches.iter().enumerate() {
        let conv = &app.convs[*index];
        let label = app.label(conv.id);
        let selected = row == state.selected;
        let base = if selected {
            Style::default().fg(theme.fg()).bg(theme.selection())
        } else {
            Style::default().fg(theme.fg())
        };
        let hit = base
            .fg(theme.accent())
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
        let mut spans = vec![Span::styled(if selected { " > " } else { "   " }, base)];
        for (i, c) in label.chars().enumerate() {
            spans.push(Span::styled(
                c.to_string(),
                if positions.contains(&i) { hit } else { base },
            ));
        }
        lines.push(Line::from(spans));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "   no match",
            Style::default().fg(theme.dim()),
        )));
    }
    frame.render_widget(Paragraph::new(lines), list_area);
}

fn draw_profile(frame: &mut Frame, app: &App, user: &User) {
    let theme = &app.theme;
    let area = centered(frame.area(), 52, 14);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent()))
        .padding(Padding::horizontal(1))
        .title(" profile ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let name_color = user
        .roles
        .first()
        .map(|r| theme.color(r.color))
        .unwrap_or(theme.fg());
    let mut lines = vec![
        Line::from(Span::styled(
            user.name.clone(),
            Style::default()
                .fg(name_color)
                .add_modifier(Modifier::BOLD),
        )),
        Line::default(),
    ];

    if !user.roles.is_empty() {
        let mut spans = Vec::new();
        for role in &user.roles {
            spans.push(Span::styled(
                format!("{} ", role.name),
                Style::default().fg(theme.color(role.color)),
            ));
        }
        lines.push(Line::from(spans));
        lines.push(Line::default());
    }

    if user.bio.is_empty() {
        lines.push(Line::from(Span::styled(
            "no bio yet",
            Style::default().fg(theme.dim()),
        )));
    } else {
        lines.extend(markdown::render(
            &user.bio,
            inner.width as usize,
            &app.theme,
        ));
    }
    lines.push(Line::default());

    let field = Style::default().fg(theme.dim());
    lines.push(Line::from(vec![
        Span::styled("joined   ", field),
        Span::styled(
            local_time(user.joined_at).format("%-d %B %Y").to_string(),
            Style::default().fg(theme.fg()),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("status   ", field),
        if user.online {
            Span::styled(
                "\u{25cf} online",
                Style::default().fg(theme.color(theme.palette.ok)),
            )
        } else {
            Span::styled(
                format!(
                    "\u{25cb} last seen {}",
                    local_time(user.last_seen).format("%-d %b %H:%M")
                ),
                Style::default().fg(theme.dim()),
            )
        },
    ]));

    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_help(frame: &mut Frame, app: &App) {
    let theme = &app.theme;
    let area = centered(frame.area(), 62, 22);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent()))
        .padding(Padding::horizontal(1))
        .title(" keys ")
        .title_alignment(Alignment::Left);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let key = Style::default()
        .fg(theme.accent())
        .add_modifier(Modifier::BOLD);
    let text = Style::default().fg(theme.fg());
    let head = Style::default()
        .fg(theme.dim())
        .add_modifier(Modifier::BOLD);
    let row = |k: &str, v: &str| {
        Line::from(vec![
            Span::styled(format!("  {k:<10}"), key),
            Span::styled(v.to_string(), text),
        ])
    };

    let mut lines = vec![Line::from(Span::styled("NORMAL", head))];
    lines.push(row("i / a", "insert mode (write a message)"));
    lines.push(row(":", "command mode"));
    lines.push(row("j / k", "scroll messages"));
    lines.push(row("gg / G", "top / bottom"));
    lines.push(row("/", "jump to a channel, group or DM"));
    lines.push(row("esc", "back to the bottom"));
    lines.push(Line::default());
    lines.push(Line::from(Span::styled("INSERT", head)));
    lines.push(row("enter", "send, and back to normal"));
    lines.push(row("esc", "normal mode"));
    lines.push(row("ctrl-w", "delete the last word"));
    lines.push(Line::default());
    lines.push(Line::from(Span::styled("COMMANDS", head)));
    lines.push(row(":help", ":q  :reload  :theme <name>"));
    lines.push(row(":info", ":nick <name>  :bio <text>"));
    lines.push(row(":dm", ":group <name> <user...>  :join #chan"));
    lines.push(row(":topic", ":mkchan  :rmchan  (admin)"));
    lines.push(row(":mkrole", ":grant <user> <role>  :revoke  (admin)"));

    frame.render_widget(Paragraph::new(lines), inner);
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

fn local_time(at: Millis) -> DateTime<Local> {
    DateTime::from_timestamp_millis(at)
        .unwrap_or_default()
        .with_timezone(&Local)
}

fn pad(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count >= width {
        text.chars().take(width).collect()
    } else {
        format!("{text}{}", " ".repeat(width - count))
    }
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
        out.push('\u{2026}');
        out
    }
}
