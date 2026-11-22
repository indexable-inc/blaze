//! Channels, bounded only (std.md, "Concurrency: what to reach for"). Each has a capacity, its
//! ring of that many slots is allocated when it is made, and no unbounded form exists: a
//! producer faster than its consumer waits, by its deadline, instead of growing memory without
//! a bound (rule bounded-progress). Two kinds, by what a message may be:
//!
//! - **[`bounded`]: flat messages, the default.** A [`Chan`] of `T: Flat` ([`crate::flat`]):
//!   a send copies the value's bytes into a slot of the ring and a receive copies them out;
//!   nothing is allocated per message and nothing is counted. A flat message holds no address,
//!   so sender and receiver share no memory through it and no mutable state by accident, and
//!   the same message can cross to another process or a remote worker unchanged.
//! - **[`owned`]: owned values, moved.** An [`OwnedChan`] of any `T`: a `Box`, a `Vec` filled
//!   in the sender's allocator, a [`crate::sync::Shared`] handle, a narrowed capability. The
//!   receiver takes over what the value owns, in this process only. It is the right one when
//!   the receiver must own memory or authority the sender made, and handing it over beats
//!   copying it: a large buffer, a capability given to a worker task. A message that is a
//!   handle to shared data is shared state between the two tasks; prefer sending a flat key or
//!   index and borrowing the data from the scope, which costs no count.
//!
//! ```text
//! let jobs = chan::bounded::<Job, _>("bstd.jobs", NonZeroUsize::new(64).unwrap(), cx.frame());
//! scope.must_overlap("produce", TASK_FRAME, |cx| { for job in work { jobs.send(cx, job)?; } jobs.close(); Ok(()) })?;
//! loop { match jobs.recv(cx) { Ok(job) => run(job), Err(RecvError::Closed { .. }) => break, Err(e) => return Err(e) } }
//! ```
//!
//! - [`Chan::send`] and [`Chan::recv`] take `cx`: each waits until it can go on or the deadline
//!   or cancellation stops it, the outcome typed ([`SendError`], [`RecvError`]); a send that
//!   stopped gives its value back. [`Chan::try_send`] and [`Chan::try_recv`] never wait.
//! - [`Chan::close`] never waits: later sends get their value back as [`Closed`], and receivers
//!   drain what is left before they see it.
//! - Values come out in the order they went in, each once.
//! - The logic is a sans-IO ring (`Queue`) under one named lock and two signals; its waits are
//!   [`crate::sync`]'s one condition wait (virtual time on the simulation, the wall clock
//!   between threads). Both kinds are this one type, told apart by its [`Messages`] parameter
//!   (rule strategy-is-a-type-parameter): only the constructors differ.
//! - Cost: one allocation of `capacity` slots when made; a send or receive is a lock, a copy of
//!   the value into or out of its slot and, when the other side may wait, a notify.

use core::{alloc::Allocator, fmt, marker::PhantomData, num::NonZeroUsize};

use blaze_own::Vec;

use crate::{
    Plant, Stopped,
    cx::Cx,
    flat::Flat,
    sync::{Lock, Signal, wait_ready},
};

/// What a channel's messages may be: [`Copies`] (flat values, from [`bounded`]) or [`Moves`]
/// (owned values, from [`owned`]).
pub trait Messages {
    /// What its errors and its `Debug` call the kind.
    const KIND: &'static str;
}

/// Flat messages: their bytes are copied in and out ([`bounded`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Copies;

/// Owned values, moved to the receiver in this process ([`owned`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Moves;

impl Messages for Copies {
    const KIND: &'static str = "flat";
}

impl Messages for Moves {
    const KIND: &'static str = "owned";
}

/// A bounded channel. See the module's doc. Made by [`bounded`] (flat messages, the default
/// `M`) or [`owned`] ([`OwnedChan`]).
pub struct Chan<T, A: Allocator, M: Messages = Copies> {
    queue: Lock<Queue<T, A>>,
    not_empty: Signal,
    not_full: Signal,
    messages: PhantomData<M>,
}

/// A bounded channel of owned values, from [`owned`]: what it is for is in the module's doc.
pub type OwnedChan<T, A> = Chan<T, A, Moves>;

/// A channel named `name` of flat messages, holding at most `capacity`, its ring allocated now
/// in `alloc`. There is no constructor without a capacity.
///
/// Built by rustc, `Flat` holds of every type, so the walk is asserted here for `T` as this is
/// compiled: a message type with an address inside does not compile ([`crate::flat`]'s
/// `compile_fail` example). Built by blaze, the bound itself is the walk.
pub fn bounded<T: Flat, A: Allocator>(name: &'static str, capacity: NonZeroUsize, alloc: A) -> Chan<T, A> {
    #[cfg(not(blaze))]
    let () = const { assert!(crate::flat::is_flat::<T>()) };
    Chan::made(name, capacity, alloc, Plant::None, Slip::None)
}

/// A channel named `name` of owned values, holding at most `capacity`, its ring allocated now
/// in `alloc`. Prefer [`bounded`] when the message can be flat (the module's doc says when it
/// cannot).
pub fn owned<T, A: Allocator>(name: &'static str, capacity: NonZeroUsize, alloc: A) -> OwnedChan<T, A> {
    Chan::made(name, capacity, alloc, Plant::None, Slip::None)
}

/// The channel was closed: the value a send gave back.
#[derive(PartialEq, Eq, Debug)]
pub struct Closed<T> {
    pub chan: &'static str,
    pub value: T,
}

/// Why a send did not deliver its value, which it gives back.
#[derive(PartialEq, Eq, Debug)]
pub enum SendError<T> {
    /// The channel is closed.
    Closed(Closed<T>),
    /// It was full ([`Chan::try_send`] only).
    Full { chan: &'static str, value: T },
    /// The wait for room stopped so.
    Stopped { value: T, stopped: Stopped },
}

/// Why a receive gave no value.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum RecvError {
    /// The channel is closed and empty.
    Closed { chan: &'static str },
    /// It was empty ([`Chan::try_recv`] only).
    Empty { chan: &'static str },
    /// The wait for a value stopped so.
    Stopped(Stopped),
}

/// A planted bug in the ring's index arithmetic, for its test's control (skill `testing`):
/// always [`Slip::None`] outside this crate's tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(test), expect(dead_code, reason = "the planted slip is made by the tests only"))]
pub(crate) enum Slip {
    None,
    /// The back of the ring is found from the count alone, as if the front never moved.
    BackForgetsFront,
}

/// The channel's state: a sans-IO ring, its every step a function of it and its argument.
///
/// Invariants: `slots.len()` is the capacity and never changes; the `len` slots from `front`,
/// wrapping, hold values and every other slot holds none.
pub(crate) struct Queue<T, A: Allocator> {
    slots: Vec<Option<T>, A>,
    front: usize,
    len: usize,
    closed: bool,
    plant: Plant,
    slip: Slip,
}

/// Why [`Queue::offer`] refused a value.
pub(crate) enum Offer<T> {
    Full(T),
    Closed(T),
}

/// Why [`Queue::take`] had none.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Take {
    Empty,
    Closed,
}

impl<T, A: Allocator> Queue<T, A> {
    #[cfg(test)]
    pub(crate) fn new_in(capacity: NonZeroUsize, alloc: A, plant: Plant) -> Queue<T, A> {
        Queue::slipped(capacity, alloc, plant, Slip::None)
    }

    pub(crate) fn slipped(capacity: NonZeroUsize, alloc: A, plant: Plant, slip: Slip) -> Queue<T, A> {
        let mut slots = Vec::with_capacity_in(capacity.get(), alloc);
        slots.resize_with(capacity.get(), || None);
        Queue { slots, front: 0, len: 0, closed: false, plant, slip }
    }

    /// The slot `offset` places after the front, wrapping: `offset` is at most the capacity,
    /// so one subtraction wraps it.
    fn at(&self, offset: usize) -> usize {
        let capacity = self.slots.len();
        let at = self.front + offset;
        match at >= capacity {
            true => at - capacity,
            false => at,
        }
    }

    /// `value` in at the back, if there is room and the queue is open.
    pub(crate) fn offer(&mut self, value: T) -> Result<(), Offer<T>> {
        if self.closed {
            return Err(Offer::Closed(value));
        }
        let capacity = self.slots.len();
        let room = match self.plant {
            Plant::ChanPassesCapacity => self.len <= capacity,
            Plant::None | Plant::SubdirKeepsParent | Plant::OpenCreates | Plant::LexicalParent | Plant::WaitIgnoresDeadline | Plant::UntilWidens | Plant::WaitIgnoresCancel | Plant::ArenaUnlimited | Plant::SyncSkipped | Plant::EnvOnlyReplaces | Plant::CancelSkipsWait | Plant::CloseSkipsSync | Plant::JoinDropsFailure | Plant::OffersIgnored | Plant::OnlyWidens | Plant::KvPutUnsynced | Plant::LocalDropsChunk | Plant::HttpOnlyIgnored | Plant::BudgetCheckedAfter | Plant::KeyIgnoresLast | Plant::MemoNeverEvicts | Plant::MemoSharesPoints | Plant::NestedUnderGrandparent | Plant::PublishBeforeRename | Plant::DurableSkipsNameSync | Plant::UnsyncedRenameSyncs | Plant::FlushedBeforeFlush | Plant::StaleFlushedCloned | Plant::ParJoinsInRunOrder | Plant::FreezeLeaks | Plant::SwapFreesAtOnce | Plant::FlightComputesTwice | Plant::PublishOnceOverwrites | Plant::OrderTokenTwice | Plant::TaskMigrates | Plant::AncestorsUnbounded => self.len < capacity,
        };
        if !room {
            return Err(Offer::Full(value));
        }
        let back = match self.slip {
            Slip::None => self.at(self.len),
            Slip::BackForgetsFront => self.len % capacity,
        };
        self.slots[back] = Some(value);
        self.len += 1;
        Ok(())
    }

    /// The value at the front: once the queue is closed, what is left, then `Closed`.
    pub(crate) fn take(&mut self) -> Result<T, Take> {
        if self.len == 0 {
            return Err(match self.closed {
                true => Take::Closed,
                false => Take::Empty,
            });
        }
        let front = self.front;
        let value = self.slots[front].take();
        self.front = self.at(1);
        self.len -= 1;
        match value {
            Some(value) => Ok(value),
            None => unreachable!("bstd::sync::chan: the ring's front slot {front} was empty with values held (its invariant)"),
        }
    }

    pub(crate) fn close(&mut self) {
        self.closed = true;
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }
}

impl<T, A: Allocator, M: Messages> Chan<T, A, M> {
    fn made(name: &'static str, capacity: NonZeroUsize, alloc: A, plant: Plant, slip: Slip) -> Chan<T, A, M> {
        Chan {
            queue: Lock::new(name, Queue::slipped(capacity, alloc, plant, slip)),
            not_empty: Signal::new(name),
            not_full: Signal::new(name),
            messages: PhantomData,
        }
    }

    /// A channel with a planted bug, for this crate's tests.
    #[cfg(test)]
    pub(crate) fn planted(name: &'static str, capacity: NonZeroUsize, alloc: A, plant: Plant) -> Chan<T, A, M> {
        Chan::made(name, capacity, alloc, plant, Slip::None)
    }

    /// A channel whose ring has a planted slip, for this crate's tests.
    #[cfg(test)]
    pub(crate) fn slipped(name: &'static str, capacity: NonZeroUsize, alloc: A, slip: Slip) -> Chan<T, A, M> {
        Chan::made(name, capacity, alloc, Plant::None, slip)
    }

    /// The channel's name: what its errors and its waits' spans say.
    pub fn name(&self) -> &'static str {
        self.queue.name()
    }

    /// Sends `value`, waiting for room until `cx`'s deadline or cancellation; a send that does
    /// not deliver gives `value` back.
    pub fn send(&self, cx: &Cx, value: T) -> Result<(), SendError<T>> {
        let chan = self.name();
        let mut held = Some(value);
        let sent = wait_ready(cx, &self.queue, &self.not_full, |queue| {
            let value = held.take()?;
            match queue.offer(value) {
                Ok(()) => Some(Ok(())),
                Err(Offer::Closed(value)) => Some(Err(value)),
                Err(Offer::Full(value)) => {
                    held = Some(value);
                    None
                }
            }
        });
        match (sent, held) {
            (Ok(Ok(())), _) => {
                self.not_empty.notify_one();
                Ok(())
            }
            (Ok(Err(value)), _) => Err(SendError::Closed(Closed { chan, value })),
            (Err(stopped), Some(value)) => Err(SendError::Stopped { value, stopped }),
            (Err(stopped), None) => unreachable!("bstd::sync::chan {chan}: a stopped send kept its value ({stopped})"),
        }
    }

    /// Sends `value` if there is room now; never waits.
    pub fn try_send(&self, value: T) -> Result<(), SendError<T>> {
        let chan = self.name();
        let Some(mut queue) = self.queue.try_lock() else {
            return Err(SendError::Full { chan, value });
        };
        let offered = queue.offer(value);
        drop(queue);
        match offered {
            Ok(()) => {
                self.not_empty.notify_one();
                Ok(())
            }
            Err(Offer::Full(value)) => Err(SendError::Full { chan, value }),
            Err(Offer::Closed(value)) => Err(SendError::Closed(Closed { chan, value })),
        }
    }

    /// The next value, waiting for one until `cx`'s deadline or cancellation; once the channel
    /// is closed, what is left, then [`RecvError::Closed`].
    pub fn recv(&self, cx: &Cx) -> Result<T, RecvError> {
        let chan = self.name();
        let got = wait_ready(cx, &self.queue, &self.not_empty, |queue| match queue.take() {
            Ok(value) => Some(Ok(value)),
            Err(Take::Closed) => Some(Err(RecvError::Closed { chan })),
            Err(Take::Empty) => None,
        });
        match got {
            Ok(Ok(value)) => {
                self.not_full.notify_one();
                Ok(value)
            }
            Ok(Err(closed)) => Err(closed),
            Err(stopped) => Err(RecvError::Stopped(stopped)),
        }
    }

    /// The next value if there is one now; never waits.
    pub fn try_recv(&self) -> Result<T, RecvError> {
        let chan = self.name();
        let Some(mut queue) = self.queue.try_lock() else {
            return Err(RecvError::Empty { chan });
        };
        let taken = queue.take();
        drop(queue);
        match taken {
            Ok(value) => {
                self.not_full.notify_one();
                Ok(value)
            }
            Err(Take::Empty) => Err(RecvError::Empty { chan }),
            Err(Take::Closed) => Err(RecvError::Closed { chan }),
        }
    }

    /// Closes the channel; never waits beyond the queue's own lock, which is held only for a
    /// move. Every sender and receiver waiting is woken to see it.
    pub fn close(&self) {
        self.queue.lock_briefly().close();
        self.not_empty.notify_all();
        self.not_full.notify_all();
    }

    /// How many values wait in it now.
    pub fn len(&self) -> usize {
        self.queue.lock_briefly().len()
    }

    /// Whether none waits in it now.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendError::Closed(Closed { chan, .. }) => write!(f, "bstd::sync::chan {chan}: closed, the value given back"),
            SendError::Full { chan, .. } => write!(f, "bstd::sync::chan {chan}: full, the value given back"),
            SendError::Stopped { stopped, .. } => write!(f, "bstd::sync::chan: a send stopped waiting for room: {stopped}"),
        }
    }
}

impl<T: fmt::Debug> core::error::Error for SendError<T> {}

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecvError::Closed { chan } => write!(f, "bstd::sync::chan {chan}: closed and empty"),
            RecvError::Empty { chan } => write!(f, "bstd::sync::chan {chan}: empty"),
            RecvError::Stopped(stopped) => write!(f, "bstd::sync::chan: a receive stopped waiting: {stopped}"),
        }
    }
}

impl core::error::Error for RecvError {}

impl<T, A: Allocator, M: Messages> fmt::Debug for Chan<T, A, M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Chan").field("name", &self.name()).field("messages", &M::KIND).finish()
    }
}
