//! The command end to end on a real checkout in a temporary directory, the real sweep, the real
//! store under a temporary home and the real host, with a scripted front end over two units:
//! unit 0 (a library) and unit 1 (a program on it), each one file `src/u<n>.rs` of three items,
//! a line `name bbb..` each. An item's body key is its line; a unit's surface key is its item
//! names alone, so a body edit moves its body and its unit's manifest, never a surface. Bodies
//! read and make no fact, so an edit of one body redoes that body and nothing else.
//!
//! Each test states its invariant, checks it with an oracle that returns what broke, and runs
//! the oracle against a planted bug of the command ([`crate::run::Plant`]), which it must catch.

use std::alloc::{Allocator, Global};

use blaze_core::{lock::Lock, temp::TempDir};
use blaze_format::Aligned;
use blaze_ids::ContentId;
use blaze_keyed::{
    driver::{Bodies, Filed, ItemPlace, Outcome, TaskFailed},
    front::{FileItems, Front, Keyed, Universe},
    kind::{
        Body, BodyInputs, FileImage, FileInputs, Key, Kind, Namespace, Origin, Settings, Surface, SurfaceInputs, ToolId,
        UnitInputs, UnitManifest,
    },
    machine::Machine,
    record::{self, Fact, FactKind, Owner, StableId},
};
use blaze_sim::{Config, Tape};

use crate::{
    CommandError,
    front::Source,
    graph::{Blocked, Dep, Graph, Unit, UnitKind},
    run::{self, Gathers, PLANT, Plant, Summary},
};

const TOOL: ToolId = ToolId { tool: ContentId([1; 32]), target: ContentId([4; 32]), namespace: Namespace::LOCAL };
const SETTINGS: Settings = Settings(ContentId([2; 32]));
const MACHINE: Machine = Machine { cores: 4, fast_cores: 4, page_bytes: 16384, memory_bytes: 16 << 30, qos: false, huge_pages: false };
const NAMES: [&str; 3] = ["a", "b", "c"];

// --- the world --------------------------------------------------------------------------------

/// Each unit's items' body lengths, by [`NAMES`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct World {
    bodies: [[u32; 3]; 2],
}

const WORLD: World = World { bodies: [[1; 3]; 2] };

impl World {
    fn text(&self, unit: usize) -> String {
        let mut text = String::new();
        for (name, body) in NAMES.iter().zip(self.bodies[unit]) {
            text.push_str(&format!("{name} {}\n", "b".repeat(body as usize)));
        }
        text
    }

    /// One more byte in item `at` of unit `unit`: the file's size moves with every edit.
    fn edit(mut self, unit: usize, at: usize) -> World {
        self.bodies[unit][at] += 1;
        self
    }
}

/// A checkout and a home, each its own directory of one temporary one.
struct Bench {
    dir: TempDir,
}

impl Bench {
    fn new(prefix: &str) -> Bench {
        let dir = TempDir::for_test(prefix);
        std::fs::create_dir_all(dir.path().join("checkout").join("src")).expect("the checkout's source directory is made");
        std::fs::create_dir_all(dir.path().join("home")).expect("the home is made");
        Bench { dir }
    }

    fn checkout(&self) -> String {
        self.dir.path().join("checkout").to_str().expect("a temporary directory named in UTF-8").to_owned()
    }

    fn home(&self) -> std::path::PathBuf {
        self.dir.path().join("home")
    }

    fn write(&self, world: &World) {
        for unit in 0..2 {
            std::fs::write(format!("{}/src/u{unit}.rs", self.checkout()), world.text(unit)).expect("a unit's file is written");
        }
    }
}

fn unit_key(unit: u32) -> ContentId {
    ContentId([0x10 + unit as u8; 32])
}

fn owner(unit: u32) -> Owner {
    Owner::of_subject(&unit_key(unit))
}

fn text(it: &str) -> Box<str, Global> {
    crate::boxed(it, Global)
}

/// The two units of the bench's checkout; unit 1 `blocked` when given.
fn graph(bench: &Bench, blocked: Option<Blocked>) -> Graph<Global> {
    let checkout = bench.checkout();
    let unit = |at: u32, kind: UnitKind, deps: Vec<Dep<Global>, Global>, blocked: Option<Blocked>| Unit {
        name: text(&format!("u{at}")),
        package: text(&format!("u{at} 0.1.0")),
        edition: span::Edition::Edition2024,
        root: text(&format!("{checkout}/src/u{at}.rs")),
        kind,
        cfg: Vec::new_in(Global),
        deps,
        env: Vec::new_in(Global),
        member: true,
        owner: owner(at),
        namespace: Namespace::LOCAL,
        key: unit_key(at),
        origin: Origin::Private,
        bodies: Bodies::Asked,
        blaze_edition: None,
        settings: SETTINGS.0,
        blocked,
    };
    let mut units = Vec::new_in(Global);
    units.push(unit(0, UnitKind::Lib, Vec::new_in(Global), None));
    let mut deps = Vec::new_in(Global);
    deps.push(Dep { name: text("u0"), unit: 0, marks: blaze_units::schema::EdgeMarks { prelude: true, sysroot: false } });
    units.push(unit(1, UnitKind::Bin, deps, blocked));
    Graph {
        units,
        tool: TOOL.tool,
        settings: SETTINGS.0,
        target: TOOL.target,
        triple: text("test-triple"),
        checkout: text(&checkout),
        identity: ContentId([9; 32]),
        load: crate::graph::LoadTimes::default(),
        programs: Vec::new_in(Global),
        library: crate::graph::LibraryNames { dir: text("/library"), printed: text("/rustc/0/library") },
    }
}

// --- the scripted front -----------------------------------------------------------------------

struct Scripted {
    roots: [String; 2],
    /// Each unit's text, as [`Gathers::gather`] read it through the command's source.
    texts: [Vec<u8>; 2],
    /// Every task run: `key u0`, `surface u1`, `body u0::a`.
    log: Lock<Vec<String>>,
}

impl Scripted {
    fn new(graph: &Graph<Global>) -> Scripted {
        Scripted {
            roots: [graph.units[0].root.to_string(), graph.units[1].root.to_string()],
            texts: [Vec::new(), Vec::new()],
            log: Lock::new("test.command.front.log", Vec::new()),
        }
    }

    fn tasks(&self) -> usize {
        self.log.lock().len()
    }

    /// Unit `unit`'s items: (name, its line).
    fn items(&self, unit: u32) -> Vec<(&str, &[u8])> {
        let text = &self.texts[unit as usize];
        text.split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                let name = line.split(|byte| *byte == b' ').next().unwrap_or(line);
                (std::str::from_utf8(name).expect("an item's name is UTF-8"), line)
            })
            .collect()
    }
}

fn stable(unit: u32, name: &str) -> StableId {
    StableId::of(format!("u{unit}::{name}").as_bytes(), 1)
}

fn file_key(text: &[u8]) -> Key<FileImage> {
    FileImage::key(&FileInputs { tool: TOOL, edition: 2024, bytes: text })
}

fn body_key(unit: u32, name: &str, line: &[u8]) -> Key<Body> {
    Body::key(&BodyInputs { tool: TOOL, settings: SETTINGS, unit: unit_key(unit), item: stable(unit, name), text: record::digest(line) })
}

impl Gathers for Scripted {
    fn gather<S: Source>(&mut self, unit: u32, source: &mut S) -> Result<(), CommandError> {
        let at = unit as usize;
        let mut text = Vec::new();
        match source.read(&self.roots[at], &mut text) {
            Ok(true) => {}
            Ok(false) => return Err(CommandError::Unit { unit, rule: "the test's unit has no file" }),
            Err(rule) => return Err(CommandError::Unit { unit, rule }),
        }
        self.texts[at] = text;
        Ok(())
    }

    fn late(&self, _unit: u32) -> &[Fact] {
        &[]
    }

    fn told<B: Allocator + Clone>(&self, alloc: B) -> Vec<(u32, Box<str, B>), B> {
        Vec::new_in(alloc)
    }

    // The scripted key task never finds two items under one key: no place is asked.
    fn place<B: Allocator + Clone>(&self, unit: u32, at: ItemPlace, alloc: B) -> Box<str, B> {
        let mut out = blaze_mem::String::new_in(alloc);
        blaze_mem::text::format_into(&mut out, format_args!("u{unit}.rs, item row {}", at.row));
        out.into_boxed_str()
    }
}

impl Front for Scripted {
    fn key<A: Allocator + Clone>(&self, unit: u32, frame: A) -> Result<Keyed<A>, TaskFailed> {
        self.log.lock().push(format!("key u{unit}"));
        let items = self.items(unit);
        let mut bodies = Vec::new_in(frame.clone());
        for (name, line) in &items {
            bodies.push((stable(unit, name), body_key(unit, name, line)));
        }
        bodies.sort_by_key(|(item, _)| *item);
        let image = file_key(&self.texts[unit as usize]);
        let mut files = Vec::new_in(frame.clone());
        files.push(FileItems { image, at: 0, len: bodies.len() as u32 });
        // The scripted program prints no file name: one zero name for its one file.
        let mut names = Vec::new_in(frame);
        names.push(ContentId([0; 32]));
        let pieces: Vec<ContentId> = items.iter().map(|(name, _)| record::digest(name.as_bytes())).collect();
        let surface = Surface::key(&SurfaceInputs { tool: TOOL, settings: SETTINGS, unit: unit_key(unit), pieces: &pieces });
        let manifest = UnitManifest::key(&UnitInputs { tool: TOOL, settings: SETTINGS, unit: unit_key(unit), files: &[image], late: &[] });
        Ok(Keyed { manifest, surface, files, items: bodies, names })
    }

    fn surface(&self, unit: u32, key: Key<Surface>, universe: Universe<'_>) -> Result<Aligned, TaskFailed> {
        let _ = universe;
        self.log.lock().push(format!("surface u{unit}"));
        let mut seen = vec![unit as u8];
        let mut facts: Vec<Fact> = Vec::new();
        for (name, _) in self.items(unit).iter() {
            seen.extend_from_slice(name.as_bytes());
            let item = stable(unit, name);
            facts.push(Fact { id: record::fact(FactKind::Signature, owner(unit), 1, &item, b""), digest: record::digest(name.as_bytes()) });
            // Its maker row, its digest the body's text id: the surface names every body, so
            // the driver runs it (an asked unit's own item its surface does not name is silent).
            facts.push(Fact { id: record::fact(FactKind::Maker, owner(unit), record::MAKER_CODE, &item, b""), digest: item.0 });
        }
        facts.push(Fact { id: record::seen_id(), digest: record::digest(&seen) });
        facts.sort_by_key(|fact| fact.id);
        let (_, bytes) = record::write_read::<Surface>(key, &[], &facts, b"surface");
        Ok(Aligned::copy_from(&bytes))
    }

    fn body(&self, unit: u32, item: StableId, key: Key<Body>, universe: Universe<'_>) -> Result<Aligned, TaskFailed> {
        let _ = universe;
        let items = self.items(unit);
        let (name, line) = items
            .iter()
            .find(|(name, _)| stable(unit, name) == item)
            .ok_or(TaskFailed::Refused { what: "a body of no item of its unit" })?;
        self.log.lock().push(format!("body u{unit}::{name}"));
        let (_, bytes) = record::write_read::<Body>(key, &[], &[], line);
        Ok(Aligned::copy_from(&bytes))
    }
}

// --- one command ------------------------------------------------------------------------------

/// One command over `world`, written first, with a fresh front end; what it did and how many
/// tasks the front end ran.
fn command(bench: &Bench, world: &World, blocked: Option<Blocked>) -> (Result<Summary<Global>, CommandError>, usize) {
    bench.write(world);
    let graph = graph(bench, blocked);
    let mut front = Scripted::new(&graph);
    let done = run::check(&graph, &mut front, &bench.home(), MACHINE, Global);
    (done, front.tasks())
}

/// Runs `body` with `plant` planted in the command, and takes it out again.
fn planted<T>(plant: Plant, body: impl FnOnce() -> T) -> T {
    PLANT.set(plant);
    let out = body();
    PLANT.set(Plant::None);
    out
}

// --- (a) cold, then a no-op -------------------------------------------------------------------

/// The oracle of the no-op: the cold command gathered, built and filed its certificate; the
/// same command again gathered no unit's text, ran no task and was certified.
fn cold_then_no_op(bench: &Bench) -> Result<(), String> {
    let (cold, _) = command(bench, &WORLD, None);
    let cold = cold.map_err(|error| format!("the cold command: {error}"))?;
    if !matches!(cold.outcome, Outcome::Built { filed: Filed::Certificate, .. }) || !cold.gathered {
        return Err(format!(
            "the cold command ended {:?} (gathered: {}), not built with its certificate filed after gathering",
            cold.outcome, cold.gathered
        ));
    }
    let (again, tasks) = command(bench, &WORLD, None);
    let again = again.map_err(|error| format!("the same command again: {error}"))?;
    match (&again.outcome, tasks, again.gathered) {
        (Outcome::Certified { .. }, 0, false) => Ok(()),
        (outcome, tasks, gathered) => Err(format!(
            "the same command again ended {outcome:?} after {tasks} tasks, gathered: {gathered}: a no-op is certified, runs none and gathers nothing"
        )),
    }
}

/// The invariant: a command over an unchanged checkout, after one that built it, stands on the
/// certificate, gathers no text and runs no task. The controls, each caught by name: with the
/// certificate's base taking the sweep's instant ([`Plant::BaseOfRun`]) no two commands share
/// a certificate; with no look of the command's own ([`Plant::NoLook`]) a no-op gathers every
/// unit's text.
#[test]
fn an_unchanged_checkout_is_certified_and_runs_no_task() {
    let bench = Bench::new("blaze-command-noop");
    assert_eq!(cold_then_no_op(&bench), Ok(()));

    let control = Bench::new("blaze-command-noop-control");
    let caught = planted(Plant::BaseOfRun, || cold_then_no_op(&control));
    assert!(caught.is_err(), "the planted base of the run is caught: {caught:?}");

    let control = Bench::new("blaze-command-noop-look-control");
    let caught = planted(Plant::NoLook, || cold_then_no_op(&control));
    assert!(caught.as_ref().is_err_and(|why| why.contains("gathered: true")), "the planted missing look is caught: {caught:?}");
}

// --- (b) an edit reruns only what reads it -----------------------------------------------------

/// The oracle of an edit of item `at` of unit `unit`: that body's task ran, and no other body
/// or surface task of either unit.
fn only_the_edited_body(summary: &Summary<Global>, unit: usize, at: usize) -> Result<(), String> {
    if !matches!(summary.outcome, Outcome::Built { .. }) {
        return Err(format!("an edit of u{unit}::{} ended {:?}, not built", NAMES[at], summary.outcome));
    }
    for (other, done) in summary.units.iter().enumerate() {
        let bodies = u32::from(other == unit);
        if done.bodies != bodies || done.surfaces != 0 || done.failed != 0 {
            return Err(format!(
                "an edit of u{unit}::{}: u{other} ran {} bodies and {} surfaces ({} failed), not {bodies} and 0",
                NAMES[at], done.bodies, done.surfaces, done.failed
            ));
        }
    }
    Ok(())
}

/// One case: a cold command, then each edit and a command after it, each held to the oracle.
fn edits(bench: &Bench, edits: &[(usize, usize)]) -> Result<(), String> {
    let mut world = WORLD;
    command(bench, &world, None).0.map_err(|error| format!("the cold command: {error}"))?;
    for &(unit, at) in edits {
        world = world.edit(unit, at);
        let (done, _) = command(bench, &world, None);
        let done = done.map_err(|error| format!("after an edit of u{unit}::{}: {error}", NAMES[at]))?;
        only_the_edited_body(&done, unit, at)?;
    }
    Ok(())
}

fn edit(tape: &Tape) -> (usize, usize) {
    (usize::from(tape.int_in(0..=1u8, "unit")), usize::from(tape.int_in(0..=2u8, "item")))
}

/// The invariant: for any sequence of body edits, each command after one runs the edited body
/// and nothing else: the sweep sees the edit, the certificate misses, the edited unit's other
/// bodies are found by key and the other unit is settled by its mark. The control: a request
/// that leaves out its last read ([`Plant::SkipsARead`], `src/u1.rs`) never sees an edit of
/// unit 1, and the oracle names it.
#[test]
fn an_edit_reruns_only_the_body_it_edits() {
    blaze_sim::check(Config::cases(12), |tape: &Tape| {
        let drawn = tape.vec(1..=4, "edits", edit);
        edits(&Bench::new("blaze-command-edits"), &drawn)
    });

    let control = Bench::new("blaze-command-edits-control");
    let caught = planted(Plant::SkipsARead, || edits(&control, &[(1, 0)]));
    assert!(caught.is_err(), "the planted skipped read is caught: {caught:?}");
}

// --- (c) a blocked unit stops the command by name ------------------------------------------------

/// The oracle of a blocked unit: the command stopped with [`CommandError::NotBuiltYet`] naming
/// `u1`, before any task and before the home was written.
fn stopped(bench: &Bench) -> Result<(), String> {
    let (done, tasks) = command(bench, &WORLD, Some(Blocked::ProcMacro));
    let written = bench.home().join("v2").exists();
    match done {
        Err(CommandError::NotBuiltYet { count: 1, units }) if units.contains("`u1` (u1 0.1.0)") && tasks == 0 && !written => Ok(()),
        Err(error) => Err(format!("the command stopped with `{error}` after {tasks} tasks (home written: {written})")),
        Ok(summary) => Err(format!("the command ran to {:?} after {tasks} tasks with a blocked unit", summary.outcome)),
    }
}

/// The invariant: a unit the command cannot build yet stops it before anything is read or run,
/// naming the unit and why. The control: a command that ignores blocked units
/// ([`Plant::IgnoresBlocked`]) runs tasks, and the oracle says so.
#[test]
fn a_blocked_unit_stops_the_command_by_name_before_any_task() {
    assert_eq!(stopped(&Bench::new("blaze-command-blocked")), Ok(()));

    let control = Bench::new("blaze-command-blocked-control");
    let caught = planted(Plant::IgnoresBlocked, || stopped(&control));
    assert!(caught.is_err(), "the planted ignored block is caught: {caught:?}");
}
