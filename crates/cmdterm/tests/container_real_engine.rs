//! `up_builtin` against a real podman or docker with a tiny image. Skips with
//! a note when no engine works or the image cannot be pulled in time.
#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use cmdterm::container::{UpOptions, remove, up_builtin};
use cmdterm_env::Environment;

const IMAGE: &str = "docker.io/library/alpine:3.20";
const PULL_TIMEOUT: Duration = Duration::from_secs(120);

/// Runs an engine command with a deadline. True when it exits with 0.
fn run_within(engine: &str, args: &[&str], timeout: Duration) -> bool {
    let Ok(mut child) = Environment::Local
        .command(engine)
        .args(args)
        .to_std()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if start.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(200));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

fn options<'a>(worktree: &Path, engine: &str, log: &'a mut Vec<u8>) -> UpOptions<'a> {
    UpOptions {
        host: Environment::Local,
        worktree: worktree.to_path_buf(),
        engine: Some(engine.to_owned()),
        rebuild: false,
        mounts: Vec::new(),
        log,
    }
}

fn working_engine() -> Option<&'static str> {
    ["podman", "docker"]
        .into_iter()
        .find(|engine| run_within(engine, &["info"], Duration::from_secs(20)))
}

#[test]
fn starts_a_real_container_and_runs_commands_in_it() {
    let Some(engine) = working_engine() else {
        eprintln!("skipped: neither podman nor docker works here");
        return;
    };
    if !run_within(engine, &["pull", IMAGE], PULL_TIMEOUT) {
        eprintln!("skipped: could not pull {IMAGE} with {engine}");
        return;
    }
    // Under the target directory, not /tmp: a sandbox that reaches the
    // host's engine has a /tmp of its own, which the engine cannot mount.
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let worktree = dir.path().join("realrepo");
    fs::create_dir_all(worktree.join(".devcontainer")).unwrap();
    fs::write(worktree.join("hello.txt"), "from the host\n").unwrap();
    fs::write(
        worktree.join(".devcontainer/devcontainer.json"),
        format!(
            r#"{{
                "image": "{IMAGE}",
                "remoteEnv": {{"GREETING": "hi from ${{containerWorkspaceFolderBasename}}"}},
                "postCreateCommand": "cat hello.txt > /tmp/created",
            }}"#
        ),
    )
    .unwrap();

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut log = Vec::new();
        let up = up_builtin(options(&worktree, engine, &mut log)).unwrap_or_else(|e| {
            panic!("{e}\n{}", String::from_utf8_lossy(&log));
        });
        assert!(up.created);
        let env = Environment::Container(Box::new(up.target.clone()));
        let out = env
            .command("sh")
            .args(["-c", "cat /tmp/created; pwd; echo $GREETING"])
            .cwd(&worktree)
            .stdout()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out),
            "from the host\n/workspaces/realrepo\nhi from realrepo\n"
        );
        let again = up_builtin(options(&worktree, engine, &mut Vec::new())).unwrap();
        assert!(!again.created);
    }));
    remove(&Environment::Local, &worktree, Some(engine)).unwrap();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}
