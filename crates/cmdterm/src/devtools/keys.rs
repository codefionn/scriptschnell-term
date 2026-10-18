//! DOM key names to the codes each protocol wants.

use crate::devtools::driver::{ALT, CONTROL, KeyStroke, META, SHIFT};

/// Named keys: DOM key, DOM code, Windows key code, WebDriver code point,
/// typed text.
const NAMED: &[(&str, &str, u32, char, Option<&str>)] = &[
    ("Enter", "Enter", 13, '\u{E007}', Some("\r")),
    ("Tab", "Tab", 9, '\u{E004}', Some("\t")),
    ("Backspace", "Backspace", 8, '\u{E003}', None),
    ("Escape", "Escape", 27, '\u{E00C}', None),
    ("Delete", "Delete", 46, '\u{E017}', None),
    ("Insert", "Insert", 45, '\u{E016}', None),
    ("Home", "Home", 36, '\u{E011}', None),
    ("End", "End", 35, '\u{E010}', None),
    ("PageUp", "PageUp", 33, '\u{E00E}', None),
    ("PageDown", "PageDown", 34, '\u{E00F}', None),
    ("ArrowLeft", "ArrowLeft", 37, '\u{E012}', None),
    ("ArrowUp", "ArrowUp", 38, '\u{E013}', None),
    ("ArrowRight", "ArrowRight", 39, '\u{E014}', None),
    ("ArrowDown", "ArrowDown", 40, '\u{E015}', None),
    ("Shift", "ShiftLeft", 16, '\u{E008}', None),
    ("Control", "ControlLeft", 17, '\u{E009}', None),
    ("Alt", "AltLeft", 18, '\u{E00A}', None),
    ("Meta", "MetaLeft", 91, '\u{E03D}', None),
    ("F1", "F1", 112, '\u{E031}', None),
    ("F2", "F2", 113, '\u{E032}', None),
    ("F3", "F3", 114, '\u{E033}', None),
    ("F4", "F4", 115, '\u{E034}', None),
    ("F5", "F5", 116, '\u{E035}', None),
    ("F6", "F6", 117, '\u{E036}', None),
    ("F7", "F7", 118, '\u{E037}', None),
    ("F8", "F8", 119, '\u{E038}', None),
    ("F9", "F9", 120, '\u{E039}', None),
    ("F10", "F10", 121, '\u{E03A}', None),
    ("F11", "F11", 122, '\u{E03B}', None),
    ("F12", "F12", 123, '\u{E03C}', None),
];

/// The key named `name`: a DOM key name such as `ArrowDown`, or one
/// character. `Space` and `Esc` are understood too.
pub fn stroke(name: &str, modifiers: u8) -> Option<KeyStroke> {
    let name = match name {
        "Space" => " ",
        "Esc" => "Escape",
        "Return" => "Enter",
        "Up" => "ArrowUp",
        "Down" => "ArrowDown",
        "Left" => "ArrowLeft",
        "Right" => "ArrowRight",
        "Ctrl" => "Control",
        "Cmd" | "Command" | "Super" => "Meta",
        other => other,
    };
    if let Some(&(key, code, key_code, webdriver, text)) = NAMED
        .iter()
        .find(|(key, ..)| key.eq_ignore_ascii_case(name))
    {
        return Some(KeyStroke {
            key: key.to_owned(),
            code: code.to_owned(),
            text: text.map(str::to_owned),
            key_code,
            webdriver,
            modifiers,
        });
    }
    let mut chars = name.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some(char_stroke(c, modifiers))
}

/// The key that types `c`.
pub fn char_stroke(c: char, modifiers: u8) -> KeyStroke {
    let (code, key_code) = match c {
        'a'..='z' | 'A'..='Z' => (
            format!("Key{}", c.to_ascii_uppercase()),
            c.to_ascii_uppercase() as u32,
        ),
        '0'..='9' => (format!("Digit{c}"), c as u32),
        ' ' => ("Space".to_owned(), 32),
        _ => (String::new(), 0),
    };
    // A key with Control, Alt or Meta held is a shortcut and types nothing.
    let typed = modifiers & (CONTROL | ALT | META) == 0;
    KeyStroke {
        key: c.to_string(),
        code,
        text: typed.then(|| c.to_string()),
        key_code,
        webdriver: c,
        modifiers,
    }
}

/// `Control+Shift+a` as its modifier strokes and its main key. Each part is
/// a key name [`stroke`] knows.
pub fn chord(spec: &str) -> Result<(Vec<KeyStroke>, KeyStroke), String> {
    let parts: Vec<&str> = if spec == "+" {
        vec!["+"]
    } else if let Some(rest) = spec.strip_suffix("++") {
        rest.split('+').chain(["+"]).collect()
    } else {
        spec.split('+').collect()
    };
    let (last, held) = parts.split_last().ok_or("no key given")?;
    let mut modifiers = 0;
    let mut strokes = Vec::new();
    for name in held {
        let bit = match stroke(name, 0).map(|s| s.key) {
            Some(key) if key == "Shift" => SHIFT,
            Some(key) if key == "Control" => CONTROL,
            Some(key) if key == "Alt" => ALT,
            Some(key) if key == "Meta" => META,
            _ => return Err(format!("{name} in {spec} is no modifier")),
        };
        modifiers |= bit;
        strokes.push(stroke(name, modifiers).expect("checked above"));
    }
    let key = stroke(last, modifiers).ok_or_else(|| format!("unknown key {last}"))?;
    Ok((strokes, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_keys_and_characters() {
        let enter = stroke("Enter", 0).unwrap();
        assert_eq!((enter.key_code, enter.webdriver), (13, '\u{E007}'));
        assert_eq!(enter.text.as_deref(), Some("\r"));
        assert_eq!(stroke("Esc", 0).unwrap().key, "Escape");
        let a = stroke("a", 0).unwrap();
        assert_eq!(
            (a.code.as_str(), a.key_code, a.text.as_deref()),
            ("KeyA", 65, Some("a"))
        );
        assert_eq!(stroke("Space", 0).unwrap().key, " ");
        assert!(stroke("Nonsense", 0).is_none());
    }

    #[test]
    fn chords_hold_modifiers_and_type_nothing() {
        let (held, key) = chord("Control+Shift+a").unwrap();
        assert_eq!(held.len(), 2);
        assert_eq!(key.modifiers, CONTROL | SHIFT);
        assert_eq!(key.text, None);
        let (held, key) = chord("Shift++").unwrap();
        assert_eq!((held.len(), key.key.as_str()), (1, "+"));
        assert!(chord("a+b").is_err());
    }
}
