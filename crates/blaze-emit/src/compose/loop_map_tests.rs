//! The loop map's builder (loop_map.rs) against the record's rule, blaze-tier record/form.rs:
//! "each header's live values by value" sorted, refused as "a code record's loop header {header}
//! has live values not sorted by value, each once" (D100 point 11). The last test is the tier
//! equality harness's failing case, seed 63 (tape `0 2 0*2 1`): no parameters, three blocks, the
//! middle one a counted loop on itself, the entry block taking the address of the generator's
//! three slots, so values 0 to 2 (the `slot_addr` results) and slots 0 to 2 are all live into
//! the one header.

use std::alloc::Global;

use blaze_lir::builder::{MemAttrs, Param, Variadic};
use blaze_lir::func::{BlockKind, Conv, Func, SlotKind};
use blaze_lir::{Builder, Flags, Full, IntCond, Op, Ty};
use blaze_own::Alloc;
use blaze_tier::record::seal;
use blaze_tier::{ARM64, Named, Names, Outside, Place, Reg, Table, Tier, TierError, TierName};

use super::Fast;
use super::analysis::{Home, analyse};
use super::loop_map::{LoopMap, slot_key, slot_of};
use crate::table_gen::EncodedTable;

/// Slot 0 and value 0 under one header are two rows with two keys (value 0, then the slot at
/// `values + 0`), sorted, each once; a value found twice at one place is one row.
/// Fails if a slot is keyed by its bare index (two rows keyed 0), or a repeat is not merged.
/// Expected: passes, printing "loop map: keys [0, 2, 3, 5] for values 0 and 2 and slots 0 and 2".
#[test]
fn a_slot_and_a_value_of_one_index_get_two_keys() {
    let alloc = Alloc::global();
    let mut map = LoopMap::new(3, 3, 1, alloc).unwrap();
    map.slot(0, 2).unwrap();
    map.value(0, 0, Place::Reg(Reg(19))).unwrap();
    map.slot(0, 0).unwrap();
    map.value(0, 2, Place::Cell { sp_offset: 0 }).unwrap();
    map.value(0, 0, Place::Reg(Reg(19))).unwrap();
    let (loops, live) = map.finish(alloc).unwrap();
    assert_eq!(loops.len(), 1);
    assert_eq!((loops[0].first, loops[0].count), (0, 4), "four distinct keys");
    let keys: std::vec::Vec<u32> = live.iter().map(|it| it.value).collect();
    assert_eq!(keys, [0, 2, slot_key(3, 0), slot_key(3, 2)]);
    assert!(live.windows(2).all(|w| w[0].value < w[1].value), "sorted by value, each once");
    assert_eq!(live[2].place, Place::Slot { sp_offset: 0 });
    assert_eq!(slot_of(3, live[3].value), Some(2), "a slot's index reads back from its key");
    println!("loop map: keys {keys:?} for values 0 and 2 and slots 0 and 2");
}

/// One value at two places in one header is a broken analysis: refused by name, never one place
/// picked (a value has one home at a header).
/// Expected: passes.
#[test]
fn one_value_at_two_places_is_refused() {
    let alloc = Alloc::global();
    let mut map = LoopMap::new(4, 0, 1, alloc).unwrap();
    map.value(0, 1, Place::Reg(Reg(19))).unwrap();
    map.value(0, 1, Place::Cell { sp_offset: 0 }).unwrap();
    match map.finish(alloc) {
        Err(TierError::Refused { tier: TierName::Fast, inst: None, what }) => {
            assert_eq!(what, "one value live into a loop header at two places");
        }
        Err(other) => panic!("refused, but not by the rule: {other}"),
        Ok(_) => panic!("one value at two places was taken"),
    }
}

/// The tape's program: entry `data`, `acc`, `counters` = the addresses of slots 0, 1, 2, zeroed
/// and the loop's count (3) stored; block 1 counts down, adds `data[0] + 1` into `acc`, and
/// branches to itself while the count was not 0; block 2 returns `acc`.
fn tape_shape() -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let data_slot = b.slot(16, 4, SlotKind::Temp);
    let acc_slot = b.slot(8, 3, SlotKind::Temp);
    let counter_slot = b.slot(24, 3, SlotKind::Temp);
    let (entry, header, exit) =
        (b.create_block(BlockKind::Normal), b.create_block(BlockKind::Normal), b.create_block(BlockKind::Normal));
    let attrs = MemAttrs::aligned(3);
    b.begin(entry);
    let (data, acc, counters) = (b.slot_addr(data_slot), b.slot_addr(acc_slot), b.slot_addr(counter_slot));
    let zero = b.iconst(Ty::I64, 0);
    b.store(Flags::NONE, zero, data, 0, attrs);
    b.store(Flags::NONE, zero, data, 8, attrs);
    b.store(Flags::NONE, zero, acc, 0, attrs);
    for (at, count) in [0u128, 3, 0].into_iter().enumerate() {
        let count = b.iconst(Ty::I64, count);
        b.store(Flags::NONE, count, counters, 8 * at as i32, attrs);
    }
    b.jump((header, &[]));
    b.begin(header);
    let count = b.load(Ty::I64, Flags::NONE, counters, 8, attrs);
    let zero = b.iconst(Ty::I64, 0);
    let again = b.icmp(IntCond::Ne, count, zero);
    let one = b.iconst(Ty::I64, 1);
    let less = b.binary(Op::Sub, Flags::NONE, count, one);
    b.store(Flags::NONE, less, counters, 8, attrs);
    let old = b.load(Ty::I64, Flags::NONE, acc, 0, attrs);
    let first = b.load(Ty::I64, Flags::NONE, data, 0, attrs);
    let sum = b.binary(Op::Add, Flags::NONE, old, first);
    let next = b.binary(Op::Add, Flags::NONE, sum, one);
    b.store(Flags::NONE, next, acc, 0, attrs);
    b.br(Flags::NONE, again, (header, &[]), (exit, &[]));
    b.begin(exit);
    let out = b.load(Ty::I64, Flags::NONE, acc, 0, attrs);
    b.ret(&[out]);
    b.finish_checked().expect("the tape's program builds")
}

/// The program names nothing outside itself.
struct Nameless;

impl Names for Nameless {
    fn name(&self, _: Outside) -> Option<Named<'_>> {
        None
    }

    fn routines(&self) -> crate::RoutinePlaces {
        crate::RoutinePlaces::NONE
    }
}

/// The harness's seed 63: header 0's map holds values 0, 1, 2 at their homes and slots 0, 1, 2
/// keyed after every instruction, sorted by value, each once; the Fast tier's record of it seals.
/// Fails without the one key space: the map is `[0, 0, 1, 1, 2, 2]` (a value and the slot of
/// its index under one key) and `seal` refuses it with "loop header 0 has live values not
/// sorted by value, each once".
/// Expected: passes, printing "seed 63's shape: header 0 keys [0, 1, 2, n, n+1, n+2] with n = N;
/// the Fast record seals".
#[test]
fn the_harness_seed_63_shape_seals() {
    let alloc = Alloc::global();
    let func = tape_shape();
    let view = func.view();
    blaze_lir::verify(view).expect("the tape's program verifies");
    let n = view.len() as u32;
    let an = match analyse(view, &ARM64, alloc) {
        Ok(an) => an,
        Err(error) => panic!("the analysis refused the tape's program: {error}"),
    };
    assert_eq!(an.loops.len(), 1, "one loop");
    let row = an.loops[0];
    let rows = &an.live[row.first as usize..(row.first + row.count) as usize];
    let keys: std::vec::Vec<u32> = rows.iter().map(|it| it.value).collect();
    assert_eq!(keys, [0, 1, 2, n, n + 1, n + 2], "values 0 to 2, then slots 0 to 2 after the {n} instructions");
    for it in &rows[..3] {
        let right = match (an.home[it.value as usize], it.place) {
            (Home::Reg(home), Place::Reg(place)) => home == place,
            (Home::Cell, Place::Cell { .. }) => true,
            _ => false,
        };
        assert!(right, "v{} is at its home in the map: {:?} against {:?}", it.value, it.place, an.home[it.value as usize]);
    }
    assert!(rows[3..].iter().all(|it| matches!(it.place, Place::Slot { .. })), "the slots' rows say Slot");
    let table = match EncodedTable::read(include_bytes!("../../table/arm64.bztab"), Alloc::global()) {
        Ok(table) => table,
        Err(error) => panic!("the committed table does not read: {error}"),
    };
    let machine = table.head().machine;
    let emitted = match (Fast { table: &table, regs: &ARM64 }).emit(blaze_lir::verify(view).expect("the tape's program is LIR"), &Nameless, machine, alloc) {
        Ok(emitted) => emitted,
        Err(error) => panic!("the Fast tier refused the tape's program: {error}"),
    };
    assert_eq!(emitted.live.len(), 6, "the record carries the analysis's six rows");
    assert!(emitted.live.windows(2).all(|w| w[0].value < w[1].value), "sorted by value, each once");
    if let Err(error) = seal(emitted) {
        panic!("the Fast tier's record does not seal: {error}");
    }
    println!("seed 63's shape: header 0 keys {keys:?} with n = {n}; the Fast record seals");
}
