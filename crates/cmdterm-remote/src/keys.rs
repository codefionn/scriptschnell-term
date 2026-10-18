//! Hybrid identity keys: Ed25519 plus ML-DSA-65.
//!
//! A signature is valid only when both halves verify, so a forger has to
//! break Ed25519 and ML-DSA. The private key file stores the two 32-byte
//! seeds. The public key is one line, like an OpenSSH key:
//! `cmdterm-hybrid-v1 <base64> comment`.
//!
//! `authorized_keys` holds such lines, optionally prefixed with options
//! (`vm-only`, `host`, `projects`, `no-projects`). `known_hosts` holds
//! `address <public key line>`.

use std::io::{self, Write};
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use ed25519_dalek::{Signer as _, Verifier as _};
use ml_dsa::{EncodedSignature, EncodedVerifyingKey, Keypair as _, MlDsa65};
use sha3::{Digest, Sha3_256};
use zeroize::Zeroizing;

/// Algorithm name in public key lines.
pub const ALGORITHM: &str = "cmdterm-hybrid-v1";
const PRIVATE_HEADER: &str = "cmdterm-hybrid-private-v1";
/// ML-DSA context string; also separates cmdterm signatures from any other
/// use of the same key.
const CONTEXT: &[u8] = b"cmdterm-remote-v1";

const ED_PUBLIC: usize = 32;
const ML_PUBLIC: usize = 1952;
const ED_SIGNATURE: usize = 64;
const ML_SIGNATURE: usize = 3309;
/// Length of an encoded public key.
pub const PUBLIC_LEN: usize = ED_PUBLIC + ML_PUBLIC;
/// Length of an encoded signature.
pub const SIGNATURE_LEN: usize = ED_SIGNATURE + ML_SIGNATURE;

/// A private identity.
pub struct Identity {
    seed: Zeroizing<[u8; 64]>,
    ed: ed25519_dalek::SigningKey,
    ml: ml_dsa::SigningKey<MlDsa65>,
    public: PublicKey,
}

impl Identity {
    pub fn generate() -> io::Result<Self> {
        let mut seed = Zeroizing::new([0u8; 64]);
        getrandom::fill(seed.as_mut()).map_err(io::Error::other)?;
        Ok(Self::from_seed(&seed))
    }

    /// The Ed25519 seed followed by the ML-DSA seed.
    pub fn from_seed(seed: &[u8; 64]) -> Self {
        let mut ed_seed = [0u8; 32];
        ed_seed.copy_from_slice(&seed[..32]);
        let ed = ed25519_dalek::SigningKey::from_bytes(&ed_seed);
        let mut ml_seed = ml_dsa::Seed::default();
        ml_seed.copy_from_slice(&seed[32..]);
        let ml = ml_dsa::SigningKey::<MlDsa65>::from_seed(&ml_seed);
        let mut public = Vec::with_capacity(PUBLIC_LEN);
        public.extend_from_slice(ed.verifying_key().as_bytes());
        public.extend_from_slice(&ml.verifying_key().encode());
        let public = PublicKey::from_bytes(&public).expect("own public key decodes");
        Self {
            seed: Zeroizing::new(*seed),
            ed,
            ml,
            public,
        }
    }

    pub fn public(&self) -> &PublicKey {
        &self.public
    }

    /// Signs `message` with both keys.
    pub fn sign(&self, message: &[u8]) -> Vec<u8> {
        let mut signature = Vec::with_capacity(SIGNATURE_LEN);
        signature.extend_from_slice(&self.ed.sign(message).to_bytes());
        let ml = self
            .ml
            .expanded_key()
            .sign_deterministic(message, CONTEXT)
            .expect("context is shorter than 255 bytes");
        signature.extend_from_slice(&ml.encode());
        signature
    }

    /// Reads a private key file.
    pub fn load(path: &Path) -> io::Result<Self> {
        let text = Zeroizing::new(std::fs::read_to_string(path)?);
        let mut lines = text.lines();
        if lines.next() != Some(PRIVATE_HEADER) {
            return Err(invalid(format!("{} is not a cmdterm key", path.display())));
        }
        Self::from_secret(lines.next().unwrap_or_default())
            .map_err(|_| invalid(format!("{} is damaged", path.display())))
    }

    /// The seed as one line of base64: the second line of the key file, and
    /// what the operating system's secret storage keeps.
    pub fn to_secret(&self) -> Zeroizing<String> {
        Zeroizing::new(STANDARD.encode(self.seed.as_ref()))
    }

    /// The identity of a [`Self::to_secret`] line.
    pub fn from_secret(secret: &str) -> io::Result<Self> {
        let body = Zeroizing::new(
            STANDARD
                .decode(secret.trim())
                .map_err(|_| invalid("the key is not base64".into()))?,
        );
        let seed: [u8; 64] = body
            .as_slice()
            .try_into()
            .map_err(|_| invalid("the key has the wrong length".into()))?;
        let seed = Zeroizing::new(seed);
        Ok(Self::from_seed(&seed))
    }

    /// Reads `path`, or creates a new identity there (mode 0600) plus
    /// `path.pub`.
    pub fn load_or_generate(path: &Path) -> io::Result<Self> {
        if path.exists() {
            return Self::load(path);
        }
        let identity = Self::generate()?;
        identity.save(path)?;
        Ok(identity)
    }

    /// Writes the private key (mode 0600) and `<path>.pub`. Fails if the
    /// private key file exists.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            crate::create_private_dir(dir)?;
        }
        let mut file = crate::sys::mode(
            std::fs::OpenOptions::new().write(true).create_new(true),
            0o600,
        )
        .open(path)?;
        writeln!(file, "{PRIVATE_HEADER}\n{}", self.to_secret().as_str())?;
        let comment = default_comment();
        let mut public = path.as_os_str().to_owned();
        public.push(".pub");
        std::fs::write(public, format!("{}\n", self.public.to_line(&comment)))
    }
}

/// `user@host` for new key comments.
fn default_comment() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "cmdterm".into());
    let host = std::fs::read_to_string("/etc/hostname")
        .map(|h| h.trim().to_owned())
        .unwrap_or_else(|_| "localhost".into());
    format!("{user}@{host}")
}

/// A public identity.
#[derive(Clone)]
pub struct PublicKey {
    bytes: Vec<u8>,
    ed: ed25519_dalek::VerifyingKey,
    ml: ml_dsa::VerifyingKey<MlDsa65>,
}

impl PartialEq for PublicKey {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for PublicKey {}

impl std::fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.fingerprint())
    }
}

impl PublicKey {
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != PUBLIC_LEN {
            return None;
        }
        let ed_bytes: [u8; ED_PUBLIC] = bytes[..ED_PUBLIC].try_into().ok()?;
        let ed = ed25519_dalek::VerifyingKey::from_bytes(&ed_bytes).ok()?;
        let ml_bytes = EncodedVerifyingKey::<MlDsa65>::try_from(&bytes[ED_PUBLIC..]).ok()?;
        let ml = ml_dsa::VerifyingKey::<MlDsa65>::decode(&ml_bytes);
        Some(Self {
            bytes: bytes.to_vec(),
            ed,
            ml,
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// True when both halves of `signature` verify over `message`.
    pub fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        if signature.len() != SIGNATURE_LEN {
            return false;
        }
        let Ok(ed_sig) = <[u8; ED_SIGNATURE]>::try_from(&signature[..ED_SIGNATURE]) else {
            return false;
        };
        let ed_ok = self
            .ed
            .verify(message, &ed25519_dalek::Signature::from_bytes(&ed_sig))
            .is_ok();
        let ml_ok = EncodedSignature::<MlDsa65>::try_from(&signature[ED_SIGNATURE..])
            .ok()
            .and_then(|enc| ml_dsa::Signature::<MlDsa65>::decode(&enc))
            .is_some_and(|sig| self.ml.verify_with_context(message, CONTEXT, &sig));
        ed_ok & ml_ok
    }

    /// `SHA3-256:<base64>` of the encoded key.
    pub fn fingerprint(&self) -> String {
        format!(
            "SHA3-256:{}",
            STANDARD_NO_PAD.encode(Sha3_256::digest(&self.bytes))
        )
    }

    /// A short, path-safe name for this key, used for per-client VM
    /// directories.
    pub fn short_id(&self) -> String {
        let digest = Sha3_256::digest(&self.bytes);
        digest[..12].iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn to_line(&self, comment: &str) -> String {
        let key = STANDARD.encode(&self.bytes);
        if comment.is_empty() {
            format!("{ALGORITHM} {key}")
        } else {
            format!("{ALGORITHM} {key} {comment}")
        }
    }

    /// Parses `cmdterm-hybrid-v1 <base64> [comment]`.
    pub fn parse(line: &str) -> Option<Self> {
        let mut words = line.split_whitespace();
        if words.next()? != ALGORITHM {
            return None;
        }
        Self::from_bytes(&STANDARD.decode(words.next()?).ok()?)
    }
}

/// Options of an `authorized_keys` entry. `None` follows `server.json`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyOptions {
    /// Whether the key may run commands on the host itself: `host` allows
    /// it even when `offer_host` is off, `vm-only` limits the key to
    /// Firecracker VMs.
    pub host: Option<bool>,
    /// Whether the key may ask for the shared projects: `projects` or
    /// `no-projects`. Needs host access.
    pub projects: Option<bool>,
}

impl KeyOptions {
    /// The key may only use VMs, whatever `server.json` says.
    pub fn vm_only(&self) -> bool {
        self.host == Some(false)
    }

    /// Parses a comma-separated option list. `None` for an unknown option or
    /// two that contradict each other: either might be meant to restrict the
    /// key, so the safe reading is to refuse it.
    pub fn parse(list: &str) -> Option<Self> {
        let mut parsed = Self::default();
        let set = |slot: &mut Option<bool>, value: bool| match *slot {
            Some(old) if old != value => false,
            _ => {
                *slot = Some(value);
                true
            }
        };
        for option in list.split(',').filter(|o| !o.is_empty()) {
            let ok = match option {
                "vm-only" => set(&mut parsed.host, false),
                "host" => set(&mut parsed.host, true),
                "projects" => set(&mut parsed.projects, true),
                "no-projects" => set(&mut parsed.projects, false),
                _ => false,
            };
            if !ok {
                return None;
            }
        }
        Some(parsed)
    }

    /// The option list [`KeyOptions::parse`] reads back, empty for none.
    pub fn to_list(&self) -> String {
        let mut list = Vec::new();
        match self.host {
            Some(true) => list.push("host"),
            Some(false) => list.push("vm-only"),
            None => {}
        }
        match self.projects {
            Some(true) => list.push("projects"),
            Some(false) => list.push("no-projects"),
            None => {}
        }
        list.join(",")
    }

    /// `line` with these options in front.
    pub fn prefix(&self, line: &str) -> String {
        let list = self.to_list();
        if list.is_empty() {
            line.to_owned()
        } else {
            format!("{list} {line}")
        }
    }
}

/// Splits an `authorized_keys` line into its option list and the key line.
fn split_options(line: &str) -> (&str, &str) {
    if line.starts_with(ALGORITHM) {
        ("", line)
    } else {
        line.split_once(char::is_whitespace).unwrap_or((line, ""))
    }
}

/// Looks `key` up in an `authorized_keys` file. Lines are
/// `[option,option] cmdterm-hybrid-v1 <base64> [comment]`; `#` starts a
/// comment line. A missing file authorizes nobody. The first entry of `key`
/// whose options parse decides its options; an entry whose options do not
/// parse authorizes nobody and is skipped, so it cannot shadow a later entry
/// of the same key.
pub fn authorized(path: &Path, key: &PublicKey) -> io::Result<Option<KeyOptions>> {
    for line in read_lines(path)? {
        let (options, rest) = split_options(&line);
        if PublicKey::parse(rest).as_ref() == Some(key)
            && let Some(options) = KeyOptions::parse(options)
        {
            return Ok(Some(options));
        }
    }
    Ok(None)
}

/// Appends a public key line to `authorized_keys`.
pub fn authorize(path: &Path, line: &str) -> io::Result<()> {
    if !line.split_whitespace().any(|word| word == ALGORITHM) {
        return Err(invalid("not a cmdterm public key line".into()));
    }
    append_line(path, line.trim())
}

/// What `known_hosts` says about a server key.
#[derive(Debug, PartialEq, Eq)]
pub enum HostTrust {
    Trusted,
    Unknown,
    /// The address is listed with a different key.
    Changed,
}

/// Checks `key` for `address` against a `known_hosts` file.
pub fn host_trust(path: &Path, address: &str, key: &PublicKey) -> io::Result<HostTrust> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(HostTrust::Unknown),
        Err(error) => return Err(error),
    };
    let mut seen = false;
    for line in text.lines() {
        let Some((host, rest)) = line.trim().split_once(char::is_whitespace) else {
            continue;
        };
        if host != address {
            continue;
        }
        seen = true;
        if PublicKey::parse(rest.trim()).as_ref() == Some(key) {
            return Ok(HostTrust::Trusted);
        }
    }
    Ok(if seen {
        HostTrust::Changed
    } else {
        HostTrust::Unknown
    })
}

/// Records `key` as the server key of `address`.
pub fn trust_host(path: &Path, address: &str, key: &PublicKey) -> io::Result<()> {
    append_line(path, &format!("{address} {}", key.to_line("")))
}

/// One usable `authorized_keys` entry.
#[derive(Clone, Debug)]
pub struct AuthorizedKey {
    pub key: PublicKey,
    pub options: KeyOptions,
    pub comment: String,
}

/// The entries of an `authorized_keys` file that authorize a key. Lines
/// with an unknown option authorize nobody and are left out.
pub fn authorized_list(path: &Path) -> io::Result<Vec<AuthorizedKey>> {
    let mut list = Vec::new();
    for line in read_lines(path)? {
        let (options, rest) = split_options(&line);
        let (Some(key), Some(options)) = (PublicKey::parse(rest), KeyOptions::parse(options))
        else {
            continue;
        };
        let comment = rest
            .split_whitespace()
            .skip(2)
            .collect::<Vec<_>>()
            .join(" ");
        list.push(AuthorizedKey {
            key,
            options,
            comment,
        });
    }
    Ok(list)
}

/// Gives every `authorized_keys` line for `key` the options `options`.
pub fn set_options(path: &Path, key: &PublicKey, options: KeyOptions) -> io::Result<()> {
    rewrite_lines(path, |line| {
        let (_, rest) = split_options(line);
        if PublicKey::parse(rest).as_ref() == Some(key) {
            Some(options.prefix(rest))
        } else {
            Some(line.to_owned())
        }
    })
}

/// Removes every `authorized_keys` line for `key`.
pub fn unauthorize(path: &Path, key: &PublicKey) -> io::Result<()> {
    retain_lines(path, |line| {
        PublicKey::parse(split_options(line).1).as_ref() != Some(key)
    })
}

/// The servers in a `known_hosts` file, in file order.
pub fn known_hosts_list(path: &Path) -> io::Result<Vec<(String, PublicKey)>> {
    Ok(read_lines(path)?
        .iter()
        .filter_map(|line| {
            let (host, rest) = line.split_once(char::is_whitespace)?;
            Some((host.to_owned(), PublicKey::parse(rest.trim())?))
        })
        .collect())
}

/// Removes every `known_hosts` line for `address`.
pub fn forget_host(path: &Path, address: &str) -> io::Result<()> {
    retain_lines(path, |line| line.split_whitespace().next() != Some(address))
}

/// Non-empty, non-comment lines, trimmed. A missing file has none.
fn read_lines(path: &Path) -> io::Result<Vec<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_owned)
            .collect()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

/// Rewrites `path` with the lines `keep` accepts. Comments and blank lines
/// stay.
fn retain_lines(path: &Path, keep: impl Fn(&str) -> bool) -> io::Result<()> {
    rewrite_lines(path, |line| keep(line).then(|| line.to_owned()))
}

/// Rewrites `path` with what `map` makes of each line; `None` drops it.
/// Comments and blank lines stay. The new file replaces the old one in one
/// rename, mode 0600.
fn rewrite_lines(path: &Path, map: impl Fn(&str) -> Option<String>) -> io::Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            out.push_str(line);
            out.push('\n');
        } else if let Some(line) = map(trimmed) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    let partial = path.with_extension("partial");
    let mut file = crate::sys::mode(
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true),
        0o600,
    )
    .open(&partial)?;
    file.write_all(out.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(partial, path)
}

fn append_line(path: &Path, line: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        crate::create_private_dir(dir)?;
    }
    let mut file = crate::sys::mode(std::fs::OpenOptions::new().append(true).create(true), 0o600)
        .open(path)?;
    writeln!(file, "{line}")
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_need_both_halves() {
        let id = Identity::from_seed(&[7; 64]);
        let sig = id.sign(b"transcript");
        assert_eq!(sig.len(), SIGNATURE_LEN);
        assert!(id.public().verify(b"transcript", &sig));
        assert!(!id.public().verify(b"other", &sig));
        for flip in [0, ED_SIGNATURE + 10] {
            let mut bad = sig.clone();
            bad[flip] ^= 1;
            assert!(!id.public().verify(b"transcript", &bad), "flip at {flip}");
        }
        let other = Identity::from_seed(&[8; 64]);
        assert!(!other.public().verify(b"transcript", &sig));
    }

    #[test]
    fn key_files_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remote/id");
        let id = Identity::load_or_generate(&path).unwrap();
        let again = Identity::load_or_generate(&path).unwrap();
        assert_eq!(id.public(), again.public());
        let line = std::fs::read_to_string(dir.path().join("remote/id.pub")).unwrap();
        assert_eq!(PublicKey::parse(&line).as_ref(), Some(id.public()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(id.save(&path).is_err(), "never overwrites a key");
        // The keyring's line is the file's second line and restores the key.
        let secret = id.to_secret();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().lines().nth(1),
            Some(secret.as_str())
        );
        let restored = Identity::from_secret(&secret).unwrap();
        assert_eq!(restored.public(), id.public());
        assert!(Identity::from_secret("c2hvcnQ=").is_err());
    }

    #[test]
    fn authorized_keys_and_options() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("authorized_keys");
        let a = Identity::from_seed(&[1; 64]);
        let b = Identity::from_seed(&[2; 64]);
        let c = Identity::from_seed(&[3; 64]);
        assert_eq!(authorized(&path, a.public()).unwrap(), None);
        authorize(&path, &a.public().to_line("a@x")).unwrap();
        authorize(&path, &format!("vm-only {}", b.public().to_line(""))).unwrap();
        authorize(&path, &format!("future-thing {}", c.public().to_line(""))).unwrap();
        assert_eq!(
            authorized(&path, a.public()).unwrap(),
            Some(KeyOptions::default())
        );
        assert_eq!(
            authorized(&path, b.public()).unwrap(),
            Some(KeyOptions {
                host: Some(false),
                projects: None
            })
        );
        assert_eq!(authorized(&path, c.public()).unwrap(), None);
        let d = Identity::from_seed(&[4; 64]);
        authorize(
            &path,
            &format!("host,no-projects {}", d.public().to_line("")),
        )
        .unwrap();
        assert_eq!(
            authorized(&path, d.public()).unwrap(),
            Some(KeyOptions {
                host: Some(true),
                projects: Some(false)
            })
        );
        let e = Identity::from_seed(&[5; 64]);
        authorize(&path, &format!("host,vm-only {}", e.public().to_line(""))).unwrap();
        assert_eq!(
            authorized(&path, e.public()).unwrap(),
            None,
            "contradicting options authorize nobody"
        );
        // A broken line does not shadow a later working one of the same key.
        authorize(&path, &format!("nonsense {}", e.public().to_line(""))).unwrap();
        authorize(&path, &e.public().to_line("")).unwrap();
        assert_eq!(
            authorized(&path, e.public()).unwrap(),
            Some(KeyOptions::default()),
            "the entry whose options parse decides"
        );
        assert!(authorize(&path, "ssh-ed25519 AAAA").is_err());
    }

    #[test]
    fn lists_and_removes_keys_and_hosts() {
        let dir = tempfile::tempdir().unwrap();
        let keys = dir.path().join("authorized_keys");
        let a = Identity::from_seed(&[1; 64]);
        let b = Identity::from_seed(&[2; 64]);
        let c = Identity::from_seed(&[3; 64]);
        authorize(&keys, &a.public().to_line("a@x laptop")).unwrap();
        authorize(&keys, &format!("vm-only {}", b.public().to_line(""))).unwrap();
        authorize(&keys, &format!("future-thing {}", c.public().to_line(""))).unwrap();
        let list = authorized_list(&keys).unwrap();
        assert_eq!(list.len(), 2, "the unknown option authorizes nobody");
        assert_eq!(list[0].comment, "a@x laptop");
        assert!(list[1].options.vm_only());
        let grant = KeyOptions {
            host: Some(true),
            projects: Some(true),
        };
        set_options(&keys, b.public(), grant).unwrap();
        assert_eq!(authorized(&keys, b.public()).unwrap(), Some(grant));
        set_options(&keys, a.public(), KeyOptions::default()).unwrap();
        let list = authorized_list(&keys).unwrap();
        assert_eq!(list[0].comment, "a@x laptop", "the comment stays");
        assert_eq!(list[1].options, grant);
        unauthorize(&keys, a.public()).unwrap();
        assert_eq!(authorized(&keys, a.public()).unwrap(), None);
        assert!(authorized(&keys, b.public()).unwrap().is_some());

        let hosts = dir.path().join("known_hosts");
        trust_host(&hosts, "one:7722", a.public()).unwrap();
        trust_host(&hosts, "two:7722", b.public()).unwrap();
        let list = known_hosts_list(&hosts).unwrap();
        assert_eq!(list[0].0, "one:7722");
        assert_eq!(&list[1].1, b.public());
        forget_host(&hosts, "one:7722").unwrap();
        assert_eq!(known_hosts_list(&hosts).unwrap().len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&hosts).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn known_hosts_detects_changed_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        let a = Identity::from_seed(&[1; 64]);
        let b = Identity::from_seed(&[2; 64]);
        assert_eq!(
            host_trust(&path, "h:1", a.public()).unwrap(),
            HostTrust::Unknown
        );
        trust_host(&path, "h:1", a.public()).unwrap();
        assert_eq!(
            host_trust(&path, "h:1", a.public()).unwrap(),
            HostTrust::Trusted
        );
        assert_eq!(
            host_trust(&path, "h:1", b.public()).unwrap(),
            HostTrust::Changed
        );
        assert_eq!(
            host_trust(&path, "h:2", b.public()).unwrap(),
            HostTrust::Unknown
        );
    }
}
