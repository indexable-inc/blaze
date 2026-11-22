//! The retired-code table: rows refused by name, lookups while the launcher appends, and the
//! stop line against its exact bytes. Plain threads: the crate has no `blaze-sim` dev-dependency.

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use blaze_launch::Slot;

use super::{AppendError, AppenderTaken, REPORT_MAX, ReportTooLong, Retired, Table, report};

fn range(i: usize) -> Retired {
    Retired { start: 0x10_0000 + i * 64, len: 32, function: Slot((i % 7) as u32), version: i as u64 }
}

#[test]
fn a_full_table_refuses_by_name() {
    let table = Table::<2>::new();
    let mut appender = table.appender().expect("the first appender");
    appender.append(range(0)).expect("row 0 of 2");
    appender.append(range(1)).expect("row 1 of 2");
    assert_eq!(
        appender.append(range(2)),
        Err(AppendError::Full { capacity: 2, range: range(2) })
    );
    assert_eq!(table.len(), 2, "a refused row is not published");
    assert_eq!(table.which(range(2).start), None);
}

#[test]
fn overlaps_empties_and_a_second_writer_are_refused() {
    let table = Table::<8>::new();
    let mut appender = table.appender().expect("the first appender");
    assert!(matches!(table.appender(), Err(AppenderTaken)));
    appender.append(range(0)).expect("row 0");
    let over = Retired { start: range(0).start + 31, len: 4, ..range(0) };
    assert_eq!(
        appender.append(over),
        Err(AppendError::Overlaps { range: over, earlier: range(0) })
    );
    let empty = Retired { len: 0, ..range(3) };
    assert_eq!(appender.append(empty), Err(AppendError::Empty { range: empty }));
    let wraps = Retired { start: usize::MAX - 3, len: 8, ..range(3) };
    assert_eq!(appender.append(wraps), Err(AppendError::Empty { range: wraps }));
    // Adjacent is not overlapping: the end is exclusive.
    let next = Retired { start: range(0).end(), len: 8, ..range(3) };
    appender.append(next).expect("the range right after row 0");
    assert_eq!(table.which(range(0).end() - 1), Some(range(0)));
    assert_eq!(table.which(range(0).end()), Some(next));
}

/// One appender and four readers at once: every published row is found whole, at every byte
/// of it, and no address between rows is ever claimed.
#[test]
fn lookups_while_the_launcher_appends() {
    const ROWS: usize = 4096;
    static TABLE: Table<ROWS> = Table::new();
    static DONE: AtomicBool = AtomicBool::new(false);
    thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                let mut checked = 0usize;
                loop {
                    let done = DONE.load(Ordering::Acquire);
                    let published = TABLE.len();
                    for i in (0..published).rev().take(64) {
                        let row = range(i);
                        assert_eq!(TABLE.which(row.start), Some(row), "row {i}'s first byte");
                        assert_eq!(TABLE.which(row.end() - 1), Some(row), "row {i}'s last byte");
                        assert_eq!(TABLE.which(row.end()), None, "the gap after row {i}");
                        checked += 1;
                    }
                    if done {
                        assert_eq!(published, ROWS);
                        break;
                    }
                }
                assert!(checked > 0, "a reader checked no row");
            });
        }
        scope.spawn(|| {
            let mut appender = TABLE.appender().expect("the one appender");
            for i in 0..ROWS {
                appender.append(range(i)).expect("a row within the capacity");
            }
            DONE.store(true, Ordering::Release);
        });
    });
}

#[test]
fn the_stop_line_byte_for_byte() {
    let retired = Retired { start: 0x1000, len: 64, function: Slot(7), version: 2 };
    let mut out = [0u8; REPORT_MAX];
    let n = report(&retired, 0x1010, &mut out).expect("fits REPORT_MAX");
    // The crate builds for 64-bit targets only (lib.rs), so an address is 16 nibbles.
    let expected: &[u8] =
        b"stopped: a call reached retired code of function 7 version 2 at 0x0000000000001010; \
this is a blaze bug\nattach to the report: these two lines, the output of `blaze --version`, \
the run's profile, and the edit made just before the stop\n";
    assert_eq!(&out[..n], expected, "{}", String::from_utf8_lossy(&out[..n]));
}

#[test]
fn the_widest_line_fits_and_a_short_buffer_is_refused() {
    let widest =
        Retired { start: usize::MAX - 1, len: 1, function: Slot(u32::MAX), version: u64::MAX };
    let mut out = [0u8; REPORT_MAX];
    let n = report(&widest, usize::MAX, &mut out).expect("the widest report fits REPORT_MAX");
    let line = &out[..n];
    assert!(line.windows(10).any(|it| it == b"4294967295"));
    assert!(line.windows(20).any(|it| it == b"18446744073709551615"));
    assert!(line.windows(18).any(|it| it == b"0xffffffffffffffff"));

    let mut short = [0u8; 40];
    assert_eq!(report(&widest, 0, &mut short), Err(ReportTooLong { needed: n, had: 40 }));
}
