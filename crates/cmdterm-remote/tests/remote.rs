//! End-to-end tests: a server in this process, the real `scriptschnell-term-remote`
//! binary as client, and a stand-in for Firecracker that runs the guest
//! agent on the VM's vsock socket path. Unix only, like the agent.

#![cfg(unix)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use cmdterm_remote::Paths;
use cmdterm_remote::keys::{self, Identity};
use cmdterm_remote::server::{Server, ServerConfig};
use cmdterm_remote::vm::FirecrackerConfig;

const BIN: &str = env!("CARGO_BIN_EXE_scriptschnell-term-remote");

struct Setup {
    server: std::sync::Arc<Server>,
    _dir: tempfile::TempDir,
    root: PathBuf,
    client: Paths,
    address: String,
}

/// A server with `config`, and a client that trusts it. The client key is
/// authorized with `options` as its `authorized_keys` prefix.
fn setup(config: ServerConfig, options: &str) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let server_paths = Paths::new(Some(root.join("server")));
    let client = Paths::new(Some(root.join("client")));
    let client_id = Identity::load_or_generate(&client.identity()).unwrap();
    keys::authorize(
        &server_paths.authorized_keys(),
        &format!("{options} {}", client_id.public().to_line("test")),
    )
    .unwrap();
    let identity = Identity::load_or_generate(&server_paths.identity()).unwrap();
    let server = Server::with_config(server_paths, identity, config);
    let address = server.spawn("127.0.0.1:0").unwrap().to_string();
    keys::trust_host(&client.known_hosts(), &address, server.public_key()).unwrap();
    Setup {
        server,
        _dir: dir,
        root,
        client,
        address,
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        self.server.shutdown();
    }
}

impl Setup {
    fn exec(&self, extra: &[&str], command: &[&str]) -> Output {
        Command::new(BIN)
            .arg("--config-dir")
            .arg(&self.client.config_dir)
            .args(["exec", "--target", &self.address])
            .args(extra)
            .arg("--")
            .args(command)
            .env("CMDTERM_REMOTE_PERSIST_SECS", "3")
            .output()
            .unwrap()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn direct_exec_carries_output_stderr_cwd_env_and_exit_code() {
    let s = setup(ServerConfig::default(), "");
    let out = s.exec(
        &["--direct", "--cwd", "/tmp", "--env", "GREETING=hi"],
        &[
            "sh",
            "-c",
            "printf \"$GREETING from $(pwd)\"; printf oops >&2; exit 7",
        ],
    );
    assert_eq!(text(&out.stdout), "hi from /tmp");
    assert_eq!(text(&out.stderr), "oops");
    assert_eq!(out.status.code(), Some(7));
}

/// `--forward` carries connections to a socket on the server back to one
/// on the client, while the command runs.
#[test]
fn forwards_reach_the_client_socket_while_the_command_runs() {
    use std::io::Write;
    use std::os::unix::net::{UnixListener, UnixStream};
    let s = setup(ServerConfig::default(), "");
    let local = s.root.join("local.sock");
    let inside = s.root.join("inside.sock");
    let listener = UnixListener::bind(&local).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut request = [0u8; 4];
            stream.read_exact(&mut request).unwrap();
            stream.write_all(b"pong:").unwrap();
            stream.write_all(&request).unwrap();
        }
    });
    let forward = format!("{}={}", inside.display(), local.display());
    let mut child = Command::new(BIN)
        .arg("--config-dir")
        .arg(&s.client.config_dir)
        .args(["exec", "--target", &s.address, "--forward", &forward])
        .args(["--", "sleep", "3"])
        .env("CMDTERM_REMOTE_PERSIST_SECS", "3")
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !inside.exists() {
        assert!(Instant::now() < deadline, "the forward never listened");
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut stream = UnixStream::connect(&inside).unwrap();
    stream.write_all(b"ping").unwrap();
    let mut reply = [0u8; 9];
    stream.read_exact(&mut reply).unwrap();
    assert_eq!(&reply, b"pong:ping");
    assert!(child.wait().unwrap().success());
    let deadline = Instant::now() + Duration::from_secs(10);
    while inside.exists() {
        assert!(
            Instant::now() < deadline,
            "the forward outlived its command"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn exec_shares_a_master_connection() {
    let s = setup(ServerConfig::default(), "");
    let socket = cmdterm_remote::client::master_socket(&s.client, &s.address);
    let first = s.exec(&[], &["echo", "one"]);
    assert_eq!(text(&first.stdout), "one\n", "{}", text(&first.stderr));
    assert!(socket.exists(), "master socket at {}", socket.display());
    let second = s.exec(&[], &["sh", "-c", "cat; exit 3"]);
    assert_eq!(second.status.code(), Some(3));
    // Many short commands at once, like a VCS poll.
    let runs: Vec<_> = (0..8)
        .map(|i| {
            Command::new(BIN)
                .arg("--config-dir")
                .arg(&s.client.config_dir)
                .args(["exec", "--target", &s.address, "--", "echo"])
                .arg(i.to_string())
                .output()
        })
        .collect();
    for (i, run) in runs.into_iter().enumerate() {
        assert_eq!(text(&run.unwrap().stdout), format!("{i}\n"));
    }
    // The master leaves once idle.
    let deadline = Instant::now() + Duration::from_secs(15);
    while socket.exists() {
        assert!(Instant::now() < deadline, "master did not exit when idle");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn untrusted_and_unauthorized_logins_fail_with_guidance() {
    let s = setup(ServerConfig::default(), "");
    std::fs::remove_file(s.client.known_hosts()).unwrap();
    let out = s.exec(&["--direct"], &["true"]);
    assert_eq!(out.status.code(), Some(255));
    assert!(
        text(&out.stderr).contains("scriptschnell-term-remote trust"),
        "{}",
        text(&out.stderr)
    );

    let other = Paths::new(Some(s.root.join("stranger")));
    let stranger = Identity::load_or_generate(&other.identity()).unwrap();
    drop(stranger);
    let fp = cmdterm_remote::client::trust(&other, &s.address, |_, _| true).unwrap();
    assert!(matches!(fp, cmdterm_remote::client::TrustOutcome::Added));
    let out = Command::new(BIN)
        .arg("--config-dir")
        .arg(&other.config_dir)
        .args(["exec", "--direct", "--target", &s.address, "--", "true"])
        .output()
        .unwrap();
    assert!(
        text(&out.stderr).contains("key not authorized"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn environment_remote_runs_commands_and_ptys() {
    let s = setup(ServerConfig::default(), "");
    cmdterm_env::set_remote_client(cmdterm_env::RemoteClient {
        program: BIN.into(),
        config_dir: Some(s.client.config_dir.clone()),
    });
    let env = cmdterm_env::Environment::Remote(cmdterm_env::RemoteTarget::new(&s.address));
    let out = env
        .command("sh")
        .args(["-c", "printf \"%s\" \"$1\"", "sh", "it's quoted"])
        .stdout()
        .unwrap();
    assert_eq!(text(&out), "it's quoted");

    let pty = cmdterm_env::pty::Pty::open(cmdterm_env::pty::PtySize {
        rows: 12,
        cols: 50,
        pixel_width: 0,
        pixel_height: 0,
    })
    .unwrap();
    let mut child = pty
        .spawn(
            &env.command("sh")
                .args(["-c", "stty size; tty >/dev/null && echo has-tty"])
                .to_pty(),
        )
        .unwrap();
    let mut reader = pty.reader().unwrap();
    let status = child.wait().unwrap();
    let mut output = Vec::new();
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(5);
    while !text(&output).contains("has-tty") && Instant::now() < deadline {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => output.extend_from_slice(&buf[..n]),
        }
    }
    assert!(status.success());
    assert!(text(&output).contains("12 50"), "{}", text(&output));
    assert!(text(&output).contains("has-tty"), "{}", text(&output));
}

/// A TCP forwarder in front of the server whose connections the test can
/// cut, like a network that goes away.
struct Cuttable {
    address: String,
    streams: std::sync::Arc<std::sync::Mutex<Vec<std::net::TcpStream>>>,
}

impl Cuttable {
    fn new(upstream: &str) -> Self {
        use std::net::{TcpListener, TcpStream};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let streams = std::sync::Arc::<std::sync::Mutex<Vec<TcpStream>>>::default();
        let (upstream, kept) = (upstream.to_owned(), streams.clone());
        std::thread::spawn(move || {
            for client in listener.incoming() {
                let client = client.unwrap();
                let server = TcpStream::connect(&upstream).unwrap();
                let mut list = kept.lock().unwrap();
                list.push(client.try_clone().unwrap());
                list.push(server.try_clone().unwrap());
                for (mut from, mut to) in [
                    (client.try_clone().unwrap(), server.try_clone().unwrap()),
                    (server, client),
                ] {
                    std::thread::spawn(move || {
                        let _ = std::io::copy(&mut from, &mut to);
                        let _ = to.shutdown(std::net::Shutdown::Both);
                    });
                }
            }
        });
        Self { address, streams }
    }

    fn cut(&self) {
        for stream in self.streams.lock().unwrap().drain(..) {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// Standard output of a child, collected by a thread.
struct Collected(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl Collected {
    fn new(mut source: impl Read + Send + 'static) -> Self {
        let bytes = std::sync::Arc::<std::sync::Mutex<Vec<u8>>>::default();
        let sink = bytes.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = source.read(&mut buf) {
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Self(bytes)
    }

    fn text(&self) -> String {
        text(&self.0.lock().unwrap())
    }

    fn wait_for(&self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self.text().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "no {needle:?} in {:?}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Kills the child when a test fails, since a session's client would
/// reconnect forever.
struct Reaped(std::process::Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A session survives a cut connection and its client: the client
/// reconnects without showing output twice and sends what was typed
/// meanwhile, a new client gets the buffered output, and `--end` stops it.
#[test]
fn sessions_survive_lost_connections_and_clients() {
    use std::io::Write;
    use std::process::Stdio;
    let s = setup(ServerConfig::default(), "");
    let net = Cuttable::new(&s.address);
    keys::trust_host(&s.client.known_hosts(), &net.address, s.server.public_key()).unwrap();
    let session = |extra: &[&str]| {
        let mut command = Command::new(BIN);
        command
            .arg("--config-dir")
            .arg(&s.client.config_dir)
            .args(["exec", "--direct", "--target", &net.address])
            .args(["--pty", "--session", "e2e"])
            .args(extra);
        command
    };
    let script = "stty -echo; read a; echo got-$a; read b; echo got-$b; sleep 30";
    let mut first = Reaped(
        session(&["--", "sh", "-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stdin = first.0.stdin.take().unwrap();
    let out = Collected::new(first.0.stdout.take().unwrap());
    let err = Collected::new(first.0.stderr.take().unwrap());
    stdin.write_all(b"one\n").unwrap();
    out.wait_for("got-one");

    net.cut();
    err.wait_for("reconnecting");
    stdin.write_all(b"two\n").unwrap();
    out.wait_for("got-two");
    assert_eq!(out.text().matches("got-one").count(), 1, "{}", out.text());
    drop(first);

    let mut second = Reaped(
        session(&["--", "sh", "-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let replay = Collected::new(second.0.stdout.take().unwrap());
    replay.wait_for("got-two");
    assert!(replay.text().contains("got-one"), "{}", replay.text());

    let end = session(&["--end"]).output().unwrap();
    assert_eq!(end.status.code(), Some(0), "{}", text(&end.stderr));
    let status = second.0.wait().unwrap();
    assert_eq!(status.code(), Some(128 + 9));
    let again = session(&["--end"]).output().unwrap();
    assert_eq!(again.status.code(), Some(1));
}

/// A key that may no longer use the host cannot attach to the host session
/// it started before, but can still end it.
#[test]
fn a_key_made_vm_only_cannot_attach_to_its_host_session() {
    use std::process::Stdio;
    let s = setup(ServerConfig::default(), "");
    let session = |extra: &[&str]| {
        let mut command = Command::new(BIN);
        command
            .arg("--config-dir")
            .arg(&s.client.config_dir)
            .args(["exec", "--direct", "--target", &s.address])
            .args(["--pty", "--session", "kept"])
            .args(extra);
        command
    };
    let mut first = Reaped(
        session(&["--", "sh", "-c", "echo started; sleep 30"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let out = Collected::new(first.0.stdout.take().unwrap());
    out.wait_for("started");
    drop(first);

    let authorized = Paths::new(Some(s.root.join("server"))).authorized_keys();
    let line = std::fs::read_to_string(&authorized).unwrap();
    std::fs::write(&authorized, format!("vm-only {}\n", line.trim())).unwrap();
    let again = session(&["--", "sh", "-c", "echo again"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(again.status.code(), Some(255));
    assert!(
        text(&again.stderr).contains("may only use VMs"),
        "{}",
        text(&again.stderr)
    );
    assert!(!text(&again.stdout).contains("started"));
    let end = session(&["--end"]).output().unwrap();
    assert_eq!(end.status.code(), Some(0), "{}", text(&end.stderr));
}

/// The connection of a session's first open dropped before `Attached`
/// reached the client, and the program exited meanwhile. The client's
/// next open, flagged `exit_if_ended`, gets the exit, and the program does
/// not run a second time. A new run under the same name is not ended by
/// that exit.
#[test]
fn an_open_whose_attach_was_lost_does_not_run_an_ended_program_again() {
    use cmdterm_remote::proto::{Msg, OpenSpec};
    use cmdterm_remote::session::{LIST_PROGRAM, SessionInfo};
    use cmdterm_remote::transport::{FrameRead, FrameWrite};
    let s = setup(ServerConfig::default(), "");
    let runs = s.root.join("runs");
    let spec = OpenSpec {
        pty: Some((80, 24)),
        program: "sh".into(),
        args: vec![
            "-c".into(),
            format!("echo run >> '{}'; exit 5", runs.display()),
        ],
        session: "lost-attach".into(),
        run: Some(1),
        ..OpenSpec::default()
    };
    // Sends `msg` on a new connection and returns what the server answers
    // until the channel ends.
    let open = |msg: Msg| {
        let mut conn = cmdterm_remote::client::connect(&s.client, &s.address).unwrap();
        let mut buf = Vec::new();
        msg.encode(&mut buf);
        conn.writer.write_frame(&buf).unwrap();
        let mut answers = Vec::new();
        while conn.reader.read_frame(&mut buf).unwrap() {
            let answer = Msg::decode(&buf).unwrap();
            let done = matches!(answer, Msg::Exit(..) | Msg::OpenFailed(..));
            answers.push(answer);
            if done {
                break;
            }
        }
        answers
    };

    // The exit is queued for this connection, which then drops: the
    // client of the real network saw neither it nor `Attached`.
    let first = open(Msg::Open(1, spec.clone()));
    assert!(matches!(first.last(), Some(Msg::Exit(1, 5))), "{first:?}");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let list = open(Msg::Open(
            2,
            OpenSpec {
                program: LIST_PROGRAM.into(),
                ..OpenSpec::default()
            },
        ));
        let Some(Msg::Data(_, json)) = list.first() else {
            panic!("no list: {list:?}");
        };
        let sessions: Vec<SessionInfo> = serde_json::from_slice(json).unwrap();
        if sessions.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the session stayed: {sessions:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let again = open(Msg::Open(
        3,
        OpenSpec {
            exit_if_ended: true,
            ..spec.clone()
        },
    ));
    assert_eq!(again, [Msg::Exit(3, 5)]);
    assert_eq!(std::fs::read_to_string(&runs).unwrap(), "run\n");

    // Another run reuses the name, and its first open never arrived. The
    // exit of the earlier run does not answer it: the program starts.
    let reused = open(Msg::Open(
        4,
        OpenSpec {
            exit_if_ended: true,
            run: Some(2),
            ..spec
        },
    ));
    assert!(
        matches!(reused.first(), Some(Msg::Attached(4, ..))),
        "{reused:?}"
    );
    assert!(matches!(reused.last(), Some(Msg::Exit(4, 5))), "{reused:?}");
    assert_eq!(std::fs::read_to_string(&runs).unwrap(), "run\nrun\n");
}

/// Logins with an authorized key do not count against the per-address
/// limit, so `exec --direct` can run more than eight commands a minute.
#[test]
fn direct_commands_are_not_rate_limited() {
    let s = setup(ServerConfig::default(), "");
    for i in 0..12 {
        let out = s.exec(&["--direct"], &["true"]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "command {i}: {}",
            text(&out.stderr)
        );
    }
}

/// Writes a stand-in for the firecracker binary: it serves the guest agent
/// on the vsock socket path the config file names.
fn fake_firecracker(dir: &Path) -> PathBuf {
    let script = dir.join("firecracker");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             # args: --api-sock PATH --config-file PATH\n\
             vmdir=$(dirname \"$4\")\n\
             echo booted >> \"{boots}\"\n\
             exec \"{BIN}\" guest --unix \"$vmdir/vsock.sock\"\n",
            boots = dir.join("boots").display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn vm_config(root: &Path) -> ServerConfig {
    std::fs::create_dir_all(root).unwrap();
    let firecracker = fake_firecracker(root);
    std::fs::write(root.join("base.ext4"), b"disk").unwrap();
    let config: FirecrackerConfig = serde_json::from_value(serde_json::json!({
        "binary": firecracker,
        "kernel": root.join("vmlinux"),
        "rootfs": root.join("base.ext4"),
        "state_dir": root.join("vms"),
        "boot_timeout_secs": 10,
    }))
    .unwrap();
    ServerConfig {
        offer_host: true,
        firecracker: Some(config),
        ..ServerConfig::default()
    }
}

/// The server answers `cmdterm-remote:projects` with the repositories in
/// its own `state.json`, and keys limited to VMs learn nothing.
#[test]
fn servers_share_the_projects_of_their_state() {
    let s = setup(ServerConfig::default(), "");
    let state = s.root.join("server").join("state.json");
    std::fs::write(
        &state,
        r#"{"projects": [
            {"id": 1, "name": "app", "env": {"kind": "local"}, "root": "/srv/app", "vcs": "git"},
            {"id": 2, "name": "far", "env": {"kind": "ssh", "host": "x"}, "root": "/a", "vcs": "git"}
        ]}"#,
    )
    .unwrap();
    let out = s.exec(&["--direct"], &[cmdterm_remote::projects::PROGRAM]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let shared: Vec<cmdterm_remote::projects::SharedProject> =
        serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(shared.len(), 1);
    assert_eq!(shared[0].root, Path::new("/srv/app"));

    let limited = setup(ServerConfig::default(), "vm-only");
    std::fs::copy(&state, limited.root.join("server").join("state.json")).unwrap();
    let out = limited.exec(&["--direct"], &[cmdterm_remote::projects::PROGRAM]);
    assert_ne!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
}

/// A key's own options in `authorized_keys` beat `server.json`: `host`
/// reaches a server that offers only VMs, `no-projects` hides the projects
/// of one that shares them.
#[test]
fn key_options_override_the_server_config() {
    let closed = ServerConfig {
        offer_host: false,
        share_projects: false,
        ..ServerConfig::default()
    };
    let refused = setup(closed.clone(), "");
    let out = refused.exec(&["--direct"], &["true"]);
    assert_eq!(out.status.code(), Some(255));
    assert!(
        text(&out.stderr).contains("only offers VMs"),
        "{}",
        text(&out.stderr)
    );

    let granted = setup(closed, "host,projects");
    let out = granted.exec(&["--direct"], &["sh", "-c", "echo on-host"]);
    assert_eq!(text(&out.stdout), "on-host\n", "{}", text(&out.stderr));
    let out = granted.exec(&["--direct"], &[cmdterm_remote::projects::PROGRAM]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));

    let hidden = setup(ServerConfig::default(), "no-projects");
    let out = hidden.exec(&["--direct"], &[cmdterm_remote::projects::PROGRAM]);
    assert_ne!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
    let out = hidden.exec(&["--direct"], &["true"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
}

#[test]
fn vm_channels_boot_once_and_keep_their_disk() {
    let fc_root = tempfile::tempdir().unwrap();
    let s = setup(vm_config(fc_root.path()), "vm-only");

    let host = s.exec(&["--direct"], &["true"]);
    assert_eq!(host.status.code(), Some(255));
    assert!(
        text(&host.stderr).contains("only use VMs"),
        "{}",
        text(&host.stderr)
    );

    let first = s.exec(&["--vm", "ci"], &["sh", "-c", "echo in-vm; exit 5"]);
    assert_eq!(text(&first.stdout), "in-vm\n", "{}", text(&first.stderr));
    assert_eq!(first.status.code(), Some(5));
    let second = s.exec(&["--vm", "ci", "--pty"], &["sh", "-c", "echo again"]);
    assert!(
        text(&second.stdout).contains("again"),
        "{}",
        text(&second.stderr)
    );

    let boots = std::fs::read_to_string(fc_root.path().join("boots")).unwrap();
    assert_eq!(boots.lines().count(), 1, "the VM was reused");
    let vms = fc_root.path().join("vms");
    let client_dir = std::fs::read_dir(&vms)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let vm_dir = client_dir.join("ci");
    assert_eq!(std::fs::read(vm_dir.join("rootfs.ext4")).unwrap(), b"disk");
    let config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(vm_dir.join("vm.json")).unwrap()).unwrap();
    assert_eq!(config["machine-config"]["vcpu_count"], 2);

    let bad = s.exec(&["--vm", "../x"], &["true"]);
    assert!(
        text(&bad.stderr).contains("invalid VM name"),
        "{}",
        text(&bad.stderr)
    );
}

#[test]
fn servers_without_vms_say_so() {
    let s = setup(ServerConfig::default(), "");
    let out = s.exec(&["--direct", "--vm", "ci"], &["true"]);
    assert_eq!(out.status.code(), Some(255));
    assert!(
        text(&out.stderr).contains("does not offer VMs"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn local_vms_run_through_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let fc_root = dir.path().join("fc");
    let config = vm_config(&fc_root);
    let paths = Paths::new(Some(dir.path().join("cfg")));
    std::fs::create_dir_all(&paths.dir).unwrap();
    let fc = config.firecracker.unwrap();
    std::fs::write(
        paths.server_config(),
        serde_json::json!({
            "firecracker": {
                "binary": fc.binary,
                "kernel": fc.kernel,
                "images": { "dev": fc.rootfs },
                "state_dir": fc.state_dir,
                "boot_timeout_secs": 10,
            }
        })
        .to_string(),
    )
    .unwrap();
    let run = |args: &[&str]| {
        Command::new(BIN)
            .arg("--config-dir")
            .arg(&paths.config_dir)
            .args(["exec", "--local-vm", "dev", "--"])
            .args(args)
            .output()
            .unwrap()
    };
    let first = run(&["sh", "-c", "echo local-vm; exit 4"]);
    assert_eq!(text(&first.stdout), "local-vm\n", "{}", text(&first.stderr));
    assert_eq!(first.status.code(), Some(4));
    assert!(run(&["true"]).status.success());
    assert!(fc_root.join("vms/local/dev/rootfs.ext4").exists());
    let boots = std::fs::read_to_string(fc_root.join("boots")).unwrap();
    assert_eq!(boots.lines().count(), 1);

    let fc = cmdterm_remote::server::ServerConfig::load(&paths)
        .unwrap()
        .firecracker
        .unwrap();
    let vms = cmdterm_remote::local::local_vms(&fc);
    assert_eq!(vms.len(), 1);
    assert_eq!(vms[0].name, "dev");

    // Stopping the daemon (SIGTERM) stops its VMs.
    assert!(cmdterm_remote::local::stop_vmd(&paths).unwrap());
    let socket = cmdterm_remote::local::vmd_socket(&paths);
    let deadline = Instant::now() + Duration::from_secs(15);
    while socket.exists() {
        assert!(Instant::now() < deadline, "daemon did not stop");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!cmdterm_remote::local::stop_vmd(&paths).unwrap());
    cmdterm_remote::local::delete_local_vm(&fc, "dev").unwrap();
    assert!(cmdterm_remote::local::local_vms(&fc).is_empty());
}

#[test]
fn local_vms_without_setup_explain_what_to_add() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(BIN)
        .arg("--config-dir")
        .arg(dir.path())
        .args(["exec", "--local-vm", "dev", "--", "true"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(255));
    assert!(
        text(&out.stderr).contains("firecracker section"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn a_stopped_server_refuses_new_connections() {
    let s = setup(ServerConfig::default(), "");
    let (key, state) = cmdterm_remote::client::fetch_host_key(&s.client, &s.address).unwrap();
    assert_eq!(&key, s.server.public_key());
    assert_eq!(state, keys::HostTrust::Trusted);
    s.server.stop();
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::net::TcpStream::connect(&s.address).is_ok() {
        assert!(Instant::now() < deadline, "still accepting");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn server_config_edits_keep_other_fields_and_reject_bad_values() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().to_path_buf()));
    std::fs::create_dir_all(&paths.dir).unwrap();
    std::fs::write(
        paths.server_config(),
        r#"{"firecracker": {"kernel": "/k", "rootfs": "/r", "state_dir": "/s",
            "network": [{"tap": "fc-tap0"}]}}"#,
    )
    .unwrap();
    let config = ServerConfig::edit(&paths, |map| {
        map.insert("offer_host".into(), false.into());
        map["firecracker"]["vcpus"] = 4.into();
    })
    .unwrap();
    assert!(!config.offer_host);
    let fc = config.firecracker.unwrap();
    assert_eq!(fc.vcpus, 4);
    assert_eq!(fc.network[0].tap, "fc-tap0");
    let error = ServerConfig::edit(&paths, |map| {
        map.insert("offer_hots".into(), true.into());
    })
    .unwrap_err();
    assert!(error.to_string().contains("offer_hots"), "{error}");
    assert!(!ServerConfig::load(&paths).unwrap().offer_host);
}
