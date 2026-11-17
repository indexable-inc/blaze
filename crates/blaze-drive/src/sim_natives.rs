//! A program's native image as drive tasks, simulated (run.md, "What a run takes from the
//! request, and the native image as a task"; the coordinator's ruling of 2026-11-16 23:1x on
//! the program's name). A toy world of crates, crate 0 the program's root: each crate's
//! `CrateScope` lists its `Links` part, located by its link table's digest, which is the part's
//! out; a crate with a build script has a `Script` part of `Units`. The `Native` job is the
//! request's ([`Ask::program`], named by [`Name::native`]): it reads the crate list (the
//! `Units` out), then each closure crate's `Links` and `Script`. Cases, each with the bug it
//! plants on the cli's side of the seam:
//! - a closure crate's link table edited reruns the image; an unreached crate's does not
//!   (plant: the image reads only its root crate's `Links`);
//! - an edit that leaves every link table equal runs no `Links` and no `Native`, and a cold
//!   repeat over the same store finds the image by key and runs nothing (plant: the scope lists
//!   its `Links` by a source that folds the scope's exports).
//!
//! Every test's expected line is "test sim_natives::<name> ... ok"; each control passes when
//! its plant is caught, with the message it names.

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
use crate::record::{self, Finding, Locate, Part, PathOf, Stored};
use crate::request::{
    Answer, Ask, BatchId, Command, Config, Effect, Event, FileEntry, Gate, Outcome, Output, Ran, Request, Task,
    TaskFailure, Ticket, Want, Why,
};

const MOST_EVENTS: u32 = 100_000;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Crate {
    /// The crates it depends on (the crate list's edges).
    deps: StdVec<usize>,
    /// Its link table's content: what its `Links` out digests.
    links: u64,
    /// Its exports: what its `CrateScope` out digests, never its table.
    surface: u64,
    /// Its build script's output, when it has one.
    script: Option<u64>,
}

/// The cli's side of the seam misbehaving, one way a world.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Bug {
    #[default]
    None,
    /// The image reads only its root crate's `Links`.
    RootLinksOnly,
    /// The scope lists its `Links` by a source that folds its exports.
    LinksSourceFoldsScope,
    /// The stored certificate's native slot is cleared between commands (an older form's
    /// certificate): a repeat must refuse by name, never start on a default image key.
    NativeSlotDropped,
}

#[derive(Clone, Debug, Default)]
struct World {
    crates: StdVec<Crate>,
    bug: Bug,
}

fn scope_name(c: usize) -> Name {
    Name::new(Kind::CrateScope, key::digest("sim natives scope", format!("{c}").as_bytes()))
}

fn links_name(c: usize) -> Name {
    Name::links_of(scope_name(c))
}

fn script_name(c: usize) -> Name {
    Name::new(Kind::Script, key::digest("sim natives script", format!("{c}").as_bytes()))
}

/// The program: crate 0's key, one target, one profile.
fn native_name() -> Name {
    Name::native(&key::digest("sim natives crate key", b"0"), b"aarch64-apple-darwin", b"dev")
}

impl World {
    fn of_name(&self, name: Name, of: fn(usize) -> Name) -> Option<usize> {
        (0..self.crates.len()).find(|c| of(*c) == name)
    }

    /// The crates the root reaches, itself first, then in index order.
    fn closure(&self) -> StdVec<usize> {
        let mut seen = BTreeSet::new();
        let mut work = std::vec![0usize];
        while let Some(c) = work.pop() {
            if seen.insert(c) {
                work.extend(self.crates[c].deps.iter().copied());
            }
        }
        seen.into_iter().collect()
    }

    fn links_source(&self, c: usize) -> ContentId {
        let krate = &self.crates[c];
        let folded = if self.bug == Bug::LinksSourceFoldsScope { krate.surface } else { 0 };
        key::digest("sim natives link table", format!("{c} {} {folded}", krate.links).as_bytes())
    }

    /// The crate list: its edges only, so an edit of a table or an export leaves it.
    fn list_out(&self) -> ContentId {
        let edges: StdVec<&StdVec<usize>> = self.crates.iter().map(|c| &c.deps).collect();
        key::digest("sim natives crate list", format!("{edges:?}").as_bytes())
    }

    fn units_source(&self) -> ContentId {
        key::digest("sim natives units", format!("{:?}", self.crates).as_bytes())
    }
}

// --- The fake executor ------------------------------------------------------------------------

struct Made {
    name: Name,
    out: ContentId,
    reads: StdVec<(Name, ContentId)>,
    parts: StdVec<Part>,
}

enum Answered {
    Done(Made),
    Need(StdVec<Want>),
    Failed(TaskFailure),
}

fn made(name: Name, out: ContentId) -> Made {
    Made { name, out, reads: StdVec::new(), parts: StdVec::new() }
}

fn execute(world: &World, task: &Task<'static>) -> Result<Answered, Broke> {
    let name = task.name;
    match name.kind {
        Kind::Units => {
            let mut m = made(name, world.list_out());
            for (c, krate) in world.crates.iter().enumerate() {
                // The scope's source holds what its record is made from: its exports and its
                // link table, both of its crate's text (a `#[link]`, an `extern` block, a
                // `#[no_mangle]` item). Without the table here the scope's record lists its
                // `Links` part by a digest its key does not cover (rule 21): a table edit left
                // the scope standing on its old listing, so no `Links` and no `Native` ran.
                let source = key::digest("sim natives scope source", format!("{c} {} {}", krate.surface, krate.links).as_bytes());
                m.parts.push(Part { name: scope_name(c), locate: Locate::Source(source), place: 0 });
                if let Some(output) = krate.script {
                    let source = key::digest("sim natives script source", format!("{c} {output}").as_bytes());
                    m.parts.push(Part { name: script_name(c), locate: Locate::Source(source), place: 0 });
                }
            }
            m.parts.sort_by_key(|part| part.name);
            Ok(Answered::Done(m))
        }
        Kind::CrateScope => {
            let c = world.of_name(name, scope_name).ok_or(Broke::Harness(format!("no crate for {name}")))?;
            let mut m = made(name, key::digest("sim natives scope out", format!("{c} {}", world.crates[c].surface).as_bytes()));
            m.parts.push(Part { name: links_name(c), locate: Locate::Source(world.links_source(c)), place: 0 });
            Ok(Answered::Done(m))
        }
        Kind::Script => {
            let c = world.of_name(name, script_name).ok_or(Broke::Harness(format!("no script for {name}")))?;
            let output = world.crates[c].script.ok_or(Broke::Harness(format!("{name} ran for a crate with no script")))?;
            Ok(Answered::Done(made(name, key::digest("sim natives script out", format!("{output}").as_bytes()))))
        }
        // The table's bytes are in its home's record: the task is handed it, copies the table,
        // and answers the digest it was located by. Valid by its key: no read.
        Kind::Links => {
            if task.home.kind != Kind::CrateScope {
                return Err(Broke::Oracle(format!("{name} is homed at {}, not its crate's scope", task.home)));
            }
            if !task.inputs.iter().any(|input| input.name == task.home) {
                return Ok(Answered::Need(std::vec![Want { name: task.home, home: Name::units() }]));
            }
            Ok(Answered::Done(made(name, task.source)))
        }
        Kind::Native => {
            if name != native_name() {
                return Err(Broke::Oracle(format!("a native image of another program ran: {name}")));
            }
            let list = Name::units();
            let Some(listed) = task.inputs.iter().find(|input| input.name == list) else {
                return Ok(Answered::Need(std::vec![Want { name: list, home: list }]));
            };
            let mut reads = std::vec![(list, listed.out)];
            let mut wants = StdVec::new();
            for c in world.closure() {
                let mut reads_of = StdVec::new();
                if world.bug != Bug::RootLinksOnly || c == 0 {
                    reads_of.push((links_name(c), scope_name(c)));
                }
                if world.crates[c].script.is_some() {
                    reads_of.push((script_name(c), Name::units()));
                }
                for (read, home) in reads_of {
                    if task.failed.contains(&read) {
                        return Ok(Answered::Failed(TaskFailure { name, why: Why::Waited { on: read } }));
                    }
                    match task.inputs.iter().find(|input| input.name == read) {
                        Some(held) => reads.push((read, held.out)),
                        None => wants.push(Want { name: read, home }),
                    }
                }
            }
            if !wants.is_empty() {
                return Ok(Answered::Need(wants));
            }
            reads.sort();
            reads.dedup();
            let outs: StdVec<ContentId> = reads.iter().map(|(_, out)| *out).collect();
            let mut m = made(name, key::digest("sim natives image", format!("{outs:?}").as_bytes()));
            m.reads = reads;
            Ok(Answered::Done(m))
        }
        kind => Err(Broke::Harness(format!("the natives sim has no {} task", kind.name()))),
    }
}

// --- One request, driven ----------------------------------------------------------------------

#[derive(Default)]
struct Ended {
    answer: Option<Answer>,
    /// Every name a batch ran this request (a re-entry after a `Need` is the same name).
    ran: BTreeSet<Name>,
    last: Option<StdVec<u8>>,
    /// The keys the program started from (`Effect::StartProgram`), when it started.
    started: Option<(Key, Key)>,
    /// The request's certificate key.
    cert: Key,
}

impl Ended {
    fn ran_of(&self, kind: Kind) -> usize {
        self.ran.iter().filter(|name| name.kind == kind).count()
    }

    fn natives_found(&self) -> u32 {
        self.answer.as_ref().map_or(0, |answer| answer.profile.kinds[Kind::Native.index()].found)
    }
}

fn never_an_error(_: u16) -> bool {
    false
}

fn config(workers: u32) -> Config {
    let tools = Kind::ALL.map(|kind| Tool(key::digest("sim natives kind tool", &[kind as u8])));
    Config { tool: Tool(key::digest("sim natives tool", b"")), tools, workers, most_entries: 32, most_jobs: 1 << 16, is_error: never_an_error }
}

/// `run` of the root's program, its check done before it starts.
fn ask() -> Ask<'static> {
    Ask { command: Command::Run, args: b"", gate: Gate::Before, space: Space::NONE, program: Some(native_name()), facts: &[] }
}

/// One `run` over `store`, walking forward from `last`; `cold`: no leaf table (another
/// worktree's first command), so no certificate is looked up.
fn request(tape: &Tape, world: &World, store: &mut BTreeMap<Key, StdVec<u8>>, last: Option<&[u8]>, cold: bool) -> Result<Ended, Broke> {
    let manifest = b"Cargo.toml".to_vec();
    let files = [FileEntry { path: &manifest, id: world.units_source(), changed: false, manifest: true }];
    let workers = [1, 2, 16][tape.index(3, "workers")];
    let mut req = Request::planted_in(config(workers), ask(), Plant::None, blaze_own::testing::heap().alloc());
    let mut out: Vec<Effect<'static>, Alloc<'static>> = Vec::new_in(blaze_own::testing::heap().alloc());
    let mut ended = Ended::default();
    let mut gets: StdVec<Key> = StdVec::new();
    let mut batches: StdVec<(BatchId, StdVec<(Ticket, Task<'static>)>)> = StdVec::new();
    // The program started and not yet ended: its exit is one more completion to pick.
    let mut running = false;
    let mut first = Some(Event::Start { files: &files, gone: &[], last, units: world.units_source(), cold, graph: None });
    for _ in 0..MOST_EVENTS {
        let stepped = match first.take() {
            Some(event) => req.step(event, &mut out),
            None => {
                let total = gets.len() + batches.len() + usize::from(running);
                if total == 0 {
                    return Err(Broke::Oracle(String::from("the request stalled: nothing out and no answer")));
                }
                let pick = tape.index(total, "which completes");
                if pick < gets.len() {
                    let key = gets.swap_remove(pick);
                    let bytes = store.get(&key).cloned();
                    req.step(Event::Got { key, bytes: bytes.as_deref() }, &mut out)
                } else if pick < gets.len() + batches.len() {
                    let (id, tasks) = batches.swap_remove(pick - gets.len());
                    let mut answered = StdVec::new();
                    for (ticket, task) in tasks.iter() {
                        answered.push((*ticket, execute(world, task)?));
                    }
                    let no_findings: [Finding<'_>; 0] = [];
                    let no_paths: [PathOf<'_>; 0] = [];
                    let outputs: StdVec<Output<'_>> = answered
                        .iter()
                        .filter_map(|(_, answer)| match answer {
                            Answered::Done(m) => Some(Output { name: m.name, out: m.out, reads: &m.reads, cells: &[], parts: &m.parts, paths: &no_paths, findings: &no_findings, payload: b"" }),
                            _ => None,
                        })
                        .collect();
                    let mut at = 0;
                    let mut results = StdVec::new();
                    for (ticket, answer) in answered.iter() {
                        let ran = match answer {
                            Answered::Need(wants) => Ran::Need(wants),
                            Answered::Failed(failure) => Ran::Failed(*failure),
                            Answered::Done(_) => {
                                at += 1;
                                Ran::Done(&outputs[at - 1..at])
                            }
                        };
                        results.push(Outcome { ticket: *ticket, ran, in_place: 0, ns: 1_000 });
                    }
                    req.step(Event::Ran { batch: id, results: &results }, &mut out)
                } else {
                    running = false;
                    req.step(Event::Exited { ok: true }, &mut out)
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
                    Some(held) if held.as_slice() != bytes.as_slice() => {
                        let what = match record::open(held) {
                            Ok(Stored::Shape(shape)) => format!("a shape of {}", shape.name),
                            Ok(Stored::Whole(view)) => format!("a record of {}", view.name),
                            Err(error) => format!("{error}"),
                        };
                        return Err(Broke::Oracle(format!("two values under one key {key}, {what} (rule 21)")));
                    }
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
                Effect::StartProgram { units, native } => {
                    if running {
                        return Err(Broke::Oracle(String::from("a second program started")));
                    }
                    // Under `Before` the check, the image among it, is done when it starts, and
                    // the effect carries the keys the runner starts from: the image's settled
                    // record and the units'. A certified repeat walks nothing: its keys are the
                    // certificate's, checked against the walked command's by `certified_repeat`.
                    match req.settled_input(native_name()) {
                        None if !req.profile().certified => {
                            return Err(Broke::Oracle(String::from("the program started with no native image settled")));
                        }
                        Some(input) if input.at != native => {
                            return Err(Broke::Oracle(format!("the program started from {native}, not its image's record {}", input.at)));
                        }
                        _ => {}
                    }
                    if req.settled_input(Name::units()).is_some_and(|input| input.at != units) {
                        return Err(Broke::Oracle(format!("the program started from units {units}, not the units' record")));
                    }
                    ended.started = Some((units, native));
                    running = true;
                }
                Effect::Stop { .. } => return Err(Broke::Oracle(String::from("a clean world's program was stopped"))),
                Effect::Publish(_) | Effect::Unpublish(_) | Effect::Print(_) => {}
                Effect::WriteLast { bytes } => ended.last = Some(bytes.as_slice().to_vec()),
                Effect::Done(answer) => ended.answer = Some(answer),
                Effect::Open { name, .. } => return Err(Broke::Harness(format!("{name} was opened, and the sim has no files"))),
            }
        }
        if ended.answer.is_some() {
            if !gets.is_empty() || !batches.is_empty() || running {
                return Err(Broke::Oracle(String::from("answered with work still out")));
            }
            if !req.failures().is_empty() {
                return Err(Broke::Oracle(format!("tasks failed: {:?}", req.failures())));
            }
            ended.cert = req.certificate();
            return Ok(ended);
        }
    }
    Err(Broke::Harness(format!("more than {MOST_EVENTS} events")))
}

/// Crate 0 the root, depending on crate 1; crates 1..n a chain the tape draws, some with a
/// script; one last crate nothing reaches.
fn draw(tape: &Tape, bug: Bug) -> World {
    let n = 2 + tape.index(3, "crates in the closure");
    let mut crates: StdVec<Crate> = (0..n)
        .map(|c| Crate {
            deps: if c + 1 < n { std::vec![c + 1] } else { StdVec::new() },
            links: 10 + c as u64,
            surface: 100 + c as u64,
            script: (tape.index(2, "a script") == 1).then_some(1000 + c as u64),
        })
        .collect();
    crates.push(Crate { deps: StdVec::new(), links: 7, surface: 70, script: None });
    World { crates, bug }
}

fn caught(bug: Bug, words: &str, case: fn(&Tape, Bug) -> Result<(), Broke>) {
    control(SimConfig::cases(300), Bug::None, bug, words, case);
}

// --- Case 1: a closure crate's link table edited reruns the image -----------------------------

fn a_changed_closure_table(tape: &Tape, bug: Bug) -> Result<(), Broke> {
    let before = draw(tape, bug);
    let unreached = before.crates.len() - 1;
    // A crate of the closure other than the root: its table is read only through the closure.
    let edited_at = 1 + tape.index(unreached - 1, "which closure crate");
    let mut after = before.clone();
    after.crates[edited_at].links += 1;
    let mut store = BTreeMap::new();
    let first = request(tape, &before, &mut store, None, false)?;
    if first.ran_of(Kind::Native) != 1 {
        return Err(Broke::Oracle(format!("a first run ran {} native images", first.ran_of(Kind::Native))));
    }
    let second = request(tape, &after, &mut store, first.last.as_deref(), false)?;
    if second.ran_of(Kind::Native) != 1 {
        return Err(Broke::Oracle(format!("a changed link table left the program's native image standing (crate {edited_at} of the closure)")));
    }
    if second.ran.iter().any(|name| name.kind == Kind::Links && *name != links_name(edited_at)) {
        return Err(Broke::Oracle(String::from("a table edit ran another crate's Links")));
    }
    // A crate the root does not reach: its table is no input of the image.
    let mut third_world = after.clone();
    third_world.crates[unreached].links += 1;
    let third = request(tape, &third_world, &mut store, second.last.as_deref(), false)?;
    if third.ran_of(Kind::Native) != 0 {
        return Err(Broke::Oracle(String::from("an unreached crate's table edit reran the program's native image")));
    }
    Ok(())
}

/// Expected: "test sim_natives::a_native_whose_closure_crates_links_changed_is_rerun ... ok".
#[test]
fn a_native_whose_closure_crates_links_changed_is_rerun() {
    check(SimConfig::cases(200), |tape| a_changed_closure_table(tape, Bug::None));
}

#[test]
fn control_an_image_reading_only_its_roots_links_is_caught() {
    caught(Bug::RootLinksOnly, "a changed link table left the program's native image standing", a_changed_closure_table);
}

// --- Case 2: nothing the image reads changed: found by key, nothing runs ----------------------

fn nothing_changed(tape: &Tape, bug: Bug) -> Result<(), Broke> {
    let before = draw(tape, bug);
    let edited_at = tape.index(before.crates.len(), "whose exports");
    let mut after = before.clone();
    after.crates[edited_at].surface += 1;
    let mut store = BTreeMap::new();
    let first = request(tape, &before, &mut store, None, false)?;
    let second = request(tape, &after, &mut store, first.last.as_deref(), false)?;
    if second.ran_of(Kind::Links) != 0 || second.ran_of(Kind::Native) != 0 {
        return Err(Broke::Oracle(format!(
            "an edit that left every link table equal ran {} Links and {} Native",
            second.ran_of(Kind::Links),
            second.ran_of(Kind::Native)
        )));
    }
    // Another worktree's first command over the same store: no row, so the image is found by
    // its value key from its reads, and nothing is linked.
    let cold = request(tape, &after, &mut store, None, true)?;
    if cold.ran_of(Kind::Native) != 0 || cold.ran_of(Kind::Links) != 0 || cold.natives_found() != 1 {
        return Err(Broke::Oracle(format!(
            "a cold repeat ran {} Native and {} Links and found {} images by key",
            cold.ran_of(Kind::Native),
            cold.ran_of(Kind::Links),
            cold.natives_found()
        )));
    }
    Ok(())
}

/// Expected: "test sim_natives::a_native_whose_reads_are_unchanged_is_found_by_key_and_runs_nothing ... ok".
#[test]
fn a_native_whose_reads_are_unchanged_is_found_by_key_and_runs_nothing() {
    check(SimConfig::cases(200), |tape| nothing_changed(tape, Bug::None));
}

#[test]
fn control_links_listed_by_a_source_folding_the_exports_is_caught() {
    caught(Bug::LinksSourceFoldsScope, "an edit that left every link table equal ran", nothing_changed);
}

// --- Case 3: a certified repeat starts from both keys -----------------------------------------

/// Clears the native slot's flag in the certificate `bytes` (request.rs `write_cert`: the root,
/// then per slot a `u32` flag and a 32-byte key, units first): the four bytes before the key
/// `native`, which the walked command wrote there.
fn drop_native_slot(bytes: &mut [u8], native: Key) -> Result<(), Broke> {
    let at = bytes
        .windows(32)
        .position(|window| window == native.0.0.as_slice())
        .ok_or(Broke::Harness(format!("the certificate does not hold the image key {native}")))?;
    if at < 4 {
        return Err(Broke::Harness(String::from("the image key opens the certificate: no flag before it")));
    }
    bytes[at - 4..at].fill(0);
    Ok(())
}

fn certified_repeat(tape: &Tape, bug: Bug) -> Result<(), Broke> {
    let world = draw(tape, Bug::None);
    let mut store = BTreeMap::new();
    let first = request(tape, &world, &mut store, None, false)?;
    let walked = first.started.ok_or(Broke::Oracle(String::from("the walked run started no program")))?;
    if bug == Bug::NativeSlotDropped {
        let held = store.get_mut(&first.cert).ok_or(Broke::Harness(format!("no certificate at {}", first.cert)))?;
        drop_native_slot(held, walked.1)?;
    }
    let again = request(tape, &world, &mut store, first.last.as_deref(), false)?;
    let certified = again.answer.is_some_and(|answer| answer.profile.certified);
    if !certified || !again.ran.is_empty() {
        return Err(Broke::Oracle(format!("an unchanged repeat was not certified ({certified}) or ran {} tasks", again.ran.len())));
    }
    match again.started {
        Some(keys) if keys == walked => Ok(()),
        Some((units, native)) => Err(Broke::Oracle(format!(
            "the certified repeat started from units {units} and image {native}, not the walked run's {} and {}",
            walked.0, walked.1
        ))),
        None => Err(Broke::Oracle(String::from("the certified repeat started no program"))),
    }
}

/// A program whose certificate is valid starts with both keys, the walked run's.
/// Expected: "test sim_natives::a_certified_repeat_starts_from_the_units_and_the_image ... ok".
#[test]
fn a_certified_repeat_starts_from_the_units_and_the_image() {
    check(SimConfig::cases(200), |tape| certified_repeat(tape, Bug::None));
}

/// The native slot absent: the repeat refuses, naming the program (`NoNativeImage`).
/// Expected: "test sim_natives::control_a_certificate_with_no_native_slot_refuses_by_name ... ok".
#[test]
fn control_a_certificate_with_no_native_slot_refuses_by_name() {
    caught(Bug::NativeSlotDropped, "the certificate holds no native image for it", certified_repeat);
}
