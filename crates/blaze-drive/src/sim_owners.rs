//! The owners fact and body demand, simulated (drive.md, "RUNNER 19"; the coordinator's rulings
//! of 2026-11-16 20:30 and after). A toy checkout of crates (members under `Units`, dependencies
//! under the `UnitGraph`), files that list their `mod` files, and bodies that ask their file's
//! `Owners`, then the `CrateScope`s of those owners and their dependencies. Cases, each with the
//! bug it plants:
//! - a file with two parents keeps both owners (plant: the last parent wins);
//! - a new `mod x;` in another crate gives x a new owner, and its bodies rerun under it (plant:
//!   an added part does not dirty its owners);
//! - a removed parent drops its owner while the file stays, and a file a parent reaches that no
//!   root owns is named, its bodies unchecked (plant: a part removed from one parent is dropped
//!   though another lists it);
//! - one crate's scope fails: the bodies of crates not depending on it are checked and printed,
//!   and only the bodies its crates own fail (plant: the last parent wins);
//! - a dependency's body runs only when a workspace body reads it (plant: every part a job).

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
use crate::owners::{self, Root};
use crate::record::{self, Finding, Locate, Part, PathOf, Stored};
use crate::request::{
    Answer, Ask, BatchId, Command, Config, Effect, Event, FileEntry, Gate, OWNERLESS_RULE, Outcome, Output,
    Ran, Request, Task, TaskFailure, Ticket, Want, Why,
};

const EDITION: u8 = 3;
/// A body whose check finds an error: an error under the sim's policy.
const BAD_RULE: u16 = 7;
const MOST_EVENTS: u32 = 100_000;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct File {
    /// The files this one lists as `mod` (or `#[path]`) parts.
    mods: StdVec<usize>,
    /// Its bodies' values.
    bodies: StdVec<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Crate {
    root: usize,
    /// A workspace member (a `Units` part) or a dependency (a `UnitGraph` part).
    member: bool,
    deps: StdVec<usize>,
    /// Its scope fails (rule 20).
    fails: bool,
    /// Files its scope record lists itself (a file reached by no `mod`).
    scope_files: StdVec<usize>,
}

#[derive(Clone, Debug, Default)]
struct World {
    files: StdVec<File>,
    crates: StdVec<Crate>,
    /// A body that reads another body's fact (a hidden type): (file, body) reads (file, body).
    reads: StdVec<((usize, usize), (usize, usize))>,
    /// Bodies whose check finds an error.
    bad: BTreeSet<(usize, usize)>,
}

fn path(f: usize) -> StdVec<u8> {
    format!("src/f{f}.rs").into_bytes()
}

fn file_name(f: usize) -> Name {
    Name::file(&path(f))
}

fn body_name(f: usize, i: usize) -> Name {
    Name::new(Kind::BodyCheck, key::digest("sim owners body", format!("{f} {i}").as_bytes()))
}

fn scope_name(c: usize) -> Name {
    Name::new(Kind::CrateScope, key::digest("sim owners scope", format!("{c}").as_bytes()))
}

impl World {
    fn id(&self, f: usize) -> ContentId {
        key::digest("sim owners file", format!("{f} {:?}", self.files[f]).as_bytes())
    }

    fn file_of(&self, name: Name) -> Option<usize> {
        (0..self.files.len()).find(|f| file_name(*f) == name)
    }

    fn body_of(&self, name: Name) -> Option<(usize, usize)> {
        (0..self.files.len()).find_map(|f| (0..self.files[f].bodies.len()).find(|i| body_name(f, *i) == name).map(|i| (f, i)))
    }

    fn crate_of_root(&self, root: Name) -> Option<usize> {
        self.crates.iter().position(|c| file_name(c.root) == root)
    }

    fn scope_of(&self, name: Name) -> Option<usize> {
        (0..self.crates.len()).find(|c| scope_name(*c) == name)
    }

    /// The crates whose module tree reaches `f`: the truth the machine's owners are held to.
    fn owners(&self, f: usize) -> BTreeSet<usize> {
        let mut found = BTreeSet::new();
        for (c, krate) in self.crates.iter().enumerate() {
            let mut work = std::vec![krate.root];
            let mut seen = BTreeSet::new();
            while let Some(at) = work.pop() {
                if !seen.insert(at) {
                    continue;
                }
                if at == f {
                    found.insert(c);
                }
                work.extend(self.files[at].mods.iter().copied());
            }
        }
        found
    }

    /// `crates` and every crate they depend on, transitively.
    fn closure(&self, crates: &BTreeSet<usize>) -> BTreeSet<usize> {
        let mut all = BTreeSet::new();
        let mut work: StdVec<usize> = crates.iter().copied().collect();
        while let Some(c) = work.pop() {
            if all.insert(c) {
                work.extend(self.crates[c].deps.iter().copied());
            }
        }
        all
    }

    fn units_source(&self) -> ContentId {
        key::digest("sim owners units", format!("{:?}", self.crates.iter().filter(|c| c.member).collect::<StdVec<_>>()).as_bytes())
    }

    fn graph_source(&self) -> Option<ContentId> {
        let deps: StdVec<&Crate> = self.crates.iter().filter(|c| !c.member).collect();
        (!deps.is_empty()).then(|| key::digest("sim owners graph", format!("{deps:?}").as_bytes()))
    }
}

// --- The fake executor ------------------------------------------------------------------------

struct Made {
    name: Name,
    out: ContentId,
    reads: StdVec<(Name, ContentId)>,
    parts: StdVec<Part>,
    paths: StdVec<(Name, StdVec<u8>)>,
    findings: StdVec<(u16, StdVec<u8>)>,
    /// A body's owners as it read them.
    roots: Option<StdVec<Root>>,
}

enum Answered {
    Done(Made),
    Need(StdVec<Want>),
    Failed(TaskFailure),
}

fn made(name: Name, out: ContentId) -> Made {
    Made { name, out, reads: StdVec::new(), parts: StdVec::new(), paths: StdVec::new(), findings: StdVec::new(), roots: None }
}

fn crate_parts(world: &World, member: bool, name: Name) -> Made {
    let mut m = made(name, key::digest("sim owners crates", format!("{member} {:?}", world.crates).as_bytes()));
    for (c, krate) in world.crates.iter().enumerate().filter(|(_, k)| k.member == member) {
        m.parts.push(Part { name: file_name(krate.root), locate: Locate::File { edition: EDITION }, place: 0 });
        m.paths.push((file_name(krate.root), path(krate.root)));
        let source = key::digest("sim owners scope source", format!("{c} {krate:?}").as_bytes());
        m.parts.push(Part { name: scope_name(c), locate: Locate::Source(source), place: 0 });
    }
    m.parts.sort_by_key(|part| part.name);
    m
}

fn execute(world: &World, store: &BTreeMap<Key, StdVec<u8>>, task: &Task<'static>) -> Result<Answered, Broke> {
    let name = task.name;
    let input = |want: Name| task.inputs.iter().find(|input| input.name == want).copied();
    match name.kind {
        Kind::Units => Ok(Answered::Done(crate_parts(world, true, name))),
        Kind::UnitGraph => Ok(Answered::Done(crate_parts(world, false, name))),
        Kind::FileItems => {
            let f = world.file_of(name).ok_or(Broke::Harness(format!("no file for {name}")))?;
            let mut m = made(name, world.id(f));
            for child in world.files[f].mods.iter() {
                m.parts.push(Part { name: file_name(*child), locate: Locate::File { edition: EDITION }, place: *child as u32 });
                m.paths.push((file_name(*child), path(*child)));
            }
            for (i, value) in world.files[f].bodies.iter().enumerate() {
                let source = key::digest("sim owners body source", format!("{f} {i} {value}").as_bytes());
                m.parts.push(Part { name: body_name(f, i), locate: Locate::Source(source), place: i as u32 });
            }
            m.parts.sort_by_key(|part| part.name);
            m.parts.dedup_by_key(|part| part.name);
            Ok(Answered::Done(m))
        }
        Kind::CrateScope => {
            let c = world.scope_of(name).ok_or(Broke::Harness(format!("no crate for {name}")))?;
            if world.crates[c].fails {
                return Ok(Answered::Failed(TaskFailure { name, why: Why::Refused { what: "a planted scope failure", rule: "sim" } }));
            }
            let mut m = made(name, key::digest("sim owners scope out", format!("{c}").as_bytes()));
            for f in world.crates[c].scope_files.iter() {
                m.parts.push(Part { name: file_name(*f), locate: Locate::File { edition: EDITION }, place: 0 });
                m.paths.push((file_name(*f), path(*f)));
            }
            m.parts.sort_by_key(|part| part.name);
            Ok(Answered::Done(m))
        }
        Kind::BodyCheck => {
            let (f, i) = world.body_of(name).ok_or(Broke::Harness(format!("no body for {name}")))?;
            let owners_name = owners::of(file_name(f));
            if task.failed.contains(&owners_name) {
                return Ok(Answered::Failed(TaskFailure { name, why: Why::Waited { on: owners_name } }));
            }
            let Some(held) = input(owners_name) else {
                return Ok(Answered::Need(std::vec![Want { name: owners_name, home: file_name(f) }]));
            };
            let bytes = store.get(&held.at).ok_or(Broke::Oracle(format!("{owners_name} handed with no record at {}", held.at)))?;
            let payload = match record::open(bytes) {
                Ok(Stored::Whole(view)) => view.payload,
                _ => return Err(Broke::Oracle(format!("{owners_name}'s record is not a whole record"))),
            };
            let roots: StdVec<Root> = owners::roots(payload).map_err(|e| Broke::Oracle(format!("{e}")))?.collect();
            let mut reads = std::vec![(owners_name, held.out)];
            if roots.is_empty() {
                // No root owns it: not checked (the `Owners` record names the file).
                let mut m = made(name, key::digest("sim owners unchecked", b""));
                m.reads = reads;
                m.roots = Some(roots);
                return Ok(Answered::Done(m));
            }
            let mut crates = BTreeSet::new();
            for root in roots.iter() {
                crates.insert(world.crate_of_root(root.file).ok_or(Broke::Oracle(format!("a root {} that is no crate's", root.file)))?);
            }
            let mut wants = StdVec::new();
            let mut outs = StdVec::new();
            for c in world.closure(&crates) {
                let scope = scope_name(c);
                if task.failed.contains(&scope) {
                    return Ok(Answered::Failed(TaskFailure { name, why: Why::Waited { on: scope } }));
                }
                let home = if world.crates[c].member { Name::units() } else { Name::unit_graph() };
                match input(scope) {
                    Some(held) => reads.push((scope, held.out)),
                    None => wants.push(Want { name: scope, home }),
                }
            }
            for (_, (g, j)) in world.reads.iter().filter(|(at, _)| *at == (f, i)) {
                let other = body_name(*g, *j);
                if task.failed.contains(&other) {
                    return Ok(Answered::Failed(TaskFailure { name, why: Why::Waited { on: other } }));
                }
                match input(other) {
                    Some(held) => {
                        reads.push((other, held.out));
                        outs.push(held.out);
                    }
                    None => wants.push(Want { name: other, home: file_name(*g) }),
                }
            }
            if !wants.is_empty() {
                return Ok(Answered::Need(wants));
            }
            reads.sort();
            let value = world.files[f].bodies[i];
            let mut m = made(name, key::digest("sim owners body out", format!("{value} {roots:?} {outs:?}").as_bytes()));
            m.reads = reads;
            if world.bad.contains(&(f, i)) {
                m.findings.push((BAD_RULE, format!("bad body {f} {i}").into_bytes()));
            }
            m.roots = Some(roots);
            Ok(Answered::Done(m))
        }
        kind => Err(Broke::Harness(format!("the owners sim has no {} task", kind.name()))),
    }
}

// --- One request, driven ----------------------------------------------------------------------

#[derive(Default)]
struct Ended {
    answer: Option<Answer>,
    runs: BTreeMap<Kind, u32>,
    /// (name, rule, message) of every print.
    prints: BTreeSet<(Name, u16, StdVec<u8>)>,
    last: Option<StdVec<u8>>,
    /// Each body's owners as its last run this request read them.
    seen: BTreeMap<Name, StdVec<Root>>,
    failures: StdVec<TaskFailure>,
}

fn is_bad(rule: u16) -> bool {
    rule == BAD_RULE
}

fn config(workers: u32) -> Config {
    let tools = Kind::ALL.map(|kind| Tool(key::digest("sim owners kind tool", &[kind as u8])));
    Config { tool: Tool(key::digest("sim owners tool", b"")), tools, workers, most_entries: 32, most_jobs: 1 << 16, is_error: is_bad }
}

const ASK: Ask<'static> = Ask { command: Command::Check, args: b"", gate: Gate::Beside, space: Space::NONE };

fn request(
    tape: &Tape,
    world: &World,
    before: Option<&World>,
    store: &mut BTreeMap<Key, StdVec<u8>>,
    last: Option<&[u8]>,
    plant: Plant,
) -> Result<Ended, Broke> {
    let paths: StdVec<StdVec<u8>> = (0..world.files.len()).map(path).collect();
    let mut files: StdVec<FileEntry<'_>> = (0..world.files.len())
        .map(|f| FileEntry {
            path: &paths[f],
            id: world.id(f),
            changed: before.is_none_or(|old| old.files.get(f).is_none_or(|_| old.id(f) != world.id(f))),
            manifest: false,
        })
        .collect();
    let manifest = b"Cargo.toml".to_vec();
    files.push(FileEntry { path: &manifest, id: world.units_source(), changed: false, manifest: true });
    files.sort_by(|a, b| a.path.cmp(b.path));
    let workers = [1, 2, 16][tape.index(3, "workers")];
    let mut req = Request::planted_in(config(workers), ASK, plant, blaze_own::testing::heap().alloc());
    let mut out: Vec<Effect<'static>, Alloc<'static>> = Vec::new_in(blaze_own::testing::heap().alloc());
    let mut ended = Ended::default();
    let mut gets: StdVec<Key> = StdVec::new();
    let mut batches: StdVec<(BatchId, StdVec<(Ticket, Task<'static>)>)> = StdVec::new();
    let mut first =
        Some(Event::Start { files: &files, gone: &[], last, units: world.units_source(), cold: false, graph: world.graph_source() });
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
                        answered.push((*ticket, task.name, execute(world, store, task)?));
                    }
                    let mut held: StdVec<(StdVec<Finding<'_>>, StdVec<PathOf<'_>>, &Made)> = StdVec::new();
                    for (_, _, answer) in answered.iter() {
                        if let Answered::Done(m) = answer {
                            let findings = m.findings.iter().map(|(rule, message)| Finding { rule: *rule, start: 0, end: 1, message }).collect();
                            let paths = m.paths.iter().map(|(name, path)| PathOf { name: *name, path, module: false }).collect();
                            held.push((findings, paths, m));
                        }
                    }
                    let outputs: StdVec<Output<'_>> = held
                        .iter()
                        .map(|(findings, paths, m)| Output {
                            name: m.name,
                            out: m.out,
                            reads: &m.reads,
                            parts: &m.parts,
                            paths,
                            findings,
                            payload: b"",
                        })
                        .collect();
                    let mut at = 0;
                    let mut results = StdVec::new();
                    for (ticket, name, answer) in answered.iter() {
                        let ran = match answer {
                            Answered::Need(wants) => Ran::Need(wants),
                            Answered::Failed(failure) => Ran::Failed(*failure),
                            Answered::Done(m) => {
                                if let Some(roots) = &m.roots {
                                    ended.seen.insert(*name, roots.clone());
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
                        *ended.runs.entry(task.name.kind).or_insert(0) += 1;
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
            return Ok(ended);
        }
    }
    Err(Broke::Harness(format!("more than {MOST_EVENTS} events")))
}

fn fresh(tape: &Tape, world: &World, plant: Plant) -> Result<Ended, Broke> {
    request(tape, world, None, &mut BTreeMap::new(), None, plant)
}

fn member(root: usize) -> Crate {
    Crate { root, member: true, deps: StdVec::new(), fails: false, scope_files: StdVec::new() }
}

fn file(mods: &[usize], bodies: usize) -> File {
    File { mods: mods.to_vec(), bodies: (0..bodies as u64).collect() }
}

/// The owners each body of `f` last read, as crate indices; the oracle's comparison form.
fn seen_crates(world: &World, ended: &Ended, f: usize) -> Result<BTreeSet<usize>, Broke> {
    let roots = ended.seen.get(&body_name(f, 0)).ok_or(Broke::Oracle(format!("body 0 of file {f} never ran")))?;
    roots.iter().map(|root| world.crate_of_root(root.file).ok_or(Broke::Oracle(format!("a root {} no crate has", root.file)))).collect()
}

// --- Case 1: two parents ----------------------------------------------------------------------

/// Crates 0 and 1 (members, roots files 0 and 1) both list file 2 as a module; file 2 lists file
/// 3. Both files' bodies read owners {0, 1}.
fn two_parents(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let world = World {
        files: std::vec![file(&[2], 0), file(&[2], 0), file(&[3], 1), file(&[], 1)],
        crates: std::vec![member(0), member(1)],
        ..World::default()
    };
    let ended = fresh(tape, &world, plant)?;
    for f in [2, 3] {
        let got = seen_crates(&world, &ended, f)?;
        if got != world.owners(f) {
            return Err(Broke::Oracle(format!("owners lost a parent: file {f} read {got:?}, its module tree gives {:?}", world.owners(f))));
        }
    }
    Ok(())
}

/// Expected: "test sim_owners::a_file_with_two_parents_keeps_both ... ok".
#[test]
fn a_file_with_two_parents_keeps_both() {
    check(SimConfig::cases(200), |tape| two_parents(tape, Plant::None));
}

#[test]
fn control_the_last_parent_winning_is_caught() {
    control(SimConfig::cases(500), Plant::None, Plant::OwnersLastParentWins, "owners lost a parent", two_parents);
}

// --- Case 2: a new `mod x;` -------------------------------------------------------------------

/// File 2 is crate 0's module; an edit adds `mod` of it to crate 1's root. Its bodies rerun under
/// owners {0, 1}, as a fresh build of the edited checkout gives.
fn a_new_mod(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let before = World {
        files: std::vec![file(&[2], 0), file(&[], 0), file(&[], 2)],
        crates: std::vec![member(0), member(1)],
        ..World::default()
    };
    let mut after = before.clone();
    after.files[1].mods.push(2);
    let mut store = BTreeMap::new();
    let first = request(tape, &before, None, &mut store, None, plant)?;
    let second = request(tape, &after, Some(&before), &mut store, first.last.as_deref(), plant)?;
    let got = seen_crates(&after, &second, 2).map_err(|_| Broke::Oracle(String::from("a body did not rerun under its new owner")))?;
    if got != after.owners(2) {
        return Err(Broke::Oracle(format!("a body did not rerun under its new owner: it read {got:?}")));
    }
    Ok(())
}

/// Expected: "test sim_owners::a_new_mod_gives_its_file_a_new_owner ... ok".
#[test]
fn a_new_mod_gives_its_file_a_new_owner() {
    check(SimConfig::cases(200), |tape| a_new_mod(tape, Plant::None));
}

#[test]
fn control_an_added_part_left_clean_is_caught() {
    control(SimConfig::cases(500), Plant::None, Plant::OwnersAddedNotDirtied, "did not rerun under its new owner", a_new_mod);
}

// --- Case 3: a parent removed -----------------------------------------------------------------

/// Before: crates 0 and 1 list file 2; crate 0's root lists file 4; crate 0's scope lists file 3
/// itself, and file 3 lists file 4. After: crate 1 no longer lists file 2 and crate 0's root no
/// longer lists file 4. File 2 stays, owned by crate 0 alone; file 4 is reached by a parent and
/// owned by no root: its `Owners` is named, its bad body is not checked. The prints equal a
/// fresh build's of the edited checkout.
fn a_removed_parent(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut zero = member(0);
    zero.scope_files.push(3);
    let before = World {
        files: std::vec![file(&[2, 4], 0), file(&[2], 0), file(&[], 1), file(&[4], 0), file(&[], 1)],
        crates: std::vec![zero, member(1)],
        bad: [(2, 0), (4, 0)].into_iter().collect(),
        ..World::default()
    };
    let mut after = before.clone();
    after.files[1].mods.clear();
    after.files[0].mods = std::vec![2];
    let mut store = BTreeMap::new();
    let first = request(tape, &before, None, &mut store, None, plant)?;
    let second = request(tape, &after, Some(&before), &mut store, first.last.as_deref(), plant)?;
    let clean = fresh(tape, &after, Plant::None)?;
    if second.prints != clean.prints {
        return Err(Broke::Oracle(format!("the prints differ from a fresh build: {:?} against {:?}", second.prints, clean.prints)));
    }
    let named: StdVec<Name> = clean.prints.iter().filter(|(_, rule, _)| *rule == OWNERLESS_RULE).map(|(name, _, _)| *name).collect();
    if named != [owners::of(file_name(4))] {
        return Err(Broke::Oracle(format!("the unowned file is not the one named: {named:?}")));
    }
    let got = seen_crates(&after, &second, 2).or_else(|_| seen_crates(&after, &first, 2))?;
    if got != after.owners(2) {
        return Err(Broke::Oracle(format!("the removed parent's owner stayed: file 2 read {got:?}")));
    }
    Ok(())
}

/// Expected: "test sim_owners::a_removed_parent_drops_its_owner_and_an_unowned_file_is_named ... ok".
#[test]
fn a_removed_parent_drops_its_owner_and_an_unowned_file_is_named() {
    check(SimConfig::cases(200), |tape| a_removed_parent(tape, Plant::None));
}

#[test]
fn control_a_part_dropped_while_listed_elsewhere_is_caught() {
    control(SimConfig::cases(500), Plant::None, Plant::OwnersDropListedElsewhere, "differ from a fresh build", a_removed_parent);
}

// --- Case 4: one failed scope -----------------------------------------------------------------

/// Crate 0's scope fails; crate 2 depends on crate 0; file 3 is a module of crates 0 and 1. The
/// bodies printed are exactly those whose owners' closure misses crate 0, and the bodies not run
/// (`Waited`) are exactly the rest.
fn one_failed_scope(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut zero = member(0);
    zero.fails = true;
    let mut two = member(2);
    two.deps.push(0);
    let files = std::vec![file(&[3], 1), file(&[3], 1), file(&[], 1), file(&[], 1)];
    let bad = (0..files.len()).map(|f| (f, 0)).collect();
    let world = World { files, crates: std::vec![zero, member(1), two], bad, ..World::default() };
    let ended = fresh(tape, &world, plant)?;
    let mut printed = BTreeSet::new();
    let mut blocked = BTreeSet::new();
    for f in 0..world.files.len() {
        match world.closure(&world.owners(f)).contains(&0) {
            true => blocked.insert(body_name(f, 0)),
            false => printed.insert(body_name(f, 0)),
        };
    }
    let got_printed: BTreeSet<Name> = ended.prints.iter().filter(|(_, rule, _)| *rule == BAD_RULE).map(|(name, _, _)| *name).collect();
    let got_blocked: BTreeSet<Name> = ended
        .failures
        .iter()
        .filter(|failure| failure.name.kind == Kind::BodyCheck && matches!(failure.why, Why::Waited { .. }))
        .map(|failure| failure.name)
        .collect();
    if got_printed != printed || got_blocked != blocked {
        return Err(Broke::Oracle(format!(
            "a failed scope blocked the wrong bodies: printed {got_printed:?} (want {printed:?}), not run {got_blocked:?} (want {blocked:?})"
        )));
    }
    Ok(())
}

/// Expected: "test sim_owners::one_failed_scope_blocks_only_the_bodies_its_crates_own ... ok".
#[test]
fn one_failed_scope_blocks_only_the_bodies_its_crates_own() {
    check(SimConfig::cases(200), |tape| one_failed_scope(tape, Plant::None));
}

#[test]
fn control_owners_from_one_parent_only_are_caught() {
    control(SimConfig::cases(500), Plant::None, Plant::OwnersLastParentWins, "blocked the wrong bodies", one_failed_scope);
}

// --- Case 5: demand ---------------------------------------------------------------------------

/// Crate 0 is a member (root file 0, two good bodies); crate 1 a dependency (root file 1, two bad
/// bodies). When the tape says, body 1 of file 0 reads body 0 of file 1's fact. `check` runs the
/// member's bodies and exactly the dependency bodies read; a dependency body never read neither
/// runs nor fails the check.
fn demand(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let reads_dep = tape.one_in(2, "a member body reads a dependency body");
    let dependency = Crate { root: 1, member: false, deps: StdVec::new(), fails: false, scope_files: StdVec::new() };
    let mut zero = member(0);
    zero.deps.push(1);
    let world = World {
        files: std::vec![file(&[], 2), file(&[], 2)],
        crates: std::vec![zero, dependency],
        reads: if reads_dep { std::vec![((0, 1), (1, 0))] } else { StdVec::new() },
        bad: [(1, 0), (1, 1)].into_iter().collect(),
    };
    let ended = fresh(tape, &world, plant)?;
    let ran: BTreeSet<Name> = ended.seen.keys().copied().collect();
    let mut want: BTreeSet<Name> = [body_name(0, 0), body_name(0, 1)].into_iter().collect();
    if reads_dep {
        want.insert(body_name(1, 0));
    }
    if ran != want {
        return Err(Broke::Oracle(format!("a dependency body ran unasked: ran {ran:?}, want {want:?}")));
    }
    let answer = ended.answer.ok_or(Broke::Harness(String::from("no answer")))?;
    let bad = u32::from(reads_dep);
    if answer.findings != bad {
        return Err(Broke::Oracle(format!("a dependency body ran unasked: {} findings, want {bad}", answer.findings)));
    }
    Ok(())
}

/// Expected: "test sim_owners::a_dependency_body_runs_only_when_read ... ok".
#[test]
fn a_dependency_body_runs_only_when_read() {
    check(SimConfig::cases(200), |tape| demand(tape, Plant::None));
}

#[test]
fn control_every_part_made_a_job_is_caught() {
    control(SimConfig::cases(500), Plant::None, Plant::EveryPartAJob, "ran unasked", demand);
}

/// A cycle of module files (`#[path]` both ways) under a root is a named failure of their
/// `Owners`, not a hang: the request answers, and the cycle's files' bodies are not run.
/// Expected: "test sim_owners::a_cycle_of_module_files_is_named ... ok".
#[test]
fn a_cycle_of_module_files_is_named() {
    check(SimConfig::cases(50), |tape| {
        let world = World { files: std::vec![file(&[1], 0), file(&[2], 1), file(&[1], 1)], crates: std::vec![member(0)], ..World::default() };
        let ended = fresh(tape, &world, Plant::None)?;
        let refused = ended.failures.iter().any(|f| f.name.kind == Kind::Owners && matches!(f.why, Why::Refused { rule: "owners", .. }));
        if !refused {
            return Err(Broke::Oracle(format!("a module cycle was not named: {:?}", ended.failures)));
        }
        Ok(())
    });
}
