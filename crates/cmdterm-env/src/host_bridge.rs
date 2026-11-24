//! The proxy bridge on SSH hosts and hosts of extension environments, so
//! their bubblewrap sandboxes, which always keep the host's network, can
//! scope themselves away from its abstract Unix sockets (see `landlock.rs`).
//!
//! Those hosts have no program of ours. [`install_host_bridge`] copies this
//! machine's `scriptschnell-term-proxy-bridge` to
//! `${XDG_CACHE_HOME:-~/.cache}/scriptschnell-term/bin/HASH/` there, with
//! the directory and the file readable by the user alone, when the host
//! runs Linux on this build's architecture. A file with the same SHA-256
//! already there is used as it is; one with another is replaced. The copy
//! must then run there, which an older C library would prevent. Only then
//! do new sandboxes on that host start under it.
//!
//! Each sandbox checks the file's SHA-256 again when it starts
//! ([`SSH_SCOPE`]) and runs without the bridge, with a warning, when it
//! changed. Sandboxes that start before the copy is done start without it.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::Environment;

/// Where a host stands with the bridge.
#[derive(Clone, Debug)]
enum State {
    /// A job copies or checks it.
    Installing,
    /// At `path` on the host, with this SHA-256 in hex.
    Ready { path: String, sha256: String },
    /// None on this host: another system, no bridge here, or a failure.
    Unavailable,
}

fn states() -> &'static Mutex<HashMap<Environment, State>> {
    static STATES: OnceLock<Mutex<HashMap<Environment, State>>> = OnceLock::new();
    STATES.get_or_init(Default::default)
}

/// Whether hosts like `host` get a copy of the bridge: SSH hosts and hosts
/// of extension environments, whose sandboxes keep the host's network.
pub fn takes_host_bridge(host: &Environment) -> bool {
    matches!(host, Environment::Ssh(_) | Environment::Custom(_))
}

/// True the first time it is asked for `host`, when a job should run
/// [`install_host_bridge`] for it; false after, and for hosts that take
/// none. So one job runs per host and app run.
pub fn host_bridge_wanted(host: &Environment) -> bool {
    if !takes_host_bridge(host) {
        return false;
    }
    let mut states = states().lock().unwrap_or_else(|e| e.into_inner());
    if states.contains_key(host) {
        return false;
    }
    states.insert(host.clone(), State::Installing);
    true
}

/// The `scriptschnell-term-proxy-bridge` program of this machine that hosts get a
/// copy of. Only the standalone program, which release builds put beside
/// the app: the app itself, which a `cargo build -p cmdterm` runs as the
/// bridge, is far too large to copy. `None` off Linux.
pub fn local_bridge_program() -> Option<PathBuf> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    match crate::proxy::known_proxy_bridge()? {
        [program] => Some(PathBuf::from(program)),
        _ => None,
    }
}

/// What [`install_host_bridge`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostBridge {
    /// Copied to this path on the host.
    Copied(String),
    /// Already at this path on the host, with the same SHA-256.
    Reused(String),
}

/// Blocking: puts `local`, this machine's bridge, on `host` as the module
/// documentation says, and lets later sandboxes there use it. An error says
/// why the host gets none; its sandboxes then start as before.
pub fn install_host_bridge(host: &Environment, local: &Path) -> Result<HostBridge, String> {
    let result = install(host, local);
    let state = match &result {
        Ok((path, sha256, _)) => State::Ready {
            path: path.clone(),
            sha256: sha256.clone(),
        },
        Err(_) => State::Unavailable,
    };
    states()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(host.clone(), state);
    result.map(|(_, _, done)| done)
}

/// Checks the host's system, then the copy there, and copies when needed.
/// Returns the path there, the SHA-256 and what it did.
fn install(host: &Environment, local: &Path) -> Result<(String, String, HostBridge), String> {
    if !takes_host_bridge(host) {
        return Err("only SSH hosts and extension environments take a copy".into());
    }
    let ours = format!("Linux {}", std::env::consts::ARCH);
    let theirs = text(host.command("uname").arg("-sm").stdout())?;
    if !cfg!(target_os = "linux") || theirs != ours {
        return Err(format!("the host runs {theirs}, this bridge {ours}"));
    }
    if local.file_name() != Some("scriptschnell-term-proxy-bridge".as_ref()) {
        return Err(format!(
            "{} is not a scriptschnell-term-proxy-bridge",
            local.display()
        ));
    }
    let sha256 = text(Environment::Local.command("sha256sum").arg(local).stdout())?;
    let sha256 = sha256.get(..64).unwrap_or_default().to_owned();
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("cannot hash {}", local.display()));
    }
    let answer = text(
        host.command("sh")
            .args(["-c", PROBE, "sh", &sha256[..16], &sha256])
            .stdout(),
    )?;
    let (path, done) = if let Some(path) = answer.strip_prefix("same ") {
        (path.to_owned(), HostBridge::Reused(path.to_owned()))
    } else if let Some(dir) = answer.strip_prefix("copy ") {
        host.copy_in(local, dir, &[])
            .map_err(|error| format!("cannot copy the bridge: {error}"))?;
        let path = format!("{dir}/scriptschnell-term-proxy-bridge");
        (path.clone(), HostBridge::Copied(path))
    } else {
        return Err(format!("unexpected answer from the host: {answer}"));
    };
    let checked = text(
        host.command("sh")
            .args(["-c", CHECK, "sh", &path, &sha256])
            .stdout(),
    )?;
    if checked != "ok" {
        return Err(format!("the copy at {path} does not run there: {checked}"));
    }
    Ok((path, sha256, done))
}

fn text(output: Result<Vec<u8>, crate::CommandError>) -> Result<String, String> {
    let output = output.map_err(|error| error.to_string())?;
    Ok(String::from_utf8_lossy(&output).trim().to_owned())
}

/// `sh -c PROBE sh PREFIX SHA256`: prints `same PATH` when the bridge is in
/// the cache with that SHA-256, else makes its directory, readable by the
/// user alone, removes what is there and prints `copy DIR`.
const PROBE: &str = r#"d=${XDG_CACHE_HOME:-$HOME/.cache}/scriptschnell-term/bin/$1
f=$d/scriptschnell-term-proxy-bridge
if [ -f "$f" ] && [ "$(sha256sum < "$f" | cut -c1-64)" = "$2" ]; then
  echo "same $f"
else
  umask 077 && mkdir -p "$d" && chmod 700 "$d" && rm -f "$f" && echo "copy $d"
fi
"#;

/// `sh -c CHECK sh PATH SHA256`: makes the copy the user's alone, checks
/// its SHA-256 and that it runs, and prints `ok`.
const CHECK: &str = r#"chmod 700 "$1" || exit 1
[ "$(sha256sum < "$1" | cut -c1-64)" = "$2" ] || { echo "its SHA-256 differs"; exit 0; }
out=$("$1" --cmdterm-proxy-bridge -- true 2>&1) && echo ok || echo "$out"
"#;

/// `sh -c SSH_SCOPE sh PATH SHA256 COMMAND...` inside a sandbox: runs the
/// command under the bridge at `PATH` with `--try-scope` when the file
/// still has that SHA-256, else as it is, with a warning.
pub(crate) const SSH_SCOPE: &str = r#"x=$1 h=$2; shift 2
if [ "$(sha256sum < "$x" 2>/dev/null | cut -c1-64)" = "$h" ]; then
  exec "$x" --cmdterm-proxy-bridge --try-scope -- "$@"
fi
echo "scriptschnell-term: $x changed since the app copied it there, so this sandbox starts without it and can reach the host's abstract Unix sockets" >&2
exec "$@"
"#;

/// What goes before the command of a sandbox with the host's network on
/// `host`, when the host has a bridge from [`install_host_bridge`].
pub(crate) fn scope_args(host: &Environment) -> Option<Vec<OsString>> {
    let states = states().lock().unwrap_or_else(|e| e.into_inner());
    let State::Ready { path, sha256 } = states.get(host)? else {
        return None;
    };
    let args = ["sh", "-c", SSH_SCOPE, "sh", path, sha256];
    Some(args.map(OsString::from).to_vec())
}
