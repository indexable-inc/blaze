//! Build scripts as drive tasks, simulated (drive.md, "Build scripts in the command", rulings
//! (1) to (3) of 19:48 and 20:33). A toy world of crates, each built for the target side, the
//! macro side or both; a crate with a build script has one `Script` run a side, named by the
//! side (ruling (3)), listed as a part of `Units` by its source (the script's text). A
//! `CrateScope` of a side Needs its own side's `Script` and joins its out at task time (ruling
//! (1): the script's output is a read, never part of a key). A script's out is the cfg it sets
//! for that side, never its text. Cases, each with the bug it plants (a plant is the cli's side
//! of the seam misbehaving: which runs a reader names, what a run's out holds):
//! - an edited script reruns only its unit's readers (plant: every scope reads every script of
//!   its side);
//! - a script edited to equal output moves nobody (plant: the out folds the script's text);
//! - a macro-side copy sees the cfg its script set for the macro side, never one set only for the
//!   target side (plant: the macro side given the target side's record);
//! - a unit on both sides gets two runs (plant: the run named without its side).
//!
//! Every test's expected line is "test sim_scripts::<name> ... ok"; each control passes when its
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
use crate::record::{self, Finding, Locate, Part, PathOf, Stored};
use crate::request::{
    Answer, Ask, BatchId, Command, Config, Effect, Event, FileEntry, Gate, Outcome, Output, Ran, Request, Task,
    TaskFailure, Ticket, Want, Why,
};

const MOST_EVENTS: u32 = 100_000;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Side {
    Target = 0,
    Macro = 1,
}

const SIDES: [Side; 2] = [Side::Target, Side::Macro];

#[derive(Clone, Debug, PartialEq, Eq)]
struct Script {
    /// Its text: in its run's source, never in its out.
    text: u64,
    /// The cfg it sets, by side (`Side as usize`).
    cfg: [StdVec<u16>; 2],
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Crate {
    sides: StdVec<Side>,
    script: Option<Script>,
}

/// The cli's side of the seam misbehaving, one way a world.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Bug {
    #[default]
    None,
    /// Every scope reads every script of its side, not only its own unit's.
    ReadsAll,
    /// A run's out folds the script's text: an edit to equal output moves its readers.
    OutFoldsText,
    /// The macro side's scope reads the target side's run.
    MacroReadsTarget,
    /// A run is named without its side: a unit on both sides is one run.
    Sideless,
}

#[derive(Clone, Debug, Default)]
struct World {
    crates: StdVec<Crate>,
    bug: Bug,
}

fn scope_name(c: usize, side: Side) -> Name {
    Name::new(Kind::CrateScope, key::digest("sim scripts scope", format!("{c} {side:?}").as_bytes()))
}

impl World {
    fn script_name(&self, c: usize, side: Side) -> Name {
        let side = if self.bug == Bug::Sideless { String::new() } else { format!("{side:?}") };
        Name::new(Kind::Script, key::digest("sim scripts run", format!("{c} {side}").as_bytes()))
    }

    /// The (crate, side) a `Script` name runs for: under `Sideless`, the target side's.
    fn script_of(&self, name: Name) -> Option<(usize, Side)> {
        (0..self.crates.len()).find_map(|c| SIDES.into_iter().find(|side| self.script_name(c, *side) == name).map(|side| (c, side)))
    }

    fn scope_of(&self, name: Name) -> Option<(usize, Side)> {
        (0..self.crates.len()).find_map(|c| SIDES.into_iter().find(|side| scope_name(c, *side) == name).map(|side| (c, side)))
    }

    /// The source of crate `c`'s run for `side`: its script's text, and its side.
    fn script_source(&self, c: usize, side: Side, script: &Script) -> ContentId {
        let side = if self.bug == Bug::Sideless { String::new() } else { format!("{side:?}") };
        key::digest("sim scripts run source", format!("{c} {side} {}", script.text).as_bytes())
    }

    /// The out a run of crate `c`'s script for `side` gives: the cfg it sets for that side.
    fn script_out(&self, c: usize, side: Side) -> Option<ContentId> {
        let script = self.crates[c].script.as_ref()?;
        let text = if self.bug == Bug::OutFoldsText { script.text } else { 0 };
        Some(key::digest("sim scripts run out", format!("{:?} {text}", script.cfg[side as usize]).as_bytes()))
    }

    /// The runs the scope of crate `c` on `side` reads, by (crate, side).
    fn reads_of(&self, c: usize, side: Side) -> StdVec<(usize, Side)> {
        match self.bug {
            Bug::ReadsAll => (0..self.crates.len()).filter(|d| self.crates[*d].script.is_some() && self.crates[*d].sides.contains(&side)).map(|d| (d, side)).collect(),
            _ if self.crates[c].script.is_none() => StdVec::new(),
            Bug::MacroReadsTarget => std::vec![(c, Side::Target)],
            _ => std::vec![(c, side)],
        }
    }

    fn units_source(&self) -> ContentId {
        key::digest("sim scripts units", format!("{:?}", self.crates).as_bytes())
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
            let mut m = made(name, key::digest("sim scripts units out", b""));
            for (c, krate) in world.crates.iter().enumerate() {
                for side in krate.sides.iter().copied() {
                    let source = key::digest("sim scripts scope source", format!("{c} {side:?}").as_bytes());
                    m.parts.push(Part { name: scope_name(c, side), locate: Locate::Source(source), place: 0 });
                    if let Some(script) = &krate.script {
                        m.parts.push(Part { name: world.script_name(c, side), locate: Locate::Source(world.script_source(c, side, script)), place: 0 });
                    }
                }
            }
            m.parts.sort_by_key(|part| part.name);
            m.parts.dedup_by_key(|part| part.name);
            Ok(Answered::Done(m))
        }
        Kind::Script => {
            let (c, side) = world.script_of(name).ok_or(Broke::Harness(format!("no script for {name}")))?;
            let out = world.script_out(c, side).ok_or(Broke::Harness(format!("{name} ran for a crate with no script")))?;
            Ok(Answered::Done(made(name, out)))
        }
        Kind::CrateScope => {
            let (c, side) = world.scope_of(name).ok_or(Broke::Harness(format!("no crate for {name}")))?;
            let mut reads = StdVec::new();
            let mut wants = StdVec::new();
            for (d, read_side) in world.reads_of(c, side) {
                let run = world.script_name(d, read_side);
                if task.failed.contains(&run) {
                    return Ok(Answered::Failed(TaskFailure { name, why: Why::Waited { on: run } }));
                }
                match task.inputs.iter().find(|input| input.name == run) {
                    Some(held) => reads.push((run, held.out)),
                    None => wants.push(Want { name: run, home: Name::units() }),
                }
            }
            if !wants.is_empty() {
                return Ok(Answered::Need(wants));
            }
            reads.sort();
            reads.dedup();
            let outs: StdVec<ContentId> = reads.iter().map(|(_, out)| *out).collect();
            let mut m = made(name, key::digest("sim scripts scope out", format!("{c} {side:?} {outs:?}").as_bytes()));
            m.reads = reads;
            Ok(Answered::Done(m))
        }
        kind => Err(Broke::Harness(format!("the scripts sim has no {} task", kind.name()))),
    }
}

// --- One request, driven ----------------------------------------------------------------------

#[derive(Default)]
struct Ended {
    answer: Option<Answer>,
    /// Every name a batch ran this request (a re-entry after a `Need` is the same name).
    ran: BTreeSet<Name>,
    /// Each `Script` entry this request, by name.
    script_runs: BTreeMap<Name, u32>,
    /// Each scope's reads as its last run this request made them.
    read: BTreeMap<Name, StdVec<(Name, ContentId)>>,
    last: Option<StdVec<u8>>,
    failures: StdVec<TaskFailure>,
}

impl Ended {
    fn scopes_ran(&self) -> BTreeSet<Name> {
        self.ran.iter().copied().filter(|name| name.kind == Kind::CrateScope).collect()
    }
}

fn never_an_error(_: u16) -> bool {
    false
}

fn config(workers: u32) -> Config {
    let tools = Kind::ALL.map(|kind| Tool(key::digest("sim scripts kind tool", &[kind as u8])));
    Config { tool: Tool(key::digest("sim scripts tool", b"")), tools, workers, most_entries: 32, most_jobs: 1 << 16, is_error: never_an_error }
}

const ASK: Ask<'static> = Ask { command: Command::Check, args: b"", gate: Gate::Beside, space: Space::NONE, program: None, facts: &[] };

fn request(tape: &Tape, world: &World, store: &mut BTreeMap<Key, StdVec<u8>>, last: Option<&[u8]>) -> Result<Ended, Broke> {
    let manifest = b"Cargo.toml".to_vec();
    let files = [FileEntry { path: &manifest, id: world.units_source(), changed: false, manifest: true }];
    let workers = [1, 2, 16][tape.index(3, "workers")];
    let mut req = Request::planted_in(config(workers), ASK, Plant::None, blaze_own::testing::heap().alloc());
    let mut out: Vec<Effect<'static>, Alloc<'static>> = Vec::new_in(blaze_own::testing::heap().alloc());
    let mut ended = Ended::default();
    let mut gets: StdVec<Key> = StdVec::new();
    let mut batches: StdVec<(BatchId, StdVec<(Ticket, Task<'static>)>)> = StdVec::new();
    let mut first = Some(Event::Start { files: &files, gone: &[], last, units: world.units_source(), cold: false, graph: None });
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
                            Answered::Done(m) => {
                                if m.name.kind == Kind::CrateScope {
                                    ended.read.insert(m.name, m.reads.clone());
                                }
                                at += 1;
                                Ran::Done(&outputs[at - 1..at])
                            }
                        };
                        results.push(Outcome { ticket: *ticket, ran, in_place: 0, ns: 1_000 });
                    }
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
                        if task.name.kind == Kind::Script {
                            *ended.script_runs.entry(task.name).or_insert(0) += 1;
                        }
                        tasks.push((ticket, task));
                    }
                    batches.push((batch.id, tasks));
                }
                Effect::Publish(_) | Effect::Unpublish(_) | Effect::Print(_) => {}
                Effect::WriteLast { bytes } => ended.last = Some(bytes.as_slice().to_vec()),
                Effect::Done(answer) => ended.answer = Some(answer),
                Effect::Open { name, .. } => return Err(Broke::Harness(format!("{name} was opened, and the sim has no files"))),
                Effect::StartProgram { .. } | Effect::Stop { .. } => {
                    return Err(Broke::Harness(String::from("`check` started or stopped a program")));
                }
            }
        }
        if ended.answer.is_some() {
            if !gets.is_empty() || !batches.is_empty() {
                return Err(Broke::Oracle(String::from("answered with work still out")));
            }
            ended.failures = req.failures().to_vec();
            if !ended.failures.is_empty() {
                return Err(Broke::Oracle(format!("tasks failed: {:?}", ended.failures)));
            }
            return Ok(ended);
        }
    }
    Err(Broke::Harness(format!("more than {MOST_EVENTS} events")))
}

/// `before`, then `after` over the same store: the second request's ending.
fn edited(tape: &Tape, before: &World, after: &World) -> Result<Ended, Broke> {
    let mut store = BTreeMap::new();
    let first = request(tape, before, &mut store, None)?;
    request(tape, after, &mut store, first.last.as_deref())
}

/// A crate with a script on `sides`, setting `target` for the target side and `macro_side`
/// for the macro side.
fn with_script(sides: &[Side], text: u64, target: &[u16], macro_side: &[u16]) -> Crate {
    Crate { sides: sides.to_vec(), script: Some(Script { text, cfg: [target.to_vec(), macro_side.to_vec()] }) }
}

/// Two to four crates on the target side, each with a script setting a cfg of its own, and one
/// with none.
fn draw(tape: &Tape, bug: Bug) -> World {
    let n = 2 + tape.index(3, "crates with scripts");
    let mut crates: StdVec<Crate> = (0..n).map(|c| with_script(&[Side::Target], 100 + c as u64, &[c as u16], &[c as u16])).collect();
    crates.push(Crate { sides: std::vec![Side::Target], script: None });
    World { crates, bug }
}

/// A control: the clean world passes and the plant is caught with `words` (blaze-sim's
/// `control`, the bug being the cli's side of the seam).
fn caught(bug: Bug, words: &str, case: fn(&Tape, Bug) -> Result<(), Broke>) {
    control(SimConfig::cases(300), Bug::None, bug, words, case);
}

// --- Case 1: an edited script reruns only its unit's readers ---------------------------------

fn an_edited_script(tape: &Tape, bug: Bug) -> Result<(), Broke> {
    let before = draw(tape, bug);
    let edited_at = tape.index(before.crates.len() - 1, "which script");
    let mut after = before.clone();
    if let Some(script) = after.crates[edited_at].script.as_mut() {
        script.text += 1;
        script.cfg[Side::Target as usize].push(900);
    }
    let ended = edited(tape, &before, &after)?;
    let want: BTreeSet<Name> = [scope_name(edited_at, Side::Target)].into_iter().collect();
    if ended.scopes_ran() != want {
        return Err(Broke::Oracle(format!("an edited script reran other readers: crate {edited_at}'s edit ran {} scopes", ended.scopes_ran().len())));
    }
    let runs: u32 = ended.script_runs.values().sum();
    if ended.script_runs.len() != 1 || runs != 1 {
        return Err(Broke::Oracle(format!("an edited script ran {runs} runs of {} scripts", ended.script_runs.len())));
    }
    Ok(())
}

/// Expected: "test sim_scripts::an_edited_script_reruns_only_its_units_readers ... ok".
#[test]
fn an_edited_script_reruns_only_its_units_readers() {
    check(SimConfig::cases(200), |tape| an_edited_script(tape, Bug::None));
}

#[test]
fn control_a_scope_reading_every_script_is_caught() {
    caught(Bug::ReadsAll, "an edited script reran other readers", an_edited_script);
}

// --- Case 2: equal output moves nobody --------------------------------------------------------

fn an_equal_output(tape: &Tape, bug: Bug) -> Result<(), Broke> {
    let before = draw(tape, bug);
    let edited_at = tape.index(before.crates.len() - 1, "which script");
    let mut after = before.clone();
    if let Some(script) = after.crates[edited_at].script.as_mut() {
        script.text += 1;
    }
    let ended = edited(tape, &before, &after)?;
    if !ended.scopes_ran().is_empty() {
        return Err(Broke::Oracle(format!("a script edited to equal output moved {} readers", ended.scopes_ran().len())));
    }
    if ended.script_runs.len() != 1 {
        return Err(Broke::Oracle(format!("a script edited to equal output ran {} scripts, not its own", ended.script_runs.len())));
    }
    Ok(())
}

/// Expected: "test sim_scripts::a_script_edited_to_equal_output_moves_nobody ... ok".
#[test]
fn a_script_edited_to_equal_output_moves_nobody() {
    check(SimConfig::cases(200), |tape| an_equal_output(tape, Bug::None));
}

#[test]
fn control_an_out_folding_the_text_is_caught() {
    caught(Bug::OutFoldsText, "moved", an_equal_output);
}

// --- Case 3: the macro side sees its own side's cfg -------------------------------------------

/// Crate 0 on both sides: its script sets cfg 1 for both and cfg 2 for the target side only.
fn macro_side_cfg(tape: &Tape, bug: Bug) -> Result<(), Broke> {
    let mut world = draw(tape, bug);
    world.crates[0] = with_script(&[Side::Target, Side::Macro], 7, &[1, 2], &[1]);
    let ended = request(tape, &world, &mut BTreeMap::new(), None)?;
    for side in SIDES {
        let scope = scope_name(0, side);
        let reads = ended.read.get(&scope).ok_or(Broke::Oracle(format!("the {side:?} side's scope never ran")))?;
        let want = world.script_out(0, side).ok_or(Broke::Harness(String::from("crate 0 has no script")))?;
        if reads.len() != 1 || reads[0].1 != want {
            return Err(Broke::Oracle(format!("the {side:?} side's scope saw another side's cfg: it read {} runs", reads.len())));
        }
    }
    Ok(())
}

/// Expected: "test sim_scripts::a_macro_side_copy_sees_its_own_sides_cfg ... ok".
#[test]
fn a_macro_side_copy_sees_its_own_sides_cfg() {
    check(SimConfig::cases(200), |tape| macro_side_cfg(tape, Bug::None));
}

#[test]
fn control_the_macro_side_given_the_target_record_is_caught() {
    caught(Bug::MacroReadsTarget, "the Macro side's scope saw another side's cfg", macro_side_cfg);
}

// --- Case 4: a unit on both sides gets two runs -----------------------------------------------

fn both_sides(tape: &Tape, bug: Bug) -> Result<(), Broke> {
    let mut world = draw(tape, bug);
    let both = tape.index(world.crates.len() - 1, "which crate on both sides");
    world.crates[both] = with_script(&[Side::Target, Side::Macro], 9, &[1, 2], &[1]);
    let ended = request(tape, &world, &mut BTreeMap::new(), None)?;
    let runs: BTreeSet<Name> = ended.script_runs.keys().copied().filter(|name| SIDES.iter().any(|side| world.script_name(both, *side) == *name)).collect();
    if runs.len() != 2 {
        return Err(Broke::Oracle(format!("a unit on both sides got {} runs", runs.len())));
    }
    Ok(())
}

/// Expected: "test sim_scripts::a_unit_on_both_sides_gets_two_runs ... ok".
#[test]
fn a_unit_on_both_sides_gets_two_runs() {
    check(SimConfig::cases(200), |tape| both_sides(tape, Bug::None));
}

#[test]
fn control_a_run_named_without_its_side_is_caught() {
    caught(Bug::Sideless, "a unit on both sides got 1 runs", both_sides);
}
