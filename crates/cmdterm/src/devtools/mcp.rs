//! The debug browser as MCP tools, in the style of Playwright's MCP server.
//!
//! The control socket hands every JSON-RPC message an agent sends to
//! [`handle`] and returns the answer; nothing is kept per connection.
//! `browser_session_new` gives the agent a session id, and every other tool
//! takes it as its `session` argument. A session is an isolated browser
//! context: its own cookies, storage and tabs. It lasts until
//! `browser_session_close` or until the browser stops.
//!
//! Elements are named by the refs of the last `browser_snapshot`
//! (`page.js` keeps them in the page), and pointer tools send real input
//! events to the element's center, like a user would.

use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Map, Value, json};

use crate::devtools::driver::{Button, Mouse, MouseKind};
use crate::devtools::{Browser, DEFAULT_VIEWPORT, Hub, Session, keys, lock};

/// The script every page tool installs first.
const PAGE: &str = include_str!("page.js");
const VERSIONS: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
const MAX_SESSIONS: usize = 16;
/// Longest wait `browser_wait_for` accepts.
const MAX_WAIT: Duration = Duration::from_secs(60);
/// Longest wait for a page to settle after an action.
const SETTLE: Duration = Duration::from_secs(5);

const INSTRUCTIONS: &str = "Drives the debug browser of the scriptschnell-term app this \
terminal belongs to; the user watches it in the app's debug browser panel. Start with \
browser_session_new and pass the session id it returns as `session` to every other tool. \
A session is an isolated browser context with its own cookies and tabs. Give the id to \
another agent to let it work in the same tabs. Take a browser_snapshot to get element refs \
for clicking and typing. Close the session with browser_session_close when you are done.";

/// Who calls, as the control socket knows the terminal.
#[derive(Clone, Debug, Default)]
pub struct Caller {
    /// The terminal runs on this machine, outside sandboxes and containers.
    /// Only such callers may open local files and browser pages.
    pub host: bool,
    /// The hosts a sandbox with a restricted network may reach. Pages
    /// elsewhere stay closed to it. `None` for no limit.
    pub allowed_hosts: Option<Vec<String>>,
    /// Why this terminal may not use the browser at all, if it may not.
    pub refused: Option<String>,
    /// A proxy such as `http://cmdterm:PASSWORD@127.0.0.1:8080` that sends
    /// `localhost` into the caller's sandbox and other hosts through the
    /// sandbox's filter. The driver answers its `407` with the password.
    /// Set for sandboxed callers whose worktree has a port hub. Their
    /// sessions create the context with it, and `localhost` pages pass
    /// the host check, since they are the sandbox's own.
    pub proxy: Option<String>,
}

/// Answers one JSON-RPC message; `None` for a notification.
pub fn handle(hub: &'static Hub, message: &Value, caller: &Caller) -> Option<Value> {
    let id = message.get("id")?.clone();
    let method = message["method"].as_str().unwrap_or_default();
    let params = &message["params"];
    let result = match method {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or_default();
            let version = VERSIONS
                .iter()
                .find(|v| **v == asked)
                .copied()
                .unwrap_or("2025-06-18");
            Ok(json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "scriptschnell-term-browser", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS,
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or_default();
            let args = params["arguments"].as_object().cloned().unwrap_or_default();
            let outcome = match &caller.refused {
                Some(reason) => Err(reason.clone()),
                None => call(hub, name, &args, caller),
            };
            Ok(match outcome {
                Ok(content) => json!({ "content": content }),
                Err(error) => json!({
                    "content": [{ "type": "text", "text": error }],
                    "isError": true,
                }),
            })
        }
        other => Err((-32601, format!("no method {other}"))),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => json!({
            "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message },
        }),
    })
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    let mut properties = properties;
    let mut required: Vec<&str> = required.to_vec();
    if name != "browser_session_new" && name != "browser_sessions" {
        properties["session"] = json!({
            "type": "string",
            "description": "The id browser_session_new returned",
        });
        required.insert(0, "session");
    }
    json!({
        "name": name,
        "description": description,
        "inputSchema": { "type": "object", "properties": properties, "required": required },
    })
}

fn tools() -> Vec<Value> {
    let element =
        json!({ "type": "string", "description": "What the element is, in words, for the user" });
    let reference =
        json!({ "type": "string", "description": "The element's ref from the last snapshot" });
    vec![
        tool(
            "browser_session_new",
            "Starts a session: an isolated browser context with one tab. Returns its id, which every other tool takes as `session`.",
            json!({ "url": { "type": "string", "description": "A page to open, else about:blank" } }),
            &[],
        ),
        tool(
            "browser_sessions",
            "Lists the open sessions and their tabs.",
            json!({}),
            &[],
        ),
        tool(
            "browser_session_close",
            "Closes the session and all its tabs.",
            json!({}),
            &[],
        ),
        tool(
            "browser_navigate",
            "Loads a URL in the session's tab and returns a snapshot.",
            json!({ "url": { "type": "string" } }),
            &["url"],
        ),
        tool(
            "browser_navigate_back",
            "Goes back in the tab's history.",
            json!({}),
            &[],
        ),
        tool(
            "browser_navigate_forward",
            "Goes forward in the tab's history.",
            json!({}),
            &[],
        ),
        tool(
            "browser_snapshot",
            "The page's accessibility tree with a ref per element. Better than a screenshot for acting on the page.",
            json!({}),
            &[],
        ),
        tool(
            "browser_click",
            "Clicks an element.",
            json!({
                "element": element,
                "ref": reference,
                "button": { "type": "string", "enum": ["left", "right", "middle"] },
                "doubleClick": { "type": "boolean" },
            }),
            &["ref"],
        ),
        tool(
            "browser_hover",
            "Moves the mouse over an element.",
            json!({ "element": element, "ref": reference }),
            &["ref"],
        ),
        tool(
            "browser_type",
            "Replaces an editable element's text with `text`.",
            json!({
                "element": element,
                "ref": reference,
                "text": { "type": "string" },
                "submit": { "type": "boolean", "description": "Press Enter afterwards" },
            }),
            &["ref", "text"],
        ),
        tool(
            "browser_press_key",
            "Presses a key such as Enter, ArrowDown, a or Control+a.",
            json!({ "key": { "type": "string" } }),
            &["key"],
        ),
        tool(
            "browser_select_option",
            "Selects options of a select element, by value or label.",
            json!({
                "element": element,
                "ref": reference,
                "values": { "type": "array", "items": { "type": "string" } },
            }),
            &["ref", "values"],
        ),
        tool(
            "browser_evaluate",
            "Runs a JavaScript function in the page and returns its result as JSON. With `ref`, the function gets the element.",
            json!({
                "function": { "type": "string", "description": "() => { ... } or (element) => { ... }" },
                "element": element,
                "ref": reference,
            }),
            &["function"],
        ),
        tool(
            "browser_take_screenshot",
            "A PNG of the tab's viewport.",
            json!({}),
            &[],
        ),
        tool(
            "browser_console_messages",
            "The tab's console messages and errors.",
            json!({}),
            &[],
        ),
        tool(
            "browser_wait_for",
            "Waits for text to appear or disappear, or for a number of seconds.",
            json!({
                "text": { "type": "string" },
                "textGone": { "type": "string" },
                "time": { "type": "number", "description": "Seconds" },
            }),
            &[],
        ),
        tool(
            "browser_resize",
            "Sets the viewport of the session's tabs. Without it they take the panel's size while shown.",
            json!({ "width": { "type": "number" }, "height": { "type": "number" } }),
            &["width", "height"],
        ),
        tool(
            "browser_tabs",
            "Lists, opens, selects or closes the session's tabs.",
            json!({
                "action": { "type": "string", "enum": ["list", "new", "select", "close"] },
                "index": { "type": "number" },
                "url": { "type": "string", "description": "For new" },
            }),
            &["action"],
        ),
    ]
}

type Args = Map<String, Value>;
type Content = Vec<Value>;

fn text(text: String) -> Content {
    vec![json!({ "type": "text", "text": text })]
}

fn string<'a>(args: &'a Args, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("`{key}` is missing"))
}

fn call(hub: &'static Hub, name: &str, args: &Args, caller: &Caller) -> Result<Content, String> {
    match name {
        "browser_session_new" => return session_new(hub, args, caller),
        "browser_sessions" => return sessions(hub),
        _ => {}
    }
    if !tools().iter().any(|t| t["name"] == name) {
        return Err(format!("no tool {name}"));
    }
    let id = string(args, "session")
        .map_err(|_| "`session` is missing; start one with browser_session_new".to_owned())?
        .to_owned();
    let browser = hub.browser()?;
    let session = lock(&hub.sessions).get(&id).cloned().ok_or_else(|| {
        format!("no session {id}; it was closed or the browser restarted. Start one with browser_session_new")
    })?;
    session_allowed(caller, &session)?;
    if name == "browser_session_close" {
        lock(&hub.sessions).remove(&id);
        browser.driver.remove_context(&session.context)?;
        return Ok(text(format!("Closed session {id}.")));
    }
    let tab = tab_of(hub, &browser, &id, &session)?;
    hub.show_tab(&tab);
    let page = Page {
        hub,
        browser: &browser,
        tab: &tab,
        session: &id,
        caller,
    };
    match name {
        "browser_navigate" => {
            let url = check_url(caller, string(args, "url")?)?;
            browser.driver.navigate(&tab, &url)?;
            page.report("", true)
        }
        "browser_navigate_back" | "browser_navigate_forward" => {
            page.guard()?;
            let step = if name == "browser_navigate_back" {
                "back"
            } else {
                "forward"
            };
            browser.driver.evaluate(
                &tab,
                &format!("(history.{step}(), Promise.resolve('null'))"),
            )?;
            page.settle();
            page.report("", true)
        }
        "browser_snapshot" => page.report("", true),
        "browser_click" => {
            let point = page.point(string(args, "ref")?)?;
            let button = match args.get("button").and_then(Value::as_str) {
                Some("right") => Button::Right,
                Some("middle") => Button::Middle,
                _ => Button::Left,
            };
            let double = args.get("doubleClick").and_then(Value::as_bool) == Some(true);
            page.click(point, button, if double { 2 } else { 1 })?;
            page.settle();
            page.report("Clicked.", true)
        }
        "browser_hover" => {
            let (x, y) = page.point(string(args, "ref")?)?;
            page.mouse(MouseKind::Move, x, y, Button::None, 0)?;
            page.report("Hovered.", true)
        }
        "browser_type" => {
            let reference = string(args, "ref")?;
            let value = string(args, "text")?;
            let point = page.point(reference)?;
            page.click(point, Button::Left, 1)?;
            page.eval(&format!(
                "(() => {{ const el = window.__cmdterm.el({}); el.focus(); \
                 if (el.select) el.select(); else {{ const range = document.createRange(); \
                 range.selectNodeContents(el); const sel = getSelection(); \
                 sel.removeAllRanges(); sel.addRange(range); }} return null; }})()",
                json!(reference)
            ))?;
            if value.is_empty() {
                page.press("Backspace")?;
            } else {
                browser.driver.insert_text(&tab, value)?;
            }
            if args.get("submit").and_then(Value::as_bool) == Some(true) {
                page.press("Enter")?;
                page.settle();
            }
            page.report("Typed.", true)
        }
        "browser_press_key" => {
            page.guard()?;
            page.press(string(args, "key")?)?;
            page.settle();
            page.report("Pressed.", true)
        }
        "browser_select_option" => {
            let reference = string(args, "ref")?;
            let values = args.get("values").cloned().unwrap_or(json!([]));
            let picked = page.eval(&format!(
                "(() => {{ const el = window.__cmdterm.el({}); const want = {values}; \
                 if (!el.options) throw new Error('the element is no select'); \
                 for (const o of el.options) o.selected = want.includes(o.value) || \
                 want.includes(o.textContent.trim()); \
                 el.dispatchEvent(new Event('input', {{ bubbles: true }})); \
                 el.dispatchEvent(new Event('change', {{ bubbles: true }})); \
                 return [...el.selectedOptions].map((o) => o.value); }})()",
                json!(reference)
            ))?;
            page.report(&format!("Selected {picked}."), true)
        }
        "browser_evaluate" => {
            let function = string(args, "function")?;
            let argument = match args.get("ref").and_then(Value::as_str) {
                Some(reference) => format!("window.__cmdterm.el({})", json!(reference)),
                None => String::new(),
            };
            let value = page.eval(&format!("({function})({argument})"))?;
            let pretty = serde_json::to_string_pretty(&value).unwrap_or_default();
            page.report(&format!("Result:\n```json\n{pretty}\n```"), false)
        }
        "browser_take_screenshot" => {
            page.guard()?;
            let png = browser.driver.screenshot(&tab)?;
            let mut content = page.report("Screenshot of the viewport.", false)?;
            content.push(json!({
                "type": "image",
                "data": base64::engine::general_purpose::STANDARD.encode(png),
                "mimeType": "image/png",
            }));
            Ok(content)
        }
        "browser_console_messages" => {
            page.guard()?;
            let messages = browser.sink.console_of(&tab);
            let lines: Vec<String> = messages
                .iter()
                .map(|m| format!("[{}] {}", m.level, m.text))
                .collect();
            let body = if lines.is_empty() {
                "No console messages.".to_owned()
            } else {
                lines.join("\n")
            };
            page.report(&body, false)
        }
        "browser_wait_for" => {
            page.guard()?;
            let start = Instant::now();
            if let Some(seconds) = args.get("time").and_then(Value::as_f64) {
                std::thread::sleep(Duration::from_secs_f64(seconds.clamp(0.0, 60.0)));
            }
            let appear = args.get("text").and_then(Value::as_str);
            let gone = args.get("textGone").and_then(Value::as_str);
            if appear.is_some() || gone.is_some() {
                let check = format!(
                    "(() => {{ const t = document.body ? document.body.innerText : ''; \
                     return ({} === null || t.includes({})) && ({} === null || !t.includes({})); }})()",
                    json!(appear),
                    json!(appear),
                    json!(gone),
                    json!(gone)
                );
                loop {
                    if page.eval(&check).ok() == Some(json!(true)) {
                        break;
                    }
                    if start.elapsed() > MAX_WAIT {
                        return Err(format!("waited {}s in vain", MAX_WAIT.as_secs()));
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
            page.report("Done waiting.", true)
        }
        "browser_resize" => {
            let size = |key| {
                args.get(key)
                    .and_then(Value::as_f64)
                    .filter(|v| (100.0..=8192.0).contains(v))
                    .map(|v| v as u32)
                    .ok_or_else(|| format!("`{key}` must be between 100 and 8192"))
            };
            let (width, height) = (size("width")?, size("height")?);
            if let Some(session) = lock(&hub.sessions).get_mut(&id) {
                session.viewport = Some((width, height));
            }
            browser.driver.set_viewport(&tab, width, height)?;
            page.report(&format!("The viewport is {width}x{height}."), false)
        }
        "browser_tabs" => tabs(&page, &session, args),
        _ => Err(format!("no tool {name}")),
    }
}

/// A caller with a proxy reaches the `localhost` of its own sandbox through
/// it, so it may only use sessions created with that same proxy. Others
/// would send `localhost` to the host or to another sandbox.
fn session_allowed(caller: &Caller, session: &Session) -> Result<(), String> {
    // A proxied session reaches its sandbox's loopback, so only the host and
    // terminals of the same worktree may drive it.
    let foreign = session.proxy.is_some() && !caller.host;
    if (caller.proxy.is_some() || foreign) && session.proxy != caller.proxy {
        return Err(
            "this session belongs to another worktree; start your own with browser_session_new"
                .into(),
        );
    }
    Ok(())
}

fn session_new(hub: &'static Hub, args: &Args, caller: &Caller) -> Result<Content, String> {
    let url = match args.get("url").and_then(Value::as_str) {
        Some(url) => check_url(caller, url)?,
        None => "about:blank".to_owned(),
    };
    if lock(&hub.sessions).len() >= MAX_SESSIONS {
        return Err(format!(
            "{MAX_SESSIONS} sessions are open; close one with browser_session_close"
        ));
    }
    let browser = hub.browser()?;
    let context = browser.driver.create_context(caller.proxy.as_deref())?;
    let tab = match browser.driver.new_tab(Some(&context), "about:blank") {
        Ok(tab) => tab,
        Err(error) => {
            let _ = browser.driver.remove_context(&context);
            return Err(error);
        }
    };
    let _ = browser
        .driver
        .set_viewport(&tab, DEFAULT_VIEWPORT.0, DEFAULT_VIEWPORT.1);
    let id = new_id();
    lock(&hub.sessions).insert(
        id.clone(),
        Session {
            context,
            tab: Some(tab.clone()),
            viewport: None,
            created: Instant::now(),
            proxy: caller.proxy.clone(),
        },
    );
    hub.show_tab(&tab);
    if url != "about:blank" {
        browser.driver.navigate(&tab, &url)?;
    }
    let page = Page {
        hub,
        browser: &browser,
        tab: &tab,
        session: &id,
        caller,
    };
    page.report(
        &format!("Started session {id}. Pass \"session\": \"{id}\" to the other browser tools."),
        url != "about:blank",
    )
}

fn sessions(hub: &'static Hub) -> Result<Content, String> {
    let Some(browser) = hub.running() else {
        return Ok(text("No sessions; the browser is not running.".into()));
    };
    let tabs = browser.tabs()?;
    let mut list: Vec<(String, Session)> = lock(&hub.sessions)
        .iter()
        .map(|(id, s)| (id.clone(), s.clone()))
        .collect();
    if list.is_empty() {
        return Ok(text("No sessions.".into()));
    }
    list.sort_by_key(|(_, s)| s.created);
    let mut out = String::new();
    for (id, session) in list {
        out.push_str(&format!("- {id}\n"));
        for tab in tabs
            .iter()
            .filter(|t| t.context.as_ref() == Some(&session.context))
        {
            let mark = if session.tab.as_ref() == Some(&tab.id) {
                " (current)"
            } else {
                ""
            };
            out.push_str(&format!("  - {} {}{mark}\n", tab.url, json!(tab.title)));
        }
    }
    Ok(text(out))
}

fn tabs(page: &Page<'_>, session: &Session, args: &Args) -> Result<Content, String> {
    let driver = &page.browser.driver;
    let mine = |browser: &Browser| -> Result<Vec<crate::devtools::TabInfo>, String> {
        Ok(browser
            .tabs()?
            .into_iter()
            .filter(|t| t.context.as_ref() == Some(&session.context))
            .collect())
    };
    let list = mine(page.browser)?;
    let index = || {
        args.get("index")
            .and_then(Value::as_u64)
            .map(|i| i as usize)
            .ok_or("`index` is missing".to_owned())
            .and_then(|i| {
                list.get(i)
                    .map(|t| t.id.clone())
                    .ok_or(format!("no tab {i}; the session has {}", list.len()))
            })
    };
    let select = |tab: &str| {
        if let Some(s) = lock(&page.hub.sessions).get_mut(page.session) {
            s.tab = Some(tab.to_owned());
        }
        page.hub.show_tab(tab);
        let _ = driver.activate(tab);
    };
    match string(args, "action")? {
        "list" => {}
        "new" => {
            let url = match args.get("url").and_then(Value::as_str) {
                Some(url) => check_url(page.caller, url)?,
                None => "about:blank".to_owned(),
            };
            let tab = driver.new_tab(Some(&session.context), "about:blank")?;
            let (w, h) = session.viewport.unwrap_or(DEFAULT_VIEWPORT);
            let _ = driver.set_viewport(&tab, w, h);
            select(&tab);
            if url != "about:blank" {
                driver.navigate(&tab, &url)?;
            }
        }
        "select" => select(&index()?),
        "close" => {
            let tab = match args.get("index") {
                Some(_) => index()?,
                None => page.tab.to_owned(),
            };
            driver.close_tab(&tab)?;
        }
        other => {
            return Err(format!(
                "unknown action {other}; use list, new, select or close"
            ));
        }
    }
    let list = mine(page.browser)?;
    let current = lock(&page.hub.sessions)
        .get(page.session)
        .and_then(|s| s.tab.clone());
    let mut out = format!("Tabs of session {}:\n", page.session);
    for (i, tab) in list.iter().enumerate() {
        let mark = if current.as_ref() == Some(&tab.id) {
            " (current)"
        } else {
            ""
        };
        out.push_str(&format!("- {i}: {} {}{mark}\n", tab.url, json!(tab.title)));
    }
    Ok(text(out))
}

/// The session's tab: the one it acts on if still open, else another of
/// its context, else a new one.
fn tab_of(hub: &Hub, browser: &Browser, id: &str, session: &Session) -> Result<String, String> {
    let tabs = browser.tabs()?;
    let mine: Vec<&crate::devtools::TabInfo> = tabs
        .iter()
        .filter(|t| t.context.as_ref() == Some(&session.context))
        .collect();
    let tab = match &session.tab {
        Some(tab) if mine.iter().any(|t| &t.id == tab) => return Ok(tab.clone()),
        _ => match mine.last() {
            Some(tab) => tab.id.clone(),
            None => {
                let tab = browser
                    .driver
                    .new_tab(Some(&session.context), "about:blank")?;
                let (w, h) = session.viewport.unwrap_or(DEFAULT_VIEWPORT);
                let _ = browser.driver.set_viewport(&tab, w, h);
                tab
            }
        },
    };
    if let Some(s) = lock(&hub.sessions).get_mut(id) {
        s.tab = Some(tab.clone());
    }
    Ok(tab)
}

fn new_id() -> String {
    let mut bytes = [0u8; 6];
    let _ = getrandom::fill(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("s{hex}")
}

/// What one tool call acts on.
struct Page<'a> {
    hub: &'a Hub,
    browser: &'a Browser,
    tab: &'a str,
    session: &'a str,
    caller: &'a Caller,
}

impl Page<'_> {
    fn url(&self) -> String {
        (self.browser.tabs().unwrap_or_default().into_iter())
            .find(|t| t.id == self.tab)
            .map(|t| t.url)
            .unwrap_or_default()
    }

    /// Refuses to read a page the caller may not open.
    fn guard(&self) -> Result<(), String> {
        let url = self.url();
        if url.is_empty() {
            return Ok(());
        }
        check_url(self.caller, &url).map(drop)
    }

    /// Evaluates `body` after installing `page.js`, as JSON.
    fn eval(&self, body: &str) -> Result<Value, String> {
        self.guard()?;
        self.browser.driver.evaluate(
            self.tab,
            &format!(
                "(async () => {{ {PAGE}\nconst value = await ({body}); \
                 return JSON.stringify(value === undefined ? null : value); }})()"
            ),
        )
    }

    /// The center of `reference`, scrolled into view.
    fn point(&self, reference: &str) -> Result<(f64, f64), String> {
        let point = self.eval(&format!("window.__cmdterm.point({})", json!(reference)))?;
        match (point["x"].as_f64(), point["y"].as_f64()) {
            (Some(x), Some(y)) => Ok((x, y)),
            _ => Err(format!("{reference} has no position")),
        }
    }

    fn mouse(
        &self,
        kind: MouseKind,
        x: f64,
        y: f64,
        button: Button,
        clicks: u32,
    ) -> Result<(), String> {
        self.browser.driver.mouse(
            self.tab,
            &Mouse {
                kind,
                x,
                y,
                button,
                clicks,
                modifiers: 0,
            },
        )
    }

    fn click(&self, (x, y): (f64, f64), button: Button, clicks: u32) -> Result<(), String> {
        self.mouse(MouseKind::Move, x, y, Button::None, 0)?;
        for count in 1..=clicks {
            self.mouse(MouseKind::Down, x, y, button, count)?;
            self.mouse(MouseKind::Up, x, y, button, count)?;
        }
        Ok(())
    }

    fn press(&self, spec: &str) -> Result<(), String> {
        let (held, key) = keys::chord(spec)?;
        let driver = &self.browser.driver;
        for modifier in &held {
            driver.key(self.tab, modifier, true)?;
        }
        driver.key(self.tab, &key, true)?;
        driver.key(self.tab, &key, false)?;
        for modifier in held.iter().rev() {
            driver.key(self.tab, modifier, false)?;
        }
        Ok(())
    }

    /// Waits until a navigation an action started finished loading.
    fn settle(&self) {
        std::thread::sleep(Duration::from_millis(150));
        let start = Instant::now();
        while start.elapsed() < SETTLE {
            let state = self.browser.driver.evaluate(
                self.tab,
                "Promise.resolve(JSON.stringify(document.readyState))",
            );
            if state.ok() == Some(json!("complete")) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// `message`, the page's address and title, and its snapshot.
    fn report(&self, message: &str, snapshot: bool) -> Result<Content, String> {
        let mut out = String::new();
        if !message.is_empty() {
            out.push_str(message);
            out.push_str("\n\n");
        }
        let url = self.url();
        let title = self.browser.driver.title(self.tab).unwrap_or_default();
        out.push_str(&format!(
            "### Page\n- Session: {}\n- URL: {url}\n- Title: {title}\n",
            self.session
        ));
        if snapshot {
            let tree = self.eval("window.__cmdterm.snapshot()")?;
            out.push_str(&format!(
                "\n### Snapshot\n```yaml\n{}\n```\n",
                tree.as_str().unwrap_or_default()
            ));
        }
        Ok(text(out))
    }
}

/// The host of `url`, without user info, port or brackets.
fn host_of(url: &str) -> Option<&str> {
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    if let Some(v6) = authority.strip_prefix('[') {
        return v6.split(']').next();
    }
    Some(authority.split(':').next().unwrap_or(authority))
}

/// `url` with a scheme, when the caller may open it.
fn check_url(caller: &Caller, url: &str) -> Result<String, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("`url` is empty".into());
    }
    let lower = url.to_ascii_lowercase();
    let schemed = lower.contains("://")
        || [
            "about:",
            "data:",
            "blob:",
            "javascript:",
            "view-source:",
            "chrome:",
            "file:",
        ]
        .iter()
        .any(|scheme| lower.starts_with(scheme));
    let url = if schemed {
        url.to_owned()
    } else if url.starts_with("localhost") || url.starts_with("127.") || url.starts_with("[::1]") {
        format!("http://{url}")
    } else {
        format!("https://{url}")
    };
    let scheme = url.split(':').next().unwrap_or("").to_ascii_lowercase();
    let web = matches!(scheme.as_str(), "http" | "https");
    let harmless = url == "about:blank" || matches!(scheme.as_str(), "data" | "blob");
    if !caller.host && !web && !harmless {
        return Err(format!(
            "{url}: only terminals on this machine outside sandboxes may open {scheme}: pages, \
             since the browser runs outside"
        ));
    }
    if web && let Some(allowed) = &caller.allowed_hosts {
        let host = host_of(&url).unwrap_or_default();
        let sandbox_local = caller.proxy.is_some()
            && matches!(
                host.to_ascii_lowercase().as_str(),
                "localhost" | "127.0.0.1" | "[::1]" | "::1"
            );
        if !sandbox_local && !cmdterm_env::host_allowed(allowed, host) {
            return Err(format!(
                "{host} is not among the hosts this sandbox may reach; ask the user to allow it \
                 (sandbox.allowed_hosts)"
            ));
        }
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rpc(method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params })
    }

    #[test]
    fn initialize_echoes_a_known_version_and_lists_tools_with_sessions() {
        let caller = Caller::default();
        let answer = handle(
            crate::devtools::hub(),
            &rpc("initialize", json!({ "protocolVersion": "2025-03-26" })),
            &caller,
        )
        .unwrap();
        assert_eq!(answer["id"], 7);
        assert_eq!(answer["result"]["protocolVersion"], "2025-03-26");
        let notification = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(handle(crate::devtools::hub(), &notification, &caller).is_none());
        let list = handle(
            crate::devtools::hub(),
            &rpc("tools/list", json!({})),
            &caller,
        )
        .unwrap();
        let tools = list["result"]["tools"].as_array().unwrap();
        let navigate = tools
            .iter()
            .find(|t| t["name"] == "browser_navigate")
            .unwrap();
        assert_eq!(
            navigate["inputSchema"]["required"],
            json!(["session", "url"])
        );
        let new = tools
            .iter()
            .find(|t| t["name"] == "browser_session_new")
            .unwrap();
        assert_eq!(new["inputSchema"]["required"], json!([]));
    }

    #[test]
    fn calls_without_a_session_or_from_refused_terminals_fail_as_tool_errors() {
        let call = rpc(
            "tools/call",
            json!({ "name": "browser_snapshot", "arguments": {} }),
        );
        let answer = handle(crate::devtools::hub(), &call, &Caller::default()).unwrap();
        assert_eq!(answer["result"]["isError"], true);
        let refused = Caller {
            refused: Some("not here".into()),
            ..Caller::default()
        };
        let answer = handle(crate::devtools::hub(), &call, &refused).unwrap();
        assert_eq!(answer["result"]["content"][0]["text"], "not here");
        let unknown = handle(
            crate::devtools::hub(),
            &rpc("resources/list", json!({})),
            &refused,
        )
        .unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
    }

    #[test]
    fn urls_get_a_scheme_and_respect_the_caller() {
        let host = Caller {
            host: true,
            ..Caller::default()
        };
        assert_eq!(
            check_url(&host, "example.com").unwrap(),
            "https://example.com"
        );
        assert_eq!(
            check_url(&host, "localhost:3000").unwrap(),
            "http://localhost:3000"
        );
        assert!(check_url(&host, "file:///etc/hosts").is_ok());
        let sandbox = Caller {
            host: false,
            allowed_hosts: Some(vec!["*.example.com".into()]),
            ..Caller::default()
        };
        assert!(check_url(&sandbox, "file:///etc/hosts").is_err());
        assert!(check_url(&sandbox, "chrome://settings").is_err());
        assert!(check_url(&sandbox, "about:blank").is_ok());
        assert!(check_url(&sandbox, "https://docs.example.com/a").is_ok());
        assert!(check_url(&sandbox, "https://user@evil.org:8080/").is_err());
        assert_eq!(host_of("http://[::1]:80/x"), Some("::1"));
    }

    #[test]
    fn a_sandbox_with_a_proxy_may_open_its_own_localhost_only() {
        let plain = Caller {
            allowed_hosts: Some(vec!["*.example.com".into()]),
            ..Caller::default()
        };
        assert!(check_url(&plain, "localhost:3000").is_err());
        let proxied = Caller {
            proxy: Some("http://127.0.0.1:9".into()),
            ..plain
        };
        assert_eq!(
            check_url(&proxied, "localhost:3000").unwrap(),
            "http://localhost:3000"
        );
        assert!(check_url(&proxied, "http://127.0.0.1:80/").is_ok());
        assert!(check_url(&proxied, "http://[::1]:80/").is_ok());
        assert!(check_url(&proxied, "https://evil.org").is_err());
        assert!(check_url(&proxied, "http://localhost.evil.org").is_err());
    }

    #[test]
    fn a_proxied_caller_only_uses_sessions_made_with_its_proxy() {
        let session = |proxy: Option<&str>| Session {
            context: "c".into(),
            tab: None,
            viewport: None,
            created: Instant::now(),
            proxy: proxy.map(str::to_owned),
        };
        let mine = Caller {
            allowed_hosts: Some(vec![]),
            proxy: Some("http://127.0.0.1:9".into()),
            ..Caller::default()
        };
        assert!(session_allowed(&mine, &session(Some("http://127.0.0.1:9"))).is_ok());
        assert!(session_allowed(&mine, &session(None)).is_err());
        assert!(session_allowed(&mine, &session(Some("http://127.0.0.1:10"))).is_err());
        // Callers without a proxy keep unproxied sessions.
        assert!(session_allowed(&Caller::default(), &session(None)).is_ok());
        // A container or a sandbox without a hub cannot drive a proxied
        // session; the host can.
        assert!(session_allowed(&Caller::default(), &session(Some("http://127.0.0.1:9"))).is_err());
        let host = Caller {
            host: true,
            ..Caller::default()
        };
        assert!(session_allowed(&host, &session(Some("http://127.0.0.1:9"))).is_ok());
    }
}
