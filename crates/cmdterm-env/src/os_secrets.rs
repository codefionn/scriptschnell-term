//! The operating system's secret storage, where there is one: the Secret
//! Service of the desktop session (gnome-keyring, KWallet, KeePassXC)
//! through `secret-tool`, the login keychain on macOS through `security`,
//! or the Windows Credential Manager through its API. A secret is found by a
//! service and an account name. Secrets go to the tools through stdin, never
//! as arguments, so other processes cannot read them from the process list.
//!
//! Every call may wait for the user: a locked keyring asks for its
//! password first. Callers that must not wait run them on a thread.

use std::io;
#[cfg(not(windows))]
use std::io::Write;
use std::path::Path;
#[cfg(not(windows))]
use std::process::{Command, Stdio};

/// Whether this session has secret storage the functions here can reach.
pub fn available() -> bool {
    if cfg!(windows) {
        true
    } else if cfg!(target_os = "macos") {
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
    #[cfg(windows)]
    {
        windows::lookup(service, account)
    }
    #[cfg(not(windows))]
    {
        tool_lookup(service, account)
    }
}

/// Stores `secret` for `service` and `account`, replacing an older one.
/// `label` is what the keyring shows for it. `secret` must be one line.
pub fn store(label: &str, service: &str, account: &str, secret: &str) -> io::Result<()> {
    if secret.contains(['\n', '\r']) {
        return Err(io::Error::other("secrets are stored as one line"));
    }
    #[cfg(windows)]
    {
        windows::store(label, service, account, secret)
    }
    #[cfg(not(windows))]
    {
        tool_store(label, service, account, secret)
    }
}

/// Removes the secret of `service` and `account`. Removing one that is not
/// there is no error.
pub fn remove(service: &str, account: &str) -> io::Result<()> {
    #[cfg(windows)]
    {
        windows::remove(service, account)
    }
    #[cfg(not(windows))]
    {
        tool_remove(service, account)
    }
}

/// [`lookup`] through `secret-tool` or `security`.
#[cfg(not(windows))]
fn tool_lookup(service: &str, account: &str) -> io::Result<Option<String>> {
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

/// [`store`] through `secret-tool` or `security`.
#[cfg(not(windows))]
fn tool_store(label: &str, service: &str, account: &str, secret: &str) -> io::Result<()> {
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

/// [`remove`] through `secret-tool` or `security`.
#[cfg(not(windows))]
fn tool_remove(service: &str, account: &str) -> io::Result<()> {
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

/// The Windows Credential Manager: one generic credential per secret, named
/// `<service>/<account>`, with the secret's UTF-8 bytes as its blob.
#[cfg(windows)]
mod windows {
    #![allow(unsafe_code)]

    use std::io;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, GetLastError};
    use windows_sys::Win32::Security::Credentials::{
        CRED_MAX_CREDENTIAL_BLOB_SIZE, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW,
        CredDeleteW, CredFree, CredReadW, CredWriteW,
    };

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    fn target(service: &str, account: &str) -> Vec<u16> {
        wide(&format!("{service}/{account}"))
    }

    pub(super) fn lookup(service: &str, account: &str) -> io::Result<Option<String>> {
        let target = target(service, account);
        let mut credential: *mut CREDENTIALW = null_mut();
        // SAFETY: `target` is a NUL-terminated wide string that outlives the
        // call and `credential` is a live out pointer.
        let read = unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) };
        if read == 0 {
            // SAFETY: reads this thread's last error, set by the call above.
            let error = unsafe { GetLastError() };
            return if error == ERROR_NOT_FOUND {
                Ok(None)
            } else {
                Err(io::Error::from_raw_os_error(error as i32))
            };
        }
        // SAFETY: on success `credential` points at a credential the system
        // allocated, whose blob holds `CredentialBlobSize` bytes. It is
        // copied before `CredFree` releases it.
        let bytes = unsafe {
            let credential = &*credential;
            let blob = if credential.CredentialBlob.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(
                    credential.CredentialBlob,
                    credential.CredentialBlobSize as usize,
                )
                .to_vec()
            };
            CredFree(credential as *const CREDENTIALW as *const _);
            blob
        };
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| io::Error::other("the stored secret is not UTF-8"))
    }

    pub(super) fn store(label: &str, service: &str, account: &str, secret: &str) -> io::Result<()> {
        if secret.len() > CRED_MAX_CREDENTIAL_BLOB_SIZE as usize {
            return Err(io::Error::other(format!(
                "the Windows Credential Manager holds secrets of at most {CRED_MAX_CREDENTIAL_BLOB_SIZE} bytes"
            )));
        }
        let mut target = target(service, account);
        let mut comment = wide(label);
        let mut user = wide(account);
        let mut blob = secret.as_bytes().to_vec();
        let credential = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: target.as_mut_ptr(),
            Comment: comment.as_mut_ptr(),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            UserName: user.as_mut_ptr(),
            ..Default::default()
        };
        // SAFETY: every pointer in `credential` points into a buffer above
        // that outlives the call; the strings are NUL-terminated.
        if unsafe { CredWriteW(&credential, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn remove(service: &str, account: &str) -> io::Result<()> {
        let target = target(service, account);
        // SAFETY: `target` is a NUL-terminated wide string that outlives the
        // call.
        if unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
            // SAFETY: reads this thread's last error, set by the call above.
            let error = unsafe { GetLastError() };
            if error != ERROR_NOT_FOUND {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
        }
        Ok(())
    }
}

/// `value` in double quotes for a `security -i` command line.
#[cfg(not(windows))]
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

    #[cfg(not(windows))]
    #[test]
    fn keychain_commands_quote_their_values() {
        assert_eq!(quote(r#"a "b" \c"#), r#""a \"b\" \\c""#);
    }

    #[cfg(windows)]
    #[test]
    fn the_credential_manager_keeps_and_forgets_secrets() {
        let service = format!("scriptschnell-term-test-{}", std::process::id());
        store("test", &service, "acc", "s3cret").unwrap();
        assert_eq!(lookup(&service, "acc").unwrap().as_deref(), Some("s3cret"));
        store("test", &service, "acc", "newer").unwrap();
        assert_eq!(lookup(&service, "acc").unwrap().as_deref(), Some("newer"));
        remove(&service, "acc").unwrap();
        assert_eq!(lookup(&service, "acc").unwrap(), None);
        remove(&service, "acc").unwrap();
    }

    #[test]
    fn multi_line_secrets_are_refused_before_any_tool_runs() {
        assert!(store("l", "s", "a", "one\ntwo").is_err());
    }
}
