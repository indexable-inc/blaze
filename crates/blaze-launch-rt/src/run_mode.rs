//! The launcher's run mode (docs/v2/plan/design/run.md, "End state (corrected 2026-11-16)",
//! piece 5; bottom.md section 11): the program's process under `blaze run`, its parent on the
//! socket pair at descriptor 3, its code placed one record at a time on first call.
//!
//! No executable is written and nothing is compiled before the first instruction. The launcher
//! numbers the entry's slot, points its word at its stub, and calls it; the stub fires, the
//! control thread asks the parent (`Up::Need`, blaze-launch wire/run.rs), and the parent answers
//! with the slots it numbers (`Down::Declare`) and the record to place (`Down::Publish`), or why
//! there is none (`Down::Failed`). A record's targets are named by symbol; placing it completes
//! every site against the run's slot words (blaze-code `place::place`), registers its unwind
//! entry, and only then stores the slot's word (crate::code_unwind::publish's order). Every
//! thread waiting in the stub is then woken to load the word again.
//!
//! **Statics** (run.md, "Statics in the run"): a static has no stub, so the parent sends each
//! one a record names (`Down::Data`, its symbol and datum record) before that record. One
//! message is one batch ([`Act::Statics`], [`RunMode::place_statics`]): every datum in it is
//! read and given its room first, so a word naming another static of the batch resolves; then
//! each is filled (blaze-code `datum::fill`), copied, and only then its word stored
//! ([`Memory::publish_static`]). A read-only datum's room is in the code area, read-only in the
//! run view as a relro part is. A code record's load of a static reads that word.
//!
//! Two parts:
//!
//! - [`RunMode`], sans-IO: events in ([`Event`]), actions out ([`Act`]), the wire's bytes in an
//!   outbox the driver writes. The first-call machine is blaze-run's
//!   [`blaze_run::first_call::FirstCall`] (one ask, one placement, however many threads fire one
//!   stub). [`RunMode::place`] does one placement through the [`Memory`] seam, so the simulation
//!   (run_mode/tests.rs) and the process run the same order.
//! - [`run`], the thin driver: the one mapping (slot words, address words, outside words, stubs,
//!   entry thunks, the code area: one reservation, so every `adrp` reaches), the control thread,
//!   the socket's reader thread, and the program's entry called on the process's first thread.
//!
//! **Slots.** The child numbers its own slots as symbols first appear (FirstCall's rule); the
//! parent numbers its own in `Declare`. Each local slot keeps the parent's number once declared
//! ([`RunMode`]'s two maps), and a need names the parent's slot when there is one, else the
//! symbol (`Needed::Symbol`). The entry is numbered first: slot 0.
//!
//! **What is refused by name**, never guessed (rule 3): a record at the optimized tier for a slot
//! whose fast code is not bound yet (a bound one is promoted: [`Act::Promote`]); a target that is a vtable or a
//! thread-local, or a writable static (run.md, "Statics in the run", steps 3 and 4); a record naming a
//! static the parent did not send before it ([`RunModeError::StaticNotTold`]); a record whose pads land placed
//! before the parent's `Down::Personality` named the program's personality routine
//! ([`EhError::NoPersonality`]; run_mode/personality.rs); an outside name the run's binder
//! does not bind, naming the record, the symbol and the name ([`RunModeError::Outside`]); an
//! entry form none of `unit`, `c-main`, `hosted` and `comptime` ([`Start::parse`]); run
//! mode on x86-64 (code records are AArch64). A parent's `Failed` stops the program with
//! [`STOPPED`] (D86) naming the function.
//!
//! **C names** (THE BINDER SEAM, run_mode/bind.rs): a record's `Space::Outside` target, from a
//! call, a load of its word or a datum's word, resolves first to what the program defines (a
//! declared slot or a told static of the name's own symbol), then through the run's strategy,
//! a type parameter: [`Dlsym`] for an ordinary run, [`Table`] (blaze-comptime-sys) for a
//! compile-time one.
//!
//! **A hosted entry** (`hosted:<symbol>`, bstd's `bstd_hosted_start`; run.md, "Every program
//! runs in the child") is called with bstd's host table, which the launcher makes in this
//! process over its own world before [`run`] ([`Handed::Hosted`]); this crate never names bstd.
//!
//! **A compile-time entry** (`comptime:<symbol>`, D175) is called as C `main` with the
//! compile-time host's arguments ([`Handed::Comptime`]), its C names bound through the table,
//! and [`run`] returns its status ([`End::Return`]): the launcher owns the process (G5).
//!
//! **TO VERIFY BY A TEST** (listed in the piece's report): (1) the two-view code area
//! (`sys::Code`) runs placed code under the launcher's current signature (run.md, "Executable
//! memory": ad hoc signed runs, the hardened runtime kills it); (2) an unwind entry's length does
//! not depend on the addresses it is made for (eh.rs writes every address as an 8-byte absptr),
//! which [`RunMode::place`] relies on to size the entry before it has its address; (3) the
//! hello-world rung's entry is `extern "C" fn()`, C `main(argc, argv)` or bstd's hosted
//! `extern "C" fn(*const HostTable) -> i32`, the three forms [`EntryForm`] has.
//!
//! The simulation (run_mode/tests.rs) prints "run_mode: one need, one placement; plant
//! StoreBeforeRegister caught".

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use blaze_code::datum::{self, DatumError, DatumView, FillError, Place as DatumPlace};
use blaze_code::eh::{self, EhError, Entry, Placed};
use blaze_code::image::ImageError;
use blaze_code::place::{At, PlaceError, Words};
use blaze_code::record::{CodeView, DecodeError, Space, Symbol as CodeSymbol, Target};
use blaze_code::unwind::UnwindError;
use blaze_launch::observe::{Kind as ObserveKind, Observe, ObserveWrite, Slotword};
use blaze_launch::wire::run::{Declared, Down, Failure, HookKind, Needed, Symbol as WireSymbol, Up};
use blaze_launch::wire::{RecordId, WireError, split};
use blaze_launch::{Slot, Thread, Tier};
use blaze_own::{ADeque, AVec, Alloc, Vec, VecDeque};
use blaze_run::first_call::{self, Do, FirstCall, Woken};
use blaze_run::{CodeRef, FindingsRef, Nanos, SlotId, ThreadId, Why};

use crate::code_unwind::{self, Step};
use crate::native_lib::{NativeError, PATH_MOST, RecordFiles};

/// The exit status of a program blaze stopped: it could not build or start it, or stopped it
/// at a function it cannot build (decision D86: what `env` and `timeout` use for the tool's
/// own failure; Rust's 101 stays the program's panic).
pub const STOPPED: i32 = 125;

/// The most slots a run's reservation holds: words, stubs and thunks for each.
pub const MOST_SLOTS: u32 = 1 << 20;
/// The most C names one run binds (`dlsym`), each a word.
pub const MOST_OUTSIDE: u32 = 1 << 14;
/// The most statics one run places, each a word holding its address (run.md, "Statics in the
/// run").
pub const MOST_STATICS: u32 = 1 << 16;
/// The most threads waiting in stubs at once.
pub const MOST_WAITERS: u32 = 1 << 12;
/// How long an ask may go unanswered before its waiters fail (rule 18): the parent's own
/// compile has a shorter deadline, so this fires only when the parent is stuck.
pub const ASK_BOUND: Nanos = Nanos(300_000_000_000);
/// The reader thread's wait for the parent's bytes: each one that passes gives the control
/// thread a tick, so its own wait is bounded by this.
pub const TICK_MS: i32 = 250;

/// Where code records come from when the parent files them: the store's folder the parent wrote
/// them into, read in place (today's control). A run whose parent sends them inline
/// (`Down::Bytes`) reads none from here (run_mode/received.rs: one run reads one way).
pub trait RecordSource {
    /// The bytes of record `id`, whole, checked to be that record's (once, when first read).
    fn bytes(&self, id: RecordId) -> Result<&[u8], RunModeError>;

    /// Reads record `id` into `scratch` when backed by files, or borrows directly when in-memory.
    /// Avoids leaking a permanent `mmap` VMA per loaded function record.
    fn read_into<'a>(&'a self, id: RecordId, scratch: &'a mut std::vec::Vec<u8>) -> Result<&'a [u8], RunModeError> {
        let _ = scratch;
        self.bytes(id)
    }
}

/// What the run mode is told.
#[derive(Clone, Copy, Debug)]
pub enum Event<'e> {
    /// Bytes the parent wrote, as the socket gave them (any part of any number of frames).
    Frame(&'e [u8]),
    /// `thread` called `slot` and waits in its stub.
    Fired { thread: Thread, slot: SlotId },
    /// The driver's clock reads this.
    Tick(Nanos),
    /// The parent's socket ended.
    Closed,
}

/// Why the program is stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stop {
    /// `symbol`'s function, called through `slot`, has no code, for `why` (for `NotBuilt`, the
    /// parent's words are [`RunMode::parent_said`]).
    Failed { slot: SlotId, symbol: CodeSymbol, why: Why },
    /// A thread needs code and the parent is gone.
    ParentGone { waiting: usize },
}

/// One thing for the driver to do, in the order given.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Act {
    /// A slot is new: its words are made to name its stub ([`Memory::stub`]).
    Stub { slot: SlotId },
    /// Place `record` as `slot`'s body ([`RunMode::place`]).
    Place { slot: SlotId, record: RecordId },
    /// Place `record`, `slot`'s optimized code, in the hot region and publish it by one store of
    /// the slot word ([`RunMode::promote`]); no thread waits on it.
    Promote { slot: SlotId, record: RecordId },
    /// Wake `thread`, waiting in `slot`'s stub, to load the word again: it names the body.
    Wake { thread: Thread, slot: SlotId },
    /// End the process with [`STOPPED`], saying why.
    Stop(Stop),
    /// Load the program's native library `record` ([`RunMode::load_native`]): its slots are
    /// stubbed, its words and pointer cells filled; its init list goes to the first thread.
    Native { record: RecordId },
    /// Place the statics the parent's last `Data` told, together ([`RunMode::place_statics`]):
    /// before the acts after it, so before any record that names them.
    Statics,
    /// Make the run's thread-local descriptor and key, as the parent's `ThreadLocals` said
    /// ([`RunMode::make_thread_locals`]): before any record naming a thread-local.
    ThreadLocals,
    /// Toggle a live observe stub on or off for `slot` (`Down::Hooks`): installs/removes the
    /// stub and automatically retreats/re-promotes any optimized caller that inlined `slot`.
    Hook { slot: SlotId, kind: HookKind },
}

/// Where a record's code is placed: the area in first-call order, or the hot region in the order
/// promotions come (D100 point 13).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Region {
    Area,
    Hot,
}

/// A safeguard taken out, for the simulation's planted-bug control (rule 43).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// The slot's word is stored before the record's unwind entry is registered.
    StoreBeforeRegister,
}

/// A C name as an error keeps it: its first [`CName::MOST`] bytes, cut at a character, and
/// whether it was cut. An error is `Copy` and outlives the record whose bytes name it, so it
/// holds the bytes, not a borrow; [`CName::NONE`] where the record keeps no name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CName {
    bytes: [u8; CName::MOST],
    len: u8,
    cut: bool,
}

impl CName {
    /// The most bytes of a name an error keeps: every C name a std program binds is shorter.
    pub const MOST: usize = 96;
    /// No name: the record keeps none for the symbol.
    pub const NONE: CName = CName { bytes: [0; CName::MOST], len: 0, cut: false };

    /// `name`'s first [`CName::MOST`] bytes, cut back to a character boundary.
    pub fn new(name: &str) -> CName {
        let mut len = name.len().min(CName::MOST);
        while !name.is_char_boundary(len) {
            len -= 1;
        }
        let mut bytes = [0u8; CName::MOST];
        bytes[..len].copy_from_slice(&name.as_bytes()[..len]);
        CName { bytes, len: len as u8, cut: len < name.len() }
    }
}

impl fmt::Display for CName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.len == 0 && !self.cut {
            return f.write_str("(unnamed)");
        }
        // The bytes are a prefix of a `&str` cut at a character boundary (`CName::new`).
        let text = core::str::from_utf8(&self.bytes[..usize::from(self.len)]).map_err(|_| fmt::Error)?;
        write!(f, "`{text}{}`", if self.cut { "..." } else { "" })
    }
}

/// A C name bound for a run: the word a site loads, and the address it holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Outside {
    pub word: u64,
    pub value: u64,
}

/// One record on its way in: what [`Memory::carry`] copies, registers and stores.
#[derive(Clone, Copy, Debug)]
pub struct Placing<'p> {
    pub slot: SlotId,
    pub code_at: u64,
    pub code: &'p [u8],
    pub relro_at: u64,
    pub relro: &'p [u8],
    /// The unwind entry's address, bytes and parts; `None` for a record with no frame
    /// description (blaze-code unwind.rs: the frame word 0 has nothing to unwind through).
    pub entry: Option<(u64, &'p [u8], Entry)>,
}

/// The run's memory, as the driver has it: the slot words, their stubs and thunks, the C
/// names' words, the code area. The simulation's impl records what a reader would see.
pub trait Memory {
    /// Writes `slot`'s stub and entry thunk, then stores its word (the stub's address) and its
    /// address word (the thunk's): before any placed code names the slot.
    fn stub(&mut self, slot: SlotId) -> Result<(), RunModeError>;
    /// The address of `slot`'s word: what a call loads.
    fn word(&self, slot: SlotId) -> u64;
    /// The address of the word holding `slot`'s entry thunk: what a load of the function's
    /// address reads.
    fn address_word(&self, slot: SlotId) -> u64;
    /// The address of `slot`'s entry thunk: the function's address.
    fn thunk(&self, slot: SlotId) -> u64;
    /// Binds the C name `name` of symbol `symbol`, which record `record` names, through the
    /// run's strategy (run_mode/bind.rs; once a run; again gives the same). When `weak` is true
    /// (`Space::WeakOutside`), a missing symbol (`BindError::NotFound`) binds to 0 (null) instead
    /// of failing. A C name the program defines never comes here: the binder resolves it to its
    /// slot or static first.
    fn outside(&mut self, record: RecordId, symbol: CodeSymbol, name: &str, weak: bool) -> Result<Outside, RunModeError>;
    /// `len` bytes of the code area on `align`: their run-view address.
    fn reserve(&mut self, len: usize, align: usize) -> Result<u64, RunModeError>;
    /// `len` bytes of the hot region on `align`, where promoted code is laid out in the order the
    /// parent publishes it (its sampled call edges' order, blaze-run promote.rs).
    fn reserve_hot(&mut self, len: usize, align: usize) -> Result<u64, RunModeError>;
    /// One step of a publication (code_unwind's order).
    fn carry(&mut self, step: Step, placing: &Placing<'_>) -> Result<(), RunModeError>;
    /// `len` bytes on `align` of the run's writable data area, every byte zero: read-write in
    /// the run view, for the statics the program writes (`static mut`, interior mutability).
    /// No code is placed there and no site reaches it by `adrp`: code finds a static by its
    /// word, so the area may lie anywhere.
    fn reserve_data(&mut self, len: usize, align: usize) -> Result<u64, RunModeError>;
    /// The address of static number `index`'s word: what a load of the static's address reads.
    fn static_word(&self, index: u32) -> u64;
    /// Copies static number `index`'s filled `bytes` to `at` (room [`Memory::reserve`] gave for
    /// [`Room::Code`], [`Memory::reserve_data`] for [`Room::Data`]), then stores `at` in its
    /// word: nothing names the static before both hold.
    fn publish_static(&mut self, index: u32, at: u64, bytes: &[u8], room: Room) -> Result<(), RunModeError>;
    /// Makes the run's one pthread key with the function at `ender` (the ender's entry thunk)
    /// as its destructor: the key and the descriptor's `root` word (run_mode/thread_locals.rs).
    fn thread_key(&mut self, ender: u64) -> Result<ThreadKey, RunModeError>;
    /// Copies `bytes` to `at`, inside room [`Memory::reserve`] gave, before any record naming
    /// them is published: a thread-local's row, the descriptor's count, an index word.
    fn write(&mut self, at: u64, bytes: &[u8]) -> Result<(), RunModeError>;
    /// Told after each publication: what now lies at `row.at`, for the crash report
    /// (run_mode/fault_report.rs). The native memory keeps it; a simulation has no crash to
    /// report, and its memory keeps nothing (the one method with a body here: an observer).
    fn note(&mut self, row: fault_report::Row) {
        let _ = row;
    }
    /// Ensures an observe stub of `kind` exists for `slot` and returns its run-view address.
    /// Emitted once per `(slot, kind)`; its body word is updated separately by
    /// [`Memory::store_body_word`].
    fn ensure_observe_stub(&mut self, slot: SlotId, kind: ObserveKind) -> Result<u64, RunModeError> {
        let _ = (slot, kind);
        Err(RunModeError::NotBuilt { what: "observe stubs on this memory implementation" })
    }
    /// Atomically stores `body` into the body word of every observe stub emitted for `slot`.
    fn store_body_word(&mut self, slot: SlotId, body: u64) -> Result<(), RunModeError> {
        let _ = (slot, body);
        Ok(())
    }
    /// Atomically stores `target` (a plain body address or an observe stub address) into
    /// `slot`'s slot word.
    fn store_slot_word(&mut self, slot: SlotId, target: u64) -> Result<(), RunModeError> {
        let _ = (slot, target);
        Ok(())
    }
}

/// Where a static's datum lies (blaze-code datum.rs `Place`): never written, in the code area
/// (read-only in the run view, written once through the write view); written by the program,
/// in the writable data area.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Room {
    Code,
    Data,
}

/// Why the run mode stopped, naming the thing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunModeError {
    /// The parent's bytes are no message of the run wire.
    Wire(WireError),
    /// The first-call machine refused an answer: the wire is out of step.
    FirstCall(first_call::Error),
    /// A placement was carried out of order with the inputs supplied by its publication.
    PositionOrder { placement: Act, queued: Option<Act> },
    /// The parent named a slot it never declared.
    Undeclared { slot: Slot },
    /// The parent declared `slot` for `symbol`, which runs through another slot already, or
    /// declared one slot twice for two symbols.
    Redeclared { slot: Slot, symbol: WireSymbol },
    /// A slot number past what the run's reservation holds.
    SlotPast { slot: Slot, most: u32 },
    /// The parent failed a need nobody asked.
    FailedUnasked { symbol: WireSymbol },
    /// Something the run mode does not do yet, by name.
    NotBuilt { what: &'static str },
    /// A record's target of a kind the run mode does not bind yet.
    Target { record: RecordId, space: Space, symbol: CodeSymbol },
    /// A record names static `symbol`, which no `Data` message sent before it.
    StaticNotTold { record: RecordId, symbol: CodeSymbol },
    /// The parent sent static `symbol` twice.
    StaticTwice { symbol: WireSymbol },
    /// More statics than [`MOST_STATICS`].
    StaticsFull,
    /// A static's datum record does not read.
    Datum { record: RecordId, why: DatumError },
    /// A static's datum could not be filled at its address.
    Fill { record: RecordId, why: FillError },
    /// The store has no record `record`, or its bytes do not read.
    Missing { record: RecordId, errno: i32 },
    /// The bytes read as `record` hash to `got`.
    Hash { record: RecordId, got: [u8; 32] },
    /// The store's folder path with a record's name is longer than a path holds.
    StorePath { len: usize },
    /// The bytes are not a code record.
    Decode { record: RecordId, why: DecodeError },
    /// The record's unwind rows are refused.
    Unwind { record: RecordId, why: UnwindError },
    /// The record's unwind entry could not be made.
    Eh { record: RecordId, why: EhError },
    /// The record could not be placed.
    Place { record: RecordId, why: PlaceError },
    /// A C name record `record` calls or reads: unnamed in the record, too long, not in this
    /// process, or (a compile-time run) not in blaze-comptime-sys's table, refused at bind time
    /// (run_mode/bind.rs). `name` is the C name as far as the record gave it ([`CName`]): the
    /// words name it, so a stop at `_tlv_atexit` says `_tlv_atexit`, not a hash.
    Outside { record: RecordId, symbol: CodeSymbol, name: CName, why: &'static str },
    /// The parent declared or told `symbol` (a C name the program defines) after a record's
    /// C name of that symbol was already bound by the run's strategy: two answers for one name
    /// in one run. Refused: the parent declares every program-defined C name before its first
    /// record (blaze-cli run_parent.rs `Needs::declare_c_defined`).
    DefinedAfterBound { symbol: CodeSymbol },
    /// More C names than [`MOST_OUTSIDE`].
    OutsideFull,
    /// The code area has no room for `want` more bytes.
    AreaFull { want: u64, left: u64 },
    /// The writable data area could not be mapped (`mmap`'s `errno`).
    DataMap { errno: i32 },
    /// The writable data area has no room for a static of `want` bytes.
    DataFull { want: u64, left: u64 },
    /// A writable static's `len` bytes at `at` are outside the room the data area gave.
    DataPast { at: u64, len: u64 },
    /// A call of the system failed.
    Sys(crate::sys::SysError),
    /// The launcher's own code cannot reach what it must.
    Reach(crate::arch::OutOfReach),
    /// A call on the parent's socket failed (`errno`), in the wait or call named.
    Socket { call: &'static str, errno: i32 },
    /// A thread of the launcher's could not start.
    Thread { what: &'static str },
    /// The parent sent a second `Personality` (this one naming `slot`): an entry names one
    /// personality routine for the run.
    PersonalityTwice { slot: Slot },
    /// The run's unwind registry could not be made, installed or written (unwind_reg.rs).
    Unwinder(crate::unwind_reg::RegError),
    /// A record names C name `symbol` before the program's native library was loaded: names
    /// bind through it only (NATIVE.md 3.7).
    NoNativeLibrary { symbol: CodeSymbol },
    /// The program's native library could not be loaded (native_lib.rs).
    Native(NativeError),
    /// The parent sent a second `ThreadLocals`: a run has one descriptor and one key.
    ThreadLocalsTwice,
    /// Record `record` names thread-local `symbol` before the parent's `ThreadLocals`.
    ThreadLocalsUntold { record: RecordId, symbol: CodeSymbol },
    /// More thread-locals than the descriptor's rows (`blaze_code::tls::CAP`).
    ThreadLocalsFull,
    /// `pthread_key_create` failed with `errno`.
    ThreadKey { errno: i32 },
    /// The thread-local root's offset uses bit 63, which the access sequence clears.
    TlsRoot { offset: u64 },
    /// The parent's inline bytes for `record` (`Down::Bytes`) are refused, for `why`
    /// (run_mode/received.rs).
    Sent { record: RecordId, why: SentRefused },
    /// An uncaught guest exception unwound out of the program's entry under [`End::Return`].
    Unwound,
}

impl From<WireError> for RunModeError {
    fn from(why: WireError) -> RunModeError {
        RunModeError::Wire(why)
    }
}

impl From<first_call::Error> for RunModeError {
    fn from(why: first_call::Error) -> RunModeError {
        RunModeError::FirstCall(why)
    }
}

/// The first six bytes of an id in hex: how a message names a record or a symbol.
struct Short<'b>(&'b [u8; 32]);

impl fmt::Display for Short<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0[..6].iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::Display for RunModeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            RunModeError::Wire(why) => write!(f, "the parent's message does not read: {why}"),
            RunModeError::FirstCall(why) => write!(f, "{why}"),
            RunModeError::PositionOrder { placement, queued } => write!(
                f, "source positions for {placement:?} were requested with {queued:?} next in publication order"
            ),
            RunModeError::Undeclared { slot } => {
                write!(f, "the parent named slot {}, which it never declared (run wire, `Declare`)", slot.0)
            }
            RunModeError::Redeclared { slot, symbol } => write!(
                f,
                "the parent declared slot {} for function {}, and one of them already runs through another (a slot is fixed for the run)",
                slot.0,
                Short(&symbol.0)
            ),
            RunModeError::SlotPast { slot, most } => {
                write!(f, "slot {} is past the {most} slots the run's reservation holds (run.md, \"Slot table\")", slot.0)
            }
            RunModeError::FailedUnasked { symbol } => {
                write!(f, "the parent failed function {}, which this program never asked for", Short(&symbol.0))
            }
            RunModeError::NotBuilt { what } => write!(f, "the launcher's run mode does not do this yet: {what}"),
            RunModeError::Target { record, space, symbol } => write!(
                f,
                "record {} names {space:?} {symbol} where the run mode binds no such target: functions, C names, statics, vtables and a load of a thread-local's index are bound; a call of a static, a vtable or a thread-local, and a thread-local's address in a static's value are not (run.md, \"Statics in the run\")",
                Short(&record.0)
            ),
            RunModeError::StaticNotTold { record, symbol } => write!(
                f,
                "record {} names static {symbol}, which the parent did not send before it (run wire `Data` comes before the record that names a static: run.md, \"Statics in the run\")",
                Short(&record.0)
            ),
            RunModeError::StaticTwice { symbol } => {
                write!(f, "the parent sent static {} a second time: a static is one allocation for the whole program (run wire, `Data`)", Short(&symbol.0))
            }
            RunModeError::StaticsFull => write!(f, "the program places more than {MOST_STATICS} statics (the run's reservation, run.md \"Slot table\")"),
            RunModeError::Datum { record, why } => write!(f, "static record {}: {why}", Short(&record.0)),
            RunModeError::Fill { record, why } => write!(f, "static record {}: {why}", Short(&record.0)),
            RunModeError::Missing { record, errno } => {
                write!(f, "the store has no readable record {} (errno {errno})", Short(&record.0))
            }
            RunModeError::Hash { record, got } => {
                write!(f, "the store's bytes for record {} hash to {}", Short(&record.0), Short(&got))
            }
            RunModeError::StorePath { len } => {
                write!(f, "the store's folder with a record's name is {len} bytes, longer than a path the launcher holds")
            }
            RunModeError::Decode { record, why } => write!(f, "record {}: {why}", Short(&record.0)),
            RunModeError::Unwind { record, why } => write!(f, "record {}: {why}", Short(&record.0)),
            RunModeError::Eh { record, why } => write!(f, "record {}: {why}", Short(&record.0)),
            RunModeError::Place { record, why } => write!(f, "record {}: {why}", Short(&record.0)),
            RunModeError::Outside { record, symbol, name, why } => {
                write!(f, "record {} names the C name {name} (symbol {symbol}): {why}", Short(&record.0))
            }
            RunModeError::OutsideFull => write!(f, "the program binds more than {MOST_OUTSIDE} C names"),
            RunModeError::DefinedAfterBound { symbol } => write!(
                f,
                "the parent declared the program's own C name of symbol {symbol} after a record's use of that name was bound outside the program: every program-defined C name is declared before the first record"
            ),
            RunModeError::AreaFull { want, left } => {
                write!(f, "the code area has {left} bytes left and a record needs {want} (the run's reservation, run.md \"Slot table\")")
            }
            RunModeError::DataMap { errno } => write!(f, "the run's writable data area ({DATA_BYTES} bytes) could not be mapped: errno {errno}"),
            RunModeError::DataFull { want, left } => {
                write!(f, "the run's writable data area has {left} bytes left and a static the program writes needs {want}")
            }
            RunModeError::DataPast { at, len } => {
                write!(f, "a writable static's {len} bytes at {at:#x} are outside the room the run's data area gave")
            }
            RunModeError::Sys(why) => write!(f, "{why}"),
            RunModeError::Reach(why) => write!(f, "{why}"),
            RunModeError::Socket { call, errno } => write!(f, "{call} on the parent's socket failed with errno {errno}"),
            RunModeError::Thread { what } => write!(f, "the launcher's {what} thread did not start"),
            RunModeError::PersonalityTwice { slot } => write!(
                f,
                "the parent named the personality routine a second time (slot {}): an unwind entry names one routine for the run (run wire, `Personality`)",
                slot.0
            ),
            RunModeError::Unwinder(why) => write!(f, "the run's unwind registry: {why}"),
            RunModeError::NoNativeLibrary { symbol } => write!(
                f,
                "the C name of symbol {symbol} is asked before the program's native library was loaded (NATIVE.md 3.7: the parent's `Native` comes first)"
            ),
            RunModeError::Native(why) => write!(f, "{why}"),
            RunModeError::ThreadLocalsTwice => write!(
                f,
                "the parent named the thread-local descriptor a second time: a run has one descriptor and one key (run wire, `ThreadLocals`)"
            ),
            RunModeError::ThreadLocalsUntold { record, symbol } => write!(
                f,
                "record {} names thread-local {symbol} before the parent named the run's thread-local descriptor (run wire `ThreadLocals` comes before the record: run.md, \"Statics in the run\", step 4)",
                Short(&record.0)
            ),
            RunModeError::ThreadLocalsFull => {
                write!(f, "the program touches more than {} thread-locals (the descriptor's rows, blaze-code tls.rs)", blaze_code::tls::CAP)
            }
            RunModeError::ThreadKey { errno } => write!(f, "the run's thread-local key: pthread_key_create failed with errno {errno}"),
            RunModeError::TlsRoot { offset } => {
                write!(f, "the run's thread-local root is at offset {offset:#x}, which bit 63 of the descriptor's root cannot say")
            }
            RunModeError::Sent { record, why } => {
                write!(f, "the parent's bytes for record {} (run wire `Bytes`): {why}", Short(&record.0))
            }
            RunModeError::Unwound => {
                write!(f, "an uncaught panic unwound out of the program's entry")
            }
        }
    }
}

impl core::error::Error for RunModeError {}

/// No slot: the value an empty row of a slot map holds.
const NONE: u32 = u32::MAX;

/// Append-only open-addressed O(1) map keyed by 32-byte BLAKE3 hashes (`Symbol` / `RecordId`).
/// Keys are cryptographic digests so the first 8 bytes are already uniformly distributed.
pub(super) struct HashIndexMap<'a, V> {
    entries: AVec<'a, ([u8; 32], V)>,
    /// Power-of-two table; 0 = empty, else `entry_index + 1`. Load factor <= 0.5.
    slots: AVec<'a, u32>,
    alloc: Alloc<'a>,
}

impl<'a, V> HashIndexMap<'a, V> {
    pub(super) fn new_in(alloc: Alloc<'a>) -> Self {
        Self { entries: Vec::new_in(alloc), slots: Vec::new_in(alloc), alloc }
    }

    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn slot_of(key: &[u8; 32], mask: usize) -> usize {
        let head = u64::from_le_bytes([key[0], key[1], key[2], key[3], key[4], key[5], key[6], key[7]]);
        let mask_u64 = u64::try_from(mask).unwrap_or(u64::MAX);
        usize::try_from(head & mask_u64).unwrap_or(0)
    }

    fn find(&self, key: &[u8; 32]) -> Result<usize, usize> {
        let mask = self.slots.len().saturating_sub(1);
        let mut at = Self::slot_of(key, mask);
        loop {
            match self.slots.get(at).copied() {
                None | Some(0) => return Err(at),
                Some(n) => {
                    let idx = usize::try_from(n - 1).unwrap_or(usize::MAX);
                    if self.entries.get(idx).is_some_and(|entry| entry.0 == *key) {
                        return Ok(idx);
                    }
                    at = (at + 1) & mask;
                }
            }
        }
    }

    fn grow(&mut self) {
        let len = (self.slots.len() * 2).max(16);
        let mut slots: AVec<'a, u32> = Vec::with_capacity_in(len, self.alloc);
        slots.resize(len, 0);
        self.slots = slots;
        for idx in 0..self.entries.len() {
            let Some(key) = self.entries.get(idx).map(|entry| entry.0) else {
                continue;
            };
            if let Err(at) = self.find(&key)
                && let Some(slot) = self.slots.get_mut(at)
            {
                *slot = u32::try_from(idx + 1).unwrap_or(u32::MAX);
            }
        }
    }

    pub(super) fn contains_key(&self, key: &[u8; 32]) -> bool {
        !self.slots.is_empty() && self.find(key).is_ok()
    }

    pub(super) fn get(&self, key: &[u8; 32]) -> Option<&V> {
        if self.slots.is_empty() {
            return None;
        }
        let idx = self.find(key).ok()?;
        self.entries.get(idx).map(|entry| &entry.1)
    }

    /// Inserts `(key, value)` if absent. Returns `true` if inserted, `false` if already present.
    pub(super) fn insert(&mut self, key: [u8; 32], value: V) -> bool {
        if (self.entries.len() + 1) * 2 > self.slots.len() {
            self.grow();
        }
        match self.find(&key) {
            Ok(_) => false,
            Err(at) => {
                let idx = self.entries.len();
                self.entries.push((key, value));
                if let Some(slot) = self.slots.get_mut(at) {
                    *slot = u32::try_from(idx + 1).unwrap_or(u32::MAX);
                }
                true
            }
        }
    }
}

/// The run's slots: the first-call machine, each local slot's symbol, and the parent's numbers.
struct Numbering<'a> {
    first: FirstCall<'a>,
    /// By local slot: its symbol.
    symbols: AVec<'a, CodeSymbol>,
    /// By local slot: the parent's slot, or [`NONE`].
    to_wire: AVec<'a, u32>,
    /// By the parent's slot: the local slot, or [`NONE`].
    to_local: AVec<'a, u32>,
    /// Every symbol the parent declared, with its local slot: how a C name the program
    /// defines (its symbol `Symbol::of_name`, run_mode/bind.rs) is found in O(1) without numbering it.
    declared: HashIndexMap<'a, SlotId>,
    /// Every C name symbol the run's strategy bound: a later declaration or static of
    /// one is [`RunModeError::DefinedAfterBound`], so no name has two answers in one run.
    bound_outside: HashIndexMap<'a, ()>,
    alloc: Alloc<'a>,
}

impl<'a> Numbering<'a> {
    /// The local slot of `symbol`, and whether it is new (its words must be made to name its
    /// stub before anything names it).
    fn number(&mut self, symbol: CodeSymbol) -> Result<(SlotId, bool), RunModeError> {
        let mut dos: AVec<'_, Do> = Vec::new_in(self.alloc);
        let slot = self.first.slot_of(first_call::Symbol(symbol.0), &mut dos)?;
        let new = slot.index() == self.symbols.len();
        if new {
            self.symbols.push(symbol);
            self.to_wire.push(NONE);
        }
        Ok((slot, new))
    }

    fn symbol(&self, slot: SlotId) -> CodeSymbol {
        self.symbols[slot.index()]
    }

    /// The local slot of `symbol` when the parent declared it; `None` numbers nothing.
    fn declared(&self, symbol: CodeSymbol) -> Option<SlotId> {
        self.declared.get(&symbol.0).copied()
    }

    fn wire(&self, slot: SlotId) -> Option<Slot> {
        match self.to_wire[slot.index()] {
            NONE => None,
            wire => Some(Slot(wire)),
        }
    }

    fn local(&self, slot: Slot) -> Result<SlotId, RunModeError> {
        match self.to_local.get(slot.0 as usize) {
            Some(&local) if local != NONE => Ok(SlotId(local)),
            _ => Err(RunModeError::Undeclared { slot }),
        }
    }
}

/// The run mode's state. See the module's documentation.
pub struct RunMode<'a> {
    slots: Numbering<'a>,
    /// The parent's bytes not yet a whole frame.
    inbox: AVec<'a, u8>,
    /// One input per emitted placement, including ordinary publications with zero rows and
    /// optional inlined callee slots for optimized promotions.
    /// A repeated answer that FirstCall coalesces has no placement and takes no entry.
    position_inputs: ADeque<'a, (Act, AVec<'a, u8>, AVec<'a, Slot>)>,
    /// The unified two-word observe + inlining-deoptimization state machine for every slot.
    observe: Observe<Alloc<'a>>,
    /// Frames for the parent, written by the driver ([`RunMode::outbox`]). The wire's encoder
    /// writes onto `alloc`'s vector, so this one is that type, in the run's allocator.
    outbox: std::vec::Vec<u8, Alloc<'a>>,
    /// The parent's words in its last `Failed { NotBuilt }`.
    said: AVec<'a, u8>,
    next_id: u64,
    now: Nanos,
    closed: bool,
    stopped: bool,
    /// The local slot of the program's `eh_personality` fn, once the parent's `Personality`
    /// named it (run_mode/personality.rs); every entry made after names its entry thunk.
    personality: Option<SlotId>,
    /// The program's native library: waiting for the parent's `Native`, declared, or loaded
    /// (run_mode/native.rs).
    native: native::NativeState<'a>,
    /// Every static placed or being placed, indexed by symbol: its word's number and its address.
    statics: HashIndexMap<'a, StaticAt>,
    /// The statics the parent told and [`Act::Statics`] has not placed yet, in its order.
    told: AVec<'a, (CodeSymbol, RecordId)>,
    /// The run's thread-local descriptor and indexes (run_mode/thread_locals.rs).
    thread_locals: thread_locals::ThreadLocals<'a>,
    /// The records the parent sent inline, assembled, checked and held for the run
    /// (run_mode/received.rs, which says where they live and when they are freed).
    received: received::Received<'a>,
    alloc: Alloc<'a>,
}

/// A static of the run: its word's number ([`Memory::static_word`]), its first byte, its
/// length and alignment (a thread-local's row is made from its template's).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct StaticAt {
    index: u32,
    at: u64,
    len: u64,
    align: u64,
}

impl<'a> RunMode<'a> {
    pub fn new(alloc: Alloc<'a>) -> RunMode<'a> {
        let config = first_call::Config { most_slots: MOST_SLOTS, most_waiters: MOST_WAITERS, ask_bound: ASK_BOUND };
        RunMode {
            slots: Numbering {
                first: FirstCall::new(config, alloc),
                symbols: Vec::new_in(alloc),
                to_wire: Vec::new_in(alloc),
                to_local: Vec::new_in(alloc),
                declared: HashIndexMap::new_in(alloc),
                bound_outside: HashIndexMap::new_in(alloc),
                alloc,
            },
            inbox: Vec::new_in(alloc),
            position_inputs: VecDeque::new_in(alloc),
            observe: Observe::new(0, alloc),
            outbox: std::vec::Vec::new_in(alloc),
            said: Vec::new_in(alloc),
            next_id: 1,
            now: Nanos(0),
            closed: false,
            stopped: false,
            personality: None,
            native: native::NativeState::Waiting,
            statics: HashIndexMap::new_in(alloc),
            told: Vec::new_in(alloc),
            thread_locals: thread_locals::ThreadLocals::new(),
            received: received::Received::new(alloc),
            alloc,
        }
    }

    /// Numbers the entry's slot (slot 0: nothing is numbered before it) and asks for its stub.
    pub fn start(&mut self, entry: CodeSymbol, out: &mut AVec<'_, Act>) -> Result<SlotId, RunModeError> {
        let (slot, new) = self.slots.number(entry)?;
        if new {
            out.push(Act::Stub { slot });
        }
        Ok(slot)
    }

    /// The frames for the parent made so far: the driver writes them, then calls
    /// [`RunMode::sent`].
    pub fn outbox(&self) -> &[u8] {
        &self.outbox
    }

    pub fn sent(&mut self) {
        self.outbox.clear();
    }

    /// The parent's words in the `NotBuilt` failure a [`Stop::Failed`] carries.
    pub fn parent_said(&self) -> &str {
        core::str::from_utf8(&self.said).unwrap_or("(the parent's words are not UTF-8)")
    }

    /// The symbol of a numbered slot.
    pub fn symbol(&self, slot: SlotId) -> CodeSymbol {
        self.slots.symbol(slot)
    }

    pub fn on(&mut self, ev: Event<'_>, out: &mut AVec<'_, Act>) -> Result<(), RunModeError> {
        match ev {
            Event::Tick(now) => {
                self.now = now;
                let mut dos: AVec<'_, Do> = Vec::new_in(self.alloc);
                self.slots.first.tick(now, &mut dos);
                self.carry_dos(&dos, None, &[], out)
            }
            Event::Fired { thread, slot } => {
                let mut dos: AVec<'_, Do> = Vec::new_in(self.alloc);
                self.slots.first.fired(ThreadId(thread.0), slot, self.now, &mut dos)?;
                self.carry_dos(&dos, Some(thread), &[], out)
            }
            Event::Closed => {
                self.closed = true;
                let waiting = self.slots.first.waiting();
                if waiting > 0 {
                    self.stop(Stop::ParentGone { waiting }, out);
                }
                Ok(())
            }
            Event::Frame(bytes) => {
                self.inbox.extend_from_slice(bytes);
                // Taken out while its frames are read, so a message borrowing from it can be
                // handled with the rest of `self`.
                let inbox = core::mem::replace(&mut self.inbox, Vec::new_in(self.alloc));
                let mut used = 0;
                let mut result = Ok(());
                while result.is_ok() {
                    match split(&inbox[used..]) {
                        Ok(Some((item, took))) => {
                            used += took;
                            result = Down::decode(item).map_err(RunModeError::from).and_then(|(_, down)| self.down(down, out));
                        }
                        Ok(None) => break,
                        Err(why) => result = Err(why.into()),
                    }
                }
                self.inbox = inbox;
                let left = self.inbox.len() - used;
                self.inbox.copy_within(used.., 0);
                self.inbox.truncate(left);
                result
            }
        }
    }

    fn down(&mut self, down: Down<'_>, out: &mut AVec<'_, Act>) -> Result<(), RunModeError> {
        // A record's parts come together: any other message while one is not whole is the
        // record cut short (run_mode/received.rs).
        if !matches!(down, Down::Bytes { .. })
            && let Some(cut) = self.received.assembling()
        {
            return Err(cut);
        }
        match down {
            Down::Bytes { record, at, total, part } => self.received.part(record, at, total, part),
            Down::Declare { pairs } => {
                for Declared { symbol, slot: wire } in pairs.iter() {
                    let (local, new) = self.declare_pair(symbol, wire)?;
                    if new {
                        out.push(Act::Stub { slot: local });
                    }
                }
                Ok(())
            }
            Down::Native { library, words } => self.on_native(library, words, out),
            Down::Publish { slot, tier, record } => self.publication(slot, tier, record, &[], out),
            Down::Positioned { slot, tier, record, values } => self.publication(slot, tier, record, values, out),
            Down::Personality { slot } => self.bind_personality(slot),
            Down::Data { data } => {
                for told in data.iter() {
                    let symbol = CodeSymbol(told.symbol.0);
                    if self.slots.bound_outside.contains_key(&symbol.0) {
                        return Err(RunModeError::DefinedAfterBound { symbol });
                    }
                    let placed = self.statics.contains_key(&symbol.0);
                    if placed || self.told.iter().any(|&(it, _)| it == symbol) {
                        return Err(RunModeError::StaticTwice { symbol: told.symbol });
                    }
                    self.told.push((symbol, RecordId(told.record.0)));
                }
                if !self.told.is_empty() {
                    out.push(Act::Statics);
                }
                Ok(())
            }
            Down::ThreadLocals { descriptor, ender } => self.tell_thread_locals(descriptor, ender, out),
            Down::Hooks { slot, kind } => {
                let local = self.slots.local(slot)?;
                out.push(Act::Hook { slot: local, kind });
                Ok(())
            }
            Down::Promote { slot, record, inlined, values } => {
                let local = self.slots.local(slot)?;
                let state = self.slots.first.state(local)?;
                if !matches!(state, first_call::State::Bound(_)) {
                    return Err(RunModeError::NotBuilt {
                        what: "a record at the optimized tier for a slot whose fast code is not bound yet",
                    });
                }
                let mut local_inlined: AVec<'a, Slot> = Vec::new_in(self.alloc);
                for callee_wire in inlined.iter() {
                    let callee_local = self.slots.local(callee_wire)?;
                    local_inlined.push(Slot(callee_local.0));
                }
                self.queue_placement_with_inlined(Act::Promote { slot: local, record }, values, local_inlined, out);
                Ok(())
            }
            Down::Failed { need, why } => {
                let local = match need {
                    Needed::Slot(slot) => self.slots.local(slot)?,
                    Needed::Symbol(symbol) => {
                        // Only on the way to a stop: a scan, not a second map.
                        match self.slots.symbols.iter().position(|it| it.0 == symbol.0) {
                            Some(index) => SlotId(index as u32),
                            None => return Err(RunModeError::FailedUnasked { symbol }),
                        }
                    }
                };
                let why = match why {
                    Failure::Unresolved => {
                        Why::Unresolved { symbol: first_call::Symbol(self.slots.symbol(local).0) }
                    }
                    Failure::Refused { findings } => Why::Refused(FindingsRef(findings)),
                    Failure::Panicked => Why::Panicked,
                    Failure::NotBuilt { what } => {
                        self.said.clear();
                        self.said.extend_from_slice(what.as_bytes());
                        Why::NotBuilt { what: "the parent's words follow" }
                    }
                    Failure::Overran { bound } => Why::Overran { bound: Nanos(bound) },
                };
                let mut dos: AVec<'_, Do> = Vec::new_in(self.alloc);
                self.slots.first.answered(local, Err(why), &mut dos)?;
                self.carry_dos(&dos, None, &[], out)
            }
        }
    }

    fn publication(&mut self, slot: Slot, tier: Tier, record: RecordId, values: &[u8], out: &mut AVec<'_, Act>) -> Result<(), RunModeError> {
        let local = self.slots.local(slot)?;
        let state = self.slots.first.state(local)?;
        if tier == Tier::Optimized {
            return match state {
                first_call::State::Bound(_) => {
                    self.queue_placement(Act::Promote { slot: local, record }, values, out);
                    Ok(())
                }
                _ => Err(RunModeError::NotBuilt {
                    what: "a record at the optimized tier for a slot whose fast code is not bound yet",
                }),
            };
        }
        let code = CodeRef(record.0);
        let mut dos = Vec::new_in(self.alloc);
        match state {
            first_call::State::Asked { .. } => self.slots.first.answered(local, Ok(code), &mut dos)?,
            _ => {
                // An unasked offer or a repeat. FirstCall emits a placement only for new work.
                let symbol = first_call::Symbol(self.slots.symbol(local).0);
                self.slots.first.know(symbol, code, true, &mut dos)?;
            }
        }
        self.carry_dos(&dos, None, values, out)
    }

    fn queue_placement(&mut self, act: Act, values: &[u8], out: &mut AVec<'_, Act>) {
        let empty = Vec::new_in(self.alloc);
        self.queue_placement_with_inlined(act, values, empty, out);
    }

    fn queue_placement_with_inlined(
        &mut self,
        act: Act,
        values: &[u8],
        inlined: AVec<'a, Slot>,
        out: &mut AVec<'_, Act>,
    ) {
        self.position_inputs.push_back((act, Vec::from_slice_in(values, self.alloc), inlined));
        out.push(act);
    }

    /// One pair the parent declared (in `Declare` or `Native`): `symbol` runs through the
    /// parent's slot `wire`, fixed for the run. The local slot, and whether it is new (its
    /// words must name its stub before anything names it: the caller stubs it).
    fn declare_pair(&mut self, symbol: WireSymbol, wire: Slot) -> Result<(SlotId, bool), RunModeError> {
        if wire.0 >= MOST_SLOTS {
            return Err(RunModeError::SlotPast { slot: wire, most: MOST_SLOTS });
        }
        let (local, new) = self.slots.number(CodeSymbol(symbol.0))?;
        let at = wire.0 as usize;
        if self.slots.to_local.len() <= at {
            self.slots.to_local.resize(at + 1, NONE);
        }
        let had_local = self.slots.to_local[at];
        let had_wire = self.slots.to_wire[local.index()];
        match (had_local, had_wire) {
            (NONE, NONE) => {
                let symbol = CodeSymbol(symbol.0);
                if self.slots.bound_outside.contains_key(&symbol.0) {
                    return Err(RunModeError::DefinedAfterBound { symbol });
                }
                self.slots.to_local[at] = local.0;
                self.slots.to_wire[local.index()] = wire.0;
                // Not found: `to_wire` was NONE, so this symbol was never declared before.
                self.slots.declared.insert(symbol.0, local);
            }
            (l, w) if l == local.0 && w == wire.0 => {}
            _ => return Err(RunModeError::Redeclared { slot: wire, symbol }),
        }
        Ok((local, new))
    }

    /// The first-call machine's effects as acts and wire frames. `asker` is the thread whose
    /// stub made them (an ask comes only from a fired stub).
    fn carry_dos(&mut self, dos: &[Do], asker: Option<Thread>, values: &[u8], out: &mut AVec<'_, Act>) -> Result<(), RunModeError> {
        for &step in dos {
            match step {
                Do::Stub { slot } => out.push(Act::Stub { slot }),
                Do::Ask { slot, symbol } => {
                    if self.closed {
                        let waiting = self.slots.first.waiting();
                        self.stop(Stop::ParentGone { waiting }, out);
                        continue;
                    }
                    let need = match self.slots.wire(slot) {
                        Some(wire) => Needed::Slot(wire),
                        None => Needed::Symbol(WireSymbol(symbol.0)),
                    };
                    // FirstCall asks only from `fired`, which names its thread.
                    let thread = asker.unwrap_or(Thread(0));
                    Up::Need { need, thread }.encode(self.next_id, &mut self.outbox)?;
                    self.next_id += 1;
                }
                Do::Publish { slot, code } => self.queue_placement(Act::Place { slot, record: RecordId(code.0) }, values, out),
                Do::Wake { thread, slot, woken: Woken::Again } => out.push(Act::Wake { thread: Thread(thread.0), slot }),
                Do::Wake { slot, woken: Woken::Fail(why), .. } => {
                    let symbol = self.slots.symbol(slot);
                    self.stop(Stop::Failed { slot, symbol, why }, out);
                }
            }
        }
        Ok(())
    }

    /// One stop: the process ends at the first, so later ones say nothing new.
    fn stop(&mut self, stop: Stop, out: &mut AVec<'_, Act>) {
        if !self.stopped {
            self.stopped = true;
            out.push(Act::Stop(stop));
        }
    }

    /// Carries [`Act::Place`]: reads `record`, places it at room the memory reserves, completes
    /// its sites against the run's words (a target first seen gets its slot and stub here,
    /// before any word names it), makes its unwind entry, then publishes in code_unwind's order
    /// (copy, cache maintenance, register, store), and tells the first-call machine: the
    /// waiting threads' [`Act::Wake`]s follow in `out`.
    pub fn place(
        &mut self,
        slot: SlotId,
        record: RecordId,
        source: &impl RecordSource,
        memory: &mut impl Memory,
        plant: Plant,
        out: &mut AVec<'_, Act>,
    ) -> Result<(), RunModeError> {
        self.lay(slot, record, source, memory, plant, Region::Area)?;
        let mut dos: AVec<'_, Do> = Vec::new_in(self.alloc);
        self.slots.first.published(slot, &mut dos)?;
        self.carry_dos(&dos, None, &[], out)
    }

    /// Carries [`Act::Promote`]: places `record` in the hot region and publishes it in
    /// code_unwind's order (copy, cache maintenance, register, store): the store of the slot word
    /// is the promotion. The fast body it replaces stays placed (the area is append-only), so a
    /// frame still in it returns through code that is still there.
    pub fn promote(
        &mut self,
        slot: SlotId,
        record: RecordId,
        source: &impl RecordSource,
        memory: &mut impl Memory,
        plant: Plant,
    ) -> Result<(), RunModeError> {
        self.lay(slot, record, source, memory, plant, Region::Hot)
    }

    /// Carries [`Act::Statics`]: every static told since the last one, placed together (the
    /// module doc, "Statics"). Each datum is read and given its room and its word's number
    /// first, so a word naming another static of the batch resolves (a cycle included); then
    /// each is filled against the run's words and published ([`Memory::publish_static`]: the
    /// bytes, then the word).
    pub fn place_statics(&mut self, source: &impl RecordSource, memory: &mut impl Memory) -> Result<(), RunModeError> {
        let count = self.told.len();
        let mut scratch: std::vec::Vec<std::vec::Vec<u8>> = (0..count).map(|_| std::vec::Vec::new()).collect();
        let mut metas: AVec<'_, (CodeSymbol, RecordId, StaticAt, Room)> = Vec::with_capacity_in(count, self.alloc);
        for i in 0..count {
            let (symbol, record) = self.told[i];
            let raw = self.received.bytes_of(record, source, &mut scratch[i])?;
            let view = DatumView::read(raw).map_err(|why| RunModeError::Datum { record, why })?;
            let index = u32::try_from(self.statics.len()).ok().filter(|&n| n < MOST_STATICS).ok_or(RunModeError::StaticsFull)?;
            // `DatumView::read` checked the length fits a u32, and the alignment a page.
            let (len, align) = (view.len() as usize, view.align() as usize);
            let (at, room) = match view.place() {
                DatumPlace::ReadOnly => (memory.reserve(len, align)?, Room::Code),
                // A zeroed datum keeps no bytes: its room is zero as reserved.
                DatumPlace::Writable | DatumPlace::Zeroed { .. } => (memory.reserve_data(len, align)?, Room::Data),
            };
            let row = StaticAt { index, at, len: view.len(), align: view.align() };
            if !self.statics.insert(symbol.0, row) {
                return Err(RunModeError::StaticTwice { symbol: WireSymbol(symbol.0) });
            }
            metas.push((symbol, record, row, room));
        }
        self.told.clear();
        for (i, &(symbol, record, row, room)) in metas.iter().enumerate() {
            let raw = self.received.again(record, source, &scratch[i])?;
            let view = DatumView::read(raw).map_err(|why| RunModeError::Datum { record, why })?;
            let mut bytes: AVec<'_, u8> = Vec::from_elem_in(0, view.bytes().len(), self.alloc);
            {
                let mut binder = Binder {
                    slots: &mut self.slots,
                    statics: &self.statics,
                    thread_locals: &mut self.thread_locals,
                    memory: &mut *memory,
                    names: Names::Datum(view),
                    record,
                    refused: None,
                };
                let filled = datum::fill(&view, symbol, row.at, &mut binder, &mut bytes[..]);
                if let Some(why) = binder.refused {
                    return Err(why);
                }
                filled.map_err(|why| RunModeError::Fill { record, why })?;
            }
            memory.publish_static(row.index, row.at, &bytes[..], room)?;
            let kind = match room {
                Room::Code => fault_report::Kind::ReadOnlyDatum,
                Room::Data => fault_report::Kind::WritableDatum,
            };
            memory.note(fault_report::Row::new(kind, row.at, u64::from(row.len), symbol, record));
        }
        Ok(())
    }

    /// Carries [`Act::Hook`]: installs or removes an observe stub on `slot` through the unified
    /// `Observe` overlay, automatically retreating/re-promoting any optimized caller whose
    /// inlined read list includes `slot`.
    pub fn hook(&mut self, slot: SlotId, kind: HookKind, memory: &mut impl Memory) -> Result<(), RunModeError> {
        let obs_slot = Slot(slot.0);
        let mut writes = std::vec::Vec::new_in(self.alloc);
        match kind {
            HookKind::Plain => {
                self.observe.plain(&[obs_slot], &mut writes);
            }
            HookKind::Count | HookKind::Mark | HookKind::Hook | HookKind::Span | HookKind::Alloc(_) => {
                let obs_kind = match kind {
                    HookKind::Count => ObserveKind::Count,
                    HookKind::Mark => ObserveKind::Mark,
                    HookKind::Hook => ObserveKind::Hook,
                    HookKind::Span => ObserveKind::Span,
                    HookKind::Alloc(ak) => ObserveKind::Alloc(ak),
                    HookKind::Plain => unreachable!(),
                };
                if self.observe.body(obs_slot).is_some() {
                    let stub_addr = memory.ensure_observe_stub(slot, obs_kind)?;
                    self.observe.observe(&[obs_slot], obs_kind, |_| stub_addr, &mut writes);
                }
            }
        }
        apply_observe_writes(memory, &writes)
    }

    /// Places `record` as `slot`'s code in `region` and publishes it.
    fn lay(
        &mut self,
        slot: SlotId,
        record: RecordId,
        source: &impl RecordSource,
        memory: &mut impl Memory,
        plant: Plant,
        region: Region,
    ) -> Result<(), RunModeError> {
        let placement = match region {
            Region::Area => Act::Place { slot, record },
            Region::Hot => Act::Promote { slot, record },
        };
        let queued = self.position_inputs.front().map(|(act, _, _)| *act);
        if queued != Some(placement) {
            return Err(RunModeError::PositionOrder { placement, queued });
        }
        let (_, values, inlined) = self.position_inputs.pop_front().expect("the placement at the front was just checked");
        let mut scratch = std::vec::Vec::new();
        let bytes = self.received.bytes_of(record, source, &mut scratch)?;
        let view = CodeView::read(bytes).map_err(|why| RunModeError::Decode { record, why })?;
        let unwind = blaze_code::unwind::read(view).map_err(|why| RunModeError::Unwind { record, why })?;
        let code_at = match region {
            // The record states its own alignments (blaze-code record.rs, form `bzcode06`: a
            // datum aligned past 16 raises the code part's; log2 in the head).
            Region::Area => memory.reserve(view.code().len(), 1 << view.code_align())?,
            Region::Hot => memory.reserve_hot(view.code().len(), 1 << view.code_align())?,
        };
        let relro_at = memory.reserve(view.relro().len(), 1 << view.relro_align())?;
        let mut code: AVec<'_, u8> = Vec::from_elem_in(0, view.code().len(), self.alloc);
        let mut relro: AVec<'_, u8> = Vec::from_elem_in(0, view.relro().len(), self.alloc);
        let symbol = self.slots.symbol(slot);
        {
            let mut binder = Binder {
                slots: &mut self.slots,
                statics: &self.statics,
                thread_locals: &mut self.thread_locals,
                memory: &mut *memory,
                names: Names::Code(view),
                record,
                refused: None,
            };
            let placed = blaze_code::place::place_resolved(&view, symbol, At { code: code_at, relro: relro_at }, &mut binder, &mut code, &mut relro, &values);
            if let Some(why) = binder.refused {
                return Err(why);
            }
            placed.map_err(|why| RunModeError::Place { record, why })?;
        }
        let mut entry_bytes: AVec<'_, u8> = Vec::new_in(self.alloc);
        // The program's personality routine (its slot's entry thunk), once the parent named it;
        // `None` before, and a record whose rows land is then `EhError::NoPersonality`.
        let personality = self.personality_at(&*memory);
        let entry = match unwind.frame() {
            None => None,
            Some(_) => {
                let eh = |why| RunModeError::Eh { record, why };
                // Sized at address 0, then made at its own: an entry's length does not depend
                // on its addresses (every one is an 8-byte absptr; TO VERIFY BY A TEST).
                eh::entry(&unwind, Placed { entry_at: 0, code_at, personality }, &mut entry_bytes).map_err(eh)?;
                let entry_at = memory.reserve(entry_bytes.len(), 8)?;
                let parts = eh::entry(&unwind, Placed { entry_at, code_at, personality }, &mut entry_bytes).map_err(eh)?;
                Some((entry_at, parts))
            }
        };
        let placing = Placing {
            slot,
            code_at,
            code: &code,
            relro_at,
            relro: &relro,
            entry: entry.map(|(at, parts)| (at, &entry_bytes[..], parts)),
        };
        // Decide the exact body-word / slot-word stores through the unified Observe overlay
        // (preserving active observe stubs and keeping callers on fast tier while any inlined
        // callee is observed).
        let mut obs_writes = std::vec::Vec::new_in(self.alloc);
        match region {
            Region::Area => self.observe.publish(Slot(slot.0), code_at, &mut obs_writes),
            Region::Hot => self.observe.publish_optimized(Slot(slot.0), code_at, inlined.as_slice(), &mut obs_writes),
        }
        let plain_store = obs_writes.iter().any(|w| matches!(w, ObserveWrite::Slot { slot: s, to: Slotword::Body(addr) } if *s == Slot(slot.0) && *addr == code_at));
        let mut publication = Publication { memory, placing: &placing, obs_writes: &obs_writes, plain_store, failed: None };
        let order = match plant {
            Plant::None => code_unwind::Plant::None,
            Plant::StoreBeforeRegister => code_unwind::Plant::RegisterAfterStore,
        };
        code_unwind::publish(&mut publication, order);
        if let Some(why) = publication.failed {
            return Err(why);
        }
        memory.note(fault_report::Row::new(fault_report::Kind::Code, code_at, code.len() as u64, symbol, record));
        if !relro.is_empty() {
            memory.note(fault_report::Row::new(fault_report::Kind::Relro, relro_at, relro.len() as u64, symbol, record));
        }
        Ok(())
    }
}

/// The run's words as blaze-code's placement and a datum's fill ask for them. A refusal is kept
/// whole in `refused` (the trait's error type is the image's); the caller returns it.
struct Binder<'s, 'a, M: Memory> {
    slots: &'s mut Numbering<'a>,
    /// The statics placed (or given their room in this batch), indexed by symbol.
    statics: &'s HashIndexMap<'a, StaticAt>,
    /// The run's thread-local indexes: a load of a thread-local's index numbers it.
    thread_locals: &'s mut thread_locals::ThreadLocals<'a>,
    memory: &'s mut M,
    /// The record whose C names' texts this placement reads: a code record's, or a datum's.
    names: Names<'s>,
    record: RecordId,
    refused: Option<RunModeError>,
}

/// Where a placement finds a C name's text: the record being placed keeps it.
#[derive(Clone, Copy)]
enum Names<'s> {
    Code(CodeView<'s>),
    Datum(DatumView<'s>),
}

impl<'s> Names<'s> {
    fn name(self, symbol: CodeSymbol) -> Option<&'s str> {
        match self {
            Names::Code(view) => view.name(symbol),
            Names::Datum(view) => view.name(symbol),
        }
    }
}

/// What a C name target is bound to: THE BINDER SEAM's three answers (run_mode/bind.rs), one
/// for every space that asks (a call, a load of its word, a datum's word).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Bound {
    /// A C name the program defines as a function, through the slot the parent declared.
    Slot(SlotId),
    /// A C name the program defines as data, the static the parent told.
    Static(StaticAt),
    /// A C name bound by the run's strategy ([`Bind`]): its word and the address in it.
    Outside(Outside),
}

impl<M: Memory> Binder<'_, '_, M> {
    fn func(&mut self, symbol: CodeSymbol) -> Result<SlotId, RunModeError> {
        let (slot, new) = self.slots.number(symbol)?;
        if new {
            self.memory.stub(slot)?;
        }
        Ok(slot)
    }

    /// THE BINDER SEAM (run_mode/bind.rs): C name target `symbol` of the record being placed,
    /// first as what the program defines (a declared slot, a told static: its symbol is the
    /// name's own), then through the memory's strategy, which needs the name's text.
    fn outside(&mut self, symbol: CodeSymbol, weak: bool) -> Result<Bound, RunModeError> {
        if let Some(slot) = self.slots.declared(symbol) {
            return Ok(Bound::Slot(slot));
        }
        if let Some(&at) = self.statics.get(&symbol.0) {
            return Ok(Bound::Static(at));
        }
        let Some(name) = self.names.name(symbol) else {
            return Err(RunModeError::Outside { record: self.record, symbol, name: CName::NONE, why: "the record keeps no name for it" });
        };
        let bound = self.memory.outside(self.record, symbol, name, weak)?;
        self.slots.bound_outside.insert(symbol.0, ());
        Ok(Bound::Outside(bound))
    }

    /// The word a C name target's site loads: for a call the word holding the code to run, for
    /// a load the word holding its address.
    fn outside_word(&mut self, symbol: CodeSymbol, call: bool, weak: bool) -> Result<u64, RunModeError> {
        match self.outside(symbol, weak)? {
            Bound::Slot(slot) => Ok(match call {
                true => self.memory.word(slot),
                false => self.memory.address_word(slot),
            }),
            Bound::Static(at) if !call => Ok(self.memory.static_word(at.index)),
            // A call of data the program defines: no code is there.
            Bound::Static(_) => Err(RunModeError::Target {
                record: self.record,
                space: if weak { Space::WeakOutside } else { Space::Outside },
                symbol,
            }),
            Bound::Outside(it) => Ok(it.word),
        }
    }

    /// A C name target's address, as a datum's word holds it.
    fn outside_value(&mut self, symbol: CodeSymbol, weak: bool) -> Result<u64, RunModeError> {
        Ok(match self.outside(symbol, weak)? {
            Bound::Slot(slot) => self.memory.thunk(slot),
            Bound::Static(at) => at.at,
            Bound::Outside(it) => it.value,
        })
    }

    /// Static `symbol`, which the parent sent before the record naming it.
    fn static_at(&self, symbol: CodeSymbol) -> Result<StaticAt, RunModeError> {
        match self.statics.get(&symbol.0) {
            Some(&at) => Ok(at),
            None => Err(RunModeError::StaticNotTold { record: self.record, symbol }),
        }
    }

    /// Keeps `why` and gives the image's error naming the target.
    fn refuse(&mut self, target: Target, why: RunModeError) -> ImageError {
        if self.refused.is_none() {
            self.refused = Some(why);
        }
        ImageError::Unbound { target }
    }
}

impl<M: Memory> Words for Binder<'_, '_, M> {
    fn word(&mut self, target: Target, call: bool) -> Result<u64, ImageError> {
        let found = match target.space {
            Space::Func => self.func(target.symbol).map(|slot| match call {
                true => self.memory.word(slot),
                false => self.memory.address_word(slot),
            }),
            Space::Outside | Space::WeakOutside => self.outside_word(target.symbol, call, target.space == Space::WeakOutside),
            // A load of the static's address reads its word; nothing calls a static. A vtable is
            // a datum the parent sent as a static's (code_records.rs `VtableDatum`), bound alike.
            Space::Static | Space::Vtable if !call => self.static_at(target.symbol).map(|it| self.memory.static_word(it.index)),
            // A load of a thread-local's index reads a word holding it (run_mode/thread_locals.rs).
            Space::Tls if !call => {
                let template = self.static_at(target.symbol);
                thread_locals::made(self.thread_locals, self.record, target.symbol)
                    .and_then(|made| made.index_word(target.symbol, template, &mut *self.memory))
            }
            Space::Static | Space::Vtable | Space::Tls => {
                Err(RunModeError::Target { record: self.record, space: target.space, symbol: target.symbol })
            }
        };
        found.map_err(|why| self.refuse(target, why))
    }

    fn value(&mut self, target: Target) -> Result<u64, ImageError> {
        let found = match target.space {
            Space::Func => self.func(target.symbol).map(|slot| self.memory.thunk(slot)),
            Space::Outside | Space::WeakOutside => self.outside_value(target.symbol, target.space == Space::WeakOutside),
            // A vtable's first word (a supertrait's table, in a vtable's datum) is its datum's.
            Space::Static | Space::Vtable => self.static_at(target.symbol).map(|it| it.at),
            Space::Tls => {
                Err(RunModeError::Target { record: self.record, space: target.space, symbol: target.symbol })
            }
        };
        found.map_err(|why| self.refuse(target, why))
    }
}

/// Applies `ObserveWrite` operations in the exact order emitted by `Observe`: body words
/// before any slot word pointing at a stub.
fn apply_observe_writes(memory: &mut impl Memory, writes: &[ObserveWrite]) -> Result<(), RunModeError> {
    for &write in writes {
        match write {
            ObserveWrite::Body { slot, body } => memory.store_body_word(SlotId(slot.0), body)?,
            ObserveWrite::Slot { slot, to } => match to {
                Slotword::Body(addr) | Slotword::Stub { at: addr, .. } => {
                    memory.store_slot_word(SlotId(slot.0), addr)?
                }
                Slotword::Lazy => {}
            },
        }
    }
    Ok(())
}

/// code_unwind's four steps over a [`Memory`]: the first failure is kept and every later step
/// is skipped (a word is never stored over code that was not copied).
struct Publication<'m, 'p, M: Memory> {
    memory: &'m mut M,
    placing: &'m Placing<'p>,
    obs_writes: &'m [ObserveWrite],
    plain_store: bool,
    failed: Option<RunModeError>,
}

impl<M: Memory> code_unwind::Effects for Publication<'_, '_, M> {
    fn step(&mut self, step: Step) {
        if self.failed.is_some() {
            return;
        }
        let res = match step {
            Step::Store if !self.plain_store => apply_observe_writes(self.memory, self.obs_writes),
            Step::Store => {
                // Keep every body word updated before the plain slot-word store.
                for &w in self.obs_writes {
                    if let ObserveWrite::Body { slot, body } = w {
                        if let Err(why) = self.memory.store_body_word(SlotId(slot.0), body) {
                            self.failed = Some(why);
                            return;
                        }
                    }
                }
                self.memory.carry(Step::Store, self.placing)
            }
            other => self.memory.carry(other, self.placing),
        };
        if let Err(why) = res {
            self.failed = Some(why);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The driver: the process's memory, threads and socket.

/// How the entry is called (TO VERIFY BY A TEST: the hello-world rung's entry is one of these).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryForm {
    /// `extern "C" fn()`: the status is 0 when it returns.
    Unit,
    /// C `main`: `extern "C" fn(c_int, *const *const c_char) -> c_int`.
    CMain,
    /// bstd's hosted entry, `extern "C" fn(*const HostTable) -> i32` (`bstd_hosted_start`,
    /// crates/blaze-std entry.rs; the table is bstd's `wire::HostTable`, which the launcher makes
    /// in this process over its own world: run.md, "Every program runs in the child").
    Hosted,
    /// A compile-time program's entry (D175, `comptime:<symbol>`): C `main`'s form,
    /// `extern "C" fn(c_int, *const *const c_char) -> c_int`, called with the arguments the
    /// compile-time host sent (blaze-comptime-sys `child::argv`), never the process's own; for a
    /// proc macro the symbol is `comptime:blaze_proc_macro_expand`'s. Its C names are bound
    /// through blaze-comptime-sys's table ([`Table`]) and the run RETURNS its status
    /// ([`End::Return`]): the launcher owns the process (G5).
    Comptime,
}

/// How [`run`] ends once the entry returns (G5).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum End {
    /// An ordinary program: the process ends through `exit` with the entry's status, on the
    /// entry's thread with the control thread alive, so the program's `atexit` functions (its
    /// code) can still reach code not yet placed.
    Exit,
    /// A test entry with an unwind marker (`BLAZE_LAUNCH_UNWOUND_FILE`): on normal return, ends
    /// through `libc::exit(status)` with the control thread alive so TLS destructors and `atexit`
    /// handlers (e.g. tokio runtime TLS) can still JIT-resolve unplaced destructors; on an
    /// uncaught guest panic unwind, stops the control thread and returns [`RunModeError::Unwound`]
    /// so the launcher records the unwind marker and ends with `_exit(101)`.
    ExitOrReturnUnwound,
    /// A compile-time program: the control thread is stopped and joined and [`run`] returns the
    /// status to its caller, which owns the process (blaze-launcher's `comptime:` branch, which
    /// ends it through the compile-time transport's `end`). The program's `atexit` functions
    /// do not run here: a compile-time program's `exit` is the layer's, which never returns.
    Return,
}

/// A compile-time entry's `argc` and `argv`: the arguments the compile-time host sent. Made
/// only by [`ComptimeArgs::new`], whose contract is the words' life.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ComptimeArgs {
    argc: core::ffi::c_int,
    argv: *const *const core::ffi::c_char,
}

impl ComptimeArgs {
    /// # Safety
    /// `argv` points at `argc` NUL-terminated strings and a null after them (blaze-comptime-sys
    /// `child::argv`, the layer's words), all live and unchanged until the process ends.
    pub unsafe fn new(argc: core::ffi::c_int, argv: *const *const core::ffi::c_char) -> ComptimeArgs {
        ComptimeArgs { argc, argv }
    }
}

/// What the program starts at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Start {
    pub entry: CodeSymbol,
    pub form: EntryForm,
}

/// The address of bstd's host table (`bstd::wire::HostTable`) a hosted entry is called with.
/// Made only by [`HostTableAt::new`], whose contract is the table's life.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HostTableAt(core::ptr::NonNull<core::ffi::c_void>);

impl HostTableAt {
    /// # Safety
    /// `table` is a `bstd::wire::HostTable` made by a `bstd::hosting::Hosting` of this process,
    /// and both live, unmoved, until [`run`] returns or the process ends: the program calls the
    /// table's entries, which read the `Hosting` behind its `host` pointer, at any time it runs.
    pub unsafe fn new(table: core::ptr::NonNull<core::ffi::c_void>) -> HostTableAt {
        HostTableAt(table)
    }
}

/// What the entry is handed, by its form: [`EntryForm`] with a hosted entry's table. The launcher
/// makes it from [`Start::form`] (one `match`, so a form never meets another's arguments).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Handed {
    /// [`EntryForm::Unit`]: nothing.
    Unit,
    /// [`EntryForm::CMain`]: the process's `argc` and `argv`, made in [`run`].
    CMain,
    /// [`EntryForm::Hosted`]: the host's table.
    Hosted(HostTableAt),
    /// [`EntryForm::Comptime`]: the compile-time host's arguments.
    Comptime(ComptimeArgs),
}

/// Why `BLAZE_LAUNCH_ENTRY`'s text names no entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EntryRefused {
    pub why: &'static str,
}

impl fmt::Display for EntryRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "BLAZE_LAUNCH_ENTRY is `<form>:<64 hex digits of the entry's symbol>` with the form `unit`, `c-main`, `hosted` or `comptime`: {}",
            self.why
        )
    }
}

impl Start {
    /// Reads the parent's `BLAZE_LAUNCH_ENTRY`: `unit:<symbol>`, `c-main:<symbol>`,
    /// `hosted:<symbol>` (bstd's hosted entry, `bstd_hosted_start`) or `comptime:<symbol>` (a
    /// compile-time program, D175), the symbol in 64 lowercase or uppercase hex digits. Any
    /// other form is refused by name.
    pub fn parse(text: &[u8]) -> Result<Start, EntryRefused> {
        let (form, hex) = match text {
            [b'u', b'n', b'i', b't', b':', hex @ ..] => (EntryForm::Unit, hex),
            [b'c', b'-', b'm', b'a', b'i', b'n', b':', hex @ ..] => (EntryForm::CMain, hex),
            [b'h', b'o', b's', b't', b'e', b'd', b':', hex @ ..] => (EntryForm::Hosted, hex),
            [b'c', b'o', b'm', b'p', b't', b'i', b'm', b'e', b':', hex @ ..] => (EntryForm::Comptime, hex),
            _ => return Err(EntryRefused { why: "the form is none of `unit`, `c-main`, `hosted` and `comptime`" }),
        };
        if hex.len() != 64 {
            return Err(EntryRefused { why: "the symbol is not 64 hex digits" });
        }
        let digit = |it: u8| (it as char).to_digit(16).map(|it| it as u8);
        let mut symbol = [0u8; 32];
        for (byte, pair) in symbol.iter_mut().zip(hex.chunks(2)) {
            match (digit(pair[0]), digit(pair[1])) {
                (Some(high), Some(low)) => *byte = high << 4 | low,
                _ => return Err(EntryRefused { why: "the symbol holds a character that is no hex digit" }),
            }
        }
        Ok(Start { entry: CodeSymbol(symbol), form })
    }
}

/// The store's records as files of one folder, each named by its id's 64 hex digits, mapped
/// read-only when first asked and checked against their id. A record is placed once a run, so
/// each file is mapped once; a mapping is kept for the process's life (placed code was copied
/// out of it; unmapping after placement comes with the store's pack form).
pub struct Folder<'p> {
    path: &'p [u8],
}

impl<'p> Folder<'p> {
    /// `path` is the folder, without a trailing `/`.
    pub fn new(path: &'p [u8]) -> Folder<'p> {
        Folder { path }
    }
}

/// The native library's file is `dlopen`ed in place from the same folder (native_lib.rs).
impl RecordFiles for Folder<'_> {
    fn folder(&self) -> &[u8] {
        self.path
    }
}

impl RecordSource for Folder<'_> {
    fn bytes(&self, id: RecordId) -> Result<&[u8], RunModeError> {
        let mut path = [0u8; PATH_MOST];
        crate::native_lib::record_path(self.path, id, &mut path)?;
        let missing = |errno| RunModeError::Missing { record: id, errno };
        // SAFETY: `path` is a NUL-terminated C string (zeroed past what was written above).
        let fd = unsafe { libc::open(path.as_ptr().cast(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(missing(errno()));
        }
        // SAFETY: an all-zero `stat` is a valid value for the call to fill.
        let mut stat: libc::stat = unsafe { core::mem::zeroed() };
        // SAFETY: `fd` is open; `stat` is a live local.
        let statted = unsafe { libc::fstat(fd, &mut stat) };
        let size = stat.st_size as usize;
        let mapped = match (statted, size) {
            (0, 0) => Err(missing(libc::EINVAL)),
            (0, size) => {
                // SAFETY: a fresh read-only private mapping of a file this process opened;
                // nothing else is at the address the kernel picks.
                let at = unsafe { libc::mmap(core::ptr::null_mut(), size, libc::PROT_READ, libc::MAP_PRIVATE, fd, 0) };
                match at == libc::MAP_FAILED {
                    true => Err(missing(errno())),
                    false => Ok(at),
                }
            }
            _ => Err(missing(errno())),
        };
        // SAFETY: closes the descriptor opened above; a mapping outlives it.
        unsafe { libc::close(fd) };
        let at = mapped?;
        // SAFETY: `size` readable bytes at `at`, a mapping never unmapped and never written
        // (private, read-only): valid for any lifetime.
        let bytes: &[u8] = unsafe { core::slice::from_raw_parts(at.cast::<u8>(), size) };
        let got = blaze_blake3::hash(bytes);
        if got != id.0 {
            return Err(RunModeError::Hash { record: id, got });
        }
        Ok(bytes)
    }

    /// Reads record `id` into `scratch` via `read(2)` and closes the descriptor immediately —
    /// avoiding leaking a permanent `mmap` VMA and 16 KiB macOS page entry per loaded function.
    fn read_into<'a>(&'a self, id: RecordId, scratch: &'a mut std::vec::Vec<u8>) -> Result<&'a [u8], RunModeError> {
        let mut path = [0u8; PATH_MOST];
        crate::native_lib::record_path(self.path, id, &mut path)?;
        let missing = |errno| RunModeError::Missing { record: id, errno };
        // SAFETY: `path` is a NUL-terminated C string.
        let fd = unsafe { libc::open(path.as_ptr().cast(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(missing(errno()));
        }
        // SAFETY: zeroed `stat` filled by `fstat`.
        let mut stat: libc::stat = unsafe { core::mem::zeroed() };
        let statted = unsafe { libc::fstat(fd, &mut stat) };
        let size = usize::try_from(stat.st_size).unwrap_or(0);
        if statted != 0 || size == 0 {
            let err = if statted != 0 { errno() } else { libc::EINVAL };
            // SAFETY: closes the descriptor opened above.
            unsafe { libc::close(fd) };
            return Err(missing(err));
        }
        scratch.resize(size, 0);
        let mut filled = 0usize;
        let mut read_err = 0i32;
        while filled < size {
            // SAFETY: `scratch[filled..size]` is live writable memory.
            let n = unsafe { libc::read(fd, scratch.as_mut_ptr().add(filled).cast(), size - filled) };
            if n > 0 {
                filled += n as usize;
            } else if n == 0 {
                read_err = libc::EIO;
                break;
            } else {
                let e = errno();
                if e == libc::EINTR {
                    continue;
                }
                read_err = e;
                break;
            }
        }
        // SAFETY: closes the descriptor opened above.
        unsafe { libc::close(fd) };
        if filled != size {
            return Err(missing(read_err));
        }
        let got = blaze_blake3::hash(scratch.as_slice());
        if got != id.0 {
            return Err(RunModeError::Hash { record: id, got });
        }
        Ok(scratch.as_slice())
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// The run's one mapping, by offset from its base: the slot words, the address words (each a
/// slot's thunk address), the C names' words, the stubs' head and stubs, the entry thunks, the
/// code area. One reservation within [`crate::drive::REACH`], so every site reaches every word.
struct Layout {
    words: usize,
    address_words: usize,
    outside: usize,
    /// One word a static, holding its address ([`MOST_STATICS`]).
    statics: usize,
    stubs: usize,
    thunks: usize,
    area: usize,
    /// The hot region: `hot..whole`, the top [`HOT_BYTES`] of the reservation.
    hot: usize,
    whole: usize,
}

/// The hot region's length: promoted code only, a small part of a run's code (D100 point 13).
const HOT_BYTES: usize = 256 << 20;

impl Layout {
    fn of(page: usize) -> Result<Layout, RunModeError> {
        let slots = MOST_SLOTS as usize;
        let words = 0;
        let address_words = words + slots * 8;
        let outside = address_words + slots * 8;
        let statics = outside + MOST_OUTSIDE as usize * 8;
        let stubs = (statics + MOST_STATICS as usize * 8).next_multiple_of(16);
        let thunks = (stubs + crate::arch::STUB_HEAD + slots * crate::arch::STUB).next_multiple_of(16);
        let area = (thunks + slots * crate::arch::THUNK).next_multiple_of(page);
        let whole = crate::drive::REACH as usize;
        let hot = whole - HOT_BYTES;
        if area >= hot {
            return Err(RunModeError::NotBuilt { what: "a slot table larger than the run's reservation" });
        }
        Ok(Layout { words, address_words, outside, statics, stubs, thunks, area, hot, whole })
    }
}

/// The process's run memory: one `sys::Code` mapping laid out as [`Layout`] says, its C names
/// bound by strategy `B` (run_mode/bind.rs).
struct NativeMemory<'a, B: Bind> {
    /// How a C name the program does not define is bound: [`Dlsym`] or [`Table`].
    bind: B,
    code: crate::sys::Code,
    layout: Layout,
    base: u64,
    /// The next free byte of the area, from the mapping's base.
    next: usize,
    /// The next free byte of the hot region, from the mapping's base.
    hot_next: usize,
    /// The C names bound, sorted by symbol: each with its word's index.
    outside: AVec<'a, (CodeSymbol, u32)>,
    shared: std::sync::Arc<crate::table::Shared>,
    /// The run's unwind registry's one writer (run_mode/personality.rs `run_registry`): every
    /// placed record's entry is added through it at its publication's Register step.
    unwind: crate::unwind_reg::Writer<'static>,
    /// The writable data area ([`Memory::reserve_data`]): [`DATA_BYTES`] at `data_base`.
    data_base: u64,
    /// The next free byte of the data area, from its base.
    data_next: usize,
    /// The program's native library once loaded (`Act::Native`): from then on every C name not
    /// claimed by [`Bind::intercept`] is bound through its handle (`native_lib::bind`).
    natives: Option<crate::native_lib::DlNatives>,
    /// Shared veneers for `observe::hook_entry`, `observe::span_entry`, and `observe::alloc_entry`
    /// (reachable within ±128 MiB from every observe stub placed in the code area).
    hook_veneer: u64,
    span_veneer: u64,
    alloc_veneer: u64,
    /// Emitted observe stubs per `(slot, kind)` with their run-view address (each emitted once;
    /// its trailing `.quad body` is updated atomically by `store_body_word`).
    observe_stubs: AVec<'a, (SlotId, ObserveKind, u64)>,
}

/// The writable data area's length: address space only, each page backed by the kernel on its
/// first touch (an anonymous private mapping), so its size costs nothing until used.
const DATA_BYTES: usize = 1 << 30;

/// Maps the writable data area: read-write, zero, never executable, never unmapped (a static
/// lives as long as the process).
fn map_data() -> Result<u64, RunModeError> {
    // SAFETY: a fresh anonymous private mapping at an address the kernel picks: nothing else
    // is there, and no Rust object is aliased by it.
    let at = unsafe { libc::mmap(core::ptr::null_mut(), DATA_BYTES, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANON, -1, 0) };
    match at == libc::MAP_FAILED {
        true => Err(RunModeError::DataMap { errno: errno() }),
        false => Ok(at as u64),
    }
}

impl<'a, B: Bind> NativeMemory<'a, B> {
    fn new(bind: B, alloc: Alloc<'a>, unwind: crate::unwind_reg::Writer<'static>) -> Result<NativeMemory<'a, B>, RunModeError> {
        let page = crate::sys::page().map_err(RunModeError::Sys)?;
        let layout = Layout::of(page)?;
        let mut code = crate::sys::Code::map(layout.whole).map_err(RunModeError::Sys)?;
        let base = code.base();
        let shared = std::sync::Arc::new(crate::table::Shared::new(
            MOST_SLOTS,
            base + layout.words,
            base + layout.stubs,
            base + layout.thunks,
            base + layout.area..base + layout.whole,
            crate::stub_wait::Waiters::reserve(crate::stub_wait::MOST_WAITERS).map_err(RunModeError::Sys)?,
        ));
        // The stubs hand this pointer to `table::resolve` for as long as the process runs
        // code: one count of the `Arc` is leaked for it (as `Driver::new` does).
        let context = std::sync::Arc::into_raw(std::sync::Arc::clone(&shared)) as u64;
        let entry = crate::arch::resolver as usize as u64;
        code.write(layout.stubs, &crate::arch::stub_head(context, entry)).map_err(RunModeError::Sys)?;
        let mut next = layout.area;
        #[cfg(target_arch = "aarch64")]
        let (hook_veneer, span_veneer, alloc_veneer) = {
            let hv_off = next;
            code.write(hv_off, &crate::observe::veneer(crate::observe::hook_entry as *const () as usize as u64))
                .map_err(RunModeError::Sys)?;
            next += crate::observe::VENEER;
            let sv_off = next;
            code.write(sv_off, &crate::observe::veneer(crate::observe::span_entry as *const () as usize as u64))
                .map_err(RunModeError::Sys)?;
            next += crate::observe::VENEER;
            let av_off = next;
            code.write(av_off, &crate::observe::veneer(crate::observe::alloc_entry as *const () as usize as u64))
                .map_err(RunModeError::Sys)?;
            next += crate::observe::VENEER;
            ((base + hv_off) as u64, (base + sv_off) as u64, (base + av_off) as u64)
        };
        #[cfg(not(target_arch = "aarch64"))]
        let (hook_veneer, span_veneer, alloc_veneer) = (0u64, 0u64, 0u64);
        let hot_next = layout.hot;
        let at = |offset: usize| (base + offset) as u64;
        let regions = fault_report::Regions {
            edges: [
                at(layout.words),
                at(layout.address_words),
                at(layout.outside),
                at(layout.statics),
                at(layout.stubs),
                at(layout.thunks),
                at(layout.area),
                at(layout.hot),
                at(layout.whole),
            ],
        };
        // SAFETY: once a process, here, on the control thread, before the entry's stub is
        // written and so before any program code runs.
        unsafe { fault_report::install(regions) }?;
        let data_base = map_data()?;
        Ok(NativeMemory {
            bind,
            code,
            layout,
            base: base as u64,
            next,
            hot_next,
            outside: Vec::new_in(alloc),
            shared,
            unwind,
            data_base,
            data_next: 0,
            natives: None,
            hook_veneer,
            span_veneer,
            alloc_veneer,
            observe_stubs: Vec::new_in(alloc),
        })
    }

    fn at(&self, offset: usize) -> u64 {
        self.base + offset as u64
    }

    /// The offset of a run-view address in the mapping.
    fn offset(&self, at: u64) -> usize {
        (at - self.base) as usize
    }
}

impl<B: Bind> Memory for NativeMemory<'_, B> {
    fn stub(&mut self, slot: SlotId) -> Result<(), RunModeError> {
        if slot.0 >= MOST_SLOTS {
            return Err(RunModeError::SlotPast { slot: Slot(slot.0), most: MOST_SLOTS });
        }
        let stub_off = self.layout.stubs + crate::arch::STUB_HEAD + slot.index() * crate::arch::STUB;
        let thunk_off = self.layout.thunks + slot.index() * crate::arch::THUNK;
        let stub = crate::arch::stub(slot.0).map_err(RunModeError::Reach)?;
        self.code.write(stub_off, &stub).map_err(RunModeError::Sys)?;
        let thunk = crate::arch::thunk(self.at(thunk_off), self.word(slot)).map_err(RunModeError::Reach)?;
        self.code.write(thunk_off, &thunk).map_err(RunModeError::Sys)?;
        // The address word, then the call word: nothing names the slot before both hold.
        self.code.store(self.layout.address_words + slot.index() * 8, self.at(thunk_off)).map_err(RunModeError::Sys)?;
        self.code.store(self.layout.words + slot.index() * 8, self.at(stub_off)).map_err(RunModeError::Sys)?;
        Ok(())
    }

    fn word(&self, slot: SlotId) -> u64 {
        self.at(self.layout.words + slot.index() * 8)
    }

    fn address_word(&self, slot: SlotId) -> u64 {
        self.at(self.layout.address_words + slot.index() * 8)
    }

    fn thunk(&self, slot: SlotId) -> u64 {
        self.at(self.layout.thunks + slot.index() * crate::arch::THUNK)
    }

    fn outside(&mut self, record: RecordId, symbol: CodeSymbol, name: &str, weak: bool) -> Result<Outside, RunModeError> {
        let found = self.outside.binary_search_by_key(&symbol, |&(it, _)| it);
        let index = match found {
            Ok(at) => self.outside[at].1,
            Err(at) => {
                let count = self.outside.len() as u32;
                if count >= MOST_OUTSIDE {
                    return Err(RunModeError::OutsideFull);
                }
                let refused = |why| RunModeError::Outside { record, symbol, name: CName::new(name), why };
                let mut cname = [0u8; 512];
                if name.len() >= cname.len() || name.as_bytes().contains(&0) {
                    return Err(refused("its name is longer than 511 bytes or holds a NUL"));
                }
                cname[..name.len()].copy_from_slice(name.as_bytes());
                let Ok(cname) = core::ffi::CStr::from_bytes_until_nul(&cname) else {
                    return Err(refused("its name is longer than 511 bytes or holds a NUL"));
                };
                // 1. Runtime-owned interception (`LauncherOwn`, `ObservedBind`) always runs
                //    first, even when a native library is loaded, so `pthread_create`,
                //    `_Unwind_RaiseException`, and telemetry wrappers are never bypassed.
                // 2. Otherwise, a run with its native library binds through the library handle;
                //    without one, through the run's strategy (`Dlsym` or `Table`).
                //    For `Space::WeakOutside` (`weak == true`), a missing symbol resolves to 0.
                let address = match self.bind.intercept(name) {
                    Some(addr) => addr,
                    None => match self.natives.as_mut() {
                        Some(natives) => crate::native_lib::bind(Some(natives), record, symbol, name, weak)?,
                        None => match self.bind.address(name, cname) {
                            Ok(addr) => addr,
                            Err(bind::BindError::NotFound) if weak => 0,
                            Err(err) => return Err(refused(err.reason())),
                        },
                    },
                };
                self.code.store(self.layout.outside + count as usize * 8, address).map_err(RunModeError::Sys)?;
                self.outside.insert_shift(at, (symbol, count));
                count
            }
        };
        let word = self.at(self.layout.outside + index as usize * 8);
        // SAFETY: the word stored above, in a mapping never unmapped; only this thread writes it.
        let value = unsafe { (word as *const u64).read() };
        if !weak && value == 0 {
            return Err(RunModeError::Outside {
                record,
                symbol,
                name: CName::new(name),
                why: bind::BindError::NotFound.reason(),
            });
        }
        Ok(Outside { word, value })
    }

    fn reserve_hot(&mut self, len: usize, align: usize) -> Result<u64, RunModeError> {
        let at = self.hot_next.next_multiple_of(align);
        let end = at + len;
        if end > self.layout.whole {
            return Err(RunModeError::AreaFull { want: len as u64, left: (self.layout.whole - self.hot_next) as u64 });
        }
        self.hot_next = end;
        Ok(self.at(at))
    }

    fn reserve(&mut self, len: usize, align: usize) -> Result<u64, RunModeError> {
        let at = self.next.next_multiple_of(align);
        let end = at + len;
        if end > self.layout.hot {
            return Err(RunModeError::AreaFull { want: len as u64, left: (self.layout.hot - self.next) as u64 });
        }
        self.next = end;
        Ok(self.at(at))
    }

    fn carry(&mut self, step: Step, placing: &Placing<'_>) -> Result<(), RunModeError> {
        match step {
            Step::Copy => {
                self.code.copy(self.offset(placing.code_at), placing.code).map_err(RunModeError::Sys)?;
                self.code.copy(self.offset(placing.relro_at), placing.relro).map_err(RunModeError::Sys)?;
                if let Some((at, bytes, _)) = placing.entry {
                    self.code.copy(self.offset(at), bytes).map_err(RunModeError::Sys)?;
                }
            }
            Step::Flush => self.code.settle().map_err(RunModeError::Sys)?,
            Step::Register => {
                if let Some(span) = personality::span_of(placing) {
                    // SAFETY: `Writer::add`'s contract: the entry's bytes, made by eh.rs for
                    // `span.eh_at` and the code at `span.code_lo`, were copied to `eh_at`
                    // (Step::Copy) and the code to `code_lo` before this step; the area is never
                    // rewritten or unmapped in run mode (nothing is freed before tiers, piece 14),
                    // so both stay as they are for the process, and no `remove` is ever called.
                    unsafe { self.unwind.add(span) }.map_err(RunModeError::Unwinder)?;
                }
            }
            Step::Store => {
                self.code.store(self.layout.words + placing.slot.index() * 8, placing.code_at).map_err(RunModeError::Sys)?;
            }
        }
        Ok(())
    }

    fn static_word(&self, index: u32) -> u64 {
        self.at(self.layout.statics + index as usize * 8)
    }

    fn reserve_data(&mut self, len: usize, align: usize) -> Result<u64, RunModeError> {
        let at = self.data_next.next_multiple_of(align);
        let end = at + len;
        if end > DATA_BYTES {
            return Err(RunModeError::DataFull { want: len as u64, left: (DATA_BYTES - self.data_next) as u64 });
        }
        self.data_next = end;
        Ok(self.data_base + at as u64)
    }

    fn publish_static(&mut self, index: u32, at: u64, bytes: &[u8], room: Room) -> Result<(), RunModeError> {
        if index >= MOST_STATICS {
            return Err(RunModeError::StaticsFull);
        }
        match room {
            // Through the write view (the area is read-only in the run view, as a relro part
            // is).
            Room::Code => self.code.copy(self.offset(at), bytes).map_err(RunModeError::Sys)?,
            Room::Data => {
                let inside = at.checked_sub(self.data_base).and_then(|off| off.checked_add(bytes.len() as u64)).is_some_and(|end| end <= self.data_next as u64);
                if !inside {
                    return Err(RunModeError::DataPast { at, len: bytes.len() as u64 });
                }
                // SAFETY: `at..at + bytes.len()` lies in room `reserve_data` gave (checked
                // above) of the data mapping, read-write and never unmapped; no program code
                // can name it yet (its word is stored below), so nothing reads it meanwhile.
                unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), at as *mut u8, bytes.len()) };
            }
        }
        // Then the word: a load of the address finds the bytes.
        self.code.store(self.layout.statics + index as usize * 8, at).map_err(RunModeError::Sys)?;
        Ok(())
    }

    fn thread_key(&mut self, ender: u64) -> Result<ThreadKey, RunModeError> {
        thread_locals::native_thread_key(ender)
    }

    fn write(&mut self, at: u64, bytes: &[u8]) -> Result<(), RunModeError> {
        // Through the write view: the area is read-only in the run view.
        self.code.copy(self.offset(at), bytes).map_err(RunModeError::Sys)
    }

    fn note(&mut self, row: fault_report::Row) {
        fault_report::note(row);
    }

    fn ensure_observe_stub(&mut self, slot: SlotId, kind: ObserveKind) -> Result<u64, RunModeError> {
        if let Some(&(_, _, addr)) = self.observe_stubs.iter().find(|&&(s, k, _)| s == slot && k == kind) {
            return Ok(addr);
        }
        #[cfg(target_arch = "aarch64")]
        {
            let (stub_bytes, at) = match kind {
                ObserveKind::Hook => {
                    let at = self.reserve(crate::observe::HOOK_STUB, 8)?;
                    let bytes = crate::observe::hook_stub_slot(at, self.hook_veneer, slot.0, 0)
                        .map_err(RunModeError::Reach)?;
                    (bytes, at)
                }
                ObserveKind::Span => {
                    let at = self.reserve(crate::observe::SPAN_STUB, 8)?;
                    let bytes = crate::observe::span_stub(at, self.span_veneer, slot.0, 0)
                        .map_err(RunModeError::Reach)?;
                    (bytes, at)
                }
                ObserveKind::Alloc(ak) => {
                    let at = self.reserve(crate::observe::ALLOC_STUB, 8)?;
                    let bytes = crate::observe::alloc_stub(at, self.alloc_veneer, ak, 0)
                        .map_err(RunModeError::Reach)?;
                    (bytes, at)
                }
                ObserveKind::Count | ObserveKind::Mark => {
                    return Err(RunModeError::NotBuilt {
                        what: "count/mark observe stubs in run_mode (use Hook, Span, or Alloc)",
                    });
                }
            };
            self.code.write(self.offset(at), &stub_bytes).map_err(RunModeError::Sys)?;
            self.observe_stubs.push((slot, kind, at));
            Ok(at)
        }
        #[cfg(not(target_arch = "aarch64"))]
        {
            let _ = (slot, kind);
            Err(RunModeError::NotBuilt { what: "observe stubs on non-AArch64" })
        }
    }

    fn store_body_word(&mut self, slot: SlotId, body: u64) -> Result<(), RunModeError> {
        #[cfg(target_arch = "aarch64")]
        {
            for i in 0..self.observe_stubs.len() {
                let (s, k, at) = self.observe_stubs[i];
                if s == slot {
                    let rt_kind = match k {
                        ObserveKind::Count => crate::observe::Kind::Count,
                        ObserveKind::Mark => crate::observe::Kind::Mark,
                        ObserveKind::Hook => crate::observe::Kind::Hook,
                        ObserveKind::Span => crate::observe::Kind::Span,
                        ObserveKind::Alloc(_) => crate::observe::Kind::Alloc,
                    };
                    let body_off = self.offset(at) + crate::observe::body_offset(rt_kind);
                    self.code.store(body_off, body).map_err(RunModeError::Sys)?;
                }
            }
        }
        #[cfg(not(target_arch = "aarch64"))]
        let _ = (slot, body);
        Ok(())
    }

    fn store_slot_word(&mut self, slot: SlotId, target: u64) -> Result<(), RunModeError> {
        self.code.store(self.layout.words + slot.index() * 8, target).map_err(RunModeError::Sys)
    }
}

/// The driver's clock: `CLOCK_MONOTONIC`, read at its edge.
fn now() -> Nanos {
    let mut at = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `at` is a live local the call fills.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut at) };
    Nanos(at.tv_sec as u64 * 1_000_000_000 + at.tv_nsec as u64)
}

/// Writes `line` to standard error and ends the process with [`STOPPED`], without running the
/// program's `atexit` functions (they are its code, and no code can come any more).
fn stop_process(line: &[u8]) -> ! {
    // SAFETY: a write of a live buffer to descriptor 2, then the process's end.
    unsafe {
        libc::write(2, line.as_ptr().cast(), line.len());
        libc::_exit(STOPPED)
    }
}

/// A line in a fixed buffer: the stop's words, made without allocating.
struct Line {
    bytes: [u8; 1024],
    len: usize,
}

impl fmt::Write for Line {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let room = self.bytes.len() - self.len;
        let take = text.len().min(room);
        self.bytes[self.len..self.len + take].copy_from_slice(&text.as_bytes()[..take]);
        self.len += take;
        Ok(())
    }
}

fn stop_with(args: fmt::Arguments<'_>) -> ! {
    let mut line = Line { bytes: [0; 1024], len: 0 };
    let _ = fmt::Write::write_fmt(&mut line, args);
    let end = line.len.min(line.bytes.len() - 1);
    line.bytes[end] = b'\n';
    stop_process(&line.bytes[..end + 1])
}

/// Writes all of `bytes` to the parent's socket, each part after a bounded wait for room.
fn send(fd: i32, mut bytes: &[u8]) -> Result<(), RunModeError> {
    // The socket held for the whole write, so a crash report's frame never lands inside it.
    let _held = fault_report::hold_channel()?;
    while !bytes.is_empty() {
        let mut poll = libc::pollfd { fd, events: libc::POLLOUT, revents: 0 };
        // The wait "run mode: room on the parent's socket", bounded by the ask's own bound.
        // SAFETY: one live pollfd.
        let ready = unsafe { libc::poll(&mut poll, 1, (ASK_BOUND.0 / 1_000_000) as i32) };
        match ready {
            0 => return Err(RunModeError::Socket { call: "poll (run mode: room on the parent's socket; deadline passed)", errno: libc::ETIMEDOUT }),
            n if n < 0 && errno() == libc::EINTR => continue,
            n if n < 0 => return Err(RunModeError::Socket { call: "poll (run mode: room on the parent's socket)", errno: errno() }),
            _ => {}
        }
        // SAFETY: `bytes` is live for the call.
        let wrote = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        match wrote {
            n if n > 0 => bytes = &bytes[n as usize..],
            // The same write, interrupted before a byte moved: not a retry of a failed one.
            n if n < 0 && errno() == libc::EINTR => {}
            _ => return Err(RunModeError::Socket { call: "write (run mode: a frame to the parent)", errno: errno() }),
        }
    }
    Ok(())
}

/// The socket's reader: each read's bytes, and a tick each time [`TICK_MS`] passes with none,
/// onto the control thread's queue. A tick is an empty `Event::Frame` (a read never gives one:
/// zero bytes is the end, sent as `Event::Closed`).
fn read_socket(fd: i32, shared: std::sync::Arc<crate::table::Shared>) {
    use crate::table::Event as Queued;

    let mut buffer = [0u8; 1 << 16];
    loop {
        let mut poll = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // The wait "run mode: the parent's next bytes", bounded by the tick.
        // SAFETY: one live pollfd.
        let ready = unsafe { libc::poll(&mut poll, 1, TICK_MS) };
        if ready == 0 {
            shared.send(Queued::Frame(std::vec::Vec::new_in(blaze_launch::KEPT)));
            continue;
        }
        if ready < 0 {
            if errno() == libc::EINTR {
                continue;
            }
            shared.send(Queued::Closed(format_errno("poll", errno())));
            return;
        }
        // SAFETY: `buffer` is live and writable for its length.
        let got = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        match got {
            0 => {
                shared.send(Queued::Closed(format_errno("read (the parent closed the socket)", 0)));
                return;
            }
            n if n > 0 => {
                let mut chunk = std::vec::Vec::with_capacity_in(n as usize, blaze_launch::KEPT);
                chunk.extend_from_slice(&buffer[..n as usize]);
                shared.send(Queued::Frame(chunk));
            }
            _ if errno() == libc::EINTR => {}
            _ => {
                shared.send(Queued::Closed(format_errno("read", errno())));
                return;
            }
        }
    }
}

/// The text `Event::Closed` carries (table.rs's form is a `String`).
fn format_errno(call: &str, errno: i32) -> String {
    let mut text = String::new();
    let _ = fmt::Write::write_fmt(&mut text, format_args!("{call} on the parent's socket ended (errno {errno})"));
    text
}

/// Runs the program under its parent, the socket at `fd`: maps the run's memory, starts the
/// control thread and the socket's reader, numbers the entry `entry`, and calls it on this
/// thread (the process's first) with what `handed` holds, its C names bound by `bind`
/// (run_mode/bind.rs). How it ends is `end` (G5): [`End::Exit`] ends the process through
/// `exit` with the program's status and never returns `Ok`; [`End::Return`] stops the control
/// thread and returns `Ok(status)` to the caller, which owns the process. `Err` when the program
/// could not be started. A stop mid-run (code that could not come, a fault) still ends the
/// process with [`STOPPED`] under either end.
///
/// `alloc` crosses to the control thread, so it is a [`blaze_own::SharedAlloc`]; it lives for
/// the process, because the run's unwind registry is made in it and the system's unwinder keeps
/// the registry's finder until the process ends (run_mode/personality.rs `run_registry`).
///
/// How the program's native library is loaded (`BLAZE_LAUNCH_NATIVE`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NativeStart {
    /// Eager: the parent writes `Down::Native` first; the control thread loads it and the first
    /// thread runs its initializers before calling the entry (`blaze run`, or images with inits).
    Eager(RecordId),
    /// Lazy: the entry runs immediately without loading the library; if the parent later sends
    /// `Down::Native` for this exact record before publishing foreign code, the control thread
    /// loads it on demand and publishes any C initializers to run once on the waking mutator
    /// thread before entering the newly bound function.
    Lazy(RecordId),
}

/// Parses `BLAZE_LAUNCH_NATIVE`: `<64 hex>` for eager loading, or `lazy:<64 hex>` for lazy loading.
pub fn parse_native_start(raw: &[u8]) -> Result<NativeStart, EntryRefused> {
    if let Some(hex) = raw.strip_prefix(b"lazy:") {
        Ok(NativeStart::Lazy(parse_record(hex)?))
    } else {
        Ok(NativeStart::Eager(parse_record(raw)?))
    }
}

/// `native` is the program's native library start mode when its start names one
/// (`BLAZE_LAUNCH_NATIVE`, [`parse_native_start`]; the parent's rule is wire/run.rs `Down::Native`'s):
/// under [`NativeStart::Eager`], the control thread reads frames until its `Native`, at most
/// [`NATIVE_WAIT_MS`], loads the library from `source`'s folder (`dlopen`, its words and
/// pointer cells filled) BEFORE the entry's thunk is handed here, and this thread runs the
/// library's initializers ([`crate::native_lib::run_inits`]) before the entry is called; under
/// [`NativeStart::Lazy`], the entry starts immediately and the library loads on demand if the
/// parent sends `Down::Native` for that exact record mid-run. `None`: nothing waits and nothing
/// is opened.
#[expect(clippy::too_many_arguments, reason = "the run's edge: each argument one handle or fact of it")]
pub fn run<S: RecordSource + RecordFiles + Sync, B: Bind + Send>(
    fd: std::os::fd::OwnedFd,
    source: &S,
    entry: CodeSymbol,
    native: Option<NativeStart>,
    handed: Handed,
    bind: B,
    end: End,
    alloc: blaze_own::SharedAlloc<'static>,
) -> Result<i32, RunModeError> {
    use std::os::fd::AsRawFd;

    if cfg!(not(target_arch = "aarch64")) {
        return Err(RunModeError::NotBuilt { what: "run mode on x86-64: code records are AArch64 (blaze-code a64)" });
    }
    let raw = fd.as_raw_fd();
    // Capture process `argc`/`argv` once before guest execution starts so eager initializers,
    // lazy initializers (allocation-free in `table::resolve`), and `Handed::CMain` all share the
    // exact same argument array.
    let (argc, argv) = match handed {
        Handed::Comptime(args) => (args.argc, args.argv),
        Handed::CMain => process_args(),
        Handed::Unit | Handed::Hosted(_) => {
            if cfg!(target_os = "linux") {
                process_args()
            } else {
                (0, core::ptr::null())
            }
        }
    };
    let eager_native = matches!(native, Some(NativeStart::Eager(_)));
    let open_ask: crate::sys::Handoff<Option<RecordId>> = crate::sys::Handoff::new();
    let open_ans: crate::sys::Handoff<Result<crate::native_lib::DlNatives, RunModeError>> = crate::sys::Handoff::new();
    let ready: crate::sys::Handoff<Ready> = crate::sys::Handoff::new();
    let open_bridge = eager_native.then_some((&open_ask, &open_ans));
    std::thread::scope(|scope| {
        let control = std::thread::Builder::new()
            .name("blaze-run-control".to_owned())
            .spawn_scoped(scope, || control(raw, source, entry, native, bind, alloc, open_bridge, &ready));
        let Ok(control) = control else {
            return Err(RunModeError::Thread { what: "control" });
        };
        if eager_native {
            if let Some(record) = open_ask.take() {
                // Run `dlopen` on the initial thread so any `DT_NEEDED` dependency constructors
                // execute on the initial thread before `main`.
                open_ans.put(crate::native_lib::DlNatives::open_record(source.folder(), record));
            }
        }
        // The wait "run mode: the entry's stub written": the control thread answers at once,
        // with the thunk or with why it has none (and then returns, so the scope's join ends).
        let (thunk, shared, loaded) = ready.take()?;
        let me = match crate::threads::adopt(&shared) {
            Ok(me) => me,
            Err(_) => stop_with(format_args!("blaze: the program could not start: pthread_key_create failed")),
        };
        // The native library's initializers, on this first thread, after the entry is numbered
        // and before it is called (native_lib.rs step 5): an executable's order. They may call
        // Rust words: each is a slot's thunk, served by the control thread, already in its loop.
        if let Some(loaded) = loaded {
            if let Err(why) = crate::native_lib::run_inits_with(loaded, argc, argv) {
                drop(me);
                shared.stop();
                let _ = control.join();
                return Err(why);
            }
        }
        let exit = call_entry_with(thunk, handed, argc, argv);
        match (exit, end) {
            (EntryExit::Returned(status), End::Exit | End::ExitOrReturnUnwound) => {
                // The main thread stays a mutator to the end: `exit` runs the program's
                // `atexit` functions and TLS destructors, which are its code and may trigger
                // demand-compilation through the live control thread.
                core::mem::forget(me);
                // SAFETY: ends the process as a C program's return from `main` does; `fd` and
                // every mapping end with it.
                unsafe { libc::exit(status) }
            }
            (EntryExit::Unwound, End::Exit) => {
                // An uncaught guest panic unwound out of the entry (for instance a failing
                // `#[test]` entered via `Handed::Unit`): guest destructors and the guest panic
                // hook have already run. End the process cleanly with `SIGABRT` rather than
                // letting the guest exception cross into `std::thread::scope`'s host
                // `catch_unwind` (whose `panic_unwind` has a different `CANARY` and aborts with
                // `"Rust cannot catch foreign exceptions"`).
                core::mem::forget(me);
                crate::observe::abort_process()
            }
            (EntryExit::Returned(status), End::Return) => {
                // The control thread's loop ends at `Stop` (no code is placed after it: the
                // program's one thread has returned). The wait "run mode: the control thread
                // stops" is the join; it is bounded by the loop's next event, at most the
                // reader's tick away. The mappings stay: the process's owner ends it.
                // The thread's end is told first, so the loop sees it before `Stop`.
                drop(me);
                shared.stop();
                if control.join().is_err() {
                    return Err(RunModeError::Thread { what: "control (it panicked while stopping)" });
                }
                Ok(status)
            }
            (EntryExit::Unwound, End::Return | End::ExitOrReturnUnwound) => {
                drop(me);
                shared.stop();
                if control.join().is_err() {
                    return Err(RunModeError::Thread { what: "control (it panicked while stopping)" });
                }
                Err(RunModeError::Unwound)
            }
        }
    })
}

/// The process's own `argc` and `argv`, built in `blaze_launch::KEPT` for the process's life
/// and shared by `Handed::CMain` and Linux native-library initializers (`native_lib::run_inits`).
pub(crate) fn process_args() -> (core::ffi::c_int, *const *const core::ffi::c_char) {
    use core::ffi::{c_char, c_int};
    use std::os::unix::ffi::OsStringExt;

    // The program's `argv`, leaked for its life (it is the process's).
    let mut args: std::vec::Vec<*const c_char, _> = std::vec::Vec::new_in(blaze_launch::KEPT);
    for arg in std::env::args_os() {
        match std::ffi::CString::new(arg.into_vec()) {
            Ok(it) => args.push(it.into_raw().cast_const()),
            Err(_) => stop_with(format_args!("blaze: an argument of this process has a NUL in it")),
        }
    }
    args.push(core::ptr::null());
    let argc = c_int::try_from(args.len() - 1).unwrap_or(c_int::MAX);
    let argv = args.leak().as_ptr();
    (argc, argv)
}

/// How the program's entry ended when called from the launcher's run thread.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EntryExit {
    /// Returned normally with the `i32` process exit status.
    Returned(i32),
    /// Unwound out of the entry via an uncaught guest exception.
    Unwound,
}

/// Calls the entry's thunk on this thread with what `handed` holds: [`EntryExit::Returned`]
/// with the `i32` the process exits with, or [`EntryExit::Unwound`] when an uncaught guest
/// exception unwound out of the entry. Every form's status is the same rule (the in-process
/// runner's, blaze-ladder place.rs `Placed::call`): `fn()` gives 0, and an `i32`-returning
/// entry's value is handed to `exit` whole, whose status the parent sees as its low 8 bits. A
/// hosted program that calls its table's `exit` never returns here: the host's `exit` (bstd
/// hosting.rs) writes what its streams hold and ends the process with that status, the same low
/// 8 bits.
///
/// The guest runs its own `std` and `panic_unwind` (with its own `CANARY` static). Wrapping the
/// call in `core::intrinsics::catch_unwind` installs a raw catch-all landing pad at the
/// host/guest boundary so phase 2 runs all guest destructors cleanly and stops the unwind here
/// without invoking host `panic_unwind::__rust_panic_cleanup` (which would reject the guest's
/// `CANARY` with `"fatal runtime error: Rust cannot catch foreign exceptions, aborting"`) or
/// `_Unwind_DeleteException` (which would invoke guest `__rust_drop_panic`).
#[cfg(test)]
fn call_entry(thunk: u64, handed: Handed) -> EntryExit {
    let (argc, argv) = match handed {
        Handed::CMain => process_args(),
        _ => (0, core::ptr::null()),
    };
    call_entry_with(thunk, handed, argc, argv)
}

fn call_entry_with(thunk: u64, handed: Handed, argc: core::ffi::c_int, argv: *const *const core::ffi::c_char) -> EntryExit {
    use core::ffi::{c_char, c_int, c_void};

    enum Call {
        Hosted { entry: extern "C-unwind" fn(*const c_void) -> c_int, table: *const c_void },
        Unit { entry: extern "C-unwind" fn() },
        Main { entry: extern "C-unwind" fn(c_int, *const *const c_char) -> c_int, argc: c_int, argv: *const *const c_char },
    }

    struct State {
        call: Call,
        status: i32,
    }

    // SAFETY: `data` points to the live `State` local in `call_entry` and is called once.
    unsafe fn do_call(data: *mut State) {
        let state = unsafe { &mut *data };
        state.status = match state.call {
            Call::Hosted { entry, table } => entry(table),
            Call::Unit { entry } => {
                entry();
                0
            }
            Call::Main { entry, argc, argv } => entry(argc, argv),
        };
    }

    // `catch_unwind` requires `catch_fn` never to unwind; leaving the guest exception object
    // untouched avoids both host `__rust_panic_cleanup` (foreign `CANARY`) and
    // `_Unwind_DeleteException` (guest `__rust_drop_panic`).
    unsafe fn do_catch(_data: *mut State, _exception: *mut u8) {}

    let entry_addr = thunk as usize;
    let call = match handed {
        Handed::Hosted(table) => {
            // SAFETY: `thunk` is the entry slot's thunk, in a mapping never unmapped (or, in a
            // test, a function of this form); the function there is bstd's hosted entry,
            // `extern "C-unwind" fn(*const HostTable) -> i32` (the form the parent named,
            // `hosted:`), compiled when this first call fires its stub. `table` lives across
            // the call and every later reach of the program (`HostTableAt::new`'s contract).
            let entry: extern "C-unwind" fn(*const c_void) -> c_int = unsafe { core::mem::transmute(entry_addr) };
            Call::Hosted { entry, table: table.0.as_ptr().cast_const() }
        }
        Handed::Unit => {
            // SAFETY: `thunk` is the entry slot's thunk, in a mapping never unmapped; the
            // function there is a guest `fn()` (which may unwind on panic), compiled when this
            // first call fires its stub.
            let entry: extern "C-unwind" fn() = unsafe { core::mem::transmute(entry_addr) };
            Call::Unit { entry }
        }
        Handed::CMain => {
            // SAFETY: as above, for `extern "C-unwind" fn(c_int, *const *const c_char) -> c_int`.
            let entry: extern "C-unwind" fn(c_int, *const *const c_char) -> c_int =
                unsafe { core::mem::transmute(entry_addr) };
            Call::Main { entry, argc, argv }
        }
        Handed::Comptime(args) => {
            // SAFETY: as for `CMain` (the form the parent named, `comptime:`); `args` holds the
            // host's arguments, live for the process (`ComptimeArgs::new`'s contract).
            let entry: extern "C-unwind" fn(c_int, *const *const c_char) -> c_int =
                unsafe { core::mem::transmute(entry_addr) };
            Call::Main { entry, argc: args.argc, argv: args.argv }
        }
    };

    let mut state = State { call, status: 0 };
    // SAFETY: `state` is a live, aligned local; `do_call` accesses it only during this call;
    // `do_catch` never unwinds.
    let unwound = unsafe { core::intrinsics::catch_unwind(do_call, &raw mut state, do_catch) };
    if unwound { EntryExit::Unwound } else { EntryExit::Returned(state.status) }
}

/// Counts the thread events the run mode does not act on (births, ends, reports: it retires
/// nothing before tiers, piece 14), so a hang's report can say how many there were.
static THREAD_EVENTS: AtomicU64 = AtomicU64::new(0);

/// What the control thread hands the first thread: the entry's thunk, the words' owner and the
/// native library loaded (its initializers are the first thread's to run), or why the program
/// cannot start.
type Ready = Result<(u64, std::sync::Arc<crate::table::Shared>, Option<crate::native_lib::Loaded>), RunModeError>;

/// How long the control thread waits for the parent's `Native` when the start names a library
/// (rule 18): the parent writes it first, before anything is built, so it is on the socket
/// within the spawn's own time; past this the start is refused ([`NativeError::NotSent`]).
pub const NATIVE_WAIT_MS: u64 = 10_000;

/// A record's id from its 64 lowercase or uppercase hex digits (BLAZE_LAUNCH_NATIVE's value).
pub fn parse_record(hex: &[u8]) -> Result<RecordId, EntryRefused> {
    if hex.len() != 64 {
        return Err(EntryRefused { why: "the record is not 64 hex digits" });
    }
    let digit = |it: u8| (it as char).to_digit(16).map(|it| it as u8);
    let mut id = [0u8; 32];
    for (byte, pair) in id.iter_mut().zip(hex.chunks(2)) {
        match (digit(pair[0]), digit(pair[1])) {
            (Some(high), Some(low)) => *byte = high << 4 | low,
            _ => return Err(EntryRefused { why: "the record holds a character that is no hex digit" }),
        }
    }
    Ok(RecordId(id))
}

/// Whether the control thread expects a `Down::Native` from the parent, and when.
enum NativeExpect {
    None,
    Eager { record: RecordId, since: Nanos },
    Lazy { record: RecordId },
}

/// Ensures the initial thread waiting on `open_ask` for an eager native library is always woken
/// with `None` if `control` returns early before reaching `Act::Native`.
struct EagerOpenGate<'a> {
    bridge: Option<(&'a crate::sys::Handoff<Option<RecordId>>, &'a crate::sys::Handoff<Result<crate::native_lib::DlNatives, RunModeError>>)>,
}

impl<'a> EagerOpenGate<'a> {
    fn open(&mut self, record: RecordId) -> Option<Result<crate::native_lib::DlNatives, RunModeError>> {
        let (ask, ans) = self.bridge.take()?;
        ask.put(Some(record));
        Some(ans.take())
    }
}

impl Drop for EagerOpenGate<'_> {
    fn drop(&mut self) {
        if let Some((ask, _)) = self.bridge.take() {
            ask.put(None);
        }
    }
}

/// The control thread: maps the run's memory (its tables are in the run's allocator, which
/// stays on this thread), starts the socket's reader, writes the entry's stub, then is the one
/// writer of code and words for the life of the process.
#[expect(clippy::too_many_arguments, reason = "receives the run's handoffs and eager native open bridge")]
fn control<S: RecordSource + RecordFiles, B: Bind>(
    fd: i32,
    source: &S,
    entry: CodeSymbol,
    native: Option<NativeStart>,
    bind: B,
    alloc: blaze_own::SharedAlloc<'static>,
    open_bridge: Option<(&crate::sys::Handoff<Option<RecordId>>, &crate::sys::Handoff<Result<crate::native_lib::DlNatives, RunModeError>>)>,
    ready: &crate::sys::Handoff<Ready>,
) {
    use crate::stub_wait::Answer;
    use crate::table::Event as Queued;

    let mut open_gate = EagerOpenGate { bridge: open_bridge };

    // The one unwind registry, before any record is placed: the finder installed (macOS) and
    // its writer kept by this thread, the one writer of code and words.
    let unwind = match personality::run_registry(alloc) {
        Ok(unwind) => unwind,
        Err(why) => return ready.put(Err(why)),
    };
    let alloc = alloc.local();
    let mut memory = match NativeMemory::new(bind, alloc, unwind) {
        Ok(memory) => memory,
        Err(why) => return ready.put(Err(why)),
    };
    // The crash report tells the parent where the program struck on this socket (its frame
    // and `send`'s never interleave: `fault_report::hold_channel`).
    fault_report::channel(fd);
    let shared = std::sync::Arc::clone(&memory.shared);
    let reader_shared = std::sync::Arc::clone(&shared);
    let reader = std::thread::Builder::new().name("blaze-run-read".to_owned()).spawn(move || read_socket(fd, reader_shared));
    if reader.is_err() {
        return ready.put(Err(RunModeError::Thread { what: "socket reader" }));
    }
    let mut mode = RunMode::new(alloc);
    let mut acts: AVec<'_, Act> = Vec::new_in(alloc);
    let entry = match mode.start(entry, &mut acts) {
        Ok(entry) => entry,
        Err(why) => return ready.put(Err(why)),
    };
    for index in 0..acts.len() {
        let Act::Stub { slot } = acts[index] else { continue };
        if let Err(why) = memory.stub(slot) {
            return ready.put(Err(why));
        }
    }
    acts.clear();
    // The entry's thunk goes to the first thread now (for `None` or `NativeStart::Lazy`), or,
    // under `NativeStart::Eager`, once that library is loaded (`Act::Native` below; bounded by
    // `NATIVE_WAIT_MS`, rule 18).
    let mut expect = match native {
        Some(NativeStart::Eager(_)) if cfg!(not(any(target_os = "macos", target_os = "linux"))) => {
            return ready.put(Err(RunModeError::NotBuilt { what: "a program's native library on this platform (NATIVE.md W5)" }));
        }
        Some(NativeStart::Eager(record)) => NativeExpect::Eager { record, since: now() },
        None => NativeExpect::None,
        Some(NativeStart::Lazy(record)) => NativeExpect::Lazy { record },
    };
    if !matches!(expect, NativeExpect::Eager { .. }) {
        ready.put(Ok((memory.thunk(entry), std::sync::Arc::clone(&shared), None)));
    }
    // Threads waiting in a stub: the slot, the thread, and the stub waiters' row its answer
    // goes to (`Waiters::answer`).
    let mut waiting: AVec<'_, (SlotId, Thread, u32)> = Vec::new_in(alloc);
    loop {
        // The wait "run mode: the next event" (a stub, the parent's bytes, a tick): bounded by
        // the reader's tick, [`TICK_MS`].
        let event = shared.wait();
        let now = now();
        let told = match event {
            Queued::Need { slot, thread, row } => {
                waiting.push((SlotId(slot.0), thread, row));
                mode.on(Event::Tick(now), &mut acts).and_then(|()| mode.on(Event::Fired { thread, slot: SlotId(slot.0) }, &mut acts))
            }
            Queued::Frame(bytes) if bytes.is_empty() => mode.on(Event::Tick(now), &mut acts),
            Queued::Frame(bytes) => mode.on(Event::Frame(&bytes), &mut acts),
            Queued::Closed(_) => mode.on(Event::Closed, &mut acts),
            Queued::Thread(_) => {
                THREAD_EVENTS.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Queued::Stop => return,
        };
        if let Err(why) = told {
            stop_with(format_args!("blaze: the program stopped: its code could not come: {why}"));
        }
        // A work list: a placement adds the wakes it makes.
        let mut next = 0;
        while next < acts.len() {
            let act = acts[next];
            next += 1;
            let carried = match act {
                Act::Stub { slot } => memory.stub(slot),
                Act::Place { slot, record } => mode.place(slot, record, source, &mut memory, Plant::None, &mut acts),
                Act::Promote { slot, record } => mode.promote(slot, record, source, &mut memory, Plant::None),
                Act::Statics => mode.place_statics(source, &mut memory),
                Act::ThreadLocals => mode.make_thread_locals(&mut memory),
                Act::Hook { slot, kind } => mode.hook(slot, kind, &mut memory),
                Act::Wake { thread, slot } => {
                    match waiting.iter().position(|(at, who, _)| *at == slot && *who == thread) {
                        Some(index) => {
                            let (_, _, row) = waiting.remove_shift(index);
                            shared.waiters.answer(row, Answer::Bound);
                        }
                        None => stop_with(format_args!(
                            "blaze: the run mode woke thread {} in slot {}, which waits there no more: the control thread is out of step",
                            thread.0, slot.0
                        )),
                    }
                    Ok(())
                }
                Act::Stop(Stop::Failed { slot, symbol, why }) => match why {
                    Why::NotBuilt { .. } => stop_with(format_args!(
                        "blaze: the program called function {symbol} (slot {}), which blaze cannot build yet: {}",
                        slot.0,
                        mode.parent_said()
                    )),
                    _ => stop_with(format_args!(
                        "blaze: the program called function {symbol} (slot {}), which has no code: {}",
                        slot.0,
                        WhyText(why)
                    )),
                },
                Act::Stop(Stop::ParentGone { waiting }) => stop_with(format_args!(
                    "blaze: the program's parent (`blaze run`) is gone while {waiting} threads wait for code"
                )),
                // The library the start named (BLAZE_LAUNCH_NATIVE): under Eager, loaded before
                // the entry's thunk is handed over; under Lazy, loaded on demand before placing
                // the first foreign record/datum (any C initializers run lazily on the waking
                // mutator thread before entering the newly bound function). A `Native` the start
                // did not name, or another record than it named, is the parent out of step:
                // stopped by name.
                Act::Native { record } => match core::mem::replace(&mut expect, NativeExpect::None) {
                    NativeExpect::Eager { record: named, .. } if named == record => {
                        let opened = match open_gate.open(record) {
                            Some(Ok(natives)) => load_library_opened(&mut mode, natives, &mut memory),
                            Some(Err(why)) => Err(why),
                            None => Err(RunModeError::NotBuilt {
                                what: "eager native library open bridge missing (cannot open on control thread)",
                            }),
                        };
                        match opened {
                            Ok(loaded) => {
                                ready.put(Ok((memory.thunk(entry), std::sync::Arc::clone(&shared), Some(loaded))));
                                Ok(())
                            }
                            Err(why) => return ready.put(Err(why)),
                        }
                    }
                    NativeExpect::Lazy { record: named } if named == record => {
                        if cfg!(not(any(target_os = "macos", target_os = "linux"))) {
                            Err(RunModeError::NotBuilt { what: "a program's native library on this platform (NATIVE.md W5)" })
                        } else {
                            match load_library(&mut mode, source, &mut memory) {
                                Ok(loaded) if cfg!(target_os = "linux") && loaded.inits > 0 => Err(RunModeError::NotBuilt {
                                    what: "lazy native library initializers on Linux (images with C initializers must load eagerly)",
                                }),
                                Ok(loaded) => {
                                    shared.publish_lazy_inits(loaded);
                                    Ok(())
                                }
                                Err(why) => Err(why),
                            }
                        }
                    }
                    _ => stop_with(format_args!(
                        "blaze: the parent sent native library {record:?}, which this program's start did not name (BLAZE_LAUNCH_NATIVE; NATIVE.md 3.7)"
                    )),
                },
            };
            if let Err(why) = carried {
                stop_with(format_args!("blaze: the program stopped: its code could not come: {why}"));
            }
        }
        // The start's wait for an eager parent `Native`, bounded (rule 18): the reader's tick
        // brings an event at least every TICK_MS, so this is checked while nothing comes.
        if let NativeExpect::Eager { since, .. } = &expect
            && now.0.saturating_sub(since.0) > NATIVE_WAIT_MS * 1_000_000
        {
            return ready.put(Err(RunModeError::Native(NativeError::NotSent { bound_ms: NATIVE_WAIT_MS })));
        }
        acts.clear();
        if !mode.outbox().is_empty() {
            if let Err(why) = send(fd, mode.outbox()) {
                stop_with(format_args!("blaze: the program stopped: {why}"));
            }
            mode.sent();
        }
    }
}

/// A failure's words.
struct WhyText(Why);

impl fmt::Display for WhyText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Why::Refused(findings) => write!(f, "its compile was refused; findings {}", Short(&findings.0)),
            Why::Panicked => f.write_str("its compile panicked (rule 20: this function alone fails)"),
            Why::NotBuilt { what } => write!(f, "not built yet: {what}"),
            Why::Overran { bound } => write!(f, "no answer within {} ms (rule 18)", bound.0 / 1_000_000),
            Why::Unresolved { symbol } => {
                write!(f, "it names function {}, which the program's manifest does not resolve", Short(&symbol.0))
            }
        }
    }
}

pub mod bin_remap;
pub mod bind;
pub mod fault_report;
mod native;
mod personality;
mod received;
mod thread_locals;

/// Carries `Act::Native`: the library `mode` was told opened from `source`'s folder and its words
/// and pointer cells filled ([`RunMode::load_native`] through [`crate::native_lib::DlNatives`]),
/// then kept in `memory` so every C name after it binds through the library's handle. A `dlopen`
/// refusal is typed from its `dlerror` text ([`crate::native_lib::open_refused`]): the first
/// name dyld could not bind, or its words.
///
/// What the load rests on (the review rule's "unsafe resting on an assumption", stated here for
/// the whole load; the calls' own SAFETY comments are in native_lib.rs `DlNatives::open`):
/// - the file is `<the command's home>/.blaze/v2/run/records/<64 hex>` (BLAZE_LAUNCH_STORE, set by
///   the parent from the command's own `HOME`, a folder only its user writes), named by the
///   image record's key, which covers every input of its bytes (rule 21); the parent filed the
///   payload of the store's record there once, the store having checked the record's bytes when
///   it was put (blaze-cli engine_native.rs `send_of`: written once, never rewritten in place);
/// - `dlopen` runs the initializers of the libraries the image loads (libSystem's, already
///   run in this process) and none of the image's own (`__blaze_inits`, run by the first thread
///   before the entry: `run`); all of it runs in THIS process, inside whatever sandbox the
///   parent started it under (for `blaze test`, the test's sandbox), never in the parent;
/// - each word and pointer cell gets the entry thunk of the slot the parent paired with it: a
///   Rust function C calls is assumed to have the ABI the Rust side declared (`extern "C"`
///   with the C prototype's types). Nothing here can check that: neither the image nor the
///   code record carries a type, so a mismatch is the program's own undefined behaviour, as
///   it is under a native link.
fn load_library<S: RecordFiles, B: Bind>(mode: &mut RunMode<'_>, source: &S, memory: &mut NativeMemory<'_, B>) -> Result<crate::native_lib::Loaded, RunModeError> {
    let natives = crate::native_lib::DlNatives::new(source.folder())?;
    load_library_opened(mode, natives, memory)
}

fn load_library_opened<B: Bind>(
    mode: &mut RunMode<'_>,
    mut natives: crate::native_lib::DlNatives,
    memory: &mut NativeMemory<'_, B>,
) -> Result<crate::native_lib::Loaded, RunModeError> {
    let loaded = match mode.load_native(&mut natives, memory) {
        Ok(loaded) => loaded,
        Err(RunModeError::Native(NativeError::Open { record })) => return Err(RunModeError::Native(crate::native_lib::open_refused(record, natives.said()))),
        Err(why) => return Err(why),
    };
    memory.natives = Some(natives);
    Ok(loaded)
}

pub use bind::{Bind, BindError, Dlsym, LauncherOwn, ObservedBind, Table, TableError};
pub use received::SentRefused;
pub use thread_locals::ThreadKey;

#[cfg(test)]
mod bind_tests;
#[cfg(test)]
mod entry_tests;
#[cfg(test)]
mod fault_code_window_tests;
#[cfg(test)]
mod fault_walk_tests;
#[cfg(test)]
mod fault_wire_tests;
#[cfg(test)]
mod personality_tests;
#[cfg(test)]
mod statics_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod position_tests {
    use super::*;
    use blaze_code::record::{CodeRecord, PositionField, PositionPart, PositionRow};
    use blaze_launch::wire::List;

    struct Source<'a> {
        record: RecordId,
        bytes: &'a [u8],
    }

    impl RecordSource for Source<'_> {
        fn bytes(&self, id: RecordId) -> Result<&[u8], RunModeError> {
            assert_eq!(id, self.record, "both slots name the same content-addressed record");
            Ok(self.bytes)
        }
    }

    /// A reader observes the resolved relro word when the slot is stored.
    #[derive(Default)]
    struct MemorySeen {
        next: u64,
        stored: std::vec::Vec<(SlotId, u32)>,
    }

    impl Memory for MemorySeen {
        fn stub(&mut self, _: SlotId) -> Result<(), RunModeError> { Ok(()) }
        fn word(&self, _: SlotId) -> u64 { unreachable!("no calls") }
        fn address_word(&self, _: SlotId) -> u64 { unreachable!("no function address loads") }
        fn thunk(&self, _: SlotId) -> u64 { unreachable!("no function addresses") }
        fn outside(&mut self, _: RecordId, _: CodeSymbol, _: &str, _: bool) -> Result<Outside, RunModeError> { unreachable!("no C names") }
        fn reserve(&mut self, len: usize, align: usize) -> Result<u64, RunModeError> {
            let at = self.next.next_multiple_of(align as u64);
            self.next = at + len as u64;
            Ok(at)
        }
        fn reserve_hot(&mut self, len: usize, align: usize) -> Result<u64, RunModeError> { self.reserve(len, align) }
        fn carry(&mut self, step: Step, placing: &Placing<'_>) -> Result<(), RunModeError> {
            if step == Step::Store {
                let word = placing.relro.try_into().expect("one source line in relro");
                self.stored.push((placing.slot, u32::from_le_bytes(word)));
            }
            Ok(())
        }
        fn reserve_data(&mut self, _: usize, _: usize) -> Result<u64, RunModeError> { unreachable!("no writable data") }
        fn static_word(&self, _: u32) -> u64 { unreachable!("no static") }
        fn publish_static(&mut self, _: u32, _: u64, _: &[u8], _: Room) -> Result<(), RunModeError> { unreachable!("no static") }
        fn thread_key(&mut self, _: u64) -> Result<ThreadKey, RunModeError> { unreachable!("no thread local") }
        fn write(&mut self, _: u64, _: &[u8]) -> Result<(), RunModeError> { unreachable!("no thread local") }
    }

    #[test]
    fn coalesced_positioned_publications_follow_their_placements() {
        let alloc = Alloc::global();
        let mut code = CodeRecord::new_in(alloc);
        code.code.extend_from_slice(&0xd65f03c0u32.to_le_bytes());
        code.relro.extend_from_slice(&[0; 4]);
        code.positions.push(PositionRow { part: PositionPart::Relro, at: 0, item: 91, site: 17, field: PositionField::Line });
        let mut encoded = Vec::new_in(alloc);
        code.seal().unwrap().encode(&mut encoded);
        let record = RecordId(blaze_blake3::hash(&encoded));
        let source = Source { record, bytes: &encoded };

        let mut mode = RunMode::new(alloc);
        let mut acts = Vec::new_in(alloc);
        let mut memory = MemorySeen::default();
        let pairs = [
            Declared { symbol: WireSymbol([1; 32]), slot: Slot(7) },
            Declared { symbol: WireSymbol([2; 32]), slot: Slot(5) },
        ];
        let mut down = std::vec::Vec::new();
        Down::Declare { pairs: List::of(&pairs) }.encode(1, &mut down).unwrap();
        mode.on(Event::Frame(&down), &mut acts).unwrap();
        for &act in &acts[..] {
            let Act::Stub { slot } = act else { panic!("unexpected declaration action {act:?}") };
            memory.stub(slot).unwrap();
        }
        acts.clear();

        let (a, b) = (SlotId(0), SlotId(1));
        for (slot, thread) in [(a, Thread(1)), (b, Thread(2)), (a, Thread(3))] {
            mode.on(Event::Fired { slot, thread }, &mut acts).unwrap();
        }
        let mut up = mode.outbox();
        for (slot, who) in [(Slot(7), Thread(1)), (Slot(5), Thread(2))] {
            let (frame, took) = split(up).unwrap().unwrap();
            assert!(matches!(Up::decode(frame).unwrap().1, Up::Need { need: Needed::Slot(sent), thread } if sent == slot && thread == who));
            up = &up[took..];
        }
        assert!(up.is_empty(), "a repeat firing shares the outstanding need");
        mode.sent();

        down.clear();
        // A prepublication and the reply to the need it crossed both arrive for slot 7.
        // Slot 5's publication is in the same read, before either placement is carried.
        for (id, slot, line) in [(2, Slot(7), 13u32), (3, Slot(7), 13), (4, Slot(5), 29)] {
            Down::Positioned { slot, tier: Tier::Zero, record, values: &line.to_le_bytes() }.encode(id, &mut down).unwrap();
        }
        mode.on(Event::Frame(&down), &mut acts).unwrap();
        let first = Act::Place { slot: a, record };
        let second = Act::Place { slot: b, record };
        assert_eq!(acts.as_slice(), &[first, second]);

        // Plant the one-off pairing, the second action against the first answer. Even with an
        // identical record id it is refused before either word is published or input consumed.
        assert_eq!(mode.place(b, record, &source, &mut memory, Plant::None, &mut acts),
            Err(RunModeError::PositionOrder { placement: second, queued: Some(first) }));
        assert!(memory.stored.is_empty());
        for index in 0..2 {
            let Act::Place { slot, record } = acts[index] else { unreachable!() };
            mode.place(slot, record, &source, &mut memory, Plant::None, &mut acts).unwrap();
        }
        assert_eq!(memory.stored, [(a, 13), (b, 29)]);
        assert_eq!(&acts[2..], &[
            Act::Wake { thread: Thread(1), slot: a },
            Act::Wake { thread: Thread(3), slot: a },
            Act::Wake { thread: Thread(2), slot: b },
        ]);
        assert!(mode.position_inputs.is_empty());

        acts.clear();
        down.clear();
        Down::Positioned { slot: Slot(7), tier: Tier::Zero, record, values: &13u32.to_le_bytes() }.encode(5, &mut down).unwrap();
        mode.on(Event::Frame(&down), &mut acts).unwrap();
        assert!(acts.is_empty(), "a bound slot needs no second placement");
        assert!(mode.position_inputs.is_empty(), "the repeated answer leaves no input");
        println!("run_mode: slots received lines 13 and 29 once; PositionOrder caught the one-off pairing");
    }
}

/// Deterministic simulation proof of the two-`Handoff` eager-open protocol (`EagerOpenGate` +
/// `ready` + `NullInit` control stop) between the initial thread and the control thread.
#[cfg(test)]
mod eager_open_dst {
    use blaze_sim::{Broke, Config, Tape, check, control};

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Plant {
        None,
        /// Planted bug: omit `EagerOpenGate::drop`'s `open_ask.put(None)` on early control return.
        SkipDropWake,
        /// Planted bug: omit `shared.stop()` when `run_inits_with` returns `NullInit`.
        SkipInitErrStop,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum InitStep {
        WaitOpenAsk,
        DoOpen,
        WaitReady,
        RunInits,
        JoinControl,
        Done,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum CtrlStep {
        Setup,
        WaitOpenAns,
        LoadNative,
        EventLoop,
        Done,
    }

    fn run_case(tape: &Tape, plant: Plant) -> Result<(), Broke> {
        let early_fail = tape.below(3, "early_fail") == 0;
        let open_fails = tape.below(3, "open_fails") == 0;
        let load_fails = tape.below(3, "load_fails") == 0;
        let init_fails = tape.below(3, "init_fails") == 0;

        let mut open_ask: Option<Option<u32>> = None;
        let mut open_ans: Option<bool> = None;
        let mut ready: Option<bool> = None;
        let mut stop_sent = false;
        let mut gate_armed = true;

        let mut init_step = InitStep::WaitOpenAsk;
        let mut ctrl_step = CtrlStep::Setup;

        let mut opened_on_init = 0u32;
        let mut loaded_after_open = false;
        let mut inits_after_load = false;

        for _ in 0..64 {
            if init_step == InitStep::Done && ctrl_step == CtrlStep::Done {
                if !early_fail && opened_on_init != 1 {
                    return Err(Broke::Oracle("dlopen must run once on initial thread when control reaches Act::Native".into()));
                }
                if !early_fail && !open_fails && !loaded_after_open {
                    return Err(Broke::Oracle("load_native must follow initial-thread dlopen".into()));
                }
                if !early_fail && !open_fails && !load_fails && !inits_after_load {
                    return Err(Broke::Oracle("run_inits must follow load_native".into()));
                }
                return Ok(());
            }

            let init_ready = match init_step {
                InitStep::WaitOpenAsk => open_ask.is_some(),
                InitStep::DoOpen | InitStep::RunInits => true,
                InitStep::WaitReady => ready.is_some(),
                InitStep::JoinControl => ctrl_step == CtrlStep::Done,
                InitStep::Done => false,
            };
            let ctrl_ready = match ctrl_step {
                CtrlStep::Setup | CtrlStep::LoadNative => true,
                CtrlStep::WaitOpenAns => open_ans.is_some(),
                CtrlStep::EventLoop => stop_sent,
                CtrlStep::Done => false,
            };

            if !init_ready && !ctrl_ready {
                return Err(Broke::Oracle("deadlock between initial thread and control thread".into()));
            }

            let pick_init = match (init_ready, ctrl_ready) {
                (true, false) => true,
                (false, true) => false,
                (true, true) => tape.below(2, "turn") == 0,
                (false, false) => unreachable!(),
            };

            if pick_init {
                match init_step {
                    InitStep::WaitOpenAsk => {
                        let asked = open_ask.take();
                        init_step = match asked {
                            Some(Some(_)) => InitStep::DoOpen,
                            Some(None) => InitStep::WaitReady,
                            None => return Err(Broke::Oracle("missing open_ask".into())),
                        };
                    }
                    InitStep::DoOpen => {
                        opened_on_init += 1;
                        open_ans = Some(!open_fails);
                        init_step = InitStep::WaitReady;
                    }
                    InitStep::WaitReady => match ready.take() {
                        Some(true) => init_step = InitStep::RunInits,
                        Some(false) => init_step = InitStep::JoinControl,
                        None => return Err(Broke::Oracle("missing ready".into())),
                    },
                    InitStep::RunInits => {
                        inits_after_load = loaded_after_open;
                        if init_fails {
                            if plant != Plant::SkipInitErrStop {
                                stop_sent = true;
                            }
                            init_step = InitStep::JoinControl;
                        } else {
                            stop_sent = true;
                            init_step = InitStep::JoinControl;
                        }
                    }
                    InitStep::JoinControl => {
                        init_step = InitStep::Done;
                    }
                    InitStep::Done => {}
                }
            } else {
                match ctrl_step {
                    CtrlStep::Setup => {
                        if early_fail {
                            // Control exits early before Act::Native: EagerOpenGate::drop wakes initial thread.
                            if gate_armed && plant != Plant::SkipDropWake {
                                gate_armed = false;
                                open_ask = Some(None);
                            }
                            ready = Some(false);
                            ctrl_step = CtrlStep::Done;
                        } else {
                            gate_armed = false;
                            open_ask = Some(Some(1));
                            ctrl_step = CtrlStep::WaitOpenAns;
                        }
                    }
                    CtrlStep::WaitOpenAns => match open_ans.take() {
                        Some(true) => ctrl_step = CtrlStep::LoadNative,
                        Some(false) => {
                            ready = Some(false);
                            ctrl_step = CtrlStep::Done;
                        }
                        None => return Err(Broke::Oracle("missing open_ans".into())),
                    },
                    CtrlStep::LoadNative => {
                        loaded_after_open = opened_on_init == 1;
                        if load_fails {
                            ready = Some(false);
                            ctrl_step = CtrlStep::Done;
                        } else {
                            ready = Some(true);
                            ctrl_step = CtrlStep::EventLoop;
                        }
                    }
                    CtrlStep::EventLoop => {
                        ctrl_step = CtrlStep::Done;
                    }
                    CtrlStep::Done => {}
                }
            }
        }
        Err(Broke::Oracle("protocol did not finish within step bound".into()))
    }

    #[test]
    fn eager_open_handoff_is_deadlock_free_and_ordered() {
        check(Config::cases(4_000), |tape| run_case(tape, Plant::None));
    }

    #[test]
    fn control_missing_drop_wake_and_missing_init_err_stop_are_caught() {
        control(Config::cases(1_000), Plant::None, Plant::SkipDropWake, "deadlock between initial thread and control thread", run_case);
        control(Config::cases(1_000), Plant::None, Plant::SkipInitErrStop, "deadlock between initial thread and control thread", run_case);
    }
}

#[cfg(test)]
mod weak_outside_tests {
    use core::ffi::CStr;

    use blaze_code::place::Words;
    use blaze_code::record::{CodeRecord, CodeView, Space, Symbol as CodeSymbol, Target};
    use blaze_launch::wire::RecordId;
    use blaze_own::{Alloc, SharedAlloc, Vec};

    use super::bind::{Bind, BindError};
    use super::{Binder, Memory, Names, NativeMemory, RunMode, RunModeError};

    const PRESENT_ADDR: u64 = 0x0000_0000_1234_5678;

    struct FakeBind;

    impl Bind for FakeBind {
        fn address(&mut self, name: &str, _cname: &CStr) -> Result<u64, BindError> {
            match name {
                "present_sym" => Ok(PRESENT_ADDR),
                _ => Err(BindError::NotFound),
            }
        }
    }

    /// Rule 23 test for `NativeMemory::outside` + `Binder` on `Space::WeakOutside` vs `Space::Outside`:
    /// - weak missing symbol binds its import word and value to 0 (null)
    /// - weak present symbol binds to its real address
    /// - strong missing symbol is refused with `RunModeError::Outside`
    /// - strong reference to a symbol previously cached as 0 via weak MUST still be refused!
    #[test]
    fn native_memory_and_binder_handle_weak_and_strong_outside() {
        let alloc = Alloc::global();
        let Ok(reg) = crate::unwind_reg::Registry::new_in(16, SharedAlloc::global()) else {
            panic!("unwind registry allocates");
        };
        let reg: &'static crate::unwind_reg::Registry = std::boxed::Box::leak(std::boxed::Box::new(reg));
        let Ok(unwind) = reg.writer() else {
            panic!("unwind registry writer");
        };
        let Ok(mut memory) = NativeMemory::new(FakeBind, alloc, unwind) else {
            panic!("NativeMemory::new must allocate code reservation");
        };
        let record = RecordId([0xAB; 32]);
        let missing_sym = CodeSymbol::of_name("missing_weak_sym");
        let present_sym = CodeSymbol::of_name("present_sym");
        let strong_missing_sym = CodeSymbol::of_name("missing_strong_sym");

        // 1. Weak missing symbol -> binds to 0 (not an error!).
        let Ok(weak_missing) = memory.outside(record, missing_sym, "missing_weak_sym", true) else {
            panic!("weak missing symbol must bind to 0");
        };
        assert_eq!(weak_missing.value, 0);
        // SAFETY: `weak_missing.word` is the live outside import word slot inside `memory`.
        let word_val = unsafe { (weak_missing.word as *const u64).read() };
        assert_eq!(word_val, 0);

        // 2. Strong reference to the SAME symbol that was previously cached as 0 via weak -> MUST FAIL!
        let strong_after_weak = memory.outside(record, missing_sym, "missing_weak_sym", false);
        assert!(
            matches!(strong_after_weak, Err(RunModeError::Outside { symbol, .. }) if symbol == missing_sym),
            "strong reference after weak-missing cache must still be refused: {strong_after_weak:?}"
        );

        // 3. Weak present symbol -> binds to PRESENT_ADDR.
        let Ok(weak_present) = memory.outside(record, present_sym, "present_sym", true) else {
            panic!("weak present symbol must bind to its real address");
        };
        assert_eq!(weak_present.value, PRESENT_ADDR);
        // SAFETY: `weak_present.word` is the live outside import word slot inside `memory`.
        let present_word_val = unsafe { (weak_present.word as *const u64).read() };
        assert_eq!(present_word_val, PRESENT_ADDR);

        // 4. Strong missing symbol -> refused with `RunModeError::Outside`.
        let strong_missing = memory.outside(record, strong_missing_sym, "missing_strong_sym", false);
        assert!(
            matches!(strong_missing, Err(RunModeError::Outside { symbol, .. }) if symbol == strong_missing_sym),
            "strong missing symbol must be refused: {strong_missing:?}"
        );

        // 5. Through `Binder` (Words::word and Words::value) over a sealed `CodeView`.
        let mut rec = CodeRecord::new_in(alloc);
        rec.code.extend_from_slice(&0xD65F_03C0u32.to_le_bytes());
        rec.weak_outside("missing_weak_sym");
        rec.weak_outside("present_sym");
        rec.outside("missing_strong_sym");
        let mut bytes = Vec::new_in(alloc);
        let Ok(sealed) = rec.seal() else { panic!("record seals") };
        sealed.encode(&mut bytes);
        let Ok(view) = CodeView::read(&bytes) else { panic!("view reads") };
        let mut mode = RunMode::new(alloc);
        let mut binder = Binder {
            record,
            names: Names::Code(view),
            slots: &mut mode.slots,
            statics: &mode.statics,
            thread_locals: &mut mode.thread_locals,
            memory: &mut memory,
            refused: None,
        };
        assert_eq!(binder.value(Target { space: Space::WeakOutside, symbol: missing_sym }), Ok(0));
        assert_eq!(binder.value(Target { space: Space::WeakOutside, symbol: present_sym }), Ok(PRESENT_ADDR));
        assert!(binder.word(Target { space: Space::WeakOutside, symbol: missing_sym }, false).is_ok());
        assert!(binder.value(Target { space: Space::Outside, symbol: strong_missing_sym }).is_err());
        assert!(matches!(binder.refused, Some(RunModeError::Outside { symbol, .. }) if symbol == strong_missing_sym));
    }
}
