//! Accounts, linked machines and browser sessions, in `store.json` of the
//! data directory. Every change writes the whole file through a temporary
//! file and a rename, mode 0600.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use cmdterm_remote::projects::SharedProject;
use cmdterm_remote::session::SessionInfo;
use serde::{Deserialize, Serialize};

use crate::now;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Data {
    #[serde(default)]
    pub users: Vec<User>,
    #[serde(default)]
    pub machines: Vec<Machine>,
    #[serde(default)]
    pub sessions: Vec<BrowserSession>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub name: String,
    /// Argon2id PHC string. `None` for accounts that only use single sign-on.
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub identities: Vec<Identity>,
    pub created: u64,
}

/// An account at a single sign-on provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// The `oidc` entry's id.
    pub provider: String,
    pub issuer: String,
    pub subject: String,
    #[serde(default)]
    pub email: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Machine {
    pub id: String,
    /// The user it belongs to.
    pub owner: String,
    pub name: String,
    /// Its host key line, which it logs in with.
    pub key: String,
    pub created: u64,
    #[serde(default)]
    pub last_seen: Option<u64>,
    /// What the machine answered last.
    #[serde(default)]
    pub projects: Vec<SharedProject>,
    #[serde(default)]
    pub sessions: Vec<SessionInfo>,
}

/// A logged-in browser.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrowserSession {
    /// SHA3-256 of the cookie's token, hex.
    pub hash: String,
    pub user: String,
    pub created: u64,
    pub last_used: u64,
}

/// How long a session lasts after its last use.
pub const SESSION_LIFETIME: u64 = 30 * 24 * 60 * 60;
/// `last_used` is written at most this often, so requests do not each
/// rewrite the file.
const TOUCH_EVERY: u64 = 60 * 60;

pub struct Store {
    path: PathBuf,
    data: Mutex<Data>,
}

impl Store {
    /// Opens `store.json` in `dir`, creating `dir` with mode 0700.
    pub fn open(dir: &Path) -> io::Result<Self> {
        cmdterm_remote::create_private_dir(dir)?;
        let path = dir.join("store.json");
        let data = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {error}", path.display()),
                )
            })?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Data::default(),
            Err(error) => return Err(error),
        };
        Ok(Self {
            path,
            data: Mutex::new(data),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Data> {
        self.data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn read<T>(&self, read: impl FnOnce(&Data) -> T) -> T {
        read(&self.lock())
    }

    /// Changes the data and saves it. The change stays in memory when the
    /// save fails, and the error says why.
    pub fn write<T>(&self, change: impl FnOnce(&mut Data) -> T) -> io::Result<T> {
        let mut data = self.lock();
        let result = change(&mut data);
        save(&self.path, &data)?;
        Ok(result)
    }

    pub fn user(&self, id: &str) -> Option<User> {
        self.read(|d| d.users.iter().find(|u| u.id == id).cloned())
    }

    pub fn user_by_name(&self, name: &str) -> Option<User> {
        self.read(|d| {
            d.users
                .iter()
                .find(|u| u.name.eq_ignore_ascii_case(name))
                .cloned()
        })
    }

    /// The user of the browser session whose token hashes to `hash`, if it
    /// has not expired.
    pub fn session_user(&self, hash: &str) -> Option<User> {
        let now = now();
        let (user, touch) = self.read(|d| {
            let session = d.sessions.iter().find(|s| s.hash == hash)?;
            if now.saturating_sub(session.last_used) > SESSION_LIFETIME {
                return None;
            }
            let user = d.users.iter().find(|u| u.id == session.user)?.clone();
            Some((user, now.saturating_sub(session.last_used) > TOUCH_EVERY))
        })?;
        if touch {
            let _ = self.write(|d| {
                d.sessions
                    .retain(|s| now.saturating_sub(s.last_used) <= SESSION_LIFETIME);
                if let Some(session) = d.sessions.iter_mut().find(|s| s.hash == hash) {
                    session.last_used = now;
                }
            });
        }
        Some(user)
    }

    pub fn machine(&self, id: &str) -> Option<Machine> {
        self.read(|d| d.machines.iter().find(|m| m.id == id).cloned())
    }

    /// The machine of `owner` with `id`; `None` for other users' machines.
    pub fn machine_of(&self, owner: &str, id: &str) -> Option<Machine> {
        self.machine(id).filter(|m| m.owner == owner)
    }

    pub fn machine_by_key(&self, key: &cmdterm_remote::keys::PublicKey) -> Option<Machine> {
        self.read(|d| {
            d.machines
                .iter()
                .find(|m| cmdterm_remote::keys::PublicKey::parse(&m.key).as_ref() == Some(key))
                .cloned()
        })
    }
}

fn save(path: &Path, data: &Data) -> io::Result<()> {
    use std::io::Write;
    let partial = path.with_extension("partial");
    let mut text = serde_json::to_vec_pretty(data).map_err(io::Error::other)?;
    text.push(b'\n');
    let mut file = cmdterm_remote::sys::mode(
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true),
        0o600,
    )
    .open(&partial)?;
    file.write_all(&text)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(partial, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_expire_and_data_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store
            .write(|d| {
                d.users.push(User {
                    id: "u1".into(),
                    name: "Alice".into(),
                    password: None,
                    identities: Vec::new(),
                    created: 0,
                });
                d.sessions.push(BrowserSession {
                    hash: "fresh".into(),
                    user: "u1".into(),
                    created: now(),
                    last_used: now(),
                });
                d.sessions.push(BrowserSession {
                    hash: "old".into(),
                    user: "u1".into(),
                    created: 0,
                    last_used: now() - SESSION_LIFETIME - 1,
                });
            })
            .unwrap();
        assert_eq!(store.session_user("fresh").unwrap().id, "u1");
        assert!(store.session_user("old").is_none());
        assert!(store.session_user("none").is_none());
        assert_eq!(store.user_by_name("alice").unwrap().id, "u1");

        let again = Store::open(dir.path()).unwrap();
        assert_eq!(again.read(|d| d.users.len()), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("store.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
