//! What the app asks of a browser, whichever protocol drives it, and what
//! the browser reports back on its own.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

/// Which browser runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Chrome or Chromium, over the DevTools protocol on a pipe.
    Chrome,
    /// Firefox, over WebDriver BiDi on a loopback WebSocket.
    Firefox,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Chrome => "Chrome",
            Kind::Firefox => "Firefox",
        }
    }
}

/// One page of the browser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabInfo {
    pub id: String,
    pub url: String,
    pub title: String,
    /// The browser context (CDP) or user context (BiDi) it belongs to.
    pub context: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    None,
    Left,
    Middle,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MouseKind {
    Move,
    Down,
    Up,
    Wheel { dx: f64, dy: f64 },
}

/// A pointer event in CSS pixels of the page's viewport.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mouse {
    pub kind: MouseKind,
    pub x: f64,
    pub y: f64,
    pub button: Button,
    pub clicks: u32,
    pub modifiers: u8,
}

/// Modifier bits, as CDP numbers them.
pub const ALT: u8 = 1;
pub const CONTROL: u8 = 2;
pub const META: u8 = 4;
pub const SHIFT: u8 = 8;

/// One key, described for both protocols.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyStroke {
    /// The DOM `key` value, such as `Enter` or `a`.
    pub key: String,
    /// The DOM `code` value, such as `Enter` or `KeyA`. Empty when unknown.
    pub code: String,
    /// What the key types, if anything.
    pub text: Option<String>,
    /// The Windows virtual key code CDP wants for keys that do something.
    pub key_code: u32,
    /// What WebDriver sends: the character, or its code point for a named
    /// key.
    pub webdriver: char,
    pub modifiers: u8,
}

pub type Result<T> = std::result::Result<T, String>;

/// A running browser. Every method blocks until the browser answered and
/// may be called from any thread.
pub trait Driver: Send + Sync {
    fn kind(&self) -> Kind;
    /// Whether the connection is still open.
    fn alive(&self) -> bool;
    /// A new isolated context: its own cookies, storage and tabs. With a
    /// `proxy` such as `http://127.0.0.1:8080`, every page of the context
    /// goes through it, `localhost` included. A user name and password in
    /// the URL answer that proxy's `407`, and only its own.
    fn create_context(&self, proxy: Option<&str>) -> Result<String>;
    fn remove_context(&self, context: &str) -> Result<()>;
    fn tabs(&self) -> Result<Vec<TabInfo>>;
    fn title(&self, tab: &str) -> Result<String>;
    /// Opens `url` in a new tab of `context` (the default one for `None`).
    fn new_tab(&self, context: Option<&str>, url: &str) -> Result<String>;
    fn close_tab(&self, tab: &str) -> Result<()>;
    fn activate(&self, tab: &str) -> Result<()>;
    /// Loads `url` and waits until the page finished loading.
    fn navigate(&self, tab: &str, url: &str) -> Result<()>;
    /// Evaluates `expression`, which yields a promise of a JSON string, and
    /// parses that string.
    fn evaluate(&self, tab: &str, expression: &str) -> Result<Value>;
    /// A PNG of the viewport.
    fn screenshot(&self, tab: &str) -> Result<Vec<u8>>;
    fn set_viewport(&self, tab: &str, width: u32, height: u32) -> Result<()>;
    fn mouse(&self, tab: &str, mouse: &Mouse) -> Result<()>;
    fn key(&self, tab: &str, key: &KeyStroke, down: bool) -> Result<()>;
    fn insert_text(&self, tab: &str, text: &str) -> Result<()>;
    /// Streams frames of `tab` into [`Sink::frame`], or stops for `None`.
    /// Returns whether the browser can push frames at all; without, the
    /// panel polls screenshots.
    fn stream(&self, tab: Option<&str>) -> bool;
    fn shutdown(&self);
}

/// A proxy URL split into the address the browser is given and the user
/// name and password it answers the proxy's `407` with:
/// `http://user:pass@127.0.0.1:8080` gives `http://127.0.0.1:8080` and the
/// pair. Browsers take no credentials in their proxy settings.
pub fn split_proxy(proxy: &str) -> (String, Option<(String, String)>) {
    let proxy = proxy.trim_end_matches('/');
    let (scheme, rest) = proxy.split_once("://").unwrap_or(("http", proxy));
    match rest.rsplit_once('@') {
        Some((userinfo, address)) if !address.contains('/') => {
            let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
            let credentials = (user.to_owned(), password.to_owned());
            (format!("{scheme}://{address}"), Some(credentials))
        }
        _ => (format!("{scheme}://{rest}"), None),
    }
}

/// Request ids a driver answered a proxy's `407` for, so a second one for
/// the same request, after a wrong password, is turned down and does not
/// loop.
#[derive(Default)]
pub struct Answered(VecDeque<String>);

impl Answered {
    /// True the first time for `request`.
    pub fn first(&mut self, request: &str) -> bool {
        if self.0.iter().any(|seen| seen == request) {
            return false;
        }
        if self.0.len() == 256 {
            self.0.pop_front();
        }
        self.0.push_back(request.to_owned());
        true
    }
}

/// A console message or an uncaught error of a page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Console {
    pub tab: String,
    pub level: String,
    pub text: String,
}

/// Most console messages kept, over every tab.
const MAX_CONSOLE: usize = 500;

/// What drivers report from their reader threads.
pub struct Sink {
    /// The newest encoded frame (JPEG or PNG) and its tab.
    pub frame: Mutex<Option<(String, Vec<u8>)>>,
    console: Mutex<VecDeque<Console>>,
    /// Bumped when a tab opens, closes or navigates.
    pub tabs: AtomicU64,
    /// Wakes whoever waits for frames.
    wake: Box<dyn Fn() + Send + Sync>,
}

impl Sink {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            frame: Mutex::new(None),
            console: Mutex::default(),
            tabs: AtomicU64::new(0),
            wake: Box::new(wake),
        }
    }

    pub fn frame(&self, tab: String, bytes: Vec<u8>) {
        *self.frame.lock().unwrap_or_else(|p| p.into_inner()) = Some((tab, bytes));
        (self.wake)();
    }

    pub fn console(&self, entry: Console) {
        let mut console = self.console.lock().unwrap_or_else(|p| p.into_inner());
        if console.len() == MAX_CONSOLE {
            console.pop_front();
        }
        console.push_back(entry);
    }

    /// The console messages of `tab`, oldest first.
    pub fn console_of(&self, tab: &str) -> Vec<Console> {
        let console = self.console.lock().unwrap_or_else(|p| p.into_inner());
        console.iter().filter(|c| c.tab == tab).cloned().collect()
    }

    pub fn tabs_changed(&self) {
        self.tabs.fetch_add(1, Ordering::Release);
        (self.wake)();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_credentials_come_off_the_address() {
        assert_eq!(
            split_proxy("http://cmdterm:abc@127.0.0.1:8080"),
            (
                "http://127.0.0.1:8080".into(),
                Some(("cmdterm".into(), "abc".into()))
            )
        );
        assert_eq!(
            split_proxy("http://127.0.0.1:8080/"),
            ("http://127.0.0.1:8080".into(), None)
        );
        let mut answered = Answered::default();
        assert!(answered.first("1") && answered.first("2") && !answered.first("1"));
    }
}
