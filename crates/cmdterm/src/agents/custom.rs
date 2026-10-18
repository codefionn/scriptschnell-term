//! Coding agents extensions register with `term.agent`.
//!
//! [`Agent`](super::Agent) is `Copy` and answers with `&'static str`, so the
//! app's code treats every agent alike. An extension agent becomes a
//! [`CustomAgent`] with leaked strings. Equal registrations share one, so a
//! reload of an unchanged extension leaks nothing; only a changed
//! registration costs a few hundred bytes for the rest of the run.
//!
//! The workspace hands each extension host's agents to [`set`] under the
//! host's owner id. Worker threads read them through [`all`], as they read
//! the built-in agents.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{PoisonError, RwLock};

use crate::ext::{AgentInfo, AgentIntegration, McpStyle};

use super::{BROWSER_MCP_PATH, CONTROL_PATH, HELPER_PATH, Hook};

/// An extension agent, with every string leaked so it lives as long as the
/// built-in ones. Paths are relative to `$HOME`.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct CustomAgent {
    pub name: &'static str,
    pub extension: &'static str,
    pub title: &'static str,
    pub program: &'static str,
    pub config_dir: &'static str,
    pub skills_dir: Option<&'static str>,
    /// The integration's file, empty for a native one.
    pub target: &'static str,
    pub integration: CustomIntegration,
    /// `(file, key, style)`.
    pub mcp: Option<(&'static str, &'static str, McpStyle)>,
    /// The program and the arguments in front of a session id.
    pub resume: Option<&'static [&'static str]>,
    pub session_at_start: bool,
    /// The capabilities it opted in to, as [`Capability::bit`]s.
    ///
    /// [`Capability::bit`]: super::Capability::bit
    pub supports: u8,
    /// The arguments that start it with a first prompt.
    pub prompt: Option<&'static [&'static str]>,
    /// Where it keeps its sessions, as `sh` expands it, and whether a
    /// session's id is in a file's name there.
    pub sessions: Option<(&'static str, bool)>,
    pub writable: &'static [&'static str],
    pub read_only: &'static [&'static str],
    pub synced: &'static [&'static str],
    pub home_var: Option<(&'static str, &'static str)>,
    /// The variables it logs in with, which sandboxes keep.
    pub login_vars: &'static [&'static str],
    pub guarded: &'static [Guarded],
    pub npm: &'static [&'static str],
}

/// A synced file and the key paths in it whose changes ask.
pub type Guarded = (&'static str, &'static [&'static [&'static str]]);

#[derive(Debug, PartialEq, Eq, Hash)]
pub enum CustomIntegration {
    Native,
    Hooks(&'static [Hook], u64),
    /// The file's content with its placeholders filled in.
    File(&'static str),
}

/// Registered agents per owner, in registration order.
static REGISTERED: RwLock<Vec<(u64, &'static CustomAgent)>> = RwLock::new(Vec::new());
/// Every agent leaked so far, with the registration it came from.
static INTERNED: RwLock<Vec<(AgentInfo, &'static CustomAgent)>> = RwLock::new(Vec::new());
/// Bumped by every [`set`] that changes the list.
static REVISION: AtomicU64 = AtomicU64::new(0);

/// Replaces the agents `owner` registered with `agents`. Returns the
/// extension and an error for each one that takes the name or program of
/// an agent the app knows itself or another owner registered first; those
/// are left out.
pub fn set(owner: u64, agents: &[AgentInfo]) -> Vec<crate::ext::ExtError> {
    let mut errors = Vec::new();
    let mut list = REGISTERED.write().unwrap_or_else(PoisonError::into_inner);
    let before: Vec<&'static CustomAgent> = (list.iter())
        .filter(|(id, _)| *id == owner)
        .map(|(_, agent)| *agent)
        .collect();
    list.retain(|(id, _)| *id != owner);
    for info in agents {
        let clash = super::BUILTIN_NAMES
            .iter()
            .any(|name| *name == info.name || *name == info.program)
            || (list.iter())
                .any(|(_, other)| other.name == info.name || other.program == info.program);
        if clash {
            errors.push(crate::ext::ExtError::new(
                &info.extension,
                format!(
                    "term.agent: '{}' is not registered: an agent named '{}' or '{}' exists already",
                    info.name, info.name, info.program
                ),
            ));
            continue;
        }
        list.push((owner, intern(info)));
    }
    let after: Vec<&'static CustomAgent> = (list.iter())
        .filter(|(id, _)| *id == owner)
        .map(|(_, agent)| *agent)
        .collect();
    if before != after {
        REVISION.fetch_add(1, Ordering::Release);
    }
    errors
}

/// Every registered agent, in order of owner and registration.
pub fn all() -> Vec<&'static CustomAgent> {
    let list = REGISTERED.read().unwrap_or_else(PoisonError::into_inner);
    list.iter().map(|(_, agent)| *agent).collect()
}

/// The registered agent named `name`.
pub fn named(name: &str) -> Option<&'static CustomAgent> {
    let list = REGISTERED.read().unwrap_or_else(PoisonError::into_inner);
    list.iter()
        .map(|(_, agent)| *agent)
        .find(|agent| agent.name == name)
}

/// Changes whenever the registered agents change.
pub fn revision() -> u64 {
    REVISION.load(Ordering::Acquire)
}

fn intern(info: &AgentInfo) -> &'static CustomAgent {
    {
        let interned = INTERNED.read().unwrap_or_else(PoisonError::into_inner);
        if let Some((_, agent)) = interned.iter().find(|(known, _)| known == info) {
            return agent;
        }
    }
    let agent: &'static CustomAgent = Box::leak(Box::new(leak_agent(info)));
    let mut interned = INTERNED.write().unwrap_or_else(PoisonError::into_inner);
    interned.push((info.clone(), agent));
    agent
}

fn leak(text: &str) -> &'static str {
    Box::leak(text.to_owned().into_boxed_str())
}

fn leak_list(list: &[String]) -> &'static [&'static str] {
    Box::leak(list.iter().map(|item| leak(item)).collect::<Box<[_]>>())
}

/// `content` with the helpers' paths in place of their placeholders.
pub fn fill(content: &str) -> String {
    content
        .replace("{helper}", HELPER_PATH)
        .replace("{control}", CONTROL_PATH)
        .replace("{browser_mcp}", BROWSER_MCP_PATH)
}

fn leak_agent(info: &AgentInfo) -> CustomAgent {
    let (target, integration) = match &info.integration {
        AgentIntegration::Native => ("", CustomIntegration::Native),
        AgentIntegration::Hooks {
            file,
            hooks,
            timeout_unit,
        } => {
            let hooks: Box<[Hook]> = (hooks.iter())
                .map(|hook| {
                    (
                        leak(&hook.event),
                        hook.matcher.as_deref().map(leak),
                        leak(&hook.args),
                    )
                })
                .collect();
            (
                leak(file),
                CustomIntegration::Hooks(Box::leak(hooks), *timeout_unit),
            )
        }
        AgentIntegration::File { file, content } => {
            (leak(file), CustomIntegration::File(leak(&fill(content))))
        }
    };
    let resume = info.resume.as_ref().map(|args| {
        let argv: Vec<String> = std::iter::once(info.program.clone())
            .chain(args.iter().cloned())
            .collect();
        leak_list(&argv)
    });
    let guarded: Box<[Guarded]> = (info.sandbox.guarded)
        .iter()
        .map(|(file, paths)| {
            let paths: Box<[&'static [&'static str]]> =
                paths.iter().map(|path| leak_list(path)).collect();
            (leak(file), &*Box::leak(paths))
        })
        .collect();
    CustomAgent {
        name: leak(&info.name),
        extension: leak(&info.extension),
        title: leak(&info.title),
        program: leak(&info.program),
        config_dir: leak(&info.config_dir),
        skills_dir: info.skills_dir.as_deref().map(leak),
        target,
        integration,
        mcp: (info.mcp.as_ref()).map(|mcp| (leak(&mcp.file), leak(&mcp.key), mcp.style)),
        resume,
        session_at_start: info.session_at_start,
        supports: (info.supports.iter()).fold(0, |set, capability| set | capability.bit()),
        prompt: info.prompt.as_deref().map(leak_list),
        sessions: (info.sessions.as_ref())
            .map(|sessions| (leak(&format!("$HOME/{}", sessions.dir)), sessions.by_name)),
        writable: leak_list(&info.sandbox.writable),
        read_only: leak_list(&info.sandbox.read_only),
        synced: leak_list(&info.sandbox.synced),
        home_var: (info.sandbox.home_var.as_ref()).map(|(var, dir)| (leak(var), leak(dir))),
        login_vars: leak_list(&info.sandbox.login_vars),
        guarded: Box::leak(guarded),
        npm: leak_list(&info.npm),
    }
}
