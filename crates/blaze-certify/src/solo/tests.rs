//! A client's check against a model: a checkout of a few files in two directories, each with
//! its own stamp (an edit keeps the inode and the size, a rename the inode, an add or a
//! remove moves the directory's), three variables, one header outside, an engine that can be
//! replaced, and a build that is a pure function of what it reads, which leaves a program and
//! its launcher's start file. The clock ticks a nanosecond an operation, waits jump it, and
//! the file system's stamps are of whole seconds, so every racy window is two of them.
//!
//! The daemon drives the request machine with the world's own digests over a write-once
//! store, and after each answer writes the slot as the daemon will (B): its stamps as the
//! world has them now, its records from `rehearse`. The client opens the slot and answers its
//! looks with the world's `lstat`s.

use std::{collections::BTreeMap, sync::Once};

use blaze_api::{Cargo, Targets};
use blaze_ids::Hasher;
use hegel::generators as gs;

use super::*;
use crate::certificate::{LAUNCHER, Output, Shape};
use crate::request::Built;
use crate::slot::{Column, KEY_DROPS_FILES, Local, MAGIC, Outside, Placed, Premise, key_of};
use crate::testing::{GRANULE_NS, Shelf};

const DIR: &str = "/w";
const ENGINE: &str = "/blaze/engine";
const LIBRARY: &str = "/blaze/lib/DIGEST";
const HEADER: &str = "/usr/z.h";
const PROGRAM: &str = "/w/target/app";
const FILES: [&str; 5] = ["Cargo.toml", "README", "src/lib.rs", "src/main.rs", "src/new.rs"];
const VARS: [&str; 3] = ["CC", "BLAZE_OPT", "SHLVL"];
const KEY: ContentId = ContentId([7; 32]);

/// A stamp of the model's file system: whole seconds, size 32 whatever the bytes.
fn stamp(ino: u64, clock_ns: u64, mode: u32) -> Stamp {
    let at = clock_ns / GRANULE_NS * GRANULE_NS;
    Stamp { ino, size: 32, mtime_ns: at, ctime_ns: at, mode }
}

const FILE: u32 = blaze_core::S_IFREG | 0o644;
const DIRECTORY: u32 = blaze_core::S_IFDIR | 0o755;

#[derive(Clone, Copy, Debug)]
enum Op {
    /// A file of the checkout written (an edit keeps its inode and size) or removed.
    File(&'static str, Option<u8>),
    /// A file renamed over no other: its inode goes with it.
    Rename(&'static str, &'static str),
    Env(&'static str, Option<u8>),
    Header(Option<u8>),
    /// The program changed by hand, or removed with its start file.
    Output(Option<u8>),
    /// The engine replaced by the version given: its stamp and the request's base move.
    Tool(u8),
    Wait(u64),
    /// The daemon stops: what it knew beside its store goes.
    Restart,
    Request,
}

#[derive(Clone, Debug)]
struct World {
    clock_ns: u64,
    inos: u64,
    /// By name from the checkout's directory: the byte and the stamp.
    files: BTreeMap<&'static str, (u8, Stamp)>,
    /// `.` and `src`.
    dirs: BTreeMap<&'static str, Stamp>,
    env: BTreeMap<&'static str, u8>,
    header: Option<(u8, Stamp)>,
    tool: u8,
    engine: Stamp,
    library: Stamp,
    output: Option<(ContentId, Stamp)>,
    /// The record the program's start file names.
    start: Option<ContentId>,
}

impl World {
    fn new() -> World {
        let clock_ns = 10 * GRANULE_NS;
        let mut world = World {
            clock_ns,
            inos: 100,
            files: BTreeMap::new(),
            dirs: BTreeMap::from([(".", stamp(1, clock_ns, DIRECTORY)), ("src", stamp(2, clock_ns, DIRECTORY))]),
            env: BTreeMap::from([("CC", 1), ("SHLVL", 1)]),
            header: None,
            tool: 1,
            engine: stamp(3, clock_ns, FILE | 0o111),
            library: stamp(4, clock_ns, FILE),
            output: None,
            start: None,
        };
        for name in ["Cargo.toml", "README", "src/lib.rs", "src/main.rs"] {
            let ino = world.ino();
            world.files.insert(name, (1, stamp(ino, clock_ns, FILE)));
        }
        world
    }

    fn ino(&mut self) -> u64 {
        self.inos += 1;
        self.inos
    }

    fn dir_of(name: &str) -> &'static str {
        if name.contains('/') { "src" } else { "." }
    }

    fn moved_dir(&mut self, name: &str) {
        let dir = self.dirs.get_mut(World::dir_of(name)).expect("the model has both directories");
        *dir = stamp(dir.ino, self.clock_ns, DIRECTORY);
    }

    fn apply(&mut self, op: Op) {
        self.clock_ns += 1;
        let now = self.clock_ns;
        match op {
            Op::File(name, Some(byte)) => match self.files.get_mut(name) {
                Some(file) => *file = (byte, stamp(file.1.ino, now, FILE)),
                None => {
                    let ino = self.ino();
                    self.files.insert(name, (byte, stamp(ino, now, FILE)));
                    self.moved_dir(name);
                }
            },
            Op::File(name, None) => {
                if self.files.remove(name).is_some() {
                    self.moved_dir(name);
                }
            }
            Op::Rename(from, to) => {
                if !self.files.contains_key(to)
                    && let Some((byte, was)) = self.files.remove(from)
                {
                    self.files.insert(to, (byte, stamp(was.ino, now, FILE)));
                    self.moved_dir(from);
                    self.moved_dir(to);
                }
            }
            Op::Env(name, Some(byte)) => drop(self.env.insert(name, byte)),
            Op::Env(name, None) => drop(self.env.remove(name)),
            Op::Header(Some(byte)) => {
                let ino = match self.header {
                    Some((_, was)) => was.ino,
                    None => self.ino(),
                };
                self.header = Some((byte, stamp(ino, now, FILE)));
            }
            Op::Header(None) => self.header = None,
            Op::Output(Some(byte)) => {
                let ino = match self.output {
                    Some((_, was)) => was.ino,
                    None => self.ino(),
                };
                self.output = Some((ContentId::of("test hand edit", &[byte]), stamp(ino, now, FILE | 0o111)));
            }
            Op::Output(None) => {
                self.output = None;
                self.start = None;
            }
            Op::Tool(version) => {
                if version != self.tool {
                    self.tool = version;
                    let ino = self.ino();
                    self.engine = stamp(ino, now, FILE | 0o111);
                }
            }
            Op::Wait(ns) => self.clock_ns += ns,
            Op::Restart | Op::Request => {}
        }
    }

    /// What `lstat` says of `path`.
    fn lstat(&self, path: &str) -> Stamp {
        let found = match path {
            ENGINE => Some(self.engine),
            LIBRARY => Some(self.library),
            DIR => self.dirs.get(".").copied(),
            HEADER => self.header.map(|it| it.1),
            PROGRAM => self.output.map(|it| it.1),
            _ => path.strip_prefix("/w/").and_then(|name| {
                self.dirs.get(name).copied().or_else(|| self.files.get(name).map(|it| it.1))
            }),
        };
        found.unwrap_or(Stamp::GONE)
    }

    /// The first `len` bytes of the file at `path`.
    fn head(&self, path: &str, len: usize) -> Option<Vec<u8>> {
        if path != start_file(PROGRAM) {
            return None;
        }
        let record = self.start?;
        let mut head = START_MAGIC.to_vec();
        head.resize(72, 0);
        head.extend_from_slice(&record.0);
        head.truncate(len);
        Some(head)
    }

    fn env_pairs(&self) -> Vec<(String, String)> {
        self.env.iter().map(|(name, byte)| ((*name).to_owned(), byte.to_string())).collect()
    }

    /// The variables a build reads unasked that are set.
    fn unasked(&self) -> Vec<String> {
        self.env.keys().filter(|name| name.starts_with("BLAZE_")).map(|name| (*name).to_owned()).collect()
    }

    fn base(&self) -> ContentId {
        let id = |n: u8| ContentId::of("test request shape", &[n]);
        certificate::base(&Shape { plan: id(0), config: id(1), request: id(2) }, &[self.tool])
    }

    fn root(&self) -> ContentId {
        let mut hasher = Hasher::new("test root");
        for (name, (byte, _)) in &self.files {
            hasher.part(name.as_bytes()).part(&[*byte]);
        }
        hasher.finish()
    }

    fn digest(&self, read: &Read) -> Digest {
        match read {
            Read::Node(path) if path.is_empty() => Digest::Is(self.root()),
            Read::Node(path) => match self.files.get(path.as_str()) {
                Some((byte, _)) => Digest::Is(ContentId::of("test file", &[*byte])),
                None => Digest::Nothing,
            },
            Read::Env(name) => env_digest(&self.env_pairs(), name),
            Read::Outside(path) if path == HEADER => {
                self.header.map_or(Digest::Nothing, |(byte, _)| Digest::Is(ContentId::of("test header", &[byte])))
            }
            _ => Digest::Nothing,
        }
    }

    /// What a build reads: the checkout whole, a variable a step asks for, one read
    /// unasked, the header.
    fn reads() -> [Read; 4] {
        [
            Read::Node(String::new()),
            Read::Env("CC".to_owned()),
            Read::Env("BLAZE_OPT".to_owned()),
            Read::Outside(HEADER.to_owned()),
        ]
    }

    fn product(&self) -> ContentId {
        let mut hasher = Hasher::new("test product");
        for read in World::reads() {
            match self.digest(&read) {
                Digest::Nothing => hasher.part(b"-"),
                Digest::Is(id) => hasher.id(&id),
            };
        }
        hasher.part(&[self.tool]).finish()
    }

    fn launcher(product: ContentId) -> ContentId {
        ContentId::of("test launcher record", &product.0)
    }

    fn answer(product: ContentId) -> Answer {
        Answer {
            outputs: vec![
                Output { base: ABSOLUTE, path: PROGRAM.to_owned(), content: product },
                Output { base: LAUNCHER, path: PROGRAM.to_owned(), content: World::launcher(product) },
            ],
            units: vec![product],
            said: product.0.to_vec(),
        }
    }

    /// The oracle: what a fresh build of the world as it is says.
    fn fresh(&self) -> Answer {
        World::answer(self.product())
    }

    /// A build: it places the program and its start file.
    fn build(&mut self) -> Built {
        let product = self.product();
        self.clock_ns += 1;
        let ino = match self.output {
            Some((_, was)) => was.ino,
            None => self.ino(),
        };
        let placed = stamp(ino, self.clock_ns, FILE | 0o111);
        self.output = Some((product, placed));
        self.start = Some(World::launcher(product));
        let valued = [Read::Env("CC".to_owned()), Read::Outside(HEADER.to_owned())]
            .into_iter()
            .map(|read| {
                let digest = self.digest(&read);
                (read, digest)
            })
            .collect();
        let answer = World::answer(product);
        let mut outputs = answer.outputs.into_iter();
        let file = outputs.next().expect("the program's file");
        let code = outputs.next().expect("the launcher's code");
        Built {
            nodes: vec![String::new()],
            valued,
            outputs: vec![(file, Some((placed, self.clock_ns))), (code, None)],
            units: answer.units,
            said: answer.said,
            untraced: Vec::new(),
        }
    }

    /// The daemon's look.
    fn look(&self, ask: &Ask) -> Told {
        match ask {
            Ask::Read { read, .. } => Told::Read(self.digest(read)),
            Ask::Stamp { .. } => Told::Stamp { stamp: self.output.map(|it| it.1), looked_ns: self.clock_ns },
            Ask::Content { base: LAUNCHER, .. } => Told::Content(self.start),
            Ask::Content { .. } => Told::Content(self.output.map(|it| it.0)),
        }
    }
}

/// One request the daemon answers: a hit, or the answer of the build its miss ran.
fn answered(world: &mut World, shelf: &Shelf, evidence: &mut Evidence) -> Result<(bool, Answer), String> {
    let mut machine = Request::new(world.base(), false, world.unasked());
    let mut built = None;
    for _ in 0..64 {
        match machine.step(shelf, evidence).map_err(|why| why.to_string())? {
            Next::Asks(asks) => {
                for ask in &asks {
                    machine.told(ask, world.look(ask)).map_err(|why| why.to_string())?;
                }
            }
            Next::Waiting { on, left } => return Err(format!("{on}: waits for {left} with everything told")),
            Next::Hit(answer) => return Ok((true, answer)),
            Next::Miss(_) => {
                let build = world.build();
                built = Some(World::answer(build.units[0]));
                machine.built(build).map_err(|why| why.to_string())?;
            }
            Next::Written { .. } => return Ok((false, built.ok_or("a record was written with no build")?)),
            Next::Unwritten(why) => return Err(format!("a traced build left no record: {why}")),
        }
    }
    Err("the daemon's request did not end in 64 steps".to_owned())
}

/// A bug planted in the model's slot writer, for a control.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Writer {
    Faithful,
    /// A read left out of the slot: `src/main.rs` is not among the column's names.
    LeavesANameOut,
    /// The answer record's bytes swapped for another answer's (other units and words).
    SwapsAnAnswer,
}

/// The slot the daemon writes after an answer, or none where a client would not hit on it.
fn write_slot(world: &World, shelf: &Shelf, writer: Writer) -> Option<Slot> {
    let env = world.env_pairs();
    let looked_ns = world.clock_ns;
    let mut names: Vec<(String, Stamp)> = world.dirs.iter().map(|(name, stamp)| ((*name).to_owned(), *stamp)).collect();
    for (name, (_, stamp)) in &world.files {
        if !(writer == Writer::LeavesANameOut && *name == "src/main.rs") {
            names.push(((*name).to_owned(), *stamp));
        }
    }
    names.sort_by(|a, b| a.0.cmp(&b.0));
    let header = Read::Outside(HEADER.to_owned());
    let mut slot = Slot {
        key: KEY,
        written_ns: looked_ns,
        base: world.base(),
        traced: false,
        drawn: ContentId::of("test drawn", b""),
        names: names_of(&env),
        unasked: world.unasked(),
        premises: vec![
            Premise { path: ENGINE.to_owned(), stamp: world.engine, looked_ns },
            Premise { path: LIBRARY.to_owned(), stamp: world.library, looked_ns },
        ],
        column: Column { dir: DIR.to_owned(), root: world.root(), looked_ns, names },
        locals: Vec::new(),
        outside: vec![Outside {
            digest: world.digest(&header),
            read: header,
            stamp: world.lstat(HEADER),
            looked_ns,
        }],
        placed: world
            .output
            .map(|(content, stamp)| Placed { base: ABSOLUTE, path: PROGRAM.to_owned(), stamp, looked_ns, content })
            .into_iter()
            .collect(),
        records: Vec::new(),
        request: b"test request".to_vec(),
    };
    slot.records = rehearse(&slot, shelf, &env).ok()?;
    if writer == Writer::SwapsAnAnswer {
        for (_, held) in &mut slot.records {
            let Some(bytes) = held else { continue };
            if let Ok(mut answer) = Answer::decode(bytes) {
                let other = ContentId::of("test other answer", &answer.said);
                answer.units = vec![other];
                answer.said = other.0.to_vec();
                *bytes = answer.encode();
            }
        }
    }
    Some(slot)
}

/// The client's check of the world over the slot `bytes`, every look answered by the world.
fn solo(world: &World, bytes: &[u8]) -> Result<Verdict, String> {
    let env = world.env_pairs();
    let mut check = match Solo::open(bytes, &KEY, &env, ENGINE, false) {
        Ok(check) => check,
        Err(why) => return Ok(Verdict::Daemon(why)),
    };
    let mut seen = None;
    for _ in 0..64 {
        match check.step(seen.take()) {
            Step::Look(Look::Stamps(paths)) => {
                seen = Some(Seen::Stamps(paths.iter().map(|path| world.lstat(path)).collect()));
            }
            Step::Look(Look::Head { path, len }) => seen = Some(Seen::Head(world.head(&path, len))),
            Step::Done(Verdict::Daemon(Why::Stopped(what))) => return Err(format!("the client's check stopped: {what}")),
            Step::Done(verdict) => return Ok(verdict),
        }
    }
    Err("the client's check did not end in 64 steps".to_owned())
}

/// Which claim a run checks at a solo hit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Oracle {
    /// The hit is what a fresh build of the world says, and the outputs are its.
    Fresh,
    /// The machine driven with the world's own digests says the same; and a slot rehearsed
    /// to a hit is hit on while nothing moves.
    Daemon,
}

/// Runs `ops` with `plant` in the client and `writer` writing the slots, and checks
/// `oracle` at every request. How many requests the client answered alone, or what the
/// oracle says.
fn run(ops: &[Op], plant: Plant, writer: Writer, oracle: Oracle) -> Result<usize, String> {
    PLANT.set(plant);
    let ran = run_planted(ops, writer, oracle);
    PLANT.set(Plant::None);
    ran
}

fn run_planted(ops: &[Op], writer: Writer, oracle: Oracle) -> Result<usize, String> {
    let mut world = World::new();
    let shelf = Shelf::new();
    let mut evidence = Evidence::default();
    let mut slot: Option<Vec<u8>> = None;
    let mut alone = 0;
    for (n, op) in ops.iter().enumerate() {
        if matches!(op, Op::Restart) {
            evidence = Evidence::default();
        }
        if !matches!(op, Op::Request) {
            world.apply(*op);
            continue;
        }
        world.apply(*op);
        if let Some(bytes) = &slot
            && let Verdict::Hit(answer) = solo(&world, bytes).map_err(|why| format!("step {n}: {why}"))?
        {
            match oracle {
                Oracle::Fresh => {
                    let fresh = world.fresh();
                    if answer != fresh {
                        return Err(format!("step {n}: a solo hit says other than a fresh build would"));
                    }
                    if world.output.map(|it| it.0) != Some(fresh.outputs[0].content)
                        || world.start != Some(fresh.outputs[1].content)
                    {
                        return Err(format!("step {n}: a solo hit with outputs that are not the build's"));
                    }
                }
                Oracle::Daemon => {
                    let (mut scratch, mut known) = (world.clone(), evidence.clone());
                    match answered(&mut scratch, &shelf, &mut known) {
                        Ok((true, said)) if said == answer => {}
                        other => {
                            return Err(format!("step {n}: a solo hit where the daemon's machine says {other:?}"));
                        }
                    }
                }
            }
            alone += 1;
            continue;
        }
        answered(&mut world, &shelf, &mut evidence).map_err(|why| format!("step {n}: {why}"))?;
        if let Some(written) = write_slot(&world, &shelf, writer) {
            let bytes = written.encode();
            if oracle == Oracle::Daemon {
                match solo(&world, &bytes).map_err(|why| format!("step {n}: {why}"))? {
                    Verdict::Hit(_) => {}
                    Verdict::Daemon(why) => {
                        return Err(format!("step {n}: a slot rehearsed to a hit is not hit on, nothing moved: {why}"));
                    }
                }
            }
            slot = Some(bytes);
        }
    }
    Ok(alone)
}

#[hegel::composite]
fn op(tc: &hegel::TestCase) -> Op {
    let which = tc.draw(gs::integers::<u8>().max_value(4)) as usize;
    let byte = tc.draw(gs::integers::<u8>().max_value(2));
    let value = (byte > 0).then_some(byte);
    match tc.draw(gs::integers::<u8>().max_value(13)) {
        0 | 1 => Op::File(FILES[which], value),
        2 => Op::Rename(FILES[which], FILES[(which + 1) % FILES.len()]),
        3 => Op::Env(VARS[which % VARS.len()], value),
        4 => Op::Header(value),
        5 => Op::Output(value),
        6 => Op::Tool(byte),
        7 => Op::Wait(GRANULE_NS / 3),
        8 => Op::Wait(3 * GRANULE_NS),
        9 => Op::Restart,
        _ => Op::Request,
    }
}

const WAIT: Op = Op::Wait(3 * GRANULE_NS);

/// A build, then a hit after the racy window: the daemon writes a slot whose stamps are all
/// steady.
const SLOTTED: [Op; 3] = [Op::Request, WAIT, Op::Request];

/// `ops` pass and are answered alone at least once as the check is, and with `plant` and
/// `writer` `oracle` says `says`.
fn caught(ops: &[Op], plant: Plant, writer: Writer, oracle: Oracle, says: &str) {
    run(ops, Plant::None, Writer::Faithful, oracle).unwrap();
    let why = run(ops, plant, writer, oracle).expect_err("the oracle missed the planted bug");
    assert!(why.contains(says), "{why}");
}

fn with(then: &[Op]) -> Vec<Op> {
    SLOTTED.iter().chain(then).copied().collect()
}

static FRESH_CONTROLS: Once = Once::new();

/// Each planted bug of the client and of the slot's writer gives a hit on other bytes, and
/// the fresh build's oracle says so.
fn fresh_controls() {
    // Unplanted, a slot written after the racy window is answered alone.
    let alone = run(&with(&[Op::Request]), Plant::None, Writer::Faithful, Oracle::Fresh).unwrap();
    assert_eq!(alone, 1, "a request with nothing moved since a slot was written is answered alone");
    let fresh = "a solo hit says other than a fresh build would";
    // A slot written inside the edit's tick, then an edit in the same tick: the same stamp.
    let racy = [
        Op::Request,
        WAIT,
        Op::File("src/lib.rs", Some(2)),
        Op::Request,
        Op::File("src/lib.rs", Some(1)),
        Op::Request,
    ];
    caught(&racy, Plant::TrustsRacy, Writer::Faithful, Oracle::Fresh, fresh);
    caught(&with(&[Op::Env("BLAZE_OPT", Some(1)), Op::Request]), Plant::SkipsNames, Writer::Faithful, Oracle::Fresh, fresh);
    caught(&with(&[Op::Tool(2), Op::Request]), Plant::SkipsPremises, Writer::Faithful, Oracle::Fresh, fresh);
    caught(&with(&[WAIT, Op::File("src/new.rs", Some(1)), Op::Request]), Plant::SkipsDirectories, Writer::Faithful, Oracle::Fresh, fresh);
    caught(&with(&[Op::File("src/main.rs", Some(2)), Op::Request]), Plant::None, Writer::LeavesANameOut, Oracle::Fresh, fresh);
}

/// The invariant: over any builds, edits (in and out of the saved stamp's tick), renames,
/// added and removed files, variables, a replaced engine, outputs changed or removed by hand
/// and restarts, `Verdict::Hit(a)` is given only where a fresh build of the model's files
/// says `a` and leaves its outputs. Controls: `Plant::TrustsRacy`, `Plant::SkipsNames`,
/// `Plant::SkipsPremises`, `Plant::SkipsDirectories` (a file added is missed), and a read left
/// out of the slot (`Writer::LeavesANameOut`).
#[hegel::test]
fn a_solo_hit_is_what_a_fresh_build_says(tc: hegel::TestCase) {
    FRESH_CONTROLS.call_once(fresh_controls);
    let ops: Vec<Op> = tc.draw(gs::vecs(op()).max_size(40));
    if let Err(why) = run(&ops, Plant::None, Writer::Faithful, Oracle::Fresh) {
        panic!("{why}");
    }
}

static DAEMON_CONTROLS: Once = Once::new();

/// One record's bytes swapped for another answer's: the client hits on it, and the daemon's
/// machine says otherwise.
fn daemon_controls() {
    let alone = run(&with(&[Op::Request]), Plant::None, Writer::Faithful, Oracle::Daemon).unwrap();
    assert_eq!(alone, 1, "a request with nothing moved since a slot was written is answered alone");
    caught(
        &with(&[Op::Request]),
        Plant::None,
        Writer::SwapsAnAnswer,
        Oracle::Daemon,
        "a solo hit where the daemon's machine says",
    );
}

/// The refinement: for any state, where `Solo` answers `Hit(a)` the machine driven with the
/// world's own digests answers `Hit(a)`; and after `rehearse` gave `Ok`, with nothing moved,
/// `Solo` hits. Control: one record's bytes swapped for another answer's.
#[hegel::test]
fn the_client_and_the_daemon_decide_the_same(tc: hegel::TestCase) {
    DAEMON_CONTROLS.call_once(daemon_controls);
    let ops: Vec<Op> = tc.draw(gs::vecs(op()).max_size(40));
    if let Err(why) = run(&ops, Plant::None, Writer::Faithful, Oracle::Daemon) {
        panic!("{why}");
    }
}

/// A slot with something in every list.
fn full_slot() -> Slot {
    let mut world = World::new();
    world.apply(Op::Header(Some(1)));
    let shelf = Shelf::new();
    let mut evidence = Evidence::default();
    answered(&mut world, &shelf, &mut evidence).unwrap();
    let mut slot = write_slot(&world, &shelf, Writer::Faithful).unwrap_or_else(|| {
        world.apply(WAIT);
        answered(&mut world, &shelf, &mut evidence).unwrap();
        write_slot(&world, &shelf, Writer::Faithful).expect("a slot after the racy window")
    });
    slot.unasked = vec!["BLAZE_OPT".to_owned()];
    slot.locals = vec![Local {
        at: "/dep".to_owned(),
        looked_ns: 5,
        names: vec![(".".to_owned(), stamp(9, 0, DIRECTORY)), ("lib.rs".to_owned(), stamp(10, 0, FILE))],
    }];
    slot.outside.push(Outside {
        read: Read::Listing("/etc".to_owned()),
        digest: Digest::Nothing,
        stamp: Stamp::GONE,
        looked_ns: 6,
    });
    slot.records.push((ContentId([9; 32]), None));
    slot
}

/// The check word of `bytes` made again, as a writer of other bytes would.
fn sealed(mut bytes: Vec<u8>) -> Vec<u8> {
    let check = ContentId::of(crate::slot::CHECK, &bytes[40..]);
    bytes[8..40].copy_from_slice(&check.0);
    bytes
}

/// The invariant (rule 26): a slot decodes to the slot encoded, and encodes to the same bytes
/// again; a slot of another layout is refused by its word, a slot cut anywhere inside a list
/// is refused at a byte inside it, a flipped byte is torn, another request's slot names both
/// keys, and no count or path of a slot leads a reader past its bytes or out of what it names.
#[test]
fn a_slot_round_trips_and_another_layout_is_refused_by_name() {
    let slot = full_slot();
    assert!(!slot.records.is_empty() && !slot.placed.is_empty(), "the slot has records and outputs");
    let bytes = slot.encode();
    let read = Slot::decode(&bytes, &KEY).expect("a slot reads back");
    assert_eq!(read, slot);
    assert_eq!(read.encode(), bytes, "one encoding a value");

    let mut old = bytes.clone();
    old[..8].copy_from_slice(b"bzslot00");
    let refused = Slot::decode(&old, &KEY).expect_err("a slot of another layout is refused");
    assert_eq!(refused, Undecoded::Layout { found: *b"bzslot00" });
    assert!(refused.to_string().contains("bzslot00") && refused.to_string().contains("bzslot01"), "{refused}");
    assert_eq!(&bytes[..8], MAGIC);

    for len in 40..bytes.len() {
        match Slot::decode(&sealed(bytes[..len].to_vec()), &KEY) {
            Err(Undecoded::Cut { at }) => assert!(at <= len, "cut at {len}, refused past it at {at}"),
            other => panic!("a slot cut at {len} of {} is {other:?}", bytes.len()),
        }
    }
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(matches!(Slot::decode(&sealed(longer), &KEY), Err(Undecoded::Cut { .. })), "bytes left over");

    for at in [8, 40, 100, bytes.len() / 2, bytes.len() - 1] {
        let mut flipped = bytes.clone();
        flipped[at] ^= 1;
        assert_eq!(Slot::decode(&flipped, &KEY), Err(Undecoded::Torn), "a byte flipped at {at}");
    }

    let other = ContentId([8; 32]);
    let refused = Slot::decode(&bytes, &other).expect_err("another request's slot");
    assert_eq!(refused, Undecoded::Key { wanted: other, found: KEY });
    let said = refused.to_string();
    assert!(said.contains(&format!("{other:x}")) && said.contains(&format!("{KEY:x}")), "{said}");

    // The request's length past every byte there is: refused where it is, nothing reserved.
    let at = bytes.len() - slot.request.len() - 4;
    let mut huge = bytes.clone();
    huge[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(Slot::decode(&sealed(huge), &KEY), Err(Undecoded::Cut { at }));

    // A path that climbs out of what it names, or is not whole, is refused.
    for (dir, name) in [("/w/../etc", "."), ("w", "."), ("/w", "../x"), ("/w", "src//lib.rs"), ("/w", "./a"), ("/w\0", ".")] {
        let mut bad = slot.clone();
        bad.column.dir = dir.to_owned();
        bad.column.names = vec![(name.to_owned(), Stamp::GONE)];
        assert!(
            matches!(Slot::decode(&bad.encode(), &KEY), Err(Undecoded::Cut { .. })),
            "the column {dir:?} with the name {name:?} is read"
        );
    }
    let mut bad = slot.clone();
    bad.placed[0].base = LAUNCHER;
    assert!(matches!(Slot::decode(&bad.encode(), &KEY), Err(Undecoded::Cut { .. })), "a placed output of no file");
}

fn targets(files: Vec<(String, Vec<u8>)>) -> Targets {
    Targets {
        cargo: Some(Cargo {
            cwd: "/w".to_owned(),
            env: vec![("CC".to_owned(), "cc".to_owned())],
            args: vec!["--bin".to_owned(), "app".to_owned()],
            root: "/w".to_owned(),
            color: true,
            files,
            width: Some(100),
            command: "run".to_owned(),
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

/// The invariant: two requests that say anything different but their environment's values
/// (which the certificate reads by name, and whose names are `names_of`) have two keys, the
/// config files outside the checkout cargo reads among it; one with no cargo command line has
/// none. Control: `key_of` leaving out `cargo.files` gives two config files one key.
#[test]
fn two_requests_that_differ_have_keys_that_differ() {
    let config = |byte: u8| vec![("/home/u/.cargo/config.toml".to_owned(), vec![byte])];
    let one = targets(config(1));
    let key = |targets: &Targets| key_of("build", targets, "/w").expect("a cargo request has a key");
    let mut other_env = one.clone();
    if let Some(cargo) = &mut other_env.cargo {
        cargo.env = vec![("CC".to_owned(), "clang".to_owned()), ("SHLVL".to_owned(), "2".to_owned())];
    }
    assert_eq!(key(&one), key(&other_env), "the environment's values are read by name, not keyed");
    assert_ne!(names_of(&one.cargo.as_ref().unwrap().env), names_of(&other_env.cargo.as_ref().unwrap().env));
    assert_eq!(
        names_of(&[("A".to_owned(), "1".to_owned()), ("B".to_owned(), "2".to_owned())]),
        names_of(&[("B".to_owned(), "3".to_owned()), ("A".to_owned(), "4".to_owned()), ("A".to_owned(), "5".to_owned())]),
        "the names set, whatever their order and values",
    );

    let mut differing = vec![targets(config(2)), targets(Vec::new())];
    let mut args = one.clone();
    if let Some(cargo) = &mut args.cargo {
        cargo.args.push("--release".to_owned());
    }
    differing.push(args);
    let mut profile = one.clone();
    profile.profile = "release".to_owned();
    differing.push(profile);
    let mut keys = vec![key(&one)];
    keys.extend(differing.iter().map(key));
    keys.push(key_of("check", &one, "/w").unwrap());
    keys.push(key_of("build", &one, "/v").unwrap());
    let mut distinct = keys.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), keys.len(), "every request that differs has its own key");

    let mut agent = one.clone();
    agent.cargo = None;
    assert_eq!(key_of("build", &agent, "/w"), None, "an agent's request has no slot");

    KEY_DROPS_FILES.set(true);
    let dropped = key(&targets(config(1))) == key(&targets(config(2)));
    KEY_DROPS_FILES.set(false);
    assert!(dropped, "the control: a key that leaves out the config files gives two of them one key");
}
