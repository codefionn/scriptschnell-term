//! The bridge's side of signing (see `proxy/sign.rs`): git runs it as
//! `gpg` and `ssh-keygen`, and it hands what git asked for to the proxy.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};

use super::{
    ALLOWED, Action, END, Format, KEY, MAX_FILE, MAX_INPUT, PAYLOAD, REVOKED, Request, SIGNATURE,
};
use crate::proxy::server::read_head;
use crate::proxy::ssh::client::proxy_from_env;
use crate::proxy::ssh::{EXIT, STDERR, STDOUT, read_frame, write_frame};

/// The largest frame the bridge sends; [`crate::proxy::ssh`] takes up to
/// 1 MiB.
const CHUNK: usize = 512 * 1024;

/// Whether the bridge's first argument is the start of git's command line
/// for `gpg` (`--status-fd=2 -bsau KEY` to sign,
/// `--keyid-format=long --status-fd=1 --verify FILE -` to verify) or for
/// `ssh-keygen -Y`. None of the bridge's own flags looks like these.
pub fn is_git_signer(first: &OsStr) -> bool {
    first.to_str().is_some_and(|first| {
        first.starts_with("--status-fd=") || first == "--keyid-format=long" || first == "-Y"
    })
}

/// What git's command line asks for, and the files it names in the
/// sandbox.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::proxy) struct Call {
    pub request: Request,
    /// The signature to verify (`--verify FILE`, `-s FILE`).
    pub signature: Option<PathBuf>,
    /// What `ssh-keygen -Y sign` signs. It writes the signature next to
    /// it, with `.sig` added to the name.
    pub message: Option<PathBuf>,
}

/// Reads the command line git gives `gpg` or `ssh-keygen`.
pub(in crate::proxy) fn parse_args(args: &[String]) -> Result<Call, String> {
    if args.first().is_some_and(|a| a == "-Y") {
        parse_ssh_keygen(&args[1..])
    } else {
        parse_gpg(args)
    }
}

fn parse_gpg(args: &[String]) -> Result<Call, String> {
    let (mut status_fd, mut key, mut signature) = (None, None, None);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = |what: &str| {
            args.next()
                .cloned()
                .ok_or_else(|| format!("gpg {arg} needs {what}"))
        };
        match arg.as_str() {
            "--keyid-format=long" => {}
            "-bsau" => key = Some(value("a key")?),
            "--verify" => {
                signature = Some(PathBuf::from(value("a file")?));
                if value("-")? != "-" {
                    return Err("gpg --verify takes the data on stdin only".into());
                }
            }
            _ => match arg.strip_prefix("--status-fd=") {
                Some(fd) => status_fd = Some(fd.parse().map_err(|_| format!("bad {arg}"))?),
                None => return Err(format!("unsupported gpg argument {arg:?}")),
            },
        }
    }
    let action = match (&key, &signature) {
        (Some(_), None) => Action::Sign,
        (None, Some(_)) => Action::Verify,
        _ => return Err("gpg is asked to sign (-bsau KEY) or to verify (--verify FILE -)".into()),
    };
    let mut request = Request::new(Format::OpenPgp, action);
    request.key = key;
    request.status_fd = status_fd;
    request.check()?;
    Ok(Call {
        request,
        signature,
        message: None,
    })
}

fn parse_ssh_keygen(args: &[String]) -> Result<Call, String> {
    let mut args = args.iter();
    let name = args.next().ok_or("ssh-keygen -Y needs an action")?;
    let action = Action::parse(name).ok_or_else(|| format!("unsupported ssh-keygen -Y {name}"))?;
    let mut request = Request::new(Format::Ssh, action);
    let (mut signature, mut files) = (None, Vec::new());
    while let Some(arg) = args.next() {
        let mut value = |what: &str| {
            args.next()
                .cloned()
                .ok_or_else(|| format!("ssh-keygen {arg} needs {what}"))
        };
        match arg.as_str() {
            "-n" => request.namespace = Some(value("a namespace")?),
            "-f" => {
                let file = value("a file")?;
                match action {
                    Action::Sign => request.key = Some(file),
                    _ => request.allowed = Some(file),
                }
            }
            "-U" => request.agent = true,
            "-s" => signature = Some(PathBuf::from(value("a file")?)),
            "-I" => request.principal = Some(value("a principal")?),
            "-r" => request.revoked = Some(value("a file")?),
            "-O" => request.time = Some(verify_time(&value("an option")?)?),
            _ if arg.starts_with("-O") => request.time = Some(verify_time(&arg[2..])?),
            _ if arg.starts_with('-') => {
                return Err(format!("unsupported ssh-keygen argument {arg:?}"));
            }
            _ => files.push(PathBuf::from(arg)),
        }
    }
    let message = match (action, files.as_slice()) {
        (Action::Sign, [file]) => Some(file.clone()),
        (Action::Sign, _) => return Err("ssh-keygen -Y sign is asked to sign one file".into()),
        (_, []) => None,
        _ => return Err(format!("ssh-keygen -Y {name} takes no files")),
    };
    if action != Action::Sign && signature.is_none() {
        return Err(format!("ssh-keygen -Y {name} needs a signature (-s FILE)"));
    }
    request.check()?;
    Ok(Call {
        request,
        signature,
        message,
    })
}

/// The value of `-O verify-time=TIME`, the only option git passes.
fn verify_time(option: &str) -> Result<String, String> {
    (option.strip_prefix("verify-time="))
        .map(str::to_owned)
        .ok_or_else(|| format!("unsupported ssh-keygen option -O {option}"))
}

/// The frames of a call: the payload, the signature and the files the
/// sandbox can read, read from `stdin` and the sandbox's files.
pub(in crate::proxy) fn input_frames(
    call: &Call,
    stdin: impl Read,
) -> Result<Vec<(u8, Vec<u8>)>, String> {
    let mut frames = Vec::new();
    let request = &call.request;
    let payload = match &call.message {
        Some(message) => read_limited(File::open(message), MAX_INPUT as u64)
            .map_err(|error| format!("{}: {error}", message.display()))?,
        None if request.action.has_payload() => {
            read_limited(Ok(stdin), MAX_INPUT as u64).map_err(|error| format!("stdin: {error}"))?
        }
        None => Vec::new(),
    };
    frames.push((PAYLOAD, payload));
    if let Some(signature) = &call.signature {
        let data = read_limited(File::open(signature), MAX_FILE)
            .map_err(|error| format!("{}: {error}", signature.display()))?;
        frames.push((SIGNATURE, data));
    }
    // A file the sandbox cannot read, such as one in ~/.ssh, goes by its
    // path only, and the proxy finds it on this machine.
    let files = [
        (
            KEY,
            request
                .key
                .as_ref()
                .filter(|_| request.format == Format::Ssh),
        ),
        (ALLOWED, request.allowed.as_ref()),
        (REVOKED, request.revoked.as_ref()),
    ];
    for (kind, path) in files {
        if let Some(path) = path
            && let Ok(data) = read_limited(File::open(path), MAX_FILE)
        {
            frames.push((kind, data));
        }
    }
    Ok(frames)
}

/// Everything `input` holds, unless that is more than `limit` bytes.
fn read_limited(input: io::Result<impl Read>, limit: u64) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    input?.take(limit + 1).read_to_end(&mut data)?;
    if data.len() as u64 > limit {
        return Err(io::Error::other("too large"));
    }
    Ok(data)
}

/// The exit code for a failure of the bridge itself: gpg's for an error,
/// and ssh-keygen's.
fn failed(format: Option<Format>) -> i32 {
    match format {
        Some(Format::Ssh) => 255,
        _ => 2,
    }
}

/// The bridge's signing mode, which git runs as `gpg` and `ssh-keygen` in
/// a sandbox (see the module docs). Exits with the program's code.
pub fn git_sign_main(args: impl IntoIterator<Item = OsString>) -> i32 {
    let args: Option<Vec<String>> = args.into_iter().map(|a| a.into_string().ok()).collect();
    let call = match args.ok_or_else(|| "arguments that are not UTF-8".to_owned()) {
        Ok(args) => parse_args(&args),
        Err(error) => Err(error),
    };
    let call = match call {
        Ok(call) => call,
        Err(error) => {
            eprintln!("cmdterm sandbox: signing: {error}");
            return failed(None);
        }
    };
    let code = failed(Some(call.request.format));
    let Some((host, port, auth)) = proxy_from_env(|key| std::env::var(key).ok()) else {
        eprintln!("cmdterm sandbox: signing: no proxy in HTTP_PROXY, so git cannot sign or verify");
        return code;
    };
    let frames = match input_frames(&call, io::stdin().lock()) {
        Ok(frames) => frames,
        Err(error) => {
            eprintln!("cmdterm sandbox: signing: {error}");
            return code;
        }
    };
    let stream = match TcpStream::connect((host.as_str(), port)) {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("cmdterm sandbox: signing: the proxy at {host}:{port}: {error}");
            return code;
        }
    };
    let head = call.request.head(auth.as_deref());
    let mut stderr = io::stderr();
    if let Some(message) = &call.message {
        // ssh-keygen writes the signature of FILE to FILE.sig.
        let mut signature = Vec::new();
        let status = exchange(stream, &head, &frames, &mut signature, &mut stderr);
        if status != 0 {
            return status;
        }
        let mut path = message.clone().into_os_string();
        path.push(".sig");
        if let Err(error) = std::fs::write(Path::new(&path), &signature) {
            eprintln!(
                "cmdterm sandbox: signing: {}: {error}",
                Path::new(&path).display()
            );
            return code;
        }
        return 0;
    }
    exchange(stream, &head, &frames, &mut io::stdout(), &mut stderr)
}

/// Sends `head` and `frames`, then writes what the proxy sends to
/// `stdout` and `stderr` and returns the exit code.
pub(in crate::proxy) fn exchange(
    mut stream: TcpStream,
    head: &str,
    frames: &[(u8, Vec<u8>)],
    mut stdout: impl Write,
    mut stderr: impl Write,
) -> i32 {
    let format = Request::parse(head).ok().map(|r| r.format);
    let code = failed(format);
    let mut send = || -> io::Result<()> {
        stream.write_all(head.as_bytes())?;
        for (kind, data) in frames {
            for chunk in data.chunks(CHUNK) {
                write_frame(&mut stream, *kind, chunk)?;
            }
        }
        write_frame(&mut stream, END, &[])
    };
    if let Err(error) = send() {
        let _ = writeln!(stderr, "cmdterm sandbox: signing: the proxy: {error}");
        return code;
    }
    let Some((answer, rest)) = read_head(&mut stream) else {
        let _ = writeln!(
            stderr,
            "cmdterm sandbox: signing: the proxy closed the connection"
        );
        return code;
    };
    let answer = String::from_utf8_lossy(&answer);
    if answer.split(' ').nth(1) != Some("200") {
        let _ = stderr.write_all(&rest);
        let _ = io::copy(&mut stream, &mut stderr);
        return code;
    }
    let mut input = io::Cursor::new(rest).chain(stream);
    let mut data = Vec::new();
    loop {
        match read_frame(&mut input, &mut data) {
            Ok(Some(STDOUT)) => {
                if stdout
                    .write_all(&data)
                    .and_then(|()| stdout.flush())
                    .is_err()
                {
                    return code;
                }
            }
            Ok(Some(STDERR)) => {
                let _ = stderr.write_all(&data);
                let _ = stderr.flush();
            }
            Ok(Some(EXIT)) => {
                return <[u8; 4]>::try_from(&data[..]).map_or(code, i32::from_be_bytes);
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                let _ = writeln!(
                    stderr,
                    "cmdterm sandbox: signing: the proxy closed the connection"
                );
                return code;
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
    fn gits_gpg_command_lines_read_as_sign_and_verify() {
        let call =
            parse_args(&strings(&["--status-fd=2", "-bsau", "Me <me@example.com>"])).unwrap();
        assert_eq!(call.request.action, Action::Sign);
        assert_eq!(call.request.key.as_deref(), Some("Me <me@example.com>"));
        assert_eq!(call.request.status_fd, Some(2));
        let call = parse_args(&strings(&[
            "--keyid-format=long",
            "--status-fd=1",
            "--verify",
            "/tmp/.git_vtag_tmpX",
            "-",
        ]))
        .unwrap();
        assert_eq!(call.request.action, Action::Verify);
        assert_eq!(call.signature, Some(PathBuf::from("/tmp/.git_vtag_tmpX")));
        for bad in [
            &["--status-fd=2", "-bsau"][..],
            &["--status-fd=2", "-bsau", "k", "--homedir", "/x"],
            &["--status-fd=2", "-bsau", "--export-secret-keys"],
            &["--status-fd=3", "-bsau", "k"],
            &["--verify", "f", "data"],
            &["--status-fd=2"],
        ] {
            assert!(parse_args(&strings(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn gits_ssh_keygen_command_lines_read_as_their_actions() {
        let call = parse_args(&strings(&[
            "-Y",
            "sign",
            "-n",
            "git",
            "-f",
            "/home/me/.ssh/id.pub",
            "-U",
            "/tmp/.git_signing_buffer_tmpA",
        ]))
        .unwrap();
        assert_eq!(call.request.action, Action::Sign);
        assert_eq!(call.request.key.as_deref(), Some("/home/me/.ssh/id.pub"));
        assert!(call.request.agent);
        assert_eq!(
            call.message,
            Some(PathBuf::from("/tmp/.git_signing_buffer_tmpA"))
        );

        let call = parse_args(&strings(&[
            "-Y",
            "find-principals",
            "-f",
            "/a",
            "-s",
            "/tmp/s",
            "-Overify-time=20260101120000",
        ]))
        .unwrap();
        assert_eq!(call.request.action, Action::FindPrincipals);
        assert_eq!(call.request.allowed.as_deref(), Some("/a"));
        assert_eq!(call.request.time.as_deref(), Some("20260101120000"));

        let call = parse_args(&strings(&[
            "-Y",
            "verify",
            "-n",
            "git",
            "-f",
            "/a",
            "-I",
            "me@example.com",
            "-s",
            "/tmp/s",
            "-r",
            "/r",
        ]))
        .unwrap();
        assert_eq!(call.request.principal.as_deref(), Some("me@example.com"));
        assert_eq!(call.request.revoked.as_deref(), Some("/r"));
        assert!(call.message.is_none());

        for bad in [
            &["-Y", "sign", "-n", "git", "-f", "/k"][..],
            &["-Y", "sign", "-n", "git", "-f", "/k", "a", "b"],
            &["-Y", "check-novalidate", "-n", "git"],
            &["-Y", "verify", "-n", "git", "-f", "/a", "-s", "/s"],
            &[
                "-Y",
                "check-novalidate",
                "-n",
                "git",
                "-s",
                "/s",
                "-O",
                "hashalg=sha1",
            ],
            &["-Y", "sign", "-n", "git", "-f", "/k", "-D", "/lib.so", "f"],
            &["-Y", "shell"],
            &["-Y"],
        ] {
            assert!(parse_args(&strings(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_bridge_tells_signers_from_its_own_flags() {
        for first in ["--status-fd=2", "--keyid-format=long", "-Y"] {
            assert!(is_git_signer(OsStr::new(first)), "{first}");
        }
        for first in ["--scope", "--port", "--git-ssh", "--", "-y"] {
            assert!(!is_git_signer(OsStr::new(first)), "{first}");
        }
    }

    #[test]
    fn readable_files_go_with_the_request_and_hidden_ones_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let (message, key) = (dir.path().join("buffer"), dir.path().join("key.pub"));
        std::fs::write(&message, "tree 1\n\nhi\n").unwrap();
        std::fs::write(&key, "ssh-ed25519 AAAA me").unwrap();
        let hidden = dir.path().join("hidden/allowed");
        let args = strings(&[
            "-Y",
            "sign",
            "-n",
            "git",
            "-f",
            key.to_str().unwrap(),
            message.to_str().unwrap(),
        ]);
        let frames = input_frames(&parse_args(&args).unwrap(), io::empty()).unwrap();
        assert_eq!(
            frames,
            [
                (PAYLOAD, b"tree 1\n\nhi\n".to_vec()),
                (KEY, b"ssh-ed25519 AAAA me".to_vec())
            ]
        );
        let signature = dir.path().join("sig");
        std::fs::write(&signature, "SIG").unwrap();
        let args = strings(&[
            "-Y",
            "check-novalidate",
            "-n",
            "git",
            "-s",
            signature.to_str().unwrap(),
        ]);
        let frames = input_frames(&parse_args(&args).unwrap(), &b"data"[..]).unwrap();
        assert_eq!(
            frames,
            [(PAYLOAD, b"data".to_vec()), (SIGNATURE, b"SIG".to_vec())]
        );
        let args = strings(&[
            "-Y",
            "find-principals",
            "-f",
            hidden.to_str().unwrap(),
            "-s",
            signature.to_str().unwrap(),
        ]);
        // find-principals reads no stdin, and the hidden file goes by path.
        let frames = input_frames(&parse_args(&args).unwrap(), &b"unread"[..]).unwrap();
        assert_eq!(
            frames,
            [(PAYLOAD, Vec::new()), (SIGNATURE, b"SIG".to_vec())]
        );
    }
}
