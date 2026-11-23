//! The published set end to end (published.md), on bstd's simulated world: home A holds
//! publishable records (`PUBLIC`, under the toolchain's root) and private ones (`LOCAL`, under the
//! same root, each carrying a marker); A's walk, layout and folder write publish the set; home B
//! reads it through the command's store.
//!
//! 1. Only the publishable class leaves A: no object written holds a private record's marker.
//! 2. B reads each record byte-equal, verified once: a second lookup is a local hit that asks the
//!    origin nothing and checks nothing again; B's floor is the set's generation.
//! 3. Each planted fault is refused by name with nothing taken: a flipped byte, a cut object, a
//!    head naming a size past the limit, an untrusted key, another set's head, a rollback, a name
//!    that would leave the folder; and a publish stopped at any object, with syncs failing and a
//!    crash (a DST: the oracle says B sees the old set whole or the new one whole; the planted
//!    control writes the head first and is caught).
//! 4. Two homes publishing the same records, put in other orders, write equal bytes.

use blaze_cas::Key as CasKey;
use blaze_cas::plain::{Namespace, Settings};
use blaze_drive::Key;
use blaze_ids::ContentId;
use blaze_own::Alloc;
use blaze_remote::published::set::{SetCheck, SetName, SetNameError, is_object_name};
use blaze_remote::read::form::HEAD_SIGNED;
use blaze_remote::read::{Answer, Head, HeadCheck, Limits as ReadLimits, Reader, Refused, Report, SignatureWhy, Unfetched, fetch};
use blaze_remote::trust::{self, Signer};
use blaze_sim::{Broke, Config, Tape};
use blaze_toolchain::ToolchainId;
use bstd::cx::Cx;
use bstd::fs::{Content, Dir, FsKind, ReadWrite};
use bstd::io::{Io, Limits, SimWorld};
use bstd::mem::TASK_FRAME;
use bstd::path::Path;
use bstd::test::BUDGET;

use super::plain_place::PlainPlace;
use blaze_keyed::kind::ReservedTag;
use super::published::folder::{self, Order, Plan};
use super::published::{self, PublishedError};
use super::{CommandLent, CommandStore, Facts, FolderOrigin, Home, NamedOrigin, Store};

// The fixture's pieces are the store's tests' (store/take_tests.rs, store/publish_command_tests.rs
// take them too): one home made one way.
pub(super) const TOOLCHAIN: ToolchainId = ToolchainId([0x5a; 32]);
pub(super) const TARGET: &str = "aarch64-apple-darwin";
pub(super) const SEED: [u8; 32] = [7; 32];
const OTHER_SEED: [u8; 32] = [9; 32];
pub(super) const MARKER: &[u8] = b"PRIVATE-MARKER";
/// Publishable records, and private ones.
pub(super) const N: u32 = 24;
const M: u32 = 8;

pub(super) type Records = std::vec::Vec<(ContentId, std::vec::Vec<u8>)>;

pub(super) struct Planted {
    pub(super) public: Records,
    pub(super) private: Records,
}

pub(super) fn planted(public: core::ops::Range<u32>, private: u32) -> Planted {
    let record = |public: bool, n: u32| {
        let key = ContentId::of("published e2e test record", &[u8::from(public)].iter().chain(&n.to_le_bytes()).copied().collect::<std::vec::Vec<u8>>());
        let mut bytes = if public { b"public record ".to_vec() } else { MARKER.to_vec() };
        bytes.extend_from_slice(&n.to_le_bytes());
        bytes.extend((0..64 + (n as usize * 37) % 512).map(|i| (n as usize * 31 + i) as u8));
        (key, bytes)
    };
    Planted { public: public.map(|n| record(true, n)).collect(), private: (0..private).map(|n| record(false, n)).collect() }
}

pub(super) fn path(text: &str) -> Path {
    Path::new(text).expect("a path")
}

pub(super) fn set_name(hex: &blaze_toolchain::Hex) -> SetName<'_> {
    SetName::new(hex.as_str(), TARGET).expect("a set's name")
}

pub(super) fn set_path(origin: &str) -> std::string::String {
    format!("{origin}/public/{}/{TARGET}", TOOLCHAIN.hex().as_str())
}

/// The home `home` (its store opened with the only test heap there is: UNSURE in the report)
/// holding `records` under the toolchain's root, put in that order or reversed.
pub(super) fn plant(io: &Io, cx: &Cx, home: &str, records: &Planted, reversed: bool) -> (Home, CommandLent) {
    let fs = io.need_fs().expect("files");
    let home = Home::of(cx, &fs, Some(home)).expect("the home");
    // CI's settings: a trusted writer, whose puts may name the public namespace.
    let settings = Settings { trusted_writer: true, ..Settings::default() };
    let lent = home.open_store(&settings, Facts { boot: Some(1), nonce: 5 }, blaze_own::testing::heap().alloc()).expect("the store opens");
    {
        // A trusted writer's store: a publishable record is `PUBLIC`, a private one `LOCAL`.
        let mut place = PlainPlace::new(&lent, true);
        let mut all: std::vec::Vec<(bool, &(ContentId, std::vec::Vec<u8>))> = records.public.iter().map(|it| (true, it)).chain(records.private.iter().map(|it| (false, it))).collect();
        if reversed {
            all.reverse();
        }
        let mut made: std::vec::Vec<CasKey> = std::vec::Vec::new();
        for (public, (key, bytes)) in all {
            place.put_record(Key(*key), public, ReservedTag::Drive.tag(), bytes).expect("a put");
            made.push(CasKey(*key));
        }
        made.sort_unstable();
        super::roots::keep_toolchain(&lent, TOOLCHAIN, &made, &[]).expect("the toolchain's root");
        place.commit().expect("a commit");
    }
    (home, lent)
}

/// A's set laid out for the folder origin `origin` and written in `order`, at most `most`
/// objects: the plan and how many were written.
pub(super) fn publish<'a>(io: &Io, cx: &Cx, lent: &CommandLent, origin: &str, signer: &Signer, order: Order, most: usize, alloc: Alloc<'a>) -> Result<(Plan<'a>, usize), PublishedError> {
    let fs = io.need_fs().expect("files");
    let root = match fs.root().make_dir_synced(cx, &path(origin)) {
        Ok(dir) => dir,
        Err(error) if error.kind == FsKind::Exists => fs.root().subdir(&path(origin)),
        Err(error) => panic!("the origin's folder: {error}"),
    };
    let mut walked = std::vec::Vec::new_in(*lent.alloc());
    published::walk_toolchain(lent, TOOLCHAIN, &mut walked)?;
    let hex = TOOLCHAIN.hex();
    let set = set_name(&hex);
    let dir = folder::set_dir(cx, &root, &set)?;
    let plan = folder::plan(cx, &dir, &set, &walked, signer, alloc)?;
    let written = if plan.unchanged { 0 } else { folder::write(cx, &dir, &plan, order, most)? };
    Ok((plan, written))
}

/// What one key read as: its answer and, when found, its bytes.
type Got = (Answer, Option<std::vec::Vec<u8>>);

/// `keys` read from the set's folder `dir` by a fresh reader with the published limits.
fn read(io: &Io, cx: &Cx, dir: &Dir<ReadWrite>, check: &dyn HeadCheck, keys: &[ContentId]) -> (std::vec::Vec<Got>, Report) {
    let erased = blaze_own::Erased::new(cx.frame());
    let alloc = erased.handle();
    let mut reader = Reader::new_in(Namespace::PUBLIC, check, ReadLimits::PUBLISHED, alloc);
    let clock = io.need_clock().expect("a clock");
    let mut now = || u64::try_from(clock.now().since_start().as_nanos()).unwrap_or(u64::MAX);
    fetch(&mut reader, &mut FolderOrigin { cx, folder: dir }, &mut now, keys, alloc).expect("a batch begun");
    let got = (0..keys.len()).map(|i| (reader.answer(i), reader.found(i).map(<[u8]>::to_vec))).collect();
    (got, reader.report())
}

pub(super) fn contains(bytes: &[u8], what: &[u8]) -> bool {
    bytes.windows(what.len()).any(|window| window == what)
}

/// A developer's home at `home`: the default settings, so everything it writes is `LOCAL`.
/// The boot is the simulated machine's, as a command takes it ([`super::boot_fact`]): a
/// crash (`io.crash()`, a power cut) is a new boot, so a segment the cut tore is checked and
/// lost by name, not trusted as this boot's (a constant boot here reported a torn segment as
/// "changed outside blaze" on 2026-11-18, seed 4, tape `0 8`).
pub(super) fn developer_home(io: &Io, cx: &Cx, home: &str, nonce: u64) -> (Home, CommandLent) {
    let fs = io.need_fs().expect("files");
    let home = Home::of(cx, &fs, Some(home)).expect("the home");
    let boot = super::boot_fact(&io.boot().expect("the simulation's boot"));
    let lent = home.open_store(&Settings::default(), Facts { boot: Some(boot), nonce }, blaze_own::testing::heap().alloc()).expect("the store opens");
    (home, lent)
}

#[test]
fn only_the_published_class_leaves_and_a_second_home_reads_each_record_once() {
    let io = Io::simulated(SimWorld::new(41, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let records = planted(0..N, M);
    let (_a, lent_a) = plant(&io, &cx, "/a", &records, false);
    let signer = Signer::from_seed(&SEED);
    let erased = blaze_own::Erased::new(cx.frame());
    let (plan, written) = publish(&io, &cx, &lent_a, "o", &signer, Order::HeadLast, usize::MAX, erased.handle()).expect("A publishes");
    assert_eq!((plan.published, plan.left_out, plan.generation, plan.unchanged), (N, M, 1, false));
    assert_eq!(written, plan.objects.len(), "every object written");
    // 1. No object holds a private record's marker.
    for sealed in plan.objects.iter() {
        assert!(!contains(&sealed.bytes, MARKER), "{} holds a private record's bytes", sealed.object.name().as_str());
        assert!(is_object_name(sealed.object.name().as_str()));
    }
    // 2. B reads through the command's store.
    let fs = io.need_fs().expect("files");
    let (_home_b, lent_b) = developer_home(&io, &cx, "/b", 6);
    let hex = TOOLCHAIN.hex();
    let set = set_name(&hex);
    let origin = NamedOrigin::published(&cx, &fs, None, "o", Some(signer.public().to_string().as_str()), set, &lent_b).expect("the published origin");
    let (clock, err) = (io.need_clock().expect("a clock"), io.need_err().expect("err"));
    let erased_b = blaze_own::Erased::new(cx.frame());
    let mut store = CommandStore::new(&cx, &lent_b, false, 0, Some(&origin), clock, &err, erased_b.handle()).expect("B's store");
    trust::reset_checks();
    for (key, bytes) in &records.public {
        let held = store.get(&cx, Key(*key)).expect("a get").expect("a published record found");
        assert_eq!(held.bytes, bytes.as_slice(), "a published record's bytes");
    }
    for (key, _) in &records.private {
        assert!(store.get(&cx, Key(*key)).expect("a get").is_none(), "a private record reached B");
    }
    let first = trust::checks();
    let asked = store.remote.as_ref().expect("the origin").reader.report();
    assert!(first.signatures >= 1 && first.twice == 0, "the head verified, nothing twice: {first:?}");
    for (key, bytes) in &records.public {
        let held = store.get(&cx, Key(*key)).expect("a get").expect("kept");
        assert_eq!(held.bytes, bytes.as_slice(), "a kept record's bytes");
    }
    assert_eq!(trust::checks(), first, "a second lookup verified again");
    assert_eq!(store.remote.as_ref().expect("the origin").reader.report().keys, asked.keys, "a second lookup asked the origin");
    lent_b.commit().expect("B's commit");
    for (key, _) in &records.public {
        let lend = lent_b.get(&CasKey(*key)).expect("a get").expect("kept");
        assert_eq!((lend.tag, lend.namespace), (ReservedTag::Drive.tag(), Namespace::LOCAL), "kept as B's own record, under its own tag");
    }
    assert_eq!(published::floor(&lent_b, &set.id()).expect("the floor"), 1, "B's floor is the generation it took");
}

/// The fixture of the planted faults: A's set published to `o`, the set's folder, its objects.
struct Published {
    io: Io,
    cx: Cx,
    records: Planted,
    objects: std::vec::Vec<(std::string::String, std::vec::Vec<u8>)>,
}

fn published_set(seed: u64) -> Published {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let records = planted(0..N, M);
    let objects = {
        let (_a, lent) = plant(&io, &cx, "/a", &records, false);
        let erased = blaze_own::Erased::new(cx.frame());
        let (plan, _) = publish(&io, &cx, &lent, "o", &Signer::from_seed(&SEED), Order::HeadLast, usize::MAX, erased.handle()).expect("A publishes");
        plan.objects.iter().map(|sealed| (sealed.object.name().as_str().to_owned(), sealed.bytes.to_vec())).collect()
    };
    Published { io, cx, records, objects }
}

impl Published {
    fn dir(&self) -> Dir<ReadWrite> {
        self.io.need_fs().expect("files").open_dir(&self.cx, &path(&set_path("o"))).expect("the set's folder")
    }

    fn object(&self, starts: &str) -> (std::string::String, std::vec::Vec<u8>) {
        self.objects.iter().find(|(name, _)| name.starts_with(starts)).cloned().expect("the object")
    }

    fn replace(&self, name: &str, bytes: &[u8]) {
        self.dir().publish_synced(&self.cx, &path(name), Content::bytes(bytes)).expect("the origin's file replaced");
    }

    fn read(&self, check: &dyn HeadCheck, keys: &[ContentId]) -> (std::vec::Vec<Got>, Report) {
        read(&self.io, &self.cx, &self.dir(), check, keys)
    }

    fn keys(&self) -> std::vec::Vec<ContentId> {
        self.records.public.iter().map(|(key, _)| *key).collect()
    }
}

fn trusted(set: ContentId, seen: u64) -> ([trust::PublicKey; 1], ContentId, u64) {
    ([Signer::from_seed(&SEED).public()], set, seen)
}

fn set_id() -> ContentId {
    let hex = TOOLCHAIN.hex();
    set_name(&hex).id()
}

#[test]
fn a_flipped_byte_and_a_cut_object_are_refused_by_name() {
    let world = published_set(43);
    let (keys, set, seen) = trusted(set_id(), 0);
    let check = SetCheck { set, keys: &keys, seen };
    let (name, segment) = world.object("seg-");
    // A byte flipped inside the first record's payload.
    let (key, value) = &world.records.public[0];
    let at = segment.windows(value.len()).position(|window| window == value.as_slice()).expect("the record in its segment");
    let mut flipped = segment.clone();
    flipped[at + 3] ^= 0x40;
    world.replace(&name, &flipped);
    let (got, _) = world.read(&check, &[*key]);
    assert!(matches!(got[0], (Answer::Unfetched(Unfetched::Refused(Refused::Digest { .. })), None)), "a flipped byte: {:?}", got[0].0);
    // The segment cut in half: what lies past the cut is short, never other bytes.
    world.replace(&name, &segment[..segment.len() / 2]);
    let (got, _) = world.read(&check, &world.keys());
    let mut short = 0;
    for ((answer, bytes), (_, value)) in got.iter().zip(&world.records.public) {
        match answer {
            Answer::Found { .. } => assert_eq!(bytes.as_deref(), Some(value.as_slice()), "a record of a cut segment read as other bytes"),
            Answer::Unfetched(Unfetched::Short { .. }) => short += 1,
            other => panic!("a cut segment: {other:?}"),
        }
    }
    assert!(short > 0, "no record lay past the cut: the case proves nothing");
}

#[test]
fn a_head_past_the_limit_an_untrusted_key_and_another_sets_head_are_refused_before_a_payload_is_asked() {
    let world = published_set(47);
    let signer = Signer::from_seed(&SEED);
    let (keys, set, seen) = trusted(set_id(), 0);
    // A head the trusted key signed for this set, naming a manifest past any limit.
    let forged = Head { namespace: Namespace::PUBLIC, generation: 2, manifest: ContentId([1; 32]), manifest_len: u64::MAX >> 1, signer: signer.public() };
    let forged = forged.encode(|domain, body| {
        let mut message = [0u8; 32 + HEAD_SIGNED];
        message[..32].copy_from_slice(&set.0);
        message[32..].copy_from_slice(body);
        signer.sign(domain, &message)
    });
    let (_, prefix) = world.object("prefix");
    world.replace("head", &forged);
    world.replace("prefix", &[&forged[..], &prefix[forged.len()..]].concat());
    let (got, report) = world.read(&SetCheck { set, keys: &keys, seen }, &world.keys());
    assert!(matches!(got[0].0, Answer::Unfetched(Unfetched::Refused(Refused::TooLarge { .. }))), "a head past the limit: {:?}", got[0].0);
    assert_eq!(report.payload_asks, 0, "a payload was asked under a refused head");
    // The true set back; a reader that trusts another key, and one reading as another set.
    let (_, head) = world.object("head");
    world.replace("head", &head);
    world.replace("prefix", &prefix);
    let other = [Signer::from_seed(&OTHER_SEED).public()];
    let (got, report) = world.read(&SetCheck { set, keys: &other, seen }, &world.keys());
    assert!(matches!(got[0].0, Answer::Unfetched(Unfetched::Refused(Refused::Signature { why: SignatureWhy::UnknownSigner, .. }))), "an untrusted key: {:?}", got[0].0);
    assert_eq!(report.payload_asks, 0);
    let linux = SetName::new(TOOLCHAIN.hex().as_str(), "x86_64-unknown-linux-gnu").expect("a set's name").id();
    let (got, report) = world.read(&SetCheck { set: linux, keys: &keys, seen }, &world.keys());
    assert!(matches!(got[0].0, Answer::Unfetched(Unfetched::Refused(Refused::Signature { why: SignatureWhy::Invalid(_), .. }))), "another set's head: {:?}", got[0].0);
    assert_eq!(report.payload_asks, 0);
    // A name that would leave the folder is no set's and no object's.
    assert!(matches!(SetName::new("..", TARGET), Err(SetNameError::Byte { .. })));
    assert!(matches!(SetName::new(TOOLCHAIN.hex().as_str(), "../../etc"), Err(SetNameError::Byte { .. })));
    for name in ["../head", "seg-../../x", "/head", "head/"] {
        assert!(!is_object_name(name), "{name} taken as an object's name");
    }
}

#[test]
fn a_rollback_to_an_older_head_is_refused_after_its_signature_stands() {
    let world = published_set(53);
    let (old_head, old_prefix) = (world.object("head").1, world.object("prefix").1);
    // A second publisher's home with more records lays the set out again, one generation on.
    {
        let (_a2, lent) = plant(&world.io, &world.cx, "/a2", &planted(0..N + 4, M), false);
        let erased = blaze_own::Erased::new(world.cx.frame());
        let (plan, _) = publish(&world.io, &world.cx, &lent, "o", &Signer::from_seed(&SEED), Order::HeadLast, usize::MAX, erased.handle()).expect("the second publish");
        assert_eq!((plan.generation, plan.unchanged), (2, false));
        // The same records again: the same head, nothing written.
        let (again, written) = publish(&world.io, &world.cx, &lent, "o", &Signer::from_seed(&SEED), Order::HeadLast, usize::MAX, erased.handle()).expect("a publish of nothing new");
        assert_eq!((again.generation, again.unchanged, written), (2, true, 0));
    }
    let (keys, set, _) = trusted(set_id(), 0);
    let (got, _) = world.read(&SetCheck { set, keys: &keys, seen: 2 }, &world.keys());
    assert!(got.iter().all(|(answer, _)| matches!(answer, Answer::Found { .. })), "generation 2 read at floor 2");
    world.replace("head", &old_head);
    world.replace("prefix", &old_prefix);
    let (got, report) = world.read(&SetCheck { set, keys: &keys, seen: 2 }, &world.keys());
    assert!(
        matches!(got[0].0, Answer::Unfetched(Unfetched::Refused(Refused::Signature { why: SignatureWhy::Stale { generation: 1, seen: 2 }, .. }))),
        "a rollback: {:?}",
        got[0].0
    );
    assert_eq!(report.payload_asks, 0, "a payload was asked under a rolled-back head");
}

#[test]
fn two_homes_publishing_the_same_records_write_equal_bytes() {
    let io = Io::simulated(SimWorld::new(59, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let records = planted(0..N, M);
    let signer = Signer::from_seed(&SEED);
    let (_a, lent_a) = plant(&io, &cx, "/a", &records, false);
    let (_b, lent_b) = plant(&io, &cx, "/b", &records, true);
    let erased = blaze_own::Erased::new(cx.frame());
    let (one, _) = publish(&io, &cx, &lent_a, "o1", &signer, Order::HeadLast, usize::MAX, erased.handle()).expect("A publishes");
    let (two, _) = publish(&io, &cx, &lent_b, "o2", &signer, Order::HeadLast, usize::MAX, erased.handle()).expect("B publishes");
    let named = |plan: &Plan<'_>| plan.objects.iter().map(|sealed| (sealed.object.name().as_str().to_owned(), sealed.bytes.to_vec())).collect::<std::vec::Vec<_>>();
    assert_eq!(named(&one), named(&two), "two publishers of one set wrote other bytes");
}

fn harness(what: &str, error: impl core::fmt::Display) -> Broke {
    Broke::Harness(format!("{what}: {error}"))
}

/// A second publish stopped after a drawn number of objects, its folder's syncs failing or not,
/// then a crash: a reader sees every old record (both sets hold them) and the new records all or
/// none, never other bytes.
fn interrupted(tape: &Tape, order: Order) -> Result<(), Broke> {
    let seed = tape.below(1 << 32, "sim seed");
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).map_err(|e| harness("a root context", e))?;
    let (old, new) = (planted(0..6, 2), planted(0..9, 2));
    let signer = Signer::from_seed(&SEED);
    {
        let (_a, lent) = plant(&io, &cx, "/a", &old, false);
        let erased = blaze_own::Erased::new(cx.frame());
        publish(&io, &cx, &lent, "o", &signer, Order::HeadLast, usize::MAX, erased.handle()).map_err(|e| harness("the first publish", e))?;
    }
    {
        let (_a2, lent) = plant(&io, &cx, "/a2", &new, false);
        let erased = blaze_own::Erased::new(cx.frame());
        let dir = io.need_fs().map_err(|e| harness("files", e))?.open_dir(&cx, &path(&set_path("o"))).map_err(|e| harness("the set's folder", e))?;
        let mut walked = std::vec::Vec::new_in(*lent.alloc());
        published::walk_toolchain(&lent, TOOLCHAIN, &mut walked).map_err(|e| harness("the walk", e))?;
        let hex = TOOLCHAIN.hex();
        let plan = folder::plan(&cx, &dir, &set_name(&hex), &walked, &signer, erased.handle()).map_err(|e| harness("the plan", e))?;
        let most = tape.below(plan.objects.len() as u64 + 1, "objects written before the crash") as usize;
        if tape.one_in(3, "the folder's syncs fail") {
            // clone: the directory's name, a refcount.
            let name = dir.name().name().expect("the set's folder is named").clone();
            io.with_sim(|sim| sim.fail_syncs(name));
        }
        // A failed sync stops the write where it is, as the crash does.
        let _stopped = folder::write(&cx, &dir, &plan, order, most);
    }
    if !io.crash() {
        return Err(Broke::Harness("the world is not simulated: no crash".into()));
    }
    let cx = io.cx(BUDGET, TASK_FRAME).map_err(|e| harness("a context after the crash", e))?;
    let dir = io.need_fs().map_err(|e| harness("files", e))?.open_dir(&cx, &path(&set_path("o"))).map_err(|e| harness("the set's folder", e))?;
    let (keys, set, seen) = trusted(set_id(), 0);
    let ids: std::vec::Vec<ContentId> = new.public.iter().map(|(key, _)| *key).collect();
    let (got, _) = read(&io, &cx, &dir, &SetCheck { set, keys: &keys, seen }, &ids);
    let mut new_found = 0;
    for (i, ((answer, bytes), (_, value))) in got.iter().zip(&new.public).enumerate() {
        let found = matches!(answer, Answer::Found { .. });
        if found && bytes.as_deref() != Some(value.as_slice()) {
            return Err(Broke::Oracle("a record read as other bytes".into()));
        }
        match (i < old.public.len(), found) {
            (true, false) => return Err(Broke::Oracle("neither the old set whole nor the new one: an old record was not found".into())),
            (false, true) => new_found += 1,
            _ => {}
        }
    }
    if new_found != 0 && new_found != new.public.len() - old.public.len() {
        return Err(Broke::Oracle("neither the old set whole nor the new one: some new records and not others".into()));
    }
    Ok(())
}

/// The invariant: publishing a set interrupted at any point leaves either the old published set whole or the new one, never writing a head before its payloads.
#[test]
fn a_publish_stopped_anywhere_leaves_the_old_set_whole_or_the_new() {
    blaze_sim::check(Config::cases(48), |tape| interrupted(tape, Order::HeadLast));
}

#[test]
fn the_oracle_catches_a_publish_that_writes_its_head_first() {
    blaze_sim::control(Config::cases(96), Order::HeadLast, Order::HeadFirst, "neither the old set whole nor the new one", interrupted);
}
