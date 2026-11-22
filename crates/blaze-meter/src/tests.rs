//! The meter's own tests: two threads' counters do not bleed into each other, a stretch's
//! counts are exactly the adds made in it, and the line says what it should. The allocation
//! counter through the real global allocator is blaze-mem's `tests/meter.rs`.

use super::*;

/// Two threads add at once, each its own known amounts, each between its own two snapshots:
/// each sees exactly its own, and a difference of two threads' snapshots is refused.
#[test]
fn two_threads_counters_do_not_bleed() {
    let start = std::sync::Barrier::new(2);
    let (a, b) = std::thread::scope(|scope| {
        let one = |by: u64, calls: u64| {
            let start = &start;
            move || {
                let before = Meter::now();
                start.wait();
                for _ in 0..1000 {
                    add(Count::StoreGets, by);
                    heap_allocated(by as usize);
                }
                for _ in 0..calls {
                    call(Call::Stat);
                }
                let after = Meter::now();
                (after.since(&before).expect("one thread's pair"), before)
            }
        };
        let a = scope.spawn(one(3, 7));
        let b = scope.spawn(one(5, 11));
        (a.join().unwrap(), b.join().unwrap())
    });
    assert_eq!(a.0.count(Count::StoreGets), 3000);
    assert_eq!(b.0.count(Count::StoreGets), 5000);
    assert_eq!((a.0.allocations(), a.0.count(Count::HeapBytes)), (Ok(1000), 3000));
    assert_eq!((b.0.allocations(), b.0.count(Count::HeapBytes)), (Ok(1000), 5000));
    assert_eq!(a.0.calls(Call::Stat).0, 7);
    assert_eq!(b.0.calls(Call::Stat).0, 11);
    // CONTROL: the same adds on one thread are seen together, so a pass above is not counters
    // that see nothing; and two threads' snapshots are not subtracted.
    let before = Meter::now();
    add(Count::StoreGets, 3000);
    add(Count::StoreGets, 5000);
    assert_eq!(Meter::now().since(&before).expect("one thread's pair").count(Count::StoreGets), 8000);
    assert_eq!(Meter::now().since(&a.1), Err(Unpaired::OtherThread));
}

/// The size classes: a stretch's allocation count is their sum and its largest is bounded by
/// the top of the highest class touched.
#[test]
fn size_classes_bound_the_largest() {
    let before = Meter::now();
    for bytes in [0, 1, 8, 100, 4096, 2 << 20] {
        heap_allocated(bytes);
    }
    let used = Meter::now().since(&before).expect("one thread's pair");
    assert_eq!(used.allocations(), Ok(6));
    assert_eq!(used.count(Count::HeapBytes), 1 + 8 + 100 + 4096 + (2 << 20));
    assert_eq!(used.largest_under(), Some(4 << 20), "2 MiB is in [2^21, 2^22)");
    let empty = Meter::now();
    assert_eq!(Meter::now().since(&empty).expect("one thread's pair").largest_under(), None);
}

#[test]
fn the_line_says_only_what_is_not_zero() {
    let mut used = Used::ZERO;
    used.cells.counts[Count::StoreGets as usize] = 96_285;
    used.cells.counts[Count::StoreRead as usize] = 1_003_422_144;
    used.cells.counts[Count::StoreKeys as usize] = 1_204;
    used.cells.calls[Call::Stat as usize] = 17;
    used.cells.sizes[5] = 412;
    used.cells.counts[Count::HeapBytes as usize] = 9_800_000;
    used.system.instructions = Reading::Got(1_210_000_000);
    used.system.minor_faults = Reading::Unavailable(Why::NotPerThread);
    let line = std::format!("{used}");
    assert_eq!(
        line,
        "used: 1.2 G instr, 412 allocs 9.8 MB (most under 64 B), faults unavailable (not kept per thread here), 96,285 store gets 1.0 GB (1,204 keys), 17 stat, 0 waits"
    );
}

#[test]
fn a_spread_keeps_the_sum_the_most_and_the_mean() {
    let mut spread = Spread::NONE;
    for gets in [2, 10, 6] {
        let mut used = Used::ZERO;
        used.cells.counts[Count::StoreGets as usize] = gets;
        used.system.minor_faults = Reading::Unavailable(Why::NotPerThread);
        spread.take(&used);
    }
    assert_eq!((spread.n, spread.sum.count(Count::StoreGets), spread.most.count(Count::StoreGets)), (3, 18, 10));
    assert_eq!(spread.mean().count(Count::StoreGets), 6);
    assert_eq!(spread.sum.system.minor_faults, Reading::Unavailable(Why::NotPerThread), "an unavailable reading stays so in a sum");
}

/// The system's readings of this thread: on macOS and Linux each is a number or a named
/// unavailability, and the instructions of a million adds are seen where they are given.
#[test]
fn this_threads_instructions_are_read_or_named() {
    let before = Meter::now();
    let mut spin = 0u64;
    for at in 0..1_000_000u64 {
        spin = core::hint::black_box(spin.wrapping_add(at));
    }
    let used = Meter::now().since(&before).expect("one thread's pair");
    match used.system.instructions {
        Reading::Got(n) => assert!(n >= 1_000_000, "{n}"),
        Reading::Unavailable(why) => std::println!("instructions unavailable here: {}", why.says()),
    }
    #[cfg(target_os = "macos")]
    assert_eq!(used.system.minor_faults, Reading::Unavailable(Why::NotPerThread));
    #[cfg(target_os = "linux")]
    assert!(used.system.minor_faults.got().is_some());
    let process = Process::now();
    assert!(process.system.user_ns.got().is_some_and(|ns| ns > 0));
}

/// A worker's tally is seen by a snapshot taken with it and only then: its adds are exactly
/// the stretch's, beside the thread's own.
#[test]
fn a_tally_is_counted_with_its_snapshot() {
    let mut tally = Tally::ZERO;
    let before = Meter::now_with(&tally);
    tally.call(Call::Stat);
    tally.call_took(Call::Stat, 40);
    tally.add(Count::StoreGets, 9);
    add(Count::StoreGets, 1);
    let used = Meter::now_with(&tally).since(&before).expect("one tally's pair");
    assert_eq!(used.calls(Call::Stat), (1, 40));
    assert_eq!(used.count(Count::StoreGets), 10);
    // CONTROL: a snapshot without the tally sees only the thread's add, and a thread-only
    // snapshot is never paired with a tally one (two different sums).
    let zero = Tally::ZERO;
    let with = Meter::now_with(&zero);
    assert_eq!(Meter::now().since(&with), Err(Unpaired::OtherTally));
    let before = Meter::now();
    add(Count::StoreGets, 1);
    tally.add(Count::StoreGets, 5);
    assert_eq!(Meter::now().since(&before).expect("one thread's pair").count(Count::StoreGets), 1);
}

/// A tally's waits: the longest names kept, every nanosecond in the total across evictions,
/// and a stretch's difference exact in total and in a row that stayed.
#[test]
fn waits_keep_the_longest_and_lose_nothing() {
    const NAMES: [&str; 12] = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l"];
    let mut tally = Tally::ZERO;
    let before = Meter::now_with(&tally);
    let mut sum = 0;
    for (at, name) in NAMES.iter().enumerate() {
        let ns = (at as u64 + 1) * 1000;
        tally.waited(name, None, ns);
        sum += ns;
    }
    tally.waited("l", None, 5);
    sum += 5;
    let waits = tally.waits();
    assert_eq!(waits.total(), (13, sum));
    assert_eq!(waits.longest().map(|row| (row.on, row.ns)), Some(("l", 12_005)));
    // The eight longest are named; the four shortest went to `other`.
    assert_eq!(waits.other_ns, 1000 + 2000 + 3000 + 4000);
    let used = Meter::now_with(&tally).since(&before).expect("one tally's pair");
    assert_eq!(used.waited.total(), (13, sum));
    let mid = Meter::now_with(&tally);
    tally.waited("l", None, 7);
    tally.waited("m", Some(3), 1);
    let stretch = Meter::now_with(&tally).since(&mid).expect("one tally's pair").waited;
    assert_eq!(stretch.total(), (2, 8));
    assert_eq!(stretch.rows.iter().flatten().find(|row| row.on == "l").map(|row| row.ns), Some(7));
}

/// The lock table loses nothing to eviction, keeps the holder that caused most, reconciles with
/// the meter's lock-wait counts in a stretch, and the `locks:` report says UNACCOUNTED when a
/// lock wait bypassed the table.
#[test]
fn lock_waits_reconcile_and_name_their_holder() {
    let before = Meter::now();
    let holder = |doing, hold_ns| Some(LockHolder { doing, hold_ns, whole: true });
    lock_waited("store.head", Behind::Holder, 30, holder("merge", 50));
    lock_waited("store.head", Behind::Holder, 10, holder("parse", 90));
    lock_waited("store.head", Behind::Holder, 25, holder("merge", 20));
    lock_waited("sched.ready", Behind::WriteBehindRead, 7, None);
    for at in 0..(LOCK_ROWS as u64 + 4) {
        lock_waited(["l0", "l1", "l2", "l3", "l4", "l5", "l6", "l7", "l8", "l9", "la", "lb", "lc", "ld", "le", "lf", "lg", "lh", "li", "lj"][at as usize], Behind::ReadBehindWrite, 1, None);
    }
    let used = Meter::now().since(&before).expect("one thread's pair");
    assert!(used.locks_reconcile(), "{:?}", used.locks);
    assert_eq!(used.locks.total(), (4 + LOCK_ROWS as u64 + 4, 30 + 10 + 25 + 7 + LOCK_ROWS as u64 + 4));
    let head = used.locks.by_wait()[0].expect("a row");
    assert_eq!((head.lock, head.contended, head.ns, head.longest_ns), ("store.head", 3, 65, 30));
    let top = head.top.expect("a holder");
    assert_eq!((top.doing, top.caused_ns, top.longest_hold_ns), ("merge", 55, 50));
    let ready = used.locks.rows.iter().flatten().find(|row| row.lock == "sched.ready").expect("its row");
    assert_eq!((ready.writes_behind_read, ready.unseen_holder), (1, 1));
    let report = format!("{}", LocksReport { used: &used, worker_ns: Some(1_000_000) });
    assert!(report.starts_with("locks: "), "{report}");
    assert!(report.contains("store.head") && report.contains("`merge`"), "{report}");
    assert!(!report.contains("UNACCOUNTED"), "{report}");
    // A lock wait counted by the meter alone (the control): the report says so.
    let mut bypassed = used;
    bypassed.cells.counts[Count::LockWaits as usize] += 1;
    assert!(!bypassed.locks_reconcile());
    assert!(format!("{}", LocksReport { used: &bypassed, worker_ns: None }).contains("UNACCOUNTED"));
}

/// 2026-11-20's overflow, its shape: one thread allocates, another frees it, each between its
/// own two snapshots, while the tests' switch is thrown across a third stretch. Each thread's
/// stretch is exact, the freeing one's kept heap is negative (signed, never saturated to 0),
/// and the pairs that are not one owner's are refused by name, never subtracted.
#[test]
fn a_thread_frees_what_another_allocated_and_mixed_pairs_are_refused() {
    const BLOCKS: u64 = 500;
    const SIZE: usize = 48;
    let (handed, taken) = std::sync::mpsc::sync_channel::<u64>(1);
    let (a, b) = std::thread::scope(|scope| {
        let a = scope.spawn(move || {
            let before = Meter::now();
            for _ in 0..BLOCKS {
                heap_allocated(SIZE);
            }
            let after = Meter::now();
            handed.send(BLOCKS).expect("the freeing thread waits");
            (after.since(&before), before)
        });
        let b = scope.spawn(move || {
            let blocks = taken.recv().expect("the allocating thread sends");
            let before = Meter::now();
            for _ in 0..blocks {
                heap_freed(SIZE);
            }
            Meter::now().since(&before)
        });
        (a.join().expect("thread a"), b.join().expect("thread b"))
    });
    let (allocated, a_before) = (a.0.expect("thread a's own pair"), a.1);
    let freed = b.expect("thread b's own pair");
    let bytes = BLOCKS * SIZE as u64;
    assert_eq!((allocated.allocations(), allocated.count(Count::HeapBytes), allocated.count(Count::HeapFrees)), (Ok(BLOCKS), bytes, 0));
    assert_eq!((freed.allocations(), freed.count(Count::HeapFreedBytes), freed.count(Count::HeapFrees)), (Ok(0), bytes, BLOCKS));
    assert_eq!(freed.heap_kept(), -i128::from(bytes), "a window that frees more than it allocates keeps a negative heap");
    assert_eq!(allocated.plus(&freed).heap_kept(), 0, "the two windows together keep nothing");
    assert_eq!(allocated.plus(&freed).overflowed, None);
    // A thread's snapshot paired with another's (its serial is not this thread's: a serial is
    // never given twice, where an exited thread's cell address was given to the next thread).
    let here = Meter::now();
    assert_ne!(here.whose().thread, a_before.whose().thread);
    assert_eq!(here.since(&a_before), Err(Unpaired::OtherThread));

    // The pair that overflowed: a real snapshot of a thread that has allocated in two size
    // classes, and one of the same thread taken after the tests' switch was thrown (zero cells,
    // the next epoch). Built by hand so no other test running now sees the switch move.
    heap_allocated(8);
    heap_allocated(4096);
    let real = Meter::now();
    let stopped = Meter {
        whose: Whose { epoch: real.whose().epoch + 1, ..real.whose() },
        cells: Cells::ZERO,
        system: System::none(Why::Stopped),
        waited: Waited::ZERO,
        locks: Locks::ZERO,
    };
    assert_eq!(stopped.since(&real), Err(Unpaired::AcrossStop));
    // PLANTED (rule 23): the old subtraction (no epoch, cells wrapped) of the same pair gives
    // the panic's numbers: each touched class wraps to near 2^64, and their sum passes u64.
    let wrapped: std::vec::Vec<u64> = stopped.cells.sizes.iter().zip(real.cells.sizes.iter()).map(|(now, then)| now.wrapping_sub(*then)).collect();
    assert!(wrapped.iter().filter(|class| **class > u64::MAX / 2).count() >= 2, "{wrapped:?}");
    assert_eq!(wrapped.iter().try_fold(0u64, |sum, class| sum.checked_add(*class)), None, "control: the old pair overflows the sum");
    // And the epoch is the only thing refusing it: the same pair in one epoch is a cell that
    // went down, named.
    let same_epoch = Meter { whose: real.whose(), ..stopped };
    match same_epoch.since(&real) {
        Err(Unpaired::Backwards { field, before, now: 0 }) => {
            assert!(before > 0);
            assert!(matches!(field.group, Group::Counts | Group::Sizes), "{field}");
        }
        other => panic!("a cell that went down was not refused: {other:?}"),
    }
}

/// Sums of [`Used`] never wrap: a [`Used::plus`] past `u64` saturates the cell and names it,
/// and the summed readers say which add passed `u64`; the line prints both.
#[test]
fn sums_past_u64_are_named_not_wrapped() {
    let mut big = Used::ZERO;
    big.cells.sizes[3] = u64::MAX - 1;
    let mut more = Used::ZERO;
    more.cells.sizes[3] = 5;
    more.cells.sizes[4] = 1;
    let sum = big.plus(&more);
    assert_eq!(sum.cells.sizes[3], u64::MAX);
    assert_eq!(sum.overflowed, Some(Field { group: Group::Sizes, index: 3 }));
    assert_eq!(sum.allocations(), Err(Overflow { sum: "allocations (the size classes' sum)", at: Field { group: Group::Sizes, index: 4 } }));
    let line = std::format!("{sum}");
    assert!(line.contains("OVERFLOWED sizes[3]") && line.contains("allocations (the size classes' sum) passed u64 at sizes[4]"), "{line}");
    // CONTROL: the same adds under u64 are exact and say nothing of an overflow.
    let mut small = Used::ZERO;
    small.cells.sizes[3] = 7;
    let fine = small.plus(&more);
    assert_eq!((fine.allocations(), fine.overflowed), (Ok(13), None));
    assert!(!std::format!("{fine}").contains("OVERFLOWED"));
}

/// Many tables' rows gathered: the kept set, its order and `other` are a function of the
/// multiset (every rotation and the reverse of the rows give one table), the eight largest by
/// time are kept with ties by name, and nothing is lost. The control: `plus` in two orders
/// keeps different rows from the same tables, so the test can tell.
#[test]
fn gathering_waits_is_a_function_of_the_multiset_not_the_order() {
    const NAMES: [&str; 12] = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l"];
    // Two workers' tables, neither past eight: the first saw a..h (100..107 ns), the second
    // i..l (1000..1003) and y and z at 105, tied with f.
    let mut first = Waited::ZERO;
    for (at, name) in NAMES[..8].iter().enumerate() {
        first.charge(name, None, 1, 100 + at as u64);
    }
    let mut second = Waited::ZERO;
    for (at, name) in NAMES[8..].iter().enumerate() {
        second.charge(name, None, 1, 1000 + at as u64);
    }
    second.charge("y", Some(1), 1, 105);
    second.charge("z", Some(1), 1, 105);
    assert_eq!((first.other_ns, second.other_ns), (0, 0));
    let mut rows: std::vec::Vec<WaitRow> = first.rows.iter().chain(second.rows.iter()).flatten().copied().collect();
    let other = (first.other_count + second.other_count, first.other_ns + second.other_ns);
    let want = Waited::gathered(&mut rows.clone(), other.0, other.1);
    for turn in 0..rows.len() {
        rows.rotate_left(1);
        assert_eq!(Waited::gathered(&mut rows.clone(), other.0, other.1), want, "rotation {turn}");
    }
    rows.reverse();
    assert_eq!(Waited::gathered(&mut rows, other.0, other.1), want);
    // The eight largest in rank order: l, k, j, i, h (107), g (106), then of the three at 105
    // f and y by name; z, e, d, c, b and a are in `other`.
    let kept: std::vec::Vec<&str> = want.rows.iter().flatten().map(|row| row.on).collect();
    assert_eq!(kept, ["l", "k", "j", "i", "h", "g", "f", "y"]);
    assert_eq!((want.other_count, want.other_ns), (6, 105 + 104 + 103 + 102 + 101 + 100));
    assert_eq!(want.total(), (first.total().0 + second.total().0, first.total().1 + second.total().1));
    // The control: the merge-order form keeps y one way and z the other on these tables.
    let names = |waited: Waited| {
        let mut names: std::vec::Vec<&str> = waited.rows.iter().flatten().map(|row| row.on).collect();
        names.sort_unstable();
        names
    };
    assert_ne!(names(first.plus(&second)), names(second.plus(&first)));
}

/// The task a thread runs is named and put back.
#[test]
fn doing_is_set_and_restored() {
    let was = set_doing("a.task");
    assert_eq!(doing(), "a.task");
    assert_eq!(set_doing(was), "a.task");
    assert_eq!(doing(), was);
}
