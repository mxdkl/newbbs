//! Reading ANSI art: SGR colour codes into styled ratatui lines.
//!
//! Art brings its own palette, so unlike the rest of the UI these colours are
//! passed through exactly as authored rather than degraded through the theme
//! -- the file says colour 96, the terminal gets colour 96.
//!
//! Only SGR (`ESC[...m`) is understood. Cursor-movement codes, which classic
//! CP437 `.ANS` files use for positioning, are skipped rather than drawn; that
//! format needs transcoding anyway and is a separate piece of work.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

pub struct Art {
    pub lines: Vec<Line<'static>>,
    /// Display width of the widest line, ignoring escape sequences.
    pub width: usize,
    /// Whether the source actually carried colour codes.
    pub colored: bool,
}

/// Parse art. A file with no escape codes is plain art and takes `plain`, so
/// an uncoloured banner still follows the theme.
pub fn parse(text: &str, plain: Style) -> Art {
    let colored = text.contains('\u{1b}');
    let mut lines = Vec::new();
    let mut width = 0;

    for raw in text.lines() {
        let (spans, line_width) = if colored {
            parse_line(raw)
        } else {
            (
                vec![Span::styled(raw.to_string(), plain)],
                raw.width(),
            )
        };
        width = width.max(line_width);
        lines.push(Line::from(spans));
    }
    Art {
        lines,
        width,
        colored,
    }
}

fn parse_line(raw: &str) -> (Vec<Span<'static>>, usize) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut style = Style::default();
    let mut buf = String::new();
    let mut width = 0;
    let chars: Vec<char> = raw.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] != '\u{1b}' {
            width += chars[i].to_string().width();
            buf.push(chars[i]);
            i += 1;
            continue;
        }
        // An escape ends the current run, whatever it turns out to be.
        if !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut buf), style));
        }
        match chars.get(i + 1) {
            Some('[') => {
                let mut end = i + 2;
                while end < chars.len() && !('\u{40}'..='\u{7e}').contains(&chars[end]) {
                    end += 1;
                }
                if end >= chars.len() {
                    // Truncated sequence: nothing sensible left to draw.
                    break;
                }
                if chars[end] == 'm' {
                    let params: String = chars[i + 2..end].iter().collect();
                    apply_sgr(&mut style, &params);
                }
                // Anything else (cursor moves, erases) is skipped.
                i = end + 1;
            }
            // A lone escape, or one introducing something we do not read.
            _ => i += 2,
        }
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, style));
    }
    (spans, width)
}

fn apply_sgr(style: &mut Style, params: &str) {
    let codes: Vec<u16> = if params.is_empty() {
        vec![0]
    } else {
        params
            .split(';')
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };

    let mut i = 0;
    while i < codes.len() {
        match codes[i] {
            0 => *style = Style::default(),
            1 => *style = style.add_modifier(Modifier::BOLD),
            2 => *style = style.add_modifier(Modifier::DIM),
            3 => *style = style.add_modifier(Modifier::ITALIC),
            4 => *style = style.add_modifier(Modifier::UNDERLINED),
            7 => *style = style.add_modifier(Modifier::REVERSED),
            22 => *style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => *style = style.remove_modifier(Modifier::ITALIC),
            24 => *style = style.remove_modifier(Modifier::UNDERLINED),
            27 => *style = style.remove_modifier(Modifier::REVERSED),
            n @ 30..=37 => style.fg = Some(Color::Indexed((n - 30) as u8)),
            n @ 90..=97 => style.fg = Some(Color::Indexed((n - 90 + 8) as u8)),
            39 => style.fg = None,
            n @ 40..=47 => style.bg = Some(Color::Indexed((n - 40) as u8)),
            n @ 100..=107 => style.bg = Some(Color::Indexed((n - 100 + 8) as u8)),
            49 => style.bg = None,
            38 | 48 => {
                let target_fg = codes[i] == 38;
                match codes.get(i + 1) {
                    Some(5) => {
                        if let Some(n) = codes.get(i + 2) {
                            let color = Color::Indexed(*n as u8);
                            if target_fg {
                                style.fg = Some(color);
                            } else {
                                style.bg = Some(color);
                            }
                        }
                        i += 2;
                    }
                    Some(2) => {
                        if let (Some(r), Some(g), Some(b)) =
                            (codes.get(i + 2), codes.get(i + 3), codes.get(i + 4))
                        {
                            let color = Color::Rgb(*r as u8, *g as u8, *b as u8);
                            if target_fg {
                                style.fg = Some(color);
                            } else {
                                style.bg = Some(color);
                            }
                        }
                        i += 4;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        i += 1;
    }
}

/// Art pasted through something that ate the escape characters leaves the
/// parameters behind as literal text. Worth telling someone about rather than
/// silently drawing `[0;37m` across their splash.
pub fn looks_like_stripped_escapes(text: &str) -> bool {
    !text.contains('\u{1b}')
        && text
            .match_indices('[')
            .filter(|(at, _)| {
                let rest = &text[*at + 1..];
                let end = rest.find('m').unwrap_or(0);
                end > 0
                    && end <= 12
                    && rest[..end]
                        .chars()
                        .all(|c| c.is_ascii_digit() || c == ';')
            })
            .count()
            >= 3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_art_takes_the_given_style() {
        let art = parse("hello\nthere", Style::default().fg(Color::Red));
        assert!(!art.colored);
        assert_eq!(art.width, 5);
        assert_eq!(art.lines.len(), 2);
        assert_eq!(art.lines[0].spans[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn colours_are_passed_through_as_authored() {
        let art = parse("\u{1b}[0;96m##\u{1b}[0m", Style::default());
        assert!(art.colored);
        assert_eq!(art.width, 2);
        let span = &art.lines[0].spans[0];
        assert_eq!(span.content, "##");
        assert_eq!(span.style.fg, Some(Color::Indexed(14)));
    }

    #[test]
    fn background_and_256_and_rgb() {
        let art = parse("\u{1b}[47mA\u{1b}[48;5;33mB\u{1b}[38;2;1;2;3mC", Style::default());
        let spans = &art.lines[0].spans;
        assert_eq!(spans[0].style.bg, Some(Color::Indexed(7)));
        assert_eq!(spans[1].style.bg, Some(Color::Indexed(33)));
        assert_eq!(spans[2].style.fg, Some(Color::Rgb(1, 2, 3)));
    }

    /// Escape sequences must not count toward the width, or centring would
    /// shove the art off to the left.
    #[test]
    fn width_ignores_escapes() {
        let plain = parse("▓▓ ▒▒", Style::default());
        let colored = parse("\u{1b}[0;97;47m▓▓\u{1b}[0;37m \u{1b}[0;96;47m▒▒\u{1b}[0m", Style::default());
        assert_eq!(plain.width, colored.width);
    }

    #[test]
    fn cursor_movement_is_skipped_not_drawn() {
        let art = parse("\u{1b}[5CX", Style::default());
        assert_eq!(art.lines[0].spans.len(), 1);
        assert_eq!(art.lines[0].spans[0].content, "X");
    }

    #[test]
    fn spots_art_whose_escapes_were_eaten() {
        assert!(looks_like_stripped_escapes("[0;37m##[0;96m▀▄[0;37m  [0;97m█▄"));
        assert!(!looks_like_stripped_escapes("plain ascii [art] here"));
        assert!(!looks_like_stripped_escapes("\u{1b}[0;37m real escapes"));
    }
}
