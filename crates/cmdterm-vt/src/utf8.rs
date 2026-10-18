//! Streaming UTF-8 decoder.
//!
//! Invalid input follows the "maximal subpart" rule: each maximal prefix of a valid
//! sequence that is cut short by a bad byte yields one U+FFFD, and the bad byte is looked
//! at again as the start of new input. Overlong forms, surrogates and code points above
//! U+10FFFF are rejected by narrowing the allowed range of the second byte.

/// What one more byte did to the decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The byte was accepted and more are needed.
    Pending,
    /// A complete character.
    Char(char),
    /// The byte is invalid on its own. Emit U+FFFD; the byte is consumed.
    Invalid,
    /// A pending sequence was cut short by this byte. Emit U+FFFD, then feed the same
    /// byte again.
    InvalidRetry,
}

/// Decoder state. Four bytes of state, no allocation.
#[derive(Debug, Clone, Default)]
pub struct Utf8Decoder {
    need: u8,
    cp: u32,
    lower: u8,
    upper: u8,
}

impl Utf8Decoder {
    pub const fn new() -> Self {
        Self {
            need: 0,
            cp: 0,
            lower: 0x80,
            upper: 0xBF,
        }
    }

    /// True while a multi-byte sequence is incomplete.
    pub const fn pending(&self) -> bool {
        self.need > 0
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn feed(&mut self, b: u8) -> Step {
        if self.need == 0 {
            let (need, mask, lower, upper) = match b {
                0x00..=0x7F => return Step::Char(b as char),
                0xC2..=0xDF => (1, 0x1F, 0x80, 0xBF),
                0xE0 => (2, 0x0F, 0xA0, 0xBF),
                0xED => (2, 0x0F, 0x80, 0x9F),
                0xE1..=0xEF => (2, 0x0F, 0x80, 0xBF),
                0xF0 => (3, 0x07, 0x90, 0xBF),
                0xF4 => (3, 0x07, 0x80, 0x8F),
                0xF1..=0xF3 => (3, 0x07, 0x80, 0xBF),
                _ => return Step::Invalid,
            };
            self.need = need;
            self.cp = u32::from(b & mask);
            self.lower = lower;
            self.upper = upper;
            return Step::Pending;
        }
        if b < self.lower || b > self.upper {
            self.reset();
            return Step::InvalidRetry;
        }
        self.lower = 0x80;
        self.upper = 0xBF;
        self.cp = (self.cp << 6) | u32::from(b & 0x3F);
        self.need -= 1;
        if self.need == 0 {
            Step::Char(char::from_u32(self.cp).unwrap_or('\u{FFFD}'))
        } else {
            Step::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8]) -> String {
        let mut d = Utf8Decoder::new();
        let mut out = String::new();
        for &b in bytes {
            loop {
                match d.feed(b) {
                    Step::Pending => {}
                    Step::Char(c) => out.push(c),
                    Step::Invalid => out.push('\u{FFFD}'),
                    Step::InvalidRetry => {
                        out.push('\u{FFFD}');
                        continue;
                    }
                }
                break;
            }
        }
        if d.pending() {
            out.push('\u{FFFD}');
        }
        out
    }

    #[test]
    fn valid_sequences() {
        assert_eq!(decode("aé€😀".as_bytes()), "aé€😀");
    }

    #[test]
    fn invalid_sequences() {
        let cases: &[(&[u8], &str)] = &[
            (&[0x80], "\u{FFFD}"),
            (&[0xC0, 0x80], "\u{FFFD}\u{FFFD}"),
            (&[0xC1, 0xBF], "\u{FFFD}\u{FFFD}"),
            (&[0xE0, 0x80, 0x80], "\u{FFFD}\u{FFFD}\u{FFFD}"),
            (&[0xED, 0xA0, 0x80], "\u{FFFD}\u{FFFD}\u{FFFD}"),
            (
                &[0xF4, 0x90, 0x80, 0x80],
                "\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}",
            ),
            (&[0xF5], "\u{FFFD}"),
            (&[0xFF], "\u{FFFD}"),
            (&[0xE2, 0x82, b'A'], "\u{FFFD}A"),
            (&[0xE2, 0x82], "\u{FFFD}"),
        ];
        for (bytes, want) in cases {
            assert_eq!(&decode(bytes), want, "{bytes:02x?}");
        }
    }

    proptest::proptest! {
        #[test]
        fn matches_std_lossy(bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..64)) {
            proptest::prop_assert_eq!(decode(&bytes), String::from_utf8_lossy(&bytes));
        }
    }
}
