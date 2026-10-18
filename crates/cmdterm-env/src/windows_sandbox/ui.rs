//! The UI limits of the job the Windows launcher runs a sandboxed command
//! in. The flags are the `JOB_OBJECT_UILIMIT_*` values from `winnt.h`, kept
//! here without `windows-sys` so that tests check the choice on every
//! platform. `windows_sandbox.rs` compares them with `windows-sys` when it
//! builds.
//!
//! The command shares the user's desktop with programs that run with the
//! full token. The limits keep it from reaching them through the window
//! manager. None of them touches the console: a console program reads and
//! writes through the console driver's handles, which are kernel handles,
//! not USER handles, and the pseudoconsole host (`conhost.exe` or
//! `OpenConsole.exe`) belongs to the app's process, which created the
//! pseudoconsole before the launcher started, so it stays outside the job.
//!
//! The command also runs on a desktop of its own (`desktop.rs`), which the
//! user never sees, so it cannot send keystrokes or mouse input to the
//! user's windows (`SendInput` only reaches the desktop in front) or hook
//! them. The clipboard belongs to the window station, which both desktops
//! share. The rights and the fallback decision are here, testable without
//! Windows.

/// No use of USER handles (windows, hooks, menus) that processes outside
/// the job own: no window messages, `WM_COPYDATA` or DDE to the user's
/// other windows, so the command cannot drive them. The job's own windows
/// and the desktop window keep working. A program that calls
/// `GetConsoleWindow` gets the pseudoconsole host's window, which lies
/// outside the job, so showing, hiding or parenting a dialog on it fails;
/// console input and output do not use it.
pub(crate) const HANDLES: u32 = 0x0001;
/// Reading the clipboard. Not set: pasting an image into an agent reads it.
pub(crate) const READCLIPBOARD: u32 = 0x0002;
/// Writing the clipboard. Not set, so copying from a tool keeps working.
pub(crate) const WRITECLIPBOARD: u32 = 0x0004;
/// `SystemParametersInfo` changes, such as the mouse, keyboard or
/// screen saver settings of the session. Command line tools only read them.
pub(crate) const SYSTEMPARAMETERS: u32 = 0x0008;
/// `ChangeDisplaySettings`. Command line tools do not change the display.
pub(crate) const DISPLAYSETTINGS: u32 = 0x0010;
/// The job gets a global atom table of its own. DDE passes atoms through
/// the global table, so this closes it along with `HANDLES`. Clipboard
/// formats and window messages are registered in another table and keep
/// working.
pub(crate) const GLOBALATOMS: u32 = 0x0020;
/// `CreateDesktop` and `SwitchDesktop`, so the command cannot put a desktop
/// of its own in front of the user.
pub(crate) const DESKTOP: u32 = 0x0040;
/// `ExitWindows(Ex)`: no logging off, shutting down or restarting.
pub(crate) const EXITWINDOWS: u32 = 0x0080;

/// The UI limits of the sandbox's job: all but the clipboard's.
pub(crate) const fn job_ui_limits() -> u32 {
    HANDLES | GLOBALATOMS | DESKTOP | EXITWINDOWS | SYSTEMPARAMETERS | DISPLAYSETTINGS
}

// Desktop access rights, from `winuser.h` and `winnt.h`.
pub(crate) const DESKTOP_READOBJECTS: u32 = 0x0001;
pub(crate) const DESKTOP_CREATEWINDOW: u32 = 0x0002;
pub(crate) const DESKTOP_CREATEMENU: u32 = 0x0004;
pub(crate) const DESKTOP_HOOKCONTROL: u32 = 0x0008;
pub(crate) const DESKTOP_JOURNALRECORD: u32 = 0x0010;
pub(crate) const DESKTOP_JOURNALPLAYBACK: u32 = 0x0020;
pub(crate) const DESKTOP_ENUMERATE: u32 = 0x0040;
pub(crate) const DESKTOP_WRITEOBJECTS: u32 = 0x0080;
pub(crate) const DESKTOP_SWITCHDESKTOP: u32 = 0x0100;
pub(crate) const DELETE: u32 = 0x0001_0000;
pub(crate) const READ_CONTROL: u32 = 0x0002_0000;
pub(crate) const WRITE_DAC: u32 = 0x0004_0000;
pub(crate) const WRITE_OWNER: u32 = 0x0008_0000;

/// What the user, who owns the sandbox's desktop, may do with it.
pub(crate) const DESKTOP_ALL: u32 =
    DESKTOP_PARTICIPANT | DESKTOP_SWITCHDESKTOP | DELETE | WRITE_DAC | WRITE_OWNER;

/// What the sandbox may do on its desktop: create and use windows and
/// hooks there. It may not show the desktop to the user, delete it or
/// change who may use it.
pub(crate) const DESKTOP_PARTICIPANT: u32 = DESKTOP_READOBJECTS
    | DESKTOP_CREATEWINDOW
    | DESKTOP_CREATEMENU
    | DESKTOP_HOOKCONTROL
    | DESKTOP_JOURNALRECORD
    | DESKTOP_JOURNALPLAYBACK
    | DESKTOP_ENUMERATE
    | DESKTOP_WRITEOBJECTS
    | READ_CONTROL;

/// `STATUS_DLL_INIT_FAILED`, the exit code of a process that could not
/// connect to its desktop: `user32.dll` fails to load before the program
/// runs any code of its own.
pub(crate) const DLL_INIT_FAILED: u32 = 0xC000_0142;

/// The name of the desktop of the launcher with process id `pid`. `nonce`
/// keeps a name from coming back after the launcher exits.
pub(crate) fn desktop_name(pid: u32, nonce: u64) -> String {
    format!("cmdterm-sandbox-{pid}-{nonce:x}")
}

/// What `STARTUPINFO.lpDesktop` names for the desktop `name` in the
/// interactive window station.
pub(crate) fn startup_desktop(name: &str) -> String {
    format!("WinSta0\\{name}")
}

/// Whether a command that exited with `code` on the sandbox's desktop
/// should run again on the user's: it could not use the desktop at all.
pub(crate) fn failed_on_desktop(code: u32) -> bool {
    code == DLL_INIT_FAILED
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_job_limits_everything_but_the_clipboard() {
        let limits = job_ui_limits();
        for flag in [
            HANDLES,
            GLOBALATOMS,
            DESKTOP,
            EXITWINDOWS,
            SYSTEMPARAMETERS,
            DISPLAYSETTINGS,
        ] {
            assert_eq!(limits & flag, flag, "{flag:#x} is missing");
        }
        assert_eq!(limits & (READCLIPBOARD | WRITECLIPBOARD), 0);
        // JOB_OBJECT_UILIMIT_ALL is 0xff.
        assert_eq!(limits | READCLIPBOARD | WRITECLIPBOARD, 0xff);
    }

    #[test]
    fn the_sandbox_cannot_show_delete_or_repermission_its_desktop() {
        for right in [DESKTOP_SWITCHDESKTOP, DELETE, WRITE_DAC, WRITE_OWNER] {
            assert_eq!(DESKTOP_PARTICIPANT & right, 0, "{right:#x}");
            assert_eq!(DESKTOP_ALL & right, right, "{right:#x}");
        }
        assert_eq!(DESKTOP_ALL & DESKTOP_PARTICIPANT, DESKTOP_PARTICIPANT);
        assert_eq!(
            DESKTOP_PARTICIPANT & 0xff,
            0xff,
            "windows and hooks work there"
        );
    }

    #[test]
    fn desktops_have_unique_names_in_the_interactive_window_station() {
        let name = desktop_name(4242, 0xbeef);
        assert_eq!(name, "cmdterm-sandbox-4242-beef");
        assert!(!name.contains('\\'));
        assert_ne!(name, desktop_name(4242, 0xbeee));
        assert_eq!(startup_desktop(&name), "WinSta0\\cmdterm-sandbox-4242-beef");
    }

    #[test]
    fn only_a_failed_desktop_connection_runs_the_command_again() {
        assert!(failed_on_desktop(0xC000_0142));
        for code in [0, 1, 0xC000_013A, 0xC000_0005] {
            assert!(!failed_on_desktop(code), "{code:#x}");
        }
    }
}
