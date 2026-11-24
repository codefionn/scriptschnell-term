//! A client's bubblewrap sandbox with the host's network on a remote
//! machine shares that machine's abstract Unix sockets. The server names
//! its bridge, and the sandbox runs it to scope itself away from them with
//! Landlock, while TCP on the shared loopback still works.

#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::path::PathBuf;

use cmdterm_env::{Environment, RemoteTarget, SandboxKind, SandboxPolicy, SandboxTarget};
use cmdterm_remote::Paths;
use cmdterm_remote::keys::{self, Identity};
use cmdterm_remote::server::{Server, ServerConfig};

const BIN: &str = env!("CARGO_BIN_EXE_scriptschnell-term-remote");
const NAME_VAR: &str = "CMDTERM_TEST_ABSTRACT_NAME";
const PORT_VAR: &str = "CMDTERM_TEST_TCP_PORT";

/// The client the other test runs inside the sandbox: it tries the
/// abstract socket and the TCP port and prints what happened. Run on its
/// own it does nothing.
#[test]
fn client() {
    let (Ok(name), Ok(port)) = (std::env::var(NAME_VAR), std::env::var(PORT_VAR)) else {
        return;
    };
    let read_one = |mut stream: Box<dyn Read>| {
        let mut byte = [0];
        stream.read_exact(&mut byte).map(|()| byte[0])
    };
    let unix = SocketAddr::from_abstract_name(name.as_bytes())
        .and_then(|addr| UnixStream::connect_addr(&addr))
        .and_then(|stream| read_one(Box::new(stream)));
    let tcp = TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap()))
        .and_then(|stream| read_one(Box::new(stream)));
    let show = |result: std::io::Result<u8>| match result {
        Ok(byte) => format!("got {}", byte as char),
        Err(error) => format!("failed: {error}"),
    };
    println!("abstract: {}", show(unix));
    println!("tcp: {}", show(tcp));
}

#[test]
fn host_network_sandboxes_on_a_server_cannot_reach_its_abstract_sockets() {
    let shared = Environment::Local
        .command("bwrap")
        .args([
            "--ro-bind",
            "/",
            "/",
            "--unshare-all",
            "--share-net",
            "true",
        ])
        .stdout();
    if shared.is_err() {
        eprintln!("bwrap cannot create namespaces here; skipped");
        return;
    }
    if !cmdterm_env::landlock_scopes() {
        eprintln!("this kernel has no Landlock ABI 6; skipped");
        return;
    }
    // The server runs in this process, so it names the real binary as its
    // bridge, which takes the bridge flag first.
    cmdterm_env::set_proxy_bridge(PathBuf::from(BIN));

    let dir = tempfile::tempdir().unwrap();
    let server_paths = Paths::new(Some(dir.path().join("server")));
    let client = Paths::new(Some(dir.path().join("client")));
    let client_id = Identity::load_or_generate(&client.identity()).unwrap();
    keys::authorize(
        &server_paths.authorized_keys(),
        &client_id.public().to_line("test"),
    )
    .unwrap();
    let identity = Identity::load_or_generate(&server_paths.identity()).unwrap();
    let server = Server::with_config(server_paths, identity, ServerConfig::default());
    let address = server.spawn("127.0.0.1:0").unwrap().to_string();
    keys::trust_host(&client.known_hosts(), &address, server.public_key()).unwrap();
    cmdterm_env::set_remote_client(cmdterm_env::RemoteClient {
        program: BIN.into(),
        config_dir: Some(client.config_dir.clone()),
    });

    let name = format!("cmdterm-test-remote-abstract-{}", std::process::id());
    let addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
    let unix = UnixListener::bind_addr(&addr).unwrap();
    std::thread::spawn(move || {
        for mut stream in unix.incoming().flatten() {
            let _ = stream.write_all(b"u");
        }
    });
    let tcp = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = tcp.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut stream in tcp.incoming().flatten() {
            let _ = stream.write_all(b"t");
        }
    });

    let remote = Environment::Remote(RemoteTarget::new(&address));
    let mut policy = SandboxPolicy::local(vec![], &[], true);
    policy.kind = SandboxKind::Bwrap;
    policy.host_network = true;
    let sandbox = Environment::Sandbox(Box::new(SandboxTarget::new(remote, policy)));
    let exe = std::env::current_exe().unwrap();
    let out = sandbox
        .command(exe.as_os_str())
        .args(["--exact", "client", "--nocapture", "--test-threads=1"])
        .var(NAME_VAR, &name)
        .var(PORT_VAR, port.to_string())
        .output()
        .unwrap();
    server.shutdown();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = |prefix: &str| {
        // libtest prints `test client ... ` in front of the first line.
        (stdout.lines())
            .find_map(|line| Some(line.split_once(prefix)?.1))
            .unwrap_or_else(|| {
                panic!(
                    "no {prefix} line in {stdout}\n{}",
                    String::from_utf8_lossy(&out.stderr)
                )
            })
            .to_owned()
    };
    let abstract_socket = line("abstract: ");
    assert!(
        abstract_socket.starts_with("failed:"),
        "the sandbox reached the server's abstract socket: {abstract_socket}"
    );
    assert_eq!(line("tcp: "), "got t");
}
