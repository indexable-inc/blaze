//! The published set against a model origin (rule 43; published.md): the tape draws records of
//! every namespace and class, two publishers lay them out in different orders, the origin is
//! damaged (a cut file, a flipped byte in a segment, an index page or the prefix, a segment file
//! holding another's bytes, an old prefix replayed, an index taken away, a prefix forged by
//! another key, answers cut short), the limits sit at and past what the set needs, and a reader
//! asks key sets in batches.
//!
//! The oracle: a record the reader yields is byte-equal to the one published under its key, and
//! only a publishable record is ever published; a damaged byte is never yielded (a typed
//! refusal instead); a published key is never answered absent; on a calm origin every published
//! key within the limits is found; no ask passes its bound; a batch takes at most three waves;
//! two publishers of one set write equal origins; no object's name could be a path.
//!
//! Every allocation is from an `Inline` arena on the case's own stack (no ambient heap): the
//! cases run on one thread with a stack large enough for it.

use blaze_cas::plain::Namespace;
use blaze_ids::ContentId;
use blaze_keyed::kind::Tag;
use blaze_own::{Alloc, Erased, Inline, Vec};
use blaze_sim::{Broke, Config, Tape};

use crate::published::{Candidate, Class, Laid, Layout, LayOutError, Unpublishable, lay_out, publishable};
use crate::read::Ed25519Keys;
use crate::read::form::{Object, PAGE_ENTRIES_MOST, REMOTE_ENTRY};
use crate::read::machine::{Answer, Ask, Limits, OriginError, Poll, Reader};
use crate::trust::Signer;
use crate::{PLANT, Plant};

/// A case's arena, and the stack it lives on.
const ARENA: usize = 8 << 20;
const STACK: usize = 64 << 20;

/// What a reader built for a case takes of the prefix: far more than any case's set needs.
const PREFIX_MOST: u32 = 256 << 10;

/// Runs `body` on a thread whose stack holds a case's arena, and passes on its panic.
fn on_a_big_stack(body: impl FnOnce() + Send + 'static) {
    let thread = std::thread::Builder::new().stack_size(STACK).spawn(body).expect("a thread for the simulation");
    if let Err(panic) = thread.join() {
        std::panic::resume_unwind(panic);
    }
}

/// A finding, in blaze-sim's words (its `Broke` holds the message as text).
fn broke(why: &'static str) -> Broke {
    Broke::Oracle(why.into())
}

/// One record the walk found.
struct Rec<'a> {
    key: ContentId,
    tag: Tag,
    namespace: Namespace,
    class: Class,
    bytes: Vec<u8, Alloc<'a>>,
}

/// The model's own statement of the predicate (published.md, point 1), not the code's.
fn truly_publishable(rec: &Rec<'_>) -> bool {
    rec.namespace == Namespace::PUBLIC && matches!(rec.class, Class::Toolchain | Class::Registry { .. })
}

fn records<'a>(tape: &Tape, alloc: Alloc<'a>) -> Vec<Rec<'a>, Alloc<'a>> {
    let n = 1 + tape.below(40, "records") as usize;
    let mut out = Vec::with_capacity_in(n, alloc);
    for i in 0..n {
        let mut key = [0u8; 32];
        tape.fill(&mut key, "key");
        // Distinct keys, even on a calm tape.
        key[30] = 0xa5;
        key[31] = i as u8;
        let len = if tape.below(8, "long") == 0 { tape.below(700, "long len") } else { tape.below(90, "len") } as usize;
        let mut bytes = Vec::with_capacity_in(len, alloc);
        bytes.resize(len, 0u8);
        tape.fill(&mut bytes[..], "payload");
        let namespace = match tape.below(8, "namespace") {
            6 => Namespace::LOCAL,
            7 => Namespace(7),
            _ => Namespace::PUBLIC,
        };
        let class = match tape.below(8, "class") {
            5 => Class::Answer,
            6 => Class::Program,
            3 | 4 => Class::Registry { checksum: ContentId([i as u8; 32]) },
            _ => Class::Toolchain,
        };
        let tag = Tag(3 + tape.below(2, "tag") as u16);
        out.push(Rec { key: ContentId(key), tag, namespace, class, bytes });
    }
    out
}

fn candidate<'r>(rec: &'r Rec<'_>) -> Candidate<'r> {
    Candidate { key: rec.key, tag: rec.tag, namespace: rec.namespace, class: rec.class, bytes: &rec.bytes[..] }
}

fn shuffle<T>(tape: &Tape, items: &mut [T]) {
    for i in (1..items.len()).rev() {
        let j = tape.index(i + 1, "order");
        items.swap(i, j);
    }
}

fn signer(tape: &Tape, label: &'static str) -> Signer {
    let mut seed = [0u8; 32];
    tape.fill(&mut seed, label);
    seed[0] |= 1;
    Signer::from_seed(&seed)
}

/// An origin directory: its files by object.
struct Origin<'a> {
    alloc: Alloc<'a>,
    files: Vec<(Object, Vec<u8, Alloc<'a>>), Alloc<'a>>,
}

impl<'a> Origin<'a> {
    fn new(alloc: Alloc<'a>) -> Origin<'a> {
        Origin { alloc, files: Vec::new_in(alloc) }
    }

    fn file(&self, object: &Object) -> Option<&[u8]> {
        self.files.iter().find(|(o, _)| o == object).map(|(_, bytes)| &bytes[..])
    }

    fn file_mut(&mut self, object: &Object) -> Option<&mut Vec<u8, Alloc<'a>>> {
        self.files.iter_mut().find(|(o, _)| o == object).map(|(_, bytes)| bytes)
    }

    /// Writes `bytes` as `object`, replacing what is there (a rename over it).
    fn put(&mut self, object: Object, bytes: &[u8]) {
        let mut copy = Vec::with_capacity_in(bytes.len(), self.alloc);
        copy.extend_from_slice(bytes);
        match self.file_mut(&object) {
            Some(held) => *held = copy,
            None => self.files.push((object, copy)),
        }
    }

    fn put_all(&mut self, laid: &Laid<'_>) {
        for sealed in laid.objects.iter() {
            self.put(sealed.object, &sealed.bytes[..]);
        }
    }

    fn remove(&mut self, object: &Object) {
        if let Some(at) = self.files.iter().position(|(o, _)| o == object) {
            self.files.remove_swap(at);
        }
    }

    /// The objects that are `which`: 0 segments, 1 index objects.
    fn of_kind(&self, which: u8) -> Vec<Object, Alloc<'a>> {
        let mut out = Vec::new_in(self.alloc);
        for (object, _) in self.files.iter() {
            let hit = match object {
                Object::Segment(_) => which == 0,
                Object::Index(_) => which == 1,
                Object::Head | Object::Prefix | Object::Manifest(_) | Object::Summary(_) => false,
            };
            if hit {
                out.push(*object);
            }
        }
        out
    }
}

/// What was done to the origin after it was published.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fault {
    None,
    /// A file cut short.
    Truncate,
    FlipSegment,
    FlipIndex,
    FlipPrefix,
    /// A segment's file holds another segment's bytes: the index names it by the wrong digest
    /// and length.
    WrongSegment,
    /// An older prefix (of a smaller set) replayed over the newest.
    OldPrefix,
    /// The prefix names an index object the origin does not hold.
    MissingIndex,
    /// A prefix of the same set signed by a key the reader was not given.
    Forged,
}

fn flip(tape: &Tape, bytes: &mut [u8]) {
    if bytes.is_empty() {
        return;
    }
    let at = tape.index(bytes.len(), "flip at");
    bytes[at] ^= 1 << tape.below(8, "flip bit");
}

/// The bound an ask must keep, or `None` for an object a prefix reader never asks.
fn bound(ask: &Ask, limits: &Limits) -> Option<u64> {
    match ask.object {
        Object::Prefix => Some(u64::from(limits.prefix_most)),
        Object::Segment(_) => Some(u64::from(limits.range_most.max(limits.record_most))),
        Object::Index(_) => Some(u64::from(PAGE_ENTRIES_MOST) * REMOTE_ENTRY as u64),
        Object::Head | Object::Manifest(_) | Object::Summary(_) => None,
    }
}

/// One batch against the origin: each wave's asks are answered (cut short when `short` and the
/// tape says so) and handed back in an order the tape picks. Returns the waves it took.
fn batch(tape: &Tape, reader: &mut Reader<'_>, origin: &Origin<'_>, keys: &[ContentId], short: bool, now: &mut u64, alloc: Alloc<'_>) -> Result<u32, Broke> {
    let limits = *reader.limits();
    reader.begin(keys, *now).map_err(|_| Broke::Harness("a batch was not begun".into()))?;
    let before = reader.report().waves;
    let mut asks: Vec<Ask, Alloc<'_>> = Vec::new_in(alloc);
    let mut replies: Vec<(u32, Result<usize, OriginError>), Alloc<'_>> = Vec::new_in(alloc);
    for _ in 0..10_000 {
        asks.clear();
        if let Poll::Done = reader.poll(*now, &mut asks) {
            return Ok(reader.report().waves - before);
        }
        replies.clear();
        for ask in asks.iter() {
            match bound(ask, &limits) {
                None => return Err(broke("a prefix reader asked the head, a manifest or a summary on its own")),
                Some(most) if u64::from(ask.len) > most => return Err(broke("an ask past its limit")),
                Some(_) => {}
            }
            let Some(into) = reader.buffer(ask.id) else { continue };
            let result = match origin.file(&ask.object) {
                None => Err(OriginError::Missing),
                Some(bytes) => {
                    let from = (ask.at as usize).min(bytes.len());
                    let mut n = (bytes.len() - from).min(into.len());
                    if short && n > 0 && tape.below(3, "short") == 0 {
                        n = tape.below(n as u64, "short to") as usize;
                    }
                    into[..n].copy_from_slice(&bytes[from..from + n]);
                    Ok(n)
                }
            };
            replies.push((ask.id, result));
        }
        shuffle(tape, &mut replies[..]);
        for &(id, result) in replies.iter() {
            reader.replied(id, result, *now);
        }
        *now += 1;
    }
    Err(broke("a hang: the batch did not end in 10,000 steps"))
}

fn case(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    PLANT.set(plant);
    let arena = Inline::<ARENA>::new();
    let erased = Erased::new(&arena);
    let result = case_in(tape, erased.handle());
    PLANT.set(Plant::None);
    result
}

fn case_in(tape: &Tape, alloc: Alloc<'_>) -> Result<(), Broke> {
    let recs = records(tape, alloc);
    let trusted = signer(tape, "seed");
    let layout = Layout { segment_bytes: 64 + tape.below(2048, "segment bytes"), page_entries: 1 + tape.below(8, "page entries") as u32, generation: 2 };

    // Two publishers, one set (a record may be given twice), two orders.
    let mut given: Vec<Candidate<'_>, Alloc<'_>> = Vec::new_in(alloc);
    for rec in recs.iter() {
        given.push(candidate(rec));
        if tape.below(5, "twice") == 0 {
            given.push(candidate(rec));
        }
    }
    let mut reordered: Vec<Candidate<'_>, Alloc<'_>> = Vec::with_capacity_in(given.len(), alloc);
    reordered.extend_from_slice(&given[..]);
    shuffle(tape, &mut reordered[..]);
    let laid = lay_out(&given[..], layout, &trusted, alloc).map_err(|_| Broke::Harness("a set was not laid out".into()))?;
    let again = lay_out(&reordered[..], layout, &trusted, alloc).map_err(|_| Broke::Harness("a set was not laid out".into()))?;
    let equal = laid.objects.len() == again.objects.len()
        && laid.objects.iter().zip(again.objects.iter()).all(|(a, b)| a.object == b.object && a.bytes[..] == b.bytes[..]);
    if !equal {
        return Err(broke("two publishers of one set wrote different origins"));
    }
    let publishable_count = recs.iter().filter(|rec| truly_publishable(rec)).count() as u32;
    if laid.published != publishable_count {
        return Err(broke("an unpublishable record was published"));
    }
    for sealed in laid.objects.iter() {
        let name = sealed.object.name();
        let text = name.as_str();
        if text.is_empty() || text == "." || text == ".." || !text.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
            return Err(broke("an object's name could be a path"));
        }
    }

    // The origin, then what was done to it.
    let mut origin = Origin::new(alloc);
    let mut served: Vec<bool, Alloc<'_>> = Vec::with_capacity_in(recs.len(), alloc);
    for rec in recs.iter() {
        served.push(truly_publishable(rec));
    }
    let fault = match tape.below(16, "fault") {
        1 => Fault::Truncate,
        2 | 9 => Fault::FlipSegment,
        3 => Fault::FlipIndex,
        4 => Fault::FlipPrefix,
        5 => Fault::WrongSegment,
        6 => Fault::OldPrefix,
        7 => Fault::MissingIndex,
        8 => Fault::Forged,
        _ => Fault::None,
    };
    let mut forged = false;
    match fault {
        Fault::OldPrefix => {
            // Generation 1 held a part of the set; its objects stay (immutable), and its prefix
            // is put back over generation 2's.
            let mut older: Vec<Candidate<'_>, Alloc<'_>> = Vec::new_in(alloc);
            for (i, rec) in recs.iter().enumerate() {
                if tape.below(2, "in the older set") == 0 {
                    older.push(candidate(rec));
                } else {
                    served[i] = false;
                }
            }
            let old = lay_out(&older[..], Layout { generation: 1, ..layout }, &trusted, alloc).map_err(|_| Broke::Harness("the older set was not laid out".into()))?;
            origin.put_all(&old);
            origin.put_all(&laid);
            let old_prefix = old.objects.iter().find(|sealed| sealed.object == Object::Prefix).ok_or_else(|| Broke::Harness("no prefix".into()))?;
            origin.put(Object::Prefix, &old_prefix.bytes[..]);
        }
        Fault::Forged => {
            origin.put_all(&laid);
            let forger = signer(tape, "forger");
            if forger.public() != trusted.public() {
                let lie = lay_out(&given[..], layout, &forger, alloc).map_err(|_| Broke::Harness("the forged set was not laid out".into()))?;
                let prefix = lie.objects.iter().find(|sealed| sealed.object == Object::Prefix).ok_or_else(|| Broke::Harness("no prefix".into()))?;
                origin.put(Object::Prefix, &prefix.bytes[..]);
                forged = true;
            }
        }
        _ => origin.put_all(&laid),
    }
    match fault {
        Fault::Truncate => {
            let at = tape.index(origin.files.len(), "file cut");
            let len = origin.files[at].1.len();
            if len > 0 {
                let keep = tape.below(len as u64, "cut to") as usize;
                origin.files[at].1.truncate(keep);
            }
        }
        Fault::FlipSegment | Fault::FlipIndex | Fault::MissingIndex => {
            let kind = if fault == Fault::FlipSegment { 0 } else { 1 };
            let objects = origin.of_kind(kind);
            if !objects.is_empty() {
                let object = objects[tape.index(objects.len(), "which object")];
                if fault == Fault::MissingIndex {
                    origin.remove(&object);
                } else if let Some(bytes) = origin.file_mut(&object) {
                    for _ in 0..1 + tape.below(3, "flips") {
                        flip(tape, &mut bytes[..]);
                    }
                }
            }
        }
        Fault::FlipPrefix => {
            if let Some(bytes) = origin.file_mut(&Object::Prefix) {
                flip(tape, &mut bytes[..]);
            }
        }
        Fault::WrongSegment => {
            let segments = origin.of_kind(0);
            if segments.len() >= 2 {
                let a = tape.index(segments.len(), "segment a");
                let b = (a + 1 + tape.index(segments.len() - 1, "segment b")) % segments.len();
                let other = origin.file(&segments[b]).map(|bytes| {
                    let mut copy = Vec::with_capacity_in(bytes.len(), alloc);
                    copy.extend_from_slice(bytes);
                    copy
                });
                if let Some(other) = other {
                    origin.put(segments[a], &other[..]);
                }
            }
        }
        Fault::None | Fault::OldPrefix | Fault::Forged => {}
    }

    // The reader, its limits at and past what the set needs.
    let keys_given = [trusted.public()];
    let check = Ed25519Keys { namespace: Namespace::PUBLIC, keys: &keys_given };
    let mut limits = Limits { prefix_most: PREFIX_MOST, ..Limits::PUBLISHED };
    let small = tape.below(if fault == Fault::FlipSegment { 2 } else { 4 }, "small limits") == 0;
    if small {
        limits.record_most = 100;
        limits.range_most = 64;
        limits.gap_most = tape.below(64, "gap") as u32;
        limits.in_flight_most = 1 + tape.below(4, "in flight") as u32;
    }
    let prefix_len = origin.file(&Object::Prefix).map_or(0, |bytes| bytes.len());
    let mut prefix_fits = true;
    match tape.below(8, "prefix limit") {
        // At the prefix's length: taken. One less: refused. (0 would be a reader without a
        // prefix, which this test does not drive.)
        0 if prefix_len > 0 => limits.prefix_most = prefix_len as u32,
        1 if prefix_len > 1 => {
            limits.prefix_most = prefix_len as u32 - 1;
            prefix_fits = false;
        }
        _ => {}
    }
    if tape.below(6, "record limit at a record") == 0 && !recs.is_empty() {
        limits.record_most = recs[tape.index(recs.len(), "record at the limit")].bytes.len() as u32;
    }
    let short = tape.below(4, "short answers") == 0;
    let calm = fault == Fault::None && !short && prefix_fits;

    let mut reader = Reader::new_in(Namespace::PUBLIC, &check, limits, alloc);
    let mut now = 0u64;
    for round in 0..1 + tape.below(3, "batches") {
        // The keys, computed before any fetch: some records (every unpublishable one in the
        // first batch), and keys nobody published.
        let mut keys: Vec<ContentId, Alloc<'_>> = Vec::new_in(alloc);
        for rec in recs.iter() {
            if (round == 0 && !truly_publishable(rec)) || tape.below(3, "asked") != 0 {
                keys.push(rec.key);
            }
        }
        for _ in 0..tape.below(4, "unknown keys") {
            let mut key = [0u8; 32];
            tape.fill(&mut key, "unknown key");
            key[30] = 0x5b;
            keys.push(ContentId(key));
        }
        shuffle(tape, &mut keys[..]);
        let waves = batch(tape, &mut reader, &origin, &keys[..], short, &mut now, alloc)?;
        if waves > 3 {
            return Err(broke("a batch took more than three waves"));
        }
        for (i, key) in keys.iter().enumerate() {
            let rec = recs.iter().position(|rec| rec.key == *key);
            match reader.answer(i) {
                Answer::Found { tag, .. } => {
                    let Some(at) = rec else { return Err(broke("a key nobody published was found")) };
                    let rec = &recs[at];
                    if !truly_publishable(rec) {
                        return Err(broke("an unpublishable record was published"));
                    }
                    if reader.found(i) != Some(&rec.bytes[..]) || tag != rec.tag {
                        return Err(broke("wrong bytes for a published key"));
                    }
                    if forged {
                        return Err(broke("a forged prefix was believed"));
                    }
                }
                Answer::Absent => {
                    if rec.is_some_and(|at| served[at]) {
                        return Err(broke("a published key was answered absent"));
                    }
                }
                Answer::Unfetched(_) => {
                    if calm && rec.is_some_and(|at| served[at] && recs[at].bytes.len() <= limits.record_most as usize) {
                        return Err(broke("a published record was not found on a calm origin"));
                    }
                }
                Answer::Pending => return Err(broke("a key left pending after its batch")),
            }
        }
    }
    Ok(())
}

/// The invariant: published remote reads return only verified true bytes of public settled records or typed refusals within configured index limits.
#[test]
fn published_records_are_true_bytes_or_typed_refusals_in_three_waves() {
    on_a_big_stack(|| blaze_sim::check(Config::cases(400), |tape| case(tape, Plant::None)));
}

#[test]
fn the_oracle_catches_a_second_range_answered_unverified() {
    on_a_big_stack(|| blaze_sim::control(Config::cases(600), Plant::None, Plant::SecondRangeUnverified, "wrong bytes", case));
}

#[test]
fn the_oracle_catches_a_private_record_admitted() {
    on_a_big_stack(|| blaze_sim::control(Config::cases(300), Plant::None, Plant::PrivateAdmitted, "an unpublishable record was published", case));
}

#[test]
fn the_oracle_catches_an_index_length_taken_past_its_limit() {
    on_a_big_stack(|| blaze_sim::control(Config::cases(600), Plant::None, Plant::SizeFromWire, "an ask past its limit", case));
}

/// The predicate over every namespace and class: published only when public AND settled, and
/// each refusal names which failed.
#[test]
fn only_public_settled_records_are_publishable() {
    let classes = [Class::Toolchain, Class::Registry { checksum: ContentId([1; 32]) }, Class::Answer, Class::Program];
    for namespace in [Namespace::PUBLIC, Namespace::LOCAL, Namespace(9)] {
        for class in classes {
            let got = publishable(namespace, class);
            match (namespace == Namespace::PUBLIC, class.settled()) {
                (true, true) => assert!(got.is_ok(), "{namespace:?} {class:?}"),
                (false, _) => assert_eq!(got, Err(Unpublishable::Namespace { found: namespace })),
                (true, false) => assert_eq!(got, Err(Unpublishable::Unsettled { class })),
            }
        }
    }
}

/// Two values under one key are refused by name; one value given twice is one record.
#[test]
fn two_values_under_one_key_are_refused() {
    on_a_big_stack(|| {
        let arena = Inline::<ARENA>::new();
        let erased = Erased::new(&arena);
        let alloc = erased.handle();
        let signer = Signer::from_seed(&[3; 32]);
        let layout = Layout { segment_bytes: 4096, page_entries: 4, generation: 1 };
        let key = ContentId([7; 32]);
        let one = Candidate { key, tag: Tag(3), namespace: Namespace::PUBLIC, class: Class::Toolchain, bytes: b"one" };
        let other = Candidate { bytes: b"two", ..one };
        let twice = lay_out(&[one, one], layout, &signer, alloc).expect("one value twice is one record");
        assert_eq!(twice.published, 1);
        match lay_out(&[one, other], layout, &signer, alloc) {
            Err(LayOutError::TwoValues { key: refused, .. }) => assert_eq!(refused, key),
            Err(error) => panic!("refused, but not as two values under one key: {error}"),
            Ok(_) => panic!("two values under one key were laid out"),
        }
    });
}

/// The measure for a small set (runtime.md row H0): a fresh reader asking 64 records of a set
/// of 64 in one batch, then 8 again. Prints bytes, waves and asks.
#[test]
fn a_small_set_is_three_waves_from_a_fresh_home() {
    on_a_big_stack(|| {
        let arena = Inline::<ARENA>::new();
        let erased = Erased::new(&arena);
        let alloc = erased.handle();
        let signer = Signer::from_seed(&[5; 32]);
        let mut recs: Vec<Rec<'_>, Alloc<'_>> = Vec::new_in(alloc);
        for i in 0..64u32 {
            let mut key = [0u8; 32];
            key[0] = (i * 37) as u8;
            key[1] = i as u8;
            let len = 200 + (i as usize * 311) % 2000;
            let mut bytes = Vec::with_capacity_in(len, alloc);
            bytes.resize(len, (i % 251) as u8);
            recs.push(Rec { key: ContentId(key), tag: Tag(3), namespace: Namespace::PUBLIC, class: Class::Toolchain, bytes });
        }
        let mut given: Vec<Candidate<'_>, Alloc<'_>> = Vec::new_in(alloc);
        for rec in recs.iter() {
            given.push(candidate(rec));
        }
        let layout = Layout { segment_bytes: 32 << 10, page_entries: 64, generation: 1 };
        let laid = lay_out(&given[..], layout, &signer, alloc).expect("laid out");
        let mut origin = Origin::new(alloc);
        origin.put_all(&laid);
        let stored: usize = origin.files.iter().map(|(_, bytes)| bytes.len()).sum();
        let keys_given = [signer.public()];
        let check = Ed25519Keys { namespace: Namespace::PUBLIC, keys: &keys_given };
        let mut reader = Reader::new_in(Namespace::PUBLIC, &check, Limits::PUBLISHED, alloc);
        let mut keys: Vec<ContentId, Alloc<'_>> = Vec::new_in(alloc);
        for rec in recs.iter() {
            keys.push(rec.key);
        }
        let calm = Tape::calm();
        let mut now = 0;
        let waves = batch(&calm, &mut reader, &origin, &keys[..], false, &mut now, alloc).expect("a calm batch");
        let r = reader.report();
        println!(
            "published, a fresh home: {} records in {} segments ({} bytes stored); {} found in {} waves, {} asks (prefix {}, index {}, payload {}), {} bytes fetched for {} used",
            laid.published, laid.segments, stored, r.found, waves, r.asks, r.prefix_asks, r.page_asks, r.payload_asks, r.bytes_fetched, r.bytes_used
        );
        assert_eq!((r.found, waves), (64, 3), "every record found, in three waves");
        let again = batch(&calm, &mut reader, &origin, &keys[..8], false, &mut now, alloc).expect("a calm batch");
        let after = reader.report();
        println!("published, 8 more keys in the same command: {} waves, {} more asks, {} more bytes", again, after.asks - r.asks, after.bytes_fetched - r.bytes_fetched);
        assert!(again <= 2, "a later batch reads no prefix: {again} waves");
    });
}
