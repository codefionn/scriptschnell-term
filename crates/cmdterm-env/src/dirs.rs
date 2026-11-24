//! Where the app and `scriptschnell-term-remote` keep their files, per platform.
//!
//! | Kind | Linux and BSD | macOS | Windows |
//! | --- | --- | --- | --- |
//! | config | `$XDG_CONFIG_HOME` or `~/.config` | `~/Library/Application Support` | `%APPDATA%` |
//! | data | `$XDG_DATA_HOME` or `~/.local/share` | `~/Library/Application Support` | `%LOCALAPPDATA%` |
//! | state | `$XDG_STATE_HOME` or `~/.local/state` | `~/Library/Application Support` | `%LOCALAPPDATA%` |
//! | cache | `$XDG_CACHE_HOME` or `~/.cache` | `~/Library/Caches` | `%LOCALAPPDATA%\…\cache` |
//! | logs | `<state>/logs` | `~/Library/Logs` | `<state>\logs` |
//!
//! Each gets a `scriptschnell-term` directory. `--config-dir` or
//! `$SCRIPTSCHNELL_TERM_CONFIG_DIR` puts all of them into that one
//! directory instead, which is how tests and scratch configurations stay
//! apart from the user's files.

use std::path::{Path, PathBuf};

/// The directories' name in each base directory.
const NAME: &str = "scriptschnell-term";
/// Its name before the app was renamed.
const OLD_NAME: &str = "cmdterm";

/// The directories of one installation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppDirs {
    /// What the user writes or changes: `settings.json`, their own
    /// extensions, the remote machine keys and `server.json`.
    pub config: PathBuf,
    /// What the app installs and manages: the references agents read, the
    /// shipped extensions, VM images and disks.
    pub data: PathBuf,
    /// What the app remembers between runs: `state.json`, sandbox shell
    /// histories, the proxy socket.
    pub state: PathBuf,
    /// What the app can download again: blocklists.
    pub cache: PathBuf,
    pub logs: PathBuf,
}

impl AppDirs {
    /// Every directory in `dir`, the layout of `--config-dir`.
    #[must_use]
    pub fn under(dir: &Path) -> Self {
        Self {
            config: dir.to_path_buf(),
            data: dir.to_path_buf(),
            state: dir.to_path_buf(),
            cache: dir.to_path_buf(),
            logs: dir.join("logs"),
        }
    }

    /// `explicit`, then `$SCRIPTSCHNELL_TERM_CONFIG_DIR` or the older
    /// `$CMDTERM_CONFIG_DIR`, as one directory for everything; else the
    /// platform's directories. A relative directory is made absolute against
    /// the current directory: sandboxes bind sockets from it, and bubblewrap
    /// resolves a relative source against its own working directory.
    #[must_use]
    pub fn resolve(explicit: Option<&Path>) -> Self {
        let absolute = |dir: &Path| std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
        if let Some(dir) = explicit {
            return Self::under(&absolute(dir));
        }
        for var in ["SCRIPTSCHNELL_TERM_CONFIG_DIR", "CMDTERM_CONFIG_DIR"] {
            if let Some(dir) = std::env::var_os(var) {
                return Self::under(&absolute(Path::new(&dir)));
            }
        }
        Self::platform()
    }

    /// True when every kind shares one directory, as with `--config-dir`.
    #[must_use]
    pub fn is_single(&self) -> bool {
        self.config == self.data && self.config == self.state && self.config == self.cache
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    fn platform() -> Self {
        let home = home();
        let base = |var: &str, fallback: &str| {
            std::env::var_os(var)
                .filter(|dir| Path::new(dir).is_absolute())
                .map_or_else(|| home.join(fallback), PathBuf::from)
                .join(NAME)
        };
        let state = base("XDG_STATE_HOME", ".local/state");
        Self {
            config: base("XDG_CONFIG_HOME", ".config"),
            data: base("XDG_DATA_HOME", ".local/share"),
            logs: state.join("logs"),
            state,
            cache: base("XDG_CACHE_HOME", ".cache"),
        }
    }

    #[cfg(target_os = "macos")]
    fn platform() -> Self {
        let library = home().join("Library");
        let support = library.join("Application Support").join(NAME);
        Self {
            config: support.clone(),
            data: support.clone(),
            state: support,
            cache: library.join("Caches").join(NAME),
            logs: library.join("Logs").join(NAME),
        }
    }

    #[cfg(windows)]
    fn platform() -> Self {
        let known = |var: &str, fallback: &str| {
            std::env::var_os(var)
                .map_or_else(|| home().join(fallback), PathBuf::from)
                .join(NAME)
        };
        let local = known("LOCALAPPDATA", "AppData\\Local");
        Self {
            config: known("APPDATA", "AppData\\Roaming"),
            data: local.clone(),
            cache: local.join("cache"),
            logs: local.join("logs"),
            state: local,
        }
    }

    /// Moves the files of older versions, which kept everything in
    /// `~/.config/scriptschnell-term` (before that `~/.config/cmdterm`),
    /// to where they belong now. A file already at its new place stays and
    /// the old one is left alone. Does nothing for a single directory.
    ///
    /// VM images and disks keep a symlink at their old path, because
    /// `server.json` holds absolute paths to them. The references and
    /// shipped extensions the app wrote there are the app's to clean up.
    pub fn migrate(&self) {
        if self.is_single() {
            return;
        }
        // Older versions used the XDG base on every platform.
        let legacy = std::env::var_os("XDG_CONFIG_HOME")
            .map_or_else(|| home().join(".config"), PathBuf::from);
        migrate_old_name(&legacy);
        let legacy = legacy.join(NAME);
        if legacy != self.config
            && std::fs::symlink_metadata(&legacy).is_ok_and(|meta| meta.is_dir())
            && !self.config.exists()
            && move_path(&legacy, &self.config)
        {
            leave_link(&self.config, &legacy);
        }
        let from = &self.config;
        move_into(&from.join("state.json"), &self.state.join("state.json"));
        move_into(&from.join("sandbox"), &self.state.join("sandbox"));
        move_into(&from.join("run"), &self.state.join("run"));
        move_into(&from.join("logs"), &self.logs);
        move_into(&from.join("blocklists"), &self.cache.join("blocklists"));
        for dir in ["images", "vms"] {
            let old = from.join("remote").join(dir);
            let new = self.data.join("remote").join(dir);
            if move_into(&old, &new) {
                leave_link(&new, &old);
            }
        }
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
}

/// Moves `base/cmdterm` to `base/scriptschnell-term` when only the old one
/// exists, and leaves a symlink at the old path, since files such as
/// `server.json` hold absolute paths into it.
fn migrate_old_name(base: &Path) {
    let new = base.join(NAME);
    let old = base.join(OLD_NAME);
    let is_real_dir = std::fs::symlink_metadata(&old).is_ok_and(|meta| meta.is_dir());
    if new.exists() || !is_real_dir {
        return;
    }
    if std::fs::rename(&old, &new).is_ok() {
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink(NAME, &old);
    }
}

/// Moves `old` to `new` when `old` is a real file or directory and `new`
/// does not exist yet. True when it moved.
fn move_into(old: &Path, new: &Path) -> bool {
    let real = std::fs::symlink_metadata(old).is_ok_and(|meta| !meta.file_type().is_symlink());
    real && !new.exists() && move_path(old, new)
}

/// Renames `old` to `new`, or copies and removes it when they are on
/// different file systems.
fn move_path(old: &Path, new: &Path) -> bool {
    if let Some(parent) = new.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return false;
    }
    if std::fs::rename(old, new).is_ok() {
        return true;
    }
    if copy_all(old, new).is_err() {
        let _ = std::fs::remove_dir_all(new).or_else(|_| std::fs::remove_file(new));
        return false;
    }
    let removed = if old.is_dir() {
        std::fs::remove_dir_all(old)
    } else {
        std::fs::remove_file(old)
    };
    removed.is_ok()
}

fn copy_all(from: &Path, to: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(std::fs::read_link(from)?, to);
        #[cfg(not(unix))]
        return Ok(());
    }
    if !meta.is_dir() {
        std::fs::copy(from, to)?;
        return Ok(());
    }
    std::fs::create_dir_all(to)?;
    std::fs::set_permissions(to, meta.permissions())?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        copy_all(&entry.path(), &to.join(entry.file_name()))?;
    }
    Ok(())
}

/// A symlink at `old` to `new`, for absolute paths other files still hold.
fn leave_link(new: &Path, old: &Path) {
    #[cfg(unix)]
    let _ = std::os::unix::fs::symlink(new, old);
    #[cfg(not(unix))]
    let _ = (new, old);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_old_directory_moves_once_and_stays_reachable() {
        let base = tempfile::tempdir().unwrap();
        let old = base.path().join(OLD_NAME);
        std::fs::create_dir_all(old.join("remote")).unwrap();
        std::fs::write(old.join("settings.json"), "{}").unwrap();
        migrate_old_name(base.path());
        let new = base.path().join(NAME);
        assert_eq!(
            std::fs::read_to_string(new.join("settings.json")).unwrap(),
            "{}"
        );
        #[cfg(unix)]
        assert!(
            old.join("remote").is_dir(),
            "the old path leads to the new one"
        );
        // Nothing happens once the new directory exists.
        std::fs::write(new.join("state.json"), "{}").unwrap();
        migrate_old_name(base.path());
        assert!(new.join("state.json").is_file());
    }

    #[test]
    fn a_fresh_machine_gets_no_old_directory() {
        let base = tempfile::tempdir().unwrap();
        migrate_old_name(base.path());
        assert!(!base.path().join(OLD_NAME).exists());
        assert!(!base.path().join(NAME).exists());
    }

    #[test]
    fn an_explicit_directory_holds_everything() {
        let root = Path::new(if cfg!(windows) {
            r"C:\scratch\cfg"
        } else {
            "/scratch/cfg"
        });
        let dirs = AppDirs::resolve(Some(root));
        assert!(dirs.is_single());
        assert_eq!(dirs.state, root);
        assert_eq!(dirs.logs, root.join("logs"));
    }

    #[test]
    fn a_relative_directory_becomes_absolute() {
        let dirs = AppDirs::resolve(Some(Path::new("cfg")));
        assert!(dirs.config.is_absolute());
        assert!(dirs.config.ends_with("cfg"));
    }

    #[test]
    fn files_of_the_single_directory_move_to_their_kinds() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path();
        let dirs = AppDirs {
            config: root.join("config"),
            data: root.join("data"),
            state: root.join("state"),
            cache: root.join("cache"),
            logs: root.join("state/logs"),
        };
        let config = &dirs.config;
        std::fs::create_dir_all(config.join("remote/images")).unwrap();
        std::fs::create_dir_all(config.join("logs")).unwrap();
        std::fs::create_dir_all(config.join("sandbox/3")).unwrap();
        std::fs::write(config.join("state.json"), "{\"projects\":[]}").unwrap();
        std::fs::write(config.join("settings.json"), "{}").unwrap();
        std::fs::write(config.join("logs/a.log"), "log").unwrap();
        std::fs::write(config.join("remote/images/alpine.ext4"), "disk").unwrap();
        std::fs::write(config.join("remote/id_hybrid"), "key").unwrap();
        // A state.json at the new place wins over the old one.
        std::fs::create_dir_all(&dirs.state).unwrap();
        std::fs::write(dirs.state.join("state.json"), "{}").unwrap();
        dirs.migrate();
        assert_eq!(
            std::fs::read_to_string(dirs.state.join("state.json")).unwrap(),
            "{}"
        );
        assert!(config.join("state.json").is_file(), "left alone");
        assert!(config.join("settings.json").is_file());
        assert!(config.join("remote/id_hybrid").is_file());
        assert_eq!(
            std::fs::read_to_string(dirs.logs.join("a.log")).unwrap(),
            "log"
        );
        assert!(dirs.state.join("sandbox/3").is_dir());
        assert!(!config.join("sandbox").exists());
        assert_eq!(
            std::fs::read_to_string(dirs.data.join("remote/images/alpine.ext4")).unwrap(),
            "disk"
        );
        #[cfg(unix)]
        assert_eq!(
            std::fs::read_to_string(config.join("remote/images/alpine.ext4")).unwrap(),
            "disk",
            "server.json's old paths still lead to the image"
        );
        // A second start finds nothing to move.
        dirs.migrate();
        assert!(dirs.data.join("remote/images/alpine.ext4").is_file());
    }
}
