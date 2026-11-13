//! The request machine against a model: a tiny world (a checkout's files in two package
//! directories and at its root, three variables, two files outside, one output), a build that
//! is a pure function of what it reads of that world, and a driver that answers the
//! machine's asks from the world in a seeded order. The store is a write-once map.
//!
//! The invariant: a hit is given only when a fresh build would say the same bytes and leave
//! the same outputs; and a request with nothing it reads changed since a recorded build
//! hits.

use std::collections::BTreeMap;

use blaze_core::sim::{Faults, Sim};
use blaze_ids::Hasher;
use hegel::generators as gs;

use super::*;
use crate::testing::{GRANULE_NS, Shelf, stamp_at, the_base};

const FILES: [&str; 6] = ["a/lib.rs", "a/build.rs", "b/lib.rs", "Cargo.toml", "Cargo.lock", "README"];
const VARS: [&str; 3] = ["CC", "BLAZE_OPT", "SHLVL"];
const OUTSIDE: [&str; 2] = ["/usr/z.h", "/tmp/x"];
const OUTPUT: &str = "debug/app";

#[derive(Clone, Copy, Debug)]
enum Op {
    /// A file of the checkout written (or deleted).
    File(&'static str, Option<u8>),
    Env(&'static str, Option<u8>),
    Outside(&'static str, Option<u8>),
    /// The output changed by hand (or deleted).
    Output(Option<u8>),
    Wait(u64),
    /// The daemon restarts: the store stays, what it knew beside it goes.
    Restart,
    /// A request; `mid`: a file of the checkout someone saves while its build runs.
    Request { mid: Option<(&'static str, u8)> },
}

#[derive(Clone, Debug)]
struct World {
    files: BTreeMap<&'static str, u8>,
    env: BTreeMap<&'static str, u8>,
    outside: BTreeMap<&'static str, u8>,
    output: Option<(ContentId, Stamp)>,
    clock_ns: u64,
    /// The build script of `a` runs outside the tracer.
    untraced: bool,
}

type Relevant = (BTreeMap<&'static str, u8>, BTreeMap<&'static str, u8>, Option<u8>, Option<ContentId>);

impl World {
    fn new() -> World {
        World {
            files: BTreeMap::from([("a/lib.rs", 1), ("b/lib.rs", 1), ("Cargo.toml", 1), ("README", 1)]),
            env: BTreeMap::from([("CC", 1), ("SHLVL", 1)]),
            outside: BTreeMap::new(),
            output: None,
            clock_ns: 10 * GRANULE_NS,
            untraced: false,
        }
    }

    fn stamp(&self) -> Stamp {
        stamp_at(self.clock_ns)
    }

    fn apply(&mut self, op: Op) {
        self.clock_ns += 1;
        match op {
            Op::File(path, Some(byte)) => drop(self.files.insert(path, byte)),
            Op::File(path, None) => drop(self.files.remove(path)),
            Op::Env(name, Some(byte)) => drop(self.env.insert(name, byte)),
            Op::Env(name, None) => drop(self.env.remove(name)),
            Op::Outside(path, Some(byte)) => drop(self.outside.insert(path, byte)),
            Op::Outside(path, None) => drop(self.outside.remove(path)),
            Op::Output(Some(byte)) => {
                self.output = Some((ContentId::of("test hand edit", &[byte]), self.stamp()));
            }
            Op::Output(None) => self.output = None,
            Op::Wait(ns) => self.clock_ns += ns,
            Op::Restart | Op::Request { .. } => {}
        }
    }

    fn node(&self, path: &str) -> Digest {
        if let Some(byte) = self.files.get(path) {
            return Digest::Is(ContentId::of("test file", &[*byte]));
        }
        let below = format!("{path}/");
        let mut hasher = Hasher::new("test directory");
        let mut any = false;
        for (name, byte) in &self.files {
            if name.starts_with(&below) {
                hasher.part(name.as_bytes()).part(&[*byte]);
                any = true;
            }
        }
        if any { Digest::Is(hasher.finish()) } else { Digest::Nothing }
    }

    fn digest(&self, read: &Read) -> Digest {
        let value = |byte: Option<&u8>| {
            byte.map_or(Digest::Nothing, |byte| Digest::Is(ContentId::of("test value", &[*byte])))
        };
        match read {
            Read::Node(path) => self.node(path),
            Read::Env(name) => value(self.env.get(name.as_str())),
            Read::Outside(path)
            | Read::Entry(path)
            | Read::Listing(path)
            | Read::Tree(path)
            | Read::Package(path) => {
                value(self.outside.get(path.as_str()))
            }
            Read::Drawn => Digest::Nothing,
        }
    }

    /// What a build reads: two package directories, the root's manifest and lock file, a
    /// variable a step asks for, one read unasked, a header probed outside.
    fn reads() -> [Read; 7] {
        [
            Read::Node("a".to_owned()),
            Read::Node("b".to_owned()),
            Read::Node("Cargo.toml".to_owned()),
            Read::Node("Cargo.lock".to_owned()),
            Read::Env("CC".to_owned()),
            Read::Env("BLAZE_OPT".to_owned()),
            Read::Outside("/usr/z.h".to_owned()),
        ]
    }

    /// The lock file a build writes: a function of the manifest.
    fn lock(&self) -> u8 {
        self.files.get("Cargo.toml").map_or(0, |byte| byte.wrapping_add(10))
    }

    /// This world as a build of it reads it: with the build's own write in it.
    fn locked(&self) -> World {
        let mut pinned = self.clone();
        pinned.files.insert("Cargo.lock", self.lock());
        pinned
    }

    /// The bytes a build of this (locked) world makes.
    fn product(&self) -> ContentId {
        let mut hasher = Hasher::new("test product");
        for read in World::reads() {
            match self.digest(&read) {
                Digest::Nothing => hasher.part(b"-"),
                Digest::Is(id) => hasher.id(&id),
            };
        }
        hasher.finish()
    }

    fn answer(product: ContentId) -> Answer {
        Answer {
            outputs: vec![Output { base: 0, path: OUTPUT.to_owned(), content: product }],
            units: vec![product],
            said: product.0.to_vec(),
        }
    }

    /// The oracle: what a fresh build of the world as it is would say and leave.
    fn fresh(&self) -> Answer {
        World::answer(self.locked().product())
    }

    /// A build: it pins the world, `mid` is saved by someone else under it, it writes its
    /// lock file and places its output. What it hands the machine, and the view it read.
    fn build(&mut self, mid: Option<(&'static str, u8)>) -> (Built, World) {
        let pinned = self.locked();
        if let Some((path, byte)) = mid {
            self.apply(Op::File(path, Some(byte)));
        }
        self.files.insert("Cargo.lock", pinned.lock());
        let product = pinned.product();
        self.clock_ns += 1;
        let stamp = self.stamp();
        self.output = Some((product, stamp));
        let valued = [Read::Env("CC".to_owned()), Read::Outside("/usr/z.h".to_owned())]
            .into_iter()
            .map(|read| {
                let digest = pinned.digest(&read);
                (read, digest)
            })
            .collect();
        let answer = World::answer(product);
        let built = Built {
            nodes: ["a", "b", "Cargo.toml", "Cargo.lock"].map(str::to_owned).to_vec(),
            valued,
            outputs: answer.outputs.into_iter().map(|it| (it, Some((stamp, self.clock_ns)))).collect(),
            units: answer.units,
            said: answer.said,
            untraced: if self.untraced { vec!["build script of a".to_owned()] } else { Vec::new() },
        };
        (built, pinned)
    }

    fn look(&self, built: Option<&World>, ask: &Ask) -> Told {
        match ask {
            Ask::Read { read, view: View::Now } => Told::Read(self.digest(read)),
            Ask::Read { read, view: View::Built } => {
                Told::Read(built.expect("the build's view is asked for after a build").digest(read))
            }
            Ask::Stamp { .. } => Told::Stamp { stamp: self.output.map(|it| it.1), looked_ns: self.clock_ns },
            Ask::Content { .. } => Told::Content(self.output.map(|it| it.0)),
        }
    }

    /// Everything a build reads or leaves; not the file, the variable and the outside file
    /// nothing reads, not a stamp.
    fn relevant(&self) -> Relevant {
        let mut files = self.files.clone();
        files.remove("README");
        let mut env = self.env.clone();
        env.remove("SHLVL");
        (files, env, self.outside.get("/usr/z.h").copied(), self.output.map(|it| it.0))
    }
}

struct Daemon {
    shelf: Shelf,
    evidence: Evidence,
}

enum Outcome {
    Hit(Answer),
    /// The miss, and what the step after its build said.
    Built(Miss, Next),
}

/// One request driven to its end: every ask answered from the world, in the seed's order.
fn request(
    world: &mut World,
    daemon: &mut Daemon,
    sim: &mut Sim,
    mut mid: Option<(&'static str, u8)>,
) -> Result<Outcome, String> {
    let unasked = world.env.keys().filter(|name| name.starts_with("BLAZE_")).map(|name| (*name).to_owned());
    let mut machine = Request::new(the_base(), false, unasked.collect());
    let mut view = None;
    let mut missed = None;
    for _ in 0..64 {
        match machine.step(&daemon.shelf, &mut daemon.evidence).map_err(|why| why.to_string())? {
            Next::Asks(mut asks) => {
                sim.rng().shuffle(&mut asks);
                for (told, ask) in asks.iter().enumerate() {
                    if told == 1 {
                        // With asks open a step only waits, and says for what.
                        let left = asks.len() - 1;
                        match machine.step(&daemon.shelf, &mut daemon.evidence) {
                            Ok(Next::Waiting { left: said, .. }) if said == left => {}
                            other => return Err(format!("a step with {left} asks open said {other:?}")),
                        }
                    }
                    machine.told(ask, world.look(view.as_ref(), ask)).map_err(|why| why.to_string())?;
                }
            }
            Next::Waiting { on, left } => return Err(format!("{on}: waits for {left} with everything told")),
            Next::Hit(answer) => return Ok(Outcome::Hit(answer)),
            Next::Miss(miss) => {
                let (built, pinned) = world.build(mid.take());
                view = Some(pinned);
                machine.built(built).map_err(|why| why.to_string())?;
                missed = Some(miss);
            }
            next @ (Next::Written { .. } | Next::Unwritten(_)) => {
                let miss = missed.ok_or("a record was written with no miss before it")?;
                return Ok(Outcome::Built(miss, next));
            }
        }
    }
    Err("the request did not end in 64 steps".to_owned())
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum Seen {
    Hit,
    Miss(Why),
}

/// Runs `ops` with `plant` in the machine and checks the invariant at every request. What
/// each request came to, or what the oracle says.
fn run(ops: &[Op], seed: u64, plant: Plant) -> Result<Vec<Seen>, String> {
    PLANT.set(plant);
    let ran = run_planted(ops, seed);
    PLANT.set(Plant::None);
    ran
}

fn run_planted(ops: &[Op], seed: u64) -> Result<Vec<Seen>, String> {
    let mut sim = Sim::new(seed, Faults::default());
    let mut world = World::new();
    let mut daemon = Daemon { shelf: Shelf::new(), evidence: Evidence::default() };
    // What a recorded build (or a hit) left, as far as any build reads it.
    let mut recorded: Option<Relevant> = None;
    let mut seen = Vec::new();
    for (n, op) in ops.iter().enumerate() {
        let Op::Request { mid } = *op else {
            if matches!(op, Op::Restart) {
                daemon.evidence = Evidence::default();
            }
            world.apply(*op);
            continue;
        };
        let before = world.clone();
        match request(&mut world, &mut daemon, &mut sim, mid).map_err(|why| format!("step {n}: {why}"))? {
            Outcome::Hit(answer) => {
                let fresh = before.fresh();
                if answer != fresh {
                    return Err(format!("step {n}: a hit says other than a fresh build would"));
                }
                if world.output.map(|it| it.0) != Some(fresh.outputs[0].content) {
                    return Err(format!("step {n}: a hit with an output that is not the build's"));
                }
                recorded = Some(world.relevant());
                seen.push(Seen::Hit);
            }
            Outcome::Built(miss, next) => {
                if recorded == Some(before.relevant()) {
                    return Err(format!(
                        "step {n}: nothing a build reads changed since a recorded one, and the request missed: {}",
                        miss.why
                    ));
                }
                if !matches!(next, Next::Written { .. }) {
                    return Err(format!("step {n}: a traced build that answered left no record: {next:?}"));
                }
                recorded = mid.is_none().then(|| world.relevant());
                seen.push(Seen::Miss(miss.why));
            }
        }
    }
    Ok(seen)
}

#[hegel::composite]
fn op(tc: &hegel::TestCase) -> Op {
    let which = tc.draw(gs::integers::<u8>().max_value(5)) as usize;
    let byte = tc.draw(gs::integers::<u8>().max_value(2));
    let value = (byte > 0).then_some(byte);
    match tc.draw(gs::integers::<u8>().max_value(11)) {
        0 | 1 => Op::File(FILES[which], value),
        2 => Op::Env(VARS[which % 3], value),
        3 => Op::Outside(OUTSIDE[which % 2], value),
        4 => Op::Output(value),
        5 => Op::Wait(if which % 2 == 0 { GRANULE_NS / 3 } else { 3 * GRANULE_NS }),
        6 => Op::Restart,
        7 => Op::Request { mid: Some((FILES[which], byte + 3)) },
        _ => Op::Request { mid: None },
    }
}

/// The invariant: a hit is given only when a fresh build would say the same bytes and leave
/// the same outputs, and a request with nothing a build reads changed since a recorded build
/// hits; for any sequence of edits, variable changes, outside files appearing and going,
/// outputs changed by hand, waits, restarts and saves under a build, in any order of the
/// driver's answers.
#[hegel::test]
fn a_hit_is_given_only_when_a_fresh_build_would_say_the_same(tc: hegel::TestCase) {
    let ops: Vec<Op> = tc.draw(gs::vecs(op()).max_size(40));
    let seed = tc.draw(gs::integers::<u64>());
    if let Err(why) = run(&ops, seed, Plant::None) {
        panic!("{why}");
    }
}

const REQUEST: Op = Op::Request { mid: None };

/// `ops` pass as the machine is, and with `plant` the oracle says `says`.
fn caught(ops: &[Op], plant: Plant, says: &str) {
    for seed in 0..8 {
        run(ops, seed, Plant::None).unwrap();
        let why = run(ops, seed, plant).expect_err("the oracle missed the planted bug");
        assert!(why.contains(says), "{why}");
    }
}

/// A control: a variable a step read is left out of the record, then changes.
#[test]
fn the_oracle_catches_a_variable_left_out_of_the_record() {
    caught(&[REQUEST, Op::Env("CC", Some(2)), REQUEST], Plant::DropsEnv, "a hit says other");
}

/// A control: a file that was looked for and absent is left out of the record, then is
/// there.
#[test]
fn the_oracle_catches_a_negative_probe_left_out_of_the_record() {
    caught(&[REQUEST, Op::Outside("/usr/z.h", Some(1)), REQUEST], Plant::DropsProbes, "a hit says other");
}

/// A control: a file is saved under the build, and the record is of the checkout at the
/// build's end, not of the view the build read with its own writes.
#[test]
fn the_oracle_catches_a_root_taken_after_the_build() {
    let ops = [Op::Request { mid: Some(("a/lib.rs", 2)) }, REQUEST];
    caught(&ops, Plant::RootAfterBuild, "a hit says other");
    // And the build's own write is in the view recorded: the lock file moves nothing.
    assert_eq!(run(&[REQUEST, REQUEST], 0, Plant::None).unwrap()[1], Seen::Hit);
}

/// A control: an output changed by hand since, and never looked at.
#[test]
fn the_oracle_catches_an_output_changed_by_hand() {
    let ops = [REQUEST, Op::Wait(3 * GRANULE_NS), Op::Output(Some(2)), REQUEST];
    caught(&ops, Plant::IgnoresOutputs, "an output that is not the build's");
}

/// A control (Relax's racy-stamp case): an output changed by hand within the granule it was
/// placed in keeps its stamp; a stamp taken that close to the change must not vouch.
#[test]
fn the_oracle_catches_a_stamp_trusted_too_close_to_the_change() {
    let ops = [REQUEST, Op::Output(Some(2)), REQUEST];
    caught(&ops, Plant::TrustsRacyStamps, "an output that is not the build's");
}

/// A control: a variable read unasked was unset at the build, so no list names it; set now,
/// it must fail the list.
#[test]
fn the_oracle_catches_an_unasked_variable_set_since() {
    caught(&[REQUEST, Op::Env("BLAZE_OPT", Some(1)), REQUEST], Plant::ForgetsUnasked, "a hit says other");
}

fn node(path: &str) -> Read {
    Read::Node(path.to_owned())
}

/// A miss names what moved, and only these miss: an edit, a file that appears in a package,
/// a variable read, an output gone. A file touched and left as it was, a file and a variable
/// nothing reads, a wait and a restart do not.
#[test]
fn a_miss_names_what_moved_and_nothing_else_misses() {
    let ops = [
        REQUEST,
        REQUEST,
        Op::File("a/lib.rs", Some(1)),
        Op::File("README", Some(2)),
        Op::Env("SHLVL", Some(2)),
        Op::Outside("/tmp/x", Some(1)),
        Op::Wait(3 * GRANULE_NS),
        REQUEST,
        Op::Restart,
        REQUEST,
        Op::File("a/lib.rs", Some(2)),
        REQUEST,
        Op::File("a/build.rs", Some(1)),
        REQUEST,
        Op::Env("CC", None),
        REQUEST,
        Op::Output(None),
        REQUEST,
        Op::Restart,
        Op::File("b/lib.rs", Some(2)),
        REQUEST,
        // Back to content a build is on record for, after a restart: the record answers,
        // and the output in place is the other build's, so it is placed again; no read moved.
        Op::File("b/lib.rs", Some(1)),
        Op::Restart,
        REQUEST,
    ];
    let seen = run(&ops, 3, Plant::None).unwrap();
    let want = [
        Seen::Miss(Why::Unfiled),
        Seen::Hit,
        Seen::Hit,
        Seen::Hit,
        Seen::Miss(Why::Moved(vec![node("a")])),
        Seen::Miss(Why::Moved(vec![node("a")])),
        Seen::Miss(Why::Moved(vec![Read::Env("CC".to_owned())])),
        Seen::Miss(Why::Output { base: 0, path: OUTPUT.to_owned(), how: How::Gone }),
        Seen::Miss(Why::Unnamed),
        Seen::Miss(Why::Output { base: 0, path: OUTPUT.to_owned(), how: How::Changed }),
    ];
    assert_eq!(seen, want);
}

/// An untraced step leaves no certificate and names its unit; the next request builds.
#[test]
fn an_untraced_step_leaves_no_certificate_and_names_its_unit() {
    let mut sim = Sim::new(1, Faults::default());
    let mut world = World::new();
    world.untraced = true;
    let mut daemon = Daemon { shelf: Shelf::new(), evidence: Evidence::default() };
    for _ in 0..2 {
        match request(&mut world, &mut daemon, &mut sim, None).unwrap() {
            Outcome::Built(miss, Next::Unwritten(Unwritten::Untraced(units))) => {
                assert_eq!(miss.why, Why::Unfiled);
                assert_eq!(units, ["build script of a"]);
            }
            _ => panic!("an untraced build was certified"),
        }
    }
    assert!(daemon.shelf.held.borrow().is_empty());
}

/// Rule 26: a read-list a previous layout filed is refused by that layout's name, never read
/// as no record; the build that follows files this layout's beside it, and that one answers.
#[test]
fn a_list_of_the_previous_layout_is_refused_by_name() {
    let mut sim = Sim::new(1, Faults::default());
    let mut world = World::new();
    let mut daemon = Daemon { shelf: Shelf::new(), evidence: Evidence::default() };
    let mut note = b"blzcert\n".to_vec();
    note.extend_from_slice(&1_u32.to_le_bytes());
    let mut list = Vec::new();
    blaze_record::write_list(&mut list, &the_base(), &note, [b"\x00a".as_slice()]);
    let first = blaze_record::list_key(&the_base(), blaze_record::Slot { generation: 0, n: 0 });
    daemon.shelf.put(Kind::List, first, &list).unwrap();
    match request(&mut world, &mut daemon, &mut sim, None).unwrap() {
        Outcome::Built(miss, Next::Written { .. }) => {
            assert_eq!(miss.why, Why::Unfiled);
            assert!(matches!(miss.refused[..], [Undecoded::Previous(1, _)]), "{:?}", miss.refused);
            assert!(miss.refused[0].to_string().contains("layout 1"));
        }
        _ => panic!("a list of layout 1 was read"),
    }
    daemon.evidence = Evidence::default();
    assert!(matches!(request(&mut world, &mut daemon, &mut sim, None).unwrap(), Outcome::Hit(_)));
}

/// Rule 18: asks left open end the machine in a typed outcome naming the wait, after
/// [`MOST_IDLE`] steps with nothing told.
#[test]
fn asks_never_answered_end_in_a_named_stall() {
    let mut sim = Sim::new(1, Faults::default());
    let mut world = World::new();
    let mut daemon = Daemon { shelf: Shelf::new(), evidence: Evidence::default() };
    assert!(matches!(request(&mut world, &mut daemon, &mut sim, None).unwrap(), Outcome::Built(..)));
    let mut machine = Request::new(the_base(), false, Vec::new());
    assert!(matches!(machine.step(&daemon.shelf, &mut daemon.evidence), Ok(Next::Asks(_))));
    for _ in 0..MOST_IDLE {
        assert!(matches!(machine.step(&daemon.shelf, &mut daemon.evidence), Ok(Next::Waiting { .. })));
    }
    match machine.step(&daemon.shelf, &mut daemon.evidence) {
        Err(Stopped::Stalled { on, left }) => {
            assert!(on.contains("each read on record"), "{on}");
            assert!(left > 0);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        machine.step(&daemon.shelf, &mut daemon.evidence),
        Err(Stopped::Misused(Misused("stepped past its end")))
    );
}

/// A request for the build's trace is another request: it builds, whatever is on record.
#[test]
fn a_traced_request_builds() {
    let daemon = Daemon { shelf: Shelf::new(), evidence: Evidence::default() };
    let mut evidence = daemon.evidence;
    let mut machine = Request::new(the_base(), true, Vec::new());
    match machine.step(&daemon.shelf, &mut evidence) {
        Ok(Next::Miss(miss)) => assert_eq!(miss.why, Why::Traced),
        other => panic!("{other:?}"),
    }
}
