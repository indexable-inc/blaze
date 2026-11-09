//! Model tests (docs/v2/plan/design/model-checker.md) of the slots, `enter`/`Guard`, `Grace`,
//! `Horizon` and `Limbo`, from 2 or 3 threads under every interleaving to the preemption bound and every
//! value C11 lets a load read. A retired value holds a tracked cell its readers read and its
//! drop writes, so a free not ordered after a read is a data race the checker reports. What
//! reclaim promises is one-sided (a grace may pass later than it could, never earlier), so these
//! check it with asserts and the race check rather than a sequential model.

use std::{ptr, sync::Arc};

use blaze_check::{cell::UnsafeCell, hint, model, sync::lot::Mutex, thread};

use super::{Grace, Horizon, LOCAL, Limbo, blocks, enter};
use crate::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};

/// A value behind a shared pointer: readers read `n` in their tasks, its drop writes it.
struct Node {
    n: UnsafeCell<u64>,
    drops: Arc<AtomicUsize>,
}

// SAFETY: `n` is reached only through `with` and `get_mut`, each an access the model tracks. It
// runs every thread on one OS thread, so no two accesses overlap, and one its clocks do not
// order is reported as a race.
unsafe impl Sync for Node {}

impl Node {
    fn new(n: u64, drops: &Arc<AtomicUsize>) -> *mut Node {
        Box::into_raw(Box::new(Node { n: UnsafeCell::new(n), drops: Arc::clone(drops) }))
    }

    fn n(&self) -> u64 {
        // SAFETY: a `u64` of a live node: the caller loaded it in a task begun before any grace
        // that covers it.
        self.n.with(|n| unsafe { *n })
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        *self.n.get_mut() = u64::MAX;
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

/// One task of this thread, reading what `shared` points to.
fn read(shared: &AtomicPtr<Node>) -> u64 {
    let guard = enter();
    // SAFETY: what `shared` names is freed only through a limbo, after a grace started once it
    // was unlinked, and this task holds `guard` until it is done reading.
    let n = unsafe { &*shared.load(Ordering::Acquire) }.n();
    drop(guard);
    n
}

/// Links `new` in place of what `shared` points to and retires the old one.
fn unlink(shared: &AtomicPtr<Node>, new: *mut Node, limbo: &mut Limbo) {
    let old = shared.swap(new, Ordering::AcqRel);
    // SAFETY: the swap unlinked `old`, a `Box::into_raw` of `Node::new`; the limbo owns it now.
    limbo.retire(unsafe { Box::from_raw(old) });
}

/// The writer's side after its reader ended: the grace has passed, the unlinked node was freed
/// once, and the linked one is freed here.
fn settle(shared: &AtomicPtr<Node>, mut limbo: Limbo, drops: &AtomicUsize) {
    limbo.collect();
    assert!(limbo.is_empty(), "a grace outlived every task");
    assert_eq!(drops.load(Ordering::Relaxed), 1, "the unlinked node was freed once");
    // SAFETY: the reader has ended; the node still linked is owned here.
    drop(unsafe { Box::from_raw(shared.load(Ordering::Acquire)) });
    assert_eq!(drops.load(Ordering::Relaxed), 2);
}

/// A reader that loaded a node before the writer unlinked it reads it whole: the writer's
/// limbo frees it only once the reader's task has ended.
#[test]
fn model_a_value_outlives_the_task_that_read_it() {
    model(|| {
        let drops = Arc::new(AtomicUsize::new(0));
        let shared = Arc::new(AtomicPtr::new(Node::new(1, &drops)));
        let reader = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || read(&shared))
        };
        let mut limbo = Limbo::new();
        unlink(&shared, Node::new(2, &drops), &mut limbo);
        limbo.collect();
        let n = reader.join().unwrap();
        assert!(n == 1 || n == 2, "a reader read {n}");
        settle(&shared, limbo, &drops);
    });
}

/// A reader's next task keeps the reads of its last one ordered before a writer that sees the
/// new task's epoch and frees what they read.
#[test]
fn model_a_readers_next_task_keeps_its_last_one_ordered() {
    model(|| {
        let drops = Arc::new(AtomicUsize::new(0));
        let shared = Arc::new(AtomicPtr::new(Node::new(1, &drops)));
        let reader = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                let n = read(&shared);
                drop(enter());
                n
            })
        };
        let mut limbo = Limbo::new();
        unlink(&shared, Node::new(2, &drops), &mut limbo);
        limbo.collect();
        let n = reader.join().unwrap();
        assert!(n == 1 || n == 2, "a reader read {n}");
        settle(&shared, limbo, &drops);
    });
}

/// A thread in a task until both are: gives the address of its slot.
fn meet(entered: Arc<AtomicU32>) -> impl FnOnce() -> usize + Send + 'static {
    move || {
        let guard = enter();
        entered.fetch_add(1, Ordering::AcqRel);
        while entered.load(Ordering::Acquire) < 2 {
            hint::spin_loop();
        }
        let slot = LOCAL.with(|local| local.slot.get()).map(ptr::from_ref);
        drop(guard);
        slot.expect("a thread in a task has a slot").addr()
    }
}

/// Two threads in tasks at once hold two slots, and each gives its slot back when it ends.
#[test]
fn model_threads_in_tasks_at_once_hold_different_slots() {
    model(|| {
        let entered = Arc::new(AtomicU32::new(0));
        let a = thread::spawn(meet(Arc::clone(&entered)));
        let b = thread::spawn(meet(Arc::clone(&entered)));
        let (a, b) = (a.join().unwrap(), b.join().unwrap());
        assert_ne!(a, b, "two threads in tasks at once share a slot");
        let taken = blocks()
            .flat_map(|block| &block.slots)
            .filter(|slot| slot.taken.load(Ordering::Acquire))
            .count();
        assert_eq!(taken, 0, "a slot is still taken after its thread ended");
    });
}

/// What the writers of tls_cache's tables share: values each retired under the table's lock,
/// with the grace it started there.
type Retired = Mutex<Vec<(Grace, Box<Node>)>>;

/// T4's fix (v2-spinstuck.md): a writer takes its `Horizon` before it locks the limbo, so the
/// scan of every slot is out from under the lock, and frees by it under the lock what another
/// writer retired there meanwhile. A grace that writer started after the horizon was taken must
/// not count as passed: a reader may have entered after the scan, unseen by it, and loaded the
/// value before the other writer unlinked it. Reader, unlinker and freer are three threads; a
/// free not ordered after the reader's read is a race the checker reports.
///
/// Both halves of `Horizon::now` are load-bearing here: without the `min` with the epoch, a scan
/// that found no thread in a task passes every grace; with the epoch loaded after the scan, a
/// grace started between the two is passed although the scan never saw its reader.
#[test]
fn model_a_horizon_taken_before_a_lock_frees_nothing_retired_after_it() {
    model(|| {
        let drops = Arc::new(AtomicUsize::new(0));
        let shared = Arc::new(AtomicPtr::new(Node::new(1, &drops)));
        let retired: Arc<Retired> = Arc::new(Mutex::new(Vec::new()));
        let reader = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || read(&shared))
        };
        let unlinker = {
            let (shared, retired, drops) =
                (Arc::clone(&shared), Arc::clone(&retired), Arc::clone(&drops));
            thread::spawn(move || {
                let old = shared.swap(Node::new(2, &drops), Ordering::AcqRel);
                let mut retired = retired.lock();
                // SAFETY: the swap unlinked `old`, a `Box::into_raw` of `Node::new`; the list owns
                // it now.
                retired.push((Grace::start(), unsafe { Box::from_raw(old) }));
            })
        };
        let horizon = Horizon::now();
        retired.lock().retain(|&(grace, _)| !horizon.passed(grace));
        let n = reader.join().unwrap();
        assert!(n == 1 || n == 2, "a reader read {n}");
        unlinker.join().unwrap();
        // Every task has ended: what is left is freed, and so is the node still linked.
        retired.lock().clear();
        assert_eq!(drops.load(Ordering::Relaxed), 1, "the unlinked node was freed once");
        // SAFETY: the reader has ended; the node still linked is owned here.
        drop(unsafe { Box::from_raw(shared.load(Ordering::Acquire)) });
        assert_eq!(drops.load(Ordering::Relaxed), 2);
    });
}
