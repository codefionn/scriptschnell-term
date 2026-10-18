//! Entry point for the `scriptschnell-term` binary. The application lives in
//! the `cmdterm` library crate so its integration tests can link it; this file
//! only forwards to it.

fn main() -> std::process::ExitCode {
    cmdterm::main()
}
