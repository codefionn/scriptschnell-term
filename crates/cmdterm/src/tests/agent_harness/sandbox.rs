//! Coding agents in the sandbox of this machine (bubblewrap on Linux), next
//! to `tests/agents/fake_forge.py`, a git forge and npm registry that only
//! take the real credentials. The shipped `git-credentials` and `npm`
//! extensions give the sandbox fakes of them, and the model
//! (`tests/agents/fake_api.py`) has the agent run `git push` and
//! `npm publish` through its shell tool. The checks:
//!
//! - The sandbox holds only fakes, and the forge gets the real credentials
//!   through the proxy.
//! - A guard set to allow lets pushes through without a question; one set
//!   to block refuses them without one.
//! - A guard set to ask holds the push or upload until the user answers,
//!   and it reaches the forge only when the answer allows it.
//! - The real credentials never reach the model.
//!
//! A plain shell stands in for the agent in the tests that are not ignored,
//! so they run wherever bubblewrap, python3, git and npm are. The real
//! agents are installed with npm into `target/agent-harness/npm` on first
//! use, which needs the network; `CMDTERM_HARNESS_REBUILD=1` installs them
//! again.
//!
//! ```sh
//! cargo test -p cmdterm --lib agent_harness::sandbox -- --ignored --test-threads=1
//! ```
//!
//! Seatbelt (macOS) cannot bind the faked `~/.git-credentials` over the real
//! one and hides it instead, so there the tests skip.

use std::path::{Path, PathBuf};
use std::process::Child;
use std::rc::Rc;
use std::time::{Duration, Instant};

use cmdterm_env::{Environment, GuardMode, SandboxKind};
use serde_json::Value;

use super::{PATIENCE, screen, scripts, text, type_line};
use crate::state::{Runtime, SettingsStore};
use crate::terminal_panel::TerminalPanel;
use crate::tests::{add_project_and_wait, git, granted_extension, repo, workspace};
use crate::workspace::Workspace;

/// What the forge takes, and what the sandbox must never see.
const USER: &str = "harness";
const GIT_TOKEN: &str = "ghp_HarnessRealGitToken0123456789abcdef";
const NPM_TOKEN: &str = "npm_HarnessRealNpmToken0123456789abcdef";
const GIT_PUSH: &str = "cmdterm.git-push";
const NPM_PUBLISH: &str = "npm.publish";
/// A loopback address other than 127.0.0.1, so the proxy variables'
/// `NO_PROXY` does not send the sandbox around the proxy.
const FORGE_ADDRESS: &str = "127.0.0.2";

fn available(program: &str, args: &[&str]) -> bool {
    Environment::Local
        .command(program)
        .args(args)
        .output()
        .is_ok_and(|out| out.status.success())
}

/// Why the tests cannot run here, if they cannot.
fn missing() -> Option<String> {
    if SandboxKind::native() != SandboxKind::Bwrap {
        return Some(format!(
            "{:?} sandboxes hide ~/.git-credentials instead of faking it",
            SandboxKind::native()
        ));
    }
    if !available("bwrap", &["--ro-bind", "/", "/", "--unshare-all", "true"]) {
        return Some("bwrap cannot create namespaces here".into());
    }
    for program in ["python3", "git", "npm"] {
        if !available(program, &["--version"]) {
            return Some(format!("{program} is missing"));
        }
    }
    None
}

fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

/// A loopback port nothing listens on right now.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.local_addr().unwrap().port()
}

/// A child process killed when it goes out of scope.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Reads a JSON lines file, oldest line first.
fn json_lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// The forge, the home directory the sandbox sees and the worktree, under a
/// scratch directory outside `/tmp`, which the sandbox replaces.
struct Setup {
    _root: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    home: PathBuf,
    bin: PathBuf,
    forge_state: PathBuf,
    _forge: Server,
}

impl Setup {
    fn new() -> Self {
        let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let dir = tempfile::tempdir_in(&scratch).unwrap();
        let root = cmdterm_env::canonicalize(dir.path()).unwrap();
        let repo = repo(&root);
        let home = root.join("home");
        let bin = root.join("bin");
        let forge_root = root.join("forge");
        let forge_state = root.join("forge-state");
        for dir in [&home, &bin, &forge_root, &forge_state] {
            std::fs::create_dir_all(dir).unwrap();
        }
        git(&forge_root, &["init", "-q", "--bare", "app.git"]);
        let forge = Environment::Local
            .command("python3")
            .arg(scripts().join("fake_forge.py"))
            .arg(FORGE_ADDRESS)
            .var("FORGE_ROOT", &forge_root)
            .var("FORGE_STATE", &forge_state)
            .var("FORGE_USER", USER)
            .var("FORGE_GIT_TOKEN", GIT_TOKEN)
            .var("FORGE_NPM_TOKEN", NPM_TOKEN)
            .to_std()
            .spawn()
            .expect("python3 runs");
        let forge = Server(forge);
        let deadline = Instant::now() + Duration::from_secs(10);
        let port = loop {
            if let Ok(port) = std::fs::read_to_string(forge_state.join("port")) {
                break port;
            }
            assert!(Instant::now() < deadline, "the forge did not come up");
            std::thread::sleep(Duration::from_millis(50));
        };
        let forge_url = format!("http://{FORGE_ADDRESS}:{port}");
        git(
            &repo,
            &["remote", "add", "origin", &format!("{forge_url}/app.git")],
        );
        std::fs::write(repo.join(".git/info/exclude"), ".harness/\n").unwrap();

        // The logins as the user has them; the extensions turn them into
        // fakes for the sandbox.
        std::fs::write(
            home.join(".git-credentials"),
            format!("http://{USER}:{GIT_TOKEN}@{FORGE_ADDRESS}:{port}\n"),
        )
        .unwrap();
        std::fs::write(
            home.join(".npmrc"),
            format!("//{FORGE_ADDRESS}:{port}/:_authToken={NPM_TOKEN}\n"),
        )
        .unwrap();
        std::fs::write(
            home.join(".gitconfig"),
            "[credential]\n\thelper = store\n[user]\n\tname = t\n\temail = t@t\n\
             [commit]\n\tgpgsign = false\n",
        )
        .unwrap();
        std::fs::create_dir_all(repo.join("pkg")).unwrap();

        // What the agent runs, as scripts, so the prompts stay short.
        let repo_dir = repo.display();
        let scripts = [
            (
                "show",
                "cat ~/.git-credentials ~/.npmrc; env | grep -i token; echo EXIT=0".to_owned(),
            ),
            (
                "fetch",
                format!("cd {repo_dir} && git ls-remote origin 2>&1; echo EXIT=$?"),
            ),
            (
                "push",
                format!(
                    "cd {repo_dir} && git push origin \"HEAD:refs/heads/$1\" 2>&1; echo EXIT=$?"
                ),
            ),
            (
                "publish",
                format!(
                    "cd {repo_dir}/pkg && printf '{{\"name\": \"harness-pkg\", \"version\": \"%s\"}}' \"$1\" > package.json && \
                     npm publish --registry {forge_url}/ 2>&1; echo EXIT=$?"
                ),
            ),
        ];
        for (name, body) in scripts {
            let path = bin.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            make_executable(&path);
        }

        let setup = Self {
            _root: dir,
            root,
            repo,
            home,
            bin,
            forge_state,
            _forge: forge,
        };
        setup.extensions();
        setup
    }

    /// The shipped `git-credentials` and `npm` extensions, reading the
    /// scratch home instead of this machine's, and one that lets the
    /// sandbox write that home, as agents write their state there.
    fn extensions(&self) {
        let home = self.home.to_str().unwrap();
        let stub = format!(
            "local os_getenv = function(name) if name == 'HOME' then return {home:?} end return nil end\n"
        );
        for (name, source, permissions) in [
            (
                "git-credentials",
                include_str!("../../../../../extensions/git-credentials/init.lua"),
                &["env", "filesystem", "sandbox", "secrets"][..],
            ),
            (
                "npm",
                include_str!("../../../../../extensions/npm/init.lua"),
                &["env", "filesystem", "sandbox", "secrets", "guards"][..],
            ),
        ] {
            let source = stub.clone() + &source.replace("os.getenv(", "os_getenv(");
            granted_extension(&self.root, name, permissions, &source);
        }
        granted_extension(
            &self.root,
            "harness-home",
            &["sandbox"],
            &format!("term.sandbox{{ writable = {{ {home:?} }} }}"),
        );
    }

    /// A workspace whose new worktrees run in the sandbox.
    fn workspace(&self) -> Workspace {
        let settings = SettingsStore::new(self.root.join("cfg/settings.json"));
        let mut saved = settings.load().unwrap();
        saved.default_runtime = Runtime::Sandbox;
        saved.sandbox.hosts_chosen = true;
        // Allowing every host does not cover this machine's loopback; the
        // forge's address has to be listed.
        saved.sandbox.allowed_hosts.push(FORGE_ADDRESS.to_owned());
        settings.save(&saved).unwrap();
        let ws = workspace(&self.root);
        add_project_and_wait(&ws, &self.repo);
        crate::fit_grid(&ws, super::super::VIEWPORT);
        assert_eq!(ws.borrow().active_runtime(), Runtime::Sandbox);
        ws
    }

    /// A program here that sets `vars` and the scratch home and runs
    /// `program`, the way a user starts an agent from a shell.
    fn shim(&self, name: &str, program: &str, vars: &[(&str, String)]) -> String {
        let mut script = format!(
            "#!/bin/sh\n\
             # Nothing of an agent session the tests run in reaches the agent.\n\
             for var in $(env | sed -n 's/^\\(CLAUDE[A-Z_]*\\|CODEX[A-Z_]*\\|QWEN[A-Z_]*\\|ANTHROPIC[A-Z_]*\\|OPENAI[A-Z_]*\\)=.*/\\1/p'); do unset \"$var\"; done\n\
             unset XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME\n\
             export HOME={:?}\n",
            self.home
        );
        for (key, value) in vars {
            script.push_str(&format!("export {key}={value:?}\n"));
        }
        script.push_str(&format!("exec {program} \"$@\"\n"));
        let path = self.bin.join(format!("start-{name}"));
        std::fs::write(&path, script).unwrap();
        make_executable(&path);
        path.to_string_lossy().into_owned()
    }

    /// What the forge was asked, oldest first.
    fn forge(&self) -> Vec<Value> {
        json_lines(&self.forge_state.join("forge.jsonl"))
    }

    /// The forge's requests whose path contains `part` and that it served.
    fn served(&self, part: &str) -> Vec<Value> {
        (self.forge().into_iter())
            .filter(|r| r["path"].as_str().is_some_and(|p| p.contains(part)) && r["status"] == 200)
            .collect()
    }

    fn command(&self, script: &str, arg: &str) -> String {
        format!("{} {arg}", self.bin.join(script).display())
            .trim_end()
            .to_owned()
    }
}

/// What runs the commands in the sandbox: a shell, or an agent's shell tool.
trait Driver {
    fn terminal(&self) -> &Rc<TerminalPanel>;
    /// Starts `command`; `step` names it uniquely.
    fn start(&mut self, step: usize, command: &str);
    /// What `step` printed, once it finished.
    fn output(&self, step: usize) -> Option<String>;
    /// More to show when a step times out.
    fn report(&self) -> String {
        String::new()
    }
}

/// A plain sandboxed shell, standing in for an agent.
struct Shell {
    terminal: Rc<TerminalPanel>,
    out: PathBuf,
}

impl Shell {
    fn open(setup: &Setup, ws: &Workspace) -> Self {
        let shim = setup.shim("sh", "sh", &[]);
        ws.open_terminal(Some(vec![shim]), Some("shell".into()));
        let out = setup.repo.join(".harness");
        std::fs::create_dir_all(&out).unwrap();
        Self {
            terminal: ws.active_terminal().unwrap(),
            out,
        }
    }
}

impl Driver for Shell {
    fn terminal(&self) -> &Rc<TerminalPanel> {
        &self.terminal
    }

    fn start(&mut self, step: usize, command: &str) {
        let out = self.out.display();
        type_line(
            &self.terminal,
            &format!("{command} > {out}/{step}.tmp 2>&1; mv {out}/{step}.tmp {out}/{step}.txt"),
        );
    }

    fn output(&self, step: usize) -> Option<String> {
        std::fs::read_to_string(self.out.join(format!("{step}.txt"))).ok()
    }
}

/// A real agent, whose model has it run the commands.
struct Agent {
    terminal: Rc<TerminalPanel>,
    api_state: PathBuf,
    _api: Server,
}

/// The fake API's key, as the agents send it.
const API_KEY: &str = "sk-ant-api03-harness-0000000000000000000000000000";

impl Agent {
    fn open(setup: &Setup, ws: &Workspace, program: &str, args: &[&str]) -> Self {
        let port = free_port();
        let api_state = setup.root.join("api-state");
        std::fs::create_dir_all(&api_state).unwrap();
        let api = Environment::Local
            .command("python3")
            .arg(scripts().join("fake_api.py"))
            .var("HARNESS_STATE", &api_state)
            .var("HARNESS_PORT", port.to_string())
            .to_std()
            .spawn()
            .expect("python3 runs");
        let api = Server(api);
        let configured = Environment::Local
            .command("sh")
            .arg(scripts().join("setup.sh"))
            .arg(&setup.repo)
            .var("HOME", &setup.home)
            .var("ANTHROPIC_API_KEY", API_KEY)
            .var("HARNESS_PORT", port.to_string())
            .output()
            .unwrap();
        assert!(
            configured.status.success(),
            "setup.sh: {}",
            text(&configured.stderr)
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "the fake API did not come up");
            std::thread::sleep(Duration::from_millis(100));
        }
        let base = format!("http://127.0.0.1:{port}");
        let shim = setup.shim(
            program,
            &agent_bin().join(program).to_string_lossy(),
            &[
                ("ANTHROPIC_BASE_URL", base.clone()),
                ("ANTHROPIC_API_KEY", API_KEY.into()),
                ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".into()),
                ("DISABLE_AUTOUPDATER", "1".into()),
                ("OPENAI_API_KEY", "sk-harness-0000000000000000".into()),
                ("OPENAI_BASE_URL", format!("{base}/v1")),
                ("OPENAI_MODEL", "harness-model".into()),
            ],
        );
        let command = std::iter::once(shim)
            .chain(args.iter().map(|arg| (*arg).to_owned()))
            .collect();
        ws.open_terminal(Some(command), Some(program.into()));
        let agent = Self {
            terminal: ws.active_terminal().unwrap(),
            api_state,
            _api: api,
        };
        agent.wait_until_drawn(ws);
        agent
    }

    /// Polls until the agent drew its screen and it stayed still for three
    /// seconds, so the first prompt is not typed while it starts.
    fn wait_until_drawn(&self, ws: &Workspace) {
        let deadline = Instant::now() + PATIENCE;
        let mut last = self.terminal.session.revision();
        let mut still = Instant::now();
        loop {
            ws.poll();
            let revision = self.terminal.session.revision();
            if revision != last {
                (last, still) = (revision, Instant::now());
            }
            if still.elapsed() > Duration::from_secs(3) && !screen(&self.terminal).trim().is_empty()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the agent did not start:\n{}",
                screen(&self.terminal)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn requests(&self) -> Vec<Value> {
        json_lines(&self.api_state.join("requests.jsonl"))
    }

    /// Everything the model was sent that the log keeps.
    fn sent(&self) -> String {
        std::fs::read_to_string(self.api_state.join("requests.jsonl")).unwrap_or_default()
    }
}

impl Driver for Agent {
    fn terminal(&self) -> &Rc<TerminalPanel> {
        &self.terminal
    }

    fn start(&mut self, step: usize, command: &str) {
        type_line(&self.terminal, &format!("TASK step{step} RUN: {command}"));
    }

    fn output(&self, step: usize) -> Option<String> {
        let tag = format!("TASK step{step} RUN:");
        (self.requests().into_iter())
            .filter(|r| r["prompt"].as_str().is_some_and(|p| p.contains(&tag)))
            .find_map(|r| r["tool_output"].as_str().map(str::to_owned))
    }

    fn report(&self) -> String {
        let requests: Vec<String> = self.requests().iter().map(Value::to_string).collect();
        format!("--- requests ---\n{}", requests.join("\n"))
    }
}

/// How the user answers the questions a step raises.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Expect {
    /// No question comes up.
    NoQuestion,
    /// The question of guard `.0` comes up, answered with `.1`.
    Answer(&'static str, bool),
}

/// Runs the steps of one test in order.
struct Run<'a> {
    setup: &'a Setup,
    ws: &'a Workspace,
    driver: &'a mut dyn Driver,
    step: usize,
}

impl Run<'_> {
    /// Runs `script arg`, answers its questions as `expect` says, and
    /// returns what it printed with its exit status.
    fn step(&mut self, script: &str, arg: &str, expect: Expect) -> (String, i32) {
        self.step += 1;
        let step = self.step;
        let command = self.setup.command(script, arg);
        self.driver.start(step, &command);
        let deadline = Instant::now() + PATIENCE;
        let mut asked = Vec::new();
        let output = loop {
            self.ws.poll();
            let pending = self.ws.borrow().pending_approval();
            if let Some(question) = pending {
                asked.push((question.guard.clone(), question.host.clone()));
                let allow = match expect {
                    Expect::Answer(guard, allow) if guard == question.guard => allow,
                    _ => false,
                };
                self.ws.answer_approval(question.id, allow);
            }
            if let Some(output) = self.driver.output(step) {
                break output;
            }
            if Instant::now() > deadline {
                panic!(
                    "timed out waiting for `{command}`\n--- terminal ---\n{}\n{}",
                    screen(self.driver.terminal()),
                    self.driver.report()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let expected = match expect {
            Expect::NoQuestion => Vec::new(),
            Expect::Answer(guard, _) => vec![(guard.to_owned(), FORGE_ADDRESS.to_owned())],
        };
        assert_eq!(
            asked,
            expected,
            "questions of `{command}`:\n{output}\n--- forge ---\n{:#?}",
            self.setup.forge()
        );
        let status = (output.lines().rev())
            .find_map(|line| line.trim().strip_prefix("EXIT="))
            .and_then(|code| code.trim().parse().ok())
            .unwrap_or_else(|| panic!("`{command}` printed no status:\n{output}"));
        (output, status)
    }

    /// The sandbox holds fakes in place of the logins.
    fn only_fakes(&mut self) {
        let (shown, _) = self.step("show", "", Expect::NoQuestion);
        assert!(!shown.contains(GIT_TOKEN), "{shown}");
        assert!(!shown.contains(NPM_TOKEN), "{shown}");
        let login = format!("http://{USER}:");
        assert!(shown.contains(&login), "no faked git login:\n{shown}");
        assert!(
            shown.contains(":_authToken="),
            "no faked npm login:\n{shown}"
        );
    }

    /// Fetching needs no question, and the forge gets the real login.
    fn fetch(&mut self) {
        let (output, status) = self.step("fetch", "", Expect::NoQuestion);
        assert_eq!(status, 0, "{output}");
        let fetched = self.setup.served("service=git-upload-pack");
        assert!(!fetched.is_empty(), "{:#?}", self.setup.forge());
        let login = format!("{USER}:{GIT_TOKEN}");
        assert!(fetched.iter().all(|r| r["basic"] == login.as_str()));
    }
}

/// No request carried anything but the real credentials, or none.
fn forge_saw_no_fakes(setup: &Setup) {
    for request in setup.forge() {
        let auth = request["auth"].as_str().unwrap_or_default();
        let real_git = request["basic"] == format!("{USER}:{GIT_TOKEN}").as_str();
        let real_npm = auth == format!("Bearer {NPM_TOKEN}");
        assert!(
            auth.is_empty() || real_git || real_npm,
            "a fake reached the forge: {request}"
        );
    }
}

/// Pushes and an upload pass when the guards allow them, by setting or by
/// the user's answer, with the real credentials.
fn allowed_pushes_reach_the_forge(setup: &Setup, ws: &Workspace, driver: &mut dyn Driver) {
    let mut run = Run {
        setup,
        ws,
        driver,
        step: 0,
    };
    run.only_fakes();
    run.fetch();

    ws.set_guard_mode(GIT_PUSH, GuardMode::Allow);
    let (output, status) = run.step("push", "allowed", Expect::NoQuestion);
    assert_eq!(status, 0, "{output}");
    assert_eq!(
        setup.served("git-receive-pack").len(),
        2,
        "{:#?}",
        setup.forge()
    );

    ws.set_guard_mode(GIT_PUSH, GuardMode::Ask);
    let (output, status) = run.step("push", "answered", Expect::Answer(GIT_PUSH, true));
    assert_eq!(status, 0, "{output}");
    let pushed = setup.served("git-receive-pack");
    assert!(
        pushed.iter().any(|r| r["method"] == "POST") && pushed.len() == 4,
        "{:#?}",
        setup.forge()
    );

    let (output, status) = run.step("publish", "1.0.0", Expect::Answer(NPM_PUBLISH, true));
    assert_eq!(status, 0, "{output}");
    let uploads: Vec<Value> = (setup.forge().into_iter())
        .filter(|r| r["method"] == "PUT")
        .collect();
    assert_eq!(uploads.len(), 1, "{:#?}", setup.forge());
    assert_eq!(uploads[0]["auth"], format!("Bearer {NPM_TOKEN}").as_str());
    assert_eq!(uploads[0]["status"], 200);

    let branches = Environment::Local
        .command("git")
        .args(["ls-remote", "--heads"])
        .arg(setup.root.join("forge/app.git"))
        .stdout()
        .unwrap();
    let branches = String::from_utf8_lossy(&branches);
    assert!(
        branches.contains("refs/heads/allowed") && branches.contains("refs/heads/answered"),
        "{branches}"
    );
    forge_saw_no_fakes(setup);
}

/// Pushes and uploads the guards hold back, by setting or by the user's
/// answer, never reach the forge. Fetching still works.
fn refused_pushes_never_reach_the_forge(setup: &Setup, ws: &Workspace, driver: &mut dyn Driver) {
    let mut run = Run {
        setup,
        ws,
        driver,
        step: 0,
    };
    run.only_fakes();
    run.fetch();

    ws.set_guard_mode(GIT_PUSH, GuardMode::Block);
    let (output, status) = run.step("push", "blocked", Expect::NoQuestion);
    assert_ne!(status, 0, "{output}");
    assert!(output.contains("403"), "{output}");

    ws.set_guard_mode(GIT_PUSH, GuardMode::Ask);
    let (output, status) = run.step("push", "refused", Expect::Answer(GIT_PUSH, false));
    assert_ne!(status, 0, "{output}");

    let (output, status) = run.step("publish", "1.0.0", Expect::Answer(NPM_PUBLISH, false));
    assert_ne!(status, 0, "{output}");
    ws.set_guard_mode(NPM_PUBLISH, GuardMode::Block);
    let (output, status) = run.step("publish", "1.0.1", Expect::NoQuestion);
    assert_ne!(status, 0, "{output}");

    let forge = setup.forge();
    assert!(
        !(forge.iter()).any(|r| {
            r["method"] == "PUT"
                || r["path"]
                    .as_str()
                    .is_some_and(|p| p.contains("receive-pack"))
        }),
        "a held back request reached the forge: {forge:#?}"
    );
    forge_saw_no_fakes(setup);
}

#[test]
fn a_sandboxed_shell_pushes_with_real_logins_once_allowed() {
    if let Some(why) = missing() {
        eprintln!("{why}; skipped");
        return;
    }
    let setup = Setup::new();
    let ws = setup.workspace();
    let mut shell = Shell::open(&setup, &ws);
    allowed_pushes_reach_the_forge(&setup, &ws, &mut shell);
}

#[test]
fn a_sandboxed_shell_cannot_push_what_the_guards_refuse() {
    if let Some(why) = missing() {
        eprintln!("{why}; skipped");
        return;
    }
    let setup = Setup::new();
    let ws = setup.workspace();
    let mut shell = Shell::open(&setup, &ws);
    refused_pushes_never_reach_the_forge(&setup, &ws, &mut shell);
}

/// The agents' programs, installed with npm on first use.
fn agent_bin() -> PathBuf {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    let prefix = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/agent-harness/npm");
    let bin = prefix.join("node_modules/.bin");
    INSTALLED.call_once(|| {
        let present = ["claude", "codex", "qwen"]
            .iter()
            .all(|program| bin.join(program).exists());
        if present && std::env::var_os("CMDTERM_HARNESS_REBUILD").is_none() {
            return;
        }
        std::fs::create_dir_all(&prefix).unwrap();
        let output = Environment::Local
            .command("npm")
            .args(["install", "--no-audit", "--no-fund", "--prefix"])
            .arg(&prefix)
            .args([
                "@anthropic-ai/claude-code@latest",
                "@openai/codex@latest",
                "@qwen-code/qwen-code@latest",
            ])
            .output()
            .expect("npm runs");
        assert!(
            output.status.success(),
            "npm install: {}",
            text(&output.stderr)
        );
        // npm may block install scripts, and Claude Code's puts its native
        // program in place.
        let claude = prefix.join("node_modules/@anthropic-ai/claude-code");
        let output = Environment::Local
            .command("node")
            .arg("install.cjs")
            .cwd(&claude)
            .output()
            .expect("node runs");
        assert!(
            output.status.success(),
            "claude-code install.cjs: {}",
            text(&output.stderr)
        );
    });
    bin
}

/// The agent whose shell tool runs the steps, as the user starts it.
struct Case {
    program: &'static str,
    args: &'static [&'static str],
}

const CLAUDE: Case = Case {
    program: "claude",
    // Auto mode would have the model judge every command first.
    args: &["--permission-mode", "default"],
};
const CODEX: Case = Case {
    program: "codex",
    args: &[],
};
const QWEN: Case = Case {
    program: "qwen",
    args: &["--yolo"],
};

fn agent_test(case: &Case, allowed: bool) {
    if let Some(why) = missing() {
        eprintln!("{why}; skipped");
        return;
    }
    let setup = Setup::new();
    let ws = setup.workspace();
    let mut agent = Agent::open(&setup, &ws, case.program, case.args);
    if allowed {
        allowed_pushes_reach_the_forge(&setup, &ws, &mut agent);
    } else {
        refused_pushes_never_reach_the_forge(&setup, &ws, &mut agent);
    }
    let sent = agent.sent();
    assert!(
        !sent.contains(GIT_TOKEN) && !sent.contains(NPM_TOKEN),
        "a real login reached the model:\n{sent}"
    );
}

#[test]
#[ignore = "needs npm to install the agents"]
fn claude_code_in_the_sandbox_pushes_with_real_logins_once_allowed() {
    agent_test(&CLAUDE, true);
}

#[test]
#[ignore = "needs npm to install the agents"]
fn claude_code_in_the_sandbox_cannot_push_what_the_guards_refuse() {
    agent_test(&CLAUDE, false);
}

#[test]
#[ignore = "needs npm to install the agents"]
fn codex_in_the_sandbox_pushes_with_real_logins_once_allowed() {
    agent_test(&CODEX, true);
}

#[test]
#[ignore = "needs npm to install the agents"]
fn codex_in_the_sandbox_cannot_push_what_the_guards_refuse() {
    agent_test(&CODEX, false);
}

#[test]
#[ignore = "needs npm to install the agents"]
fn qwen_code_in_the_sandbox_pushes_with_real_logins_once_allowed() {
    agent_test(&QWEN, true);
}

#[test]
#[ignore = "needs npm to install the agents"]
fn qwen_code_in_the_sandbox_cannot_push_what_the_guards_refuse() {
    agent_test(&QWEN, false);
}
