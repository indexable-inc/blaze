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
//!   the gate before the start left its check `Clean` ([`Gate`]); anything else stops the parent
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
use blaze_ladder::run::make_one::{MOST_WALKED, Maker, NamesRecords};
use blaze_ladder::run::special::{self, Roots, RootsError, SpecialMade, StartHead, make_special, special_of};
use blaze_ladder::run::{OverflowChecks, RunError};
use blaze_launch::wire::run::{Declared, Down, Failure, Needed, Symbol, Up};
use blaze_launch::wire::{List, RecordId, WireError, split};
use blaze_launch::{Slot, Tier};
use blaze_own::{AVec, Alloc, Vec};
use blaze_screen::{Answered, Stage, Step, Unwatched, Watch};
use blaze_typeck::ItemKey;
use bstd::Cx;
use bstd::fs::{Content, Dir, Durable, FsError, ReadWrite};
use bstd::path::{Path, PathError};
use bstd::procs::Ended;

use crate::run_ids::{Checked, Grain, RunIds};
use crate::start_list::{self, Line, Row, TodayRows};

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
    /// The workspace fn `function` has no clean check today (`why`; `None`: the body ids hold
    /// nothing for it): the gate before the start must have left it `Clean`, and the parent
    /// never checks or builds it another way.
    NotChecked { function: ItemKey, symbol: [u8; 32], why: Option<Checked> },
    /// The code record of `symbol` has a row that lands (a cleanup or a catch that unwinding
    /// must run: its unwind entry needs a personality routine) and no crate of the program
    /// declares `#[lang = "eh_personality"]` (blaze-ladder personality.rs: a program that never
    /// lands needs none, so this is refused here, at the record, never at the start).
    /// `function`: the record's fn (`None`: a special record); `path`: its item path, when the
    /// names can give it.
    NoPersonality { function: Option<ItemKey>, path: Option<Box<str>>, symbol: [u8; 32] },
    /// The code record of `symbol` does not read as a code record (asked whether it lands).
    Decode { symbol: [u8; 32], why: blaze_code::record::DecodeError },
    /// The code record of `symbol` has unwind rows that do not read (asked whether it lands).
    Unwind { symbol: [u8; 32], why: blaze_code::unwind::UnwindError },
    /// The resolver was asked twice for `symbol` (`Needs` asks once: a bug).
    ResolvedTwice { symbol: [u8; 32] },
    /// The special record of `symbol` (the shim, the getter, the ender) names a static: the
    /// specials are made by run/special.rs over no program, so none may (run.md, "Statics in
    /// the run").
    SpecialStatic { symbol: [u8; 32] },
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
    /// A row placed ahead has no record answer after it was settled (a bug of `place_ahead`).
    UndeclaredSymbol { symbol: [u8; 32] },
    /// Start-list rows of data or names placed with no code row after them (static `symbol`
    /// the first): `read` and [`ahead_of`] leave no group open, so a bug.
    StartGroupOpen { symbol: [u8; 32] },
    /// The walk forward from the roots stepped `most` functions the program has not called and
    /// did not reach `symbol` (rule 18).
    TooFar { symbol: [u8; 32], most: usize },
}

impl fmt::Display for ParentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = "`blaze run` did not start or serve the program (run.md piece 6, D124)";
        match self {
            ParentError::Build(why) => write!(f, "{rule}: {why}"),
            ParentError::Walk { symbol, why } => write!(f, "{rule}: the first call of symbol {}: {why}", Hex(symbol)),
            ParentError::Special { symbol, why } => write!(f, "{rule}: the run's special record {}: {why:?}", Hex(symbol)),
            ParentError::Roots(why) => write!(f, "{rule}: {why}"),
            ParentError::BodyIds { shown } => write!(f, "{rule}: the body ids: {shown}"),
            ParentError::Names { shown } => write!(f, "{rule}: the names records: {shown}"),
            ParentError::NotChecked { function, symbol, why } => {
                write!(f, "{rule}: the program called fn {:#x} (symbol {}), a workspace body ", function.0, Hex(symbol))?;
                match why {
                    Some(why) => write!(f, "whose check today is {why:?}")?,
                    None => write!(f, "the body ids hold nothing for")?,
                }
                write!(f, "; the gate before the start leaves every body the program reaches `Clean`, and the parent never checks or builds one another way")
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
            ParentError::Unwind { symbol, why } => write!(f, "{rule}: the unwind rows of the code record of symbol {}: {why}", Hex(symbol)),
            ParentError::ResolvedTwice { symbol } => write!(f, "{rule}: the symbol {} was resolved twice (run_parent.rs `Needs` asks once a symbol)", Hex(symbol)),
            ParentError::SpecialStatic { symbol } => write!(f, "{rule}: the run's special record {} names a static, and the specials are made over no program (run.md, \"Statics in the run\")", Hex(symbol)),
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
            ParentError::UndeclaredSymbol { symbol } => write!(f, "{rule}: the start list's row of symbol {} was placed ahead without a slot", Hex(symbol)),
            ParentError::StartGroupOpen { symbol } => write!(f, "{rule}: the start list's rows naming static {} were placed with no code row after them (run_parent.rs `Needs::place_ahead`)", Hex(symbol)),
            ParentError::TooFar { symbol, most } => write!(f, "{rule}: the walk forward from the roots stepped {most} functions the program has not called and did not reach symbol {}", Hex(symbol)),
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
    const OURS: [&str; 4] = ["BLAZE_LAUNCH_FD", "BLAZE_LAUNCH_STORE", "BLAZE_LAUNCH_ENTRY", blaze_launch::grants::VAR];
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

/// How the child ended, as `blaze run`'s status; `None` while it runs.
pub(crate) fn status_of(ended: Ended) -> Option<u8> {
    match ended {
        // A process's exit status is its low 8 bits (`_exit`): what the kernel reports.
        Ended::Exited(status) => Some((status & 0xff) as u8),
        // The shell's form for an end by signal: 128 + its number.
        Ended::Signaled(signal) => Some(128u8.saturating_add((signal & 0x7f) as u8)),
        Ended::Deadline => None,
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
    /// (start_list.rs version 4), so a replay sends the same.
    Record { key: CodeKey, record: RecordId, declares: &'r [Symbol], data: &'r [Datum], named: &'r [Symbol] },
    /// No function of the program by that symbol: the need is answered `Failed` with
    /// `Failure::Unresolved`, naming it.
    Unknown,
}

/// The seam a need asks through for a symbol [`Needs`] has not resolved yet. [`Needs`] asks it
/// at most once a symbol, however many needs name it, and never for a row placed from today's
/// start list. The command's one impl is [`Lazy`].
pub(crate) trait Resolve {
    fn resolve(&mut self, symbol: &Symbol) -> Result<Resolved<'_>, ParentError>;
    /// The sink the run's events go to (blaze-screen [`Watch`]): the loop tells it when the
    /// program starts and when a need is answered. [`Unwatched`] unless the resolver has one.
    fn watch(&self) -> &dyn Watch {
        &Unwatched
    }
}

/// THE GATE'S PROOF, read before a row is stepped: which grain a fn's body is keyed at and,
/// for a workspace body, what its check left today (run_ids.rs).
pub(crate) trait Gate: BodyIds {
    fn grain(&self, item: ItemKey) -> Option<Grain>;
    fn checked(&self, item: ItemKey) -> Option<Checked>;
}

impl Gate for RunIds<'_> {
    fn grain(&self, item: ItemKey) -> Option<Grain> {
        RunIds::grain(self, item)
    }

    fn checked(&self, item: ItemKey) -> Option<Checked> {
        RunIds::checked(self, item)
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

/// The front end, made only when a need requires it ([`Lazy`]): the roots on the first call
/// the start list does not answer, the body ids and the maker on the first row stepped.
pub(crate) trait Front {
    type Start: Start;
    type Ids: Gate;
    type Maker: Make<Error = RunError>;
    fn start(&mut self) -> Result<Self::Start, ParentError>;
    fn ids(&mut self) -> Result<Self::Ids, ParentError>;
    fn maker(&mut self, start: &Self::Start) -> Result<Self::Maker, ParentError>;
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
    /// Whether the code record `record` (of `symbol`) has a row that lands, so that its unwind
    /// entry names a personality routine (blaze-code unwind.rs `Unwind::lands`). Asked only
    /// when the run's head names no routine ([`no_routine_lands`]).
    fn lands(&self, symbol: [u8; 32], record: &[u8]) -> Result<bool, ParentError> {
        let view = blaze_code::record::CodeView::read(record).map_err(|why| ParentError::Decode { symbol, why })?;
        let unwind = blaze_code::unwind::read(view).map_err(|why| ParentError::Unwind { symbol, why })?;
        Ok(unwind.lands())
    }
    /// The item path of fn `item` (`krate::module::name`), for a refusal's words: `None` when
    /// the front cannot name it (the refusal then shows its key). Read only on an error's path.
    fn path_of(&mut self, _item: ItemKey) -> Option<Box<str>> {
        None
    }
    /// The sink the run's events go to ([`Watch`]): [`Unwatched`] unless the front has one.
    fn watch(&self) -> &dyn Watch {
        &Unwatched
    }
}

/// A code record that lands, refused when the run names no personality routine
/// ([`ParentError::NoPersonality`]): `personality` is the run's head's; `function`, the record's
/// fn (`None`: a special record). Nothing is read when the head names a routine.
fn no_routine_lands<F: Front>(front: &mut F, personality: Option<CodeSymbol>, function: Option<ItemKey>, symbol: [u8; 32], record: &[u8]) -> Result<(), ParentError> {
    if personality.is_some() || !front.lands(symbol, record)? {
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
    kept: Option<NamesRecords<'h>>,
    builder: Builder,
    tables: Alloc<'static>,
    alloc: Alloc<'r>,
    watch: &'c dyn Watch,
}

impl<'h, 'c, 'r> CommandFront<'h, 'c, 'r> {
    pub(crate) fn new(
        names: &'c mut dyn FnMut() -> Result<NamesRecords<'h>, ParentError>,
        ids: &'c mut dyn FnMut() -> Result<RunIds<'h>, ParentError>,
        builder: Builder,
        tables: Alloc<'static>,
        alloc: Alloc<'r>,
        watch: &'c dyn Watch,
    ) -> CommandFront<'h, 'c, 'r> {
        CommandFront { names, ids, kept: None, builder, tables, alloc, watch }
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

impl<'h, 'r> Front for CommandFront<'h, '_, 'r> {
    type Start = Roots<'r>;
    type Ids = RunIds<'h>;
    type Maker = Maker<'h>;

    fn start(&mut self) -> Result<Roots<'r>, ParentError> {
        let names = self.names()?;
        self.watch.see(Step::Began(Stage::Roots));
        special::run_roots(names.records, names.lang_crates, self.builder.settings.panic, names.frame, self.tables, self.alloc).map_err(ParentError::Roots)
    }

    fn ids(&mut self) -> Result<RunIds<'h>, ParentError> {
        (self.ids)()
    }

    fn maker(&mut self, start: &Roots<'r>) -> Result<Maker<'h>, ParentError> {
        let names = self.names()?;
        Ok(Maker::new(names, start.maker_entry(), &self.builder, self.tables))
    }

    /// From the names records: the checker key's library item (its first 16 bytes,
    /// front2 keys.rs `checker_of`), its module's path and its name.
    fn path_of(&mut self, item: ItemKey) -> Option<Box<str>> {
        use blaze_ladder::front2::source::NamesSource;
        let records = self.names().ok()?.records;
        let lib = records.key_beginning(item.0.to_be_bytes())?;
        let module = records.module_path(records.home_of(lib)?)?;
        let name = records.name_of(lib)?;
        Some(std::format!("{module}::{name}").into_boxed_str())
    }

    fn watch(&self) -> &dyn Watch {
        self.watch
    }
}

/// The records folder the launcher maps from (blaze-launch-rt `run_mode::Folder`): one file a
/// record, named by the 64 hex digits of its BLAKE3.
pub(crate) trait Folder {
    /// Whether the file of `record` is there.
    fn holds(&self, record: &RecordId) -> Result<bool, ParentError>;
    /// `record` filed (a file already there holds equal bytes and is not written again): its id.
    fn file(&mut self, record: &[u8]) -> Result<RecordId, ParentError>;
}

/// The command's [`Folder`]: `<HOME>/` [`RECORDS`].
#[derive(Clone, Copy)]
pub(crate) struct RecordsDir<'r> {
    pub cx: &'r Cx,
    pub dir: &'r Dir<ReadWrite>,
}

/// A record's file name: its id's 64 hex digits.
fn file_name(id: &[u8; 32], out: &mut [u8; 64]) -> Result<Path, ParentError> {
    *out = hex(id);
    // The digits are ASCII: always UTF-8.
    let name = core::str::from_utf8(out).map_err(|_| ParentError::UndeclaredSymbol { symbol: *id })?;
    Path::new(name).map_err(ParentError::Path)
}

impl Folder for RecordsDir<'_> {
    fn holds(&self, record: &RecordId) -> Result<bool, ParentError> {
        let mut name = [0u8; 64];
        let path = file_name(&record.0, &mut name)?;
        Ok(self.dir.stat(self.cx, &path).map_err(ParentError::Fs)?.is_some())
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
        write!(f, "code records: reached {}, found {}, made {}, unkeyed {}", self.reached, self.found, self.made, self.unkeyed)
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
/// data); resolved after the list R; front end built: no` (or `yes (..)`, naming why).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct StartTally {
    /// Code rows placed from today's list.
    pub placed: usize,
    /// This run's first answers with a record, the placed ones included.
    pub answered: usize,
    /// Datum rows placed from today's list.
    pub data: usize,
    /// Symbols asked of the resolver (each once; an unknown one too).
    pub resolved: usize,
    pub built: Built,
    pub held: ListHeld,
}

impl fmt::Display for StartTally {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "start list: placed {} of {} first answers ({} data); resolved after the list {}; front end built: ",
            self.placed, self.answered, self.data, self.resolved
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
    /// The symbols placed from today's start list, sorted: their records are published, and the
    /// walk steps such a row (found in the store) only to reach a callee the program asks for.
    placed: AVec<'a, [u8; 32]>,
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
}

/// A row stepped ahead of its first call, as `step` answered it: its entry kept when it is not
/// in the store under its key (made or unkeyed: the scratch's bytes are the next step's).
struct Ahead<'a> {
    row: usize,
    how: How,
    key: Option<CodeKey>,
    bytes: Option<AVec<'a, u8>>,
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
        }
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

    pub(crate) fn folder(&self) -> &D {
        &self.folder
    }

    /// `lines` placed ahead from today's start list: each code row's symbol among the placed
    /// (the walk steps such a row only to reach a callee), each datum's among the statics told
    /// (a later record reaching it sends it again never), each row reached and found.
    fn placed(&mut self, lines: &[Line]) {
        let mut rows = 0usize;
        for line in lines {
            match line {
                Line::Code(row) => {
                    self.placed.push(row.symbol.0);
                    rows += 1;
                }
                Line::Datum(datum) => {
                    if let Err(at) = self.told.binary_search(&datum.symbol.0) {
                        self.told.insert(at, datum.symbol.0);
                    }
                    rows += 1;
                }
                Line::Named(_) => {}
            }
        }
        self.placed.sort_unstable();
        self.placed.dedup();
        self.tally.reached += rows;
        self.tally.found += rows;
        self.front.watch().see(Step::Placed { rows: u32::try_from(rows).unwrap_or(u32::MAX) });
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

/// ONE step of the walk for `row`: the body ids and the maker made if they are not yet, THE
/// GATE'S PROOF read (a workspace body must be `Clean` today), then `Walk::one` in its span,
/// whose event says how it was answered.
fn step<F: Front>(cx: &Cx, front: &mut F, started: &mut Started<'_, F::Start, F::Ids, F::Maker>, store: &mut dyn CodeStore, builder: &Builder, row: usize) -> Result<(How, Option<CodeKey>), ParentError> {
    let symbol = started.walk.symbol(row).0;
    let heavy = match started.heavy.take() {
        Some(heavy) => heavy,
        None => {
            let ids = front.ids()?;
            let maker = front.maker(&started.start)?;
            Heavy { ids, maker }
        }
    };
    let heavy = started.heavy.insert(heavy);
    let tabled = started.walk.tabled(row);
    if heavy.ids.grain(tabled) == Some(Grain::Workspace) {
        match heavy.ids.checked(tabled) {
            Some(Checked::Clean) => {}
            why => return Err(ParentError::NotChecked { function: tabled, symbol, why }),
        }
    }
    let path = if front.watch().paths() { front.path_of(tabled) } else { None };
    front.watch().see(Step::Resolving { path: path.as_deref() });
    let cx = cx.span(FIRST_CALL_SPAN);
    let one = started
        .walk
        .one(row, &heavy.ids, store, builder, &builder.producer, &mut heavy.maker, front.form(), MOST_WALKED, &mut started.scratch)
        .map_err(|why| ParentError::Walk { symbol, why })?;
    let (how, key) = (one.how, one.key);
    cx.event(match how {
        How::Found => HOW_FOUND,
        How::Made => HOW_MADE,
        How::Unkeyed(Unkeyed::NoRecord) => HOW_NO_RECORD,
        How::Unkeyed(Unkeyed::Stale { .. }) => HOW_STALE,
        // `one` never answers `Waiting`.
        How::Waiting => FIRST_CALL_SPAN,
    });
    front.watch().see(Step::Answered(match how {
        How::Found => Answered::Found,
        How::Made => Answered::Made,
        How::Unkeyed(_) | How::Waiting => Answered::Unkeyed,
    }));
    Ok((how, key))
}

/// Row `row` served once: stepped now if it is waiting, else taken from what its step ahead
/// answered (`twice`: the refusal when it is neither, a row served already).
#[expect(clippy::too_many_arguments, reason = "the resolver's parts, borrowed apart")]
fn serve<'a, F: Front>(
    cx: &Cx,
    front: &mut F,
    started: &mut Started<'a, F::Start, F::Ids, F::Maker>,
    store: &mut dyn CodeStore,
    builder: &Builder,
    ahead: &mut AVec<'a, Ahead<'a>>,
    row: usize,
    twice: ParentError,
) -> Result<(How, Option<CodeKey>, Option<AVec<'a, u8>>), ParentError> {
    if started.walk.how(row) == How::Waiting {
        let (how, key) = step(cx, front, started, store, builder, row)?;
        return Ok((how, key, None));
    }
    let Some(i) = ahead.iter().position(|one| one.row == row) else { return Err(twice) };
    let one = ahead.swap_remove(i);
    Ok((one.how, one.key, one.bytes))
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

fn count(tally: &mut Tally, how: How) {
    tally.reached += 1;
    match how {
        How::Found => tally.found += 1,
        How::Made => tally.made += 1,
        How::Unkeyed(_) | How::Waiting => tally.unkeyed += 1,
    }
}

impl<F: Front, D: Folder> Resolve for Lazy<'_, '_, F, D> {
    fn resolve(&mut self, symbol: &Symbol) -> Result<Resolved<'_>, ParentError> {
        let Lazy { cx, front, folder, store, builder, alloc, started, placed, declares, ahead, told, data, statics, named, tally, built } = self;
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
            if started.walk.how(at) == How::Waiting && placed.binary_search(&started.walk.symbol(at).0).is_ok() {
                step(cx, front, started, &mut **store, &builder, at)?;
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
            no_routine_lands(&mut *front, started.start.head().personality, None, symbol.0, record)?;
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
            return Ok(Resolved::Record { key: made.key, record: id, declares: declares.as_slice(), data: &[], named: named.as_slice() });
        }
        // Still unreached (the start list's cap broke the chain to it): walk forward from the
        // roots, stepping each row still waiting in walk order, at most `MOST_WALKED`, until the
        // walk declares it. Each is kept to be served at its own first call ([`Ahead`]).
        // `Unknown` is left for a symbol no record declares.
        let mut stepped = 0;
        let mut at = 0;
        while row.is_none() && at < started.walk.len() {
            if started.walk.how(at) == How::Waiting {
                if stepped == MOST_WALKED {
                    return Err(ParentError::TooFar { symbol: symbol.0, most: MOST_WALKED });
                }
                stepped += 1;
                let (how, key) = step(cx, front, started, &mut **store, &builder, at)?;
                let bytes = match how {
                    How::Found => None,
                    How::Made | How::Unkeyed(_) | How::Waiting => {
                        let mut bytes: AVec<'_, u8> = Vec::new_in(*alloc);
                        bytes.extend_from_slice(started.scratch.made());
                        Some(bytes)
                    }
                };
                ahead.push(Ahead { row: at, how, key, bytes });
                row = started.walk.row_of(&code);
            }
            at += 1;
        }
        let Some(row) = row else { return Ok(Resolved::Unknown) };
        // A static or a thread-local is no call target: its symbol is never a need (the child
        // binds its word).
        if matches!(started.walk.kind(row), RowKind::Static | RowKind::ThreadLocal) {
            return Ok(Resolved::Unknown);
        }
        // A row stepped ahead is served once, from what its step answered; any other row that is
        // not waiting was served already.
        let (how, key, held) = serve(cx, front, started, &mut **store, &builder, ahead, row, ParentError::ResolvedTwice { symbol: symbol.0 })?;
        let entry: &[u8] = match (&held, how) {
            (Some(bytes), _) => bytes.as_slice(),
            (None, How::Found) => {
                let key = key.ok_or(ParentError::NotStored { symbol: symbol.0 })?;
                store.get(key).map_err(|why| ParentError::Store { symbol: symbol.0, why })?.ok_or(ParentError::NotStored { symbol: symbol.0 })?
            }
            // The made entry; an unkeyed one is not put, so these bytes are its only copy.
            (None, How::Made | How::Unkeyed(_) | How::Waiting) => started.scratch.made(),
        };
        let record = front.record_of(entry).map_err(|why| ParentError::Entry { symbol: symbol.0, why })?;
        no_routine_lands(&mut *front, started.start.head().personality, Some(started.walk.tabled(row)), symbol.0, record)?;
        let id = folder.file(record)?;
        // An unkeyed instance's row in the next list: its entry's content key.
        let key = key.unwrap_or_else(|| content_key(entry));
        front.statics_named(entry, named).map_err(|why| ParentError::Entry { symbol: symbol.0, why })?;
        callees(&started.walk, row, declares, told, statics);
        // The vtables the record names: each a datum the entry carries, filed and sent in this
        // record's `Data` unless this run told it already (its method words' instances are
        // among the entry's declared rows, so their slots go in the `Declare` before it).
        front.each_vtable(symbol.0, entry, &mut |vtable, datum| {
            if let Err(at) = told.binary_search(&vtable.0) {
                told.insert(at, vtable.0);
                data.push(Datum { symbol: vtable, record: folder.file(datum)? });
            }
            Ok(())
        })?;
        count(tally, how);
        // The statics the record reaches and this run has not told, each served once and its
        // datum filed; their own callees after them (a static's value names statics, and fns
        // from step 2 on: those are declared like the record's own).
        let mut next = 0;
        while next < statics.len() {
            let at = statics[next];
            next += 1;
            let static_ = started.walk.symbol(at).0;
            let (how, key, held) = serve(cx, front, started, &mut **store, &builder, ahead, at, ParentError::StaticServed { symbol: static_ })?;
            let entry: &[u8] = match (&held, how) {
                (Some(bytes), _) => bytes.as_slice(),
                (None, How::Found) => {
                    let key = key.ok_or(ParentError::NotStored { symbol: static_ })?;
                    store.get(key).map_err(|why| ParentError::Store { symbol: static_, why })?.ok_or(ParentError::NotStored { symbol: static_ })?
                }
                (None, How::Made | How::Unkeyed(_) | How::Waiting) => started.scratch.made(),
            };
            let datum = front.record_of(entry).map_err(|why| ParentError::Entry { symbol: static_, why })?;
            data.push(Datum { symbol: Symbol(static_), record: folder.file(datum)? });
            front.statics_named(entry, named).map_err(|why| ParentError::Entry { symbol: static_, why })?;
            callees(&started.walk, at, declares, told, statics);
            count(tally, how);
        }
        named.sort_unstable();
        named.dedup();
        Ok(Resolved::Record { key, record: id, declares: declares.as_slice(), data: data.as_slice(), named: named.as_slice() })
    }

    fn watch(&self) -> &dyn Watch {
        self.front.watch()
    }
}

/// Before the spawn, onto `out`: the personality routine named ([`Needs::name_personality`],
/// first, so no `Publish` precedes it), then today's start list (`today`, the rows under
/// today's certificate) placed. A first answer's group (start_list.rs: its data, the statics
/// it names, its code row) is kept WHOLE or left out whole: out when the folder does not hold
/// its record's file or one of its data's files, or when it names a static that neither a
/// group kept before it nor its own data send (a group left out takes its data with it, so
/// a later namer of those goes too); the thread-local descriptor is no datum, and the one
/// writer tells it before the first record naming it. Whatever is left out is resolved at its
/// first call like any other (rule 3: nothing is published that is not there). The start's
/// head: the list's when there is one (nothing of the front end is made), else the roots'.
/// `kept` holds the rows placed.
pub(crate) fn ahead_of<F: Front, D: Folder, A: Allocator>(
    today: Option<TodayRows<'_>>,
    lazy: &mut Lazy<'_, '_, F, D>,
    needs: &mut Needs<'_>,
    kept: &mut AVec<'_, Line>,
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
    let mut group = kept.len();
    let mut whole = true;
    for line in today.lines() {
        match line {
            Line::Datum(datum) => whole &= lazy.folder().holds(&datum.record)?,
            Line::Named(_) => {}
            Line::Code(row) => whole &= lazy.folder().holds(&row.record)?,
        }
        kept.push(line);
        let Line::Code(_) = line else { continue };
        let lines = &kept[group..];
        let stands = |symbol: &Symbol| {
            symbol.0 == descriptor || on.binary_search(&symbol.0).is_ok() || lines.iter().any(|it| matches!(it, Line::Datum(datum) if datum.symbol == *symbol))
        };
        if whole && !lines.iter().any(|it| matches!(it, Line::Named(symbol) if !stands(symbol))) {
            for it in lines {
                if let Line::Datum(datum) = it
                    && let Err(at) = on.binary_search(&datum.symbol.0)
                {
                    on.insert(at, datum.symbol.0);
                }
            }
        } else {
            kept.truncate(group);
        }
        group = kept.len();
        whole = true;
    }
    // `read` refused a list ending in data or names with no code row; nothing is left open.
    kept.truncate(group);
    needs.place_ahead(kept.as_slice(), out)?;
    lazy.placed(kept.as_slice());
    Ok(head)
}

/// What a symbol is to this run, once met.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Answer {
    /// Declared (a record's target) and not needed yet: not resolved.
    NotAsked,
    /// Resolved, or placed from today's start list: published through `slot`.
    Record { record: RecordId, slot: Slot },
    /// Resolved to nothing: every need of it is `Failed`.
    Unknown,
}

#[derive(Clone, Copy, Debug)]
struct Seen {
    /// Its slot, once declared.
    slot: Option<Slot>,
    answer: Answer,
}

/// The parent's slot bookkeeping (the module's doc): every symbol met, the slots numbered, the
/// pairs not yet told, and the first-answer order the next start list is.
pub(crate) struct Needs<'a> {
    /// Each symbol met, by its bytes; at most [`MOST_SLOTS`] (rule 18).
    met: ByHash<[u8; 32], Seen, Alloc<'a>>,
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
    /// The data of the `Data` at hand (those not sent before), reused.
    batch: AVec<'a, Datum>,
    /// The next message's id.
    next_id: u64,
    /// The start list's window closed: a first answer's group did not fit under its cap, so no
    /// later answer (which may stand on that group's statics) is kept.
    closed: bool,
    /// A placed group's data and names, gathered from its rows ([`Needs::place_ahead`]), reused.
    group_data: AVec<'a, Datum>,
    group_named: AVec<'a, Symbol>,
    alloc: Alloc<'a>,
    /// The start list's count ([`StartTally`]): code rows and datum rows placed, first answers,
    /// symbols asked of the resolver.
    placed: usize,
    placed_data: usize,
    answered: usize,
    resolved: usize,
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
            batch: Vec::new_in(alloc),
            next_id: 0,
            closed: false,
            group_data: Vec::new_in(alloc),
            group_named: Vec::new_in(alloc),
            alloc,
            placed: 0,
            placed_data: 0,
            answered: 0,
            resolved: 0,
        }
    }

    /// The start list's line of the closing count: what was placed, answered and asked, with
    /// `built` (the resolver's: whether the front end was built, and for what) and `held` (what
    /// the list was at the start).
    pub(crate) fn start_tally(&self, built: Built, held: ListHeld) -> StartTally {
        StartTally { placed: self.placed, answered: self.answered, data: self.placed_data, resolved: self.resolved, built, held }
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
        Down::Publish { slot, tier: Tier::Zero, record }.encode(id, out)?;
        Ok(())
    }

    fn id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// This run's first answers with a record, in order, each as its group (its data sent, the
    /// statics it names, its code row; at most the start list's cap): the rows of the list
    /// written at exit.
    pub(crate) fn first(&self) -> &[Line] {
        self.first.as_slice()
    }

    /// `symbol`'s entry; a symbol not met yet is entered with no slot, not asked.
    fn meet(&mut self, symbol: Symbol) -> Result<Seen, ParentError> {
        if let Some(seen) = self.met.get(&symbol.0) {
            return Ok(*seen);
        }
        if self.met.len() >= MOST_SLOTS as usize {
            return Err(ParentError::Full);
        }
        let seen = Seen { slot: None, answer: Answer::NotAsked };
        self.met.insert(symbol.0, seen);
        Ok(seen)
    }

    /// `symbol`'s slot: its own, or the next one numbered, its pair kept for the next `Declare`.
    fn slot_of(&mut self, symbol: Symbol) -> Result<Slot, ParentError> {
        if let Some(slot) = self.meet(symbol)?.slot {
            return Ok(slot);
        }
        let number = u32::try_from(self.slots.len()).map_err(|_| ParentError::Full)?;
        if number >= MOST_SLOTS {
            return Err(ParentError::Full);
        }
        let slot = Slot(number);
        self.slots.push(symbol);
        self.pairs.push(Declared { symbol, slot });
        if let Some(seen) = self.met.get_mut(&symbol.0) {
            seen.slot = Some(slot);
        }
        Ok(slot)
    }

    fn settle(&mut self, symbol: Symbol, answer: Answer) {
        if let Some(seen) = self.met.get_mut(&symbol.0) {
            seen.answer = answer;
        }
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
        if !self.first.fits(self.batch.len() + named.len() + 1) {
            self.closed = true;
            return Ok(());
        }
        self.first.push_answer(self.batch.as_slice(), named, row).map_err(ParentError::StartList)
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
    /// need of it later resolves nothing. `lines` are the groups kept whole ([`ahead_of`]); a
    /// row of data or names after the last code row is refused by name.
    pub(crate) fn place_ahead<A: Allocator>(&mut self, lines: &[Line], out: &mut std::vec::Vec<u8, A>) -> Result<(), ParentError> {
        for line in lines {
            if let Line::Code(row) = line {
                let slot = self.slot_of(row.symbol)?;
                self.settle(row.symbol, Answer::Record { record: row.record, slot });
            }
        }
        self.declare(out)?;
        self.group_data.clear();
        self.group_named.clear();
        for line in lines {
            let row = match *line {
                Line::Datum(datum) => {
                    self.group_data.push(datum);
                    continue;
                }
                Line::Named(symbol) => {
                    self.group_named.push(symbol);
                    continue;
                }
                Line::Code(row) => row,
            };
            let Some(Seen { answer: Answer::Record { record, slot }, .. }) = self.met.get(&row.symbol.0).copied() else {
                return Err(ParentError::UndeclaredSymbol { symbol: row.symbol.0 });
            };
            // The group's two lists out of `self` while the one writer borrows it, then back.
            let data = core::mem::replace(&mut self.group_data, Vec::new_in(self.alloc));
            let named = core::mem::replace(&mut self.group_named, Vec::new_in(self.alloc));
            let placed = self.publish(slot, record, Stands::Answered { named: named.as_slice(), data: data.as_slice() }, out).and_then(|()| self.first_answer(row, named.as_slice()));
            self.placed += 1;
            self.placed_data += data.len();
            self.group_data = data;
            self.group_named = named;
            self.group_data.clear();
            self.group_named.clear();
            placed?;
        }
        if let Some(open) = self.group_data.first().map(|datum| datum.symbol).or(self.group_named.first().copied()) {
            return Err(ParentError::StartGroupOpen { symbol: open.0 });
        }
        Ok(())
    }

    /// Every whole frame at the start of `inbox` answered onto the end of `out`, asking
    /// `resolve` for each symbol not resolved yet: the bytes taken (the rest is a frame not all
    /// here yet). Each frame takes at least its 4-byte length, so the loop ends.
    pub(crate) fn serve<R: Resolve + ?Sized, A: Allocator>(&mut self, resolve: &mut R, inbox: &[u8], out: &mut std::vec::Vec<u8, A>) -> Result<usize, ParentError> {
        let mut taken = 0;
        while let Some((item, len)) = split(&inbox[taken..])? {
            let (_, up) = Up::decode(item)?;
            match up {
                Up::Need { need, .. } => self.answer(need, resolve, out)?,
            }
            taken += len;
        }
        Ok(taken)
    }

    /// One need answered onto the end of `out`.
    pub(crate) fn answer<R: Resolve + ?Sized, A: Allocator>(&mut self, need: Needed, resolve: &mut R, out: &mut std::vec::Vec<u8, A>) -> Result<(), ParentError> {
        let symbol = match need {
            Needed::Symbol(symbol) => symbol,
            Needed::Slot(slot) => *self.slots.get(slot.0 as usize).ok_or(ParentError::UndeclaredSlot { slot: slot.0 })?,
        };
        match self.meet(symbol)?.answer {
            Answer::Record { record, slot } => self.publish(slot, record, Stands::Again, out),
            Answer::Unknown => self.failed(need, out),
            Answer::NotAsked => {
                self.resolved += 1;
                match resolve.resolve(&symbol)? {
                    Resolved::Unknown => {
                        self.settle(symbol, Answer::Unknown);
                        self.failed(need, out)
                    }
                    Resolved::Record { key, record, declares, data, named } => {
                        let slot = self.slot_of(symbol)?;
                        for each in declares {
                            self.slot_of(*each)?;
                        }
                        self.settle(symbol, Answer::Record { record, slot });
                        // Published first, so the batch is what this `Publish` stood on; then
                        // the next list's group: those data, every static named, the row
                        // (start_list.rs version 4; run.md, "Statics in the run").
                        self.publish(slot, record, Stands::Answered { named, data }, out)?;
                        self.first_answer(Row { symbol, key, record }, named)
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
