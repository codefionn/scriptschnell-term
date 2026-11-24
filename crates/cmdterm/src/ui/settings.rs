//! The settings overlay, opened with Ctrl+, the rail's Settings button or the
//! palette. A modal dialog with these sections: the active project (its
//! Jujutsu choice and the folders its sandboxes may use, with Allowed and
//! Blocked subsections for the launches and guarded requests it allowed or
//! blocked for good),
//! terminal font (size and family), color scheme, keyboard (keymap preset
//! and the bindings in effect), extensions, agents (hooks that let coding
//! agents report when they wait for the user), remote machines with its VPN
//! networks subsection, virtual machines, sandboxing with its Files and
//! Hosts subsections, language, and the About pages: the changelog and the
//! licenses of the app and its crates. `ui/machines.rs` builds the project,
//! machine and sandboxing pages. It is its own region; picking a section or
//! toggling an extension rebuilds only the overlay, while a font, scheme or
//! language change remounts the app because every region draws with it.
//! The font family is a searchable dropdown: opening it rebuilds the overlay
//! once, and typing filters its rows in place without a rebuild. The header
//! shows a toast for a few seconds after an action such as a sign-in, an
//! added account or a trusted machine worked or failed.

use crate::state::{
    AgentControl, ExtensionFilter, FONT_SIZE_RANGE, SYSTEM_LANGUAGE, SYSTEM_SCHEME, ShellChoice,
};
use schnellui::layout::EdgeInsets;
use schnellui::scene::{Color, Point, Size};
use schnellui::widgets::{
    Align, Badge, Button, ButtonAppearance, Column, ComboBox, ComboBoxOption, Dialog,
    DialogPosition, Dropdown, DropdownOption, Justify, ListRow, LoadingSpinner, Pad, PasswordInput,
    Row, Scroll, Switch, Text, TextInput, Theme, ThemeProvider, View, WrapMode,
};
use std::time::{Duration, Instant};

use super::theme::{SCHEMES, Scheme, resolve};
use super::{Closed, Fill, Named, tail_path, truncate};
use crate::ext::{SettingKind, SettingValue};
use cmdterm_env::{Environment, GuardKind, GuardMode};

use crate::agents::{Agent, AgentSetup, Status};
use crate::i18n::{Lang, Strings};
use crate::keymap::{PRESETS, Source};
use crate::workspace::Workspace;

/// Accessible name of the field that finds a setting; key handlers find it
/// by this name, so it stays English.
pub const SETTINGS_FIND_INPUT: &str = "Find a setting";
/// Width of the settings surface while the window has room for it.
pub const SETTINGS_WIDTH: f32 = 960.0;
/// The least the content right of the nav shrinks to in a narrow window.
const CONTENT_MIN_WIDTH: f32 = 360.0;
/// Height of the section content while the window has room for it, so
/// switching sections does not resize.
const BODY_HEIGHT: f32 = 560.0;
/// The least the section content shrinks to in a small window.
const BODY_MIN_HEIGHT: f32 = 240.0;
/// Height of the overlay's header row with the title and Close.
const HEADER_HEIGHT: f32 = 28.0;

thread_local! {
    /// The section content's height in the overlay being built, set by
    /// [`settings`] from the window's height.
    static BODY: std::cell::Cell<f32> = const { std::cell::Cell::new(BODY_HEIGHT) };
    /// The surface's width in the overlay being built, set by [`settings`]
    /// from the window's width so the dialog never runs off its edge.
    static SURFACE: std::cell::Cell<f32> = const { std::cell::Cell::new(SETTINGS_WIDTH) };
    /// The theme of a segmented control's chosen segment in the overlay
    /// being built: the full accent, so the choice reads at a glance.
    static CHOSEN: std::cell::Cell<Option<&'static Scheme>> = const { std::cell::Cell::new(None) };
    /// The settings a search found on the shown page, with the color that
    /// marks their titles there.
    static FOUND: std::cell::RefCell<(Vec<String>, Option<Theme>)> =
        const { std::cell::RefCell::new((Vec::new(), None)) };
}

/// The color that marks a setting titled `title` as one the search found,
/// if it is one.
pub(super) fn found_color(title: &str) -> Option<Color> {
    found_theme(title).map(|theme| theme.text)
}

/// The page's theme with the marking color as its text, for a setting
/// titled `title` the search found.
fn found_theme(title: &str) -> Option<Theme> {
    FOUND.with(|found| {
        let (names, theme) = &*found.borrow();
        theme.filter(|_| names.iter().any(|name| name.eq_ignore_ascii_case(title)))
    })
}

/// `row` with a bar in the marking color when the search found the setting
/// titled `title`, so the hit stands out on its page.
fn found_row(row: ListRow, title: &str) -> ListRow {
    match found_color(title) {
        Some(color) => row.accent_bar(color),
        None => row,
    }
}

/// A segment of a segmented control, framed and tinted in the accent when
/// chosen, like a chosen row.
fn segment(chosen: bool, text: &str, button: Button) -> Box<dyn View> {
    tinted_segment(chosen, text, button, None)
}

/// [`segment`] whose chosen frame and tint take `tint` instead of the
/// accent, for a choice whose meaning has a color, such as Block or Allow.
///
/// The chosen segment looks like a chosen row, not like a solid main
/// button, so the page has one way to say "picked".
fn tinted_segment(chosen: bool, text: &str, button: Button, tint: Option<Color>) -> Box<dyn View> {
    let button = button.appearance(ButtonAppearance::Outline);
    match CHOSEN.with(std::cell::Cell::get).filter(|_| chosen) {
        // A check mark too, so the pick does not rest on color alone.
        Some(scheme) => {
            let shown = format!("✓ {text}");
            let tint = tint.unwrap_or(scheme.lanes[0]);
            let theme = Theme {
                surface: scheme.tinted(tint),
                outline: tint,
                ..scheme.theme()
            };
            Box::new(ThemeProvider::new(theme, button.display(shown)))
        }
        None => Box::new(button),
    }
}

/// Height of the section content: [`BODY_HEIGHT`], or what a smaller window
/// leaves for it.
pub(super) fn body_height() -> f32 {
    BODY.with(std::cell::Cell::get)
}

/// Where the overlay's top edge sits and how tall its section content is
/// in a window `height` tall. The overlay keeps [`TOP`] above and below it
/// and grows with the window up to [`BODY_MAX_HEIGHT`]; a small window gives
/// it the window's whole height.
fn body_layout(height: f32) -> (f32, f32) {
    // The dialog's padding, the header and the gap under it, and a margin
    // under the dialog.
    let chrome = PAD * 2.0 + HEADER_HEIGHT + PAD + PAD;
    let roomy = height - TOP * 2.0 - chrome;
    if roomy >= BODY_HEIGHT {
        return (TOP, roomy.min(BODY_MAX_HEIGHT));
    }
    let body = (height - PAD - chrome).clamp(BODY_MIN_HEIGHT, BODY_HEIGHT);
    if height - TOP - chrome >= BODY_HEIGHT {
        return (TOP, body.min(height - TOP - chrome));
    }
    (PAD, body)
}

/// The most the section content grows to in a tall window.
const BODY_MAX_HEIGHT: f32 = 820.0;
const NAV_WIDTH: f32 = 184.0;
/// Room the nav's rows leave for its scrollbar when the pages scroll.
const NAV_SCROLLBAR: f32 = 12.0;
/// The pages that start a group in the nav, under a small heading.
const NAV_GROUPS: [SettingsSection; 5] = [
    SettingsSection::Project,
    SettingsSection::Extensions,
    SettingsSection::Remote,
    SettingsSection::Sandboxing,
    SettingsSection::Changelog,
];
/// How far subsections sit right of their section in the nav.
const SUB_INDENT: f32 = 16.0;
/// Where a sub-page's guide line sits inside its indent.
const SUB_GUIDE: f32 = 12.0;
pub(super) const ROW_HEIGHT: f32 = 28.0;
const TOP: f32 = 40.0;
const PAD: f32 = 8.0;
pub(super) const GAP: f32 = 12.0;
/// Room for the scroll bar at the right edge of a `Scroll`. Every settings
/// `Scroll` shows its bar, and rows inside it end this much earlier so the bar
/// does not cover their buttons or switches.
pub(super) const SCROLLBAR_ROOM: f32 = 20.0;
/// Height of one option in the open list of a settings dropdown, with
/// [`super::machines::INPUT_TEXT_SIZE`] text, rounded up.
const DROPDOWN_ROW: f32 = 34.0;
/// Least width of a settings dropdown, so the dropdowns of a page line up.
pub(super) const SELECT_WIDTH: f32 = 320.0;
/// Width of the surface in the overlay being built.
fn surface_width() -> f32 {
    SURFACE.with(std::cell::Cell::get)
}

/// The surface's width in a window `width` wide: the full width with
/// room to spare, else the window less a margin, down to what the nav and
/// the narrowest content need.
fn surface_for(width: f32) -> f32 {
    let least = PAD * 2.0 + NAV_WIDTH + 1.0 + GAP * 2.0 + CONTENT_MIN_WIDTH;
    (width - PAD * 2.0).clamp(least, SETTINGS_WIDTH)
}

/// Width of the content right of the section list.
pub(super) fn content_width() -> f32 {
    surface_width() - PAD * 2.0 - NAV_WIDTH - 1.0 - GAP * 2.0
}
/// Width of the font family dropdown: room for long family names, not
/// the whole page.
const FONT_PICKER_WIDTH: f32 = SELECT_WIDTH;
/// Width of the field that shows the terminal font size.
const FONT_SIZE_FIELD: f32 = 60.0;
/// Most of the font dropdown's height before its rows scroll: eight whole
/// rows, so the last one is not cut through its text.
const FONT_POPUP_HEIGHT: f32 = 8.0 * 33.0 + 1.0;

/// A page of the settings overlay.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SettingsSection {
    /// Settings of the active project, with its sandbox folders.
    Project,
    /// What the active project's sandboxes may do without asking, under
    /// Project.
    ProjectAllowed,
    /// What the active project's sandboxes are refused without asking,
    /// under Project.
    ProjectBlocked,
    #[default]
    Font,
    ColorScheme,
    Keyboard,
    /// The default shell and the shells the new panel list offers. Shown
    /// where the app finds shells: on Windows.
    Shells,
    Extensions,
    /// Forge accounts and their sign-in.
    Accounts,
    /// The logins extensions pass into sandboxes, under Accounts.
    Registries,
    Agents,
    Remote,
    /// Tailscale, NetBird and Nebula peers, under Remote machines.
    Vpn,
    Vms,
    /// Where terminals run, the devcontainer engine and the sandbox's network.
    Sandboxing,
    /// The sandbox's writable and hidden paths, under Sandboxing.
    SandboxFiles,
    /// The hosts a restricted sandbox may reach, under Sandboxing.
    SandboxHosts,
    Language,
    /// What changed in each release, from the changelog built into the app.
    Changelog,
    /// The app's license and those of the crates built into it.
    Licenses,
}

impl SettingsSection {
    pub const ALL: [SettingsSection; 20] = [
        SettingsSection::Project,
        SettingsSection::ProjectAllowed,
        SettingsSection::ProjectBlocked,
        SettingsSection::Font,
        SettingsSection::ColorScheme,
        SettingsSection::Keyboard,
        SettingsSection::Shells,
        SettingsSection::Language,
        SettingsSection::Extensions,
        SettingsSection::Accounts,
        SettingsSection::Registries,
        SettingsSection::Agents,
        SettingsSection::Remote,
        SettingsSection::Vpn,
        SettingsSection::Vms,
        SettingsSection::Sandboxing,
        SettingsSection::SandboxFiles,
        SettingsSection::SandboxHosts,
        SettingsSection::Changelog,
        SettingsSection::Licenses,
    ];

    pub fn title(self, tr: &Strings) -> &'static str {
        match self {
            SettingsSection::Project => tr.section_project,
            SettingsSection::ProjectAllowed => tr.section_project_allowed,
            SettingsSection::ProjectBlocked => tr.section_project_blocked,
            SettingsSection::Font => tr.section_font,
            SettingsSection::ColorScheme => tr.section_color_scheme,
            SettingsSection::Keyboard => tr.section_keyboard,
            SettingsSection::Shells => tr.section_shells,
            SettingsSection::Extensions => tr.section_extensions,
            SettingsSection::Accounts => tr.section_accounts,
            SettingsSection::Registries => tr.accounts_logins,
            SettingsSection::Agents => tr.section_agents,
            SettingsSection::Remote => tr.section_remote,
            SettingsSection::Vpn => tr.section_vpn,
            SettingsSection::Vms => tr.section_vms,
            SettingsSection::Sandboxing => tr.section_sandboxing,
            SettingsSection::SandboxFiles => tr.section_sandbox_files,
            SettingsSection::SandboxHosts => tr.section_sandbox_hosts,
            SettingsSection::Language => tr.section_language,
            SettingsSection::Changelog => tr.section_changelog,
            SettingsSection::Licenses => tr.section_licenses,
        }
    }

    /// The section this one is a subsection of, if any. The nav lists a
    /// subsection only while its parent or a sibling is shown.
    pub fn parent(self) -> Option<SettingsSection> {
        match self {
            SettingsSection::SandboxFiles | SettingsSection::SandboxHosts => {
                Some(SettingsSection::Sandboxing)
            }
            SettingsSection::ProjectAllowed | SettingsSection::ProjectBlocked => {
                Some(SettingsSection::Project)
            }
            SettingsSection::Vpn => Some(SettingsSection::Remote),
            SettingsSection::Registries => Some(SettingsSection::Accounts),
            _ => None,
        }
    }
}

/// A dropdown of the overlay. At most one is open; switching sections
/// closes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Picker {
    Font,
    Keymap,
    Language,
    ContainerEngine,
    /// Where worktrees the app opens for the first time run.
    Runtime,
    /// Where the active project's worktrees run.
    ProjectRuntime,
    SandboxEngines,
    SidecarEngine,
    AgentControl,
    /// The CPU mode of terminals at a place.
    Cpu(crate::cpu::Place),
    Shell,
    /// The agent New agent tab starts.
    DefaultAgent,
    /// The forge to add an account on another host for.
    ForgeHost,
    /// The guard of a new entry on the project's Allowed or Blocked page.
    ProjectGuard,
}

/// What the overlay is showing.
#[derive(Default)]
pub struct SettingsState {
    pub open: bool,
    /// Opened by the rail's Settings button, which takes focus back when
    /// the overlay closes.
    pub from_rail: bool,
    /// The sandbox settings when the overlay opened or the shown
    /// worktree's terminals last restarted from it. The restart button
    /// shows only once they differ.
    pub sandbox_applied: Option<crate::state::SandboxSettings>,
    pub section: SettingsSection,
    /// Installed monospace families, listed on first open. Cold: loading
    /// them reads every installed font's metadata.
    pub fonts: Option<Vec<String>>,
    /// The dropdown that shows its options, if any.
    pub picker: Option<Picker>,
    /// What the user typed into the open searchable dropdown, kept so a
    /// rebuild of the overlay does not drop it.
    pub picker_query: String,
    /// The Agents page: which agent integrations are installed where.
    pub agents: AgentSetup,
    /// The Remote machines, Virtual machines and Containers pages.
    pub machines: crate::machines::MachinesUi,
    /// The extension whose page the Extensions section shows; `None` is
    /// the built-in page.
    pub extension: Option<String>,
    /// Whether the Extensions section shows `extension`'s page instead of
    /// the list of every extension.
    pub extension_open: bool,
    /// What the find field above the extension list holds.
    pub extension_query: String,
    /// Whether the find field is being typed in, so a rebuild keeps focus
    /// in it.
    pub finding_extension: bool,
    /// What the find field above the nav holds.
    pub page_query: String,
    /// Whether that field is being typed in, so a rebuild keeps focus in it.
    pub finding_page: bool,
    /// A typed extension setting changed; the extensions reload when the
    /// user leaves the page.
    pub extensions_stale: bool,
    /// What the user typed into the new-entry field of a list setting, by
    /// [`list_key`], and why the last Add refused it.
    pub list_drafts: std::collections::HashMap<String, ListDraft>,
    /// The reset button was clicked once; the next click resets.
    pub confirm_reset: bool,
    /// The action whose binding waits for a new key on the Keyboard page.
    pub recording: Option<crate::keymap::Action>,
    /// The binding last recorded or reset on the Keyboard page, whose row
    /// takes focus back from the recorder or the reset button.
    pub last_binding: Option<crate::keymap::Action>,
    /// Counts recordings and resets, so a second one on the same row still
    /// moves focus back to it.
    pub binding_serial: u32,
    /// The open form on the Accounts page: an account's key for its token
    /// form, or a forge id and `+add` for its add form.
    pub account_form: Option<String>,
    /// How the last action on a page ended, shown in the header.
    pub toast: Option<Toast>,
    /// The pages the mouse's back button returns to, latest last. Opening
    /// the overlay starts it over.
    pub back: Vec<SettingsPlace>,
    /// The pages the forward button returns to, latest last. Going to a
    /// page drops them.
    pub forward: Vec<SettingsPlace>,
    /// The license whose text the Licenses page shows: [`LICENSE_MIT_KEY`],
    /// [`LICENSE_APACHE_KEY`] or a crate's name and version.
    pub license: Option<String>,
}

impl SettingsFocusKey {
    /// Whether `self` is `before` with its dropdown closed and nothing else
    /// changed. Focus then stays on the dropdown, as the closed list hands
    /// it back to the control that opened it.
    pub fn closed_picker_of(&self, before: &Self) -> bool {
        self.picker.is_none()
            && before.picker.is_some()
            && *self
                == Self {
                    picker: None,
                    ..before.clone()
                }
    }
}

/// See [`Workspace::settings_focus_key`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsFocusKey {
    section: SettingsSection,
    picker: Option<Picker>,
    recording: bool,
    finding_page: bool,
    finding_extension: bool,
    extension: Option<Option<String>>,
    account_form: Option<String>,
    typing: bool,
    binding: Option<(crate::keymap::Action, u32)>,
}

/// Most pages the back and forward histories each keep.
const HISTORY: usize = 32;

/// A page of the overlay, as its back and forward history keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsPlace {
    section: SettingsSection,
    /// The page the Extensions section shows in place of its list, with
    /// `Some(None)` for the built-in page.
    extension: Option<Option<String>>,
}

impl SettingsState {
    fn place(&self) -> SettingsPlace {
        let extension = (self.section == SettingsSection::Extensions && self.extension_open)
            .then(|| self.extension.clone());
        SettingsPlace {
            section: self.section,
            extension,
        }
    }

    /// Puts the shown page on the back history before the user goes to
    /// another, unless the overlay is closed.
    fn remember_place(&mut self) {
        if !self.open {
            return;
        }
        let place = self.place();
        if self.back.last() != Some(&place) {
            if self.back.len() == HISTORY {
                self.back.remove(0);
            }
            self.back.push(place);
        }
        self.forward.clear();
    }
}

/// How long a toast about a finished action stays in the header.
pub const TOAST_TTL: Duration = Duration::from_secs(4);
/// How long a toast about a failed action stays: longer, to read the error.
pub const FAILED_TOAST_TTL: Duration = Duration::from_secs(8);
/// Longest toast shown, in characters.
const TOAST_CHARS: usize = 72;

/// A short message about the last action on a settings page, such as a
/// sign-in or a trusted machine. The header shows it until it expires or a
/// newer one replaces it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toast {
    pub text: String,
    pub failed: bool,
    pub at: Instant,
}

impl Toast {
    fn ttl(&self) -> Duration {
        if self.failed {
            FAILED_TOAST_TTL
        } else {
            TOAST_TTL
        }
    }
}

/// The new-entry field of one list setting.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListDraft {
    pub text: String,
    pub error: Option<String>,
}

/// Names a list setting in [`SettingsState::list_drafts`].
fn list_key(extension: &str, key: &str) -> String {
    format!("{extension}\n{key}")
}

impl crate::workspace::Inner {
    /// Reports how an action on a settings page ended: a toast in the
    /// overlay's header while it is open, else a notice, since the action
    /// may finish on a thread after the user closed the overlay.
    pub(crate) fn toast(&mut self, failed: bool, text: String) {
        if !self.settings_ui.open {
            self.push_notice(text);
            return;
        }
        let toast = Toast {
            text,
            failed,
            at: Instant::now(),
        };
        crate::workspace::arm_expiry(self.wake.clone(), toast.ttl());
        self.settings_ui.toast = Some(toast);
        self.dirty.settings = true;
    }

    /// Drops the toast once it is older than its time to live.
    pub(crate) fn expire_toast(&mut self, now: Instant) {
        if let Some(toast) = &self.settings_ui.toast
            && now.saturating_duration_since(toast.at) >= toast.ttl()
        {
            self.settings_ui.toast = None;
            self.dirty.settings = true;
        }
    }

    /// The entries of extension `extension`'s list setting `key`: the
    /// user's, else the manifest's default. `None` when it declares no such
    /// list.
    pub(crate) fn list_setting(&self, extension: &str, key: &str) -> Option<Vec<String>> {
        let info = self.ext.extensions().find(|e| e.name == extension)?;
        let spec = info.settings.iter().find(|s| s.key == key)?;
        if !spec.kind.is_list() {
            return None;
        }
        let own = self
            .settings
            .extension_settings
            .get(extension)
            .and_then(|own| own.get(key))
            .and_then(|value| serde_json::from_value::<SettingValue>(value.clone()).ok())
            .filter(|value| spec.kind.fits(value));
        match own.unwrap_or_else(|| spec.default.clone()) {
            SettingValue::List(items) => Some(items),
            _ => None,
        }
    }
}

impl Workspace {
    /// Reports an action on a settings page that worked.
    pub fn toast_done(&self, text: impl Into<String>) {
        self.borrow_mut().toast(false, text.into());
    }

    /// Reports an action on a settings page that failed, and why.
    pub fn toast_failed(&self, text: impl Into<String>) {
        self.borrow_mut().toast(true, text.into());
    }

    /// The toast the header shows, for tests.
    #[cfg(test)]
    pub fn toast_for_test(&self) -> Option<(bool, String)> {
        let inner = self.borrow();
        let toast = inner.settings_ui.toast.as_ref()?;
        Some((toast.failed, toast.text.clone()))
    }

    /// Keeps what the user typed into the new-entry field of a list
    /// setting. No rebuild: the field shows it already, and a refusal under
    /// it goes with the next one.
    pub fn set_list_draft(&self, extension: &str, key: &str, text: &str) {
        let mut inner = self.borrow_mut();
        let draft = inner
            .settings_ui
            .list_drafts
            .entry(list_key(extension, key))
            .or_default();
        draft.text = text.to_owned();
        draft.error = None;
    }

    /// Adds the typed entry to a list setting and reloads the extension. An
    /// entry the list has already, or for a `hosts` setting one that is no
    /// host, stays in the field with the reason under it.
    pub fn add_list_entry(&self, extension: &str, key: &str) {
        let draft_key = list_key(extension, key);
        let (items, error) = {
            let mut inner = self.borrow_mut();
            let Some(mut items) = inner.list_setting(extension, key) else {
                return;
            };
            let tr = inner.tr;
            let typed = (inner.settings_ui.list_drafts.get(&draft_key))
                .map(|d| d.text.trim().to_owned())
                .unwrap_or_default();
            if typed.is_empty() {
                return;
            }
            let kind = (inner.ext.extensions())
                .find(|e| e.name == extension)
                .and_then(|e| e.settings.iter().find(|s| s.key == key))
                .map(|spec| spec.kind.clone());
            let entry = match kind {
                Some(SettingKind::Hosts) => {
                    let host = typed.to_ascii_lowercase();
                    cmdterm_env::valid_host_pattern(&host).then_some(host)
                }
                Some(SettingKind::Urls) => crate::ext::https_host(&typed)
                    .is_some()
                    .then(|| typed.clone()),
                _ => Some(typed.clone()),
            };
            let error = match &entry {
                None if kind == Some(SettingKind::Urls) => Some((tr.not_a_url)(&typed)),
                None => Some((tr.not_a_host)(&typed)),
                Some(entry) if items.contains(entry) => Some((tr.list_duplicate)(&typed)),
                Some(entry) => {
                    items.push(entry.clone());
                    None
                }
            };
            inner.dirty.settings = true;
            (items, error)
        };
        if let Some(error) = error {
            let mut inner = self.borrow_mut();
            let draft = inner.settings_ui.list_drafts.entry(draft_key).or_default();
            draft.error = Some(error);
            return;
        }
        self.borrow_mut().settings_ui.list_drafts.remove(&draft_key);
        self.set_extension_setting(extension, key, &SettingValue::List(items), true);
    }

    /// Removes entry `index` of a list setting and reloads the extension.
    pub fn remove_list_entry(&self, extension: &str, key: &str, index: usize) {
        let items = {
            let inner = self.borrow();
            let Some(mut items) = inner.list_setting(extension, key) else {
                return;
            };
            if index >= items.len() {
                return;
            }
            items.remove(index);
            items
        };
        let mut inner = self.borrow_mut();
        if let Some(draft) = inner
            .settings_ui
            .list_drafts
            .get_mut(&list_key(extension, key))
        {
            draft.error = None;
        }
        inner.dirty.settings = true;
        drop(inner);
        self.set_extension_setting(extension, key, &SettingValue::List(items), true);
    }

    /// Opens the settings from the rail's button, which Escape gives focus
    /// back to, as a menu does to its button.
    pub fn open_settings_from_rail(&self) {
        let fresh = !self.borrow().settings_ui.open;
        self.open_settings();
        if fresh {
            self.borrow_mut().settings_ui.from_rail = true;
        }
    }

    pub fn open_settings(&self) {
        self.close_palette();
        self.close_notifications();
        let mut inner = self.borrow_mut();
        if inner.settings_ui.fonts.is_none() {
            let fonts = inner.shaper.monospace_font_families();
            inner.settings_ui.fonts = Some(fonts);
        }
        if !inner.settings_ui.open {
            inner.settings_ui.from_rail = false;
            inner.settings_ui.back.clear();
            inner.settings_ui.forward.clear();
            // Each opening starts with the whole nav, not a search left
            // over from the last time, and with focus in the find field, so
            // typing searches at once.
            inner.settings_ui.page_query.clear();
            inner.settings_ui.finding_page = true;
            inner.settings_ui.finding_extension = false;
            inner.settings_ui.machines.form.filtering_hosts = false;
            inner.settings_ui.machines.form.typing_key = false;
            inner.settings_ui.machines.form.typing_address = false;
            inner.forges.typing = false;
        }
        inner.settings_ui.open = true;
        inner.settings_ui.sandbox_applied = Some(inner.settings.sandbox.clone());
        inner.settings_ui.picker = None;
        inner.settings_ui.last_binding = None;
        inner.settings_ui.confirm_reset = false;
        inner.dirty.settings = true;
        let section = inner.settings_ui.section;
        drop(inner);
        if matches!(
            section,
            SettingsSection::Remote | SettingsSection::Vpn | SettingsSection::Vms
        ) {
            self.refresh_machines();
        }
        if section == SettingsSection::Vpn {
            self.refresh_vpn();
        }
    }

    /// Opens the Accounts page form named by `key`, or closes it when it
    /// is open.
    pub fn toggle_account_form(&self, key: &str) {
        let mut inner = self.borrow_mut();
        let form = &mut inner.settings_ui.account_form;
        *form = if form.as_deref() == Some(key) {
            None
        } else {
            Some(key.to_owned())
        };
        inner.dirty.settings = true;
    }

    /// Closes the token form of `forge`'s `account` and drops what was
    /// typed in it.
    pub fn cancel_account_form(&self, forge: &str, account: &str) {
        let mut inner = self.borrow_mut();
        inner
            .forges
            .typed
            .remove(&crate::forges::account_key(forge, account));
        inner.settings_ui.account_form = None;
        inner.dirty.settings = true;
    }

    pub fn close_settings(&self) {
        {
            let mut inner = self.borrow_mut();
            if inner.settings_ui.open {
                inner.settings_ui.open = false;
                inner.settings_ui.toast = None;
                inner.settings_ui.recording = None;
                inner.settings_ui.machines.form.project_env_value.clear();
                inner.settings_ui.machines.form.project_env_error = None;
                inner.dirty.settings = true;
            }
        }
        self.apply_extension_settings();
    }

    /// Shows extension `name`'s page of the Extensions section, or the
    /// built-in page with `None`.
    pub fn show_extension_page(&self, name: Option<String>) {
        self.go_to_settings_place(
            SettingsPlace {
                section: SettingsSection::Extensions,
                extension: Some(name),
            },
            true,
        );
    }

    /// Goes back from an extension's page to the list of every extension.
    pub fn show_extension_list(&self) {
        let section = self.borrow().settings_ui.section;
        self.go_to_settings_place(
            SettingsPlace {
                section,
                extension: None,
            },
            true,
        );
    }

    /// Shows `place`. With `record`, the page it leaves goes on the back
    /// history, as following a link does in a browser.
    fn go_to_settings_place(&self, place: SettingsPlace, record: bool) {
        self.apply_extension_settings();
        if self.borrow().settings_ui.place() == place {
            self.borrow_mut().settings_ui.finding_extension = false;
            return;
        }
        if record {
            self.borrow_mut().settings_ui.remember_place();
        }
        self.switch_settings_section(place.section);
        let mut inner = self.borrow_mut();
        inner.settings_ui.finding_extension = false;
        match place.extension {
            Some(name) => {
                inner.settings_ui.extension = name;
                inner.settings_ui.extension_open = true;
            }
            None => inner.settings_ui.extension_open = false,
        }
        inner.dirty.settings = true;
    }

    /// Steps back or forward through the pages the overlay showed since it
    /// opened, for the mouse's back and forward buttons. Back on an
    /// extension's page with no history goes to the list, as the page's own
    /// back button does. Returns whether the overlay moved.
    pub fn navigate_settings(&self, navigation: schnellui::Navigation) -> bool {
        let place = {
            let mut inner = self.borrow_mut();
            let ui = &mut inner.settings_ui;
            if !ui.open {
                return false;
            }
            let here = ui.place();
            let (from, to) = match navigation {
                schnellui::Navigation::Back => (&mut ui.back, &mut ui.forward),
                schnellui::Navigation::Forward => (&mut ui.forward, &mut ui.back),
            };
            // A page the user reached without the history, such as through
            // the nav's find field, may equal the one on top.
            while from.last() == Some(&here) {
                from.pop();
            }
            let place = match from.pop() {
                Some(place) => place,
                None if navigation == schnellui::Navigation::Back && here.extension.is_some() => {
                    SettingsPlace {
                        section: here.section,
                        extension: None,
                    }
                }
                None => return false,
            };
            if to.len() == HISTORY {
                to.remove(0);
            }
            to.push(here);
            place
        };
        self.go_to_settings_place(place, false);
        true
    }

    /// Narrows the extension list to names that contain `query`.
    /// Narrows the settings' nav to the pages that match `query`, and
    /// shows the first of them unless the shown page matches too, so the
    /// page beside the nav is always one the query found.
    pub fn set_settings_query(&self, query: &str) {
        let first = {
            let mut inner = self.borrow_mut();
            if inner.settings_ui.page_query == query {
                return;
            }
            inner.settings_ui.page_query.clear();
            inner.settings_ui.page_query.push_str(query);
            inner.dirty.settings = true;
            let tr = inner.tr;
            let no_shells = inner.shells.is_empty();
            let mut found = SettingsSection::ALL.into_iter().filter(|page| {
                !(no_shells && *page == SettingsSection::Shells) && page_matches(*page, query, tr)
            });
            let shown = inner.settings_ui.section;
            let first = found.clone().next();
            if found.any(|page| page == shown) {
                None
            } else {
                first
            }
        };
        if let Some(page) = first {
            self.switch_settings_section(page);
        }
        self.find_page_focused();
    }

    /// The find field holds focus. Only one field holds the settings' focus
    /// ref, so every other field lets it go.
    fn find_page_focused(&self) {
        let mut inner = self.borrow_mut();
        let inner = &mut *inner;
        inner.settings_ui.finding_page = true;
        inner.settings_ui.finding_extension = false;
        inner.settings_ui.machines.form.filtering_hosts = false;
        inner.settings_ui.machines.form.typing_key = false;
        inner.settings_ui.machines.form.typing_address = false;
        inner.forges.typing = false;
    }

    /// Escape in the find field: empties it while it has text, so a second
    /// Escape closes the settings. Returns whether it emptied the field.
    pub fn escape_settings_query(&self) -> bool {
        if self.borrow().settings_ui.page_query.is_empty() {
            return false;
        }
        self.set_settings_query("");
        true
    }

    /// Up and Down in the find field: shows the previous or next page that
    /// matches, and the field keeps focus. Returns whether a search runs.
    pub fn step_settings_result(&self, delta: isize) -> bool {
        let next = {
            let inner = self.borrow();
            let query = inner.settings_ui.page_query.trim().to_lowercase();
            if query.is_empty() {
                return false;
            }
            let tr = inner.tr;
            let no_shells = inner.shells.is_empty();
            let found: Vec<SettingsSection> = SettingsSection::ALL
                .into_iter()
                .filter(|page| {
                    !(no_shells && *page == SettingsSection::Shells)
                        && page_matches(*page, &query, tr)
                })
                .collect();
            if found.is_empty() {
                return true;
            }
            let at = found
                .iter()
                .position(|page| *page == inner.settings_ui.section)
                .map_or(0, |at| {
                    (at as isize + delta).rem_euclid(found.len() as isize) as usize
                });
            found[at]
        };
        self.switch_settings_section(next);
        self.find_page_focused();
        true
    }

    /// Up and Down on a page's row in the nav: shows the page above or
    /// below it, and focus follows to that page's row, so the nav reads as
    /// one list and Tab leaves it for the page. `from` is the focused row's
    /// name. Returns whether it moved.
    pub fn step_settings_nav(&self, from: &str, delta: isize) -> bool {
        let next = {
            let inner = self.borrow();
            let ui = &inner.settings_ui;
            if !ui.open || ui.extension_open || ui.picker.is_some() || ui.recording.is_some() {
                return false;
            }
            let tr = inner.tr;
            let query = ui.page_query.trim().to_lowercase();
            let no_shells = inner.shells.is_empty();
            let shown: Vec<SettingsSection> = SettingsSection::ALL
                .into_iter()
                .filter(|page| {
                    !(no_shells && *page == SettingsSection::Shells)
                        && page_matches(*page, &query, tr)
                })
                .collect();
            let Some(at) = shown.iter().position(|page| page.title(tr) == from) else {
                return false;
            };
            let to = (at as isize + delta).clamp(0, shown.len() as isize - 1) as usize;
            shown[to]
        };
        self.show_settings_section(next);
        let mut inner = self.borrow_mut();
        inner.settings_ui.finding_page = false;
        inner.dirty.settings = true;
        true
    }

    /// Enter in the find field: focus moves to the shown page's row in the
    /// nav, from where Tab reaches its settings.
    pub fn leave_settings_query(&self) -> bool {
        let mut inner = self.borrow_mut();
        if !inner.settings_ui.finding_page {
            return false;
        }
        inner.settings_ui.finding_page = false;
        inner.dirty.settings = true;
        true
    }

    pub fn set_extension_query(&self, query: &str) {
        let mut inner = self.borrow_mut();
        if inner.settings_ui.extension_query != query {
            inner.settings_ui.extension_query.clear();
            inner.settings_ui.extension_query.push_str(query);
            inner.settings_ui.finding_extension = true;
            // Only one field holds the settings' focus ref.
            inner.settings_ui.finding_page = false;
            inner.dirty.settings = true;
        }
    }

    /// Sets which extensions the Extensions section lists and saves it.
    pub fn set_extension_filter(&self, filter: ExtensionFilter) {
        let changed = {
            let mut inner = self.borrow_mut();
            let changed = inner.settings.extension_filter != filter;
            if changed {
                inner.settings.extension_filter = filter;
                inner.dirty.settings = true;
            }
            changed
        };
        if changed {
            self.save_settings();
        }
    }

    /// The reset button. The first click asks for a second one, which puts
    /// every setting back to its default, closes the overlay and opens the
    /// first-start setup again.
    pub fn click_reset_settings(&self) {
        {
            let mut inner = self.borrow_mut();
            if !inner.settings_ui.confirm_reset {
                inner.settings_ui.confirm_reset = true;
                inner.dirty.settings = true;
                return;
            }
            inner.settings_ui.confirm_reset = false;
        }
        self.close_settings();
        self.reset_settings();
    }

    /// Opens the overlay on the active project's page.
    pub fn open_project_settings(&self) {
        self.show_settings_section(SettingsSection::Project);
        self.open_settings();
    }

    pub fn settings_is_open(&self) -> bool {
        self.borrow().settings_ui.open
    }

    /// What decides where the overlay's focus belongs: its page and the
    /// picker, recorder, find field or form it shows. A rebuild that keeps
    /// this keeps focus on the control that had it, such as a switch just
    /// toggled; one that changes it moves focus to `settings_focus`. `None`
    /// while the overlay is closed.
    pub fn settings_focus_key(&self) -> Option<SettingsFocusKey> {
        let inner = self.borrow();
        let ui = &inner.settings_ui;
        ui.open.then(|| SettingsFocusKey {
            section: ui.section,
            picker: ui.picker,
            recording: ui.recording.is_some(),
            finding_page: ui.finding_page,
            finding_extension: ui.finding_extension,
            extension: ui.extension_open.then(|| ui.extension.clone()),
            account_form: ui.account_form.clone(),
            typing: inner.forges.typing,
            binding: ui
                .last_binding
                .clone()
                .map(|action| (action, ui.binding_serial)),
        })
    }

    pub fn show_settings_section(&self, section: SettingsSection) {
        if self.borrow().settings_ui.section != section {
            self.borrow_mut().settings_ui.remember_place();
        }
        // A page picked in the nav takes focus from the find field.
        self.borrow_mut().settings_ui.finding_page = false;
        self.switch_settings_section(section);
    }

    /// Shows `section` without putting the page it leaves on the back
    /// history.
    fn switch_settings_section(&self, section: SettingsSection) {
        self.apply_extension_settings();
        let mut inner = self.borrow_mut();
        if inner.settings_ui.section != section {
            inner.settings_ui.section = section;
            inner.settings_ui.machines.form.filtering_hosts = false;
            inner.settings_ui.machines.form.removed_host = None;
            inner.settings_ui.machines.form.typing_key = false;
            inner.settings_ui.machines.form.typing_address = false;
            inner.settings_ui.finding_extension = false;
            inner.settings_ui.finding_page = false;
            inner.settings_ui.extension_open = false;
            inner.settings_ui.last_binding = None;
            inner.forges.typing = false;
            inner.settings_ui.picker = None;
            inner.settings_ui.confirm_reset = false;
            inner.dirty.settings = true;
        }
        drop(inner);
        match section {
            SettingsSection::Agents => self.refresh_agents(),
            SettingsSection::Remote | SettingsSection::Vms => self.refresh_machines(),
            SettingsSection::Vpn => {
                self.refresh_machines();
                self.refresh_vpn();
            }
            _ => {}
        }
    }

    /// Every environment a project lives in, local first: where agents can
    /// run and so where their integrations go.
    pub fn agent_environments(&self) -> Vec<Environment> {
        let inner = self.borrow();
        let mut envs = vec![Environment::Local];
        for project in &inner.state.projects {
            if !envs.contains(&project.env) {
                envs.push(project.env.clone());
            }
        }
        envs
    }

    /// Checks the agent integrations again, on a worker thread.
    pub fn refresh_agents(&self) {
        let envs = self.agent_environments();
        let setup = self.borrow().settings_ui.agents.clone();
        setup.refresh(envs, self.redraw_handle());
    }

    /// Installs (`on`) or removes an agent integration, on a worker thread.
    pub fn set_agent_integration(&self, agent: Agent, env: Environment, on: bool) {
        let setup = self.borrow().settings_ui.agents.clone();
        setup.set(agent, env, on, self.redraw_handle());
    }

    /// Opens `picker`, or closes it when it is open. Opening starts a new
    /// search.
    pub fn toggle_picker(&self, picker: Picker) {
        let mut inner = self.borrow_mut();
        let ui = &mut inner.settings_ui;
        ui.picker = if ui.picker == Some(picker) {
            None
        } else {
            Some(picker)
        };
        // The open list holds the settings' focus ref, no field.
        if ui.picker.is_some() {
            ui.finding_page = false;
            ui.finding_extension = false;
        }
        ui.picker_query.clear();
        inner.dirty.settings = true;
    }

    /// Shows the text of license `key` on the Licenses page, or hides it
    /// when it shows.
    pub fn toggle_license(&self, key: &str) {
        let mut inner = self.borrow_mut();
        let ui = &mut inner.settings_ui;
        ui.license = if ui.license.as_deref() == Some(key) {
            None
        } else {
            Some(key.to_owned())
        };
        inner.dirty.settings = true;
    }

    /// Closes the open dropdown, as picking an option does.
    pub fn close_picker(&self) {
        let mut inner = self.borrow_mut();
        inner.settings_ui.picker = None;
        inner.dirty.settings = true;
    }

    /// Mirrors a searchable dropdown's search text. The dropdown filters its
    /// own rows, so this sets no dirty flag.
    pub fn set_picker_query(&self, query: &str) {
        let mut inner = self.borrow_mut();
        inner.settings_ui.picker_query.clear();
        inner.settings_ui.picker_query.push_str(query);
    }

    /// Typing in a searchable dropdown's field. While it is closed the field
    /// shows `shown`, the current choice; the first keys open the list and
    /// search for what was typed after it.
    pub fn type_in_picker(&self, picker: Picker, shown: &str, text: &str) {
        if self.borrow().settings_ui.picker == Some(picker) {
            // The open list's field shows the current choice until the first
            // key; what is typed after it is the search. Keys typed before
            // the field shows the search alone still land after the choice.
            let searching = self.borrow().settings_ui.picker_query.starts_with(shown);
            let typed = text.strip_prefix(shown).filter(|rest| !rest.is_empty());
            match typed.filter(|_| !searching && !shown.is_empty()) {
                Some(query) => {
                    self.set_picker_query(query);
                    self.borrow_mut().dirty.settings = true;
                }
                None => self.set_picker_query(text),
            }
            return;
        }
        let query = text.strip_prefix(shown).unwrap_or(text);
        let mut inner = self.borrow_mut();
        inner.settings_ui.picker = Some(picker);
        inner.settings_ui.picker_query.clear();
        inner.settings_ui.picker_query.push_str(query);
        inner.dirty.settings = true;
    }

    /// Picks a keymap from the dropdown and closes it.
    pub fn pick_keymap(&self, id: &str) {
        self.close_picker();
        self.set_keymap(id);
    }

    /// Picks a font family from the dropdown and closes it.
    pub fn pick_font_family(&self, family: Option<String>) {
        self.close_picker();
        self.set_font_family(family);
    }

    /// Picks a language from the dropdown and closes it.
    pub fn pick_language(&self, id: &str) {
        self.close_picker();
        self.set_language(id);
    }

    /// Picks which terminals get the control socket from the dropdown and
    /// closes it.
    pub fn pick_agent_control(&self, mode: AgentControl) {
        self.close_picker();
        self.set_agent_control(mode);
    }

    /// Picks the agent New agent tab starts and closes the dropdown.
    pub fn pick_default_agent(&self, agent: Option<Agent>) {
        self.close_picker();
        self.set_default_agent(agent);
    }

    /// Picks the CPU mode of the terminals at `place` and closes the
    /// dropdown. Running terminals follow.
    pub fn pick_cpu_mode(&self, place: crate::cpu::Place, mode: crate::state::CpuMode) {
        self.close_picker();
        self.set_cpu_mode(place, mode);
    }

    /// Picks the default shell from the dropdown and closes it. `None` is
    /// the system's. Terminals started from now on run it.
    pub fn pick_default_shell(&self, choice: Option<ShellChoice>) {
        self.close_picker();
        {
            let mut inner = self.borrow_mut();
            if inner.settings.shell == choice {
                return;
            }
            inner.settings.shell = choice;
            inner.dirty.settings = true;
        }
        self.save_settings();
    }

    /// Lists the installed shell `id` in the new panel list, or takes it
    /// out.
    pub fn set_shell_listed(&self, id: &str, on: bool) {
        {
            let mut inner = self.borrow_mut();
            let shells = &mut inner.settings.shells;
            let listed = shells.iter().any(|choice| choice.id() == Some(id));
            if listed == on {
                return;
            }
            if on {
                shells.push(ShellChoice::Name(id.to_owned()));
            } else {
                shells.retain(|choice| choice.id() != Some(id));
            }
            inner.dirty.settings = true;
        }
        self.save_settings();
    }
}

/// The overlay: a dialog while open, an empty slot while closed.
pub fn settings(ws: &Workspace, viewport: Size) -> Box<dyn View> {
    let (top, body) = body_layout(viewport.height);
    BODY.with(|cell| cell.set(body));
    SURFACE.with(|cell| cell.set(surface_for(viewport.width)));
    let inner = ws.borrow();
    // The scheme the chosen segments take their tint from.
    CHOSEN.with(|cell| cell.set(Some(inner.scheme)));
    if !inner.settings_ui.open {
        return Box::new(Closed);
    }
    let scheme = inner.scheme;
    let tr = inner.tr;
    let section = inner.settings_ui.section;
    let regions = inner.regions;
    // Focus goes to an open dropdown, else to the selected section.
    let picker_open = inner.settings_ui.picker.is_some();
    let recording = inner.settings_ui.recording.is_some();
    // A binding just recorded or reset keeps focus on its own row.
    let binding_focused = inner.settings_ui.last_binding.is_some();
    // Typing in the hosts field rebuilds the page; the field keeps focus.
    let filtering = (section == SettingsSection::SandboxHosts
        && inner.settings_ui.machines.form.filtering_hosts)
        || (section == SettingsSection::Accounts && inner.forges.typing)
        || (section == SettingsSection::Remote
            && (inner.settings_ui.machines.form.typing_key
                || inner.settings_ui.machines.form.typing_address))
        || (section == SettingsSection::Extensions && inner.settings_ui.finding_extension);
    let finding_page = inner.settings_ui.finding_page;
    // An open extension page takes focus on its back button, so the nav
    // shows its usual selection and no focus ring.
    let extension_page_open =
        section == SettingsSection::Extensions && inner.settings_ui.extension_open;
    let filtering = filtering || finding_page;
    let page_query = inner.settings_ui.page_query.clone();
    let toast = inner.settings_ui.toast.clone();
    drop(inner);

    // A find field over the nav narrows it to the pages whose title or
    // contents match, so a setting is found without knowing its page.
    let needle = page_query.trim().to_lowercase();
    // The shown page marks the settings it found in the accent.
    FOUND.with(|found| {
        *found.borrow_mut() = (
            matching_settings(section, &needle, tr),
            Some(Theme {
                text: scheme.lanes[0],
                ..scheme.theme()
            }),
        );
    });
    let find = ws.clone();
    let field = super::Named(
        SETTINGS_FIND_INPUT.into(),
        TextInput::new(page_query)
            .hint(tr.find_setting)
            .text_size(13.0)
            // As wide as the nav rows under it, so both share their edges.
            .width(NAV_WIDTH)
            .height(super::machines::INPUT_HEIGHT)
            .on_input(move |text| find.set_settings_query(text)),
    );
    let field: Box<dyn View> = if finding_page {
        Box::new(ThemeProvider::new(
            scheme.field_theme(),
            field.with_ref(regions.settings_focus),
        ))
    } else {
        Box::new(ThemeProvider::new(scheme.field_theme(), field))
    };
    // The find field stays put over the nav's pages, so a scrolled nav
    // never runs its scrollbar over it.
    let field = Pad::insets(EdgeInsets {
        bottom: 8.0,
        ..EdgeInsets::default()
    })
    .child(field);
    let field_height = super::machines::INPUT_HEIGHT + 8.0;
    let mut nav = Column::new().width(NAV_WIDTH);
    let no_shells = ws.borrow().shells.is_empty();
    // The nav's height, about, so a short window scrolls it instead of
    // cutting off its last pages.
    let mut nav_height = 0.0;
    // A narrowed nav says it is a list of results, so it does not read as a
    // nav with pages missing. With no result, the page beside it says so.
    let any_page = SettingsSection::ALL.into_iter().any(|entry| {
        !(entry == SettingsSection::Shells && no_shells) && page_matches(entry, &needle, tr)
    });
    if !needle.is_empty() && any_page {
        nav = nav.child(
            Pad::insets(EdgeInsets {
                left: 12.0,
                bottom: 2.0,
                ..EdgeInsets::default()
            })
            .child(ThemeProvider::new(
                Theme {
                    text: scheme.muted,
                    ..scheme.theme()
                },
                Text::new(tr.settings_results).size(11.5),
            )),
        );
        nav_height += 18.0;
    }
    // A search that finds nothing keeps every page in the nav, so the
    // pages stay in reach while the page beside it says nothing matched.
    let nav_needle = if any_page { needle.as_str() } else { "" };
    // Whether the pages scroll, so their rows keep clear of the scrollbar.
    let scrolls = {
        let mut height = nav_height;
        for entry in SettingsSection::ALL {
            if (entry == SettingsSection::Shells && no_shells)
                || !page_matches(entry, nav_needle, tr)
            {
                continue;
            }
            if nav_needle.is_empty() && NAV_GROUPS.contains(&entry) {
                height += 28.0;
            }
            height += if matching_settings(entry, nav_needle, tr).is_empty() {
                ROW_HEIGHT
            } else {
                ROW_HEIGHT + 14.0
            };
        }
        height > (body_height() - field_height).max(ROW_HEIGHT)
    };
    let gutter = if scrolls { NAV_SCROLLBAR } else { 0.0 };
    // Where the shown page's row ends, so a nav that scrolls opens with it
    // in view.
    let mut shown_bottom = 0.0;
    for entry in SettingsSection::ALL {
        if entry == SettingsSection::Shells && no_shells {
            continue;
        }
        if !page_matches(entry, nav_needle, tr) {
            continue;
        }
        // Sub-pages always show under their parent, so the nav never
        // changes shape as the user moves through it.
        let indent = if entry.parent().is_some() && nav_needle.is_empty() {
            SUB_INDENT
        } else {
            0.0
        };
        // Small muted headings group the pages: the app, what extensions
        // and agents connect to, other machines, and the sandbox.
        let group = match entry {
            // A narrowed nav is a flat list of the pages that match.
            _ if !nav_needle.is_empty() => None,
            SettingsSection::Project => Some(tr.settings_group_app),
            SettingsSection::Extensions => Some(tr.settings_group_integrations),
            SettingsSection::Remote => Some(tr.settings_group_machines),
            SettingsSection::Sandboxing => Some(tr.settings_group_sandbox),
            SettingsSection::Changelog => Some(tr.settings_group_about),
            _ => None,
        };
        if let Some(group) = group {
            nav_height += 28.0;
            nav = nav.child(
                Pad::insets(EdgeInsets {
                    left: 12.0,
                    // The first group keeps a step from the search field
                    // above it.
                    top: if entry == SettingsSection::Project {
                        8.0
                    } else {
                        10.0
                    },
                    bottom: 2.0,
                    ..EdgeInsets::default()
                })
                // Smaller and dimmer than the pages, so a group's name
                // never reads as one more page.
                .child(ThemeProvider::new(
                    Theme {
                        text: scheme.muted,
                        ..scheme.theme()
                    },
                    Text::new(group).size(11.5).bold(),
                )),
            );
        }
        let show = ws.clone();
        // While searching, a row names the settings on its page that
        // matched, so the hit is clear before the page is read.
        let found = matching_settings(entry, nav_needle, tr);
        let mut row = list_row(entry.title(tr))
            .width(NAV_WIDTH - indent - gutter)
            .height(if found.is_empty() {
                ROW_HEIGHT
            } else {
                ROW_HEIGHT + 14.0
            })
            // A search that found nothing shows no page, so no row is marked.
            .selected(entry == section && any_page)
            .selected_background(scheme.selected())
            .on_click(move || show.show_settings_section(entry));
        nav_height += if found.is_empty() {
            ROW_HEIGHT
        } else {
            ROW_HEIGHT + 14.0
        };
        if entry == section && any_page {
            shown_bottom = nav_height;
        }
        if !found.is_empty() {
            // The first hit and how many more, so the count survives the
            // narrow nav where a list of names would be cut.
            let more = match found.len() {
                1 => String::new(),
                n => format!(", +{}", n - 1),
            };
            let first = truncate(&found[0], 26 - more.chars().count());
            row = row.subtitle(format!("{first}{more}"));
        }
        // The rail's mark for the shown row: an accent bar on the raised
        // fill, so the selection never depends on the focus ring.
        if entry == section && any_page {
            row = row.accent_bar(scheme.lanes[0]);
        }
        let row: Box<dyn View> = if entry == section
            && !picker_open
            && !recording
            && !filtering
            && !extension_page_open
            && !binding_focused
        {
            Box::new(row.with_ref(regions.settings_focus))
        } else {
            Box::new(row)
        };
        // A sub-page hangs off a thin guide line, so its parent reads as one.
        nav = if indent > 0.0 {
            nav.child(
                Row::new()
                    .align(Align::Center)
                    .child(
                        Pad::insets(EdgeInsets {
                            left: SUB_GUIDE,
                            right: indent - SUB_GUIDE - 1.0,
                            ..EdgeInsets::default()
                        })
                        .child(Fill::new(1.0, ROW_HEIGHT, scheme.line)),
                    )
                    .child(row),
            )
        } else {
            nav.child(row)
        };
    }
    // A search that finds nothing says so once, beside the nav.
    let pages_height = (body_height() - field_height).max(ROW_HEIGHT);
    let pages: Box<dyn View> = if nav_height > pages_height {
        Box::new(
            Scroll::new()
                .label(tr.settings)
                .restoration_key(tr.settings)
                .scrollbar(true)
                .size(NAV_WIDTH, pages_height)
                .initial_offset((shown_bottom + 8.0 - pages_height).max(0.0))
                .child(nav),
        )
    } else {
        Box::new(
            Column::new()
                .width(NAV_WIDTH)
                .height(pages_height)
                .child(nav),
        )
    };
    // The pages are one list: Tab enters it at the shown page and leaves it
    // for the page beside it; the arrows move through it.
    let nav = Column::new()
        .width(NAV_WIDTH)
        .height(body_height())
        .child(field)
        .child(schnellui::widgets::List::new().child(pages));

    let content: Box<dyn View> = match section {
        // A search that found nothing says so where the page would be,
        // so the last page shown never reads as its result.
        _ if !any_page => Box::new(
            Pad::insets(EdgeInsets {
                top: GAP,
                ..EdgeInsets::default()
            })
            .child(label(scheme, (tr.settings_no_match)(needle.trim()))),
        ),
        SettingsSection::Project => Box::new(super::machines::project_section(ws, scheme)),
        SettingsSection::ProjectAllowed => {
            Box::new(super::machines::project_answers_section(ws, scheme, false))
        }
        SettingsSection::ProjectBlocked => {
            Box::new(super::machines::project_answers_section(ws, scheme, true))
        }
        SettingsSection::Font => Box::new(font_section(ws, scheme)),
        SettingsSection::ColorScheme => Box::new(scheme_section(ws, scheme)),
        SettingsSection::Keyboard => Box::new(keyboard_section(ws, scheme)),
        SettingsSection::Shells => Box::new(shell_section(ws, scheme)),
        SettingsSection::Extensions => Box::new(extension_section(ws, scheme)),
        SettingsSection::Accounts => Box::new(accounts_section(ws, scheme)),
        SettingsSection::Registries => Box::new(registries_section(ws, scheme)),
        SettingsSection::Agents => Box::new(agent_section(ws, scheme)),
        SettingsSection::Remote => Box::new(super::machines::remote_section(ws, scheme)),
        SettingsSection::Vpn => Box::new(super::machines::vpn_section(ws, scheme)),
        SettingsSection::Vms => Box::new(super::machines::vm_section(ws, scheme)),
        SettingsSection::Sandboxing => Box::new(super::machines::sandbox_section(ws, scheme)),
        SettingsSection::SandboxFiles => {
            Box::new(super::machines::sandbox_files_section(ws, scheme))
        }
        SettingsSection::SandboxHosts => {
            Box::new(super::machines::sandbox_hosts_section(ws, scheme))
        }
        SettingsSection::Language => Box::new(language_section(ws, scheme)),
        SettingsSection::Changelog => Box::new(changelog_section(ws, scheme)),
        SettingsSection::Licenses => Box::new(licenses_section(ws, scheme)),
    };

    let close = ws.clone();
    // Exactly as tall as `body_layout` counts it, so a small window keeps
    // the same margin under the dialog as above it.
    let header = Row::new()
        .width(surface_width() - PAD * 2.0)
        .height(HEADER_HEIGHT)
        .justify(Justify::SpaceBetween)
        .align(Align::Center)
        .child(Pad::insets(EdgeInsets::symmetric(PAD, 0.0)).child({
            let mut trail = Row::new()
                .gap(PAD)
                .align(Align::Center)
                .child(ThemeProvider::new(
                    scheme.muted_theme(),
                    Text::new(tr.settings).size(15.0),
                ));
            // A search that found nothing shows no page, so the trail
            // names none either.
            if !any_page {
                trail
            } else {
                // A subpage names its parent page too.
                if let Some(parent) = section.parent() {
                    trail = trail
                        .child(ThemeProvider::new(
                            scheme.muted_theme(),
                            Pad::insets(EdgeInsets {
                                top: 5.0,
                                ..EdgeInsets::default()
                            })
                            .child(Text::new("›").size(15.0)),
                        ))
                        .child(ThemeProvider::new(
                            scheme.muted_theme(),
                            Text::new(parent.title(tr)).size(15.0),
                        ));
                }
                // An open extension page names the extension after the
                // section. The page's own back button returns to the list.
                let open_extension = (section == SettingsSection::Extensions)
                    .then(|| {
                        let inner = ws.borrow();
                        inner.settings_ui.extension_open.then(|| {
                            inner
                                .settings_ui
                                .extension
                                .as_deref()
                                .and_then(|name| {
                                    inner.ext.extensions().find(|ext| ext.name == name)
                                })
                                .map_or_else(
                                    || tr.builtin_page.to_owned(),
                                    |ext| ext.display_name().to_owned(),
                                )
                        })
                    })
                    .flatten();
                trail = trail.child(ThemeProvider::new(
                    scheme.muted_theme(),
                    Pad::insets(EdgeInsets {
                        top: 5.0,
                        ..EdgeInsets::default()
                    })
                    .child(Text::new("›").size(15.0)),
                ));
                match open_extension {
                    Some(name) => trail
                        .child(ThemeProvider::new(
                            scheme.muted_theme(),
                            Text::new(section.title(tr)).size(15.0),
                        ))
                        .child(ThemeProvider::new(
                            scheme.muted_theme(),
                            Pad::insets(EdgeInsets {
                                top: 5.0,
                                ..EdgeInsets::default()
                            })
                            .child(Text::new("›").size(15.0)),
                        ))
                        .child(Text::new(name).size(17.0).bold()),
                    // The page shown is the last crumb, in bold ink, so it
                    // reads as the page's title.
                    None => trail.child(Text::new(section.title(tr)).size(17.0).bold()),
                }
            }
        }))
        .child({
            let mut end = Row::new().gap(PAD).align(Align::Center);
            if let Some(toast) = &toast {
                end = end.child(toast_badge(scheme, tr, toast));
            }
            end.child(
                // Framed, so the way out reads as a button at rest.
                Button::new(tr.close_settings)
                    .display("×")
                    .appearance(ButtonAppearance::Outline)
                    .width(HEADER_HEIGHT)
                    .height(HEADER_HEIGHT)
                    .text_size(18.0)
                    .tooltip(tr.close_escape)
                    .on_click(move || close.close_settings()),
            )
        });
    let body = Row::new()
        .gap(GAP)
        .child(nav)
        .child(Fill::new(1.0, body_height(), scheme.line))
        .child(content);

    let dismiss = ws.clone();
    // Centered on the window, like the palette.
    let x = ((viewport.width - surface_width()) / 2.0).max(PAD);
    Box::new(
        Dialog::new(tr.settings)
            .modal()
            .undecorated()
            .position(DialogPosition::At(Point { x, y: top }))
            // `x` and `top` are window coordinates.
            .viewport_inset(0.0)
            .width(surface_width())
            .max_width(surface_width())
            .backdrop(scheme.backdrop)
            .surface(scheme.rail)
            .padding(PAD)
            .gap(PAD)
            .on_dismiss(move || dismiss.close_settings())
            .child(header)
            .child(body),
    )
}

/// The header's toast: a pill in the positive color for an action that
/// worked, in rose for one that failed.
fn toast_badge(scheme: &Scheme, tr: &'static Strings, toast: &Toast) -> impl View + use<> {
    let (mark, color, name) = if toast.failed {
        ("!", scheme.lanes[3], tr.toast_failed)
    } else {
        ("✓", scheme.lanes[4], tr.toast_done)
    };
    let text = format!("{mark} {}", truncate(&toast.text, TOAST_CHARS));
    ThemeProvider::new(
        Theme {
            attention: color,
            on_accent: scheme.page,
            ..scheme.theme()
        },
        Badge::new(text).name(name),
    )
}

/// A list row with the settings' subtitle size: a step above the widget's
/// default, since subtitles here carry what a setting does.
pub(super) fn list_row(title: impl Into<std::borrow::Cow<'static, str>>) -> ListRow {
    ListRow::new(title).meta_size(12.5)
}

pub(super) fn label<T: Into<std::borrow::Cow<'static, str>>>(
    scheme: &Scheme,
    text: T,
) -> impl View + use<T> {
    ThemeProvider::new(
        scheme.muted_theme(),
        Text::new(text).size(13.0).wrap(WrapMode::Word),
    )
}

/// Reset all settings, at the end of the Project page, apart from the
/// navigation. Once asked, it turns the color of a destructive action
/// until the second click.
pub(super) fn reset_settings(ws: &Workspace, scheme: &Scheme) -> impl View + use<> {
    let (confirm, tr) = {
        let inner = ws.borrow();
        (inner.settings_ui.confirm_reset, inner.tr)
    };
    let reset = ws.clone();
    // A destructive action: rose text in a rose frame, and a rose fill
    // once it asks to be clicked again.
    let rose = scheme.lanes[3];
    Column::new()
        .gap(GAP / 2.0)
        .child(section_title(tr.reset_settings))
        .child(label(scheme, tr.reset_settings_hint))
        .child(ThemeProvider::new(
            Theme {
                text: rose,
                outline: super::theme::mix(scheme.line, rose, 160),
                accent: rose,
                on_accent: scheme.page,
                ..scheme.theme()
            },
            Button::new(tr.reset_settings)
                .display(if confirm {
                    tr.reset_settings_confirm
                } else {
                    tr.reset_settings
                })
                .appearance(if confirm {
                    ButtonAppearance::Solid
                } else {
                    ButtonAppearance::Outline
                })
                .text_size(13.0)
                .height(super::machines::INPUT_HEIGHT)
                .on_click(move || reset.click_reset_settings()),
        ))
}

/// The title of a group of settings on a page: bold ink text with room above it,
/// so a page reads as groups instead of one run of muted lines.
pub(super) fn section_title<T: Into<std::borrow::Cow<'static, str>>>(
    text: T,
) -> impl View + use<T> {
    let text = text.into();
    let found = found_theme(&text);
    // A step under the page's title in the breadcrumb.
    let title = Text::new(text).size(14.5).bold();
    let title: Box<dyn View> = match found {
        Some(theme) => Box::new(ThemeProvider::new(theme, title)),
        None => Box::new(title),
    };
    Pad::insets(EdgeInsets {
        top: GAP * 1.5,
        ..EdgeInsets::default()
    })
    .child(title)
}

/// Muted text that breaks into lines at the extension page's width.
/// Narrower by a form's indent, so a label indented under a row still ends
/// at the page's right edge.
fn wrapped_label<T: Into<std::borrow::Cow<'static, str>>>(
    scheme: &Scheme,
    text: T,
) -> impl View + use<T> {
    wrapped_label_at(scheme, text, ext_row_width() - FORM_INDENT)
}

/// Muted text that breaks into lines at `width`.
fn wrapped_label_at<T: Into<std::borrow::Cow<'static, str>>>(
    scheme: &Scheme,
    text: T,
    width: f32,
) -> impl View + use<T> {
    Column::new().width(width).child(ThemeProvider::new(
        scheme.muted_theme(),
        Text::new(text).size(13.0).wrap(WrapMode::Word),
    ))
}

/// The Shells page: the default shell, the shells the new panel list
/// offers, and where agents find Git Bash.
fn shell_section(ws: &Workspace, scheme: &Scheme) -> impl View + use<> {
    let (current, listed, shells, open, focus, tr) = {
        let inner = ws.borrow();
        (
            inner.settings.shell.clone(),
            inner.settings.shells.clone(),
            inner.shells.clone(),
            inner.settings_ui.picker == Some(Picker::Shell),
            inner.regions.settings_focus,
            inner.tr,
        )
    };
    let toggle = ws.clone();
    let mut dropdown = Dropdown::new(tr.default_shell)
        .min_width(SELECT_WIDTH)
        .text_size(super::machines::INPUT_TEXT_SIZE)
        .open(open)
        .on_toggle(move || toggle.toggle_picker(Picker::Shell));
    let pick = ws.clone();
    dropdown = dropdown.option(
        DropdownOption::new(tr.default_shell_system)
            .selected(current.is_none())
            .on_select(move || pick.pick_default_shell(None)),
    );
    for shell in &shells {
        let choice = ShellChoice::Name(shell.id.to_owned());
        let selected = current.as_ref() == Some(&choice);
        let pick = ws.clone();
        dropdown = dropdown.option(
            DropdownOption::new(shell.name)
                .selected(selected)
                .on_select(move || pick.pick_default_shell(Some(choice.clone()))),
        );
    }
    // A shell settings.json names that the list does not have.
    if let Some(choice) = &current
        && !(shells.iter()).any(|shell| choice.id() == Some(shell.id))
    {
        let name = choice.resolve(&shells).map_or_else(
            || choice.id().unwrap_or_default().to_owned(),
            |(name, _)| name,
        );
        dropdown = dropdown.option(DropdownOption::new(name).selected(true));
    }
    let dropdown: Box<dyn View> = if open {
        Box::new(ThemeProvider::new(
            scheme.control_theme(),
            dropdown.with_ref(focus),
        ))
    } else {
        Box::new(ThemeProvider::new(scheme.control_theme(), dropdown))
    };
    let row_width = content_width() - SCROLLBAR_ROOM;
    let mut list = Column::new().width(row_width);
    for shell in &shells {
        let on = listed.iter().any(|choice| choice.id() == Some(shell.id));
        let program = shell
            .argv
            .iter()
            .map(|a| a.to_string_lossy())
            .collect::<Vec<_>>();
        let id = shell.id;
        let set = ws.clone();
        list = list.child(
            Row::new()
                .width(row_width)
                .align(Align::Center)
                .child(
                    list_row(shell.name)
                        .subtitle(truncate(&program.join(" "), 80))
                        .width(row_width - 44.0),
                )
                .child(Named(
                    shell.name.into(),
                    ThemeProvider::new(
                        scheme.switch_theme(),
                        Switch::new(on).on_toggle(move |on| set.set_shell_listed(id, on)),
                    ),
                )),
        );
    }
    let git_bash = match cmdterm_env::git_bash() {
        Some(path) => (tr.git_bash_found)(&path.to_string_lossy()),
        None => tr.git_bash_missing.to_owned(),
    };
    let wrapped = |text: String| {
        Column::new()
            .width(content_width())
            .child(ThemeProvider::new(
                scheme.muted_theme(),
                Text::new(text).size(12.0).wrap(WrapMode::Word),
            ))
    };
    Column::new()
        .width(content_width())
        .gap(GAP / 2.0)
        .child(section_title(tr.section_shells))
        .child(dropdown)
        .child(wrapped(tr.shells_note.to_owned()))
        .child(section_title(tr.shells_listed))
        .child(list)
        .child(wrapped(git_bash))
}

fn font_section(ws: &Workspace, scheme: &Scheme) -> impl View + use<> {
    let (size, family, fonts, open, query, focus, tr) = {
        let inner = ws.borrow();
        let ui = &inner.settings_ui;
        (
            inner.settings.font_size,
            inner.settings.font_family.clone(),
            ui.fonts.clone().unwrap_or_default(),
            ui.picker == Some(Picker::Font),
            ui.picker_query.clone(),
            inner.regions.settings_focus,
            inner.tr,
        )
    };
    let smaller = ws.clone();
    let larger = ws.clone();
    let size_row = Row::new()
        .gap(8.0)
        .align(Align::Center)
        .child(
            Button::new(tr.smaller_font)
                .display("−")
                .appearance(ButtonAppearance::Outline)
                .width(32.0)
                .height(28.0)
                .disabled(size <= FONT_SIZE_RANGE.0)
                .on_click(move || smaller.step_font_size(-1.0)),
        )
        // The value sits between the buttons, centered on them, so the three
        // read as one control. It has no frame: a framed value reads as a
        // field to type in.
        .child(Named(
            tr.font_size.into(),
            schnellui::widgets::Stack::new()
                .size(FONT_SIZE_FIELD, 28.0)
                .align(Align::Center)
                .justify(Justify::Center)
                // The line box holds room for descenders under the digits;
                // a nudge down centers the digits themselves.
                .child(
                    Pad::insets(EdgeInsets {
                        top: 3.5,
                        ..EdgeInsets::default()
                    })
                    .child(Text::new(format!("{size} px")).size(13.0)),
                ),
        ))
        .child(
            Button::new(tr.larger_font)
                .display("+")
                .appearance(ButtonAppearance::Outline)
                .width(32.0)
                .height(28.0)
                .disabled(size >= FONT_SIZE_RANGE.1)
                .on_click(move || larger.step_font_size(1.0)),
        );

    let current = family.as_deref().unwrap_or(tr.bundled_font);
    // An open list keeps the current choice in its field until a search is
    // typed, so opening it never blanks the value.
    let value = if open && !query.is_empty() {
        query
    } else {
        current.to_owned()
    };
    let toggle = ws.clone();
    let typed = ws.clone();
    let bundled = ws.clone();
    // Under its own heading, one line high like the page's other pickers.
    let mut picker = ComboBox::new(value)
        .hint(tr.font_picker)
        .text_size(super::machines::INPUT_TEXT_SIZE)
        .width(FONT_PICKER_WIDTH)
        .open(open)
        .max_popup_height(FONT_POPUP_HEIGHT)
        .on_toggle(move || toggle.toggle_picker(Picker::Font))
        .on_input({
            let shown = current.to_owned();
            move |query| typed.type_in_picker(Picker::Font, &shown, query)
        })
        .option(
            ComboBoxOption::new(tr.bundled_font)
                .selected(family.is_none())
                .on_select(move || bundled.pick_font_family(None)),
        );
    for name in fonts {
        let pick = ws.clone();
        let selected = family.as_deref() == Some(name.as_str());
        picker = picker.option(
            ComboBoxOption::new(truncate(&name, 48))
                .selected(selected)
                .on_select(move || pick.pick_font_family(Some(name.clone()))),
        );
    }
    let preview = font_preview(scheme, size, family.as_deref(), tr);
    Column::new()
        .width(content_width())
        .gap(GAP / 2.0)
        .child(section_title(tr.size))
        .child(size_row)
        .child(section_title(tr.font_picker))
        .child(if open {
            Box::new(ThemeProvider::new(
                scheme.control_theme(),
                picker.with_ref(focus),
            )) as Box<dyn View>
        } else {
            Box::new(ThemeProvider::new(scheme.control_theme(), picker))
        })
        .child(Fill::new(1.0, GAP / 2.0, Color::TRANSPARENT))
        .child(section_title(tr.font_preview))
        .child(preview)
}

/// Columns of the font page's preview.
const PREVIEW_COLUMNS: usize = 60;

/// A few terminal lines in the chosen family and size, on the terminal's
/// own colors, so a change shows before the user closes the overlay.
fn font_preview(
    scheme: &Scheme,
    size: f32,
    family: Option<&str>,
    tr: &'static Strings,
) -> impl View + use<> {
    use schnellui::widgets::{TerminalCell, TerminalGrid, TerminalGridModel};
    let [sage, brass, periwinkle, _, moss, heather] = scheme.lanes;
    let lines: [&[(&str, Color)]; 6] = [
        &[],
        &[
            ("~/src/app", periwinkle),
            (" main ", scheme.muted),
            ("❯ ", heather),
            ("cargo build --release", scheme.ink),
        ],
        &[("   Compiling ", moss), ("app v0.4.2", scheme.ink)],
        &[("    Finished ", moss), ("release in 12.4s", scheme.ink)],
        &[
            ("fn ", heather),
            ("main", sage),
            ("() { ", scheme.ink),
            ("\"0O 1lI {} [] => != ->\"", brass),
            (" }", scheme.ink),
        ],
        &[],
    ];
    let mut model =
        TerminalGridModel::with_colors(PREVIEW_COLUMNS, lines.len(), scheme.ink, scheme.page);
    for (row, spans) in lines.iter().enumerate() {
        let mut column = 1;
        for (text, color) in spans.iter() {
            for ch in text.chars() {
                let point = schnellui::widgets::TerminalGridPoint { row, column };
                if let Some(cell) = model.cell_mut(point) {
                    *cell = TerminalCell::new(ch.to_string(), *color, scheme.page);
                }
                column += 1;
            }
        }
    }
    let grid = TerminalGrid::new(model)
        .font_size(size)
        .label(tr.font_preview);
    let grid = match family {
        Some(family) => grid.font_family(family.to_owned()),
        None => grid,
    };
    // It ends where the rest of the page ends, clear of the scrollbar.
    Column::new()
        .width(content_width() - SCROLLBAR_ROOM)
        .child(grid)
}

fn scheme_section(ws: &Workspace, scheme: &Scheme) -> impl View + use<> {
    let (current, system_dark, size, family, tr) = {
        let inner = ws.borrow();
        (
            inner.settings.color_scheme.clone(),
            inner.system_dark(),
            inner.settings.font_size,
            inner.settings.font_family.clone(),
            inner.tr,
        )
    };
    let following = resolve(SYSTEM_SCHEME, system_dark);
    // Clear of the scrollbar, so the chosen row's fill ends where the
    // other pages' rows do.
    let row_width = content_width() - SCROLLBAR_ROOM;
    // Each row ends in its scheme's worktree lanes, laid over the row's
    // end, so the row's hover and the chosen row's fill take them in.
    // The chosen row also says "In use" in a pill, after dark or light
    // (`kind`) where its row says that at the end.
    let swatched = |row: ListRow, lanes: [Color; 6], chosen: bool, kind: Option<&str>| {
        let row = row
            .end_room(SWATCHES_WIDTH)
            .selected(chosen)
            .selected_background(scheme.selected());
        let row = if chosen {
            row.accent_bar(scheme.lanes[0])
                .trailing(match kind {
                    Some(kind) => {
                        let mut chars = kind.chars();
                        let first = chars.next().map(|c| c.to_uppercase().collect::<String>());
                        format!(
                            "{}{}, {}",
                            first.unwrap_or_default(),
                            chars.as_str(),
                            tr.scheme_in_use.to_lowercase()
                        )
                    }
                    None => tr.scheme_in_use.to_owned(),
                })
                .trailing_pill(scheme.theme().accent)
        } else {
            row
        };
        Row::new()
            .width(row_width)
            .gap(-SWATCHES_WIDTH)
            .align(Align::Center)
            .child(row)
            .child(lane_swatches(lanes))
    };
    let system = ws.clone();
    let mut list = Column::new().width(content_width()).child(swatched(
        list_row(tr.system)
            // Dark or light as the scheme it follows, as every row says, in
            // the subtitle, where the "In use" pill does not take its place.
            .subtitle((tr.follows_desktop)(&format!(
                "{}, {}",
                following.name,
                if following.dark { tr.dark } else { tr.light }
            )))
            .width(row_width)
            .dot(following.lanes[0])
            .on_click(move || system.set_color_scheme(SYSTEM_SCHEME)),
        following.lanes,
        current == SYSTEM_SCHEME,
        None,
    ));
    for option in SCHEMES {
        let pick = ws.clone();
        list = list.child(swatched(
            list_row(option.name)
                // The System row's subtitle names the scheme it applies.
                .trailing(if option.dark { tr.dark } else { tr.light })
                .width(row_width)
                .height(ROW_HEIGHT)
                .dot(option.lanes[0])
                .on_click(move || pick.set_color_scheme(option.id)),
            option.lanes,
            current == option.id,
            Some(if option.dark { tr.dark } else { tr.light }),
        ));
    }
    Column::new()
        .width(content_width())
        .gap(GAP / 2.0)
        .child(section_title(tr.section_color_scheme))
        .child(list)
        .child(section_title(tr.font_preview))
        .child(font_preview(scheme, size, family.as_deref(), tr))
}

/// Side of a lane swatch at the end of a color scheme row.
const SWATCH: f32 = 10.0;
const SWATCH_GAP: f32 = 4.0;
/// Room of the six swatches, a gap before them and the row's end padding.
pub(super) const SWATCHES_WIDTH: f32 = 6.0 * SWATCH + 5.0 * SWATCH_GAP + 28.0;

/// A scheme's six lane colors as a strip of squares, ending at the row's
/// end padding.
pub(super) fn lane_swatches(lanes: [Color; 6]) -> impl View {
    let mut swatches = Row::new().gap(SWATCH_GAP).align(Align::Center);
    for lane in lanes {
        swatches = swatches.child(Fill::new(SWATCH, SWATCH, lane));
    }
    Row::new()
        .width(SWATCHES_WIDTH)
        .justify(Justify::End)
        .child(
            Pad::insets(EdgeInsets {
                right: 12.0,
                ..EdgeInsets::default()
            })
            .child(swatches),
        )
}

fn keyboard_section(ws: &Workspace, scheme: &Scheme) -> impl View + use<> {
    let (current, open, focus, rows, tr) = {
        let inner = ws.borrow();
        let tr = inner.tr;
        let keymap = &inner.keymap;
        let mut rows: Vec<(String, String, String, crate::keymap::Action)> = keymap
            .bindings
            .iter()
            .map(|binding| {
                let source = match &binding.source {
                    Source::Preset => String::new(),
                    Source::Extension(name) => (tr.from_extension)(name),
                    Source::User => tr.from_user.to_owned(),
                };
                (
                    tr.action(&binding.action),
                    binding.keys.to_string(),
                    source,
                    binding.action.clone(),
                )
            })
            .collect();
        // Actions no key is bound to follow, so every one can get a key
        // here and not only in settings.json.
        for action in crate::keymap::simple_actions() {
            if !rows.iter().any(|row| row.3 == action) {
                rows.push((tr.action(&action), String::new(), String::new(), action));
            }
        }
        // One fixed order, so a row keeps its place when its keys change:
        // a new binding is stored after the preset's.
        let order: Vec<_> = crate::keymap::simple_actions().collect();
        rows.sort_by_key(|row| {
            order
                .iter()
                .position(|action| *action == row.3)
                .unwrap_or(usize::MAX)
        });
        (
            keymap.preset.id,
            inner.settings_ui.picker == Some(Picker::Keymap),
            inner.regions.settings_focus,
            rows,
            tr,
        )
    };
    let toggle = ws.clone();
    let mut picker = Dropdown::new(tr.keymap_picker)
        .min_width(SELECT_WIDTH)
        .text_size(super::machines::INPUT_TEXT_SIZE)
        .open(open)
        .on_toggle(move || toggle.toggle_picker(Picker::Keymap));
    let mut description = "";
    for preset in &PRESETS {
        let pick = ws.clone();
        if current == preset.id {
            description = (tr.preset_description)(preset);
        }
        picker = picker.option(
            DropdownOption::new(preset.name)
                .selected(current == preset.id)
                .on_select(move || pick.pick_keymap(preset.id)),
        );
    }
    let picker: Box<dyn View> = if open {
        Box::new(ThemeProvider::new(
            scheme.control_theme(),
            picker.with_ref(focus),
        ))
    } else {
        Box::new(ThemeProvider::new(scheme.control_theme(), picker))
    };
    let (recording, last_binding, query) = {
        let ui = &ws.borrow().settings_ui;
        (
            ui.recording.clone(),
            ui.last_binding.clone(),
            ui.page_query.clone(),
        )
    };
    // A search that names actions shows only their rows; one that found the
    // page by its own words shows them all.
    let shown_actions = matching_actions(&query, tr);
    // Narrower than the page, so a binding's keys stay near its name.
    let row_width = content_width() - SCROLLBAR_ROOM;
    let mut list = Column::new().width(row_width);
    let reset_width = 84.0;
    for (title, keys, source, action) in rows {
        if !shown_actions.is_empty() && !shown_actions.contains(&title) {
            continue;
        }
        // The row being recorded shows the recorder in place of its keys.
        if recording.as_ref() == Some(&action) {
            // The old keys stay in view until new ones replace them.
            let title = if keys.is_empty() {
                title
            } else {
                (tr.recording_was)(&title, &keys)
            };
            list = list.child(key_recorder(ws, scheme, tr, title, row_width, focus));
            continue;
        }
        // A binding the user changed can go back to its default.
        let changed = ws.binding_changed(&action);
        let reset_for = (tr.reset_binding_for)(&title);
        let reset_action = action.clone();
        let is_last = recording.is_none() && last_binding.as_ref() == Some(&action);
        let record = ws.clone();
        // The keys sit on a keycap pill, so the eye finds them across the
        // row and they read as the thing a click changes.
        let unset = keys.is_empty();
        let row = found_row(list_row(title.clone()), &title)
            .trailing(if unset {
                tr.binding_unset.to_owned()
            } else {
                keys
            })
            .width(if changed {
                row_width - reset_width
            } else {
                row_width
            })
            .on_click(move || record.record_binding(Some(action.clone())));
        // An action without keys shows muted text without a keycap: the
        // same place to click as a binding, but never read as one.
        let row = if unset {
            row
        } else {
            row.trailing_pill(scheme.keycap())
        };
        let row = if source.is_empty() {
            row.height(ROW_HEIGHT)
        } else {
            row.subtitle(source)
        };
        let row = ThemeProvider::new(
            Theme {
                on_accent: scheme.ink,
                ..scheme.theme()
            },
            row,
        );
        // The row whose binding was just recorded or reset takes focus
        // back, so Enter does not land on the next one.
        let row: Box<dyn View> = if is_last {
            Box::new(row.with_ref(focus))
        } else {
            Box::new(row)
        };
        if changed {
            let reset = ws.clone();
            list = list.child(
                Row::new()
                    .width(row_width)
                    .align(Align::Center)
                    .child(row)
                    .child(Named(
                        reset_for.into(),
                        Button::new(tr.reset_binding)
                            .appearance(ButtonAppearance::Ghost)
                            .width(reset_width - 8.0)
                            .height(26.0)
                            .text_size(12.5)
                            .on_click(move || reset.reset_binding(&reset_action)),
                    )),
            );
        } else {
            list = list.child(row);
        }
    }
    // The whole page scrolls as one, so the list never sits in a second
    // scroll box cut at the dialog's edge.
    let page = Column::new()
        .width(content_width() - SCROLLBAR_ROOM)
        .gap(GAP / 2.0)
        .child(section_title(tr.keymap_picker))
        .child(picker)
        .child(label(scheme, description))
        .child(section_title(tr.bindings))
        // The hint says how to change a binding before the list it applies to.
        .child(label(scheme, tr.bindings_hint))
        .child(list);
    Scroll::new()
        .label(tr.binding_list)
        .restoration_key(tr.binding_list)
        .scrollbar(true)
        .size(content_width(), body_height())
        .child(bottom_room(page))
}

/// Accessible name of the key recorder on the Keyboard page; `build_app`
/// routes its keys to [`Workspace::record_key`].
pub const KEY_RECORDER: &str = "Key recorder";

/// The row of a binding that waits for its new key: the action and a
/// prompt in the accent color, on a one-row grid that takes the keys.
fn key_recorder(
    ws: &Workspace,
    scheme: &Scheme,
    tr: &'static Strings,
    title: String,
    width: f32,
    focus: schnellui::scene::ComponentRef,
) -> impl View + use<> {
    use schnellui::widgets::{TerminalGrid, TerminalGridModel};
    // The row looks like the others, marked and with the prompt in place of
    // the keys. Keys go to a grid of one tiny blank cell at its end, which
    // holds the focus because only a grid gets raw key events with every
    // modifier.
    let model = TerminalGridModel::with_colors(1, 1, scheme.raised, scheme.raised);
    let grid = TerminalGrid::new(model)
        .font_size(1.0)
        .label(KEY_RECORDER)
        .with_ref(focus);
    let cancel = ws.clone();
    Row::new()
        .width(width)
        .height(ROW_HEIGHT)
        .align(Align::Center)
        .child(
            list_row(title)
                .trailing(tr.press_keys)
                .trailing_pill(scheme.theme().accent)
                .width(width - RECORDER_GRID_WIDTH)
                .height(ROW_HEIGHT)
                .selected(true)
                .selected_background(scheme.selected())
                .accent_bar(scheme.lanes[0])
                .on_click(move || cancel.record_binding(None)),
        )
        .child(grid)
}

/// Room the recorder's blank grid takes at the end of its row.
const RECORDER_GRID_WIDTH: f32 = 2.0;

/// Width of the extension list and of an extension's page: each takes the
/// whole section.
fn ext_page_width() -> f32 {
    content_width()
}
/// Width of a row in the extension list, beside its scroll bar.
fn ext_nav_row_width() -> f32 {
    ext_page_width() - SCROLLBAR_ROOM
}
/// Width of the find field and of the state filter above the list.
const EXT_FIND_WIDTH: f32 = 300.0;
const EXT_FILTER_WIDTH: f32 = 210.0;
/// Characters of an extension's description its list row shows.
const EXT_DESCRIPTION_CHARS: usize = 96;
/// Width of a row in an extension's page, beside its scroll bar.
fn ext_row_width() -> f32 {
    ext_page_width() - SCROLLBAR_ROOM
}

/// One guard row: title, where it comes from and applies, and its mode.
struct GuardRow {
    id: String,
    title: String,
    from: String,
    mode: GuardMode,
}

impl GuardRow {
    fn new(guard: &cmdterm_env::GuardRule, mode: GuardMode, tr: &Strings) -> Self {
        // Built-in guards sit on the built-in page, so they name no source.
        let extension = match guard.id.rsplit_once('.').map_or("", |(ext, _)| ext) {
            "cmdterm" => None,
            ext => Some(ext),
        };
        let hosts = if guard.id == cmdterm_env::KEYRING_GUARD {
            tr.keyring_scope.to_owned()
        } else if guard.id == cmdterm_env::AGENT_SETTINGS_GUARD {
            tr.agent_settings_scope.to_owned()
        } else if guard.id == cmdterm_env::OPEN_URL_GUARD {
            tr.open_url_scope.to_owned()
        } else if guard.hosts.iter().any(|h| h == "*") {
            tr.every_host.to_owned()
        } else {
            guard.hosts.join(", ")
        };
        Self {
            id: guard.id.clone(),
            title: guard.title.clone(),
            from: truncate(
                &match extension {
                    Some(extension) => (tr.guard_from)(extension, &hosts),
                    // A host name keeps its case; "every host" starts a line.
                    None if hosts.contains('.') => hosts,
                    None => upper_first(&hosts),
                },
                72,
            ),
            mode,
        }
    }
}

/// `text` with its first letter in upper case.
fn upper_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Buttons side by side that pick one of `options`: the current one
/// filled, the others outlined, so the row reads as one segmented control.
#[allow(clippy::too_many_arguments)]
fn segmented<T: Copy + PartialEq + 'static>(
    mut row: Row,
    title: &str,
    current: T,
    options: Vec<(T, String)>,
    width: f32,
    tr: &'static Strings,
    tint: impl Fn(T) -> Option<Color>,
    pick: impl Fn(T) + Clone + 'static,
) -> Row {
    for (value, text) in options {
        let pick = pick.clone();
        let appearance = if value == current {
            ButtonAppearance::Solid
        } else {
            ButtonAppearance::Outline
        };
        let chosen = value == current;
        row = row.child(Named(
            (tr.guard_mode_for)(title, &text).into(),
            tinted_segment(
                chosen,
                &text.clone(),
                Button::new(text)
                    .appearance(appearance)
                    .width(width)
                    .text_size(12.0)
                    .on_click(move || pick(value)),
                tint(value),
            ),
        ));
    }
    row
}

/// The fill of a chosen guard mode: rose for Block, brass for Allow, the
/// accent for Ask.
fn guard_tint(scheme: &Scheme) -> impl Fn(GuardMode) -> Option<Color> + use<> {
    let (block, allow) = (scheme.lanes[3], scheme.lanes[1]);
    move |mode| match mode {
        GuardMode::Ask => None,
        GuardMode::Block => Some(block),
        GuardMode::Allow => Some(allow),
    }
}

fn guard_row(ws: &Workspace, guard: GuardRow, tr: &'static Strings) -> Row {
    let mode_width = 62.0;
    let row = Row::new()
        .width(ext_row_width())
        .align(Align::Center)
        .child(
            // Shown in sentence case; the buttons keep the guard's own title in
            // their names.
            list_row(upper_first(&guard.title))
                .subtitle(guard.from)
                .width(ext_row_width() - mode_width * 3.0),
        );
    let pick = ws.clone();
    let id = guard.id;
    segmented(
        row,
        &guard.title,
        guard.mode,
        vec![
            (GuardMode::Ask, tr.guard_ask.to_owned()),
            (GuardMode::Block, tr.guard_block.to_owned()),
            (GuardMode::Allow, tr.guard_allow.to_owned()),
        ],
        mode_width,
        tr,
        guard_tint(ws.borrow().scheme),
        move |mode| pick.set_guard_mode(&id, mode),
    )
}

/// The mode of every forge's guards of `kind`.
fn change_request_row(
    ws: &Workspace,
    kind: GuardKind,
    mode: GuardMode,
    tr: &'static Strings,
) -> Row {
    let mode_width = 62.0;
    let title = match kind {
        GuardKind::Create => tr.change_request_create,
        GuardKind::Comment => tr.change_request_comment,
        GuardKind::Merge => tr.change_request_merge,
    };
    let row = Row::new()
        .width(ext_row_width())
        .align(Align::Center)
        .child(
            list_row(title)
                .subtitle((tr.change_request_subtitle)(kind.name()))
                .width(ext_row_width() - mode_width * 3.0),
        );
    let pick = ws.clone();
    segmented(
        row,
        title,
        mode,
        vec![
            (GuardMode::Ask, tr.guard_ask.to_owned()),
            (GuardMode::Block, tr.guard_block.to_owned()),
            (GuardMode::Allow, tr.guard_allow.to_owned()),
        ],
        mode_width,
        tr,
        guard_tint(ws.borrow().scheme),
        move |mode| pick.set_change_request_mode(kind, mode),
    )
}

/// A heading inside an extension's page.
fn heading(text: impl Into<std::borrow::Cow<'static, str>>) -> impl View {
    Pad::insets(EdgeInsets {
        top: GAP / 2.0,
        ..EdgeInsets::default()
    })
    .child(Text::new(text).size(14.0).bold())
}

/// The Extensions section. It lists every extension across the whole
/// section, under a find field and a state filter, with the app's own
/// rules first; picking one opens its page in the list's place, with a way
/// back.
fn extension_section(ws: &Workspace, scheme: &Scheme) -> impl View + use<> {
    let (open, selected, focus, tr) = {
        let inner = ws.borrow();
        // An extension that went away shows the list again.
        let selected = inner
            .settings_ui
            .extension
            .clone()
            .filter(|name| inner.ext.extensions().any(|ext| &ext.name == name));
        let open = inner.settings_ui.extension_open
            && (inner.settings_ui.extension.is_none() || selected.is_some());
        // The back button holds focus unless a picker or search does.
        let focus = (inner.settings_ui.picker.is_none()
            && inner.settings_ui.recording.is_none()
            && !inner.settings_ui.finding_page)
            .then_some(inner.regions.settings_focus);
        (open, selected, focus, inner.tr)
    };
    if !open {
        return Box::new(extension_list(ws, scheme)) as Box<dyn View>;
    }
    let page: Box<dyn View> = match &selected {
        None => Box::new(builtin_page(ws, scheme)),
        Some(name) => Box::new(extension_page(ws, scheme, name)),
    };
    let back = ws.clone();
    Box::new(
        Column::new()
            .gap(GAP / 2.0)
            .child({
                let button = Button::new(tr.all_extensions)
                    .display(format!("‹  {}", tr.all_extensions))
                    .appearance(ButtonAppearance::Ghost)
                    .text_size(13.0)
                    .height(28.0)
                    .on_click(move || back.show_extension_list());
                let view: Box<dyn View> = match focus {
                    Some(focus) => Box::new(button.with_ref(focus)),
                    None => Box::new(button),
                };
                // Room under it, so it does not crowd the page's heading.
                Pad::insets(EdgeInsets {
                    bottom: 6.0,
                    ..EdgeInsets::default()
                })
                .child(view)
            })
            .child(
                Scroll::new()
                    .label(tr.extension_page)
                    .restoration_key(tr.extension_page)
                    .scrollbar(true)
                    .size(ext_page_width(), body_height() - 28.0 - GAP / 2.0)
                    .child(bottom_room(page)),
            ),
    )
}

/// The list of every extension: find field and state filter on one line,
/// the built-in rules, then one row per extension with its description and
/// state, and where they load from at the end.
fn extension_list(ws: &Workspace, scheme: &Scheme) -> impl View + use<> {
    let (names, any, filter, query, finding, focus, dir, tr) = {
        let inner = ws.borrow();
        let filter = inner.settings.extension_filter;
        let any = inner.ext.extensions().next().is_some();
        let query = inner.settings_ui.extension_query.clone();
        let needle = query.trim().to_lowercase();
        let names: Vec<(String, String, String, String, bool, bool)> = inner
            .ext
            .extensions()
            .filter(|ext| filter.shows(ext.turned_off))
            .filter(|ext| {
                needle.is_empty()
                    || ext.name.to_lowercase().contains(&needle)
                    || ext.display_name().to_lowercase().contains(&needle)
                    || ext
                        .description
                        .as_deref()
                        .is_some_and(|text| text.to_lowercase().contains(&needle))
            })
            .map(|ext| {
                let fine = !ext.turned_off
                    && ext.enabled
                    && ext.last_error.is_none()
                    && ext.missing.is_empty();
                // The list shows a description's first sentence; the
                // extension's page has the rest.
                let description = ext.description.as_deref().unwrap_or_default();
                let summary = match description.find(". ") {
                    Some(end) => &description[..=end],
                    None => description,
                };
                let summary = summary.to_owned();
                (
                    ext.name.clone(),
                    ext.display_name().to_owned(),
                    summary,
                    extension_status(ext, inner.tr),
                    fine,
                    !ext.turned_off,
                )
            })
            .collect();
        (
            names,
            any,
            filter,
            query,
            inner.settings_ui.finding_extension,
            inner.regions.settings_focus,
            if crate::bundle::THIRD_PARTY_EXTENSIONS {
                inner.extensions_dir.clone()
            } else {
                inner.shipped_dir.clone()
            },
            inner.tr,
        )
    };
    let mut list = Column::new().width(ext_nav_row_width()).gap(GAP / 4.0);
    let find = ws.clone();
    let field = TextInput::new(query)
        .hint(tr.find_extension)
        .text_size(13.0)
        .width(EXT_FIND_WIDTH)
        .height(28.0)
        .on_input(move |text| find.set_extension_query(text));
    // Typing rebuilds the list; the field keeps focus through it.
    let field: Box<dyn View> = if finding {
        Box::new(ThemeProvider::new(
            scheme.field_theme(),
            field.with_ref(focus),
        ))
    } else {
        Box::new(ThemeProvider::new(scheme.field_theme(), field))
    };
    let pick = ws.clone();
    // The filter is named, so it does not read as an on/off switch.
    let filter_row = segmented(
        Row::new().width(EXT_FILTER_WIDTH),
        tr.extension_filter,
        filter,
        vec![
            (ExtensionFilter::All, tr.filter_all.to_owned()),
            (ExtensionFilter::Active, tr.filter_active.to_owned()),
            (ExtensionFilter::Disabled, tr.filter_disabled.to_owned()),
        ],
        EXT_FILTER_WIDTH / 3.0,
        tr,
        |_| None,
        move |filter| pick.set_extension_filter(filter),
    );
    list = list.child(
        Row::new()
            .width(ext_nav_row_width())
            .justify(Justify::SpaceBetween)
            .align(Align::Center)
            .child(field)
            .child(
                Row::new()
                    .gap(8.0)
                    .align(Align::Center)
                    .child(label(scheme, tr.extension_filter))
                    .child(filter_row),
            ),
    );
    list = list.child(Fill::new(1.0, GAP / 2.0, Color::TRANSPARENT));
    // Room for the switch and the arrow at the end of each row: the
    // switch turns the extension on or off, the arrow says the row opens
    // its page. Both sit centered on the row.
    let switch_width = 44.0;
    // Room right of the switch for the row's arrow and a gap before it.
    let arrow_width = 32.0;
    // The arrow is the row's own trailing text at its end, so the row's
    // hover and click take it in; the switch lies over the row before it.
    let show = ws.clone();
    list = list.child(
        list_row(tr.builtin_page)
            .subtitle(tr.builtin_subtitle)
            .dot(Color::TRANSPARENT)
            .trailing("›")
            .trailing_centered()
            .width(ext_nav_row_width())
            .on_click(move || show.show_extension_page(None)),
    );
    // A line keeps the built-in rules apart from the extensions.
    list = list.child(Fill::new(ext_nav_row_width(), 1.0, scheme.line));
    if !any {
        list = list.child(label(scheme, tr.no_extensions));
    } else if names.is_empty() {
        list = list.child(label(scheme, tr.no_matching_extensions));
    }
    for (name, title, description, status, fine, on) in names {
        let show = ws.clone();
        let target = name.clone();
        // The dot says how the extension is doing, the switch at the row's
        // end turns it on or off in place, and the line under the name says
        // what it does, or what it needs when that is the news.
        // The switch says on or off, so only an extension that needs
        // something keeps a dot, in the warning color.
        let (dot, subtitle, warn) = if fine || status == tr.off {
            (Color::TRANSPARENT, description, false)
        } else {
            (scheme.lanes[1], status, true)
        };
        // The row spans the list, so its hover and its click take in the
        // switch and the arrow laid over its end.
        let switch_name = (tr.enable_extension)(&title);
        let mut row = list_row(title)
            .dot(dot)
            .trailing("›")
            .trailing_centered()
            .width(ext_nav_row_width())
            .on_click(move || show.show_extension_page(Some(target.clone())));
        if !subtitle.is_empty() {
            row = row.subtitle(truncate(&subtitle, EXT_DESCRIPTION_CHARS));
        }
        if warn {
            row = row.subtitle_color(scheme.lanes[1]);
        }
        let toggle = ws.clone();
        let target = name.clone();
        list = list.child(
            Row::new()
                .width(ext_nav_row_width())
                .gap(-(switch_width + arrow_width))
                .align(Align::Center)
                .child(row)
                // One child, since the negative gap comes between every two.
                .child(
                    Pad::insets(EdgeInsets {
                        right: arrow_width,
                        ..EdgeInsets::default()
                    })
                    .child(
                        Row::new()
                            .width(switch_width)
                            .justify(Justify::End)
                            .child(Named(
                                switch_name.into(),
                                ThemeProvider::new(
                                    scheme.switch_theme(),
                                    Switch::new(on).on_toggle(move |on| {
                                        toggle.set_extension_enabled(&target, on)
                                    }),
                                ),
                            )),
                    ),
                ),
        );
    }
    // Where every extension loads from, and the reload after editing them,
    // close the list they belong to.
    let reload = ws.clone();
    list = list
        .child(Fill::new(ext_nav_row_width(), 1.0, scheme.line))
        .child(
            Row::new()
                .width(ext_nav_row_width())
                .justify(Justify::SpaceBetween)
                .align(Align::Center)
                .child(label(scheme, (tr.loaded_from)(&tail_path(&dir, 60))))
                .child(
                    Button::new(tr.reload_extensions)
                        .appearance(ButtonAppearance::Outline)
                        .tooltip(tr.reload_note)
                        .text_size(13.0)
                        .height(28.0)
                        .on_click(move || reload.reload_extensions()),
                ),
        );
    Scroll::new()
        .label(tr.extension_list)
        .restoration_key(tr.extension_list)
        .scrollbar(true)
        .size(ext_page_width(), body_height())
        .child(bottom_room(list))
}

/// One short line on how an extension is doing.
fn extension_status(ext: &crate::ext::ExtensionInfo, tr: &Strings) -> String {
    if ext.turned_off {
        tr.off.to_owned()
    } else if let Some(error) = &ext.last_error {
        (tr.failed_with)(&error.to_string())
    } else if !ext.missing.is_empty() {
        (tr.needs_permissions)(&ext.missing.names().join(", "))
    } else if ext.enabled {
        tr.on.to_owned()
    } else {
        tr.failed.to_owned()
    }
}

fn builtin_page(ws: &Workspace, scheme: &Scheme) -> impl View + use<> {
    let (guards, change_requests, tr) = {
        let inner = ws.borrow();
        // A guard a shipped extension lists shows on that extension's page.
        let guards: Vec<GuardRow> = cmdterm_env::builtin_guards()
            .iter()
            .filter(|g| {
                crate::bundle::guard_extension(&g.id)
                    .is_none_or(|owner| !inner.ext.extensions().any(|ext| ext.name == owner))
            })
            .map(|g| {
                GuardRow::new(
                    g,
                    inner.settings.sandbox.guard_mode(&g.id, g.kind),
                    inner.tr,
                )
            })
            .collect();
        let change_requests = inner.settings.sandbox.change_requests;
        (guards, change_requests, inner.tr)
    };
    let always = always_allowed(ws);
    let mut page = Column::new()
        .width(ext_row_width())
        .gap(GAP / 2.0)
        .child(Text::new(tr.builtin_page).size(17.0).bold())
        .child(label(scheme, tr.builtin_description))
        .child(heading(tr.guards_heading))
        .child(label(scheme, tr.guards_note));
    for guard in guards {
        page = page.child(guard_row(ws, guard, tr));
    }
    page = page
        .child(heading(tr.change_requests_heading))
        .child(wrapped_label(scheme, tr.change_requests_note));
    for kind in GuardKind::ALL {
        page = page.child(change_request_row(ws, kind, change_requests.get(kind), tr));
    }
    if !always.is_empty() {
        page = page
            .child(heading(tr.always_heading))
            .child(label(scheme, tr.always_note));
    }
    for (project, index, title, subtitle) in always {
        let remove = ws.clone();
        // Each Remove names its entry, so they differ for assistive tools.
        let name = format!("{} ({subtitle}): {}", tr.always_remove, title);
        let revoke = move || match index {
            Some(index) => remove.revoke_always_allowed(project, index),
            None => remove.revoke_launches(project),
        };
        page = page.child(
            Row::new()
                .width(ext_row_width())
                .align(Align::Center)
                .child(
                    list_row(title)
                        .subtitle(subtitle)
                        .width(ext_row_width() - 100.0),
                )
                .child(Named(
                    name.into(),
                    Button::new(tr.always_remove)
                        .appearance(ButtonAppearance::Ghost)
                        .text_size(13.0)
                        .on_click(revoke),
                )),
        );
    }
    page
}

/// What projects allowed for good, as (project id, index in
/// `always_allowed` or `None` for launches, title, project name), with the
/// guard's title where a guard of that id is known.
fn always_allowed(ws: &Workspace) -> Vec<(u64, Option<usize>, String, String)> {
    let inner = ws.borrow();
    let guards = crate::runtime::all_guards(inner.ext.sandbox_rules());
    let mut rows = Vec::new();
    for project in &inner.state.projects {
        for (index, entry) in project.always_allowed.iter().enumerate() {
            let title = (guards.iter())
                .find(|g| g.id == entry.guard)
                .map_or(entry.guard.as_str(), |g| g.title.as_str());
            let title = truncate(&(inner.tr.always_entry)(title, &entry.host), 72);
            rows.push((project.id, Some(index), title, project.name.clone()));
        }
        if project.allows_launches {
            let title = inner.tr.always_launches.to_owned();
            rows.push((project.id, None, title, project.name.clone()));
        }
    }
    rows
}

fn extension_page(ws: &Workspace, scheme: &Scheme, name: &str) -> impl View + use<> {
    let inner = ws.borrow();
    let tr = inner.tr;
    let Some(ext) = inner.ext.extensions().find(|ext| ext.name == name).cloned() else {
        return Column::new().width(ext_row_width());
    };
    let granted = crate::workspace::granted_permissions(&inner.settings)
        .get(name)
        .is_some_and(|names| !names.is_empty());
    let own_values = inner.settings.extension_settings.get(name).cloned();
    let has_forges = inner
        .ext
        .forges()
        .iter()
        .any(|forge| forge.extension == name);
    let prefix = format!("{name}.");
    // The proxy's own guards this extension stands for, then its own.
    let builtin = cmdterm_env::builtin_guards();
    let guards: Vec<GuardRow> = builtin
        .iter()
        .filter(|g| crate::bundle::guard_extension(&g.id) == Some(name))
        .chain(
            inner
                .ext
                .sandbox_rules()
                .guards
                .iter()
                .filter(|g| g.id.starts_with(&prefix)),
        )
        .map(|g| GuardRow::new(g, inner.settings.sandbox.guard_mode(&g.id, g.kind), tr))
        .collect();
    drop(inner);

    let mut page = Column::new()
        .width(ext_row_width())
        .gap(GAP / 2.0)
        .child(
            Row::new()
                .gap(8.0)
                .align(Align::End)
                .child(Text::new(ext.display_name().to_owned()).size(17.0).bold())
                .child(if ext.display_name().eq_ignore_ascii_case(&ext.name) {
                    Box::new(Fill::new(0.0, 0.0, Color::TRANSPARENT)) as Box<dyn View>
                } else {
                    Box::new(ThemeProvider::new(
                        scheme.muted_theme(),
                        Text::new(ext.name.clone()).size(13.0),
                    ))
                }),
        )
        .child(wrapped_label(
            scheme,
            ext.description
                .clone()
                .unwrap_or_else(|| tr.no_description.to_owned()),
        ));

    // Status, with the switch that turns it on or off.
    let toggle = ws.clone();
    let target = ext.name.clone();
    let switch_width = 44.0;
    page = page.child(
        Row::new()
            .width(ext_row_width())
            .justify(Justify::SpaceBetween)
            .align(Align::Center)
            .child({
                // The switch says on or off; the line under the title only
                // has news, such as an error or a missing permission.
                let status = extension_status(&ext, tr);
                let row = list_row(tr.extension_enabled).width(ext_row_width() - switch_width);
                if status == tr.on || status == tr.off {
                    row
                } else {
                    row.subtitle(truncate(&status, 90))
                        .subtitle_color(scheme.lanes[1])
                }
            })
            .child(Named(
                (tr.enable_extension)(ext.display_name()).into(),
                ThemeProvider::new(
                    scheme.switch_theme(),
                    Switch::new(!ext.turned_off)
                        .on_toggle(move |on| toggle.set_extension_enabled(&target, on)),
                ),
            )),
    );

    // Permissions, with Grant or Revoke.
    page = page.child(heading(tr.permissions_heading));
    if ext.permissions.is_empty() {
        page = page.child(label(scheme, tr.no_permissions));
    } else {
        let action = if !ext.missing.is_empty() {
            Some(true)
        } else {
            granted.then_some(false)
        };
        let action_width = 120.0;
        let mut row = Row::new()
            .width(ext_row_width())
            .align(Align::Center)
            .child(
                Pad::insets(EdgeInsets::default()).child(label(scheme, tr.permissions_switch_note)),
            );
        if let Some(grant) = action {
            let act = ws.clone();
            let target = ext.name.clone();
            let (text, accessible) = if grant {
                (tr.grant, (tr.grant_for)(&ext.name))
            } else {
                (tr.revoke, (tr.revoke_for)(&ext.name))
            };
            row = row.justify(Justify::SpaceBetween).child(Named(
                accessible.into(),
                Button::new(text)
                    .appearance(if grant {
                        ButtonAppearance::Solid
                    } else {
                        ButtonAppearance::Outline
                    })
                    .width(action_width)
                    .text_size(12.0)
                    .on_click(move || {
                        if grant {
                            act.grant_extension_permissions(&target);
                        } else {
                            act.revoke_extension_permissions(&target);
                        }
                    }),
            ));
        }
        page = page.child(row);

        // One switch per permission: what the extension actually runs with.
        // The rest stay off until the user turns them on.
        for permission in ext.permissions.iter() {
            let on = !ext.missing.contains(permission);
            let act = ws.clone();
            let target = ext.name.clone();
            let held = permission;
            page = page.child(
                Row::new()
                    .width(ext_row_width())
                    .justify(Justify::SpaceBetween)
                    .align(Align::Center)
                    .child({
                        // What the permission lets the extension do, with
                        // its manifest name under it. Only those that read
                        // this machine's secrets, run its programs or
                        // change what sandboxes reach say so in the warning
                        // color: a tag on every row would mean nothing.
                        let name = permission.name();
                        let sensitive = matches!(name, "secrets" | "process" | "sandbox");
                        let row = match (tr.permission_note)(name) {
                            "" => list_row(name),
                            note => list_row(note).subtitle(if sensitive {
                                (tr.permission_sensitive)(name)
                            } else {
                                name.to_owned()
                            }),
                        }
                        .width(ext_row_width() - switch_width);
                        if sensitive {
                            row.subtitle_color(scheme.lanes[1])
                        } else {
                            row
                        }
                    })
                    .child(Named(
                        (tr.permission_for)(&ext.name, permission.name()).into(),
                        ThemeProvider::new(
                            scheme.switch_theme(),
                            Switch::new(on).on_toggle(move |on| {
                                act.set_extension_permission(&target, held, on)
                            }),
                        ),
                    )),
            );
        }
    }

    // The settings its manifest declares.
    if !ext.settings.is_empty() {
        page = page.child(heading(tr.settings_heading));
    }
    for spec in &ext.settings {
        let value = own_values
            .as_ref()
            .and_then(|own| own.get(&spec.key))
            .and_then(|value| serde_json::from_value::<SettingValue>(value.clone()).ok())
            .filter(|value| spec.kind.fits(value))
            .unwrap_or_else(|| spec.default.clone());
        page = page.child(setting_control(ws, scheme, &ext.name, spec, value, tr));
    }

    if has_forges {
        let open = ws.clone();
        page = page.child(heading(tr.forge_accounts)).child(
            Row::new()
                .width(ext_row_width())
                .align(Align::Center)
                .justify(Justify::SpaceBetween)
                .child(label(scheme, tr.ext_accounts_moved))
                .child(Named(
                    tr.open_accounts.into(),
                    Button::new(tr.open_accounts)
                        .appearance(ButtonAppearance::Outline)
                        .text_size(12.0)
                        .on_click(move || open.show_settings_section(SettingsSection::Accounts)),
                )),
        );
    }

    // What its requests may reach.
    if ext.permissions.contains(crate::ext::Permission::Network) {
        page = page
            .child(heading(tr.ext_hosts_heading))
            .child(wrapped_label(scheme, tr.ext_hosts_note));
        if ext.hosts.is_empty() {
            page = page.child(label(scheme, tr.ext_hosts_none));
        }
        for host in &ext.hosts {
            page = page.child(list_row(host.clone()).width(ext_row_width()));
        }
    }

    if !guards.is_empty() {
        page = page
            .child(heading(tr.ext_guards_heading))
            .child(label(scheme, tr.guards_note));
    }
    for guard in guards {
        page = page.child(guard_row(ws, guard, tr));
    }
    page
}

/// The Accounts page: every forge's accounts with their sign-in. The
/// registry logins have their own page under it.
fn accounts_section(ws: &Workspace, scheme: &Scheme) -> impl View + use<> {
    let tr = ws.tr();
    let mut page = Column::new()
        .width(account_width())
        .gap(GAP / 2.0)
        .child(section_title(tr.accounts_forges))
        .child(wrapped_label(scheme, tr.accounts_forges_note));
    let (mut forges, used) = {
        let inner = ws.borrow();
        (inner.ext.forges().to_vec(), inner.active_forge())
    };
    forges.sort_by(|a, b| a.title.cmp(&b.title));
    // The forge the active project's remote is on comes first, named as
    // such, since it is the one whose account the Agents tab needs.
    if let Some((id, project)) = used
        && let Some(at) = forges.iter().position(|forge| forge.id == id)
    {
        let mut forge = forges.remove(at);
        forge.title = (tr.forge_used_by)(&forge.title, &project);
        forges.insert(0, forge);
    }
    if forges.is_empty() {
        page = page.child(wrapped_label(scheme, tr.accounts_no_forges));
    }
    for forge in &forges {
        page = forge_accounts(page, ws, scheme, forge);
    }
    // One menu adds an account on another host for the forges whose rows
    // have no heading to hold that button.
    let singles: Vec<_> = forges.iter().filter(|f| f.hosts.len() == 1).collect();
    if !singles.is_empty() {
        let (open, focus) = {
            let inner = ws.borrow();
            (
                inner.settings_ui.picker == Some(Picker::ForgeHost),
                inner.regions.settings_focus,
            )
        };
        let toggle = ws.clone();
        let mut menu = Dropdown::new(tr.forge_other_host)
            .placeholder(tr.forge_other_host)
            .min_width(SELECT_WIDTH)
            .text_size(13.0)
            .open(open)
            .on_toggle(move || toggle.toggle_picker(Picker::ForgeHost));
        for forge in singles {
            let pick = ws.clone();
            let key = format!("{}+add", forge.id);
            menu = menu.option(DropdownOption::new(forge.title.clone()).on_select(move || {
                pick.close_picker();
                pick.toggle_account_form(&key);
            }));
        }
        let menu: Box<dyn View> = if open {
            Box::new(ThemeProvider::new(
                scheme.control_theme(),
                menu.with_ref(focus),
            ))
        } else {
            Box::new(ThemeProvider::new(scheme.control_theme(), menu))
        };
        page = page.child(
            Pad::insets(EdgeInsets {
                top: 4.0,
                ..EdgeInsets::default()
            })
            .child(menu),
        );
    }
    Scroll::new()
        .label(tr.section_accounts)
        .restoration_key(tr.section_accounts)
        .scrollbar(true)
        .size(content_width(), body_height())
        .child(bottom_room(page))
}

/// Whether `text` has a word that starts with `word`, so "tab" finds "Close
/// tab" but not "writable". Both are lowercase.
fn starts_a_word(text: &str, word: &str) -> bool {
    text.split(|c: char| !c.is_alphanumeric())
        .any(|part| part.starts_with(word))
}

/// Whether every word of `query` starts a word of `text`.
fn words_match(text: &str, query: &str) -> bool {
    let text = text.to_lowercase();
    query
        .split_whitespace()
        .all(|word| starts_a_word(&text, &word.to_lowercase()))
}

/// The names of the actions the Keyboard page lists that hold every word of
/// `query`, so a search for a shortcut finds its row.
fn matching_actions(query: &str, tr: &Strings) -> Vec<String> {
    if query.trim().is_empty() {
        return Vec::new();
    }
    crate::keymap::simple_actions()
        .map(|action| tr.action(&action))
        .filter(|name| words_match(name, query))
        .collect()
}

/// Whether every word of `query` starts a word in the title of settings
/// page `page` or in the words that say what it holds. The Keyboard page
/// also matches the names of its actions.
fn page_matches(page: SettingsSection, query: &str, tr: &Strings) -> bool {
    let text = format!("{} {}", page.title(tr), (tr.settings_keywords)(page));
    words_match(&text, query)
        || (page == SettingsSection::Keyboard && !matching_actions(query, tr).is_empty())
}

/// The settings of page `page` whose names hold every word of `query`, for
/// the nav to say what a search found there.
fn matching_settings(page: SettingsSection, query: &str, tr: &Strings) -> Vec<String> {
    if query.split_whitespace().next().is_none() {
        return Vec::new();
    }
    let names = (tr.settings_keywords)(page);
    let names = names.split('|').next().unwrap_or_default();
    let mut found: Vec<String> = names
        .split(',')
        .map(str::trim)
        .filter(|name| words_match(name, query))
        .map(str::to_owned)
        .collect();
    if page == SettingsSection::Keyboard {
        found.extend(matching_actions(query, tr));
    }
    found
}

/// The Registry logins page under Accounts: what each extension passes into
/// sandboxes, read from this machine's files and variables.
fn registries_section(ws: &Workspace, scheme: &Scheme) -> impl View + use<> {
    let tr = ws.tr();
    let page = Column::new().width(account_width()).gap(GAP / 2.0);
    Scroll::new()
        .label(tr.accounts_logins)
        .restoration_key(tr.accounts_logins)
        .scrollbar(true)
        .size(content_width(), body_height())
        .child(bottom_room(login_sources(page, ws, scheme)))
}

/// One forge on the Accounts page: its title with Add account, then a row
/// per account with its sign-in state and Sign in, Sign out or Remove. The
/// token form of an account and the add form open under their rows, one at
/// a time, so the page stays a list.
fn forge_accounts(
    mut page: Column,
    ws: &Workspace,
    scheme: &Scheme,
    forge: &crate::ext::ForgeInfo,
) -> Column {
    use crate::forges::{AccountState, account_key};
    let inner = ws.borrow();
    let tr = inner.tr;
    let button_width = 112.0;
    let form = inner.settings_ui.account_form.clone();
    let add_key = format!("{}+add", forge.id);
    let toggle_add = ws.clone();
    let add_open = form.as_deref() == Some(add_key.as_str());
    let toggle_key = add_key.clone();
    // A forge with one host is one row: its title over the host and state,
    // with Add account as a small button beside Sign in. A forge with more
    // hosts gets a heading that holds Add account, and a row per host.
    let single = forge.hosts.len() == 1;
    let add_button = {
        let toggle_key = toggle_key.clone();
        let toggle_add = toggle_add.clone();
        let button = Button::new(tr.forge_add_account)
            .appearance(ButtonAppearance::Ghost)
            .height(26.0)
            .text_size(12.5)
            .tooltip(tr.forge_add_account_open)
            .on_click(move || toggle_add.toggle_account_form(&toggle_key));
        let button = button
            .display(tr.forge_add_account_open)
            .width(button_width + 16.0);
        Named((tr.forge_open_add_for)(&forge.title).into(), button)
    };
    // A forge with one host adds others from the line under the list,
    // so its row keeps Sign in as its one action.
    let mut add_button = (!single).then_some(add_button);
    if !single {
        let heading = Row::new()
            .width(account_width())
            .align(Align::End)
            .justify(Justify::SpaceBetween)
            .child(section_title(forge.title.clone()))
            .child(add_button.take().expect("the add button is built once"));
        page = page.child(heading);
    }
    for host in &forge.hosts {
        let state = inner
            .forges
            .account_state(forge, host, &inner.settings.forge_accounts);
        let (status, signed_in) = match &state {
            AccountState::SignedOut => (tr.forge_signed_out.to_owned(), false),
            AccountState::SigningIn => (tr.forge_signing_in.to_owned(), false),
            AccountState::SignedIn(user) if user.is_empty() => {
                (tr.forge_signed_in.to_owned(), true)
            }
            AccountState::SignedIn(user) => ((tr.forge_signed_in_as)(user), true),
            AccountState::FromEnv(var) => ((tr.forge_from_env)(var), false),
            AccountState::Failed(error) => ((tr.forge_failed)(&truncate(error, 80)), false),
        };
        let key = account_key(&forge.id, host);
        let typed = inner.forges.typed.get(&key).cloned().unwrap_or_default();
        // A form with text in it stays open.
        let open = !signed_in && (form.as_deref() == Some(key.as_str()) || !typed.is_empty());
        let status_color = match state {
            AccountState::SignedIn(_) => scheme.lanes[4],
            AccountState::Failed(_) => scheme.lanes[3],
            _ => scheme.muted,
        };
        let mut buttons = Row::new().gap(8.0).align(Align::Center);
        if let Some(add) = add_button.take() {
            buttons = buttons.child(add);
        }
        let (id, account) = (forge.id.clone(), host.clone());
        if signed_in {
            let out = ws.clone();
            let (id, account) = (id.clone(), account.clone());
            buttons = buttons.child(Named(
                (tr.forge_sign_out_for)(host).into(),
                Button::new(tr.forge_sign_out)
                    .appearance(ButtonAppearance::Outline)
                    .width(button_width)
                    .text_size(12.0)
                    .on_click(move || out.forge_sign_out(&id, &account)),
            ));
        } else if !open {
            let toggle = ws.clone();
            let key = key.clone();
            buttons = buttons.child(Named(
                (tr.forge_open_sign_in_for)(host).into(),
                Button::new(tr.forge_sign_in)
                    .display(tr.forge_sign_in_open)
                    .appearance(ButtonAppearance::Outline)
                    .width(button_width)
                    .height(28.0)
                    .text_size(13.0)
                    .on_click(move || toggle.toggle_account_form(&key)),
            ));
        } else {
            // The open form holds Sign in and Cancel; an empty slot keeps
            // the row's other buttons in the same column as the rows below.
            buttons = buttons.child(Row::new().width(button_width).height(28.0));
        }
        // A lone account that was never signed in has nothing to remove.
        if signed_in || forge.hosts.len() > 1 {
            let remove = ws.clone();
            let (remove_id, remove_account) = (id.clone(), account.clone());
            buttons = buttons.child(
                super::machines::remove_icon((tr.forge_remove_account_for)(host))
                    .on_click(move || remove.remove_forge_account(&remove_id, &remove_account)),
            );
        }
        page = page.child(
            Row::new()
                .width(account_width())
                .align(Align::Center)
                .justify(Justify::SpaceBetween)
                .child(
                    list_row(if single {
                        forge.title.clone()
                    } else {
                        host.clone()
                    })
                    .subtitle(if single {
                        format!("{host} · {status}")
                    } else {
                        status.clone()
                    })
                    .subtitle_color(status_color)
                    .dot(status_color)
                    .value(status)
                    .width(account_width() - button_width * 2.0 - 64.0),
                )
                .child(buttons),
        );
        if !open {
            continue;
        }
        let (input, sign_in, token_page) = (ws.clone(), ws.clone(), ws.clone());
        let (typing, focus) = (inner.forges.typing, inner.regions.settings_focus);
        let (input_id, input_host) = (id.clone(), account.clone());
        let (page_id, page_host) = (id.clone(), account.clone());
        let cancel_width = 72.0;
        // The form ends where the rows' buttons end, so the right edge
        // stays one line.
        let field_width = account_width() - FORM_INDENT - button_width * 2.0 - cancel_width - 24.0;
        let cancel = ws.clone();
        let (cancel_id, cancel_host) = (id.clone(), account.clone());
        // The scopes a token needs, read before the token is made.
        if let Some(scopes) = &forge.scopes {
            page = page.child(
                Pad::insets(EdgeInsets {
                    left: FORM_INDENT,
                    ..EdgeInsets::default()
                })
                .child(wrapped_label(scheme, scopes.clone())),
            );
        }
        page = page.child(
            Pad::insets(EdgeInsets {
                left: FORM_INDENT,
                ..EdgeInsets::default()
            })
            .child(
                Row::new()
                    .gap(8.0)
                    .align(Align::Center)
                    // A first sign-in needs a token before anything else, so
                    // the button that opens the token page comes first.
                    .child(Named(
                        (tr.forge_create_token_for)(host).into(),
                        Button::new(tr.forge_create_token)
                            .tooltip(tr.forge_create_token_hint)
                            .appearance(ButtonAppearance::Outline)
                            .width(button_width)
                            .height(super::machines::INPUT_HEIGHT)
                            .text_size(13.0)
                            .on_click(move || {
                                token_page.open_forge_token_page(&page_id, &page_host)
                            }),
                    ))
                    .child({
                        let field = PasswordInput::new(typed.clone())
                            .text_size(super::machines::INPUT_TEXT_SIZE)
                            .hint((tr.forge_token_for)(host))
                            .height(super::machines::INPUT_HEIGHT)
                            .width(field_width)
                            .on_input(move |token| {
                                input.set_forge_token_input(&input_id, &input_host, token);
                            });
                        // Typing can rebuild the form; the field keeps focus.
                        // Its hint is dimmer than a typed token.
                        let view: Box<dyn View> = if typing {
                            Box::new(ThemeProvider::new(
                                scheme.field_theme(),
                                field.with_ref(focus),
                            ))
                        } else {
                            Box::new(ThemeProvider::new(scheme.field_theme(), field))
                        };
                        view
                    })
                    .child(Named(
                        (tr.forge_sign_in_for)(host).into(),
                        // The form's main action, on once there is a token.
                        // Until then its label fades toward the fill, so it
                        // does not read as a live button.
                        ThemeProvider::new(
                            Theme {
                                text_muted: super::theme::mix(scheme.muted, scheme.rail, 140),
                                ..scheme.theme()
                            },
                            Button::new(tr.forge_sign_in)
                                .appearance(ButtonAppearance::Solid)
                                .disabled(typed.is_empty())
                                .width(button_width)
                                .height(super::machines::INPUT_HEIGHT)
                                .text_size(13.0)
                                .on_click(move || sign_in.forge_sign_in(&id, &account)),
                        ),
                    ))
                    .child(
                        Button::new(tr.cancel)
                            .appearance(ButtonAppearance::Outline)
                            .width(cancel_width)
                            .height(super::machines::INPUT_HEIGHT)
                            .text_size(13.0)
                            .on_click(move || cancel.cancel_account_form(&cancel_id, &cancel_host)),
                    ),
            ),
        );
    }
    let (draft, error) = inner
        .forges
        .drafts
        .get(&forge.id)
        .cloned()
        .unwrap_or_default();
    if !(add_open || !draft.is_empty() || error.is_some()) {
        return page;
    }
    if let Some(hint) = &forge.hint {
        page = page.child(wrapped_label(scheme, hint.clone()));
    }
    let field = match &forge.example {
        Some(example) => format!(
            "{} ({})",
            (tr.forge_new_account_for)(&forge.title),
            (tr.accounts_example)(example)
        ),
        None => (tr.forge_new_account_for)(&forge.title),
    };
    let (input, add) = (ws.clone(), ws.clone());
    let (input_id, add_id) = (forge.id.clone(), forge.id.clone());
    page = page.child(
        Row::new()
            .width(account_width())
            .gap(8.0)
            .align(Align::Center)
            .child(
                TextInput::new(draft)
                    .text_size(super::machines::INPUT_TEXT_SIZE)
                    .hint(field)
                    .height(super::machines::INPUT_HEIGHT)
                    .width(account_width() - LIST_BUTTON_WIDTH - 8.0)
                    .on_input(move |typed| input.set_forge_account_draft(&input_id, typed)),
            )
            .child(Named(
                (tr.forge_add_account_for)(&forge.title).into(),
                Button::new(tr.forge_add_account)
                    .appearance(ButtonAppearance::Solid)
                    .width(LIST_BUTTON_WIDTH)
                    .height(super::machines::INPUT_HEIGHT)
                    .text_size(13.0)
                    .on_click(move || add.add_forge_account(&add_id)),
            )),
    );
    if let Some(error) = error {
        page = page.child(wrapped_label(scheme, error));
    }
    page
}

/// Indent of an account's token form under its row: in line with the
/// account's name after its dot.
const FORM_INDENT: f32 = 28.0;

/// Width of the Accounts page beside its scroll bar.
fn account_width() -> f32 {
    content_width() - SCROLLBAR_ROOM
}
/// Width of the button that opens a registry's extension, at the row's end.
const LOGINS_BUTTON: f32 = 96.0;
/// Room the details leave for the button column.
const LOGINS_BUTTON_WIDTH: f32 = 120.0;
/// Width of a registry's details under its row: they end a gap left of the
/// button column.
fn logins_text_width() -> f32 {
    account_width() - FORM_INDENT - LOGINS_BUTTON_WIDTH - GAP
}

/// The extensions whose manifest has a `[logins]` table: where each reads
/// the logins and the hosts it found logins for.
fn login_sources(mut page: Column, ws: &Workspace, scheme: &Scheme) -> Column {
    let inner = ws.borrow();
    let tr = inner.tr;
    let sources: Vec<_> = (inner.ext.extensions())
        .filter_map(|ext| Some((ext, ext.logins.as_ref()?)))
        .collect();
    page = page
        .child(section_title(tr.accounts_logins))
        .child(wrapped_label(scheme, tr.accounts_logins_note));
    if sources.is_empty() {
        return page.child(wrapped_label(scheme, tr.logins_no_sources));
    }
    for (ext, logins) in sources {
        // One row per registry, like an account: a dot that is lit when
        // logins were found and the name, with a line under it that says
        // in words whether logins were found, and for which hosts.
        let (status, found) = if !ext.enabled {
            (tr.logins_off.to_owned(), false)
        } else {
            let hosts = inner.ext.secret_hosts(&ext.name);
            if hosts.is_empty() {
                (tr.logins_none.to_owned(), false)
            } else {
                ((tr.logins_for)(&hosts.join(", ")), true)
            }
        };
        // The row and where it looks stay together, a step apart from the
        // next registry.
        // The extension's page turns it on or off and says what it reads.
        let open = ws.clone();
        let name = ext.name.clone();
        // A little air between the source lines, so a stack of five
        // still reads line by line.
        let mut entry = Column::new().gap(2.0).child(
            Row::new()
                .width(account_width())
                .align(Align::Center)
                .child(
                    found_row(list_row(logins.title.clone()), &logins.title)
                        .value(status.clone())
                        // An empty dot as light as a control's frame, so it still shows.
                        .dot(if found {
                            scheme.lanes[0]
                        } else {
                            scheme.theme().outline
                        })
                        .height(28.0)
                        .width(account_width() - LOGINS_BUTTON),
                )
                .child(
                    Button::new((tr.logins_extension_for)(&ext.name))
                        .display(tr.logins_extension)
                        // Outlined like the other buttons, but small, so six
                        // of them do not outweigh the logins.
                        .appearance(ButtonAppearance::Outline)
                        .text_size(12.5)
                        .width(LOGINS_BUTTON)
                        .height(26.0)
                        .on_click(move || open.show_extension_page(Some(name.clone()))),
                ),
        );
        // What was found wraps, so every host that gets a login shows.
        entry = entry.child(
            Pad::insets(EdgeInsets {
                left: FORM_INDENT,
                ..EdgeInsets::default()
            })
            .child(wrapped_label_at(scheme, status, logins_text_width())),
        );
        // One source per line, each saying whether it is a file or a
        // variable: a list that wraps at a slash reads as two paths.
        for source in &logins.sources {
            let variable = !source.is_empty()
                && source
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
            let line = if source.contains('/') || source.starts_with('~') {
                (tr.logins_file)(source)
            } else if variable {
                (tr.logins_variable)(source)
            } else {
                // Anything else, such as a command, as written.
                (tr.logins_other)(source)
            };
            entry = entry.child(
                Pad::insets(EdgeInsets {
                    left: FORM_INDENT,
                    ..EdgeInsets::default()
                })
                // A size under the status line above, so the sources read
                // as details of it.
                .child(
                    Column::new()
                        .width(logins_text_width())
                        .child(ThemeProvider::new(
                            scheme.muted_theme(),
                            Text::new(line).size(12.5).wrap(WrapMode::Word),
                        )),
                ),
            );
        }
        // A step apart from the next registry, so each block reads as one.
        page = page.child(
            Pad::insets(EdgeInsets {
                bottom: 14.0,
                ..EdgeInsets::default()
            })
            .child(entry),
        );
    }
    page
}

/// The control for one manifest setting, under its title and description.
fn setting_control(
    ws: &Workspace,
    scheme: &Scheme,
    extension: &str,
    spec: &crate::ext::SettingSpec,
    value: SettingValue,
    tr: &'static Strings,
) -> Box<dyn View> {
    let set = {
        let ws = ws.clone();
        let extension = extension.to_owned();
        let key = spec.key.clone();
        move |value: SettingValue, reload: bool| {
            ws.set_extension_setting(&extension, &key, &value, reload);
        }
    };
    let mut column = Column::new().width(ext_row_width()).gap(2.0);
    let described = |column: Column| match &spec.description {
        Some(text) => column.child(wrapped_label(scheme, text.clone())),
        None => column,
    };
    match (&spec.kind, value) {
        (SettingKind::Bool, SettingValue::Bool(on)) => {
            let switch_width = 44.0;
            let title = list_row(spec.title.clone()).width(ext_row_width() - switch_width);
            Box::new(described(
                column.child(
                    Row::new()
                        .width(ext_row_width())
                        .align(Align::Center)
                        .child(title)
                        .child(Named(
                            spec.title.clone().into(),
                            ThemeProvider::new(
                                scheme.switch_theme(),
                                Switch::new(on)
                                    .on_toggle(move |on| set(SettingValue::Bool(on), true)),
                            ),
                        )),
                ),
            ))
        }
        (SettingKind::Choice(choices), SettingValue::String(current)) => {
            column = described(column.child(setting_title(spec.title.clone())));
            let options = choices.iter().map(|c| (c.clone(), c.clone())).collect();
            let row = Row::new().gap(4.0);
            let row = segmented_owned(row, &spec.title, current, options, tr, move |choice| {
                set(SettingValue::String(choice), true);
            });
            Box::new(column.child(row))
        }
        (SettingKind::List | SettingKind::Hosts | SettingKind::Urls, SettingValue::List(items)) => {
            column = described(column.child(setting_title(spec.title.clone())));
            Box::new(list_control(
                column,
                ws,
                scheme,
                extension,
                &spec.key,
                &spec.title,
                &items,
            ))
        }
        (kind, value) => {
            let text = match &value {
                SettingValue::Bool(on) => on.to_string(),
                SettingValue::Integer(n) => n.to_string(),
                SettingValue::String(text) => text.clone(),
                SettingValue::List(items) => items.join(", "),
            };
            let kind = kind.clone();
            column = described(column);
            Box::new(
                column.child(
                    TextInput::new(text)
                        .text_size(super::machines::INPUT_TEXT_SIZE)
                        .label(spec.title.clone())
                        .width(ext_row_width())
                        .on_input(move |typed| {
                            let value = match kind {
                                SettingKind::Integer => match typed.trim().parse() {
                                    Ok(n) => SettingValue::Integer(n),
                                    Err(_) => return,
                                },
                                _ => SettingValue::String(typed.to_owned()),
                            };
                            set(value, false);
                        }),
                ),
            )
        }
    }
}

/// The title of a setting with its control under it: bold and set off
/// above, so it reads as the head of its group and not as one more entry.
fn setting_title(text: String) -> impl View {
    Pad::insets(EdgeInsets {
        top: 8.0,
        bottom: 2.0,
        ..EdgeInsets::default()
    })
    .child(Text::new(text).size(13.0).bold())
}

/// The widest a paragraph of explanation runs, about 90 characters.
const PROSE_WIDTH: f32 = 600.0;

/// The width of the Remove and Add buttons of a list setting.
const LIST_BUTTON_WIDTH: f32 = 96.0;

/// A list setting: one row per entry with Remove, and a field with Add for
/// a new entry. Each change saves and reloads the extension.
fn list_control(
    mut column: Column,
    ws: &Workspace,
    scheme: &Scheme,
    extension: &str,
    key: &str,
    title: &str,
    items: &[String],
) -> Column {
    let tr = ws.tr();
    if items.is_empty() {
        column = column.child(label(scheme, tr.list_empty));
    }
    for (index, item) in items.iter().enumerate() {
        let remove = ws.clone();
        let (extension, key) = (extension.to_owned(), key.to_owned());
        column = column.child(
            Row::new()
                .width(ext_row_width())
                .align(Align::Center)
                .justify(Justify::SpaceBetween)
                .child(list_row(item.clone()).width(ext_row_width() - LIST_BUTTON_WIDTH - 8.0))
                // Quiet, since it repeats down the list, but in a muted
                // danger color, so it reads as a button that removes.
                .child(Named(
                    (tr.list_remove_from)(item, title).into(),
                    ThemeProvider::new(
                        Theme {
                            text: super::theme::mix(scheme.muted, scheme.lanes[3], 150),
                            ..scheme.theme()
                        },
                        Button::new(tr.list_remove)
                            .appearance(ButtonAppearance::Ghost)
                            .width(LIST_BUTTON_WIDTH)
                            .height(28.0)
                            .text_size(12.0)
                            .on_click(move || remove.remove_list_entry(&extension, &key, index)),
                    ),
                )),
        );
    }
    let labels = ((tr.list_new_entry_for)(title), (tr.list_add_to)(title));
    new_entry(column, ws, scheme, (extension, key), labels, tr.add)
}

/// The field and Add button that add an entry to list setting `key` of
/// `extension`, with why the last Add refused one under them. `labels` are
/// the field's accessible name and the button's.
fn new_entry(
    column: Column,
    ws: &Workspace,
    scheme: &Scheme,
    (extension, key): (&str, &str),
    (field, accessible): (String, String),
    text: &'static str,
) -> Column {
    let draft = (ws.borrow().settings_ui.list_drafts)
        .get(&list_key(extension, key))
        .cloned()
        .unwrap_or_default();
    let (input, add) = (ws.clone(), ws.clone());
    let (input_ext, input_key) = (extension.to_owned(), key.to_owned());
    let (add_ext, add_key) = (extension.to_owned(), key.to_owned());
    let column = column.child(
        Row::new()
            .width(ext_row_width())
            .gap(8.0)
            .align(Align::Center)
            .child(
                TextInput::new(draft.text)
                    .text_size(super::machines::INPUT_TEXT_SIZE)
                    .hint(field)
                    .height(super::machines::INPUT_HEIGHT)
                    .width(ext_row_width() - LIST_BUTTON_WIDTH - 8.0)
                    .on_input(move |typed| input.set_list_draft(&input_ext, &input_key, typed)),
            )
            .child(Named(
                accessible.into(),
                // Outlined like the other pages' Add buttons beside a field,
                // and as tall as the field.
                Button::new(text)
                    .appearance(ButtonAppearance::Outline)
                    .width(LIST_BUTTON_WIDTH)
                    .height(super::machines::INPUT_HEIGHT)
                    .text_size(12.0)
                    .on_click(move || add.add_list_entry(&add_ext, &add_key)),
            )),
    );
    match draft.error {
        Some(error) => column.child(wrapped_label(scheme, error)),
        None => column,
    }
}

/// [`segmented`] for owned values such as the choices of a setting.
fn segmented_owned(
    mut row: Row,
    title: &str,
    current: String,
    options: Vec<(String, String)>,
    tr: &'static Strings,
    pick: impl Fn(String) + Clone + 'static,
) -> Row {
    for (value, text) in options {
        let pick = pick.clone();
        let appearance = if value == current {
            ButtonAppearance::Solid
        } else {
            ButtonAppearance::Outline
        };
        let chosen = value == current;
        row = row.child(Named(
            (tr.guard_mode_for)(title, &text).into(),
            segment(
                chosen,
                &text.clone(),
                Button::new(text)
                    .appearance(appearance)
                    .text_size(12.0)
                    .on_click(move || pick(value.clone())),
            ),
        ));
    }
    row
}

fn agent_section(ws: &Workspace, scheme: &Scheme) -> impl View + use<> {
    let mut rows = ws.borrow().settings_ui.agents.rows();
    // What needs a click comes first, then what is set up, what is off and
    // what is not installed, so the list reads as a to-do list.
    rows.sort_by_key(|row| match row.status {
        Status::Outdated | Status::Failed(_) => 0,
        Status::On => 1,
        Status::Off => 2,
        Status::Checking | Status::NotFound => 3,
    });
    let tr = ws.tr();
    let several_envs = rows.iter().any(|row| !row.env.is_local());
    let button_width = 156.0;
    let row_width = content_width() - SCROLLBAR_ROOM;
    let outdated: Vec<_> = rows
        .iter()
        .filter(|row| matches!(row.status, Status::Outdated) && !row.busy)
        .map(|row| (row.agent, row.env.clone()))
        .collect();
    let mut list = Column::new().width(row_width);
    // The list's heading, with Check again beside it.
    let heading = Row::new()
        .width(row_width)
        .justify(Justify::SpaceBetween)
        .align(Align::End)
        .child(section_title(tr.agents_installed));
    let refresh = ws.clone();
    // One spinner beside Check again while any check runs, instead of
    // one per row.
    let checking = rows
        .iter()
        .any(|row| matches!(row.status, Status::Checking));
    let mut actions = Row::new().gap(8.0).align(Align::Center);
    if checking {
        actions = actions.child(LoadingSpinner::new().size(16.0).name(tr.checking));
    }
    let actions = actions.child(
        Button::new(tr.check_agents)
            .appearance(ButtonAppearance::Outline)
            .text_size(13.0)
            .width(super::text_button_width(tr.check_agents, 13.0))
            .height(28.0)
            .disabled(checking)
            .on_click(move || refresh.refresh_agents()),
    );
    // Several outdated integrations get one banner with Update all, above
    // the rows, so the fix is offered once where the problem is named.
    let banner = (outdated.len() > 1).then(|| {
        let update = ws.clone();
        let count = outdated.len();
        schnellui::widgets::Panel::new()
            .width(row_width)
            .insets(EdgeInsets {
                left: 12.0,
                right: 10.0,
                top: 8.0,
                bottom: 8.0,
            })
            .surface(scheme.warning_surface())
            .outline(scheme.lanes[1])
            .rail(scheme.lanes[1])
            .child(
                Row::new()
                    .width(row_width - 24.0)
                    .justify(Justify::SpaceBetween)
                    .align(Align::Center)
                    .child(
                        Column::new()
                            .width(row_width - 24.0 - button_width - GAP)
                            .child(ThemeProvider::new(
                                Theme {
                                    text: scheme.ink,
                                    ..scheme.theme()
                                },
                                Text::new((tr.agents_outdated)(count))
                                    .size(12.5)
                                    .wrap(WrapMode::Word),
                            )),
                    )
                    .child(
                        Button::new((tr.agents_update_all)(count))
                            .appearance(ButtonAppearance::Solid)
                            .text_size(13.0)
                            .width(button_width)
                            .height(28.0)
                            .on_click(move || {
                                for (agent, env) in &outdated {
                                    update.set_agent_integration(*agent, env.clone(), true);
                                }
                            }),
                    ),
            )
    });
    list = list.child(heading.child(actions));
    // What the integrations and their hooks do sits under the heading, so
    // "hooks outdated" in the rows reads with its meaning.
    list = list.child(
        Pad::insets(EdgeInsets {
            top: 4.0,
            bottom: 6.0,
            ..EdgeInsets::default()
        })
        .child(
            // Narrower than the list, so a line stays short enough to read.
            Column::new()
                .width(row_width.min(PROSE_WIDTH))
                .child(ThemeProvider::new(
                    scheme.muted_theme(),
                    Text::new(
                        [
                            tr.agents_note[0],
                            tr.agents_note[1],
                            tr.agents_skill[0],
                            tr.agents_skill[1],
                            tr.hooks_run,
                        ]
                        .join(" "),
                    )
                    .size(12.5)
                    .wrap(WrapMode::Word),
                )),
        ),
    );
    // With the banner there, outdated rows keep their own Update but leave
    // the warning color to their dots.
    let bulk_update = banner.is_some();
    if let Some(banner) = banner {
        list = list.child(
            Pad::insets(EdgeInsets {
                bottom: 6.0,
                ..EdgeInsets::default()
            })
            .child(banner),
        );
    }
    for row in rows {
        let name = if several_envs {
            (tr.agent_on)(row.agent.name(), row.env.label())
        } else {
            row.agent.name().to_owned()
        };
        let package = crate::notifications::npm_packages(row.agent).first();
        let mut status = match (&row.status, package) {
            _ if row.busy => tr.working.to_owned(),
            // Say how to get an agent that is missing.
            (Status::NotFound, Some(package)) => {
                (tr.agent_install_with)(&format!("npm i -g {package}"))
            }
            (status, _) => tr.agent_status(status),
        };
        // The note says what to do after an install, not before an update.
        if let Some(note) = row.note.filter(|_| !matches!(row.status, Status::Outdated)) {
            status.push_str(". ");
            status.push_str(tr.agent_note(note));
        }
        let (action, on) = match row.status {
            Status::Off => (tr.agent_turn_on, true),
            Status::Outdated => (tr.update, true),
            Status::On => (tr.remove, false),
            Status::Failed(_) => (tr.retry, true),
            Status::Checking | Status::NotFound => (tr.install, true),
        };
        // Green when set up, brass when outdated, rose when it failed,
        // grey when off and a faint grey when not installed at all, so
        // every name sits in the same column.
        let status_color = match row.status {
            Status::On => scheme.lanes[4],
            Status::Outdated => scheme.lanes[1],
            Status::Failed(_) => scheme.lanes[3],
            Status::Off | Status::Checking => scheme.muted,
            Status::NotFound => super::theme::mix(scheme.muted, scheme.raised, 40),
        };
        let change = ws.clone();
        let (agent, env) = (row.agent, row.env.clone());
        // A spinner while the check runs; an agent that is not installed
        // has nothing to set up.
        let trailing: Box<dyn View> = match row.status {
            // The heading's spinner says the check runs; the row keeps
            // the room its button will take.
            Status::Checking => Box::new(Fill::new(button_width, 1.0, Color::TRANSPARENT)),
            Status::NotFound => match package {
                Some(package) => {
                    let copy = ws.clone();
                    let command = format!("npm i -g {package}");
                    Box::new(Named(
                        (tr.copy_install_for)(&name).into(),
                        Button::new(tr.copy_command)
                            .tooltip(tr.copy_command_hint)
                            .appearance(ButtonAppearance::Outline)
                            .text_size(13.0)
                            .width(button_width)
                            .height(28.0)
                            .on_click(move || copy.copy_command(command.clone())),
                    ))
                }
                None => Box::new(Fill::new(button_width, 1.0, Color::rgba(0, 0, 0, 0))),
            },
            _ => Box::new(
                Button::new((tr.agent_button)(action, &name))
                    .display(action)
                    .appearance(ButtonAppearance::Outline)
                    .text_size(13.0)
                    .width(button_width)
                    .height(28.0)
                    .disabled(row.busy)
                    .on_click(move || change.set_agent_integration(agent, env.clone(), on)),
            ),
        };
        list = list.child(
            Row::new()
                .width(row_width)
                .align(Align::Center)
                .child(
                    list_row(name.clone())
                        .subtitle(truncate(&status, 80))
                        .dot(status_color)
                        // The dot tells off from not installed; the words
                        // stay readable in the muted color.
                        // Under the banner, outdated rows leave the brass
                        // to their dots, so it does not repeat five times.
                        .subtitle_color(
                            if matches!(row.status, Status::NotFound)
                                || (bulk_update && matches!(row.status, Status::Outdated))
                            {
                                scheme.muted
                            } else {
                                status_color
                            },
                        )
                        .width(row_width - button_width),
                )
                .child(trailing),
        );
    }
    let (current, open, focus) = {
        let inner = ws.borrow();
        (
            inner.settings.agent_control,
            inner.settings_ui.picker == Some(Picker::AgentControl),
            inner.regions.settings_focus,
        )
    };
    let toggle = ws.clone();
    let mut control = Dropdown::new(tr.agent_control)
        .min_width(SELECT_WIDTH)
        .text_size(super::machines::INPUT_TEXT_SIZE)
        .open(open)
        .on_toggle(move || toggle.toggle_picker(Picker::AgentControl));
    for (mode, title) in [
        (AgentControl::On, tr.agent_control_on),
        (AgentControl::Local, tr.agent_control_local),
        (AgentControl::Host, tr.agent_control_host),
        (AgentControl::Off, tr.agent_control_off),
    ] {
        let pick = ws.clone();
        control = control.option(
            DropdownOption::new(title)
                .selected(current == mode)
                .on_select(move || pick.pick_agent_control(mode)),
        );
    }
    let control: Box<dyn View> = if open {
        Box::new(ThemeProvider::new(
            scheme.control_theme(),
            control.with_ref(focus),
        ))
    } else {
        Box::new(ThemeProvider::new(scheme.control_theme(), control))
    };
    // The whole page scrolls as one, so the list never clips.
    let mut page = Column::new()
        .width(row_width)
        .gap(GAP / 2.0)
        .child(list)
        .child(section_title(tr.default_agent))
        .child(label(scheme, tr.default_agent_note))
        .child(default_agent_dropdown(ws, scheme))
        .child(section_title(tr.agent_control))
        .child(label(scheme, tr.agent_control_note))
        .child(control)
        .child(section_title(tr.cpu_heading))
        .child(label(scheme, tr.cpu_note));
    for (place, title) in [
        (crate::cpu::Place::Local, tr.cpu_local),
        (crate::cpu::Place::Remote, tr.cpu_remote),
    ] {
        page = page
            .child(label(scheme, title))
            .child(cpu_dropdown(ws, scheme, place, title));
    }
    // Room below the dropdowns, so the page scrolls them clear of its lower
    // edge. An open one gets room for its whole list, which opening it
    // scrolls into view.
    let cpu_open = matches!(ws.borrow().settings_ui.picker, Some(Picker::Cpu(_)));
    let room = if cpu_open {
        crate::state::CpuMode::ALL.len() as f32 * DROPDOWN_ROW + GAP * 2.0
    } else {
        GAP * 2.0
    };
    let page = page.child(Fill::new(1.0, room, Color::TRANSPARENT));
    Scroll::new()
        .label(tr.agent_list)
        .restoration_key(tr.agent_list)
        .scrollbar(true)
        .size(content_width(), body_height())
        .child(bottom_room(page))
}

/// The agents New agent tab can start: those installed on this machine,
/// every agent while no check has found any, and the chosen one.
pub(crate) fn default_agent_choices(ws: &Workspace) -> (Option<Agent>, Vec<Agent>) {
    let inner = ws.borrow();
    let current = (inner.settings.default_agent.as_deref()).and_then(Agent::find);
    let rows = inner.settings_ui.agents.rows();
    let mut agents: Vec<Agent> = Agent::all()
        .into_iter()
        .filter(|agent| {
            rows.iter().any(|row| {
                row.agent == *agent
                    && row.env.is_local()
                    && !matches!(row.status, Status::NotFound | Status::Checking)
            })
        })
        .collect();
    if agents.is_empty() {
        agents = Agent::all();
    }
    if let Some(agent) = current
        && !agents.contains(&agent)
    {
        agents.push(agent);
    }
    (current, agents)
}

/// The dropdown of the agent New agent tab starts.
fn default_agent_dropdown(ws: &Workspace, scheme: &Scheme) -> Box<dyn View> {
    let (current, agents) = default_agent_choices(ws);
    let (open, focus, tr) = {
        let inner = ws.borrow();
        (
            inner.settings_ui.picker == Some(Picker::DefaultAgent),
            inner.regions.settings_focus,
            inner.tr,
        )
    };
    let toggle = ws.clone();
    let pick = ws.clone();
    let mut dropdown = Dropdown::new(tr.default_agent)
        .min_width(SELECT_WIDTH)
        .text_size(super::machines::INPUT_TEXT_SIZE)
        .open(open)
        .on_toggle(move || toggle.toggle_picker(Picker::DefaultAgent))
        .option(
            DropdownOption::new(tr.default_agent_first)
                .selected(current.is_none())
                .on_select(move || pick.pick_default_agent(None)),
        );
    for agent in agents {
        let pick = ws.clone();
        dropdown = dropdown.option(
            DropdownOption::new(agent.name())
                .selected(current == Some(agent))
                .on_select(move || pick.pick_default_agent(Some(agent))),
        );
    }
    if open {
        Box::new(ThemeProvider::new(
            scheme.control_theme(),
            dropdown.with_ref(focus),
        ))
    } else {
        Box::new(ThemeProvider::new(scheme.control_theme(), dropdown))
    }
}

/// The dropdown of the CPU mode at `place`, each mode named with what it
/// does under the user's settings.
fn cpu_dropdown(
    ws: &Workspace,
    scheme: &Scheme,
    place: crate::cpu::Place,
    name: &'static str,
) -> Box<dyn View> {
    let (modes, open, focus, tr) = {
        let inner = ws.borrow();
        (
            inner.settings.cpu_mode,
            inner.settings_ui.picker == Some(Picker::Cpu(place)),
            inner.regions.settings_focus,
            inner.tr,
        )
    };
    let toggle = ws.clone();
    let mut dropdown = Dropdown::new(name)
        .min_width(SELECT_WIDTH)
        .text_size(super::machines::INPUT_TEXT_SIZE)
        .open(open)
        .on_toggle(move || toggle.toggle_picker(Picker::Cpu(place)));
    for mode in crate::state::CpuMode::ALL {
        let pick = ws.clone();
        dropdown = dropdown.option(
            DropdownOption::new(modes.describe(mode, tr))
                .selected(modes.of(place) == mode)
                .on_select(move || pick.pick_cpu_mode(place, mode)),
        );
    }
    if open {
        Box::new(ThemeProvider::new(
            scheme.control_theme(),
            dropdown.with_ref(focus),
        ))
    } else {
        Box::new(ThemeProvider::new(scheme.control_theme(), dropdown))
    }
}

fn language_section(ws: &Workspace, scheme: &Scheme) -> impl View + use<> {
    let (current, open, focus, tr) = {
        let inner = ws.borrow();
        (
            inner.settings.language.clone(),
            inner.settings_ui.picker == Some(Picker::Language),
            inner.regions.settings_focus,
            inner.tr,
        )
    };
    // Three choices need no search: a dropdown like every other choice on
    // the settings pages, under the same kind of heading.
    let toggle = ws.clone();
    let system = ws.clone();
    let mut picker = Dropdown::new(tr.language)
        .min_width(SELECT_WIDTH)
        .text_size(super::machines::INPUT_TEXT_SIZE)
        .open(open)
        .on_toggle(move || toggle.toggle_picker(Picker::Language))
        .option(
            DropdownOption::new((tr.system_language)(Lang::system().name()))
                .selected(current == SYSTEM_LANGUAGE)
                .on_select(move || system.pick_language(SYSTEM_LANGUAGE)),
        );
    for lang in Lang::ALL {
        let pick = ws.clone();
        picker = picker.option(
            DropdownOption::new(lang.name())
                .selected(current == lang.id())
                .on_select(move || pick.pick_language(lang.id())),
        );
    }
    Column::new()
        .width(content_width())
        .gap(GAP)
        .child(section_title(tr.language))
        .child(if open {
            Box::new(ThemeProvider::new(
                scheme.control_theme(),
                picker.with_ref(focus),
            )) as Box<dyn View>
        } else {
            Box::new(ThemeProvider::new(scheme.control_theme(), picker))
        })
}

/// The key of the app's MIT license on the Licenses page.
pub const LICENSE_MIT_KEY: &str = "scriptschnell-term MIT";
/// The key of the app's Apache 2.0 license on the Licenses page.
pub const LICENSE_APACHE_KEY: &str = "scriptschnell-term Apache-2.0";
/// How far a nested change sits right of its parent on the Changelog page.
const CHANGELOG_INDENT: f32 = 18.0;
/// Width of the bullet in front of a change.
const BULLET_WIDTH: f32 = 14.0;

/// Text in ink that breaks into lines at `width`.
fn prose<T: Into<std::borrow::Cow<'static, str>>>(text: T, width: f32) -> impl View + use<T> {
    Column::new()
        .width(width)
        .child(Text::new(text).size(13.0).wrap(WrapMode::Word))
}

/// The Changelog page: the running version, then each release from the
/// changelog built into the app, newest first, with its groups of changes.
fn changelog_section(ws: &Workspace, scheme: &Scheme) -> impl View + use<> {
    let tr = ws.tr();
    let width = content_width() - SCROLLBAR_ROOM;
    let mut page = Column::new().width(width).gap(GAP / 2.0).child(label(
        scheme,
        (tr.changelog_running)(env!("CARGO_PKG_VERSION")),
    ));
    for block in crate::about::changelog(crate::about::CHANGELOG) {
        page = match block {
            crate::about::Block::Release { version, date } => {
                let version = if version.eq_ignore_ascii_case("unreleased") {
                    tr.changelog_unreleased.to_owned()
                } else {
                    version
                };
                let mut heading = Row::new()
                    .gap(PAD)
                    .align(Align::End)
                    .child(section_title(version));
                if !date.is_empty() {
                    heading = heading.child(ThemeProvider::new(
                        scheme.muted_theme(),
                        Text::new(date).size(12.5),
                    ));
                }
                page.child(heading)
            }
            crate::about::Block::Group(group) => page.child(
                Pad::insets(EdgeInsets {
                    top: GAP / 2.0,
                    ..EdgeInsets::default()
                })
                .child(ThemeProvider::new(
                    scheme.muted_theme(),
                    Text::new(group).size(12.5).bold(),
                )),
            ),
            crate::about::Block::Item { depth, text } => {
                let indent = depth as f32 * CHANGELOG_INDENT;
                page.child(
                    Row::new()
                        .align(Align::Start)
                        .child(
                            Pad::insets(EdgeInsets {
                                left: indent,
                                ..EdgeInsets::default()
                            })
                            .child(
                                Column::new().width(BULLET_WIDTH).child(ThemeProvider::new(
                                    scheme.muted_theme(),
                                    Text::new("•").size(13.0),
                                )),
                            ),
                        )
                        .child(prose(text, width - indent - BULLET_WIDTH)),
                )
            }
            crate::about::Block::Paragraph(text) => page.child(prose(text, width)),
        };
    }
    Scroll::new()
        .label(tr.section_changelog)
        .restoration_key(tr.section_changelog)
        .scrollbar(true)
        .size(content_width(), body_height())
        .child(bottom_room(page))
}

/// A license's text indented under its row, its paragraphs joined into lines
/// that wrap at `width`, so text written for 80 columns fills the page.
fn license_text(page: Column, scheme: &Scheme, text: &str, width: f32) -> Column {
    let mut body = Column::new().width(width).gap(GAP / 2.0);
    for paragraph in text.split("\n\n") {
        let paragraph = paragraph.split_whitespace().collect::<Vec<_>>().join(" ");
        if !paragraph.is_empty() {
            body = body.child(label(scheme, paragraph));
        }
    }
    page.child(
        Pad::insets(EdgeInsets {
            left: FORM_INDENT,
            top: GAP / 2.0,
            bottom: GAP,
            ..EdgeInsets::default()
        })
        .child(body),
    )
}

/// A row on the Licenses page that shows its license text under it when
/// clicked, marked as selected while it does.
fn license_row(
    ws: &Workspace,
    scheme: &Scheme,
    title: &'static str,
    key: String,
    open: bool,
    width: f32,
) -> ListRow {
    let toggle = ws.clone();
    list_row(title)
        .width(width)
        .selected(open)
        .height(ROW_HEIGHT)
        .selected_background(scheme.selected())
        .on_click(move || toggle.toggle_license(&key))
}

/// The Licenses page: the app's own licenses, then every third-party crate
/// built into it with its license. Clicking a row shows its license text
/// under it, one at a time.
fn licenses_section(ws: &Workspace, scheme: &Scheme) -> impl View + use<> {
    let (tr, open) = {
        let inner = ws.borrow();
        (inner.tr, inner.settings_ui.license.clone())
    };
    let width = content_width() - SCROLLBAR_ROOM;
    let text_width = width - FORM_INDENT;
    let open = open.as_deref();
    let mut page = Column::new()
        .width(width)
        .gap(GAP / 2.0)
        .child(section_title("scriptschnell-term"))
        .child(label(scheme, tr.licenses_app_hint));
    for (key, title, text) in [
        (LICENSE_MIT_KEY, tr.license_mit, crate::about::LICENSE_MIT),
        (
            LICENSE_APACHE_KEY,
            tr.license_apache,
            crate::about::LICENSE_APACHE,
        ),
    ] {
        let shown = open == Some(key);
        page = page.child(license_row(ws, scheme, title, key.to_owned(), shown, width));
        if shown {
            page = license_text(page, scheme, text, text_width);
        }
    }
    page = page.child(section_title(tr.licenses_third_party));
    let crates = crate::about::CRATES;
    if crates.is_empty() {
        page = page.child(label(scheme, tr.licenses_none));
    } else {
        page = page.child(label(scheme, (tr.licenses_third_party_hint)(crates.len())));
    }
    for krate in crates {
        let key = format!("{} {}", krate.name, krate.version);
        let shown = open == Some(key.as_str());
        let license = if krate.license.is_empty() {
            tr.license_in_file
        } else {
            krate.license
        };
        let mut row = license_row(ws, scheme, krate.name, key, shown, width)
            .trailing(format!("{} · {license}", krate.version));
        if !krate.repository.is_empty() {
            row = row.tooltip(krate.repository);
        }
        page = page.child(row);
        if !shown {
            continue;
        }
        let mut texts = krate.license_texts().peekable();
        if texts.peek().is_none() {
            page = page.child(
                Pad::insets(EdgeInsets {
                    left: FORM_INDENT,
                    bottom: GAP,
                    ..EdgeInsets::default()
                })
                .child(wrapped_label_at(
                    scheme,
                    (tr.license_no_file)(license),
                    text_width,
                )),
            );
        }
        for text in texts {
            page = license_text(page, scheme, text, text_width);
        }
    }
    Scroll::new()
        .label(tr.section_licenses)
        .restoration_key(tr.section_licenses)
        .scrollbar(true)
        .size(content_width(), body_height())
        .child(bottom_room(page))
}

/// A scrolled page with room under its last item, so it does not touch the
/// dialog's edge.
fn bottom_room(page: impl View + 'static) -> Pad {
    Pad::insets(EdgeInsets {
        bottom: 16.0,
        ..EdgeInsets::default()
    })
    .child(page)
}

#[cfg(test)]
mod tests {
    use crate::state::{SettingsStore, StateStore};
    use schnellui::SubtreeReplacement;
    use schnellui::a11y::Role;

    use super::*;

    #[test]
    fn the_settings_keep_their_height_and_fill_a_small_window() {
        // A tall window gives the overlay its height, less a margin above
        // and below, up to a limit.
        let (top, body) = body_layout(900.0);
        assert_eq!(top, TOP);
        assert!(body > BODY_HEIGHT);
        assert!(top + PAD * 4.0 + HEADER_HEIGHT + body + TOP <= 900.0);
        assert_eq!(body_layout(4000.0).1, BODY_MAX_HEIGHT);
        assert_eq!(
            body_layout(TOP + PAD * 4.0 + HEADER_HEIGHT + BODY_HEIGHT).1,
            BODY_HEIGHT
        );
        let (top, body) = body_layout(500.0);
        assert_eq!(top, PAD);
        assert!(body < BODY_HEIGHT);
        // The whole overlay fits the window, with a margin under it.
        assert!(top + PAD * 3.0 + HEADER_HEIGHT + body + PAD <= 500.0);
        assert_eq!(body_layout(100.0).1, BODY_MIN_HEIGHT);
    }
    use crate::i18n::EN;

    const VIEWPORT: Size = Size {
        width: 1000.0,
        height: 600.0,
    };

    /// Rebuilds the overlay the way the window loop does.
    fn rebuild(ws: &Workspace, app: &mut schnellui::App) {
        assert!(ws.take_dirty().settings);
        let regions = ws.regions();
        app.apply_subtree_replacement(
            SubtreeReplacement::new(regions.settings, settings(ws, VIEWPORT), "settings")
                .with_focus_after(regions.settings_focus),
        )
        .unwrap();
        app.frame();
    }

    #[test]
    fn the_mouse_steps_back_and_forward_through_the_pages() {
        use schnellui::Navigation::{Back, Forward};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &root.join("cfg/extensions"),
        );
        let mut app = crate::build_app(&ws, VIEWPORT);
        let shown = |ws: &Workspace| ws.borrow().settings_ui.place();
        let page = |section, extension| SettingsPlace { section, extension };
        let list = page(SettingsSection::Extensions, None);
        let builtin = page(SettingsSection::Extensions, Some(None));
        // A closed overlay leaves the buttons alone.
        assert!(!app.dispatch_navigation(Back));

        ws.show_settings_section(SettingsSection::Font);
        ws.open_settings();
        ws.show_settings_section(SettingsSection::Keyboard);
        ws.show_settings_section(SettingsSection::Extensions);
        ws.show_extension_page(None);
        assert!(app.dispatch_navigation(Back));
        assert_eq!(shown(&ws), list);
        assert!(app.dispatch_navigation(Back));
        assert_eq!(shown(&ws), page(SettingsSection::Keyboard, None));
        assert!(app.dispatch_navigation(Back));
        assert_eq!(shown(&ws), page(SettingsSection::Font, None));
        // The page shown before the overlay opened is not in it.
        assert!(!app.dispatch_navigation(Back));
        assert!(app.dispatch_navigation(Forward));
        assert!(app.dispatch_navigation(Forward));
        assert!(app.dispatch_navigation(Forward));
        assert_eq!(shown(&ws), builtin);
        assert!(!app.dispatch_navigation(Forward));

        // Going to a page drops what forward would return to.
        assert!(app.dispatch_navigation(Back));
        ws.show_settings_section(SettingsSection::Language);
        assert!(!app.dispatch_navigation(Forward));
        assert!(app.dispatch_navigation(Back));
        assert_eq!(shown(&ws), list);

        // The nav's find field shows pages without adding them.
        ws.set_settings_query(EN.section_language);
        assert_eq!(shown(&ws).section, SettingsSection::Language);
        assert!(app.dispatch_navigation(Back));
        assert_eq!(shown(&ws), page(SettingsSection::Keyboard, None));
        ws.set_settings_query("");

        // Reopened on an extension's page, back goes to the list.
        ws.show_extension_page(None);
        ws.close_settings();
        ws.open_settings();
        assert!(app.dispatch_navigation(Back));
        assert_eq!(shown(&ws), list);
        assert!(!app.dispatch_navigation(Back));
        assert!(app.dispatch_navigation(Forward));
        assert_eq!(shown(&ws), builtin);
    }

    #[test]
    fn typing_a_host_filters_the_list_and_keeps_the_field_focused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &root.join("cfg/extensions"),
        );
        ws.borrow_mut().settings.sandbox.allowed_hosts = vec![
            "github.com".into(),
            "*.github.com".into(),
            "crates.io".into(),
        ];
        ws.open_settings();
        ws.show_settings_section(SettingsSection::SandboxHosts);
        let mut app = crate::build_app(&ws, VIEWPORT);
        let _ = ws.take_dirty();
        let shown = |app: &schnellui::App, host: &str| {
            app.find_widget(Role::Button, Some(&(EN.remove_named)(host)))
                .is_some()
        };
        // The common groups start closed.
        assert!(!shown(&app, "crates.io"));
        ws.set_machine_text(crate::machines::MachineText::SandboxHost, "git");
        rebuild(&ws, &mut app);
        assert!(!shown(&app, "crates.io"));
        assert!(shown(&app, "*.github.com"));
        let field = app
            .find_widget(Role::TextInput, Some(EN.sandbox_host))
            .unwrap();
        assert_eq!(schnellui::a11y::focused(app.scene()), Some(field));
        ws.set_machine_text(crate::machines::MachineText::SandboxHost, "zz");
        rebuild(&ws, &mut app);
        assert!(
            app.find_widget(Role::Label, Some(&(EN.sandbox_host_none)("zz")))
                .is_some()
        );
        assert!(
            app.find_widget(Role::Label, Some("Allowed hosts (0 of 3 match)"))
                .is_some()
        );
    }

    #[test]
    fn hosts_show_in_their_groups_and_a_removed_one_comes_back() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &root.join("cfg/extensions"),
        );
        ws.borrow_mut().settings.sandbox.allowed_hosts =
            vec!["crates.io".into(), "example.org".into()];
        ws.open_settings();
        ws.show_settings_section(SettingsSection::SandboxHosts);
        let app = crate::build_app(&ws, VIEWPORT);
        let button =
            |app: &schnellui::App, text: &str| app.find_widget(Role::Button, Some(text)).is_some();
        let remove = |host: &str| (EN.remove_named)(host);
        // Each heading carries its group's count. The common groups start
        // closed and the user's own starts open.
        let rust = format!("{} (1)", (EN.host_group)("rust"));
        assert!(button(&app, &(EN.show_named)(&rust)));
        assert!(!button(&app, &remove("crates.io")));
        let added = format!("{} (1)", EN.hosts_added_by_you);
        assert!(button(&app, &(EN.hide_named)(&added)));
        assert!(button(&app, &remove("example.org")));
        let mut app = app;
        app.dispatch_click(
            app.find_widget(Role::Button, Some(&(EN.show_named)(&rust)))
                .unwrap(),
        );
        let app = crate::build_app(&ws, VIEWPORT);
        assert!(button(&app, &remove("crates.io")));
        assert!(button(&app, &(EN.hide_named)(&rust)));
        ws.remove_sandbox_host("example.org");
        let mut app = crate::build_app(&ws, VIEWPORT);
        assert!(!button(&app, &(EN.hide_named)(&added)));
        let undo = app
            .find_widget(Role::Button, Some(&(EN.undo_remove_named)("example.org")))
            .unwrap();
        app.dispatch_click(undo);
        assert!(
            ws.borrow()
                .settings
                .sandbox
                .allowed_hosts
                .contains(&"example.org".to_owned())
        );
    }

    fn visible_options(app: &schnellui::App) -> Vec<String> {
        let scene = app.scene();
        scene
            .preorder()
            .filter(|id| scene.is_effectively_visible(*id))
            .filter_map(|id| {
                let a11y = scene.a11y(id)?;
                (Role::from_u16(a11y.role) == Role::ListBoxOption).then(|| a11y.name.clone())?
            })
            .collect()
    }

    #[test]
    fn the_font_dropdown_filters_as_you_type_and_picks_a_family() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &root.join("cfg/extensions"),
        );
        ws.open_settings();
        ws.borrow_mut().settings_ui.fonts = Some(
            [
                "DejaVu Sans Mono",
                "Fira Code",
                "JetBrains Mono",
                "Fira Mono",
            ]
            .map(str::to_owned)
            .to_vec(),
        );
        let mut app = crate::build_app(&ws, VIEWPORT);
        ws.take_dirty();
        assert!(visible_options(&app).is_empty(), "starts collapsed");

        let field = app
            .find_widget(Role::ComboBox, Some(EN.font_picker))
            .unwrap();
        assert_eq!(
            app.scene().a11y(field).unwrap().value.as_deref(),
            Some(EN.bundled_font)
        );
        app.dispatch_click(field);
        rebuild(&ws, &mut app);
        let field = app
            .find_widget(Role::ComboBox, Some(EN.font_picker))
            .unwrap();
        assert_eq!(
            app.focused_widget(),
            Some(field),
            "typing goes to the search"
        );
        assert_eq!(visible_options(&app).len(), 5);

        // Typing filters in place: no overlay rebuild.
        assert!(app.set_text_value(field, "fira"));
        app.frame();
        assert_eq!(visible_options(&app), ["Fira Code", "Fira Mono"]);
        assert!(!ws.take_dirty().any());
        assert_eq!(ws.borrow().settings_ui.picker_query, "fira");

        let mono = app
            .find_widget(Role::ListBoxOption, Some("Fira Mono"))
            .unwrap();
        app.dispatch_click(mono);
        assert_eq!(
            ws.borrow().settings.font_family.as_deref(),
            Some("Fira Mono")
        );
        assert_eq!(ws.borrow().settings_ui.picker, None);
    }

    #[test]
    fn the_shells_page_picks_the_default_and_lists_shells() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &root.join("cfg/extensions"),
        );
        let shell = |id, name, program: &str| cmdterm_env::ShellOption {
            id,
            name,
            argv: vec![program.into()],
        };
        ws.borrow_mut().shells = vec![
            shell("git-bash", "Git Bash", "bash.exe"),
            shell("pwsh", "PowerShell", "pwsh.exe"),
        ];
        ws.open_settings();
        ws.show_settings_section(SettingsSection::Shells);
        let mut app = crate::build_app(&ws, VIEWPORT);
        ws.take_dirty();

        let field = app
            .find_widget(Role::ComboBox, Some(EN.default_shell))
            .unwrap();
        app.dispatch_click(field);
        rebuild(&ws, &mut app);
        assert_eq!(
            visible_options(&app),
            [EN.default_shell_system, "Git Bash", "PowerShell"]
        );
        let bash = app
            .find_widget(Role::ListBoxOption, Some("Git Bash"))
            .unwrap();
        app.dispatch_click(bash);
        assert_eq!(
            ws.borrow().settings.shell,
            Some(ShellChoice::Name("git-bash".into()))
        );
        rebuild(&ws, &mut app);

        let switch = app.find_widget(Role::Switch, Some("PowerShell")).unwrap();
        app.dispatch_click(switch);
        assert_eq!(
            ws.borrow().settings.shells,
            [ShellChoice::Name("pwsh".into())]
        );
        let saved = std::fs::read_to_string(root.join("cfg/settings.json")).unwrap();
        assert!(saved.contains("\"shell\": \"git-bash\""), "{saved}");
    }

    #[test]
    fn the_keymap_dropdown_picks_a_keymap() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &root.join("cfg/extensions"),
        );
        ws.open_settings();
        ws.show_settings_section(SettingsSection::Keyboard);
        let mut app = crate::build_app(&ws, VIEWPORT);
        ws.take_dirty();
        assert!(visible_options(&app).is_empty(), "starts collapsed");

        let picker = app
            .find_widget(Role::ComboBox, Some(EN.keymap_picker))
            .unwrap();
        app.dispatch_click(picker);
        rebuild(&ws, &mut app);
        assert_eq!(visible_options(&app).len(), crate::keymap::PRESETS.len());

        let tmux = app.find_widget(Role::ListBoxOption, Some("tmux")).unwrap();
        app.dispatch_click(tmux);
        assert_eq!(ws.borrow().settings.keymap, "tmux");
        assert_eq!(ws.borrow().keymap.preset.id, "tmux");
        assert_eq!(ws.borrow().settings_ui.picker, None);
        let saved = SettingsStore::new(root.join("cfg/settings.json"))
            .load()
            .unwrap();
        assert_eq!(saved.keymap, "tmux");
    }

    #[test]
    fn the_cpu_dropdowns_switch_each_place_on_its_own() {
        use crate::cpu::Place;
        use crate::state::CpuMode;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &root.join("cfg/extensions"),
        );
        ws.open_settings();
        ws.show_settings_section(SettingsSection::Agents);
        let mut app = crate::build_app(&ws, VIEWPORT);
        ws.take_dirty();

        let picker = app
            .find_widget(Role::ComboBox, Some(EN.cpu_remote))
            .unwrap();
        app.dispatch_click(picker);
        rebuild(&ws, &mut app);
        assert_eq!(visible_options(&app).len(), 3);
        let quiet = ws.borrow().settings.cpu_mode.describe(CpuMode::Quiet, &EN);
        let quiet = app.find_widget(Role::ListBoxOption, Some(&quiet)).unwrap();
        app.dispatch_click(quiet);
        let modes = ws.borrow().settings.cpu_mode;
        assert_eq!(modes.of(Place::Remote), CpuMode::Quiet);
        assert_eq!(modes.of(Place::Local), CpuMode::Balanced, "only one place");
        assert_eq!(ws.borrow().settings_ui.picker, None);
        let saved = SettingsStore::new(root.join("cfg/settings.json"))
            .load()
            .unwrap();
        assert_eq!(saved.cpu_mode.remote, CpuMode::Quiet);
    }

    /// A CPU dropdown at the lower edge of the scrolled Agents page opens a
    /// list that would run past it. Opening one scrolls the page to show the
    /// list whole, not cut off by the dialog's edge.
    #[test]
    fn the_cpu_dropdowns_lists_show_whole_at_the_pages_end() {
        for name in [EN.cpu_local, EN.cpu_remote] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let ws = Workspace::new(
                StateStore::new(root.join("cfg/state.json")),
                SettingsStore::new(root.join("cfg/settings.json")),
                &root.join("cfg/extensions"),
            );
            ws.open_settings();
            ws.show_settings_section(SettingsSection::Agents);
            // Tall enough that the list fits below the trigger in the window,
            // where it opens unless it runs past the window's bottom.
            let viewport = Size {
                height: 1400.0,
                ..VIEWPORT
            };
            let mut app = crate::build_app(&ws, viewport);
            ws.take_dirty();
            app.frame();
            // The trigger shows just above the page's lower edge.
            let page = app
                .find_widget(Role::ScrollView, Some(EN.agent_list))
                .unwrap();
            let picker = app.find_widget(Role::ComboBox, Some(name)).unwrap();
            let view = app.scene().layout(page).unwrap().rect;
            let trigger = app.scene().layout(picker).unwrap().rect;
            app.dispatch_scroll(page, trigger.bottom() + 12.0 - view.bottom());
            app.frame();
            app.dispatch_click(picker);
            assert!(ws.take_dirty().settings);
            let regions = ws.regions();
            app.apply_subtree_replacement(
                SubtreeReplacement::new(regions.settings, settings(&ws, viewport), "settings")
                    .with_focus_after(regions.settings_focus),
            )
            .unwrap();
            app.frame();
            let scene = app.scene();
            let options: Vec<_> = (scene.preorder())
                .filter(|id| scene.is_effectively_visible(*id))
                .filter(|id| {
                    (scene.a11y(*id)).is_some_and(|a| Role::from_u16(a.role) == Role::ListBoxOption)
                })
                .collect();
            assert_eq!(options.len(), 3, "{name}");
            for option in options {
                let whole = scene.layout(option).unwrap().rect.height;
                let shown = scene.composited_rect(option).height;
                assert!(shown >= whole - 0.5, "{name}: {shown} of {whole} shown");
            }
        }
    }

    #[test]
    fn the_control_socket_dropdown_picks_where_agents_reach_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &root.join("cfg/extensions"),
        );
        ws.open_settings();
        ws.show_settings_section(SettingsSection::Agents);
        let mut app = crate::build_app(&ws, VIEWPORT);
        ws.take_dirty();
        assert!(visible_options(&app).is_empty(), "starts collapsed");

        let picker = app
            .find_widget(Role::ComboBox, Some(EN.agent_control))
            .unwrap();
        app.dispatch_click(picker);
        rebuild(&ws, &mut app);
        assert_eq!(visible_options(&app).len(), 4);

        let local = app
            .find_widget(Role::ListBoxOption, Some(EN.agent_control_local))
            .unwrap();
        app.dispatch_click(local);
        assert_eq!(ws.borrow().settings.agent_control, AgentControl::Local);
        assert_eq!(ws.borrow().settings_ui.picker, None);
        let saved = SettingsStore::new(root.join("cfg/settings.json"))
            .load()
            .unwrap();
        assert_eq!(saved.agent_control, AgentControl::Local);
    }

    /// schnellui hides a `Scroll`'s bar unless asked, and settings lists lost
    /// theirs more than once.
    #[test]
    fn a_settings_list_that_overflows_shows_its_scroll_bar() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &root.join("cfg/extensions"),
        );
        ws.open_settings();
        ws.show_settings_section(SettingsSection::Keyboard);
        let mut app = crate::build_app(&ws, VIEWPORT);
        app.frame();
        let list = app
            .find_widget(Role::ScrollView, Some(EN.binding_list))
            .unwrap();
        let paint = app.scene().paint(list).map_or(0, |p| p.primitives.len());
        assert_eq!(paint, 2, "track and thumb");
    }

    #[test]
    fn the_machine_pages_show_their_controls() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &root.join("cfg/extensions"),
        );
        let paths = ws.remote_paths();
        let other = cmdterm_remote::keys::Identity::from_seed(&[9; 64]);
        cmdterm_remote::keys::authorize(
            &paths.authorized_keys(),
            &other.public().to_line("laptop"),
        )
        .unwrap();
        cmdterm_remote::keys::trust_host(&paths.known_hosts(), "box:7722", other.public()).unwrap();
        let vms = root.join("vms");
        std::fs::create_dir_all(vms.join("local/dev")).unwrap();
        std::fs::write(vms.join("local/dev/rootfs.ext4"), "").unwrap();
        std::fs::write(
            paths.server_config(),
            serde_json::json!({
                "firecracker": {"kernel": "/k", "rootfs": "/r", "state_dir": vms}
            })
            .to_string(),
        )
        .unwrap();
        ws.open_settings();
        let viewport = Size {
            width: 1100.0,
            height: 700.0,
        };
        let pages = [
            (SettingsSection::Remote, EN.machine_address, EN.add_machine),
            (SettingsSection::Vms, EN.vm_name, "Set up Ubuntu 24.04"),
            (
                SettingsSection::SandboxFiles,
                EN.sandbox_path,
                EN.add_writable,
            ),
            (SettingsSection::SandboxHosts, EN.sandbox_host, EN.add_host),
        ];
        for (section, field, button) in pages {
            ws.show_settings_section(section);
            // Add host shows once the field holds a host to add.
            if section == SettingsSection::SandboxHosts {
                ws.set_machine_text(crate::machines::MachineText::SandboxHost, "example.org");
            }
            let app = crate::build_app(&ws, viewport);
            assert!(
                app.find_widget(Role::TextInput, Some(field)).is_some(),
                "{section:?}"
            );
            assert!(
                app.find_widget(Role::Button, Some(button)).is_some(),
                "{section:?}"
            );
        }
        ws.show_settings_section(SettingsSection::Remote);
        let app = crate::build_app(&ws, viewport);
        for name in [
            "Remove laptop",
            "Open a project on box:7722",
            "Forget box:7722",
        ] {
            assert!(
                app.find_widget(Role::Button, Some(name)).is_some(),
                "{name}"
            );
        }
        // A key's own switches write its options, and Defaults takes them back.
        let mut app = crate::build_app(&ws, viewport);
        let shell = app
            .find_widget(Role::Switch, Some("Shell for laptop"))
            .unwrap();
        assert!(
            app.find_widget(Role::Button, Some("Use defaults for laptop"))
                .is_none()
        );
        app.dispatch_click(shell);
        let options = ws.borrow().settings_ui.machines.files.allowed[0].options;
        assert!(options.vm_only());
        let mut app = crate::build_app(&ws, viewport);
        let reset = app
            .find_widget(Role::Button, Some("Use defaults for laptop"))
            .unwrap();
        app.dispatch_click(reset);
        let options = ws.borrow().settings_ui.machines.files.allowed[0].options;
        assert_eq!(options, cmdterm_remote::keys::KeyOptions::default());
        ws.show_settings_section(SettingsSection::Vms);
        let mut app = crate::build_app(&ws, viewport);
        let open = app
            .find_widget(Role::Button, Some("Open a project on dev"))
            .unwrap();
        app.dispatch_click(open);
        assert!(!ws.settings_is_open());
        assert_eq!(ws.borrow().palette.query, "vm://dev/~/");
        ws.close_palette();
        ws.open_settings();

        // Serving from the page binds the listen address in the settings.
        ws.borrow_mut().settings.remote.listen = "127.0.0.1:0".into();
        ws.show_settings_section(SettingsSection::Remote);
        let mut app = crate::build_app(&ws, viewport);
        let serve = app
            .find_widget(Role::Switch, Some(EN.serve_machine))
            .unwrap();
        app.dispatch_click(serve);
        assert!(ws.serving_on().is_some());
        // What connecting machines get shows once they may connect.
        let app = crate::build_app(&ws, viewport);
        assert!(
            app.find_widget(Role::Switch, Some(EN.share_projects))
                .is_some()
        );
        assert!(
            SettingsStore::new(root.join("cfg/settings.json"))
                .load()
                .unwrap()
                .remote
                .serve
        );
        ws.stop_serving();
    }

    #[test]
    fn the_project_page_edits_the_active_projects_sandbox_folders() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &root.join("cfg/extensions"),
        );
        ws.open_project_settings();
        let app = crate::build_app(&ws, VIEWPORT);
        assert!(
            app.find_widget(Role::TextInput, Some(EN.sandbox_path))
                .is_none()
        );

        let id = ws.borrow_mut().state.add_project(
            "demo".into(),
            cmdterm_env::Environment::Local,
            root.join("repo"),
            crate::vcs::VcsKind::Git,
            None,
        );
        ws.borrow_mut().state.active_project = Some(id);
        ws.borrow_mut().dirty.settings = true;
        // Windows runs worktrees on the host unless asked.
        if let Some(project) = ws.borrow_mut().state.project_mut(id) {
            project.worktree_mut(&root.join("repo")).runtime = crate::state::Runtime::Sandbox;
        }
        let mut app = crate::build_app(&ws, VIEWPORT);
        assert!(
            app.find_widget(Role::TextInput, Some(EN.sandbox_path))
                .is_some()
        );
        assert!(app.find_widget(Role::Switch, Some(EN.project_jj)).is_some());
        // The reference branch applies only when set.
        let base = app
            .find_widget(Role::TextInput, Some(EN.project_base))
            .unwrap();
        assert!(app.set_text_value(base, "develop"));
        assert_eq!(ws.borrow().state.project(id).unwrap().base, None);
        let set = app
            .find_widget(Role::Button, Some(EN.project_base_set))
            .unwrap();
        app.dispatch_click(set);
        assert_eq!(
            ws.borrow().state.project(id).unwrap().base.as_deref(),
            Some("develop")
        );
        assert!(
            app.find_widget(Role::Button, Some(EN.run_in_sandbox))
                .is_none()
        );
        assert!(
            app.find_widget(Role::Button, Some(EN.choose_folder))
                .is_some()
        );
        // A folder from the chooser fills the path input.
        let sender = ws.borrow().picked_folders.0.clone();
        sender
            .send((
                crate::workspace::FolderTarget::ProjectSandbox,
                "/srv/picked".into(),
            ))
            .unwrap();
        ws.poll();
        assert_eq!(
            ws.borrow().settings_ui.machines.form.project_path,
            "/srv/picked"
        );
        assert!(ws.borrow().dirty.settings);
        ws.set_machine_text(crate::machines::MachineText::ProjectPath, " ~/shared ");
        let add = app
            .find_widget(Role::Button, Some(EN.add_writable))
            .unwrap();
        app.dispatch_click(add);
        ws.set_machine_text(crate::machines::MachineText::ProjectPath, "/etc/creds");
        ws.add_project_sandbox_path(false);
        let saved = || {
            let state = StateStore::new(root.join("cfg/state.json")).load().unwrap();
            state.projects[0].sandbox.clone()
        };
        let sandbox = saved();
        assert_eq!(sandbox.writable, ["~/shared"]);
        assert_eq!(sandbox.readable, ["/etc/creds"]);

        let mut app = crate::build_app(&ws, VIEWPORT);
        let remove = app
            .find_widget(Role::Button, Some("Remove ~/shared"))
            .unwrap();
        app.dispatch_click(remove);
        assert!(saved().writable.is_empty());

        // The launches switch is on the Allowed page.
        let app = crate::build_app(&ws, VIEWPORT);
        assert!(
            app.find_widget(Role::Switch, Some(EN.project_launches))
                .is_none()
        );
        ws.show_settings_section(SettingsSection::ProjectAllowed);
        let mut app = crate::build_app(&ws, VIEWPORT);
        let launches = app
            .find_widget(Role::Switch, Some(EN.project_launches))
            .unwrap();
        app.dispatch_click(launches);
        assert!(ws.borrow().state.project(id).unwrap().allows_launches);
    }

    #[test]
    fn the_allowed_and_blocked_pages_edit_what_the_project_answered_for_good() {
        use crate::state::GuardEntry;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &root.join("cfg/extensions"),
        );
        let id = ws.borrow_mut().state.add_project(
            "demo".into(),
            cmdterm_env::Environment::Local,
            root.join("repo"),
            crate::vcs::VcsKind::Git,
            None,
        );
        ws.borrow_mut().state.active_project = Some(id);
        let guard = cmdterm_env::builtin_guards()[0].clone();
        assert_eq!(guard.id, "cmdterm.git-push");
        let approvals = std::sync::Arc::clone(&ws.borrow().runtimes.approvals);
        let scope = id.to_string();
        let ask = |host: &str| {
            let (approvals, guard, scope, host) = (
                std::sync::Arc::clone(&approvals),
                guard.clone(),
                scope.clone(),
                host.to_owned(),
            );
            std::thread::spawn(move || approvals.ask(&guard, Some(&scope), &host, "POST", "/r"))
        };
        let saved = || StateStore::new(root.join("cfg/state.json")).load().unwrap();
        let entry = |host: &str| GuardEntry {
            guard: guard.id.clone(),
            host: host.to_owned(),
        };

        // The Blocked page adds an entry for the first guard, in lowercase.
        ws.open_settings();
        ws.show_settings_section(SettingsSection::ProjectBlocked);
        let mut app = crate::build_app(&ws, VIEWPORT);
        assert!(app.a11y_json().contains("refused without a question"));
        let host = app
            .find_widget(Role::TextInput, Some(EN.project_answer_host))
            .unwrap();
        assert!(app.set_text_value(host, " Example.ORG "));
        let add = app
            .find_widget(Role::Button, Some(EN.project_answer_add))
            .unwrap();
        app.dispatch_click(add);
        assert_eq!(saved().projects[0].always_blocked, [entry("example.org")]);
        assert!(
            ws.borrow()
                .settings_ui
                .machines
                .form
                .project_answer_host
                .is_empty()
        );
        // Blocked requests are refused without a question.
        assert!(!ask("example.org").join().unwrap());
        assert!(!ws.approval_is_open());

        // Moved to the Allowed page, it passes without a question.
        ws.take_dirty();
        let mut app = crate::build_app(&ws, VIEWPORT);
        let allow = format!("{}: git push: example.org", EN.project_answer_to_allowed);
        let allow = app.find_widget(Role::Button, Some(&allow)).unwrap();
        app.dispatch_click(allow);
        let project = saved().projects[0].clone();
        assert!(project.always_blocked.is_empty());
        assert_eq!(project.always_allowed, [entry("example.org")]);
        assert!(ask("example.org").join().unwrap());

        // Adding it as blocked takes it off the allowed list.
        ws.show_settings_section(SettingsSection::ProjectAllowed);
        ws.set_machine_text(
            crate::machines::MachineText::ProjectAnswerHost,
            "example.org",
        );
        ws.add_project_answer(true);
        let project = saved().projects[0].clone();
        assert!(project.always_allowed.is_empty());
        assert_eq!(project.always_blocked, [entry("example.org")]);

        // Another guard can be picked; the keyring keeps a label's case.
        ws.set_project_answer_guard(cmdterm_env::KEYRING_GUARD);
        ws.set_machine_text(
            crate::machines::MachineText::ProjectAnswerHost,
            "GitHub token",
        );
        ws.add_project_answer(false);
        assert_eq!(
            saved().projects[0].always_allowed,
            [GuardEntry {
                guard: cmdterm_env::KEYRING_GUARD.into(),
                host: "GitHub token".into(),
            }]
        );
        ws.take_dirty();
        let mut app = crate::build_app(&ws, VIEWPORT);
        // The keyring guard is a built-in one on Linux only.
        let title = (cmdterm_env::builtin_guards().into_iter())
            .find(|g| g.id == cmdterm_env::KEYRING_GUARD)
            .map_or_else(|| cmdterm_env::KEYRING_GUARD.to_owned(), |g| g.title);
        let remove = format!("Remove {title}: GitHub token");
        let remove = app.find_widget(Role::Button, Some(&remove)).unwrap();
        app.dispatch_click(remove);
        assert!(saved().projects[0].always_allowed.is_empty());

        // The Blocked page removes its entry, and the guard asks again.
        ws.show_settings_section(SettingsSection::ProjectBlocked);
        let mut app = crate::build_app(&ws, VIEWPORT);
        let remove = app
            .find_widget(Role::Button, Some("Remove git push: example.org"))
            .unwrap();
        app.dispatch_click(remove);
        assert!(saved().projects[0].always_blocked.is_empty());
        let asking = ask("example.org");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ws.approval_is_open() {
            assert!(std::time::Instant::now() < deadline, "no question came");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let question = ws.borrow().pending_approval().unwrap().id;
        ws.answer_approval(question, false);
        assert!(!asking.join().unwrap());
    }

    #[test]
    fn the_project_page_edits_environment_variables_with_keyring_and_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &root.join("cfg/extensions"),
        );
        let id = ws.borrow_mut().state.add_project(
            "demo".into(),
            cmdterm_env::Environment::Local,
            root.join("repo"),
            crate::vcs::VcsKind::Git,
            None,
        );
        ws.borrow_mut().state.active_project = Some(id);
        ws.open_project_settings();

        // With Secret Service available, values go only to the keyring and never to state.json.
        let backend = crate::project_env::tests::MemoryBackend::available();
        ws.set_project_env_backend(backend.clone());
        let mut app = crate::build_app(&ws, VIEWPORT);

        // Invalid variable names are rejected without saving.
        let name_input = app
            .find_widget(Role::TextInput, Some(EN.project_env_name))
            .unwrap();
        assert!(app.set_text_value(name_input, "BAD-NAME"));
        let save_btn = app
            .find_widget(Role::Button, Some(EN.project_env_add))
            .unwrap();
        app.dispatch_click(save_btn);
        assert!(ws.borrow().state.project(id).unwrap().env_vars.is_empty());
        assert!(
            ws.borrow()
                .settings_ui
                .machines
                .form
                .project_env_error
                .is_some()
        );

        // Valid variable saves into keyring; state.json only keeps its name.
        ws.set_machine_text(crate::machines::MachineText::ProjectEnvName, " API_SECRET ");
        ws.set_machine_text(
            crate::machines::MachineText::ProjectEnvValue,
            "super-secret-value",
        );
        app.dispatch_click(save_btn);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while ws.borrow().project_env.has_pending() && std::time::Instant::now() < deadline {
            ws.poll();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        ws.poll();
        let raw = std::fs::read_to_string(root.join("cfg/state.json")).unwrap();
        assert!(raw.contains("API_SECRET"), "{raw}");
        assert!(
            !raw.contains("super-secret-value"),
            "secret must not be written to state.json: {raw}"
        );
        let account = crate::project_env::account_key(ws.borrow().state.project(id).unwrap());
        let stored = backend
            .entries
            .lock()
            .unwrap()
            .get(&(crate::project_env::SERVICE.to_owned(), account.clone()))
            .cloned()
            .expect("value stored in keyring");
        assert!(stored.contains("super-secret-value"), "{stored}");

        // Removing via UI deletes it from state.json and the keyring.
        let mut app = crate::build_app(&ws, VIEWPORT);
        let remove = app
            .find_widget(Role::Button, Some("Remove API_SECRET"))
            .unwrap();
        app.dispatch_click(remove);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while ws.borrow().project_env.has_pending() && std::time::Instant::now() < deadline {
            ws.poll();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        ws.poll();
        assert!(ws.borrow().state.project(id).unwrap().env_vars.is_empty());
        assert!(
            !backend
                .entries
                .lock()
                .unwrap()
                .contains_key(&(crate::project_env::SERVICE.to_owned(), account))
        );

        // When no keyring is available, values fall back to state.json.
        ws.set_project_env_backend(crate::project_env::tests::MemoryBackend::unavailable());
        ws.set_machine_text(crate::machines::MachineText::ProjectEnvName, "PLAIN_KEY");
        ws.set_machine_text(crate::machines::MachineText::ProjectEnvValue, "plain-val");
        ws.add_project_env_var();
        let saved = StateStore::new(root.join("cfg/state.json")).load().unwrap();
        assert_eq!(
            saved.project(id).unwrap().env_vars,
            vec![crate::state::ProjectEnvVar {
                name: "PLAIN_KEY".into(),
                value: Some("plain-val".into()),
            }]
        );
    }

    #[test]
    fn the_find_field_narrows_the_nav_to_matching_pages() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &root.join("cfg/extensions"),
        );
        ws.open_settings();
        let mut app = crate::build_app(&ws, VIEWPORT);
        let _ = ws.take_dirty();
        let page =
            |app: &schnellui::App, name: &str| app.find_widget(Role::Button, Some(name)).is_some();
        assert!(page(&app, EN.section_keyboard));
        // A setting's words find the page that holds it.
        ws.set_settings_query("Inspect HTTPS");
        rebuild(&ws, &mut app);
        assert!(page(&app, EN.section_sandboxing));
        // The page beside the nav is the one found.
        assert_eq!(ws.borrow().settings_ui.section, SettingsSection::Sandboxing);
        assert!(!page(&app, EN.section_keyboard));
        let field = app
            .find_widget(Role::TextInput, Some(EN.find_setting))
            .unwrap();
        assert_eq!(schnellui::a11y::focused(app.scene()), Some(field));
        ws.set_settings_query("zzz");
        rebuild(&ws, &mut app);
        assert!(
            app.find_widget(Role::Label, Some(&(EN.settings_no_match)("zzz")))
                .is_some()
        );
    }

    #[test]
    fn sandboxing_lists_files_and_hosts_as_sub_pages() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &root.join("cfg/extensions"),
        );
        ws.open_settings();
        let app = crate::build_app(&ws, VIEWPORT);
        assert!(
            app.find_widget(Role::Button, Some(EN.section_sandboxing))
                .is_some()
        );
        // Sub-pages show in the nav even while their parent is not shown.
        assert!(
            app.find_widget(Role::Button, Some(EN.section_sandbox_files))
                .is_some()
        );

        ws.show_settings_section(SettingsSection::Sandboxing);
        let mut app = crate::build_app(&ws, VIEWPORT);
        let files = app
            .find_widget(Role::Button, Some(EN.section_sandbox_files))
            .unwrap();
        assert!(
            app.find_widget(Role::Button, Some(EN.section_sandbox_hosts))
                .is_some()
        );
        // Paths live on their own page now.
        assert!(
            app.find_widget(Role::TextInput, Some(EN.sandbox_path))
                .is_none()
        );
        app.dispatch_click(files);
        assert_eq!(
            ws.borrow().settings_ui.section,
            SettingsSection::SandboxFiles
        );
        let app = crate::build_app(&ws, VIEWPORT);
        assert!(
            app.find_widget(Role::Button, Some(EN.section_sandbox_hosts))
                .is_some()
        );
    }

    #[test]
    fn the_container_page_picks_engine_and_runtime_from_dropdowns() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &root.join("cfg/extensions"),
        );
        ws.open_settings();
        ws.show_settings_section(SettingsSection::Sandboxing);
        let mut app = crate::build_app(&ws, VIEWPORT);
        ws.take_dirty();
        assert!(visible_options(&app).is_empty(), "starts collapsed");

        // Where new worktrees run belongs to the app, so it shows without
        // an open worktree.
        use crate::state::Runtime;
        let runtime = app
            .find_widget(Role::ComboBox, Some(EN.setup_runtime))
            .unwrap();
        app.dispatch_click(runtime);
        rebuild(&ws, &mut app);
        assert_eq!(
            visible_options(&app),
            [EN.run_on_host, EN.run_in_devcontainer, EN.run_in_sandbox]
        );
        let host = app
            .find_widget(Role::ListBoxOption, Some(EN.run_on_host))
            .unwrap();
        app.dispatch_click(host);
        assert_eq!(ws.borrow().settings.default_runtime, Runtime::Host);
        assert_eq!(ws.borrow().settings_ui.picker, None);
        let saved = SettingsStore::new(root.join("cfg/settings.json"))
            .load()
            .unwrap();
        assert_eq!(saved.default_runtime, Runtime::Host);
        rebuild(&ws, &mut app);

        let engine = app
            .find_widget(Role::ComboBox, Some(EN.container_engine))
            .unwrap();
        app.dispatch_click(engine);
        rebuild(&ws, &mut app);
        assert_eq!(visible_options(&app), [EN.automatic, "docker", "podman"]);
        let podman = app
            .find_widget(Role::ListBoxOption, Some("podman"))
            .unwrap();
        app.dispatch_click(podman);
        assert_eq!(
            ws.borrow().settings.container_engine.as_deref(),
            Some("podman")
        );
        assert_eq!(ws.borrow().settings_ui.picker, None);
        let saved = SettingsStore::new(root.join("cfg/settings.json"))
            .load()
            .unwrap();
        assert_eq!(saved.container_engine.as_deref(), Some("podman"));

        // An engine only settings.json names shows as its own option.
        ws.borrow_mut().settings.container_engine = Some("nerdctl".into());
        ws.toggle_picker(Picker::ContainerEngine);
        rebuild(&ws, &mut app);
        assert_eq!(
            visible_options(&app),
            [EN.automatic, "docker", "podman", "nerdctl"]
        );
    }

    #[test]
    fn the_about_pages_show_the_changelog_and_open_one_license_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &root.join("cfg/extensions"),
        );
        ws.open_settings();
        ws.show_settings_section(SettingsSection::Changelog);
        let mut app = crate::build_app(&ws, VIEWPORT);
        ws.take_dirty();
        let running = (EN.changelog_running)(env!("CARGO_PKG_VERSION"));
        assert!(app.find_widget(Role::Label, Some(&running)).is_some());
        assert!(app.find_widget(Role::Label, Some("Added")).is_some());

        ws.show_settings_section(SettingsSection::Licenses);
        rebuild(&ws, &mut app);
        let mit = app.find_widget(Role::Button, Some(EN.license_mit)).unwrap();
        app.dispatch_click(mit);
        rebuild(&ws, &mut app);
        assert_eq!(
            ws.borrow().settings_ui.license.as_deref(),
            Some(LICENSE_MIT_KEY)
        );
        assert!(app.find_widget(Role::Label, Some("MIT License")).is_some());

        // Another row's text replaces the open one.
        let apache = app
            .find_widget(Role::Button, Some(EN.license_apache))
            .unwrap();
        app.dispatch_click(apache);
        rebuild(&ws, &mut app);
        assert_eq!(
            ws.borrow().settings_ui.license.as_deref(),
            Some(LICENSE_APACHE_KEY)
        );
        assert!(app.find_widget(Role::Label, Some("MIT License")).is_none());
        let apache = app
            .find_widget(Role::Button, Some(EN.license_apache))
            .unwrap();
        app.dispatch_click(apache);
        assert_eq!(ws.borrow().settings_ui.license, None);

        // The search finds both pages.
        assert!(page_matches(SettingsSection::Licenses, "copyright", &EN));
        assert!(page_matches(
            SettingsSection::Changelog,
            "release notes",
            &EN
        ));
    }

    #[test]
    fn picking_a_language_remounts_in_it_and_saves_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &root.join("cfg/extensions"),
        );
        ws.open_settings();
        ws.show_settings_section(SettingsSection::Language);
        let mut app = crate::build_app(&ws, VIEWPORT);
        ws.take_dirty();

        let picker = app.find_widget(Role::ComboBox, Some(EN.language)).unwrap();
        app.dispatch_click(picker);
        rebuild(&ws, &mut app);
        assert_eq!(
            visible_options(&app),
            ["System (English)", "English", "Deutsch"]
        );

        let german = app
            .find_widget(Role::ListBoxOption, Some("Deutsch"))
            .unwrap();
        app.dispatch_click(german);
        assert_eq!(ws.borrow().settings_ui.picker, None);
        let dirty = ws.take_dirty();
        assert!(dirty.full && dirty.settings);
        assert!(std::ptr::eq(ws.tr(), &crate::i18n::DE));
        let saved = SettingsStore::new(root.join("cfg/settings.json"))
            .load()
            .unwrap();
        assert_eq!(saved.language, "de");

        let app = crate::build_app(&ws, VIEWPORT);
        assert!(
            app.find_widget(Role::Button, Some("Einstellungen schließen"))
                .is_some()
        );
        assert!(
            app.find_widget(Role::Button, Some("Close settings"))
                .is_none()
        );

        // The system entry follows the locale, which tests see as English.
        ws.set_language(SYSTEM_LANGUAGE);
        assert!(ws.take_dirty().full);
        assert!(std::ptr::eq(ws.tr(), &EN));
    }

    #[test]
    fn guards_get_a_mode_on_the_extensions_page_and_ask_in_a_dialog() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ext = root.join("cfg/extensions");
        std::fs::create_dir_all(&ext).unwrap();
        std::fs::create_dir_all(ext.join("forge")).unwrap();
        std::fs::write(
            ext.join("forge/manifest.toml"),
            r#"description = "Pushes to the forge"
permissions = ["sandbox", "guards"]

[[settings]]
key = "remote"
title = "Remote name"
description = "Which remote to push to."
type = "string"
default = "origin"

[[settings]]
key = "force"
title = "Allow force pushes"
type = "bool"
"#,
        )
        .unwrap();
        std::fs::write(
            ext.join("forge/init.lua"),
            r#"term.sandbox{ guards = { { name = "push", title = "push to the forge",
                hosts = { "*" }, requests = { "POST */git-receive-pack" } } } }
               local s = term.settings()
               term.notify(s.remote .. " " .. tostring(s.force))"#,
        )
        .unwrap();
        let ws = Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &ext,
        );
        // Not loaded until the user grants what the manifest declares.
        assert!(ws.borrow().ext.sandbox_rules().guards.is_empty());
        ws.open_settings();
        ws.show_settings_section(SettingsSection::Extensions);
        // The section opens on the list of extensions.
        let app = crate::build_app(&ws, VIEWPORT);
        ws.take_dirty();
        let forge = app
            .find_widget(Role::ListBoxOption, Some("forge"))
            .or_else(|| app.find_widget(Role::Button, Some("forge")));
        assert!(forge.is_some(), "the extension is listed");
        // The built-in page has the proxy's own guards.
        ws.show_extension_page(None);
        let app = crate::build_app(&ws, VIEWPORT);
        ws.take_dirty();
        let builtin = (EN.guard_mode_for)("git push", EN.guard_allow);
        assert!(app.find_widget(Role::Button, Some(&builtin)).is_some());
        assert!(
            app.find_widget(Role::Button, Some(EN.all_extensions))
                .is_some()
        );
        ws.show_extension_page(Some("forge".to_owned()));
        let mut app = crate::build_app(&ws, VIEWPORT);
        ws.take_dirty();
        let json = app.a11y_json();
        assert!(json.contains("Pushes to the forge"), "{json}");
        assert!(json.contains("Remote name"), "{json}");
        let missing = ws
            .borrow()
            .ext
            .extensions()
            .find(|e| e.name == "forge")
            .map(|e| e.missing.names());
        assert_eq!(
            missing,
            Some(vec!["sandbox".to_owned(), "guards".to_owned()])
        );
        let grant = app
            .find_widget(Role::Button, Some(&(EN.grant_for)("forge")))
            .unwrap();
        app.dispatch_click(grant);
        assert_eq!(ws.borrow().ext.sandbox_rules().guards.len(), 1);
        assert_eq!(
            ws.borrow().settings.extension_permissions["forge"],
            ["guards", "sandbox"]
        );
        let mut app = crate::build_app(&ws, VIEWPORT);
        ws.take_dirty();
        let ask = (EN.guard_mode_for)("push to the forge", EN.guard_ask);
        let block = (EN.guard_mode_for)("push to the forge", EN.guard_block);
        assert!(app.find_widget(Role::Button, Some(&ask)).is_some());
        // Manifest settings: a switch applies at once, typing when the page
        // is left.
        ws.borrow_mut().ext.drain_requests(&mut Vec::new());
        let force = app
            .find_widget(Role::Switch, Some("Allow force pushes"))
            .unwrap();
        app.dispatch_click(force);
        let field = app
            .find_widget(Role::TextInput, Some("Remote name"))
            .unwrap();
        assert!(app.set_text_value(field, "upstream"));
        let mut requests = Vec::new();
        ws.borrow_mut().ext.drain_requests(&mut requests);
        assert!(
            matches!(requests.as_slice(), [crate::ext::Request::Notify { message, .. }] if message == "origin true"),
            "{requests:?}"
        );
        ws.show_extension_page(None);
        ws.borrow_mut().ext.drain_requests(&mut requests);
        assert!(
            matches!(requests.last(), Some(crate::ext::Request::Notify { message, .. }) if message == "upstream true"),
            "{requests:?}"
        );
        ws.show_extension_page(Some("forge".to_owned()));
        let mut app = crate::build_app(&ws, VIEWPORT);
        ws.take_dirty();
        app.dispatch_click(app.find_widget(Role::Button, Some(&block)).unwrap());
        assert_eq!(
            ws.borrow().settings.sandbox.guard_mode("forge.push", None),
            GuardMode::Block
        );
        let saved = std::fs::read_to_string(root.join("cfg/settings.json")).unwrap();
        assert!(saved.contains(r#""forge.push": "block""#), "{saved}");
        rebuild(&ws, &mut app);
        ws.set_guard_mode("forge.push", GuardMode::Ask);
        ws.close_settings();
        ws.take_dirty();

        // A question from a proxy thread opens the dialog; Allow answers it.
        let guard = ws.borrow().ext.sandbox_rules().guards[0].clone();
        let approvals = std::sync::Arc::clone(&ws.borrow().runtimes.approvals);
        let asking = std::thread::spawn(move || {
            approvals.ask(
                &guard,
                None,
                "example.org",
                "POST",
                "/me/r.git/git-receive-pack",
            )
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ws.approval_is_open() {
            assert!(std::time::Instant::now() < deadline, "no question came");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        ws.poll();
        assert!(ws.take_dirty().approval);
        let regions = ws.regions();
        app.apply_subtree_replacement(
            SubtreeReplacement::new(
                regions.approval,
                crate::ui::approval(&ws, VIEWPORT),
                "approval",
            )
            .with_focus_after(regions.approval_focus),
        )
        .unwrap();
        app.frame();
        let json = app.a11y_json();
        assert!(
            json.contains("A sandbox wants to push to the forge"),
            "{json}"
        );
        assert!(json.contains("POST example.org/me/r.git/git-receive-pack"));
        let allow = app
            .find_widget(Role::Button, Some(EN.approval_allow))
            .unwrap();
        let block = app
            .find_widget(Role::Button, Some(EN.approval_block))
            .unwrap();
        assert_eq!(app.focused_widget(), Some(block), "Enter blocks");
        app.dispatch_click(allow);
        assert!(asking.join().unwrap());
        assert!(!ws.approval_is_open());
    }

    #[test]
    fn always_allow_keeps_the_answer_in_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &root.join("cfg/extensions"),
        );
        let id = ws.borrow_mut().state.add_project(
            "demo".into(),
            cmdterm_env::Environment::Local,
            root.join("repo"),
            crate::vcs::VcsKind::Git,
            None,
        );
        let guard = cmdterm_env::builtin_guards()[0].clone();
        let approvals = std::sync::Arc::clone(&ws.borrow().runtimes.approvals);
        let ask = move |scope: Option<u64>| {
            let (approvals, guard) = (std::sync::Arc::clone(&approvals), guard.clone());
            std::thread::spawn(move || {
                let scope = scope.map(|id| id.to_string());
                approvals.ask(&guard, scope.as_deref(), "example.org", "POST", "/r")
            })
        };
        let wait_for_question = || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !ws.approval_is_open() {
                assert!(std::time::Instant::now() < deadline, "no question came");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            ws.poll();
            ws.take_dirty();
            crate::build_app(&ws, VIEWPORT)
        };

        // Without a project there is nothing to keep the answer in.
        let unscoped = ask(None);
        let app = wait_for_question();
        assert!(
            app.find_widget(Role::Button, Some(EN.approval_always))
                .is_none()
        );
        let question = ws.borrow().pending_approval().unwrap().id;
        ws.answer_approval(question, false);
        assert!(!unscoped.join().unwrap());

        let asking = ask(Some(id));
        let mut app = wait_for_question();
        assert!(app.a11y_json().contains("From project demo"));
        let always = app
            .find_widget(Role::Button, Some(EN.approval_always))
            .unwrap();
        app.dispatch_click(always);
        assert!(asking.join().unwrap());
        let saved = std::fs::read_to_string(root.join("cfg/state.json")).unwrap();
        assert!(saved.contains(r#""always_allowed""#), "{saved}");
        // The next request in the project passes without a question.
        assert!(ask(Some(id)).join().unwrap());
        assert!(!ws.approval_is_open());

        // The built-in page lists it, and Remove takes it back.
        ws.open_settings();
        ws.show_settings_section(SettingsSection::Extensions);
        ws.show_extension_page(None);
        ws.take_dirty();
        let mut app = crate::build_app(&ws, VIEWPORT);
        let remove = format!("{} (demo): git push: example.org", EN.always_remove);
        let remove = app.find_widget(Role::Button, Some(&remove)).unwrap();
        app.dispatch_click(remove);
        assert!(
            ws.borrow()
                .state
                .project(id)
                .unwrap()
                .always_allowed
                .is_empty()
        );
        // Taken back, it asks again. Always block keeps the refusal, and
        // the next request is refused without a question.
        let asking = ask(Some(id));
        let mut app = wait_for_question();
        let never = app
            .find_widget(Role::Button, Some(EN.approval_never))
            .unwrap();
        app.dispatch_click(never);
        assert!(!asking.join().unwrap());
        let project = ws.borrow().state.project(id).unwrap().clone();
        assert_eq!(project.always_blocked.len(), 1);
        assert!(project.always_allowed.is_empty());
        assert!(!ask(Some(id)).join().unwrap());
        assert!(!ws.approval_is_open());
    }

    #[test]
    fn shipped_extensions_are_granted_and_the_list_filters() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        crate::bundle::install(&cmdterm_env::AppDirs::under(&root.join("cfg")));
        let ext = root.join("cfg/extensions");
        let ws = Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &ext,
        );
        assert!(ws.borrow().ext.extensions().all(|e| e.missing.is_empty()));
        ws.set_extension_enabled("dotfiles", false);
        ws.open_settings();
        ws.show_settings_section(SettingsSection::Extensions);
        let app = crate::build_app(&ws, VIEWPORT);
        ws.take_dirty();
        let listed = |app: &schnellui::App, name: &str| {
            app.find_widget(Role::ListBoxOption, Some(name))
                .or_else(|| app.find_widget(Role::Button, Some(name)))
                .is_some()
        };
        // All by default: a turned-off extension can still be found.
        assert!(listed(&app, "npm"));
        assert!(listed(&app, "Dotfiles"));
        // The npm guard moved to the npm page; hg push stays built in.
        let npm = (EN.guard_mode_for)("publish to npm", EN.guard_ask);
        let hg = (EN.guard_mode_for)("hg push", EN.guard_ask);
        ws.show_extension_page(None);
        let builtin = crate::build_app(&ws, VIEWPORT);
        assert!(builtin.find_widget(Role::Button, Some(&npm)).is_none());
        assert!(builtin.find_widget(Role::Button, Some(&hg)).is_some());
        ws.show_extension_list();
        let app = crate::build_app(&ws, VIEWPORT);
        ws.take_dirty();
        // The find field narrows the list by name.
        ws.set_extension_query("GIT");
        let found = crate::build_app(&ws, VIEWPORT);
        assert!(listed(&found, "GitHub"));
        assert!(!listed(&found, "npm"));
        ws.set_extension_query("");
        let disabled = (EN.guard_mode_for)(EN.extension_filter, EN.filter_disabled);
        let mut app = app;
        app.dispatch_click(app.find_widget(Role::Button, Some(&disabled)).unwrap());
        let app = crate::build_app(&ws, VIEWPORT);
        assert!(listed(&app, "Dotfiles"));
        assert!(!listed(&app, "npm"));
        let saved = std::fs::read_to_string(root.join("cfg/settings.json")).unwrap();
        assert!(
            saved.contains(r#""extension_filter": "disabled""#),
            "{saved}"
        );

        ws.show_extension_page(Some("npm".to_owned()));
        let mut app = crate::build_app(&ws, VIEWPORT);
        ws.take_dirty();
        assert!(app.find_widget(Role::Button, Some(&npm)).is_some());
        // Revoking a shipped extension sticks over its default grant.
        let revoke = app
            .find_widget(Role::Button, Some(&(EN.revoke_for)("npm")))
            .unwrap();
        app.dispatch_click(revoke);
        assert_eq!(
            ws.borrow().settings.extension_permissions["npm"],
            Vec::<String>::new()
        );
        let npm_ext = |ws: &Workspace| {
            ws.borrow()
                .ext
                .extensions()
                .find(|e| e.name == "npm")
                .map(|e| e.missing.is_empty())
        };
        assert_eq!(npm_ext(&ws), Some(false));
        ws.grant_extension_permissions("npm");
        assert_eq!(npm_ext(&ws), Some(true));
    }

    #[test]
    fn one_permission_at_a_time_is_granted_and_saved() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ext = root.join("cfg/extensions");
        std::fs::create_dir_all(ext.join("mixed")).unwrap();
        std::fs::write(
            ext.join("mixed/manifest.toml"),
            r#"permissions = ["env", "secrets"]"#,
        )
        .unwrap();
        std::fs::write(
            ext.join("mixed/init.lua"),
            r#"term.notify(type(os.getenv) == "function" and "env" or "noenv")"#,
        )
        .unwrap();
        let ws = Workspace::new(
            StateStore::new(root.join("cfg/state.json")),
            SettingsStore::new(root.join("cfg/settings.json")),
            &ext,
        );
        // Nothing is granted: it waits, running nothing.
        assert!(
            ws.borrow()
                .ext
                .extensions()
                .find(|e| e.name == "mixed")
                .is_some_and(|e| !e.enabled)
        );
        ws.open_settings();
        ws.show_settings_section(SettingsSection::Extensions);
        ws.show_extension_page(Some("mixed".to_owned()));
        let mut app = crate::build_app(&ws, VIEWPORT);
        ws.take_dirty();
        // Turn on just `env`.
        let env_switch = app
            .find_widget(Role::Switch, Some(&(EN.permission_for)("mixed", "env")))
            .unwrap();
        app.dispatch_click(env_switch);
        // The grant saved is exactly the subset the user turned on.
        assert_eq!(ws.borrow().settings.extension_permissions["mixed"], ["env"]);
        let saved = SettingsStore::new(root.join("cfg/settings.json"))
            .load()
            .unwrap();
        assert_eq!(saved.extension_permissions["mixed"], ["env"]);
        // It runs, with only `env`.
        let mixed = ws
            .borrow()
            .ext
            .extensions()
            .find(|e| e.name == "mixed")
            .cloned()
            .unwrap();
        assert!(mixed.enabled);
        assert_eq!(mixed.missing.names(), ["secrets"]);
    }
}
