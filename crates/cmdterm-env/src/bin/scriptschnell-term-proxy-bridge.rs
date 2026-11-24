//! Runs a command inside a sandbox whose only way out is a filtering proxy.
//! See `cmdterm_env::proxy_bridge_main`.

#[cfg(unix)]
fn main() {
    // A server's sandboxes call every bridge as the app and scriptschnell-term-remote
    // take it, with the flag first (see `cmdterm_env::SCOPE_EXE_VAR`).
    let mut args = std::env::args_os().skip(1).peekable();
    args.next_if(|first| first == cmdterm_env::BUILT_IN_BRIDGE_FLAG);
    std::process::exit(cmdterm_env::proxy_bridge_main(args));
}

#[cfg(not(unix))]
fn main() {
    eprintln!("scriptschnell-term-proxy-bridge runs on Unix only");
    std::process::exit(125);
}
