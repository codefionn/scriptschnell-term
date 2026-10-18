use cmdterm_vt::{Config, Size, Terminal};
use proptest::prelude::*;

fn check(t: &Terminal) {
    let s = t.size();
    let c = t.cursor();
    assert!(c.row < s.rows && c.col < s.cols, "cursor {c:?} in {s:?}");
    let (top, bottom, left, right) = t.margins();
    assert!(top <= bottom && bottom < s.rows && left <= right && right < s.cols);
    for y in 0..s.rows {
        let r = t.row(y).unwrap();
        assert!(r.len() <= s.cols, "row {y} has {} cells", r.len());
    }
    for i in 0..t.scrollback_len() {
        assert!(t.screen().scrollback_row(i).unwrap().len() <= s.cols);
    }
}

fn cases() -> u32 {
    std::env::var("CMDTERM_VT_FUZZ_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000)
}

/// Bytes weighted toward escape sequences.
fn soup() -> impl Strategy<Value = Vec<u8>> {
    let piece = prop_oneof![
        3 => any::<u8>().prop_map(|b| vec![b]),
        2 => "[ -~]{1,6}".prop_map(String::into_bytes),
        2 => "\u{1b}\\[[0-9;:?<>!]{0,8}[@-~]".prop_map(String::into_bytes),
        1 => "\u{1b}[()#*+]?[0-9A-Za-z]".prop_map(String::into_bytes),
        1 => Just("日本\u{301}é".as_bytes().to_vec()),
        1 => Just(b"\r\n\t\x08".to_vec()),
        1 => Just(b"\x1b[?69h\x1b[2;5s\x1b[2;4r\x1b[?6h".to_vec()),
    ];
    prop::collection::vec(piece, 0..40).prop_map(|v| v.concat())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), rng_algorithm: proptest::test_runner::RngAlgorithm::ChaCha, rng_seed: proptest::test_runner::RngSeed::Fixed(0x5eed), failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn arbitrary_bytes_keep_invariants(bytes in prop::collection::vec(any::<u8>(), 0..400), rows in 1usize..8, cols in 1usize..12) {
        let mut t = Terminal::new(Size { rows, cols }, Config::default());
        t.advance(&bytes);
        check(&t);
    }

    #[test]
    fn escape_soup_keeps_invariants(bytes in soup(), rows in 1usize..8, cols in 1usize..12) {
        let mut t = Terminal::new(Size { rows, cols }, Config { scrollback: 5, ..Config::default() });
        t.advance(&bytes);
        check(&t);
        // Continue in chunks to cover state carried across calls.
        for chunk in bytes.chunks(3) {
            t.advance(chunk);
            check(&t);
        }
        let mut out = Vec::new();
        t.take_replies(&mut out);
        let _ = t.drain_events().count();
    }

    #[test]
    fn resize_keeps_invariants(bytes in soup(), r2 in 1usize..8, c2 in 1usize..12) {
        let mut t = Terminal::new(Size { rows: 5, cols: 9 }, Config::default());
        t.advance(&bytes);
        t.resize(Size { rows: r2, cols: c2 });
        check(&t);
        t.advance(&bytes);
        check(&t);
    }
}
