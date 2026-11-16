//! The proof of store/chunks.rs (rule 43: a simulation, never review). Under test a chunk is
//! 1024 bytes and an append of the seal 512 ([`CHUNK_HERE`], [`PART`]), so records cross chunk
//! ends, some are longer than a chunk, and a pack is several appends.
//!
//! The crash proof of the seal's writes is in store.rs's `tests`, which run through this seal:
//! `a_crash_or_failure_anywhere_keeps_each_record_whole_or_absent` (clean) and
//! `the_oracle_catches_an_index_published_before_its_payloads` (the planted
//! [`StorePlant::IndexBeforePayloads`]).

use blaze_sim::{Broke, Config, Tape, check as sim_check};
use bstd::io::{Io, Limits, SimWorld};
use bstd::mem::{Bytes, TASK_FRAME};
use bstd::path::Path;
use bstd::test::BUDGET;

use super::super::{Boot, PACKS, PACKS_MOST, Store, pack_bytes};
use super::*;

fn key(k: u32) -> Key {
    Key(blaze_ids::ContentId::of("blaze-cli chunks test", &k.to_le_bytes()))
}

/// Key `k`'s one value at length `len` (rule 21).
fn payload(k: u32, len: usize) -> std::vec::Vec<u8> {
    (0..len).map(|i| (k as usize * 7 + i * 13 + len) as u8).collect()
}

fn harness<E: core::fmt::Display>(what: &'static str) -> impl Fn(E) -> Broke {
    move |error| Broke::Harness(std::format!("{what}: {error}"))
}

fn oracle(said: std::string::String) -> Broke {
    Broke::Oracle(std::format!("store oracle: {said}"))
}

/// Every key against `held` (its bytes, or `None` for one never committed).
fn all_as_held(cx: &Cx, store: &mut PackStore, held: &[Option<std::vec::Vec<u8>>], when: &str) -> Result<(), Broke> {
    for (k, want) in held.iter().enumerate() {
        let mut into = Bytes::new_in(cx.frame(), "a record read back");
        let found = store.get_into(cx, key(k as u32), &mut into).map_err(harness("get"))?;
        match (found, want) {
            (true, Some(want)) if into.as_slice() == want.as_slice() => {}
            (false, None) => {}
            (true, Some(want)) => return Err(oracle(std::format!("{when}: key {k} read back as other bytes ({} for {})", into.len(), want.len()))),
            (true, None) => return Err(oracle(std::format!("{when}: key {k} was never committed and is held ({} bytes)", into.len()))),
            (false, Some(_)) => return Err(oracle(std::format!("{when}: key {k} was put and is lost"))),
        }
    }
    Ok(())
}

/// (i) and (iii) on the store: puts, in-place records committed and in-place records dropped,
/// in a drawn order; every record reads back as put before the seal, after it, at a new open
/// in the same boot and at one in another boot (which reads the pack whole and checks it); a
/// dropped reservation is never held; and the pack on disk is byte for byte `pack_bytes` of
/// the records (rule 25: the layout did not change).
fn round_trip(tape: &Tape) -> Result<(), Broke> {
    let io = Io::simulated(SimWorld::new(tape.below(1 << 30, "seed") as u64, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).map_err(harness("cx"))?;
    let root = io.need_fs().map_err(harness("fs"))?.root();
    let boot = Boot::Known([1; 16]);
    let mut store = PackStore::open_with(&cx, &root, boot, PACKS_MOST, StorePlant::None).map_err(harness("open"))?;
    let count = 1 + tape.index(60, "records");
    let mut held: std::vec::Vec<Option<std::vec::Vec<u8>>> = std::vec::Vec::new();
    for k in 0..count as u32 {
        let len = match tape.index(4, "how long") {
            0 => 0,
            1 => tape.index(9, "a few bytes"),
            2 => tape.index(700, "under a chunk"),
            _ => 1024 + tape.index(3000, "past a chunk"),
        };
        let bytes = payload(k, len);
        let how = tape.index(4, "how put");
        if how < 2 {
            let mut reserved = store.reserve(key(k), len).map_err(harness("reserve"))?.ok_or_else(|| oracle(std::format!("key {k} is new and the reservation said held")))?;
            if how == 0 {
                reserved.bytes().copy_from_slice(&bytes);
                reserved.commit().map_err(harness("commit"))?;
                held.push(Some(bytes));
                continue;
            }
            // Half written, then dropped: no row.
            reserved.bytes()[..len / 2].copy_from_slice(&bytes[..len / 2]);
            drop(reserved);
            if tape.index(2, "put after the drop") == 0 {
                held.push(None);
                continue;
            }
        }
        store.put(&cx, key(k), &bytes).map_err(harness("put"))?;
        // A key holds one value: a second put writes nothing.
        store.put(&cx, key(k), &bytes).map_err(harness("put again"))?;
        held.push(Some(bytes));
    }
    all_as_held(&cx, &mut store, &held, "before the seal")?;
    let packs_before = store.packs.len();
    store.seal(&cx).map_err(harness("seal"))?;
    all_as_held(&cx, &mut store, &held, "after the seal")?;
    let mut sorted: std::vec::Vec<(Key, &[u8])> = held.iter().enumerate().filter_map(|(k, bytes)| bytes.as_ref().map(|bytes| (key(k as u32), bytes.as_slice()))).collect();
    sorted.sort_by_key(|row| row.0);
    if sorted.is_empty() {
        if store.packs.len() != packs_before {
            return Err(oracle(std::format!("no record was committed and a pack was sealed")));
        }
    } else {
        let want = pack_bytes(sorted.iter().copied(), sorted.len()).map_err(harness("pack_bytes"))?;
        let number = store.packs.last().map(|(number, _)| *number).ok_or_else(|| oracle(std::format!("the seal took no pack")))?;
        let leaf = pack_name(number).map_err(harness("name"))?;
        let name = Path::new(&std::format!("{PACKS}/{}", leaf.as_str())).map_err(harness("path"))?;
        let mut on_disk = Bytes::new_in(cx.frame(), "the pack");
        root.read_all(&cx, &name, &mut on_disk).map_err(harness("read the pack"))?;
        if on_disk.as_slice() != want.as_slice() {
            return Err(oracle(std::format!("the sealed pack is not `pack_bytes` of its records ({} bytes for {})", on_disk.len(), want.len())));
        }
    }
    drop(store);
    let mut same_boot = PackStore::open_with(&cx, &root, boot, PACKS_MOST, StorePlant::None).map_err(harness("open again"))?;
    all_as_held(&cx, &mut same_boot, &held, "at a new open in the same boot")?;
    drop(same_boot);
    let mut next_boot = PackStore::open_with(&cx, &root, Boot::Known([2; 16]), PACKS_MOST, StorePlant::None).map_err(harness("open in the next boot"))?;
    if next_boot.refused != 0 {
        return Err(oracle(std::format!("the next boot's whole read refused {} pack", next_boot.refused)));
    }
    all_as_held(&cx, &mut next_boot, &held, "at an open in the next boot")
}

/// Expected: passes and prints
/// `store chunks: 300 cases: every record read back as put; each pack equals pack_bytes`.
#[test]
fn every_record_reads_back_as_put_and_the_pack_is_the_same_bytes() {
    sim_check(Config::cases(300), round_trip);
    println!("store chunks: 300 cases: every record read back as put; each pack equals pack_bytes");
}

/// (iii) on the records alone: a reservation dropped uncommitted leaves no row and gives its
/// bytes back; a committed one is held and read in place.
/// Expected: passes and prints
/// `a reservation dropped uncommitted: 0 rows, 0 bytes kept; committed: read back in place`.
#[test]
fn a_reservation_dropped_uncommitted_files_nothing() {
    let mut records = Records::new(64);
    {
        let mut reserved = records.reserve(key(1), 40).expect("room").expect("a new key");
        reserved.bytes()[..20].fill(7);
    }
    assert_eq!(records.count(), 0, "a dropped reservation filed a row");
    assert!(records.find(key(1)).is_none(), "a dropped reservation is held");
    assert_eq!(records.chunks[0].len(), 0, "a dropped reservation kept its bytes");
    assert!(records.put(key(2), &[9; 40]).expect("room"));
    assert_eq!((records.chunks.len(), records.chunks[0].len()), (1, 40), "the next record starts where the dropped one did");
    let mut reserved = records.reserve(key(1), 3).expect("room").expect("still a new key");
    reserved.bytes().copy_from_slice(b"abc");
    reserved.commit().expect("room");
    assert_eq!(records.find(key(1)), Some(&b"abc"[..]));
    assert_eq!(records.find(key(2)), Some(&[9u8; 40][..]));
    assert!(records.reserve(key(1), 3).expect("room").is_none(), "a held key was reserved again");
    assert_eq!(records.count(), 2);
    println!("a reservation dropped uncommitted: 0 rows, 0 bytes kept; committed: read back in place");
}

/// (iv) The counter: 200,000 puts of B bytes copy B bytes and no more, no record's bytes
/// ever change address, and the index's own moves (the table's doublings, 4 bytes a row) stay
/// under 8 bytes a put. The form this replaced moved 24 bytes of row for every row already
/// held, on average, at each put: this test's bound fails it by five orders of magnitude.
/// Expected: passes and prints a line starting
/// `bytes moved per byte put: 1.00 (200000 puts`.
#[test]
fn n_puts_move_each_byte_once() {
    const PUTS: u32 = 200_000;
    let scratch: std::vec::Vec<u8> = (0..1200usize).map(|i| i as u8).collect();
    let mut records = Records::new(CHUNK);
    let mut put = 0u64;
    let mut first: std::vec::Vec<*const u8> = std::vec::Vec::new();
    for k in 0..PUTS {
        let len = (k as usize * 131) % 1200;
        assert!(records.put(key(k), &scratch[..len]).expect("room"), "key {k} is new");
        put += len as u64;
        if k < 2000 {
            first.push(records.find(key(k)).expect("just put").as_ptr());
        }
    }
    let moved = records.moved();
    assert_eq!(moved.payload, put, "a put copied its record other than once");
    assert_eq!(moved.zeroed, 0);
    assert!(moved.index <= 8 * u64::from(PUTS), "the index moved {} bytes over {PUTS} puts", moved.index);
    for k in 0..PUTS {
        let len = (k as usize * 131) % 1200;
        let held = records.find(key(k)).expect("put");
        assert!(held == &scratch[..len], "key {k} read back as other bytes");
        if k < 2000 {
            assert_eq!(held.as_ptr(), first[k as usize], "key {k}'s bytes moved after their put");
        }
    }
    println!(
        "bytes moved per byte put: {:.2} ({PUTS} puts, {put} bytes; index bytes moved again: {}, {:.1} a put)",
        moved.payload as f64 / put as f64,
        moved.index,
        moved.index as f64 / f64::from(PUTS)
    );
}

/// The merge streams (store.rs `merge`; 2026-11-16: eight packs of 23 MB together passed the
/// store's frame and every command in that home failed at the merge). Four packs are sealed
/// and merged past `packs_most` = 2: the store's frame never holds more than a quarter of the
/// packs' bytes (it holds one 512-byte piece and the indexes), one pack is left, every record
/// reads back, the merged pack is byte for byte `pack_bytes` of the records, and the next
/// boot's whole read takes it.
fn merged(tape: &Tape) -> Result<(), Broke> {
    let io = Io::simulated(SimWorld::new(tape.below(1 << 30, "seed") as u64, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).map_err(harness("cx"))?;
    let root = io.need_fs().map_err(harness("fs"))?.root();
    let boot = Boot::Known([1; 16]);
    let mut store = PackStore::open_with(&cx, &root, boot, 2, StorePlant::None).map_err(harness("open"))?;
    let mut held: std::vec::Vec<Option<std::vec::Vec<u8>>> = std::vec::Vec::new();
    let mut total = 0usize;
    for _ in 0..4 {
        for _ in 0..3 {
            let k = held.len() as u32;
            let bytes = payload(k, 6000 + tape.index(6000, "a record's length"));
            store.put(&cx, key(k), &bytes).map_err(harness("put"))?;
            total += bytes.len();
            held.push(Some(bytes));
        }
        store.seal(&cx).map_err(harness("seal"))?;
    }
    if store.packs.len() != 4 {
        return Err(Broke::Harness(std::format!("{} packs before the merge, not 4", store.packs.len())));
    }
    let mut profile = super::super::Profile::default();
    store.after_answer(&cx, &mut profile).map_err(harness("the merge"))?;
    let peak = store.scratch.peak();
    if peak > total / 4 {
        return Err(oracle(std::format!("the merge's frame peaked at {peak} bytes for packs of {total}: it held the packs, not a piece")));
    }
    if store.packs.len() != 1 {
        return Err(oracle(std::format!("{} packs after the merge, not 1", store.packs.len())));
    }
    all_as_held(&cx, &mut store, &held, "after the merge")?;
    let mut sorted: std::vec::Vec<(Key, &[u8])> = held.iter().enumerate().filter_map(|(k, bytes)| bytes.as_ref().map(|bytes| (key(k as u32), bytes.as_slice()))).collect();
    sorted.sort_by_key(|row| row.0);
    let want = pack_bytes(sorted.iter().copied(), sorted.len()).map_err(harness("pack_bytes"))?;
    let leaf = pack_name(store.packs[0].0).map_err(harness("name"))?;
    let name = Path::new(&std::format!("{PACKS}/{}", leaf.as_str())).map_err(harness("path"))?;
    let mut on_disk = Bytes::new_in(cx.frame(), "the merged pack");
    root.read_all(&cx, &name, &mut on_disk).map_err(harness("read the pack"))?;
    if on_disk.as_slice() != want.as_slice() {
        return Err(oracle(std::format!("the merged pack is not `pack_bytes` of its records ({} bytes for {})", on_disk.len(), want.len())));
    }
    drop(store);
    let mut next_boot = PackStore::open_with(&cx, &root, Boot::Known([2; 16]), 2, StorePlant::None).map_err(harness("open in the next boot"))?;
    if next_boot.refused != 0 || next_boot.packs.len() != 1 {
        return Err(oracle(std::format!("the next boot refused {} pack and holds {}", next_boot.refused, next_boot.packs.len())));
    }
    all_as_held(&cx, &mut next_boot, &held, "at an open in the next boot after the merge")
}

/// Expected: passes and prints
/// `store merge: 50 cases: four packs merged through a frame under a quarter of their bytes; the pack equals pack_bytes`.
#[test]
fn a_merge_holds_a_piece_never_the_packs() {
    sim_check(Config::cases(50), merged);
    println!("store merge: 50 cases: four packs merged through a frame under a quarter of their bytes; the pack equals pack_bytes");
}
