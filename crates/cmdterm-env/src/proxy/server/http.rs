//! Reads the requests a sandbox sends to an inspected host, one after the
//! other on a connection, and applies the [`ProxyRules`]: a guard can
//! refuse a request, and secrets are revealed in the headers that carry
//! credentials. Other headers and the request target never get a secret,
//! since servers echo them back in redirects, error pages, CORS headers
//! and search results. A filter made with
//! [`RequestFilter::without_secrets`] reveals none, for plain HTTP beyond
//! the local network and for an upstream whose certificate failed
//! verification.
//!
//! Bodies pass unchanged; the filter only follows `Content-Length` and
//! chunked framing to find where the next request starts. A guard whose
//! pattern names a body, such as a GraphQL mutation, makes the filter hold
//! the request until its body is in, up to [`MAX_BODY`]; a body it cannot
//! read (larger, compressed or chunked) counts as matching.
//!
//! After a request that asks for an `Upgrade`, and its body, the filter
//! cannot see whether the server switched protocols. If the next bytes look
//! like text, they
//! are read as the next request, since a server that turned the upgrade
//! down reads them that way too. Anything else, such as a WebSocket frame,
//! makes the rest of the connection pass unread.
//!
//! Only HTTP/1.x is read. The TLS side offers nothing else to inspected
//! hosts, and a request in any other version is refused.

use std::sync::Arc;

use super::Shared;
use crate::proxy::rules::{GuardMode, GuardRule, ProxyRules};

/// Longest request head or chunk line the filter keeps while it waits for
/// the rest.
const MAX_HEAD: usize = 64 * 1024;
/// Longest body the filter holds back for a guard to read.
const MAX_BODY: u64 = 1024 * 1024;
/// Characters of a body a question shows.
const DETAIL: usize = 600;

enum State {
    Head,
    /// Bytes of a `Content-Length` body still to come.
    Body(u64),
    ChunkSize,
    ChunkData(u64),
    /// The `\r\n` after a chunk's data; bytes of it still to come.
    ChunkEnd(u8),
    Trailers,
    /// After a request that asked for an upgrade, before the next byte.
    Upgrading,
    /// Everything after an upgrade.
    Opaque,
    /// A request held until its body is in; bytes of the body still to come.
    Held(u64),
}

/// A request a guard reads the body of before it goes upstream.
struct HeldRequest {
    method: String,
    target: String,
    /// The head as it goes upstream, secrets revealed.
    head: Vec<u8>,
    body: Vec<u8>,
}

pub(in crate::proxy) struct RequestFilter {
    shared: Arc<Shared>,
    /// Where the connection comes from, see [`super::NetworkProxy::endpoint_for`].
    scope: Option<Arc<str>>,
    host: String,
    /// False when no secret may be revealed on this connection.
    secrets: bool,
    /// The request being read asked for an upgrade, which takes effect
    /// after its body.
    upgrade_next: bool,
    state: State,
    /// The incomplete head or line read so far.
    pending: Vec<u8>,
    held: Option<HeldRequest>,
}

/// A response for a refused request. The connection closes after it.
fn refusal(status: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn bad_request(what: &str) -> Vec<u8> {
    refusal("400 Bad Request", &format!("cmdterm sandbox: {what}\n"))
}

impl RequestFilter {
    pub(in crate::proxy) fn new(shared: Arc<Shared>, scope: Option<Arc<str>>, host: &str) -> Self {
        Self {
            shared,
            scope,
            host: host.to_ascii_lowercase(),
            secrets: true,
            upgrade_next: false,
            state: State::Head,
            pending: Vec::new(),
            held: None,
        }
    }

    /// This filter, but it applies guards only and reveals no secret.
    pub(in crate::proxy) fn without_secrets(mut self) -> Self {
        self.secrets = false;
        self
    }

    /// Appends to `out` what goes upstream for `data`, the next bytes from
    /// the client. An error refuses a request: it holds the response for
    /// the client, `out` holds what may still go upstream, and the
    /// connection must close.
    pub(in crate::proxy) fn feed(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<(), Vec<u8>> {
        let mut rest: Vec<u8>;
        let mut data = data;
        while !data.is_empty() {
            match self.state {
                State::Opaque => {
                    out.extend_from_slice(data);
                    return Ok(());
                }
                State::Upgrading => {
                    self.state = if looks_like_text(data) {
                        State::Head
                    } else {
                        State::Opaque
                    };
                }
                State::Held(left) => {
                    let n = usize::try_from(left).map_or(data.len(), |left| left.min(data.len()));
                    let held = self.held.as_mut().expect("a held request");
                    held.body.extend_from_slice(&data[..n]);
                    data = &data[n..];
                    let left = left - n as u64;
                    if left > 0 {
                        self.state = State::Held(left);
                        continue;
                    }
                    let held = self.held.take().expect("a held request");
                    let rules = self.rules();
                    let body = Some(held.body.as_slice());
                    for guard in rules.body_guards_for(&self.host, &held.method, &held.target, body)
                    {
                        let detail = detail(&held.body);
                        self.decide(guard, &held.method, &held.target, Some(detail), body)?;
                    }
                    out.extend_from_slice(&held.head);
                    out.extend_from_slice(&held.body);
                    self.state = self.after_request();
                }
                State::Body(left) | State::ChunkData(left) => {
                    let n = usize::try_from(left).map_or(data.len(), |left| left.min(data.len()));
                    out.extend_from_slice(&data[..n]);
                    data = &data[n..];
                    let left = left - n as u64;
                    self.state = match (&self.state, left) {
                        (State::Body(_), 0) => self.after_request(),
                        (State::Body(_), left) => State::Body(left),
                        (_, 0) => State::ChunkEnd(2),
                        (_, left) => State::ChunkData(left),
                    };
                }
                State::ChunkEnd(left) => {
                    let n = usize::from(left).min(data.len());
                    let expected = &b"\r\n"[2 - usize::from(left)..][..n];
                    if &data[..n] != expected {
                        return Err(bad_request("bad chunk"));
                    }
                    out.extend_from_slice(&data[..n]);
                    data = &data[n..];
                    let left = left - n as u8;
                    self.state = if left == 0 {
                        State::ChunkSize
                    } else {
                        State::ChunkEnd(left)
                    };
                }
                State::Head | State::ChunkSize | State::Trailers => {
                    let end: &[u8] = if matches!(self.state, State::Head) {
                        b"\r\n\r\n"
                    } else {
                        b"\r\n"
                    };
                    let from = self.pending.len().saturating_sub(end.len() - 1);
                    self.pending.extend_from_slice(data);
                    let Some(at) = self.pending[from..]
                        .windows(end.len())
                        .position(|w| w == end)
                    else {
                        if self.pending.len() > MAX_HEAD {
                            return Err(bad_request("request head too large"));
                        }
                        return Ok(());
                    };
                    rest = self.pending.split_off(from + at + end.len());
                    let unit = std::mem::take(&mut self.pending);
                    match self.state {
                        State::Head => self.head(&unit, out)?,
                        State::ChunkSize => {
                            let line = std::str::from_utf8(&unit[..unit.len() - 2])
                                .ok()
                                .filter(|line| !has_line_break(line))
                                .ok_or_else(|| bad_request("bad chunk size"))?;
                            let size = line.split(';').next().unwrap_or_default().trim();
                            let size = u64::from_str_radix(size, 16)
                                .map_err(|_| bad_request("bad chunk size"))?;
                            out.extend_from_slice(&unit);
                            self.state = if size == 0 {
                                State::Trailers
                            } else {
                                State::ChunkData(size)
                            };
                        }
                        _ => {
                            // A trailer line with a bare LF would end the
                            // body early for a lenient server.
                            if unit[..unit.len() - 2]
                                .iter()
                                .any(|b| matches!(b, b'\r' | b'\n' | 0))
                            {
                                return Err(bad_request("bad line end in trailers"));
                            }
                            out.extend_from_slice(&unit);
                            if unit == b"\r\n" {
                                self.state = self.after_request();
                            }
                        }
                    }
                    data = &rest;
                }
            }
        }
        Ok(())
    }

    fn rules(&self) -> Arc<ProxyRules> {
        self.shared
            .rules
            .read()
            .map(|rules| Arc::clone(&rules))
            .unwrap_or_default()
    }

    /// Lets the request `guard` matched pass, or refuses it, as the guard's
    /// mode or the user says. `detail` is what the question shows of the
    /// body.
    fn decide(
        &self,
        guard: &GuardRule,
        method: &str,
        target: &str,
        detail: Option<String>,
        body: Option<&[u8]>,
    ) -> Result<(), Vec<u8>> {
        let allowed = match guard.mode {
            GuardMode::Allow => true,
            GuardMode::Block => false,
            GuardMode::Ask => {
                let approvals = self.shared.approvals.read().ok().and_then(|a| a.clone());
                approvals.is_some_and(|a| {
                    let scope = self.scope.as_deref();
                    a.ask_about(guard, scope, &self.host, method, target, detail, body)
                })
            }
        };
        if allowed {
            return Ok(());
        }
        let body = format!(
            "cmdterm sandbox: {} is blocked ({method} {}{target}). Settings, \
             Extensions decides what sandboxes may publish.\n",
            guard.title, self.host
        );
        Err(refusal("403 Forbidden", &body))
    }

    /// Checks one request head against the guards and writes it with its
    /// secrets revealed.
    fn head(&mut self, head: &[u8], out: &mut Vec<u8>) -> Result<(), Vec<u8>> {
        let text = std::str::from_utf8(head).map_err(|_| bad_request("bad request head"))?;
        // A server that takes a bare LF as a line end would read more
        // headers, or another request, than the filter sees.
        let text = text.strip_suffix("\r\n\r\n").unwrap_or(text);
        if text.split("\r\n").any(has_line_break) {
            return Err(bad_request("bad line end in request head"));
        }
        let mut lines = text.split("\r\n");
        let mut first = lines.next().unwrap_or_default().split(' ');
        let (Some(method), Some(target), Some(version), None) =
            (first.next(), first.next(), first.next(), first.next())
        else {
            return Err(bad_request("bad request line"));
        };
        if !(version.eq_ignore_ascii_case("HTTP/1.1") || version.eq_ignore_ascii_case("HTTP/1.0")) {
            let body = format!("cmdterm sandbox: {} takes HTTP/1.1 only\n", self.host);
            return Err(refusal("505 HTTP Version Not Supported", &body));
        }
        if method.eq_ignore_ascii_case("CONNECT") {
            return Err(refusal(
                "405 Method Not Allowed",
                "cmdterm sandbox: no tunnels inside a tunnel\n",
            ));
        }
        // TRACE sends the request back, headers and all.
        if method.eq_ignore_ascii_case("TRACE") {
            return Err(refusal(
                "405 Method Not Allowed",
                "cmdterm sandbox: TRACE is not passed on\n",
            ));
        }
        // A server takes an absolute target's authority over the Host
        // header, so it has to name this connection's host as well.
        if let Some(authority) = absolute_authority(target)
            && !self.is_host(authority)
        {
            let body = format!(
                "cmdterm sandbox: {target} does not go to {}, the host this connection goes to\n",
                self.host
            );
            return Err(refusal("421 Misdirected Request", &body));
        }
        let target = origin_form(target);
        let rules = self.rules();
        for guard in rules.guards_for(&self.host, method, target) {
            self.decide(guard, method, target, None, None)?;
        }
        let wants_body = rules.wants_body(&self.host, method, target);
        let start = out.len();
        out.extend_from_slice(method.as_bytes());
        out.push(b' ');
        out.extend_from_slice(target.as_bytes());
        out.push(b' ');
        out.extend_from_slice(version.as_bytes());
        out.extend_from_slice(b"\r\n");
        let mut length: Option<u64> = None;
        let mut chunked = false;
        let mut upgrade = false;
        let mut encoded = false;
        for line in lines.filter(|line| !line.is_empty()) {
            let Some((name, value)) = line.split_once(':') else {
                out.truncate(start);
                return Err(bad_request("bad header"));
            };
            // Whitespace before the colon is how requests get smuggled.
            if name.is_empty() || name.bytes().any(|b| b.is_ascii_whitespace()) {
                out.truncate(start);
                return Err(bad_request("bad header name"));
            }
            let value = value.trim();
            if name.eq_ignore_ascii_case("proxy-connection")
                || name.eq_ignore_ascii_case("proxy-authorization")
            {
                continue;
            }
            if name.eq_ignore_ascii_case("host") && !self.is_host(value) {
                out.truncate(start);
                let body = format!(
                    "cmdterm sandbox: the Host header {value} does not name {}, \
                     the host this connection goes to\n",
                    self.host
                );
                return Err(refusal("421 Misdirected Request", &body));
            }
            if name.eq_ignore_ascii_case("content-length") {
                // Digits only: `+5` or `5 5` parse differently upstream.
                let parsed = (!value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
                    .then(|| value.parse::<u64>().ok())
                    .flatten();
                if parsed.is_none() || length.is_some_and(|known| Some(known) != parsed) {
                    out.truncate(start);
                    return Err(bad_request("bad Content-Length"));
                }
                length = parsed;
            } else if name.eq_ignore_ascii_case("transfer-encoding") {
                let last = value.rsplit(',').next().unwrap_or_default().trim();
                if !last.eq_ignore_ascii_case("chunked") {
                    out.truncate(start);
                    return Err(bad_request("a request body must be chunked or sized"));
                }
                chunked = true;
            } else if name.eq_ignore_ascii_case("upgrade") {
                upgrade = true;
            } else if name.eq_ignore_ascii_case("content-encoding") {
                encoded |= !value.eq_ignore_ascii_case("identity");
            }
            // Only headers that carry credentials get the real value; a
            // server echoes others, such as Origin or a request id.
            let revealed = if self.secrets && carries_credentials(name) {
                rules.reveal(&self.host, value)
            } else {
                None
            };
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(revealed.as_deref().unwrap_or(value).as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"\r\n");
        if wants_body {
            let readable = !chunked && !encoded && length.is_none_or(|n| n <= MAX_BODY);
            match length {
                Some(n) if n > 0 && readable => {
                    self.held = Some(HeldRequest {
                        method: method.to_owned(),
                        target: target.to_owned(),
                        head: out.split_off(start),
                        body: Vec::new(),
                    });
                    self.state = State::Held(n);
                    return Ok(());
                }
                _ => {
                    let body: Option<&[u8]> = readable.then_some(&[]);
                    for guard in rules.body_guards_for(&self.host, method, target, body) {
                        if let Err(refused) = self.decide(guard, method, target, None, body) {
                            out.truncate(start);
                            return Err(refused);
                        }
                    }
                }
            }
        }
        // An upgrade takes effect after the request's body, which a server
        // that turns it down reads as a body too.
        self.upgrade_next = upgrade;
        self.state = if chunked {
            State::ChunkSize
        } else {
            match length {
                Some(n) if n > 0 => State::Body(n),
                _ => self.after_request(),
            }
        };
        Ok(())
    }

    /// The state after a whole request went through.
    fn after_request(&mut self) -> State {
        if std::mem::take(&mut self.upgrade_next) {
            State::Upgrading
        } else {
            State::Head
        }
    }
}

/// True when `line` holds a CR, LF or NUL. Lines are split at CRLF, and a
/// server that also ends a line at a bare LF would see more lines than the
/// filter does.
fn has_line_break(line: &str) -> bool {
    line.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0))
}

/// True for header names that carry credentials: `Authorization`,
/// `Cookie`, and names with `auth`, `token`, `key`, `secret`, `password`,
/// `credential` or `session`, such as `PRIVATE-TOKEN` or
/// `X-NuGet-ApiKey`.
fn carries_credentials(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "cookie"
        || [
            "auth",
            "token",
            "key",
            "secret",
            "pass",
            "credential",
            "session",
        ]
        .iter()
        .any(|part| name.contains(part))
}

/// The authority of an absolute `http://` or `https://` target.
fn absolute_authority(target: &str) -> Option<&str> {
    let rest = ["http://", "https://"].iter().find_map(|scheme| {
        target
            .get(..scheme.len())
            .filter(|start| start.eq_ignore_ascii_case(scheme))
            .map(|_| &target[scheme.len()..])
    })?;
    Some(rest.find(['/', '?']).map_or(rest, |at| &rest[..at]))
}

impl RequestFilter {
    /// True when `value`, a Host header, names the host this connection
    /// goes to, with or without a port.
    fn is_host(&self, value: &str) -> bool {
        let name = match value.strip_prefix('[') {
            Some(rest) => rest.split_once(']').map_or(rest, |(name, _)| name),
            None => match value.rsplit_once(':') {
                Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) => name,
                _ => value,
            },
        };
        let name = name.trim_end_matches('.');
        let host = self.host.trim_start_matches('[').trim_end_matches(']');
        name.eq_ignore_ascii_case(host.trim_end_matches('.'))
    }
}

/// True when `data` can start a request line, or the empty line a server
/// skips before one: printable ASCII or whitespace. A client's WebSocket
/// frame starts with a byte outside that range, or, with an extension bit
/// set, has the mask bit in its second byte.
fn looks_like_text(data: &[u8]) -> bool {
    let text = |byte: u8| matches!(byte, b'\t' | b'\n' | b'\r' | b' '..=b'~');
    data.first().is_some_and(|&b| text(b)) && data.get(1).is_none_or(|&b| text(b))
}

/// What a question shows of a request body: the text, with runs of
/// whitespace as one space, cut to [`DETAIL`] characters. For a GraphQL
/// request that is its variables, such as a pull request's title and
/// text, since the query in front of them would fill the space.
fn detail(body: &[u8]) -> String {
    let variables = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|mut json| Some(json.get_mut("variables")?.take()))
        .filter(|variables| !variables.is_null())
        .map(|variables| variables.to_string());
    let text = match &variables {
        Some(variables) => variables.as_str().into(),
        None => String::from_utf8_lossy(body),
    };
    let mut out = String::new();
    for (i, word) in text.split_whitespace().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(word);
        if out.len() > DETAIL * 4 {
            break;
        }
    }
    if let Some((at, _)) = out.char_indices().nth(DETAIL) {
        out.truncate(at);
        out.push('…');
    }
    out
}

/// `target` in origin form: an absolute `http://host/path` or
/// `https://host/path` loses its scheme and authority, so the origin
/// server gets `/path`.
fn origin_form(target: &str) -> &str {
    if absolute_authority(target).is_none() {
        return target;
    }
    let rest = target.find("://").map_or(target, |at| &target[at + 3..]);
    match rest.find(['/', '?']) {
        Some(at) if rest[at..].starts_with('/') => &rest[at..],
        // `http://host?q` has an empty path; the query alone is not a
        // valid origin form, so fall back to the root.
        Some(_) | None => "/",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::RwLock;
    use std::sync::atomic::AtomicBool;

    use super::*;
    use crate::proxy::rules::{GuardRule, SecretRule};

    fn shared(rules: ProxyRules) -> Arc<Shared> {
        Arc::new(Shared {
            allowed: RwLock::new(Vec::new()),
            any_host: AtomicBool::new(false),
            tls: RwLock::new(None),
            intercept_all: AtomicBool::new(false),
            rules: RwLock::new(Arc::new(rules)),
            blocking: RwLock::default(),
            approvals: RwLock::new(None),
            refused: crate::proxy::RefusalLog::default(),
            stop: AtomicBool::new(false),
        })
    }

    fn rules() -> ProxyRules {
        ProxyRules {
            secrets: vec![SecretRule {
                fake: "FAKEFAKEFAKE".into(),
                real: "real-token".into(),
                hosts: vec!["registry.example".into()],
            }],
            guards: vec![GuardRule {
                id: "npm.publish".into(),
                title: "npm publish".into(),
                hosts: vec!["registry.example".into()],
                requests: vec!["PUT /*".into()],
                mode: GuardMode::Ask,
                kind: None,
            }],
        }
    }

    /// Feeds `input` in pieces of `step` bytes.
    fn run(filter: &mut RequestFilter, input: &[u8], step: usize) -> Result<Vec<u8>, Vec<u8>> {
        let mut out = Vec::new();
        for piece in input.chunks(step) {
            filter.feed(piece, &mut out)?;
        }
        Ok(out)
    }

    #[test]
    fn secrets_are_revealed_in_the_headers_of_every_request_of_a_connection() {
        let input = b"GET /a?t=FAKEFAKEFAKE HTTP/1.1\r\nHost: registry.example\r\n\
            Authorization: Bearer FAKEFAKEFAKE\r\nContent-Length: 4\r\n\r\nFAKE\
            POST /b HTTP/1.1\r\nTransfer-Encoding: chunked\r\nX-Key:FAKEFAKEFAKE\r\n\r\n\
            3;x=y\r\nabc\r\n0\r\nTrailer: 1\r\n\r\n\
            GET http://registry.example/c HTTP/1.1\r\nProxy-Connection: keep-alive\r\n\r\n";
        // The target keeps its fake: servers echo targets back.
        let expected = "GET /a?t=FAKEFAKEFAKE HTTP/1.1\r\nHost: registry.example\r\n\
            Authorization: Bearer real-token\r\nContent-Length: 4\r\n\r\nFAKE\
            POST /b HTTP/1.1\r\nTransfer-Encoding: chunked\r\nX-Key: real-token\r\n\r\n\
            3;x=y\r\nabc\r\n0\r\nTrailer: 1\r\n\r\n\
            GET /c HTTP/1.1\r\n\r\n";
        let shared = shared(rules());
        for step in [1, 3, 7, input.len()] {
            let mut filter = RequestFilter::new(Arc::clone(&shared), None, "registry.example");
            let out = run(&mut filter, input, step).unwrap();
            assert_eq!(String::from_utf8(out).unwrap(), expected, "step {step}");
        }
        // Other hosts keep the fake, and so does a filter without secrets.
        let header = b"GET / HTTP/1.1\r\nX-Key: FAKEFAKEFAKE\r\n\r\n";
        let mut filter = RequestFilter::new(Arc::clone(&shared), None, "other.example");
        assert_eq!(run(&mut filter, header, 5).unwrap(), header);
        let mut filter = RequestFilter::new(shared, None, "registry.example").without_secrets();
        assert_eq!(run(&mut filter, header, 5).unwrap(), header);
    }

    #[test]
    fn a_host_header_must_name_the_connections_host() {
        let shared = shared(rules());
        for good in [
            "registry.example",
            "Registry.Example.",
            "registry.example:443",
        ] {
            let mut filter = RequestFilter::new(Arc::clone(&shared), None, "registry.example");
            let input = format!("GET / HTTP/1.1\r\nHost: {good}\r\n\r\n");
            assert!(run(&mut filter, input.as_bytes(), 64).is_ok(), "{good}");
        }
        let mut filter = RequestFilter::new(shared, None, "registry.example");
        let refused = run(
            &mut filter,
            b"GET / HTTP/1.1\r\nHost: other.example\r\n\r\n",
            64,
        );
        assert!(
            String::from_utf8(refused.unwrap_err())
                .unwrap()
                .starts_with("HTTP/1.1 421")
        );
    }

    #[test]
    fn body_guards_hold_the_request_until_its_body_is_read() {
        let graphql = ProxyRules {
            secrets: vec![],
            guards: vec![GuardRule {
                id: "github.merge".into(),
                title: "merge a pull request".into(),
                hosts: vec!["api.github.com".into()],
                requests: vec!["POST /graphql *mutation*mergePullRequest*".into()],
                mode: GuardMode::Block,
                kind: Some(crate::proxy::GuardKind::Merge),
            }],
        };
        let shared = shared(graphql);
        let request = |body: &str| {
            format!(
                "POST /graphql HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
        };
        let query = request(r#"{"query":"query{viewer{login}}"}"#);
        for step in [1, 5, query.len()] {
            let mut filter = RequestFilter::new(Arc::clone(&shared), None, "api.github.com");
            let out = run(&mut filter, query.as_bytes(), step).unwrap();
            assert_eq!(out, query.as_bytes());
        }
        // Two requests on one connection: the query passes, the mutation
        // is refused before any of it goes upstream.
        let merge = request(r#"{"query":"mutation{mergePullRequest(input:{})}"}"#);
        assert_eq!(
            detail(br#"{"query":"mutation M { x }","variables":{"title":"Fix it"}}"#),
            r#"{"title":"Fix it"}"#
        );
        assert_eq!(detail(b"a  b\n c"), "a b c");
        let both = format!("{query}{merge}");
        for step in [1, 7, both.len()] {
            let mut filter = RequestFilter::new(Arc::clone(&shared), None, "api.github.com");
            let mut out = Vec::new();
            let mut refused = None;
            for piece in both.as_bytes().chunks(step) {
                if let Err(response) = filter.feed(piece, &mut out) {
                    refused = Some(response);
                    break;
                }
            }
            let refused = String::from_utf8(refused.expect("refused")).unwrap();
            assert!(refused.contains("merge a pull request is blocked"));
            assert_eq!(out, query.as_bytes());
        }
        // A body the filter cannot read counts as matching.
        let mut filter = RequestFilter::new(Arc::clone(&shared), None, "api.github.com");
        let gzip =
            b"POST /graphql HTTP/1.1\r\nContent-Encoding: gzip\r\nContent-Length: 2\r\n\r\nxx";
        assert!(run(&mut filter, gzip, 64).is_err());
        let mut filter = RequestFilter::new(Arc::clone(&shared), None, "api.github.com");
        let chunked = b"POST /graphql HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n";
        assert!(run(&mut filter, chunked, 64).is_err());
    }

    #[test]
    fn guarded_requests_need_an_answer_and_bad_framing_is_refused() {
        // Ask without anyone to answer refuses.
        let shared = shared(rules());
        let mut filter = RequestFilter::new(Arc::clone(&shared), None, "registry.example");
        let mut out = Vec::new();
        let refused = filter
            .feed(
                b"GET /ok HTTP/1.1\r\n\r\nPUT /pkg HTTP/1.1\r\nContent-Length: 1\r\n\r\nx",
                &mut out,
            )
            .unwrap_err();
        assert!(String::from_utf8_lossy(&refused).starts_with("HTTP/1.1 403"));
        assert!(String::from_utf8_lossy(&refused).contains("npm publish is blocked"));
        assert_eq!(out, b"GET /ok HTTP/1.1\r\n\r\n");

        let mut allowed = rules();
        allowed.guards[0].mode = GuardMode::Allow;
        *shared.rules.write().unwrap() = Arc::new(allowed);
        let mut filter = RequestFilter::new(Arc::clone(&shared), None, "registry.example");
        assert!(run(&mut filter, b"PUT /pkg HTTP/1.1\r\n\r\n", 4).is_ok());

        for bad in [
            &b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"[..],
            b"GET / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
            b"GET / HTTP/1.1\r\nTransfer-Encoding: gzip\r\n\r\n",
            b"GET / HTTP/1.1\r\nHost : x\r\n\r\n",
            b"GET / HTTP/1.1\r\nContent-Length: +5\r\n\r\n",
            b"GET / HTTP/1.1\r\nContent-Length: \r\n\r\n",
            // A bare LF hides a header from the filter but not from a
            // lenient server.
            b"POST / HTTP/1.1\r\nX: a\nTransfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\n",
            b"GET / HTTP/1.1\r\nX: a\rb\r\n\r\n",
            b"GET / HTTP/1.1\r\nX: a\0b\r\n\r\n",
            b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
            b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nab\r\n",
        ] {
            let mut filter = RequestFilter::new(Arc::clone(&shared), None, "registry.example");
            assert!(
                run(&mut filter, bad, 64).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn after_an_upgrade_frames_pass_unread_and_requests_are_read() {
        let shared = shared(rules());
        let upgrade = b"GET /ws HTTP/1.1\r\nUpgrade: websocket\r\n\r\n".to_vec();
        // A masked WebSocket text frame, then bytes that would be a guarded
        // request if they were read.
        let mut frames = upgrade.clone();
        frames.extend_from_slice(&[0x81, 0x84, 1, 2, 3, 4, 0x55, 0x55, 0x55, 0x55]);
        frames.extend_from_slice(b"PUT /pkg HTTP/1.1\r\n\r\n");
        // With permessage-deflate and a fragmented message, the first byte
        // is a letter, but the mask bit still tells it apart.
        let mut compressed = upgrade.clone();
        compressed.extend_from_slice(&[0x41, 0x81, 1, 2, 3, 4, 0x55]);
        for input in [frames, compressed] {
            let mut filter = RequestFilter::new(Arc::clone(&shared), None, "registry.example");
            assert_eq!(run(&mut filter, &input, input.len()).unwrap(), input);
        }
        // A server that turned the upgrade down reads the next request, so
        // the filter reads it too and its guard applies.
        let mut pipelined = upgrade.clone();
        pipelined.extend_from_slice(b"PUT /pkg HTTP/1.1\r\n\r\n");
        for step in [1, 9, pipelined.len()] {
            let mut filter = RequestFilter::new(Arc::clone(&shared), None, "registry.example");
            let refused = run(&mut filter, &pipelined, step).unwrap_err();
            assert!(
                String::from_utf8(refused)
                    .unwrap()
                    .starts_with("HTTP/1.1 403")
            );
        }
    }

    #[test]
    fn an_upgrade_takes_effect_after_the_body() {
        let shared = shared(rules());
        // A body that starts like a WebSocket frame is still a body, and
        // the request after it is read.
        let sized = b"POST /ws HTTP/1.1\r\nUpgrade: websocket\r\nContent-Length: 2\r\n\r\n\x81\x84\
            PUT /pkg HTTP/1.1\r\n\r\n";
        let chunked = b"POST /ws HTTP/1.1\r\nUpgrade: h2c\r\nTransfer-Encoding: chunked\r\n\r\n\
            2\r\n\x81\x84\r\n0\r\n\r\nPUT /pkg HTTP/1.1\r\n\r\n";
        for input in [&sized[..], &chunked[..]] {
            for step in [1, 7, input.len()] {
                let mut filter = RequestFilter::new(Arc::clone(&shared), None, "registry.example");
                let refused = run(&mut filter, input, step).unwrap_err();
                assert!(
                    String::from_utf8(refused)
                        .unwrap()
                        .starts_with("HTTP/1.1 403")
                );
            }
        }
        // Frames after the body pass.
        let mut frames =
            b"POST /ws HTTP/1.1\r\nUpgrade: h2c\r\nContent-Length: 1\r\n\r\nx".to_vec();
        frames.extend_from_slice(&[0x81, 0x84, 1, 2, 3, 4]);
        let mut filter = RequestFilter::new(Arc::clone(&shared), None, "registry.example");
        assert_eq!(run(&mut filter, &frames, frames.len()).unwrap(), frames);
    }

    #[test]
    fn chunk_lines_and_trailers_with_bare_line_ends_are_refused() {
        let shared = shared(rules());
        let trailer: &[u8] = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n\
            0\r\nX: a\n\r\nPUT /pkg HTTP/1.1\r\n\r\n";
        let size: &[u8] =
            b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n1;x\n\r\na\r\n0\r\n\r\n";
        for bad in [trailer, size] {
            let mut filter = RequestFilter::new(Arc::clone(&shared), None, "registry.example");
            let refused = run(&mut filter, bad, 64).unwrap_err();
            assert!(
                String::from_utf8(refused)
                    .unwrap()
                    .starts_with("HTTP/1.1 400")
            );
        }
    }

    #[test]
    fn absolute_targets_must_name_the_connections_host() {
        let shared = shared(rules());
        let mut filter = RequestFilter::new(Arc::clone(&shared), None, "registry.example");
        let input =
            b"GET https://registry.example:443/a HTTP/1.1\r\nHost: registry.example\r\n\r\n";
        let out = run(&mut filter, input, 64).unwrap();
        assert!(out.starts_with(b"GET /a HTTP/1.1\r\n"));
        for bad in [
            "https://evil.example/b",
            "http://evil.example/b",
            "HTTPS://evil.example",
        ] {
            let mut filter = RequestFilter::new(Arc::clone(&shared), None, "registry.example");
            let input = format!("GET {bad} HTTP/1.1\r\nHost: registry.example\r\n\r\n");
            let refused = run(&mut filter, input.as_bytes(), 64).unwrap_err();
            let refused = String::from_utf8(refused).unwrap();
            assert!(refused.starts_with("HTTP/1.1 421"), "{bad}");
        }
    }

    #[test]
    fn only_credential_headers_get_secrets_and_trace_is_refused() {
        let shared = shared(rules());
        let mut filter = RequestFilter::new(Arc::clone(&shared), None, "registry.example");
        let input = b"GET / HTTP/1.1\r\nOrigin: https://FAKEFAKEFAKE.example\r\n\
            X-Request-Id: FAKEFAKEFAKE\r\nPRIVATE-TOKEN: FAKEFAKEFAKE\r\nCookie: s=FAKEFAKEFAKE\r\n\r\n";
        let out = String::from_utf8(run(&mut filter, input, 64).unwrap()).unwrap();
        assert!(
            out.contains("Origin: https://FAKEFAKEFAKE.example\r\n"),
            "{out}"
        );
        assert!(out.contains("X-Request-Id: FAKEFAKEFAKE\r\n"), "{out}");
        assert!(out.contains("PRIVATE-TOKEN: real-token\r\n"), "{out}");
        assert!(out.contains("Cookie: s=real-token\r\n"), "{out}");
        let mut filter = RequestFilter::new(shared, None, "registry.example");
        let refused = run(&mut filter, b"TRACE / HTTP/1.1\r\n\r\n", 64).unwrap_err();
        assert!(
            String::from_utf8(refused)
                .unwrap()
                .starts_with("HTTP/1.1 405")
        );
    }

    #[test]
    fn every_guard_a_request_matches_decides() {
        let guard = |id: &str, mode: GuardMode, body: &str| GuardRule {
            id: id.into(),
            title: id.into(),
            hosts: vec!["api.example".into()],
            requests: vec![format!("POST /graphql *{body}*")],
            mode,
            kind: None,
        };
        let shared = shared(ProxyRules {
            secrets: vec![],
            guards: vec![
                guard("create", GuardMode::Allow, "createPullRequest"),
                guard("merge", GuardMode::Block, "mergePullRequest"),
            ],
        });
        let request = |body: &str| {
            format!(
                "POST /graphql HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
        };
        let create = request(r#"{"query":"mutation{createPullRequest(x:1){id}}"}"#);
        let mut filter = RequestFilter::new(Arc::clone(&shared), None, "api.example");
        assert!(run(&mut filter, create.as_bytes(), 64).is_ok());
        let both = request(
            r#"{"query":"mutation{createPullRequest(x:1){id} mergePullRequest(y:2){id}}"}"#,
        );
        let mut filter = RequestFilter::new(shared, None, "api.example");
        let refused = run(&mut filter, both.as_bytes(), 64).unwrap_err();
        assert!(
            String::from_utf8(refused)
                .unwrap()
                .contains("merge is blocked")
        );
    }

    #[test]
    fn non_ascii_requests_do_not_panic() {
        let shared = shared(rules());
        for input in [
            "GET /aaaaa\u{20ac} HTTP/1.1\r\n\r\n",
            "GET / HTTP/1.1\r\nX: aaaaa\u{20ac}\r\n\r\n",
            "GET / HTTP/1.1\r\nAuthorization: Basi\u{20ac}\r\n\r\n",
        ] {
            let mut filter = RequestFilter::new(Arc::clone(&shared), None, "registry.example");
            assert!(run(&mut filter, input.as_bytes(), 64).is_ok(), "{input}");
        }
    }
}
