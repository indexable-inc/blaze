//! blaze's one channel (rule standard-structures): many senders, many receivers, bounded
//! ([`bounded`]: a [`blaze_queue::ArrayQueue`], or a rendezvous for a capacity of 0) or not
//! ([`unbounded`]: a [`blaze_queue::SegQueue`]). The names and errors are those of the
//! `crossbeam-channel` it replaces, so a caller changes only its path.
//!
//! The values move through the lock-free queues; only a thread with nothing to do waits. A
//! side that may wait has an eventcount ([`Waiters`]): a sequence word a waiter sleeps on in
//! the kernel ([`crate::lock::raw::Word`], a futex on Linux, the wait on an address on macOS)
//! and a count of those about to sleep. A waiter reads the sequence, counts itself, looks at
//! the queue again and sleeps only while the sequence is the one it read; the other side moves
//! the sequence and wakes one only when the count says someone waits, so an uncontended send
//! or receive makes no system call.
//!
//! Orderings, in one line: the waiter's count is a `SeqCst` read-modify-write before its
//! second look, and the notifier's `SeqCst` fence sits between its queue operation (itself
//! `SeqCst` at the queue's ends) and its relaxed read of the count, so one of the two sees the
//! other (Dekker's pattern); the sequence is read `SeqCst` and moved `Release`, and the
//! kernel compares it in the step that blocks. The senders' and receivers' counts are
//! `SeqCst`, so a disconnect is seen by a waiter's second look or wakes it.
//!
//! A wait is the signal span of the channel, named by its value type ([`signal_span`]), or by
//! what the caller says it waits for ([`Receiver::recv_timeout_named`]), with its row in the
//! wait-for graph. A rendezvous (capacity 0) is a slot under a [`raw::Mutex`]
//! and a [`raw::Cond`]: it is for gates and handshakes, not throughput.
//!
//! Under `--cfg blaze_check` the atomics and the word's wait are the model checker's;
//! `chan/tests.rs` checks the protocols and plants a bug in each ([`Plant`]).

use std::{
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

// std's atomics as the locks' (`lock::raw`): a channel is a real handoff in blaze-core's tests,
// never the scheduled atomics; under `--cfg blaze_check`, the model checker's.
#[cfg(not(blaze_check))]
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering, fence};

#[cfg(blaze_check)]
use blaze_check::sync::atomic::{AtomicU32, AtomicUsize, Ordering, fence};
use blaze_queue::{ArrayQueue, SegQueue};

use crate::lock::{
    raw::{self, Word},
    signal_span,
};

#[cfg(test)]
mod tests;

/// A safeguard a model test removes, to see the model catch the removal. Production code
/// passes [`Plant::None`], a constant the branch folds away.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// A notify reads the waiters' count without the fence before it: a receiver that
    /// counted itself after the sender's look, but looked at the queue before the push, sleeps
    /// through the value.
    #[cfg(test)]
    Unfenced,
    /// The last sender's drop moves nothing: a receiver asleep on an empty channel never
    /// hears that it is disconnected.
    #[cfg(test)]
    SilentDisconnect,
}

// --- Waiters ------------------------------------------------------------------------------------

/// The threads of one side that wait for the other: an eventcount.
struct Waiters {
    seq: Word,
    waiting: AtomicU32,
}

impl Waiters {
    const fn new() -> Waiters {
        Waiters { seq: Word::new(0), waiting: AtomicU32::new(0) }
    }

    /// Wakes one waiter, if any: after a change one waiter can use (a value sent, a slot
    /// freed).
    #[inline]
    fn notify(&self, plant: Plant) {
        #[cfg(test)]
        let fenced = plant != Plant::Unfenced;
        #[cfg(not(test))]
        let fenced = {
            _ = plant;
            true
        };
        if fenced {
            fence(Ordering::SeqCst);
        }
        if self.waiting.load(Ordering::Relaxed) != 0 {
            self.seq.value.fetch_add(1, Ordering::Release);
            self.seq.wake(false);
        }
    }

    /// Wakes every waiter: after a change all of them must see (a disconnect).
    fn notify_all(&self) {
        self.seq.value.fetch_add(1, Ordering::SeqCst);
        self.seq.wake(true);
    }

    /// Waits until `ready` gives a value, or `deadline` passes (`None` then). `ready` is
    /// asked first, again after this thread is counted, and after each wake.
    fn wait<R>(
        &self,
        name: &'static str,
        deadline: Option<Instant>,
        mut ready: impl FnMut() -> Option<R>,
    ) -> Option<R> {
        if let Some(it) = ready() {
            return Some(it);
        }
        signal_span(name, deadline.map(|at| at.saturating_duration_since(Instant::now())), || {
            loop {
                let seq = self.seq.value.load(Ordering::SeqCst);
                self.waiting.fetch_add(1, Ordering::SeqCst);
                if let Some(it) = ready() {
                    self.waiting.fetch_sub(1, Ordering::Relaxed);
                    return Some(it);
                }
                let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
                if left == Some(Duration::ZERO) {
                    self.waiting.fetch_sub(1, Ordering::Relaxed);
                    return None;
                }
                // wakes: `Waiters::notify` after the other side's change, `notify_all` at a
                // disconnect
                self.seq.wait(seq, left);
                self.waiting.fetch_sub(1, Ordering::Relaxed);
                if let Some(it) = ready() {
                    return Some(it);
                }
            }
        })
    }
}

// --- Rendezvous ---------------------------------------------------------------------------------

/// A channel of capacity 0: a send waits until a receive takes its value.
struct Rendezvous<T> {
    hand: raw::Mutex<Hand<T>>,
    cond: raw::Cond,
}

struct Hand<T> {
    value: Option<T>,
    /// Values put in the slot, and taken from it, since the channel was made.
    put: u64,
    taken: u64,
    /// Receivers waiting in the slot's lock now: a `try_send` succeeds only to one of them.
    receiving: usize,
}

// --- The channel --------------------------------------------------------------------------------

enum Flavor<T> {
    Array(ArrayQueue<T, std::alloc::Global>),
    List(SegQueue<T, std::alloc::Global>),
    Zero(Rendezvous<T>),
}

struct Shared<T> {
    flavor: Flavor<T>,
    senders: AtomicUsize,
    receivers: AtomicUsize,
    /// Receivers waiting for a value (or a disconnect).
    sent: Waiters,
    /// Senders of a full ring waiting for a slot (or a disconnect).
    received: Waiters,
}

impl<T> Shared<T> {
    fn new(flavor: Flavor<T>, senders: usize) -> Arc<Shared<T>> {
        Arc::new(Shared {
            flavor,
            senders: AtomicUsize::new(senders),
            receivers: AtomicUsize::new(1),
            sent: Waiters::new(),
            received: Waiters::new(),
        })
    }

    fn name() -> &'static str {
        std::any::type_name::<T>()
    }

    fn disconnected_from_senders(&self) -> bool {
        self.senders.load(Ordering::SeqCst) == 0
    }

    fn disconnected_from_receivers(&self) -> bool {
        self.receivers.load(Ordering::SeqCst) == 0
    }

    fn pop(&self) -> Option<T> {
        match &self.flavor {
            Flavor::Array(queue) => {
                let value = queue.pop()?;
                self.received.notify(Plant::None);
                Some(value)
            }
            Flavor::List(queue) => queue.pop(),
            Flavor::Zero(_) => unreachable!("a rendezvous is taken under its lock"),
        }
    }

    fn send(&self, value: T, deadline: Option<Instant>, plant: Plant) -> Result<(), SendTimeoutError<T>> {
        match &self.flavor {
            Flavor::List(queue) => {
                if self.disconnected_from_receivers() {
                    return Err(SendTimeoutError::Disconnected(value));
                }
                queue.push(value);
                self.sent.notify(plant);
                Ok(())
            }
            Flavor::Array(queue) => {
                let mut held = Some(value);
                let done = self.received.wait(Self::name(), deadline, || {
                    let value = held.take().expect("a value until it is sent or refused");
                    if self.disconnected_from_receivers() {
                        return Some(Err(value));
                    }
                    match queue.push(value) {
                        Ok(()) => Some(Ok(())),
                        Err(back) => {
                            held = Some(back);
                            None
                        }
                    }
                });
                match done {
                    Some(Ok(())) => {
                        self.sent.notify(plant);
                        Ok(())
                    }
                    Some(Err(value)) => Err(SendTimeoutError::Disconnected(value)),
                    None => Err(SendTimeoutError::Timeout(held.take().expect("not sent"))),
                }
            }
            Flavor::Zero(zero) => self.hand_over(zero, value, deadline),
        }
    }

    /// A receive whose wait's span is `name` ([`Shared::name`] unless the caller says what it
    /// waits for).
    fn recv(&self, name: &'static str, deadline: Option<Instant>) -> Result<T, RecvTimeoutError> {
        if let Flavor::Zero(zero) = &self.flavor {
            return self.take_over(zero, name, deadline);
        }
        let got = self.sent.wait(name, deadline, || {
            if let Some(value) = self.pop() {
                return Some(Ok(value));
            }
            // Gone: a value sent before the last sender's drop is still taken.
            self.disconnected_from_senders().then(|| self.pop().ok_or(RecvTimeoutError::Disconnected))
        });
        got.unwrap_or(Err(RecvTimeoutError::Timeout))
    }

    /// A rendezvous send: the value goes into the empty slot, and the send waits for its take.
    fn hand_over(
        &self,
        zero: &Rendezvous<T>,
        value: T,
        deadline: Option<Instant>,
    ) -> Result<(), SendTimeoutError<T>> {
        let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
        signal_span(Self::name(), left, || {
            let mut hand = zero.hand.lock();
            loop {
                if self.disconnected_from_receivers() {
                    return Err(SendTimeoutError::Disconnected(value));
                }
                if hand.value.is_none() {
                    break;
                }
                if !wait_in(&zero.cond, &mut hand, deadline) {
                    return Err(SendTimeoutError::Timeout(value));
                }
            }
            hand.value = Some(value);
            hand.put += 1;
            let ticket = hand.put;
            zero.cond.notify(true);
            loop {
                if hand.taken >= ticket {
                    return Ok(());
                }
                // Not taken: the slot still holds this send's value.
                if self.disconnected_from_receivers() {
                    let back = hand.value.take().expect("the value not taken");
                    zero.cond.notify(true);
                    return Err(SendTimeoutError::Disconnected(back));
                }
                if !wait_in(&zero.cond, &mut hand, deadline) && hand.taken < ticket {
                    let back = hand.value.take().expect("the value not taken");
                    zero.cond.notify(true);
                    return Err(SendTimeoutError::Timeout(back));
                }
            }
        })
    }

    /// A rendezvous receive: takes the slot's value, waiting for a send to put one.
    fn take_over(
        &self,
        zero: &Rendezvous<T>,
        name: &'static str,
        deadline: Option<Instant>,
    ) -> Result<T, RecvTimeoutError> {
        let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
        signal_span(name, left, || {
            let mut hand = zero.hand.lock();
            hand.receiving += 1;
            let got = loop {
                if let Some(value) = hand.value.take() {
                    hand.taken += 1;
                    zero.cond.notify(true);
                    break Ok(value);
                }
                if self.disconnected_from_senders() {
                    break Err(RecvTimeoutError::Disconnected);
                }
                if !wait_in(&zero.cond, &mut hand, deadline) {
                    break Err(RecvTimeoutError::Timeout);
                }
            };
            hand.receiving -= 1;
            got
        })
    }

    /// Wakes the other side's waiters after this side's last end dropped. The rendezvous
    /// takes its lock first, so a waiter between its look and its sleep hears it.
    fn disconnect(&self, waiters: &Waiters, plant: Plant) {
        #[cfg(test)]
        if plant == Plant::SilentDisconnect {
            return;
        }
        _ = plant;
        match &self.flavor {
            Flavor::Zero(zero) => {
                drop(zero.hand.lock());
                zero.cond.notify(true);
            }
            Flavor::Array(_) | Flavor::List(_) => waiters.notify_all(),
        }
    }
}

/// One wait of a rendezvous's condition by `deadline`: false once it has passed.
fn wait_in<T>(cond: &raw::Cond, hand: &mut raw::MutexGuard<'_, Hand<T>>, deadline: Option<Instant>) -> bool {
    match deadline {
        None => {
            cond.wait(hand, None);
            true
        }
        Some(at) => {
            let left = at.saturating_duration_since(Instant::now());
            !left.is_zero() && {
                cond.wait(hand, Some(left));
                true
            }
        }
    }
}

/// A channel of at most `cap` values in flight; `cap` 0 is a rendezvous, whose send waits for
/// its receive. The ring is allocated now, from `Global`.
pub fn bounded<T>(cap: usize) -> (Sender<T>, Receiver<T>) {
    let flavor = match cap {
        0 => Flavor::Zero(Rendezvous {
            hand: raw::Mutex::new(Hand { value: None, put: 0, taken: 0, receiving: 0 }),
            cond: raw::Cond::new(),
        }),
        cap => Flavor::Array(ArrayQueue::new(cap)),
    };
    ends(Shared::new(flavor, 1))
}

/// A channel with no bound: a send never waits. Its blocks are allocated as it grows, from
/// `Global`; a full-speed producer with no consumer grows it without end, so prefer
/// [`bounded`] where the producer can be told to wait (`blaze::unbounded_channel`).
pub fn unbounded<T>() -> (Sender<T>, Receiver<T>) {
    ends(Shared::new(Flavor::List(SegQueue::new()), 1))
}

/// A receiver no value ever arrives on and that is never disconnected: for a select's branch
/// that is off, or a stop that never comes.
pub fn never<T>() -> Receiver<T> {
    // A sender counted that no `Sender` is: the count never reaches 0.
    Receiver { shared: Shared::new(Flavor::List(SegQueue::new()), 1) }
}

fn ends<T>(shared: Arc<Shared<T>>) -> (Sender<T>, Receiver<T>) {
    (Sender { shared: Arc::clone(&shared) }, Receiver { shared })
}

// --- Sender -------------------------------------------------------------------------------------

/// The sending end; a clone is another sender of the same channel.
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Sender<T> {
    /// Sends `value`, waiting for room on a full bounded channel; refused once every receiver
    /// is gone.
    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        self.shared.send(value, None, Plant::None).map_err(|it| SendError(it.into_inner()))
    }

    pub(crate) fn send_with(&self, value: T, plant: Plant) -> Result<(), SendError<T>> {
        self.shared.send(value, None, plant).map_err(|it| SendError(it.into_inner()))
    }

    /// Sends `value` if there is room now (for a rendezvous: a receiver waits now).
    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        let shared = &*self.shared;
        if shared.disconnected_from_receivers() {
            return Err(TrySendError::Disconnected(value));
        }
        match &shared.flavor {
            Flavor::List(_) => {
                shared.send(value, None, Plant::None).map_err(|it| TrySendError::Disconnected(it.into_inner()))
            }
            Flavor::Array(queue) => match queue.push(value) {
                Ok(()) => {
                    shared.sent.notify(Plant::None);
                    Ok(())
                }
                Err(back) => Err(TrySendError::Full(back)),
            },
            Flavor::Zero(zero) => {
                let mut hand = zero.hand.lock();
                if hand.receiving == 0 || hand.value.is_some() {
                    return Err(TrySendError::Full(value));
                }
                hand.value = Some(value);
                hand.put += 1;
                zero.cond.notify(true);
                Ok(())
            }
        }
    }

    /// [`Sender::send`], waiting at most `most` for room.
    pub fn send_timeout(&self, value: T, most: Duration) -> Result<(), SendTimeoutError<T>> {
        self.send_deadline(value, Instant::now() + most)
    }

    /// [`Sender::send`], waiting for room until `deadline`.
    pub fn send_deadline(&self, value: T, deadline: Instant) -> Result<(), SendTimeoutError<T>> {
        self.shared.send(value, Some(deadline), Plant::None)
    }

    pub fn len(&self) -> usize {
        self.shared.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_full(&self) -> bool {
        self.shared.is_full()
    }

    /// Its bound: `None` for an unbounded channel.
    pub fn capacity(&self) -> Option<usize> {
        self.shared.capacity()
    }
}

impl<T> Shared<T> {
    fn len(&self) -> usize {
        match &self.flavor {
            Flavor::Array(queue) => queue.len(),
            Flavor::List(queue) => queue.len(),
            Flavor::Zero(_) => 0,
        }
    }

    fn is_full(&self) -> bool {
        match &self.flavor {
            Flavor::Array(queue) => queue.is_full(),
            Flavor::List(_) => false,
            Flavor::Zero(_) => true,
        }
    }

    fn capacity(&self) -> Option<usize> {
        match &self.flavor {
            Flavor::Array(queue) => Some(queue.capacity()),
            Flavor::List(_) => None,
            Flavor::Zero(_) => Some(0),
        }
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Sender<T> {
        self.shared.senders.fetch_add(1, Ordering::SeqCst);
        Sender { shared: Arc::clone(&self.shared) }
    }
}

impl<T> blaze_own::Share for Sender<T> {}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.drop_with(Plant::None);
    }
}

impl<T> Sender<T> {
    fn drop_with(&self, plant: Plant) {
        if self.shared.senders.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.shared.disconnect(&self.shared.sent, plant);
        }
    }

    /// Drops this sender, with a safeguard of its disconnect removed for a model test.
    #[cfg(test)]
    pub(crate) fn drop_planted(self, plant: Plant) {
        let this = std::mem::ManuallyDrop::new(self);
        this.drop_with(plant);
        // SAFETY: read once out of the `ManuallyDrop`, which never drops it.
        #[expect(unsafe_code, reason = "the sender's Arc, dropped once")]
        drop(unsafe { std::ptr::read(&this.shared) });
    }
}

impl<T> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("Sender { .. }")
    }
}

// --- Receiver -----------------------------------------------------------------------------------

/// The receiving end; a clone is another receiver of the same channel (each value goes to one).
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Receiver<T> {
    /// The next value, waiting for one; refused once the channel is empty and every sender is
    /// gone.
    pub fn recv(&self) -> Result<T, RecvError> {
        self.shared.recv(Shared::<T>::name(), None).map_err(|_| RecvError)
    }

    /// The next value, if one is there now.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        let shared = &*self.shared;
        match &shared.flavor {
            Flavor::Zero(zero) => {
                let mut hand = zero.hand.lock();
                match hand.value.take() {
                    Some(value) => {
                        hand.taken += 1;
                        zero.cond.notify(true);
                        Ok(value)
                    }
                    None if shared.disconnected_from_senders() => Err(TryRecvError::Disconnected),
                    None => Err(TryRecvError::Empty),
                }
            }
            Flavor::Array(_) | Flavor::List(_) => match shared.pop() {
                Some(value) => Ok(value),
                None if shared.disconnected_from_senders() => {
                    shared.pop().ok_or(TryRecvError::Disconnected)
                }
                None => Err(TryRecvError::Empty),
            },
        }
    }

    /// [`Receiver::recv`], waiting at most `most`.
    pub fn recv_timeout(&self, most: Duration) -> Result<T, RecvTimeoutError> {
        self.recv_deadline(Instant::now() + most)
    }

    /// [`Receiver::recv`], waiting until `deadline`.
    pub fn recv_deadline(&self, deadline: Instant) -> Result<T, RecvTimeoutError> {
        self.shared.recv(Shared::<T>::name(), Some(deadline))
    }

    /// [`Receiver::recv_timeout`], its wait's span named `what` (what the caller waits for)
    /// instead of the value's type (rule every-wait-says-what).
    pub fn recv_timeout_named(&self, what: &'static str, within: Duration) -> Result<T, RecvTimeoutError> {
        self.shared.recv(what, Some(Instant::now() + within))
    }

    /// Each value as it arrives, until the channel is disconnected.
    pub fn iter(&self) -> Iter<'_, T> {
        Iter { receiver: self }
    }

    /// Each value there now, without waiting.
    pub fn try_iter(&self) -> TryIter<'_, T> {
        TryIter { receiver: self }
    }

    pub fn len(&self) -> usize {
        self.shared.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_full(&self) -> bool {
        self.shared.is_full()
    }

    /// Its bound: `None` for an unbounded channel.
    pub fn capacity(&self) -> Option<usize> {
        self.shared.capacity()
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Receiver<T> {
        self.shared.receivers.fetch_add(1, Ordering::SeqCst);
        Receiver { shared: Arc::clone(&self.shared) }
    }
}

impl<T> blaze_own::Share for Receiver<T> {}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        if self.shared.receivers.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.shared.disconnect(&self.shared.received, Plant::None);
        }
    }
}

impl<T> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("Receiver { .. }")
    }
}

/// [`Receiver::iter`].
pub struct Iter<'a, T> {
    receiver: &'a Receiver<T>,
}

impl<T> Iterator for Iter<'_, T> {
    type Item = T;
    fn next(&mut self) -> Option<T> {
        self.receiver.recv().ok()
    }
}

/// [`Receiver::try_iter`].
pub struct TryIter<'a, T> {
    receiver: &'a Receiver<T>,
}

impl<T> Iterator for TryIter<'_, T> {
    type Item = T;
    fn next(&mut self) -> Option<T> {
        self.receiver.try_recv().ok()
    }
}

/// A receiver as an iterator of each value until the channel is disconnected.
pub struct IntoIter<T> {
    receiver: Receiver<T>,
}

impl<T> Iterator for IntoIter<T> {
    type Item = T;
    fn next(&mut self) -> Option<T> {
        self.receiver.recv().ok()
    }
}

impl<T> IntoIterator for Receiver<T> {
    type Item = T;
    type IntoIter = IntoIter<T>;
    fn into_iter(self) -> IntoIter<T> {
        IntoIter { receiver: self }
    }
}

impl<'a, T> IntoIterator for &'a Receiver<T> {
    type Item = T;
    type IntoIter = Iter<'a, T>;
    fn into_iter(self) -> Iter<'a, T> {
        self.iter()
    }
}

// --- Errors -------------------------------------------------------------------------------------

/// A send to a channel every receiver of which is gone: the value back.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SendError<T>(pub T);

impl<T> SendError<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

/// A [`Sender::try_send`] that did not send: the value back.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TrySendError<T> {
    Full(T),
    Disconnected(T),
}

impl<T> TrySendError<T> {
    pub fn into_inner(self) -> T {
        match self {
            TrySendError::Full(it) | TrySendError::Disconnected(it) => it,
        }
    }

    pub fn is_full(&self) -> bool {
        matches!(self, TrySendError::Full(_))
    }

    pub fn is_disconnected(&self) -> bool {
        matches!(self, TrySendError::Disconnected(_))
    }
}

/// A timed send that did not send: the value back.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SendTimeoutError<T> {
    Timeout(T),
    Disconnected(T),
}

impl<T> SendTimeoutError<T> {
    pub fn into_inner(self) -> T {
        match self {
            SendTimeoutError::Timeout(it) | SendTimeoutError::Disconnected(it) => it,
        }
    }

    pub fn is_timeout(&self) -> bool {
        matches!(self, SendTimeoutError::Timeout(_))
    }

    pub fn is_disconnected(&self) -> bool {
        matches!(self, SendTimeoutError::Disconnected(_))
    }
}

/// A receive on an empty channel every sender of which is gone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RecvError;

/// A [`Receiver::try_recv`] that found no value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TryRecvError {
    Empty,
    Disconnected,
}

impl TryRecvError {
    pub fn is_empty(&self) -> bool {
        matches!(self, TryRecvError::Empty)
    }

    pub fn is_disconnected(&self) -> bool {
        matches!(self, TryRecvError::Disconnected)
    }
}

/// A timed receive that found no value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecvTimeoutError {
    Timeout,
    Disconnected,
}

impl RecvTimeoutError {
    pub fn is_timeout(&self) -> bool {
        matches!(self, RecvTimeoutError::Timeout)
    }

    pub fn is_disconnected(&self) -> bool {
        matches!(self, RecvTimeoutError::Disconnected)
    }
}

impl<T> fmt::Debug for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("SendError(..)")
    }
}

impl<T> fmt::Debug for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrySendError::Full(_) => f.pad("Full(..)"),
            TrySendError::Disconnected(_) => f.pad("Disconnected(..)"),
        }
    }
}

impl<T> fmt::Debug for SendTimeoutError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendTimeoutError::Timeout(_) => f.pad("Timeout(..)"),
            SendTimeoutError::Disconnected(_) => f.pad("Disconnected(..)"),
        }
    }
}

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("sending on a channel every receiver of which is gone")
    }
}

impl<T> fmt::Display for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrySendError::Full(_) => f.pad("sending on a full channel"),
            TrySendError::Disconnected(_) => {
                f.pad("sending on a channel every receiver of which is gone")
            }
        }
    }
}

impl<T> fmt::Display for SendTimeoutError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendTimeoutError::Timeout(_) => f.pad("a send timed out waiting for room"),
            SendTimeoutError::Disconnected(_) => {
                f.pad("sending on a channel every receiver of which is gone")
            }
        }
    }
}

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("receiving on an empty channel every sender of which is gone")
    }
}

impl fmt::Display for TryRecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TryRecvError::Empty => f.pad("receiving on an empty channel"),
            TryRecvError::Disconnected => {
                f.pad("receiving on an empty channel every sender of which is gone")
            }
        }
    }
}

impl fmt::Display for RecvTimeoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecvTimeoutError::Timeout => f.pad("a receive timed out waiting for a value"),
            RecvTimeoutError::Disconnected => {
                f.pad("receiving on an empty channel every sender of which is gone")
            }
        }
    }
}

impl<T> std::error::Error for SendError<T> {}
impl<T> std::error::Error for TrySendError<T> {}
impl<T> std::error::Error for SendTimeoutError<T> {}
impl std::error::Error for RecvError {}
impl std::error::Error for TryRecvError {}
impl std::error::Error for RecvTimeoutError {}

impl<T> From<SendError<T>> for TrySendError<T> {
    fn from(error: SendError<T>) -> TrySendError<T> {
        TrySendError::Disconnected(error.0)
    }
}

impl<T> From<SendError<T>> for SendTimeoutError<T> {
    fn from(error: SendError<T>) -> SendTimeoutError<T> {
        SendTimeoutError::Disconnected(error.0)
    }
}

impl From<RecvError> for TryRecvError {
    fn from(_: RecvError) -> TryRecvError {
        TryRecvError::Disconnected
    }
}

impl From<RecvError> for RecvTimeoutError {
    fn from(_: RecvError) -> RecvTimeoutError {
        RecvTimeoutError::Disconnected
    }
}
