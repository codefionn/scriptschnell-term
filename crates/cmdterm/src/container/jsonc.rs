//! JSON with comments, as devcontainer.json allows: `//` and `/* */`
//! comments plus trailing commas before `}` or `]`.

/// `text` with comments and trailing commas removed, so `serde_json` can
/// read it. Comments become spaces, which keeps byte offsets in parse errors
/// close to the original. Content inside strings is never touched.
pub fn strip(text: &str) -> String {
    let without_comments = strip_comments(text);
    strip_trailing_commas(&without_comments)
}

fn strip_comments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => i = copy_string(bytes, i, &mut out),
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    out.push(b' ');
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                out.extend_from_slice(b"  ");
                i += 2;
                while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                    // Keep newlines so line numbers in errors stay right.
                    out.push(if bytes[i] == b'\n' { b'\n' } else { b' ' });
                    i += 1;
                }
                if i < bytes.len() {
                    out.extend_from_slice(b"  ");
                    i += 2;
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    // Only ASCII bytes were replaced, by ASCII bytes, so this stays UTF-8.
    String::from_utf8(out).unwrap_or_default()
}

fn strip_trailing_commas(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => i = copy_string(bytes, i, &mut out),
            b',' => {
                let next = bytes[i + 1..]
                    .iter()
                    .find(|b| !b.is_ascii_whitespace())
                    .copied();
                out.push(if matches!(next, Some(b'}' | b']')) {
                    b' '
                } else {
                    b','
                });
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

/// Copies the string literal starting at `bytes[start] == b'"'` and returns
/// the index after its closing quote, or the end of input.
fn copy_string(bytes: &[u8], start: usize, out: &mut Vec<u8>) -> usize {
    out.push(b'"');
    let mut i = start + 1;
    while i < bytes.len() {
        let byte = bytes[i];
        out.push(byte);
        i += 1;
        match byte {
            b'\\' if i < bytes.len() => {
                out.push(bytes[i]);
                i += 1;
            }
            b'"' => break,
            _ => {}
        }
    }
    i
}

#[cfg(test)]
mod tests {
    use super::strip;

    fn parse(text: &str) -> serde_json::Value {
        serde_json::from_str(&strip(text)).unwrap_or_else(|e| panic!("{e}: {}", strip(text)))
    }

    #[test]
    fn removes_line_and_block_comments() {
        let value = parse(
            "{\n  // the name\n  \"name\": \"x\", /* inline */ \"a\": 1\n  /* multi\n line */\n}",
        );
        assert_eq!(value, serde_json::json!({"name": "x", "a": 1}));
    }

    #[test]
    fn keeps_comment_markers_inside_strings() {
        let value = parse(r#"{"url": "https://example.com/a//b", "glob": "/* not a comment */"}"#);
        assert_eq!(value["url"], "https://example.com/a//b");
        assert_eq!(value["glob"], "/* not a comment */");
    }

    #[test]
    fn handles_escaped_quotes_and_backslashes() {
        let value = parse(
            r#"{"a": "say \"hi\" // still string", "b": "C:\\", "c": 1 // gone
        }"#,
        );
        assert_eq!(value["a"], "say \"hi\" // still string");
        assert_eq!(value["b"], "C:\\");
        assert_eq!(value["c"], 1);
    }

    #[test]
    fn removes_trailing_commas() {
        let value = parse("{\"a\": [1, 2, ], \"b\": {\"c\": \"x,\",\n},\n}");
        assert_eq!(value, serde_json::json!({"a": [1, 2], "b": {"c": "x,"}}));
    }

    #[test]
    fn trailing_comma_before_a_comment() {
        let value = parse("{\"a\": 1, // last\n}");
        assert_eq!(value, serde_json::json!({"a": 1}));
    }

    #[test]
    fn keeps_line_numbers() {
        let text = "{\n/* a\nb */\n\"a\": 1}";
        assert_eq!(strip(text).lines().count(), text.lines().count());
    }
}
