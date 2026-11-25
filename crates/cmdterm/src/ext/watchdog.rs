//! Wall-clock budget for calls into Lua.
//!
//! Oximond has no instruction-count hook for hosts, but its VM polls a host
//! `AtomicBool` and raises a catchable `interrupted!` error when it is set.
//! One background thread per host sets the running VM's flag once the
//! deadline passes, and keeps setting it every millisecond until the call
//! returns. The VM's `pcall`, `xpcall` and `coroutine.resume` are wrapped
//! (see `api::PROTECT`) so they re-raise once the deadline passed, which
//! keeps a script from swallowing the interrupt forever.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often an expired deadline re-raises the interrupt.
const REPEAT: Duration = Duration::from_millis(1);

/// The flags one VM's call is timed with.
#[derive(Clone, Default)]
pub(crate) struct Alarm {
    /// Polled by the VM; set raises a catchable `interrupted!` error.
    pub(crate) interrupt: Arc<AtomicBool>,
    /// Set once the deadline passed, until the call returns. The protected
    /// call wrappers read it to re-raise errors they would swallow.
    pub(crate) expired: Arc<AtomicBool>,
}

#[derive(Default)]
struct State {
    armed: Option<(Instant, Alarm)>,
    fired: bool,
    shutdown: bool,
}

type Shared = Arc<(Mutex<State>, Condvar)>;

pub(crate) struct Watchdog {
    shared: Shared,
    thread: Option<JoinHandle<()>>,
    /// Set when the thread could not be spawned, so we stop trying.
    unavailable: bool,
}

fn lock(shared: &Shared) -> MutexGuard<'_, State> {
    // The lock guards plain data, so a poisoned lock is still consistent.
    shared
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Watchdog {
    pub(crate) fn new() -> Self {
        Self {
            shared: Arc::default(),
            thread: None,
            unavailable: false,
        }
    }

    /// Starts timing a call. Returns false when no budget can be enforced.
    pub(crate) fn arm(&mut self, budget: Duration, alarm: &Alarm) -> bool {
        if self.thread.is_none() && !self.unavailable {
            let shared = Arc::clone(&self.shared);
            match std::thread::Builder::new()
                .name("cmdterm-ext-watchdog".into())
                .spawn(move || run(&shared))
            {
                Ok(handle) => self.thread = Some(handle),
                Err(error) => {
                    self.unavailable = true;
                    eprintln!("[cmdterm-ext] no time budget for extensions: {error}");
                    crate::logs::error(
                        "extensions",
                        format!("no time budget for extensions: {error}"),
                    );
                }
            }
        }
        if self.thread.is_none() {
            return false;
        }
        alarm.interrupt.store(false, Ordering::Relaxed);
        alarm.expired.store(false, Ordering::Relaxed);
        let mut state = lock(&self.shared);
        state.armed = Some((Instant::now() + budget, alarm.clone()));
        state.fired = false;
        drop(state);
        self.shared.1.notify_one();
        true
    }

    /// Stops timing. Returns whether the deadline passed during the call.
    pub(crate) fn disarm(&mut self) -> bool {
        let mut state = lock(&self.shared);
        if let Some((_, alarm)) = state.armed.take() {
            // Under the lock, so the thread cannot set them again afterwards.
            alarm.interrupt.store(false, Ordering::Relaxed);
            alarm.expired.store(false, Ordering::Relaxed);
        }
        std::mem::take(&mut state.fired)
    }
}

fn run(shared: &Shared) {
    let mut state = lock(shared);
    loop {
        if state.shutdown {
            return;
        }
        let wait = match &state.armed {
            None => None,
            Some((deadline, alarm)) => {
                let now = Instant::now();
                if now >= *deadline {
                    alarm.expired.store(true, Ordering::Relaxed);
                    alarm.interrupt.store(true, Ordering::Relaxed);
                    state.fired = true;
                    Some(REPEAT)
                } else {
                    Some(*deadline - now)
                }
            }
        };
        state = match wait {
            None => shared
                .1
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            Some(timeout) => {
                shared
                    .1
                    .wait_timeout(state, timeout)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0
            }
        };
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        lock(&self.shared).shutdown = true;
        self.shared.1.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
