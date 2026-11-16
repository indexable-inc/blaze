//! The request machine's simulation (drive.md, "Tests"; rules 43, 64): a fake store that counts
//! its reads, a fake checkout of toy items (a signature that may use an earlier item's, a body
//! that calls any item's signature), and a fake executor whose answers arrive in an order the
//! tape picks. The oracle: an incremental request leaves the same answer table and prints the
//! same findings as a fresh build of the same files; a repeat runs nothing for one store read; a
//! one-body edit runs one `FileItems` and one `BodyCheck`; no two runs of one key are out at
//! once; any completion order writes equal bytes; a text-less crate's missing record is the
//! typed error. Two planted bugs show the oracle catches what it claims.

use std::alloc::Global;
use std::collections::{BTreeMap, BTreeSet};
use std::format;
use std::string::String;
use std::vec::Vec as StdVec;

use blaze_id::ContentId;
use blaze_own::Vec;
use blaze_sim::{Broke, Config as SimConfig, Tape, check, control};

use crate::Plant;
use crate::answers::{read_last, read_page, read_root};
use crate::key::{self, Key, Kind, Name, Tool};
use crate::record::{Finding, Locate, Part};
use crate::request::{
    Answer, Ask, Builder, Command, Config, DriveError, Effect, Event, FileEntry, Output, Ran, Request, Task, Ticket,
    Want,
};

// --- The toy checkout -------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Item {
    sig: u64,
    body: u64,
    /// An earlier item whose signature this one's reads.
    uses: Option<(usize, usize)>,
    /// Items whose signatures the body reads.
    calls: StdVec<(usize, usize)>,
}

#[derive(Clone, Debug)]
struct World {
    files: StdVec<StdVec<Item>>,
    /// A crate with no text: a part of the units found only by this record key.
    dep: Option<Key>,
}

fn path(file: usize) -> StdVec<u8> {
    format!("src/f{file}.rs").into_bytes()
}

fn file_name(file: usize) -> Name {
    Name::file(&path(file))
}

fn sig_name(file: usize, item: usize) -> Name {
    Name::new(Kind::Signature, key::digest("sim sig name", format!("{file}/{item}").as_bytes()))
}

fn body_name(file: usize, item: usize) -> Name {
    Name::new(Kind::BodyCheck, key::digest("sim body name", format!("{file}/{item}").as_bytes()))
}

fn dep_name() -> Name {
    Name::new(Kind::Fetch, key::digest("sim dep", b"dep"))
}

fn sig_source(item: &Item) -> ContentId {
    key::digest("sim sig source", format!("{} {:?}", item.sig, item.uses).as_bytes())
}

fn body_source(item: &Item) -> ContentId {
    key::digest("sim body source", format!("{} {:?}", item.body, item.calls).as_bytes())
}

impl World {
    fn draw(tape: &Tape) -> World {
        let nfiles = 1 + tape.index(3, "files");
        let mut files = StdVec::new();
        let mut earlier: StdVec<(usize, usize)> = StdVec::new();
        let sizes: StdVec<usize> = (0..nfiles).map(|_| 1 + tape.index(4, "items")).collect();
        let all: StdVec<(usize, usize)> =
            sizes.iter().enumerate().flat_map(|(f, n)| (0..*n).map(move |i| (f, i))).collect();
        for (f, n) in sizes.iter().enumerate() {
            let mut items = StdVec::new();
            for i in 0..*n {
                let uses = match earlier.is_empty() || tape.one_in(2, "no use") {
                    true => None,
                    false => Some(earlier[tape.index(earlier.len(), "use")]),
                };
                let calls = (0..tape.index(3, "calls")).map(|_| all[tape.index(all.len(), "call")]).collect();
                items.push(Item { sig: tape.below(4, "sig"), body: tape.below(4, "body"), uses, calls });
                earlier.push((f, i));
            }
            files.push(items);
        }
        World { files, dep: None }
    }

    fn bytes(&self, file: usize) -> StdVec<u8> {
        format!("{:?}", self.files[file]).into_bytes()
    }

    fn id(&self, file: usize) -> ContentId {
        key::digest("sim file", &self.bytes(file))
    }

    fn file_of(&self, subject: &ContentId) -> Option<usize> {
        (0..self.files.len()).find(|f| file_name(*f).subject == *subject)
    }

    fn item_of(&self, name: Name) -> Option<(usize, usize)> {
        let make = match name.kind {
            Kind::Signature => sig_name,
            Kind::BodyCheck => body_name,
            _ => return None,
        };
        (0..self.files.len()).flat_map(|f| (0..self.files[f].len()).map(move |i| (f, i))).find(|(f, i)| make(*f, *i) == name)
    }
}

// --- The fake store ---------------------------------------------------------------------------

#[derive(Default, Clone, PartialEq, Eq)]
struct Store {
    records: BTreeMap<Key, StdVec<u8>>,
    reads: u32,
}

impl Store {
    fn put(&mut self, key: Key, bytes: &[u8]) -> Result<(), Broke> {
        match self.records.get(&key) {
            Some(held) if held.as_slice() != bytes => {
                Err(Broke::Oracle(format!("two values under one key {key} (rule 21)")))
            }
            Some(_) => Ok(()),
            None => {
                self.records.insert(key, bytes.to_vec());
                Ok(())
            }
        }
    }
}

// --- The fake executor ------------------------------------------------------------------------

struct Made {
    name: Name,
    out: ContentId,
    reads: StdVec<(Name, ContentId)>,
    parts: StdVec<Part>,
    findings: StdVec<(u16, u32, u32, StdVec<u8>)>,
}

enum Answered {
    Done(Made),
    Need(StdVec<Want>),
}

fn input_out(task: &Task<Global>, name: Name) -> Option<ContentId> {
    task.inputs.iter().find(|input| input.name == name).map(|input| input.out)
}

fn reads_of(task: &Task<Global>, wanted: &[(Name, Name)]) -> Result<StdVec<(Name, ContentId)>, StdVec<Want>> {
    let mut reads = StdVec::new();
    let mut lacking = StdVec::new();
    for (name, home) in wanted {
        match input_out(task, *name) {
            Some(out) => reads.push((*name, out)),
            None => lacking.push(Want { name: *name, home: *home }),
        }
    }
    match lacking.is_empty() {
        true => {
            reads.sort();
            reads.dedup();
            Ok(reads)
        }
        false => Err(lacking),
    }
}

fn execute(world: &World, task: &Task<Global>) -> Result<Answered, Broke> {
    let name = task.name;
    let done = |out, reads, parts, findings| Ok(Answered::Done(Made { name, out, reads, parts, findings }));
    match name.kind {
        Kind::Units => {
            let mut parts = StdVec::new();
            for f in 0..world.files.len() {
                parts.push(Part { name: file_name(f), locate: Locate::File { edition: 0 }, place: 0 });
            }
            if let Some(dep) = world.dep {
                parts.push(Part { name: dep_name(), locate: Locate::Key(dep), place: 0 });
            }
            let out = key::digest("sim units out", format!("{}", world.files.len()).as_bytes());
            done(out, StdVec::new(), parts, StdVec::new())
        }
        Kind::FileItems => {
            let f = world.file_of(&name.subject).ok_or(Broke::Harness(format!("no file for {name}")))?;
            let mut parts = StdVec::new();
            for (i, item) in world.files[f].iter().enumerate() {
                let place = u32::try_from(i).unwrap_or(u32::MAX);
                parts.push(Part { name: sig_name(f, i), locate: Locate::Source(sig_source(item)), place });
                parts.push(Part { name: body_name(f, i), locate: Locate::Source(body_source(item)), place });
            }
            parts.sort_by_key(|part| part.name);
            let out = key::digest("sim file out", format!("{:?}", parts).as_bytes());
            done(out, StdVec::new(), parts, StdVec::new())
        }
        Kind::Signature | Kind::BodyCheck => {
            let (f, i) = world.item_of(name).ok_or(Broke::Harness(format!("no item for {name}")))?;
            let item = &world.files[f][i];
            let (wanted, value, source): (StdVec<(Name, Name)>, u64, ContentId) = match name.kind {
                Kind::Signature => {
                    let wanted = item.uses.iter().map(|(uf, ui)| (sig_name(*uf, *ui), file_name(*uf))).collect();
                    (wanted, item.sig, sig_source(item))
                }
                _ => {
                    let wanted = item.calls.iter().map(|(cf, ci)| (sig_name(*cf, *ci), file_name(*cf))).collect();
                    (wanted, item.body, body_source(item))
                }
            };
            if source != task.source {
                return Err(Broke::Oracle(format!("{name} ran with a source that is not its text's")));
            }
            match reads_of(task, &wanted) {
                Err(lacking) => Ok(Answered::Need(lacking)),
                Ok(reads) => {
                    let out = key::digest("sim item out", format!("{value} {:?}", reads).as_bytes());
                    let findings = match out.0[0] < 64 && name.kind == Kind::BodyCheck {
                        true => std::vec![(7, 0, 1, format!("toy finding {}", out.0[1]).into_bytes())],
                        false => StdVec::new(),
                    };
                    done(out, reads, StdVec::new(), findings)
                }
            }
        }
        kind => Err(Broke::Harness(format!("the sim has no {} task", kind.name()))),
    }
}

// --- One request, driven ----------------------------------------------------------------------

struct Ended {
    answer: Option<Answer>,
    error: Option<DriveError>,
    /// Runs emitted, by kind.
    runs: BTreeMap<Kind, u32>,
    prints: StdVec<(Name, u16, u32, u32, StdVec<u8>, Option<StdVec<u8>>, u32)>,
    last: Option<StdVec<u8>>,
}

fn config(tape: &Tape) -> Config {
    Config {
        tool: Tool(key::digest("sim tool", b"tool")),
        workers: 1 + u32::try_from(tape.index(4, "workers")).unwrap_or(0),
        most_entries: 32,
        most_jobs: 10_000,
    }
}

const ASK: Ask<'static> = Ask { command: Command::Check, args: b"" };

/// The most events one request takes in the sim (rule 18): a toy build is far below it.
const MOST_EVENTS: u32 = 100_000;

fn request(
    tape: &Tape,
    world: &World,
    before: Option<&World>,
    store: &mut Store,
    last: Option<&[u8]>,
    plant: Plant,
) -> Result<Ended, Broke> {
    let ids: StdVec<ContentId> = (0..world.files.len()).map(|f| world.id(f)).collect();
    let paths: StdVec<StdVec<u8>> = (0..world.files.len()).map(path).collect();
    let mut files: StdVec<FileEntry<'_>> = (0..world.files.len())
        .map(|f| FileEntry {
            path: &paths[f],
            id: ids[f],
            changed: before.is_none_or(|old| old.files.get(f).is_none_or(|_| old.id(f) != ids[f])),
            manifest: false,
        })
        .collect();
    let manifest = b"Cargo.toml".to_vec();
    files.push(FileEntry { path: &manifest, id: key::digest("sim manifest", b"[package]"), changed: false, manifest: true });
    files.sort_by(|a, b| a.path.cmp(b.path));

    let mut req = Request::planted_in(config(tape), ASK, plant, Global);
    let mut out: Vec<Effect<Global>, Global> = Vec::new_in(Global);
    let mut ended = Ended { answer: None, error: None, runs: BTreeMap::new(), prints: StdVec::new(), last: None };
    let mut gets: StdVec<Key> = StdVec::new();
    let mut runs: StdVec<(Ticket, Task<Global>)> = StdVec::new();
    let mut first = Some(Event::Start { files: &files, gone: &[], last });
    for _ in 0..MOST_EVENTS {
        let stepped = match first.take() {
            Some(event) => req.step(event, &mut out),
            None => {
                let total = gets.len() + runs.len();
                if total == 0 {
                    return Err(Broke::Oracle(String::from("the request stalled: nothing out and no answer")));
                }
                let pick = tape.index(total, "which completes");
                if pick < gets.len() {
                    let key = gets.swap_remove(pick);
                    store.reads += 1;
                    let bytes = store.records.get(&key).cloned();
                    req.step(Event::Got { key, bytes: bytes.as_deref() }, &mut out)
                } else {
                    let (ticket, task) = runs.swap_remove(pick - gets.len());
                    match execute(world, &task)? {
                        Answered::Need(wants) => req.step(Event::Ran { ticket, ran: Ran::Need(&wants) }, &mut out),
                        Answered::Done(made) => {
                            let findings: StdVec<Finding<'_>> = made
                                .findings
                                .iter()
                                .map(|(rule, start, end, message)| Finding { rule: *rule, start: *start, end: *end, message })
                                .collect();
                            let output = Output {
                                name: made.name,
                                out: made.out,
                                reads: &made.reads,
                                parts: &made.parts,
                                findings: &findings,
                                payload: b"",
                            };
                            req.step(Event::Ran { ticket, ran: Ran::Done(core::slice::from_ref(&output)) }, &mut out)
                        }
                    }
                }
            }
        };
        if let Err(error) = stepped {
            ended.error = Some(error);
            return Ok(ended);
        }
        for effect in out.drain(..) {
            match effect {
                Effect::Get { key } => gets.push(key),
                Effect::Put { key, bytes } => store.put(key, &bytes)?,
                Effect::Run { ticket, task } => {
                    if runs.iter().any(|(_, running)| running.key == task.key) {
                        return Err(Broke::Oracle(format!("single flight: {} ran twice at once", task.name)));
                    }
                    *ended.runs.entry(task.name.kind).or_insert(0) += 1;
                    runs.push((ticket, task));
                }
                Effect::Print(print) => {
                    let file = print.file.map(|index| req.path(index).to_vec());
                    ended.prints.push((
                        print.name,
                        print.rule,
                        print.start,
                        print.end,
                        print.message.as_slice().to_vec(),
                        file,
                        print.place,
                    ));
                }
                Effect::WriteLast { bytes } => ended.last = Some(bytes.as_slice().to_vec()),
                Effect::Done(answer) => ended.answer = Some(answer),
            }
        }
        if ended.answer.is_some() {
            if !gets.is_empty() || !runs.is_empty() {
                return Err(Broke::Oracle(String::from("answered with work still out")));
            }
            return Ok(ended);
        }
    }
    Err(Broke::Harness(format!("more than {MOST_EVENTS} events")))
}

fn finished(ended: Ended) -> Result<(Answer, Ended), Broke> {
    if let Some(error) = ended.error {
        return Err(Broke::Oracle(format!("the request failed: {error}")));
    }
    let answer = ended.answer.ok_or(Broke::Harness(String::from("no answer")))?;
    Ok((answer, ended))
}

type Table = BTreeMap<Name, (Key, Key, ContentId, ContentId, Name, u32, StdVec<(Name, ContentId)>, StdVec<Name>, StdVec<Name>)>;

/// The answer table `last` names, read from `store`.
fn table(store: &Store, last: &[u8]) -> Result<Table, Broke> {
    let wire = |error| Broke::Oracle(format!("a stored answer is not one: {error}"));
    let last = read_last(last).map_err(wire)?;
    let root_bytes = store.records.get(&last.root).ok_or(Broke::Oracle(String::from("the root is not stored")))?;
    let root = read_root(root_bytes, Global).map_err(wire)?;
    let mut table = Table::new();
    for page_key in root.pages.iter() {
        if *page_key == Key::default() {
            continue;
        }
        let bytes = store.records.get(page_key).ok_or(Broke::Oracle(String::from("a page is not stored")))?;
        for (name, row) in read_page(bytes, Global).map_err(wire)?.iter() {
            table.insert(
                *name,
                (
                    row.key,
                    row.at,
                    row.source,
                    row.out,
                    row.home,
                    row.place,
                    row.reads.as_slice().to_vec(),
                    row.parts.as_slice().to_vec(),
                    row.readers.as_slice().to_vec(),
                ),
            );
        }
    }
    Ok(table)
}

fn oracle(holds: bool, why: impl FnOnce() -> String) -> Result<(), Broke> {
    match holds {
        true => Ok(()),
        false => Err(Broke::Oracle(why())),
    }
}

/// A fresh build of `world` in its own store: what every incremental answer must equal.
fn fresh(tape: &Tape, world: &World) -> Result<(Store, Ended), Broke> {
    let mut store = Store::default();
    let (_, ended) = finished(request(tape, world, None, &mut store, None, Plant::None)?)?;
    Ok((store, ended))
}

fn same_as_fresh(tape: &Tape, world: &World, store: &Store, ended: &Ended) -> Result<(), Broke> {
    let (fresh_store, fresh_ended) = fresh(tape, world)?;
    let last = ended.last.as_deref().ok_or(Broke::Oracle(String::from("no last written")))?;
    let fresh_last = fresh_ended.last.as_deref().ok_or(Broke::Oracle(String::from("no last written")))?;
    let here = table(store, last)?;
    let there = table(&fresh_store, fresh_last)?;
    oracle(here == there, || {
        let differ: StdVec<String> = here
            .keys()
            .chain(there.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|name| here.get(name) != there.get(name))
            .take(3)
            .map(|name| format!("{name}"))
            .collect();
        format!("the answer table differs from a fresh build at {differ:?}")
    })?;
    oracle(ended.prints == fresh_ended.prints, || String::from("the findings printed differ from a fresh build"))
}

/// An edit of one item's signature or body, to a value no draw makes.
fn edit(tape: &Tape, world: &World, serial: u64, body: bool) -> World {
    let mut next = world.clone();
    let f = tape.index(next.files.len(), "edit file");
    let i = tape.index(next.files[f].len(), "edit item");
    match body {
        true => next.files[f][i].body = 1_000 + serial,
        false => next.files[f][i].sig = 1_000 + serial,
    }
    next
}

// --- The properties ---------------------------------------------------------------------------

fn edits(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut world = World::draw(tape);
    let mut store = Store::default();
    let (_, first) = finished(request(tape, &world, None, &mut store, None, plant)?)?;
    let mut last = first.last;
    for serial in 0..1 + tape.below(3, "edits") {
        let next = edit(tape, &world, serial, tape.one_in(2, "body or signature"));
        let (_, ended) = finished(request(tape, &next, Some(&world), &mut store, last.as_deref(), plant)?)?;
        same_as_fresh(tape, &next, &store, &ended)?;
        last = ended.last.clone();
        world = next;
    }
    Ok(())
}

#[test]
fn an_incremental_answer_equals_a_fresh_build() {
    check(SimConfig::cases(300), |tape| edits(tape, Plant::None));
}

#[test]
fn control_a_skipped_changed_read_is_caught() {
    control(SimConfig::cases(2000), Plant::None, Plant::SkipChangedRead, "differs from a fresh build", edits);
}

fn flights(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let world = World::draw(tape);
    let mut store = Store::default();
    finished(request(tape, &world, None, &mut store, None, plant)?)?;
    Ok(())
}

#[test]
fn one_flight_per_key() {
    check(SimConfig::cases(300), |tape| flights(tape, Plant::None));
}

#[test]
fn control_a_second_flight_is_caught() {
    control(SimConfig::cases(2000), Plant::None, Plant::DoubleFlight, "single flight", flights);
}

#[test]
fn a_repeat_runs_nothing_for_one_read() {
    check(SimConfig::cases(200), |tape| {
        let world = World::draw(tape);
        let mut store = Store::default();
        let (_, first) = finished(request(tape, &world, None, &mut store, None, Plant::None)?)?;
        store.reads = 0;
        let (answer, again) = finished(request(tape, &world, Some(&world), &mut store, first.last.as_deref(), Plant::None)?)?;
        oracle(again.runs.is_empty(), || format!("a repeat ran {:?}", again.runs))?;
        oracle(store.reads == 1 && answer.profile.gets == 1, || format!("a repeat read the store {} times", store.reads))?;
        oracle(answer.profile.certified, || String::from("a repeat did not stand on its certificate"))?;
        oracle(again.prints == first.prints, || String::from("a repeat printed other findings"))
    });
}

#[test]
fn a_one_body_edit_runs_one_file_and_one_body() {
    check(SimConfig::cases(300), |tape| {
        let world = World::draw(tape);
        let mut store = Store::default();
        let (_, first) = finished(request(tape, &world, None, &mut store, None, Plant::None)?)?;
        let next = edit(tape, &world, 0, true);
        store.reads = 0;
        let (answer, ended) = finished(request(tape, &next, Some(&world), &mut store, first.last.as_deref(), Plant::None)?)?;
        let ran = |kind| ended.runs.get(&kind).copied().unwrap_or(0);
        oracle(ran(Kind::FileItems) == 1 && ran(Kind::BodyCheck) == 1 && ended.runs.len() == 2, || {
            format!("a one-body edit ran {:?}", ended.runs)
        })?;
        // drive.md's count: certificate, old certificate, root, the pages, the file's key, the
        // body's key; the body's inputs stand on their rows.
        let pages = answer.profile.pages;
        oracle(store.reads <= 5 + pages, || format!("a one-body edit read the store {} times ({pages} pages)", store.reads))?;
        same_as_fresh(tape, &next, &store, &ended)
    });
}

#[test]
fn any_completion_order_writes_equal_bytes() {
    check(SimConfig::cases(200), |tape| {
        let world = World::draw(tape);
        let (one, _) = fresh(tape, &world)?;
        let (two, _) = fresh(tape, &world)?;
        oracle(one.records == two.records, || String::from("two orders of one build wrote other bytes"))
    });
}

#[test]
fn a_missing_record_of_a_textless_crate_is_the_typed_error() {
    check(SimConfig::cases(50), |tape| {
        let mut world = World::draw(tape);
        let dep = Key(key::digest("sim dep key", &tape.next_u64("dep").to_le_bytes()));
        world.dep = Some(dep);
        let mut store = Store::default();
        let ended = request(tape, &world, None, &mut store, None, Plant::None)?;
        match ended.error {
            Some(DriveError::Missing { name, key, builder: Builder::Pack { krate } }) => {
                oracle(name == dep_name() && key == dep && krate == Name::units(), || {
                    format!("the missing record named {name} {key} {krate}")
                })
            }
            other => Err(Broke::Oracle(format!("a missing dependency record ended as {other:?}"))),
        }
    });
}
