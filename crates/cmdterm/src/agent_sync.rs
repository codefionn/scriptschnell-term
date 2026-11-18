//! Coding agents' settings in bubblewrap sandboxes.
//!
//! The host's agents load their settings and run the hooks and MCP servers
//! named there, so a sandbox must not change them. But an agent in a
//! sandbox writes them too, such as when the user trusts a folder or
//! allows a tool. Each project's sandboxes therefore get a copy of every
//! file an agent lists in [`Agent::sandbox_synced`], bound over the
//! original or, for agents that rename a new file over it, in a directory
//! of their own ([`Agent::sandbox_home_var`]).
//!
//! A worker thread compares each copy with the host's file and with the
//! content both started from (the base), once a second:
//!
//! - Changes on the host go into the copy.
//! - Changes in the copy outside [`Agent::sandbox_guarded`] go to the host.
//! - Guarded changes go to the host once the user allows them, under the
//!   [`cmdterm_env::AGENT_SETTINGS_GUARD`]: Allow, Block, Allow in this
//!   session or Always allow in this project, as for a guarded request. A
//!   blocked change stays in the copy and is not asked about again until
//!   the app restarts.
//!
//! Changes are key paths with their new value, in JSON or TOML, so both
//! sides may change other keys meanwhile. TOML keeps its formatting and
//! comments.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, SystemTime};

use cmdterm_env::{AGENT_SETTINGS_GUARD, Approvals, GuardMode, GuardRule};

use crate::agents::Agent;

/// How often the worker compares the files.
const TICK: Duration = Duration::from_secs(1);

/// What a sandbox gets for the agents' settings, see [`AgentSync::prepare`].
#[derive(Debug, Default, PartialEq)]
pub struct Prepared {
    /// Copies to bind read-write over the host's files, as `(copy, file)`.
    pub binds: Vec<(PathBuf, PathBuf)>,
    /// Directories of their own the sandbox may write.
    pub dirs: Vec<PathBuf>,
    /// Variables that point agents to those directories.
    pub vars: Vec<(String, String)>,
}

/// The copies of every project's sandboxes, and the worker that syncs them.
pub struct AgentSync {
    inner: Arc<Inner>,
}

struct Inner {
    /// One directory per project id, with `copy/` and `base/` inside.
    dir: PathBuf,
    approvals: Arc<Approvals>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    files: Vec<Synced>,
    mode: GuardMode,
    worker: bool,
}

/// One synced file of one project.
#[derive(Clone, Debug)]
struct Synced {
    project: u64,
    agent: Agent,
    /// Relative to `$HOME`, as the agent lists it.
    file: &'static str,
    /// The host's file, its symbolic links resolved.
    host: PathBuf,
    copy: PathBuf,
    /// What the host's file held when the copy last caught up with it.
    base: PathBuf,
    /// [`Change::key`]s the user blocked.
    blocked: Vec<String>,
    /// A question about this file waits for the user.
    asking: bool,
    /// The host's and the copy's modification time and size at the last
    /// comparison.
    seen: Option<(Stamp, Stamp)>,
}

type Stamp = Option<(SystemTime, u64)>;

fn stamp(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

impl AgentSync {
    /// Copies live in `dir`; questions go to `approvals`.
    pub fn new(dir: PathBuf, approvals: Arc<Approvals>) -> Self {
        Self {
            inner: Arc::new(Inner {
                dir,
                approvals,
                state: Mutex::default(),
            }),
        }
    }

    /// The user's mode for [`AGENT_SETTINGS_GUARD`].
    pub fn set_mode(&self, mode: GuardMode) {
        self.inner.lock().mode = mode;
    }

    /// Sets up the copies of project `project` for a sandbox, for the
    /// agents' files that exist under `home` and that `skip` lets through,
    /// brings the host's latest changes into them, and starts the worker.
    pub fn prepare(&self, project: u64, home: &Path, skip: impl Fn(&Path) -> bool) -> Prepared {
        let mut prepared = Prepared::default();
        let root = self.inner.dir.join(project.to_string());
        for agent in Agent::all() {
            // Set on the host, the variable points elsewhere already.
            let own_dir =
                (agent.sandbox_home_var()).filter(|(var, _)| std::env::var_os(var).is_none());
            for file in agent.sandbox_synced() {
                let original = home.join(file);
                let Ok(host) = cmdterm_env::canonicalize(&original) else {
                    continue;
                };
                if !host.is_file() || skip(&original) || skip(&host) {
                    continue;
                }
                let copy = match own_dir {
                    Some((var, dir)) => {
                        let Ok(inside) = Path::new(file).strip_prefix(dir) else {
                            continue;
                        };
                        let shadow = root.join("copy").join(dir);
                        if !link_entries(&shadow, home, dir, agent, inside) {
                            continue;
                        }
                        let value = shadow.to_string_lossy().into_owned();
                        if !prepared.vars.iter().any(|(v, _)| v == var) {
                            prepared.vars.push((var.to_owned(), value));
                            prepared.dirs.push(shadow.clone());
                        }
                        shadow.join(inside)
                    }
                    None => root.join("copy").join(file),
                };
                let mut synced = Synced {
                    project,
                    agent,
                    file,
                    host: host.clone(),
                    copy: copy.clone(),
                    base: root.join("base").join(file),
                    blocked: Vec::new(),
                    asking: false,
                    seen: None,
                };
                if self.inner.register(&mut synced).is_err() {
                    continue;
                }
                if own_dir.is_none() {
                    prepared.binds.push((copy, host));
                }
            }
        }
        if !prepared.binds.is_empty() || !prepared.dirs.is_empty() {
            self.start();
        }
        prepared
    }

    /// Drops the copies of a removed project.
    pub fn forget(&self, project: u64) {
        self.inner.lock().files.retain(|f| f.project != project);
        let _ = std::fs::remove_dir_all(self.inner.dir.join(project.to_string()));
    }

    fn start(&self) {
        {
            let mut state = self.inner.lock();
            if state.worker {
                return;
            }
            state.worker = true;
        }
        let weak = Arc::downgrade(&self.inner);
        let _ = std::thread::Builder::new()
            .name("agent-sync".into())
            .spawn(move || work(weak));
    }

    /// One round of the worker, for tests.
    #[cfg(test)]
    fn tick(&self) {
        Inner::tick(&self.inner);
    }
}

fn work(inner: Weak<Inner>) {
    loop {
        std::thread::sleep(TICK);
        let Some(inner) = inner.upgrade() else {
            return;
        };
        Inner::tick(&inner);
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Creates the copy and its base on first use, then catches up with
    /// the host. A file the worker knows already keeps its blocked changes.
    fn register(&self, synced: &mut Synced) -> io::Result<()> {
        if !synced.copy.is_file() || !synced.base.is_file() {
            let text = std::fs::read(&synced.host)?;
            for path in [&synced.copy, &synced.base] {
                std::fs::create_dir_all(path.parent().unwrap_or(Path::new("/")))?;
                std::fs::write(path, &text)?;
            }
        }
        let mut state = self.lock();
        match (state.files.iter_mut())
            .find(|f| f.project == synced.project && f.copy == synced.copy)
        {
            Some(known) => {
                known.host = synced.host.clone();
                known.seen = None;
            }
            None => state.files.push(synced.clone()),
        }
        drop(state);
        // Nothing waits yet, so a guarded change from an earlier run asks
        // on the worker's first round.
        catch_up(synced).map(|_| ())
    }

    fn tick(self: &Arc<Self>) {
        let due: Vec<Synced> = {
            let mut state = self.lock();
            (state.files.iter_mut())
                .filter(|f| !f.asking)
                .filter_map(|f| {
                    let now = (stamp(&f.host), stamp(&f.copy));
                    (f.seen != Some(now)).then(|| {
                        f.seen = Some(now);
                        f.clone()
                    })
                })
                .collect()
        };
        for synced in due {
            let Ok(guarded) = catch_up(&synced) else {
                continue;
            };
            let guarded: Vec<Change> = (guarded.into_iter())
                .filter(|c| !synced.blocked.contains(&c.key()))
                .collect();
            if guarded.is_empty() {
                continue;
            }
            let mode = self.lock().mode;
            match mode {
                GuardMode::Allow => self.settle(&synced, &guarded, true),
                GuardMode::Block => self.settle(&synced, &guarded, false),
                GuardMode::Ask => self.ask(synced, guarded),
            }
        }
    }

    /// Asks about `changes` on a thread of its own, since the answer may
    /// take minutes.
    fn ask(self: &Arc<Self>, synced: Synced, changes: Vec<Change>) {
        self.update(&synced, |f| f.asking = true);
        let inner = Arc::clone(self);
        let guard = guard();
        let key = synced.clone();
        let spawned = std::thread::Builder::new()
            .name("agent-sync-ask".into())
            .spawn(move || {
                let detail = changes.iter().map(Change::line).collect::<Vec<_>>();
                let allow = inner.approvals.ask_about(
                    &guard,
                    Some(&synced.project.to_string()),
                    &format!("~/{}", synced.file),
                    synced.agent.name(),
                    "",
                    Some(detail.join("\n")),
                    None,
                );
                inner.settle(&synced, &changes, allow);
            });
        if spawned.is_err() {
            self.update(&key, |f| f.asking = false);
        }
    }

    /// Writes allowed `changes` to the host, or remembers blocked ones.
    fn settle(&self, synced: &Synced, changes: &[Change], allow: bool) {
        if allow {
            let _ = apply_to_host(synced, changes);
        }
        self.update(synced, |f| {
            if !allow {
                f.blocked.extend(changes.iter().map(Change::key));
            }
            f.asking = false;
            f.seen = None;
        });
    }

    fn update(&self, synced: &Synced, change: impl FnOnce(&mut Synced)) {
        let mut state = self.lock();
        let found =
            (state.files.iter_mut()).find(|f| f.project == synced.project && f.copy == synced.copy);
        if let Some(found) = found {
            change(found);
        }
    }
}

/// The guard questions go under.
fn guard() -> GuardRule {
    (cmdterm_env::builtin_guards().into_iter())
        .find(|guard| guard.id == AGENT_SETTINGS_GUARD)
        .unwrap_or_else(|| GuardRule {
            id: AGENT_SETTINGS_GUARD.to_owned(),
            title: AGENT_SETTINGS_GUARD.to_owned(),
            hosts: Vec::new(),
            requests: Vec::new(),
            mode: GuardMode::Ask,
            kind: None,
        })
}

/// One round for one file: the host's changes go into the copy, the
/// copy's unguarded changes go to the host. Returns the copy's guarded
/// changes, which wait for the user. A file that does not parse, such as
/// one written halfway, waits for the next round.
fn catch_up(synced: &Synced) -> io::Result<Vec<Change>> {
    let format = Format::of(synced.file);
    let host_text = std::fs::read_to_string(&synced.host)?;
    let base_text = std::fs::read_to_string(&synced.base).unwrap_or_else(|_| host_text.clone());
    let copy_text = std::fs::read_to_string(&synced.copy)?;
    let (Some(mut base), Some(host), Some(mut copy)) = (
        format.parse(&base_text),
        format.parse(&host_text),
        format.parse(&copy_text),
    ) else {
        return Ok(Vec::new());
    };
    if host_text != base_text {
        let changes = diff(&base, &host);
        if !changes.is_empty() {
            for change in &changes {
                copy.apply(change);
            }
            // In place: the sandbox binds this very file.
            std::fs::write(&synced.copy, copy.render(&copy_text))?;
        }
        write_atomic(&synced.base, host_text.as_bytes())?;
        base = host;
    }
    let (guarded, quiet): (Vec<Change>, Vec<Change>) =
        (diff(&base, &copy).into_iter()).partition(|change| {
            !change.is_empty()
                && is_guarded(synced.agent.sandbox_guarded(synced.file), &change.path)
        });
    if !quiet.is_empty() {
        apply_to_host(synced, &quiet)?;
    }
    Ok(guarded)
}

/// Applies `changes` to the host's file as it is now and makes that the
/// base.
fn apply_to_host(synced: &Synced, changes: &[Change]) -> io::Result<()> {
    let format = Format::of(synced.file);
    let text = std::fs::read_to_string(&synced.host)?;
    let mut doc = format
        .parse(&text)
        .ok_or_else(|| io::Error::other("the host's file does not parse"))?;
    for change in changes {
        doc.apply(change);
    }
    let text = doc.render(&text);
    // A file in a directory sandboxes may write is a mount point there:
    // renaming over it would take the copy or the read-only bind off and
    // leave the sandbox the new file to write. Only files in the home
    // directory sit on the sandbox's scratch home instead.
    if synced.file.contains('/') {
        std::fs::write(&synced.host, text.as_bytes())?;
    } else {
        write_atomic(&synced.host, text.as_bytes())?;
    }
    write_atomic(&synced.base, text.as_bytes())
}

/// Writes `path` through a temporary file beside it, keeping its
/// permissions, so a reader never sees half of it.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("/"));
    std::fs::create_dir_all(parent)?;
    let mut hasher = DefaultHasher::new();
    (path, std::process::id()).hash(&mut hasher);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = parent.join(format!(".{name}.{:x}.sync", hasher.finish()));
    std::fs::write(&temp, bytes)?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&temp, meta.permissions());
    }
    std::fs::rename(&temp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

/// Fills `shadow`, an agent's directory of its own, with links to every
/// entry of the host's `dir` (relative to `home`) but `own`, the copy. An
/// entry the sandbox created there, which the host lacks and the agent
/// does not keep read-only, moves to the host first, as if the sandbox had
/// written the host's directory. Returns whether the directory is ready.
fn link_entries(shadow: &Path, home: &Path, dir: &str, agent: Agent, own: &Path) -> bool {
    let Ok(real) = cmdterm_env::canonicalize(home.join(dir)) else {
        return false;
    };
    if std::fs::create_dir_all(shadow).is_err() {
        return false;
    }
    let protected = |name: &std::ffi::OsStr| {
        (agent.sandbox_read_only().iter())
            .chain(agent.sandbox_synced())
            .filter_map(|path| Path::new(path).strip_prefix(dir).ok())
            .any(|path| path.iter().next() == Some(name))
    };
    let dir = real;
    if let Ok(entries) = std::fs::read_dir(shadow) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let is_link = entry.file_type().is_ok_and(|t| t.is_symlink());
            if is_link || Path::new(&name) == own || protected(&name) {
                continue;
            }
            let target = dir.join(&name);
            if !target.exists() && std::fs::rename(entry.path(), &target).is_err() {
                continue;
            }
        }
    }
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if Path::new(&name) == own {
            continue;
        }
        let link = shadow.join(&name);
        let target = entry.path();
        match std::fs::read_link(&link) {
            Ok(current) if current == target => {}
            Ok(_) => {
                let _ = std::fs::remove_file(&link);
                let _ = symlink(&target, &link);
            }
            // The sandbox's own entry stays where it is.
            Err(_) if link.symlink_metadata().is_ok() => {}
            Err(_) => {
                let _ = symlink(&target, &link);
            }
        }
    }
    true
}

/// Bubblewrap sandboxes are Linux only, so other systems never get here.
fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(not(unix))]
    {
        let _ = (target, link);
        Err(io::ErrorKind::Unsupported.into())
    }
}

/// Whether a change at `path` touches one of `patterns`: lies at, inside
/// or above one.
fn is_guarded(patterns: &[&[&str]], path: &[String]) -> bool {
    (patterns.iter()).any(|pattern| {
        (pattern.iter())
            .zip(path)
            .all(|(want, key)| *want == "*" || want == key)
    })
}

// --- documents ---------------------------------------------------------------

#[derive(Clone, Copy)]
enum Format {
    Json,
    Toml,
}

impl Format {
    fn of(file: &str) -> Self {
        if file.ends_with(".toml") {
            Self::Toml
        } else {
            Self::Json
        }
    }

    fn parse(self, text: &str) -> Option<Doc> {
        match self {
            Self::Json if text.trim().is_empty() => Some(Doc::Json(serde_json::json!({}))),
            Self::Json => serde_json::from_str(text).ok().map(Doc::Json),
            Self::Toml => text.parse().ok().map(Doc::Toml),
        }
    }
}

enum Doc {
    Json(serde_json::Value),
    Toml(toml_edit::DocumentMut),
}

/// A key path and its new value, `None` when the key is removed.
#[derive(Clone, Debug)]
struct Change {
    path: Vec<String>,
    value: Option<Value>,
}

#[derive(Clone, Debug)]
enum Value {
    Json(serde_json::Value),
    Toml(toml_edit::Item),
}

impl Change {
    /// Sets an empty list or table, as agents do for every new project,
    /// which allows nothing yet.
    fn is_empty(&self) -> bool {
        match &self.value {
            Some(Value::Json(serde_json::Value::Array(items))) => items.is_empty(),
            Some(Value::Json(serde_json::Value::Object(map))) => map.is_empty(),
            Some(Value::Toml(item)) => {
                item.as_array().is_some_and(|a| a.is_empty())
                    || item.as_table_like().is_some_and(|t| t.is_empty())
            }
            _ => false,
        }
    }

    /// Identifies the change among others, to remember a blocked one.
    fn key(&self) -> String {
        self.line()
    }

    /// The change as the dialog gets it: the key path, then a tab and the
    /// new value unless the key is removed.
    fn line(&self) -> String {
        let path = path_label(&self.path);
        match &self.value {
            Some(Value::Json(value)) => format!("{path}\t{value}"),
            Some(Value::Toml(item)) => format!("{path}\t{}", toml_text(item).trim()),
            None => path,
        }
    }
}

/// `projects."/home/me/repo".trust_level`: keys joined by dots, quoted
/// where they hold more than letters, digits, `_` and `-`.
fn path_label(path: &[String]) -> String {
    let plain = |key: &str| {
        !key.is_empty()
            && key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    };
    (path.iter())
        .map(|key| {
            if plain(key) {
                key.clone()
            } else {
                serde_json::Value::String(key.clone()).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// What changed from `old` to `new`, key by key. A new table or object
/// counts as its keys, so each one is judged on its own.
fn diff(old: &Doc, new: &Doc) -> Vec<Change> {
    let mut changes = Vec::new();
    let mut path = Vec::new();
    match (old, new) {
        (Doc::Json(old), Doc::Json(new)) => diff_json(Some(old), new, &mut path, &mut changes),
        (Doc::Toml(old), Doc::Toml(new)) => {
            diff_toml(Some(old.as_item()), new.as_item(), &mut path, &mut changes);
        }
        _ => {}
    }
    changes
}

fn diff_json(
    old: Option<&serde_json::Value>,
    new: &serde_json::Value,
    path: &mut Vec<String>,
    changes: &mut Vec<Change>,
) {
    let old_map = old.and_then(serde_json::Value::as_object);
    if let Some(new_map) = new.as_object()
        && (old_map.is_some() || (old.is_none() && !new_map.is_empty()))
    {
        for (key, value) in new_map {
            path.push(key.clone());
            diff_json(old_map.and_then(|old| old.get(key)), value, path, changes);
            path.pop();
        }
        for key in old_map.into_iter().flat_map(|old| old.keys()) {
            if !new_map.contains_key(key) {
                path.push(key.clone());
                changes.push(Change {
                    path: path.clone(),
                    value: None,
                });
                path.pop();
            }
        }
    } else if old != Some(new) {
        changes.push(Change {
            path: path.clone(),
            value: Some(Value::Json(new.clone())),
        });
    }
}

fn diff_toml(
    old: Option<&toml_edit::Item>,
    new: &toml_edit::Item,
    path: &mut Vec<String>,
    changes: &mut Vec<Change>,
) {
    let old_table = old.and_then(toml_edit::Item::as_table_like);
    if let Some(table) = new.as_table_like()
        && (old_table.is_some() || (old.is_none() && !table.is_empty()))
    {
        for (key, value) in table.iter() {
            path.push(key.to_owned());
            diff_toml(old_table.and_then(|old| old.get(key)), value, path, changes);
            path.pop();
        }
        for (key, _) in old_table.into_iter().flat_map(|old| old.iter()) {
            if table.get(key).is_none_or(toml_edit::Item::is_none) {
                path.push(key.to_owned());
                changes.push(Change {
                    path: path.clone(),
                    value: None,
                });
                path.pop();
            }
        }
    } else if old.is_none_or(|old| toml_text(old) != toml_text(new)) {
        changes.push(Change {
            path: path.clone(),
            value: Some(Value::Toml(new.clone())),
        });
    }
}

/// An item as written, without the whitespace and comments around it.
fn toml_text(item: &toml_edit::Item) -> String {
    match item {
        toml_edit::Item::Value(value) => {
            let mut value = value.clone();
            value.decor_mut().clear();
            value.to_string()
        }
        other => other.to_string(),
    }
}

impl Doc {
    fn apply(&mut self, change: &Change) {
        match (self, &change.value) {
            (Doc::Json(root), Some(Value::Json(_)) | None) => apply_json(root, change),
            (Doc::Toml(doc), Some(Value::Toml(_)) | None) => apply_toml(doc, change),
            _ => {}
        }
    }

    /// The document as text, ending in a newline where `before` did.
    fn render(&self, before: &str) -> String {
        match self {
            Doc::Json(value) => {
                let mut text = serde_json::to_string_pretty(value).unwrap_or_default();
                if before.ends_with('\n') {
                    text.push('\n');
                }
                text
            }
            Doc::Toml(doc) => doc.to_string(),
        }
    }
}

fn apply_json(root: &mut serde_json::Value, change: &Change) {
    let value = match &change.value {
        Some(Value::Json(value)) => Some(value),
        _ => None,
    };
    let Some((last, parents)) = change.path.split_last() else {
        if let Some(value) = value {
            *root = value.clone();
        }
        return;
    };
    let mut node = root;
    for key in parents {
        if !node.is_object() {
            *node = serde_json::json!({});
        }
        node = (node.as_object_mut().expect("made an object above"))
            .entry(key.clone())
            .or_insert_with(|| serde_json::json!({}));
    }
    if !node.is_object() {
        *node = serde_json::json!({});
    }
    let map = node.as_object_mut().expect("made an object above");
    match value {
        Some(value) => {
            map.insert(last.clone(), value.clone());
        }
        None => {
            map.shift_remove(last);
        }
    }
}

fn implicit_table() -> toml_edit::Item {
    let mut table = toml_edit::Table::new();
    table.set_implicit(true);
    toml_edit::Item::Table(table)
}

/// A table for `item` to hold keys: itself when it is one, else a new one
/// of the kind its parent can hold.
fn as_table(item: &mut toml_edit::Item) -> &mut dyn toml_edit::TableLike {
    if item.as_table_like().is_none() {
        *item = if item.is_value() {
            toml_edit::Item::Value(toml_edit::InlineTable::new().into())
        } else {
            implicit_table()
        };
    }
    item.as_table_like_mut().expect("made a table above")
}

fn apply_toml(doc: &mut toml_edit::DocumentMut, change: &Change) {
    let Some((last, parents)) = change.path.split_last() else {
        return;
    };
    let mut item = doc.as_item_mut();
    for key in parents {
        let inline = item.is_inline_table();
        let table = as_table(item);
        if table.get(key).is_none_or(toml_edit::Item::is_none) {
            let fresh = if inline {
                toml_edit::Item::Value(toml_edit::InlineTable::new().into())
            } else {
                implicit_table()
            };
            table.insert(key, fresh);
        }
        item = table.get_mut(key).expect("inserted above");
    }
    let inline = item.is_inline_table();
    let table = as_table(item);
    match &change.value {
        Some(Value::Toml(value)) if inline => {
            if let Ok(value) = value.clone().into_value() {
                table.insert(last, toml_edit::Item::Value(value));
            }
        }
        Some(Value::Toml(value)) => {
            let mut value = value.clone();
            // The host's comment and spacing around the value stay.
            if let (Some(toml_edit::Item::Value(old)), toml_edit::Item::Value(new)) =
                (table.get(last), &mut value)
            {
                *new.decor_mut() = old.decor().clone();
            }
            // In place, so the key keeps its comments too.
            match table.get_mut(last) {
                Some(old) => *old = value,
                None => {
                    table.insert(last, value);
                }
            }
        }
        _ => {
            table.remove(last);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approvals() -> Arc<Approvals> {
        Arc::new(Approvals::new(|| {}))
    }

    fn home() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = cmdterm_env::canonicalize(dir.path()).unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        (dir, home, root.join("sync"))
    }

    fn read(path: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn counters_come_over_and_trust_waits_for_the_user() {
        let (_dir, home, sync_dir) = home();
        let host = home.join(".claude.json");
        std::fs::write(&host, "{\"numStartups\": 1, \"projects\": {}}\n").unwrap();
        let approvals = approvals();
        let sync = AgentSync::new(sync_dir.clone(), Arc::clone(&approvals));
        let prepared = sync.prepare(7, &home, |_| false);
        let copy = sync_dir.join("7/copy/.claude.json");
        assert_eq!(prepared.binds, [(copy.clone(), host.clone())]);
        assert!(prepared.vars.is_empty());
        // Inside, the agent counts a start and trusts the worktree.
        std::fs::write(
            &copy,
            r#"{"numStartups": 2, "projects": {"/w": {"hasTrustDialogAccepted": true, "lastCost": 1, "allowedTools": []}}}"#,
        )
        .unwrap();
        // Meanwhile the host's agent changes something else.
        std::fs::write(
            &host,
            "{\"numStartups\": 1, \"projects\": {}, \"theme\": \"dark\"}\n",
        )
        .unwrap();
        sync.tick();
        let host_now = read(&host);
        assert_eq!(host_now["numStartups"], 2);
        assert_eq!(host_now["theme"], "dark");
        assert_eq!(host_now["projects"]["/w"]["lastCost"], 1);
        assert_eq!(
            host_now["projects"]["/w"]["allowedTools"],
            serde_json::json!([])
        );
        assert!(
            host_now["projects"]["/w"]
                .get("hasTrustDialogAccepted")
                .is_none()
        );
        assert_eq!(read(&copy)["theme"], "dark");
        // The trust waits in a question.
        let question = loop {
            if let Some(q) = approvals.pending().into_iter().next() {
                break q;
            }
            std::thread::yield_now();
        };
        assert_eq!(question.guard, AGENT_SETTINGS_GUARD);
        assert_eq!(question.host, "~/.claude.json");
        assert_eq!(question.method, "Claude Code");
        assert_eq!(question.scope.as_deref(), Some("7"));
        assert_eq!(
            question.detail.as_deref(),
            Some("projects.\"/w\".hasTrustDialogAccepted\ttrue")
        );
        approvals.answer(question.id, true);
        while sync.inner.lock().files[0].asking {
            std::thread::yield_now();
        }
        assert_eq!(
            read(&host)["projects"]["/w"]["hasTrustDialogAccepted"],
            true
        );
        sync.tick();
        assert!(approvals.pending().is_empty());
    }

    #[test]
    fn a_blocked_change_stays_in_the_copy_and_asks_no_more() {
        let (_dir, home, sync_dir) = home();
        let host = home.join(".claude/settings.json");
        std::fs::write(&host, "{}").unwrap();
        let approvals = approvals();
        let sync = AgentSync::new(sync_dir.clone(), Arc::clone(&approvals));
        sync.set_mode(GuardMode::Block);
        sync.prepare(1, &home, |_| false);
        let copy = sync_dir.join("1/copy/.claude/settings.json");
        std::fs::write(&copy, r#"{"hooks": {"Stop": ["x"]}}"#).unwrap();
        sync.tick();
        assert_eq!(read(&host), serde_json::json!({}));
        assert_eq!(sync.inner.lock().files[0].blocked, ["hooks.Stop\t[\"x\"]"]);
        // The host's own changes still reach the copy.
        std::fs::write(&host, r#"{"model": "opus"}"#).unwrap();
        sync.tick();
        assert_eq!(
            read(&copy),
            serde_json::json!({"hooks": {"Stop": ["x"]}, "model": "opus"})
        );
        assert!(approvals.pending().is_empty());
        // Allowed, a change goes over without a question.
        sync.set_mode(GuardMode::Allow);
        std::fs::write(
            &copy,
            r#"{"hooks": {"Stop": ["x"]}, "model": "opus", "theme": "light"}"#,
        )
        .unwrap();
        sync.tick();
        assert_eq!(
            read(&host),
            serde_json::json!({"model": "opus", "theme": "light"})
        );
    }

    #[test]
    #[cfg_attr(
        not(unix),
        ignore = "the links are for bubblewrap sandboxes, which are Linux only"
    )]
    fn codex_gets_a_directory_with_links_and_its_own_config() {
        let (_dir, home, sync_dir) = home();
        let codex = home.join(".codex");
        std::fs::create_dir_all(codex.join("sessions")).unwrap();
        std::fs::write(codex.join("auth.json"), "{}").unwrap();
        std::fs::write(codex.join("hooks.json"), "{}").unwrap();
        let config = "# mine\nmodel = \"gpt\"\n";
        std::fs::write(codex.join("config.toml"), config).unwrap();
        if std::env::var_os("CODEX_HOME").is_some() {
            return;
        }
        let approvals = approvals();
        let sync = AgentSync::new(sync_dir.clone(), Arc::clone(&approvals));
        sync.set_mode(GuardMode::Allow);
        let shadow = sync_dir.join("3").join("copy").join(".codex");
        let prepared = sync.prepare(3, &home, |_| false);
        assert!(prepared.binds.is_empty());
        assert_eq!(prepared.dirs, std::slice::from_ref(&shadow));
        assert_eq!(
            prepared.vars,
            [(
                "CODEX_HOME".to_owned(),
                shadow.to_string_lossy().into_owned()
            )]
        );
        assert_eq!(
            std::fs::read_link(shadow.join("auth.json")).unwrap(),
            codex.join("auth.json")
        );
        assert!(shadow.join("sessions").is_dir());
        assert!(!shadow.join("config.toml").is_symlink());
        // Codex renames its new config over the copy and adds a log.
        let trusted = format!("{config}\n[projects.\"/w\"]\ntrust_level = \"trusted\"\n");
        std::fs::write(shadow.join("new.toml"), &trusted).unwrap();
        std::fs::rename(shadow.join("new.toml"), shadow.join("config.toml")).unwrap();
        std::fs::create_dir(shadow.join("log")).unwrap();
        // A rename over a link to a read-only file stays the sandbox's.
        std::fs::remove_file(shadow.join("hooks.json")).unwrap();
        std::fs::write(shadow.join("hooks.json"), "evil").unwrap();
        sync.tick();
        let host = std::fs::read_to_string(codex.join("config.toml")).unwrap();
        assert!(host.starts_with("# mine\nmodel = \"gpt\"\n"), "{host}");
        assert!(
            host.contains("[projects.\"/w\"]\ntrust_level = \"trusted\"\n"),
            "{host}"
        );
        // The next sandbox moves the log to the host and links it.
        sync.prepare(3, &home, |_| false);
        assert!(codex.join("log").is_dir());
        assert!(shadow.join("log").is_symlink());
        assert_eq!(
            std::fs::read_to_string(codex.join("hooks.json")).unwrap(),
            "{}"
        );
        assert_eq!(
            std::fs::read_to_string(shadow.join("hooks.json")).unwrap(),
            "evil"
        );
    }

    #[test]
    fn toml_changes_keep_the_hosts_formatting() {
        let base: toml_edit::DocumentMut = "a = 1\n".parse().unwrap();
        let copy: toml_edit::DocumentMut = "a = 2\n[t]\nb = [1, 2]\n".parse().unwrap();
        let changes = diff(&Doc::Toml(base), &Doc::Toml(copy));
        let lines: Vec<String> = changes.iter().map(Change::line).collect();
        assert_eq!(lines, ["a\t2", "t.b\t[1, 2]"]);
        let mut host = Doc::Toml("# keep\na = 1 # one\nz = \"x\"\n".parse().unwrap());
        for change in &changes {
            host.apply(change);
        }
        assert_eq!(
            host.render(""),
            "# keep\na = 2 # one\nz = \"x\"\n\n[t]\nb = [1, 2]\n"
        );
    }

    #[test]
    fn guarded_paths_match_at_inside_and_above() {
        let patterns: &[&[&str]] = &[&["projects", "*", "mcpServers"]];
        let path = |p: &[&str]| p.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert!(is_guarded(
            patterns,
            &path(&["projects", "/w", "mcpServers"])
        ));
        assert!(is_guarded(
            patterns,
            &path(&["projects", "/w", "mcpServers", "x"])
        ));
        assert!(is_guarded(patterns, &path(&["projects", "/w"])));
        assert!(!is_guarded(
            patterns,
            &path(&["projects", "/w", "lastCost"])
        ));
        assert!(!is_guarded(patterns, &path(&["numStartups"])));
        assert!(is_guarded(&[&[]], &path(&["anything"])));
    }
}
