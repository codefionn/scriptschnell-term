//! Character sets designated into G0 to G3.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Charset {
    #[default]
    Ascii,
    /// DEC special graphics (line drawing), designated by `0`.
    DecSpecial,
    /// UK national set, designated by `A`: `#` prints a pound sign.
    Uk,
}

impl Charset {
    /// Designation final byte after `ESC (` and friends.
    pub fn from_final(b: u8) -> Charset {
        match b {
            b'0' | b'2' => Charset::DecSpecial,
            b'A' => Charset::Uk,
            _ => Charset::Ascii,
        }
    }

    /// Maps a printable character. Characters outside ASCII are never mapped.
    pub fn map(self, c: char) -> char {
        match self {
            Charset::Ascii => c,
            Charset::Uk => {
                if c == '#' {
                    '\u{a3}'
                } else {
                    c
                }
            }
            Charset::DecSpecial => {
                const TABLE: [char; 32] = [
                    ' ', '◆', '▒', '␉', '␌', '␍', '␊', '°', '±', '␤', '␋', '┘', '┐', '┌', '└', '┼',
                    '⎺', '⎻', '─', '⎼', '⎽', '├', '┤', '┴', '┬', '│', '≤', '≥', 'π', '≠', '£', '·',
                ];
                match c {
                    '\u{5f}'..='\u{7e}' => TABLE[c as usize - 0x5f],
                    _ => c,
                }
            }
        }
    }
}
