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

/// A NAMES LOOKUP HASHES NOTHING ON A REPEAT (recorder.rs `ReadSink::binding`): 10,000 binding
/// lookups of 37 distinct (module, namespace, name) reads, put through the walk's sink
/// (`NamesInto` over the recorder, as front2 tables/view.rs walks), derive 37 keys, and the list
/// equals the per-read keying's (the trait's default, through a sink that keys each read):
/// byte-equal cells (rule 21). The owned recorder the same. CONTROL (rule 23): the default
/// keying derives a key per lookup, so the counter is the recorder's own.
#[test]
fn ten_thousand_lookups_of_thirty_seven_names_derive_thirty_seven_keys() {
    use blaze_scope::names_cells::CellSink as _;
    use blaze_surface::Namespace;

    use super::owned_reads::{OwnedRecorder, OwnedRecorderOpts, Owning, ReadOwners as _};
    use super::recorder::NamesInto;
    use crate::front2::heads::Want;

    const NAMES: [&str; 37] = [
        "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r", "s", "t", "u", "v", "w", "x", "y", "z", "Vec", "Option", "Result", "String", "Box", "Rc", "Arc", "Cell", "RefCell", "HashMap", "Iterator",
    ];
    let read = |i: u32| -> (ItemKey, Namespace, &'static str) {
        let at = (i * 7 + i / 5) as usize % NAMES.len();
        // The module and namespace are functions of the name: 37 distinct reads in all.
        let ns = if at % 2 == 0 { Namespace::Types } else { Namespace::Values };
        (key((at % 3) as u32), ns, NAMES[at])
    };
    let r = Recorder::new(RecorderOpts { alloc: a() });
    let walk = NamesInto(&r);
    // The per-read keying (the trait's default), the oracle.
    let oracle = Recorder::new(RecorderOpts { alloc: a() });
    for i in 0..10_000u32 {
        let (module, ns, name) = read(i);
        walk.put_binding(module, ns, name);
        oracle.read(TableRead::Names(NameCell::Binding { module, name: blaze_scope::cell_key::cell_key(ns, name) }));
    }
    assert_eq!(r.binding_keys_derived(), 37, "one key a distinct read");
    assert_eq!(oracle.binding_keys_derived(), 0, "CONTROL: keyed by the caller, the recorder derived none");
    let (mine, theirs) = (r.finish().expect("within the cap"), oracle.finish().expect("within the cap"));
    assert_eq!(mine.len(), 37);
    assert_eq!(&mine[..], &theirs[..], "byte-equal cells however the key is derived");
    // The owned recorder, through one owner.
    let owned = OwnedRecorder::new(OwnedRecorderOpts { alloc: a(), owning: Owning::Each });
    let owner = Want::Fn(blaze_id::ItemKey(ContentId([9; 32])));
    owned.enter(owner);
    let walk = NamesInto(&owned);
    for i in 0..10_000u32 {
        let (module, ns, name) = read(i);
        walk.put_binding(module, ns, name);
    }
    owned.leave();
    assert_eq!(owned.binding_keys_derived(), 37, "the owned recorder: one key a distinct read");
    let mut kept: AVec<'_, TableRead> = AVec::new_in(a());
    owned.reads_of(owner, &mut |one| kept.push(one)).expect("its reads");
    assert_eq!(&kept[..], &mine[..], "the owned recorder's cells equal the recorder's");
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
