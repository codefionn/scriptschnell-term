//! Files the app keeps in its data directory: the shipped extensions, the
//! extension reference with its LuaLS and Teal type files, and the
//! configuration reference. Agents read them through the
//! `configure-scriptschnell-term` skill (see `agents.rs`). The user's own
//! extensions directory, in the configuration directory, gets the editor
//! settings that point lua-language-server and Teal at the type files.
//!
//! Every start writes what is missing and updates what changed in this
//! version. `.bundled.json` in the data directory records the hash of each
//! file as last written, so a file the user edited, or deleted, stays as the
//! user left it.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use cmdterm_env::AppDirs;

/// This run's directories, set by [`install`].
static DIRS: OnceLock<AppDirs> = OnceLock::new();

/// This run's directories: `--config-dir` or the platform's.
pub fn dirs() -> AppDirs {
    DIRS.get()
        .cloned()
        .unwrap_or_else(|| AppDirs::resolve(None))
}

/// The record of what [`install`] wrote, in the data directory.
const RECORD: &str = ".bundled.json";

/// The editor settings in the user's extensions directory, relative to the
/// configuration directory.
const LUARC: &str = "extensions/.luarc.json";
const TLCONFIG: &str = "extensions/tlconfig.lua";

/// The type files' directory as the user's extensions directory reaches
/// it: next to it in one directory, else in the data directory.
fn types_dir(dirs: &AppDirs) -> String {
    if dirs.config == dirs.data {
        "types".to_owned()
    } else {
        dirs.data
            .join("extensions/types")
            .to_string_lossy()
            .into_owned()
    }
}

fn luarc(dirs: &AppDirs) -> String {
    let types = serde_json::to_string(&types_dir(dirs)).unwrap_or_default();
    format!(
        r#"{{
  "$schema": "https://raw.githubusercontent.com/LuaLS/vscode-lua/master/setting/schema.json",
  "runtime.version": "Lua 5.4",
  "workspace.library": [{types}]
}}
"#
    )
}

fn tlconfig(dirs: &AppDirs) -> String {
    let types = types_dir(dirs);
    format!(
        r#"-- Teal settings for the extensions in this directory, written by
-- scriptschnell-term. Run `tl gen <name>/init.tl` here; the app loads the
-- generated init.lua.
return {{
  include_dir = {{ {types:?} }},
  global_env_def = "term",
}}
"#
    )
}

macro_rules! extension {
    ($name:literal) => {
        [
            (
                concat!("extensions/", $name, "/init.lua"),
                include_str!(concat!("../../../extensions/", $name, "/init.lua")),
            ),
            (
                concat!("extensions/", $name, "/manifest.toml"),
                include_str!(concat!("../../../extensions/", $name, "/manifest.toml")),
            ),
            (
                concat!("extensions/", $name, "/README.md"),
                include_str!(concat!("../../../extensions/", $name, "/README.md")),
            ),
        ]
    };
}

/// The shipped extensions, by directory name.
pub const EXTENSIONS: [&str; 20] = [
    "aspnet-devcerts",
    "azure-devops",
    "bitbucket",
    "blocklist",
    "cargo",
    "claude-code",
    "coder",
    "dotfiles",
    "forge-clis",
    "forgejo",
    "git-credentials",
    "gitea",
    "github",
    "gitlab",
    "knowledge",
    "kubernetes",
    "npm",
    "nuget",
    "pypi",
    "toolchains",
];

/// Every file of the data directory, as (relative path, content).
pub fn files() -> impl Iterator<Item = (&'static str, &'static str)> {
    const DOCS: [(&str, &str); 4] = [
        ("CONFIGURATION.md", include_str!("../CONFIGURATION.md")),
        ("extensions/README.md", include_str!("ext/README.md")),
        (
            "extensions/types/term.lua",
            include_str!("ext/types/term.lua"),
        ),
        (
            "extensions/types/term.d.tl",
            include_str!("ext/types/term.d.tl"),
        ),
    ];
    const SHIPPED: [[(&str, &str); 3]; 20] = [
        extension!("aspnet-devcerts"),
        extension!("azure-devops"),
        extension!("bitbucket"),
        extension!("blocklist"),
        extension!("cargo"),
        extension!("claude-code"),
        extension!("coder"),
        extension!("dotfiles"),
        extension!("forge-clis"),
        extension!("forgejo"),
        extension!("git-credentials"),
        extension!("gitea"),
        extension!("github"),
        extension!("gitlab"),
        extension!("knowledge"),
        extension!("kubernetes"),
        extension!("npm"),
        extension!("nuget"),
        extension!("pypi"),
        extension!("toolchains"),
    ];
    DOCS.into_iter().chain(SHIPPED.into_iter().flatten())
}

/// What each shipped extension may use without a grant: the permissions its
/// shipped `manifest.toml` declares. A manifest the user widened waits for a
/// grant of the rest, and an explicit entry in `extension_permissions`, even
/// an empty one, replaces this default.
pub fn default_grants() -> &'static BTreeMap<String, Vec<String>> {
    static GRANTS: OnceLock<BTreeMap<String, Vec<String>>> = OnceLock::new();
    GRANTS.get_or_init(|| {
        files()
            .filter_map(|(path, content)| {
                let name = path
                    .strip_prefix("extensions/")?
                    .strip_suffix("/manifest.toml")?;
                let manifest = crate::ext::Manifest::parse(content).ok()?;
                Some((name.to_owned(), manifest.permissions.names()))
            })
            .collect()
    })
}

/// The shipped extension whose settings page lists one of the proxy's own
/// guards ([`cmdterm_env::builtin_guards`]), by guard id. Guards without
/// one, or whose extension is not installed, stay on the app's own page.
pub fn guard_extension(guard_id: &str) -> Option<&'static str> {
    Some(match guard_id {
        "cmdterm.git-push" => "git-credentials",
        "cmdterm.npm-publish" => "npm",
        "cmdterm.cargo-publish" => "cargo",
        "cmdterm.nuget-push" => "nuget",
        "cmdterm.pypi-upload" => "pypi",
        _ => return None,
    })
}

/// Whether this build loads extensions besides the shipped ones (the
/// `third-party-extensions` feature). Without it the app loads only
/// [`EXTENSIONS`] from the data directory, never the user's own
/// `extensions/`, and the control socket refuses extension changes.
pub const THIRD_PARTY_EXTENSIONS: bool = cfg!(feature = "third-party-extensions");

/// True for an extension this app ships, by name.
pub fn is_shipped(name: &str) -> bool {
    EXTENSIONS.contains(&name)
}

/// The paths under the data directory that agents in a sandbox may read:
/// the references and the shipped extensions. The user's own extensions,
/// `extensions/` in the configuration directory, are readable too.
pub const READABLE: [&str; 2] = ["CONFIGURATION.md", "extensions"];

/// FNV-1a, stable across builds and platforms.
fn hash(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        h ^= u64::from(*byte);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// The extension directory a path belongs to, such as `extensions/npm/`.
fn extension_dir(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("extensions/")?;
    let end = rest.find('/')?;
    Some(&path[..="extensions/".len() + end])
}

fn write(path: &Path, content: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut temp = path.as_os_str().to_owned();
    temp.push(".new");
    std::fs::write(&temp, content)?;
    std::fs::rename(&temp, path)
}

/// Writes missing files and updates the ones this app wrote and nobody
/// changed since. Returns one message per file it could not write.
pub fn install(dirs: &AppDirs) -> Vec<String> {
    let _ = DIRS.set(dirs.clone());
    let record_path = dirs.data.join(RECORD);
    let mut record: BTreeMap<String, String> = read_record(&record_path).unwrap_or_default();
    let before = record.clone();
    let mut errors = Vec::new();
    if !dirs.is_single() {
        leave_the_config_dir(dirs, &mut record, &mut errors);
    }
    let editor = [(LUARC, luarc(dirs)), (TLCONFIG, tlconfig(dirs))];
    let all = files()
        .map(|(relative, content)| (dirs.data.as_path(), relative, content))
        .chain(
            (editor.iter())
                .map(|(relative, content)| (dirs.config.as_path(), *relative, content.as_str())),
        );
    let mut shipped = Vec::new();
    for (root, relative, content) in all {
        shipped.push(relative);
        let target = root.join(relative);
        let fresh = hash(content.as_bytes());
        let write_it = match std::fs::read(&target) {
            Ok(current) if current == content.as_bytes() => {
                record.insert(relative.to_owned(), fresh);
                continue;
            }
            // Ours and untouched: update it.
            Ok(current) => record.get(relative) == Some(&hash(&current)),
            // Deleted by the user, or a whole extension they removed.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let removed = record.contains_key(relative)
                    || extension_dir(relative).is_some_and(|dir| {
                        !root.join(dir).exists()
                            && record.keys().any(|known| known.starts_with(dir))
                    });
                !removed
            }
            Err(error) => {
                errors.push(format!("cannot read {}: {error}", target.display()));
                false
            }
        };
        if write_it {
            match write(&target, content) {
                Ok(()) => {
                    record.insert(relative.to_owned(), fresh);
                }
                Err(error) => errors.push(format!("cannot write {}: {error}", target.display())),
            }
        }
    }
    // Files an older version shipped and this one does not.
    record.retain(|relative, recorded| {
        if shipped.contains(&relative.as_str()) {
            return true;
        }
        let target = dirs.data.join(relative);
        if std::fs::read(&target).is_ok_and(|current| hash(&current) == *recorded) {
            let _ = std::fs::remove_file(&target);
        }
        false
    });
    if record != before {
        let text = serde_json::to_string_pretty(&record).unwrap_or_default();
        if let Err(error) = write(&record_path, &text) {
            errors.push(format!("cannot write {}: {error}", record_path.display()));
        }
    }
    errors
}

fn read_record(path: &Path) -> Option<BTreeMap<String, String>> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Removes what older versions wrote into the configuration directory,
/// which held everything, now that it goes to the data directory. Files the
/// user changed stay, and so does a shipped extension with any changed
/// file, whole: it then replaces the shipped one as the user's own. The
/// editor settings stay in the configuration directory, so their record
/// moves to `record`.
fn leave_the_config_dir(
    dirs: &AppDirs,
    record: &mut BTreeMap<String, String>,
    errors: &mut Vec<String>,
) {
    let old_path = dirs.config.join(RECORD);
    let Some(old) = read_record(&old_path) else {
        return;
    };
    let untouched = |relative: &str, recorded: &str| match std::fs::read(dirs.config.join(relative))
    {
        Ok(current) => hash(&current) == recorded,
        Err(error) => error.kind() == io::ErrorKind::NotFound,
    };
    let mut kept_dirs: Vec<&str> = Vec::new();
    for (relative, recorded) in &old {
        if let Some(dir) = extension_dir(relative)
            && !untouched(relative, recorded)
            && !kept_dirs.contains(&dir)
        {
            kept_dirs.push(dir);
        }
    }
    let mut emptied: Vec<PathBuf> = Vec::new();
    for (relative, recorded) in &old {
        if relative == LUARC || relative == TLCONFIG {
            record
                .entry(relative.clone())
                .or_insert_with(|| recorded.clone());
            continue;
        }
        if extension_dir(relative).is_some_and(|dir| kept_dirs.contains(&dir))
            || !untouched(relative, recorded)
        {
            continue;
        }
        let target = dirs.config.join(relative);
        match std::fs::remove_file(&target) {
            Ok(()) => emptied.extend(target.parent().map(Path::to_path_buf)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => errors.push(format!("cannot remove {}: {error}", target.display())),
        }
    }
    // Deepest first, so `types/` goes before `extensions/` is looked at.
    emptied.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));
    emptied.dedup();
    for dir in emptied {
        if dir != dirs.config.join("extensions") {
            let _ = std::fs::remove_dir(dir);
        }
    }
    if let Err(error) = std::fs::remove_file(&old_path) {
        errors.push(format!("cannot remove {}: {error}", old_path.display()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shipped_extension_is_bundled() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extensions");
        let mut found: Vec<String> = std::fs::read_dir(root)
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().join("init.lua").is_file())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        found.sort();
        assert_eq!(found, EXTENSIONS);
        for name in EXTENSIONS {
            let dir = format!("extensions/{name}/");
            assert_eq!(files().filter(|(p, _)| p.starts_with(&dir)).count(), 3);
        }
    }

    #[test]
    fn install_writes_updates_and_respects_the_users_changes() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path();
        assert!(install(&AppDirs::under(config)).is_empty());
        for (relative, content) in files() {
            assert_eq!(
                std::fs::read_to_string(config.join(relative)).unwrap(),
                content
            );
        }

        // An older version's file that nobody touched is updated.
        let readme = config.join("extensions/npm/README.md");
        std::fs::write(&readme, "old").unwrap();
        let mut record: BTreeMap<String, String> =
            serde_json::from_slice(&std::fs::read(config.join(RECORD)).unwrap()).unwrap();
        record.insert("extensions/npm/README.md".into(), hash(b"old"));
        // And one it shipped that this version dropped goes away.
        std::fs::write(config.join("extensions/npm/gone.lua"), "x").unwrap();
        record.insert("extensions/npm/gone.lua".into(), hash(b"x"));
        std::fs::write(config.join(RECORD), serde_json::to_string(&record).unwrap()).unwrap();
        assert!(install(&AppDirs::under(config)).is_empty());
        assert_ne!(std::fs::read_to_string(&readme).unwrap(), "old");
        assert!(!config.join("extensions/npm/gone.lua").exists());

        // An edited file stays, and so does a deleted extension's absence.
        let init = config.join("extensions/dotfiles/init.lua");
        std::fs::write(&init, "-- mine").unwrap();
        std::fs::remove_dir_all(config.join("extensions/pypi")).unwrap();
        std::fs::remove_file(config.join("CONFIGURATION.md")).unwrap();
        assert!(install(&AppDirs::under(config)).is_empty());
        assert_eq!(std::fs::read_to_string(&init).unwrap(), "-- mine");
        assert!(!config.join("extensions/pypi").exists());
        assert!(!config.join("CONFIGURATION.md").exists());
    }

    #[test]
    fn a_users_own_copy_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let init = dir.path().join("extensions/npm/init.lua");
        std::fs::create_dir_all(init.parent().unwrap()).unwrap();
        std::fs::write(&init, "-- copied by hand from an old checkout").unwrap();
        assert!(install(&AppDirs::under(dir.path())).is_empty());
        assert_eq!(
            std::fs::read_to_string(&init).unwrap(),
            "-- copied by hand from an old checkout"
        );
        assert!(dir.path().join("extensions/npm/README.md").is_file());
    }

    #[test]
    fn guards_move_only_to_shipped_extensions() {
        for guard in cmdterm_env::builtin_guards() {
            if let Some(name) = guard_extension(&guard.id) {
                assert!(is_shipped(name), "{name}");
            }
        }
        assert_eq!(guard_extension("cmdterm.npm-publish"), Some("npm"));
        assert_eq!(guard_extension("cmdterm.hg-push"), None);
    }

    #[test]
    fn shipped_extensions_load_without_a_grant() {
        let grants = default_grants();
        assert_eq!(grants.keys().collect::<Vec<_>>(), EXTENSIONS);
        assert!(grants["npm"].contains(&"secrets".to_owned()));
        let dir = tempfile::tempdir().unwrap();
        install(&AppDirs::under(dir.path()));
        let mut host = crate::ext::ExtensionHost::new();
        host.load_dir_except(
            &dir.path().join("extensions"),
            &[],
            grants,
            &Default::default(),
        );
        assert!(host.extensions().all(|e| e.missing.is_empty()));
    }

    #[test]
    fn the_type_files_and_editor_configs_are_not_loaded_as_extensions() {
        let dir = tempfile::tempdir().unwrap();
        install(&AppDirs::under(dir.path()));
        let mut host = crate::ext::ExtensionHost::new();
        host.load_dir(&dir.path().join("extensions"));
        let mut names: Vec<String> = host.extensions().map(|e| e.name.clone()).collect();
        names.sort();
        assert_eq!(names, EXTENSIONS);
    }

    fn split(root: &Path) -> AppDirs {
        AppDirs {
            config: root.join("config"),
            data: root.join("data"),
            state: root.join("state"),
            cache: root.join("cache"),
            logs: root.join("state/logs"),
        }
    }

    #[test]
    fn split_directories_get_the_references_in_data_and_the_editor_settings_in_config() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = split(dir.path());
        assert!(install(&dirs).is_empty());
        assert!(dirs.data.join("CONFIGURATION.md").is_file());
        assert!(dirs.data.join("extensions/npm/init.lua").is_file());
        assert!(!dirs.config.join("CONFIGURATION.md").exists());
        let luarc = std::fs::read_to_string(dirs.config.join(LUARC)).unwrap();
        let types = dirs.data.join("extensions/types");
        assert!(
            luarc.contains(&*serde_json::to_string(&types).unwrap()),
            "{luarc}"
        );
        assert!(dirs.data.join(RECORD).is_file());
    }

    #[test]
    fn what_an_older_version_wrote_into_the_config_dir_moves_out_unless_changed() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = split(dir.path());
        // The old layout: everything in the configuration directory.
        assert!(install(&AppDirs::under(&dirs.config)).is_empty());
        let config = &dirs.config;
        std::fs::write(config.join("extensions/dotfiles/init.lua"), "-- mine").unwrap();
        std::fs::write(config.join("extensions/mine.lua"), "-- own").unwrap();
        assert!(install(&dirs).is_empty());
        assert!(!config.join("CONFIGURATION.md").exists());
        assert!(!config.join("extensions/npm").exists());
        assert!(!config.join("extensions/types").exists());
        assert!(!config.join(RECORD).exists());
        assert_eq!(
            std::fs::read_to_string(config.join("extensions/dotfiles/init.lua")).unwrap(),
            "-- mine"
        );
        assert!(
            config.join("extensions/dotfiles/manifest.toml").is_file(),
            "a changed extension stays whole"
        );
        assert!(config.join("extensions/mine.lua").is_file());
        assert!(dirs.data.join("extensions/npm/init.lua").is_file());
        // The editor settings now point at the data directory.
        let luarc = std::fs::read_to_string(config.join(LUARC)).unwrap();
        assert!(luarc.contains("data"), "{luarc}");
    }
}
