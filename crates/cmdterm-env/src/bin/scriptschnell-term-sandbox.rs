//! Runs a command in a Windows sandbox for scriptschnell-term (`SandboxKind::Windows`).
//! scriptschnell-term starts it as `scriptschnell-term-sandbox [--write PATH] [--read PATH]
//! [--unset VAR] [--set KEY=VALUE] [--pipe VAR=PIPE] [--profile NAME]
//! [--read-only PATH] [--appcontainer NAME] [--hide PATH]
//! [--ports-tcp PORT:TOKEN] [--cwd DIR] -- PROGRAM [ARG]...` and exits with the
//! command's exit code. `--profile` names the project's SID and temporary
//! directory, and `--read-only` paths cannot be changed. `--appcontainer`,
//! `--read`, `--hide` and `--ports-tcp` are the experimental AppContainer
//! mode; `--hide` is ignored without it.

use std::process::exit;

fn main() {
    let launch = match cmdterm_env::SandboxLaunch::parse(std::env::args_os().skip(1)) {
        Ok(launch) => launch,
        Err(error) => {
            eprintln!("scriptschnell-term-sandbox: {error}");
            exit(2);
        }
    };
    #[cfg(windows)]
    match cmdterm_env::launch_sandboxed(&launch) {
        // Windows exit codes are 32 bits; the cast keeps every bit.
        Ok(code) => exit(code as i32),
        Err(error) => {
            eprintln!("scriptschnell-term-sandbox: {error}");
            exit(126);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = launch;
        eprintln!("scriptschnell-term-sandbox: only Windows sandboxes use this launcher");
        exit(126);
    }
}
