//! Byte scans for the parser's fast paths, written so the compiler turns them into SIMD.
//!
//! The crate has no unsafe code, so there are no intrinsics here. Each scan tests a
//! fixed chunk of [`LANES`] bytes, builds a bit mask with one bit per byte and stops at the
//! first set bit. LLVM compiles the chunk loop to one load, a compare and a `pmovmskb` on
//! x86-64 with SSE2 (the baseline), and to the NEON equivalent on aarch64. The bytes left
//! after the last whole chunk are scanned one at a time.

/// Bytes tested per step. 16 is one SSE2 or NEON register.
const LANES: usize = 16;

/// Length of the prefix of `bytes` in which `stop` is false for every byte.
#[inline(always)]
fn run_len(bytes: &[u8], stop: impl Fn(u8) -> bool) -> usize {
    let mut chunks = bytes.chunks_exact(LANES);
    let mut off = 0;
    for chunk in &mut chunks {
        let mut mask = 0u32;
        for (i, &b) in chunk.iter().enumerate() {
            mask |= u32::from(stop(b)) << i;
        }
        if mask != 0 {
            return off + mask.trailing_zeros() as usize;
        }
        off += LANES;
    }
    let rest = chunks.remainder();
    off + rest.iter().position(|&b| stop(b)).unwrap_or(rest.len())
}

/// Length of the leading run of printable ASCII (0x20 to 0x7E).
pub(crate) fn printable_ascii(bytes: &[u8]) -> usize {
    run_len(bytes, |b| b.wrapping_sub(0x20) > 0x7E - 0x20)
}

/// Length of the leading run of bytes an OSC string keeps as payload without looking at
/// them: everything but C0 controls and 0xC2, which may start the UTF-8 form of ST.
pub(crate) fn osc_payload(bytes: &[u8]) -> usize {
    run_len(bytes, |b| b < 0x20 || b == 0xC2)
}

/// Length of the leading run of bytes that an ignored string (SOS, PM, APC or an ignored
/// DCS) drops without effect: everything but ESC, CAN, SUB and 0xC2.
pub(crate) fn ignored_string(bytes: &[u8]) -> usize {
    run_len(bytes, |b| matches!(b, 0x1B | 0x18 | 0x1A | 0xC2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn naive(bytes: &[u8], stop: impl Fn(u8) -> bool) -> usize {
        bytes.iter().position(|&b| stop(b)).unwrap_or(bytes.len())
    }

    #[test]
    fn printable_bounds() {
        assert_eq!(printable_ascii(b""), 0);
        assert_eq!(printable_ascii(b" ~"), 2);
        assert_eq!(printable_ascii(b"\x1f"), 0);
        assert_eq!(printable_ascii(b"\x7f"), 0);
        assert_eq!(printable_ascii(&[0x80]), 0);
        let mut long = vec![b'a'; 40];
        long[33] = 0x1B;
        assert_eq!(printable_ascii(&long), 33);
        long[33] = b'a';
        assert_eq!(printable_ascii(&long), 40);
    }

    proptest! {
        #[test]
        fn scans_match_naive(bytes in proptest::collection::vec(any::<u8>(), 0..80), skip in 0usize..80) {
            // Mostly printable input, so runs cross chunk boundaries.
            let bytes: Vec<u8> = bytes.iter().enumerate()
                .map(|(i, &b)| if i % 7 == skip % 7 { b } else { b' ' + b % 95 })
                .collect();
            prop_assert_eq!(printable_ascii(&bytes), naive(&bytes, |b| !(0x20..=0x7E).contains(&b)));
            prop_assert_eq!(osc_payload(&bytes), naive(&bytes, |b| b < 0x20 || b == 0xC2));
            prop_assert_eq!(ignored_string(&bytes), naive(&bytes, |b| matches!(b, 0x1B | 0x18 | 0x1A | 0xC2)));
        }
    }
}
