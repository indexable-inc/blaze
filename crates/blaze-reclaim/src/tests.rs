use std::{
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use super::*;

/// Drops set their flag.
struct Dropped(Arc<AtomicBool>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

/// Collects until `limbo` is empty. Other tests of this process hold guards of their own, so
/// a grace can take a moment to pass.
fn collect_all(limbo: &mut Limbo) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !limbo.is_empty() {
        limbo.collect();
        assert!(Instant::now() < deadline, "a grace never passed with no task of this test open");
        thread::yield_now();
    }
}

#[test]
fn a_value_is_kept_while_a_task_that_began_before_its_grace_runs() {
    let (entered, leave) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
    let reader = {
        let (entered, leave) = (entered.clone(), leave.clone());
        thread::spawn(move || {
            let guard = enter();
            entered.wait();
            leave.wait();
            drop(guard);
        })
    };
    entered.wait();
    let dropped = flag();
    let mut limbo = Limbo::new();
    limbo.retire(Dropped(dropped.clone()));
    for _ in 0..100 {
        limbo.collect();
    }
    assert!(!dropped.load(Ordering::SeqCst), "freed under a task that could read it");
    leave.wait();
    reader.join().unwrap();
    collect_all(&mut limbo);
    assert!(dropped.load(Ordering::SeqCst));
}

#[test]
fn a_task_that_begins_after_a_grace_does_not_hold_it() {
    let mut limbo = Limbo::new();
    let dropped = flag();
    limbo.retire(Dropped(dropped.clone()));
    let (entered, leave) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
    let reader = {
        let (entered, leave) = (entered.clone(), leave.clone());
        thread::spawn(move || {
            let guard = enter();
            entered.wait();
            leave.wait();
            drop(guard);
        })
    };
    entered.wait();
    collect_all(&mut limbo);
    assert!(dropped.load(Ordering::SeqCst), "a later task held an earlier grace");
    leave.wait();
    reader.join().unwrap();
}

#[test]
fn only_the_outermost_guard_ends_a_task() {
    let outer = enter();
    let inner = enter();
    assert!(in_task());
    drop(inner);
    assert!(in_task(), "a pumped job's end is not its waiter's");
    let grace = Grace::start();
    assert!(!grace.passed(), "this thread is still in the task that began before the grace");
    drop(outer);
    assert!(!in_task());
    let deadline = Instant::now() + Duration::from_secs(20);
    while !grace.passed() {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
}

#[test]
fn a_thread_that_ends_gives_its_slot_back() {
    for _ in 0..(SLOTS * 3) {
        thread::spawn(|| drop(enter())).join().unwrap();
    }
    let taken: usize = blocks().map(|block| block.slots.iter().filter(|it| it.taken.load(Ordering::SeqCst)).count()).sum();
    assert!(taken < SLOTS * 2, "{taken} slots held after their threads ended");
}

/// Readers swap through a shared pointer while a writer replaces what it points to and frees
/// the old nodes through a limbo. A node is marked freed when dropped; a reader that finds its
/// node marked while still in the task that loaded it has read freed memory.
#[test]
fn readers_never_see_a_node_freed_under_them() {
    struct Node {
        id: usize,
        freed: Arc<Vec<AtomicBool>>,
    }
    impl Drop for Node {
        fn drop(&mut self) {
            self.freed[self.id].store(true, Ordering::SeqCst);
        }
    }
    const NODES: usize = 20_000;
    let freed: Arc<Vec<AtomicBool>> = Arc::new((0..NODES).map(|_| AtomicBool::new(false)).collect());
    let shared = Arc::new(AtomicPtr::new(Box::into_raw(Box::new(Node { id: 0, freed: freed.clone() }))));
    let stop = Arc::new(AtomicBool::new(false));
    let (bad, reads) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicU64::new(0)));
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let (shared, stop, freed, bad, reads) =
                (shared.clone(), stop.clone(), freed.clone(), bad.clone(), reads.clone());
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let guard = enter();
                    for _ in 0..8 {
                        // SAFETY: what the pointer names is freed only after a grace that
                        // began once it was unlinked, and this task is inside that grace.
                        let node = unsafe { &*shared.load(Ordering::Acquire) };
                        let id = node.id;
                        thread::yield_now();
                        if freed[id].load(Ordering::SeqCst) {
                            bad.fetch_add(1, Ordering::SeqCst);
                        }
                        reads.fetch_add(1, Ordering::Relaxed);
                    }
                    drop(guard);
                }
            })
        })
        .collect();
    let mut limbo = Limbo::new();
    for id in 1..NODES {
        let new = Box::into_raw(Box::new(Node { id, freed: freed.clone() }));
        let old = shared.swap(new, Ordering::AcqRel);
        // SAFETY: `old` was unlinked by the swap and is owned by the limbo from here.
        limbo.retire(unsafe { Box::from_raw(old) });
        if id % 64 == 0 {
            limbo.collect();
        }
    }
    stop.store(true, Ordering::SeqCst);
    for reader in readers {
        reader.join().unwrap();
    }
    collect_all(&mut limbo);
    assert_eq!(bad.load(Ordering::SeqCst), 0, "a node was freed while a task read it");
    assert!(reads.load(Ordering::Relaxed) > 0);
    // SAFETY: every reader has ended; the last node is owned here.
    drop(unsafe { Box::from_raw(shared.load(Ordering::Acquire)) });
    assert!(freed.iter().all(|it| it.load(Ordering::SeqCst)), "every node was freed in the end");
}
