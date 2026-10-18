//! What a terminal tells the programs in it about itself. Programs such as
//! Claude Code probe some features with escape sequences and read others
//! only from the environment, so the variables have to match what the
//! emulator and the grid actually do.

/// The name the terminal answers XTVERSION with and sets as `TERM_PROGRAM`.
pub const PROGRAM: &str = "scriptschnell-term";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Set in every terminal, also on other machines, where ssh would only pass
/// `TERM`. The grid draws 24-bit color, OSC 8 links and strikethrough.
/// Claude Code reads strikethrough support from a list of known
/// `TERM_PROGRAM`s, so it needs its own variable.
pub const VARS: &[(&str, &str)] = &[
    ("COLORTERM", "truecolor"),
    ("TERM_PROGRAM", PROGRAM),
    ("TERM_PROGRAM_VERSION", VERSION),
    ("FORCE_HYPERLINK", "1"),
    ("CLAUDE_CODE_FORCE_STRIKETHROUGH", "1"),
];

/// Variables of the terminal the app itself was started from. Programs
/// trust them over `TERM_PROGRAM`, so a local child that inherits
/// `KITTY_WINDOW_ID` would ask for kitty graphics the grid cannot draw.
pub const FOREIGN_VARS: &[&str] = &[
    "ALACRITTY_LOG",
    "ALACRITTY_SOCKET",
    "ALACRITTY_WINDOW_ID",
    "GHOSTTY_BIN_DIR",
    "GHOSTTY_RESOURCES_DIR",
    "GHOSTTY_SHELL_FEATURES",
    "GNOME_TERMINAL_SCREEN",
    "GNOME_TERMINAL_SERVICE",
    "ITERM_PROFILE",
    "ITERM_SESSION_ID",
    "KITTY_INSTALLATION_DIR",
    "KITTY_LISTEN_ON",
    "KITTY_PID",
    "KITTY_PUBLIC_KEY",
    "KITTY_WINDOW_ID",
    "KONSOLE_DBUS_SERVICE",
    "KONSOLE_DBUS_SESSION",
    "KONSOLE_DBUS_WINDOW",
    "KONSOLE_PROFILE_NAME",
    "KONSOLE_VERSION",
    "LC_TERMINAL",
    "LC_TERMINAL_VERSION",
    "STY",
    "TERMINAL_EMULATOR",
    "TERMINATOR_UUID",
    "TERMINATOR_DBUS_NAME",
    "TERMINATOR_DBUS_PATH",
    "TILIX_ID",
    "TMUX",
    "TMUX_PANE",
    "VTE_VERSION",
    "WARP_IS_LOCAL_SHELL_SESSION",
    "WEZTERM_CONFIG_DIR",
    "WEZTERM_CONFIG_FILE",
    "WEZTERM_EXECUTABLE",
    "WEZTERM_EXECUTABLE_DIR",
    "WEZTERM_PANE",
    "WEZTERM_UNIX_SOCKET",
    "WT_PROFILE_ID",
    "WT_SESSION",
    "ZED_TERM",
];
