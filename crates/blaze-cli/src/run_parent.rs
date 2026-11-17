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
//!   [`Needs::place_ahead`] publishes a start list's rows (start_list.rs) before any need is
//!   read, and [`Needs::first`] is this run's first-answer order, the next list.
//! - [`ahead_of`]: today's start list placed ([`Needs::place_ahead`]), each row whose record's
//!   file is not in the folder left out (rule 3: nothing is published that is not there; it is
//!   resolved at its first call), and the entry line: the list's under today's certificate (no
//!   front end built), else the roots'.
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
use blaze_ladder::body_ids::{self, BodyIds, DeclaredList, Form, How, Make, Scratch, Unkeyed, Walk, WalkError};
use blaze_ladder::code_records::{Builder, CodeKey, CodeRecordError, CodeStore, EntryError, EntryView, NotEmitting, Settings, StoreError, TierName, content_key};
use blaze_ladder::run::make_one::{MOST_WALKED, Maker, NamesRecords};
use blaze_ladder::run::special::{self, EntryLine, Roots, RootsError, SpecialMade, make_special, special_of};
use blaze_ladder::run::{OverflowChecks, RunError};
use blaze_launch::wire::run::{Declared, Down, Failure, Needed, Symbol, Up};
use blaze_launch::wire::{List, RecordId, WireError, split};
use blaze_launch::{Slot, Tier};
use blaze_own::{AVec, Alloc, Vec};
use blaze_typeck::ItemKey;
use bstd::Cx;
use bstd::fs::{Content, Dir, Durable, FsError, ReadWrite};
use bstd::path::{Path, PathError};
use bstd::procs::Ended;

use crate::run_ids::{Checked, Grain, RunIds};
use crate::start_list::{self, Row, TodayRows};

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
    /// The resolver was asked twice for `symbol` (`Needs` asks once: a bug).
    ResolvedTwice { symbol: [u8; 32] },
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
            ParentError::ResolvedTwice { symbol } => write!(f, "{rule}: the symbol {} was resolved twice (run_parent.rs `Needs` asks once a symbol)", Hex(symbol)),
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
    /// placing it never meets a symbol without a slot).
    Record { key: CodeKey, record: RecordId, declares: &'r [Symbol] },
    /// No function of the program by that symbol: the need is answered `Failed` with
    /// `Failure::Unresolved`, naming it.
    Unknown,
}

/// The seam a need asks through for a symbol [`Needs`] has not resolved yet. [`Needs`] asks it
/// at most once a symbol, however many needs name it, and never for a row placed from today's
/// start list. The command's one impl is [`Lazy`].
pub(crate) trait Resolve {
    fn resolve(&mut self, symbol: &Symbol) -> Result<Resolved<'_>, ParentError>;
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
    /// The launcher's line for the entry.
    fn entry_line(&self) -> EntryLine;
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
    fn entry_line(&self) -> EntryLine {
        self.line()
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
    /// The code record an entry holds: what the launcher maps.
    fn record_of<'e>(&self, entry: &'e [u8]) -> Result<&'e [u8], EntryError> {
        Ok(EntryView::read(entry)?.record.bytes())
    }
}

/// The command's [`Front`]: the names records (`names`, opened once on the first need of them
/// and kept) for the roots and the maker, and the body ids (`ids`, made once, on the first row
/// stepped). `tables` is the process's heap, where the head store's and the lazy program's
/// tables live (typed `'static`); `alloc` holds the roots.
pub(crate) struct CommandFront<'h, 'c, 'r> {
    names: &'c mut dyn FnMut() -> Result<NamesRecords<'h>, ParentError>,
    ids: &'c mut dyn FnMut() -> Result<RunIds<'h>, ParentError>,
    kept: Option<NamesRecords<'h>>,
    builder: Builder,
    tables: Alloc<'static>,
    alloc: Alloc<'r>,
}

impl<'h, 'c, 'r> CommandFront<'h, 'c, 'r> {
    pub(crate) fn new(
        names: &'c mut dyn FnMut() -> Result<NamesRecords<'h>, ParentError>,
        ids: &'c mut dyn FnMut() -> Result<RunIds<'h>, ParentError>,
        builder: Builder,
        tables: Alloc<'static>,
        alloc: Alloc<'r>,
    ) -> CommandFront<'h, 'c, 'r> {
        CommandFront { names, ids, kept: None, builder, tables, alloc }
    }

    fn names(&mut self) -> Result<NamesRecords<'h>, ParentError> {
        if let Some(names) = self.kept {
            return Ok(names);
        }
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
        special::roots(names.records, names.frame, self.tables, self.alloc).map_err(ParentError::Roots)
    }

    fn ids(&mut self) -> Result<RunIds<'h>, ParentError> {
        (self.ids)()
    }

    fn maker(&mut self, start: &Roots<'r>) -> Result<Maker<'h>, ParentError> {
        let names = self.names()?;
        Ok(Maker::new(names, start.maker_entry(), &self.builder, self.tables))
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
    tally: Tally,
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
        Lazy { cx, front, folder, store, builder, alloc, started: None, placed: Vec::new_in(alloc), declares: Vec::new_in(alloc), ahead: Vec::new_in(alloc), tally: Tally::default() }
    }

    /// The entry line from the roots (made now if they are not yet).
    pub(crate) fn line(&mut self) -> Result<EntryLine, ParentError> {
        Ok(ensure(&mut self.started, &mut self.front, self.alloc)?.start.entry_line())
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

    /// `rows` placed ahead from today's start list.
    fn placed(&mut self, rows: &[Row]) {
        for row in rows {
            self.placed.push(row.symbol.0);
        }
        self.placed.sort_unstable();
        self.placed.dedup();
        self.tally.reached += rows.len();
        self.tally.found += rows.len();
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
                walk.root::<RunError>(start.root_row(i), MOST_WALKED).map_err(|why| ParentError::Walk { symbol: start.entry_line().symbol.0, why })?;
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
    Ok((how, key))
}

impl<F: Front, D: Folder> Resolve for Lazy<'_, '_, F, D> {
    fn resolve(&mut self, symbol: &Symbol) -> Result<Resolved<'_>, ParentError> {
        let Lazy { cx, front, folder, store, builder, alloc, started, placed, declares, ahead, tally } = self;
        let started = ensure(started, front, *alloc)?;
        let builder = *builder;
        let code = CodeSymbol(symbol.0);
        declares.clear();
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
            let id = folder.file(record)?;
            started.list.clear();
            front.form().declared(entry, &mut started.list).map_err(|why| ParentError::Entry { symbol: symbol.0, why })?;
            for i in 0..started.list.len() {
                let callee = started.walk.root::<RunError>(started.list.row(i), MOST_WALKED).map_err(|why| ParentError::Walk { symbol: symbol.0, why })?;
                declares.push(Symbol(started.walk.symbol(callee).0));
            }
            tally.reached += 1;
            match made.found {
                true => tally.found += 1,
                false => tally.made += 1,
            }
            return Ok(Resolved::Record { key: made.key, record: id, declares: declares.as_slice() });
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
        // A row stepped ahead is served once, from what its step answered; any other row that is
        // not waiting was served already.
        let (how, key, held) = if started.walk.how(row) == How::Waiting {
            let (how, key) = step(cx, front, started, &mut **store, &builder, row)?;
            (how, key, None)
        } else {
            let Some(i) = ahead.iter().position(|one| one.row == row) else { return Err(ParentError::ResolvedTwice { symbol: symbol.0 }) };
            let one = ahead.swap_remove(i);
            (one.how, one.key, one.bytes)
        };
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
        let id = folder.file(record)?;
        // An unkeyed instance's row in the next list: its entry's content key.
        let key = key.unwrap_or_else(|| content_key(entry));
        for &callee in started.walk.declared(row) {
            declares.push(Symbol(started.walk.symbol(callee as usize).0));
        }
        tally.reached += 1;
        match how {
            How::Found => tally.found += 1,
            How::Made => tally.made += 1,
            How::Unkeyed(_) | How::Waiting => tally.unkeyed += 1,
        }
        Ok(Resolved::Record { key, record: id, declares: declares.as_slice() })
    }
}

/// Before the spawn: today's start list (`today`, the rows under today's certificate) placed
/// onto `out`, each row whose record's file `lazy`'s folder does not hold left out (it is
/// resolved at its first call like any other), and the entry line: the list's when there is
/// one (nothing of the front end is made), else the roots'. `kept` holds the rows placed.
pub(crate) fn ahead_of<F: Front, D: Folder, A: Allocator>(
    today: Option<TodayRows<'_>>,
    lazy: &mut Lazy<'_, '_, F, D>,
    needs: &mut Needs<'_>,
    kept: &mut AVec<'_, Row>,
    out: &mut std::vec::Vec<u8, A>,
) -> Result<EntryLine, ParentError> {
    let Some(today) = today else { return lazy.line() };
    for row in today.iter() {
        if lazy.folder().holds(&row.record)? {
            kept.push(row);
        }
    }
    needs.place_ahead(kept.as_slice(), out)?;
    lazy.placed(kept.as_slice());
    Ok(today.entry())
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
    /// The next message's id.
    next_id: u64,
}

impl<'a> Needs<'a> {
    pub(crate) fn new(alloc: Alloc<'a>) -> Needs<'a> {
        Needs { met: ByHash::new_in(alloc), slots: Vec::new_in(alloc), pairs: Vec::new_in(alloc), first: start_list::Rows::new_in(alloc), next_id: 0 }
    }

    fn id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// This run's first answers with a record, in order (at most the start list's cap): the
    /// rows of the list written at exit.
    pub(crate) fn first(&self) -> &[Row] {
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

    /// A record answered for the first time: a row of the next start list while the window is
    /// open (its cap: past it the rows are not kept, and nothing fails).
    fn first_answer(&mut self, row: Row) -> Result<(), ParentError> {
        if self.first.is_full() {
            return Ok(());
        }
        self.first.push(row).map_err(ParentError::StartList)
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

    /// Today's start list placed before any need is read (run.md D124: "its rows are published
    /// as they are before `Begin`, nothing keyed or looked up"): every row's slot told in one
    /// `Declare`, then each row's `Publish`, in the list's order; each row is this run's first
    /// answer of its symbol, so a need of it later resolves nothing. `rows` are those whose
    /// record's file is there ([`ahead_of`]).
    pub(crate) fn place_ahead<A: Allocator>(&mut self, rows: &[Row], out: &mut std::vec::Vec<u8, A>) -> Result<(), ParentError> {
        for row in rows {
            let slot = self.slot_of(row.symbol)?;
            self.settle(row.symbol, Answer::Record { record: row.record, slot });
            self.first_answer(*row)?;
        }
        self.declare(out)?;
        for row in rows {
            let Some(Seen { answer: Answer::Record { record, slot }, .. }) = self.met.get(&row.symbol.0).copied() else {
                return Err(ParentError::UndeclaredSymbol { symbol: row.symbol.0 });
            };
            let id = self.id();
            Down::Publish { slot, tier: Tier::Zero, record }.encode(id, out)?;
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
        let (record, slot) = match self.meet(symbol)?.answer {
            Answer::Record { record, slot } => (record, slot),
            Answer::Unknown => return self.failed(need, out),
            Answer::NotAsked => match resolve.resolve(&symbol)? {
                Resolved::Unknown => {
                    self.settle(symbol, Answer::Unknown);
                    return self.failed(need, out);
                }
                Resolved::Record { key, record, declares } => {
                    let slot = self.slot_of(symbol)?;
                    for each in declares {
                        self.slot_of(*each)?;
                    }
                    self.settle(symbol, Answer::Record { record, slot });
                    self.first_answer(Row { symbol, key, record })?;
                    (record, slot)
                }
            },
        };
        self.declare(out)?;
        let id = self.id();
        Down::Publish { slot, tier: Tier::Zero, record }.encode(id, out)?;
        Ok(())
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
