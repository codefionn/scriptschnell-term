//! Byte streams from a weighted vocabulary of the sequences stage 1 implements.

use proptest::prelude::*;

/// One generated case: a size and the pieces of the stream. Shrinking drops pieces.
#[derive(Clone)]
pub struct Case {
    pub rows: usize,
    pub cols: usize,
    pub pieces: Vec<Vec<u8>>,
}

impl Case {
    pub fn bytes(&self) -> Vec<u8> {
        self.pieces.concat()
    }
}

impl std::fmt::Debug for Case {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Case {{ rows: {}, cols: {}, stream: \"{}\" }}",
            self.rows,
            self.cols,
            escape(&self.bytes())
        )
    }
}

/// Renders bytes the way a failure message shows them: printable ASCII as is, the rest
/// escaped, so the stream can be pasted into a Rust string literal.
pub fn escape(bytes: &[u8]) -> String {
    let mut s = String::new();
    for &b in bytes {
        match b {
            0x1b => s.push_str("\\x1b"),
            b'\\' => s.push_str("\\\\"),
            b'"' => s.push_str("\\\""),
            b'\r' => s.push_str("\\r"),
            b'\n' => s.push_str("\\n"),
            b'\t' => s.push_str("\\t"),
            0x20..=0x7e => s.push(b as char),
            _ => s.push_str(&format!("\\x{b:02x}")),
        }
    }
    s
}

fn param() -> impl Strategy<Value = u32> {
    prop_oneof![
        4 => 0u32..4,
        3 => 4u32..12,
        1 => 12u32..40,
    ]
}

fn csi(params: &'static str, fin: &'static str) -> impl Strategy<Value = Vec<u8>> {
    // `params` is the prefix such as "" or "?"; the numbers follow.
    (prop::collection::vec(param(), 0..3), Just((params, fin))).prop_map(|(nums, (pre, fin))| {
        let n: Vec<String> = nums.iter().map(u32::to_string).collect();
        format!("\x1b[{pre}{}{fin}", n.join(";")).into_bytes()
    })
}

fn sgr() -> impl Strategy<Value = Vec<u8>> {
    let one = prop_oneof![
        // Attribute resets get a high weight: a reset that does nothing only shows when
        // the attribute was set before and the reset is the next thing to touch it.
        6 => prop::sample::select(vec![
            "22", "23", "24", "25", "27", "28", "29", "39", "49", "59",
        ])
        .prop_map(String::from),
        3 => prop::sample::select(vec![
            "0", "1", "2", "3", "4", "5", "7", "8", "9", "",
        ])
        .prop_map(String::from),
        3 => (30u32..38).prop_map(|n| n.to_string()),
        3 => (40u32..48).prop_map(|n| n.to_string()),
        2 => (90u32..98).prop_map(|n| n.to_string()),
        2 => (100u32..108).prop_map(|n| n.to_string()),
        2 => (prop::sample::select(vec![38u32, 48]), 0u32..256).prop_map(|(c, n)| format!("{c};5;{n}")),
        2 => (prop::sample::select(vec![38u32, 48]), 0u32..256).prop_map(|(c, n)| format!("{c}:5:{n}")),
        2 => (prop::sample::select(vec![38u32, 48]), 0u32..256, 0u32..256, 0u32..256)
            .prop_map(|(c, r, g, b)| format!("{c};2;{r};{g};{b}")),
        2 => (prop::sample::select(vec![38u32, 48]), 0u32..256, 0u32..256, 0u32..256)
            .prop_map(|(c, r, g, b)| format!("{c}:2::{r}:{g}:{b}")),
        1 => (0u32..6).prop_map(|n| format!("4:{n}")),
    ];
    prop::collection::vec(one, 1..4).prop_map(|v| format!("\x1b[{}m", v.join(";")).into_bytes())
}

fn text(wide: bool) -> BoxedStrategy<Vec<u8>> {
    let narrow = prop_oneof![
        8 => "[ -~]{1,10}".prop_map(String::into_bytes),
        1 => prop::sample::select(vec!["é", "Ωßñ", "ｱｲ", "☃"])
            .prop_map(|s| s.as_bytes().to_vec()),
    ];
    if !wide {
        return narrow.boxed();
    }
    // A wide character at the edge of the line stops a case, so only some cases have them.
    prop_oneof![
        3 => narrow,
        1 => prop::sample::select(vec!["日本語", "你好", "한국", "a日b"])
            .prop_map(|s| s.as_bytes().to_vec()),
    ]
    .boxed()
}

/// Grapheme clusters of more than one character. The wide ones stop a case at the edge of
/// the line like any wide character, so only cases with `wide` get them.
fn clusters(wide: bool) -> BoxedStrategy<Vec<u8>> {
    let narrow = prop::sample::select(vec![
        "e\u{301}",
        "a\u{300}\u{323}",
        "\u{1E9B}\u{323}",
        "n\u{303}",
        "\u{2764}\u{FE0E}",
        "\u{2603}\u{FE0E}",
    ]);
    if !wide {
        return narrow.prop_map(|s| s.as_bytes().to_vec()).boxed();
    }
    let wide_set = prop::sample::select(vec![
        "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
        "\u{1F469}\u{200D}\u{1F4BB}",
        "\u{1F1E9}\u{1F1EA}",
        "\u{1F1EB}\u{1F1F7}",
        "\u{1F44D}\u{1F3FD}",
        "\u{1F600}\u{FE0F}",
        "\u{2764}\u{FE0F}",
        "\u{2603}\u{FE0F}",
        "\u{1F600}\u{FE0E}",
        "\u{1100}\u{1161}\u{11A8}",
        "\u{AC00}\u{11A8}",
    ]);
    prop_oneof![
        3 => narrow.prop_map(|s| s.as_bytes().to_vec()),
        1 => wide_set.prop_map(|s| s.as_bytes().to_vec()),
    ]
    .boxed()
}

/// A lone zero-width character, which joins whatever is left of the cursor.
fn zero_width() -> impl Strategy<Value = Vec<u8>> {
    prop::sample::select(vec![
        "\u{301}", "\u{200D}", "\u{FE0F}", "\u{FE0E}", "\u{200B}",
    ])
    .prop_map(|s| s.as_bytes().to_vec())
}

fn controls() -> impl Strategy<Value = Vec<u8>> {
    prop::sample::select(vec![
        b"\r".to_vec(),
        b"\n".to_vec(),
        b"\r\n".to_vec(),
        b"\t".to_vec(),
        b"\x08".to_vec(),
        b"\x0b".to_vec(),
        b"\x0c".to_vec(),
        b"\x07".to_vec(),
        b"\x0e".to_vec(),
        b"\x0f".to_vec(),
    ])
}

fn esc() -> impl Strategy<Value = Vec<u8>> {
    prop::sample::select(vec![
        "\x1b7", "\x1b8", "\x1bD", "\x1bE", "\x1bM", "\x1bH", "\x1bc", "\x1b(0", "\x1b(B",
        "\x1b)0", "\x1b)B", "\x1b(A", "\x1b*0", "\x1b+0",
    ])
    .prop_map(|s| s.as_bytes().to_vec())
}

/// ESC n and ESC o (locking shifts to G2 and G3) cannot be compared with wezterm and
/// stop the case. They are only generated as the last piece, where nothing is lost.
fn tail() -> impl Strategy<Value = Option<Vec<u8>>> {
    prop_oneof![
        5 => Just(None),
        1 => prop::sample::select(vec![b"\x1bn".to_vec(), b"\x1bo".to_vec()]).prop_map(Some),
    ]
}

fn modes(f: Features) -> BoxedStrategy<Vec<u8>> {
    let mut list = vec![6u32, 7, 25];
    if f.alt {
        list.extend([47, 1047, 1049]);
    }
    let private = prop::sample::select(list);
    let margins = f.margins;
    let base = prop_oneof![
        4 => (private, prop::sample::select(vec!['h', 'l']))
            .prop_map(|(m, f)| format!("\x1b[?{m}{f}").into_bytes()),
        1 => prop::sample::select(vec![b"\x1b[4h".to_vec(), b"\x1b[4l".to_vec()]),
        1 => prop::sample::select(vec![b"\x1b[20h".to_vec(), b"\x1b[20l".to_vec()]),
    ];
    if !margins {
        return base.boxed();
    }
    prop_oneof![
        3 => base,
        1 => prop::sample::select(vec![b"\x1b[?69h".to_vec(), b"\x1b[?69l".to_vec()]),
    ]
    .boxed()
}

fn csi_piece(f: Features) -> BoxedStrategy<Vec<u8>> {
    let margins = f.margins;
    // A feature that is off is replaced by a plain cursor move, so the weights stay.
    let region = if f.region {
        csi("", "r").boxed()
    } else {
        csi("", "H").boxed()
    };
    let decstr = if f.decstr {
        Just(b"\x1b[!p".to_vec()).boxed()
    } else {
        Just(b"\x1b[H".to_vec()).boxed()
    };
    let base = prop_oneof![
        // Cursor movement.
        6 => prop::sample::select(vec!["A", "B", "C", "D", "E", "F", "G", "H", "f", "d", "`", "a", "e"])
            .prop_flat_map(|f| csi("", f)),
        2 => prop::sample::select(vec!["I", "Z"]).prop_flat_map(|f| csi("", f)),
        // Erase, insert, delete, scroll.
        4 => prop::sample::select(vec!["J", "K", "X"]).prop_flat_map(|f| csi("", f)),
        4 => prop::sample::select(vec!["@", "P", "L", "M", "S", "T"]).prop_flat_map(|f| csi("", f)),
        // REP always follows a printed character: with none, wezterm prints a space
        // where xterm does nothing, and that case is not worth a mask.
        2 => ("[a-zA-Z0-9]".prop_map(String::from), csi("", "b")).prop_map(|(c, mut rep)| {
            let mut v = c.into_bytes();
            v.append(&mut rep);
            v
        }),
        // Tabs.
        1 => prop::sample::select(vec![b"\x1b[g".to_vec(), b"\x1b[0g".to_vec(), b"\x1b[3g".to_vec()]),
        // Margins and soft reset.
        3 => region,
        3 => csi("", "s"),
        // Restore cursor. The bare form is what programs send. Parameters are ignored by
        // xterm, so they are worth a few cases.
        2 => Just(b"\x1b[u".to_vec()),
        1 => csi("", "u"),
        1 => decstr,
    ];
    if !margins {
        return base.boxed();
    }
    prop_oneof![
        8 => base,
        1 => Just(b"\x1b[?69h\x1b[2;5s".to_vec()),
    ]
    .boxed()
}

/// Features that make many sequences stop a case, so only some cases get them.
#[derive(Clone, Copy, Debug)]
pub struct Features {
    /// Column margins (DECLRMM).
    margins: bool,
    /// Wide characters.
    wide: bool,
    /// Top and bottom margins (DECSTBM).
    region: bool,
    /// The alternate screen modes.
    alt: bool,
    /// DECSTR.
    decstr: bool,
    /// Lone zero-width characters, which stop a case (see the allowlist).
    lone: bool,
}

fn features() -> impl Strategy<Value = Features> {
    (
        prop::bool::weighted(0.125),
        prop::bool::weighted(0.12),
        prop::bool::weighted(0.35),
        prop::bool::weighted(0.3),
        prop::bool::weighted(0.25),
        prop::bool::weighted(0.005),
    )
        .prop_map(|(margins, wide, region, alt, decstr, lone)| Features {
            margins,
            wide,
            region,
            alt,
            decstr,
            lone,
        })
}

pub fn piece(f: Features) -> impl Strategy<Value = Vec<u8>> {
    let mut parts = vec![
        (8, text(f.wide)),
        (1, clusters(f.wide)),
        (4, controls().boxed()),
        (3, esc().boxed()),
        (14, csi_piece(f).boxed()),
        (8, sgr().boxed()),
        (3, modes(f).boxed()),
    ];
    if f.lone {
        parts.push((6, zero_width().boxed()));
    }
    proptest::strategy::Union::new_weighted(parts)
}

pub fn case() -> impl Strategy<Value = Case> {
    (2usize..9, 3usize..14, features(), tail()).prop_flat_map(|(rows, cols, f, tail)| {
        prop::collection::vec(piece(f), 0..40).prop_map(move |mut pieces| {
            pieces.extend(tail.clone());
            Case { rows, cols, pieces }
        })
    })
}
