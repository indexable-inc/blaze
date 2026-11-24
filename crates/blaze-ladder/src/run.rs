//! The straight line: source files to a status, each stage the shape of the task the request
//! machine will ask for (docs/v2/plan/design/bottom.md section 9): lower the files, check each
//! body, build it to LIR, emit it, place every body behind its slot, call the entry.
//!
//! Every stage's refusal is a [`RunError`] naming the function it is about (rule 19); nothing
//! outside a stage's subset is skipped (rule 3).

use core::ffi::c_void;
use core::ptr::NonNull;
use std::alloc::Global;

use blaze_build::data::{DataAlloc, DataError, DataRecord, DataTarget, ImageTarget, StaticImage};
use blaze_build::tls::{self, TlsError};
use blaze_build::layout::{LayoutCx, LayoutFacts, Layouts, Missing, ReprAttrs, ValidRange};
use blaze_build::vtable::{Shape, Vtable, VtableRef, VtableTarget};
use blaze_consteval::{
    Answer, Codes, ConstError, ConstRecord, ConstWorld, FnKind as ConstFnKind, Limits, Lits, Need, NeedKind, RecordKey, Settle,
    Typed, value_of,
};
use blaze_ids::ContentId;
use blaze_typeck::{Checked, ConstHolds, Fetch, PTys};
use blaze_build::{Abi, Adts, BuildError, Instance, InstanceKey, Intrinsic, StaticItem, Subst};
/// The profile's overflow setting every build entry here takes (D101), for callers that do not
/// name blaze-build.
pub use blaze_build::OverflowChecks;
pub use blaze_build::Panic;
use blaze_emit::{Code, EmitError, Isa, Os, Via};
use blaze_own::Alloc;
use blaze_typeck::{
    Adt, CheckError, DiagCode, ExprId, InferCtx, ItemKey, LangItem, List, Mutbl, Needs, Outcome, PNode, PTy, PatId, PortableTys,
    ResolveError,
};

use crate::front::{At, BlazeAttrs, FnKind, FnLang, Front, HOSTED_START};
use crate::crates::{CratesError, Inputs};
use crate::program_texts::ProgramTexts;
use crate::front2::program::{Front2Program, Front2Refusal};
use crate::seam::FrontEnd;
use crate::refusal::{NOT_YET, Refusal, RefusalRule, SourceFile};
use crate::place::{self, Body, Entry, PlaceError, Target, VtableImage, VtableWord};
use crate::tls_index::{self, TlsVars};
use crate::class_chain::class_chain;

/// D106: one missing instance made for the record walk (body_ids.rs `Walk`).
pub mod make_one;

/// D124: the run's roots (its entry, its launcher form, the root instances) and its special
/// records (the C entry shim, the thread-local getter and ender), from names records.
pub mod special;

/// Coroutine states laid out from types, then each coroutine built into its state (ASYNC.md P8).
mod lay_states;

/// A function a stage refused: its path, for the words, and where its item is, for the finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    pub path: Box<str, Global>,
    pub at: At,
}

impl core::fmt::Display for Named {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.path)
    }
}

/// Why a program did not run.
#[derive(Debug)]
pub enum RunError {
    /// The front end refused the program: each refusal as a finding (file, byte range, rule,
    /// words). `shown` is their plain rendering, one line each and [`RUNS_TODAY`] last, until
    /// the CLI's `print.rs` draws `refusals` through blaze-report; then it goes.
    Lower { refusals: std::vec::Vec<Refusal, Global>, shown: Box<str, Global> },
    /// More instances than a slot index holds.
    TooManyInstances,
    /// A record walk asked for a walk with one overflow setting and records keyed by the other
    /// ([`build_records_new`]): the code would be filed under a key that does not describe it
    /// (rule 21).
    OverflowDisagrees { walk: OverflowChecks, key: OverflowChecks },
    /// The checker stopped on a malformed body or a bound.
    Check { function: Named, error: CheckError },
    /// The checker asked for a question the lowered program's world holds back: the world's bug.
    Need { function: Named },
    /// The body has type errors: the first one's code, and how many.
    Types { function: Named, first: DiagCode, count: u32 },
    /// The build refused the function; `range` is where in its item, when the error names an
    /// expression or a pattern of its body.
    Build { function: Named, error: BuildError, range: Option<blaze_typeck::Range> },
    /// The build refused the function at item `item` it reads (a static `Adts` does not
    /// describe, a const whose value it does not hold, in an expression or a pattern, a record
    /// not made yet): `path` is the item's name where the front end gives it, so the survey's
    /// class names the item, not its key.
    ItemRefused { function: Named, error: BuildError, item: ItemKey, path: Option<Box<str>> },
    /// The build met a type naming ADT `adt`, which the seam's `Adts` has no row for; `path`
    /// is its name where the front end can give it.
    NoAdt { function: Named, adt: ItemKey, path: Option<Box<str>> },
    Emit { function: Named, error: EmitError },
    /// An instance's code record was not made, found or stored (code_records.rs).
    CodeRecord { function: Named, error: crate::code_records::CodeRecordError },
    /// An instance the walk cannot run (callee_shown.rs): `function` is the fn whose step met
    /// it (the caller, or the fn the instance is tabled as), `at` that step's instance's
    /// arguments (its `Self` first); `callee` the item, `path` its name where the front end
    /// gives it, `args` its arguments; `gap` which site met it.
    Callee { function: Named, at: Box<str>, callee: ItemKey, path: Option<Box<str>>, args: Box<str>, gap: crate::callee_shown::CalleeGap },
    /// The build of `function` met `item` (a fn it calls, an ADT a type names), which the
    /// survey left out of the program for `why` (the seam's `left_out`: the callee's own check
    /// failed, its ADT row was not made, it has no head): the cause, not the symptom ("no ABI",
    /// "no ADT row", "no callee"). `path` is its name where the front end gives it.
    LeftOut { function: Named, item: ItemKey, path: Option<Box<str>>, why: Box<str> },
    /// A constant (or a `const { .. }` block) the instance of `function` reads, at the
    /// instance's ground arguments, which the front end's evaluator refused before the walk
    /// (front2/const_at.rs): the evaluator's own reason. `path` is the constant's (or the
    /// block's owner's) name where the front end gives it.
    ConstAt { function: Named, refusal: crate::front2::const_at::ConstAtRefusal, path: Option<Box<str>> },
    /// The build of `function` asked the seam to normalize a projection and got no type: `why`
    /// names the trait, `Self`'s form and the solver's error (the seam's `last_projection`).
    Projection { function: Named, why: Box<str> },
    /// A call of a trait's fn found no impl to run. `path` is the callee's name where the front
    /// end gives it (the seam's `item_named`). `at` is the FORM of the call's `Self`
    /// (callee_shown.rs `self_shown`): "a tuple", "a closure", "an ADT `path`", .., so a gap of
    /// the language's builtin impls is told from an impl the program lacks, and that one by
    /// its type.
    Resolve { function: Named, callee: ItemKey, path: Option<Box<str>>, at: Box<str>, error: ResolveError },
    /// A call of `Fn::call`, `FnMut::call_mut` or `FnOnce::call_once` (`callee`, `path` its
    /// name) whose `Self` is a shape the LANGUAGE gives the `Fn*` impls of, with no impl item to
    /// select, and whose built-in rule on this path did not make it (fn_shim.rs
    /// `builtin_shape`): `shape` names which ("a fn item whose fn no row tables", "an async
    /// block (a coroutine)", ..); `error` is what impl selection said when asked anyway.
    NoBuiltinCall { function: Named, callee: ItemKey, path: Option<Box<str>>, shape: &'static str, error: ResolveError },
    /// A vtable the function takes the address of could not be made.
    Vtable { function: Named, principal: ItemKey, why: VtableFail },
    Place(PlaceError),
    /// The program has no `main` at its crate root.
    NoMain,
    /// D106's one-instance maker (run/make_one.rs) could not make the instance of fn `key`:
    /// `why` names the cause (a front end that checks only the whole program, a fn with no
    /// checked body, the maker's own state).
    OneBody { key: ItemKey, why: &'static str },
    /// Static `static_`'s value has no run record (code_records.rs `statics`; run.md, "Statics
    /// in the run"): `error` names what of it is refused.
    StaticRecord { static_: Box<str>, error: crate::code_records::statics::StaticRecordError },
    /// A lang item the back half asks for that the front end refused to answer.
    Lang(crate::seam::LangRefusal),
    /// `main`'s signature is not one the skeleton calls.
    MainShape { main: Named, what: &'static str },
    /// The program starts at bstd's hosted entry and the caller gave no host table.
    NoHost { start: Named },
    /// The program starts at a C `main(argc, argv)` and the caller gave no argument vector
    /// ([`Given::args`]): refused, never called with a made-up one.
    NoArgs { start: Named },
    /// The command line cannot be given to the program (crate::argv: a NUL inside an argument,
    /// more arguments than `argc` counts, or no memory for the vector).
    Args(crate::argv::ArgvError),
    /// The program is a Rust-edition program (its start is `lang_start::<T>` through the C
    /// entry shim) and this is the in-process runner: it runs only in its own process under the
    /// run parent (run.md piece 6). `start` is its `main`. `blaze run` asks [`entry_start`]
    /// first and never hands such a program to the in-process runner, so this is a caller's
    /// error, not a route.
    OwnProcess { start: Named },
    /// The constant evaluator refused `const` item `constant` (or a constant it reads).
    Const { constant: Named, error: ConstError<'static> },
    /// The statics do not form an image.
    Statics { error: DataError },
    /// The thread-locals do not form a template.
    Tls { error: TlsError },
    /// The run numbered `cap` thread-locals and `thread_local` would be one more
    /// (crate::tls_index, THREADS.md D2: the descriptor reserves `cap` rows).
    TlsCap { thread_local: ItemKey, cap: u64 },
    /// The new front end refused the program, by what it cannot do yet.
    Front2(Front2Refusal),
    /// The new front end's crates: the toolchain or a pack.
    Crates(CratesError),
    /// The package's edition is none the names library knows.
    Edition { written: Box<str, Global> },
    /// The names library refused the program's crates (its error, as shown).
    Names { shown: Box<str, Global> },
    /// The package's manifest does not say what the run needs of it (its name, its edition):
    /// the command's reader's words (blaze-cli program.rs `package`).
    Manifest { why: Box<str, Global> },
    /// The program ran and called a function this build could not make (its slot held a stub,
    /// crate::stub): the program was stopped at that call. `refusal` is why that function was
    /// not built; `not_built` of the `reached` instances the program reaches have a stub.
    NotBuilt { refusal: Box<RunError>, not_built: u32, reached: u32 },
    /// D91: `function` raised a panic to unwind (it called [`UNWIND_RAISE`]) and unwinding
    /// through code blaze placed is not registered yet (run.md piece 11, part C): the program was
    /// stopped at the raise, never left to unwind past frames no unwinder can read. No panic
    /// strategy is swapped silently: a panic that is never raised costs nothing.
    UnwindRaised { function: Named },
    /// A static (`static_`, when the image tells which) points at fn instance `key`, which the
    /// walk did not reach: the image names it by key alone, so it is not built; its slot holds a
    /// stub, and only a call through the static stops the program here.
    StaticFn { static_: Option<ItemKey>, key: InstanceKey },
    /// The program trapped in its placed code (a `brk`: LIR's `unreachable`, a division by zero
    /// or overflow, a LIR `trap`) and was stopped there: the trap, the function, its callers,
    /// in one line (crate::trap). The PROGRAM's failure: `blaze run` prints the line and exits
    /// `trap::PROGRAM_TRAPPED`, not blaze's own status.
    Trapped { trap: crate::trap::Trap },
    /// The run's roots (its entry, its form, its root instances) could not be made from the
    /// names (run/special.rs `roots`), by name.
    Roots(special::RootsError),
}

/// The Itanium ABI's entry that starts an unwind (`_Unwind_RaiseException`): a call of it by a
/// placed function is a stub that stops the run ([`RunError::UnwindRaised`], D91) until each
/// placed body's unwind entry is registered (run.md piece 11, part C).
pub const UNWIND_RAISE: &str = "_Unwind_RaiseException";

/// What a run that stubs its refused instances ([`run_program`]) ended with: the program's
/// status, and how many of the instances it reaches are not built (none of them was called).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ran {
    pub status: u8,
    pub not_built: u32,
    pub reached: u32,
}

impl core::fmt::Display for RunError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RunError::Lower { shown, .. } => write!(f, "{shown}"),
            RunError::TooManyInstances => write!(f, "more than 2^32 instances"),
            RunError::OverflowDisagrees { walk, key } => {
                write!(f, "the walk's overflow checks are {walk:?} but its records are keyed {key:?} (rule 21: a key covers every input)")
            }
            RunError::Check { function, error } => write!(f, "`{function}`: the checker stopped: {error:?}"),
            RunError::Need { function } => {
                write!(f, "`{function}`: the checker needed a fact the program's world holds back")
            }
            RunError::Types { function, first, count } => {
                write!(f, "`{function}`: {count} type errors, the first {first:?}")
            }
            RunError::Build { function, error, .. } => write!(f, "`{function}`: {error}"),
            RunError::ItemRefused { function, error, path, .. } => match path {
                Some(path) => write!(f, "`{function}`: `{path}`: {error}"),
                None => write!(f, "`{function}`: {error}"),
            },
            RunError::NoAdt { function, adt, path } => match path {
                Some(path) => write!(f, "`{function}`: a type names the ADT `{path}` ({:#x}), which the build's `Adts` has no row for", adt.0),
                None => write!(f, "`{function}`: a type names ADT {:#x}, which the build's `Adts` has no row for", adt.0),
            },
            RunError::Emit { function, error } => write!(f, "`{function}`: {error}"),
            RunError::CodeRecord { function, error } => write!(f, "`{function}`: {error}"),
            RunError::Callee { function, at, callee, path, args, gap } => match path {
                Some(path) => write!(f, "`{function}` at {at} reaches `{path}` at {args} ({:#x}), {gap}", callee.0),
                None => write!(f, "`{function}` at {at} reaches item {:#x} at {args} (no path in the front end), {gap}", callee.0),
            },
            RunError::Projection { function, why } => write!(f, "`{function}`: a projection the seam did not normalize: {why}"),
            RunError::LeftOut { function, item, path, why } => match path {
                Some(path) => write!(f, "`{function}` needs `{path}` ({:#x}), which the program left out: {why}", item.0),
                None => write!(f, "`{function}` needs item {:#x}, which the program left out: {why}", item.0),
            },
            RunError::ConstAt { function, refusal, path } => match path {
                Some(path) => write!(f, "`{function}` reads `{path}`: {refusal}"),
                None => write!(f, "`{function}`: {refusal}"),
            },
            RunError::Resolve { function, callee, path, at, error } => match path {
                Some(path) => write!(f, "`{function}` calls trait fn `{path}` ({:#x}) with Self {at}: {error}", callee.0),
                None => write!(f, "`{function}` calls trait fn {:#x} with Self {at}: {error}", callee.0),
            },
            RunError::NoBuiltinCall { function, callee, path, shape, error } => {
                match path {
                    Some(path) => write!(f, "`{function}` calls trait fn `{path}` ({:#x}) with Self {shape}", callee.0)?,
                    None => write!(f, "`{function}` calls trait fn {:#x} with Self {shape}", callee.0)?,
                }
                write!(f, ": the language gives this `Fn*` impl (no impl item to select), and the built-in rule for {shape} is not built on this path (fn_shim.rs `builtin_shape`; impl selection said: {error})")
            }
            RunError::Vtable { function, principal, why } => {
                write!(f, "`{function}` takes a vtable of trait {:#x}: ", principal.0)?;
                match why {
                    VtableFail::Shape(error) => write!(f, "{error}"),
                    VtableFail::Layout(error) => write!(f, "the concrete type's layout: {error}"),
                    VtableFail::Build(error) => write!(f, "{error}"),
                    VtableFail::NoDropGlue => write!(f, "the program has no `drop_in_place` lang item for the drop word"),
                }
            }
            RunError::Place(error) => write!(f, "{error}"),
            RunError::NoMain => write!(f, "the crate root has no `main`"),
            RunError::OneBody { key, why } => write!(f, "fn {:#x}: {why} (D106: the record walk makes one instance at a time)", key.0),
            RunError::StaticRecord { static_, error } => write!(f, "the static `{static_}`: {error}"),
            RunError::Lang(l) => write!(f, "the lang item {:?}: {} (rule no-fallback: no build without it)", l.item, l.why),
            RunError::MainShape { main, what } => write!(f, "`{main}`: {what}"),
            RunError::NoHost { start } => {
                write!(f, "`{start}` is bstd's hosted entry, and this run was given no host table to start it with")
            }
            RunError::NoArgs { start } => {
                write!(f, "`{start}` is a C `main` of `argc` and `argv`, and this run was given no argument vector to call it with")
            }
            RunError::Args(error) => write!(f, "{error}"),
            RunError::OwnProcess { .. } => write!(f, "a Rust-edition program runs only in its own process (run.md piece 6)"),
            RunError::Const { constant, error } => write!(f, "`{constant}`: {error}"),
            RunError::Statics { error } => write!(f, "the program's statics: {error}"),
            RunError::Tls { error } => write!(f, "the program's thread-locals: {error}"),
            RunError::TlsCap { thread_local, cap } => write!(
                f,
                "thread-local {:#x} would be past the run's {cap} thread-local rows (rule bounded-progress: the table has a cap)",
                thread_local.0
            ),
            RunError::Front2(refusal) => write!(f, "{refusal}"),
            RunError::Crates(error) => write!(f, "the new front end's crates: {error}"),
            RunError::Edition { written } => write!(f, "the edition `{written}` is none the names library knows"),
            RunError::Names { shown } => write!(f, "the names library: {shown}"),
            RunError::Manifest { why } => write!(f, "the package's manifest: {why}"),
            RunError::NotBuilt { refusal, not_built, reached } => write!(
                f,
                "the program called a function this build of blaze cannot build yet and was stopped there: {refusal} ({not_built} of {reached} reachable functions are not built yet)"
            ),
            RunError::StaticFn { static_, key } => match static_ {
                Some(s) => write!(f, "the static {:#x} points to fn instance {:?}, which no code of the program reaches, so it is not built yet", s.0, key.0),
                None => write!(f, "a static points to fn instance {:?}, which no code of the program reaches, so it is not built yet", key.0),
            },
            RunError::Trapped { trap } => write!(f, "{trap}"),
            RunError::Roots(error) => write!(f, "{error}"),
            RunError::UnwindRaised { function } => write!(
                f,
                "the program panicked and `{function}` raised the panic to unwind (`{UNWIND_RAISE}`): unwinding from code records is not registered yet (run.md piece 11, part C), so the run was stopped at the raise"
            ),
        }
    }
}

impl core::error::Error for RunError {}

impl RunError {
    /// This error as findings: the front end's refusals as they are; a later stage's as one
    /// refusal at the item of the function it names, under `ladder:program` (the build, its
    /// checker or its backend refused the program); none for a failure of placing or running.
    pub fn refusals(self) -> std::vec::Vec<Refusal, Global> {
        let mut out = std::vec::Vec::new_in(Global);
        // A construct the build does not take yet, at its place: the same finding the front end
        // gives for one it does not take (`front::Why::Unsupported`).
        if let RunError::Build {
            ref function,
            error:
                BuildError::Unsupported { what, .. }
                | BuildError::UnsupportedPat { what, .. }
                | BuildError::UnsupportedType { what, .. },
            range: Some(r),
        } = self
        {
            out.push(Refusal {
                file: Some(function.at.file),
                rule: RefusalRule::Unsupported,
                place: function.at.place,
                start: r.start,
                end: r.end,
                secondary: None,
                construct: Some(what),
                message: std::format!("{what} {NOT_YET}").into_boxed_str(),
            });
            return out;
        }
        let function = match self {
            RunError::Lower { refusals, .. } => return refusals,
            RunError::Place(_) => return out,
            ref other => other.function_at(),
        };
        out.push(Refusal {
            file: function.map(|at| at.file),
            rule: RefusalRule::Program,
            place: function.map_or(0, |at| at.place),
            start: 0,
            end: 0,
            secondary: None,
            construct: None,
            message: std::format!("{self}").into_boxed_str(),
        });
        out
    }

    /// Where the function this error is about is written, when it names one.
    fn function_at(&self) -> Option<At> {
        match *self {
            RunError::Lower { .. } | RunError::Place(_) => None,
            // The function that was called and is not built: its refusal's own place.
            RunError::NotBuilt { ref refusal, .. } => refusal.function_at(),
            RunError::Check { ref function, .. }
            | RunError::Need { ref function }
            | RunError::Types { ref function, .. }
            | RunError::Build { ref function, .. }
            | RunError::ItemRefused { ref function, .. }
            | RunError::NoAdt { ref function, .. }
            | RunError::Emit { ref function, .. }
            | RunError::CodeRecord { ref function, .. }
            | RunError::Callee { ref function, .. }
            | RunError::LeftOut { ref function, .. }
            | RunError::Projection { ref function, .. }
            | RunError::Resolve { ref function, .. }
            | RunError::NoBuiltinCall { ref function, .. }
            | RunError::Vtable { ref function, .. }
            | RunError::MainShape { main: ref function, .. }
            | RunError::NoHost { start: ref function }
            | RunError::NoArgs { start: ref function }
            | RunError::OwnProcess { start: ref function }
            | RunError::UnwindRaised { ref function }
            | RunError::ConstAt { ref function, .. }
            | RunError::Const { constant: ref function, .. } => Some(function.at),
            RunError::TooManyInstances
            | RunError::OverflowDisagrees { .. }
            | RunError::NoMain
            | RunError::OneBody { .. }
            | RunError::StaticRecord { .. }
            | RunError::Lang(_)
            | RunError::Statics { .. }
            | RunError::Tls { .. }
            | RunError::TlsCap { .. }
            | RunError::Front2(_)
            | RunError::Crates(_)
            | RunError::Edition { .. }
            | RunError::Manifest { .. }
            | RunError::StaticFn { .. }
            | RunError::Trapped { .. }
            | RunError::Roots(_)
            | RunError::Args(_)
            | RunError::Names { .. } => None,
        }
    }
}

/// The build's questions answered from the front end: ADTs, literals, lang fns, `Drop` impls,
/// ABIs and intrinsics; and each `const` item's evaluated value, by item.
struct FrontAdts<'a, P>(&'a P, usize, &'a [(ItemKey, DataRecord<Global>)], &'a BackLangs, &'a Bodies<'a>);

/// What the build reads of the checked bodies beyond each one's own: every record (a float
/// literal's type) and every closure's facts.
struct Bodies<'a> {
    checked: &'a [Option<Checked<'static>>],
    /// Each constant and const block an instance built so far reads, at its ground arguments,
    /// as the front end evaluated it before that instance's walk (front2/const_at.rs): what
    /// [`FrontAdts`]'s `const_value` and `const_block_value` read.
    const_at: crate::front2::const_at::ConstTable<Global>,
    closures: ClosureTable,
    /// Every async block's facts, the states of the coroutine instances built so far, and the
    /// library's async items (front2/coroutines.rs).
    coroutines: crate::front2::coroutines::Coroutines,
    /// What [`FrontAdts`]'s `overflow_checks` answers: the build's caller's setting (D101).
    overflow: OverflowChecks,
    /// What [`FrontAdts`]'s `ub_checks` answers: on the record path the key's
    /// `Settings::debug_assertions` (code_records.rs), so the key covers it (rule 21).
    ub: blaze_build::UbChecks,
    /// What [`FrontAdts`]'s `panic` answers: on the record path the program's strategy (the
    /// key's, from which its inputs were made); Abort on the in-process paths (D91).
    panic: blaze_build::Panic,
    /// The build's allocator, handed down from its caller: the scratch of [`FrontAdts`]'s
    /// answers (an opaque type's hidden type, a promoted fn's key).
    alloc: Alloc<'static>,
}

/// One built coroutine instance, its three functions kept until each one's slot is reached
/// (`build_from`): `parts` by [`crate::front2::coroutines::Part`], poll, resume, drop.
struct BuiltParts {
    owner: ItemKey,
    ordinal: u32,
    args: Vec<PTy, Global>,
    parts: [Option<blaze_build::Built<Global>>; 3],
    /// The state it was built into, its full lay digested (code_reads.rs `laid_answer`): each
    /// part's `Laid` row.
    laid: ContentId,
    /// What its build asked (run/lay_states.rs `build_laid`), one log for the three parts:
    /// each part's make takes these rows into its read list (run/make_one.rs `build_one`).
    reads: crate::code_records::reads::ReadLog,
}

/// The most owner fns one instance's coroutine lay may have checked on demand (make_one.rs's
/// `Step::Check` loop; rule 18): past it the build is refused by name, never waited on.
const MOST_HELD_COROUTINES: usize = 4096;

/// Every closure's facts ([`LayoutFacts::closure`]), made from the check record of the body
/// that holds it by `blaze_build::layout::closure_facts` ([`ClosureTable::add`]), sorted by
/// (owner, ordinal). THE ONE RULE for every kind of body (rule 30): a fn's checked body (the
/// build's table, [`ClosureTable::of`]) and a constant's or a const block's body the constant
/// evaluator checked (front2/consts.rs `Consts::closures`) each add their closures here, keyed
/// by the owner the checker wrote into the closure's type (`BodyView::owner`, writeback.rs).
/// A closure whose owner did not check has no row: the builder refuses it, naming it.
pub struct ClosureTable {
    tys: PortableTys<'static>,
    rows: Vec<(ItemKey, u32, Result<(blaze_build::FnTrait, PTy, PTy), BuildError>), Global>,
}

impl ClosureTable {
    /// An empty table; `alloc` holds its types for the build or evaluator that keeps it.
    pub fn new(alloc: Alloc<'static>) -> ClosureTable {
        ClosureTable { tys: PortableTys::new_in(alloc), rows: Vec::new_in(Global) }
    }

    /// `alloc` holds the table's types for the build that keeps it, and each closure's scratch.
    fn of<P: FrontEnd>(program: &P, checked: &[Option<Checked<'static>>], alloc: Alloc<'static>) -> ClosureTable {
        let mut table = ClosureTable::new(alloc);
        for (i, done) in checked.iter().enumerate() {
            let Some(done) = done else { continue };
            let sig = program.sig(i);
            let generics = sig.parent_count + sig.own_count;
            let Some((body, _)) = program.function(i).body else { continue };
            table.add(&body.view(program.body_types(i)), done, generics, alloc);
        }
        table
    }

    /// The facts of every closure of `body` (checked as `checked`; `generics` its owner's
    /// parameter count, parents first), under the body's owner. A body whose owner already
    /// has rows (one fn body checked again for each of its const blocks) adds none: one body
    /// is one check, so its facts are the same.
    pub fn add(&mut self, body: &blaze_typeck::BodyView<'_>, checked: &Checked<'_>, generics: u32, scratch: Alloc<'_>) {
        let owner = body.owner;
        let count = checked.closures.len() as u32;
        if count == 0 || self.rows.binary_search_by_key(&(owner, 0), |r| (r.0, r.1)).is_ok() {
            return;
        }
        let at = self.rows.partition_point(|r| r.0 < owner);
        for ordinal in 0..count {
            let facts = blaze_build::layout::closure_facts(body, checked, ordinal, generics, &mut self.tys, scratch);
            self.rows.insert(at + ordinal as usize, (owner, ordinal, facts));
        }
    }

    pub fn get(&self, owner: ItemKey, ordinal: u32) -> Option<blaze_build::layout::ClosureFacts<'_>> {
        let at = self.rows.binary_search_by_key(&(owner, ordinal), |r| (r.0, r.1)).ok()?;
        let (kind, sig, env) = *self.rows[at].2.as_ref().ok()?;
        Some(blaze_build::layout::ClosureFacts { types: self.tys.view(), kind, sig, env })
    }
}

/// The front end's normalization as the builder asks it: an answer not given is the builder's
/// own refusal naming the projection.
/// Whether `e` is a need the front end can fill (`FrontEnd::make_asked`) before the step is
/// taken again: an instance question's `Missing` (an answer no check made), or an ADT whose
/// row is not made (`NoAdt`: the lazy program's `adt` noted its head). A front end that noted
/// nothing makes nothing, and `e` is the refusal.
fn asks_again(e: &RunError) -> bool {
    matches!(
        e,
        RunError::Build { error: BuildError::Select(blaze_typeck::ResolveError::Missing(_)) | BuildError::NoAdt { .. }, .. }
            | RunError::Resolve { error: blaze_typeck::ResolveError::Missing(_), .. }
            | RunError::NoAdt { .. }
    )
}

fn normalized<P: FrontEnd>(program: &P, trait_: ItemKey, name: blaze_typeck::Name, args: &[PTy], ground: &mut PortableTys<'_>) -> Result<PTy, BuildError> {
    match program.normalize(trait_, name, args, ground) {
        Ok(Some(t)) => Ok(t),
        Ok(None) => Err(BuildError::Projection { trait_, name }),
        // The solver's own refusal (an unmade answer is `Missing`, which the instance loop
        // makes and asks again), never "not normalized".
        Err(e) => Err(BuildError::Select(e)),
    }
}

/// `called` as a closure's `Fn*` method instance (`[closure, (A..)]` with the item one of
/// [`Adts::fn_trait_method`]'s): its owner's index, which tables it; `None` for any other call.
fn closure_owner<P: FrontEnd>(program: &P, adts: &dyn Adts, called: Instance, ground: &PortableTys<'_>) -> Option<usize> {
    let first = *ground.view().list(called.args).ok()?.first()?;
    let of = blaze_build::closure_of(ground.view(), first)?;
    let methods = [blaze_build::FnTrait::Fn, blaze_build::FnTrait::FnMut, blaze_build::FnTrait::FnOnce];
    if !methods.into_iter().any(|w| adts.fn_trait_method(w) == Some(called.item)) {
        return None;
    }
    program.fn_index(of.owner)
}

/// The tabled index `called` runs as where the LANGUAGE gives its trait's method and no impl
/// is selected: an async block's `Future::poll` (its owner's, front2/coroutines.rs), a
/// closure's `Fn*` method (its owner's, [`closure_owner`]), a fn item's or pointer's `Fn*`
/// method (its shim, fn_shim.rs; a pointer's at `caller`). One answer for a call's callee and
/// for a vtable's method word (census g091226: 467 words "NoImpl at a closure" while calls
/// found the closure's body).
fn language_owner<P: FrontEnd>(program: &P, adts: &dyn Adts, called: Instance, ground: &PortableTys<'_>, caller: usize) -> Option<usize> {
    crate::front2::coroutines::owner_of(program, adts, called, ground)
        .or_else(|| closure_owner(program, adts, called, ground))
        .or_else(|| crate::fn_shim::fn_shim_owner(program, adts, called, ground, caller))
        .or_else(|| blaze_build::reify_shim_of(called, ground.view()).and_then(|t| program.fn_index(t.item)))
        .or_else(|| crate::clone_shim::language_from(program, called, ground).and_then(|f| program.fn_index(f.from)))
}

/// The refusal of `function`'s call of trait fn `called` when impl selection refused it: a
/// [`RunError::NoBuiltinCall`] when `called` is an `Fn*` method at a `Self` the language gives
/// that impl for (fn_shim.rs `builtin_shape`: its built-in rule did not make it, so "no impl
/// holds" would name the wrong gap), else [`RunError::Resolve`] with `Self`'s form. A
/// `Missing` answer stays a `Resolve`: it is a question to ask and take the step again
/// (`asks_again`), never a refusal of the shape.
fn resolve_refusal<P: FrontEnd>(program: &P, adts: &dyn Adts, ground: &PortableTys<'_>, function: Named, called: Instance, error: blaze_typeck::ResolveError) -> RunError {
    let path = program.item_named(called.item);
    if !matches!(error, blaze_typeck::ResolveError::Missing(_))
        && let Some(shape) = crate::fn_shim::builtin_shape(adts, called, ground)
    {
        return RunError::NoBuiltinCall { function, callee: called.item, path, shape, error };
    }
    let at = crate::callee_shown::self_shown(program, ground.view(), called.args);
    RunError::Resolve { function, callee: called.item, path, at, error }
}

/// The lang ADTs the builder asks for ([`blaze_build::Adts::lang_adt`]), asked of the front end
/// once before the build: a lookup the front end refuses stops the build by the item's name
/// ([`RunError::Lang`]), never a quiet `None` at the builder's question.
struct BackLangs {
    asked: [(LangItem, Option<ItemKey>); 2],
}

impl BackLangs {
    const ITEMS: [LangItem; 2] = [LangItem::ManuallyDrop, LangItem::OwnedBox];

    fn of<P: FrontEnd>(program: &P) -> Result<BackLangs, RunError> {
        let mut asked = [(LangItem::ManuallyDrop, None); 2];
        for (slot, item) in asked.iter_mut().zip(Self::ITEMS) {
            *slot = (item, program.lang_item(item).map_err(RunError::Lang)?);
        }
        Ok(BackLangs { asked })
    }
}

/// The constant evaluator's world: the front end's items and the checked bodies, and the value
/// records made so far (each read back by the constants that read it).
struct FrontConsts<'a, P> {
    program: &'a P,
    checked: &'a [Option<Checked<'static>>],
    store: Vec<ConstRecord<Alloc<'static>>, Global>,
}

/// As [`FrontAdts`]'s facts: the front end's ADTs and reprs.
impl<P: FrontEnd> LayoutFacts for FrontConsts<'_, P> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.program.adt(key)
    }

    fn repr(&self, key: ItemKey) -> Result<ReprAttrs, Missing> {
        self.program.repr(key)
    }

    /// The front end's answer (the seam's `discriminant`). Its `Missing` is the enum's unmade
    /// head; every reader asks `adt` or `repr` of the enum first, which refuse it as the head's
    /// need, so it reaches no reader of a value record.
    fn discriminant(&self, adt: ItemKey, variant: u32) -> Result<Option<i128>, Missing> {
        self.program.discriminant(adt, variant)
    }

    /// The front end's answer (the seam's `valid_range`): the same one the code builder's
    /// layout reads ([`FrontAdts`]), so a constant and the code agree on every niche.
    fn valid_range(&self, key: ItemKey) -> Result<ValidRange, Missing> {
        self.program.valid_range(key)
    }

    fn normalize(&self, trait_: ItemKey, name: blaze_typeck::Name, args: &[PTy], ground: &mut PortableTys<'_>) -> Result<PTy, BuildError> {
        normalized(self.program, trait_, name, args, ground)
    }
}

impl<P: FrontEnd> ConstWorld for FrontConsts<'_, P> {
    fn body(&self, item: ItemKey) -> Fetch<Option<Typed<'_>>> {
        let Some(i) = self.program.fn_index(item) else { return Ok(None) };
        let function = self.program.function(i);
        let Some((body, l)) = function.body else { return Ok(None) };
        // Not checked yet: the evaluator stops with `NeedKind::Body` (`condition_holds`).
        let Some(checked) = self.checked[i].as_ref() else { return Err(blaze_typeck::Missing) };
        let lits = Lits { text: &l.text, exprs: (&l.exprs, &l.expr_ranges), pats: (&l.pats, &l.pat_ranges) };
        let view = body.view(self.program.body_types(i));
        Ok(Some(Typed::new(view, checked, function.text, lits, self.program.code_of(i))))
    }

    /// A tuple constructor builds its value and a `const fn` is interpreted; every other fn is
    /// refused by the evaluator as not `const`.
    fn fn_kind(&self, item: ItemKey) -> ConstFnKind {
        let Some(i) = self.program.fn_index(item) else { return ConstFnKind::NotConst };
        let function = self.program.function(i);
        match function.kind {
            FnKind::Ctor => {
                self.program.ctor_of(item).map_or(ConstFnKind::NotConst, |(adt, variant)| ConstFnKind::Ctor { adt, variant })
            }
            _ if function.const_fn => ConstFnKind::Const,
            _ => ConstFnKind::NotConst,
        }
    }

    fn code_id(&self, item: ItemKey) -> ContentId {
        match self.program.fn_index(item) {
            Some(i) => self.program.code_of(i),
            None => crate::symbols::item_identity(item),
        }
    }

    fn is_static(&self, item: ItemKey) -> bool {
        self.program.static_item(item).is_some()
    }

    fn record(&self, key: RecordKey) -> Fetch<&ConstRecord<Alloc<'_>>> {
        self.store.iter().find(|r| r.key == key).ok_or(blaze_typeck::Missing)
    }

    fn explicit_discriminant(&self, _: ItemKey, _: u32) -> Option<ItemKey> {
        None
    }

    /// The item [`FrontAdts`]'s `fn_trait_method` answers the builder: the program's lang
    /// trait's method, by the one map (front2/langs.rs `fn_trait_method`).
    fn fn_trait_method(&self, which: blaze_build::FnTrait) -> blaze_consteval::FnTraitMethod {
        use blaze_consteval::FnTraitMethod::{Absent, Found};
        let (lang, name) = crate::front2::langs::fn_trait_method(which);
        self.program.lang_trait_fn(lang, name).map_or(Absent, Found)
    }
}

/// Every `const` item's value, evaluated once each with what it reads settled first, as the
/// build reads it: the record's bytes in the build's allocator.
fn eval_consts<P: FrontEnd>(
    program: &P,
    checked: &[Option<Checked<'static>>],
    ground: &mut PortableTys<'_>,
    static_instances: &mut blaze_hash::HashMap<InstanceKey, Instance, Alloc<'static>>,
    alloc: Alloc<'static>,
) -> Result<Vec<(ItemKey, DataRecord<Global>), Global>, RunError> {
    let mut world = FrontConsts { program, checked, store: Vec::new_in(Global) };
    let mut layout = Layouts::new_in(alloc);
    let mut codes = Codes::new_in(alloc);
    let mut out = Vec::new_in(Global);
    for i in 0..program.fn_count() {
        let function = program.function(i);
        // A `where const` condition is evaluated at each impl selection's arguments
        // (`condition_holds`), never once on its own.
        if !matches!(function.kind, FnKind::Const | FnKind::Static) || function.body.is_none() || function.condition {
            continue;
        }
        let mut settle = Settle::new(Need { kind: NeedKind::Value, item: function.key, args: List::EMPTY }, 64, alloc);
        let mut last = None;
        while let Some(n) = settle.next() {
            let step = match value_of(&world, &mut layout, &mut codes, n.item, n.args, ground, Limits::DEFAULT, alloc) {
                Ok(Answer::Done(record)) => {
                    last = Some(record.key);
                    if !world.store.iter().any(|r| r.key == record.key) {
                        world.store.push(record);
                    }
                    settle.done();
                    Ok(())
                }
                Ok(Answer::Need(need)) => settle.need(need),
                Err(error) => Err(error),
            };
            step.map_err(|error| RunError::Const { constant: function.named(), error })?;
        }
        let Some(record) = last.and_then(|key| world.store.iter().find(|r| r.key == key)) else {
            return Err(RunError::Build {
                function: function.named(),
                error: BuildError::Malformed { what: "a settled constant left no record" },
                range: None,
            });
        };
        if matches!(function.kind, FnKind::Static) {
            static_instances.extend(record.fns.iter().map(|named| (named.key, named.instance)));
        }
        let mut allocs = Vec::with_capacity_in(record.data.allocs.len(), Global);
        for a in record.data.allocs.iter() {
            let mut bytes = Vec::with_capacity_in(a.bytes.len(), Global);
            bytes.extend_from_slice(&a.bytes);
            let mut relocs = Vec::with_capacity_in(a.relocs.len(), Global);
            relocs.extend_from_slice(&a.relocs);
            allocs.push(DataAlloc { bytes, align: a.align, mutable: a.mutable, relocs });
        }
        out.push((function.key, DataRecord { allocs }));
    }
    Ok(out)
}

/// Evaluations a condition's `Settle` loop runs at most, bodies checked included (rule 18).
const CONDITION_STEPS: u32 = 4096;

/// `where const` condition `cond`'s value at ground `args` (of `types`): the condition's body
/// and each body it calls checked on demand, then evaluated in a [`Settle`] loop as
/// [`eval_consts`] does. An evaluation error, a body that does not check, a value that is no
/// `bool`, and a condition read while checking a body for another (`Front::nested_world`) are
/// all `Error`: never true.
pub(crate) fn condition_holds(program: &Front<'_>, cond: ItemKey, args: &[PTy], types: PTys<'_>) -> ConstHolds {
    match settle_condition(program, cond, args, types) {
        Some(true) => ConstHolds::True,
        Some(false) => ConstHolds::False,
        None => ConstHolds::Error,
    }
}

fn settle_condition(program: &Front<'_>, cond: ItemKey, args: &[PTy], types: PTys<'_>) -> Option<bool> {
    // The program's own allocator: the condition's scratch lives no longer than the program.
    let alloc = program.alloc;
    let mut ground = PortableTys::new_in(alloc);
    let args = ground_list(&mut ground, args, types)?;
    let mut checked = Vec::with_capacity_in(program.fns.len(), Global);
    checked.resize_with(program.fns.len(), || None);
    let mut store = Vec::new_in(Global);
    let mut layout = Layouts::new_in(alloc);
    let mut codes = Codes::new_in(alloc);
    let mut settle = Settle::new(Need { kind: NeedKind::Value, item: cond, args }, 64, alloc);
    let mut last = None;
    let mut steps = 0;
    while let Some(n) = settle.next() {
        steps += 1;
        if steps > CONDITION_STEPS {
            return None;
        }
        let world = FrontConsts { program, checked: &checked, store: core::mem::replace(&mut store, Vec::new_in(Global)) };
        let answer = value_of(&world, &mut layout, &mut codes, n.item, n.args, &mut ground, Limits::DEFAULT, alloc);
        store = world.store;
        match answer.ok()? {
            Answer::Done(record) => {
                last = Some(record.key);
                if !store.iter().any(|r| r.key == record.key) {
                    store.push(record);
                }
                settle.done();
            }
            Answer::Need(need) if need.kind == NeedKind::Body => {
                let j = program.fn_index(need.item)?;
                if checked[j].is_some() {
                    return None;
                }
                checked[j] = Some(check_for_condition(program, j)?);
            }
            Answer::Need(need) => settle.need(need).ok()?,
        }
    }
    let record = last.and_then(|key| store.iter().find(|r| r.key == key))?;
    match record.data.allocs.first().map(|a| a.bytes.as_slice()) {
        Some([0]) => Some(false),
        Some([1]) => Some(true),
        _ => None,
    }
}

/// Fn `j`'s body checked in [`Front::nested_world`]; `None` when it does not check clean.
fn check_for_condition(program: &Front<'_>, j: usize) -> Option<Checked<'static>> {
    let body = program.fns[j].body.as_ref()?;
    let view = body.body.view(program.tys.view());
    let world = program.nested_world(j);
    let mut cx = InferCtx::new_in(program.alloc);
    match blaze_typeck::check(&view, &world, &mut cx, &Needs::new_in(program.alloc), program.alloc) {
        Ok(Outcome::Done(done)) if done.diags.is_empty() => Some(done),
        _ => None,
    }
}

/// `args` of table `types` in `ground`: the table copied node by node (each node's children
/// come before it, as `PortableTys::intern` requires), then the list.
fn ground_list(ground: &mut PortableTys<'_>, args: &[PTy], types: PTys<'_>) -> Option<List> {
    let mut map = Vec::with_capacity_in(types.len(), Global);
    for &n in types.nodes() {
        let node = match n {
            PNode::Tuple(l) => PNode::Tuple(ground_of_list(ground, types, &map, l)?),
            PNode::Record(s, l) => {
                if let Some(names) = types.record_names(s) {
                    let _ = ground.register_record_schema(s, names);
                }
                PNode::Record(s, ground_of_list(ground, types, &map, l)?)
            }
            PNode::Union(l) => PNode::Union(ground_of_list(ground, types, &map, l)?),
            PNode::Ref(m, t) => PNode::Ref(m, *map.get(t.0 as usize)?),
            PNode::Ptr(m, t) => PNode::Ptr(m, *map.get(t.0 as usize)?),
            PNode::Array(t, len) => PNode::Array(*map.get(t.0 as usize)?, *map.get(len.0 as usize)?),
            PNode::Slice(t) => PNode::Slice(*map.get(t.0 as usize)?),
            PNode::Pat(t, l) => PNode::Pat(*map.get(t.0 as usize)?, ground_of_list(ground, types, &map, l)?),
            PNode::Adt(k, l) => PNode::Adt(k, ground_of_list(ground, types, &map, l)?),
            PNode::FnDef(k, l) => PNode::FnDef(k, ground_of_list(ground, types, &map, l)?),
            PNode::FnPtr(l, h) => PNode::FnPtr(ground_of_list(ground, types, &map, l)?, h),
            PNode::Closure { owner, ordinal, args } => PNode::Closure { owner, ordinal, args: ground_of_list(ground, types, &map, args)? },
            PNode::Future(e, o, i) => PNode::Future(e, *map.get(o.0 as usize)?, *map.get(i.0 as usize)?),
            PNode::Coroutine { owner, ordinal, args } => PNode::Coroutine { owner, ordinal, args: ground_of_list(ground, types, &map, args)? },
            PNode::Alias { trait_, name, args } => PNode::Alias { trait_, name, args: ground_of_list(ground, types, &map, args)? },
            PNode::Opaque(k, l) => PNode::Opaque(k, ground_of_list(ground, types, &map, l)?),
            PNode::Dynamic(l) => PNode::Dynamic(ground_of_list(ground, types, &map, l)?),
            PNode::ExTrait(k, l) => PNode::ExTrait(k, ground_of_list(ground, types, &map, l)?),
            PNode::ExProj { trait_, name, args, term } => PNode::ExProj {
                trait_,
                name,
                args: ground_of_list(ground, types, &map, args)?,
                term: *map.get(term.0 as usize)?,
            },
            leaf @ (PNode::ExAuto(_) | PNode::Foreign(_)) => leaf,
            PNode::ConstValue { ty, bits } => PNode::ConstValue { ty: *map.get(ty.0 as usize)?, bits },
            leaf @ (PNode::Bool
            | PNode::ConstParam(_)
            | PNode::Char
            | PNode::Int(_)
            | PNode::Uint(_)
            | PNode::Float(_)
            | PNode::Str
            | PNode::Never
            | PNode::Param(_)
            | PNode::Infer
            | PNode::Error) => leaf,
        };
        map.push(ground.intern(node).ok()?);
    }
    let mut out = Vec::with_capacity_in(args.len(), Global);
    for a in args {
        out.push(*map.get(a.0 as usize)?);
    }
    ground.intern_list(&out).ok()
}

fn ground_of_list(ground: &mut PortableTys<'_>, types: PTys<'_>, map: &[PTy], l: List) -> Option<List> {
    let mut out = Vec::with_capacity_in(l.len as usize, Global);
    for t in types.list(l).ok()? {
        out.push(*map.get(t.0 as usize)?);
    }
    ground.intern_list(&out).ok()
}

/// The layout's facts from the front end: each the seam's own answer.
impl<P: FrontEnd> LayoutFacts for FrontAdts<'_, P> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.0.adt(key)
    }

    /// As front.rs read the ADT's `#[repr(..)]`.
    fn repr(&self, key: ItemKey) -> Result<ReprAttrs, Missing> {
        self.0.repr(key)
    }

    /// The front end's answer (the seam's `discriminant`; its `Missing` as [`FrontConsts`]'s).
    fn discriminant(&self, adt: ItemKey, variant: u32) -> Result<Option<i128>, Missing> {
        self.0.discriminant(adt, variant)
    }

    /// The front end's `rustc_layout_scalar_valid_range_*` bounds (the seam's `valid_range`:
    /// front2's heads.rs `AdtRow::valid`).
    fn valid_range(&self, key: ItemKey) -> Result<ValidRange, Missing> {
        self.0.valid_range(key)
    }

    fn normalize(&self, trait_: ItemKey, name: blaze_typeck::Name, args: &[PTy], ground: &mut PortableTys<'_>) -> Result<PTy, BuildError> {
        normalized(self.0, trait_, name, args, ground)
    }

    fn closure(&self, owner: ItemKey, ordinal: u32) -> Option<blaze_build::layout::ClosureFacts<'_>> {
        self.4.closures.get(owner, ordinal)
    }

    fn coroutine(&self, owner: ItemKey, ordinal: u32) -> Option<blaze_build::CoroutineFacts<'_>> {
        self.4.coroutines.table.get(owner, ordinal)
    }

    /// `Missing` until the driver has built that instance ([`build_coroutines`]): the body
    /// that asked is then built again.
    fn coroutine_state(&self, owner: ItemKey, ordinal: u32, args: &[PTy], _: PTys<'_>) -> Result<Option<blaze_build::StateShape>, Missing> {
        self.4.coroutines.state(owner, ordinal, args)
    }

    fn opaque_hidden(&self, opaque: ItemKey, args: &[PTy], ground: &mut PortableTys<'_>) -> Result<PTy, BuildError> {
        crate::front2::coroutines::opaque_hidden(self.0, self.4.checked, self, opaque, args, ground, self.4.alloc)
    }
}

impl<P: FrontEnd> Adts for FrontAdts<'_, P> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.0.adt(key)
    }

    fn opaque_of(&self, opaque: ItemKey) -> Option<(ItemKey, u32)> {
        self.0.opaque_of(opaque)
    }

    fn overflow_checks(&self) -> OverflowChecks {
        self.4.overflow
    }

    fn ub_checks(&self) -> blaze_build::UbChecks {
        self.4.ub
    }

    fn panic(&self) -> blaze_build::Panic {
        self.4.panic
    }

    fn facts(&self) -> &dyn LayoutFacts {
        self
    }

    fn int_lit(&self, owner: ItemKey, expr: ExprId) -> Option<u128> {
        self.0.int_lit(owner, expr)
    }

    fn int_pat(&self, owner: ItemKey, pat: PatId) -> Option<u128> {
        self.0.int_pat(owner, pat)
    }

    fn byte_str(&self, owner: ItemKey, expr: ExprId) -> Option<&[u8]> {
        self.0.string(owner, expr)
    }

    fn str_lit(&self, owner: ItemKey, expr: ExprId) -> Option<&str> {
        core::str::from_utf8(self.0.string(owner, expr)?).ok()
    }

    /// The front end's (front2: front2/fmt_template.rs, from the lowering's `FormatArgs` row).
    fn format_template(&self, owner: ItemKey, expr: ExprId) -> Option<&[blaze_build::FmtPiece<'_>]> {
        self.0.format_template(owner, expr)
    }

    fn fmt_argument_new(&self, trait_: blaze_typeck::FmtTrait) -> Option<ItemKey> {
        self.0.fmt_argument_new(trait_)
    }

    fn fmt_argument_adt(&self) -> Option<ItemKey> {
        self.0.fmt_argument_adt()
    }

    /// The literal's bits at the type its body's check gave it: `f32`'s in the low 32, else
    /// `f64`'s (each rounded once from the text by the front end).
    fn float_lit(&self, owner: ItemKey, expr: ExprId) -> Option<u128> {
        let (wide, narrow) = self.0.float_lit(owner, expr)?;
        let done = self.4.checked.get(self.0.fn_index(owner)?)?.as_ref()?;
        let ty = *done.expr_ty.get(expr.0 as usize)?;
        match done.types.view().node(ty).ok()? {
            PNode::Float(blaze_typeck::FloatTy::F32) => Some(u128::from(narrow)),
            PNode::Float(blaze_typeck::FloatTy::F64) => Some(u128::from(wide)),
            _ => None,
        }
    }

    /// A float literal pattern's magnitude at the type its body's check gave the pattern, as
    /// [`Adts::float_lit`] an expression's.
    fn float_pat(&self, owner: ItemKey, pat: PatId) -> Option<u128> {
        let (wide, narrow) = self.0.float_pat(owner, pat)?;
        let done = self.4.checked.get(self.0.fn_index(owner)?)?.as_ref()?;
        let ty = *done.pat_ty.get(pat.0 as usize)?;
        match done.types.view().node(ty).ok()? {
            PNode::Float(blaze_typeck::FloatTy::F32) => Some(u128::from(narrow)),
            PNode::Float(blaze_typeck::FloatTy::F64) => Some(u128::from(wide)),
            _ => None,
        }
    }

    fn str_pat(&self, owner: ItemKey, pat: PatId) -> Option<&[u8]> {
        self.0.str_pat(owner, pat)
    }

    /// The `Fn*` lang traits' methods, as looked up when the program was made (the one map,
    /// front2/langs.rs `fn_trait_method`).
    fn fn_trait_method(&self, which: blaze_build::FnTrait) -> Option<ItemKey> {
        let (lang, name) = crate::front2::langs::fn_trait_method(which);
        self.0.lang_trait_fn(lang, name)
    }

    fn lang_fn(&self, name: &str) -> Option<ItemKey> {
        self.0.fn_lang(FnLang::named(name)?)
    }

    fn type_names(&self) -> Option<&dyn blaze_build::type_name::TypeNames> {
        self.0.type_names()
    }

    fn future_poll(&self) -> Option<ItemKey> {
        self.4.coroutines.poll
    }

    fn async_items(&self) -> Option<blaze_build::AsyncItems> {
        self.4.coroutines.items
    }

    /// `ty`'s `Drop` impl, found by the ground type's head; its `drop` fn as the trait's, which
    /// the instance loop resolves to the impl's. Only a struct, enum or union can implement
    /// `Drop` (E0120; the Reference, "Destructors"): a closure, tuple, array, pointer, `dyn` or
    /// primitive has none and drops only its parts, so it is answered without the solver (a
    /// closure's selection would import it, which needs its owner's checked record). An ADT's
    /// impl covers every instance (E0366/E0367), so it is found in the ADT's own bucket with no
    /// argument imported (blaze-typeck `adt_drop_impl`). Impls are found by the trait, never by
    /// the module: any body's world sees them all.
    fn drop_impl(&self, ty: PTy, ground: &mut PortableTys<'_>) -> Result<Option<ItemKey>, BuildError> {
        let Some((trait_, f)) = self.0.drop_trait() else { return Ok(None) };
        let PNode::Adt(adt, _) = ground.view().node(ty).map_err(|_| BuildError::Malformed { what: "a type past the ground table" })? else {
            return Ok(None);
        };
        // The scratch lives in the ground table's allocator: the caller's, handed down (D102).
        if self.0.adt_drop_impl(self.1, trait_, adt, ground.alloc()).map_err(BuildError::Select)?.is_none() {
            return Ok(None);
        }
        f.map(Some).ok_or(BuildError::Malformed { what: "the `drop` lang trait has no `drop` fn" })
    }

    /// The `index` / `index_mut` lang traits' methods, as looked up when the program was made
    /// (front2's `SEAM_LANGS`); the instance loop resolves a call at `[Self, Idx]` to the
    /// selected impl's, as for `deref_fn`.
    fn index_fn(&self, mutbl: Mutbl) -> Option<ItemKey> {
        match mutbl {
            Mutbl::Not => self.0.lang_trait_fn(LangItem::Index, "index"),
            Mutbl::Mut => self.0.lang_trait_fn(LangItem::IndexMut, "index_mut"),
        }
    }

    fn deref_fn(&self, mutbl: Mutbl) -> Option<ItemKey> {
        match mutbl {
            Mutbl::Not => self.0.lang_trait_fn(LangItem::Deref, "deref"),
            Mutbl::Mut => self.0.lang_trait_fn(LangItem::DerefMut, "deref_mut"),
        }
    }

    /// From [`BackLangs`]; an item outside it is one the builder never asks (its doc's list).
    fn lang_adt(&self, item: LangItem) -> Option<ItemKey> {
        self.3.asked.iter().find(|(i, _)| *i == item).and_then(|(_, k)| *k)
    }

    fn object_trait(&self, t: ItemKey) -> Option<blaze_build::vtable::ObjectTrait<'_>> {
        self.0.object_trait(t)
    }

    fn abi(&self, item: ItemKey) -> Option<Abi> {
        self.0.fn_abi(item)
    }
    /// From the front end (`FrontEnd::nounwind`, front2 program/head_abi.rs): `extern
    /// "C"`/"system" and `#[rustc_nounwind]` fns are nounwind; "Rust", "rust-call", "C-unwind"
    /// and "system-unwind" fns are not (the Reference, "Functions",
    /// [items.fn.extern.unwind.behavior]).
    fn nounwind(&self, owner: ItemKey) -> Option<bool> {
        self.0.nounwind(owner)
    }

    fn fn_attrs(&self, owner: ItemKey) -> blaze_lir::func::FnAttrs {
        self.0.fn_attrs(owner)
    }

    /// The front end resolves the function's own or inherited caller-tracking attribute.
    /// At a call from an untracked function, the fast emitter records the item-relative
    /// caller site. Placement supplies its current line and column.
    fn track_caller(&self, instance: Instance, _ground: &mut PortableTys<'_>) -> Option<bool> {
        self.0.track_caller(instance.item)
    }

    /// The library's `#[lang = "panic_location"]` struct (core/src/panic/location.rs).
    fn location_adt(&self) -> Option<ItemKey> {
        self.0.lang_named("panic_location")
    }

    /// From the front end (`FrontEnd::caller_site`): item-relative, never a line (rule 71).
    fn caller_site(&self, owner: ItemKey, expr: ExprId) -> Result<blaze_build::CallerSite<'_>, blaze_build::SiteGap> {
        self.0.caller_site(owner, expr)
    }

    /// From the front end (`FrontEnd::def_site`): item-relative, as `caller_site`.
    fn def_site(&self, item: ItemKey) -> Result<blaze_build::CallerSite<'_>, blaze_build::SiteGap> {
        self.0.def_site(item)
    }

    /// From the front end (`FrontEnd::expr_site`): a trap row's site, with no file.
    fn expr_site(&self, owner: ItemKey, expr: ExprId) -> Option<u32> {
        self.0.expr_site(owner, expr)
    }

    fn c_variadic(&self, item: ItemKey) -> Option<u32> {
        self.0.c_variadic(item)
    }

    /// From the fn's head (`FnHead::legacy_const_generics`, front2 heads.rs).
    fn legacy_const_generics(&self, item: ItemKey) -> &[u32] {
        self.0.legacy_const_generics(item)
    }

    fn static_item(&self, item: ItemKey) -> Option<StaticItem> {
        self.0.static_item(item)
    }

    /// A `const` item's value: the front end's own (front2 evaluates every constant its bodies
    /// read) or, for a front that evaluates none, as `eval_consts` made it from the tabled
    /// constants (the two never both hold one: front2 tables no constant).
    ///
    /// `args` (the path's recorded arguments, ground at the instance: blaze-typeck
    /// `Checked::const_args_of`): a path with arguments reads the value pulled for this
    /// instance before its walk, at exactly those arguments (front2/const_at.rs; the table is
    /// keyed by the constant and its arguments, so `<u8 as P>::S` and `<Q as P>::S` are two
    /// rows). A front end that evaluates no constant at arguments (the old front) pulls none;
    /// its records were evaluated at NO arguments, and its evaluator refuses a generic
    /// parameter it has none for, so such a record read no parameter and is the same at every
    /// instance. front2 makes no record at no arguments for a path with arguments
    /// (program.rs `const_values`): there the table is the only answer.
    fn const_value(&self, item: ItemKey, args: &[PTy]) -> Option<&DataRecord<Global>> {
        // A pulled row first, at no arguments too: a non-generic ADT's field default (RFC
        // 3681) is pulled at no arguments (const_at.rs `needs_of`) and has no program record.
        if let Some(record) = self.4.const_at.get(crate::front2::const_at::ConstNeed::Item(item), args) {
            return Some(record);
        }
        match self.0.const_record(item) {
            Some(record) => Some(record),
            None => self.2.iter().find(|(k, _)| *k == item).map(|(_, record)| record),
        }
    }

    /// `const { .. }` block `block` of `owner` at the instance's ground arguments `args`: the
    /// value pulled before the instance's walk (front2/const_at.rs), keyed by the block and its
    /// arguments, so `const { size_of::<T>() }` at `u64` and at `u16` are two rows.
    fn const_block_value(&self, owner: ItemKey, block: blaze_typeck::ExprId, args: &[PTy]) -> Option<&DataRecord<Global>> {
        self.4.const_at.get(crate::front2::const_at::ConstNeed::Block { owner, block }, args)
    }

    /// Promoted borrow `borrow` of `owner` at the instance's ground arguments `args`: the
    /// operand's value pulled before the instance's walk (front2/const_at.rs
    /// `ConstNeed::Promoted`), as a const block's is.
    fn promoted_value(&self, owner: ItemKey, borrow: blaze_typeck::ExprId, args: &[PTy]) -> Option<&DataRecord<Global>> {
        self.4.const_at.get(crate::front2::const_at::ConstNeed::Promoted { owner, borrow }, args)
    }

    /// The fn instance a constant's record names by key (`DataTarget::Fn(key)`: `const F: fn()
    /// -> i32 = seven;`, `const G: fn(u8) -> u8 = id::<u8>;`, a closure coerced in a constant,
    /// a fn declared in the constant's own block). The evaluator that made the record named the
    /// instance it made each fn word from (blaze-consteval `ConstRecord::fns`), so the front end
    /// that kept the record answers it, its arguments copied into `ground`
    /// ([`FrontEnd::const_fn_instance`]); nothing is searched for and no fn's body needs to be
    /// checked first. One answer for both paths (rule 30): the whole-program build here and the
    /// record walk's maker (run/make_one.rs) read the same records. The walk files the instance
    /// in `Built::refs`, which the build loop reaches like any callee and the maker declares in
    /// the made entry (its row, made on its first call through the pointer), so a later process
    /// that finds the record by key never asks again. `None` (refused by the walk as
    /// `BuildError::NoConstFnInstance`): no record of the front end names the key.
    fn const_fn_instance(&self, key: InstanceKey, ground: &mut PortableTys<'_>) -> Option<Instance> {
        self.0.const_fn_instance(key, ground, self.4.alloc)
    }

    fn intrinsic(&self, item: ItemKey) -> Option<Intrinsic> {
        self.0.intrinsic(item)
    }
}

/// What this build of blaze runs today: the closing line of a refusal.
pub const RUNS_TODAY: &str = "this build of blaze runs a package with no dependencies and no standard library \
     (`#![no_core]`), in the subset its ladder covers (crates/blaze-cli/tests/ladder/README.md)";

/// Refusals as plain lines, `path:line:col: words` each (positions become lines only here, at
/// print), then [`RUNS_TODAY`]. The stand-in for the renderer's drawing.
pub fn plain(files: &[SourceFile<'_>], refusals: &[Refusal]) -> std::string::String {
    use core::fmt::Write;
    let mut text = std::string::String::new();
    for r in refusals {
        if let Some(sf) = r.file.and_then(|f| files.get(f as usize)) {
            let before = sf.text.get(..(r.place + r.start) as usize).unwrap_or(sf.text);
            let line = before.bytes().filter(|&b| b == b'\n').count() + 1;
            let col = before.len() - before.rfind('\n').map_or(0, |i| i + 1) + 1;
            let _ = write!(text, "{}:{line}:{col}: ", sf.path);
        }
        let _ = writeln!(text, "{}", r.message);
    }
    let _ = write!(text, "{RUNS_TODAY}");
    text
}

/// How `main` is called, from its signature: `fn main()`, `extern "C" fn main() -> i32`, a C
/// `extern "C" fn main(argc: i32, argv: *const *const c_char) -> i32`, or bstd's hosted entry.
fn entry_shape<P: FrontEnd>(program: &P, main: usize) -> Result<Start, RunError> {
    let sig = program.sig(main);
    let row = program.function(main);
    let shape = |what| RunError::MainShape { main: row.named(), what };
    let node = |t: PTy| sig.types.get(t);
    let returns_i32 = matches!(node(sig.output), Some(PNode::Int(blaze_typeck::IntTy::I32)));
    // A Rust-edition root: `fn main() -> T` starts through `lang_start::<T>`, called by the C
    // entry shim (blaze-build start.rs); `T: Termination` is the check's. The Reference,
    // "Crates and source files": "it must take no arguments". `rust_start` is `Some` exactly
    // when front2's start rule (`front2::program::start_of`) answered `LangStart`: the rule
    // [`entry_start`] asks before anything is built, so `blaze run` never builds this here.
    if program.rust_start().is_some() {
        if !sig.inputs.is_empty() {
            return Err(shape("a Rust program's `main` takes no arguments (the Reference, \"Crates and source files\")"));
        }
        return Ok(Start::Plain(Entry::CMainShim));
    }
    if row.export == Some(HOSTED_START) {
        let one_pointer = matches!(sig.inputs, [p] if matches!(node(*p), Some(PNode::Ptr(Mutbl::Not, _))));
        return match (row.abi, one_pointer, returns_i32) {
            (Abi::C, true, true) => Ok(Start::Hosted),
            _ => Err(shape("bstd's hosted entry is `extern \"C\" fn(*const HostTable) -> i32`")),
        };
    }
    // A C `main` of its command line (C17 5.1.2.2.1: `int main(int argc, char *argv[])`):
    // called with the run's argument vector (crate::argv), the one a hosted program's `args`
    // grant reads. `c_char` is `i8` on arm64 macOS and `u8` on arm64 Linux (core::ffi's alias
    // resolves to the integer), so both are taken; either mutability at each level (`char **`
    // is `*mut *mut c_char`; the call passes the same address). Any other list is refused by
    // name: no start passes it.
    match sig.inputs {
        [] => {}
        [argc, argv] => {
            let is_argc = matches!(node(*argc), Some(PNode::Int(blaze_typeck::IntTy::I32)));
            let is_argv = match node(*argv) {
                Some(PNode::Ptr(_, inner)) => match node(*inner) {
                    Some(PNode::Ptr(_, ch)) => matches!(node(*ch), Some(PNode::Int(blaze_typeck::IntTy::I8) | PNode::Uint(blaze_typeck::UintTy::U8))),
                    _ => false,
                },
                _ => false,
            };
            return match (is_argc && is_argv, row.abi, returns_i32) {
                (false, _, _) => Err(shape("its two parameters are not `(argc: i32, argv: *const *const c_char)`")),
                (true, Abi::C, true) => Ok(Start::Main),
                (true, Abi::C, false) => Err(shape("it takes `argc` and `argv` and does not return `i32`, as a C `main` does")),
                (true, _, _) => Err(shape("it takes `argc` and `argv` but is not `extern \"C\"`; write `extern \"C\"` and `#![no_main]`")),
            };
        }
        _ => return Err(shape("its parameters are neither none nor `(argc: i32, argv: *const *const c_char)`")),
    }
    match node(sig.output) {
        Some(PNode::Tuple(list)) if list.len == 0 => Ok(Start::Plain(Entry::Unit)),
        Some(PNode::Int(blaze_typeck::IntTy::I32)) if row.abi == Abi::C => Ok(Start::Plain(Entry::I32)),
        Some(PNode::Int(blaze_typeck::IntTy::I32)) => {
            Err(shape("it returns `i32` with the Rust ABI; write `extern \"C\"` and `#![no_main]`"))
        }
        _ => Err(shape("it returns neither `()` nor `i32`")),
    }
}

/// How the program starts: a plain entry, or bstd's hosted entry, given the host's table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Start {
    Plain(Entry),
    Hosted,
    /// A C `main(argc, argv)`: called with the run's argument vector ([`Given::args`]).
    Main,
}

/// A program built and ready to place: each reached instance's code, the entry's first.
pub struct Built {
    bodies: Vec<Body, Global>,
    /// Every vtable a body names, once each (`Body::vtables` index it).
    vtables: Vec<VtableImage, Global>,
    start: Start,
    start_fn: Named,
    /// Every static's first bytes, the thread-locals' templates and descriptor among them.
    image: Option<StaticImage<Global>>,
    /// With thread-locals: the getter's slot.
    getter: Option<u32>,
    /// With thread-locals: the ender's slot (the key's destructor, crate::tls_index).
    ender: Option<u32>,
    /// By stub number: why the instance in that stub's slot was not built (crate::stub).
    /// Empty unless the build stubbed its refusals ([`build_stubbed`]).
    refused: Vec<RunError, Global>,
    /// The instances the entry reaches, built or not: the slots before the getter's.
    reached: u32,
    /// Each fn instance a static points at, sorted by key, and its slot ([`static_fn_slots`]):
    /// placement writes the slot's thunk into the static's word.
    static_fns: Vec<(InstanceKey, u32), Global>,
    /// Each slot's function path, by slot: what a trap's line names (crate::trap).
    paths: Vec<Box<str, Global>, Global>,
}

impl Built {
    /// Why each stubbed instance was not built, by stub number: empty unless the build stubbed
    /// its refusals ([`build_stubbed`]). What a run that calls one stops with; read without
    /// running by examples/hello_stops.rs, which lists them all.
    pub fn refused(&self) -> &[RunError] {
        &self.refused
    }

    /// The instances the entry reaches, built or stubbed.
    pub fn reached(&self) -> u32 {
        self.reached
    }

    /// Slot `slot`'s code as placement binds it: its record id (place.rs `place`) and its bytes,
    /// source position fields zero; `None` for a slot past the bodies.
    pub fn code_record(&self, slot: usize) -> Option<(blaze_launch::wire::RecordId, &[u8])> {
        let body = self.bodies.get(slot)?;
        Some((place::record_id(&body.code), &body.code.bytes))
    }
}

/// The machine the code is emitted for. Every Apple M-series core implements FEAT_LSE
/// (Armv8.1); the code runs in this process, so its system is this one's.
fn isa() -> Isa {
    Isa {
        lse: true,
        os: match cfg!(target_os = "macos") {
            true => Os::MacOs,
            false => Os::Linux,
        },
    }
}

/// The thread-locals' descriptor static: not an item of the program, so a key of its own,
/// fixed, in the image beside the program's statics. Each thread-local's template has a key
/// derived from its own (crate::tls_index).
const TLS_DESC: ItemKey = ItemKey(u128::from_be_bytes(*b"blaze-ladder dsc"));

/// The program's statics as one image, and the run's numbering of its thread-locals: each
/// plain static's evaluated record, each thread-local's template and the descriptor
/// (THREADS.md D2). This whole-program run numbers every thread-local in item-key order here,
/// before any body is emitted, so each index a body names has its row in the image.
fn static_image<P: FrontEnd>(
    program: &P,
    values: &[(ItemKey, DataRecord<Global>)],
    refused: Option<&mut Vec<(Box<str>, Box<str>), Global>>,
) -> Result<(Option<StaticImage<Global>>, Option<TlsVars>), RunError> {
    let mut keys = Vec::new_in(Global);
    // `eval_consts`'s records (the old front's tabled statics) and the front end's own
    // (front2's, `static_records`): never both for one front.
    for (key, _) in values.iter().chain(program.static_records()) {
        if program.static_item(*key).is_some_and(|s| s.thread_local) {
            keys.push(*key);
        }
    }
    // A key with two records stays twice in neither list: `image_parts` refuses it by name.
    keys.sort_unstable();
    keys.dedup();
    let mut vars = TlsVars::new();
    for &key in keys.iter() {
        vars.index_of(key)?;
    }
    let parts = match vars.is_empty() {
        true => None,
        false => Some(vars.image_parts(program, values)?),
    };
    let mut plain = Vec::new_in(Global);
    for (key, record) in values.iter().chain(program.static_records()) {
        if program.static_item(*key).is_some_and(|s| !s.thread_local) {
            plain.push((*key, record));
        }
    }
    if let Some((desc, templates)) = &parts {
        for (key, record) in templates.iter() {
            plain.push((*key, record));
        }
        plain.push((TLS_DESC, desc));
    }
    // A survey places what it can: a record relocating to a static the program does not place
    // (one the survey left out, or one with no evaluated value) is filed by its static and
    // dropped, in rounds (dropping one can leave another's relocation dangling; each round
    // drops one at least, so `plain.len()` rounds bound it). A run keeps every record and
    // `StaticImage::new` refuses the first such relocation by name.
    if let Some(out) = refused {
        let shown = |k: ItemKey| program.item_named(k).unwrap_or_else(|| format!("{:#x}", k.0).into_boxed_str());
        for _ in 0..=plain.len() {
            let mut placed: Vec<ItemKey, Global> = Vec::with_capacity_in(plain.len(), Global);
            placed.extend(plain.iter().map(|p| p.0));
            placed.sort_unstable();
            let before = plain.len();
            plain.retain(|(key, record)| {
                let dangling = record.allocs.iter().flat_map(|a| a.relocs.iter()).find_map(|r| match r.target {
                    DataTarget::Static(t) if placed.binary_search(&t).is_err() => Some(t),
                    _ => None,
                });
                match dangling {
                    Some(t) => {
                        out.push((shown(*key), format!("static: a relocation to static `{}`, which the program does not place", shown(t)).into_boxed_str()));
                        false
                    }
                    None => true,
                }
            });
            if plain.len() == before {
                break;
            }
        }
    }
    if plain.is_empty() {
        return Ok((None, None));
    }
    // The image copies each record's bytes.
    let image = StaticImage::new(&plain, Global).map_err(|error| RunError::Statics { error })?;
    drop(plain);
    Ok((Some(image), (!vars.is_empty()).then_some(vars)))
}

/// Runs the program of `files`, whose crate root is `root`, in this process: its status.
/// `host` is the host's table (bstd's `wire::HostTable`, built by bstd's `Hosting`): a program
/// that starts at bstd's hosted entry is called with it, and refused without one; any other
/// program does not read it.
/// `alloc` holds what the run keeps past the program (a trap's names): the caller's.
pub fn run(files: &[SourceFile<'_>], root: &str, host: Option<NonNull<c_void>>, overflow: OverflowChecks, alloc: blaze_own::SharedAlloc<'static>) -> Result<u8, RunError> {
    run_with(files, root, host, FrontChoice::Old(overflow), alloc)
}

/// Which front end answers the back half (crate::seam): one explicit input of the runner. The
/// new one never hands a program to the old one: what it cannot do is a [`RunError::Front2`]
/// naming it. Each carries the profile's `overflow-checks` (D101; [`NewFront::overflow`]).
#[derive(Clone, Copy, Debug)]
pub enum FrontChoice<'a> {
    Old(OverflowChecks),
    New(NewFront<'a>),
}

/// What the new front end needs: the package as the names library takes it, and the
/// profile's overflow setting the build takes (the old front end takes it beside the files).
#[derive(Clone, Copy, Debug)]
pub struct NewFront<'a> {
    /// The package's name: its crate's name.
    pub package: &'a str,
    /// Its `Cargo.toml`'s edition, as written (`2024`, `blaze-2026.11`).
    pub edition: &'a str,
    /// The toolchain's shelf; `None` resolves over no toolchain crate (a `#![no_core]` program).
    pub shelf: Option<&'a str>,
    /// The profile's `overflow-checks` (decision D101): `run` and `test` build with the `dev`
    /// profile's (on unless the manifest's `[profile.dev]` says otherwise), `export` with
    /// `release`'s (off unless `[profile.release]` says otherwise).
    pub overflow: OverflowChecks,
}

/// [`run`] through the front end `choice` names.
pub fn run_with(files: &[SourceFile<'_>], root: &str, host: Option<NonNull<c_void>>, choice: FrontChoice<'_>, alloc: blaze_own::SharedAlloc<'static>) -> Result<u8, RunError> {
    let built = match choice {
        FrontChoice::Old(overflow) => build(files, root, overflow, alloc.local())?,
        FrontChoice::New(new) => build_new(files, root, new, alloc.local())?,
    };
    run_built(built, host, alloc)
}

/// The last stage of [`run`]: `built` placed in this process and its entry called; the
/// program's status.
pub fn run_built(built: Built, host: Option<NonNull<c_void>>, alloc: blaze_own::SharedAlloc<'static>) -> Result<u8, RunError> {
    ran_built(built, host, alloc).map(|ran| ran.status)
}

/// [`run_built`], with the count of instances not built; a called stub is
/// [`RunError::NotBuilt`] with that instance's refusal. A trap's names are kept in `alloc`, the
/// caller's (D102: no allocator is fetched here).
pub fn ran_built(built: Built, host: Option<NonNull<c_void>>, alloc: blaze_own::SharedAlloc<'static>) -> Result<Ran, RunError> {
    ran_built_given(built, Given { host, args: None }, alloc)
}

/// What a run gives the program to start with (run.md, "What a program is given"): the host's
/// table, for bstd's hosted entry, and the command line's C form, for a C `main(argc, argv)`.
/// Both come from one command line: the caller makes its `Argv` (crate::argv) once, hands the
/// host `Argv::strings` and this `Argv::c`. An entry that needs one that is `None` is refused
/// by name ([`RunError::NoHost`], [`RunError::NoArgs`]).
#[derive(Clone, Copy, Debug)]
pub struct Given {
    /// bstd's `wire::HostTable`, alive until the run returns (the caller holds its `Hosting`).
    pub host: Option<NonNull<c_void>>,
    /// The program's `argc` and `argv`, alive until the run returns (the caller holds the
    /// `Argv` they point into).
    pub args: Option<crate::argv::CArgs>,
}

/// [`ran_built`], given the host's table and the command line ([`Given`]).
pub fn ran_built_given(mut built: Built, given: Given, alloc: blaze_own::SharedAlloc<'static>) -> Result<Ran, RunError> {
    let entry = match (built.start, given.host, given.args) {
        // L1b: a Rust-edition program never runs inside blaze's process (its `process::exit`
        // would end blaze, and its argv would be blaze's own): run.md piece 6. `blaze run`
        // decides that before building ([`entry_start`]) and never hands one here; a caller
        // that does is refused by name, never placed.
        (Start::Plain(Entry::CMainShim), _, _) => return Err(RunError::OwnProcess { start: built.start_fn }),
        (Start::Plain(entry), _, _) => entry,
        (Start::Hosted, Some(table), _) => Entry::Hosted(table),
        (Start::Hosted, None, _) => return Err(RunError::NoHost { start: built.start_fn }),
        (Start::Main, _, Some(args)) => Entry::Main(args),
        (Start::Main, _, None) => return Err(RunError::NoArgs { start: built.start_fn }),
    };
    let image = match &built.image {
        Some(image) => Some((image, place::load_statics(image).map_err(RunError::Place)?)),
        None => None,
    };
    let tls = match (built.getter, image) {
        (Some(getter), Some((image, base))) if built.ender.is_some() => {
            let at = image.static_at(TLS_DESC).ok_or(RunError::Place(PlaceError::NoDescriptor))?;
            // The key goes into the descriptor after `place` wrote the image (below).
            Some(place::Tls { desc: base + u64::from(at), getter })
        }
        _ => None,
    };
    // Each stub's refusal in words, for the one stop that cannot come back here (a stub called
    // on a thread the program started: stub.rs `stopped`).
    let said: Box<[Box<str>]> = built.refused.iter().map(|refusal| format!("{refusal}").into_boxed_str()).collect();
    // A raise's stub (D91) is no function left unbuilt: not counted.
    let raises = built.refused.iter().filter(|refusal| matches!(refusal, RunError::UnwindRaised { .. })).count();
    let not_built = u32::try_from(built.refused.len() - raises).map_err(|_| RunError::TooManyInstances)?;
    let placed = place::place(&built.bodies, &built.vtables, image, &built.static_fns, tls, said).map_err(RunError::Place)?;
    if let (Some(tls), Some(ender)) = (tls, built.ender) {
        let ender = placed.thunk(ender).ok_or(RunError::Place(PlaceError::NoEnder))?;
        tls_index::create_key(tls.desc, ender).map_err(RunError::Place)?;
    }
    // The entry's instance was the first reached.
    match placed.call(0, entry).map_err(RunError::Place)? {
        place::Ended::Status(status) => Ok(Ran { status, not_built, reached: built.reached }),
        // D91: the program raised a panic to unwind; stopped there, by its own error.
        place::Ended::Stub(stub) if matches!(built.refused.get(stub as usize), Some(RunError::UnwindRaised { .. })) => Err(built.refused.swap_remove(stub as usize)),
        place::Ended::Stub(stub) if (stub as usize) < built.refused.len() => {
            Err(RunError::NotBuilt { refusal: Box::new(built.refused.swap_remove(stub as usize)), not_built, reached: built.reached })
        }
        // A stub's number is its place in `refused`: one past it was never made.
        place::Ended::Stub(_) => Err(RunError::Place(PlaceError::NoEntry { slot: 0 })),
        // A trap in the placed code: the program's own failure, by name (crate::trap).
        place::Ended::Trap { code, slot, offset, callers } => Err(RunError::Trapped {
            trap: crate::trap::Trap::new(code, slot, offset, &callers, &built.paths, alloc),
        }),
    }
}

/// What `blaze run` refuses of the program of `files`, every stage up to placing: empty when
/// it would place and call `main`. blaze-cli's findings path calls this, and [`run`] stops at
/// the same refusals, so the two cannot disagree (rule 30). Each stage stops at its first.
/// `alloc` is the caller's, handed down from its entry: the program's records live in it.
pub fn refusals(files: &[SourceFile<'_>], root: &str, overflow: OverflowChecks, alloc: Alloc<'static>) -> Vec<Refusal, Global> {
    findings(files, root, overflow, alloc).0
}

/// [`refusals`], and the `blaze::` attributes the front end read: the advice about unknown
/// names and the count nothing checks ([`BlazeAttrs::note`]). `blaze check`'s call.
/// `overflow` is the setting `blaze run` would build with: with it on, an integer `+` needs
/// core's `panic_const_add_overflow`, so the refusals can differ.
pub fn findings(files: &[SourceFile<'_>], root: &str, overflow: OverflowChecks, alloc: Alloc<'static>) -> (Vec<Refusal, Global>, BlazeAttrs) {
    let mut blaze = BlazeAttrs::default();
    let refusals = match build_noting(files, root, &mut blaze, overflow, alloc) {
        Ok(_) => Vec::new_in(Global),
        Err(error) => error.refusals(),
    };
    (refusals, blaze)
}

/// Every stage of [`run`] before placing: read, check, build and emit.
///
/// Every body is checked once, generically. Instances are then built from `main` forward:
/// slot `n` is the `n`th instance reached, and only reached instances are built (a generic fn
/// once per list of ground arguments, an unreached fn never).
/// `alloc` is the caller's, handed down from its entry: the build's tables live in it.
pub fn build(files: &[SourceFile<'_>], root: &str, overflow: OverflowChecks, alloc: Alloc<'static>) -> Result<Built, RunError> {
    build_noting(files, root, &mut BlazeAttrs::default(), overflow, alloc)
}

fn infer_frame() -> Result<blaze_mem::Frame, RunError> {
    blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).map_err(|e| RunError::Names { shown: std::format!("the command's frame: {e}").into_boxed_str() })
}

/// [`build`] through the new front end: the program's names resolved by the names library over
/// the toolchain `new` names, then front2's answers to the back half.
pub fn build_new(files: &[SourceFile<'_>], root: &str, new: NewFront<'_>, alloc: Alloc<'static>) -> Result<Built, RunError> {
    // The in-process path (D91): the program's inputs say Unwind, Rust's default, so std binds
    // panic_unwind and a raise stops at D91's `UnwindRaised`; the code is built Abort.
    let panic = blaze_build::Panic::Unwind;
    let tc = crate::library_set::toolchain_for(files, root, new, panic, alloc)?;
    let texts = ProgramTexts::new(files, root, new, &tc, panic)?;
    let user = texts.user();
    let names_frame = infer_frame()?;
    let names_erased = blaze_own::Erased::new(&names_frame);
    let inputs = Inputs::new(&user, &tc).map_err(RunError::Crates)?;
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, names_erased.bump()).map_err(|e| RunError::Names { shown: std::format!("{e}").into_boxed_str() })?;
    build_over(&names, files, &texts, alloc)
}

/// [`build_new`] from the program's names on: front2's answers to the back half over `names`
/// (the program's crate at index 0, resolved from `texts`). For a caller that resolved the
/// names itself (examples/front2_rungs.rs: the toolchain library once, `crates::library`).
pub fn build_over(names: &blaze_scope::resolve::Names<'_>, files: &[SourceFile<'_>], texts: &ProgramTexts, alloc: Alloc<'static>) -> Result<Built, RunError> {
    let frame = infer_frame()?;
    let mut program = Front2Program::new(names, &frame, files, alloc).map_err(RunError::Front2)?;
    build_with(&mut program, texts.overflow, alloc)
}

/// `blaze run`'s call on the new front end: the program of `files` built with a STUB for each
/// reached function this build of blaze cannot make ([`build_stubbed`]), placed and its entry
/// called. `Ok` is the program's own status with the two counts the command's closing note
/// prints ([`Ran`]); a called stub is [`RunError::NotBuilt`] naming that function's refusal.
pub fn run_program(files: &[SourceFile<'_>], root: &str, host: Option<NonNull<c_void>>, new: NewFront<'_>, alloc: blaze_own::SharedAlloc<'static>) -> Result<Ran, RunError> {
    run_program_given(files, root, Given { host, args: None }, new, alloc)
}

/// `blaze run`'s first question of the program of `files` (crate root `root`, package `new`,
/// built with the profile's `panic`): WHICH START it has, as the launcher's form
/// ([`special::EntryForm`]), from the cheapest facts that decide it, before any body is
/// lowered, checked or built. Every program runs in the launcher's child (run.md, "Every
/// program runs in the child"); the form says how the child starts its entry.
///
/// One rule, [`crate::front2::program::start_of`] (through [`entry_start_of`]), so the run and
/// the build cannot disagree:
/// - the package's edition alone, first: a blaze edition's `std` is bstd, which links no
///   `#[lang = "start"]` (start.rs `may_link_lang_start`), and its root's `fn main` is always
///   expanded by `bstd::main` into the exported hosted entry (D104), so its form is
///   [`special::EntryForm::Hosted`] with no file read and no name resolved (a root that exports
///   none is refused by the walk's roots, `Front2Refusal::NoHostedStart`, never started);
/// - otherwise the program's names, resolved as the run resolves them, and [`entry_start_of`]
///   over them: crate 0's items (its exports), its root's attributes, the linked crates' lang
///   rows, and for a Rust start `main`'s head (its return type is `lang_start`'s `T`).
///
/// `alloc` is the caller's, handed down from its entry: the names live in it for the call.
pub fn entry_start(files: &[SourceFile<'_>], root: &str, new: NewFront<'_>, panic: Panic, alloc: Alloc<'static>) -> Result<special::EntryForm, RunError> {
    let edition = edition::PackageEdition::of_manifest(new.edition).map_err(|_| RunError::Edition { written: new.edition.into() })?;
    if !crate::front2::start::may_link_lang_start(crate::crates::program_implicit(edition).std) {
        return Ok(special::EntryForm::Hosted);
    }
    let tc = crate::library_set::toolchain_for(files, root, new, panic, alloc)?;
    let texts = ProgramTexts::new(files, root, new, &tc, panic)?;
    let user = texts.user();
    let frame = infer_frame()?;
    let erased = blaze_own::Erased::new(&frame);
    let inputs = Inputs::new(&user, &tc).map_err(RunError::Crates)?;
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, erased.bump()).map_err(|e| RunError::Names { shown: std::format!("{e}").into_boxed_str() })?;
    Ok(entry_start_of(&names, &frame, alloc, alloc)?.form)
}

/// The run's roots over any names source (run/special.rs [`special::roots`]): `blaze run`'s
/// parent asks it over the names RECORDS with nothing solved (D124), [`entry_start`] over the
/// solved names. `frame` and `tables` hold the head store `main`'s head is lowered in (for a
/// Rust start); `alloc` holds the roots.
pub fn entry_start_of<'n, 'a, S: crate::front2::source::NamesSource<'n>>(names: S, frame: &'n blaze_mem::Frame, tables: Alloc<'static>, alloc: Alloc<'a>) -> Result<special::Roots<'a>, RunError> {
    special::roots(names, frame, tables, alloc).map_err(RunError::Roots)
}

/// [`run_program`], given the host's table and the command line ([`Given`]). The in-process
/// run: `blaze run` no longer calls it (every program runs in the launcher's child, run.md
/// "Every program runs in the child"); tests and examples still do, and it is deleted at the
/// switch. Handed a Rust program with std, its build ends at
/// [`ran_built_given`]'s typed refusal ([`RunError::OwnProcess`]): no caller hands it one.
pub fn run_program_given(files: &[SourceFile<'_>], root: &str, given: Given, new: NewFront<'_>, alloc: blaze_own::SharedAlloc<'static>) -> Result<Ran, RunError> {
    // The in-process path (D91), as [`build_new`]: inputs Unwind, code built Abort.
    let panic = blaze_build::Panic::Unwind;
    let tc = crate::library_set::toolchain_for(files, root, new, panic, alloc.local())?;
    let texts = ProgramTexts::new(files, root, new, &tc, panic)?;
    let user = texts.user();
    let names_frame = infer_frame()?;
    let names_erased = blaze_own::Erased::new(&names_frame);
    let inputs = Inputs::new(&user, &tc).map_err(RunError::Crates)?;
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, names_erased.bump()).map_err(|e| RunError::Names { shown: std::format!("{e}").into_boxed_str() })?;
    let built = build_stubbed(&names, files, &texts, alloc.local())?;
    ran_built_given(built, given, alloc)
}

/// [`build_over`] for a run that starts although some reached function cannot be built: a
/// function not yet compiled is a stub (docs/v2/plan/design/bottom.md, section 11), and the
/// first call of a stub whose function blaze cannot build stops the program AT THAT CALL with
/// an error naming the function and the refusal (crate::stub; [`RunError::NotBuilt`]).
///
/// This is not a fallback (rule 3): no other route is taken and nothing is skipped quietly. A
/// function that is never called was never owed its code; one that is called and has none
/// stops the program by name; and the run's answer carries how many reached functions are not
/// built ([`Ran::not_built`] of [`Ran::reached`]), which the command prints.
///
/// The program is still refused BEFORE it starts when the finding is its own: its entry did
/// not check (the strict front end names why), or a function or item of its own crate was left
/// out ([`Front2Program::own_left_out`]). Only a library's function blaze cannot check or
/// build is left behind a stub.
pub fn build_stubbed(names: &blaze_scope::resolve::Names<'_>, files: &[SourceFile<'_>], texts: &ProgramTexts, alloc: Alloc<'static>) -> Result<Built, RunError> {
    let frame = infer_frame()?;
    // The survey front leaves a fn it refuses out of the program (each caller's build then
    // names it: [`RunError::LeftOut`]); its list of them is the seam's `left_out`, read there.
    let mut left_out = Vec::new_in(Global);
    let mut program = Front2Program::survey(names, &frame, files, &mut left_out, alloc).map_err(RunError::Front2)?;
    if program.entry().is_none() {
        // The entry did not check or could not be tabled: the strict front end stops at
        // exactly that, and its refusal is the answer.
        return Err(match Front2Program::new(names, &frame, files, alloc) {
            Err(refusal) => RunError::Front2(refusal),
            Ok(_) => RunError::NoMain,
        });
    }
    if let Some((item, why)) = program.own_left_out() {
        return Err(RunError::Front2(Front2Refusal::OwnLeftOut { item, why }));
    }
    // Placed in this process as literal-word code (place.rs), which carries no landing pads:
    // built Abort, with D91's stop at a raise, until run.md piece 13 deletes this path.
    let built = build_from(&mut program, None, None, true, texts.overflow, IN_PROCESS_UB, blaze_build::Panic::Abort, alloc)?;
    built.ok_or(RunError::NoMain)
}

/// [`build`], the `blaze::` attributes the front end read put in `blaze`.
fn build_noting(files: &[SourceFile<'_>], root: &str, blaze: &mut BlazeAttrs, overflow: OverflowChecks, alloc: Alloc<'static>) -> Result<Built, RunError> {
    let mut program = Front::new(files, root, blaze, alloc).map_err(|refusal| {
        let mut refusals = Vec::with_capacity_in(1, Global);
        refusals.push(refusal);
        let shown = plain(files, &refusals).into_boxed_str();
        RunError::Lower { refusals, shown }
    })?;
    build_with(&mut program, overflow, alloc)
}

/// The back half over any front end (crate::seam): its bodies checked, consts and statics
/// evaluated, instances built from the entry forward, emitted, with the profile's `overflow`.
/// `alloc` is the caller's: the build's tables and scratch live in it.
pub fn build_with<P: FrontEnd>(program: &mut P, overflow: OverflowChecks, alloc: Alloc<'static>) -> Result<Built, RunError> {
    // Only a survey goes on with no entry: a run's `None` is no `main`. Placed in this process
    // (place.rs): built Abort with D91's stop, until run.md piece 13 deletes this path.
    build_from(program, None, None, false, overflow, IN_PROCESS_UB, blaze_build::Panic::Abort, alloc)?.ok_or(RunError::NoMain)
}

/// What `ub_checks()` is on the in-process paths ([`build_with`], [`build_survey`], the
/// strict front's build), which make no records and keep no key: off, as the record path's
/// baseline settings (code_records.rs `Settings::baseline`: no debug assertions), until run.md
/// piece 13 deletes these paths.
const IN_PROCESS_UB: blaze_build::UbChecks = blaze_build::UbChecks::Off;

/// The run path's code records (code_records.rs; run.md pieces 3 and 7): every instance the
/// entry reaches, each as one entry in `records.store` under its full key, computed before the
/// work. An instance whose key the store holds is not built: its entry's declared instances are
/// what the walk goes on to. A made instance's record names every target by symbol or C name;
/// nothing is placed or run. `records.met` lists each instance met, in the walk's order, then
/// the thread-local getter when the program has thread-locals: one function a program, made as
/// its own record (`RecordRun::getter`) since no caller declares it. The program's vtables and
/// statics are data, not records here: placed by the launcher's piece.
/// The walk's overflow setting and panic strategy are the ones the records are keyed by
/// (`records.builder`): the key is where the record path's strategy lives.
pub fn build_records<P: FrontEnd>(program: &mut P, records: &mut crate::code_records::RecordRun<'_>, alloc: Alloc<'static>) -> Result<(), RunError> {
    let overflow = records.builder.settings.overflow_checks;
    let panic = crate::panic_strategy::of_strategy(records.builder.settings.panic);
    let ub = blaze_build::UbChecks::of_debug_assertions(records.builder.settings.debug_assertions);
    build_from(program, None, Some(records), false, overflow, ub, panic, alloc)?;
    Ok(())
}

/// [`build_records`] over the new front end, the program read as [`build_new`] reads it: the
/// run path of `blaze run` on front2 (run.md piece 3).
pub fn build_records_new(files: &[SourceFile<'_>], root: &str, new: NewFront<'_>, records: &mut crate::code_records::RecordRun<'_>, alloc: Alloc<'static>) -> Result<(), RunError> {
    let key = records.builder.settings.overflow_checks;
    if new.overflow != key {
        return Err(RunError::OverflowDisagrees { walk: new.overflow, key });
    }
    // The record path: the program's inputs are made from the key's strategy, so the code and
    // the key it is filed under cannot disagree (rule 21) and nothing compares two copies.
    let panic = crate::panic_strategy::of_strategy(records.builder.settings.panic);
    let tc = crate::library_set::toolchain_for(files, root, new, panic, alloc)?;
    let texts = ProgramTexts::new(files, root, new, &tc, panic)?;
    let user = texts.user();
    let frame = infer_frame()?;
    let erased = blaze_own::Erased::new(&frame);
    let inputs = Inputs::new(&user, &tc).map_err(RunError::Crates)?;
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, erased.bump()).map_err(|e| RunError::Names { shown: std::format!("{e}").into_boxed_str() })?;
    let mut program = Front2Program::new(&names, &frame, files, alloc).map_err(RunError::Front2)?;
    build_records(&mut program, records, alloc)
}

/// What [`build_survey`] tried: instances built clean, instances tried, and the generic fns no
/// built body called at a ground instance (never built: "no instance"), by name.
pub struct SurveyCounts {
    pub built: usize,
    pub tried: usize,
    pub no_instance: Vec<Box<str>, Global>,
}

/// What the back half builds of a program, never run: EVERY clean fn, not only what the entry
/// reaches: the entry, then each non-generic fn with a checked body as an instance of its own
/// (in key order), then each generic fn at every ground instance a built body calls it at. Each
/// is built as the run builds it, but a refusal (the builder's, a callee's resolution or
/// absence, the emitter's) is filed in `refused` (the fn, the refusal's typed name) and the
/// walk goes on, so one refusal hides nothing behind it but the callees of the refused fn.
pub fn build_survey<P: FrontEnd>(program: &mut P, refused: &mut Vec<(Box<str>, Box<str>), Global>, overflow: OverflowChecks, alloc: Alloc<'static>) -> Result<SurveyCounts, RunError> {
    let mut counts = SurveyCounts { built: 0, tried: 0, no_instance: Vec::new_in(Global) };
    // As the in-process run builds (D91): Abort, until run.md piece 13.
    build_from(program, Some((refused, &mut counts)), None, false, overflow, IN_PROCESS_UB, blaze_build::Panic::Abort, alloc)?;
    Ok(counts)
}

/// In a survey, `e` filed by its fn and typed name (a callee's prefixed `callee:`, the
/// emitter's `emit:`) and the walk goes on; in the stub build, `e` kept as its slot's refusal
/// ([`Stubs::keep`]) and the walk goes on; in a strict run, `e` stops it.
fn filed(sink: &mut Sink<'_>, e: RunError) -> Result<(), RunError> {
    if sink.stub_refusals
        && let Some(stubs) = sink.stubs.as_mut()
    {
        if !stubbed(&e) {
            return Err(e);
        }
        stubs.keep(e);
        return Ok(());
    }
    let Some(out) = sink.survey.as_deref_mut() else { return Err(e) };
    let (function, class) = match &e {
        // The builder's refusal with its own words (a `Malformed`'s `what`), cut to one line.
        RunError::Build { function, error, .. } => {
            let said = format!("{}: {error}", class_of(&format!("{error:?}")));
            (function, Box::from(said.char_indices().nth(200).map_or(&*said, |(i, _)| &said[..i])))
        }
        // By the variant and the item's name, so each refused item is one class.
        RunError::ItemRefused { function, error, item, path } => {
            let shown = path.as_deref().map_or_else(|| format!("{:#x}", item.0), |p| format!("`{p}`"));
            (function, format!("{}: {shown}", class_of(&format!("{error:?}"))).into_boxed_str())
        }
        RunError::NoAdt { function, adt, path } => {
            let shown = path.as_deref().map_or_else(|| format!("{:#x}", adt.0), |p| format!("`{p}`"));
            (function, format!("NoAdt: no row for the ADT {shown}").into_boxed_str())
        }
        RunError::Emit { function, error } => (function, crate::emit_class::emit_class(&error)),
        // A soft-float routine not bound is the emitter's (legalize's) refusal: filed at the
        // tier with the routine and why (builtins_shim.rs `BuiltinsRefused`).
        RunError::CodeRecord { function, error: crate::code_records::CodeRecordError::Routine(refused) } => (function, format!("emit: {refused}").into_boxed_str()),
        RunError::CodeRecord { function, error } => (function, format!("code record: {}", class_chain(&format!("{error:?}"), 2)).into_boxed_str()),
        // By the callee's key, so each fn the walk never reached is its own class.
        RunError::Callee { function, callee, path, gap, .. } => {
            let shown = path.as_deref().map_or_else(|| format!("{:#x}", callee.0), |p| format!("`{p}` ({:#x})", callee.0));
            (function, format!("callee {shown}: {gap}").into_boxed_str())
        }
        RunError::Projection { function, why } => (function, format!("Projection: {why}").into_boxed_str()),
        // Grouped by the item and its cause, so each left-out item is one class.
        RunError::LeftOut { function, item, path, why } => {
            let shown = path.as_deref().map_or_else(|| format!("{:#x}", item.0), |p| format!("`{p}`"));
            let said = format!("left out: {shown}: {why}");
            (function, Box::from(said.char_indices().nth(240).map_or(&*said, |(i, _)| &said[..i])))
        }
        // By the constant and the evaluator's reason, so each refused constant is one class.
        // Cut at 480: the need and the place take some 200 characters before the reason
        // (census g054702 cut "a body it needs: .." at its first letters, 32 rows).
        RunError::ConstAt { function, refusal, path } => {
            let said = match path {
                Some(path) => format!("const at the instance: `{path}`: {refusal}"),
                None => format!("const at the instance: {refusal}"),
            };
            (function, Box::from(said.char_indices().nth(480).map_or(&*said, |(i, _)| &said[..i])))
        }
        // By the error's variant chain (`Check(Solve(Overflow`) and the callee's name, so each
        // cause at each trait fn is one class and the cause is not cut at its first field.
        RunError::Resolve { function, callee, path, at, error } => {
            let shown = path.as_deref().map_or_else(|| format!("{:#x}", callee.0), |p| format!("`{p}`"));
            (function, format!("callee: resolve {} at {at}: {shown}", class_chain(&format!("{error:?}"), 3)).into_boxed_str())
        }
        // By the shape and the `Fn*` method: one class per built-in rule not built.
        RunError::NoBuiltinCall { function, callee, path, shape, .. } => {
            let shown = path.as_deref().map_or_else(|| format!("{:#x}", callee.0), |p| format!("`{p}`"));
            (function, format!("callee: no built-in Fn* call at {shape}: {shown}").into_boxed_str())
        }
        RunError::Vtable { function, why, .. } => (function, format!("vtable: {}", class_of(&format!("{why:?}"))).into_boxed_str()),
        _ => return Err(e),
    };
    out.push((format!("{function}").into_boxed_str(), class));
    Ok(())
}

/// The refusal of `function`'s call of `callee`, which is neither a tabled body nor an outside
/// symbol: the survey's cause where it left the callee out ([`RunError::LeftOut`]), else
/// [`RunError::Callee`] (a fn the walk never reached: reach.rs's `Instances` did not table it).
fn callee_refusal<P: FrontEnd>(program: &P, function: Named, at: List, callee: Instance, gap: crate::callee_shown::CalleeGap, ground: PTys<'_>) -> RunError {
    match program.left_out(callee.item) {
        Some(why) => RunError::LeftOut { function, item: callee.item, path: program.item_named(callee.item), why: Box::from(why) },
        None => {
            let name = |k: ItemKey| program.item_named(k);
            RunError::Callee {
                function,
                at: crate::callee_shown::args_shown(ground, at, &name),
                callee: callee.item,
                path: program.item_named(callee.item),
                args: crate::callee_shown::args_shown(ground, callee.args, &name),
                gap,
            }
        }
    }
}

/// Why a vtable was not made, named.
#[derive(Debug)]
pub enum VtableFail {
    /// The object trait's shape: no facts for a trait on its supertrait tree, or too large.
    Shape(blaze_build::vtable::VtableError),
    /// The concrete type's size and alignment.
    Layout(blaze_build::layout::LayoutError),
    Build(BuildError),
    /// The drop word needs `drop_in_place::<T>` and the program has none.
    NoDropGlue,
}

/// The program's vtables, each made once: `refs[n]` is what `images[n]` is the table of.
struct Tables {
    refs: Vec<VtableRef, Global>,
    images: Vec<VtableImage, Global>,
}

/// The reached instances: slot `n` is `instances[n]`, of fn `tabled[n]`, told apart by `keys[n]`
/// (`None`: a reference the stub build could not resolve or key, in a slot of its own).
struct Reached {
    instances: Vec<Instance, Global>,
    tabled: Vec<usize, Global>,
    keys: Vec<Option<InstanceKey>, Global>,
    /// The stub build's unresolved references ([`Reached::unresolved`]): each one's slot and
    /// refusal, in slot order; the walk takes a slot's refusal when it comes to that slot.
    pre: Vec<(usize, Option<RunError>), Global>,
    /// How many of `pre` the walk has passed.
    pre_taken: usize,
}

impl Reached {
    /// `callee`'s slot (of fn `c`, key `key`): the one reached before under that key, else a new
    /// one at the end, built in its turn.
    fn slot(&mut self, callee: Instance, c: usize, key: InstanceKey) -> Result<u32, RunError> {
        let slot = match self.keys.iter().position(|&known| known == Some(key)) {
            Some(slot) => slot,
            None => {
                self.instances.push(callee);
                self.tabled.push(c);
                self.keys.push(Some(key));
                self.instances.len() - 1
            }
        };
        u32::try_from(slot).map_err(|_| RunError::TooManyInstances)
    }

    /// A slot of its own for `called`, a reference of fn `caller` that has no instance (it did
    /// not resolve, or has no key): the slot holds a stub with `refusal`, so the caller is built
    /// and only a call through this reference stops the program.
    fn unresolved(&mut self, called: Instance, caller: usize, refusal: RunError) -> Result<u32, RunError> {
        let slot = self.instances.len();
        self.instances.push(called);
        self.tabled.push(caller);
        self.keys.push(None);
        self.pre.push((slot, Some(refusal)));
        u32::try_from(slot).map_err(|_| RunError::TooManyInstances)
    }

    /// The refusal `slot` was made with, once: `Some` only for a slot of [`Reached::unresolved`].
    fn refusal_of(&mut self, slot: usize) -> Option<RunError> {
        let (at, refusal) = self.pre.get_mut(self.pre_taken)?;
        if *at != slot {
            return None;
        }
        self.pre_taken += 1;
        refusal.take()
    }
}

/// Where the walk of [`build_from`] puts a refusal it can go on from ([`filed`]): nowhere (a
/// strict build stops at it), a survey's list, or the stub build's table.
struct Sink<'a> {
    survey: Option<&'a mut Vec<(Box<str>, Box<str>), Global>>,
    /// The stubs of a build that runs (the stub build's and a strict one's): a strict build
    /// keeps only the stops no build can avoid yet (an unwind's raise, D91).
    stubs: Option<Stubs>,
    /// Whether a refusal of one instance becomes its stub ([`build_stubbed`]) rather than the
    /// strict build's stop.
    stub_refusals: bool,
}

/// The stub build's refusals ([`build_stubbed`]): each instance that could not be built keeps
/// its FIRST refusal, and its slot holds a stub with that refusal's number (crate::stub).
struct Stubs {
    /// By stub number ([`Built::refused`]).
    refused: Vec<RunError, Global>,
    /// The slot the walk is at.
    slot: usize,
    /// The slot `refused`'s last is of.
    last: Option<usize>,
}

impl Stubs {
    /// `refusal` kept as the walk's slot's, unless that slot has one already.
    fn keep(&mut self, refusal: RunError) {
        if self.last != Some(self.slot) {
            self.refused.push(refusal);
            self.last = Some(self.slot);
        }
    }

    /// The stub for `slot`, the last slot refused; `None` when `slot` kept no refusal.
    fn body(&self, slot: usize) -> Option<Body> {
        if self.last != Some(slot) {
            return None;
        }
        let number = u32::try_from(self.refused.len().checked_sub(1)?).ok()?;
        let mut code = Code::new_in(Global);
        code.bytes.extend_from_slice(&crate::stub::code(number));
        code.fixups.push(blaze_emit::Fixup { at: crate::stub::STOPS_AT, place: 0 });
        code.fixups.push(blaze_emit::Fixup { at: crate::stub::STOP_AT, place: 1 });
        let mut targets = Vec::with_capacity_in(2, Global);
        targets.push(Target::Stops);
        targets.push(Target::Stop);
        Some(Body { code, targets, statics: Vec::new_in(Global), tls: Vec::new_in(Global), vtables: Vec::new_in(Global), positions: Vec::new_in(Global) })
    }
}

/// A call of `called` by fn `caller` that has no instance (`refusal`): in the stub build a slot
/// of its own holding a stub (`Some`: the caller is built, and stops only if it makes this
/// call); else `refusal` goes to [`filed`] and the caller is left unbuilt (`None`).
fn unresolved(sink: &mut Sink<'_>, reached: &mut Reached, called: Instance, caller: usize, refusal: RunError) -> Result<Option<u32>, RunError> {
    if sink.stub_refusals && sink.stubs.is_some() && stubbed(&refusal) {
        return reached.unresolved(called, caller, refusal).map(Some);
    }
    filed(sink, refusal)?;
    Ok(None)
}

/// The stub build's own error: a slot of `function` left with neither a body nor a refusal, so
/// slot `n` would not be `bodies[n]`.
fn unstubbed(function: Named) -> RunError {
    RunError::Build { function, error: BuildError::Malformed { what: "a slot the stub build left with neither a body nor a refusal" }, range: None }
}

/// Whether `e` is a refusal of ONE instance (the walk can go on past it): what a survey files
/// and the stub build puts behind a stub. Any other error is the whole build's.
fn stubbed(e: &RunError) -> bool {
    matches!(
        e,
        RunError::Build { .. }
            | RunError::ItemRefused { .. }
            | RunError::NoAdt { .. }
            | RunError::Emit { .. }
            | RunError::CodeRecord { .. }
            | RunError::Callee { .. }
            | RunError::Projection { .. }
            | RunError::LeftOut { .. }
            | RunError::ConstAt { .. }
            | RunError::Resolve { .. }
            | RunError::NoBuiltinCall { .. }
            | RunError::Vtable { .. }
    )
}

/// A refusal's typed name: its `Debug` form up to its first field.
fn class_of(debug: &str) -> Box<str> {
    Box::from(&debug[..debug.find(['(', ' ', '{']).unwrap_or(debug.len())])
}

/// `None` only in a survey whose program has no entry (its `main` did not check): every clean
/// fn is still built, and no image is made.
///
/// With `records` ([`build_records`]): each instance is made as a code record into its store
/// (or found there by key and not built), no body is kept, and the answer is `None`.
///
/// With `stub_refusals` ([`build_stubbed`]; never with a survey or `records`): an instance the
/// walk cannot build keeps its first refusal and its slot holds a stub (crate::stub), and a
/// reference that has no instance gets a stub slot of its own ([`unresolved`]): the answer's
/// `refused` lists them by stub number.
fn build_from<P: FrontEnd>(
    program: &mut P,
    surveyed: Option<(&mut Vec<(Box<str>, Box<str>), Global>, &mut SurveyCounts)>,
    mut records: Option<&mut crate::code_records::RecordRun<'_>>,
    stub_refusals: bool,
    overflow: OverflowChecks,
    ub: blaze_build::UbChecks,
    panic: blaze_build::Panic,
    // The caller's, for the build's tables (ground types, layouts, closures, coroutines) and
    // each instance's scratch: none outlives the build.
    alloc: Alloc<'static>,
) -> Result<Option<Built>, RunError> {
    let (survey, mut counts) = match surveyed {
        Some((refused, counts)) => (Some(refused), Some(counts)),
        None => (None, None),
    };
    // Every build that runs has a stub table: a strict one for the raise's stop alone (D91). A
    // build runs when it is neither a survey nor a record walk (a record walk only writes
    // records and keeps no body, so it has no slots to stub).
    let stubs = (stub_refusals || (survey.is_none() && records.is_none())).then(|| Stubs { refused: Vec::new_in(Global), slot: 0, last: None });
    let mut survey = Sink { survey, stubs, stub_refusals };
    let entry = match (program.entry(), survey.survey.is_some()) {
        (Some(main), _) => Some(main),
        (None, true) => None,
        (None, false) => return Err(RunError::NoMain),
    };
    // A survey builds what the entry reaches whatever its shape (his `main` takes `cx, io`).
    let start = match (survey.survey.is_some(), entry) {
        (false, Some(main)) => entry_shape(&*program, main)?,
        _ => Start::Plain(Entry::Unit),
    };
    if let Some(main) = entry
        && program.function(main).body.is_none()
    {
        return Err(RunError::MainShape { main: program.function(main).named(), what: "it has no body" });
    }
    let checked = program.check_all()?;
    // Kept mutable: between instances the loop pulls the constants the next instance needs at
    // its ground arguments (`FrontEnd::const_at`, front2/const_at.rs); `program` is a shared
    // borrow of it, taken again in each step of the loop and after it.
    let program_mut = program;
    let program = &*program_mut;
    let mut ground = PortableTys::new_in(alloc);
    let mut static_instances = blaze_hash::HashMap::new_in(alloc);
    let consts = eval_consts(program, &checked, &mut ground, &mut static_instances, alloc)?;
    let langs = BackLangs::of(program)?;
    // `Clone::clone` at a tuple, closure, fn item or fn pointer: built (clone_shim.rs).
    let clones = crate::clone_shim::Clones::of(program).map_err(RunError::Lang)?;
    let (mut image, mut tls_vars) = static_image(program, &consts, survey.survey.as_deref_mut())?;
    // The rows the descriptor holds: a thread-local first numbered past them has none.
    let tls_rows = tls_vars.as_ref().map_or(0, TlsVars::len);
    // The program's compiler_builtins soft-float routines (front2/builtins_shim.rs), found once
    // a build and bound in every instance as the record walk's maker binds them
    // (run/make_one.rs `Work::routines`): one rule for both paths. A front end that does not
    // look them up answers `NotLookedUp`, and an `f128` operation is refused naming that.
    let builtins = program.soft_float_routines(alloc);
    let bound = crate::front2::builtins_shim::Bound::new(builtins, &ground, alloc).map_err(|_| RunError::OneBody {
        // `Bound::new` fails only naming a found routine's instance, so one is found.
        key: blaze_lir::legalize::SoftFloat::ALL.into_iter().find_map(|r| builtins.lib(r).map(crate::front2::keys::checker_of)).unwrap_or(ItemKey(0)),
        why: "a compiler_builtins soft-float routine's instance name did not encode (front2/builtins_shim.rs `Bound::new`)",
    })?;
    // Each found routine's slot, made the first time an instance that legalizes is emitted.
    let mut routine_slots: [Option<u32>; blaze_lir::legalize::SoftFloat::ALL.len()] = [None; blaze_lir::legalize::SoftFloat::ALL.len()];
    // An item's code identity, until the item layer's records give one (shortcuts 2 and 12): its
    // key and its item's text. Never stale; coarse only in that a signature edit changes it too.
    // The key covers what each projection of the fn's checked types normalizes to at the
    // instance (`instance_key_normalized`): two instances that differ only there never share
    // one. A closure's `Fn*` method instance keys its owner's code at the owner's arguments,
    // then the method and the instance's own arguments (`blaze_build::closure_method_key_args`).
    let mut checked_bodies = Bodies {
        checked: &checked,
        closures: ClosureTable::of(program, &checked, alloc),
        coroutines: crate::front2::coroutines::Coroutines::of(program, &checked, alloc),
        const_at: crate::front2::const_at::ConstTable::new_in(Global),
        overflow,
        ub,
        panic,
        alloc,
    };
    // Each coroutine instance built, its three functions waiting for their slots.
    let mut coroutine_stash: Vec<BuiltParts, Global> = Vec::new_in(Global);
    // The instance key over the code identity `code`: `key_of`'s with the item's text (which
    // tells instances apart in this run), a record key's with the item's identity alone (its
    // ground facts: code_records.rs, part 3; the body is in the record key's body digest).
    // `program` is a parameter, not captured: the loop takes `program_mut` between instances.
    let key_with = |program: &P, code: ContentId, c: usize, instance: Instance, ground: &mut PortableTys<'_>, facts: &dyn LayoutFacts| {
        let fail = |error| match error {
            // A key's projection the seam did not answer: named by its cause.
            BuildError::Projection { .. } if let Some(why) = program.last_projection() => RunError::Projection { function: program.function(c).named(), why },
            error => RunError::Build { function: program.function(c).named(), error, range: None },
        };
        let from = checked.get(c).and_then(Option::as_ref).map_or(PTys::EMPTY, |d| d.types.view());
        let args = if let Some(shim) = clones.shim_of(instance, ground) {
            // A builtin clone, keyed by `Self` and whether it is `Copy` (clone_shim.rs).
            clones.key_args(shim, ground).map_err(fail)?
        } else if instance.item == program.function(c).key {
            instance.args
        } else if let Some(target) = blaze_build::reify_shim_of(instance, ground.view()) {
            blaze_build::reify_shim_key_args(target, ground, alloc).map_err(fail)?
        } else if crate::front2::coroutines::self_is_coroutine(instance, ground) {
            // One of a coroutine's three functions, tabled as its owner (front2/coroutines.rs).
            crate::front2::coroutines::key_args(instance, ground, alloc).map_err(fail)?
        } else if crate::fn_shim::self_is_fn(instance, ground) {
            crate::fn_shim::fn_shim_key_args(instance, ground).map_err(fail)?
        } else {
            blaze_build::closure_method_key_args(instance, ground, alloc).map_err(fail)?
        };
        blaze_build::instance_key_normalized(code, args, from, ground, facts, alloc).map_err(fail)
    };
    let key_of = |program: &P, c: usize, instance: Instance, ground: &mut PortableTys<'_>, facts: &dyn LayoutFacts| key_with(program, program.code_of(c), c, instance, ground, facts);
    // Slot `n` is the `n`th instance reached; `keys[n]` its key, by which instances are told apart.
    let mut reached = Reached {
        instances: Vec::with_capacity_in(program.fn_count(), Global),
        tabled: Vec::with_capacity_in(program.fn_count(), Global),
        keys: Vec::with_capacity_in(program.fn_count(), Global),
        pre: Vec::new_in(Global),
        pre_taken: 0,
    };
    let mut tables = Tables { refs: Vec::new_in(Global), images: Vec::new_in(Global) };
    let mut layouts = Layouts::new_in(alloc);
    if let Some(main) = entry {
        reached.instances.push(Instance { item: program.function(main).key, args: List::EMPTY });
        reached.tabled.push(main);
        // The entry's key reads its body's types' facts (a `Drop` impl at each): an answer no
        // check made is made (`FrontEnd::make_asked`) and the key asked again, as an
        // instance's step is (at most `MOST_ROUNDS`).
        let mut rounds = 0;
        let key = loop {
            let program = &*program_mut;
            let keyer = FrontAdts(program, main, &consts, &langs, &checked_bodies);
            match key_of(program, main, Instance { item: program.function(main).key, args: List::EMPTY }, &mut ground, &keyer) {
                Err(e) if asks_again(&e) && rounds < crate::front2::check::MOST_ROUNDS => {
                    if !program_mut.make_asked(main)? {
                        return Err(e);
                    }
                    rounds += 1;
                }
                made => break made?,
            }
        };
        reached.keys.push(Some(key));
    }
    // The program's borrows again, after the entry's makes.
    let program = &*program_mut;
    let keyer = FrontAdts(program, entry.unwrap_or(0), &consts, &langs, &checked_bodies);
    // A survey seeds every non-generic fn with a checked body, by key (the program's order,
    // never the asking's); a generic one is met at the instances built bodies call it at.
    let generic = |program: &P, c: usize| {
        let sig = program.sig(c);
        sig.parent_count != 0 || sig.own_count != 0
    };
    if counts.is_some() {
        let mut seeds: Vec<(ItemKey, usize), Global> = Vec::new_in(Global);
        for c in 0..program.fn_count() {
            if Some(c) != entry && program.function(c).body.is_some() && checked[c].is_some() && !generic(program, c) {
                seeds.push((program.function(c).key, c));
            }
        }
        seeds.sort_unstable_by_key(|s| s.0);
        for (item, c) in seeds {
            let instance = Instance { item, args: List::EMPTY };
            let key = match key_of(program, c, instance, &mut ground, &keyer) {
                Ok(k) => k,
                Err(e) => {
                    filed(&mut survey, e)?;
                    continue;
                }
            };
            reached.instances.push(instance);
            reached.tabled.push(c);
            reached.keys.push(Some(key));
        }
    }
    // A build that runs: each fn a static points at is reached as the entry is (a static's fn
    // pointer is called through the static, which no walk step sees). The transient image's key
    // is the evaluator's key, so resolve it to the instance and normalize it exactly as the
    // build's own slot key before placement. A missing descriptor or untabled body remains a
    // static-fn stub and keeps the named first-call refusal in [`static_fn_slots`].
    if survey.stubs.is_some()
        && let Some(image) = image.as_mut()
    {
        let mut canonical = blaze_hash::HashMap::new_in(alloc);
        for reloc in &mut image.relocs {
            let ImageTarget::Fn(key) = reloc.target else { continue };
            let reached_key = match canonical.get(&key) {
                Some(&known) => known,
                None => {
                    let target = if program.evaluates_at_args() {
                        program.const_fn_instance(key, &mut ground, alloc)
                    } else {
                        static_instances.get(&key).copied()
                    };
                    let Some(mut instance) = target else { continue };
                    if program.track_caller(instance.item) == Some(true) {
                        instance = blaze_build::reify_shim_instance(instance, &mut ground)
                            .map_err(|error| RunError::Build { function: program.function(entry.unwrap_or(0)).named(), error, range: None })?;
                    }
                    let tabled = make_one::static_fn_tabled(program, instance, ground.view());
                    let Some(c) = program.fn_index(tabled) else { continue };
                    let facts = FrontAdts(program, c, &consts, &langs, &checked_bodies);
                    let normalized = key_of(program, c, instance, &mut ground, &facts)?;
                    reached.slot(instance, c, normalized)?;
                    canonical.insert(key, normalized);
                    normalized
                }
            };
            reloc.target = ImageTarget::Fn(reached_key);
        }
    }
    let mut bodies = Vec::with_capacity_in(program.fn_count(), Global);
    let mut args = Vec::new_in(Global);
    let mut at = 0;
    // The constants each instance reads at its ground arguments (front2/const_at.rs), refilled
    // per instance.
    let mut const_needs = crate::front2::const_at::Needs::new_in(Global);
    // The slot built again after `FrontEnd::make_asked` made what its world asked, and how
    // many times (bounded by `MOST_ROUNDS`, rule 18).
    let mut asked_rounds: (usize, u32) = (usize::MAX, 0);
    while let Some(&instance) = reached.instances.get(at) {
        // Shared borrows of the program for this step only: the step takes `program_mut` to
        // pull its instance's constants before the walk.
        let program = &*program_mut;
        let keyer = FrontAdts(program, entry.unwrap_or(0), &consts, &langs, &checked_bodies);
        at += 1;
        // The stub build: slot `n` is `bodies[n]`, so a slot whose step kept a refusal and left
        // no body gets its stub before the next slot's step; a slot made for a reference with
        // no instance ([`Reached::unresolved`]) is its refusal and nothing else.
        if let Some(stubs) = survey.stubs.as_mut() {
            if bodies.len() + 1 < at {
                bodies.push(stubs.body(at - 2).ok_or_else(|| unstubbed(program.function(reached.tabled[at - 2]).named()))?);
            }
            if bodies.len() + 1 != at {
                return Err(unstubbed(program.function(reached.tabled[at - 1]).named()));
            }
            stubs.slot = at - 1;
            if let Some(refusal) = reached.refusal_of(at - 1) {
                stubs.keep(refusal);
                continue;
            }
        }
        // Each instance is pushed with its fn's index (`tabled`), so none is looked up again.
        // If this is a generic `inline for` fn instantiated at concrete args, switch to its
        // monomorphized unrolled specialization row (with 0 generic args of its own).
        let orig_i = reached.tabled[at - 1];
        let spec_i = if instance.args.len > 0 && instance.item == program.function(orig_i).key {
            ground.view().list(instance.args).ok().and_then(|a| program.specialized_row(orig_i, a, ground.view()))
        } else {
            None
        };
        let i = spec_i.unwrap_or(orig_i);
        let is_own_instance = spec_i.is_some() || instance.item == program.function(i).key;
        // This step's refusal `e`, filed, and the loop goes on; but an instance question's
        // `Missing` (an answer no check made: a bucket at a shape only this instance's
        // arguments hold) has the front end make what its worlds asked
        // (`FrontEnd::make_asked`) and the step is taken again, at most `MOST_ROUNDS` times, as
        // the body check retries its world (front2 check.rs). Nothing made: `e` is filed.
        macro_rules! step_refused {
            ($e:expr) => {{
                let e = $e;
                let rounds = if asked_rounds.0 == at { asked_rounds.1 } else { 0 };
                if asks_again(&e) && rounds < crate::front2::check::MOST_ROUNDS {
                    match program_mut.make_asked(i) {
                        Ok(true) => {
                            asked_rounds = (at, rounds + 1);
                            at -= 1;
                            continue;
                        }
                        Ok(false) => filed(&mut survey, e)?,
                        Err(made) => filed(&mut survey, made)?,
                    }
                } else {
                    filed(&mut survey, e)?;
                }
                continue;
            }};
        }
        let function = program.function(i);
        let name = || function.named();
        // A tuple struct's or variant's ctor, tabled with no body: the function the language
        // makes for its name as a value (blaze-build ctor_fn.rs), its own instance or its `Fn*`
        // shim's (fn_shim.rs tables a fn item's shim at the item's row). Its variant.
        let ctor_row = match (function.kind, function.body) {
            (FnKind::Ctor, None) => program.ctor_of(function.key).map(|(_, variant)| variant),
            _ => None,
        };
        // `Clone::clone` at a `Self` the language gives it (clone_shim.rs), tabled at the
        // trait's bodiless row: its `Self` and whether it is `Copy`, asked where it was called.
        let clone_row = match function.body {
            None => clones.shim_of(instance, &ground).map(|s| (s, clones.copy_of(s))),
            Some(_) => None,
        };
        // Builtin `From::from` for structural error unions (clone_shim.rs `language_from`).
        let from_row = match function.body {
            None if clone_row.is_none() => crate::clone_shim::language_from(&*program, instance, &ground),
            _ => None,
        };
        let (body, done) = match (function.body, checked[i].as_ref()) {
            (Some((body, _)), Some(done)) => (Some(body), Some(done)),
            _ if ctor_row.is_some() || clone_row.is_some() || from_row.is_some() => (None, None),
            _ => {
            let refusal = callee_refusal(&*program, name(), instance.args, instance, crate::callee_shown::CalleeGap::NoBody, ground.view());
            // A survey files it (by the fn and the callee) and goes on, as its doc promises; a
            // run and the stub build stop here as before.
            if survey.survey.is_some() {
                filed(&mut survey, refusal)?;
                continue;
            }
            return Err(refusal);
            }
        };
        // The record walk (`build_records`): the instance's full key, before the work. Its entry
        // is looked up once this instance's constants are pulled (below), so its read list can
        // be asked again.
        let record_key = match records.as_deref() {
            None => None,
            Some(run) => {
                let fail = |error| RunError::CodeRecord { function: name(), error };
                let symbol = crate::symbols::fn_symbol(instance.item, instance.args, ground.view(), alloc)
                    .map_err(|e| fail(crate::code_records::CodeRecordError::Symbol(e)))?;
                // A ctor row has no body, so no read list: its code is its fields' layout, which
                // the ground facts below key.
                let reads = match done {
                    Some(done) => Some(program.body_reads(i, done).ok_or_else(|| fail(crate::code_records::CodeRecordError::NoReads { item: function.key }))?),
                    None => None,
                };
                let body_digest = crate::code_records::body_digest(function.text, reads);
                let facts = key_with(program, crate::symbols::item_identity(function.key), i, instance, &mut ground, &keyer)?;
                let parts = crate::code_records::KeyParts { symbol, body: body_digest, ground: facts.0 };
                Some((parts, crate::code_records::code_key(&parts, &run.builder)))
            }
        };
        // The constants and const blocks this instance reads, at its ground arguments, PULLED
        // from the front end before the walk (front2/const_at.rs): the walk's `Adts` only reads
        // them. A plain instance's body only: a closure's, a coroutine's or a shim's instance is
        // built at its owner's arguments, and the owner's instance, built first (it is what
        // reaches them), pulled them. A need the evaluator refuses is this instance's refusal.
        const_needs.clear();
        if is_own_instance
            && program.evaluates_at_args()
            && let (Some(body), Some(done)) = (body, done)
        {
            args.clear();
            if spec_i.is_none() {
                args.extend_from_slice(ground.view().list(instance.args).map_err(|_| RunError::Build { function: name(), error: BuildError::Malformed { what: "an instance's arguments past the ground table" }, range: None })?);
            }
            let view = body.view(program.body_types(i));
            let facts = FrontAdts(program, i, &consts, &langs, &checked_bodies);
            if let Err(error) = crate::front2::const_at::needs_of(&view, done, &args, &mut ground, &facts, alloc, &mut const_needs) {
                filed(&mut survey, RunError::Build { function: name(), error, range: None })?;
                continue;
            }
        }
        let named = function.named();
        if !const_needs.is_empty()
            && let Err(refusal) = crate::front2::const_at::pull(&mut *program_mut, &const_needs, ground.view(), &mut checked_bodies.const_at, &checked_bodies.closures)
        {
            let item = refusal.need.item();
            let path = program_mut.item_named(item);
            filed(&mut survey, RunError::ConstAt { function: named, refusal, path })?;
            continue;
        }
        // The step's borrows again, after the pull.
        let program = &*program_mut;
        let function = program.function(i);
        let name = || function.named();
        let body = function.body.map(|(body, _)| body);
        if body.is_none() && ctor_row.is_none() && clone_row.is_none() && from_row.is_none() {
            return Err(callee_refusal(&*program, named, instance.args, instance, crate::callee_shown::CalleeGap::NoBody, ground.view()));
        }
        // The record walk: the entry its key's pointer names is used only when every row of its
        // read list, asked again through the world the build is handed, answers the same; the
        // instances it declares are then what the walk goes on to, and nothing is built. A row
        // answered otherwise: the entry is stale, built again below and pointed to.
        if let (Some(run), Some((parts, key))) = (records.as_deref_mut(), record_key) {
            use crate::code_records::CodeRecordError;
            use crate::code_records::reads::{Verdict, verify};
            let fail = |error| RunError::CodeRecord { function: name(), error };
            let found = crate::code_records::entry_of(&*run.store, key).map_err(|e| fail(CodeRecordError::Store(e)))?;
            if let Some(bytes) = found {
                let entry = crate::code_records::EntryView::read(bytes).map_err(|e| fail(CodeRecordError::Entry(e)))?;
                let world = FrontAdts(program, i, &consts, &langs, &checked_bodies);
                let mut resolve = |item: ItemKey, args: List, ground: &mut PortableTys<'_>| program.resolve(i, item, args, ground).ok();
                match verify(&entry, &world, &mut resolve, &mut ground, alloc).map_err(|e| fail(CodeRecordError::Reads(e)))? {
                    // The whole program holds every head it has: a row whose record is not made
                    // reads an item gone today, and the entry is built again.
                    Verdict::Stale { .. } | Verdict::Needs { .. } => run.stale += 1,
                    Verdict::Holds => {
                        let mut declared: Vec<(ItemKey, Instance), Global> = Vec::new_in(Global);
                        entry
                            .each_declared(
                                |d| {
                                    let (item, args) = crate::symbols::decode_instance(d.name, &mut ground, alloc).map_err(CodeRecordError::Symbol)?;
                                    declared.push((d.tabled, Instance { item, args }));
                                    Ok(())
                                },
                                CodeRecordError::Entry,
                            )
                            .map_err(fail)?;
                        for &(tabled, callee) in declared.iter() {
                            let c = program.fn_index(tabled).ok_or_else(|| fail(CodeRecordError::NoTabledFn { tabled }))?;
                            // A builtin clone's key holds its `Self: Copy`, asked in this fn's world.
                            if let Some(Err(e)) = clones.owner(program, &world, i, callee, &mut ground, alloc) {
                                return Err(match e {
                                    crate::clone_shim::NoteError::Resolve(error) => {
                                        let at = crate::callee_shown::self_shown(program, ground.view(), callee.args);
                                        RunError::Resolve { function: name(), callee: callee.item, path: program.item_named(callee.item), at, error }
                                    }
                                    crate::clone_shim::NoteError::Build(error) => RunError::Build { function: name(), error, range: None },
                                });
                            }
                            let k = key_of(program, c, callee, &mut ground, &world)?;
                            reached.slot(callee, c, k)?;
                        }
                        run.met.push(crate::code_records::Met { symbol: parts.symbol, key, parts, function: format!("{}", name()).into_boxed_str(), found: true });
                        continue;
                    }
                }
            }
        }
        args.clear();
        let malformed =|_| RunError::Build { function: name(), error: BuildError::Malformed { what: "an instance's arguments past the ground table" }, range: None };
        // A closure's `Fn*` method instance (`[closure, (A..)]`, tabled as its owner): built
        // from the owner's body at the owner's arguments the closure's type holds.
        // A fn item's or fn pointer's `Fn*` method instance: a shim (fn_shim.rs).
        // One of a coroutine's three functions (`Future::poll`, the resume fn, `drop_in_place`,
        // each at `[coroutine]`; tabled as the block's owner): taken from the one build of that
        // coroutine instance (front2/coroutines.rs, `build_coroutines`).
        let reify = if is_own_instance {
            None
        } else {
            blaze_build::reify_shim_of(instance, ground.view())
        };
        let coroutine = if is_own_instance || reify.is_some() {
            None
        } else {
            crate::front2::coroutines::part_of(&FrontAdts(program, i, &consts, &langs, &checked_bodies), instance, &ground)
        };
        let shim = if is_own_instance || reify.is_some() || coroutine.is_some() {
            None
        } else {
            crate::fn_shim::fn_shim_of(&FrontAdts(program, i, &consts, &langs, &checked_bodies), instance, &ground)
        };
        let closure = if is_own_instance || reify.is_some() || shim.is_some() || coroutine.is_some() {
            None
        } else {
            let first = ground.view().list(instance.args).map_err(malformed)?.first().copied();
            let of = first.and_then(|t| blaze_build::closure_of(ground.view(), t));
            let which = [blaze_build::FnTrait::Fn, blaze_build::FnTrait::FnMut, blaze_build::FnTrait::FnOnce]
                .into_iter()
                .find(|&w| FrontAdts(program, i, &consts, &langs, &checked_bodies).fn_trait_method(w) == Some(instance.item));
            match (of, which) {
                (Some(of), Some(which)) => Some((of, which)),
                _ => return Err(callee_refusal(&*program, name(), instance.args, instance, crate::callee_shown::CalleeGap::NotOwnInstance, ground.view())),
            }
        };
        let owner_args = if spec_i.is_some() {
            List::EMPTY
        } else if let Some(target) = reify {
            target.args
        } else {
            match shim {
                Some(s) => s.item.map_or(List::EMPTY, |(_, fn_args)| fn_args),
                None => match coroutine {
                    Some((of, _)) => of.owner_args,
                    None => closure.map_or(instance.args, |(of, _)| of.owner_args),
                },
            }
        };
        args.extend_from_slice(ground.view().list(owner_args).map_err(malformed)?);
        // A coroutine's function: its instance is built once for all three (here, when no
        // body asked for its state before), and this slot takes its own.
        let from_coroutine = match coroutine {
            None => None,
            Some((of, part)) => {
                let find = |stash: &Vec<BuiltParts, Global>| stash.iter().position(|b| b.owner == of.owner && b.ordinal == of.ordinal && b.args[..] == args[..]);
                if find(&coroutine_stash).is_none() {
                    // Its state laid out from types, with every state it holds, and each built
                    // into its state (run/lay_states.rs).
                    let laid = lay_states::lay_and_build(&*program, &consts, &langs, &checked_bodies, &mut ground, &mut layouts, &mut coroutine_stash, i, &[of], alloc);
                    match laid {
                        Ok(lay_states::Lay::Done) => {}
                        Ok(lay_states::Lay::Unchecked(owner)) => {
                            filed(&mut survey, lay_states::unchecked_owner(&*program, i, owner))?;
                            continue;
                        }
                        Err(e) => step_refused!(e),
                    }
                }
                let taken = match find(&coroutine_stash) {
                    Some(n) => coroutine_stash[n].parts[part.index()].take(),
                    None => None,
                };
                let Some(taken) = taken else {
                    let error = BuildError::Malformed { what: "a coroutine's function its instance's build did not leave (asked for twice, or never made)" };
                    filed(&mut survey, RunError::Build { function: name(), error, range: None })?;
                    continue;
                };
                Some(taken)
            }
        };
        // The states this instance's own types hold, laid out from types and each coroutine
        // built into its state BEFORE this build (run/lay_states.rs): the build never meets a
        // state not laid. A coroutine's own function was built with its coroutine; a shim has
        // no body of its own.
        if from_coroutine.is_none() && shim.is_none() && clone_row.is_none() && from_row.is_none() {
            let adts = FrontAdts(&*program, i, &consts, &langs, &checked_bodies);
            let laid = match lay_states::roots_of_instance(&*program, &checked_bodies, &adts, i, &args, &mut ground, alloc) {
                Ok(roots) if roots.is_empty() => Ok(lay_states::Lay::Done),
                Ok(roots) => lay_states::lay_and_build(&*program, &consts, &langs, &checked_bodies, &mut ground, &mut layouts, &mut coroutine_stash, i, &roots, alloc),
                Err(e) => Err(e),
            };
            match laid {
                Ok(lay_states::Lay::Done) => {}
                Ok(lay_states::Lay::Unchecked(owner)) => {
                    filed(&mut survey, lay_states::unchecked_owner(&*program, i, owner))?;
                    continue;
                }
                Err(e) => step_refused!(e),
            }
        }
        let view = body.map(|body| body.view(program.body_types(i)));
        let adts = FrontAdts(program, i, &consts, &langs, &checked_bodies);
        // The record walk hands the builder its world recorded: what the build reads beyond the
        // key is the entry's read list (code_reads.rs).
        let recorded = records.as_deref().map(|run| {
            run.reads.clear();
            crate::code_records::reads::Recorded::new(&adts, &run.reads)
        });
        let world: &dyn Adts = match &recorded {
            Some(it) => it,
            None => &adts,
        };
        let mut subst = Subst { args: &args, ground: &mut ground, scratch: alloc };
        // `drop_in_place::<T>` is built as `T`'s drop glue, never from its written body.
        let built = if let Some(taken) = from_coroutine {
            Ok(taken)
        } else if let Some(s) = shim {
            match s.item {
                Some(_) => blaze_build::build_fn_shim(s.callee, s.tuple, s.which, Some(&program.sig(i)), &mut subst, world, Global),
                None => blaze_build::build_fn_shim(s.callee, s.tuple, s.which, None, &mut subst, world, Global),
            }
        } else if reify.is_some() || clone_row.is_some() || from_row.is_some() {
            let g = Global;
            if let Some(target) = reify {
                blaze_build::build_reify_shim(target.item, &program.sig(i), &mut subst, world, g)
            } else if let Some((s, copy)) = clone_row {
                // A builtin clone: its bytes when `Self: Copy`, else each field's own `clone`.
                match copy {
                    Some(copy) => blaze_build::build_builtin_clone(s.clone, copy, &mut subst, world, g),
                    None => Err(BuildError::Malformed { what: "a builtin `clone` built before its `Self: Copy` was asked" }),
                }
            } else if let Some(f) = from_row {
                blaze_build::build_builtin_from(f.from, &mut subst, world, g)
            } else {
                Err(BuildError::Malformed { what: "unreachable builtin row" })
            }
        } else if let Some(variant) = ctor_row {
            // The ctor's own instance (a shim of it was taken above): its fields in, its value out.
            blaze_build::build_ctor_fn(function.key, variant, &program.sig(i), &mut subst, world, Global)
        } else if let (Some(view), Some(done)) = (view.as_ref(), done) {
            if let Some((of, which)) = closure {
                blaze_build::build_closure(view, done, of.ordinal, which, &program.sig(i), &mut subst, world, Global)
            } else if Some(function.key) == program.fn_lang(FnLang::DropInPlace) {
                blaze_build::build_drop_glue(view, done, &program.sig(i), &mut subst, world, Global)
            } else {
                blaze_build::build(view, done, &program.sig(i), &mut subst, world, Global)
            }
        } else {
            Err(BuildError::Malformed { what: "a row with no body built as other than a ctor or its shim" })
        }
        .map_err(|error| {
            let ranges = |all: &[blaze_typeck::Range], i: u32| all.get(i as usize).copied();
            let range = match error {
                BuildError::Unsupported { expr, .. }
                | BuildError::UnsupportedType { expr, .. }
                | BuildError::NoConstBlockValue { expr, .. }
                | BuildError::NoConstFnInstance { expr, .. } => {
                    body.and_then(|body| ranges(&body.expr_ranges, expr.0))
                }
                BuildError::UnsupportedPat { pat, .. } => body.and_then(|body| ranges(&body.pat_ranges, pat.0)),
                // A static the statics pass refused (front2 program.rs `made_statics` files it in
                // `left_out` with the evaluator's words) is that cause, not "no static".
                BuildError::NoAdt { adt }
                | BuildError::NoAbi { item: adt }
                | BuildError::NoStatic { item: adt, .. }
                | BuildError::NoConstValue { item: adt, .. }
                | BuildError::NoPatConstValue { item: adt, .. }
                    if let Some(why) = program.left_out(adt) =>
                {
                    return RunError::LeftOut { function: name(), item: adt, path: program.item_named(adt), why: Box::from(why) };
                }
                BuildError::NoAdt { adt } => return RunError::NoAdt { function: name(), adt, path: program.item_named(adt) },
                // An item the body reads, refused: the survey's class names it.
                BuildError::NoStatic { item, .. }
                | BuildError::NoConstValue { item, .. }
                | BuildError::NoPatConstValue { item, .. }
                | BuildError::MissingRecord { item } => {
                    return RunError::ItemRefused { function: name(), error, item, path: program.item_named(item) };
                }
                BuildError::Projection { .. } if let Some(why) = program.last_projection() => {
                    return RunError::Projection { function: name(), why };
                }
                _ => None,
            };
            RunError::Build { function: name(), error, range }
        });
        let built = match built {
            Ok(b) => b,
            Err(e) => step_refused!(e),
        };
        let mut via = Vec::with_capacity_in(built.refs.len(), Global);
        let mut targets = Vec::with_capacity_in(built.refs.len(), Global);
        // In a survey, a callee refused leaves this instance unemitted (its targets short).
        let mut broken = false;
        // The world the calls resolve in: this fn's, or for a builtin clone (bodiless, no
        // world of its own) its caller's, whose world asked its `Self: Copy` (clone_shim.rs).
        let world_of = clone_row.and_then(|(s, _)| clones.caller_of(s)).unwrap_or(i);
        // A callee's selection or key met an answer no check made: the step is taken again
        // after `make_asked` (as the build's `Missing` above), never filed as the callee's.
        let may_ask = asked_rounds.0 != at || asked_rounds.1 < crate::front2::check::MOST_ROUNDS;
        let mut ask_again = false;
        for &called in built.refs.iter() {
            // A fn item of a trait's method as an `Fn*` value: its Self resolved to the body
            // that runs, as a call is, before the shim is tabled (fn_item_resolve.rs).
            let called = match crate::fn_item_resolve::resolved_shim_self(program, &adts, i, called, &mut ground) {
                Ok(c) => c,
                Err(error) => {
                    let at = crate::callee_shown::self_shown(program, ground.view(), called.args);
                    let refusal = RunError::Resolve { function: name(), callee: called.item, path: program.item_named(called.item), at, error };
                    if may_ask && asks_again(&refusal) {
                        ask_again = true;
                        break;
                    }
                    match unresolved(&mut survey, &mut reached, called, i, refusal)? {
                        Some(slot) => {
                            via.push(Via::Slot);
                            targets.push(Target::Slot(slot));
                        }
                        None => broken = true,
                    }
                    continue;
                }
            };
            // A closure's `Fn*` method: no impl to select; the instance is the closure's own,
            // tabled as its owner (the loop above builds it from the owner's body).
            // A fn item's or fn pointer's: the shim, tabled as fn_shim.rs says.
            // A coroutine's own function (its `Future::poll`, its resume fn, its drop): no impl
            // to select; tabled as the block's owner (front2/coroutines.rs).
            // A builtin `Clone::clone` (a tuple's, a closure's, a fn item's or pointer's): built,
            // tabled at the trait's `clone` row, its `Self: Copy` asked here (clone_shim.rs).
            let tabled_as = match clones.owner_non_adt(program, &adts, world_of, called, &mut ground, alloc) {
                Some(Ok(c)) => Some(c),
                Some(Err(e)) => {
                    let refusal = match e {
                        crate::clone_shim::NoteError::Resolve(error) => {
                            let at = crate::callee_shown::self_shown(program, ground.view(), called.args);
                            RunError::Resolve { function: name(), callee: called.item, path: program.item_named(called.item), at, error }
                        }
                        crate::clone_shim::NoteError::Build(error) => RunError::Build { function: name(), error, range: None },
                    };
                    if may_ask && asks_again(&refusal) {
                        ask_again = true;
                        break;
                    }
                    match unresolved(&mut survey, &mut reached, called, i, refusal)? {
                        Some(slot) => {
                            via.push(Via::Slot);
                            targets.push(Target::Slot(slot));
                        }
                        None => broken = true,
                    }
                    continue;
                }
                None => language_owner(program, &adts, called, &ground, i),
            };
            if let Some(c) = tabled_as {
                match key_of(program, c, called, &mut ground, &adts) {
                    Ok(key) => {
                        via.push(Via::Slot);
                        targets.push(Target::Slot(reached.slot(called, c, key)?));
                    }
                    Err(e) if may_ask && asks_again(&e) => {
                        ask_again = true;
                        break;
                    }
                    Err(e) => match unresolved(&mut survey, &mut reached, called, i, e)? {
                        Some(slot) => {
                            via.push(Via::Slot);
                            targets.push(Target::Slot(slot));
                        }
                        None => broken = true,
                    },
                }
                continue;
            }
            // A trait's fn runs as the selected impl's. A ctor is no trait's fn: it runs as
            // itself, and nothing is selected, so a caller with no body of its own (the `Fn*`
            // shim of a ctor's fn item, tabled at the ctor's bodiless row) asks no world.
            let is_ctor = program.fn_index(called.item).is_some_and(|c| program.function(c).kind == FnKind::Ctor);
            let selected = match is_ctor {
                true => Ok((called.item, called.args)),
                false => program.resolve(world_of, called.item, called.args, &mut ground),
            };
            let (item, resolved) = match selected {
                Ok(r) => r,
                Err(blaze_typeck::ResolveError::Missing(_)) if may_ask => {
                    ask_again = true;
                    break;
                }
                Err(blaze_typeck::ResolveError::NoImpl { trait_, item: err_item }) => {
                    match clones.owner_adt(program, &adts, world_of, called, trait_, &mut ground, alloc) {
                        Some(Ok(c)) => {
                            match key_of(program, c, called, &mut ground, &adts) {
                                Ok(key) => {
                                    via.push(Via::Slot);
                                    targets.push(Target::Slot(reached.slot(called, c, key)?));
                                }
                                Err(e) if may_ask && asks_again(&e) => {
                                    ask_again = true;
                                    break;
                                }
                                Err(e) => match unresolved(&mut survey, &mut reached, called, i, e)? {
                                    Some(slot) => {
                                        via.push(Via::Slot);
                                        targets.push(Target::Slot(slot));
                                    }
                                    None => broken = true,
                                },
                            }
                            continue;
                        }
                        Some(Err(crate::clone_shim::NoteError::Resolve(blaze_typeck::ResolveError::Missing(_)))) if may_ask => {
                            ask_again = true;
                            break;
                        }
                        Some(Err(e)) => {
                            let refusal = match e {
                                crate::clone_shim::NoteError::Resolve(error) => {
                                    let at = crate::callee_shown::self_shown(program, ground.view(), called.args);
                                    RunError::Resolve { function: name(), callee: called.item, path: program.item_named(called.item), at, error }
                                }
                                crate::clone_shim::NoteError::Build(error) => RunError::Build { function: name(), error, range: None },
                            };
                            match unresolved(&mut survey, &mut reached, called, i, refusal)? {
                                Some(slot) => {
                                    via.push(Via::Slot);
                                    targets.push(Target::Slot(slot));
                                }
                                None => broken = true,
                            }
                            continue;
                        }
                        None => {
                            let refusal = resolve_refusal(program, &adts, &ground, name(), called, blaze_typeck::ResolveError::NoImpl { trait_, item: err_item });
                            match unresolved(&mut survey, &mut reached, called, i, refusal)? {
                                Some(slot) => {
                                    via.push(Via::Slot);
                                    targets.push(Target::Slot(slot));
                                }
                                None => broken = true,
                            }
                            continue;
                        }
                    }
                }
                Err(error) => {
                    let refusal = resolve_refusal(program, &adts, &ground, name(), called, error);
                    match unresolved(&mut survey, &mut reached, called, i, refusal)? {
                        Some(slot) => {
                            via.push(Via::Slot);
                            targets.push(Target::Slot(slot));
                        }
                        None => broken = true,
                    }
                    continue;
                }
            };
            let callee = Instance { item, args: resolved };
            let Some(c) = program.fn_index(callee.item) else {
                let refusal = callee_refusal(&*program, name(), instance.args, callee, crate::callee_shown::CalleeGap::NotTabled, ground.view());
                match unresolved(&mut survey, &mut reached, callee, i, refusal)? {
                    Some(slot) => {
                        via.push(Via::Slot);
                        targets.push(Target::Slot(slot));
                    }
                    None => broken = true,
                }
                continue;
            };
            let target = program.function(c);
            // A ctor is built as its fn (blaze-build ctor_fn.rs), as a body is.
            if target.body.is_some() || target.kind == FnKind::Ctor {
                // A key the callee cannot have (a projection not normalized) refuses this caller.
                let key = match key_of(program, c, callee, &mut ground, &adts) {
                    Ok(k) => k,
                    Err(e) if may_ask && asks_again(&e) => {
                        ask_again = true;
                        break;
                    }
                    Err(e) => {
                        match unresolved(&mut survey, &mut reached, callee, i, e)? {
                            Some(slot) => {
                                via.push(Via::Slot);
                                targets.push(Target::Slot(slot));
                            }
                            None => broken = true,
                        }
                        continue;
                    }
                };
                via.push(Via::Slot);
                targets.push(Target::Slot(reached.slot(callee, c, key)?));
            } else if target.kind == FnKind::Foreign && target.symbol == UNWIND_RAISE && survey.stubs.is_some() {
                // D91: the raise is a stub of its own that stops the run (a survey, which runs
                // nothing, keeps the outside symbol below).
                let slot = reached.unresolved(callee, i, RunError::UnwindRaised { function: name() })?;
                via.push(Via::Slot);
                targets.push(Target::Slot(slot));
            } else if target.kind == FnKind::Foreign {
                via.push(Via::Address);
                targets.push(Target::Symbol(target.symbol.into()));
            } else {
                let refusal = callee_refusal(&*program, name(), instance.args, callee, crate::callee_shown::CalleeGap::NoBody, ground.view());
                match unresolved(&mut survey, &mut reached, callee, i, refusal)? {
                    Some(slot) => {
                        via.push(Via::Slot);
                        targets.push(Target::Slot(slot));
                    }
                    None => broken = true,
                }
            }
        }
        // Each vtable the body names, made once for the program (`Shape::of`, `Vtable::build`):
        // the tables of its supertraits off the first chain first, by a stack; each method word
        // the impl's fn, resolved and reached as a call is; the drop word `drop_in_place::<T>`.
        // A word's selection that met an answer no check made asks again as a call's does
        // (`dyn Debug` of an ADT: `ImplsOf(Debug, ..)`, census g091226's 347 + 146 rows).
        let mut table_of = Vec::with_capacity_in(built.vtables.len(), Global);
        let vtables: &[VtableRef] = match ask_again || broken {
            true => &[],
            false => &built.vtables,
        };
        for &want in vtables.iter() {
            let made = (|| -> Result<u32, RunError> {
                let fail = |why| RunError::Vtable { function: name(), principal: want.principal, why };
                let full = || fail(VtableFail::Build(BuildError::Malformed { what: "the ground table is full" }));
                let mut stack = Vec::new_in(Global);
                stack.push(want);
                while let Some(&t) = stack.last() {
                    if tables.refs.contains(&t) {
                        stack.pop();
                        continue;
                    }
                    let shape = Shape::of(t.principal, |k| program.object_trait(k), alloc).map_err(|e| fail(VtableFail::Shape(e)))?;
                    let mut cx = LayoutCx { ground: &mut ground, facts: &adts, scratch: alloc };
                    let id = layouts.of(t.concrete, &mut cx).map_err(|e| fail(VtableFail::Layout(e)))?;
                    let (size, align) = (layouts.get(id).size, layouts.get(id).align_bytes());
                    // Word 0: `drop_in_place::<T>` (a no-op glue when `T` needs none). With no
                    // glue fn AND no `Drop` trait, no type of the program can need drop: word 0
                    // is 0, which the built object drop skips (walk.rs `dyn_glue`).
                    let glue = match program.fn_lang(FnLang::DropInPlace) {
                        Some(item) => Some(Instance { item, args: ground.intern_list(&[t.concrete]).map_err(|_| full())? }),
                        None => match program.lang_item(LangItem::Drop).map_err(RunError::Lang)? {
                            None => None,
                            Some(_) => return Err(fail(VtableFail::NoDropGlue)),
                        },
                    };
                    let glue_item = glue.map(|g| g.item);
                    let mut targs = Vec::new_in(Global);
                    targs.extend_from_slice(ground.view().list(t.args).map_err(|_| full())?);
                    let table = Vtable::build(&shape, t.concrete, &targs, size, align, glue, &mut ground, &adts, alloc, Global)
                        .map_err(|e| fail(VtableFail::Build(e)))?;
                    let mut pushed_super = false;
                    for &(_, to) in table.relocs.iter() {
                        if let VtableTarget::Super(sup) = to
                            && !tables.refs.contains(&sup)
                            && !stack.contains(&sup)
                        {
                            stack.push(sup);
                            pushed_super = true;
                        }
                    }
                    if pushed_super {
                        continue;
                    }
                    let mut words = Vec::with_capacity_in(table.relocs.len(), Global);
                    for &(offset, to) in table.relocs.iter() {
                        // A fn item of a trait's method as the word's self: the body that runs,
                        // as for a call (fn_item_resolve.rs).
                        let to = match to {
                            VtableTarget::Fn(f) => VtableTarget::Fn(crate::fn_item_resolve::resolved_shim_self(program, &adts, i, f, &mut ground).map_err(|error| {
                                RunError::Resolve { function: name(), callee: f.item, path: program.item_named(f.item), at: crate::callee_shown::self_shown(program, ground.view(), f.args), error }
                            })?),
                            to => to,
                        };
                        let word = match to {
                            // A coroutine's own `Future::poll` or drop (`dyn Future` of an async
                            // block), a closure's `Fn*` method (`&dyn FnMut` of a closure), a fn
                            // item's or pointer's (its shim): what the language gives, as a
                            // call's callee (`language_owner`), never a selected impl.
                            VtableTarget::Fn(f) if let Some(c) = language_owner(program, &adts, f, &ground, i) => {
                                let key = key_of(program, c, f, &mut ground, &adts)?;
                                VtableWord::Slot(reached.slot(f, c, key)?)
                            }
                            VtableTarget::Fn(f) => {
                                // A trait method runs as the selected impl's; the glue is itself.
                                let (item, args) = match Some(f.item) == glue_item {
                                    true => (f.item, f.args),
                                    false => program
                                        .resolve(i, f.item, f.args, &mut ground)
                                        .map_err(|error| RunError::Resolve { function: name(), callee: f.item, path: program.item_named(f.item), at: crate::callee_shown::self_shown(program, ground.view(), f.args), error })?,
                                };
                                let c = program.fn_index(item).filter(|&c| program.function(c).body.is_some());
                                let Some(c) = c else {
                                    return Err(callee_refusal(&*program, name(), instance.args, Instance { item, args }, crate::callee_shown::CalleeGap::VtableWord, ground.view()));
                                };
                                let key = key_of(program, c, Instance { item, args }, &mut ground, &adts)?;
                                VtableWord::Slot(reached.slot(Instance { item, args }, c, key)?)
                            }
                            VtableTarget::Super(sup) => match tables.refs.iter().position(|r| *r == sup) {
                                Some(n) => VtableWord::Table(u32::try_from(n).map_err(|_| RunError::TooManyInstances)?),
                                None => return Err(fail(VtableFail::Shape(blaze_build::vtable::VtableError::NoTrait(sup.principal)))),
                            },
                        };
                        words.push((offset, word));
                    }
                    tables.refs.push(t);
                    tables.images.push(VtableImage { bytes: table.bytes, words });
                    stack.pop();
                }
                match tables.refs.iter().position(|r| *r == want) {
                    Some(n) => u32::try_from(n).map_err(|_| RunError::TooManyInstances),
                    None => Err(fail(VtableFail::Shape(blaze_build::vtable::VtableError::NoTrait(want.principal)))),
                }
            })();
            match made {
                Ok(n) => table_of.push(n),
                Err(e) if may_ask && asks_again(&e) => {
                    ask_again = true;
                    break;
                }
                Err(e) => {
                    filed(&mut survey, e)?;
                    broken = true;
                }
            }
        }
        if ask_again {
            // A coroutine's function was taken from its stash: put back for the step again.
            if let Some((of, part)) = coroutine
                && let Some(n) = coroutine_stash.iter().position(|b| b.owner == of.owner && b.ordinal == of.ordinal && b.args[..] == args[..])
            {
                coroutine_stash[n].parts[part.index()] = Some(built);
            }
            let rounds = if asked_rounds.0 == at { asked_rounds.1 } else { 0 };
            asked_rounds = match program_mut.make_asked(i) {
                Ok(true) => (at, rounds + 1),
                // Nothing to make: the step once more, its `Missing` then filed as it is.
                Ok(false) => (at, crate::front2::check::MOST_ROUNDS),
                Err(made) => {
                    filed(&mut survey, made)?;
                    continue;
                }
            };
            at -= 1;
            continue;
        }
        if broken {
            continue;
        }
        // The record walk: the instance as one code record, every target named (no slot, no
        // `via`), put under its key with the instances it reaches declared.
        if let (Some(run), Some((parts, key))) = (records.as_deref_mut(), record_key) {
            use crate::code_records::{Callee, CodeRecordError, Declared, InstanceNames};
            use crate::symbols::{encode_instance, fn_symbol_of_name, vtable_symbol};
            let fail = |error| RunError::CodeRecord { function: name(), error };
            let sym = |e| fail(CodeRecordError::Symbol(e));
            // Each instance the record reaches by slot: its tabled fn and its name.
            let mut reaches: Vec<(ItemKey, blaze_own::AVec<'static, u8>), Global> = Vec::new_in(Global);
            let name_of = |n: u32, reaches: &mut Vec<(ItemKey, blaze_own::AVec<'static, u8>), Global>| -> Result<blaze_code::record::Symbol, RunError> {
                let callee = reached.instances[n as usize];
                let tabled = program.function(reached.tabled[n as usize]).key;
                let mut bytes = blaze_own::Vec::new_in(alloc);
                encode_instance(callee.item, callee.args, ground.view(), alloc, &mut bytes).map_err(sym)?;
                let symbol = fn_symbol_of_name(&bytes);
                reaches.push((tabled, bytes));
                Ok(symbol)
            };
            let mut funcs: blaze_own::AVec<'_, Callee<'_>> = blaze_own::Vec::with_capacity_in(targets.len(), alloc);
            for target in targets.iter() {
                match target {
                    Target::Slot(n) => funcs.push(Callee::Symbol(name_of(*n, &mut reaches)?)),
                    Target::Symbol(c_name) => funcs.push(Callee::C(&**c_name)),
                    // A stub's words (crate::stub) are in a stub's own body only, never a built one's.
                    Target::Stops | Target::Stop => {
                        return Err(RunError::Build { function: name(), error: BuildError::Malformed { what: "a stub's word among a built body's targets" }, range: None });
                    }
                }
            }
            // The methods and drop glue of each vtable the body names, and of the tables above
            // it, by a work list.
            let mut todo: Vec<u32, Global> = Vec::new_in(Global);
            todo.extend_from_slice(&table_of);
            let mut seen: Vec<u32, Global> = Vec::new_in(Global);
            while let Some(t) = todo.pop() {
                if seen.contains(&t) {
                    continue;
                }
                seen.push(t);
                for &(_, word) in tables.images[t as usize].words.iter() {
                    match word {
                        VtableWord::Slot(n) => {
                            name_of(n, &mut reaches)?;
                        }
                        VtableWord::Table(above) => todo.push(above),
                    }
                }
            }
            let mut vtables = Vec::with_capacity_in(built.vtables.len(), Global);
            for &table in built.vtables.iter() {
                vtables.push(vtable_symbol(table, ground.view(), alloc).map_err(sym)?);
            }
            // The soft-float routines after the instance's own references, as run/make_one.rs
            // binds them: `make_entry` declares those the emitter says the record calls.
            let routines = bound.bind(&mut funcs).map_err(fail)?;
            let mut c_statics: Vec<(ItemKey, &str), Global> = Vec::new_in(Global);
            for &s in built.statics.iter() {
                if let Some(c) = program.foreign_static(s) {
                    c_statics.push((s, c));
                }
            }
            let names = InstanceNames { funcs: &funcs, statics: &built.statics, c_statics: &c_statics, vtables: &vtables, tls: &built.thread_locals };
            // A census walk (`RecordRun::census`): the instance's LIR and names go to the census,
            // which emits them itself; no record is made. The walk goes on to the slots it filled.
            if let Some(seen) = run.seen.as_deref_mut() {
                let function = format!("{}", name());
                let mut reached = blaze_own::Vec::with_capacity_in(reaches.len(), alloc);
                for (tabled, bytes) in reaches.iter() {
                    reached.push(Declared { tabled: *tabled, name: &bytes[..] });
                }
                let declares = crate::code_records::instance_declares(&reached, &names, &built.vtables, ground.view(), &routines, alloc).map_err(fail)?;
                let instance = crate::code_records::SeenInstance {
                    function: &function,
                    symbol: parts.symbol,
                    func: built.func.verified(),
                    names: &names,
                    routines: *routines.places(),
                    declares,
                };
                seen.seen(instance);
                run.met.push(crate::code_records::Met { symbol: parts.symbol, key, parts, function: function.into_boxed_str(), found: false });
                continue;
            }
            let mut declared = blaze_own::Vec::new_in(alloc);
            for (tabled, bytes) in reaches.iter() {
                declared.push(Declared { tabled: *tabled, name: &bytes[..] });
            }
            // The instance each call runs is part of what the entry read: a trait's fn selected
            // to an impl (the front end's `resolve`, asked again when the entry is found).
            for &called in built.refs.iter() {
                let resolved = program.resolve(i, called.item, called.args, &mut ground).ok();
                run.reads.ask_resolve(called, resolved, ground.view(), alloc);
            }
            let reads = run.reads.seal(&mut ground, alloc).map_err(|e| fail(CodeRecordError::Reads(e)))?;
            let entry = crate::code_records::make_entry_reading(built.func.verified(), &names, routines, &mut declared, &reads, run.builder.isa, alloc)
                .map_err(fail)?;
            // Put under its content key, and its code key pointed at it: an entry built again
            // after a stale one replaces the pointer, never the bytes under a key.
            let content = crate::code_records::content_key(&entry);
            // The whole-program record walk keys an instance by its parts, not by the crates of
            // its items: kept under the program's root (the lazy walk, body_ids.rs, classes it).
            let class = crate::code_records::CodeClass::Program;
            run.store.put(class, content, &entry).map_err(|e| fail(CodeRecordError::Store(e)))?;
            run.store.point(class, key, content).map_err(|e| fail(CodeRecordError::Store(e)))?;
            run.met.push(crate::code_records::Met { symbol: parts.symbol, key, parts, function: format!("{}", name()).into_boxed_str(), found: false });
            continue;
        }
        // The soft-float routines, bound as the record walk binds them (run/make_one.rs,
        // front2/builtins_shim.rs `Bound::bind`): a function that legalizes gets one reference
        // place per found routine after its own, each the routine's instance's slot; legalize
        // turns an `f128` operation into a call there. A routine whose fn no row tables (the
        // program reaches its exports beside the roots, program.rs `make_from`, unless a
        // survey's `Within` leaves its crate out) stays unbound, and its operation is refused
        // naming why.
        let mut places = blaze_emit::RoutinePlaces::NONE;
        if !matches!(blaze_lir::legalize::needed(&built.func.view()), Ok(false)) {
            for r in blaze_lir::legalize::SoftFloat::ALL {
                let Some(k) = builtins.lib(r).map(crate::front2::keys::checker_of) else { continue };
                let slot = match routine_slots[r as usize] {
                    Some(s) => s,
                    None => {
                        let Some(c) = program.fn_index(k) else { continue };
                        let routine = Instance { item: k, args: List::EMPTY };
                        let key = key_of(program, c, routine, &mut ground, &adts)?;
                        let s = reached.slot(routine, c, key)?;
                        routine_slots[r as usize] = Some(s);
                        s
                    }
                };
                let place = u32::try_from(via.len()).map_err(|_| RunError::TooManyInstances)?;
                places.bind(r, place);
                via.push(Via::Slot);
                targets.push(Target::Slot(slot));
            }
        }
        let mut code = Code::new_in(Global);
        if let Err(error) = blaze_emit::emit_with_routines(built.func.verified(), &via, &places, isa(), &mut code) {
            let refusal = match error {
                blaze_emit::EmitError::Legalize(blaze_lir::legalize::LegalizeError::NoRoutine(routine)) => RunError::CodeRecord {
                    function: name(),
                    error: crate::code_records::CodeRecordError::Routine(crate::front2::builtins_shim::BuiltinsRefused { routine, why: builtins.found(routine) }),
                },
                error => RunError::Emit { function: name(), error },
            };
            filed(&mut survey, refusal)?;
            continue;
        }
        let mut tls_at = Vec::with_capacity_in(built.thread_locals.len(), Global);
        for &key in built.thread_locals.iter() {
            let index = match tls_vars.as_mut() {
                Some(vars) => vars.index_of(key)?,
                None => tls_rows,
            };
            if index >= tls_rows {
                return Err(RunError::Build {
                    function: name(),
                    error: BuildError::Malformed { what: "a thread-local the program's descriptor has no row for" },
                    range: None,
                });
            }
            tls_at.push(index);
        }
        let positions = match place::resolve_positions(&code, &|item, site| program_mut.source_position(ItemKey(item), site)) {
            Ok(positions) => positions,
            Err(error) => {
                filed(&mut survey, RunError::Place(PlaceError::Position(error)))?;
                continue;
            }
        };
        // A static an `extern` block declares is its C symbol's address (the seam's
        // `foreign_static`), any other the static image's.
        let mut statics_at = Vec::with_capacity_in(built.statics.len(), Global);
        for &s in built.statics.iter() {
            statics_at.push(match program_mut.foreign_static(s) {
                Some(c) => crate::place::StaticAt::Symbol(Box::from(c)),
                None => crate::place::StaticAt::Image(s),
            });
        }
        bodies.push(Body { code, targets, statics: statics_at, tls: tls_at, vtables: table_of, positions });
    }
    let program = &*program_mut;
    // The record walk keeps no body: its answer is the store's entries and `met`. A program
    // with thread-locals has one getter (the slot after every instance, below), which every
    // record touching a thread-local calls by `tls_getter_symbol`: no caller declares it, so
    // its record is made here, once. A record walk always has an entry: it takes no survey, so
    // a program with no `main` stopped at `RunError::NoMain` above.
    if let Some(run) = records.as_deref_mut() {
        if let (Some(_), Some(main)) = (tls_vars.as_ref(), entry) {
            run.getter(alloc).map_err(|error| RunError::CodeRecord { function: program.function(main).named(), error })?;
            run.ender(alloc).map_err(|error| RunError::CodeRecord { function: program.function(main).named(), error })?;
        }
        return Ok(None);
    }
    // The stub build: the last slot's stub, as the loop's top makes each slot's before it; then
    // every slot has its body (the getter's slot comes after them).
    if let Some(stubs) = survey.stubs.as_ref() {
        let slots = reached.instances.len();
        if bodies.len() + 1 == slots {
            bodies.push(stubs.body(slots - 1).ok_or_else(|| unstubbed(program.function(reached.tabled[slots - 1]).named()))?);
        }
        if bodies.len() != slots {
            return Err(unstubbed(program.function(reached.tabled[bodies.len()]).named()));
        }
    }
    if let Some(counts) = counts.as_deref_mut() {
        counts.built = bodies.len();
        counts.tried = reached.instances.len();
        let mut met: blaze_hash::HashMap<ItemKey, (), Global> = blaze_hash::HashMap::new_in(Global);
        met.extend(reached.instances.iter().map(|i| (i.item, ())));
        for c in 0..program.fn_count() {
            let f = program.function(c);
            if f.body.is_some() && checked[c].is_some() && generic(program, c) && !met.contains_key(&f.key) {
                counts.no_instance.push(format!("{}", f.named()).into_boxed_str());
            }
        }
        counts.no_instance.sort_unstable();
    }
    // A survey with no entry makes no image: its counts are the answer.
    let Some(main) = entry else { return Ok(None) };
    // A build that runs: each fn a static points at gets its slot (a stub's when the walk did
    // not reach it), before the getter's slot.
    let static_fns = match (image.as_ref(), survey.stubs.as_mut()) {
        (Some(image), Some(stubs)) => static_fn_slots(image, &reached.keys, stubs, &mut bodies)?,
        _ => Vec::new_in(Global),
    };
    // The thread-locals' getter and ender (THREADS.md D2), once each, in the two slots after
    // every instance; each calls its imports directly. The ender is the key's destructor:
    // `ran_built` gives its thunk to `tls_index::create_key`.
    let (getter, ender) = match tls_vars {
        None => (None, None),
        Some(_) => {
            let named = || program.function(main).named();
            let getter = tls::index::getter(Global)
                .map_err(|error| RunError::Build { function: named(), error: BuildError::Lir(error), range: None })?;
            let ender = tls::index::ender(Global)
                .map_err(|error| RunError::Build { function: named(), error: BuildError::Lir(error), range: None })?;
            for (func, imports) in [(&getter, &tls::index::GETTER_IMPORTS[..]), (&ender, &tls::index::ENDER_IMPORTS[..])] {
                let mut via = Vec::with_capacity_in(imports.len(), Global);
                let mut targets = Vec::with_capacity_in(imports.len(), Global);
                for name in imports {
                    via.push(Via::Address);
                    targets.push(Target::Symbol((*name).into()));
                }
                let mut code = Code::new_in(Global);
                blaze_emit::emit(func.verified(), &via, isa(), &mut code).map_err(|error| RunError::Emit { function: named(), error })?;
                bodies.push(Body {
                    code,
                    targets,
                    statics: Vec::new_in(Global),
                    tls: Vec::new_in(Global),
                    vtables: Vec::new_in(Global),
                    positions: Vec::new_in(Global),
                });
            }
            let ender = u32::try_from(bodies.len() - 1).map_err(|_| RunError::TooManyInstances)?;
            (Some(ender - 1), Some(ender))
        }
    };
    // Each slot's function path, by slot, for a trap's line (crate::trap): a reached
    // instance's fn path (slot `n` is `reached.tabled[n]`'s); after them a static's fn stub
    // ([`static_fn_slots`]) and the thread-locals' getter.
    let mut paths: Vec<Box<str, Global>, Global> = Vec::with_capacity_in(bodies.len(), Global);
    for slot in 0..bodies.len() {
        paths.push(match reached.tabled.get(slot) {
            Some(&tabled) if slot < reached.instances.len() => program.function(tabled).named().path,
            _ if getter.is_some_and(|g| g as usize == slot) => Box::from("the thread-locals' getter (blaze's own)"),
            _ if ender.is_some_and(|g| g as usize == slot) => Box::from("the thread-locals' ender (blaze's own)"),
            _ => Box::from("a fn a static points to, reached by no call (its stub)"),
        });
    }
    // `reached` counts every slot before the getter's, built or stubbed; only the stub build
    // has any refusal to list.
    let reached = u32::try_from(reached.instances.len()).map_err(|_| RunError::TooManyInstances)?;
    let refused = survey.stubs.map_or_else(|| Vec::new_in(Global), |stubs| stubs.refused);
    Ok(Some(Built { bodies, vtables: tables.images, start, start_fn: program.function(main).named(), image, getter, ender, refused, reached, static_fns, paths }))
}

/// Each fn instance `image` points at (its `ImageTarget::Fn` words), sorted by key, with its
/// slot: the slot the walk built it in (`keys` by slot), else a slot of its own at the end of
/// `bodies` holding a stub with [`RunError::StaticFn`] (the image names it by key alone, so the
/// walk cannot build it: it stops the program only if called through the static).
fn static_fn_slots(image: &StaticImage<Global>, keys: &[Option<InstanceKey>], stubs: &mut Stubs, bodies: &mut Vec<Body, Global>) -> Result<Vec<(InstanceKey, u32), Global>, RunError> {
    let mut by_key: Vec<(InstanceKey, u32), Global> = Vec::with_capacity_in(keys.len(), Global);
    for (slot, key) in keys.iter().enumerate() {
        if let Some(key) = key {
            by_key.push((*key, u32::try_from(slot).map_err(|_| RunError::TooManyInstances)?));
        }
    }
    // The first slot of a key, as `Reached::slot` gives it.
    by_key.sort_by_key(|k| k.0);
    by_key.dedup_by_key(|k| k.0);
    let mut out: Vec<(InstanceKey, u32), Global> = Vec::new_in(Global);
    for r in image.relocs.iter() {
        let ImageTarget::Fn(key) = r.target else { continue };
        let Err(at) = out.binary_search_by_key(&key, |f| f.0) else { continue };
        let slot = match by_key.binary_search_by_key(&key, |k| k.0) {
            Ok(found) => by_key[found].1,
            Err(_) => {
                let slot = bodies.len();
                // The static whose bytes hold the word: the last whose first byte is at or before it.
                let static_ = image.statics.iter().filter(|s| s.1 <= r.at).max_by_key(|s| s.1).map(|s| s.0);
                stubs.slot = slot;
                stubs.keep(RunError::StaticFn { static_, key });
                bodies.push(stubs.body(slot).ok_or(RunError::TooManyInstances)?);
                u32::try_from(slot).map_err(|_| RunError::TooManyInstances)?
            }
        };
        out.insert(at, (key, slot));
    }
    Ok(out)
}
