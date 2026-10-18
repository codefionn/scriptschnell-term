use cmdterm_vt::{Config, Rgb};

/// Lines of scrollback kept per terminal.
pub const SCROLLBACK: usize = 10_000;

/// The 16 ANSI colors of a scheme that sets none: the palette the app showed
/// when wezterm-term drew its terminals, so programs keep their colors.
pub const DEFAULT_ANSI: [(u8, u8, u8); 16] = [
    (0x00, 0x00, 0x00),
    (0xcc, 0x55, 0x55),
    (0x55, 0xcc, 0x55),
    (0xcd, 0xcd, 0x55),
    (0x54, 0x55, 0xcb),
    (0xcc, 0x55, 0xcc),
    (0x7a, 0xca, 0xca),
    (0xcc, 0xcc, 0xcc),
    (0x55, 0x55, 0x55),
    (0xff, 0x55, 0x55),
    (0x55, 0xff, 0x55),
    (0xff, 0xff, 0x55),
    (0x55, 0x55, 0xff),
    (0xff, 0x55, 0xff),
    (0x55, 0xff, 0xff),
    (0xff, 0xff, 0xff),
];

/// The colors a terminal draws with. The app builds one from its color
/// scheme (`crates/cmdterm/src/ui/theme.rs`) and hands it to every session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermColors {
    pub foreground: (u8, u8, u8),
    pub background: (u8, u8, u8),
    pub cursor: (u8, u8, u8),
    /// The 16 ANSI colors, normal then bright. `None` uses [`DEFAULT_ANSI`].
    pub ansi: Option<[(u8, u8, u8); 16]>,
}

impl Default for TermColors {
    /// The Spruce scheme: ink on spruce with a sage cursor.
    fn default() -> Self {
        Self {
            foreground: (0xd7, 0xe0, 0xe2),
            background: (0x10, 0x17, 0x1c),
            cursor: (0x7f, 0xb7, 0xa4),
            ansi: None,
        }
    }
}

fn rgb((r, g, b): (u8, u8, u8)) -> Rgb {
    Rgb::new(r, g, b)
}

impl TermColors {
    /// The emulator's settings for a session: these colors, the scrollback
    /// size and the name XTVERSION answers with. A program cannot read the
    /// clipboard through OSC 52.
    pub fn config(self) -> Config {
        let mut config = Config {
            fg: rgb(self.foreground),
            bg: rgb(self.background),
            cursor: rgb(self.cursor),
            scrollback: SCROLLBACK,
            allow_clipboard_query: false,
            xtversion: format!(
                "{} {}",
                crate::term::identity::PROGRAM,
                crate::term::identity::VERSION
            ),
            ..Config::default()
        };
        for (slot, color) in config
            .ansi
            .iter_mut()
            .zip(self.ansi.unwrap_or(DEFAULT_ANSI))
        {
            *slot = rgb(color);
        }
        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cmdterm_vt::{Palette, Size, Terminal};

    #[test]
    fn ansi_colors_replace_the_first_sixteen_slots() {
        let mut ansi = [(0, 0, 0); 16];
        ansi[1] = (0xff, 0, 0);
        ansi[15] = (0xee, 0xee, 0xee);
        let colors = TermColors {
            ansi: Some(ansi),
            ..TermColors::default()
        };
        let config = colors.config();
        assert_eq!(config.ansi[1], rgb((0xff, 0, 0)));
        assert_eq!(config.ansi[15], rgb((0xee, 0xee, 0xee)));
        let palette = Palette::new(&config);
        assert_eq!(palette.get(1), rgb((0xff, 0, 0)));
        // The 256-color cube stays the emulator's.
        assert_eq!(palette.get(16), Palette::new(&Config::default()).get(16));
        assert_eq!(palette.bg, rgb(TermColors::default().background));
        assert_eq!(palette.cursor, rgb(TermColors::default().cursor));
    }

    #[test]
    fn a_scheme_without_ansi_colors_keeps_the_palette_of_earlier_versions() {
        let config = TermColors::default().config();
        // Bold red text reads as bright red, 0xff5555, not xterm's 0xff0000.
        assert_eq!(config.ansi[9], rgb((0xff, 0x55, 0x55)));
        assert_eq!(config.ansi[1], rgb((0xcc, 0x55, 0x55)));
        let mut term = Terminal::new(Size { rows: 2, cols: 20 }, config);
        term.advance(b"\x1b[91mx");
        let palette = term.palette();
        assert_eq!(palette.get(9), rgb((0xff, 0x55, 0x55)));
        // The cube and the gray ramp match wezterm's.
        assert_eq!(palette.get(196), rgb((0xff, 0, 0)));
        assert_eq!(palette.get(232), rgb((8, 8, 8)));
    }

    #[test]
    fn the_config_sets_scrollback_and_the_version_reply() {
        let config = TermColors::default().config();
        assert_eq!(config.scrollback, SCROLLBACK);
        assert!(!config.allow_clipboard_query);
        let mut term = Terminal::new(Size { rows: 2, cols: 20 }, config);
        term.advance(b"\x1b[>0q");
        let mut reply = Vec::new();
        term.take_replies(&mut reply);
        let expected = format!(
            "\x1bP>|{} {}\x1b\\",
            crate::term::identity::PROGRAM,
            crate::term::identity::VERSION
        );
        assert_eq!(reply, expected.as_bytes());
    }
}
