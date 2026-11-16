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
    /// From the driver's start to `main`'s first line.
    before_main: u64,
    rows: [(&'static str, u64); MOST],
    len: usize,
    /// Inside the request: the machine's steps and each kind of effect performed, summed.
    pub request: Effects,
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
        Phases {
            began: now,
            last: now,
            before_main: nanos(now.since_start()),
            clock,
            rows: [("", 0); MOST],
            len: 0,
            request: Effects::default(),
        }
    }

    pub fn now(&self) -> Instant {
        self.clock.now()
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
        write!(f, "phases       before main {}", self.before_main)?;
        for (name, ns) in &self.rows[..self.len] {
            write!(f, " · {name} {ns}")?;
        }
        writeln!(f, " · in main {} (ns; the profile's own write and exit are after this line)", self.in_main())?;
        let r = self.request;
        writeln!(
            f,
            "request      steps {} gets {} puts {} runs {} publish {} prints {} write last {} (ns)",
            r.steps, r.gets, r.puts, r.runs, r.publish, r.prints, r.write_last
        )
    }
}
