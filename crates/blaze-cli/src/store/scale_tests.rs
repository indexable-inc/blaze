//! The store at the size of a real home (2026-11-16: ten packs of 23 to 24 MB in
//! `~/.blaze/v2/shared` failed every command once their sum passed the 64 MiB frame), in the
//! deterministic simulation: what a get reads, what a repeated command writes, and the
//! compaction's crash proof with a planted bug.
//!
//! Expected lines (`LANECHECK_WAIT=540 bash .../lanecheck.sh blaze-cli test store`):
//! - `one_get_reads_its_record_and_the_indexes_only` prints
//!   "bytes read for one record of 960 MB of packs: 8398848" (40 packs of three 8 MiB records:
//!   the record, 8388608, and each pack's footer twice and its three rows, 40 × 256 = 10240;
//!   nothing else) and passes.
//! - `forty_packs_past_the_frame_merge_into_one` prints "merged 40 packs of 80 MiB under a
//!   64 MiB frame into 1" and passes (before the streamed merge: "arena the packs' records: ...
//!   would pass its limit").
//! - `two_equal_commands_write_one_pack` prints "packs after two equal commands: 1" and passes
//!   (with `Layered::put` asking only the record's own layer: 2).
//! - `compaction_of_forty_packs_survives_a_crash_anywhere` passes (power cuts and failures
//!   counted past 300 each); `the_compaction_oracle_catches_a_merge_that_removes_first` passes,
//!   the planted `MergeRemovesFirst` caught with "is lost".

use blaze_drive::profile::Profile;
use blaze_drive::Key;
use blaze_ids::ContentId;
use bstd::cx::Cx;
use bstd::fs::{Content, Dir, Durable, FsKind, ReadWrite};
use bstd::io::{Io, Limits, SimWorld};
use bstd::mem::{Bytes, TASK_FRAME};
use bstd::path::Path;
use bstd::test::{BUDGET, Given, Seeds, seeded};

use super::{BOOT, BOOT_MAGIC, Boot, CliError, FOOTER, Layered, Machine, PACKS, PACKS_MOST, PackStore, Place, ROW, SHARED, Sharing, Store, StorePlant, index_number, le_u64, pack_bytes, pack_name};

const MIB: usize = 1 << 20;

fn sim(seed: u64) -> (Io, Cx) {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    (io, cx)
}

fn big_key(pack: u32, record: u32) -> Key {
    let mut at = [0u8; 8];
    at[..4].copy_from_slice(&pack.to_le_bytes());
    at[4..].copy_from_slice(&record.to_le_bytes());
    Key(ContentId::of("blaze-cli store scale test", &at))
}

/// Record (`pack`, `record`)'s one value: `len` bytes of one byte drawn from both.
fn big_payload(pack: u32, record: u32, len: usize) -> std::vec::Vec<u8> {
    std::vec![(pack.wrapping_mul(31).wrapping_add(record.wrapping_mul(7)) % 251) as u8 + 1; len]
}

/// `packs` packs of `records` records of `len` bytes each, written as the seal writes them
/// (the layout's statement, `pack_bytes`), in `folder` under `root`, and a boot record naming
/// them all as known whole in `boot`: the open then reads only footers and indexes.
fn plant_home(cx: &Cx, root: &Dir<ReadWrite>, folder: &str, boot: [u8; 16], packs: u32, records: u32, len: usize) -> std::vec::Vec<u64> {
    let dir = root.subdir(&Path::new(folder).expect("the folder"));
    let mut numbers = std::vec::Vec::new();
    for pack in 0..packs {
        let mut held: std::vec::Vec<(Key, std::vec::Vec<u8>)> = (0..records).map(|record| (big_key(pack, record), big_payload(pack, record, len))).collect();
        held.sort_by_key(|(key, _)| *key);
        let bytes = pack_bytes(held.iter().map(|(key, bytes)| (*key, bytes.as_slice())), held.len()).expect("a pack");
        let index_at = le_u64(&bytes[bytes.len() - FOOTER + 16..]) as usize;
        let number = index_number(&bytes[index_at..]);
        dir.publish(cx, &pack_name(number).expect("a name"), Content::bytes(&bytes), Durable::No).expect("a pack published");
        numbers.push(number);
    }
    numbers.sort_unstable();
    let mut boot_record = std::vec::Vec::from(*BOOT_MAGIC);
    boot_record.extend_from_slice(&boot);
    for number in numbers.iter() {
        boot_record.extend_from_slice(&number.to_le_bytes());
    }
    dir.publish(cx, &Path::new(BOOT).expect("boot"), Content::bytes(&boot_record), Durable::No).expect("the boot record");
    numbers
}

/// (i) A home of 40 packs of 24 MiB (three 8 MiB records each: 960 MiB) under a command
/// frame of 64 MiB: the open reads each pack's footer and index (two positioned reads, 48-byte
/// rows), and a get reads its record's bytes with one positioned read into the asker's buffer,
/// nothing else. With the packs read whole (the old merge's way, or a get that reads its pack)
/// the count is past 24 MiB and the assertion fails. A map effect (bstd has none today) would
/// take the index rows in place, saving the copy of 48 bytes a row into the command's table:
/// a named piece (std.md, piece 8).
#[test]
fn one_get_reads_its_record_and_the_indexes_only() {
    let (io, cx) = sim(11);
    let root = io.need_fs().expect("a file system").root();
    let boot = [7u8; 16];
    let (packs, records, len) = (40u32, 3u32, 8 * MIB);
    plant_home(&cx, &root, PACKS, boot, packs, records, len);
    let mut store = PackStore::open_with(&cx, &root, Boot::Known(boot), PACKS_MOST, StorePlant::None).expect("the open");
    assert_eq!((store.packs.len(), store.verified), (packs as usize, 0), "every pack taken, none read whole");
    let mut into = Bytes::new_in(cx.frame(), "one record of the scale test");
    assert!(store.get_into(&cx, big_key(23, 1), &mut into).expect("the get"), "the record is held");
    assert!(into.as_slice() == big_payload(23, 1, len).as_slice(), "the record's own bytes");
    let indexes = u64::from(packs) * (2 * FOOTER + records as usize * ROW) as u64;
    std::println!("bytes read for one record of 960 MB of packs: {}", store.read);
    assert_eq!(store.read, indexes + len as u64, "the record and each pack's footer and index, nothing more");
}

/// The reported failure itself: packs whose sum passes the store's 64 MiB frame (40 of 2 MiB,
/// 80 MiB) are merged into one by the streamed copy, and every record reads back whole from
/// the merged pack. The merge before 2026-11-16 read every pack into one scratch of the frame
/// and failed here with "arena the packs' records: ... would pass its limit".
#[test]
fn forty_packs_past_the_frame_merge_into_one() {
    let (io, cx) = sim(12);
    let root = io.need_fs().expect("a file system").root();
    let boot = [8u8; 16];
    let (packs, records, len) = (40u32, 2u32, MIB);
    plant_home(&cx, &root, PACKS, boot, packs, records, len);
    let mut store = PackStore::open_with(&cx, &root, Boot::Known(boot), PACKS_MOST, StorePlant::None).expect("the open");
    let mut profile = Profile::default();
    store.after_answer(&cx, &mut profile).expect("the merge after the answer");
    assert_eq!(store.packs.len(), 1, "one pack after the merge");
    let listed = root.entries(&cx, &Path::new(PACKS).expect("packs")).expect("a listing").expect("the folder");
    let on_disk = (0..listed.len()).filter(|i| listed.name(*i).len() == 16).count();
    assert_eq!(on_disk, 1, "the old packs removed after the merged one was renamed in");
    for pack in 0..packs {
        for record in 0..records {
            let mut into = Bytes::new_in(cx.frame(), "a merged record");
            assert!(store.get_into(&cx, big_key(pack, record), &mut into).expect("the get"), "record ({pack}, {record}) held after the merge");
            assert!(into.as_slice() == big_payload(pack, record, len).as_slice(), "record ({pack}, {record}) whole after the merge");
        }
    }
    std::println!("merged {packs} packs of {} MiB under a {} MiB frame into {}", (packs as usize * records as usize * len) / MIB, TASK_FRAME / MIB, store.packs.len());
}

/// Every record public, or every record the user's own: the place a record goes differs
/// between two commands (as between a debug and a release build's routing), the key does not.
struct Everything(Place);

impl Sharing for Everything {
    fn add_public(&mut self, _subjects: &[ContentId]) {}

    fn place(&self, _bytes: &[u8]) -> Place {
        self.0
    }
}

/// (ii) The same command twice through the chain (the checkout's store, the public machine
/// store, the user's own) writes one pack: the second finds every key held, the first time
/// in the public layer and the second routed to the user's own, and puts nothing. With
/// `Layered::put` asking only the layer a record is routed to, the second command seals a
/// second pack in `v2/own` and the count is 2.
#[test]
fn two_equal_commands_write_one_pack() {
    let (io, cx) = sim(13);
    let fs = io.need_fs().expect("a file system");
    let root = fs.root().subdir(&Path::new("work/project").expect("the project"));
    let home = match Machine::of(&cx, &fs, Some("/home/sim")).expect("the home") {
        Machine::Home(home) => home,
        Machine::Off(why) => panic!("the simulation's home was refused: {why}"),
    };
    let err = io.need_err().expect("stderr");
    let boot = Boot::Known([9; 16]);
    for place in [Place::Public, Place::Own] {
        let mut store = Layered::open_chain(&cx, &root, Some(&home), boot, Everything(place), PACKS_MOST, StorePlant::None, &err, None).expect("the chain");
        for record in 0..16u32 {
            let mut into = Bytes::new_in(cx.frame(), "a record asked first");
            if !store.get_into(&cx, big_key(0, record), &mut into).expect("the get") {
                store.put(&cx, big_key(0, record), &big_payload(0, record, 3000 + record as usize)).expect("the put");
            }
            // The driver puts what it made even when a get found it (a record made beside its
            // get): the put of a held key must write nothing.
            store.put(&cx, big_key(0, record), &big_payload(0, record, 3000 + record as usize)).expect("the put again");
        }
        store.seal(&cx).expect("the seal");
        let mut profile = Profile::default();
        store.after_answer(&cx, &mut profile).expect("after the answer");
    }
    let mut packs = 0usize;
    for (dir, folder) in [(&root, PACKS), (&home, SHARED), (&home, super::OWN)] {
        match dir.entries(&cx, &Path::new(folder).expect("a folder")) {
            Ok(Some(listed)) => packs += (0..listed.len()).filter(|i| listed.name(*i).len() == 16).count(),
            Ok(None) => {}
            Err(error) if error.kind == FsKind::NotFound => {}
            Err(error) => panic!("listing {folder}: {error}"),
        }
    }
    std::println!("packs after two equal commands: {packs}");
    assert_eq!(packs, 1, "the second command wrote nothing");
}

/// The compaction's keys: 40 packs of two records each.
const FORTY: u32 = 40;
const WITNESS: &str = "compaction-witness";

fn small(k: u32) -> std::vec::Vec<u8> {
    big_payload(k, k, (k as usize * 37) % 300)
}

fn boot_of(io: &Io) -> Boot {
    let cuts = io.with_sim(|sim| sim.counts().cuts).unwrap_or(0);
    let mut boot = [0u8; 16];
    boot[..8].copy_from_slice(&cuts.to_le_bytes());
    boot[8] = 2;
    Boot::Known(boot)
}

fn boot_bytes(boot: Boot) -> [u8; 16] {
    match boot {
        Boot::Known(bytes) => bytes,
        Boot::Unknown => [0; 16],
    }
}

/// Every key's get is its bytes or a miss, never other bytes; each witnessed key is held. A
/// finding panics: it fails the seed.
fn oracle(cx: &Cx, store: &mut PackStore, witnessed: &[u32]) -> Result<std::vec::Vec<u32>, CliError> {
    let mut held = std::vec::Vec::new();
    for k in 0..FORTY * 2 {
        let mut into = Bytes::new_in(cx.frame(), "a record read back");
        if store.get_into(cx, big_key(k, 0), &mut into)? {
            assert!(into.as_slice() == small(k).as_slice(), "store oracle: key {k} read back as other bytes");
            held.push(k);
        } else {
            assert!(!witnessed.contains(&k), "store oracle: key {k}, which a witness stands on, is lost");
        }
    }
    Ok(held)
}

fn read_witness(cx: &Cx, root: &Dir<ReadWrite>, boot: Boot) -> Result<std::vec::Vec<u32>, CliError> {
    let mut bytes = Bytes::new_in(cx.frame(), "the witness");
    match root.read_all(cx, &Path::new(WITNESS)?, &mut bytes) {
        Ok(_) => {}
        Err(error) if error.kind == FsKind::NotFound => return Ok(std::vec::Vec::new()),
        Err(error) => return Err(error.into()),
    }
    let bytes = bytes.as_slice();
    if bytes.len() < 16 || bytes[..16] != boot_bytes(boot) {
        return Ok(std::vec::Vec::new());
    }
    Ok(bytes[16..].chunks_exact(4).map(|word| u32::from_le_bytes([word[0], word[1], word[2], word[3]])).collect())
}

/// One start of the program the runner cuts and restarts: the oracle on what the home holds;
/// forty commands' packs made (two records each, a seal each, no merge while they are made);
/// the witness; then a command whose rule merges the forty (more than `PACKS_MOST`), and the
/// oracle again. A cut anywhere in the merge leaves every witnessed record readable from the
/// old packs or the new one.
fn compaction(cx: &Cx, io: &Io, plant: StorePlant) -> Result<(), CliError> {
    let root = io.need_fs().map_err(CliError::Missing)?.root();
    let boot = boot_of(io);
    let witnessed = read_witness(cx, &root, boot)?;
    let mut store = PackStore::open_with(cx, &root, boot, FORTY as usize, plant)?;
    let held = oracle(cx, &mut store, &witnessed)?;
    for pack in 0..FORTY {
        let (a, b) = (pack * 2, pack * 2 + 1);
        if held.contains(&a) && held.contains(&b) {
            continue;
        }
        store.put(cx, big_key(a, 0), &small(a))?;
        store.put(cx, big_key(b, 0), &small(b))?;
        store.seal(cx)?;
    }
    let held = oracle(cx, &mut store, &witnessed)?;
    let mut witness = std::vec::Vec::from(boot_bytes(boot));
    witness.extend(held.iter().flat_map(|k| k.to_le_bytes()));
    root.publish(cx, &Path::new(WITNESS)?, Content::bytes(&witness), Durable::No)?;
    drop(store);
    let witnessed = read_witness(cx, &root, boot)?;
    let mut store = PackStore::open_with(cx, &root, boot, PACKS_MOST, plant)?;
    let mut profile = Profile::default();
    store.after_answer(cx, &mut profile)?;
    oracle(cx, &mut store, &witnessed)?;
    assert!(store.packs.len() <= PACKS_MOST, "store oracle: {} packs left after the compaction rule ran", store.packs.len());
    Ok(())
}

fn compaction_crashes(count: u32, plant: StorePlant) -> Result<bstd::test::Passed, bstd::test::Failed> {
    seeded("blaze-cli", module_path!(), "pack_store_compaction", Seeds::DEFAULT.from(0, count), Given::NOTHING, |cx: &Cx, io: Io| compaction(cx, &io, plant))
}

/// (iii) Power cuts and injected failures at effects each seed draws (the merge's reads, its
/// publish's appends and rename, the boot record, each removal), the program started again
/// after each: no get returns other bytes, no witnessed record is lost, and the healed start
/// leaves at most `PACKS_MOST` packs.
#[test]
fn compaction_of_forty_packs_survives_a_crash_anywhere() {
    match compaction_crashes(400, StorePlant::None) {
        Ok(passed) => {
            assert!(passed.counts.cuts > 300, "too few power cuts: {:?}", passed.counts);
            assert!(passed.counts.failures > 300, "too few failures: {:?}", passed.counts);
        }
        Err(failed) => panic!("{failed}"),
    }
}

/// The planted bug: the merge removes the forty old packs before the merged one is renamed
/// in; a failure between loses witnessed records, which the oracle names ("is lost").
#[test]
fn the_compaction_oracle_catches_a_merge_that_removes_first() {
    match compaction_crashes(400, StorePlant::MergeRemovesFirst) {
        Ok(passed) => panic!("the planted MergeRemovesFirst was not caught in {} starts", passed.starts),
        Err(failed) => {
            let said = std::format!("{failed}");
            assert!(said.contains("is lost"), "the planted MergeRemovesFirst failed otherwise: {said}");
        }
    }
}
