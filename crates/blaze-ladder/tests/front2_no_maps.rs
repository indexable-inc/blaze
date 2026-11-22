//! A heads store's tables are in the allocator they are handed, and map nothing of their own
//! (front2 rows.rs, "Whose memory"): making, filling and dropping 1,000 of each of
//! [`Rows`], the type table's columns ([`stable_tys`]), [`RegionTable`] and [`Store`] reserves no
//! `blaze_mem` region and unmaps none; in a frame, they leave it at its mark after a reset.
//!
//! The ONLY test of this binary: blaze-mem's system-call counters are the process's, so a
//! second test running beside it would move them.
//!
//! Rule 23, the cause removed: before this piece each table reserved its own `AppendVec`
//! region at its first row and each type column at its first entry (an `mmap`, and an `munmap`
//! at the drop), so the same loop counted at least 1,000 reservations per kind of table.

#![feature(allocator_api)]

use blaze_ladder::front2::head_record::RegionTable;
use blaze_ladder::front2::heads::Store;
use blaze_ladder::front2::rows::{Rows, stable_tys};
use blaze_mem::syscalls::{Kind, snapshot};
use blaze_own::Alloc;
use blaze_typeck::{List, PNode};

const EACH: u64 = 1_000;

/// blaze-mem's reservations and unmaps so far, process-wide.
fn maps() -> (u64, u64) {
    let mut reserved = 0;
    let mut unmapped = 0;
    for (kind, calls) in snapshot() {
        match kind {
            Kind::Reserve => reserved += calls.calls,
            Kind::Munmap => unmapped += calls.calls,
            _ => {}
        }
    }
    (reserved, unmapped)
}

/// Makes, fills and drops `EACH` rows tables and type tables in `alloc`.
fn tables(alloc: Alloc<'_>) {
    for i in 0..EACH {
        let rows: Rows<u64, u64, Alloc<'_>> = Rows::new_in(alloc);
        for k in 0..20 {
            rows.insert(k, i + k);
        }
        assert_eq!(rows.get(&3), Some(&(i + 3)));
        drop(rows);
        let tys = stable_tys(alloc).expect("a type table over fresh columns");
        let unit = tys.intern_shared(PNode::Tuple(List::EMPTY)).expect("interned");
        let mut last = unit;
        for _ in 0..100 {
            let list = tys.intern_list_shared(&[last, unit]).expect("a list");
            last = tys.intern_shared(PNode::Tuple(list)).expect("interned");
        }
        drop(tys);
    }
}

#[test]
fn heads_stores_map_nothing_and_leave_the_frame_at_its_mark() {
    // The frames first: their own reservations are not what is measured.
    let mut frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let kept: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the stores' frame")));
    let kept_erased: &'static blaze_own::Erased<&'static blaze_mem::Frame> = Box::leak(Box::new(blaze_own::Erased::new(kept)));
    let kept_alloc: Alloc<'static> = kept_erased.handle();
    let before = maps();

    // (1) Rows and type tables in a frame: mapped nothing, and the frame back at its mark.
    let mark = frame.mark();
    let used = frame.used();
    {
        let erased = blaze_own::Erased::new(&frame);
        tables(erased.handle());
    }
    assert!(frame.used() > used, "the tables were in the frame");
    frame.reset_to(mark);
    assert_eq!(frame.used(), used, "the frame is at its mark after the reset");

    // (2) Region tables and whole stores, in the allocator a store is handed.
    for i in 0..EACH {
        drop(RegionTable::new_in(kept_alloc));
        let store = Store::new(None, kept, kept_alloc);
        // A row in the store's region table (D148: the head records' regions; a refused head
        // is a row as a made one is).
        store.regions.file_refused(blaze_typeck::ItemKey(u128::from(i)));
        let unit = store.tys.intern_shared(PNode::Tuple(List::EMPTY)).expect("interned");
        store.maps.fieldless_enums.insert(blaze_typeck::ItemKey(u128::from(i)), 3);
        let list = store.tys.intern_list_shared(&[unit, unit]).expect("a list");
        store.tys.intern_shared(PNode::Tuple(list)).expect("interned");
        drop(store);
    }

    let after = maps();
    // Not one a table: the first run printed (2, 3) across 1,000 of each table, which is the two
    // frames growing and the reset giving chunks back, where a region a table was thousands.
    let grew = (after.0 - before.0, after.1 - before.1);
    assert!(grew.0 <= 8 && grew.1 <= 8, "(reservations, unmaps) of blaze-mem across {EACH} of each table grew by {grew:?}: a map per table would be thousands");
    std::println!("heads tables: {EACH} of each made and dropped; blaze-mem reservations and unmaps {before:?} -> {after:?}");
}
