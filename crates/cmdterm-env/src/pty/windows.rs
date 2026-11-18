//! The Windows PTY: a pseudo console (ConPTY) with a pipe pair for the master.
//!
//! ConPTY keeps the output pipe open until the pseudo console closes, so the
//! reader would never see the end of the child's output. [`Child::wait`] and
//! [`Child::try_wait`] close the pseudo console once the process has ended, and
//! dropping the [`Pty`] closes it too. The reader then drains what is left and
//! reports end of input.

#![allow(unsafe_code)]

use std::ffi::{OsString, c_void};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex, MutexGuard};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES,
    STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

use super::{CommandBuilder, DEFAULT_TERM, ExitStatus, PtySize};
use crate::windows_sandbox::{command_line, environment_block, wide};

/// An owned kernel handle, closed on drop.
struct Handle(HANDLE);

// SAFETY: a kernel handle is a process-wide token that any thread may use.
unsafe impl Send for Handle {}
// SAFETY: as above; the calls made through it are thread-safe.
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is valid and owned by this value.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn coord(size: PtySize) -> COORD {
    COORD {
        X: size.cols.min(i16::MAX as u16) as i16,
        Y: size.rows.min(i16::MAX as u16) as i16,
    }
}

/// The pseudo console. [`Pty`] owns it and closes it on drop; a [`Child`] holds
/// a clone only to close it once the process has ended.
struct Console {
    console: Mutex<Option<HPCON>>,
}

impl Console {
    fn lock(&self) -> MutexGuard<'_, Option<HPCON>> {
        self.console.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn close(&self) {
        if let Some(console) = self.lock().take() {
            // SAFETY: the console came from `CreatePseudoConsole` and `take`
            // makes sure it closes once.
            unsafe {
                ClosePseudoConsole(console);
            }
        }
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        self.close();
    }
}

/// The master side of a PTY.
///
/// Dropping it closes the pseudo console, even while readers, writers or the
/// child are alive, so the child gets its close event and the reader sees the
/// end of the output.
pub struct Pty {
    console: Arc<Console>,
    /// We write the child's input here.
    input: Arc<Handle>,
    /// We read the child's output here.
    output: Arc<Handle>,
}

impl Drop for Pty {
    fn drop(&mut self) {
        // Closing can wait for the reader to drain the output on older
        // Windows, so it must not block the caller.
        let console = self.console.clone();
        std::thread::spawn(move || console.close());
    }
}

impl Pty {
    pub fn open(size: PtySize) -> io::Result<Pty> {
        let mut in_read: HANDLE = null_mut();
        let mut in_write: HANDLE = null_mut();
        let mut out_read: HANDLE = null_mut();
        let mut out_write: HANDLE = null_mut();
        // SAFETY: the out pointers point at live locals and the security
        // attributes are null, which makes the handles non-inheritable.
        unsafe {
            if CreatePipe(&mut in_read, &mut in_write, null(), 0) == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let (in_read, in_write) = (Handle(in_read), Handle(in_write));
        // SAFETY: as above.
        unsafe {
            if CreatePipe(&mut out_read, &mut out_write, null(), 0) == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let (out_read, out_write) = (Handle(out_read), Handle(out_write));
        let mut console: HPCON = 0;
        // SAFETY: both handles are valid pipe ends and `console` is a live
        // local. The pseudo console duplicates the ends it needs, so ours close
        // when `in_read` and `out_write` drop at the end of this function.
        let hr =
            unsafe { CreatePseudoConsole(coord(size), in_read.0, out_write.0, 0, &mut console) };
        if hr < 0 {
            return Err(io::Error::from_raw_os_error(hr & 0xFFFF));
        }
        Ok(Pty {
            console: Arc::new(Console {
                console: Mutex::new(Some(console)),
            }),
            input: Arc::new(in_write),
            output: Arc::new(out_read),
        })
    }

    /// Starts `command` attached to the pseudo console.
    ///
    /// The child gets `TERM` set to [`DEFAULT_TERM`] unless the builder sets or
    /// removes it. A working directory that is not a directory is replaced by
    /// the user's profile directory.
    pub fn spawn(&self, command: &CommandBuilder) -> io::Result<Child> {
        let program = command
            .argv
            .first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty command"))?;
        let mut line = command_line(program, &command.argv[1..]);

        let mut unset: Vec<OsString> = Vec::new();
        let mut vars: Vec<(OsString, OsString)> = Vec::new();
        for (key, value) in &command.env {
            match value {
                Some(value) => vars.push((key.clone(), value.clone())),
                None => unset.push(key.clone()),
            }
        }
        if !command.has_term() {
            vars.push(("TERM".into(), DEFAULT_TERM.into()));
        }
        if command.clear_env {
            // Drop every inherited variable except the names the builder sets.
            // Windows names ignore case.
            let upper = |key: &std::ffi::OsStr| key.to_string_lossy().to_uppercase();
            let set: Vec<String> = vars.iter().map(|(key, _)| upper(key)).collect();
            unset.extend(
                std::env::vars_os()
                    .map(|(key, _)| key)
                    .filter(|key| !set.contains(&upper(key))),
            );
        }
        let mut environment = environment_block(&unset, &vars);

        let cwd: Option<PathBuf> = match &command.cwd {
            Some(cwd) if cwd.is_dir() => Some(cwd.clone()),
            // Without a usable directory the child starts in the profile
            // directory, not in the app's own directory.
            _ => std::env::var_os("USERPROFILE").map(PathBuf::from),
        };
        let cwd = cwd.map(|cwd| wide(cwd.as_os_str()));

        // Hold the lock until the process exists, so a waiter closing the
        // console on another thread cannot free it in between.
        let guard = self.console.lock();
        let console = guard.ok_or_else(|| io::Error::other("the pseudo console is closed"))?;

        let mut size = 0usize;
        // SAFETY: a null list with a size out pointer only asks how much space
        // the list needs. It fails with ERROR_INSUFFICIENT_BUFFER by design.
        unsafe {
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size);
        }
        let mut storage = vec![0u8; size];
        let list: *mut c_void = storage.as_mut_ptr().cast();
        // SAFETY: `storage` has the size the call asked for and outlives the
        // list, which is deleted before `storage` drops.
        unsafe {
            if InitializeProcThreadAttributeList(list, 1, 0, &mut size) == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        // SAFETY: the attribute value is the pseudo console handle itself, passed
        // by value as the documentation requires, and it outlives the call that
        // creates the process.
        let updated = unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                console as *const c_void,
                size_of::<HPCON>(),
                null_mut(),
                null(),
            )
        };
        if updated == 0 {
            let error = io::Error::last_os_error();
            // SAFETY: the list was initialized above.
            unsafe { DeleteProcThreadAttributeList(list) };
            return Err(error);
        }

        // SAFETY: all-zero is a valid value for these plain C structs.
        let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        // Name the standard handles as invalid so the child does not inherit
        // the parent's redirected ones (a CI pipe, a daemon's log) and writes
        // to the pseudo console instead.
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
        startup.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
        startup.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
        startup.lpAttributeList = list;
        // A limited child starts suspended and runs once it is in its
        // jobs, so nothing it starts escapes them.
        let job = command.cpu.as_ref().and_then(crate::cpu::windows::scope);
        let suspended = if job.is_some() { CREATE_SUSPENDED } else { 0 };
        // SAFETY: as above.
        let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: the command line and environment are NUL-terminated wide
        // buffers that live through the call, the command line is mutable as
        // `CreateProcessW` requires, `startup` carries a valid attribute list and
        // `info` is a live out parameter.
        let created = unsafe {
            CreateProcessW(
                null(),
                line.as_mut_ptr(),
                null(),
                null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | suspended,
                environment.as_mut_ptr().cast(),
                cwd.as_ref().map_or(null(), |cwd| cwd.as_ptr()),
                &startup.StartupInfo,
                &mut info,
            )
        };
        let error = io::Error::last_os_error();
        // SAFETY: the list was initialized above and is no longer needed.
        unsafe { DeleteProcThreadAttributeList(list) };
        if created == 0 {
            return Err(error);
        }
        drop(guard);
        if let Some(job) = &job {
            crate::cpu::windows::contain(info.hProcess, job);
            // SAFETY: the thread handle is the new process's main thread,
            // created suspended above.
            unsafe {
                ResumeThread(info.hThread);
            }
        }
        drop(Handle(info.hThread));
        Ok(Child {
            process: Arc::new(Handle(info.hProcess)),
            pid: info.dwProcessId,
            console: self.console.clone(),
            status: None,
            _job: job,
        })
    }

    /// A blocking reader for the master. It reads end of input once the child
    /// has ended and the pseudo console is closed.
    pub fn reader(&self) -> io::Result<PtyReader> {
        Ok(PtyReader {
            output: self.output.clone(),
        })
    }

    /// A blocking writer for the master: bytes written arrive as typed input.
    pub fn writer(&self) -> io::Result<PtyWriter> {
        Ok(PtyWriter {
            input: self.input.clone(),
        })
    }

    /// The foreground process group of the terminal. Windows has none.
    pub fn foreground_group(&self) -> Option<i32> {
        None
    }

    pub fn resize(&self, size: PtySize) -> io::Result<()> {
        let guard = self.console.lock();
        let console = guard.ok_or_else(|| io::Error::other("the pseudo console is closed"))?;
        // SAFETY: the console handle is open while the lock guard is held, and
        // `guard` lives until after the call.
        let hr = unsafe { ResizePseudoConsole(console, coord(size)) };
        drop(guard);
        if hr < 0 {
            return Err(io::Error::from_raw_os_error(hr & 0xFFFF));
        }
        Ok(())
    }
}

pub struct PtyReader {
    output: Arc<Handle>,
}

impl Read for PtyReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut read = 0u32;
        let len = buf.len().min(u32::MAX as usize) as u32;
        // SAFETY: `buf` is valid for `len` bytes, `read` is a live local and
        // the handle is open while this value is alive. The call is not
        // overlapped.
        let ok = unsafe { ReadFile(self.output.0, buf.as_mut_ptr(), len, &mut read, null_mut()) };
        if ok != 0 {
            return Ok(read as usize);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error().map(|code| code as u32) {
            Some(ERROR_BROKEN_PIPE | ERROR_NO_DATA | ERROR_PIPE_NOT_CONNECTED) => Ok(0),
            _ => Err(error),
        }
    }
}

pub struct PtyWriter {
    input: Arc<Handle>,
}

impl Write for PtyWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut written = 0u32;
        let len = buf.len().min(u32::MAX as usize) as u32;
        // SAFETY: `buf` is valid for `len` bytes, `written` is a live local and
        // the handle is open while this value is alive. The call is not
        // overlapped.
        let ok = unsafe { WriteFile(self.input.0, buf.as_ptr(), len, &mut written, null_mut()) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(written as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A running child. Dropping it neither kills nor waits for the process.
pub struct Child {
    process: Arc<Handle>,
    pid: u32,
    console: Arc<Console>,
    status: Option<ExitStatus>,
    /// The CPU job the process runs in, kept while the child is, so the
    /// app can change its limits.
    _job: Option<Arc<crate::cpu::windows::Job>>,
}

impl Child {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    fn finish(&mut self) -> io::Result<ExitStatus> {
        let mut code = 0u32;
        // SAFETY: the process handle is open and `code` is a live local.
        if unsafe { GetExitCodeProcess(self.process.0, &mut code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let status = ExitStatus::from_code(code);
        self.status = Some(status);
        // Closing can wait for the reader to drain the output on older
        // Windows, so it must not block the caller.
        let console = self.console.clone();
        std::thread::spawn(move || console.close());
        Ok(status)
    }

    /// Blocks until the child ends.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        // SAFETY: the process handle is open.
        if unsafe { WaitForSingleObject(self.process.0, INFINITE) } != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
        self.finish()
    }

    /// The status if the child has ended, without blocking.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        // SAFETY: the process handle is open.
        match unsafe { WaitForSingleObject(self.process.0, 0) } {
            WAIT_OBJECT_0 => self.finish().map(Some),
            WAIT_TIMEOUT => Ok(None),
            _ => Err(io::Error::last_os_error()),
        }
    }

    /// Ends the child.
    pub fn kill(&mut self) -> io::Result<()> {
        terminate(&self.process)
    }

    /// A handle other threads can use to end the child.
    pub fn killer(&self) -> ChildKiller {
        ChildKiller {
            process: self.process.clone(),
        }
    }
}

fn terminate(process: &Handle) -> io::Result<()> {
    // SAFETY: the process handle stays open while the `Arc` is alive, so it
    // cannot refer to a reused process id. Terminating an ended process fails
    // harmlessly.
    if unsafe { TerminateProcess(process.0, 1) } == 0 {
        let error = io::Error::last_os_error();
        // SAFETY: as above.
        let mut code = 0u32;
        let ended = unsafe { GetExitCodeProcess(process.0, &mut code) } != 0 && code != 259; // STILL_ACTIVE
        if !ended {
            return Err(error);
        }
    }
    Ok(())
}

#[derive(Clone)]
pub struct ChildKiller {
    process: Arc<Handle>,
}

impl ChildKiller {
    pub fn kill(&self) -> io::Result<()> {
        terminate(&self.process)
    }
}
