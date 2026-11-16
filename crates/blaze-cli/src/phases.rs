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
    /// The command's own frame, by what took its bytes ([`Phases::mark_in`], [`Phases::took`]).
    held: Held,
}

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
            held: Held { phase_at: 0, kind_at: 0, kind_since: None, phases: [0; MOST], kinds: [("", 0, 0, 0); MOST_KINDS], len: 0, seen: (0, 0) },
        }
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
        memory_line(f, "by the profile", &blaze_mem::syscalls::snapshot())
    }
}
