//! Named pipe relays for the Windows sandbox.
//!
//! A write-restricted token cannot open a container engine's pipe for
//! writing: its ACL grants the user or a group such as `docker-users`, never
//! the sandbox's SID. The launcher runs with the full token, so it serves a
//! pipe of its own that only the user, the sandbox and SYSTEM may open, and
//! copies each connection to the engine's pipe and back. The relays end with
//! the launcher.

use std::ffi::OsStr;
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ,
    GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
};
use windows_sys::Win32::System::Threading::CreateEventW;

use super::{Handle, Sid, wide};

const BUFFER: u32 = 64 * 1024;

/// A handle that relay threads share. Each thread waits on its own
/// operations, which overlapped handles allow.
struct Shared(Handle);

unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

/// Starts a relay for the pipe that `address` names, such as
/// `npipe:////./pipe/docker_engine` or `\\.\pipe\containerd-containerd`,
/// and returns the address of the relay in the same form. `None` when that
/// pipe does not exist.
pub(super) fn start(address: &OsStr, users: &[&Sid]) -> Result<Option<String>, String> {
    let address = address.to_str().ok_or("a pipe address needs UTF-8")?;
    let name = pipe_name(address).ok_or_else(|| format!("not a pipe: {address}"))?;
    let target = wide(OsStr::new(&format!(r"\\.\pipe\{name}")));
    if !exists(&target) {
        return Ok(None);
    }
    let own = format!("cmdterm-sandbox-{}-{name}", std::process::id());
    let relay = wide(OsStr::new(&format!(r"\\.\pipe\{own}")));
    let sddl = wide(OsStr::new(&sddl(users)?));
    // The first instance exists before the sandbox starts, so its first
    // client finds it.
    let first = Shared(instance(&relay, &sddl, FILE_FLAG_FIRST_PIPE_INSTANCE)?);
    std::thread::spawn(move || {
        let first = first;
        serve(first.0, &relay, &target, &sddl);
    });
    Ok(Some(format!(
        "{}{own}",
        &address[..address.len() - name.len()]
    )))
}

/// The pipe's name: what follows `/pipe/` or `\pipe\`.
fn pipe_name(address: &str) -> Option<&str> {
    let lower = address.to_ascii_lowercase();
    let start = lower.rfind("/pipe/").or_else(|| lower.rfind(r"\pipe\"))? + "/pipe/".len();
    Some(&address[start..]).filter(|name| !name.is_empty())
}

/// True when the pipe has an instance, free or busy. Unlike opening it,
/// this does not take an instance from its server.
fn exists(pipe: &[u16]) -> bool {
    unsafe { WaitNamedPipeW(pipe.as_ptr(), 1) != 0 || GetLastError() != ERROR_FILE_NOT_FOUND }
}

/// An ACL that grants full access to `users` and SYSTEM only.
fn sddl(users: &[&Sid]) -> Result<String, String> {
    let mut sddl = String::from("D:P(A;;GA;;;SY)");
    for sid in users {
        let mut text = null_mut();
        unsafe {
            if ConvertSidToStringSidW(sid.as_psid(), &mut text) == 0 {
                return Err(format!("cannot format a SID: {}", super::last_error()));
            }
            let len = (0..).take_while(|&i| *text.add(i) != 0).count();
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast());
            sddl.push_str(&format!("(A;;GA;;;{sid})"));
        }
    }
    Ok(sddl)
}

fn instance(name: &[u16], sddl: &[u16], flags: u32) -> Result<Handle, String> {
    unsafe {
        let mut descriptor = null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            null_mut(),
        ) == 0
        {
            return Err(format!(
                "cannot build the relay ACL: {}",
                super::last_error()
            ));
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let pipe = CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | flags,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            BUFFER,
            BUFFER,
            0,
            &attributes,
        );
        LocalFree(descriptor);
        if pipe == INVALID_HANDLE_VALUE {
            return Err(format!(
                "cannot create a relay pipe: {}",
                super::last_error()
            ));
        }
        Ok(Handle(pipe))
    }
}

/// Accepts clients on `relay` and connects each to `target`, until a pipe
/// instance cannot be created.
fn serve(mut next: Handle, relay: &[u16], target: &[u16], sddl: &[u16]) {
    let never = AtomicBool::new(false);
    loop {
        let server = next;
        let Some(event) = event() else { return };
        let connected = unsafe {
            io(server.0, event.0, &never, |overlapped| {
                let ok = ConnectNamedPipe(server.0, overlapped);
                // A client that came before the call is connected already.
                ok != 0 || GetLastError() == ERROR_PIPE_CONNECTED
            })
        };
        // The next instance waits while this one is busy, so clients get
        // `ERROR_PIPE_BUSY`, which they retry, instead of no pipe at all.
        next = match instance(relay, sddl, 0) {
            Ok(pipe) => pipe,
            Err(_) => return,
        };
        if connected.is_none() {
            continue;
        }
        let Some(engine) = open(target) else { continue };
        let server = Arc::new(Shared(server));
        let engine = Arc::new(Shared(engine));
        let closed = Arc::new(AtomicBool::new(false));
        for (from, to) in [(server.clone(), engine.clone()), (engine, server)] {
            let closed = closed.clone();
            std::thread::spawn(move || pump(&from, &to, &closed));
        }
    }
}

/// Opens the engine's pipe, waiting a little while every instance is busy.
fn open(target: &[u16]) -> Option<Handle> {
    for _ in 0..10 {
        let handle = unsafe {
            CreateFileW(
                target.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            return Some(Handle(handle));
        }
        if unsafe { GetLastError() } != ERROR_PIPE_BUSY
            || unsafe { WaitNamedPipeW(target.as_ptr(), 1000) } == 0
        {
            return None;
        }
    }
    None
}

/// Copies `from` to `to` until either side closes, then stops the copy the
/// other way.
fn pump(from: &Shared, to: &Shared, closed: &AtomicBool) {
    let (from, to) = (from.0.0, to.0.0);
    if let Some(event) = event() {
        let mut buffer = vec![0u8; BUFFER as usize];
        'copy: loop {
            let read = unsafe {
                io(from, event.0, closed, |overlapped| {
                    ReadFile(from, buffer.as_mut_ptr(), BUFFER, null_mut(), overlapped) != 0
                })
            };
            let len = match read {
                Some(0) | None => break,
                Some(len) => len as usize,
            };
            let mut done = 0;
            while done < len {
                let rest = &buffer[done..len];
                let written = unsafe {
                    io(to, event.0, closed, |overlapped| {
                        WriteFile(to, rest.as_ptr(), rest.len() as u32, null_mut(), overlapped) != 0
                    })
                };
                match written {
                    Some(0) | None => break 'copy,
                    Some(n) => done += n as usize,
                }
            }
        }
    }
    closed.store(true, Ordering::SeqCst);
    unsafe {
        CancelIoEx(from, null());
        CancelIoEx(to, null());
    }
}

/// Runs one overlapped operation on `handle` and waits for it. `start`
/// issues it and returns whether it completed at once. An operation issued
/// after the other direction closed is cancelled: `closed` is checked after
/// issuing, and the other side sets it before cancelling.
unsafe fn io(
    handle: HANDLE,
    event: HANDLE,
    closed: &AtomicBool,
    start: impl FnOnce(*mut OVERLAPPED) -> bool,
) -> Option<u32> {
    if closed.load(Ordering::SeqCst) {
        return None;
    }
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.hEvent = event;
    if !start(&mut overlapped) && unsafe { GetLastError() } != ERROR_IO_PENDING {
        return None;
    }
    if closed.load(Ordering::SeqCst) {
        unsafe { CancelIoEx(handle, &overlapped) };
    }
    let mut len = 0;
    let ok = unsafe { GetOverlappedResult(handle, &overlapped, &mut len, 1) };
    (ok != 0).then_some(len)
}

/// A manual-reset event, which each overlapped operation resets as it
/// starts.
fn event() -> Option<Handle> {
    let event = unsafe { CreateEventW(null(), 1, 0, null()) };
    (!event.is_null()).then_some(Handle(event))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_names_come_from_both_address_forms() {
        assert_eq!(
            pipe_name("npipe:////./pipe/docker_engine"),
            Some("docker_engine")
        );
        assert_eq!(
            pipe_name(r"\\.\pipe\containerd-containerd"),
            Some("containerd-containerd")
        );
        assert_eq!(pipe_name("unix:///run/docker.sock"), None);
        assert_eq!(pipe_name("npipe:////./pipe/"), None);
    }
}
