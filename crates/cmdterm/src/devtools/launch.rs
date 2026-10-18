//! Finding and starting the browser. It always runs on this machine, with a
//! profile of its own in the app's cache directory, never the user's.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use cmdterm_env::Environment;

use crate::devtools::bidi::Bidi;
use crate::devtools::cdp::Cdp;
use crate::devtools::driver::{Driver, Kind, Sink};
use crate::devtools::{Choice, Config};

/// How long Firefox may take to open its WebSocket.
const STARTUP: Duration = Duration::from_secs(30);
/// Lines of the browser's stderr kept for error messages.
const KEPT_LINES: usize = 12;

const CHROME_NAMES: [&str; 5] = [
    "google-chrome-stable",
    "google-chrome",
    "chromium",
    "chromium-browser",
    "chrome",
];
const CHROME_APPS: [&str; 2] = [
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
];
const FIREFOX_NAMES: [&str; 1] = ["firefox"];
const FIREFOX_APPS: [&str; 1] = ["/Applications/Firefox.app/Contents/MacOS/firefox"];

/// Prefs for the Firefox profile: no first-run pages, no telemetry, no
/// prompts, and WebDriver BiDi only.
const FIREFOX_PREFS: &str = r#"user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("browser.aboutwelcome.enabled", false);
user_pref("browser.tabs.warnOnClose", false);
user_pref("browser.sessionstore.resume_from_crash", false);
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("toolkit.telemetry.reportingpolicy.firstRun", false);
user_pref("remote.active-protocols", 1);
user_pref("network.proxy.allow_hijacking_localhost", true);
"#;

pub struct Launched {
    pub driver: Arc<dyn Driver>,
    pub child: Child,
}

/// The executable on `PATH` named one of `names`, else the first of `apps`
/// that exists.
fn find(names: &[&str], apps: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    apps.iter().map(PathBuf::from).find(|app| app.is_file())
}

/// The browser `config` names and its executable.
pub fn resolve(config: &Config) -> Result<(Kind, PathBuf), String> {
    let chrome = || find(&CHROME_NAMES, &CHROME_APPS).map(|path| (Kind::Chrome, path));
    let firefox = || find(&FIREFOX_NAMES, &FIREFOX_APPS).map(|path| (Kind::Firefox, path));
    if let Some(executable) = &config.executable {
        let kind = match config.browser {
            Choice::Chrome => Kind::Chrome,
            Choice::Firefox => Kind::Firefox,
            Choice::Auto => {
                let name = executable
                    .file_name()
                    .map(|n| n.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                if name.contains("firefox") {
                    Kind::Firefox
                } else {
                    Kind::Chrome
                }
            }
        };
        return Ok((kind, executable.clone()));
    }
    let found = match config.browser {
        Choice::Auto => chrome().or_else(firefox),
        Choice::Chrome => chrome(),
        Choice::Firefox => firefox(),
    };
    found.ok_or_else(|| {
        match config.browser {
            Choice::Auto => "found neither Chrome, Chromium nor Firefox",
            Choice::Chrome => "found neither Chrome nor Chromium",
            Choice::Firefox => "found no Firefox",
        }
        .to_owned()
            + "; set debug_browser.executable in settings.json"
    })
}

pub fn launch(config: &Config, sink: Arc<Sink>) -> Result<Launched, String> {
    let (kind, executable) = resolve(config)?;
    let profile = config.profile_dir.join(match kind {
        Kind::Chrome => "chrome",
        Kind::Firefox => "firefox",
    });
    std::fs::create_dir_all(&profile)
        .map_err(|e| format!("cannot create {}: {e}", profile.display()))?;
    match kind {
        Kind::Chrome => chrome(&executable, &profile, config.headless, sink),
        Kind::Firefox => firefox(&executable, &profile, config.headless, sink),
    }
}

/// Keeps the last lines of `stderr` in `kept`, and sends every line to
/// `lines` while someone listens. The receiver it returns disconnects once
/// all of `stderr` was read, see [`failed`].
fn drain(
    stderr: impl std::io::Read + Send + 'static,
    kept: Arc<Mutex<Vec<String>>>,
    lines: Option<mpsc::Sender<String>>,
) -> mpsc::Receiver<()> {
    let (done, drained) = mpsc::channel::<()>();
    let _ = std::thread::Builder::new()
        .name("debug-browser-stderr".into())
        .spawn(move || {
            let _done = done;
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if let Some(lines) = &lines {
                    let _ = lines.send(line.clone());
                }
                let mut kept = kept.lock().unwrap_or_else(|p| p.into_inner());
                if kept.len() == KEPT_LINES {
                    kept.remove(0);
                }
                kept.push(line);
            }
        });
    drained
}

/// `what`, with the last lines the browser wrote to stderr. Called after
/// the browser was waited for; waits a little for the rest of its stderr,
/// since the line that says why it failed is often the last one.
fn failed(what: String, kept: &Mutex<Vec<String>>, drained: &mpsc::Receiver<()>) -> String {
    let _ = drained.recv_timeout(std::time::Duration::from_secs(2));
    let kept = kept.lock().unwrap_or_else(|p| p.into_inner());
    if kept.is_empty() {
        what
    } else {
        format!("{what}: {}", kept.join(" | "))
    }
}

/// Starts Chrome with the DevTools protocol on its fd 3 and 4. `sh` moves
/// our stdin and stdout pipes there, so no file descriptor juggling is
/// needed here.
fn chrome(
    executable: &Path,
    profile: &Path,
    headless: bool,
    sink: Arc<Sink>,
) -> Result<Launched, String> {
    let mut command = Environment::Local
        .command("sh")
        .arg("-c")
        .arg(r#"exec "$0" "$@" 3<&0 4>&1 </dev/null >/dev/null"#)
        .arg(executable)
        .arg("--remote-debugging-pipe")
        .arg(format!("--user-data-dir={}", profile.display()))
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-sync",
            "--disable-features=Translate,MediaRouter",
            "--password-store=basic",
            "--use-mock-keychain",
            "--window-size=1280,800",
        ]);
    if headless {
        command = command.arg("--headless=new");
    }
    let mut child = command
        .arg("about:blank")
        .to_std()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", executable.display()))?;
    let kept = Arc::new(Mutex::new(Vec::new()));
    let drained = drain(child.stderr.take().expect("piped"), kept.clone(), None);
    let input = child.stdin.take().expect("piped");
    let output = child.stdout.take().expect("piped");
    match Cdp::start(input, output, sink) {
        Ok(cdp) => Ok(Launched {
            driver: Arc::new(cdp),
            child,
        }),
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(failed(
                format!("Chrome did not answer ({error})"),
                &kept,
                &drained,
            ))
        }
    }
}

/// Starts Firefox with WebDriver BiDi on a free loopback port and connects
/// once it printed the address.
fn firefox(
    executable: &Path,
    profile: &Path,
    headless: bool,
    sink: Arc<Sink>,
) -> Result<Launched, String> {
    std::fs::write(profile.join("user.js"), FIREFOX_PREFS)
        .map_err(|e| format!("cannot write the Firefox profile: {e}"))?;
    let mut command = Environment::Local
        .command(executable.as_os_str())
        .arg("--remote-debugging-port=0")
        .arg("--profile")
        .arg(profile)
        .args(["--no-remote", "--new-instance"]);
    if headless {
        command = command.arg("--headless");
    }
    let mut child = command
        .arg("about:blank")
        .to_std()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", executable.display()))?;
    let kept = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = mpsc::channel();
    let drained = drain(child.stderr.take().expect("piped"), kept.clone(), Some(tx));
    let deadline = std::time::Instant::now() + STARTUP;
    let url = loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) => {
                if let Some(at) = line.find("WebDriver BiDi listening on ") {
                    let url = line[at + "WebDriver BiDi listening on ".len()..].trim();
                    break url.to_owned();
                }
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(failed(
                    "Firefox opened no WebDriver BiDi port".into(),
                    &kept,
                    &drained,
                ));
            }
        }
    };
    drop(rx);
    match Bidi::connect(&url, sink) {
        Ok(bidi) => Ok(Launched {
            driver: Arc::new(bidi),
            child,
        }),
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(failed(
                format!("cannot connect to Firefox ({error})"),
                &kept,
                &drained,
            ))
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// The reason a browser did not start is its last stderr line, which a
    /// thread reads; the error waits for it instead of racing it.
    #[test]
    fn a_browser_that_does_not_start_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-chrome");
        let config = Config {
            browser: crate::devtools::Choice::Chrome,
            executable: Some(missing),
            headless: true,
            profile_dir: dir.path().join("profiles"),
        };
        for _ in 0..20 {
            let sink = Arc::new(crate::devtools::driver::Sink::new(|| {}));
            let error = launch(&config, sink).err().expect("no browser there");
            assert!(error.contains("no-chrome"), "{error}");
        }
    }
}
