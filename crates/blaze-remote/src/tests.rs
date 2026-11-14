//! The simulation: push and pull machines against the in-memory bucket ([`crate::fake`]),
//! with crashes at every step, two pushers at once, slow and failing requests, and forgeries.
//!
//! The invariant: no byte is used unverified; after any crash the bucket is readable at its
//! last root and a later push completes it; what is pulled is what was pushed, byte for byte;
//! nothing of an organisation's namespace is under public; no client checks one object twice.
//! Each planted bug ([`Plant`]) is caught by the oracle that states it.

use std::collections::{BTreeMap, BTreeSet};

use blaze_cas::plain::Namespace;
use blaze_ids::ContentId;
use blaze_keyed::kind::{Origin, Tag};
use blaze_rng::SplitMix64;
use hegel::generators as gs;

use crate::{
    PLANT, Plant,
    bucket::{Bucket, Failure, Machine, Reply, Step, Ticket, drive},
    bundle::{Bundle, BundleId, SegmentRef},
    exec::{self, Batch, Outcome, RecordRef, Sent, Status, Usage},
    expire,
    fake::{self, Fake, Faults, Shared},
    find::Cache,
    name::{self, Toolchain},
    privacy,
    pull::{Fetch, Proven, Remote},
    push::{self, BundleOut, Plan, Push},
    segment,
    settings::Settings,
    shard::{self, Entry, Root, Run, ShardRef},
    trust::{self, KeyEntry, KeyList, PublicKey, Roles, Signer, Statement, Tier, Trusted, Why},
};

const TOOLCHAIN: &str = "nightly-2026-11-13";

fn org() -> Namespace {
    Namespace::named("acme")
}

/// The keys of one world: an anchor, a builder listed for build and root in both namespaces.
struct World {
    anchor: Signer,
    builder: Signer,
    toolchain: Toolchain,
}

impl World {
    fn new(seed: u8) -> World {
        World { anchor: Signer::from_seed(&[seed; 32]), builder: Signer::from_seed(&[seed.wrapping_add(1); 32]), toolchain: Toolchain::new(TOOLCHAIN).expect("a toolchain name") }
    }

    fn key_list(&self, namespace: Namespace) -> Vec<u8> {
        KeyList { namespace, serial: 1, issued: 0, keys: vec![KeyEntry { key: self.builder.public(), roles: Roles(Roles::BUILD.0 | Roles::ROOT.0), revoked: false, from: 0, until: u64::MAX }] }.encode(&self.anchor)
    }

    fn trusted(&self) -> Trusted {
        let mut trusted = Trusted::new(self.toolchain.clone());
        trusted.anchor(Namespace::PUBLIC, vec![self.anchor.public()]);
        trusted.anchor(org(), vec![self.anchor.public()]);
        trusted
    }
}

type Records = BTreeMap<[u8; 32], (u8, Vec<u8>)>;

fn records(rng: &mut SplitMix64, n: usize) -> Records {
    let mut out = Records::new();
    while out.len() < n {
        let mut key = [0u8; 32];
        rng.fill(&mut key);
        let mut payload = vec![0u8; 1 + rng.index(200)];
        rng.fill(&mut payload);
        out.insert(key, (1 + rng.below(3) as u8, payload));
    }
    out
}

fn segments_of(namespace: Namespace, recs: &Records, per: usize) -> Vec<Vec<u8>> {
    let all: Vec<([u8; 32], u8, Vec<u8>)> = recs.iter().map(|(k, (kind, v))| (*k, *kind, v.clone())).collect();
    all.chunks(per).map(|chunk| segment::make(namespace, chunk)).collect()
}

fn plan<'a>(w: &'a World, namespace: Namespace, segs: &'a [Vec<u8>], bundles: Vec<BundleOut>, cas: bool) -> Plan<'a> {
    Plan {
        namespace,
        root_name: "main".to_owned(),
        tier: Tier::Top,
        signer: &w.builder,
        key_list: w.key_list(namespace),
        trusted: w.trusted(),
        segments: segs.iter().map(Vec::as_slice).collect(),
        bundles,
        rebuilt: Vec::new(),
        sample_per_1024: 1024,
        cas,
        now_s: 1_000,
    }
}

/// One push by a fresh client; its checks are held to once each.
fn push_with(w: &World, store: &Shared, namespace: Namespace, segs: &[Vec<u8>], seed: u64, faults: Faults, cas: bool) -> Result<push::Report, Failure> {
    trust::reset_checks();
    let mut machine = Push::new(plan(w, namespace, segs, Vec::new(), cas))?;
    let mut bucket = Fake::new(store, seed, faults);
    let out = drive(&mut machine, &mut bucket, &mut |()| Ok(()));
    assert_eq!(trust::checks().twice, 0, "a pusher checked one object twice: {:?}", trust::checks());
    out
}

fn push_clean(w: &World, store: &Shared, namespace: Namespace, segs: &[Vec<u8>]) -> push::Report {
    push_with(w, store, namespace, segs, 1, Faults::default(), true).unwrap_or_else(|it| panic!("a clean push failed: {it}"))
}

/// What a fresh client pulls of `keys` in `namespace`: every record of every segment handed to
/// it, read through the proof. A client that checks an object twice is an error.
fn pull_all(w: &World, store: &Shared, namespace: Namespace, keys: &[[u8; 32]]) -> Result<Records, String> {
    trust::reset_checks();
    let mut bucket = Fake::new(store, 7, Faults::default());
    let mut remote = Remote::new(&mut bucket, "main", w.trusted());
    let mut got = Records::new();
    let pulled = remote.pull(namespace, keys.to_vec(), &mut |proven: Proven| {
        for rec in proven.records() {
            got.insert(rec.key, (rec.kind, proven.payload(rec).to_vec()));
        }
        Ok(())
    });
    let checks = trust::checks();
    if checks.twice != 0 {
        return Err(format!("a client checked one object twice: {checks:?}"));
    }
    pulled.map_err(|it| format!("unreadable: {it}"))?;
    Ok(got)
}

/// The oracle, after any run: nothing private under public; the bucket readable; every record
/// of `committed` pulled as pushed; every record pulled one that was pushed (`committed` or
/// `maybe`, a push that may or may not have landed) to this namespace.
fn oracle(w: &World, store: &Shared, namespace: Namespace, committed: &Records, maybe: &Records) -> Result<(), String> {
    for (name, object) in &store.borrow().objects {
        if name.starts_with("public/") && name.contains("/segments/") && !name.ends_with(".statement") && object.bytes.get(8..16).is_none_or(|it| it != [0; 8]) {
            return Err(format!("{name}: a segment of another namespace under public"));
        }
    }
    let keys: Vec<[u8; 32]> = committed.keys().chain(maybe.keys()).copied().collect();
    let got = pull_all(w, store, namespace, &keys)?;
    for (key, value) in committed {
        if got.get(key) != Some(value) {
            return Err(format!("key {}: pushed and not pulled as pushed", blaze_format::hex::encode(key)));
        }
    }
    for (key, value) in &got {
        if committed.get(key) != Some(value) && maybe.get(key) != Some(value) {
            return Err(format!("key {}: pulled and never pushed to this namespace (bytes used unverified, or private)", blaze_format::hex::encode(key)));
        }
    }
    Ok(())
}

/// Runs `f` with `plant` planted, then plants nothing again.
fn planted<T>(plant: Plant, f: impl FnOnce() -> T) -> T {
    PLANT.set(plant);
    let out = f();
    PLANT.set(Plant::None);
    out
}

fn union(a: &Records, b: &Records) -> Records {
    a.iter().chain(b.iter()).map(|(k, v)| (*k, v.clone())).collect()
}

// ---- crashes, faults, concurrency ----

/// A push crashed after each number of requests that took effect, then pushed again.
fn crash_at_every_step(seed: u64) -> Result<(), String> {
    let w = World::new(1);
    let mut rng = SplitMix64::new(seed);
    let base_recs = records(&mut rng, 40);
    let next_recs = records(&mut rng, 60);
    let base = fake::shared();
    push_clean(&w, &base, Namespace::PUBLIC, &segments_of(Namespace::PUBLIC, &base_recs, 16));
    let next = segments_of(Namespace::PUBLIC, &next_recs, 16);
    let clean = fake::fork(&base);
    let before = clean.borrow().requests;
    push_clean(&w, &clean, Namespace::PUBLIC, &next);
    let steps = clean.borrow().requests - before;
    for k in 0..=steps {
        let store = fake::fork(&base);
        // Answers out of order, so a crash can fall between any two of one batch of puts.
        let crashed = push_with(&w, &store, Namespace::PUBLIC, &next, seed ^ k, Faults { crash_after: Some(k), latency_ms: 30, ..Faults::default() }, true);
        let committed = if crashed.is_ok() { union(&base_recs, &next_recs) } else { base_recs.clone() };
        oracle(&w, &store, Namespace::PUBLIC, &committed, &next_recs).map_err(|it| format!("crash after {k} of {steps}: {it}"))?;
        push_with(&w, &store, Namespace::PUBLIC, &next, seed, Faults::default(), true).map_err(|it| format!("crash after {k}: the push after it failed: {it}"))?;
        oracle(&w, &store, Namespace::PUBLIC, &union(&base_recs, &next_recs), &Records::new()).map_err(|it| format!("crash after {k}, pushed again: {it}"))?;
    }
    Ok(())
}

/// The invariant: a crash at any request leaves the bucket readable at its last root, and the
/// next push completes it.
#[test]
fn a_crash_at_any_step_leaves_the_bucket_readable() {
    for seed in 0..4 {
        crash_at_every_step(seed).unwrap_or_else(|it| panic!("seed {seed}: {it}"));
    }
}

#[test]
fn the_oracle_catches_a_root_put_before_its_shards() {
    let caught = planted(Plant::RootBeforeShards, || (0..4).any(|seed| crash_at_every_step(seed).is_err()));
    assert!(caught, "the planted root-before-shards bug went unseen");
}

/// The invariant: under slow, failing, ambiguous and timed-out requests, a push ends (never
/// retrying), the bucket stays readable, and a later clean push completes it.
#[test]
fn slow_and_failing_requests_never_break_the_bucket() {
    let w = World::new(1);
    for seed in 0..48u64 {
        let mut rng = SplitMix64::new(seed);
        let base_recs = records(&mut rng, 30);
        let next_recs = records(&mut rng, 50);
        let store = fake::shared();
        push_clean(&w, &store, Namespace::PUBLIC, &segments_of(Namespace::PUBLIC, &base_recs, 8));
        let next = segments_of(Namespace::PUBLIC, &next_recs, 8);
        let faults = Faults { latency_ms: 20_000, fail_one_in: 9, ambiguous_one_in: 9, timeout_one_in: 13, crash_after: None };
        let out = push_with(&w, &store, Namespace::PUBLIC, &next, seed, faults, seed % 2 == 0);
        let committed = if out.is_ok() { union(&base_recs, &next_recs) } else { base_recs.clone() };
        oracle(&w, &store, Namespace::PUBLIC, &committed, &next_recs).unwrap_or_else(|it| panic!("seed {seed}, after a faulty push ({out:?}): {it}"));
        push_with(&w, &store, Namespace::PUBLIC, &next, seed, Faults::default(), seed % 2 == 0).unwrap_or_else(|it| panic!("seed {seed}: the clean push after: {it}"));
        oracle(&w, &store, Namespace::PUBLIC, &union(&base_recs, &next_recs), &Records::new()).unwrap_or_else(|it| panic!("seed {seed}, pushed again: {it}"));
    }
}

/// A push machine stepped one round trip at a time, as `drive` steps it.
struct Runner<'a> {
    machine: Push<'a>,
    bucket: Fake,
    replies: Vec<(Ticket, Reply)>,
    done: Option<Result<push::Report, Failure>>,
}

fn runner<'a>(w: &'a World, store: &Shared, segs: &'a [Vec<u8>], seed: u64, cas: bool) -> Runner<'a> {
    Runner { machine: Push::new(plan(w, Namespace::PUBLIC, segs, Vec::new(), cas)).expect("a plan"), bucket: Fake::new(store, seed, Faults { latency_ms: 40, ..Faults::default() }), replies: Vec::new(), done: None }
}

impl Runner<'_> {
    fn tick(&mut self) {
        if self.done.is_some() {
            return;
        }
        let now = self.bucket.now_ms();
        match self.machine.step(now, std::mem::take(&mut self.replies)) {
            Step::Done(out) => {
                while self.bucket.in_flight() > 0 {
                    drop(self.bucket.wait());
                }
                self.done = Some(out);
            }
            Step::Ask(asked) => {
                self.bucket.send(asked);
                if self.bucket.in_flight() == 0 {
                    self.done = Some(Err(Failure::Misused("waits with nothing asked".to_owned())));
                } else {
                    self.replies = self.bucket.wait();
                }
            }
        }
    }
}

/// Two pushers at once on one bucket; the records each pushed that the bucket must hold.
fn two_pushers(seed: u64, cas: bool) -> Result<(), String> {
    let w = World::new(1);
    let mut rng = SplitMix64::new(seed);
    let base_recs = records(&mut rng, 30);
    let (a_recs, b_recs) = (records(&mut rng, 40), records(&mut rng, 40));
    let store = fake::shared();
    push_clean(&w, &store, Namespace::PUBLIC, &segments_of(Namespace::PUBLIC, &base_recs, 8));
    let (a_segs, b_segs) = (segments_of(Namespace::PUBLIC, &a_recs, 8), segments_of(Namespace::PUBLIC, &b_recs, 8));
    let (mut a, mut b) = (runner(&w, &store, &a_segs, seed, cas), runner(&w, &store, &b_segs, seed + 1, cas));
    let mut turns = SplitMix64::new(seed ^ 0x5eed);
    for _ in 0..1_000_000 {
        if a.done.is_some() && b.done.is_some() {
            break;
        }
        if turns.coin() { a.tick() } else { b.tick() }
    }
    let (Some(a_out), Some(b_out)) = (a.done.take(), b.done.take()) else { return Err("two pushers did not end".to_owned()) };
    let mut committed = base_recs.clone();
    let mut maybe = Records::new();
    for (out, recs) in [(&a_out, &a_recs), (&b_out, &b_recs)] {
        match out {
            // Without compare-and-swap a root read back as ours can still be overwritten by a
            // slower pusher after: only readability and nothing-unpushed are promised then.
            Ok(_) if cas => committed.extend(recs.iter().map(|(k, v)| (*k, v.clone()))),
            Ok(_) => maybe.extend(recs.iter().map(|(k, v)| (*k, v.clone()))),
            Err(Failure::LostRace { .. } | Failure::Contended { .. }) => maybe.extend(recs.iter().map(|(k, v)| (*k, v.clone()))),
            Err(other) => return Err(format!("a pusher failed: {other}")),
        }
    }
    oracle(&w, &store, Namespace::PUBLIC, &committed, &maybe)
}

/// The invariant: with compare-and-swap, two pushers at once both land; without it, a pusher
/// whose root lost says so (never success with its run unindexed).
#[test]
fn two_pushers_at_once_both_land_or_say_they_lost() {
    for seed in 0..32 {
        two_pushers(seed, true).unwrap_or_else(|it| panic!("seed {seed}, with compare-and-swap: {it}"));
        two_pushers(seed, false).unwrap_or_else(|it| panic!("seed {seed}, without: {it}"));
    }
}

#[test]
fn the_oracle_catches_a_root_put_without_its_compare_and_swap() {
    let caught = planted(Plant::IgnoreCas, || (0..32).any(|seed| two_pushers(seed, true).is_err()));
    assert!(caught, "the planted unconditional root put went unseen");
}

// ---- forgeries ----

fn pushed_world(seed: u64) -> (World, Shared, Records) {
    let w = World::new(1);
    let recs = records(&mut SplitMix64::new(seed), 50);
    let store = fake::shared();
    push_clean(&w, &store, Namespace::PUBLIC, &segments_of(Namespace::PUBLIC, &recs, 12));
    (w, store, recs)
}

/// No unverified byte is used: a pull is refused, or answers exactly what was pushed.
fn uses_only_verified(w: &World, store: &Shared, namespace: Namespace, pushed: &Records, asked: &[[u8; 32]]) -> Result<Option<String>, String> {
    match pull_all(w, store, namespace, asked) {
        Err(why) => Ok(Some(why)),
        Ok(got) => {
            for (key, value) in &got {
                if pushed.get(key) != Some(value) {
                    return Err(format!("key {}: a forged record was used", blaze_format::hex::encode(key)));
                }
            }
            Ok(None)
        }
    }
}

/// The invariant: a byte flipped anywhere in any object a pull reads is refused.
#[test]
fn a_flipped_byte_in_any_object_is_refused() {
    let (w, store, recs) = pushed_world(3);
    let keys: Vec<[u8; 32]> = recs.keys().copied().collect();
    let names: Vec<String> = store.borrow().objects.keys().cloned().collect();
    let mut rng = SplitMix64::new(11);
    for name in names {
        let forged = fake::fork(&store);
        {
            let mut it = forged.borrow_mut();
            let bytes = &mut it.objects.get_mut(&name).expect("listed").bytes;
            let at = rng.index(bytes.len());
            bytes[at] ^= 1 << rng.below(8);
        }
        match uses_only_verified(&w, &forged, Namespace::PUBLIC, &recs, &keys) {
            Ok(Some(_)) => {}
            Ok(None) => panic!("{name}: a flipped byte was not refused"),
            Err(why) => panic!("{name}: {why}"),
        }
    }
}

#[test]
fn the_oracle_catches_a_segment_taken_without_its_digest() {
    let (w, store, recs) = pushed_world(4);
    let keys: Vec<[u8; 32]> = recs.keys().copied().collect();
    let name = store.borrow().objects.keys().find(|it| it.contains("/segments/") && !it.ends_with(".statement")).cloned().expect("a segment");
    {
        let mut it = store.borrow_mut();
        let bytes = &mut it.objects.get_mut(&name).expect("found").bytes;
        // The first record's payload, just past the header.
        bytes[segment::HEADER] ^= 0x80;
    }
    assert!(uses_only_verified(&w, &store, Namespace::PUBLIC, &recs, &keys).expect("refused without the plant").is_some());
    assert!(planted(Plant::SkipSegmentDigest, || uses_only_verified(&w, &store, Namespace::PUBLIC, &recs, &keys)).is_err(), "the planted unchecked digest went unseen");
}

/// Another world's push, its objects copied into `store` (its key list left out), each
/// signed object then claiming `claim` as its signer when given.
fn forge_from(other: &World, store: &Shared, recs: &Records, claim: Option<&PublicKey>) {
    let theirs = fake::shared();
    push_clean(other, &theirs, Namespace::PUBLIC, &segments_of(Namespace::PUBLIC, recs, 12));
    let mut ours = store.borrow_mut();
    for (name, mut object) in theirs.borrow().objects.clone() {
        if name.contains("/keys/") {
            continue;
        }
        if let Some(claim) = claim {
            let len = object.bytes.len();
            if name.ends_with(".statement") {
                object.bytes[104..136].copy_from_slice(&claim.0);
            } else if name.contains("/roots/") {
                object.bytes[len - 96..len - 64].copy_from_slice(&claim.0);
            }
        }
        ours.objects.insert(name, object);
    }
}

fn changed(recs: &Records) -> Records {
    recs.iter().map(|(k, (kind, v))| (*k, (*kind, v.iter().map(|b| b ^ 0x55).collect()))).collect()
}

/// The invariant: objects signed by a key no anchor listed are refused.
#[test]
fn an_unknown_signer_is_refused() {
    let (w, store, recs) = pushed_world(5);
    forge_from(&World::new(9), &store, &changed(&recs), None);
    let keys: Vec<[u8; 32]> = recs.keys().copied().collect();
    let why = uses_only_verified(&w, &store, Namespace::PUBLIC, &recs, &keys).expect("not used").expect("refused");
    assert!(why.contains("not trusted") || why.contains("signer"), "refused for another reason: {why}");
}

/// The invariant: objects naming a trusted signer but not signed by it are refused; the
/// planted unchecked signature is caught.
#[test]
fn a_signature_not_by_its_named_signer_is_refused() {
    let (w, store, recs) = pushed_world(6);
    forge_from(&World::new(9), &store, &changed(&recs), Some(&w.builder.public()));
    let keys: Vec<[u8; 32]> = recs.keys().copied().collect();
    let why = uses_only_verified(&w, &store, Namespace::PUBLIC, &recs, &keys).expect("not used").expect("refused");
    assert!(why.contains("signature"), "refused for another reason: {why}");
    assert!(planted(Plant::SkipSignature, || uses_only_verified(&w, &store, Namespace::PUBLIC, &recs, &keys)).is_err(), "the planted unchecked signature went unseen");
}

/// The invariant: an organisation's objects moved under public are refused, though one builder
/// signs for both; the planted unchecked namespace is caught.
#[test]
fn an_organisations_objects_moved_to_public_are_refused() {
    let (w, store, public_recs) = pushed_world(7);
    let private = records(&mut SplitMix64::new(77), 30);
    let theirs = fake::shared();
    push_clean(&w, &theirs, org(), &segments_of(org(), &private, 10));
    let prefix = name::space(org()).expect("an organisation's namespace");
    for (name, object) in theirs.borrow().objects.clone() {
        if name.contains("/keys/") {
            continue;
        }
        let moved = format!("public{}", name.strip_prefix(&prefix).expect("under the organisation"));
        store.borrow_mut().objects.insert(moved, object);
    }
    let keys: Vec<[u8; 32]> = private.keys().copied().collect();
    let why = uses_only_verified(&w, &store, Namespace::PUBLIC, &public_recs, &keys).expect("not used").expect("refused");
    assert!(why.contains("namespace"), "refused for another reason: {why}");
    assert!(planted(Plant::SkipNamespace, || uses_only_verified(&w, &store, Namespace::PUBLIC, &public_recs, &keys)).is_err(), "the planted unchecked namespace went unseen");
}

/// The invariant: a segment of an organisation's namespace is never pushed to public; the
/// planted privacy hole is caught by the oracle's scan of the bucket.
#[test]
fn a_private_segment_is_never_pushed_to_public() {
    let w = World::new(1);
    let private = records(&mut SplitMix64::new(8), 20);
    let segs = segments_of(org(), &private, 10);
    let store = fake::shared();
    let refused = Push::new(plan(&w, Namespace::PUBLIC, &segs, Vec::new(), true)).err().expect("refused");
    assert!(matches!(refused, Failure::Refused(ref it) if matches!(it.why, Why::Private { .. })), "{refused}");
    assert_eq!(store.borrow().requests, 0);
    planted(Plant::SkipPrivacy, || push_with(&w, &store, Namespace::PUBLIC, &segs, 1, Faults::default(), true)).expect("pushed with the plant");
    assert!(oracle(&w, &store, Namespace::PUBLIC, &Records::new(), &Records::new()).is_err(), "the planted privacy hole went unseen");
}

#[test]
fn the_fast_tier_never_goes_to_public() {
    let w = World::new(1);
    let segs = segments_of(Namespace::PUBLIC, &records(&mut SplitMix64::new(9), 4), 4);
    let mut it = plan(&w, Namespace::PUBLIC, &segs, Vec::new(), true);
    it.tier = Tier::Fast;
    let refused = Push::new(it).err().expect("refused");
    assert!(matches!(refused, Failure::Refused(ref it) if it.why == Why::Tier), "{refused}");
}

/// The invariant: a second builder's different value under a key already in the bucket is
/// refused before its root moves, naming the key.
#[test]
fn a_different_value_under_a_pushed_key_is_refused() {
    let (w, store, recs) = pushed_world(10);
    let first: Records = recs.iter().take(5).map(|(k, v)| (*k, v.clone())).collect();
    let segs = segments_of(Namespace::PUBLIC, &changed(&first), 5);
    let out = push_with(&w, &store, Namespace::PUBLIC, &segs, 1, Faults::default(), true);
    assert!(matches!(out, Err(Failure::Refused(ref it)) if matches!(it.why, Why::NotDeterministic { .. }) && it.key.is_some()), "{out:?}");
    oracle(&w, &store, Namespace::PUBLIC, &recs, &Records::new()).expect("the bucket as it was");
}

#[test]
fn an_index_entry_outside_its_segment_is_refused() {
    let entry = Entry { key: [3; 32], value: ContentId([1; 32]), segment: ContentId([2; 32]), segment_len: 128 + 16 + 48 + 48, index_at: 144, at: 128, len: 32, count: 1 };
    let (digest, bytes) = shard::encode(Namespace::PUBLIC, 1, &[entry]);
    let shard_ref = ShardRef { first: entry.key, digest, count: 1 };
    let refused = shard::decode_whole("a shard", &bytes, &shard_ref, Namespace::PUBLIC, &Cache::default()).expect_err("outside");
    assert_eq!(refused.why, Why::OutsideSegment);
    let inside = Entry { len: 16, ..entry };
    let (digest, bytes) = shard::encode(Namespace::PUBLIC, 1, &[inside]);
    assert_eq!(shard::decode_whole("a shard", &bytes, &ShardRef { digest, ..shard_ref }, Namespace::PUBLIC, &Cache::default()).expect("inside"), vec![inside]);
}

#[test]
fn an_oversized_object_is_refused_before_it_is_read() {
    let (w, store, recs) = pushed_world(12);
    let root = name::root(Namespace::PUBLIC, &w.toolchain, "main").expect("a root name");
    store.borrow_mut().objects.get_mut(&root).expect("pushed").bytes = vec![0; shard::ROOT_MOST as usize + 1];
    let keys: Vec<[u8; 32]> = recs.keys().copied().collect();
    assert!(uses_only_verified(&w, &store, Namespace::PUBLIC, &recs, &keys).expect("not used").is_some());
    let mut fetch = Fetch::new(Namespace::PUBLIC, w.toolchain.clone(), vec![SegmentRef { digest: ContentId([1; 32]), len: blaze_cas::plain::SEGMENT_MOST + 1, index_at: 128, count: 0 }]);
    let mut bucket = Fake::new(&store, 1, Faults::default());
    let out = drive(&mut fetch, &mut bucket, &mut |_| Ok(()));
    assert!(matches!(out, Err(Failure::Refused(ref it)) if matches!(it.why, Why::TooLarge { .. })), "{out:?}");
    assert_eq!(bucket.counts().requests, 0);
}

#[test]
fn a_name_that_could_escape_its_directory_is_refused() {
    let long = "x".repeat(65);
    for bad in ["..", ".", "../x", "a/b", "a\\b", "", ".hidden", "A", "a b", "a%2f", long.as_str()] {
        assert!(name::part("a name", bad).is_err(), "{bad:?} taken");
        assert!(Toolchain::new(bad).is_err(), "toolchain {bad:?} taken");
        assert!(BundleId::new(TOOLCHAIN, bad, "std").is_err(), "target {bad:?} taken");
    }
    assert!(name::namespace_named("local").is_err());
    assert!(name::space(Namespace::LOCAL).is_err());
    assert!(name::is_under("public/t/x", Namespace::PUBLIC) && !name::is_under("publicx/t", Namespace::PUBLIC));
}

#[test]
fn a_stale_key_list_is_refused() {
    let w = World::new(1);
    let mut trusted = w.trusted();
    trusted.saw(Namespace::PUBLIC, 2);
    let refused = trusted.take_list("keys", Namespace::PUBLIC, &w.key_list(Namespace::PUBLIC)).expect_err("stale");
    assert!(matches!(refused.why, Why::StaleKeyList { serial: 1, seen: 2 }));
}

// ---- bundles and the store's seam ----

/// The invariant: a bundle pulled ahead is every segment it names, each checked once, equal
/// to what was pushed; the store's seam serves those bytes and statements unchanged.
#[test]
fn a_bundle_is_pulled_whole_and_checked_once() {
    let w = World::new(1);
    let recs = records(&mut SplitMix64::new(13), 60);
    let segs = segments_of(Namespace::PUBLIC, &recs, 15);
    let id = BundleId::new(TOOLCHAIN, "aarch64-apple-darwin", "std").expect("a bundle id");
    let digests = segs.iter().map(|it| segment::digest_of(it).expect("a segment")).collect();
    let store = fake::shared();
    trust::reset_checks();
    let mut machine = Push::new(plan(&w, Namespace::PUBLIC, &segs, vec![BundleOut { id: id.clone(), segments: digests }], true)).expect("a plan");
    drive(&mut machine, &mut Fake::new(&store, 1, Faults::default()), &mut |()| Ok(())).expect("pushed");

    trust::reset_checks();
    let mut bucket = Fake::new(&store, 2, Faults::default());
    let mut remote = Remote::new(&mut bucket, "main", w.trusted());
    let mut got = Records::new();
    let bundle = remote
        .bundle(Namespace::PUBLIC, &id, &mut |proven| {
            for rec in proven.records() {
                got.insert(rec.key, (rec.kind, proven.payload(rec).to_vec()));
            }
            Ok(())
        })
        .expect("pulled");
    assert_eq!(bundle.segments.len(), segs.len());
    assert_eq!(got, recs);
    assert_eq!(trust::checks().twice, 0, "{:?}", trust::checks());
    let requests = remote.report.counts.requests;
    // A manifest, the key list, then a segment and a statement each.
    assert_eq!(requests, 2 + 2 * segs.len() as u64, "{requests} requests");

    trust::reset_checks();
    let mut bucket = Fake::new(&store, 3, Faults::default());
    let mut remote = Remote::new(&mut bucket, "main", w.trusted());
    let (_, wanted) = remote.bundle_ahead(Namespace::PUBLIC, &id).expect("ahead");
    let verifier = remote.trusted().clone();
    for (it, bytes) in wanted.iter().zip(&segs) {
        use blaze_cas::plain::{Source, Verifier};
        let statement = remote.statement(&it.digest).expect("held");
        verifier.verify(Namespace::PUBLIC, &it.digest, it.len, &statement).expect("trusted");
        assert_eq!(&remote.fetch(&it.digest, 0, it.len).expect("held"), bytes);
    }
}

/// The two readers of the plain format agree: the store's own check takes what this crate
/// writes.
#[test]
fn the_store_takes_the_segments_this_crate_writes() {
    let dir = std::env::temp_dir().join(format!("blaze-remote-test-{}", std::process::id()));
    let recs = records(&mut SplitMix64::new(14), 20);
    for bytes in segments_of(Namespace::PUBLIC, &recs, 7) {
        let read = segment::read(&bytes).expect("ours");
        let digest = blaze_cas::plain::LocalDir::new(&dir).put(&bytes, &[0; 200]).expect("the store's check");
        assert_eq!(digest, read.digest);
    }
    std::fs::remove_dir_all(&dir).expect("the test's directory");
}

// ---- local only ----

fn no_env(_: &str) -> Option<std::ffi::OsString> {
    None
}

/// The invariant: with no cache configured, or the cache disabled, no command makes a bucket
/// and nothing is asked of one.
#[test]
fn no_cache_or_a_disabled_one_contacts_nothing() {
    let store = fake::shared();
    let mut made = 0;
    let dir = std::env::temp_dir().join(format!("blaze-remote-off-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a directory");
    for disabled in [false, true] {
        if disabled {
            std::fs::write(dir.join(crate::settings::FILE), br#"{"enabled": false, "bucket": {"endpoint": "https://bucket.invalid", "name": "b", "public_url": "https://public.invalid"}}"#).expect("written");
        }
        let settings = Settings::read(&dir, &no_env).expect("read");
        assert!(settings.contacts().is_none());
        assert!(crate::cmd::status(&settings).contains("nothing leaves this machine"));
        for words in [&["pull", TOOLCHAIN, "aarch64-apple-darwin", "std"][..], &["push", r#"{"toolchain": "nightly-2026-11-13"}"#]] {
            let out = crate::cmd::run(&dir, &no_env, words, 0, &mut |_| {
                made += 1;
                Ok(Box::new(Fake::new(&store, 1, Faults::default())) as Box<dyn Bucket>)
            });
            assert!(out.is_err_and(|it| it.contains("nothing was contacted")), "{words:?}");
        }
    }
    assert_eq!(made, 0);
    assert_eq!(store.borrow().requests, 0);
    std::fs::remove_dir_all(&dir).expect("the test's directory");
}

#[test]
fn settings_never_show_a_secret() {
    let mut rng = SplitMix64::new(15);
    let mut seed = [0u8; 32];
    rng.fill(&mut seed);
    let (seed_hex, secret_hex) = (blaze_format::hex::encode(&seed), blaze_format::hex::encode(&seed[..16]));
    let env = |name: &str| -> Option<std::ffi::OsString> {
        match name {
            "BLAZE_CACHE_ENDPOINT" => Some("https://bucket.invalid".into()),
            "BLAZE_CACHE_BUCKET" => Some("b".into()),
            "BLAZE_CACHE_ACCESS_KEY_ID" => Some("id".into()),
            "BLAZE_CACHE_SECRET_ACCESS_KEY" => Some(secret_hex.clone().into()),
            "BLAZE_CACHE_SIGNING_SEED" => Some(seed_hex.clone().into()),
            _ => None,
        }
    };
    let settings = Settings::read(std::path::Path::new("/nonexistent/blaze-remote"), &env).expect("read");
    assert_eq!(settings.leaves(), crate::settings::Leaves::Pushes);
    for shown in [format!("{settings:?}"), crate::cmd::status(&settings)] {
        assert!(!shown.contains(&secret_hex) && !shown.contains(&seed_hex), "a secret shown: {shown}");
    }
    let signed = crate::http::request(
        &crate::http::Access { endpoint: Some("https://bucket.invalid".into()), bucket: Some("b".into()), public_url: Some("https://public.invalid".into()), credentials: settings.credentials.clone() },
        crate::bucket::Op::Put { name: "public/x".into(), bytes: vec![1], cond: crate::bucket::Cond::Absent },
        0,
    )
    .expect("signed");
    assert!(signed.0.headers.iter().any(|(k, _)| k == "authorization") && signed.0.headers.iter().all(|(_, v)| !v.contains(&secret_hex)));
    let anonymous = crate::http::request(
        &crate::http::Access { endpoint: None, bucket: None, public_url: Some("https://public.invalid".into()), credentials: None },
        crate::bucket::Op::Get { name: "public/t/x/roots/main".into(), range: None, most: 10 },
        0,
    )
    .expect("anonymous");
    assert!(anonymous.0.headers.is_empty() && anonymous.0.url == "https://public.invalid/public/t/x/roots/main");
    for url in ["http://a", "https://a/b", "https://u:p@a"] {
        let bad = |name: &str| (name == "BLAZE_CACHE_ENDPOINT").then(|| std::ffi::OsString::from(url));
        assert!(Settings::read(std::path::Path::new("/nonexistent/blaze-remote"), &bad).is_err(), "{url} taken");
    }
}

#[test]
fn sigv4_dates_are_civil() {
    assert_eq!(crate::s3::amz_date(0), "19700101T000000Z");
    assert_eq!(crate::s3::amz_date(951_782_400), "20000229T000000Z");
    assert_eq!(crate::s3::amz_date(1_794_571_199), "20261113T115959Z");
    assert_eq!(crate::s3::encode("a b/c~", true), "a%20b/c~");
    assert_eq!(crate::s3::upload_id(b"<InitiateMultipartUploadResult><UploadId>abc-1</UploadId></InitiateMultipartUploadResult>").as_deref(), Some("abc-1"));
}

// ---- privacy and expiry ----

#[test]
fn only_wholly_public_inputs_make_a_public_record() {
    let registry = Origin::Registry { checksum: ContentId([4; 32]) };
    let public = privacy::Inputs { origin: registry, reads: &[Namespace::PUBLIC], outside: false };
    assert_eq!(privacy::namespace_for(&public, Some(org())), Namespace::PUBLIC);
    for inputs in [privacy::Inputs { outside: true, ..public }, privacy::Inputs { reads: &[org()], ..public }, privacy::Inputs { origin: Origin::Private, ..public }] {
        assert_eq!(privacy::namespace_for(&inputs, Some(org())), org());
        assert_eq!(privacy::namespace_for(&inputs, None), Namespace::LOCAL);
    }
}

#[test]
fn expiry_never_takes_the_current_toolchain() {
    let tc = |n: u32| Toolchain::new(&format!("nightly-2026-10-{n:02}")).expect("a name");
    let seen: Vec<expire::Seen> = (1..=20).map(|n| expire::Seen { toolchain: tc(n), last_push: u64::from(n) * 86_400 }).collect();
    let now = 60 * 86_400;
    let gone = expire::toolchains(Namespace::PUBLIC, &tc(1), &seen, now, expire::POLICY).expect("named");
    assert!(!gone.iter().any(|it| it.contains("nightly-2026-10-01")), "the current toolchain expired");
    assert_eq!(gone.len(), 20 - 1 - expire::POLICY.keep);
    let root = Root { namespace: Namespace::PUBLIC, generation: 100, keys_serial: 1, issued: 0, previous: ContentId([0; 32]), runs: vec![Run { generation: 10, shards: vec![ShardRef { first: [0; 32], digest: ContentId([1; 32]), count: 1 }] }] };
    assert_eq!(expire::generations(&root, &[5, 10, 36, 50, 99], expire::POLICY), vec![5]);
}

// ---- remote execution ----

fn rref(tag: u16, n: u8) -> RecordRef {
    RecordRef { tag: Tag(tag), key: ContentId([n; 32]) }
}

/// The invariant: an action run remotely, from its batch as sent and only its inputs, makes the
/// same bytes as the same action run locally; a read of anything else is refused.
#[test]
fn a_remote_action_is_byte_identical_to_a_local_one() {
    let mut records = BTreeMap::new();
    for n in 0..8u8 {
        records.insert(rref(1, n), vec![n; 1 + usize::from(n)]);
    }
    let task = |inputs: &exec::Inputs<'_>| -> Result<Vec<u8>, blaze_keyed::exec::NotAnInput> {
        let mut out = Vec::new();
        for n in 0..3u8 {
            out.extend_from_slice(inputs.read(rref(1, n))?);
        }
        Ok(out)
    };
    let batch = Batch {
        namespace: Namespace::PUBLIC,
        toolchain: Toolchain::new(TOOLCHAIN).expect("a name").digest(),
        actions: vec![Sent { made: rref(2, 100), inputs: vec![rref(1, 0), rref(1, 1), rref(1, 2)] }, Sent { made: rref(2, 101), inputs: vec![rref(1, 0), rref(1, 1)] }],
    };
    let (local, local_outcomes) = exec::run(&batch, &records, &task);
    let sent = Batch::decode(&batch.encode()).expect("a batch");
    assert_eq!(sent, batch);
    let worker: BTreeMap<RecordRef, Vec<u8>> = sent.actions.iter().flat_map(|it| it.inputs.iter()).map(|it| (*it, records[it].clone())).collect();
    let (remote, remote_outcomes) = exec::run(&sent, &worker, &task);
    assert_eq!(remote, local);
    assert_eq!(remote_outcomes, local_outcomes);
    assert!(matches!(&remote_outcomes[1].status, Status::NotAnInput(it) if it.asked == rref(1, 2)));
    segment::read(&remote).expect("a segment");
    for (outcome, action) in remote_outcomes.iter().zip(&sent.actions) {
        assert_eq!(&Outcome::decode(&outcome.encode(), action.made).expect("an outcome"), outcome);
    }
}

// ---- the encoders, by property ----

fn id(tc: &hegel::TestCase) -> ContentId {
    let mut out = [0u8; 32];
    out[..8].copy_from_slice(&tc.draw(gs::integers::<u64>()).to_le_bytes());
    out[8] = tc.draw(gs::integers::<u8>());
    ContentId(out)
}

#[hegel::test(test_cases = 200)]
fn statements_round_trip(tc: hegel::TestCase) {
    let signer = Signer::from_seed(&[tc.draw(gs::integers::<u8>()); 32]);
    let it = Statement {
        namespace: Namespace(tc.draw(gs::integers::<u64>())),
        segment: id(&tc),
        len: tc.draw(gs::integers::<u64>()),
        toolchain: id(&tc),
        records: tc.draw(gs::integers::<u32>()),
        tier: if tc.draw(gs::booleans()) { Tier::Top } else { Tier::Fast },
        issued: tc.draw(gs::integers::<u64>()),
        builder: signer.public(),
    };
    let bytes = it.encode(&signer);
    assert_eq!(Statement::decode("s", &bytes).expect("decodes"), it);
    let mut flipped = bytes.clone();
    let at = tc.draw(gs::integers::<usize>().max_value(bytes.len() - 1));
    flipped[at] ^= 1;
    assert!(Statement::decode("s", &flipped).is_err());
}

#[hegel::test(test_cases = 100)]
fn key_lists_round_trip(tc: hegel::TestCase) {
    let anchor = Signer::from_seed(&[tc.draw(gs::integers::<u8>()); 32]);
    let n = tc.draw(gs::integers::<usize>().max_value(20));
    let keys = (0..n)
        .map(|_| KeyEntry { key: PublicKey(id(&tc).0), roles: Roles(tc.draw(gs::integers::<u32>().max_value(3))), revoked: tc.draw(gs::booleans()), from: tc.draw(gs::integers::<u64>()), until: tc.draw(gs::integers::<u64>()) })
        .collect();
    let ns = Namespace(tc.draw(gs::integers::<u64>()));
    let it = KeyList { namespace: ns, serial: tc.draw(gs::integers::<u64>()), issued: tc.draw(gs::integers::<u64>()), keys };
    let bytes = it.encode(&anchor);
    assert_eq!(KeyList::decode("k", &bytes, ns, &[anchor.public()]).expect("decodes"), it);
    assert!(KeyList::decode("k", &bytes, ns, &[]).is_err());
}

#[hegel::test(test_cases = 100)]
fn roots_round_trip(tc: hegel::TestCase) {
    let signer = Signer::from_seed(&[tc.draw(gs::integers::<u8>()); 32]);
    let runs_n = tc.draw(gs::integers::<usize>().max_value(4));
    let mut runs = Vec::new();
    for g in 0..runs_n {
        let n = 1 + tc.draw(gs::integers::<usize>().max_value(5));
        let firsts: BTreeSet<[u8; 32]> = (0..n).map(|_| id(&tc).0).collect();
        runs.push(Run { generation: g as u64, shards: firsts.into_iter().map(|first| ShardRef { first, digest: id(&tc), count: 1 + tc.draw(gs::integers::<u32>().max_value(1000)) }).collect() });
    }
    let ns = Namespace(tc.draw(gs::integers::<u64>()));
    let it = Root { namespace: ns, generation: tc.draw(gs::integers::<u64>()), keys_serial: tc.draw(gs::integers::<u64>()), issued: tc.draw(gs::integers::<u64>()), previous: id(&tc), runs };
    let (back, by) = Root::decode("r", &it.encode(&signer), ns).expect("decodes");
    assert_eq!((back, by), (it, signer.public()));
}

#[hegel::test(test_cases = 100)]
fn bundles_round_trip(tc: hegel::TestCase) {
    let signer = Signer::from_seed(&[tc.draw(gs::integers::<u8>()); 32]);
    let id_ = BundleId::new(TOOLCHAIN, "x86_64-unknown-linux-gnu", if tc.draw(gs::booleans()) { "std" } else { "blaze-std.release" }).expect("an id");
    let n = tc.draw(gs::integers::<usize>().max_value(30));
    let segments = (0..n).map(|_| SegmentRef { digest: id(&tc), len: tc.draw(gs::integers::<u64>()), index_at: tc.draw(gs::integers::<u64>()), count: tc.draw(gs::integers::<u32>()) }).collect();
    let ns = Namespace(tc.draw(gs::integers::<u64>()));
    let it = Bundle { namespace: ns, id: id_.clone(), issued: tc.draw(gs::integers::<u64>()), keys_serial: tc.draw(gs::integers::<u64>()), segments };
    let bytes = it.encode(&signer);
    let (back, by) = Bundle::decode("b", &bytes, ns, &id_).expect("decodes");
    assert_eq!((back, by), (it, signer.public()));
    // A manifest is only ever the one its name asks for.
    let other = BundleId::new(TOOLCHAIN, "x86_64-unknown-linux-gnu", "other").expect("an id");
    assert!(Bundle::decode("b", &bytes, ns, &other).is_err());
}

#[hegel::test(test_cases = 100)]
fn segments_round_trip(tc: hegel::TestCase) {
    let n = tc.draw(gs::integers::<usize>().max_value(20));
    let mut recs = Records::new();
    for _ in 0..n {
        recs.insert(id(&tc).0, (tc.draw(gs::integers::<u8>()), tc.draw(gs::binary().max_size(100))));
    }
    let ns = Namespace(tc.draw(gs::integers::<u64>()));
    let all: Vec<([u8; 32], u8, Vec<u8>)> = recs.iter().map(|(k, (kind, v))| (*k, *kind, v.clone())).collect();
    let bytes = segment::make(ns, &all);
    trust::reset_checks();
    let read = segment::read(&bytes).expect("reads");
    assert_eq!(read.namespace, ns);
    assert_eq!(read.records.len(), recs.len());
    for (rec, (key, (kind, payload))) in read.records.iter().zip(&recs) {
        assert_eq!((rec.key, rec.kind), (*key, *kind));
        assert_eq!(&bytes[rec.at as usize..rec.at as usize + rec.len as usize], payload.as_slice());
        assert_eq!(rec.value, segment::value_digest(*kind, payload));
    }
}

#[hegel::test(test_cases = 100)]
fn shards_round_trip(tc: hegel::TestCase) {
    let n = 1 + tc.draw(gs::integers::<usize>().max_value(600));
    let keys: BTreeSet<[u8; 32]> = (0..n).map(|_| id(&tc).0).collect();
    let entries: Vec<Entry> = keys
        .into_iter()
        .map(|key| {
            let count = 1 + tc.draw(gs::integers::<u32>().max_value(100));
            let index_at = 128 + 16 * (1 + u64::from(tc.draw(gs::integers::<u32>().max_value(1000))));
            Entry { key, value: id(&tc), segment: id(&tc), segment_len: index_at + u64::from(count) * 48 + 48, index_at, at: 128, len: 16, count }
        })
        .collect();
    let ns = Namespace(tc.draw(gs::integers::<u64>()));
    let (digest, bytes) = shard::encode(ns, 3, &entries);
    let shard_ref = ShardRef { first: entries[0].key, digest, count: entries.len() as u32 };
    trust::reset_checks();
    assert_eq!(shard::decode_whole("s", &bytes, &shard_ref, ns, &Cache::default()).expect("decodes"), entries);
    assert_eq!(trust::checks().twice, 0);
}

#[hegel::test(test_cases = 100)]
fn batches_and_outcomes_round_trip(tc: hegel::TestCase) {
    let n = tc.draw(gs::integers::<usize>().max_value(10));
    let actions: Vec<Sent> = (0..n)
        .map(|_| {
            let inputs = (0..tc.draw(gs::integers::<usize>().max_value(5))).map(|_| RecordRef { tag: Tag(tc.draw(gs::integers::<u16>())), key: id(&tc) }).collect();
            Sent { made: RecordRef { tag: Tag(tc.draw(gs::integers::<u16>())), key: id(&tc) }, inputs }
        })
        .collect();
    let batch = Batch { namespace: Namespace(tc.draw(gs::integers::<u64>())), toolchain: id(&tc), actions };
    assert_eq!(Batch::decode(&batch.encode()).expect("decodes"), batch);
    let made = RecordRef { tag: Tag(1), key: id(&tc) };
    let status = match tc.draw(gs::integers::<u8>().max_value(2)) {
        0 => Status::Made((0..tc.draw(gs::integers::<usize>().max_value(4))).map(|_| SegmentRef { digest: id(&tc), len: tc.draw(gs::integers::<u64>()), index_at: tc.draw(gs::integers::<u64>()), count: tc.draw(gs::integers::<u32>()) }).collect()),
        1 => Status::NotAnInput(blaze_keyed::exec::NotAnInput { asked: RecordRef { tag: Tag(tc.draw(gs::integers::<u16>())), key: id(&tc) }, action: made }),
        _ => Status::Failed,
    };
    let outcome = Outcome { action: made.key, status, logs: tc.draw(gs::binary().max_size(64)), usage: Usage { cpu_ns: tc.draw(gs::integers::<u64>()), peak_bytes: tc.draw(gs::integers::<u64>()) } };
    assert_eq!(Outcome::decode(&outcome.encode(), made).expect("decodes"), outcome);
}
