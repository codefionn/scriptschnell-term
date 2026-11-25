//! cmdterm remote machines.
//!
//! A remote machine runs `scriptschnell-term-remote serve`, either as a dedicated server
//! or inside a desktop cmdterm started with `--serve`. Clients reach it over
//! TCP. The handshake ([`handshake`]) combines X25519 with ML-KEM-768 for the
//! session keys and signs the transcript with both Ed25519 and ML-DSA-65, so
//! breaking it takes breaking the classical and the post-quantum scheme. Both
//! sides authenticate with key pairs ([`keys`]): the server checks
//! `authorized_keys`, the client checks `known_hosts`.
//!
//! After the handshake one connection carries many channels ([`proto`]): each
//! channel is one command or shell, with or without a PTY. A channel runs on
//! the server's host or in a Firecracker microVM ([`vm`]), so a machine can
//! offer its own environment, its compute resources, or both.
//!
//! `cmdterm-env`'s `Environment::Remote` runs `scriptschnell-term-remote exec`, which
//! hands its channel to a per-server background master ([`client`]), so a VCS
//! poll opens a channel instead of repeating the handshake. `exec --forward`
//! carries a Unix socket of the client to the remote machine ([`relay`]).
//! `exec --session` runs a terminal in a session ([`session`]) that keeps
//! running when the connection drops or the client exits, and reconnects.
//! A server also tells its clients which projects it has ([`projects`]), so
//! their sidebars can show them next to their own.
//!
//! The same Firecracker support runs VMs for this machine's own user through
//! the local VM daemon ([`local`]), which `Environment::Vm` reaches.

pub mod client;
pub mod exec;
pub mod handshake;
pub mod image;
pub mod keys;
pub mod link;
pub mod local;
pub mod mux;
pub mod projects;
pub mod proto;
mod queue;
pub mod rate;
pub mod relay;
pub mod server;
pub mod session;
pub mod sys;
pub mod transport;
pub mod vm;

use std::path::PathBuf;

pub use cmdterm_env::AppDirs;
use cmdterm_env::os_secrets;

/// Removes control characters from untrusted text received over the network
/// before printing it to a terminal, so a remote server cannot inject escape
/// sequences or line breaks.
pub(crate) fn sanitize_terminal_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\r' | '\n' | '\t') {
            out.push(' ');
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out
}

/// The service name of the identity in the operating system's secret
/// storage. The account is the key file's path.
const KEYRING_SERVICE: &str = "scriptschnell-term remote key";
pub use cmdterm_env::REMOTE_DEFAULT_PORT as DEFAULT_PORT;

/// Files of one machine's remote configuration: keys and `server.json` in
/// `remote/` of the configuration directory, VM images and disks in
/// `remote/` of the data directory (see [`AppDirs`]).
#[derive(Clone, Debug)]
pub struct Paths {
    /// scriptschnell-term's configuration directory.
    pub config_dir: PathBuf,
    /// `<config_dir>/remote`.
    pub dir: PathBuf,
    /// `remote/` in the data directory: images and VM disks.
    pub data: PathBuf,
    /// The state directory, whose `state.json` lists the projects the
    /// server shares.
    pub state: PathBuf,
    /// True when one directory holds everything (`--config-dir`), which
    /// processes started for these paths must be told.
    pub single: bool,
}

impl Paths {
    /// The paths of `--config-dir`, `$SCRIPTSCHNELL_TERM_CONFIG_DIR`, else
    /// the platform's directories.
    pub fn new(config_dir: Option<PathBuf>) -> Self {
        Self::of(&AppDirs::resolve(config_dir.as_deref()))
    }

    pub fn of(dirs: &AppDirs) -> Self {
        Self {
            config_dir: dirs.config.clone(),
            dir: dirs.config.join("remote"),
            data: dirs.data.join("remote"),
            state: dirs.state.clone(),
            single: dirs.is_single(),
        }
    }

    /// This machine's identity, used as client key and as server host key.
    pub fn identity(&self) -> PathBuf {
        self.dir.join("id_hybrid")
    }

    /// Loads [`Self::identity`], or makes a new one. Where the identity
    /// also lives in the operating system's secret storage (see
    /// [`Self::keeps_identity`]), a missing key file comes back from there
    /// first, and a new identity is stored there too.
    pub fn load_identity(&self) -> std::io::Result<keys::Identity> {
        let path = self.identity();
        if path.exists() || !self.keeps_identity() {
            return keys::Identity::load_or_generate(&path);
        }
        let stored = os_secrets::lookup(KEYRING_SERVICE, &path.to_string_lossy());
        if let Ok(Some(secret)) = stored {
            let secret = zeroize::Zeroizing::new(secret);
            if let Ok(identity) = keys::Identity::from_secret(&secret) {
                identity.save(&path)?;
                return Ok(identity);
            }
        }
        let identity = keys::Identity::load_or_generate(&path)?;
        let _ = self.store_identity(&identity);
        Ok(identity)
    }

    /// Copies the key file into the operating system's secret storage,
    /// unless it holds the same key already. Returns whether it stored it.
    /// May wait for the user to unlock the keyring.
    pub fn keep_identity(&self) -> std::io::Result<bool> {
        if !self.keeps_identity() {
            return Ok(false);
        }
        let identity = keys::Identity::load(&self.identity())?;
        let account = self.identity().to_string_lossy().into_owned();
        let stored = os_secrets::lookup(KEYRING_SERVICE, &account)?.map(zeroize::Zeroizing::new);
        if stored.as_deref().map(String::as_str) == Some(identity.to_secret().as_str()) {
            return Ok(false);
        }
        self.store_identity(&identity)?;
        Ok(true)
    }

    /// Whether the identity also goes into the operating system's secret
    /// storage: there is one, and these are the platform's directories.
    /// Scratch directories (`--config-dir`) stay out of it, so test runs
    /// leave no keys behind.
    pub fn keeps_identity(&self) -> bool {
        !self.single && os_secrets::available()
    }

    fn store_identity(&self, identity: &keys::Identity) -> std::io::Result<()> {
        let path = self.identity();
        let label = format!("scriptschnell-term remote machine key ({})", path.display());
        os_secrets::store(
            &label,
            KEYRING_SERVICE,
            &path.to_string_lossy(),
            &identity.to_secret(),
        )
    }

    /// Client keys this machine's server accepts.
    pub fn authorized_keys(&self) -> PathBuf {
        self.dir.join("authorized_keys")
    }

    /// Server keys this machine's client trusts, by address.
    pub fn known_hosts(&self) -> PathBuf {
        self.dir.join("known_hosts")
    }

    /// The app's `state.json`, whose projects the server shares.
    pub fn app_state(&self) -> PathBuf {
        self.state.join("state.json")
    }

    /// Server options such as Firecracker VMs.
    pub fn server_config(&self) -> PathBuf {
        self.dir.join("server.json")
    }
}

/// Directory for master control sockets, mode 0700.
pub fn runtime_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("cmdterm-remote")
}

/// Creates [`runtime_dir`] as [`create_private_dir`] does and checks that it
/// is this user's own directory, not a symlink to one. Without the check, a
/// symlink planted where [`runtime_dir`] falls back to the shared temporary
/// directory would put the master and VM daemon sockets in another directory
/// whose mode was never checked. The sockets themselves bind mode 0600 (see
/// [`sys::bind_private_socket`]), so the directory is a second line of
/// defense, not the only one.
pub fn create_runtime_dir() -> std::io::Result<PathBuf> {
    let dir = runtime_dir();
    check_own_dir(&dir)?;
    create_private_dir(&dir)?;
    check_own_dir(&dir)?;
    Ok(dir)
}

/// Fails when `dir` exists and is a symlink, or not a directory of this
/// user's own.
fn check_own_dir(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(not(unix))]
    let _ = dir;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let Ok(meta) = std::fs::symlink_metadata(dir) else {
            return Ok(());
        };
        if meta.file_type().is_symlink()
            || !meta.file_type().is_dir()
            || meta.uid() != rustix::process::geteuid().as_raw()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "{} is not this user's own directory; refusing it",
                    dir.display()
                ),
            ));
        }
    }
    Ok(())
}

/// Creates `dir` with mode 0700 if it does not exist.
pub fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    if !dir.exists() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(dir)?;
    }
    sys::set_mode(dir, 0o700)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn terminal_text_strips_escape_sequences_and_controls() {
        assert_eq!(
            sanitize_terminal_text("bad\x1b]52;c;YQ==\x07\r\n\u{009b}2Jok"),
            "bad]52;c;YQ==  2Jok"
        );
    }

    /// A symlink would put the sockets in the directory it points at, whose
    /// mode was never checked.
    #[test]
    fn the_runtime_dir_refuses_symlinks_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = dir.path().join("cmdterm-remote");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(check_own_dir(&link).is_err(), "a symlink is refused");
        assert!(check_own_dir(&target).is_ok(), "one's own dir is fine");
        assert!(check_own_dir(&dir.path().join("missing")).is_ok());
        let file = dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        assert!(check_own_dir(&file).is_err(), "a file is refused");
    }
}
