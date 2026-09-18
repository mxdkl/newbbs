//! The markdown subset: bold, italic, strikethrough, inline code, fenced code
//! blocks and quotes. Deliberately no images, no tables, no embeds -- it all
//! has to render on a plain tty.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::theme::Theme;

/// Render a message body, wrapped to `width` columns.
pub fn render(body: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let width = width.max(8);
    let mut out = Vec::new();
    let mut in_code = false;
    let code_style = Style::default().fg(theme.color(theme.palette.code));
    let quote_style = Style::default()
        .fg(theme.color(theme.palette.quote))
        .add_modifier(Modifier::ITALIC);

    for raw in body.split('\n') {
        let trimmed = raw.trim_end();
        if trimmed.trim_start().starts_with("```") {
            in_code = !in_code;
            // The fence itself renders as a thin rule so the block reads as one.
            out.push(Line::from(Span::styled(
                "\u{2500}".repeat(width.min(24)),
                Style::default().fg(theme.dim()),
            )));
            continue;
        }
        if in_code {
            for chunk in hard_wrap(trimmed, width.saturating_sub(2)) {
                out.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(chunk, code_style),
                ]));
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('>') {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            let spans = inline(rest, theme, quote_style);
            for mut line in wrap(spans, width.saturating_sub(2)) {
                line.spans.insert(
                    0,
                    Span::styled(
                        "\u{2502} ",
                        Style::default().fg(theme.color(theme.palette.quote)),
                    ),
                );
                out.push(line);
            }
            continue;
        }
        if trimmed.is_empty() {
            out.push(Line::default());
            continue;
        }
        let base = Style::default().fg(theme.fg());
        out.extend(wrap(inline(trimmed, theme, base), width));
    }
    if out.is_empty() {
        out.push(Line::default());
    }
    out
}

// ---------------------------------------------------------------------------
// inline markup
// ---------------------------------------------------------------------------

/// Split a single source line into styled runs. Inside backticks nothing else
/// is markup, which is the whole point of inline code.
fn inline(text: &str, theme: &Theme, base: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut bold = false;
    let mut italic = false;
    let mut strike = false;
    let mut code = false;

    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    let style_for = |bold: bool, italic: bool, strike: bool, code: bool| {
        let mut style = if code {
            Style::default().fg(theme.color(theme.palette.code))
        } else {
            base
        };
        if bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if italic {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if strike {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        style
    };

    macro_rules! flush {
        () => {
            if !buf.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut buf),
                    style_for(bold, italic, strike, code),
                ));
            }
        };
    }

    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '\\' if next.is_some() && !code => {
                // Escaped punctuation is literal.
                buf.push(next.unwrap());
                i += 2;
                continue;
            }
            '`' => {
                flush!();
                code = !code;
                i += 1;
                continue;
            }
            _ if code => {
                buf.push(c);
                i += 1;
                continue;
            }
            '*' if next == Some('*') => {
                flush!();
                bold = !bold;
                i += 2;
                continue;
            }
            '~' if next == Some('~') => {
                flush!();
                strike = !strike;
                i += 2;
                continue;
            }
            '*' | '_' => {
                // Only treat it as emphasis when it hugs a word, so
                // snake_case_names and "3 * 4" survive intact.
                let prev = if i == 0 { None } else { chars.get(i - 1).copied() };
                let opens = next.is_some_and(|n| !n.is_whitespace())
                    && prev.is_none_or(|p| !p.is_alphanumeric());
                let closes = italic
                    && prev.is_some_and(|p| !p.is_whitespace())
                    && next.is_none_or(|n| !n.is_alphanumeric());
                if opens || closes {
                    flush!();
                    italic = !italic;
                    i += 1;
                    continue;
                }
                buf.push(c);
                i += 1;
                continue;
            }
            _ => {
                buf.push(c);
                i += 1;
            }
        }
    }
    flush!();
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    spans
}

// ---------------------------------------------------------------------------
// wrapping
// ---------------------------------------------------------------------------

/// Word-wrap a run of styled spans, preserving styles across breaks.
fn wrap(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;

    for span in spans {
        let style = span.style;
        for word in split_keeping_spaces(&span.content) {
            let w = word.width();
            if word.trim().is_empty() {
                // Never start a wrapped line with the space that caused the break.
                if used > 0 && used + w <= width {
                    used += w;
                    current.push(Span::styled(word, style));
                }
                continue;
            }
            if used + w > width {
                if used > 0 {
                    lines.push(Line::from(std::mem::take(&mut current)));
                    used = 0;
                }
                // A single word longer than the line gets split.
                if w > width {
                    for chunk in hard_wrap(&word, width) {
                        if used > 0 {
                            lines.push(Line::from(std::mem::take(&mut current)));
                        }
                        used = chunk.width();
                        current.push(Span::styled(chunk, style));
                    }
                    continue;
                }
            }
            used += w;
            current.push(Span::styled(word, style));
        }
    }
    if !current.is_empty() {
        lines.push(Line::from(current));
    }
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

/// Words and the whitespace between them, as separate pieces.
fn split_keeping_spaces(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut in_space = false;
    for c in text.chars() {
        let is_space = c == ' ' || c == '\t';
        if buf.is_empty() {
            in_space = is_space;
        } else if is_space != in_space {
            out.push(std::mem::take(&mut buf));
            in_space = is_space;
        }
        buf.push(c);
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

/// Split on display width, for words that cannot fit on one line.
fn hard_wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.to_string().width();
        if used + w > width && !buf.is_empty() {
            out.push(std::mem::take(&mut buf));
            used = 0;
        }
        buf.push(c);
        used += w;
    }
    if !buf.is_empty() || out.is_empty() {
        out.push(buf);
    }
    out
}
