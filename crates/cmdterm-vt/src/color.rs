//! Colors, the 256-entry palette and the default colors.

/// A color as a style stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Color {
    /// The terminal's default foreground, background or underline color.
    #[default]
    Default,
    /// An index into the 256-entry palette.
    Palette(u8),
    Rgb(u8, u8, u8),
}

/// A resolved color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// xterm's default 16 ANSI colors.
pub const XTERM_ANSI: [Rgb; 16] = [
    Rgb::new(0x00, 0x00, 0x00),
    Rgb::new(0xcd, 0x00, 0x00),
    Rgb::new(0x00, 0xcd, 0x00),
    Rgb::new(0xcd, 0xcd, 0x00),
    Rgb::new(0x00, 0x00, 0xee),
    Rgb::new(0xcd, 0x00, 0xcd),
    Rgb::new(0x00, 0xcd, 0xcd),
    Rgb::new(0xe5, 0xe5, 0xe5),
    Rgb::new(0x7f, 0x7f, 0x7f),
    Rgb::new(0xff, 0x00, 0x00),
    Rgb::new(0x00, 0xff, 0x00),
    Rgb::new(0xff, 0xff, 0x00),
    Rgb::new(0x5c, 0x5c, 0xff),
    Rgb::new(0xff, 0x00, 0xff),
    Rgb::new(0x00, 0xff, 0xff),
    Rgb::new(0xff, 0xff, 0xff),
];

/// The palette plus the default foreground, background and cursor colors.
#[derive(Debug, Clone)]
pub struct Palette {
    entries: [Rgb; 256],
    pub fg: Rgb,
    pub bg: Rgb,
    pub cursor: Rgb,
}

impl Palette {
    /// Entries 0 to 15 come from `config`; 16 to 231 are xterm's 6x6x6 cube and 232 to
    /// 255 its grayscale ramp.
    pub fn new(config: &crate::Config) -> Self {
        const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let mut entries = [Rgb::new(0, 0, 0); 256];
        entries[..16].copy_from_slice(&config.ansi);
        for i in 0..216usize {
            entries[16 + i] = Rgb::new(LEVELS[i / 36], LEVELS[(i / 6) % 6], LEVELS[i % 6]);
        }
        for i in 0..24usize {
            let v = (8 + 10 * i) as u8;
            entries[232 + i] = Rgb::new(v, v, v);
        }
        Self {
            entries,
            fg: config.fg,
            bg: config.bg,
            cursor: config.cursor,
        }
    }

    pub fn get(&self, index: u8) -> Rgb {
        self.entries[usize::from(index)]
    }

    pub fn set(&mut self, index: u8, rgb: Rgb) {
        self.entries[usize::from(index)] = rgb;
    }

    /// Resolves a style color. `Default` becomes the default foreground when `foreground`
    /// is true and the default background otherwise.
    pub fn resolve(&self, color: Color, foreground: bool) -> Rgb {
        match color {
            Color::Default if foreground => self.fg,
            Color::Default => self.bg,
            Color::Palette(i) => self.get(i),
            Color::Rgb(r, g, b) => Rgb { r, g, b },
        }
    }
}
