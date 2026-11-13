//! What a store holds on the global heap once a build's values are durable and a trim has
//! run with nothing pinned ([`Store::heap_bytes`], `crate::heap`): its tables' slots, never a
//! value's bytes that a pack extent or a file of its own already holds.

use super::*;
use crate::heap::{HeapPlant, IDLE_HEAP_MOST};

/// The heap of a store that put `n` records of 256 incompressible bytes, flushed them and
/// was trimmed.
fn heap_after(n: u64, plant: HeapPlant) -> u64 {
    let mut sim = Sim::new(n, quiet());
    let mut store = open(&mut sim, "store");
    store.plant_heap = plant;
    for at in 0..n {
        store.put(key(at + 1), Kind::Code, &noise(256, at)).unwrap();
    }
    flush(&mut sim, &mut store);
    store.trim_pool();
    store.heap_bytes()
}

/// The oracle of an idle store's heap: under the bound, and what 100,000 records leave is
/// what 1,000 leave, but for room per block of the packs on disk (their checks), never per
/// record.
fn idle_heap_holds(plant: HeapPlant) -> Result<(), String> {
    let few = heap_after(1_000, plant);
    let many = heap_after(100_000, plant);
    if many > IDLE_HEAP_MOST {
        return Err(format!("after 100,000 records it holds {many} bytes, over {IDLE_HEAP_MOST}"));
    }
    // The checks of 100,000 records' packs: 4 bytes a block, far under this.
    if many.abs_diff(few) > 1 << 20 {
        return Err(format!("1,000 records leave {few} bytes and 100,000 leave {many}"));
    }
    Ok(())
}

/// The invariant: a store with nothing pinned, trimmed after its records are durable, holds
/// under `IDLE_HEAP_MOST` on the global heap whatever number of records it wrote.
#[test]
fn an_idle_store_holds_the_same_heap_after_a_thousand_records_and_a_hundred_thousand() {
    assert_eq!(idle_heap_holds(HeapPlant::None), Ok(()));
}

/// The control: a commit that keeps the room its drained records took holds a buffer the
/// size of every record, and the oracle says so.
#[test]
fn the_oracle_catches_a_pack_keeping_its_drained_room() {
    let found = idle_heap_holds(HeapPlant::KeepsPackMemory);
    assert!(found.as_ref().is_err_and(|why| why.contains("100,000")), "{found:?}");
}

/// The heap of a store that put one 16 MiB value (a file of its own), flushed it and was
/// trimmed.
fn heap_after_a_large_value(plant: HeapPlant) -> u64 {
    let mut sim = Sim::new(16, quiet());
    let mut store = open(&mut sim, "store");
    store.plant_heap = plant;
    let bytes = noise(16 << 20, 16);
    assert!(bytes.len() >= EXTERNAL_FROM, "a file of its own");
    store.put(key(1), Kind::Query, &bytes).unwrap();
    flush(&mut sim, &mut store);
    store.trim_pool();
    store.heap_bytes()
}

/// The invariant: a value held in a file of its own is not also held on the heap once that
/// file is durable and mapped.
#[test]
fn a_large_value_is_not_held_on_the_heap_once_its_file_is_written() {
    let heap = heap_after_a_large_value(HeapPlant::None);
    assert!(heap < IDLE_HEAP_MOST, "it holds {heap} bytes");
}

/// The control: a file of its own that keeps its bytes once mapped is over the bound.
#[test]
fn the_oracle_catches_a_file_of_its_own_kept_on_the_heap() {
    let heap = heap_after_a_large_value(HeapPlant::KeepsExternalMemory);
    assert!(heap >= 16 << 20, "it holds {heap} bytes");
}
