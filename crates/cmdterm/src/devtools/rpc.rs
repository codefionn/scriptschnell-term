//! Request and response matching for the two browser protocols. Both send
//! JSON messages with an `id`; answers carry the same `id`, everything else
//! is an event. The transport's reader thread hands every message to
//! [`Rpc::dispatch`], callers wait in [`Rpc::call`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, mpsc};
use std::time::Duration;

use serde_json::{Map, Value, json};

/// Writes one message to the browser.
pub type Send = Box<dyn Fn(&str) -> std::io::Result<()> + std::marker::Send + Sync>;

pub struct Rpc {
    next: AtomicU64,
    pending: Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>,
    send: Send,
    closed: AtomicBool,
}

impl Rpc {
    pub fn new(send: Send) -> Self {
        Self {
            next: AtomicU64::new(1),
            pending: Mutex::default(),
            send,
            closed: AtomicBool::new(false),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Sends `method` and waits for its answer. `session` is the CDP session
    /// the command goes to; BiDi has none.
    pub fn call(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
        timeout: Duration,
    ) -> Result<Value, String> {
        if self.is_closed() {
            return Err("the browser closed".into());
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut message = Map::new();
        message.insert("id".into(), json!(id));
        message.insert("method".into(), json!(method));
        message.insert("params".into(), params);
        if let Some(session) = session {
            message.insert("sessionId".into(), json!(session));
        }
        let (tx, rx) = mpsc::channel();
        self.lock().insert(id, tx);
        let text = Value::Object(message).to_string();
        if let Err(error) = (self.send)(&text) {
            self.lock().remove(&id);
            return Err(format!("cannot write to the browser: {error}"));
        }
        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(_) => {
                self.lock().remove(&id);
                if self.is_closed() {
                    Err("the browser closed".into())
                } else {
                    Err(format!("{method} got no answer in {}s", timeout.as_secs()))
                }
            }
        }
    }

    /// Sends `method` without waiting for an answer.
    pub fn notify(&self, method: &str, params: Value, session: Option<&str>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut message = Map::new();
        message.insert("id".into(), json!(id));
        message.insert("method".into(), json!(method));
        message.insert("params".into(), params);
        if let Some(session) = session {
            message.insert("sessionId".into(), json!(session));
        }
        let _ = (self.send)(&Value::Object(message).to_string());
    }

    /// Routes one message from the browser. Returns it when it is an event.
    pub fn dispatch(&self, message: Value) -> Option<Value> {
        let Some(id) = message.get("id").and_then(Value::as_u64) else {
            return Some(message);
        };
        let waiter = self.lock().remove(&id)?;
        let failed = message.get("type").and_then(Value::as_str) == Some("error")
            || message.get("error").is_some_and(|e| !e.is_null());
        let result = if failed {
            Err(error_text(&message))
        } else {
            Ok(message.get("result").cloned().unwrap_or(Value::Null))
        };
        let _ = waiter.send(result);
        None
    }

    /// Fails every waiting call, when the transport closed.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        for (_, waiter) in self.lock().drain() {
            let _ = waiter.send(Err("the browser closed".into()));
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, mpsc::Sender<Result<Value, String>>>> {
        self.pending.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// The message of an error answer: CDP's `error.message`, BiDi's `message`.
fn error_text(message: &Value) -> String {
    let cdp = message
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str);
    let bidi = message.get("message").and_then(Value::as_str);
    let code = message.get("error").and_then(Value::as_str);
    match (cdp, bidi, code) {
        (Some(text), _, _) => text.to_owned(),
        (None, Some(text), Some(code)) => format!("{code}: {text}"),
        (None, Some(text), None) => text.to_owned(),
        (None, None, Some(code)) => code.to_owned(),
        (None, None, None) => "the browser refused the command".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn answers_reach_their_caller_and_events_pass_through() {
        let sent = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = sent.clone();
        let rpc = Arc::new(Rpc::new(Box::new(move |text| {
            log.lock().unwrap().push(text.to_owned());
            Ok(())
        })));
        let caller = rpc.clone();
        let call = std::thread::spawn(move || {
            caller.call(
                "Page.navigate",
                json!({"url": "x"}),
                Some("s1"),
                Duration::from_secs(5),
            )
        });
        while sent.lock().unwrap().is_empty() {
            std::thread::yield_now();
        }
        let request: Value = serde_json::from_str(&sent.lock().unwrap()[0]).unwrap();
        assert_eq!(request["sessionId"], "s1");
        let event = json!({"method": "Page.loadEventFired", "params": {}});
        assert_eq!(rpc.dispatch(event.clone()), Some(event));
        assert_eq!(
            rpc.dispatch(json!({"id": request["id"], "result": {"ok": 1}})),
            None
        );
        assert_eq!(call.join().unwrap(), Ok(json!({"ok": 1})));
    }

    #[test]
    fn errors_of_both_protocols_read_as_text() {
        assert_eq!(
            error_text(&json!({"id": 1, "error": {"code": -32000, "message": "nope"}})),
            "nope"
        );
        assert_eq!(
            error_text(
                &json!({"type": "error", "id": 1, "error": "no such frame", "message": "gone"})
            ),
            "no such frame: gone"
        );
    }

    #[test]
    fn a_closed_transport_fails_waiting_calls() {
        let rpc = Arc::new(Rpc::new(Box::new(|_| Ok(()))));
        let caller = rpc.clone();
        let call = std::thread::spawn(move || {
            caller.call(
                "Browser.getVersion",
                json!({}),
                None,
                Duration::from_secs(5),
            )
        });
        while rpc.lock().is_empty() {
            std::thread::yield_now();
        }
        rpc.close();
        assert!(call.join().unwrap().is_err());
    }
}
