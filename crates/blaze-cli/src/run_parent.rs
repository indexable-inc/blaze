//! `blaze run`'s parent (docs/v2/plan/design/run.md, "Pieces" item 6 and "The cut to the lazy
//! parent (D124)"; bottom.md section 11): the program runs in a process of its own, the
//! launcher's run mode (crates/blaze-launcher/src/main.rs `run_mode`), and this process answers
//! its first calls over the socket pair on its descriptor 3 with blaze-launch's run wire
//! (`blaze_launch::wire::run`: `Up::Need` in; `Down::Declare`, `Down::Publish`, `Down::Failed`
//! out), until it exits; its status is `blaze run`'s, unchanged.
//!
//! What is here, sans-IO and whole:
//!
//! - [`Lazy`]: THE ONE [`Resolve`]. A symbol's first call is ONE step of the record walk
//!   (blaze-ladder body_ids.rs `Walk::one`): the instance keyed from text and today's facts, its
//!   entry FOUND in the home's one store (code_store.rs [`crate::code_store::HomeCodeStore`]) or
//!   made by the maker and put there, its record filed in the records folder the launcher reads
//!   ([`Folder`]: one file a record, named by the 64 hex digits of its BLAKE3), and the instances
//!   it declares reached as rows with nothing done for them. Nothing is walked ahead of what is
//!   asked. A symbol the walk has no row for is a SPECIAL record of the run (the C entry shim,
//!   the thread-local getter or ender: blaze-ladder run/special.rs `special_of`, made once by
//!   `make_special`), or else unknown. Nothing of the front end is built until a need requires
//!   it ([`Front`]): the roots on the first call the start list does not answer, the body ids
//!   and the maker on the first row that must be keyed. A workspace body is stepped only when
//!   its check today runs (D151, `Checked::runs`: types whole and no error, asked at the call);
//!   anything else stops the parent
//!   by name ([`ParentError::NotChecked`]), never a second check.
//! - [`Needs`]: the slot bookkeeping, one need answered. A symbol's first need asks the
//!   [`Resolve`] seam ONCE (however many needs name it later); a record is told with the slots
//!   of its symbol and of every symbol it declares (`Declare`) before it (`Publish`); a later
//!   need by that slot gets the record alone; a symbol the resolver does not know is `Failed`
//!   with `Unresolved`, naming the need, every time it is needed, and never resolved again.
//!   [`Needs::place_ahead`] publishes a start list's groups (start_list.rs: each first answer's
//!   data, the statics it names, its code row) before any need is read, each as the run sent
//!   it (its data in one `Data`, then its `Publish`, through the one writer), and
//!   [`Needs::first`] is this run's first-answer order with each answer's statics, the next list.
//! - [`ahead_of`]: the program's personality routine named (its slot declared, then
//!   `Down::Personality`, before any `Publish`: [`Needs::name_personality`]), today's start list
//!   placed ([`Needs::place_ahead`]), each group whose record's or datum's file is not in the
//!   folder, or that names a static no group kept before it sent, left out whole (rule 3:
//!   nothing is published that is not there; it is resolved at its first call), and
//!   the start's head (entry line and personality, run/special.rs `StartHead`): the list's
//!   under today's certificate (no front end built), else the roots'.
//! - [`status_of`]: how the child ended as `blaze run`'s status: an exit's status unchanged; an
//!   end by signal `n` is 128 + n, the shell's form (a Rust program's own status is never
//!   above 255, so it cannot be confused with an exit).
//! - [`launch_env`]: what the launcher's run mode reads from its environment: the command's
//!   own environment (LADDER.md L3), then `BLAZE_LAUNCH_FD=3`, `BLAZE_LAUNCH_STORE`,
//!   `BLAZE_LAUNCH_ENTRY` and, for a hosted program only, `BLAZE_LAUNCH_GRANTS`, which the
//!   launcher removes before the program's first instruction.
//!
//! The driver loop (spawn, serve, wait) is run_loop.rs; the command's edge is own_process.rs.
//!
//! Every failure here comes before the program's first instruction, or is the parent's own
//! while serving it, and is a [`ParentError`], which `blaze run` ends with `program::NOT_RUN`
//! (125), never a status the program could have returned.

use core::alloc::Allocator;
use core::fmt;

use blaze_code::record::Symbol as CodeSymbol;
use blaze_emit::{Isa, Os};
use blaze_hash::ByHash;
use blaze_ids::producer::{CodeBuilder, ProducerId};
use blaze_ladder::body_ids::{self, BodyIds, DeclaredList, Form, How, Make, RowKind, Scratch, Unkeyed, Walk, WalkError};
use blaze_ladder::code_records::{Builder, CodeKey, CodeRecordError, CodeStore, EntryError, NotEmitting, Settings, StoreError, TierName, content_key};
use blaze_launch::wire::data::Datum;
use blaze_ladder::run::make_one::{ByKind, LoweringCounts, MOST_WALKED, MakeSizes, MakeWatch, Maker, NamesRecords};
use blaze_ladder::run::special::{self, Roots, RootsError, SpecialMade, StartHead, make_special, special_of};
use blaze_ladder::run::{OverflowChecks, RunError};
use blaze_launch::wire::run::{BYTES_PART_MOST, Declared, Down, Failure, Needed, Symbol, Up};
use blaze_launch::wire::{List, RecordId, WireError, split};
use blaze_launch::{Slot, Tier};
use blaze_own::{AVec, Alloc, Vec};
use blaze_screen::{Answered, Stage, Step, Unwatched, Watch};
use blaze_typeck::ItemKey;
use bstd::Cx;
use bstd::fs::{Content, Dir, Durable, FsError, ReadWrite};
use bstd::path::{Path, PathError};
use bstd::procs::Exit;

use crate::run_bytes::{Kept, Sent, SentWhy};
use crate::run_ids::{NotRunnable, RunIds, Runnable};
use crate::start_list::{self, Row, TodayGroup, TodayRows};

/// How long the parent waits for the program's next message before it looks at the child
/// again (the launcher's own tick is 250 ms: `blaze_launch_rt::run_mode::TICK_MS`).
pub(crate) const TICK: core::time::Duration = core::time::Duration::from_millis(250);

/// How long a write of answers may wait for the program to read (rule 18): a program whose
/// control thread reads nothing for this long is stopped, named.
pub(crate) const WRITE_BOUND: core::time::Duration = core::time::Duration::from_secs(30);

/// The most slots a run numbers (`blaze_launch_rt::run_mode::MOST_SLOTS`, the launcher's
/// table): one past it is refused, never wrapped.
pub(crate) const MOST_SLOTS: u32 = 1 << 20;

/// The records folder beneath the home: content-named, so two runs of one program share it.
pub(crate) const RECORDS: &str = ".blaze/v2/run/records";

/// The span of one first call answered by the walk ([`Lazy`]); its event says how
/// ([`HOW_FOUND`] and the rest). A span's name is static (bstd `Cx::span`), so the function is
/// not in it: the walk's row is, in the counts line ([`Tally`]).
pub(crate) const FIRST_CALL_SPAN: &str = "blaze run: a first call, one step of the record walk";

/// A first call's events: how it was answered.
pub(crate) const HOW_FOUND: &str = "found: its entry was in the store under its key";
pub(crate) const HOW_MADE: &str = "made: checked, lowered and built, then put under its key";
pub(crate) const HOW_NO_RECORD: &str = "unkeyed: no BodyCheck record, made and not put";
pub(crate) const HOW_STALE: &str = "unkeyed: its BodyCheck record read a fact that moved, made and not put";
pub(crate) const HOW_SPECIAL: &str = "special: the C entry shim or a thread-local routine";

/// 32 bytes as 64 lowercase hex digits, the form the launcher reads.
pub(crate) fn hex(bytes: &[u8; 32]) -> [u8; 64] {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 64];
    for (index, byte) in bytes.iter().enumerate() {
        out[index * 2] = DIGITS[usize::from(byte >> 4)];
        out[index * 2 + 1] = DIGITS[usize::from(byte & 0xf)];
    }
    out
}

/// A symbol's 64 digits in an error.
struct Hex<'h>(&'h [u8; 32]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// Why `blaze run` could not start the program in a process of its own, or stopped serving it.
/// Each names the thing.
#[derive(Debug)]
pub enum ParentError {
    /// The program's edition or another input of its builder (the ladder's words).
    Build(RunError),
    /// One step of the record walk refused the instance of `symbol` (the store, its entry,
    /// its name, the maker, the cap).
    Walk { symbol: [u8; 32], why: WalkError<RunError> },
    /// The special record `symbol` (the shim, the getter, the ender) was not made.
    Special { symbol: [u8; 32], why: CodeRecordError },
    /// The run's roots: the start rule, or a root's name.
    Roots(RootsError),
    /// The body ids were not made (run_ids.rs `RunIdsError`, its words: it borrows the crate
    /// list, so its text is kept).
    BodyIds { shown: Box<str> },
    /// The names records the maker and the roots read were not opened (the command's words).
    Names { shown: Box<str> },
    /// The fn `function` may not run (`why`, run_ids.rs `NotRunnable`: a workspace body's
    /// verdict today, a workspace fn the body ids hold no row for and what they hold beside it,
    /// or a fn of no crate of the list): the call is the gate, so the program stops here, and
    /// the parent never checks or builds it another way.
    NotChecked { function: ItemKey, symbol: [u8; 32], why: NotRunnable },
    /// The code record of `symbol` has a row that lands (a cleanup or a catch that unwinding
    /// must run: its unwind entry needs a personality routine) and no crate of the program
    /// declares `#[lang = "eh_personality"]` (blaze-ladder personality.rs: a program that never
    /// lands needs none, so this is refused here, at the record, never at the start).
    /// `function`: the record's fn (`None`: a special record); `path`: its item path, when the
    /// names can give it.
    NoPersonality { function: Option<ItemKey>, path: Option<Box<str>>, symbol: [u8; 32] },
    /// The code record of `symbol` does not read as a code record (asked whether it lands).
    Decode { symbol: [u8; 32], why: blaze_code::record::DecodeError },
    /// An item-relative slot of this code record could not be filled from today's source.
    Position { symbol: [u8; 32], row: blaze_code::record::PositionRow, why: blaze_ladder::seam::PositionGap },
    /// The code record of `symbol` has unwind rows that do not read (asked whether it lands).
    Unwind { symbol: [u8; 32], why: blaze_code::unwind::UnwindError },
    /// The resolver was asked twice for `symbol` (`Needs` asks once: a bug).
    ResolvedTwice { symbol: [u8; 32] },
    /// The special record of `symbol` (the shim, the getter, the ender) names a static: the
    /// specials are made by run/special.rs over no program, so none may (run.md, "Statics in
    /// the run").
    SpecialStatic { symbol: [u8; 32] },
    /// A C name the program defines (`symbol`, `Symbol::of_name`) was to be declared after a
    /// record was published: the child may have bound that name outside the program already.
    /// Refused ([`Needs::declare_c_defined`]): every program-defined C name is declared before
    /// the first `Publish` (D175; the child refuses the converse, run_mode.rs
    /// `DefinedAfterBound`).
    CDefinedLate { symbol: [u8; 32] },
    /// The program's native library was to be told after `slots` slots were numbered: the
    /// child takes its `Native` first, before any `Declare` (blaze-launch wire/run.rs), or it
    /// would hand the entry over before the library is loaded ([`Needs::tell_native`]).
    NativeLate { slots: usize },
    /// A static row of the walk (`symbol`) was served once already and is not told: a bug of
    /// `Lazy::resolve`'s statics.
    StaticServed { symbol: [u8; 32] },
    /// Record `record` names static `symbol` (a `Space::Static` target of its own, or of a
    /// datum sent with it; or a thread-local, whose template is its datum), and no `Data` of
    /// this run sent it: refused before its `Publish` ([`Needs::publish`]). `instance`: the fn
    /// symbol of the slot the record was to fill; `touches_tls`: the record also names the
    /// thread-local descriptor, so `symbol` may be a thread-local's template (symbols.rs
    /// `tls_symbol`) that the walk's declared list did not hold.
    StaticUnsent { record: [u8; 32], symbol: [u8; 32], instance: Option<[u8; 32]>, touches_tls: bool },
    /// Record `record` was published again as one published before, and never was (a bug of
    /// `Needs`'s answers).
    NotCleared { record: [u8; 32] },
    /// The store refused the entry of `symbol`.
    Store { symbol: [u8; 32], why: StoreError },
    /// The walk found the entry of `symbol` and the store has none under its key.
    NotStored { symbol: [u8; 32] },
    /// The entry of `symbol` does not read.
    Entry { symbol: [u8; 32], why: EntryError },
    /// Code records are AArch64 (blaze-emit): the run mode does not start on another machine.
    NotAarch64,
    /// A record's file name or the records folder.
    Path(PathError),
    /// Writing or looking at a record's file.
    Fs(FsError),
    /// The records folder beneath the home.
    Home(bstd::fs::HomeError),
    /// No `HOME` to put the records folder under.
    NoHome,
    /// A message of the run wire.
    Wire(WireError),
    /// The program named a slot this parent never declared (a launcher bug).
    UndeclaredSlot { slot: u32 },
    /// The run numbered [`MOST_SLOTS`] slots and the program asked for one more.
    Full,
    /// The code builder names a tier whose emitter does not make entries.
    NotEmitting(NotEmitting),
    /// The start list's rows past its cap (never while `Needs` keeps the window: a bug).
    StartList(start_list::CapError),
    /// The record id `record` is not a file name of the records folder.
    RecordName { record: [u8; 32] },
    /// The walk forward from the roots stepped `most` functions the program has not called and
    /// did not reach `symbol` (rule 18).
    TooFar { symbol: [u8; 32], most: usize },
    /// A made entry was handed to the parent ([`Lazy::made`]) and no row of it waits for one (a
    /// bug of the relay, D194).
    NothingAsked,
    /// The walk began the row of `symbol` while the row of `asked` waits for its code: one
    /// child asks one make at a time (D194; a bug of [`Lazy`]).
    TwoAsked { symbol: [u8; 32], asked: [u8; 32] },
    /// The row of `symbol` was to be served under [`Codes::Pooled`] before its code (and its
    /// statics') came: the ready pass steps them first (a bug of [`Lazy::publication`]).
    NotReady { symbol: [u8; 32] },
    /// Under [`Codes::Pooled`], a code worker refused the make of the row of `symbol`: the walk's
    /// refusal as it crossed to the command thread (D194, [`CodeRefusal`]).
    CodeRefused { symbol: [u8; 32], why: CodeRefusal },
    /// Record `record`, sent inline (run_bytes.rs, run wire `Bytes`), or found again for a
    /// replay or a crash's read: `why`.
    Sent { record: [u8; 32], why: SentWhy },
}

/// A make's refusal as it crosses from a code worker to the command thread (D194 piece 2).
/// `WalkError<RunError>` is not `Send` (rustc, the whole-tree check c94 at test_drive.rs's
/// `must_overlap`), so the worker keeps of it, typed, what the command thread reads of a
/// refusal: whose it is and where (decided on the worker by the serial path's own function,
/// test_drive.rs `instance_site` over [`ParentError::scope`], so the two cannot differ), and its words as a test's NOT RUN
/// and a suite's stop print them ([`ParentError::Walk`]'s, byte for byte). The words are in the
/// command's heap, handed in.
#[derive(Debug)]
pub struct CodeRefusal {
    /// The refused instance's tabled fn and the byte of its body where its make refused (`0`: the
    /// body, no place); `None`: a fault that is the suite's, not one test's.
    pub at: Option<(ItemKey, u32)>,
    /// What a test's NOT RUN prints after the instance for a maker's refusal (the build's error,
    /// or the maker's words: test_drive.rs `InstanceRefusal`); `None` for the walk's own.
    pub cause: Option<blaze_own::String<blaze_own::SharedAlloc<'static>>>,
    /// The walk's refusal as [`ParentError::Walk`] prints it after the symbol.
    pub walk: blaze_own::String<blaze_own::SharedAlloc<'static>>,
    /// The instance as the maker that refused it shows it (its path and ground arguments, by
    /// test_drive.rs `InstanceName`, the serial NOT RUN's printer): the command thread's walk
    /// has no program to show it by (its maker is the worker's). `None`: the worker's maker
    /// showed none (no program made yet, or the make panicked and its maker was forgotten).
    pub instance: Option<blaze_own::String<blaze_own::SharedAlloc<'static>>>,
    /// `at` placed in its file by the worker's maker, for a person only (rule 71: printed,
    /// never kept); `None`: not placed.
    pub site: Option<CrossedSite>,
    /// Whether the words are whole: cut at [`MOST_REFUSAL_WORDS`] or refused in part by the
    /// command's heap (rule 3: said, never cut quietly).
    pub whole: bool,
}

/// A crossed refusal's source place: its file as the program observes it (`file!()`), line and
/// column, in the command's heap.
#[derive(Debug)]
pub struct CrossedSite {
    pub file: blaze_own::String<blaze_own::SharedAlloc<'static>>,
    pub line: u32,
    pub column: u32,
}

/// The most bytes each of a crossed refusal's words is kept at (rule 18): the relay keeps one
/// refusal a key for every asker, so its size is bounded here (test_drive.rs `crossing`).
pub const MOST_REFUSAL_WORDS: usize = 4 << 10;

/// What a cut refusal says after its words.
pub const WORDS_CUT: &str = " (its words were cut: past the cap of run_parent.rs `MOST_REFUSAL_WORDS`, or the command's heap refused the rest)";

impl fmt::Display for CodeRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.walk.as_str())?;
        if !self.whole {
            f.write_str(WORDS_CUT)?;
        }
        Ok(())
    }
}

impl CodeRefusal {
    /// The same refusal for one more asker of its key: the relay keeps one a key and hands it to
    /// every asker (code_pool_relay.rs, rule 19: each test prints the cause itself, never a
    /// pointer to another test's). Its words are copied into the heap they are in (the command's);
    /// a part that heap refused is said, `whole` cleared (rule 3).
    pub fn for_asker(&self) -> CodeRefusal {
        let alloc = *self.walk.allocator();
        let mut whole = self.whole;
        let mut copy = |words: &str| match blaze_own::String::try_from_str_in(words, alloc) {
            Ok(copied) => copied,
            Err(_) => {
                whole = false;
                blaze_own::String::new_in(alloc)
            }
        };
        let cause = self.cause.as_ref().map(|cause| copy(cause.as_str()));
        let walk = copy(self.walk.as_str());
        let instance = self.instance.as_ref().map(|instance| copy(instance.as_str()));
        let site = self.site.as_ref().map(|site| CrossedSite { file: copy(site.file.as_str()), line: site.line, column: site.column });
        CodeRefusal { at: self.at, cause, walk, instance, site, whole }
    }

    /// Whose the refusal is, as the worker sorted it (`at`: `None` for the suite's).
    pub fn scope(&self) -> blaze_ladder::scope::Scope {
        match self.at {
            Some(_) => blaze_ladder::scope::Scope::One,
            None => blaze_ladder::scope::Scope::Suite,
        }
    }
}

impl ParentError {
    /// Whose this fault is while one test's instance is made (blaze-ladder scope.rs's rule):
    /// a step of the walk, a crossed refusal and a special record are the scope of what they
    /// hold; a fn that may not run and a record that lands with no personality routine are
    /// the instance's; every other (the wire and the protocol of the run, its roots, the names
    /// records it opened, the store, the relay's own invariants, and the program's builder
    /// before any instance, `Build`) is the suite's. Before 2026-11-21 those were
    /// test_drive.rs `refused_instance`'s `_ => return None`.
    pub fn scope(&self) -> blaze_ladder::scope::Scope {
        use blaze_ladder::scope::Scope;
        match self {
            ParentError::Walk { why, .. } => why.scope(),
            ParentError::CodeRefused { why, .. } => why.scope(),
            ParentError::Special { why, .. } => why.scope(),
            ParentError::NotChecked { .. } | ParentError::NoPersonality { .. } => Scope::One,
            ParentError::Build(_)
            | ParentError::Roots(_)
            | ParentError::BodyIds { .. }
            | ParentError::Names { .. }
            | ParentError::Decode { .. }
            | ParentError::Position { .. }
            | ParentError::Unwind { .. }
            | ParentError::ResolvedTwice { .. }
            | ParentError::SpecialStatic { .. }
            | ParentError::CDefinedLate { .. }
            | ParentError::NativeLate { .. }
            | ParentError::StaticServed { .. }
            | ParentError::StaticUnsent { .. }
            | ParentError::NotCleared { .. }
            | ParentError::Store { .. }
            | ParentError::NotStored { .. }
            | ParentError::Entry { .. }
            | ParentError::NotAarch64
            | ParentError::Path(_)
            | ParentError::Fs(_)
            | ParentError::Home(_)
            | ParentError::NoHome
            | ParentError::Wire(_)
            | ParentError::UndeclaredSlot { .. }
            | ParentError::Full
            | ParentError::NotEmitting(_)
            | ParentError::StartList(_)
            | ParentError::RecordName { .. }
            | ParentError::TooFar { .. }
            | ParentError::NothingAsked
            | ParentError::TwoAsked { .. }
            | ParentError::NotReady { .. }
            | ParentError::Sent { .. } => Scope::Suite,
        }
    }
}

impl fmt::Display for ParentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = "`blaze run` did not start or serve the program (run.md piece 6, D124)";
        match self {
            ParentError::Build(why) => write!(f, "{rule}: {why}"),
            ParentError::Walk { symbol, why } => write!(f, "{rule}: the first call of symbol {}: {why}", Hex(symbol)),
            ParentError::CodeRefused { symbol, why } => write!(f, "{rule}: the first call of symbol {}: {why}", Hex(symbol)),
            ParentError::Special { symbol, why } => write!(f, "{rule}: the run's special record {}: {why:?}", Hex(symbol)),
            ParentError::Roots(why) => write!(f, "{rule}: {why}"),
            ParentError::BodyIds { shown } => write!(f, "{rule}: the body ids: {shown}"),
            ParentError::Names { shown } => write!(f, "{rule}: the names records: {shown}"),
            ParentError::NotChecked { function, symbol, why } => {
                write!(f, "{rule}: the program called fn {:#x} (symbol {}), which may not run: ", function.0, Hex(symbol))?;
                match why {
                    NotRunnable::Checked(verdict) => write!(f, "a workspace body whose check today is {verdict:?}")?,
                    NotRunnable::NoRow(held) => write!(f, "a workspace body the body ids hold nothing for ({held})")?,
                    NotRunnable::NoGrain { .. } | NotRunnable::Unread(_) => write!(f, "{why}")?,
                }
                write!(f, "; the call is the gate (D151): a workspace body is run only when its types were checked whole and it has no error (`Clean`, or `Typed`: its borrow check refused or not run), and the parent never checks or builds one another way")
            }
            ParentError::NoPersonality { function, path, symbol } => {
                match (path, function) {
                    (Some(path), Some(function)) => write!(f, "{rule}: fn `{path}` ({:#x}, symbol {})", function.0, Hex(symbol))?,
                    (None, Some(function)) => write!(f, "{rule}: fn {:#x} (symbol {})", function.0, Hex(symbol))?,
                    (_, None) => write!(f, "{rule}: the run's special record {}", Hex(symbol))?,
                }
                write!(
                    f,
                    " has a cleanup or a catch that unwinding must run, and no crate of the program declares a `#[lang = \"eh_personality\"]` fn to run it: declare one, or build with `panic = \"abort\"` (blaze-ladder personality.rs)"
                )
            }
            ParentError::Decode { symbol, why } => write!(f, "{rule}: the code record of symbol {} does not read: {why}", Hex(symbol)),
            ParentError::Position { symbol, row, why } => write!(f, "{rule}: symbol {}, {:?} offset {} needs the {:?} of item {:#x} at byte {}: {why}", Hex(symbol), row.part, row.at, row.field, row.item, row.site),
            ParentError::Unwind { symbol, why } => write!(f, "{rule}: the unwind rows of the code record of symbol {}: {why}", Hex(symbol)),
            ParentError::ResolvedTwice { symbol } => write!(f, "{rule}: the symbol {} was resolved twice (run_parent.rs `Needs` asks once a symbol)", Hex(symbol)),
            ParentError::SpecialStatic { symbol } => write!(f, "{rule}: the run's special record {} names a static, and the specials are made over no program (run.md, \"Statics in the run\")", Hex(symbol)),
            ParentError::NativeLate { slots } => write!(f, "{rule}: the program's native library was to be told after {slots} slots were numbered: it is the run's first frame, before any `Declare` (run_parent.rs `Needs::tell_native`, blaze-launch wire/run.rs `Down::Native`)"),
            ParentError::CDefinedLate { symbol } => write!(f, "{rule}: the program's own C name of symbol {} was declared after a record was published: every program-defined C name is declared before the first record (run_parent.rs `Needs::declare_c_defined`)", Hex(symbol)),
            ParentError::StaticServed { symbol } => write!(f, "{rule}: the static {} was served once and is not among the statics told (run_parent.rs `Lazy::resolve`)", Hex(symbol)),
            ParentError::StaticUnsent { record, symbol, instance, touches_tls } => {
                write!(f, "{rule}: record {}", Hex(record))?;
                if let Some(instance) = instance {
                    write!(f, " (fn symbol {})", Hex(instance))?;
                }
                match touches_tls {
                    true => write!(f, " touches a thread-local and names {}, a static or a thread-local's template", Hex(symbol))?,
                    false => write!(f, " names {}, a static or a vtable", Hex(symbol))?,
                }
                write!(f, ", which no `Data` of this run sent: the walk's declared list for this record does not hold it; refused before its `Publish` (run_parent.rs `Needs::publish`; run.md, \"Statics in the run\")")
            }
            ParentError::NotCleared { record } => write!(f, "{rule}: record {} was published again and never published first (run_parent.rs `Needs::publish`)", Hex(record)),
            ParentError::Store { symbol, why } => write!(f, "{rule}: the code entry of symbol {}: {why}", Hex(symbol)),
            ParentError::NotStored { symbol } => write!(f, "{rule}: the record walk found symbol {} and the store holds no entry under its key", Hex(symbol)),
            ParentError::Entry { symbol, why } => write!(f, "{rule}: the code entry of symbol {} does not read: {why}", Hex(symbol)),
            ParentError::NotAarch64 => write!(f, "{rule}: code records are AArch64 and this machine is not"),
            ParentError::Path(why) => write!(f, "{rule}: a record's path: {why}"),
            ParentError::Fs(why) => write!(f, "{rule}: a code record's file: {why}"),
            ParentError::Home(why) => write!(f, "{rule}: the records folder `{RECORDS}`: {why}"),
            ParentError::NoHome => write!(f, "{rule}: `HOME` is not set, and the records folder `{RECORDS}` is beneath it"),
            ParentError::Wire(why) => write!(f, "{rule}: the program's run wire: {why}"),
            ParentError::UndeclaredSlot { slot } => write!(f, "{rule}: the program asked for slot {slot}, which this parent never declared"),
            ParentError::Full => write!(f, "{rule}: the program asked for more than {MOST_SLOTS} functions"),
            ParentError::NotEmitting(why) => write!(f, "{rule}: {why}"),
            ParentError::StartList(why) => write!(f, "{rule}: {why}"),
            ParentError::RecordName { record } => write!(f, "{rule}: the record id {} is not a file name of `{RECORDS}`", Hex(record)),
            ParentError::TooFar { symbol, most } => write!(f, "{rule}: the walk forward from the roots stepped {most} functions the program has not called and did not reach symbol {}", Hex(symbol)),
            ParentError::NothingAsked => write!(f, "{rule}: a made entry was handed back and no row waits for one (run_parent.rs `Lazy::made`, D194)"),
            ParentError::TwoAsked { symbol, asked } => write!(f, "{rule}: the row of symbol {} was begun while the row of symbol {} waits for its code: one make at a time a child (run_parent.rs `Lazy`, D194)", Hex(symbol), Hex(asked)),
            ParentError::NotReady { symbol } => write!(f, "{rule}: the row of symbol {} was to be served before its code came (run_parent.rs `Lazy::publication`'s ready pass, D194)", Hex(symbol)),
            ParentError::Sent { record, why } => write!(f, "{rule}: record {}, sent to the program inline (run_bytes.rs, run wire `Bytes`): {why}", Hex(record)),
        }
    }
}

impl core::error::Error for ParentError {}

impl From<WireError> for ParentError {
    fn from(why: WireError) -> ParentError {
        ParentError::Wire(why)
    }
}

/// The machine the records are made for: this one (the program runs on it).
pub(crate) fn isa() -> Result<Isa, ParentError> {
    if !cfg!(target_arch = "aarch64") {
        return Err(ParentError::NotAarch64);
    }
    Ok(Isa {
        lse: true,
        os: match cfg!(target_os = "macos") {
            true => Os::MacOs,
            false => Os::Linux,
        },
    })
}

/// What the program's records are made for, the code key's builder half. THE COMMAND'S PANIC
/// STRATEGY: `panic`, the program's one panic strategy, the profile's `panic` (UNWIND.md 3.1),
/// passed in by the caller; it enters the code key here, and the maker reads it from there
/// (make_one.rs `Maker::new`). `overflow` is the profile's `overflow-checks` (D101). The tier is
/// the emitter's that makes every entry (code_records.rs `EMITTING`, checked by `emits`), the
/// toolchain the pinned one, `producer` the code builder's identity (D105), `edition` the
/// program package's as its manifest writes it.
pub(crate) fn builder_for(edition: &str, overflow: OverflowChecks, panic: blaze_ladder::run::Panic, producer: ProducerId<CodeBuilder>, isa: Isa) -> Result<Builder, ParentError> {
    let edition = edition::PackageEdition::of_manifest(edition).map_err(|_| ParentError::Build(RunError::Edition { written: Box::from(edition) }))?;
    Builder { isa, tier: TierName::Reference, settings: Settings::baseline(overflow, panic), toolchain: blaze_toolchain::PINNED, producer, edition }.emits().map_err(ParentError::NotEmitting)
}

/// The launcher run mode's environment: the command's own (`env`, each pair in the world's
/// order), then the names the launcher reads, each set once (a pair of the command's own under
/// one of those names is left out: the parent's word is the run's). `grants` is
/// `BLAZE_LAUNCH_GRANTS`'s value (blaze-launch grants.rs `format`), set for a HOSTED entry
/// alone, even when empty; `None` for any other form, so the variable is not set.
pub(crate) fn launch_env<'e, A: Allocator>(
    env: impl Iterator<Item = (&'e str, &'e str)>,
    store: &'e str,
    entry: &'e str,
    grants: Option<&'e str>,
    out: &mut Vec<(&'e str, &'e str), A>,
) {
    // BLAZE_LAUNCH_NATIVE and BLAZE_LAUNCH_TELEMETRY are the parent's word alone: one of the
    // command's own is never passed on.
    const OURS: [&str; 7] = [
        "BLAZE_LAUNCH_FD",
        "BLAZE_LAUNCH_STORE",
        "BLAZE_LAUNCH_ENTRY",
        "BLAZE_LAUNCH_NATIVE",
        blaze_launch::grants::VAR,
        blaze_launch::telemetry::VAR,
        "BLAZE_LAUNCH_EXPLORE",
    ];
    for (name, value) in env {
        if !OURS.contains(&name) {
            out.push((name, value));
        }
    }
    out.push(("BLAZE_LAUNCH_FD", "3"));
    out.push(("BLAZE_LAUNCH_STORE", store));
    out.push(("BLAZE_LAUNCH_ENTRY", entry));
    if let Some(grants) = grants {
        out.push((blaze_launch::grants::VAR, grants));
    }
}

/// How the child ended, as `blaze run`'s status.
pub(crate) fn status_of(exit: Exit) -> u8 {
    match exit {
        // A process's exit status is its low 8 bits (`_exit`): what the kernel reports.
        Exit::Exited(status) => (status & 0xff) as u8,
        // The shell's form for an end by signal: 128 + its number.
        Exit::Signaled(signal) => 128u8.saturating_add((signal & 0x7f) as u8),
    }
}

/// What a [`Resolve`] found for a symbol.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Resolved<'r> {
    /// The symbol's code record: ALREADY FILED in the records folder by the resolver
    /// ([`Folder::file`]), its code key then (the start list's row), its id, and the symbols the
    /// record declares (its targets: each is told its slot before the record is published, so
    /// placing it never meets a symbol without a slot), and the statics the record reaches that
    /// this run has not told yet: each datum ALREADY FILED too, sent in one `Down::Data` before
    /// the record's `Publish` (a static has no stub; run.md, "Statics in the run").
    /// `named`: every static the record and those data name, read from their own targets
    /// ([`Front::statics_named`]), sorted, each once: [`Needs::publish`] refuses the record
    /// unless each is on the child before it; the start list keeps them with the record's row
    /// (start_list.rs version 4), so a replay sends the same. `sent`: the records the folder
    /// sends inline for this answer ([`Folder::take`], run_bytes.rs), which [`Needs::answer`]
    /// puts on the wire before any frame naming them; [`Sent::NONE`] for the files control.
    Record { key: CodeKey, record: RecordId, declares: &'r [Symbol], data: &'r [Datum], named: &'r [Symbol], sent: Sent<'r> },
    /// No function of the program by that symbol: the need is answered `Failed` with
    /// `Failure::Unresolved`, naming it.
    Unknown,
    /// Not answered yet: a row on the way waits for its code, made off the command thread
    /// ([`Codes::Pooled`], D194; [`Lazy::asked`]). The need is not taken: it is asked again once
    /// the code came ([`Lazy::made`]), and nothing of it was told.
    Waiting,
}

/// Where a [`Lazy`]'s code is made (D194, rule 78: the one seam, each tested equal): on the
/// command thread inside the step (`Serial`, `blaze run`'s and today's), or by a worker of the
/// command's pool while the command thread only relays (`Pooled`: the walk begins the row,
/// [`Lazy::asked`] says what to make, [`Lazy::made`] finishes it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Codes {
    Serial,
    Pooled,
}

/// The make a [`Lazy`] waits for under [`Codes::Pooled`]: what a worker needs, all copied or
/// `Copy` (the walk and the store stay on the command thread).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ask<'w> {
    /// The key the entry is put under; `None`: unkeyed, made for this ask alone.
    pub key: Option<CodeKey>,
    pub order: body_ids::Order,
    /// The row's instance name (body_ids.rs `Walk::name`).
    pub name: &'w [u8],
    /// The gate's proof for the row's fn, which the make takes (D163).
    pub proof: Runnable,
}

/// The seam a need asks through for a symbol [`Needs`] has not resolved yet. [`Needs`] asks it
/// at most once a symbol, however many needs name it, and never for a row placed from today's
/// start list. The command's one impl is [`Lazy`].
pub(crate) trait Resolve {
    /// Read a model answer in the slot-bookkeeping tests.
    #[cfg(test)]
    fn resolve(&mut self, symbol: &Symbol) -> Result<Resolved<'_>, ParentError>;
    /// Resolve a publication, appending the values for its position rows in sealed order.
    #[cfg(not(test))]
    fn publication(&mut self, symbol: &Symbol, values: &mut AVec<'_, u8>) -> Result<Resolved<'_>, ParentError>;
    /// The slot-bookkeeping model's records have no positions.
    #[cfg(test)]
    fn publication(&mut self, symbol: &Symbol, _values: &mut AVec<'_, u8>) -> Result<Resolved<'_>, ParentError> {
        self.resolve(symbol)
    }
    /// The sink the run's events go to (blaze-screen [`Watch`]): the loop tells it when the
    /// program starts and when a need is answered. [`Unwatched`] unless the resolver has one.
    fn watch(&self) -> &dyn Watch {
        &Unwatched
    }
}

/// THE GATE, asked before a row is stepped: the proof that the fn may run (run_ids.rs
/// `Runnable`), or why not. The step takes the proof ([`Cleared`]), so no row is stepped that
/// the gate did not let.
pub(crate) trait Gate: BodyIds {
    fn runnable(&self, item: ItemKey) -> Result<Runnable, NotRunnable>;
}

impl Gate for RunIds<'_> {
    fn runnable(&self, item: ItemKey) -> Result<Runnable, NotRunnable> {
        RunIds::runnable(self, item)
    }
}

/// What the run starts from: the entry line and the root instances (run/special.rs [`Roots`]),
/// and the special records (the shim, the getter, the ender).
pub(crate) trait Start {
    /// The launcher's line for the entry and the personality routine's symbol.
    fn head(&self) -> StartHead;
    /// How many root instances.
    fn root_count(&self) -> usize;
    /// Root instance `i`: `(tabled fn, instance name)`, as `Walk::root` takes it.
    fn root_row(&self, i: usize) -> (ItemKey, &[u8]);
    /// The checker key of the fn the process starts at, as the maker takes it: `main`'s.
    fn maker_entry(&self) -> Option<ItemKey>;
    /// `symbol`'s special record found or made in `store` (`None`: it is none of the run's).
    fn special(&self, symbol: &CodeSymbol, builder: &Builder, store: &mut dyn CodeStore, alloc: Alloc<'_>) -> Result<Option<SpecialMade>, ParentError>;
}

impl Start for Roots<'_> {
    fn head(&self) -> StartHead {
        Roots::head(self)
    }

    fn root_count(&self) -> usize {
        self.len()
    }

    fn root_row(&self, i: usize) -> (ItemKey, &[u8]) {
        self.row(i)
    }

    fn maker_entry(&self) -> Option<ItemKey> {
        match self.shim() {
            Some(shim) => Some(shim.main.tabled),
            None => (!self.is_empty()).then(|| self.root(0).tabled),
        }
    }

    fn special(&self, symbol: &CodeSymbol, builder: &Builder, store: &mut dyn CodeStore, alloc: Alloc<'_>) -> Result<Option<SpecialMade>, ParentError> {
        let Some(which) = special_of(symbol, self) else { return Ok(None) };
        make_special(which, builder, store, alloc).map(Some).map_err(|why| ParentError::Special { symbol: symbol.0, why })
    }
}

/// Prepare a record for publication. Supply its source-position values and, if the
/// program has no personality, say whether the record needs one.
pub(crate) trait Publication {
    fn for_publication(&mut self, symbol: [u8; 32], record: &[u8], no_personality: bool, values: &mut AVec<'_, u8>) -> Result<bool, ParentError>;
}

/// The front end, made only when a need requires it ([`Lazy`]): the roots on the first call
/// the start list does not answer, the body ids and the maker on the first row stepped.
pub(crate) trait Front: Publication {
    type Start: Start;
    type Ids: Gate;
    /// The maker takes the run gate's proof and nothing else (D163: `Runnable` is the only
    /// thing the make path accepts).
    type Maker: Make<Error = RunError, Grant = Runnable>;
    fn start(&mut self) -> Result<Self::Start, ParentError>;
    fn ids(&mut self) -> Result<Self::Ids, ParentError>;
    /// The maker over the roots `start`, made after the body ids `ids` (whose view a typing
    /// that reads the check's records reads through: [`RunMaker`]).
    fn maker(&mut self, start: &Self::Start, ids: &Self::Ids) -> Result<Self::Maker, ParentError>;
    /// How an entry says what it declares: the run path's entry form.
    fn form(&self) -> &dyn Form {
        &body_ids::EntryForm
    }
    /// The record an entry holds, what the launcher maps: a code record, or a static's datum.
    fn record_of<'e>(&self, entry: &'e [u8]) -> Result<&'e [u8], EntryError> {
        blaze_ladder::code_records::statics::entry_record(entry)
    }
    /// Each static the entry's RECORD names (its `Space::Static` targets, read from the record:
    /// code_records.rs `statics::each_static_named`), onto `out`: what [`Needs::publish`] checks
    /// is on the child before the record.
    fn statics_named(&self, entry: &[u8], out: &mut AVec<'_, Symbol>) -> Result<(), EntryError> {
        blaze_ladder::code_records::statics::each_static_named(entry, |symbol| out.push(Symbol(symbol.0)))
    }
    /// Each vtable the entry carries, its symbol and its datum's bytes, to `each`: what
    /// [`Lazy::resolve`] files and sends in the record's `Data` (code_records.rs form 6). A
    /// malformed entry is refused as `symbol`'s (the instance the entry is of).
    fn each_vtable(&self, symbol: [u8; 32], entry: &[u8], each: &mut dyn FnMut(Symbol, &[u8]) -> Result<(), ParentError>) -> Result<(), ParentError> {
        blaze_ladder::code_records::statics::each_vtable_datum(entry, |vtable, datum| each(Symbol(vtable.0), datum), |why| ParentError::Entry { symbol, why })
    }
    /// The item path of fn `item` (`krate::module::name`), for a refusal's words: `None` when
    /// the front cannot name it (the refusal then shows its key). Read only on an error's path.
    fn path_of(&mut self, _item: ItemKey) -> Option<Box<str>> {
        None
    }
    /// Instance `name` (its walk row's name bytes) of fn `tabled`, for a crash's words
    /// (run_fault.rs): `maker` is the walk's, when a row was stepped. This default names the
    /// item by [`Front::path_of`] alone; a front with a printer names it by that and gives the
    /// instance's arguments. Read only on a crash's path.
    fn named(&mut self, maker: Option<&Self::Maker>, tabled: ItemKey, name: &[u8]) -> crate::run_fault::Named {
        let _ = (maker, name);
        match self.path_of(tabled) {
            Some(path) => crate::run_fault::Named::Path(path),
            None => crate::run_fault::Named::Item(tabled),
        }
    }
    /// Byte `site` of item `item`'s text (a code record's trap row, item-relative: rule 71)
    /// for a crash's words: its file, line and column today. This default places nothing.
    /// Read only on a crash's path.
    fn site_shown(&mut self, maker: Option<&Self::Maker>, item: ItemKey, site: u32) -> Option<crate::run_fault::SiteShown> {
        let _ = (maker, item, site);
        None
    }
    /// The sink the run's events go to ([`Watch`]): [`Unwatched`] unless the front has one.
    fn watch(&self) -> &dyn Watch {
        &Unwatched
    }
}

/// A code record that lands, refused when the run names no personality routine
/// ([`ParentError::NoPersonality`]): `personality` is the run's head's; `function`, the record's
/// fn (`None`: a special record). The same decoded record supplies the position values.
fn prepare_publication<F: Front>(
    front: &mut F, personality: Option<CodeSymbol>, function: Option<ItemKey>, symbol: [u8; 32], record: &[u8], values: &mut AVec<'_, u8>,
) -> Result<(), ParentError> {
    if !front.for_publication(symbol, record, personality.is_none(), values)? {
        return Ok(());
    }
    let path = function.and_then(|item| front.path_of(item));
    Err(ParentError::NoPersonality { function, path, symbol })
}

/// The command's [`Front`]: the names records (`names`, opened once on the first need of them
/// and kept) for the roots and the maker, and the body ids (`ids`, made once, on the first row
/// stepped). `tables` is the process's heap, where the head store's and the lazy program's
/// tables live (typed `'static`); `alloc` holds the roots; `watch` is told each stage begun.
pub(crate) struct CommandFront<'h, 'c, 'r> {
    names: &'c mut dyn FnMut() -> Result<NamesRecords<'h>, ParentError>,
    ids: &'c mut dyn FnMut() -> Result<RunIds<'h>, ParentError>,
    /// The typed reader's parts besides the ids (engine_body_record.rs `TypedFrom`).
    typed: crate::engine_body_record::TypedFrom<'h>,
    kept: Option<NamesRecords<'h>>,
    builder: Builder,
    tables: Alloc<'static>,
    alloc: Alloc<'r>,
    watch: &'c dyn Watch,
    /// The Rust fns the program's C calls (engine_native.rs `NativeSend::items`): roots of the
    /// walk (blaze-ladder run/special.rs `run_roots`).
    exports: &'c [blaze_id::ItemKey],
    /// What each maker reports its large or slow makes with ([`command_make_watch`]).
    make_watch: MakeWatch<'static>,
}

/// The line of one large or slow make (make_one.rs [`MakeSizes`], printed only past its
/// `MAKE_REPORT_BYTES` or `MAKE_REPORT_TIME`), to the command's stderr (rule 16).
fn report_make(sizes: &MakeSizes) {
    std::eprintln!("{sizes}");
}

/// The command's make watch: ONE clock, started here at the command's edge where its front or
/// its code workers are made (rule 75: the makers are handed it, never read one), and
/// [`report_make`] as where each report goes. make_one.rs reports only a make past its
/// threshold, so a run of small makes prints nothing.
pub(crate) fn command_make_watch() -> MakeWatch<'static> {
    MakeWatch { clock: blaze_core::wait::Monotonic::new(), report: &report_make }
}

impl<'h, 'c, 'r> CommandFront<'h, 'c, 'r> {
    #[expect(clippy::too_many_arguments, reason = "each is one input of the run's front, named at the call")]
    pub(crate) fn new(
        names: &'c mut dyn FnMut() -> Result<NamesRecords<'h>, ParentError>,
        ids: &'c mut dyn FnMut() -> Result<RunIds<'h>, ParentError>,
        typed: crate::engine_body_record::TypedFrom<'h>,
        builder: Builder,
        tables: Alloc<'static>,
        alloc: Alloc<'r>,
        watch: &'c dyn Watch,
        exports: &'c [blaze_id::ItemKey],
        make_watch: MakeWatch<'static>,
    ) -> CommandFront<'h, 'c, 'r> {
        CommandFront { names, ids, typed, kept: None, builder, tables, alloc, watch, exports, make_watch }
    }

    fn names(&mut self) -> Result<NamesRecords<'h>, ParentError> {
        if let Some(names) = self.kept {
            return Ok(names);
        }
        self.watch.see(Step::Began(Stage::Names));
        let names = (self.names)()?;
        self.kept = Some(names);
        Ok(names)
    }
}

/// THE RUN'S MAKER: handed the run gate's proof ([`Runnable`]), each body's checked columns
/// from the typing seam (blaze-ladder make_one.rs `Typing`), BY KIND (D163 part 2, D197 root
/// 1): a body `check` kept a typed record of is read from it, with no checker round; a body
/// of a kind `check` keeps none for yet is the run's own check, counted by kind in the closing
/// line (engine_body_record.rs `StoreTyped`).
pub(crate) type RunMaker<'h> = Maker<'h, Runnable, ByKind<crate::engine_body_record::StoreTyped<'h>>>;

impl<'h, 'r> Front for CommandFront<'h, '_, 'r> {
    type Start = Roots<'r>;
    type Ids = RunIds<'h>;
    type Maker = RunMaker<'h>;

    fn start(&mut self) -> Result<Roots<'r>, ParentError> {
        let names = self.names()?;
        self.watch.see(Step::Began(Stage::Roots));
        special::run_roots(names.records, names.lang_crates, self.builder.settings.panic, self.exports, names.frame, self.tables, self.alloc).map_err(ParentError::Roots)
    }

    fn ids(&mut self) -> Result<RunIds<'h>, ParentError> {
        (self.ids)()
    }

    fn maker(&mut self, start: &Roots<'r>, ids: &RunIds<'h>) -> Result<RunMaker<'h>, ParentError> {
        let names = self.names()?;
        // `run_roots` start by the rule (`MakerStart::ByRule`): what `Maker::new` did.
        let mut maker = Maker::of_roots(names, start.maker_entry(), start, ByKind::new(self.typed.over(*ids)), &self.builder, self.tables);
        maker.watch(self.make_watch);
        Ok(maker)
    }

    /// From the names records: the checker key's library item (its first 16 bytes,
    /// front2 keys.rs `checker_of`), its module's path and its name.
    fn path_of(&mut self, item: ItemKey) -> Option<Box<str>> {
        self.records_shown(item)?.path().map(Box::from)
    }

    /// By the lazy program's printer when the maker made it (its key map files every member,
    /// so a fn of an impl is named by the impl's header), else from the records alone.
    fn named(&mut self, maker: Option<&RunMaker<'h>>, tabled: ItemKey, name: &[u8]) -> crate::run_fault::Named {
        use crate::run_fault::Named;
        if let Some(shown) = maker.and_then(|maker| maker.instance_shown(tabled, name)) {
            return Named::Shown { item: shown.item, args: shown.args };
        }
        match self.records_shown(tabled) {
            Some(item) => Named::Shown { item, args: None },
            None => Named::Item(tabled),
        }
    }

    /// By the lazy program the maker made (its key map places members too); none without it.
    fn site_shown(&mut self, maker: Option<&RunMaker<'h>>, item: ItemKey, site: u32) -> Option<crate::run_fault::SiteShown> {
        let (file, line, column) = maker?.site_shown(item, site)?;
        Some(crate::run_fault::SiteShown { file: Box::from(file), line, column })
    }

    fn watch(&self) -> &dyn Watch {
        self.watch
    }
}

impl Publication for CommandFront<'_, '_, '_> {
    fn for_publication(&mut self, symbol: [u8; 32], record: &[u8], no_personality: bool, values: &mut AVec<'_, u8>) -> Result<bool, ParentError> {
        record_publication(symbol, record, no_personality, values, || self.names())
    }
}

impl Publication for crate::test_drive::TestFront<'_, '_, '_> {
    fn for_publication(&mut self, symbol: [u8; 32], record: &[u8], no_personality: bool, values: &mut AVec<'_, u8>) -> Result<bool, ParentError> {
        record_publication(symbol, record, no_personality, values, || Ok(self.names))
    }
}

fn record_publication<'h>(
    symbol: [u8; 32], record: &[u8], no_personality: bool, values: &mut AVec<'_, u8>,
    names: impl FnOnce() -> Result<NamesRecords<'h>, ParentError>,
) -> Result<bool, ParentError> {
    let view = blaze_code::record::CodeView::read(record).map_err(|why| ParentError::Decode { symbol, why })?;
    if no_personality && blaze_code::unwind::read(view).map_err(|why| ParentError::Unwind { symbol, why })?.lands() {
        return Ok(true);
    }
    if view.positions() != 0 {
        // A store hit can be the first need of the run. Resolving the coordinates from
        // today's names records needs neither a body check nor a maker.
        let records = names()?.records;
        let alloc = *values.allocator();
        position_values(&view, symbol, values, |item, site| records.source_position(item, site, alloc))?;
    }
    Ok(false)
}

fn position_values(
    view: &blaze_code::record::CodeView<'_>,
    symbol: [u8; 32],
    values: &mut AVec<'_, u8>,
    mut source: impl FnMut(ItemKey, u32) -> Result<(u32, u32), blaze_ladder::seam::PositionGap>,
) -> Result<(), ParentError> {
    let mut previous = None;
    for index in 0..view.positions() {
        let row = view.position(index);
        let site = (ItemKey(row.item), row.site);
        let answer = match previous {
            Some((last, answer)) if last == site => answer,
            _ => {
                let answer = source(site.0, site.1).map_err(|why| ParentError::Position { symbol, row, why })?;
                previous = Some((site, answer));
                answer
            }
        };
        values.extend_from_slice(&row.field.value(answer).to_le_bytes());
    }
    Ok(())
}

impl<'h, 'c, 'r> CommandFront<'h, 'c, 'r> {
    /// The checker key `item` by the one printer (front2 item_shown.rs `ItemShown`) over the
    /// names records alone: its library item (the key's first 16 bytes, front2 keys.rs
    /// `checker_of`). `None`: no library item begins so (a member's key, which only a key map
    /// files: [`Front::named`] asks the maker's program first).
    fn records_shown(&mut self, item: ItemKey) -> Option<blaze_ladder::front2::item_shown::ItemShown> {
        use blaze_ladder::front2::source::NamesSource;
        let records = self.names().ok()?.records;
        let lib = records.key_beginning(item.0.to_be_bytes())?;
        Some(blaze_ladder::front2::item_shown::ItemShown::of_lib(records, lib))
    }
}

/// HOW THE PROGRAM GETS A RECORD'S BYTES (rule 78: the one seam, each strategy tested equal on
/// the wire): one file a record named by its BLAKE3's 64 hex digits, which the launcher reads
/// ([`RecordsDir`], the command's default until the inline tests pass on a clean compile), or
/// sent inline over descriptor 3 ([`crate::run_bytes::RunBytes`]); each caller picks one at
/// its one seam line ([`crate::run_bytes::Chosen::of`]). Either way the program verifies the bytes against the id
/// before it places them (blaze-launch-rt run_mode/received.rs and `RecordSource`).
pub(crate) trait Folder {
    /// A publication (or a start-list group) begins: what was queued for the last one and
    /// never taken is dropped as not sent (its answer failed part way, so no frame carried it).
    fn begin(&mut self);
    /// `record` filed (a file already there holds equal bytes and is not written again), or
    /// queued to be sent inline once a run: its id.
    fn file(&mut self, record: &[u8]) -> Result<RecordId, ParentError>;
    /// Where `record`'s bytes are kept past this run, told after it was filed: what a replay
    /// ([`Folder::ahead`]) and a crash's read ([`Folder::read`]) find them by. The files
    /// control's file is its keeping.
    fn kept(&mut self, record: RecordId, kept: Kept, store: &mut dyn CodeStore) -> Result<(), ParentError>;
    /// A start-list record placed ahead: whether its bytes are there to place it (its file; or
    /// found through the store and queued). `code`: the code row's key; `None` for a datum,
    /// `symbol` its static's or vtable's.
    fn ahead<F: Front>(&mut self, symbol: Symbol, record: &RecordId, code: Option<CodeKey>, front: &F, store: &dyn CodeStore) -> Result<bool, ParentError>;
    /// The records queued since [`Folder::begin`], handed to the one writer, which frames them
    /// before anything that names them.
    fn take(&mut self) -> Sent<'_>;
    /// The bytes of code record `record` into `into` (a crash report reads the record the pc is
    /// in, run_fault.rs): one not kept is refused by name.
    fn read<F: Front>(&self, record: &RecordId, front: &F, store: &dyn CodeStore, into: &mut bstd::mem::Bytes<'_>) -> Result<(), ParentError>;
}

/// The files control's [`Folder`]: `<HOME>/` [`RECORDS`].
#[derive(Clone, Copy)]
pub(crate) struct RecordsDir<'r> {
    pub cx: &'r Cx,
    pub dir: &'r Dir<ReadWrite>,
}

/// A record's file name: its id's 64 hex digits.
fn file_name(id: &[u8; 32], out: &mut [u8; 64]) -> Result<Path, ParentError> {
    *out = hex(id);
    // The digits are ASCII: always UTF-8.
    let name = core::str::from_utf8(out).map_err(|_| ParentError::RecordName { record: *id })?;
    Path::new(name).map_err(ParentError::Path)
}

impl Folder for RecordsDir<'_> {
    /// Nothing is queued: a record is a file once filed.
    fn begin(&mut self) {}

    /// Its file is its keeping.
    fn kept(&mut self, _record: RecordId, _kept: Kept, _store: &mut dyn CodeStore) -> Result<(), ParentError> {
        Ok(())
    }

    /// Whether the file of `record` is there.
    fn ahead<F: Front>(&mut self, _symbol: Symbol, record: &RecordId, _code: Option<CodeKey>, _front: &F, _store: &dyn CodeStore) -> Result<bool, ParentError> {
        let mut name = [0u8; 64];
        let path = file_name(&record.0, &mut name)?;
        Ok(self.dir.stat(self.cx, &path).map_err(ParentError::Fs)?.is_some())
    }

    fn take(&mut self) -> Sent<'_> {
        Sent::NONE
    }

    /// The bytes of `record`'s file: a file not there is the folder's refusal, named.
    fn read<F: Front>(&self, record: &RecordId, _front: &F, _store: &dyn CodeStore, into: &mut bstd::mem::Bytes<'_>) -> Result<(), ParentError> {
        let mut name = [0u8; 64];
        let path = file_name(&record.0, &mut name)?;
        self.dir.read_all(self.cx, &path, into).map_err(ParentError::Fs)?;
        Ok(())
    }

    fn file(&mut self, record: &[u8]) -> Result<RecordId, ParentError> {
        let id = blaze_blake3::hash(record);
        let mut name = [0u8; 64];
        let path = file_name(&id, &mut name)?;
        if self.dir.stat(self.cx, &path).map_err(ParentError::Fs)?.is_none() {
            self.dir.publish(self.cx, &path, Content::bytes(record), Durable::No).map_err(ParentError::Fs)?;
        }
        Ok(RecordId(id))
    }
}

/// What a run's first calls cost, the closing line: `code records: reached R, found F, made M,
/// unkeyed U`. A row placed from today's start list counts as reached and found.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Tally {
    pub reached: usize,
    pub found: usize,
    pub made: usize,
    pub unkeyed: usize,
}

impl fmt::Display for Tally {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} compiled · {} cached ({} reached{})",
            blaze_report::ui::fmt_count(self.made as u64),
            blaze_report::ui::fmt_count(self.found as u64),
            blaze_report::ui::fmt_count(self.reached as u64),
            if self.unkeyed > 0 { std::format!(", {} unkeyed", self.unkeyed) } else { std::string::String::new() },
        )
    }
}

/// Whether the run built the front end (the roots and the walk: [`ensure`]'s first build), and
/// for what (rule 16: the profile says why a warm run cost one).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Built {
    /// Never built: every answer came from the start list.
    No,
    /// Built for the start's head: no list under today's certificate was placed.
    Head,
    /// Built by the first resolve, of `symbol`'s need.
    Need { symbol: [u8; 32] },
}

/// What the start list held at the run's start, as the profile says it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ListHeld {
    /// No list under the list's name.
    None,
    /// A list under today's certificate: its groups placed.
    Today,
    /// A list under another certificate: its symbols only, each resolved at its first call.
    Other,
    /// A list that did not read (an older form among them), rebuilt by this run.
    Unread(start_list::ReadError),
}

/// The start list's half of the closing count: `start list: placed P of F first answers (D
/// data, Q positioned); resolved after the list R; front end built: no` (or `yes (..)`,
/// naming why).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct StartTally {
    /// Code rows placed from today's list.
    pub placed: usize,
    /// This run's first answers with a record, the placed ones included.
    pub answered: usize,
    /// Datum rows placed from today's list.
    pub data: usize,
    /// Positioned groups placed from today's list (start_list.rs version 5), counted among
    /// `placed`.
    pub positioned: usize,
    /// Symbols asked of the resolver (each once; an unknown one too).
    pub resolved: usize,
    pub built: Built,
    pub held: ListHeld,
}

impl fmt::Display for StartTally {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "start list: placed {} of {} first answers ({} data, {} positioned); resolved after the list {}; front end built: ",
            self.placed, self.answered, self.data, self.positioned, self.resolved
        )?;
        match self.built {
            Built::No => f.write_str("no")?,
            Built::Head => f.write_str("yes (the start's head: no list under today's certificate)")?,
            Built::Need { symbol } => {
                // The symbol's first 12 hex digits.
                f.write_str("yes (first need: ")?;
                for byte in &symbol[..6] {
                    write!(f, "{byte:02x}")?;
                }
                f.write_str(")")?;
            }
        }
        match self.held {
            ListHeld::None => f.write_str("; no list held"),
            ListHeld::Today => Ok(()),
            ListHeld::Other => f.write_str("; the list held is another certificate's, its rows resolved at their first calls"),
            ListHeld::Unread(why) => write!(f, "; the list held was not read ({why}), and this run's replaces it"),
        }
    }
}

/// The run's closing count for the profile (phases.rs): the code records' line and the start
/// list's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct RunTally {
    pub codes: Tally,
    pub start: StartTally,
    pub lowerings: LoweringCounts,
    /// The record walk's own counts (body_ids.rs `Counts`): its names' longest, most items and
    /// arena bytes among them; `None` when the run never started the walk.
    pub walk: Option<body_ids::Counts>,
}

/// The body ids and the maker: made on the first row stepped.
struct Heavy<I, M> {
    ids: I,
    maker: M,
}

/// The roots and the walk over them: made on the first call the start list does not answer.
struct Started<'a, S, I, M> {
    start: S,
    walk: Walk<'a>,
    scratch: Scratch<'a>,
    /// A special record's declared rows, reused.
    list: DeclaredList<'a>,
    heavy: Option<Heavy<I, M>>,
}

/// THE LAZY PARENT (the module's doc): the one [`Resolve`].
pub(crate) struct Lazy<'r, 'a, F: Front, D: Folder> {
    cx: &'r Cx,
    front: F,
    folder: D,
    store: &'r mut dyn CodeStore,
    builder: Builder,
    alloc: Alloc<'a>,
    started: Option<Started<'a, F::Start, F::Ids, F::Maker>>,
    /// The symbols placed from today's start list, sorted, each with the list's proof that it
    /// may run (run_ids.rs `Runnable::listed`): their records are published, and the walk steps
    /// such a row (found in the store) only to reach a callee the program asks for, under that
    /// proof, never the gate again (rule 74).
    placed: AVec<'a, ([u8; 32], Runnable)>,
    /// The last record's declared symbols, handed out in [`Resolved::Record`].
    declares: AVec<'a, Symbol>,
    /// The rows stepped walking forward from the roots to reach a callee no placed row led to
    /// (answer (c)): not handed to the program yet, so each is served when the program asks.
    ahead: AVec<'a, Ahead<'a>>,
    /// The statics told (or being told in the answer at hand), by symbol, sorted: each is sent
    /// once a run, with the first record that reaches it.
    told: AVec<'a, [u8; 32]>,
    /// The last record's statics not told before, handed out in [`Resolved::Record`].
    data: AVec<'a, Datum>,
    /// The static rows of the answer at hand still to serve (a work list: a static's value may
    /// name another).
    statics: AVec<'a, usize>,
    /// The statics the answer at hand's record and data name, handed out in
    /// [`Resolved::Record`].
    named: AVec<'a, Symbol>,
    tally: Tally,
    /// Whether, and for what, the front end was built ([`ensure`]'s first build).
    built: Built,
    /// Where the code is made ([`Lazy::make_by`]; `Serial` from [`Lazy::new`]).
    codes: Codes,
    /// Under `Pooled`: the one row begun and waiting for its code ([`Lazy::asked`]).
    asked: Option<Asked>,
    /// Under `Pooled`: the rows the forward walk stepped for the need at hand, kept across
    /// its waits so the bound [`MOST_WALKED`] is the need's, not each try's.
    walked: usize,
    /// Under `Pooled`: the ready pass's work list (a row's statics, then theirs), reused.
    readying: AVec<'a, usize>,
}

/// The row begun under [`Codes::Pooled`], its proof and symbol: what [`Lazy::made`] finishes.
struct Asked {
    pending: body_ids::Pending,
    proof: Runnable,
    symbol: [u8; 32],
}

/// What one step under either [`Codes`] did: stepped, or begun and waiting for its code.
enum Took {
    Stepped(Stepped),
    Begun,
}

/// A row of the walk, by its index: the one id [`Ahead`] and [`Cleared`] name a row by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct WalkRow(usize);

use cleared::Cleared;

/// The one home of [`Cleared`]: its fields are private here, so a row and a proof are paired
/// only by its two makers, each of which derives the proof FROM the row (its fn asked of the
/// gate, or its symbol found in today's list). A proof of another body cannot be handed to
/// [`step`]: there is no way to write the pair (rule 76).
mod cleared {
    use super::{Gate, ParentError, WalkRow};
    use crate::run_ids::Runnable;
    use blaze_ladder::body_ids::Walk;

    /// A row the gate let run, with its proof (run_ids.rs `Runnable`): what [`super::step`]
    /// takes, so no row is stepped that the gate did not let.
    pub(super) struct Cleared {
        row: WalkRow,
        proof: Runnable,
    }

    impl Cleared {
        /// THE GATE for `row`, asked of `ids` now (run_ids.rs `RunIds::runnable`) for the row's
        /// own fn: its proof, or the parent's stop naming the fn and why (rule 19).
        pub(super) fn gated<G: Gate + ?Sized>(walk: &Walk<'_>, ids: &G, row: usize) -> Result<Cleared, ParentError> {
            let function = walk.tabled(row);
            let proof = ids.runnable(function).map_err(|why| ParentError::NotChecked { function, symbol: walk.symbol(row).0, why })?;
            Ok(Cleared { row: WalkRow(row), proof })
        }

        /// `row`'s proof from today's start list, when the row's own symbol was placed from it
        /// (run_ids.rs `Runnable::listed`); `None`: it was not.
        pub(super) fn placed(walk: &Walk<'_>, placed: &[([u8; 32], Runnable)], row: usize) -> Option<Cleared> {
            let symbol = walk.symbol(row).0;
            let at = placed.binary_search_by_key(&symbol, |placed| placed.0).ok()?;
            Some(Cleared { row: WalkRow(row), proof: placed[at].1 })
        }

        /// The row, and the proof it was let run by, taken apart by the one step.
        pub(super) fn into_parts(self) -> (WalkRow, Runnable) {
            (self.row, self.proof)
        }
    }
}

/// What a made row's entry is (finding 16): put under its key, or made with none.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Made {
    Put(CodeKey),
    Unkeyed(Unkeyed),
}

/// What one step answered: its entry found in the store under its key, or made (the scratch's
/// bytes are the made entry until the next step).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stepped {
    Found(CodeKey),
    Made(Made),
}

/// A row stepped ahead of its first call, as [`step`] answered it (finding 16): found, by its
/// key; or made, with its entry kept (the scratch's bytes are the next step's).
enum Ahead<'a> {
    Found { row: WalkRow, key: CodeKey },
    Made { row: WalkRow, made: Made, entry: AVec<'a, u8> },
}

impl Ahead<'_> {
    fn row(&self) -> WalkRow {
        match self {
            Ahead::Found { row, .. } | Ahead::Made { row, .. } => *row,
        }
    }
}

/// A row served at its first call ([`serve`]): found by its key, made now (its entry the
/// scratch's), or made ahead (its entry kept).
enum Served<'a> {
    Found(CodeKey),
    MadeNow(Made),
    MadeAhead(Made, AVec<'a, u8>),
}

impl<'r, 'a, F: Front, D: Folder> Lazy<'r, 'a, F, D> {
    /// Nothing of `front` is made here.
    pub(crate) fn new(cx: &'r Cx, front: F, folder: D, store: &'r mut dyn CodeStore, builder: Builder, alloc: Alloc<'a>) -> Lazy<'r, 'a, F, D> {
        Lazy {
            cx,
            front,
            folder,
            store,
            builder,
            alloc,
            started: None,
            placed: Vec::new_in(alloc),
            declares: Vec::new_in(alloc),
            ahead: Vec::new_in(alloc),
            told: Vec::new_in(alloc),
            data: Vec::new_in(alloc),
            statics: Vec::new_in(alloc),
            named: Vec::new_in(alloc),
            tally: Tally::default(),
            built: Built::No,
            codes: Codes::Serial,
            asked: None,
            walked: 0,
            readying: Vec::new_in(alloc),
        }
    }

    /// Where this parent's code is made from now on (before its first need).
    pub(crate) fn make_by(&mut self, codes: Codes) {
        self.codes = codes;
    }

    /// Under [`Codes::Pooled`]: the make the last [`Resolved::Waiting`] waits for; `None` when
    /// no row waits.
    pub(crate) fn asked(&self) -> Option<Ask<'_>> {
        let asked = self.asked.as_ref()?;
        let started = self.started.as_ref()?;
        let row = asked.pending.row();
        Some(Ask { key: asked.pending.key(), order: asked.pending.order(), name: started.walk.name(row), proof: asked.proof })
    }

    /// The code the waiting row asked for came: `entry`, made by a worker ([`body_ids::Order::make`]).
    /// The walk finishes the row (puts the entry under its key, reaches what it declares), and
    /// the row is kept to be served when its need is asked again, as a row stepped ahead is.
    pub(crate) fn made(&mut self, entry: &[u8]) -> Result<(), ParentError> {
        let Asked { pending, symbol, .. } = self.asked.take().ok_or(ParentError::NothingAsked)?;
        let started = self.started.as_mut().ok_or(ParentError::NothingAsked)?;
        let row = pending.row();
        let one = started.walk.finish::<RunError>(pending, entry, &mut *self.store, self.front.form(), MOST_WALKED, &mut started.scratch).map_err(|why| ParentError::Walk { symbol, why })?;
        let stepped = match one.stepped {
            body_ids::Stepped::Found(key) => Stepped::Found(key),
            body_ids::Stepped::Made(key) => Stepped::Made(Made::Put(key)),
            body_ids::Stepped::Unkeyed(unkeyed) => Stepped::Made(Made::Unkeyed(unkeyed)),
        };
        say_how(&self.cx.span(FIRST_CALL_SPAN), &self.front, stepped);
        let kept = match stepped {
            Stepped::Found(key) => Ahead::Found { row: WalkRow(row), key },
            Stepped::Made(made) => {
                let mut kept: AVec<'_, u8> = Vec::new_in(self.alloc);
                kept.extend_from_slice(entry);
                Ahead::Made { row: WalkRow(row), made, entry: kept }
            }
        };
        self.ahead.push(kept);
        Ok(())
    }

    /// The code the waiting row asked for was refused (`why`, the worker's): the row is given
    /// back, waiting again, and the refusal is the parent's error naming its symbol, as the
    /// serial step's would be. The refusal crossed from its code worker as a [`CodeRefusal`]
    /// (D194 piece 2), printed and sorted as the serial step's `ParentError::Walk`.
    pub(crate) fn refused(&mut self, why: CodeRefusal) -> ParentError {
        let Some(symbol) = self.asked.as_ref().map(|asked| asked.symbol) else { return ParentError::NothingAsked };
        match self.withdraw() {
            Ok(()) => ParentError::CodeRefused { symbol, why },
            Err(lost) => lost,
        }
    }

    /// The waiting row's code will not come (its ask was late, or refused to another child):
    /// the row is given back, waiting again, so the walk holds no row begun.
    pub(crate) fn withdraw(&mut self) -> Result<(), ParentError> {
        let Asked { pending, symbol, .. } = self.asked.take().ok_or(ParentError::NothingAsked)?;
        let started = self.started.as_mut().ok_or(ParentError::NothingAsked)?;
        started.walk.abandon::<RunError>(pending).map_err(|why| ParentError::Walk { symbol, why })
    }

    /// The roots this parent's walk started from, once it started: what a worker's maker is
    /// made of (D194: the same maker the serial step would use, `Front::maker`).
    pub(crate) fn start(&self) -> Option<&F::Start> {
        self.started.as_ref().map(|started| &started.start)
    }

    /// The start's head from the roots (made now if they are not yet).
    pub(crate) fn head(&mut self) -> Result<StartHead, ParentError> {
        if self.started.is_none() && self.built == Built::No {
            self.built = Built::Head;
        }
        Ok(ensure(&mut self.started, &mut self.front, self.alloc)?.start.head())
    }

    /// Whether, and for what, the front end was built.
    pub(crate) fn built(&self) -> Built {
        self.built
    }

    /// Whether nothing of the front end has been made: no roots, no walk, no body ids, no maker.
    pub(crate) fn is_cold(&self) -> bool {
        self.started.is_none()
    }

    /// Whether the body ids and the maker have been made.
    pub(crate) fn is_heavy(&self) -> bool {
        self.started.as_ref().is_some_and(|started| started.heavy.is_some())
    }

    pub(crate) fn tally(&self) -> Tally {
        self.tally
    }

    /// The record walk's counts (its names' sizes among them), once it started.
    pub(crate) fn walk_counts(&self) -> Option<body_ids::Counts> {
        self.started.as_ref().map(|started| started.walk.counts())
    }

    pub(crate) fn lowering_counts<'h>(&self) -> LoweringCounts
    where
        F: Front<Maker = RunMaker<'h>>,
    {
        self.started.as_ref().and_then(|s| s.heavy.as_ref()).map_or_else(LoweringCounts::default, |heavy| heavy.maker.lowering_counts())
    }

    /// The bytes of code record `record` into `into`, from where the folder keeps them (a crash
    /// report reads the record the pc is in, run_fault.rs).
    pub(crate) fn read_record(&self, record: &RecordId, into: &mut bstd::mem::Bytes<'_>) -> Result<(), ParentError> {
        self.folder.read(record, &self.front, &*self.store, into)
    }

    /// The walked instance for `symbol` (when it is a walked fn, not a special record) and every
    /// static row reached while serving its last publication: `(symbol, tabled_item, name, key)`.
    pub(crate) fn walked_entered(&self, symbol: &Symbol, mut emit: impl FnMut(Symbol, ItemKey, &[u8], Option<CodeKey>)) {
        let Some(started) = self.started.as_ref() else { return };
        if let Some(row) = started.walk.row_of(&CodeSymbol(symbol.0)) {
            emit(*symbol, started.walk.tabled(row), started.walk.name(row), started.walk.key(row));
        }
        for &at in &self.statics {
            emit(
                Symbol(started.walk.symbol(at).0),
                started.walk.tabled(at),
                started.walk.name(at),
                started.walk.key(at),
            );
        }
    }

    /// What names `symbol` in a crash report (run_fault.rs): the walk's row for it is the
    /// instance's tabled item, and the names records give its path. A symbol with no row this
    /// run is a special record (the C entry shim, the thread-local getter or ender) or a row
    /// placed from today's start list and never stepped.
    pub(crate) fn name_of(&mut self, symbol: [u8; 32]) -> crate::run_fault::Named {
        use crate::run_fault::Named;
        let Some(started) = self.started.as_ref() else { return Named::NoRow };
        let Some(row) = started.walk.row_of(&CodeSymbol(symbol)) else { return Named::NoRow };
        let maker = started.heavy.as_ref().map(|heavy| &heavy.maker);
        self.front.named(maker, started.walk.tabled(row), started.walk.name(row))
    }

    /// Byte `site` of item `item`'s text for a crash's words (run_fault.rs: a trap row's
    /// construct), by the front and the walk's maker; `None` when neither places it.
    pub(crate) fn site_of(&mut self, item: ItemKey, site: u32) -> Option<crate::run_fault::SiteShown> {
        let maker = self.started.as_ref()?.heavy.as_ref().map(|heavy| &heavy.maker);
        self.front.site_shown(maker, item, site)
    }

    /// `groups` placed ahead from today's start list: each code row's symbol among the placed
    /// with the list's proof (the walk steps such a row only to reach a callee), each datum's
    /// among the statics told (a later record reaching it sends it again never), each row
    /// reached and found.
    fn placed(&mut self, groups: &[TodayGroup<'_>]) {
        let mut rows = 0usize;
        for group in groups {
            self.placed.push((group.code().symbol.0, Runnable::listed(group)));
            rows += 1;
            for datum in group.view().data() {
                if let Err(at) = self.told.binary_search(&datum.symbol.0) {
                    self.told.insert(at, datum.symbol.0);
                }
                rows += 1;
            }
        }
        self.placed.sort_unstable_by_key(|placed| placed.0);
        self.placed.dedup_by_key(|placed| placed.0);
        self.tally.reached += rows;
        self.tally.found += rows;
        self.front.watch().see(Step::Placed { rows: u32::try_from(rows).unwrap_or(u32::MAX) });
    }
}

/// The walk names a crash's places while it is alive (own_process.rs builds the report before
/// the walk is dropped).
impl<F: Front, D: Folder> crate::run_fault::Places for Lazy<'_, '_, F, D> {
    fn named(&mut self, symbol: [u8; 32]) -> crate::run_fault::Named {
        self.name_of(symbol)
    }

    fn site(&mut self, item: ItemKey, at: u32) -> Option<crate::run_fault::SiteShown> {
        self.site_of(item, at)
    }

    /// The item itself, no instance (a trap row's subject enum): `Front::named` with no name,
    /// which the maker's `instance_shown` reads as the bare item.
    fn item(&mut self, item: ItemKey) -> crate::run_fault::Named {
        let maker = self.started.as_ref().and_then(|started| started.heavy.as_ref()).map(|heavy| &heavy.maker);
        self.front.named(maker, item, &[])
    }
}

/// The roots and the walk, made on the first call (each root rooted, nothing stepped).
fn ensure<'s, 'a, F: Front>(slot: &'s mut Option<Started<'a, F::Start, F::Ids, F::Maker>>, front: &mut F, alloc: Alloc<'a>) -> Result<&'s mut Started<'a, F::Start, F::Ids, F::Maker>, ParentError> {
    let started = match slot.take() {
        Some(started) => started,
        None => {
            let start = front.start()?;
            let mut walk = Walk::new_in(alloc);
            for i in 0..start.root_count() {
                walk.root::<RunError>(start.root_row(i), MOST_WALKED).map_err(|why| ParentError::Walk { symbol: start.head().line.symbol.0, why })?;
            }
            Started { start, walk, scratch: Scratch::new_in(alloc), list: DeclaredList::new_in(alloc), heavy: None }
        }
    };
    Ok(slot.insert(started))
}

/// The body ids and the maker, made on the first row stepped.
fn heavy<'h, F: Front>(front: &mut F, slot: &'h mut Option<Heavy<F::Ids, F::Maker>>, start: &F::Start) -> Result<&'h mut Heavy<F::Ids, F::Maker>, ParentError> {
    let heavy = match slot.take() {
        Some(heavy) => heavy,
        None => {
            let ids = front.ids()?;
            let maker = front.maker(start, &ids)?;
            Heavy { ids, maker }
        }
    };
    Ok(slot.insert(heavy))
}

/// THE GATE for `row`, asked now (run_ids.rs `RunIds::runnable`): its fn's proof that it may
/// run, or the parent's stop naming the fn and why (rule 19).
fn clear<F: Front>(front: &mut F, started: &mut Started<'_, F::Start, F::Ids, F::Maker>, row: usize) -> Result<Cleared, ParentError> {
    let heavy = heavy(front, &mut started.heavy, &started.start)?;
    Cleared::gated(&started.walk, &heavy.ids, row)
}

/// ONE step of the walk for the row `cleared` names, under its proof: the body ids and the
/// maker made if they are not yet, then, under [`Codes::Serial`], `Walk::one` in its span, whose
/// event says how it was answered (the only caller of `Walk::one`); under [`Codes::Pooled`],
/// `Walk::begin`: a row found is answered the same, and a row to be made is kept in `asked`
/// for a worker ([`Took::Begun`]), its event said when its code comes ([`Lazy::made`]).
#[expect(clippy::too_many_arguments, reason = "the resolver's parts, borrowed apart")]
fn step<F: Front>(
    cx: &Cx,
    front: &mut F,
    started: &mut Started<'_, F::Start, F::Ids, F::Maker>,
    store: &mut dyn CodeStore,
    builder: &Builder,
    codes: Codes,
    asked: &mut Option<Asked>,
    cleared: Cleared,
) -> Result<Took, ParentError> {
    // The proof was derived from this row by its maker (`Cleared::gated`, `Cleared::placed`):
    // it is this row's body's, by construction, and nothing here compares it again (rule 74).
    // The make takes it (D163): no instance is made without the gate's proof in hand.
    let (WalkRow(row), proof) = cleared.into_parts();
    let symbol = started.walk.symbol(row).0;
    let tabled = started.walk.tabled(row);
    let heavy = heavy(front, &mut started.heavy, &started.start)?;
    let path = if front.watch().paths() { front.path_of(tabled) } else { None };
    front.watch().see(Step::Resolving { path: path.as_deref() });
    let cx = cx.span(FIRST_CALL_SPAN);
    let one = match codes {
        Codes::Serial => {
            started
                .walk
                .one(row, &heavy.ids, store, builder, &builder.producer, &mut heavy.maker, &proof, front.form(), MOST_WALKED, &mut started.scratch)
                .map_err(|why| ParentError::Walk { symbol, why })?
                .stepped
        }
        Codes::Pooled => {
            if let Some(waiting) = asked.as_ref() {
                return Err(ParentError::TwoAsked { symbol, asked: waiting.symbol });
            }
            match started.walk.begin(row, &heavy.ids, store, builder, &builder.producer, &mut heavy.maker, front.form(), MOST_WALKED, &mut started.scratch).map_err(|why| ParentError::Walk { symbol, why })? {
                body_ids::Begun::Stepped(one) => one.stepped,
                body_ids::Begun::Make(pending) => {
                    *asked = Some(Asked { pending, proof, symbol });
                    return Ok(Took::Begun);
                }
            }
        }
    };
    // Total: a step's answer carries its key in the variant (body_ids.rs `Stepped`), and is
    // never waiting.
    let stepped = match one {
        body_ids::Stepped::Found(key) => Stepped::Found(key),
        body_ids::Stepped::Made(key) => Stepped::Made(Made::Put(key)),
        body_ids::Stepped::Unkeyed(unkeyed) => Stepped::Made(Made::Unkeyed(unkeyed)),
    };
    say_how(&cx, front, stepped);
    Ok(Took::Stepped(stepped))
}

/// How a row was answered, as an event of `span` (its first call's, [`FIRST_CALL_SPAN`]) and
/// to the watch: the one place both [`Codes`] say it.
fn say_how<F: Front>(span: &Cx, front: &F, stepped: Stepped) {
    span.event(match stepped {
        Stepped::Found(_) => HOW_FOUND,
        Stepped::Made(Made::Put(_)) => HOW_MADE,
        Stepped::Made(Made::Unkeyed(Unkeyed::NoRecord)) => HOW_NO_RECORD,
        Stepped::Made(Made::Unkeyed(Unkeyed::Stale { .. })) => HOW_STALE,
    });
    front.watch().see(Step::Answered(match stepped {
        Stepped::Found(_) => Answered::Found,
        Stepped::Made(Made::Put(_)) => Answered::Made,
        Stepped::Made(Made::Unkeyed(_)) => Answered::Unkeyed,
    }));
}

/// The row `row` as [`step`] answered it, kept to be served at its own first call ([`Ahead`]):
/// a made entry copied out of the scratch (the next step's).
fn ahead_of_step<'a>(row: usize, stepped: Stepped, scratch: &Scratch<'_>, alloc: Alloc<'a>) -> Ahead<'a> {
    match stepped {
        Stepped::Found(key) => Ahead::Found { row: WalkRow(row), key },
        Stepped::Made(made) => {
            let mut entry: AVec<'_, u8> = Vec::new_in(alloc);
            entry.extend_from_slice(scratch.made());
            Ahead::Made { row: WalkRow(row), made, entry }
        }
    }
}

/// Row `row` served once: gated and stepped now if it is waiting (under [`Codes::Serial`]; under
/// `Pooled` the ready pass stepped it before, so a waiting row here is [`ParentError::NotReady`]),
/// else taken from what its step ahead answered (`twice`: the refusal when it is neither, a row
/// served already).
#[expect(clippy::too_many_arguments, reason = "the resolver's parts, borrowed apart")]
fn serve<'a, F: Front>(
    cx: &Cx,
    front: &mut F,
    started: &mut Started<'a, F::Start, F::Ids, F::Maker>,
    store: &mut dyn CodeStore,
    builder: &Builder,
    codes: Codes,
    ahead: &mut AVec<'a, Ahead<'a>>,
    row: usize,
    twice: ParentError,
) -> Result<Served<'a>, ParentError> {
    if started.walk.how(row) == How::Waiting {
        if codes == Codes::Pooled {
            return Err(ParentError::NotReady { symbol: started.walk.symbol(row).0 });
        }
        let cleared = clear(front, started, row)?;
        return Ok(match step(cx, front, started, store, builder, Codes::Serial, &mut None, cleared)? {
            Took::Stepped(Stepped::Found(key)) => Served::Found(key),
            Took::Stepped(Stepped::Made(made)) => Served::MadeNow(made),
            // `Serial` makes inside the step: it never begins a row.
            Took::Begun => return Err(ParentError::NotReady { symbol: started.walk.symbol(row).0 }),
        });
    }
    let Some(i) = ahead.iter().position(|one| one.row() == WalkRow(row)) else { return Err(twice) };
    Ok(match ahead.swap_remove(i) {
        Ahead::Found { key, .. } => Served::Found(key),
        Ahead::Made { made, entry, .. } => Served::MadeAhead(made, entry),
    })
}

/// THE READY PASS, under [`Codes::Pooled`], before `row` is served: the row, then the statics
/// its answer will tell (the statics loop's own closure: each declared static or thread-local
/// not in `told`, then each of theirs), stepped in that order if waiting, each kept in `ahead`,
/// so the serve after it makes nothing on the command thread. `told` is only read: the serve
/// tells them. `false`: a row was begun and waits for its code ([`Lazy::asked`]).
#[expect(clippy::too_many_arguments, reason = "the resolver's parts, borrowed apart")]
fn ready<'a, F: Front>(
    cx: &Cx,
    front: &mut F,
    started: &mut Started<'a, F::Start, F::Ids, F::Maker>,
    store: &mut dyn CodeStore,
    builder: &Builder,
    asked: &mut Option<Asked>,
    ahead: &mut AVec<'a, Ahead<'a>>,
    told: &[[u8; 32]],
    readying: &mut AVec<'_, usize>,
    row: usize,
    alloc: Alloc<'a>,
) -> Result<bool, ParentError> {
    readying.clear();
    readying.push(row);
    let mut next = 0;
    while next < readying.len() {
        let at = readying[next];
        next += 1;
        if started.walk.how(at) == How::Waiting {
            let cleared = clear(front, started, at)?;
            match step(cx, front, started, store, builder, Codes::Pooled, asked, cleared)? {
                Took::Stepped(stepped) => ahead.push(ahead_of_step(at, stepped, &started.scratch, alloc)),
                Took::Begun => return Ok(false),
            }
        }
        // Done now (stepped here, kept ahead, or served before): its declared rows are read.
        for &callee in started.walk.declared(at) {
            let callee = callee as usize;
            if matches!(started.walk.kind(callee), RowKind::Static | RowKind::ThreadLocal)
                && told.binary_search(&started.walk.symbol(callee).0).is_err()
                && !readying.contains(&callee)
            {
                readying.push(callee);
            }
        }
    }
    Ok(true)
}

/// The entry `served` answered: the store's under its key, the scratch's (made now), or the one
/// kept (made ahead). `symbol` names a refusal.
fn entry_of<'e>(served: &'e Served<'_>, store: &'e dyn CodeStore, scratch: &'e Scratch<'_>, symbol: [u8; 32]) -> Result<&'e [u8], ParentError> {
    match served {
        Served::Found(key) => store.get(*key).map_err(|why| ParentError::Store { symbol, why })?.ok_or(ParentError::NotStored { symbol }),
        // The made entry; an unkeyed one is not put, so these bytes are its only copy.
        Served::MadeNow(_) => Ok(scratch.made()),
        Served::MadeAhead(_, entry) => Ok(entry.as_slice()),
    }
}

/// The rows `row`'s entry declares, split by kind: an instance's symbol onto `declares` (its
/// slot is told before the record is published); a static not told yet onto `statics`, and
/// into `told` at once (so a cycle of statics is one batch, each once).
fn callees(walk: &Walk<'_>, row: usize, declares: &mut AVec<'_, Symbol>, told: &mut AVec<'_, [u8; 32]>, statics: &mut AVec<'_, usize>) {
    for &callee in walk.declared(row) {
        let callee = callee as usize;
        let symbol = walk.symbol(callee).0;
        match walk.kind(callee) {
            RowKind::Instance => declares.push(Symbol(symbol)),
            // A thread-local's row is its template's datum, sent as a static's is.
            RowKind::Static | RowKind::ThreadLocal => {
                if let Err(at) = told.binary_search(&symbol) {
                    told.insert(at, symbol);
                    statics.push(callee);
                }
            }
        }
    }
}

fn count(tally: &mut Tally, served: &Served<'_>) {
    tally.reached += 1;
    match served {
        Served::Found(_) => tally.found += 1,
        Served::MadeNow(Made::Put(_)) | Served::MadeAhead(Made::Put(_), _) => tally.made += 1,
        Served::MadeNow(Made::Unkeyed(_)) | Served::MadeAhead(Made::Unkeyed(_), _) => tally.unkeyed += 1,
    }
}

/// The key of `served`'s row in the next start list: its key, or, for an unkeyed instance, its
/// entry's content key.
fn listed_key(served: &Served<'_>, entry: &[u8]) -> CodeKey {
    match served {
        Served::Found(key) | Served::MadeNow(Made::Put(key)) | Served::MadeAhead(Made::Put(key), _) => *key,
        Served::MadeNow(Made::Unkeyed(_)) | Served::MadeAhead(Made::Unkeyed(_), _) => content_key(entry),
    }
}

/// The key `served`'s entry is stored under (found, or made and put: its content key); `None`
/// for an unkeyed instance's, never put. What a datum it carries is kept by ([`Folder::kept`]).
fn stored(served: &Served<'_>) -> Option<CodeKey> {
    match served {
        Served::Found(key) | Served::MadeNow(Made::Put(key)) | Served::MadeAhead(Made::Put(key), _) => Some(*key),
        Served::MadeNow(Made::Unkeyed(_)) | Served::MadeAhead(Made::Unkeyed(_), _) => None,
    }
}

impl<F: Front, D: Folder> Resolve for Lazy<'_, '_, F, D> {
    #[cfg(test)]
    fn resolve(&mut self, symbol: &Symbol) -> Result<Resolved<'_>, ParentError> {
        let mut values = Vec::new_in(self.alloc);
        let answer = self.publication(symbol, &mut values)?;
        assert!(values.is_empty(), "the position-free model expected a plain publication");
        Ok(answer)
    }

    fn publication(&mut self, symbol: &Symbol, values: &mut AVec<'_, u8>) -> Result<Resolved<'_>, ParentError> {
        let Lazy { cx, front, folder, store, builder, alloc, started, placed, declares, ahead, told, data, statics, named, tally, built, codes, asked, walked, readying } = self;
        let codes = *codes;
        // What the last answer queued and the one writer did not take is not on the wire.
        folder.begin();
        // A row waits for its code: nothing more is stepped until it comes (one make at a time).
        if asked.is_some() {
            return Ok(Resolved::Waiting);
        }
        // The forward walk's bound is the need's: under `Pooled` it is kept across the need's
        // waits, and set back once the need is reached or unknown.
        if codes == Codes::Serial {
            *walked = 0;
        }
        // The profile's why (rule 16): the first need that built the front end, by its symbol.
        if started.is_none() && *built == Built::No {
            *built = Built::Need { symbol: symbol.0 };
        }
        let started = ensure(started, front, *alloc)?;
        let builder = *builder;
        let code = CodeSymbol(symbol.0);
        declares.clear();
        data.clear();
        statics.clear();
        named.clear();
        let mut row = started.walk.row_of(&code);
        // A callee of a record placed from today's list: the placed rows on the way are stepped
        // (found in the store, their files already there) until the walk reaches it; no row the
        // program has not run is stepped.
        let mut at = 0;
        while row.is_none() && at < started.walk.len() {
            if started.walk.how(at) == How::Waiting
                && let Some(cleared) = Cleared::placed(&started.walk, placed.as_slice(), at)
            {
                if let Took::Begun = step(cx, front, started, &mut **store, &builder, codes, asked, cleared)? {
                    return Ok(Resolved::Waiting);
                }
                row = started.walk.row_of(&code);
            }
            at += 1;
        }
        // No instance the walk has reached: a special record of the run (the shim, the getter,
        // the ender), asked first, as it is never a row.
        if row.is_none()
            && let Some(made) = started.start.special(&code, &builder, &mut **store, *alloc)?
        {
            cx.span(FIRST_CALL_SPAN).event(HOW_SPECIAL);
            let entry = store.get(made.key).map_err(|why| ParentError::Store { symbol: symbol.0, why })?.ok_or(ParentError::NotStored { symbol: symbol.0 })?;
            let record = front.record_of(entry).map_err(|why| ParentError::Entry { symbol: symbol.0, why })?;
            prepare_publication(&mut *front, started.start.head().personality, None, symbol.0, record, values)?;
            let id = folder.file(record)?;
            started.list.clear();
            front.form().declared(entry, &mut started.list).map_err(|why| ParentError::Entry { symbol: symbol.0, why })?;
            if !started.list.statics().is_empty() || !started.list.thread_locals().is_empty() {
                return Err(ParentError::SpecialStatic { symbol: symbol.0 });
            }
            front.statics_named(entry, named).map_err(|why| ParentError::Entry { symbol: symbol.0, why })?;
            named.sort_unstable();
            named.dedup();
            for i in 0..started.list.len() {
                let callee = started.walk.root::<RunError>(started.list.row(i), MOST_WALKED).map_err(|why| ParentError::Walk { symbol: symbol.0, why })?;
                declares.push(Symbol(started.walk.symbol(callee).0));
            }
            tally.reached += 1;
            match made.found {
                true => tally.found += 1,
                false => tally.made += 1,
            }
            front.watch().see(Step::Answered(if made.found { Answered::Found } else { Answered::Made }));
            folder.kept(id, Kept::Code(made.key), &mut **store)?;
            return Ok(Resolved::Record { key: made.key, record: id, declares: declares.as_slice(), data: &[], named: named.as_slice(), sent: folder.take() });
        }
        // Still unreached (the start list's cap broke the chain to it): walk forward from the
        // roots, stepping each row still waiting in walk order, at most `MOST_WALKED`, until the
        // walk declares it. Each is kept to be served at its own first call ([`Ahead`]).
        // `Unknown` is left for a symbol no record declares.
        let mut at = 0;
        while row.is_none() && at < started.walk.len() {
            if started.walk.how(at) == How::Waiting {
                if *walked == MOST_WALKED {
                    return Err(ParentError::TooFar { symbol: symbol.0, most: MOST_WALKED });
                }
                *walked += 1;
                let cleared = clear(front, started, at)?;
                match step(cx, front, started, &mut **store, &builder, codes, asked, cleared)? {
                    Took::Stepped(stepped) => ahead.push(ahead_of_step(at, stepped, &started.scratch, *alloc)),
                    // Its code comes later ([`Lazy::made`] keeps it ahead); the walk goes on then.
                    Took::Begun => return Ok(Resolved::Waiting),
                }
                row = started.walk.row_of(&code);
            }
            at += 1;
        }
        *walked = 0;
        let Some(row) = row else { return Ok(Resolved::Unknown) };
        // A static or a thread-local is no call target: its symbol is never a need (the child
        // binds its word).
        if matches!(started.walk.kind(row), RowKind::Static | RowKind::ThreadLocal) {
            return Ok(Resolved::Unknown);
        }
        // Under `Pooled`, nothing is made below: the row and the statics its answer tells are
        // stepped first, each kept ahead, and a row begun waits for its code.
        if codes == Codes::Pooled && !ready(cx, front, started, &mut **store, &builder, asked, ahead, told.as_slice(), readying, row, *alloc)? {
            return Ok(Resolved::Waiting);
        }
        // A row stepped ahead is served once, from what its step answered; any other row that is
        // not waiting was served already.
        let served = serve(cx, front, started, &mut **store, &builder, codes, ahead, row, ParentError::ResolvedTwice { symbol: symbol.0 })?;
        let entry = entry_of(&served, &**store, &started.scratch, symbol.0)?;
        let record = front.record_of(entry).map_err(|why| ParentError::Entry { symbol: symbol.0, why })?;
        prepare_publication(&mut *front, started.start.head().personality, Some(started.walk.tabled(row)), symbol.0, record, values)?;
        let id = folder.file(record)?;
        // An unkeyed instance's row in the next list: its entry's content key.
        let key = listed_key(&served, entry);
        front.statics_named(entry, named).map_err(|why| ParentError::Entry { symbol: symbol.0, why })?;
        callees(&started.walk, row, declares, told, statics);
        // The vtables the record names: each a datum the entry carries, filed and sent in this
        // record's `Data` unless this run told it already (its method words' instances are
        // among the entry's declared rows, so their slots go in the `Declare` before it).
        let vtables = data.len();
        front.each_vtable(symbol.0, entry, &mut |vtable, datum| {
            if let Err(at) = told.binary_search(&vtable.0) {
                told.insert(at, vtable.0);
                data.push(Datum { symbol: vtable, record: folder.file(datum)? });
            }
            Ok(())
        })?;
        // Where the record and its vtables' data are kept: the entry, when it was put.
        let under = stored(&served);
        folder.kept(id, Kept::Code(key), &mut **store)?;
        for datum in &data[vtables..] {
            folder.kept(datum.record, under.map_or(Kept::Unkept, |under| Kept::Vtable { under }), &mut **store)?;
        }
        count(tally, &served);
        // The statics the record reaches and this run has not told, each served once and its
        // datum filed; their own callees after them (a static's value names statics, and fns
        // from step 2 on: those are declared like the record's own).
        let mut next = 0;
        while next < statics.len() {
            let at = statics[next];
            next += 1;
            let static_ = started.walk.symbol(at).0;
            let served = serve(cx, front, started, &mut **store, &builder, codes, ahead, at, ParentError::StaticServed { symbol: static_ })?;
            let entry = entry_of(&served, &**store, &started.scratch, static_)?;
            let datum = front.record_of(entry).map_err(|why| ParentError::Entry { symbol: static_, why })?;
            let filed = folder.file(datum)?;
            data.push(Datum { symbol: Symbol(static_), record: filed });
            front.statics_named(entry, named).map_err(|why| ParentError::Entry { symbol: static_, why })?;
            callees(&started.walk, at, declares, told, statics);
            folder.kept(filed, stored(&served).map_or(Kept::Unkept, |under| Kept::Static { under }), &mut **store)?;
            count(tally, &served);
        }
        named.sort_unstable();
        named.dedup();
        Ok(Resolved::Record { key, record: id, declares: declares.as_slice(), data: data.as_slice(), named: named.as_slice(), sent: folder.take() })
    }

    fn watch(&self) -> &dyn Watch {
        self.front.watch()
    }
}

/// Before the spawn, onto `out`: the personality routine named ([`Needs::name_personality`],
/// first, so no `Publish` precedes it), then today's start list (`today`, the rows under
/// today's certificate) placed. A first answer's group (start_list.rs: its data, the statics
/// it names, its code row) is kept WHOLE or left out whole: out when the folder does not hold
/// its record's bytes or one of its data's ([`Folder::ahead`]: a file, or an entry the store
/// holds, then sent inline before the frames), or when it names a static that neither a
/// group kept before it nor its own data send (a group left out takes its data with it, so
/// a later namer of those goes too); the thread-local descriptor is no datum, and the one
/// writer tells it before the first record naming it. Whatever is left out is resolved at its
/// first call like any other (rule 3: nothing is published that is not there). The start's
/// head: the list's when there is one (nothing of the front end is made), else the roots'.
/// A positioned group (start_list.rs version 5) is kept with its values rows, which
/// `Needs::place_ahead` sends in its `Positioned`: the list is today's, so they are today's
/// positions and this replay needs no source lookup. A group left out for a file the folder
/// does not hold still takes its data with it, which is why a later group's names are checked
/// here. `kept` holds the groups placed (each a [`TodayGroup`]: the list is today's, so each
/// group's row runs under the list's proof, run_ids.rs `Runnable::listed`).
pub(crate) fn ahead_of<'b, F: Front, D: Folder, A: Allocator>(
    today: Option<TodayRows<'b>>,
    lazy: &mut Lazy<'_, '_, F, D>,
    needs: &mut Needs<'_>,
    kept: &mut AVec<'_, TodayGroup<'b>>,
    out: &mut std::vec::Vec<u8, A>,
) -> Result<StartHead, ParentError> {
    let Some(today) = today else {
        let head = lazy.head()?;
        needs.name_personality(head.personality, out)?;
        return Ok(head);
    };
    let head = today.head();
    needs.name_personality(head.personality, out)?;
    let descriptor = blaze_ladder::symbols::tls_desc_symbol().0;
    // The statics the groups kept so far send, sorted.
    let mut on: AVec<'_, [u8; 32]> = Vec::new_in(lazy.alloc);
    for group in today.groups() {
        let view = group.view();
        // Its names first: a group that does not stand is left out before any of its bytes
        // is looked for.
        let stands = |symbol: &Symbol| symbol.0 == descriptor || on.binary_search(&symbol.0).is_ok() || view.data().any(|datum| datum.symbol == *symbol);
        if !view.named().all(|symbol| stands(&symbol)) {
            continue;
        }
        // A positioned group's values are today's (the certificate is): nothing to hold. Each
        // record's bytes are looked for through the folder (its file; or the store, queued to
        // be sent inline), the code row by its key.
        let Lazy { folder, front, store, .. } = &mut *lazy;
        folder.begin();
        let mut whole = true;
        for datum in view.data() {
            whole = whole && folder.ahead(datum.symbol, &datum.record, None, &*front, &**store)?;
        }
        let code = group.code();
        whole = whole && folder.ahead(code.symbol, &code.record, Some(code.key), &*front, &**store)?;
        if !whole {
            // What this group queued is dropped as not sent.
            folder.begin();
            continue;
        }
        // The group's bytes on the wire before any frame names them (the one writer's).
        needs.send(folder.take(), out)?;
        for datum in view.data() {
            if let Err(at) = on.binary_search(&datum.symbol.0) {
                on.insert(at, datum.symbol.0);
            }
        }
        kept.push(group);
    }
    needs.place_ahead(kept.as_slice(), out)?;
    lazy.placed(kept.as_slice());
    Ok(head)
}

/// What a symbol is to this run once met (finding 9); a symbol not in [`Needs::met`] is met
/// by no need and no record: neither declared nor asked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Met {
    /// Declared through `Slot` (a record's target, or the personality routine) and not
    /// answered yet: a need of it resolves it.
    Declared(Slot),
    /// Resolved, or placed from today's start list: published through `slot`.
    Published { slot: Slot, record: RecordId },
    /// Resolved to nothing: every need of it is `Failed`. `slot`: its slot when one was
    /// declared (a record's target that no record answers), none when only a need named it.
    Unknown { slot: Option<Slot> },
}

impl Met {
    fn slot(self) -> Option<Slot> {
        match self {
            Met::Declared(slot) | Met::Published { slot, .. } => Some(slot),
            Met::Unknown { slot } => slot,
        }
    }
}

/// The parent's slot bookkeeping (the module's doc): every symbol met, the slots numbered, the
/// pairs not yet told, and the first-answer order the next start list is.
pub(crate) struct Needs<'a> {
    /// Each symbol met, by its bytes; at most [`MOST_SLOTS`] (rule 18, checked at each insert).
    met: ByHash<[u8; 32], Met, Alloc<'a>>,
    /// Each declared slot's symbol, by slot: slots are dense, in the order they were numbered.
    slots: AVec<'a, Symbol>,
    /// Pairs numbered and not yet told: one `Declare` before the next `Publish`.
    pairs: AVec<'a, Declared>,
    /// The first answers with a record, in order, up to the start list's cap (the window).
    first: start_list::Rows<'a>,
    /// The statics on the child (each in a `Data` sent), by symbol, sorted.
    sent: AVec<'a, [u8; 32]>,
    /// The records published, by id, sorted: every static each names was on the child before
    /// it, so publishing one again needs nothing more.
    cleared: AVec<'a, [u8; 32]>,
    /// The current run's values for records carrying source positions, kept until its last
    /// need so a repeat publication sends the same values. These groups cannot be replayed.
    positioned: ByHash<[u8; 32], AVec<'a, u8>, Alloc<'a>>,
    /// The data of the `Data` at hand (those not sent before), reused.
    batch: AVec<'a, Datum>,
    /// The next message's id.
    next_id: u64,
    /// A `Publish` (or `Positioned`) was written: no program-defined C name may be declared
    /// after it ([`Needs::declare_c_defined`]).
    published: bool,
    /// The start list's window closed: a first answer's group did not fit under its cap, so no
    /// later answer (which may stand on that group's statics) is kept.
    closed: bool,
    /// A placed group's data and names, gathered from its rows ([`Needs::place_ahead`]), reused.
    group_data: AVec<'a, Datum>,
    group_named: AVec<'a, Symbol>,
    /// The placed groups' slots, in the list's order ([`Needs::place_ahead`]), reused.
    group_slots: AVec<'a, Slot>,
    alloc: Alloc<'a>,
    /// The start list's count ([`StartTally`]): code rows and datum rows placed, first answers,
    /// symbols asked of the resolver.
    placed: usize,
    placed_data: usize,
    /// Positioned groups placed from today's list, their values the list's.
    placed_positioned: usize,
    answered: usize,
    resolved: usize,
    /// Where the program struck, as its crash handler told it (`Up::Fault`): the first one
    /// read, said by name once the program has ended (run_fault.rs).
    fault: Option<blaze_launch::wire::fault::Fault>,
}

/// What a `Publish` stands on ([`Needs::publish`]): how the statics its record names are on the
/// child before it.
#[derive(Clone, Copy)]
enum Stands<'n> {
    /// A record answered now, or a start-list group placed (its rows hold the same two lists,
    /// as the run that wrote it sent them): `data` (its datums and theirs, each sent in one
    /// `Data` before it unless sent already) and `named`, every static the record and those
    /// datums name: each must be on the child once `data` is, or the publish is refused by name.
    Answered { named: &'n [Symbol], data: &'n [Datum] },
    /// A record published before in this run: what it names went then.
    Again,
}

impl<'a> Needs<'a> {
    pub(crate) fn new(alloc: Alloc<'a>) -> Needs<'a> {
        Needs {
            met: ByHash::new_in(alloc),
            slots: Vec::new_in(alloc),
            pairs: Vec::new_in(alloc),
            first: start_list::Rows::new_in(alloc),
            sent: Vec::new_in(alloc),
            cleared: Vec::new_in(alloc),
            positioned: ByHash::new_in(alloc),
            batch: Vec::new_in(alloc),
            next_id: 0,
            published: false,
            closed: false,
            group_data: Vec::new_in(alloc),
            group_named: Vec::new_in(alloc),
            group_slots: Vec::new_in(alloc),
            alloc,
            placed: 0,
            placed_data: 0,
            placed_positioned: 0,
            answered: 0,
            resolved: 0,
            fault: None,
        }
    }

    /// Where the program struck, when its crash handler told the parent (the first report).
    pub(crate) fn fault(&self) -> Option<blaze_launch::wire::fault::Fault> {
        self.fault
    }

    /// The start list's line of the closing count: what was placed, answered and asked, with
    /// `built` (the resolver's: whether the front end was built, and for what) and `held` (what
    /// the list was at the start).
    pub(crate) fn start_tally(&self, built: Built, held: ListHeld) -> StartTally {
        StartTally { placed: self.placed, answered: self.answered, data: self.placed_data, positioned: self.placed_positioned, resolved: self.resolved, built, held }
    }

    /// THE ONE WRITER OF `Publish` (rule 76; run.md, "Statics in the run"): the pairs not yet
    /// told in one `Declare`, then what the record stands on ([`Stands`]): the data not sent
    /// yet in one `Data` (a cycle of statics is one batch: the child gives every datum of a
    /// batch its room before it fills any), each static the record or a datum names checked to
    /// be on the child, and only then `slot`'s `Publish` of `record`. A static named and never
    /// sent is refused by name, before any `Publish`: the child would stop on it
    /// (run_mode.rs `StaticNotTold`).
    fn publish<A: Allocator>(&mut self, slot: Slot, record: RecordId, stands: Stands<'_>, out: &mut std::vec::Vec<u8, A>) -> Result<(), ParentError> {
        self.declare(out)?;
        match stands {
            Stands::Answered { named, data } => {
                self.batch.clear();
                for datum in data {
                    if let Err(at) = self.sent.binary_search(&datum.symbol.0) {
                        self.sent.insert(at, datum.symbol.0);
                        self.batch.push(*datum);
                    }
                }
                if !self.batch.is_empty() {
                    let id = self.id();
                    Down::Data { data: List::of(self.batch.as_slice()) }.encode(id, out)?;
                }
                // The thread-local descriptor is the child's own (its key and root are facts of
                // that process): named once, with the key's destructor, before the first record
                // naming it (wire/thread_locals.rs). Each thread-local's template went in `data`.
                let descriptor = blaze_ladder::symbols::tls_desc_symbol().0;
                if named.iter().any(|it| it.0 == descriptor)
                    && let Err(at) = self.sent.binary_search(&descriptor)
                {
                    self.sent.insert(at, descriptor);
                    let id = self.id();
                    let ender = Symbol(blaze_ladder::tls_index::tls_ender_symbol().0);
                    Down::ThreadLocals { descriptor: Symbol(descriptor), ender }.encode(id, out)?;
                }
                for static_ in named {
                    if self.sent.binary_search(&static_.0).is_err() {
                        let instance = self.slots.get(slot.0 as usize).map(|s| s.0);
                        let touches_tls = named.iter().any(|it| it.0 == descriptor);
                        return Err(ParentError::StaticUnsent { record: record.0, symbol: static_.0, instance, touches_tls });
                    }
                }
                if let Err(at) = self.cleared.binary_search(&record.0) {
                    self.cleared.insert(at, record.0);
                }
            }
            Stands::Again => {
                if self.cleared.binary_search(&record.0).is_err() {
                    return Err(ParentError::NotCleared { record: record.0 });
                }
            }
        }
        let id = self.id();
        self.published = true;
        match self.positioned.get(&record.0) {
            Some(values) => Down::Positioned { slot, tier: Tier::Zero, record, values: values.as_slice() },
            None => Down::Publish { slot, tier: Tier::Zero, record },
        }.encode(id, out)?;
        Ok(())
    }

    /// THE ONE WRITER OF `Bytes` (run schema 6, blaze-launch wire/bytes.rs): each record
    /// `sent` carries, whole, in consecutive parts of at most `BYTES_PART_MOST`, BEFORE any
    /// frame that names it (the folder hands them over first: [`Folder::take`]). Each record in
    /// `sent` is 1 byte to `BYTES_RECORD_MOST` long (run_bytes.rs refuses any other by name), so
    /// every part has the shape the child's decode takes; the child checks the rest (the order,
    /// the sum, the digest) and refuses a breach by name.
    pub(crate) fn send<A: Allocator>(&mut self, sent: Sent<'_>, out: &mut std::vec::Vec<u8, A>) -> Result<(), ParentError> {
        for (record, bytes) in sent.each() {
            let total = bytes.len() as u64;
            let mut at = 0u64;
            for part in bytes.chunks(BYTES_PART_MOST as usize) {
                let id = self.id();
                Down::Bytes { record, at, total, part }.encode(id, out)?;
                at += part.len() as u64;
            }
        }
        Ok(())
    }

    /// THE ORDER OF C NAMES (D175): every C name the program itself defines (a C unit's global
    /// function, a Rust item exported under its C name), each by its `Symbol::of_name`, given
    /// its slot and told in one `Declare` BEFORE the first `Publish`. The child resolves a
    /// record's C name to a declared slot before its strategy (run_mode/bind.rs), so a name the
    /// program defines is never bound to the process's or the table's definition. After a
    /// `Publish` it is refused ([`ParentError::CDefinedLate`]); the child refuses the converse
    /// (a declaration of a name it already bound: run_mode.rs `DefinedAfterBound`). A C name
    /// the program defines as DATA has no slot: it goes in the first record's `Data` as any
    /// static does, under the same symbol.
    #[expect(dead_code, reason = "D175: called once the front end registers the program's own C names (the checker's writer's item), before `ahead_of`")]
    pub(crate) fn declare_c_defined<A: Allocator>(&mut self, functions: &[Symbol], out: &mut std::vec::Vec<u8, A>) -> Result<(), ParentError> {
        if let Some(first) = functions.first()
            && self.published
        {
            return Err(ParentError::CDefinedLate { symbol: first.0 });
        }
        for &symbol in functions {
            self.slot_of(symbol)?;
        }
        self.declare(out)
    }

    fn id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// This run's first answers with a record, in order, each as its group (its data sent, the
    /// statics it names, its code row; at most the start list's cap): the rows of the list
    /// written at exit.
    pub(crate) fn first(&self) -> &start_list::Rows<'a> {
        &self.first
    }

    /// `symbol` entered as `met`: a symbol not met before counts against [`MOST_SLOTS`].
    fn enter(&mut self, symbol: Symbol, met: Met) -> Result<(), ParentError> {
        if !self.met.contains_key(&symbol.0) && self.met.len() >= MOST_SLOTS as usize {
            return Err(ParentError::Full);
        }
        self.met.insert(symbol.0, met);
        Ok(())
    }

    /// `symbol`'s slot: its own, or the next one numbered, its pair kept for the next `Declare`.
    fn slot_of(&mut self, symbol: Symbol) -> Result<Slot, ParentError> {
        let met = self.met.get(&symbol.0).copied();
        if let Some(slot) = met.and_then(Met::slot) {
            return Ok(slot);
        }
        let number = u32::try_from(self.slots.len()).map_err(|_| ParentError::Full)?;
        if number >= MOST_SLOTS {
            return Err(ParentError::Full);
        }
        let slot = Slot(number);
        // Unknown before it was declared stays unknown, now through its slot.
        let entered = match met {
            Some(Met::Unknown { .. }) => Met::Unknown { slot: Some(slot) },
            Some(Met::Declared(_) | Met::Published { .. }) | None => Met::Declared(slot),
        };
        self.enter(symbol, entered)?;
        self.slots.push(symbol);
        self.pairs.push(Declared { symbol, slot });
        Ok(slot)
    }

    /// `symbol` resolved to nothing: its slot kept, when it has one.
    fn settle_unknown(&mut self, symbol: Symbol) -> Result<(), ParentError> {
        let slot = self.met.get(&symbol.0).copied().and_then(Met::slot);
        self.enter(symbol, Met::Unknown { slot })
    }

    /// A record answered for the first time, just published: a group of the next start list
    /// while the window is open, the data its `Publish` stood on as sent now (the batch: each
    /// datum once, in the group of its first namer) and `named`, then its row. The window
    /// closes at the first group that does not fit under the cap: no later answer is kept (it
    /// may stand on that group's statics), and nothing fails.
    fn first_answer(&mut self, row: Row, named: &[Symbol]) -> Result<(), ParentError> {
        self.answered += 1;
        if self.closed {
            return Ok(());
        }
        // A positioned record's group keeps the values its `Positioned` sent (start_list.rs
        // version 5): the list is written under today's certificate, under which every file's
        // text is the same, so they are what a replay under it must send. Under another
        // certificate the list's values are not used (start_list.rs `Held::Other`).
        let values: &[u8] = self.positioned.get(&row.record.0).map_or(&[], |values| values.as_slice());
        if !self.first.fits(start_list::Rows::group_rows(self.batch.len(), named.len(), values.len())) {
            self.closed = true;
            return Ok(());
        }
        self.first.push_answer(self.batch.as_slice(), named, values, row).map_err(ParentError::StartList)
    }

    /// The pairs numbered since the last `Declare`, told in one.
    fn declare<A: Allocator>(&mut self, out: &mut std::vec::Vec<u8, A>) -> Result<(), ParentError> {
        if self.pairs.is_empty() {
            return Ok(());
        }
        let id = self.id();
        Down::Declare { pairs: List::of(self.pairs.as_slice()) }.encode(id, out)?;
        self.pairs.clear();
        Ok(())
    }

    /// The program's native library `library` told as the run's FIRST frame (blaze-launch
    /// wire/run.rs `Down::Native`, its one rule): each Rust word's symbol, in word order, given
    /// its slot (pair i fills word i), in one `Native` instead of a `Declare`. Each symbol is met
    /// as declared, so the C's first call of a word is a need of it like any. Before any slot
    /// is numbered (the personality's and today's list's come after), else
    /// [`ParentError::NativeLate`]. A run whose image holds no native code never calls it.
    pub(crate) fn tell_native<A: Allocator>(&mut self, library: RecordId, words: &[Symbol], out: &mut std::vec::Vec<u8, A>) -> Result<(), ParentError> {
        if !self.slots.is_empty() {
            return Err(ParentError::NativeLate { slots: self.slots.len() });
        }
        // Each numbered here is a pair not yet told (`slot_of`), in word order: none was before.
        for &symbol in words {
            self.slot_of(symbol)?;
        }
        let id = self.id();
        Down::Native { library, words: List::of(self.pairs.as_slice()) }.encode(id, out)?;
        // Told in the `Native`: not again in the next `Declare`.
        self.pairs.clear();
        Ok(())
    }

    /// The program's personality routine named before anything is published (UNWIND.md 3.3,
    /// piece P3; blaze-launch wire/personality.rs): its symbol's slot numbered and told in a
    /// `Declare`, then `Down::Personality` naming that slot, once. The symbol is NOT resolved:
    /// its record is made at the routine's first call (the first throw), like any fn's, through
    /// the slot's thunk. `None` (built `panic = "abort"`) sends nothing.
    pub(crate) fn name_personality<A: Allocator>(&mut self, personality: Option<CodeSymbol>, out: &mut std::vec::Vec<u8, A>) -> Result<(), ParentError> {
        let Some(symbol) = personality else { return Ok(()) };
        let slot = self.slot_of(Symbol(symbol.0))?;
        self.declare(out)?;
        let id = self.id();
        Down::Personality { slot }.encode(id, out)?;
        Ok(())
    }

    /// Today's start list placed before any need is read (run.md D124: "its rows are published
    /// as they are before `Begin`, nothing keyed or looked up"): every code row's slot told in
    /// one `Declare`, then each group in the list's order as its run sent it, through the one
    /// writer ([`Needs::publish`] with the group's data and names: its data not on the child
    /// yet in one `Data`, the descriptor told if it names it, each static it names checked on
    /// the child, then its `Publish`); each group is this run's first answer of its symbol, so a
    /// need of it later resolves nothing. `groups` are the groups kept whole ([`ahead_of`]),
    /// each whole by its type (a group always ends in its code row).
    pub(crate) fn place_ahead<A: Allocator>(&mut self, groups: &[TodayGroup<'_>], out: &mut std::vec::Vec<u8, A>) -> Result<(), ParentError> {
        self.group_slots.clear();
        for group in groups {
            let row = group.code();
            let slot = self.slot_of(row.symbol)?;
            self.enter(row.symbol, Met::Published { slot, record: row.record })?;
            self.group_slots.push(slot);
        }
        self.declare(out)?;
        let slots = core::mem::replace(&mut self.group_slots, Vec::new_in(self.alloc));
        for (group, slot) in groups.iter().zip(slots.iter().copied()) {
            let (view, row) = (group.view(), group.code());
            let record = row.record;
            self.group_data.clear();
            self.group_data.extend(view.data());
            self.group_named.clear();
            self.group_named.extend(view.named());
            // A positioned group: its values as the list holds them (today's certificate, so
            // today's lines), sent in its `Positioned` by the one writer and kept for its group
            // in this run's list, as a resolved record's are (`answer`).
            if view.values().next().is_some() {
                let mut values: AVec<'a, u8> = Vec::new_in(self.alloc);
                for chunk in view.values() {
                    values.extend_from_slice(chunk);
                }
                self.positioned.insert(record.0, values);
                self.placed_positioned += 1;
            }
            // The group's two lists out of `self` while the one writer borrows it, then back.
            let data = core::mem::replace(&mut self.group_data, Vec::new_in(self.alloc));
            let named = core::mem::replace(&mut self.group_named, Vec::new_in(self.alloc));
            let placed = self.publish(slot, record, Stands::Answered { named: named.as_slice(), data: data.as_slice() }, out).and_then(|()| self.first_answer(row, named.as_slice()));
            self.placed += 1;
            self.placed_data += data.len();
            self.group_data = data;
            self.group_named = named;
            if let Err(why) = placed {
                self.group_slots = slots;
                return Err(why);
            }
        }
        self.group_slots = slots;
        Ok(())
    }

    /// Every whole frame at the start of `inbox` answered onto the end of `out`, asking
    /// `resolve` for each symbol not resolved yet: the bytes taken (the rest is a frame not all
    /// here yet). Each frame takes at least its 4-byte length, so the loop ends. A need the
    /// resolver answers [`Resolved::Waiting`] stops the loop at its frame, not taken: the caller
    /// serves the rest again once the code came (D194).
    pub(crate) fn serve<R: Resolve + ?Sized, A: Allocator>(&mut self, resolve: &mut R, inbox: &[u8], out: &mut std::vec::Vec<u8, A>) -> Result<usize, ParentError> {
        let mut taken = 0;
        while let Some((item, len)) = split(&inbox[taken..])? {
            let (_, up) = Up::decode(item)?;
            match up {
                Up::Need { need, .. } => {
                    if !self.answer(need, resolve, out)? {
                        return Ok(taken);
                    }
                }
                // No ask: nothing answers it; the first is kept to be said by name.
                Up::Fault { fault } => {
                    if self.fault.is_none() {
                        self.fault = Some(fault);
                    }
                }
            }
            taken += len;
        }
        Ok(taken)
    }

    /// One need answered onto the end of `out`: `true`; `false` when the resolver answered
    /// [`Resolved::Waiting`], with nothing told and nothing counted (the need is asked again).
    pub(crate) fn answer<R: Resolve + ?Sized, A: Allocator>(&mut self, need: Needed, resolve: &mut R, out: &mut std::vec::Vec<u8, A>) -> Result<bool, ParentError> {
        let symbol = match need {
            Needed::Symbol(symbol) => symbol,
            Needed::Slot(slot) => *self.slots.get(slot.0 as usize).ok_or(ParentError::UndeclaredSlot { slot: slot.0 })?,
        };
        match self.met.get(&symbol.0).copied() {
            Some(Met::Published { slot, record }) => self.publish(slot, record, Stands::Again, out).map(|()| true),
            Some(Met::Unknown { .. }) => self.failed(need, out).map(|()| true),
            Some(Met::Declared(_)) | None => {
                let mut values = Vec::new_in(self.alloc);
                match resolve.publication(&symbol, &mut values)? {
                    Resolved::Waiting => Ok(false),
                    Resolved::Unknown => {
                        self.resolved += 1;
                        self.settle_unknown(symbol)?;
                        self.failed(need, out).map(|()| true)
                    }
                    Resolved::Record { key, record, declares, data, named, sent } => {
                        // The bytes first: nothing may name a record before its bytes.
                        self.send(sent, out)?;
                        self.resolved += 1;
                        if !values.is_empty() {
                            self.positioned.insert(record.0, values);
                        }
                        let slot = self.slot_of(symbol)?;
                        for each in declares {
                            self.slot_of(*each)?;
                        }
                        self.enter(symbol, Met::Published { slot, record })?;
                        // Published first, so the batch is what this `Publish` stood on; then
                        // the next list's group: those data, every static named, the row
                        // (start_list.rs version 4; run.md, "Statics in the run").
                        self.publish(slot, record, Stands::Answered { named, data }, out)?;
                        self.first_answer(Row { symbol, key, record }, named).map(|()| true)
                    }
                }
            }
        }
    }

    fn failed<A: Allocator>(&mut self, need: Needed, out: &mut std::vec::Vec<u8, A>) -> Result<(), ParentError> {
        let id = self.id();
        Down::Failed { need, why: Failure::Unresolved }.encode(id, out)?;
        Ok(())
    }
}

/// The slot bookkeeping, and the lazy parent over a model program (run_parent_tests.rs).
#[cfg(test)]
#[path = "run_parent_tests.rs"]
mod tests;

/// A warm run replayed from today's list: every first call answered with no resolve, statics
/// included, and the control without the static-naming groups (run_replay_tests.rs).
#[cfg(test)]
#[path = "run_replay_tests.rs"]
mod replay_tests;

/// The records sent inline (run_bytes.rs) against the files control, on the wire: the bytes
/// before every frame naming them, each once, and a replay finding them through the store.
#[cfg(test)]
#[path = "run_bytes_tests.rs"]
mod bytes_tests;

/// Asserts: a positioned record's `Positioned` carries the values one source lookup gave, at
/// every need of it; its group (its data, its name, its values, its row) is in the list written
/// under today's certificate (start_list.rs version 5), and a replay under that certificate
/// sends `Positioned` with the list's values and looks nothing up. CONTROL: under another
/// certificate the list's values are not used (`Held::Other` hands out symbols alone), and the
/// record resolves again with today's positions. A missing source is refused by row.
#[cfg(test)]
#[test]
fn positioned_groups_are_listed_with_their_values_and_replayed_under_todays_certificate() {
    use blaze_code::record::{CodeRecord, CodeView, PositionField, PositionPart, PositionRow};
    use blaze_ladder::seam::PositionGap;

    const FUNCTION: Symbol = Symbol([1; 32]);
    const STATIC: Symbol = Symbol([2; 32]);
    const OTHER: Symbol = Symbol([3; 32]);
    const DATUM: Datum = Datum { symbol: STATIC, record: RecordId([4; 32]) };
    let frame = bstd::mem::Frame::with_reserve(1 << 17, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let mut record = CodeRecord::new_in(alloc);
    record.relro.resize(8, 0);
    let line = PositionRow { part: PositionPart::Relro, at: 0, item: 91, site: 17, field: PositionField::Line };
    record.positions.push(line);
    record.positions.push(PositionRow { at: 4, field: PositionField::Col, ..line });
    let mut bytes = Vec::new_in(alloc);
    record.seal().expect("two four-byte slots").encode(&mut bytes);
    let view = CodeView::read(&bytes).expect("sealed record");

    struct FromSource<'r> {
        view: CodeView<'r>,
        at: Result<(u32, u32), PositionGap>,
        reads: usize,
    }
    impl Resolve for FromSource<'_> {
        fn resolve(&mut self, symbol: &Symbol) -> Result<Resolved<'_>, ParentError> {
            Ok(Resolved::Record {
                record: if *symbol == FUNCTION { RecordId(blaze_blake3::hash(self.view.bytes())) } else { RecordId(symbol.0) },
                key: CodeKey(blaze_ids::ContentId(symbol.0)),
                declares: &[],
                data: if *symbol == FUNCTION { &[DATUM] } else { &[] },
                named: if *symbol == FUNCTION { &[STATIC] } else { &[] },
                sent: Sent::NONE,
            })
        }
        fn publication(&mut self, symbol: &Symbol, values: &mut AVec<'_, u8>) -> Result<Resolved<'_>, ParentError> {
            if *symbol == FUNCTION {
                position_values(&self.view, symbol.0, values, |item, site| {
                    assert_eq!((item, site), (ItemKey(91), 17));
                    self.reads += 1;
                    self.at
                })?;
            }
            self.resolve(symbol)
        }
    }

    // Each `Positioned` in `out`: its record, slot and values.
    let positioned_in = |out: &[u8]| {
        let mut found = std::vec::Vec::new();
        let mut rest = out;
        while let Some((item, len)) = split(rest).expect("whole frame") {
            if let (_, Down::Positioned { record, slot, values, .. }) = Down::decode(item).expect("valid publication") {
                found.push((record, slot, std::vec::Vec::from(values)));
            }
            rest = &rest[len..];
        }
        found
    };
    let positioned_record = RecordId(blaze_blake3::hash(&bytes));
    let other_row = Row { symbol: OTHER, key: CodeKey(blaze_ids::ContentId(OTHER.0)), record: RecordId(OTHER.0) };
    let function_row = Row { symbol: FUNCTION, key: CodeKey(blaze_ids::ContentId(FUNCTION.0)), record: positioned_record };
    use start_list::Line;
    let mut written: std::vec::Vec<Line> = std::vec::Vec::new();
    for source_line in [11u32, 14] {
        let mut front = FromSource { view, at: Ok((source_line, 7)), reads: 0 };
        let mut needs = Needs::new(alloc);
        let mut out = std::vec::Vec::new();
        for need in [Needed::Symbol(FUNCTION), Needed::Slot(Slot(0)), Needed::Symbol(OTHER)] {
            needs.answer(need, &mut front, &mut out).expect("served");
        }
        assert_eq!(front.reads, 1, "one lookup gives both fields and every repeat need");
        let values = [source_line.to_le_bytes(), 7u32.to_le_bytes()].concat();
        let chunks: std::vec::Vec<Line> = start_list::Values::rows(&values).map(Line::Values).collect();
        assert_eq!(chunks.len(), 1, "eight bytes of values are one values row");
        let mut want = std::vec![Line::Datum(DATUM), Line::Named(STATIC)];
        want.extend_from_slice(&chunks);
        want.extend_from_slice(&[Line::Code(function_row), Line::Code(other_row)]);
        assert_eq!(needs.first().lines().collect::<std::vec::Vec<_>>(), want, "the positioned record's group, its values with it, is in the list");
        let sent = positioned_in(&out);
        assert_eq!(sent.len(), 2);
        assert!(sent.iter().all(|(record, slot, sent)| (*record, *slot, sent.as_slice()) == (positioned_record, Slot(0), values.as_slice())), "{sent:?}");
        if source_line == 11 {
            written = needs.first().lines().collect();
        }
    }

    // The list run 1 wrote, through its bytes, under its certificate.
    const TODAY: start_list::Certificate = start_list::Certificate::for_tests([0xa1; 32]);
    const MOVED: start_list::Certificate = start_list::Certificate::for_tests([0xb2; 32]);
    let head = StartHead { line: blaze_ladder::run::special::EntryLine { form: blaze_ladder::run::special::EntryForm::Unit, symbol: CodeSymbol([9; 32]) }, personality: None };
    let mut list = Vec::new_in(alloc);
    start_list::encode_lines(&TODAY, head, written.as_slice(), &mut list).expect("a small list");
    let view_of = start_list::read(&list, alloc).expect("the list reads back");

    // Under today's certificate: placed as written, `Positioned` with the list's values (line
    // 11), and nothing looked up: the source answers nothing.
    let start_list::Held::Today(rows) = view_of.under(&TODAY) else { panic!("the list under its own certificate is not today's") };
    let lines: std::vec::Vec<Line> = rows.lines().collect();
    assert_eq!(lines, written, "the bytes round-trip the positioned group");
    let groups: std::vec::Vec<TodayGroup<'_>> = rows.groups().collect();
    let mut front = FromSource { view, at: Err(PositionGap::Unanswered), reads: 0 };
    let mut needs = Needs::new(alloc);
    let mut out = std::vec::Vec::new();
    needs.place_ahead(&groups, &mut out).expect("today's list placed");
    needs.answer(Needed::Slot(Slot(0)), &mut front, &mut out).expect("a repeat need of the placed record");
    assert_eq!(front.reads, 0, "a replay under today's certificate looked a position up");
    let listed = [11u32.to_le_bytes(), 7u32.to_le_bytes()].concat();
    let sent = positioned_in(&out);
    assert_eq!(sent.len(), 2, "the placed publish and the repeat need");
    assert!(sent.iter().all(|(record, slot, sent)| (*record, *slot, sent.as_slice()) == (positioned_record, Slot(0), listed.as_slice())), "{sent:?}");
    assert_eq!(needs.first().lines().collect::<std::vec::Vec<_>>(), written, "the replay writes the same list again");
    let tally = needs.start_tally(Built::No, ListHeld::Today);
    assert_eq!((tally.placed, tally.positioned, tally.resolved), (2, 1, 0));

    // CONTROL: under another certificate the list's values are not used (they may have moved):
    // symbols alone, and the record resolves again with today's positions (line 14).
    let start_list::Held::Other(symbols) = view_of.under(&MOVED) else { panic!("the list under another certificate was today's") };
    assert_eq!(symbols.iter().collect::<std::vec::Vec<_>>(), [FUNCTION, OTHER], "another certificate's list hands out its code symbols alone");
    let mut front = FromSource { view, at: Ok((14, 7)), reads: 0 };
    let mut needs = Needs::new(alloc);
    let mut out = std::vec::Vec::new();
    needs.answer(Needed::Symbol(FUNCTION), &mut front, &mut out).expect("resolved");
    assert_eq!(front.reads, 1, "under another certificate the position is looked up today");
    let today = [14u32.to_le_bytes(), 7u32.to_le_bytes()].concat();
    assert_eq!(positioned_in(&out).iter().map(|(_, _, values)| values.clone()).collect::<std::vec::Vec<_>>(), [today], "the old list's line 11 was sent");

    // Remove the source entirely. The parent refuses the exact row before any frame is sent.
    let mut front = FromSource { view, at: Err(PositionGap::Unanswered), reads: 0 };
    let mut needs = Needs::new(alloc);
    let mut out = std::vec::Vec::new();
    let error = needs.answer(Needed::Symbol(FUNCTION), &mut front, &mut out).expect_err("no resolver");
    assert!(matches!(error, ParentError::Position { symbol, row, why: PositionGap::Unanswered } if symbol == FUNCTION.0 && row == line));
    assert!(out.is_empty() && needs.first().is_empty());
    std::println!("positions: (11, 7) then (14, 7); today's list replays (11, 7) with no lookup; another certificate's resolves (14, 7); missing source refused");
}
