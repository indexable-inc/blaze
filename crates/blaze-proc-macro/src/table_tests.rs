//! A crate's results table under power cuts, failures and two commands writing it at once
//! (rule 43: proven by the simulation, never by review), and its bytes as a function of its
//! rows.
//!
//! Oracles: (1) every row of a table a command opens is a row some command kept, whole: its
//! key's own read list and answer; (2) a table kept and opened again with no fault between
//! holds every row kept; (3) with no power cut, no table is ever found torn: what lets a boot's
//! first check stand for the boot. Planted bugs: a read that skips the seal breaks (1); a
//! write under the table's own name before its bytes are whole breaks (3).

use blaze_ids::ContentId;
use bstd::cx::Cx;
use bstd::io::Io;
use bstd::test::{Fault, Given, Seeds, seeded};

use super::super::{DirStore, FilesError, OnDir, RecordError, RecordPlant};
use super::{CrateResults, Kept, Table, Whole, body_of, open};

const KEYS: u32 = 24;

fn krate() -> ContentId {
    ContentId::of("blaze-proc-macro table test crate", b"one")
}

fn key(k: u32) -> ContentId {
    ContentId::of("blaze-proc-macro table test", &k.to_le_bytes())
}

/// Key `k`'s read list: none, or one absent variable (three distinct lists over the keys).
fn list(k: u32) -> Vec<u8> {
    let mut out = Vec::new();
    match k % 3 {
        0 => out.extend_from_slice(&0u32.to_le_bytes()),
        n => {
            let subject = if n == 1 { "env:ONE" } else { "env:OTHER" };
            out.extend_from_slice(&1u32.to_le_bytes());
            out.extend_from_slice(&(subject.len() as u32).to_le_bytes());
            out.extend_from_slice(subject.as_bytes());
            out.push(0);
        }
    }
    out
}

/// Key `k`'s one answer (rule 21): from empty to past two sectors, so a cut can tear a table.
fn answer(k: u32) -> Vec<u8> {
    (0..(k as usize * 131) % 1400).map(|i| (k as usize * 7 + i * 13 + 1) as u8).collect()
}

/// Oracle (1): each row of `table` is its key's own. A finding panics: it fails the seed.
fn rows_are_kept_rows(table: &Table) {
    for at in 0..table.len() {
        let row = table.row(at);
        let k = (0..KEYS).find(|k| key(*k).0[..] == *row.key);
        let Some(k) = k else { panic!("table oracle: row {at} has a key no command kept") };
        assert!(table.list(row.list) == list(k), "table oracle: key {k} read back with another read list");
        assert!(row.answer == answer(k), "table oracle: key {k} read back as other bytes ({} for {})", row.answer.len(), answer(k).len());
    }
}

/// The crate's results opened as a command does, the oracles on what it found.
fn opened(store: &DirStore, cuts: bool) -> Result<CrateResults, RecordError> {
    let results = match open(store, krate()) {
        Err(RecordError::Table { what, .. }) => panic!("table oracle: a table taken as whole is no table: {what}"),
        other => other?,
    };
    rows_are_kept_rows(results.table());
    if !cuts {
        assert!(store.torn_and_hashed().0 == 0, "table oracle: a torn table under its name with no power cut");
    }
    Ok(results)
}

/// One start: two commands open the crate's results at once, each runs the keys of its half
/// that the table lacks, both keep (in an order the table's size picks: the second's table
/// replaces the first's); then a third runs every key and keeps, and a fourth finds them all.
fn program(io: &Io, plant: RecordPlant, cuts: bool) -> Result<(), RecordError> {
    let dir = io.need_fs().map_err(|_| RecordError::Files(FilesError::NoTree))?.root();
    // clone: the world, a refcount.
    let store = DirStore::on(OnDir { io: io.clone(), dir }, plant);
    let (a, b) = (opened(&store, cuts)?, opened(&store, cuts)?);
    for k in 0..KEYS {
        let into = if k % 2 == 0 { &a } else { &b };
        into.insert_bytes(&store, key(k), list(k), answer(k))?;
    }
    let (first, second) = if a.table().len() % 2 == 0 { (&a, &b) } else { (&b, &a) };
    first.keep(&store)?;
    second.keep(&store)?;
    let third = opened(&store, cuts)?;
    for k in 0..KEYS {
        third.insert_bytes(&store, key(k), list(k), answer(k))?;
    }
    third.keep(&store)?;
    // Nothing more to keep: a second keep writes nothing.
    assert!(third.keep(&store)? == Kept::Nothing, "table oracle: a keep with no new row wrote");
    let fourth = opened(&store, cuts)?;
    // Oracle (2). A power cut does not return here: the program is killed and started again.
    assert!(fourth.table().len() == KEYS as usize, "table oracle: a kept table holds {} of {KEYS} rows", fourth.table().len());
    Ok(())
}

fn runs(count: u32, plant: RecordPlant, cuts: bool) -> Result<bstd::test::Passed, bstd::test::Failed> {
    let seeds = Seeds::DEFAULT.from(0, count);
    let seeds = if cuts { seeds } else { seeds.without(Fault::Crashes) };
    seeded("blaze-proc-macro", module_path!(), "tables_crash", seeds, Given::NOTHING, move |_: &Cx, io: Io| program(&io, plant, cuts))
}

#[test]
fn a_crash_or_failure_anywhere_keeps_each_table_whole_or_absent() {
    match runs(1000, RecordPlant::None, true) {
        Ok(passed) => {
            // Proof the faults ran (rule 61).
            std::println!("tables under crashes: {:?}", passed.counts);
            assert!(passed.counts.cuts > 500, "too few power cuts: {:?}", passed.counts);
            assert!(passed.counts.failures > 500, "too few failures: {:?}", passed.counts);
        }
        Err(failed) => panic!("{failed}"),
    }
}

#[test]
fn with_no_power_cut_no_table_is_ever_torn() {
    match runs(1000, RecordPlant::None, false) {
        Ok(passed) => {
            std::println!("tables under failures alone: {:?}", passed.counts);
            assert!(passed.counts.failures > 500, "too few failures: {:?}", passed.counts);
        }
        Err(failed) => panic!("{failed}"),
    }
}

#[test]
fn the_oracle_catches_a_read_that_skips_the_tables_seal() {
    match runs(1000, RecordPlant::SealSkipped, true) {
        Ok(passed) => panic!("the planted SealSkipped was not caught in {} starts", passed.starts),
        Err(failed) => {
            let said = std::format!("{failed}");
            std::println!("planted SealSkipped caught: {}", said.lines().next().unwrap_or(""));
            assert!(said.contains("table oracle"), "the planted SealSkipped failed otherwise: {said}");
        }
    }
}

#[test]
fn the_oracle_catches_a_name_given_before_the_bytes_are_whole() {
    match runs(1000, RecordPlant::RenameFirst, false) {
        Ok(passed) => panic!("the planted RenameFirst was not caught in {} starts", passed.starts),
        Err(failed) => {
            let said = std::format!("{failed}");
            std::println!("planted RenameFirst caught: {}", said.lines().next().unwrap_or(""));
            assert!(said.contains("table oracle: a torn table"), "the planted RenameFirst failed otherwise: {said}");
        }
    }
}

/// The body of keys `order`'s rows, in that order of arrival.
fn body(order: &[u32]) -> Vec<u8> {
    let owned: Vec<(ContentId, Vec<u8>, Vec<u8>)> = order.iter().map(|k| (key(*k), list(*k), answer(*k))).collect();
    let mut rows: Vec<(&[u8], &[u8], &[u8])> = owned.iter().map(|(key, list, answer)| (&key.0[..], &list[..], &answer[..])).collect();
    body_of(&krate(), &mut rows).unwrap_or_else(|error| panic!("{error}"))
}

#[test]
fn a_table_is_a_function_of_its_rows_and_finds_each_by_its_key() {
    let forward: Vec<u32> = (0..KEYS).collect();
    let backward: Vec<u32> = (0..KEYS).rev().collect();
    let bytes = body(&forward);
    assert!(bytes == body(&backward), "two orders of arrival wrote two tables");
    let table = Table::of(Whole { body: bytes.clone() }).unwrap_or_else(|what| panic!("its own body is no table: {what}"));
    assert_eq!(table.len(), KEYS as usize);
    assert_eq!(table.lists, 3, "the keys' three distinct read lists, each once");
    for k in 0..KEYS {
        let found = table.rows_of(&key(k));
        assert_eq!(found.len(), 1, "key {k}");
        let row = table.row(found.start);
        assert!(row.answer == answer(k) && table.list(row.list) == list(k), "key {k} found as another row");
    }
    assert!(table.rows_of(&key(KEYS)).is_empty(), "a key never kept was found");
    std::println!("table: {} rows, {} lists, {} bytes; each key found by a binary search", table.len(), table.lists, bytes.len());
    // A body cut short anywhere is refused, never read past its end.
    let mut refused = 0;
    for cut in 0..bytes.len() {
        if Table::of(Whole { body: bytes[..cut].to_vec() }).is_err() {
            refused += 1;
        }
    }
    assert_eq!(refused, bytes.len(), "a cut body was taken as a table");
    std::println!("table: all {refused} cuts of the body refused");
}
