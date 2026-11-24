//! Git in a bubblewrap sandbox clones and pushes over SSH through the
//! proxy, which runs the host's `ssh`, here a stand-in that runs git's
//! command on a repository on the host. Needs bwrap with user namespaces
//! and git; skipped without them.
#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use cmdterm_env::{
    Approvals, Environment, NetworkProxy, ProxyRules, SandboxPolicy, SandboxTarget, builtin_guards,
};

fn available(program: &str, args: &[&str]) -> bool {
    Environment::Local
        .command(program)
        .args(args)
        .stdout()
        .is_ok()
}

fn git(dir: &Path, args: &[&str]) -> String {
    Environment::Local
        .command("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.test"])
        .args(args)
        .cwd(dir)
        .var("GIT_CONFIG_NOSYSTEM", "1")
        .var("HOME", dir)
        .stdout()
        .map(|out| String::from_utf8(out).unwrap())
        .unwrap()
}

/// The host's `ssh` for the proxy: `-G` names a host behind a jump host,
/// so the proxy resolves nothing, and otherwise it runs the remote command
/// in `server`.
fn fake_ssh(dir: &Path, server: &Path) -> PathBuf {
    let program = dir.join("ssh");
    let script = format!(
        "#!/bin/sh\nfor a in \"$@\"; do\n  [ \"$a\" = -G ] && \
         exec printf 'hostname git.example.test\\nport 22\\nproxyjump jump\\n'\n  last=$a\ndone\n\
         cd '{}' && exec sh -c \"$last\"\n",
        server.display()
    );
    std::fs::write(&program, script).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    program
}

#[test]
fn git_in_a_sandbox_clones_and_pushes_over_ssh() {
    if !available("bwrap", &["--ro-bind", "/", "/", "--unshare-all", "true"])
        || !available("git", &["--version"])
    {
        eprintln!("bwrap or git unavailable here; skipped");
        return;
    }
    cmdterm_env::set_proxy_bridge(PathBuf::from(env!(
        "CARGO_BIN_EXE_scriptschnell-term-proxy-bridge"
    )));
    let dir = tempfile::tempdir().unwrap();
    let (server, seed, work) = (
        dir.path().join("server"),
        dir.path().join("seed"),
        dir.path().join("work"),
    );
    for d in [&server, &seed, &work] {
        std::fs::create_dir(d).unwrap();
    }
    git(
        &server,
        &["init", "-q", "--bare", "-b", "main", "remote.git"],
    );
    git(&seed, &["init", "-q", "-b", "main"]);
    std::fs::write(seed.join("a"), "a\n").unwrap();
    git(&seed, &["add", "a"]);
    git(&seed, &["commit", "-qm", "a"]);
    git(
        &seed,
        &[
            "push",
            "-q",
            server.join("remote.git").to_str().unwrap(),
            "main",
        ],
    );

    let proxy = NetworkProxy::start(
        &dir.path().join("proxy.sock"),
        vec!["git.example.test".into()],
    )
    .unwrap();
    proxy.set_ssh_program(fake_ssh(dir.path(), &server));
    proxy.set_rules(ProxyRules {
        secrets: Vec::new(),
        guards: builtin_guards(),
    });
    let approvals = Arc::new(Approvals::new(|| {}));
    proxy.set_approvals(Some(Arc::clone(&approvals)));
    let asked = Arc::new(AtomicUsize::new(0));
    {
        let (approvals, asked) = (Arc::clone(&approvals), Arc::clone(&asked));
        thread::spawn(move || {
            loop {
                for question in approvals.pending() {
                    assert_eq!(question.guard, "cmdterm.git-push");
                    asked.fetch_add(1, Ordering::Relaxed);
                    approvals.answer(question.id, true);
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
    }

    let mut policy = SandboxPolicy::local(vec![work.clone()], &[], true);
    policy.proxy = Some(proxy.endpoint().without_token());
    let env = Environment::Sandbox(Box::new(SandboxTarget::new(Environment::Local, policy)));
    let script = "git clone -q git@git.example.test:remote.git clone && cd clone && \
                  echo b > b && git add b && git commit -qm b && git push -q origin HEAD && \
                  git ls-remote origin main";
    let output = env
        .command("sh")
        .args(["-c", script])
        .cwd(&work)
        .var("HOME", &work)
        .var("GIT_CONFIG_NOSYSTEM", "1")
        .var("GIT_AUTHOR_NAME", "t")
        .var("GIT_AUTHOR_EMAIL", "t@example.test")
        .var("GIT_COMMITTER_NAME", "t")
        .var("GIT_COMMITTER_EMAIL", "t@example.test")
        .to_std()
        .output()
        .unwrap();
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    assert!(
        output.status.success(),
        "{}{}",
        text(&output.stdout),
        text(&output.stderr)
    );
    let head = git(&server.join("remote.git"), &["rev-parse", "main"]);
    assert!(
        text(&output.stdout).starts_with(head.trim()),
        "{}",
        text(&output.stdout)
    );
    assert_eq!(
        git(&server.join("remote.git"), &["log", "--format=%s", "main"]),
        "b\na\n"
    );
    assert_eq!(
        asked.load(Ordering::Relaxed),
        1,
        "one question for the push"
    );
}
