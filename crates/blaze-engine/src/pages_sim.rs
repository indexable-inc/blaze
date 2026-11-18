//! The stored answer's durable order (pages.rs, "Durable order"; rule 43): answers written one
//! after another into a model of the plain store's contract (blaze-cas plain: puts and pins are
//! durable only at a commit, which is one atomic flip; an early commit may come after any put,
//! as `PlainPlace::put` commits past `COMMIT_BYTES`; compaction keeps only what a root names),
//! a crash at every point of a write, then compaction, then a read. The oracle: the checkout's
//! root reads back whole, as the last committed answer, never a mix and never a refusal. The
//! history half (sim.rs `store_and_restore`) holds each restored answer to a fresh build.

use std::collections::{BTreeMap, BTreeSet};
use std::format;
use std::vec::Vec as StdVec;

use blaze_drive::cells::Cell;
use blaze_drive::key::{Key, Kind, Name};
use blaze_id::ContentId;
use blaze_own::{SharedAlloc, Vec};
use blaze_sim::{Broke, Config as SimConfig, Tape, check, control};

use crate::engine::{Cause, CellId, Gen, Phase, Row, Stage};
use crate::pages::{Kept, PAGE_MAGIC, PagesError, PagesPlant, decode, encode_planted};

fn alloc() -> SharedAlloc<'static> {
    blaze_own::testing::heap().shared()
}

const ROOT_NAME: &str = "last";

/// The plain store's contract, as the stored answer relies on it.
#[derive(Default)]
struct Store {
    records: BTreeMap<Key, StdVec<u8>>,
    roots: BTreeMap<&'static str, StdVec<Key>>,
    pending: BTreeMap<Key, StdVec<u8>>,
    pending_roots: BTreeMap<&'static str, StdVec<Key>>,
}

impl Store {
    fn commit(&mut self) {
        self.records.append(&mut self.pending);
        self.roots.append(&mut self.pending_roots);
    }

    /// A crash: what no commit made durable is gone; then compaction, the worst case: every
    /// record no root names goes.
    fn crash_and_compact(&mut self) {
        self.pending.clear();
        self.pending_roots.clear();
        let named: BTreeSet<Key> = self.roots.values().flatten().copied().collect();
        self.records.retain(|key, _| named.contains(key));
    }

    fn read(&self) -> Option<Result<Kept<'static>, PagesError>> {
        let root = *self.roots.get(ROOT_NAME)?.first()?;
        Some(decode(root, |key| self.records.get(&key).map(|bytes| bytes.as_slice()), alloc()))
    }
}

fn name(kind: Kind, seed: u64, i: u64) -> Name {
    Name::new(kind, ContentId::of("pages sim name", &[seed.to_le_bytes(), i.to_le_bytes()].concat()))
}

fn id(seed: u64, i: u64, what: u8) -> ContentId {
    ContentId::of("pages sim digest", &[seed.to_le_bytes().as_slice(), &i.to_le_bytes(), &[what]].concat())
}

/// An answer of `rows` rows, row `i`'s contents a function of `(seed, version[i])`: big enough
/// to page by several bits, each row read by a few others.
fn answer(rows: &BTreeMap<u64, u64>, seed: u64, serial: u64) -> Kept<'static> {
    let a = alloc();
    let mut kept = Kept::new_in(a);
    kept.answer.serial = Gen(serial);
    let names: StdVec<u64> = rows.keys().copied().collect();
    for (&i, &version) in rows.iter() {
        let me = name(Kind::ModuleScope, seed, i);
        let file = name(Kind::FileItems, seed, i % 7);
        let mut defines: crate::engine::SVec<'static, (Cell, ContentId)> = Vec::new_in(a);
        for c in 0..(version % 3) {
            defines.push((Cell([c as u8; 16]), id(seed ^ version, i, c as u8)));
            kept.answer.cells.insert(CellId { of: me, cell: Cell([c as u8; 16]) }, (id(seed ^ version, i, c as u8), Gen(serial)));
        }
        let mut reads: crate::engine::SVec<'static, (CellId, ContentId)> = Vec::new_in(a);
        let mut read_ids: StdVec<CellId> = (0..(version % 4)).map(|k| CellId { of: name(Kind::ModuleScope, seed, names[((i + k * 13) as usize) % names.len()]), cell: Cell([0; 16]) }).collect();
        read_ids.sort();
        read_ids.dedup();
        for read in read_ids {
            reads.push((read, id(seed, version, 9)));
            kept.answer.readers.entry(read).or_insert_with(|| blaze_own::BTreeSet::new_in(a)).insert(me);
        }
        let findings = (version % 5 == 0) as u32;
        if findings > 0 {
            kept.answer.with_findings.insert(me);
        }
        kept.answer.rows.insert(
            me,
            Row {
                key: Key(id(seed, version, 1)),
                phase: Phase::new(Stage::Names, (i % 3) as u16),
                file,
                source: id(seed, version, 2),
                out: id(seed, version, 3),
                serial: Gen(serial),
                defines,
                reads,
                findings,
                inline: false,
                failed: false,
            },
        );
        let list = kept.answer.by_file.entry(file).or_insert_with(|| Vec::new_in(a));
        if let Err(at) = list.binary_search(&me) {
            list.insert(at, me);
        }
        let cause = match version % 3 {
            0 => Cause::New,
            1 => Cause::KeyMoved,
            _ => Cause::ReadMoved { read: CellId { of: file, cell: Cell([1; 16]) }, was: id(seed, version, 4), now: id(seed, version, 5) },
        };
        kept.causes.insert(me, (Gen(serial), cause));
    }
    kept
}

fn writes(tape: &Tape, plant: PagesPlant) -> Result<(), Broke> {
    let seed = tape.index(1 << 16, "seed") as u64;
    let mut rows: BTreeMap<u64, u64> = (0..tape.index(1200, "rows") as u64).map(|i| (i, i)).collect();
    let mut store = Store::default();
    let mut committed: Option<Key> = None;
    let mut last_root = None;
    for serial in 1..2 + tape.index(4, "requests") as u64 {
        // An edit: a few rows changed, removed or added.
        for _ in 0..tape.index(8, "rows edited") {
            let i = tape.index(1300, "the row") as u64;
            match tape.index(3, "how") {
                0 => {
                    rows.remove(&i);
                }
                _ => {
                    rows.insert(i, i + serial * 1000);
                }
            }
        }
        let kept = answer(&rows, seed, serial);
        let encoded = encode_planted(&kept, last_root.as_ref(), plant, alloc());
        let pins: StdVec<Key> = encoded.pins(alloc()).iter().copied().collect();
        // The write: each put (an early commit may follow any), the pin, the one commit.
        let steps = encoded.puts.len() + 2;
        let crash_at = match tape.one_in(2, "a crash") {
            true => Some(tape.index(steps + 1, "the crash point")),
            false => None,
        };
        let mut step = 0;
        let mut crashed = false;
        for (key, bytes) in encoded.puts.iter() {
            if crash_at == Some(step) {
                crashed = true;
                break;
            }
            store.pending.insert(*key, bytes.iter().copied().collect());
            step += 1;
            if tape.one_in(3, "an early commit") {
                store.commit();
            }
        }
        if !crashed && crash_at == Some(step) {
            crashed = true;
        }
        if !crashed {
            store.pending_roots.insert(ROOT_NAME, pins);
            step += 1;
            if crash_at == Some(step) {
                crashed = true;
            } else {
                store.commit();
                committed = Some(encoded.root);
            }
        }
        store.crash_and_compact();
        // The read: the last committed answer, whole.
        match (store.read(), committed) {
            (None, None) => {}
            (Some(Ok(read)), Some(root)) => {
                let again = crate::pages::encode(&read, None, alloc());
                if again.root != root {
                    return Err(Broke::Oracle(format!("request {serial}: the stored answer reads back as another one than the last committed")));
                }
                last_root = Some(crate::pages::decode_root(root, &store.records[&root], alloc()).map_err(|error| Broke::Harness(format!("{error}")))?);
            }
            (Some(Err(error)), _) => return Err(Broke::Oracle(format!("request {serial}: after a crash at {crash_at:?} the stored answer does not read: {error}"))),
            (found, root) => return Err(Broke::Oracle(format!("request {serial}: the store names {:?}, the last commit {root:?}", found.map(|found| found.is_ok())))),
        }
        // The next request starts from what stands, crashed or not.
    }
    Ok(())
}

#[test]
fn a_crash_at_any_point_of_a_write_leaves_the_last_committed_answer_whole() {
    check(SimConfig::cases(200), |tape| writes(tape, PagesPlant::None));
}

#[test]
fn the_oracle_catches_pages_not_pinned() {
    control(SimConfig::cases(200), PagesPlant::None, PagesPlant::PagesNotPinned, "does not read", writes);
}

#[test]
fn the_history_oracle_catches_files_not_stored() {
    control(SimConfig::cases(300), PagesPlant::None, PagesPlant::FilesDropped, "read back differs in its files", crate::sim::histories_every);
}

/// A flipped bit or a cut page is a typed refusal naming the page, never an answer.
#[test]
fn a_flipped_or_cut_page_is_refused_by_name() {
    let rows: BTreeMap<u64, u64> = (0..900).map(|i| (i, i)).collect();
    let kept = answer(&rows, 3, 1);
    let encoded = crate::pages::encode(&kept, None, alloc());
    assert!(encoded.bits > 0, "the fixture pages by more than one page");
    let records: BTreeMap<Key, StdVec<u8>> = encoded.puts.iter().map(|(key, bytes)| (*key, bytes.iter().copied().collect())).collect();
    let back = decode(encoded.root, |key| records.get(&key).map(|bytes| bytes.as_slice()), alloc()).expect("the answer reads back");
    assert_eq!(crate::pages::encode(&back, None, alloc()).root, encoded.root, "a restore equals the write in bytes (rule 26)");
    let page = *encoded.pages.iter().find(|key| **key != Key::default()).expect("a page");
    let index = encoded.pages.iter().position(|key| *key == page).expect("its index") as u32;
    // A flipped bit.
    let mut flipped = records.clone();
    flipped.get_mut(&page).expect("the page")[40] ^= 1;
    match decode(encoded.root, |key| flipped.get(&key).map(|bytes| bytes.as_slice()), alloc()) {
        Err(PagesError::Digest { page: at, .. }) => assert_eq!(at, index),
        other => panic!("a flipped page read as {:?}", other.map(|_| ())),
    }
    // A cut page whose key is its own bytes' (so the form's own checks are what refuse it).
    let mut cut = records.get(&page).expect("the page").clone();
    cut.truncate(cut.len() - 5);
    assert!(cut.starts_with(PAGE_MAGIC));
    let error = crate::pages::decode_page_for_test(&cut, index, encoded.bits).expect_err("a cut page is refused");
    assert!(matches!(error, PagesError::Short { .. }), "a cut page is Short, got {error:?}");
    // A missing page.
    let mut missing = records.clone();
    missing.remove(&page);
    match decode(encoded.root, |key| missing.get(&key).map(|bytes| bytes.as_slice()), alloc()) {
        Err(PagesError::Missing { page: at, key }) => assert_eq!((at, key), (index, page)),
        other => panic!("a missing page read as {:?}", other.map(|_| ())),
    }
}
