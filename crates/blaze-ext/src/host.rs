//! The host's side of the API: what `blaze-ext-host` makes a job from and reads back. Nothing
//! here is of use to an extension: a [`Job`] takes a [`HostKey`], and a `HostKey` takes the
//! database, which no extension is handed.
//!
//! # Bounds
//!
//! A job is bounded three ways, each ending it with a typed [`Stop`] that the host turns into
//! an error naming the extension, the lint or fact and the item (rule `bounded-progress`):
//!
//! - **Fuel.** A count of steps, not a time: the same job stops at the same step on every run
//!   and every machine, so a stop never depends on load (rule `deterministic-output`). Code
//!   blaze compiled for an extension is metered (`blaze_stencil::fuel`: a tick at every
//!   function's entry and every loop head, so no cycle of its control flow runs without one)
//!   and finds the job's counter through [`fuel_cell`], calling [`fuel_out`] at zero; every
//!   call into this crate ([`crate::Cx`]'s methods) costs a step too.
//! - **Stack.** [`fuel_cell`] runs at every metered function's entry, and compares the stack's
//!   depth since the job began with [`Limits::stack`]: a runaway recursion stops there, not at
//!   the guard page.
//! - **Memory.** [`crate::Arena`] counts what it is asked for against [`Limits::arena`], and
//!   the text of findings against [`Limits::text`].
//!
//! A stop unwinds (`resume_unwind`, so no panic hook runs) to the host's `catch_unwind`.
//!
//! # No waits
//!
//! The job's counter is found through a thread-local, so nothing an extension calls may wait:
//! a fiber that waits can resume on another thread (rule `no-wait-across-thread-locals`). That
//! is why a [`crate::Body`] carries what the front end answered before the job began
//! ([`Prefetched`]) and why no method of this crate asks the database.

use std::{
    cell::{Cell, RefCell},
    fmt::{self, Write as _},
    marker::PhantomData,
};

use blaze_mem::Frame;

use crate::{Applicability, Arena, Body, Callee, Cx, ImplOf, Item, mir::MirSpan};

/// What only the host can make: the right to start a job.
pub struct HostKey(());

impl HostKey {
    /// The key of a host that holds the database.
    pub fn of(_db: &base_db::Db) -> HostKey {
        HostKey(())
    }

    /// The key of a test that runs a job with no database.
    #[cfg(test)]
    pub fn testing() -> HostKey {
        HostKey(())
    }
}

/// The bounds of one job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Steps: metered ticks and calls into the API.
    pub fuel: u64,
    /// Bytes the arena may be asked for.
    pub arena: usize,
    /// Bytes of stack the job may use below where it began.
    pub stack: usize,
    /// Findings one job may report.
    pub findings: usize,
    /// Bytes of text (messages, notes, fixes) one job may report.
    pub text: usize,
}

impl Limits {
    /// What a job has where the workspace sets nothing: 50 million steps (tens of
    /// milliseconds of compiled code), 64 MiB, 256 KiB of stack, 256 findings and 256 KiB of
    /// text. A lint over one body is far under each; a job at one is stuck, not slow.
    pub const DEFAULT: Limits = Limits {
        fuel: 50_000_000,
        arena: 64 << 20,
        stack: 256 << 10,
        findings: 256,
        text: 256 << 10,
    };
}

/// Why a job was stopped before it ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// It ran out of steps.
    Fuel { budget: u64 },
    /// Its arena was asked for more than its cap.
    Memory { asked: usize, cap: usize },
    /// Its stack went deeper than its cap.
    Stack { cap: usize },
    /// It reported more findings than its cap.
    Findings { cap: usize },
    /// It reported more text than its cap.
    Text { cap: usize },
    /// It asked for a fact it did not declare, or of an item its body does not call.
    Undeclared { fact: String },
    /// It asked for a fact whose own job failed on that item.
    Failed { fact: String },
    /// A fact's bytes did not decode as the type asked for.
    Malformed { fact: String },
    /// It used the API as its kind of job may not.
    Misuse { what: &'static str },
}

impl fmt::Display for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Stop::Fuel { budget } => write!(
                f,
                "it ran out of fuel ({budget} steps): a loop that does not end, or work past the bound"
            ),
            Stop::Memory { asked, cap } => {
                write!(f, "its arena was asked for {asked} bytes, past its cap of {cap}")
            }
            Stop::Stack { cap } => write!(
                f,
                "its stack went deeper than its cap of {cap} bytes: a recursion that does not end"
            ),
            Stop::Findings { cap } => write!(f, "it reported more than {cap} findings of one body"),
            Stop::Text { cap } => {
                write!(f, "it reported more than {cap} bytes of text of one body")
            }
            Stop::Undeclared { fact } => write!(
                f,
                "it asked for the fact `{fact}`, which it does not declare in `requires`, or of an item its body does not call"
            ),
            Stop::Failed { fact } => write!(
                f,
                "it asked for the fact `{fact}` of an item on which that fact's own job failed"
            ),
            Stop::Malformed { fact } => {
                write!(f, "the bytes of the fact `{fact}` do not decode as the type it asked for")
            }
            Stop::Misuse { what } => f.write_str(what),
        }
    }
}

/// Ends the running job with `stop`: an unwind to the host, with no panic hook run.
pub(crate) fn stop(stop: Stop) -> ! {
    // alloc: the payload of one unwind, the end of a job
    std::panic::resume_unwind(Box::new(stop))
}

/// One finding of a lint, as the job leaves it: placed and named by the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// Which lint of the extension, by its index in `Extension::lints`.
    pub lint: u32,
    pub at: MirSpan,
    pub message: String,
    pub notes: Vec<String>,
    pub helps: Vec<String>,
    pub fixes: Vec<Fix>,
}

/// One fix of a finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fix {
    pub at: MirSpan,
    pub message: String,
    pub replacement: String,
    pub applicability: Applicability,
}

/// What the host answers a job that asks for a fact.
pub enum Lookup<'s> {
    Value(&'s [u8]),
    /// The host holds no such fact of the item: another crate's, or one with no body.
    Unknown,
    /// The job did not declare the fact, or the item is not the body's or one it calls.
    Undeclared,
    /// The fact's own job failed on the item: whoever asks is stopped too, never handed a
    /// stand-in.
    Failed,
}

/// Where a job's facts come from: the host's table, narrowed to what the job declared.
pub trait Source {
    fn fact(&self, name: &str, item: Item) -> Lookup<'_>;
}

/// A job with no facts to ask for.
pub struct NoFacts;

impl Source for NoFacts {
    fn fact(&self, _: &str, _: Item) -> Lookup<'_> {
        Lookup::Undeclared
    }
}

/// One run of an extension's function: its counter, its arena and what it reported.
pub struct Job<'a> {
    arena: Arena<'a>,
    limits: Limits,
    fuel: Cell<u64>,
    /// The lowest address the job's stack may reach: set when it is entered.
    floor: Cell<usize>,
    text: Cell<usize>,
    findings: RefCell<Vec<Finding>>,
    // dyn: asked once per `Cx::fact`, a few times per body
    facts: &'a dyn Source,
}

impl<'a> Job<'a> {
    /// A job in `frame`, bounded by `limits`, asking `facts`. `skew` bytes of the frame are
    /// taken first and never used: a replay gives another skew, so every address the arena
    /// hands out differs, and a result that depends on one shows.
    pub fn new(
        _key: &HostKey,
        frame: &'a Frame,
        limits: Limits,
        // dyn: as the field
        facts: &'a dyn Source,
        skew: usize,
    ) -> Job<'a> {
        if let Ok(layout) = std::alloc::Layout::from_size_align(skew, 1)
            && skew > 0
        {
            let _unused = frame.alloc_layout(layout);
        }
        Job {
            arena: Arena::new(frame, limits.arena),
            limits,
            fuel: Cell::new(limits.fuel),
            floor: Cell::new(0),
            text: Cell::new(0),
            // alloc: the findings of one job, read back by the host
            findings: RefCell::new(Vec::new()),
            facts,
        }
    }

    /// The context of a lint's job (`lint` its index in `Extension::lints`) or of a fact's.
    pub fn cx(&'a self, lint: Option<u32>) -> Cx<'a> {
        Cx::new(self, lint)
    }

    /// Makes this the job the calling thread runs until the result is dropped: metered code
    /// then counts down this job's fuel and is held to its stack. The job's stack begins
    /// here. Hold it only around the call into the extension, which never waits.
    pub fn enter(&self) -> Entered<'_> {
        let here = stack_address();
        let floor = here.saturating_sub(self.limits.stack);
        self.floor.set(floor);
        let running = Running {
            cell: self.fuel.as_ptr(),
            floor,
            stack: self.limits.stack,
            budget: self.limits.fuel,
        };
        let outer = RUNNING.with(|it| it.replace(running));
        Entered { outer, _job: PhantomData, _thread: PhantomData }
    }

    /// The fuel left.
    pub fn fuel_left(&self) -> u64 {
        self.fuel.get()
    }

    /// What the job reported, taken.
    pub fn take_findings(&self) -> Vec<Finding> {
        std::mem::take(&mut *self.findings.borrow_mut())
    }

    pub(crate) fn arena(&self) -> &Arena<'a> {
        &self.arena
    }

    /// `steps` of fuel for a call into the API, and the stack's depth checked.
    pub(crate) fn charge(&self, steps: u64) {
        let left = self.fuel.get();
        if left < steps {
            stop(Stop::Fuel { budget: self.limits.fuel });
        }
        self.fuel.set(left - steps);
        let floor = self.floor.get();
        if floor != 0 && stack_address() < floor {
            stop(Stop::Stack { cap: self.limits.stack });
        }
    }

    pub(crate) fn lookup(&self, name: &str, item: Item) -> Lookup<'_> {
        self.facts.fact(name, item)
    }

    /// `text` formatted, counted against the job's text.
    pub(crate) fn text(&self, text: fmt::Arguments<'_>) -> String {
        let mut out = Capped { text: String::new(), job: self }; // alloc: a finding's text, capped
        // A `Display` of the extension's runs here, metered like the rest of it. `Capped`
        // never errs (it stops the job), so an error is the extension's own `Display`'s.
        if out.write_fmt(text).is_err() {
            stop(Stop::Misuse {
                what: "a `Display` it formatted into a message returned an error",
            });
        }
        out.text
    }

    pub(crate) fn push(&self, finding: Finding) -> usize {
        let mut findings = self.findings.borrow_mut();
        if findings.len() >= self.limits.findings {
            drop(findings);
            stop(Stop::Findings { cap: self.limits.findings });
        }
        findings.push(finding);
        findings.len() - 1
    }

    pub(crate) fn amend(&self, at: usize, change: impl FnOnce(&mut Finding)) {
        self.charge(1);
        if let Some(finding) = self.findings.borrow_mut().get_mut(at) {
            change(finding);
        }
    }
}

/// Text a job formats, stopped at the job's cap.
struct Capped<'j, 'a> {
    text: String,
    job: &'j Job<'a>,
}

impl fmt::Write for Capped<'_, '_> {
    fn write_str(&mut self, part: &str) -> fmt::Result {
        let used = self.job.text.get().saturating_add(part.len());
        if used > self.job.limits.text {
            stop(Stop::Text { cap: self.job.limits.text });
        }
        self.job.text.set(used);
        self.text.push_str(part);
        Ok(())
    }
}

/// A job entered on this thread: dropping it gives the thread back to what ran before.
pub struct Entered<'j> {
    outer: Running,
    _job: PhantomData<&'j ()>,
    /// Thread-bound: it restores this thread's slot.
    _thread: PhantomData<*const ()>,
}

impl Drop for Entered<'_> {
    fn drop(&mut self) {
        RUNNING.with(|it| it.set(self.outer));
    }
}

/// The job a thread runs, as metered code finds it.
#[derive(Clone, Copy)]
struct Running {
    /// The job's counter; null when the thread runs none.
    cell: *mut u64,
    /// The lowest address its stack may reach, and the cap that is.
    floor: usize,
    stack: usize,
    budget: u64,
}

thread_local! {
    static RUNNING: Cell<Running> =
        const { Cell::new(Running { cell: std::ptr::null_mut(), floor: 0, stack: 0, budget: 0 }) };
    /// The counter metered code counts down on a thread that runs no job: never stopped.
    static IDLE: Cell<u64> = const { Cell::new(u64::MAX) };
}

/// About where the calling frame is on its stack.
#[inline(never)]
fn stack_address() -> usize {
    let here = 0u8;
    std::ptr::from_ref(&here).addr()
}

/// The fuel runtime's `__blaze_fuel_cell` for an extension's code: the counter of the job the
/// calling thread runs, asked once per call of a metered function. The stack's depth is
/// checked here too, so every call of the extension's is held to [`Limits::stack`]. The
/// loader resolves the symbol to this function; nothing exports it by name.
pub extern "C-unwind" fn fuel_cell() -> *mut u64 {
    let running = RUNNING.with(Cell::get);
    if running.cell.is_null() {
        return IDLE.with(Cell::as_ptr);
    }
    if stack_address() < running.floor {
        stop(Stop::Stack { cap: running.stack });
    }
    running.cell
}

/// The fuel runtime's `__blaze_fuel_out` for an extension's code: a tick found `cell` at zero
/// (it now reads `u64::MAX`). The job's counter ends the job; the idle counter is one no job
/// owns, and goes on.
pub extern "C-unwind" fn fuel_out(cell: *mut u64) {
    let running = RUNNING.with(Cell::get);
    if !running.cell.is_null() && running.cell == cell {
        stop(Stop::Fuel { budget: running.budget });
    }
    IDLE.with(|it| it.set(u64::MAX));
}

/// One tick, as `blaze_stencil::fuel` emits it: `c = *cell; *cell = c - 1; if c == 0 { out }`.
/// What metered code does at every function entry and loop head; the tests of the bounds
/// tick with it, since code linked into a test is not metered.
pub fn tick() {
    let cell = fuel_cell();
    // SAFETY: `fuel_cell` gives the address of a `Cell<u64>` that outlives this call: the
    // entered job's counter (the `Entered` guard is alive on this thread while it is set) or
    // this thread's idle one; only this thread reads or writes either.
    let count = unsafe { cell.read() };
    // SAFETY: as above.
    unsafe { cell.write(count.wrapping_sub(1)) };
    if count == 0 {
        fuel_out(cell);
    }
}

/// What the front end answered of one body before its job began: everything a
/// [`Body`]'s methods read, so that none of them asks the database.
pub struct Prefetched {
    pub name: String,
    pub impl_of: Option<ImplOf>,
    /// The callee of each block that ends in a call of a named function, by the block's
    /// index, ascending.
    pub callees: Vec<(u32, Callee)>,
}

/// A body for a job: `item`'s MIR with what was prefetched of it.
pub fn body<'a, 'db>(
    _key: &HostKey,
    item: Item,
    mir: &'db crate::mir::MirBody<'db>,
    prefetched: &'a Prefetched,
) -> Body<'a, 'db> {
    Body::new(item, mir, prefetched)
}

/// The item a body is of, for the host's tables.
pub fn item(_key: &HostKey, def: hir_def::DefWithBodyId) -> Item {
    Item::new(def)
}

/// Which body an item is.
pub fn def_of(item: Item) -> hir_def::DefWithBodyId {
    item.def()
}

#[cfg(test)]
mod tests {
    use blaze_mem::{Tag, frame};

    use super::*;

    fn stopped(run: impl FnOnce(&Job<'_>)) -> Option<Stop> {
        frame::task(Tag::Diagnostics, |frame| {
            let limits = Limits { fuel: 1000, arena: 4096, ..Limits::DEFAULT };
            let job = Job::new(&HostKey::testing(), frame, limits, &NoFacts, 0);
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _entered = job.enter();
                run(&job);
            }));
            outcome.err().map(|payload| *payload.downcast::<Stop>().expect("a stop"))
        })
    }

    #[test]
    fn metered_ticks_stop_a_job_at_its_fuel_and_nowhere_else() {
        assert_eq!(
            stopped(|_| {
                for _ in 0..999 {
                    tick();
                }
            }),
            None
        );
        // The tick that finds the counter at zero is the 1001st.
        assert_eq!(
            stopped(|_| {
                loop {
                    tick();
                }
            }),
            Some(Stop::Fuel { budget: 1000 })
        );
        // A thread with no job ticks the idle counter and is never stopped.
        for _ in 0..10_000 {
            tick();
        }
    }

    #[test]
    fn the_arena_stops_a_job_at_its_cap() {
        let stop = stopped(|job| {
            let mut bytes = job.cx(None).vec::<u64>();
            loop {
                bytes.push(1);
            }
        });
        assert!(matches!(stop, Some(Stop::Memory { cap: 4096, .. })), "{stop:?}");
    }

    #[test]
    fn a_recursion_stops_at_the_stack_cap() {
        fn down(job: &Job<'_>, depth: u64) -> u64 {
            let pad = [depth; 64];
            job.charge(0);
            if depth == u64::MAX {
                return 0;
            }
            down(job, depth + 1) + pad[0]
        }
        let stop = frame::task(Tag::Diagnostics, |frame| {
            let limits = Limits { fuel: u64::MAX, stack: 64 << 10, ..Limits::DEFAULT };
            let job = Job::new(&HostKey::testing(), frame, limits, &NoFacts, 0);
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _entered = job.enter();
                down(&job, 0)
            }))
            .err()
            .map(|payload| *payload.downcast::<Stop>().expect("a stop"))
        });
        assert_eq!(stop, Some(Stop::Stack { cap: 64 << 10 }));
    }
}
