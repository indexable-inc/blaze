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
use super::super::SEAL_LEN;
use super::{CrateResults, Kept, Table, Whole, file_of};

const KEYS: u32 = 24;

fn krate() -> ContentId {
    ContentId::of("blaze-proc-macro table test crate", b"one")
}

fn key(k: u32) -> ContentId {
    ContentId::of("blaze-proc-macro table test", &k.to_le_bytes())
}

/// `bytes` as a pending row holds them: one block of their length in the test's heap.
fn held(bytes: Vec<u8>) -> super::Bytes {
    let mut out = super::Bytes::with_capacity_in(bytes.len(), blaze_own::testing::heap().shared());
    out.extend_from_slice(&bytes);
    out
}

/// The crate's results opened as a command opens them, its rows in the test's heap.
fn open(store: &DirStore, krate: ContentId) -> Result<CrateResults, RecordError> {
    super::open(store, krate, blaze_own::testing::heap().shared())
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
    loose((0..(k as usize * 131) % 1400).map(|i| (k as usize * 7 + i * 13 + 1) as u8).collect())
}

/// A loose answer ([`super::answer_into`]'s form) that holds `payload`: no token, no
/// diagnostic, and `payload` as the diagnostics' text, which a table keeps as it lies.
fn loose(payload: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 12);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

/// Oracle (1): each row of `table` is its key's own. A finding panics: it fails the seed.
fn rows_are_kept_rows(table: &Table) {
    for at in 0..table.len() {
        let row = table.row(at);
        let k = (0..KEYS).find(|k| key(*k).0[..] == *row.key);
        let Some(k) = k else { panic!("table oracle: row {at} has a key no command kept") };
        assert!(table.list(row.list) == list(k), "table oracle: key {k} read back with another read list");
        assert!(table.loose_answer(at) == Ok(answer(k)), "table oracle: key {k} read back as other bytes ({} for {})", row.answer.len(), answer(k).len());
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
    let store = DirStore::on(OnDir { cx: io.spawner(), boot: io.boot().ok(), dir }, plant);
    let (a, b) = (opened(&store, cuts)?, opened(&store, cuts)?);
    for k in 0..KEYS {
        let into = if k % 2 == 0 { &a } else { &b };
        into.insert_bytes(&store, key(k), held(list(k)), held(answer(k)))?;
    }
    let (first, second) = if a.table().len() % 2 == 0 { (&a, &b) } else { (&b, &a) };
    first.keep(&store, &[])?;
    second.keep(&store, &[])?;
    let third = opened(&store, cuts)?;
    for k in 0..KEYS {
        third.insert_bytes(&store, key(k), held(list(k)), held(answer(k)))?;
    }
    third.keep(&store, &[])?;
    // Nothing more to keep: a second keep writes nothing.
    assert!(third.keep(&store, &[])? == Kept::Nothing, "table oracle: a keep with no new row wrote");
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

/// The invariant: under power cuts, failures, and concurrent writers, every opened proc-macro results table is sealed and whole or absent, and never published before its bytes are whole.
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
    let mut rows: Vec<(&[u8], &[u8], super::Answer<'_>)> = owned.iter().map(|(key, list, answer)| (&key.0[..], &list[..], super::Answer::Loose(&answer[..]))).collect();
    let mut file = file_of(&krate(), None, &mut rows).unwrap_or_else(|error| panic!("{error}"));
    file.truncate(file.len() - SEAL_LEN);
    file
}

/// A fresh folder under the system's temporary one and the store over it.
fn store_on_disk(name: &str, plant: RecordPlant) -> (std::path::PathBuf, DirStore) {
    let dir = std::env::temp_dir().join(std::format!("blaze-proc-macro-table-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a temporary folder");
    let (io, root) = super::super::native_world(&dir).unwrap_or_else(|error| panic!("the world over {}: {error}", dir.display()));
    (dir, DirStore::on(OnDir { cx: io.spawner(), boot: io.boot().ok(), dir: root }, plant))
}

/// Row `k` of the large table: 1.6 MB that no other row holds.
fn large_answer(k: u32) -> Vec<u8> {
    loose((0..LARGE_ANSWER).map(|i| (i as u32).wrapping_mul(2_654_435_761).wrapping_add(k.wrapping_mul(40_503)).to_le_bytes()[1]).collect())
}

const LARGE_ROWS: u32 = 72;
const LARGE_ANSWER: usize = 1_600_000;

/// 72 rows of 1.6 MB (115 MB) kept by one command under `plant`, then opened by another.
fn large_table(name: &str, plant: RecordPlant) -> Result<(usize, usize), RecordError> {
    let (dir, store) = store_on_disk(name, plant);
    let run = || -> Result<(usize, usize), RecordError> {
        let first = open(&store, krate())?;
        for k in 0..LARGE_ROWS {
            first.insert_bytes(&store, key(k), held(list(k)), held(large_answer(k)))?;
        }
        let Kept::Written { bytes, .. } = first.keep(&store, &[])? else { panic!("a command with new rows wrote nothing") };
        let again = open(&store, krate())?;
        for k in 0..LARGE_ROWS {
            let found = again.table().rows_of(&key(k));
            assert_eq!(found.len(), 1, "row {k} of the large table");
            assert!(again.table().loose_answer(found.start) == Ok(large_answer(k)), "row {k} of the large table read back as other bytes");
        }
        Ok((bytes, again.table().len()))
    };
    let ran = run();
    let _ = std::fs::remove_dir_all(&dir);
    ran
}

/// A crate's results are sized by no fixed block: a table of a hundred megabytes (more than
/// the task's 64 MB frame) is published as it lies and read back into a buffer of its own
/// length, every row its own bytes.
#[test]
fn a_table_of_a_hundred_megabytes_is_kept_and_read_back() {
    let (bytes, rows) = large_table("large", RecordPlant::None).unwrap_or_else(|error| panic!("{error}"));
    assert!(bytes > 100 << 20 && rows == LARGE_ROWS as usize, "{bytes} bytes, {rows} rows");
    std::println!("large table: {rows} rows, {bytes} bytes kept and read back");
}

/// The cause, put back (rule 23): the same table published through the task's frame, as
/// the store did before, is refused by the frame.
#[test]
fn control_a_publish_through_the_tasks_frame_refuses_a_large_table() {
    match large_table("large-framed", RecordPlant::FramedPublish) {
        Err(RecordError::Files(error @ FilesError::Stopped { .. })) => std::println!("planted FramedPublish refused: {error}"),
        Err(other) => panic!("the planted FramedPublish failed otherwise: {other}"),
        Ok((bytes, _)) => panic!("the planted FramedPublish kept a table of {bytes} bytes"),
    }
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
        assert!(table.loose_answer(found.start) == Ok(answer(k)) && table.list(row.list) == list(k), "key {k} found as another row");
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

/// An answer with every kind of token, origins in runs of both strides and of one token, and
/// two diagnostics: the loose form [`super::answer_into`] writes.
fn rich_answer(source: &str) -> Vec<u8> {
    use blaze_expand::{Edition, ProcDiagnostic as D, ProcLevel, Produced, Reads, Text, TokenOrigin as O, Transparency};
    let alloc = blaze_own::Alloc::global();
    let tokens = blaze_expand::lex(source, Edition::E2021, alloc).expect("the source lexes");
    let mut origins = blaze_own::Vec::new_in(alloc);
    // Input 3, 4, 5 (stride 1), one made token, the same definition token twice (stride 0),
    // then the rest made: a run of one, a run that steps, a run that stays, a long run.
    let pattern = [O::Input(3), O::Input(4), O::Input(5), O::Made(Transparency::Opaque), O::Def(7), O::Def(7), O::AttrArgs(0)];
    for at in 0..tokens.len() {
        origins.push(pattern.get(at).copied().unwrap_or(O::Made(Transparency::SemiOpaque)));
    }
    let mut diagnostics = blaze_own::Vec::new_in(alloc);
    diagnostics.push(D::Emitted { level: ProcLevel::Warning, message: Text { at: 0, len: 4 }, at: Some(O::Input(4)) });
    diagnostics.push(D::Panicked { message: Text { at: 4, len: 3 } });
    let mut text = blaze_own::Vec::new_in(alloc);
    text.extend_from_slice(b"warnbad");
    let mut out = Vec::new();
    super::answer_into(&mut out, &Produced { tokens, origins, reads: Reads::new_in(alloc), diagnostics, text });
    out
}

/// A kept answer read back through the dense form means what the run gave: its tokens, each
/// one's origin, its diagnostics and their text, compared in the loose form; and a table
/// written from found (dense) rows and run (loose) rows is the table of the same rows all
/// run. Expected line: `table: 2 answers of 3 distinct forms read back; found and run rows wrote one table`.
#[test]
fn a_dense_answer_read_back_is_the_answer_that_was_run() {
    let alloc = blaze_own::Alloc::global();
    let sources = ["pub fn r#a ( x : u8 ) -> & 'static str { \"h\u{e9}\" . len ( ) + 1u8 as usize ; a :: b ( 1.5f32 , b'c' ) }", "struct S ; impl S { fn f ( & self ) { } }"];
    let owned: Vec<(ContentId, Vec<u8>, Vec<u8>)> = sources.iter().zip(0u32..).map(|(source, k)| (key(k), list(k), rich_answer(source))).collect();
    let all_run = |upto: usize| {
        let mut rows: Vec<(&[u8], &[u8], super::Answer<'_>)> = owned[..upto].iter().map(|(key, list, answer)| (&key.0[..], &list[..], super::Answer::Loose(&answer[..]))).collect();
        let mut file = file_of(&krate(), None, &mut rows).unwrap_or_else(|error| panic!("{error}"));
        file.truncate(file.len() - SEAL_LEN);
        file
    };
    let both = all_run(2);
    let table = Table::of(Whole { body: both.clone() }).unwrap_or_else(|what| panic!("its own body is no table: {what}"));
    for (key, list, answer) in &owned {
        let found = table.rows_of(key);
        assert_eq!(found.len(), 1);
        let row = table.row(found.start);
        assert!(row.answer.len() < answer.len(), "the dense answer ({} bytes) is not below the loose one ({})", row.answer.len(), answer.len());
        let produced = super::dense_produced(&table.atoms(), list, row.answer, blaze_expand::Edition::E2021, alloc).unwrap_or_else(|what| panic!("the kept answer is refused: {what:?}"));
        let mut back = Vec::new();
        super::answer_into(&mut back, &produced);
        assert!(back == *answer, "a kept answer read back is not the answer that was run");
        assert!(table.loose_answer(found.start).as_ref() == Ok(answer), "a kept answer loosened is not the answer that was run");
    }
    // The first row found in a table of its own, the second run now: the same table.
    let first = Table::of(Whole { body: all_run(1) }).unwrap_or_else(|what| panic!("its own body is no table: {what}"));
    let mut rows: Vec<(&[u8], &[u8], super::Answer<'_>)> =
        vec![(&owned[1].0.0[..], &owned[1].1[..], super::Answer::Loose(&owned[1].2[..])), (first.row(0).key, first.list(first.row(0).list), super::Answer::Kept(first.row(0).answer))];
    let mut mixed = file_of(&krate(), Some(&first.atoms()), &mut rows).unwrap_or_else(|error| panic!("{error}"));
    mixed.truncate(mixed.len() - SEAL_LEN);
    assert!(mixed == both, "found and run rows wrote another table than the same rows all run");
    std::println!("table: 2 answers of 3 distinct forms read back; found and run rows wrote one table");
}
