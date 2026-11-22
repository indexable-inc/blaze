//! An index filed BESIDE its owner ([`Plain::index_beside`]) on the deterministic simulation
//! (rules 43, 64): a root that names an owner's beside index keeps the records it was put with
//! (a names record's expansion records, blaze-cli engine_store.rs `put_all`) through cleanup
//! slices that sweep the segment they lie in; the writer of that root finds the index from the
//! owner's key alone ([`beside_key`]).
//!
//! One command puts the owner, its sixteen records beside it and dead records in one segment,
//! files the beside index and sets the root; later commands fold slices past every lease and
//! grace. The oracle: the owner and every record beside it read back; the dead are swept.
//!
//! The control (rule 23): the root names only an index of the owner, not the one beside it, and
//! the records beside the owner are swept while the root lives.

use std::{alloc::Global, time::Duration};

use blaze_core::sim::{Faults, Sim};
use blaze_mem::Frame;

use blaze_keyed::kind::ReservedTag;

use super::core::{GRACE_NS, LEASE_NS, Plant, ROOT_LIFE_NS, beside_key};
use super::index_tests::{World, key, readable};
use super::test_root;
use crate::Key;

const MINUTE: Duration = Duration::from_secs(60);
/// The owner's record.
const OWNER: u32 = 0;
/// The records put beside it.
const BESIDE: core::ops::Range<u32> = 1..17;
/// Dead records in the same segment, long (index_tests.rs `value`), so the segment is mostly dead.
const DEAD: core::ops::Range<u32> = 20_000..20_064;

/// What the history showed: each claim broken. `beside` false is the control.
fn history(beside: bool, root: &Frame) -> Result<Vec<String>, String> {
    let mut w = World { sim: Sim::new(53, Faults::default()), next: 0, plant: Plant::None, root };
    let mut problems = Vec::new_in(Global);
    let mut puts: Vec<u32> = core::iter::once(OWNER).chain(BESIDE).chain(DEAD).collect();
    puts.sort_unstable();
    let mut filed = None;
    w.command(
        &puts,
        |store| {
            let extras: Vec<Key> = BESIDE.map(key).collect();
            let at = store.index_beside(key(OWNER), &extras).map_err(|why| format!("the index beside: {why}"))?;
            let own = store.index(&[key(OWNER)]).map_err(|why| format!("the owner's index: {why}"))?;
            let indexes: Vec<Key> = if beside { [own, at].into_iter().collect() } else { [own].into_iter().collect() };
            store.root_through("answer", &indexes, ROOT_LIFE_NS).map_err(|why| format!("the root: {why}"))?;
            filed = Some(at);
            Ok(())
        },
        false,
    )?;
    if filed != Some(beside_key(key(OWNER))) {
        problems.push(format!("the index beside was filed under {filed:?}, not the owner's beside key"));
    }
    {
        let r = w.open(Plant::None)?;
        match r.get(&beside_key(key(OWNER))) {
            Some(got) if ReservedTag::Index.is(got.tag) &&got.bytes.len() == BESIDE.len() * 32 => {}
            other => problems.push(format!("the index beside does not read as an index of {} keys: {:?}", BESIDE.len(), other.map(|it| (it.tag, it.bytes.len())))),
        }
    }
    for g in 0..6u32 {
        let garbage: Vec<u32> = (30_000 + g * 32..30_000 + (g + 1) * 32).collect();
        w.command(&garbage, |_| Ok(()), true)?;
        w.sim.advance(MINUTE);
    }
    w.sim.advance(Duration::from_nanos(LEASE_NS.max(GRACE_NS)) + MINUTE);
    for _ in 0..4 {
        w.command(&[], |_| Ok(()), true)?;
        w.sim.advance(Duration::from_secs(1));
    }
    let left = readable(&mut w, core::iter::once(OWNER).chain(BESIDE))?;
    if left.len() != 1 + BESIDE.len() {
        problems.push(format!("{} of the owner and its {} records beside it read back after the sweep", left.len(), BESIDE.len()));
    }
    let dead = readable(&mut w, DEAD)?;
    if !dead.is_empty() {
        problems.push(format!("{} dead records were not swept: no slice ran", dead.len()));
    }
    Ok(problems)
}

/// Asserts: a root naming an owner's beside index keeps the owner and every record beside it
/// through the slices that sweep their segment; the dead around them are swept.
/// Prints "beside: the owner and its 16 records kept through the sweep".
#[test]
fn an_index_beside_its_owner_keeps_what_was_put_with_it() {
    let root = test_root();
    let problems = history(true, &root).expect("the history runs");
    assert!(problems.is_empty(), "{problems:?}");
    std::println!("beside: the owner and its {} records kept through the sweep", BESIDE.len());
}

/// The control (rule 23): the root names the owner but not its beside index, so the records
/// beside it are swept, and the oracle says so.
/// Prints "beside control: no index beside named: <the oracle's finding>".
#[test]
fn a_root_without_the_index_beside_loses_what_was_put_with_it() {
    let root = test_root();
    let problems = history(false, &root).expect("the history runs");
    assert!(problems.iter().any(|it| it.contains("read back after the sweep")), "the missing index beside was not caught: {problems:?}");
    std::println!("beside control: no index beside named: {}", problems[0]);
}
