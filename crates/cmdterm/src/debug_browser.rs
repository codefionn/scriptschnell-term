//! The debug browser panel: shows the app's one debug browser
//! (`devtools`) and passes the user's input to it. Every such
//! panel, in every worktree, shows the same browser and follows the tab an
//! agent used last.
//!
//! The browser runs on its own threads. The panel draws its frames through
//! a dynamic image keyed by the hub's frame revision, so a new frame
//! rewrites the image's texels and no view remounts. Only a change of the
//! browser's status (it failed to start, or recovered) remounts the panel.

use std::rc::Rc;

use crate::devtools::{
    Button as MouseButton, Choice, Config, Input, Mouse, MouseKind, Status, hub,
};
use crate::state::{DebugBrowserKind, Settings};
use schnellui::raw_keyboard::Key;
use schnellui::scene::Size;
use schnellui::widgets::{Button, Column, DynamicImageFrame, Image, Scroll, Text, View};
use schnellui::{
    FocusedClipboardEvent, FocusedInputEvent, FocusedInputResult, RawImeEvent, RawInputState,
    RawModifiers, RawPointerAction, RawPointerButton, RawWheelDelta,
};

use crate::i18n::Strings;
use crate::workspace::MAX_PANES;

/// Accessible names of the debug browser panels, by pane position, like
/// [`crate::terminal_panel::TERMINAL_NAMES`].
pub const DEBUG_BROWSER_NAMES: [&str; MAX_PANES] = [
    "Debug browser",
    "Debug browser (pane 2)",
    "Debug browser (pane 3)",
    "Debug browser (pane 4)",
    "Debug browser (pane 5)",
    "Debug browser (pane 6)",
    "Debug browser (pane 7)",
    "Debug browser (pane 8)",
];

/// Pixels one wheel line scrolls.
const LINE: f64 = 40.0;

/// Hands the hub the browser the settings ask for. A running browser keeps
/// running; the next start uses the new choice.
pub fn configure(settings: &Settings, dirs: &cmdterm_env::AppDirs) {
    let browser = &settings.debug_browser;
    hub().configure(Config {
        browser: match browser.browser {
            DebugBrowserKind::Auto => Choice::Auto,
            DebugBrowserKind::Chrome => Choice::Chrome,
            DebugBrowserKind::Firefox => Choice::Firefox,
        },
        executable: browser.executable.as_ref().map(Into::into),
        headless: browser.headless,
        profile_dir: dirs.cache.join("debug-browser"),
    });
}

/// One debug browser panel. The browser itself lives in the hub.
pub struct DebugBrowserPanel {
    tr: &'static Strings,
}

impl DebugBrowserPanel {
    pub fn new(tr: &'static Strings) -> Self {
        Self { tr }
    }

    pub fn title(&self) -> String {
        let title = hub().title();
        if title.is_empty() || title == "about:blank" {
            self.tr.debug_browser.to_owned()
        } else {
            title
        }
    }

    pub fn view(&self, size: Size, label: &'static str) -> Box<dyn View> {
        let tr = self.tr;
        if let Status::Failed(error) = hub().status() {
            return Box::new(
                Column::new()
                    .width(size.width)
                    .gap(12.0)
                    .child(Text::new(tr.debug_browser_failed).size(15.0))
                    .child(Text::new(error).size(12.0))
                    .child(Button::new(tr.debug_browser_retry).on_click(|| hub().retry())),
            );
        }
        Box::new(
            Scroll::new()
                .label(label)
                .size(size.width, size.height)
                .child(
                    Image::dynamic_rgba_versioned(
                        || hub().frame_revision(),
                        || {
                            hub().frame().map(|frame| {
                                DynamicImageFrame::new(
                                    frame.width,
                                    frame.height,
                                    Rc::<[u8]>::from(&*frame.pixels),
                                )
                            })
                        },
                    )
                    .alt(tr.debug_browser_alt)
                    .size(size.width, size.height),
                ),
        )
    }
}

fn modifiers(raw: RawModifiers) -> u8 {
    let mut bits = 0;
    if raw.alt {
        bits |= crate::devtools::ALT;
    }
    if raw.control {
        bits |= crate::devtools::CONTROL;
    }
    if raw.super_key {
        bits |= crate::devtools::META;
    }
    if raw.shift {
        bits |= crate::devtools::SHIFT;
    }
    bits
}

/// Passes focused input of a panel of `size` to the browser.
pub fn input(event: FocusedInputEvent, size: Size) -> FocusedInputResult {
    let panel = (size.width.max(1.0) as u32, size.height.max(1.0) as u32);
    let input = match event {
        FocusedInputEvent::Pointer(pointer) => {
            let (kind, button) = match pointer.action {
                // The page keeps its own hover; nothing to send.
                RawPointerAction::Leave => return FocusedInputResult::Ignored,
                RawPointerAction::Move => (MouseKind::Move, MouseButton::None),
                RawPointerAction::Button { button, state } => {
                    let button = match button {
                        RawPointerButton::Left => MouseButton::Left,
                        RawPointerButton::Right => MouseButton::Right,
                        RawPointerButton::Middle => MouseButton::Middle,
                        _ => return FocusedInputResult::Ignored,
                    };
                    let kind = match state {
                        RawInputState::Pressed => MouseKind::Down,
                        RawInputState::Released => MouseKind::Up,
                    };
                    (kind, button)
                }
                RawPointerAction::Wheel(delta) => {
                    let (dx, dy) = match delta {
                        RawWheelDelta::Lines { x, y } => (f64::from(x) * LINE, f64::from(y) * LINE),
                        RawWheelDelta::Pixels { x, y } => (f64::from(x), f64::from(y)),
                    };
                    (MouseKind::Wheel { dx: -dx, dy: -dy }, MouseButton::None)
                }
            };
            Input::Mouse(Mouse {
                kind,
                x: f64::from(pointer.position.x),
                y: f64::from(pointer.position.y),
                button,
                clicks: u32::from(matches!(kind, MouseKind::Down | MouseKind::Up)),
                modifiers: modifiers(pointer.modifiers),
            })
        }
        FocusedInputEvent::Key(key) => {
            let bits = modifiers(key.modifiers);
            let stroke = match &key.logical_key {
                Key::Named(named) => crate::devtools::keys::stroke(&format!("{named:?}"), bits),
                Key::Character(text) => {
                    let mut chars = text.chars();
                    match (chars.next(), chars.next()) {
                        (Some(c), None) => Some(crate::devtools::keys::char_stroke(c, bits)),
                        _ => {
                            if key.state == RawInputState::Pressed
                                && bits & !crate::devtools::SHIFT == 0
                            {
                                hub().input(Input::Text(text.to_string()), panel);
                            }
                            return FocusedInputResult::Handled;
                        }
                    }
                }
                _ => None,
            };
            let Some(stroke) = stroke else {
                return FocusedInputResult::Ignored;
            };
            Input::Key(stroke, key.state == RawInputState::Pressed)
        }
        FocusedInputEvent::Ime(RawImeEvent::Commit(text)) => Input::Text(text),
        FocusedInputEvent::Clipboard(FocusedClipboardEvent::Paste(text)) => Input::Text(text),
        FocusedInputEvent::Clipboard(FocusedClipboardEvent::Copy) => {
            hub().copy_selection();
            return FocusedInputResult::Handled;
        }
        FocusedInputEvent::Focus(_) => return FocusedInputResult::Handled,
        _ => return FocusedInputResult::Ignored,
    };
    hub().input(input, panel);
    FocusedInputResult::Handled
}
