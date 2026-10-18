//! The notification center behind the bell in the rail's footer. It keeps
//! the recent status bar notices, so they can be read after they expire,
//! and what the update checks find: agent integrations an older
//! scriptschnell-term installed, newer releases of agents installed with npm
//! or mise, and on Windows newer releases of scriptschnell-term itself.
//!
//! The list is shared with worker threads. Checks and updates run on their
//! own thread, since npm asks the registry and an SSH environment can take
//! seconds; the thread bumps `revision` and wakes the UI, and the next poll
//! rebuilds the rail's badge and the open overlay.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use cmdterm_env::Environment;
use cmdterm_update::{Channel, newer};
use schnellui::RedrawSignal;
use serde_json::Value;

use crate::agents::{self, Agent};

/// Notifications kept; the oldest go first.
const MAX_NOTIFICATIONS: usize = 50;
/// Wait after start before the first check, so it never slows the start.
const FIRST_CHECK: Duration = Duration::from_secs(10);
/// Time between two update checks while the app runs.
const CHECK_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);

/// How an agent was installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Manager {
    Npm,
    Mise,
}

impl Manager {
    pub fn name(self) -> &'static str {
        match self {
            Manager::Npm => "npm",
            Manager::Mise => "mise",
        }
    }
}

/// What a notification is about. Views turn it into text in the language
/// in effect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Topic {
    /// A status bar notice.
    Notice(String),
    /// An agent integration that an older version installed, or that was
    /// edited since.
    IntegrationOutdated {
        agent: Agent,
        env: Environment,
    },
    /// A newer release of an installed agent. `command` installs it.
    AgentUpdate {
        agent: Agent,
        env: Environment,
        manager: Manager,
        current: String,
        latest: String,
        command: Vec<String>,
    },
    /// An update or integration install finished.
    Updated {
        agent: Agent,
        env: Environment,
    },
    /// An update or integration install failed.
    Failed {
        agent: Agent,
        env: Environment,
        error: String,
    },
    /// A newer scriptschnell-term release on the update channel.
    AppUpdate {
        version: String,
    },
    /// A scriptschnell-term release was installed; it runs after a restart.
    AppUpdated {
        version: String,
    },
    AppUpdateFailed {
        error: String,
    },
}

impl Topic {
    /// Whether `self` and `other` report on the same thing, so a newer
    /// check result replaces the older one.
    fn same_subject(&self, other: &Topic) -> bool {
        match (self, other) {
            (
                Topic::IntegrationOutdated { agent, env },
                Topic::IntegrationOutdated {
                    agent: other_agent,
                    env: other_env,
                },
            ) => agent == other_agent && env == other_env,
            (
                Topic::AgentUpdate {
                    agent,
                    env,
                    manager,
                    ..
                },
                Topic::AgentUpdate {
                    agent: other_agent,
                    env: other_env,
                    manager: other_manager,
                    ..
                },
            ) => agent == other_agent && env == other_env && manager == other_manager,
            (Topic::AppUpdate { .. }, Topic::AppUpdate { .. }) => true,
            _ => false,
        }
    }

    /// Whether a check of `env` produces this topic, so a check that no
    /// longer finds it removes it.
    fn checked_on(&self, checked: &Environment) -> bool {
        match self {
            Topic::IntegrationOutdated { env, .. } | Topic::AgentUpdate { env, .. } => {
                env == checked
            }
            _ => false,
        }
    }

    /// Whether the overlay offers an Update button for it.
    pub fn actionable(&self) -> bool {
        matches!(
            self,
            Topic::IntegrationOutdated { .. } | Topic::AgentUpdate { .. } | Topic::AppUpdate { .. }
        )
    }
}

#[derive(Clone, Debug)]
pub struct Notification {
    pub id: u64,
    pub topic: Topic,
    pub at: SystemTime,
    pub read: bool,
    /// Its update runs.
    pub busy: bool,
}

#[derive(Default)]
struct List {
    items: VecDeque<Notification>,
    /// Check results the user dismissed. The same result does not come
    /// back while the app runs; a newer release does.
    dismissed: Vec<Topic>,
    next_id: u64,
}

impl List {
    fn push(&mut self, topic: Topic, read: bool) {
        if self.dismissed.contains(&topic) {
            return;
        }
        if let Topic::Notice(_) = topic {
            // A repeated notice moves to the top with its new time.
            self.items.retain(|item| item.topic != topic);
        } else if self.items.iter().any(|item| item.topic == topic) {
            return;
        }
        self.items.retain(|item| !item.topic.same_subject(&topic));
        if self.items.len() == MAX_NOTIFICATIONS {
            self.items.pop_front();
        }
        self.next_id += 1;
        self.items.push_back(Notification {
            id: self.next_id,
            topic,
            at: SystemTime::now(),
            read,
            busy: false,
        });
    }
}

/// The notification list, its revision, and the update check schedule.
#[derive(Clone, Default)]
pub struct Notifications {
    list: Arc<Mutex<List>>,
    revision: Arc<AtomicU64>,
    /// The revision the UI last drew.
    shown: Arc<AtomicU64>,
    /// Set by the schedule thread when a check is due.
    due: Arc<AtomicBool>,
    checking: Arc<AtomicBool>,
}

impl Notifications {
    /// The notifications, newest first.
    pub fn items(&self) -> Vec<Notification> {
        self.list
            .lock()
            .map(|list| list.items.iter().rev().cloned().collect())
            .unwrap_or_default()
    }

    /// The unread notifications as the list shows them: the outdated
    /// integrations of several agents share one card, so they count once.
    pub fn unread(&self) -> usize {
        self.list
            .lock()
            .map(|list| {
                let unread = list.items.iter().filter(|item| !item.read).count();
                let outdated = || {
                    list.items.iter().filter(|item| {
                        !item.busy && matches!(item.topic, Topic::IntegrationOutdated { .. })
                    })
                };
                if outdated().count() < 2 {
                    return unread;
                }
                let grouped = outdated().filter(|item| !item.read).count();
                unread - grouped + grouped.min(1)
            })
            .unwrap_or(0)
    }

    /// Adds a notification. One that reports on the same subject as an
    /// older one replaces it.
    pub fn push(&self, topic: Topic, read: bool) {
        self.edit(|list| list.push(topic, read));
    }

    pub fn mark_all_read(&self) {
        let any = self
            .list
            .lock()
            .map(|list| list.items.iter().any(|item| !item.read))
            .unwrap_or(false);
        if any {
            self.edit(|list| list.items.iter_mut().for_each(|item| item.read = true));
        }
    }

    pub fn dismiss(&self, id: u64) {
        self.edit(|list| {
            if let Some(index) = list.items.iter().position(|item| item.id == id) {
                let item = list.items.remove(index);
                if let Some(item) = item
                    && item.topic.actionable()
                {
                    list.dismissed.push(item.topic);
                }
            }
        });
    }

    /// Removes every notification whose update does not run.
    pub fn clear(&self) {
        self.edit(|list| {
            let (busy, gone): (VecDeque<_>, VecDeque<_>) =
                list.items.drain(..).partition(|item| item.busy);
            list.items = busy;
            list.dismissed.extend(
                gone.into_iter()
                    .map(|item| item.topic)
                    .filter(Topic::actionable),
            );
        });
    }

    /// True once after the list changed. Allocation-free, called per poll.
    pub fn changed(&self) -> bool {
        let revision = self.revision.load(Ordering::Acquire);
        self.shown.swap(revision, Ordering::AcqRel) != revision
    }

    /// True once after the schedule thread found a check due.
    pub fn take_due(&self) -> bool {
        self.due.swap(false, Ordering::AcqRel)
    }

    pub fn checking(&self) -> bool {
        self.checking.load(Ordering::Acquire)
    }

    fn edit(&self, change: impl FnOnce(&mut List)) {
        if let Ok(mut list) = self.list.lock() {
            change(&mut list);
        }
        self.revision.fetch_add(1, Ordering::Release);
    }

    fn signal(&self, wake: &OnceLock<RedrawSignal>) {
        self.revision.fetch_add(1, Ordering::Release);
        if let Some(signal) = wake.get() {
            signal.request_redraw();
        }
    }

    /// Marks a check due shortly after start, then every
    /// [`CHECK_INTERVAL`], and wakes the UI each time.
    pub fn schedule(&self, wake: Arc<OnceLock<RedrawSignal>>) {
        let due = self.due.clone();
        let _ = std::thread::Builder::new()
            .name("update-schedule".into())
            .spawn(move || {
                let mut wait = FIRST_CHECK;
                loop {
                    std::thread::sleep(wait);
                    due.store(true, Ordering::Release);
                    if let Some(signal) = wake.get() {
                        signal.request_redraw();
                    }
                    wait = CHECK_INTERVAL;
                }
            });
    }

    /// Checks every environment in `envs` for outdated agent integrations
    /// and agent releases, and the update channel for a newer app release,
    /// on a worker thread. Does nothing while a check runs.
    pub fn check(&self, envs: Vec<Environment>, wake: Arc<OnceLock<RedrawSignal>>) {
        if self.checking.swap(true, Ordering::AcqRel) {
            return;
        }
        self.revision.fetch_add(1, Ordering::Release);
        let center = self.clone();
        let spawned = std::thread::Builder::new()
            .name("update-check".into())
            .spawn(move || {
                for env in envs {
                    let found = check_env(&env);
                    center.edit(|list| {
                        list.items.retain(|item| {
                            item.busy || !item.topic.checked_on(&env) || found.contains(&item.topic)
                        });
                        for topic in found {
                            list.push(topic, false);
                        }
                    });
                    center.signal(&wake);
                }
                if let Some(channel) = app_channel() {
                    match channel.check(env!("CARGO_PKG_VERSION")) {
                        Ok(found) => center.edit(|list| {
                            list.items.retain(|item| {
                                item.busy || !matches!(item.topic, Topic::AppUpdate { .. })
                            });
                            if let Some(manifest) = found {
                                list.push(
                                    Topic::AppUpdate {
                                        version: manifest.version,
                                    },
                                    false,
                                );
                            }
                        }),
                        Err(error) => eprintln!("scriptschnell-term: update check: {error}"),
                    }
                }
                center.checking.store(false, Ordering::Release);
                center.signal(&wake);
            });
        if spawned.is_err() {
            self.checking.store(false, Ordering::Release);
        }
    }

    /// Runs the update notification `id` offers, on a worker thread. It is
    /// replaced by an [`Topic::Updated`] or [`Topic::Failed`] notification.
    pub fn run(&self, id: u64, wake: Arc<OnceLock<RedrawSignal>>) {
        self.run_where(|item| item.id == id, wake);
    }

    /// Runs the updates of the notifications `ids`, one after another on one
    /// worker thread.
    pub fn run_ids(&self, ids: Vec<u64>, wake: Arc<OnceLock<RedrawSignal>>) {
        self.run_where(move |item| ids.contains(&item.id), wake);
    }

    /// Runs every update the notifications offer, one after another on one
    /// worker thread, so two package managers never run in the same
    /// environment at once.
    pub fn run_all(&self, wake: Arc<OnceLock<RedrawSignal>>) {
        self.run_where(|_| true, wake);
    }

    /// Runs the updates of the notifications `pick` selects, in order on a
    /// worker thread.
    fn run_where(&self, pick: impl Fn(&Notification) -> bool, wake: Arc<OnceLock<RedrawSignal>>) {
        let jobs = self.claim(pick);
        if jobs.is_empty() {
            return;
        }
        let center = self.clone();
        let _ = std::thread::Builder::new()
            .name("update".into())
            .spawn(move || {
                for (id, topic) in jobs {
                    let Some(topic) = perform(topic) else {
                        continue;
                    };
                    center.edit(|list| {
                        list.items.retain(|item| item.id != id);
                        list.push(topic, false);
                    });
                    center.signal(&wake);
                }
            });
    }

    /// Marks the actionable notifications `pick` selects busy and returns
    /// their ids and topics, oldest first.
    fn claim(&self, pick: impl Fn(&Notification) -> bool) -> Vec<(u64, Topic)> {
        let mut jobs = Vec::new();
        self.edit(|list| {
            for item in list.items.iter_mut() {
                if !item.busy && item.topic.actionable() && pick(item) {
                    item.busy = true;
                    jobs.push((item.id, item.topic.clone()));
                }
            }
        });
        jobs
    }
}

/// Runs the update `topic` offers and returns the notification that
/// reports how it went.
fn perform(topic: Topic) -> Option<Topic> {
    Some(match topic {
        Topic::IntegrationOutdated { agent, env } => match agents::reinstall(&env, agent) {
            Ok(()) => Topic::Updated { agent, env },
            Err(error) => Topic::Failed { agent, env, error },
        },
        Topic::AgentUpdate {
            agent,
            env,
            command,
            ..
        } => match run_command(&env, &command) {
            Ok(()) => Topic::Updated { agent, env },
            Err(error) => Topic::Failed { agent, env, error },
        },
        Topic::AppUpdate { .. } => match install_app_update() {
            Ok(version) => Topic::AppUpdated { version },
            Err(error) => Topic::AppUpdateFailed { error },
        },
        _ => return None,
    })
}

/// The update channel of this platform, when this build updates itself.
/// A `cargo install` build leaves updates to cargo.
fn app_channel() -> Option<Channel> {
    let platform = cmdterm_update::PLATFORM?;
    let key = cmdterm_update::release_key()?;
    if std::env::current_exe().is_ok_and(|exe| cmdterm_update::installed_by_cargo(&exe)) {
        return None;
    }
    Some(Channel::new(
        cmdterm_update::DEFAULT_CHANNEL,
        platform,
        key,
        crate::forge::http_proxy(),
    ))
}

/// Downloads the channel's current release, checks it against the signed
/// manifest and replaces the files next to the running executable. Asks the
/// channel again rather than trusting the earlier check.
fn install_app_update() -> Result<String, String> {
    let channel = app_channel().ok_or("this build does not update itself")?;
    let manifest = channel
        .check(env!("CARGO_PKG_VERSION"))
        .map_err(|e| e.to_string())?
        .ok_or("no newer release")?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().ok_or("the executable has no directory")?;
    channel.install(&manifest, dir).map_err(|e| e.to_string())?;
    Ok(manifest.version)
}

/// Removes the files an earlier app update replaced. Windows keeps a
/// running executable locked, so they can only go at the next start.
pub fn remove_update_leftovers() {
    if cmdterm_update::PLATFORM.is_none() {
        return;
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.to_owned()))
    {
        cmdterm_update::remove_leftovers(&dir);
    }
}

/// Runs `command` on `env`; the error is its last line of output.
fn run_command(env: &Environment, command: &[String]) -> Result<(), String> {
    let output = env
        .command(&command[0])
        .args(&command[1..])
        .output()
        .map_err(|error| format!("{}: {error}", command.join(" ")))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let last = stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or_default();
    Err(format!("{}: {last}", command.join(" ")))
}

/// What one environment has to report. Blocking.
fn check_env(env: &Environment) -> Vec<Topic> {
    let _ = agents::refresh_scripts(env);
    let mut found = Vec::new();
    for agent in Agent::all() {
        if agents::integration_outdated(env, agent) {
            found.push(Topic::IntegrationOutdated {
                agent,
                env: env.clone(),
            });
        }
    }
    found.extend(npm_updates(env));
    found.extend(mise_updates(env));
    found
}

/// The npm packages each agent is published as.
pub(crate) fn npm_packages(agent: Agent) -> &'static [&'static str] {
    match agent {
        Agent::ClaudeCode => &["@anthropic-ai/claude-code"],
        Agent::Codex => &["@openai/codex"],
        // opencode moved to @opencode/cli with 2.0.
        Agent::OpenCode => &["@opencode/cli", "opencode-ai"],
        Agent::Pi => &[
            "@earendil-works/pi-coding-agent",
            "@mariozechner/pi-coding-agent",
        ],
        Agent::Copilot => &["@github/copilot"],
        Agent::Qwen => &["@qwen-code/qwen-code"],
        Agent::Gemini => &["@google/gemini-cli"],
        Agent::Kimi => &["@moonshot-ai/kimi-code"],
        // Published on PyPI only.
        Agent::Vibe => &[],
        Agent::Custom(agent) => agent.npm,
    }
}

/// The agent a mise tool name stands for: a registry short name, or an
/// npm backend with one of [`npm_packages`].
fn mise_agent(tool: &str) -> Option<Agent> {
    let short = match tool {
        "claude" | "claude-code" => Some(Agent::ClaudeCode),
        "codex" => Some(Agent::Codex),
        "opencode" => Some(Agent::OpenCode),
        "pi" => Some(Agent::Pi),
        "copilot" | "copilot-cli" => Some(Agent::Copilot),
        "qwen" | "qwen-code" => Some(Agent::Qwen),
        "gemini" | "gemini-cli" => Some(Agent::Gemini),
        _ => Agent::all()
            .into_iter()
            .find(|agent| agent.extension().is_some() && agent.program() == tool),
    };
    short.or_else(|| {
        let package = tool.strip_prefix("npm:")?;
        Agent::all()
            .into_iter()
            .find(|agent| npm_packages(*agent).contains(&package))
    })
}

/// Agent packages in `npm ls -g --depth=0 --json` output, with their
/// versions.
fn parse_npm_ls(json: &str) -> Vec<(Agent, String, String)> {
    let Ok(root) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let Some(dependencies) = root.get("dependencies").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for agent in Agent::all() {
        for package in npm_packages(agent) {
            if let Some(version) = dependencies
                .get(*package)
                .and_then(|dependency| dependency.get("version"))
                .and_then(Value::as_str)
            {
                found.push((agent, (*package).to_owned(), version.to_owned()));
            }
        }
    }
    found
}

/// Active agent tools in `mise ls --json` output: the tool, its version,
/// and the version the configuration asks for.
fn parse_mise_ls(json: &str) -> Vec<(Agent, String, String, Option<String>)> {
    let Ok(Value::Object(tools)) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for (tool, versions) in tools {
        let Some(agent) = mise_agent(&tool) else {
            continue;
        };
        let active = versions.as_array().into_iter().flatten().find(|version| {
            version.get("active").and_then(Value::as_bool) == Some(true)
                && version.get("installed").and_then(Value::as_bool) != Some(false)
        });
        if let Some(active) = active
            && let Some(version) = active.get("version").and_then(Value::as_str)
        {
            let requested = active
                .get("requested_version")
                .and_then(Value::as_str)
                .map(str::to_owned);
            found.push((agent, tool.clone(), version.to_owned(), requested));
        }
    }
    found
}

/// The first line of a successful command's output.
fn first_line(env: &Environment, program: &str, args: &[&str]) -> Option<String> {
    let output = env
        .command(program)
        .args(args.iter().copied())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    Some(line.to_owned())
}

fn npm_updates(env: &Environment) -> Vec<Topic> {
    // `npm ls` exits non-zero for problems in unrelated packages, so its
    // output counts even then.
    let Ok(output) = env
        .command("npm")
        .args(["ls", "-g", "--depth=0", "--json"])
        .output()
    else {
        return Vec::new();
    };
    let installed = parse_npm_ls(&String::from_utf8_lossy(&output.stdout));
    let root = first_line(env, "npm", &["root", "-g"]);
    let mut found = Vec::new();
    for (agent, package, current) in installed {
        if let Some(root) = &root
            && !npm_writable(env, root, &package)
        {
            continue;
        }
        let Some(latest) = first_line(env, "npm", &["view", &package, "version"]) else {
            continue;
        };
        if newer(&latest, &current) {
            found.push(Topic::AgentUpdate {
                agent,
                env: env.clone(),
                manager: Manager::Npm,
                current,
                latest,
                command: vec![
                    "npm".into(),
                    "install".into(),
                    "-g".into(),
                    format!("{package}@latest"),
                ],
            });
        }
    }
    found
}

/// The directory `npm install -g` replaces `package` in: the scope's
/// directory for a scoped package, else the global `node_modules`.
fn npm_install_dir(root: &str, package: &str) -> String {
    match package.split_once('/') {
        Some((scope, _)) => format!("{}/{scope}", root.trim_end_matches(['/', '\\'])),
        None => root.to_owned(),
    }
}

/// Whether `npm install -g` can replace `package` without root. A system
/// package manager such as pacman installs agents like Gemini CLI into a
/// root-owned global prefix; npm fails there with `EACCES`, and the update
/// belongs to that package manager anyway. Where `test` cannot run, as on
/// Windows, the package counts as writable.
fn npm_writable(env: &Environment, root: &str, package: &str) -> bool {
    match env
        .command("test")
        .args(["-w", &npm_install_dir(root, package)])
        .output()
    {
        Ok(output) => output.status.success(),
        Err(_) => true,
    }
}

fn mise_updates(env: &Environment) -> Vec<Topic> {
    let Some(json) = env
        .command("mise")
        .args(["ls", "--json"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
    else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for (agent, tool, current, requested) in parse_mise_ls(&json) {
        let Some(latest) = first_line(env, "mise", &["latest", &tool]) else {
            continue;
        };
        if newer(&latest, &current) {
            found.push(Topic::AgentUpdate {
                agent,
                env: env.clone(),
                manager: Manager::Mise,
                current,
                latest,
                command: mise_upgrade(&tool, requested.as_deref()),
            });
        }
    }
    found
}

/// The command that installs the newest `tool`. A pinned version needs
/// `--bump`, which also writes the new version into the configuration.
fn mise_upgrade(tool: &str, requested: Option<&str>) -> Vec<String> {
    let mut command = vec!["mise".to_owned(), "upgrade".to_owned()];
    if requested.is_some_and(|requested| requested != "latest") {
        command.push("--bump".to_owned());
    }
    command.push(tool.to_owned());
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(latest: &str) -> Topic {
        Topic::AgentUpdate {
            agent: Agent::Codex,
            env: Environment::Local,
            manager: Manager::Npm,
            current: "1.0.0".into(),
            latest: latest.into(),
            command: Vec::new(),
        }
    }

    #[test]
    fn npm_install_dir_is_the_scope_directory() {
        assert_eq!(
            npm_install_dir("/usr/lib/node_modules", "@google/gemini-cli"),
            "/usr/lib/node_modules/@google"
        );
        assert_eq!(
            npm_install_dir("/usr/lib/node_modules/", "opencode-ai"),
            "/usr/lib/node_modules/"
        );
    }

    #[test]
    fn npm_ls_finds_agent_packages() {
        let json = r#"{"name":"lib","dependencies":{
            "@anthropic-ai/claude-code":{"version":"2.1.0"},
            "typescript":{"version":"6.0.3"},
            "@mariozechner/pi-coding-agent":{"version":"0.70.0"}}}"#;
        let found = parse_npm_ls(json);
        assert_eq!(
            found,
            [
                (
                    Agent::ClaudeCode,
                    "@anthropic-ai/claude-code".into(),
                    "2.1.0".into()
                ),
                (
                    Agent::Pi,
                    "@mariozechner/pi-coding-agent".into(),
                    "0.70.0".into()
                ),
            ]
        );
        assert!(parse_npm_ls("not json").is_empty());
    }

    #[test]
    fn mise_ls_takes_the_active_version() {
        let json = r#"{
            "claude-code": [
                {"version":"2.1.302","installed":true,"active":false},
                {"version":"2.1.304","requested_version":"latest","installed":true,"active":true}
            ],
            "npm:@openai/codex": [
                {"version":"0.150.0","requested_version":"0.150","installed":true,"active":true}
            ],
            "usage": [{"version":"7.1.0","active":true}],
            "opencode": [{"version":"1.0.0","installed":true,"active":false}]
        }"#;
        let mut found = parse_mise_ls(json);
        found.sort_by(|a, b| a.1.cmp(&b.1));
        assert_eq!(
            found,
            [
                (
                    Agent::ClaudeCode,
                    "claude-code".into(),
                    "2.1.304".into(),
                    Some("latest".into())
                ),
                (
                    Agent::Codex,
                    "npm:@openai/codex".into(),
                    "0.150.0".into(),
                    Some("0.150".into())
                ),
            ]
        );
    }

    #[test]
    fn pinned_mise_tools_upgrade_with_bump() {
        assert_eq!(
            mise_upgrade("claude-code", Some("latest")),
            ["mise", "upgrade", "claude-code"]
        );
        assert_eq!(
            mise_upgrade("codex", Some("0.150")),
            ["mise", "upgrade", "--bump", "codex"]
        );
        assert_eq!(mise_upgrade("codex", None), ["mise", "upgrade", "codex"]);
    }

    #[test]
    fn a_newer_release_replaces_the_older_notification() {
        let center = Notifications::default();
        center.push(update("1.1.0"), false);
        center.push(update("1.1.0"), false);
        assert_eq!(center.items().len(), 1);
        center.push(update("1.2.0"), false);
        let items = center.items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].topic, update("1.2.0"));
    }

    #[test]
    fn a_newer_app_release_replaces_the_older_notification() {
        let center = Notifications::default();
        let app = |version: &str| Topic::AppUpdate {
            version: version.into(),
        };
        center.push(app("0.2.0"), false);
        center.push(update("1.1.0"), false);
        center.push(app("0.3.0"), false);
        let items = center.items();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].topic, app("0.3.0"));
        assert!(items[0].topic.actionable());
    }

    #[test]
    fn update_all_claims_every_waiting_update_once() {
        let center = Notifications::default();
        center.push(update("1.1.0"), false);
        center.push(Topic::Notice("hello".into()), false);
        center.push(
            Topic::AppUpdate {
                version: "0.3.0".into(),
            },
            false,
        );
        let jobs = center.claim(|_| true);
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].1, update("1.1.0"));
        assert!(center.claim(|_| true).is_empty());
        let busy = center.items().iter().filter(|item| item.busy).count();
        assert_eq!(busy, 2);
    }

    #[test]
    fn dismissed_results_stay_away_until_a_newer_release() {
        let center = Notifications::default();
        center.push(update("1.1.0"), false);
        let id = center.items()[0].id;
        center.dismiss(id);
        center.push(update("1.1.0"), false);
        assert!(center.items().is_empty());
        center.push(update("1.2.0"), false);
        assert_eq!(center.items().len(), 1);
    }

    #[test]
    fn notices_arrive_read_and_the_list_is_bounded() {
        let center = Notifications::default();
        assert!(!center.changed());
        for i in 0..MAX_NOTIFICATIONS + 5 {
            center.push(Topic::Notice(format!("notice {i}")), true);
        }
        assert!(center.changed());
        assert!(!center.changed());
        let items = center.items();
        assert_eq!(items.len(), MAX_NOTIFICATIONS);
        assert_eq!(
            items[0].topic,
            Topic::Notice(format!("notice {}", MAX_NOTIFICATIONS + 4))
        );
        assert_eq!(center.unread(), 0);
        center.push(update("1.1.0"), false);
        assert_eq!(center.unread(), 1);
        center.mark_all_read();
        assert_eq!(center.unread(), 0);
        center.clear();
        assert!(center.items().is_empty());
    }

    #[test]
    fn outdated_integrations_of_several_agents_count_once_as_unread() {
        let center = Notifications::default();
        let outdated = |agent| Topic::IntegrationOutdated {
            agent,
            env: Environment::Local,
        };
        center.push(outdated(Agent::ClaudeCode), false);
        assert_eq!(center.unread(), 1);
        center.push(outdated(Agent::Codex), false);
        center.push(outdated(Agent::Pi), false);
        // One card in the list, so one in the badge.
        assert_eq!(center.unread(), 1);
        center.push(Topic::Notice("hello".to_owned()), false);
        assert_eq!(center.unread(), 2);
        center.mark_all_read();
        assert_eq!(center.unread(), 0);
    }
}
