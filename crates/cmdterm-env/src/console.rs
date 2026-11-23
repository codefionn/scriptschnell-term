//! Console windows of the processes this machine starts. Release builds of
//! the app on Windows are GUI programs with no console of their own to
//! share, so a console child started plainly would open a window of its
//! own. Elsewhere every function does nothing.

use std::process::Command;

/// Keeps a console child of `command` from opening a window. The child gets
/// a console without a window, which its own children inherit.
pub fn no_window(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

/// Lets a console child of `command` share this process's console, undoing
/// [`no_window`], for commands the user types into.
pub fn shared(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0);
    }
    #[cfg(not(windows))]
    let _ = command;
}
