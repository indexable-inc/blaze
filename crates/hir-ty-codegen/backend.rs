//! Which code producer turns the IR of a function into machine code, and what each one did.
//!
//! A producer takes the Cranelift IR the front half builds from MIR, after layout and the
//! calling convention have been decided, and gives back a [`Code`]: bytes, the relocations
//! in them, and how to unwind through them. Nothing after that can tell the producers apart.
//! A function LLVM has no lowering for stops the build ([`CodegenError::LlvmRefused`]):
//! Cranelift never compiles it in LLVM's place.

use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use parking_lot::Mutex;
use stdx::time::Instant;

use super::{CodegenError, Result, debuginfo::SlotPlaces, fragment::Code, fragment::Recorder};

/// Names the producer: `cranelift` (the default) or `llvm`.
const BACKEND: &str = "BLAZE_BACKEND";
/// How hard LLVM optimizes each function, `0` to `3`. Each function is compiled alone, with
/// what it calls built into it at `-O2` and above only in a build of a whole program (see
/// `tier::across`).
const OPTIMIZATION: &str = "BLAZE_OPT";
/// How LLVM turns the optimized function into machine code: `optimized` (the default) or
/// `fast`, its quick instruction selection and register allocation. Most of LLVM's time on a
/// function is here, not in the passes before it.
const SELECTION: &str = "BLAZE_EMIT";

/// The most instructions of Cranelift IR a body has that LLVM compiles in a build. LLVM takes
/// 5 to 7 µs for each at `-O2` (16 Zen 5 cores, nushell 0.108.0, 2026-10-20), Cranelift about
/// 1, and an edit waits for the body edited: nushell's `main`, 35 778 instructions, held its
/// edit up by 229 ms in LLVM, against 28 in Cranelift. A larger body is Cranelift's. A hot one
/// is still compiled by LLVM as a tier, which does not come through here.
#[cfg(feature = "llvm")]
const LLVM_LIMIT: usize = 10_000;

/// Whether the build under way is of a whole program: one rustc built nothing of, whatever
/// its `main` is (blaze's, or C's as bun's), in a process no edit follows. Its object is
/// linked once and runs as it is, never edited nor compiled again as a tier, so LLVM is given
/// every body, as large and with what handlers it has (see [`Backend::emit`]).
static WHOLE_PROGRAM: AtomicBool = AtomicBool::new(false);

/// Whether the process goes on to build edits of what it builds: a daemon's worker, or one
/// that watches its files. What it compiles is kept by each function's own code alone (the
/// store, and the build an edit is a patch of), so none of its functions may have what it
/// calls in another crate built into it (see `tier::across`).
static BUILDS_EDITS: AtomicBool = AtomicBool::new(false);

/// Says the process goes on to build edits of what it builds. Before its first build.
pub fn builds_edits() {
    BUILDS_EDITS.store(true, Ordering::Relaxed);
}

pub(super) fn edits_follow() -> bool {
    BUILDS_EDITS.load(Ordering::Relaxed)
}

/// Whether the process links its programs into executables of their own, which no edit is
/// spliced into: a whole build's daemon worker (docs/no-rustc.md, step K) relinks each edit.
static LINKS_OWN: AtomicBool = AtomicBool::new(false);

/// Says the process links what it builds into executables of its own. Before its first build.
pub fn links_own() {
    LINKS_OWN.store(true, Ordering::Relaxed);
}

/// Whether each function and static of an object is in a section of its own, for the linker
/// to leave out what nothing reaches. The links a daemon splices edits into keep everything.
pub fn sections_of_their_own() -> bool {
    !edits_follow() || LINKS_OWN.load(Ordering::Relaxed)
}

/// Whether every function of the build under way keeps its frame pointer, as the program asks
/// with `-C force-frame-pointers`: one answer for all the code a build writes, as the debug
/// level is.
static FRAME_POINTERS: AtomicBool = AtomicBool::new(false);

pub(super) fn keep_frame_pointers(keep: bool) {
    FRAME_POINTERS.store(keep, Ordering::Relaxed);
}

pub(super) fn frame_pointers() -> bool {
    FRAME_POINTERS.load(Ordering::Relaxed)
}

/// Whether rustc's build of the program has `-C link-dead-code`
/// ([`rustc_links_dead_code`](super::rustc_links_dead_code)).
static LINKS_DEAD_CODE: AtomicBool = AtomicBool::new(false);

pub(super) fn links_dead_code() -> bool {
    LINKS_DEAD_CODE.load(Ordering::Relaxed)
}

pub(super) fn whole_program() -> bool {
    WHOLE_PROGRAM.load(Ordering::Relaxed)
}

/// Whether compiles go to LLVM above `-O0`, where rustc generates code from MIR of its level 2.
pub(super) fn above_o0() -> bool {
    matches!(Backend::from_env(), Ok(Backend::Llvm(it)) if it.passes != Passes::None)
}

/// Whether compiles go to LLVM at `-O2` or above.
pub(super) fn optimizing() -> bool {
    matches!(
        Backend::from_env(),
        Ok(Backend::Llvm(Optimization { passes: Passes::Default | Passes::Aggressive, .. }))
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Cranelift,
    Llvm(Optimization),
}

/// What LLVM does to a function: the passes over its IR, and how code is selected for what
/// they leave.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Optimization {
    pub passes: Passes,
    pub selection: Selection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Passes {
    None,
    Less,
    Default,
    Aggressive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Selection {
    Fast,
    Optimized,
}

impl Optimization {
    /// No passes and the quick selection: what a function too large for more is given.
    pub const NONE: Optimization =
        Optimization { passes: Passes::None, selection: Selection::Fast };
}

impl Backend {
    /// The producer the environment asks for, or what is wrong with what it asks.
    pub fn from_env() -> std::result::Result<Backend, String> {
        static SELECTED: OnceLock<std::result::Result<Backend, String>> = OnceLock::new();
        match SELECTED.get_or_init(Self::read_env) {
            Ok(backend) => Ok(*backend),
            // clone: the error's message
            Err(why) => Err(why.clone()),
        }
    }

    fn read_env() -> std::result::Result<Backend, String> {
        let passes = match std::env::var(OPTIMIZATION).as_deref() {
            Ok("0") => Passes::None,
            Ok("1") => Passes::Less,
            Ok("2") | Err(_) => Passes::Default,
            Ok("3") => Passes::Aggressive,
            // alloc: the error's message
            Ok(other) => return Err(format!("BLAZE_OPT={other}: expected 0, 1, 2 or 3")),
        };
        let selection = match std::env::var(SELECTION).as_deref() {
            Ok("fast") => Selection::Fast,
            Ok("optimized") | Err(_) if passes != Passes::None => Selection::Optimized,
            Ok("optimized") | Err(_) => Selection::Fast,
            Ok(other) => {
                // alloc: the error's message
                return Err(format!("BLAZE_EMIT={other}: expected `fast` or `optimized`"));
            }
        };
        let optimization = Optimization { passes, selection };
        match std::env::var(BACKEND).as_deref() {
            Ok("cranelift") | Err(_) => Ok(Backend::Cranelift),
            Ok("llvm") if cfg!(feature = "llvm") => Ok(Backend::Llvm(optimization)),
            Ok("llvm") => {
                // alloc: the error's message
                Err(format!("BLAZE_BACKEND=llvm: blaze was built without `--features llvm`"))
            }
            // alloc: the error's message
            Ok(other) => Err(format!("BLAZE_BACKEND={other}: expected `cranelift` or `llvm`")),
        }
    }

    /// Compiles the function `context` holds. `recorder` is where a producer asks for a
    /// function its own code calls that the IR did not name.
    pub(super) fn emit(
        recorder: &mut Recorder<'_>,
        context: &mut cranelift_codegen::Context,
    ) -> Result<Code> {
        pad(Self::produce(recorder, context)?)
    }

    fn produce(
        recorder: &mut Recorder<'_>,
        context: &mut cranelift_codegen::Context,
    ) -> Result<Code> {
        let tally = Tally::global();
        match Self::from_env().map_err(CodegenError::Backend)? {
            Backend::Cranelift => {}
            // Which bodies an edit's build gives LLVM is decided by the body alone, before
            // either producer runs, never by what LLVM failed at: the same body always goes
            // to the same producer.
            #[cfg(feature = "llvm")]
            Backend::Llvm(_) if !whole_program() && context.func.dfg.num_insts() > LLVM_LIMIT => {}
            // A handler of several, or one that takes more than the exception, costs LLVM a
            // landing pad for each call: a generic edit of nushell waited on 177 such bodies,
            // 17 ms each in LLVM. A tier, which does not come through here, lowers them, as
            // does the build of a whole program, which nothing edits.
            #[cfg(feature = "llvm")]
            Backend::Llvm(_) if !whole_program() && !super::llvm::plain_handlers(&context.func) => {}
            #[cfg(feature = "llvm")]
            Backend::Llvm(optimization) => {
                let start = Instant::now();
                let code = Self::emit_with_llvm(recorder, &context.func, optimization)
                    .map_err(CodegenError::LlvmRefused)?;
                tally.llvm.add(start.elapsed(), code.bytes.len());
                return Ok(code);
            }
            #[cfg(not(feature = "llvm"))]
            Backend::Llvm(_) => {}
        }
        let _span = tracing::info_span!(target: "blaze", "cranelift").entered();
        let start = Instant::now();
        if let Some(frame) = recorder.frame_locals.take() {
            super::overlap::share(&mut context.func, &frame, &mut recorder.named);
        }
        let pairs = super::float_pair::lower(&mut context.func);
        let mut code = super::emit(context)?;
        if let Some(frame) = context.compiled_code().and_then(|it| it.buffer.frame_layout()) {
            code.slots =
                SlotPlaces::of_frame(recorder.isa, frame, &context.func, recorder.named_slots());
        }
        if let Some(pairs) = pairs {
            pairs.hold(context, &mut code)?;
        }
        tally.cranelift.add(start.elapsed(), code.bytes.len());
        Ok(code)
    }
}

/// A function is replaced in a linked program by a jump written over its entry, so none is
/// shorter than that jump. Nothing runs the bytes that make up the length.
pub(super) fn pad(mut code: Code) -> Result<Code> {
    let target = super::Target::selected().map_err(CodegenError::Backend)?;
    if let Some((shortest, filler)) = target.arch.shortest_function()
        && code.bytes.len() < shortest
    {
        code.bytes.resize(shortest, filler);
    }
    Ok(code)
}

#[cfg(feature = "llvm")]
impl Backend {
    /// What LLVM makes of the function.
    fn emit_with_llvm(
        recorder: &mut Recorder<'_>,
        func: &cranelift_codegen::ir::Function,
        optimization: Optimization,
    ) -> std::result::Result<Code, super::llvm::Unhandled> {
        let mut named: smallvec::SmallVec<[cranelift_codegen::ir::StackSlot; 16]> =
            smallvec::SmallVec::from_iter(recorder.named_slots());
        let shared = recorder
            .frame_locals
            .as_ref()
            .map(|frame| super::overlap::share_escaping(func, frame, &mut named));
        let func = shared.as_ref().unwrap_or(func);
        let imports = match super::imports::enabled() {
            true => super::imports::chosen(recorder),
            // alloc: empty, allocates nothing
            false => Vec::new(),
        };
        if !imports.is_empty() {
            let mut code =
                super::llvm::emit(&mut recorder.wants, &named, func, optimization, None, &imports)?;
            recorder.keep_referenced(&mut code);
            return Ok(code);
        }
        super::llvm::emit(&mut recorder.wants, &named, func, optimization, None, &[])
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Backend::Cranelift => f.write_str("cranelift"),
            Backend::Llvm(Optimization { passes, selection }) => {
                let level = match passes {
                    Passes::None => 0,
                    Passes::Less => 1,
                    Passes::Default => 2,
                    Passes::Aggressive => 3,
                };
                write!(f, "llvm -O{level}")?;
                match selection {
                    Selection::Fast if *passes != Passes::None => f.write_str(" fast emit"),
                    _ => Ok(()),
                }
            }
        }
    }
}

/// What the producers did since the last report. Every compile on every core counts into it,
/// so no count is a shared word: each thread adds to [`Counts`] of its own, and [`report`]
/// sums them, as the Linux kernel's per-CPU counters do. A field names which word of a
/// thread's counts it is.
///
/// [`report`]: Tally::report
pub(super) struct Tally {
    cranelift: Produced,
    pub(super) llvm: Produced,
    /// The units LLVM compiled ([`super::units`]), the functions of them and the imports
    /// they kept as the code of the functions those are of. Why a function of one was
    /// compiled alone instead, and how often, is [`Counts::alone`].
    units: Count,
    unit_members: Count,
    pub(super) unit_kept: Count,
    /// Functions too large for LLVM to optimize in reasonable time, compiled at `-O0`.
    pub(super) large: Count,
    /// The exports built for compiles to import ([`super::imports`]), and how many bodies
    /// and instructions those imported.
    pub(super) exports: Produced,
    pub(super) imported: Count,
    pub(super) imported_size: Count,
}

/// The words of [`Counts::words`]: three for each [`Produced`], one for each [`Count`], then
/// LLVM's phases and the steps around the producer, in nanoseconds.
mod word {
    pub(super) const CRANELIFT: usize = 0;
    pub(super) const LLVM: usize = 3;
    pub(super) const EXPORTS: usize = 6;
    pub(super) const UNITS: usize = 9;
    pub(super) const UNIT_MEMBERS: usize = 10;
    pub(super) const UNIT_KEPT: usize = 11;
    pub(super) const LARGE: usize = 12;
    pub(super) const IMPORTED: usize = 13;
    pub(super) const IMPORTED_SIZE: usize = 14;
    pub(super) const PHASES: usize = 15;
    pub(super) const STEPS: usize = PHASES + super::Phase::COUNT;
    pub(super) const COUNT: usize = STEPS + super::Step::COUNT;
}

/// A count of [`Tally`]: which of a thread's [`Counts::words`] it is.
pub(super) struct Count(usize);

#[cfg(feature = "llvm")]
impl Count {
    /// Adds `n` to the calling thread's count. Takes the ordering its callers gave the
    /// shared atomic it replaced; a thread's own word needs none.
    pub(super) fn fetch_add(&self, n: usize, _: Ordering) {
        counts().add(self.0, n as u64);
    }
}

/// Functions compiled, and the nanoseconds and bytes of code they took: three words from the
/// one named.
pub(super) struct Produced(usize);

/// One thread's counts, written by that thread alone with a plain load and store (no locked
/// instruction, no line another core writes), read by [`Tally::report`]. 2 × 256 bytes of
/// words, each block on lines of its own, so a report's writes never touch the line a worker
/// counts into. A thread that ends hands its counts on to the next thread that starts
/// ([`Registry::claim`]): nothing counted is lost, and there are never more of them than
/// the most threads alive at once.
#[repr(C, align(128))]
struct Counts {
    words: Lines<[AtomicU64; word::COUNT]>,
    /// What a report read of each word last, written by reports alone.
    reported: Lines<[AtomicU64; word::COUNT]>,
    /// Why a function of a unit was compiled alone, and how often.
    alone: Mutex<BTreeMap<String, usize>>,
    /// Where Cranelift's time went on this thread, with `BLAZE_STATS`.
    passes: Mutex<Option<cranelift_codegen::timing::PassTimes>>,
    /// Whether a thread holds these counts. The counts of [`Registry::late`] are held by
    /// none and shared by all: their words are added to with `fetch_add`.
    held: AtomicBool,
    next: OnceLock<&'static Counts>,
}

#[repr(C, align(128))]
struct Lines<T>(T);

const _: () = assert!(size_of::<Lines<[AtomicU64; word::COUNT]>>() == 256);

impl Counts {
    const fn new(held: bool) -> Counts {
        Counts {
            words: Lines([const { AtomicU64::new(0) }; word::COUNT]),
            reported: Lines([const { AtomicU64::new(0) }; word::COUNT]),
            alone: Mutex::new(BTreeMap::new()),
            passes: Mutex::new(None),
            held: AtomicBool::new(held),
            next: OnceLock::new(),
        }
    }

    fn add(&self, word: usize, n: u64) {
        let word = &self.words.0[word];
        if self.held.load(Ordering::Relaxed) {
            word.store(word.load(Ordering::Relaxed).wrapping_add(n), Ordering::Relaxed);
        } else {
            word.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// What each word grew by since a report last read it. Reports at once each get a part,
    /// and the parts add up to the growth once.
    fn take(&self, sums: &mut [u64; word::COUNT]) {
        for ((sum, word), reported) in sums.iter_mut().zip(&self.words.0).zip(&self.reported.0) {
            let now = word.load(Ordering::Relaxed);
            let before = reported.fetch_max(now, Ordering::Relaxed);
            *sum += now.saturating_sub(before);
        }
    }

    fn release(&self) {
        self.held.store(false, Ordering::Release);
    }
}

/// Every thread's [`Counts`]: a list that only grows, of counts that are never freed, so a
/// report reads them with no lock while threads count. A link is set once: reading one is an
/// acquire load, and a thread appends at the end (once, as it starts).
struct Registry {
    head: OnceLock<&'static Counts>,
    /// Where a thread counts once its own counts are gone, as it ends.
    late: Counts,
}

static REGISTRY: Registry =
    Registry { head: OnceLock::new(), late: Counts::new(false) };

impl Registry {
    /// Counts no thread holds, now held by the calling one: those a thread that ended left,
    /// or new ones.
    fn claim(&'static self) -> &'static Counts {
        for counts in self.listed() {
            if !counts.held.load(Ordering::Relaxed)
                && counts
                    .held
                    .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
            {
                return counts;
            }
        }
        // alloc: once per thread, counts that are never freed
        let counts: &'static Counts = Box::leak(Box::new(Counts::new(true)));
        let mut link = &self.head;
        loop {
            match link.set(counts) {
                Ok(()) => return counts,
                Err(_) => link = &link.get().expect("a link that refused a set is set").next,
            }
        }
    }

    /// The counts threads held, held now or not.
    fn listed(&'static self) -> impl Iterator<Item = &'static Counts> {
        let mut link = &self.head;
        std::iter::from_fn(move || {
            let counts = *link.get()?;
            link = &counts.next;
            Some(counts)
        })
    }

    /// The counts of every thread that counted, after [`Registry::late`].
    fn all(&'static self) -> impl Iterator<Item = &'static Counts> {
        std::iter::once(&self.late).chain(self.listed())
    }

    /// What every word grew by since the last report, and the reasons counted since.
    fn take(&'static self) -> Taken {
        let mut taken = Taken {
            sums: [0; word::COUNT],
            alone: BTreeMap::new(),
            passes: None,
        };
        for counts in self.all() {
            counts.take(&mut taken.sums);
            let alone = std::mem::take(&mut *counts.alone.lock());
            for (reason, count) in alone {
                *taken.alone.entry(reason).or_default() += count;
            }
            if let Some(times) = counts.passes.lock().take() {
                taken.passes.get_or_insert_with(Default::default).add(&times);
            }
        }
        taken
    }
}

struct Taken {
    sums: [u64; word::COUNT],
    alone: BTreeMap<String, usize>,
    passes: Option<cranelift_codegen::timing::PassTimes>,
}

/// The calling thread's counts, given back for the next thread when it ends.
struct Held(&'static Counts);

impl Held {
    fn claim() -> Held {
        // Cranelift times every pass of every function it compiles, with two reads of the
        // clock and a token for each, for the report `BLAZE_STATS` asks for alone.
        if !stats() {
            // alloc: of no size, allocates nothing; once per thread
            drop(cranelift_codegen::timing::set_thread_profiler(Box::new(Untimed)));
        }
        Held(REGISTRY.claim())
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.0.release();
    }
}

thread_local! {
    static HELD: Held = Held::claim();
}

fn counts() -> &'static Counts {
    HELD.try_with(|held| held.0).unwrap_or(&REGISTRY.late)
}

/// A profiler of Cranelift's that reads no clock: its token is of no size, so it is not
/// allocated either.
struct Untimed;

impl cranelift_codegen::timing::Profiler for Untimed {
    // dyn: cranelift's `Profiler::start_pass` returns `Box<dyn Any>`; a `()` is no allocation
    fn start_pass(&self, _: cranelift_codegen::timing::Pass) -> Box<dyn std::any::Any> {
        // alloc: a `()` is of no size, which allocates nothing
        Box::new(())
    }
}

/// Whether `BLAZE_STATS` asks for the steps and Cranelift's passes to be reported.
fn stats() -> bool {
    static STATS: OnceLock<bool> = OnceLock::new();
    *STATS.get_or_init(|| std::env::var_os("BLAZE_STATS").is_some())
}

/// The steps of compiling one function with LLVM.
#[derive(Debug, Clone, Copy)]
pub(super) enum Phase {
    Lower,
    Verify,
    Optimize,
    Emit,
    Read,
    /// Building an export for compiles to import, all its steps.
    Export,
}

impl Phase {
    const COUNT: usize = 6;
    const ALL: [Phase; Phase::COUNT] =
        [Phase::Lower, Phase::Verify, Phase::Optimize, Phase::Emit, Phase::Read, Phase::Export];
}

/// The steps of compiling one function that are not the producer's.
#[derive(Debug, Clone, Copy)]
pub(super) enum Step {
    /// The types of a body.
    Infer,
    /// The MIR of a body, without the time its types took when they were asked for first.
    Mir,
    /// A generic body with the types of one instance in it.
    Monomorphize,
    /// From MIR to the IR the producer takes.
    Translate,
    /// Lines and variables for a debugger.
    Describe,
}

impl Step {
    const COUNT: usize = 5;
    const ALL: [(Step, &str); Step::COUNT] = [
        (Step::Infer, "infer"),
        (Step::Mir, "mir"),
        (Step::Monomorphize, "monomorphize"),
        (Step::Translate, "translate"),
        (Step::Describe, "describe"),
    ];
}

impl Produced {
    pub(super) fn add(&self, took: Duration, bytes: usize) {
        let counts = counts();
        counts.add(self.0, 1);
        counts.add(self.0 + 1, took.as_nanos() as u64);
        counts.add(self.0 + 2, bytes as u64);
    }

    fn take(&self, sums: &[u64; word::COUNT]) -> Option<String> {
        let functions = sums[self.0];
        let took = Duration::from_nanos(sums[self.0 + 1]);
        let bytes = sums[self.0 + 2];
        (functions != 0).then(|| {
            let each = took / functions as u32;
            // alloc: a line of the report, once per build
            format!("{functions} functions, {bytes} bytes, {took:.1?} ({each:.1?} each)")
        })
    }
}

static TALLY: Tally = Tally {
    cranelift: Produced(word::CRANELIFT),
    llvm: Produced(word::LLVM),
    units: Count(word::UNITS),
    unit_members: Count(word::UNIT_MEMBERS),
    unit_kept: Count(word::UNIT_KEPT),
    large: Count(word::LARGE),
    exports: Produced(word::EXPORTS),
    imported: Count(word::IMPORTED),
    imported_size: Count(word::IMPORTED_SIZE),
};

impl Tally {
    pub(super) fn global() -> &'static Tally {
        &TALLY
    }

    #[cfg(feature = "llvm")]
    pub(super) fn unit(&self, members: usize) {
        let counts = counts();
        counts.add(self.units.0, 1);
        counts.add(self.unit_members.0, members as u64);
    }

    #[cfg(feature = "llvm")]
    pub(super) fn alone(&self, reason: &str, functions: usize) {
        let counts = counts();
        let mut alone = counts.alone.lock();
        match alone.get_mut(reason) {
            Some(count) => *count += functions,
            // alloc: once per distinct reason, the first time it is counted
            None => _ = alone.insert(reason.to_owned(), functions),
        }
    }

    #[cfg(feature = "llvm")]
    pub(super) fn time<T>(&self, phase: Phase, work: impl FnOnce() -> T) -> T {
        let start = Instant::now();
        let out = work();
        counts().add(word::PHASES + phase as usize, start.elapsed().as_nanos() as u64);
        out
    }

    /// Adds the times of Cranelift's passes on this thread since it was last asked, with
    /// `BLAZE_STATS`. Without it, the thread's Cranelift reads no clock from its next
    /// function on ([`Held::claim`]).
    pub(super) fn passes(&self) {
        let counts = counts();
        if !stats() {
            return;
        }
        let times = cranelift_codegen::timing::take_current();
        let mut passes = counts.passes.lock();
        passes.get_or_insert_with(Default::default).add(&times);
    }

    /// Runs `work` as part of `step`.
    pub(super) fn step<T>(&self, step: Step, work: impl FnOnce() -> T) -> T {
        // For blaze's trace: target `blaze` spans of 200 µs and longer.
        let _span = match step {
            Step::Infer => tracing::info_span!(target: "blaze", "infer"),
            Step::Mir => tracing::info_span!(target: "blaze", "mir"),
            Step::Monomorphize => tracing::info_span!(target: "blaze", "monomorphize"),
            Step::Translate => tracing::info_span!(target: "blaze", "translate"),
            Step::Describe => tracing::info_span!(target: "blaze", "describe"),
        }
        .entered();
        let start = Instant::now();
        let out = work();
        self.add(step, start.elapsed());
        out
    }

    pub(super) fn add(&self, step: Step, took: Duration) {
        counts().add(word::STEPS + step as usize, took.as_nanos() as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> &'static Registry {
        Box::leak(Box::new(Registry { head: OnceLock::new(), late: Counts::new(false) }))
    }

    #[test]
    fn every_count_is_a_word_of_its_own() {
        let tally = Tally::global();
        let mut words: Vec<usize> = [&tally.cranelift, &tally.llvm, &tally.exports]
            .into_iter()
            .flat_map(|it| it.0..it.0 + 3)
            .collect();
        words.extend(
            [
                &tally.units,
                &tally.unit_members,
                &tally.unit_kept,
                &tally.large,
                &tally.imported,
                &tally.imported_size,
            ]
            .map(|it| it.0),
        );
        words.extend(word::PHASES..word::COUNT);
        words.sort_unstable();
        assert_eq!(words, (0..word::COUNT).collect::<Vec<_>>());
    }

    #[test]
    fn the_counts_of_every_thread_add_up_once() {
        let registry = registry();
        std::thread::scope(|scope| {
            for thread in 0..8u64 {
                scope.spawn(move || {
                    let counts = registry.claim();
                    for _ in 0..1000 {
                        counts.add(word::LARGE, thread + 1);
                    }
                    counts.add(word::STEPS, 1);
                    counts.release();
                });
            }
        });
        registry.late.add(word::LARGE, 5);
        let taken = registry.take();
        assert_eq!(taken.sums[word::LARGE], 1000 * (1..=8).sum::<u64>() + 5);
        assert_eq!(taken.sums[word::STEPS], 8);
        assert_eq!(registry.take().sums, [0; word::COUNT]);
    }

    #[test]
    fn a_thread_that_ends_hands_its_counts_to_the_next() {
        let registry = registry();
        let run = move |n| {
            std::thread::spawn(move || {
                let counts = registry.claim();
                counts.add(word::UNITS, n);
                counts.release();
                std::ptr::from_ref(counts) as usize
            })
            .join()
            .expect("the thread")
        };
        assert_eq!(run(3), run(4));
        assert_eq!(registry.listed().count(), 1);
        assert_eq!(registry.take().sums[word::UNITS], 7);
    }

    #[test]
    fn reports_while_threads_count_see_each_count_once() {
        let registry = registry();
        let reported = std::sync::atomic::AtomicU64::new(0);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    let counts = registry.claim();
                    for _ in 0..10_000 {
                        counts.add(word::LARGE, 1);
                    }
                    counts.release();
                });
            }
            for _ in 0..3 {
                scope.spawn(|| {
                    for _ in 0..200 {
                        let taken = registry.take().sums[word::LARGE];
                        reported.fetch_add(taken, Ordering::Relaxed);
                    }
                });
            }
        });
        let rest = registry.take().sums[word::LARGE];
        assert_eq!(reported.load(Ordering::Relaxed) + rest, 40_000);
    }

    #[test]
    fn reasons_of_every_thread_are_merged() {
        let registry = registry();
        for (reason, n) in [("a", 1), ("b", 2), ("a", 3)] {
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    let counts = registry.claim();
                    *counts.alone.lock().entry(reason.to_owned()).or_default() += n;
                    counts.release();
                });
            });
        }
        let taken = registry.take();
        assert_eq!(taken.alone, BTreeMap::from([("a".to_owned(), 4), ("b".to_owned(), 2)]));
        assert!(registry.take().alone.is_empty());
    }
}
