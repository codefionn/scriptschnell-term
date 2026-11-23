//! Git over SSH for sandboxes, through the filtering proxy.
//!
//! A sandbox holds no SSH keys and has no SSH agent, and the only way out
//! of an isolated one is the proxy, which speaks HTTP. So git in a sandbox
//! gets `GIT_SSH_COMMAND` set to the bridge in its `--git-ssh` mode
//! (`client::git_ssh_main`, Unix only), which stands in for `ssh`. It sends the proxy one
//! `SSH` request that names the host, the user and git's remote command.
//! The proxy runs this machine's `ssh` for it (`server/ssh.rs`), with the
//! user's configuration, agent and known hosts, and relays its input and
//! output. The keys never enter the sandbox.
//!
//! Only git's own commands pass: `git-upload-pack`, `git-receive-pack` and
//! `git-upload-archive` with one quoted path, as git sends them. The proxy
//! checks each against the guards as the HTTP request of the same service
//! ([`GitCommand::request_target`]), so a push over SSH asks under
//! `cmdterm.git-push` like one over HTTPS, for the same repository.
//!
//! The request, after which the client sends nothing until the answer:
//!
//! ```text
//! SSH github.com[:22] HTTP/1.1
//! Ssh-User: git                                  (optional)
//! Ssh-Command: git-receive-pack 'org/repo.git'
//! Git-Protocol: version=2                        (optional)
//! ```
//!
//! A refusal is an HTTP response like the proxy's others. After a `200`,
//! both sides send frames: a kind byte, the length as four bytes
//! big-endian, then the data. The client sends [`STDIN`] frames, an empty
//! one when its input ends. The proxy sends [`STDOUT`] and [`STDERR`]
//! frames, then one [`EXIT`] frame with `ssh`'s exit code as four bytes.

use std::io::{self, Read, Write};

#[cfg(unix)]
pub(super) mod client;

/// The flag of the bridge that makes it `client::git_ssh_main`.
pub(crate) const GIT_SSH_FLAG: &str = "--git-ssh";

/// The request method.
pub(crate) const METHOD: &str = "SSH";

pub(crate) const STDIN: u8 = b'i';
pub(crate) const STDOUT: u8 = b'o';
pub(crate) const STDERR: u8 = b'e';
pub(crate) const EXIT: u8 = b'x';

/// The largest frame either side takes.
const MAX_FRAME: usize = 1 << 20;

/// Writes one frame in a single write.
pub(crate) fn write_frame(out: &mut impl Write, kind: u8, data: &[u8]) -> io::Result<()> {
    let mut frame = Vec::with_capacity(5 + data.len());
    frame.push(kind);
    frame.extend_from_slice(&(data.len() as u32).to_be_bytes());
    frame.extend_from_slice(data);
    out.write_all(&frame)?;
    out.flush()
}

/// Reads one frame into `data`. `None` when the stream ends between
/// frames.
pub(crate) fn read_frame(input: &mut impl Read, data: &mut Vec<u8>) -> io::Result<Option<u8>> {
    let mut head = [0u8; 5];
    let mut filled = 0;
    while filled < head.len() {
        match input.read(&mut head[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => filled += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    data.resize(len, 0);
    input.read_exact(data)?;
    Ok(Some(head[0]))
}

/// The git services that pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Service {
    UploadPack,
    ReceivePack,
    UploadArchive,
}

impl Service {
    const ALL: [Self; 3] = [Self::UploadPack, Self::ReceivePack, Self::UploadArchive];

    pub(crate) const fn program(self) -> &'static str {
        match self {
            Self::UploadPack => "git-upload-pack",
            Self::ReceivePack => "git-receive-pack",
            Self::UploadArchive => "git-upload-archive",
        }
    }
}

/// A remote command git runs over SSH.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GitCommand {
    pub service: Service,
    /// The repository, unquoted.
    pub path: String,
}

impl GitCommand {
    /// `git-upload-pack 'path'` and the like, with the path quoted the way
    /// git quotes it: in single quotes, with `'` and `!` outside them
    /// after a backslash. Anything else is `None`.
    pub(crate) fn parse(command: &str) -> Option<Self> {
        let (program, quoted) = command.split_once(' ')?;
        let service = Service::ALL.into_iter().find(|s| s.program() == program)?;
        let path = sq_dequote(quoted)?;
        let fine = !path.is_empty() && !path.chars().any(char::is_control);
        fine.then_some(Self { service, path })
    }

    /// The command as the remote shell gets it, quoted as git does.
    pub(crate) fn to_command(&self) -> String {
        format!("{} {}", self.service.program(), sq_quote(&self.path))
    }

    /// The target of the HTTP request of the same service on the same
    /// repository, which guards compare as a `POST`, such as
    /// `/org/repo.git/git-receive-pack`.
    pub(crate) fn request_target(&self) -> String {
        format!(
            "/{}/{}",
            self.path.trim_matches('/'),
            self.service.program()
        )
    }
}

/// Undoes git's `sq_quote`: `'a'\''b'` is `a'b`.
fn sq_dequote(quoted: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = quoted.strip_prefix('\'')?;
    loop {
        let end = rest.find('\'')?;
        out.push_str(&rest[..end]);
        rest = &rest[end + 1..];
        if rest.is_empty() {
            return Some(out);
        }
        let mut chars = rest.chars();
        match (chars.next(), chars.next(), chars.next()) {
            (Some('\\'), Some(c @ ('\'' | '!')), Some('\'')) => {
                out.push(c);
                rest = &rest[3..];
            }
            _ => return None,
        }
    }
}

/// Quotes as git's `sq_quote` does.
fn sq_quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('\'');
    for c in text.chars() {
        match c {
            '\'' | '!' => {
                out.push_str("'\\");
                out.push(c);
                out.push('\'');
            }
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// A host name or address `ssh` may get from a sandbox: letters, digits,
/// dots, dashes and underscores, starting with a letter or digit, so it
/// never reads as an option.
pub(crate) fn valid_host(host: &str) -> bool {
    (1..=253).contains(&host.len())
        && host.as_bytes()[0].is_ascii_alphanumeric()
        && (host.bytes()).all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// A user name `ssh` may get from a sandbox.
pub(crate) fn valid_user(user: &str) -> bool {
    (1..=64).contains(&user.len())
        && !user.starts_with('-')
        && (user.bytes()).all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// A `GIT_PROTOCOL` value, such as `version=2`.
pub(crate) fn valid_protocol(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && (value.bytes()).all(|b| b.is_ascii_alphanumeric() || b"=:._-".contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_commands_parse_with_git_quoting_and_nothing_else() {
        let command = GitCommand::parse("git-receive-pack 'org/repo.git'").unwrap();
        assert_eq!(command.service, Service::ReceivePack);
        assert_eq!(command.path, "org/repo.git");
        assert_eq!(command.request_target(), "/org/repo.git/git-receive-pack");
        assert_eq!(command.to_command(), "git-receive-pack 'org/repo.git'");

        let odd = GitCommand::parse(r"git-upload-pack '/srv/it'\''s'\!'.git'").unwrap();
        assert_eq!(odd.path, "/srv/it's!.git");
        assert_eq!(odd.request_target(), "/srv/it's!.git/git-upload-pack");
        assert_eq!(GitCommand::parse(&odd.to_command()), Some(odd));
        assert_eq!(
            GitCommand::parse("git-upload-archive '~/r'")
                .unwrap()
                .service,
            Service::UploadArchive
        );

        for bad in [
            "sh -c id",
            "git-upload-pack org/repo",
            "git-upload-pack 'a'; id",
            "git-upload-pack 'a' 'b'",
            "git-upload-pack 'a",
            "git-upload-pack ''",
            "git-upload-pack 'a\nb'",
            "git upload-pack 'a'",
            "git-lfs-authenticate 'a' download",
        ] {
            assert_eq!(GitCommand::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn frames_round_trip_and_end_between_frames() {
        let mut wire = Vec::new();
        write_frame(&mut wire, STDOUT, b"abc").unwrap();
        write_frame(&mut wire, STDIN, b"").unwrap();
        let mut input = &wire[..];
        let mut data = Vec::new();
        assert_eq!(read_frame(&mut input, &mut data).unwrap(), Some(STDOUT));
        assert_eq!(data, b"abc");
        assert_eq!(read_frame(&mut input, &mut data).unwrap(), Some(STDIN));
        assert!(data.is_empty());
        assert_eq!(read_frame(&mut input, &mut data).unwrap(), None);
        assert!(read_frame(&mut &wire[..3], &mut data).is_err());
    }
}
