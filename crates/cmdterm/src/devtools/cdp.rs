//! Chrome over the DevTools protocol, on the pipe of
//! `--remote-debugging-pipe`: the browser reads commands from its fd 3 and
//! writes answers and events to its fd 4, each message ended by a NUL byte.
//! No port is opened, so no other process can drive this browser.
//!
//! One browser-level connection carries every tab: `Target.attachToTarget`
//! with `flatten` gives each tab a session id that later commands name.
//!
//! A context whose proxy takes a password (a sandbox's port hub) gets it
//! through `Fetch`, since Chrome takes no credentials in a proxy setting:
//! once such a context exists, `Fetch.enable` on the browser connection
//! pauses every request, the reader continues each, and it answers a
//! `407` whose origin is that proxy with its password. Chrome raises auth
//! challenges only for paused requests, so the pattern cannot be narrower.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};

use crate::devtools::driver::{
    Answered, Button, Console, Driver, KeyStroke, Kind, Mouse, MouseKind, Result, Sink, TabInfo,
    split_proxy,
};
use crate::devtools::rpc::Rpc;

const TIMEOUT: Duration = Duration::from_secs(30);
/// Longest wait for a page to finish loading after it committed.
const LOAD: Duration = Duration::from_secs(20);

pub struct Cdp {
    rpc: Arc<Rpc>,
    /// Session id per target id.
    sessions: Arc<Mutex<HashMap<String, String>>>,
    /// Held while one thread attaches, so two never attach to one tab.
    attaching: Mutex<()>,
    /// The target whose frames are streamed.
    streaming: Mutex<Option<String>>,
    auth: Arc<Mutex<ProxyAuth>>,
    /// Set once `Fetch.enable` went out.
    fetch: Mutex<bool>,
}

/// The passwords of the proxies of contexts, see the module docs.
#[derive(Default)]
struct ProxyAuth {
    /// Per browser context: the proxy's origin, user name and password.
    contexts: HashMap<String, (String, String, String)>,
    answered: Answered,
}

impl Cdp {
    /// Talks to the browser on `input` (its fd 3) and `output` (its fd 4).
    pub fn start(
        input: impl Write + Send + 'static,
        output: impl Read + Send + 'static,
        sink: Arc<Sink>,
    ) -> Result<Self> {
        let input = Mutex::new(input);
        let rpc = Arc::new(Rpc::new(Box::new(move |text| {
            let mut input = input.lock().unwrap_or_else(|p| p.into_inner());
            input.write_all(text.as_bytes())?;
            input.write_all(b"\0")?;
            input.flush()
        })));
        let sessions: Arc<Mutex<HashMap<String, String>>> = Arc::default();
        let auth: Arc<Mutex<ProxyAuth>> = Arc::default();
        let (reader_rpc, reader_sessions) = (rpc.clone(), sessions.clone());
        let reader_auth = auth.clone();
        thread::Builder::new()
            .name("debug-browser-cdp".into())
            .spawn(move || read(output, &reader_rpc, &reader_sessions, &reader_auth, &sink))
            .map_err(|error| error.to_string())?;
        let cdp = Self {
            rpc,
            sessions,
            attaching: Mutex::new(()),
            streaming: Mutex::new(None),
            auth,
            fetch: Mutex::new(false),
        };
        cdp.call(
            "Target.setDiscoverTargets",
            json!({ "discover": true }),
            None,
        )?;
        Ok(cdp)
    }

    fn call(&self, method: &str, params: Value, session: Option<&str>) -> Result<Value> {
        self.rpc.call(method, params, session, TIMEOUT)
    }

    /// The session of `tab`, attaching to it first if needed.
    fn session(&self, tab: &str) -> Result<String> {
        if let Some(session) = lock(&self.sessions).get(tab) {
            return Ok(session.clone());
        }
        let _attaching = self.attaching.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(session) = lock(&self.sessions).get(tab) {
            return Ok(session.clone());
        }
        let attached = self.call(
            "Target.attachToTarget",
            json!({ "targetId": tab, "flatten": true }),
            None,
        )?;
        let session = attached["sessionId"]
            .as_str()
            .ok_or("Chrome gave no session")?
            .to_owned();
        lock(&self.sessions).insert(tab.to_owned(), session.clone());
        for method in ["Page.enable", "Runtime.enable"] {
            self.call(method, json!({}), Some(&session))?;
        }
        Ok(session)
    }

    fn on(&self, tab: &str, method: &str, params: Value) -> Result<Value> {
        let session = self.session(tab)?;
        self.call(method, params, Some(&session))
    }

    fn ready_state(&self, tab: &str) -> Option<String> {
        let result = self
            .on(
                tab,
                "Runtime.evaluate",
                json!({ "expression": "document.readyState", "returnByValue": true }),
            )
            .ok()?;
        result["result"]["value"].as_str().map(str::to_owned)
    }

    fn wait_loaded(&self, tab: &str) {
        let start = Instant::now();
        while start.elapsed() < LOAD {
            if self.ready_state(tab).as_deref() == Some("complete") {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

/// The reader thread: splits the pipe at NUL bytes and routes each message.
fn read(
    output: impl Read,
    rpc: &Rpc,
    sessions: &Mutex<HashMap<String, String>>,
    auth: &Mutex<ProxyAuth>,
    sink: &Sink,
) {
    let mut output = BufReader::new(output);
    let mut message = Vec::new();
    loop {
        message.clear();
        match output.read_until(0, &mut message) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if message.last() == Some(&0) {
            message.pop();
        }
        let Ok(value) = serde_json::from_slice::<Value>(&message) else {
            continue;
        };
        if let Some(event) = rpc.dispatch(value) {
            on_event(&event, rpc, sessions, auth, sink);
        }
    }
    rpc.close();
    sink.tabs_changed();
}

fn on_event(
    event: &Value,
    rpc: &Rpc,
    sessions: &Mutex<HashMap<String, String>>,
    auth: &Mutex<ProxyAuth>,
    sink: &Sink,
) {
    let params = &event["params"];
    let session = event["sessionId"].as_str();
    let tab_of = |session: &str| {
        lock(sessions)
            .iter()
            .find(|(_, s)| s.as_str() == session)
            .map(|(tab, _)| tab.clone())
    };
    match event["method"].as_str().unwrap_or_default() {
        "Target.targetCreated" | "Target.targetInfoChanged" => sink.tabs_changed(),
        "Fetch.requestPaused" => rpc.notify(
            "Fetch.continueRequest",
            json!({ "requestId": params["requestId"] }),
            session,
        ),
        "Fetch.authRequired" => {
            let response = auth_answer(params, &mut lock(auth));
            rpc.notify(
                "Fetch.continueWithAuth",
                json!({ "requestId": params["requestId"], "authChallengeResponse": response }),
                session,
            );
        }
        "Target.targetDestroyed" => {
            if let Some(tab) = params["targetId"].as_str() {
                lock(sessions).remove(tab);
            }
            sink.tabs_changed();
        }
        "Target.detachedFromTarget" => {
            if let Some(gone) = params["sessionId"].as_str() {
                lock(sessions).retain(|_, s| s != gone);
            }
        }
        "Page.screencastFrame" => {
            rpc.notify(
                "Page.screencastFrameAck",
                json!({ "sessionId": params["sessionId"] }),
                session,
            );
            let Some(tab) = session.and_then(tab_of) else {
                return;
            };
            if let Some(data) = params["data"].as_str()
                && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data)
            {
                sink.frame(tab, bytes);
            }
        }
        "Runtime.consoleAPICalled" => {
            let Some(tab) = session.and_then(tab_of) else {
                return;
            };
            let text = (params["args"].as_array().into_iter().flatten())
                .map(remote_text)
                .collect::<Vec<_>>()
                .join(" ");
            let level = match params["type"].as_str().unwrap_or("log") {
                "warning" => "warn",
                other => other,
            };
            sink.console(Console {
                tab,
                level: level.to_owned(),
                text,
            });
        }
        "Runtime.exceptionThrown" => {
            let Some(tab) = session.and_then(tab_of) else {
                return;
            };
            let details = &params["exceptionDetails"];
            let text = details["exception"]["description"]
                .as_str()
                .or(details["text"].as_str())
                .unwrap_or("uncaught error");
            sink.console(Console {
                tab,
                level: "error".into(),
                text: text.to_owned(),
            });
        }
        // Dialogs stop the page until someone answers; accept them and
        // say so in the console.
        "Page.javascriptDialogOpening" => {
            rpc.notify(
                "Page.handleJavaScriptDialog",
                json!({ "accept": true }),
                session,
            );
            if let Some(tab) = session.and_then(tab_of) {
                sink.console(Console {
                    tab,
                    level: "info".into(),
                    text: format!(
                        "{} dialog accepted: {}",
                        params["type"].as_str().unwrap_or("a"),
                        params["message"].as_str().unwrap_or_default()
                    ),
                });
            }
        }
        _ => {}
    }
}

/// The `authChallengeResponse` for a `Fetch.authRequired`: the password of
/// a context's proxy when the challenge comes from that proxy, the first
/// time for the request. A site's challenge, or another proxy's, gets
/// Chrome's own handling.
fn auth_answer(params: &Value, auth: &mut ProxyAuth) -> Value {
    let challenge = &params["authChallenge"];
    let origin = challenge["origin"].as_str().unwrap_or_default();
    let origin = origin.trim_end_matches('/');
    let proxy = (challenge["source"] == "Proxy")
        .then(|| auth.contexts.values().find(|(o, _, _)| o == origin))
        .flatten()
        .map(|(_, user, password)| (user.clone(), password.clone()));
    let request = params["requestId"].as_str().unwrap_or_default();
    match proxy {
        Some((user, password)) if auth.answered.first(request) => json!({
            "response": "ProvideCredentials",
            "username": user,
            "password": password,
        }),
        Some(_) => json!({ "response": "CancelAuth" }),
        None => json!({ "response": "Default" }),
    }
}

/// A console argument as text: its value, else its description.
fn remote_text(object: &Value) -> String {
    match &object["value"] {
        Value::String(text) => text.clone(),
        Value::Null => object["description"]
            .as_str()
            .or(object["type"].as_str())
            .unwrap_or_default()
            .to_owned(),
        value => value.to_string(),
    }
}

fn button(button: Button) -> &'static str {
    match button {
        Button::None => "none",
        Button::Left => "left",
        Button::Middle => "middle",
        Button::Right => "right",
    }
}

impl Driver for Cdp {
    fn kind(&self) -> Kind {
        Kind::Chrome
    }

    fn alive(&self) -> bool {
        !self.rpc.is_closed()
    }

    fn create_context(&self, proxy: Option<&str>) -> Result<String> {
        let (server, credentials) = match proxy.map(split_proxy) {
            Some((server, credentials)) => (Some(server), credentials),
            None => (None, None),
        };
        let params = context_params(server.as_deref());
        let created = self.call("Target.createBrowserContext", params, None)?;
        let context = created["browserContextId"]
            .as_str()
            .map(str::to_owned)
            .ok_or("Chrome gave no browser context")?;
        if let (Some(server), Some((user, password))) = (server, credentials) {
            lock(&self.auth)
                .contexts
                .insert(context.clone(), (server, user, password));
            let mut fetch = lock(&self.fetch);
            if !*fetch {
                let params = json!({
                    "handleAuthRequests": true,
                    "patterns": [{ "urlPattern": "*" }],
                });
                if let Err(error) = self.call("Fetch.enable", params, None) {
                    let _ = self.remove_context(&context);
                    return Err(error);
                }
                *fetch = true;
            }
        }
        Ok(context)
    }

    fn remove_context(&self, context: &str) -> Result<()> {
        lock(&self.auth).contexts.remove(context);
        self.call(
            "Target.disposeBrowserContext",
            json!({ "browserContextId": context }),
            None,
        )
        .map(drop)
    }

    fn tabs(&self) -> Result<Vec<TabInfo>> {
        let targets = self.call("Target.getTargets", json!({}), None)?;
        Ok((targets["targetInfos"].as_array().into_iter().flatten())
            .filter(|info| info["type"] == "page")
            .map(|info| TabInfo {
                id: info["targetId"].as_str().unwrap_or_default().to_owned(),
                url: info["url"].as_str().unwrap_or_default().to_owned(),
                title: info["title"].as_str().unwrap_or_default().to_owned(),
                context: info["browserContextId"].as_str().map(str::to_owned),
            })
            .collect())
    }

    fn title(&self, tab: &str) -> Result<String> {
        let info = self.call("Target.getTargetInfo", json!({ "targetId": tab }), None)?;
        Ok(info["targetInfo"]["title"]
            .as_str()
            .unwrap_or_default()
            .to_owned())
    }

    fn new_tab(&self, context: Option<&str>, url: &str) -> Result<String> {
        let mut params = json!({ "url": url });
        if let Some(context) = context {
            params["browserContextId"] = json!(context);
        }
        let created = self.call("Target.createTarget", params, None)?;
        let tab = created["targetId"]
            .as_str()
            .ok_or("Chrome opened no tab")?
            .to_owned();
        self.session(&tab)?;
        self.wait_loaded(&tab);
        Ok(tab)
    }

    fn close_tab(&self, tab: &str) -> Result<()> {
        self.call("Target.closeTarget", json!({ "targetId": tab }), None)
            .map(drop)
    }

    fn activate(&self, tab: &str) -> Result<()> {
        self.session(tab)?;
        self.call("Target.activateTarget", json!({ "targetId": tab }), None)
            .map(drop)
    }

    fn navigate(&self, tab: &str, url: &str) -> Result<()> {
        let result = self.on(tab, "Page.navigate", json!({ "url": url }))?;
        if let Some(error) = result["errorText"].as_str().filter(|e| !e.is_empty()) {
            return Err(format!("{url}: {error}"));
        }
        self.wait_loaded(tab);
        Ok(())
    }

    fn evaluate(&self, tab: &str, expression: &str) -> Result<Value> {
        let result = self.on(
            tab,
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "awaitPromise": true,
                "returnByValue": true,
                "userGesture": true,
            }),
        )?;
        if let Some(details) = result.get("exceptionDetails") {
            let text = details["exception"]["description"]
                .as_str()
                .or(details["text"].as_str())
                .unwrap_or("the script threw");
            return Err(text.to_owned());
        }
        match &result["result"]["value"] {
            Value::String(json) => serde_json::from_str(json).map_err(|e| e.to_string()),
            _ => Ok(Value::Null),
        }
    }

    fn screenshot(&self, tab: &str) -> Result<Vec<u8>> {
        let shot = self.on(tab, "Page.captureScreenshot", json!({ "format": "png" }))?;
        let data = shot["data"].as_str().ok_or("Chrome sent no picture")?;
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| e.to_string())
    }

    fn set_viewport(&self, tab: &str, width: u32, height: u32) -> Result<()> {
        self.on(
            tab,
            "Emulation.setDeviceMetricsOverride",
            json!({ "width": width, "height": height, "deviceScaleFactor": 1, "mobile": false }),
        )
        .map(drop)
    }

    fn mouse(&self, tab: &str, mouse: &Mouse) -> Result<()> {
        let (kind, dx, dy) = match mouse.kind {
            MouseKind::Move => ("mouseMoved", 0.0, 0.0),
            MouseKind::Down => ("mousePressed", 0.0, 0.0),
            MouseKind::Up => ("mouseReleased", 0.0, 0.0),
            MouseKind::Wheel { dx, dy } => ("mouseWheel", dx, dy),
        };
        let mut params = json!({
            "type": kind,
            "x": mouse.x,
            "y": mouse.y,
            "button": button(mouse.button),
            "clickCount": mouse.clicks,
            "modifiers": mouse.modifiers,
        });
        if matches!(mouse.kind, MouseKind::Wheel { .. }) {
            params["deltaX"] = json!(dx);
            params["deltaY"] = json!(dy);
        }
        self.on(tab, "Input.dispatchMouseEvent", params).map(drop)
    }

    fn key(&self, tab: &str, key: &KeyStroke, down: bool) -> Result<()> {
        let kind = match (down, &key.text) {
            (false, _) => "keyUp",
            (true, Some(_)) => "keyDown",
            (true, None) => "rawKeyDown",
        };
        let mut params = json!({
            "type": kind,
            "key": key.key,
            "code": key.code,
            "windowsVirtualKeyCode": key.key_code,
            "nativeVirtualKeyCode": key.key_code,
            "modifiers": key.modifiers,
        });
        if down && let Some(text) = &key.text {
            params["text"] = json!(text);
            params["unmodifiedText"] = json!(text);
        }
        self.on(tab, "Input.dispatchKeyEvent", params).map(drop)
    }

    fn insert_text(&self, tab: &str, text: &str) -> Result<()> {
        self.on(tab, "Input.insertText", json!({ "text": text }))
            .map(drop)
    }

    fn stream(&self, tab: Option<&str>) -> bool {
        let mut streaming = lock(&self.streaming);
        if streaming.as_deref() == tab {
            return true;
        }
        if let Some(old) = streaming.take()
            && let Some(session) = lock(&self.sessions).get(&old).cloned()
        {
            let _ = self.call("Page.stopScreencast", json!({}), Some(&session));
        }
        if let Some(tab) = tab
            && self
                .on(
                    tab,
                    "Page.startScreencast",
                    json!({ "format": "jpeg", "quality": 80, "everyNthFrame": 1 }),
                )
                .is_ok()
        {
            *streaming = Some(tab.to_owned());
        }
        true
    }

    fn shutdown(&self) {
        let _ = self
            .rpc
            .call("Browser.close", json!({}), None, Duration::from_secs(3));
        self.rpc.close();
    }
}

/// Params of `Target.createBrowserContext`. `<-loopback>` takes loopback
/// addresses off Chrome's built-in bypass list, so `localhost` reaches the
/// proxy too.
fn context_params(proxy: Option<&str>) -> Value {
    match proxy {
        Some(proxy) => json!({ "proxyServer": proxy, "proxyBypassList": "<-loopback>" }),
        None => json!({}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_contexts_own_proxy_gets_its_password() {
        let mut auth = ProxyAuth::default();
        auth.contexts.insert(
            "c".into(),
            (
                "http://127.0.0.1:8080".into(),
                "cmdterm".into(),
                "pw".into(),
            ),
        );
        let challenge = |id: &str, source: &str, origin: &str| json!({ "requestId": id, "authChallenge": { "source": source, "origin": origin } });
        assert_eq!(
            auth_answer(&challenge("1", "Proxy", "http://127.0.0.1:8080"), &mut auth),
            json!({ "response": "ProvideCredentials", "username": "cmdterm", "password": "pw" })
        );
        // A wrong password does not loop.
        let again = auth_answer(&challenge("1", "Proxy", "http://127.0.0.1:8080"), &mut auth);
        assert_eq!(again, json!({ "response": "CancelAuth" }));
        // A site on the proxy's address, and another proxy, get nothing.
        for (source, origin) in [
            ("Server", "http://127.0.0.1:8080"),
            ("Proxy", "http://127.0.0.1:8081"),
        ] {
            let answer = auth_answer(&challenge("2", source, origin), &mut auth);
            assert_eq!(answer, json!({ "response": "Default" }));
        }
    }

    #[test]
    fn a_proxy_context_unbypasses_loopback() {
        assert_eq!(context_params(None), json!({}));
        assert_eq!(
            context_params(Some("http://127.0.0.1:8080")),
            json!({ "proxyServer": "http://127.0.0.1:8080", "proxyBypassList": "<-loopback>" })
        );
    }
}
