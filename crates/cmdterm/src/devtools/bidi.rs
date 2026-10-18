//! Firefox over WebDriver BiDi, on the WebSocket that
//! `--remote-debugging-port` opens on loopback.
//!
//! BiDi has no frame stream, so [`Driver::stream`] says no and the panel
//! polls screenshots. Titles come from `document.title`, since the tree of
//! browsing contexts carries none.
//!
//! A user context whose proxy takes a password (a sandbox's port hub) gets
//! it through an intercept in the `authRequired` phase, since Firefox takes
//! no credentials in a proxy setting: a `407` in a browsing context of that
//! user context is answered with the password, every other challenge gets
//! Firefox's own handling. The reader keeps which user context each
//! browsing context belongs to from the `browsingContext` events.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};

use crate::devtools::driver::{
    Answered, Button, Console, Driver, KeyStroke, Kind, Mouse, MouseKind, Result, Sink, TabInfo,
    split_proxy,
};
use crate::devtools::rpc::Rpc;
use crate::devtools::ws;

const TIMEOUT: Duration = Duration::from_secs(30);

pub struct Bidi {
    rpc: Arc<Rpc>,
    writer: Arc<ws::Writer>,
    auth: Arc<Mutex<ProxyAuth>>,
    /// Set once the `authRequired` intercept is in place.
    intercepting: Mutex<bool>,
}

/// See the module docs.
#[derive(Default)]
struct ProxyAuth {
    /// User name and password per user context with such a proxy.
    users: HashMap<String, (String, String)>,
    /// The user context of each browsing context.
    contexts: HashMap<String, String>,
    answered: Answered,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

impl Bidi {
    /// Connects to `url`, the `ws://` address Firefox printed, and starts a
    /// session.
    pub fn connect(url: &str, sink: Arc<Sink>) -> Result<Self> {
        let url = format!("{}/session", url.trim_end_matches('/'));
        let (mut reader, writer) = ws::connect(&url).map_err(|e| format!("{url}: {e}"))?;
        let writer = Arc::new(writer);
        let send_writer = writer.clone();
        let rpc = Arc::new(Rpc::new(Box::new(move |text| send_writer.send_text(text))));
        let (reader_rpc, reader_writer) = (rpc.clone(), writer.clone());
        let auth: Arc<Mutex<ProxyAuth>> = Arc::default();
        let reader_auth = auth.clone();
        thread::Builder::new()
            .name("debug-browser-bidi".into())
            .spawn(move || {
                while let Ok(Some(text)) = reader.next(&reader_writer) {
                    let Ok(value) = serde_json::from_str::<Value>(&text) else {
                        continue;
                    };
                    if let Some(event) = reader_rpc.dispatch(value) {
                        on_event(&event, &reader_rpc, &reader_auth, &sink);
                    }
                }
                reader_rpc.close();
                sink.tabs_changed();
            })
            .map_err(|e| e.to_string())?;
        let bidi = Self {
            rpc,
            writer,
            auth,
            intercepting: Mutex::new(false),
        };
        bidi.call("session.new", json!({ "capabilities": {} }))?;
        bidi.call(
            "session.subscribe",
            json!({ "events": ["browsingContext", "log.entryAdded"] }),
        )?;
        Ok(bidi)
    }

    fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.rpc.call(method, params, None, TIMEOUT)
    }

    fn perform(&self, tab: &str, source: Value) -> Result<()> {
        self.call(
            "input.performActions",
            json!({ "context": tab, "actions": [source] }),
        )
        .map(drop)
    }
}

fn on_event(event: &Value, rpc: &Rpc, auth: &Mutex<ProxyAuth>, sink: &Sink) {
    let params = &event["params"];
    let method = event["method"].as_str().unwrap_or_default();
    match method {
        "browsingContext.contextCreated" | "browsingContext.contextDestroyed" => {
            track_context(method, params, &mut lock(auth));
            sink.tabs_changed();
        }
        "browsingContext.load" | "browsingContext.fragmentNavigated" => sink.tabs_changed(),
        "network.authRequired" if params["isBlocked"] == true => {
            let answer = auth_answer(params, &mut lock(auth));
            rpc.notify("network.continueWithAuth", answer, None);
        }
        "log.entryAdded" => {
            let tab = params["source"]["context"].as_str().unwrap_or_default();
            sink.console(Console {
                tab: tab.to_owned(),
                level: params["level"].as_str().unwrap_or("info").to_owned(),
                text: params["text"].as_str().unwrap_or_default().to_owned(),
            });
        }
        // Dialogs stop the page until someone answers; accept them and
        // say so in the console.
        "browsingContext.userPromptOpened" => {
            let tab = params["context"].as_str().unwrap_or_default();
            rpc.notify(
                "browsingContext.handleUserPrompt",
                json!({ "context": tab, "accept": true }),
                None,
            );
            sink.console(Console {
                tab: tab.to_owned(),
                level: "info".into(),
                text: format!(
                    "{} dialog accepted: {}",
                    params["type"].as_str().unwrap_or("a"),
                    params["message"].as_str().unwrap_or_default()
                ),
            });
        }
        _ => {}
    }
}

/// Keeps the user context of a created browsing context, its parent's for a
/// frame that names none, and forgets a destroyed one.
fn track_context(method: &str, params: &Value, auth: &mut ProxyAuth) {
    let Some(context) = params["context"].as_str() else {
        return;
    };
    if method == "browsingContext.contextDestroyed" {
        auth.contexts.remove(context);
        return;
    }
    let user = (params["userContext"].as_str().map(str::to_owned))
        .or_else(|| auth.contexts.get(params["parent"].as_str()?).cloned());
    if let Some(user) = user {
        auth.contexts.insert(context.to_owned(), user);
    }
}

/// The `network.continueWithAuth` params for a blocked `authRequired`: the
/// password of the proxy of the request's user context for a `407`, the
/// first time for the request. Any other challenge gets Firefox's own
/// handling.
fn auth_answer(params: &Value, auth: &mut ProxyAuth) -> Value {
    let request = params["request"]["request"].as_str().unwrap_or_default();
    let credentials = (params["response"]["status"] == 407)
        .then(|| auth.contexts.get(params["context"].as_str()?))
        .flatten()
        .and_then(|user| auth.users.get(user))
        .cloned();
    match credentials {
        Some((user, password)) if auth.answered.first(request) => json!({
            "request": request,
            "action": "provideCredentials",
            "credentials": { "type": "password", "username": user, "password": password },
        }),
        Some(_) => json!({ "request": request, "action": "cancel" }),
        None => json!({ "request": request, "action": "default" }),
    }
}

fn button(button: Button) -> u8 {
    match button {
        Button::None | Button::Left => 0,
        Button::Middle => 1,
        Button::Right => 2,
    }
}

impl Driver for Bidi {
    fn kind(&self) -> Kind {
        Kind::Firefox
    }

    fn alive(&self) -> bool {
        !self.rpc.is_closed()
    }

    fn create_context(&self, proxy: Option<&str>) -> Result<String> {
        let (server, credentials) = match proxy.map(split_proxy) {
            Some((server, credentials)) => (Some(server), credentials),
            None => (None, None),
        };
        let created = self.call(
            "browser.createUserContext",
            context_params(server.as_deref()),
        )?;
        let context = created["userContext"]
            .as_str()
            .map(str::to_owned)
            .ok_or("Firefox gave no user context")?;
        if let Some(credentials) = credentials {
            lock(&self.auth).users.insert(context.clone(), credentials);
            let mut intercepting = lock(&self.intercepting);
            if !*intercepting {
                let events = json!({ "events": ["network.authRequired"] });
                let intercept = json!({ "phases": ["authRequired"] });
                let set_up = self
                    .call("session.subscribe", events)
                    .and_then(|_| self.call("network.addIntercept", intercept));
                if let Err(error) = set_up {
                    let _ = self.remove_context(&context);
                    return Err(error);
                }
                *intercepting = true;
            }
        }
        Ok(context)
    }

    fn remove_context(&self, context: &str) -> Result<()> {
        lock(&self.auth).users.remove(context);
        self.call(
            "browser.removeUserContext",
            json!({ "userContext": context }),
        )
        .map(drop)
    }

    fn tabs(&self) -> Result<Vec<TabInfo>> {
        let tree = self.call("browsingContext.getTree", json!({ "maxDepth": 0 }))?;
        let mut tabs: Vec<TabInfo> = (tree["contexts"].as_array().into_iter().flatten())
            .map(|context| TabInfo {
                id: context["context"].as_str().unwrap_or_default().to_owned(),
                url: context["url"].as_str().unwrap_or_default().to_owned(),
                title: String::new(),
                context: context["userContext"].as_str().map(str::to_owned),
            })
            .collect();
        for tab in &mut tabs {
            tab.title = self.title(&tab.id).unwrap_or_default();
        }
        Ok(tabs)
    }

    fn title(&self, tab: &str) -> Result<String> {
        Ok(self
            .evaluate(tab, "Promise.resolve(JSON.stringify(document.title))")?
            .as_str()
            .unwrap_or_default()
            .to_owned())
    }

    fn new_tab(&self, context: Option<&str>, url: &str) -> Result<String> {
        let mut params = json!({ "type": "tab" });
        if let Some(context) = context {
            params["userContext"] = json!(context);
        }
        let created = self.call("browsingContext.create", params)?;
        let tab = created["context"]
            .as_str()
            .ok_or("Firefox opened no tab")?
            .to_owned();
        if url != "about:blank" {
            self.navigate(&tab, url)?;
        }
        Ok(tab)
    }

    fn close_tab(&self, tab: &str) -> Result<()> {
        self.call("browsingContext.close", json!({ "context": tab }))
            .map(drop)
    }

    fn activate(&self, tab: &str) -> Result<()> {
        self.call("browsingContext.activate", json!({ "context": tab }))
            .map(drop)
    }

    fn navigate(&self, tab: &str, url: &str) -> Result<()> {
        self.call(
            "browsingContext.navigate",
            json!({ "context": tab, "url": url, "wait": "complete" }),
        )
        .map(drop)
    }

    fn evaluate(&self, tab: &str, expression: &str) -> Result<Value> {
        let result = self.call(
            "script.evaluate",
            json!({
                "expression": expression,
                "target": { "context": tab },
                "awaitPromise": true,
                "resultOwnership": "none",
                "userActivation": true,
            }),
        )?;
        if result["type"] == "exception" {
            let details = &result["exceptionDetails"];
            return Err(details["text"]
                .as_str()
                .unwrap_or("the script threw")
                .to_owned());
        }
        match &result["result"]["value"] {
            Value::String(json) => serde_json::from_str(json).map_err(|e| e.to_string()),
            _ => Ok(Value::Null),
        }
    }

    fn screenshot(&self, tab: &str) -> Result<Vec<u8>> {
        let shot = self.call(
            "browsingContext.captureScreenshot",
            json!({ "context": tab }),
        )?;
        let data = shot["data"].as_str().ok_or("Firefox sent no picture")?;
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| e.to_string())
    }

    fn set_viewport(&self, tab: &str, width: u32, height: u32) -> Result<()> {
        self.call(
            "browsingContext.setViewport",
            json!({ "context": tab, "viewport": { "width": width, "height": height } }),
        )
        .map(drop)
    }

    fn mouse(&self, tab: &str, mouse: &Mouse) -> Result<()> {
        let (x, y) = (mouse.x.round() as i64, mouse.y.round() as i64);
        if let MouseKind::Wheel { dx, dy } = mouse.kind {
            return self.perform(
                tab,
                json!({
                    "type": "wheel",
                    "id": "wheel",
                    "actions": [{
                        "type": "scroll", "x": x, "y": y,
                        "deltaX": dx.round() as i64, "deltaY": dy.round() as i64,
                    }],
                }),
            );
        }
        let mut actions = vec![json!({ "type": "pointerMove", "x": x, "y": y })];
        match mouse.kind {
            MouseKind::Down => {
                actions.push(json!({ "type": "pointerDown", "button": button(mouse.button) }));
            }
            MouseKind::Up => {
                actions.push(json!({ "type": "pointerUp", "button": button(mouse.button) }));
            }
            MouseKind::Move | MouseKind::Wheel { .. } => {}
        }
        self.perform(
            tab,
            json!({
                "type": "pointer",
                "id": "mouse",
                "parameters": { "pointerType": "mouse" },
                "actions": actions,
            }),
        )
    }

    fn key(&self, tab: &str, key: &KeyStroke, down: bool) -> Result<()> {
        let kind = if down { "keyDown" } else { "keyUp" };
        self.perform(
            tab,
            json!({
                "type": "key",
                "id": "keyboard",
                "actions": [{ "type": kind, "value": key.webdriver.to_string() }],
            }),
        )
    }

    fn insert_text(&self, tab: &str, text: &str) -> Result<()> {
        let actions: Vec<Value> = text
            .chars()
            .flat_map(|c| {
                let value = if c == '\n' { '\u{E007}' } else { c }.to_string();
                [
                    json!({ "type": "keyDown", "value": value }),
                    json!({ "type": "keyUp", "value": value }),
                ]
            })
            .collect();
        self.perform(
            tab,
            json!({ "type": "key", "id": "keyboard", "actions": actions }),
        )
    }

    fn stream(&self, _: Option<&str>) -> bool {
        false
    }

    fn shutdown(&self) {
        let _ = self
            .rpc
            .call("browser.close", json!({}), None, Duration::from_secs(3));
        self.rpc.close();
        self.writer.close();
    }
}

/// Params of `browser.createUserContext`. Firefox wants `host:port`
/// without a scheme.
fn context_params(proxy: Option<&str>) -> Value {
    match proxy {
        Some(proxy) => {
            let address = proxy.split_once("://").map_or(proxy, |(_, rest)| rest);
            let address = address.trim_end_matches('/');
            json!({ "proxy": {
                "proxyType": "manual",
                "httpProxy": address,
                "sslProxy": address,
            } })
        }
        None => json!({}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_407_in_a_user_context_with_a_proxy_password_gets_it() {
        let mut auth = ProxyAuth::default();
        auth.users
            .insert("u".into(), ("cmdterm".into(), "pw".into()));
        let created = |context: &str, user: Option<&str>, parent: Option<&str>| json!({ "context": context, "userContext": user, "parent": parent });
        let create = "browsingContext.contextCreated";
        track_context(create, &created("tab", Some("u"), None), &mut auth);
        track_context(create, &created("frame", None, Some("tab")), &mut auth);
        track_context(create, &created("other", Some("default"), None), &mut auth);
        let challenge = |id: &str, context: &str, status: u16| {
            json!({ "context": context, "request": { "request": id },
                    "response": { "status": status } })
        };
        assert_eq!(
            auth_answer(&challenge("1", "frame", 407), &mut auth),
            json!({ "request": "1", "action": "provideCredentials",
                    "credentials": { "type": "password", "username": "cmdterm", "password": "pw" } })
        );
        let again = auth_answer(&challenge("1", "frame", 407), &mut auth);
        assert_eq!(again["action"], "cancel");
        // A site's 401, and a 407 elsewhere, get Firefox's own handling.
        assert_eq!(
            auth_answer(&challenge("2", "tab", 401), &mut auth)["action"],
            "default"
        );
        assert_eq!(
            auth_answer(&challenge("3", "other", 407), &mut auth)["action"],
            "default"
        );
        track_context(
            "browsingContext.contextDestroyed",
            &created("tab", None, None),
            &mut auth,
        );
        assert_eq!(
            auth_answer(&challenge("4", "tab", 407), &mut auth)["action"],
            "default"
        );
    }

    #[test]
    fn a_proxy_context_gets_a_manual_proxy_in_host_port_form() {
        assert_eq!(context_params(None), json!({}));
        assert_eq!(
            context_params(Some("http://127.0.0.1:8080")),
            json!({ "proxy": {
                "proxyType": "manual",
                "httpProxy": "127.0.0.1:8080",
                "sslProxy": "127.0.0.1:8080",
            } })
        );
    }
}
