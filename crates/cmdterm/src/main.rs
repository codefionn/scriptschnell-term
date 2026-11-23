//! Entry point for the `scriptschnell-term` binary. The application lives in
//! the `cmdterm` library crate so its integration tests can link it; this file
//! only forwards to it.

// Release builds on Windows are GUI programs, so starting them from Explorer
// or the Start menu opens no console window. `cmdterm::main` attaches to the
// starting terminal's console instead, when there is one.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() -> std::process::ExitCode {
    cmdterm::main()
}
