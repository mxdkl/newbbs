//! Colour handling.
//!
//! Every colour in the UI is authored once as 0xRRGGBB and degraded at render
//! time to whatever the terminal actually supports: truecolor, the xterm 256
//! cube, or the 16 ANSI colours on a plain console. Backgrounds are left
//! unset wherever possible so the terminal's own theme shows through.

use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    Truecolor,
    Ansi256,
    Ansi16,
}

impl Depth {
    /// Best guess from the environment, the way most terminal apps do it.
    pub fn detect() -> Depth {
        let colorterm = std::env::var("COLORTERM").unwrap_or_default();
        if colorterm.contains("truecolor") || colorterm.contains("24bit") {
            return Depth::Truecolor;
        }
        let term = std::env::var("TERM").unwrap_or_default();
        if term.contains("256color") || term.contains("direct") {
            return Depth::Ansi256;
        }
        Depth::Ansi16
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub fg: u32,
    pub dim: u32,
    pub border: u32,
    pub border_focus: u32,
    pub accent: u32,
    pub selection: u32,
    pub ok: u32,
    pub warn: u32,
    pub code: u32,
    pub quote: u32,
}

pub const DARK: Palette = Palette {
    fg: 0xc8ccd4,
    dim: 0x6b7280,
    border: 0x3b4048,
    border_focus: 0x61afef,
    accent: 0x61afef,
    selection: 0x2c323c,
    ok: 0x98c379,
    warn: 0xe5c07b,
    code: 0xd19a66,
    quote: 0x7f8c98,
};

pub const LIGHT: Palette = Palette {
    fg: 0x24292f,
    dim: 0x6e7781,
    border: 0xd0d7de,
    border_focus: 0x0969da,
    accent: 0x0969da,
    selection: 0xeaeef2,
    ok: 0x1a7f37,
    warn: 0x9a6700,
    code: 0x953800,
    quote: 0x6e7781,
};

#[derive(Debug, Clone)]
pub struct Theme {
    pub name: String,
    pub depth: Depth,
    pub palette: Palette,
    /// When set, every colour resolves to the terminal default -- useful if a
    /// console's palette fights ours.
    pub mono: bool,
}

impl Theme {
    pub fn new() -> Theme {
        Theme {
            name: "dark".into(),
            depth: Depth::detect(),
            palette: DARK,
            mono: false,
        }
    }

    /// `:theme <name>`. Returns false if the name is unknown.
    pub fn set(&mut self, name: &str) -> bool {
        match name {
            "dark" => (self.palette, self.mono) = (DARK, false),
            "light" => (self.palette, self.mono) = (LIGHT, false),
            "mono" => (self.palette, self.mono) = (DARK, true),
            _ => return false,
        }
        self.name = name.to_string();
        true
    }

    pub fn names() -> &'static [&'static str] {
        &["dark", "light", "mono"]
    }

    /// Degrade an authored colour to something this terminal can show.
    pub fn color(&self, rgb: u32) -> Color {
        if self.mono {
            return Color::Reset;
        }
        let (r, g, b) = unpack(rgb);
        match self.depth {
            Depth::Truecolor => Color::Rgb(r, g, b),
            Depth::Ansi256 => Color::Indexed(to_256(r, g, b)),
            Depth::Ansi16 => Color::Indexed(to_16(r, g, b)),
        }
    }

    pub fn fg(&self) -> Color {
        self.color(self.palette.fg)
    }
    pub fn dim(&self) -> Color {
        self.color(self.palette.dim)
    }
    pub fn border(&self, focused: bool) -> Color {
        if focused {
            self.color(self.palette.border_focus)
        } else {
            self.color(self.palette.border)
        }
    }
    pub fn accent(&self) -> Color {
        self.color(self.palette.accent)
    }
    pub fn selection(&self) -> Color {
        self.color(self.palette.selection)
    }
}

fn unpack(rgb: u32) -> (u8, u8, u8) {
    (
        ((rgb >> 16) & 0xff) as u8,
        ((rgb >> 8) & 0xff) as u8,
        (rgb & 0xff) as u8,
    )
}

/// xterm-256: the 6x6x6 colour cube, or the grayscale ramp when the channels
/// are close enough together.
fn to_256(r: u8, g: u8, b: u8) -> u8 {
    let max = r.max(g).max(b) as i32;
    let min = r.min(g).min(b) as i32;
    if max - min < 12 {
        let level = ((r as i32 + g as i32 + b as i32) / 3) as i32;
        if level < 8 {
            return 16;
        }
        if level > 238 {
            return 231;
        }
        return (232 + (level - 8) * 24 / 230) as u8;
    }
    let q = |v: u8| -> u8 {
        // Cube levels are 0, 95, 135, 175, 215, 255.
        const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let mut best = 0;
        let mut best_d = i32::MAX;
        for (i, level) in LEVELS.iter().enumerate() {
            let d = (v as i32 - *level as i32).abs();
            if d < best_d {
                best_d = d;
                best = i as u8;
            }
        }
        best
    };
    16 + 36 * q(r) + 6 * q(g) + q(b)
}

/// The 16 ANSI colours, as most terminals render them. Used only to pick the
/// nearest index -- the terminal's own palette decides what is actually drawn,
/// which is what we want on a console themed by its owner.
const ANSI16: [(u8, u8, u8); 16] = [
    (0x00, 0x00, 0x00),
    (0xaa, 0x00, 0x00),
    (0x00, 0xaa, 0x00),
    (0xaa, 0x55, 0x00),
    (0x00, 0x00, 0xaa),
    (0xaa, 0x00, 0xaa),
    (0x00, 0xaa, 0xaa),
    (0xaa, 0xaa, 0xaa),
    (0x55, 0x55, 0x55),
    (0xff, 0x55, 0x55),
    (0x55, 0xff, 0x55),
    (0xff, 0xff, 0x55),
    (0x55, 0x55, 0xff),
    (0xff, 0x55, 0xff),
    (0x55, 0xff, 0xff),
    (0xff, 0xff, 0xff),
];

fn to_16(r: u8, g: u8, b: u8) -> u8 {
    let mut best = 7u8;
    let mut best_d = i32::MAX;
    for (i, (cr, cg, cb)) in ANSI16.iter().enumerate() {
        // Weighted to match perceived brightness rather than raw distance.
        let dr = r as i32 - *cr as i32;
        let dg = g as i32 - *cg as i32;
        let db = b as i32 - *cb as i32;
        let d = 2 * dr * dr + 4 * dg * dg + 3 * db * db;
        if d < best_d {
            best_d = d;
            best = i as u8;
        }
    }
    best
}
