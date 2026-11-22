//! Per-project environment variables backed by the operating system's secret
//! storage (Secret Service on Linux via `secret-tool`, Login Keychain on macOS
//! via `security`) when available, with a fallback to `state.json` (mode 0600
//! on Unix) when no secret storage is reachable.
//!
//! Each project with keyring-backed variables keeps one entry in the secret
//! storage, keyed by [`crate::workspace::project_storage_id`], holding a
//! single-line JSON object of `{name: value}` pairs. One entry per project
//! means one keyring lookup per project instead of one per variable, and JSON
//! escapes newlines so multi-line values satisfy `os_secrets::store`'s
//! one-line rule.
//!
//! All calls to the secret storage run in FIFO order on a lazy worker thread,
//! because a locked keyring may wait for the user's password. The UI thread
//! never blocks except for a brief grace window on startup when the active
//! project has keyring-backed variables so unlocked desktop keyrings populate
//! before the first terminal opens.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::state::{AppState, Project, ProjectEnvVar};
use crate::workspace::project_storage_id;
use schnellui::RedrawSignal;

/// The secret storage service name for per-project environment variables.
pub const SERVICE: &str = "scriptschnell-term-project-env";

/// How long startup waits for the active project's unlocked keyring entry
/// before opening the window anyway. An unlocked keyring typically answers in
/// a few milliseconds; a locked one times out here and finishes asynchronously.
pub const STARTUP_WAIT: Duration = Duration::from_millis(200);

/// Returns whether `name` is a valid portable environment variable name:
/// non-empty, starting with an ASCII letter or `_`, followed by ASCII letters,
/// digits or `_`.
pub fn valid_var_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Operating system secret storage operations, abstracted so tests can verify
/// both keyring and fallback paths without touching the desktop keyring.
pub trait SecretBackend: Send + Sync {
    fn available(&self) -> bool;
    fn lookup(&self, service: &str, account: &str) -> io::Result<Option<String>>;
    fn store(&self, label: &str, service: &str, account: &str, secret: &str) -> io::Result<()>;
    fn remove(&self, service: &str, account: &str) -> io::Result<()>;
}

/// Real secret storage backed by `cmdterm_env::os_secrets`.
pub struct OsSecretBackend {
    enabled: bool,
}

impl OsSecretBackend {
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }
}

impl SecretBackend for OsSecretBackend {
    fn available(&self) -> bool {
        self.enabled && cmdterm_env::os_secrets::available()
    }

    fn lookup(&self, service: &str, account: &str) -> io::Result<Option<String>> {
        cmdterm_env::os_secrets::lookup(service, account)
    }

    fn store(&self, label: &str, service: &str, account: &str, secret: &str) -> io::Result<()> {
        cmdterm_env::os_secrets::store(label, service, account, secret)
    }

    fn remove(&self, service: &str, account: &str) -> io::Result<()> {
        cmdterm_env::os_secrets::remove(service, account)
    }
}

/// Where one project's keyring lookup stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LoadState {
    #[default]
    Idle,
    Loading,
    Loaded,
    Failed,
}

/// How one project environment variable is currently stored, for the UI row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VarStorage {
    /// Stored in the system keyring and loaded in memory.
    Keyring,
    /// Stored in the system keyring, still loading on the worker thread.
    Loading,
    /// Stored in the system keyring, but could not be read (locked or missing).
    Locked,
    /// Stored directly in `state.json` (keyring was unavailable or failed).
    Plain,
}

enum Cmd {
    Load {
        project_id: u64,
        account: String,
    },
    Set {
        project_id: u64,
        seq: u64,
        account: String,
        label: String,
        name: String,
        value: String,
    },
    Remove {
        account: String,
        label: String,
        name: String,
    },
    RemoveProject {
        account: String,
    },
}

/// Completion reported from the worker thread to the UI thread.
pub enum Done {
    Loaded {
        project_id: u64,
        result: Result<Option<BTreeMap<String, String>>, String>,
    },
    Saved {
        project_id: u64,
        seq: u64,
        name: String,
        value: String,
        result: Result<(), String>,
    },
    Removed,
    ProjectRemoved,
}

/// Outcome of applying one [`Done`] event to [`ProjectEnvStore`].
pub enum Applied {
    Loaded {
        project_id: u64,
        found_any: bool,
    },
    SavedInKeyring {
        name: String,
    },
    /// Storing in the keyring failed; if the sequence number still matches the
    /// latest write for `(project_id, name)`, the caller falls back to saving
    /// `value` in `state.json`.
    SaveFallback {
        project_id: u64,
        name: String,
        value: String,
        error: String,
    },
    Removed,
    Ignored,
}

/// Live in-memory cache and FIFO secret-storage worker for per-project
/// environment variables.
pub struct ProjectEnvStore {
    backend: Arc<dyn SecretBackend>,
    /// Decrypted values currently known in memory, per project id.
    values: HashMap<u64, BTreeMap<String, String>>,
    /// Keyring load state per project id.
    loads: HashMap<u64, LoadState>,
    /// Latest write sequence number per `(project_id, var_name)` so an older
    /// failed keyring write never overwrites a newer write's state.
    seqs: HashMap<(u64, String), u64>,
    next_seq: u64,
    /// Lazy worker command sender; spawned on first keyring operation.
    tx: Option<Sender<Cmd>>,
    done_tx: Sender<Done>,
    done_rx: Receiver<Done>,
    wake: Arc<OnceLock<RedrawSignal>>,
    /// Number of commands sent to the worker whose replies have not been
    /// drained yet. Idle polls check this integer and do no work when zero.
    pending: usize,
}

impl ProjectEnvStore {
    pub fn new(backend: Arc<dyn SecretBackend>, wake: Arc<OnceLock<RedrawSignal>>) -> Self {
        let (done_tx, done_rx) = mpsc::channel();
        Self {
            backend,
            values: HashMap::new(),
            loads: HashMap::new(),
            seqs: HashMap::new(),
            next_seq: 0,
            tx: None,
            done_tx,
            done_rx,
            wake,
            pending: 0,
        }
    }

    pub fn available(&self) -> bool {
        self.backend.available()
    }

    /// True while any keyring command is in flight. Zero allocations.
    pub fn has_pending(&self) -> bool {
        self.pending > 0
    }

    /// Replaces the secret backend. Used by tests to exercise keyring and
    /// fallback paths deterministically.
    #[cfg(test)]
    pub fn set_backend(&mut self, backend: Arc<dyn SecretBackend>) {
        self.tx = None;
        self.backend = backend;
        self.values.clear();
        self.loads.clear();
        self.seqs.clear();
        self.pending = 0;
        while self.done_rx.try_recv().is_ok() {}
    }

    fn send(&mut self, cmd: Cmd) {
        let tx = self.tx.get_or_insert_with(|| {
            let (tx, rx) = mpsc::channel();
            let backend = Arc::clone(&self.backend);
            let done = self.done_tx.clone();
            let wake = Arc::clone(&self.wake);
            let _ = std::thread::Builder::new()
                .name("project-env".into())
                .spawn(move || worker_loop(rx, backend, done, wake));
            tx
        });
        if tx.send(cmd).is_ok() {
            self.pending += 1;
        }
    }

    /// Starts loading keyring-backed variables for every project in `state`
    /// that has any, queuing the active project first.
    pub fn start_initial_loads(&mut self, state: &AppState) {
        if !self.available() {
            return;
        }
        let active = state.active_project;
        if let Some(project) = active.and_then(|id| state.project(id))
            && project.has_keyring_env_vars()
        {
            self.load_project(project);
        }
        for project in &state.projects {
            if Some(project.id) != active && project.has_keyring_env_vars() {
                self.load_project(project);
            }
        }
    }

    /// Queues a keyring lookup for `project`.
    pub fn load_project(&mut self, project: &Project) {
        if project.temporary || !self.available() {
            return;
        }
        let account = account_key(project);
        self.loads.insert(project.id, LoadState::Loading);
        self.send(Cmd::Load {
            project_id: project.id,
            account,
        });
    }

    /// Waits up to `timeout` for `project_id`'s keyring load to finish,
    /// applying any completed replies that arrive meanwhile.
    pub fn wait_for_project(&mut self, project_id: u64, timeout: Duration) {
        if self.loads.get(&project_id) != Some(&LoadState::Loading) {
            return;
        }
        let deadline = Instant::now() + timeout;
        while self.loads.get(&project_id) == Some(&LoadState::Loading) && self.pending > 0 {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            match self.done_rx.recv_timeout(remaining) {
                Ok(done) => {
                    self.pending = self.pending.saturating_sub(1);
                    let _ = self.apply_done(done);
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    self.pending = 0;
                    break;
                }
            }
        }
    }

    /// Records `(name, value)` in memory and queues a keyring store when
    /// `project` can use the keyring. Returns `true` if the value is being
    /// saved to the keyring (caller keeps `ProjectEnvVar::value = None`), or
    /// `false` if the caller should store `value` in `state.json` directly.
    pub fn set_var(&mut self, project: &Project, name: &str, value: &str) -> bool {
        self.values
            .entry(project.id)
            .or_default()
            .insert(name.to_owned(), value.to_owned());
        self.next_seq += 1;
        let seq = self.next_seq;
        self.seqs.insert((project.id, name.to_owned()), seq);
        if project.temporary || !self.available() {
            return false;
        }
        let account = account_key(project);
        let label = secret_label(project);
        self.send(Cmd::Set {
            project_id: project.id,
            seq,
            account,
            label,
            name: name.to_owned(),
            value: value.to_owned(),
        });
        true
    }

    /// Removes `name` from memory and, if it was keyring-backed on a kept
    /// project, queues removing it from the keyring.
    pub fn remove_var(&mut self, project: &Project, name: &str, was_in_keyring: bool) {
        if let Some(map) = self.values.get_mut(&project.id) {
            map.remove(name);
        }
        self.seqs.remove(&(project.id, name.to_owned()));
        if !was_in_keyring || project.temporary || !self.available() {
            return;
        }
        let account = account_key(project);
        let label = secret_label(project);
        self.send(Cmd::Remove {
            account,
            label,
            name: name.to_owned(),
        });
    }

    /// Drops cached values for `project` and removes its keyring entry if it
    /// had keyring-backed variables.
    pub fn forget_project(&mut self, project: &Project) {
        self.values.remove(&project.id);
        self.loads.remove(&project.id);
        self.seqs.retain(|(id, _), _| *id != project.id);
        if project.temporary || !project.has_keyring_env_vars() || !self.available() {
            return;
        }
        self.send(Cmd::RemoveProject {
            account: account_key(project),
        });
    }

    /// Drains all completed worker replies currently queued without blocking.
    pub fn drain(&mut self) -> Vec<Applied> {
        if self.pending == 0 {
            return Vec::new();
        }
        let mut applied = Vec::new();
        while let Ok(done) = self.done_rx.try_recv() {
            self.pending = self.pending.saturating_sub(1);
            applied.push(self.apply_done(done));
        }
        applied
    }

    fn apply_done(&mut self, done: Done) -> Applied {
        match done {
            Done::Loaded { project_id, result } => match result {
                Ok(Some(map)) => {
                    let found_any = !map.is_empty();
                    let entry = self.values.entry(project_id).or_default();
                    // Keep any value written in memory while the load ran.
                    for (key, value) in map {
                        entry.entry(key).or_insert(value);
                    }
                    self.loads.insert(project_id, LoadState::Loaded);
                    Applied::Loaded {
                        project_id,
                        found_any,
                    }
                }
                Ok(None) => {
                    self.loads.insert(project_id, LoadState::Failed);
                    Applied::Loaded {
                        project_id,
                        found_any: false,
                    }
                }
                Err(_) => {
                    self.loads.insert(project_id, LoadState::Failed);
                    Applied::Loaded {
                        project_id,
                        found_any: false,
                    }
                }
            },
            Done::Saved {
                project_id,
                seq,
                name,
                value,
                result,
            } => {
                let current = self.seqs.get(&(project_id, name.clone())).copied();
                if current != Some(seq) {
                    return Applied::Ignored;
                }
                match result {
                    Ok(()) => Applied::SavedInKeyring { name },
                    Err(error) => Applied::SaveFallback {
                        project_id,
                        name,
                        value,
                        error,
                    },
                }
            }
            Done::Removed | Done::ProjectRemoved => Applied::Removed,
        }
    }

    /// Resolved `(name, value)` pairs for `project`, in the order saved on
    /// `project.env_vars`. Keyring-backed variables that have not loaded yet
    /// are omitted.
    pub fn resolve_project(&self, project: &Project) -> Vec<(String, String)> {
        if project.env_vars.is_empty() {
            return Vec::new();
        }
        let cached = self.values.get(&project.id);
        let mut out = Vec::with_capacity(project.env_vars.len());
        for var in &project.env_vars {
            if let Some(value) = &var.value {
                out.push((var.name.clone(), value.clone()));
            } else if let Some(value) = cached.and_then(|map| map.get(&var.name)) {
                out.push((var.name.clone(), value.clone()));
            }
        }
        out
    }

    /// Storage status of `var` in `project`, for display in the Project page.
    pub fn var_storage(&self, project: &Project, var: &ProjectEnvVar) -> VarStorage {
        if var.value.is_some() {
            return VarStorage::Plain;
        }
        if self
            .values
            .get(&project.id)
            .is_some_and(|map| map.contains_key(&var.name))
        {
            return VarStorage::Keyring;
        }
        if self.loads.get(&project.id) == Some(&LoadState::Loading) {
            return VarStorage::Loading;
        }
        VarStorage::Locked
    }

    /// True when `project` has at least one keyring-backed variable whose
    /// value is not currently loaded (and is not still loading).
    pub fn has_locked_vars(&self, project: &Project) -> bool {
        project
            .env_vars
            .iter()
            .any(|var| self.var_storage(project, var) == VarStorage::Locked)
    }
}

/// Combines project-level and worktree-level environment variables, with
/// worktree values taking precedence over project values of the same name.
pub fn merge_env_vars(
    project_vars: Vec<(String, String)>,
    worktree_vars: &[(String, String)],
) -> Vec<(String, String)> {
    if project_vars.is_empty() {
        return worktree_vars.to_vec();
    }
    if worktree_vars.is_empty() {
        return project_vars;
    }
    let mut merged = project_vars;
    for (key, value) in worktree_vars {
        if let Some(existing) = merged.iter_mut().find(|(k, _)| k == key) {
            existing.1 = value.clone();
        } else {
            merged.push((key.clone(), value.clone()));
        }
    }
    merged
}

/// The stable keyring account key for `project`.
pub fn account_key(project: &Project) -> String {
    project_storage_id(&project.name, &project.env, &project.root)
}

fn secret_label(project: &Project) -> String {
    format!("scriptschnell-term: {} environment variables", project.name)
}

fn parse_map(raw: &str) -> Result<BTreeMap<String, String>, String> {
    serde_json::from_str(raw).map_err(|error| error.to_string())
}

fn worker_loop(
    rx: Receiver<Cmd>,
    backend: Arc<dyn SecretBackend>,
    done: Sender<Done>,
    wake: Arc<OnceLock<RedrawSignal>>,
) {
    let mut maps: HashMap<String, BTreeMap<String, String>> = HashMap::new();
    while let Ok(cmd) = rx.recv() {
        let reply = match cmd {
            Cmd::Load {
                project_id,
                account,
            } => {
                let result = match backend.lookup(SERVICE, &account) {
                    Ok(Some(raw)) => match parse_map(&raw) {
                        Ok(map) => {
                            maps.insert(account, map.clone());
                            Ok(Some(map))
                        }
                        Err(error) => Err(error),
                    },
                    Ok(None) => Ok(None),
                    Err(error) => Err(error.to_string()),
                };
                Done::Loaded { project_id, result }
            }
            Cmd::Set {
                project_id,
                seq,
                account,
                label,
                name,
                value,
            } => {
                let mut next = maps.get(&account).cloned().unwrap_or_else(|| {
                    backend
                        .lookup(SERVICE, &account)
                        .ok()
                        .flatten()
                        .and_then(|raw| parse_map(&raw).ok())
                        .unwrap_or_default()
                });
                next.insert(name.clone(), value.clone());
                let result = serde_json::to_string(&next)
                    .map_err(|error| error.to_string())
                    .and_then(|json| {
                        backend
                            .store(&label, SERVICE, &account, &json)
                            .map_err(|error| error.to_string())
                    });
                if result.is_ok() {
                    maps.insert(account, next);
                }
                Done::Saved {
                    project_id,
                    seq,
                    name,
                    value,
                    result,
                }
            }
            Cmd::Remove {
                account,
                label,
                name,
            } => {
                let mut next = maps.get(&account).cloned().unwrap_or_else(|| {
                    backend
                        .lookup(SERVICE, &account)
                        .ok()
                        .flatten()
                        .and_then(|raw| parse_map(&raw).ok())
                        .unwrap_or_default()
                });
                next.remove(&name);
                let result = if next.is_empty() {
                    backend
                        .remove(SERVICE, &account)
                        .map_err(|error| error.to_string())
                } else {
                    serde_json::to_string(&next)
                        .map_err(|error| error.to_string())
                        .and_then(|json| {
                            backend
                                .store(&label, SERVICE, &account, &json)
                                .map_err(|error| error.to_string())
                        })
                };
                if result.is_ok() {
                    if next.is_empty() {
                        maps.remove(&account);
                    } else {
                        maps.insert(account, next);
                    }
                }
                Done::Removed
            }
            Cmd::RemoveProject { account } => {
                let _ = backend.remove(SERVICE, &account);
                maps.remove(&account);
                Done::ProjectRemoved
            }
        };
        let _ = done.send(reply);
        if let Some(signal) = wake.get() {
            signal.request_redraw();
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// In-memory secret storage for tests.
    #[derive(Default)]
    pub(crate) struct MemoryBackend {
        pub available: bool,
        pub fail_store: bool,
        pub entries: Mutex<HashMap<(String, String), String>>,
    }

    impl MemoryBackend {
        pub(crate) fn available() -> Arc<Self> {
            Arc::new(Self {
                available: true,
                fail_store: false,
                entries: Mutex::new(HashMap::new()),
            })
        }

        pub(crate) fn unavailable() -> Arc<Self> {
            Arc::new(Self {
                available: false,
                fail_store: false,
                entries: Mutex::new(HashMap::new()),
            })
        }

        pub(crate) fn failing_store() -> Arc<Self> {
            Arc::new(Self {
                available: true,
                fail_store: true,
                entries: Mutex::new(HashMap::new()),
            })
        }
    }

    impl SecretBackend for MemoryBackend {
        fn available(&self) -> bool {
            self.available
        }

        fn lookup(&self, service: &str, account: &str) -> io::Result<Option<String>> {
            Ok(self
                .entries
                .lock()
                .unwrap()
                .get(&(service.to_owned(), account.to_owned()))
                .cloned())
        }

        fn store(
            &self,
            _label: &str,
            service: &str,
            account: &str,
            secret: &str,
        ) -> io::Result<()> {
            if self.fail_store {
                return Err(io::Error::other("keyring locked"));
            }
            assert!(
                !secret.contains(['\n', '\r']),
                "secret must be one line for os_secrets::store"
            );
            self.entries
                .lock()
                .unwrap()
                .insert((service.to_owned(), account.to_owned()), secret.to_owned());
            Ok(())
        }

        fn remove(&self, service: &str, account: &str) -> io::Result<()> {
            self.entries
                .lock()
                .unwrap()
                .remove(&(service.to_owned(), account.to_owned()));
            Ok(())
        }
    }

    fn drain_all(store: &mut ProjectEnvStore) -> Vec<Applied> {
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while store.has_pending() && Instant::now() < deadline {
            out.extend(store.drain());
            if store.has_pending() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        assert!(!store.has_pending(), "worker did not finish in time");
        out
    }

    fn demo_project() -> Project {
        let mut state = AppState::default();
        let id = state.add_project(
            "demo".into(),
            cmdterm_env::Environment::Local,
            PathBuf::from("/tmp/demo"),
            crate::vcs::VcsKind::Git,
            None,
        );
        state.project(id).unwrap().clone()
    }

    #[test]
    fn variable_name_validation_accepts_portable_names_only() {
        assert!(valid_var_name("API_KEY"));
        assert!(valid_var_name("_private_1"));
        assert!(valid_var_name("a"));
        assert!(!valid_var_name(""));
        assert!(!valid_var_name("1ABC"));
        assert!(!valid_var_name("FOO=BAR"));
        assert!(!valid_var_name("FOO-BAR"));
        assert!(!valid_var_name("FOO BAR"));
        assert!(!valid_var_name("FOO\0BAR"));
    }

    #[test]
    fn keyring_stores_and_reloads_single_and_multiline_values() {
        let backend = MemoryBackend::available();
        let wake = Arc::new(OnceLock::new());
        let mut store = ProjectEnvStore::new(backend.clone(), wake.clone());
        let mut project = demo_project();

        assert!(store.set_var(&project, "API_TOKEN", "s3cr3t"));
        assert!(store.set_var(&project, "CERT_PEM", "line1\nline2\r\nline3"));
        project.env_vars = vec![
            ProjectEnvVar {
                name: "API_TOKEN".into(),
                value: None,
            },
            ProjectEnvVar {
                name: "CERT_PEM".into(),
                value: None,
            },
        ];
        let applied = drain_all(&mut store);
        assert_eq!(applied.len(), 2);
        assert!(
            applied
                .iter()
                .all(|a| matches!(a, Applied::SavedInKeyring { .. }))
        );

        // A fresh store after restart loads all values from the keyring.
        let mut reloaded = ProjectEnvStore::new(backend.clone(), wake);
        reloaded.load_project(&project);
        reloaded.wait_for_project(project.id, Duration::from_secs(2));
        assert_eq!(
            reloaded.resolve_project(&project),
            vec![
                ("API_TOKEN".into(), "s3cr3t".into()),
                ("CERT_PEM".into(), "line1\nline2\r\nline3".into()),
            ]
        );
        assert_eq!(
            reloaded.var_storage(&project, &project.env_vars[0]),
            VarStorage::Keyring
        );
    }

    #[test]
    fn removing_last_keyring_var_deletes_the_keyring_entry() {
        let backend = MemoryBackend::available();
        let mut store = ProjectEnvStore::new(backend.clone(), Arc::new(OnceLock::new()));
        let project = demo_project();
        let account = account_key(&project);

        assert!(store.set_var(&project, "ONE", "1"));
        assert!(store.set_var(&project, "TWO", "2"));
        drain_all(&mut store);
        assert!(
            backend
                .entries
                .lock()
                .unwrap()
                .contains_key(&(SERVICE.to_owned(), account.clone()))
        );

        store.remove_var(&project, "ONE", true);
        drain_all(&mut store);
        assert!(
            backend
                .entries
                .lock()
                .unwrap()
                .contains_key(&(SERVICE.to_owned(), account.clone()))
        );

        store.remove_var(&project, "TWO", true);
        drain_all(&mut store);
        assert!(
            !backend
                .entries
                .lock()
                .unwrap()
                .contains_key(&(SERVICE.to_owned(), account))
        );
    }

    #[test]
    fn setting_var_before_load_merges_with_existing_keyring_entries() {
        let backend = MemoryBackend::available();
        let project = demo_project();
        let account = account_key(&project);
        backend
            .store("label", SERVICE, &account, r#"{"EXISTING":"keep"}"#)
            .unwrap();

        let mut store = ProjectEnvStore::new(backend.clone(), Arc::new(OnceLock::new()));
        assert!(store.set_var(&project, "ADDED", "new"));
        drain_all(&mut store);

        let raw = backend.lookup(SERVICE, &account).unwrap().unwrap();
        let parsed: BTreeMap<String, String> = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.get("EXISTING").map(String::as_str), Some("keep"));
        assert_eq!(parsed.get("ADDED").map(String::as_str), Some("new"));
    }

    #[test]
    fn store_failure_triggers_fallback_only_for_latest_sequence() {
        let backend = MemoryBackend::failing_store();
        let mut store = ProjectEnvStore::new(backend, Arc::new(OnceLock::new()));
        let project = demo_project();

        assert!(store.set_var(&project, "TOKEN", "first"));
        assert!(store.set_var(&project, "TOKEN", "second"));
        let applied = drain_all(&mut store);
        assert!(matches!(applied[0], Applied::Ignored));
        assert!(matches!(
            &applied[1],
            Applied::SaveFallback { name, value, .. } if name == "TOKEN" && value == "second"
        ));
    }

    #[test]
    fn worktree_vars_override_project_vars_without_duplicates() {
        let merged = merge_env_vars(
            vec![
                ("A".into(), "project-a".into()),
                ("B".into(), "project-b".into()),
            ],
            &[
                ("B".into(), "worktree-b".into()),
                ("C".into(), "worktree-c".into()),
            ],
        );
        assert_eq!(
            merged,
            vec![
                ("A".into(), "project-a".into()),
                ("B".into(), "worktree-b".into()),
                ("C".into(), "worktree-c".into()),
            ]
        );
    }
}
