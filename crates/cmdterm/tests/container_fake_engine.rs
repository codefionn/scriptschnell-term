//! `up_builtin` against a fake engine script that records its calls.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use cmdterm::container::{UpOptions, container_name, remove, up_builtin};
use cmdterm_env::Environment;

const FAKE_ENGINE: &str = r#"#!/bin/sh
dir=$(dirname "$0")
printf '%s\n' "$*" >> "$dir/calls.log"
case "$1" in
  inspect)
    if [ -f "$dir/state" ]; then cat "$dir/state"; else echo "Error: no such container" >&2; exit 1; fi ;;
  run) echo running > "$dir/state"; echo 0123abcd ;;
  start) echo running > "$dir/state" ;;
  rm) rm -f "$dir/state" ;;
  ps) if [ -f "$dir/state" ]; then echo 0123abcd; fi ;;
  build) echo "STEP 1/1: FROM alpine"; echo "build progress" >&2 ;;
  exec)
    case "$*" in
      *" env") echo "PATH=/usr/bin:/bin"; echo "HOME=/root" ;;
      *"echo create"*) echo "created inside" ;;
    esac ;;
esac
exit 0
"#;

const CONFIG: &str = r#"{
  // A Dockerfile build with lifecycle commands.
  "build": {"dockerfile": "Dockerfile", "context": "..", "args": {"V": "1"}},
  "workspaceFolder": "/workspaces/${localWorkspaceFolderBasename}",
  "containerEnv": {"A": "1"},
  "remoteEnv": {"PATH": "${containerEnv:PATH}:/opt/bin", "GONE": null},
  "remoteUser": "dev",
  "mounts": [{"source": "cache", "target": "/cache", "type": "volume"}],
  "onCreateCommand": "echo create",
  "postCreateCommand": {"b": ["echo", "b"], "a": "echo a"},
  "postStartCommand": "echo start",
  "postAttachCommand": "echo never",
}"#;

struct Fixture {
    _dir: tempfile::TempDir,
    bin: PathBuf,
    worktree: PathBuf,
    engine: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let worktree = dir.path().join("repo");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(worktree.join(".devcontainer")).unwrap();
        fs::write(worktree.join(".devcontainer/devcontainer.json"), CONFIG).unwrap();
        fs::write(worktree.join(".devcontainer/Dockerfile"), "FROM alpine\n").unwrap();
        let engine = bin.join("docker");
        fs::write(&engine, FAKE_ENGINE).unwrap();
        fs::set_permissions(&engine, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            engine: engine.display().to_string(),
            _dir: dir,
            bin,
            worktree,
        }
    }

    /// Calls since the last `take_calls`.
    fn take_calls(&self) -> Vec<String> {
        let path = self.bin.join("calls.log");
        let text = fs::read_to_string(&path).unwrap_or_default();
        let _ = fs::remove_file(path);
        text.lines().map(str::to_owned).collect()
    }

    fn up(&self, rebuild: bool, log: &mut Vec<u8>) -> cmdterm::container::Up {
        up_builtin(UpOptions {
            host: Environment::Local,
            worktree: self.worktree.clone(),
            engine: Some(self.engine.clone()),
            rebuild,
            mounts: Vec::new(),
            log,
        })
        .unwrap()
    }
}

fn hash(worktree: &Path) -> String {
    container_name(worktree)
        .rsplit_once('-')
        .unwrap()
        .1
        .to_owned()
}

#[test]
fn creates_reuses_restarts_rebuilds_and_removes() {
    let fx = Fixture::new();
    let w = fx.worktree.display().to_string();
    let name = container_name(&fx.worktree);
    let tag = format!("cmdterm-dc-{}", hash(&fx.worktree));
    let inspect = format!("inspect --format {{{{.State.Status}}}} {name}");
    let env_call = format!("exec {name} env");
    let exec = |cmd: &str| {
        format!("exec -u dev -w /workspaces/repo -e PATH=/usr/bin:/bin:/opt/bin {name} {cmd}")
    };
    let create_calls = [
        format!(
            "build -f {w}/.devcontainer/Dockerfile --build-arg V=1 -t {tag} {w}/.devcontainer/.."
        ),
        format!(
            "run -d --name {name} --label cmdterm.worktree={w} --label devcontainer.local_folder={w} \
             --mount type=bind,source={w},target=/workspaces/repo -w /workspaces/repo -e A=1 \
             --mount type=volume,source=cache,target=/cache --entrypoint /bin/sh {tag} \
             -c trap 'exit 0' TERM; while sleep 1000 & wait $!; do :; done"
        ),
        env_call.clone(),
        exec("sh -c echo create"),
        exec("sh -c echo a"),
        exec("echo b"),
        exec("sh -c echo start"),
    ];

    // First start: build, run, every lifecycle command but postAttach.
    let mut log = Vec::new();
    let up = fx.up(false, &mut log);
    let mut expected = vec![inspect.clone()];
    expected.extend(create_calls.iter().cloned());
    assert_eq!(fx.take_calls(), expected);
    assert!(up.created);
    assert_eq!(up.workspace_folder, Path::new("/workspaces/repo"));
    assert_eq!(up.target.host.as_ref(), &Environment::Local);
    assert_eq!(up.target.engine, fx.engine);
    assert_eq!(up.target.container, name);
    assert_eq!(up.target.user.as_deref(), Some("dev"));
    assert_eq!(
        up.target.mounts,
        [(fx.worktree.clone(), PathBuf::from("/workspaces/repo"))]
    );
    assert_eq!(
        up.target.vars,
        [("PATH".to_owned(), "/usr/bin:/bin:/opt/bin".to_owned())]
    );
    let log = String::from_utf8(log).unwrap();
    assert!(log.contains("STEP 1/1: FROM alpine\n"), "{log}");
    assert!(log.contains("build progress\n"), "{log}");
    assert!(log.contains("created inside\n"), "{log}");
    assert!(log.contains(&format!("> {} build", fx.engine)), "{log}");

    // Running: reuse, no lifecycle commands.
    let up = fx.up(false, &mut Vec::new());
    assert!(!up.created);
    assert_eq!(fx.take_calls(), [inspect.clone(), env_call.clone()]);

    // Exited: start, then postStartCommand only.
    fs::write(fx.bin.join("state"), "exited\n").unwrap();
    let up = fx.up(false, &mut Vec::new());
    assert!(!up.created);
    assert_eq!(
        fx.take_calls(),
        [
            inspect.clone(),
            format!("start {name}"),
            env_call.clone(),
            exec("sh -c echo start")
        ]
    );

    // Rebuild: remove, then the full create sequence.
    let up = fx.up(true, &mut Vec::new());
    assert!(up.created);
    let mut expected = vec![inspect.clone(), format!("rm -f {name}")];
    expected.extend(create_calls.iter().cloned());
    assert_eq!(fx.take_calls(), expected);

    // Remove finds the container by label; a second remove is a no-op.
    let ps = format!("ps -a -q --filter label=cmdterm.worktree={w}");
    remove(&Environment::Local, &fx.worktree, Some(&fx.engine)).unwrap();
    assert_eq!(fx.take_calls(), [ps.clone(), "rm -f 0123abcd".to_owned()]);
    remove(&Environment::Local, &fx.worktree, Some(&fx.engine)).unwrap();
    assert_eq!(fx.take_calls(), [ps]);
}

#[test]
fn failing_lifecycle_command_names_the_step() {
    let fx = Fixture::new();
    let failing = FAKE_ENGINE.replace(
        "      *\"echo create\"*) echo \"created inside\" ;;",
        "      *\"echo a\"*) echo \"no such file\" >&2; exit 7 ;;",
    );
    fs::write(&fx.engine, failing).unwrap();
    let err = up_builtin(UpOptions {
        host: Environment::Local,
        worktree: fx.worktree.clone(),
        engine: Some(fx.engine.clone()),
        rebuild: false,
        mounts: Vec::new(),
        log: &mut Vec::new(),
    })
    .unwrap_err();
    let text = err.to_string();
    assert!(
        text.starts_with("postCreateCommand \"a\" failed with exit code 7: no such file"),
        "{text}"
    );
}

#[test]
fn features_need_the_cli() {
    let fx = Fixture::new();
    fs::write(
        fx.worktree.join(".devcontainer/devcontainer.json"),
        r#"{"image": "alpine", "features": {"ghcr.io/devcontainers/features/node:1": {}}}"#,
    )
    .unwrap();
    let err = up_builtin(UpOptions {
        host: Environment::Local,
        worktree: fx.worktree.clone(),
        engine: Some(fx.engine.clone()),
        rebuild: false,
        mounts: Vec::new(),
        log: &mut Vec::new(),
    })
    .unwrap_err();
    assert!(err.to_string().contains("@devcontainers/cli"), "{err}");
    assert!(
        fx.take_calls().is_empty(),
        "no engine calls before the check"
    );
}
