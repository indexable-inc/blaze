//! The command's own time (rule 17): every phase of one command in order, in nanoseconds on the
//! clock `main` is handed (rule 75), and inside the request each kind of effect summed. Written
//! as two lines of `.blaze/v2/profile.txt`, so why a command took what it took is read, not
//! guessed. What happens after `main` returns (the runtime's teardown, the process exit) is
//! outside any clock this binary holds: the wall time less `in main` is start and exit.

use core::fmt;

use bstd::clock::{Clock, Instant};

/// The most phases one command marks.
const MOST: usize = 16;

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
    pub write_last: u64,
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
            clock,
            rows: [("", 0); MOST],
            len: 0,
            request: Effects::default(),
            first_chunk: None,
            at_main: blaze_mem::syscalls::snapshot(),
            frames: None,
        }
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
        match self.len < MOST {
            true => {
                self.rows[self.len] = (name, ns);
                self.len += 1;
            }
            false => self.rows[MOST - 1].1 += ns,
        }
    }

    /// Nanoseconds since `main` began.
    pub fn in_main(&self) -> u64 {
        self.since(self.began)
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
        let r = self.request;
        writeln!(
            f,
            "request      steps {} gets {} puts {} runs {} publish {} prints {} write last {} (ns)",
            r.steps, r.gets, r.puts, r.runs, r.publish, r.prints, r.write_last
        )?;
        if let Some(h) = self.frames {
            writeln!(
                f,
                "frames       most held (bytes): request {} · a task's scratch {} · a batch's records {} · a record read back {}",
                h.request, h.scratch, h.batch, h.got
            )?;
        }
        memory_line(f, "by main", &self.at_main)?;
        if let Some(calls) = &self.first_chunk {
            memory_line(f, "by the first chunk", calls)?;
        }
        memory_line(f, "by the profile", &blaze_mem::syscalls::snapshot())
    }
}
