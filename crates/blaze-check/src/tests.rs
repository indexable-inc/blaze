//! The checker against litmus tests whose allowed outcomes C11 fixes (each outcome set is the
//! one the C++ memory model and herd's RC11 give; where loom or Miri reach fewer, the comment
//! says), against programs with a known race or deadlock, and against itself (replay, bounds).

use std::{
    cell::Cell as StdCell,
    collections::BTreeSet,
    panic::{AssertUnwindSafe, Location, catch_unwind},
    sync::{Arc as StdArc, atomic::AtomicU64 as Id},
    time::Duration,
};

use parking_lot::Mutex as StdMutex;

use crate::{
    Builder,
    cell::UnsafeCell,
    linear::{History, Spec},
    model,
    path::{Mode, Path},
    rt::{FENCE, Kind, Object, Op, Runtime, memory},
    sync::{
        Arc, Condvar, Mutex, OnceLock, RwLock,
        atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, AtomicUsize, Ordering, fence},
        lot,
    },
    thread,
};

use Ordering::{AcqRel, Acquire, Relaxed, Release, SeqCst};

/// Every outcome `f` returns over the model's executions.
fn outcomes<T: Ord + Clone + 'static>(builder: Builder, f: impl Fn() -> T + 'static) -> BTreeSet<T> {
    let seen = StdArc::new(StdMutex::new(BTreeSet::new()));
    let into = StdArc::clone(&seen);
    builder.check(move || {
        let out = f();
        into.lock().insert(out);
    });
    let seen = seen.lock().clone();
    seen
}

/// The report of the finding `f` must make.
fn finding(builder: Builder, f: impl Fn() + 'static) -> String {
    let out = catch_unwind(AssertUnwindSafe(|| builder.check(f)));
    match out {
        Ok(()) => panic!("the model found nothing"),
        Err(panic) => match panic.downcast::<String>() {
            Ok(report) => *report,
            Err(_) => panic!("the model's report is a String"),
        },
    }
}

fn builder() -> Builder {
    let mut builder = Builder::new();
    builder.max_preemptions = Some(3);
    builder.random = None;
    builder.replay = None;
    builder
}

fn store_buffering(store: Ordering, load: Ordering) -> BTreeSet<(u32, u32)> {
    outcomes(builder(), move || {
        let x = Arc::new(AtomicU32::new(0));
        let y = Arc::new(AtomicU32::new(0));
        let other = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || {
                y.store(1, store);
                x.load(load)
            })
        };
        x.store(1, store);
        let a = y.load(load);
        let b = other.join().unwrap();
        (a, b)
    })
}

#[test]
fn store_buffering_relaxed_reads_both_old_values() {
    let all = BTreeSet::from([(0, 0), (0, 1), (1, 0), (1, 1)]);
    assert_eq!(store_buffering(Relaxed, Relaxed), all);
    assert_eq!(store_buffering(Release, Acquire), all);
}

#[test]
fn store_buffering_seq_cst_never_reads_both_old_values() {
    assert_eq!(store_buffering(SeqCst, SeqCst), BTreeSet::from([(0, 1), (1, 0), (1, 1)]));
}

#[test]
fn store_buffering_with_seq_cst_fences_never_reads_both_old_values() {
    let seen = outcomes(builder(), || {
        let x = Arc::new(AtomicU32::new(0));
        let y = Arc::new(AtomicU32::new(0));
        let other = {
            let (x, y) = (x.clone(), y.clone());
            thread::spawn(move || {
                y.store(1, Relaxed);
                fence(SeqCst);
                x.load(Relaxed)
            })
        };
        x.store(1, Relaxed);
        fence(SeqCst);
        let a = y.load(Relaxed);
        (a, other.join().unwrap())
    });
    assert!(!seen.contains(&(0, 0)), "{seen:?}");
    assert!(seen.contains(&(1, 1)));
}

fn message_passing(flag_store: Ordering, flag_load: Ordering) -> BTreeSet<(bool, u32)> {
    outcomes(builder(), move || {
        let data = Arc::new(AtomicU32::new(0));
        let flag = Arc::new(AtomicBool::new(false));
        let writer = {
            let (data, flag) = (data.clone(), flag.clone());
            thread::spawn(move || {
                data.store(42, Relaxed);
                flag.store(true, flag_store);
            })
        };
        let seen = flag.load(flag_load);
        let value = data.load(Relaxed);
        writer.join().unwrap();
        (seen, value)
    })
}

#[test]
fn message_passing_release_acquire_publishes() {
    let seen = message_passing(Release, Acquire);
    assert_eq!(seen, BTreeSet::from([(false, 0), (false, 42), (true, 42)]));
}

#[test]
fn message_passing_relaxed_may_read_the_flag_before_the_data() {
    assert!(message_passing(Relaxed, Relaxed).contains(&(true, 0)));
    assert!(message_passing(Release, Relaxed).contains(&(true, 0)));
}

#[test]
fn message_passing_through_fences_publishes() {
    let seen = outcomes(builder(), || {
        let data = Arc::new(AtomicU32::new(0));
        let flag = Arc::new(AtomicBool::new(false));
        let writer = {
            let (data, flag) = (data.clone(), flag.clone());
            thread::spawn(move || {
                data.store(42, Relaxed);
                fence(Release);
                flag.store(true, Relaxed);
            })
        };
        let seen = flag.load(Relaxed);
        fence(Acquire);
        let value = data.load(Relaxed);
        writer.join().unwrap();
        (seen, value)
    });
    assert!(!seen.contains(&(true, 0)), "{seen:?}");
}

#[test]
fn a_release_sequence_goes_on_through_an_rmw() {
    let seen = outcomes(builder(), || {
        let data = Arc::new(AtomicU32::new(0));
        let flag = Arc::new(AtomicU32::new(0));
        let writer = {
            let (data, flag) = (data.clone(), flag.clone());
            thread::spawn(move || {
                data.store(42, Relaxed);
                flag.store(1, Release);
            })
        };
        let bumper = {
            let flag = flag.clone();
            thread::spawn(move || {
                flag.fetch_add(1, Relaxed);
            })
        };
        let seen = flag.load(Acquire);
        let value = data.load(Relaxed);
        writer.join().unwrap();
        bumper.join().unwrap();
        (seen, value)
    });
    // 2 is read only after the release store (1) and the RMW on it: the data is published.
    assert!(!seen.contains(&(2, 0)), "{seen:?}");
    assert!(seen.contains(&(1, 0)) || seen.contains(&(1, 42)));
}

#[test]
fn coherence_never_reads_backwards() {
    let seen = outcomes(builder(), || {
        let x = Arc::new(AtomicU32::new(0));
        let writer = {
            let x = x.clone();
            thread::spawn(move || {
                x.store(1, Relaxed);
                x.store(2, Relaxed);
            })
        };
        let a = x.load(Relaxed);
        let b = x.load(Relaxed);
        writer.join().unwrap();
        (a, b)
    });
    assert!(seen.iter().all(|&(a, b)| a <= b), "{seen:?}");
    assert!(seen.contains(&(0, 2)) && seen.contains(&(1, 1)));
}

#[test]
fn rmws_never_lose_an_update() {
    let seen = outcomes(builder(), || {
        let n = Arc::new(AtomicUsize::new(0));
        let others: Vec<_> = (0..2)
            .map(|_| {
                let n = n.clone();
                thread::spawn(move || {
                    n.fetch_add(1, Relaxed);
                })
            })
            .collect();
        n.fetch_add(1, Relaxed);
        for other in others {
            other.join().unwrap();
        }
        n.load(Relaxed)
    });
    assert_eq!(seen, BTreeSet::from([3]));
}

#[test]
fn a_load_then_store_increment_loses_updates() {
    let seen = outcomes(builder(), || {
        let n = Arc::new(AtomicI32::new(0));
        let other = {
            let n = n.clone();
            thread::spawn(move || {
                let v = n.load(Relaxed);
                n.store(v + 1, Relaxed);
            })
        };
        let v = n.load(Relaxed);
        n.store(v + 1, Relaxed);
        other.join().unwrap();
        n.load(Relaxed)
    });
    assert_eq!(seen, BTreeSet::from([1, 2]));
}

#[test]
fn compare_exchange_elects_one_winner() {
    let seen = outcomes(builder(), || {
        let owner = Arc::new(AtomicU32::new(0));
        let other = {
            let owner = owner.clone();
            thread::spawn(move || owner.compare_exchange(0, 2, AcqRel, Acquire).is_ok())
        };
        let mine = owner.compare_exchange(0, 1, AcqRel, Acquire).is_ok();
        let theirs = other.join().unwrap();
        (mine, theirs, owner.load(Relaxed))
    });
    assert_eq!(seen, BTreeSet::from([(true, false, 1), (false, true, 2)]));
}

/// A cell shared between threads, as a crate's own type shares one: `UnsafeCell` is `!Sync`
/// like std's, so the claim that sharing is safe is the wrapper's, and the model checks it.
struct Shared<T>(UnsafeCell<T>);

#[allow(unsafe_code)]
// SAFETY: every access goes through `with`/`with_mut`, which the model checks for races.
unsafe impl<T: Send> Sync for Shared<T> {}

impl<T> std::ops::Deref for Shared<T> {
    type Target = UnsafeCell<T>;

    fn deref(&self) -> &UnsafeCell<T> {
        &self.0
    }
}

#[test]
fn an_unsynchronized_cell_write_is_a_race() {
    let report = finding(builder(), || {
        let cell = Arc::new(Shared(UnsafeCell::new(0u32)));
        let other = {
            let cell = cell.clone();
            thread::spawn(move || cell.with_mut(|_| ()))
        };
        cell.with(|_| ());
        other.join().unwrap();
    });
    assert!(report.contains("data race"), "{report}");
    assert!(report.contains("CHECK_REPLAY="), "{report}");
}

#[test]
fn a_relaxed_flag_does_not_order_a_cell() {
    let report = finding(builder(), || {
        let cell = Arc::new(Shared(UnsafeCell::new(0u32)));
        let flag = Arc::new(AtomicBool::new(false));
        let writer = {
            let (cell, flag) = (cell.clone(), flag.clone());
            thread::spawn(move || {
                cell.with_mut(|_| ());
                flag.store(true, Relaxed);
            })
        };
        if flag.load(Relaxed) {
            cell.with(|_| ());
        }
        writer.join().unwrap();
    });
    assert!(report.contains("data race"), "{report}");
}

#[test]
fn a_release_acquire_flag_orders_a_cell() {
    builder().check(|| {
        let cell = Arc::new(Shared(UnsafeCell::new(0u32)));
        let flag = Arc::new(AtomicBool::new(false));
        let writer = {
            let (cell, flag) = (cell.clone(), flag.clone());
            thread::spawn(move || {
                cell.with_mut(|_| ());
                flag.store(true, Release);
            })
        };
        if flag.load(Acquire) {
            cell.with(|_| ());
        }
        writer.join().unwrap();
        cell.with(|_| ());
    });
}

#[test]
fn a_mutex_orders_a_cell_and_counts_right() {
    let seen = outcomes(builder(), || {
        let n = Arc::new(Mutex::new(0u32));
        let others: Vec<_> = (0..2)
            .map(|_| {
                let n = n.clone();
                thread::spawn(move || *n.lock().unwrap() += 1)
            })
            .collect();
        *n.lock().unwrap() += 1;
        for other in others {
            other.join().unwrap();
        }
        let n = *n.lock().unwrap();
        n
    });
    assert_eq!(seen, BTreeSet::from([3]));
}

#[test]
fn locks_taken_in_opposite_orders_deadlock() {
    let report = finding(builder(), || {
        let a = Arc::new(Mutex::new(()));
        let b = Arc::new(Mutex::new(()));
        let other = {
            let (a, b) = (a.clone(), b.clone());
            thread::spawn(move || {
                let _b = b.lock().unwrap();
                let _a = a.lock().unwrap();
            })
        };
        {
            let _a = a.lock().unwrap();
            let _b = b.lock().unwrap();
        }
        other.join().unwrap();
    });
    assert!(report.contains("deadlock"), "{report}");
}

#[test]
fn a_rwlock_lets_readers_share_and_orders_writers() {
    let seen = outcomes(builder(), || {
        let lock = Arc::new(RwLock::new(0u32));
        let writer = {
            let lock = lock.clone();
            thread::spawn(move || *lock.write().unwrap() += 1)
        };
        let read = *lock.read().unwrap();
        writer.join().unwrap();
        (read, *lock.read().unwrap())
    });
    assert_eq!(seen, BTreeSet::from([(0, 1), (1, 1)]));
}

#[test]
fn a_condvar_wakes_its_waiter() {
    builder().check(|| {
        let pair = Arc::new((Mutex::new(false), Condvar::new()));
        let notifier = {
            let pair = pair.clone();
            thread::spawn(move || {
                *pair.0.lock().unwrap() = true;
                pair.1.notify_one();
            })
        };
        let ready = pair.1.wait_while(pair.0.lock().unwrap(), |ready| !*ready).unwrap();
        assert!(*ready);
        drop(ready);
        notifier.join().unwrap();
    });
}

#[test]
fn a_lost_wakeup_is_a_deadlock() {
    let report = finding(builder(), || {
        let pair = Arc::new((Mutex::new(()), Condvar::new()));
        let notifier = {
            let pair = pair.clone();
            thread::spawn(move || pair.1.notify_one())
        };
        // Waits without a condition: the notify may come first and be lost.
        let guard = pair.1.wait(pair.0.lock().unwrap()).unwrap();
        drop(guard);
        notifier.join().unwrap();
    });
    assert!(report.contains("deadlock"), "{report}");
}

#[test]
fn the_lot_locks_count_right_and_wake_their_waiter() {
    let seen = outcomes(builder(), || {
        let pair = Arc::new((lot::Mutex::new(0u32), lot::Condvar::new()));
        let others: Vec<_> = (0..2)
            .map(|_| {
                let pair = pair.clone();
                thread::spawn(move || {
                    *pair.0.lock() += 1;
                    pair.1.notify_all();
                })
            })
            .collect();
        let n = {
            let mut n = pair.0.lock();
            pair.1.wait_while(&mut n, |n| *n < 2);
            *n
        };
        for other in others {
            other.join().unwrap();
        }
        n
    });
    assert_eq!(seen, BTreeSet::from([2]));
}

#[test]
fn a_lot_condvar_timed_out_only_while_its_condition_holds() {
    builder().check(|| {
        let pair = Arc::new((lot::Mutex::new(false), lot::Condvar::new()));
        let notifier = {
            let pair = pair.clone();
            thread::spawn(move || {
                *pair.0.lock() = true;
                pair.1.notify_one();
            })
        };
        let mut ready = pair.0.lock();
        let waited = pair.1.wait_while_for(&mut ready, |ready| !*ready, Duration::from_secs(1));
        assert_eq!(waited.timed_out(), !*ready);
        drop(ready);
        notifier.join().unwrap();
    });
}

#[test]
fn a_lost_wakeup_on_a_lot_condvar_is_a_deadlock() {
    let report = finding(builder(), || {
        let pair = Arc::new((lot::Mutex::new(()), lot::Condvar::new()));
        let notifier = {
            let pair = pair.clone();
            thread::spawn(move || pair.1.notify_one())
        };
        let mut guard = pair.0.lock();
        pair.1.wait(&mut guard);
        drop(guard);
        notifier.join().unwrap();
    });
    assert!(report.contains("deadlock"), "{report}");
}

#[test]
fn a_spin_loop_with_a_hint_ends() {
    builder().check(|| {
        let flag = Arc::new(AtomicBool::new(false));
        let setter = {
            let flag = flag.clone();
            thread::spawn(move || flag.store(true, Release))
        };
        while !flag.load(Acquire) {
            crate::hint::spin_loop();
        }
        setter.join().unwrap();
    });
}

#[test]
fn a_spin_loop_without_a_yield_is_a_finding() {
    let mut builder = builder();
    builder.max_branches = 200;
    let report = finding(builder, || {
        let flag = Arc::new(AtomicBool::new(false));
        let setter = {
            let flag = flag.clone();
            thread::spawn(move || flag.store(true, Release))
        };
        while !flag.load(Acquire) {}
        setter.join().unwrap();
    });
    assert!(report.contains("branches"), "{report}");
}

#[test]
fn park_and_unpark_hand_over_a_token() {
    builder().check(|| {
        let flag = Arc::new(AtomicBool::new(false));
        let main = thread::current();
        let waker = {
            let flag = flag.clone();
            thread::spawn(move || {
                flag.store(true, Release);
                main.unpark();
            })
        };
        while !flag.load(Acquire) {
            thread::park();
        }
        waker.join().unwrap();
    });
}

#[test]
fn a_once_lock_runs_its_initializer_once() {
    let seen = outcomes(builder(), || {
        let calls = Arc::new(AtomicU32::new(0));
        let cell = Arc::new(OnceLock::new());
        let other = {
            let (calls, cell) = (calls.clone(), cell.clone());
            thread::spawn(move || {
                *cell.get_or_init(|| {
                    calls.fetch_add(1, Relaxed);
                    2u32
                })
            })
        };
        let mine = *cell.get_or_init(|| {
            calls.fetch_add(1, Relaxed);
            1u32
        });
        let theirs = other.join().unwrap();
        assert_eq!(mine, theirs);
        (mine, calls.load(Relaxed))
    });
    assert_eq!(seen, BTreeSet::from([(1, 1), (2, 1)]));
}

crate::thread_local! {
    static COUNT: StdCell<u32> = const { StdCell::new(0) };
}

#[test]
fn each_model_thread_has_its_own_thread_local() {
    builder().check(|| {
        COUNT.set(COUNT.get() + 1);
        let other = thread::spawn(|| {
            COUNT.set(COUNT.get() + 10);
            COUNT.get()
        });
        assert_eq!(other.join().unwrap(), 10);
        assert_eq!(COUNT.get(), 1);
    });
}

#[test]
fn an_atomic_ptr_publishes_what_it_points_to() {
    builder().check(|| {
        let slot = Arc::new(AtomicPtr::<u64>::new(std::ptr::null_mut()));
        let writer = {
            let slot = slot.clone();
            thread::spawn(move || {
                let boxed = Box::into_raw(Box::new(7u64));
                slot.store(boxed, Release);
            })
        };
        writer.join().unwrap();
        let ptr = slot.swap(std::ptr::null_mut(), Acquire);
        assert!(!ptr.is_null());
        #[allow(unsafe_code)]
        // SAFETY: `ptr` is the writer's `Box::into_raw`, which the swap took out of the slot:
        // nothing else holds it.
        let boxed = unsafe { Box::from_raw(ptr) };
        assert_eq!(*boxed, 7);
    });
}

#[test]
fn statics_start_again_in_each_execution() {
    let seen = outcomes(builder(), || {
        static N: AtomicU32 = AtomicU32::new(5);
        let other = thread::spawn(|| {
            N.fetch_add(1, Relaxed);
        });
        let first = N.fetch_add(1, Relaxed);
        other.join().unwrap();
        first
    });
    assert_eq!(seen, BTreeSet::from([5, 6]));
}

#[test]
fn a_finding_replays_from_its_schedule() {
    fn racy() {
        let cell = Arc::new(Shared(UnsafeCell::new(0u32)));
        let flag = Arc::new(AtomicBool::new(false));
        let writer = {
            let (cell, flag) = (cell.clone(), flag.clone());
            thread::spawn(move || {
                cell.with_mut(|_| ());
                flag.store(true, Relaxed);
            })
        };
        if flag.load(Relaxed) {
            cell.with(|_| ());
        }
        writer.join().unwrap();
    }
    let report = finding(builder(), racy);
    let schedule = report
        .split("CHECK_REPLAY=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("the report names the schedule")
        .to_owned();
    let mut replay = builder();
    replay.replay = Some(schedule.clone());
    let again = finding(replay, racy);
    assert!(again.contains("data race"), "{again}");
    assert!(again.contains(&format!("schedule {schedule}")), "{again}");
}

#[test]
fn random_runs_find_the_lost_update() {
    let mut random = builder();
    random.random = Some(200);
    random.seed = 7;
    let report = finding(random, || {
        let n = Arc::new(AtomicU32::new(0));
        let other = {
            let n = n.clone();
            thread::spawn(move || {
                let v = n.load(Relaxed);
                n.store(v + 1, Relaxed);
            })
        };
        let v = n.load(Relaxed);
        n.store(v + 1, Relaxed);
        other.join().unwrap();
        assert_eq!(n.load(Relaxed), 2);
    });
    assert!(report.contains("random search"), "{report}");
    assert!(report.contains("panicked"), "{report}");
}

#[test]
fn model_runs_with_the_environments_bounds() {
    model(|| {
        let n = Arc::new(AtomicU32::new(0));
        let other = {
            let n = n.clone();
            thread::spawn(move || n.fetch_add(1, Relaxed))
        };
        n.fetch_add(1, Relaxed);
        other.join().unwrap();
        assert_eq!(n.load(Relaxed), 2);
    });
}

#[test]
fn outside_a_model_the_shims_are_std() {
    let n = AtomicU32::new(1);
    assert_eq!(n.fetch_add(2, SeqCst), 1);
    assert_eq!(n.compare_exchange(3, 4, SeqCst, SeqCst), Ok(3));
    let m = Mutex::new(5);
    *m.lock().unwrap() += 1;
    assert_eq!(*m.lock().unwrap(), 6);
    let cell = OnceLock::new();
    assert_eq!(*cell.get_or_init(|| 9), 9);
    assert_eq!(thread::spawn(|| 3).join().unwrap(), 3);
    COUNT.set(4);
    assert_eq!(COUNT.get(), 4);
}

/// A stack behind a mutex, and its sequential model.
#[derive(Clone, PartialEq, Eq, Hash, Default)]
struct Stack(Vec<u32>);

#[derive(Debug)]
enum StackOp {
    Push(u32),
    Pop,
}

impl Spec for Stack {
    type Op = StackOp;
    type Ret = Option<u32>;

    fn apply(&mut self, op: &StackOp) -> Option<u32> {
        match op {
            StackOp::Push(value) => {
                self.0.push(*value);
                None
            }
            StackOp::Pop => self.0.pop(),
        }
    }
}

#[test]
fn a_locked_stack_is_linearizable() {
    builder().check(|| {
        let stack = Arc::new(Mutex::new(Vec::new()));
        let history = Arc::new(History::<Stack>::new());
        let other = {
            let (stack, history) = (stack.clone(), history.clone());
            thread::spawn(move || {
                history.record(StackOp::Push(1), |_| {
                    stack.lock().unwrap().push(1);
                    None
                });
                history.record(StackOp::Pop, |_| stack.lock().unwrap().pop());
            })
        };
        history.record(StackOp::Push(2), |_| {
            stack.lock().unwrap().push(2);
            None
        });
        history.record(StackOp::Pop, |_| stack.lock().unwrap().pop());
        other.join().unwrap();
        history.check(Stack::default());
    });
}

/// A counter and its sequential model, for a broken implementation.
#[derive(Clone, PartialEq, Eq, Hash, Default)]
struct Counter(u32);

impl Spec for Counter {
    type Op = ();
    type Ret = u32;

    fn apply(&mut self, (): &()) -> u32 {
        self.0 += 1;
        self.0
    }
}

#[test]
fn a_racy_counter_is_not_linearizable() {
    let report = finding(builder(), || {
        let n = Arc::new(AtomicU32::new(0));
        let history = Arc::new(History::<Counter>::new());
        let bump = |n: &AtomicU32| {
            let v = n.load(Relaxed) + 1;
            n.store(v, Relaxed);
            v
        };
        let other = {
            let (n, history) = (n.clone(), history.clone());
            thread::spawn(move || history.record((), |()| bump(&n)))
        };
        history.record((), |()| bump(&n));
        other.join().unwrap();
        history.check(Counter::default());
    });
    assert!(report.contains("not linearizable"), "{report}");
}

/// A register and its sequential model.
#[derive(Clone, PartialEq, Eq, Hash, Default)]
struct Register(u32);

#[derive(Debug)]
enum RegisterOp {
    Set(u32),
    Get,
}

impl Spec for Register {
    type Op = RegisterOp;
    type Ret = Option<u32>;

    fn apply(&mut self, op: &RegisterOp) -> Option<u32> {
        match *op {
            RegisterOp::Set(value) => {
                self.0 = value;
                None
            }
            RegisterOp::Get => Some(self.0),
        }
    }
}

/// A relaxed register: a get run after a set returned, on a thread that never synchronized with
/// it, may read the old value (both orders of the two are allowed), so no execution is reported.
#[test]
fn a_history_orders_operations_by_happens_before_not_by_execution() {
    let reads = outcomes(builder(), || {
        let x = Arc::new(AtomicU32::new(0));
        let history = Arc::new(History::<Register>::new());
        let other = {
            let (x, history) = (x.clone(), history.clone());
            thread::spawn(move || {
                history.record(RegisterOp::Set(1), |_| {
                    x.store(1, Relaxed);
                    None
                })
            })
        };
        let read = history.record(RegisterOp::Get, |_| Some(x.load(Relaxed)));
        other.join().unwrap();
        history.check(Register::default());
        read
    });
    assert_eq!(reads, BTreeSet::from([Some(0), Some(1)]));
}

/// A set ordered before a get through a release and an acquire outside both operations must be
/// seen by it: a register whose set writes elsewhere is a finding.
#[test]
fn a_history_orders_operations_across_threads_through_synchronization() {
    let report = finding(builder(), || {
        let (x, lost) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
        let done = Arc::new(AtomicBool::new(false));
        let history = Arc::new(History::<Register>::new());
        let other = {
            let (lost, done, history) = (lost.clone(), done.clone(), history.clone());
            thread::spawn(move || {
                history.record(RegisterOp::Set(1), |_| {
                    lost.store(1, Relaxed);
                    None
                });
                done.store(true, Release);
            })
        };
        if done.load(Acquire) {
            history.record(RegisterOp::Get, |_| Some(x.load(Relaxed)));
        }
        other.join().unwrap();
        history.check(Register::default());
    });
    assert!(report.contains("not linearizable"), "{report}");
}

#[test]
fn a_history_outside_a_model_checks_too() {
    let history = History::<Counter>::new();
    let mut n = 0;
    for _ in 0..3 {
        history.record((), |()| {
            n += 1;
            n
        });
    }
    history.check(Counter::default());
    history.clear();
    history.record((), |()| 5);
    assert!(catch_unwind(AssertUnwindSafe(|| history.check(Counter::default()))).is_err());
}

/// A buffer's address for the tracked-bytes tests: only its address matters, and it lives
/// across the model's executions.
fn buffer() -> *const u8 {
    static BUFFER: [u8; 64] = [0; 64];
    BUFFER.as_ptr()
}

#[test]
fn tracked_bytes_published_with_release_acquire_do_not_race() {
    builder().check(|| {
        let published = Arc::new(AtomicUsize::new(0));
        let writer = {
            let published = published.clone();
            thread::spawn(move || {
                crate::cell::write(buffer(), 16);
                published.store(16, Release);
                crate::cell::write(buffer().wrapping_add(16), 16);
                published.store(32, Release);
            })
        };
        let len = published.load(Acquire);
        crate::cell::read(buffer(), len);
        writer.join().unwrap();
    });
}

#[test]
fn tracked_bytes_read_past_what_was_published_race() {
    let report = finding(builder(), || {
        let published = Arc::new(AtomicUsize::new(0));
        let writer = {
            let published = published.clone();
            thread::spawn(move || {
                crate::cell::write(buffer(), 10);
                published.store(10, Release);
                crate::cell::write(buffer().wrapping_add(10), 6);
            })
        };
        if published.load(Acquire) == 10 {
            crate::cell::read(buffer(), 11);
        }
        writer.join().unwrap();
    });
    assert!(report.contains("data race on the bytes"), "{report}");
}

#[test]
fn tracked_bytes_are_exact_to_the_byte() {
    builder().check(|| {
        crate::cell::write(buffer(), 13);
        let writer = thread::spawn(|| crate::cell::write(buffer().wrapping_add(13), 3));
        crate::cell::read(buffer(), 13);
        writer.join().unwrap();
    });
}

#[test]
fn a_free_not_ordered_after_a_read_is_a_use_after_reclaim() {
    let report = finding(builder(), || {
        crate::cell::write(buffer(), 8);
        let reader = thread::spawn(|| crate::cell::read(buffer(), 8));
        crate::cell::free(buffer(), 8);
        reader.join().unwrap();
    });
    assert!(report.contains("data race") || report.contains("use after free"), "{report}");
}

#[test]
fn freed_bytes_can_not_be_read_but_can_be_allocated_again() {
    builder().check(|| {
        crate::cell::write(buffer(), 8);
        crate::cell::free(buffer(), 8);
        crate::cell::write(buffer(), 4);
        crate::cell::read(buffer(), 4);
    });
    let report = finding(builder(), || {
        crate::cell::write(buffer(), 8);
        crate::cell::free(buffer(), 8);
        crate::cell::read(buffer().wrapping_add(4), 2);
    });
    assert!(report.contains("use after free"), "{report}");
    let report = finding(builder(), || {
        crate::cell::write(buffer(), 8);
        crate::cell::free(buffer(), 8);
        crate::cell::free(buffer(), 8);
    });
    assert!(report.contains("double free"), "{report}");
}

#[test]
fn the_generator_is_splitmix64() {
    let mut rng = crate::rng::Rng::new(0);
    assert_eq!(rng.next_u64(), 0xE220_A839_7B1D_CDAF);
    assert_eq!(rng.next_u64(), 0x6E78_9E6A_A1B9_65F4);
    assert_eq!(rng.next_u64(), 0x06C4_5D18_8009_454F);
}

#[test]
fn a_schedule_reads_back_as_it_was_written() {
    let path = crate::path::Path::replay("01a0{30}+-.", 100).unwrap_or_else(|why| panic!("{why}"));
    assert_eq!(path.len(), 8);
    assert!(crate::path::Path::replay("01#", 100).is_err());
    assert!(crate::path::Path::replay("01{x}", 100).is_err());
}

#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

/// One schedule point and the operation after it, on a runtime driven directly (thread 0, no
/// coroutines), as `rt::op` does them.
fn point(rt: &mut Runtime, obj: u32, kind: Kind) {
    rt.threads[0].op = Some(Op { obj, kind });
    assert_eq!(rt.schedule(), Ok(false));
    rt.tick();
}

/// An execution of every kind of runtime operation: atomic stores, loads, RMWs, an SC fence, a
/// mutex, a cell and tracked bytes; then the reset to the next execution.
fn one_execution(rt: &mut Runtime, ids: &[Id; 3]) {
    let at = Location::caller();
    rt.begin();
    let x = rt.atomic(&ids[0], 0);
    let lock = rt.mutex(&ids[1]);
    let cell = rt.object(&ids[2], |_| Object::Cell(memory::Cell::new()));
    for round in 0..50 {
        point(rt, x, Kind::Store);
        assert!(rt.store(x, round, Release, at, 0).is_ok());
        point(rt, x, Kind::Load);
        assert!(rt.load(x, Acquire, at, 0).is_ok());
        point(rt, x, Kind::Rmw);
        assert!(rt.rmw(x, AcqRel, Acquire, at, 0, |old| Some(old + 1)).is_ok());
        point(rt, FENCE, Kind::Rmw);
        rt.fence(SeqCst);
        point(rt, lock, Kind::Write);
        assert!(rt.mutex_take(lock));
        rt.mutex_release(lock);
        rt.threads[0].op = None;
        assert!(rt.cell_write(cell, at).is_ok());
        assert!(rt.cell_read(cell, at).is_ok());
        assert!(rt.bytes_access(0x2000, 16, at, memory::Bytes::Write).is_ok());
        assert!(rt.bytes_access(0x2004, 8, at, memory::Bytes::Read).is_ok());
    }
    rt.end();
    assert!(!rt.path.step(), "one thread leaves no other execution");
}

/// Once the first execution has sized the runtime's tables, an execution and the reset after
/// it allocate nothing: no schedule point, store, lock, cell or byte access, and no reset.
#[test]
fn an_execution_and_its_reset_allocate_nothing_when_warm() {
    assert!(blaze_alloc_count::installed());
    let ids: [Id; 3] = std::array::from_fn(|_| Id::new(0));
    let mut rt = Runtime::new(Path::new(Mode::Explore, Some(3), 10_000), 2);
    one_execution(&mut rt, &ids);
    let ((), used) = blaze_alloc_count::measure(|| one_execution(&mut rt, &ids));
    assert_eq!(used.allocations, 0, "a warm execution allocated {} bytes", used.bytes);
}

/// The same inside a model, through the shims and the coroutine: once warm, a round of every
/// shim operation allocates nothing.
#[test]
fn a_warm_scheduling_point_allocates_nothing() {
    assert!(blaze_alloc_count::installed());
    let mut builder = builder();
    builder.max_branches = 10_000;
    builder.check(|| {
        static LOCK: Mutex<u32> = Mutex::new(0);
        static RW: RwLock<u32> = RwLock::new(0);
        static CONDVAR: Condvar = Condvar::new();
        let x = AtomicU32::new(0);
        let cell = UnsafeCell::new(0u32);
        let once = OnceLock::new();
        let round = || {
            x.fetch_add(1, AcqRel);
            x.store(x.load(Acquire) + 1, Release);
            fence(SeqCst);
            *LOCK.lock().unwrap() += 1;
            *RW.write().unwrap() += 1;
            assert!(*RW.read().unwrap() > 0);
            CONDVAR.notify_one();
            cell.with_mut(|_| ());
            cell.with(|_| ());
            assert_eq!(*once.get_or_init(|| 7), 7);
            crate::cell::write(buffer(), 16);
            crate::cell::read(buffer(), 8);
            thread::yield_now();
        };
        for _ in 0..200 {
            round();
        }
        let ((), used) = blaze_alloc_count::measure(|| {
            for _ in 0..100 {
                round();
            }
        });
        assert_eq!(used.allocations, 0, "100 warm rounds allocated {} bytes", used.bytes);
    });
}

/// The allocations of a random search of `runs` executions of one model.
fn allocations_of(runs: u64) -> u64 {
    let mut random = builder();
    random.random = Some(runs);
    random.seed = 3;
    random.max_branches = 10_000;
    let ((), used) = blaze_alloc_count::measure(|| {
        random.check(|| {
            static LOCK: Mutex<u32> = Mutex::new(0);
            static FLAG: AtomicBool = AtomicBool::new(false);
            let x = AtomicU32::new(0);
            let cell = UnsafeCell::new(0u32);
            for _ in 0..20 {
                x.fetch_add(1, AcqRel);
                FLAG.store(x.load(Acquire) % 2 == 0, Release);
                *LOCK.lock().unwrap() += 1;
                cell.with_mut(|_| ());
                crate::cell::write(buffer(), 16);
                crate::cell::read(buffer(), 8);
            }
        });
    });
    used.allocations
}

/// Executions after the first allocate nothing: the explorer's own work between them (the
/// path's buffers, the main job, the runtime's tables) is reused.
#[test]
fn executions_after_the_first_allocate_nothing() {
    assert!(blaze_alloc_count::installed());
    allocations_of(2);
    assert_eq!(allocations_of(50), allocations_of(2));
}

/// Every execution's order of steps, in the order the search ran them.
fn interleavings() -> Vec<Vec<u8>> {
    let log = StdArc::new(StdMutex::new(Vec::new()));
    let runs = StdArc::new(StdMutex::new(Vec::new()));
    let (into_log, into_runs) = (log.clone(), runs.clone());
    let mut builder = builder();
    builder.max_preemptions = Some(2);
    builder.check(move || {
        into_log.lock().clear();
        let lock = Arc::new(Mutex::new(0u32));
        let x = Arc::new(AtomicU32::new(0));
        let workers: Vec<_> = (1..3u8)
            .map(|me| {
                let (log, lock, x) = (into_log.clone(), lock.clone(), x.clone());
                thread::spawn(move || {
                    for _ in 0..2 {
                        log.lock().push(me);
                        x.fetch_add(1, AcqRel);
                        log.lock().push(me);
                        *lock.lock().unwrap() += 1;
                        thread::yield_now();
                    }
                })
            })
            .collect();
        into_log.lock().push(0);
        x.load(Acquire);
        into_log.lock().push(0);
        for worker in workers {
            worker.join().unwrap();
        }
        let trace = into_log.lock().clone();
        into_runs.lock().push(trace);
    });
    let runs = runs.lock().clone();
    runs
}

/// Threads that switch straight to each other run the same executions, in the same order, as
/// when every switch goes through the explorer (the reference for `coro::switch_to`).
#[test]
fn threads_switching_to_each_other_explore_what_the_explorer_does() {
    let direct = interleavings();
    crate::coro::DIRECT.set(false);
    let through = interleavings();
    crate::coro::DIRECT.set(true);
    assert!(direct.len() > 1, "the model has more than one execution");
    assert_eq!(direct, through);
}
