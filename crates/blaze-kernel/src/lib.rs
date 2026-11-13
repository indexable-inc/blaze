//! The kernel: blaze v2's compute steps from MIR down (docs/v2/plan/design/kernel.md).
//!
//! A step is a registered, pure function of its input ([`Step`], [`Compute`]). It sees the
//! world only through [`Reads`], which records each read it makes: its name (a [`Read`], a kind
//! and the key's canonical bytes) and the hash the read saw ([`Seen`]). The value is kept in
//! blaze-cas as an [`Entry`] under the step's [`Identity`] (what it is of, the step's
//! [`CodeId`], its flags) and those reads. A later lookup takes it only when every recorded
//! read hashes the same now. That is a verifying trace (Mokhov, Mitchell and Peyton Jones,
//! "Build systems à la carte", 2018).
//!
//! - **Beside the front end, under codegen.** The front end keeps its own engine
//!   (`blaze_query::db`, derived from salsa): dense ids, in memory, one view at a time. A kernel
//!   entry is keyed by content, persisted, and shared by every process and checkout, so its
//!   reads are narrow facts named by stable keys (an item key's layout, not the item). A
//!   [`World`] that the wiring crate implements over the front end answers them. The kernel
//!   does not depend on `blaze-query`.
//! - **Identity first.** An entry with no reads is kept under [`whole::key`] alone, as its bare
//!   value: one probe, no header. A step that reads keeps the names of its reads (a read-list)
//!   in a bounded chain of slots under its identity ([`MOST_SHAPES`]). Lookup hashes each
//!   list's reads now and probes the entry those hashes name.
//! - **Stored as a record.** The chain, the read-list's bytes and the entry's key are
//!   `blaze_record`'s (base key → read-lists → answer): the base is the identity's ID, and the
//!   kernel adds what `entry` says (a read's name, the entry's bytes). The kernel is the API:
//!   registered steps, [`Identity`], [`Reads`], [`World`], and the refusals below.
//! - **Refusals, not overwrites.** Two values under one identity and set of reads is a
//!   [`KernelError::Conflict`] (rule 21). A run whose read failed, or which saw two hashes for
//!   one read, keeps nothing.
//! - **Deterministic** (rule 25): an entry keeps its reads sorted by kind and key, each once.
//! - **Checked** (lane verify, `blaze_store_check`): a hit that `BLAZE_VERIFY_STORE` samples is
//!   computed again with fresh reads, and it must make the same reads and the same bytes.

#![feature(allocator_api, register_tool)]
#![register_tool(blaze)]

use std::alloc::Global;
use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::fmt;

use blaze_cas::{Key, Kind, content_of};
use blaze_ids::{ContentId, Hasher};
use blaze_record::{AnswerKey, Doing, Filing, Placed, Slot, Walked, keep_answer, list_key, walk};
use blaze_report::{
    say,
    say::{Code, Count, Words},
};
use blaze_store_check::{Setting, VerifyError};

mod entry;
#[cfg(test)]
mod exhaustive;
mod nest;
mod reads;
mod session;
mod store;
pub mod table;
mod trie;
#[cfg(test)]
mod tests;
pub mod why;

pub use crate::entry::{Entry, EntryReads, whole};
pub use crate::nest::MOST_NESTED;
use crate::nest::{Door, Engine, How, Lent, LentMut, Need, Now, Runner, Valued};
pub use crate::session::{Carried, CarryError, Changed, Dropped, Session};
pub use crate::reads::{Reads, Trace, TwoSeen};
pub use crate::store::{Get, Put, Reading, Writing, take_lookups};
use crate::store::{Keeping, Looking, Stopped};
pub use crate::why::{Miss, Missed, Part, PartKind};

/// The most shapes (read-lists: the names of one run's reads) one identity's chain holds. A
/// step whose reads differ by shape more often than this reads too much of what varies, so its
/// keys should be split. Reaching the bound is [`KernelError::TooManyShapes`], never an endless
/// walk (rule 18).
pub const MOST_SHAPES: u32 = 1024;

/// Every step's number, in one table so that no two crates take the same one (registration
/// refuses a second step of one number or one name too).
pub mod steps {
    use crate::StepId;

    /// blaze-codegen's dev template of a function instance (`blaze_codegen::kernel::DevTemplate`).
    pub const DEV_TEMPLATE: StepId = StepId(1);

    /// blaze-front-items' `parse(file, edition)` (`blaze_front_items::steps::Parse`).
    pub const PARSE: StepId = StepId(0x0201);
    /// blaze-front-items' `items(file)`, a table (`blaze_front_items::steps::Items`).
    pub const ITEMS: StepId = StepId(0x0202);
    /// blaze-front-items' `lines(file)`, a table (`blaze_front_items::steps::Lines`).
    pub const LINES: StepId = StepId(0x0203);
    /// blaze-front-items' `modules(crate)`, a table (`blaze_front_items::steps::Modules`).
    pub const MODULES: StepId = StepId(0x0204);

    /// hir-ty's body entries (`hir_ty::body_store::entries`): a body's inference.
    pub const BODY_INFER: StepId = StepId(0x0301);
    /// A body's built MIR (`hir_ty::body_store::entries::MirEntry`).
    pub const BODY_MIR: StepId = StepId(0x0302);
    /// That a body's borrow check passed (`hir_ty::body_store::entries::BorrowckEntry`).
    pub const BODY_BORROWCK: StepId = StepId(0x0303);
    /// A constant's evaluated value (`hir_ty::body_store::entries::ConstEntry`).
    pub const BODY_CONST: StepId = StepId(0x0304);
}

/// A compute step, by number: one number is one step for every blaze ([`steps`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct StepId(pub u16);

/// The code identity of a step: its `compute` and everything that calls, as lane identity
/// hashes it. Until lane identity gives one, a wiring passes what it has (codegen passes
/// `Toolchain::id`: the pinned nightly and the blaze executable's hash).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CodeId(pub ContentId);

/// The parts of an [`Identity`] that a step's input gives.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Subject {
    /// The stable identity of what is computed (lane identity's identity of a function,
    /// instance or constant). Never an interned id or a path in the checkout.
    pub of: ContentId,
    /// Everything else, outside the reads, that changes the value: target, settings.
    pub flags: ContentId,
}

/// What an entry is of: what lookup starts from, before any read.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Identity {
    pub step: StepId,
    pub of: ContentId,
    pub code: CodeId,
    pub flags: ContentId,
}

const IDENTITY: &str = "blaze v2 kernel identity 1";

impl Identity {
    /// Its content ID: the base key its read-lists and entries are filed under.
    pub fn id(&self) -> ContentId {
        let mut hasher = Hasher::new(IDENTITY);
        hasher.update(&self.step.0.to_le_bytes()).id(&self.of).id(&self.code.0).id(&self.flags);
        hasher.finish()
    }
}

/// The identity of `S`'s value of `input` when its code is `code`. [`Kernel::identity`] gives
/// it with the code `S` was registered with.
pub fn identity<S: Step>(code: CodeId, input: &S::Input) -> Identity {
    let Subject { of, flags } = S::subject(input);
    Identity { step: S::ID, of, code, flags }
}

/// A kind of read, by number. Its [`World`] names it ([`World::describe`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ReadKind(pub u16);

impl ReadKind {
    /// A step's value: the one kind the kernel answers itself, never a [`World`]. Its key is
    /// the step's number (`u16`, big-endian) and the input's stable bytes ([`Ask::key`]); what
    /// it saw is the hash of the value's bytes. [`Reads::step`] records it.
    pub const STEP: ReadKind = ReadKind(0xffff);

    /// One part of a step's value that is a table ([`table`]): the kernel answers it. Its
    /// key is the length of the step's key (two bytes, big-endian), the step's key as a
    /// [`ReadKind::STEP`] read has it, then the part's key; what it saw is the digest of the
    /// part's own bytes, or [`Seen::Gone`] where the table has no such key.
    /// [`Reads::part`] records it.
    pub const PART: ReadKind = ReadKind(0xfffe);

    /// The key set of a step's value that is a table: its key is the step's key, and what it
    /// saw is the digest of the keys alone. [`Reads::keys`] records it.
    pub const KEYS: ReadKind = ReadKind(0xfffd);

    /// Never a read: the name a session posts every reader of any part, or of the keys, of
    /// a step under, so that a table that cannot be valued at a carry reaches them all.
    pub const TABLE: ReadKind = ReadKind(0xfffc);

    /// Whether the kernel answers reads of this kind itself: no [`World`] is asked for one,
    /// and no change set names one.
    pub fn is_kernels(self) -> bool {
        self.0 >= ReadKind::TABLE.0
    }
}

/// One read: its kind and the canonical bytes of its key (content IDs and item keys; no dense
/// id, no absolute path). It is narrow: the layout of `Foo`, not `Foo`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Read<'a> {
    pub kind: ReadKind,
    pub key: &'a [u8],
}

/// What a read names now.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Seen {
    Is(ContentId),
    /// The key names nothing now (the item was deleted or renamed).
    Gone,
}

impl Seen {
    /// The hash a check names it by: all zeros when gone.
    pub fn hash(self) -> ContentId {
        match self {
            Seen::Is(hash) => hash,
            Seen::Gone => ContentId::default(),
        }
    }
}

/// The world could not answer a read. This is never a [`Seen::Gone`]: a read that fails keeps
/// nothing.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WorldError {
    pub kind: ReadKind,
    pub key: Vec<u8>,
    pub why: String,
    /// The read only cannot be valued now ([`WorldError::not_now`]): to a lookup that is an
    /// outcome, not a failure.
    pub not_now: bool,
}

impl WorldError {
    /// The read failed: a lookup that asks for it fails, and so does a run.
    pub fn new(read: Read<'_>, why: impl Into<String>) -> WorldError {
        // alloc: an error
        WorldError { kind: read.kind, key: read.key.to_vec(), why: why.into(), not_now: false }
    }

    /// The read cannot be valued now: what it names is not there to hash in the world as it
    /// is (a file of a crate not loaded, a name in a scope that no longer exists). A lookup
    /// passes over the read-list that names it, since an entry whose run made that read cannot
    /// be the one valid now, and goes on to the identity's other lists; nothing fails. A run
    /// that asks for it fails as with any other error: a run asks only for what it needs.
    pub fn not_now(read: Read<'_>, why: impl Into<String>) -> WorldError {
        // alloc: an error
        WorldError { kind: read.kind, key: read.key.to_vec(), why: why.into(), not_now: true }
    }

    pub fn read(&self) -> Read<'_> {
        Read { kind: self.kind, key: &self.key }
    }
}

impl fmt::Display for WorldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "read {} of {} failed: {}", self.kind.0, hex(&self.key), self.why)
    }
}

/// What a [`World`] is pinned at ([`World::pin`]): equal pins, equal hashes of every read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pin(pub ContentId);

/// The outside world, as hashes. The crate that wires a step implements it, over the front
/// end's facts, the file system, and the rest.
///
/// `hash` must give the [`Seen`] that [`Fact::get`] gives for the same read. Lookup trusts it,
/// and a sampled check (lane verify) finds out when the two disagree.
///
/// What `hash` comes to, for a read: a hash ([`Seen::Is`]); nothing under that key
/// ([`Seen::Gone`], which equals no hash, so an entry that saw nothing there holds only while
/// nothing is there); that it cannot be valued now ([`WorldError::not_now`]: a lookup passes
/// over the list, a run fails); or a failure ([`WorldError::new`]). Absence over a set (no impl
/// for a type, no such name in a scope) is none of these: it is a read of the set, under a
/// kind of its own, whose hash covers the set's members.
pub trait World {
    fn hash(&self, read: Read<'_>) -> Result<Seen, WorldError>;

    /// What the world is pinned at now: while two calls give the same pin, every read hashes
    /// the same at both. A world that can change says so here (a revision's digest, a
    /// snapshot's id); it is asked at the start and the end of every request, so it is a value
    /// the world holds, not one it works out. What a [`Session`] verified holds for one pin.
    fn pin(&self) -> Pin;

    /// The covering read `read` is under, none where nothing covers it (a fact of a crate
    /// under the crate's digest, a file under its directory's Merkle id; a covering read may
    /// be under a wider one). The claim, which is the world's to keep: while the covering
    /// read hashes the same, every value computed from `read` is the same bytes. A digest of
    /// an interface may ignore part of what `read` holds; it may not ignore anything a value
    /// can show. [`Kernel::cover`] refuses a member with a read under none of the cover's
    /// reads, so a world that says nothing here has no covers and no wrong ones.
    fn cover<'r>(&'r self, _read: Read<'r>) -> Option<Read<'r>> {
        None
    }

    /// The read as an error or a check names it, e.g. "layout of `core::option::Option<u8>`".
    /// Rendered only for an error or a sampled check, never on a hit's path.
    fn describe(&self, read: Read<'_>) -> String {
        plain(read)
    }

    /// The item the read is of, by path (the type whose layout it is, the callee whose
    /// identity it is), for a reason's chain to follow ([`why::Missed::Read`]). Asked only on a
    /// miss.
    fn names(&self, _read: Read<'_>) -> Option<String> {
        None
    }
}

/// A typed read: what a step asks [`Reads::get`] for, and how its key is written.
pub trait Fact<W: ?Sized> {
    const KIND: ReadKind;
    type Key: ?Sized;
    type Value<'w>
    where
        W: 'w;

    /// Appends the key's canonical bytes.
    fn key(key: &Self::Key, out: &mut Vec<u8>);

    /// The value and the hash of what it is. A key that names nothing now is `Seen::Gone`,
    /// with a value saying so.
    fn get<'w>(world: &'w W, key: &Self::Key) -> Result<(Self::Value<'w>, Seen), WorldError>;
}

/// A registered step: what its identity is made of. Every method of every impl must be pure
/// (lane pure's `#[blaze::pure]` on a trait checks each one).
#[blaze::pure]
pub trait Step: 'static {
    const ID: StepId;
    /// Its name, as errors give it (`codegen dev template`).
    const NAME: &'static str;
    /// The store kind of its entries.
    const KIND: Kind;
    type Input: ?Sized;

    /// The input's stable identity and flags. The kernel adds the step and its code.
    fn subject(input: &Self::Input) -> Subject;

    /// What an error or a check calls the input (a function's path). Never on a hit's path.
    fn label(input: &Self::Input) -> String;
}

/// A step the kernel computes. A step whose value is computed elsewhere is a [`Step`] only:
/// with no reads (codegen's batched templates) it is kept through [`Kernel::find_whole`],
/// [`Kernel::keep_whole`] or [`whole::key`]; with reads made elsewhere, through
/// [`Kernel::keep`] and found by [`Kernel::find`].
#[blaze::pure]
pub trait Compute: Step {
    type World: World + ?Sized;

    /// The value of `input`, from what `ctx` reads and nothing else.
    fn compute(ctx: &mut Reads<'_, Self::World>, input: &Self::Input)
    -> Result<Vec<u8>, StepError>;
}

/// A step another step may ask for ([`Reads::step`]): its input has stable bytes, so a read of
/// its value can be keyed by them and the kernel can run it from that key at a later lookup
/// ([`Kernel::register_ask`]).
///
/// `input(key(i))` must be `i`: the kernel computes the step from the input its key decodes
/// to, and from no other.
#[blaze::pure]
pub trait Ask: Compute<Input: Sized> {
    /// Whether its value is a table ([`table::Builder`]): a step may then read one part of it
    /// ([`Reads::part`]) or its key set ([`Reads::keys`]), and a carry compares the old value
    /// with the new one part by part. A step that says so and makes other bytes is refused
    /// where a part of it is read.
    const TABLE: bool = false;

    /// Appends the input's stable bytes: content IDs and item keys, never a dense id or a
    /// path in the checkout.
    fn key(input: &Self::Input, out: &mut Vec<u8>);

    /// The input `key` wrote; bytes that are none are refused, naming why.
    fn input(key: &[u8]) -> Result<Self::Input, String>;
}

/// One read of the run before whose value is other now: what a maintained step is given.
#[derive(Clone, Copy)]
pub struct Change<'a> {
    pub read: Read<'a>,
    /// What the run before saw.
    pub was: Seen,
    /// What it is now.
    pub now: Seen,
}

/// A step that may run from the value it made before and the reads that changed
/// ([`Kernel::register_maintain`]): what makes an edit of one fact cost that fact, where
/// `compute` reads them all (name resolution of a crate, the impls of a trait). `compute`
/// stays the definition; `maintain` is a faster way to the same bytes.
///
/// The kernel calls `maintain` in a carry's repair, where the session holds the entry the
/// step had before and every read of that entry that changed can be valued now; it calls
/// `compute` otherwise (a first run, another checkout, a session that was dropped). The entry
/// kept has the same identity, reads and value form either way.
///
/// `ctx` starts with the reads of the run before, each with what it is now, none valued
/// again for the run: a read the maintainer does not touch is in the trace as `compute` would
/// record it. The maintainer reads again what it needs the value of ([`Reads::get`]), makes
/// new reads as any run does, and forgets a read `compute` would no longer make
/// ([`Reads::forget`]).
#[blaze::pure]
pub trait Maintain: Ask {
    /// The value of `input` now, from `previous` (the bytes the run before made) and
    /// `changed` (the reads of that run that are other now, in the order of their names).
    ///
    /// The invariant: `maintain(previous, changed)` is `compute()` byte for byte, reads
    /// included. The kernel runs both where the request's setting samples the identity and
    /// refuses a difference ([`KernelError::Maintained`]), keeping neither (controls
    /// `a_maintainer_that_forgets_a_deletion_is_refused_under_verify` and
    /// `a_maintainer_that_forgets_a_deletion_is_stale_with_verify_off`, `tests/cost.rs`).
    fn maintain(
        ctx: &mut Reads<'_, Self::World>,
        input: &Self::Input,
        previous: &[u8],
        changed: &[Change<'_>],
    ) -> Result<Vec<u8>, StepError>;
}

/// Why a step's run made no value.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StepError {
    /// A read failed (`?` on [`Reads::get`]).
    World(WorldError),
    /// The step refused its input, naming why.
    Refused(String),
    /// A step it asked for was refused (`?` on [`Reads::step`]).
    Asked(Box<KernelError>),
}

/// One value on a cycle: its step and its identity's ID.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Link {
    pub step: &'static str,
    pub identity: ContentId,
}

impl From<WorldError> for StepError {
    fn from(error: WorldError) -> StepError {
        StepError::World(error)
    }
}

/// What [`Kernel::get`] gave.
#[derive(Debug)]
pub enum Got<'s> {
    /// The store's entry, read in place.
    Hit(Entry<'s>),
    /// Computed now and kept under `key`, with the reads its entry records (sorted by kind
    /// and key, each once): what the invalidation oracle's trace takes of a computed entry
    /// (`blaze_v2_proof::kernel`), as it takes [`Entry::reads`] of a hit. Why the lookup found
    /// nothing is [`Kernel::why`]'s, asked only by whoever wants it.
    Computed { key: Key, value: Vec<u8>, reads: Trace },
}

impl Got<'_> {
    pub fn value(&self) -> &[u8] {
        match self {
            Got::Hit(entry) => entry.value,
            Got::Computed { value, .. } => value,
        }
    }

    pub fn is_hit(&self) -> bool {
        matches!(self, Got::Hit(_))
    }
}

/// Which value an error is about: the step, the input as its step names it, and the identity.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Of {
    pub step: &'static str,
    pub label: String,
    pub identity: ContentId,
}

impl fmt::Display for Of {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let identity = format!("{:x}", self.identity); // alloc: an error's text
        // alloc: an error's text
        f.write_str(&say!(Words(self.step), " of ", Code(&self.label), " (identity ", Words(&identity), ")").flat())
    }
}

/// Why the kernel refused. Each variant names the step, the input and the identity; text
/// that needs the world is rendered while the world is at hand.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum KernelError {
    /// A step used before it was registered, or a type other than the one registered under
    /// its number.
    Unregistered { step: StepId, name: &'static str, holder: Option<&'static str> },
    /// Two steps of one number or one name.
    Twice { first: (StepId, &'static str), second: (StepId, &'static str) },
    /// The store could not read or keep a value.
    Store { of: Of, key: Key, why: String },
    /// A read failed: nothing was kept.
    World { of: Of, read: String, why: String },
    /// One read gave two hashes in one run: the world changed under the step, so nothing was kept.
    Unsteady { of: Of, read: String, first: Seen, then: Seen },
    /// The store has other bytes for this identity and set of reads (rule 21). Either the
    /// identity misses an input the value depends on, or the step is not deterministic.
    Conflict { of: Of, key: Key, reads: Vec<String>, kept: ContentId, given: ContentId },
    /// The identity's chain of shapes is full ([`MOST_SHAPES`]).
    TooManyShapes { of: Of },
    /// The step refused its input.
    Compute { of: Of, why: String },
    /// A maintained step's `maintain` made other reads or other bytes than its `compute`
    /// ([`Maintain`]'s invariant): `why` names the first read or the first part that differs.
    /// Neither value was kept.
    Maintained { of: Of, why: String },
    /// A sampled hit computed again to other reads or other bytes.
    Verify(VerifyError),
    /// The world is not at the pin the request's session is of: it moved between two requests
    /// of one session, or while one ran. Nothing is answered from the session's rows, and
    /// whoever holds the session makes a new one for the world as it is now.
    Moved { pinned: Pin, now: Pin },
    /// The kernel's registry of steps ([`Kernel::registry`]) is not the one the request's
    /// session was verified under: a step's code, or which steps there are, is an input of
    /// every row. Nothing is answered; a carry drops what the session verified
    /// ([`Kernel::carry`]), or its holder makes a new one.
    Reregistered { session: ContentId, now: ContentId },
    /// A cover was not kept: `member` cannot be under it, for `read` and `why`.
    Uncovered { member: Of, read: String, why: String },
    /// A carry was refused before it changed anything ([`Kernel::carry`]).
    Carry(CarryError),
    /// A step asked for a value that was being computed for it: `cycle` is the values on the
    /// way, from the one asked for again (`of`) to the one that asked. The kernel has no
    /// fixpoint: steps that need each other are one step over their whole cycle.
    Cycle { of: Of, cycle: Vec<Link> },
    /// Steps nest deeper than [`MOST_NESTED`].
    TooDeep { of: Of },
    /// A step this one asked for was refused, so nothing was kept.
    Child { of: Of, child: Box<KernelError> },
    /// A read of a step's value names a step that is not registered to be asked for
    /// ([`Kernel::register_ask`]): `name` is the step registered under the number, if any.
    Unasked { step: StepId, name: Option<&'static str> },
    /// A read of a step's value whose key is no input of its step.
    Input { step: &'static str, key: String, why: String },
}

/// Why a session's entry could not be read: the store it reads has no value under the entry's
/// key ([`KernelError::missing_entry`]). A session names entries of one store, and a driver
/// that advances a session before the puts of its entries reach that store breaks that.
pub const NOT_IN_STORE: &str = "its entry is not in the store: a session is of one store";

impl KernelError {
    /// The entry a session named that the store it reads does not hold, when that is what this
    /// says, through any [`KernelError::Child`]: the error a session that ran ahead of its
    /// store's puts gives. Its holder drops the session, saying so, and makes a new one, whose
    /// every answer is looked up in the store again and verified by its reads.
    pub fn missing_entry(&self) -> Option<Key> {
        match self {
            KernelError::Store { key, why, .. } if why == NOT_IN_STORE => Some(*key),
            KernelError::Child { child, .. } => child.missing_entry(),
            _ => None,
        }
    }

    /// Whether it says only that a step's value cannot be had now. During a lookup that makes
    /// the read-list that named the step not hold (`nest`'s doc says why); during a run it is
    /// the run's error.
    pub(crate) fn unvaluable(&self) -> bool {
        match self {
            KernelError::Cycle { .. }
            | KernelError::TooDeep { .. }
            | KernelError::Compute { .. }
            | KernelError::World { .. }
            | KernelError::Unasked { .. }
            | KernelError::Input { .. } => true,
            KernelError::Child { child, .. } => child.unvaluable(),
            KernelError::Unregistered { .. }
            | KernelError::Twice { .. }
            | KernelError::Store { .. }
            | KernelError::Maintained { .. }
            | KernelError::Unsteady { .. }
            | KernelError::Conflict { .. }
            | KernelError::TooManyShapes { .. }
            | KernelError::Verify(_)
            | KernelError::Moved { .. }
            | KernelError::Reregistered { .. }
            | KernelError::Uncovered { .. }
            | KernelError::Carry(_) => false,
        }
    }
}

impl fmt::Display for KernelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // alloc: an error's text
            KernelError::Unregistered { step, name, holder: None } => f.write_str(
                &say!("the kernel has no step ", Code(name), " (step ", Count(u64::from(step.0)), "): register it first")
                    .flat(),
            ),
            // alloc: an error's text
            KernelError::Unregistered { step, name, holder: Some(holder) } => f.write_str(
                &say!(
                    "step ",
                    Count(u64::from(step.0)),
                    " is ",
                    Code(holder),
                    " in the kernel, not ",
                    Code(name),
                    ": one number is one step (blaze_kernel::steps)"
                )
                .flat(),
            ),
            // alloc: an error's text
            KernelError::Twice { first, second } => f.write_str(
                &say!(
                    "two kernel steps share a number or a name: ",
                    Code(first.1),
                    " (step ",
                    Count(u64::from(first.0.0)),
                    ") and ",
                    Code(second.1),
                    " (step ",
                    Count(u64::from(second.0.0)),
                    ")"
                )
                .flat(),
            ),
            KernelError::Store { of, key, why } => {
                write!(f, "{of}: the store at {:x}: {why}", key.0)
            }
            KernelError::World { of, read, why } => {
                write!(f, "{of}: the read {read} failed, so nothing was kept: {why}")
            }
            KernelError::Unsteady { of, read, first, then } => write!(
                f,
                "{of}: the read {read} saw {:x} and then {:x} in one run: the world changed under the step, so nothing was kept",
                first.hash(),
                then.hash()
            ),
            KernelError::Conflict { of, key, reads, kept, given } => write!(
                f,
                "{of}: the store holds other bytes ({kept:x}) than this run made ({given:x}) under {:x}, for the same identity and reads [{}]: the identity misses an input the value depends on, or the step is not deterministic",
                key.0,
                reads.join(", ")
            ),
            KernelError::TooManyShapes { of } => write!(
                f,
                "{of}: {MOST_SHAPES} shapes of reads are kept under this identity, the most one holds: the step reads too much of what varies"
            ),
            KernelError::Compute { of, why } => write!(f, "{of}: {why}"),
            KernelError::Maintained { of, why } => write!(
                f,
                "{of}: run from its value before and the reads that changed (`Maintain::maintain`) it is not what `compute` makes, so neither was kept: {why}"
            ),
            KernelError::Verify(error) => error.fmt(f),
            KernelError::Moved { pinned, now } => write!(
                f,
                "the world moved under a session: the session is of the world pinned at {:x}, and the world is at {:x} now, so nothing was answered; make a new session for the world as it is",
                pinned.0, now.0
            ),
            KernelError::Reregistered { session, now } => write!(
                f,
                "the kernel's steps are not the ones a session was verified under: the session is of the registry {session:x}, and the kernel's is {now:x} now, so nothing was answered; carry the session (which drops what it verified) or make a new one"
            ),
            KernelError::Uncovered { member, read, why } => write!(
                f,
                "no cover was kept: its member {member} reads {read}, {why}; a cover's hit admits every member with none of its reads valued, so each must be under a read the cover makes"
            ),
            KernelError::Carry(error) => error.fmt(f),
            KernelError::Cycle { of, cycle } => {
                write!(f, "{of} is asked for while it is computed: a cycle of steps, ")?;
                for link in cycle {
                    write!(f, "{} (identity {:x}) asks for ", link.step, link.identity)?;
                }
                write!(
                    f,
                    "the first again; the kernel has no fixpoint, so steps that need each other are one step over their whole cycle"
                )
            }
            KernelError::TooDeep { of } => write!(
                f,
                "{of}: {MOST_NESTED} steps are asking for one another's values on the way to it, the most that may nest"
            ),
            KernelError::Child { of, child } => {
                write!(f, "{of}: a step it asked for was refused, so nothing was kept: {child}")
            }
            KernelError::Unasked { step, name: Some(name) } => write!(
                f,
                "step `{name}` (step {}) is asked for by another step, and is not registered to be (Kernel::register_ask)",
                step.0
            ),
            KernelError::Unasked { step, name: None } => write!(
                f,
                "a read of a step's value names step {}, and the kernel has no such step",
                step.0
            ),
            KernelError::Input { step, key, why } => {
                write!(f, "a read of the value of {step} is keyed by {key}, no input of it: {why}")
            }
        }
    }
}

impl std::error::Error for KernelError {}

/// `blaze kernel c`, then the layout's number: the value of a cover, before its count and rows.
const MANIFEST: [u8; 16] = *b"blaze kernel c 1";

/// How many times [`Kernel::cover`] follows [`World::cover`] from one read.
pub const MOST_COVERS: usize = 8;

/// The members a cover is to be kept over ([`Kernel::member`], [`Kernel::cover`]).
#[derive(Default)]
pub struct Members {
    rows: Vec<Member>,
}

struct Member {
    id: ContentId,
    key: Key,
    hash: ContentId,
    of: Of,
    reads: Vec<(ReadKind, Vec<u8>)>,
}

/// What [`Kernel::seed`] did: how many members the cover's manifest names, and how many of
/// them the session did not hold and now does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Seeded {
    pub members: usize,
    pub admitted: usize,
}

struct Registered<W: ?Sized> {
    name: &'static str,
    code: CodeId,
    type_id: TypeId,
    /// Its lookup, run and keep behind its input's bytes, when other steps may ask for it.
    asked: Option<Runner<W>>,
    /// Whether it is a cover ([`Kernel::register_cover`]): a carry never looks it up.
    cover: bool,
    /// Whether its value is a table ([`Ask::TABLE`]): a carry compares it part by part.
    table: bool,
    /// Its `maintain` behind its input's bytes, where it is a maintained step
    /// ([`Kernel::register_maintain`]).
    maintain: Option<nest::Maintainer<W>>,
    /// Its [`Ask::key`], a [`Keyer`] of it, where other steps may ask for it: how a value
    /// asked for at the top ([`Kernel::get_in`], whose bound is [`Compute`]) is named by the
    /// key a carry asks it again by.
    keyer: Option<Box<dyn Any + Send + Sync>>,
}

/// [`Ask::key`] of `S`, as the registry keeps it behind `Any`: a get of `S` finds it by `S`'s
/// own type, where its bound says nothing of [`Ask`] ([`Kernel::root`]).
struct Keyer<S: Step>(fn(&S::Input, &mut Vec<u8>));

/// The registry of steps, each with its code, over the one world `W` its steps read. It is
/// built at setup and shared (`&Kernel<W>`) after that.
///
/// One kernel is of one world type: a step that asks for another step's value runs it in the
/// world it reads itself, and a lookup values a stored read in that world, so which world a
/// step is of is checked where it is registered and used, by the compiler.
pub struct Kernel<W: ?Sized> {
    steps: blaze_hash::HashMap<StepId, Registered<W>, Global>,
    /// What [`Kernel::registry`] gives: made again at each registration, so a request reads it.
    registry: ContentId,
}

const REGISTRY: &str = "blaze v2 kernel registry 1";

/// The [`Keyer`] of `S`, as [`Registered::keyer`] holds it.
fn keyer<S: Ask>() -> Box<dyn Any + Send + Sync> {
    // alloc: the registry, once a step that may be asked for
    Box::new(Keyer::<S>(S::key))
}

/// The digest of a registry: every step's number, name and code, whether it may be asked
/// for, whether its value is a table and whether it is maintained, in the order of the
/// numbers.
fn registry_of<W: ?Sized>(steps: &blaze_hash::HashMap<StepId, Registered<W>, Global>) -> ContentId {
    // alloc: once a registration, the steps in the order of their numbers
    let mut sorted: Vec<(StepId, &Registered<W>)> = steps.iter().map(|(step, it)| (*step, it)).collect();
    sorted.sort_by_key(|(step, _)| step.0);
    let mut hasher = Hasher::new(REGISTRY);
    for (step, it) in sorted {
        let name = u32::try_from(it.name.len()).expect("a step's name is short");
        hasher
            .update(&step.0.to_le_bytes())
            .update(&name.to_le_bytes())
            .update(it.name.as_bytes())
            .id(&it.code.0)
            .update(&[u8::from(it.asked.is_some()), u8::from(it.table), u8::from(it.maintain.is_some())]);
    }
    hasher.finish()
}

impl<W: World + ?Sized> Default for Kernel<W> {
    fn default() -> Kernel<W> {
        Kernel::new()
    }
}

impl<W: World + ?Sized> Kernel<W> {
    pub fn new() -> Kernel<W> {
        // alloc: the registry, once a process, a few entries
        let steps = blaze_hash::HashMap::new_in(Global);
        let registry = registry_of(&steps);
        Kernel { steps, registry }
    }

    /// The digest of the registry as it is: every step's number, name and code, and whether
    /// other steps may ask for it. A [`Session`] is of one registry, as it is of one pin: a
    /// step read names its child by number and input, and the child's code is the registry's.
    pub fn registry(&self) -> ContentId {
        self.registry
    }

    /// Registers `S` with its code. A second step of one number or one name is refused.
    pub fn register<S: Step>(&mut self, code: CodeId) -> Result<(), KernelError> {
        self.enter::<S>(code, None, false, false, None, None)
    }

    /// Registers `C` with its code as a cover ([`Kernel::cover`], [`Kernel::seed`]): a step
    /// no step asks for and no carry looks up, whose value is the manifest of its members.
    pub fn register_cover<C: Step>(&mut self, code: CodeId) -> Result<(), KernelError> {
        self.enter::<C>(code, None, true, false, None, None)
    }

    /// Whether the step a read of a step's value names (its number, then its input: what
    /// [`Session::asked`] gives) is a cover.
    pub(crate) fn is_cover(&self, key: &[u8]) -> bool {
        key.split_first_chunk::<2>()
            .and_then(|(step, _)| self.steps.get(&StepId(u16::from_be_bytes(*step))))
            .is_some_and(|it| it.cover)
    }

    /// Registers `S` with its code as a step other steps may ask for ([`Reads::step`]): the
    /// registry keeps how to look it up, run it and keep it from the bytes of its input, which
    /// is how a read of its value is answered at a later lookup.
    pub fn register_ask<S: Ask<World = W>>(&mut self, code: CodeId) -> Result<(), KernelError> {
        self.enter::<S>(code, Some(Runner::of::<S>()), false, S::TABLE, None, Some(keyer::<S>()))
    }

    /// Registers `S` as [`Kernel::register_ask`] does, and as a maintained step: a carry
    /// that repairs it runs [`Maintain::maintain`] from the entry the session had of it.
    pub fn register_maintain<S: Maintain<World = W>>(&mut self, code: CodeId) -> Result<(), KernelError> {
        let maintain = Some(nest::Maintainer::of::<S>());
        self.enter::<S>(code, Some(Runner::of::<S>()), false, S::TABLE, maintain, Some(keyer::<S>()))
    }

    /// The `maintain` of the step numbered `step`, where it is registered as maintained.
    pub(crate) fn maintainer(&self, step: StepId) -> Option<nest::Maintainer<W>> {
        self.steps.get(&step).and_then(|it| it.maintain)
    }

    /// Whether the step a read of a step's value names (as [`Kernel::is_cover`] takes it)
    /// says its value is a table.
    pub(crate) fn is_table(&self, key: &[u8]) -> bool {
        key.split_first_chunk::<2>()
            .and_then(|(step, _)| self.steps.get(&StepId(u16::from_be_bytes(*step))))
            .is_some_and(|it| it.table)
    }

    /// Whether the step numbered `step` says its value is a table.
    pub(crate) fn table_step(&self, step: StepId) -> bool {
        self.steps.get(&step).is_some_and(|it| it.table)
    }

    fn enter<S: Step>(
        &mut self,
        code: CodeId,
        asked: Option<Runner<W>>,
        cover: bool,
        table: bool,
        maintain: Option<nest::Maintainer<W>>,
        keyer: Option<Box<dyn Any + Send + Sync>>,
    ) -> Result<(), KernelError> {
        if let Some(had) = self.steps.get(&S::ID) {
            return Err(KernelError::Twice { first: (S::ID, had.name), second: (S::ID, S::NAME) });
        }
        if let Some((step, had)) = self.steps.iter().find(|(_, it)| it.name == S::NAME) {
            return Err(KernelError::Twice { first: (*step, had.name), second: (S::ID, S::NAME) });
        }
        // alloc: the registry, once a step
        let registered =
            Registered { name: S::NAME, code, type_id: TypeId::of::<S>(), asked, cover, table, maintain, keyer };
        // alloc: the registry, once a step
        self.steps.insert(S::ID, registered);
        self.registry = registry_of(&self.steps);
        Ok(())
    }

    /// How the step numbered `step` is asked for.
    pub(crate) fn runner(&self, step: StepId) -> Result<Runner<W>, KernelError> {
        match self.steps.get(&step) {
            Some(Registered { asked: Some(runner), .. }) => Ok(*runner),
            Some(it) => Err(KernelError::Unasked { step, name: Some(it.name) }),
            None => Err(KernelError::Unasked { step, name: None }),
        }
    }

    /// The step and the input a read of a step's value names, where the kernel has the step.
    fn asked_of<'r>(&self, read: Read<'r>) -> Option<(&Registered<W>, &'r [u8])> {
        let (step, input) = read.key.split_first_chunk::<2>()?;
        Some((self.steps.get(&StepId(u16::from_be_bytes(*step)))?, input))
    }

    /// A read as an error or a check names it: a step's value by the step and its input, any
    /// other read as the world names it. Never on a hit's path.
    pub fn describe(&self, world: &W, read: Read<'_>) -> String {
        if !read.kind.is_kernels() {
            return world.describe(read);
        }
        let Some((key, which)) = table::split(read) else {
            // alloc: an error's or a check's text
            return format!("a read of kind {} whose key names no step: {}", read.kind.0, hex(read.key));
        };
        let value = self.describe_step(key);
        match which {
            table::Which::Whole => value,
            // alloc: an error's or a check's text
            table::Which::Part(part) => format!("the part {} of {value}", hex(part)),
            // alloc: an error's or a check's text
            table::Which::Keys => format!("the keys of {value}"),
        }
    }

    /// The value of the step a read names by `key` (its number, then its input), as an error
    /// or a check names it.
    fn describe_step(&self, key: &[u8]) -> String {
        let read = Read { kind: ReadKind::STEP, key };
        match self.asked_of(read) {
            // alloc: an error's or a check's text
            Some((Registered { name, asked: Some(runner), .. }, input)) => {
                format!("the value of {name} of `{}`", (runner.label)(input))
            }
            // alloc: an error's or a check's text
            Some((it, input)) => format!("the value of {} of input {}", it.name, hex(input)),
            // alloc: an error's or a check's text
            None => format!("the value of a step the kernel does not have, keyed {}", hex(read.key)),
        }
    }

    /// The item a read is of, for a reason's chain to follow: the input of the step whose
    /// value it is, as that step labels it; any other read as the world says.
    pub fn names(&self, world: &W, read: Read<'_>) -> Option<String> {
        if !read.kind.is_kernels() {
            return world.names(read);
        }
        let (key, _) = table::split(read)?;
        let (registered, input) = self.asked_of(Read { kind: ReadKind::STEP, key })?;
        Some((registered.asked?.label)(input))
    }

    /// The code `S` was registered with.
    pub fn code<S: Step>(&self) -> Result<CodeId, KernelError> {
        match self.steps.get(&S::ID) {
            Some(it) if it.type_id == TypeId::of::<S>() => Ok(it.code),
            Some(it) => {
                Err(KernelError::Unregistered { step: S::ID, name: S::NAME, holder: Some(it.name) })
            }
            None => Err(KernelError::Unregistered { step: S::ID, name: S::NAME, holder: None }),
        }
    }

    pub fn identity<S: Step>(&self, input: &S::Input) -> Result<Identity, KernelError> {
        Ok(identity::<S>(self.code::<S>()?, input))
    }

    /// The entry of `input`, else `S` computed now and kept. A hit that `verify` samples is
    /// computed again with fresh reads, and its reads and bytes must equal the entry's.
    ///
    /// A session of this one request: a child asked for by many parents under it is still
    /// verified once, and a world that moved while it ran is [`KernelError::Moved`].
    pub fn get<'s, S: Compute<World = W>, P: Put + ?Sized>(
        &self,
        store: &'s mut P,
        world: &W,
        input: &S::Input,
        verify: &Setting,
    ) -> Result<Got<'s>, KernelError> {
        Ok(self.get_at::<S, P>(&mut Session::new(world), store, world, input, verify)?.0)
    }

    /// [`Kernel::get`] in `session`: what the session verified under its pin is not looked up
    /// again (a child of many requests' parents is verified once; a value the session found
    /// at the top before is one get, with no probe and no list read), and what this request
    /// verifies is the session's after it. `store` is the one store the session is of. A world
    /// that is not at the session's pin, before or after, is [`KernelError::Moved`].
    ///
    /// The value is the session's as any other it verified: where `S` may be asked for
    /// ([`Kernel::register_ask`]), a carry that reaches it repairs it in place and cuts there
    /// when its value is the same bytes, whether or not a step reads it (`Kernel::root`).
    pub fn get_in<'s, S: Compute<World = W>, P: Put + ?Sized>(
        &self,
        session: &mut Session,
        store: &'s mut P,
        world: &W,
        input: &S::Input,
        verify: &Setting,
    ) -> Result<Got<'s>, KernelError> {
        session.enter(world, self.registry)?;
        let (got, row, hash) = self.get_at::<S, P>(session, store, world, input, verify)?;
        self.root::<S>(session, row, input, hash);
        Ok(got)
    }

    /// Names the row `row` of `S` of `input`, a value a request asked for at the top, by the
    /// key a read of its value names it by, and gives it its value's hash where the row has
    /// none (`hash`, from the entry the request read): what a carry that reaches it looks it
    /// up again by, and compares its value against, as it does a value a step read
    /// ([`Kernel::carry`]). A row a step read is named already.
    ///
    /// Only a step that may be asked for has such a key (its input's bytes, [`Ask::key`]): a
    /// value of any other step that no step reads is dropped when a carry reaches it
    /// ([`Carried::gone`]), and looked up when it is next asked for.
    fn root<S: Step>(&self, session: &mut Session, row: u32, input: &S::Input, hash: Option<ContentId>) {
        if session.asked(row).is_none() {
            let keyer = self.steps.get(&S::ID).and_then(|it| it.keyer.as_deref());
            let Some(Keyer(key_of)) = keyer.and_then(|it| it.downcast_ref::<Keyer<S>>()) else {
                return;
            };
            // alloc: one root's key, once its row
            let mut key = Vec::new();
            key_of(input, &mut key);
            session.link(row, S::ID, &key);
        }
        if let Some(hash) = hash {
            session.fill_hash(row, hash);
        }
    }

    /// A get in a session whose pin is the world's at the start: the value, its row in the
    /// session, and the hash of its value where the request has it with no work (an entry
    /// with reads keeps it; a bare value's is made only for a parent that asked).
    fn get_at<'s, S: Compute<World = W>, P: Put + ?Sized>(
        &self,
        session: &mut Session,
        store: &'s mut P,
        world: &W,
        input: &S::Input,
        verify: &Setting,
    ) -> Result<(Got<'s>, u32, Option<ContentId>), KernelError> {
        let identity = self.identity::<S>(input)?;
        let valued = {
            let mut lent = LentMut(&mut *store);
            let mut engine = Engine::new(self, world, Door::Keep(&mut lent), verify, session);
            engine.value::<S>(input, &identity, Need::Key)
        };
        // Before the answer, or the request's own error: a world that moved gives neither.
        session.leave(world)?;
        match valued? {
            Some(Valued { key, hash, how: How::Computed { value, reads }, row, .. }) => {
                Ok((Got::Computed { key, value, reads }, row, hash))
            }
            Some(Valued { key, hash, how: How::Hit { .. }, row, .. }) => {
                let store: &'s P = store;
                let entry = read_entry::<S, P>(store, key, input, &identity)?;
                let hash = hash.or_else(|| entry.kept_hash());
                Ok((Got::Hit(entry), row, hash))
            }
            None => unreachable!("a get keeps, and what keeps finds a value or makes it"),
        }
    }

    /// Carries `session` from its pin to the pin `world` is at now, given `changed`, what
    /// differs between the two: afterwards the session is of the new pin, and every value it
    /// holds is the one a lookup with no session finds there. `store` is the one store the
    /// session is of. What a request then pays for a value the carry did not reach is what it
    /// paid before: nothing but the get of its entry.
    ///
    /// - **Seed.** The entries with a read `changed` names ([`Changed`], total by kind of
    ///   read) are dropped from the session and queued, found through the postings their
    ///   admission wrote. For a kind said [`Changed::valued`], the reads that differ are found
    ///   here: each read of the kind the session posted is valued once in `world` and compared
    ///   with what its posting says its entries saw, and the readers of each that is other
    ///   (gone, new, not valuable now) are the ones queued; what it is now is kept on the
    ///   posting for the next carry.
    /// - **Every reached entry with a key is repaired.** A value a step read, and one a request
    ///   asked for at the top of a step that may be asked for ([`Kernel::get_in`]), is named
    ///   by the key a read of it uses, so it is repaired in place and cut where its value is
    ///   the same bytes, whether or not anything reads it. Only a value of a step that may not
    ///   be asked for that no step read has no key: reached, it is dropped (`gone`) and looked
    ///   up when it is next asked for.
    /// - **Repair, lowest first.** The lowest queued entry is looked up again by the kernel's
    ///   ordinary lookup at the new pin: an entry the store has for the reads as they are now
    ///   is a hit, else it is run and kept. Its value's hash is compared with the one its
    ///   readers saw: the same, and its readers stay verified with no work (the cutoff);
    ///   another, and its readers are dropped and queued at their heights. An entry that
    ///   cannot be valued at the new pin (its step refuses, a read of its world fails) is
    ///   dropped, and its readers are queued: the failure is the error of whoever asks for it
    ///   next.
    /// - **Bounded** (rule 18): by the queue, and by a count of the session's rows past which
    ///   the session is dropped whole ([`Dropped::Repairs`]).
    ///
    /// Refused, with the session as it was: a change set from another pin
    /// ([`CarryError::From`]), one that leaves a kind of read unsaid ([`CarryError::Unsaid`]),
    /// a world that is not at the change set's pin ([`KernelError::Moved`]). A kernel whose
    /// registry is another drops the session whole ([`Dropped::Registry`]). A failure of the
    /// store, or a world that moves during the repair, is the carry's error, and the session
    /// then holds nothing.
    ///
    /// The invariant: if every entry of the session is indexed (I1: its postings were written
    /// when it was admitted) and was valued at the old pin (I4: nothing reads the session
    /// while it is carried, which `&mut` says), the seed covers every read whose value
    /// differs (I2), and an entry is repaired after every queued entry it reads (I3: lowest
    /// first, and a reader is queued whenever an entry it read comes to another hash, even
    /// one repaired already), then every entry the session holds after the carry is what a
    /// lookup with no session gives at the new pin. A valued kind meets I2 by itself: every
    /// live entry's reads are posted (I1) with what they saw at the old pin (I4), and the seed
    /// compares each with the value every lookup uses. By induction on height: an entry never
    /// queued has no read that differs and no child whose hash differs, so its list holds as
    /// it did; a queued one was looked up after its children were final. Checked by
    /// `exhaustive/nested.rs` (every sequence, with the pin moved under a carried session at
    /// every place, against a computation with no kernel), with a planted bug for each of
    /// I1 (twice), I2 and I3 that must end stale.
    pub fn carry<P: Put + ?Sized>(
        &self,
        session: &mut Session,
        store: &mut P,
        world: &W,
        changed: &Changed,
        verify: &Setting,
    ) -> Result<Carried, KernelError> {
        let mut lent = LentMut(&mut *store);
        nest::carry(self, session, &mut lent, world, changed, verify)
    }

    /// Why the store has no entry of `input` whose reads all hash the same now, against `last`:
    /// the key of the entry the caller saw kept last under its identity ([`Got::Computed`]'s,
    /// remembered as [`Miss::kept`]), none when it saw none. For a lookup that found nothing;
    /// worked out by reading the store and the world again, so only when it is wanted, and it
    /// writes nothing.
    pub fn why<S: Step, G: Get + ?Sized>(
        &self,
        store: &G,
        world: &W,
        input: &S::Input,
        last: Option<Key>,
    ) -> Result<Missed, KernelError> {
        let identity = self.identity::<S>(input)?;
        let (lent, off) = (Lent(store), Setting::OFF);
        let mut session = Session::new(world);
        let mut engine = Engine::new(self, world, Door::Look(&lent), &off, &mut session);
        let mut now = |read: Read<'_>| engine.now(read);
        Ok(why::explain(store, self, world, &identity, last, &mut now))
    }

    /// The entry of `input` whose reads all hash the same now, if the store has one: of a step
    /// the kernel computed, or of one kept with a trace made elsewhere ([`Kernel::keep`]).
    pub fn find<'s, S: Step, G: Get + ?Sized>(
        &self,
        store: &'s G,
        world: &W,
        input: &S::Input,
    ) -> Result<Option<Entry<'s>>, KernelError> {
        self.find_at::<S, G>(&mut Session::new(world), store, world, input)
    }

    /// Notes `S` of `input` as a member of the cover `members` is for: it must be verified
    /// in `session` (asked for at this pin), and its entry is read for its key, its value's
    /// hash and its reads, which [`Kernel::cover`] checks.
    pub fn member<S: Step, G: Get + ?Sized>(
        &self,
        members: &mut Members,
        session: &mut Session,
        store: &G,
        input: &S::Input,
    ) -> Result<(), KernelError> {
        let identity = self.identity::<S>(input)?;
        let of = of::<S>(input, &identity);
        let Some((_, known)) = session.known(store, &identity.id())? else {
            return Err(KernelError::Uncovered {
                member: of,
                // alloc: an error
                read: "nothing yet".to_owned(),
                // alloc: an error
                why: "it is not verified in this session: ask for it at this pin first".to_owned(),
            });
        };
        let entry = read_entry::<S, G>(store, known.key, input, &identity)?;
        // alloc: one member's reads, until its cover is kept
        let reads = entry.reads().map(|(read, _)| (read.kind, read.key.to_vec())).collect();
        // alloc: the members of one cover, until it is kept
        members.rows.push(Member {
            id: identity.id(),
            key: known.key,
            hash: entry.value_hash(),
            of,
            reads,
        });
        Ok(())
    }

    /// Keeps the cover `C` of `input` over `members`: an entry whose reads are `reads` (the
    /// covering reads, valued now) and whose value is the manifest of the members: each one's
    /// identity, entry key and value hash, in order of identity. Its key.
    ///
    /// The proof that it covers is checked here, and a cover that fails it is not kept
    /// ([`KernelError::Uncovered`], naming the member and the read): every read of every
    /// member is under one of `reads` by the world's word ([`World::cover`], followed at most
    /// [`MOST_COVERS`] times), and a read of a step's value is of a member. So while the
    /// covering reads hash the same, every member's value is the bytes its entry holds.
    ///
    /// A covering read is a world read: the hash of an interface's value is given by the
    /// world as a read of its own kind. Whoever keeps a cover asked for each member in
    /// `session` at this pin; a world that moved since is [`KernelError::Moved`].
    pub fn cover<C: Step, P: Put + ?Sized>(
        &self,
        session: &mut Session,
        store: &mut P,
        world: &W,
        input: &C::Input,
        reads: &[Read<'_>],
        members: &Members,
    ) -> Result<Key, KernelError> {
        session.enter(world, self.registry)?;
        let identity = self.identity::<C>(input)?;
        let mut trace = Trace::new();
        for read in reads {
            let seen = world.hash(*read).map_err(|error| KernelError::World {
                of: of::<C>(input, &identity),
                read: self.describe(world, *read),
                // alloc: an error
                why: format!("{error:?}"),
            })?;
            // The same read named twice sees the same: valued once is enough.
            let _ = trace.read(*read, seen);
        }
        // alloc: the members' identities in order, for one keep
        let mut ids: Vec<ContentId> = members.rows.iter().map(|it| it.id).collect();
        ids.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        for member in &members.rows {
            for (kind, key) in &member.reads {
                let read = Read { kind: *kind, key };
                let why = if kind.is_kernels() {
                    // A part, or the keys, of a step's value: the step must be a member.
                    let step = table::split(read).map_or(&key[..], |(step, _)| step);
                    match session.named_row(ReadKind::STEP, step) {
                        Some(child) if ids.binary_search_by(|it| it.0.cmp(&child.0)).is_ok() => continue,
                        _ => "the value of a step that is not a member of the cover",
                    }
                } else {
                    let mut at = read;
                    let mut under = reads.contains(&at);
                    // Bounded (rule 18): a covering read is under a wider one a few times.
                    for _ in 0..MOST_COVERS {
                        if under {
                            break;
                        }
                        match world.cover(at) {
                            Some(wider) => at = wider,
                            None => break,
                        }
                        under = reads.contains(&at);
                    }
                    if under {
                        continue;
                    }
                    "which is under none of the cover's reads (`World::cover`)"
                };
                return Err(KernelError::Uncovered {
                    // clone: an error
                    member: member.of.clone(),
                    read: self.describe(world, read),
                    // alloc: an error
                    why: why.to_owned(),
                });
            }
        }
        // alloc: the manifest, until it is kept
        let mut value = Vec::with_capacity(MANIFEST.len() + 4 + 96 * members.rows.len());
        value.extend_from_slice(&MANIFEST);
        trie::put_var(&mut value, members.rows.len() as u64);
        // alloc: the members in order of identity (rule 25), for one keep
        let mut rows: Vec<&Member> = members.rows.iter().collect();
        rows.sort_unstable_by(|a, b| a.id.0.cmp(&b.id.0));
        for member in rows {
            value.extend_from_slice(&member.id.0);
            value.extend_from_slice(&member.key.0.0);
            value.extend_from_slice(&member.hash.0);
        }
        session.leave(world)?;
        self.keep::<C, P>(store, input, &trace, &value)
    }

    /// Seeds `session` through the cover `C` of `input`, where the store has one that holds
    /// at the world's pin: the ordinary lookup of the cover (its covering reads valued, its
    /// list and its entry read), then every member its manifest names is admitted with no
    /// read of its own valued and no record of its own read. None: no cover holds, and the
    /// session is as it was; each value is then looked up when asked for.
    ///
    /// For a cold start or another checkout: a carry never comes here, and drops a cover
    /// whose reads changed with every member admitted through it (invariant I5).
    pub fn seed<C: Step, G: Get + ?Sized>(
        &self,
        session: &mut Session,
        store: &G,
        world: &W,
        input: &C::Input,
    ) -> Result<Option<Seeded>, KernelError> {
        let Some(entry) = self.find_in::<C, G>(session, store, world, input)? else {
            return Ok(None);
        };
        let identity = self.identity::<C>(input)?;
        let unreadable = |why: &str| KernelError::Store {
            of: of::<C>(input, &identity),
            key: entry.key,
            // alloc: an error
            why: why.to_owned(),
        };
        let rows = entry
            .value
            .strip_prefix(&MANIFEST[..])
            .ok_or_else(|| unreadable("its value is no cover's manifest of layout 1 (`blaze kernel c 1`)"))?;
        let mut input_ = trie::Cursor::new(rows);
        let count = input_.var().and_then(|count| usize::try_from(count).ok());
        let rows = count
            .and_then(|count| input_.take(count.checked_mul(96)?))
            .filter(|_| input_.ended())
            .ok_or_else(|| unreadable("a cover's manifest ends early or has bytes left over"))?;
        let id = |bytes: &[u8]| ContentId(bytes.try_into().expect("32 bytes of a manifest's row"));
        let mut members =
            rows.chunks_exact(96).map(|row| (id(&row[..32]), Key(id(&row[32..64])), id(&row[64..])));
        let admitted = session.cover(store, &identity.id(), C::ID, &mut members)?;
        Ok(Some(Seeded { members: rows.len() / 96, admitted }))
    }

    /// [`Kernel::find`] in `session`, as [`Kernel::get_in`] is of [`Kernel::get`].
    pub fn find_in<'s, S: Step, G: Get + ?Sized>(
        &self,
        session: &mut Session,
        store: &'s G,
        world: &W,
        input: &S::Input,
    ) -> Result<Option<Entry<'s>>, KernelError> {
        session.enter(world, self.registry)?;
        self.find_at::<S, G>(session, store, world, input)
    }

    fn find_at<'s, S: Step, G: Get + ?Sized>(
        &self,
        session: &mut Session,
        store: &'s G,
        world: &W,
        input: &S::Input,
    ) -> Result<Option<Entry<'s>>, KernelError> {
        let identity = self.identity::<S>(input)?;
        let found = {
            let (lent, off) = (Lent(store), Setting::OFF);
            let mut engine = Engine::new(self, world, Door::Look(&lent), &off, session);
            engine.found::<S>(input, &identity)
        };
        session.leave(world)?;
        match found? {
            Some(key) => Ok(Some(read_entry::<S, G>(store, key, input, &identity)?)),
            None => Ok(None),
        }
    }

    /// Keeps `value` as the entry of `input` with `trace`: the reads its run made where the
    /// kernel could not run it (a process: a build script's traced reads), each with the hash
    /// it saw. [`Kernel::find`] then takes the entry by the rule of every entry: only when
    /// every read of `trace` hashes the same now. Other bytes under the same identity and
    /// reads are a [`KernelError::Conflict`] naming the identity and the reads, and the first
    /// value stays, as for an entry the kernel computed.
    ///
    /// For a step the kernel could run itself this is an interim, listed as such per step
    /// (docs/v2/plan/design/kernel.md, "The kernel as the home", (b)): a trace made elsewhere
    /// is complete only as far as whoever made it is, where a step run through [`Reads`]
    /// cannot leave a read out. No world is at hand here, so an error names a read by its
    /// kind and key.
    pub fn keep<S: Step, P: Put + ?Sized>(
        &self,
        store: &mut P,
        input: &S::Input,
        trace: &Trace,
        value: &[u8],
    ) -> Result<Key, KernelError> {
        let identity = self.identity::<S>(input)?;
        let reads = || -> Vec<String> {
            // alloc: an error
            trace.iter().map(|(read, seen)| said(plain(read), seen)).collect()
        };
        Ok(keep::<S, P>(store, input, &identity, trace, value, reads)?.key)
    }

    /// The entry with no reads of a step computed elsewhere, if the store has one.
    pub fn find_whole<'s, S: Step, G: Get + ?Sized>(
        &self,
        store: &'s G,
        input: &S::Input,
    ) -> Result<Option<Entry<'s>>, KernelError> {
        let identity = self.identity::<S>(input)?;
        let key = whole::key(&identity);
        match store.get(&key) {
            Ok(Some(bytes)) => Ok(Some(Entry::whole(bytes, identity))),
            Ok(None) => Ok(None),
            Err(why) => Err(KernelError::Store { of: of::<S>(input, &identity), key, why }),
        }
    }

    /// Keeps `value` as the entry with no reads of a step computed elsewhere. Other bytes under
    /// its key are a [`KernelError::Conflict`].
    pub fn keep_whole<S: Step, P: Put + ?Sized>(
        &self,
        store: &mut P,
        input: &S::Input,
        value: &[u8],
    ) -> Result<Key, KernelError> {
        let identity = self.identity::<S>(input)?;
        let key = whole::key(&identity);
        keep_entry::<S, P>(&RefCell::new(store), input, &identity, key, value, Vec::new)?;
        Ok(key)
    }
}

fn of<S: Step>(input: &S::Input, identity: &Identity) -> Of {
    Of { step: S::NAME, label: S::label(input), identity: identity.id() }
}

/// Why a lookup's walk of an identity's read-lists stopped.
enum Stop {
    Chain(blaze_record::Error<Stopped>),
    /// The slot `at` holds a read-list one of whose reads is named by `len` bytes: no kernel
    /// read's name.
    Name { at: Slot, len: usize },
    /// Valuing a read, or reading the answer, failed: the lookup's error as it is.
    Kernel(KernelError),
}

impl From<blaze_record::Error<Stopped>> for Stop {
    fn from(error: blaze_record::Error<Stopped>) -> Stop {
        Stop::Chain(error)
    }
}

/// The error of a lookup or a filing that `blaze_record` stopped, for the identity whose ID is
/// `id`. A second value of an entry is not this: [`keep`] makes it a
/// [`KernelError::Conflict`], which names the reads.
fn chain_error(of: Of, id: &ContentId, error: blaze_record::Error<Stopped>) -> KernelError {
    match error {
        blaze_record::Error::Store { doing, key, error } => KernelError::Store {
            of,
            key: Key(key),
            // alloc: an error
            why: format!("{doing}: {}", error.why()),
        },
        blaze_record::Error::List { at, why } => KernelError::Store {
            of,
            key: Key(list_key(id, at)),
            // alloc: an error
            why: format!("slot {} of its chain of read-lists holds {why}", at.n),
        },
        blaze_record::Error::TwoAnswers { key } => KernelError::Store {
            of,
            key: Key(key),
            // alloc: an error
            why: "another value is kept under its entry's key".to_owned(),
        },
    }
}

/// What a lookup's walk of an identity's read-lists asks of whoever walks ([`find_key`]).
pub(crate) trait Lists<T> {
    /// What `read` is now. A list with a read that cannot be valued does not hold, and one
    /// with a step's value that must be run first is passed over (`Engine::lookup`).
    fn now(&mut self, read: Read<'_>) -> Result<Now, KernelError>;

    /// The entry under `key`, none where the store has nothing there: probed or read as the
    /// caller needs (`nest::Need`), so a lookup reads the store no more than its caller uses.
    fn answer(&mut self, key: Key) -> Result<Option<T>, KernelError>;

    /// `found` is the entry of the list in `slot` of the identity's chain, whose reads are
    /// named and saw as `reads` says (what [`Lists::now`] gave for each, in the list's
    /// order); no slot and no reads for the entry with no reads. Said once, for the entry the
    /// walk ends with: where a session's postings are written, with what each read saw.
    fn held<'n>(
        &mut self,
        slot: Option<u32>,
        reads: &mut dyn Iterator<Item = (&'n [u8], Seen)>,
        found: &mut T,
    );

    /// Whether the identity is known to have an entry with reads: then its entry with no reads
    /// is not looked for. A step is pure, so the first read it makes is made by every run of
    /// one input: an identity with an entry that has reads has none without.
    fn listed(&self) -> bool {
        false
    }
}

/// The entry of `identity` whose reads all hash the same now: the entry with no reads, else
/// the first read-list in the identity's chain whose reads' hashes now name an entry the store
/// has. `lists` values the reads, reads the answer and is told the list that held.
fn find_key<S: Step, G: Get + ?Sized, T>(
    store: &G,
    input: &S::Input,
    identity: &Identity,
    lists: &mut dyn Lists<T>,
) -> Result<Option<T>, KernelError> {
    let id = identity.id();
    let failed =
        |key: Key, why: String| KernelError::Store { of: of::<S>(input, identity), key, why };
    if !lists.listed()
        && let Some(mut found) = lists.answer(whole::key(identity))?
    {
        lists.held(None, &mut std::iter::empty(), &mut found);
        return Ok(Some(found));
    }
    // What each read of the list being walked is now, for the postings of the one that holds:
    // one buffer for the walk, cleared a list.
    // alloc: one lookup's values of one list's reads
    let mut seen: Vec<Seen> = Vec::new();
    let walked = walk::<_, T, Stop, _>(&Looking(store), entry::BOUNDS, &id, |at, bytes, list| {
        let mut key = AnswerKey::new(bytes);
        let mut holds = true;
        seen.clear();
        for name in list.reads() {
            let read = entry::named(name).ok_or(Stop::Name { at, len: name.len() })?;
            // The walk leaves the reads to its visitor: each one asked the value of is said.
            blaze_record::Store::tally(&Looking(store)).valued(1);
            match lists.now(read).map_err(Stop::Kernel)? {
                Now::Seen(now) => {
                    key.value(entry::digest(now));
                    // alloc: one lookup's values of one list's reads
                    seen.push(now);
                }
                Now::Pending => holds = false,
                Now::Not => return Ok(None),
            }
        }
        if !holds {
            return Ok(None);
        }
        let Some(mut found) = lists.answer(Key(key.finish())).map_err(Stop::Kernel)? else {
            return Ok(None);
        };
        // Every read of a list that holds was seen (none was pending), so the two are zipped
        // read for read.
        lists.held(Some(at.n), &mut list.reads().zip(seen.iter().copied()), &mut found);
        Ok(Some(found))
    });
    match walked {
        Ok(Walked::Found { found, .. }) => Ok(Some(found)),
        Ok(Walked::Passed { full: false, .. }) => Ok(None),
        Ok(Walked::Passed { full: true, .. }) => {
            Err(KernelError::TooManyShapes { of: of::<S>(input, identity) })
        }
        Err(Stop::Chain(error)) => Err(chain_error(of::<S>(input, identity), &id, error)),
        Err(Stop::Name { at, len }) => Err(failed(
            Key(list_key(&id, at)),
            // alloc: an error
            format!(
                "slot {} of its chain of read-lists names a read by {len} bytes, too few to hold its kind",
                at.n
            ),
        )),
        Err(Stop::Kernel(error)) => Err(error),
    }
}

fn read_entry<'s, S: Step, G: Get + ?Sized>(
    store: &'s G,
    key: Key,
    input: &S::Input,
    identity: &Identity,
) -> Result<Entry<'s>, KernelError> {
    let failed = |why: String| KernelError::Store { of: of::<S>(input, identity), key, why };
    // alloc: an error
    let bytes = store
        .get(&key)
        .map_err(failed)?
        .ok_or_else(|| failed("its entry went missing after it was probed".to_owned()))?;
    Entry::read(bytes, key, identity).map_err(failed)
}

/// An entry kept: its key, and the hash of its value's bytes where the entry keeps it (one
/// with reads), made once while the entry was written.
pub(crate) struct Kept {
    pub key: Key,
    pub hash: Option<ContentId>,
    /// The slot of its read-list in its identity's chain: none for an entry with no reads.
    pub slot: Option<u32>,
}

/// Keeps `value` as the entry of `identity` and `trace`, then makes sure the trace's read-list
/// is in the identity's chain. The entry goes first: a list found always names reads that some
/// entry recorded.
///
/// `store` must read its own puts at once (blaze-cas does, through every writer): a slot taken
/// by another list is found by its get, and a slot taken between the get and the put (another
/// writer, a [`blaze_cas::Mismatch`]) is read again and passed over for the next slot.
///
/// `reads` names the trace's reads for a conflict's error, asked only then.
fn keep<S: Step, P: Put + ?Sized>(
    store: &mut P,
    input: &S::Input,
    identity: &Identity,
    trace: &Trace,
    value: &[u8],
    reads: impl Fn() -> Vec<String>,
) -> Result<Kept, KernelError> {
    let store = RefCell::new(store);
    if trace.is_empty() {
        let key = whole::key(identity);
        keep_entry::<S, P>(&store, input, identity, key, value, &reads)?;
        return Ok(Kept { key, hash: None, slot: None });
    }
    let entry = entry::encode(identity, trace, value);
    keep_entry::<S, P>(&store, input, identity, entry.key, &entry.bytes, &reads)?;
    let id = identity.id();
    let records = Keeping { store: &store, answers: S::KIND };
    match Filing::new(id, entry::BOUNDS).place(&records, entry.list()) {
        Ok(Placed::Written(at) | Placed::Kept(at)) => {
            Ok(Kept { key: entry.key, hash: Some(entry.hash), slot: Some(at.n) })
        }
        Ok(Placed::Full) => Err(KernelError::TooManyShapes { of: of::<S>(input, identity) }),
        Err(error) => Err(chain_error(of::<S>(input, identity), &id, error)),
    }
}

/// Keeps `bytes` as the entry under `key`: `blaze_record`'s rule that a key names one answer.
/// A second value under it is a [`KernelError::Conflict`] naming `reads` (rule 21), whether
/// the get before the put saw the first, or the put itself did where another writer kept its
/// value in between; the first stays.
fn keep_entry<S: Step, P: Put + ?Sized>(
    store: &RefCell<&mut P>,
    input: &S::Input,
    identity: &Identity,
    key: Key,
    bytes: &[u8],
    reads: impl FnOnce() -> Vec<String>,
) -> Result<(), KernelError> {
    let failed = |why: String| KernelError::Store { of: of::<S>(input, identity), key, why };
    let second = |kept: ContentId, given: ContentId| KernelError::Conflict {
        of: of::<S>(input, identity),
        key,
        reads: reads(),
        kept,
        given,
    };
    match keep_answer(&Keeping { store, answers: S::KIND }, key.0, bytes) {
        Ok(_) => Ok(()),
        Err(blaze_record::Error::TwoAnswers { .. }) => {
            let held = store.try_borrow().map_err(|_| failed(Stopped::Held.why()))?;
            let refused = match Get::get(&**held, &key) {
                Ok(Some(kept)) => second(content_of(kept), content_of(bytes)),
                // alloc: an error
                Ok(None) => failed(
                    "another value was kept under its entry's key, and then none".to_owned(),
                ),
                Err(why) => failed(why),
            };
            Err(refused)
        }
        Err(blaze_record::Error::Store {
            doing: Doing::KeepingAnswer,
            error: Stopped::Refused(mismatch),
            ..
        }) => Err(second(mismatch.kept, mismatch.given)),
        Err(error) => Err(chain_error(of::<S>(input, identity), &identity.id(), error)),
    }
}

/// A read by its name and what it saw, as an error or a check gives it.
fn said(read: String, seen: Seen) -> String {
    match seen {
        Seen::Is(hash) => format!("{read} = {hash:x}"), // alloc: an error's or a check's text
        Seen::Gone => format!("{read} (gone)"), // alloc: an error's or a check's text
    }
}

/// A read by its kind and key alone: what a world that does not name it gives, and what the
/// kernel gives where it has no world.
fn plain(read: Read<'_>) -> String {
    // alloc: an error's or a check's text
    format!("read {} of {}", read.kind.0, hex(read.key))
}

/// `bytes` in hex, at most 32 of them.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    // alloc: an error's or a check's text
    let mut out = String::with_capacity(2 * bytes.len().min(32) + 16);
    for byte in bytes.iter().take(32) {
        let _ = write!(out, "{byte:02x}");
    }
    if bytes.len() > 32 {
        let _ = write!(out, "... ({} bytes)", bytes.len());
    }
    out
}
