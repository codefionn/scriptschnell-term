//! The proxy's side of signing (see `proxy/sign.rs`): it reads the
//! request, asks under [`GIT_SIGN_GUARD`] before it signs, runs this
//! machine's `gpg` or `ssh-keygen` and relays their output.
//!
//! The programs are the ones this machine's git configuration names
//! (`gpg.openpgp.program` or `gpg.program`, `gpg.ssh.program`), read
//! outside any repository, so a repository in a sandbox cannot pick them.
//! They run with the app's environment, so they reach the user's GPG and
//! SSH agents, and a PIN or a touch of a security key is asked for on this
//! machine.
//!
//! An ssh key, allowed signers or revocation file that the sandbox could
//! read comes with the request and goes to a temporary file. One it could
//! not, such as one in `~/.ssh`, is read on this machine: a key file by the
//! path git named, and an allowed signers or revocation file only when it
//! is the one this machine's git configuration names
//! (`gpg.ssh.allowedSignersFile`, `gpg.ssh.revocationFile`), since
//! verifying does not ask. `gpg --verify` runs with
//! `--no-auto-key-retrieve`, so a signature from a sandbox never makes it
//! fetch a key from the network.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use super::{Duplex, Shared, respond};
use crate::Environment;
use crate::proxy::rules::GuardMode;
use crate::proxy::sign::{
    ALLOWED, Action, END, Format, GIT_SIGN_GUARD, KEY, LITERAL_KEY_FILE, MAX_INPUT, PAYLOAD,
    REVOKED, Request, SIGNATURE, describe,
};
use crate::proxy::ssh::{EXIT, STDERR, STDOUT, read_frame, write_frame};

/// How long the proxy waits for the frames after the head.
const INPUT_TIMEOUT: Duration = Duration::from_secs(30);

/// The largest frame the proxy sends.
const CHUNK: usize = 512 * 1024;

/// The programs and files of this machine's git configuration that
/// signing uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Signers {
    pub gpg: OsString,
    pub ssh_keygen: OsString,
    pub allowed: Option<PathBuf>,
    pub revoked: Option<PathBuf>,
}

impl Signers {
    /// Reads them from git's system and global configuration.
    fn from_git_config() -> Self {
        let program = |keys: &[&str], default: &str| {
            (keys.iter())
                .find_map(|key| git_config(key, false))
                .unwrap_or_else(|| default.to_owned())
                .into()
        };
        Self {
            gpg: program(&["gpg.openpgp.program", "gpg.program"], "gpg"),
            ssh_keygen: program(&["gpg.ssh.program"], "ssh-keygen"),
            allowed: git_config("gpg.ssh.allowedSignersFile", true).map(PathBuf::from),
            revoked: git_config("gpg.ssh.revocationFile", true).map(PathBuf::from),
        }
    }
}

/// `git config --get KEY` outside any repository, with `~` expanded for
/// a `path`.
fn git_config(key: &str, path: bool) -> Option<String> {
    let mut command = Environment::Local
        .command("git")
        .cwd("/")
        .args(["config", "--get"]);
    if path {
        command = command.arg("--type=path");
    }
    let output = (command.arg(key).to_std())
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let value = String::from_utf8_lossy(&output.stdout);
    let value = value.trim_end_matches(['\r', '\n']);
    (output.status.success() && !value.is_empty()).then(|| value.to_owned())
}

/// What the frames of a request carry.
#[derive(Debug, Default)]
struct Input {
    payload: Vec<u8>,
    signature: Option<Vec<u8>>,
    key: Option<Vec<u8>>,
    allowed: Option<Vec<u8>>,
    revoked: Option<Vec<u8>>,
}

fn read_input(input: &mut impl Read) -> Result<Input, String> {
    let (mut got, mut data, mut total) = (Input::default(), Vec::new(), 0);
    loop {
        let kind = read_frame(input, &mut data)
            .map_err(|error| error.to_string())?
            .ok_or("the request ended early")?;
        if kind == END {
            return Ok(got);
        }
        total += data.len();
        if total > MAX_INPUT {
            return Err("the request is too large".into());
        }
        let slot = match kind {
            PAYLOAD => {
                got.payload.extend_from_slice(&data);
                continue;
            }
            SIGNATURE => &mut got.signature,
            KEY => &mut got.key,
            ALLOWED => &mut got.allowed,
            REVOKED => &mut got.revoked,
            _ => continue,
        };
        slot.get_or_insert_with(Vec::new).extend_from_slice(&data);
    }
}

/// A command to run, with the temporary files it reads.
struct Prepared {
    program: OsString,
    args: Vec<OsString>,
    stdin: Vec<u8>,
    files: Vec<tempfile::NamedTempFile>,
}

impl Prepared {
    fn new(program: &OsString) -> Self {
        Self {
            program: program.clone(),
            args: Vec::new(),
            stdin: Vec::new(),
            files: Vec::new(),
        }
    }

    fn arg(&mut self, arg: impl Into<OsString>) {
        self.args.push(arg.into());
    }

    /// A temporary file of this user's alone that holds `data`.
    fn temp(&mut self, data: &[u8]) -> Result<OsString, String> {
        let mut file = (tempfile::Builder::new().prefix("cmdterm-sign-").tempfile())
            .map_err(|error| format!("a temporary file: {error}"))?;
        (file.write_all(data).and_then(|()| file.flush()))
            .map_err(|error| format!("a temporary file: {error}"))?;
        let path = file.path().as_os_str().to_owned();
        self.files.push(file);
        Ok(path)
    }
}

/// The command for `request`, or why there is none.
fn prepare(request: &Request, input: &Input, signers: &Signers) -> Result<Prepared, String> {
    let signature = |prepared: &mut Prepared| match &input.signature {
        Some(data) => prepared.temp(data),
        None => Err(format!("{} needs a signature", request.action.name())),
    };
    let value = |value: &Option<String>| value.clone().unwrap_or_default();
    let status_fd = |default: u8| format!("--status-fd={}", request.status_fd.unwrap_or(default));
    let mut prepared = match request.format {
        Format::OpenPgp => Prepared::new(&signers.gpg),
        Format::Ssh => Prepared::new(&signers.ssh_keygen),
    };
    match (request.format, request.action) {
        (Format::OpenPgp, Action::Sign) => {
            prepared.arg(status_fd(2));
            prepared.arg("-bsau");
            prepared.arg(value(&request.key));
        }
        (Format::OpenPgp, Action::Verify) => {
            let signature = signature(&mut prepared)?;
            for arg in ["--keyid-format=long", "--no-auto-key-retrieve"] {
                prepared.arg(arg);
            }
            prepared.arg(status_fd(1));
            prepared.arg("--verify");
            prepared.arg(signature);
            prepared.arg("-");
        }
        (Format::OpenPgp, action) => return Err(format!("gpg does not {}", action.name())),
        (Format::Ssh, action) => {
            prepared.arg("-Y");
            prepared.arg(action.name());
            if let Some(namespace) = &request.namespace {
                prepared.arg("-n");
                prepared.arg(namespace);
            }
            if action == Action::Sign {
                let key = ssh_key_file(request, input, &mut prepared)?;
                prepared.arg("-f");
                prepared.arg(key);
                if request.agent {
                    prepared.arg("-U");
                }
            } else {
                if let Some(allowed) = &request.allowed {
                    let configured = signers.allowed.as_deref();
                    let file = host_file(
                        "allowed signers file",
                        "gpg.ssh.allowedSignersFile",
                        allowed,
                        input.allowed.as_deref(),
                        configured,
                        &mut prepared,
                    )?;
                    prepared.arg("-f");
                    prepared.arg(file);
                }
                if let Some(principal) = &request.principal {
                    prepared.arg("-I");
                    prepared.arg(principal);
                }
                let signature = signature(&mut prepared)?;
                prepared.arg("-s");
                prepared.arg(signature);
                if let Some(time) = &request.time {
                    prepared.arg(format!("-Overify-time={time}"));
                }
                if let Some(revoked) = &request.revoked {
                    let configured = signers.revoked.as_deref();
                    let file = host_file(
                        "revocation file",
                        "gpg.ssh.revocationFile",
                        revoked,
                        input.revoked.as_deref(),
                        configured,
                        &mut prepared,
                    )?;
                    prepared.arg("-r");
                    prepared.arg(file);
                }
            }
        }
    }
    if request.action.has_payload() {
        prepared.stdin = input.payload.clone();
    }
    Ok(prepared)
}

/// Whether `path` is the file git writes a literal key to.
fn is_literal_key(path: &str) -> bool {
    (Path::new(path).file_name())
        .is_some_and(|name| (name.to_str()).is_some_and(|name| name.starts_with(LITERAL_KEY_FILE)))
}

/// The key file `ssh-keygen -Y sign` reads on this machine: the one git
/// named when it is here, since `ssh-keygen` finds the private key next to
/// a public one, else a copy of the one the sandbox sent.
fn ssh_key_file(
    request: &Request,
    input: &Input,
    prepared: &mut Prepared,
) -> Result<OsString, String> {
    let key = request.key.as_deref().unwrap_or_default();
    let here = Path::new(key);
    if !is_literal_key(key) && here.is_absolute() && here.is_file() {
        return Ok(here.as_os_str().to_owned());
    }
    match &input.key {
        Some(data) => prepared.temp(data),
        None => Err(format!(
            "the key file {key} is not on this machine and the sandbox cannot read it"
        )),
    }
}

/// A copy of the file the sandbox sent, or the file at `path` on this
/// machine when it is the `configured` one of git's `setting`.
fn host_file(
    what: &str,
    setting: &str,
    path: &str,
    sent: Option<&[u8]>,
    configured: Option<&Path>,
    prepared: &mut Prepared,
) -> Result<OsString, String> {
    if let Some(data) = sent {
        return prepared.temp(data);
    }
    let named = crate::canonicalize(Path::new(path)).ok();
    let known = configured.and_then(|path| crate::canonicalize(path).ok());
    match (named, known) {
        (Some(named), Some(known)) if named == known => Ok(named.into_os_string()),
        _ => Err(format!(
            "the sandbox cannot read the {what} {path}, and it is not the one {setting} names in \
             this machine's git configuration (git config --global {setting} {path} makes it so)"
        )),
    }
}

/// How the question names the key: the gpg key as git named it, the path
/// of an ssh key file, or for a literal ssh key its type, the end of the
/// key and its comment.
fn key_label(request: &Request, input: &Input) -> String {
    let key = request.key.as_deref().unwrap_or_default();
    if request.format == Format::Ssh
        && is_literal_key(key)
        && let Some(line) = input.key.as_deref().map(String::from_utf8_lossy)
    {
        let mut words = line.split_whitespace();
        if let (Some(kind), Some(blob)) = (words.next(), words.next()) {
            let tail: String = (blob
                .chars()
                .rev()
                .take(12)
                .collect::<Vec<_>>()
                .into_iter()
                .rev())
            .collect();
            let comment: Vec<&str> = words.collect();
            let label = format!("{kind} ...{tail} {}", comment.join(" "));
            return label.trim_end().to_owned();
        }
    }
    key.to_owned()
}

/// Lets the signature be made, or says why not.
fn decide(
    shared: &Shared,
    scope: Option<&str>,
    request: &Request,
    input: &Input,
) -> Result<(), String> {
    let guard = (shared.rules.read().ok()).and_then(|rules| {
        (rules.guards.iter())
            .find(|g| g.id == GIT_SIGN_GUARD)
            .cloned()
    });
    // The rules leave out the guards that allow.
    let Some(guard) = guard else {
        return Ok(());
    };
    let (kind, about) = describe(&input.payload);
    let key = key_label(request, input);
    let allowed = match guard.mode {
        GuardMode::Allow => true,
        GuardMode::Block => false,
        GuardMode::Ask => {
            let approvals = shared.approvals.read().ok().and_then(|a| a.clone());
            approvals.is_some_and(|approvals| {
                approvals.ask_noting(&guard, scope, &key, request.format.name(), kind, about)
            })
        }
    };
    if allowed {
        return Ok(());
    }
    Err(format!(
        "cmdterm sandbox: {} is blocked (a {kind} with {key}). Settings, Extensions decides \
         whether sandboxes may sign with your keys.\n",
        guard.title
    ))
}

/// Serves one `SIGN` request, whose head is `head`. `early` holds what
/// the client sent after it.
pub(super) fn serve<S: Duplex>(
    shared: &Arc<Shared>,
    mut client: S,
    head: &str,
    early: Vec<u8>,
    scope: Option<Arc<str>>,
) {
    let request = match Request::parse(head) {
        Ok(request) => request,
        Err(error) => {
            let body = format!("cmdterm sandbox: signing: {error}\n");
            return respond(&mut client, "400 Bad Request", &body);
        }
    };
    client.set_timeout(Some(INPUT_TIMEOUT));
    let input = read_input(&mut io::Cursor::new(early).chain(&mut client));
    client.set_timeout(None);
    let input = match input {
        Ok(input) => input,
        Err(error) => {
            let body = format!("cmdterm sandbox: signing: {error}\n");
            return respond(&mut client, "400 Bad Request", &body);
        }
    };
    let signers = (shared.signers.read().ok())
        .and_then(|signers| signers.clone())
        .unwrap_or_else(Signers::from_git_config);
    let prepared = match prepare(&request, &input, &signers) {
        Ok(prepared) => prepared,
        Err(error) => {
            let body = format!("cmdterm sandbox: signing: {error}\n");
            return respond(&mut client, "403 Forbidden", &body);
        }
    };
    if request.action == Action::Sign
        && let Err(body) = decide(shared, scope.as_deref(), &request, &input)
    {
        return respond(&mut client, "403 Forbidden", &body);
    }
    let output = match run(prepared) {
        Ok(output) => output,
        Err((program, error)) => {
            let body = format!(
                "cmdterm sandbox: signing: {}: {error}\n",
                program.to_string_lossy()
            );
            return respond(&mut client, "502 Bad Gateway", &body);
        }
    };
    let failed = if request.format == Format::Ssh {
        255
    } else {
        2
    };
    let code = output.status.code().unwrap_or(failed);
    let mut send = || -> io::Result<()> {
        client.write_all(b"HTTP/1.1 200 OK\r\n\r\n")?;
        for (kind, data) in [(STDOUT, &output.stdout), (STDERR, &output.stderr)] {
            for chunk in data.chunks(CHUNK) {
                write_frame(&mut client, kind, chunk)?;
            }
        }
        write_frame(&mut client, EXIT, &code.to_be_bytes())
    };
    let _ = send();
    client.close_write();
}

/// Runs `prepared` with its input and waits for it.
fn run(prepared: Prepared) -> Result<Output, (OsString, io::Error)> {
    let Prepared {
        program,
        args,
        stdin,
        files,
    } = prepared;
    let child = (Environment::Local.command(&program).args(args).to_std())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = child.map_err(|error| (program.clone(), error))?;
    let pipe = child.stdin.take();
    let writer = thread::spawn(move || {
        if let Some(mut pipe) = pipe {
            let _ = pipe.write_all(&stdin);
        }
    });
    let output = child.wait_with_output();
    let _ = writer.join();
    drop(files);
    output.map_err(|error| (program, error))
}

#[cfg(all(test, unix))]
mod tests {
    use std::net::TcpStream;
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::proxy::rules::{ProxyRules, builtin_guards};
    use crate::proxy::sign::client::{exchange, input_frames, parse_args};
    use crate::proxy::{Approvals, NetworkProxy};

    const COMMIT: &str = "tree 4b82\nauthor A <a@b> 1 +0000\ncommitter A <a@b> 1 +0000\n\n\
                          Fix the parser\n";

    /// A stand-in for gpg and ssh-keygen: prints its arguments to stderr,
    /// each temporary file it gets as `file=CONTENT`, and `SIG(STDIN)` to
    /// stdout.
    fn fake(dir: &Path, name: &str) -> OsString {
        let program = dir.join(name);
        let script = "#!/bin/sh\nprintf '%s\\n' \"$@\" >&2\n\
                      for a; do case \"$a\" in */cmdterm-sign-*) printf 'file=%s\\n' \"$(cat \"$a\")\" >&2 ;; esac; done\n\
                      printf 'SIG(%s)' \"$(cat)\"\n";
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        program.into_os_string()
    }

    fn proxy(dir: &Path, allowed: Option<PathBuf>) -> NetworkProxy {
        let proxy = NetworkProxy::start(&dir.join("p.sock"), Vec::new()).unwrap();
        proxy.set_signers(Signers {
            gpg: fake(dir, "gpg"),
            ssh_keygen: fake(dir, "ssh-keygen"),
            allowed,
            revoked: None,
        });
        proxy.set_rules(ProxyRules {
            secrets: Vec::new(),
            guards: builtin_guards(),
        });
        proxy
    }

    /// Runs git's command line through `proxy` as the bridge does, with
    /// `stdin`. Returns the exit code, stdout and stderr.
    fn run(proxy: &NetworkProxy, args: &[&str], stdin: &str) -> (i32, String, String) {
        let endpoint = proxy.endpoint();
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        let call = parse_args(&args).unwrap();
        let frames = input_frames(&call, stdin.as_bytes()).unwrap();
        let auth = endpoint.token.as_ref().unwrap().header();
        let head = call.request.head(Some(&auth));
        let stream = TcpStream::connect(("127.0.0.1", endpoint.port)).unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = exchange(stream, &head, &frames, &mut out, &mut err);
        let text = |bytes: Vec<u8>| String::from_utf8(bytes).unwrap();
        (code, text(out), text(err))
    }

    /// Answers the next question with `allow` and returns it.
    fn answer(
        approvals: &Arc<Approvals>,
        allow: bool,
    ) -> thread::JoinHandle<Option<crate::Approval>> {
        let approvals = Arc::clone(approvals);
        thread::spawn(move || {
            for _ in 0..250 {
                if let Some(question) = approvals.pending().first().cloned() {
                    approvals.answer(question.id, allow);
                    return Some(question);
                }
                thread::sleep(Duration::from_millis(20));
            }
            None
        })
    }

    #[test]
    fn gpg_signs_on_this_machine_after_asking_once_per_key() {
        let dir = tempfile::tempdir().unwrap();
        let proxy = proxy(dir.path(), None);
        let sign = ["--status-fd=2", "-bsau", "ABCD1234"];

        // Without anyone to ask, the guard refuses.
        let (code, out, err) = run(&proxy, &sign, COMMIT);
        assert_eq!(code, 2);
        assert!(out.is_empty());
        assert!(err.contains("is blocked (a commit with ABCD1234)"), "{err}");

        let approvals = Arc::new(Approvals::new(|| {}));
        proxy.set_approvals(Some(Arc::clone(&approvals)));
        let asked = answer(&approvals, true);
        let (code, out, err) = run(&proxy, &sign, COMMIT);
        let question = asked.join().unwrap().expect("no question came");
        assert_eq!(code, 0, "{err}");
        assert_eq!(out, format!("SIG({})", COMMIT.trim_end()));
        assert!(err.starts_with("--status-fd=2\n-bsau\nABCD1234\n"), "{err}");
        assert_eq!(question.guard, GIT_SIGN_GUARD);
        assert_eq!(question.host, "ABCD1234");
        assert_eq!(question.method, "openpgp");
        assert_eq!(question.target, "commit");
        assert_eq!(question.detail.as_deref(), Some("Fix the parser"));

        // The next commit of a rebase passes on the same answer.
        let next = COMMIT.replace("Fix the parser", "Fix the lexer");
        let (code, _, err) = run(&proxy, &sign, &next);
        assert_eq!(code, 0, "{err}");
        assert!(approvals.pending().is_empty());

        // Another key asks again, and a refusal blocks it.
        let asked = answer(&approvals, false);
        let (code, _, err) = run(&proxy, &["--status-fd=2", "-bsau", "FFFF0000"], COMMIT);
        assert_eq!(asked.join().unwrap().unwrap().host, "FFFF0000");
        assert_eq!(code, 2);
        assert!(err.contains("is blocked"), "{err}");
    }

    #[test]
    fn gpg_verifies_without_asking_or_fetching_keys() {
        let dir = tempfile::tempdir().unwrap();
        let proxy = proxy(dir.path(), None);
        let signature = dir.path().join("sig");
        std::fs::write(&signature, "SIGNATURE").unwrap();
        let verify = [
            "--keyid-format=long",
            "--status-fd=1",
            "--verify",
            signature.to_str().unwrap(),
            "-",
        ];
        let (code, out, err) = run(&proxy, &verify, "payload");
        assert_eq!(code, 0, "{err}");
        assert_eq!(out, "SIG(payload)");
        assert!(
            err.starts_with(
                "--keyid-format=long\n--no-auto-key-retrieve\n--status-fd=1\n--verify\n"
            ),
            "{err}"
        );
        assert!(err.contains("file=SIGNATURE\n"), "{err}");
    }

    #[test]
    fn ssh_keygen_signs_with_a_literal_key_and_names_it_in_the_question() {
        let dir = tempfile::tempdir().unwrap();
        let proxy = proxy(dir.path(), None);
        let approvals = Arc::new(Approvals::new(|| {}));
        proxy.set_approvals(Some(Arc::clone(&approvals)));
        let key = dir.path().join(".git_signing_key_tmpXYZ");
        std::fs::write(
            &key,
            "sk-ssh-ed25519@openssh.com AAAAGnNrLXNzaC1lZDI1NTE5QG9wZW5zc2guY29t me@yubikey\n",
        )
        .unwrap();
        let buffer = dir.path().join(".git_signing_buffer_tmpXYZ");
        std::fs::write(&buffer, COMMIT).unwrap();
        let args = [
            "-Y",
            "sign",
            "-n",
            "git",
            "-f",
            key.to_str().unwrap(),
            "-U",
            buffer.to_str().unwrap(),
        ];
        let asked = answer(&approvals, true);
        let (code, out, err) = run(&proxy, &args, "");
        let question = asked.join().unwrap().expect("no question came");
        assert_eq!(code, 0, "{err}");
        assert_eq!(out, format!("SIG({})", COMMIT.trim_end()));
        assert!(err.starts_with("-Y\nsign\n-n\ngit\n-f\n"), "{err}");
        assert!(err.contains("\n-U\n"), "{err}");
        assert!(
            err.contains("file=sk-ssh-ed25519@openssh.com AAAA"),
            "{err}"
        );
        assert_eq!(
            question.host,
            "sk-ssh-ed25519@openssh.com ...ZW5zc2guY29t me@yubikey"
        );
        assert_eq!(question.method, "ssh");

        // A key file in a hidden directory is read here, by its path.
        let hidden = dir.path().join("id_ed25519.pub");
        std::fs::write(&hidden, "ssh-ed25519 AAAA").unwrap();
        let call = parse_args(&[
            "-Y".into(),
            "sign".into(),
            "-n".into(),
            "git".into(),
            "-f".into(),
            hidden.to_str().unwrap().into(),
            buffer.to_str().unwrap().into(),
        ])
        .unwrap();
        let mut prepared = Prepared::new(&"ssh-keygen".into());
        let file = ssh_key_file(&call.request, &Input::default(), &mut prepared).unwrap();
        assert_eq!(file, hidden.as_os_str());
        assert!(prepared.files.is_empty());
    }

    #[test]
    fn a_hidden_allowed_signers_file_must_be_the_configured_one() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("allowed_signers");
        std::fs::write(&allowed, "me@example.com ssh-ed25519 AAAA\n").unwrap();
        let signature = dir.path().join("sig");
        std::fs::write(&signature, "SIGNATURE").unwrap();
        // The sandbox cannot read a file named in another directory.
        let elsewhere = dir.path().join("hidden/allowed_signers");
        let find = |path: &Path| {
            vec![
                "-Y".to_owned(),
                "find-principals".into(),
                "-f".into(),
                path.to_str().unwrap().into(),
                "-s".into(),
                signature.to_str().unwrap().into(),
                "-Overify-time=20260101120000".into(),
            ]
        };
        let proxy = proxy(dir.path(), Some(elsewhere.clone()));
        let args = find(&elsewhere);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let (code, _, err) = run(&proxy, &args, "");
        assert_eq!(code, 255);
        assert!(err.contains("gpg.ssh.allowedSignersFile"), "{err}");

        // The configured file is read in place.
        std::fs::create_dir(dir.path().join("hidden")).unwrap();
        std::fs::write(&elsewhere, "me@example.com ssh-ed25519 AAAA\n").unwrap();
        let mut prepared = Prepared::new(&"ssh-keygen".into());
        let file = host_file(
            "allowed signers file",
            "gpg.ssh.allowedSignersFile",
            elsewhere.to_str().unwrap(),
            None,
            Some(&elsewhere),
            &mut prepared,
        )
        .unwrap();
        assert_eq!(
            PathBuf::from(file),
            crate::canonicalize(&elsewhere).unwrap()
        );
        let other = host_file(
            "allowed signers file",
            "gpg.ssh.allowedSignersFile",
            allowed.to_str().unwrap(),
            None,
            Some(&elsewhere),
            &mut prepared,
        );
        assert!(other.is_err());

        // One the sandbox can read comes with the request, and verifying
        // never asks.
        let args = find(&allowed);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let (code, out, err) = run(&proxy, &args, "unread");
        assert_eq!(code, 0, "{err}");
        assert_eq!(out, "SIG()");
        assert!(
            err.contains("file=me@example.com ssh-ed25519 AAAA\n"),
            "{err}"
        );
        assert!(err.contains("-Overify-time=20260101120000\n"), "{err}");
    }
}
