//! Browser panels. With the `browser` feature a panel is a Servo tab whose
//! frames stream into a retained image (same-size frames rewrite their atlas
//! texels in place, no remount). Without it, the panel keeps its URL so
//! layouts round-trip, and offers to open the page in the system browser.

#[cfg(feature = "browser")]
use std::rc::Rc;

use schnellui::widgets::{Button, Column, Text, View};

use crate::workspace::MAX_PANES;

/// Accessible names of the web content surfaces, by pane position, like
/// [`crate::terminal_panel::TERMINAL_NAMES`]. One focused-input handler per
/// name routes events to the browser panel in its pane.
pub const WEB_CONTENT_NAMES: [&str; MAX_PANES] = [
    "Web content",
    "Web content (pane 2)",
    "Web content (pane 3)",
    "Web content (pane 4)",
    "Web content (pane 5)",
    "Web content (pane 6)",
    "Web content (pane 7)",
    "Web content (pane 8)",
];

pub struct BrowserPanel {
    url: String,
    #[cfg(feature = "browser")]
    tab: Option<Rc<engine::LiveTab>>,
    #[cfg(feature = "browser")]
    error: Option<String>,
}

impl BrowserPanel {
    pub fn new(url: &str) -> Self {
        #[cfg(feature = "browser")]
        {
            let (tab, error) = match engine::open(url) {
                Ok(tab) => (Some(tab), None),
                Err(error) => (None, Some(error)),
            };
            Self {
                url: url.to_owned(),
                tab,
                error,
            }
        }
        #[cfg(not(feature = "browser"))]
        Self {
            url: url.to_owned(),
        }
    }

    /// The current URL: where the tab navigated to, else the saved one.
    pub fn url(&self) -> String {
        #[cfg(feature = "browser")]
        if let Some(url) = self.tab.as_ref().and_then(|tab| tab.url()) {
            return url;
        }
        self.url.clone()
    }

    pub fn title(&self) -> String {
        #[cfg(feature = "browser")]
        if let Some(title) = self.tab.as_ref().and_then(|tab| tab.title()) {
            return title;
        }
        let host = self
            .url
            .split_once("://")
            .map_or(self.url.as_str(), |(_, rest)| rest);
        host.split('/').next().unwrap_or(host).to_owned()
    }

    #[cfg(feature = "browser")]
    pub fn tab(&self) -> Option<&Rc<engine::LiveTab>> {
        self.tab.as_ref()
    }

    pub fn view(&self, width: f32, height: f32, label: &'static str) -> Box<dyn View> {
        #[cfg(feature = "browser")]
        if let Some(tab) = &self.tab {
            return Box::new(tab.view(width, height, label));
        }
        let _ = (height, label);
        let url = self.url.clone();
        #[cfg(feature = "browser")]
        let reason = self
            .error
            .clone()
            .unwrap_or_else(|| "The browser engine is not available.".to_owned());
        #[cfg(not(feature = "browser"))]
        let reason =
            "This build has no embedded browser. Rebuild with --features browser.".to_owned();
        Box::new(
            Column::new()
                .width(width)
                .gap(12.0)
                .child(Text::new(format!("Browser: {}", self.url)).size(15.0))
                .child(Text::new(reason).size(12.0))
                .child(Button::new("Open in system browser").on_click(move || {
                    crate::forges::open_in_system_browser(&url);
                })),
        )
    }
}

#[cfg(feature = "browser")]
impl Drop for BrowserPanel {
    fn drop(&mut self) {
        if let Some(tab) = self.tab.take() {
            engine::close(&tab);
        }
    }
}

#[cfg(feature = "browser")]
pub mod engine {
    //! One Servo instance per process, shared by every browser panel.

    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use schnellui::scene::Size;
    use schnellui::widgets::{DynamicImageFrame, Image, Scroll, View};
    use schnellui::{FocusedInputEvent, FocusedInputResult};
    use schnellui_servo::servo_engine::ServoEngine;
    use schnellui_servo::{Browser, BrowserInput, BrowserTabId, BrowserTabState};
    use url::Url;

    thread_local! {
        static BROWSER: RefCell<Option<Browser<ServoEngine>>> = const { RefCell::new(None) };
    }

    static NEXT_TAB: AtomicU64 = AtomicU64::new(1);

    /// One browser panel's tab and its frame feed.
    pub struct LiveTab {
        id: BrowserTabId,
        revision: Cell<u64>,
        frame: RefCell<Option<DynamicImageFrame>>,
        size: Cell<(u32, u32)>,
        shown_title: RefCell<String>,
    }

    fn with_browser<R>(f: impl FnOnce(&mut Browser<ServoEngine>) -> R) -> Result<R, String> {
        BROWSER.with(|cell| {
            let mut slot = cell.borrow_mut();
            if slot.is_none() {
                // Rustls needs a process-wide provider before Servo visits HTTPS.
                let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
                let engine = ServoEngine::new(1280, 800).map_err(|error| error.to_string())?;
                *slot = Some(Browser::new(engine));
            }
            Ok(f(slot.as_mut().expect("browser initialized above")))
        })
    }

    pub fn open(url: &str) -> Result<Rc<LiveTab>, String> {
        let url = Url::parse(url).map_err(|error| format!("bad URL {url}: {error}"))?;
        let id = BrowserTabId::new(format!(
            "panel-{}",
            NEXT_TAB.fetch_add(1, Ordering::Relaxed)
        ))
        .map_err(|error| error.to_string())?;
        with_browser(|browser| browser.open(BrowserTabState::new(id.clone(), url)))?
            .map_err(|error| error.to_string())?;
        Ok(Rc::new(LiveTab {
            id,
            revision: Cell::new(0),
            frame: RefCell::new(None),
            size: Cell::new((0, 0)),
            shown_title: RefCell::new(String::new()),
        }))
    }

    pub fn close(tab: &LiveTab) {
        let _ = with_browser(|browser| browser.close(&tab.id));
    }

    impl LiveTab {
        pub fn url(&self) -> Option<String> {
            with_browser(|browser| browser.tab(&self.id).map(|tab| tab.url.to_string()))
                .ok()
                .flatten()
        }

        pub fn title(&self) -> Option<String> {
            with_browser(|browser| {
                browser
                    .tab(&self.id)
                    .map(|tab| tab.title.trim().to_owned())
                    .filter(|title| !title.is_empty())
            })
            .ok()
            .flatten()
        }

        /// Whether the page title changed since the last call.
        pub fn title_changed(&self) -> bool {
            let title = self.title().unwrap_or_default();
            let mut shown = self.shown_title.borrow_mut();
            if *shown == title {
                return false;
            }
            *shown = title;
            true
        }

        pub fn view(
            self: &Rc<Self>,
            width: f32,
            height: f32,
            label: &'static str,
        ) -> impl View + use<> {
            let revision = self.clone();
            let frame = self.clone();
            Scroll::new().label(label).size(width, height).child(
                Image::dynamic_rgba_versioned(
                    move || revision.revision.get(),
                    move || frame.frame.borrow().clone(),
                )
                .alt("Web page rendered by Servo")
                .size(width, height),
            )
        }

        /// Spins Servo, resizes this tab to `size`, and publishes a new frame
        /// when Servo painted one. Returns whether a frame was published.
        pub fn pump(&self, size: Size) -> bool {
            let physical = (size.width.max(1.0) as u32, size.height.max(1.0) as u32);
            let resized = self.size.replace(physical) != physical;
            let frame = with_browser(|browser| {
                if resized {
                    let _ = browser.with_engine_tab(&self.id, |engine, tab| {
                        engine.resize(tab, physical.0, physical.1);
                    });
                }
                browser.spin_event_loop();
                let ready = browser
                    .with_engine_tab(&self.id, |engine, tab| engine.frame_ready(tab))
                    .unwrap_or(false);
                if ready || self.frame.borrow().is_none() {
                    browser.render(&self.id).ok().flatten()
                } else {
                    None
                }
            });
            let Ok(Some(frame)) = frame else {
                return false;
            };
            *self.frame.borrow_mut() = Some(DynamicImageFrame::new(
                frame.width,
                frame.height,
                frame.rgba,
            ));
            self.revision.set(self.revision.get() + 1);
            true
        }

        pub fn input(&self, event: FocusedInputEvent) -> FocusedInputResult {
            let Ok(input) = BrowserInput::try_from(event) else {
                return FocusedInputResult::Ignored;
            };
            match with_browser(|browser| browser.dispatch_input(&self.id, input)) {
                Ok(Ok(())) => FocusedInputResult::Handled,
                _ => FocusedInputResult::Ignored,
            }
        }
    }

    /// Requests redraws at about 60 Hz while `visible` is set, so Servo keeps
    /// spinning. Sleeps longer while no browser panel is shown.
    pub fn spawn_ticker(visible: Arc<AtomicBool>, wake: impl Fn() + Send + 'static) {
        let _ = std::thread::Builder::new()
            .name("cmdterm-browser-tick".into())
            .spawn(move || {
                loop {
                    if visible.load(Ordering::Acquire) {
                        wake();
                        std::thread::sleep(std::time::Duration::from_millis(16));
                    } else {
                        std::thread::sleep(std::time::Duration::from_millis(200));
                    }
                }
            });
    }
}
