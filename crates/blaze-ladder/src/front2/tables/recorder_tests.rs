//! The recorder keeps each read once as it is offered (recorder.rs `Recorder::read`): a
//! repeat is never pushed, so the sort at `finish` is of the distinct reads alone.
//!
//! Rule 23, the cause removed: before this piece every offer was pushed and the whole tape
//! sorted at `finish`, so the first test's 100,000 offers were sorted (at least 99,999
//! compares, and `kept` was the tape's length until the dedup); with `seen` and `last` taken
//! out of `read` it fails on `sort_compares`.

use blaze_id::{ContentId, ItemKey};
use blaze_own::AVec;
use blaze_scope::names_cells::NameCell;

use super::head_tests::a;
use super::recorder::{ReadSink, Recorder, RecorderOpts, TableRead};

fn key(n: u32) -> ItemKey {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&n.to_be_bytes());
    ItemKey(ContentId(bytes))
}

/// The `i`th of a pool of distinct reads, of every kind.
fn read_of(i: u32) -> TableRead {
    match i % 4 {
        0 => TableRead::Names(NameCell::Item { key: key(i) }),
        1 => TableRead::Names(NameCell::Module { module: key(i) }),
        2 => {
            let mut item = [0u8; 16];
            item[..4].copy_from_slice(&i.to_be_bytes());
            TableRead::Head { item, part: (i % 7) as u8 }
        }
        _ => {
            let mut krate = [0u8; 16];
            krate[..4].copy_from_slice(&i.to_be_bytes());
            TableRead::Impls { krate, bucket: [(i % 251) as u8; 16] }
        }
    }
}

/// 100,000 offers of 50 distinct cells (runs of one cell, then interleaved): 50 kept, every
/// offer counted, and the sort's compares bounded by the distinct reads (50 * 50), not the
/// tape (the old recorder sorted all 100,000).
#[test]
fn a_hundred_thousand_reads_of_fifty_cells_keep_fifty() {
    let r = Recorder::new(RecorderOpts { alloc: a() });
    for i in 0..100_000u32 {
        // Runs of 7 of one cell (the `last` path), cycling through 50 cells (the `seen` path).
        r.read(read_of((i / 7) % 50));
    }
    let so_far = r.tally();
    assert_eq!((so_far.offered, so_far.kept), (100_000, 50), "{so_far:?}");
    let (list, tally) = r.finish_tallied().expect("within the cap");
    assert_eq!(tally.offered, 100_000);
    assert_eq!(tally.kept, 50);
    assert_eq!(list.len(), 50);
    assert!(tally.sort_compares <= 50 * 50, "compares {} past the bound of the distinct reads", tally.sort_compares);
    assert!(list.windows(2).all(|w| w[0] < w[1]), "sorted, each once");
}

/// A mixed tape (drawn: lengths, a pool, repeats, every kind) finishes as the old recorder's
/// list: the whole tape sorted and deduplicated. The planted drop is offered too and must be
/// absent from both.
#[test]
fn a_mixed_tape_finishes_as_the_whole_tape_sorted() {
    blaze_sim::check(blaze_sim::Config::DEFAULT, |tape| {
        let pool = tape.below(64, "pool") as u32 + 1;
        let len = tape.below(2_000, "len") as usize;
        let planted = tape.below(2, "plant") == 1;
        let dropped = NameCell::Item { key: key(0) };
        let r = Recorder::new(RecorderOpts { alloc: a() });
        if planted {
            r.plant_drop(dropped);
        }
        let mut oracle: AVec<'_, TableRead> = AVec::new_in(a());
        for _ in 0..len {
            let read = read_of(tape.below(u64::from(pool), "read") as u32);
            r.read(read);
            if !(planted && read == TableRead::Names(dropped)) {
                oracle.push(read);
            }
        }
        oracle.sort_unstable();
        oracle.dedup();
        let (list, tally) = r.finish_tallied().expect("within the cap");
        assert_eq!(&list[..], &oracle[..], "the old recorder's list");
        assert_eq!(tally.kept as usize, oracle.len());
    });
}
