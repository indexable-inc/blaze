//! [`Chan`]: a bounded channel, the only kind (std.md, "Concurrency: what to reach for"). Its
//! capacity is an argument, its room is allocated when it is made, and no unbounded form
//! exists: a producer faster than its consumer waits, by its deadline, instead of growing
//! memory without a bound (rule bounded-progress).
//!
//! ```text
//! let jobs = Chan::with_capacity_in("bstd.jobs", NonZeroUsize::new(64).unwrap(), cx.frame());
//! scope.spawn("produce", TASK_FRAME, |cx| { for job in work { jobs.send(cx, job)?; } jobs.close(); Ok(()) });
//! scope.spawn("consume", TASK_FRAME, |cx| loop { match jobs.recv(cx) { Ok(job) => run(job), Err(RecvError::Closed { .. }) => return Ok(()), Err(e) => return Err(e) } });
//! ```
//!
//! - [`Chan::send`] and [`Chan::recv`] take `cx`: each waits until it can go on or the deadline
//!   or cancellation stops it, the outcome typed ([`SendError`], [`RecvError`]); a send that
//!   stopped gives its value back. [`Chan::try_send`] and [`Chan::try_recv`] never wait.
//! - [`Chan::close`] never waits: later sends get their value back as [`Closed`], and receivers
//!   drain what is left before they see it.
//! - Values come out in the order they went in, each once.
//! - The logic is a sans-IO queue (`Queue`) under one named lock and two signals; its waits are
//!   [`crate::sync`]'s one condition wait (virtual time on the simulation, the wall clock
//!   between threads).
//! - Cost: one allocation of `capacity` values when made; a send or receive is a lock, a move
//!   and, when the other side may wait, a notify.

use core::{alloc::Allocator, fmt, num::NonZeroUsize};

use blaze_own::VecDeque;

use crate::{
    Plant, Stopped,
    cx::Cx,
    sync::{Lock, Signal, wait_ready},
};

/// A bounded channel. See the module's doc.
pub struct Chan<T, A: Allocator> {
    queue: Lock<Queue<T, A>>,
    not_empty: Signal,
    not_full: Signal,
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

/// The channel's state: a sans-IO core, its every step a function of it and its argument.
pub(crate) struct Queue<T, A: Allocator> {
    items: VecDeque<T, A>,
    capacity: NonZeroUsize,
    closed: bool,
    plant: Plant,
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
    pub(crate) fn new_in(capacity: NonZeroUsize, alloc: A, plant: Plant) -> Queue<T, A> {
        let items = VecDeque::with_capacity_in(capacity.get(), alloc);
        Queue { items, capacity, closed: false, plant }
    }

    /// `value` in at the back, if there is room and the queue is open.
    pub(crate) fn offer(&mut self, value: T) -> Result<(), Offer<T>> {
        if self.closed {
            return Err(Offer::Closed(value));
        }
        let room = match self.plant {
            Plant::ChanPassesCapacity => self.items.len() <= self.capacity.get(),
            Plant::None | Plant::SubdirKeepsParent | Plant::OpenCreates | Plant::LexicalParent | Plant::WaitIgnoresDeadline | Plant::UntilWidens | Plant::WaitIgnoresCancel | Plant::ArenaUnlimited | Plant::SyncSkipped | Plant::EnvOnlyReplaces | Plant::CancelSkipsWait | Plant::CloseSkipsSync | Plant::JoinDropsFailure | Plant::OffersIgnored | Plant::OnlyWidens | Plant::KvPutUnsynced | Plant::LocalDropsChunk | Plant::HttpOnlyIgnored | Plant::BudgetCheckedAfter | Plant::KeyIgnoresLast | Plant::MemoNeverEvicts | Plant::MemoSharesPoints | Plant::NestedUnderGrandparent | Plant::PublishBeforeRename | Plant::DurableSkipsNameSync | Plant::FlushedBeforeFlush | Plant::StaleFlushedCloned | Plant::ParJoinsInRunOrder | Plant::FreezeLeaks | Plant::SwapFreesAtOnce | Plant::FlightComputesTwice | Plant::PublishOnceOverwrites | Plant::OrderTokenTwice | Plant::TaskMigrates => self.items.len() < self.capacity.get(),
        };
        match room {
            true => {
                self.items.push_back(value);
                Ok(())
            }
            false => Err(Offer::Full(value)),
        }
    }

    /// The value at the front: once the queue is closed, what is left, then `Closed`.
    pub(crate) fn take(&mut self) -> Result<T, Take> {
        match (self.items.pop_front(), self.closed) {
            (Some(value), _) => Ok(value),
            (None, false) => Err(Take::Empty),
            (None, true) => Err(Take::Closed),
        }
    }

    pub(crate) fn close(&mut self) {
        self.closed = true;
    }

    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }
}

impl<T, A: Allocator> Chan<T, A> {
    /// A channel named `name` holding at most `capacity` values, its room allocated now in
    /// `alloc`. There is no constructor without a capacity.
    pub fn with_capacity_in(name: &'static str, capacity: NonZeroUsize, alloc: A) -> Chan<T, A> {
        Chan::planted(name, capacity, alloc, Plant::None)
    }

    pub(crate) fn planted(name: &'static str, capacity: NonZeroUsize, alloc: A, plant: Plant) -> Chan<T, A> {
        Chan {
            queue: Lock::new(name, Queue::new_in(capacity, alloc, plant)),
            not_empty: Signal::new(name),
            not_full: Signal::new(name),
        }
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

impl<T, A: Allocator> fmt::Debug for Chan<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Chan").field("name", &self.name()).finish()
    }
}
