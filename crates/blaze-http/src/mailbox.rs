//! The handles' side of the drivers ([`crate::driver`], [`crate::h3::driver`]): what an asker
//! on another thread hands a driver's loop and waits on, with no runtime under it.
//!
//! - [`channel`]: a queue of items from any number of senders to one reader (which may be
//!   shared by reference), bounded or not. Its [`Rx::recv`] is a future any executor polls,
//!   and [`block_on`] polls one on the calling thread, parked on blaze-core's deadline wait.
//! - [`latch`]: a value set once and read by any number of waiters (how a session ended, a
//!   stream's stop).
//!
//! Every item is handed over under one named lock ([`blaze_core::lock::Lock`]), and every
//! waiter is woken through the `std::task::Waker` it left: the loop wakes askers this way,
//! and askers wake the loop through blaze-core's [`blaze_core::Waker`] (the drivers' inbox).

use std::{
    alloc::Global,
    collections::VecDeque,
    future::{Future, poll_fn},
    pin::pin,
    sync::Arc,
    task::{Context, Poll, Wake, Waker},
    time::Duration,
};

use blaze_core::{
    lock::Lock,
    wait::{self, Deadline, Monotonic, Parker, WaitError, Waited},
};

/// The waiters one channel or latch keeps: each reader polling it (a session's clones), at
/// most this many before the oldest is woken and let go, which polls again and leaves its
/// waker anew. A bound, so a reader that polls and goes away leaves nothing that grows.
const WAITERS_MOST: usize = 16;

struct State<T> {
    items: VecDeque<T, Global>,
    /// Senders alive: at none, the channel is over once its items are read.
    senders: usize,
    /// Whether the reader is alive: once not, a send hands its item back.
    reader: bool,
    /// At most this many items held: a send past it hands its item back. `usize::MAX`: none.
    most: usize,
    waiters: Vec<Waker, Global>,
}

fn wake_all(waiters: &mut Vec<Waker, Global>) {
    for waiter in waiters.drain(..) {
        waiter.wake();
    }
}

fn wait_on(waiters: &mut Vec<Waker, Global>, cx: &Context<'_>) {
    if waiters.iter().any(|it| it.will_wake(cx.waker())) {
        return;
    }
    if waiters.len() >= WAITERS_MOST {
        waiters.remove(0).wake();
    }
    waiters.push(cx.waker().clone()); // clone: the waiter left, woken by the next send
}

struct Shared<T> {
    state: Lock<State<T>>,
}

/// A channel's sending side; cloned, each clone a sender.
pub struct Tx<T> {
    shared: Arc<Shared<T>, Global>,
}

/// A channel's one reader.
pub struct Rx<T> {
    shared: Arc<Shared<T>, Global>,
}

/// A channel holding at most `most` items (`None`: any number).
pub fn channel<T>(most: Option<usize>) -> (Tx<T>, Rx<T>) {
    let state = State {
        items: VecDeque::new_in(Global),
        senders: 1,
        reader: true,
        most: most.unwrap_or(usize::MAX),
        waiters: Vec::new_in(Global),
    };
    let shared = Arc::new_in(Shared { state: Lock::new("http.mailbox.channel", state) }, Global);
    (Tx { shared: Arc::clone(&shared) }, Rx { shared })
}

impl<T> Tx<T> {
    /// Hands `item` to the reader; back when the reader is gone or the channel is full.
    pub fn send(&self, item: T) -> Result<(), T> {
        let mut state = self.shared.state.lock();
        if !state.reader || state.items.len() >= state.most {
            return Err(item);
        }
        state.items.push_back(item);
        wake_all(&mut state.waiters);
        Ok(())
    }

    /// Whether the reader is gone.
    pub fn is_closed(&self) -> bool {
        !self.shared.state.lock().reader
    }
}

impl<T> Clone for Tx<T> {
    fn clone(&self) -> Tx<T> {
        self.shared.state.lock().senders += 1;
        Tx { shared: Arc::clone(&self.shared) }
    }
}

impl<T> Drop for Tx<T> {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock();
        state.senders -= 1;
        if state.senders == 0 {
            wake_all(&mut state.waiters);
        }
    }
}

impl<T> Rx<T> {
    /// The next item, `None` once every sender is gone and every item read.
    pub fn recv(&self) -> impl Future<Output = Option<T>> + '_ {
        poll_fn(|cx| {
            let mut state = self.shared.state.lock();
            if let Some(item) = state.items.pop_front() {
                return Poll::Ready(Some(item));
            }
            if state.senders == 0 {
                return Poll::Ready(None);
            }
            wait_on(&mut state.waiters, cx);
            Poll::Pending
        })
    }

    /// The next item if one is here: `Ok(None)` once every sender is gone, `Err(())` when
    /// none is here yet.
    pub fn try_recv(&self) -> Result<Option<T>, ()> {
        let mut state = self.shared.state.lock();
        match state.items.pop_front() {
            Some(item) => Ok(Some(item)),
            None if state.senders == 0 => Ok(None),
            None => Err(()),
        }
    }
}

impl<T> Drop for Rx<T> {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock();
        state.reader = false;
        // What was sent and not read goes with the reader.
        state.items.clear();
    }
}

struct LatchState<T> {
    value: Option<T>,
    /// The setter dropped without setting it.
    abandoned: bool,
    waiters: Vec<Waker, Global>,
}

struct LatchShared<T> {
    state: Lock<LatchState<T>>,
}

/// A latch's one setter: dropped unset, its waiters read `None`.
pub struct Setter<T> {
    shared: Arc<LatchShared<T>, Global>,
}

/// A latch's waiters' side, cloned to each.
pub struct Latch<T> {
    shared: Arc<LatchShared<T>, Global>,
}

/// A value set once, read by every waiter.
pub fn latch<T>() -> (Setter<T>, Latch<T>) {
    let state = LatchState { value: None, abandoned: false, waiters: Vec::new_in(Global) };
    let shared = Arc::new_in(LatchShared { state: Lock::new("http.mailbox.latch", state) }, Global);
    (Setter { shared: Arc::clone(&shared) }, Latch { shared })
}

impl<T> Setter<T> {
    /// Sets the value, if it is not set yet, and wakes every waiter.
    pub fn set(&self, value: T) {
        let mut state = self.shared.state.lock();
        if state.value.is_none() {
            state.value = Some(value);
        }
        wake_all(&mut state.waiters);
    }
}

impl<T> Drop for Setter<T> {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock();
        state.abandoned = true;
        wake_all(&mut state.waiters);
    }
}

impl<T> Clone for Latch<T> {
    fn clone(&self) -> Latch<T> {
        Latch { shared: Arc::clone(&self.shared) }
    }
}

impl<T: Clone> Latch<T> {
    /// The value once it is set; `None` when its setter went away first.
    pub fn wait(&self) -> impl Future<Output = Option<T>> + '_ {
        poll_fn(|cx| {
            let mut state = self.shared.state.lock();
            if let Some(value) = &state.value {
                return Poll::Ready(Some(value.clone())); // clone: each waiter reads its own
            }
            if state.abandoned {
                return Poll::Ready(None);
            }
            wait_on(&mut state.waiters, cx);
            Poll::Pending
        })
    }

    /// The value, if it is set.
    pub fn get(&self) -> Option<T> {
        self.shared.state.lock().value.clone() // clone: as above
    }
}

/// What [`block_on`]'s waker does: unparks the thread polling.
struct Unpark {
    parker: Parker,
}

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.parker.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.parker.unpark();
    }
}

/// How [`block_on`] ended without the future's output.
#[derive(Debug)]
pub enum Blocked {
    /// `within` passed first: the future is dropped, which cancels what it asked.
    TimedOut { what: &'static str, within: Duration },
    /// The wait itself failed.
    Wait(WaitError),
}

impl std::fmt::Display for Blocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Blocked::TimedOut { what, within } => write!(f, "waiting for {what}: not done in {within:?}"),
            Blocked::Wait(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for Blocked {}

/// `future` polled on this thread to its end, parked between polls on blaze-core's deadline
/// wait for `what` (rule every-wait-says-what): at most `within` when given. `None` is for a
/// future whose every wait is bounded by deadlines of its own (a request's, which the client
/// ends by its timeout), and says so in `what`.
pub fn block_on<F: Future>(what: &'static str, within: Option<Duration>, future: F) -> Result<F::Output, Blocked> {
    let clock = Monotonic::new();
    let deadline = within.map(|it| Deadline::after(wait::Clock::now(&clock), it));
    let unpark = Arc::new_in(Unpark { parker: Parker::new() }, Global);
    let waker = Waker::from(Arc::clone(&unpark));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return Ok(output);
        }
        match unpark.parker.park(what, deadline, &clock) {
            Ok(Waited::Ready(())) => {}
            Ok(Waited::Deadline) => {
                return Err(Blocked::TimedOut { what, within: within.unwrap_or(Duration::MAX) });
            }
            Err(error) => return Err(Blocked::Wait(error)),
        }
    }
}

/// Which of two futures ended first.
pub enum Either<A, B> {
    First(A),
    Second(B),
}

/// `first` and `second` polled together: the output of whichever ends first, the other
/// dropped. `first` is polled first, so it wins a tie.
pub async fn race<A: Future, B: Future>(first: A, second: B) -> Either<A::Output, B::Output> {
    let mut first = pin!(first);
    let mut second = pin!(second);
    poll_fn(|cx| {
        if let Poll::Ready(it) = first.as_mut().poll(cx) {
            return Poll::Ready(Either::First(it));
        }
        if let Poll::Ready(it) = second.as_mut().poll(cx) {
            return Poll::Ready(Either::Second(it));
        }
        Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a sender sends comes to the reader in order; once every sender is gone the reader
    /// reads `None`; a bounded channel hands back what does not fit; a send to a reader that
    /// is gone is handed back.
    #[test]
    fn a_channel_hands_over_in_order_and_ends_with_its_senders() {
        let (tx, rx) = channel(Some(2));
        let other = tx.clone();
        assert!(tx.send(1).is_ok());
        assert!(other.send(2).is_ok());
        assert_eq!(tx.send(3), Err(3));
        drop((tx, other));
        let got = block_on("the test's items", Some(Duration::from_secs(5)), async {
            (rx.recv().await, rx.recv().await, rx.recv().await)
        })
        .unwrap();
        assert_eq!(got, (Some(1), Some(2), None));
        let (tx, rx) = channel::<u8>(None);
        drop(rx);
        assert_eq!(tx.send(7), Err(7));
    }

    /// A latch is read by every waiter, after it is set, and as `None` once its setter went
    /// away unset.
    #[test]
    fn a_latch_is_read_by_every_waiter() {
        let (set, latch) = latch();
        let other = latch.clone();
        set.set(4);
        set.set(5);
        let got = block_on("the test's latch", Some(Duration::from_secs(5)), async { (latch.wait().await, other.wait().await) }).unwrap();
        assert_eq!(got, (Some(4), Some(4)));
        let (set, latch) = super::latch::<u8>();
        drop(set);
        assert_eq!(block_on("the test's latch", Some(Duration::from_secs(5)), latch.wait()).unwrap(), None);
    }
}
