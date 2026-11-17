//! The runner's host, simulated as `blaze test` drives it: per request, the selection driven
//! in one snapshot until its runs are out, detached, its runs finished by a scripted queue in an
//! order the tape picks, each given back and the selection driven again ([`Driver::settled`]),
//! its line said from the verdict that returns, to its end. The store is a map that never
//! overwrites; the program is a model binary of a few functions whose tests each execute some
//! of them and read one file. Each run's reads are assembled from the lines the sandbox writes
//! for a test (`trace::assemble_seen`), as the runner assembles them: a test server's spec
//! names no given variables, so every test reads the whole environment from the start
//! (`E\t\t*`), beside its file. Until 2026-11-14 the script handed the machine a hand-made
//! read of one file and read verdicts only from the report: the runner's whole-environment
//! read, which the host answered as a variable no one has, and its line read before the drive
//! that settles it, were both outside it, and every real run went unkept unseen.
//!
//! The invariant: across a sequence of requests (the first, a no-op, an edit of one function,
//! its undo, an edit of one file, a run of every test, a no-op), a test is reported cached only
//! when it passes now and everything it executed and read is as it was at a kept pass; and it
//! runs exactly when that is not so (or every test is asked, or it failed last time): the
//! no-op runs only the failing tests, the edit runs exactly the tests that executed the
//! function, the undo runs only the failing tests, `--run-all` runs every test. Each run's line
//! says the verdict the request ends with, and why it ran ([`Missed`]).

use std::alloc::Global;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use blaze_core::Traced;
use blaze_ids::ContentId;
use blaze_keyed::{
    kind::{Key, Namespace, Tag, TestIndex, ToolId},
    proof,
    store::Put,
};
use blaze_sim::{Broke, Config, Tape};
use blaze_usercode::trace::{self, Entry, Inputs, Names, Seen};

use super::{BaseOf, Driver, Drove, Ended, HostError, Out, Program, Records, base};
use crate::{
    PLANT, Plant,
    machine::{Missed, NotKept, Selection, Start, Test, Verdict},
    record::{Record, Table},
    table::{Bound, FnId, Image, Version},
};

const FNS: usize = 6;
/// The binary's jump table, which every record of the model names: the model edits what its
/// functions are, never which slot each is in.
const TABLE: Table = Table { lineage: [6; 16], generation: 1, slots: FNS as u32 };
const TESTS: usize = 4;
const FILES: usize = 2;

fn function(at: usize) -> FnId {
    FnId(ContentId::of("host sim fn", &[at as u8]))
}

fn bound(at: usize, version: u8) -> Bound {
    Bound(ContentId::of("host sim bound", &[at as u8, version]))
}

fn file(at: usize) -> PathBuf {
    PathBuf::from(format!("/w/data/{at}"))
}

fn content(value: u8) -> [u8; 32] {
    ContentId::of("host sim file", &[value]).0
}

/// The store: a map that refuses to overwrite.
#[derive(Default)]
struct Mem {
    records: BTreeMap<(u16, ContentId), Vec<u8, Global>>,
}

impl Records for Mem {
    type Error = String;

    fn get(&mut self, tag: Tag, key: ContentId) -> Result<Option<&[u8]>, String> {
        Ok(self.records.get(&(tag.0, key)).map(|bytes| &bytes[..]))
    }

    fn put(&mut self, tag: Tag, key: ContentId, _: Namespace, bytes: &[u8]) -> Result<Put, String> {
        match self.records.get(&(tag.0, key)) {
            Some(held) if held[..] == bytes[..] => Ok(Put::Same),
            Some(_) => Ok(Put::Other),
            None => {
                let mut owned = Vec::with_capacity_in(bytes.len(), Global);
                owned.extend_from_slice(bytes);
                self.records.insert((tag.0, key), owned);
                Ok(Put::Written)
            }
        }
    }
}

/// The environment blaze gives each test: in its base key, and what a walk of it sees.
fn env() -> Vec<(OsString, OsString)> {
    vec![("HOME".into(), "/h".into()), ("PATH".into(), "/bin".into())]
}

/// The files now, and the environment blaze gives the tests (sorted by name).
struct Disk {
    files: [u8; FILES],
    env: Vec<(OsString, OsString)>,
}

impl Inputs for Disk {
    type Error = String;

    fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, String> {
        Ok((0..FILES).find(|at| file(*at) == path).map(|at| content(self.files[at])))
    }

    fn dir(&mut self, _: &Path) -> Result<Option<[u8; 32]>, String> {
        Ok(None)
    }

    fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
        let at = self.env.binary_search_by(|it| it.0.as_os_str().cmp(name)).ok()?;
        Some(&self.env[at].1)
    }

    fn env_all(&mut self) -> &[(OsString, OsString)] {
        &self.env
    }

    fn names(&mut self, _: &Path, _: &mut Names<'_>) -> Result<bool, String> {
        Ok(false)
    }

    fn entry(&mut self, _: &Path) -> Result<Entry, String> {
        Ok(Entry::Missing)
    }
}

/// The program and its tests.
#[derive(Clone)]
struct Model {
    versions: [u8; FNS],
    files: [u8; FILES],
    /// Per test, the functions it executes (sorted), the file it reads and whether it fails.
    executes: [Vec<usize>; TESTS],
    reads: [usize; TESTS],
    fails: [bool; TESTS],
}

impl Model {
    fn draw(tape: &Tape) -> Model {
        let mut executes: [Vec<usize>; TESTS] = Default::default();
        let mut reads = [0; TESTS];
        let mut fails = [false; TESTS];
        for test in 0..TESTS {
            executes[test] = (0..FNS).filter(|_| tape.coin("executes")).collect();
            if executes[test].is_empty() {
                executes[test].push(tape.index(FNS, "the one it executes"));
            }
            reads[test] = tape.index(FILES, "the file it reads");
            // Test 0 always passes: an edit of what it executes has a test to rerun.
            fails[test] = test != 0 && tape.one_in(4, "it fails");
        }
        Model { versions: [0; FNS], files: [0; FILES], executes, reads, fails }
    }

    /// The binary: every function has a slot, slot `n` holding function `n`.
    fn image(&self) -> Result<Image, Broke> {
        let fns = (0..FNS).map(|at| Version { id: function(at), bound: bound(at, self.versions[at]) }).collect();
        Image::new(fns, (0..FNS as u32).collect(), ContentId::of("host sim beside", &[]), TABLE)
            .map_err(|why| Broke::Harness(why.to_string()))
    }

    /// What test `test` depends on now: its functions' versions and its file's value.
    fn depends(&self, test: usize) -> (Vec<u8>, u8) {
        (self.executes[test].iter().map(|at| self.versions[*at]).collect(), self.files[self.reads[test]])
    }
}

fn name(test: usize) -> String {
    format!("model::t{test}")
}

fn tool() -> ToolId {
    ToolId { tool: ContentId::of("host sim tool", &[]), target: ContentId::of("host sim target", &[]), namespace: Namespace::LOCAL }
}

/// The runner across requests: its store, the suite's head and the binary the head is of.
#[derive(Default)]
struct Runner {
    store: Mem,
    head: Option<Key<TestIndex>>,
    before: Option<Image>,
    /// Per test, what it depended on at each kept pass.
    kept: [Vec<(Vec<u8>, u8)>; TESTS],
}

fn harness(why: HostError) -> Broke {
    Broke::Harness(why.to_string())
}

/// One request, driven as `blaze test` drives it.
fn request(runner: &mut Runner, model: &Model, run_all: bool, tape: &Tape) -> Result<Vec<Verdict>, Broke> {
    let image = model.image()?;
    let names: Vec<String> = (0..TESTS).map(name).collect();
    let given = env();
    let tests: Vec<Test<'_>> = names
        .iter()
        .map(|name| {
            let base = base(&BaseOf { tool: tool(), settings: ContentId::of("host sim suite", &[]), env: &given, name, attrs: ContentId::default() });
            Test { base, name }
        })
        .collect();
    let mut disk = Disk { files: model.files, env: given.clone() };
    // By test: the verdict its line said, from the drive after its run.
    let mut said: Vec<Option<Verdict>> = vec![None; TESTS];
    let mut driver = Driver::default();
    let mut runs = Vec::new_in(Global);
    let program = Program { now: &image, before: runner.before.as_ref() };
    let (state, head) = (image.state(), runner.head);
    let store = &mut runner.store;
    let (mut selection, mut drove) = proof::scope(|snap| -> Result<(Selection<'static, Global>, Drove), Broke> {
        let start = Start {
            tool: tool(),
            suite: ContentId::of("host sim suite", &[]),
            program: state,
            head,
            certified: None,
            run_all,
            workers: TESTS as u32,
            compact_at: 4,
        };
        let mut selection = Selection::new(snap, start, &tests, Global).map_err(|why| Broke::Harness(why.to_string()))?;
        let drove = driver.drive(&mut selection, store, program, &mut disk, &mut runs).map_err(harness)?;
        Ok((selection.detach().map_err(|why| Broke::Harness(why.to_string()))?, drove))
    })?;
    // The queue: every run out at once, finished in an order the tape picks.
    let mut out: Vec<Out, Global> = std::mem::replace(&mut runs, Vec::new_in(Global));
    while !out.is_empty() {
        let Out { ticket, test } = out.swap_remove(tape.index(out.len(), "which run ends next"));
        let test = test as usize;
        let passed = !model.fails[test];
        let hits: Vec<u32> = model.executes[test].iter().map(|at| *at as u32).collect();
        let record = Record::Complete { table: TABLE, hits };
        // What the sandbox writes for the test: the whole environment from the start (its spec
        // names no given variables), then its file; assembled against the disk now, as the
        // runner assembles a test's lines once it ends.
        let read = file(model.reads[test]).to_string_lossy().into_owned();
        let lines = [(Seen::Env, trace::ALL_ENV), (Seen::Read, read.as_str())];
        let traced: Traced = trace::assemble_seen(lines, &mut disk).map_err(Broke::Harness)?;
        let reached = || Vec::new_in(Global);
        let ended = Ended {
            ticket,
            name: &names[test],
            passed,
            micros: 100 + test as u64,
            record: &record,
            reached: &reached,
            traced: Some(&traced),
            sandboxed: true,
            foreign: false,
            moved: false,
        };
        driver.ran(&mut selection, &image, &ended).map_err(harness)?;
        let (verdict, now) =
            driver.settled(&mut selection, &mut runner.store, program, &mut disk, &mut runs, test as u32).map_err(harness)?;
        drove = now;
        said[test] = Some(verdict);
        if !runs.is_empty() {
            return Err(Broke::Harness("the selection asked for a run after its runs were out".into()));
        }
    }
    if drove != Drove::Ended {
        return Err(Broke::Harness(format!("the request did not end once its runs had: {drove:?}")));
    }
    let report = match selection.outcome() {
        Some(Ok(report)) => report,
        Some(Err(why)) => return Err(Broke::Harness(why.to_string())),
        None => return Err(Broke::Harness("a request that ended with no outcome".into())),
    };
    runner.head = Some(report.head);
    let verdicts = report.verdicts.to_vec();
    // Each run's line is the verdict its request ends with, and says why it ran: none of the
    // first request's tests had a head to be found in, every `--run-all` test ran for it.
    for (test, line) in said.iter().enumerate() {
        let Some(line) = line else { continue };
        if *line != verdicts[test] {
            return Err(Broke::Oracle(format!("test {test}'s line said {line:?}, and its request ended with {:?}", verdicts[test])));
        }
        let missed = match line {
            Verdict::Ran { missed, .. } | Verdict::NotKept { missed, .. } => *missed,
            Verdict::Cached { .. } => return Err(Broke::Oracle(format!("test {test} ran and its line says cached"))),
        };
        let expected = match (run_all, head) {
            (true, _) => Some(Missed::RunAll),
            (false, None) => Some(Missed::NoHead),
            (false, Some(_)) => None,
        };
        if expected.is_some_and(|expected| expected != missed) {
            return Err(Broke::Oracle(format!("test {test} ran, and says {missed:?} for why")));
        }
    }
    runner.before = Some(image);
    for (test, verdict) in verdicts.iter().enumerate() {
        if matches!(verdict, Verdict::Ran { .. }) {
            runner.kept[test].push(model.depends(test));
        }
    }
    Ok(verdicts)
}

/// The tests that ran.
fn ran(verdicts: &[Verdict]) -> Vec<usize> {
    (0..verdicts.len()).filter(|at| !matches!(verdicts[*at], Verdict::Cached { .. })).collect()
}

/// The oracle over one request's verdicts: soundness for every test, and the runs it must make.
fn judge(step: &str, runner: &Runner, model: &Model, verdicts: &[Verdict], runs: &[usize]) -> Result<(), Broke> {
    for (test, verdict) in verdicts.iter().enumerate() {
        match verdict {
            Verdict::Cached { .. } if model.fails[test] => {
                return Err(Broke::Oracle(format!("{step}: failing test {test} reported cached")));
            }
            Verdict::Cached { .. } if !runner.kept[test].contains(&model.depends(test)) => {
                return Err(Broke::Oracle(format!(
                    "{step}: test {test} reported cached with no kept pass on what it depends on now"
                )));
            }
            Verdict::NotKept { why, .. } if model.fails[test] && *why != NotKept::Failed => {
                return Err(Broke::Oracle(format!("{step}: failing test {test} not kept as {why:?}")));
            }
            Verdict::Ran { .. } if model.fails[test] => {
                return Err(Broke::Oracle(format!("{step}: failing test {test} kept")));
            }
            _ => {}
        }
    }
    let got = ran(verdicts);
    if got != runs {
        return Err(Broke::Oracle(format!("{step}: ran {got:?}, and exactly {runs:?} can have changed (cached otherwise)")));
    }
    Ok(())
}

fn failing(model: &Model) -> Vec<usize> {
    (0..TESTS).filter(|test| model.fails[*test]).collect()
}

fn sequence(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    PLANT.set(plant);
    let result = (|| {
        let mut model = Model::draw(tape);
        let mut runner = Runner::default();
        let every: Vec<usize> = (0..TESTS).collect();

        let verdicts = request(&mut runner, &model, false, tape)?;
        judge("the first request", &runner, &model, &verdicts, &every)?;

        let verdicts = request(&mut runner, &model, false, tape)?;
        judge("a no-op", &runner, &model, &verdicts, &failing(&model))?;

        // An edit of one function test 0 executes: exactly its executors rerun.
        let edited = *tape.pick(&model.executes[0], "the function edited").unwrap_or(&0);
        model.versions[edited] = 1;
        let verdicts = request(&mut runner, &model, false, tape)?;
        let want: Vec<usize> =
            (0..TESTS).filter(|test| model.fails[*test] || model.executes[*test].contains(&edited)).collect();
        judge("an edit of one function", &runner, &model, &verdicts, &want)?;

        model.versions[edited] = 0;
        let verdicts = request(&mut runner, &model, false, tape)?;
        judge("the edit undone", &runner, &model, &verdicts, &failing(&model))?;

        let edited = tape.index(FILES, "the file edited");
        model.files[edited] = 1;
        let verdicts = request(&mut runner, &model, false, tape)?;
        let want: Vec<usize> = (0..TESTS).filter(|test| model.fails[*test] || model.reads[*test] == edited).collect();
        judge("an edit of one file", &runner, &model, &verdicts, &want)?;

        let verdicts = request(&mut runner, &model, true, tape)?;
        judge("a run of every test", &runner, &model, &verdicts, &every)?;

        let verdicts = request(&mut runner, &model, false, tape)?;
        judge("a no-op after a run of every test", &runner, &model, &verdicts, &failing(&model))
    })();
    PLANT.set(Plant::None);
    result
}

/// The invariant: the module docs'.
#[test]
fn the_runner_reruns_exactly_what_can_have_changed() {
    blaze_sim::check(Config::cases(128), |tape| sequence(tape, Plant::None));
}

/// The control: with the program's changed functions dropped, an edited function's executor is
/// reported cached, and the oracle says so.
#[test]
fn the_runner_oracle_catches_a_shortcut_that_skips_the_changes() {
    blaze_sim::control(Config::cases(64), Plant::None, Plant::ShortcutSkipsChanged, "reported cached with no kept pass", sequence);
}

/// The control: with the whole environment answered as a variable of its name (as until
/// 2026-11-14), every run that read it races its settle and is not kept, so a no-op runs every
/// test again, and the oracle says so.
#[test]
fn the_runner_oracle_catches_the_whole_environment_answered_by_name() {
    blaze_sim::control(Config::cases(32), Plant::None, Plant::WholeEnvAskedByName, "can have changed", sequence);
}
