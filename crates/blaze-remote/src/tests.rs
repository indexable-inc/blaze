//! The simulation: push and pull machines against the in-memory bucket ([`crate::fake`]),
//! with crashes at every step, two pushers at once, slow and failing requests, and forgeries.
//!
//! The invariant: no byte is used unverified; after any crash the bucket is readable at its
//! last root and a later push completes it; what is pulled is what was pushed, byte for byte;
//! nothing of an organisation's namespace is under public; no client checks one object twice.
//! Each planted bug ([`Plant`]) is caught by the oracle that states it. Every choice of a case
//! (its records, every client's latencies and faults, whose turn it is) is drawn from one tape.

use std::collections::{BTreeMap, BTreeSet};

use blaze_cas::plain::{ENTRY, FOOTER, HEADER, Namespace, SEGMENT_MOST};
use blaze_ids::ContentId;
use blaze_keyed::{
    kind::{ActionResult, Kind, Origin, Tag, ToolId},
    store::{Located, Refused, Source, StoreError, Verifier, WireError},
};
use blaze_sim::{Broke, Config, Tape};

use crate::{
    PLANT, Plant,
    bucket::{Bucket, Failure, Machine, Reply, Step, Ticket, drive},
    bundle::{self, Bundle, BundleId},
    exec::{self, Batch, Outcome, RecordRef, Sent, Status, Usage},
    expire,
    fake::{self, Fake, Faults, Shared},
    find::Cache,
    name::{self, Toolchain},
    privacy,
    push::{self, BundleOut, Plan, Push},
    pull::{Proven, Remote},
    segment::{self, SegmentId, SegmentLen, SegmentRef},
    settings::Settings,
    shard::{self, Entry, Root, Run, ShardRef},
    trust::{self, ClientChecks, KeyEntry, KeyList, PublicKey, Roles, Signer, Statement, Tier, Trusted, Why},
    wire::Bounded,
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
        let keys = Bounded::new(vec![KeyEntry { key: self.builder.public(), roles: Roles(Roles::BUILD.0 | Roles::ROOT.0), revoked: false, from: 0, until: u64::MAX }]).expect("one key");
        KeyList { namespace, serial: 1, issued: 0, keys }.encode(&self.anchor)
    }

    fn trusted(&self) -> Trusted {
        let mut trusted = Trusted::new(self.toolchain.clone());
        trusted.anchor(Namespace::PUBLIC, vec![self.anchor.public()]);
        trusted.anchor(org(), vec![self.anchor.public()]);
        trusted
    }
}

/// Records by key: each one's kind (its tag) and payload.
type Records = BTreeMap<[u8; 32], (Tag, Vec<u8>)>;

fn records(tape: &Tape, n: usize) -> Records {
    let mut out = Records::new();
    for _ in 0..n {
        let mut key = [0u8; 32];
        tape.fill(&mut key, "key");
        // A calm tape draws every key alike: count up past the keys taken, so there are `n`.
        // (A seeded one never repeats a key, so its records are its stream's.)
        while out.contains_key(&key) {
            let mut low = [0u8; 8];
            low.copy_from_slice(&key[..8]);
            key[..8].copy_from_slice(&u64::from_le_bytes(low).wrapping_add(1).to_le_bytes());
        }
        let mut payload = vec![0u8; 1 + tape.index(200, "payload len")];
        tape.fill(&mut payload, "payload");
        out.insert(key, (Tag(tape.index(64, "tag") as u16), payload));
    }
    out
}

/// A segment of `namespace` holding `recs`, by the store's own writer.
fn segment_of(namespace: Namespace, recs: &[([u8; 32], (Tag, Vec<u8>))]) -> Vec<u8> {
    let all: Vec<(RecordRef, Vec<u8>)> = recs.iter().map(|(key, (tag, payload))| (RecordRef { tag: *tag, key: ContentId(*key) }, payload.clone())).collect();
    segment::make(namespace, &all).expect("the store's writer takes records of a test's size")
}

fn segments_of(namespace: Namespace, recs: &Records, per: usize) -> Vec<Vec<u8>> {
    let all: Vec<([u8; 32], (Tag, Vec<u8>))> = recs.iter().map(|(k, v)| (*k, v.clone())).collect();
    all.chunks(per).map(|chunk| segment_of(namespace, chunk)).collect()
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
fn push_with(w: &World, store: &Shared, namespace: Namespace, segs: &[Vec<u8>], tape: &Tape, faults: Faults, cas: bool) -> Result<push::Report, Failure> {
    trust::reset_checks();
    let mut machine = Push::new(plan(w, namespace, segs, Vec::new(), cas))?;
    let mut bucket = Fake::new(store, tape, faults);
    let out = drive(&mut machine, &mut bucket, &mut |()| Ok(()));
    assert_eq!(trust::checks().twice, 0, "a pusher checked one object twice: {:?}", trust::checks());
    out
}

fn push_clean(w: &World, store: &Shared, namespace: Namespace, segs: &[Vec<u8>]) -> push::Report {
    // No faults: the client draws nothing.
    push_with(w, store, namespace, segs, &Tape::seeded(1), Faults::default(), true).unwrap_or_else(|it| panic!("a clean push failed: {it}"))
}

/// What a fresh client pulls of `keys` in `namespace`: every record of every segment handed to
/// it, read through the proof. A client that checks an object twice is an error.
fn pull_all(w: &World, store: &Shared, namespace: Namespace, keys: &[[u8; 32]]) -> Result<Records, String> {
    trust::reset_checks();
    let mut bucket = Fake::new(store, &Tape::seeded(7), Faults::default());
    let mut remote = Remote::new(&mut bucket, "main", w.trusted());
    let mut got = Records::new();
    let pulled = remote.pull(namespace, keys.to_vec(), &mut |proven: Proven| {
        for rec in proven.records() {
            got.insert(rec.key, (rec.tag, proven.payload(rec).to_vec()));
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
fn crash_at_every_step(tape: &Tape) -> Result<(), String> {
    let w = World::new(1);
    let base_recs = records(tape, 40);
    let next_recs = records(tape, 60);
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
        let crashed = push_with(&w, &store, Namespace::PUBLIC, &next, tape, Faults { crash_after: Some(k), latency_ms: 30, ..Faults::default() }, true);
        let committed = if crashed.is_ok() { union(&base_recs, &next_recs) } else { base_recs.clone() };
        oracle(&w, &store, Namespace::PUBLIC, &committed, &next_recs).map_err(|it| format!("crash after {k} of {steps}: {it}"))?;
        push_with(&w, &store, Namespace::PUBLIC, &next, tape, Faults::default(), true).map_err(|it| format!("crash after {k}: the push after it failed: {it}"))?;
        oracle(&w, &store, Namespace::PUBLIC, &union(&base_recs, &next_recs), &Records::new()).map_err(|it| format!("crash after {k}, pushed again: {it}"))?;
    }
    Ok(())
}

/// The invariant: a crash at any request leaves the bucket readable at its last root, and the
/// next push completes it.
#[test]
fn a_crash_at_any_step_leaves_the_bucket_readable() {
    blaze_sim::check(Config::cases(4), crash_at_every_step);
}

#[test]
fn the_oracle_catches_a_root_put_before_its_shards() {
    // Every error of `crash_at_every_step` names the crash it follows.
    blaze_sim::control(Config::cases(4), Plant::None, Plant::RootBeforeShards, "crash after", |tape, plant| {
        planted(plant, || crash_at_every_step(tape)).map_err(Broke::Oracle)
    });
}

/// The invariant: under slow, failing, ambiguous and timed-out requests, a push ends (never
/// retrying), the bucket stays readable, and a later clean push completes it.
#[test]
fn slow_and_failing_requests_never_break_the_bucket() {
    let w = World::new(1);
    blaze_sim::check(Config::cases(48), |tape| {
        let base_recs = records(tape, 30);
        let next_recs = records(tape, 50);
        // Calm: with compare-and-swap.
        let cas = !tape.coin("without compare-and-swap");
        let store = fake::shared();
        push_clean(&w, &store, Namespace::PUBLIC, &segments_of(Namespace::PUBLIC, &base_recs, 8));
        let next = segments_of(Namespace::PUBLIC, &next_recs, 8);
        let faults = Faults { latency_ms: 20_000, fail_one_in: 9, ambiguous_one_in: 9, timeout_one_in: 13, crash_after: None };
        let out = push_with(&w, &store, Namespace::PUBLIC, &next, tape, faults, cas);
        let committed = if out.is_ok() { union(&base_recs, &next_recs) } else { base_recs.clone() };
        oracle(&w, &store, Namespace::PUBLIC, &committed, &next_recs).unwrap_or_else(|it| panic!("after a faulty push ({out:?}): {it}"));
        push_with(&w, &store, Namespace::PUBLIC, &next, tape, Faults::default(), cas).unwrap_or_else(|it| panic!("the clean push after: {it}"));
        oracle(&w, &store, Namespace::PUBLIC, &union(&base_recs, &next_recs), &Records::new()).unwrap_or_else(|it| panic!("pushed again: {it}"));
    });
}

/// A push machine stepped one round trip at a time, as `drive` steps it, with its own checks:
/// two runners on one thread are two clients, each held to checking an object once.
struct Runner<'a> {
    machine: Push<'a>,
    bucket: Fake,
    replies: Vec<(Ticket, Reply)>,
    done: Option<Result<push::Report, Failure>>,
    checks: ClientChecks,
}

fn runner<'a>(w: &'a World, store: &Shared, segs: &'a [Vec<u8>], tape: &Tape, cas: bool) -> Runner<'a> {
    // `Push::new` checks the plan's segments: those checks are this client's.
    let outer = trust::swap_checks(ClientChecks::default());
    let machine = Push::new(plan(w, Namespace::PUBLIC, segs, Vec::new(), cas)).expect("a plan");
    let checks = trust::swap_checks(outer);
    Runner { machine, bucket: Fake::new(store, tape, Faults { latency_ms: 40, ..Faults::default() }), replies: Vec::new(), done: None, checks }
}

impl Runner<'_> {
    /// One round trip, under this client's checks.
    fn tick(&mut self) {
        let outer = trust::swap_checks(std::mem::take(&mut self.checks));
        self.round_trip();
        self.checks = trust::swap_checks(outer);
    }

    fn round_trip(&mut self) {
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
fn two_pushers(tape: &Tape, cas: bool) -> Result<(), String> {
    let w = World::new(1);
    let base_recs = records(tape, 30);
    let (a_recs, b_recs) = (records(tape, 40), records(tape, 40));
    let store = fake::shared();
    push_clean(&w, &store, Namespace::PUBLIC, &segments_of(Namespace::PUBLIC, &base_recs, 8));
    let (a_segs, b_segs) = (segments_of(Namespace::PUBLIC, &a_recs, 8), segments_of(Namespace::PUBLIC, &b_recs, 8));
    let (mut a, mut b) = (runner(&w, &store, &a_segs, tape, cas), runner(&w, &store, &b_segs, tape, cas));
    for _ in 0..1_000_000 {
        // Whose turn is a choice only while both are running; calm runs the first to its end,
        // then the second.
        match (a.done.is_some(), b.done.is_some()) {
            (true, true) => break,
            (false, true) => a.tick(),
            (true, false) => b.tick(),
            (false, false) => {
                if tape.coin("the second pusher's turn") {
                    b.tick();
                } else {
                    a.tick();
                }
            }
        }
    }
    let (Some(a_out), Some(b_out)) = (a.done.take(), b.done.take()) else { return Err("two pushers did not end".to_owned()) };
    for (who, it) in [("the first", &a), ("the second", &b)] {
        let checks = it.checks.checks();
        if checks.twice != 0 {
            return Err(format!("{who} pusher checked one object twice: {checks:?}"));
        }
        if checks.digests == 0 {
            return Err(format!("{who} pusher's checks were not counted as its own: {checks:?}"));
        }
    }
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
    blaze_sim::check(Config::cases(32), |tape| -> Result<(), String> {
        two_pushers(tape, true).map_err(|it| format!("with compare-and-swap: {it}"))?;
        two_pushers(tape, false).map_err(|it| format!("without: {it}"))
    });
}

#[test]
fn the_oracle_catches_a_root_put_without_its_compare_and_swap() {
    // The unconditional root put shows as a lost write or an unreadable bucket, by the oracle,
    // or as a pusher failing: any error of `two_pushers` is the plant seen, as before the port.
    blaze_sim::control(Config::cases(32), Plant::None, Plant::IgnoreCas, "", |tape, plant| {
        planted(plant, || two_pushers(tape, true)).map_err(Broke::Oracle)
    });
}

// ---- forgeries ----

fn pushed_world(seed: u64) -> (World, Shared, Records) {
    let w = World::new(1);
    let recs = records(&Tape::seeded(seed), 50);
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
    let tape = Tape::seeded(11);
    for name in names {
        let forged = fake::fork(&store);
        {
            let mut it = forged.borrow_mut();
            let bytes = &mut it.objects.get_mut(&name).expect("listed").bytes;
            let at = tape.index(bytes.len(), "flipped byte");
            bytes[at] ^= 1 << tape.below(8, "flipped bit");
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
        // The first record's payload, just past the store's header.
        bytes[HEADER] ^= 0x80;
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
    recs.iter().map(|(k, (tag, v))| (*k, (*tag, v.iter().map(|b| b ^ 0x55).collect()))).collect()
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
    let private = records(&Tape::seeded(77), 30);
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
    let private = records(&Tape::seeded(8), 20);
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
    let segs = segments_of(Namespace::PUBLIC, &records(&Tape::seeded(9), 4), 4);
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

/// A segment of one record: header, a 16-byte payload, index, footer.
fn one_record_segment() -> SegmentRef {
    SegmentRef::new(SegmentId(ContentId([2; 32])), (HEADER + 16 + ENTRY + FOOTER) as u64, (HEADER + 16) as u64, 1).expect("a segment's shape")
}

/// The invariant: an index entry places its payload inside its segment's payloads. One that
/// does not cannot be made, and a shard naming one (written by hand) is refused.
#[test]
fn an_index_entry_outside_its_segment_is_refused() {
    let segment = one_record_segment();
    assert_eq!(Entry::new([3; 32], ContentId([1; 32]), segment, HEADER as u64, 32), Err(Why::OutsideSegment));
    assert_eq!(Entry::new([3; 32], ContentId([1; 32]), segment, HEADER as u64 - 1, 1), Err(Why::OutsideSegment));
    let inside = Entry::new([3; 32], ContentId([1; 32]), segment, HEADER as u64, 16).expect("inside");
    let (digest, bytes) = shard::encode(Namespace::PUBLIC, 1, &[inside]);
    let shard_ref = ShardRef { first: inside.key, digest, count: 1 };
    assert_eq!(shard::decode_whole("a shard", &bytes, &shard_ref, Namespace::PUBLIC, &mut Cache::default()).expect("inside"), vec![inside]);
}

/// The invariant: every limited field is limited by its type, so nothing past a limit can be
/// built, and so nothing built is refused by its own decoder (the failing bundle round trip of
/// round 1: a length of 4 GiB + 1 encoded, then refused). The segment's length and reference
/// are the shared ones (blaze-keyed's store), held to the same bounds.
#[test]
fn nothing_past_a_limit_can_be_built() {
    assert_eq!(SegmentLen::MOST, SEGMENT_MOST);
    assert_eq!(SegmentLen::new(SEGMENT_MOST + 1), Err(WireError::SegmentLen { len: SEGMENT_MOST + 1 }));
    assert_eq!(SegmentLen::new(SEGMENT_MOST).map(SegmentLen::get), Ok(SEGMENT_MOST));
    assert!(SegmentLen::new(SegmentLen::LEAST - 1).is_err());
    let zero = SegmentId(ContentId([0; 32]));
    // The minimal case of round 1's property failure: every field zero, or the length one past the limit.
    assert!(SegmentRef::new(zero, 0, 0, 0).is_err());
    assert!(SegmentRef::new(zero, SEGMENT_MOST + 1, SEGMENT_MOST + 1 - (ENTRY + FOOTER) as u64, 1).is_err());
    // An index not aligned, or inside the header, or not ending at the footer.
    let len = (HEADER + 16 + ENTRY + FOOTER) as u64;
    assert!(matches!(SegmentRef::new(zero, len + 1, (HEADER + 17) as u64, 1), Err(WireError::Index { .. })));
    assert!(SegmentRef::new(zero, len, (HEADER + 16) as u64, 2).is_err());
    assert!(SegmentRef::new(zero, (HEADER - 16 + ENTRY + FOOTER) as u64, (HEADER - 16) as u64, 1).is_err());
    // A reference's row is refused on decode by the same constructor: one past the limit.
    let mut row = one_record_segment().encode();
    row[32..40].copy_from_slice(&(SEGMENT_MOST + 1).to_le_bytes());
    assert!(SegmentRef::decode(&row).is_err());
    assert_eq!(Bounded::<u8, 2>::new(vec![1, 2, 3]), Err(Why::TooMany { count: 3, most: 2 }));
    assert_eq!(Bounded::<u8, 2>::new(vec![1, 2]).map(|it| it.len()), Ok(2));
    assert!(Bounded::<SegmentRef, { bundle::SEGMENTS_MOST }>::new(vec![one_record_segment(); bundle::SEGMENTS_MOST + 1]).is_err());
    assert!(Bounded::<KeyEntry, { trust::KEYS_MOST }>::new(vec![KeyEntry { key: PublicKey([0; 32]), roles: Roles(0), revoked: false, from: 0, until: 0 }; trust::KEYS_MOST + 1]).is_err());
    let shard = |first: u8| ShardRef { first: [first; 32], digest: ContentId([1; 32]), count: 1 };
    assert!(Run::new(1, Vec::new()).is_err());
    assert!(Run::new(1, vec![shard(2), shard(1)]).is_err());
    assert!(Run::new(1, vec![ShardRef { count: 0, ..shard(1) }]).is_err());
    assert!(Run::new(1, (0..=shard::SHARDS_MOST).map(|i| ShardRef { first: { let mut it = [0; 32]; it[..8].copy_from_slice(&(i as u64).to_be_bytes()); it }, ..shard(0) }).collect()).is_err());
    let run = Run::new(1, vec![shard(1)]).expect("a run");
    assert!(Bounded::<Run, { shard::RUNS_MOST }>::new(vec![run; shard::RUNS_MOST + 1]).is_err());
    let too_many_inputs = Bounded::<RecordRef, { exec::INPUTS_MOST }>::new(vec![rref(1, 0); exec::INPUTS_MOST + 1]);
    assert!(too_many_inputs.is_err());
    // A batch past its count. (Past its bytes takes 1.9 million inputs, 70 MB held: not run.)
    let empty = Sent { made: rref(2, 0), inputs: Bounded::empty() };
    assert_eq!(Batch::new(Namespace::PUBLIC, ContentId([0; 32]), vec![empty; exec::ACTIONS_MOST + 1]), Err("a batch of more actions than a batch holds"));
}

#[test]
fn an_oversized_object_is_refused_before_it_is_read() {
    let (w, store, recs) = pushed_world(12);
    let root = name::root(Namespace::PUBLIC, &w.toolchain, "main").expect("a root name");
    store.borrow_mut().objects.get_mut(&root).expect("pushed").bytes = vec![0; shard::ROOT_MOST as usize + 1];
    let keys: Vec<[u8; 32]> = recs.keys().copied().collect();
    assert!(uses_only_verified(&w, &store, Namespace::PUBLIC, &recs, &keys).expect("not used").is_some());
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
    let recs = records(&Tape::seeded(13), 60);
    let segs = segments_of(Namespace::PUBLIC, &recs, 15);
    let id = BundleId::new(TOOLCHAIN, "aarch64-apple-darwin", "std").expect("a bundle id");
    let digests: Vec<SegmentId> = segs.iter().map(|it| segment::read(it).expect("a segment").segment.digest()).collect();
    let store = fake::shared();
    trust::reset_checks();
    let mut machine = Push::new(plan(&w, Namespace::PUBLIC, &segs, vec![BundleOut { id: id.clone(), segments: digests }], true)).expect("a plan");
    drive(&mut machine, &mut Fake::new(&store, &Tape::seeded(1), Faults::default()), &mut |()| Ok(())).expect("pushed");

    trust::reset_checks();
    let mut bucket = Fake::new(&store, &Tape::seeded(2), Faults::default());
    let mut remote = Remote::new(&mut bucket, "main", w.trusted());
    let mut got = Records::new();
    let bundle = remote
        .bundle(Namespace::PUBLIC, &id, &mut |proven| {
            for rec in proven.records() {
                got.insert(rec.key, (rec.tag, proven.payload(rec).to_vec()));
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
    let mut bucket = Fake::new(&store, &Tape::seeded(3), Faults::default());
    let mut remote = Remote::new(&mut bucket, "main", w.trusted());
    let (_, wanted) = remote.bundle_ahead(Namespace::PUBLIC, &id).expect("ahead");
    let trusted = remote.trusted().clone();
    let verifier = trusted.verifier(Namespace::PUBLIC);
    for (it, bytes) in wanted.iter().zip(&segs) {
        let statement = remote.statement(it).expect("held");
        verifier.verify(&statement, it).expect("trusted");
        let mut out = vec![0u8; it.size() as usize];
        remote.fetch_into(it, 0..it.size(), &mut out).expect("held");
        assert_eq!(&out, bytes);
    }
    // Held bytes are let go once read whole: the same segment again is a ranged GET.
    let before = bucket_requests(&remote);
    let first = wanted[0];
    let mut head = vec![0u8; HEADER];
    remote.fetch_into(&first, 0..HEADER as u64, &mut head).expect("a ranged read");
    assert_eq!(&head[..], &segs[0][..HEADER]);
    assert_eq!(bucket_requests(&remote), before + 1);
}

fn bucket_requests(remote: &Remote<'_>) -> u64 {
    remote.report.counts.requests
}

/// The seam: a segment as this crate hands it on, with its statement as a push signs it, is one
/// the store's own directory source takes, under the id this crate's reference names.
#[test]
fn the_store_takes_the_segments_this_crate_hands_on() {
    let w = World::new(1);
    let dir = std::env::temp_dir().join(format!("blaze-remote-test-{}", std::process::id()));
    let recs = records(&Tape::seeded(14), 20);
    let mut trusted = w.trusted();
    trusted.take_list("keys", Namespace::PUBLIC, &w.key_list(Namespace::PUBLIC)).expect("the world's list");
    for bytes in segments_of(Namespace::PUBLIC, &recs, 7) {
        let read = segment::read(&bytes).expect("ours");
        let statement = signed_statement(&w, Namespace::PUBLIC, &read.segment);
        trusted.verifier(Namespace::PUBLIC).verify(&statement, &read.segment).expect("as a push signs it");
        let id = blaze_cas::plain::LocalDir::new(&dir).put(&bytes, &statement).expect("the store's check");
        assert_eq!(id, read.segment.digest());
        assert_eq!(read.segment.size(), bytes.len() as u64);
    }
    std::fs::remove_dir_all(&dir).expect("the test's directory");
}

/// A statement about `segment` of `namespace`, signed by the world's builder, as a push makes it.
fn signed_statement(w: &World, namespace: Namespace, segment: &SegmentRef) -> Statement {
    let mut statement = Statement {
        namespace,
        segment: segment.digest(),
        len: segment.segment_len(),
        toolchain: w.toolchain.digest(),
        records: segment.count(),
        tier: Tier::Top,
        issued: 1_000,
        signer: [0; 32],
        signature: [0; 64],
    };
    w.builder.sign_statement(&mut statement);
    statement
}

/// The invariant: a verifier is bound to one namespace. A statement of namespace B, signed by a
/// builder both namespaces trust and naming the segment exactly, is refused by the verifier of
/// namespace A as of another namespace; the verifier of B takes it. The planted unchecked
/// namespace is caught.
#[test]
fn a_verifier_bound_to_one_namespace_refuses_another() {
    let w = World::new(1);
    let mut trusted = w.trusted();
    trusted.take_list("keys", Namespace::PUBLIC, &w.key_list(Namespace::PUBLIC)).expect("the world's list");
    trusted.take_list("keys", org(), &w.key_list(org())).expect("the world's list");
    let bytes = segment_of(org(), &records(&Tape::seeded(17), 5).into_iter().collect::<Vec<_>>());
    let segment = segment::read(&bytes).expect("ours").segment;
    let statement = signed_statement(&w, org(), &segment);
    trust::reset_checks();
    assert_eq!(trusted.verifier(org()).verify(&statement, &segment), Ok(()));
    trust::reset_checks();
    assert_eq!(trusted.verifier(Namespace::PUBLIC).verify(&statement, &segment), Err(Refused::Namespace { found: org(), wanted: Namespace::PUBLIC }));
    // With the namespace check planted away the same statement passes: that check alone refuses.
    assert!(planted(Plant::SkipNamespace, || trusted.verifier(Namespace::PUBLIC).verify(&statement, &segment)).is_ok(), "refused by something other than its namespace");
    // And each field it binds: another segment, length, toolchain, or a flipped signature.
    let other = one_record_segment();
    assert!(matches!(trusted.verifier(org()).verify(&statement, &other), Err(Refused::Segment { .. })));
    let mut forged = statement;
    forged.signature[0] ^= 1;
    trust::reset_checks();
    assert_eq!(trusted.verifier(org()).verify(&forged, &segment), Err(Refused::Signature { signer: w.builder.public().0 }));
    let mut stranger = statement;
    World::new(9).builder.sign_statement(&mut stranger);
    assert!(matches!(trusted.verifier(org()).verify(&stranger, &segment), Err(Refused::Untrusted { .. })));
}

/// The invariant: a `Source::find` answer is only ever of the namespace asked. Records pushed to
/// an organisation's namespace are found there, and found nowhere (every slot `None`) when the
/// public namespace is asked, though one builder signs both and the keys are the same.
#[test]
fn a_find_never_answers_from_a_namespace_not_asked() {
    let w = World::new(1);
    let private = records(&Tape::seeded(18), 30);
    let store = fake::shared();
    push_clean(&w, &store, org(), &segments_of(org(), &private, 10));
    let public = records(&Tape::seeded(19), 10);
    push_clean(&w, &store, Namespace::PUBLIC, &segments_of(Namespace::PUBLIC, &public, 10));
    let keys: Vec<RecordRef> = private.iter().map(|(key, (tag, _))| RecordRef { tag: *tag, key: ContentId(*key) }).collect();

    trust::reset_checks();
    let mut bucket = Fake::new(&store, &Tape::seeded(4), Faults::default());
    let mut remote = Remote::new(&mut bucket, "main", w.trusted());
    let mut found: Vec<Option<Located>> = vec![None; keys.len()];
    remote.find(Namespace::PUBLIC, &keys, &mut found).expect("the public index read");
    assert!(found.iter().all(Option::is_none), "a key of the organisation's namespace answered from public");
    remote.find(org(), &keys, &mut found).expect("the organisation's index read");
    assert!(found.iter().all(Option::is_some), "a key pushed to the organisation's namespace not found there");
    // Every segment found there vouches for itself as the organisation's, and the public
    // verifier refuses its statement: so the store never files it as public.
    let trusted = remote.trusted().clone();
    for it in found.iter().flatten() {
        let statement = remote.statement(&it.segment).expect("its statement");
        assert_eq!(statement.namespace, org());
        assert!(matches!(trusted.verifier(Namespace::PUBLIC).verify(&statement, &it.segment), Err(Refused::Namespace { .. })));
    }
    // A slot list of the wrong length is refused, never answered in part.
    assert!(matches!(remote.find(org(), &keys, &mut found[1..]), Err(StoreError::Other { .. })));
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
                Ok(Box::new(Fake::new(&store, &Tape::seeded(1), Faults::default())) as Box<dyn Bucket>)
            });
            assert!(out.is_err_and(|it| it.contains("nothing was contacted")), "{words:?}");
        }
    }
    assert_eq!(made, 0);
    assert_eq!(store.borrow().requests, 0);
    std::fs::remove_dir_all(&dir).expect("the test's directory");
}

/// The invariant: on a machine that is not a trusted writer (the store's default settings), a
/// push with every cache setting present sends nothing and makes no bucket: its store holds
/// nothing but namespace local, by construction, and the refusal says `trusted_writer` is off.
/// No real or example secret: each value is derived from a constant label.
#[test]
fn a_push_from_a_machine_that_is_not_a_trusted_writer_sends_nothing() {
    let dir = std::env::temp_dir().join(format!("blaze-remote-untrusted-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a directory");
    let derived = |label: &[u8]| blaze_format::hex::encode(&blaze_sha256::digest(label));
    let (secret, seed) = (derived(b"blaze-remote push test: not a credential"), derived(b"blaze-remote push test: not a seed"));
    let keys = dir.join("keys");
    let env = |name: &str| -> Option<std::ffi::OsString> {
        match name {
            "BLAZE_CACHE_ENDPOINT" => Some("https://bucket.invalid".into()),
            "BLAZE_CACHE_BUCKET" => Some("b".into()),
            "BLAZE_CACHE_ACCESS_KEY_ID" => Some("test-id".into()),
            "BLAZE_CACHE_SECRET_ACCESS_KEY" => Some(secret.clone().into()),
            "BLAZE_CACHE_SIGNING_SEED" => Some(seed.clone().into()),
            "BLAZE_CACHE_KEY_LIST" => Some(keys.clone().into_os_string()),
            _ => None,
        }
    };
    let store = fake::shared();
    let mut made = 0;
    let out = crate::cmd::run(&dir, &env, &["push", r#"{"toolchain": "nightly-2026-11-13"}"#], 0, &mut |_| {
        made += 1;
        Ok(Box::new(Fake::new(&store, &Tape::seeded(1), Faults::default())) as Box<dyn Bucket>)
    });
    let why = out.expect_err("refused");
    assert!(why.contains("`trusted_writer` is off") && why.contains("nothing pushed"), "{why}");
    assert!(!why.contains(&secret) && !why.contains(&seed), "a secret shown: {why}");
    assert_eq!((made, store.borrow().requests), (0, 0));
    std::fs::remove_dir_all(&dir).expect("the test's directory");
}

#[test]
fn settings_never_show_a_secret() {
    let mut seed = [0u8; 32];
    Tape::seeded(15).fill(&mut seed, "seed");
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

/// HMAC-SHA256 (RFC 2104) on blaze-sha256: a chain independent of `ring`, which signs.
fn hmac(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&blaze_sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let (mut inner, mut outer) = (blaze_sha256::Sha256::new(), blaze_sha256::Sha256::new());
    inner.update(&block.map(|b| b ^ 0x36)).update(message);
    outer.update(&block.map(|b| b ^ 0x5c)).update(&inner.finish());
    outer.finish()
}

/// The invariant: a SigV4 signature is the one the published algorithm gives, recomputed here
/// from its definition (canonical request, string to sign, the four-step key chain) on an
/// independent HMAC-SHA256, itself held to RFC 4231's first case. No real or example secret:
/// the secret is derived from a constant label. And every signed input moves the signature.
#[test]
fn sigv4_signs_as_the_algorithm_defines() {
    let rfc4231_case_1 = hmac(&[0x0b; 20], b"Hi There");
    assert_eq!(blaze_format::hex::encode(&rfc4231_case_1), "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7");

    let secret = blaze_format::hex::encode(&blaze_sha256::digest(b"blaze-remote sigv4 test: not a credential"));
    let credentials = crate::settings::Credentials { access_key_id: "test-id".to_owned(), secret: crate::settings::Secret::new(secret.clone()) };
    let sign = |method: &str, name: &str, body: &[u8], unix_s: u64, credentials: &crate::settings::Credentials| {
        crate::s3::sign(method, "https://bucket.invalid", "b", name, &[("uploadId", "u-1"), ("partNumber", "1")], &[("If-None-Match", "*".to_owned())], body, credentials, unix_s)
    };
    let body = b"hello";
    let signed = sign("PUT", "public/x y", body, 1_794_571_199, &credentials);
    let sha = |bytes: &[u8]| blaze_format::hex::encode(&blaze_sha256::digest(bytes));
    let payload = sha(body);
    let canonical = format!(
        "PUT\n/b/public/x%20y\npartNumber=1&uploadId=u-1\nhost:bucket.invalid\nif-none-match:*\nx-amz-content-sha256:{payload}\nx-amz-date:20261113T115959Z\n\nhost;if-none-match;x-amz-content-sha256;x-amz-date\n{payload}"
    );
    let to_sign = format!("AWS4-HMAC-SHA256\n20261113T115959Z\n20261113/auto/s3/aws4_request\n{}", sha(canonical.as_bytes()));
    let key = hmac(&hmac(&hmac(&hmac(format!("AWS4{secret}").as_bytes(), b"20261113"), b"auto"), b"s3"), b"aws4_request");
    let signature = blaze_format::hex::encode(&hmac(&key, to_sign.as_bytes()));
    let header = |signed: &crate::s3::Signed, name: &str| signed.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()).expect("the header");
    assert_eq!(
        header(&signed, "authorization"),
        format!("AWS4-HMAC-SHA256 Credential=test-id/20261113/auto/s3/aws4_request, SignedHeaders=host;if-none-match;x-amz-content-sha256;x-amz-date, Signature={signature}")
    );
    assert_eq!(signed.url, "https://bucket.invalid/b/public/x%20y?partNumber=1&uploadId=u-1");
    assert!(signed.headers.iter().all(|(_, v)| !v.contains(&secret)), "the secret in a header");

    let other = crate::settings::Credentials { secret: crate::settings::Secret::new(sha(b"another label")), ..credentials.clone() };
    let moved = [
        sign("GET", "public/x y", body, 1_794_571_199, &credentials),
        sign("PUT", "public/x z", body, 1_794_571_199, &credentials),
        sign("PUT", "public/x y", b"hellp", 1_794_571_199, &credentials),
        sign("PUT", "public/x y", body, 1_794_571_200, &credentials),
        sign("PUT", "public/x y", body, 1_794_571_199, &other),
    ];
    for it in &moved {
        assert_ne!(header(it, "authorization"), header(&signed, "authorization"), "a signed input left the signature as it was");
    }
}

/// The invariant: a segment past the multipart size goes up in parts and lands whole, pulled
/// back byte for byte; the part size lowered by the machine's test-only parameter.
#[test]
fn a_large_segment_goes_up_in_parts() {
    let w = World::new(1);
    let recs = records(&Tape::seeded(16), 40);
    let segs = segments_of(Namespace::PUBLIC, &recs, 20);
    let largest = segs.iter().map(Vec::len).max().expect("segments");
    let store = fake::shared();
    trust::reset_checks();
    let mut machine = Push::new(plan(&w, Namespace::PUBLIC, &segs, Vec::new(), true)).expect("a plan").with_parts(1024, 700);
    drive(&mut machine, &mut Fake::new(&store, &Tape::seeded(1), Faults { latency_ms: 30, ..Faults::default() }), &mut |()| Ok(())).expect("pushed");
    let (parts, multipart) = (store.borrow().parts, store.borrow().multipart);
    assert!(largest > 1024 && multipart as usize == segs.iter().filter(|it| it.len() > 1024).count(), "{multipart} multipart uploads for segments of up to {largest} bytes");
    assert_eq!(parts as usize, segs.iter().filter(|it| it.len() > 1024).map(|it| it.len().div_ceil(700)).sum::<usize>());
    for bytes in &segs {
        let digest = segment::read(bytes).expect("a segment").segment.digest();
        let object = name::segment(Namespace::PUBLIC, &w.toolchain, &digest.0).expect("a name");
        assert_eq!(&store.borrow().objects[&object].bytes, bytes, "{object}: not the bytes pushed");
    }
    oracle(&w, &store, Namespace::PUBLIC, &recs, &Records::new()).expect("pulled as pushed");
}

// ---- privacy and expiry ----

#[test]
fn only_wholly_public_inputs_make_a_public_record() {
    let registry = Origin::Registry { checksum: ContentId([4; 32]) };
    let public = privacy::Inputs { origin: registry, reads: &[Namespace::PUBLIC], outside: false };
    assert_eq!(privacy::namespace_for(&public, Some(org()), true), Namespace::PUBLIC);
    assert_eq!(privacy::namespace_for(&public, None, true), Namespace::PUBLIC);
    for inputs in [privacy::Inputs { outside: true, ..public }, privacy::Inputs { reads: &[org()], ..public }, privacy::Inputs { origin: Origin::Private, ..public }] {
        assert_eq!(privacy::namespace_for(&inputs, Some(org()), true), org());
        assert_eq!(privacy::namespace_for(&inputs, None, true), Namespace::LOCAL);
    }
}

/// The invariant: on a machine that is not a trusted writer every record is local, whatever its
/// inputs and whatever organisation is set: a fully local, private build names no shared
/// namespace.
#[test]
fn a_machine_that_is_not_a_trusted_writer_writes_only_local() {
    let registry = Origin::Registry { checksum: ContentId([4; 32]) };
    for origin in [registry, Origin::Toolchain, Origin::Private] {
        for reads in [&[][..], &[Namespace::PUBLIC][..], &[org()][..]] {
            for outside in [false, true] {
                for named in [None, Some(org())] {
                    let inputs = privacy::Inputs { origin, reads, outside };
                    assert_eq!(privacy::namespace_for(&inputs, named, false), Namespace::LOCAL, "{inputs:?} under {named:?}");
                }
            }
        }
    }
    let off = blaze_cas::plain::Settings { org: Some("acme".to_owned()), ..Default::default() };
    assert_eq!(off.writes(), vec![Namespace::LOCAL]);
    let said = crate::cmd::store_status(&blaze_cas::plain::Settings::default());
    assert!(said.contains("nothing of it leaves this machine") && said.contains("trusted_writer is off"), "{said}");
    let on = blaze_cas::plain::Settings { trusted_writer: true, ..Default::default() };
    assert!(crate::cmd::store_status(&on).contains("trusted_writer is on"));
}

#[test]
fn expiry_never_takes_the_current_toolchain() {
    let tc = |n: u32| Toolchain::new(&format!("nightly-2026-10-{n:02}")).expect("a name");
    let seen: Vec<expire::Seen> = (1..=20).map(|n| expire::Seen { toolchain: tc(n), last_push: u64::from(n) * 86_400 }).collect();
    let now = 60 * 86_400;
    let gone = expire::toolchains(Namespace::PUBLIC, &tc(1), &seen, now, expire::POLICY).expect("named");
    assert!(!gone.iter().any(|it| it.contains("nightly-2026-10-01")), "the current toolchain expired");
    assert_eq!(gone.len(), 20 - 1 - expire::POLICY.keep);
    let run = Run::new(10, vec![ShardRef { first: [0; 32], digest: ContentId([1; 32]), count: 1 }]).expect("a run");
    let root = Root { namespace: Namespace::PUBLIC, generation: 100, keys_serial: 1, issued: 0, previous: ContentId([0; 32]), runs: Bounded::new(vec![run]).expect("one run") };
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
    let sent_of = |made: RecordRef, inputs: Vec<RecordRef>| Sent { made, inputs: Bounded::new(inputs).expect("within") };
    let batch = Batch::new(
        Namespace::PUBLIC,
        Toolchain::new(TOOLCHAIN).expect("a name").digest(),
        vec![sent_of(rref(2, 100), vec![rref(1, 0), rref(1, 1), rref(1, 2)]), sent_of(rref(2, 101), vec![rref(1, 0), rref(1, 1)])],
    )
    .expect("a batch");
    let (local, local_outcomes) = exec::run(&batch, &records, &task).expect("sealed");
    let sent = Batch::decode(&batch.encode()).expect("a batch");
    assert_eq!(sent, batch);
    let worker: BTreeMap<RecordRef, Vec<u8>> = sent.actions().iter().flat_map(|it| it.inputs.iter()).map(|it| (*it, records[it].clone())).collect();
    let (remote, remote_outcomes) = exec::run(&sent, &worker, &task).expect("sealed");
    assert_eq!(remote, local);
    assert_eq!(remote_outcomes, local_outcomes);
    assert!(matches!(&remote_outcomes[1].status, Status::NotAnInput(it) if it.asked == rref(1, 2)));
    // Each record made is filed under its own kind and key: the action's `made`.
    let read = segment::read(&remote).expect("a segment");
    assert_eq!(read.records.iter().map(segment::Rec::record).collect::<Vec<_>>(), vec![rref(2, 100)]);
    let tool = ToolId { tool: ContentId([7; 32]), target: ContentId([8; 32]), namespace: Namespace::PUBLIC };
    for (outcome, action) in remote_outcomes.iter().zip(sent.actions()) {
        assert_eq!(&Outcome::decode(&outcome.encode(), action.made).expect("an outcome"), outcome);
        // The outcome itself is a record of the core's `ActionResult` kind.
        let (record, bytes) = outcome.record(tool, action);
        assert_eq!((record.tag, bytes), (ActionResult::TAG, outcome.encode()));
    }
}

// ---- the encoders, by property ----

fn id(tape: &Tape) -> ContentId {
    let mut out = [0u8; 32];
    out[..8].copy_from_slice(&tape.int::<u64>("id").to_le_bytes());
    out[8] = tape.int::<u8>("id byte");
    ContentId(out)
}

/// A segment's length, drawn over its whole range, both ends included.
fn segment_len(tape: &Tape) -> SegmentLen {
    SegmentLen::new(tape.int_in(SegmentLen::LEAST..=SEGMENT_MOST, "segment len")).expect("drawn within")
}

/// A reference to a segment of any shape the store writes: up to a million records, its index
/// anywhere aligned past the header, its length up to [`SEGMENT_MOST`].
fn segment_ref(tape: &Tape) -> SegmentRef {
    let count = tape.int_in(0u32..=1 << 20, "record count");
    let tail = u64::from(count) * ENTRY as u64 + FOOTER as u64;
    let slots = (SEGMENT_MOST - tail - HEADER as u64) / 16;
    let index_at = HEADER as u64 + 16 * tape.int_in(0u64..=slots, "index slot");
    SegmentRef::new(SegmentId(id(tape)), index_at + tail, index_at, count).expect("drawn in shape")
}

fn tag(tape: &Tape) -> Tag {
    Tag(tape.int::<u16>("tag"))
}

/// The invariant: a statement signed as a push signs it decodes to itself by the one layout and
/// is taken by its namespace's verifier; a byte flipped anywhere in it is refused, by the layout
/// or by the verifier (every byte of it is bound).
#[test]
fn statements_round_trip() {
    blaze_sim::check(Config::cases(200), |tape| {
        let w = World::new(tape.int::<u8>("world"));
        let ns = Namespace(tape.int::<u64>("namespace"));
        let segment = segment_ref(tape);
        let mut it = Statement {
            namespace: ns,
            segment: segment.digest(),
            len: segment.segment_len(),
            toolchain: w.toolchain.digest(),
            records: tape.int::<u32>("records"),
            tier: if tape.bool("top tier") { Tier::Top } else { Tier::Fast },
            issued: tape.int::<u64>("issued"),
            signer: [0; 32],
            signature: [0; 64],
        };
        w.builder.sign_statement(&mut it);
        let bytes = it.encode();
        assert_eq!(Statement::decode(&bytes), Ok(it));
        let mut trusted = Trusted::new(w.toolchain.clone());
        trusted.anchor(ns, vec![w.anchor.public()]);
        trusted.take_list("keys", ns, &w.key_list(ns)).expect("the world's list");
        assert_eq!(trusted.verifier(ns).verify(&it, &segment), Ok(()));
        // Signed over another length: refused as not the segment's, by the shared length type.
        let other = segment_len(tape);
        if other != segment.segment_len() {
            let mut wrong = Statement { len: other, ..it };
            w.builder.sign_statement(&mut wrong);
            assert_eq!(trusted.verifier(ns).verify(&wrong, &segment), Err(Refused::Length { found: other, wanted: segment.segment_len() }));
        }
        let mut flipped = bytes;
        let at = tape.int_in(0usize..=Statement::LEN - 1, "flipped byte");
        flipped[at] ^= 1;
        let refused = match Statement::decode(&flipped) {
            Err(_) => true,
            Ok(back) => trusted.verifier(ns).verify(&back, &segment).is_err(),
        };
        assert!(refused, "byte {at} flipped and taken");
    });
}

#[test]
fn key_lists_round_trip() {
    blaze_sim::check(Config::cases(100), |tape| {
        let anchor = Signer::from_seed(&[tape.int::<u8>("anchor seed"); 32]);
        let n = tape.int_in(0usize..=20, "keys");
        let keys = (0..n)
            .map(|_| KeyEntry {
                key: PublicKey(id(tape).0),
                roles: Roles(tape.int_in(0u32..=3, "roles")),
                revoked: tape.bool("revoked"),
                from: tape.int::<u64>("from"),
                until: tape.int::<u64>("until"),
            })
            .collect();
        let keys = Bounded::new(keys).expect("drawn within");
        let ns = Namespace(tape.int::<u64>("namespace"));
        let it = KeyList { namespace: ns, serial: tape.int::<u64>("serial"), issued: tape.int::<u64>("issued"), keys };
        let bytes = it.encode(&anchor);
        assert_eq!(KeyList::decode("k", &bytes, ns, &[anchor.public()]).expect("decodes"), it);
        assert!(KeyList::decode("k", &bytes, ns, &[]).is_err());
    });
}

#[test]
fn roots_round_trip() {
    blaze_sim::check(Config::cases(100), |tape| {
        let signer = Signer::from_seed(&[tape.int::<u8>("signer seed"); 32]);
        let runs_n = tape.int_in(0usize..=4, "runs");
        let mut runs = Vec::new();
        for g in 0..runs_n {
            let n = 1 + tape.int_in(0usize..=5, "shards");
            let firsts: BTreeSet<[u8; 32]> = (0..n).map(|_| id(tape).0).collect();
            let shards = firsts
                .into_iter()
                .map(|first| ShardRef { first, digest: id(tape), count: 1 + tape.int_in(0u32..=shard::SHARD_MOST as u32 - 1, "shard count") })
                .collect();
            runs.push(Run::new(g as u64, shards).expect("drawn sorted, each within"));
        }
        let runs = Bounded::new(runs).expect("drawn within");
        let ns = Namespace(tape.int::<u64>("namespace"));
        let it = Root {
            namespace: ns,
            generation: tape.int::<u64>("generation"),
            keys_serial: tape.int::<u64>("keys serial"),
            issued: tape.int::<u64>("issued"),
            previous: id(tape),
            runs,
        };
        let (back, by) = Root::decode("r", &it.encode(&signer), ns).expect("decodes");
        assert_eq!((back, by), (it, signer.public()));
    });
}

#[test]
fn bundles_round_trip() {
    blaze_sim::check(Config::cases(100), |tape| {
        let signer = Signer::from_seed(&[tape.int::<u8>("signer seed"); 32]);
        let id_ = BundleId::new(TOOLCHAIN, "x86_64-unknown-linux-gnu", if tape.bool("std") { "std" } else { "blaze-std.release" }).expect("an id");
        let n = tape.int_in(0usize..=30, "segments");
        // Drawn through `SegmentRef::new`, so a reference past a limit is never drawn (round 1's
        // failure was one drawn as raw fields: a length of 4 GiB + 1 encoded, then refused).
        let segments = Bounded::new((0..n).map(|_| segment_ref(tape)).collect()).expect("drawn within");
        let ns = Namespace(tape.int::<u64>("namespace"));
        let it = Bundle { namespace: ns, id: id_.clone(), issued: tape.int::<u64>("issued"), keys_serial: tape.int::<u64>("keys serial"), segments };
        let bytes = it.encode(&signer);
        let (back, by) = Bundle::decode("b", &bytes, ns, &id_).expect("decodes");
        assert_eq!((back, by), (it, signer.public()));
        // A manifest is only ever the one its name asks for.
        let other = BundleId::new(TOOLCHAIN, "x86_64-unknown-linux-gnu", "other").expect("an id");
        assert!(Bundle::decode("b", &bytes, ns, &other).is_err());
    });
}

#[test]
fn segments_round_trip() {
    blaze_sim::check(Config::cases(100), |tape| {
        let n = tape.int_in(0usize..=20, "records");
        let mut recs = Records::new();
        for _ in 0..n {
            recs.insert(id(tape).0, (tag(tape), tape.bytes(0..=100, "payload")));
        }
        let ns = Namespace(tape.int::<u64>("namespace"));
        let all: Vec<([u8; 32], (Tag, Vec<u8>))> = recs.iter().map(|(k, v)| (*k, v.clone())).collect();
        let bytes = segment_of(ns, &all);
        trust::reset_checks();
        let read = segment::read(&bytes).expect("reads");
        assert_eq!(read.namespace, ns);
        assert_eq!((read.segment.size(), read.segment.count() as usize), (bytes.len() as u64, recs.len()));
        assert_eq!(SegmentRef::decode(&read.segment.encode()), Ok(read.segment));
        assert_eq!(read.records.len(), recs.len());
        for (rec, (key, (tag, payload))) in read.records.iter().zip(&recs) {
            assert_eq!((rec.key, rec.tag), (*key, *tag));
            assert_eq!(&bytes[rec.at as usize..rec.at as usize + rec.len as usize], payload.as_slice());
            assert_eq!(rec.value, segment::value_digest(*tag, payload));
        }
    });
}

#[test]
fn shards_round_trip() {
    blaze_sim::check(Config::cases(100), |tape| {
        let n = 1 + tape.int_in(0usize..=600, "entries");
        let keys: BTreeSet<[u8; 32]> = (0..n).map(|_| id(tape).0).collect();
        let entries: Vec<Entry> = keys
            .into_iter()
            .map(|key| {
                let segment = segment_ref(tape);
                let room = segment.index_at() - HEADER as u64;
                let at = HEADER as u64 + tape.int_in(0u64..=room, "entry at");
                let len = tape.int_in(0u64..=(segment.index_at() - at).min(u64::from(u32::MAX)), "entry len") as u32;
                Entry::new(key, id(tape), segment, at, len).expect("drawn inside")
            })
            .collect();
        let ns = Namespace(tape.int::<u64>("namespace"));
        let (digest, bytes) = shard::encode(ns, 3, &entries);
        let shard_ref = ShardRef { first: entries[0].key, digest, count: entries.len() as u32 };
        trust::reset_checks();
        assert_eq!(shard::decode_whole("s", &bytes, &shard_ref, ns, &mut Cache::default()).expect("decodes"), entries);
        assert_eq!(trust::checks().twice, 0);
    });
}

#[test]
fn batches_and_outcomes_round_trip() {
    blaze_sim::check(Config::cases(100), |tape| {
        let n = tape.int_in(0usize..=10, "actions");
        let actions: Vec<Sent> = (0..n)
            .map(|_| {
                let inputs = (0..tape.int_in(0usize..=5, "inputs")).map(|_| RecordRef { tag: Tag(tape.int::<u16>("input tag")), key: id(tape) }).collect();
                Sent { made: RecordRef { tag: Tag(tape.int::<u16>("made tag")), key: id(tape) }, inputs: Bounded::new(inputs).expect("drawn within") }
            })
            .collect();
        let batch = Batch::new(Namespace(tape.int::<u64>("namespace")), id(tape), actions).expect("drawn within");
        assert_eq!(Batch::decode(&batch.encode()).expect("decodes"), batch);
        let made = RecordRef { tag: Tag(1), key: id(tape) };
        let status = match tape.int_in(0u8..=2, "status") {
            0 => Status::Made(Bounded::new((0..tape.int_in(0usize..=4, "segments")).map(|_| segment_ref(tape)).collect()).expect("drawn within")),
            1 => Status::NotAnInput(blaze_keyed::exec::NotAnInput { asked: RecordRef { tag: Tag(tape.int::<u16>("asked tag")), key: id(tape) }, action: made }),
            _ => Status::Failed,
        };
        let logs = Bounded::new(tape.bytes(0..=64, "logs")).expect("drawn within");
        let outcome = Outcome { action: made.key, status, logs, usage: Usage { cpu_ns: tape.int::<u64>("cpu ns"), peak_bytes: tape.int::<u64>("peak bytes") } };
        assert_eq!(Outcome::decode(&outcome.encode(), made).expect("decodes"), outcome);
    });
}
