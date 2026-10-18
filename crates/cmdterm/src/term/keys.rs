//! Translates SchnellUI raw key events into cmdterm-vt key codes. The
//! emulator then encodes them for the application's current modes (cursor
//! keys, kitty keyboard protocol, modifyOtherKeys).

use cmdterm_vt::{KeyCode, Mods};
use schnellui::raw_keyboard::{Key, NamedKey};
use schnellui::{RawKeyEvent, RawModifiers};

/// Returns `None` for keys without a key code (bare modifiers, media keys)
/// and for multi-character text, which the caller writes verbatim.
pub fn translate(event: &RawKeyEvent) -> Option<(KeyCode, Mods)> {
    let mut mods = modifiers(event.modifiers);

    let code = match &event.logical_key {
        Key::Named(named) => named_key(*named)?,
        Key::Character(text) => {
            // With Ctrl or Alt held, encode the unmodified key and let the emulator
            // apply the modifier, so Ctrl+C becomes ETX on every layout.
            let source = if mods.intersects(Mods::CTRL | Mods::ALT) {
                match &event.key_without_modifiers {
                    Key::Character(base) => base.as_str(),
                    _ => text.as_str(),
                }
            } else {
                text.as_str()
            };
            let mut chars = source.chars();
            let first = chars.next()?;
            if chars.next().is_some() {
                // Composed text: the caller writes `event.text` verbatim.
                return None;
            } else {
                // Shift is already folded into the character.
                if !first.is_alphabetic() || first.is_uppercase() {
                    mods = mods.without(Mods::SHIFT);
                }
                KeyCode::Char(first)
            }
        }
        _ => return None,
    };
    Some((code, mods))
}

fn named_key(key: NamedKey) -> Option<KeyCode> {
    Some(match key {
        NamedKey::Enter => KeyCode::Enter,
        NamedKey::Tab => KeyCode::Tab,
        NamedKey::Space => KeyCode::Char(' '),
        NamedKey::Backspace => KeyCode::Backspace,
        NamedKey::Escape => KeyCode::Escape,
        NamedKey::Delete => KeyCode::Delete,
        NamedKey::Insert => KeyCode::Insert,
        NamedKey::Home => KeyCode::Home,
        NamedKey::End => KeyCode::End,
        NamedKey::PageUp => KeyCode::PageUp,
        NamedKey::PageDown => KeyCode::PageDown,
        NamedKey::ArrowLeft => KeyCode::Left,
        NamedKey::ArrowRight => KeyCode::Right,
        NamedKey::ArrowUp => KeyCode::Up,
        NamedKey::ArrowDown => KeyCode::Down,
        NamedKey::F1 => KeyCode::F(1),
        NamedKey::F2 => KeyCode::F(2),
        NamedKey::F3 => KeyCode::F(3),
        NamedKey::F4 => KeyCode::F(4),
        NamedKey::F5 => KeyCode::F(5),
        NamedKey::F6 => KeyCode::F(6),
        NamedKey::F7 => KeyCode::F(7),
        NamedKey::F8 => KeyCode::F(8),
        NamedKey::F9 => KeyCode::F(9),
        NamedKey::F10 => KeyCode::F(10),
        NamedKey::F11 => KeyCode::F(11),
        NamedKey::F12 => KeyCode::F(12),
        _ => return None,
    })
}

/// Converts SchnellUI modifier state to the emulator's.
pub fn modifiers(raw: RawModifiers) -> Mods {
    let mut mods = Mods::NONE;
    if raw.shift {
        mods |= Mods::SHIFT;
    }
    if raw.control {
        mods |= Mods::CTRL;
    }
    if raw.alt {
        mods |= Mods::ALT;
    }
    if raw.super_key {
        mods |= Mods::SUPER;
    }
    mods
}

#[cfg(test)]
mod tests {
    use super::*;
    use cmdterm_vt::{Config, KeyEventKind, Size, Terminal};
    use schnellui::RawInputState;
    use schnellui::raw_keyboard::{KeyCode as Physical, KeyLocation, PhysicalKey};

    fn event(logical: Key, base: Key, modifiers: RawModifiers) -> RawKeyEvent {
        RawKeyEvent {
            logical_key: logical,
            physical_key: PhysicalKey::Code(Physical::KeyA),
            key_without_modifiers: base,
            location: KeyLocation::Standard,
            modifiers,
            state: RawInputState::Pressed,
            repeat: false,
            text: None,
            text_with_all_modifiers: None,
        }
    }

    fn named(key: NamedKey) -> RawKeyEvent {
        event(Key::Named(key), Key::Named(key), RawModifiers::default())
    }

    fn character(text: &str, modifiers: RawModifiers, base: &str) -> RawKeyEvent {
        event(
            Key::Character(text.into()),
            Key::Character(base.into()),
            modifiers,
        )
    }

    #[test]
    fn every_named_key_maps_to_a_key_code() {
        let table = [
            (NamedKey::Enter, KeyCode::Enter),
            (NamedKey::Tab, KeyCode::Tab),
            (NamedKey::Space, KeyCode::Char(' ')),
            (NamedKey::Backspace, KeyCode::Backspace),
            (NamedKey::Escape, KeyCode::Escape),
            (NamedKey::Delete, KeyCode::Delete),
            (NamedKey::Insert, KeyCode::Insert),
            (NamedKey::Home, KeyCode::Home),
            (NamedKey::End, KeyCode::End),
            (NamedKey::PageUp, KeyCode::PageUp),
            (NamedKey::PageDown, KeyCode::PageDown),
            (NamedKey::ArrowLeft, KeyCode::Left),
            (NamedKey::ArrowRight, KeyCode::Right),
            (NamedKey::ArrowUp, KeyCode::Up),
            (NamedKey::ArrowDown, KeyCode::Down),
            (NamedKey::F1, KeyCode::F(1)),
            (NamedKey::F2, KeyCode::F(2)),
            (NamedKey::F3, KeyCode::F(3)),
            (NamedKey::F4, KeyCode::F(4)),
            (NamedKey::F5, KeyCode::F(5)),
            (NamedKey::F6, KeyCode::F(6)),
            (NamedKey::F7, KeyCode::F(7)),
            (NamedKey::F8, KeyCode::F(8)),
            (NamedKey::F9, KeyCode::F(9)),
            (NamedKey::F10, KeyCode::F(10)),
            (NamedKey::F11, KeyCode::F(11)),
            (NamedKey::F12, KeyCode::F(12)),
        ];
        for (key, code) in table {
            assert_eq!(translate(&named(key)), Some((code, Mods::NONE)), "{key:?}");
        }
        assert_eq!(translate(&named(NamedKey::Shift)), None);
        assert_eq!(translate(&named(NamedKey::AudioVolumeUp)), None);
    }

    #[test]
    fn modifiers_map_one_to_one() {
        let all = RawModifiers {
            shift: true,
            control: true,
            alt: true,
            super_key: true,
        };
        assert_eq!(
            modifiers(all),
            Mods::SHIFT | Mods::CTRL | Mods::ALT | Mods::SUPER
        );
        assert_eq!(modifiers(RawModifiers::default()), Mods::NONE);
        let mut event = named(NamedKey::F5);
        event.modifiers = all;
        assert_eq!(
            translate(&event),
            Some((
                KeyCode::F(5),
                Mods::SHIFT | Mods::CTRL | Mods::ALT | Mods::SUPER
            ))
        );
    }

    #[test]
    fn characters_fold_shift_and_use_the_base_key_with_ctrl_or_alt() {
        let none = RawModifiers::default();
        let shift = RawModifiers {
            shift: true,
            ..none
        };
        let ctrl = RawModifiers {
            control: true,
            ..none
        };
        assert_eq!(
            translate(&character("a", none, "a")),
            Some((KeyCode::Char('a'), Mods::NONE))
        );
        // Shift is already part of an uppercase letter or a symbol.
        assert_eq!(
            translate(&character("A", shift, "a")),
            Some((KeyCode::Char('A'), Mods::NONE))
        );
        assert_eq!(
            translate(&character("!", shift, "1")),
            Some((KeyCode::Char('!'), Mods::NONE))
        );
        // Ctrl encodes the key without modifiers, whatever the layout typed.
        assert_eq!(
            translate(&character("\u{3}", ctrl, "c")),
            Some((KeyCode::Char('c'), Mods::CTRL))
        );
        // Composed text is left to the caller.
        assert_eq!(translate(&character("ab", none, "a")), None);
        // Other key kinds have no code.
        assert_eq!(
            translate(&event(Key::Dead(None), Key::Dead(None), none)),
            None
        );
    }

    /// What the key table sends to a program in its default modes.
    #[test]
    fn translated_keys_encode_the_way_programs_expect() {
        let term = Terminal::new(Size { rows: 2, cols: 10 }, Config::default());
        let encode = |event: RawKeyEvent| {
            let (code, mods) = translate(&event).unwrap();
            let mut out = Vec::new();
            term.encode_key(code, mods, KeyEventKind::Press, None, &mut out);
            out
        };
        let none = RawModifiers::default();
        let ctrl = RawModifiers {
            control: true,
            ..none
        };
        assert_eq!(encode(named(NamedKey::Enter)), b"\r");
        assert_eq!(encode(named(NamedKey::Tab)), b"\t");
        assert_eq!(encode(named(NamedKey::Backspace)), b"\x7f");
        assert_eq!(encode(named(NamedKey::Escape)), b"\x1b");
        assert_eq!(encode(named(NamedKey::Delete)), b"\x1b[3~");
        assert_eq!(encode(named(NamedKey::ArrowUp)), b"\x1b[A");
        assert_eq!(encode(named(NamedKey::F1)), b"\x1bOP");
        assert_eq!(encode(character("c", ctrl, "c")), b"\x03");
        assert_eq!(encode(character("x", none, "x")), b"x");
        let mut shift_tab = named(NamedKey::Tab);
        shift_tab.modifiers.shift = true;
        assert_eq!(encode(shift_tab), b"\x1b[Z");
    }
}
