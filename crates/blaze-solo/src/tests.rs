//! The leaf table's layout, the slots, and the command under the deterministic simulation:
//! edits, adds, removes, renames, touches, writes inside the racy window, cache directory
//! tags added and removed, runs alone, two commands interleaved at every effect, crashes at
//! any effect (between the run's puts and its root's save too), and a store lost whole; each
//! against a fresh state's answers. Each planted bug ([`Plant`]) is caught.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Once,
    time::Duration,
};

use blaze_cas::{Key, Kind as StoreKind, Mismatch, content_of};
use blaze_core::{
    Completion, Done, Driver as _, FileName, Stamp,
    sim::{Faults, Sim},
    wait::Deadline,
};
use blaze_front_items::{Reading, steps::{FileKey, Items}};
use blaze_ids::ContentId;
use blaze_kernel::{CodeId, Get, Put, Session};
use blaze_sim::{Broke, Config as SimConfig, Tape};
use blaze_store_check::Setting;
use blaze_vfs::watch::fixed_out;
use syntax::{Edition, Extensions};

use crate::{
    PLANT, Plant,
    root::{self, Opened, SLOT_BYTES, Slot},
    run::{ANSWER_DOMAIN, Ask, Codes, Command, Config, Outcome, Poll, SoloError, TAG_SPAN, kernel},
    sweep::SIGNATURE,
    table::{self, Kind, LAYOUT, Row, Table, Undecoded},
    world::SoloWorld,
};

const SECOND: u64 = 1_000_000_000;
const CO: &str = "co";
const ST: &str = "st";

/// The checkout's files the operations touch: `keep.txt` is never removed (the checkout stays
/// a directory), `target/` and `.git/` are out of the view by name, `cache/` and `src/e/` by
/// a tag when one is written.
const FILES: [&str; 10] =
    ["keep.txt", "a.rs", "b.rs", "src/c.rs", "src/d.rs", "src/e/f.rs", "cache/g.rs", "target/t.rs", ".git/h.rs", "notes.md"];
const TAG_DIRS: [&str; 3] = ["cache", "src/e", "src"];

fn name(it: &str) -> FileName {
    FileName::new(it).expect("a name")
}

fn codes() -> Codes {
    let code = |n: u8| CodeId(ContentId::of("blaze solo test step code", &[n]));
    Codes { parse: code(0), items: code(1), lines: code(2), modules: code(3) }
}

fn reading() -> Reading {
    Reading { edition: Edition::Edition2021, extensions: Extensions::NONE }
}

/// A file's text: `0..10` all of one length, so a rewrite in the same tick keeps the stamp.
fn content(n: u8) -> Vec<u8> {
    match n {
        0..10 => format!("fn f{n}() {{}}\n").into_bytes(),
        _ => format!("struct S{n};\nfn g() {{ let _ = S{n}; }}\n").into_bytes(),
    }
}

/// The store a command is handed: what was put, readable at once, and what of it is durable.
#[derive(Default)]
struct Shelf {
    live: BTreeMap<Key, Vec<u8>>,
    /// Puts since the last flush, in order: what a crash may lose (a suffix of).
    pending: Vec<Key>,
}

impl Get for Shelf {
    fn get(&self, key: &Key) -> Result<Option<&[u8]>, String> {
        Ok(self.live.get(key).map(Vec::as_slice))
    }
}

impl Put for Shelf {
    fn put(&mut self, key: Key, _kind: StoreKind, bytes: Vec<u8>) -> Result<(), Mismatch> {
        match self.live.get(&key) {
            Some(kept) if *kept != bytes => Err(Mismatch { key, kept: content_of(kept), given: content_of(&bytes) }),
            Some(_) => Ok(()),
            None => {
                self.live.insert(key, bytes);
                self.pending.push(key);
                Ok(())
            }
        }
    }
}

impl Shelf {
    fn flush(&mut self) {
        self.pending.clear();
    }

    /// A crash: the first `keep` puts since the last flush survive, the rest are gone.
    fn crash(&mut self, keep: usize) {
        let lost = self.pending.split_off(keep.min(self.pending.len()));
        for key in lost {
            self.live.remove(&key);
        }
        self.pending.clear();
    }
}

/// Where a run is cut by a crash.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cut {
    Never,
    /// Before the `n`th delivery (a completion, or the store's flush).
    At(usize),
    /// At the store's flush, before it: a crash between the run's puts and their durability.
    Flush,
}

struct Live {
    command: Command,
    effects: Vec<blaze_core::Effect>,
    inbox: VecDeque<Completion>,
}

enum Stepped {
    Pending,
    Flush,
    Done(Result<Outcome, SoloError>),
}

struct Harness {
    sim: Sim,
    /// Stamps floored to whole seconds, as a file system with a coarse clock gives them.
    coarse: bool,
    store: Shelf,
    /// The checkout's files, by path: what a fresh state reads.
    model: BTreeMap<String, Vec<u8>>,
    tags: u64,
    /// Slot sequences written since the last crash.
    written: BTreeSet<u64>,
    /// The store was lost whole: an old slot may name a root it lacks.
    may_lose: bool,
    /// A crash keeps none of the store's puts since its last flush.
    keep_none: bool,
}

impl Harness {
    fn new(seed: u64, coarse: bool) -> Harness {
        let mut h = Harness {
            sim: Sim::new(seed, Faults::default()),
            coarse,
            store: Shelf::default(),
            model: BTreeMap::new(),
            tags: 1 << 40,
            written: BTreeSet::new(),
            may_lose: false,
            keep_none: false,
        };
        h.put("keep.txt", b"kept\n".to_vec());
        h
    }

    fn put(&mut self, path: &str, bytes: Vec<u8>) {
        self.sim.change(&name(&format!("{CO}/{path}")), &bytes);
        self.model.insert(path.to_owned(), bytes);
    }

    fn delete(&mut self, path: &str) {
        if self.model.remove(path).is_some() {
            self.sim.delete(&name(&format!("{CO}/{path}")));
        }
    }

    fn start(&mut self, ask: Ask) -> Live {
        let config = Config { checkout: name(CO), state: name(ST), reading: reading(), ask, codes: codes(), tags: self.tags };
        self.tags += TAG_SPAN;
        let mut effects = Vec::new();
        let command = Command::start(config, self.sim.wall_ns(), self.sim.now(), &mut effects).expect("a command starts");
        Live { command, effects, inbox: VecDeque::new() }
    }

    /// The effects of `lives` submitted, and one poll's completions handed to their owners.
    fn pump(&mut self, lives: &mut [&mut Live]) -> bool {
        for live in lives.iter_mut() {
            self.sim.submit(&mut live.effects);
        }
        let mut done = Vec::new();
        let deadline = Deadline::after(self.sim.now(), Duration::from_secs(1));
        self.sim.poll(&mut done, deadline);
        let got = !done.is_empty();
        for mut completion in done {
            if self.coarse {
                floor(&mut completion.done);
            }
            let live = lives.iter_mut().find(|it| it.command.owns(completion.tag)).expect("a completion of a live command");
            live.inbox.push_back(completion);
        }
        got
    }

    fn deliver(&mut self, live: &mut Live) -> Stepped {
        let completion = live.inbox.pop_front().expect("a completion to deliver");
        let now = self.sim.now();
        match live.command.complete(completion, &mut self.store, || now, &mut live.effects) {
            Poll::Pending => Stepped::Pending,
            Poll::Flush => Stepped::Flush,
            Poll::Done => Stepped::Done(live.command.finish()),
        }
    }

    fn flush(&mut self, live: &mut Live) -> Stepped {
        self.store.flush();
        let now = self.sim.now();
        match live.command.flushed(Ok(()), || now, &mut live.effects) {
            Poll::Pending => Stepped::Pending,
            Poll::Flush => panic!("a flush asked for twice in a row"),
            Poll::Done => Stepped::Done(live.command.finish()),
        }
    }

    fn crash(&mut self) {
        self.sim.crash();
        let keep = match self.keep_none {
            true => 0,
            false => self.sim.rng().index(self.store.pending.len() + 1),
        };
        self.store.crash(keep);
        // A crash may take back a slot's write: its sequence may be written again.
        self.written.clear();
    }

    /// `live` driven alone to its end, or to the crash `cut` names (`None`).
    fn finish(&mut self, mut live: Live, cut: Cut) -> Option<Result<Outcome, SoloError>> {
        let (mut steps, mut idle) = (0, 0);
        loop {
            if live.inbox.is_empty() {
                match self.pump(&mut [&mut live]) {
                    true => idle = 0,
                    false => {
                        idle += 1;
                        assert!(idle < 1_000, "the command stalled: nothing in flight completes");
                    }
                }
                continue;
            }
            if cut == Cut::At(steps) {
                self.crash();
                return None;
            }
            steps += 1;
            let mut stepped = self.deliver(&mut live);
            if let Stepped::Flush = stepped {
                if cut == Cut::Flush || cut == Cut::At(steps) {
                    self.crash();
                    return None;
                }
                steps += 1;
                stepped = self.flush(&mut live);
            }
            if let Stepped::Done(result) = stepped {
                return Some(result);
            }
        }
    }

    fn run(&mut self, ask: Ask, cut: Cut) -> Option<Result<Outcome, SoloError>> {
        let live = self.start(ask);
        self.finish(live, cut)
    }

    fn run_ok(&mut self, ask: Ask) -> Result<Outcome, String> {
        let outcome = self.run(ask, Cut::Never).expect("no cut").map_err(|e| e.to_string())?;
        self.check(&outcome)?;
        Ok(outcome)
    }

    /// Two commands, each completion handed to one the simulation's coin picks.
    fn interleave(&mut self) -> Result<(), String> {
        let mut lives = [self.start(Ask::Changed), self.start(Ask::Changed)];
        let mut done = [false; 2];
        let mut idle = 0;
        while !(done[0] && done[1]) {
            let ready: Vec<usize> = (0..2).filter(|&n| !done[n] && !lives[n].inbox.is_empty()).collect();
            if ready.is_empty() {
                let mut open: Vec<&mut Live> =
                    lives.iter_mut().zip(done).filter(|(_, done)| !done).map(|(live, _)| live).collect();
                match self.pump(&mut open) {
                    true => idle = 0,
                    false => {
                        idle += 1;
                        assert!(idle < 1_000, "two commands stalled");
                    }
                }
                continue;
            }
            let n = match ready.len() {
                2 => usize::from(self.sim.rng().coin()),
                _ => ready[0],
            };
            let mut stepped = self.deliver(&mut lives[n]);
            if let Stepped::Flush = stepped {
                stepped = self.flush(&mut lives[n]);
            }
            if let Stepped::Done(result) = stepped {
                done[n] = true;
                let outcome = result.map_err(|e| e.to_string())?;
                self.check(&outcome)?;
            }
        }
        Ok(())
    }

    /// The invariant of every run: each answer is a fresh state's, only files a step reads are
    /// hashed, a slot is written one past the one opened and never twice, and a session is
    /// lost only where the store was.
    fn check(&mut self, outcome: &Outcome) -> Result<(), String> {
        let fresh = fresh(&self.model);
        for (path, answer) in &outcome.answers {
            if fresh.get(path) != Some(answer) {
                return Err(format!("items of {path}: the command answered {answer:x}, a fresh state {:?}", fresh.get(path)));
            }
        }
        for path in &outcome.hashed {
            if !(path.ends_with(".rs") && self.model.contains_key(path) && in_view(&self.model, path)) {
                return Err(format!("{path} was hashed, and no step reads it"));
            }
        }
        if matches!(outcome.opened, Opened::Lost { .. }) && !self.may_lose {
            return Err("a session opened lost with no store lost: a slot names records the store never kept".to_owned());
        }
        if let Some(wrote) = outcome.wrote_sequence {
            if wrote != outcome.opened_sequence + 1 {
                return Err(format!("slot {wrote} written over a command that opened {}", outcome.opened_sequence));
            }
            if !self.written.insert(wrote) {
                return Err(format!("slot sequence {wrote} written twice: a command that lost the race kept its save"));
            }
        }
        Ok(())
    }

    /// The table on disk: its file rows are the view's files, and each hashed row's id is its
    /// file's.
    fn check_table(&self) -> Result<(), String> {
        let bytes = self.sim.contents(&name(&format!("{ST}/table")));
        let table = Table::decode(&bytes).map_err(|e| format!("{ST}/table: {e}"))?;
        let rows: BTreeMap<&str, &Row> =
            table.rows.iter().filter(|row| row.kind == Kind::File).map(|row| (row.path.as_str(), row)).collect();
        let view: BTreeSet<&str> =
            self.model.keys().filter(|path| in_view(&self.model, path)).map(String::as_str).collect();
        let held: BTreeSet<&str> = rows.keys().copied().collect();
        if held != view {
            return Err(format!("the table's files {held:?} are not the view's {view:?}"));
        }
        for (path, row) in rows {
            let now = ContentId::of(blaze_vfs::FILE_DOMAIN, &self.model[path]);
            if row.id.is_some_and(|id| id != now) {
                return Err(format!("{path}: the table holds the id of bytes it no longer has"));
            }
        }
        Ok(())
    }

    /// Past every racy window: two runs, the second with nothing to do; the table true to the
    /// disk; and every file asked answered as a fresh state does.
    fn settle(&mut self) -> Result<(), String> {
        self.sim.advance(Duration::from_secs(3));
        self.run_ok(Ask::Changed)?;
        let again = self.run_ok(Ask::Changed)?;
        if again.report.hashed != 0 || again.report.repaired != 0 {
            return Err(format!(
                "a run after a run with nothing changed hashed {} files and repaired {} entries",
                again.report.hashed, again.report.repaired
            ));
        }
        self.check_table()?;
        let all = self.run_ok(Ask::All)?;
        let got: BTreeMap<String, ContentId> = all.answers.into_iter().collect();
        let fresh = fresh(&self.model);
        if got != fresh {
            return Err(format!("every file asked: {got:?}, a fresh state {fresh:?}"));
        }
        Ok(())
    }

    fn apply(&mut self, op: Op) -> Result<(), String> {
        match op {
            Op::Write(file, n) => self.put(FILES[file as usize % FILES.len()], content(n)),
            Op::Remove(file) if file as usize % FILES.len() != 0 => self.delete(FILES[file as usize % FILES.len()]),
            Op::Remove(_) => {}
            Op::Rename(from, to) => {
                let (from, to) = (FILES[from as usize % FILES.len()], FILES[to as usize % FILES.len()]);
                if from != to && from != "keep.txt" && self.model.contains_key(from) {
                    self.sim.rename(&name(&format!("{CO}/{from}")), &name(&format!("{CO}/{to}")));
                    let bytes = self.model.remove(from).expect("held");
                    self.model.insert(to.to_owned(), bytes);
                }
            }
            Op::Touch(file) => {
                let path = FILES[file as usize % FILES.len()];
                if let Some(bytes) = self.model.get(path).cloned() {
                    self.put(path, bytes);
                }
            }
            Op::Tag(dir, what) => {
                let path = format!("{}/CACHEDIR.TAG", TAG_DIRS[dir as usize % TAG_DIRS.len()]);
                match what % 3 {
                    0 => self.delete(&path),
                    1 => self.put(&path, [&SIGNATURE[..], b"\n# a cache\n"].concat()),
                    _ => self.put(&path, b"Signature: not a cache directory tag, but as long".to_vec()),
                }
            }
            Op::Tick(ms) => self.sim.advance(Duration::from_millis(u64::from(ms))),
            Op::Run => {
                self.run_ok(Ask::Changed)?;
            }
            Op::Interleave => self.interleave()?,
            Op::Crash(at) => match self.run(Ask::Changed, Cut::At(usize::from(at))) {
                Some(Ok(outcome)) => self.check(&outcome)?,
                Some(Err(e)) => return Err(e.to_string()),
                None => {}
            },
            Op::LoseStore => {
                self.store = Shelf::default();
                self.may_lose = true;
            }
        }
        Ok(())
    }
}

/// A stamp's times floored to whole seconds.
fn floor(done: &mut Done) {
    let floor = |stamp: &mut Stamp| {
        if !stamp.is_gone() {
            stamp.mtime_ns -= stamp.mtime_ns % SECOND;
            stamp.ctime_ns -= stamp.ctime_ns % SECOND;
        }
    };
    match done {
        Done::Stamped(stamps) => stamps.iter_mut().for_each(floor),
        Done::Entries(entries) => entries.iter_mut().for_each(|(_, stamp)| floor(stamp)),
        _ => {}
    }
}

/// Whether a fresh look at the checkout sees `path`: not out by name, and under no directory
/// (below the checkout) holding a cache directory tag.
fn in_view(model: &BTreeMap<String, Vec<u8>>, path: &str) -> bool {
    if fixed_out(path) {
        return false;
    }
    let mut at = path;
    while let Some((up, _)) = at.rsplit_once('/') {
        let tag = model.get(&format!("{up}/CACHEDIR.TAG"));
        if tag.is_some_and(|bytes| bytes.len() >= SIGNATURE.len() && bytes[..SIGNATURE.len()] == SIGNATURE[..]) {
            return false;
        }
        at = up;
    }
    true
}

/// `items` of every `.rs` file in the view, from nothing: a new kernel, session and store.
fn fresh(model: &BTreeMap<String, Vec<u8>>) -> BTreeMap<String, ContentId> {
    let files: Vec<(&String, &Vec<u8>)> =
        model.iter().filter(|(path, _)| path.ends_with(".rs") && in_view(model, path)).collect();
    let rows: Vec<Row> = files
        .iter()
        .map(|(path, bytes)| Row {
            path: (*path).clone(),
            stamp: Stamp::GONE,
            id: Some(ContentId::of(blaze_vfs::FILE_DOMAIN, bytes)),
            kind: Kind::File,
        })
        .collect();
    let texts = files.iter().map(|(path, bytes)| ((*path).clone(), (*bytes).clone())).collect();
    let world = SoloWorld::new(&rows, texts);
    let kernel = kernel(&codes()).expect("the steps register");
    let mut session = Session::new(&world);
    let mut store = Shelf::default();
    rows.iter()
        .map(|row| {
            let input = FileKey { name: row.path.clone(), reading: reading() };
            let got = kernel
                .get_in::<Items<SoloWorld>, _>(&mut session, &mut store, &world, &input, &Setting::OFF)
                .expect("a fresh state answers");
            (row.path.clone(), ContentId::of(ANSWER_DOMAIN, got.value()))
        })
        .collect()
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Write(u8, u8),
    Remove(u8),
    Rename(u8, u8),
    /// The file written again with the bytes it holds: its stamp moves, its id does not.
    Touch(u8),
    /// A tag in one of [`TAG_DIRS`]: removed, a cache directory tag, or a file of the name
    /// that is none.
    Tag(u8, u8),
    Tick(u16),
    Run,
    Interleave,
    /// A run crashed before its `n`th delivery (its flush counts as one).
    Crash(u8),
    /// The store lost whole: every slot names a root it lacks.
    LoseStore,
}

fn op(tape: &Tape) -> Op {
    let file = tape.int_in(0..=FILES.len() as u8 - 1, "file");
    let other = tape.int_in(0..=FILES.len() as u8 - 1, "other file");
    match tape.int_in(0..=15u8, "op") {
        0..=2 => Op::Write(file, tape.int_in(0..=12u8, "content")),
        3 => Op::Remove(file),
        4 => Op::Rename(file, other),
        5 => Op::Touch(file),
        6 => Op::Tag(file, other),
        7 => Op::Tick(tape.int_in(0..=3_000u16, "tick ms")),
        8 => Op::Tick(5),
        9 => Op::Interleave,
        10 => Op::Crash(tape.int_in(0..=40u8, "crash before delivery")),
        11 => Op::LoseStore,
        _ => Op::Run,
    }
}

fn scenario(seed: u64, coarse: bool, ops: &[Op]) -> Result<(), String> {
    let mut h = Harness::new(seed, coarse);
    for (n, op) in ops.iter().enumerate() {
        h.apply(*op).map_err(|why| format!("op {n} ({op:?}): {why}"))?;
    }
    h.settle().map_err(|why| format!("settling after {} ops: {why}", ops.len()))
}

static CONTROLS: Once = Once::new();

/// The invariant: over any edits, adds, removes, renames, touches, writes in and out of the
/// racy window (on a fine clock and a coarse one), cache directory tags added and removed,
/// runs alone, two commands interleaved at every completion, crashes before any delivery
/// (between the run's puts and their flush, between the flush and the slot, inside the
/// table's and the slot's writes) and a store lost whole: every `items` answer a command gives
/// is a fresh state's; a command hashes only files a step reads; a slot is written one past
/// the one its command opened and never twice; a session is `lost` only where the store was;
/// and once settled, a run after a run with nothing changed hashes 0 files and repairs 0
/// entries, the table on disk holds exactly the view's files with their bytes' ids, and every
/// file asked is answered as a fresh state answers it.
#[test]
fn a_command_answers_as_a_fresh_state_does() {
    CONTROLS.call_once(each_planted_bug_is_caught);
    blaze_sim::check(SimConfig::cases(200), |tape| {
        let seed = tape.next_u64("sim seed");
        let coarse = tape.bool("coarse");
        let ops: Vec<Op> = tape.vec(0..=30, "ops", op);
        if let Err(why) = scenario(seed, coarse, &ops) {
            panic!("seed {seed}, coarse {coarse}: {why}");
        }
    });
}

/// A file rewritten within its racy window, to the same length in the same coarse tick: its
/// stamp is equal, its bytes are not.
fn racy_rewrite(h: &mut Harness) -> Result<(), String> {
    h.coarse = true;
    let now = h.sim.wall_ns();
    let next = (now / SECOND + 1) * SECOND + SECOND / 10;
    h.sim.advance(Duration::from_nanos(next - now));
    h.put("a.rs", content(1));
    h.sim.advance(Duration::from_millis(10));
    h.run_ok(Ask::Changed)?;
    h.sim.advance(Duration::from_millis(10));
    h.put("a.rs", content(2));
    h.sim.advance(Duration::from_millis(10));
    h.run_ok(Ask::Changed)?;
    h.settle()
}

fn removed(h: &mut Harness) -> Result<(), String> {
    h.put("a.rs", content(1));
    h.put("b.rs", content(2));
    h.run_ok(Ask::Changed)?;
    h.delete("b.rs");
    h.run_ok(Ask::Changed)?;
    h.settle()
}

/// A command that read the slot file, then another command's whole run, then the first's
/// save.
fn raced(h: &mut Harness) -> Result<(), String> {
    h.put("a.rs", content(1));
    let mut first = h.start(Ask::Changed);
    let mut read = 0;
    while read < 2 {
        if first.inbox.is_empty() {
            h.pump(&mut [&mut first]);
            continue;
        }
        match h.deliver(&mut first) {
            Stepped::Pending => read += 1,
            _ => return Err("a command finished on reading its table and slot".to_owned()),
        }
    }
    let second = h.run(Ask::Changed, Cut::Never).expect("no cut").map_err(|e| e.to_string())?;
    h.check(&second)?;
    let first = h.finish(first, Cut::Never).expect("no cut").map_err(|e| e.to_string())?;
    h.check(&first)?;
    h.settle()
}

/// A crash at the store's flush keeping none of the run's puts, then a run.
fn crash_at_flush(h: &mut Harness) -> Result<(), String> {
    h.put("a.rs", content(1));
    h.keep_none = true;
    if let Some(result) = h.run(Ask::Changed, Cut::Flush) {
        h.check(&result.map_err(|e| e.to_string())?)?;
    }
    h.keep_none = false;
    h.run_ok(Ask::Changed)?;
    h.settle()
}

/// `scenario` passes on every case of `cases` with nothing planted, and with `plant` fails on
/// at least one (`every`: on each); a case's simulation seed is drawn from its tape.
fn caught(plant: Plant, scenario: fn(&mut Harness) -> Result<(), String>, cases: u32, every: bool) {
    let ran = |seed: u64, plant: Plant| {
        PLANT.set(plant);
        let result = scenario(&mut Harness::new(seed, false));
        PLANT.set(Plant::None);
        result
    };
    if every {
        blaze_sim::check(SimConfig::cases(cases), |tape| {
            let seed = tape.next_u64("sim seed");
            if let Err(why) = ran(seed, Plant::None) {
                return Err(Broke::Oracle(format!("seed {seed}, nothing planted: {why}")));
            }
            match ran(seed, plant) {
                Err(_) => Ok(()),
                Ok(()) => Err(Broke::Oracle(format!("seed {seed}: {plant:?} planted, and the oracle passed"))),
            }
        });
    } else {
        // Any finding of the oracle is the catch: `caught` asked for no words.
        blaze_sim::control(SimConfig::cases(cases), Plant::None, plant, "", |tape, plant| {
            ran(tape.next_u64("sim seed"), plant).map_err(Broke::Oracle)
        });
    }
}

/// The controls: each planted bug makes the oracle fail, and its scenario passes without it.
/// `TrustsRacy` (a racy stamp trusted: the rewrite missed), `KeepsRemoved` (a removed file's
/// row kept), `SkipsSequence` (a command that lost the race saves anyway: a sequence written
/// twice), `SlotBeforeFlush` (the slot written before the run's puts are durable: a crash
/// between leaves a slot naming records the store lost).
#[test]
fn each_planted_bug_is_caught() {
    caught(Plant::TrustsRacy, racy_rewrite, 4, true);
    caught(Plant::KeepsRemoved, removed, 4, true);
    caught(Plant::SkipsSequence, raced, 4, true);
    caught(Plant::SlotBeforeFlush, crash_at_flush, 64, false);
}

fn row(tape: &Tape) -> (String, Row) {
    const PARTS: [&str; 5] = ["a", "b", "src", "x.rs", "CACHEDIR.TAG"];
    let depth = tape.int_in(1..=3usize, "depth");
    let path: Vec<&str> = (0..depth).map(|_| PARTS[tape.index(PARTS.len(), "part")]).collect();
    let path = path.join("/");
    let stamp = Stamp {
        ino: tape.int::<u64>("ino"),
        size: tape.int::<u64>("size"),
        mtime_ns: tape.int::<u64>("mtime ns"),
        ctime_ns: tape.int::<u64>("ctime ns"),
        mode: tape.int::<u32>("mode"),
    };
    let id = tape.bool("has an id").then(|| ContentId::of("a test id", &stamp.ino.to_le_bytes()));
    let kind = [Kind::File, Kind::Dir, Kind::Link, Kind::Hidden, Kind::Tag][tape.index(5, "kind")];
    (path.clone(), Row { path, stamp, id, kind })
}

/// The refinement: a table's bytes decode to the table, and encode back to the same bytes.
#[test]
fn table_round_trips() {
    blaze_sim::check(SimConfig::cases(200), |tape| {
        let drawn: Vec<(String, Row)> = tape.vec(0..=20, "rows", row);
        let mut rows: BTreeMap<String, Row> = drawn.into_iter().collect();
        if tape.bool("has a root row") {
            rows.insert(String::new(), Row { path: String::new(), stamp: Stamp::GONE, id: None, kind: Kind::Dir });
        }
        let table = Table { instant_ns: tape.int::<u64>("instant ns"), rows: rows.into_values().collect() };
        let bytes = table.encode();
        let back = Table::decode(&bytes).expect("a table's own bytes decode");
        assert_eq!(back, table);
        assert_eq!(back.encode(), bytes);
    });
}

/// A table of another layout word is refused by that word, by `decode` and by a command,
/// which names the table's file; never read as an empty table.
#[test]
fn table_of_another_layout_is_refused_by_name() {
    let table = Table {
        instant_ns: 7,
        rows: vec![Row { path: String::new(), stamp: Stamp::GONE, id: None, kind: Kind::Dir }],
    };
    let mut bytes = table::encode(table.instant_ns, &table.rows);
    bytes[4..8].copy_from_slice(&(LAYOUT + 1).to_le_bytes());
    let refused = Table::decode(&bytes).expect_err("another layout");
    assert_eq!(refused, Undecoded::Layout { found: LAYOUT + 1 });
    assert!(refused.to_string().contains(&format!("layout word {}", LAYOUT + 1)), "{refused}");
    let mut h = Harness::new(1, false);
    h.sim.change(&name(&format!("{ST}/table")), &bytes);
    let error = h.run(Ask::Changed, Cut::Never).expect("no cut").expect_err("a table of another layout");
    assert_eq!(error.thing, format!("{ST}/table"));
    assert!(error.why.contains(&format!("layout word {}", LAYOUT + 1)), "{error}");
}

/// The newest whole slot at its own offset is the one opened: a torn one is as if never
/// written, and one at the other's offset was not written by this protocol.
#[test]
fn the_newest_whole_slot_is_opened() {
    let slot = |sequence: u64, n: u8| Slot {
        sequence,
        root: Key(ContentId::of("a test root", &[n])),
        pin: blaze_kernel::Pin(ContentId::of("a test pin", &[n])),
    };
    let mut file = vec![0; 2 * SLOT_BYTES];
    let at = |sequence: u64| Slot::offset(sequence) as usize;
    file[at(1)..at(1) + SLOT_BYTES].copy_from_slice(&slot(1, 1).encode());
    assert_eq!(root::newest(&file), Some(slot(1, 1)));
    file[at(2)..at(2) + SLOT_BYTES].copy_from_slice(&slot(2, 2).encode());
    assert_eq!(root::newest(&file), Some(slot(2, 2)));
    file[at(2) + 20] ^= 1;
    assert_eq!(root::newest(&file), Some(slot(1, 1)), "a torn slot is the older one");
    file[at(2)..at(2) + SLOT_BYTES].copy_from_slice(&slot(3, 3).encode());
    assert_eq!(root::newest(&file), Some(slot(1, 1)), "a slot at the other's offset is none");
    assert_eq!(root::newest(&[]), None);
}
