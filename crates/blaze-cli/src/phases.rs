//! The command's own time (rule 17): every phase of one command in order, in nanoseconds on the
//! clock `main` is handed (rule 75), and inside the request each kind of effect summed. Written
//! as two lines of `.blaze/v2/profile.txt`, so why a command took what it took is read, not
//! guessed. What happens after `main` returns (the runtime's teardown, the process exit) is
//! outside any clock this binary holds: the wall time less `in main` is start and exit.

use core::fmt;

use bstd::clock::{Clock, Instant};

pub(crate) mod byte_rows;

/// The most phases one command marks: tempo's edit marked 16 when the open checkout was one
/// phase (2026-11-20, score-c1725wave42), and its parts are four more (engine_check.rs `edge`).
const MOST: usize = 24;

pub struct Phases {
    clock: Clock,
    began: Instant,
    last: Instant,
    /// From the process's start (its exec) to the driver's: the loader, the runtime's
    /// initialisers, Rust std's start, bstd's reading of the start; `None` where the system
    /// would not say when the process began.
    before_runtime: Option<u64>,
    /// From the driver's start to `main`'s first line.
    before_main: u64,
    /// What the process used from its start to `main`'s first line (`proc_pid_rusage` and
    /// `getrusage` of the whole process, read at [`Phases::new`]): the CPU, instructions and
    /// page faults of the kernel's exec, dyld, Rust std's start and bstd's runtime start, all
    /// threads. It says which of `before_runtime` and `before_main` was work (user or system
    /// time), page-ins (major faults: read from disk; minor: mapped from the page cache, each
    /// also validated against the code signature on its first map), or a wait (wall less CPU).
    before_main_used: blaze_meter::System,
    rows: [(&'static str, u64); MOST],
    len: usize,
    /// Inside the request: the machine's steps and each kind of effect performed, summed.
    pub request: Effects,
    /// The memory layer's system calls since the process began, when the frame's first chunk
    /// was taken ([`Phases::first_chunk`]).
    first_chunk: Option<Syscalls>,
    /// The same, when `main` began.
    at_main: Syscalls,
    /// The most each frame held over the request, once it answered.
    pub frames: Option<crate::work::HighWater>,
    /// How many names worlds the request made (names.rs `world`: 0 or 1): an edit's check reads
    /// the crate list in place and makes none (D114 rule 1, graph_payload.rs).
    pub worlds: u32,
    /// The records the request made, by kind, and the `CrateScope` records' sections: where
    /// the bytes written are (work.rs `answer_batch`).
    pub records: byte_rows::Records,
    /// `blaze run`'s closing count of code records (run_parent.rs `Tally`), on every run that
    /// started its program: the proof a repeat made nothing (its error-stream line is quiet then).
    pub(crate) code_records: Option<crate::run_parent::RunTally>,
    /// `blaze run`'s parent's reads for its names records (run_names.rs `NamesRead`): the crates'
    /// records read in place, the bytes copied (0), the trees opened at their first read.
    pub(crate) run_names: Option<crate::run_names::NamesRead>,
    /// The cleanup line's part known before the commit after the answer (store/roots.rs
    /// `CleanupLine::before_commit`), for a profile written before that commit (main.rs).
    pub(crate) cleanup: Option<crate::store::roots::BeforeCommit>,
    /// The published set named by `--origin`, taken before the first phase (store/take.rs).
    pub(crate) published: Option<crate::store::take::Taken>,
    /// The command's own frame, by what took its bytes ([`Phases::mark_in`], [`Phases::took`]).
    held: Held,
    /// What each phase used on this thread (D169), beside `rows`: the meter's difference of
    /// the snapshots at its two marks, or why the two are not a pair (the marks were on two
    /// threads, or the tests' meter switch was thrown between them: blaze-meter `Unpaired`).
    used: [Result<blaze_meter::Used, blaze_meter::Unpaired>; MOST],
    /// The meter at the last mark (at `Phases::new` before the first).
    meter: blaze_meter::Meter,
    /// The command's own worker cell's waits at the last [`Phases::mark_cx`] (`ZERO` before
    /// the first: the cell is the command's, so the first phase marked this way takes every
    /// wait since the command began), and at the start of the last row once it began.
    waits_at: blaze_meter::Waited,
    last_row_waits_from: blaze_meter::Waited,
    /// Stretches on other threads, by what ran them ([`Phases::worked_used`]): the tasks'
    /// spread, counted beside the phases in the reconciliation.
    beside: [(&'static str, blaze_meter::Spread); MOST_BESIDE],
    beside_len: usize,
}

/// The most kinds of other threads' stretches [`Phases::worked_used`] names; past it a kind
/// is added to the last row.
const MOST_BESIDE: usize = 8;

/// A share under this of the process's total is printed `UNACCOUNTED` (D169).
const ACCOUNTED_PERCENT: u64 = 95;

/// The most kinds [`Held`] names; past it a kind is added to the last row.
const MOST_KINDS: usize = 40;

/// The command frame's bytes by lifetime's evidence (rule 17): how many each phase took, and
/// inside the request how many each kind of effect and each kind of task took, with the times
/// it came and the main thread's nanoseconds in it. A frame only grows, so each number is the
/// frame's `used` at one boundary less its `used` at the one before: every byte is in one row.
#[derive(Clone, Copy)]
struct Held {
    /// The frame's `used` at the last phase marked, and at the last kind charged.
    phase_at: usize,
    kind_at: usize,
    /// When the last kind was charged.
    kind_since: Option<Instant>,
    phases: [u64; MOST],
    /// A kind's name, the bytes it took, the times it came, its nanoseconds.
    kinds: [(&'static str, u64, u64, u64); MOST_KINDS],
    len: usize,
    /// The frame's `used` and reserve when last seen.
    seen: (usize, usize),
}

type Syscalls = [(blaze_mem::syscalls::Kind, blaze_mem::syscalls::Calls); blaze_mem::syscalls::KINDS.len()];

/// One `memory` line: each kind of call made so far, as calls and nanoseconds.
fn memory_line(f: &mut fmt::Formatter<'_>, label: &str, calls: &Syscalls) -> fmt::Result {
    write!(f, "memory       {label}:")?;
    for (kind, c) in calls.iter().filter(|(_, c)| c.calls > 0) {
        write!(f, " {} {}x {} ns (most {})", kind.name(), c.calls, c.ns, c.max_ns)?;
    }
    writeln!(f)
}

/// Nanoseconds spent performing each kind of effect, and the machine's own steps.
#[derive(Clone, Copy, Default)]
pub struct Effects {
    pub steps: u64,
    pub gets: u64,
    pub puts: u64,
    pub runs: u64,
    pub publish: u64,
    pub prints: u64,
    /// `prints` by part (main.rs `draw`): what the drawing does with each finding is named,
    /// not guessed (rule 17).
    pub print_parts: PrintParts,
    pub write_last: u64,
}

/// The parts of the drawing, in the order it does them (ns).
#[derive(Clone, Copy, Default)]
pub struct PrintParts {
    /// Each finding's words and file said, then each placed in its file with its id
    /// (print.rs `list`, fix.rs `lengths`).
    pub listing: u64,
    /// The findings sorted by cause: the waited grouped by root, the counted kept.
    pub grouping: u64,
    /// The note lines: failures, causes, items and tasks not checked.
    pub notes: u64,
    /// The code's findings drawn, each file read again.
    pub findings: u64,
    /// The failures drawn last, with the count under each.
    pub roots: u64,
}

impl Phases {
    pub fn new(clock: Clock) -> Phases {
        let now = clock.now();
        let since_process = clock.since_process_start(now).ok();
        Phases {
            began: now,
            last: now,
            before_runtime: since_process.map(|it| nanos(it.saturating_sub(now.since_start()))),
            before_main: nanos(now.since_start()),
            before_main_used: blaze_meter::Process::now().system,
            clock,
            rows: [("", 0); MOST],
            len: 0,
            request: Effects::default(),
            first_chunk: None,
            at_main: blaze_mem::syscalls::snapshot(),
            frames: None,
            worlds: 0,
            records: byte_rows::Records::new(),
            code_records: None,
            run_names: None,
            cleanup: None,
            published: None,
            held: Held { phase_at: 0, kind_at: 0, kind_since: None, phases: [0; MOST], kinds: [("", 0, 0, 0); MOST_KINDS], len: 0, seen: (0, 0) },
            used: [Ok(blaze_meter::Used::ZERO); MOST],
            meter: blaze_meter::Meter::now(),
            waits_at: blaze_meter::Waited::ZERO,
            last_row_waits_from: blaze_meter::Waited::ZERO,
            beside: [("", blaze_meter::Spread::NONE); MOST_BESIDE],
            beside_len: 0,
        }
    }

    /// Resets the phase ledger for the next request in a resident daemon without cloning `Clock`.
    pub fn reset(&mut self) {
        let now = self.clock.now();
        self.began = now;
        self.last = now;
        self.before_runtime = None;
        self.before_main = 0;
        self.before_main_used = blaze_meter::Process::now().system;
        self.len = 0;
        self.request = Effects::default();
        self.first_chunk = None;
        self.at_main = blaze_mem::syscalls::snapshot();
        self.frames = None;
        self.worlds = 0;
        self.records = byte_rows::Records::new();
        self.code_records = None;
        self.run_names = None;
        self.cleanup = None;
        self.published = None;
        self.held = Held { phase_at: 0, kind_at: 0, kind_since: None, phases: [0; MOST], kinds: [("", 0, 0, 0); MOST_KINDS], len: 0, seen: (0, 0) };
        self.meter = blaze_meter::Meter::now();
        self.waits_at = blaze_meter::Waited::ZERO;
        self.last_row_waits_from = blaze_meter::Waited::ZERO;
        self.beside = [("", blaze_meter::Spread::NONE); MOST_BESIDE];
        self.beside_len = 0;
    }

    /// Stretches of `kind` that ran on threads other than this one (a pool's tasks on its
    /// workers), their spread: counted beside the phases when the profile reconciles against
    /// the process. A stretch on this thread is inside a phase already and is never given here.
    pub fn worked_used(&mut self, kind: &'static str, spread: &blaze_meter::Spread) {
        let at = match self.beside[..self.beside_len].iter().position(|row| row.0 == kind) {
            Some(at) => at,
            None if self.beside_len < MOST_BESIDE => {
                if let Some(slot) = self.beside.get_mut(self.beside_len) {
                    *slot = (kind, blaze_meter::Spread::NONE);
                }
                self.beside_len += 1;
                self.beside_len - 1
            }
            None => MOST_BESIDE - 1,
        };
        self.beside[at].1.join(spread);
    }

    /// [`Phases::mark`], and the bytes of the command's `frame` taken since the last phase
    /// marked so are the phase's.
    pub fn mark_in(&mut self, name: &'static str, frame: &bstd::mem::Frame) {
        let row = self.len.min(MOST - 1);
        self.mark(name);
        let used = frame.used();
        self.held.phases[row] += used.saturating_sub(self.held.phase_at) as u64;
        self.held.phase_at = used;
        self.held.seen = (used, frame.reserved());
    }

    /// [`Phases::mark_in`], with the waits the command's own worker cell was charged since the
    /// last such mark put in the phase's use. [`Phases::mark`] reads the meter of the thread
    /// alone (`Meter::now`), whose `waited` is zero by construction (blaze-meter `Meter::read`),
    /// and its "N waits" are the thread's lock waits and parks: a bstd wait (`Cx::wait`,
    /// charged to the worker's tally on its way out, D171, bstd workers.rs `charge_wait_at`)
    /// is in neither, so a 177 ms journal replay printed "0 waits" (tempo, score-c0230wave85).
    /// Here the cell's waits are read through `cx`, the command's root, on the thread that
    /// marks.
    pub fn mark_cx(&mut self, cx: &bstd::Cx, name: &'static str, frame: &bstd::mem::Frame) {
        let row = self.len.min(MOST - 1);
        let merged = self.len >= MOST;
        if self.len == MOST - 1 {
            self.last_row_waits_from = self.waits_at;
        }
        self.mark_in(name, frame);
        let now = cx.on_worker(|w| *w.meter().waits());
        // Past MOST the last row is a sum ([`Phases::mark`]): its waits are those since it began.
        let from = match merged {
            true => self.last_row_waits_from,
            false => self.waits_at,
        };
        self.waits_at = now;
        if let Ok(used) = &mut self.used[row] {
            used.waited = now.since(&from);
        }
    }

    /// The bytes of the command's `frame` taken since the last call, and the main thread's time
    /// since it, are `kind`'s: an effect performed, a task run here, the machine's step. Called
    /// at every boundary inside the request, so the kinds' rows sum to the request's.
    pub fn took(&mut self, kind: &'static str, frame: &bstd::mem::Frame) {
        let now = self.clock.now();
        let ns = self.held.kind_since.and_then(|since| now.since(since)).map_or(0, nanos);
        self.held.kind_since = Some(now);
        let used = frame.used();
        let bytes = used.saturating_sub(self.held.kind_at) as u64;
        self.held.kind_at = used;
        self.held.seen = (used, frame.reserved());
        self.charge(kind, bytes, ns);
    }

    /// A pooled task of `kind` took `ns` on its worker: a row beside the main thread's, with no
    /// bytes of the command's frame (a pooled task works in its own).
    pub fn worked(&mut self, kind: &'static str, ns: u64) {
        self.charge(kind, 0, ns);
    }

    /// The times `kind` was charged ([`Phases::took`], [`Phases::worked`]): 0 for one never seen.
    pub fn times(&self, kind: &'static str) -> u64 {
        self.held.kinds[..self.held.len].iter().find(|row| row.0 == kind).map_or(0, |row| row.2)
    }

    fn charge(&mut self, kind: &'static str, bytes: u64, ns: u64) {
        let at = match self.held.kinds[..self.held.len].iter().position(|row| row.0 == kind) {
            Some(at) => at,
            None if self.held.len < MOST_KINDS => {
                self.held.kinds[self.held.len].0 = kind;
                self.held.len += 1;
                self.held.len - 1
            }
            None => MOST_KINDS - 1,
        };
        let row = &mut self.held.kinds[at];
        row.1 += bytes;
        row.2 += 1;
        row.3 += ns;
    }

    /// The frame's first chunk is taken: marks it as a phase and keeps the memory layer's calls
    /// so far.
    pub fn first_chunk(&mut self) {
        self.mark("frame's first chunk");
        self.first_chunk = Some(blaze_mem::syscalls::snapshot());
    }

    pub fn now(&self) -> Instant {
        self.clock.now()
    }

    /// The clock `main` was handed: the sweep reads its wall time from it.
    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    /// Nanoseconds since `from`.
    pub fn since(&self, from: Instant) -> u64 {
        self.clock.now().since(from).map_or(0, nanos)
    }

    /// The phase `name` ended now: it took the time since the last mark. Past [`MOST`] the
    /// phase is added to the last row (named by a bound, never dropped).
    pub fn mark(&mut self, name: &'static str) {
        let now = self.clock.now();
        let ns = now.since(self.last).map_or(0, nanos);
        self.last = now;
        // The meter after the clock: the clock's read is the phase's, the meter's own read the
        // next one's (its cost, stated in blaze-meter's docs, is in some phase either way).
        let meter = blaze_meter::Meter::now();
        let used = meter.since(&self.meter);
        self.meter = meter;
        match self.len < MOST {
            true => {
                self.rows[self.len] = (name, ns);
                self.used[self.len] = used;
                self.len += 1;
            }
            false => {
                self.rows[MOST - 1].1 += ns;
                self.used[MOST - 1] = match (self.used[MOST - 1], used) {
                    (Ok(have), Ok(new)) => Ok(have.plus(&new)),
                    (Err(why), _) | (_, Err(why)) => Err(why),
                };
            }
        }
    }

    /// Every phase's use summed, and every other thread's stretch given: what the meter
    /// accounts of the process.
    /// A phase whose pair was refused is not in it: the reconciliation then says what it misses.
    fn accounted(&self) -> blaze_meter::Used {
        let mut sum = match self.used[..self.len].iter().flatten().next() {
            Some(first) => first.zero_like(),
            None => blaze_meter::Used::ZERO,
        };
        for used in self.used[..self.len].iter().flatten() {
            sum = sum.plus(used);
        }
        for (_, spread) in &self.beside[..self.beside_len] {
            if spread.n > 0 {
                sum = sum.plus(&spread.sum);
            }
        }
        sum
    }

    /// Nanoseconds since `main` began.
    pub fn in_main(&self) -> u64 {
        self.since(self.began)
    }

    pub(crate) fn write_lowering_stats(&self, cx: &bstd::Cx, err: &bstd::out::Out) -> Result<(), bstd::out::OutError> {
        if let Some(tally) = self.code_records {
            err.line(cx, format_args!("blaze: {}", tally.lowerings))?;
        }
        Ok(())
    }
}

fn nanos(d: core::time::Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

impl fmt::Display for Phases {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.before_runtime {
            Some(ns) => write!(f, "phases       process start to runtime start {ns} · runtime start to main {}", self.before_main)?,
            None => write!(f, "phases       process start unknown · runtime start to main {}", self.before_main)?,
        }
        for (name, ns) in &self.rows[..self.len] {
            write!(f, " · {name} {ns}")?;
        }
        writeln!(f, " · in main {} (ns; the profile's own write and exit are after this line)", self.in_main())?;
        // Where the time before `main` went, as the process's own counters say (all threads,
        // from its start): CPU against the wall above is work; the rest a wait or a page-in.
        let used = self.before_main_used;
        write!(f, "before main  ")?;
        for (name, reading) in [
            ("user ns", used.user_ns),
            ("system ns", used.system_ns),
            ("instructions", used.instructions),
            ("minor faults", used.minor_faults),
            ("major faults", used.major_faults),
        ] {
            match reading {
                blaze_meter::Reading::Got(n) => write!(f, "{name} {n} · ")?,
                blaze_meter::Reading::Unavailable(why) => write!(f, "{name} unread ({why:?}) · ")?,
            }
        }
        writeln!(f, "(the process's own counters from its start to main's first line)")?;
        // D169: what each phase used, one line a phase, only its counters that are not zero.
        for ((name, _), used) in self.rows[..self.len].iter().zip(self.used.iter()) {
            match used {
                Ok(used) => writeln!(f, "phase used   {name}: {used}")?,
                Err(why) => writeln!(f, "phase used   {name}: not metered: {why}")?,
            }
        }
        for (kind, spread) in &self.beside[..self.beside_len] {
            writeln!(f, "beside       {kind}, {} stretches on other threads: {}", spread.n, spread.sum)?;
            if let Some(why) = spread.first_unpaired {
                writeln!(f, "             {} stretches not metered, the first: {why}", spread.unpaired)?;
            }
            writeln!(f, "             the mean one: {}", spread.mean())?;
            writeln!(f, "             each counter's most: {}", spread.most)?;
        }
        reconciled(f, &self.accounted(), &blaze_meter::Process::now())?;
        floors(f, &self.accounted(), &self.used[..self.len])?;
        writeln!(f, "names        worlds made {}", self.worlds)?;
        let r = self.request;
        writeln!(
            f,
            "request      steps {} gets {} puts {} runs {} publish {} prints {} write last {} (ns)",
            r.steps, r.gets, r.puts, r.runs, r.publish, r.prints, r.write_last
        )?;
        let p = r.print_parts;
        writeln!(f, "prints       listing {} grouping {} notes {} findings {} roots {} (ns)", p.listing, p.grouping, p.notes, p.findings, p.roots)?;
        if let Some(h) = self.frames {
            writeln!(
                f,
                "frames       most held (bytes): request {} · a task's scratch {} · a batch's records {} · a crate scope {} · a file's body job {}",
                h.request, h.scratch, h.batch, h.scope, h.bodies
            )?;
            write!(f, "{}", h.tasks)?;
            write!(f, "{}", h.scope_bytes)?;
        }
        write!(f, "{}", self.records)?;
        if let Some(tally) = self.code_records {
            writeln!(f, "run          {}", tally.codes)?;
            writeln!(f, "run          {}", tally.start)?;
            writeln!(f, "run          {}", tally.lowerings)?;
            if let Some(walk) = tally.walk {
                writeln!(f, "run walk     {walk}")?;
            }
        }
        if let Some(read) = self.run_names {
            writeln!(f, "run          {read}")?;
        }
        if let Some(cleanup) = self.cleanup {
            writeln!(f, "{cleanup}")?;
        }
        if let Some(taken) = self.published {
            writeln!(f, "{taken}")?;
        }
        let held = &self.held;
        write!(f, "frame        the command's frame: {} of {} bytes used · by phase (bytes):", held.seen.0, held.seen.1)?;
        for ((name, _), bytes) in self.rows[..self.len].iter().zip(held.phases.iter()).filter(|(_, bytes)| **bytes > 0) {
            write!(f, " · {name} {bytes}")?;
        }
        writeln!(f)?;
        write!(f, "frame        inside the request, by what took them (bytes, times, main thread ns):")?;
        for (kind, bytes, times, ns) in held.kinds[..held.len].iter() {
            write!(f, " · {kind} {bytes} {times}x {ns}")?;
        }
        writeln!(f)?;
        memory_line(f, "by main", &self.at_main)?;
        if let Some(calls) = &self.first_chunk {
            memory_line(f, "by the first chunk", calls)?;
        }
        memory_line(f, "by the profile", &blaze_mem::syscalls::snapshot())?;
        held_line(f, &blaze_mem::tag::snapshot())
    }
}

/// The reconciliation (D169): each counter the process keeps in all, against what the phases
/// and the stretches beside them accounted; a share under [`ACCOUNTED_PERCENT`] is a line of
/// its own, `UNACCOUNTED <counter>: <amount>`. The process's heap is blaze-mem's churn, folded
/// a batch at a time per thread (counted.rs), so it may trail the meter's by a batch a thread.
fn reconciled(f: &mut fmt::Formatter<'_>, accounted: &blaze_meter::Used, process: &blaze_meter::Process) -> fmt::Result {
    use blaze_meter::{Bytes, Count, Large, Ns, Reading};
    let s = blaze_mem::tag::snapshot();
    let heap = blaze_mem::Tag::ALL.iter().fold(s.heap_outside_allocated, |sum, tag| sum.saturating_add(s.get(*tag).heap_allocated));
    let a = accounted.system;
    let p = process.system;
    #[derive(Clone, Copy)]
    enum Unit {
        Count,
        Bytes,
        Ns,
    }
    let rows: [(&str, Reading, Reading, Unit); 6] = [
        ("instr", a.instructions, p.instructions, Unit::Count),
        ("cycles", a.cycles, p.cycles, Unit::Count),
        ("user", a.user_ns, p.user_ns, Unit::Ns),
        ("sys", a.system_ns, p.system_ns, Unit::Ns),
        ("faults", a.faults(), p.faults(), Unit::Count),
        ("heap bytes", Reading::Got(accounted.count(Count::HeapBytes)), Reading::Got(heap), Unit::Bytes),
    ];
    let amount = |f: &mut fmt::Formatter<'_>, unit: Unit, n: u64| match unit {
        Unit::Count => write!(f, "{}", Large(n)),
        Unit::Bytes => write!(f, "{}", Bytes(n)),
        Unit::Ns => write!(f, "{}", Ns(n)),
    };
    write!(f, "meter        accounted of the process's totals:")?;
    let mut short: [Option<(&str, u64, Unit)>; 6] = [None; 6];
    for (at, (name, got, all, unit)) in rows.iter().enumerate() {
        match (got, all) {
            (Reading::Got(got), Reading::Got(all)) => {
                write!(f, " · {name} ")?;
                amount(f, *unit, *got)?;
                write!(f, " of ")?;
                amount(f, *unit, *all)?;
                let percent = if *all == 0 { 100 } else { got.saturating_mul(100) / all };
                write!(f, " ({percent}%)")?;
                if percent < ACCOUNTED_PERCENT {
                    short[at] = Some((name, all.saturating_sub(*got), *unit));
                }
            }
            (Reading::Unavailable(why), _) | (_, Reading::Unavailable(why)) => write!(f, " · {name} unavailable ({})", why.says())?,
        }
    }
    match process.most_resident {
        Reading::Got(bytes) => writeln!(f, " · most resident {}", Bytes(bytes))?,
        Reading::Unavailable(why) => writeln!(f, " · most resident unavailable ({})", why.says())?,
    }
    for (name, missing, unit) in short.iter().flatten() {
        write!(f, "UNACCOUNTED {name}: ")?;
        amount(f, *unit, *missing)?;
        writeln!(f)?;
    }
    Ok(())
}

/// D168's floors, each with its floor: store asks answered again (asks less keys that reached
/// the store, floor 0); heads listed (no counter yet: said so, never 0); heap bytes kept past
/// the phases on this thread (allocated less freed over every metered phase, floor 0; signed,
/// as the thread may free what other threads allocated: `-` then, never a saturated 0).
fn floors(f: &mut fmt::Formatter<'_>, accounted: &blaze_meter::Used, phases: &[Result<blaze_meter::Used, blaze_meter::Unpaired>]) -> fmt::Result {
    use blaze_meter::{Bytes, Count, Grouped};
    let repeat = accounted.count(Count::StoreGets).saturating_sub(accounted.count(Count::StoreKeys));
    let kept: i128 = phases.iter().flatten().map(blaze_meter::Used::heap_kept).sum();
    let sign = if kept < 0 { "-" } else { "" };
    writeln!(
        f,
        "floors       repeat store asks {} (floor 0) · heads listed: not metered · heap kept past the phases on the command's thread {sign}{} (floor 0)",
        Grouped(repeat),
        Bytes(u64::try_from(kept.unsigned_abs()).unwrap_or(u64::MAX))
    )
}

/// The `memory held` line: where the command's peak footprint can sit, each number blaze-mem's
/// own counter (rule 17: a peak the task frames do not explain is named by its holder). The
/// regions' committed bytes at most and now, what of them is purged (out of the footprint), each
/// tag's most held in regions and in the global heap, the global heap outside any tag now, and
/// what the global allocator says it committed. High waters are each their own counter's, not
/// one instant's: a tag whose high water nears the command's peak footprint holds it.
fn held_line(f: &mut fmt::Formatter<'_>, s: &blaze_mem::tag::Snapshot) -> fmt::Result {
    write!(
        f,
        "memory       held (bytes): regions committed most {} now {} purged {} · global heap outside tags most {} now {} (allocated {} freed {}) · global allocator committed {}",
        s.committed_high_water,
        s.committed,
        s.purged,
        blaze_mem::tag::heap_outside_high_water(),
        s.heap_outside,
        s.heap_outside_allocated,
        s.heap_outside_freed,
        s.heap_committed.map_or(-1, i128::from),
    )?;
    write!(f, " · by tag (most in regions, most in the heap):")?;
    for tag in blaze_mem::Tag::ALL {
        let c = s.get(tag);
        if c.high_water > 0 || c.heap_high_water > 0 {
            write!(f, " · {tag} {} {}", c.high_water, c.heap_high_water)?;
        }
    }
    writeln!(f)
}

#[cfg(test)]
mod tests {
    use super::Phases;
    use crate::run_parent::{Built, ListHeld, RunTally, StartTally, Tally};
    use blaze_ladder::run::make_one::LoweringCounts;
    use bstd::io::{Io, Limits, SimWorld};

    #[test]
    fn run_stats_and_profile_print_lowering_counts() {
        let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
        let mut phases = Phases::new(io.need_clock().expect("the simulated clock"));
        let start = StartTally { placed: 0, answered: 0, data: 0, positioned: 0, resolved: 0, built: Built::Head, held: ListHeld::None };
        let lowerings = LoweringCounts { bodies_lowered: 7, lower_again: 2, forks: 1, typing: None };
        phases.code_records = Some(RunTally { codes: Tally::default(), start, lowerings, walk: None });
        let profile = std::format!("{phases}");
        assert!(profile.lines().any(|line| line == "run          bodies lowered 7, lower_again calls 2, forks 1"), "{profile}");
        let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("the simulated context");
        let err = io.need_err().expect("stderr");
        phases.write_lowering_stats(&cx, &err).unwrap();
        err.flush(&cx).unwrap();
        io.script(|host| assert_eq!(host.err.as_slice(), b"blaze: bodies lowered 7, lower_again calls 2, forks 1\n")).expect("the simulated host");
    }

    /// A bstd wait in a phase is in that phase's use when it is marked through the command's
    /// `cx` ([`Phases::mark_cx`]); CONTROL (rule 23): marked as before ([`Phases::mark_in`]),
    /// the same wait is in no phase ("0 waits" on tempo's 177 ms replay).
    #[test]
    fn a_wait_in_a_phase_is_in_its_use_when_marked_through_cx() {
        let io = Io::simulated(SimWorld::new(5, Limits::USUAL));
        let clock = io.need_clock().expect("the simulated clock");
        let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("the simulated context");
        let frame = cx.frame();
        let mut phases = Phases::new(io.need_clock().expect("the simulated clock"));
        phases.mark_cx(&cx, "before", frame);
        clock.sleep(&cx, core::time::Duration::from_millis(5)).expect("the sleep");
        phases.mark_cx(&cx, "slept", frame);
        phases.mark_cx(&cx, "nothing", frame);
        let waited = |row: usize| phases.used[row].as_ref().map(|used| used.waited.total()).expect("one thread's marks pair");
        let (count, ns) = waited(1);
        assert!(count >= 1 && ns >= 5_000_000, "the sleep is in the phase it ran in: {count} waits {ns} ns");
        assert_eq!(waited(2), (0, 0), "and in no later phase");
        clock.sleep(&cx, core::time::Duration::from_millis(5)).expect("the sleep");
        phases.mark_in("slept, marked without cx", frame);
        // A fresh reader: the first one's borrow of `phases` ended at its last call, before the mark.
        let waited = |row: usize| phases.used[row].as_ref().map(|used| used.waited.total()).expect("one thread's marks pair");
        assert_eq!(waited(3), (0, 0), "the control: the thread's meter alone cannot see a worker's wait");
    }

    #[test]
    fn reset_clears_beside_slots() {
        let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
        let mut p = Phases::new(io.need_clock().expect("clock"));
        let mut spread = blaze_meter::Spread::NONE;
        spread.n = 1;
        p.worked_used("bodies", &spread);
        assert_eq!(p.beside_len, 1);
        assert_eq!(p.beside[0].1.n, 1);
        p.reset();
        assert_eq!(p.beside_len, 0);
        assert_eq!(p.beside[0].1.n, 0);
        p.worked_used("bodies", &spread);
        assert_eq!(p.beside_len, 1);
        assert_eq!(p.beside[0].1.n, 1);
    }
}
