//! Which code producer turns the IR of a function into machine code, and what each one did.
//!
//! A producer takes the Cranelift IR the front half builds from MIR, after layout and the
//! calling convention have been decided, and gives back a [`Code`]: bytes, the relocations
//! in them, and how to unwind through them. Nothing after that can tell the producers apart.
//! LLVM hands a function it has no lowering for to Cranelift, and says why.

use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use stdx::time::Instant;

use super::{CodegenError, Result, debuginfo::SlotPlaces, fragment::Code, fragment::Recorder};

/// Names the producer: `cranelift` (the default) or `llvm`.
const BACKEND: &str = "RA_JIT_BACKEND";
/// How hard LLVM optimizes each function, `0` to `3`. Each function is compiled alone, with
/// what it calls built into it at `-O2` and above only in a build of a whole program (see
/// `tier::across`).
const OPTIMIZATION: &str = "RA_JIT_OPT";
/// How LLVM turns the optimized function into machine code: `optimized` (the default) or
/// `fast`, its quick instruction selection and register allocation. Most of LLVM's time on a
/// function is here, not in the passes before it.
const SELECTION: &str = "RA_JIT_EMIT";

/// The most instructions of Cranelift IR a body has that LLVM compiles in a build. LLVM takes
/// 5 to 7 µs for each at `-O2` (16 Zen 5 cores, nushell 0.108.0, 2026-10-20), Cranelift about
/// 1, and an edit waits for the body edited: nushell's `main`, 35 778 instructions, held its
/// edit up by 229 ms in LLVM, against 28 in Cranelift. A larger body is Cranelift's. A hot one
/// is still compiled by LLVM as a tier, which does not come through here.
#[cfg(feature = "llvm")]
const LLVM_LIMIT: usize = 10_000;

/// Whether the build under way is of a whole program: one rustc built nothing of, whatever
/// its `main` is (ra-jit's, or C's as bun's), in a process no edit follows. Its object is
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

/// Whether the build under way tells a debugger where its code comes from: one answer for
/// all the code a build writes, as the frame pointers are.
static DESCRIBED: AtomicBool = AtomicBool::new(true);

pub(super) fn describe(described: bool) {
    DESCRIBED.store(described, Ordering::Relaxed);
}

/// Whether LLVM is to say what source each instruction is of. A build that describes nothing
/// reads that only to describe it, and a whole program is not compiled again for a build that
/// does: LLVM is told nothing of the source there, which took 4% off its time on helix
/// (2026-10-24).
pub(super) fn marks_lines() -> bool {
    lines_marked(DESCRIBED.load(Ordering::Relaxed), whole_program())
}

fn lines_marked(described: bool, whole_program: bool) -> bool {
    described || !whole_program
}

/// Says the build under way is of a whole program.
pub(super) fn set_whole_program() {
    WHOLE_PROGRAM.store(true, Ordering::Relaxed);
}

pub(super) fn whole_program() -> bool {
    WHOLE_PROGRAM.load(Ordering::Relaxed)
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
        SELECTED.get_or_init(Self::read_env).clone()
    }

    fn read_env() -> std::result::Result<Backend, String> {
        let passes = match std::env::var(OPTIMIZATION).as_deref() {
            Ok("0") => Passes::None,
            Ok("1") => Passes::Less,
            Ok("2") | Err(_) => Passes::Default,
            Ok("3") => Passes::Aggressive,
            Ok(other) => return Err(format!("{OPTIMIZATION}={other}: expected 0, 1, 2 or 3")),
        };
        let selection = match std::env::var(SELECTION).as_deref() {
            Ok("fast") => Selection::Fast,
            Ok("optimized") | Err(_) if passes != Passes::None => Selection::Optimized,
            Ok("optimized") | Err(_) => Selection::Fast,
            Ok(other) => {
                return Err(format!("{SELECTION}={other}: expected `fast` or `optimized`"));
            }
        };
        let optimization = Optimization { passes, selection };
        match std::env::var(BACKEND).as_deref() {
            Ok("cranelift") | Err(_) => Ok(Backend::Cranelift),
            Ok("llvm") if cfg!(feature = "llvm") => Ok(Backend::Llvm(optimization)),
            Ok("llvm") => {
                Err(format!("{BACKEND}=llvm: ra-jit was built without `--features llvm`"))
            }
            Ok(other) => Err(format!("{BACKEND}={other}: expected `cranelift` or `llvm`")),
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
            #[cfg(feature = "llvm")]
            Backend::Llvm(_) if !whole_program() && context.func.dfg.num_insts() > LLVM_LIMIT => {
                tally.fell_back(Duration::ZERO, "a body larger than LLVM is given in a build");
            }
            // A handler of several, or one that takes more than the exception, costs LLVM a
            // landing pad for each call: a generic edit of nushell waited on 177 such bodies,
            // 17 ms each in LLVM. A tier, which does not come through here, lowers them, as
            // does the build of a whole program, which nothing edits.
            #[cfg(feature = "llvm")]
            Backend::Llvm(_) if !whole_program() && !super::llvm::plain_handlers(&context.func) => {
                tally.fell_back(Duration::ZERO, "a handler LLVM lowers only for a tier");
            }
            #[cfg(feature = "llvm")]
            Backend::Llvm(optimization) => {
                let start = Instant::now();
                match Self::emit_with_llvm(recorder, &context.func, optimization) {
                    Ok(code) => {
                        tally.llvm.add(start.elapsed(), code.bytes.len());
                        return Ok(code);
                    }
                    Err(unhandled) => tally.fell_back(start.elapsed(), &unhandled.to_string()),
                }
            }
            #[cfg(not(feature = "llvm"))]
            Backend::Llvm(_) => {}
        }
        let _span = tracing::info_span!(target: "ra_jit", "cranelift").entered();
        let start = Instant::now();
        let pairs = super::float_pair::lower(recorder, &mut context.func);
        let mut code = super::emit(context)?;
        if let Some(frame) = context.compiled_code().and_then(|it| it.buffer.frame_layout()) {
            code.slots = SlotPlaces::of_frame(recorder.isa, frame, recorder.named_slots());
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
    /// What LLVM makes of the function, from the store if the same IR was compiled at the
    /// same level before. That LLVM left the function to Cranelift is kept as well: else
    /// every build would try LLVM on it again, miss in the store and count the function as
    /// compiled, though Cranelift's code of it then comes from the store.
    fn emit_with_llvm(
        recorder: &mut Recorder<'_>,
        func: &cranelift_codegen::ir::Function,
        optimization: Optimization,
    ) -> std::result::Result<Code, super::llvm::Unhandled> {
        use super::{
            cached::{LlvmEntry, StoredCode},
            llvm::Unhandled,
            store::Store,
        };
        let named: Vec<_> = recorder.named_slots().collect();
        let imports = match super::imports::enabled() {
            true => super::imports::chosen(recorder),
            false => Vec::new(),
        };
        // Not stored: the code is of the bodies imported too.
        if !imports.is_empty() {
            let mut code =
                super::llvm::emit(&mut recorder.wants, &named, func, optimization, None, &imports)?;
            recorder.keep_referenced(&mut code);
            return Ok(code);
        }
        let recorder = &mut recorder.wants;
        // The key is of the function's own IR, which the bodies LLVM is shown are not part of.
        let store = Store::global().filter(|_| recorder.seen.is_empty());
        let Some((store, key)) = store.zip(StoredCode::key(
            optimization,
            func,
            named.iter().copied(),
            recorder.constants(),
            &recorder.pointees,
        )) else {
            return super::llvm::emit(recorder, &named, func, optimization, None, &[]);
        };
        match store.lookup(&key).and_then(|it| LlvmEntry::decode(&it)) {
            Some(LlvmEntry::Code(stored)) => {
                if let Some(code) = stored.restore(recorder) {
                    return Ok(code);
                }
            }
            Some(LlvmEntry::LeftToCranelift(reason)) => return Err(Unhandled::Kept(reason)),
            None => {}
        }
        let mark = recorder.mark();
        let (entry, out) = match super::llvm::emit(recorder, &named, func, optimization, None, &[])
        {
            Ok(code) => (StoredCode::of(&code, recorder, mark).map(LlvmEntry::Code), Ok(code)),
            Err(unhandled) => {
                (Some(LlvmEntry::LeftToCranelift(unhandled.to_string())), Err(unhandled))
            }
        };
        if let Some(bytes) = entry.and_then(|it| it.encode()) {
            store.insert(&key, &bytes);
        }
        out
    }
}

impl fmt::Display for Backend {
    /// Also what tells the entries of one producer from another's in the store.
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

/// What the producers did since the last report.
#[derive(Default)]
pub(super) struct Tally {
    cranelift: Produced,
    pub(super) llvm: Produced,
    /// The units LLVM compiled ([`super::units`]), the functions of them and the imports
    /// they kept as the code of the functions those are of, and why a function of one was
    /// compiled alone instead, and how often.
    units: AtomicUsize,
    unit_members: AtomicUsize,
    pub(super) unit_kept: AtomicUsize,
    alone: Mutex<BTreeMap<String, usize>>,
    /// Time LLVM spent on functions it then gave up on.
    wasted: AtomicU64,
    /// Why LLVM left a function to Cranelift, and how often.
    fallbacks: Mutex<BTreeMap<String, usize>>,
    /// Functions too large for LLVM to optimize in reasonable time, compiled at `-O0`.
    pub(super) large: AtomicUsize,
    /// Where LLVM's time went.
    pub(super) phases: [AtomicU64; Phase::COUNT],
    /// Where the time around the producer went.
    steps: [AtomicU64; Step::COUNT],
    /// Where Cranelift's time went, summed over the threads, with `RA_JIT_STATS`.
    passes: Mutex<Option<cranelift_codegen::timing::PassTimes>>,
    /// The exports built for compiles to import ([`super::imports`]), and how many bodies
    /// and instructions those imported.
    pub(super) exports: Produced,
    /// The builds of exports that gave none: a body too large, or one LLVM has no lowering
    /// for.
    pub(super) exports_refused: Produced,
    /// How many of the exports a compile imported.
    pub(super) exports_imported: AtomicUsize,
    /// The imports LLVM left a call of in some function, and their instructions.
    pub(super) imports_called: AtomicUsize,
    pub(super) imports_called_size: AtomicUsize,
    pub(super) imported: AtomicUsize,
    pub(super) imported_size: AtomicUsize,
}

#[derive(Default)]
pub(super) struct Produced {
    functions: AtomicUsize,
    nanos: AtomicU64,
    bytes: AtomicUsize,
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
    /// Of `Lower`: linking the imports in.
    Link,
    /// Of `Optimize`: the passes of the function before its imports are linked in.
    Own,
    /// Of `Optimize`: the first passes after the imports are linked in.
    Light,
    /// Of `Link`: reading the imports and naming what they refer to.
    Parse,
}

impl Phase {
    const COUNT: usize = 10;
    const ALL: [Phase; Phase::COUNT] = [
        Phase::Lower,
        Phase::Verify,
        Phase::Optimize,
        Phase::Emit,
        Phase::Read,
        Phase::Export,
        Phase::Link,
        Phase::Own,
        Phase::Light,
        Phase::Parse,
    ];
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
    /// Reading an entry of the store.
    StoreRead,
    /// Encoding an entry and adding it to the store.
    StoreWrite,
}

impl Step {
    const COUNT: usize = 7;
    const ALL: [(Step, &str); Step::COUNT] = [
        (Step::Infer, "infer"),
        (Step::Mir, "mir"),
        (Step::Monomorphize, "monomorphize"),
        (Step::Translate, "translate"),
        (Step::Describe, "describe"),
        (Step::StoreRead, "store read"),
        (Step::StoreWrite, "store write"),
    ];
}

impl Produced {
    pub(super) fn add(&self, took: Duration, bytes: usize) {
        self.functions.fetch_add(1, Ordering::Relaxed);
        self.nanos.fetch_add(took.as_nanos() as u64, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn take(&self) -> Option<String> {
        let functions = self.functions.swap(0, Ordering::Relaxed);
        let took = Duration::from_nanos(self.nanos.swap(0, Ordering::Relaxed));
        let bytes = self.bytes.swap(0, Ordering::Relaxed);
        (functions != 0).then(|| {
            let each = took / functions as u32;
            format!("{functions} functions, {bytes} bytes, {took:.1?} ({each:.1?} each)")
        })
    }
}

impl Tally {
    pub(super) fn global() -> &'static Tally {
        static TALLY: OnceLock<Tally> = OnceLock::new();
        TALLY.get_or_init(Tally::default)
    }

    #[cfg(feature = "llvm")]
    fn fell_back(&self, took: Duration, reason: &str) {
        self.wasted.fetch_add(took.as_nanos() as u64, Ordering::Relaxed);
        let mut fallbacks = self.fallbacks.lock().unwrap_or_else(|e| e.into_inner());
        *fallbacks.entry(reason.to_owned()).or_default() += 1;
    }

    #[cfg(feature = "llvm")]
    pub(super) fn unit(&self, members: usize) {
        self.units.fetch_add(1, Ordering::Relaxed);
        self.unit_members.fetch_add(members, Ordering::Relaxed);
    }

    #[cfg(feature = "llvm")]
    pub(super) fn alone(&self, reason: &str, functions: usize) {
        let mut alone = self.alone.lock().unwrap_or_else(|e| e.into_inner());
        *alone.entry(reason.to_owned()).or_default() += functions;
    }

    #[cfg(feature = "llvm")]
    pub(super) fn time<T>(&self, phase: Phase, work: impl FnOnce() -> T) -> T {
        let start = Instant::now();
        let out = work();
        self.phases[phase as usize].fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        out
    }

    /// Adds the times of Cranelift's passes on this thread since it was last asked, with
    /// `RA_JIT_STATS`.
    pub(super) fn passes(&self) {
        static STATS: OnceLock<bool> = OnceLock::new();
        if !*STATS.get_or_init(|| std::env::var_os("RA_JIT_STATS").is_some()) {
            return;
        }
        let times = cranelift_codegen::timing::take_current();
        let mut passes = self.passes.lock().unwrap_or_else(|e| e.into_inner());
        passes.get_or_insert_with(Default::default).add(&times);
    }

    /// Runs `work` as part of `step`.
    pub(super) fn step<T>(&self, step: Step, work: impl FnOnce() -> T) -> T {
        // For ra-jit's trace: target `ra_jit` spans of 200 µs and longer.
        let _span = match step {
            Step::Infer => tracing::info_span!(target: "ra_jit", "infer"),
            Step::Mir => tracing::info_span!(target: "ra_jit", "mir"),
            Step::Monomorphize => tracing::info_span!(target: "ra_jit", "monomorphize"),
            Step::Translate => tracing::info_span!(target: "ra_jit", "translate"),
            Step::Describe => tracing::info_span!(target: "ra_jit", "describe"),
            Step::StoreRead => tracing::info_span!(target: "ra_jit", "store read"),
            Step::StoreWrite => tracing::info_span!(target: "ra_jit", "store write"),
        }
        .entered();
        let start = Instant::now();
        let out = work();
        self.add(step, start.elapsed());
        out
    }

    pub(super) fn add(&self, step: Step, took: Duration) {
        self.steps[step as usize].fetch_add(took.as_nanos() as u64, Ordering::Relaxed);
    }

    /// Says what was compiled by which producer since the last call, on stderr. Times are
    /// summed over the threads that compiled.
    pub(super) fn report(&self) {
        let fallbacks =
            std::mem::take(&mut *self.fallbacks.lock().unwrap_or_else(|e| e.into_inner()));
        let wasted = Duration::from_nanos(self.wasted.swap(0, Ordering::Relaxed));
        let phases = Phase::ALL.map(|phase| {
            Duration::from_nanos(self.phases[phase as usize].swap(0, Ordering::Relaxed))
        });
        let (llvm, cranelift) = (self.llvm.take(), self.cranelift.take());
        let large = self.large.swap(0, Ordering::Relaxed);
        let steps = Step::ALL.map(|(step, name)| {
            let took = Duration::from_nanos(self.steps[step as usize].swap(0, Ordering::Relaxed));
            format!("{name} {took:.1?}")
        });
        let selected = Self::selected_llvm();
        let stats = std::env::var_os("RA_JIT_STATS").is_some();
        if !selected && !stats {
            return;
        }
        if stats {
            eprintln!("steps: {}", steps.join(", "));
            let passes = self.passes.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(passes) = passes {
                eprint!("cranelift passes:\n{passes}");
            }
        }
        if let Some(llvm) = llvm {
            let phases = std::iter::zip(Phase::ALL, phases)
                .map(|(phase, took)| format!("{} {took:.1?}", format!("{phase:?}").to_lowercase()))
                .collect::<Vec<_>>()
                .join(", ");
            eprintln!("backend: llvm compiled {llvm}: {phases}");
        }
        if large != 0 {
            eprintln!("backend: llvm compiled {large} large functions without optimization");
        }
        if let Some(exports) = self.exports.take() {
            let imported = self.imported.swap(0, Ordering::Relaxed);
            let size = self.imported_size.swap(0, Ordering::Relaxed);
            let used = self.exports_imported.swap(0, Ordering::Relaxed);
            let called = self.imports_called.swap(0, Ordering::Relaxed);
            let called_size = self.imports_called_size.swap(0, Ordering::Relaxed);
            eprintln!(
                "backend: llvm built exports of {exports}, {used} of them imported; {imported} \
                 imported, {size} instructions, of which {called} ({called_size} instructions) \
                 still called after the passes"
            );
        }
        if let Some(refused) = self.exports_refused.take() {
            eprintln!("backend: llvm built no export of {refused}");
        }
        let (units, members) =
            (self.units.swap(0, Ordering::Relaxed), self.unit_members.swap(0, Ordering::Relaxed));
        let kept = self.unit_kept.swap(0, Ordering::Relaxed);
        if units != 0 {
            eprintln!(
                "backend: llvm compiled {members} functions in {units} units, which kept {kept} \
                 imports"
            );
        }
        let alone = std::mem::take(&mut *self.alone.lock().unwrap_or_else(|e| e.into_inner()));
        if !alone.is_empty() {
            let count: usize = alone.values().sum();
            eprintln!("backend: {count} functions of units compiled alone:");
            for (reason, count) in alone {
                eprintln!("  {count}\t{reason}");
            }
        }
        if let Some(cranelift) = cranelift {
            eprintln!("backend: cranelift compiled {cranelift}");
        }
        if !fallbacks.is_empty() {
            let count: usize = fallbacks.values().sum();
            eprintln!("backend: llvm left {count} functions to cranelift after {wasted:.1?}:");
            for (reason, count) in fallbacks {
                eprintln!("  {count}\t{reason}");
            }
        }
    }

    fn selected_llvm() -> bool {
        matches!(Backend::from_env(), Ok(Backend::Llvm(_)))
    }
}

#[cfg(test)]
mod tests {
    use super::lines_marked;

    /// An edit is a patch of code compiled before, which may be of a build that describes.
    #[test]
    fn only_a_whole_program_that_describes_nothing_has_no_lines_marked() {
        assert!(!lines_marked(false, true));
        assert!(lines_marked(true, true));
        assert!(lines_marked(false, false));
        assert!(lines_marked(true, false));
    }
}
