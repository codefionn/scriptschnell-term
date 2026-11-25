//! `term.store`: data an extension keeps across runs.
//!
//! Each extension's store is a directory with one JSON file per key. The
//! store reads the directory once, on first use, and answers from memory
//! after that. Writes change memory at once and reach the disk on a writer
//! thread, so a handler never waits for the disk. Stores are shared per
//! directory for the whole process, so a host that replaces another after a
//! reload sees writes the old one has not finished yet.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use serde_json::Value as Json;

/// Longest key, in bytes.
pub(crate) const MAX_KEY: usize = 200;
/// Largest value, as JSON.
pub(crate) const MAX_VALUE: usize = 4 * 1024 * 1024;
/// Most keys one extension keeps.
pub(crate) const MAX_KEYS: usize = 20_000;

/// One extension's data.
#[derive(Debug, Default)]
pub(crate) struct Store {
    /// `None` keeps the data in memory only.
    dir: Option<PathBuf>,
    loaded: bool,
    entries: BTreeMap<String, Json>,
}

pub(crate) type StoreHandle = Arc<Mutex<Store>>;

/// The store kept in `dir`, or one in memory for `None`.
pub(crate) fn open(dir: Option<PathBuf>) -> StoreHandle {
    let Some(dir) = dir else {
        return Arc::new(Mutex::new(Store {
            loaded: true,
            ..Store::default()
        }));
    };
    static STORES: OnceLock<Mutex<HashMap<PathBuf, StoreHandle>>> = OnceLock::new();
    let mut stores = lock(STORES.get_or_init(Mutex::default));
    stores
        .entry(dir.clone())
        .or_insert_with(|| {
            Arc::new(Mutex::new(Store {
                dir: Some(dir),
                ..Store::default()
            }))
        })
        .clone()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Whether `key` may name an entry: 1 to [`MAX_KEY`] bytes of letters,
/// digits, `.`, `_`, `-`, `/` and `:`.
pub(crate) fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_KEY
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/:".contains(&b))
}

impl Store {
    fn load(&mut self) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        let Some(dir) = &self.dir else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
                continue;
            };
            let Some(key) = decode(stem).filter(|key| valid_key(key)) else {
                continue;
            };
            match std::fs::read(entry.path())
                .map_err(|e| e.to_string())
                .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()))
            {
                Ok(value) => {
                    self.entries.insert(key, value);
                }
                Err(error) => crate::logs::error(
                    "extension store",
                    format!("skipping {}: {error}", entry.path().display()),
                ),
            }
        }
    }

    pub(crate) fn get(&mut self, key: &str) -> Option<Json> {
        self.load();
        self.entries.get(key).cloned()
    }

    /// Keys starting with `prefix`, in byte order.
    pub(crate) fn keys(&mut self, prefix: &str) -> Vec<String> {
        self.load();
        self.entries
            .range(prefix.to_owned()..)
            .take_while(|(key, _)| key.starts_with(prefix))
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// Stores `value` under `key`, or removes the key for `None`.
    pub(crate) fn set(&mut self, key: &str, value: Option<Json>) -> Result<(), String> {
        self.load();
        let bytes = match &value {
            Some(value) => {
                let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
                if bytes.len() > MAX_VALUE {
                    return Err(format!(
                        "the value is {} bytes; the most is {MAX_VALUE}",
                        bytes.len()
                    ));
                }
                if !self.entries.contains_key(key) && self.entries.len() >= MAX_KEYS {
                    return Err(format!("the store already holds {MAX_KEYS} keys"));
                }
                Some(bytes)
            }
            None => None,
        };
        match value {
            Some(value) => {
                self.entries.insert(key.to_owned(), value);
            }
            None => {
                if self.entries.remove(key).is_none() {
                    return Ok(());
                }
            }
        }
        if let Some(dir) = &self.dir {
            write(dir.join(format!("{}.json", encode(key))), bytes);
        }
        Ok(())
    }
}

/// A key as a file name: bytes other than letters, digits, `_` and `-`,
/// and a leading `.`, become `%XX`.
fn encode(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for (index, byte) in key.bytes().enumerate() {
        if byte.is_ascii_alphanumeric()
            || byte == b'_'
            || byte == b'-'
            || (byte == b'.' && index > 0)
        {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn decode(name: &str) -> Option<String> {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

enum Job {
    Write(PathBuf, Option<Vec<u8>>),
    Flush(Sender<()>),
}

fn writer() -> &'static Mutex<Sender<Job>> {
    static WRITER: OnceLock<Mutex<Sender<Job>>> = OnceLock::new();
    WRITER.get_or_init(|| {
        let (send, receive) = channel::<Job>();
        std::thread::Builder::new()
            .name("ext-store".into())
            .spawn(move || {
                for job in receive {
                    match job {
                        Job::Write(path, Some(bytes)) => {
                            if let Err(error) = write_now(&path, &bytes) {
                                crate::logs::error(
                                    "extension store",
                                    format!("cannot write {}: {error}", path.display()),
                                );
                            }
                        }
                        Job::Write(path, None) => match std::fs::remove_file(&path) {
                            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                                crate::logs::error(
                                    "extension store",
                                    format!("cannot remove {}: {error}", path.display()),
                                );
                            }
                            _ => {}
                        },
                        Job::Flush(done) => {
                            let _ = done.send(());
                        }
                    }
                }
            })
            .expect("cannot start the extension store writer");
        Mutex::new(send)
    })
}

fn write(path: PathBuf, bytes: Option<Vec<u8>>) {
    let _ = lock(writer()).send(Job::Write(path, bytes));
}

/// Writes through a temporary file, so a crash leaves the old value or the
/// new one, never half of one.
fn write_now(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, path)
}

/// Waits until every store write queued so far reached the disk. The app
/// calls it before it quits.
pub fn flush() {
    let (send, receive) = channel();
    if lock(writer()).send(Job::Flush(send)).is_ok() {
        let _ = receive.recv();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_round_trip_through_file_names() {
        for key in ["doc/notes:1", ".hidden", "a.b-c_d", "%"] {
            assert_eq!(decode(&encode(key)).as_deref(), Some(key));
        }
        assert!(!encode(".x").starts_with('.'));
        assert!(!encode("a/b").contains('/'));
        assert!(valid_key("doc/main/intro"));
        assert!(!valid_key(""));
        assert!(!valid_key("a b"));
        assert!(!valid_key(&"x".repeat(MAX_KEY + 1)));
    }

    #[test]
    fn values_persist_and_list_by_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ext");
        {
            let store = open(Some(path.clone()));
            let mut store = lock(&store);
            store.set("doc/a", Some(json!({ "title": "A" }))).unwrap();
            store.set("doc/b", Some(json!([1, 2]))).unwrap();
            store.set("other", Some(json!("x"))).unwrap();
            store.set("other", None).unwrap();
            assert_eq!(store.keys("doc/"), ["doc/a", "doc/b"]);
        }
        flush();
        let mut fresh = Store {
            dir: Some(path),
            ..Store::default()
        };
        assert_eq!(fresh.get("doc/a"), Some(json!({ "title": "A" })));
        assert_eq!(fresh.get("other"), None);
        assert_eq!(fresh.keys(""), ["doc/a", "doc/b"]);
    }
}
