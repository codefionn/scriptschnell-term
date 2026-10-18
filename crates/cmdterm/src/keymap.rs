//! App keybindings: the presets modeled on other terminals, the bindings
//! extensions ask for, and the user's own from `settings.json`.
//!
//! A binding maps a key sequence of up to [`MAX_CHORDS`] chords to an
//! [`Action`]. Later layers win: preset, then extensions, then the user.
//! [`Keymap::press`] runs on every key a terminal or web pane receives, so it
//! compares fixed-size values and never allocates.

use std::collections::BTreeMap;
use std::fmt;

use crate::ext::CommandInfo;
use crate::state::{DEFAULT_KEYMAP, SplitAxis};
use schnellui::raw_keyboard::{Key, NamedKey};
use schnellui::{RawInputState, RawKeyEvent, Shortcut};

use crate::workspace::{MAX_PANES, Workspace};

/// Most chords in one sequence, such as tmux's `Ctrl+B %`.
pub const MAX_CHORDS: usize = 3;

const CTRL: u8 = 1;
const SHIFT: u8 = 2;
const ALT: u8 = 4;
const SUPER: u8 = 8;

/// Named keys a binding can use: the name in `settings.json`, the name shown
/// to the user, and the key.
const NAMED: [(&str, &str, NamedKey); 28] = [
    ("enter", "Enter", NamedKey::Enter),
    ("tab", "Tab", NamedKey::Tab),
    ("space", "Space", NamedKey::Space),
    ("backspace", "Backspace", NamedKey::Backspace),
    ("escape", "Escape", NamedKey::Escape),
    ("delete", "Delete", NamedKey::Delete),
    ("insert", "Insert", NamedKey::Insert),
    ("home", "Home", NamedKey::Home),
    ("end", "End", NamedKey::End),
    ("pageup", "PageUp", NamedKey::PageUp),
    ("pagedown", "PageDown", NamedKey::PageDown),
    ("left", "Left", NamedKey::ArrowLeft),
    ("right", "Right", NamedKey::ArrowRight),
    ("up", "Up", NamedKey::ArrowUp),
    ("down", "Down", NamedKey::ArrowDown),
    ("f1", "F1", NamedKey::F1),
    ("f2", "F2", NamedKey::F2),
    ("f3", "F3", NamedKey::F3),
    ("f4", "F4", NamedKey::F4),
    ("f5", "F5", NamedKey::F5),
    ("f6", "F6", NamedKey::F6),
    ("f7", "F7", NamedKey::F7),
    ("f8", "F8", NamedKey::F8),
    ("f9", "F9", NamedKey::F9),
    ("f10", "F10", NamedKey::F10),
    ("f11", "F11", NamedKey::F11),
    ("f12", "F12", NamedKey::F12),
    ("esc", "Escape", NamedKey::Escape),
];

/// Key names that stand for a character, for keys that are awkward to write.
const CHAR_NAMES: [(&str, char); 3] = [("plus", '+'), ("minus", '-'), ("comma", ',')];

/// A key without modifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyId {
    /// A character key, lowercased.
    Char(char),
    Named(NamedKey),
    /// A key no binding can name.
    Other,
}

/// One key with its modifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    pub key: KeyId,
    mods: u8,
}

impl Chord {
    pub fn ctrl(self) -> bool {
        self.mods & CTRL != 0
    }

    pub fn shift(self) -> bool {
        self.mods & SHIFT != 0
    }

    pub fn alt(self) -> bool {
        self.mods & ALT != 0
    }

    /// The chord in the syntax of `settings.json`, such as `ctrl+shift+p`;
    /// `None` for a key no binding can name.
    pub fn settings_name(self) -> Option<String> {
        let mut out = String::new();
        for (bit, name) in [
            (CTRL, "ctrl+"),
            (ALT, "alt+"),
            (SHIFT, "shift+"),
            (SUPER, "super+"),
        ] {
            if self.mods & bit != 0 {
                out.push_str(name);
            }
        }
        match self.key {
            KeyId::Char(' ') => out.push_str("space"),
            KeyId::Char(c) => match CHAR_NAMES.iter().find(|(_, named)| *named == c) {
                Some((name, _)) => out.push_str(name),
                None => out.push(c),
            },
            KeyId::Named(named) => {
                let (name, ..) = NAMED.iter().find(|(.., key)| *key == named)?;
                out.push_str(name);
            }
            KeyId::Other => return None,
        }
        Some(out)
    }

    /// Parses one chord such as `ctrl+shift+p`, `alt+plus` or `%`.
    fn parse(text: &str) -> Result<Self, String> {
        let mut rest = text;
        let mut mods = 0;
        while let Some((head, tail)) = rest.split_once('+') {
            // `ctrl++` names the plus key: the tail is the key itself.
            if tail.is_empty() {
                break;
            }
            let bit = match head.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => CTRL,
                "shift" => SHIFT,
                "alt" | "option" => ALT,
                "super" | "meta" | "cmd" | "win" => SUPER,
                _ => break,
            };
            mods |= bit;
            rest = tail;
        }
        let lower = rest.to_ascii_lowercase();
        let key = if let Some((_, _, named)) = NAMED.iter().find(|(name, ..)| *name == lower) {
            KeyId::Named(*named)
        } else if let Some((_, c)) = CHAR_NAMES.iter().find(|(name, _)| *name == lower) {
            KeyId::Char(*c)
        } else {
            let mut chars = rest.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => KeyId::Char(lowercase(c)),
                _ => return Err(format!("unknown key '{rest}' in '{text}'")),
            }
        };
        Ok(Self { key, mods })
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (bit, name) in [
            (CTRL, "Ctrl+"),
            (ALT, "Alt+"),
            (SHIFT, "Shift+"),
            (SUPER, "Super+"),
        ] {
            if self.mods & bit != 0 {
                f.write_str(name)?;
            }
        }
        match self.key {
            KeyId::Char(' ') => f.write_str("Space"),
            KeyId::Char(c) => write!(f, "{}", c.to_uppercase()),
            KeyId::Named(named) => {
                let name = NAMED
                    .iter()
                    .find(|(.., key)| *key == named)
                    .map_or("?", |(_, shown, _)| shown);
                f.write_str(name)
            }
            KeyId::Other => f.write_str("?"),
        }
    }
}

/// A sequence of chords. Fixed size, so matching a key copies no heap data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Keys {
    chords: [Chord; MAX_CHORDS],
    len: u8,
}

impl Keys {
    const EMPTY: Self = Self {
        chords: [Chord {
            key: KeyId::Other,
            mods: 0,
        }; MAX_CHORDS],
        len: 0,
    };

    /// Parses a sequence of chords separated by spaces, such as `ctrl+b %`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut keys = Self::EMPTY;
        for part in text.split_whitespace() {
            if keys.len() == MAX_CHORDS {
                return Err(format!("'{text}' has more than {MAX_CHORDS} chords"));
            }
            keys.push(Chord::parse(part)?);
        }
        if keys.len() == 0 {
            return Err("empty key sequence".to_owned());
        }
        Ok(keys)
    }

    pub fn chords(&self) -> &[Chord] {
        &self.chords[..self.len()]
    }

    pub fn len(&self) -> usize {
        usize::from(self.len)
    }

    fn push(&mut self, chord: Chord) {
        self.chords[self.len()] = chord;
        self.len += 1;
    }

    fn starts_with(&self, prefix: &Keys) -> bool {
        self.len >= prefix.len && self.chords[..prefix.len()] == *prefix.chords()
    }
}

impl Keys {
    /// The sequence in the syntax of `settings.json`, such as `ctrl+b %`.
    pub fn to_settings(self) -> String {
        let mut out = String::new();
        for (index, chord) in self.chords().iter().enumerate() {
            if index > 0 {
                out.push(' ');
            }
            out.push_str(&chord.settings_name().unwrap_or_default());
        }
        out
    }
}

impl fmt::Display for Keys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, chord) in self.chords().iter().enumerate() {
            if index > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{chord}")?;
        }
        Ok(())
    }
}

/// What a binding does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Palette,
    Settings,
    /// Focuses the rail's search field.
    SearchWorktrees,
    /// Hides the rail, or shows it again.
    ToggleSidebar,
    /// Moves focus from the panel to the shown tab in the tab strip, then
    /// on through the status bar and the rail.
    FocusTabs,
    /// Moves focus through the same stops the other way round.
    FocusBack,
    NewTerminal,
    /// Opens the file explorer of the shown worktree.
    Files,
    /// Opens the history of the shown worktree.
    Explore,
    /// Opens the review of the shown worktree's changes against its base.
    Review,
    /// Opens the monitor of the shown worktree's machine.
    Monitor,
    /// Opens a panel on the app's debug browser.
    DebugBrowser,
    /// Opens the tasks panel: every panel of every worktree, the
    /// extension tasks and the app's threads.
    Tasks,
    /// Stops the panel in the focused pane, which closes the pane.
    ClosePanel,
    /// The same as [`Action::ClosePanel`], under the name pane-minded
    /// presets use.
    ClosePane,
    /// Stops every panel of the shown tab.
    CloseTab,
    /// Moves the focused pane of a split tab into a tab of its own.
    MovePaneToTab,
    Split(SplitAxis),
    NextPane,
    PreviousPane,
    /// Grows the focused pane of a split tab.
    GrowPane,
    /// Shrinks the focused pane of a split tab.
    ShrinkPane,
    NextTab,
    PreviousTab,
    /// Shows tab `n` (from 1) in the focused pane.
    Tab(u8),
    /// Focuses pane `n` (from 1).
    Pane(u8),
    /// Writes text to the focused terminal, such as tmux's prefix key.
    SendText(String),
    /// Runs an extension's palette command.
    Command(String),
}

/// Thousandths of a split one grow or shrink moves its edge by.
pub const PANE_STEP: i32 = 50;

/// Actions that take no argument, by their name in `settings.json`.
const SIMPLE_ACTIONS: [(&str, Action); 24] = [
    ("palette", Action::Palette),
    ("settings", Action::Settings),
    ("search_worktrees", Action::SearchWorktrees),
    ("toggle_sidebar", Action::ToggleSidebar),
    ("focus_tabs", Action::FocusTabs),
    ("focus_back", Action::FocusBack),
    ("new_terminal", Action::NewTerminal),
    ("files", Action::Files),
    ("explore", Action::Explore),
    ("review", Action::Review),
    ("monitor", Action::Monitor),
    ("debug_browser", Action::DebugBrowser),
    ("tasks", Action::Tasks),
    ("close_panel", Action::ClosePanel),
    ("close_pane", Action::ClosePane),
    ("close_tab", Action::CloseTab),
    ("move_pane_to_tab", Action::MovePaneToTab),
    ("split_side_by_side", Action::Split(SplitAxis::SideBySide)),
    ("split_stacked", Action::Split(SplitAxis::Stacked)),
    ("focus_next_pane", Action::NextPane),
    ("focus_previous_pane", Action::PreviousPane),
    ("grow_pane", Action::GrowPane),
    ("shrink_pane", Action::ShrinkPane),
    ("next_tab", Action::NextTab),
];

/// The actions without an argument, which the Keyboard page lists even
/// while no key is bound to them.
pub fn simple_actions() -> impl Iterator<Item = Action> {
    SIMPLE_ACTIONS.iter().map(|(_, action)| action.clone())
}

impl Action {
    /// The action's name in `settings.json`, the inverse of [`Action::parse`].
    pub fn settings_name(&self) -> String {
        if let Some((name, _)) = SIMPLE_ACTIONS.iter().find(|(_, action)| action == self) {
            return (*name).to_owned();
        }
        match self {
            Action::PreviousTab => "previous_tab".to_owned(),
            Action::SendText(text) => format!("send_text:{text}"),
            Action::Command(command) => format!("command:{command}"),
            Action::Tab(n) => format!("tab_{n}"),
            Action::Pane(n) => format!("pane_{n}"),
            // Every other action is a simple one.
            _ => String::new(),
        }
    }

    /// Parses an action name from `settings.json`: one of the simple names,
    /// `previous_tab`, `tab_1` to `tab_9`, `pane_1` to `pane_8`,
    /// `send_text:<text>` or `command:<extension command>`.
    pub fn parse(name: &str) -> Result<Self, String> {
        if let Some((_, action)) = SIMPLE_ACTIONS.iter().find(|(n, _)| *n == name) {
            return Ok(action.clone());
        }
        if name == "previous_tab" {
            return Ok(Action::PreviousTab);
        }
        if let Some(text) = name.strip_prefix("send_text:") {
            return Ok(Action::SendText(text.to_owned()));
        }
        if let Some(command) = name.strip_prefix("command:")
            && !command.is_empty()
        {
            return Ok(Action::Command(command.to_owned()));
        }
        let numbered = |prefix: &str, max: usize| {
            let n: u8 = name.strip_prefix(prefix)?.parse().ok()?;
            (1..=max).contains(&usize::from(n)).then_some(n)
        };
        if let Some(n) = numbered("tab_", 9) {
            return Ok(Action::Tab(n));
        }
        if let Some(n) = numbered("pane_", MAX_PANES) {
            return Ok(Action::Pane(n));
        }
        Err(format!("unknown action '{name}'"))
    }
}

/// A keymap preset modeled on another program's defaults.
pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    /// Key sequences and action names, in the syntax of `settings.json`, in
    /// layers: a preset built on another lists that one's bindings first.
    pub bindings: &'static [&'static [(&'static str, &'static str)]],
}

impl Preset {
    /// Every binding of every layer, in order.
    pub fn entries(&self) -> impl Iterator<Item = &'static (&'static str, &'static str)> {
        self.bindings.iter().flat_map(|layer| layer.iter())
    }
}

/// VS Code's own keys for its integrated terminal and editor groups.
const VSCODE: &[(&str, &str)] = &[
    ("ctrl+shift+p", "palette"),
    ("f1", "palette"),
    ("ctrl+,", "settings"),
    ("f6", "focus_tabs"),
    ("shift+f6", "focus_back"),
    ("ctrl+shift+`", "new_terminal"),
    ("ctrl+shift+e", "files"),
    ("ctrl+shift+g", "explore"),
    ("ctrl+f4", "close_tab"),
    ("ctrl+shift+5", "split_side_by_side"),
    ("ctrl+pagedown", "next_tab"),
    ("ctrl+pageup", "previous_tab"),
    ("ctrl+1", "pane_1"),
    ("ctrl+2", "pane_2"),
    ("ctrl+3", "pane_3"),
    ("ctrl+4", "pane_4"),
    ("ctrl+5", "pane_5"),
    ("ctrl+6", "pane_6"),
    ("ctrl+7", "pane_7"),
    ("ctrl+8", "pane_8"),
    ("alt+1", "tab_1"),
    ("alt+2", "tab_2"),
    ("alt+3", "tab_3"),
    ("alt+4", "tab_4"),
    ("alt+5", "tab_5"),
    ("alt+6", "tab_6"),
    ("alt+7", "tab_7"),
    ("alt+8", "tab_8"),
    ("alt+9", "tab_9"),
];

/// Every preset, the default first.
pub const PRESETS: [Preset; 6] = [
    Preset {
        id: DEFAULT_KEYMAP,
        name: "scriptschnell-term",
        description: "Uses Ctrl+Shift, so every Ctrl+letter key still reaches the shell",
        bindings: &[&[
            ("ctrl+shift+p", "palette"),
            ("ctrl+,", "settings"),
            ("ctrl+shift+s", "search_worktrees"),
            ("ctrl+shift+b", "toggle_sidebar"),
            ("f6", "focus_tabs"),
            ("shift+f6", "focus_back"),
            ("ctrl+shift+t", "new_terminal"),
            ("ctrl+shift+f", "files"),
            ("ctrl+shift+h", "explore"),
            ("ctrl+shift+r", "review"),
            ("ctrl+shift+w", "close_tab"),
            ("ctrl+shift+d", "split_side_by_side"),
            ("ctrl+shift+e", "split_stacked"),
            ("ctrl+shift+o", "focus_next_pane"),
            ("ctrl+shift+i", "focus_previous_pane"),
            ("ctrl+shift+alt+right", "grow_pane"),
            ("ctrl+shift+alt+left", "shrink_pane"),
            ("ctrl+shift+x", "close_pane"),
            ("ctrl+shift+m", "move_pane_to_tab"),
            ("ctrl+pagedown", "next_tab"),
            ("ctrl+pageup", "previous_tab"),
        ]],
    },
    Preset {
        id: "tmux",
        name: "tmux",
        description: "Ctrl+B, then a key. Ctrl+B twice sends Ctrl+B to the shell",
        bindings: &[&[
            ("ctrl+b :", "palette"),
            ("ctrl+,", "settings"),
            ("ctrl+b c", "new_terminal"),
            ("ctrl+b f", "files"),
            ("ctrl+b g", "explore"),
            ("ctrl+b r", "review"),
            ("ctrl+b x", "close_pane"),
            ("ctrl+b &", "close_tab"),
            ("ctrl+b !", "move_pane_to_tab"),
            ("ctrl+b %", "split_side_by_side"),
            ("ctrl+b \"", "split_stacked"),
            ("ctrl+b o", "focus_next_pane"),
            ("ctrl+b ;", "focus_previous_pane"),
            ("ctrl+b right", "focus_next_pane"),
            ("ctrl+b down", "focus_next_pane"),
            ("ctrl+b left", "focus_previous_pane"),
            ("ctrl+b up", "focus_previous_pane"),
            ("ctrl+b n", "next_tab"),
            ("ctrl+b p", "previous_tab"),
            ("ctrl+b 0", "tab_1"),
            ("ctrl+b 1", "tab_2"),
            ("ctrl+b 2", "tab_3"),
            ("ctrl+b 3", "tab_4"),
            ("ctrl+b 4", "tab_5"),
            ("ctrl+b 5", "tab_6"),
            ("ctrl+b 6", "tab_7"),
            ("ctrl+b 7", "tab_8"),
            ("ctrl+b 8", "tab_9"),
            ("ctrl+b ctrl+b", "send_text:\u{2}"),
        ]],
    },
    Preset {
        id: "vscode",
        name: "VS Code",
        description: "The integrated terminal's keys: Ctrl+Shift+` for a terminal, Ctrl+Shift+5 to split",
        bindings: &[VSCODE],
    },
    Preset {
        id: "cursor",
        name: "Cursor",
        description: "VS Code's keys plus Ctrl+Shift+J for settings",
        bindings: &[VSCODE, &[("ctrl+shift+j", "settings")]],
    },
    Preset {
        id: "windows-terminal",
        name: "Windows Terminal",
        description: "Alt+Shift+Plus and Alt+Shift+Minus split, Alt+arrows move between panes",
        bindings: &[&[
            ("ctrl+shift+p", "palette"),
            ("ctrl+,", "settings"),
            ("ctrl+shift+t", "new_terminal"),
            ("ctrl+shift+w", "close_pane"),
            ("alt+shift+plus", "split_side_by_side"),
            ("alt+shift+minus", "split_stacked"),
            ("alt+right", "focus_next_pane"),
            ("alt+down", "focus_next_pane"),
            ("alt+left", "focus_previous_pane"),
            ("alt+up", "focus_previous_pane"),
            ("ctrl+tab", "next_tab"),
            ("ctrl+shift+tab", "previous_tab"),
            ("ctrl+alt+1", "tab_1"),
            ("ctrl+alt+2", "tab_2"),
            ("ctrl+alt+3", "tab_3"),
            ("ctrl+alt+4", "tab_4"),
            ("ctrl+alt+5", "tab_5"),
            ("ctrl+alt+6", "tab_6"),
            ("ctrl+alt+7", "tab_7"),
            ("ctrl+alt+8", "tab_8"),
            ("ctrl+alt+9", "tab_9"),
        ]],
    },
    Preset {
        id: "konsole",
        name: "Konsole",
        description: "Ctrl+( and Ctrl+) split, Shift+arrows switch tabs",
        bindings: &[&[
            ("ctrl+alt+i", "palette"),
            ("ctrl+,", "settings"),
            ("ctrl+shift+t", "new_terminal"),
            ("ctrl+shift+w", "close_panel"),
            ("ctrl+(", "split_side_by_side"),
            ("ctrl+*", "split_side_by_side"),
            ("ctrl+)", "split_stacked"),
            ("ctrl+f11", "focus_next_pane"),
            ("ctrl+shift+f11", "focus_previous_pane"),
            ("ctrl+shift+right", "focus_next_pane"),
            ("ctrl+shift+down", "focus_next_pane"),
            ("ctrl+shift+left", "focus_previous_pane"),
            ("ctrl+shift+up", "focus_previous_pane"),
            ("shift+right", "next_tab"),
            ("ctrl+pagedown", "next_tab"),
            ("shift+left", "previous_tab"),
            ("ctrl+pageup", "previous_tab"),
            ("alt+1", "tab_1"),
            ("alt+2", "tab_2"),
            ("alt+3", "tab_3"),
            ("alt+4", "tab_4"),
            ("alt+5", "tab_5"),
            ("alt+6", "tab_6"),
            ("alt+7", "tab_7"),
            ("alt+8", "tab_8"),
            ("alt+9", "tab_9"),
        ]],
    },
];

/// The preset with `id`, if there is one.
pub fn preset(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|preset| preset.id == id)
}

/// Where a binding came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Preset,
    /// The extension that registered the command.
    Extension(String),
    User,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub keys: Keys,
    pub action: Action,
    pub source: Source,
}

/// What [`Keymap::press`] made of a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    /// No binding wants the key; it goes to the panel.
    Ignored,
    /// The key started or continued a sequence, or ended one no binding has.
    Consumed,
    /// The key completed the binding at this index.
    Run(usize),
}

/// The bindings in effect and the sequence typed so far.
pub struct Keymap {
    pub preset: &'static Preset,
    pub bindings: Vec<Binding>,
    pending: Keys,
}

impl Default for Keymap {
    fn default() -> Self {
        Self::build(DEFAULT_KEYMAP, &[], &BTreeMap::new()).0
    }
}

impl Keymap {
    /// Layers the preset `preset_id`, the extensions' key requests and the
    /// user's bindings. Returns the keymap and one message per entry it could
    /// not use.
    pub fn build(
        preset_id: &str,
        commands: &[CommandInfo],
        user: &BTreeMap<String, Option<String>>,
    ) -> (Self, Vec<String>) {
        let mut errors = Vec::new();
        let preset = preset(preset_id).unwrap_or_else(|| {
            errors.push(format!(
                "unknown keymap '{preset_id}', using {DEFAULT_KEYMAP}"
            ));
            &PRESETS[0]
        });
        let mut bindings: Vec<Binding> = Vec::new();
        for (keys, action) in preset.entries() {
            // The presets are checked by tests, so these never fail.
            if let (Ok(keys), Ok(action)) = (Keys::parse(keys), Action::parse(action)) {
                put(&mut bindings, keys, action, Source::Preset);
            }
        }

        for command in commands {
            let request = command
                .keys
                .iter()
                .find(|r| r.keymap.as_deref() == Some(preset.id))
                .or_else(|| command.keys.iter().find(|r| r.keymap.is_none()));
            let Some(request) = request else {
                continue;
            };
            let keys = match Keys::parse(&request.keys) {
                Ok(keys) => keys,
                Err(error) => {
                    errors.push(format!("extension {}: {error}", command.extension));
                    continue;
                }
            };
            if let Some(taken) = bindings.iter().find(|b| b.keys == keys)
                && let Source::Extension(owner) = &taken.source
            {
                errors.push(format!(
                    "extension {}: {keys} is already bound by extension {owner}",
                    command.extension
                ));
                continue;
            }
            put(
                &mut bindings,
                keys,
                Action::Command(command.name.clone()),
                Source::Extension(command.extension.clone()),
            );
        }

        for (keys, action) in user {
            let parsed = Keys::parse(keys).and_then(|keys| {
                let action = action.as_deref().map(Action::parse).transpose()?;
                Ok((keys, action))
            });
            match parsed {
                Ok((keys, Some(action))) => put(&mut bindings, keys, action, Source::User),
                Ok((keys, None)) => bindings.retain(|b| b.keys != keys),
                Err(error) => errors.push(format!("keybinding '{keys}': {error}")),
            }
        }

        let keymap = Self {
            preset,
            bindings,
            pending: Keys::EMPTY,
        };
        (keymap, errors)
    }

    /// Matches one key against the bindings. Allocation-free: it runs for
    /// every key a terminal receives.
    pub fn press(&mut self, event: &RawKeyEvent) -> Press {
        if event.state != RawInputState::Pressed {
            return Press::Ignored;
        }
        let Some(stroke) = Stroke::of(event) else {
            // A modifier on its own keeps the sequence going.
            return Press::Ignored;
        };
        let depth = self.pending.len();
        let mut prefix = None;
        for (index, binding) in self.bindings.iter().enumerate() {
            if binding.keys.len() <= depth || !binding.keys.starts_with(&self.pending) {
                continue;
            }
            let chord = binding.keys.chords[depth];
            if !stroke.matches(chord) {
                continue;
            }
            if binding.keys.len() == depth + 1 {
                self.pending = Keys::EMPTY;
                return Press::Run(index);
            }
            prefix.get_or_insert(chord);
        }
        if let Some(chord) = prefix {
            self.pending.push(chord);
            return Press::Consumed;
        }
        self.pending = Keys::EMPTY;
        // Like tmux, a key after a prefix that ends no sequence is dropped.
        if depth > 0 {
            Press::Consumed
        } else {
            Press::Ignored
        }
    }

    /// Whether a sequence has started and waits for its next key.
    #[cfg(test)]
    pub fn is_pending(&self) -> bool {
        self.pending.len() > 0
    }

    /// The first sequence bound to `action`, for hints in the UI.
    pub fn keys_for(&self, action: &Action) -> Option<Keys> {
        self.bindings
            .iter()
            .find(|binding| binding.action == *action)
            .map(|binding| binding.keys)
    }

    /// `text` followed by the action's keys in parentheses, if it has any.
    pub fn hint(&self, text: &str, action: &Action) -> String {
        match self.keys_for(action) {
            Some(keys) => format!("{text} ({keys})"),
            None => text.to_owned(),
        }
    }

    /// The single-chord bindings as SchnellUI shortcuts, so they also work
    /// while no terminal has focus. SchnellUI shortcuts cover character keys
    /// with Ctrl or Alt only.
    pub fn shortcuts(&self) -> impl Iterator<Item = (Shortcut, &Action)> {
        self.bindings.iter().flat_map(|binding| {
            let mut out = [None, None];
            let chord = binding.keys.chords[0];
            if let (1, KeyId::Char(c)) = (binding.keys.len(), chord.key)
                && (chord.ctrl() || chord.alt())
                && chord.mods & SUPER == 0
            {
                out[0] = Some((
                    Shortcut::new(c, chord.ctrl(), chord.shift(), chord.alt()),
                    &binding.action,
                ));
                // `ctrl+(` arrives as Shift+9 producing `(`.
                if !c.is_alphanumeric() && !chord.shift() {
                    out[1] = Some((
                        Shortcut::new(c, chord.ctrl(), true, chord.alt()),
                        &binding.action,
                    ));
                }
            }
            // A function key needs no modifier: no control types it.
            if let (1, KeyId::Named(named)) = (binding.keys.len(), chord.key)
                && chord.mods & SUPER == 0
                && let Some(number) = function_number(named)
            {
                out[0] = Some((
                    Shortcut::key(
                        schnellui::ShortcutKey::F(number),
                        chord.ctrl(),
                        chord.shift(),
                        chord.alt(),
                    ),
                    &binding.action,
                ));
            }
            out.into_iter().flatten()
        })
    }
}

/// The number of a function key, F1 to F12.
fn function_number(key: NamedKey) -> Option<u8> {
    Some(match key {
        NamedKey::F1 => 1,
        NamedKey::F2 => 2,
        NamedKey::F3 => 3,
        NamedKey::F4 => 4,
        NamedKey::F5 => 5,
        NamedKey::F6 => 6,
        NamedKey::F7 => 7,
        NamedKey::F8 => 8,
        NamedKey::F9 => 9,
        NamedKey::F10 => 10,
        NamedKey::F11 => 11,
        NamedKey::F12 => 12,
        _ => return None,
    })
}

/// Adds a binding, replacing one with the same keys.
fn put(bindings: &mut Vec<Binding>, keys: Keys, action: Action, source: Source) {
    let binding = Binding {
        keys,
        action,
        source,
    };
    match bindings.iter_mut().find(|b| b.keys == keys) {
        Some(slot) => *slot = binding,
        None => bindings.push(binding),
    }
}

fn lowercase(c: char) -> char {
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(single), None) => single,
        _ => c,
    }
}

/// A pressed key in the two forms a binding can name it by.
struct Stroke {
    /// The key as if no modifier were held: `5` for Shift+5.
    base: KeyId,
    /// The character the layout produced, when it differs: `%` for Shift+5.
    text: Option<char>,
    mods: u8,
}

impl Stroke {
    fn of(event: &RawKeyEvent) -> Option<Self> {
        let base = match &event.key_without_modifiers {
            Key::Character(text) => {
                single(text).map_or(KeyId::Other, |c| KeyId::Char(lowercase(c)))
            }
            Key::Named(
                NamedKey::Shift
                | NamedKey::Control
                | NamedKey::Alt
                | NamedKey::AltGraph
                | NamedKey::Super
                | NamedKey::Meta
                | NamedKey::Hyper,
            ) => return None,
            Key::Named(named) => NAMED
                .iter()
                .find(|(.., key)| key == named)
                .map_or(KeyId::Other, |_| KeyId::Named(*named)),
            _ => KeyId::Other,
        };
        let text = match &event.logical_key {
            Key::Character(text) => single(text)
                .map(lowercase)
                .filter(|c| KeyId::Char(*c) != base),
            _ => None,
        };
        let m = event.modifiers;
        let mut mods = 0;
        for (held, bit) in [
            (m.control, CTRL),
            (m.shift, SHIFT),
            (m.alt, ALT),
            (m.super_key, SUPER),
        ] {
            if held {
                mods |= bit;
            }
        }
        Some(Self { base, text, mods })
    }

    /// Whether this key is `chord`: the same key and modifiers, or the
    /// produced character with the same modifiers apart from Shift.
    fn matches(&self, chord: Chord) -> bool {
        if chord.key == KeyId::Other {
            return false;
        }
        if chord.key == self.base && chord.mods == self.mods {
            return true;
        }
        self.text.is_some_and(|c| {
            chord.key == KeyId::Char(c) && chord.mods & !SHIFT == self.mods & !SHIFT
        })
    }
}

fn single(text: &str) -> Option<char> {
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

/// What a key does while the settings record a new binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recorded {
    /// A modifier on its own: recording goes on.
    Waiting,
    /// Escape: recording stopped, nothing changed.
    Cancelled,
    /// The chord is bound now.
    Bound,
}

impl Workspace {
    /// Starts recording a new key for the binding of `action`, or stops
    /// with `None`. The settings show the recorder in its row.
    pub fn record_binding(&self, action: Option<Action>) {
        let mut inner = self.borrow_mut();
        // Stopping hands focus back to the row that recorded.
        if let Some(recorded) = inner.settings_ui.recording.clone() {
            inner.settings_ui.last_binding = Some(recorded);
            inner.settings_ui.binding_serial += 1;
        }
        // The recorder holds the settings' focus ref, no field.
        if action.is_some() {
            inner.settings_ui.finding_page = false;
            inner.settings_ui.finding_extension = false;
        }
        inner.settings_ui.recording = action;
        inner.dirty.settings = true;
    }

    /// Takes a key for the binding being recorded: binds it to the action
    /// in `settings.json` in place of the action's old keys, which the
    /// user's layer turns off. Cold: it runs once per recorded binding.
    pub fn record_key(&self, event: &RawKeyEvent) -> Recorded {
        if event.state != RawInputState::Pressed {
            return Recorded::Waiting;
        }
        let Some(stroke) = Stroke::of(event) else {
            return Recorded::Waiting;
        };
        let Some(action) = self.borrow().settings_ui.recording.clone() else {
            return Recorded::Cancelled;
        };
        if stroke.mods == 0 && stroke.base == KeyId::Named(NamedKey::Escape) {
            self.record_binding(None);
            return Recorded::Cancelled;
        }
        let chord = Chord {
            key: stroke.base,
            mods: stroke.mods,
        };
        let Some(name) = chord.settings_name() else {
            return Recorded::Waiting;
        };
        {
            let mut inner = self.borrow_mut();
            let inner = &mut *inner;
            let action_name = action.settings_name();
            // The action's old keys go, and so does another action on the
            // new chord, which the toast names so the user knows.
            let taken: Vec<&Binding> = inner
                .keymap
                .bindings
                .iter()
                .filter(|binding| binding.action != action && binding.keys.to_settings() == name)
                .collect();
            let conflict = taken
                .first()
                .map(|binding| (inner.tr.binding_taken)(&name, &inner.tr.action(&binding.action)));
            let old: Vec<String> = inner
                .keymap
                .bindings
                .iter()
                .filter(|binding| binding.action == action || binding.keys.to_settings() == name)
                .map(|binding| binding.keys.to_settings())
                .collect();
            let user = &mut inner.settings.keybindings;
            for keys in old {
                // A user binding goes; one from a preset or an extension is
                // turned off.
                if user.get(&keys).is_some_and(Option::is_some) {
                    user.remove(&keys);
                } else {
                    user.insert(keys, None);
                }
            }
            user.insert(name, Some(action_name));
            inner.settings_ui.recording = None;
            inner.settings_ui.last_binding = Some(action.clone());
            inner.settings_ui.binding_serial += 1;
            inner.dirty.settings = true;
            if let Some(conflict) = conflict {
                inner.toast(false, conflict);
            }
        }
        self.save_settings();
        self.rebuild_keymap();
        Recorded::Bound
    }

    /// Whether the user changed the keys of `action` in `settings.json`.
    pub fn binding_changed(&self, action: &Action) -> bool {
        let inner = self.borrow();
        !Self::user_entries(&inner.keymap, &inner.settings.keybindings, action).is_empty()
    }

    /// Puts the keys of `action` back to what the keymap and extensions
    /// give it: drops the user's keys for it and the turned-off entries
    /// that were its keys.
    pub fn reset_binding(&self, action: &Action) {
        {
            let mut inner = self.borrow_mut();
            let inner = &mut *inner;
            for keys in Self::user_entries(&inner.keymap, &inner.settings.keybindings, action) {
                inner.settings.keybindings.remove(&keys);
            }
            inner.settings_ui.last_binding = Some(action.clone());
            inner.settings_ui.binding_serial += 1;
            inner.dirty.settings = true;
        }
        self.save_settings();
        self.rebuild_keymap();
    }

    /// The `keybindings` entries that change `action`: those that bind it,
    /// and those that turn off or rebind a key the preset binds to it.
    fn user_entries(
        keymap: &Keymap,
        user: &BTreeMap<String, Option<String>>,
        action: &Action,
    ) -> Vec<String> {
        let name = action.settings_name();
        let preset_keys: Vec<Keys> = keymap
            .preset
            .entries()
            .filter(|(_, bound)| *bound == name)
            .filter_map(|(keys, _)| Keys::parse(keys).ok())
            .collect();
        user.iter()
            .filter(|(keys, bound)| {
                bound.as_deref() == Some(name.as_str())
                    || Keys::parse(keys).is_ok_and(|keys| preset_keys.contains(&keys))
            })
            .map(|(keys, _)| keys.clone())
            .collect()
    }

    /// Offers a key to the keymap.
    pub fn press_key(&self, event: &RawKeyEvent) -> Press {
        self.borrow_mut().keymap.press(event)
    }

    /// Runs the binding at `index` for `pane`, the pane whose widget had focus.
    pub fn run_binding(&self, index: usize, pane: usize) {
        let action = self
            .borrow()
            .keymap
            .bindings
            .get(index)
            .map(|binding| binding.action.clone());
        if let Some(action) = action {
            self.run_action(&action, pane);
        }
    }

    pub fn run_action(&self, action: &Action, pane: usize) {
        // A shortcut pressed while the palette is open acts on the panels
        // behind it, so the palette makes way instead of hiding the result.
        if !matches!(action, Action::Palette) {
            self.close_palette();
        }
        match action {
            Action::Palette => self.open_palette(None),
            Action::Settings => self.open_settings(),
            Action::SearchWorktrees => self.focus_rail_search(),
            Action::ToggleSidebar => self.toggle_sidebar(),
            Action::FocusTabs => self.focus_tabs(),
            Action::FocusBack => self.focus_chrome_back(),
            Action::NewTerminal => self.open_terminal(None, None),
            Action::Files => self.open_files(),
            Action::Explore => self.open_explore(false),
            Action::Review => self.open_explore(true),
            Action::Monitor => self.open_monitor(),
            Action::DebugBrowser => self.open_debug_browser(),
            Action::Tasks => self.open_tasks(),
            Action::ClosePanel => {
                if let Some(id) = self.pane_panel_id(pane) {
                    self.close_panel(id);
                }
            }
            Action::ClosePane => self.close_pane(),
            Action::Split(axis) => self.split_pane(*axis),
            Action::NextPane => self.cycle_pane(1),
            Action::PreviousPane => self.cycle_pane(-1),
            Action::GrowPane => self.resize_pane(PANE_STEP),
            Action::ShrinkPane => self.resize_pane(-PANE_STEP),
            Action::NextTab => self.cycle_tab(1),
            Action::PreviousTab => self.cycle_tab(-1),
            Action::Tab(n) => self.select_tab(usize::from(*n) - 1),
            Action::CloseTab => {
                if let Some(index) = self.with_active(|live| live.map(|live| live.active)) {
                    self.close_tab(index);
                }
            }
            Action::MovePaneToTab => self.move_pane_to_tab(),
            Action::Pane(n) => self.focus_pane(usize::from(*n) - 1),
            Action::SendText(text) => {
                if let Some(terminal) = self.pane_terminal(pane) {
                    let _ = terminal.session.write_bytes(text.as_bytes());
                }
            }
            Action::Command(name) => self.run_command(name),
        }
    }

    /// Switches to the keymap preset `id` and saves the choice.
    pub fn set_keymap(&self, id: &str) {
        {
            let mut inner = self.borrow_mut();
            if inner.settings.keymap == id {
                return;
            }
            inner.settings.keymap = id.to_owned();
        }
        self.save_settings();
        self.rebuild_keymap();
    }

    /// Builds the keymap again from the settings and the extensions'
    /// commands. Remounts when the bindings changed, because the app
    /// registers the single-chord ones as SchnellUI shortcuts.
    pub fn rebuild_keymap(&self) {
        let errors = {
            let mut inner = self.borrow_mut();
            let inner = &mut *inner;
            let (keymap, errors) = Keymap::build(
                &inner.settings.keymap,
                inner.ext.commands(),
                &inner.settings.keybindings,
            );
            inner.keymap_revision = inner.ext.commands_revision();
            if keymap.bindings != inner.keymap.bindings
                || !std::ptr::eq(keymap.preset, inner.keymap.preset)
            {
                inner.keymap = keymap;
                inner.dirty.full = true;
            }
            errors
        };
        for error in errors {
            self.notify(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::ext::KeyRequest;
    use schnellui::RawModifiers;
    use schnellui::raw_keyboard::{KeyCode, KeyLocation, PhysicalKey};

    use super::*;

    /// A key press. `base` is the key without modifiers, `text` what the
    /// layout produced.
    fn key(base: Key, text: Key, mods: &str) -> RawKeyEvent {
        RawKeyEvent {
            logical_key: text,
            physical_key: PhysicalKey::Code(KeyCode::KeyA),
            key_without_modifiers: base,
            location: KeyLocation::Standard,
            modifiers: RawModifiers {
                shift: mods.contains('s'),
                control: mods.contains('c'),
                alt: mods.contains('a'),
                super_key: false,
            },
            state: RawInputState::Pressed,
            repeat: false,
            text: None,
            text_with_all_modifiers: None,
        }
    }

    fn ch(c: &str) -> Key {
        Key::Character(c.into())
    }

    fn named(n: NamedKey) -> Key {
        Key::Named(n)
    }

    fn action(keymap: &Keymap, press: Press) -> Option<Action> {
        match press {
            Press::Run(index) => Some(keymap.bindings[index].action.clone()),
            _ => None,
        }
    }

    #[test]
    fn every_preset_parses_without_duplicate_keys() {
        for preset in &PRESETS {
            let mut seen = Vec::new();
            for (keys, action) in preset.entries() {
                let parsed = Keys::parse(keys).unwrap_or_else(|e| panic!("{}: {e}", preset.id));
                Action::parse(action).unwrap_or_else(|e| panic!("{}: {e}", preset.id));
                assert!(!seen.contains(&parsed), "{}: {keys} twice", preset.id);
                seen.push(parsed);
            }
            let (keymap, errors) = Keymap::build(preset.id, &[], &BTreeMap::new());
            assert!(errors.is_empty(), "{errors:?}");
            assert_eq!(keymap.bindings.len(), preset.entries().count());
            assert!(keymap.keys_for(&Action::Palette).is_some(), "{}", preset.id);
            assert!(
                keymap.keys_for(&Action::NewTerminal).is_some(),
                "{}",
                preset.id
            );
        }
    }

    #[test]
    fn keys_parse_and_display() {
        let keys = Keys::parse("Ctrl+Shift+P").unwrap();
        assert_eq!(keys.to_string(), "Ctrl+Shift+P");
        assert_eq!(Keys::parse("ctrl+b %").unwrap().to_string(), "Ctrl+B %");
        assert_eq!(
            Keys::parse("alt+shift+plus").unwrap(),
            Keys::parse("alt+shift++").unwrap()
        );
        assert_eq!(
            Keys::parse("ctrl+pagedown").unwrap().to_string(),
            "Ctrl+PageDown"
        );
        assert!(Keys::parse("ctrl+nope").is_err());
        assert!(Keys::parse("  ").is_err());
        assert!(Keys::parse("a b c d").is_err());
        assert_eq!(Action::parse("tab_3"), Ok(Action::Tab(3)));
        assert!(Action::parse("tab_10").is_err());
        assert_eq!(
            Action::parse("command:git.log"),
            Ok(Action::Command("git.log".into()))
        );
        assert!(Action::parse("fly").is_err());
    }

    #[test]
    fn the_default_keymap_matches_ctrl_shift_chords() {
        let mut keymap = Keymap::default();
        let press = keymap.press(&key(ch("t"), ch("T"), "cs"));
        assert_eq!(action(&keymap, press), Some(Action::NewTerminal));
        // Ctrl+T without Shift belongs to the shell.
        assert_eq!(keymap.press(&key(ch("t"), ch("t"), "c")), Press::Ignored);
        let press = keymap.press(&key(ch(","), ch(","), "c"));
        assert_eq!(action(&keymap, press), Some(Action::Settings));
    }

    #[test]
    fn tmux_sequences_wait_for_their_second_key() {
        let (mut keymap, _) = Keymap::build("tmux", &[], &BTreeMap::new());
        assert_eq!(keymap.press(&key(ch("b"), ch("b"), "c")), Press::Consumed);
        assert!(keymap.is_pending());
        // Shift on its own keeps the sequence; Shift+5 types `%`.
        assert_eq!(
            keymap.press(&key(named(NamedKey::Shift), named(NamedKey::Shift), "s")),
            Press::Ignored
        );
        let press = keymap.press(&key(ch("5"), ch("%"), "s"));
        assert_eq!(
            action(&keymap, press),
            Some(Action::Split(SplitAxis::SideBySide))
        );
        assert!(!keymap.is_pending());

        // An unbound key after the prefix is dropped, the next one is not.
        keymap.press(&key(ch("b"), ch("b"), "c"));
        assert_eq!(keymap.press(&key(ch("z"), ch("z"), "")), Press::Consumed);
        assert_eq!(keymap.press(&key(ch("z"), ch("z"), "")), Press::Ignored);

        keymap.press(&key(ch("b"), ch("b"), "c"));
        let press = keymap.press(&key(ch("b"), ch("b"), "c"));
        assert_eq!(
            action(&keymap, press),
            Some(Action::SendText("\u{2}".into()))
        );
    }

    #[test]
    fn symbol_bindings_match_the_produced_character() {
        let (mut keymap, _) = Keymap::build("windows-terminal", &[], &BTreeMap::new());
        // Alt+Shift+= produces `+` on a US layout.
        let press = keymap.press(&key(ch("="), ch("+"), "as"));
        assert_eq!(
            action(&keymap, press),
            Some(Action::Split(SplitAxis::SideBySide))
        );
        let (mut keymap, _) = Keymap::build("konsole", &[], &BTreeMap::new());
        let press = keymap.press(&key(ch("9"), ch("("), "cs"));
        assert_eq!(
            action(&keymap, press),
            Some(Action::Split(SplitAxis::SideBySide))
        );
    }

    #[test]
    fn extensions_layer_over_the_preset_and_the_user_over_both() {
        let command = |name: &str, extension: &str, keys: &[(Option<&str>, &str)]| CommandInfo {
            name: name.into(),
            title: name.into(),
            extension: extension.into(),
            keys: keys
                .iter()
                .map(|(keymap, keys)| KeyRequest {
                    keymap: keymap.map(str::to_owned),
                    keys: (*keys).into(),
                })
                .collect(),
        };
        let commands = [
            command(
                "git.log",
                "git",
                &[(None, "ctrl+alt+g"), (Some("tmux"), "ctrl+b g")],
            ),
            command("other.log", "other", &[(None, "ctrl+alt+g")]),
            command("shadow", "shadow", &[(None, "ctrl+shift+t")]),
            command("broken", "broken", &[(None, "ctrl+huh")]),
        ];
        let mut user = BTreeMap::new();
        user.insert("ctrl+shift+e".to_owned(), None);
        user.insert("ctrl+alt+s".to_owned(), Some("split_stacked".to_owned()));
        user.insert("ctrl+alt+x".to_owned(), Some("nonsense".to_owned()));

        let (keymap, errors) = Keymap::build(DEFAULT_KEYMAP, &commands, &user);
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(
            errors[0].contains("already bound by extension git"),
            "{errors:?}"
        );
        let find = |keys: &str| {
            let keys = Keys::parse(keys).unwrap();
            keymap.bindings.iter().find(|b| b.keys == keys).cloned()
        };
        let git = find("ctrl+alt+g").unwrap();
        assert_eq!(git.action, Action::Command("git.log".into()));
        assert_eq!(git.source, Source::Extension("git".into()));
        assert_eq!(
            find("ctrl+shift+t").unwrap().action,
            Action::Command("shadow".into())
        );
        assert!(find("ctrl+shift+e").is_none(), "the user removed it");
        assert_eq!(find("ctrl+alt+s").unwrap().source, Source::User);
        assert_eq!(
            keymap.hint("Split stacked", &Action::Split(SplitAxis::Stacked)),
            "Split stacked (Ctrl+Alt+S)"
        );

        let (tmux, _) = Keymap::build("tmux", &commands, &BTreeMap::new());
        let keys = Keys::parse("ctrl+b g").unwrap();
        assert!(tmux.bindings.iter().any(|b| b.keys == keys));
    }

    #[test]
    fn an_unknown_preset_falls_back_to_the_default() {
        let (keymap, errors) = Keymap::build("emacs", &[], &BTreeMap::new());
        assert_eq!(keymap.preset.id, DEFAULT_KEYMAP);
        assert_eq!(errors.len(), 1);
    }
}
