//! Running from an AppImage.
//!
//! The AppImage runtime mounts the image under `/tmp/.mount_*`, starts the
//! app from there and unmounts it when the app quits. Two things follow.
//! Programs beside the app (`scriptschnell-term-remote`, `scriptschnell-term-proxy-bridge`) vanish
//! with the mount, though some outlive the app, such as the remote helper
//! that keeps connections to remote machines, and bubblewrap sandboxes get a
//! fresh `/tmp` without the mount. [`install_helpers`] copies them to a
//! directory that stays. And the runtime's variables reach every program the
//! app starts, which would take them for its own AppImage; [`RUNTIME_VARS`]
//! are removed from them.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

/// What the AppImage runtime sets for the program it starts: the image
/// file, its mount, the name it was started by and the working directory it
/// was started in.
pub(crate) const RUNTIME_VARS: [&str; 4] = ["APPIMAGE", "APPDIR", "ARGV0", "OWD"];

/// The programs the app starts from beside its own executable.
pub const HELPERS: [&str; 2] = [
    "scriptschnell-term-remote",
    "scriptschnell-term-proxy-bridge",
];

/// Whether this process inherited the AppImage runtime's variables, its own
/// or those of an AppImage that started it, so commands lose them.
pub(crate) fn strips_runtime_vars() -> bool {
    static STRIP: OnceLock<bool> = OnceLock::new();
    *STRIP.get_or_init(|| {
        std::env::var_os("APPIMAGE").is_some() || std::env::var_os("APPDIR").is_some()
    })
}

/// The AppImage file this process runs from: the runtime names it in
/// `APPIMAGE` and mounts it at `APPDIR`, which holds the executable. `None`
/// for other installs, also when an AppImage only started this process.
pub fn running_from() -> Option<PathBuf> {
    let file = std::env::var_os("APPIMAGE")?;
    let mount = std::env::var_os("APPDIR")?;
    let exe = std::env::current_exe().ok()?;
    is_inside(&exe, Path::new(&mount)).then(|| PathBuf::from(file))
}

fn is_inside(exe: &Path, mount: &Path) -> bool {
    !mount.as_os_str().is_empty() && exe.starts_with(mount)
}

/// Copies [`HELPERS`] from beside the executable into `dir` and returns
/// `dir`. A copy whose size and modification time match its source stays;
/// another is replaced by a rename, so a helper that still runs from it
/// keeps its file. A helper missing beside the executable is skipped.
pub fn install_helpers(dir: &Path) -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let Some(from) = exe.parent() else {
        return Err(io::Error::other("the executable has no directory"));
    };
    std::fs::create_dir_all(dir)?;
    for name in HELPERS {
        let name = format!("{name}{}", std::env::consts::EXE_SUFFIX);
        let source = from.join(&name);
        if source.is_file() {
            copy_if_changed(&source, &dir.join(&name))?;
        }
    }
    Ok(dir.to_path_buf())
}

fn copy_if_changed(source: &Path, target: &Path) -> io::Result<()> {
    let wanted = std::fs::metadata(source)?;
    let stamp = |meta: &std::fs::Metadata| (meta.len(), meta.modified().ok());
    if std::fs::metadata(target).is_ok_and(|have| stamp(&have) == stamp(&wanted)) {
        return Ok(());
    }
    let mut partial: OsString = target.as_os_str().to_owned();
    partial.push(format!(".{}.part", std::process::id()));
    let partial = PathBuf::from(partial);
    let copied = (|| {
        std::fs::copy(source, &partial)?;
        let modified = wanted.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        std::fs::File::options()
            .write(true)
            .open(&partial)?
            .set_modified(modified)?;
        std::fs::rename(&partial, target)
    })();
    if copied.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    copied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_executable_inside_the_mount_runs_from_the_image() {
        let mount = Path::new("/tmp/.mount_abc");
        assert!(is_inside(Path::new("/tmp/.mount_abc/usr/bin/app"), mount));
        assert!(!is_inside(Path::new("/usr/bin/app"), mount));
        assert!(!is_inside(Path::new("/tmp/.mount_abcd/usr/bin/app"), mount));
        assert!(!is_inside(Path::new("/usr/bin/app"), Path::new("")));
    }

    #[test]
    fn a_helper_is_copied_once_and_again_when_it_changes() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("helper");
        let target = dir.path().join("copy");
        std::fs::write(&source, "one").unwrap();
        copy_if_changed(&source, &target).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "one");
        let first = std::fs::metadata(&target).unwrap();
        assert_eq!(
            first.modified().unwrap(),
            std::fs::metadata(&source).unwrap().modified().unwrap()
        );

        // Unchanged: the same file stays.
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt;
            copy_if_changed(&source, &target).unwrap();
            assert_eq!(std::fs::metadata(&target).unwrap().ino(), first.ino());
            first.ino()
        };

        std::fs::write(&source, "two!").unwrap();
        copy_if_changed(&source, &target).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "two!");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_ne!(std::fs::metadata(&target).unwrap().ino(), inode);
        }
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(left.len(), 2, "no partial copy stays");
    }
}
