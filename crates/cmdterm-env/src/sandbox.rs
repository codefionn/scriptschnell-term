//! Commands inside a sandbox on the host environment. [`SandboxKind`] picks
//! the mechanism for the host's operating system.
//!
//! With bubblewrap (`bwrap`, Linux) the sandbox sees the host's filesystem
//! read-only, with fresh `/tmp`, `/dev` and `/proc`. Listed paths stay
//! writable (the worktree), listed secrets are hidden (an empty tmpfs over a
//! directory, `/dev/null` over a file), listed host directories can stand in
//! for sandbox paths (a project's own shell history), and the network can be
//! cut. Every namespace bwrap can unshare is unshared, so processes inside
//! cannot see or signal the host's. Where listed paths nest, the deeper one
//! wins: a hidden directory inside the worktree stays hidden, and a
//! writable folder inside a hidden directory shows through.
//!
//! Seatbelt (`sandbox-exec`, macOS) enforces the same write, hide and network
//! rules with a generated profile, and lets processes signal only each
//! other. It cannot remount paths, so `/tmp` and the per-user temporary
//! directories stay shared and writable and `redirects` are ignored. The
//! profile also denies asking LaunchServices, Apple Events or launchd to
//! start programs, which would run outside it, and connecting to Unix
//! sockets other than the sandbox's own, the passed-in `sockets` and the
//! system's DNS and log sockets. Processes outside it stay out of view, no
//! process's arguments and environment can be read, and the window server
//! is out of reach. `seatbelt.rs` holds these rule groups and leaves out
//! the ones the host's `sandbox-exec` rejects.
//!
//! Instead of the whole network, bubblewrap and Seatbelt sandboxes can get a
//! [`SandboxProxy`]: the sandbox has no network of its own and reaches the
//! hosts a [`crate::NetworkProxy`] allows through it (see `proxy.rs`). A
//! bubblewrap sandbox is isolated, with a loopback of its own, unless it has
//! the host's network (`network` and `host_network`). So servers inside bind
//! no host ports, and a [`crate::PortHub`] (`ports`, see `ports.rs`)
//! forwards them to other sandboxes of the project, and to the host when the
//! user exposes them. A sandbox with the host's network can get a proxy
//! too; then only the proxy variables point to it.
//!
//! bubblewrap can also bind single host files read-only over sandbox files
//! (`files`), such as a config file whose credentials an extension
//! replaced with fakes.
//!
//! On Windows the `cmdterm-sandbox` launcher runs the command with a
//! write-restricted token: writes need a grant to the sandbox's own SID,
//! which the launcher adds to the writable paths. Each project can have its
//! own SID and temporary directory (`profile`), so sandboxes of different
//! projects cannot write each other's paths. `read_only` paths get a deny
//! entry for that SID. Reads and the network cannot be limited this way, so
//! `hidden_dirs`, `hidden_files`, `redirects` and `network` are ignored
//! there. An AppContainer (`appcontainer`) does hide `hidden_dirs` and
//! `hidden_files` that lie inside a path it may read or write.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::proxy::{INNER_CA, INNER_PORTS, INNER_SOCKET, SandboxProxy, proxy_bridge};
use crate::seatbelt::Rules;

/// Where a bubblewrap sandbox sees [`SandboxPolicy::keyring`].
const INNER_BUS: &str = "/tmp/.cmdterm-bus";
use crate::{EnvCommand, Environment};

/// How a sandbox is built on its host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxKind {
    /// bubblewrap, on Linux.
    #[default]
    Bwrap,
    /// `sandbox-exec` with a Seatbelt profile, on macOS.
    Seatbelt,
    /// The `cmdterm-sandbox` launcher, on Windows.
    Windows,
}

impl SandboxKind {
    /// The kind for sandboxes on this machine.
    pub const fn native() -> Self {
        if cfg!(target_os = "macos") {
            Self::Seatbelt
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Bwrap
        }
    }

    /// True when it applies [`SandboxPolicy::redirects`].
    pub fn redirects(self) -> bool {
        self == Self::Bwrap
    }

    /// True when it enforces hidden paths and a cut network.
    pub fn limits_reads(self) -> bool {
        self != Self::Windows
    }
}

/// What a sandbox may touch.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SandboxPolicy {
    #[serde(default)]
    pub kind: SandboxKind,
    /// Paths bound read-write. Missing ones are skipped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writable: Vec<PathBuf>,
    /// Paths inside writable ones that stay read-only, such as an agent's
    /// hooks inside its writable state directory. At the same path they win
    /// over `writable` and `redirects`. One inside a hidden path stays
    /// hidden, unless a writable or readable path below the hidden one
    /// holds it. Missing ones are skipped, so the sandbox could create
    /// them: the caller creates the ones that matter first. Seatbelt denies
    /// writes by path and needs nothing to exist. The Windows launcher
    /// denies the sandbox's SID write, delete and permission changes on the
    /// ones that exist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_only: Vec<PathBuf>,
    /// Read-only paths that bwrap's host creates before the sandbox starts,
    /// when they are missing and their parent directory exists: a directory
    /// for `None`, else a file with that content. For hosts where the
    /// caller cannot create them itself, such as another machine.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub placeholders: Vec<(PathBuf, Option<String>)>,
    /// Directories replaced by an empty tmpfs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden_dirs: Vec<PathBuf>,
    /// Files replaced by `/dev/null`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden_files: Vec<PathBuf>,
    /// Paths bound read-only at their own place, which show through a
    /// hidden directory they lie in, such as the app's references inside
    /// its hidden configuration directory. Missing ones are skipped.
    /// The Windows launcher gives an AppContainer read access to them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readable: Vec<PathBuf>,
    /// Host directories bound read-write in place of a sandbox path, as
    /// `(host, sandbox)`. They win over the read-only root and over the
    /// hidden directories they lie in. The sandbox path must exist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redirects: Vec<(PathBuf, PathBuf)>,
    /// Host files bound read-only in place of a sandbox file, as `(host,
    /// sandbox)`. At the same path they win over `redirects` and the
    /// hidden ones. The sandbox path must exist.
    /// Only bubblewrap applies them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<(PathBuf, PathBuf)>,
    /// Host files bound read-write in place of a sandbox file, as `(host,
    /// sandbox)`, winning over `files` at the same path: the copies of
    /// coding agents' settings that the app brings back to the host after asking. Nothing
    /// else is mounted at the sandbox path, since a mount stacked on a bind
    /// of a host file comes off when the host renames a new file over that
    /// one, and agents write their settings that way. The sandbox path must
    /// exist, unless it lies in `scratch_home`. Only bubblewrap applies
    /// them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub synced: Vec<(PathBuf, PathBuf)>,
    /// A directory, the home directory, that the sandbox sees as an empty
    /// tmpfs with every entry of the host's bound back read-only and its
    /// symbolic links made again. The sandbox can then create new files
    /// there, such as the lock and temporary files an agent writes next to
    /// its state file, but they vanish with it. It is mounted first, so
    /// the other paths apply on top. Only bubblewrap applies it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratch_home: Option<PathBuf>,
    /// Directories, such as `/run`, that the sandbox sees as an empty
    /// tmpfs with every entry the host had at the start bound back
    /// read-only and its symbolic links made again. Entries the host adds
    /// later, such as the socket of a container engine started after the
    /// sandbox, stay out of it. They are mounted first, like
    /// `scratch_home`. Only bubblewrap applies them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scratch_dirs: Vec<PathBuf>,
    /// Unix sockets the sandbox may connect to at their own place, such as
    /// a container engine's API (see `engines.rs`). They show through the
    /// hidden directories they lie in. Missing ones are skipped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sockets: Vec<PathBuf>,
    /// Named pipes the Windows launcher relays, as `(variable, address)`,
    /// such as `("DOCKER_HOST", "npipe:////./pipe/docker_engine")`: a pipe
    /// the sandbox may write stands in for the addressed one, and the
    /// variable gets its address in the same form. Pipes that do not exist
    /// are skipped. Only the Windows launcher applies them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pipes: Vec<(String, String)>,
    /// Variables removed from the environment, such as `SSH_AUTH_SOCK`.
    /// Only inherited ones: a variable that `vars`, the proxy or the command
    /// sets keeps that value. See [`Self::unset_credentials`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unset: Vec<String>,
    /// Variables set inside, such as `HISTFILE`. The command's own
    /// variables override them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vars: Vec<(String, String)>,
    #[serde(default = "yes")]
    pub network: bool,
    /// With `network`, share the host's network namespace instead of
    /// running isolated behind the proxy. Only bubblewrap reads it. Servers
    /// inside then bind the host's ports, and `ports` is useless. Here and
    /// on cmdterm-remote servers and VMs a bridge keeps such a sandbox from
    /// the host's abstract Unix sockets with Landlock, where the kernel has
    /// it (see `landlock.rs`).
    #[serde(default)]
    pub host_network: bool,
    /// A filtering proxy. Without `network` it is the only way out. With
    /// `network` and `host_network` the sandbox keeps the host's network
    /// and the proxy variables send HTTP clients through it, for its
    /// secrets and guards. With `network` but not `host_network` the
    /// sandbox is isolated and always has the proxy: only proxy-aware
    /// traffic (HTTP, HTTPS, CONNECT) leaves. On Windows the launcher only
    /// gets the proxy variables, with the CA bundle at its host path. An
    /// AppContainer (`appcontainer`) reaches the proxy's loopback port when
    /// the loopback exemption holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<SandboxProxy>,
    /// The socket of a port hub (`ports.rs`). An isolated bubblewrap
    /// sandbox gets it at a fixed path, and the bridge reports the ports
    /// the sandbox listens on through it. Other kinds ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<PathBuf>,
    /// The socket of a keyring bus (`keyring.rs`) that stands in for the
    /// session bus: bubblewrap binds it at a fixed path and points
    /// `DBUS_SESSION_BUS_ADDRESS` there. Seatbelt and Windows ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyring: Option<PathBuf>,
    /// The name of the AppContainer profile (`cmdterm-sandbox-<id>`) the
    /// Windows launcher runs the command in, instead of under a
    /// write-restricted token. Experimental. The launcher falls back to the
    /// token when it cannot set the AppContainer up. `readable` paths get
    /// read and execute grants for it, `writable` ones the usual grant.
    /// Other kinds ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub appcontainer: Option<String>,
    /// The loopback TCP port and access token of a port hub (`ports.rs`),
    /// for the Windows launcher's bridge, since an AppContainer cannot use
    /// the hub's Unix socket. Other kinds ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports_tcp: Option<(u16, String)>,
    /// A name for the Windows launcher's restricted-token SID and temporary
    /// directory (`%TEMP%\cmdterm-sandbox\NAME`), so sandboxes of different
    /// projects cannot write each other's paths. Without it every sandbox
    /// shares the legacy SID and temporary directory. Other kinds ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// With `Some(keep)`, a `sh` on the host removes every variable it
    /// inherited that looks like a credential, but those named in `keep`,
    /// before it starts bwrap (see [`credential_filter`]). For hosts whose
    /// environment the app cannot see, such as another machine; on this
    /// one [`Self::unset_credentials`] does the same through `unset`.
    /// Variables the policy or the command set come after it. Only
    /// bubblewrap applies it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strip_credentials: Option<Vec<String>>,
}

fn yes() -> bool {
    true
}

/// Parts of a variable name that mark a credential, such as `GITHUB_TOKEN`,
/// `AWS_SECRET_ACCESS_KEY` or `OPENAI_API_KEY`.
const CREDENTIAL_PARTS: [&str; 9] = [
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "API_KEY",
    "APIKEY",
    "ACCESS_KEY",
    "PRIVATE_KEY",
    "CREDENTIAL",
];

/// AWS's session credentials, named even though the parts above match them,
/// and the Azure DevOps token the app reads, which they do not.
const CREDENTIAL_NAMES: [&str; 5] = [
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_SECURITY_TOKEN",
    "AZURE_DEVOPS_EXT_PAT",
];

/// A word of a name (between underscores) that holds this marks a
/// credential, unless it also holds [`NOT_AUTH`] but not [`STILL_AUTH`].
const AUTH: &str = "AUTH";
const NOT_AUTH: &str = "AUTHOR";
const STILL_AUTH: &str = "AUTHORIZ";

/// Whether the variable `name` looks like it holds a credential, judged on
/// the name alone and ignoring case: it contains one of
/// [`CREDENTIAL_PARTS`], is one of [`CREDENTIAL_NAMES`], or has a word
/// (between underscores) with `AUTH` in it, such as `NPM_CONFIG__AUTH` or
/// `GOOGLE_OAUTH_ACCESS_TOKEN`. Words with `AUTHOR` in them do not count
/// unless they say `AUTHORIZATION`, so `GIT_AUTHOR_NAME` and `XAUTHORITY`
/// stay. [`credential_filter`] applies the same rule in `sh`.
fn looks_like_credential(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    let auth =
        |word: &str| word.contains(AUTH) && (!word.contains(NOT_AUTH) || word.contains(STILL_AUTH));
    CREDENTIAL_NAMES.contains(&upper.as_str())
        || CREDENTIAL_PARTS.iter().any(|part| upper.contains(part))
        || upper.split(|c: char| !c.is_ascii_alphanumeric()).any(auth)
}

/// `text` as a `case` pattern that ignores case: `[Aa][Uu]...`. Only
/// letters, digits and underscores come through; the callers pass nothing
/// else.
fn any_case(text: &str) -> String {
    let mut pattern = String::new();
    for c in text
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
    {
        if c.is_ascii_alphabetic() {
            pattern.push('[');
            pattern.push(c.to_ascii_uppercase());
            pattern.push(c.to_ascii_lowercase());
            pattern.push(']');
        } else {
            pattern.push(c);
        }
    }
    pattern
}

/// Whether `name` can be a shell variable's name.
fn is_shell_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `sh` that unsets every inherited variable whose name
/// [`looks_like_credential`], except the names in `keep` (ignoring case),
/// for a host whose environment the app cannot see. The patterns come from
/// the same constants as [`looks_like_credential`]. awk lists the names, so
/// values with newlines cannot pass for names. Without awk the sandbox does
/// not start.
fn credential_filter(keep: &[String]) -> String {
    let alternatives = |patterns: Vec<String>| patterns.join("|");
    let parts = (CREDENTIAL_PARTS.iter()).map(|part| format!("*{}*", any_case(part)));
    let names = CREDENTIAL_NAMES.iter().map(|name| any_case(name));
    let credential = alternatives(parts.chain(names).collect());
    let kept: Vec<String> = (keep.iter())
        .filter(|name| is_shell_name(name))
        .map(|name| any_case(name))
        .collect();
    let keep_line = if kept.is_empty() {
        String::new()
    } else {
        format!(
            "  case $cmdterm_n in {}) continue ;; esac\n",
            alternatives(kept)
        )
    };
    format!(
        r#"cmdterm_names=$(awk 'BEGIN {{ for (k in ENVIRON) print k }}') || {{
  echo 'cmdterm sandbox: awk is missing, so credentials cannot be removed' >&2
  exit 127
}}
set -f
IFS='
'
for cmdterm_n in $cmdterm_names; do
  [ -z "$cmdterm_n" ] && continue
  cmdterm_odd_name=
  case $cmdterm_n in [0-9]*|*[!A-Za-z0-9_]*) cmdterm_odd_name=1 ;; esac
{keep_line}  cmdterm_c=
  case $cmdterm_n in {credential}) cmdterm_c=1 ;; esac
  cmdterm_w=$cmdterm_n
  while [ -z "$cmdterm_c" ]; do
    cmdterm_word=${{cmdterm_w%%_*}}
    case $cmdterm_word in *{still}*) cmdterm_c=1 ;; *{not}*) ;; *{auth}*) cmdterm_c=1 ;; esac
    [ "$cmdterm_w" = "$cmdterm_word" ] && break
    cmdterm_w=${{cmdterm_w#*_}}
  done
  [ -z "$cmdterm_c" ] && continue
  if [ -n "$cmdterm_odd_name" ]; then
    cmdterm_odd="$cmdterm_odd$cmdterm_n
"
  else
    unset "$cmdterm_n"
  fi
done
unset IFS cmdterm_names cmdterm_n cmdterm_c cmdterm_w cmdterm_word cmdterm_odd_name
set +f
"#,
        still = any_case(STILL_AUTH),
        not = any_case(NOT_AUTH),
        auth = any_case(AUTH),
    )
}

impl SandboxPolicy {
    /// A policy for this machine, of its [`SandboxKind::native`] kind:
    /// `hidden` paths are sorted into directories and files by looking at
    /// them, and missing ones are dropped, because bwrap cannot mount over a
    /// path that does not exist.
    pub fn local(writable: Vec<PathBuf>, hidden: &[PathBuf], network: bool) -> Self {
        let mut policy = Self {
            kind: SandboxKind::native(),
            writable,
            read_only: Vec::new(),
            placeholders: Vec::new(),
            hidden_dirs: Vec::new(),
            hidden_files: Vec::new(),
            readable: Vec::new(),
            redirects: Vec::new(),
            files: Vec::new(),
            synced: Vec::new(),
            scratch_home: None,
            scratch_dirs: Vec::new(),
            sockets: Vec::new(),
            pipes: Vec::new(),
            unset: vec!["SSH_AUTH_SOCK".into(), "GPG_AGENT_INFO".into()],
            vars: Vec::new(),
            network,
            host_network: false,
            proxy: None,
            ports: None,
            keyring: None,
            appcontainer: None,
            ports_tcp: None,
            profile: None,
            strip_credentials: None,
        };
        for path in hidden {
            // bwrap cannot mount over a symlink, so the mount goes on its
            // target, which the link then leads to.
            let Ok(path) = crate::canonicalize(path) else {
                continue;
            };
            let list = if path.is_dir() {
                &mut policy.hidden_dirs
            } else {
                &mut policy.hidden_files
            };
            if !list.contains(&path) {
                list.push(path);
            }
        }
        policy
    }

    /// Adds to [`Self::unset`] every name in `inherited` that looks like a
    /// credential (see [`looks_like_credential`]), except those in `keep`,
    /// which are compared ignoring case. `inherited` is the environment the
    /// sandbox program starts with, usually the app's own. Variables the
    /// policy or the command set are not affected: they are set after the
    /// unset ones are removed. Names that are not UTF-8 are skipped.
    pub fn unset_credentials<S: AsRef<str>>(
        &mut self,
        inherited: impl IntoIterator<Item = OsString>,
        keep: &[S],
    ) {
        for name in inherited {
            let Some(name) = name.to_str() else {
                continue;
            };
            let kept = keep.iter().any(|k| k.as_ref().eq_ignore_ascii_case(name));
            if !kept && looks_like_credential(name) && !self.unset.iter().any(|u| u == name) {
                self.unset.push(name.to_owned());
            }
        }
    }

    /// True when `path` lies in a hidden path and no writable, readable or
    /// redirected path below that one holds it, so a read-only bind of it
    /// would uncover it.
    fn stays_hidden(&self, path: &Path) -> bool {
        let depth = |p: &PathBuf| p.components().count();
        let hidden = (self.hidden_dirs.iter())
            .filter(|dir| path.starts_with(dir))
            .chain(self.hidden_files.iter().filter(|file| *file == path))
            .map(depth)
            .max();
        let Some(hidden) = hidden else {
            return false;
        };
        let shown = (self.writable.iter())
            .chain(&self.readable)
            .chain(self.redirects.iter().map(|(_, inside)| inside))
            .filter(|p| path.starts_with(p))
            .map(depth)
            .max();
        shown.is_none_or(|shown| shown < hidden)
    }

    /// Whether bubblewrap gives the sandbox a network namespace of its own:
    /// it does unless the sandbox has the host's network.
    fn isolated(&self) -> bool {
        !(self.network && self.host_network)
    }

    /// bwrap arguments up to, not including, the command.
    fn args(&self, pty: bool) -> Vec<OsString> {
        let mut args: Vec<OsString> = ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"]
            .into_iter()
            .map(Into::into)
            .collect();
        args.extend(["--tmpfs".into(), "/tmp".into()]);
        let synced = |path: &PathBuf| self.synced.iter().any(|(_, inside)| inside == path);
        for dir in &self.scratch_dirs {
            scratch_home_args(dir, &|_| false, &mut args);
        }
        if let Some(home) = &self.scratch_home {
            scratch_home_args(home, &synced, &mut args);
        }
        // Each mount covers what was mounted below it before, so a deeper
        // path goes after a shallower one: a writable worktree does not
        // uncover the hidden directory inside it, and a project folder
        // inside a hidden directory shows through. The sort is stable, so
        // at one path the order of the lists below decides.
        let mut mounts: Vec<(usize, [OsString; 3])> = Vec::new();
        let mut mount = |flag: &str, from: &Path, at: &Path| {
            let triple = [flag.into(), from.as_os_str().into(), at.as_os_str().into()];
            mounts.push((at.components().count(), triple));
        };
        for dir in &self.hidden_dirs {
            // `--tmpfs` takes one path; the second is dropped below.
            mount("--tmpfs", dir, dir);
        }
        for file in &self.hidden_files {
            mount("--ro-bind", Path::new("/dev/null"), file);
        }
        for path in &self.readable {
            mount("--ro-bind-try", path, path);
        }
        for path in self.writable.iter().filter(|p| !synced(p)) {
            mount("--bind-try", path, path);
        }
        for (host, inside) in &self.redirects {
            mount("--bind", host, inside);
        }
        for path in self
            .read_only
            .iter()
            .filter(|p| !synced(p) && !self.stays_hidden(p))
        {
            mount("--ro-bind-try", path, path);
        }
        for (host, inside) in &self.files {
            mount("--ro-bind", host, inside);
        }
        for (host, inside) in &self.synced {
            mount("--bind", host, inside);
        }
        for socket in &self.sockets {
            mount("--bind-try", socket, socket);
        }
        mounts.sort_by_key(|(depth, _)| *depth);
        for (_, [flag, from, at]) in mounts {
            let tmpfs = flag == "--tmpfs";
            args.push(flag);
            if !tmpfs {
                args.push(from);
            }
            args.push(at);
        }
        let isolated = self.isolated();
        if let Some(ports) = self.ports.as_ref().filter(|_| isolated) {
            args.push("--bind".into());
            args.push(ports.clone().into_os_string());
            args.push(INNER_PORTS.into());
        }
        if let Some(proxy) = &self.proxy {
            // With the host's network the sandbox reaches the proxy's TCP
            // port.
            if isolated {
                args.push("--bind".into());
                args.push(proxy.socket.clone().into_os_string());
                args.push(INNER_SOCKET.into());
            }
            if let Some(bundle) = &proxy.ca_bundle {
                args.push("--ro-bind".into());
                args.push(bundle.clone().into_os_string());
                args.push(INNER_CA.into());
            }
        }
        if let Some(socket) = &self.keyring {
            args.push("--bind".into());
            args.push(socket.clone().into_os_string());
            args.push(INNER_BUS.into());
        }
        args.push("--unshare-all".into());
        if !isolated {
            args.push("--share-net".into());
        }
        args.push("--die-with-parent".into());
        // A new session blocks TIOCSTI injection into the caller's terminal,
        // but it also takes job control from an interactive shell. Kernels
        // since 6.2 refuse TIOCSTI anyway, so PTYs keep their session.
        if !pty {
            args.push("--new-session".into());
        }
        for var in &self.unset {
            args.push("--unsetenv".into());
            args.push(var.into());
        }
        // `self.vars` and the proxy's reach bwrap through its environment,
        // see `SandboxTarget::lower_bwrap`.
        if self.keyring.is_some() {
            args.push("--setenv".into());
            args.push("DBUS_SESSION_BUS_ADDRESS".into());
            args.push(format!("unix:path={INNER_BUS}").into());
            // The keyring bus passes only the Secret Service, so GIO's
            // attempt to start the GVFS daemon fails and prints a warning
            // into the terminal of every GTK program, fastfetch included.
            // Local file access needs no daemon.
            if !self.vars.iter().any(|(key, _)| key == "GIO_USE_VFS") {
                args.push("--setenv".into());
                args.push("GIO_USE_VFS".into());
                args.push("local".into());
            }
        }
        args
    }

    /// The variables that point HTTP clients to the proxy, if there is one.
    /// bwrap mounts the CA bundle at a fixed path; Seatbelt and the Windows
    /// launcher read it in place.
    ///
    /// On Windows, git with the schannel backend checks certificates against
    /// the Windows store and ignores `GIT_SSL_CAINFO`, so while there is a
    /// bundle git is switched to its OpenSSL backend, which reads it. The
    /// command's own `GIT_CONFIG_COUNT` replaces this one.
    fn proxy_vars(&self) -> Vec<(String, String)> {
        let Some(proxy) = &self.proxy else {
            return Vec::new();
        };
        let ca = proxy.ca_bundle.as_ref().map(|bundle| match self.kind {
            SandboxKind::Bwrap => INNER_CA.to_owned(),
            _ => bundle.to_string_lossy().into_owned(),
        });
        let mut vars = proxy.vars(ca.as_deref());
        if self.kind == SandboxKind::Windows && ca.is_some() {
            for (key, value) in [
                ("GIT_CONFIG_COUNT", "1"),
                ("GIT_CONFIG_KEY_0", "http.sslBackend"),
                ("GIT_CONFIG_VALUE_0", "openssl"),
            ] {
                vars.push((key.to_owned(), value.to_owned()));
            }
        }
        vars
    }

    /// Stand-ins for the host's shell startup files that set proxy
    /// variables, as `(sandbox path, content)`: the file as it is, then
    /// the sandbox's own proxy variables again. A login shell, such as the
    /// `bash -lc` Codex runs commands in, reads `/etc/profile` and would
    /// otherwise send HTTP clients to the host's proxy, around this one.
    /// Only bubblewrap binds files, and only a sandbox with a proxy needs
    /// them. The caller writes them and adds them to [`Self::files`].
    pub fn shell_startup_stand_ins(&self) -> Vec<(PathBuf, String)> {
        if self.kind != SandboxKind::Bwrap || self.proxy.is_none() {
            return Vec::new();
        }
        shell_startup_stand_ins(Path::new("/"), &self.proxy_vars())
    }

    /// `sandbox-exec` arguments up to, not including, the command. Paths
    /// reach the profile as `-D` parameters, so they need no quoting.
    /// Seatbelt matches resolved paths (`/private/var`, not `/var`), and the
    /// last matching rule wins.
    ///
    /// The profile allows what it does not deny; the groups of denies are in
    /// `seatbelt.rs`, and `rules` picks which of them it has.
    pub(crate) fn seatbelt_args(&self, rules: Rules) -> Vec<OsString> {
        let mut profile = String::from(
            "(version 1)\n(allow default)\n(deny file-write*)\n\
             (allow file-write* (subpath \"/dev\") (subpath \"/private/tmp\") \
             (subpath \"/private/var/folders\"))\n",
        );
        rules.push_text(&mut profile);
        // The last matching rule wins, so deeper paths go after shallower
        // ones, as bubblewrap mounts them: a hidden directory inside a
        // writable one stays hidden, and a writable one inside a hidden
        // one, such as the project's shell history in the app's state
        // directory, can be read and written. The sort is stable, so at
        // one path the order below decides, the same as bubblewrap's.
        // Each rule is a file rule and, where sockets matter, a Unix socket
        // rule on the same path: hidden paths deny connecting to sockets in
        // them, writable paths and passed-in sockets (the app's control
        // socket, container engines) allow it.
        let hide = "deny file-read* file-write*";
        let open = "allow file-read* file-write*";
        let sockets = rules.has(Rules::UNIX_SOCKETS);
        let deny = sockets.then_some("deny network-outbound");
        let allow = sockets.then_some("allow network-outbound");
        let mut rules: Vec<(&str, &str, Option<&str>, PathBuf)> = Vec::new();
        for dir in &self.hidden_dirs {
            rules.push((hide, "subpath", deny, resolved(dir)));
        }
        for file in &self.hidden_files {
            rules.push((hide, "literal", deny, resolved(file)));
        }
        for path in &self.readable {
            rules.push(("allow file-read*", "subpath", None, resolved(path)));
        }
        for path in &self.writable {
            rules.push((open, "subpath", allow, resolved(path)));
        }
        for path in &self.read_only {
            rules.push(("deny file-write*", "subpath", None, resolved(path)));
        }
        for socket in &self.sockets {
            rules.push((open, "literal", allow, resolved(socket)));
        }
        // The bundle may sit in a hidden directory, such as the config's.
        if let Some(bundle) = self.proxy.as_ref().and_then(|p| p.ca_bundle.as_ref()) {
            rules.push(("allow file-read*", "literal", None, resolved(bundle)));
        }
        rules.sort_by_key(|(_, _, _, path)| path.components().count());
        let mut params = Vec::new();
        for (index, (rule, filter, sockets, path)) in rules.into_iter().enumerate() {
            let _ = writeln!(profile, "({rule} ({filter} (param \"P{index}\")))");
            if let Some(sockets) = sockets {
                let _ = writeln!(
                    profile,
                    "({sockets} (remote unix-socket (subpath (param \"P{index}\"))))"
                );
            }
            let mut define = OsString::from(format!("P{index}="));
            define.push(path);
            params.push(OsString::from("-D"));
            params.push(define);
        }
        if !self.network {
            profile.push_str(
                "(deny network-outbound (remote ip \"*:*\"))\n\
                 (deny network-inbound (local ip \"*:*\"))\n\
                 (deny network-bind (local ip \"*:*\"))\n",
            );
        }
        if let Some(proxy) = self.proxy.as_ref().filter(|_| !self.network) {
            let _ = writeln!(
                profile,
                "(allow network-outbound (remote ip \"localhost:{}\"))",
                proxy.port
            );
        }
        profile.push_str("(deny signal)\n(allow signal (target same-sandbox))\n");
        let mut args = vec![OsString::from("-p"), profile.into()];
        args.extend(params);
        args
    }

    /// `cmdterm-sandbox` arguments up to, not including, the command, whose
    /// variables are `command_vars`.
    fn launcher_args(&self, command_vars: &[(OsString, OsString)]) -> Vec<OsString> {
        let mut args = Vec::new();
        for path in &self.writable {
            args.push("--write".into());
            args.push(path.clone().into_os_string());
        }
        // The launcher drops an unset name from the variables it sets too,
        // so a name that the policy, the proxy or the command sets is left
        // out here. Windows names ignore case.
        let proxy_vars = self.proxy_vars();
        let set = |name: &str| {
            let same = |key: &str| key.eq_ignore_ascii_case(name);
            (self.vars.iter().chain(&proxy_vars)).any(|(key, _)| same(key))
                || (command_vars.iter()).any(|(key, _)| key.to_str().is_some_and(same))
        };
        for var in self.unset.iter().filter(|var| !set(var)) {
            args.push("--unset".into());
            args.push(var.into());
        }
        for (key, value) in &self.vars {
            args.push("--set".into());
            args.push(format!("{key}={value}").into());
        }
        // The proxy's variables come before the command's own, which
        // `lower_windows` adds, so the command wins.
        for (key, value) in &proxy_vars {
            args.push("--set".into());
            args.push(format!("{key}={value}").into());
        }
        for (var, pipe) in &self.pipes {
            args.push("--pipe".into());
            args.push(format!("{var}={pipe}").into());
        }
        if let Some(name) = &self.profile {
            args.push("--profile".into());
            args.push(name.into());
        }
        for path in &self.read_only {
            args.push("--read-only".into());
            args.push(path.clone().into_os_string());
        }
        if let Some(name) = &self.appcontainer {
            args.push("--appcontainer".into());
            args.push(name.into());
            for path in self.hidden_dirs.iter().chain(&self.hidden_files) {
                args.push("--hide".into());
                args.push(path.clone().into_os_string());
            }
            for path in &self.readable {
                args.push("--read".into());
                args.push(path.clone().into_os_string());
            }
            if let Some((port, token)) = &self.ports_tcp {
                args.push("--ports-tcp".into());
                args.push(format!("{port}:{token}").into());
            }
        }
        args
    }
}

/// bwrap arguments for [`SandboxPolicy::scratch_home`]: a tmpfs over
/// `home`, then each of its entries as the host has it, sorted by name,
/// but the `synced` ones, whose copies go straight onto the tmpfs. A home
/// it cannot list stays as the host has it.
fn scratch_home_args(home: &Path, synced: &dyn Fn(&PathBuf) -> bool, args: &mut Vec<OsString>) {
    let Ok(entries) = std::fs::read_dir(home) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    args.push("--tmpfs".into());
    args.push(home.as_os_str().to_owned());
    for entry in entries {
        let path = entry.path();
        if synced(&path) {
            continue;
        }
        // A bind would follow a link and lose it, and paths the policy
        // mounts at the link's target would no longer show through it.
        if entry.file_type().is_ok_and(|kind| kind.is_symlink()) {
            if let Ok(target) = std::fs::read_link(&path) {
                args.push("--symlink".into());
                args.push(target.into_os_string());
                args.push(path.into_os_string());
            }
            continue;
        }
        args.push("--ro-bind-try".into());
        args.push(path.clone().into_os_string());
        args.push(path.into_os_string());
    }
}

/// `path` with symlinks resolved, as far as it exists.
fn resolved(path: &Path) -> PathBuf {
    if let Ok(path) = crate::canonicalize(path) {
        return path;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => resolved(parent).join(name),
        _ => path.to_path_buf(),
    }
}

/// bwrap's `args` without the `--unsetenv` of a name in `carried`, which the
/// environment bwrap starts with sets.
fn without_unsets(args: Vec<OsString>, carried: &[(OsString, OsString)]) -> Vec<OsString> {
    let mut out = Vec::with_capacity(args.len());
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--unsetenv" {
            let name = args.next().unwrap_or_default();
            if !carried.iter().any(|(key, _)| *key == name) {
                out.push(arg);
                out.push(name);
            }
            continue;
        }
        out.push(arg);
    }
    out
}

/// `lowered` with the sockets `command` forwards, so the host forwards
/// them to the path the sandbox binds.
fn forward_sockets(mut lowered: EnvCommand, command: &EnvCommand) -> EnvCommand {
    lowered.sockets.extend(command.sockets.iter().cloned());
    lowered
}

static LAUNCHER: OnceLock<PathBuf> = OnceLock::new();

/// Sets the `cmdterm-sandbox` executable for this process. Only the first
/// call counts.
pub fn set_sandbox_launcher(program: PathBuf) {
    let _ = LAUNCHER.set(program);
}

/// What [`set_sandbox_launcher`] set, otherwise a `cmdterm-sandbox` next to
/// the running executable, otherwise the one on `PATH`.
fn launcher() -> &'static Path {
    LAUNCHER.get_or_init(|| {
        let name = format!("cmdterm-sandbox{}", std::env::consts::EXE_SUFFIX);
        std::env::current_exe()
            .ok()
            .and_then(|exe| Some(exe.parent()?.join(&name)))
            .filter(|path| path.is_file())
            .unwrap_or_else(|| PathBuf::from(name))
    })
}

/// A command for the `cmdterm-sandbox` launcher, parsed from its arguments:
/// `[--write PATH] [--read PATH] [--unset VAR] [--set KEY=VALUE]
/// [--pipe VAR=PIPE] [--profile NAME] [--read-only PATH] [--appcontainer NAME]
/// [--hide PATH] [--ports-tcp PORT:TOKEN] [--cwd DIR] -- PROGRAM [ARG]...`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SandboxLaunch {
    pub writable: Vec<PathBuf>,
    pub unset: Vec<OsString>,
    pub vars: Vec<(OsString, OsString)>,
    /// See [`SandboxPolicy::pipes`].
    pub pipes: Vec<(OsString, OsString)>,
    /// Paths an AppContainer may read and run from (`--read`).
    pub readable: Vec<PathBuf>,
    /// The AppContainer profile name (`--appcontainer`). Without it the
    /// launcher uses a restricted token.
    pub appcontainer: Option<OsString>,
    /// The port hub's loopback TCP port and token (`--ports-tcp`).
    pub ports_tcp: Option<(u16, String)>,
    /// The name the capability SID and temporary directory come from
    /// (`--profile`). Without it the launcher uses the legacy name.
    pub profile: Option<OsString>,
    /// Paths the sandbox may not change (`--read-only`).
    pub read_only: Vec<PathBuf>,
    /// Paths an AppContainer may not touch (`--hide`). Only those inside a
    /// path it may read or write are denied.
    pub hidden: Vec<PathBuf>,
    /// The CPU cap of the sandbox's job (`--cpu RESERVE:PERCENT`), which
    /// cannot join the app's slice.
    pub cpu: Option<crate::CpuShare>,
    pub cwd: Option<PathBuf>,
    pub program: OsString,
    pub args: Vec<OsString>,
}

impl SandboxLaunch {
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, String> {
        let mut args = args.into_iter();
        let mut launch = Self::default();
        loop {
            let flag = args.next().ok_or("missing `-- PROGRAM`")?;
            let flag = flag.to_str().ok_or("unknown flag")?.to_owned();
            if flag == "--" {
                break;
            }
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--write" => launch.writable.push(value.into()),
                "--read" => launch.readable.push(value.into()),
                "--appcontainer" => launch.appcontainer = Some(value),
                "--profile" => launch.profile = Some(value),
                "--read-only" => launch.read_only.push(value.into()),
                "--hide" => launch.hidden.push(value.into()),
                "--ports-tcp" => {
                    launch.ports_tcp = Some(
                        (value.to_str().and_then(|v| v.split_once(':')))
                            .and_then(|(port, token)| {
                                let valid = !token.is_empty()
                                    && token.bytes().all(|b| b.is_ascii_alphanumeric());
                                valid.then_some((port.parse().ok()?, token.to_owned()))
                            })
                            .ok_or("--ports-tcp needs PORT:TOKEN")?,
                    );
                }
                "--unset" => launch.unset.push(value),
                "--cpu" => {
                    launch.cpu = Some(
                        (value.to_str().and_then(|v| v.split_once(':')))
                            .and_then(|(reserve, percent)| {
                                Some(crate::CpuShare {
                                    reserve: reserve.parse().ok()?,
                                    percent: percent.parse().ok()?,
                                })
                            })
                            .ok_or("--cpu needs RESERVE:PERCENT")?,
                    );
                }
                "--cwd" => launch.cwd = Some(value.into()),
                "--set" | "--pipe" => {
                    let pair = value
                        .to_str()
                        .ok_or_else(|| format!("{flag} needs UTF-8"))?;
                    let (key, value) =
                        (pair.split_once('=')).ok_or_else(|| format!("{flag} needs KEY=VALUE"))?;
                    let list = match flag.as_str() {
                        "--set" => &mut launch.vars,
                        _ => &mut launch.pipes,
                    };
                    list.push((key.into(), value.into()));
                }
                _ => return Err(format!("unknown flag {flag}")),
            }
        }
        launch.program = args.next().ok_or("missing PROGRAM")?;
        launch.args = args.collect();
        Ok(launch)
    }
}

/// True when `path` is `root` or lies below it. Both are compared as text
/// the way Windows does: a `\\?\` prefix is dropped, `/` and `\` are the same,
/// ASCII case and trailing separators do not matter. The launcher uses it
/// to find the hidden paths inside a granted one.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn path_is_inside(path: &Path, root: &Path) -> bool {
    fn normal(path: &Path) -> String {
        let text = path.to_string_lossy();
        let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
        let text: String = text
            .chars()
            .map(|c| {
                if c == '/' {
                    '\\'
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect();
        text.trim_end_matches('\\').to_owned()
    }
    let (path, root) = (normal(path), normal(root));
    match path.strip_prefix(&root) {
        Some(rest) => rest.is_empty() || rest.starts_with('\\') || root.is_empty(),
        None => false,
    }
}

/// The system-wide startup files of sh, bash, zsh and fish under `root`.
/// User files in the home directory are the user's own business.
fn shell_startup_files(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = [
        "etc/profile",
        "etc/bash.bashrc",
        "etc/bashrc",
        "etc/zshenv",
        "etc/zprofile",
        "etc/zshrc",
        "etc/zsh/zshenv",
        "etc/zsh/zprofile",
        "etc/zsh/zshrc",
        "etc/fish/config.fish",
    ]
    .iter()
    .map(|path| root.join(path))
    .collect();
    for (dir, extension) in [("etc/profile.d", "sh"), ("etc/fish/conf.d", "fish")] {
        let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
            continue;
        };
        let mut found: Vec<PathBuf> = (entries.flatten())
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|e| e == extension))
            .collect();
        found.sort();
        files.extend(found);
    }
    files
}

/// See [`SandboxPolicy::shell_startup_stand_ins`]. `root` stands for `/`.
fn shell_startup_stand_ins(root: &Path, vars: &[(String, String)]) -> Vec<(PathBuf, String)> {
    let mut stand_ins = Vec::new();
    for path in shell_startup_files(root) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !text.to_ascii_lowercase().contains("_proxy") {
            continue;
        }
        let fish = path.extension().is_some_and(|e| e == "fish");
        let mut out = text;
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("# scriptschnell-term: the sandbox's proxy, not the host's.\n");
        for (key, value) in vars {
            if fish {
                let value = value.replace('\\', "\\\\").replace('\'', "\\'");
                let _ = writeln!(out, "set -gx {key} '{value}'");
            } else {
                let value = value.replace('\'', "'\\''");
                let _ = writeln!(out, "export {key}='{value}'");
            }
        }
        let inside = Path::new("/").join(path.strip_prefix(root).unwrap_or(&path));
        stand_ins.push((inside, out));
    }
    stand_ins
}

/// `sh -c PLACEHOLDERS sh PATH KIND... -- BWRAP_ARGS...`: creates each
/// missing `PATH` whose parent directory exists, a directory for the kind
/// `d` and a file with what follows the `f` of any other kind, then runs
/// bwrap. `set -C` keeps it from replacing a file another process created
/// in between. Names in `cmdterm_odd`, which [`credential_filter`] fills
/// with credentials whose names the shell cannot `unset`, go to bwrap's
/// `--unsetenv`, which takes any name.
const PLACEHOLDERS: &str = r#"while [ "$#" -gt 1 ] && [ "$1" != -- ]; do
  p=$1 k=$2; shift 2
  if [ ! -e "$p" ] && [ ! -L "$p" ] && [ -d "${p%/*}" ]; then
    case $k in
      d) mkdir -- "$p" ;;
      *) (set -C; printf '%s' "${k#f}" > "$p") ;;
    esac 2>/dev/null
  fi
done
shift
set -f
IFS='
'
for n in $cmdterm_odd; do set -- --unsetenv "$n" "$@"; done
unset IFS
exec bwrap "$@"
"#;

/// Another environment's commands, inside a sandbox.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SandboxTarget {
    pub host: Box<Environment>,
    pub policy: SandboxPolicy,
}

impl SandboxTarget {
    pub fn new(host: Environment, policy: SandboxPolicy) -> Self {
        Self {
            host: Box::new(host),
            policy,
        }
    }

    /// `command` as a call of the sandbox program on the host.
    pub(crate) fn lower(&self, command: &EnvCommand, pty: bool) -> EnvCommand {
        match self.policy.kind {
            SandboxKind::Bwrap => self.lower_bwrap(command, pty),
            SandboxKind::Seatbelt => self.lower_seatbelt(command),
            SandboxKind::Windows => self.lower_windows(command),
        }
    }

    fn lower_bwrap(&self, command: &EnvCommand, pty: bool) -> EnvCommand {
        // On this machine the variables of the policy, the proxy and the
        // command reach bwrap through its environment, which it passes on,
        // not as `--setenv`: every user of the machine can read a command
        // line in /proc, and they hold the proxy's token, the control
        // socket's token and the worktree's own variables. A name set here
        // is not unset after. A shell that strips credentials (another
        // machine) would take some of them for inherited ones, so there
        // they stay `--setenv`.
        let proxy_vars = self.policy.proxy_vars();
        let vars: Vec<(OsString, OsString)> = (self.policy.vars.iter().chain(&proxy_vars))
            .map(|(key, value)| (key.into(), value.into()))
            .chain(command.vars.iter().cloned())
            .collect();
        let strip = self.policy.strip_credentials.as_deref();
        let (mut args, carried) = match strip {
            None => (without_unsets(self.policy.args(pty), &vars), vars),
            Some(_) => {
                let mut args = self.policy.args(pty);
                for (key, value) in vars {
                    args.extend(["--setenv".into(), key, value]);
                }
                (args, Vec::new())
            }
        };
        if let Some(cwd) = &command.cwd {
            args.push("--chdir".into());
            args.push(cwd.clone().into_os_string());
        }
        // Forwarded sockets show at the path the host's forward listens on.
        for (_, inside) in &command.sockets {
            args.push("--bind-try".into());
            args.push(inside.clone().into_os_string());
            args.push(inside.clone().into_os_string());
        }
        args.push("--".into());
        if self.policy.isolated() {
            let proxy = self.policy.proxy.as_ref();
            let ports = self.policy.ports.as_ref();
            if proxy.is_some() || ports.is_some() {
                args.extend(proxy_bridge().iter().cloned());
                if let Some(proxy) = proxy {
                    args.push("--port".into());
                    args.push(proxy.port.to_string().into());
                    args.push("--socket".into());
                    args.push(INNER_SOCKET.into());
                }
                if ports.is_some() {
                    args.push("--ports".into());
                    args.push(INNER_PORTS.into());
                }
                args.push("--".into());
            }
        } else if let Some(scope) = crate::landlock::host_network_scope(&self.host) {
            // The host's network namespace holds the host's abstract Unix
            // sockets, X11's among them. A bridge scopes the sandbox away
            // from them with Landlock, then becomes the command.
            args.extend(scope);
        }
        args.push(command.program.clone());
        args.extend(command.args.iter().cloned());
        let with_vars = |mut lowered: EnvCommand| {
            for (key, value) in &carried {
                lowered = lowered.var(key, value);
            }
            forward_sockets(lowered, command)
        };
        if self.policy.placeholders.is_empty() && strip.is_none() {
            return with_vars(self.host.command("bwrap").args(args));
        }
        // The credentials go first, in the shell that then execs bwrap, so
        // bwrap's `--setenv` still sets what the policy and command name.
        let script: OsString = match strip {
            Some(keep) => format!("{}{PLACEHOLDERS}", credential_filter(keep)).into(),
            None => PLACEHOLDERS.into(),
        };
        let mut prelude: Vec<OsString> = vec!["-c".into(), script, "sh".into()];
        for (path, content) in &self.policy.placeholders {
            prelude.push(path.clone().into_os_string());
            prelude.push(match content {
                None => "d".into(),
                Some(content) => format!("f{content}").into(),
            });
        }
        prelude.push("--".into());
        with_vars(self.host.command("sh").args(prelude).args(args))
    }

    /// `sandbox-exec` runs `env`, which drops and sets the variables. The
    /// filesystem is the host's, so the working directory is set outside.
    fn lower_seatbelt(&self, command: &EnvCommand) -> EnvCommand {
        let rules = crate::seatbelt::accepted(&self.host);
        let mut args = if command.sockets.is_empty() {
            self.policy.seatbelt_args(rules)
        } else {
            let mut policy = self.policy.clone();
            (policy.sockets).extend(command.sockets.iter().map(|(_, inside)| inside.clone()));
            policy.seatbelt_args(rules)
        };
        args.push("/usr/bin/env".into());
        for var in &self.policy.unset {
            args.push("-u".into());
            args.push(var.into());
        }
        let proxy_vars = self.policy.proxy_vars();
        let policy_vars =
            (self.policy.vars.iter().chain(&proxy_vars)).map(|(k, v)| (k.into(), v.into()));
        for (key, value) in policy_vars.chain(command.vars.iter().cloned()) {
            let mut pair: OsString = key;
            pair.push("=");
            pair.push(value);
            args.push(pair);
        }
        args.push(command.program.clone());
        args.extend(command.args.iter().cloned());
        let lowered = forward_sockets(
            self.host.command("/usr/bin/sandbox-exec").args(args),
            command,
        );
        match &command.cwd {
            Some(cwd) => lowered.cwd(cwd),
            None => lowered,
        }
    }

    fn lower_windows(&self, command: &EnvCommand) -> EnvCommand {
        let mut args = self.policy.launcher_args(&command.vars);
        for (key, value) in &command.vars {
            let mut pair = key.clone();
            pair.push("=");
            pair.push(value);
            args.push("--set".into());
            args.push(pair);
        }
        if let Some(cwd) = &command.cwd {
            args.push("--cwd".into());
            args.push(cwd.clone().into_os_string());
        }
        if let Some(cpu) = &command.cpu {
            args.push("--cpu".into());
            args.push(format!("{}:{}", cpu.share.reserve, cpu.share.percent).into());
        }
        args.push("--".into());
        args.push(command.program.clone());
        args.extend(command.args.iter().cloned());
        self.host.command(launcher()).args(args)
    }

    /// True when `path` is inside one of the writable paths.
    pub fn can_write(&self, path: &Path) -> bool {
        self.policy.writable.iter().any(|w| path.starts_with(w))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    fn bwrap_available() -> bool {
        Environment::Local
            .command("bwrap")
            .args(["--ro-bind", "/", "/", "--unshare-all", "true"])
            .stdout()
            .is_ok()
    }

    #[test]
    fn startup_files_that_set_a_proxy_get_the_sandboxs_back() {
        let root = tempfile::tempdir().unwrap();
        let write = |path: &str, text: &str| {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("etc/profile", "append_path /usr/bin\n");
        write(
            "etc/profile.d/90-proxy.sh",
            "export HTTP_PROXY=\"http://corp:3128\"",
        );
        write("etc/profile.d/locale.sh", "export LANG=C\n");
        write("etc/profile.d/notes.txt", "https_proxy=x\n");
        write(
            "etc/fish/conf.d/proxy.fish",
            "set -gx https_proxy http://corp:3128\n",
        );
        let vars = vec![
            ("HTTP_PROXY".to_owned(), "http://127.0.0.1:9".to_owned()),
            ("odd".to_owned(), "it's".to_owned()),
        ];
        let stand_ins = shell_startup_stand_ins(root.path(), &vars);
        let paths: Vec<&Path> = stand_ins.iter().map(|(path, _)| path.as_path()).collect();
        assert_eq!(
            paths,
            [
                Path::new("/etc/profile.d/90-proxy.sh"),
                Path::new("/etc/fish/conf.d/proxy.fish"),
            ]
        );
        assert!(
            stand_ins[0]
                .1
                .starts_with("export HTTP_PROXY=\"http://corp:3128\"\n")
        );
        assert!(
            stand_ins[0]
                .1
                .ends_with("export HTTP_PROXY='http://127.0.0.1:9'\nexport odd='it'\\''s'\n")
        );
        assert!(
            stand_ins[1]
                .1
                .ends_with("set -gx HTTP_PROXY 'http://127.0.0.1:9'\nset -gx odd 'it\\'s'\n")
        );

        // Without a proxy, or where files cannot be bound, nothing.
        let mut policy = SandboxPolicy::local(vec![], &[], false);
        policy.kind = SandboxKind::Bwrap;
        assert!(policy.shell_startup_stand_ins().is_empty());
        policy.kind = SandboxKind::Seatbelt;
        policy.proxy = Some(SandboxProxy {
            socket: PathBuf::from("/run/p.sock"),
            port: 9,
            ca_bundle: None,
            token: None,
        });
        assert!(policy.shell_startup_stand_ins().is_empty());
    }

    #[test]
    fn hidden_paths_are_sorted_and_missing_ones_dropped() {
        let temp = tempfile::tempdir().unwrap();
        // Hidden paths are canonical, and the temporary directory is a
        // symlink on macOS (/var is /private/var).
        let dir = crate::canonicalize(temp.path()).unwrap();
        let file = dir.join("token");
        std::fs::write(&file, "secret").unwrap();
        let policy = SandboxPolicy::local(
            vec![],
            &[dir.clone(), file.clone(), dir.join("nope")],
            false,
        );
        assert_eq!(policy.hidden_dirs, [dir]);
        assert_eq!(policy.hidden_files, [file]);
        let args: Vec<String> = policy
            .args(true)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(!args.contains(&"--share-net".to_owned()));
        assert!(!args.contains(&"--new-session".to_owned()));
    }

    #[test]
    fn readable_paths_show_through_a_hidden_directory() {
        let temp = tempfile::tempdir().unwrap();
        let dir = crate::canonicalize(temp.path()).unwrap();
        let docs = dir.join("extensions");
        let mut policy = SandboxPolicy::local(vec![], std::slice::from_ref(&dir), true);
        policy.kind = SandboxKind::Bwrap;
        policy.readable.push(docs.clone());
        let args = strings(&policy.args(true));
        let hide = args
            .iter()
            .position(|a| *a == dir.to_string_lossy())
            .unwrap();
        let show = args
            .iter()
            .position(|a| *a == docs.to_string_lossy())
            .unwrap();
        assert_eq!(args[hide - 1], "--tmpfs");
        assert_eq!(args[show - 1], "--ro-bind-try");
        assert!(
            hide < show,
            "the bind comes after the tmpfs it shows through"
        );
    }

    #[test]
    fn read_only_paths_are_bound_over_the_writable_ones() {
        let mut policy = SandboxPolicy::local(vec!["/h/.agent".into()], &[], true);
        policy.kind = SandboxKind::Bwrap;
        policy.read_only.push("/h/.agent/hooks".into());
        let args = strings(&policy.args(true));
        let write = args.iter().position(|a| a == "/h/.agent").unwrap();
        let keep = args.iter().position(|a| a == "/h/.agent/hooks").unwrap();
        assert_eq!(args[write - 1], "--bind-try");
        assert_eq!(args[keep - 1], "--ro-bind-try");
        assert!(
            write < keep,
            "the read-only bind comes after the writable one"
        );
    }

    #[cfg(unix)]
    #[test]
    fn hidden_symlinks_hide_their_target() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let policy = SandboxPolicy::local(vec![], &[link, real.clone()], true);
        assert_eq!(policy.hidden_dirs, [crate::canonicalize(&real).unwrap()]);
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn credential_names_are_told_apart_from_others() {
        for name in [
            "GITHUB_TOKEN",
            "gh_token",
            "GITLAB_TOKEN",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "NPM_TOKEN",
            "CARGO_REGISTRY_TOKEN",
            "HF_TOKEN",
            "OPENAI_API_KEY",
            "STRIPE_APIKEY",
            "AZURE_CLIENT_SECRET",
            "PGPASSWORD",
            "DB_PASSWD",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "DEPLOY_PRIVATE_KEY",
            "NPM_CONFIG__AUTH",
            "BASICAUTH",
            "HTTP_AUTHORIZATION",
            "AZURE_DEVOPS_EXT_PAT",
        ] {
            assert!(looks_like_credential(name), "{name}");
        }
        for name in [
            "PATH",
            "HOME",
            "GIT_AUTHOR_NAME",
            "GIT_AUTHOR_EMAIL",
            "XAUTHORITY",
            "AWS_PROFILE",
            "AWS_REGION",
            "SSH_ASKPASS",
            "KEYMAP",
        ] {
            assert!(!looks_like_credential(name), "{name}");
        }
    }

    /// A policy whose inherited environment holds credentials: one the
    /// user lets through, one the policy sets to a fake and one the
    /// command sets.
    fn policy_with_credentials(kind: SandboxKind) -> SandboxPolicy {
        let mut policy = SandboxPolicy::local(vec![PathBuf::from("/w")], &[], true);
        policy.kind = kind;
        policy.vars.push(("NPM_TOKEN".into(), "fake".into()));
        let inherited = [
            "PATH",
            "GIT_AUTHOR_NAME",
            "GITHUB_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "NPM_TOKEN",
            "SET_TOKEN",
            "ANTHROPIC_API_KEY",
            "SSH_AUTH_SOCK",
        ];
        policy.unset_credentials(inherited.map(OsString::from), &["anthropic_api_key"]);
        policy
    }

    fn command_setting_a_credential() -> EnvCommand {
        Environment::Local
            .command("sh")
            .var("SET_TOKEN", "explicit")
    }

    #[test]
    fn inherited_credentials_are_unset_but_kept_and_set_ones_stay() {
        let policy = policy_with_credentials(SandboxKind::Bwrap);
        for name in [
            "GITHUB_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "NPM_TOKEN",
            "SET_TOKEN",
        ] {
            assert!(policy.unset.iter().any(|u| u == name), "{name}");
        }
        for name in ["PATH", "GIT_AUTHOR_NAME", "ANTHROPIC_API_KEY"] {
            assert!(!policy.unset.iter().any(|u| u == name), "{name}");
        }
        let socks = policy.unset.iter().filter(|u| *u == "SSH_AUTH_SOCK");
        assert_eq!(socks.count(), 1);

        // The variables the policy and the command set reach bwrap through
        // its environment, and their names are not unset.
        let target = SandboxTarget::new(Environment::Local, policy);
        let lowered = target.lower(&command_setting_a_credential(), false);
        let args = strings(&lowered.args);
        let unset =
            |name: &str| (args.windows(2)).any(|pair| pair[0] == "--unsetenv" && pair[1] == name);
        assert!(unset("GITHUB_TOKEN") && unset("AWS_SECRET_ACCESS_KEY"));
        assert!(!unset("NPM_TOKEN") && !unset("SET_TOKEN"));
        let var = |name: &str| {
            (lowered.vars.iter())
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.to_string_lossy().into_owned())
        };
        assert_eq!(var("NPM_TOKEN").as_deref(), Some("fake"));
        assert_eq!(var("SET_TOKEN").as_deref(), Some("explicit"));
        assert!(!args.iter().any(|a| a == "ANTHROPIC_API_KEY"));
    }

    #[test]
    fn seatbelt_unsets_inherited_credentials_before_it_sets_variables() {
        let target = SandboxTarget::new(
            Environment::Local,
            policy_with_credentials(SandboxKind::Seatbelt),
        );
        let args = strings(&target.lower(&command_setting_a_credential(), true).args);
        let env = args.iter().position(|a| a == "/usr/bin/env").unwrap();
        let args = &args[env..];
        let unset = |name: &str| (args.windows(2)).any(|pair| pair[0] == "-u" && pair[1] == name);
        assert!(unset("GITHUB_TOKEN") && unset("AWS_SECRET_ACCESS_KEY"));
        assert!(!unset("ANTHROPIC_API_KEY") && !unset("PATH"));
        // `env` removes the `-u` names before it sets the pairs.
        assert!(args.iter().any(|a| a == "NPM_TOKEN=fake"));
        assert!(args.iter().any(|a| a == "SET_TOKEN=explicit"));
    }

    #[test]
    fn the_windows_launcher_unsets_inherited_credentials_but_not_set_ones() {
        let target = SandboxTarget::new(
            Environment::Local,
            policy_with_credentials(SandboxKind::Windows),
        );
        let lowered = target.lower(&command_setting_a_credential(), true);
        let launch = SandboxLaunch::parse(lowered.args.iter().cloned()).unwrap();
        let unset = |name: &str| launch.unset.iter().any(|u| u == name);
        assert!(unset("GITHUB_TOKEN") && unset("AWS_SECRET_ACCESS_KEY"));
        assert!(unset("SSH_AUTH_SOCK"));
        assert!(!unset("ANTHROPIC_API_KEY") && !unset("PATH"));
        // The launcher drops an unset name from what it sets as well, so
        // the names set here are not unset.
        assert!(!unset("NPM_TOKEN") && !unset("SET_TOKEN"));
        let var = |key: &str| {
            (launch.vars.iter())
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.to_string_lossy().into_owned())
        };
        assert_eq!(var("NPM_TOKEN").as_deref(), Some("fake"));
        assert_eq!(var("SET_TOKEN").as_deref(), Some("explicit"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_live_sandbox_loses_inherited_credentials_but_keeps_set_ones() {
        if !bwrap_available() {
            eprintln!("bwrap cannot create namespaces here; skipped");
            return;
        }
        let mut policy = SandboxPolicy::local(vec![], &[], false);
        policy.vars.push(("NPM_TOKEN".into(), "fake".into()));
        let inherited = ["GITHUB_TOKEN", "NPM_TOKEN", "SET_TOKEN", "KEPT_TOKEN"];
        policy.unset_credentials(inherited.map(OsString::from), &["KEPT_TOKEN"]);
        let target = SandboxTarget::new(Environment::Local, policy);
        let script = "echo \"[${GITHUB_TOKEN-gone} $NPM_TOKEN $SET_TOKEN $KEPT_TOKEN]\"";
        let inner = command_setting_a_credential().args(["-c", script]);
        let lowered = target.lower(&inner, false);
        // bwrap inherits these as the sandbox program would from the app,
        // with the lowered command's own variables on top.
        let mut set: Vec<OsString> = inherited.map(|name| format!("{name}=real").into()).to_vec();
        for (key, value) in &lowered.vars {
            let mut pair = key.clone();
            pair.push("=");
            pair.push(value);
            set.push(pair);
        }
        let out = Environment::Local
            .command("env")
            .args(set)
            .arg(lowered.program)
            .args(lowered.args)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "[gone fake explicit real]\n",
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Runs `policy`'s wrapper through this machine's `sh`, with `vars` as
    /// the whole inherited environment besides `PATH`, and a `bwrap` that
    /// prints the names it inherited and its own arguments.
    #[cfg(target_os = "linux")]
    fn run_wrapper(policy: SandboxPolicy, vars: &[(&str, &str)]) -> (Vec<String>, Vec<String>) {
        use std::os::unix::fs::PermissionsExt as _;
        let bin = tempfile::tempdir().unwrap();
        let fake = bin.path().join("bwrap");
        std::fs::write(
            &fake,
            "#!/bin/sh\nawk 'BEGIN { for (k in ENVIRON) print \"env \" k }'\n\
             for a; do printf 'arg %s\\n' \"$a\"; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let target = SandboxTarget::new(Environment::Local, policy);
        let lowered = target.lower(&command_setting_a_credential(), false);
        assert_eq!(lowered.program, "sh");
        let path = format!(
            "PATH={}:{}",
            bin.path().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let out = Environment::Local
            .command("env")
            .arg("-i")
            .arg(path)
            .args(vars.iter().map(|(key, value)| format!("{key}={value}")))
            .arg(lowered.program)
            .args(lowered.args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let out = String::from_utf8_lossy(&out.stdout).into_owned();
        let lines = |prefix: &str| -> Vec<String> {
            (out.lines())
                .filter_map(|line| line.strip_prefix(prefix).map(str::to_owned))
                .collect()
        };
        (lines("env "), lines("arg "))
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_host_shell_strips_what_looks_like_a_credential_by_the_same_rule() {
        let names = [
            "GITHUB_TOKEN",
            "gh_token",
            "AWS_ACCESS_KEY_ID",
            "Aws_Session_Token",
            "AZURE_DEVOPS_EXT_PAT",
            "NPM_CONFIG__AUTH",
            "GOOGLE_OAUTH_ACCESS_TOKEN",
            "HTTP_AUTHORIZATION",
            "BASICAUTH",
            "PGPASSWORD",
            "Db_Passwd",
            "X_API_KEY",
            "TOKENIZERS_PARALLELISM",
            "GIT_AUTHOR_NAME",
            "XAUTHORITY",
            "AUTHOR_AUTH",
            "AWS_PROFILE",
            "A__B",
            "_AUTH",
            "PLAIN",
            "SSH_AUTH_SOCK",
            "KEPT_TOKEN",
        ];
        let mut policy = SandboxPolicy::local(vec![], &[], false);
        policy.vars.push(("NPM_TOKEN".into(), "fake".into()));
        policy.strip_credentials = Some(vec!["kept_token".into(), "not a name".into()]);
        let mut vars: Vec<(&str, &str)> = names.iter().map(|name| (*name, "real")).collect();
        // A value with lines that look like variables is still one value.
        vars.push(("MULTI", "one\nGITHUB_TOKEN=x\nNPM_TOKEN=y"));
        vars.push(("NPM_TOKEN", "real"));
        // Names the shell cannot unset reach bwrap's `--unsetenv` instead.
        vars.push(("MY-TOKEN", "real"));
        vars.push(("my.password", "real"));
        vars.push(("PLAIN-NAME", "kept"));
        let (seen, args) = run_wrapper(policy, &vars);
        let unsets = |name: &str| (args.windows(2)).any(|w| w[0] == "--unsetenv" && w[1] == name);
        assert!(unsets("MY-TOKEN") && unsets("my.password") && !unsets("PLAIN-NAME"));
        for name in names {
            let kept = name == "KEPT_TOKEN" || !looks_like_credential(name);
            assert_eq!(seen.iter().any(|s| s == name), kept, "{name}");
        }
        assert!(seen.iter().any(|s| s == "MULTI") && seen.iter().any(|s| s == "PATH"));
        assert!(!seen.iter().any(|s| s.starts_with("cmdterm_")));
        // bwrap sets what the policy and the command name after that.
        let set = |name: &str, value: &str| {
            (args.windows(3)).any(|w| w[0] == "--setenv" && w[1] == name && w[2] == value)
        };
        assert!(set("NPM_TOKEN", "fake") && set("SET_TOKEN", "explicit"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn placeholders_still_come_with_the_credential_filter() {
        let dir = tempfile::tempdir().unwrap();
        let mut policy = SandboxPolicy::local(vec![], &[], false);
        policy.placeholders = vec![(dir.path().join("made"), None)];
        policy.strip_credentials = Some(Vec::new());
        let (seen, args) = run_wrapper(policy, &[("HF_TOKEN", "real")]);
        assert!(dir.path().join("made").is_dir());
        assert!(!seen.iter().any(|s| s == "HF_TOKEN"));
        assert!(args.iter().any(|a| a == "--ro-bind"));
    }

    #[test]
    fn seatbelt_allows_writes_then_hides_and_cuts_the_network() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy {
            kind: SandboxKind::Seatbelt,
            writable: vec![dir.path().join("missing")],
            read_only: vec![],
            placeholders: vec![],
            hidden_dirs: vec![dir.path().to_path_buf()],
            hidden_files: vec![],
            readable: vec![],
            redirects: vec![],
            files: vec![],
            synced: vec![],
            scratch_home: None,
            scratch_dirs: vec![],
            sockets: vec![dir.path().join("engine.sock")],
            pipes: vec![],
            unset: vec!["SSH_AUTH_SOCK".into()],
            vars: vec![("HISTFILE".into(), "/h".into())],
            network: false,
            host_network: false,
            proxy: None,
            ports: None,
            keyring: None,
            appcontainer: None,
            ports_tcp: None,
            profile: None,
            strip_credentials: None,
        };
        let target = SandboxTarget::new(Environment::Local, policy);
        let lowered = target.lower(&Environment::Local.command("ls").arg("-a").cwd("/w"), true);
        assert_eq!(lowered.program, "/usr/bin/sandbox-exec");
        assert_eq!(lowered.cwd.as_deref(), Some(Path::new("/w")));
        let args = strings(&lowered.args);
        let profile = &args[1];
        // The writable path and the socket lie inside the hidden
        // directory, so they come after it and show through.
        let hide = profile
            .find("(deny file-read* file-write* (subpath (param \"P0\")))")
            .unwrap();
        let write = profile
            .find("(allow file-read* file-write* (subpath (param \"P1\")))")
            .unwrap();
        let socket = profile
            .find("(allow file-read* file-write* (literal (param \"P2\")))")
            .unwrap();
        assert!(profile.find("(deny file-write*)").unwrap() < hide && hide < write);
        assert!(
            hide < socket,
            "the socket shows through the hidden directory"
        );
        assert!(profile.contains("(deny network-outbound"));
        let resolved = crate::canonicalize(dir.path()).unwrap();
        assert_eq!(args[3], format!("P0={}", resolved.display()));
        assert_eq!(
            args[5],
            format!("P1={}", resolved.join("missing").display())
        );
        assert_eq!(
            args[7],
            format!("P2={}", resolved.join("engine.sock").display())
        );
        assert_eq!(
            args[8..],
            [
                "/usr/bin/env",
                "-u",
                "SSH_AUTH_SOCK",
                "HISTFILE=/h",
                "ls",
                "-a"
            ]
        );
    }

    #[test]
    fn the_windows_launcher_parses_what_the_target_passes() {
        let policy = SandboxPolicy {
            kind: SandboxKind::Windows,
            writable: vec![PathBuf::from("C:/work")],
            read_only: vec![],
            placeholders: vec![],
            hidden_dirs: vec![],
            hidden_files: vec![],
            readable: vec![],
            redirects: vec![],
            files: vec![],
            synced: vec![],
            scratch_home: None,
            scratch_dirs: vec![],
            sockets: vec![],
            pipes: vec![("DOCKER_HOST".into(), "docker_engine".into())],
            unset: vec!["SSH_AUTH_SOCK".into()],
            vars: vec![("HISTFILE".into(), "C:/h".into())],
            network: true,
            host_network: false,
            proxy: None,
            ports: None,
            keyring: None,
            appcontainer: None,
            ports_tcp: None,
            profile: None,
            strip_credentials: None,
        };
        let target = SandboxTarget::new(Environment::Local, policy);
        let command = Environment::Local
            .command("git")
            .args(["status", "--"])
            .cwd("C:/work")
            .var("A", "b=c")
            .limit_cpu(Some(crate::CpuLimit::new(
                "t1",
                crate::CpuShare {
                    reserve: 1,
                    percent: 50,
                },
                100,
            )));
        let lowered = target.lower(&command, true);
        assert!(
            lowered
                .program
                .to_string_lossy()
                .contains("cmdterm-sandbox")
        );
        let launch = SandboxLaunch::parse(lowered.args.iter().cloned()).unwrap();
        assert_eq!(
            launch,
            SandboxLaunch {
                writable: vec![PathBuf::from("C:/work")],
                unset: vec!["SSH_AUTH_SOCK".into()],
                vars: vec![
                    ("HISTFILE".into(), "C:/h".into()),
                    ("A".into(), "b=c".into())
                ],
                pipes: vec![("DOCKER_HOST".into(), "docker_engine".into())],
                readable: vec![],
                appcontainer: None,
                ports_tcp: None,
                profile: None,
                read_only: vec![],
                hidden: vec![],
                cpu: Some(crate::CpuShare {
                    reserve: 1,
                    percent: 50
                }),
                cwd: Some(PathBuf::from("C:/work")),
                program: "git".into(),
                args: vec!["status".into(), "--".into()],
            }
        );
        assert!(SandboxLaunch::parse(["--bogus".into(), "x".into()]).is_err());
        assert!(SandboxLaunch::parse(["--write".into(), "x".into()]).is_err());
    }

    #[test]
    fn the_windows_launcher_gets_proxy_variables_and_appcontainer_flags() {
        let mut policy = SandboxPolicy::local(vec![PathBuf::from("C:/work")], &[], true);
        policy.kind = SandboxKind::Windows;
        policy.unset.clear();
        policy.readable = vec![PathBuf::from("C:/Users/me/.cargo")];
        policy.proxy = Some(SandboxProxy {
            socket: PathBuf::from("unused"),
            port: 4321,
            ca_bundle: Some(PathBuf::from("C:/c/bundle.pem")),
            token: None,
        });
        policy.appcontainer = Some("cmdterm-sandbox-7".into());
        policy.ports_tcp = Some((5555, "abc123".into()));
        let target = SandboxTarget::new(Environment::Local, policy.clone());
        let command = Environment::Local.command("cargo").arg("build");
        let lowered = target.lower(&command, true);
        let launch = SandboxLaunch::parse(lowered.args.iter().cloned()).unwrap();
        let var = |key: &str| {
            launch
                .vars
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.to_string_lossy().into_owned())
        };
        // The CA bundle keeps its host path, not the bwrap inner path.
        assert_eq!(var("HTTPS_PROXY").as_deref(), Some("http://127.0.0.1:4321"));
        assert_eq!(var("http_proxy").as_deref(), Some("http://127.0.0.1:4321"));
        assert_eq!(var("SSL_CERT_FILE").as_deref(), Some("C:/c/bundle.pem"));
        assert_eq!(var("PIP_CERT").as_deref(), Some("C:/c/bundle.pem"));
        // git leaves schannel, which ignores the bundle, for OpenSSL.
        assert_eq!(var("GIT_CONFIG_COUNT").as_deref(), Some("1"));
        assert_eq!(var("GIT_CONFIG_KEY_0").as_deref(), Some("http.sslBackend"));
        assert_eq!(var("GIT_CONFIG_VALUE_0").as_deref(), Some("openssl"));
        assert_eq!(launch.readable, [PathBuf::from("C:/Users/me/.cargo")]);
        assert_eq!(launch.appcontainer, Some("cmdterm-sandbox-7".into()));
        assert_eq!(launch.ports_tcp, Some((5555, "abc123".into())));
        assert_eq!(launch.program, "cargo");

        // Without the setting, none of the AppContainer flags appear, but
        // the proxy variables still do.
        policy.appcontainer = None;
        let lowered = SandboxTarget::new(Environment::Local, policy.clone()).lower(&command, true);
        let launch = SandboxLaunch::parse(lowered.args.iter().cloned()).unwrap();
        assert_eq!(launch.appcontainer, None);
        assert!(launch.readable.is_empty());
        assert_eq!(launch.ports_tcp, None);
        assert!(launch.vars.iter().any(|(k, _)| k == "HTTP_PROXY"));

        // Without a bundle git keeps the backend the user configured.
        if let Some(proxy) = &mut policy.proxy {
            proxy.ca_bundle = None;
        }
        let lowered = SandboxTarget::new(Environment::Local, policy).lower(&command, true);
        let launch = SandboxLaunch::parse(lowered.args.iter().cloned()).unwrap();
        assert!(!launch.vars.iter().any(|(k, _)| k == "GIT_CONFIG_COUNT"));
    }

    #[test]
    fn the_launcher_flags_for_appcontainers_parse_and_reject_bad_ports() {
        let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
        let launch = SandboxLaunch::parse(args(&[
            "--appcontainer",
            "n",
            "--read",
            "C:/r",
            "--read",
            "C:/s",
            "--ports-tcp",
            "80:tok",
            "--",
            "x",
        ]))
        .unwrap();
        assert_eq!(launch.appcontainer, Some("n".into()));
        assert_eq!(launch.readable.len(), 2);
        assert_eq!(launch.ports_tcp, Some((80, "tok".into())));
        assert!(SandboxLaunch::parse(args(&["--ports-tcp", "http:t", "--", "x"])).is_err());
        assert!(SandboxLaunch::parse(args(&["--ports-tcp", "70000:t", "--", "x"])).is_err());
        assert!(SandboxLaunch::parse(args(&["--ports-tcp", "80", "--", "x"])).is_err());
        assert!(SandboxLaunch::parse(args(&["--ports-tcp", "80:", "--", "x"])).is_err());
        assert!(SandboxLaunch::parse(args(&["--appcontainer"])).is_err());
    }

    #[test]
    fn a_windows_policy_round_trips_through_the_launcher_flags() {
        let mut policy = SandboxPolicy::local(vec![PathBuf::from("C:/work")], &[], true);
        policy.kind = SandboxKind::Windows;
        policy.profile = Some("cmdterm-sandbox-7".into());
        policy.read_only = vec![PathBuf::from("C:/work/.claude/hooks")];
        policy.hidden_dirs = vec![PathBuf::from("C:/Users/me/.cargo/registry")];
        policy.hidden_files = vec![PathBuf::from("C:/Users/me/.cargo/credentials.toml")];
        policy.appcontainer = Some("cmdterm-sandbox-7".into());
        let command = Environment::Local.command("cargo");
        let lowered = SandboxTarget::new(Environment::Local, policy.clone()).lower(&command, true);
        let launch = SandboxLaunch::parse(lowered.args.iter().cloned()).unwrap();
        assert_eq!(launch.profile, Some("cmdterm-sandbox-7".into()));
        assert_eq!(launch.read_only, [PathBuf::from("C:/work/.claude/hooks")]);
        assert_eq!(
            launch.hidden,
            [
                PathBuf::from("C:/Users/me/.cargo/registry"),
                PathBuf::from("C:/Users/me/.cargo/credentials.toml")
            ]
        );
        assert_eq!(launch.appcontainer, Some("cmdterm-sandbox-7".into()));

        // The token mode gets the read-only denies but cannot hide reads.
        policy.appcontainer = None;
        let lowered = SandboxTarget::new(Environment::Local, policy).lower(&command, true);
        let launch = SandboxLaunch::parse(lowered.args.iter().cloned()).unwrap();
        assert_eq!(launch.read_only.len(), 1);
        assert!(launch.hidden.is_empty());
        assert_eq!(launch.profile, Some("cmdterm-sandbox-7".into()));
    }

    #[test]
    fn the_launcher_parses_profile_read_only_and_hide() {
        let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
        let launch = SandboxLaunch::parse(args(&[
            "--profile",
            "p",
            "--read-only",
            "C:/a",
            "--read-only",
            "C:/b",
            "--hide",
            "C:/c",
            "--",
            "x",
        ]))
        .unwrap();
        assert_eq!(launch.profile, Some("p".into()));
        assert_eq!(launch.read_only.len(), 2);
        assert_eq!(launch.hidden, [PathBuf::from("C:/c")]);
        assert!(SandboxLaunch::parse(args(&["--profile"])).is_err());
        assert!(SandboxLaunch::parse(args(&["--hide"])).is_err());
    }

    #[test]
    fn paths_are_compared_the_way_windows_does() {
        let inside = |path: &str, root: &str| path_is_inside(Path::new(path), Path::new(root));
        assert!(inside(
            r"\\?\C:\Users\a\.cargo\credentials.toml",
            "C:/users/A/.cargo"
        ));
        assert!(inside(r"C:\a\b", r"C:\a\b"));
        assert!(inside(r"C:\a\b\", r"C:\a\b"));
        assert!(inside(r"C:\a\b\c", r"C:\a\b\"));
        assert!(inside("C:/a/B/c", r"c:\A\b"));
        assert!(!inside(r"C:\a\bc", r"C:\a\b"));
        assert!(!inside(r"C:\a", r"C:\a\b"));
        assert!(!inside(r"D:\a\b", r"C:\a\b"));
    }

    #[test]
    fn secrets_stay_off_the_bwrap_command_line() {
        let token = crate::proxy::ProxyToken::generate().unwrap();
        let mut policy = SandboxPolicy::local(vec![], &[], true);
        policy.kind = SandboxKind::Bwrap;
        policy.host_network = true;
        policy.proxy = Some(SandboxProxy {
            socket: PathBuf::from("/run/p.sock"),
            port: 4321,
            ca_bundle: None,
            token: Some(token.clone()),
        });
        policy
            .vars
            .push(("WORKTREE_SECRET".into(), "policy-secret".into()));
        policy.unset.push("SCRIPTSCHNELL_TERM_TOKEN".into());
        policy
            .placeholders
            .push((PathBuf::from("/nonexistent/x"), None));
        let target = SandboxTarget::new(Environment::Local, policy);
        let command = Environment::Local
            .command("sh")
            .var("SCRIPTSCHNELL_TERM_TOKEN", "control-secret");
        let lowered = target.lower(&command, true);
        let args = strings(&lowered.args).join(" ");
        let url = token.url(4321);
        for secret in [url.as_str(), "policy-secret", "control-secret"] {
            assert!(!args.contains(secret), "{secret} in {args}");
        }
        // They come through bwrap's environment, and a set name is not
        // unset after.
        let vars: Vec<(String, String)> = (lowered.vars.iter())
            .map(|(k, v)| (k.to_string_lossy().into(), v.to_string_lossy().into()))
            .collect();
        for (name, value) in [
            ("HTTP_PROXY", url.as_str()),
            ("WORKTREE_SECRET", "policy-secret"),
            ("SCRIPTSCHNELL_TERM_TOKEN", "control-secret"),
        ] {
            assert!(vars.iter().any(|(k, v)| k == name && v == value), "{name}");
        }
        assert!(
            !args.contains("--unsetenv SCRIPTSCHNELL_TERM_TOKEN"),
            "{args}"
        );
        assert!(!args.contains("--setenv"), "{args}");
    }

    #[test]
    fn a_proxy_replaces_the_network() {
        let proxy = SandboxProxy {
            socket: PathBuf::from("/run/p.sock"),
            port: 4321,
            ca_bundle: Some(PathBuf::from("/c/bundle.pem")),
            token: None,
        };
        let mut policy = SandboxPolicy::local(vec![], &[], false);
        policy.kind = SandboxKind::Bwrap;
        policy.proxy = Some(proxy);
        let target = SandboxTarget::new(Environment::Local, policy.clone());
        let lowered = target.lower(&Environment::Local.command("ls"), true);
        let args = strings(&lowered.args);
        assert!(!args.contains(&"--share-net".to_owned()));
        let bind = args.iter().position(|a| a == "/run/p.sock").unwrap();
        assert_eq!(args[bind + 1], INNER_SOCKET);
        let var = |name: &str| {
            (lowered.vars.iter())
                .rfind(|(key, _)| key == name)
                .map(|(_, value)| value.to_string_lossy().into_owned())
        };
        assert_eq!(var("HTTPS_PROXY").as_deref(), Some("http://127.0.0.1:4321"));
        let ca = args.iter().position(|a| a == "/c/bundle.pem").unwrap();
        assert_eq!(
            (args[ca - 1].as_str(), args[ca + 1].as_str()),
            ("--ro-bind", INNER_CA)
        );
        assert_eq!(var("SSL_CERT_FILE").as_deref(), Some(INNER_CA));
        let end = args.iter().position(|a| a == "--").unwrap();
        assert!(args[end + 1].contains("cmdterm-proxy-bridge"));
        assert_eq!(
            args[end + 2..],
            ["--port", "4321", "--socket", INNER_SOCKET, "--", "ls"]
        );

        policy.kind = SandboxKind::Seatbelt;
        let target = SandboxTarget::new(Environment::Local, policy.clone());
        let args = strings(&target.lower(&Environment::Local.command("ls"), true).args);
        let deny = args[1].find("(deny network-outbound").unwrap();
        let allow = args[1]
            .find("(allow network-outbound (remote ip \"localhost:4321\"))")
            .unwrap();
        assert!(deny < allow);
        assert!(args.contains(&"HTTP_PROXY=http://127.0.0.1:4321".to_owned()));
        assert!(args.contains(&"SSL_CERT_FILE=/c/bundle.pem".to_owned()));
        assert!(args[1].contains("(allow file-read* (literal (param"));

        // With the network but without the host's, the sandbox is isolated
        // and the bridge is still its way out.
        policy.kind = SandboxKind::Bwrap;
        policy.network = true;
        let target = SandboxTarget::new(Environment::Local, policy.clone());
        let args = strings(&target.lower(&Environment::Local.command("ls"), true).args);
        assert!(!args.contains(&"--share-net".to_owned()));
        let end = args.iter().position(|a| a == "--").unwrap();
        assert!(args[end + 1].contains("cmdterm-proxy-bridge"));
        assert_eq!(
            args[end + 2..],
            ["--port", "4321", "--socket", INNER_SOCKET, "--", "ls"]
        );

        // With the host's network, only the variables lead to the proxy.
        policy.host_network = true;
        policy.files.push(("/c/npmrc".into(), "/h/.npmrc".into()));
        let target = SandboxTarget::new(Environment::Local, policy.clone());
        let lowered = target.lower(&Environment::Local.command("ls"), true);
        let args = strings(&lowered.args);
        assert!(args.contains(&"--share-net".to_owned()));
        assert!(
            !args
                .iter()
                .any(|a| a == INNER_SOCKET || a.contains("cmdterm-proxy-bridge"))
        );
        assert!(
            (lowered.vars.iter()).any(|(k, v)| k == "HTTP_PROXY" && v == "http://127.0.0.1:4321")
        );
        let file = args.iter().position(|a| a == "/c/npmrc").unwrap();
        assert_eq!(
            (args[file - 1].as_str(), args[file + 1].as_str()),
            ("--ro-bind", "/h/.npmrc")
        );
        policy.kind = SandboxKind::Seatbelt;
        let target = SandboxTarget::new(Environment::Local, policy);
        let args = strings(&target.lower(&Environment::Local.command("ls"), true).args);
        // Only Unix sockets stay limited.
        assert!(!args[1].contains("(remote ip"));
        assert!(args.contains(&"HTTPS_PROXY=http://127.0.0.1:4321".to_owned()));
    }

    #[test]
    fn isolated_sandboxes_run_the_bridge_with_what_they_have() {
        let lower = |policy: &SandboxPolicy| {
            let target = SandboxTarget::new(Environment::Local, policy.clone());
            strings(&target.lower(&Environment::Local.command("ls"), true).args)
        };
        let mut policy = SandboxPolicy::local(vec![], &[], true);
        policy.kind = SandboxKind::Bwrap;
        // Nothing to forward, no bridge. Still no host network.
        let args = lower(&policy);
        assert!(!args.contains(&"--share-net".to_owned()));
        assert!(!args.iter().any(|a| a.contains("cmdterm-proxy-bridge")));

        // A hub alone.
        policy.ports = Some("/run/hub.sock".into());
        let args = lower(&policy);
        assert!(!args.contains(&"--share-net".to_owned()));
        let bind = args.iter().position(|a| a == "/run/hub.sock").unwrap();
        assert_eq!(
            (args[bind - 1].as_str(), args[bind + 1].as_str()),
            ("--bind", INNER_PORTS)
        );
        let end = args.iter().position(|a| a == "--").unwrap();
        assert!(args[end + 1].contains("cmdterm-proxy-bridge"));
        assert_eq!(args[end + 2..], ["--ports", INNER_PORTS, "--", "ls"]);

        // Both.
        policy.proxy = Some(SandboxProxy {
            socket: "/run/p.sock".into(),
            port: 99,
            ca_bundle: None,
            token: None,
        });
        let args = lower(&policy);
        let end = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(
            args[end + 2..],
            [
                "--port",
                "99",
                "--socket",
                INNER_SOCKET,
                "--ports",
                INNER_PORTS,
                "--",
                "ls"
            ]
        );

        // The host's network keeps today's arguments: no bridge, no hub.
        policy.host_network = true;
        let args = lower(&policy);
        assert!(args.contains(&"--share-net".to_owned()));
        assert!(
            !args
                .iter()
                .any(|a| a == INNER_PORTS || a.contains("cmdterm-proxy-bridge"))
        );

        // host_network does nothing without network.
        policy.network = false;
        assert!(!lower(&policy).contains(&"--share-net".to_owned()));
        let old: SandboxPolicy = serde_json::from_str(r#"{"network": true}"#).unwrap();
        assert!(!old.host_network && old.ports.is_none());
    }

    #[test]
    fn policies_without_a_kind_use_bwrap() {
        let policy: SandboxPolicy = serde_json::from_str(r#"{"network": false}"#).unwrap();
        assert_eq!(policy.kind, SandboxKind::Bwrap);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_writes_only_where_allowed_and_hides_secrets() {
        let work = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        let other = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        let secret = other.path().join("secret");
        std::fs::write(&secret, "hunter2").unwrap();
        let mut policy = SandboxPolicy::local(
            vec![work.path().to_path_buf()],
            std::slice::from_ref(&secret),
            false,
        );
        policy.vars.push(("W".into(), "policy".into()));
        let env = Environment::Sandbox(Box::new(SandboxTarget::new(Environment::Local, policy)));
        let script = format!(
            "echo ok > out; echo no > {other}/x 2>/dev/null; cat {secret} 2>/dev/null; \
             echo \"[$V $W]\"",
            other = other.path().display(),
            secret = secret.display(),
        );
        let out = env
            .command("sh")
            .args(["-c", &script])
            .cwd(work.path())
            .var("V", "set")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "[set policy]\n");
        assert_eq!(
            std::fs::read_to_string(work.path().join("out")).unwrap(),
            "ok\n"
        );
        assert!(!other.path().join("x").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_sandbox_writes_only_where_allowed_and_hides_secrets() {
        if !bwrap_available() {
            eprintln!("bwrap cannot create namespaces here; skipped");
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let secret = other.path().join("secret");
        std::fs::write(&secret, "hunter2").unwrap();
        let history = other.path().join("history");
        let own = other.path().join("own");
        std::fs::create_dir_all(&history).unwrap();
        std::fs::create_dir_all(&own).unwrap();
        let mut policy = SandboxPolicy::local(
            vec![work.path().to_path_buf()],
            std::slice::from_ref(&secret),
            false,
        );
        policy.redirects.push((own.clone(), history.clone()));
        policy.vars.push(("W".into(), "policy".into()));
        let env = Environment::Sandbox(Box::new(SandboxTarget::new(Environment::Local, policy)));
        let script = format!(
            "echo ok > out; echo no > {other}/x 2>/dev/null; cat {secret}; \
             echo h > {history}/h; echo \"[$V $W]\"",
            other = other.path().display(),
            secret = secret.display(),
            history = history.display(),
        );
        let out = env
            .command("sh")
            .args(["-c", &script])
            .cwd(work.path())
            .var("V", "set")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "[set policy]\n");
        assert_eq!(
            std::fs::read_to_string(work.path().join("out")).unwrap(),
            "ok\n"
        );
        assert!(!other.path().join("x").exists());
        // Writes to a redirected path land in the host directory behind it.
        assert_eq!(std::fs::read_to_string(own.join("h")).unwrap(), "h\n");
        assert!(!history.join("h").exists());
    }

    /// What another machine creates before bwrap starts, here on this one.
    #[cfg(target_os = "linux")]
    #[test]
    fn placeholders_are_created_before_the_sandbox_and_stay_read_only() {
        if !bwrap_available() {
            eprintln!("bwrap cannot create namespaces here; skipped");
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let work = crate::canonicalize(work.path()).unwrap();
        let git = work.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("config"), "kept").unwrap();
        let mut policy = SandboxPolicy {
            kind: SandboxKind::Bwrap,
            ..SandboxPolicy::local(vec![work.clone()], &[], false)
        };
        let content = |c: &str| Some(c.to_owned());
        policy.placeholders = vec![
            (git.join("commondir"), content(".\n")),
            (git.join("config"), content("")),
            (git.join("hooks"), None),
            (work.join("absent/config"), content("")),
        ];
        policy.read_only = vec![git.join("commondir"), git.join("hooks")];
        let env = Environment::Sandbox(Box::new(SandboxTarget::new(Environment::Local, policy)));
        let out = env
            .command("sh")
            .args([
                "-c",
                "echo x > .git/commondir 2>/dev/null || echo read-only",
            ])
            .cwd(&work)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "read-only\n");
        let read = |path: PathBuf| std::fs::read_to_string(path).unwrap();
        assert_eq!(read(git.join("commondir")), ".\n");
        assert_eq!(read(git.join("config")), "kept");
        assert!(git.join("hooks").is_dir());
        assert!(!work.join("absent").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn forwarded_sockets_are_bound_and_forwarded_by_the_host() {
        let policy = SandboxPolicy {
            kind: SandboxKind::Bwrap,
            ..SandboxPolicy::local(Vec::new(), &[], false)
        };
        let env = Environment::Sandbox(Box::new(SandboxTarget::new(
            Environment::ssh("box"),
            policy,
        )));
        let local = PathBuf::from("/run/app.sock");
        let inside = env.forwarded_socket(&local, "s1").unwrap();
        assert_eq!(inside, Path::new("/tmp/s1.sock"));
        let command = env.command("true").forward_socket(&local, &inside);
        let args: Vec<String> = (command.to_std().get_args())
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let at = args.iter().position(|a| a == "-R").unwrap();
        assert_eq!(args[at + 1], "/tmp/s1.sock:/run/app.sock");
        let script = args.last().unwrap();
        assert!(
            script.contains("--bind-try /tmp/s1.sock /tmp/s1.sock"),
            "{script}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_scratch_home_takes_new_files_and_synced_copies_take_writes() {
        if !bwrap_available() {
            eprintln!("bwrap cannot create namespaces here; skipped");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let dir = crate::canonicalize(dir.path()).unwrap();
        let (home, copies) = (dir.join("home"), dir.join("copies"));
        std::fs::create_dir_all(home.join("state")).unwrap();
        std::fs::create_dir_all(&copies).unwrap();
        std::fs::write(home.join("kept"), "host").unwrap();
        std::fs::write(home.join("agent.json"), "host").unwrap();
        std::fs::write(copies.join("agent.json"), "copy").unwrap();
        std::os::unix::fs::symlink("state", home.join("link")).unwrap();
        let mut policy = SandboxPolicy::local(vec![home.join("state")], &[], false);
        policy.scratch_home = Some(home.clone());
        policy
            .synced
            .push((copies.join("agent.json"), home.join("agent.json")));
        policy.writable.push(home.join("agent.json"));
        let env = Environment::Sandbox(Box::new(SandboxTarget::new(Environment::Local, policy)));
        // Like an agent: a lock next to the file, then the file in place,
        // after the host's agent renamed a new file over its own.
        let script = "mkdir agent.json.lock && cat agent.json; \
             while [ ! -e state/go ]; do sleep 0.02; done; echo new > agent.json; cat agent.json; \
             echo x > kept 2>/dev/null || echo kept; echo s > link/s; echo n > fresh; cat fresh";
        let sandboxed = {
            let home = home.clone();
            std::thread::spawn(move || {
                env.command("sh")
                    .args(["-c", script])
                    .cwd(&home)
                    .output()
                    .unwrap()
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(300));
        std::fs::write(home.join("agent.json.new"), "replaced").unwrap();
        std::fs::rename(home.join("agent.json.new"), home.join("agent.json")).unwrap();
        std::fs::write(home.join("state/go"), "").unwrap();
        let out = sandboxed.join().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "copynew\nkept\nn\n");
        assert_eq!(
            std::fs::read_to_string(copies.join("agent.json")).unwrap(),
            "new\n"
        );
        assert_eq!(
            std::fs::read_to_string(home.join("agent.json")).unwrap(),
            "replaced"
        );
        assert_eq!(std::fs::read_to_string(home.join("kept")).unwrap(), "host");
        assert_eq!(
            std::fs::read_to_string(home.join("state/s")).unwrap(),
            "s\n"
        );
        assert!(!home.join("fresh").exists() && !home.join("agent.json.lock").exists());
    }

    #[test]
    fn deeper_paths_are_mounted_after_shallower_ones() {
        let mut policy = SandboxPolicy::local(vec!["/w".into(), "/h/own".into()], &[], true);
        policy.kind = SandboxKind::Bwrap;
        policy.hidden_dirs = vec!["/w/.ssh".into(), "/h".into()];
        policy.hidden_files = vec!["/w/token".into()];
        policy.read_only = vec!["/h/kept".into(), "/h/own/hooks".into()];
        let args = strings(&policy.args(true));
        let at = |path: &str| args.iter().rposition(|a| a == path).unwrap();
        assert!(at("/w") < at("/w/.ssh") && at("/w") < at("/w/token"));
        assert!(at("/h") < at("/h/own") && at("/h/own") < at("/h/own/hooks"));
        assert!(
            !args.iter().any(|a| a == "/h/kept"),
            "a read-only path in a hidden directory stays hidden"
        );
        assert_eq!(args[at("/w/.ssh") - 1], "--tmpfs");
    }

    #[cfg(unix)]
    #[test]
    fn seatbelt_lets_writable_paths_inside_hidden_ones_be_read_and_written() {
        let mut policy = SandboxPolicy::local(vec!["/h/own".into(), "/w".into()], &[], true);
        policy.kind = SandboxKind::Seatbelt;
        policy.hidden_dirs = vec!["/h".into(), "/w/.ssh".into()];
        let args = strings(&policy.seatbelt_args(Rules::ALL));
        let profile = &args[1];
        let param = |path: &str| {
            let at = args
                .iter()
                .position(|a| a.ends_with(&format!("={path}")))
                .unwrap();
            let name = args[at].split_once('=').unwrap().0.to_owned();
            profile.find(&format!("(param \"{name}\")")).unwrap()
        };
        assert!(param("/h") < param("/h/own"), "{profile}");
        assert!(param("/w") < param("/w/.ssh"), "{profile}");
        assert!(profile.contains("(allow file-read* file-write* (subpath"));
    }

    #[cfg(unix)]
    #[test]
    fn seatbelt_denies_starting_programs_outside_and_unlisted_unix_sockets() {
        for network in [false, true] {
            let mut policy = SandboxPolicy::local(vec!["/w".into()], &[], network);
            policy.kind = SandboxKind::Seatbelt;
            policy.hidden_dirs = vec!["/w/.ssh".into()];
            let target = SandboxTarget::new(Environment::Local, policy);
            let command = (Environment::Local.command("ls"))
                .forward_socket("/c/control.sock", "/c/control.sock");
            let args = strings(&target.lower(&command, true).args);
            let profile = &args[1];
            for deny in [
                "(deny lsopen)",
                "(deny appleevent-send)",
                "(deny job-creation)",
                "(deny process-exec (literal \"/bin/launchctl\"))",
                "(global-name \"com.apple.coreservices.launchservicesd\")",
                "(global-name \"com.apple.coreservices.appleevents\")",
                "(global-name-prefix \"com.apple.lsd.\")",
                "(deny network-outbound (remote unix-socket))",
                "com\\.apple\\.launchd\\.",
            ] {
                assert!(profile.contains(deny), "{deny} missing from {profile}");
            }
            // Command line tools keep DNS, the keychain, TLS trust, user
            // lookups and logging.
            for kept in [
                "mDNSResponder\")",
                "SecurityServer",
                "trustd",
                "ocspd",
                "opendirectoryd",
                "notification_center",
                "logd",
                "diagnosticd",
            ] {
                let denied = (profile.lines())
                    .skip_while(|line| !line.starts_with("(deny mach-lookup"))
                    .take_while(|line| !line.starts_with("(allow"))
                    .any(|line| line.contains(kept));
                assert!(!denied, "{kept} is denied: {profile}");
            }
            assert!(
                profile
                    .contains("(remote unix-socket (subpath \"/private/var/run/mDNSResponder\"))")
            );
            // Paths reach the profile as parameters, never spliced in.
            assert!(!profile.contains("/c/control.sock") && !profile.contains("\"/w"));
            let param = |path: &str| {
                let at = (args.iter())
                    .position(|a| a.ends_with(&format!("={path}")))
                    .unwrap_or_else(|| panic!("{path} is not a parameter: {args:?}"));
                assert_eq!(args[at - 1], "-D");
                args[at].split_once('=').unwrap().0.to_owned()
            };
            let rule = |rule: &str, path: &str| {
                let line = format!(
                    "({rule} (remote unix-socket (subpath (param \"{}\"))))",
                    param(path)
                );
                (profile.find(&line)).unwrap_or_else(|| panic!("no {line} in {profile}"))
            };
            let base = profile
                .find("(deny network-outbound (remote unix-socket))")
                .unwrap();
            let control = rule("allow network-outbound", "/c/control.sock");
            let work = rule("allow network-outbound", "/w");
            let ssh = rule("deny network-outbound", "/w/.ssh");
            assert!(base < control && base < work, "{profile}");
            assert!(
                work < ssh,
                "a hidden directory in the worktree keeps its sockets"
            );
            // The network switch only decides IP.
            assert_eq!(profile.contains("(remote ip \"*:*\")"), !network);
            // Other processes' arguments and environments stay out of view.
            let deny = profile.find("(deny process-info*)").unwrap();
            let own = (profile.find("(allow process-info* (target same-sandbox))")).unwrap();
            assert!(deny < own, "{profile}");
            assert!(profile.contains("(deny sysctl-read (sysctl-name-prefix \"kern.procargs\"))"));
            assert!(profile.contains("(global-name \"com.apple.windowserver.active\")"));
        }
        // Without the groups a probe dropped, the paths keep their file rules.
        let mut policy = SandboxPolicy::local(vec!["/w".into()], &[], true);
        policy.kind = SandboxKind::Seatbelt;
        let profile = strings(&policy.seatbelt_args(Rules::NONE)).remove(1);
        assert!(profile.contains("(allow file-read* file-write* (subpath (param \"P0\")))"));
        for gone in [
            "unix-socket",
            "lsopen",
            "mach-lookup",
            "process-info",
            "procargs",
        ] {
            assert!(!profile.contains(gone), "{gone} in {profile}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn nested_paths_in_a_sandbox_follow_the_deeper_one() {
        if !bwrap_available() {
            eprintln!("bwrap cannot create namespaces here; skipped");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let dir = crate::canonicalize(dir.path()).unwrap();
        let (work, hidden) = (dir.join("work"), dir.join("hidden"));
        std::fs::create_dir_all(work.join(".ssh")).unwrap();
        std::fs::create_dir_all(hidden.join("own")).unwrap();
        std::fs::write(work.join(".ssh/id"), "key").unwrap();
        std::fs::write(work.join("token"), "token").unwrap();
        std::fs::write(hidden.join("kept"), "kept").unwrap();
        let mut policy = SandboxPolicy::local(
            vec![work.clone(), hidden.join("own")],
            &[work.join(".ssh"), work.join("token"), hidden.clone()],
            false,
        );
        policy.read_only.push(hidden.join("kept"));
        let env = Environment::Sandbox(Box::new(SandboxTarget::new(Environment::Local, policy)));
        let script = "cat work/.ssh/id work/token hidden/kept 2>/dev/null; \
             echo own > hidden/own/file; ls -A hidden work/.ssh; echo end";
        let out = env
            .command("sh")
            .args(["-c", script])
            .cwd(&dir)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "hidden:\nown\n\nwork/.ssh:\nend\n"
        );
        assert_eq!(
            std::fs::read_to_string(hidden.join("own/file")).unwrap(),
            "own\n"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_scratch_dir_keeps_out_what_the_host_adds_later() {
        if !bwrap_available() {
            eprintln!("bwrap cannot create namespaces here; skipped");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let dir = crate::canonicalize(dir.path()).unwrap();
        let run = dir.join("run");
        std::fs::create_dir_all(run.join("systemd")).unwrap();
        std::fs::write(run.join("early"), "early").unwrap();
        std::os::unix::fs::symlink("systemd", run.join("link")).unwrap();
        let mut policy = SandboxPolicy::local(vec![], &[], false);
        policy.scratch_dirs.push(run.clone());
        let env = Environment::Sandbox(Box::new(SandboxTarget::new(Environment::Local, policy)));
        let script = "while [ ! -e systemd/go ]; do sleep 0.02; done; \
             cat early; ls -A; readlink link";
        let sandboxed = {
            let run = run.clone();
            std::thread::spawn(move || {
                env.command("sh")
                    .args(["-c", script])
                    .cwd(&run)
                    .output()
                    .unwrap()
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(300));
        let _late = std::os::unix::net::UnixListener::bind(run.join("late.sock")).unwrap();
        std::fs::write(run.join("systemd/go"), "").unwrap();
        let out = sandboxed.join().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "earlyearly\nlink\nsystemd\nsystemd\n"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sockets_show_through_a_hidden_directory() {
        if !bwrap_available() {
            eprintln!("bwrap cannot create namespaces here; skipped");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let dir = crate::canonicalize(dir.path()).unwrap();
        let socket = dir.join("engine.sock");
        let other = dir.join("other.sock");
        let _listeners =
            [&socket, &other].map(|s| std::os::unix::net::UnixListener::bind(s).unwrap());
        let mut policy = SandboxPolicy::local(vec![], std::slice::from_ref(&dir), false);
        policy.sockets.push(socket.clone());
        let env = Environment::Sandbox(Box::new(SandboxTarget::new(Environment::Local, policy)));
        let script = format!(
            "test -S {}; echo $?; test -e {}; echo $?",
            socket.display(),
            other.display()
        );
        let out = env.command("sh").args(["-c", &script]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "0\n1\n");
    }
}
