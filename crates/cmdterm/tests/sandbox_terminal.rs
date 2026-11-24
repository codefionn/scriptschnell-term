//! The built app opens terminals in a sandbox with the default settings.
//!
//! `cargo build -p cmdterm` builds no `scriptschnell-term-proxy-bridge`, so the app has
//! to stand in for it. Before it did, every sandboxed shell died with
//! `bwrap: execvp scriptschnell-term-proxy-bridge` and its pane closed at once: no
//! panel could be opened.

#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Command;

const APP: &str = env!("CARGO_BIN_EXE_scriptschnell-term");

fn bwrap_works() -> bool {
    Command::new("bwrap")
        .args(["--ro-bind", "/", "/", "--unshare-all", "true"])
        .status()
        .is_ok_and(|status| status.success())
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

#[test]
fn the_app_runs_as_the_proxy_bridge() {
    let status = Command::new(APP)
        .args(["--cmdterm-proxy-bridge", "--", "sh", "-c", "exit 7"])
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(7));
}

/// Sandboxes with the host's network run the app as the bridge with
/// `--scope`. Under the scope a program cannot signal its parent, this
/// test, which started outside it.
#[test]
fn the_app_as_the_bridge_scopes_what_it_runs() {
    if !cmdterm_env::landlock_scopes() {
        eprintln!("this kernel has no Landlock ABI 6; skipped");
        return;
    }
    let signals_parent = |flags: &[&str]| {
        Command::new(APP)
            .arg("--cmdterm-proxy-bridge")
            .args(flags)
            .args(["--", "sh", "-c", "kill -0 $PPID"])
            .status()
            .unwrap()
            .success()
    };
    assert!(signals_parent(&[]));
    assert!(!signals_parent(&["--scope"]));
    assert!(!signals_parent(&["--try-scope"]));
}

#[test]
fn a_new_worktree_opens_a_sandboxed_shell_that_stays_open() {
    if !bwrap_works() {
        eprintln!("bwrap cannot create namespaces here; skipped");
        return;
    }
    // A `cargo test` of cmdterm-env builds a bridge beside the app. Run a
    // link of the app from a folder of its own, where it has none.
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let app = dir.path().join("scriptschnell-term");
    if std::fs::hard_link(APP, &app).is_err() {
        std::fs::copy(APP, &app).unwrap();
    }
    let repo = dir.path().join("app");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("README"), "hi\n").unwrap();
    git(&repo, &["add", "README"]);
    git(&repo, &["commit", "-qm", "init"]);

    // The default sandbox: its own network namespace and the allowlist,
    // which both need the bridge.
    let cfg = dir.path().join("cfg");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("settings.json"),
        r#"{"language": "en", "sandbox": {"hosts_chosen": true}}"#,
    )
    .unwrap();
    let a11y = dir.path().join("a11y.json");
    let out = Command::new(&app)
        .arg("--config-dir")
        .arg(&cfg)
        .arg("--screenshot")
        .arg(dir.path().join("shot.png"))
        .arg("--settle-ms")
        .arg("3000")
        .arg("--dump-a11y")
        .arg(&a11y)
        .arg(&repo)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let tree = std::fs::read_to_string(&a11y).unwrap();
    assert!(
        !tree.contains("exited right after it started"),
        "the shell failed to start:\n{tree}"
    );
    assert!(!tree.contains("No panels"), "no panel is open:\n{tree}");
    assert!(tree.contains("\"Terminal\""), "no terminal:\n{tree}");
}
