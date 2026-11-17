//! What a command gives, simulated (key.rs `Kind::Given`; drive.md, RUNNER 19 ruling (2)). A
//! toy world of readers listed by the units:
//! - `Expansion` reader i reads the CONTENT of the file `data/<i>.bin` by path
//!   (`key::content_of`), a file the checkout's sweep never saw: present with a value, or absent;
//! - `ImplBucket` readers read the fact `SIM_GIVEN_ENV` (`key::fact_of`), handed in
//!   `Ask::facts`.
//!
//! The harness plays the cli's leaf table: a path a task read joins it as the command ends (an
//! absent one as absent), and the next command's `Start` hands it from the sweep, changed when
//! its bytes moved, in `gone` when it went.
//!
//! Cases:
//! - a file the sweep never saw: a first command runs every reader, the repeat runs nothing,
//!   and an edit (a change, a removal or a creation) of one reader's file reruns that reader
//!   alone (controls: the harness does not join a first-read path; the request seeds no `Given`
//!   job for a changed file);
//! - a moved fact reruns its readers alone, and the content readers stand;
//! - the ask's facts unsorted, or one not a `Given` name, are refused at `Start`.
//!
//! Every test's expected line is "test sim_given::<name> ... ok"; each control passes when its
//! plant is caught, with the message it names.

use std::collections::{BTreeMap, BTreeSet};
use std::format;
use std::vec::Vec as StdVec;

use blaze_id::ContentId;
use blaze_own::{Alloc, Vec};
use blaze_sim::{Broke, Config as SimConfig, Tape, check, control};

use crate::Plant;
use crate::condition::Space;
use crate::key::{self, ABSENT, Key, Kind, Name, Tool};
use crate::record::{Finding, Locate, Part, PathOf};
use crate::request::{
    Answer, Ask, BatchId, Command, Config, DriveError, Effect, Event, FileEntry, Gate, Outcome, Output, Ran, Request,
    Task, Ticket,
};

const MOST_EVENTS: u32 = 100_000;

/// The harness's own bug, beside the request's plants: the cli's half of the ruling.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Bug {
    None,
    /// A path a task read is not joined to the leaf table: the next command's sweep does not
    /// follow it, so its edit reaches nobody.
    FirstReadNotJoined,
}

/// Each content reader's file (`None`: absent), the fact's value, and the fact readers.
#[derive(Clone, Debug)]
struct World {
    files: StdVec<Option<u64>>,
    fact: u64,
    fact_readers: usize,
}

fn path(i: usize) -> StdVec<u8> {
    format!("data/{i}.bin").into_bytes()
}

fn content_id(value: u64) -> ContentId {
    key::digest("sim given bytes", &value.to_le_bytes())
}

fn fact() -> Name {
    key::fact_of(b"SIM_GIVEN_ENV")
}

fn fact_value(value: u64) -> ContentId {
    key::digest("sim given fact", &value.to_le_bytes())
}

fn reader(kind: Kind, i: usize) -> Name {
    Name::new(kind, key::digest("sim given reader", format!("{} {i}", kind.name()).as_bytes()))
}

impl World {
    fn readers(&self) -> StdVec<Name> {
        let mut names: StdVec<Name> = (0..self.files.len()).map(|i| reader(Kind::Expansion, i)).collect();
        names.extend((0..self.fact_readers).map(|i| reader(Kind::ImplBucket, i)));
        names
    }

    /// What the layout read: the readers' counts only, so no file or fact edit moves the units.
    fn units_source(&self) -> ContentId {
        key::digest("sim given units", format!("{} {}", self.files.len(), self.fact_readers).as_bytes())
    }

    /// Today's id of reader i's file, as the sweep finds it.
    fn id_of(&self, i: usize) -> Option<ContentId> {
        self.files[i].map(content_id)
    }
}

// --- The fake executor ------------------------------------------------------------------------

struct Made {
    name: Name,
    out: ContentId,
    reads: StdVec<(Name, ContentId)>,
    parts: StdVec<Part>,
    /// The path a content reader read, for the leaf table.
    read_path: Option<usize>,
}

fn execute(world: &World, task: &Task<'static>) -> Result<Made, Broke> {
    let name = task.name;
    if name.kind == Kind::Units {
        let mut parts: StdVec<Part> = world
            .readers()
            .into_iter()
            .map(|read| Part { name: read, locate: Locate::Source(key::digest("sim given reader source", &read.subject.0)), place: 0 })
            .collect();
        parts.sort_by_key(|part| part.name);
        return Ok(Made { name, out: key::digest("sim given units out", b""), reads: StdVec::new(), parts, read_path: None });
    }
    match name.kind {
        Kind::Expansion => {
            let i = (0..world.files.len()).find(|i| reader(Kind::Expansion, *i) == name).ok_or(Broke::Harness(format!("no reader {name}")))?;
            let saw = world.id_of(i).unwrap_or(ABSENT);
            let out = key::digest("sim given content reader out", &saw.0);
            Ok(Made { name, out, reads: std::vec![(key::content_of(&path(i)), saw)], parts: StdVec::new(), read_path: Some(i) })
        }
        Kind::ImplBucket => {
            let saw = fact_value(world.fact);
            let out = key::digest("sim given fact reader out", format!("{name} {saw:?}").as_bytes());
            Ok(Made { name, out, reads: std::vec![(fact(), saw)], parts: StdVec::new(), read_path: None })
        }
        kind => Err(Broke::Harness(format!("the given sim has no {} task", kind.name()))),
    }
}

// --- The leaf table, as the cli keeps it -------------------------------------------------------

/// The paths joined to the leaf table, each with the id the last command saw (`None`: absent).
#[derive(Clone, Default)]
struct Leaves {
    joined: BTreeMap<usize, Option<ContentId>>,
}

// --- One request, driven ----------------------------------------------------------------------

#[derive(Default)]
struct Ended {
    answer: Option<Answer>,
    /// Every name a batch ran this request.
    ran: BTreeSet<Name>,
    last: Option<StdVec<u8>>,
}

impl Ended {
    fn ran_of(&self, kind: Kind) -> usize {
        self.ran.iter().filter(|name| name.kind == kind).count()
    }
}

fn never_an_error(_: u16) -> bool {
    false
}

fn config(workers: u32) -> Config {
    let tools = Kind::ALL.map(|kind| Tool(key::digest("sim given kind tool", &[kind as u8])));
    Config { tool: Tool(key::digest("sim given tool", b"")), tools, workers, most_entries: 32, most_jobs: 1 << 16, is_error: never_an_error }
}

/// One `check` over `store`, walking forward from `last`; the leaf table updated as the cli
/// writes it beside the answer.
fn request(
    tape: &Tape,
    world: &World,
    plant: Plant,
    bug: Bug,
    store: &mut BTreeMap<Key, StdVec<u8>>,
    leaves: &mut Leaves,
    last: Option<&[u8]>,
) -> Result<Ended, DriveError> {
    let manifest = b"Cargo.toml".to_vec();
    let paths: BTreeMap<usize, StdVec<u8>> = leaves.joined.keys().map(|i| (*i, path(*i))).collect();
    let mut files = std::vec![FileEntry { path: &manifest, id: world.units_source(), changed: false, manifest: true }];
    let mut gone: StdVec<&[u8]> = StdVec::new();
    for (i, was) in leaves.joined.iter() {
        match (world.id_of(*i), was) {
            (Some(id), was) => files.push(FileEntry { path: &paths[i], id, changed: *was != Some(id), manifest: false }),
            (None, Some(_)) => gone.push(&paths[i]),
            (None, None) => {}
        }
    }
    files.sort_by(|a, b| a.path.cmp(b.path));
    let facts = [(fact(), fact_value(world.fact))];
    let ask = Ask { command: Command::Check, args: b"", gate: Gate::Beside, space: Space::NONE, program: None, facts: &facts };
    let workers = [1, 2, 16][tape.index(3, "workers")];
    let mut req = Request::planted_in(config(workers), ask, plant, blaze_own::testing::heap().alloc());
    let mut out: Vec<Effect<'static>, Alloc<'static>> = Vec::new_in(blaze_own::testing::heap().alloc());
    let mut ended = Ended::default();
    let mut read_paths: BTreeSet<usize> = BTreeSet::new();
    let mut gets: StdVec<Key> = StdVec::new();
    let mut batches: StdVec<(BatchId, StdVec<(Ticket, Task<'static>)>)> = StdVec::new();
    let mut first = Some(Event::Start { files: &files, gone: &gone, last, units: world.units_source(), cold: false, graph: None });
    for _ in 0..MOST_EVENTS {
        match first.take() {
            Some(event) => req.step(event, &mut out)?,
            None => {
                let total = gets.len() + batches.len();
                assert!(total > 0, "the request stalled: nothing out and no answer");
                let pick = tape.index(total, "which completes");
                if pick < gets.len() {
                    let key = gets.swap_remove(pick);
                    let bytes = store.get(&key).cloned();
                    req.step(Event::Got { key, bytes: bytes.as_deref() }, &mut out)?
                } else {
                    let (id, tasks) = batches.swap_remove(pick - gets.len());
                    let mut made = StdVec::new();
                    for (ticket, task) in tasks.iter() {
                        let m = execute(world, task).unwrap_or_else(|broke| panic!("the executor broke: {broke:?}"));
                        if let Some(i) = m.read_path {
                            read_paths.insert(i);
                        }
                        made.push((*ticket, m));
                    }
                    let no_findings: [Finding<'_>; 0] = [];
                    let no_paths: [PathOf<'_>; 0] = [];
                    let outputs: StdVec<Output<'_>> = made
                        .iter()
                        .map(|(_, m)| Output { name: m.name, out: m.out, reads: &m.reads, cells: &[], parts: &m.parts, paths: &no_paths, findings: &no_findings, payload: &[] })
                        .collect();
                    let results: StdVec<Outcome<'_>> = made
                        .iter()
                        .enumerate()
                        .map(|(at, (ticket, _))| Outcome { ticket: *ticket, ran: Ran::Done(&outputs[at..at + 1]), in_place: 0, ns: 1_000 })
                        .collect();
                    req.step(Event::Ran { batch: id, results: &results }, &mut out)?
                }
            }
        }
        for effect in out.drain(..) {
            match effect {
                Effect::Get { key } => gets.push(key),
                Effect::Put { key, bytes } => match store.get(&key) {
                    Some(held) => assert!(held.as_slice() == bytes.as_slice(), "two values under one key {key} (rule 21)"),
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
                Effect::Publish(_) | Effect::Unpublish(_) | Effect::Print(_) => {}
                Effect::WriteLast { bytes } => ended.last = Some(bytes.as_slice().to_vec()),
                Effect::Done(answer) => ended.answer = Some(answer),
                Effect::Open { name, .. } => panic!("{name} was opened: a content read is never a file part"),
                Effect::StartProgram { .. } | Effect::Stop { .. } => panic!("`check` started or stopped a program"),
            }
        }
        if ended.answer.is_some() {
            assert!(gets.is_empty() && batches.is_empty(), "answered with work still out");
            assert!(req.failures().is_empty(), "tasks failed: {:?}", req.failures());
            // The leaf table written beside the answer: every path it held, as today, and each
            // path a task read this command joined (the cli's worker names it).
            let mut joined = BTreeMap::new();
            for i in leaves.joined.keys() {
                if world.id_of(*i).is_none() || req.holds(&path(*i)) {
                    joined.insert(*i, world.id_of(*i));
                }
            }
            if bug != Bug::FirstReadNotJoined {
                for i in read_paths.iter() {
                    joined.insert(*i, world.id_of(*i));
                }
            }
            leaves.joined = joined;
            return Ok(ended);
        }
    }
    panic!("more than {MOST_EVENTS} events");
}

/// One to three content readers, each file present or absent, and one or two fact readers.
fn draw(tape: &Tape) -> World {
    let n = 1 + tape.index(3, "content readers");
    let files = (0..n).map(|i| (tape.index(4, "file absent") != 0).then_some(i as u64 + 1)).collect();
    World { files, fact: 7, fact_readers: 1 + tape.index(2, "fact readers") }
}

fn caught(plant: Plant, bug: Bug, words: &str, case: fn(&Tape, Plant, Bug) -> Result<(), Broke>) {
    control(SimConfig::cases(300), (Plant::None, Bug::None), (plant, bug), words, |tape, (plant, bug)| case(tape, plant, bug));
}

fn failed(error: DriveError) -> Broke {
    Broke::Oracle(format!("the request failed: {error}"))
}

// --- Case 1: a file the sweep never saw --------------------------------------------------------

fn edited_file(tape: &Tape, plant: Plant, bug: Bug) -> Result<(), Broke> {
    let before = draw(tape);
    let mut store = BTreeMap::new();
    let mut leaves = Leaves::default();
    let first = request(tape, &before, plant, bug, &mut store, &mut leaves, None).map_err(failed)?;
    if first.ran_of(Kind::Expansion) != before.files.len() || first.ran_of(Kind::ImplBucket) != before.fact_readers {
        return Err(Broke::Oracle(format!("a first command ran {} of {} readers", first.ran.len() - 1, before.readers().len())));
    }
    let again = request(tape, &before, plant, bug, &mut store, &mut leaves, first.last.as_deref()).map_err(failed)?;
    if !again.ran.is_empty() {
        return Err(Broke::Oracle(format!("an unchanged repeat ran {} tasks", again.ran.len())));
    }
    // One reader's file edited: changed, removed, or created.
    let k = tape.index(before.files.len(), "edited file");
    let mut after = before.clone();
    after.files[k] = match before.files[k] {
        Some(value) if tape.index(2, "remove it") == 0 => Some(value + 100),
        Some(_) => None,
        None => Some(42),
    };
    let last = again.last.as_deref().or(first.last.as_deref());
    let third = request(tape, &after, plant, bug, &mut store, &mut leaves, last).map_err(failed)?;
    let edited = reader(Kind::Expansion, k);
    if !third.ran.contains(&edited) {
        return Err(Broke::Oracle(format!("an edited file's reader stood ({:?} to {:?})", before.files[k], after.files[k])));
    }
    if third.ran.len() != 1 {
        return Err(Broke::Oracle(format!("one file's edit ran {} tasks: {:?}", third.ran.len(), third.ran)));
    }
    Ok(())
}

/// Expected: "test sim_given::a_file_the_sweep_never_saw_reruns_its_reader_once_edited ... ok".
#[test]
fn a_file_the_sweep_never_saw_reruns_its_reader_once_edited() {
    check(SimConfig::cases(300), |tape| edited_file(tape, Plant::None, Bug::None));
}

/// The ruling's plant: the first-read path never joins the leaf table.
/// Expected: "test sim_given::control_a_first_read_path_not_joined_is_caught ... ok".
#[test]
fn control_a_first_read_path_not_joined_is_caught() {
    caught(Plant::None, Bug::FirstReadNotJoined, "an edited file's reader stood", edited_file);
}

/// Expected: "test sim_given::control_a_changed_file_seeding_no_given_job_is_caught ... ok".
#[test]
fn control_a_changed_file_seeding_no_given_job_is_caught() {
    caught(Plant::GivenNotSeeded, Bug::None, "an edited file's reader stood", edited_file);
}

// --- Case 2: a moved fact ----------------------------------------------------------------------

fn moved_fact(tape: &Tape, plant: Plant, bug: Bug) -> Result<(), Broke> {
    let before = draw(tape);
    let mut store = BTreeMap::new();
    let mut leaves = Leaves::default();
    let first = request(tape, &before, plant, bug, &mut store, &mut leaves, None).map_err(failed)?;
    let after = World { fact: before.fact + 1, ..before.clone() };
    let second = request(tape, &after, plant, bug, &mut store, &mut leaves, first.last.as_deref()).map_err(failed)?;
    if second.ran_of(Kind::ImplBucket) != after.fact_readers {
        return Err(Broke::Oracle(format!("a moved fact reran {} of its {} readers", second.ran_of(Kind::ImplBucket), after.fact_readers)));
    }
    if second.ran_of(Kind::Expansion) != 0 {
        return Err(Broke::Oracle(format!("a moved fact reran {} readers of files", second.ran_of(Kind::Expansion))));
    }
    Ok(())
}

/// Expected: "test sim_given::a_moved_fact_reruns_only_its_readers ... ok".
#[test]
fn a_moved_fact_reruns_only_its_readers() {
    check(SimConfig::cases(200), |tape| moved_fact(tape, Plant::None, Bug::None));
}

// --- Case 3: the ask's facts are checked -------------------------------------------------------

/// Expected: "test sim_given::unsorted_or_foreign_facts_are_refused_at_start ... ok".
#[test]
fn unsorted_or_foreign_facts_are_refused_at_start() {
    let (a, b) = (key::fact_of(b"A"), key::fact_of(b"B"));
    let (low, high) = if a < b { (a, b) } else { (b, a) };
    let manifest = b"Cargo.toml".to_vec();
    let files = [FileEntry { path: &manifest, id: ABSENT, changed: false, manifest: true }];
    let start = |facts: &[(Name, ContentId)]| {
        let ask = Ask { command: Command::Check, args: b"", gate: Gate::Beside, space: Space::NONE, program: None, facts };
        let mut req = Request::new_in(config(1), ask, blaze_own::testing::heap().alloc());
        let mut out: Vec<Effect<'static>, Alloc<'static>> = Vec::new_in(blaze_own::testing::heap().alloc());
        req.step(Event::Start { files: &files, gone: &[], last: None, units: ABSENT, cold: true, graph: None }, &mut out).err()
    };
    assert_eq!(start(&[(high, ABSENT), (low, ABSENT)]), Some(DriveError::Unsorted { what: "the ask's facts" }));
    let file = Name::file(b"src/lib.rs");
    assert_eq!(start(&[(file, ABSENT)]), Some(DriveError::NotGiven { name: file }));
    assert_eq!(start(&[(low, ABSENT), (high, ABSENT)]), None);
}
