//! A server inside a bubblewrap sandbox is reachable from the host only
//! after the port is exposed. Needs bwrap with user namespaces and python3;
//! skipped without them.
#![cfg(target_os = "linux")]

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use cmdterm_env::{Environment, PortHub, SandboxPolicy, SandboxTarget};

fn available(program: &str, args: &[&str]) -> bool {
    Environment::Local
        .command(program)
        .args(args)
        .stdout()
        .is_ok()
}

#[test]
fn a_server_in_a_sandbox_is_reachable_only_after_expose() {
    if !available("bwrap", &["--ro-bind", "/", "/", "--unshare-all", "true"])
        || !available("python3", &["--version"])
    {
        eprintln!("bwrap or python3 unavailable here; skipped");
        return;
    }
    cmdterm_env::set_proxy_bridge(PathBuf::from(env!(
        "CARGO_BIN_EXE_scriptschnell-term-proxy-bridge"
    )));
    let dir = tempfile::tempdir().unwrap();
    let hub = PortHub::start(&dir.path().join("hub.sock")).unwrap();
    // A port that is free on the host now, and that the sandbox takes.
    let port = TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut policy = SandboxPolicy::local(vec![dir.path().to_path_buf()], &[], true);
    policy.ports = Some(dir.path().join("hub.sock"));
    let env = Environment::Sandbox(Box::new(SandboxTarget::new(Environment::Local, policy)));
    let script = format!(
        "import socket\n\
         s = socket.socket()\n\
         s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)\n\
         s.bind(('127.0.0.1', {port}))\n\
         s.listen(1)\n\
         s.settimeout(30)\n\
         c, _ = s.accept()\n\
         c.sendall(b'hello')\n\
         c.close()\n"
    );
    let server = thread::spawn(move || env.command("python3").args(["-c", &script]).output());

    for _ in 0..200 {
        if hub.ports().iter().any(|p| p.port == port) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        hub.ports().iter().any(|p| p.port == port),
        "{:?}",
        hub.ports()
    );
    // The sandbox's loopback is its own, so nothing answers on the host.
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());

    let host = hub.expose(port, None).unwrap();
    assert_eq!(host, port);
    let mut stream = TcpStream::connect(("127.0.0.1", host)).unwrap();
    let mut got = String::new();
    stream.read_to_string(&mut got).unwrap();
    assert_eq!(got, "hello");
    let out = server.join().unwrap().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_bridge_that_cannot_reach_the_hub_still_runs_the_program() {
    let dir = std::env::temp_dir().join(format!("cmdterm-bridge-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let missing = dir.join("missing.sock");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_scriptschnell-term-proxy-bridge"))
        .arg("--ports")
        .arg(&missing)
        .args(["--", "sh", "-c", "exit 7"])
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.status.code(), Some(7));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("running without port forwarding"), "{err}");
}
