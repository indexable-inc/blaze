//! Lint units, simulated (key.rs `Kind::Lint`; docs/v2/plan/design/lint.md). A toy checkout:
//! crate 0 a member (root file 0, which lists file 2 as a module), crate 1 a dependency (root
//! file 1, under the `UnitGraph`). Each file's `FileItems` record lists one `Lint` part a unit,
//! keyed by that unit's own value alone; a `Lint` task reads nothing and finds one finding when
//! its value is odd. Cases:
//! - `check` lints every unit of the member's files and no unit of the dependency's, and prints
//!   each finding; an unchanged repeat runs no task (plant: every part a job, which lints the
//!   dependency);
//! - one unit edited relints that unit alone (the harness's bug: a unit keyed by its whole file,
//!   which relints every unit of it).
//!
//! Every test's expected line is "test sim_lints::<name> ... ok"; each control passes when its
//! plant is caught, with the message it names.

use std::collections::{BTreeMap, BTreeSet};
use std::format;
use std::string::String;
use std::vec::Vec as StdVec;

use blaze_id::ContentId;
use blaze_own::{Alloc, Vec};
use blaze_sim::{Broke, Config as SimConfig, Tape, check, control};

use crate::Plant;
use crate::condition::Space;
use crate::key::{self, Key, Kind, Name, Tool};
use crate::record::{Finding, Locate, Part, PathOf};
use crate::request::{Answer, Ask, BatchId, Command, Config, Effect, Event, FileEntry, Gate, Outcome, Output, Ran, Request, Task, Ticket};

const EDITION: u8 = 3;
/// A lint finding: advice, never an error (the cli's policy when no level is configured).
const LINT_RULE: u16 = 0x4C01;
const MOST_EVENTS: u32 = 100_000;

/// The harness's own bug, beside the request's plants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Bug {
    None,
    /// Each `Lint` part keyed by its whole file, not its unit: an edit of one unit relints all.
    KeyedByFile,
}

/// Each file's lint units' values; crate 0's root is file 0 (which lists file 2), crate 1's
/// file 1.
#[derive(Clone, Debug)]
struct World {
    units: [StdVec<u64>; 3],
}

const MEMBER_FILES: [usize; 2] = [0, 2];
const DEPENDENCY_FILE: usize = 1;

fn path(f: usize) -> StdVec<u8> {
    format!("src/f{f}.rs").into_bytes()
}

fn file_name(f: usize) -> Name {
    Name::file(&path(f))
}

fn lint_name(f: usize, u: usize) -> Name {
    Name::new(Kind::Lint, key::digest("sim lints unit", format!("{f} {u}").as_bytes()))
}

fn scope_name(c: usize) -> Name {
    Name::new(Kind::CrateScope, key::digest("sim lints scope", format!("{c}").as_bytes()))
}

impl World {
    fn id(&self, f: usize) -> ContentId {
        key::digest("sim lints file", format!("{f} {:?}", self.units[f]).as_bytes())
    }

    fn unit_of(&self, name: Name) -> Option<(usize, usize)> {
        (0..3).find_map(|f| (0..self.units[f].len()).find(|u| lint_name(f, *u) == name).map(|u| (f, u)))
    }

    fn source(&self, f: usize, u: usize, bug: Bug) -> ContentId {
        match bug {
            Bug::None => key::digest("sim lints unit source", format!("{f} {u} {}", self.units[f][u]).as_bytes()),
            Bug::KeyedByFile => key::digest("sim lints unit source", format!("{f} {u} {:?}", self.id(f)).as_bytes()),
        }
    }
}

// --- The fake executor ------------------------------------------------------------------------

struct Made {
    name: Name,
    out: ContentId,
    parts: StdVec<Part>,
    paths: StdVec<(Name, StdVec<u8>)>,
    findings: StdVec<(u16, StdVec<u8>)>,
}

fn made(name: Name, out: ContentId) -> Made {
    Made { name, out, parts: StdVec::new(), paths: StdVec::new(), findings: StdVec::new() }
}

/// The member's crate (`Units`) or the dependency's (`UnitGraph`): its root file and its scope.
fn crate_parts(member: bool, name: Name) -> Made {
    let (c, root) = if member { (0, 0) } else { (1, DEPENDENCY_FILE) };
    let mut m = made(name, key::digest("sim lints crates", format!("{member}").as_bytes()));
    m.parts.push(Part { name: file_name(root), locate: Locate::File { edition: EDITION }, place: 0 });
    m.paths.push((file_name(root), path(root)));
    m.parts.push(Part { name: scope_name(c), locate: Locate::Source(key::digest("sim lints scope source", format!("{c}").as_bytes())), place: 0 });
    m.parts.sort_by_key(|part| part.name);
    m
}

fn execute(world: &World, bug: Bug, task: &Task<'static>) -> Result<Made, Broke> {
    let name = task.name;
    match name.kind {
        Kind::Units => Ok(crate_parts(true, name)),
        Kind::UnitGraph => Ok(crate_parts(false, name)),
        Kind::CrateScope => Ok(made(name, task.source)),
        Kind::FileItems => {
            let f = (0..3).find(|f| file_name(*f) == name).ok_or(Broke::Harness(format!("no file for {name}")))?;
            let mut m = made(name, world.id(f));
            if f == 0 {
                m.parts.push(Part { name: file_name(2), locate: Locate::File { edition: EDITION }, place: 0 });
                m.paths.push((file_name(2), path(2)));
            }
            for u in 0..world.units[f].len() {
                m.parts.push(Part { name: lint_name(f, u), locate: Locate::Source(world.source(f, u, bug)), place: u as u32 });
            }
            m.parts.sort_by_key(|part| part.name);
            Ok(m)
        }
        Kind::Lint => {
            let (f, u) = world.unit_of(name).ok_or(Broke::Harness(format!("no unit for {name}")))?;
            if task.source != world.source(f, u, bug) {
                return Err(Broke::Oracle(format!("{name} ran with a source that is not its unit's")));
            }
            if !task.inputs.is_empty() {
                return Err(Broke::Oracle(format!("{name} was handed inputs: a lint unit reads nothing")));
            }
            let value = world.units[f][u];
            let mut m = made(name, key::digest("sim lints out", format!("{value}").as_bytes()));
            if value % 2 == 1 {
                m.findings.push((LINT_RULE, format!("odd unit {f} {u}").into_bytes()));
            }
            Ok(m)
        }
        kind => Err(Broke::Harness(format!("the lints sim has no {} task", kind.name()))),
    }
}

// --- One request, driven ----------------------------------------------------------------------

#[derive(Default)]
struct Ended {
    answer: Option<Answer>,
    /// Every name a batch ran this request.
    ran: BTreeSet<Name>,
    /// (name, rule, message) of every print.
    prints: BTreeSet<(Name, u16, StdVec<u8>)>,
    last: Option<StdVec<u8>>,
}

impl Ended {
    fn ran_of(&self, kind: Kind) -> BTreeSet<Name> {
        self.ran.iter().filter(|name| name.kind == kind).copied().collect()
    }
}

fn never_an_error(_: u16) -> bool {
    false
}

fn config(workers: u32) -> Config {
    let tools = Kind::ALL.map(|kind| Tool(key::digest("sim lints kind tool", &[kind as u8])));
    Config { tool: Tool(key::digest("sim lints tool", b"")), tools, workers, most_entries: 32, most_jobs: 1 << 16, is_error: never_an_error }
}

const ASK: Ask<'static> = Ask { command: Command::Check, args: b"", gate: Gate::Beside, space: Space::NONE, program: None, facts: &[] };

fn request(
    tape: &Tape,
    world: &World,
    before: Option<&World>,
    plant: Plant,
    bug: Bug,
    store: &mut BTreeMap<Key, StdVec<u8>>,
    last: Option<&[u8]>,
) -> Result<Ended, Broke> {
    let paths: StdVec<StdVec<u8>> = (0..3).map(path).collect();
    let mut files: StdVec<FileEntry<'_>> = (0..3)
        .map(|f| FileEntry { path: &paths[f], id: world.id(f), changed: before.is_none_or(|old| old.id(f) != world.id(f)), manifest: false })
        .collect();
    let manifest = b"Cargo.toml".to_vec();
    let units = key::digest("sim lints units", b"one member");
    files.push(FileEntry { path: &manifest, id: units, changed: false, manifest: true });
    files.sort_by(|a, b| a.path.cmp(b.path));
    let graph = Some(key::digest("sim lints graph", b"one dependency"));
    let workers = [1, 2, 16][tape.index(3, "workers")];
    let mut req = Request::planted_in(config(workers), ASK, plant, blaze_own::testing::heap().alloc());
    let mut out: Vec<Effect<'static>, Alloc<'static>> = Vec::new_in(blaze_own::testing::heap().alloc());
    let mut ended = Ended::default();
    let mut gets: StdVec<Key> = StdVec::new();
    let mut batches: StdVec<(BatchId, StdVec<(Ticket, Task<'static>)>)> = StdVec::new();
    let mut first = Some(Event::Start { files: &files, gone: &[], last, units, cold: false, graph });
    for _ in 0..MOST_EVENTS {
        let stepped = match first.take() {
            Some(event) => req.step(event, &mut out),
            None => {
                let total = gets.len() + batches.len();
                if total == 0 {
                    return Err(Broke::Oracle(String::from("the request stalled: nothing out and no answer")));
                }
                let pick = tape.index(total, "which completes");
                if pick < gets.len() {
                    let key = gets.swap_remove(pick);
                    let bytes = store.get(&key).cloned();
                    req.step(Event::Got { key, bytes: bytes.as_deref() }, &mut out)
                } else {
                    let (id, tasks) = batches.swap_remove(pick - gets.len());
                    let mut made = StdVec::new();
                    for (ticket, task) in tasks.iter() {
                        made.push((*ticket, execute(world, bug, task)?));
                    }
                    let held: StdVec<(StdVec<Finding<'_>>, StdVec<PathOf<'_>>)> = made
                        .iter()
                        .map(|(_, m)| {
                            let findings = m.findings.iter().map(|(rule, message)| Finding { rule: *rule, start: 0, end: 1, message }).collect();
                            let paths = m.paths.iter().map(|(name, path)| PathOf { name: *name, path, module: false }).collect();
                            (findings, paths)
                        })
                        .collect();
                    let outputs: StdVec<Output<'_>> = made
                        .iter()
                        .zip(held.iter())
                        .map(|((_, m), (findings, paths))| Output { name: m.name, out: m.out, reads: &[], cells: &[], parts: &m.parts, paths, findings, payload: b"" })
                        .collect();
                    let results: StdVec<Outcome<'_>> = made
                        .iter()
                        .enumerate()
                        .map(|(at, (ticket, _))| Outcome { ticket: *ticket, ran: Ran::Done(&outputs[at..at + 1]), in_place: 0, ns: 1_000 })
                        .collect();
                    req.step(Event::Ran { batch: id, results: &results }, &mut out)
                }
            }
        };
        if let Err(error) = stepped {
            return Err(Broke::Oracle(format!("the request failed: {error}")));
        }
        for effect in out.drain(..) {
            match effect {
                Effect::Get { key } => gets.push(key),
                Effect::Put { key, bytes } => match store.get(&key) {
                    Some(held) if held.as_slice() != bytes.as_slice() => return Err(Broke::Oracle(format!("two values under one key {key} (rule 21)"))),
                    Some(_) => {}
                    None => {
                        store.insert(key, bytes.as_slice().to_vec());
                    }
                },
                Effect::Run { batch } => {
                    let mut tasks = StdVec::new();
                    for (ticket, task) in batch.tasks.into_iter() {
                        ended.ran.insert(task.name);
                        tasks.push((ticket, task));
                    }
                    batches.push((batch.id, tasks));
                }
                Effect::Publish(_) | Effect::Unpublish(_) => {}
                Effect::Print(print) => {
                    ended.prints.insert((print.name, print.rule, print.message.as_slice().to_vec()));
                }
                Effect::WriteLast { bytes } => ended.last = Some(bytes.as_slice().to_vec()),
                Effect::Done(answer) => ended.answer = Some(answer),
                Effect::Open { name, .. } => return Err(Broke::Harness(format!("{name} was opened, and every file was handed"))),
                Effect::StartProgram { .. } | Effect::Stop { .. } => return Err(Broke::Harness(String::from("`check` started or stopped a program"))),
            }
        }
        if ended.answer.is_some() {
            if !gets.is_empty() || !batches.is_empty() {
                return Err(Broke::Oracle(String::from("answered with work still out")));
            }
            if !req.failures().is_empty() {
                return Err(Broke::Oracle(format!("tasks failed: {:?}", req.failures())));
            }
            return Ok(ended);
        }
    }
    Err(Broke::Harness(format!("more than {MOST_EVENTS} events")))
}

/// One to three units a file, each value drawn.
fn draw(tape: &Tape) -> World {
    let file = |label: &'static str| -> StdVec<u64> { (0..1 + tape.index(3, label)).map(|_| tape.index(4, "unit value") as u64).collect() };
    World { units: [file("units of file 0"), file("units of file 1"), file("units of file 2")] }
}

/// The member's units: what `check` lints.
fn member_units(world: &World) -> BTreeSet<Name> {
    MEMBER_FILES.iter().flat_map(|f| (0..world.units[*f].len()).map(move |u| lint_name(*f, u))).collect()
}

// --- Case 1: only the asked crates, and a repeat runs nothing ----------------------------------

fn asked_only(tape: &Tape, plant: Plant, bug: Bug) -> Result<(), Broke> {
    let world = draw(tape);
    let mut store = BTreeMap::new();
    let first = request(tape, &world, None, plant, bug, &mut store, None)?;
    let ran = first.ran_of(Kind::Lint);
    if ran.iter().any(|name| world.unit_of(*name).is_some_and(|(f, _)| f == DEPENDENCY_FILE)) {
        return Err(Broke::Oracle(format!("a dependency's unit was linted: ran {ran:?}")));
    }
    if ran != member_units(&world) {
        return Err(Broke::Oracle(format!("check linted {ran:?}, not the member's units {:?}", member_units(&world))));
    }
    let printed: BTreeSet<Name> = first.prints.iter().filter(|(_, rule, _)| *rule == LINT_RULE).map(|(name, _, _)| *name).collect();
    let odd: BTreeSet<Name> = member_units(&world).into_iter().filter(|name| world.unit_of(*name).is_some_and(|(f, u)| world.units[f][u] % 2 == 1)).collect();
    if printed != odd {
        return Err(Broke::Oracle(format!("printed lint findings of {printed:?}, want {odd:?}")));
    }
    let again = request(tape, &world, Some(&world), plant, bug, &mut store, first.last.as_deref())?;
    if !again.ran.is_empty() {
        return Err(Broke::Oracle(format!("an unchanged repeat ran {} tasks: {:?}", again.ran.len(), again.ran)));
    }
    Ok(())
}

/// Expected: "test sim_lints::check_lints_the_members_units_alone_and_a_repeat_runs_none ... ok".
#[test]
fn check_lints_the_members_units_alone_and_a_repeat_runs_none() {
    check(SimConfig::cases(200), |tape| asked_only(tape, Plant::None, Bug::None));
}

/// Expected: "test sim_lints::control_every_part_made_a_job_lints_the_dependency ... ok".
#[test]
fn control_every_part_made_a_job_lints_the_dependency() {
    control(SimConfig::cases(300), (Plant::None, Bug::None), (Plant::EveryPartAJob, Bug::None), "a dependency's unit was linted", |tape, (plant, bug)| asked_only(tape, plant, bug));
}

// --- Case 2: one unit edited ------------------------------------------------------------------

fn one_edited(tape: &Tape, plant: Plant, bug: Bug) -> Result<(), Broke> {
    let before = draw(tape);
    let mut store = BTreeMap::new();
    let first = request(tape, &before, None, plant, bug, &mut store, None)?;
    let f = MEMBER_FILES[tape.index(MEMBER_FILES.len(), "edited file")];
    let u = tape.index(before.units[f].len(), "edited unit");
    let mut after = before.clone();
    after.units[f][u] += 1 + tape.index(2, "how far") as u64;
    let second = request(tape, &after, Some(&before), plant, bug, &mut store, first.last.as_deref())?;
    let ran = second.ran_of(Kind::Lint);
    let want: BTreeSet<Name> = [lint_name(f, u)].into_iter().collect();
    if ran != want {
        return Err(Broke::Oracle(format!("one unit's edit relinted {} units: {ran:?}", ran.len())));
    }
    Ok(())
}

/// Expected: "test sim_lints::one_unit_edited_relints_it_alone ... ok".
#[test]
fn one_unit_edited_relints_it_alone() {
    check(SimConfig::cases(200), |tape| one_edited(tape, Plant::None, Bug::None));
}

/// Expected: "test sim_lints::control_a_unit_keyed_by_its_file_is_caught ... ok".
#[test]
fn control_a_unit_keyed_by_its_file_is_caught() {
    control(SimConfig::cases(300), (Plant::None, Bug::None), (Plant::None, Bug::KeyedByFile), "one unit's edit relinted", |tape, (plant, bug)| one_edited(tape, plant, bug));
}
