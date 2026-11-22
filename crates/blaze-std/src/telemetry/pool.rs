//! A pool worker's facts: its time working, parked and idle (spinning before a park), the
//! tasks it ran and failed, its longest task by name, and a ring of the last
//! [`DEPTH_SAMPLES`] ready-queue depths, each with why the worker was idle when it was.
//!
//! The three times reconcile: from the worker's start to its stop, every nanosecond is in one
//! of them (the pool loop measures each stretch between two clock reads it makes anyway).

/// The samples a worker's ring keeps: the last 100, 24 bytes each.
pub const DEPTH_SAMPLES: usize = 100;

/// Why a worker had nothing to run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Idle {
    /// No task was ready anywhere it looks.
    NothingReady,
    /// Tasks exist, and every one of them is waiting on something.
    AllWaiting,
}

impl Idle {
    pub const fn code(self) -> u64 {
        match self {
            Idle::NothingReady => 0,
            Idle::AllWaiting => 1,
        }
    }

    pub const fn says(self) -> &'static str {
        match self {
            Idle::NothingReady => "nothing ready",
            Idle::AllWaiting => "all waiting",
        }
    }
}

/// One sample of the ready queue.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DepthSample {
    /// On the world's clock, from the run's start.
    pub at_ns: u64,
    pub worker: u32,
    /// Tasks ready to run where the worker looks (its deque and the injector).
    pub depth: u32,
    /// `None`: the worker took a task.
    pub idle: Option<Idle>,
}

impl DepthSample {
    pub const ZERO: DepthSample = DepthSample { at_ns: 0, worker: 0, depth: 0, idle: None };
}

/// The longest task: its name and time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Longest {
    pub name: &'static str,
    pub ns: u64,
}

/// A worker's facts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pool {
    pub started_ns: u64,
    pub stopped_ns: Option<u64>,
    pub working_ns: u64,
    pub parked_ns: u64,
    pub idle_ns: u64,
    pub tasks: u64,
    pub failed: u64,
    pub longest: Option<Longest>,
    samples: [DepthSample; DEPTH_SAMPLES],
    /// Samples offered: the ring holds the last `min(sampled, DEPTH_SAMPLES)`.
    sampled: u64,
}

impl Pool {
    pub const ZERO: Pool = Pool {
        started_ns: 0,
        stopped_ns: None,
        working_ns: 0,
        parked_ns: 0,
        idle_ns: 0,
        tasks: 0,
        failed: 0,
        longest: None,
        samples: [DepthSample::ZERO; DEPTH_SAMPLES],
        sampled: 0,
    };

    /// The worker's loop began at `at_ns`.
    pub fn started(&mut self, at_ns: u64) {
        self.started_ns = at_ns;
    }

    /// The worker's loop ended at `at_ns`.
    pub fn stopped(&mut self, at_ns: u64) {
        self.stopped_ns = Some(at_ns);
    }

    #[inline]
    pub fn worked(&mut self, ns: u64) {
        self.working_ns = self.working_ns.wrapping_add(ns);
    }

    #[inline]
    pub fn parked(&mut self, ns: u64) {
        self.parked_ns = self.parked_ns.wrapping_add(ns);
    }

    #[inline]
    pub fn idled(&mut self, ns: u64) {
        self.idle_ns = self.idle_ns.wrapping_add(ns);
    }

    #[inline]
    pub(super) fn task_ended(&mut self, name: &'static str, ns: u64, failed: bool) {
        self.tasks += 1;
        if failed {
            self.failed += 1;
        }
        match self.longest {
            Some(longest) if longest.ns >= ns => {}
            _ => self.longest = Some(Longest { name, ns }),
        }
    }

    /// One sample of the ready queue: a store into the ring.
    #[inline]
    pub fn sample(&mut self, sample: DepthSample) {
        let at = (self.sampled % DEPTH_SAMPLES as u64) as usize;
        self.samples[at] = sample;
        self.sampled += 1;
    }

    /// The ring's samples, oldest first.
    pub fn samples(&self) -> impl Iterator<Item = &DepthSample> {
        let held = if self.sampled < DEPTH_SAMPLES as u64 { self.sampled as usize } else { DEPTH_SAMPLES };
        let first = if self.sampled < DEPTH_SAMPLES as u64 { 0 } else { (self.sampled % DEPTH_SAMPLES as u64) as usize };
        (0..held).map(move |n| &self.samples[(first + n) % DEPTH_SAMPLES])
    }

    pub fn sampled(&self) -> u64 {
        self.sampled
    }

    /// Its time from start to stop (`None` while running).
    pub fn lifetime(&self) -> Option<u64> {
        self.stopped_ns.map(|stopped| stopped.saturating_sub(self.started_ns))
    }

    /// Working, parked and idle together.
    pub fn accounted(&self) -> u64 {
        self.working_ns.wrapping_add(self.parked_ns).wrapping_add(self.idle_ns)
    }
}
