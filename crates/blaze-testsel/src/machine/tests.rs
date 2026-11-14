//! The simulation of [`Selection`]: a model program (functions with versions and a call graph
//! their code decides, outside files, an environment variable, the code not called through a
//! slot, a clock), a host that answers the machine's effects in an order drawn from the tape from
//! a simulated store, and an oracle that reruns every asked test from scratch after each request.

use std::alloc::Global;
use std::collections::{BTreeMap, BTreeSet};

use blaze_core::Untraceable;
use blaze_format::Aligned;
use blaze_ids::{ContentId, Hasher};
use blaze_keyed::{
    kind::{Key, Kind, Namespace, Settings, TestIndex, TestInputs, TestRun, ToolId},
    pool::Ticket,
    proof,
    store::Put,
};
use blaze_sim::{Broke, Config, Tape};

use super::{By, Effect, Executed, Input, NotKept, NotWhole, Ran, Read, Selection, Start, Step, Test, Verdict};
use crate::{
    PLANT, Plant,
    ask::{self, ABSENT, Ask, Basis, ReadKind, Shape, UNLOGGED},
    index::{self, HeadLayout, POSTED, PostingsLayout, Row},
    table::{Bound, FnId},
};

const FNS: usize = 8;
const TESTS: usize = 5;
const FILES: usize = 3;
const FILE_NAMES: [&[u8]; FILES] = [b"data/0", b"data/1", b"data/2"];
const ENV: &[u8] = b"MODEL_ENV";
const WORKERS: u32 = 2;
const COMPACT_AT: u32 = 6;
/// The most steps one request may take: a machine that does not end is a failure.
const STEPS: u32 = 100_000;

#[derive(Clone, PartialEq, Eq, Debug)]
struct World {
    versions: [u8; FNS],
    unlogged: u8,
    files: [Option<u8>; FILES],
    env: Option<u8>,
    clock: u64,
}

impl World {
    fn new() -> World {
        World { versions: [0; FNS], unlogged: 0, files: [Some(0); FILES], env: None, clock: 0 }
    }

    fn program(&self) -> ContentId {
        let mut bytes = [0; FNS + 1];
        bytes[..FNS].copy_from_slice(&self.versions);
        bytes[FNS] = self.unlogged;
        ContentId::of("model program", &bytes)
    }

    fn file(&self, at: usize) -> Option<ContentId> {
        self.files[at].map(|value| ContentId::of("model file", &[value]))
    }

    fn env(&self) -> Option<ContentId> {
        self.env.map(|value| ContentId::of("model env", &[value]))
    }

    fn unlogged(&self) -> ContentId {
        ContentId::of("model unlogged", &[self.unlogged])
    }

    /// What `ask` answers now.
    fn answer(&self, ask: Ask<'_>) -> ContentId {
        match ask {
            Ask::Function(function) if function == UNLOGGED => self.unlogged(),
            Ask::Function(function) => {
                (0..FNS).find(|at| self::function(*at) == function).map_or(ABSENT, |at| bound(at, self.versions[at]).0)
            }
            Ask::Read(ReadKind::File, name) => {
                FILE_NAMES.iter().position(|file| *file == name).and_then(|at| self.file(at)).unwrap_or(ABSENT)
            }
            Ask::Read(ReadKind::Env, name) if name == ENV => self.env().unwrap_or(ABSENT),
            Ask::Read(..) => ABSENT,
        }
    }
}

fn function(at: usize) -> FnId {
    FnId(ContentId::of("model fn", &[at as u8]))
}

fn bound(at: usize, version: u8) -> Bound {
    Bound(ContentId::of("model bound", &[at as u8, version]))
}

fn mix(a: u64, b: u64) -> u64 {
    let mut z = a ^ b.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Function `at`'s code at `version`, in the program `seed` picks.
fn code(seed: u64, at: usize, version: u8) -> u64 {
    mix(seed, ((at as u64) << 8) | u64::from(version))
}

/// One run of a test, as the runner reports it.
struct RunOut {
    passed: bool,
    functions: Vec<(FnId, Bound)>,
    basis: Basis,
    reads: Vec<Read<'static>>,
    untraceable: Vec<Untraceable>,
    /// Everything its result depends on, when it is traceable.
    state: ContentId,
}

/// Runs test `test` against `world`: its entry function and every function its code calls
/// (callees always later functions), each reading what its code says.
fn run(seed: u64, world: &World, test: usize) -> RunOut {
    let mut visited = [false; FNS];
    let mut stack = vec![test];
    while let Some(at) = stack.pop() {
        if visited[at] {
            continue;
        }
        visited[at] = true;
        let code = code(seed, at, world.versions[at]);
        stack.extend((at + 1..FNS).filter(|callee| (code >> (16 + callee)) & 1 == 1));
    }
    let mut acc = mix(seed, 7);
    let mut reads = Vec::new();
    let mut untraceable = Vec::new();
    let mut clock_fails = false;
    for at in (0..FNS).filter(|at| visited[*at]) {
        let code = code(seed, at, world.versions[at]);
        acc = mix(acc, code);
        if code & 2 != 0 {
            let file = (code >> 8) as usize % FILES;
            reads.push(Read { kind: ReadKind::File, name: FILE_NAMES[file], digest: world.file(file) });
            acc = mix(acc, world.files[file].map_or(0xabc, |value| u64::from(value) + 1));
        }
        if code & 4 != 0 {
            reads.push(Read { kind: ReadKind::Env, name: ENV, digest: world.env() });
            acc = mix(acc, world.env.map_or(0xdef, |value| u64::from(value) + 1));
        }
        if code % 13 == 0 {
            untraceable.push(Untraceable::Clock);
            clock_fails |= world.clock & 1 == 1;
        }
    }
    let passed = acc % 5 != 0 && mix(seed ^ 99, u64::from(world.unlogged)) % 9 != 0 && !clock_fails;
    let (basis, functions) = if mix(seed, 1000 + test as u64) & 3 == 0 {
        (Basis::Reached, (test..FNS).map(|at| (function(at), bound(at, world.versions[at]))).collect::<Vec<_>>())
    } else {
        (Basis::Executed, (0..FNS).filter(|at| visited[*at]).map(|at| (function(at), bound(at, world.versions[at]))).collect())
    };
    let mut hasher = Hasher::new("model state");
    hasher.update(&[basis as u8]);
    let mut sorted: Vec<_> = functions.clone();
    sorted.sort_unstable();
    for (function, bound) in &sorted {
        hasher.id(&function.0).id(&bound.0);
    }
    hasher.id(&world.unlogged());
    let mut sorted_reads: Vec<_> = reads.iter().map(|read| (read.kind, read.name, read.digest.unwrap_or(ABSENT))).collect();
    sorted_reads.sort_unstable();
    sorted_reads.dedup();
    for (kind, name, digest) in &sorted_reads {
        hasher.update(&[*kind as u8]).part(name).id(digest);
    }
    RunOut { passed, functions, basis, reads, untraceable, state: hasher.finish() }
}

fn tool() -> ToolId {
    ToolId { tool: ContentId::of("model tool", &[]), target: ContentId::of("model target", &[]), namespace: Namespace::LOCAL }
}

fn suite() -> ContentId {
    ContentId::of("model suite", &[])
}

fn base(test: usize) -> Key<TestRun> {
    TestRun::key(&TestInputs {
        tool: tool(),
        settings: Settings(ContentId::of("model settings", &[])),
        env: ContentId::of("model declared env", &[]),
        item: ContentId::of("model test", &[test as u8]),
        entry_args: ABSENT,
        attrs: ContentId::of("model attrs", &[test as u8]),
        args: &[],
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Edit(usize, u8),
    /// Undo the last edit not undone.
    Undo,
    File(usize, Option<u8>),
    Env(Option<u8>),
    Unlogged(u8),
    Tick,
    /// Ask the tests in `mask` (every test when 0); with `race`, a file the first run read
    /// changes while it runs.
    Request { mask: u8, race: bool },
    RunAll,
}

const REQUEST: Op = Op::Request { mask: 0, race: false };

/// An effect as the host keeps it until it answers it.
enum Pending {
    Get { ticket: Ticket, tag: u16, key: ContentId },
    Changes { ticket: Ticket, since: ContentId },
    Answer { ticket: Ticket, shape: Vec<u8> },
    Run { ticket: Ticket, test: u32 },
    Put { ticket: Ticket, tag: u16, key: ContentId, bytes: Vec<u8> },
}

impl Pending {
    fn of(effect: &Effect<'_>) -> Pending {
        match *effect {
            Effect::Get { ticket, tag, key, .. } => Pending::Get { ticket, tag: tag.0, key },
            Effect::Changes { ticket, since } => Pending::Changes { ticket, since },
            Effect::Answer { ticket, shape, .. } => Pending::Answer { ticket, shape: shape.to_vec() },
            Effect::Run { ticket, test } => Pending::Run { ticket, test },
            Effect::Put { ticket, tag, key, bytes, .. } => Pending::Put { ticket, tag: tag.0, key, bytes: bytes.to_vec() },
        }
    }
}

struct Host {
    store: BTreeMap<(u16, ContentId), Aligned>,
    head: Option<Key<TestIndex>>,
    programs: BTreeMap<ContentId, World>,
    /// Per test, the states of its kept passes.
    filed: [BTreeSet<ContentId>; TESTS],
}

/// What a simulation ended with: per request, each asked test's verdict; the store; and which
/// of the oracle's clauses had something to judge.
struct Sim {
    requests: Vec<Vec<(usize, Verdict)>>,
    store: Vec<((u16, ContentId), Vec<u8>)>,
    reached: Reached,
}

/// How often each situation the oracle judges came up: a clause never reached proves nothing.
#[derive(Clone, Copy, Default, Debug)]
struct Reached {
    /// A row cached as unchanged in a request after an edit: the shortcut judged against an
    /// edit elsewhere.
    unchanged_after_edit: u32,
    /// A test cached by its answers in a request after an undo: an undone edit reused.
    answers_after_undo: u32,
    /// A run that read what cannot be traced.
    untraceable_runs: u32,
    /// A cached test whose fresh rerun the oracle compared (the stale clause had a case).
    cached_judged: u32,
    /// A test whose fresh state was kept before, judged for exactness.
    kept_judged: u32,
    failed_runs: u32,
    raced: u32,
    compactions: u32,
}

impl Reached {
    /// Each count, in the order of [`POINTS`].
    fn counts(&self) -> [u32; 8] {
        let Reached {
            unchanged_after_edit,
            answers_after_undo,
            untraceable_runs,
            cached_judged,
            kept_judged,
            failed_runs,
            raced,
            compactions,
        } = *self;
        [unchanged_after_edit, answers_after_undo, untraceable_runs, cached_judged, kept_judged, failed_runs, raced, compactions]
    }
}

/// What each count of [`Reached`] means, as the point a case that counted it reaches.
const POINTS: [&str; 8] = [
    "a row cached as unchanged after an edit elsewhere",
    "a test cached by its answers after an undone edit",
    "a run that read what cannot be traced",
    "a cached test rerun by the oracle",
    "a test whose state now was kept before",
    "a failing run",
    "a request with a race",
    "a compaction of the postings",
];

/// Which clause of the oracle a request broke.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Clause {
    /// (a) A test reported cached fails when rerun now.
    StaleCached,
    /// (d) A test reported cached reads, rerun now, what cannot be traced.
    CachedUntraceable,
    /// (b) A test rerun whose state now was kept before.
    Reran,
    /// A run that failed, reported as a cached pass.
    FailureCached,
    /// A test both run and reported cached.
    RanAndCached,
    /// A run that read what cannot be traced, kept.
    UntraceableKept,
    /// A run that failed, reported otherwise than not kept for failing.
    FailureMisreported,
    /// A test neither run nor cached.
    NeitherRanNorCached,
    /// A run of every test that cached one.
    RunAllCached,
    /// More tests run at once than there are workers.
    OverWorkers,
}

/// Why a simulation stopped: an oracle clause broken, or the machine or host broke.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Failure {
    Violation { clause: Clause, what: String },
    Broke(String),
}

impl From<String> for Failure {
    fn from(what: String) -> Failure {
        Failure::Broke(what)
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Violation { clause, what } => write!(f, "{clause:?}: {what}"),
            Failure::Broke(what) => write!(f, "broke: {what}"),
        }
    }
}

fn violation<T>(clause: Clause, what: String) -> Result<T, Failure> {
    Err(Failure::Violation { clause, what })
}

/// A failure as blaze-sim's: a clause broken is the oracle's finding, anything else the
/// harness's.
fn broke(failure: Failure) -> Broke {
    match failure {
        Failure::Violation { .. } => Broke::Oracle(failure.to_string()),
        Failure::Broke(what) => Broke::Harness(format!("broke: {what}")),
    }
}

/// Runs `ops` on the program `seed` picks, the host answering in the order `order` draws.
fn sim(ops: &[Op], seed: u64, order: &Tape, plant: Plant) -> Result<Sim, Failure> {
    PLANT.set(plant);
    let result = (|| -> Result<Sim, Failure> {
        let mut host = Host { store: BTreeMap::new(), head: None, programs: BTreeMap::new(), filed: Default::default() };
        let mut world = World::new();
        let mut edits: Vec<(usize, u8)> = Vec::new();
        let mut requests = Vec::new();
        let mut reached = Reached::default();
        // Since the last request: an edit, an undo.
        let mut since = Since::default();
        for op in ops {
            match *op {
                Op::Edit(at, version) => {
                    edits.push((at, world.versions[at]));
                    world.versions[at] = version;
                    since.edited = true;
                }
                Op::Undo => {
                    if let Some((at, version)) = edits.pop() {
                        world.versions[at] = version;
                        since.edited = true;
                        since.undone = true;
                    }
                }
                Op::File(at, value) => world.files[at] = value,
                Op::Env(value) => world.env = value,
                Op::Unlogged(value) => world.unlogged = value,
                Op::Tick => world.clock += 1,
                Op::Request { mask, race } => {
                    let tests: Vec<usize> = (0..TESTS).filter(|test| mask == 0 || mask & (1 << test) != 0).collect();
                    let asked = Asked { tests: &tests, run_all: false, race, since };
                    requests.push(request(&mut host, &mut world, seed, order, &asked, &mut reached)?);
                    since = Since::default();
                }
                Op::RunAll => {
                    let tests: Vec<usize> = (0..TESTS).collect();
                    let asked = Asked { tests: &tests, run_all: true, race: false, since };
                    requests.push(request(&mut host, &mut world, seed, order, &asked, &mut reached)?);
                    since = Since::default();
                }
            }
        }
        let store = host.store.iter().map(|(key, bytes)| (*key, bytes.to_vec())).collect();
        Ok(Sim { requests, store, reached })
    })();
    PLANT.set(Plant::None);
    result
}

/// What changed in the program since the last request.
#[derive(Clone, Copy, Default, Debug)]
struct Since {
    edited: bool,
    undone: bool,
}

/// One request's ask.
struct Asked<'a> {
    tests: &'a [usize],
    run_all: bool,
    /// A file the first run read changes while it runs.
    race: bool,
    since: Since,
}

/// One request: the machine run against the host to its end, then the oracle.
fn request(
    host: &mut Host,
    world: &mut World,
    seed: u64,
    order: &Tape,
    asked: &Asked<'_>,
    reached: &mut Reached,
) -> Result<Vec<(usize, Verdict)>, Failure> {
    let Asked { tests: asked, run_all, race, since } = *asked;
    host.programs.insert(world.program(), world.clone());
    let names: Vec<String> = asked.iter().map(|test| format!("model::t{test}")).collect();
    let tests: Vec<Test<'_>> = asked.iter().zip(&names).map(|(test, name)| Test { base: base(*test), name }).collect();
    let mut ran: Vec<Option<RunOut>> = asked.iter().map(|_| None).collect();
    let mut raced = false;
    let mut compacted = false;
    let verdicts = proof::scope(|snap| -> Result<Vec<Verdict>, Failure> {
        let start = Start {
            tool: tool(),
            suite: suite(),
            program: world.program(),
            head: host.head,
            certified: None,
            run_all,
            workers: WORKERS,
            compact_at: COMPACT_AT,
        };
        let mut selection = Selection::new(snap, start, &tests, Global).map_err(|why| why.to_string())?;
        let mut pending: Vec<Pending> = Vec::new();
        let mut ended = false;
        for _ in 0..STEPS {
            let step = selection.next();
            match step {
                Step::Do(effect) => {
                    pending.push(Pending::of(&effect));
                    continue;
                }
                Step::Exit => {
                    ended = true;
                    break;
                }
                Step::Wait(_) => {}
            }
            let running = pending.iter().filter(|pending| matches!(pending, Pending::Run { .. })).count();
            if running > WORKERS as usize {
                return violation(Clause::OverWorkers, format!("{running} tests run at once, over {WORKERS} workers"));
            }
            if pending.is_empty() {
                return Err(Failure::Broke("the machine waits with nothing asked".into()));
            }
            match pending.swap_remove(order.index(pending.len(), "which answer")) {
                Pending::Get { ticket, tag, key } => {
                    let record = host.store.get(&(tag, key)).map(|bytes| &bytes[..]);
                    selection.give(Input::Got { ticket, record });
                }
                Pending::Changes { ticket, since } => {
                    let Some(then) = host.programs.get(&since) else {
                        return Err(Failure::Broke(format!("changes asked since a program state never seen: {since:?}")));
                    };
                    let mut functions: Vec<FnId> =
                        (0..FNS).filter(|at| then.versions[*at] != world.versions[*at]).map(function).collect();
                    if then.unlogged != world.unlogged {
                        functions.push(UNLOGGED);
                    }
                    selection.give(Input::Changes { ticket, functions: &functions });
                }
                Pending::Answer { ticket, shape } => {
                    let shape = Shape::decode(&shape).map_err(|why| why.to_string())?;
                    let now: Vec<ContentId> = shape.asks().map(|ask| world.answer(ask)).collect();
                    selection.give(Input::Answers { ticket, now: &now });
                }
                Pending::Run { ticket, test } => {
                    let at = test as usize;
                    let out = run(seed, world, asked[at]);
                    let unlogged = world.unlogged();
                    if race && !raced {
                        raced = true;
                        let file = out
                            .reads
                            .iter()
                            .find(|read| read.kind == ReadKind::File)
                            .and_then(|read| FILE_NAMES.iter().position(|file| *file == read.name))
                            .unwrap_or(0);
                        world.files[file] = Some(world.files[file].map_or(0, |value| value.wrapping_add(1)));
                    }
                    let executed = match out.basis {
                        Basis::Executed => Executed::Whole(&out.functions),
                        Basis::Reached => Executed::Reached { why: NotWhole::NotArmed, functions: &out.functions },
                    };
                    let given = Ran {
                        passed: out.passed,
                        micros: 10 + asked[at] as u64,
                        executed,
                        unlogged,
                        reads: &out.reads,
                        untraceable: &out.untraceable,
                        sandboxed: true,
                    };
                    selection.give(Input::Ran { ticket, ran: given });
                    ran[at] = Some(out);
                }
                Pending::Put { ticket, tag, key, bytes } => {
                    let put = match host.store.get(&(tag, key)) {
                        None => {
                            host.store.insert((tag, key), Aligned::copy_from(&bytes));
                            Put::Written
                        }
                        Some(held) if held[..] == bytes[..] => Put::Same,
                        Some(_) => Put::Other,
                    };
                    selection.give(Input::Put { ticket, put: Ok(put) });
                }
            }
        }
        if !ended {
            return Err(Failure::Broke(format!("the request did not end in {STEPS} steps")));
        }
        match selection.outcome() {
            None => Err(Failure::Broke("the machine exited with no outcome".into())),
            Some(Err(why)) => Err(Failure::Broke(why.to_string())),
            Some(Ok(report)) => {
                if !host.store.contains_key(&(TestIndex::TAG.0, report.head.id())) {
                    return Err(Failure::Broke(format!("the new head {:?} is not filed", report.head)));
                }
                if report.checks.rechecks != 0 {
                    return Err(Failure::Broke(format!("{} rechecks", report.checks.rechecks)));
                }
                host.head = Some(report.head);
                compacted = report.cost.compacted;
                Ok(report.verdicts.to_vec())
            }
        }
    })?;
    if verdicts.len() != asked.len() {
        return Err(Failure::Broke(format!("{} verdicts for {} tests", verdicts.len(), asked.len())));
    }
    reached.compactions += u32::from(compacted);
    reached.raced += u32::from(raced);

    // The oracle: every asked test rerun from scratch, now.
    let mut out = Vec::with_capacity(asked.len());
    for (at, verdict) in verdicts.iter().enumerate() {
        let test = asked[at];
        let cached = matches!(verdict, Verdict::Cached { .. });
        match verdict {
            Verdict::Cached { by: By::Unchanged, .. } if since.edited => reached.unchanged_after_edit += 1,
            Verdict::Cached { by: By::Answers, .. } if since.undone => reached.answers_after_undo += 1,
            _ => {}
        }
        match &ran[at] {
            Some(done) => {
                reached.untraceable_runs += u32::from(!done.untraceable.is_empty());
                reached.failed_runs += u32::from(!done.passed);
                if cached && !done.passed {
                    return violation(Clause::FailureCached, format!("a failing result reported as a cached pass: test {test}"));
                }
                if cached {
                    return violation(Clause::RanAndCached, format!("test {test} both ran and was reported cached"));
                }
                if !done.untraceable.is_empty() && !matches!(verdict, Verdict::NotKept { .. }) {
                    return violation(Clause::UntraceableKept, format!("test {test} read what cannot be traced and was kept: {verdict:?}"));
                }
                if !done.passed && !matches!(verdict, Verdict::NotKept { passed: false, why: NotKept::Failed, .. }) {
                    return violation(Clause::FailureMisreported, format!("test {test} failed and was reported {verdict:?}"));
                }
            }
            None => {
                if !cached {
                    return violation(Clause::NeitherRanNorCached, format!("test {test} neither ran nor was cached: {verdict:?}"));
                }
            }
        }
        if run_all && cached {
            return violation(Clause::RunAllCached, format!("a run of every test reported test {test} cached"));
        }
        if !raced {
            let fresh = run(seed, world, test);
            let traceable = fresh.untraceable.is_empty();
            let kept_before = fresh.passed && traceable && host.filed[test].contains(&fresh.state);
            reached.cached_judged += u32::from(cached);
            reached.kept_judged += u32::from(!run_all && kept_before);
            if cached && !fresh.passed {
                return violation(
                    Clause::StaleCached,
                    format!("a stale cached result: test {test} is reported a cached pass and fails when rerun now"),
                );
            }
            if cached && !traceable {
                return violation(
                    Clause::CachedUntraceable,
                    format!("a cached result for test {test}, which reads, rerun now, what cannot be traced"),
                );
            }
            if !run_all && !cached && kept_before {
                return violation(Clause::Reran, format!("reran a test whose result was kept: test {test}"));
            }
        }
        if let (Verdict::Ran { .. }, Some(done)) = (verdict, &ran[at]) {
            host.filed[test].insert(done.state);
        }
        out.push((test, *verdict));
    }
    Ok(out)
}

fn op(tape: &Tape) -> Op {
    let at = tape.int_in(0..=7u8, "at") as usize;
    let value = tape.int_in(0..=3u8, "value");
    match tape.int_in(0..=11u8, "op") {
        0 | 1 => Op::Edit(at, value),
        2 => Op::Undo,
        3 => Op::File(at % FILES, (value > 0).then_some(value)),
        4 => Op::Env((value > 0).then_some(value)),
        5 => Op::Unlogged(value),
        6 => Op::Tick,
        7 => Op::Request { mask: tape.int_in(0..=31u8, "mask"), race: tape.int_in(0..=1u8, "race") == 1 },
        8 => Op::RunAll,
        _ => REQUEST,
    }
}

/// The invariant: a test is reported cached only if rerunning it now would give the same
/// result. For any edits, undone edits, outside file and variable changes, clock ticks, partial
/// requests, runs of every test and races, in any order of the host's answers: (a) no cached
/// result is stale (a fresh run passes); (b) a test is rerun only when no kept pass holds now
/// (a fresh run's state was never kept); (c) so an undone edit reruns nothing that was kept;
/// (d) a test that reads what cannot be traced is never cached; a failure is never cached; at
/// most `WORKERS` tests run at once; every request ends.
#[test]
fn a_cached_result_is_what_a_rerun_gives() {
    blaze_sim::check(Config::cases(300), |tape| -> Result<(), Broke> {
        let ops: Vec<Op> = tape.vec(0..=30, "ops", op);
        let seed = tape.next_u64("program seed");
        sim(&ops, seed, tape, Plant::None).map(drop).map_err(broke)
    });
}

/// Thirty ops, each drawn as `op` draws one.
fn random_ops(tape: &Tape) -> Vec<Op> {
    (0..30).map(|_| op(tape)).collect()
}

/// The same invariant over many cases of thirty ops, replayable by seed; and those cases reach
/// every situation a clause judges (a pass that holds no clause to a case proves nothing): each
/// of [`POINTS`] is a point some passing case has to reach.
#[test]
fn many_seeds_hold_the_invariant() {
    blaze_sim::check(Config::cases(200).must_reach(&POINTS), |tape| -> Result<(), Broke> {
        let ops = random_ops(tape);
        let seed = tape.next_u64("program seed");
        tape.note(format_args!("ops: {ops:?}"));
        let sim = sim(&ops, seed, tape, Plant::None).map_err(broke)?;
        for (count, point) in sim.reached.counts().into_iter().zip(POINTS) {
            if count > 0 {
                tape.reached(point);
            }
        }
        Ok(())
    });
}

/// Edits to every function, each then undone, with a request after each.
fn edits() -> Vec<Op> {
    let mut ops = vec![REQUEST];
    for at in 0..FNS {
        ops.extend([Op::Edit(at, 1), REQUEST, Op::Edit(at, 2), REQUEST, Op::Undo, Op::Undo, REQUEST]);
    }
    ops
}

/// Changes to every file and the variable, with a request after each.
fn file_changes() -> Vec<Op> {
    let mut ops = vec![REQUEST];
    for value in [Some(1), Some(2), None, Some(3)] {
        for at in 0..FILES {
            ops.extend([Op::File(at, value), REQUEST]);
        }
        ops.extend([Op::Env(value), REQUEST]);
    }
    ops
}

/// The invariant: an edit undone reruns nothing that was kept before it: every test kept by
/// the first request is cached by the last.
#[test]
fn an_undone_edit_reruns_nothing() {
    blaze_sim::check(Config::cases(64), |tape| -> Result<(), Broke> {
        let edited = tape.index(FNS, "edited function");
        let seed = tape.next_u64("program seed");
        let ops = [REQUEST, Op::Edit(edited, 1), Op::Edit(0, 3), REQUEST, Op::Undo, Op::Undo, REQUEST];
        let sim = sim(&ops, seed, tape, Plant::None).map_err(broke)?;
        let (first, last) = (&sim.requests[0], &sim.requests[2]);
        for ((test, before), (_, after)) in first.iter().zip(last) {
            let kept = matches!(before, Verdict::Ran { .. } | Verdict::Cached { .. });
            if kept && !matches!(after, Verdict::Cached { .. }) {
                return Err(Broke::Oracle(format!("test {test} was kept, the edit was undone, and it was {after:?}")));
            }
        }
        Ok(())
    });
}

/// The records and verdicts are the same whatever order the host answers in.
#[test]
fn the_records_do_not_depend_on_answer_order() {
    blaze_sim::check(Config::cases(32), |tape| -> Result<(), Broke> {
        let ops: Vec<Op> = random_ops(tape)
            .into_iter()
            .map(|op| match op {
                Op::Request { mask, .. } => Op::Request { mask, race: false },
                op => op,
            })
            .collect();
        let seed = tape.next_u64("program seed");
        let a = sim(&ops, seed, &Tape::seeded(1), Plant::None).map_err(broke)?;
        let b = sim(&ops, seed, &Tape::seeded(2), Plant::None).map_err(broke)?;
        assert_eq!(a.requests, b.requests, "the verdicts depend on the order of the answers");
        assert!(a.store == b.store, "the store depends on the order of the answers");
        Ok(())
    });
}

/// `ops` pass as the code is, and with `plant` some case breaks a clause; every case that
/// breaks one breaks one of `causes`, the clauses the plant can cause. Any other failure (another
/// clause, or the machine breaking) fails the control: a control caught by an unrelated clause
/// is not a control. A clause of `causes` is the oracle's finding; anything else is the harness's,
/// which ends the control at once.
fn caught(ops: &[Op], plant: Plant, causes: &[Clause]) {
    blaze_sim::control(Config::cases(64), Plant::None, plant, "", |tape, plant| {
        let seed = tape.next_u64("program seed");
        match sim(ops, seed, tape, plant) {
            Ok(_) => Ok(()),
            Err(Failure::Violation { clause, what }) if causes.contains(&clause) => {
                Err(Broke::Oracle(format!("{clause:?}: {what}")))
            }
            Err(Failure::Violation { clause, what }) => Err(Broke::Harness(format!(
                "{plant:?}: {clause:?} is not a clause it can cause ({causes:?}): {what}"
            ))),
            Err(Failure::Broke(what)) => Err(Broke::Harness(format!("{plant:?}: broke: {what}"))),
        }
    });
}

/// A function left out of a kept pass's asks: an edit to it is not seen, so the test is cached
/// while what it runs changed; rerun now, the changed code fails (stale) or reads the clock
/// (untraceable). Never a rerun of a kept state, never a failure kept.
#[test]
fn the_oracle_catches_an_executed_function_left_out_of_the_asks() {
    caught(&edits(), Plant::AskLeavesOutFunction, &[Clause::StaleCached, Clause::CachedUntraceable]);
}

/// A read left out of a kept pass's asks: a change to the file is not seen, so the test is
/// cached while what it reads changed; rerun now, it fails. The model's untraceable reads are
/// decided by code alone, so file changes cannot make it untraceable.
#[test]
fn the_oracle_catches_an_outside_read_left_out_of_the_asks() {
    caught(&file_changes(), Plant::AskLeavesOutRead, &[Clause::StaleCached]);
}

/// A failure reported as a cached pass: the run's own verdict is wrong.
#[test]
fn the_oracle_catches_a_failure_reported_as_a_cached_pass() {
    caught(&edits(), Plant::FailureReportedCached, &[Clause::FailureCached]);
}

/// The changed functions dropped: a row that asks an edited function is cached as unchanged;
/// rerun now, the edited code fails (stale) or reads the clock (untraceable).
#[test]
fn the_oracle_catches_the_changed_set_shortcut_skipping_a_changed_function() {
    caught(&edits(), Plant::ShortcutSkipsChanged, &[Clause::StaleCached, Clause::CachedUntraceable]);
}

/// A head and its postings decode to what was written.
#[test]
fn an_index_round_trips() {
    let mut reads = Vec::new();
    ask::write_shape(Basis::Executed, &[Ask::Read(ReadKind::File, b"a"), Ask::Read(ReadKind::Env, b"B")], &mut reads);
    let digests = [ContentId::of("model", b"1"), ABSENT];
    let mut bases = [ContentId::of("model", b"x"), ContentId::of("model", b"y")];
    bases.sort_unstable();
    let inline = [Ask::Function(function(0)).id(), Ask::Read(ReadKind::File, b"a").id()];
    let rows = [
        Row {
            base: bases[0],
            address: ContentId::of("model", b"address 0"),
            shape: ContentId::of("model", b"shape 0"),
            micros: 12,
            inline_at: 0,
            inline_len: 2,
            basis: Basis::Executed,
        },
        Row {
            base: bases[1],
            address: ContentId::of("model", b"address 1"),
            shape: ContentId::of("model", b"shape 1"),
            micros: 34,
            inline_at: POSTED,
            inline_len: 0,
            basis: Basis::Reached,
        },
    ];
    let ask = Ask::Function(function(1)).id();
    let mut postings = Vec::new();
    index::write_postings(&mut postings, &bases[1..], &[(ask, 0)]);
    let postings_key = ContentId::of("model", b"postings");
    let program = ContentId::of("model", b"program");
    let mut head = Vec::new();
    index::write_head(&mut head, &program, Some(&postings_key), &reads, &digests, &rows, &inline);

    let layout = HeadLayout::decode(&head).unwrap();
    let view = layout.view(&head);
    assert_eq!(view.program, program);
    assert_eq!(view.postings, Some(postings_key));
    assert_eq!(view.reads_count(), 2);
    assert_eq!(view.reads().collect::<Vec<_>>(), [Ask::Read(ReadKind::File, b"a"), Ask::Read(ReadKind::Env, b"B")]);
    assert_eq!((view.read_digest(0), view.read_digest(1)), (digests[0], digests[1]));
    assert_eq!(view.rows(), 2);
    assert_eq!((view.row(0), view.row(1)), (rows[0], rows[1]));
    assert_eq!(view.find(&bases[1]), Some(1));
    assert_eq!(view.find(&program), None);
    assert_eq!(view.inline(&rows[0]).collect::<Vec<_>>(), inline);

    let layout = PostingsLayout::decode(&postings).unwrap();
    let view = layout.view(&postings);
    assert_eq!(view.bases(), 1);
    let entry = view.find(&ask).unwrap();
    assert_eq!(view.ids(entry).map(|id| view.base(id)).collect::<Vec<_>>(), [bases[1]]);
    assert_eq!(view.find(&program), None);

    // A head with a posted row and no postings does not decode.
    let mut bad = Vec::new();
    index::write_head(&mut bad, &program, None, &reads, &digests, &rows, &inline);
    assert!(HeadLayout::decode(&bad).is_err());
}
