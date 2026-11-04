//! A value that lives on a thread of its own and is reached by calls: how a multi-threaded program
//! (the daemon) uses a [`Store`](crate::Store), which never leaves the thread that made it.
//!
//! Two ways in. [`Home::run`] is for async code: it queues the call and awaits a oneshot, so the
//! caller's runtime thread stays free while the call waits its turn and runs. [`Home::call`] is
//! for code that is not async: it blocks its thread until the call has run.

use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::mpsc;
use std::thread::{self, ThreadId};

use futures_channel::oneshot;

/// One queued call, run once on the home thread.
trait Job<T>: Send {
    fn run(self: Box<Self>, value: &mut T);
}

type Queued<T> = Box<dyn Job<T> + 'static>;

/// A call that may borrow from the caller of [`Home::call`] (`'a`). Field order is the point:
/// Rust drops struct fields in declaration order, so a job dropped unrun drops `f` (and every
/// borrow it holds) before `done`, and the caller, blocked until `done` is gone, cannot return
/// while anything of `f` is still alive.
struct Borrowing<F, R> {
    f: F,
    done: mpsc::SyncSender<thread::Result<R>>,
}

impl<T, F, R> Job<T> for Borrowing<F, R>
where
    F: FnOnce(&mut T) -> R + Send,
    R: Send,
{
    fn run(self: Box<Self>, value: &mut T) {
        let Borrowing { f, done } = *self;
        // `f` is consumed here, before the result is sent: nothing of it outlives the send.
        let out = catch_unwind(AssertUnwindSafe(|| f(value)));
        let _ = done.send(out);
    }
}

/// A call from [`Home::run`]: owns everything it touches, so it may outlive the caller's future.
struct Owned<F, R> {
    f: F,
    done: oneshot::Sender<thread::Result<R>>,
}

impl<T, F, R> Job<T> for Owned<F, R>
where
    F: FnOnce(&mut T) -> R + Send + 'static,
    R: Send + 'static,
{
    fn run(self: Box<Self>, value: &mut T) {
        let Owned { f, done } = *self;
        let out = catch_unwind(AssertUnwindSafe(|| f(value)));
        // The caller may have dropped its future; the call has run all the same.
        let _ = done.send(out);
    }
}

/// The handle to a value on its home thread. Clones share the thread; it ends once every handle is
/// dropped and the calls already sent have run.
pub struct Home<T: 'static> {
    jobs: mpsc::Sender<Queued<T>>,
    thread: ThreadId,
}

impl<T: 'static> Clone for Home<T> {
    fn clone(&self) -> Self {
        Self {
            jobs: self.jobs.clone(),
            thread: self.thread,
        }
    }
}

impl<T: 'static> std::fmt::Debug for Home<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Home")
            .field("thread", &self.thread)
            .finish()
    }
}

impl<T: 'static> Home<T> {
    /// Start the thread `name` and build the value on it with `make`.
    pub fn spawn(name: &str, make: impl FnOnce() -> T + Send + 'static) -> std::io::Result<Self> {
        let (jobs, rx) = mpsc::channel::<Queued<T>>();
        let handle = thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                let mut value = make();
                while let Ok(job) = rx.recv() {
                    job.run(&mut value);
                }
            })?;
        Ok(Self {
            jobs,
            thread: handle.thread().id(),
        })
    }

    /// Run `f` on the value, on its thread, and block until it has run. For code that is not
    /// async: on an async runtime it holds a worker thread for the whole wait, and deadlocks if
    /// anything under `f` waits for a task of that runtime; use [`Home::run`] there. Calls run one
    /// at a time in the order they arrive. A panic in `f` comes back here; the thread lives on.
    /// Calling from the home thread itself (from inside another call) would wait forever, so it
    /// panics.
    pub fn call<R: Send>(&self, f: impl FnOnce(&mut T) -> R + Send) -> R {
        assert!(
            !self.is_home(),
            "Home::call from its own thread would wait on itself"
        );
        let (done, result) = mpsc::sync_channel::<thread::Result<R>>(1);
        let job: Box<dyn Job<T> + '_> = Box::new(Borrowing { f, done });
        // SAFETY: only the lifetime is erased. This function does not return before `result`
        // reports: the job sent its result (after `f` was consumed) or the job was dropped unrun,
        // and `Borrowing` drops `f` before `done` (declaration order). Either way nothing the job
        // borrows from the caller is touched after `call` returns.
        let job: Queued<T> = unsafe { std::mem::transmute::<Box<dyn Job<T> + '_>, Queued<T>>(job) };
        if self.jobs.send(job).is_err() {
            panic!("the home thread is gone");
        }
        match result.recv() {
            Ok(Ok(out)) => out,
            Ok(Err(panic)) => resume_unwind(panic),
            Err(_) => panic!("the home thread dropped a call unrun"),
        }
    }

    /// Run `f` on the value, on its thread, and await its result without holding the caller's
    /// thread. Calls (from `run` and [`Home::call`] alike) run one at a time in the order they
    /// were queued, which is when `run` is called, not when its future is first polled. `f` owns
    /// what it uses (`'static`): the call runs even if the future is dropped, and its result is
    /// then discarded. A panic in `f` resumes in the awaiting task; the thread lives on.
    pub fn run<R>(
        &self,
        f: impl FnOnce(&mut T) -> R + Send + 'static,
    ) -> impl Future<Output = R> + Send + 'static
    where
        R: Send + 'static,
    {
        let (done, result) = oneshot::channel::<thread::Result<R>>();
        let sent = self.jobs.send(Box::new(Owned { f, done })).is_ok();
        async move {
            assert!(sent, "the home thread is gone");
            match result.await {
                Ok(Ok(out)) => out,
                Ok(Err(panic)) => resume_unwind(panic),
                Err(_) => panic!("the home thread dropped a call unrun"),
            }
        }
    }

    /// True on the value's own thread.
    pub fn is_home(&self) -> bool {
        thread::current().id() == self.thread
    }
}

// These tests touch no flecs, so they also run under Miri (`cargo miri test -p harness-native-ecs home::`),
// which checks the lifetime erasure in `call`.
#[cfg(test)]
mod tests {
    use super::Home;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    #[test]
    fn calls_borrow_from_the_caller_and_run_in_order() {
        let home = Home::spawn("v", Vec::<String>::new).unwrap();
        let local = String::from("borrowed");
        home.call(|v| v.push(local.clone()));
        let later = home.run(|v| {
            v.push("owned".into());
            v.len()
        });
        home.call(|v| v.push(format!("{}!", local)));
        assert_eq!(futures_executor::block_on(later), 2);
        assert_eq!(home.call(|v| v.clone()), ["borrowed", "owned", "borrowed!"]);
    }

    #[test]
    fn a_panic_comes_back_to_the_caller_and_the_thread_lives_on() {
        let home = Home::spawn("v", || 0u32).unwrap();
        let caught = catch_unwind(AssertUnwindSafe(|| home.call(|_| panic!("in call"))));
        assert!(caught.is_err());
        let caught = catch_unwind(AssertUnwindSafe(|| {
            futures_executor::block_on(home.run(|_| panic!("in run")))
        }));
        assert!(caught.is_err());
        assert_eq!(home.call(|n| *n + 1), 1);
    }

    /// F3: a call dropped unrun (the home thread died) must drop what it borrows before `call`
    /// returns. `Borrowing` drops `f` before `done` by field order; a closure's captures have no
    /// specified drop order.
    #[test]
    fn a_call_dropped_unrun_drops_its_borrows_before_call_returns() {
        struct SlowDrop<'a>(&'a AtomicBool);
        impl Drop for SlowDrop<'_> {
            fn drop(&mut self) {
                std::thread::sleep(Duration::from_millis(50));
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let (open, gate) = mpsc::channel::<()>();
        let home = Home::spawn("dies", move || -> u32 {
            let _ = gate.recv();
            panic!("the value could not be made");
        })
        .unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let caller = {
            let dropped = dropped.clone();
            std::thread::spawn(move || {
                let flag: &AtomicBool = &dropped;
                let guard = SlowDrop(flag);
                let out = catch_unwind(AssertUnwindSafe(|| {
                    home.call(move |_| {
                        let _keep = &guard;
                    })
                }));
                // `call` has returned (with a panic): the borrow must already be gone.
                (out.is_err(), flag.load(Ordering::SeqCst))
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        open.send(()).unwrap();
        let (refused, dropped_first) = caller.join().unwrap();
        assert!(refused, "a call that never ran must not return a value");
        assert!(dropped_first, "the job's borrow outlived Home::call");
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn a_dropped_run_future_still_runs_its_call() {
        let home = Home::spawn("v", || 0u32).unwrap();
        drop(home.run(|n| *n += 1));
        assert_eq!(home.call(|n| *n), 1);
    }

    /// P2: `call` holds its thread until the call has run, so on a one-thread runtime a call that
    /// waits for a task of that runtime never finishes. `run` awaits instead, and the task gets to
    /// run.
    #[cfg_attr(miri, ignore)]
    #[test]
    fn run_leaves_the_runtime_thread_free() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let home = Home::spawn("v", || 0u32).unwrap();
        let got = rt.block_on(async {
            let (tx, rx) = mpsc::channel::<u32>();
            let waits = home.run(move |n| {
                *n = rx.recv_timeout(Duration::from_secs(10)).unwrap_or(0);
                *n
            });
            tokio::spawn(async move { tx.send(7).unwrap() });
            waits.await
        });
        assert_eq!(got, 7);
    }
}
