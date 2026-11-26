//! D106 piece X: ONE missing instance made, for the record walk ([`crate::body_ids::Walk`]).
//!
//! The walk finds every instance it can by a key made from text before any check; the maker
//! here is called only for an instance whose record is not found. It checks the bodies that one
//! instance needs (through the seam's [`FrontEnd::check_one`], never `check_all`), pulls the
//! constants it reads at its ground arguments, builds it (a plain body, a closure's `Fn*`
//! method, a fn shim, one of a coroutine's three functions, or `drop_in_place`'s glue), names
//! every reference by symbol, and appends its entry: the code, and the instances it declares
//! (code_records.rs `make_entry`).
//!
//! It is a child of `run` so it calls run.rs's own per-item functions and types
//! ([`FrontAdts`], [`Bodies`], [`ClosureTable`], [`BackLangs`], `callee_refusal`, `closure_owner`)
//! without copying them and without widening their visibility.
//!
//! WHAT `build_from` DOES FOR THE WHOLE PROGRAM, AND ITS ONE-INSTANCE FORM HERE:
//! - `check_all` (every body): [`MakerState::ensure`], one `check_one` per fn key, asked once a
//!   run: the instance's tabled fn, and each owner its ground ARGUMENTS hold (`held_owners`: a
//!   closure's or coroutine's owner, whose facts the layouts read, and an opaque type's
//!   defining fn), and each async block's owner a coroutine demand names. A fn ITEM type in the
//!   arguments is not checked: this instance reads none of its body. Its row is made from its
//!   head ([`FrontEnd::head_row`]): the build reads its ABI and signature.
//! - `eval_consts` (every `const` and `static` of the program): not run. The record walk takes a
//!   front end that evaluates each constant at its arguments (`evaluates_at_args`): the per-item
//!   pull `const_at::needs_of` + `const_at::pull` for THIS instance. The old front is refused by
//!   name ([`RunError::OneBody`]).
//! - `static_image` (every static's bytes): not run. A record names a static, thread-local or
//!   vtable by symbol; each one's data is its own record (D106 item 6, bzcode03's kinds 1-3).
//! - `ClosureTable::of` (every checked body): each newly checked body adds its own closures'
//!   rows (`ClosureTable::add`, [`MakerState::ensure`]); `CoroutineTable::of` likewise: each
//!   newly checked body with an async block adds its own blocks' rows (`CoroutineTable::add`).
//! - coroutine states: run/lay_states.rs `lay_and_build`, the one path (build_from's too): laid
//!   out from types before the instance is built, each coroutine then built into its state; an
//!   owner fn the lay needs that is not checked yet is checked ([`Step::Check`]) and the
//!   instance is made again (types only were read).
//! - the reference loop and the vtable loop inside build_from's step (slots, keys): [`name_refs`]
//!   and [`vtable_rows`], by instance NAME, no slot, no instance key (the walk tells rows apart by
//!   symbol).
//!
//! THE ONE GROUND TABLE lives across instances in [`MakerState`]: coroutine states, pulled
//! constants and the coroutine stash are keyed by interned types of it, as in build_from.
//!
//! Allocators: `Alloc<'static>` handed in by the caller (the tables this calls into take
//! `Alloc<'static>`: `CoroutineTable::of`, lay_states.rs). `Global` is named only where run.rs's
//! existing types fix it (`Bodies::const_at`, `BuiltParts::args`, blaze-build's `Built<Global>`).

use super::*;

use blaze_ids::producer::{CodeBuilder, ProducerId};
use blaze_own::AVec;

use crate::body_ids::{BodyIds, Counts, EntryForm, Fixture, Grant, Make, Walk, WalkError};
use crate::code_records::reads::{self, Answers, ReadKind, ReadLog, ReadPlant, ReadsError, Recorded, Verdict};
use crate::code_records::{Builder, Callee, CodeRecordError, CodeStore, Declared, EntryView, InstanceNames, VtableDatum, make_entry_with_vtables};
use crate::front2::const_at::{ConstNeed, ConstTable, Needs};
use crate::front2::coroutines::Coroutines;
use crate::front2::program::lazy::LazyRefusal;
use super::lay_states::{LaidOnly, Lay, forget_laid_in, lay_and_build_one, lay_only, roots_of_instance, unchecked_owner};
use crate::symbols::{decode_instance, encode_instance, fn_symbol_of_name, vtable_symbol};

/// Whole-program demand-driven cross-function LIR inlining and LTO for `blaze export`.
#[path = "export_lto.rs"]
pub mod export_lto;

/// One function reference in a [`BuiltLir`] body: either a Rust instance symbol or an offset span
/// into [`BuiltLir::c_text`] naming a C extern function.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BuiltCallee {
    Symbol(blaze_code::record::Symbol),
    C { start: u32, len: u32 },
}

/// One instance lowered to verified LIR with all outside references resolved, before machine-code
/// emission (used by [`export_lto`] for whole-program cross-function inlining and LTO).
pub struct BuiltLir<'a> {
    pub func: blaze_lir::Func<Alloc<'a>>,
    /// Direct function references in `func.refs` order (without soft-float routines appended yet).
    pub funcs: AVec<'a, BuiltCallee>,
    /// Static items in `func.refs` order.
    pub statics: AVec<'a, ItemKey>,
    /// Foreign (`extern`) statics among `statics`: `(item, start, len)` into `c_text`.
    pub c_statics: AVec<'a, (ItemKey, u32, u32)>,
    /// Vtable symbols in `func.refs` order.
    pub vtables: AVec<'a, blaze_code::record::Symbol>,
    /// Encoded vtable datums needed by `vtables` (and their supertraits).
    pub vtable_data: AVec<'a, (blaze_code::record::Symbol, AVec<'a, u8>)>,
    /// Thread-local items in `func.refs` order.
    pub tls: AVec<'a, ItemKey>,
    /// Build-side trap sources for `Op::Unreachable` instructions.
    pub traps: AVec<'a, blaze_code::record::TrapSource>,
    /// Concatenated UTF-8 bytes of all C function and C static names referenced by this body.
    pub c_text: AVec<'a, u8>,
}

impl<'a> BuiltLir<'a> {
    /// Resolves `BuiltCallee` at `place` into a [`Callee`] borrowing from `self.c_text`.
    pub fn callee_at(&self, place: u32) -> Option<Callee<'_>> {
        let idx = usize::try_from(place).ok()?;
        match *self.funcs.get(idx)? {
            BuiltCallee::Symbol(sym) => Some(Callee::Symbol(sym)),
            BuiltCallee::C { start, len } => self.c_str(start, len).map(Callee::C),
        }
    }

    /// Returns the C static symbol name for `item`, if it is a foreign static.
    pub fn foreign_static_name(&self, item: ItemKey) -> Option<&str> {
        let &(_, start, len) = self.c_statics.iter().find(|(k, _, _)| *k == item)?;
        self.c_str(start, len)
    }

    /// Slices a UTF-8 C symbol name out of `self.c_text`.
    pub fn c_str(&self, start: u32, len: u32) -> Option<&str> {
        let s = usize::try_from(start).ok()?;
        let l = usize::try_from(len).ok()?;
        let e = s.checked_add(l)?;
        let bytes = self.c_text.get(s..e)?;
        core::str::from_utf8(bytes).ok()
    }
}

/// Destination for one built instance inside [`build_one`]: either emit an encoded `CodeStore`
/// entry, or hand back the verified LIR and resolved reference tables for whole-program LTO.
enum BuildSink<'e, 'a> {
    Entry(&'e mut AVec<'a, u8>),
    Lir { alloc: Alloc<'a>, out: &'e mut Option<BuiltLir<'a>> },
}

/// How many bodies were checked for a walk's makes: the claim "a run that finds every record
/// checks no body" is read here. A plain counter, owned by the maker and read by the test.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CheckCount {
    /// `FrontEnd::check_one` calls that handed a body over.
    pub check_one: usize,
    /// Bodies the program held CHECKED when it was constructed (fns with a body, at
    /// construction): today's `Front2Program::new` checks its whole reach there; a lazy program
    /// holds none.
    pub by_construction: usize,
    /// Programs constructed: 0 in a run that found every record.
    pub constructed: usize,
    /// Checked bodies walked to table their async blocks ([`MakerState::ensure`]): each body
    /// with a closure or an async block is walked once, when its check hands it over
    /// (`CoroutineTable::add`), so this is at most `check_one` (the d106_records test asserts it).
    pub coroutine_walks: usize,
}

/// A made function at or past this many bytes of code or of estimated frame is reported
/// ([`MakeSizes`]; rule 16: a huge function is a P0 until its profile names it).
pub const MAKE_REPORT_BYTES: u64 = 1 << 20;
/// A make that took at least this long is reported ([`MakeSizes`]).
pub const MAKE_REPORT_TIME: core::time::Duration = core::time::Duration::from_secs(1);

/// What a maker reports its large or slow makes with: the clock the make's times are read on,
/// and where each [`MakeSizes`] goes (the parent prints it, one line to stderr). Handed in
/// ([`Maker::watch`]; rule 75: no clock and no output of its own); without it, no make is
/// timed and none reported.
#[derive(Clone, Copy)]
pub struct MakeWatch<'n> {
    pub clock: blaze_core::wait::Monotonic,
    pub report: &'n (dyn Fn(&MakeSizes) + Sync),
}

/// The slots of a made function of one [`blaze_lir::func::SlotKind`]: how many, and their
/// bytes, as the build wrote them (before the Fast tier's simplify scalarizes any).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct SlotSum {
    pub count: u64,
    pub bytes: u64,
}

/// One made function's size and time, reported when it is large or slow: its code or its
/// estimated frame at least [`MAKE_REPORT_BYTES`], or its make at least [`MAKE_REPORT_TIME`].
/// Before (tempo t1935d, tempo-precompiles-macros, 3 tests): `syn::ty::parsing::ambig_ty` took
/// 48.9 s to make and its stack probe faulted 13.4 MB into its code (blaze-emit pass/probe.rs
/// places the probing tail after the last instruction), a frame past the 8 MiB main stack,
/// named by no line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MakeSizes {
    /// The fn, by path (its instance's arguments are not named).
    pub function: Named,
    /// LIR blocks.
    pub blocks: u64,
    /// LIR instructions; each numbers a value, so this is the values' count too (blaze-lir
    /// func.rs `View::len`).
    pub insts: u64,
    /// The instructions the build's aggregate copies wrote (blaze-build `Built::copy_insts`).
    pub copy_insts: u64,
    /// Slots by kind: `Temp`, `Named`, `ArgSpill` ([`blaze_lir::func::SlotKind::ALL`]'s order).
    pub slots: [SlotSum; 3],
    /// An ESTIMATE of the frame: the slots' bytes plus 8 per instruction (blaze-emit lib.rs:
    /// every value its own 8-byte cell, a pair's 16; an instruction with no result has none).
    /// blaze-emit does not expose its `frame_bytes`.
    pub frame_estimate: u64,
    /// The entry's bytes (its record, declarations and tables): the code is no longer.
    pub entry_bytes: u64,
    /// The record's code, read from the entry when it is at least [`MAKE_REPORT_BYTES`].
    pub code: CodeSize,
    /// The make's time: the checks and constant pulls before and between builds (`lower`), the
    /// builds (`build`, every try of the instance), the emits (`emit`).
    pub lower: core::time::Duration,
    pub build: core::time::Duration,
    pub emit: core::time::Duration,
}

/// A made function's code, as [`MakeSizes`] reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodeSize {
    /// The entry is shorter than [`MAKE_REPORT_BYTES`], so the code is too: not read.
    Small,
    /// The record's instruction bytes and its whole code (instructions and their data).
    Read { insts: u64, bytes: u64 },
    /// The entry just made did not read back (code_records.rs `EntryView::read`'s words).
    Unread(Box<str>),
}

impl core::fmt::Display for CodeSize {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CodeSize::Small => f.write_str("under the entry's length"),
            CodeSize::Read { insts, bytes } => write!(f, "{bytes} B ({insts} B instructions)"),
            CodeSize::Unread(why) => write!(f, "not read: the entry did not read back: {why}"),
        }
    }
}

impl core::fmt::Display for MakeSizes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let [t, n, a] = self.slots;
        write!(
            f,
            "blaze make: large or slow fn `{}`: lir blocks {}, insts {} (= values), copy insts {}; slots temp {} ({} B), named {} ({} B), arg-spill {} ({} B); frame estimate {} B (slots + 8 B per value; not blaze-emit's frame_bytes); entry {} B, code {}; make lower {:?}, build {:?}, emit {:?} (rule 16)",
            self.function, self.blocks, self.insts, self.copy_insts, t.count, t.bytes, n.count, n.bytes, a.count, a.bytes, self.frame_estimate, self.entry_bytes, self.code, self.lower, self.build, self.emit,
        )
    }
}

/// The sizes of a made function, before its times: [`MakeSizes`] less `lower`, `build` and
/// `emit`, read from the built function and its entry.
#[derive(Clone, Debug)]
struct MadeSizes {
    function: Named,
    blocks: u64,
    insts: u64,
    copy_insts: u64,
    slots: [SlotSum; 3],
    frame_estimate: u64,
    entry_bytes: u64,
    code: CodeSize,
}

impl MadeSizes {
    /// The sizes of `built` and its entry `made`. The entry is read (`EntryView::read`) only
    /// when it is at least [`MAKE_REPORT_BYTES`] long: its code is no longer than it.
    fn of(function: Named, built: &blaze_build::Built<Global>, made: &[u8]) -> MadeSizes {
        let view = built.func.verified().view();
        let mut slots = [SlotSum::default(); 3];
        for s in view.slots.iter() {
            use blaze_lir::func::SlotKind;
            if let Some(at) = SlotKind::from_byte(s.kind).and_then(|k| SlotKind::ALL.iter().position(|&x| x == k)) {
                slots[at].count += 1;
                slots[at].bytes += u64::from(s.size);
            }
        }
        let insts = view.len() as u64;
        let frame_estimate = slots.iter().map(|s| s.bytes).sum::<u64>() + 8 * insts;
        let entry_bytes = made.len() as u64;
        let code = if entry_bytes < MAKE_REPORT_BYTES {
            CodeSize::Small
        } else {
            match crate::code_records::EntryView::read(made) {
                Ok(e) => CodeSize::Read { insts: u64::from(e.insts), bytes: e.record.code().len() as u64 },
                Err(why) => CodeSize::Unread(std::format!("{why}").into_boxed_str()),
            }
        };
        MadeSizes { function, blocks: view.blocks() as u64, insts, copy_insts: built.copy_insts, slots, frame_estimate, entry_bytes, code }
    }

    /// Whether a make of these sizes, `took` long, is reported.
    fn reported(&self, took: core::time::Duration) -> bool {
        let code = match self.code {
            CodeSize::Read { bytes, .. } => bytes,
            CodeSize::Small => 0,
            CodeSize::Unread(_) => self.entry_bytes,
        };
        code >= MAKE_REPORT_BYTES || self.frame_estimate >= MAKE_REPORT_BYTES || took >= MAKE_REPORT_TIME
    }
}

/// The current make's times and last sizes, kept by [`Work`] while one instance is made.
#[derive(Clone, Debug, Default)]
struct MakeTimes {
    build: core::time::Duration,
    emit: core::time::Duration,
    last: Option<MadeSizes>,
}

/// Work at the lazy program's body boundary for this run. An initial lowering counts
/// when it produces a body. Declarations and requests for other instances of that body
/// add none. The other two counters count calls.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoweringCounts {
    pub bodies_lowered: u64,
    pub lower_again: u64,
    pub forks: u64,
    /// Where the bodies' checked columns came from, by a typing that counts them ([`ByKind`]);
    /// `None` for one that does not ([`SecondCheck`], [`FromRecord`]) or before a make.
    pub typing: Option<TypingCounts>,
}

impl core::fmt::Display for LoweringCounts {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "bodies lowered {}, lower_again calls {}, forks {}", self.bodies_lowered, self.lower_again, self.forks)?;
        match self.typing {
            Some(typing) => write!(f, "; {typing}"),
            None => Ok(()),
        }
    }
}

/// The bodies a run handed over by where their checked columns came from ([`ByKind`]): read
/// from the record `check` kept, or checked by the run, by kind (lazy typed.rs `RunChecked`).
/// The run's closing line: "N bodies read from records, M checked by the run (kinds: …)".
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct TypingCounts {
    pub read: u64,
    /// By `RunChecked::index`.
    pub checked: [u64; crate::front2::program::lazy::typed::RunChecked::ALL.len()],
}

impl TypingCounts {
    /// Every body the run checked itself, of any kind.
    pub fn checked_by_run(&self) -> u64 {
        self.checked.iter().sum()
    }
}

impl core::fmt::Display for TypingCounts {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} bodies read from records, {} checked by the run (kinds:", self.read, self.checked_by_run())?;
        let mut none = true;
        for kind in crate::front2::program::lazy::typed::RunChecked::ALL {
            let n = self.checked[kind.index()];
            if n > 0 {
                write!(f, "{} {} {n}", if none { "" } else { "," }, kind.name())?;
                none = false;
            }
        }
        if none {
            write!(f, " none")?;
        }
        write!(f, ")")
    }
}

impl CheckCount {
    /// Every check the makes cost.
    pub fn all(&self) -> usize {
        self.check_one + self.by_construction
    }
}

/// THE TYPING SEAM (D163 part 2, rule 78): where the make takes a body's checked columns from.
/// [`SecondCheck`] checks the body here (today's production: the check `blaze check` made is
/// done a second time); [`FromRecord`] reads what the check kept and never checks (D159 "one
/// body record"; a gap is a typed refusal naming the body, never a second check). Both lower
/// the body here and table it the same way (lazy fill.rs `check_one_key_by`), so the oracle
/// ([`Twin`]) holds them to byte-equal entries.
///
/// THE FLIP: production's typing is the one line in blaze-cli run_parent.rs `maker` that hands
/// `SecondCheck` to `Maker::of_roots` (and the `Front::Maker` type naming it), which becomes
/// `FromRecord(<the check's typed records>)` once the check keeps them.
pub trait Typing<P: FrontEnd> {
    /// Fn `key`'s row and checked body, as the seam's [`FrontEnd::check_one`] answers.
    fn check_one(&mut self, program: &mut P, key: ItemKey) -> Result<Option<(usize, Checked<'static>)>, RunError>;
    /// The bodies handed over by where their columns came from, for a typing that counts them
    /// ([`ByKind`]); `None` for one that does not.
    fn counts(&self) -> Option<TypingCounts>;
}

/// The body checked in the make, a second time (the seam's own `check_one`).
#[derive(Clone, Copy, Debug, Default)]
pub struct SecondCheck;

impl<P: FrontEnd> Typing<P> for SecondCheck {
    fn check_one(&mut self, program: &mut P, key: ItemKey) -> Result<Option<(usize, Checked<'static>)>, RunError> {
        program.check_one(key)
    }

    fn counts(&self) -> Option<TypingCounts> {
        None
    }
}

/// The body's checked columns read from the check's record `R` (lazy typed.rs), never checked
/// in the make. Only the lazy program answers it: the one program the record walk makes.
pub struct FromRecord<R>(pub R);

impl<'n, S: crate::front2::program::lazy::PositionSource<'n>, R: crate::front2::program::lazy::typed::TypedRecords> Typing<crate::front2::program::lazy::LazyProgram<'n, S>> for FromRecord<R> {
    fn check_one(&mut self, program: &mut crate::front2::program::lazy::LazyProgram<'n, S>, key: ItemKey) -> Result<Option<(usize, Checked<'static>)>, RunError> {
        program.check_one_typed(key, &mut self.0)
    }

    fn counts(&self) -> Option<TypingCounts> {
        None
    }
}

/// THE TYPING BY KIND (D163 part 2, D197 root 1): each body's checked columns from the record
/// `check` kept (lazy typed.rs `KeptBodies`), read with no checker round; a body of a kind
/// `check` keeps no record for yet (`RunChecked`) checked here, as the only check it gets,
/// counted by kind. A gap in a kind that has records is the program's typed refusal naming the
/// body (`LazyRefusal::Typed`), never a check in its place (rule 3).
pub struct ByKind<K> {
    pub kept: K,
    counts: TypingCounts,
}

impl<K> ByKind<K> {
    pub fn new(kept: K) -> ByKind<K> {
        ByKind { kept, counts: TypingCounts::default() }
    }

    /// The bodies handed over so far, by where their columns came from.
    pub fn counts(&self) -> TypingCounts {
        self.counts
    }
}

impl<'n, S: crate::front2::program::lazy::PositionSource<'n>, K: crate::front2::program::lazy::typed::KeptBodies> Typing<crate::front2::program::lazy::LazyProgram<'n, S>> for ByKind<K> {
    fn check_one(&mut self, program: &mut crate::front2::program::lazy::LazyProgram<'n, S>, key: ItemKey) -> Result<Option<(usize, Checked<'static>)>, RunError> {
        use crate::front2::program::lazy::typed::{BodySource, Given};
        match self.kept.source(key) {
            Ok(BodySource::Run(kind)) => {
                let made = program.check_one(key)?;
                if made.is_some() {
                    self.counts.checked[kind.index()] += 1;
                }
                Ok(made)
            }
            Ok(BodySource::Read(checked)) => {
                let made = program.check_one_typed(key, &mut Given { key, got: Some(Ok(checked)) })?;
                if made.is_some() {
                    self.counts.read += 1;
                }
                Ok(made)
            }
            // Refused by the program, which names the body as for any gap of its record.
            Err(gap) => program.check_one_typed(key, &mut Given { key, got: Some(Err(gap)) }),
        }
    }

    fn counts(&self) -> Option<TypingCounts> {
        Some(self.counts)
    }
}

/// The run's tables that [`Bodies`] owns, kept between instances.
struct Tables {
    closures: ClosureTable,
    coroutines: Coroutines,
    const_at: ConstTable<Global>,
}

/// What lives across instances besides the checked bodies.
struct Work {
    alloc: Alloc<'static>,
    isa: Isa,
    overflow: OverflowChecks,
    /// What `ub_checks()` is: the records' key's debug assertions (`Maker::new`).
    ub: blaze_build::UbChecks,
    /// The program's panic strategy: the records' key's (`Maker::new`).
    panic: blaze_build::Panic,
    langs: BackLangs,
    ground: PortableTys<'static>,
    layouts: Layouts<Alloc<'static>>,
    /// Each coroutine instance built, its three functions waiting for their instances.
    stash: Vec<BuiltParts, Global>,
    needs: Needs<Global>,
    /// Whether a made instance touched a thread-local: the program then needs the getter's
    /// and ender's records (code_records.rs `RecordRun::getter`/`ender`).
    thread_locals: bool,
    /// The program's compiler_builtins soft-float routines as instances, found once a run
    /// (front2/builtins_shim.rs): every instance's emitter gets their places.
    routines: crate::front2::builtins_shim::Bound,
    /// The clock a make is timed on ([`MakeWatch::clock`]); `None`: no make is timed or
    /// reported.
    clock: Option<blaze_core::wait::Monotonic>,
    /// The instance being made: its builds' and emits' times, and its last build's sizes.
    times: MakeTimes,
    /// The last make's report, when it was large or slow, for the maker to hand on
    /// ([`MakeWatch::report`]).
    reported: Option<MakeSizes>,
    /// The questions the build being made asked ([`Asking`]): cleared as each build starts,
    /// so a build made again after a retry keeps only its own answers (a lazy front's default
    /// `repr` before a head was made is never kept), then sealed into the entry's read list
    /// (code_reads.rs).
    reads: ReadLog,
    /// The read-list answers found holding this run, by question ([`verify_instance`]): a
    /// question many found entries ask is asked of the front end once.
    answers: Answers<'static>,
    /// Per-`MakerState` memoization cache for pure ground-type queries (`normalize`,
    /// `adt_drop_impl`, `resolve`) keyed by interned IDs in `self.ground`. Sits below `Recorded` /
    /// `Asking` so every build's `ReadLog` is unchanged.
    query_cache: core::cell::RefCell<QueryCache>,
}

/// Compact stack-inline argument list key for [`QueryCache::normalize`] (99%+ of projections have
/// <= 4 arguments, avoiding heap allocation on every lookup).
#[derive(PartialEq, Eq, Hash)]
enum SmallArgs {
    Inline { len: u8, buf: [PTy; 4] },
    Heap(AVec<'static, PTy>),
}

impl SmallArgs {
    fn of(args: &[PTy], alloc: Alloc<'static>) -> Self {
        if let Ok(len) = u8::try_from(args.len())
            && len <= 4
        {
            let mut buf = [PTy(0); 4];
            if let Some(dst) = buf.get_mut(..args.len()) {
                dst.copy_from_slice(args);
            }
            SmallArgs::Inline { len, buf }
        } else {
            let mut heap = AVec::with_capacity_in(args.len(), alloc);
            heap.extend_from_slice(args);
            SmallArgs::Heap(heap)
        }
    }
}

/// Memoization cache for pure ground-type front-end queries over `Work::ground`.
struct QueryCache {
    alloc: Alloc<'static>,
    normalize: blaze_hash::HashMap<(ItemKey, blaze_typeck::Name, SmallArgs), PTy, Alloc<'static>>,
    drop_impl: blaze_hash::HashMap<ItemKey, Option<ItemKey>, Alloc<'static>>,
    resolve: blaze_hash::HashMap<(ItemKey, List), (ItemKey, List), Alloc<'static>>,
    track_caller: blaze_hash::HashMap<ItemKey, bool, Alloc<'static>>,
}

impl QueryCache {
    fn new_in(alloc: Alloc<'static>) -> Self {
        Self {
            alloc,
            normalize: blaze_hash::HashMap::new_in(alloc),
            drop_impl: blaze_hash::HashMap::new_in(alloc),
            resolve: blaze_hash::HashMap::new_in(alloc),
            track_caller: blaze_hash::HashMap::new_in(alloc),
        }
    }
}

/// One maker's state over program `P` (made with the program, on the first miss).
pub struct MakerState {
    /// Each fn key asked of `check_one`, sorted: the fn's index, `None` when it had no body.
    asked: AVec<'static, (ItemKey, Option<usize>)>,
    /// Fn `i`'s checked body, `None` until asked (or with none): what [`Bodies`] reads.
    checked: AVec<'static, Option<Checked<'static>>>,
    tables: Option<Tables>,
    work: Work,
}

/// Outcome of Phase 1 (LIR construction) before reference resolution and emission.
struct BuiltPhase<A: core::alloc::Allocator> {
    own: Option<blaze_build::Built<A>>,
    from_coroutine: Option<(usize, usize)>,
}

/// Phase 1 step outcome.
enum Phase1<A: core::alloc::Allocator> {
    Built(BuiltPhase<A>),
    /// The lay of a coroutine state met owner fn `.0` not checked yet (lay_states.rs
    /// `Lay::Unchecked`): check it, then build again.
    Check(ItemKey),
    /// One or more coroutines laid for this build need their owner's constants pulled
    /// (lay_states.rs `Lay::PullConstants`): pull them, then build again.
    PullConstants(AVec<'static, (ItemKey, List)>),
}

/// Missing facts collected in a single Phase 2 sweep across all `built.refs` and `built.vtables`.
struct RefGaps {
    first_ask: Option<RunError>,
    missing_objects: AVec<'static, (ItemKey, RunError)>,
    untabled: AVec<'static, (ItemKey, RunError)>,
}

impl RefGaps {
    fn new_in(alloc: Alloc<'static>) -> Self {
        Self {
            first_ask: None,
            missing_objects: AVec::new_in(alloc),
            untabled: AVec::new_in(alloc),
        }
    }

    fn is_empty(&self) -> bool {
        self.first_ask.is_none() && self.missing_objects.is_empty() && self.untabled.is_empty()
    }
}

/// Phase 2 step outcome (reference resolution, vtable assembly, and emission).
enum Phase2 {
    Done,
    Needs(RefGaps),
}

fn tables_gone(key: ItemKey) -> RunError {
    RunError::OneBody { key, why: "the maker's tables were left taken by an earlier failed step (make_one.rs `MakerState::tables`)" }
}

impl MakerState {
    /// Build a static datum or TLS template using the same ground names as code entries.
    pub fn static_entry<P: FrontEnd>(
        &mut self,
        program: &P,
        value: &crate::seam::SeamData,
        kind: StaticItem,
        template: bool,
    ) -> Result<AVec<'static, u8>, crate::code_records::statics::StaticRecordError> {
        self.static_entry_with_vtables(program, value, kind, template, &[], &[], &[])
    }

    /// [`Self::static_entry`] carrying the resolved vtable symbols, declared instances and datums
    /// built for any `DataTarget::Vtable` words in `value`.
    #[allow(clippy::too_many_arguments)]
    fn static_entry_with_vtables<P: FrontEnd>(
        &mut self,
        program: &P,
        value: &crate::seam::SeamData,
        kind: StaticItem,
        template: bool,
        vtable_syms: &[(blaze_build::vtable::VtableKey, blaze_code::record::Symbol)],
        extra_declared: &[Declared<'_>],
        carried_vtables: &[VtableDatum<'_>],
    ) -> Result<AVec<'static, u8>, crate::code_records::statics::StaticRecordError> {
        use crate::code_records::statics::{make_static_entry_with_targets, make_template_entry_with_targets};

        let alloc = self.work.alloc;
        let ground = &mut self.work.ground;
        let fn_target = |key| {
            let Some(mut instance) = program.const_fn_instance(key, ground, alloc) else {
                return Ok(None);
            };
            if program.track_caller(instance.item) == Some(true) {
                instance = blaze_build::reify_shim_instance(instance, ground).map_err(|_| crate::symbols::SymbolError::TableFull)?;
            }
            let view = ground.view();
            let tabled = static_fn_tabled(program, instance, view);
            let mut name = blaze_own::Vec::new_in(alloc);
            encode_instance(instance.item, instance.args, view, alloc, &mut name)?;
            Ok(Some((tabled, name)))
        };
        let vtable_target = |key| Ok(vtable_syms.iter().find(|&&(k, _)| k == key).map(|&(_, sym)| sym));
        if template {
            make_template_entry_with_targets(value, kind, fn_target, vtable_target, extra_declared, carried_vtables, alloc)
        } else {
            make_static_entry_with_targets(value, kind, fn_target, vtable_target, extra_declared, carried_vtables, alloc)
        }
    }

    /// The state for `program`, before its first instance. Refuses a front end that evaluates
    /// constants for the whole program only (`evaluates_at_args` false), naming `first`.
    pub fn new<P: FrontEnd>(program: &P, first: ItemKey, isa: Isa, overflow: OverflowChecks, ub: blaze_build::UbChecks, panic: blaze_build::Panic, alloc: Alloc<'static>) -> Result<MakerState, RunError> {
        if !program.evaluates_at_args() {
            return Err(RunError::OneBody {
                key: first,
                why: "this front end evaluates its constants for the whole program (run.rs `eval_consts`); the record walk takes one that evaluates each at its arguments (front2)",
            });
        }
        let langs = BackLangs::of(program)?;
        let empty: &[Option<Checked<'static>>] = &[];
        let tables = Tables {
            closures: ClosureTable::of(program, empty, alloc),
            coroutines: Coroutines::of(program, empty, alloc),
            const_at: ConstTable::new_in(Global),
        };
        let ground = PortableTys::new_in(alloc);
        let routines = crate::front2::builtins_shim::Bound::new(program.soft_float_routines(alloc), &ground, alloc).map_err(|_| RunError::OneBody {
            key: first,
            why: "a compiler_builtins soft-float routine's instance name did not encode (front2/builtins_shim.rs `Bound::new`)",
        })?;
        let work = Work {
            alloc,
            isa,
            overflow,
            ub,
            panic,
            langs,
            ground,
            layouts: Layouts::new_in(alloc),
            stash: Vec::new_in(Global),
            needs: Needs::new_in(Global),
            thread_locals: false,
            routines,
            clock: None,
            times: MakeTimes::default(),
            reported: None,
            reads: ReadLog::new(ReadPlant::None),
            answers: Answers::new_in(alloc),
            query_cache: core::cell::RefCell::new(QueryCache::new_in(alloc)),
        };
        Ok(MakerState { asked: blaze_own::Vec::new_in(alloc), checked: blaze_own::Vec::new_in(alloc), tables: Some(tables), work })
    }

    /// The read-list tests' planted bug (code_reads.rs `ReadPlant`, rule 23): every build after
    /// this records with `plant`.
    pub fn plant_reads(&mut self, plant: ReadPlant) {
        self.work.reads = ReadLog::new(plant);
    }

    /// Times every later make on `clock`, and keeps a large or slow one's [`MakeSizes`] for
    /// [`MakerState::take_report`].
    pub fn time_on(&mut self, clock: blaze_core::wait::Monotonic) {
        self.work.clock = Some(clock);
    }

    /// The last make's report, when it was large or slow ([`MakeSizes`]); taken once.
    pub fn take_report(&mut self) -> Option<MakeSizes> {
        self.work.reported.take()
    }

    /// Fn `key` checked, once a run: its index, `None` when the front end has no body for it.
    /// A newly checked body with a closure or an async block re-makes the closure and coroutine
    /// tables over the bodies checked so far. Every OTHER fn whose closure a checked body's
    /// types hold is checked too, before this returns (its closure's facts are in its body):
    /// by a work list, each owner once.
    fn ensure<P: FrontEnd, T: Typing<P>>(&mut self, program: &mut P, typing: &mut T, key: ItemKey, checks: &mut CheckCount) -> Result<Option<usize>, RunError> {
        /// The most closure owners one ensure follows (rule 18).
        const MOST_OWNERS: usize = 4096;
        let first = self.ensure_one(program, typing, key, checks)?;
        let mut todo: AVec<'_, ItemKey> = blaze_own::Vec::new_in(self.work.alloc);
        self.owners_named(&*program, key, &mut todo);
        let mut steps = 0;
        while let Some(owner) = todo.pop() {
            if self.asked.binary_search_by_key(&owner, |r| r.0).is_ok() {
                continue;
            }
            steps += 1;
            if steps > MOST_OWNERS {
                return Err(RunError::OneBody { key, why: "more closure owners named than the most one ensure follows" });
            }
            self.ensure_one(program, typing, owner, checks)?;
            self.owners_named(&*program, owner, &mut todo);
        }
        Ok(first)
    }

    /// Each fn other than `key` whose check `key`'s checked body's types read, pushed on `todo`
    /// (in node order: deterministic for one body): a closure's owner (its facts are in its
    /// body), and an `impl Trait` type's defining fn (the seam's `opaque_of`: its hidden type is
    /// in that fn's check, front2/coroutines.rs `opaque_hidden`), an `async fn`'s future among
    /// them.
    fn owners_named<P: FrontEnd>(&self, program: &P, key: ItemKey, todo: &mut AVec<'_, ItemKey>) {
        let Some(Some(i)) = self.asked.binary_search_by_key(&key, |r| r.0).ok().map(|n| self.asked[n].1) else { return };
        let Some(Some(done)) = self.checked.get(i) else { return };
        let view = done.types.view();
        for n in view.nodes() {
            let owner = match *n {
                blaze_typeck::PNode::Closure { owner, .. } => owner,
                // The fn that defines it (an alias's is its `#[define_opaque]` fn); an alias
                // whose definer is not found is left to the build, which refuses it by name
                // (front2/coroutines.rs `opaque_hidden`).
                blaze_typeck::PNode::Opaque(opaque, _) => match program.opaque_definer(opaque) {
                    Some(Ok(owner)) => owner,
                    Some(Err(_)) | None => continue,
                },
                _ => continue,
            };
            if owner != key {
                todo.push(owner);
            }
        }
    }

    /// [`MakerState::ensure`] of one key, owners not followed.
    fn ensure_one<P: FrontEnd, T: Typing<P>>(&mut self, program: &mut P, typing: &mut T, key: ItemKey, checks: &mut CheckCount) -> Result<Option<usize>, RunError> {
        let slot = match self.asked.binary_search_by_key(&key, |r| r.0) {
            Ok(n) => return Ok(self.asked[n].1),
            Err(n) => n,
        };
        // A ctor has no body to check: its row is made, and its instance built as a fn
        // ([`build_one`]'s ctor arm).
        // The trait's `Clone::clone` has no body either: the one instance tabled at it is a
        // builtin clone ([`build_one`]'s arm; clone_shim.rs `language_clone`).
        if program.ctor_of(key).is_some()
            || program.lang_trait_fn(LangItem::Clone, "clone") == Some(key)
            || program.lang_trait_fn(LangItem::From, "from") == Some(key)
        {
            let at = program.head_row(key)?;
            self.asked.insert_shift(slot, (key, at));
            return Ok(at);
        }
        // Populate head facts first so a tuple/unit struct ctor whose ADT has not yet been
        // visited (e.g. when callers came from the verified code cache) is registered in
        // `ctor_of` instead of being sent to `check_one` as a body-bearing fn.
        let head_at = program.head_row(key)?;
        if program.ctor_of(key).is_some() {
            self.asked.insert_shift(slot, (key, head_at));
            return Ok(head_at);
        }
        // The body's checked columns by the typing seam: checked again, or read (`Typing`).
        let got = typing.check_one(program, key)?;
        let at = match got {
            None => None,
            Some((i, done)) => {
                self.register_checked_row(&*program, key, i, done, checks)?;
                Some(i)
            }
        };
        self.asked.insert_shift(slot, (key, at));
        Ok(at)
    }

    /// Stores `done` at row `i` and refreshes the closure and coroutine tables if the body
    /// declares any closures or async blocks. Shared by `ensure_one` and on-demand generic
    /// `inline for` specialization (`register_specialized`).
    fn register_checked_row<P: FrontEnd>(&mut self, program: &P, key: ItemKey, i: usize, done: Checked<'static>, checks: &mut CheckCount) -> Result<(), RunError> {
        checks.check_one += 1;
        while self.checked.len() < program.fn_count().max(i + 1) {
            self.checked.push(None);
        }
        let mut refresh = !done.closures.is_empty();
        if !refresh && let Some((body, _)) = program.function(i).body {
            let view = body.view(program.body_types(i));
            refresh = blaze_build::coroutines_in(&view).into_iter().next().is_some();
        }
        if let Some(slot) = self.checked.get_mut(i) {
            *slot = Some(done);
        }
        if refresh {
            let tables = self.tables.as_mut().ok_or_else(|| tables_gone(key))?;
            // This body's closures, one row each ([`ClosureTable::add`]): the table is
            // never re-made over the bodies checked before.
            if let (Some((body, _)), Some(done)) = (program.function(i).body, self.checked.get(i).and_then(|d| d.as_ref())) {
                let sig = program.sig(i);
                tables.closures.add(&body.view(program.body_types(i)), done, sig.parent_count + sig.own_count, self.work.alloc);
            }
            // This body's async blocks, one row each (front2/coroutines.rs
            // `CoroutineTable::add`): the table is never re-made over the bodies checked
            // before, so each checked body is walked once (counted).
            if let Some(done) = self.checked.get(i).and_then(|d| d.as_ref()) {
                checks.coroutine_walks += 1;
                tables.coroutines.table.add(program, i, done, self.work.alloc);
            }
            // The library's async items, read again with the rows made so far: before the
            // first check a lazy front end has no `Poll` row yet, so `MakerState::new`'s
            // reading is `None` (front2/coroutines.rs `async_items`).
            tables.coroutines.items = crate::front2::coroutines::async_items(program);
            tables.coroutines.poll = crate::front2::coroutines::future_poll(program);
        }
        Ok(())
    }

    /// Registers a newly created generic `inline for` specialization row `spec_i` (synthetic
    /// key `spec_key`) and ensures any external closure or opaque owners its checked types name.
    fn register_specialized<P: FrontEnd, T: Typing<P>>(
        &mut self,
        program: &mut P,
        typing: &mut T,
        spec_key: ItemKey,
        spec_i: usize,
        done: Checked<'static>,
        checks: &mut CheckCount,
    ) -> Result<(), RunError> {
        /// The most closure owners one ensure follows (rule 18).
        const MOST_OWNERS: usize = 4096;
        if let Err(slot) = self.asked.binary_search_by_key(&spec_key, |r| r.0) {
            self.asked.insert_shift(slot, (spec_key, Some(spec_i)));
        }
        self.register_checked_row(&*program, spec_key, spec_i, done, checks)?;
        let mut todo: AVec<'_, ItemKey> = blaze_own::Vec::new_in(self.work.alloc);
        self.owners_named(&*program, spec_key, &mut todo);
        let mut steps = 0;
        while let Some(owner) = todo.pop() {
            if self.asked.binary_search_by_key(&owner, |r| r.0).is_ok() {
                continue;
            }
            steps += 1;
            if steps > MOST_OWNERS {
                return Err(RunError::OneBody { key: spec_key, why: "more closure owners named than the most one ensure follows" });
            }
            self.ensure_one(program, typing, owner, checks)?;
            self.owners_named(&*program, owner, &mut todo);
        }
        Ok(())
    }
}

/// Reification records a selected fn item. Closure coercion records `call_once` at the
/// closure type, whose body belongs to the item that contains the closure expression.
pub(super) fn static_fn_tabled<P: FrontEnd>(program: &P, mut instance: Instance, ground: blaze_typeck::PTys<'_>) -> ItemKey {
    for _ in 0..8 {
        let Some(target) = blaze_build::vtable_shim_of(instance, ground) else {
            break;
        };
        instance = target;
    }
    if let Some(target) = blaze_build::reify_shim_of(instance, ground) {
        return target.item;
    }
    if let Ok([first, _]) = ground.list(instance.args)
        && let Some(closure) = blaze_build::closure_of(ground, *first)
    {
        let (lang, name) = crate::front2::langs::fn_trait_method(blaze_build::FnTrait::FnOnce);
        if program.lang_trait_fn(lang, name) == Some(instance.item) {
            return closure.owner;
        }
    }
    instance.item
}

/// Makes ONE instance: `name` (symbols.rs `encode_instance` bytes), tabled as fn `tabled`;
/// appends its entry (code_records.rs `make_entry`) to `entry`. The one generic step of D106:
/// every front end behind the seam is made through it (rule 78). `typing`: where each body's
/// checked columns come from ([`Typing`]).
pub fn make_instance<P: FrontEnd, T: Typing<P>>(
    program: &mut P,
    typing: &mut T,
    state: &mut MakerState,
    checks: &mut CheckCount,
    tabled: ItemKey,
    name: &[u8],
    entry: &mut AVec<'_, u8>,
) -> Result<(), RunError> {
    use blaze_core::wait::Clock as _;
    let made_from = state.work.clock.map(|c| c.now());
    state.work.times = MakeTimes::default();
    state.work.reported = None;
    make_instance_timed(program, typing, state, checks, tabled, name, entry)?;
    // The make's report, when it was large or slow (rule 16): its times split, the checks and
    // pulls (`lower`) being what the builds and emits leave of the whole.
    if let (Some(c), Some(from)) = (state.work.clock, made_from) {
        let took = c.now().saturating_sub(from);
        let times = core::mem::take(&mut state.work.times);
        if let Some(last) = times.last
            && last.reported(took)
        {
            let MadeSizes { function, blocks, insts, copy_insts, slots, frame_estimate, entry_bytes, code } = last;
            let lower = took.saturating_sub(times.build).saturating_sub(times.emit);
            state.work.reported = Some(MakeSizes { function, blocks, insts, copy_insts, slots, frame_estimate, entry_bytes, code, lower, build: times.build, emit: times.emit });
        }
    }
    Ok(())
}

/// [`make_instance`]'s work, timed by it.
fn make_instance_timed<'a, P: FrontEnd, T: Typing<P>>(
    program: &mut P,
    typing: &mut T,
    state: &mut MakerState,
    checks: &mut CheckCount,
    tabled: ItemKey,
    name: &[u8],
    entry: &mut AVec<'a, u8>,
) -> Result<(), RunError> {
    let mut sink = BuildSink::Entry(entry);
    run_instance_loop(program, typing, state, checks, tabled, name, &mut sink)
}

/// Shared instance-lowering loop for both [`make_instance_timed`] (entry emission) and
/// [`Maker::build_lir`] (LIR extraction for export LTO).
fn run_instance_loop<'a, P: FrontEnd, T: Typing<P>>(
    program: &mut P,
    typing: &mut T,
    state: &mut MakerState,
    checks: &mut CheckCount,
    tabled: ItemKey,
    name: &[u8],
    sink: &mut BuildSink<'_, 'a>,
) -> Result<(), RunError> {
    let alloc = state.work.alloc;
    // The bodies this ONE instance reads: its fn's, and each owner its name holds.
    let Some(i) = state.ensure(program, typing, tabled, checks)? else {
        return Err(RunError::OneBody { key: tabled, why: "the record walk reached an instance of a fn the front end holds no checked body for" });
    };
    let (item, args) = decode_instance(name, &mut state.work.ground, alloc).map_err(|e| RunError::CodeRecord { function: program.function(i).named(), error: CodeRecordError::Symbol(e) })?;
    let instance = Instance { item, args };
    // Each fn whose check another fact of this instance's ARGUMENTS is in: a closure's or an
    // async block's owner (its capture facts are in its body: `LayoutFacts::closure`, which
    // lay_states.rs `roots_of_instance` reads before the build), and an `impl Trait`'s
    // defining fn (its hidden type), as [`MakerState::owners_named`] follows them for a
    // checked body's types. Read from the ground tree, not by the program's rows: a lazy front
    // end has no row for an owner until it is checked (front2/program/lazy `fn_index`), so a
    // filter by row skipped `rand::rngs::thread::rng` for `LocalKey::try_with` at its closure.
    let Held { owners, items, objects } = held_owners(&*program, state.work.ground.view(), args, tabled, alloc)?;
    // Each `dyn` principal the arguments hold: its object table asked by name BEFORE the build
    // (D194, D197): core's `<&mut F as FnMut>::call_mut` at `F = dyn FnMut(..)` reads the table
    // of a trait no body of this instance names, which before came only from the check of the
    // caller that coerced to the object (on another worker, or found in the store). `false` is
    // a front end that makes none here: the build's `NoTrait` then names it.
    for &principal in objects.iter() {
        program.object_table(principal)?;
    }
    // The heads the body's `#[track_caller]` sites count from, asked BEFORE the build (D194):
    // for a body-local fn, each enclosing item's (seam_rules.rs `body_anchor`), which before
    // came only from a check of the enclosing body this maker happened to make. One the ask
    // could not serve stays the build's typed `SiteGap::NoEnclosingRow`.
    program.site_heads(tabled)?;
    for &owner in owners.iter() {
        state.ensure(program, typing, owner, checks)?;
    }
    // Each fn item whose TYPE the arguments hold: its row from its head, its body not checked
    // (this instance reads none of it; its own instance is the walk's, found or made). The
    // Reference, "Function item types": "When referred to, a function item, or the
    // constructor of a tuple-like struct or enum variant, yields a zero-sized value of its
    // function item type. That type explicitly identifies the function", so calling the value
    // calls that fn with its own ABI and signature, which the build asks (`Adts::abi`) as it
    // builds the call `f(x)` at `F` = the item's type, before any reference is named. The body
    // that named the item as a value may have been found by its record and not checked in
    // this run, so no check made that head (t1530, 36 tests: "the build's `Adts` gave no ABI
    // for it" at `Result::map` with `NonZero::<u64>::new`, `Option::map` with `Into::into`,
    // fastrand's `try_with_rng(Rng::fork)`, a fn handed to `OnceLock::get_or_init`).
    for &item in items.iter() {
        program.head_row(item)?;
    }
    // A test report shim calls `Termination::report` (the trait method, which `name_ref` then
    // resolves to the selected impl): ensure its trait head and row exist before the build asks
    // for its signature and ABI.
    if blaze_build::test_report_shim_of(instance, state.work.ground.view()).is_some()
        && let Some(report_fn) = program.termination_report()
    {
        program.head_row(report_fn)?;
    }
    // Generic `inline for` functions specialize on demand at concrete ground arguments into a
    // monomorphic unrolled row with 0 generic args of its own (matching `run.rs:2321-2327`).
    let (build_i, build_instance) = if instance.args.len > 0 && instance.item == tabled {
        let mut ground_args: AVec<'_, PTy> = blaze_own::Vec::new_in(alloc);
        if let Ok(list) = state.work.ground.view().list(instance.args) {
            ground_args.extend_from_slice(list);
        }
        match program.specialize_inline_for(i, &ground_args, state.work.ground.view())? {
            Some((spec_i, spec_checked)) => {
                let spec_key = program.function(spec_i).key;
                if let Some(done) = spec_checked {
                    state.register_specialized(program, typing, spec_key, spec_i, done, checks)?;
                }
                (spec_i, Instance { item: spec_key, args: List::EMPTY })
            }
            None => (i, instance),
        }
    } else {
        (i, instance)
    };
    // The constants this instance reads at its ground arguments, pulled before the build
    // (front2/const_at.rs): a plain instance's own body only, as in build_from. Grounding a
    // constant path's arguments normalizes them, and that asks the world as the build does:
    // arbitrary 1.4.2's `T::Unsigned::ZERO` (`<<T as Int>::Unsigned as Int>::ZERO`) at `T =
    // usize` asks `ImplsOf(Int, usize)` for `<usize as Int>::Unsigned` HERE, before the build's
    // loop below, so an unmade answer (`Missing`) ended the instance with the fills its world
    // noted never made (tempo t1855d, 15 tests; d106_callables.rs rows 4-11: only the cut of the
    // projection paths passed). The same ask-again as the build's: the program makes what the
    // worlds asked (`FrontEnd::make_asked`) and the constants are pulled again; bounded by the
    // same count (rule 18), and nothing made is the `Missing` itself.
    let mut pulled = AskProgress::new_in(alloc);
    let mut pull_owners = 0usize;
    loop {
        match pull_constants(program, state, build_i, build_instance) {
            Ok(PullStep::Done) => break,
            Ok(PullStep::Check(owner)) => {
                pull_owners += 1;
                if pull_owners > MOST_HELD_COROUTINES {
                    let error = BuildError::UnsupportedSig { what: "an instance whose constant evaluation needed more than 4096 coroutine owner fns checked" };
                    return Err(RunError::Build { function: program.function(build_i).named(), error, range: None });
                }
                if state.ensure(program, typing, owner, checks)?.is_none() {
                    return Err(unchecked_owner(&*program, build_i, owner));
                }
            }
            Err(RunError::Build { error: BuildError::OpaqueOwnerUnchecked { owner, .. }, .. })
                if pull_owners < MOST_HELD_COROUTINES
                    && match state.asked.binary_search_by_key(&owner, |r| r.0) {
                        Err(_) => true,
                        Ok(slot) => state.asked.get(slot).is_some_and(|e| e.1.is_none()) && program.fn_index(owner).is_none(),
                    } =>
            {
                pull_owners += 1;
                if state.ensure(program, typing, owner, checks)?.is_none() {
                    let Some(row) = program.head_row(owner)? else {
                        return Err(unchecked_owner(&*program, build_i, owner));
                    };
                    if let Ok(slot) = state.asked.binary_search_by_key(&owner, |r| r.0)
                        && let Some(entry) = state.asked.get_mut(slot)
                    {
                        entry.1 = Some(row);
                    }
                }
            }
            Err(e) if asks_again(&e) && pulled.allow(&e) => {
                if !program.make_asked(build_i)? {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        }
    }
    let mut owners = 0usize;
    // Phase 1: lay out coroutine states and build LIR ONCE into an owned `BuiltPhase`. Any
    // missing fact (`Missing` / `OpaqueOwnerUnchecked` / `NoTrait` / `Check`) retries only Phase 1.
    let mut asked = AskProgress::new_in(alloc);
    let built_phase = loop {
        let MakerState { checked, tables, work, .. } = &mut *state;
        let t = tables.take().ok_or_else(|| tables_gone(tabled))?;
        let bodies = Bodies { checked: &checked[..], closures: t.closures, coroutines: t.coroutines, const_at: t.const_at, overflow: work.overflow, ub: work.ub, panic: work.panic, alloc: work.alloc };
        let step = build_lir_phase(&*program, &bodies, work, build_i, build_instance);
        *tables = Some(Tables { closures: bodies.closures, coroutines: bodies.coroutines, const_at: bodies.const_at });
        match step {
            Ok(Phase1::Built(built)) => break built,
            Ok(Phase1::Check(owner)) => {
                owners += 1;
                if owners > MOST_HELD_COROUTINES {
                    let error = BuildError::UnsupportedSig { what: "an instance whose coroutine states needed more than 4096 owner fns checked" };
                    return Err(RunError::Build { function: program.function(build_i).named(), error, range: None });
                }
                if state.ensure(program, typing, owner, checks)?.is_none() {
                    return Err(unchecked_owner(&*program, build_i, owner));
                }
            }
            Ok(Phase1::PullConstants(missing)) => {
                owners += 1;
                if owners > MOST_HELD_COROUTINES {
                    let error = BuildError::UnsupportedSig { what: "an instance whose coroutine states needed more than 4096 constant pull rounds" };
                    return Err(RunError::Build { function: program.function(build_i).named(), error, range: None });
                }
                for &(owner, owner_args) in missing.iter() {
                    let Some(owner_i) = state.ensure(program, typing, owner, checks)? else {
                        return Err(unchecked_owner(&*program, build_i, owner));
                    };
                    let owner_inst = Instance { item: owner, args: owner_args };
                    let mut owner_pulled = AskProgress::new_in(alloc);
                    loop {
                        match pull_constants(program, state, owner_i, owner_inst) {
                            Ok(PullStep::Done) => break,
                            Ok(PullStep::Check(co_owner)) => {
                                pull_owners += 1;
                                if pull_owners > MOST_HELD_COROUTINES {
                                    let error = BuildError::UnsupportedSig { what: "an instance whose constant evaluation needed more than 4096 coroutine owner fns checked" };
                                    return Err(RunError::Build { function: program.function(build_i).named(), error, range: None });
                                }
                                if state.ensure(program, typing, co_owner, checks)?.is_none() {
                                    return Err(unchecked_owner(&*program, build_i, co_owner));
                                }
                            }
                            Err(RunError::Build { error: BuildError::OpaqueOwnerUnchecked { owner: op_owner, .. }, .. })
                                if pull_owners < MOST_HELD_COROUTINES
                                    && match state.asked.binary_search_by_key(&op_owner, |r| r.0) {
                                        Err(_) => true,
                                        Ok(slot) => state.asked.get(slot).is_some_and(|e| e.1.is_none()) && program.fn_index(op_owner).is_none(),
                                    } =>
                            {
                                pull_owners += 1;
                                if state.ensure(program, typing, op_owner, checks)?.is_none() {
                                    let Some(row) = program.head_row(op_owner)? else {
                                        return Err(unchecked_owner(&*program, build_i, op_owner));
                                    };
                                    if let Ok(slot) = state.asked.binary_search_by_key(&op_owner, |r| r.0)
                                        && let Some(entry) = state.asked.get_mut(slot)
                                    {
                                        entry.1 = Some(row);
                                    }
                                }
                            }
                            Err(e) if asks_again(&e) && owner_pulled.allow(&e) => {
                                if !program.make_asked(owner_i)? {
                                    return Err(e);
                                }
                            }
                            Err(e) => return Err(e),
                        }
                    }
                }
            }
            Err(e) if asks_again(&e) && asked.allow(&e) => {
                if program.make_asked(build_i)? {
                    continue;
                }
                return Err(e);
            }
            Err(RunError::Build { error: BuildError::OpaqueOwnerUnchecked { owner, .. }, .. })
                if owners < MOST_HELD_COROUTINES
                    && match state.asked.binary_search_by_key(&owner, |r| r.0) {
                        Err(_) => true,
                        Ok(slot) => state.asked.get(slot).is_some_and(|e| e.1.is_none()) && program.fn_index(owner).is_none(),
                    } =>
            {
                owners += 1;
                if state.ensure(program, typing, owner, checks)?.is_none() {
                    let Some(row) = program.head_row(owner)? else {
                        return Err(unchecked_owner(&*program, build_i, owner));
                    };
                    if let Ok(slot) = state.asked.binary_search_by_key(&owner, |r| r.0)
                        && let Some(entry) = state.asked.get_mut(slot)
                    {
                        entry.1 = Some(row);
                    }
                }
            }
            Err(e) if let Some(t) = object_missing(&e) && asked.allow(&e) => {
                if program.object_table(t)? {
                    continue;
                }
                return Err(e);
            }
            Err(e) => return Err(e),
        }
    };
    // Phase 2: resolve all call refs and vtables over the ALREADY-BUILT LIR, batch-collecting all
    // missing impls/ADTs/object tables/head rows in one sweep per round without ever re-running
    // `blaze_build::build`, and using `program.head_row` (not `state.ensure`) for untabled callees.
    let mut ref_asked = AskProgress::new_in(alloc);
    let mut tried_table: blaze_hash::HashSet<ItemKey, Alloc<'_>> = blaze_hash::HashSet::new_in(alloc);
    let ref_reads = ReadLog::new(ReadPlant::None);
    let mut built_phase = Some(built_phase);
    loop {
        ref_reads.clear();
        let MakerState { checked, tables, work, .. } = &mut *state;
        let t = tables.take().ok_or_else(|| tables_gone(tabled))?;
        let bodies = Bodies { checked: &checked[..], closures: t.closures, coroutines: t.coroutines, const_at: t.const_at, overflow: work.overflow, ub: work.ub, panic: work.panic, alloc: work.alloc };
        let phase_step = resolve_and_emit_phase(
            &*program,
            &bodies,
            work,
            build_i,
            build_instance,
            &mut built_phase,
            &ref_reads,
            sink,
        );
        *tables = Some(Tables { closures: bodies.closures, coroutines: bodies.coroutines, const_at: bodies.const_at });
        match phase_step? {
            Phase2::Done => break,
            Phase2::Needs(gaps) => {
                let mut made_any = false;
                let mut unrecovered: Option<RunError> = None;
                if let Some(e) = gaps.first_ask {
                    if ref_asked.allow(&e) {
                        if program.make_asked(build_i)? {
                            made_any = true;
                        } else {
                            unrecovered.get_or_insert(e);
                        }
                    } else {
                        unrecovered.get_or_insert(e);
                    }
                }
                for (t, e) in gaps.missing_objects {
                    if ref_asked.allow(&e) && program.object_table(t)? {
                        made_any = true;
                    } else {
                        unrecovered.get_or_insert(e);
                    }
                }
                if !gaps.untabled.is_empty() {
                    for (key, refusal) in gaps.untabled {
                        owners += 1;
                        if owners > MOST_HELD_COROUTINES || !tried_table.insert(key) {
                            return Err(refusal);
                        }
                        if program.head_row(key)?.is_some() {
                            made_any = true;
                        } else if state.asked.binary_search_by_key(&key, |r| r.0).is_ok() {
                            return Err(refusal);
                        } else {
                            state.ensure(program, typing, key, checks)?;
                            made_any = true;
                        }
                    }
                }
                if !made_any {
                    if let Some(e) = unrecovered {
                        return Err(e);
                    }
                    return Err(RunError::OneBody { key: tabled, why: "phase 2 reported missing facts without making progress" });
                }
            }
        }
    }
    // No `done` here: the walk may reach another instance of this fn at other arguments (a
    // generic fn, `pick` at `i32` then at `u8`), whose make reads the same lowered body, and a
    // body is checked once a run (the lazy program refuses a second `check_one`). The body is
    // held for the run beside its checked result (`MakerState::checked`, held for the run too).
    Ok(())
}

/// What an `asks_again` / `object_missing` retry round stopped on, for [`AskProgress`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum AskKey {
    Question(blaze_typeck::Question),
    Adt(ItemKey),
    Object(ItemKey),
    Row(ReadKind, ItemKey),
}

fn build_error_ask_key(error: &BuildError) -> Option<AskKey> {
    match error {
        BuildError::Select(blaze_typeck::ResolveError::Missing(q)) => Some(AskKey::Question(*q)),
        BuildError::NoAdt { adt } | BuildError::MissingRecord { item: adt } => Some(AskKey::Adt(*adt)),
        _ => None,
    }
}

fn layout_refusal_ask_key(why: &blaze_build::layout::Refusal) -> Option<AskKey> {
    use blaze_build::layout::{Missing, Refusal};
    match why {
        Refusal::NoAdt(adt) | Refusal::Missing(Missing { item: adt }) => Some(AskKey::Adt(*adt)),
        Refusal::Ground(error) | Refusal::FieldGround { error, .. } => build_error_ask_key(error),
        _ => None,
    }
}

fn ask_key(e: &RunError) -> Option<AskKey> {
    match e {
        RunError::Build { error, .. } => build_error_ask_key(error).or_else(|| object_missing(e).map(AskKey::Object)),
        RunError::ItemRefused { error, .. } => build_error_ask_key(error),
        RunError::Resolve { error: blaze_typeck::ResolveError::Missing(q), .. } => Some(AskKey::Question(*q)),
        RunError::NoAdt { adt, .. } => Some(AskKey::Adt(*adt)),
        RunError::Vtable { why: VtableFail::Layout(error), .. } => layout_refusal_ask_key(&error.why),
        RunError::Vtable { why: VtableFail::Build(error), .. } => build_error_ask_key(error),
        _ => object_missing(e).map(AskKey::Object),
    }
}

/// Consecutive rounds without a new distinct missing fact before stopping once past `MOST_ROUNDS`.
const MOST_ASK_STALLED: u32 = 64;

/// Progress-gated round bound for instance-level `make_asked` / `object_table` retry loops
/// (rule 18, matching `front2/check.rs`'s `MOST_PROGRESS_ROUNDS`): cold lazy builds of large
/// bodies read from typed records can legitimately fill >`MOST_ROUNDS` distinct `ImplsOf` /
/// `Adt` / `ObjectTrait` records across retries, while a stuck loop repeating the same missing
/// fact still stops at `MOST_ROUNDS`.
struct AskProgress<'a> {
    rounds: u32,
    stalled: u32,
    seen: blaze_hash::HashSet<AskKey, Alloc<'a>>,
}

impl<'a> AskProgress<'a> {
    fn new_in(alloc: Alloc<'a>) -> Self {
        Self { rounds: 0, stalled: 0, seen: blaze_hash::HashSet::new_in(alloc) }
    }

    fn allow(&mut self, e: &RunError) -> bool {
        self.allow_key(ask_key(e))
    }

    fn allow_key(&mut self, key: Option<AskKey>) -> bool {
        if self.rounds >= crate::front2::check::MOST_PROGRESS_ROUNDS {
            return false;
        }
        let fresh = key.is_some_and(|k| self.seen.insert(k));
        if fresh {
            self.stalled = 0;
        } else {
            self.stalled += 1;
        }
        if self.rounds >= crate::front2::check::MOST_ROUNDS && self.stalled >= MOST_ASK_STALLED {
            return false;
        }
        self.rounds += 1;
        true
    }
}

/// The trait a refused build found no object table for: the walk's own (blaze-build
/// `BuildError::Vtable`, a dynamic call or an upcast) or a vtable record's words
/// ([`vtable_rows`]' shape). `None` for every other refusal.
fn object_missing(e: &RunError) -> Option<ItemKey> {
    use blaze_build::vtable::VtableError;
    match e {
        RunError::Build { error: BuildError::Vtable { error: VtableError::NoTrait(t), .. }, .. } => Some(*t),
        RunError::Vtable { why: VtableFail::Shape(VtableError::NoTrait(t)), .. } => Some(*t),
        _ => None,
    }
}

/// The owners the ground arguments `args` of an instance of `tabled` hold, each once, in the
/// order first met: a closure's or a coroutine's owner, and an opaque type's defining fn; never
/// `tabled` itself (checked first); and `.1`, each fn item whose type they hold (`PNode::FnDef`,
/// a closure's parent arguments included), each once, whose row the make takes from its head.
/// Each distinct node is visited once (ground types are interned: one id per type), so the
/// walk is linear in the arguments' distinct nodes; bounded (rule 18).
fn held_owners<'a, P: FrontEnd>(program: &P, ground: blaze_typeck::PTys<'_>, args: blaze_typeck::List, tabled: ItemKey, alloc: Alloc<'a>) -> Result<Held<'a>, RunError> {
    /// The most distinct type nodes one instance's arguments are walked through (rule 18).
    const MOST_NODES: usize = 1 << 20;
    let malformed = |_| RunError::OneBody { key: tabled, why: "an instance's ground arguments refer past their table (make_one.rs `held_owners`)" };
    let mut owners: AVec<'a, ItemKey> = blaze_own::Vec::new_in(alloc);
    let mut items: AVec<'a, ItemKey> = blaze_own::Vec::new_in(alloc);
    let mut objects: AVec<'a, ItemKey> = blaze_own::Vec::new_in(alloc);
    let mut seen: AVec<'a, blaze_typeck::PTy> = blaze_own::Vec::new_in(alloc);
    let mut todo: AVec<'a, blaze_typeck::PTy> = blaze_own::Vec::new_in(alloc);
    todo.extend_from_slice(ground.list(args).map_err(malformed)?);
    while let Some(t) = todo.pop() {
        let Err(slot) = seen.binary_search(&t) else { continue };
        if seen.len() >= MOST_NODES {
            return Err(RunError::OneBody { key: tabled, why: "an instance whose ground arguments hold more distinct types than the most one make walks (2^20)" });
        }
        seen.insert_shift(slot, t);
        let node = ground.node(t).map_err(malformed)?;
        if let blaze_typeck::PNode::FnDef(item, _) = node
            && !items.contains(&item)
        {
            items.push(item);
        }
        if let blaze_typeck::PNode::Dynamic(bounds) = node
            && let Some(principal) = blaze_typeck::object_principal(ground, bounds)
            && !objects.contains(&principal)
        {
            objects.push(principal);
        }
        let owner = match node {
            blaze_typeck::PNode::Closure { owner, .. } | blaze_typeck::PNode::Coroutine { owner, .. } => Some(owner),
            // Its defining fn (`FrontEnd::opaque_definer`); a miss is the build's refusal.
            blaze_typeck::PNode::Opaque(opaque, _) => program.opaque_definer(opaque).and_then(Result::ok),
            _ => None,
        };
        if let Some(owner) = owner
            && owner != tabled
            && !owners.contains(&owner)
        {
            owners.push(owner);
        }
        ground.children_of(node, &mut todo).map_err(malformed)?;
    }
    Ok(Held { owners, items, objects })
}

/// What an instance's ground arguments hold that its make must have before its build
/// ([`held_owners`]): each asked by name, never left to another body's check.
struct Held<'a> {
    /// A closure's or a coroutine's owner, an opaque type's defining fn: checked.
    owners: AVec<'a, ItemKey>,
    /// Each fn item whose type they hold: its row from its head.
    items: AVec<'a, ItemKey>,
    /// Each `dyn` principal: its object table (`FrontEnd::object_table`), so the build's
    /// vtables and dynamic calls at these arguments find it (D194).
    objects: AVec<'a, ItemKey>,
}

/// Result of [`pull_constants`]: either every constant need was pulled, or a coroutine held by
/// one of the constant needs' arguments requires its owner fn to be checked first.
enum PullStep {
    Done,
    Check(ItemKey),
}

/// The per-item constant pull of build_from's step (`const_at::needs_of`, `const_at::pull`),
/// and every constant the body reads at no arguments (`const_at::plain_needs_of`): the walk's
/// builds and its verifier read every constant from the one pulled table (rule 30). A
/// closure's and a coroutine's function read their owner's body at the owner's arguments, as
/// `build_one` builds them, so they pull the owner's needs there.
fn pull_constants<P: FrontEnd>(program: &mut P, state: &mut MakerState, i: usize, instance: Instance) -> Result<PullStep, RunError> {
    let MakerState { checked, tables, work, .. } = &mut *state;
    work.needs.clear();
    let function = program.function(i);
    // A ctor's fn reads no constant: its fields in, its value out. Nor a builtin clone (a copy,
    // or its fields' own clones), nor a shim of a fn with no body.
    if (function.kind == FnKind::Ctor && function.body.is_none())
        || (function.body.is_none()
            && (instance.item != function.key
                || crate::clone_shim::language_clone(&*program, instance, &work.ground).is_some()
                || crate::clone_shim::language_from(&*program, instance, &work.ground).is_some()))
    {
        return Ok(PullStep::Done);
    }
    let (Some((body, _)), Some(done)) = (function.body, checked.get(i).and_then(Option::as_ref)) else {
        return Err(callee_refusal(&*program, function.named(), instance.args, instance, crate::callee_shown::CalleeGap::NoBody, work.ground.view()));
    };
    let malformed = |_| RunError::Build { function: function.named(), error: BuildError::Malformed { what: "an instance's arguments past the ground table" }, range: None };
    let t = tables.take().ok_or_else(|| tables_gone(function.key))?;
    let bodies = Bodies { checked: &checked[..], closures: t.closures, coroutines: t.coroutines, const_at: t.const_at, overflow: work.overflow, ub: work.ub, panic: work.panic, alloc: work.alloc };
    let view = body.view(program.body_types(i));
    let facts = CachedFront { front: FrontAdts(&*program, i, &[], &work.langs, &bodies), cache: &work.query_cache };
    // The arguments `build_one` reads the body at: the instance's own; a coroutine's
    // function's or a closure's owner's; a `Fn*` shim reads no body.
    let at = if instance.item == function.key {
        Some(instance.args)
    } else if let Some((of, _)) = crate::front2::coroutines::part_of(&facts, instance, &work.ground) {
        Some(of.owner_args)
    } else if crate::fn_shim::fn_shim_of(&facts, instance, &work.ground).is_some()
        || blaze_build::reify_shim_of(instance, work.ground.view()).is_some()
        || blaze_build::vtable_shim_of(instance, work.ground.view()).is_some()
        || blaze_build::test_report_shim_of(instance, work.ground.view()).is_some()
    {
        None
    } else {
        let first = work.ground.view().list(instance.args).ok().and_then(|a| a.first().copied());
        first.and_then(|t| blaze_build::closure_of(work.ground.view(), t)).map(|of| of.owner_args)
    };
    let mut args: AVec<'_, PTy> = blaze_own::Vec::new_in(work.alloc);
    let found = match at {
        None => Ok(()),
        Some(at) => match work.ground.view().list(at) {
            Ok(list) => {
                args.extend_from_slice(list);
                crate::front2::const_at::needs_of(&view, done, &args, &mut work.ground, &facts, work.alloc, &mut work.needs)
                    .and_then(|()| crate::front2::const_at::plain_needs_of(done, &mut work.needs))
                    .map_err(|error| RunError::Build { function: function.named(), error, range: None })
            }
            Err(e) => Err(malformed(e)),
        },
    };
    // Any coroutine held by value in a constant need's arguments (e.g. `Box::new(async move { .. })`
    // reading `<{coroutine} as SizedTypeProperties>::LAYOUT`): lay out its state from types first
    // (`lay_only`) so constant evaluation observes the exact same `StateShape` as the build.
    // Shapes remain recorded in `bodies.coroutines.states` (the coroutine's own `poll`/`resume`/`drop`
    // entry always forgets its own shape before `lay_and_build_one`), avoiding redundant re-laying.
    let mut unchecked_coro = None;
    let lay_result = if found.is_ok() && !work.needs.is_empty() {
        let mut need_tys: AVec<'_, PTy> = blaze_own::Vec::new_in(work.alloc);
        for (_, a) in work.needs.iter() {
            need_tys.extend_from_slice(a);
        }
        need_tys.sort_unstable();
        need_tys.dedup();
        if need_tys.is_empty() {
            Ok(())
        } else {
            match crate::front2::coroutine_needs::referenced_coroutines(&mut work.ground, &need_tys, &facts, work.alloc) {
                Ok(coro_roots) if coro_roots.is_empty() => Ok(()),
                Ok(coro_roots) => match lay_only(&*program, &[], &work.langs, &bodies, &mut work.ground, &mut work.layouts, i, &coro_roots, work.alloc) {
                    Ok(LaidOnly::Done(_)) => Ok(()),
                    Ok(LaidOnly::Unchecked(owner)) => {
                        unchecked_coro = Some(owner);
                        Ok(())
                    }
                    Err(e) => Err(e),
                },
                Err(error) => Err(RunError::Build { function: function.named(), error, range: None }),
            }
        }
    } else {
        Ok(())
    };
    *tables = Some(Tables { closures: bodies.closures, coroutines: bodies.coroutines, const_at: bodies.const_at });
    found?;
    lay_result?;
    if let Some(owner) = unchecked_coro {
        return Ok(PullStep::Check(owner));
    }
    if work.needs.is_empty() {
        return Ok(PullStep::Done);
    }
    let named = function.named();
    let table = tables.as_mut().ok_or_else(|| tables_gone(instance.item))?;
    let pull_res = crate::front2::const_at::pull(
        &mut *program,
        &work.needs,
        work.ground.view(),
        &mut table.const_at,
        crate::front2::const_at::BuildFacts::new(&table.closures, &table.coroutines),
    );
    if let Err(refusal) = pull_res {
        let item = refusal.need.item();
        let path = program.item_named(item);
        return Err(RunError::ConstAt { function: named, refusal, path });
    }
    Ok(PullStep::Done)
}

/// Caching wrapper around [`FrontAdts`] backed by [`Work::query_cache`]. Sits strictly below
/// [`Recorded`] so every build's [`ReadLog`] records the exact same questions and answers while
/// repeated `normalize` and `drop_impl` queries over `Work::ground` hit in O(1).
struct CachedFront<'w, P> {
    front: FrontAdts<'w, P>,
    cache: &'w core::cell::RefCell<QueryCache>,
}

impl<P: FrontEnd> blaze_build::layout::LayoutFacts for CachedFront<'_, P> {
    fn adt(&self, key: ItemKey) -> Option<blaze_typeck::Adt<'_>> {
        self.front.0.adt(key)
    }

    fn repr(&self, key: ItemKey) -> Result<blaze_build::layout::ReprAttrs, blaze_build::layout::Missing> {
        self.front.repr(key)
    }

    fn discriminant(&self, adt: ItemKey, variant: u32) -> Result<Option<i128>, blaze_build::layout::Missing> {
        self.front.discriminant(adt, variant)
    }

    fn valid_range(&self, key: ItemKey) -> Result<blaze_build::layout::ValidRange, blaze_build::layout::Missing> {
        self.front.valid_range(key)
    }

    fn normalize(&self, trait_: ItemKey, name: blaze_typeck::Name, args: &[PTy], ground: &mut PortableTys<'_>) -> Result<PTy, BuildError> {
        let alloc = self.cache.borrow().alloc;
        let key = (trait_, name, SmallArgs::of(args, alloc));
        if let Some(&hit) = self.cache.borrow().normalize.get(&key) {
            return Ok(hit);
        }
        let res = self.front.normalize(trait_, name, args, ground)?;
        self.cache.borrow_mut().normalize.insert(key, res);
        Ok(res)
    }

    fn closure(&self, owner: ItemKey, ordinal: u32) -> Option<blaze_build::layout::ClosureFacts<'_>> {
        self.front.closure(owner, ordinal)
    }

    fn coroutine(&self, owner: ItemKey, ordinal: u32) -> Option<blaze_build::CoroutineFacts<'_>> {
        self.front.coroutine(owner, ordinal)
    }

    fn coroutine_state(&self, owner: ItemKey, ordinal: u32, args: &[PTy], ground: PTys<'_>) -> Result<Option<blaze_build::StateShape>, blaze_build::layout::Missing> {
        self.front.coroutine_state(owner, ordinal, args, ground)
    }

    fn opaque_hidden(&self, opaque: ItemKey, args: &[PTy], ground: &mut PortableTys<'_>) -> Result<PTy, BuildError> {
        crate::front2::coroutines::opaque_hidden(self.front.0, self.front.4.checked, self, opaque, args, ground, self.front.4.alloc)
    }
}

impl<P: FrontEnd> blaze_build::Adts for CachedFront<'_, P> {
    fn adt(&self, key: ItemKey) -> Option<blaze_typeck::Adt<'_>> { self.front.0.adt(key) }
    fn opaque_of(&self, opaque: ItemKey) -> Option<(ItemKey, u32)> { self.front.opaque_of(opaque) }
    fn overflow_checks(&self) -> OverflowChecks { self.front.overflow_checks() }
    fn ub_checks(&self) -> blaze_build::UbChecks { self.front.ub_checks() }
    fn panic(&self) -> blaze_build::Panic { self.front.panic() }
    fn facts(&self) -> &dyn blaze_build::layout::LayoutFacts { self }
    fn int_lit(&self, owner: ItemKey, expr: blaze_typeck::ExprId) -> Option<u128> { self.front.int_lit(owner, expr) }
    fn int_pat(&self, owner: ItemKey, pat: blaze_typeck::PatId) -> Option<u128> { self.front.int_pat(owner, pat) }
    fn byte_str(&self, owner: ItemKey, expr: blaze_typeck::ExprId) -> Option<&[u8]> { self.front.byte_str(owner, expr) }
    fn str_lit(&self, owner: ItemKey, expr: blaze_typeck::ExprId) -> Option<&str> { self.front.str_lit(owner, expr) }
    fn format_template(&self, owner: ItemKey, expr: blaze_typeck::ExprId) -> Option<&[blaze_build::FmtPiece<'_>]> { self.front.format_template(owner, expr) }
    fn fmt_argument_new(&self, trait_: blaze_typeck::FmtTrait) -> Option<ItemKey> { self.front.fmt_argument_new(trait_) }
    fn fmt_argument_adt(&self) -> Option<ItemKey> { self.front.fmt_argument_adt() }
    fn float_lit(&self, owner: ItemKey, expr: blaze_typeck::ExprId) -> Option<u128> { self.front.float_lit(owner, expr) }
    fn float_pat(&self, owner: ItemKey, pat: blaze_typeck::PatId) -> Option<u128> { self.front.float_pat(owner, pat) }
    fn str_pat(&self, owner: ItemKey, pat: blaze_typeck::PatId) -> Option<&[u8]> { self.front.str_pat(owner, pat) }
    fn fn_trait_method(&self, which: blaze_build::FnTrait) -> Option<ItemKey> { self.front.fn_trait_method(which) }
    fn lang_fn(&self, name: &str) -> Option<ItemKey> { self.front.lang_fn(name) }
    fn type_names(&self) -> Option<&dyn blaze_build::type_name::TypeNames> { self.front.type_names() }
    fn future_poll(&self) -> Option<ItemKey> { self.front.future_poll() }
    fn async_items(&self) -> Option<blaze_build::AsyncItems> { self.front.async_items() }

    fn drop_impl(&self, ty: PTy, ground: &mut PortableTys<'_>) -> Result<Option<ItemKey>, BuildError> {
        let Some((trait_, f)) = self.front.0.drop_trait() else { return Ok(None) };
        let PNode::Adt(adt, _) = ground.view().node(ty).map_err(|_| BuildError::Malformed { what: "a type past the ground table" })? else {
            return Ok(None);
        };
        let has_drop = if let Some(&cached) = self.cache.borrow().drop_impl.get(&adt) {
            cached
        } else {
            let found = self.front.0.adt_drop_impl(self.front.1, trait_, adt, ground.alloc()).map_err(BuildError::Select)?;
            self.cache.borrow_mut().drop_impl.insert(adt, found);
            found
        };
        if has_drop.is_none() {
            return Ok(None);
        }
        f.map(Some).ok_or(BuildError::Malformed { what: "the `drop` lang trait has no `drop` fn" })
    }

    fn index_fn(&self, mutbl: blaze_typeck::Mutbl) -> Option<ItemKey> { self.front.index_fn(mutbl) }
    fn deref_fn(&self, mutbl: blaze_typeck::Mutbl) -> Option<ItemKey> { self.front.deref_fn(mutbl) }
    fn lang_adt(&self, item: LangItem) -> Option<ItemKey> { self.front.lang_adt(item) }
    fn object_trait(&self, t: ItemKey) -> Option<blaze_build::vtable::ObjectTrait<'_>> { self.front.object_trait(t) }
    fn abi(&self, item: ItemKey) -> Option<blaze_build::Abi> { self.front.abi(item) }
    fn nounwind(&self, owner: ItemKey) -> Option<bool> { self.front.nounwind(owner) }
    fn fn_attrs(&self, owner: ItemKey) -> blaze_lir::func::FnAttrs { self.front.fn_attrs(owner) }
    fn track_caller(&self, instance: Instance, ground: &mut PortableTys<'_>) -> Option<bool> {
        if let Some(&hit) = self.cache.borrow().track_caller.get(&instance.item) {
            return Some(hit);
        }
        let ans = self.front.track_caller(instance, ground)?;
        self.cache.borrow_mut().track_caller.insert(instance.item, ans);
        Some(ans)
    }
    fn location_adt(&self) -> Option<ItemKey> { self.front.location_adt() }
    fn caller_site(&self, owner: ItemKey, expr: blaze_typeck::ExprId) -> Result<blaze_build::CallerSite<'_>, blaze_build::SiteGap> { self.front.caller_site(owner, expr) }
    fn def_site(&self, item: ItemKey) -> Result<blaze_build::CallerSite<'_>, blaze_build::SiteGap> { self.front.def_site(item) }
    fn expr_site(&self, owner: ItemKey, expr: blaze_typeck::ExprId) -> Option<u32> { self.front.expr_site(owner, expr) }
    fn c_variadic(&self, item: ItemKey) -> Option<u32> { self.front.c_variadic(item) }
    fn legacy_const_generics(&self, item: ItemKey) -> &[u32] { self.front.legacy_const_generics(item) }
    fn static_item(&self, item: ItemKey) -> Option<StaticItem> { self.front.static_item(item) }
    fn const_value(&self, item: ItemKey, args: &[PTy]) -> Option<&crate::seam::SeamData> { self.front.const_value(item, args) }
    fn const_block_value(&self, owner: ItemKey, block: blaze_typeck::ExprId, args: &[PTy]) -> Option<&crate::seam::SeamData> { self.front.const_block_value(owner, block, args) }
    fn promoted_value(&self, owner: ItemKey, borrow: blaze_typeck::ExprId, args: &[PTy]) -> Option<&crate::seam::SeamData> { self.front.promoted_value(owner, borrow, args) }
    fn const_fn_instance(&self, key: blaze_build::InstanceKey, ground: &mut PortableTys<'_>) -> Option<Instance> { self.front.const_fn_instance(key, ground) }
    fn const_vtable_ref(&self, key: blaze_build::vtable::VtableKey, ground: &mut PortableTys<'_>) -> Option<blaze_build::vtable::VtableRef> { self.front.const_vtable_ref(key, ground) }
    fn intrinsic(&self, item: ItemKey) -> Option<blaze_build::Intrinsic> { self.front.intrinsic(item) }
}

/// The one interface a build asks the world through (F20; rule 30: the recorder of
/// code_reads.rs, no second one): the build's facts through [`Recorded`] over [`CachedFront`]
/// ([`Asking::world`]), and the questions beyond them (a call's instance, a C symbol) by its
/// own methods, each logged in `log` as it is answered.
struct Asking<'w, P> {
    front: CachedFront<'w, P>,
    log: &'w ReadLog,
}

impl<'w, P: FrontEnd> Asking<'w, P> {
    fn new(
        program: &'w P,
        i: usize,
        langs: &'w BackLangs,
        bodies: &'w Bodies<'w>,
        cache: &'w core::cell::RefCell<QueryCache>,
        log: &'w ReadLog,
    ) -> Asking<'w, P> {
        Asking { front: CachedFront { front: FrontAdts(program, i, &[], langs, bodies), cache }, log }
    }

    /// The build's facts, each answer logged.
    fn world(&self) -> Recorded<'_> {
        Recorded::new(&self.front, self.log)
    }

    /// The checked bodies (not a fact of the world: which fns this run checked).
    fn checked(&self) -> &'w [Option<Checked<'static>>] {
        self.front.front.4.checked
    }

    /// The instance a call of `called` from this build's fn runs (the seam's `resolve`, in the
    /// fn's world), logged (`ReadKind::Resolve`); a refusal is logged as no instance.
    fn resolve(&self, called: Instance, ground: &mut PortableTys<'_>, scratch: Alloc<'_>) -> Result<(ItemKey, List), blaze_typeck::ResolveError> {
        let key = (called.item, called.args);
        if let Some(&hit) = self.front.cache.borrow().resolve.get(&key) {
            self.log.ask_resolve(called, Some(hit), ground.view(), scratch);
            return Ok(hit);
        }
        let answer = self.front.front.0.resolve(self.front.front.1, called.item, called.args, ground);
        if let Ok(hit) = answer {
            self.front.cache.borrow_mut().resolve.insert(key, hit);
        }
        self.log.ask_resolve(called, answer.as_ref().ok().copied(), ground.view(), scratch);
        answer
    }

    /// `called` with its fn item self resolved (fn_item_resolve.rs), that resolution logged.
    fn shim_self(&self, called: Instance, ground: &mut PortableTys<'static>, scratch: Alloc<'_>) -> Result<Instance, blaze_typeck::ResolveError> {
        let world = self.world();
        crate::fn_item_resolve::shim_self_resolved_by(self.front.front.0, &world, self.front.front.1, called, ground, |item, args, ground| {
            self.resolve(Instance { item, args }, ground, scratch)
        })
    }

    /// Static `s`'s C symbol (`None`: a record of its own), logged.
    fn foreign_static(&self, s: ItemKey) -> Option<&'w str> {
        let answer = self.front.front.0.foreign_static(s);
        self.log.ask_foreign_static(s, answer);
        answer
    }

    /// Fn row `c`'s C symbol when it is an `extern` block's fn (`None`: an instance of its
    /// own), logged under its item.
    fn foreign_fn(&self, c: usize) -> Option<&'w str> {
        let f = self.front.front.0.function(c);
        let answer = (f.kind == FnKind::Foreign).then_some(f.symbol);
        self.log.ask_foreign_fn(f.key, answer);
        answer
    }
}

/// The questions of a read list beyond `Adts`, asked again of today's program in fn `i`'s
/// world, as [`Asking`] asked them.
struct Today<'p, P> {
    program: &'p P,
    i: usize,
    /// Each coroutine the rows name, laid again in full and digested (lay_states.rs `relay`).
    laid: &'p [(blaze_build::CoroutineOf, ContentId)],
}

impl<P: FrontEnd> reads::Beyond for Today<'_, P> {
    fn resolve(&mut self, item: ItemKey, args: List, ground: &mut PortableTys<'_>) -> Option<(ItemKey, List)> {
        self.program.resolve(self.i, item, args, ground).ok()
    }

    fn foreign_static(&mut self, item: ItemKey) -> Result<Option<&str>, ReadsError> {
        Ok(self.program.foreign_static(item))
    }

    fn foreign_fn(&mut self, item: ItemKey) -> Result<Option<&str>, ReadsError> {
        // Its row was made from its head before the read list was asked ([`verify_instance`]);
        // no row today: the row does not hold (code_reads.rs `verify_today`).
        let Some(c) = self.program.fn_index(item) else {
            return Err(ReadsError::Unanswered { kind: ReadKind::ForeignFn });
        };
        let f = self.program.function(c);
        Ok((f.kind == FnKind::Foreign).then_some(f.symbol))
    }

    fn laid(&mut self, owner: ItemKey, ordinal: u32, args: List) -> Result<ContentId, ReadsError> {
        // `args` and each lay's were interned in the one ground table, so equal lists are one.
        self.laid
            .iter()
            .find(|(c, _)| c.owner == owner && c.ordinal == ordinal && c.owner_args == args)
            .map(|&(_, digest)| digest)
            .ok_or(ReadsError::Unanswered { kind: ReadKind::Laid })
    }
}

/// Whether code `entry`, found under an instance key of fn `tabled`, holds today: every row of
/// its read list asked again of today's program in `tabled`'s world (code_reads.rs
/// `verify_today`), no body checked but an opaque type's defining fn and the owner of a
/// coroutine whose state a row reads (laid again: lay_states.rs `relay`). Before the rows are
/// asked, what a fresh program answers only once made is made, by name, from each row: a fn's
/// head row (its ABI, unwinding, caller tracking, C symbol), a trait's object table, a static's
/// value, the constants the rows read (pulled as a make pulls them); an ADT head a row meets
/// unmade is made by the program's ask-again (`make_asked`) and the rows asked again, bounded
/// (rule 18). A refusal of any of these is a row that does not hold (`Stale`): the make that
/// follows meets the same question and refuses it by name, so nothing is passed over (rule 3).
///
/// Cost per found entry of N rows: each row a binary search of the run's [`Answers`] (a
/// question many entries ask, the same ADT's layout, is asked of the front end once a run), or
/// one decode, one front-end answer and one digest; the preparation above is once per item a
/// run. Answers are kept only from an entry that held.
pub fn verify_instance<'n, T: Typing<RecordsProgram<'n>>>(
    program: &mut RecordsProgram<'n>,
    typing: &mut T,
    state: &mut MakerState,
    checks: &mut CheckCount,
    tabled: ItemKey,
    entry: &[u8],
) -> Result<Verdict, RunError> {
    let Some(i) = program.head_row(tabled)? else {
        return Err(RunError::OneBody { key: tabled, why: "a found code entry's fn has no row today (make_one.rs `verify_instance`)" });
    };
    let refused = |program: &RecordsProgram<'n>, error| RunError::CodeRecord { function: program.function(i).named(), error };
    let view = EntryView::read(entry).map_err(|e| refused(&*program, CodeRecordError::Entry(e)))?;
    let alloc = state.work.alloc;
    let mut owners: AVec<'_, ItemKey> = blaze_own::Vec::new_in(alloc);
    // The coroutines whose states the rows read: laid again below, as the build's lay laid
    // them, before any row is asked.
    let mut roots: AVec<'_, blaze_build::CoroutineOf> = blaze_own::Vec::new_in(alloc);
    state.work.needs.clear();
    for row in view.reads() {
        let (bytes, _) = row.map_err(|e| refused(&*program, CodeRecordError::Entry(e)))?;
        // A row of no kind: `verify_today` refuses it by name below.
        let Some(kind) = bytes.first().copied().and_then(ReadKind::of) else { continue };
        match kind {
            ReadKind::Laid | ReadKind::CoroutineState | ReadKind::CoroutineNiche
            | ReadKind::Abi
            | ReadKind::Nounwind
            | ReadKind::TrackCaller
            | ReadKind::CVariadic
            | ReadKind::LegacyConstGenerics
            | ReadKind::ForeignFn
            | ReadKind::ObjectTrait
            | ReadKind::OpaqueHidden
            | ReadKind::Static
            | ReadKind::Const
            | ReadKind::ConstBlock
            | ReadKind::Promoted => {}
            _ => continue,
        }
        let (kind, extra, item, list) = reads::question(bytes, &mut state.work.ground, alloc).map_err(|e| refused(&*program, CodeRecordError::Reads(e)))?;
        let stale = Verdict::Stale { kind, item };
        let expr = u32::try_from(extra).ok().map(blaze_typeck::ExprId);
        let need = match kind {
            ReadKind::Laid | ReadKind::CoroutineState | ReadKind::CoroutineNiche => {
                let Ok(ordinal) = u32::try_from(extra) else {
                    return Err(refused(&*program, CodeRecordError::Reads(ReadsError::Question)));
                };
                let c = blaze_build::CoroutineOf { owner: item, owner_args: list, ordinal };
                if !roots.contains(&c) {
                    roots.push(c);
                }
                continue;
            }
            // Each answered from the fn's row (lazy front_end.rs: tabled rows only): its row
            // made from its head; the row's answer decides whether it holds.
            ReadKind::Abi | ReadKind::Nounwind | ReadKind::TrackCaller | ReadKind::CVariadic | ReadKind::LegacyConstGenerics | ReadKind::ForeignFn => {
                if program.head_row(item).is_err() {
                    return Ok(stale);
                }
                continue;
            }
            ReadKind::ObjectTrait => {
                if program.object_table(item).is_err() {
                    return Ok(stale);
                }
                continue;
            }
            ReadKind::OpaqueHidden => {
                // Named by its owner row (code_reads.rs `Recorded::opaque_hidden`): `item` is
                // the fn whose head made it, checked here, by name, though no body of this
                // program named it yet (a fn declared in a body); an alias's is its defining
                // fn (`FrontEnd::opaque_definer`'s rule). A miss is the build's refusal.
                let Ok(index) = u32::try_from(extra) else {
                    return Err(refused(&*program, CodeRecordError::Reads(ReadsError::Question)));
                };
                let definer = if crate::front2::tait::of_alias(index) { program.alias_definer(item).ok() } else { Some(item) };
                if let Some(owner) = definer
                    && !owners.contains(&owner)
                {
                    owners.push(owner);
                }
                continue;
            }
            ReadKind::Static => {
                if program.make_static_value(item).is_err() {
                    return Ok(stale);
                }
                continue;
            }
            ReadKind::Const => ConstNeed::Item(item),
            ReadKind::ConstBlock | ReadKind::Promoted => {
                let Some(expr) = expr else {
                    return Err(refused(&*program, CodeRecordError::Reads(ReadsError::Question)));
                };
                match kind {
                    ReadKind::ConstBlock => ConstNeed::Block { owner: item, block: expr },
                    _ => ConstNeed::Promoted { owner: item, borrow: expr },
                }
            }
            _ => continue,
        };
        let args = state.work.ground.view().list(list).map_err(|_| refused(&*program, CodeRecordError::Reads(ReadsError::Question)))?;
        state.work.needs.push(need, args).map_err(|error| RunError::Build { function: program.function(i).named(), error, range: None })?;
    }
    // An opaque type's hidden type is its defining fn's check's (coroutines.rs `opaque_hidden`);
    // a required trait fn's RPITIT has no body and needs only its signature row (`head_row`).
    for &owner in owners.iter() {
        match state.ensure(program, typing, owner, checks) {
            Ok(Some(_)) => {}
            Ok(None) => {
                let Ok(Some(row)) = program.head_row(owner) else {
                    return Ok(Verdict::Stale { kind: ReadKind::OpaqueHidden, item: owner });
                };
                if let Ok(slot) = state.asked.binary_search_by_key(&owner, |r| r.0)
                    && let Some(entry) = state.asked.get_mut(slot)
                {
                    entry.1 = Some(row);
                }
            }
            Err(_) => return Ok(Verdict::Stale { kind: ReadKind::OpaqueHidden, item: owner }),
        }
    }
    // Also include any coroutine held by value in the constant needs' arguments (e.g.
    // `Box::new(async move { .. })` verifying `<{coroutine} as SizedTypeProperties>::LAYOUT`),
    // laid BEFORE `const_at::pull` below so constant evaluation sees their laid `StateShape`s.
    if !state.work.needs.is_empty() {
        let mut need_tys: AVec<'_, PTy> = blaze_own::Vec::new_in(alloc);
        for (_, a) in state.work.needs.iter() {
            need_tys.extend_from_slice(a);
        }
        need_tys.sort_unstable();
        need_tys.dedup();
        let mut rounds = 0u32;
        while !need_tys.is_empty() {
            let found = {
                let MakerState { checked, tables, work, .. } = &mut *state;
                let t = tables.take().ok_or_else(|| tables_gone(tabled))?;
                let bodies = Bodies { checked: &checked[..], closures: t.closures, coroutines: t.coroutines, const_at: t.const_at, overflow: work.overflow, ub: work.ub, panic: work.panic, alloc: work.alloc };
                let facts = FrontAdts(&*program, i, &[], &work.langs, &bodies);
                let found = crate::front2::coroutine_needs::referenced_coroutines(&mut work.ground, &need_tys, &facts, work.alloc);
                *tables = Some(Tables { closures: bodies.closures, coroutines: bodies.coroutines, const_at: bodies.const_at });
                found
            };
            match found {
                Ok(extra) => {
                    for c in extra {
                        if roots.scan_for(&c).is_none() {
                            roots.push(c);
                        }
                    }
                    break;
                }
                Err(BuildError::OpaqueOwnerUnchecked { owner, .. }) if rounds < crate::front2::check::MOST_ROUNDS => {
                    rounds += 1;
                    if state.ensure(program, typing, owner, checks).is_err() {
                        return Ok(Verdict::Stale { kind: ReadKind::Const, item: tabled });
                    }
                }
                Err(_) if rounds < crate::front2::check::MOST_ROUNDS && matches!(program.make_asked(i), Ok(true)) => {
                    rounds += 1;
                }
                Err(_) => return Ok(Verdict::Stale { kind: ReadKind::Const, item: tabled }),
            }
        }
    }
    // The coroutine states the rows (and constant needs) read, laid again as the build's lay laid
    // them (lay_states.rs `relay`: the same order, the same `lay_one`), so a `CoroutineState` or
    // `CoroutineNiche` row or constant evaluation is asked of the same facts and a `Laid` row
    // compared with the same digest: a row moves only when its answer does. An owner not checked
    // is checked and an ADT head a lay met unmade is made, then the lay again (bounded, rule 18);
    // a lay that cannot be made today leaves the entry stale, and the make that follows refuses by name.
    let mut relaid: AVec<'_, (blaze_build::CoroutineOf, ContentId)> = blaze_own::Vec::new_in(alloc);
    let mut recorded: AVec<'_, blaze_build::CoroutineOf> = blaze_own::Vec::new_in(alloc);
    let mut lays = 0usize;
    while !roots.is_empty() {
        lays += 1;
        let out = {
            let MakerState { checked, tables, work, .. } = &mut *state;
            let t = tables.take().ok_or_else(|| tables_gone(tabled))?;
            let bodies = Bodies { checked: &checked[..], closures: t.closures, coroutines: t.coroutines, const_at: t.const_at, overflow: work.overflow, ub: work.ub, panic: work.panic, alloc: work.alloc };
            let out = super::lay_states::relay(&*program, &work.langs, &bodies, &mut work.ground, &mut work.layouts, i, &roots, work.alloc);
            *tables = Some(Tables { closures: bodies.closures, coroutines: bodies.coroutines, const_at: bodies.const_at });
            out
        };
        let stale = Verdict::Stale { kind: ReadKind::Laid, item: tabled };
        match out {
            Ok(super::lay_states::Relaid::Done { recorded: r, laid }) => {
                recorded.extend_from_slice(&r);
                relaid.extend_from_slice(&laid);
                break;
            }
            Ok(super::lay_states::Relaid::Unchecked(owner)) => {
                if lays > MOST_HELD_COROUTINES || state.ensure(program, typing, owner, checks).is_err() {
                    return Ok(stale);
                }
            }
            Err(_) => {
                if lays > crate::front2::check::MOST_ROUNDS as usize || !matches!(program.make_asked(i), Ok(true)) {
                    return Ok(stale);
                }
            }
        }
    }
    // The constants the rows read, pulled as a make pulls them (`pull_constants`), with the
    // relaid coroutine states visible to constant evaluation.
    let mut pulled = 0u32;
    let mut pull_stale = None;
    while !state.work.needs.is_empty() {
        let table = state.tables.as_mut().ok_or_else(|| tables_gone(tabled))?;
        let Err(refusal) = crate::front2::const_at::pull(
            &mut *program,
            &state.work.needs,
            state.work.ground.view(),
            &mut table.const_at,
            crate::front2::const_at::BuildFacts::new(&table.closures, &table.coroutines),
        ) else {
            break;
        };
        if pulled < crate::front2::check::MOST_ROUNDS && matches!(program.make_asked(i), Ok(true)) {
            pulled += 1;
            continue;
        }
        let kind = match refusal.need {
            ConstNeed::Item(_) => ReadKind::Const,
            ConstNeed::Block { .. } => ReadKind::ConstBlock,
            ConstNeed::Promoted { .. } => ReadKind::Promoted,
        };
        pull_stale = Some(Verdict::Stale { kind, item: refusal.need.item() });
        break;
    }
    let verdict = match pull_stale {
        Some(stale) => Ok(stale),
        None => verify_rows(program, state, i, tabled, &view, &relaid),
    };
    // The states laid again above were built into nothing: forgotten, so a make that follows
    // lays and builds them (lay_states.rs `forget_laid`).
    if let Some(t) = state.tables.as_ref() {
        forget_laid_in(&t.coroutines, &recorded, state.work.ground.view());
    }
    verdict
}

/// [`verify_instance`]'s asking: every row of `view` asked again (code_reads.rs
/// `verify_today`) over the states laid again (`relaid`), by rounds while the program makes
/// what the rows noted unmade.
fn verify_rows<'n>(program: &mut RecordsProgram<'n>, state: &mut MakerState, i: usize, tabled: ItemKey, view: &EntryView<'_>, relaid: &[(blaze_build::CoroutineOf, ContentId)]) -> Result<Verdict, RunError> {
    let refused = |program: &RecordsProgram<'n>, error| RunError::CodeRecord { function: program.function(i).named(), error };
    let mut rounds = AskProgress::new_in(state.work.alloc);
    loop {
        let MakerState { checked, tables, work, .. } = &mut *state;
        let t = tables.take().ok_or_else(|| tables_gone(tabled))?;
        let bodies = Bodies { checked: &checked[..], closures: t.closures, coroutines: t.coroutines, const_at: t.const_at, overflow: work.overflow, ub: work.ub, panic: work.panic, alloc: work.alloc };
        let verified = {
            let front = FrontAdts(&*program, i, &[], &work.langs, &bodies);
            let mut today = Today { program: &*program, i, laid: relaid };
            reads::verify_today(view, &front, &mut today, &mut work.answers, true, &mut work.ground, work.alloc)
        };
        *tables = Some(Tables { closures: bodies.closures, coroutines: bodies.coroutines, const_at: bodies.const_at });
        let verified = verified.map_err(|e| refused(&*program, CodeRecordError::Reads(e)))?;
        match verified.verdict {
            Verdict::Holds => return Ok(Verdict::Holds),
            // A row asked before the record it reads was made (a lazy front's unmade ADT head:
            // its `repr`, `valid_range`, `discriminant` answer `Missing`, D194): a NEED, never
            // compared. What the rows noted is made and they are asked again; nothing made (the
            // item is gone today), the entry is stale and the make that follows meets the
            // question itself.
            Verdict::Needs { kind, item, .. } => {
                if rounds.allow_key(Some(AskKey::Row(kind, item))) && matches!(program.make_asked(i), Ok(true)) {
                    continue;
                }
                return Ok(Verdict::Stale { kind, item });
            }
            // A row answered otherwise: what the rows asked is made (a row may differ only for
            // a fact asked of another, unmade record) and they are asked again; nothing made,
            // the entry is stale.
            stale @ Verdict::Stale { kind, item } => {
                if rounds.allow_key(Some(AskKey::Row(kind, item))) && matches!(program.make_asked(i), Ok(true)) {
                    continue;
                }
                return Ok(stale);
            }
        }
    }
}

/// Phase 1 of making one instance: lays out coroutine states and lowers the body to LIR
/// (`blaze_build::build`), recording Phase 1's reads in `work.reads`. Returns an owned
/// [`BuiltPhase`] so Phase 2 can retry reference/vtable resolution without ever rebuilding LIR.
fn build_lir_phase<P: FrontEnd>(
    program: &P,
    bodies: &Bodies<'_>,
    work: &mut Work,
    i: usize,
    instance: Instance,
) -> Result<Phase1<Global>, RunError> {
    let function = program.function(i);
    let name = || function.named();
    let ctor_row = match (function.kind, function.body) {
        (FnKind::Ctor, None) => program.ctor_of(function.key).map(|(_, variant)| variant),
        _ => None,
    };
    let builtin_clone = match function.body {
        None if instance.item == function.key => crate::clone_shim::language_clone(program, instance, &work.ground),
        _ => None,
    };
    let builtin_from = match function.body {
        None if instance.item == function.key && builtin_clone.is_none() => crate::clone_shim::language_from(program, instance, &work.ground),
        _ => None,
    };
    let is_vtable_shim = instance.item != function.key && blaze_build::vtable_shim_of(instance, work.ground.view()).is_some();
    let is_reify_shim = instance.item != function.key && blaze_build::reify_shim_of(instance, work.ground.view()).is_some();
    let is_test_report_shim = instance.item != function.key && blaze_build::test_report_shim_of(instance, work.ground.view()).is_some();
    let is_fn_shim = instance.item != function.key
        && crate::fn_shim::fn_shim_of(&FrontAdts(program, i, &[], &work.langs, bodies), instance, &work.ground).is_some();
    let checked = match (function.body, bodies.checked.get(i).and_then(Option::as_ref)) {
        (Some((body, _)), Some(done)) => Some((body, done)),
        _ if ctor_row.is_some()
            || builtin_clone.is_some()
            || builtin_from.is_some()
            || is_vtable_shim
            || is_reify_shim
            || is_test_report_shim
            || is_fn_shim =>
        {
            None
        }
        _ => return Err(callee_refusal(program, name(), instance.args, instance, crate::callee_shown::CalleeGap::NoBody, work.ground.view())),
    };
    let malformed = |_| RunError::Build { function: name(), error: BuildError::Malformed { what: "an instance's arguments past the ground table" }, range: None };
    work.reads.clear();
    let asking = Asking::new(program, i, &work.langs, bodies, &work.query_cache, &work.reads);
    let adts = asking.world();
    let vtable_shim = if instance.item == function.key {
        None
    } else {
        blaze_build::vtable_shim_of(instance, work.ground.view()).map(|target| super::vtable_shim_plan(&adts, target, &work.ground))
    };
    let reify = if instance.item == function.key || vtable_shim.is_some() { None } else { blaze_build::reify_shim_of(instance, work.ground.view()) };
    let test_report = if instance.item == function.key || vtable_shim.is_some() || reify.is_some() {
        None
    } else {
        blaze_build::test_report_shim_of(instance, work.ground.view())
    };
    let coroutine = if instance.item == function.key || vtable_shim.is_some() || reify.is_some() || test_report.is_some() {
        None
    } else {
        crate::front2::coroutines::part_of(&adts, instance, &work.ground)
    };
    let shim = if instance.item == function.key || vtable_shim.is_some() || reify.is_some() || test_report.is_some() || coroutine.is_some() {
        None
    } else {
        crate::fn_shim::fn_shim_of(&adts, instance, &work.ground)
    };
    let closure = if instance.item == function.key || vtable_shim.is_some() || reify.is_some() || test_report.is_some() || shim.is_some() || coroutine.is_some() {
        None
    } else {
        let first = work.ground.view().list(instance.args).map_err(malformed)?.first().copied();
        let of = first.and_then(|t| blaze_build::closure_of(work.ground.view(), t));
        let which = [blaze_build::FnTrait::Fn, blaze_build::FnTrait::FnMut, blaze_build::FnTrait::FnOnce]
            .into_iter()
            .find(|&w| adts.fn_trait_method(w) == Some(instance.item));
        match (of, which) {
            (Some(of), Some(which)) => Some((of, which)),
            _ => return Err(callee_refusal(program, name(), instance.args, instance, crate::callee_shown::CalleeGap::NotOwnInstance, work.ground.view())),
        }
    };
    let owner_args = if let Some(plan) = vtable_shim {
        plan.owner_args
    } else if let Some(target) = reify {
        target.args
    } else if test_report.is_some() {
        List::EMPTY
    } else {
        match shim {
            Some(s) => s.item.map_or(List::EMPTY, |(_, fn_args)| fn_args),
            None => match coroutine {
                Some((of, _)) => of.owner_args,
                None => closure.map_or(instance.args, |(of, _)| of.owner_args),
            },
        }
    };
    let mut args: AVec<'_, PTy> = blaze_own::Vec::new_in(work.alloc);
    args.extend_from_slice(work.ground.view().list(owner_args).map_err(malformed)?);
    let twice = || RunError::Build { function: name(), error: BuildError::Malformed { what: "a coroutine's function its instance's build did not leave (asked for twice)" }, range: None };
    let from_coroutine: Option<(usize, usize)> = match coroutine {
        None => None,
        Some((of, part)) => {
            let find = |stash: &[BuiltParts]| stash.iter().position(|b| b.owner == of.owner && b.ordinal == of.ordinal && b.args[..] == args[..]);
            if let Some(at) = find(&work.stash[..])
                && work.stash.get(at).and_then(|b| b.parts.get(part.index())).is_some_and(Option::is_none)
            {
                bodies.coroutines.states.forget(of.owner, of.ordinal, &args);
                work.stash.swap_remove(at);
            }
            if find(&work.stash[..]).is_none() {
                // `of` may already have a types-only `StateShape` in `bodies.coroutines.states`
                // from an earlier non-coroutine caller (`lay_only`). Forget `of`'s own shape
                // (preserving already-laid child shapes) so `lay_and_build_one` lays and builds
                // only `of` itself into `work.stash`.
                bodies.coroutines.states.forget(of.owner, of.ordinal, &args);
                match lay_and_build_one(program, &[], &work.langs, bodies, &mut work.ground, &mut work.layouts, &mut work.stash, i, of, true, work.alloc)? {
                    Lay::Done => {}
                    Lay::Unchecked(owner) => return Ok(Phase1::Check(owner)),
                    Lay::PullConstants(missing) => return Ok(Phase1::PullConstants(missing)),
                }
            }
            let Some(at) = find(&work.stash[..]) else {
                let error = BuildError::Malformed { what: "a coroutine laid out and built whose functions are not in the stash (lay_states.rs)" };
                return Err(RunError::Build { function: name(), error, range: None });
            };
            if work.stash[at].parts[part.index()].is_none() {
                return Err(twice());
            }
            work.reads.absorb(&work.stash[at].reads);
            work.reads.ask_laid(of.owner, of.ordinal, &args[..], work.stash[at].laid);
            Some((at, part.index()))
        }
    };
    if from_coroutine.is_none() {
        let include_body = shim.is_none() && vtable_shim.is_none() && test_report.is_none() && builtin_clone.is_none() && builtin_from.is_none();
        let extra_args = (instance.args != owner_args).then_some(instance.args);
        let roots = roots_of_instance(program, bodies, &adts, i, &args, extra_args, include_body, &mut work.ground, work.alloc)?;
        if !roots.is_empty() {
            // Non-coroutine callers (e.g. `Pin::as_mut`, `DerefMut::deref_mut`, `Box::into_pin`,
            // `drop_glue`, async callers) only need each referenced coroutine's `StateShape`
            // (types only), never its LIR — each coroutine builds its own LIR lazily when its
            // `poll`/`resume`/`drop` entry is actually demanded.
            match lay_only(program, &[], &work.langs, bodies, &mut work.ground, &mut work.layouts, i, &roots, work.alloc)? {
                LaidOnly::Done(_) => {}
                LaidOnly::Unchecked(owner) => return Ok(Phase1::Check(owner)),
            }
        }
    }
    let view = checked.map(|(body, done)| (body.view(program.body_types(i)), done));
    use blaze_core::wait::Clock as _;
    let built_from = work.clock.map(|c| c.now());
    let own = if from_coroutine.is_some() {
        None
    } else {
        let mut subst = Subst { args: &args, ground: &mut work.ground, scratch: work.alloc };
        let g = Global;
        let made = if let Some(s) = shim {
            match s.item {
                Some(_) => blaze_build::build_fn_shim(s.callee, s.tuple, s.which, Some(&program.sig(i)), &mut subst, &adts, g),
                None => blaze_build::build_fn_shim(s.callee, s.tuple, s.which, None, &mut subst, &adts, g),
            }
        } else if vtable_shim.is_some() || reify.is_some() || test_report.is_some() || builtin_clone.is_some() || builtin_from.is_some() {
            if let Some(plan) = vtable_shim {
                let sig = plan.needs_sig.then(|| program.sig(i));
                blaze_build::build_vtable_shim(plan.target, sig.as_ref(), &mut subst, &adts, g)
            } else if let Some(target) = reify {
                blaze_build::build_reify_shim(target.item, &program.sig(i), &mut subst, &adts, g)
            } else if let Some(test_item) = test_report {
                let report_fn = program.termination_report().ok_or(BuildError::NoLangFn { expr: None, name: "termination::report" });
                report_fn.and_then(|report_fn| {
                    let report_row = program.fn_index(report_fn).ok_or(BuildError::NoAbi { item: report_fn })?;
                    blaze_build::build_test_report_shim(test_item, &program.sig(i), report_fn, &program.sig(report_row), &mut subst, &adts, g)
                })
            } else if let Some(made) = builtin_clone {
                blaze_build::build_builtin_clone(made.clone, made.bytes, &mut subst, &adts, g)
            } else if let Some(made) = builtin_from {
                blaze_build::build_builtin_from(made.from, &mut subst, &adts, g)
            } else {
                Err(BuildError::Malformed { what: "unreachable builtin row" })
            }
        } else if let Some(variant) = ctor_row {
            blaze_build::build_ctor_fn(function.key, variant, &program.sig(i), &mut subst, &adts, g)
        } else if let Some((view, done)) = view.as_ref() {
            if let Some((of, which)) = closure {
                blaze_build::build_closure(view, done, of.ordinal, which, &program.sig(i), &mut subst, &adts, g)
            } else if Some(function.key) == program.fn_lang(FnLang::DropInPlace) {
                blaze_build::build_drop_glue(view, done, &program.sig(i), &mut subst, &adts, g)
            } else {
                blaze_build::build(view, done, &program.sig(i), &mut subst, &adts, g)
            }
        } else {
            Err(BuildError::Malformed { what: "a row with no body built as other than a ctor or its shim" })
        };
        Some(made.map_err(|error| build_refusal(program, name(), checked.map(|(body, _)| body), error))?)
    };
    if let (Some(c), Some(from)) = (work.clock, built_from) {
        work.times.build += c.now().saturating_sub(from);
    }
    Ok(Phase1::Built(BuiltPhase { own, from_coroutine }))
}

/// Phase 2 of making one instance: resolves all call references and vtable datums over the
/// already-built LIR, batch-collecting all missing facts in one sweep, then seals the read log
/// and emits the entry (or hands back `BuiltLir`). Never re-runs `blaze_build::build`.
#[allow(clippy::too_many_arguments)]
fn resolve_and_emit_phase<'a, P: FrontEnd>(
    program: &P,
    bodies: &Bodies<'_>,
    work: &mut Work,
    i: usize,
    instance: Instance,
    built_slot: &mut Option<BuiltPhase<Global>>,
    ref_reads: &ReadLog,
    sink: &mut BuildSink<'_, 'a>,
) -> Result<Phase2, RunError> {
    let function = program.function(i);
    let name = || function.named();
    let twice = || RunError::Build { function: name(), error: BuildError::Malformed { what: "a coroutine's function its instance's build did not leave (asked for twice)" }, range: None };
    let built_phase = built_slot.as_ref().ok_or_else(twice)?;
    let from_coroutine = built_phase.from_coroutine;
    let built = match (&built_phase.own, from_coroutine) {
        (Some(b), _) => b,
        (None, Some((at, part))) => work.stash[at].parts[part].as_ref().ok_or_else(twice)?,
        (None, None) => return Err(twice()),
    };
    let asking = Asking::new(program, i, &work.langs, bodies, &work.query_cache, ref_reads);
    work.thread_locals |= !built.thread_locals.is_empty();
    let mut reach = Reach::new_in(work.alloc);
    let mut funcs: AVec<'_, Callee<'_>> = blaze_own::Vec::with_capacity_in(built.refs.len(), work.alloc);
    let mut gaps = RefGaps::new_in(work.alloc);
    for &called in built.refs.iter() {
        match name_ref(program, &asking, &mut work.ground, work.alloc, &mut reach, i, instance.args, called) {
            Ok(Named1::Instance(symbol)) => funcs.push(Callee::Symbol(symbol)),
            Ok(Named1::C(c_name)) => funcs.push(Callee::C(c_name)),
            Ok(Named1::Untabled(key, refusal)) => {
                if !gaps.untabled.iter().any(|(k, _)| *k == key) {
                    gaps.untabled.push((key, refusal));
                }
            }
            Err(e) if asks_again(&e) => {
                gaps.first_ask.get_or_insert(e);
            }
            Err(e) if let Some(t) = object_missing(&e) => {
                if !gaps.missing_objects.iter().any(|(k, _)| *k == t) {
                    gaps.missing_objects.push((t, e));
                }
            }
            Err(e) => return Err(e),
        }
    }
    let mut vtable_data: AVec<'_, (blaze_code::record::Symbol, AVec<'static, u8>)> = blaze_own::Vec::new_in(work.alloc);
    let mut layouts: Layouts<Alloc<'static>> = Layouts::new_in(work.alloc);
    for &want in built.vtables.iter() {
        match vtable_rows(
            program,
            &asking,
            &mut work.ground,
            &mut layouts,
            work.alloc,
            &mut reach,
            i,
            instance.args,
            want,
            &mut |k, e| {
                if !gaps.untabled.iter().any(|(x, _)| *x == k) {
                    gaps.untabled.push((k, e));
                }
            },
            &mut vtable_data,
        ) {
            Ok(()) => {}
            Err(e) if asks_again(&e) => {
                gaps.first_ask.get_or_insert(e);
            }
            Err(e) if let Some(t) = object_missing(&e) => {
                if !gaps.missing_objects.iter().any(|(k, _)| *k == t) {
                    gaps.missing_objects.push((t, e));
                }
            }
            Err(e) => return Err(e),
        }
    }
    if !gaps.is_empty() {
        return Ok(Phase2::Needs(gaps));
    }
    let sym = |e| RunError::CodeRecord { function: name(), error: CodeRecordError::Symbol(e) };
    let mut vtables: AVec<'_, blaze_code::record::Symbol> = blaze_own::Vec::with_capacity_in(built.vtables.len(), work.alloc);
    for &table in built.vtables.iter() {
        vtables.push(vtable_symbol(table, work.ground.view(), work.alloc).map_err(sym)?);
    }
    let mut c_statics: AVec<'_, (ItemKey, &str)> = blaze_own::Vec::new_in(work.alloc);
    for &s in built.statics.iter() {
        if let Some(c) = asking.foreign_static(s) {
            c_statics.push((s, c));
        }
    }
    // Absorb Phase 2's clean read log into `work.reads` (which applies `work.reads.plant`).
    work.reads.absorb(ref_reads);
    match sink {
        BuildSink::Lir { alloc, out } => {
            let out_alloc = *alloc;
            let mut c_text: AVec<'a, u8> = blaze_own::Vec::new_in(out_alloc);
            let mut push_c = |s: &str| -> Result<(u32, u32), RunError> {
                let start = u32::try_from(c_text.len()).map_err(|_| RunError::CodeRecord {
                    function: name(),
                    error: CodeRecordError::Seal(blaze_code::record::RecordError::TooLarge { what: "LTO C symbol table" }),
                })?;
                let len = u32::try_from(s.len()).map_err(|_| RunError::CodeRecord {
                    function: name(),
                    error: CodeRecordError::Seal(blaze_code::record::RecordError::TooLarge { what: "LTO C symbol name" }),
                })?;
                c_text.extend_from_slice(s.as_bytes());
                Ok((start, len))
            };
            let mut lir_funcs: AVec<'a, BuiltCallee> = blaze_own::Vec::with_capacity_in(funcs.len(), out_alloc);
            for &callee in funcs.iter() {
                match callee {
                    Callee::Symbol(s) => lir_funcs.push(BuiltCallee::Symbol(s)),
                    Callee::C(cname) => {
                        let (start, len) = push_c(cname)?;
                        lir_funcs.push(BuiltCallee::C { start, len });
                    }
                }
            }
            let mut lir_c_statics: AVec<'a, (ItemKey, u32, u32)> = blaze_own::Vec::with_capacity_in(c_statics.len(), out_alloc);
            for &(item, cname) in c_statics.iter() {
                let (start, len) = push_c(cname)?;
                lir_c_statics.push((item, start, len));
            }
            let mut lir_statics: AVec<'a, ItemKey> = blaze_own::Vec::with_capacity_in(built.statics.len(), out_alloc);
            lir_statics.extend_from_slice(&built.statics);
            let mut lir_vtables: AVec<'a, blaze_code::record::Symbol> = blaze_own::Vec::with_capacity_in(vtables.len(), out_alloc);
            lir_vtables.extend_from_slice(vtables.as_slice());
            let mut lir_vtable_data: AVec<'a, (blaze_code::record::Symbol, AVec<'a, u8>)> = blaze_own::Vec::with_capacity_in(vtable_data.len(), out_alloc);
            for (vsym, bytes) in vtable_data.iter() {
                let mut copied: AVec<'a, u8> = blaze_own::Vec::with_capacity_in(bytes.len(), out_alloc);
                copied.extend_from_slice(bytes.as_slice());
                lir_vtable_data.push((*vsym, copied));
            }
            let mut lir_tls: AVec<'a, ItemKey> = blaze_own::Vec::with_capacity_in(built.thread_locals.len(), out_alloc);
            lir_tls.extend_from_slice(&built.thread_locals);
            let mut lir_traps: AVec<'a, blaze_code::record::TrapSource> = blaze_own::Vec::with_capacity_in(built.traps.len(), out_alloc);
            lir_traps.extend_from_slice(&built.traps);
            **out = Some(BuiltLir {
                func: built.func.view().to_func_in(out_alloc),
                funcs: lir_funcs,
                statics: lir_statics,
                c_statics: lir_c_statics,
                vtables: lir_vtables,
                vtable_data: lir_vtable_data,
                tls: lir_tls,
                traps: lir_traps,
                c_text,
            });
            let _ = built_slot.take();
            return Ok(Phase2::Done);
        }
        BuildSink::Entry(entry) => {
            use blaze_core::wait::Clock as _;
            let routines = work.routines.bind(&mut funcs).map_err(|error| RunError::CodeRecord { function: name(), error })?;
            let names = InstanceNames { funcs: &funcs, statics: &built.statics, c_statics: &c_statics, vtables: &vtables, tls: &built.thread_locals };
            let mut declared: AVec<'_, Declared<'_>> = blaze_own::Vec::with_capacity_in(reach.rows.len(), work.alloc);
            for n in 0..reach.rows.len() {
                let (tabled, name_bytes) = reach.row(n);
                declared.push(Declared { tabled, name: name_bytes });
            }
            let mut carried: AVec<'_, VtableDatum<'_>> = blaze_own::Vec::with_capacity_in(vtable_data.len(), work.alloc);
            for (symbol, datum) in vtable_data.iter() {
                carried.push(VtableDatum { symbol: *symbol, datum: datum.as_slice() });
            }
            let read_list = work.reads.seal(&mut work.ground, work.alloc).map_err(|error| RunError::CodeRecord { function: name(), error: CodeRecordError::Reads(error) })?;
            let emitted_from = work.clock.map(|c| c.now());
            let made = make_entry_with_vtables(built.func.verified(), &names, &mut carried, routines, &mut declared, &read_list, &built.traps, work.isa, work.alloc)
                .map_err(|error| RunError::CodeRecord { function: name(), error })?;
            if let (Some(c), Some(from)) = (work.clock, emitted_from) {
                work.times.emit += c.now().saturating_sub(from);
                work.times.last = Some(MadeSizes::of(name(), built, &made));
            }
            entry.extend_from_slice(&made);
            let _ = built_slot.take();
            if let Some((at, part)) = from_coroutine {
                work.stash[at].parts[part] = None;
            }
        }
    }
    Ok(Phase2::Done)
}

/// build_from's mapping of a build error to the run's refusal, by the body's ranges (a ctor's
/// fn has no body, so no range).
fn build_refusal<P: FrontEnd>(program: &P, function: Named, body: Option<&blaze_lower::typeck::TypeckBody<'static>>, error: BuildError) -> RunError {
    let ranges = |all: &[blaze_typeck::Range], i: u32| all.get(i as usize).copied();
    let range = match error {
        BuildError::Unsupported { expr, .. }
        | BuildError::UnsupportedType { expr, .. }
        | BuildError::NoConstBlockValue { expr, .. }
        | BuildError::NoConstFnInstance { expr, .. }
        | BuildError::NoConstVtable { expr, .. } => {
            body.and_then(|body| ranges(&body.expr_ranges, expr.0))
        }
        BuildError::UnsupportedPat { pat, .. } => body.and_then(|body| ranges(&body.pat_ranges, pat.0)),
        BuildError::NoAdt { adt }
        | BuildError::NoAbi { item: adt }
        | BuildError::NoStatic { item: adt, .. }
        | BuildError::NoConstValue { item: adt, .. }
        | BuildError::NoPatConstValue { item: adt, .. }
        | BuildError::MissingRecord { item: adt }
            if let Some(why) = program.left_out(adt) =>
        {
            return RunError::LeftOut { function, item: adt, path: program.item_named(adt), why: Box::from(why) };
        }
        BuildError::NoAdt { adt } => return RunError::NoAdt { function, adt, path: program.item_named(adt) },
        BuildError::NoStatic { item, .. } | BuildError::NoConstValue { item, .. } | BuildError::NoPatConstValue { item, .. } | BuildError::MissingRecord { item } => {
            return RunError::ItemRefused { function, error, item, path: program.item_named(item) };
        }
        BuildError::Projection { .. } if let Some(why) = program.last_projection() => {
            return RunError::Projection { function, why };
        }
        _ => None,
    };
    RunError::Build { function, error, range }
}

/// The instances one made entry declares: (tabled fn, name) rows over one arena, each once.
struct Reach<'a> {
    rows: AVec<'a, (ItemKey, u32, u32)>,
    seen: blaze_hash::HashSet<(ItemKey, [u8; 32]), Alloc<'a>>,
    text: AVec<'a, u8>,
    scratch: AVec<'a, u8>,
}

impl<'a> Reach<'a> {
    fn new_in(alloc: Alloc<'a>) -> Reach<'a> {
        Reach {
            rows: blaze_own::Vec::new_in(alloc),
            seen: blaze_hash::HashSet::new_in(alloc),
            text: blaze_own::Vec::new_in(alloc),
            scratch: blaze_own::Vec::new_in(alloc),
        }
    }

    fn row(&self, n: usize) -> (ItemKey, &[u8]) {
        let (tabled, at, len) = self.rows[n];
        (tabled, &self.text[at as usize..(at + len) as usize])
    }

    /// Declares `callee` (tabled as `tabled`) and returns its symbol.
    fn declare(&mut self, tabled: ItemKey, callee: Instance, ground: &PortableTys<'_>, alloc: Alloc<'_>) -> Result<blaze_code::record::Symbol, crate::symbols::SymbolError> {
        self.scratch.clear();
        encode_instance(callee.item, callee.args, ground.view(), alloc, &mut self.scratch)?;
        let symbol = fn_symbol_of_name(&self.scratch);
        if self.seen.insert((tabled, symbol.0)) {
            let at = self.text.len() as u32;
            self.text.extend_from_slice(&self.scratch);
            self.rows.push((tabled, at, self.scratch.len() as u32));
        }
        Ok(symbol)
    }
}

/// How one reference of a built body is named.
enum Named1<'p> {
    Instance(blaze_code::record::Symbol),
    C(&'p str),
    /// The fn a trait's fn resolved to (the selected impl's) is tabled by no row yet: a lazy
    /// front end tables a body's callees as the body names them, and an impl's fn a call
    /// reaches only at ground arguments (`Future::poll` of an `.await`ed future) is named by
    /// no body. [`Step::Table`] has it checked; `.1` is the refusal if that tables no row.
    Untabled(ItemKey, RunError),
}

/// build_from's reference loop for one reference, by name: a closure's, a coroutine's or a
/// shim's instance tabled as its owner; a trait's fn as the selected impl's; an outside fn by
/// its C name. A reference with no instance is this instance's refusal (no stub on this path).
#[allow(clippy::too_many_arguments)]
fn name_ref<'p, P: FrontEnd>(
    program: &'p P,
    asking: &Asking<'_, P>,
    ground: &mut PortableTys<'static>,
    alloc: Alloc<'static>,
    reach: &mut Reach<'_>,
    i: usize,
    at: List,
    called: Instance,
) -> Result<Named1<'p>, RunError> {
    let world = asking.world();
    let adts = &world;
    let function = program.function(i);
    let name = || function.named();
    let sym = |e| RunError::CodeRecord { function: name(), error: CodeRecordError::Symbol(e) };
    // A fn item of a trait's method as an `Fn*` value: resolved first (fn_item_resolve.rs).
    let called = match asking.shim_self(called, ground, alloc) {
        Ok(c) => c,
        Err(error) => {
            let at = crate::callee_shown::self_shown(program, ground.view(), called.args);
            return Err(RunError::Resolve { function: name(), callee: called.item, path: program.item_named(called.item), at, error });
        }
    };
    // A callee whose name passes its cap (rustc's type-length limit) is this instance's
    // refusal, the callee shown by its head path and arguments (rule 19); the test's NOT RUN.
    let named = |e: crate::symbols::SymbolError, callee: Instance, ground: &PortableTys<'static>| match e {
        crate::symbols::SymbolError::TooLarge { nodes, bytes } => {
            callee_refusal(program, name(), at, callee, crate::callee_shown::CalleeGap::NameTooLarge { nodes, bytes }, ground.view())
        }
        e => sym(e),
    };
    if let Some(c) = language_owner(program, adts, called, ground, i) {
        return reach.declare(program.function(c).key, called, ground, alloc).map(Named1::Instance).map_err(|e| named(e, called, &*ground));
    }
    // A fn item's (or a closure's) `Fn*` method whose item no row tables yet: the fn item was
    // passed as a zero-sized value, named by no body's callees (fn_shim.rs `untabled_owner`).
    // Checked, which tables its row, then this instance made again: the shim is declared then.
    if let Some(owner) = crate::fn_shim::untabled_owner(program, adts, called, ground) {
        let refusal = callee_refusal(program, name(), at, owner, crate::callee_shown::CalleeGap::NotTabled, ground.view());
        return Ok(Named1::Untabled(owner.item, refusal));
    }
    // `Clone::clone` at a fn item, fn pointer, tuple or closure: the language's clone, no impl
    // item to select (clone_shim.rs `language_clone`; t1800: proptest's derived `Clone for
    // Config` cloning its `fn(..)` field, 30 tests NOT RUN; t1855d: the same `Config` cloning a
    // tuple field, 30 NOT RUN). Tabled at the trait's `clone` row; a tuple's field clones are
    // resolved in that row's world (world_items.rs `ctor_world`: its trait's).
    if let Some(crate::clone_shim::LanguageClone { clone, .. }) = crate::clone_shim::language_clone_unconditional(program, called, ground) {
        let Some(c) = program.fn_index(clone) else {
            let refusal = callee_refusal(program, name(), at, called, crate::callee_shown::CalleeGap::NotTabled, ground.view());
            return Ok(Named1::Untabled(clone, refusal));
        };
        return reach.declare(program.function(c).key, called, ground, alloc).map(Named1::Instance).map_err(|e| named(e, called, &*ground));
    }
    if let Some(crate::clone_shim::LanguageFrom { from }) = crate::clone_shim::language_from(program, called, ground) {
        let Some(c) = program.fn_index(from) else {
            let refusal = callee_refusal(program, name(), at, called, crate::callee_shown::CalleeGap::NotTabled, ground.view());
            return Ok(Named1::Untabled(from, refusal));
        };
        return reach.declare(program.function(c).key, called, ground, alloc).map(Named1::Instance).map_err(|e| named(e, called, &*ground));
    }
    // A ctor is no trait's fn: it runs as itself, with no world asked (build_from's refs loop).
    // Known by its ADT's head (`ctor_of`), whether or not a row tables it yet: a lazy front end
    // tables a body's CALLEES, and a ctor used as a value (`let p: fn(..) -> S = S`) is none.
    let is_ctor = program.ctor_of(called.item).is_some();
    let selected = match is_ctor {
        true => Ok((called.item, called.args)),
        false => asking.resolve(called, ground, alloc),
    };
    let (item, resolved) = match selected {
        Ok(r) => r,
        Err(blaze_typeck::ResolveError::NoImpl { trait_, .. }) => match crate::clone_shim::language_clone_adt(program, i, called, trait_, ground) {
            Ok(Some(crate::clone_shim::LanguageClone { clone, .. })) => {
                let Some(c) = program.fn_index(clone) else {
                    let refusal = callee_refusal(program, name(), at, called, crate::callee_shown::CalleeGap::NotTabled, ground.view());
                    return Ok(Named1::Untabled(clone, refusal));
                };
                return reach.declare(program.function(c).key, called, ground, alloc).map(Named1::Instance).map_err(|e| named(e, called, &*ground));
            }
            Ok(None) => return Err(resolve_refusal(program, adts, ground, name(), called, blaze_typeck::ResolveError::NoImpl { trait_, item: called.item })),
            Err(error) => return Err(resolve_refusal(program, adts, ground, name(), called, error)),
        },
        Err(error) => return Err(resolve_refusal(program, adts, ground, name(), called, error)),
    };
    let callee = Instance { item, args: resolved };
    // `at`: the arguments of the instance whose body makes this call (its `Self` first).
    let Some(c) = program.fn_index(callee.item) else {
        // Owed a row whatever names it: a ctor's ([`Step::Table`] makes it by the seam's
        // `head_row`), a selected impl's fn, and a fn the body names as a VALUE, which its
        // caller's check does not table as a callee (bstd `main!`'s `guest::start(table,
        // hosted)`: the local `hosted` passed as a fn item, hello world's stop of 2026-11-17
        // g080047). A fn a check of it tables no row for is refused when met again.
        let refusal = callee_refusal(program, name(), at, callee, crate::callee_shown::CalleeGap::NotTabled, ground.view());
        return Ok(Named1::Untabled(callee.item, refusal));
    };
    let target = program.function(c);
    if asking.foreign_fn(c).is_some() {
        return Ok(Named1::C(target.symbol));
    }
    if bodied(program, asking.checked(), c) {
        return reach.declare(target.key, callee, ground, alloc).map(Named1::Instance).map_err(|e| named(e, callee, &*ground));
    }
    Err(callee_refusal(program, name(), at, callee, crate::callee_shown::CalleeGap::NoBody, ground.view()))
}

/// Whether fn row `c` is made as an instance of its own: its body is held now, or this run
/// checked it (a made body `done` dropped), or it is owed one: a row no `check_one` of this
/// run was asked of yet, neither an outside fn nor an intrinsic. A front end that tables every
/// body up front holds each now; the lazy program (front2/program/lazy) tables a callee's row
/// from its head and makes the body when the walk reaches the callee's own instance, whose
/// make checks it ([`MakerState::ensure`]) and refuses by name a fn with no body to check.
fn bodied<P: FrontEnd>(program: &P, checked: &[Option<Checked<'static>>], c: usize) -> bool {
    let f = program.function(c);
    if f.body.is_some() || checked.get(c).is_some_and(Option::is_some) {
        return true;
    }
    f.kind != FnKind::Foreign && program.intrinsic(f.key).is_none()
}

/// build_from's vtable loop for one vtable the body names, by name: the method words and the
/// drop glue of it and of each table above it (by a work list, no recursion) declared as the
/// instances they reach. The tables' bytes are data, made by their own record (D106 item 6).
/// A word whose fn no row tables yet (the drop glue `drop_in_place`, an impl's method no body
/// called: a lazy front end tables a body's CALLEES, and a vtable's words are no body's) goes
/// to `untabled`, as an untabled callee of a call does ([`Named1::Untabled`]): checked, then
/// the instance made again. Each table's DATUM goes onto `data` by its symbol, each once (the
/// entry carries it: code_records.rs `VtableDatum`), when every word of it is named.
#[allow(clippy::too_many_arguments)]
fn vtable_rows<P: FrontEnd>(
    program: &P,
    asking: &Asking<'_, P>,
    ground: &mut PortableTys<'static>,
    layouts: &mut Layouts<Alloc<'static>>,
    alloc: Alloc<'static>,
    reach: &mut Reach<'_>,
    i: usize,
    at: List,
    want: VtableRef,
    on_untabled: &mut impl FnMut(ItemKey, RunError),
    data: &mut AVec<'_, (blaze_code::record::Symbol, AVec<'static, u8>)>,
) -> Result<(), RunError> {
    let world = asking.world();
    let adts = &world;
    let function = program.function(i);
    let name = || function.named();
    let fail = |why| RunError::Vtable { function: name(), principal: want.principal, why };
    let full = || fail(VtableFail::Build(BuildError::Malformed { what: "the ground table is full" }));
    let sym = |e| RunError::CodeRecord { function: name(), error: CodeRecordError::Symbol(e) };
    // A method word's instance whose name passes its cap: this instance's refusal, as a
    // call's is (`name_ref`).
    let named = |e: crate::symbols::SymbolError, callee: Instance, ground: &PortableTys<'static>| match e {
        crate::symbols::SymbolError::TooLarge { nodes, bytes } => {
            callee_refusal(program, name(), at, callee, crate::callee_shown::CalleeGap::NameTooLarge { nodes, bytes }, ground.view())
        }
        e => sym(e),
    };
    let mut todo: AVec<'_, VtableRef> = blaze_own::Vec::new_in(alloc);
    let mut seen: AVec<'_, VtableRef> = blaze_own::Vec::new_in(alloc);
    todo.push(want);
    while let Some(t) = todo.pop() {
        if seen.contains(&t) {
            continue;
        }
        seen.push(t);
        let shape = Shape::of(t.principal, |k| adts.object_trait(k), alloc).map_err(|e| fail(VtableFail::Shape(e)))?;
        let id = {
            let mut cx = LayoutCx { ground: &mut *ground, facts: adts, scratch: alloc };
            layouts.of(t.concrete, &mut cx).map_err(|e| fail(VtableFail::Layout(e)))?
        };
        let (size, align) = (layouts.get(id).size, layouts.get(id).align_bytes());
        let glue = match program.fn_lang(FnLang::DropInPlace) {
            Some(item) => Some(Instance { item, args: ground.intern_list(&[t.concrete]).map_err(|_| full())? }),
            None => match program.lang_item(LangItem::Drop).map_err(RunError::Lang)? {
                None => None,
                Some(_) => return Err(fail(VtableFail::NoDropGlue)),
            },
        };
        let glue_item = glue.map(|g| g.item);
        let mut targs: AVec<'_, PTy> = blaze_own::Vec::new_in(alloc);
        targs.extend_from_slice(ground.view().list(t.args).map_err(|_| full())?);
        let table = Vtable::build(&shape, t.concrete, &targs, size, align, glue, &mut *ground, adts, alloc, Global).map_err(|e| fail(VtableFail::Build(e)))?;
        // The table's datum (code_records.rs `VtableDatum`): its bytes as built (the header's
        // size and alignment, every address word 0), each address word a target by symbol.
        let mut datum = blaze_code::datum::Datum::new_in(blaze_code::datum::Place::ReadOnly, 3, alloc);
        datum.bytes.extend_from_slice(&table.bytes);
        // False once a word's fn is untabled: the instance is made again ([`Step::Table`]), and
        // this table's datum with it.
        let mut whole = true;
        for &(off, to) in table.relocs.iter() {
            let (f, by_value_shim) = match to {
                VtableTarget::Fn(f) => (f, false),
                VtableTarget::ByValueShim(f) => (f, true),
                VtableTarget::Super(sup) => {
                    todo.push(sup);
                    let symbol = vtable_symbol(sup, ground.view(), alloc).map_err(sym)?;
                    let target = datum.target(blaze_code::record::Target { space: blaze_code::record::Space::Vtable, symbol });
                    datum.words.push(blaze_code::record::Word { at: off, target, addend: 0 });
                    continue;
                }
            };
            let mut word = |symbol: blaze_code::record::Symbol| {
                let target = datum.target(blaze_code::record::Target { space: blaze_code::record::Space::Func, symbol });
                datum.words.push(blaze_code::record::Word { at: off, target, addend: 0 });
            };
            // As a call's callee (`name_ref` above): a fn item of a trait's method resolved
            // to the body that runs, then what the language gives (`language_owner`).
            let f = match asking.shim_self(f, ground, alloc) {
                Ok(f) => f,
                Err(error) => {
                    let at = crate::callee_shown::self_shown(program, ground.view(), f.args);
                    return Err(RunError::Resolve { function: name(), callee: f.item, path: program.item_named(f.item), at, error });
                }
            };
            if let Some(c) = language_owner(program, adts, f, ground, i) {
                let inst = match by_value_shim {
                    true => blaze_build::vtable_shim_instance(f, ground).map_err(|e| fail(VtableFail::Build(e)))?,
                    false => f,
                };
                word(reach.declare(program.function(c).key, inst, ground, alloc).map_err(|e| named(e, inst, &*ground))?);
                continue;
            }
            // A `dyn Fn*` made from a fn item (or a closure) whose item no row tables yet: as
            // `name_ref`'s, checked and the instance made again (fn_shim.rs `untabled_owner`).
            if let Some(owner) = crate::fn_shim::untabled_owner(program, adts, f, ground) {
                let refusal = callee_refusal(program, name(), at, owner, crate::callee_shown::CalleeGap::VtableWord, ground.view());
                on_untabled(owner.item, refusal);
                whole = false;
                continue;
            }
            let (item, args) = match Some(f.item) == glue_item {
                true => (f.item, f.args),
                false => match asking.resolve(f, &mut *ground, alloc) {
                    Ok(r) => r,
                    Err(error) => return Err(resolve_refusal(program, adts, ground, name(), f, error)),
                },
            };
            let refusal = || callee_refusal(program, name(), at, Instance { item, args }, crate::callee_shown::CalleeGap::VtableWord, ground.view());
            let Some(c) = program.fn_index(item) else {
                on_untabled(item, refusal());
                whole = false;
                continue;
            };
            // A vtable's word is never an `extern` fn's C symbol: a foreign fn has no `Self` to
            // be a method of. Logged with the calls' (`ForeignFn`) all the same.
            asking.foreign_fn(c);
            if !bodied(program, asking.checked(), c) {
                return Err(refusal());
            }
            let target = Instance { item, args };
            let inst = match by_value_shim {
                true => blaze_build::vtable_shim_instance(target, ground).map_err(|e| fail(VtableFail::Build(e)))?,
                false => target,
            };
            word(reach.declare(program.function(c).key, inst, ground, alloc).map_err(|e| named(e, inst, &*ground))?);
        }
        if whole {
            let symbol = vtable_symbol(t, ground.view(), alloc).map_err(sym)?;
            let sealed = datum.seal().map_err(|why| RunError::CodeRecord { function: name(), error: CodeRecordError::VtableDatum(why) })?;
            let mut bytes: AVec<'static, u8> = blaze_own::Vec::new_in(alloc);
            sealed.encode(&mut bytes);
            if !data.iter().any(|(s, _)| *s == symbol) {
                data.push((symbol, bytes));
            }
        }
    }
    Ok(())
}

/// What the command hands the [`Maker`]: every crate's names READ FROM RECORDS, nothing solved
/// in this command. The same four things `blaze check` hands its world (blaze-cli
/// body_types.rs `checker`: a `StoreLoad` over each crate's `CrateScope` record, a `Log` over
/// the crates' names, `Records::new(load.keys(g), load, log, frame, g)`, and
/// `ProjectWorld::from_records(records, frame, owner.lang, g)` with `set_body_sources`), so the
/// command makes them ONE way for both. All borrowed for `'n`, the command's life: the maker
/// holds no input of its own, so nothing in it borrows itself.
#[derive(Clone, Copy)]
pub struct NamesRecords<'n> {
    /// Each crate's names and items records, the program's own at index 0, opened on the first
    /// question about each (`Records::new`: nothing is read when it is made).
    pub records: &'n crate::front2::source::Records<'n>,
    /// The command's frame: what a body's lowering keeps for the command (`Front2::frame`).
    pub frame: &'n blaze_mem::Frame,
    /// The crates that can declare a lang item, by index among `records`' crates: the
    /// program's own (0), core, alloc, std, and bstd under a blaze edition
    /// (`Front2::from_records` reads only their lang rows).
    pub lang_crates: &'n [usize],
    /// The sources a body's macro calls expand with (environment, texts, proc-macro runner:
    /// front2 body_host.rs); `None`: every body `env!`, include and proc macro is refused by
    /// name.
    pub body_sources: Option<&'n dyn crate::front2::body_host::BodySources>,
}

/// The one lazy program the maker makes: over the names records.
pub type RecordsProgram<'n> = crate::front2::program::lazy::LazyProgram<'n, &'n crate::front2::source::Records<'n>>;

/// An instance for a person ([`Maker::instance_shown`]): its item by the one printer, and its
/// ground arguments as Rust spells them (`None`: it has none).
pub struct InstanceShown {
    pub item: crate::front2::item_shown::ItemShown,
    pub args: Option<Box<str>>,
}

/// THE RECORD WALK'S MAKER, held by the parent for the whole program (D124): one owned session
/// over the names READ FROM RECORDS. [`Maker::new`] resolves, reads and checks nothing; the
/// first [`Make::make`] constructs the lazy program over the records (no `resolve_crates`
/// anywhere under it), and every make after it types only the bodies its instance needs
/// (`check_one`). A run that finds every record constructs no program and reads no record.
///
/// What the records could not answer is in their log (front2 source.rs): after each make the
/// log is read, and a make over a log that is not empty is refused
/// ([`LazyRefusal::RecordsVoid`]), as is every make after it (the session is void).
///
/// Two type parameters, each a seam chosen by type (rule 78): `G`, the proof a make is handed
/// (body_ids.rs `Grant`: blaze-cli's run gate's `Runnable` in production, `Fixture` in this
/// crate's walks), and `T`, where a body's checked columns come from ([`Typing`]).
pub struct Maker<'n, G: Grant = Fixture, T: Typing<RecordsProgram<'n>> = SecondCheck> {
    /// Where each body's checked columns come from.
    typing: T,
    /// The proof the maker's makes are handed: a type only.
    grant: core::marker::PhantomData<fn(&G)>,
    names: NamesRecords<'n>,
    entry: Option<ItemKey>,
    /// Where its program starts: the front's roots' say (run/special.rs `MakerStart`).
    start: crate::run::special::MakerStart,
    made: Option<(RecordsProgram<'n>, MakerState)>,
    isa: Isa,
    overflow: OverflowChecks,
    ub: blaze_build::UbChecks,
    panic: blaze_build::Panic,
    alloc: Alloc<'static>,
    checks: CheckCount,
    /// Where a large or slow make is reported ([`Maker::watch`]); `None`: nowhere, untimed.
    watch: Option<MakeWatch<'n>>,
    /// How its builds record their read lists ([`Maker::plant_reads`]): `ReadPlant::None` but
    /// in the planted control.
    plant: ReadPlant,
}

impl<'n> Maker<'n, Fixture, SecondCheck> {
    /// The maker over `names`. `entry`: the checker key of the fn the process starts at, as
    /// `LazyProgram::from_records` takes it (nothing lowers it until a make asks). `builder`:
    /// the records' builder; its ISA, overflow setting and panic strategy are the makes' (rule
    /// 21: one source, nothing to compare). `alloc`: the `'static` heap the lazy program's
    /// tables live in (`Store::new`, `MakerState`). Cheap: reads, resolves and checks nothing.
    ///
    /// Its program starts by THE start rule (`MakerStart::ByRule`): a program's run, for this
    /// crate's walks (`Fixture`), each body checked in the make. A parent whose roots name
    /// another start takes [`Maker::of_roots`]; a walk reading another typing,
    /// [`Maker::typed`].
    pub fn new(names: NamesRecords<'n>, entry: Option<ItemKey>, builder: &Builder, alloc: Alloc<'static>) -> Maker<'n> {
        Maker::starting(names, entry, crate::run::special::MakerStart::ByRule, SecondCheck, builder, alloc)
    }
}

impl<'n, T: Typing<RecordsProgram<'n>>> Maker<'n, Fixture, T> {
    /// [`Maker::new`] with the body's checked columns from `typing`: the oracle's two sides.
    pub fn typed(names: NamesRecords<'n>, entry: Option<ItemKey>, typing: T, builder: &Builder, alloc: Alloc<'static>) -> Maker<'n, Fixture, T> {
        Maker::starting(names, entry, crate::run::special::MakerStart::ByRule, typing, builder, alloc)
    }
}

impl<'n, G: Grant, T: Typing<RecordsProgram<'n>>> Maker<'n, G, T> {
    /// The maker for the roots a front handed (run/special.rs `Roots`): `entry` as [`Maker::new`]
    /// takes it, and the program's start the roots' own (`Roots::maker_start`): `blaze run`'s
    /// roots start by the rule, a test's at its fn. The one way the lazy parent makes a maker.
    /// `typing`: where each body's checked columns come from (production: [`SecondCheck`]
    /// until the check keeps its typed record; then [`FromRecord`]).
    pub fn of_roots(names: NamesRecords<'n>, entry: Option<ItemKey>, roots: &crate::run::special::Roots<'_>, typing: T, builder: &Builder, alloc: Alloc<'static>) -> Maker<'n, G, T> {
        Self::starting(names, entry, roots.maker_start, typing, builder, alloc)
    }

    /// [`Maker::of_roots`] for the `entry` and `start` read off those roots beforehand
    /// (`Roots::maker_start`): what a maker made away from the roots needs (blaze-cli
    /// test_drive.rs `CodePool`, D194: a code worker's maker over its own names records, for the
    /// start the asking test's roots said), so its roots are not made again. Equal to `of_roots`
    /// over roots whose `maker_start` is `start`: that one reads nothing else of them.
    pub fn of_start(names: NamesRecords<'n>, entry: Option<ItemKey>, start: crate::run::special::MakerStart, typing: T, builder: &Builder, alloc: Alloc<'static>) -> Maker<'n, G, T> {
        Self::starting(names, entry, start, typing, builder, alloc)
    }

    /// Which start class this maker was made for (`MakerKind::ByRule(entry)` or
    /// `MakerKind::Direct`), so a keeper of makers can verify that a make is handed to a maker
    /// of its own start class without splitting `Direct(_)` tests in one suite into separate
    /// makers (blaze-cli test_drive.rs `SuiteMakers`).
    pub fn made_for(&self) -> crate::run::special::MakerKind {
        crate::run::special::MakerKind::of(self.entry, self.start)
    }

    /// Retargets a `MakerStart::Direct(_)` maker to another direct test entry in the same
    /// suite without reconstructing its `RecordsProgram` or `MakerState`. Returns `false` if
    /// either this maker or `start` is `MakerStart::ByRule`.
    pub fn set_direct_entry(&mut self, entry: Option<ItemKey>, start: crate::run::special::MakerStart) -> bool {
        let (crate::run::special::MakerStart::Direct(_), crate::run::special::MakerStart::Direct(lib)) = (self.start, start) else {
            return false;
        };
        if let Some((program, _)) = self.made.as_mut()
            && !program.set_direct_entry(entry, lib)
        {
            return false;
        }
        self.entry = entry;
        self.start = start;
        true
    }

    /// Hints the cold lookup of body-local items in `locals` (`LibKey::INNER` set — either the
    /// tabled item itself or a local fn/closure owner referenced inside its generic arguments) by
    /// lowering their declarer chain `enclosers` (outermost first) in this maker's lazy program so
    /// macro-generated local fns (e.g. `const_eval_select!` -> `runtime`, or `thread_local!`) are
    /// filed in O(1) on any worker without scanning the crate.
    pub fn prepare_local(&mut self, locals: &[ItemKey], enclosers: &[ItemKey]) {
        if locals.is_empty() || enclosers.is_empty() {
            return;
        }
        let Some(&first) = locals.first() else { return };
        if self.construct(first).is_err() {
            return;
        }
        if let Some((program, _)) = self.made.as_mut() {
            for &key in locals {
                program.prepare_local(key, enclosers);
            }
        }
    }

    fn starting(names: NamesRecords<'n>, entry: Option<ItemKey>, start: crate::run::special::MakerStart, typing: T, builder: &Builder, alloc: Alloc<'static>) -> Maker<'n, G, T> {
        Maker {
            typing,
            grant: core::marker::PhantomData,
            names,
            entry,
            start,
            made: None,
            isa: builder.isa,
            overflow: builder.settings.overflow_checks,
            // `ub_checks()` follows the key's debug assertions, as rustc's `ub_checks` cfg
            // follows `-C debug-assertions`: in the key, so never stale (rule 21).
            ub: blaze_build::UbChecks::of_debug_assertions(builder.settings.debug_assertions),
            panic: crate::panic_strategy::of_strategy(builder.settings.panic),
            alloc,
            checks: CheckCount::default(),
            watch: None,
            plant: ReadPlant::None,
        }
    }

    /// The read-list tests' planted bug (code_reads.rs `ReadPlant`, rule 23): every later build
    /// records with `plant`.
    pub fn plant_reads(&mut self, plant: ReadPlant) {
        if let Some((_, state)) = self.made.as_mut() {
            state.plant_reads(plant);
        }
        self.plant = plant;
    }

    /// Every later make timed on `watch.clock`, and each that is large or slow (its code or
    /// estimated frame at least [`MAKE_REPORT_BYTES`], or its make at least
    /// [`MAKE_REPORT_TIME`]) handed to `watch.report` as its [`MakeSizes`] (rule 16).
    pub fn watch(&mut self, watch: MakeWatch<'n>) {
        if let Some((_, state)) = self.made.as_mut() {
            state.time_on(watch.clock);
        }
        self.watch = Some(watch);
    }

    /// Whether an instance made in this session touched a thread-local. A FOUND record's use of
    /// one is said by bzcode03's declared rows (kinds 4 and 5); until that form lands the
    /// caller makes the getter's and ender's records when this is true or its own records say so.
    pub fn made_thread_locals(&self) -> bool {
        self.made.as_ref().is_some_and(|(_, s)| s.work.thread_locals)
    }

    /// The checks the makes cost so far.
    pub fn checks(&self) -> CheckCount {
        self.checks
    }

    /// The lazy program's lowering counts, with the typing's counts of where each body's
    /// columns came from (the run's closing line).
    pub fn lowering_counts(&self) -> LoweringCounts {
        let lowered = self.program().map_or_else(LoweringCounts::default, |program| program.lowering_counts());
        LoweringCounts { typing: self.typing.counts(), ..lowered }
    }

    /// Where this maker's bodies' checked columns come from.
    pub fn typing(&self) -> &T {
        &self.typing
    }

    /// The lazy program, once a miss made it.
    pub fn program(&self) -> Option<&RecordsProgram<'n>> {
        self.made.as_ref().map(|(p, _)| p)
    }

    /// Instance `name` (symbols.rs `encode_instance` bytes) of fn `tabled`, for a person
    /// (rule 19: a crash names the fn and, since generics matter for a miscompile, its ground
    /// arguments): the item its name holds by the one printer (front2 item_shown.rs
    /// `ItemShown`: a path, or where a path-less item is written) and its arguments as Rust
    /// spells them (callee_shown.rs `args_shown`: a closure as `<closure N of owner>`). `None`
    /// before a make built the program (a run whose every record was found): the caller then
    /// names the item from the records alone. Read only on a crash's path.
    pub fn instance_shown(&self, tabled: ItemKey, name: &[u8]) -> Option<InstanceShown> {
        let program = self.program()?;
        let mut ground = PortableTys::new_in(self.alloc);
        let (item, args) = match decode_instance(name, &mut ground, self.alloc) {
            Ok((item, args)) => {
                let view = ground.view();
                if let Some(test) = blaze_build::test_report_shim_of(Instance { item, args }, view) {
                    return Some(InstanceShown { item: program.item_shown(test), args: None });
                }
                let args = match view.list(args) {
                    Ok([]) => None,
                    _ => Some(crate::callee_shown::args_shown(view, args, &|k| program.item_named(k))),
                };
                (item, args)
            }
            Err(_) => (tabled, None),
        };
        Some(InstanceShown { item: program.item_shown(item), args })
    }

    /// Byte `site` of item `item`'s text (a code record's trap row, item-relative) for a
    /// person: its file, line and column today (lazy fill.rs `site_shown`). `None` before a
    /// make built the program, or when the item's text is in no file. Read only on a crash's
    /// path.
    pub fn site_shown(&self, item: ItemKey, site: u32) -> Option<(&'n str, u32, u32)> {
        self.program()?.site_shown(item, site)
    }

    /// The refusal of a make over records whose log is not empty, naming its first error.
    fn void(&self, tabled: ItemKey) -> Option<RunError> {
        let log = self.names.records.log();
        let errors = log.errors();
        let first = errors.first()?;
        let first = std::format!("{first}").into_boxed_str();
        Some(RunError::Front2(Front2Refusal::Lazy(LazyRefusal::RecordsVoid { tabled, first, kept: errors.len(), dropped: log.dropped() })))
    }

    /// The program made on the first call, then the one instance.
    fn make_over(&mut self, tabled: ItemKey, name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        self.construct(tabled)?;
        let Some((program, state)) = self.made.as_mut() else {
            return Err(RunError::OneBody { key: tabled, why: "the maker's program was not made (make_one.rs `Maker::make`)" });
        };
        make_instance(program, &mut self.typing, state, &mut self.checks, tabled, name, entry)?;
        if let (Some(watch), Some(sizes)) = (self.watch, state.take_report()) {
            (watch.report)(&sizes);
        }
        Ok(())
    }

    /// Builds verified LIR and resolved outside references for one instance without emitting
    /// machine code yet (used by [`export_lto`] for whole-program cross-function inlining).
    pub fn build_lir<'a>(&mut self, _grant: &G, tabled: ItemKey, name: &[u8], alloc: Alloc<'a>) -> Result<BuiltLir<'a>, RunError> {
        if let Some(void) = self.void(tabled) {
            return Err(void);
        }
        let made = self.build_lir_over(tabled, name, alloc);
        if let Some(void) = self.void(tabled) {
            return Err(void);
        }
        made
    }

    fn build_lir_over<'a>(&mut self, tabled: ItemKey, name: &[u8], alloc: Alloc<'a>) -> Result<BuiltLir<'a>, RunError> {
        self.construct(tabled)?;
        let Some((program, state)) = self.made.as_mut() else {
            return Err(RunError::OneBody { key: tabled, why: "the maker's program was not made (make_one.rs `Maker::build_lir`)" });
        };
        let mut out: Option<BuiltLir<'a>> = None;
        let mut sink = BuildSink::Lir { alloc, out: &mut out };
        run_instance_loop(program, &mut self.typing, state, &mut self.checks, tabled, name, &mut sink)?;
        out.ok_or(RunError::OneBody { key: tabled, why: "the LIR build completed without producing a body (make_one.rs `Maker::build_lir`)" })
    }

    /// Binds compiler_builtins soft-float routines onto `funcs` (after all original + LTO-imported
    /// function references) so host-machine emission can legalize `f128` operations if needed.
    pub fn bind_routines<'m>(&'m self, funcs: &mut AVec<'_, Callee<'_>>) -> Result<crate::front2::builtins_shim::Routines<'m>, CodeRecordError> {
        match self.made.as_ref() {
            Some((_, state)) => state.work.routines.bind(funcs),
            None => Ok(crate::front2::builtins_shim::Routines::NONE),
        }
    }

    /// The program made on the first call, then static `item`'s value made its entry
    /// (code_records.rs `statics`): a static's datum, or (`template`) a thread-local's template.
    fn static_over(&mut self, item: ItemKey, template: bool, entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        self.construct(item)?;
        let Some((program, state)) = self.made.as_mut() else {
            return Err(RunError::OneBody { key: item, why: "the maker's program was not made (make_one.rs `Maker::make_static`)" });
        };
        let kind = program.make_static_value(item).map_err(|e| RunError::Front2(Front2Refusal::Lazy(e)))?;
        let alloc = state.work.alloc;
        let mut vtable_keys: AVec<'_, blaze_build::vtable::VtableKey> = blaze_own::Vec::new_in(alloc);
        {
            let value = program.static_value(item).map_err(|e| RunError::Front2(Front2Refusal::Lazy(e)))?;
            for a in value.allocs.iter() {
                for r in a.relocs.iter() {
                    if let blaze_build::data::DataTarget::Vtable(key) = r.target
                        && vtable_keys.scan_for(&key).is_none()
                    {
                        vtable_keys.push(key);
                    }
                }
            }
        }
        if vtable_keys.is_empty() {
            let value = program.static_value(item).map_err(|e| RunError::Front2(Front2Refusal::Lazy(e)))?;
            let made = match state.static_entry(program, value, kind, template) {
                Ok(made) => made,
                Err(error) => return Err(RunError::StaticRecord { static_: program.static_shown(item), error }),
            };
            entry.extend_from_slice(&made);
            return Ok(());
        }
        // Resolve each vtable key the static names and build its datum(s) + declared method rows,
        // retrying missing object traits, lazy impl fills, and untabled method bodies as `make_instance` does.
        let mut resolved: AVec<'_, (blaze_build::vtable::VtableKey, VtableRef)> = blaze_own::Vec::with_capacity_in(vtable_keys.len(), alloc);
        for key in vtable_keys {
            if let Some(vtable) = program.const_vtable_ref(key, &mut state.work.ground, alloc) {
                program.object_table(vtable.principal)?;
                resolved.push((key, vtable));
            }
        }
        let mut vtable_syms: AVec<'_, (blaze_build::vtable::VtableKey, blaze_code::record::Symbol)> = blaze_own::Vec::with_capacity_in(resolved.len(), alloc);
        for &(key, vtable) in resolved.iter() {
            let sym = vtable_symbol(vtable, state.work.ground.view(), alloc)
                .map_err(|error| RunError::StaticRecord { static_: program.static_shown(item), error: crate::code_records::statics::StaticRecordError::VtableName { alloc: 0, at: 0, error } })?;
            vtable_syms.push((key, sym));
        }
        let mut reach = Reach::new_in(alloc);
        let mut vtable_data: AVec<'_, (blaze_code::record::Symbol, AVec<'static, u8>)> = blaze_own::Vec::new_in(alloc);
        if !resolved.is_empty() {
            // A fresh pooled worker whose first job is a static with vtables starts with
            // `program.fn_count() == 0` when `self.entry` is `None` (`MakerStart::Direct`).
            // Seed at least one checked fn row so `Asking::new(..., 0, ...)`, `vtable_rows`,
            // and `program.make_asked(0)` have a valid world row — never skip building the
            // carried vtable datums (which emits a static referencing an uncarried vtable,
            // poisoning CAS and failing all later runs with `StaticUnsent`).
            if program.fn_count() == 0 {
                let seed = self
                    .entry
                    .or_else(|| program.fn_lang(FnLang::DropInPlace))
                    .or_else(|| {
                        resolved.iter().find_map(|&(_, v)| {
                            program.object_trait(v.principal).and_then(|o| o.methods.first().map(|m| m.item))
                        })
                    });
                if let Some(seed_fn) = seed {
                    state.ensure(program, &mut self.typing, seed_fn, &mut self.checks)?;
                }
            }
            if program.fn_count() == 0 {
                return Err(RunError::OneBody {
                    key: item,
                    why: "a static references a vtable, and no function row could be seeded to build its vtable datum (make_one.rs `static_over`)",
                });
            }
            let mut asked = AskProgress::new_in(alloc);
            let mut owners = 0usize;
            loop {
                reach = Reach::new_in(alloc);
                vtable_data.clear();
                let mut untabled: AVec<'_, (ItemKey, RunError)> = blaze_own::Vec::new_in(alloc);
                let step = {
                    let MakerState { checked, tables, work, .. } = &mut *state;
                    let t = tables.take().ok_or_else(|| tables_gone(item))?;
                    let bodies = Bodies { checked: &checked[..], closures: t.closures, coroutines: t.coroutines, const_at: t.const_at, overflow: work.overflow, ub: work.ub, panic: work.panic, alloc: work.alloc };
                    work.reads.clear();
                    let asking = Asking::new(&*program, 0, &work.langs, &bodies, &work.query_cache, &work.reads);
                    let mut layouts: Layouts<Alloc<'static>> = Layouts::new_in(alloc);
                    let mut res = Ok(());
                    for &(_, want) in resolved.iter() {
                        if let Err(e) = vtable_rows(
                            &*program,
                            &asking,
                            &mut work.ground,
                            &mut layouts,
                            alloc,
                            &mut reach,
                            0,
                            List::EMPTY,
                            want,
                            &mut |k, e| {
                                if !untabled.iter().any(|(x, _)| *x == k) {
                                    untabled.push((k, e));
                                }
                            },
                            &mut vtable_data,
                        ) {
                            res = Err(e);
                            break;
                        }
                    }
                    *tables = Some(Tables { closures: bodies.closures, coroutines: bodies.coroutines, const_at: bodies.const_at });
                    res
                };
                match step {
                    Err(e) if asks_again(&e) && asked.allow(&e) => {
                        if program.make_asked(0)? {
                            continue;
                        }
                        return Err(e);
                    }
                    Err(e) if let Some(t) = object_missing(&e) && asked.allow(&e) => {
                        if program.object_table(t)? {
                            continue;
                        }
                        return Err(e);
                    }
                    Err(e) => return Err(e),
                    Ok(()) if !untabled.is_empty() => {
                        for (key, refusal) in untabled {
                            owners += 1;
                            if owners > MOST_HELD_COROUTINES || state.asked.binary_search_by_key(&key, |r| r.0).is_ok() {
                                return Err(refusal);
                            }
                            state.ensure(program, &mut self.typing, key, &mut self.checks)?;
                        }
                    }
                    Ok(()) => break,
                }
            }
            for &(_, sym) in vtable_syms.iter() {
                if !vtable_data.iter().any(|(s, _)| *s == sym) {
                    return Err(RunError::OneBody {
                        key: item,
                        why: "a static's referenced vtable datum was not built (make_one.rs `static_over`)",
                    });
                }
            }
        }
        let mut extra_declared: AVec<'_, Declared<'_>> = blaze_own::Vec::with_capacity_in(reach.rows.len(), alloc);
        for n in 0..reach.rows.len() {
            let (tabled, name_bytes) = reach.row(n);
            extra_declared.push(Declared { tabled, name: name_bytes });
        }
        let mut carried: AVec<'_, VtableDatum<'_>> = blaze_own::Vec::with_capacity_in(vtable_data.len(), alloc);
        for (symbol, datum) in vtable_data.iter() {
            carried.push(VtableDatum { symbol: *symbol, datum: datum.as_slice() });
        }
        let value = program.static_value(item).map_err(|e| RunError::Front2(Front2Refusal::Lazy(e)))?;
        let made = match state.static_entry_with_vtables(program, value, kind, template, &vtable_syms, &extra_declared, &carried) {
            Ok(made) => made,
            Err(error) => return Err(RunError::StaticRecord { static_: program.static_shown(item), error }),
        };
        entry.extend_from_slice(&made);
        Ok(())
    }

    /// The lazy program over the records, made on the first make of either kind (`first`: the
    /// item that make is of).
    fn construct(&mut self, tabled: ItemKey) -> Result<(), RunError> {
        if self.made.is_none() {
            let program = program_of(self.names, self.entry, self.start, self.alloc)?;
            self.checks.constructed += 1;
            // A lazy program holds no checked body when made: 0, counted as the claim reads it.
            self.checks.by_construction += (0..program.fn_count()).filter(|&c| program.function(c).body.is_some()).count();
            let mut state = MakerState::new(&program, tabled, self.isa, self.overflow, self.ub, self.panic, self.alloc)?;
            if let Some(watch) = self.watch {
                state.time_on(watch.clock);
            }
            state.plant_reads(self.plant);
            self.made = Some((program, state));
        }
        Ok(())
    }
}

/// The lazy program over `names`, its body sources set and its start decided: the one way a
/// maker's program is made ([`Maker`]'s first make, and the test double [`FromRecord::rechecked`]).
fn program_of<'n>(names: NamesRecords<'n>, entry: Option<ItemKey>, start: crate::run::special::MakerStart, alloc: Alloc<'static>) -> Result<RecordsProgram<'n>, RunError> {
    let NamesRecords { records, frame, lang_crates, body_sources } = names;
    let mut program = RecordsProgram::from_records(records, frame, lang_crates, entry, alloc).map_err(RunError::Front2)?;
    if let Some(sources) = body_sources {
        program.set_body_sources(sources);
    }
    // The start the front's roots named (run/special.rs `MakerStart`): by the one rule
    // run/special.rs `roots` takes (the seam's `rust_start`), a refusal here the parent's own
    // roots refusal met again, never passed over; or a test fn, called directly.
    program.decide_start(start).map_err(RunError::Roots)?;
    Ok(program)
}

/// The typed records' test double over a maker's names (lazy typed.rs `Rechecked`).
pub type RecheckedRecords<'n> = crate::front2::program::lazy::typed::Rechecked<'n, &'n crate::front2::source::Records<'n>>;

impl<'n> FromRecord<RecheckedRecords<'n>> {
    /// THE TEST DOUBLE as a typing: each body's checked columns made by today's second check in
    /// a lazy program of its own over `names` (the same records, start and entry as the maker
    /// it serves; [`Maker::new`]'s start rule), handed over as a typed record would hand them.
    pub fn rechecked(names: NamesRecords<'n>, entry: Option<ItemKey>, alloc: Alloc<'static>) -> Result<FromRecord<RecheckedRecords<'n>>, RunError> {
        let program = program_of(names, entry, crate::run::special::MakerStart::ByRule, alloc)?;
        Ok(FromRecord(crate::front2::program::lazy::typed::Rechecked::new(program, alloc)))
    }
}

/// THE ORACLE of the typing seam (D163 part 2): every make made twice, by `checking` (the body
/// checked in the make, [`SecondCheck`]) and by `reading` (its checked columns from the typed
/// record, [`FromRecord`]), and the two entries compared byte for byte: an entry is the
/// instance's code, its declared names and its vtables (code_records.rs `make_entry`), so equal
/// entries are equal LIR emitted the same way. The first difference refuses the make by its
/// fn's key; `compared` counts the entries held equal. The walk keeps `checking`'s entry.
pub struct Twin<A, B> {
    pub checking: A,
    pub reading: B,
    /// Entries compared and found equal.
    pub compared: usize,
    scratch: AVec<'static, u8>,
}

impl<A, B> Twin<A, B> {
    pub fn new(checking: A, reading: B, alloc: Alloc<'static>) -> Twin<A, B> {
        Twin { checking, reading, compared: 0, scratch: blaze_own::Vec::new_in(alloc) }
    }

    /// `checking`'s entry is in `entry` from `at`; `reading`'s in the scratch.
    fn same(&mut self, key: ItemKey, entry: &AVec<'_, u8>, at: usize) -> Result<(), RunError> {
        if entry[at..] != self.scratch[..] {
            return Err(RunError::OneBody { key, why: "the typing oracle: the entry made from the typed record differs from the entry made by checking the body again (make_one.rs `Twin`)" });
        }
        self.compared += 1;
        Ok(())
    }
}

impl<A: Make<Error = RunError, Grant = Fixture>, B: Make<Error = RunError, Grant = Fixture>> Make for Twin<A, B> {
    type Error = RunError;
    type Grant = Fixture;

    fn make(&mut self, grant: &Fixture, tabled: ItemKey, name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        let at = entry.len();
        self.checking.make(grant, tabled, name, entry)?;
        self.scratch.clear();
        self.reading.make(grant, tabled, name, &mut self.scratch)?;
        self.same(tabled, entry, at)
    }

    fn make_static(&mut self, grant: &Fixture, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        let at = entry.len();
        self.checking.make_static(grant, item, entry)?;
        self.scratch.clear();
        self.reading.make_static(grant, item, &mut self.scratch)?;
        self.same(item, entry, at)
    }

    fn make_thread_local(&mut self, grant: &Fixture, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        let at = entry.len();
        self.checking.make_thread_local(grant, item, entry)?;
        self.scratch.clear();
        self.reading.make_thread_local(grant, item, &mut self.scratch)?;
        self.same(item, entry, at)
    }

    /// `checking`'s verdict: the walk keeps its entries, and an equal entry holds the same
    /// read list.
    fn verify(&mut self, tabled: ItemKey, name: &[u8], entry: &[u8]) -> Result<Option<Verdict>, RunError> {
        self.checking.verify(tabled, name, entry)
    }
}

/// A make is handed the proof `G` (body_ids.rs `Make`): the maker reads nothing of it, its
/// presence at the call is the obligation (rule 76).
impl<'n, G: Grant, T: Typing<RecordsProgram<'n>>> Make for Maker<'n, G, T> {
    type Error = RunError;
    type Grant = G;

    fn make(&mut self, _grant: &G, tabled: ItemKey, name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        if let Some(void) = self.void(tabled) {
            return Err(void);
        }
        let made = self.make_over(tabled, name, entry);
        // The log first: a refusal or an entry made over an unanswered question is void, and
        // the records' error is its cause.
        if let Some(void) = self.void(tabled) {
            return Err(void);
        }
        made
    }

    fn make_static(&mut self, _grant: &G, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        if let Some(void) = self.void(item) {
            return Err(void);
        }
        let made = self.static_over(item, false, entry);
        // As `make`: an entry made over an unanswered question is void.
        if let Some(void) = self.void(item) {
            return Err(void);
        }
        made
    }

    fn make_thread_local(&mut self, _grant: &G, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        if let Some(void) = self.void(item) {
            return Err(void);
        }
        let made = self.static_over(item, true, entry);
        // As `make`: an entry made over an unanswered question is void.
        if let Some(void) = self.void(item) {
            return Err(void);
        }
        made
    }

    /// A found entry asked again ([`verify_instance`]) over the program, made here as on a
    /// first miss; as `make`, a verdict over an unanswered question is void.
    fn verify(&mut self, tabled: ItemKey, _name: &[u8], entry: &[u8]) -> Result<Option<Verdict>, RunError> {
        if let Some(void) = self.void(tabled) {
            return Err(void);
        }
        self.construct(tabled)?;
        let Some((program, state)) = self.made.as_mut() else {
            return Err(RunError::OneBody { key: tabled, why: "the maker's program was not made (make_one.rs `Maker::verify`)" });
        };
        let verdict = verify_instance(program, &mut self.typing, state, &mut self.checks, tabled, entry);
        if let Some(void) = self.void(tabled) {
            return Err(void);
        }
        verdict.map(Some)
    }
}

/// Why the record walk over `blaze run`'s records stopped.
#[derive(Debug)]
pub enum RecordWalkError {
    /// The entry's name could not be made.
    Entry(crate::symbols::SymbolError),
    /// The walk, the store or one make refused.
    Walk(WalkError<RunError>),
}

impl core::fmt::Display for RecordWalkError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RecordWalkError::Entry(e) => write!(f, "the record walk's entry: {e}"),
            RecordWalkError::Walk(e) => e.fmt(f),
        }
    }
}

impl core::error::Error for RecordWalkError {}

/// The most instances one record walk reaches (rule 18).
pub const MOST_WALKED: usize = 1 << 22;

#[cfg(test)]
#[path = "../../tests/front_equal/lazy_lowering.rs"]
mod lazy_lowering;

/// `blaze run`'s code records by D106: from `entry` (the program's `main`, by its checker key:
/// the command reads it from crate 0's names record) every instance reached is FOUND in `store`
/// by its key from text (`ids`) or made by `maker` and put. Returns the walk (its rows: symbol,
/// key, tabled fn, name, how) for the launcher, and the counts line.
pub fn walk_records<M: Make<Error = RunError, Grant = Fixture>>(
    entry: ItemKey,
    ids: &dyn BodyIds,
    store: &mut dyn CodeStore,
    builder: &Builder,
    producer: &ProducerId<CodeBuilder>,
    maker: &mut M,
    alloc: Alloc<'static>,
) -> Result<(Walk<'static>, Counts), RecordWalkError> {
    let mut name: AVec<'_, u8> = blaze_own::Vec::new_in(alloc);
    let ground = PortableTys::new_in(alloc);
    encode_instance(entry, List::EMPTY, ground.view(), alloc, &mut name).map_err(RecordWalkError::Entry)?;
    let mut walk = Walk::new_in(alloc);
    walk.run((entry, &name), ids, store, builder, producer, maker, &EntryForm, MOST_WALKED).map_err(RecordWalkError::Walk)?;
    let counts = walk.counts();
    Ok((walk, counts))
}

#[cfg(test)]
mod ask_progress_tests {
    use super::{AskKey, AskProgress, ask_key, build_error_ask_key};
    use crate::front2::check::{MOST_PROGRESS_ROUNDS, MOST_ROUNDS};
    use crate::run::{Named, RunError, asks_again, build_error_asks_again};
    use blaze_build::BuildError;
    use blaze_typeck::{ExprId, ItemKey, Question, Shape};

    #[test]
    fn ask_progress_allows_forward_progress_and_caps_stuck_loops() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut progressing = AskProgress::new_in(alloc);
        for r in 0..400u128 {
            let key = AskKey::Question(Question::ImplsOf(ItemKey(1), Shape::Adt(ItemKey(r + 1))));
            assert!(progressing.allow_key(Some(key)), "distinct ImplsOf round {r} must be allowed past MOST_ROUNDS");
        }
        assert!(400 > MOST_ROUNDS && 400 < MOST_PROGRESS_ROUNDS);

        let mut stuck = AskProgress::new_in(alloc);
        let repeated = AskKey::Question(Question::ImplsOf(ItemKey(1), Shape::Adt(ItemKey(42))));
        let mut allowed = 0u32;
        while stuck.allow_key(Some(repeated)) {
            allowed += 1;
        }
        assert_eq!(allowed, MOST_ROUNDS, "a stuck loop repeating one ImplsOf stops at MOST_ROUNDS");

        let mut ceiling = AskProgress::new_in(alloc);
        let mut total = 0u32;
        while ceiling.allow_key(Some(AskKey::Adt(ItemKey(u128::from(total) + 1)))) {
            total += 1;
        }
        assert_eq!(total, MOST_PROGRESS_ROUNDS, "even distinct keys stop at MOST_PROGRESS_ROUNDS");
    }

    #[test]
    fn missing_record_asks_again_and_tracks_adt_ask_key() {
        let item = ItemKey(0xDEAD_BEEF);
        let named = || Named { path: "test::f".into(), at: crate::front::At { file: 0, place: 0 } };
        let missing = BuildError::MissingRecord { item };
        assert!(build_error_asks_again(&missing));
        assert_eq!(build_error_ask_key(&missing), Some(AskKey::Adt(item)));

        let as_build = RunError::Build { function: named(), error: BuildError::MissingRecord { item }, range: None };
        assert!(asks_again(&as_build));
        assert_eq!(ask_key(&as_build), Some(AskKey::Adt(item)));

        let as_refused = RunError::ItemRefused { function: named(), error: BuildError::MissingRecord { item }, item, path: None };
        assert!(asks_again(&as_refused));
        assert_eq!(ask_key(&as_refused), Some(AskKey::Adt(item)));

        let non_retryable = RunError::ItemRefused {
            function: named(),
            error: BuildError::NoStatic { expr: ExprId(0), item },
            item,
            path: None,
        };
        assert!(!asks_again(&non_retryable));
        assert_eq!(ask_key(&non_retryable), None);
    }
}
