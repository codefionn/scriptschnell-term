//! Checks against the real agents: Claude Code, Codex and Qwen Code run in a
//! container next to `tests/agents/fake_api.py`, a fake but stateful model
//! API. The app drives them from its terminals the way it drives agents on
//! this machine, with the hooks its settings page installs.
//!
//! They need podman and, for the first image build, the network, so they are
//! ignored by default:
//!
//! ```sh
//! cargo test -p cmdterm --lib agent_harness -- --ignored --test-threads=1
//! ```
//!
//! `CMDTERM_HARNESS_REBUILD=1` builds the image again, to pick up new agent
//! releases. `crates/cmdterm/tests/agents/README.md` has the details.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use cmdterm_env::{ContainerTarget, Environment};
use serde_json::Value;

use super::{add_project_and_wait, repo, workspace};
use crate::agents::Agent;
use crate::state::{Runtime, SettingsStore, StateStore};
use crate::terminal_panel::TerminalPanel;
use crate::workspace::Workspace;

mod sandbox;

const IMAGE: &str = "localhost/scriptschnell-term-agents:dev";
/// Real agents start in seconds and answer after a few round trips.
const PATIENCE: Duration = Duration::from_secs(90);

/// One agent the harness drives.
struct Case {
    agent: Agent,
    program: &'static str,
    /// What goes before a launch's prompt, as the control client sends it.
    prompt_flag: Option<&'static str>,
    /// A script in `tests/agents` that runs in the container before the
    /// test, for what a user does once by hand after installing.
    prepare: Option<&'static str>,
    /// Screens the agent shows once, with the keys a user answers.
    dialogs: &'static [(&'static str, &'static str)],
}

const CLAUDE: Case = Case {
    agent: Agent::ClaudeCode,
    program: "claude",
    prompt_flag: None,
    prepare: None,
    dialogs: &[],
};
const CODEX: Case = Case {
    agent: Agent::Codex,
    program: "codex",
    prompt_flag: None,
    // Codex asks before it runs hooks it has not seen, and a session that
    // starts before the user answers never reports itself.
    prepare: Some("trust_codex_hooks.py"),
    dialogs: &[],
};
const QWEN: Case = Case {
    agent: Agent::Qwen,
    program: "qwen",
    prompt_flag: Some("-i"),
    prepare: None,
    dialogs: &[],
};

/// The container with the agents and the fake API, and a repository mounted
/// at the same path as on this machine.
struct Harness {
    root: tempfile::TempDir,
    repo: PathBuf,
    state: PathBuf,
    shims: PathBuf,
    container: String,
    dialogs: &'static [(&'static str, &'static str)],
}

impl Harness {
    fn start(case: &Case) -> Self {
        let root = tempfile::tempdir().unwrap();
        let repo = repo(root.path());
        let state = root.path().join("harness-state");
        let shims = root.path().join("shims");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&shims).unwrap();
        build_image();
        let container = format!("cmdterm-harness-{}", crate::control::random_hex(6));
        let mount =
            |host: &Path, inside: &Path| format!("{}:{}:z", host.display(), inside.display());
        let output = podman(&[
            "run",
            "-d",
            "--rm",
            // Reaps the processes agents leave behind, as init does.
            "--init",
            "--name",
            &container,
            "--userns=keep-id",
            "--network=none",
            "-v",
            &mount(&repo, &repo),
            "-v",
            &mount(&state, Path::new("/harness-state")),
            // The scripts from this checkout, so changing them needs no new
            // image.
            "-v",
            &format!("{}:/opt/harness:ro,z", scripts().display()),
            IMAGE,
        ]);
        assert!(
            output.status.success(),
            "podman run: {}",
            text(&output.stderr)
        );
        let harness = Self {
            root,
            repo,
            state,
            shims,
            container,
            dialogs: case.dialogs,
        };
        let setup = harness.exec(&[
            "sh",
            "/opt/harness/setup.sh",
            &harness.repo.to_string_lossy(),
        ]);
        assert!(setup.status.success(), "setup.sh: {}", text(&setup.stderr));
        crate::agents::install_for_test(&harness.env(), case.agent).unwrap();
        if let Some(script) = case.prepare {
            let path = format!("/opt/harness/{script}");
            let output = harness.exec(&["python3", &path, &harness.repo.to_string_lossy()]);
            assert!(
                output.status.success(),
                "{script}: {}{}",
                text(&output.stdout),
                text(&output.stderr)
            );
        }
        harness.shim(case.program);
        harness.wait_for_api();
        harness
    }

    fn env(&self) -> Environment {
        Environment::Container(Box::new(ContainerTarget::new(
            Environment::Local,
            "podman",
            &self.container,
        )))
    }

    fn exec(&self, argv: &[&str]) -> std::process::Output {
        let mut args = vec!["exec", self.container.as_str()];
        args.extend_from_slice(argv);
        podman(&args)
    }

    /// A program on this machine that runs `program` in the container, in
    /// the terminal and the directory it starts in, the way a user reaches
    /// an agent in a container from a shell here.
    fn shim(&self, program: &str) -> PathBuf {
        let path = self.shims.join(program);
        let script = format!(
            "#!/bin/sh\nHOME=\"${{CMDTERM_HARNESS_HOME:-$HOME}}\" exec podman exec -it -e SCRIPTSCHNELL_TERM=1 \
             -e TERM=\"${{TERM:-xterm-256color}}\" -w \"$PWD\" {} {program} \"$@\"\n",
            self.container
        );
        std::fs::write(&path, script).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    fn wait_for_api(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let probe = self.exec(&[
                "python3",
                "-c",
                "import urllib.request; urllib.request.urlopen('http://127.0.0.1:8787/v1/models')",
            ]);
            if probe.status.success() {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!("the fake API did not come up");
    }

    /// The requests the fake API answered, oldest first.
    fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.state.join("requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// Requests of the agent's conversation itself (no titles or topics)
    /// whose prompt contains `text`.
    fn prompts(&self, text: &str) -> Vec<Value> {
        (self.requests().into_iter())
            .filter(|r| {
                r["side"] == false && r["prompt"].as_str().is_some_and(|p| p.contains(text))
            })
            .collect()
    }

    /// What quitting the app does to the agents in its terminals: their
    /// terminals hang up. `podman exec` leaves its program running when its
    /// client goes, so the hang-up is sent inside, to every process on a
    /// terminal. Daemons an agent started, such as Codex's server, live on.
    fn hang_up_agents(&self) {
        self.exec(&[
            "sh",
            "-c",
            "ps -eo pid=,tty= | while read -r pid tty; do [ \"$tty\" = '?' ] || kill -HUP \"$pid\" 2>/dev/null; done; true",
        ]);
        std::thread::sleep(Duration::from_secs(2));
        // Whatever outlived its terminal goes as an app killed outright
        // takes it, so no stale process holds the session's lock.
        self.exec(&[
            "sh",
            "-c",
            "ps -eo pid=,tty= | while read -r pid tty; do [ \"$tty\" = '?' ] || kill -KILL \"$pid\" 2>/dev/null; done; true",
        ]);
    }

    /// A workspace on the harness's configuration, whose terminals run on
    /// this machine, like a project the user opened.
    fn workspace(&self) -> Workspace {
        let settings = SettingsStore::new(self.root.path().join("cfg/settings.json"));
        let mut saved = settings.load().unwrap();
        saved.default_runtime = Runtime::Host;
        saved.sandbox.hosts_chosen = true;
        settings.save(&saved).unwrap();
        workspace(self.root.path())
    }
}

impl Drop for Harness {
    /// `CMDTERM_HARNESS_KEEP=1` keeps the container for a look inside.
    fn drop(&mut self) {
        if std::env::var_os("CMDTERM_HARNESS_KEEP").is_some() {
            eprintln!("kept container {}", self.container);
            return;
        }
        let _ = podman(&["rm", "-f", "-t", "0", &self.container]);
    }
}

fn podman(args: &[&str]) -> std::process::Output {
    Environment::Local
        .command("podman")
        .args(args)
        .output()
        .expect("podman runs")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The harness's container files and scripts.
fn scripts() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/agents")
}

/// Builds the image once, or again with `CMDTERM_HARNESS_REBUILD=1`.
fn build_image() {
    static BUILT: std::sync::Once = std::sync::Once::new();
    BUILT.call_once(|| {
        let exists = podman(&["image", "exists", IMAGE]).status.success();
        if exists && std::env::var_os("CMDTERM_HARNESS_REBUILD").is_none() {
            return;
        }
        let dir = scripts();
        let dir = dir.to_string_lossy();
        let output = podman(&[
            "build",
            "-t",
            IMAGE,
            "-f",
            &format!("{dir}/Containerfile"),
            &dir,
        ]);
        assert!(
            output.status.success(),
            "podman build: {}",
            text(&output.stderr)
        );
    });
}

/// Every line the terminal holds, scrollback included.
fn screen(terminal: &TerminalPanel) -> String {
    let term = terminal.session.lock();
    let first = term.screen().first_stable();
    let end = term.stable_index(term.size().rows);
    (first..end)
        .filter_map(|stable| term.row_by_stable(stable))
        .map(|row| term.row_text(row))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Polls the workspace until `done`, or fails with the terminal and the
/// requests the fake API saw.
fn wait(
    ws: &Workspace,
    harness: &Harness,
    terminal: &TerminalPanel,
    what: &str,
    mut done: impl FnMut() -> bool,
) {
    let deadline = Instant::now() + PATIENCE;
    let mut answered = Instant::now() - Duration::from_secs(60);
    while !done() {
        // A dialog still there a few seconds after its answer gets another.
        if answered.elapsed() > Duration::from_secs(3) {
            let shown = screen(terminal);
            let mut start = shown.len().saturating_sub(4000);
            while !shown.is_char_boundary(start) {
                start += 1;
            }
            let tail = &shown[start..];
            let dialog = (harness.dialogs.iter()).find(|(text, _)| tail.contains(text));
            if let Some((_, keys)) = dialog {
                press(terminal, keys);
                answered = Instant::now();
            }
        }
        if Instant::now() > deadline {
            // `CMDTERM_HARNESS_PAUSE=1` keeps everything running for a look.
            if std::env::var_os("CMDTERM_HARNESS_PAUSE").is_some() {
                eprintln!("paused in {} for {what}", harness.container);
                std::thread::sleep(Duration::from_secs(900));
            }
            let requests: Vec<String> = harness.requests().iter().map(Value::to_string).collect();
            panic!(
                "timed out waiting for {what}\n--- terminal ---\n{}\n--- requests ---\n{}",
                screen(terminal),
                requests.join("\n")
            );
        }
        ws.poll();
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Sends `keys` one by one, `\r` as Enter.
fn press(terminal: &TerminalPanel, keys: &str) {
    let input = terminal.session.input();
    for key in keys.chars() {
        let code = match key {
            '\r' => crate::term::KeyCode::Enter,
            other => crate::term::KeyCode::Char(other),
        };
        input.key_down(code, crate::term::Mods::NONE);
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Types `line` and Enter into the agent, once its screen settled.
fn type_line(terminal: &TerminalPanel, line: &str) {
    let mut last = terminal.session.revision();
    let mut still = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(10);
    while still.elapsed() < Duration::from_secs(2) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        let revision = terminal.session.revision();
        if revision != last {
            (last, still) = (revision, Instant::now());
        }
    }
    let input = terminal.session.input();
    input.paste(line);
    std::thread::sleep(Duration::from_millis(300));
    input.key_down(crate::term::KeyCode::Enter, crate::term::Mods::NONE);
}

/// The agent saved with the worktree's terminals.
fn saved_agent(harness: &Harness) -> Option<crate::state::AgentSession> {
    let state = StateStore::new(harness.root.path().join("cfg/state.json"))
        .load()
        .ok()?;
    let config = state.projects.first()?.worktree(&harness.repo)?.clone();
    config.panels.into_iter().find_map(|panel| match panel {
        crate::state::PanelSpec::Terminal { agent, .. } => agent,
        _ => None,
    })
}

/// Opens the project and starts the agent the way a launch with `prompt`
/// does. Returns the workspace and the agent's terminal.
fn launch(harness: &Harness, case: &Case, prompt: &str) -> (Workspace, Rc<TerminalPanel>) {
    let ws = harness.workspace();
    add_project_and_wait(&ws, &harness.repo);
    crate::fit_grid(&ws, super::VIEWPORT);
    let shim = harness
        .shims
        .join(case.program)
        .to_string_lossy()
        .into_owned();
    let once = (case.prompt_flag.into_iter().chain([prompt]))
        .map(str::to_owned)
        .collect();
    ws.open_terminal_once(vec![shim], once, case.agent.name().to_owned());
    let terminal = ws.active_terminal().unwrap();
    (ws, terminal)
}

/// Gives the worktree's terminals a `HOME` of their own on this machine.
/// Before it resumes an agent, the app checks the agent's session store in
/// the terminal's environment, here this machine, where the agent does not
/// run. A developer's own `~/.claude/projects` would make every container
/// session look gone, while a missing store counts as unknown. The shim
/// gives podman the real `HOME` back.
fn isolate_host_home(harness: &Harness) {
    let store = StateStore::new(harness.root.path().join("cfg/state.json"));
    let mut state = store.load().unwrap();
    let home = harness.root.path().join("host-home");
    let real = std::env::var("HOME").unwrap_or_default();
    let config = state.projects[0].worktree_mut(&harness.repo);
    config
        .env_vars
        .retain(|(key, _)| key != "HOME" && key != "CMDTERM_HARNESS_HOME");
    config.env_vars.extend([
        ("HOME".to_owned(), home.to_string_lossy().into_owned()),
        ("CMDTERM_HARNESS_HOME".to_owned(), real),
    ]);
    store.save(&state).unwrap();
}

/// Opens the project again after a restart. Returns the agent's terminal.
fn reopen(harness: &Harness) -> (Workspace, Rc<TerminalPanel>) {
    isolate_host_home(harness);
    let ws = harness.workspace();
    crate::fit_grid(&ws, super::VIEWPORT);
    let terminal = ws
        .with_active(|live| {
            live?.panels.iter().find_map(|(_, panel)| match panel {
                crate::workspace::LivePanel::Terminal(t) if t.agent_session.borrow().is_some() => {
                    Some(t.clone())
                }
                _ => None,
            })
        })
        .expect("the agent's terminal came back");
    (ws, terminal)
}

/// An agent another agent launched with a prompt resumes its session after
/// a restart, without running the prompt again, and keeps its history.
fn launched_agent_resumes_without_its_prompt(case: &Case) {
    let harness = Harness::start(case);
    {
        let (ws, terminal) = launch(&harness, case, "TASK first");
        wait(&ws, &harness, &terminal, "the first reply", || {
            !harness.prompts("TASK first").is_empty()
                && terminal.session.agent_state() == crate::term::Attention::Done
        });
        wait(&ws, &harness, &terminal, "the saved session, done", || {
            saved_agent(&harness).is_some_and(|agent| agent.session.is_some() && !agent.working)
        });
    }
    harness.hang_up_agents();

    let saved = saved_agent(&harness).and_then(|agent| agent.session);
    let (ws, terminal) = reopen(&harness);
    // Codex reports a session only once a turn starts in it, so the earlier
    // reply on screen is what shows the session is back.
    wait(&ws, &harness, &terminal, "the resumed session", || {
        screen(&terminal).contains("HARNESS-REPLY turn=1")
    });
    std::thread::sleep(Duration::from_secs(1));
    type_line(&terminal, "TASK second");
    wait(&ws, &harness, &terminal, "the second prompt", || {
        !harness.prompts("TASK second").is_empty()
    });
    wait(
        &ws,
        &harness,
        &terminal,
        "the session reported again",
        || !terminal.session.agent_session().is_empty(),
    );
    let reported = terminal.session.agent_session();
    assert_eq!(
        reported.split_once(':').map(|(_, id)| id),
        saved.as_deref(),
        "the agent reported another session"
    );
    assert_eq!(
        harness.prompts("TASK first").len(),
        1,
        "the prompt ran again: {:#?}",
        harness.requests()
    );
    let second = &harness.prompts("TASK second")[0];
    assert_eq!(
        second["turns"], 2,
        "the resumed session lost its history: {second}"
    );
}

/// An agent stopped by the app in the middle of a turn is told to go on.
fn interrupted_agent_continues(case: &Case) {
    let harness = Harness::start(case);
    {
        let (ws, terminal) = launch(&harness, case, "SLOW TASK");
        wait(&ws, &harness, &terminal, "the slow turn", || {
            !harness.prompts("SLOW TASK").is_empty()
                && saved_agent(&harness)
                    .is_some_and(|agent| agent.session.is_some() && agent.working)
        });
    }
    harness.hang_up_agents();

    let (ws, terminal) = reopen(&harness);
    wait(&ws, &harness, &terminal, "the prompt to continue", || {
        !harness.prompts("Continue where you left off").is_empty()
    });
    assert_eq!(
        harness.prompts("SLOW TASK").len(),
        1,
        "the prompt ran again: {:#?}",
        harness.requests()
    );
}

/// An agent reached through `podman exec` from a shell here does not
/// freeze: stopping the client would leave the agent running.
fn agent_behind_a_client_does_not_freeze(case: &Case) {
    use crate::terminal_panel::Freeze;
    let harness = Harness::start(case);
    let (ws, terminal) = launch(&harness, case, "TASK first");
    wait(&ws, &harness, &terminal, "the first reply", || {
        !harness.prompts("TASK first").is_empty()
            && terminal.session.agent_state() == crate::term::Attention::Done
            && terminal.agent_session.borrow().is_some()
    });
    ws.open_terminal(None, None);
    ws.freeze_idle_agents(Instant::now(), Duration::ZERO);
    assert_eq!(terminal.freeze_state(), Freeze::Live);
}

#[test]
#[ignore = "needs podman and the agent image"]
fn claude_code_launched_with_a_prompt_resumes_without_it() {
    launched_agent_resumes_without_its_prompt(&CLAUDE);
}

#[test]
#[ignore = "needs podman and the agent image"]
fn codex_launched_with_a_prompt_resumes_without_it() {
    launched_agent_resumes_without_its_prompt(&CODEX);
}

#[test]
#[ignore = "needs podman and the agent image"]
fn qwen_code_launched_with_a_prompt_resumes_without_it() {
    launched_agent_resumes_without_its_prompt(&QWEN);
}

#[test]
#[ignore = "needs podman and the agent image"]
fn claude_code_interrupted_mid_turn_continues() {
    interrupted_agent_continues(&CLAUDE);
}

#[test]
#[ignore = "needs podman and the agent image"]
fn codex_interrupted_mid_turn_continues() {
    interrupted_agent_continues(&CODEX);
}

#[test]
#[ignore = "needs podman and the agent image"]
fn qwen_code_interrupted_mid_turn_continues() {
    interrupted_agent_continues(&QWEN);
}

#[test]
#[ignore = "needs podman and the agent image"]
fn claude_code_behind_podman_exec_does_not_freeze() {
    agent_behind_a_client_does_not_freeze(&CLAUDE);
}
