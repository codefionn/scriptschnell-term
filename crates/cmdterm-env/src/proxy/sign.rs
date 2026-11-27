//! Commit and tag signing for sandboxes, through the filtering proxy.
//!
//! A sandbox sees neither the keys in `~/.gnupg` and `~/.ssh` nor the GPG
//! and SSH agents that hold them, and it cannot reach a security key. So
//! git in a sandbox gets `gpg.program` and `gpg.ssh.program` set to the
//! bridge (through `GIT_CONFIG_COUNT`, see `sandbox.rs`). The bridge knows
//! git's command lines for `gpg` and `ssh-keygen`
//! ([`client::is_git_signer`]) and sends the proxy one `SIGN` request with
//! what git asked for. The proxy runs this machine's `gpg` or
//! `ssh-keygen`, the ones the user's git configuration names, with the
//! user's keyring, agent and security key, and sends their output back
//! (`server/sign.rs`). The keys never enter the sandbox, and a PIN or
//! touch prompt shows on this machine.
//!
//! Signing asks under [`GIT_SIGN_GUARD`], once per key and kind of object
//! within a minute, so a rebase that signs many commits asks once.
//! Verifying asks nothing: it reads only public keys. The proxy builds
//! every command line itself; the request names no options, only a key,
//! a principal and the files git passed.
//!
//! The request, after which the client sends frames (a kind byte, the
//! length as four bytes big-endian, then the data), ended by an [`END`]
//! frame:
//!
//! ```text
//! SIGN openpgp HTTP/1.1                         (or ssh)
//! Sign-Action: sign                             (verify, find-principals, check-novalidate)
//! Sign-Key: 0x1234ABCD                          (sign: a gpg key or an ssh key file)
//! Sign-Agent: yes                               (ssh sign: ssh-keygen -U)
//! Sign-Namespace: git                           (ssh)
//! Sign-Principal: me@example.com                (ssh verify)
//! Sign-Time: 20260101120000                     (ssh: -O verify-time)
//! Sign-Status-Fd: 2                             (openpgp)
//! Sign-Allowed: /home/me/.ssh/allowed_signers   (ssh: allowed signers file)
//! Sign-Revoked: /home/me/.ssh/revoked           (ssh verify: revocation file)
//! ```
//!
//! The frames carry the [`PAYLOAD`], the [`SIGNATURE`] to verify, and the
//! contents of the [`KEY`], [`ALLOWED`] and [`REVOKED`] files when the
//! sandbox can read them. A refusal is an HTTP response like the proxy's
//! others. After a `200` the proxy sends `STDOUT` and `STDERR` frames, then
//! one `EXIT` frame with the program's exit code, as for git over SSH.

#[cfg(unix)]
pub(super) mod client;

/// The request method.
pub(crate) const METHOD: &str = "SIGN";

/// The guard that signing passes, see the module docs. It has no hosts and
/// no requests: the proxy asks with the key in place of a host.
pub const GIT_SIGN_GUARD: &str = "cmdterm.git-sign";

pub(crate) const PAYLOAD: u8 = b'p';
pub(crate) const SIGNATURE: u8 = b's';
pub(crate) const KEY: u8 = b'k';
pub(crate) const ALLOWED: u8 = b'a';
pub(crate) const REVOKED: u8 = b'r';
pub(crate) const END: u8 = b'.';

/// The most a request sends in all its frames.
pub(crate) const MAX_INPUT: usize = 16 << 20;

/// The largest key, allowed signers or revocation file the bridge sends.
pub(crate) const MAX_FILE: u64 = 1 << 20;

/// The start of the name git gives the file of a literal ssh key
/// (`key::ssh-ed25519 ...` in `user.signingKey`), which it writes to its
/// temporary directory.
pub(crate) const LITERAL_KEY_FILE: &str = ".git_signing_key_tmp";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Format {
    OpenPgp,
    Ssh,
}

impl Format {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::OpenPgp => "openpgp",
            Self::Ssh => "ssh",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        [Self::OpenPgp, Self::Ssh]
            .into_iter()
            .find(|f| f.name().eq_ignore_ascii_case(name))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Sign,
    Verify,
    /// `ssh-keygen -Y find-principals`: who in the allowed signers file
    /// made a signature.
    FindPrincipals,
    /// `ssh-keygen -Y check-novalidate`: whether a signature is good,
    /// whoever made it.
    CheckNovalidate,
}

impl Action {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Sign => "sign",
            Self::Verify => "verify",
            Self::FindPrincipals => "find-principals",
            Self::CheckNovalidate => "check-novalidate",
        }
    }

    pub(crate) fn parse(name: &str) -> Option<Self> {
        [
            Self::Sign,
            Self::Verify,
            Self::FindPrincipals,
            Self::CheckNovalidate,
        ]
        .into_iter()
        .find(|a| a.name() == name)
    }

    /// Whether the program reads the payload: everything but
    /// `find-principals`, which reads only the signature.
    pub(crate) fn has_payload(self) -> bool {
        self != Self::FindPrincipals
    }
}

/// What git asked gpg or ssh-keygen for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub format: Format,
    pub action: Action,
    /// The gpg key, or the path of the ssh key file.
    pub key: Option<String>,
    /// `ssh-keygen -U`: the key is in the agent.
    pub agent: bool,
    pub namespace: Option<String>,
    pub principal: Option<String>,
    pub time: Option<String>,
    /// The file descriptor gpg writes its status lines to, 1 or 2.
    pub status_fd: Option<u8>,
    /// The path of the allowed signers file.
    pub allowed: Option<String>,
    /// The path of the revocation file.
    pub revoked: Option<String>,
}

impl Request {
    pub(crate) fn new(format: Format, action: Action) -> Self {
        Self {
            format,
            action,
            key: None,
            agent: false,
            namespace: None,
            principal: None,
            time: None,
            status_fd: None,
            allowed: None,
            revoked: None,
        }
    }

    /// The request head, with the `Proxy-Authorization` value `auth`.
    pub(crate) fn head(&self, auth: Option<&str>) -> String {
        let mut head = format!("{METHOD} {} HTTP/1.1\r\n", self.format.name());
        let mut header = |name: &str, value: &str| {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        };
        if let Some(auth) = auth {
            header("Proxy-Authorization", auth);
        }
        header("Sign-Action", self.action.name());
        let optional = [
            ("Sign-Key", &self.key),
            ("Sign-Namespace", &self.namespace),
            ("Sign-Principal", &self.principal),
            ("Sign-Time", &self.time),
            ("Sign-Allowed", &self.allowed),
            ("Sign-Revoked", &self.revoked),
        ];
        for (name, value) in optional {
            if let Some(value) = value {
                header(name, value);
            }
        }
        if self.agent {
            header("Sign-Agent", "yes");
        }
        if let Some(fd) = self.status_fd {
            header("Sign-Status-Fd", &fd.to_string());
        }
        head.push_str("\r\n");
        head
    }

    /// Reads a request head and checks every value, see [`Self::check`].
    pub(crate) fn parse(head: &str) -> Result<Self, String> {
        let mut lines = head.split("\r\n");
        let first = lines.next().unwrap_or_default();
        let format = first
            .split(' ')
            .nth(1)
            .and_then(Format::parse)
            .ok_or("bad signature format")?;
        let mut action = None;
        let mut request = Self::new(format, Action::Sign);
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            let text = || Some(value.to_owned());
            match name.to_ascii_lowercase().as_str() {
                "sign-action" => action = Some(Action::parse(value).ok_or("bad action")?),
                "sign-key" => request.key = text(),
                "sign-agent" => request.agent = value == "yes",
                "sign-namespace" => request.namespace = text(),
                "sign-principal" => request.principal = text(),
                "sign-time" => request.time = text(),
                "sign-allowed" => request.allowed = text(),
                "sign-revoked" => request.revoked = text(),
                "sign-status-fd" => {
                    request.status_fd = Some(value.parse().map_err(|_| "bad status fd")?);
                }
                _ => {}
            }
        }
        request.action = action.ok_or("no action")?;
        request.check()?;
        Ok(request)
    }

    /// Checks that each value is one the programs take as a value and not
    /// as an option, and that the action has what it needs.
    pub(crate) fn check(&self) -> Result<(), String> {
        let word = |name: &str, value: &Option<String>| match value {
            Some(value) if !valid_word(value) => Err(format!("bad {name} {value:?}")),
            _ => Ok(()),
        };
        word("key", &self.key)?;
        word("principal", &self.principal)?;
        word("allowed signers file", &self.allowed)?;
        word("revocation file", &self.revoked)?;
        if let Some(namespace) = self.namespace.as_deref().filter(|n| !valid_namespace(n)) {
            return Err(format!("bad namespace {namespace:?}"));
        }
        if let Some(time) = self.time.as_deref().filter(|t| !valid_time(t)) {
            return Err(format!("bad verify time {time:?}"));
        }
        if self.status_fd.is_some_and(|fd| fd != 1 && fd != 2) {
            return Err("the status goes to stdout or stderr only".into());
        }
        let needs = |what: &str, have: bool| {
            if have {
                Ok(())
            } else {
                Err(format!("{} needs {what}", self.action.name()))
            }
        };
        match (self.format, self.action) {
            (Format::OpenPgp, Action::Sign) => needs("a key", self.key.is_some()),
            (Format::OpenPgp, Action::Verify) => Ok(()),
            (Format::OpenPgp, _) => Err(format!("gpg does not {}", self.action.name())),
            (Format::Ssh, Action::Sign) => {
                needs("a key", self.key.is_some())?;
                needs("a namespace", self.namespace.is_some())
            }
            (Format::Ssh, Action::Verify) => {
                needs("a namespace", self.namespace.is_some())?;
                needs("a principal", self.principal.is_some())?;
                needs("an allowed signers file", self.allowed.is_some())
            }
            (Format::Ssh, Action::FindPrincipals) => {
                needs("an allowed signers file", self.allowed.is_some())
            }
            (Format::Ssh, Action::CheckNovalidate) => {
                needs("a namespace", self.namespace.is_some())
            }
        }
    }
}

/// A key, principal or path: one line without control characters that
/// does not start like an option.
fn valid_word(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4096
        && !value.starts_with('-')
        && !value.chars().any(char::is_control)
}

fn valid_namespace(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('-')
        && (value.bytes()).all(|b| b.is_ascii_alphanumeric() || b"@._-".contains(&b))
}

/// `ssh-keygen`'s verify time: `YYYYMMDD[HHMM[SS]]`, optionally ending in `Z`.
fn valid_time(value: &str) -> bool {
    let digits = value.strip_suffix('Z').unwrap_or(value);
    [8, 12, 14].contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit())
}

/// What a payload is, for the question: `commit`, `tag`, `push` (a signed
/// push's certificate) or `data`, and the commit's subject, the tag's name
/// or the pushed repository.
pub(crate) fn describe(payload: &[u8]) -> (&'static str, Option<String>) {
    let text = String::from_utf8_lossy(payload);
    let (header, message) = text.split_once("\n\n").unwrap_or((&text, ""));
    let field = |name: &str| {
        (header.lines())
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(' '))
            .map(str::to_owned)
    };
    let (kind, about) = if header.starts_with("tree ") {
        let subject = message.lines().find(|l| !l.trim().is_empty());
        ("commit", subject.map(|s| s.trim().to_owned()))
    } else if header.starts_with("object ") && field("tag").is_some() {
        ("tag", field("tag"))
    } else if header.starts_with("certificate version ") {
        ("push", field("pushee"))
    } else {
        ("data", None)
    };
    let about = about
        .map(|about| {
            about
                .chars()
                .filter(|c| !c.is_control())
                .take(200)
                .collect()
        })
        .filter(|about: &String| !about.is_empty());
    (kind, about)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_go_through_their_head_and_back() {
        let mut request = Request::new(Format::Ssh, Action::Verify);
        request.namespace = Some("git".into());
        request.principal = Some("me@example.com".into());
        request.allowed = Some("/home/me/.ssh/allowed_signers".into());
        request.time = Some("20260101120000".into());
        let head = request.head(Some("Basic abc"));
        assert!(head.starts_with("SIGN ssh HTTP/1.1\r\nProxy-Authorization: Basic abc\r\n"));
        assert_eq!(Request::parse(&head), Ok(request));

        let mut sign = Request::new(Format::OpenPgp, Action::Sign);
        sign.key = Some("Me <me@example.com>".into());
        sign.status_fd = Some(2);
        assert_eq!(Request::parse(&sign.head(None)), Ok(sign));
    }

    #[test]
    fn values_that_read_as_options_or_lack_parts_are_refused() {
        let bad = [
            "SIGN x509 HTTP/1.1\r\nSign-Action: sign\r\nSign-Key: k\r\n\r\n",
            "SIGN openpgp HTTP/1.1\r\nSign-Action: sign\r\n\r\n",
            "SIGN openpgp HTTP/1.1\r\nSign-Action: sign\r\nSign-Key: --homedir=/x\r\n\r\n",
            "SIGN openpgp HTTP/1.1\r\nSign-Action: find-principals\r\n\r\n",
            "SIGN openpgp HTTP/1.1\r\nSign-Action: verify\r\nSign-Status-Fd: 5\r\n\r\n",
            "SIGN ssh HTTP/1.1\r\nSign-Action: sign\r\nSign-Key: /k\r\n\r\n",
            "SIGN ssh HTTP/1.1\r\nSign-Action: sign\r\nSign-Key: /k\r\nSign-Namespace: a b\r\n\r\n",
            "SIGN ssh HTTP/1.1\r\nSign-Action: check-novalidate\r\nSign-Namespace: git\r\n\
             Sign-Time: yesterday\r\n\r\n",
            "SIGN ssh HTTP/1.1\r\nSign-Action: verify\r\nSign-Namespace: git\r\n\
             Sign-Principal: p\r\n\r\n",
            "SIGN ssh HTTP/1.1\r\nSign-Action: shell\r\n\r\n",
        ];
        for head in bad {
            assert!(Request::parse(head).is_err(), "{head}");
        }
    }

    #[test]
    fn payloads_say_what_they_are() {
        let commit = b"tree 4b82\nparent 9a1c\nauthor A <a@b> 1 +0000\n\
                       committer A <a@b> 1 +0000\n\nFix the parser\n\nMore words.\n";
        assert_eq!(describe(commit), ("commit", Some("Fix the parser".into())));
        let tag = b"object 4b82\ntype commit\ntag v1.2.0\ntagger A <a@b> 1 +0000\n\nRelease\n";
        assert_eq!(describe(tag), ("tag", Some("v1.2.0".into())));
        let push = b"certificate version 0.1\npusher A 1 +0000\npushee git@h:o/r.git\n\n";
        assert_eq!(describe(push), ("push", Some("git@h:o/r.git".into())));
        assert_eq!(describe(b"hello"), ("data", None));
        assert_eq!(describe(b"tree 4b82\n\n"), ("commit", None));
    }
}
