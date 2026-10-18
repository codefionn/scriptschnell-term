//! Bracketed paste and focus reports.

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Encodes pasted text.
///
/// With bracketed paste on, the text is wrapped in `CSI 200 ~` and `CSI 201 ~` and every
/// ESC and C1 CSI (U+009B) is removed, so the text cannot close the bracket early or
/// start another sequence. Line feeds become carriage returns as xterm does; a CR LF
/// pair becomes one CR. With bracketed paste off only the line ends change.
pub fn encode_paste(bracketed: bool, text: &str, out: &mut Vec<u8>) {
    out.reserve(text.len() + PASTE_START.len() + PASTE_END.len());
    if bracketed {
        out.extend_from_slice(PASTE_START);
    }
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            0x1b if bracketed => {}
            b'\r' => {
                out.push(b'\r');
                if bytes.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
            }
            b'\n' => out.push(b'\r'),
            // U+009B is C2 9B in UTF-8.
            0xc2 if bracketed && bytes.get(i + 1) == Some(&0x9b) => i += 1,
            _ => out.push(b),
        }
        i += 1;
    }
    if bracketed {
        out.extend_from_slice(PASTE_END);
    }
}

/// Encodes a focus change: `CSI I` when gained, `CSI O` when lost, only when mode 1004 is
/// on. Returns whether bytes were written.
pub fn encode_focus(enabled: bool, focused: bool, out: &mut Vec<u8>) -> bool {
    if !enabled {
        return false;
    }
    out.extend_from_slice(if focused { b"\x1b[I" } else { b"\x1b[O" });
    true
}
