//! Links that programs in sandboxes open in the browser on this machine.
//!
//! A sandbox has no way to start the user's browser: its `xdg-open` finds
//! no session, and on macOS Seatbelt keeps LaunchServices out of reach. Yet
//! logins such as Claude Code's `/login` open a page and wait for it. So a
//! sandbox gets the shim in [`SHIM`] as `$BROWSER` and, in bubblewrap,
//! bound over `xdg-open`. It sends the link through the sandbox's proxy to
//! [`cmdterm_env::OPEN_HOST`], and the proxy hands it to [`LinkOpener`].
//!
//! The opener follows the [`cmdterm_env::OPEN_URL_GUARD`]: `ask` shows the
//! link in the approval overlay, under its host, so a host can be allowed
//! for the session or for good in a project. Once allowed, the loopback
//! ports the link names, itself or in a query value such as an OAuth
//! `redirect_uri`, are forwarded from the same port on this machine into
//! the sandbox for [`FORWARD_FOR`], so the browser's redirect back to the
//! program's own server reaches it. Then the system's opener opens it.
//!
//! Everything here is cold: it runs on a proxy thread per link.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use cmdterm_env::{Approvals, GuardMode, GuardRule, OPEN_URL_GUARD};

use crate::runtime::PortHubs;

/// How long a loopback port a link names stays forwarded.
pub const FORWARD_FOR: Duration = Duration::from_secs(600);
/// How long the opener waits for a sandbox server to listen on a port a
/// link names. The port bridge reports listeners twice a second.
const LISTEN_WAIT: Duration = Duration::from_secs(2);
/// Longest link that opens.
const MAX_LINK: usize = 8192;
/// Most loopback ports one link forwards.
const MAX_PORTS: usize = 4;

/// The program that opens links on this machine.
pub const SYSTEM_OPENER: &str = if cfg!(target_os = "macos") {
    "open"
} else if cfg!(windows) {
    "explorer"
} else {
    "xdg-open"
};

/// The shim script, `$BROWSER` in sandboxes and their `xdg-open`.
pub const SHIM: &str = r#"#!/bin/sh
# scriptschnell-term browser shim. A program in a sandbox cannot start your
# browser, so this script asks the app to open the link on this machine,
# through the sandbox's proxy. The app shows the link and opens it once you
# allow it. Only http and https links open.
link=
for arg in "$@"; do
  case $arg in
    --version) echo "xdg-open (scriptschnell-term sandbox)"; exit 0 ;;
    --help|--manual) echo "Usage: ${0##*/} LINK (http and https links open in your browser)"; exit 0 ;;
    -*) ;;
    *) link=$arg; break ;;
  esac
done
case $link in
  [Hh][Tt][Tt][Pp]://*|[Hh][Tt][Tt][Pp][Ss]://*) ;;
  '') echo "${0##*/}: no link given" >&2; exit 1 ;;
  *) echo "${0##*/}: a scriptschnell-term sandbox opens only http and https links, in your browser outside it" >&2; exit 3 ;;
esac
proxy=${http_proxy:-${HTTP_PROXY:-}}
if [ -z "$proxy" ]; then
  echo "${0##*/}: this sandbox has no proxy that reaches scriptschnell-term; open $link yourself" >&2
  exit 3
fi
# Every byte of the link as %XX, so its query keeps its own escapes.
encoded=$(printf '%s' "$link" | od -An -v -tx1 | tr -d ' \n' | sed 's/../%&/g')
request="http://@HOST@/open?url=$encoded"
if command -v curl >/dev/null 2>&1; then
  answer=$(http_proxy=$proxy curl -sS --max-time 200 --fail-with-body "$request" 2>&1)
elif command -v wget >/dev/null 2>&1; then
  answer=$(http_proxy=$proxy wget -q -O - --timeout=200 --tries=1 "$request" 2>&1)
else
  echo "${0##*/}: needs curl or wget; open $link yourself" >&2
  exit 3
fi || { printf '%s\n' "$answer" >&2; exit 4; }
exit 0
"#;

/// Where a sandbox finds the shim, see [`sandbox_shim`].
pub struct Shim {
    /// The directory the sandbox may read, with the shim in it.
    pub dir: PathBuf,
    /// The shim, the value of `$BROWSER`.
    pub path: PathBuf,
    /// This machine's `xdg-open`, for bubblewrap to bind the shim over.
    pub xdg_open: Option<PathBuf>,
}

/// Writes the shim into `cache/open-url` and finds this machine's
/// `xdg-open`. `None` when the file cannot be written.
pub fn sandbox_shim(cache: &Path) -> Option<Shim> {
    let dir = cache.join("open-url");
    std::fs::create_dir_all(&dir).ok()?;
    let dir = cmdterm_env::canonicalize(&dir).ok()?;
    let path = dir.join("xdg-open");
    let content = SHIM.replace("@HOST@", cmdterm_env::OPEN_HOST);
    if std::fs::read(&path).ok().as_deref() != Some(content.as_bytes()) {
        std::fs::write(&path, &content).ok()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).ok()?;
    }
    let xdg_open = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("xdg-open"))
            .find(|p| p.is_file())
            .and_then(|p| cmdterm_env::canonicalize(p).ok())
    });
    Some(Shim {
        dir,
        path,
        xdg_open,
    })
}

/// A link a sandbox asked to open, as far as the opener reads it.
#[derive(Debug, PartialEq, Eq)]
struct Link {
    /// In lowercase, without brackets around an IPv6 address.
    host: String,
    /// Loopback ports the link names, itself or in a query value.
    loopback: Vec<u16>,
}

impl Link {
    fn parse(link: &str) -> Result<Self, String> {
        if link.len() > MAX_LINK {
            return Err(format!("links longer than {MAX_LINK} bytes do not open"));
        }
        if link.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("a link with spaces or control characters does not open".into());
        }
        let (host, port) = authority(link).ok_or("only http and https links open")?;
        let mut loopback = Vec::new();
        if is_loopback(&host) {
            loopback.push(port);
        }
        // A query value that is a loopback link, such as an OAuth
        // redirect_uri, names a port the browser comes back to.
        let query = link.split_once('?').map_or("", |(_, q)| q);
        let query = query.split_once('#').map_or(query, |(q, _)| q);
        for pair in query.split('&') {
            let value = pair.split_once('=').map_or("", |(_, v)| v);
            let value = decode(value);
            if let Some((inner, port)) = authority(&value)
                && is_loopback(&inner)
                && !loopback.contains(&port)
            {
                loopback.push(port);
            }
        }
        loopback.truncate(MAX_PORTS);
        Ok(Self { host, loopback })
    }
}

/// Whether `link` is a well-formed `http` or `https` URL that is safe to
/// hand to the system opener.
pub(crate) fn is_http_url(link: &str) -> bool {
    Link::parse(link).is_ok()
}

/// The host and port of an http or https link.
fn authority(link: &str) -> Option<(String, u16)> {
    let (scheme, rest) = link.split_once("://")?;
    let default = match scheme.to_ascii_lowercase().as_str() {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    // Browsers treat `\` as a path separator in http(s) URLs, so `evil.com\@good.com`
    // would otherwise be read as `good.com` here and opened as `evil.com`.
    if authority.contains('\\') {
        return None;
    }
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => {
            let (host, after) = v6.split_once(']')?;
            (host, after.strip_prefix(':'))
        }
        None => match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        },
    };
    let port = match port {
        Some(port) => port.parse().ok()?,
        None => default,
    };
    (!host.is_empty()).then(|| (host.to_ascii_lowercase(), port))
}

fn is_loopback(host: &str) -> bool {
    host == "localhost" || host.ends_with(".localhost") || host == "::1" || host.starts_with("127.")
}

/// `text` with its `%XX` escapes decoded.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let hex = |b: u8| (b as char).to_digit(16);
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = (bytes[i] == b'%')
            .then(|| Some(hex(*bytes.get(i + 1)?)? * 16 + hex(*bytes.get(i + 2)?)?))
            .flatten();
        match escaped {
            Some(byte) => {
                out.push(byte as u8);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Opens the links of sandboxes, see the module docs.
pub struct LinkOpener {
    approvals: Arc<Approvals>,
    ports: Arc<PortHubs>,
    mode: Mutex<GuardMode>,
    /// Opens a link on this machine; tests record instead.
    launch: fn(&str) -> Result<(), String>,
}

impl LinkOpener {
    pub fn new(approvals: Arc<Approvals>, ports: Arc<PortHubs>) -> Self {
        Self {
            approvals,
            ports,
            mode: Mutex::new(GuardMode::Ask),
            launch: open_on_host,
        }
    }

    /// The user's mode for [`OPEN_URL_GUARD`].
    pub fn set_mode(&self, mode: GuardMode) {
        *self.mode.lock().unwrap_or_else(PoisonError::into_inner) = mode;
    }

    /// Opens `link` for a sandbox of `scope` (a project id), once the
    /// guard lets it, and says what it did. Waits for the user's answer.
    pub fn open(&self, scope: Option<&str>, link: &str) -> Result<String, String> {
        let parsed = Link::parse(link)?;
        let mode = *self.mode.lock().unwrap_or_else(PoisonError::into_inner);
        match mode {
            GuardMode::Block => {
                return Err(format!(
                    "the {OPEN_URL_GUARD} guard blocks links from sandboxes"
                ));
            }
            GuardMode::Ask => {
                let allowed = (self.approvals).ask_about(
                    &guard(),
                    scope,
                    &parsed.host,
                    "OPEN",
                    link,
                    None,
                    None,
                );
                if !allowed {
                    return Err(format!("the user did not allow opening {link}"));
                }
            }
            GuardMode::Allow => {}
        }
        let project = scope.and_then(|scope| scope.parse::<u64>().ok());
        let mut notes = Vec::new();
        if let Some(project) = project {
            for &port in &parsed.loopback {
                notes.extend(self.forward(project, port));
            }
        }
        (self.launch)(link)?;
        notes.insert(0, "opened in the browser outside the sandbox".to_owned());
        Ok(notes.join("; "))
    }

    /// Forwards `port` of the sandbox of `project` that listens on it from
    /// the same port on this machine, for [`FORWARD_FOR`]. Says what went
    /// wrong, if anything did. No sandbox listening on it, the link goes to
    /// this machine's own port.
    fn forward(&self, project: u64, port: u16) -> Option<String> {
        let hubs = self.ports.of_project(project);
        if hubs.is_empty() {
            return None;
        }
        let deadline = Instant::now() + LISTEN_WAIT;
        let (hub, exposed) = loop {
            let found = hubs.iter().find_map(|hub| {
                let listening = hub.ports().into_iter().find(|p| p.port == port)?;
                Some((Arc::clone(hub), listening.exposed))
            });
            match found {
                Some(found) => break found,
                None if Instant::now() >= deadline => return None,
                None => std::thread::sleep(Duration::from_millis(100)),
            }
        };
        match exposed {
            Some(host) if host == port => return None,
            Some(host) => {
                return Some(format!(
                    "port {port} of the sandbox is forwarded from port {host}, not {port}"
                ));
            }
            None => {}
        }
        if let Err(error) = hub.expose(port, Some(port)) {
            return Some(format!(
                "port {port} could not be forwarded into the sandbox: {error}"
            ));
        }
        let _ = std::thread::Builder::new()
            .name("open-url-forward".into())
            .spawn(move || {
                std::thread::sleep(FORWARD_FOR);
                let still = (hub.ports().iter()).any(|p| p.port == port && p.exposed == Some(port));
                if still {
                    hub.unexpose(port);
                }
            });
        None
    }
}

/// The guard questions go under.
fn guard() -> GuardRule {
    (cmdterm_env::builtin_guards().into_iter())
        .find(|guard| guard.id == OPEN_URL_GUARD)
        .unwrap_or_else(|| GuardRule {
            id: OPEN_URL_GUARD.to_owned(),
            title: OPEN_URL_GUARD.to_owned(),
            hosts: Vec::new(),
            requests: Vec::new(),
            mode: GuardMode::Ask,
            kind: None,
        })
}

/// Opens `link` with [`SYSTEM_OPENER`]. An opener that still runs after a
/// few seconds, such as one that started the browser in the foreground,
/// counts as done.
fn open_on_host(link: &str) -> Result<(), String> {
    let command = cmdterm_env::Environment::Local
        .command(SYSTEM_OPENER)
        .arg(link);
    let mut child = (command.to_std())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| format!("{SYSTEM_OPENER} did not start: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            // explorer exits with 1 after it opened a link.
            Ok(Some(status)) if status.success() || cfg!(windows) => return Ok(()),
            Ok(Some(status)) => return Err(format!("{SYSTEM_OPENER} failed ({status})")),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) | Err(_) => {
                let _ = std::thread::Builder::new()
                    .name("open-url-wait".into())
                    .spawn(move || child.wait());
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_name_their_host_and_loopback_ports() {
        let link = Link::parse(
            "https://claude.ai/oauth/authorize?code=true&client_id=x\
             &redirect_uri=http%3A%2F%2Flocalhost%3A54545%2Fcallback&state=s",
        )
        .unwrap();
        assert_eq!(link.host, "claude.ai");
        assert_eq!(link.loopback, [54545]);
        let local = Link::parse("http://127.0.0.1:3000/x?next=http://[::1]:8080/").unwrap();
        assert_eq!(local.loopback, [3000, 8080]);
        assert_eq!(
            Link::parse("HTTPS://User@Example.COM").unwrap().host,
            "example.com"
        );
        assert_eq!(Link::parse("http://localhost/").unwrap().loopback, [80]);
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://a b",
            "https://",
            "https://evil.com\\@trusted.com/",
            "x",
        ] {
            assert!(Link::parse(bad).is_err(), "{bad}");
        }
        assert!(Link::parse(&format!("https://a/{}", "x".repeat(MAX_LINK))).is_err());
    }

    fn recorder(link: &str) -> Result<(), String> {
        OPENED.lock().unwrap().push(link.to_owned());
        Ok(())
    }

    static OPENED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    #[test]
    fn links_open_after_the_user_allows_them_and_follow_the_guards_mode() {
        let approvals = Arc::new(Approvals::new(|| {}));
        let opener = Arc::new(LinkOpener {
            launch: recorder,
            ..LinkOpener::new(Arc::clone(&approvals), Arc::default())
        });
        let ask = |link: &'static str| {
            let opener = Arc::clone(&opener);
            std::thread::spawn(move || opener.open(Some("3"), link))
        };
        let question = || loop {
            if let Some(question) = approvals.pending().into_iter().next() {
                return question;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let allowed = ask("https://claude.ai/login");
        let asked = question();
        assert_eq!(asked.guard, OPEN_URL_GUARD);
        assert_eq!(
            (asked.host.as_str(), asked.target.as_str()),
            ("claude.ai", "https://claude.ai/login")
        );
        assert_eq!(asked.scope.as_deref(), Some("3"));
        approvals.answer(asked.id, true);
        assert!(allowed.join().unwrap().is_ok());
        let refused = ask("https://evil.example/");
        approvals.answer(question().id, false);
        assert!(refused.join().unwrap().is_err());
        assert_eq!(*OPENED.lock().unwrap(), ["https://claude.ai/login"]);

        opener.set_mode(GuardMode::Block);
        assert!(opener.open(None, "https://claude.ai/again").is_err());
        opener.set_mode(GuardMode::Allow);
        assert!(opener.open(None, "https://example.com/").is_ok());
        assert!(opener.open(None, "ftp://example.com/").is_err());
        assert!(approvals.pending().is_empty());
        assert_eq!(OPENED.lock().unwrap().len(), 2);
    }

    #[cfg(unix)]
    static THROUGH_PROXY: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// The shim, the app's proxy and the opener together, as a sandbox
    /// with a proxy endpoint of its project uses them.
    #[cfg(unix)]
    #[test]
    fn a_link_from_the_shim_reaches_the_opener_through_a_real_proxy() {
        let dir = tempfile::tempdir().unwrap();
        let shim = sandbox_shim(dir.path()).unwrap();
        let proxy =
            cmdterm_env::NetworkProxy::start(&dir.path().join("p.sock"), Vec::new()).unwrap();
        let endpoint = proxy
            .endpoint_for("5", &dir.path().join("p5.sock"))
            .unwrap();
        let opener = LinkOpener {
            launch: |link| {
                THROUGH_PROXY.lock().unwrap().push(link.to_owned());
                Ok(())
            },
            ..LinkOpener::new(Arc::new(Approvals::new(|| {})), Arc::default())
        };
        opener.set_mode(GuardMode::Allow);
        let opener = Arc::new(opener);
        proxy.set_opener(Some(Arc::new(move |scope, link| {
            assert_eq!(scope, Some("5"));
            opener.open(scope, link)
        })));
        let link =
            "https://claude.ai/oauth/authorize?redirect_uri=http%3A%2F%2Flocalhost%3A1%2Fcallback";
        let out = // Through sh: executing a file another test thread just wrote
        // can fail with ETXTBSY.
        cmdterm_env::Environment::Local
            .command("sh")
            .arg(&shim.path)
            .arg(link)
            .var("http_proxy", endpoint.url())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(*THROUGH_PROXY.lock().unwrap(), [link]);
    }

    #[cfg(unix)]
    #[test]
    fn the_shim_sends_the_link_through_the_proxy_and_reports_refusals() {
        use std::io::{Read, Write};
        let dir = tempfile::tempdir().unwrap();
        let shim = sandbox_shim(dir.path()).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let mut heads = Vec::new();
            for answer in ["200 OK", "403 Forbidden"] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut head = Vec::new();
                let mut byte = [0u8];
                while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
                    head.push(byte[0]);
                }
                heads.push(String::from_utf8(head).unwrap());
                let body = "cmdterm sandbox: no\n";
                let _ = write!(
                    stream,
                    "HTTP/1.1 {answer}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
            heads
        });
        let run = |link: &str| {
            cmdterm_env::Environment::Local
                .command("sh")
                .arg(&shim.path)
                .arg(link)
                .var("http_proxy", format!("http://127.0.0.1:{port}"))
                .output()
                .unwrap()
        };
        let link = "https://claude.ai/o?a=1&b=%2F";
        assert!(run(link).status.success());
        let refused = run("https://x.example/");
        assert_eq!(refused.status.code(), Some(4));
        assert!(String::from_utf8_lossy(&refused.stderr).contains("cmdterm sandbox: no"));
        let heads = server.join().unwrap();
        let line = heads[0].lines().next().unwrap();
        let encoded = line
            .strip_prefix(&format!("GET http://{}/open?url=", cmdterm_env::OPEN_HOST))
            .and_then(|rest| rest.strip_suffix(" HTTP/1.1"))
            .unwrap_or_else(|| panic!("{line}"));
        assert_eq!(decode(encoded), link);
        let file = run("/etc/passwd");
        assert_eq!(file.status.code(), Some(3));
    }
}
