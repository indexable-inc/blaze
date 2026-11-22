//! The take of a published set (store/take.rs), on bstd's simulated world: home A publishes its
//! toolchain's records to the folder origin `o` in many small segments; developer homes take it.
//!
//! 1. A fresh home takes the set whole, once: N records taken, none already there, every one a
//!    LOCAL record under its own tag, the root naming all N; its next command asks the origin
//!    nothing (within the window); past the window one head read, nothing taken.
//! 2. A flipped payload byte is refused by name, nothing rooted; the planted take that checks
//!    no digest takes it, and the oracle says so (a control).
//! 3. A take stopped at any record, its home's syncs failing or not, then a crash: the root is
//!    absent or names the set whole (never half a root), and the next command finishes the take
//!    (a DST; the planted take that names the root after each segment is caught).
//! 4. With no key named, a set signed by a key blaze does not carry is refused: nothing taken.

use blaze_cas::Key as CasKey;
use blaze_cas::plain::Namespace;
use blaze_ids::ContentId;
use blaze_remote::PublicKey;
use blaze_remote::read::{Refused, SignatureWhy, Unfetched};
use blaze_remote::trust::Signer;
use blaze_sim::{Broke, Config, Tape};
use bstd::cx::Cx;
use bstd::fs::{Content, Dir, FsKind, ReadWrite};
use bstd::io::{Io, Limits, SimWorld};
use bstd::mem::TASK_FRAME;
use bstd::test::BUDGET;
use core::time::Duration;

use blaze_keyed::kind::ReservedTag;
use super::super::published::folder::{self, Order};
use super::super::published;
use super::super::published_e2e_tests::{N, Planted, SEED, TOOLCHAIN, contains, developer_home, path, plant, planted, set_name, set_path};
use super::super::roots::RootName;
use super::super::{CommandLent, FolderOrigin};
use super::{FRESH_NS, How, SegmentWhy, TakeError, TakePlant, Taken, take_from};

/// Payload bytes a segment of the test's set holds at most: the records' 80 to 600 bytes make
/// about ten segments of the 24.
pub(super) const SMALL_SEGMENTS: u64 = 1024;

pub(super) fn set_id() -> ContentId {
    let hex = TOOLCHAIN.hex();
    set_name(&hex).id()
}

/// A's set, published to the folder origin `origin` in segments of `segment_bytes`: the
/// objects written, by name.
pub(super) fn publish_sized(io: &Io, cx: &Cx, lent: &CommandLent, origin: &str, segment_bytes: u64) -> std::vec::Vec<(std::string::String, std::vec::Vec<u8>)> {
    let fs = io.need_fs().expect("files");
    let root = match fs.root().make_dir_synced(cx, &path(origin)) {
        Ok(dir) => dir,
        Err(error) if error.kind == FsKind::Exists => fs.root().subdir(&path(origin)),
        Err(error) => panic!("the origin's folder: {error}"),
    };
    let mut walked = std::vec::Vec::new_in(*lent.alloc());
    published::walk_toolchain(lent, TOOLCHAIN, &mut walked).expect("A's walk");
    let hex = TOOLCHAIN.hex();
    let set = set_name(&hex);
    let dir = folder::set_dir(cx, &root, &set).expect("the set's folder");
    let erased = blaze_own::Erased::new(cx.frame());
    let plan = folder::plan_sized(cx, &dir, &set, &walked, &Signer::from_seed(&SEED), segment_bytes, erased.handle()).expect("A's plan");
    folder::write(cx, &dir, &plan, Order::HeadLast, usize::MAX).expect("A's write");
    plan.objects.iter().map(|sealed| (sealed.object.name().as_str().to_owned(), sealed.bytes.to_vec())).collect()
}

/// The fixture: A's N public and M private records, published to `o` in small segments.
pub(super) struct World {
    pub io: Io,
    pub records: Planted,
    pub objects: std::vec::Vec<(std::string::String, std::vec::Vec<u8>)>,
}

pub(super) fn world(seed: u64) -> World {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let records = planted(0..N, 8);
    let objects = {
        let (_a, lent) = plant(&io, &cx, "/a", &records, false);
        publish_sized(&io, &cx, &lent, "o", SMALL_SEGMENTS)
    };
    World { io, records, objects }
}

impl World {
    fn cx(&self) -> Cx {
        self.io.cx(BUDGET, TASK_FRAME).expect("a root context")
    }

    fn dir(&self, cx: &Cx) -> Dir<ReadWrite> {
        self.io.need_fs().expect("files").open_dir(cx, &path(&set_path("o"))).expect("the set's folder")
    }

    /// The set taken from the folder into `lent`, A's key named.
    fn take(&self, cx: &Cx, lent: &CommandLent, plant: TakePlant) -> Result<Taken, TakeError> {
        self.take_trusting(cx, lent, &[Signer::from_seed(&SEED).public()], plant)
    }

    fn take_trusting(&self, cx: &Cx, lent: &CommandLent, keys: &[PublicKey], plant: TakePlant) -> Result<Taken, TakeError> {
        let clock = self.io.need_clock().expect("a clock");
        let dir = self.dir(cx);
        take_from(&mut FolderOrigin { cx, folder: &dir }, keys, set_id(), lent, TOOLCHAIN, &clock, plant)
    }

    fn total_bytes(&self) -> u64 {
        self.records.public.iter().map(|(_, bytes)| bytes.len() as u64).sum()
    }
}

/// Whether the toolchain's root is named in `lent`.
pub(super) fn rooted(lent: &CommandLent) -> bool {
    lent.root_named(RootName::toolchain(TOOLCHAIN).as_str()).is_some()
}

/// The set as `lent` holds it: `Ok(None)` with no root; `Ok(Some(n))` with a root naming `n`
/// records, each held; an oracle's word when a record is held with other bytes or the root
/// names one the store does not hold.
pub(super) fn named_by_root(lent: &CommandLent, records: &Planted) -> Result<Option<usize>, Broke> {
    if !rooted(lent) {
        return Ok(None);
    }
    let mut walked = std::vec::Vec::new_in(*lent.alloc());
    published::walk_toolchain(lent, TOOLCHAIN, &mut walked).map_err(|why| Broke::Oracle(format!("the toolchain's root names what the store cannot give: {why}")))?;
    for record in walked.iter() {
        let Some((_, want)) = records.public.iter().find(|(key, _)| *key == record.key) else {
            return Err(Broke::Oracle("the toolchain's root names a record the set does not hold".into()));
        };
        if record.bytes != want.as_slice() {
            return Err(Broke::Oracle("a record taken with other bytes than the set's".into()));
        }
        if record.namespace != Namespace::LOCAL || !ReservedTag::Drive.is(record.tag) {
            return Err(Broke::Oracle("a record taken in another namespace or under another tag".into()));
        }
    }
    Ok(Some(walked.len()))
}

#[test]
fn a_fresh_home_takes_the_set_whole_once_and_its_next_command_asks_nothing() {
    let world = world(61);
    let cx = world.cx();
    let segments = world.objects.iter().filter(|(name, _)| name.starts_with("seg-")).count();
    assert!(segments >= 4, "the set is cut into {segments} segments: the take's segment loop is not exercised");
    let total = world.total_bytes();
    // 1. The first command: everything taken, nothing already there.
    {
        let (_b, lent) = developer_home(&world.io, &cx, "/b", 6);
        assert!(!rooted(&lent), "a fresh home holds the toolchain's root");
        let taken = world.take(&cx, &lent, TakePlant::NONE).expect("B takes the set");
        std::eprintln!("blaze: {taken}");
        assert_eq!((taken.how, taken.records, taken.already, taken.bytes, taken.generation, taken.head_reads), (How::Took, N, 0, total, 1, 1));
        // The head, the prefix, then each segment whole in one read (each under 8 MiB).
        assert_eq!(taken.reads as usize, 2 + segments, "reads of the origin");
        let line = taken.to_string();
        assert!(line.starts_with("published: set "), "{line}");
        assert!(line.contains(&format!(" generation 1: {N} records, {total} bytes taken in ")), "{line}");
        assert!(line.contains(" ms (verify ") && line.ends_with(" ms); 0 already here"), "{line}");
        assert_eq!(named_by_root(&lent, &world.records), Ok(Some(N as usize)), "the root names the set whole");
        for (key, _) in &world.records.private {
            assert!(lent.get(&CasKey(*key)).expect("a get").is_none(), "a private record reached B");
        }
        assert_eq!(published::seen(&lent, &set_id()).expect("seen").generation, 1, "B's floor");
    }
    // 2. The next command, a new open of the same home within the window: nothing asked.
    {
        let (_b, lent) = developer_home(&world.io, &cx, "/b", 7);
        let taken = world.take(&cx, &lent, TakePlant::NONE).expect("the next command");
        assert!(matches!(taken.how, How::Fresh { .. }), "{taken:?}");
        assert_eq!((taken.reads, taken.head_reads, taken.records, taken.already), (0, 0, 0, 0), "the next command read the origin");
        assert_eq!(named_by_root(&lent, &world.records), Ok(Some(N as usize)));
    }
    // 3. Past the window: the head asked once, nothing else; then the window starts again.
    let clock = world.io.need_clock().expect("a clock");
    clock.sleep(&cx, Duration::from_nanos(FRESH_NS + 1)).expect("the window passes");
    {
        let (_b, lent) = developer_home(&world.io, &cx, "/b", 8);
        let taken = world.take(&cx, &lent, TakePlant::NONE).expect("a command past the window");
        assert_eq!((taken.how, taken.head_reads, taken.reads, taken.records), (How::Current, 1, 1, 0), "past the window");
        let again = world.take(&cx, &lent, TakePlant::NONE).expect("the command after it");
        assert!(matches!(again.how, How::Fresh { .. }) && again.reads == 0, "the window did not start again: {again:?}");
    }
}

#[test]
fn a_set_signed_by_a_key_blaze_does_not_carry_is_refused_with_no_key_named() {
    let world = world(67);
    let cx = world.cx();
    let (_b, lent) = developer_home(&world.io, &cx, "/b", 6);
    let refused = world.take_trusting(&cx, &lent, &[], TakePlant::NONE);
    assert!(
        matches!(refused, Err(TakeError::Read(Unfetched::Refused(Refused::Signature { why: SignatureWhy::UnknownSigner, .. })))),
        "a set of an unknown signer: {refused:?}"
    );
    assert!(!rooted(&lent), "a refused set was rooted");
    assert!(world.records.public.iter().all(|(key, _)| lent.get(&CasKey(*key)).expect("a get").is_none()), "a refused set's record was put");
}

fn harness(what: &str, error: impl core::fmt::Display) -> Broke {
    Broke::Harness(format!("{what}: {error}"))
}

/// One payload byte of a drawn record flipped in its segment's file, then a fresh home's take:
/// refused by name with nothing rooted, or every record taken equal to the set's.
fn flipped(tape: &Tape, unverified: bool) -> Result<(), Broke> {
    let world = world(tape.below(1 << 32, "sim seed"));
    let cx = world.cx();
    let i = tape.below(u64::from(N), "the record flipped") as usize;
    let (_, value) = &world.records.public[i];
    let (name, segment) = world.objects.iter().find(|(name, bytes)| name.starts_with("seg-") && contains(bytes, value)).ok_or(Broke::Harness("the record in no segment".into()))?;
    let at = segment.windows(value.len()).position(|window| window == value.as_slice()).ok_or(Broke::Harness("the record's place".into()))?;
    let mut bytes = segment.clone();
    bytes[at + tape.below(value.len() as u64, "the byte flipped") as usize] ^= 0x40;
    world.dir(&cx).publish_synced(&cx, &path(name), Content::bytes(&bytes)).map_err(|e| harness("the origin's file replaced", e))?;
    let (_b, lent) = developer_home(&world.io, &cx, "/b", 6);
    match world.take(&cx, &lent, TakePlant { unverified, ..TakePlant::NONE }) {
        Err(TakeError::Segment { why: SegmentWhy::Check(_), .. }) => match rooted(&lent) {
            true => Err(Broke::Oracle("a refused take left the toolchain's root".into())),
            false => Ok(()),
        },
        Err(other) => Err(harness("the take", other)),
        Ok(_) => match named_by_root(&lent, &world.records)? {
            Some(_) => Ok(()),
            None => Err(Broke::Oracle("a take that succeeded left no root".into())),
        },
    }
}

#[test]
fn a_flipped_byte_is_refused_by_name_and_nothing_is_rooted() {
    blaze_sim::check(Config::cases(24), |tape| flipped(tape, false));
}

#[test]
fn the_oracle_catches_a_take_that_checks_no_digest() {
    blaze_sim::control(Config::cases(48), false, true, "a record taken with other bytes than the set's", flipped);
}

/// A take stopped after a drawn number of records, its home's syncs failing or not, then a
/// crash; then the next command. The oracle: before the crash (a reader of the same boot) and
/// after it the root is absent or names the set whole; after the next command the set is
/// whole, and a take counts every record once.
fn interrupted(tape: &Tape, root_each_segment: bool) -> Result<(), Broke> {
    let world = world(tape.below(1 << 32, "sim seed"));
    let stop = tape.below(u64::from(N) + 1, "records put before the crash") as u32;
    let fail_syncs = tape.one_in(3, "the home's syncs fail");
    {
        let cx = world.cx();
        let (home, lent) = developer_home(&world.io, &cx, "/b", 6);
        if fail_syncs {
            // clone: the directory's name, a refcount.
            let name = home.dir().name().name().expect("the home is named").clone();
            world.io.with_sim(|sim| sim.fail_syncs(name));
        }
        // A failed sync stops the take where it is, as the crash does.
        let _stopped = world.take(&cx, &lent, TakePlant { root_each_segment, stop_after: Some(stop), ..TakePlant::NONE });
        // Before the crash, as a reader of this boot sees the store: what the next boot's open
        // drops (blaze-cas D200: a root reaching a record the power cut took) cannot hide a half
        // root here, where nothing was lost.
        match named_by_root(&lent, &world.records)? {
            Some(n) if n != N as usize => return Err(Broke::Oracle(format!("half a root: before the crash the toolchain's root names {n} of the set's {N} records"))),
            _ => {}
        }
    }
    if !world.io.crash() {
        return Err(Broke::Harness("the world is not simulated: no crash".into()));
    }
    let cx = world.cx();
    let (_b, lent) = developer_home(&world.io, &cx, "/b", 7);
    match named_by_root(&lent, &world.records)? {
        Some(n) if n != N as usize => return Err(Broke::Oracle(format!("half a root: the toolchain's root names {n} of the set's {N} records"))),
        _ => {}
    }
    let taken = world.take(&cx, &lent, TakePlant::NONE).map_err(|why| Broke::Oracle(format!("the next command did not finish the take: {why}")))?;
    if named_by_root(&lent, &world.records)? != Some(N as usize) {
        return Err(Broke::Oracle("after the next command the set is not whole".into()));
    }
    if taken.how == How::Took && taken.records + taken.already != N {
        return Err(Broke::Oracle(format!("the next command counted {} taken and {} already of {N}", taken.records, taken.already)));
    }
    Ok(())
}

/// The invariant: taking a published set verifies every record digest, never names the root before the set is whole, and leaves either no root or the whole set after any interruption.
#[test]
fn a_take_stopped_anywhere_leaves_no_root_or_the_whole_set_and_the_next_command_finishes_it() {
    blaze_sim::check(Config::cases(48), |tape| interrupted(tape, false));
}

#[test]
fn the_oracle_catches_a_take_that_names_the_root_before_the_set_is_whole() {
    blaze_sim::control(Config::cases(96), false, true, "half a root", interrupted);
}
