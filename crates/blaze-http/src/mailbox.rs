//! The handles' side of the drivers ([`crate::driver`], and h3's under `h3`): what an asker
//! on another thread hands a driver's loop, and how it waits for the loop's answer. No future,
//! waker or executor is under it: a driver's thread runs a plain blocking loop over its
//! sans-IO machine, and an asker makes one timed receive.
//!
//! - [`Inbox`]: the asks a driver's thread drains at the top of each turn, posted from any
//!   thread on blaze-core's one channel ([`blaze_core::chan`], rule standard-structures), each
//!   post then waking the thread's driver ([`blaze_core::Waker`]).
//! - The loop's answers go back on blaze-core's channel too, and an asker waits for one with
//!   [`answer`]: a receive that takes how long it may wait, so no asker's wait is open (rules
//!   bounded-progress, deadline-waits), and one that passes is [`Unanswered::TimedOut`] naming
//!   what it waited for (rule every-wait-says-what).
//! - [`latch`]: a value set once and read by any number of waiters (how a session ended, a
//!   stream's stop): the value under a named lock, beside a channel that never carries one
//!   whose one sender the set drops, so every waiter's timed receive ends at the set.

use std::{alloc::Global, convert::Infallible, sync::Arc, time::Duration};

use blaze_core::{
    chan::{self, Receiver, RecvTimeoutError, Sender},
    lock::Lock,
};

/// Why an asker's wait for the loop ended with no answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unanswered {
    /// `within` passed while it waited for `what`, the loop still running.
    TimedOut { what: &'static str, within: Duration },
}

impl std::fmt::Display for Unanswered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unanswered::TimedOut { what, within } => {
                write!(f, "waiting for {what}: no answer in {within:?} (rule bounded-progress)")
            }
        }
    }
}

impl std::error::Error for Unanswered {}

/// The next item of `from` within `within`, waiting for `what`: `Ok(None)` once every sender is
/// gone and every item read (the loop stopped, or it said all it will).
pub fn answer<T>(what: &'static str, from: &Receiver<T>, within: Duration) -> Result<Option<T>, Unanswered> {
    match from.recv_timeout_named(what, within) {
        Ok(item) => Ok(Some(item)),
        Err(RecvTimeoutError::Disconnected) => Ok(None),
        Err(RecvTimeoutError::Timeout) => Err(Unanswered::TimedOut { what, within }),
    }
}

/// A latch's one setter: [`Setter::set`] spends it; dropped unset, its waiters read `None`.
pub struct Setter<T> {
    value: Arc<Lock<Option<T>>, Global>,
    /// Never sends: its drop, after the value is in, is what ends every waiter's receive.
    _open: Sender<Infallible>,
}

/// A latch's waiters' side, cloned to each.
pub struct Latch<T> {
    value: Arc<Lock<Option<T>>, Global>,
    set: Receiver<Infallible>,
}

/// A value set once, read by every waiter.
pub fn latch<T>() -> (Setter<T>, Latch<T>) {
    let (open, set) = chan::bounded(1);
    let value = Arc::new_in(Lock::new("http.mailbox.latch", None), Global);
    (Setter { value: Arc::clone(&value), _open: open }, Latch { value, set })
}

impl<T> Setter<T> {
    /// Sets the value, then (as the setter drops) wakes every waiter: the value is in before
    /// any waiter's receive ends.
    pub fn set(self, value: T) {
        *self.value.lock() = Some(value);
    }
}

impl<T> Clone for Latch<T> {
    fn clone(&self) -> Latch<T> {
        Latch { value: Arc::clone(&self.value), set: self.set.clone() }
    }
}

impl<T: Clone> Latch<T> {
    /// The value once it is set, waiting at most `within` for `what`; `Ok(None)` when its
    /// setter went away unset.
    pub fn wait(&self, what: &'static str, within: Duration) -> Result<Option<T>, Unanswered> {
        match self.set.recv_timeout_named(what, within) {
            Ok(never) => match never {},
            Err(RecvTimeoutError::Disconnected) => Ok(self.get()),
            Err(RecvTimeoutError::Timeout) => Err(Unanswered::TimedOut { what, within }),
        }
    }

    /// The value, if it is set.
    pub fn get(&self) -> Option<T> {
        self.value.lock().clone() // clone: each waiter reads its own
    }
}

/// A driver thread's inbox: askers on any thread post to it, then wake the thread's
/// blaze-core driver; the thread drains everything posted at the top of each turn. Not bounded:
/// a handle's drop posts its cancel or close, which has nowhere to go if refused; every ask
/// that grows it is an asker's own, each bounded by its wait.
pub struct Inbox<M> {
    to: Sender<M>,
    from: Receiver<M>,
    waker: blaze_core::Waker,
}

impl<M> Inbox<M> {
    /// An inbox whose posts wake the driver `waker` is of ([`blaze_core::Driver::waker`]).
    pub fn new(waker: blaze_core::Waker) -> Inbox<M> {
        let (to, from) = chan::unbounded();
        Inbox { to, from, waker }
    }

    /// Posts `message`, then wakes the driver: the order that loses no wake.
    pub fn post(&self, message: M) {
        assert!(self.to.send(message).is_ok(), "blaze-http's inbox refused a post: it holds its own receiver, so it never disconnects");
        self.waker.wake();
    }

    /// Moves everything posted so far into `into`.
    pub fn take(&self, into: &mut Vec<M, Global>) {
        into.extend(self.from.try_iter());
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use blaze_core::Driver;

    use super::*;

    /// How long a test's wait for something already there may take: never reached.
    const THERE: Duration = Duration::from_secs(5);
    /// How long a test waits for what never comes.
    const NEVER: Duration = Duration::from_millis(5);

    /// What the loop sends comes to the asker in order; once every sender is gone the asker
    /// reads `None`; with a sender alive and nothing sent, the wait ends at `within` in the
    /// typed timeout naming what it waited for. Prints nothing; a wrong `answer` fails with
    /// `left: Err(TimedOut { what: "the test's items", .. })` or the like.
    #[test]
    fn an_answer_comes_in_order_ends_with_its_senders_and_times_out_naming_what() {
        let (tx, rx) = chan::unbounded();
        let other = tx.clone();
        assert!(tx.send(1).is_ok());
        assert!(other.send(2).is_ok());
        let alive = other.clone();
        drop((tx, other));
        assert_eq!(answer("the test's items", &rx, THERE), Ok(Some(1)));
        assert_eq!(answer("the test's items", &rx, THERE), Ok(Some(2)));
        assert_eq!(answer("the test's items", &rx, NEVER), Err(Unanswered::TimedOut { what: "the test's items", within: NEVER }));
        drop(alive);
        assert_eq!(answer("the test's items", &rx, THERE), Ok(None));
    }

    /// A latch is read by every waiter once it is set; as `None` once its setter went away
    /// unset; and while it is neither, a wait ends at `within` in the typed timeout.
    #[test]
    fn a_latch_is_read_by_every_waiter() {
        let (set, latch) = latch();
        let other = latch.clone();
        assert_eq!(latch.wait("the test's latch", NEVER), Err(Unanswered::TimedOut { what: "the test's latch", within: NEVER }));
        set.set(4);
        assert_eq!((latch.wait("the test's latch", THERE), other.wait("the test's latch", THERE)), (Ok(Some(4)), Ok(Some(4))));
        let (set, latch) = super::latch::<u8>();
        drop(set);
        assert_eq!(latch.wait("the test's latch", THERE), Ok(None));
    }

    /// Everything posted to an inbox is taken in order at the next turn, and each post wakes
    /// the driver: a post after the take is there at the turn after.
    #[test]
    fn an_inbox_hands_every_post_to_the_next_take_in_order() {
        let mut sim = blaze_core::sim::Sim::new(1, blaze_core::sim::Faults::default());
        sim.give_wakers();
        let Some(waker) = sim.waker() else { panic!("Sim::give_wakers gave no waker") };
        let inbox = Inbox::new(waker);
        inbox.post(1);
        inbox.post(2);
        let mut taken = Vec::new_in(Global);
        inbox.take(&mut taken);
        assert_eq!(taken.as_slice(), &[1, 2]);
        inbox.post(3);
        taken.clear();
        inbox.take(&mut taken);
        assert_eq!(taken.as_slice(), &[3]);
    }
}
