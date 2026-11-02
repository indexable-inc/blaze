use blaze_ids::rng::SplitMix64;

use super::*;

const ROWS: usize = 16;

/// A table in miniature: a version per row, bumped by every write, with snapshots as copies.
/// A reader must be told of every row whose version differs from what it saw, unless the log
/// says rows were lost.
#[test]
fn a_reader_sees_every_row_written_since_it_read() {
    for seed in 0..64 {
        let mut rng = SplitMix64::new(seed);
        let mut log = Touched::default();
        let mut versions = [0u32; ROWS];
        let mut next_version = 0;
        let mut snapshots: Vec<(u32, [u32; ROWS])> = Vec::new();

        let mut cursor = None;
        let mut seen = versions;
        let mut rolled_back_since_read = true;

        for _ in 0..20_000 {
            match rng.below(100) {
                0..70 => {
                    let at = rng.index(ROWS);
                    next_version += 1;
                    versions[at] = next_version;
                    log.push(at as u32);
                }
                70..78 => snapshots.push((log.mark(), versions)),
                78..86 => {
                    if let Some((mark, before)) = snapshots.pop() {
                        log.rollback_to(mark);
                        versions = before;
                        rolled_back_since_read = true;
                    }
                }
                86..92 => {
                    if snapshots.pop().is_some() {
                        log.commit();
                    }
                }
                _ => {
                    let epoch_moved = cursor.is_some_and(|at: TouchedAt| at.epoch != log.epoch);
                    match log.since(cursor) {
                        Some(rows) => {
                            for at in 0..ROWS {
                                if versions[at] != seen[at] {
                                    assert!(
                                        rows.iter().any(|row| row.index == at as u32),
                                        "seed {seed}: row {at} changed but is not in the log"
                                    );
                                }
                            }
                        }
                        None => assert!(
                            rolled_back_since_read || epoch_moved || cursor.is_none(),
                            "seed {seed}: rows lost with no rollback or clear"
                        ),
                    }
                    cursor = Some(log.now());
                    seen = versions;
                    rolled_back_since_read = false;
                }
            }
        }
    }
}

#[test]
fn outside_snapshots_the_log_stays_under_its_cap() {
    let mut log = Touched::default();
    for at in 0..10 * CAP as u32 {
        log.push(at);
        assert!(log.rows.len() <= CAP);
    }
    let mark = log.mark();
    for at in 0..2 * CAP as u32 {
        log.push(at);
    }
    assert_eq!(log.rows.len(), mark as usize + 2 * CAP, "a snapshot's rows are never cleared");
    log.rollback_to(mark);
    assert!(log.rows.len() <= mark as usize);
    for _ in 0..10 * CAP {
        let _ = log.mark();
        log.push(1);
        log.push(2);
        log.commit();
        assert!(log.rows.len() <= CAP + 2, "writes in snapshots are trimmed when they close");
    }
}

#[test]
fn a_cursor_into_rolled_back_rows_is_refused() {
    let mut log = Touched::default();
    log.push(0);
    let mark = log.mark();
    log.push(1);
    let read = log.now();
    log.rollback_to(mark);
    assert_eq!(log.since(Some(read)), None, "the row read was rolled back");
    log.push(2);
    assert_eq!(log.since(Some(read)), None, "same length, but a different row");
    let read = log.now();
    log.push(3);
    let rows = log.since(Some(read)).expect("nothing read was lost");
    assert_eq!(rows.iter().map(|row| row.index).collect::<Vec<_>>(), [3]);
}

#[test]
fn reading_and_writing_after_warm_up_allocates_nothing() {
    let mut log = Touched::default();
    for at in 0..CAP as u32 {
        log.push(at);
    }
    let mut cursor = Some(log.now());
    blaze_alloc_count::assert_at_most(0, "touched log after warm-up", || {
        for at in 0..4 * CAP as u32 {
            log.push(at % 7);
            if log.since(cursor).is_none() {
                cursor = None;
            }
            cursor = Some(log.now());
        }
    });
}
