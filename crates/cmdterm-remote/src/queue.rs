//! A queue from a connection's reader to one writer thread, bounded by the
//! bytes waiting in it.
//!
//! The protocol has no flow control per channel: a connection's reader
//! hands each message to its channel and goes on to the next. Input for a
//! program that reads slowly, or output for an `exec` that does not read,
//! waits in one of these queues. The sender chooses what happens when it is
//! full: wait for room, which holds up the other channels of the
//! connection like TCP itself would, or give up on the channel.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// What every item costs on top of the cost its sender gives it, so empty
/// items fill a queue too.
pub const ITEM_COST: usize = 64;

/// What a [`Sender::send`] could not do.
#[derive(Debug, PartialEq, Eq)]
pub enum SendError {
    /// There was no room in time.
    Full,
    /// The receiver is gone.
    Gone,
}

struct State<T> {
    items: VecDeque<(T, usize)>,
    /// The cost of `items`.
    bytes: usize,
    /// The sender is gone: the receiver ends once `items` are taken.
    closed: bool,
    /// The receiver is gone: sends fail.
    gone: bool,
}

struct Shared<T> {
    state: Mutex<State<T>>,
    changed: Condvar,
    limit: usize,
}

impl<T> Shared<T> {
    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A queue that holds up to `limit` bytes, counting [`ITEM_COST`] for each
/// item. One item always fits into an empty queue, whatever its cost.
pub fn bounded<T>(limit: usize) -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            items: VecDeque::new(),
            bytes: 0,
            closed: false,
            gone: false,
        }),
        changed: Condvar::new(),
        limit,
    });
    (
        Sender {
            shared: shared.clone(),
        },
        Receiver { shared },
    )
}

/// The sending end. Dropping it closes the queue.
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Sender<T> {
    /// Queues `item`, which counts `cost` plus [`ITEM_COST`] bytes, once
    /// there is room for it. Waits up to `wait` for room, without a limit
    /// when `None`.
    pub fn send(&self, item: T, cost: usize, wait: Option<Duration>) -> Result<(), SendError> {
        let cost = cost.saturating_add(ITEM_COST);
        let until = wait.map(|wait| Instant::now() + wait);
        let mut state = self.shared.lock();
        loop {
            if state.gone {
                return Err(SendError::Gone);
            }
            if state.bytes == 0 || state.bytes + cost <= self.shared.limit {
                break;
            }
            state = match until {
                None => self
                    .shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner),
                Some(until) => {
                    let now = Instant::now();
                    if now >= until {
                        return Err(SendError::Full);
                    }
                    self.shared
                        .changed
                        .wait_timeout(state, until - now)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
            };
        }
        state.items.push_back((item, cost));
        state.bytes += cost;
        drop(state);
        self.shared.changed.notify_all();
        Ok(())
    }

    /// Queues `item` whatever the queue holds. Only for messages a channel
    /// sends once, such as its close, so the queue stays bounded.
    pub fn force(&self, item: T) -> Result<(), SendError> {
        self.force_replacing(item, |_| false)
    }

    /// Queues `item` at the end whatever the queue holds, after taking out
    /// the queued items `replaces` picks. For a message only whose newest
    /// copy matters, such as a terminal's size: at most one of them waits,
    /// so the queue stays bounded.
    pub fn force_replacing(&self, item: T, replaces: impl Fn(&T) -> bool) -> Result<(), SendError> {
        let mut state = self.shared.lock();
        if state.gone {
            return Err(SendError::Gone);
        }
        let mut freed = 0;
        state.items.retain(|(queued, cost)| {
            let keep = !replaces(queued);
            if !keep {
                freed += cost;
            }
            keep
        });
        state.bytes = state.bytes - freed + ITEM_COST;
        state.items.push_back((item, ITEM_COST));
        drop(state);
        self.shared.changed.notify_all();
        Ok(())
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.shared.lock().closed = true;
        self.shared.changed.notify_all();
    }
}

/// The receiving end. Dropping it makes every send fail.
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Receiver<T> {
    /// The next item, waiting for one. `None` once the sender is gone and
    /// every item was taken.
    pub fn recv(&self) -> Option<T> {
        let mut state = self.shared.lock();
        loop {
            if let Some((item, cost)) = state.items.pop_front() {
                state.bytes -= cost;
                drop(state);
                self.shared.changed.notify_all();
                return Some(item);
            }
            if state.closed {
                return None;
            }
            state = self
                .shared
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.gone = true;
        state.items.clear();
        state.bytes = 0;
        drop(state);
        self.shared.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn a_full_queue_waits_for_room_or_gives_up() {
        let (tx, rx) = bounded::<Vec<u8>>(10 + 2 * ITEM_COST);
        tx.send(vec![0; 6], 6, Some(Duration::ZERO)).unwrap();
        assert_eq!(
            tx.send(vec![0; 6], 6, Some(Duration::from_millis(20))),
            Err(SendError::Full)
        );
        tx.send(vec![0; 4], 4, Some(Duration::ZERO)).unwrap();
        // Room comes when the receiver takes an item.
        let taker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            let first = rx.recv().unwrap();
            (first, rx)
        });
        tx.send(vec![0; 6], 6, None).unwrap();
        let (first, rx) = taker.join().unwrap();
        assert_eq!(first.len(), 6);
        drop(tx);
        assert_eq!(rx.recv().unwrap().len(), 4);
        assert_eq!(rx.recv().unwrap().len(), 6);
        assert!(rx.recv().is_none(), "closed once the sender is gone");
    }

    #[test]
    fn one_large_item_fits_an_empty_queue_and_sends_fail_without_a_receiver() {
        let (tx, rx) = bounded::<Vec<u8>>(10);
        tx.send(vec![0; 100], 100, Some(Duration::ZERO)).unwrap();
        assert_eq!(rx.recv().unwrap().len(), 100);
        tx.force(vec![1]).unwrap();
        drop(rx);
        assert_eq!(tx.send(vec![0], 1, None), Err(SendError::Gone));
        assert_eq!(tx.force(vec![0]), Err(SendError::Gone));
    }

    /// Empty items cost something too, so their number is bounded.
    #[test]
    fn empty_items_fill_the_queue() {
        let (tx, rx) = bounded::<Vec<u8>>(10 * ITEM_COST);
        let mut sent = 0;
        while tx.send(Vec::new(), 0, Some(Duration::ZERO)).is_ok() {
            sent += 1;
            assert!(sent <= 10, "more empty items than the limit allows");
        }
        assert_eq!(sent, 10);
        rx.recv().unwrap();
        tx.send(Vec::new(), 0, Some(Duration::ZERO)).unwrap();
    }

    /// A replacing item takes the place of the queued ones it replaces,
    /// goes to the end, and fits a full queue.
    #[test]
    fn a_replacing_item_takes_the_place_of_older_copies() {
        let (tx, rx) = bounded::<(u8, u8)>(2 * ITEM_COST);
        tx.send((0, 1), 0, Some(Duration::ZERO)).unwrap();
        tx.send((1, 1), 0, Some(Duration::ZERO)).unwrap();
        assert_eq!(
            tx.send((0, 2), 0, Some(Duration::ZERO)),
            Err(SendError::Full)
        );
        for size in 2..100 {
            tx.force_replacing((1, size), |item| item.0 == 1).unwrap();
        }
        drop(tx);
        assert_eq!(rx.recv(), Some((0, 1)));
        assert_eq!(rx.recv(), Some((1, 99)));
        assert_eq!(rx.recv(), None);
    }
}
