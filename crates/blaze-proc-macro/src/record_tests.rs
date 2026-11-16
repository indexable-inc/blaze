//! The expansion records under power cuts and failures (rule 43: proven by a simulation, never
//! by review). Oracle: after any crash every record read is one that was written, whole. The
//! planted bug, a read that takes a file without its seal, must be caught.

use blaze_ids::ContentId;
use blaze_record::{Kind, Store};
use bstd::cx::Cx;
use bstd::io::Io;
use bstd::test::{Given, Seeds, seeded};

use super::{DirStore, OnDir, RecordPlant};

const KEYS: u32 = 24;

fn key(k: u32) -> ContentId {
    ContentId::of("blaze-proc-macro record test", &k.to_le_bytes())
}

/// Key `k`'s one value (rule 21): from empty to past two sectors, so a cut can tear one.
fn payload(k: u32) -> std::vec::Vec<u8> {
    let len = (k as usize * 131) % 1400;
    (0..len).map(|i| (k as usize * 7 + i * 13 + 1) as u8).collect()
}

/// Every key: a miss, or its whole value. A finding panics: it fails the seed (an error is a
/// restart, as a failed command is).
fn oracle(store: &DirStore<OnDir<'_>>) -> Result<(), std::string::String> {
    for k in 0..KEYS {
        match store.get(Kind::List, &key(k)) {
            Ok(Some(bytes)) => assert!(bytes == payload(k), "record oracle: key {k} read back as other bytes ({} for {})", bytes.len(), payload(k).len()),
            Ok(None) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

/// One start: the oracle on what the folder holds, every put, the oracle again.
fn program(cx: &Cx, io: &Io, plant: RecordPlant) -> Result<(), std::string::String> {
    let dir = io.need_fs().map_err(|missing| missing.to_string())?.root();
    let store = DirStore::on(OnDir { cx, dir }, plant);
    oracle(&store)?;
    for k in 0..KEYS {
        store.put(Kind::List, key(k), &payload(k)).map_err(|error| error.to_string())?;
    }
    oracle(&store)
}

fn crashes(count: u32, plant: RecordPlant) -> Result<bstd::test::Passed, bstd::test::Failed> {
    seeded("blaze-proc-macro", module_path!(), "records_crash", Seeds::DEFAULT.from(0, count), Given::NOTHING, |cx: &Cx, io: Io| {
        program(cx, &io, plant)
    })
}

#[test]
fn a_crash_or_failure_anywhere_keeps_each_record_whole_or_absent() {
    match crashes(1000, RecordPlant::None) {
        Ok(passed) => {
            // Proof the faults ran (rule 61).
            std::println!("records under crashes: {:?}", passed.counts);
            assert!(passed.counts.cuts > 500, "too few power cuts: {:?}", passed.counts);
            assert!(passed.counts.failures > 500, "too few failures: {:?}", passed.counts);
        }
        Err(failed) => panic!("{failed}"),
    }
}

#[test]
fn the_oracle_catches_a_read_that_skips_the_seal() {
    match crashes(1000, RecordPlant::SealSkipped) {
        Ok(passed) => panic!("the planted SealSkipped was not caught in {} starts", passed.starts),
        Err(failed) => {
            let said = std::format!("{failed}");
            std::println!("planted SealSkipped caught: {}", said.lines().next().unwrap_or(""));
            assert!(said.contains("record oracle"), "the planted SealSkipped failed otherwise: {said}");
        }
    }
}
