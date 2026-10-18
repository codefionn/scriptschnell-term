//! The extension host: one Oximond VM per extension.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use oximond::runtime::TableHandle;
use oximond::{CompatibilityProfile, Lua, LuaJitExtension, Value};

use crate::ext::api::{self, Ref, Shared};
use crate::ext::manifest::{Manifest, Permission, Permissions, SettingKind, SettingValue};

/// The user's values of extension settings: extension, then setting key.
pub type SettingValues = BTreeMap<String, BTreeMap<String, SettingValue>>;
use crate::ext::output::PrefixWriter;
use crate::ext::watchdog::{Alarm, Watchdog};
use crate::ext::{
    AgentInfo, Callback, CommandInfo, Config, Context, Event, ExtError, ExtensionInfo, ForgeCheck,
    ForgeInfo, HttpResponse, JobResult, Rendered, Request, SandboxRules, TaskInfo, TaskProject,
    ToolCaller, ToolInfo, ToolResult,
};

/// Errors about the host itself, not about one extension, carry this name.
const HOST: &str = "scriptschnell-term";

struct Extension {
    info: ExtensionInfo,
    /// `None` once disabled: a failed extension's VM is dropped.
    lua: Option<Lua>,
    refs: Option<TableHandle>,
    alarm: Alarm,
}

/// Loads extensions and dispatches events, commands, and requests.
///
/// Lives on the UI thread. Each extension gets its own VM, so one extension
/// cannot corrupt another's globals, and disabling it frees its memory.
pub struct ExtensionHost {
    config: Config,
    extensions: Vec<Extension>,
    shared: Rc<RefCell<Shared>>,
    /// Mirror of `shared.commands`, so `commands()` can hand out a slice.
    commands: Vec<CommandInfo>,
    /// Bumped whenever `commands` changes.
    commands_revision: u64,
    /// Mirror of `shared.tasks`.
    tasks: Vec<TaskInfo>,
    /// Bumped whenever `tasks` changes.
    tasks_revision: u64,
    /// Every extension's `term.sandbox` rules, merged in load order.
    sandbox: SandboxRules,
    sandbox_revision: u64,
    /// Mirror of `shared.tools`.
    tools: Vec<ToolInfo>,
    tools_revision: u64,
    /// Mirror of `shared.forges`.
    forges: Vec<ForgeInfo>,
    forges_revision: u64,
    /// Mirror of `shared.environments`, also handed to `cmdterm-env`
    /// under `owner`.
    environments: Vec<cmdterm_env::CustomProvider>,
    environments_revision: u64,
    /// Mirror of `shared.agents`.
    agents: Vec<AgentInfo>,
    agents_revision: u64,
    owner: u64,
    watchdog: Watchdog,
    /// Reused per `emit` to copy handler refs out of the shared state.
    scratch: Vec<Ref>,
    /// Extensions whose manifest says `enabled = false` that load anyway.
    turned_on: Vec<String>,
}

impl Default for ExtensionHost {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtensionHost {
    /// A host with the default [`Config`]: Lua 5.4, interpreter, 500 ms budget.
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(Config::default())
    }

    #[must_use]
    pub fn with_config(config: Config) -> Self {
        Self {
            config,
            extensions: Vec::new(),
            shared: Rc::default(),
            commands: Vec::new(),
            commands_revision: 0,
            tasks: Vec::new(),
            tasks_revision: 0,
            sandbox: SandboxRules::default(),
            sandbox_revision: next_sandbox_revision(),
            tools: Vec::new(),
            tools_revision: 0,
            forges: Vec::new(),
            forges_revision: 0,
            environments: Vec::new(),
            environments_revision: 0,
            agents: Vec::new(),
            agents_revision: 0,
            owner: next_owner(),
            watchdog: Watchdog::new(),
            scratch: Vec::new(),
            turned_on: Vec::new(),
        }
    }

    /// Names the extensions whose manifest says `enabled = false` that the
    /// user turned on, for the next [`Self::load_dirs_except`]. A name in
    /// its `off` list stays off.
    pub fn turn_on(&mut self, names: &[String]) {
        self.turned_on = names.to_vec();
    }

    #[must_use]
    pub const fn config(&self) -> &Config {
        &self.config
    }

    /// Loads every `<dir>/<name>/init.lua` and every `<dir>/<name>.lua`, in
    /// name order, each with every permission its manifest declares.
    /// Returns per-extension errors; good ones stay loaded. A missing
    /// directory is not an error.
    pub fn load_dir(&mut self, dir: &Path) -> Vec<ExtError> {
        self.load_dir_with(&[dir], &[], &|_| Permissions::all(), &SettingValues::new())
    }

    /// [`Self::load_dir`], except that extensions named in `off` are listed
    /// in [`Self::extensions`] with `turned_off` set instead of being
    /// loaded, and that `granted` holds the permission names the user gave
    /// each extension. An extension whose manifest declares more is listed
    /// with [`ExtensionInfo::missing`] set, not loaded, and reported.
    /// `values` holds the user's values of the settings manifests declare;
    /// a value that does not fit its setting's type gives way to the
    /// default.
    pub fn load_dir_except(
        &mut self,
        dir: &Path,
        off: &[String],
        granted: &BTreeMap<String, Vec<String>>,
        values: &SettingValues,
    ) -> Vec<ExtError> {
        self.load_dirs_except(&[dir], off, granted, values)
    }

    /// [`Self::load_dir_except`] for several directories, such as the
    /// shipped extensions and the user's own. An extension in a later
    /// directory replaces one of the same name in an earlier one; all of
    /// them load in name order.
    pub fn load_dirs_except(
        &mut self,
        dirs: &[&Path],
        off: &[String],
        granted: &BTreeMap<String, Vec<String>>,
        values: &SettingValues,
    ) -> Vec<ExtError> {
        let granted = |name: &str| {
            granted
                .get(name)
                .map_or(Permissions::NONE, |names| Permissions::from_names(names))
        };
        self.load_dir_with(dirs, off, &granted, values)
    }

    fn load_dir_with(
        &mut self,
        dirs: &[&Path],
        off: &[String],
        granted: &dyn Fn(&str) -> Permissions,
        values: &SettingValues,
    ) -> Vec<ExtError> {
        let mut errors = Vec::new();
        let mut found: Vec<(String, PathBuf, Option<PathBuf>)> = Vec::new();
        for (index, dir) in dirs.iter().enumerate() {
            if dirs[..index].contains(dir) {
                continue;
            }
            for (name, file, module_dir) in find_extensions(dir, &mut errors) {
                found.retain(|known| known.0 != name);
                found.push((name, file, module_dir));
            }
        }
        found.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, file, module_dir) in found {
            let manifest = match module_dir.as_deref().map(Manifest::load) {
                Some(Ok(manifest)) => manifest.unwrap_or_default(),
                Some(Err(_)) if off.contains(&name) => Manifest::default(),
                Some(Err(message)) => {
                    self.list_failed(&name, file, message.clone());
                    errors.push(ExtError::new(&name, message));
                    continue;
                }
                None => Manifest::default(),
            };
            let on = if manifest.enabled {
                !off.contains(&name)
            } else {
                self.turned_on.contains(&name) && !off.contains(&name)
            };
            if !on {
                self.list_turned_off(name, file, manifest);
                continue;
            }
            let allowed = granted(&name);
            let missing = manifest.permissions.without(allowed);
            // An extension that declares permissions runs once the user has
            // granted at least one of them, and then only with the granted
            // ones. While none are granted it waits on the Extensions page.
            let effective = manifest.permissions.intersect(allowed);
            if !manifest.permissions.is_empty() && effective.is_empty() {
                errors.push(ExtError::new(
                    &name,
                    format!(
                        "needs permission to use {}; grant it on the Extensions page of the settings",
                        missing.names().join(", ")
                    ),
                ));
                self.list_needing(name, file, manifest, missing);
                continue;
            }
            let own = values.get(&name);
            let settings: Vec<(String, SettingValue)> = manifest
                .settings
                .iter()
                .map(|spec| {
                    let value = own
                        .and_then(|own| own.get(&spec.key))
                        .filter(|value| spec.kind.fits(value))
                        .unwrap_or(&spec.default);
                    (spec.key.clone(), value.clone())
                })
                .collect();
            let hosts = allowed_hosts(&manifest, &settings);
            let result = match std::fs::read(&file) {
                Ok(source) => {
                    let loaded = self.load(
                        &name,
                        Some(file),
                        module_dir.as_deref(),
                        &source,
                        effective,
                        (settings, Some(hosts)),
                    );
                    self.describe(&name, &manifest, missing);
                    loaded
                }
                Err(error) => Err(ExtError::new(
                    &name,
                    format!("cannot read {}: {error}", file.display()),
                )),
            };
            if let Err(error) = result {
                errors.push(error);
            }
        }
        errors
    }

    fn list_turned_off(&mut self, name: String, path: PathBuf, manifest: Manifest) {
        if self.extensions.iter().any(|ext| ext.info.name == name) {
            return;
        }
        self.extensions.push(Extension {
            info: ExtensionInfo {
                name,
                path: Some(path),
                enabled: false,
                last_error: None,
                turned_off: true,
                title: manifest.title,
                description: manifest.description,
                permissions: manifest.permissions,
                missing: Permissions::NONE,
                hosts: manifest.hosts,
                settings: manifest.settings,
                logins: manifest.logins,
                default_on: manifest.enabled,
            },
            lua: None,
            refs: None,
            alarm: Alarm::default(),
        });
    }

    /// Lists an extension the user must grant `missing` before it loads.
    fn list_needing(
        &mut self,
        name: String,
        path: PathBuf,
        manifest: Manifest,
        missing: Permissions,
    ) {
        if self.extensions.iter().any(|ext| ext.info.name == name) {
            return;
        }
        self.extensions.push(Extension {
            info: ExtensionInfo {
                name,
                path: Some(path),
                enabled: false,
                last_error: None,
                turned_off: false,
                title: manifest.title,
                description: manifest.description,
                permissions: manifest.permissions,
                missing,
                hosts: manifest.hosts,
                settings: manifest.settings,
                logins: manifest.logins,
                default_on: manifest.enabled,
            },
            lua: None,
            refs: None,
            alarm: Alarm::default(),
        });
    }

    /// Lists an extension that failed before it could load.
    fn list_failed(&mut self, name: &str, path: PathBuf, message: String) {
        if self.extensions.iter().any(|ext| ext.info.name == name) {
            return;
        }
        self.extensions.push(Extension {
            info: ExtensionInfo {
                name: name.to_owned(),
                path: Some(path),
                enabled: false,
                last_error: Some(message),
                turned_off: false,
                title: None,
                description: None,
                permissions: Permissions::NONE,
                missing: Permissions::NONE,
                hosts: Vec::new(),
                settings: Vec::new(),
                logins: None,
                default_on: true,
            },
            lua: None,
            refs: None,
            alarm: Alarm::default(),
        });
    }

    fn describe(&mut self, name: &str, manifest: &Manifest, missing: Permissions) {
        if let Some(ext) = self.extensions.iter_mut().find(|ext| ext.info.name == name) {
            ext.info.title.clone_from(&manifest.title);
            ext.info.description.clone_from(&manifest.description);
            ext.info.permissions = manifest.permissions;
            ext.info.missing = missing;
            ext.info.hosts.clone_from(&manifest.hosts);
            ext.info.settings.clone_from(&manifest.settings);
            ext.info.logins.clone_from(&manifest.logins);
            ext.info.default_on = manifest.enabled;
        }
    }

    /// Loads one extension from source text, with every permission.
    ///
    /// # Errors
    ///
    /// Returns the compile or runtime error. The extension then shows up in
    /// [`Self::extensions`] as disabled.
    pub fn load_source(&mut self, name: &str, source: &str) -> Result<(), ExtError> {
        self.load_source_with(name, source, Permissions::all())
    }

    /// [`Self::load_source`] with only `permissions`.
    ///
    /// # Errors
    ///
    /// As [`Self::load_source`].
    pub fn load_source_with(
        &mut self,
        name: &str,
        source: &str,
        permissions: Permissions,
    ) -> Result<(), ExtError> {
        self.load(
            name,
            None,
            None,
            source.as_bytes(),
            permissions,
            (Vec::new(), None),
        )?;
        if let Some(ext) = self.extensions.iter_mut().find(|ext| ext.info.name == name) {
            ext.info.permissions = permissions;
        }
        Ok(())
    }

    /// Every extension seen, in load order, including failed ones.
    pub fn extensions(&self) -> impl Iterator<Item = &ExtensionInfo> {
        self.extensions.iter().map(|ext| &ext.info)
    }

    /// Calls every handler registered for this event, in load order. An
    /// extension whose handler fails is disabled; the others still run.
    pub fn emit(&mut self, event: &Event) -> Vec<ExtError> {
        let mut errors = Vec::new();
        let kind = event.kind();
        for slot in 0..self.extensions.len() {
            self.scratch.clear();
            {
                let shared = self.shared.borrow();
                let Some(handlers) = shared.slots.get(slot) else {
                    continue;
                };
                self.scratch.extend_from_slice(&handlers.handlers[kind]);
            }
            if self.scratch.is_empty() {
                continue;
            }
            let refs = std::mem::take(&mut self.scratch);
            let result = self.guarded(slot, |lua, table| {
                let argument =
                    api::event_table(lua.runtime_mut(), event).map_err(|e| e.to_string())?;
                for &key in &refs {
                    let function = api::stored_function(lua.runtime_mut(), table, key);
                    call(lua, function, vec![argument.clone()])?;
                }
                Ok(())
            });
            self.scratch = refs;
            if let Err(error) = result {
                errors.push(error);
            }
        }
        self.sync_commands();
        errors
    }

    /// Registered palette commands, in registration order.
    #[must_use]
    pub fn commands(&self) -> &[CommandInfo] {
        &self.commands
    }

    /// Changes whenever a command is added, replaced or removed, so the app
    /// can tell when to look at [`Self::commands`] again.
    #[must_use]
    pub const fn commands_revision(&self) -> u64 {
        self.commands_revision
    }

    /// What extensions add to sandboxes, merged across every enabled one.
    #[must_use]
    pub fn sandbox_rules(&self) -> &SandboxRules {
        &self.sandbox
    }

    /// Changes whenever [`Self::sandbox_rules`] changes. Unique across hosts,
    /// so a reloaded host never repeats the revision of the one it replaces.
    #[must_use]
    pub const fn sandbox_revision(&self) -> u64 {
        self.sandbox_revision
    }

    /// Runs a palette command's `run` function.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown command, or the command's Lua error
    /// (which also disables its extension).
    pub fn run_command(&mut self, name: &str) -> Result<(), ExtError> {
        let target = {
            let shared = self.shared.borrow();
            shared
                .commands
                .iter()
                .position(|c| c.name == name)
                .map(|index| shared.command_targets[index])
        };
        let Some((slot, key)) = target else {
            return Err(ExtError::new(HOST, format!("unknown command '{name}'")));
        };
        let result = self.guarded(slot, |lua, table| {
            let function = api::stored_function(lua.runtime_mut(), table, key);
            call(lua, function, Vec::new())
        });
        self.sync_commands();
        result
    }

    /// Recurring tasks, in registration order.
    #[must_use]
    pub fn tasks(&self) -> &[TaskInfo] {
        &self.tasks
    }

    /// Changes whenever a task is added, replaced or removed.
    #[must_use]
    pub const fn tasks_revision(&self) -> u64 {
        self.tasks_revision
    }

    /// Runs a task's `run` function once, for `project` or for none. The
    /// app decides when; this only calls it.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown task, or the task's Lua error (which
    /// also disables its extension).
    pub fn run_task(&mut self, id: &str, project: Option<&TaskProject>) -> Result<(), ExtError> {
        let target = {
            let shared = self.shared.borrow();
            shared
                .tasks
                .iter()
                .position(|t| t.id == id)
                .map(|index| shared.task_targets[index])
        };
        let Some((slot, key)) = target else {
            return Err(ExtError::new(HOST, format!("unknown task '{id}'")));
        };
        let result = self.guarded(slot, |lua, table| {
            let argument =
                api::task_table(lua.runtime_mut(), id, project).map_err(|e| e.to_string())?;
            let function = api::stored_function(lua.runtime_mut(), table, key);
            call(lua, function, vec![argument])
        });
        self.sync_commands();
        result
    }

    /// Calls the `done` function of a finished request with `result`. A
    /// callback of a disabled extension, or of a host this one replaced, is
    /// dropped.
    ///
    /// # Errors
    ///
    /// Returns the function's Lua error, which also disables its extension.
    pub fn complete(&mut self, callback: Callback, result: &JobResult) -> Result<(), ExtError> {
        let target = {
            let mut shared = self.shared.borrow_mut();
            let index = shared.callbacks.iter().position(|c| c.0 == callback.0);
            index.map(|index| shared.callbacks.swap_remove(index))
        };
        let Some((_, slot, key)) = target else {
            return Ok(());
        };
        let result = self.guarded(slot, |lua, table| {
            let runtime = lua.runtime_mut();
            let function = api::stored_function(runtime, table, key);
            // Release the ref: each callback runs once.
            runtime
                .table_set(table, &Value::Integer(key), Value::Nil)
                .map_err(|e| e.to_string())?;
            let argument = api::job_table(runtime, result).map_err(|e| e.to_string())?;
            call(lua, function, vec![argument])
        });
        self.sync_commands();
        result
    }

    /// Calls the `done` function of a finished `term.http` request.
    ///
    /// # Errors
    ///
    /// Returns the function's Lua error, which also disables its extension.
    pub fn complete_http(
        &mut self,
        callback: Callback,
        response: &HttpResponse,
    ) -> Result<(), ExtError> {
        let target = {
            let mut shared = self.shared.borrow_mut();
            let index = shared.callbacks.iter().position(|c| c.0 == callback.0);
            index.map(|index| shared.callbacks.swap_remove(index))
        };
        let Some((_, slot, key)) = target else {
            return Ok(());
        };
        let result = self.guarded(slot, |lua, table| {
            let runtime = lua.runtime_mut();
            let function = api::stored_function(runtime, table, key);
            runtime
                .table_set(table, &Value::Integer(key), Value::Nil)
                .map_err(|e| e.to_string())?;
            let argument = api::http_table(runtime, response).map_err(|e| e.to_string())?;
            call(lua, function, vec![argument])
        });
        self.sync_commands();
        result
    }

    /// The forges extensions registered, in registration order.
    #[must_use]
    pub fn forges(&self) -> &[ForgeInfo] {
        &self.forges
    }

    /// Changes whenever [`Self::forges`] changes.
    #[must_use]
    pub const fn forges_revision(&self) -> u64 {
        self.forges_revision
    }

    /// Takes the user's accounts per forge id. A forge with an entry gets
    /// those accounts, the others the ones their `term.forge` call names.
    /// Forges registered later take them as well.
    pub fn set_forge_hosts(&mut self, hosts: &BTreeMap<String, Vec<String>>) {
        let changed = {
            let mut shared = self.shared.borrow_mut();
            if shared.forge_hosts == *hosts {
                return;
            }
            shared.forge_hosts.clone_from(hosts);
            let shared = &mut *shared;
            for forge in &mut shared.forges {
                let own = (shared.forge_hosts.get(&forge.id)).unwrap_or(&forge.default_hosts);
                if forge.hosts != *own {
                    forge.hosts.clone_from(own);
                    shared.forges_dirty = true;
                }
            }
            let changed = shared.forges_dirty;
            if changed {
                shared.forges_dirty = false;
                self.forges.clone_from(&shared.forges);
                self.forges_revision += 1;
            }
            changed
        };
        if changed {
            // The forges' guards follow their accounts.
            let shared = Rc::clone(&self.shared);
            self.merge_sandbox(&shared.borrow());
        }
    }

    /// Whether extension `name` may reach `host` with `term.http`: a host
    /// its manifest or a `hosts` setting names, or one of its forges'
    /// accounts or their subdomains.
    #[must_use]
    pub fn allows_host(&self, name: &str, host: &str) -> bool {
        self.extensions
            .iter()
            .position(|ext| ext.info.name == name)
            .is_some_and(|slot| self.shared.borrow().allows(slot, host))
    }

    /// The hosts the `term.secret` values of extension `name` go to.
    #[must_use]
    pub fn secret_hosts(&self, name: &str) -> Vec<String> {
        let Some(slot) = self.extensions.iter().position(|ext| ext.info.name == name) else {
            return Vec::new();
        };
        let shared = self.shared.borrow();
        let mut hosts: Vec<String> = Vec::new();
        for secret in shared
            .slots
            .get(slot)
            .map_or(&[][..], |s| &s.sandbox.secrets)
        {
            for host in &secret.hosts {
                if !hosts.contains(host) {
                    hosts.push(host.clone());
                }
            }
        }
        hosts
    }

    /// Calls the `check` function of forge `id` with the repositories and
    /// branches to look at. A forge nobody registered ignores it.
    ///
    /// # Errors
    ///
    /// Returns the function's Lua error, which also disables its extension.
    pub fn forge_check(&mut self, id: &str, checks: &[ForgeCheck]) -> Result<(), ExtError> {
        let Some((slot, refs)) = self.forge_target(id) else {
            return Ok(());
        };
        let result = self.guarded(slot, |lua, table| {
            let runtime = lua.runtime_mut();
            let function = api::stored_function(runtime, table, refs.check);
            let argument = api::checks_table(runtime, checks).map_err(|e| e.to_string())?;
            call(lua, function, vec![argument])
        });
        self.sync_commands();
        result
    }

    /// Calls the `discussion` function of forge `id`, which answers with
    /// `term.forge_discussion`. False when the forge has none.
    ///
    /// # Errors
    ///
    /// Returns the function's Lua error, which also disables its extension.
    pub fn forge_discussion(
        &mut self,
        id: &str,
        request: &crate::ext::DiscussionRequest,
    ) -> Result<bool, ExtError> {
        let Some((slot, refs)) = self.forge_target(id) else {
            return Ok(false);
        };
        let Some(key) = refs.discussion else {
            return Ok(false);
        };
        let result = self.guarded(slot, |lua, table| {
            let runtime = lua.runtime_mut();
            let function = api::stored_function(runtime, table, key);
            let argument =
                api::discussion_request_table(runtime, request).map_err(|e| e.to_string())?;
            call(lua, function, vec![argument])
        });
        self.sync_commands();
        result.map(|()| true)
    }

    /// The page where the user creates a token for `account`, from forge
    /// `id`'s `token_url` function.
    ///
    /// # Errors
    ///
    /// Returns the function's Lua error, which also disables its extension.
    pub fn forge_token_url(&mut self, id: &str, account: &str) -> Result<Option<String>, ExtError> {
        Ok(self.forge_strings(id, account, |refs| refs.token_url)?.0)
    }

    /// The URL that names the owner of a token for `account` and the field of
    /// its JSON answer that holds the name, from forge `id`'s `whoami`
    /// function. The field is a dotted path such as `user.name`.
    ///
    /// # Errors
    ///
    /// Returns the function's Lua error, which also disables its extension.
    pub fn forge_whoami(
        &mut self,
        id: &str,
        account: &str,
    ) -> Result<Option<(String, Option<String>)>, ExtError> {
        let (url, field) = self.forge_strings(id, account, |refs| refs.whoami)?;
        Ok(url.map(|url| (url, field)))
    }

    fn forge_target(&self, id: &str) -> Option<(usize, api::ForgeRefs)> {
        let shared = self.shared.borrow();
        let index = shared.forges.iter().position(|f| f.id == id)?;
        Some(shared.forge_targets[index])
    }

    /// Calls one of a forge's optional functions with the host of `account`
    /// and `account` itself, and returns its first two results as strings.
    fn forge_strings(
        &mut self,
        id: &str,
        account: &str,
        pick: impl Fn(&api::ForgeRefs) -> Option<api::Ref>,
    ) -> Result<(Option<String>, Option<String>), ExtError> {
        let Some((slot, refs)) = self.forge_target(id) else {
            return Ok((None, None));
        };
        let Some(key) = pick(&refs) else {
            return Ok((None, None));
        };
        let mut out = (None, None);
        let result = self.guarded(slot, |lua, table| {
            let function = api::stored_function(lua.runtime_mut(), table, key);
            let host = Value::from(crate::ext::account_host(account));
            let values = call_returning(lua, function, vec![host, Value::from(account)])?;
            let text = |value: Option<&Value>| match value {
                Some(Value::String(text)) => {
                    Some(String::from_utf8_lossy(text.as_bytes()).into_owned())
                }
                _ => None,
            };
            out = (text(values.first()), text(values.get(1)));
            lua.runtime_mut().recycle_result_vec(values);
            Ok(())
        });
        self.sync_commands();
        result.map(|()| out)
    }

    /// The environments extensions registered, in registration order.
    /// Locations with their schemes reach them through `cmdterm-env`.
    #[must_use]
    pub fn environments(&self) -> &[cmdterm_env::CustomProvider] {
        &self.environments
    }

    /// Changes whenever [`Self::environments`] changes.
    #[must_use]
    pub const fn environments_revision(&self) -> u64 {
        self.environments_revision
    }

    /// The coding agents extensions registered, in registration order.
    #[must_use]
    pub fn agents(&self) -> &[AgentInfo] {
        &self.agents
    }

    /// Changes whenever [`Self::agents`] changes.
    #[must_use]
    pub const fn agents_revision(&self) -> u64 {
        self.agents_revision
    }

    /// A number no other host in this process has, for registries that
    /// keep what each host registered apart.
    #[must_use]
    pub const fn owner(&self) -> u64 {
        self.owner
    }

    /// The tools extensions offer agents, in registration order.
    #[must_use]
    pub fn tools(&self) -> &[ToolInfo] {
        &self.tools
    }

    /// Changes whenever [`Self::tools`] changes.
    #[must_use]
    pub const fn tools_revision(&self) -> u64 {
        self.tools_revision
    }

    /// Runs tool `name` with `arguments` for `caller`. The inner result is
    /// what the tool returned: a value, or the error it gave as
    /// `nil, message`.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown tool, or the tool's Lua error (which
    /// also disables its extension).
    pub fn call_tool(
        &mut self,
        name: &str,
        arguments: &serde_json::Value,
        caller: &ToolCaller,
    ) -> Result<ToolResult, ExtError> {
        let target = {
            let shared = self.shared.borrow();
            shared
                .tools
                .iter()
                .position(|t| t.name == name)
                .map(|index| shared.tool_targets[index])
        };
        let Some((slot, key)) = target else {
            return Err(ExtError::new(HOST, format!("unknown tool '{name}'")));
        };
        let mut outcome = Ok(serde_json::Value::Null);
        let result = self.guarded(slot, |lua, table| {
            let runtime = lua.runtime_mut();
            let function = api::stored_function(runtime, table, key);
            let arguments =
                crate::ext::json::from_json(runtime, arguments).map_err(|e| e.to_string())?;
            let arguments = match arguments {
                Value::Nil => Value::Table(runtime.create_table().map_err(|e| e.to_string())?),
                other => other,
            };
            let caller = api::caller_table(runtime, caller).map_err(|e| e.to_string())?;
            let values = call_returning(lua, function, vec![arguments, caller])?;
            outcome = match values.as_slice() {
                [] | [Value::Nil] => Ok(serde_json::Value::Null),
                [Value::Nil, message, ..] => Err(match message {
                    Value::String(text) => String::from_utf8_lossy(text.as_bytes()).into_owned(),
                    _ => "the tool failed without saying why".to_owned(),
                }),
                [value, ..] => Ok(crate::ext::json::to_json(lua.runtime_mut(), value)
                    .map_err(|e| format!("tool '{name}' returned {e}"))?),
            };
            lua.runtime_mut().recycle_result_vec(values);
            Ok(())
        });
        self.sync_commands();
        result.map(|()| outcome)
    }

    /// Whether view `id` (`<extension>.<name>`) is registered.
    #[must_use]
    pub fn has_view(&self, id: &str) -> bool {
        self.shared.borrow().views.iter().any(|v| v.id == id)
    }

    /// Calls the `render` function of view `id` with `key`. `None` for a
    /// view nobody registered, or a key its function returned `nil` for.
    ///
    /// # Errors
    ///
    /// Returns the function's Lua error, which also disables its extension.
    pub fn render_view(&mut self, id: &str, key: &str) -> Result<Option<Rendered>, ExtError> {
        let target = {
            let shared = self.shared.borrow();
            shared
                .views
                .iter()
                .position(|v| v.id == id)
                .map(|index| shared.view_targets[index])
        };
        let Some((slot, function_key)) = target else {
            return Ok(None);
        };
        let mut rendered = None;
        let result = self.guarded(slot, |lua, table| {
            let function = api::stored_function(lua.runtime_mut(), table, function_key);
            let values = call_returning(lua, function, vec![Value::from(key)])?;
            let runtime = lua.runtime_mut();
            rendered = match values.first() {
                None | Some(Value::Nil) => None,
                Some(Value::Table(page)) => {
                    let text = |field: &str| -> Result<String, String> {
                        match runtime.table_get(*page, &Value::from(field)) {
                            Ok(Value::String(text)) => {
                                Ok(String::from_utf8_lossy(text.as_bytes()).into_owned())
                            }
                            Ok(Value::Nil) => Ok(String::new()),
                            _ => Err(format!("view '{id}': '{field}' must be a string")),
                        }
                    };
                    Some(Rendered {
                        title: text("title")?,
                        markdown: text("markdown")?,
                    })
                }
                Some(Value::String(markdown)) => Some(Rendered {
                    title: String::new(),
                    markdown: String::from_utf8_lossy(markdown.as_bytes()).into_owned(),
                }),
                Some(_) => {
                    return Err(format!(
                        "view '{id}': render must return a table with 'title' and 'markdown'"
                    ));
                }
            };
            runtime.recycle_result_vec(values);
            Ok(())
        });
        self.sync_commands();
        result.map(|()| rendered)
    }

    /// Moves host actions requested by Lua since the last drain into `out`,
    /// in call order.
    pub fn drain_requests(&mut self, out: &mut Vec<Request>) {
        out.append(&mut self.shared.borrow_mut().requests);
    }

    /// Replaces the snapshot Lua reads through `term.context()`.
    pub fn set_context(&mut self, context: Context) {
        self.shared.borrow_mut().context = context;
    }

    fn load(
        &mut self,
        name: &str,
        path: Option<PathBuf>,
        module_dir: Option<&Path>,
        source: &[u8],
        permissions: Permissions,
        // The values of its settings, and what `term.http` may reach
        // besides its forges' accounts (`None` for every host).
        (settings, hosts): (Vec<(String, SettingValue)>, Option<Vec<String>>),
    ) -> Result<(), ExtError> {
        if name.is_empty() {
            return Err(ExtError::new(HOST, "extension name must not be empty"));
        }
        let slot = match self.extensions.iter().position(|ext| ext.info.name == name) {
            Some(index) if self.extensions[index].lua.is_some() => {
                return Err(ExtError::new(
                    name,
                    "an extension with this name is already loaded",
                ));
            }
            // A failed extension of the same name is replaced, so the app
            // can retry after the user fixes it.
            Some(index) => index,
            None => {
                self.extensions.push(Extension {
                    info: ExtensionInfo {
                        name: name.to_owned(),
                        path: None,
                        enabled: false,
                        last_error: None,
                        turned_off: false,
                        title: None,
                        description: None,
                        permissions,
                        missing: Permissions::NONE,
                        hosts: Vec::new(),
                        settings: Vec::new(),
                        logins: None,
                        default_on: true,
                    },
                    lua: None,
                    refs: None,
                    alarm: Alarm::default(),
                });
                self.extensions.len() - 1
            }
        };
        {
            let mut shared = self.shared.borrow_mut();
            if shared.slots.len() <= slot {
                shared.slots.resize_with(slot + 1, Default::default);
            }
            shared.clear_slot(slot);
            shared.slots[slot].settings = settings;
            shared.slots[slot].hosts = hosts;
        }
        let ext = &mut self.extensions[slot];
        ext.info.path = path;
        ext.info.last_error = None;
        ext.info.turned_off = false;
        ext.info.missing = Permissions::NONE;

        let chunk_name = match &ext.info.path {
            Some(path) => format!("@{}", path.display()),
            None => format!("={name}"),
        };
        let built = catch_unwind(AssertUnwindSafe(|| {
            build_vm(
                &self.config,
                &self.shared,
                slot,
                name,
                module_dir,
                &ext.alarm,
                permissions,
            )
        }))
        .unwrap_or_else(|_| Err("panic while creating the Lua VM".to_owned()));
        let (lua, refs) = match built {
            Ok(built) => built,
            Err(message) => {
                ext.info.enabled = false;
                ext.info.last_error = Some(message.clone());
                return Err(ExtError::new(name, message));
            }
        };
        ext.lua = Some(lua);
        ext.refs = Some(refs);
        ext.info.enabled = true;

        let result = self.guarded(slot, |lua, _| {
            let chunk = lua
                .compile(chunk_name.as_str(), source)
                .map_err(|e| e.to_string())?;
            lua.execute(&chunk)
                .map(|values| lua.runtime_mut().recycle_result_vec(values))
                .map_err(|e| e.to_string())
        });
        self.sync_commands();
        result
    }

    /// Runs `body` against slot's VM under the time budget and a panic
    /// guard. Any failure disables the extension.
    fn guarded(
        &mut self,
        slot: usize,
        body: impl FnOnce(&mut Lua, TableHandle) -> Result<(), String>,
    ) -> Result<(), ExtError> {
        let budget = self.config.time_budget;
        let ext = &mut self.extensions[slot];
        let (Some(lua), Some(refs)) = (ext.lua.as_mut(), ext.refs) else {
            return Err(ExtError::new(&ext.info.name, "extension is disabled"));
        };
        let timed = budget.is_some_and(|budget| self.watchdog.arm(budget, &ext.alarm));
        let outcome = catch_unwind(AssertUnwindSafe(|| body(lua, refs)));
        let expired = timed && self.watchdog.disarm();
        let message = if expired {
            // Reported even if the script swallowed the interrupt and returned.
            format!(
                "exceeded the time budget of {} ms",
                budget.unwrap_or_default().as_millis()
            )
        } else {
            match outcome {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(message)) => message,
                Err(_) => "panic inside the Lua VM".to_owned(),
            }
        };
        self.disable(slot, message.clone());
        Err(ExtError::new(&self.extensions[slot].info.name, message))
    }

    fn disable(&mut self, slot: usize, message: String) {
        let ext = &mut self.extensions[slot];
        ext.info.enabled = false;
        ext.info.last_error = Some(message);
        ext.lua = None;
        ext.refs = None;
        self.shared.borrow_mut().clear_slot(slot);
    }

    fn sync_commands(&mut self) {
        let mut shared = self.shared.borrow_mut();
        if shared.commands_dirty {
            shared.commands_dirty = false;
            self.commands.clone_from(&shared.commands);
            self.commands_revision += 1;
        }
        if shared.tasks_dirty {
            shared.tasks_dirty = false;
            self.tasks.clone_from(&shared.tasks);
            self.tasks_revision += 1;
        }
        if shared.tools_dirty {
            shared.tools_dirty = false;
            self.tools.clone_from(&shared.tools);
            self.tools_revision += 1;
        }
        let forges_changed = shared.forges_dirty;
        if shared.forges_dirty {
            shared.forges_dirty = false;
            self.forges.clone_from(&shared.forges);
            self.forges_revision += 1;
        }
        if shared.environments_dirty {
            shared.environments_dirty = false;
            self.environments.clone_from(&shared.environments);
            self.environments_revision += 1;
            cmdterm_env::set_custom_environments(self.owner, self.environments.clone());
        }
        if shared.agents_dirty {
            shared.agents_dirty = false;
            self.agents.clone_from(&shared.agents);
            self.agents_revision += 1;
        }
        if shared.sandbox_dirty || forges_changed {
            shared.sandbox_dirty = false;
            drop(shared);
            let shared = Rc::clone(&self.shared);
            self.merge_sandbox(&shared.borrow());
        }
    }

    /// Rebuilds [`Self::sandbox_rules`] from every extension's
    /// `term.sandbox` rules and the guards of every forge.
    fn merge_sandbox(&mut self, shared: &Shared) {
        let mut sandbox = SandboxRules::default();
        for slot in &shared.slots {
            sandbox.merge(&slot.sandbox);
        }
        for forge in &shared.forges {
            sandbox.merge(&SandboxRules {
                guards: forge.guard_rules(),
                ..SandboxRules::default()
            });
        }
        if sandbox != self.sandbox {
            self.sandbox = sandbox;
            self.sandbox_revision = next_sandbox_revision();
        }
    }
}

/// The extensions in `dir`: `(name, init file, module directory)` for every
/// `<name>/init.lua` and every `<name>.lua`. A missing directory has none.
fn find_extensions(
    dir: &Path,
    errors: &mut Vec<ExtError>,
) -> Vec<(String, PathBuf, Option<PathBuf>)> {
    let mut found = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return found,
        Err(error) => {
            errors.push(ExtError::new(
                HOST,
                format!("cannot read {}: {error}", dir.display()),
            ));
            return found;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if file_name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            let init = path.join("init.lua");
            if init.is_file() {
                found.push((file_name.to_owned(), init, Some(path.clone())));
            }
        } else if let Some(stem) = file_name.strip_suffix(".lua")
            && !stem.is_empty()
            // Teal's project file, which sits next to extensions written in Teal.
            && file_name != "tlconfig.lua"
        {
            found.push((stem.to_owned(), path.clone(), None));
        }
    }
    found
}

impl Drop for ExtensionHost {
    fn drop(&mut self) {
        if !self.environments.is_empty() {
            cmdterm_env::set_custom_environments(self.owner, Vec::new());
        }
    }
}

/// A distinct id per host, for [`cmdterm_env::set_custom_environments`].
fn next_owner() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn next_sandbox_revision() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Calls a stored function and returns its results.
fn call_returning(
    lua: &mut Lua,
    function: Value,
    arguments: Vec<Value>,
) -> Result<Vec<Value>, String> {
    if matches!(function, Value::Nil) {
        return Err("registered function was lost".to_owned());
    }
    lua.vm_mut()
        .call_value(function, arguments)
        .map_err(|e| e.to_string())
}

/// Calls a stored function and discards its results.
fn call(lua: &mut Lua, function: Value, arguments: Vec<Value>) -> Result<(), String> {
    if matches!(function, Value::Nil) {
        return Err("registered function was lost".to_owned());
    }
    let results = lua
        .vm_mut()
        .call_value(function, arguments)
        .map_err(|e| e.to_string())?;
    lua.runtime_mut().recycle_result_vec(results);
    Ok(())
}

fn build_vm(
    config: &Config,
    shared: &Rc<RefCell<Shared>>,
    slot: usize,
    name: &str,
    module_dir: Option<&Path>,
    alarm: &Alarm,
    permissions: Permissions,
) -> Result<(Lua, TableHandle), String> {
    let mut builder = Lua::builder()
        .profile(config.profile)
        .execution_mode(config.execution_mode)
        // Never install LuaJIT's `ffi`, even on the LuaJIT profile.
        .luajit_default_extensions(false);
    if config.profile == CompatibilityProfile::LuaJit21 {
        builder = builder
            .luajit_extension(LuaJitExtension::Bit)
            .luajit_extension(LuaJitExtension::Jit);
    }
    // Without `filesystem`, `require`, `loadfile` and `dofile` must read only
    // the extension's own directory. They read through this hook whatever
    // `package.path` says, so an extension cannot point `require` at the rest
    // of the disk by widening `package.path`.
    if !permissions.contains(Permission::Filesystem) {
        builder = builder.source_reader(scoped_source_reader(module_dir));
    }
    let mut lua = builder.build().map_err(|e| e.to_string())?;
    lua.vm_mut()
        .set_interrupt_flag(Arc::clone(&alarm.interrupt));
    let prefix = format!("[ext:{name}] ");
    let runtime = lua.runtime_mut();
    // Without writers the runtime would buffer output in memory forever.
    runtime.set_output_writer(PrefixWriter::new(&prefix, std::io::stderr()));
    runtime.set_error_output_writer(PrefixWriter::new(&prefix, std::io::stderr()));
    let refs = api::install(runtime, shared, slot, name, permissions, config)
        .map_err(|e| e.to_string())?;
    if let Some(dir) = module_dir {
        prepend_package_path(runtime, dir).map_err(|e| e.to_string())?;
    }
    restrict(&mut lua, module_dir, permissions)
        .map_err(|e| format!("cannot apply the permissions: {e}"))?;
    if config.time_budget.is_some() {
        api::protect(&mut lua, alarm).map_err(|e| format!("cannot install call wrappers: {e}"))?;
    }
    Ok((lua, refs))
}

/// Removes the Lua libraries behind the permissions `permissions` lacks:
/// file access without `filesystem` (then `require` finds only modules in
/// the extension's directory), processes and `debug` without `process`,
/// and `os.getenv` without `env`. `os` and `io` are replaced by copies, so
/// `package.loaded` holds no way back to the originals.
const RESTRICT: &str = r"
local fs, proc, env, own_path = ...
local function copy(t)
  local out = {}
  if type(t) == 'table' then
    for k, v in pairs(t) do out[k] = v end
  end
  return out
end
local new_os = copy(os)
local new_io = fs and copy(io) or nil
if not fs then
  dofile, loadfile = nil, nil
  new_os.remove, new_os.rename, new_os.tmpname = nil, nil, nil
  package.path = own_path
end
if not proc then
  new_os.execute = nil
  if new_io then new_io.popen = nil end
  package.loadlib, package.cpath = nil, ''
  debug = nil
  package.loaded.debug = nil
  if package.searchers then
    package.searchers = { package.searchers[1], package.searchers[2] }
  end
end
if not env then new_os.getenv = nil end
os, io = new_os, new_io
package.loaded.os, package.loaded.io = new_os, new_io
";

fn restrict(
    lua: &mut Lua,
    module_dir: Option<&Path>,
    permissions: Permissions,
) -> Result<(), oximond::Error> {
    let kept = [Permission::Filesystem, Permission::Process, Permission::Env];
    if kept.iter().all(|p| permissions.contains(*p)) {
        return Ok(());
    }
    let mut own_path = Vec::new();
    if let Some(dir) = module_dir {
        for pattern in ["?.lua", "?/init.lua"] {
            own_path.extend_from_slice(dir.join(pattern).to_string_lossy().as_bytes());
            own_path.push(b';');
        }
    }
    let chunk = lua.compile("=cmdterm.permissions", RESTRICT)?;
    let args = vec![
        Value::Boolean(permissions.contains(Permission::Filesystem)),
        Value::Boolean(permissions.contains(Permission::Process)),
        Value::Boolean(permissions.contains(Permission::Env)),
        Value::from(own_path),
    ];
    let values = lua.execute_with_arguments(&chunk, args)?;
    lua.runtime_mut().recycle_result_vec(values);
    Ok(())
}

/// The source reads a VM without `filesystem` may do: only files under the
/// extension's own directory. `require`, `loadfile` and `dofile` read through
/// this hook whatever `package.path` says, so widening `package.path` cannot
/// reach the rest of the disk. The path is resolved first, so a `..` segment
/// or a symlink cannot step outside the directory either. A single-file
/// extension, which has no directory, reads nothing.
fn scoped_source_reader(
    module_dir: Option<&Path>,
) -> impl Fn(Option<&[u8]>) -> std::io::Result<Vec<u8>> + 'static {
    let dir = module_dir.map(Path::to_path_buf);
    move |path| {
        let (Some(path), Some(dir)) = (path, dir.as_deref()) else {
            // Standard input, or a single-file extension with no directory.
            return Err(std::io::ErrorKind::NotFound.into());
        };
        let candidate = std::str::from_utf8(path).map_err(|_| std::io::ErrorKind::NotFound)?;
        let real = std::fs::canonicalize(candidate)?;
        let real_dir = std::fs::canonicalize(dir)?;
        if !real.starts_with(&real_dir) {
            return Err(std::io::ErrorKind::NotFound.into());
        }
        std::fs::read(real)
    }
}

/// Lets `require` find modules next to a directory extension's `init.lua`.
fn prepend_package_path(runtime: &mut oximond::Runtime, dir: &Path) -> oximond::RuntimeResult<()> {
    let Value::Table(package) = runtime.get_global("package") else {
        return Ok(());
    };
    let key = Value::from("path");
    let mut path = Vec::new();
    for pattern in ["?.lua", "?/init.lua"] {
        path.extend_from_slice(dir.join(pattern).to_string_lossy().as_bytes());
        path.push(b';');
    }
    if let Value::String(old) = runtime.table_get(package, &key)? {
        path.extend_from_slice(old.as_bytes());
    }
    runtime.table_set(package, &key, Value::from(path))
}

/// What an extension may reach besides its forges' accounts: its manifest's
/// `hosts`, the values of its `hosts` settings and the hosts of its `urls`
/// settings.
fn allowed_hosts(manifest: &Manifest, settings: &[(String, SettingValue)]) -> Vec<String> {
    let mut hosts = manifest.hosts.clone();
    for spec in &manifest.settings {
        let value = settings.iter().find(|(key, _)| *key == spec.key);
        let Some((_, SettingValue::List(listed))) = value else {
            continue;
        };
        for entry in listed {
            let host = match spec.kind {
                SettingKind::Hosts => Some(entry.trim().to_ascii_lowercase()),
                SettingKind::Urls => crate::ext::https_host(entry.trim()),
                _ => None,
            };
            if let Some(host) = host.filter(|host| !host.is_empty())
                && !hosts.contains(&host)
            {
                hosts.push(host);
            }
        }
    }
    hosts
}
