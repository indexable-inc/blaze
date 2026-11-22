//! Where a proc-macro call's time goes: one row per part of [`crate::hook::ProcMacros::run`]
//! and of [`crate::host::Runtime::expand`], summed over every call of a runtime.
//!
//! The clock is passed in ([`Parts::timed`], rule 75): a runtime made without one counts the
//! entries and reads no time, so a build that is not profiled pays one predicted branch a mark.
//! A profile's driver hands its own monotonic clock and prints [`Parts::rows`].

use core::sync::atomic::{AtomicU64, Ordering};

/// One part of a call. The order is the order a call passes them in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Part {
    /// The call's key hashed from its tokens (`record::base_key`).
    Key,
    /// The kept answer looked for: the lookup, its reads' check and the decode.
    Find,
    /// The module's loaded code asked of the runtime (a wait when another worker loads it).
    Load,
    /// The bridge server made and the call's input encoded for the module.
    Serve,
    /// The sandbox, the store, its limiter and its fuel.
    Setup,
    /// The instance made from the module's pre-linked form: its memory and tables.
    Instantiate,
    /// `_initialize`: the module's constructors and wasi-libc's start-up.
    Initialize,
    /// The exports found (the declarations' global, the allocator, the entry).
    Exports,
    /// The input allocated in the module and written to its memory.
    Input,
    /// The macro's own code: the entry's call, less the time inside the host's calls.
    Guest,
    /// The bridge's requests served by the host inside the entry's call.
    Bridge,
    /// The fuel read back and the store dropped: the instance's memory unmapped.
    Free,
    /// The module's output decoded as the bridge's result.
    Outcome,
    /// The run's reads gathered into its read list.
    Reads,
    /// The output handle made tokens, origins and diagnostics in the caller's allocator.
    Produce,
    /// The answer kept.
    Keep,
}

impl Part {
    /// Every part, in order.
    pub const ALL: [Part; 16] = [
        Part::Key,
        Part::Find,
        Part::Load,
        Part::Serve,
        Part::Setup,
        Part::Instantiate,
        Part::Initialize,
        Part::Exports,
        Part::Input,
        Part::Guest,
        Part::Bridge,
        Part::Free,
        Part::Outcome,
        Part::Reads,
        Part::Produce,
        Part::Keep,
    ];

    /// The part's name, as a profile prints it.
    pub const fn name(self) -> &'static str {
        match self {
            Part::Key => "Key",
            Part::Find => "Find",
            Part::Load => "Load",
            Part::Serve => "Serve",
            Part::Setup => "Setup",
            Part::Instantiate => "Instantiate",
            Part::Initialize => "Initialize",
            Part::Exports => "Exports",
            Part::Input => "Input",
            Part::Guest => "Guest",
            Part::Bridge => "Bridge",
            Part::Free => "Free",
            Part::Outcome => "Outcome",
            Part::Reads => "Reads",
            Part::Produce => "Produce",
            Part::Keep => "Keep",
        }
    }
}

/// A monotonic clock: nanoseconds since any fixed instant of the process.
pub type Clock = fn() -> u64;

/// One part's sum.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Row {
    pub part: Part,
    /// How many times the part was passed.
    pub entered: u64,
    /// The nanoseconds spent in it; 0 without a clock.
    pub nanos: u64,
}

/// The sums of every part: see the module's doc.
pub struct Parts {
    clock: Option<Clock>,
    nanos: [AtomicU64; 16],
    entered: [AtomicU64; 16],
}

impl Parts {
    /// Counts entries only.
    pub const fn counted() -> Parts {
        Parts { clock: None, nanos: [const { AtomicU64::new(0) }; 16], entered: [const { AtomicU64::new(0) }; 16] }
    }

    /// Counts entries and sums the time `clock` gives.
    pub const fn timed(clock: Clock) -> Parts {
        Parts { clock: Some(clock), nanos: [const { AtomicU64::new(0) }; 16], entered: [const { AtomicU64::new(0) }; 16] }
    }

    /// The clock, when the parts are timed.
    pub const fn clock(&self) -> Option<Clock> {
        self.clock
    }

    /// Now, 0 without a clock.
    #[inline]
    pub fn now(&self) -> u64 {
        match self.clock {
            Some(clock) => clock(),
            None => 0,
        }
    }

    /// A mark at now: each [`Mark::lap`] gives the time since the one before to a part.
    #[inline]
    pub fn start(&self) -> Mark<'_> {
        Mark { parts: self, at: self.now() }
    }

    /// `nanos` more in `part`, entered once.
    #[inline]
    pub fn add(&self, part: Part, nanos: u64) {
        self.entered[part as usize].fetch_add(1, Ordering::Relaxed);
        if nanos != 0 {
            self.nanos[part as usize].fetch_add(nanos, Ordering::Relaxed);
        }
    }

    /// `nanos` more in `part`, entered `entered` times: the bridge's requests of one call.
    pub fn add_many(&self, part: Part, nanos: u64, entered: u64) {
        self.entered[part as usize].fetch_add(entered, Ordering::Relaxed);
        self.nanos[part as usize].fetch_add(nanos, Ordering::Relaxed);
    }

    /// Every part's sum so far, in [`Part::ALL`]'s order.
    pub fn rows(&self) -> [Row; 16] {
        Part::ALL.map(|part| Row { part, entered: self.entered[part as usize].load(Ordering::Relaxed), nanos: self.nanos[part as usize].load(Ordering::Relaxed) })
    }
}

/// A point in a call: see [`Parts::start`].
pub struct Mark<'p> {
    parts: &'p Parts,
    at: u64,
}

impl Mark<'_> {
    /// The time since the last lap (or the start) given to `part`.
    #[inline]
    pub fn lap(&mut self, part: Part) {
        let spent = self.split();
        self.parts.add(part, spent);
    }

    /// The time since the last lap (or the start), given to no part: the caller shares it out.
    #[inline]
    pub fn split(&mut self) -> u64 {
        let now = self.parts.now();
        let spent = now.saturating_sub(self.at);
        self.at = now;
        spent
    }
}
