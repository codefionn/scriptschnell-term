//! Git over SSH for sandboxes, through the filtering proxy.
//!
//! A sandbox holds no SSH keys and has no SSH agent, and the only way out
//! of an isolated one is the proxy, which speaks HTTP. So git in a sandbox
//! gets `GIT_SSH_COMMAND` set to the bridge in its `--git-ssh` mode
//! ([`git_ssh_main`]), which stands in for `ssh`. It sends the proxy one
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

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::thread;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use super::server::{read_head, split_authority};

/// The flag of the bridge that makes it [`git_ssh_main`].
pub(crate) const GIT_SSH_FLAG: &str = "--git-ssh";

/// The request method.
pub(crate) const METHOD: &str = "SSH";

pub(crate) const STDIN: u8 = b'i';
pub(crate) const STDOUT: u8 = b'o';
pub(crate) const STDERR: u8 = b'e';
pub(crate) const EXIT: u8 = b'x';

/// The largest frame either side takes.
const MAX_FRAME: usize = 1 << 20;

/// The exit code of `ssh` when it fails itself, which git reports as a
/// connection failure.
const SSH_FAILED: i32 = 255;

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

/// What git asked `ssh` for.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct SshArgs {
    host: String,
    user: Option<String>,
    port: Option<u16>,
    command: String,
}

/// OpenSSH's options without a value.
const NO_VALUE: &str = "46AaCfgKkMNnqsTtVvXxYy";
/// OpenSSH's options with a value.
const WITH_VALUE: &str = "BbcDEeFIiJLlmOoPpQRSWw";

/// Reads `ssh`'s command line the way OpenSSH does, as far as git uses
/// it: options, `[user@]host` or `ssh://[user@]host[:port]`, then the
/// command. Options other than `-p` and `-l` are ignored, as the proxy's
/// `ssh` uses its own.
pub(super) fn parse_ssh_args(args: &[String]) -> Result<SshArgs, String> {
    let mut user = None;
    let mut port = None;
    let mut at = 0;
    while let Some(arg) = args.get(at) {
        at += 1;
        if arg == "--" {
            break;
        }
        let Some(flags) = arg.strip_prefix('-').filter(|f| !f.is_empty()) else {
            at -= 1;
            break;
        };
        for (i, flag) in flags.char_indices() {
            if flag == 'G' {
                return Err("ssh -G is not supported".into());
            }
            if NO_VALUE.contains(flag) {
                continue;
            }
            if !WITH_VALUE.contains(flag) {
                return Err(format!("unknown ssh option -{flag}"));
            }
            let inline = &flags[i + flag.len_utf8()..];
            let value = if inline.is_empty() {
                at += 1;
                args.get(at - 1)
                    .cloned()
                    .ok_or_else(|| format!("ssh option -{flag} needs a value"))?
            } else {
                inline.to_owned()
            };
            match flag {
                'p' => port = Some(value.parse().map_err(|_| format!("bad port {value}"))?),
                'l' => user = Some(value),
                _ => {}
            }
            break;
        }
    }
    let destination = args.get(at).ok_or("no host")?;
    let mut host = destination.as_str();
    if let Some(rest) = host.strip_prefix("ssh://") {
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        let (left, authority) = match rest.rsplit_once('@') {
            Some((left, authority)) => (Some(left), authority),
            None => (None, rest),
        };
        let (name, url_port) = split_authority(authority, 0).ok_or("bad ssh:// URL")?;
        if url_port != 0 {
            port = Some(url_port);
        }
        if let Some(left) = left {
            user = Some(left.to_owned());
        }
        return finish(name, user, port, &args[at + 1..]);
    }
    if let Some((left, right)) = host.rsplit_once('@') {
        user = Some(left.to_owned());
        host = right;
    }
    finish(host.to_owned(), user, port, &args[at + 1..])
}

fn finish(
    host: String,
    user: Option<String>,
    port: Option<u16>,
    command: &[String],
) -> Result<SshArgs, String> {
    if !valid_host(&host) {
        return Err(format!("bad host {host:?}"));
    }
    if let Some(user) = user.as_deref().filter(|u| !valid_user(u)) {
        return Err(format!("bad user name {user:?}"));
    }
    if command.is_empty() {
        return Err("only git's commands pass, and there is no command".into());
    }
    Ok(SshArgs {
        host,
        user,
        port,
        command: command.join(" "),
    })
}

/// Where the sandbox reaches the proxy, from its proxy variables:
/// host, port and the `Proxy-Authorization` value.
fn proxy_from_env(get: impl Fn(&str) -> Option<String>) -> Option<(String, u16, Option<String>)> {
    let keys = [
        "ALL_PROXY",
        "all_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ];
    keys.iter().filter_map(|key| get(key)).find_map(|url| {
        let rest = url
            .get(..7)
            .filter(|scheme| scheme.eq_ignore_ascii_case("http://"))
            .map(|_| &url[7..])?;
        let authority = rest.split('/').next()?;
        let (credentials, address) = match authority.rsplit_once('@') {
            Some((credentials, address)) => (Some(credentials), address),
            None => (None, authority),
        };
        let (host, port) = split_authority(address, 80)?;
        let header = credentials.map(|c| format!("Basic {}", STANDARD.encode(c)));
        Some((host, port, header))
    })
}

/// The request head for `args`.
pub(super) fn request_head(args: &SshArgs, protocol: Option<&str>, auth: Option<&str>) -> String {
    let mut head = format!("{METHOD} {}", args.host);
    if let Some(port) = args.port {
        head.push_str(&format!(":{port}"));
    }
    head.push_str(" HTTP/1.1\r\n");
    if let Some(auth) = auth {
        head.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
    }
    if let Some(user) = &args.user {
        head.push_str(&format!("Ssh-User: {user}\r\n"));
    }
    head.push_str(&format!("Ssh-Command: {}\r\n", args.command));
    if let Some(protocol) = protocol {
        head.push_str(&format!("Git-Protocol: {protocol}\r\n"));
    }
    head.push_str("\r\n");
    head
}

/// The bridge's `--git-ssh` mode, which git runs as `ssh` in a sandbox
/// (see the module docs). Exits with the remote command's code, or 255
/// when it cannot run, as `ssh` does.
pub fn git_ssh_main(args: impl IntoIterator<Item = OsString>) -> i32 {
    let args: Option<Vec<String>> = args.into_iter().map(|a| a.into_string().ok()).collect();
    let parsed = match args.ok_or_else(|| "arguments that are not UTF-8".to_owned()) {
        Ok(args) => parse_ssh_args(&args),
        Err(error) => Err(error),
    };
    let parsed = match parsed {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("cmdterm sandbox: ssh: {error}");
            return SSH_FAILED;
        }
    };
    let Some((host, port, auth)) = proxy_from_env(|key| std::env::var(key).ok()) else {
        eprintln!("cmdterm sandbox: ssh: no proxy in HTTP_PROXY, so git cannot reach SSH hosts");
        return SSH_FAILED;
    };
    let protocol = std::env::var("GIT_PROTOCOL")
        .ok()
        .filter(|p| valid_protocol(p));
    let stream = match TcpStream::connect((host.as_str(), port)) {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("cmdterm sandbox: ssh: the proxy at {host}:{port}: {error}");
            return SSH_FAILED;
        }
    };
    let head = request_head(&parsed, protocol.as_deref(), auth.as_deref());
    relay(stream, &head, io::stdin(), io::stdout(), io::stderr())
}

/// Sends `head`, then relays `stdin`, `stdout` and `stderr` until the
/// proxy sends the exit code, which it returns.
pub(super) fn relay(
    mut stream: TcpStream,
    head: &str,
    stdin: impl Read + Send + 'static,
    mut stdout: impl Write,
    mut stderr: impl Write,
) -> i32 {
    if let Err(error) = stream.write_all(head.as_bytes()) {
        let _ = writeln!(stderr, "cmdterm sandbox: ssh: the proxy: {error}");
        return SSH_FAILED;
    }
    let Some((answer, rest)) = read_head(&mut stream) else {
        let _ = writeln!(
            stderr,
            "cmdterm sandbox: ssh: the proxy closed the connection"
        );
        return SSH_FAILED;
    };
    let answer = String::from_utf8_lossy(&answer);
    let ok = answer
        .split(' ')
        .nth(1)
        .is_some_and(|status| status == "200");
    if !ok {
        let _ = stderr.write_all(&rest);
        let _ = io::copy(&mut stream, &mut stderr);
        return SSH_FAILED;
    }
    let Ok(mut input) = stream.try_clone() else {
        return SSH_FAILED;
    };
    thread::spawn(move || {
        let mut stdin = stdin;
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let n = match stdin.read(&mut chunk) {
                Ok(n) => n,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => 0,
            };
            if write_frame(&mut input, STDIN, &chunk[..n]).is_err() || n == 0 {
                break;
            }
        }
    });
    let mut frames = io::Cursor::new(rest).chain(stream);
    let mut data = Vec::new();
    loop {
        match read_frame(&mut frames, &mut data) {
            Ok(Some(STDOUT)) => {
                if stdout
                    .write_all(&data)
                    .and_then(|()| stdout.flush())
                    .is_err()
                {
                    return SSH_FAILED;
                }
            }
            Ok(Some(STDERR)) => {
                let _ = stderr.write_all(&data);
                let _ = stderr.flush();
            }
            Ok(Some(EXIT)) => {
                return <[u8; 4]>::try_from(&data[..]).map_or(SSH_FAILED, i32::from_be_bytes);
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                let _ = writeln!(
                    stderr,
                    "cmdterm sandbox: ssh: the proxy closed the connection"
                );
                return SSH_FAILED;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_owned()).collect()
    }

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
    fn ssh_arguments_read_as_git_passes_them() {
        let args = parse_ssh_args(&strings(&[
            "-o",
            "SendEnv=GIT_PROTOCOL",
            "git@github.com",
            "git-upload-pack 'org/repo.git'",
        ]))
        .unwrap();
        assert_eq!(
            args,
            SshArgs {
                host: "github.com".into(),
                user: Some("git".into()),
                port: None,
                command: "git-upload-pack 'org/repo.git'".into(),
            }
        );
        let args =
            parse_ssh_args(&strings(&["-4", "-p", "2222", "-lme", "host", "a", "b"])).unwrap();
        assert_eq!((args.port, args.user.as_deref()), (Some(2222), Some("me")));
        assert_eq!(args.command, "a b");
        let args = parse_ssh_args(&strings(&["-p2200", "ssh://u@h.example:2222/", "c"])).unwrap();
        assert_eq!(args.host, "h.example");
        assert_eq!((args.port, args.user.as_deref()), (Some(2222), Some("u")));
        let args = parse_ssh_args(&strings(&["--", "h", "c"])).unwrap();
        assert_eq!(args.host, "h");

        for bad in [
            &["-G", "h"][..],
            &["h"],
            &["-p"],
            &["-p", "x", "h", "c"],
            &["-Z", "h", "c"],
            &["a;b", "c"],
            &["-u@h", "c"],
            &["--", "-oProxyCommand=x", "c"],
            &["--x@h", "c"],
        ] {
            assert!(parse_ssh_args(&strings(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_proxy_comes_from_the_sandboxs_proxy_variables() {
        let vars = |pairs: &'static [(&str, &str)]| {
            move |key: &str| {
                (pairs.iter())
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        assert_eq!(
            proxy_from_env(vars(&[("HTTP_PROXY", "http://127.0.0.1:4321")])),
            Some(("127.0.0.1".into(), 4321, None))
        );
        let (_, _, auth) =
            proxy_from_env(vars(&[("ALL_PROXY", "http://cmdterm:abc@127.0.0.1:9/")])).unwrap();
        assert_eq!(
            auth,
            Some(format!("Basic {}", STANDARD.encode("cmdterm:abc")))
        );
        assert_eq!(proxy_from_env(vars(&[("ALL_PROXY", "socks5://x:1")])), None);
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
