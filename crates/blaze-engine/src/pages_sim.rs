//! The stored answer's durable order (pages.rs, "Durable order"; rule 43): answers written one
//! after another into a model of the plain store's contract (blaze-cas plain: puts and pins are
//! durable only at a commit, which is one atomic flip; an early commit may come after any put,
//! as `PlainPlace::put` commits past `COMMIT_BYTES`; compaction keeps only what a root names),
//! a crash at every point of a write, then compaction, then a read. The oracle: the checkout's
//! root reads back whole, as the last committed answer, never a mix; or, when a crash tore a
//! segment it lies in, is refused naming that segment. The history half (sim.rs
//! `store_and_restore`) holds each restored answer to a fresh build.
//!
//! VERIFY ONCE (pages.rs, "Checked once"). The model keeps the store's segments and its boots
//! (blaze-cas plain verified.rs): a commit writes its batch as one new segment, not synced, and
//! the flip that names it records the boot that wrote it; a seal (a barrier, then a flip saying
//! so) makes a segment durable whole; a crash is a new boot, and an unsealed segment keeps its
//! bytes only up to a point the tape draws, zeros after (a write torn in place). A read proves a
//! record's segment once a boot and reads its pages by that proof, hashing nothing else: proved
//! when written or checked whole in this boot, or sealed; else checked whole now (its digest
//! over every byte), and refused by name when torn. The oracle holds every record the pages
//! accepted to be its key's bytes, a refusal to name a segment the crash really tore, and a
//! read in the boot of the last one to hash nothing. The control plants the boot ignored (a
//! proof of an earlier boot taken as this one's): a torn page is then accepted, and caught.

use std::collections::{BTreeMap, BTreeSet};
use std::format;
use std::println;
use std::vec::Vec as StdVec;

use blaze_drive::cells::Cell;
use blaze_drive::key::{Key, Kind, Name, content_key};
use blaze_id::ContentId;
use blaze_own::{SharedAlloc, Vec};
use blaze_sim::{Broke, Config as SimConfig, Tape, check, control};

use crate::engine::{Cause, CellId, Gen, Phase, Row, Stage};
use crate::pages::{Kept, PAGE_MAGIC, PagesError, PagesPlant, Sealed, StoreProof, decode, encode_planted};

fn alloc() -> SharedAlloc<'static> {
    blaze_own::testing::heap().shared()
}

const ROOT_NAME: &str = "last";

/// The model store's proof of a segment proved this boot ([`Store::prove`] makes the only
/// ones): blaze-cli plain_place.rs `Checked`'s stand-in.
#[derive(Clone, Copy)]
struct SimProof;

impl StoreProof for SimProof {}

/// The bugs this simulation plants: the pages' own, and the store's proof taken across a boot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    Pages(PagesPlant),
    /// A segment's proof of an earlier boot taken as this one's (the boot id ignored): a
    /// segment a crash tore is read unchecked.
    BootIgnored,
}

impl Plant {
    fn pages(self) -> PagesPlant {
        match self {
            Plant::Pages(plant) => plant,
            Plant::None | Plant::BootIgnored => PagesPlant::None,
        }
    }
}

/// One segment as the store holds it.
struct Segment {
    bytes: StdVec<u8>,
    /// What its manifest row names: the digest of the bytes as written.
    digest: ContentId,
    /// A barrier covered it before a flip said so: durable whole.
    sealed: bool,
    /// The boot it was written in or last checked whole in (its manifest row's).
    proved: u64,
    /// Checked whole and found torn: every read of it is refused by name.
    refused: bool,
}

/// What one read did: the profile's `pages: N read, M bytes, K hashed` (K counts the bytes the
/// store's once-a-boot check hashed, and any the pages hashed: none).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct Counts {
    pages: u64,
    bytes: u64,
    hashed: u64,
}

impl Counts {
    fn line(&self) -> std::string::String {
        format!("pages: {} read, {} bytes, {} hashed", self.pages, self.bytes, self.hashed)
    }
}

/// What a read of the checkout's root found.
enum Outcome {
    /// No root is named.
    Nothing,
    /// The answer, whole.
    Whole(Kept<'static>),
    /// A segment holding a record the answer reaches was checked and is torn.
    Refused { segment: usize },
    /// The pages refused the answer.
    Pages(PagesError),
}

/// The plain store's contract, as the stored answer relies on it, down to its segments and
/// boots (the module docs, "Verify once").
#[derive(Default)]
struct Store {
    boot: u64,
    segments: StdVec<Segment>,
    /// Each committed record's place: its segment, offset and length.
    records: BTreeMap<Key, (usize, usize, usize)>,
    roots: BTreeMap<&'static str, StdVec<Key>>,
    pending: BTreeMap<Key, StdVec<u8>>,
    pending_roots: BTreeMap<&'static str, StdVec<Key>>,
    /// The segments a crash changed: the oracle's knowledge, never the reader's.
    torn: BTreeSet<usize>,
}

impl Store {
    /// The one flip: the batch as one new segment, unsynced, named with this boot; then the
    /// roots. A key already held in a segment not refused is not written again (the store's
    /// `Put::Same`).
    fn commit(&mut self) {
        let id = self.segments.len();
        let mut bytes = StdVec::new();
        for (key, record) in core::mem::take(&mut self.pending) {
            if self.records.get(&key).is_some_and(|place| !self.segments[place.0].refused) {
                continue;
            }
            self.records.insert(key, (id, bytes.len(), record.len()));
            bytes.extend_from_slice(&record);
        }
        if !bytes.is_empty() {
            let digest = ContentId::of("pages sim segment", &bytes);
            self.segments.push(Segment { bytes, digest, sealed: false, proved: self.boot, refused: false });
        }
        self.roots.append(&mut self.pending_roots);
    }

    /// A barrier over every segment, then the flip that says so (the exit tail).
    fn seal_all(&mut self) {
        for segment in self.segments.iter_mut() {
            segment.sealed = true;
        }
    }

    /// Segment `id` torn from byte `cut` on: zeros after it, as a write cut by a crash leaves
    /// a block it never reached. Recorded in [`Store::torn`] when a byte changed.
    fn tear(&mut self, id: usize, cut: usize) {
        let segment = &mut self.segments[id];
        let cut = cut.min(segment.bytes.len());
        if segment.bytes[cut..].iter().any(|&b| b != 0) {
            self.torn.insert(id);
        }
        segment.bytes[cut..].fill(0);
    }

    /// A crash: a new boot; what no commit made durable is gone; each unsealed segment kept to
    /// a point the tape draws; then compaction, the worst case: every record no root names goes.
    fn crash_and_compact(&mut self, tape: &Tape) {
        self.boot += 1;
        self.pending.clear();
        self.pending_roots.clear();
        for id in 0..self.segments.len() {
            let segment = &self.segments[id];
            if segment.sealed || segment.refused || !tape.one_in(2, "an unsealed segment torn") {
                continue;
            }
            let cut = tape.index(segment.bytes.len() + 1, "where the tear begins");
            self.tear(id, cut);
        }
        self.compact();
    }

    fn compact(&mut self) {
        let named: BTreeSet<Key> = self.roots.values().flatten().copied().collect();
        self.records.retain(|key, _| named.contains(key));
    }

    /// The store's proof of segment `id` for this boot (blaze-cas plain verified.rs): proved
    /// when written or checked whole in this boot, or sealed; else checked whole now, once,
    /// and refused by name when its bytes are not its digest's.
    fn prove(&mut self, id: usize, plant: Plant, counts: &mut Counts) -> Result<SimProof, usize> {
        let boot = self.boot;
        let segment = &mut self.segments[id];
        if segment.refused {
            return Err(id);
        }
        let recorded = segment.proved == boot || plant == Plant::BootIgnored;
        if recorded || segment.sealed {
            return Ok(SimProof);
        }
        counts.hashed += segment.bytes.len() as u64;
        if ContentId::of("pages sim segment", &segment.bytes) != segment.digest {
            // Refused whole and set aside: a later put of one of its records writes it again
            // ([`Store::commit`]); a root still naming it is refused again, by the same name.
            segment.refused = true;
            return Err(id);
        }
        segment.proved = boot;
        Ok(SimProof)
    }

    /// The record at `key` by its segment's proof: `None` when the store has none.
    fn get(&mut self, key: Key, plant: Plant, counts: &mut Counts) -> Option<Result<(SimProof, (usize, usize, usize)), usize>> {
        let place = *self.records.get(&key)?;
        Some(self.prove(place.0, plant, counts).map(|proof| (proof, place)))
    }

    fn bytes(&self, (id, at, len): (usize, usize, usize)) -> &[u8] {
        &self.segments[id].bytes[at..at + len]
    }

    /// The checkout's answer, every record by its segment's proof, the pages hashing nothing.
    /// A record the pages accepted that is not its key's bytes is the oracle's failure.
    fn read(&mut self, plant: Plant) -> Result<(Outcome, Counts), Broke> {
        let mut counts = Counts::default();
        let Some(&root) = self.roots.get(ROOT_NAME).and_then(|keys| keys.first()) else {
            return Ok((Outcome::Nothing, counts));
        };
        // Every record the answer reaches, proved first (the store's work), then read in place.
        let mut places = BTreeMap::new();
        let root_place = match self.get(root, plant, &mut counts) {
            None => return Ok((Outcome::Pages(PagesError::Missing { page: crate::pages::ROOT_PAGE, key: root }), counts)),
            Some(Err(segment)) => return Ok((Outcome::Refused { segment }, counts)),
            Some(Ok((_, place))) => place,
        };
        accepted(self, root, root_place)?;
        let directory = match crate::pages::decode_root(Sealed::by_store(root, self.bytes(root_place), SimProof), alloc()) {
            Ok(directory) => directory,
            Err(error) => return Ok((Outcome::Pages(error), counts)),
        };
        places.insert(root, root_place);
        for &key in directory.pages.iter().filter(|key| **key != Key::default()) {
            match self.get(key, plant, &mut counts) {
                None => {}
                Some(Err(segment)) => return Ok((Outcome::Refused { segment }, counts)),
                Some(Ok((_, place))) => {
                    accepted(self, key, place)?;
                    places.insert(key, place);
                }
            }
        }
        let store = &*self;
        let decoded = decode(root, |key| {
            let place = *places.get(&key)?;
            if key != root {
                counts.pages += 1;
                counts.bytes += place.2 as u64;
            }
            Some(Ok(Sealed::by_store(key, store.bytes(place), SimProof)))
        }, alloc());
        Ok((decoded.map_or_else(Outcome::Pages, Outcome::Whole), counts))
    }
}

/// The oracle on a record the pages are about to read by the store's proof: its bytes are its
/// key's. The reader never hashes them; the oracle does, to catch a proof that lied.
fn accepted(store: &Store, key: Key, place: (usize, usize, usize)) -> Result<(), Broke> {
    if content_key(store.bytes(place)) == key {
        return Ok(());
    }
    Err(Broke::Oracle(format!(
        "the pages accepted {key:?} by its proof, and its bytes are not its key's: segment {} torn, proved in boot {} and read in boot {}",
        place.0, store.segments[place.0].proved, store.boot,
    )))
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
        let findings = u32::from(version % 5 == 0);
        if findings > 0 {
            kept.answer.with_findings.insert(me);
        }
        let by_cell = crate::engine::by_cell(me, &reads, a).expect("the fixture's reads are sorted and each once");
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
                by_cell,
                findings,
                inline: false,
                failed: false,
            },
        );
        let list = kept.answer.by_file.entry(file).or_insert_with(|| Vec::new_in(a));
        if let Err(at) = list.binary_search(&me) {
            list.insert(at, me);
        }
        // Each file's place, by the file alone (a driver's place is the file's, not a row's).
        let at = i % 7;
        kept.places.insert(file, crate::pages::FilePlace { unit: name(Kind::FileItems, seed, 0), edition: (at % 3) as u8, flags: (at % 4) as u8 });
        let cause = match version % 3 {
            0 => Cause::New,
            1 => Cause::KeyMoved,
            _ => Cause::ReadMoved { read: CellId { of: file, cell: Cell([1; 16]) }, was: id(seed, version, 4), now: id(seed, version, 5) },
        };
        kept.causes.insert(me, (Gen(serial), cause));
    }
    kept
}

fn writes(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let seed = tape.index(1 << 16, "seed") as u64;
    let mut rows: BTreeMap<u64, u64> = (0..tape.index(1200, "rows") as u64).map(|i| (i, i)).collect();
    let mut store = Store::default();
    let mut committed: Option<Key> = None;
    let mut last_root = None;
    // The last read found the answer whole: every record a root names was proved this boot.
    let mut last_whole = false;
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
        let encoded = encode_planted(&kept, last_root.as_ref(), plant.pages(), alloc());
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
        // The exit tail may seal what was written before the crash point (a barrier, a flip).
        if !crashed && tape.one_in(3, "the exit tail seals") {
            store.seal_all();
        }
        // A crash is a new boot; with none the process ends and the next runs in this boot.
        let rebooted = crashed || tape.one_in(4, "a machine crash after the command");
        if rebooted {
            store.crash_and_compact(tape);
        } else {
            store.compact();
        }
        // The read: the last committed answer, whole, or refused naming a torn segment.
        let (outcome, counts) = store.read(plant)?;
        // Every record a root reaches was read whole by the last read or written since: in the
        // same boot, nothing is checked again (rule 77).
        if !rebooted && last_whole && counts.hashed != 0 {
            return Err(Broke::Oracle(format!("request {serial}: a read in the boot of the last one hashed ({}): a proof was not kept", counts.line())));
        }
        last_whole = matches!(outcome, Outcome::Whole(_));
        match (outcome, committed) {
            (Outcome::Nothing, None) => {}
            (Outcome::Whole(read), Some(root)) => {
                let again = crate::pages::encode(&read, None, alloc());
                if again.root != root {
                    return Err(Broke::Oracle(format!("request {serial}: the stored answer reads back as another one than the last committed")));
                }
                let place = store.records[&root];
                last_root = Some(crate::pages::decode_root_hashed(root, store.bytes(place), alloc()).map_err(|error| Broke::Harness(format!("{error}")))?);
            }
            (Outcome::Refused { segment }, Some(_)) => {
                if !store.torn.contains(&segment) {
                    return Err(Broke::Oracle(format!("request {serial}: the read refused segment {segment}, which no crash tore")));
                }
                // Refused by name: the command reports it and answers in full; the next write
                // starts from no answer.
                last_root = None;
                committed = None;
            }
            (Outcome::Pages(error), _) => return Err(Broke::Oracle(format!("request {serial}: after a crash at {crash_at:?} the stored answer does not read: {error}"))),
            // The answer refused before, its rewrite cut by a crash: the root still names it,
            // and it is refused again by the same name.
            (Outcome::Refused { segment }, None) => {
                if !store.torn.contains(&segment) {
                    return Err(Broke::Oracle(format!("request {serial}: the read refused segment {segment}, which no crash tore")));
                }
            }
            (Outcome::Whole(_), None) | (Outcome::Nothing, Some(_)) => {
                return Err(Broke::Oracle(format!("request {serial}: the store's root and the last commit {committed:?} disagree")));
            }
        }
        // The next request starts from what stands, crashed or not.
    }
    Ok(())
}

/// The invariant: a crash at any point of a page-backed answer write leaves the last committed answer whole, pins every live page, and never trusts a segment proof from an earlier boot.
#[test]
fn a_crash_at_any_point_of_a_write_leaves_the_last_committed_answer_whole() {
    check(SimConfig::cases(200), |tape| writes(tape, Plant::None));
}

#[test]
fn the_oracle_catches_pages_not_pinned() {
    control(SimConfig::cases(200), Plant::None, Plant::Pages(PagesPlant::PagesNotPinned), "does not read", writes);
}

/// The planted bug: a segment's proof of an earlier boot taken as this one's. A segment a
/// crash tore is then read unchecked, and the oracle catches a torn page accepted.
#[test]
fn the_oracle_catches_the_boot_ignored() {
    control(SimConfig::cases(300), Plant::None, Plant::BootIgnored, "its bytes are not its key's", writes);
}

/// The cost claim, before and after, on one answer: the old reader hashed every page it read
/// (K = M); this one hashes nothing in the boot that wrote or checked a segment, and after a
/// reboot hashes each reached segment once, at its first read, never again that boot. A
/// segment a crash tore is refused by name; with the boot ignored it would be read.
#[test]
fn a_page_is_hashed_once_a_boot_by_its_segment_and_a_torn_one_is_refused_by_name() {
    let rows: BTreeMap<u64, u64> = (0..900).map(|i| (i, i)).collect();
    let kept = answer(&rows, 5, 1);
    let encoded = crate::pages::encode(&kept, None, alloc());
    let mut store = Store::default();
    for (key, bytes) in encoded.puts.iter() {
        store.pending.insert(*key, bytes.iter().copied().collect());
    }
    store.pending_roots.insert(ROOT_NAME, encoded.pins(alloc()).iter().copied().collect());
    store.commit();
    let whole = |outcome: &Outcome| matches!(outcome, Outcome::Whole(read) if crate::pages::encode(read, None, alloc()).root == encoded.root);
    // The boot that wrote it: proved by its manifest row.
    let (outcome, written) = store.read(Plant::None).expect("the oracle holds");
    assert!(whole(&outcome), "the answer reads back whole");
    assert!(written.pages > 1 && written.hashed == 0, "{}", written.line());
    let before = Counts { hashed: written.bytes, ..written };
    println!("before (every page hashed at its open): {}", before.line());
    println!("after, in the boot that wrote it:       {}", written.line());
    // A reboot with the segment unsealed and whole: checked once, then never again this boot.
    store.boot += 1;
    let (outcome, first) = store.read(Plant::None).expect("the oracle holds");
    assert!(whole(&outcome));
    let segment_bytes = store.segments[0].bytes.len() as u64;
    assert_eq!(first.hashed, segment_bytes, "the first read after a reboot checks the segment whole, once: {}", first.line());
    let (outcome, again) = store.read(Plant::None).expect("the oracle holds");
    assert!(whole(&outcome));
    assert_eq!(again.hashed, 0, "a second read of the boot hashes nothing: {}", again.line());
    println!("after a reboot, its first read:         {}", first.line());
    println!("after a reboot, every read after it:    {}", again.line());
    // Sealed by the exit tail: a reboot checks nothing.
    store.seal_all();
    store.boot += 1;
    let (_, sealed) = store.read(Plant::None).expect("the oracle holds");
    assert_eq!(sealed.hashed, 0, "a sealed segment is proved by its seal: {}", sealed.line());
    // Unsealed, torn by a crash in the page past the root: refused by name; with the boot
    // ignored, the torn page is accepted and the oracle says so.
    store.segments[0].sealed = false;
    let page = *encoded.pages.iter().find(|key| **key != Key::default()).expect("a page");
    let (_, at, len) = store.records[&page];
    store.tear(0, at + len / 2);
    store.boot += 1;
    let mut planted = Store { boot: store.boot, ..Store::default() };
    planted.segments.push(Segment { bytes: store.segments[0].bytes.clone(), digest: store.segments[0].digest, sealed: false, proved: store.segments[0].proved, refused: false });
    planted.records = store.records.clone();
    planted.roots = store.roots.clone();
    match store.read(Plant::None).expect("the oracle holds") {
        (Outcome::Refused { segment: 0 }, counts) => assert_eq!(counts.hashed, segment_bytes, "{}", counts.line()),
        _ => panic!("a torn segment of an earlier boot is refused by name"),
    }
    let caught = planted.read(Plant::BootIgnored).err().expect("the boot ignored reads a torn page, and the oracle catches it");
    assert!(format!("{caught:?}").contains("its bytes are not its key's"), "{caught:?}");
}

#[test]
fn the_history_oracle_catches_files_not_stored() {
    control(SimConfig::cases(300), PagesPlant::None, PagesPlant::FilesDropped, "read back differs in its files", crate::sim::histories_every);
}

/// The file places (D187: the ready edit re-plans an edited file from its place alone) are in
/// the root's bytes and read back equal (rule 26). CONTROLS (rule 23): one place moved gives
/// another root, so the round trip's equality covers them; a place of no planned file is not
/// written, so a root never names one (`read_directory` refuses such a root by name).
#[test]
fn file_places_are_in_the_root_and_read_back_equal() {
    let rows: BTreeMap<u64, u64> = (0..300).map(|i| (i, i)).collect();
    let kept = answer(&rows, 5, 1);
    assert_eq!(kept.places.len(), 7, "the fixture places each of its seven files");
    let encoded = crate::pages::encode(&kept, None, alloc());
    let records: BTreeMap<Key, StdVec<u8>> = encoded.puts.iter().map(|(key, bytes)| (*key, bytes.iter().copied().collect())).collect();
    let back = decode(encoded.root, |key| records.get(&key).map(|bytes| Sealed::hashed(key, bytes)), alloc()).expect("the answer reads back");
    assert!(back.places.iter().eq(kept.places.iter()), "every place reads back as written");
    assert_eq!(crate::pages::encode(&back, None, alloc()).root, encoded.root, "a restore equals the write in bytes (rule 26)");
    let mut moved = answer(&rows, 5, 1);
    let first = *moved.places.keys().next().expect("a place");
    moved.places.insert(first, crate::pages::FilePlace { edition: 9, ..*kept.places.get(&first).expect("its place") });
    assert_ne!(crate::pages::encode(&moved, None, alloc()).root, encoded.root, "CONTROL: a moved place moves the root, or the round trip sees no place");
    let mut stray = answer(&rows, 5, 1);
    stray.places.insert(name(Kind::FileItems, 5, 999), crate::pages::FilePlace { unit: name(Kind::FileItems, 5, 0), edition: 0, flags: 0 });
    assert_eq!(crate::pages::encode(&stray, None, alloc()).root, encoded.root, "a place of no planned file is not written");
}

/// A flipped bit or a cut page is a typed refusal naming the page, never an answer.
#[test]
fn a_flipped_or_cut_page_is_refused_by_name() {
    let rows: BTreeMap<u64, u64> = (0..900).map(|i| (i, i)).collect();
    let kept = answer(&rows, 3, 1);
    let encoded = crate::pages::encode(&kept, None, alloc());
    assert!(encoded.bits > 0, "the fixture pages by more than one page");
    let records: BTreeMap<Key, StdVec<u8>> = encoded.puts.iter().map(|(key, bytes)| (*key, bytes.iter().copied().collect())).collect();
    let back = decode(encoded.root, |key| records.get(&key).map(|bytes| Sealed::hashed(key, bytes)), alloc()).expect("the answer reads back");
    assert_eq!(crate::pages::encode(&back, None, alloc()).root, encoded.root, "a restore equals the write in bytes (rule 26)");
    let page = *encoded.pages.iter().find(|key| **key != Key::default()).expect("a page");
    let index = encoded.pages.iter().position(|key| *key == page).expect("its index") as u32;
    // A flipped bit.
    let mut flipped = records.clone();
    flipped.get_mut(&page).expect("the page")[40] ^= 1;
    match decode(encoded.root, |key| flipped.get(&key).map(|bytes| Sealed::hashed(key, bytes)), alloc()) {
        Err(PagesError::Digest { page: at, .. }) => assert_eq!(at, index),
        other => panic!("a flipped page read as {:?}", other.map(|_| ())),
    }
    // A cut page whose key is its own bytes' (so the form's own checks are what refuse it).
    let mut cut = records.get(&page).expect("the page").clone();
    cut.truncate(cut.len() - 5);
    assert!(cut.starts_with(PAGE_MAGIC));
    let error = crate::pages::decode_page_for_test(&cut, index, encoded.bits).expect_err("a cut page is refused");
    // BEP4: the start table's last word is the page's length, so a cut is caught at the head.
    assert!(matches!(error, PagesError::Starts { page: at, .. } if at == index), "a cut page fails its start table, got {error:?}");
    // A missing page.
    let mut missing = records.clone();
    missing.remove(&page);
    match decode(encoded.root, |key| missing.get(&key).map(|bytes| Sealed::hashed(key, bytes)), alloc()) {
        Err(PagesError::Missing { page: at, key }) => assert_eq!((at, key), (index, page)),
        other => panic!("a missing page read as {:?}", other.map(|_| ())),
    }
}

/// Previous persisted root layout (`BER3`) is rejected cleanly by magic (rule 26 / `layouts.txt`).
#[test]
fn previous_root_magic_ber3_is_rejected() {
    let mut ber3 = StdVec::from(&b"BER3"[..]);
    ber3.extend_from_slice(&[0u8; 64]);
    let key = content_key(&ber3);
    let err = crate::pages::decode_root_hashed(key, &ber3, alloc())
        .err()
        .expect("previous BER3 root magic must be rejected");
    assert_eq!(err, PagesError::Magic { page: crate::pages::ROOT_PAGE });
}

/// A 65,536-page (`bits = 16`) 2-level 256×256 radix answer round-trips deterministically,
/// `decode_root_with` reads zero page leaf shards, and an incremental 4-page edit reads at most 4
/// leaf shards and writes `<= 45 KiB` of root+leaf+directory metadata (instead of 2.1 MiB flat).
#[test]
fn radix_65536_page_root_round_trips_and_four_page_edit_writes_under_45kb_metadata() {
    // Build a clean 4-page incremental edit using the real `Engine` over `StoredPages`:
    // edit 4 tasks belonging to 4 distinct files (1 task changed per file), each on its own
    // page across 4 distinct leaf shards, with zero changes to `places`, `units`, or `findings`.
    let tools = crate::Tools([blaze_drive::key::Tool(ContentId([7; 32])); blaze_drive::key::KINDS]);
    let mut cold = Kept::new_in(alloc());
    cold.answer.serial = Gen(1);
    let phase = Phase::new(Stage::Bodies, 0);
    let old_source = ContentId([1; 32]);
    let old_rec = Key(ContentId([11; 32]));
    let mut all_files: StdVec<Name> = StdVec::new();
    let mut file_tasks: BTreeMap<Name, StdVec<Name>> = BTreeMap::new();
    for f in 0..32u64 {
        let fl = name(Kind::FileItems, 99, f);
        all_files.push(fl);
        cold.places.insert(fl, crate::pages::FilePlace { unit: name(Kind::Units, 99, 0), edition: 3, flags: 1 });
    }
    for i in 0..320u64 {
        let fl = all_files[(i as usize) % all_files.len()];
        let tn = name(Kind::BodyCheck, 99, i);
        file_tasks.entry(fl).or_default().push(tn);
        cold.answer.rows.insert(tn, Row {
            key: tools.key(tn, &old_source), phase, file: fl, source: old_source, out: old_rec.0, serial: Gen(1),
            defines: blaze_own::Vec::new_in(alloc()), reads: blaze_own::Vec::new_in(alloc()), by_cell: blaze_own::Vec::new_in(alloc()),
            findings: 0, inline: false, failed: false,
        });
        cold.causes.insert(tn, (Gen(1), Cause::New));
        cold.records.insert(tn, old_rec);
    }
    for (fl, mut members) in file_tasks {
        members.sort_unstable();
        cold.answer.by_file.insert(fl, blaze_own::Vec::from_slice_in(&members, alloc()));
    }
    let encoded = crate::pages::encode_at_bits(&cold, 16, alloc());
    assert!(encoded.is_radix(), "bits = 16 must use the 2-level radix root");
    assert_eq!(encoded.bits, 16);
    assert!(encoded.pages.is_empty(), "radix encoded keeps flat pages empty (0 bytes)");
    assert_eq!(encoded.page_shards.len(), 256, "65,536 / 256 = 256 leaf shards");
    assert!(!encoded.tree_keys().is_empty(), "tree_keys pins all non-zero shards and dir sub-records");

    let mut store: BTreeMap<Key, StdVec<u8>> = encoded.puts.iter().map(|(k, b)| (*k, b.iter().copied().collect())).collect();

    // Full decode round-trip matches cold encode bit-for-bit.
    let decoded = decode(encoded.root, |key| store.get(&key).map(|bytes| Sealed::hashed(key, bytes)), alloc()).expect("radix decode");
    assert_eq!(
        crate::pages::encode_at_bits(&decoded, 16, alloc()).root,
        encoded.root,
        "radix answer round-trips identically (rule 26)"
    );

    // `decode_root_with` reads ONLY the top root + directory sub-records, ZERO `BEX1` leaf shards.
    let mut leaf_reads_at_open = 0usize;
    let root_v1 = crate::pages::decode_root_with(
        Sealed::hashed(encoded.root, &store[&encoded.root]).expect("root sealed"),
        |key| {
            let bytes = store.get(&key)?;
            if bytes.starts_with(crate::pages::RADIX_SHARD_MAGIC) {
                leaf_reads_at_open += 1;
            }
            Some(Sealed::hashed(key, bytes))
        },
        alloc(),
    )
    .expect("decode radix root");
    assert_eq!(leaf_reads_at_open, 0, "opening radix root must read 0 page leaf shards");
    assert!(root_v1.pages.is_empty(), "radix Root does not materialize 65,536 flat keys");
    assert_eq!(root_v1.page_shards.len(), 256);

    // Pick 3 tasks in one file whose pages + the file's own page are 4 distinct pages across 4
    // distinct leaf shards -> editing those 3 tasks touches EXACTLY 4 task pages (3 task pages + 1
    // file page) in 4 distinct leaf shards, and 1 file-directory shard (places & summary unchanged)!
    let chosen_file = all_files[0];
    let mut picked_tasks: StdVec<Name> = StdVec::new();
    let mut used_shards: BTreeSet<usize> = BTreeSet::new();
    let file_page = blaze_drive::answers::page_of(chosen_file, 16);
    used_shards.insert(crate::pages::Root::shard_of(file_page as usize).0);
    for &tn in cold.answer.by_file[&chosen_file].iter() {
        let p = blaze_drive::answers::page_of(tn, 16);
        let (s, _) = crate::pages::Root::shard_of(p as usize);
        if used_shards.insert(s) {
            picked_tasks.push(tn);
            if picked_tasks.len() == 3 {
                break;
            }
        }
    }
    assert_eq!(picked_tasks.len(), 3, "must find 3 tasks in chosen_file across 3 distinct leaf shards");

    // Pre-open ONLY the pages for our chosen file + its members.
    let mut preloaded_pages: BTreeMap<u32, crate::pages::VerifiedPage<'_>> = BTreeMap::new();
    let mut preload = |p_idx: u32| {
        if preloaded_pages.contains_key(&p_idx) {
            return;
        }
        let (shard_idx, slot_idx) = crate::pages::Root::shard_of(p_idx as usize);
        let shard_key = root_v1.page_shards[shard_idx];
        if shard_key == Key::default() {
            return;
        }
        let shard = crate::pages::decode_radix_shard(
            Sealed::hashed(shard_key, &store[&shard_key]).expect("shard"),
            shard_idx as u32,
            root_v1.bits,
            alloc(),
        )
        .expect("decode shard");
        let page_key = shard.keys[slot_idx];
        if page_key != Key::default() {
            let vp = crate::pages::VerifiedPage::open(
                Sealed::hashed(page_key, &store[&page_key]).expect("page"),
                p_idx,
                root_v1.bits,
            )
            .expect("open page");
            preloaded_pages.insert(p_idx, vp);
        }
    };
    preload(file_page);
    for &tn in cold.answer.by_file[&chosen_file].iter() {
        preload(blaze_drive::answers::page_of(tn, 16));
    }

    struct MapStored<'a> {
        bits: u8,
        pages: &'a BTreeMap<u32, crate::pages::VerifiedPage<'a>>,
    }
    impl crate::StoredPages for MapStored<'_> {
        fn page(&self, name: Name) -> Result<Option<&crate::pages::VerifiedPage<'_>>, PagesError> {
            Ok(self.pages.get(&blaze_drive::answers::page_of(name, self.bits)))
        }
        fn finding(&self, _: Name) -> Option<u32> { None }
        fn refusal(&self) -> Option<PagesError> { None }
        fn visit_rows(&self, visit: &mut dyn FnMut(Name) -> Result<(), crate::LastError>) -> Result<(), crate::LastError> {
            for page in self.pages.values() {
                for n in page.row_names() {
                    visit(n)?;
                }
            }
            Ok(())
        }
    }

    let stored_view = MapStored { bits: 16, pages: &preloaded_pages };
    let new_source = ContentId([2; 32]);
    let new_rec = Key(ContentId([22; 32]));
    let mut members: StdVec<crate::Planned> = StdVec::new();
    for &tn in cold.answer.by_file[&chosen_file].iter() {
        let src = if picked_tasks.contains(&tn) { new_source } else { old_source };
        members.push(crate::Planned { name: tn, phase, file: chosen_file, source: src });
    }
    members.sort_unstable_by_key(|p| p.name);
    let edit = crate::Edit { files: &[(chosen_file, &members)] };
    let mut eng = crate::Engine::new_in(tools, crate::Answer::from_pages(&stored_view, Gen(1), alloc()), &edit, crate::Everything, crate::Plant::None, alloc()).expect("engine");
    assert_eq!(eng.next_phase().expect("phase"), Some(phase));
    assert_eq!(eng.tasks().len(), 3);
    let mut batch = crate::Buffer::new_in(alloc());
    let mut payloads = blaze_own::BTreeMap::new_in(alloc());
    for task in eng.tasks() {
        batch.done.push(crate::Done {
            name: task.planned.name, key: task.key, out: new_rec.0,
            defines: blaze_own::Vec::new_in(alloc()), reads: blaze_own::Vec::new_in(alloc()),
            findings: 0, failed: false,
        });
        payloads.insert(task.planned.name, new_rec);
    }
    eng.merge(core::slice::from_mut(&mut batch)).expect("merge");
    assert_eq!(eng.next_phase().expect("settled"), None);
    let crate::Finished { answer: edit_answer, causes: edit_causes, .. } = eng.finish().expect("finish");
    let edit_kept = Kept::after(blaze_own::BTreeMap::new_in(alloc()), edit_answer, &edit_causes, payloads);

    let mut shards_fetched = 0usize;
    let edited = crate::pages::encode_changes_radix(
        &edit_kept,
        &root_v1,
        alloc(),
        |key| {
            let bytes = store.get(&key)?;
            if bytes.starts_with(crate::pages::RADIX_SHARD_MAGIC) {
                shards_fetched += 1;
            }
            Some(Sealed::hashed(key, bytes))
        },
        |jobs| {
            let mut out = blaze_own::Vec::with_capacity_in(jobs.len(), alloc());
            for job in jobs {
                out.push(job.run(alloc()));
            }
            out
        },
    )
    .expect("incremental radix encode");

    assert_eq!(shards_fetched, 4, "4-page edit across 4 shards must fetch only those 4 leaf shards");
    assert_eq!(edited.task_puts, 4, "only the 4 changed task pages (3 tasks + 1 file) were written");
    let task_puts = edited.task_puts as usize;
    let meta_bytes: usize = edited.puts[task_puts..].iter().map(|(_, b)| b.len()).sum();
    // 4 leaf shards (4 * 8,201 = 32,804 B) + 1 file shard (~300 B) + 1 top root (~8,814 B) = ~41.9 KiB <= 45 KiB!
    // (places, summary, and the other 15 file shards were UNCHANGED -> 0 bytes written!)
    assert!(
        meta_bytes <= 45 * 1024,
        "4-page edit metadata must be <= 45 KiB (got {meta_bytes} bytes; flat was 2,097+ KiB)"
    );
    assert_eq!(edited.page_deltas.as_ref().map(|d| d.len()), Some(4), "exact 4 page deltas reported");

    // Release borrows of `store` from `preloaded_pages` / `edit_kept`, then insert the new puts
    // and assert full decode round-trips to the exact same root key as a cold `encode_at_bits`.
    drop(edit_kept);
    drop(edit_causes);
    drop(stored_view);
    drop(preloaded_pages);
    for (k, b) in edited.puts.iter() {
        store.insert(*k, b.iter().copied().collect());
    }
    let restored = decode(edited.root, |key| store.get(&key).map(|bytes| Sealed::hashed(key, bytes)), alloc()).expect("decode edited radix answer");
    assert_eq!(
        crate::pages::encode_at_bits(&restored, 16, alloc()).root,
        edited.root,
        "incremental radix edit produces the exact same root key as a cold encode"
    );
}
