//! The proxy's side of git over SSH (see `proxy/ssh.rs`): it checks the
//! request, runs this machine's `ssh` and relays its streams.
//!
//! The host a request names may be an alias in the user's SSH
//! configuration, so `ssh -G` first says where it leads. That host name
//! must be allowed and not blocked, as for a tunnel. SSH from a sandbox
//! never reaches this machine or a link-local address, even when the
//! allowed hosts list `localhost`: `ssh` there logs in as the user, with
//! the user's keys, and would read any repository on the machine. Without
//! a `ProxyCommand` or `ProxyJump` the proxy resolves the name itself and
//! has `ssh` connect to the address it checked (`HostName`, with
//! `HostKeyAlias` keeping the known hosts entry of the name), so a name
//! that resolves elsewhere on a second lookup cannot lead there.
//!
//! `ssh` runs with `BatchMode`, so it never asks for a passphrase or
//! whether to trust a new host key: keys come from the user's agent or
//! unencrypted key files, and the host must be in the user's known hosts.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::net::{IpAddr, ToSocketAddrs};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use super::{Duplex, Shared, is_local_address, refuse, respond, split_authority};
use crate::Environment;
use crate::proxy::host_allowed;
use crate::proxy::rules::{GuardMode, GuardRule};
use crate::proxy::ssh::{
    EXIT, GitCommand, METHOD, STDERR, STDIN, STDOUT, read_frame, valid_host, valid_protocol,
    valid_user, write_frame,
};

/// How often the relay looks whether `ssh` exited or the client left.
const POLL: Duration = Duration::from_millis(50);

/// How long the relay waits for `ssh`'s output to end after it exited.
/// A connection master `ssh` leaves behind (`ControlPersist`) may hold
/// it open.
const DRAIN: Duration = Duration::from_secs(5);

/// The exit code the client gets when `ssh` gives none.
const SSH_FAILED: i32 = 255;

/// What an `SSH` request asks for.
#[derive(Debug, PartialEq, Eq)]
struct Request {
    host: String,
    port: Option<u16>,
    user: Option<String>,
    command: GitCommand,
    protocol: Option<String>,
}

fn parse_request(head: &str) -> Result<Request, String> {
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let authority = first.split(' ').nth(1).ok_or("bad request line")?;
    let (host, port) = split_authority(authority, 0).ok_or("bad host")?;
    if !valid_host(&host) {
        return Err(format!("bad host {host:?}"));
    }
    let (mut user, mut command, mut protocol) = (None, None, None);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("ssh-user") {
            if !valid_user(value) {
                return Err(format!("bad user name {value:?}"));
            }
            user = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("ssh-command") {
            let parsed = GitCommand::parse(value).ok_or_else(|| {
                format!(
                    "only git-upload-pack, git-receive-pack and git-upload-archive pass over \
                     SSH, not {value:?}"
                )
            })?;
            command = Some(parsed);
        } else if name.eq_ignore_ascii_case("git-protocol") && valid_protocol(value) {
            protocol = Some(value.to_owned());
        }
    }
    Ok(Request {
        host,
        port: (port != 0).then_some(port),
        user,
        command: command.ok_or("no command")?,
        protocol,
    })
}

/// What `ssh -G` says about a host.
#[derive(Debug, PartialEq, Eq)]
struct Config {
    hostname: String,
    port: u16,
    /// A `ProxyCommand` or `ProxyJump` reaches the host, so this machine
    /// does not connect to it itself.
    proxied: bool,
    host_key_alias: Option<String>,
}

fn parse_config(text: &str) -> Option<Config> {
    let (mut hostname, mut port, mut proxied, mut alias) = (None, 22, false, None);
    for line in text.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        let value = value.trim();
        let set = !value.is_empty() && !value.eq_ignore_ascii_case("none");
        match key.to_ascii_lowercase().as_str() {
            "hostname" => hostname = Some(value.to_owned()),
            "port" => port = value.parse().ok()?,
            "proxycommand" | "proxyjump" if set => proxied = true,
            "hostkeyalias" if set => alias = Some(value.to_owned()),
            _ => {}
        }
    }
    Some(Config {
        hostname: hostname?,
        port,
        proxied,
        host_key_alias: alias,
    })
}

/// Options that keep `ssh` to the one command: no prompts, no TTY, no
/// forwarding and no local command, whatever the user's configuration
/// says. The first value `ssh` reads for an option wins, so these go
/// before the configuration files.
const FIXED_OPTIONS: &[&str] = &[
    "-T",
    "-x",
    "-a",
    "-o",
    "BatchMode=yes",
    "-o",
    "ClearAllForwardings=yes",
    "-o",
    "PermitLocalCommand=no",
    "-o",
    "RemoteCommand=none",
];

/// `ssh -G` for the request's host, port and user.
fn ssh_config(program: &OsString, request: &Request) -> Result<Config, String> {
    let output = Environment::Local
        .command(program)
        .args(target_args(request))
        .arg("-G")
        .args(host_args(request))
        .to_std()
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("{}: {error}", program.to_string_lossy()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ssh -G {}: {}", request.host, stderr.trim()));
    }
    parse_config(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| format!("ssh -G {}: no host name", request.host))
}

/// `-p` and `-l` for the request, which `ssh -G` takes too. The host
/// comes after them, from [`host_args`].
fn target_args(request: &Request) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(port) = request.port {
        args.extend(["-p".to_owned(), port.to_string()]);
    }
    if let Some(user) = &request.user {
        args.extend(["-l".to_owned(), user.clone()]);
    }
    args
}

fn host_args(request: &Request) -> [String; 2] {
    ["--".to_owned(), request.host.clone()]
}

/// True when `host` names this machine or a link-local address itself.
fn names_local(host: &str) -> bool {
    host == "localhost"
        || host.ends_with(".localhost")
        || host.parse::<IpAddr>().is_ok_and(is_local_address)
}

/// The address `ssh` connects to, checked: `None` when the host is
/// reached through a proxy of the user's configuration. `Err` holds the
/// reason and whether it is a refusal (`true`) or a failure to resolve.
fn pin(config: &Config, host: &str) -> Result<Option<IpAddr>, (String, bool)> {
    if names_local(host) {
        return Err((
            format!("{host} is this machine, which SSH from a sandbox never reaches"),
            true,
        ));
    }
    if config.proxied {
        return Ok(None);
    }
    let addrs: Vec<IpAddr> = (host, config.port)
        .to_socket_addrs()
        .map_err(|error| (format!("{host}: {error}"), false))?
        .map(|addr| addr.ip())
        .collect();
    if let Some(local) = addrs.iter().find(|ip| is_local_address(**ip)) {
        return Err((
            format!(
                "{host} resolves to {local}, on this machine or link-local, which SSH from a sandbox never reaches"
            ),
            true,
        ));
    }
    addrs
        .first()
        .copied()
        .map(Some)
        .ok_or_else(|| (format!("{host}: no address"), false))
}

/// The guards that hold back the HTTP request of the same service, see
/// `proxy/ssh.rs`. Guards that read bodies apply as to a request whose
/// body could not be read.
fn guards_for(shared: &Shared, host: &str, target: &str) -> Vec<GuardRule> {
    let Ok(rules) = shared.rules.read().map(|rules| Arc::clone(&rules)) else {
        return Vec::new();
    };
    let mut guards: Vec<GuardRule> = Vec::new();
    let found = (rules.guards_for(host, "POST", target))
        .chain(rules.body_guards_for(host, "POST", target, None));
    for guard in found {
        if !guards.iter().any(|g| g.id == guard.id) {
            guards.push(guard.clone());
        }
    }
    guards
}

/// Lets the command pass every guard, or says why not.
fn decide(shared: &Shared, scope: Option<&str>, host: &str, target: &str) -> Result<(), String> {
    for guard in guards_for(shared, host, target) {
        let allowed = match guard.mode {
            GuardMode::Allow => true,
            GuardMode::Block => false,
            GuardMode::Ask => {
                let approvals = shared.approvals.read().ok().and_then(|a| a.clone());
                approvals.is_some_and(|a| a.ask(&guard, scope, host, METHOD, target))
            }
        };
        if !allowed {
            return Err(format!(
                "cmdterm sandbox: {} is blocked ({METHOD} {host}{target}). Settings, \
                 Extensions decides what sandboxes may publish.\n",
                guard.title
            ));
        }
    }
    Ok(())
}

/// Serves one `SSH` request, whose head is `head`. `early` holds what
/// the client sent after it.
pub(super) fn serve<S: Duplex>(
    shared: &Arc<Shared>,
    mut client: S,
    head: &str,
    early: Vec<u8>,
    scope: Option<Arc<str>>,
) {
    let request = match parse_request(head) {
        Ok(request) => request,
        Err(error) => {
            let body = format!("cmdterm sandbox: {error}\n");
            return respond(&mut client, "400 Bad Request", &body);
        }
    };
    let program = (shared.ssh.read())
        .map(|program| program.clone())
        .unwrap_or_else(|_| "ssh".into());
    let config = match ssh_config(&program, &request) {
        Ok(config) => config,
        Err(error) => {
            let body = format!("cmdterm sandbox: {error}\n");
            return respond(&mut client, "502 Bad Gateway", &body);
        }
    };
    let host = config.hostname.trim_end_matches('.').to_ascii_lowercase();
    if let Ok(blocking) = shared.blocking.read()
        && let Some(by) = blocking.blocked_by(&host)
    {
        let reason = format!("{host} is blocked by {by}");
        let fix = "sh ~/.local/share/cmdterm/control sandbox shows what blocks it";
        return refuse(shared, &mut client, &host, reason, fix);
    }
    let allowed = shared.any_host.load(Ordering::Relaxed)
        || (shared.allowed.read()).is_ok_and(|list| host_allowed(&list, &host));
    if !allowed {
        let reason = format!("{host} is not an allowed host");
        let fix = format!("sh ~/.local/share/cmdterm/control allow-host {host} 'why it is needed'");
        return refuse(shared, &mut client, &host, reason, &fix);
    }
    let address = match pin(&config, &host) {
        Ok(address) => address,
        Err((reason, true)) => {
            let fix = "run git for this host outside the sandbox";
            return refuse(shared, &mut client, &host, reason, fix);
        }
        Err((reason, false)) => {
            let body = format!("cmdterm sandbox: {reason}\n");
            return respond(&mut client, "502 Bad Gateway", &body);
        }
    };
    let target = request.command.request_target();
    if let Err(body) = decide(shared, scope.as_deref(), &host, &target) {
        return respond(&mut client, "403 Forbidden", &body);
    }
    let mut command = Environment::Local
        .command(&program)
        .args(FIXED_OPTIONS.iter().copied());
    if let Some(address) = address {
        let alias = config.host_key_alias.clone().unwrap_or_else(|| {
            if config.port == 22 {
                host.clone()
            } else {
                format!("[{host}]:{}", config.port)
            }
        });
        command = command.args([
            "-o".to_owned(),
            format!("HostName={address}"),
            "-o".to_owned(),
            format!("HostKeyAlias={alias}"),
        ]);
    }
    if let Some(protocol) = &request.protocol {
        command = command
            .args(["-o", "SendEnv=GIT_PROTOCOL"])
            .var("GIT_PROTOCOL", protocol);
    }
    let command = command
        .args(target_args(&request))
        .args(host_args(&request))
        .arg(request.command.to_command());
    let child = command
        .to_std()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(error) => {
            let body = format!("cmdterm sandbox: {}: {error}\n", program.to_string_lossy());
            return respond(&mut client, "502 Bad Gateway", &body);
        }
    };
    if client.write_all(b"HTTP/1.1 200 OK\r\n\r\n").is_err() {
        return kill(child);
    }
    relay(child, client, early);
}

fn kill(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Relays frames between the client and `child` until it exits, then
/// sends its exit code. Kills it when the client leaves.
fn relay<S: Duplex>(mut child: Child, client: S, early: Vec<u8>) {
    let (Ok(reader), Ok(writer)) = (client.split(), client.split()) else {
        return kill(child);
    };
    let writer = Arc::new(Mutex::new(writer));
    let gone = Arc::new(AtomicBool::new(false));
    let mut stdin = child.stdin.take();
    let left = Arc::clone(&gone);
    thread::spawn(move || {
        let mut frames = io::Cursor::new(early).chain(reader);
        let mut data = Vec::new();
        loop {
            match read_frame(&mut frames, &mut data) {
                // The end of the input closes the pipe; the client stays
                // until the exit code comes.
                Ok(Some(STDIN)) if data.is_empty() => stdin = None,
                Ok(Some(STDIN)) => {
                    if let Some(pipe) = &mut stdin
                        && pipe.write_all(&data).is_err()
                    {
                        stdin = None;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    left.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
    });
    let (done, drained) = mpsc::channel();
    let mut pumps = 0;
    for (pipe, kind) in [
        (
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
            STDOUT,
        ),
        (
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
            STDERR,
        ),
    ] {
        let Some(mut pipe) = pipe else {
            continue;
        };
        let (writer, done) = (Arc::clone(&writer), done.clone());
        pumps += 1;
        thread::spawn(move || {
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                let n = match pipe.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                let Ok(mut out) = writer.lock() else {
                    break;
                };
                if write_frame(&mut *out, kind, &chunk[..n]).is_err() {
                    break;
                }
            }
            let _ = done.send(());
        });
    }
    drop(done);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        if gone.load(Ordering::Relaxed) {
            return kill(child);
        }
        thread::sleep(POLL);
    };
    let deadline = Instant::now() + DRAIN;
    for _ in 0..pumps {
        let wait = deadline.saturating_duration_since(Instant::now());
        if drained.recv_timeout(wait).is_err() {
            break;
        }
    }
    let code = status.and_then(|s| s.code()).unwrap_or(SSH_FAILED);
    if let Ok(mut out) = writer.lock() {
        let _ = write_frame(&mut *out, EXIT, &code.to_be_bytes());
        out.close_write();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::net::TcpStream;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::*;
    use crate::proxy::rules::{ProxyRules, builtin_guards};
    use crate::proxy::ssh::{parse_ssh_args, relay, request_head};
    use crate::proxy::{Approvals, NetworkProxy};

    #[test]
    fn requests_carry_a_host_a_user_and_a_git_command() {
        let request = parse_request(
            "SSH github.com:2222 HTTP/1.1\r\nProxy-Authorization: x\r\nSsh-User: git\r\n\
             Ssh-Command: git-upload-pack 'o/r.git'\r\nGit-Protocol: version=2\r\n\r\n",
        )
        .unwrap();
        assert_eq!(request.host, "github.com");
        assert_eq!(request.port, Some(2222));
        assert_eq!(request.user.as_deref(), Some("git"));
        assert_eq!(request.command.path, "o/r.git");
        assert_eq!(request.protocol.as_deref(), Some("version=2"));
        let bad = [
            "SSH h HTTP/1.1\r\n\r\n",
            "SSH h HTTP/1.1\r\nSsh-Command: id\r\n\r\n",
            "SSH -oProxyCommand=x HTTP/1.1\r\nSsh-Command: git-upload-pack 'a'\r\n\r\n",
            "SSH h HTTP/1.1\r\nSsh-User: -x\r\nSsh-Command: git-upload-pack 'a'\r\n\r\n",
        ];
        for head in bad {
            assert!(parse_request(head).is_err(), "{head}");
        }
    }

    #[test]
    fn ssh_g_says_where_a_host_leads() {
        let config = parse_config("user git\nhostname github.com\nport 22\nproxycommand none\n");
        assert_eq!(
            config,
            Some(Config {
                hostname: "github.com".into(),
                port: 22,
                proxied: false,
                host_key_alias: None,
            })
        );
        let config = parse_config("hostname h\nport 2222\nproxyjump jump\nhostkeyalias k\n");
        let config = config.unwrap();
        assert!(config.proxied);
        assert_eq!(
            (config.port, config.host_key_alias.as_deref()),
            (2222, Some("k"))
        );
        assert_eq!(parse_config("port 22\n"), None);
    }

    #[test]
    fn ssh_never_reaches_this_machine() {
        let config = |hostname: &str, proxied| Config {
            hostname: hostname.into(),
            port: 22,
            proxied,
            host_key_alias: None,
        };
        for host in [
            "localhost",
            "a.localhost",
            "127.0.0.1",
            "::1",
            "169.254.169.254",
        ] {
            assert!(
                matches!(pin(&config(host, true), host), Err((_, true))),
                "{host}"
            );
        }
        // A name that resolves to loopback is refused too.
        let resolved = pin(&config("localhost.", false), "localhost.");
        assert!(matches!(resolved, Err((_, true))), "{resolved:?}");
        assert_eq!(
            pin(&config("10.0.0.7", false), "10.0.0.7"),
            Ok(Some("10.0.0.7".parse().unwrap()))
        );
        assert_eq!(pin(&config("git.corp", true), "git.corp"), Ok(None));
    }

    /// A stand-in for `ssh`: `-G` prints `config` from its directory, and
    /// anything else prints its arguments and `GIT_PROTOCOL` to stderr,
    /// copies stdin to stdout and exits with 3.
    fn fake_ssh(dir: &Path) -> std::path::PathBuf {
        let program = dir.join("ssh");
        let script = format!(
            "#!/bin/sh\nfor a in \"$@\"; do [ \"$a\" = -G ] && exec cat '{}/config'; done\n\
             printf '%s\\n' \"$@\" >&2\nprintf 'proto=%s\\n' \"$GIT_PROTOCOL\" >&2\ncat\nexit 3\n",
            dir.display()
        );
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        program
    }

    /// Runs git's `ssh` command line through `proxy` with `input`, as the
    /// bridge does. Returns the exit code, stdout and stderr.
    fn run(proxy: &NetworkProxy, args: &[&str], input: &str) -> (i32, String, String) {
        let endpoint = proxy.endpoint();
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        let parsed = parse_ssh_args(&args).unwrap();
        let auth = endpoint.token.as_ref().unwrap().header();
        let head = request_head(&parsed, Some("version=2"), Some(&auth));
        let stream = TcpStream::connect(("127.0.0.1", endpoint.port)).unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let stdin = io::Cursor::new(input.as_bytes().to_vec());
        let code = relay(stream, &head, stdin, &mut out, &mut err);
        let text = |bytes: Vec<u8>| String::from_utf8(bytes).unwrap();
        (code, text(out), text(err))
    }

    #[test]
    fn git_runs_over_the_hosts_ssh_and_pushes_ask_first() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_ssh(dir.path());
        std::fs::write(
            dir.path().join("config"),
            "hostname git.example.test\nport 22\nproxyjump jump.example.test\n",
        )
        .unwrap();
        let proxy =
            NetworkProxy::start(&dir.path().join("p.sock"), vec!["git.example.test".into()])
                .unwrap();
        proxy.set_ssh_program(program);
        proxy.set_rules(ProxyRules {
            secrets: Vec::new(),
            guards: builtin_guards(),
        });

        let fetch = [
            "-o",
            "SendEnv=GIT_PROTOCOL",
            "git@work",
            "git-upload-pack 'org/repo.git'",
        ];
        let (code, out, err) = run(&proxy, &fetch, "want abc\n");
        assert_eq!(code, 3, "{err}");
        assert_eq!(out, "want abc\n");
        assert!(err.contains("BatchMode=yes\n"), "{err}");
        assert!(err.contains("SendEnv=GIT_PROTOCOL\n"), "{err}");
        assert!(
            err.contains("-l\ngit\n--\nwork\ngit-upload-pack 'org/repo.git'\n"),
            "{err}"
        );
        assert!(err.contains("proto=version=2\n"), "{err}");
        // Behind a ProxyJump this machine does not connect, so no address
        // is pinned.
        assert!(!err.contains("HostName="), "{err}");

        // Without anyone to ask, the push guard refuses.
        let push = ["git@work", "git-receive-pack 'org/repo.git'"];
        let (code, out, err) = run(&proxy, &push, "");
        assert_eq!(code, 255);
        assert!(out.is_empty());
        assert!(err.contains("git push is blocked"), "{err}");
        assert!(
            err.contains("SSH git.example.test/org/repo.git/git-receive-pack"),
            "{err}"
        );

        // The question is the one an HTTPS push to the same repository asks.
        let approvals = Arc::new(Approvals::new(|| {}));
        proxy.set_approvals(Some(Arc::clone(&approvals)));
        let answer = {
            let approvals = Arc::clone(&approvals);
            thread::spawn(move || {
                for _ in 0..200 {
                    if let Some(question) = approvals.pending().first() {
                        assert_eq!(question.guard, "cmdterm.git-push");
                        assert_eq!(question.host, "git.example.test");
                        assert_eq!(question.method, "SSH");
                        approvals.answer(question.id, true);
                        return true;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                false
            })
        };
        let (code, _, err) = run(&proxy, &push, "pack");
        assert!(answer.join().unwrap(), "no question came");
        assert_eq!(code, 3, "{err}");

        // The host the alias leads to must be allowed.
        proxy.set_allowed(vec!["work".into()]);
        let (code, _, err) = run(&proxy, &fetch, "");
        assert_eq!(code, 255);
        assert!(
            err.contains("git.example.test is not an allowed host"),
            "{err}"
        );

        // Nor does it reach this machine, whatever the allowed hosts say.
        proxy.set_allowed(vec!["localhost".into()]);
        std::fs::write(dir.path().join("config"), "hostname localhost\nport 22\n").unwrap();
        let (code, _, err) = run(&proxy, &fetch, "");
        assert_eq!(code, 255);
        assert!(err.contains("never reaches"), "{err}");

        // Other commands never get to ssh.
        let shell = ["git@work", "sh"];
        let (code, _, err) = run(&proxy, &shell, "");
        assert_eq!(code, 255);
        assert!(err.contains("only git-upload-pack"), "{err}");
    }

    #[test]
    fn a_direct_connection_goes_to_the_checked_address() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_ssh(dir.path());
        std::fs::write(dir.path().join("config"), "hostname 192.0.2.7\nport 2222\n").unwrap();
        let proxy =
            NetworkProxy::start(&dir.path().join("p.sock"), vec!["192.0.2.7".into()]).unwrap();
        proxy.set_ssh_program(program);
        let (code, _, err) = run(&proxy, &["-p", "2222", "box", "git-upload-pack 'r'"], "");
        assert_eq!(code, 3, "{err}");
        assert!(err.contains("HostName=192.0.2.7\n"), "{err}");
        assert!(err.contains("HostKeyAlias=[192.0.2.7]:2222\n"), "{err}");
        assert!(err.contains("-p\n2222\n"), "{err}");
    }
}
