//! The expansion records under power cuts and failures (rule 43: proven by a simulation, never
//! by review). Oracle: after any crash every record read is one that was written, whole. The
//! planted bug, a read that takes a file without its seal, must be caught.

use blaze_ids::ContentId;
use blaze_record::{Kind, Store};
use bstd::cx::Cx;
use bstd::io::Io;
use bstd::test::{Given, Seeds, seeded};

use super::{DirStore, FilesError, OnDir, RecordPlant};

const KEYS: u32 = 24;

fn key(k: u32) -> ContentId {
    ContentId::of("blaze-proc-macro record test", &k.to_le_bytes())
}

/// The longest value: past two sectors, so a cut can tear one.
const LONGEST: usize = 1400;

/// Key `k`'s one value (rule 21), written into `into`: from empty to past two sectors.
fn payload(k: u32, into: &mut [u8; LONGEST]) -> &[u8] {
    let len = (k as usize * 131) % LONGEST;
    for (i, byte) in into[..len].iter_mut().enumerate() {
        *byte = (k as usize * 7 + i * 13 + 1) as u8;
    }
    &into[..len]
}

/// Every key: a miss, or its whole value. A finding panics: it fails the seed (an error is a
/// restart, as a failed command is).
fn oracle(store: &DirStore) -> Result<(), FilesError> {
    let mut want = [0; LONGEST];
    for k in 0..KEYS {
        let want = payload(k, &mut want);
        if let Some(bytes) = store.get(Kind::List, &key(k))? {
            assert!(bytes == want, "record oracle: key {k} read back as other bytes ({} for {})", bytes.len(), want.len());
        }
    }
    Ok(())
}

/// One start: the oracle on what the folder holds, every put, the oracle again.
fn program(io: &Io, plant: RecordPlant) -> Result<(), FilesError> {
    let dir = io.need_fs().map_err(|_| FilesError::NoTree)?.root();
    let store = DirStore::on(OnDir { cx: io.spawner(), boot: io.boot().ok(), dir }, plant);
    oracle(&store)?;
    let mut value = [0; LONGEST];
    for k in 0..KEYS {
        store.put(Kind::List, key(k), payload(k, &mut value))?;
    }
    oracle(&store)
}

fn crashes(count: u32, plant: RecordPlant) -> Result<bstd::test::Passed, bstd::test::Failed> {
    seeded("blaze-proc-macro", module_path!(), "records_crash", Seeds::DEFAULT.from(0, count), Given::NOTHING, |_: &Cx, io: Io| {
        program(&io, plant)
    })
}

/// The invariant: after any crash or I/O failure during proc-macro expansion persistence, every record read back is whole and sealed or absent.
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
