//! The Lua-facing `term` table and the state its functions share with the host.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use oximond::runtime::TableHandle;
use oximond::{LuaString, Runtime, RuntimeError, RuntimeResult, Value};

use crate::ext::manifest::{Permission, Permissions};
use crate::ext::watchdog::Alarm;

use crate::ext::store::{self, StoreHandle};
use crate::ext::{
    AgentHook, AgentInfo, AgentIntegration, AgentMcp, AgentSandbox, BlocklistSource, BranchReport,
    BranchState, Callback, Check, CheckState, CommandInfo, Comment, Context, Discussion,
    DiscussionRequest, EVENT_KINDS, EVENT_NAMES, Event, ForgeInfo, HttpRequest, HttpResponse,
    JobResult, KeyRequest, MIN_TASK_INTERVAL, McpStyle, NotifyLevel, Push, Request, Review,
    SandboxRules, TaskInfo, TaskProject, Thread, ToolCaller, ToolInfo, VERSION, Verdict, ViewInfo,
};

/// A key into an extension's function table, which lives in the Lua
/// registry so the GC keeps registered functions alive.
pub(crate) type Ref = i64;

/// Per-extension state the natives write.
#[derive(Default)]
pub(crate) struct Slot {
    pub(crate) handlers: [Vec<Ref>; EVENT_KINDS],
    next_ref: Ref,
    /// What the last `term.sandbox` call asked for.
    pub(crate) sandbox: SandboxRules,
    /// The values of the settings its manifest declares, in order.
    pub(crate) settings: Vec<(String, crate::ext::SettingValue)>,
    /// What `term.http` and blocklists may reach besides its forges'
    /// accounts: the manifest's `hosts` and the values of its `hosts`
    /// settings. `None` allows every host, for extensions loaded from
    /// source text.
    pub(crate) hosts: Option<Vec<String>>,
}

/// State shared by the host and every extension's natives. The host never
/// holds a borrow while Lua runs.
#[derive(Default)]
pub(crate) struct Shared {
    pub(crate) requests: Vec<Request>,
    pub(crate) context: Context,
    pub(crate) commands: Vec<CommandInfo>,
    /// Parallel to `commands`: the owning slot and the `run` function's ref.
    pub(crate) command_targets: Vec<(usize, Ref)>,
    pub(crate) commands_dirty: bool,
    pub(crate) tasks: Vec<TaskInfo>,
    /// Parallel to `tasks`: the owning slot and the `run` function's ref.
    pub(crate) task_targets: Vec<(usize, Ref)>,
    pub(crate) tasks_dirty: bool,
    /// Set when a slot's sandbox rules change.
    pub(crate) sandbox_dirty: bool,
    pub(crate) slots: Vec<Slot>,
    /// `done` functions of requests still running: callback id, slot, ref.
    pub(crate) callbacks: Vec<(u64, usize, Ref)>,
    pub(crate) views: Vec<ViewInfo>,
    /// Parallel to `views`: the owning slot and the `render` function's ref.
    pub(crate) view_targets: Vec<(usize, Ref)>,
    pub(crate) tools: Vec<ToolInfo>,
    /// Parallel to `tools`: the owning slot and the `run` function's ref.
    pub(crate) tool_targets: Vec<(usize, Ref)>,
    pub(crate) tools_dirty: bool,
    pub(crate) forges: Vec<ForgeInfo>,
    /// Parallel to `forges`: the owning slot and its functions' refs.
    pub(crate) forge_targets: Vec<(usize, ForgeRefs)>,
    pub(crate) forges_dirty: bool,
    /// The user's accounts per forge id, from the Accounts page. A forge
    /// without an entry has the accounts its `term.forge` call names.
    pub(crate) forge_hosts: BTreeMap<String, Vec<String>>,
    /// What `term.environment` registered.
    pub(crate) environments: Vec<cmdterm_env::CustomProvider>,
    /// Parallel to `environments`: the owning slot.
    pub(crate) environment_targets: Vec<(usize, ())>,
    pub(crate) environments_dirty: bool,
    /// What `term.agent` registered.
    pub(crate) agents: Vec<AgentInfo>,
    /// Parallel to `agents`: the owning slot.
    pub(crate) agent_targets: Vec<(usize, ())>,
    pub(crate) agents_dirty: bool,
}

/// The functions of a `term.forge` registration.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ForgeRefs {
    pub(crate) check: Ref,
    pub(crate) token_url: Option<Ref>,
    pub(crate) whoami: Option<Ref>,
    pub(crate) discussion: Option<Ref>,
}

/// Removes the entries of `list` whose target in `targets` belongs to
/// `slot`. True when it removed any.
fn remove_slot<T, U>(list: &mut Vec<T>, targets: &mut Vec<(usize, U)>, slot: usize) -> bool {
    let before = list.len();
    let mut i = 0;
    while i < list.len() {
        if targets[i].0 == slot {
            list.remove(i);
            targets.remove(i);
        } else {
            i += 1;
        }
    }
    list.len() != before
}

/// Puts `info` in place of the entry `same` finds, or after the others.
fn upsert<T, U>(
    list: &mut Vec<T>,
    targets: &mut Vec<(usize, U)>,
    info: T,
    target: (usize, U),
    same: impl Fn(&T) -> bool,
) {
    if let Some(index) = list.iter().position(same) {
        list[index] = info;
        targets[index] = target;
    } else {
        list.push(info);
        targets.push(target);
    }
}

impl Shared {
    /// Forgets everything slot `index` registered.
    /// Whether extension `slot` may reach `host`: one of its hosts, or an
    /// account of one of its forges or a subdomain of it.
    pub(crate) fn allows(&self, slot: usize, host: &str) -> bool {
        let Some(own) = self.slots.get(slot) else {
            return false;
        };
        let Some(hosts) = &own.hosts else {
            return true;
        };
        hosts
            .iter()
            .any(|pattern| crate::ext::host_matches(pattern, host))
            || (self.forges.iter().zip(&self.forge_targets))
                .filter(|(_, target)| target.0 == slot)
                .flat_map(|(forge, _)| &forge.hosts)
                .any(|account| crate::ext::under_account(account, host))
    }

    pub(crate) fn clear_slot(&mut self, index: usize) {
        if let Some(slot) = self.slots.get_mut(index) {
            if !slot.sandbox.is_empty() {
                self.sandbox_dirty = true;
            }
            *slot = Slot::default();
        }
        self.callbacks.retain(|callback| callback.1 != index);
        let before = self.commands.len();
        let mut i = 0;
        while i < self.commands.len() {
            if self.command_targets[i].0 == index {
                self.commands.remove(i);
                self.command_targets.remove(i);
            } else {
                i += 1;
            }
        }
        if self.commands.len() != before {
            self.commands_dirty = true;
        }
        let before = self.tasks.len();
        let mut i = 0;
        while i < self.tasks.len() {
            if self.task_targets[i].0 == index {
                self.tasks.remove(i);
                self.task_targets.remove(i);
            } else {
                i += 1;
            }
        }
        if self.tasks.len() != before {
            self.tasks_dirty = true;
        }
        remove_slot(&mut self.views, &mut self.view_targets, index);
        self.tools_dirty |= remove_slot(&mut self.tools, &mut self.tool_targets, index);
        self.forges_dirty |= remove_slot(&mut self.forges, &mut self.forge_targets, index);
        self.environments_dirty |=
            remove_slot(&mut self.environments, &mut self.environment_targets, index);
        self.agents_dirty |= remove_slot(&mut self.agents, &mut self.agent_targets, index);
    }
}

/// What the natives of one VM need to record registrations.
#[derive(Clone)]
struct Binding {
    shared: Rc<RefCell<Shared>>,
    slot: usize,
    extension: Rc<str>,
    /// Holds registered functions; rooted in the registry.
    refs: TableHandle,
    permissions: Permissions,
    store: StoreHandle,
    /// Where project stores go, as `<dir>/<project id>/<extension>`.
    project_data_dir: Option<PathBuf>,
}

impl Binding {
    /// The id of the extension's `name`: `<extension>.<name>`, unless
    /// `name` names another extension's already.
    fn qualify(&self, name: &str) -> String {
        if name.contains('.') {
            name.to_owned()
        } else {
            format!("{}.{name}", self.extension)
        }
    }

    /// An error unless the extension holds `permission`.
    fn require(&self, permission: Permission, function: &str) -> RuntimeResult<()> {
        if self.permissions.contains(permission) {
            Ok(())
        } else {
            Err(RuntimeError::message(format!(
                "term.{function} needs the '{permission}' permission in manifest.toml"
            )))
        }
    }

    /// Stores `function` in the refs table and returns its key.
    fn store(&self, runtime: &mut Runtime, function: Value) -> RuntimeResult<Ref> {
        let key = {
            let mut shared = self.shared.borrow_mut();
            let slot = &mut shared.slots[self.slot];
            slot.next_ref += 1;
            slot.next_ref
        };
        runtime.table_set(self.refs, &Value::Integer(key), function)?;
        Ok(key)
    }

    /// Reads an optional `done` function from `opts` and keeps it until
    /// [`crate::ext::ExtensionHost::complete`] calls it.
    fn done(
        &self,
        runtime: &mut Runtime,
        opts: TableHandle,
        function: &str,
    ) -> RuntimeResult<Option<Callback>> {
        let done = runtime.table_get(opts, &Value::from("done"))?;
        if matches!(done, Value::Nil) {
            return Ok(None);
        }
        if !is_callable(&done) {
            return Err(RuntimeError::message(format!(
                "term.{function}: 'done' must be a function"
            )));
        }
        let key = self.store(runtime, done)?;
        let id = NEXT_CALLBACK.fetch_add(1, Ordering::Relaxed);
        self.shared
            .borrow_mut()
            .callbacks
            .push((id, self.slot, key));
        Ok(Some(Callback(id)))
    }
}

/// Callback ids, unique across hosts.
static NEXT_CALLBACK: AtomicU64 = AtomicU64::new(1);

/// Fetches a function stored with [`Binding::store`].
pub(crate) fn stored_function(runtime: &Runtime, refs: TableHandle, key: Ref) -> Value {
    runtime
        .table_get(refs, &Value::Integer(key))
        .unwrap_or(Value::Nil)
}

const REFS_KEY: &str = "term.functions";

/// The global extensions call the API through.
pub const GLOBAL: &str = "term";

/// Installs the `term` global, and `cmdterm` as an alias, into a fresh VM,
/// with the directories of `config`. Returns the refs table.
pub(crate) fn install(
    runtime: &mut Runtime,
    shared: &Rc<RefCell<Shared>>,
    slot: usize,
    extension: &str,
    permissions: Permissions,
    config: &crate::ext::Config,
) -> RuntimeResult<TableHandle> {
    let cache_dir = config.cache_dir.as_ref().map(|dir| dir.join(extension));
    let data_dir = config.data_dir.as_ref().map(|dir| dir.join(extension));
    let project_data_dir = config.project_data_dir.clone();
    let refs = runtime.create_table()?;
    let registry = runtime.registry();
    runtime.table_set(registry, &Value::from(REFS_KEY), Value::Table(refs))?;

    let binding = Binding {
        shared: Rc::clone(shared),
        slot,
        extension: Rc::from(extension),
        refs,
        permissions,
        store: store::open(data_dir),
        project_data_dir,
    };
    let table = runtime.create_table()?;
    runtime.table_set(table, &Value::from("version"), Value::from(VERSION))?;
    runtime.table_set(table, &Value::from("os"), Value::from(std::env::consts::OS))?;

    add(runtime, table, "cache_dir", move |_, _| {
        let Some(dir) = &cache_dir else {
            return Err(RuntimeError::message(
                "term.cache_dir: this app gives extensions no cache directory",
            ));
        };
        std::fs::create_dir_all(dir).map_err(|error| {
            RuntimeError::message(format!(
                "term.cache_dir: cannot create {}: {error}",
                dir.display()
            ))
        })?;
        Ok(vec![path_value(dir)])
    })?;

    let b = binding.clone();
    add(runtime, table, "on", move |runtime, args| {
        let name = arg_string(args, 0)?;
        let Some(kind) = EVENT_NAMES.iter().position(|known| *known == name) else {
            return Err(RuntimeError::argument_error(
                1,
                format!(
                    "unknown event '{name}' (expected one of {})",
                    EVENT_NAMES.join(", ")
                ),
            ));
        };
        let function = arg_function(args, 1)?;
        let key = b.store(runtime, function)?;
        b.shared.borrow_mut().slots[b.slot].handlers[kind].push(key);
        Ok(Vec::new())
    })?;

    let b = binding.clone();
    add(runtime, table, "command", move |runtime, args| {
        let spec = arg_table(args, 0)?;
        let name = field_string(runtime, spec, "name")?
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                RuntimeError::message("term.command: 'name' must be a non-empty string")
            })?;
        let title = field_string(runtime, spec, "title")?.unwrap_or_else(|| name.clone());
        let run = runtime.table_get(spec, &Value::from("run"))?;
        if !is_callable(&run) {
            return Err(RuntimeError::message(
                "term.command: 'run' must be a function",
            ));
        }
        let keys = key_requests(runtime, spec)?;
        {
            let shared = b.shared.borrow();
            if let Some(index) = shared.commands.iter().position(|c| c.name == name)
                && shared.command_targets[index].0 != b.slot
            {
                return Err(RuntimeError::message(format!(
                    "term.command: '{name}' is already registered by extension '{}'",
                    shared.commands[index].extension
                )));
            }
        }
        let key = b.store(runtime, run)?;
        let mut shared = b.shared.borrow_mut();
        let info = CommandInfo {
            name,
            title,
            extension: b.extension.to_string(),
            keys,
        };
        if let Some(index) = shared.commands.iter().position(|c| c.name == info.name) {
            shared.commands[index] = info;
            shared.command_targets[index] = (b.slot, key);
        } else {
            shared.commands.push(info);
            shared.command_targets.push((b.slot, key));
        }
        shared.commands_dirty = true;
        Ok(Vec::new())
    })?;

    let b = binding.clone();
    add(runtime, table, "task", move |runtime, args| {
        let spec = arg_table(args, 0)?;
        let name = field_string(runtime, spec, "name")?
            .filter(|name| {
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            })
            .ok_or_else(|| {
                RuntimeError::message("term.task: 'name' must be letters, digits, '-' or '_'")
            })?;
        let title = field_string(runtime, spec, "title")?.unwrap_or_else(|| name.clone());
        let every = task_interval(&runtime.table_get(spec, &Value::from("every"))?)
            .ok_or_else(|| {
                RuntimeError::message(format!(
                    "term.task: 'every' must be seconds, or a number with s, m, h, d or w such as \"1d\", of at least {MIN_TASK_INTERVAL} seconds"
                ))
            })?;
        let projects = match runtime.table_get(spec, &Value::from("projects"))? {
            Value::Nil => None,
            Value::String(single) => Some(vec![lossy(&single)]),
            Value::Table(list) => Some(string_list(runtime, list, "projects")?),
            _ => {
                return Err(RuntimeError::message(
                    "term.task: 'projects' must be a list of project names or root paths",
                ));
            }
        };
        if projects
            .as_ref()
            .is_some_and(|list| list.is_empty() || list.iter().any(|pattern| pattern.is_empty()))
        {
            return Err(RuntimeError::message(
                "term.task: 'projects' must not be empty; leave it out for a task without a project",
            ));
        }
        let run = runtime.table_get(spec, &Value::from("run"))?;
        if !is_callable(&run) {
            return Err(RuntimeError::message("term.task: 'run' must be a function"));
        }
        let key = b.store(runtime, run)?;
        let info = TaskInfo {
            id: format!("{}.{name}", b.extension),
            title,
            extension: b.extension.to_string(),
            every,
            projects,
        };
        let mut shared = b.shared.borrow_mut();
        if let Some(index) = shared.tasks.iter().position(|t| t.id == info.id) {
            shared.tasks[index] = info;
            shared.task_targets[index] = (b.slot, key);
        } else {
            shared.tasks.push(info);
            shared.task_targets.push((b.slot, key));
        }
        shared.tasks_dirty = true;
        Ok(Vec::new())
    })?;

    let b = binding.clone();
    add(runtime, table, "open_terminal", move |runtime, args| {
        b.require(Permission::Terminal, "open_terminal")?;
        let (cwd, command, title) = match args.first() {
            None | Some(Value::Nil) => (None, None, None),
            Some(Value::Table(opts)) => {
                let opts = *opts;
                let cwd = field_string(runtime, opts, "cwd")?.map(PathBuf::from);
                let title = field_string(runtime, opts, "title")?;
                let command = match runtime.table_get(opts, &Value::from("cmd"))? {
                    Value::Nil => None,
                    Value::Table(argv) => Some(string_list(runtime, argv, "cmd")?),
                    Value::String(single) => Some(vec![lossy(&single)]),
                    _ => {
                        return Err(RuntimeError::message(
                            "term.open_terminal: 'cmd' must be a list of strings",
                        ));
                    }
                };
                (cwd, command, title)
            }
            Some(other) => return Err(RuntimeError::bad_argument(1, "table", Some(other))),
        };
        b.shared.borrow_mut().requests.push(Request::OpenTerminal {
            cwd,
            command,
            title,
        });
        Ok(Vec::new())
    })?;

    let b = binding.clone();
    add(runtime, table, "open_browser", move |_, args| {
        b.require(Permission::Browser, "open_browser")?;
        let url = arg_string(args, 0)?;
        b.shared
            .borrow_mut()
            .requests
            .push(Request::OpenBrowser { url });
        Ok(Vec::new())
    })?;

    let b = binding.clone();
    add(runtime, table, "notify", move |_, args| {
        let message = arg_string(args, 0)?;
        let level = match args.get(1) {
            None | Some(Value::Nil) => NotifyLevel::Info,
            Some(Value::String(level)) => match level.as_bytes() {
                b"info" => NotifyLevel::Info,
                b"warning" | b"warn" => NotifyLevel::Warning,
                b"error" => NotifyLevel::Error,
                _ => {
                    return Err(RuntimeError::argument_error(
                        2,
                        "expected 'info', 'warning' or 'error'",
                    ));
                }
            },
            Some(other) => return Err(RuntimeError::bad_argument(2, "string", Some(other))),
        };
        b.shared
            .borrow_mut()
            .requests
            .push(Request::Notify { level, message });
        Ok(Vec::new())
    })?;

    let b = binding.clone();
    add(runtime, table, "send_text", move |_, args| {
        b.require(Permission::Terminal, "send_text")?;
        let text = arg_string(args, 0)?;
        let terminal = match args.get(1) {
            None | Some(Value::Nil) => None,
            Some(value) => Some(
                as_u64(value)
                    .ok_or_else(|| RuntimeError::bad_argument(2, "terminal id", Some(value)))?,
            ),
        };
        b.shared
            .borrow_mut()
            .requests
            .push(Request::SendText { terminal, text });
        Ok(Vec::new())
    })?;

    let b = binding.clone();
    add(runtime, table, "switch_worktree", move |_, args| {
        b.require(Permission::Worktree, "switch_worktree")?;
        let path = PathBuf::from(arg_string(args, 0)?);
        b.shared
            .borrow_mut()
            .requests
            .push(Request::SwitchWorktree { path });
        Ok(Vec::new())
    })?;

    let b = binding.clone();
    add(runtime, table, "copy", move |runtime, args| {
        b.require(Permission::Copy, "copy")?;
        let Some(Value::Table(opts)) = args.first() else {
            return Err(RuntimeError::bad_argument(1, "table", args.first()));
        };
        let opts = *opts;
        let from = field_string(runtime, opts, "from")?
            .ok_or_else(|| RuntimeError::message("term.copy: 'from' is required"))?;
        let to = field_string(runtime, opts, "to")?
            .ok_or_else(|| RuntimeError::message("term.copy: 'to' is required"))?;
        let worktree = field_string(runtime, opts, "worktree")?.map(PathBuf::from);
        let exclude = match runtime.table_get(opts, &Value::from("exclude"))? {
            Value::Nil => Vec::new(),
            Value::Table(list) => string_list(runtime, list, "exclude")?,
            _ => {
                return Err(RuntimeError::message(
                    "term.copy: 'exclude' must be a list of strings",
                ));
            }
        };
        let done = b.done(runtime, opts, "copy")?;
        b.shared.borrow_mut().requests.push(Request::Copy {
            from: PathBuf::from(from),
            to,
            worktree,
            exclude,
            done,
        });
        Ok(Vec::new())
    })?;

    let b = binding.clone();
    add(runtime, table, "run", move |runtime, args| {
        b.require(Permission::Run, "run")?;
        let Some(Value::Table(opts)) = args.first() else {
            return Err(RuntimeError::bad_argument(1, "table", args.first()));
        };
        let opts = *opts;
        let command = match runtime.table_get(opts, &Value::from("cmd"))? {
            Value::Table(argv) => string_list(runtime, argv, "cmd")?,
            Value::String(single) => vec![lossy(&single)],
            _ => {
                return Err(RuntimeError::message(
                    "term.run: 'cmd' must be a list of strings",
                ));
            }
        };
        if command.is_empty() {
            return Err(RuntimeError::message("term.run: 'cmd' is empty"));
        }
        let worktree = field_string(runtime, opts, "worktree")?.map(PathBuf::from);
        let cwd = field_string(runtime, opts, "cwd")?;
        let on_host = match runtime.table_get(opts, &Value::from("host"))? {
            Value::Nil | Value::Boolean(false) => false,
            Value::Boolean(true) => true,
            _ => return Err(RuntimeError::message("term.run: 'host' must be a boolean")),
        };
        if on_host {
            b.require(Permission::Process, "run{ host = true }")?;
        }
        let done = b.done(runtime, opts, "run")?;
        b.shared.borrow_mut().requests.push(Request::Run {
            command,
            worktree,
            cwd,
            on_host,
            done,
        });
        Ok(Vec::new())
    })?;

    let b = binding.clone();
    add(runtime, table, "trust", move |runtime, args| {
        b.require(Permission::Trust, "trust")?;
        let opts = arg_table(args, 0)?;
        let name = field_string(runtime, opts, "name")?
            .filter(|name| valid_certificate_name(name))
            .ok_or_else(|| {
                RuntimeError::message("term.trust: 'name' must be letters, digits, '-', '_' or '.'")
            })?;
        let path = field_string(runtime, opts, "path")?
            .ok_or_else(|| RuntimeError::message("term.trust: 'path' is required"))?;
        let worktree = field_string(runtime, opts, "worktree")?.map(PathBuf::from);
        let done = b.done(runtime, opts, "trust")?;
        b.shared.borrow_mut().requests.push(Request::Trust {
            name,
            path: PathBuf::from(path),
            worktree,
            done,
        });
        Ok(Vec::new())
    })?;

    let b = binding.clone();
    add(runtime, table, "sandbox", move |runtime, args| {
        b.require(Permission::Sandbox, "sandbox")?;
        let spec = arg_table(args, 0)?;
        let list = |runtime: &mut Runtime, key: &str| -> RuntimeResult<Vec<String>> {
            match runtime.table_get(spec, &Value::from(key))? {
                Value::Nil => Ok(Vec::new()),
                Value::Table(list) => string_list(runtime, list, key),
                _ => Err(RuntimeError::message(format!(
                    "term.sandbox: '{key}' must be a list of strings"
                ))),
            }
        };
        let hosts = list(runtime, "hosts")?;
        if let Some(bad) = hosts
            .iter()
            .find(|host| !cmdterm_env::valid_host_pattern(host))
        {
            return Err(RuntimeError::message(format!(
                "term.sandbox: '{bad}' is not a host name or *.domain"
            )));
        }
        let writable = list(runtime, "writable")?;
        let hidden = list(runtime, "hidden")?;
        let blocked = list(runtime, "blocked")?;
        let unblocked = list(runtime, "unblocked")?;
        if let Some(bad) = blocked
            .iter()
            .chain(&unblocked)
            .find(|host| !cmdterm_env::valid_host_pattern(host))
        {
            return Err(RuntimeError::message(format!(
                "term.sandbox: '{bad}' is not a host name or *.domain"
            )));
        }
        let mut redirects = Vec::new();
        match runtime.table_get(spec, &Value::from("redirects"))? {
            Value::Nil => {}
            Value::Table(entries) => {
                for index in 1_i64.. {
                    let entry = match runtime.table_get(entries, &Value::Integer(index))? {
                        Value::Nil => break,
                        Value::Table(entry) => entry,
                        _ => {
                            return Err(RuntimeError::message(
                                "term.sandbox: 'redirects' must be a list of { host = ..., sandbox = ... }",
                            ));
                        }
                    };
                    let host = field_string(runtime, entry, "host")?;
                    let sandbox = field_string(runtime, entry, "sandbox")?;
                    let (Some(host), Some(sandbox)) = (host, sandbox) else {
                        return Err(RuntimeError::message(
                            "term.sandbox: every redirect needs 'host' and 'sandbox'",
                        ));
                    };
                    redirects.push((host, sandbox));
                }
            }
            _ => {
                return Err(RuntimeError::message(
                    "term.sandbox: 'redirects' must be a list of { host = ..., sandbox = ... }",
                ));
            }
        }
        let vars = sandbox_vars(runtime, spec)?;
        let files = sandbox_files(runtime, spec)?;
        let guards = sandbox_guards(runtime, spec, &b)?;
        let blocklists = sandbox_blocklists(runtime, spec, &b)?;
        let mut shared = b.shared.borrow_mut();
        let slot = &mut shared.slots[b.slot];
        let rules = SandboxRules {
            hosts,
            writable,
            hidden,
            redirects,
            vars,
            files,
            // Secrets come from `term.secret` and stay.
            secrets: slot.sandbox.secrets.clone(),
            guards,
            blocked,
            unblocked,
            blocklists,
        };
        if slot.sandbox != rules {
            slot.sandbox = rules;
            shared.sandbox_dirty = true;
        }
        Ok(Vec::new())
    })?;

    let b = binding.clone();
    add(runtime, table, "secret", move |runtime, args| {
        b.require(Permission::Secrets, "secret")?;
        let spec = arg_table(args, 0)?;
        let value = field_string(runtime, spec, "value")?
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                RuntimeError::message("term.secret: 'value' must be a non-empty string")
            })?;
        let hosts = match runtime.table_get(spec, &Value::from("hosts"))? {
            Value::Table(list) => string_list(runtime, list, "hosts")?,
            _ => Vec::new(),
        };
        if hosts.is_empty() {
            return Err(RuntimeError::message(
                "term.secret: 'hosts' must list the hosts that get the value",
            ));
        }
        if let Some(bad) = hosts
            .iter()
            .find(|host| !cmdterm_env::valid_host_pattern(host))
        {
            return Err(RuntimeError::message(format!(
                "term.secret: '{bad}' is not a host name or *.domain"
            )));
        }
        // A named secret keeps its fake when its value changes; names are
        // per extension.
        let fake = match field_string(runtime, spec, "name")? {
            Some(name) if !name.is_empty() => {
                cmdterm_env::named_fake_secret(&format!("{}:{name}", b.extension), &value)
            }
            Some(_) => {
                return Err(RuntimeError::message(
                    "term.secret: 'name' must be a non-empty string",
                ));
            }
            None => cmdterm_env::fake_secret(&value),
        };
        let mut shared = b.shared.borrow_mut();
        let secrets = &mut shared.slots[b.slot].sandbox.secrets;
        let changed = match secrets.iter_mut().find(|s| s.fake == fake) {
            Some(known) => {
                let before = known.hosts.len();
                for host in hosts {
                    if !known.hosts.contains(&host) {
                        known.hosts.push(host);
                    }
                }
                let renewed = known.real != value;
                known.real = value;
                renewed || known.hosts.len() != before
            }
            None => {
                secrets.push(cmdterm_env::SecretRule {
                    fake: fake.clone(),
                    real: value,
                    hosts,
                });
                true
            }
        };
        if changed {
            shared.sandbox_dirty = true;
        }
        Ok(vec![Value::from(fake.as_str())])
    })?;

    let b = binding.clone();
    add(runtime, table, "settings", move |runtime, _| {
        let table = runtime.create_table()?;
        let settings = b.shared.borrow().slots[b.slot].settings.clone();
        for (key, value) in settings {
            let value = match value {
                crate::ext::SettingValue::Bool(on) => Value::Boolean(on),
                crate::ext::SettingValue::Integer(n) => Value::Integer(n),
                crate::ext::SettingValue::String(text) => Value::from(text.as_str()),
                crate::ext::SettingValue::List(items) => {
                    let list = runtime.create_table()?;
                    for (index, item) in (1_i64..).zip(items) {
                        runtime.table_set(
                            list,
                            &Value::Integer(index),
                            Value::from(item.as_str()),
                        )?;
                    }
                    Value::Table(list)
                }
            };
            runtime.table_set(table, &Value::from(key.as_str()), value)?;
        }
        Ok(vec![Value::Table(table)])
    })?;

    install_ui(runtime, table, &binding)?;

    let b = binding;
    add(runtime, table, "context", move |runtime, _| {
        let table = runtime.create_table()?;
        let shared = b.shared.borrow();
        let context = &shared.context;
        set_opt_str(runtime, table, "project", context.project.as_deref())?;
        set_opt_str(runtime, table, "project_id", context.project_id.as_deref())?;
        set_opt_path(runtime, table, "worktree", context.worktree.as_deref())?;
        set_opt_str(runtime, table, "branch", context.branch.as_deref())?;
        set_opt_str(runtime, table, "env", Some(&context.env))?;
        set_opt_str(runtime, table, "runtime", Some(&context.runtime))?;
        Ok(vec![Value::Table(table)])
    })?;

    runtime.set_global(GLOBAL, Value::Table(table));
    // The name before the rename to scriptschnell-term, for older extensions.
    runtime.set_global("cmdterm", Value::Table(table));
    Ok(refs)
}

/// Whether `name` may name a view or tool: letters,
/// digits, `-` and `_`.
fn valid_id(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
}

fn required_name(runtime: &Runtime, spec: TableHandle, function: &str) -> RuntimeResult<String> {
    field_string(runtime, spec, "name")?
        .filter(|name| valid_id(name))
        .ok_or_else(|| {
            RuntimeError::message(format!(
                "term.{function}: 'name' must be 1 to 64 letters, digits, '-' or '_'"
            ))
        })
}

fn required_function(
    runtime: &Runtime,
    spec: TableHandle,
    key: &str,
    function: &str,
) -> RuntimeResult<Value> {
    let value = runtime.table_get(spec, &Value::from(key))?;
    if is_callable(&value) {
        Ok(value)
    } else {
        Err(RuntimeError::message(format!(
            "term.{function}: '{key}' must be a function"
        )))
    }
}

/// The table a tool's `run` function gets as its second argument.
pub(crate) fn caller_table(runtime: &mut Runtime, caller: &ToolCaller) -> RuntimeResult<Value> {
    let table = runtime.create_table()?;
    set_opt_str(runtime, table, "project", caller.project.as_deref())?;
    set_opt_str(runtime, table, "project_id", caller.project_id.as_deref())?;
    set_opt_path(runtime, table, "worktree", caller.worktree.as_deref())?;
    set_opt_str(runtime, table, "environment", Some(&caller.environment))?;
    set_opt_str(runtime, table, "agent", caller.agent.as_deref())?;
    Ok(Value::Table(table))
}

/// `term.store`, `term.json_encode`, `term.json_decode`, `term.view`,
/// `term.open_view`, `term.refresh_view` and `term.tool`.
fn install_ui(runtime: &mut Runtime, table: TableHandle, binding: &Binding) -> RuntimeResult<()> {
    let denied = (!binding.permissions.contains(Permission::Storage))
        .then(|| Rc::from("term.store needs the 'storage' permission in manifest.toml"));
    let data = store_table(runtime, &binding.store, "store", denied)?;
    runtime.table_set(table, &Value::from("store"), Value::Table(data))?;

    let b = binding.clone();
    add(runtime, table, "project_store", move |runtime, args| {
        b.require(Permission::ProjectStorage, "project_store")?;
        let id = match args.first() {
            None | Some(Value::Nil) => {
                b.shared
                    .borrow()
                    .context
                    .project_id
                    .clone()
                    .ok_or_else(|| {
                        RuntimeError::message(
                            "term.project_store: no project is active; pass a project_id",
                        )
                    })?
            }
            Some(_) => arg_string(args, 0)?,
        };
        if !valid_project_id(&id) {
            return Err(RuntimeError::message(format!(
                "term.project_store: '{id}' is not a project_id"
            )));
        }
        let dir = b
            .project_data_dir
            .as_ref()
            .map(|dir| dir.join(&id).join(&*b.extension));
        let data = store_table(runtime, &store::open(dir), "project_store", None)?;
        runtime.table_set(data, &Value::from("id"), Value::from(id.as_str()))?;
        Ok(vec![Value::Table(data)])
    })?;

    add(runtime, table, "json_encode", move |runtime, args| {
        let value = args.first().cloned().unwrap_or(Value::Nil);
        let json = crate::ext::json::to_json(runtime, &value)
            .map_err(|e| RuntimeError::message(format!("term.json_encode: {e}")))?;
        Ok(vec![Value::from(json.to_string().as_str())])
    })?;
    add(runtime, table, "json_decode", move |runtime, args| {
        let text = arg_string(args, 0)?;
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(json) => Ok(vec![crate::ext::json::from_json(runtime, &json)?]),
            Err(error) => Ok(vec![Value::Nil, Value::from(error.to_string().as_str())]),
        }
    })?;

    let b = binding.clone();
    add(runtime, table, "view", move |runtime, args| {
        let spec = arg_table(args, 0)?;
        let name = required_name(runtime, spec, "view")?;
        let render = required_function(runtime, spec, "render", "view")?;
        let key = b.store(runtime, render)?;
        let info = ViewInfo {
            id: format!("{}.{name}", b.extension),
            extension: b.extension.to_string(),
        };
        let mut shared = b.shared.borrow_mut();
        let shared = &mut *shared;
        let id = info.id.clone();
        upsert(
            &mut shared.views,
            &mut shared.view_targets,
            info,
            (b.slot, key),
            |v| v.id == id,
        );
        Ok(Vec::new())
    })?;

    let b = binding.clone();
    add(runtime, table, "open_view", move |_, args| {
        let view = b.qualify(&arg_string(args, 0)?);
        let key = match args.get(1) {
            None | Some(Value::Nil) => String::new(),
            Some(_) => arg_string(args, 1)?,
        };
        b.shared
            .borrow_mut()
            .requests
            .push(Request::OpenView { view, key });
        Ok(Vec::new())
    })?;

    let b = binding.clone();
    add(runtime, table, "refresh_view", move |_, args| {
        let view = b.qualify(&arg_string(args, 0)?);
        let key = match args.get(1) {
            None | Some(Value::Nil) => None,
            Some(_) => Some(arg_string(args, 1)?),
        };
        b.shared
            .borrow_mut()
            .requests
            .push(Request::RefreshView { view, key });
        Ok(Vec::new())
    })?;

    let b = binding.clone();
    add(runtime, table, "tool", move |runtime, args| {
        b.require(Permission::Agents, "tool")?;
        let spec = arg_table(args, 0)?;
        let name = required_name(runtime, spec, "tool")?;
        let description = field_string(runtime, spec, "description")?
            .filter(|d| !d.is_empty())
            .ok_or_else(|| RuntimeError::message("term.tool: 'description' is required"))?;
        let input_schema = match runtime.table_get(spec, &Value::from("input"))? {
            Value::Nil => serde_json::json!({ "type": "object", "properties": {} }),
            value @ Value::Table(_) => {
                let mut schema = crate::ext::json::to_json(runtime, &value)
                    .map_err(|e| RuntimeError::message(format!("term.tool: 'input': {e}")))?;
                if schema.as_array().is_some_and(Vec::is_empty) {
                    schema = serde_json::json!({});
                }
                let Some(object) = schema.as_object_mut() else {
                    return Err(RuntimeError::message(
                        "term.tool: 'input' must be a JSON schema table",
                    ));
                };
                object.entry("type").or_insert_with(|| "object".into());
                // An empty Lua table reads as an array; a schema wants an object.
                if object
                    .get("properties")
                    .is_none_or(|p| p.as_array().is_some_and(Vec::is_empty))
                {
                    object.insert("properties".into(), serde_json::json!({}));
                }
                schema
            }
            _ => {
                return Err(RuntimeError::message(
                    "term.tool: 'input' must be a JSON schema table",
                ));
            }
        };
        let run = required_function(runtime, spec, "run", "tool")?;
        {
            let shared = b.shared.borrow();
            if let Some(index) = shared.tools.iter().position(|t| t.name == name)
                && shared.tool_targets[index].0 != b.slot
            {
                return Err(RuntimeError::message(format!(
                    "term.tool: '{name}' is already registered by extension '{}'",
                    shared.tools[index].extension
                )));
            }
        }
        let key = b.store(runtime, run)?;
        let info = ToolInfo {
            name: name.clone(),
            description,
            input_schema,
            extension: b.extension.to_string(),
        };
        let mut shared = b.shared.borrow_mut();
        let shared = &mut *shared;
        upsert(
            &mut shared.tools,
            &mut shared.tool_targets,
            info,
            (b.slot, key),
            |t| t.name == name,
        );
        shared.tools_dirty = true;
        Ok(Vec::new())
    })?;
    install_forge(runtime, table, binding)?;
    install_environment(runtime, table, binding)?;
    install_agent(runtime, table, binding)
}

/// `term.environment`: a location scheme whose commands run through a
/// program of this machine.
fn install_environment(
    runtime: &mut Runtime,
    table: TableHandle,
    binding: &Binding,
) -> RuntimeResult<()> {
    let b = binding.clone();
    add(runtime, table, "environment", move |runtime, args| {
        b.require(Permission::Environments, "environment")?;
        let spec = arg_table(args, 0)?;
        let scheme = field_string(runtime, spec, "name")?
            .filter(|name| cmdterm_env::valid_scheme(name))
            .ok_or_else(|| {
                RuntimeError::message(
                    "term.environment: 'name' must be a lowercase letter, then up to 31 \
                     lowercase letters, digits or '-', and not a scheme the app parses itself",
                )
            })?;
        let title = field_string(runtime, spec, "title")?.unwrap_or_else(|| scheme.clone());
        let program = field_string(runtime, spec, "program")?
            .filter(|program| !program.trim().is_empty() && !program.contains('{'))
            .ok_or_else(|| {
                RuntimeError::message("term.environment: 'program' must name a program")
            })?;
        let template = |runtime: &Runtime, key: &str| -> RuntimeResult<Option<Vec<String>>> {
            let list = match runtime.table_get(spec, &Value::from(key))? {
                Value::Nil => return Ok(None),
                Value::Table(list) => string_list(runtime, list, key)?,
                _ => {
                    return Err(RuntimeError::message(format!(
                        "term.environment: '{key}' must be a list of strings"
                    )));
                }
            };
            if !cmdterm_env::valid_template(&list) {
                return Err(RuntimeError::message(format!(
                    "term.environment: '{key}' may hold {{script}} once, as a whole argument"
                )));
            }
            if !list
                .iter()
                .any(|a| a.contains(cmdterm_env::TARGET_PLACEHOLDER))
            {
                return Err(RuntimeError::message(format!(
                    "term.environment: '{key}' must hold {{target}}"
                )));
            }
            Ok(Some(list))
        };
        let Some(command_args) = template(runtime, "args")? else {
            return Err(RuntimeError::message(
                "term.environment: 'args' must be a list of strings",
            ));
        };
        let pty_args = template(runtime, "pty_args")?;
        let mut shared = b.shared.borrow_mut();
        let shared = &mut *shared;
        let taken = (shared.environments.iter())
            .find(|e| e.scheme == scheme && e.extension != *b.extension);
        if let Some(other) = taken {
            return Err(RuntimeError::message(format!(
                "term.environment: '{scheme}' is taken by the extension '{}'",
                other.extension
            )));
        }
        let info = cmdterm_env::CustomProvider {
            scheme: scheme.clone(),
            title,
            extension: b.extension.to_string(),
            program,
            args: command_args,
            pty_args,
        };
        upsert(
            &mut shared.environments,
            &mut shared.environment_targets,
            info,
            (b.slot, ()),
            |e| e.scheme == scheme,
        );
        shared.environments_dirty = true;
        Ok(Vec::new())
    })
}

/// `term.agent`: a coding agent the app integrates like its own.
fn install_agent(
    runtime: &mut Runtime,
    table: TableHandle,
    binding: &Binding,
) -> RuntimeResult<()> {
    let b = binding.clone();
    add(runtime, table, "agent", move |runtime, args| {
        b.require(Permission::Agents, "agent")?;
        let spec = arg_table(args, 0)?;
        let mut info = agent_spec(runtime, spec)?;
        if info.sandbox != AgentSandbox::default() {
            b.require(Permission::Sandbox, "agent{ sandbox }")?;
        }
        // Passing a variable into sandboxes hands them its value, as
        // `os.getenv` would hand it to the extension.
        if !info.sandbox.login_vars.is_empty() {
            b.require(Permission::Env, "agent{ sandbox.login_vars }")?;
        }
        info.extension = b.extension.to_string();
        let mut shared = b.shared.borrow_mut();
        let shared = &mut *shared;
        let name = info.name.clone();
        let taken = (shared.agents.iter()).find(|a| a.name == name && a.extension != *b.extension);
        if let Some(other) = taken {
            return Err(agent_error(format!(
                "'{name}' is taken by the extension '{}'",
                other.extension
            )));
        }
        upsert(
            &mut shared.agents,
            &mut shared.agent_targets,
            info,
            (b.slot, ()),
            |a| a.name == name,
        );
        shared.agents_dirty = true;
        Ok(Vec::new())
    })
}

fn agent_error(message: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::message(format!("term.agent: {message}"))
}

/// Reads a `term.agent` table. The extension is left empty.
fn agent_spec(runtime: &Runtime, spec: TableHandle) -> RuntimeResult<AgentInfo> {
    let name = field_string(runtime, spec, "name")?
        .filter(|name| valid_agent_name(name))
        .ok_or_else(|| {
            agent_error(
                "'name' must be a lowercase letter, then up to 31 lowercase letters, digits or '-'",
            )
        })?;
    let title = field_string(runtime, spec, "title")?.unwrap_or_else(|| name.clone());
    let program = field_string(runtime, spec, "program")?.unwrap_or_else(|| name.clone());
    if !valid_shell_word(&program) || program.starts_with('-') || program.contains('/') {
        return Err(agent_error(
            "'program' must be a program name: letters, digits and '_.+-', not starting with '-'",
        ));
    }
    let config_dir = agent_path(runtime, spec, "config_dir")?
        .ok_or_else(|| agent_error("'config_dir' is required"))?;
    let skills_dir = agent_path(runtime, spec, "skills_dir")?;
    let integration = match runtime.table_get(spec, &Value::from("integration"))? {
        Value::Nil => AgentIntegration::Native,
        Value::Table(table) => agent_integration(runtime, table)?,
        _ => return Err(agent_error("'integration' must be a table")),
    };
    let mcp = match runtime.table_get(spec, &Value::from("mcp"))? {
        Value::Nil => None,
        Value::Table(table) => {
            let file = agent_path(runtime, table, "file")?
                .ok_or_else(|| agent_error("'mcp.file' is required"))?;
            let key =
                field_string(runtime, table, "key")?.unwrap_or_else(|| "mcpServers".to_owned());
            if !valid_id(&key) {
                return Err(agent_error("'mcp.key' must be letters, digits, '-' or '_'"));
            }
            let style = match field_string(runtime, table, "style")?.as_deref() {
                None | Some("stdio") => McpStyle::Stdio,
                Some("command") => McpStyle::Command,
                Some("local") => McpStyle::Local,
                Some(other) => {
                    return Err(agent_error(format!(
                        "'mcp.style' must be stdio, command or local, not '{other}'"
                    )));
                }
            };
            Some(AgentMcp { file, key, style })
        }
        _ => return Err(agent_error("'mcp' must be a table")),
    };
    let resume = match runtime.table_get(spec, &Value::from("resume"))? {
        Value::Nil => None,
        Value::Table(list) => {
            let list = string_list(runtime, list, "resume")?;
            if list.len() > 8 || !list.iter().all(|arg| valid_shell_word(arg)) {
                return Err(agent_error(
                    "'resume' must be up to 8 arguments of letters, digits and '_.+-=:/'",
                ));
            }
            Some(list)
        }
        _ => return Err(agent_error("'resume' must be a list of strings")),
    };
    let session_at_start = !matches!(
        runtime.table_get(spec, &Value::from("session_at_start"))?,
        Value::Boolean(false)
    );
    let supports = match runtime.table_get(spec, &Value::from("supports"))? {
        Value::Nil => Vec::new(),
        Value::Table(list) => {
            let mut supports = Vec::new();
            for name in string_list(runtime, list, "supports")? {
                let capability = crate::agents::Capability::parse(&name).ok_or_else(|| {
                    agent_error(format!(
                        "'supports' names '{name}'; use {}",
                        crate::agents::Capability::ALL.map(|c| c.name()).join(", ")
                    ))
                })?;
                if !supports.contains(&capability) {
                    supports.push(capability);
                }
            }
            supports
        }
        _ => return Err(agent_error("'supports' must be a list of strings")),
    };
    let prompt = match runtime.table_get(spec, &Value::from("prompt"))? {
        Value::Nil => None,
        Value::Table(list) => {
            let list = string_list(runtime, list, "prompt")?;
            let placeholder = crate::agents::PROMPT_PLACEHOLDER;
            let fine = list.len() <= 8
                && list.iter().filter(|arg| *arg == placeholder).count() == 1
                && (list.iter()).all(|arg| arg == placeholder || valid_shell_word(arg));
            if !fine {
                return Err(agent_error(
                    "'prompt' must be up to 8 arguments of letters, digits and '_.+-=:/', \
                     one of them '{prompt}'",
                ));
            }
            Some(list)
        }
        _ => return Err(agent_error("'prompt' must be a list of strings")),
    };
    let sessions = match runtime.table_get(spec, &Value::from("sessions"))? {
        Value::Nil => None,
        Value::Table(table) => {
            let dir = agent_path(runtime, table, "dir")?
                .ok_or_else(|| agent_error("'sessions.dir' is required"))?;
            let by_name = match field_string(runtime, table, "by")?.as_deref() {
                None | Some("name") => true,
                Some("contents") => false,
                Some(other) => {
                    return Err(agent_error(format!(
                        "'sessions.by' must be name or contents, not '{other}'"
                    )));
                }
            };
            Some(crate::ext::AgentSessions { dir, by_name })
        }
        _ => return Err(agent_error("'sessions' must be a table")),
    };
    let sandbox = match runtime.table_get(spec, &Value::from("sandbox"))? {
        Value::Nil => AgentSandbox::default(),
        Value::Table(table) => agent_sandbox(runtime, table)?,
        _ => return Err(agent_error("'sandbox' must be a table")),
    };
    let npm = match runtime.table_get(spec, &Value::from("npm"))? {
        Value::Nil => Vec::new(),
        Value::Table(list) => string_list(runtime, list, "npm")?,
        _ => return Err(agent_error("'npm' must be a list of package names")),
    };
    if !npm.iter().all(|package| valid_npm_package(package)) {
        return Err(agent_error("'npm' must be a list of package names"));
    }
    let info = AgentInfo {
        name,
        extension: String::new(),
        title,
        program,
        config_dir,
        skills_dir,
        integration,
        mcp,
        resume,
        session_at_start,
        sessions,
        supports,
        prompt,
        sandbox,
        npm,
    };
    check_supports(&info).map_err(agent_error)?;
    Ok(info)
}

/// Whether an agent opted in to exactly what its fields and hooks provide:
/// every capability needs what makes it work, and every field or hook that
/// makes one work needs it in `supports`. The app relies on `supports`, so
/// a claim it cannot keep and a capability it does not know both fail.
fn check_supports(info: &AgentInfo) -> Result<(), String> {
    use crate::agents::Capability::{self, *};
    let has = |capability: Capability| info.supports.contains(&capability);
    let need = |capability: Capability, why: &str| {
        if has(capability) {
            Ok(())
        } else {
            Err(format!("{why} needs '{}' in 'supports'", capability.name()))
        }
    };
    let claim = |capability: Capability, ok: bool, what: &str| {
        if !has(capability) || ok {
            Ok(())
        } else {
            Err(format!(
                "'{}' in 'supports' needs {what}",
                capability.name()
            ))
        }
    };
    if info.resume.is_some() {
        need(Resume, "'resume'")?;
    }
    if info.prompt.is_some() {
        need(Prompt, "'prompt'")?;
    }
    if info.sessions.is_some() {
        need(Resume, "'sessions'")?;
    }
    if !info.session_at_start {
        need(Session, "'session_at_start = false'")?;
    }
    claim(Resume, info.resume.is_some(), "'resume'")?;
    claim(Resume, has(Session), "'session'")?;
    claim(Prompt, info.prompt.is_some(), "'prompt'")?;
    claim(Paste, has(Session), "'session'")?;
    claim(
        Paste,
        info.session_at_start,
        "a session named at start, not 'session_at_start = false'",
    )?;
    claim(Messages, has(State), "'state'")?;
    claim(Messages, has(Session), "'session'")?;
    // Hooks show what the agent reports; other integrations report on
    // their own, so their claims stand as made.
    let AgentIntegration::Hooks { hooks, .. } = &info.integration else {
        return Ok(());
    };
    let commands: Vec<&str> = (hooks.iter())
        .map(|hook| hook.args.split(' ').next().unwrap_or_default())
        .collect();
    let any = |names: &[&str]| commands.iter().any(|c| names.contains(c));
    for command in &commands {
        match *command {
            "context" | "session" => need(Session, &format!("the hook '{command}'"))?,
            "deliver" => need(Messages, "the hook 'deliver'")?,
            _ => need(State, &format!("the hook '{command}'"))?,
        }
    }
    claim(
        State,
        any(&["busy", "done", "stop"]),
        "a hook that reports busy, done or stop",
    )?;
    claim(
        Session,
        any(&["context", "session"]),
        "a 'context' or 'session' hook",
    )?;
    claim(Messages, any(&["context"]), "a 'context' hook")?;
    claim(Messages, any(&["deliver"]), "a 'deliver' hook")?;
    Ok(())
}

fn agent_integration(runtime: &Runtime, table: TableHandle) -> RuntimeResult<AgentIntegration> {
    let file = agent_path(runtime, table, "file")?
        .ok_or_else(|| agent_error("'integration.file' is required"))?;
    let content = field_string(runtime, table, "content")?;
    let hooks = runtime.table_get(table, &Value::from("hooks"))?;
    match (content, hooks) {
        (Some(content), Value::Nil) => Ok(AgentIntegration::File { file, content }),
        (None, Value::Table(list)) => {
            let mut hooks = Vec::new();
            for index in 1_i64.. {
                let entry = match runtime.table_get(list, &Value::Integer(index))? {
                    Value::Nil => break,
                    Value::Table(entry) => entry,
                    _ => return Err(agent_error("'integration.hooks' must be a list of tables")),
                };
                let event = field_string(runtime, entry, "event")?
                    .filter(|event| valid_id(event))
                    .ok_or_else(|| {
                        agent_error("a hook's 'event' must be letters, digits, '-' or '_'")
                    })?;
                let matcher = field_string(runtime, entry, "matcher")?;
                if matcher.as_ref().is_some_and(|m| {
                    m.is_empty() || m.len() > 256 || m.chars().any(char::is_control)
                }) {
                    return Err(agent_error(
                        "a hook's 'matcher' must be a short line of text",
                    ));
                }
                let args = field_string(runtime, entry, "args")?
                    .filter(|args| valid_hook_args(args))
                    .ok_or_else(|| {
                        agent_error(
                            "a hook's 'args' must start with an agent-state command (busy, done, \
                             input, permission, clear, context, deliver, stop, end, session) and \
                             hold only letters, digits, spaces, '_.-' and single-quoted text",
                        )
                    })?;
                hooks.push(AgentHook {
                    event,
                    matcher,
                    args,
                });
            }
            if hooks.is_empty() || hooks.len() > 64 {
                return Err(agent_error("'integration.hooks' must hold 1 to 64 hooks"));
            }
            let timeout_unit = match runtime.table_get(table, &Value::from("timeout_unit"))? {
                Value::Nil => 1,
                Value::Integer(1) => 1,
                Value::Integer(1000) => 1000,
                _ => return Err(agent_error("'integration.timeout_unit' must be 1 or 1000")),
            };
            Ok(AgentIntegration::Hooks {
                file,
                hooks,
                timeout_unit,
            })
        }
        _ => Err(agent_error(
            "'integration' needs either 'content' or 'hooks' next to 'file'",
        )),
    }
}

fn agent_sandbox(runtime: &Runtime, table: TableHandle) -> RuntimeResult<AgentSandbox> {
    let paths = |key: &str| -> RuntimeResult<Vec<String>> {
        match runtime.table_get(table, &Value::from(key))? {
            Value::Nil => Ok(Vec::new()),
            Value::Table(list) => (string_list(runtime, list, key)?.into_iter())
                .map(|path| {
                    home_relative(&path).ok_or_else(|| {
                        agent_error(format!(
                            "'sandbox.{key}' holds '{path}', which is not a path in the home directory"
                        ))
                    })
                })
                .collect(),
            _ => Err(agent_error(format!("'sandbox.{key}' must be a list of paths"))),
        }
    };
    let writable = paths("writable")?;
    let read_only = paths("read_only")?;
    let synced = paths("synced")?;
    let home_var = match runtime.table_get(table, &Value::from("home_var"))? {
        Value::Nil => None,
        Value::Table(pair) => {
            let pair = string_list(runtime, pair, "home_var")?;
            match pair.as_slice() {
                [var, dir]
                    if !var.is_empty()
                        && var.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                        && !var.starts_with(|c: char| c.is_ascii_digit()) =>
                {
                    let dir = home_relative(dir)
                        .ok_or_else(|| agent_error("'sandbox.home_var' names no home path"))?;
                    Some((var.clone(), dir))
                }
                _ => {
                    return Err(agent_error(
                        "'sandbox.home_var' must be { variable, directory }",
                    ));
                }
            }
        }
        _ => {
            return Err(agent_error(
                "'sandbox.home_var' must be { variable, directory }",
            ));
        }
    };
    let login_vars = match runtime.table_get(table, &Value::from("login_vars"))? {
        Value::Nil => Vec::new(),
        Value::Table(list) => string_list(runtime, list, "login_vars")?,
        _ => {
            return Err(agent_error(
                "'sandbox.login_vars' must be a list of variable names",
            ));
        }
    };
    let valid = |name: &String| {
        !name.is_empty()
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            && !name.starts_with(|c: char| c.is_ascii_digit())
    };
    if login_vars.len() > 32 || !login_vars.iter().all(valid) {
        return Err(agent_error(
            "'sandbox.login_vars' must be a list of up to 32 variable names",
        ));
    }
    let mut guarded = Vec::new();
    match runtime.table_get(table, &Value::from("guarded"))? {
        Value::Nil => {}
        Value::Table(map) => {
            for file in &synced {
                let key_paths = match runtime.table_get(map, &Value::from(file.as_str()))? {
                    Value::Nil => continue,
                    Value::Table(list) => {
                        let mut key_paths = Vec::new();
                        for index in 1_i64.. {
                            match runtime.table_get(list, &Value::Integer(index))? {
                                Value::Nil => break,
                                Value::Table(path) => {
                                    key_paths.push(string_list(runtime, path, "guarded")?);
                                }
                                _ => {
                                    return Err(agent_error(
                                        "'sandbox.guarded' maps a synced file to a list of key paths",
                                    ));
                                }
                            }
                        }
                        key_paths
                    }
                    _ => {
                        return Err(agent_error(
                            "'sandbox.guarded' maps a synced file to a list of key paths",
                        ));
                    }
                };
                guarded.push((file.clone(), key_paths));
            }
        }
        _ => return Err(agent_error("'sandbox.guarded' must be a table")),
    }
    Ok(AgentSandbox {
        writable,
        read_only,
        synced,
        home_var,
        login_vars,
        guarded,
    })
}

/// An optional path field relative to the home directory.
fn agent_path(runtime: &Runtime, table: TableHandle, key: &str) -> RuntimeResult<Option<String>> {
    match field_string(runtime, table, key)? {
        None => Ok(None),
        Some(path) => home_relative(&path).map(Some).ok_or_else(|| {
            agent_error(format!(
                "'{key}' must be a path in the home directory, such as '.my-agent'"
            ))
        }),
    }
}

/// `path` relative to the home directory, with a leading `~/` taken off:
/// no absolute path, no `..`, nothing a shell would expand.
pub(crate) fn home_relative(path: &str) -> Option<String> {
    let path = path
        .strip_prefix("~/")
        .unwrap_or(path)
        .trim_end_matches('/');
    let fine = !path.is_empty()
        && path.len() <= 256
        && !path.starts_with('/')
        && !path.starts_with('~')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/+@".contains(&b));
    fine.then(|| path.to_owned())
}

/// A lowercase letter, then up to 31 lowercase letters, digits and `-`.
pub(crate) fn valid_agent_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && (name.bytes()).all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A word the app may type into a shell without quoting.
fn valid_shell_word(word: &str) -> bool {
    (1..=128).contains(&word.len())
        && (word.bytes()).all(|b| b.is_ascii_alphanumeric() || b"_.+-=:/".contains(&b))
}

/// Arguments for the `agent-state` helper inside a hook's shell command:
/// a command it knows, then words and single-quoted text.
pub(crate) fn valid_hook_args(args: &str) -> bool {
    const STATES: [&str; 10] = [
        "busy",
        "done",
        "input",
        "permission",
        "clear",
        "context",
        "deliver",
        "stop",
        "end",
        "session",
    ];
    let first = args.split(' ').next().unwrap_or_default();
    args.len() <= 200
        && STATES.contains(&first)
        && args.bytes().filter(|&b| b == b'\'').count() % 2 == 0
        && (args.bytes()).all(|b| b.is_ascii_alphanumeric() || b" _.-'".contains(&b))
}

/// An npm package name, scoped or not.
fn valid_npm_package(name: &str) -> bool {
    let bare = name
        .strip_prefix('@')
        .and_then(|rest| rest.split_once('/'))
        .map_or(name, |(_, bare)| bare);
    (1..=214).contains(&name.len())
        && !bare.is_empty()
        && (name.bytes())
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"@/._-~".contains(&b))
}

/// Most headers one `term.http` request sends.
const MAX_HTTP_HEADERS: usize = 32;

/// `term.http`, `term.forge` and `term.forge_report`.
fn install_forge(
    runtime: &mut Runtime,
    table: TableHandle,
    binding: &Binding,
) -> RuntimeResult<()> {
    let b = binding.clone();
    add(runtime, table, "http", move |runtime, args| {
        b.require(Permission::Network, "http")?;
        let opts = arg_table(args, 0)?;
        let url = field_string(runtime, opts, "url")?
            .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
            .ok_or_else(|| {
                RuntimeError::message("term.http: 'url' must start with https:// or http://")
            })?;
        let host = crate::ext::url_host(&url).unwrap_or_default();
        if !b.shared.borrow().allows(b.slot, &host) {
            return Err(RuntimeError::message(format!(
                "term.http: '{host}' is not one of the extension's hosts; list it in \
                 'hosts' of manifest.toml"
            )));
        }
        let method = field_string(runtime, opts, "method")?
            .unwrap_or_else(|| "GET".to_owned())
            .to_ascii_uppercase();
        if !matches!(method.as_str(), "GET" | "POST" | "HEAD") {
            return Err(RuntimeError::message(
                "term.http: 'method' must be GET, POST or HEAD",
            ));
        }
        let mut headers = Vec::new();
        match runtime.table_get(opts, &Value::from("headers"))? {
            Value::Nil => {}
            Value::Table(map) => {
                headers = string_map(
                    runtime,
                    map,
                    "term.http: 'headers' must map names to strings",
                )?;
            }
            _ => {
                return Err(RuntimeError::message(
                    "term.http: 'headers' must map names to strings",
                ));
            }
        }
        if headers.len() > MAX_HTTP_HEADERS {
            return Err(RuntimeError::message(format!(
                "term.http: at most {MAX_HTTP_HEADERS} headers"
            )));
        }
        let body = field_string(runtime, opts, "body")?;
        let account = match field_string(runtime, opts, "account")? {
            None => None,
            Some(host) => {
                let forge = match field_string(runtime, opts, "forge")? {
                    Some(name) => b.qualify(&name),
                    None => {
                        let shared = b.shared.borrow();
                        let own = shared
                            .forges
                            .iter()
                            .zip(&shared.forge_targets)
                            .find(|(_, target)| target.0 == b.slot);
                        own.map(|(forge, _)| forge.id.clone()).ok_or_else(|| {
                            RuntimeError::message(
                                "term.http: 'account' needs a forge; register one with term.forge first",
                            )
                        })?
                    }
                };
                Some((forge, host))
            }
        };
        let done = b.done(runtime, opts, "http")?;
        b.shared
            .borrow_mut()
            .requests
            .push(Request::Http(HttpRequest {
                method,
                url,
                headers,
                body,
                account,
                done,
            }));
        Ok(Vec::new())
    })?;

    let b = binding.clone();
    add(runtime, table, "forge", move |runtime, args| {
        b.require(Permission::Network, "forge")?;
        let spec = arg_table(args, 0)?;
        let name = required_name(runtime, spec, "forge")?;
        let title = field_string(runtime, spec, "title")?.unwrap_or_else(|| name.clone());
        let list = |runtime: &Runtime, key: &str| -> RuntimeResult<Vec<String>> {
            match runtime.table_get(spec, &Value::from(key))? {
                Value::Nil => Ok(Vec::new()),
                Value::Table(list) => string_list(runtime, list, key),
                _ => Err(RuntimeError::message(format!(
                    "term.forge: '{key}' must be a list of strings"
                ))),
            }
        };
        for moved in ["hosts", "setting"] {
            if !matches!(runtime.table_get(spec, &Value::from(moved))?, Value::Nil) {
                return Err(RuntimeError::message(format!(
                    "term.forge: '{moved}' is gone; name the accounts in \
                     'accounts = {{ default = {{ ... }} }}', the Accounts page keeps the user's"
                )));
            }
        }
        let (mut default_hosts, mut example, mut hint) = (Vec::new(), None, None);
        match runtime.table_get(spec, &Value::from("accounts"))? {
            Value::Nil => {}
            Value::Table(accounts) => {
                let defaults = match runtime.table_get(accounts, &Value::from("default"))? {
                    Value::Nil => Vec::new(),
                    Value::Table(list) => string_list(runtime, list, "accounts.default")?,
                    _ => {
                        return Err(RuntimeError::message(
                            "term.forge: 'accounts.default' must be a list of strings",
                        ));
                    }
                };
                for entry in defaults {
                    if entry.trim().is_empty() {
                        continue;
                    }
                    let Some(account) = crate::ext::forge_account(&entry) else {
                        return Err(RuntimeError::message(format!(
                            "term.forge: '{}' is not a host name or host/path",
                            entry.trim()
                        )));
                    };
                    if !default_hosts.contains(&account) {
                        default_hosts.push(account);
                    }
                }
                example = field_string(runtime, accounts, "example")?.filter(|s| !s.is_empty());
                hint = field_string(runtime, accounts, "hint")?.filter(|s| !s.is_empty());
            }
            _ => {
                return Err(RuntimeError::message(
                    "term.forge: 'accounts' must be { default = { ... }, example = ..., hint = ... }",
                ));
            }
        }
        let mut aliases = Vec::new();
        match runtime.table_get(spec, &Value::from("aliases"))? {
            Value::Nil => {}
            Value::Table(map) => {
                let pairs = string_map(
                    runtime,
                    map,
                    "term.forge: 'aliases' must map host patterns to hosts",
                )?;
                for (pattern, location) in pairs {
                    let pattern = pattern.trim_end_matches('/').to_ascii_lowercase();
                    let location = location.trim_end_matches('/').to_ascii_lowercase();
                    let (host, path) = pattern.split_once('/').unwrap_or((&pattern, ""));
                    if !cmdterm_env::valid_host_pattern(host) || !crate::ext::valid_forge_path(path)
                    {
                        return Err(RuntimeError::message(format!(
                            "term.forge: '{pattern}' is not a host name or *.domain, with an optional path"
                        )));
                    }
                    let (target, path) = location.split_once('/').unwrap_or((&location, ""));
                    let wild = host.starts_with("*.");
                    if !cmdterm_env::valid_host_pattern(target)
                        || target.starts_with("*.")
                        || !crate::ext::valid_forge_path(&path.replace('*', "x"))
                        || (!wild && location.contains('*'))
                    {
                        return Err(RuntimeError::message(format!(
                            "term.forge: alias '{pattern}' must name a host or host/path, with * only for *.domain"
                        )));
                    }
                    aliases.push((pattern, location));
                }
                aliases.sort();
            }
            _ => {
                return Err(RuntimeError::message(
                    "term.forge: 'aliases' must map host patterns to hosts",
                ));
            }
        }
        let (auth_header, auth_value) = match runtime.table_get(spec, &Value::from("auth"))? {
            Value::Nil => ("Authorization".to_owned(), "Bearer {token}".to_owned()),
            Value::Table(auth) => (
                field_string(runtime, auth, "header")?
                    .unwrap_or_else(|| "Authorization".to_owned()),
                field_string(runtime, auth, "value")?
                    .unwrap_or_else(|| "Bearer {token}".to_owned()),
            ),
            _ => {
                return Err(RuntimeError::message(
                    "term.forge: 'auth' must be { header = ..., value = ... }",
                ));
            }
        };
        if !["{token}", "{basic}", "{base64}"]
            .iter()
            .any(|field| auth_value.contains(field))
        {
            return Err(RuntimeError::message(
                "term.forge: 'auth.value' must hold {token}, {basic} or {base64}",
            ));
        }
        let env = list(runtime, "env")?;
        let scopes = field_string(runtime, spec, "scopes")?.filter(|s| !s.is_empty());
        let check = required_function(runtime, spec, "check", "forge")?;
        let optional = |runtime: &Runtime, key: &str| -> RuntimeResult<Option<Value>> {
            match runtime.table_get(spec, &Value::from(key))? {
                Value::Nil => Ok(None),
                value if is_callable(&value) => Ok(Some(value)),
                _ => Err(RuntimeError::message(format!(
                    "term.forge: '{key}' must be a function"
                ))),
            }
        };
        // Without the `guards` permission a forge keeps working, without
        // its guards.
        let guards = if b.permissions.contains(Permission::Guards) {
            guard_list(runtime, spec, &b.extension, "term.forge", false)?
        } else {
            Vec::new()
        };
        let token_url = optional(runtime, "token_url")?;
        let whoami = optional(runtime, "whoami")?;
        let discussion = optional(runtime, "discussion")?;
        let refs = ForgeRefs {
            discussion: discussion.map(|f| b.store(runtime, f)).transpose()?,
            check: b.store(runtime, check)?,
            token_url: token_url.map(|f| b.store(runtime, f)).transpose()?,
            whoami: whoami.map(|f| b.store(runtime, f)).transpose()?,
        };
        let id = format!("{}.{name}", b.extension);
        let hosts = (b.shared.borrow().forge_hosts.get(&id).cloned())
            .unwrap_or_else(|| default_hosts.clone());
        let info = ForgeInfo {
            id: id.clone(),
            extension: b.extension.to_string(),
            title,
            hosts,
            default_hosts,
            example,
            hint,
            aliases,
            auth_header,
            auth_value,
            env,
            scopes,
            discussions: refs.discussion.is_some(),
            guards,
        };
        let mut shared = b.shared.borrow_mut();
        let shared = &mut *shared;
        upsert(
            &mut shared.forges,
            &mut shared.forge_targets,
            info,
            (b.slot, refs),
            |f| f.id == id,
        );
        shared.forges_dirty = true;
        Ok(Vec::new())
    })?;

    let b = binding.clone();
    add(runtime, table, "forge_discussion", move |runtime, args| {
        b.require(Permission::Network, "forge_discussion")?;
        let forge = b.qualify(&arg_string(args, 0)?);
        if !b.shared.borrow().forges.iter().any(|f| f.id == forge) {
            return Err(RuntimeError::message(format!(
                "term.forge_discussion: no forge '{forge}' is registered"
            )));
        }
        let id = arg_string(args, 1)?;
        let discussion = match args.get(2) {
            Some(Value::Table(table)) => Ok(discussion(runtime, *table)?),
            None | Some(Value::Nil) => Err(match args.get(3) {
                Some(Value::String(error)) => lossy(error),
                _ => "the forge sent no discussion".to_owned(),
            }),
            Some(other) => return Err(RuntimeError::bad_argument(3, "table", Some(other))),
        };
        b.shared
            .borrow_mut()
            .requests
            .push(Request::ForgeDiscussion {
                forge,
                id,
                discussion,
            });
        Ok(Vec::new())
    })?;

    let b = binding.clone();
    add(runtime, table, "forge_report", move |runtime, args| {
        b.require(Permission::Network, "forge_report")?;
        let forge = b.qualify(&arg_string(args, 0)?);
        if !b.shared.borrow().forges.iter().any(|f| f.id == forge) {
            return Err(RuntimeError::message(format!(
                "term.forge_report: no forge '{forge}' is registered"
            )));
        }
        let list = arg_table(args, 1)?;
        let mut results = Vec::new();
        for index in 1_i64.. {
            let item = match runtime.table_get(list, &Value::Integer(index))? {
                Value::Nil => break,
                Value::Table(item) => item,
                _ => {
                    return Err(RuntimeError::message(
                        "term.forge_report: every result must be a table",
                    ));
                }
            };
            let text = |key: &str| -> RuntimeResult<String> {
                field_string(runtime, item, key)?.ok_or_else(|| {
                    RuntimeError::message(format!("term.forge_report: every result needs '{key}'"))
                })
            };
            let state = match field_string(runtime, item, "state")?.as_deref() {
                None | Some("none") => BranchState::None,
                Some("open") => BranchState::Open,
                Some("merged") => BranchState::Merged,
                Some("closed") => BranchState::Closed,
                Some(other) => {
                    return Err(RuntimeError::message(format!(
                        "term.forge_report: state '{other}' is not open, merged, closed or none"
                    )));
                }
            };
            let number = match runtime.table_get(item, &Value::from("number"))? {
                Value::Nil => None,
                value => Some(as_u64(&value).ok_or_else(|| {
                    RuntimeError::message("term.forge_report: 'number' must be a whole number")
                })?),
            };
            results.push(BranchReport {
                account: text("account")?,
                path: text("path")?,
                branch: text("branch")?,
                state,
                number,
                url: field_string(runtime, item, "url")?,
                title: field_string(runtime, item, "title")?,
            });
        }
        let retry_after = match args.get(2) {
            Some(Value::Table(opts)) => {
                match runtime.table_get(*opts, &Value::from("retry_after"))? {
                    Value::Nil => None,
                    value => as_u64(&value),
                }
            }
            _ => None,
        };
        b.shared.borrow_mut().requests.push(Request::ForgeReport {
            forge,
            results,
            retry_after,
        });
        Ok(Vec::new())
    })?;
    Ok(())
}

const DISCUSSION: &str = "term.forge_discussion";

/// The items of list `key` of `table`, each read with `item`.
fn table_list<T>(
    runtime: &Runtime,
    table: TableHandle,
    key: &str,
    item: impl Fn(&Runtime, TableHandle) -> RuntimeResult<T>,
) -> RuntimeResult<Vec<T>> {
    let list = match runtime.table_get(table, &Value::from(key))? {
        Value::Nil => return Ok(Vec::new()),
        Value::Table(list) => list,
        _ => {
            return Err(RuntimeError::message(format!(
                "{DISCUSSION}: '{key}' must be a list of tables"
            )));
        }
    };
    let mut out = Vec::new();
    for index in 1_i64.. {
        match runtime.table_get(list, &Value::Integer(index))? {
            Value::Nil => break,
            Value::Table(entry) => out.push(item(runtime, entry)?),
            _ => {
                return Err(RuntimeError::message(format!(
                    "{DISCUSSION}: '{key}' must be a list of tables"
                )));
            }
        }
    }
    Ok(out)
}

fn flag(runtime: &Runtime, table: TableHandle, key: &str) -> RuntimeResult<bool> {
    Ok(matches!(
        runtime.table_get(table, &Value::from(key))?,
        Value::Boolean(true)
    ))
}

/// A string field, or a number written as one.
fn text_field(runtime: &Runtime, table: TableHandle, key: &str) -> RuntimeResult<Option<String>> {
    match runtime.table_get(table, &Value::from(key))? {
        Value::Integer(n) => Ok(Some(n.to_string())),
        _ => field_string(runtime, table, key),
    }
}

fn comment(runtime: &Runtime, table: TableHandle) -> RuntimeResult<Comment> {
    Ok(Comment {
        author: field_string(runtime, table, "author")?.unwrap_or_default(),
        body: field_string(runtime, table, "body")?.unwrap_or_default(),
        at: field_string(runtime, table, "at")?,
    })
}

fn discussion(runtime: &Runtime, table: TableHandle) -> RuntimeResult<Discussion> {
    let state = match field_string(runtime, table, "state")?.as_deref() {
        Some("open") => BranchState::Open,
        Some("merged") => BranchState::Merged,
        Some("closed") => BranchState::Closed,
        _ => BranchState::None,
    };
    let pushes = table_list(runtime, table, "pushes", |runtime, push| {
        Ok(Push {
            commit: field_string(runtime, push, "commit")?.ok_or_else(|| {
                RuntimeError::message(format!("{DISCUSSION}: every push needs 'commit'"))
            })?,
            at: field_string(runtime, push, "at")?,
            before: field_string(runtime, push, "before")?,
            force: flag(runtime, push, "force")?,
        })
    })?;
    let threads = table_list(runtime, table, "threads", |runtime, thread| {
        let line = match runtime.table_get(thread, &Value::from("line"))? {
            Value::Nil => None,
            value => Some(as_u64(&value).ok_or_else(|| {
                RuntimeError::message(format!("{DISCUSSION}: 'line' must be a whole number"))
            })?),
        };
        Ok(Thread {
            path: field_string(runtime, thread, "path")?.unwrap_or_default(),
            line,
            old_side: field_string(runtime, thread, "side")?.as_deref() == Some("old"),
            commit: field_string(runtime, thread, "commit")?,
            hunk: field_string(runtime, thread, "hunk")?,
            resolved: flag(runtime, thread, "resolved")?,
            outdated: flag(runtime, thread, "outdated")?,
            comments: table_list(runtime, thread, "comments", comment)?,
        })
    })?;
    let reviews = table_list(runtime, table, "reviews", |runtime, review| {
        let verdict = match field_string(runtime, review, "state")?.as_deref() {
            Some("approved") => Verdict::Approved,
            Some("changes") => Verdict::Changes,
            Some("commented") => Verdict::Commented,
            _ => Verdict::Pending,
        };
        Ok(Review {
            author: field_string(runtime, review, "author")?.unwrap_or_default(),
            verdict,
        })
    })?;
    let checks = table_list(runtime, table, "checks", |runtime, check| {
        let state = match field_string(runtime, check, "state")?.as_deref() {
            Some("success") => CheckState::Success,
            Some("failed") => CheckState::Failed,
            Some("running") => CheckState::Running,
            Some("skipped") => CheckState::Skipped,
            Some("canceled") => CheckState::Canceled,
            _ => CheckState::Pending,
        };
        Ok(Check {
            name: field_string(runtime, check, "name")?.unwrap_or_default(),
            state,
            url: field_string(runtime, check, "url")?,
        })
    })?;
    Ok(Discussion {
        title: text_field(runtime, table, "title")?.unwrap_or_default(),
        url: field_string(runtime, table, "url")?,
        state,
        author: field_string(runtime, table, "author")?,
        body: field_string(runtime, table, "body")?,
        draft: flag(runtime, table, "draft")?,
        target: field_string(runtime, table, "target")?,
        conflicts: flag(runtime, table, "conflicts")?,
        reviews,
        checks,
        pushes,
        comments: table_list(runtime, table, "comments", comment)?,
        threads,
    })
}

/// The table a forge's `discussion` function gets.
pub(crate) fn discussion_request_table(
    runtime: &mut Runtime,
    request: &DiscussionRequest,
) -> RuntimeResult<Value> {
    let table = runtime.create_table()?;
    for (key, value) in [
        ("id", &request.id),
        ("account", &request.account),
        ("host", &request.host),
        ("path", &request.path),
        ("remote", &request.remote),
        ("branch", &request.branch),
    ] {
        set_opt_str(runtime, table, key, Some(value))?;
    }
    let number = i64::try_from(request.number).unwrap_or(i64::MAX);
    runtime.table_set(table, &Value::from("number"), Value::Integer(number))?;
    runtime.table_set(
        table,
        &Value::from("signed_in"),
        Value::Boolean(request.signed_in),
    )?;
    Ok(Value::Table(table))
}

/// A table from strings to strings, as pairs.
fn string_map(
    runtime: &Runtime,
    table: TableHandle,
    message: &'static str,
) -> RuntimeResult<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    let mut key = Value::Nil;
    loop {
        let (next, value) = runtime.table_next(table, &key)?;
        if matches!(next, Value::Nil) {
            break;
        }
        let (Value::String(name), Value::String(value)) = (&next, &value) else {
            return Err(RuntimeError::message(message));
        };
        pairs.push((lossy(name), lossy(value)));
        key = next;
    }
    Ok(pairs)
}

/// The table a `term.http` request's `done` function gets.
pub(crate) fn http_table(runtime: &mut Runtime, response: &HttpResponse) -> RuntimeResult<Value> {
    let table = runtime.create_table()?;
    let ok = response.error.is_none() && (200..300).contains(&response.status);
    runtime.table_set(table, &Value::from("ok"), Value::Boolean(ok))?;
    runtime.table_set(
        table,
        &Value::from("status"),
        Value::Integer(i64::from(response.status)),
    )?;
    let headers = runtime.create_table()?;
    for (name, value) in &response.headers {
        runtime.table_set(
            headers,
            &Value::from(name.as_str()),
            Value::from(value.as_str()),
        )?;
    }
    runtime.table_set(table, &Value::from("headers"), Value::Table(headers))?;
    set_opt_str(runtime, table, "body", Some(&response.body))?;
    set_opt_str(runtime, table, "error", response.error.as_deref())?;
    Ok(Value::Table(table))
}

/// The list a forge's `check` function gets.
pub(crate) fn checks_table(
    runtime: &mut Runtime,
    checks: &[crate::ext::ForgeCheck],
) -> RuntimeResult<Value> {
    let list = runtime.create_table()?;
    for (index, check) in (1_i64..).zip(checks) {
        let table = runtime.create_table()?;
        set_opt_str(runtime, table, "account", Some(&check.account))?;
        set_opt_str(runtime, table, "host", Some(&check.host))?;
        set_opt_str(runtime, table, "path", Some(&check.path))?;
        set_opt_str(runtime, table, "remote", Some(&check.remote))?;
        runtime.table_set(
            table,
            &Value::from("signed_in"),
            Value::Boolean(check.signed_in),
        )?;
        let branches = runtime.create_table()?;
        for (n, branch) in (1_i64..).zip(&check.branches) {
            runtime.table_set(branches, &Value::Integer(n), Value::from(branch.as_str()))?;
        }
        runtime.table_set(table, &Value::from("branches"), Value::Table(branches))?;
        runtime.table_set(list, &Value::Integer(index), Value::Table(table))?;
    }
    Ok(Value::Table(list))
}

fn store_key(args: &[Value], function: &str) -> RuntimeResult<String> {
    let key = arg_string(args, 0)?;
    if store::valid_key(&key) {
        Ok(key)
    } else {
        Err(RuntimeError::message(format!(
            "term.{function}: keys are 1 to {} letters, digits, '.', '_', '-', '/' or ':'",
            store::MAX_KEY
        )))
    }
}

/// Whether `id` may name a project's directory: 1 to 100 letters, digits,
/// `-` and `_`.
fn valid_project_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 100
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
}

fn lock(store: &StoreHandle) -> std::sync::MutexGuard<'_, store::Store> {
    store.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// A table with `get`, `set`, `delete` and `keys` on `store`, named
/// `term.<name>` in errors. With `denied` every function raises it
/// instead, for an extension without the permission.
fn store_table(
    runtime: &mut Runtime,
    store: &StoreHandle,
    name: &'static str,
    denied: Option<Rc<str>>,
) -> RuntimeResult<TableHandle> {
    let check = |denied: &Option<Rc<str>>| match denied {
        Some(message) => Err(RuntimeError::message(message.to_string())),
        None => Ok(()),
    };
    let data = runtime.create_table()?;
    let (s, d) = (store.clone(), denied.clone());
    add(runtime, data, "get", move |runtime, args| {
        check(&d)?;
        let key = store_key(args, &format!("{name}.get"))?;
        let value = lock(&s).get(&key);
        Ok(vec![match value {
            Some(value) => crate::ext::json::from_json(runtime, &value)?,
            None => Value::Nil,
        }])
    })?;
    let (s, d) = (store.clone(), denied.clone());
    add(runtime, data, "set", move |runtime, args| {
        check(&d)?;
        let key = store_key(args, &format!("{name}.set"))?;
        let value = match args.get(1) {
            None | Some(Value::Nil) => None,
            Some(value) => Some(
                crate::ext::json::to_json(runtime, value)
                    .map_err(|e| RuntimeError::message(format!("term.{name}.set: {e}")))?,
            ),
        };
        lock(&s)
            .set(&key, value)
            .map_err(|e| RuntimeError::message(format!("term.{name}.set: {e}")))?;
        Ok(Vec::new())
    })?;
    let (s, d) = (store.clone(), denied.clone());
    add(runtime, data, "delete", move |_, args| {
        check(&d)?;
        let key = store_key(args, &format!("{name}.delete"))?;
        lock(&s)
            .set(&key, None)
            .map_err(|e| RuntimeError::message(format!("term.{name}.delete: {e}")))?;
        Ok(Vec::new())
    })?;
    let (s, d) = (store.clone(), denied);
    add(runtime, data, "keys", move |runtime, args| {
        check(&d)?;
        let prefix = match args.first() {
            None | Some(Value::Nil) => String::new(),
            Some(Value::String(text)) => lossy(text),
            Some(other) => return Err(RuntimeError::bad_argument(1, "string", Some(other))),
        };
        let keys = lock(&s).keys(&prefix);
        let list = runtime.create_table()?;
        for (index, key) in (1_i64..).zip(keys) {
            runtime.table_set(list, &Value::Integer(index), Value::from(key.as_str()))?;
        }
        Ok(vec![Value::Table(list)])
    })?;
    Ok(data)
}

/// Whether `name` may name a certificate in a trust store: it becomes a
/// file name there.
fn valid_certificate_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

/// Builds the table a `done` function receives.
pub(crate) fn job_table(runtime: &mut Runtime, result: &JobResult) -> RuntimeResult<Value> {
    let table = runtime.create_table()?;
    runtime.table_set(table, &Value::from("ok"), Value::Boolean(result.ok))?;
    if let Some(code) = result.code {
        runtime.table_set(table, &Value::from("code"), Value::Integer(i64::from(code)))?;
    }
    set_opt_str(runtime, table, "stdout", Some(&result.stdout))?;
    set_opt_str(runtime, table, "stderr", Some(&result.stderr))?;
    set_opt_str(runtime, table, "error", result.error.as_deref())?;
    Ok(Value::Table(table))
}

/// Reads `term.sandbox`'s `vars`: a table from variable name to value.
fn sandbox_vars(runtime: &mut Runtime, spec: TableHandle) -> RuntimeResult<Vec<(String, String)>> {
    const MESSAGE: &str = "term.sandbox: 'vars' must be a table from variable name to string";
    let table = match runtime.table_get(spec, &Value::from("vars"))? {
        Value::Nil => return Ok(Vec::new()),
        Value::Table(table) => table,
        _ => return Err(RuntimeError::message(MESSAGE)),
    };
    let mut vars = Vec::new();
    let mut key = Value::Nil;
    loop {
        let (next, value) = runtime.table_next(table, &key)?;
        if matches!(next, Value::Nil) {
            break;
        }
        let (Value::String(name), Value::String(value)) = (&next, &value) else {
            return Err(RuntimeError::message(MESSAGE));
        };
        let name = lossy(name);
        if name.is_empty() || name.contains(['=', '\0']) {
            return Err(RuntimeError::message(format!(
                "term.sandbox: '{name}' is not a variable name"
            )));
        }
        vars.push((name, lossy(value)));
        key = next;
    }
    // Table order is not insertion order; sort so the rules compare equal.
    vars.sort();
    Ok(vars)
}

/// Reads `term.sandbox`'s `files`: `{ sandbox = path, content = text }`.
fn sandbox_files(runtime: &mut Runtime, spec: TableHandle) -> RuntimeResult<Vec<(String, String)>> {
    const MESSAGE: &str =
        "term.sandbox: 'files' must be a list of { sandbox = ..., content = ... }";
    let list = match runtime.table_get(spec, &Value::from("files"))? {
        Value::Nil => return Ok(Vec::new()),
        Value::Table(list) => list,
        _ => return Err(RuntimeError::message(MESSAGE)),
    };
    let mut files = Vec::new();
    for index in 1_i64.. {
        let entry = match runtime.table_get(list, &Value::Integer(index))? {
            Value::Nil => break,
            Value::Table(entry) => entry,
            _ => return Err(RuntimeError::message(MESSAGE)),
        };
        let path = field_string(runtime, entry, "sandbox")?;
        let content = field_string(runtime, entry, "content")?;
        let (Some(path), Some(content)) = (path, content) else {
            return Err(RuntimeError::message(
                "term.sandbox: every file needs 'sandbox' and 'content'",
            ));
        };
        files.push((path, content));
    }
    Ok(files)
}

/// Reads `term.sandbox`'s `guards`: `{ name, title, kind, hosts, requests }`.
/// Their ids are `<extension>.<name>`.
fn sandbox_guards(
    runtime: &mut Runtime,
    spec: TableHandle,
    binding: &Binding,
) -> RuntimeResult<Vec<cmdterm_env::GuardRule>> {
    if matches!(runtime.table_get(spec, &Value::from("guards"))?, Value::Nil) {
        return Ok(Vec::new());
    }
    binding.require(Permission::Guards, "sandbox{ guards }")?;
    guard_list(runtime, spec, &binding.extension, "term.sandbox", true)
}

/// Reads the `guards` of `spec`, a list of
/// `{ name, title, kind, hosts, requests }`, for `function`. Without
/// `need_hosts` a guard may leave out `hosts`, as a forge's guards do,
/// which get the forge's accounts.
fn guard_list(
    runtime: &mut Runtime,
    spec: TableHandle,
    extension: &str,
    function: &str,
    need_hosts: bool,
) -> RuntimeResult<Vec<cmdterm_env::GuardRule>> {
    let message =
        format!("{function}: 'guards' must be a list of {{ name, title, kind, hosts, requests }}");
    let list = match runtime.table_get(spec, &Value::from("guards"))? {
        Value::Nil => return Ok(Vec::new()),
        Value::Table(list) => list,
        _ => return Err(RuntimeError::message(message)),
    };
    let mut guards: Vec<cmdterm_env::GuardRule> = Vec::new();
    for index in 1_i64.. {
        let entry = match runtime.table_get(list, &Value::Integer(index))? {
            Value::Nil => break,
            Value::Table(entry) => entry,
            _ => return Err(RuntimeError::message(message)),
        };
        let name = field_string(runtime, entry, "name")?
            .filter(|name| {
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            })
            .ok_or_else(|| {
                RuntimeError::message(format!(
                    "{function}: a guard's 'name' must be letters, digits, '-' or '_'"
                ))
            })?;
        let title = field_string(runtime, entry, "title")?.unwrap_or_else(|| name.clone());
        let kind = match field_string(runtime, entry, "kind")? {
            None => None,
            Some(kind) => Some(cmdterm_env::GuardKind::from_name(&kind).ok_or_else(|| {
                RuntimeError::message(format!(
                    "{function}: guard '{name}' has kind '{kind}'; use create, comment or merge"
                ))
            })?),
        };
        let list =
            |runtime: &mut Runtime, key: &str| match runtime.table_get(entry, &Value::from(key))? {
                Value::Table(list) => string_list(runtime, list, key),
                _ => Ok(Vec::new()),
            };
        let hosts = list(runtime, "hosts")?;
        let requests = list(runtime, "requests")?;
        if (need_hosts && hosts.is_empty()) || requests.is_empty() {
            let needs = if need_hosts {
                "'hosts' and 'requests'"
            } else {
                "'requests'"
            };
            return Err(RuntimeError::message(format!(
                "{function}: guard '{name}' needs {needs}"
            )));
        }
        if let Some(bad) = hosts
            .iter()
            .find(|host| *host != "*" && !cmdterm_env::valid_host_pattern(host))
        {
            return Err(RuntimeError::message(format!(
                "{function}: '{bad}' is not a host name, *.domain or *"
            )));
        }
        if let Some(bad) = requests
            .iter()
            .find(|request| !cmdterm_env::valid_request_pattern(request))
        {
            return Err(RuntimeError::message(format!(
                "{function}: '{bad}' is not a request such as 'PUT /api/*'"
            )));
        }
        let id = format!("{extension}.{name}");
        if guards.iter().any(|guard| guard.id == id) {
            return Err(RuntimeError::message(format!(
                "{function}: guard '{name}' is listed twice"
            )));
        }
        guards.push(cmdterm_env::GuardRule {
            id,
            title,
            hosts,
            requests,
            mode: cmdterm_env::GuardMode::Ask,
            kind,
        });
    }
    Ok(guards)
}

/// How old a cached blocklist may get when `max_age` is not given: a week.
const BLOCKLIST_MAX_AGE: u64 = 7 * 24 * 60 * 60;

/// Reads `term.sandbox`'s `blocklists`: `{ name, urls, max_age }`. Their
/// ids are `<extension>.<name>`.
fn sandbox_blocklists(
    runtime: &mut Runtime,
    spec: TableHandle,
    binding: &Binding,
) -> RuntimeResult<Vec<BlocklistSource>> {
    const MESSAGE: &str = "term.sandbox: 'blocklists' must be a list of { name, urls, max_age }";
    let list = match runtime.table_get(spec, &Value::from("blocklists"))? {
        Value::Nil => return Ok(Vec::new()),
        Value::Table(list) => list,
        _ => return Err(RuntimeError::message(MESSAGE)),
    };
    binding.require(Permission::Network, "sandbox{ blocklists }")?;
    let mut sources: Vec<BlocklistSource> = Vec::new();
    for index in 1_i64.. {
        let entry = match runtime.table_get(list, &Value::Integer(index))? {
            Value::Nil => break,
            Value::Table(entry) => entry,
            _ => return Err(RuntimeError::message(MESSAGE)),
        };
        let name = field_string(runtime, entry, "name")?
            .filter(|name| {
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            })
            .ok_or_else(|| {
                RuntimeError::message(
                    "term.sandbox: a blocklist's 'name' must be letters, digits, '-' or '_'",
                )
            })?;
        let urls = match runtime.table_get(entry, &Value::from("urls"))? {
            Value::Table(urls) => string_list(runtime, urls, "urls")?,
            _ => Vec::new(),
        };
        if urls.is_empty() {
            return Err(RuntimeError::message(format!(
                "term.sandbox: blocklist '{name}' needs 'urls'"
            )));
        }
        if let Some(bad) = urls.iter().find(|url| {
            !url.get(..8)
                .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
                || url.len() == 8
                || url
                    .bytes()
                    .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        }) {
            return Err(RuntimeError::message(format!(
                "term.sandbox: '{bad}' is not an https:// URL"
            )));
        }
        let shared = binding.shared.borrow();
        if let Some(bad) = urls.iter().find(|url| {
            !shared.allows(binding.slot, &crate::ext::url_host(url).unwrap_or_default())
        }) {
            return Err(RuntimeError::message(format!(
                "term.sandbox: '{bad}' is not on one of the extension's hosts; list its \
                 host in 'hosts' of manifest.toml"
            )));
        }
        drop(shared);
        let max_age = match runtime.table_get(entry, &Value::from("max_age"))? {
            Value::Nil => BLOCKLIST_MAX_AGE,
            Value::Integer(seconds) if seconds > 0 => seconds as u64,
            Value::Number(seconds) if seconds >= 1.0 => seconds as u64,
            _ => {
                return Err(RuntimeError::message(format!(
                    "term.sandbox: blocklist '{name}': 'max_age' must be a positive number of seconds"
                )));
            }
        };
        let id = format!("{}.{name}", binding.extension);
        if sources.iter().any(|source| source.id == id) {
            return Err(RuntimeError::message(format!(
                "term.sandbox: blocklist '{name}' is listed twice"
            )));
        }
        sources.push(BlocklistSource { id, urls, max_age });
    }
    Ok(sources)
}

/// Reads `term.task`'s `every`: seconds, or a string such as `"90s"`,
/// `"30m"`, `"12h"`, `"1d"` or `"2w"`. `None` for anything else or less
/// than [`MIN_TASK_INTERVAL`].
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn task_interval(value: &Value) -> Option<u64> {
    let seconds = match value {
        Value::Integer(n) => u64::try_from(*n).ok()?,
        Value::Number(n) if n.is_finite() && *n >= 0.0 => *n as u64,
        Value::String(text) => {
            let text = std::str::from_utf8(text.as_bytes()).ok()?.trim();
            let split = text.len().checked_sub(1)?;
            if !text.is_char_boundary(split) {
                return None;
            }
            let (count, unit) = text.split_at(split);
            let unit = match unit {
                "s" => 1,
                "m" => 60,
                "h" => 60 * 60,
                "d" => 24 * 60 * 60,
                "w" => 7 * 24 * 60 * 60,
                _ => return None,
            };
            count.trim().parse::<u64>().ok()?.checked_mul(unit)?
        }
        _ => return None,
    };
    (seconds >= MIN_TASK_INTERVAL).then_some(seconds)
}

/// Builds the table a task's `run` function receives: `{ task }`, plus the
/// project's `project`, `root`, `env` and `worktrees` when it runs for one.
pub(crate) fn task_table(
    runtime: &mut Runtime,
    task: &str,
    project: Option<&TaskProject>,
) -> RuntimeResult<Value> {
    let table = runtime.create_table()?;
    set_opt_str(runtime, table, "task", Some(task))?;
    if let Some(project) = project {
        set_opt_str(runtime, table, "project", Some(&project.name))?;
        set_opt_str(runtime, table, "project_id", Some(&project.id))?;
        set_opt_path(runtime, table, "root", Some(&project.root))?;
        set_opt_str(runtime, table, "env", Some(&project.env))?;
        let list = runtime.create_table()?;
        for (index, worktree) in (1_i64..).zip(&project.worktrees) {
            let entry = runtime.create_table()?;
            set_opt_path(runtime, entry, "path", Some(&worktree.path))?;
            set_opt_str(runtime, entry, "branch", worktree.branch.as_deref())?;
            set_opt_str(runtime, entry, "runtime", Some(&worktree.runtime))?;
            runtime.table_set(list, &Value::Integer(index), Value::Table(entry))?;
        }
        runtime.table_set(table, &Value::from("worktrees"), Value::Table(list))?;
    }
    Ok(Value::Table(table))
}

/// Builds the table a handler for `event` receives.
pub(crate) fn event_table(runtime: &mut Runtime, event: &Event) -> RuntimeResult<Value> {
    let table = runtime.create_table()?;
    set_opt_str(runtime, table, "event", Some(event.name()))?;
    match event {
        Event::AppStarted => {}
        Event::ProjectAdded {
            name,
            root,
            env,
            project_id,
        } => {
            set_opt_str(runtime, table, "name", Some(name))?;
            set_opt_path(runtime, table, "root", Some(root))?;
            set_opt_str(runtime, table, "env", Some(env))?;
            set_opt_str(runtime, table, "project_id", Some(project_id))?;
        }
        Event::WorktreeActivated {
            project,
            project_id,
            path,
            branch,
            env,
        } => {
            set_opt_str(runtime, table, "project", Some(project))?;
            set_opt_str(runtime, table, "project_id", Some(project_id))?;
            set_opt_path(runtime, table, "path", Some(path))?;
            set_opt_str(runtime, table, "branch", branch.as_deref())?;
            set_opt_str(runtime, table, "env", Some(env))?;
        }
        Event::TerminalOpened { id, cwd } => {
            runtime.table_set(table, &Value::from("id"), id_value(*id))?;
            set_opt_path(runtime, table, "cwd", cwd.as_deref())?;
        }
        Event::EnvironmentReady {
            project,
            worktree,
            kind,
            env,
            host,
            fresh,
        } => {
            set_opt_str(runtime, table, "project", Some(project))?;
            set_opt_path(runtime, table, "worktree", Some(worktree))?;
            set_opt_str(runtime, table, "kind", Some(kind))?;
            set_opt_str(runtime, table, "env", Some(env))?;
            set_opt_str(runtime, table, "host", Some(host))?;
            runtime.table_set(table, &Value::from("fresh"), Value::Boolean(*fresh))?;
        }
        Event::TerminalExited { id, code } => {
            runtime.table_set(table, &Value::from("id"), id_value(*id))?;
            if let Some(code) = code {
                runtime.table_set(
                    table,
                    &Value::from("code"),
                    Value::Integer(i64::from(*code)),
                )?;
            }
        }
    }
    Ok(Value::Table(table))
}

// Ids above i64::MAX do not occur in practice; they become floats rather
// than wrapping negative.
#[allow(clippy::cast_precision_loss)]
fn id_value(id: u64) -> Value {
    i64::try_from(id).map_or(Value::Number(id as f64), Value::Integer)
}

fn lossy(text: &LuaString) -> String {
    String::from_utf8_lossy(text.as_bytes()).into_owned()
}

fn is_callable(value: &Value) -> bool {
    matches!(
        value,
        Value::Closure(_)
            | Value::NativeFunction(_)
            | Value::BuiltinFunction(_)
            | Value::CoroutineWrapper(_)
            | Value::Table(_)
            | Value::Userdata(_)
    )
}

fn arg_string(args: &[Value], index: usize) -> RuntimeResult<String> {
    match args.get(index) {
        Some(Value::String(text)) => Ok(lossy(text)),
        Some(Value::Integer(n)) => Ok(n.to_string()),
        Some(Value::Number(n)) => Ok(n.to_string()),
        other => Err(RuntimeError::bad_argument(index + 1, "string", other)),
    }
}

fn arg_function(args: &[Value], index: usize) -> RuntimeResult<Value> {
    match args.get(index) {
        Some(value) if is_callable(value) => Ok(value.clone()),
        other => Err(RuntimeError::bad_argument(index + 1, "function", other)),
    }
}

fn arg_table(args: &[Value], index: usize) -> RuntimeResult<TableHandle> {
    match args.get(index) {
        Some(Value::Table(table)) => Ok(*table),
        other => Err(RuntimeError::bad_argument(index + 1, "table", other)),
    }
}

fn field_string(runtime: &Runtime, table: TableHandle, key: &str) -> RuntimeResult<Option<String>> {
    match runtime.table_get(table, &Value::from(key))? {
        Value::Nil => Ok(None),
        Value::String(text) => Ok(Some(lossy(&text))),
        _ => Err(RuntimeError::message(format!(
            "field '{key}' must be a string"
        ))),
    }
}

fn string_list(runtime: &Runtime, table: TableHandle, key: &str) -> RuntimeResult<Vec<String>> {
    let mut list = Vec::new();
    for index in 1_i64.. {
        match runtime.table_get(table, &Value::Integer(index))? {
            Value::Nil => break,
            Value::String(text) => list.push(lossy(&text)),
            Value::Integer(n) => list.push(n.to_string()),
            Value::Number(n) => list.push(n.to_string()),
            _ => {
                return Err(RuntimeError::message(format!(
                    "field '{key}' must be a list of strings"
                )));
            }
        }
    }
    Ok(list)
}

/// Reads a command's `keys` field: one sequence for every keymap, or a table
/// from keymap id to sequence where `default` covers the keymaps not named.
fn key_requests(runtime: &Runtime, spec: TableHandle) -> RuntimeResult<Vec<KeyRequest>> {
    const MESSAGE: &str = "term.command: 'keys' must be a string or a table from keymap to string";
    let table = match runtime.table_get(spec, &Value::from("keys"))? {
        Value::Nil => return Ok(Vec::new()),
        Value::String(keys) => {
            return Ok(vec![KeyRequest {
                keymap: None,
                keys: lossy(&keys),
            }]);
        }
        Value::Table(table) => table,
        _ => return Err(RuntimeError::message(MESSAGE)),
    };
    let mut out = Vec::new();
    let mut key = Value::Nil;
    loop {
        let (next, value) = runtime.table_next(table, &key)?;
        let (Value::String(keymap), Value::String(keys)) = (&next, &value) else {
            if matches!(next, Value::Nil) {
                break;
            }
            return Err(RuntimeError::message(MESSAGE));
        };
        let keymap = lossy(keymap);
        out.push(KeyRequest {
            keymap: (keymap != "default").then_some(keymap),
            keys: lossy(keys),
        });
        key = next;
    }
    // Table order is not insertion order; sort so the result is stable.
    out.sort_by(|a, b| a.keymap.cmp(&b.keymap));
    Ok(out)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn as_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Integer(n) => u64::try_from(*n).ok(),
        Value::Number(n) if *n >= 0.0 && n.fract() == 0.0 && *n < 1.8e19 => Some(*n as u64),
        _ => None,
    }
}

fn set_opt_str(
    runtime: &mut Runtime,
    table: TableHandle,
    key: &str,
    value: Option<&str>,
) -> RuntimeResult<()> {
    match value {
        Some(text) => runtime.table_set(table, &Value::from(key), Value::from(text)),
        None => Ok(()),
    }
}

fn set_opt_path(
    runtime: &mut Runtime,
    table: TableHandle,
    key: &str,
    value: Option<&Path>,
) -> RuntimeResult<()> {
    match value {
        Some(path) => runtime.table_set(table, &Value::from(key), path_value(path)),
        None => Ok(()),
    }
}

#[cfg(unix)]
fn path_value(path: &Path) -> Value {
    use std::os::unix::ffi::OsStrExt;
    Value::from(path.as_os_str().as_bytes())
}

#[cfg(not(unix))]
fn path_value(path: &Path) -> Value {
    Value::from(path.to_string_lossy().into_owned())
}

fn add(
    runtime: &mut Runtime,
    table: TableHandle,
    name: &'static str,
    callback: impl Fn(&mut Runtime, &[Value]) -> RuntimeResult<Vec<Value>> + 'static,
) -> RuntimeResult<()> {
    let handle = runtime.register_native_closure(callback)?;
    runtime.set_native_name(handle, &format!("term.{name}"));
    runtime.table_set(table, &Value::from(name), Value::NativeFunction(handle))
}

/// Wraps the functions that catch errors so they re-raise once the time
/// budget expired. Without this, `while true do pcall(f) end` would swallow
/// every interrupt. Varargs pass through without building tables.
const PROTECT: &str = r"
local expired = ...
local raw_pcall, raw_xpcall, error = pcall, xpcall, error
local function check(ok, ...)
  if not ok and expired() then error((...), 0) end
  return ok, ...
end
pcall = function(f, ...) return check(raw_pcall(f, ...)) end
xpcall = function(f, handler, ...) return check(raw_xpcall(f, handler, ...)) end
local co = coroutine
if type(co) == 'table' and co.resume then
  local raw_resume = co.resume
  co.resume = function(thread, ...) return check(raw_resume(thread, ...)) end
end
";

/// Runs [`PROTECT`] in a fresh VM.
pub(crate) fn protect(lua: &mut oximond::Lua, alarm: &Alarm) -> Result<(), oximond::Error> {
    let expired = std::sync::Arc::clone(&alarm.expired);
    let check = lua.runtime_mut().register_native_closure(move |_, _| {
        Ok(vec![Value::Boolean(
            expired.load(std::sync::atomic::Ordering::Relaxed),
        )])
    })?;
    let chunk = lua.compile("=term.protect", PROTECT)?;
    lua.execute_with_arguments(&chunk, vec![Value::NativeFunction(check)])?;
    Ok(())
}
