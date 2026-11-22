//! The read path against a simulated origin (rule 43, remote-read.md "Proof"): random stores,
//! batches and origin misbehaviour (slow, absent, short, flipped, reordered, never answering, a
//! head naming a segment that is not there, a forged head), a crash at any write-through; and
//! the hostile inputs of each trust step, one test each. Every planted bug of the read path
//! ([`Plant`]) is caught by the oracle that states it.

use std::alloc::Global;
use std::collections::BTreeMap;

use blaze_cas::plain::{Namespace, seal_segment};
use blaze_ids::ContentId;
use blaze_keyed::{exec::RecordRef, kind::Tag};
use blaze_own::{Alloc, Erased, Vec as OwnVec};
use blaze_sim::{Broke, Config, Flight, Nanos, Tape};

use super::form::{self, HEAD_LEN, Head, Object, Refused, Row, SignatureWhy, Summary};
use super::machine::{Answer, Ask, Limits, OriginError, Poll, Reader};
use super::{Ed25519Keys, fetch, publish};
use crate::trust::{PublicKey, Signer};
use crate::{PLANT, Plant};

const NS: Namespace = Namespace::PUBLIC;
const TAG: Tag = Tag(3);

/// The bytes of an origin by object name, and its records' truth.
struct World {
    objects: BTreeMap<String, std::vec::Vec<u8>>,
    truth: BTreeMap<ContentId, std::vec::Vec<u8>>,
    /// Segments whose objects were taken away after the head named them.
    gone: usize,
}

fn key(tape: &Tape) -> ContentId {
    let mut bytes = [0u8; 32];
    tape.fill(&mut bytes, "key");
    ContentId(bytes)
}

/// Records spread over one to four segments (a record may sit in two, equal bytes), sealed into
/// an origin by `signer` with pages of 1 to 8 entries.
fn world(tape: &Tape, signer: &Signer, alloc: Alloc<'_>) -> World {
    let n = tape.below(40, "records") as usize;
    let mut truth = BTreeMap::new();
    for i in 0..n {
        let mut k = key(tape);
        k.0[31] = i as u8; // distinct even on a calm tape
        k.0[30] = 0x5a;
        let len = if tape.below(8, "long") == 0 { tape.below(600, "len") } else { tape.below(80, "len") } as usize;
        let mut payload = vec![0u8; len];
        tape.fill(&mut payload, "payload");
        truth.insert(k, payload);
    }
    let segments = 1 + tape.below(4, "segments") as usize;
    let mut parts: std::vec::Vec<std::vec::Vec<(RecordRef, &[u8])>> = (0..segments).map(|_| std::vec::Vec::new()).collect();
    for (k, v) in &truth {
        let s = tape.below(segments as u64, "segment of") as usize;
        parts[s].push((RecordRef { tag: TAG, key: *k }, v.as_slice()));
        if tape.below(6, "twice") == 0 {
            let t = tape.below(segments as u64, "second segment") as usize;
            parts[t].push((RecordRef { tag: TAG, key: *k }, v.as_slice()));
        }
    }
    let mut sealed: std::vec::Vec<std::vec::Vec<u8>> = std::vec::Vec::new();
    for records in &parts {
        let mut out = std::vec::Vec::new();
        seal_segment(NS, records, &mut out).expect("seal");
        sealed.push(out);
    }
    let refs: std::vec::Vec<&[u8]> = sealed.iter().map(|s| s.as_slice()).collect();
    let page_entries = 1 + tape.below(8, "page entries") as u32;
    let objects = publish(NS, 1, signer, &refs, page_entries, alloc).expect("publish");
    let objects = objects.iter().map(|o| (o.object.name().as_str().to_owned(), o.bytes.as_slice().to_vec())).collect();
    World { objects, truth, gone: 0 }
}

/// Takes away one segment's objects (its summary, index or payloads), as an origin missing what
/// its head names.
fn take_away(tape: &Tape, world: &mut World) {
    let names: std::vec::Vec<String> = world.objects.keys().filter(|n| n.starts_with("sum-") || n.starts_with("idx-") || n.starts_with("seg-")).cloned().collect();
    if names.is_empty() {
        return;
    }
    let name = &names[tape.index(names.len(), "taken away")];
    world.objects.remove(name);
    world.gone += 1;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mood {
    Calm,
    /// Each reply about objects of `target` (0: any, 1: head, 2: manifest, 3: summary, 4: index,
    /// 5: segment) may be slow, absent, short, flipped or lost, one in three; any is reordered.
    Hostile(u8),
}

/// How a run went, for the oracle.
struct Ran {
    answers: std::vec::Vec<(ContentId, Answer, Option<std::vec::Vec<u8>>)>,
    steps: u32,
    ended_ns: u64,
}

/// One batch against the simulated origin: every ask is answered by the tape's choice of
/// behaviour after a delay, in an order the tape picks; the clock jumps to the next due reply or
/// deadline. The oracle's per-ask rule is checked as each ask is made.
fn run(tape: &Tape, reader: &mut Reader<'_>, world: &World, keys: &[ContentId], mood: Mood, start_ns: u64) -> Result<Ran, Broke> {
    let limits = *reader.limits();
    let most_ask = limits.range_most.max(limits.record_most).max(form::MANIFEST_MOST as u32).max(form::SUMMARY_MOST as u32);
    let mut now = start_ns;
    reader.begin(keys, now).map_err(|e| Broke::Harness(format!("begin: {e}")))?;
    let mut flight: Flight<(u32, Result<std::vec::Vec<u8>, OriginError>)> = Flight::new();
    let global = Erased::new(Global);
    let mut asks: OwnVec<Ask, Alloc<'_>> = OwnVec::new_in(global.handle());
    let mut steps = 0u32;
    loop {
        steps += 1;
        if steps > 100_000 {
            return Err(Broke::Oracle("a hang: the batch did not end in 100,000 steps".into()));
        }
        asks.clear();
        let until = match reader.poll(now, &mut asks) {
            Poll::Done => break,
            Poll::Waiting { until } => until,
        };
        for ask in asks.iter() {
            let bound = match ask.object {
                Object::Segment(_) => limits.range_most.max(limits.record_most),
                _ => most_ask,
            };
            if ask.len > bound {
                return Err(Broke::Oracle(format!("an ask past its limit: {} bytes of {} (bound {bound})", ask.len, ask.object)));
            }
            let mut reply = match world.objects.get(ask.object.name().as_str()) {
                None => Err(OriginError::Missing),
                Some(bytes) => {
                    let from = (ask.at as usize).min(bytes.len());
                    let to = (from + ask.len as usize).min(bytes.len());
                    Ok(bytes[from..to].to_vec())
                }
            };
            let mut delay = tape.below(1_000_000, "delay");
            let kind = match ask.object {
                Object::Head | Object::Prefix => 1,
                Object::Manifest(_) => 2,
                Object::Summary(_) => 3,
                Object::Index(_) => 4,
                Object::Segment(_) => 5,
            };
            if let Mood::Hostile(target) = mood
                && (target == 0 || target == kind)
            {
                match tape.below(18, "misbehave") {
                    0 => delay = limits.ask_ns + 1 + tape.below(limits.ask_ns, "slow"),
                    1 => reply = Err(OriginError::Missing),
                    2 => reply = Err(OriginError::Unreadable),
                    3 => {
                        if let Ok(bytes) = &mut reply
                            && !bytes.is_empty()
                        {
                            let keep = tape.below(bytes.len() as u64, "short") as usize;
                            bytes.truncate(keep);
                        }
                    }
                    4 | 6 | 7 => {
                        if let Ok(bytes) = &mut reply
                            && !bytes.is_empty()
                        {
                            let at = tape.index(bytes.len(), "flip at");
                            bytes[at] ^= 1 << tape.below(8, "flip bit");
                        }
                    }
                    5 => continue, // never answered
                    _ => {}
                }
            }
            flight.ask_at(Nanos(now + delay), (ask.id, reply));
        }
        match flight.deliver(tape, Nanos(now)) {
            Some((id, reply)) => {
                let result = match reply {
                    Ok(bytes) => match reader.buffer(id) {
                        Some(into) => {
                            let n = bytes.len().min(into.len());
                            into[..n].copy_from_slice(&bytes[..n]);
                            Ok(n)
                        }
                        None => Ok(0),
                    },
                    Err(e) => Err(e),
                };
                reader.replied(id, result, now);
            }
            None => {
                let next = flight.earliest().map_or(until, |due| due.0.min(until));
                now = next.max(now + 1);
            }
        }
    }
    let answers = keys.iter().enumerate().map(|(i, k)| (*k, reader.answer(i), reader.found(i).map(|b| b.to_vec()))).collect();
    Ok(Ran { answers, steps, ended_ns: now })
}

/// The oracle over one batch: found bytes are the truth's; absent only when the truth lacks the
/// key; when calm and whole, every key held is found; the batch ends by its deadline.
fn judge(ran: &Ran, world: &World, calm_and_whole: bool, start_ns: u64, limits: &Limits) -> Result<(), Broke> {
    for (k, answer, bytes) in &ran.answers {
        let held = world.truth.get(k);
        match answer {
            Answer::Found { .. } => {
                if bytes.as_deref() != held.map(|v| v.as_slice()) {
                    return Err(Broke::Oracle(format!("wrong bytes for key {:02x?}: found {:?}, the truth is {:?}", &k.0[..4], bytes.as_ref().map(|b| b.len()), held.map(|b| b.len()))));
                }
            }
            Answer::Absent => {
                if held.is_some() {
                    return Err(Broke::Oracle(format!("absent but held: key {:02x?}", &k.0[..4])));
                }
            }
            Answer::Unfetched(why) => {
                if calm_and_whole && held.is_some_and(|v| v.len() <= limits.record_most as usize) {
                    return Err(Broke::Oracle(format!("a key the origin holds was not found: {why}")));
                }
                if calm_and_whole && held.is_none() {
                    return Err(Broke::Oracle(format!("a key the origin lacks was not answered absent: {why}")));
                }
            }
            Answer::Pending => return Err(Broke::Oracle("a key left pending after the batch ended".into())),
        }
    }
    if ran.ended_ns > start_ns + limits.batch_ns + limits.ask_ns + 2_000_000 {
        return Err(Broke::Oracle(format!("the batch outlived its deadline: ended at {} ns", ran.ended_ns - start_ns)));
    }
    Ok(())
}

/// A batch: some keys the truth holds, some it does not.
fn batch(tape: &Tape, world: &World) -> std::vec::Vec<ContentId> {
    let held: std::vec::Vec<ContentId> = world.truth.keys().copied().collect();
    let mut keys = std::vec::Vec::new();
    for _ in 0..tape.below(30, "batch") {
        if !held.is_empty() && tape.below(3, "held?") != 0 {
            keys.push(held[tape.index(held.len(), "which")]);
        } else {
            keys.push(key(tape));
        }
    }
    keys
}

fn signer(tape: &Tape) -> Signer {
    let mut seed = [0u8; 32];
    tape.fill(&mut seed, "seed");
    seed[0] |= 1;
    Signer::from_seed(&seed)
}

/// One case: an origin (maybe missing what its head names, maybe forged by an untrusted key),
/// batches against it calm or hostile, and with small limits sometimes.
fn case(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    PLANT.set(plant);
    let result = case_inner(tape);
    PLANT.set(Plant::None);
    result
}

fn case_inner(tape: &Tape) -> Result<(), Broke> {
    let global = Erased::new(Global);
    let alloc = global.handle();
    let trusted = signer(tape);
    let keys = [trusted.public()];
    let check = Ed25519Keys { namespace: NS, keys: &keys };
    let mut world = world(tape, &trusted, alloc);
    // A forged origin: the same keys under other bytes, signed by a key not given.
    let forged = tape.below(6, "forged") == 0;
    if forged {
        let mut other = [0u8; 32];
        tape.fill(&mut other, "forger");
        other[0] |= 2;
        let forger = Signer::from_seed(&other);
        world.objects = world_from(&lies_records(&world.truth), &forger, alloc).objects;
    }
    if tape.below(5, "take away") == 0 {
        take_away(tape, &mut world);
    }
    let mood = if tape.below(2, "mood") == 0 { Mood::Calm } else { Mood::Hostile(tape.below(6, "target") as u8) };
    let mut limits = Limits::DEFAULT;
    if tape.below(4, "small limits") == 0 {
        limits.record_most = 100;
        limits.range_most = 64;
        limits.gap_most = tape.below(64, "gap") as u32;
        limits.in_flight_most = 1 + tape.below(4, "in flight") as u32;
    }
    let mut reader = Reader::new_in(NS, &check, limits, alloc);
    let mut now = 0;
    for _ in 0..1 + tape.below(3, "batches") {
        let keys = batch(tape, &world);
        let ran = run(tape, &mut reader, &world, &keys, mood, now)?;
        let whole = world.gone == 0 && !forged;
        if forged && ran.answers.iter().any(|(_, a, _)| !matches!(a, Answer::Unfetched(_))) {
            // Every answer from a forged head must be "not fetched": its found bytes are lies.
            judge(&ran, &world, false, now, &limits)?;
            return Err(Broke::Oracle("a forged head was believed".into()));
        }
        judge(&ran, &world, mood == Mood::Calm && whole, now, &limits)?;
        let _ = ran.steps;
        now = ran.ended_ns + 1;
    }
    Ok(())
}

fn lies_records(truth: &BTreeMap<ContentId, std::vec::Vec<u8>>) -> BTreeMap<ContentId, std::vec::Vec<u8>> {
    truth.iter().map(|(k, v)| (*k, v.iter().map(|b| b ^ 0x55).chain([7]).collect())).collect()
}

fn world_from(records: &BTreeMap<ContentId, std::vec::Vec<u8>>, signer: &Signer, alloc: Alloc<'_>) -> World {
    let refs: std::vec::Vec<(RecordRef, &[u8])> = records.iter().map(|(k, v)| (RecordRef { tag: TAG, key: *k }, v.as_slice())).collect();
    let mut out = std::vec::Vec::new();
    seal_segment(NS, &refs, &mut out).expect("seal");
    let objects = publish(NS, 1, signer, &[out.as_slice()], 4, alloc).expect("publish");
    World { objects: objects.iter().map(|o| (o.object.name().as_str().to_owned(), o.bytes.as_slice().to_vec())).collect(), truth: records.clone(), gone: 0 }
}

/// The invariant: the remote read path verifies head signatures, wire sizes, byte ranges, and payload digests, returning only true bytes or typed misses and never hanging.
#[test]
fn the_read_path_answers_true_bytes_or_a_typed_miss_never_wrong_never_a_hang() {
    blaze_sim::check(Config::cases(300), |tape| case(tape, Plant::None));
}

#[test]
fn the_oracle_catches_a_payload_used_before_its_digest() {
    blaze_sim::control(Config::cases(300), Plant::None, Plant::PayloadBeforeDigest, "wrong bytes", case);
}

#[test]
fn the_oracle_catches_a_head_taken_unsigned() {
    blaze_sim::control(Config::cases(300), Plant::None, Plant::HeadUnsigned, "forged head was believed", case);
}

#[test]
fn the_oracle_catches_a_size_from_the_wire_asked_unchecked() {
    blaze_sim::control(Config::cases(300), Plant::None, Plant::SizeFromWire, "an ask past its limit", case);
}

#[test]
fn the_oracle_catches_a_range_off_by_one() {
    blaze_sim::control(Config::cases(300), Plant::None, Plant::RangeOffByOne, "was not found", case);
}

/// The layered store's write-through, crashed at any put: what the local store holds is only
/// ever the truth's bytes, and a restart finds the rest.
#[test]
fn a_crash_at_any_write_through_leaves_the_local_store_true() {
    blaze_sim::check(Config::cases(200), |tape| -> Result<(), Broke> {
        let global = Erased::new(Global);
        let alloc = global.handle();
        let trusted = signer(tape);
        let keys = [trusted.public()];
        let check = Ed25519Keys { namespace: NS, keys: &keys };
        let world = world(tape, &trusted, alloc);
        let wanted = batch(tape, &world);
        let crash_at = tape.below(wanted.len() as u64 + 1, "crash at") as usize;
        let mut local: BTreeMap<ContentId, std::vec::Vec<u8>> = BTreeMap::new();
        let mut puts = 0;
        // Boot one: a get per key, as the binary's store asks, until the crash.
        let mut reader = Reader::new_in(NS, &check, Limits::DEFAULT, alloc);
        let mut now = 0;
        'boot: for k in &wanted {
            if local.contains_key(k) {
                continue;
            }
            let ran = run(tape, &mut reader, &world, core::slice::from_ref(k), Mood::Hostile(tape.below(6, "target") as u8), now)?;
            now = ran.ended_ns + 1;
            if let Some(bytes) = &ran.answers[0].2 {
                if puts == crash_at {
                    break 'boot;
                }
                // One put is one whole record or none (the store publishes a file whole).
                local.insert(*k, bytes.clone());
                puts += 1;
            }
        }
        for (k, v) in &local {
            if world.truth.get(k) != Some(v) {
                return Err(Broke::Oracle("the local store holds bytes the origin's truth does not".into()));
            }
        }
        // Boot two, calm: every held key is found, locally or from the origin.
        let mut reader = Reader::new_in(NS, &check, Limits::DEFAULT, alloc);
        for k in &wanted {
            if local.contains_key(k) {
                continue;
            }
            let start = now;
            let ran = run(tape, &mut reader, &world, core::slice::from_ref(k), Mood::Calm, start)?;
            now = ran.ended_ns + 1;
            judge(&ran, &world, true, start, &Limits::DEFAULT)?;
        }
        Ok(())
    });
}

fn trusted_world(seed: u8, records: usize) -> (Signer, BTreeMap<String, std::vec::Vec<u8>>, BTreeMap<ContentId, std::vec::Vec<u8>>) {
    let signer = Signer::from_seed(&[seed; 32]);
    let mut truth = BTreeMap::new();
    for i in 0..records {
        truth.insert(ContentId::of("test key", &(i as u64).to_le_bytes()), vec![i as u8; 1 + i % 50]);
    }
    let global = Erased::new(Global);
    let w = world_from(&truth, &signer, global.handle());
    (signer, w.objects, truth)
}

fn head_bytes(objects: &BTreeMap<String, std::vec::Vec<u8>>) -> std::vec::Vec<u8> {
    objects["head"].clone()
}

#[test]
fn a_head_signed_by_a_key_not_given_is_refused() {
    let (_, objects, _) = trusted_world(1, 10);
    let other = Signer::from_seed(&[2; 32]).public();
    let check = Ed25519Keys { namespace: NS, keys: &[other] };
    assert!(matches!(Head::decode(&head_bytes(&objects), NS, &check), Err(Refused::Signature { why: SignatureWhy::UnknownSigner, .. })));
}

#[test]
fn a_head_with_any_byte_flipped_is_refused() {
    let (signer, objects, _) = trusted_world(1, 10);
    let keys = [signer.public()];
    let check = Ed25519Keys { namespace: NS, keys: &keys };
    let head = head_bytes(&objects);
    assert!(Head::decode(&head, NS, &check).is_ok());
    for at in 0..HEAD_LEN {
        let mut bad = head.clone();
        bad[at] ^= 0x10;
        assert!(Head::decode(&bad, NS, &check).is_err(), "a head with byte {at} flipped was taken");
    }
}

#[test]
fn a_head_of_another_namespace_is_refused() {
    let (signer, objects, _) = trusted_world(1, 3);
    let keys = [signer.public()];
    let check = Ed25519Keys { namespace: Namespace(77), keys: &keys };
    assert!(matches!(Head::decode(&head_bytes(&objects), Namespace(77), &check), Err(Refused::Namespace { .. })));
}

/// A head signed by a trusted key that names a manifest past the bound is refused before any
/// buffer is sized from it.
#[test]
fn a_signed_head_naming_a_huge_manifest_is_refused_before_allocating() {
    let signer = Signer::from_seed(&[3; 32]);
    let keys = [signer.public()];
    let check = Ed25519Keys { namespace: NS, keys: &keys };
    let head = Head { namespace: NS, generation: 1, manifest: ContentId([9; 32]), manifest_len: u64::MAX, signer: signer.public() };
    let bytes = head.encode(|domain, body| signer.sign(domain, body));
    assert!(matches!(Head::decode(&bytes, NS, &check), Err(Refused::TooLarge { what: "the manifest's length", .. })));
}

/// A manifest whose digest matches but says it holds four billion rows: refused by its bound,
/// never sized from.
#[test]
fn a_manifest_claiming_too_many_rows_is_refused() {
    let global = Erased::new(Global);
    let alloc = global.handle();
    let mut bytes = std::vec::Vec::new();
    bytes.extend_from_slice(b"BLRM");
    bytes.extend_from_slice(&form::FORMAT.to_le_bytes());
    bytes.extend_from_slice(&NS.0.to_le_bytes());
    bytes.extend_from_slice(&u32::MAX.to_le_bytes());
    bytes.extend_from_slice(&[0; 12]);
    let head = Head { namespace: NS, generation: 1, manifest: ContentId::of(form::MANIFEST_DOMAIN, &bytes), manifest_len: bytes.len() as u64, signer: PublicKey([0; 32]) };
    let mut rows = OwnVec::new_in(alloc);
    assert!(matches!(form::decode_manifest(&bytes, &head, &mut rows), Err(Refused::TooLarge { what: "its rows", .. })));
    // And one byte changed under the same head is refused by its digest first.
    bytes[20] = 1;
    assert!(matches!(form::decode_manifest(&bytes, &head, &mut rows), Err(Refused::Digest { .. })));
}

/// A summary whose filter length runs past its bytes is refused, not read past.
#[test]
fn a_summary_whose_lengths_disagree_is_refused() {
    let global = Erased::new(Global);
    let alloc = global.handle();
    let mut keys: std::vec::Vec<ContentId> = (0..20u64).map(|i| ContentId::of("k", &i.to_le_bytes())).collect();
    keys.sort_unstable();
    let digests = [ContentId([1; 32]), ContentId([2; 32]), ContentId([3; 32])];
    let mut bytes = OwnVec::new_in(alloc);
    form::encode_summary(ContentId([5; 32]), &keys, 8, &digests, &mut bytes);
    let mut bad = bytes.as_slice().to_vec();
    bad[52..56].copy_from_slice(&u32::MAX.to_le_bytes());
    let row = Row { segment: ContentId([5; 32]), len: 0, index_at: 0, count: 20, pages: 3, page_entries: 8, summary: ContentId::of(form::SUMMARY_DOMAIN, &bad), summary_len: bad.len() as u64 };
    assert!(matches!(Summary::decode(&bad, &row, alloc), Err(Refused::Malformed { .. })));
    let row = Row { summary: ContentId::of(form::SUMMARY_DOMAIN, &bytes), summary_len: bytes.len() as u64, ..row };
    let summary = Summary::decode(&bytes, &row, alloc).expect("the honest summary");
    for (i, k) in keys.iter().enumerate() {
        assert!(summary.may_hold(k));
        assert_eq!(summary.page_of(k), Some((i / 8) as u32));
    }
}

/// An index entry (digest-checked: a trusted builder's bug or a stolen key) that places its
/// payload past the segment's payloads is refused, never read from.
#[test]
fn an_entry_placing_its_payload_outside_the_payloads_is_refused() {
    let k = ContentId([4; 32]);
    let entry = blaze_cas::plain::Entry { key: k, tag: TAG, at: 1 << 30, len: 16, crc: 0 };
    let mut page = std::vec::Vec::new();
    entry.encode_into(&mut page);
    page.extend_from_slice(&[0; 32]);
    let row = Row { segment: ContentId([6; 32]), len: 0, index_at: 4096, count: 1, pages: 1, page_entries: 1, summary: ContentId([0; 32]), summary_len: 0 };
    assert!(matches!(form::lookup(&page, &row, 0, &k), Err(Refused::Malformed { what: "an entry places its payload outside the segment's payloads", .. })));
}

/// No name built for any digest holds anything but `[a-z0-9-]`: nothing fetched can steer a
/// path out of the origin.
#[test]
fn every_object_name_stays_inside_its_namespace() {
    blaze_sim::check(Config::cases(200), |tape| {
        let id = key(tape);
        for object in [Object::Head, Object::Manifest(id), Object::Segment(id), Object::Summary(id), Object::Index(id)] {
            let name = object.name();
            assert!(name.as_str().bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'), "{}", name.as_str());
            assert!(!name.as_str().is_empty() && name.as_str().len() <= 73);
        }
    });
}

/// Keys the origin lacks cost no index or payload request: the filters and fences answer them.
/// The counts quoted in remote-read.md come from this test's output.
#[test]
fn absent_keys_cost_no_page_request_and_held_ones_one_page_and_one_range() {
    let (signer, objects, truth) = trusted_world(5, 2000);
    let world = World { objects, truth, gone: 0 };
    let global = Erased::new(Global);
    let alloc = global.handle();
    let keys = [signer.public()];
    let check = Ed25519Keys { namespace: NS, keys: &keys };
    let mut reader = Reader::new_in(NS, &check, Limits::DEFAULT, alloc);
    let absent: std::vec::Vec<ContentId> = (0..1000u64).map(|i| ContentId::of("absent", &i.to_le_bytes())).collect();
    let mut origin = MapOrigin { objects: &world.objects };
    let mut clock = 0u64;
    let mut tick = || {
        clock += 1;
        clock
    };
    fetch(&mut reader, &mut origin, &mut tick, &absent, alloc).expect("fetch");
    let r = reader.report();
    println!("1000 absent keys: {} asks (head {}, manifest {}, summary {}, page {}, payload {}), {} bytes", r.asks, r.head_asks, r.manifest_asks, r.summary_asks, r.page_asks, r.payload_asks, r.bytes_fetched);
    assert_eq!(r.absent, 1000);
    assert!(r.page_asks <= 12, "absent keys asked {} pages: the filter lets about 1 in 256 through", r.page_asks);
    let held: std::vec::Vec<ContentId> = world.truth.keys().copied().take(1).collect();
    let before = reader.report();
    fetch(&mut reader, &mut origin, &mut tick, &held, alloc).expect("fetch");
    let r = reader.report();
    println!("1 held key after: {} more asks ({} page, {} payload)", r.asks - before.asks, r.page_asks - before.page_asks, r.payload_asks - before.payload_asks);
    assert_eq!((r.found, r.asks - before.asks), (1, 2), "a held key: one index page and one payload range");
    let summary_bytes: usize = world.objects.iter().filter(|(n, _)| n.starts_with("sum-")).map(|(_, b)| b.len()).sum();
    println!("summary of 2000 records: {summary_bytes} bytes ({:.1} bits a key)", summary_bytes as f64 * 8.0 / 2000.0);
}

struct MapOrigin<'o> {
    objects: &'o BTreeMap<String, std::vec::Vec<u8>>,
}

impl super::Origin for MapOrigin<'_> {
    fn read(&mut self, object: &Object, at: u64, into: &mut [u8], _deadline_ns: u64) -> Result<usize, OriginError> {
        let bytes = self.objects.get(object.name().as_str()).ok_or(OriginError::Missing)?;
        let from = (at as usize).min(bytes.len());
        let to = (from + into.len()).min(bytes.len());
        into[..to - from].copy_from_slice(&bytes[from..to]);
        Ok(to - from)
    }
}

/// A batch of every held key coalesces adjacent payloads: fewer ranges than keys.
#[test]
fn a_batch_of_adjacent_payloads_is_few_ranges() {
    let (signer, objects, truth) = trusted_world(6, 300);
    let global = Erased::new(Global);
    let alloc = global.handle();
    let keys = [signer.public()];
    let check = Ed25519Keys { namespace: NS, keys: &keys };
    let mut reader = Reader::new_in(NS, &check, Limits::DEFAULT, alloc);
    let all: std::vec::Vec<ContentId> = truth.keys().copied().collect();
    let mut origin = MapOrigin { objects: &objects };
    let mut clock = 0u64;
    fetch(&mut reader, &mut origin, &mut || {
        clock += 1;
        clock
    }, &all, alloc)
    .expect("fetch");
    let r = reader.report();
    println!("300 held keys in one batch: {} asks ({} payload ranges), {} bytes fetched for {} used", r.asks, r.payload_asks, r.bytes_fetched, r.bytes_used);
    assert_eq!(r.found, 300);
    assert_eq!(r.payload_asks, 1, "300 small adjacent payloads are one range");
    for (i, k) in all.iter().enumerate() {
        assert_eq!(reader.found(i), Some(truth[k].as_slice()));
    }
}
