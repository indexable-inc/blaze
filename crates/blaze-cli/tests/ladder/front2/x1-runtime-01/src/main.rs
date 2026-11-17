//! x1-runtime-01 (front2, on the pinned library's real core and std; success 42, else the failed
//! check's number): a single-thread runtime of tokio's shape, written here with no crate: the
//! current runtime in a `thread_local!` (tokio's runtime/context.rs:82 `CONTEXT`), every task
//! linked into an intrusive list it owns (tokio's `OwnedTasks` over util/linked_list.rs, whose
//! nodes hold `PhantomPinned`), `spawn` of `Pin<Box<dyn Future<Output = ()> + Send + 'static>>`
//! from inside a running task, an `Arc` waker (`std::task::Wake`), and a `std::thread::spawn`ed
//! worker that wakes a task from another thread while the runtime waits on a `Condvar`.
//!
//! The pinned library (nightly-2026-10-27): std/src/thread/local.rs, `thread_local!`: "Declare
//! a new thread local storage key of type [`std::thread::LocalKey`]", and its `const {}` form
//! "can be used when the initialization expression can be evaluated as a constant". core's
//! marker.rs: "A marker type which does not implement `Unpin`. If a type contains a
//! `PhantomPinned`, it will not implement `Unpin` by default." alloc/src/task.rs, `Wake`:
//! `fn wake_by_ref(self: &Arc<Self>) { self.clone().wake(); }`; `Waker::from(Arc<W>)` builds its
//! `RawWaker` in `raw_waker::<W>` with `&RawWakerVTable::new(clone_waker::<W>, wake::<W>,
//! wake_by_ref::<W>, drop_waker::<W>)` (alloc/src/task.rs line 180 on): a borrow PROMOTED to a
//! static (the Reference, "Constant promotion"; core's wake.rs line 197 marks `new`
//! `#[rustc_promotable]`). The Reference, "Await expressions": a pending poll suspends the
//! future "so that, when the surrounding async context is re-polled, execution returns to"
//! the poll.
//!
//! The program: `block_on(root)` sets `CURRENT` and spawns `root` (task 1). `root` checks it
//! sees the runtime, spawns `yielder` (task 2) and `signal` (task 3), yields once: 2 polls.
//! `yielder` yields 3 times (each a `wake_by_ref` then pending): 4 polls. `signal` awaits
//! `FromThread`, whose first poll starts a worker thread holding a clone of the task's waker and
//! is pending; the worker stores 40 and calls `wake`, the runtime (parked on its `Condvar` when
//! nothing else is queued) polls `signal` again, which joins the worker and stores 40 + 2: 2
//! polls. A task's `queued` flag keeps it in the queue at most once. A finished task is
//! unlinked from the list; the run ends when the list is empty; `CURRENT` is cleared after it.
//! Only `main` prints, after the run, from values sorted by task id: the lines are the same
//! whatever order the worker's wake lands in.
//!
//! What each wrong build exits with:
//! - 1: `spawn` inside a task did not see the runtime in `CURRENT`, or `CURRENT` not cleared
//!   after `block_on` (a thread-local read from the wrong thread's block or not at all);
//! - 2: not 3 tasks spawned and 3 finished, or the owned list not empty at the end (a link or
//!   unlink through the intrusive pointers wrong);
//! - 3: the polls per task not 2, 4, 2 (a lost or doubled wake, a waker that wakes another task);
//! - 4: the value not 42 (the worker's store not seen);
//! - 5: not done within 10 seconds (a wake from the worker thread that never reaches the queue
//!   or never wakes the `Condvar`): HUNG is turned into this status, never waited out;
//! - 6: a task polled after it finished (a stale queue entry);
//! - 7: the worker thread could not be joined, or `signal` was ready without it.
//!
//! NEEDS: the run path on the pinned library's std (not a `no_core` twin: threads, `Mutex`,
//! `Condvar`, `thread_local!`, `Instant`), and P10 (constant promotion: `Waker::from` returns
//! a dangling table without it). Until they land the rung is REFUSED by name.
//!
//! The run prints: `front2/x1-runtime-01: PASS (status 42, stdout byte-equal)`.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::marker::PhantomPinned;
use std::pin::Pin;
use std::process::ExitCode;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// The run queue, shared with every waker (and so with the worker thread).
struct Shared {
    queue: Mutex<VecDeque<Arc<Task>>>,
    ready: Condvar,
}

/// A spawned task: its future, its place in the runtime's intrusive list of owned tasks, and
/// whether it is in the run queue. It lives in an `Arc` and is never moved out of it, so the
/// list's raw pointers to it stay valid while it is linked.
struct Task {
    id: u32,
    shared: Arc<Shared>,
    queued: AtomicBool,
    polls: AtomicU32,
    /// The intrusive links; read and written only on the runtime's thread.
    prev: AtomicPtr<Task>,
    next: AtomicPtr<Task>,
    future: Mutex<Option<BoxFuture>>,
    _pinned: PhantomPinned,
}

impl Wake for Task {
    fn wake(self: Arc<Self>) {
        if !self.queued.swap(true, Ordering::SeqCst) {
            let shared = Arc::clone(&self.shared);
            let mut queue = shared.queue.lock().expect("the run queue's lock");
            queue.push_back(self);
            drop(queue);
            shared.ready.notify_one();
        }
    }
}

/// The runtime; it lives on `main`'s stack for the whole of `block_on`.
struct Runtime {
    shared: Arc<Shared>,
    /// The first task of the owned list (null: none).
    head: Cell<*const Task>,
    next_id: Cell<u32>,
    /// (task id, polls) of each finished task, in the order they finished.
    done: RefCell<Vec<(u32, u32)>>,
}

thread_local! {
    /// The runtime whose `block_on` runs on this thread (null: none).
    static CURRENT: Cell<*const Runtime> = const { Cell::new(ptr::null()) };
}

static SAW_RUNTIME: AtomicBool = AtomicBool::new(false);
static VALUE: AtomicU32 = AtomicU32::new(0);
static FAILED: AtomicU32 = AtomicU32::new(0);

impl Runtime {
    fn new() -> Runtime {
        Runtime {
            shared: Arc::new(Shared { queue: Mutex::new(VecDeque::new()), ready: Condvar::new() }),
            head: Cell::new(ptr::null()),
            next_id: Cell::new(1),
            done: RefCell::new(Vec::new()),
        }
    }

    fn spawn_here(&self, future: BoxFuture) {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let task = Arc::new(Task {
            id,
            shared: Arc::clone(&self.shared),
            queued: AtomicBool::new(true),
            polls: AtomicU32::new(0),
            prev: AtomicPtr::new(ptr::null_mut()),
            next: AtomicPtr::new(ptr::null_mut()),
            future: Mutex::new(Some(future)),
            _pinned: PhantomPinned,
        });
        self.link(Arc::clone(&task));
        self.shared.queue.lock().expect("the run queue's lock").push_back(task);
    }

    /// Puts `task` at the head of the owned list; the list holds the `Arc` as a raw pointer.
    fn link(&self, task: Arc<Task>) {
        let node = Arc::into_raw(task);
        let head = self.head.get();
        // SAFETY: `node` came from `Arc::into_raw` and the list keeps that count until `unlink`.
        let task = unsafe { &*node };
        task.next.store(head.cast_mut(), Ordering::Relaxed);
        task.prev.store(ptr::null_mut(), Ordering::Relaxed);
        if !head.is_null() {
            // SAFETY: every non-null pointer in the list is a linked task the list keeps alive.
            let first = unsafe { &*head };
            first.prev.store(node.cast_mut(), Ordering::Relaxed);
        }
        self.head.set(node);
    }

    /// Takes the task at `node` out of the owned list and hands back the list's `Arc`.
    fn unlink(&self, node: *const Task) -> Arc<Task> {
        // SAFETY: `node` is linked (a task is unlinked once, when it finishes), so the list
        // keeps it alive, and so are its neighbours.
        let task = unsafe { &*node };
        let prev = task.prev.load(Ordering::Relaxed);
        let next = task.next.load(Ordering::Relaxed);
        if prev.is_null() {
            self.head.set(next.cast_const());
        } else {
            // SAFETY: as above: a linked neighbour.
            let before = unsafe { &*prev };
            before.next.store(next, Ordering::Relaxed);
        }
        if !next.is_null() {
            // SAFETY: as above: a linked neighbour.
            let after = unsafe { &*next };
            after.prev.store(prev, Ordering::Relaxed);
        }
        // SAFETY: the count `link` gave up with `Arc::into_raw`, taken back once.
        unsafe { Arc::from_raw(node) }
    }

    fn block_on(&self, root: BoxFuture, deadline: Instant) -> Result<(), u8> {
        CURRENT.with(|current| current.set(self as *const Runtime));
        self.spawn_here(root);
        let ran = self.run(deadline);
        CURRENT.with(|current| current.set(ptr::null()));
        ran
    }

    /// Polls queued tasks until the owned list is empty; `Err(5)` at the deadline.
    fn run(&self, deadline: Instant) -> Result<(), u8> {
        while !self.head.get().is_null() {
            let Some(task) = self.next_task(deadline) else {
                return Err(5);
            };
            task.queued.store(false, Ordering::SeqCst);
            task.polls.fetch_add(1, Ordering::SeqCst);
            let waker = Waker::from(Arc::clone(&task));
            let mut cx = Context::from_waker(&waker);
            let mut slot = task.future.lock().expect("a task's future's lock");
            let Some(future) = slot.as_mut() else {
                return Err(6);
            };
            if future.as_mut().poll(&mut cx).is_ready() {
                *slot = None;
                drop(slot);
                self.done.borrow_mut().push((task.id, task.polls.load(Ordering::SeqCst)));
                drop(self.unlink(Arc::as_ptr(&task)));
            }
        }
        Ok(())
    }

    /// The next queued task, waiting on the `Condvar` while the queue is empty; `None` at the
    /// deadline.
    fn next_task(&self, deadline: Instant) -> Option<Arc<Task>> {
        let mut queue = self.shared.queue.lock().expect("the run queue's lock");
        loop {
            if let Some(task) = queue.pop_front() {
                return Some(task);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let (guard, _timed_out) = self.shared.ready.wait_timeout(queue, deadline - now).expect("the run queue's lock");
            queue = guard;
        }
    }
}

/// Spawns `future` on the runtime running on this thread.
fn spawn(future: BoxFuture) {
    let runtime = CURRENT.with(|current| current.get());
    assert!(!runtime.is_null(), "spawn outside a runtime");
    // SAFETY: `CURRENT` holds a runtime only while that runtime's `block_on` runs on this thread,
    // and the runtime outlives the call.
    let runtime = unsafe { &*runtime };
    runtime.spawn_here(future);
}

/// Pending `left` times, waking its task each time; then ready.
struct YieldNow {
    left: u32,
}

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.left == 0 {
            Poll::Ready(())
        } else {
            self.left -= 1;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

/// Ready with the value a worker thread stores; the first poll starts that worker with a clone
/// of the task's waker. `Err(7)`: the worker could not be joined.
struct FromThread {
    slot: Arc<Mutex<Option<u32>>>,
    worker: Option<JoinHandle<()>>,
}

impl Future for FromThread {
    type Output = Result<u32, u8>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<u32, u8>> {
        let got = *self.slot.lock().expect("the worker's slot's lock");
        if let Some(value) = got {
            let joined = match self.worker.take() {
                Some(worker) => worker.join().is_ok(),
                None => false,
            };
            return Poll::Ready(if joined { Ok(value) } else { Err(7) });
        }
        if self.worker.is_none() {
            let slot = Arc::clone(&self.slot);
            let waker = cx.waker().clone();
            self.worker = Some(thread::spawn(move || {
                *slot.lock().expect("the worker's slot's lock") = Some(40);
                waker.wake();
            }));
        }
        Poll::Pending
    }
}

async fn root() {
    SAW_RUNTIME.store(CURRENT.with(|current| !current.get().is_null()), Ordering::SeqCst);
    spawn(Box::pin(yielder()));
    spawn(Box::pin(signal()));
    YieldNow { left: 1 }.await;
}

async fn yielder() {
    YieldNow { left: 3 }.await;
}

async fn signal() {
    match (FromThread { slot: Arc::new(Mutex::new(None)), worker: None }).await {
        Ok(value) => VALUE.store(value + 2, Ordering::SeqCst),
        Err(code) => FAILED.store(u32::from(code), Ordering::SeqCst),
    }
}

fn main() -> ExitCode {
    let runtime = Runtime::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let ran = runtime.block_on(Box::pin(root()), deadline);
    let cleared = CURRENT.with(|current| current.get().is_null());

    let spawned = runtime.next_id.get() - 1;
    let mut done = runtime.done.borrow().clone();
    done.sort();
    let polls: Vec<String> = done.iter().map(|(id, n)| format!("task {id}: {n}")).collect();
    let value = VALUE.load(Ordering::SeqCst);
    println!("spawned {spawned}, finished {}", done.len());
    println!("polls: {}", polls.join(", "));
    println!("value from the worker thread: {value}");

    if let Err(code) = ran {
        return ExitCode::from(code);
    }
    if FAILED.load(Ordering::SeqCst) != 0 {
        return ExitCode::from(7);
    }
    if !SAW_RUNTIME.load(Ordering::SeqCst) || !cleared {
        return ExitCode::from(1);
    }
    if spawned != 3 || done.len() != 3 || !runtime.head.get().is_null() {
        return ExitCode::from(2);
    }
    if done != [(1, 2), (2, 4), (3, 2)] {
        return ExitCode::from(3);
    }
    if value != 42 {
        return ExitCode::from(4);
    }
    ExitCode::from(42)
}
