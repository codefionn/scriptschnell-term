//! A bubblewrap sandbox reaches allowed hosts through the proxy and nothing
//! else. Needs bwrap with user namespaces and curl; skipped without them.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::thread;

use cmdterm_env::{Environment, NetworkProxy, SandboxPolicy, SandboxTarget};

fn available(program: &str, args: &[&str]) -> bool {
    Environment::Local
        .command(program)
        .args(args)
        .stdout()
        .is_ok()
}

/// An HTTP server on the host's loopback that answers every request with
/// `hello`.
fn server() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut buf = [0u8; 4096];
            let mut head = Vec::new();
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
            );
        }
    });
    port
}

#[test]
fn a_sandbox_with_a_proxy_reaches_only_allowed_hosts() {
    if !available("bwrap", &["--ro-bind", "/", "/", "--unshare-all", "true"])
        || !available("curl", &["--version"])
    {
        eprintln!("bwrap or curl unavailable here; skipped");
        return;
    }
    cmdterm_env::set_proxy_bridge(PathBuf::from(env!("CARGO_BIN_EXE_cmdterm-proxy-bridge")));
    let dir = tempfile::tempdir().unwrap();
    let port = server();
    let proxy =
        NetworkProxy::start(&dir.path().join("proxy.sock"), vec!["localhost".into()]).unwrap();
    let mut policy = SandboxPolicy::local(vec![dir.path().to_path_buf()], &[], false);
    policy.proxy = Some(proxy.endpoint());
    let env = Environment::Sandbox(Box::new(SandboxTarget::new(Environment::Local, policy)));
    // The proxy resolves names on the host, so `localhost` there is the
    // server above. NO_PROXY is cleared to send loopback through it too.
    let script = format!(
        "curl -sf --max-time 5 http://localhost:{port}/; echo \" $?\"; \
         curl -sf --max-time 5 http://127.0.0.1:{port}/; echo \" $?\"; \
         curl -sf --max-time 5 --noproxy '*' http://127.0.0.1:{port}/; echo \" $?\""
    );
    let out = env
        .command("sh")
        .args(["-c", &script])
        .var("NO_PROXY", "")
        .var("no_proxy", "")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        3,
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(lines[0], "hello 0");
    // Refused by the proxy (403), and no route around it.
    assert_ne!(lines[1], "hello 0");
    assert!(!lines[1].contains("hello"));
    assert!(!lines[2].contains("hello"));
}

/// HTTPS through the tunnel to a real host. Needs internet access.
#[test]
#[ignore]
fn https_reaches_an_allowed_host_and_not_another() {
    if !available("bwrap", &["--ro-bind", "/", "/", "--unshare-all", "true"]) {
        return;
    }
    cmdterm_env::set_proxy_bridge(PathBuf::from(env!("CARGO_BIN_EXE_cmdterm-proxy-bridge")));
    let dir = tempfile::tempdir().unwrap();
    let proxy =
        NetworkProxy::start(&dir.path().join("proxy.sock"), vec!["crates.io".into()]).unwrap();
    let mut policy = SandboxPolicy::local(vec![], &[], false);
    policy.proxy = Some(proxy.endpoint());
    let env = Environment::Sandbox(Box::new(SandboxTarget::new(Environment::Local, policy)));
    let script = "curl -s -o /dev/null -w '%{http_connect}\\n' https://crates.io/; \
                  curl -s -o /dev/null -w '%{http_connect}\\n' https://example.com/";
    let out = env.command("sh").args(["-c", script]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    // The tunnel opened, whatever crates.io answers inside it.
    assert_eq!(lines[0], "200", "{stdout}");
    assert_eq!(lines[1], "403", "{stdout}");
}

/// HTTPS through an intercepting proxy to real hosts. Needs internet.
#[test]
#[ignore]
fn intercepted_https_keeps_bad_certificates_untrusted() {
    if !available("bwrap", &["--ro-bind", "/", "/", "--unshare-all", "true"]) {
        return;
    }
    cmdterm_env::set_proxy_bridge(PathBuf::from(env!("CARGO_BIN_EXE_cmdterm-proxy-bridge")));
    let dir = tempfile::tempdir().unwrap();
    let allowed = vec!["crates.io".into(), "self-signed.badssl.com".into()];
    let proxy = NetworkProxy::start(&dir.path().join("proxy.sock"), allowed).unwrap();
    proxy
        .intercept_tls(Some(&dir.path().join("ca.pem")), true)
        .unwrap();
    let mut policy = SandboxPolicy::local(vec![], &[], false);
    policy.proxy = Some(proxy.endpoint());
    let env = Environment::Sandbox(Box::new(SandboxTarget::new(Environment::Local, policy)));
    let script = "curl -s -o /dev/null https://crates.io/; echo $?; \
                  curl -s -o /dev/null https://self-signed.badssl.com/; echo $?; \
                  curl -sk -o /dev/null -w '%{http_code}\\n' https://self-signed.badssl.com/";
    let out = env.command("sh").args(["-c", script]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines, ["0", "60", "200"], "{stdout}");
}

/// An HTTP server on the host's loopback that answers with the
/// `Authorization` header it got.
fn auth_echo() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut buf = [0u8; 4096];
            let mut head = Vec::new();
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
            let head = String::from_utf8_lossy(&head);
            let auth = head
                .lines()
                .find_map(|line| line.strip_prefix("Authorization: "))
                .unwrap_or("none")
                .to_owned();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{auth}",
                auth.len()
            );
        }
    });
    port
}

#[test]
fn a_sandbox_with_the_whole_network_sends_real_secrets_only_through_the_proxy() {
    if !available("bwrap", &["--ro-bind", "/", "/", "--unshare-all", "true"])
        || !available("curl", &["--version"])
    {
        eprintln!("bwrap or curl unavailable here; skipped");
        return;
    }
    // Sandboxes that share the host network start under the bridge too,
    // and the first test to look one up fixes it for the whole process.
    cmdterm_env::set_proxy_bridge(PathBuf::from(env!("CARGO_BIN_EXE_cmdterm-proxy-bridge")));
    let dir = tempfile::tempdir().unwrap();
    let port = auth_echo();
    // Any-host mode reaches this machine's loopback only for hosts listed by name.
    let allowed = vec!["localhost".into()];
    let proxy = NetworkProxy::start(&dir.path().join("proxy.sock"), allowed).unwrap();
    proxy.allow_any_host(true);
    let fake = cmdterm_env::fake_secret("real-token-for-the-test-0123");
    proxy.set_rules(cmdterm_env::ProxyRules {
        secrets: vec![cmdterm_env::SecretRule {
            fake: fake.clone(),
            real: "real-token-for-the-test-0123".into(),
            hosts: vec!["localhost".into()],
        }],
        guards: Vec::new(),
    });
    let mut policy = SandboxPolicy::local(vec![], &[], true);
    policy.host_network = true;
    policy.proxy = Some(proxy.endpoint());
    policy.vars.push(("TOKEN".into(), fake.clone()));
    let env = Environment::Sandbox(Box::new(SandboxTarget::new(Environment::Local, policy)));
    // Through the proxy the server gets the real token; around it, the fake.
    let script = format!(
        "echo \"[$TOKEN]\"; \
         curl -sf --max-time 5 -H \"Authorization: Bearer $TOKEN\" http://localhost:{port}/; echo; \
         curl -sf --max-time 5 --noproxy '*' -H \"Authorization: Bearer $TOKEN\" http://localhost:{port}/; echo"
    );
    let out = env
        .command("sh")
        .args(["-c", &script])
        .var("NO_PROXY", "")
        .var("no_proxy", "")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines,
        [
            format!("[{fake}]").as_str(),
            "Bearer real-token-for-the-test-0123",
            format!("Bearer {fake}").as_str(),
        ],
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A guard stops `git push` over HTTPS and lets fetches through. Needs
/// internet and git.
#[test]
#[ignore]
fn a_guard_blocks_git_push_to_a_real_forge() {
    if !available("bwrap", &["--ro-bind", "/", "/", "--unshare-all", "true"])
        || !available("git", &["--version"])
    {
        return;
    }
    cmdterm_env::set_proxy_bridge(PathBuf::from(env!("CARGO_BIN_EXE_cmdterm-proxy-bridge")));
    let dir = tempfile::tempdir().unwrap();
    let proxy =
        NetworkProxy::start(&dir.path().join("proxy.sock"), vec!["github.com".into()]).unwrap();
    proxy.set_rules(cmdterm_env::ProxyRules {
        secrets: Vec::new(),
        guards: vec![cmdterm_env::GuardRule {
            id: "git.push".into(),
            title: "git push".into(),
            hosts: vec!["*".into()],
            requests: vec![
                "POST */git-receive-pack".into(),
                "GET */info/refs?*service=git-receive-pack*".into(),
            ],
            mode: cmdterm_env::GuardMode::Block,
            kind: None,
        }],
    });
    proxy
        .intercept_tls(Some(&dir.path().join("ca.pem")), false)
        .unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let mut policy = SandboxPolicy::local(vec![work.clone()], &[], false);
    policy.proxy = Some(proxy.endpoint());
    let env = Environment::Sandbox(Box::new(SandboxTarget::new(Environment::Local, policy)));
    let script = "git ls-remote https://github.com/git/git HEAD >/dev/null; echo \"ls $?\"; \
                  git init -q r && cd r && git -c user.name=t -c user.email=t@t commit -q --allow-empty -m x && \
                  git push https://github.com/git/git HEAD:refs/heads/cmdterm-test 2>&1; echo \"push $?\"";
    let out = env
        .command("sh")
        .args(["-c", script])
        .cwd(&work)
        .var("HOME", work.to_str().unwrap())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("ls 0\n"), "{stdout}");
    assert!(stdout.contains("403"), "{stdout}");
    assert!(stdout.trim_end().ends_with("push 128"), "{stdout}");
}
