//! Signed release manifests and self-updates.
//!
//! Every platform of a release channel has a directory with a
//! `manifest.json` and its signature `manifest.json.sig`:
//!
//! ```text
//! <channel>/windows-x86_64/manifest.json
//! <channel>/windows-x86_64/manifest.json.sig
//! <channel>/windows-x86_64/0.2.0/scriptschnell-term.exe
//! ```
//!
//! The manifest names the version, the platform and every file with its
//! size and SHA3-256 hash. The signature covers the manifest bytes, so a
//! file the app downloads is only installed when its hash matches a manifest
//! the release key signed. The app takes a manifest only when its version is
//! newer than its own, so an old signed manifest cannot roll it back.

pub mod keys;
pub mod proxy;

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use sha3::{Digest, Sha3_256};

pub use keys::{SigningKey, VerifyingKey};

/// The release channel the app checks unless `update_channel` says otherwise.
pub const DEFAULT_CHANNEL: &str = "https://update.scriptschnell.codefionn.eu/scriptschnell-term";
pub const MANIFEST: &str = "manifest.json";
pub const SIGNATURE: &str = "manifest.json.sig";

/// TLS settings for every HTTP agent: the operating system verifies server
/// certificates against its own trust store, so certificates the user or an
/// administrator added to it are trusted.
pub fn system_tls() -> ureq::tls::TlsConfig {
    ureq::tls::TlsConfig::builder()
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build()
}
/// Signatures cover this prefix and the manifest bytes.
const DOMAIN: &[u8] = b"scriptschnell-term update manifest v1\n";
/// Suffix of a downloaded file before it replaces the installed one.
const NEW_SUFFIX: &str = ".update-new";
/// Suffix of a replaced file. Windows cannot delete a running executable,
/// so it stays until the next start removes it.
const OLD_SUFFIX: &str = ".update-old";
const MAX_MANIFEST: u64 = 64 * 1024;
const MAX_FILE: u64 = 1024 * 1024 * 1024;

/// The platform this build updates itself on. `None` where the app does not
/// replace its own files, such as Linux, where the AppImage or the package
/// manager does.
pub const PLATFORM: Option<&str> = if cfg!(all(windows, target_arch = "x86_64")) {
    Some("windows-x86_64")
} else {
    None
};

/// The release key this build trusts: `SCRIPTSCHNELL_RELEASE_KEY` at build
/// time. A build without one never updates itself.
pub fn release_key() -> Option<VerifyingKey> {
    option_env!("SCRIPTSCHNELL_RELEASE_KEY").and_then(VerifyingKey::parse)
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{url}: {message}")]
    Http { url: String, message: String },
    #[error("the update manifest has no valid signature")]
    Signature,
    #[error("the update manifest is invalid: {0}")]
    Manifest(String),
    #[error("the update is for {found}, not {expected}")]
    Platform { expected: String, found: String },
    #[error("{0} does not match the signed manifest")]
    Mismatch(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub platform: String,
    pub files: Vec<FileEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// The installed file's name, next to the running executable.
    pub name: String,
    /// Where to download it, relative to the platform directory.
    pub path: String,
    pub size: u64,
    /// Lowercase hex.
    pub sha3_256: String,
}

impl Manifest {
    /// Rejects names that leave the install directory and paths that leave
    /// the channel.
    fn validate(&self) -> Result<(), Error> {
        if self.files.is_empty() {
            return Err(Error::Manifest("it lists no files".into()));
        }
        for file in &self.files {
            let name_ok = !file.name.is_empty()
                && file.name != "."
                && file.name != ".."
                && !file.name.contains(['/', '\\', ':', '\0']);
            let path_ok = !file.path.is_empty()
                && !file.path.starts_with('/')
                && !file.path.contains(['\\', ':', '?', '#', '\0'])
                && file
                    .path
                    .split('/')
                    .all(|part| !part.is_empty() && part != "..");
            let hash_ok = file.sha3_256.len() == 64
                && file
                    .sha3_256
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
            if !name_ok || !path_ok || !hash_ok || file.size > MAX_FILE {
                return Err(Error::Manifest(format!("bad entry {:?}", file.name)));
            }
        }
        Ok(())
    }
}

fn signed_message(manifest: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(DOMAIN.len() + manifest.len());
    message.extend_from_slice(DOMAIN);
    message.extend_from_slice(manifest);
    message
}

/// The signature file's text for `manifest`: one line of base64.
pub fn sign(key: &SigningKey, manifest: &[u8]) -> String {
    format!("{}\n", STANDARD.encode(key.sign(&signed_message(manifest))))
}

/// Checks `signature` (a [`sign`] text) over `manifest` and parses it.
pub fn verify(key: &VerifyingKey, manifest: &[u8], signature: &str) -> Result<Manifest, Error> {
    let signature = STANDARD
        .decode(signature.trim())
        .map_err(|_| Error::Signature)?;
    if !key.verify(&signed_message(manifest), &signature) {
        return Err(Error::Signature);
    }
    let manifest: Manifest =
        serde_json::from_slice(manifest).map_err(|e| Error::Manifest(e.to_string()))?;
    manifest.validate()?;
    Ok(manifest)
}

/// SHA3-256 of everything `reader` yields, as lowercase hex, and its length.
pub fn hash(mut reader: impl Read, mut copy: impl Write) -> io::Result<(String, u64)> {
    let mut hasher = Sha3_256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        hasher.update(&buffer[..read]);
        copy.write_all(&buffer[..read])?;
        size += read as u64;
    }
    let hex = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok((hex, size))
}

/// One platform directory of a release channel.
pub struct Channel {
    base: String,
    platform: String,
    key: VerifyingKey,
    agent: ureq::Agent,
}

impl Channel {
    /// `base` is the channel URL without the platform, such as
    /// [`DEFAULT_CHANNEL`]. Requests go through `proxy`, else directly.
    pub fn new(base: &str, platform: &str, key: VerifyingKey, proxy: Option<ureq::Proxy>) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_global(Some(Duration::from_secs(30 * 60)))
            .proxy(proxy)
            .tls_config(system_tls())
            .user_agent(concat!("scriptschnell-term/", env!("CARGO_PKG_VERSION")))
            .build()
            .new_agent();
        Self {
            base: base.trim_end_matches('/').to_owned(),
            platform: platform.to_owned(),
            key,
            agent,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}/{path}", self.base, self.platform)
    }

    fn get(&self, path: &str) -> Result<ureq::http::Response<ureq::Body>, Error> {
        let url = self.url(path);
        self.agent.get(&url).call().map_err(|e| Error::Http {
            message: e.to_string(),
            url,
        })
    }

    fn get_small(&self, path: &str) -> Result<Vec<u8>, Error> {
        self.get(path)?
            .body_mut()
            .with_config()
            .limit(MAX_MANIFEST)
            .read_to_vec()
            .map_err(|e| Error::Http {
                url: self.url(path),
                message: e.to_string(),
            })
    }

    /// The channel's verified manifest.
    pub fn latest(&self) -> Result<Manifest, Error> {
        let manifest = self.get_small(MANIFEST)?;
        let signature = self.get_small(SIGNATURE)?;
        let signature = String::from_utf8(signature).map_err(|_| Error::Signature)?;
        let manifest = verify(&self.key, &manifest, &signature)?;
        if manifest.platform != self.platform {
            return Err(Error::Platform {
                expected: self.platform.clone(),
                found: manifest.platform,
            });
        }
        Ok(manifest)
    }

    /// The channel's manifest when it is newer than `current`.
    pub fn check(&self, current: &str) -> Result<Option<Manifest>, Error> {
        let manifest = self.latest()?;
        Ok(newer(&manifest.version, current).then_some(manifest))
    }

    /// Downloads every file of `manifest` into `dir`, checks each against
    /// its hash, then replaces the installed files. A failed download
    /// leaves the installation as it was. The running program keeps its old
    /// files until it restarts.
    pub fn install(&self, manifest: &Manifest, dir: &Path) -> Result<(), Error> {
        let result = self
            .download(manifest, dir)
            .and_then(|()| swap(manifest, dir));
        for file in &manifest.files {
            let _ = std::fs::remove_file(dir.join(format!("{}{NEW_SUFFIX}", file.name)));
        }
        result
    }

    fn download(&self, manifest: &Manifest, dir: &Path) -> Result<(), Error> {
        for file in &manifest.files {
            let target = dir.join(format!("{}{NEW_SUFFIX}", file.name));
            let mut response = self.get(&file.path)?;
            // One byte over the size, so a longer body fails the size check.
            let body = response
                .body_mut()
                .with_config()
                .limit(MAX_FILE + 1)
                .reader()
                .take(file.size + 1);
            let mut out = File::create(&target)?;
            let (hex, size) = hash(body, &mut out)?;
            out.sync_all()?;
            if hex != file.sha3_256 || size != file.size {
                return Err(Error::Mismatch(file.name.clone()));
            }
        }
        Ok(())
    }
}

/// Moves each installed file aside and the downloaded one into its place.
/// Puts everything back when one rename fails.
fn swap(manifest: &Manifest, dir: &Path) -> Result<(), Error> {
    let mut done: Vec<(&FileEntry, bool)> = Vec::new();
    let mut result = Ok(());
    for file in &manifest.files {
        let installed = dir.join(&file.name);
        let old = dir.join(format!("{}{OLD_SUFFIX}", file.name));
        let new = dir.join(format!("{}{NEW_SUFFIX}", file.name));
        match std::fs::remove_file(&old) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                result = Err(error);
                break;
            }
        }
        let existed = installed.exists();
        if existed && let Err(error) = std::fs::rename(&installed, &old) {
            result = Err(error);
            break;
        }
        if let Err(error) = std::fs::rename(&new, &installed) {
            if existed {
                let _ = std::fs::rename(&old, &installed);
            }
            result = Err(error);
            break;
        }
        done.push((file, existed));
    }
    if result.is_err() {
        for (file, existed) in done.into_iter().rev() {
            let installed = dir.join(&file.name);
            let _ = std::fs::remove_file(&installed);
            if existed {
                let _ = std::fs::rename(dir.join(format!("{}{OLD_SUFFIX}", file.name)), installed);
            }
        }
    }
    result.map_err(Error::from)
}

/// Removes what an earlier update left in `dir`: replaced files and
/// unfinished downloads. Call it at start.
pub fn remove_leftovers(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(OLD_SUFFIX) || name.ends_with(NEW_SUFFIX) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Whether `exe` was installed with `cargo install`: it sits in cargo's
/// `bin` directory and cargo's install record lists it. Cargo owns such an
/// installation, so the app leaves updates to `cargo install`.
pub fn installed_by_cargo(exe: &Path) -> bool {
    cargo_home().is_some_and(|home| installed_by_cargo_in(exe, &home))
}

/// `$CARGO_HOME`, else `.cargo` in the home directory.
fn cargo_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("CARGO_HOME").filter(|home| !home.is_empty()) {
        return Some(home.into());
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".cargo"))
}

fn installed_by_cargo_in(exe: &Path, cargo_home: &Path) -> bool {
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_owned());
    let (Some(dir), Some(name)) = (exe.parent(), exe.file_name()) else {
        return false;
    };
    if canonical(dir) != canonical(&cargo_home.join("bin")) {
        return false;
    }
    // Both records quote the binary names, `.exe` included on Windows.
    let quoted = format!("\"{}\"", name.to_string_lossy());
    [".crates2.json", ".crates.toml"].iter().any(|record| {
        std::fs::read_to_string(cargo_home.join(record)).is_ok_and(|text| text.contains(&quoted))
    })
}

/// Whether version `latest` is newer than `current`. Compares the dotted
/// numbers after an optional `v`; a pre-release suffix counts as older than
/// the release.
pub fn newer(latest: &str, current: &str) -> bool {
    fn parts(version: &str) -> (Vec<u64>, bool) {
        let version = version.trim().trim_start_matches('v');
        let (core, pre) = match version.find(['-', '+']) {
            Some(at) => (&version[..at], version[at..].starts_with('-')),
            None => (version, false),
        };
        let numbers = core
            .split('.')
            .map(|part| part.parse().unwrap_or(0))
            .collect();
        (numbers, pre)
    }
    let (latest, latest_pre) = parts(latest);
    let (current, current_pre) = parts(current);
    let len = latest.len().max(current.len());
    for i in 0..len {
        let (a, b) = (
            latest.get(i).copied().unwrap_or(0),
            current.get(i).copied().unwrap_or(0),
        );
        if a != b {
            return a > b;
        }
    }
    current_pre && !latest_pre
}

#[cfg(test)]
mod tests;
