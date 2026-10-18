//! A bubblewrap sandbox with the host's network shares the host's abstract
//! Unix sockets, where X11 listens. Landlock keeps it from them, while TCP
//! on the shared loopback still works.

#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::path::PathBuf;

use cmdterm_env::{Environment, SandboxKind, SandboxPolicy, SandboxTarget};

const NAME_VAR: &str = "CMDTERM_TEST_ABSTRACT_NAME";
const PORT_VAR: &str = "CMDTERM_TEST_TCP_PORT";

/// The client the other test runs, inside and outside the sandbox: it
/// tries the abstract socket and the TCP port and prints what happened.
/// Run on its own it does nothing.
#[test]
fn client() {
    let (Ok(name), Ok(port)) = (std::env::var(NAME_VAR), std::env::var(PORT_VAR)) else {
        return;
    };
    let unix = SocketAddr::from_abstract_name(name.as_bytes())
        .and_then(|addr| UnixStream::connect_addr(&addr))
        .and_then(|mut stream| {
            let mut byte = [0];
            stream.read_exact(&mut byte).map(|()| byte[0])
        });
    let tcp =
        TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap())).and_then(|mut stream| {
            let mut byte = [0];
            stream.read_exact(&mut byte).map(|()| byte[0])
        });
    let show = |result: std::io::Result<u8>| match result {
        Ok(byte) => format!("got {}", byte as char),
        Err(error) => format!("failed: {error}"),
    };
    println!("abstract: {}", show(unix));
    println!("tcp: {}", show(tcp));
}

/// Runs [`client`] in `env` and returns its two result lines.
fn run_client(env: &Environment, name: &str, port: u16) -> (String, String) {
    let exe = std::env::current_exe().unwrap();
    let out = env
        .command(exe.as_os_str())
        .args(["--exact", "client", "--nocapture", "--test-threads=1"])
        .var(NAME_VAR, name)
        .var(PORT_VAR, port.to_string())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = |prefix: &str| {
        (stdout.lines())
            // libtest prints `test client ... ` in front of the first line.
            .find_map(|line| Some(line.split_once(prefix)?.1))
            .unwrap_or_else(|| {
                panic!(
                    "no {prefix} line in {stdout}\n{}",
                    String::from_utf8_lossy(&out.stderr)
                )
            })
            .to_owned()
    };
    (line("abstract: "), line("tcp: "))
}

#[test]
fn host_network_sandboxes_cannot_reach_the_hosts_abstract_sockets() {
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
    cmdterm_env::set_proxy_bridge(PathBuf::from(env!("CARGO_BIN_EXE_cmdterm-proxy-bridge")));

    let name = format!("cmdterm-test-abstract-{}", std::process::id());
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

    // Outside, the client reaches both.
    let (abstract_socket, tcp) = run_client(&Environment::Local, &name, port);
    assert_eq!((abstract_socket.as_str(), tcp.as_str()), ("got u", "got t"));

    // Inside, only the TCP port.
    let mut policy = SandboxPolicy::local(vec![], &[], true);
    policy.kind = SandboxKind::Bwrap;
    policy.host_network = true;
    let sandbox = Environment::Sandbox(Box::new(SandboxTarget::new(Environment::Local, policy)));
    let (abstract_socket, tcp) = run_client(&sandbox, &name, port);
    assert!(
        abstract_socket.starts_with("failed:"),
        "the sandbox reached the host's abstract socket: {abstract_socket}"
    );
    assert_eq!(tcp, "got t");

    // An SSH host, here a stand-in for ssh that runs commands on this
    // machine in a home of their own. Before it has a copy of the bridge
    // its sandboxes reach the abstract socket; after, they do not.
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let shim = dir.path().join("ssh");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\n\
             while [ $# -gt 0 ]; do case \"$1\" in -o|-p) shift 2 ;; -*) shift ;; *) break ;; esac; done\n\
             shift\n\
             exec env -u XDG_CACHE_HOME HOME='{}' sh -c \"$1\"\n",
            home.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    let ssh: Environment = serde_json::from_value(serde_json::json!({
        "kind": "ssh", "destination": "fakehost", "program": shim,
    }))
    .unwrap();
    let mut policy = SandboxPolicy::local(vec![], &[], true);
    policy.kind = SandboxKind::Bwrap;
    policy.host_network = true;
    let sandbox = Environment::Sandbox(Box::new(SandboxTarget::new(ssh.clone(), policy)));
    let (abstract_socket, _) = run_client(&sandbox, &name, port);
    assert_eq!(abstract_socket, "got u");
    let bridge = PathBuf::from(env!("CARGO_BIN_EXE_cmdterm-proxy-bridge"));
    cmdterm_env::install_host_bridge(&ssh, &bridge).unwrap();
    let (abstract_socket, tcp) = run_client(&sandbox, &name, port);
    assert!(
        abstract_socket.starts_with("failed:"),
        "the SSH host's sandbox reached its abstract socket: {abstract_socket}"
    );
    assert_eq!(tcp, "got t");
}
