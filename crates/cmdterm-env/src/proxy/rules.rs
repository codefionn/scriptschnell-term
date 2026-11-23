//! What the proxy does with requests to some hosts besides passing them on:
//! it puts real credentials in place of the fake ones sandboxes hold, and it
//! stops requests that publish (a push, a package upload) unless the user
//! allows them.
//!
//! A sandbox never sees a [`SecretRule::real`] value. Extensions hand it the
//! [`SecretRule::fake`] instead, in config files and variables, and the
//! proxy swaps the fake for the real value in requests to the secret's
//! hosts: in the values of headers that carry credentials, also inside
//! `Basic` credentials. Other headers and the request target keep the
//! fake, since servers echo them back. A fake sent to any
//! other host, to a server whose certificate failed verification, or over
//! plain HTTP beyond this machine and the local network stays a fake.
//!
//! To read these requests the proxy ends their TLS itself (see
//! `server/tls.rs`) and speaks HTTP/1.1 with both sides (`server/http.rs`).

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, OnceLock};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};

use super::host_allowed;

/// What happens to a request a [`GuardRule`] matches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardMode {
    /// The user decides, request by request.
    #[default]
    Ask,
    Block,
    Allow,
}

/// What a forge's guard holds back on a change request (a pull or merge
/// request). The user picks one mode per kind for every forge, and a guard's
/// own mode, when set, wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardKind {
    /// Opening a change request.
    Create,
    /// Comments and reviews on one.
    Comment,
    /// Merging one, which Azure DevOps calls completing.
    Merge,
}

impl GuardKind {
    pub const ALL: [Self; 3] = [Self::Create, Self::Comment, Self::Merge];

    /// The name extensions and the settings use.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Comment => "comment",
            Self::Merge => "merge",
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }
}

/// A credential the proxy adds to requests in place of its fake.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct SecretRule {
    /// What the sandbox holds, from [`fake_secret`].
    pub fake: String,
    pub real: String,
    /// Hosts that get the real value, as in the allowed hosts. Never `*`.
    pub hosts: Vec<String>,
}

impl fmt::Debug for SecretRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretRule")
            .field("fake", &self.fake)
            .field("real", &"<hidden>")
            .field("hosts", &self.hosts)
            .finish()
    }
}

/// Requests that publish something, such as `git push` or `cargo publish`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GuardRule {
    /// `<extension>.<name>`, the key of the user's choice in the settings.
    pub id: String,
    /// What the user sees, such as `npm publish`.
    pub title: String,
    /// Host patterns as in the allowed hosts, or `*` for every host.
    pub hosts: Vec<String>,
    /// `METHOD TARGET` or `METHOD TARGET BODY` patterns, such as
    /// `PUT /api/v1/crates/new`, `POST */git-receive-pack` or
    /// `POST /graphql *mutation*createPullRequest*`. `*` matches any run of
    /// characters, and a METHOD of `*` any method. TARGET is the path with
    /// its query; a TARGET without `?` matches whatever query follows. Both sides are compared without case, after decoding
    /// `%XX` escapes and resolving `.`, `..` and repeated slashes in the
    /// path. BODY, which may hold spaces, is compared without case against
    /// the request body after decoding `%XX` and JSON's `\uXXXX` escapes.
    pub requests: Vec<String>,
    pub mode: GuardMode,
    /// Set for a forge's guards on change requests.
    pub kind: Option<GuardKind>,
}

/// One of [`GuardRule::requests`], split.
struct RequestPattern<'a> {
    method: &'a str,
    target: &'a str,
    body: Option<&'a str>,
}

impl<'a> RequestPattern<'a> {
    fn parse(pattern: &'a str) -> Option<Self> {
        let mut parts = pattern.splitn(3, ' ');
        Some(Self {
            method: parts.next()?,
            target: parts.next()?,
            body: parts.next(),
        })
    }

    /// `target` is normalized. A pattern without a query matches the path
    /// whatever query follows it.
    fn head_matches(&self, method: &str, target: &str) -> bool {
        if !(self.method == "*" || self.method.eq_ignore_ascii_case(method)) {
            return false;
        }
        let want = self.target.to_ascii_lowercase();
        glob(want.as_bytes(), target.as_bytes())
            || (!want.contains('?')
                && target
                    .split_once('?')
                    .is_some_and(|(path, _)| glob(want.as_bytes(), path.as_bytes())))
    }
}

impl GuardRule {
    fn applies(&self, host: &str) -> bool {
        self.mode != GuardMode::Allow && host_matches(&self.hosts, host)
    }

    /// The patterns that match a request head: those without a body when
    /// `with_body` is false, else those with one.
    fn patterns<'a>(
        &'a self,
        method: &'a str,
        target: &'a str,
        with_body: bool,
    ) -> impl Iterator<Item = RequestPattern<'a>> + 'a {
        (self.requests.iter())
            .filter_map(|request| RequestPattern::parse(request))
            .filter(move |p| p.body.is_some() == with_body && p.head_matches(method, target))
    }
}

/// Everything the proxy does to requests besides filtering hosts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ProxyRules {
    pub secrets: Vec<SecretRule>,
    pub guards: Vec<GuardRule>,
}

impl ProxyRules {
    /// True when the proxy has no request to read: no secrets, and every
    /// guard allows.
    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty() && self.guards.iter().all(|g| g.mode == GuardMode::Allow)
    }

    /// True when the proxy reads the requests to `host`.
    pub fn inspects(&self, host: &str) -> bool {
        self.secrets
            .iter()
            .any(|secret| host_allowed(&secret.hosts, host))
            || self
                .guards
                .iter()
                .any(|guard| guard.mode != GuardMode::Allow && host_matches(&guard.hosts, host))
    }

    /// The first guard that holds back `method target` to `host` by its
    /// head alone. `target` is the request target in origin form, as sent.
    pub fn guard(&self, host: &str, method: &str, target: &str) -> Option<&GuardRule> {
        self.guards_for(host, method, target).next()
    }

    /// Every guard that holds back `method target` to `host` by its head
    /// alone. A request passes only when each of them lets it.
    pub fn guards_for<'a>(
        &'a self,
        host: &str,
        method: &str,
        target: &str,
    ) -> impl Iterator<Item = &'a GuardRule> + use<'a> {
        let target = normalize_target(target);
        let (host, method) = (host.to_owned(), method.to_owned());
        (self.guards.iter())
            .filter(move |guard| guard.applies(&host))
            .filter(move |guard| guard.patterns(&method, &target, false).next().is_some())
    }

    /// True when a guard needs the body of `method target` to `host` to
    /// decide, because one of its patterns names a body.
    pub fn wants_body(&self, host: &str, method: &str, target: &str) -> bool {
        let target = normalize_target(target);
        (self.guards.iter())
            .filter(|guard| guard.applies(host))
            .any(|guard| guard.patterns(method, &target, true).next().is_some())
    }

    /// The first guard that holds back `method target` to `host` with
    /// `body`. Without a body, because the proxy could not read it (too
    /// large, compressed or chunked), every pattern that names one matches.
    pub fn body_guard(
        &self,
        host: &str,
        method: &str,
        target: &str,
        body: Option<&[u8]>,
    ) -> Option<&GuardRule> {
        self.body_guards_for(host, method, target, body).next()
    }

    /// Every guard that holds back `method target` to `host` with `body`,
    /// as [`Self::body_guard`] finds the first. A request that matches
    /// several, such as one GraphQL request that opens and merges a pull
    /// request, passes only when each of them lets it.
    pub fn body_guards_for<'a>(
        &'a self,
        host: &str,
        method: &str,
        target: &str,
        body: Option<&[u8]>,
    ) -> impl Iterator<Item = &'a GuardRule> + use<'a> {
        let target = normalize_target(target);
        let body = body.map(normalize_body);
        let (host, method) = (host.to_owned(), method.to_owned());
        (self.guards.iter())
            .filter(move |guard| guard.applies(&host))
            .filter(move |guard| {
                guard.patterns(&method, &target, true).any(|p| {
                    let want = p.body.unwrap_or_default().to_ascii_lowercase();
                    body.as_deref()
                        .is_none_or(|body| glob(want.as_bytes(), body.as_bytes()))
                })
            })
    }

    /// `value` with the fakes of `host`'s secrets replaced by the real
    /// values, or `None` when it holds none. `Basic` credentials are
    /// decoded, rewritten and encoded again.
    pub(crate) fn reveal(&self, host: &str, value: &str) -> Option<String> {
        let secrets: Vec<&SecretRule> = self
            .secrets
            .iter()
            .filter(|secret| host_allowed(&secret.hosts, host))
            .collect();
        if secrets.is_empty() {
            return None;
        }
        let replace = |text: &str| -> Option<String> {
            let mut out: Option<String> = None;
            for secret in &secrets {
                let current = out.as_deref().unwrap_or(text);
                if current.contains(&secret.fake) {
                    out = Some(current.replace(&secret.fake, &secret.real));
                }
            }
            out
        };
        let trimmed = value.trim_start();
        if let Some(encoded) = trimmed
            .get(..6)
            .filter(|scheme| scheme.eq_ignore_ascii_case("basic "))
            .map(|_| trimmed[6..].trim())
            .filter(|encoded| !encoded.is_empty())
            && let Some(decoded) = STANDARD
                .decode(encoded)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
            && let Some(revealed) = replace(&decoded)
        {
            return Some(format!("Basic {}", STANDARD.encode(revealed)));
        }
        replace(value)
    }
}

/// The guard for secrets in this machine's keyring, which the keyring bus
/// (`keyring.rs`) applies, on Linux only. It has no hosts and no requests,
/// so the network proxy never applies it.
pub const KEYRING_GUARD: &str = "cmdterm.keyring";

/// The guard for changes a coding agent in a sandbox makes to its settings,
/// such as trusting a folder, which the app brings over to this machine's
/// copy. Like [`KEYRING_GUARD`] it has no hosts and no requests, and only
/// bubblewrap sandboxes, on Linux, write such copies.
pub const AGENT_SETTINGS_GUARD: &str = "cmdterm.agent-settings";

/// The guard for links that programs in a sandbox open in the browser on
/// this machine, through the proxy's [`crate::OPEN_HOST`]. It has no hosts
/// and no requests either: the app asks with the link's host in its place.
pub const OPEN_URL_GUARD: &str = "cmdterm.open-url";

/// The guards the proxy always has, for the public registries and for
/// pushes over HTTPS to any host: `git push`, `hg push`, npm, Cargo,
/// NuGet, PyPI, RubyGems and container registries, the [`OPEN_URL_GUARD`],
/// and on Linux the [`KEYRING_GUARD`] and the [`AGENT_SETTINGS_GUARD`].
/// Their ids start with `cmdterm.`, and their mode is
/// [`GuardMode::Ask`] until the user picks another. Extensions add guards
/// for other hosts, such as a company registry.
pub fn builtin_guards() -> Vec<GuardRule> {
    let guard = |name: &str, title: &str, hosts: &[&str], requests: &[&str]| GuardRule {
        id: format!("cmdterm.{name}"),
        title: title.to_owned(),
        hosts: hosts.iter().map(|h| (*h).to_owned()).collect(),
        requests: requests.iter().map(|r| (*r).to_owned()).collect(),
        mode: GuardMode::Ask,
        kind: None,
    };
    vec![
        guard(
            "git-push",
            "git push",
            &["*"],
            &[
                "POST */git-receive-pack",
                "GET */info/refs?*service=git-receive-pack*",
            ],
        ),
        guard(
            "hg-push",
            "hg push",
            &["*"],
            &["POST *cmd=unbundle*", "POST *cmd=pushkey*"],
        ),
        guard(
            "npm-publish",
            "publish to npm",
            &["registry.npmjs.org", "registry.yarnpkg.com"],
            &["PUT /*", "DELETE /*"],
        ),
        guard(
            "cargo-publish",
            "publish a crate",
            &["crates.io"],
            &[
                "PUT /api/v1/crates/new",
                "DELETE /api/v1/crates/*/yank",
                "PUT /api/v1/crates/*/unyank",
                "PUT /api/v1/crates/*/owners",
                "DELETE /api/v1/crates/*/owners",
            ],
        ),
        guard(
            "nuget-push",
            "push a NuGet package",
            &["www.nuget.org"],
            &["PUT */api/v2/package*", "DELETE */api/v2/package/*"],
        ),
        guard(
            "pypi-upload",
            "upload to PyPI",
            &["upload.pypi.org", "test.pypi.org"],
            &["POST /legacy*"],
        ),
        guard(
            "gem-push",
            "push a gem",
            &["rubygems.org"],
            &["POST /api/v1/gems*", "DELETE /api/v1/gems/yank*"],
        ),
        guard(
            "container-push",
            "push a container image",
            &["*"],
            &["POST */v2/*/blobs/uploads*", "PUT */v2/*/manifests/*"],
        ),
        guard("open-url", "open a link in your browser", &[], &[]),
        guard(
            "keyring",
            "use a secret from this machine's keyring",
            &[],
            &[],
        ),
        guard(
            "agent-settings",
            "change a coding agent's settings on this machine",
            &[],
            &[],
        ),
    ]
    .into_iter()
    .filter(|guard| {
        cfg!(target_os = "linux") || ![KEYRING_GUARD, AGENT_SETTINGS_GUARD].contains(&&*guard.id)
    })
    .collect()
}

/// True when `host` matches one of `patterns`, where `*` is every host.
fn host_matches(patterns: &[String], host: &str) -> bool {
    patterns.iter().any(|p| p == "*") || host_allowed(patterns, host)
}

/// True when `pattern` is a request pattern a [`GuardRule`] can hold:
/// `METHOD TARGET`, optionally followed by a space and a BODY pattern.
pub fn valid_request_pattern(pattern: &str) -> bool {
    let Some(RequestPattern {
        method,
        target,
        body,
    }) = RequestPattern::parse(pattern)
    else {
        return false;
    };
    (method == "*" || (!method.is_empty() && method.bytes().all(|b| b.is_ascii_alphabetic())))
        && !target.is_empty()
        && !target.bytes().any(|b| b.is_ascii_whitespace())
        && body.is_none_or(|body| !body.trim().is_empty())
}

/// A request body the way guards compare it: JSON's `\uXXXX` escapes and
/// `%XX` escapes decoded, lowercase. Bytes that are not UTF-8 become
/// U+FFFD.
fn normalize_body(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let mut unescaped = String::with_capacity(text.len());
    let mut rest = &*text;
    while let Some(at) = rest.find("\\u") {
        unescaped.push_str(&rest[..at]);
        let code = rest
            .get(at + 2..at + 6)
            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
            .and_then(char::from_u32);
        match code {
            Some(c) => {
                unescaped.push(c);
                rest = &rest[at + 6..];
            }
            None => {
                unescaped.push_str("\\u");
                rest = &rest[at + 2..];
            }
        }
    }
    unescaped.push_str(rest);
    decode(&unescaped)
}

/// `target` the way guards compare it: escapes decoded, lowercase, and the
/// path with `.`, `..` and empty segments resolved, so `/A//b/./%63` and
/// `/a/b/c` are the same. An absolute URL keeps only its path and query.
pub(super) fn normalize_target(target: &str) -> String {
    let mut target = target;
    for scheme in ["http://", "https://"] {
        if target.len() > scheme.len()
            && target
                .get(..scheme.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(scheme))
        {
            let rest = &target[scheme.len()..];
            target = rest.find(['/', '?']).map_or("/", |at| &rest[at..]);
        }
    }
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (target, None),
    };
    let path = decode(path);
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            segment => segments.push(segment),
        }
    }
    let mut out = String::with_capacity(target.len());
    for segment in &segments {
        out.push('/');
        out.push_str(segment);
    }
    if out.is_empty() || (path.ends_with('/') && !segments.is_empty()) {
        out.push('/');
    }
    if let Some(query) = query {
        out.push('?');
        out.push_str(&decode(query));
    }
    out
}

/// Lowercase, with `%XX` escapes decoded. Bytes that are not UTF-8 after
/// decoding become U+FFFD.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
        {
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).to_ascii_lowercase()
}

/// True when `text` matches `pattern`, where `*` matches any run of bytes.
fn glob(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && pattern[p] == b'*' {
            star = Some((p, t));
            p += 1;
        } else if p < pattern.len() && pattern[p] == text[t] {
            p += 1;
            t += 1;
        } else if let Some((star_p, star_t)) = star {
            p = star_p + 1;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&b| b == b'*')
}

/// Fewest random characters a fake keeps the shape of the real value with.
const MIN_RANDOM: usize = 16;

/// The fake that stands for `real` in sandboxes, the same one every time
/// in this process. It has the shape of `real` where it can, so clients
/// that check a token's format accept it: the same length, a leading
/// prefix such as `npm_` or `glpat-` kept, and every other letter or digit
/// replaced by a random one of its kind. A value with fewer than 16 letters
/// and digits after its prefix gets 32 random letters and digits instead.
pub fn fake_secret(real: &str) -> String {
    let mut fakes = fakes();
    if let Some(fake) = fakes.by_real.get(real) {
        return fake.clone();
    }
    let fake = fakes.new_fake(real);
    fakes.by_real.insert(real.to_owned(), fake.clone());
    fake
}

/// The fake that stands for the credential `name`, the same one every time
/// in this process, whatever its value is now. The first value gives it its
/// shape, as in [`fake_secret`]. A token that the host refreshes, such as
/// an OAuth access token, keeps its fake this way, so files that sandboxes
/// already read stay valid when its value changes.
pub fn named_fake_secret(name: &str, real: &str) -> String {
    let mut fakes = fakes();
    if let Some(fake) = fakes.by_name.get(name) {
        return fake.clone();
    }
    let fake = fakes.new_fake(real);
    fakes.by_name.insert(name.to_owned(), fake.clone());
    fake
}

#[derive(Default)]
struct Fakes {
    by_real: HashMap<String, String>,
    by_name: HashMap<String, String>,
}

impl Fakes {
    /// A fake for `real` that is neither `real` nor one handed out before.
    fn new_fake(&self, real: &str) -> String {
        loop {
            let fake = new_fake(real);
            let taken = |map: &HashMap<String, String>| map.values().any(|other| *other == fake);
            if fake != real && !taken(&self.by_real) && !taken(&self.by_name) {
                return fake;
            }
        }
    }
}

fn fakes() -> std::sync::MutexGuard<'static, Fakes> {
    static FAKES: OnceLock<Mutex<Fakes>> = OnceLock::new();
    let fakes = FAKES.get_or_init(Mutex::default);
    fakes.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn new_fake(real: &str) -> String {
    let prefix = token_prefix(real);
    let rest = &real[prefix.len()..];
    let shaped = rest.bytes().filter(u8::is_ascii_alphanumeric).count() >= MIN_RANDOM;
    let mut random = vec![0u8; if shaped { rest.len() } else { 32 }];
    if getrandom::fill(&mut random).is_err() {
        // No entropy source: still never hand out the real value.
        let seed = std::collections::hash_map::RandomState::new();
        for (i, byte) in random.iter_mut().enumerate() {
            use std::hash::BuildHasher;
            *byte = seed.hash_one(i) as u8;
        }
    }
    let pick = |set: &[u8], r: u8| set[usize::from(r) % set.len()] as char;
    const DIGITS: &[u8] = b"0123456789";
    const LOWER: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
    const UPPER: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    const ALNUM: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    if !shaped {
        return random.iter().map(|&r| pick(ALNUM, r)).collect();
    }
    let mut fake = String::with_capacity(real.len());
    fake.push_str(prefix);
    for (c, &r) in rest.chars().zip(&random) {
        fake.push(match c {
            '0'..='9' => pick(DIGITS, r),
            'a'..='z' => pick(LOWER, r),
            'A'..='Z' => pick(UPPER, r),
            other => other,
        });
    }
    fake
}

/// A leading `[A-Za-z]{2,8}` followed by `_` or `-`, as in `npm_`, `ghp_`
/// or `glpat-`, or nothing.
fn token_prefix(real: &str) -> &str {
    let letters = real.bytes().take_while(u8::is_ascii_alphabetic).count();
    match real.as_bytes().get(letters) {
        Some(b'_' | b'-') if (2..=8).contains(&letters) => &real[..=letters],
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard(hosts: &[&str], requests: &[&str], mode: GuardMode) -> GuardRule {
        GuardRule {
            id: "test.publish".into(),
            title: "publish".into(),
            hosts: hosts.iter().map(|h| (*h).to_owned()).collect(),
            requests: requests.iter().map(|r| (*r).to_owned()).collect(),
            mode,
            kind: None,
        }
    }

    #[test]
    fn globs_match_any_run() {
        assert!(glob(b"*/git-receive-pack", b"/a/b.git/git-receive-pack"));
        assert!(!glob(b"*/git-receive-pack", b"/a/git-receive-pack/x"));
        assert!(glob(
            b"/*?*service=git-receive-pack*",
            b"/r/info/refs?service=git-receive-pack"
        ));
        assert!(glob(b"*", b""));
        assert!(glob(b"a*b*c", b"aXbYbZc"));
        assert!(!glob(b"a*b*c", b"aXbYbZ"));
    }

    #[test]
    fn guards_see_through_escapes_dots_and_case() {
        let rules = ProxyRules {
            secrets: vec![],
            guards: vec![guard(
                &["crates.io"],
                &["PUT /api/v1/crates/new"],
                GuardMode::Block,
            )],
        };
        for target in [
            "/api/v1/crates/new",
            "/api/v1//crates/./new",
            "/api/v1/x/../crates/%6Eew",
            "/API/V1/CRATES/NEW",
            "https://crates.io/api/v1/crates/new",
        ] {
            assert!(
                rules.guard("crates.io", "put", target).is_some(),
                "{target}"
            );
        }
        assert!(
            rules
                .guard("crates.io", "GET", "/api/v1/crates/new")
                .is_none()
        );
        assert!(
            rules
                .guard("static.crates.io", "PUT", "/api/v1/crates/new")
                .is_none()
        );
        assert!(rules.inspects("crates.io"));
        assert!(!rules.inspects("static.crates.io"));

        let git = ProxyRules {
            secrets: vec![],
            guards: vec![guard(
                &["*"],
                &[
                    "POST */git-receive-pack",
                    "GET */info/refs?*service=git-receive-pack*",
                ],
                GuardMode::Ask,
            )],
        };
        assert!(git.inspects("example.org"));
        assert!(
            git.guard("example.org", "POST", "/me/r.git/git-receive-pack")
                .is_some()
        );
        assert!(
            git.guard(
                "example.org",
                "GET",
                "/me/r.git/info/refs?service=git-receive-pack"
            )
            .is_some()
        );
        assert!(
            git.guard(
                "example.org",
                "GET",
                "/me/r.git/info/refs?service=git-upload-pack"
            )
            .is_none()
        );

        let allowed = ProxyRules {
            secrets: vec![],
            guards: vec![guard(&["*"], &["* /*"], GuardMode::Allow)],
        };
        assert!(allowed.is_empty());
        assert!(!allowed.inspects("example.org"));
        assert!(allowed.guard("example.org", "PUT", "/").is_none());
    }

    #[test]
    fn the_builtin_guards_catch_pushes_and_let_fetches_through() {
        let rules = ProxyRules {
            secrets: vec![],
            guards: builtin_guards(),
        };
        for guard in &rules.guards {
            assert!(
                guard.requests.iter().all(|r| valid_request_pattern(r)),
                "{}",
                guard.id
            );
        }
        let held = |host: &str, method: &str, target: &str| {
            rules.guard(host, method, target).map(|g| g.id.as_str())
        };
        assert_eq!(
            held("codeberg.org", "POST", "/a/b.git/git-receive-pack"),
            Some("cmdterm.git-push")
        );
        assert_eq!(
            held("codeberg.org", "POST", "/a/b.git/git-upload-pack"),
            None
        );
        assert_eq!(
            held("hg.example", "POST", "/repo?cmd=unbundle"),
            Some("cmdterm.hg-push")
        );
        assert_eq!(held("hg.example", "GET", "/repo?cmd=getbundle"), None);
        assert_eq!(
            held("registry.npmjs.org", "PUT", "/left-pad"),
            Some("cmdterm.npm-publish")
        );
        assert_eq!(held("registry.npmjs.org", "GET", "/left-pad"), None);
        assert_eq!(
            held("crates.io", "PUT", "/api/v1/crates/new"),
            Some("cmdterm.cargo-publish")
        );
        assert_eq!(
            held("www.nuget.org", "PUT", "/api/v2/package/"),
            Some("cmdterm.nuget-push")
        );
        assert_eq!(
            held("upload.pypi.org", "POST", "/legacy/"),
            Some("cmdterm.pypi-upload")
        );
        assert_eq!(
            held("ghcr.io", "POST", "/v2/me/app/blobs/uploads/"),
            Some("cmdterm.container-push")
        );
        assert_eq!(held("ghcr.io", "GET", "/v2/me/app/manifests/latest"), None);
    }

    #[test]
    fn request_patterns_need_a_method_and_a_target() {
        assert!(valid_request_pattern("PUT /api/*"));
        assert!(valid_request_pattern("* */git-receive-pack"));
        assert!(valid_request_pattern(
            "POST /graphql *mutation*createPullRequest*"
        ));
        assert!(valid_request_pattern(
            "PATCH /pr/* *\"status\": \"completed\"*"
        ));
        assert!(!valid_request_pattern("PUT"));
        assert!(!valid_request_pattern("PUT /a "));
        assert!(!valid_request_pattern("P-T /a"));
    }

    #[test]
    fn body_patterns_need_the_body() {
        let rules = ProxyRules {
            secrets: vec![],
            guards: vec![guard(
                &["api.github.com"],
                &[
                    "POST /repos/*/pulls",
                    "POST /graphql *mutation*createPullRequest*",
                ],
                GuardMode::Ask,
            )],
        };
        // A head-only pattern decides without the body.
        assert!(
            rules
                .guard("api.github.com", "POST", "/repos/a/b/pulls")
                .is_some()
        );
        assert!(
            rules
                .guard("api.github.com", "POST", "/repos/a/b/pulls?x=1")
                .is_some()
        );
        assert!(
            rules
                .guard("api.github.com", "POST", "/repos/a/b/pulls/1")
                .is_none()
        );
        assert!(!rules.wants_body("api.github.com", "POST", "/repos/a/b/pulls"));
        // GraphQL needs the body.
        assert!(rules.guard("api.github.com", "POST", "/graphql").is_none());
        assert!(rules.wants_body("api.github.com", "POST", "/graphql"));
        assert!(!rules.wants_body("example.org", "POST", "/graphql"));
        let held = |body: &[u8]| {
            rules
                .body_guard("api.github.com", "POST", "/graphql", Some(body))
                .is_some()
        };
        assert!(held(
            br#"{"query":"mutation PullRequestCreate($input:CreatePullRequestInput!){createPullRequest(input:$input){pullRequest{url}}}"}"#
        ));
        // Escapes do not hide it.
        assert!(held(
            br#"{"query":"mutation{\u0063reatePullRequest(input:{})}"}"#
        ));
        assert!(!held(br#"{"query":"query{viewer{login}}"}"#));
        // An unreadable body counts as matching.
        assert!(
            rules
                .body_guard("api.github.com", "POST", "/graphql", None)
                .is_some()
        );
    }

    #[test]
    fn fakes_keep_the_shape_and_stay_the_same() {
        let real = "npm_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789";
        let fake = fake_secret(real);
        assert_ne!(fake, real);
        assert_eq!(fake, fake_secret(real));
        assert!(fake.starts_with("npm_"));
        assert_eq!(fake.len(), real.len());
        for (f, r) in fake.chars().zip(real.chars()) {
            assert_eq!(f.is_ascii_digit(), r.is_ascii_digit());
            assert_eq!(f.is_ascii_uppercase(), r.is_ascii_uppercase());
        }
        let short = fake_secret("hunter2");
        assert_eq!(short.len(), 32);
        assert!(short.bytes().all(|b| b.is_ascii_alphanumeric()));
        assert_ne!(fake_secret("other-secret"), short);
        assert!(
            !format!(
                "{:?}",
                SecretRule {
                    fake: fake.clone(),
                    real: real.into(),
                    hosts: vec![],
                }
            )
            .contains(real)
        );
    }

    #[test]
    fn named_fakes_outlive_their_value() {
        let first = "eyJhbGciOiJSUzI1NiJ9.first-access-token-0123456789";
        let fake = named_fake_secret("fj:git.example.com", first);
        assert_ne!(fake, first);
        assert_eq!(fake.len(), first.len());
        let next = "eyJhbGciOiJSUzI1NiJ9.next-access-token-01234567890";
        assert_eq!(named_fake_secret("fj:git.example.com", next), fake);
        assert_ne!(named_fake_secret("fj:other.example.com", first), fake);
        assert_ne!(fake_secret(first), fake);
    }

    #[test]
    fn fakes_turn_real_only_for_their_hosts() {
        let fake = fake_secret("ghp_0123456789abcdefghijABCDEFGHIJ");
        let rules = ProxyRules {
            secrets: vec![SecretRule {
                fake: fake.clone(),
                real: "ghp_0123456789abcdefghijABCDEFGHIJ".into(),
                hosts: vec!["github.com".into()],
            }],
            guards: vec![],
        };
        assert_eq!(
            rules
                .reveal("github.com", &format!("Bearer {fake}"))
                .as_deref(),
            Some("Bearer ghp_0123456789abcdefghijABCDEFGHIJ")
        );
        assert_eq!(rules.reveal("example.org", &format!("Bearer {fake}")), None);
        assert_eq!(rules.reveal("github.com", "Bearer other"), None);
        let basic = format!("Basic {}", STANDARD.encode(format!("me:{fake}")));
        let revealed = rules.reveal("GitHub.com", &basic).unwrap();
        let decoded = STANDARD
            .decode(revealed.strip_prefix("Basic ").unwrap())
            .unwrap();
        assert_eq!(decoded, b"me:ghp_0123456789abcdefghijABCDEFGHIJ");
    }
}
