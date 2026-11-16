//! The table of waits: a guard's row comes and goes with it, on unwind too; rows claimed and
//! given back; a region checked before it is read; and a reader interleaved with a writer
//! never keeps a torn row, while the same reader without the sequence check does (the planted
//! bug's control, which shows the check is what the first result rests on).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use blaze_launch::Thread;

use super::{RegionError, Row, Snapshot, TableFull, What, Waits};

const ROWS: u32 = 4;

/// A region as the shared mapping would be: aligned, of the size the table asks.
#[repr(C, align(64))]
struct Region([u8; Waits::bytes_for(ROWS)]);

impl Region {
    fn new() -> Self {
        Self([0; Waits::bytes_for(ROWS)])
    }

    fn base(&mut self) -> NonNull<u8> {
        NonNull::from(&mut self.0).cast()
    }
}

/// A fresh table over `region`.
fn table(region: &mut Region) -> Waits<'_> {
    let bytes = region.0.len();
    // SAFETY: the region is this test's, writable, aligned, alive for the borrow the result
    // carries, and used for nothing else.
    unsafe { Waits::init(region.base(), bytes) }.expect("a region of bytes_for(ROWS)")
}

const EMPTY: Row = Row { slot: 0, thread: Thread(0), what: What::Code, arg: 0, since: 0 };

fn waiting(waits: &Waits<'_>, now: u64, bound: u64) -> (Snapshot, [Row; ROWS as usize]) {
    let mut out = [EMPTY; ROWS as usize];
    let found = waits.snapshot(now, bound, &mut out);
    (found, out)
}

#[test]
fn the_guard_says_the_wait_and_its_drop_clears_it() {
    let mut region = Region::new();
    let waits = table(&mut region);
    assert_eq!(waits.rows(), ROWS);
    let mut slot = waits.claim(Thread(7)).expect("a free row");
    let guard = slot.enter(What::Code, 3, 10);
    let (found, out) = waiting(&waits, 100, 5);
    assert_eq!(found, Snapshot { listed: 1, ..Snapshot::default() });
    assert_eq!(out[0], Row { slot: 0, thread: Thread(7), what: What::Code, arg: 3, since: 10 });
    drop(guard);
    assert_eq!(waiting(&waits, 100, 5).0, Snapshot::default());
}

#[test]
fn only_waits_past_the_bound_are_listed() {
    let mut region = Region::new();
    let waits = table(&mut region);
    let mut slot = waits.claim(Thread(1)).expect("a free row");
    let _guard = slot.enter(What::Futex, 0x1000, 95);
    assert_eq!(waiting(&waits, 100, 5).0.listed, 0, "waited exactly the bound");
    assert_eq!(waiting(&waits, 100, 4).0.listed, 1, "waited past the bound");
    assert_eq!(waiting(&waits, 90, 0).0.listed, 0, "a tick before the wait began");
}

#[test]
fn the_guard_clears_on_unwind() {
    let mut region = Region::new();
    let waits = table(&mut region);
    let mut slot = waits.claim(Thread(2)).expect("a free row");
    let unwound = catch_unwind(AssertUnwindSafe(|| {
        let _guard = slot.enter(What::Lock, 0x40, 1);
        assert_eq!(waiting(&waits, 100, 0).0.listed, 1);
        panic!("the wait unwinds");
    }));
    assert!(unwound.is_err());
    assert_eq!(waiting(&waits, 100, 0).0, Snapshot::default(), "the unwind cleared the row");
}

#[test]
fn rows_are_claimed_and_given_back() {
    let mut region = Region::new();
    let waits = table(&mut region);
    let mut slots: Vec<_> =
        (0..ROWS).map(|i| waits.claim(Thread(u64::from(i))).expect("a free row")).collect();
    assert_eq!(waits.claim(Thread(99)).err(), Some(TableFull { thread: Thread(99), rows: ROWS }));
    let given_back = slots.remove(2).slot();
    let again = waits.claim(Thread(99)).expect("the row given back");
    assert_eq!(again.slot(), given_back);
    // More rows past the bound than `out` holds: counted, not dropped.
    let guards: Vec<_> = slots.iter_mut().map(|it| it.enter(What::Join, 0, 0)).collect();
    let mut out = [EMPTY; 1];
    let found = waits.snapshot(10, 0, &mut out);
    assert_eq!(found, Snapshot { listed: 1, not_listed: 2, ..Snapshot::default() });
    drop(guards);
}

#[test]
fn a_region_is_checked_before_it_is_read() {
    let mut region = Region::new();
    let bytes = region.0.len();
    let base = region.base();
    // SAFETY: the test's own region, readable, alive across these calls.
    let zeroed = unsafe { Waits::attach(base, bytes) };
    assert_eq!(zeroed.err(), Some(RegionError::NotATable { base: base.addr().get(), magic: 0 }));
    // SAFETY: as above; one byte past an aligned base is refused before any read.
    let off = unsafe { Waits::attach(base.add(1), bytes - 1) };
    assert_eq!(off.err(), Some(RegionError::Misaligned { base: base.addr().get() + 1 }));
    // SAFETY: as above; 64 bytes hold the header and no row.
    let small = unsafe { Waits::init(base, 64) };
    assert_eq!(small.err(), Some(RegionError::TooSmall { base: base.addr().get(), bytes: 64 }));

    let waits = table(&mut region);
    let mut slot = waits.claim(Thread(5)).expect("a free row");
    let _guard = slot.enter(What::Read, 3, 0);
    // SAFETY: the region `init` made, alive across this call: the monitor's view of it.
    let monitor = unsafe { Waits::attach(base, bytes) }.expect("the region init made");
    assert_eq!(monitor.rows(), ROWS);
    let (found, out) = waiting(&monitor, 1, 0);
    assert_eq!(found.listed, 1);
    assert_eq!(out[0].what, What::Read);
}

/// One writer entering and leaving waits as fast as it can, each wait's argument equal to its
/// start, and a reader at the same time. Gives how many torn rows (argument and start unequal)
/// the reader kept within `reads` snapshots, stopping at the first with `stop_at_first`.
fn interleave(checked: bool, reads: usize, stop_at_first: bool) -> usize {
    let mut region = Region::new();
    let waits = table(&mut region);
    let stop = AtomicBool::new(false);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut slot = waits.claim(Thread(1)).expect("a free row");
    thread::scope(|scope| {
        let writer = scope.spawn(|| {
            let mut k = 1u64;
            while !stop.load(Ordering::Relaxed) {
                let _guard = slot.enter(What::Lock, k, k);
                k += 1;
            }
        });
        let mut torn = 0;
        let mut out = [EMPTY; ROWS as usize];
        for _ in 0..reads {
            assert!(Instant::now() < deadline, "the interleaving ran past its 20 s");
            let found = if checked {
                waits.snapshot(u64::MAX, 0, &mut out)
            } else {
                waits.snapshot_unchecked(u64::MAX, 0, &mut out)
            };
            torn += out[..found.listed].iter().filter(|it| it.arg != it.since).count();
            if stop_at_first && torn > 0 {
                break;
            }
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().expect("the writer ran without a panic");
        torn
    })
}

#[test]
fn a_reader_never_keeps_a_torn_row() {
    assert_eq!(interleave(true, 2_000_000, false), 0);
}

/// The control: without the sequence check the same reader keeps torn rows, so the test above
/// passes because of the check, not because tearing never happens here.
#[test]
fn without_the_sequence_check_rows_tear() {
    assert!(interleave(false, 50_000_000, true) > 0, "no torn row seen: the control proves nothing");
}
