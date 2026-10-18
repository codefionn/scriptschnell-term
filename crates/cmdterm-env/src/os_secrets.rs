//! The operating system's secret storage, where there is one: the Secret
//! Service of the desktop session (gnome-keyring, KWallet, KeePassXC)
//! through `secret-tool`, or the login keychain on macOS through
//! `security`. A secret is found by a service and an account name. Secrets
//! go to these tools through stdin, never as arguments, so other processes
//! cannot read them from the process list.
//!
//! Every call may wait for the user: a locked keyring asks for its
//! password first. Callers that must not wait run them on a thread.

use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};

/// Whether this session has secret storage the functions here can reach.
pub fn available() -> bool {
    if cfg!(target_os = "macos") {
        on_path("security")
    } else if cfg!(unix) {
        std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some_and(|a| !a.is_empty())
            && on_path("secret-tool")
    } else {
        false
    }
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| Path::new(&dir).join(program).is_file())
    })
}

/// The secret of `service` and `account`, or `None` when there is none.
pub fn lookup(service: &str, account: &str) -> io::Result<Option<String>> {
    let output = if cfg!(target_os = "macos") {
        Command::new("security")
            .args(["find-generic-password", "-s", service, "-a", account, "-w"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()?
    } else {
        Command::new("secret-tool")
            .args(["lookup", "service", service, "account", account])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()?
    };
    // Both tools exit with an error when nothing matches.
    if !output.status.success() {
        return Ok(None);
    }
    let secret = String::from_utf8(output.stdout)
        .map_err(|_| io::Error::other("the stored secret is not UTF-8"))?;
    Ok(Some(secret.trim_end_matches('\n').to_owned()))
}

/// Stores `secret` for `service` and `account`, replacing an older one.
/// `label` is what the keyring shows for it. `secret` must be one line.
pub fn store(label: &str, service: &str, account: &str, secret: &str) -> io::Result<()> {
    if secret.contains(['\n', '\r']) {
        return Err(io::Error::other("secrets are stored as one line"));
    }
    let mut child = if cfg!(target_os = "macos") {
        // `security -i` reads its commands from stdin.
        Command::new("security")
            .arg("-i")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?
    } else {
        // Without a terminal, secret-tool reads the secret from stdin.
        Command::new("secret-tool")
            .args([
                "store", "--label", label, "service", service, "account", account,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?
    };
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("no stdin"))?;
        if cfg!(target_os = "macos") {
            let command = format!(
                "add-generic-password -U -s {} -a {} -l {} -w {}\n",
                quote(service),
                quote(account),
                quote(label),
                quote(secret)
            );
            stdin.write_all(command.as_bytes())?;
        } else {
            stdin.write_all(secret.as_bytes())?;
        }
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "storing the secret failed ({status})"
        )))
    }
}

/// Removes the secret of `service` and `account`. Removing one that is not
/// there is no error.
pub fn remove(service: &str, account: &str) -> io::Result<()> {
    let mut command = if cfg!(target_os = "macos") {
        let mut command = Command::new("security");
        command.args(["delete-generic-password", "-s", service, "-a", account]);
        command
    } else {
        let mut command = Command::new("secret-tool");
        command.args(["clear", "service", service, "account", account]);
        command
    };
    // `security` exits with an error when nothing matches; that is fine.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(drop)
}

/// `value` in double quotes for a `security -i` command line.
fn quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for c in value.chars() {
        if matches!(c, '"' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keychain_commands_quote_their_values() {
        assert_eq!(quote(r#"a "b" \c"#), r#""a \"b\" \\c""#);
    }

    #[test]
    fn multi_line_secrets_are_refused_before_any_tool_runs() {
        assert!(store("l", "s", "a", "one\ntwo").is_err());
    }
}
