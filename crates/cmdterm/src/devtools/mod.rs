//! The debug browser: one Chrome or Firefox for the whole app, started on
//! first use and driven over its own debugging protocol (`cdp.rs` for
//! Chrome, `bidi.rs` for Firefox).
//!
//! Two kinds of users share it. The debug browser panels of the app show
//! one of its tabs and pass the user's input to it: [`Hub::show`] tells the
//! panel thread here which size a panel has, the thread streams frames of
//! the shown tab into [`Hub::frame`] and bumps a revision the UI compares.
//! Agents in the app's terminals reach it as MCP tools through the control
//! socket ([`mcp`]). Each agent works in a session of its own, an isolated
//! browser context with its own cookies and tabs, named by an id the agent
//! passes to every tool. The MCP side keeps no connection state: any
//! request that names a session id may use it, so agents can hand a
//! session to one another.
//!
//! The panel follows the tab an agent used last, so the user watches what
//! the agent does.

use std::collections::HashMap;
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

mod bidi;
mod cdp;
mod driver;
pub mod keys;
mod launch;
pub mod mcp;
mod rpc;
mod ws;

pub use driver::{
    ALT, Button, CONTROL, Console, Driver, KeyStroke, Kind, META, Mouse, MouseKind, SHIFT, TabInfo,
};

/// Starts the `kind` browser headless with its profile in `profiles`,
/// opens `page` in a context behind `proxy` and returns the page's text.
#[cfg(test)]
pub(crate) fn text_through_proxy(
    kind: Choice,
    profiles: &std::path::Path,
    proxy: &str,
    page: &str,
) -> Result<String, String> {
    let config = Config {
        browser: kind,
        executable: None,
        headless: true,
        profile_dir: profiles.to_path_buf(),
    };
    let mut launched = launch::launch(&config, Arc::new(driver::Sink::new(|| {})))?;
    let driver = launched.driver.clone();
    let text = (|| {
        let context = driver.create_context(Some(proxy))?;
        let tab = driver.new_tab(Some(&context), "about:blank")?;
        driver.navigate(&tab, page)?;
        let text = driver.evaluate(
            &tab,
            "Promise.resolve(JSON.stringify(document.body.innerText))",
        )?;
        Ok(text.as_str().unwrap_or_default().to_owned())
    })();
    driver.shutdown();
    let _ = launched.child.kill();
    let _ = launched.child.wait();
    text
}

/// Which browser the settings ask for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Choice {
    /// Chrome or Chromium when installed, else Firefox.
    #[default]
    Auto,
    Chrome,
    Firefox,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub browser: Choice,
    /// The browser's executable; `None` looks on `PATH`.
    pub executable: Option<std::path::PathBuf>,
    /// Runs without a window of its own; the panels show it.
    pub headless: bool,
    /// Where the browser's profiles go.
    pub profile_dir: std::path::PathBuf,
}

/// Where the browser is, as the panel shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Status {
    #[default]
    Off,
    Starting,
    Running(Kind),
    Failed(String),
}

/// One frame of the shown tab, RGBA.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub pixels: Arc<[u8]>,
}

/// Input from a panel, in logical pixels of the panel.
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    Mouse(Mouse),
    Key(KeyStroke, bool),
    Text(String),
}

/// An agent's session: an isolated context of the browser.
#[derive(Clone, Debug)]
pub(crate) struct Session {
    pub context: String,
    /// The tab the session's tools act on.
    pub tab: Option<String>,
    /// A viewport the agent asked for. Without one, tabs take the panel's
    /// size while shown.
    pub viewport: Option<(u32, u32)>,
    pub created: Instant,
    /// The proxy the context was created with, for sandboxed callers.
    pub proxy: Option<String>,
}

/// The running browser.
pub(crate) struct Browser {
    pub driver: Arc<dyn Driver>,
    pub sink: Arc<driver::Sink>,
    child: Mutex<Child>,
    /// Tab ids in the order they were first seen, which is the order the
    /// tools number them in. Protocols list them in an order of their own.
    order: Mutex<Vec<String>>,
}

impl Browser {
    /// The open tabs, oldest first.
    pub fn tabs(&self) -> Result<Vec<TabInfo>, String> {
        let mut tabs = self.driver.tabs()?;
        let mut order = lock(&self.order);
        order.retain(|id| tabs.iter().any(|t| &t.id == id));
        for tab in &tabs {
            if !order.contains(&tab.id) {
                order.push(tab.id.clone());
            }
        }
        tabs.sort_by_key(|t| order.iter().position(|id| *id == t.id));
        Ok(tabs)
    }

    fn alive(&self) -> bool {
        self.driver.alive() && matches!(lock(&self.child).try_wait(), Ok(None))
    }

    fn stop(&self) {
        self.driver.shutdown();
        let mut child = lock(&self.child);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if !matches!(child.try_wait(), Ok(None)) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

type Wake = Arc<dyn Fn() + Send + Sync>;

/// Default viewport of tabs no panel shows.
pub const DEFAULT_VIEWPORT: (u32, u32) = (1280, 800);
/// Smallest viewport a panel gives its tab.
const MIN_VIEWPORT: (u32, u32) = (200, 150);
/// Time between two screenshots of a browser that cannot stream frames.
const POLL: Duration = Duration::from_millis(250);
/// Time between two title checks of the shown tab.
const TITLE_POLL: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct Hub {
    config: Mutex<Option<Config>>,
    browser: Mutex<Option<Arc<Browser>>>,
    /// Held while the browser starts, so only one start runs.
    launching: Mutex<()>,
    pub(crate) sessions: Mutex<HashMap<String, Session>>,
    /// The tab the panels show.
    shown: Mutex<Option<String>>,
    status: Mutex<Status>,
    status_revision: AtomicU64,
    frame: Mutex<Option<Frame>>,
    frame_revision: AtomicU64,
    title: Mutex<String>,
    title_revision: AtomicU64,
    /// The panel's size as `width << 32 | height`; 0 while no panel shows.
    panel: AtomicU64,
    inputs: Mutex<Vec<(Input, u64)>>,
    worker: OnceLock<Thread>,
    /// Set by [`Self::retry`] after a failed start.
    retry: AtomicBool,
    /// Set when the app quits; no browser starts after.
    closed: AtomicBool,
    wake: Mutex<Option<Wake>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

/// The app's one debug browser.
pub fn hub() -> &'static Hub {
    static HUB: OnceLock<Hub> = OnceLock::new();
    HUB.get_or_init(Hub::default)
}

fn pack(size: Option<(u32, u32)>) -> u64 {
    size.map_or(0, |(w, h)| {
        (u64::from(w.max(1)) << 32) | u64::from(h.max(1))
    })
}

fn unpack(packed: u64) -> Option<(u32, u32)> {
    (packed != 0).then_some(((packed >> 32) as u32, packed as u32))
}

impl Hub {
    /// Which browser to start next time. A running one keeps running. Lets
    /// a browser start again after [`Self::shutdown`].
    pub fn configure(&self, config: Config) {
        *lock(&self.config) = Some(config);
        self.closed.store(false, Ordering::Release);
    }

    /// What the UI thread calls when a frame, the title or the status
    /// changed.
    pub fn set_wake(&self, wake: impl Fn() + Send + Sync + 'static) {
        *lock(&self.wake) = Some(Arc::new(wake));
    }

    fn wake_ui(&self) {
        let wake = lock(&self.wake).clone();
        if let Some(wake) = wake {
            wake();
        }
    }

    pub fn status(&self) -> Status {
        lock(&self.status).clone()
    }

    pub fn status_revision(&self) -> u64 {
        self.status_revision.load(Ordering::Acquire)
    }

    fn set_status(&self, status: Status) {
        let mut current = lock(&self.status);
        if *current != status {
            *current = status;
            drop(current);
            self.status_revision.fetch_add(1, Ordering::AcqRel);
            self.wake_ui();
        }
    }

    pub fn frame_revision(&self) -> u64 {
        self.frame_revision.load(Ordering::Acquire)
    }

    pub fn frame(&self) -> Option<Frame> {
        lock(&self.frame).clone()
    }

    /// The shown tab's title, else its address.
    pub fn title(&self) -> String {
        lock(&self.title).clone()
    }

    pub fn title_revision(&self) -> u64 {
        self.title_revision.load(Ordering::Acquire)
    }

    /// Tells the panel thread the size of the panel that shows the browser,
    /// `None` when none does. Called every frame: it compares one atomic
    /// and allocates nothing unless the size changed.
    pub fn show(&self, size: Option<(u32, u32)>) {
        let packed = pack(size);
        if self.panel.swap(packed, Ordering::AcqRel) == packed {
            return;
        }
        match self.worker.get() {
            Some(worker) => worker.unpark(),
            None if packed != 0 => {
                let spawned = thread::Builder::new()
                    .name("debug-browser-panel".into())
                    .spawn(|| hub().panel_loop());
                if let Ok(handle) = spawned {
                    let _ = self.worker.set(handle.thread().clone());
                }
            }
            None => {}
        }
    }

    /// Passes the user's input to the shown tab. `panel` is the panel's
    /// size, to which the input's positions are relative.
    pub fn input(&self, input: Input, panel: (u32, u32)) {
        lock(&self.inputs).push((input, pack(Some(panel))));
        if let Some(worker) = self.worker.get() {
            worker.unpark();
        }
    }

    /// Tries to start the browser again after a failure.
    pub fn retry(&self) {
        self.retry.store(true, Ordering::Release);
        self.set_status(Status::Off);
        if let Some(worker) = self.worker.get() {
            worker.unpark();
        }
    }

    /// Loads `url` in the shown tab, from the app's palette. Runs on a
    /// thread of its own, since loading takes a while.
    pub fn open(&'static self, url: String) {
        let _ = thread::Builder::new()
            .name("debug-browser-open".into())
            .spawn(move || {
                // A failed start shows in the panel; a page that does not
                // load shows as the browser's own error page.
                let _ = self.browser().and_then(|browser| {
                    let tab = self.shown_tab(&browser)?;
                    browser.driver.navigate(&tab, &url)
                });
            });
    }

    /// Puts the text selected in the shown tab on the clipboard, for
    /// Ctrl+Shift+C. Runs on a thread of its own, since the page answers
    /// later.
    pub fn copy_selection(&'static self) {
        let _ = thread::Builder::new()
            .name("debug-browser-copy".into())
            .spawn(move || {
                let selected = self.browser().and_then(|browser| {
                    let tab = self.shown_tab(&browser)?;
                    browser
                        .driver
                        .evaluate(&tab, "JSON.stringify(String(getSelection()))")
                });
                if let Ok(serde_json::Value::String(text)) = selected
                    && !text.is_empty()
                {
                    crate::clipboard::copy_text(text);
                }
            });
    }

    /// Stops the browser, when the app quits.
    pub fn shutdown(&self) {
        self.closed.store(true, Ordering::Release);
        let _launching = lock(&self.launching);
        let browser = lock(&self.browser).take();
        if let Some(browser) = browser {
            browser.stop();
        }
        lock(&self.sessions).clear();
    }

    /// The running browser, started first if needed.
    pub(crate) fn browser(&self) -> Result<Arc<Browser>, String> {
        if let Some(browser) = lock(&self.browser).clone()
            && browser.alive()
        {
            return Ok(browser);
        }
        let _launching = lock(&self.launching);
        if self.closed.load(Ordering::Acquire) {
            return Err("the app is quitting".into());
        }
        if let Some(browser) = lock(&self.browser).clone() {
            if browser.alive() {
                return Ok(browser);
            }
            browser.stop();
        }
        // The contexts of the sessions died with the old browser, and its
        // last frame and title no longer show anything.
        lock(&self.sessions).clear();
        *lock(&self.shown) = None;
        *lock(&self.browser) = None;
        if lock(&self.frame).take().is_some() {
            self.frame_revision.fetch_add(1, Ordering::AcqRel);
        }
        self.publish_title(String::new());
        let config = lock(&self.config)
            .clone()
            .ok_or("the app set up no debug browser")?;
        self.set_status(Status::Starting);
        let sink = Arc::new(driver::Sink::new(|| {
            if let Some(worker) = hub().worker.get() {
                worker.unpark();
            }
        }));
        match launch::launch(&config, sink.clone()) {
            Ok(launched) => {
                let kind = launched.driver.kind();
                let browser = Arc::new(Browser {
                    driver: launched.driver,
                    sink,
                    child: Mutex::new(launched.child),
                    order: Mutex::default(),
                });
                *lock(&self.browser) = Some(browser.clone());
                self.set_status(Status::Running(kind));
                Ok(browser)
            }
            Err(error) => {
                self.set_status(Status::Failed(error.clone()));
                Err(error)
            }
        }
    }

    /// The running browser without starting one.
    pub(crate) fn running(&self) -> Option<Arc<Browser>> {
        lock(&self.browser).clone().filter(|b| b.alive())
    }

    /// Shows `tab` in the panels.
    pub(crate) fn show_tab(&self, tab: &str) {
        let mut shown = lock(&self.shown);
        if shown.as_deref() != Some(tab) {
            *shown = Some(tab.to_owned());
            drop(shown);
            if let Some(worker) = self.worker.get() {
                worker.unpark();
            }
        }
    }

    /// The tab the panels show: the one an agent used last if it still
    /// exists, else the newest tab outside every session, else a new one.
    fn shown_tab(&self, browser: &Browser) -> Result<String, String> {
        let tabs = browser.tabs()?;
        if let Some(shown) = lock(&self.shown).clone()
            && tabs.iter().any(|tab| tab.id == shown)
        {
            return Ok(shown);
        }
        let contexts: Vec<String> = (lock(&self.sessions).values())
            .map(|s| s.context.clone())
            .collect();
        let free = tabs.iter().rev().find(|tab| {
            tab.context
                .as_ref()
                .is_none_or(|context| !contexts.contains(context))
        });
        let tab = match free {
            Some(tab) => tab.id.clone(),
            None => browser.driver.new_tab(None, "about:blank")?,
        };
        *lock(&self.shown) = Some(tab.clone());
        Ok(tab)
    }

    /// The viewport `tab` should have while a panel of `panel` size shows it.
    fn viewport_for(&self, tab: &str, panel: (u32, u32)) -> (u32, u32) {
        let fixed = (lock(&self.sessions).values())
            .find(|s| s.tab.as_deref() == Some(tab))
            .and_then(|s| s.viewport);
        fixed.unwrap_or((panel.0.max(MIN_VIEWPORT.0), panel.1.max(MIN_VIEWPORT.1)))
    }

    fn publish_frame(&self, bytes: &[u8]) {
        let Ok(image) = image::load_from_memory(bytes) else {
            return;
        };
        let rgba = image.into_rgba8();
        let (width, height) = rgba.dimensions();
        *lock(&self.frame) = Some(Frame {
            width,
            height,
            pixels: rgba.into_raw().into(),
        });
        self.frame_revision.fetch_add(1, Ordering::AcqRel);
        self.wake_ui();
    }

    fn publish_title(&self, title: String) {
        let mut current = lock(&self.title);
        if *current != title {
            *current = title;
            drop(current);
            self.title_revision.fetch_add(1, Ordering::AcqRel);
            self.wake_ui();
        }
    }

    /// The panel thread: starts the browser while a panel shows it, keeps
    /// the shown tab's viewport at the panel's size, streams or polls its
    /// frames, and passes the user's input on.
    fn panel_loop(&self) {
        let mut streaming: Option<String> = None;
        let mut sized: HashMap<String, (u32, u32)> = HashMap::new();
        let mut last_poll = Instant::now() - POLL;
        let mut last_shot: Option<u64> = None;
        let mut last_title = Instant::now() - TITLE_POLL;
        let mut tabs_seen = u64::MAX;
        let mut tab: Option<String> = None;
        loop {
            let Some(panel) = unpack(self.panel.load(Ordering::Acquire)) else {
                if streaming.take().is_some()
                    && let Some(browser) = self.running()
                {
                    browser.driver.stream(None);
                }
                lock(&self.inputs).clear();
                thread::park_timeout(Duration::from_secs(1));
                continue;
            };
            if matches!(self.status(), Status::Failed(_))
                && !self.retry.swap(false, Ordering::AcqRel)
            {
                thread::park_timeout(Duration::from_secs(1));
                continue;
            }
            let browser = match self.browser() {
                Ok(browser) => browser,
                Err(_) => {
                    streaming = None;
                    thread::park_timeout(Duration::from_secs(1));
                    continue;
                }
            };
            let changed = browser.sink.tabs.load(Ordering::Acquire);
            let wanted = lock(&self.shown).clone();
            if tab.is_none() || wanted != tab || changed != tabs_seen {
                tabs_seen = changed;
                match self.shown_tab(&browser) {
                    Ok(shown) => {
                        if tab.as_deref() != Some(&shown) {
                            last_shot = None;
                            last_title = Instant::now() - TITLE_POLL;
                        }
                        tab = Some(shown);
                    }
                    Err(error) => {
                        self.set_status(Status::Failed(error));
                        continue;
                    }
                }
            }
            let Some(current) = tab.clone() else { continue };
            let viewport = self.viewport_for(&current, panel);
            if sized.get(&current) != Some(&viewport)
                && browser
                    .driver
                    .set_viewport(&current, viewport.0, viewport.1)
                    .is_ok()
            {
                sized.insert(current.clone(), viewport);
            }
            let inputs = std::mem::take(&mut *lock(&self.inputs));
            for (input, size) in inputs {
                let size = unpack(size).unwrap_or(panel);
                let scale = (
                    f64::from(viewport.0) / f64::from(size.0.max(1)),
                    f64::from(viewport.1) / f64::from(size.1.max(1)),
                );
                let _ = match input {
                    Input::Mouse(mut mouse) => {
                        mouse.x *= scale.0;
                        mouse.y *= scale.1;
                        browser.driver.mouse(&current, &mouse)
                    }
                    Input::Key(key, down) => browser.driver.key(&current, &key, down),
                    Input::Text(text) => browser.driver.insert_text(&current, &text),
                };
            }
            let pushes = if streaming.as_deref() == Some(&current) {
                true
            } else {
                let pushes = browser.driver.stream(Some(&current));
                streaming = pushes.then(|| current.clone());
                pushes
            };
            if pushes {
                let frame = lock(&browser.sink.frame).take();
                if let Some((frame_tab, bytes)) = frame
                    && frame_tab == current
                {
                    self.publish_frame(&bytes);
                }
            } else if last_poll.elapsed() >= POLL {
                last_poll = Instant::now();
                if let Ok(png) = browser.driver.screenshot(&current) {
                    let hash = {
                        use std::hash::{Hash, Hasher};
                        let mut hasher = std::collections::hash_map::DefaultHasher::new();
                        png.hash(&mut hasher);
                        hasher.finish()
                    };
                    if last_shot != Some(hash) {
                        last_shot = Some(hash);
                        self.publish_frame(&png);
                    }
                }
            }
            if last_title.elapsed() >= TITLE_POLL {
                last_title = Instant::now();
                let tabs = browser.tabs().unwrap_or_default();
                if let Some(info) = tabs.iter().find(|t| t.id == current) {
                    let title = if info.title.trim().is_empty() {
                        info.url.clone()
                    } else {
                        info.title.trim().to_owned()
                    };
                    self.publish_title(title);
                }
                sized.retain(|id, _| tabs.iter().any(|t| &t.id == id));
            }
            thread::park_timeout(if pushes {
                Duration::from_millis(200)
            } else {
                POLL
            });
        }
    }
}
