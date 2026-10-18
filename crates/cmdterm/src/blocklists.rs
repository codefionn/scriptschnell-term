//! The blocklists extensions name with `term.sandbox{ blocklists }`. One
//! worker thread downloads them into the cache directory, tries a source's
//! mirrors when its first URL fails, and downloads a list again once the
//! cached copy is older than the source's `max_age`. A stale copy keeps
//! blocking while a new one cannot be downloaded.
//!
//! Nothing is downloaded until a sandbox uses the proxy: the runtime calls
//! [`Blocklists::want`] whenever it configures the proxy.

use std::cell::{OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime};

use crate::ext::BlocklistSource;
use cmdterm_env::Blocklist;
use schnellui::RedrawSignal;

/// How often the worker checks whether a cached list got too old, and
/// retries failed downloads.
const RECHECK: Duration = Duration::from_secs(60 * 60);

/// The largest download accepted.
const MAX_BYTES: u64 = 64 * 1024 * 1024;

type Fetch = dyn Fn(&str) -> Result<String, String> + Send + Sync;

#[derive(Default)]
struct Shared {
    /// Parsed lists by source id.
    loaded: Mutex<HashMap<String, Arc<Blocklist>>>,
    /// `(source id, error)` of downloads that failed, not yet shown.
    failures: Mutex<Vec<(String, String)>>,
    /// Bumped when `loaded` or `failures` change.
    revision: AtomicU64,
}

pub struct Blocklists {
    dir: PathBuf,
    shared: Arc<Shared>,
    wake: Arc<OnceLock<RedrawSignal>>,
    fetch: Arc<Fetch>,
    /// Started by the first [`Self::want`] that names a source.
    worker: OnceCell<Sender<Vec<BlocklistSource>>>,
    /// What the worker was given last.
    wanted: RefCell<Vec<BlocklistSource>>,
}

impl Blocklists {
    /// Blocklists cached in `dir`, downloaded over HTTPS.
    pub fn new(dir: PathBuf, wake: Arc<OnceLock<RedrawSignal>>) -> Self {
        Self::with_fetch(dir, wake, Arc::new(http_get))
    }

    fn with_fetch(dir: PathBuf, wake: Arc<OnceLock<RedrawSignal>>, fetch: Arc<Fetch>) -> Self {
        Self {
            dir,
            shared: Arc::default(),
            wake,
            fetch,
            worker: OnceCell::new(),
            wanted: RefCell::default(),
        }
    }

    /// Makes `sources` the lists to keep. Unchanged sources cost a
    /// comparison.
    pub fn want(&self, sources: &[BlocklistSource]) {
        if *self.wanted.borrow() == sources {
            return;
        }
        *self.wanted.borrow_mut() = sources.to_vec();
        let worker = self.worker.get_or_init(|| {
            let (sender, receiver) = mpsc::channel::<Vec<BlocklistSource>>();
            let (dir, shared, wake, fetch) = (
                self.dir.clone(),
                Arc::clone(&self.shared),
                Arc::clone(&self.wake),
                Arc::clone(&self.fetch),
            );
            let _ = thread::Builder::new()
                .name("cmdterm-blocklists".into())
                .spawn(move || {
                    let mut worker = Worker {
                        dir,
                        shared,
                        wake,
                        fetch,
                        seen: HashMap::new(),
                        failing: HashSet::new(),
                    };
                    let mut sources = Vec::new();
                    loop {
                        worker.refresh_all(&sources);
                        match receiver.recv_timeout(RECHECK) {
                            Ok(next) => sources = receiver.try_iter().last().unwrap_or(next),
                            Err(RecvTimeoutError::Timeout) => {}
                            Err(RecvTimeoutError::Disconnected) => return,
                        }
                    }
                });
            sender
        });
        let _ = worker.send(sources.to_vec());
    }

    /// Bumped whenever a list loads or a download fails.
    pub fn revision(&self) -> u64 {
        self.shared.revision.load(Ordering::Acquire)
    }

    /// The loaded lists of `sources`, in their order. Lists still
    /// downloading are left out.
    pub fn lists(&self, sources: &[BlocklistSource]) -> Vec<Arc<Blocklist>> {
        let loaded = self
            .shared
            .loaded
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        sources
            .iter()
            .filter_map(|source| loaded.get(&source.id).cloned())
            .collect()
    }

    /// Failed downloads since the last call, as `(source id, error)`.
    pub fn take_failures(&self) -> Vec<(String, String)> {
        std::mem::take(
            &mut *self
                .shared
                .failures
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        )
    }
}

struct Worker {
    dir: PathBuf,
    shared: Arc<Shared>,
    wake: Arc<OnceLock<RedrawSignal>>,
    fetch: Arc<Fetch>,
    /// The modification time of the cache file each loaded list was read from.
    seen: HashMap<String, SystemTime>,
    /// Sources whose last download failed and was reported.
    failing: HashSet<String>,
}

impl Worker {
    fn refresh_all(&mut self, sources: &[BlocklistSource]) {
        let mut changed = {
            let mut loaded = self
                .shared
                .loaded
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let before = loaded.len();
            loaded.retain(|id, _| sources.iter().any(|s| s.id == *id));
            self.seen
                .retain(|id, _| sources.iter().any(|s| s.id == *id));
            loaded.len() != before
        };
        for source in sources {
            changed |= self.refresh(source);
        }
        if changed {
            self.shared.revision.fetch_add(1, Ordering::Release);
            if let Some(signal) = self.wake.get() {
                signal.request_redraw();
            }
        }
    }

    /// Downloads `source` when its cached copy is missing or too old, and
    /// loads the copy when it changed. True when something changed.
    fn refresh(&mut self, source: &BlocklistSource) -> bool {
        let path = cache_path(&self.dir, source);
        let mut modified = modified(&path);
        // A time in the future counts as fresh.
        let fresh = modified.is_some_and(|time| {
            time.elapsed()
                .map_or(true, |age| age.as_secs() < source.max_age)
        });
        let mut changed = false;
        if !fresh {
            match download(source, &*self.fetch)
                .and_then(|(text, list)| store(&self.dir, &path, source, &text).map(|()| list))
            {
                Ok(list) => {
                    self.failing.remove(&source.id);
                    modified = self::modified(&path);
                    if let Some(time) = modified {
                        self.seen.insert(source.id.clone(), time);
                    }
                    self.insert(source, list);
                    return true;
                }
                Err(error) => {
                    if self.failing.insert(source.id.clone()) {
                        self.shared
                            .failures
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner())
                            .push((source.id.clone(), error));
                        changed = true;
                    }
                }
            }
        }
        let Some(modified) = modified else {
            return changed;
        };
        if self.seen.get(&source.id) == Some(&modified) {
            return changed;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            return changed;
        };
        self.seen.insert(source.id.clone(), modified);
        self.insert(source, Blocklist::parse(source.id.clone(), &text));
        true
    }

    fn insert(&self, source: &BlocklistSource, list: Blocklist) {
        self.shared
            .loaded
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(source.id.clone(), Arc::new(list));
    }
}

/// The first of `source`'s URLs that answers with a list of domains, as
/// its text and parsed. An answer without a single domain, such as an
/// error page, counts as a failure.
fn download(source: &BlocklistSource, fetch: &Fetch) -> Result<(String, Blocklist), String> {
    let mut errors = Vec::new();
    for url in &source.urls {
        match fetch(url) {
            Ok(text) => {
                let list = Blocklist::parse(source.id.clone(), &text);
                if !list.is_empty() {
                    return Ok((text, list));
                }
                errors.push(format!("{url}: no domains"));
            }
            Err(error) => errors.push(format!("{url}: {error}")),
        }
    }
    Err(errors.join("; "))
}

/// Writes `text` to `path` through a temporary file, and removes the copies
/// of `source` that other URLs left.
fn store(dir: &Path, path: &Path, source: &BlocklistSource, text: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let partial = path.with_extension("part");
    std::fs::write(&partial, text)
        .and_then(|()| std::fs::rename(&partial, path))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let prefix = format!("{}-", source.id);
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(&prefix) && entry.path() != path {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    Ok(())
}

/// `<id>-<hash of the first URL>.txt`, so a source that switches lists
/// does not take the old list's copy for its own.
fn cache_path(dir: &Path, source: &BlocklistSource) -> PathBuf {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in source.urls.first().map_or(&[][..], |url| url.as_bytes()) {
        h ^= u64::from(*byte);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    dir.join(format!("{}-{h:016x}.txt", source.id))
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn http_get(url: &str) -> Result<String, String> {
    let agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(Duration::from_secs(120)))
        .proxy(crate::forge::http_proxy())
        .tls_config(cmdterm_update::system_tls())
        .user_agent(concat!("scriptschnell-term/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent();
    let mut response = agent.get(url).call().map_err(|e| e.to_string())?;
    response
        .body_mut()
        .with_config()
        .limit(MAX_BYTES)
        .read_to_string()
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    fn source(urls: &[&str], max_age: u64) -> BlocklistSource {
        BlocklistSource {
            id: "blocklist.test".into(),
            urls: urls.iter().map(|u| (*u).to_owned()).collect(),
            max_age,
        }
    }

    fn wait_for(lists: &Blocklists, revision: u64) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while lists.revision() <= revision {
            assert!(Instant::now() < deadline, "the worker did not finish");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn mirrors_stand_in_and_the_cache_lasts_its_max_age() {
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let fetch: Arc<Fetch> = Arc::new(move |url: &str| {
            counted.fetch_add(1, Ordering::SeqCst);
            match url {
                "https://primary.example/list" => Err("connection refused".into()),
                "https://html.example/list" => Ok("<html>moved</html>".into()),
                _ => Ok("# list\nads.example\n".into()),
            }
        });
        let lists =
            Blocklists::with_fetch(dir.path().to_path_buf(), Arc::default(), Arc::clone(&fetch));
        let sources = [source(
            &[
                "https://primary.example/list",
                "https://html.example/list",
                "https://mirror.example/list",
            ],
            3600,
        )];
        lists.want(&sources);
        wait_for(&lists, 0);
        let loaded = lists.lists(&sources);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(lists.take_failures().is_empty());
        let cached = cache_path(dir.path(), &sources[0]);
        assert!(cached.is_file());

        // A fresh copy is read from the cache, not downloaded.
        let again = Blocklists::with_fetch(dir.path().to_path_buf(), Arc::default(), fetch);
        again.want(&sources);
        wait_for(&again, 0);
        assert_eq!(again.lists(&sources).len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    /// Downloads the shipped extension's list from both of its hosts.
    #[test]
    #[ignore = "needs the network"]
    fn the_shipped_list_downloads_from_github_and_codeberg() {
        for url in [
            "https://raw.githubusercontent.com/hagezi/dns-blocklists/main/wildcard/pro-onlydomains.txt",
            "https://codeberg.org/hagezi/mirror2/raw/branch/main/dns-blocklists/wildcard/pro-onlydomains.txt",
        ] {
            let list = Blocklist::parse("pro", &http_get(url).unwrap());
            assert!(list.len() > 100_000, "{url}: {}", list.len());
        }
    }

    #[test]
    fn a_stale_copy_keeps_blocking_when_downloads_fail() {
        let dir = tempfile::tempdir().unwrap();
        let sources = [source(&["https://down.example/list"], 1)];
        let cached = cache_path(dir.path(), &sources[0]);
        std::fs::write(&cached, "old.example\n").unwrap();
        let old = SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&cached)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let lists = Blocklists::with_fetch(
            dir.path().to_path_buf(),
            Arc::default(),
            Arc::new(|_: &str| Err("offline".to_owned())),
        );
        lists.want(&sources);
        wait_for(&lists, 0);
        assert_eq!(lists.lists(&sources).len(), 1);
        let failures = lists.take_failures();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, "blocklist.test");
        assert!(failures[0].1.contains("offline"), "{failures:?}");

        // Dropping the source drops its list.
        let revision = lists.revision();
        lists.want(&[]);
        wait_for(&lists, revision);
        assert!(lists.lists(&sources).is_empty());
    }
}
