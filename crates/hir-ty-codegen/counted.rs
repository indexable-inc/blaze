//! What one build compiled, counted by the build itself: a worker builds several programs side
//! by side in one process, so a count of the whole process says what all of them compiled.
//!
//! A [`Counting`] counts what is compiled on its thread, and on the threads it hands its
//! counts to ([`now`], [`within`]), until it is dropped. Countings nest: what an inner one
//! counts, the ones around it count too.

#![forbid(unsafe_code)]

use std::{
    cell::RefCell,
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use base_db::Crate;
use blaze_hash::SharedMap;

/// The counts of one [`Counting`].
#[derive(Default)]
pub struct Counted {
    /// The counting around this one when it started, which counts all this one does.
    around: Option<Arc<Counted>>,
    /// Functions put through Cranelift, or through LLVM at level 1: what a build compiled
    /// rather than found.
    compiled: AtomicUsize,
    /// Compile queries the query engine ran rather than handed back, in all and by the crate each was
    /// for: what a build says of itself as it goes.
    compiles: AtomicUsize,
    compiles_of: SharedMap<Crate, AtomicUsize>,
}

impl Counted {
    /// Functions put through Cranelift, or through LLVM at level 1.
    pub fn compiled(&self) -> usize {
        self.compiled.load(Ordering::Relaxed)
    }

    /// Compile queries run.
    pub fn compiles(&self) -> usize {
        self.compiles.load(Ordering::Relaxed)
    }

    /// Compile queries run, by the crate each was for.
    pub fn compiles_by_crate(&self) -> Vec<(Crate, usize)> {
        let of = self.compiles_of.pin();
        // alloc: once per build, for its report
        of.iter().map(|(&krate, count)| (krate, count.load(Ordering::Relaxed))).collect()
    }
}

thread_local! {
    static COUNTING: RefCell<Option<Arc<Counted>>> = const { RefCell::new(None) };
}

/// Counts what is compiled on this thread until it is dropped, on the thread it was made on.
pub struct Counting {
    counted: Arc<Counted>,
    held: Option<Arc<Counted>>,
    on_this_thread: PhantomData<*const ()>,
}

impl Counting {
    pub fn start() -> Counting {
        // alloc: once per build; the counts outlive it on the threads it hands them to
        let counted = Arc::new(Counted { around: now(), ..Counted::default() });
        // clone: once per build, the same `Arc` held by this thread's slot as well
        let held = COUNTING.with(|it| it.replace(Some(counted.clone())));
        Counting { counted, held, on_this_thread: PhantomData }
    }

    pub fn counted(&self) -> &Arc<Counted> {
        &self.counted
    }
}

impl Drop for Counting {
    fn drop(&mut self) {
        let held = self.held.take();
        COUNTING.with(|it| *it.borrow_mut() = held);
    }
}

/// What counts this thread's compiles, for a thread that works for it to count into.
pub fn now() -> Option<Arc<Counted>> {
    // clone: once per build or per batch handed to another thread, which must own it
    COUNTING.with(|it| it.borrow().clone())
}

/// Runs `work` with its compiles counted into `counted`, as [`now`] gave it on another thread.
pub fn within<T>(counted: Option<Arc<Counted>>, work: impl FnOnce() -> T) -> T {
    let held = COUNTING.with(|it| it.replace(counted));
    struct Restore(Option<Option<Arc<Counted>>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let held = self.0.take().flatten();
            COUNTING.with(|it| *it.borrow_mut() = held);
        }
    }
    let _restore = Restore(Some(held));
    work()
}

fn each(count: impl Fn(&Counted)) {
    COUNTING.with(|it| {
        let held = it.borrow();
        let mut at = held.as_deref();
        while let Some(counted) = at {
            count(counted);
            at = counted.around.as_deref();
        }
    });
}

/// A function was put through Cranelift, or through LLVM at level 1.
pub(crate) fn compiled() {
    each(|it| _ = it.compiled.fetch_add(1, Ordering::Relaxed));
}

/// The query engine ran a compile query for `krate`.
pub(crate) fn compiles(krate: Crate) {
    each(|it| {
        it.compiles.fetch_add(1, Ordering::Relaxed);
        it.compiles_of
            .pin()
            .get_or_insert_with(krate, AtomicUsize::default)
            .fetch_add(1, Ordering::Relaxed);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_side_by_side_count_their_own() {
        let outer = Counting::start();
        let inner = Counting::start();
        compiled();
        let handed = now();
        std::thread::spawn(move || within(handed, compiled)).join().unwrap();
        // Another build on another thread counts into its own.
        let other = std::thread::spawn(|| {
            let other = Counting::start();
            compiled();
            other.counted().compiled()
        });
        assert_eq!(other.join().unwrap(), 1);
        assert_eq!(inner.counted().compiled(), 2);
        drop(inner);
        compiled();
        assert_eq!(outer.counted().compiled(), 3);
        drop(outer);
        assert!(now().is_none());
        compiled();
    }
}
