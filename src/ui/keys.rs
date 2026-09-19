//! Modal key handling: Normal to move, Insert to talk, Command for verbs.
//!
//! The keymap is deliberately small -- `:help` fits on one screen.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::{App, Mode, Overlay, command, switcher::Switcher};
use crate::bus::Subscription;

pub async fn handle(app: &mut App, key: KeyEvent, sub: &Subscription) -> Result<()> {
    // Windows sends key-release events too; only act on presses.
    if key.kind == KeyEventKind::Release {
        return Ok(());
    }
    // Always available, in every mode, so the session can never be trapped.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.quit();
        return Ok(());
    }
    app.status = None;

    if app.overlay.is_some() {
        return overlay(app, key, sub).await;
    }
    match app.mode {
        Mode::Normal => normal(app, key, sub).await,
        Mode::Insert => insert(app, key).await,
        Mode::Command => command_line(app, key, sub).await,
    }
}

// ---------------------------------------------------------------------------
// normal
// ---------------------------------------------------------------------------

async fn normal(app: &mut App, key: KeyEvent, _sub: &Subscription) -> Result<()> {
    // `gg` is the only two-key sequence in the map.
    if let Some('g') = app.pending {
        app.pending = None;
        if key.code == KeyCode::Char('g') {
            // `gg` means the top of what is loaded; fetch more first so it
            // reaches the real beginning rather than the end of the buffer.
            app.load_older().await?;
            app.scroll = app.total_lines.saturating_sub(app.view_height);
            return Ok(());
        }
    }

    let page = app.view_height.saturating_sub(1).max(1);
    match key.code {
        KeyCode::Char('i') | KeyCode::Char('a') => {
            app.mode = Mode::Insert;
            if key.code == KeyCode::Char('a') {
                app.cursor = app.input.chars().count();
            }
        }
        KeyCode::Char(':') => {
            app.mode = Mode::Command;
            app.command.clear();
        }
        KeyCode::Char('/') => {
            app.overlay = Some(Overlay::Switcher(Switcher::new(app)));
        }
        KeyCode::Char('g') => app.pending = Some('g'),
        KeyCode::Char('G') => app.scroll = 0,
        KeyCode::Char('j') | KeyCode::Down => app.scroll = app.scroll.saturating_sub(1),
        KeyCode::Char('k') | KeyCode::Up => scroll_up(app, 1).await?,
        KeyCode::PageDown => app.scroll = app.scroll.saturating_sub(page),
        KeyCode::PageUp => scroll_up(app, page).await?,
        KeyCode::Esc => {
            app.pending = None;
            app.scroll = 0;
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// insert
// ---------------------------------------------------------------------------

async fn insert(app: &mut App, key: KeyEvent) -> Result<()> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => app.mode = Mode::Normal,
        KeyCode::Enter => {
            app.send_message().await?;
            // Sending drops you back to Normal, so j/k work immediately.
            app.mode = Mode::Normal;
        }
        KeyCode::Char('u') if ctrl => {
            app.input.clear();
            app.cursor = 0;
        }
        KeyCode::Char('w') if ctrl => delete_word(app),
        KeyCode::Char(c) => {
            let at = byte_index(&app.input, app.cursor);
            app.input.insert(at, c);
            app.cursor += 1;
        }
        KeyCode::Backspace => {
            if app.cursor > 0 {
                let start = byte_index(&app.input, app.cursor - 1);
                let end = byte_index(&app.input, app.cursor);
                app.input.replace_range(start..end, "");
                app.cursor -= 1;
            }
        }
        KeyCode::Delete => {
            let count = app.input.chars().count();
            if app.cursor < count {
                let start = byte_index(&app.input, app.cursor);
                let end = byte_index(&app.input, app.cursor + 1);
                app.input.replace_range(start..end, "");
            }
        }
        KeyCode::Left => app.cursor = app.cursor.saturating_sub(1),
        KeyCode::Right => app.cursor = (app.cursor + 1).min(app.input.chars().count()),
        KeyCode::Home => app.cursor = 0,
        KeyCode::End => app.cursor = app.input.chars().count(),
        _ => {}
    }
    Ok(())
}

/// Scroll back, loading older history when the view reaches the top of what
/// is in memory. The renderer clamps and anchors afterwards.
async fn scroll_up(app: &mut App, lines: usize) -> Result<()> {
    let top = app.total_lines.saturating_sub(app.view_height);
    if app.scroll + lines >= top {
        app.load_older().await?;
    }
    app.scroll += lines;
    Ok(())
}

fn delete_word(app: &mut App) {
    let chars: Vec<char> = app.input.chars().collect();
    let mut at = app.cursor;
    while at > 0 && chars[at - 1].is_whitespace() {
        at -= 1;
    }
    while at > 0 && !chars[at - 1].is_whitespace() {
        at -= 1;
    }
    let start = byte_index(&app.input, at);
    let end = byte_index(&app.input, app.cursor);
    app.input.replace_range(start..end, "");
    app.cursor = at;
}

/// Character index to byte index, so multi-byte input edits safely.
fn byte_index(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}

// ---------------------------------------------------------------------------
// command line
// ---------------------------------------------------------------------------

async fn command_line(app: &mut App, key: KeyEvent, sub: &Subscription) -> Result<()> {
    match key.code {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            app.command.clear();
        }
        KeyCode::Enter => {
            let line = std::mem::take(&mut app.command);
            app.mode = Mode::Normal;
            // A command that fails is the user's problem to see, not a reason
            // to tear the session down.
            if let Err(err) = command::run(app, &line, sub).await {
                tracing::error!(%err, command = %line, "command failed");
                app.set_error(format!("{err}"));
            }
        }
        KeyCode::Backspace => {
            if app.command.pop().is_none() {
                app.mode = Mode::Normal;
            }
        }
        KeyCode::Char(c) => app.command.push(c),
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// overlays
// ---------------------------------------------------------------------------

async fn overlay(app: &mut App, key: KeyEvent, sub: &Subscription) -> Result<()> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match app.overlay.as_mut() {
        Some(Overlay::Switcher(state)) => match key.code {
            KeyCode::Esc => app.overlay = None,
            KeyCode::Enter => {
                let chosen = state.selection();
                app.overlay = None;
                if let Some(index) = chosen {
                    app.select(index, sub).await?;
                }
            }
            KeyCode::Up => state.up(),
            KeyCode::Down => state.down(),
            KeyCode::Char('p') if ctrl => state.up(),
            KeyCode::Char('n') if ctrl => state.down(),
            KeyCode::Backspace => {
                state.query.pop();
                state.refilter(&app.convs, &app.labels);
            }
            KeyCode::Char(c) => {
                state.query.push(c);
                state.refilter(&app.convs, &app.labels);
            }
            _ => {}
        },
        Some(Overlay::Profile(_)) | Some(Overlay::Roles(_)) | Some(Overlay::Help) => match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => app.overlay = None,
            _ => {}
        },
        None => {}
    }
    Ok(())
}
