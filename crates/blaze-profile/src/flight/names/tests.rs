//! The names DST (docs/v2/plan/design/profile-names.md#proof): threads that each write their own
//! ring through their own [`Local`], fibers that carry names and move between threads, rings
//! drawn small enough to write over rows no reader read, a thread that stalls while others
//! burst through hundreds of generations, threads that end and pass their ring on, and a reader
//! that crashes (its [`Streams`] dropped) and starts again. The oracle: every row the reader
//! reads resolves each of its names to exactly the text recorded, or counts it as forgotten,
//! and never to another text; in a run where no row was written over unread, every name
//! resolves (bar one of a generation past [`RETAIN`], which the spec forgets on purpose).
//!
//! Its planted bugs: [`Plant::NoLink`], [`Plant::NoGenRow`], [`Plant::NoWindow`], and
//! [`Bug::KeepLocalAcrossPark`], a fiber that keeps the table of the thread it parked on and
//! writes with it on the thread it resumes on (`no-wait-across-thread-locals`).

use std::fmt;

use blaze_core::sim::Rng;
use hegel::{TestCase, generators as gs};

use super::*;

/// Seeds a control searches for a plan its plant breaks.
const CONTROL_SEEDS: u64 = 256;

/// The seed [`replay_one_seed`] runs, and its bug: set them to what a failure names.
const REPLAY: (u64, Bug) = (0, Bug::None);

/// Most acts a plan draws.
const MOST_ACTS: u64 = 60;

/// Names a fiber carries at most: the newest.
const CARRIED: usize = 16;

/// The kept name every fiber starts with.
const KEPT: Token = KEPT_TOKEN | 5;

/// The texts names are made of: short ones, an empty one, one of three rows.
fn texts() -> Vec<String> {
    let mut texts: Vec<String> = // alloc: a test's texts
        ["serde", "tokio", "app::f", "a", "", "blaze::unit", "x86_64", "worker 1", "μs"]
            .iter()
            .map(|it| (*it).to_owned())
            .collect();
    texts.push(format!("{}::long", "a_path_segment_".repeat(9)));
    texts
}

/// A bug the oracle must catch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Bug {
    None,
    Core(Plant),
    /// A fiber writes with the table of the thread it first recorded on, wherever it resumes.
    KeepLocalAcrossPark,
}

#[derive(Clone, Debug)]
enum Act {
    /// Fiber `fiber` names `text` on its thread.
    Name { fiber: usize, text: usize },
    /// Fiber `fiber` writes a row naming those of its names `picks` points at.
    Record { fiber: usize, picks: Vec<usize> },
    /// Fiber `fiber` parks and resumes on thread `thread`.
    Move { fiber: usize, thread: usize },
    /// Fiber `fiber` names and records `n` times in a row, its thread's generations churning,
    /// while every other thread stalls.
    Burst { fiber: usize, n: u64, salt: u64 },
    /// Thread `thread` ends; a new thread takes its ring.
    Respawn { thread: usize },
    /// The reader reads every ring.
    Drain,
    /// The reader crashes and starts again with nothing held.
    Crash,
}

#[derive(Clone, Debug)]
struct Plan {
    threads: usize,
    fibers: usize,
    /// Rows a ring holds.
    cap: u64,
    gen_rows: u64,
    index_max: u16,
    acts: Vec<Act>,
}

trait Draw {
    fn int(&mut self, lo: u64, hi: u64) -> u64;

    fn below(&mut self, n: usize) -> usize {
        self.int(0, n as u64 - 1) as usize
    }
}

struct Drawn<'a>(&'a TestCase);

impl Draw for Drawn<'_> {
    fn int(&mut self, lo: u64, hi: u64) -> u64 {
        self.0.draw(gs::integers::<u64>().min_value(lo).max_value(hi))
    }
}

impl Draw for Rng {
    fn int(&mut self, lo: u64, hi: u64) -> u64 {
        match (hi - lo).checked_add(1) {
            Some(n) => lo + Rng::below(self, n),
            None => self.next_u64(),
        }
    }
}

fn plan(d: &mut impl Draw) -> Plan {
    let threads = d.int(1, 4) as usize;
    let fibers = d.int(1, 4) as usize;
    let cap = [8, 16, 64, 1 << 20][d.below(4)];
    let gen_rows = [4, 8, 32, GEN_ROWS][d.below(4)];
    let index_max = [3, 4, 8, INDEX_MAX][d.below(4)];
    let n = d.int(1, MOST_ACTS);
    let texts = texts().len();
    // alloc: a plan, once a case
    let acts = (0..n)
        .map(|_| {
            let fiber = d.below(fibers);
            match d.int(0, 99) {
                0..30 => Act::Name { fiber, text: d.below(texts) },
                30..60 => {
                    let len = d.int(1, 3) as usize;
                    // alloc: a plan's act
                    Act::Record { fiber, picks: (0..len).map(|_| d.below(CARRIED)).collect() }
                }
                60..72 => Act::Move { fiber, thread: d.below(threads) },
                72..77 => Act::Burst { fiber, n: d.int(1, 1500), salt: d.int(1, 97) },
                77..81 => Act::Respawn { thread: d.below(threads) },
                81..96 => Act::Drain,
                _ => Act::Crash,
            }
        })
        .collect();
    Plan { threads, fibers, cap, gen_rows, index_max, acts }
}

fn plan_of(seed: u64) -> Plan {
    plan(&mut Rng::new(seed))
}

// --- The model --------------------------------------------------------------------------

/// A thread's ring: the newest `cap` rows, and the generation it keeps for its next thread.
struct ModelRing {
    rows: Vec<[u64; WORDS]>,
    cap: u64,
    head: u64,
    generation: u32,
    lost: u64,
}

impl ModelRing {
    fn row(&self, i: u64) -> Option<&[u64; WORDS]> {
        (i < self.head && i + self.cap >= self.head).then(|| &self.rows[(i % self.cap) as usize])
    }

    fn oldest(&self) -> u64 {
        self.head.saturating_sub(self.cap)
    }
}

struct Writer<'a>(&'a mut ModelRing);

impl Out for Writer<'_> {
    fn head(&self) -> u64 {
        self.0.head
    }

    fn put(&mut self, row: [u64; WORDS]) -> u64 {
        let ring = &mut *self.0;
        let i = ring.head;
        if (ring.rows.len() as u64) < ring.cap {
            ring.rows.push(row);
        } else {
            ring.rows[(i % ring.cap) as usize] = row;
        }
        ring.head += 1;
        i
    }

    fn began(&mut self, generation: u32) {
        self.0.generation = generation;
    }

    fn counted(&mut self, count: Count) {
        if count == Count::Lost {
            self.0.lost += 1;
        }
    }
}

/// The reader's view of the rings, numbered from 1.
struct Rings<'a>(&'a [ModelRing]);

impl Source for Rings<'_> {
    fn names_rows(
        &mut self,
        ring: u16,
        from: u64,
        into: &mut Vec<(u64, [u64; WORDS])>,
    ) -> (u64, u64) {
        let Some(it) = usize::from(ring).checked_sub(1).and_then(|at| self.0.get(at)) else {
            return (from, from);
        };
        let first = from.max(it.oldest());
        for i in first..it.head {
            let row = it.row(i).expect("a row between the oldest and the head");
            if is_names_row(row) {
                into.push((i, *row));
            }
        }
        (first, it.head)
    }
}

/// A row recorded: its names' texts as the fiber held them, and where they were given.
struct Record {
    tokens: Vec<Token>,
    texts: Vec<String>,
    ring: u16,
    /// The generation of the writer's table as it wrote the row.
    generation: u32,
    /// The row's index in its ring.
    at: u64,
    read: bool,
}

struct Fiber {
    thread: usize,
    /// The thread whose table it kept across a park ([`Bug::KeepLocalAcrossPark`] only).
    kept: Option<usize>,
    names: Vec<(Token, String)>,
}

/// What a run saw, for the controls' messages and a run's sanity.
#[derive(Debug, Default)]
struct Seen {
    records: u64,
    resolved: u64,
    forgotten: u64,
    skipped: u64,
    lossy: bool,
    /// The rows each drain found written over before it read them: ring, from, to.
    missed: Vec<(u16, u64, u64)>,
}

struct Run<'p> {
    plan: &'p Plan,
    bug: Bug,
    texts: Vec<String>,
    rings: Vec<ModelRing>,
    locals: Vec<Local>,
    fibers: Vec<Fiber>,
    records: Vec<Record>,
    reader: Streams,
    /// Per ring, the next row the reader's drain reads.
    cursor: Vec<u64>,
    /// Per ring, the next row the reader's names read: 0 again after a crash.
    names_to: Vec<u64>,
    seen: Seen,
}

impl<'p> Run<'p> {
    fn new(plan: &'p Plan, bug: Bug) -> Run<'p> {
        let mut run = Run {
            plan,
            bug,
            texts: texts(),
            // alloc: a run's model
            rings: (0..plan.threads)
                .map(|_| ModelRing {
                    rows: Vec::new(), // alloc: a model ring
                    cap: plan.cap,
                    head: 0,
                    generation: 0,
                    lost: 0,
                })
                .collect(),
            locals: Vec::new(), // alloc: a run's model
            fibers: (0..plan.fibers)
                .map(|at| Fiber {
                    thread: at % plan.threads,
                    kept: None,
                    names: vec![(KEPT, "kept".to_owned())], // alloc: a fiber's names
                })
                .collect(),
            records: Vec::new(), // alloc: a run's model
            reader: Streams::default(),
            cursor: vec![0; plan.threads], // alloc: a run's model
            names_to: vec![0; plan.threads], // alloc: a run's model
            seen: Seen::default(),
        };
        for thread in 0..plan.threads {
            let local = run.local_of(thread);
            run.locals.push(local);
        }
        run.reader.plant(run.plant());
        run
    }

    fn plant(&self) -> Plant {
        match self.bug {
            Bug::Core(plant) => plant,
            _ => Plant::None,
        }
    }

    /// A new thread's table for thread `thread`'s ring.
    fn local_of(&self, thread: usize) -> Local {
        let ring = &self.rings[thread];
        let mut local = Local::with_limits(
            thread as u16 + 1,
            ring.generation,
            self.plan.gen_rows,
            self.plan.index_max,
        );
        local.plant(self.plant());
        local
    }

    /// The table fiber `fiber` writes with, and the ring it writes into: its thread's, looked up
    /// fresh, unless the bug keeps the first it used.
    fn writing(&mut self, fiber: usize) -> (usize, usize) {
        let it = &mut self.fibers[fiber];
        let table = match self.bug {
            Bug::KeepLocalAcrossPark => *it.kept.get_or_insert(it.thread),
            _ => it.thread,
        };
        (table, it.thread)
    }

    fn name(&mut self, fiber: usize, text: usize) {
        let (table, ring) = self.writing(fiber);
        // clone: a test's text
        let text = self.texts[text].clone();
        let token = self.locals[table].intern(&text, &mut Writer(&mut self.rings[ring]));
        let names = &mut self.fibers[fiber].names;
        names.push((token, text));
        if names.len() > CARRIED {
            names.remove(0);
        }
    }

    fn record(&mut self, fiber: usize, picks: &[usize]) {
        let names = &self.fibers[fiber].names;
        // alloc: a row's names, in a test
        let picked: Vec<(Token, String)> =
            picks.iter().map(|it| names[it % names.len()].clone()).collect();
        let (table, ring) = self.writing(fiber);
        let mut words = [0u64; 3];
        for (word, (token, _)) in words.iter_mut().zip(&picked) {
            *word = *token;
        }
        let words = &mut words[..picked.len()];
        let local = &mut self.locals[table];
        local.row_syms(words, &mut Writer(&mut self.rings[ring]));
        let rid = self.records.len() as u64;
        let at = self.rings[ring].head;
        let mut row = [0; WORDS];
        row[..words.len()].copy_from_slice(words);
        row[3] = rid;
        row[5] = words.len() as u64;
        Writer(&mut self.rings[ring]).put(row);
        self.records.push(Record {
            tokens: picked.iter().map(|it| it.0).collect(),
            texts: picked.into_iter().map(|it| it.1).collect(),
            ring: ring as u16 + 1,
            generation: local.generation(),
            at,
            read: false,
        });
        self.seen.records += 1;
    }

    fn act(&mut self, act: &Act) -> Result<(), String> {
        match act {
            Act::Name { fiber, text } => self.name(*fiber, *text),
            Act::Record { fiber, picks } => self.record(*fiber, picks),
            Act::Move { fiber, thread } => self.fibers[*fiber].thread = *thread,
            Act::Burst { fiber, n, salt } => {
                for k in 0..*n {
                    let text = ((k * salt + k / 3) % self.texts.len() as u64) as usize;
                    self.name(*fiber, text);
                    self.record(*fiber, &[CARRIED - 1, CARRIED - 4]);
                }
            }
            Act::Respawn { thread } => self.locals[*thread] = self.local_of(*thread),
            Act::Drain => self.drain()?,
            Act::Crash => {
                // Restarted with nothing held: it reads every ring's names again from the
                // oldest row, and those written over first are gone ([`Run::drain`]).
                self.names_to.iter_mut().for_each(|it| *it = 0);
                self.reader = Streams::default();
                self.reader.plant(self.plant());
            }
        }
        Ok(())
    }

    /// Reads every ring's rows since the last drain and checks each name they hold.
    fn drain(&mut self) -> Result<(), String> {
        // Rows written over before the reader read them, rows or definitions: from here on a
        // name may read as forgotten.
        for (at, ring) in self.rings.iter().enumerate() {
            if ring.oldest() > self.cursor[at].min(self.names_to[at]) {
                self.seen.lossy = true;
            }
        }
        for at in 0..self.rings.len() {
            self.reader.catch_up(at as u16 + 1, &mut Rings(&self.rings));
            self.names_to[at] = self.rings[at].head;
        }
        for at in 0..self.rings.len() {
            let (oldest, head) = (self.rings[at].oldest(), self.rings[at].head);
            let first = self.cursor[at].max(oldest);
            self.seen.skipped += first - self.cursor[at];
            if first > self.cursor[at] {
                self.seen.missed.push((at as u16 + 1, self.cursor[at], first));
            }
            for i in first..head {
                let row = *self.rings[at].row(i).expect("a row the ring holds");
                if is_names_row(&row) {
                    continue;
                }
                self.check(at as u16 + 1, i, &row)?;
            }
            self.cursor[at] = head;
        }
        Ok(())
    }

    fn check(&mut self, ring: u16, i: u64, row: &[u64; WORDS]) -> Result<(), String> {
        let rid = row[3] as usize;
        let lossy = self.seen.lossy;
        let current = |rings: &[ModelRing], ring: u16| rings[usize::from(ring) - 1].generation;
        let record = &self.records[rid];
        if record.read {
            return Err(format!("record {rid} read twice"));
        }
        for k in 0..row[5] as usize {
            let (sym, want, token) = (row[k] as u32, &record.texts[k], record.tokens[k]);
            if sym == 0 {
                return Err(format!("record {rid}'s name {k} ({want:?}) was lost writing it"));
            }
            if token & KEPT_TOKEN != 0 {
                if sym != KEPT_SYM | (token & 0xffff) as u32 {
                    return Err(format!("record {rid}'s kept name {k} is {sym:#x}"));
                }
                continue;
            }
            match self.reader.resolve(ring, i, sym, &mut Rings(&self.rings)) {
                Some(got) if *got == **want => self.seen.resolved += 1,
                Some(got) => {
                    return Err(format!(
                        "record {rid} (ring {ring}, row {i}) name {k} reads {got:?}, recorded {want:?}"
                    ));
                }
                None => {
                    self.seen.forgotten += 1;
                    let (of, generation, _) = parts(token).expect("a ring's name");
                    // Of a generation the reader let go: RETAIN or more before its ring's now.
                    let old = |ring, generation| {
                        current(&self.rings, ring)
                            .checked_sub(generation)
                            .is_some_and(|back| back + 1 >= RETAIN as u32)
                    };
                    let let_go = old(record.ring, record.generation) || old(of, generation);
                    if !lossy && !let_go {
                        return Err(format!(
                            "record {rid} (ring {ring}, row {i}) name {k} ({want:?}, token \
                             {token:#x}) is forgotten in a run that wrote nothing over unread"
                        ));
                    }
                }
            }
        }
        self.records[rid].read = true;
        Ok(())
    }
}

fn run(plan: &Plan, bug: Bug) -> Result<Seen, String> {
    let mut run = Run::new(plan, bug);
    for (step, act) in plan.acts.iter().enumerate() {
        run.act(act).map_err(|why| format!("after act {step} ({act:?}): {why}"))?;
    }
    run.drain().map_err(|why| format!("at the last drain: {why}"))?;
    let lost: u64 = run.rings.iter().map(|it| it.lost).sum();
    if lost != 0 {
        return Err(format!("{lost} names lost: a row's links did not fit its generation"));
    }
    // Every record not read was in rows a drain found written over, and counted there.
    let missed = |it: &Record| {
        run.seen.missed.iter().any(|&(ring, from, to)| ring == it.ring && (from..to).contains(&it.at))
    };
    if let Some(rid) = run.records.iter().position(|it| !it.read && !missed(it)) {
        return Err(format!("record {rid} was never read, and no drain counted it skipped"));
    }
    Ok(run.seen)
}

impl fmt::Display for Seen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} records, {} names resolved, {} forgotten, {} rows skipped, lossy {}",
            self.records, self.resolved, self.forgotten, self.skipped, self.lossy
        )
    }
}

// --- The tests ------------------------------------------------------------------------------

/// The invariant: every name a row holds reads back as the text recorded or is counted as
/// forgotten, never as another text; and nothing is forgotten in a run that lost no row.
#[hegel::test(test_cases = 200)]
fn every_name_reads_back_as_recorded_or_is_counted_forgotten(tc: TestCase) {
    let plan = plan(&mut Drawn(&tc));
    if let Err(why) = run(&plan, Bug::None) {
        panic!("{why}\nplan {plan:?}");
    }
}

/// A bug the oracle must catch at one of [`CONTROL_SEEDS`] seeded plans (test-the-cause on the
/// test itself).
fn control(bug: Bug) {
    for seed in 0..CONTROL_SEEDS {
        let plan = plan_of(seed);
        if let Err(why) = run(&plan, bug) {
            eprintln!("{bug:?} caught at seed {seed} (REPLAY = ({seed}, Bug::{bug:?})): {why}");
            return;
        }
    }
    panic!(
        "the oracle caught {bug:?} at none of seeds 0..{CONTROL_SEEDS}: it, or the draws, are too weak"
    );
}

#[test]
fn the_oracle_catches_a_foreign_name_written_with_no_link() {
    control(Bug::Core(Plant::NoLink));
}

#[test]
fn the_oracle_catches_a_generation_begun_with_no_gen_row() {
    control(Bug::Core(Plant::NoGenRow));
}

#[test]
fn the_oracle_catches_a_reader_that_ignores_the_window() {
    control(Bug::Core(Plant::NoWindow));
}

#[test]
fn the_oracle_catches_a_table_kept_across_a_park() {
    control(Bug::KeepLocalAcrossPark);
}

/// Seeded plans with no bug: the seeds the controls search pass clean.
#[test]
fn the_seeds_the_controls_search_pass_with_no_bug() {
    for seed in 0..CONTROL_SEEDS {
        let plan = plan_of(seed);
        if let Err(why) = run(&plan, Bug::None) {
            panic!("seed {seed}: {why}\nplan {plan:?}");
        }
    }
}

/// One seeded plan, [`REPLAY`]'s: set it to a seed a failure names to run that case alone.
#[test]
fn replay_one_seed() {
    let (seed, bug) = REPLAY;
    let plan = plan_of(seed);
    match run(&plan, bug) {
        Ok(seen) => eprintln!("seed {seed}, {bug:?}: {seen}"),
        Err(why) => panic!("seed {seed}, {bug:?}: {why}\nplan {plan:?}"),
    }
}

/// A name a long span took as it opened, written as a link when it closes on another thread
/// some hundreds of generations of its ring later, reads back while the reader was told the
/// span held it ([`Streams::hold`], once a read), and is forgotten when it was not: the cause
/// removed, the effect gone (the memattrib lane's empty `function` of 25 to 70 s tasks,
/// 2026-11-05).
#[test]
fn a_name_a_long_span_holds_reads_back_past_retain_generations() {
    for told in [true, false] {
        let ring = |cap| ModelRing {
            rows: Vec::new(), // alloc: a test's ring
            cap,
            head: 0,
            generation: 0,
            lost: 0,
        };
        let mut rings = [ring(1 << 16), ring(1 << 16)];
        let (mut a, mut b) = (Local::with_limits(1, 0, 4, 3), Local::with_limits(2, 0, 4, 3));
        let mut reader = Streams::default();
        let function = a.intern("app::long_task", &mut Writer(&mut rings[0]));
        for k in 0..800u64 {
            let text = format!("unit {k}");
            let token = a.intern(&text, &mut Writer(&mut rings[0]));
            let mut words = [token];
            a.row_syms(&mut words, &mut Writer(&mut rings[0]));
            if k % 16 == 0 {
                for ring in 1..=2 {
                    reader.catch_up(ring, &mut Rings(&rings));
                }
                let held: &[Token] = if told { &[function] } else { &[] };
                reader.hold(held, &mut Rings(&rings));
            }
        }
        assert!(a.generation() > 3 * RETAIN as u32, "ring 1 went round its readers' generations");
        // The span closes on thread b: its function is a link in b's ring.
        let mut words = [function];
        b.row_syms(&mut words, &mut Writer(&mut rings[1]));
        let at = rings[1].head;
        Writer(&mut rings[1]).put([words[0], 0, 0, 0, 0, 0, 0, 0]);
        for ring in 1..=2 {
            reader.catch_up(ring, &mut Rings(&rings));
        }
        reader.hold(&[], &mut Rings(&rings));
        let got = reader.resolve(2, at, words[0] as u32, &mut Rings(&rings));
        match told {
            true => assert_eq!(got.as_deref(), Some("app::long_task")),
            false => assert_eq!(got, None, "with no hold the name is past RETAIN: forgotten"),
        }
    }
}
