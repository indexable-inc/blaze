use blaze_core::sim::{Faults, Sim};
use blaze_core::{Done, Driver as _, Effect, FileName, Tag, run_until_idle};
use blaze_ids::ContentId;

use super::{Listed, MOST_SLOTS, Place, Placing, RENAMES_BEFORE_THE_SYNC, keep, key_named, request_of};

fn listed(n: u8, written_ns: u64) -> Listed {
    Listed { key: ContentId([n; 32]), written_ns, body: None, programs: Vec::new(), request: vec![n] }
}

/// The invariant: however many slots are kept, at most `MOST_SLOTS` stay, a slot kept again
/// under its key replaces the one there, and each one removed is older than every one that
/// stays. The control: a bound one past `MOST_SLOTS` is caught.
#[test]
fn the_slots_kept_are_bounded() {
    let mut kept = Vec::new();
    let mut gone = Vec::new();
    // Keys written out of their order of time, so "oldest" is not "first kept".
    for n in 0..=MOST_SLOTS as u8 {
        let written_ns = u64::from(n.wrapping_mul(37) % 101);
        gone.extend(keep(&mut kept, listed(n, written_ns)));
    }
    assert_eq!(kept.len(), MOST_SLOTS);
    assert_eq!(gone.len(), 1);
    let oldest = (0..=MOST_SLOTS as u8).min_by_key(|n| (u64::from(n.wrapping_mul(37) % 101), ContentId([*n; 32]))).unwrap();
    assert_eq!(gone, vec![ContentId([oldest; 32])]);
    assert!(kept.iter().all(|it| it.key != gone[0]));
    // Kept again under its key: one slot, the newer.
    let again = kept[0].key.0[0];
    assert!(keep(&mut kept, listed(again, 1000)).is_empty());
    assert_eq!(kept.iter().filter(|it| it.key.0[0] == again).count(), 1);
    assert_eq!(request_of(&kept, "/nowhere"), None);
    // The control: a list one past the bound is not what `keep` leaves.
    let mut planted = kept.clone();
    planted.push(listed(200, 0));
    assert_ne!(planted.len(), MOST_SLOTS, "the oracle does not catch a bound one past MOST_SLOTS");
    assert_eq!(key_named(&format!("/s/{:x}", ContentId([7; 32]))), Some(ContentId([7; 32])));
    assert_eq!(key_named("/s/not-a-key"), None);
}

/// The effect a [`Place`] is carried out with, as the daemon's driver does (H5): the part
/// written waiting for no sync, synced, renamed over the start file by the rename that waits
/// for the drive, the directory synced.
fn effect_of(place: &Place, tag: Tag) -> Option<Effect> {
    let file = |path: &str| FileName::new(path.trim_start_matches('/')).expect("a placer's path is a file name");
    Some(match place {
        // clone: a test's bytes, once a step
        Place::Write { to, bytes } => Effect::WriteUnsynced { file: file(to), bytes: bytes.clone(), tag },
        Place::Sync(it) => Effect::Sync { file: file(it), tag },
        Place::Rename { from, to } => Effect::Rename { from: file(from), to: file(to), tag },
        Place::SyncDir(dir) => Effect::SyncDir { dir: file(dir), tag },
        Place::Done => return None,
    })
}

/// What a crash left of a start file: absent, the image it held before, the new image whole,
/// or anything else (torn).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Left {
    Absent,
    Old,
    New,
    Torn,
}

/// A start file placed on the simulation of `seed`, with a crash after `steps` steps
/// completed (and, where `flying`, the next one asked for and not completed): what the crash
/// left. Over a start file already there where `seed` is odd.
fn placed_then_crashed(seed: u64, steps: usize, flying: bool) -> Left {
    let mut sim = Sim::new(seed, Faults::default());
    let program = "/w/target/debug/app";
    let file = FileName::new(blaze_certify::slot::start_file(program).trim_start_matches('/')).expect("a file name");
    // A few sectors, so a torn write can keep some and not others.
    let image: Vec<u8> = (0..4 * 512 + 100).map(|n| (n % 251) as u8 + 1).collect();
    let old = b"an older start image".to_vec();
    if seed % 2 == 1 {
        sim.change(&file, &old);
    }
    // clone: the test's bytes, compared after
    let mut placing = Placing::new(program, image.clone());
    for n in 0..steps {
        let Some(effect) = effect_of(&placing.step(), Tag(n as u64 + 1)) else { break };
        let mut seen = Vec::new();
        let mut effects = vec![effect];
        run_until_idle(&mut sim, &mut effects, |done, _| seen.push(done.done)).expect("the place's effects end");
        assert!(
            seen.iter().all(|it| !matches!(it, Done::Failed(_))),
            "seed {seed}: step {n} of the place failed: {seen:?}"
        );
    }
    if flying && let Some(effect) = effect_of(&placing.step(), Tag(100)) {
        sim.submit(&mut vec![effect]);
    }
    sim.crash();
    match sim.exists(&file) {
        false => Left::Absent,
        true => match sim.contents(&file) {
            it if it == image => Left::New,
            it if it == old => Left::Old,
            _ => Left::Torn,
        },
    }
}

/// Every seed of 256, a crash at the seed's step: what each left, by the placer planted with
/// the bug or not.
fn crashes(planted: bool) -> Vec<(u64, usize, Left)> {
    RENAMES_BEFORE_THE_SYNC.set(planted);
    let left = (0..256u64)
        .map(|seed| {
            let steps = (seed / 2 % 5) as usize;
            let flying = seed / 10 % 2 == 1;
            (seed, steps, placed_then_crashed(seed, steps, flying))
        })
        .collect();
    RENAMES_BEFORE_THE_SYNC.set(false);
    left
}

/// The invariant: a crash at any step of a start file's place, the step in flight or not,
/// leaves no start file, the one there before, or the whole image, never a torn one, on
/// `blaze_core::sim::Sim`'s files, whose crash tears every write not synced by sectors and
/// may undo a rename that waited for no sync; once the place is done the whole image is
/// there after any crash. The control: a placer that renames before it syncs leaves a torn
/// start file on some seed, and the oracle says so.
#[test]
fn a_crash_while_a_start_file_is_placed_leaves_none_or_all_of_it() {
    for (seed, steps, left) in crashes(false) {
        assert_ne!(left, Left::Torn, "seed {seed}: a crash after {steps} steps left a torn start file");
        if steps >= 3 {
            assert_eq!(left, Left::New, "seed {seed}: a crash after the rename lost the start file");
        }
    }
    let planted = crashes(true);
    assert!(
        planted.iter().any(|(_, _, left)| *left == Left::Torn),
        "the oracle does not catch a rename before the sync"
    );
}

/// The slot's writer against a model: a checkout of three files in two directories, each
/// with its own stamp of whole seconds (so every racy window is two of them), a header
/// outside it, a watcher that saves the column as it looks (a name inside its racy window
/// saved unknown) and moves the head to what it read, and a build of the head that is a pure
/// function of its root and the header, certified over a write-once store. The writer's
/// looks are answered by the model as it is when each is asked, and the model moves between
/// any two of them.
mod writer {
    use std::{collections::BTreeMap, sync::Once};

    use blaze_api::{Cargo, Targets};
    use blaze_certify::certificate::{ABSOLUTE, Answer, Evidence, Output, Read, env_digest};
    use blaze_certify::request::{Ask, Built, Next, Request, Told};
    use blaze_certify::slot::{Placed, Premise, Slot};
    use blaze_certify::solo::{self, Solo, Verdict};
    use blaze_certify::testing::{GRANULE_NS, Shelf, the_base};
    use blaze_core::Stamp;
    use blaze_ids::{ContentId, Hasher};
    use blaze_record::Digest;
    use hegel::generators as gs;

    use super::super::{Of, PLANT, Plant, Saved, Seen, Step, Writing};

    const DIR: &str = "/w";
    const ENGINE: &str = "/blaze/engine";
    const LIBRARY: &str = "/blaze/lib/DIGEST";
    const HEADER: &str = "/usr/z.h";
    const PROGRAM: &str = "/w/target/app";
    const FILES: [&str; 3] = ["Cargo.toml", "src/lib.rs", "src/main.rs"];
    const FILE: u32 = blaze_core::S_IFREG | 0o644;
    const DIRECTORY: u32 = blaze_core::S_IFDIR | 0o755;
    const WAIT: Op = Op::Wait(3 * GRANULE_NS);

    fn stamp(ino: u64, clock_ns: u64, mode: u32) -> Stamp {
        let at = clock_ns / GRANULE_NS * GRANULE_NS;
        Stamp { ino, size: 32, mtime_ns: at, ctime_ns: at, mode }
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Op {
        /// A file of the checkout written in place: its inode and size kept.
        Edit(&'static str, u8),
        Header(u8),
        Wait(u64),
        /// The watcher looks: the column saved, the head moved to what it read.
        Watch,
        /// The head built and its certificate written.
        Build,
    }

    /// What the daemon's certificate was valued at, and its answer.
    #[derive(Clone)]
    struct Certified {
        reads: Vec<(Read, Digest)>,
        product: ContentId,
    }

    struct World {
        clock_ns: u64,
        files: BTreeMap<&'static str, (u8, Stamp)>,
        dirs: BTreeMap<&'static str, Stamp>,
        header: (u8, Stamp),
        output: Option<(ContentId, Stamp)>,
        /// The watcher's column (names relative to `DIR`, in name order) and the head's bytes.
        column: Vec<(String, Stamp)>,
        head: BTreeMap<&'static str, u8>,
        /// Whether a file or the header moved since the watcher's last look.
        unwatched: bool,
        certified: Option<Certified>,
    }

    impl World {
        fn new() -> World {
            let then = 10 * GRANULE_NS;
            let files = FILES.iter().zip(11..).map(|(name, ino)| (*name, (1, stamp(ino, then, FILE)))).collect();
            let mut world = World {
                clock_ns: 100 * GRANULE_NS,
                files,
                dirs: BTreeMap::from([(".", stamp(1, then, DIRECTORY)), ("src", stamp(2, then, DIRECTORY))]),
                header: (1, stamp(5, then, FILE)),
                output: None,
                column: Vec::new(),
                head: BTreeMap::new(),
                unwatched: false,
                certified: None,
            };
            world.apply(Op::Watch);
            world
        }

        fn apply(&mut self, op: Op) {
            self.clock_ns += 1;
            let now = self.clock_ns;
            match op {
                Op::Edit(name, byte) => {
                    let file = self.files.get_mut(name).expect("a file of the model");
                    *file = (byte, stamp(file.1.ino, now, FILE));
                    self.unwatched = true;
                }
                Op::Header(byte) => {
                    self.header = (byte, stamp(self.header.1.ino, now, FILE));
                    self.unwatched = true;
                }
                Op::Wait(ns) => self.clock_ns += ns,
                Op::Watch => {
                    let saved = |it: Stamp| if blaze_vfs::racy(&it, now) { Stamp::UNKNOWN } else { it };
                    let mut column: Vec<(String, Stamp)> = self.dirs.iter().map(|(name, it)| ((*name).to_owned(), saved(*it))).collect();
                    column.extend(self.files.iter().map(|(name, (_, it))| ((*name).to_owned(), saved(*it))));
                    // By name: a name is in the column once, a dir's or a file's.
                    column.sort_by(|a, b| a.0.cmp(&b.0));
                    self.column = column;
                    self.head = self.files.iter().map(|(name, (byte, _))| (*name, *byte)).collect();
                    self.unwatched = false;
                }
                Op::Build => {}
            }
        }

        fn root_of(files: impl Iterator<Item = (&'static str, u8)>) -> ContentId {
            let mut hasher = Hasher::new("test slot writer root");
            for (name, byte) in files {
                hasher.part(name.as_bytes()).part(&[byte]);
            }
            hasher.finish()
        }

        fn head_root(&self) -> ContentId {
            World::root_of(self.head.iter().map(|(name, byte)| (*name, *byte)))
        }

        fn root(&self) -> ContentId {
            World::root_of(self.files.iter().map(|(name, (byte, _))| (*name, *byte)))
        }

        fn env() -> Vec<(String, String)> {
            vec![("CC".to_owned(), "cc".to_owned())]
        }

        fn digest(&self, read: &Read) -> Digest {
            match read {
                Read::Env(name) => env_digest(&World::env(), name),
                Read::Outside(path) if path == HEADER => Digest::Is(ContentId::of("test header", &[self.header.0])),
                _ => Digest::Nothing,
            }
        }

        fn product(root: ContentId, header: u8) -> ContentId {
            Hasher::new("test slot writer product").id(&root).part(&[header]).finish()
        }

        fn answer(product: ContentId) -> Answer {
            Answer {
                outputs: vec![Output { base: ABSOLUTE, path: PROGRAM.to_owned(), content: product }],
                units: vec![product],
                said: product.0.to_vec(),
            }
        }

        /// The head built and certified, its program placed.
        fn build(&mut self, shelf: &Shelf) {
            let root = self.head_root();
            let product = World::product(root, self.header.0);
            self.clock_ns += 1;
            let placed = stamp(50, self.clock_ns, FILE | 0o111);
            self.output = Some((product, placed));
            let valued: Vec<(Read, Digest)> = [Read::Env("CC".to_owned()), Read::Outside(HEADER.to_owned())]
                .into_iter()
                .map(|read| {
                    let digest = self.digest(&read);
                    (read, digest)
                })
                .collect();
            let mut machine = Request::building(the_base(), Vec::new());
            let output = Output { base: ABSOLUTE, path: PROGRAM.to_owned(), content: product };
            let built = Built {
                nodes: vec![String::new()],
                valued: valued.clone(), // clone: the build's reads, kept as the certificate's
                outputs: vec![(output, Some((placed, self.clock_ns)))],
                units: vec![product],
                said: product.0.to_vec(),
                untraced: Vec::new(),
            };
            machine.built(built).expect("the machine asked for the build");
            let mut evidence = Evidence::default();
            for _ in 0..16 {
                match machine.step(shelf, &mut evidence).expect("the machine writes its record") {
                    Next::Asks(asks) => {
                        for ask in &asks {
                            let told = match ask {
                                Ask::Read { read: Read::Node(path), .. } if path.is_empty() => Told::Read(Digest::Is(root)),
                                other => panic!("the build's machine asked for {other:?}"),
                            };
                            machine.told(ask, told).expect("told what it asked");
                        }
                    }
                    Next::Written { .. } => {
                        let mut reads = vec![(Read::Node(String::new()), Digest::Is(root))];
                        reads.extend(valued);
                        self.certified = Some(Certified { reads, product });
                        return;
                    }
                    other => panic!("the build's machine said {other:?}"),
                }
            }
            panic!("the build's record was not written in 16 steps");
        }

        fn lstat(&self, path: &str) -> Stamp {
            let found = match path {
                ENGINE => Some(stamp(3, 10 * GRANULE_NS, FILE | 0o111)),
                LIBRARY => Some(stamp(4, 10 * GRANULE_NS, FILE)),
                DIR => self.dirs.get(".").copied(),
                HEADER => Some(self.header.1),
                PROGRAM => self.output.map(|it| it.1),
                _ => path.strip_prefix("/w/").and_then(|name| {
                    self.dirs.get(name).copied().or_else(|| self.files.get(name).map(|it| it.1))
                }),
            };
            found.unwrap_or(Stamp::GONE)
        }

        /// The client's check over the slot `bytes` with nothing moving, every look the
        /// model's.
        fn solo(&self, bytes: &[u8], key: &ContentId) -> Result<Verdict, String> {
            let mut check = match Solo::open(bytes, key, &World::env(), ENGINE, false) {
                Ok(check) => check,
                Err(why) => return Ok(Verdict::Daemon(why)),
            };
            let mut seen = None;
            for _ in 0..64 {
                match check.step(seen.take()) {
                    solo::Step::Look(solo::Look::Stamps(paths)) => {
                        seen = Some(solo::Seen::Stamps(paths.iter().map(|it| self.lstat(it)).collect()));
                    }
                    solo::Step::Look(solo::Look::Head { .. }) => seen = Some(solo::Seen::Head(None)),
                    solo::Step::Done(Verdict::Daemon(solo::Why::Stopped(what))) => return Err(format!("the client's check stopped: {what}")),
                    solo::Step::Done(verdict) => return Ok(verdict),
                }
            }
            Err("the client's check did not end in 64 steps".to_owned())
        }
    }

    fn targets() -> Targets {
        Targets {
            cargo: Some(Cargo {
                cwd: DIR.to_owned(),
                env: World::env(),
                args: vec!["--bin".to_owned(), "app".to_owned()],
                root: DIR.to_owned(),
                color: true,
                files: Vec::new(),
                width: Some(100),
                command: "build".to_owned(),
                refused: None,
                verbose: 0,
                prebuilt: true,
            }),
            target: None,
            profile: "dev".to_owned(),
            goal: None,
            features: Vec::new(),
            packages: Vec::new(),
            all_features: false,
            no_default_features: false,
        }
    }

    /// One case: the model moved by `before`, built where nothing built yet, then the slot of
    /// the last certificate written while the model moves by `during` (each op before the
    /// writer's step its number names). Whether a slot was written, or what the oracle says.
    fn run(before: &[Op], during: &[(u8, Op)], plant: Plant) -> Result<bool, String> {
        PLANT.set(plant);
        let ran = run_planted(before, during);
        PLANT.set(Plant::None);
        ran
    }

    fn run_planted(before: &[Op], during: &[(u8, Op)]) -> Result<bool, String> {
        let shelf = Shelf::new();
        let mut world = World::new();
        for op in before {
            match op {
                Op::Build => world.build(&shelf),
                op => world.apply(*op),
            }
        }
        if world.certified.is_none() {
            world.build(&shelf);
        }
        // clone: the certificate's reads, borrowed by the writer while the model moves
        let Some(certified) = world.certified.clone() else { unreachable!("built above") };
        let (Some((_, Digest::Is(read_root))), Some((content, placed))) = (certified.reads.first(), world.output) else {
            return Err("the certificate holds no root, or no program was placed".to_owned());
        };
        let read_root = *read_root;
        let targets = targets();
        let env = World::env();
        let key = blaze_certify::slot::key_of("build", &targets, DIR).expect("a cargo request has a key");
        let of = Of {
            word: "build",
            targets: &targets,
            root_dir: DIR,
            slots: "/state/slots",
            base: the_base(),
            traced: false,
            drawn: ContentId::of("test drawn", b""),
            env: &env,
            unasked: Vec::new(),
            reads: &certified.reads,
            request: b"test request".to_vec(),
        };
        let now = world.clock_ns;
        // clone: the writer's own copy of the column, as `Saved::of` makes one
        let column = Saved { dir: DIR.to_owned(), root: Some(world.head_root()), names: world.column.clone() };
        let head = world.head.clone(); // clone: what the column's root holds, looked up by the writer
        let content_of = |name: &str| head.get(name).map(|byte| ContentId::of(blaze_vfs::FILE_DOMAIN, &[*byte]));
        let premise = |path: &str| Premise { path: path.to_owned(), stamp: world.lstat(path), looked_ns: now };
        let placed = vec![Placed { base: ABSOLUTE, path: PROGRAM.to_owned(), stamp: placed, looked_ns: now, content }];
        let mut writing = match Writing::new(of, column, content_of, Vec::new(), premise(ENGINE), premise(LIBRARY), placed, None, now) {
            Ok(writing) => writing,
            Err(_) => return Ok(false),
        };
        let mut seen = None;
        let mut written = None;
        // Whether the model moved since the column the writer copied was saved: then a client
        // may not hit, and is right not to.
        let mut moved = world.unwatched;
        for at in 0..64u8 {
            // A build while the writer runs is of no matter to it: no build is made then.
            for (_, op) in during.iter().filter(|(when, op)| *when == at && *op != Op::Build) {
                moved |= matches!(op, Op::Edit(..) | Op::Header(_));
                world.apply(*op);
            }
            match writing.step(&shelf, seen.take()) {
                Step::Stamps(paths) => {
                    seen = Some(Seen::Stamps { stamps: paths.iter().map(|it| world.lstat(it)).collect(), asked_ns: world.clock_ns });
                }
                Step::Read(path) => {
                    let byte = path.strip_prefix("/w/").and_then(|name| world.files.get(name)).map(|it| vec![it.0]);
                    seen = Some(Seen::Read(byte.ok_or_else(|| format!("{path}: no file"))));
                }
                Step::Value(read) => seen = Some(Seen::Valued(world.digest(&read))),
                Step::Write { to, bytes } => {
                    if to != format!("/state/slots/{key:x}") {
                        return Err(format!("the slot was written at {to}"));
                    }
                    written = Some(bytes);
                    seen = Some(Seen::Written(Ok(())));
                }
                Step::Done(Err(_)) => return Ok(false),
                Step::Done(Ok(done)) => {
                    let Some(bytes) = written.take() else {
                        return Err(format!("the writer ended {done:?} with nothing written"));
                    };
                    let slot = Slot::decode(&bytes, &key).map_err(|why| format!("the slot written does not decode: {why}"))?;
                    if slot.column.root != read_root {
                        return Err(format!("the slot holds the root {:x}, not {read_root:x} the node read had", slot.column.root));
                    }
                    // Nothing moves from here: what the client says of the model as it is.
                    let fresh = World::product(world.root(), world.header.0);
                    match world.solo(&bytes, &key)? {
                        Verdict::Hit(answer) if answer != World::answer(fresh) || world.output.map(|it| it.0) != Some(fresh) => {
                            return Err(format!(
                                "a client hits on a written slot with {:x}, and a fresh build says {fresh:x}",
                                answer.units.first().copied().unwrap_or(ContentId([0; 32]))
                            ));
                        }
                        Verdict::Hit(_) => {}
                        Verdict::Daemon(why) if !moved => {
                            return Err(format!("a slot was written that a client with nothing moved since the watcher looked would not hit on: {why}"));
                        }
                        Verdict::Daemon(_) => {}
                    }
                    return Ok(true);
                }
            }
        }
        Err("the writer did not end in 64 steps".to_owned())
    }

    #[hegel::composite]
    fn op(tc: &hegel::TestCase) -> Op {
        let file = FILES[tc.draw(gs::integers::<u8>().max_value(2)) as usize];
        let byte = tc.draw(gs::integers::<u8>().min_value(1).max_value(3));
        match tc.draw(gs::integers::<u8>().max_value(7)) {
            0 | 1 => Op::Edit(file, byte),
            2 => Op::Header(byte),
            3 => Op::Wait(GRANULE_NS / 3),
            4 => WAIT,
            5 | 6 => Op::Watch,
            _ => Op::Build,
        }
    }

    #[hegel::composite]
    fn timed(tc: &hegel::TestCase) -> (u8, Op) {
        (tc.draw(gs::integers::<u8>().max_value(6)), tc.draw(op()))
    }

    /// With `plant`, the oracle says `says` of `before` and `during`; unplanted, they pass.
    fn caught(before: &[Op], during: &[(u8, Op)], plant: Plant, says: &str) {
        assert_eq!(run(before, during, Plant::None), Ok(false), "unplanted, {before:?} {during:?} writes no slot");
        let why = run(before, during, plant).expect_err("the oracle missed the planted bug");
        assert!(why.contains(says), "{why}");
    }

    static CONTROLS: Once = Once::new();

    /// A slot written after the racy window is hit on; each planted bug writes a slot a
    /// client hits on with other bytes than a fresh build's, and the oracle says so.
    fn controls() {
        assert_eq!(run(&[Op::Build, WAIT], &[], Plant::None), Ok(true), "a build then a wait writes a slot");
        // A name saved unknown, read; edited between the read and the look, then steady.
        let unknown = [Op::Edit("src/main.rs", 2), Op::Watch, Op::Build, WAIT];
        assert_eq!(run(&unknown, &[], Plant::None), Ok(true), "a name saved unknown is read and slotted");
        caught(&unknown, &[(1, Op::Edit("src/main.rs", 3)), (1, WAIT)], Plant::SlotBeforeLook, "a fresh build says");
        // A file saved under the build: the head moved, the certificate is of the old root.
        let moved = [Op::Build, WAIT, Op::Edit("src/lib.rs", 2), WAIT, Op::Watch];
        caught(&moved, &[], Plant::ColumnOfAnotherRoot, "a fresh build says");
    }

    /// The invariant: over any edits (in and out of their racy window), header changes,
    /// waits, watcher looks and builds, before the writer starts and between any two of its
    /// steps, `Writing` ends `Done(Ok)` only for a slot whose root is the one the
    /// certificate's node read had and on which a client's hit is what a fresh build of the
    /// model says; and where nothing moved since the watcher last looked, a client with
    /// nothing moved hits on it. Controls: `Plant::SlotBeforeLook` (the stamps taken once,
    /// after the reads), `Plant::ColumnOfAnotherRoot` (the certificate's root taken whatever
    /// root the column's stamps vouch for).
    #[hegel::test]
    fn a_slot_is_written_only_where_a_client_would_hit_on_it(tc: hegel::TestCase) {
        CONTROLS.call_once(controls);
        let before: Vec<Op> = tc.draw(gs::vecs(op()).max_size(12));
        let during: Vec<(u8, Op)> = tc.draw(gs::vecs(timed()).max_size(6));
        if let Err(why) = run(&before, &during, Plant::None) {
            panic!("{why}");
        }
    }
}
