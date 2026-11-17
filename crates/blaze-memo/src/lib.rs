//! The memo the front end's queries run on (docs/v2/plan/design/target.md, "In memory"): one
//! snapshot's answers, a table per arena, with no dependency tracking.
//!
//! A [`Memo`] is one snapshot: its inputs never change while it lives, and there are no
//! revisions, edges or durabilities. The baseline is a process that takes one snapshot,
//! answers and exits, so an id never outlives what it names; all reuse across processes is
//! records, valid by key or by read list (the crate's own image, [`Memo::serve`]), never an
//! arena kept alive. [`Memo::next`] is the resident helper's memory management only: it carries
//! the arenas no change reached into a next snapshot and drops the rest wholesale.
//!
//! - **Arenas.** One per crate, at index `1 +` its topological position (dependencies first),
//!   and the session's (index [`ArenaId::SESSION_INDEX`], 0) for what names no crate: a
//!   snapshot's own is [`Memo::session`], at the generation that snapshot holds, and no
//!   constant names one (a commit that drops the session, or a new session, makes it again
//!   at the next generation, so a constant would name a dead arena from then on). Every entry,
//!   trie node, read list and interned value of an arena is bump-allocated in its
//!   [`blaze_mem::Arena`] and freed with it.
//! - **Ids** are process-local `(arena, local)` pairs ([`Gid`]): the arena's index and the
//!   generation of its making, and an index into its column. Never persisted (a persisted
//!   word holds a stable id); an id of a dropped arena is dead and refused by name. A
//!   database's sessions share one id space ([`Memo::next_session`]), in which an id is handed
//!   out once: it names one making for ever, so a cache outside the memo keyed by (id space,
//!   id) is never answered by another making's memory. Each index has [`MAX_GENERATION`]` + 1`
//!   makings in an id space; one past the last is a typed refusal
//!   ([`NextRefused::GenerationsSpent`]), never a reused generation.
//! - **A key's home** ([`Home`]) is the greatest arena among the ids it holds, the session's
//!   when it holds none: one key has one entry in a snapshot.
//! - **Borrows are declared.** The driver names what each arena may read
//!   ([`Inputs::borrows`]: its direct dependencies and the session); a key, intern or read of
//!   an arena outside that closure panics naming the key. [`Memo::next`] drops the arenas the
//!   driver names changed and, to a fixpoint, every arena that borrowed from a dropped one, so
//!   a `&'db` borrow in a value points only into arenas that outlive its own.
//! - **Values own nothing to drop** (`!needs_drop`, checked at compile time: for each query's
//!   value and interned struct's fields by an item-level `const` that `cargo check` evaluates;
//!   for a [`keep`]ed value at monomorphization, so when built, not when checked): a value that
//!   needs a slice keeps it in its run's arena ([`keep_slice`], [`keep_iter`], [`keep_str`]). A
//!   value that is a real tree opts in with `owns` (`#[tracked(.., owns)]`,
//!   `#[interned(.., owns)]`), or is kept with [`keep_owned`]: its destructor is on its arena's
//!   drop list and runs when the arena drops. Every `owns` and every `keep_owned` call is counted
//!   per crate in `owns.txt`, a ratchet that only goes down.
//!   An `owns` query may iterate a fixpoint (`cycle_initial`): each round's value is placed once
//!   and dropped once with its arena, all of them kept until then (rounds × value size, at most
//!   `MAX_ROUNDS` rounds, counted in [`Counts::owned_rounds`]).
//! - **Old snapshots keep reading.** A snapshot that has a successor keeps answering for the
//!   tasks still in it; what it computes after the successor was made goes to its own spill
//!   table, never into an arena it now shares ([`Memo::next`] freezes each carried arena for
//!   its predecessors with a writer count it waits out, nanoseconds, bounded).
//! - **Hit path.** Home (a pass over the key), a slice index, one generation compare, one
//!   trie walk of acquire loads (16-way nodes, about log16 of the table's size), a key
//!   compare. No lock, no shared counter, no write to a shared word.
//! - **Miss path.** Single flight through the injected [`Flight`] ([`Scheduled`]: the
//!   scheduler's `pump::Flights`, a second asker parked on the scheduler, never in the kernel);
//!   the run on this task's stack of runs ([`task`]); the entry bump-allocated in the home
//!   arena and published with one compare-and-swap.
//! - **A run is a task frame.** Each run of a query's code (its body, each fixpoint round, its
//!   `cycle_*` functions) is a `blaze_mem::frame::task` of the query's tag (`mem_tag = ..`,
//!   `Tag::Query` by default), so scratch taken through `frame::current` is counted under it and
//!   freed when the run ends; one frame per run, tens of instructions and no allocation when
//!   the run takes nothing from it (`fetch::in_frame`). An input or interned struct runs no
//!   user code and gets none.
//! - **Cycles** keep blaze-query's observable results: `cycle_result` gives the head and every
//!   participant its own fallback; `cycle_initial` with an optional `cycle_fn` iterates the
//!   head to a fixpoint (`MAX_ROUNDS`, then a panic naming it); a query with neither panics
//!   naming the cycle's path.
//! - **The read recorder** ([`record`]) is off unless the snapshot was made with
//!   [`Reads::Kept`]: then each entry keeps what its run read directly (its input rows and
//!   the entries it asked), and an ask, hit or run, adds one row naming the entry to its
//!   asker; the flat list of [`Read`] rows a result depends on, through everything it called,
//!   is made when asked for ([`record`], an image record), once per entry, so it is the same
//!   whether the result was computed or found. A fact boundary ([`Builder::mark_boundary`]) is listed in its asker as one
//!   [`Read::Query`] row, its own list kept behind (the firewall). [`unrecorded`] runs
//!   bookkeeping whose reads are no part of the running query's value.
//!
//! What a query looks like is unchanged from blaze-query
//! (`#[blaze_memo::tracked(returns(ref))] fn q(db: &dyn Db, ..) -> V`); what it may return is
//! not (see `blaze_memo_macros` and the porting how-to in `agents/howto/H-port-query-to-memo.json`).

#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

mod arena;
mod attach;
mod counts;
mod fetch;
mod flight;
mod home;
mod image;
mod input;
pub mod testing;
mod intern;
mod keep;
mod memo;
mod record;
mod resident;
mod task;
mod trie;

use std::{fmt, num::NonZeroU32};

pub use blaze_keyed::record::Fact;
pub use blaze_memo_macros::{Home, input, interned, tracked};

pub use attach::{Bounded, FIELD_CAP, KEY_CAP, attach, with_attached, with_attached_db};
pub use counts::{Counts, completed, on_each_run, take_counts};
pub use fetch::{Query, Recovery, fetch};
pub use flight::{Alone, Flight, FlightKey, Flown, Hold, Kept, Scheduled};
pub use home::{Home, Homes};
pub use image::{Image, ImageOf, Imaged, Stable, StableKey};
pub use input::{Input, input, input_fact, new_input, set_singleton, singleton, try_singleton};
pub use intern::{Interned, intern, interned};
pub use keep::{keep, keep_iter, keep_owned, keep_slice, keep_str};
pub use memo::{Builder, Config, Dead, Epoch, Inputs, Live, Memo, Next, NextRefused, Plant, Reads};
pub use record::{
    AROUND_INPUTS, IMAGE_FACT, KindId, Read, Reader, Readers, RecorderOff, UNTRACKED, read, read_untracked,
    record, record_in, record_readers, unrecorded,
};
pub use resident::Resident;
pub use task::register;

/// What the front end's database gives the memo: the snapshot its queries read.
///
/// `base_db::Db` implements it; every generated query reaches the memo through it, as
/// `blaze_query::db::Database::storage` was reached before.
pub trait Database: Send + Sync {
    fn memo(&self) -> &Memo;
}

/// How many bits of an [`ArenaId`] count its generation: the arena at one index is made
/// again each time it is dropped, and an id of an earlier making is dead.
const GENERATION_BITS: u32 = 12;
const GENERATION_MASK: u32 = (1 << GENERATION_BITS) - 1;

/// The last generation an arena index has in an id space (every session of one database): its
/// making after this one would take a generation the bits cannot hold, so [`Memo::next`]
/// refuses to drop an arena at it and [`Memo::next_session`] a session that would make it
/// again ([`NextRefused::GenerationsSpent`]). One below the mask: no arena's bits are all
/// ones, which is [`ArenaId`]'s niche.
pub const MAX_GENERATION: u32 = GENERATION_MASK - 1;

/// The most arena indices a session has: the index's bits.
pub const MAX_ARENAS: u32 = 1 << (32 - GENERATION_BITS);

/// An arena of a session: its index (the driver's, a crate's surface or bodies, ordered so a
/// crate comes after its dependencies) and the generation of its making.
///
/// The invariant: within one id space (every snapshot of every session of one database:
/// [`Memo::next`] carries it, [`Memo::next_session`] takes it over; only [`Memo::builder`]
/// makes a new one) an `ArenaId` is handed out once, so it names one making of an arena for
/// ever: once that arena is dropped no later arena has its id, and an id equal to a live
/// arena's is that arena. A cache outside the memo may key by it (with the id space's
/// identity, base-db's database nonce) and judge an entry live by the id alone. Another id
/// space hands out the same ids again.
///
/// Ordered by index first, so the [`Home`] of a key is the most downstream arena it names.
///
/// Made only by this crate, from an arena a snapshot or a builder holds ([`Builder::arena`],
/// [`Builder::id`], [`Builder::session`], [`Memo::session`], [`Memo::arenas`], [`Gid::arena`]):
/// there is no constant of one, so no code can name a generation it did not get from a live
/// arena. In particular there is no constant for the session arena: it is made again at each
/// commit that drops it, and a constant would name its first making only (`arena 0.0 is dead`
/// from the second on). A value of session-level data that names no arena says so with `None`
/// (an `Option<ArenaId>`, 4 bytes by the niche, ordered below every arena: the bottom of a
/// "greatest arena named"), and a key that names none is homed at the asking snapshot's own
/// session arena ([`Memo::session`]). The session's index, where an index is what is meant, is
/// [`ArenaId::SESSION_INDEX`].
///
/// How the types hold it (a compile-time property; this crate runs no doc tests, so it is
/// stated here, where `ArenaId::SESSION` was): the field is private and every constructor is
/// `pub(crate)` ([`ArenaId::new`], `from_bits`), so outside this crate an `ArenaId` comes only
/// from a call that reads a live arena; `ArenaId::SESSION` is gone, and no `const` of this type
/// is public. [`Home`] has no way to say "the session" but naming nothing: `Homes::arena` takes
/// an `ArenaId`, which session-level data cannot have made up.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArenaId(
    /// The complement of the bits `index << GENERATION_BITS | generation`: never zero, as no
    /// arena's generation is `GENERATION_MASK` ([`MAX_GENERATION`]). Complemented, so the bits
    /// of index 0 generation 0 stay an id's (a `Gid::from_session_bits` word with a zero arena
    /// half is one), and the all-ones word, which no arena has, is the niche.
    NonZeroU32,
);

// `None` is free: a home column of `Option<ArenaId>` is 4 bytes a row, as one of `ArenaId`.
const _: () = assert!(size_of::<Option<ArenaId>>() == 4);

/// By `(index, generation)`, the bits' order (not the stored complement's).
impl Ord for ArenaId {
    #[inline]
    fn cmp(&self, other: &ArenaId) -> std::cmp::Ordering {
        self.bits().cmp(&other.bits())
    }
}

impl PartialOrd for ArenaId {
    #[inline]
    fn partial_cmp(&self, other: &ArenaId) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl ArenaId {
    /// The session arena's index: the arena of singletons and of keys that name no arena. An
    /// index, never an arena: the arena at it in a snapshot is [`Memo::session`], whose
    /// generation moves with every commit that drops it.
    pub const SESSION_INDEX: u32 = 0;

    pub(crate) const fn new(index: u32, generation: u32) -> ArenaId {
        assert!(generation <= MAX_GENERATION, "a generation past `MAX_GENERATION`");
        assert!(index < MAX_ARENAS, "an arena index past `MAX_ARENAS`");
        match ArenaId::from_bits((index << GENERATION_BITS) | generation) {
            Some(id) => id,
            None => unreachable!(),
        }
    }

    pub const fn index(self) -> u32 {
        self.bits() >> GENERATION_BITS
    }

    pub const fn generation(self) -> u32 {
        self.bits() & GENERATION_MASK
    }

    /// Whether this is a making of the session arena (index [`ArenaId::SESSION_INDEX`]), of
    /// whatever generation.
    pub const fn is_session(self) -> bool {
        self.index() == ArenaId::SESSION_INDEX
    }

    pub(crate) const fn bits(self) -> u32 {
        !self.0.get()
    }

    /// The arena whose [`ArenaId::bits`] are `bits`; `None` for the all-ones word, which no
    /// arena has.
    const fn from_bits(bits: u32) -> Option<ArenaId> {
        match NonZeroU32::new(!bits) {
            Some(stored) => Some(ArenaId(stored)),
            None => None,
        }
    }
}

impl fmt::Debug for ArenaId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "arena {}.{}", self.index(), self.generation())
    }
}

/// The bit of [`Gid::local`] that says the id is of a snapshot's spill table, not of its
/// arena (made by a snapshot after it had a successor, see [`Memo::next`]). Public for a
/// crate that packs a local into fewer bits (span's `SyntaxContext`) and must keep this one.
pub const SPILL: u32 = 1 << 31;

/// An interned value's or an input's id: its arena and its index there.
///
/// Ids are handed out in the order values are first interned, which depends on thread timing:
/// never order output by one (rule deterministic-output). `Ord` is for maps that need an order
/// of their own, never for what a user sees.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Gid {
    arena: ArenaId,
    /// The local index plus one: never zero, so `Option<Gid>` (and every generated id struct
    /// around a `Gid`) is 8 bytes. A local is below `SPILL - 1` (`Slot::reserve_local`).
    local: NonZeroU32,
}

// What an id costs: 8 bytes (an arena's index and generation, and an index there), against the
// old engine's 4; `Option<Gid>` is 8 too, by the niche. Crate-local 4-byte ids inside a crate's
// own records are the end state (target decision 1), not part of this port.
const _: () = assert!(size_of::<Option<Gid>>() == 8 && size_of::<Gid>() == 8);

impl Gid {
    pub(crate) const fn new(arena: ArenaId, local: u32) -> Gid {
        match NonZeroU32::new(local.wrapping_add(1)) {
            Some(local) => Gid { arena, local },
            None => panic!("a local index of u32::MAX: past what an arena hands out"),
        }
    }

    /// The id of local index `local` in `arena`: the inverse of [`Gid::arena`] and
    /// [`Gid::local`], for a crate that stores a local in fewer bits and knows its arena from
    /// where it came (span's `SyntaxContext`, a call's `MacroCallId::arena`). `local` keeps its
    /// [`SPILL`] bit. Unchecked here: a read through the id checks its kind where it is looked
    /// up, and an id naming nothing panics there.
    ///
    /// # Panics
    ///
    /// Where `local` is `u32::MAX`, which no arena hands out.
    pub const fn in_arena(arena: ArenaId, local: u32) -> Gid {
        Gid::new(arena, local)
    }

    /// An id a test makes without a memo, to fill a structure that holds ids: local `local` of
    /// arena index 1, generation 0 (the first crate's arena of a session's first snapshot). It
    /// names nothing in any memo; a lookup of it panics or reads another value.
    ///
    /// # Panics
    ///
    /// Where `local` has the [`SPILL`] bit or is past what an arena hands out.
    pub const fn for_test(local: u32) -> Gid {
        assert!(local < SPILL - 1, "a test id's local is below `SPILL - 1`, as an arena's are");
        Gid::new(ArenaId::new(1, 0), local)
    }

    pub const fn arena(self) -> ArenaId {
        self.arena
    }

    pub const fn local(self) -> u32 {
        self.local.get() - 1
    }

    pub(crate) const fn is_spill(self) -> bool {
        self.local() & SPILL != 0
    }

    /// This id's bits, for an in-process table keyed by ids (a hash, a dense map of one
    /// database). Not a persisted id: the bits name an arena generation of this id space
    /// ([`ArenaId`]'s invariant) and mean nothing in another process or another id space. The
    /// store never files them; persist stable ids (fact ids) instead.
    pub const fn session_bits(self) -> u64 {
        ((self.arena.bits() as u64) << 32) | self.local.get() as u64
    }

    /// The id whose [`Gid::session_bits`] are `bits`, in this id space; `None` for bits no id
    /// has (a zero local, or an all-ones arena half).
    pub const fn from_session_bits(bits: u64) -> Option<Gid> {
        match NonZeroU32::new(bits as u32) {
            Some(local) => match ArenaId::from_bits((bits >> 32) as u32) {
                Some(arena) => Some(Gid { arena, local }),
                None => None,
            },
            None => None,
        }
    }
}

impl fmt::Debug for Gid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}:{}", self.arena.index(), self.arena.generation(), self.local())
    }
}

/// An interned or input struct: an id and nothing else (generated).
pub trait Id: Copy {
    fn gid(self) -> Gid;
    /// The struct of `gid`. Unchecked here: a read through it checks the kind once, where the
    /// fields are looked up.
    fn from_gid(gid: Gid) -> Self;
}

/// What a query does when it is asked again inside its own run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CycleKind {
    /// Panics naming the cycle's path: the query declared no recovery.
    Panic,
    /// `cycle_result`: the head and every participant get their own fallback.
    Result,
    /// `cycle_initial` (and `cycle_fn`): the head reruns from the initial value until its value
    /// equals the last one and no inner head moved, at most `MAX_ROUNDS` rounds.
    Fixpoint,
}

/// The most rounds a fixpoint head runs before it panics naming itself.
pub const MAX_ROUNDS: u32 = 200;

/// A query kind, an interned struct or an input: its name and what its cycles do.
///
/// One `static` per kind; entries compare kinds by the static's address, and hash by
/// [`Kind::id`], a hash of the name, so a table's layout is the same in every process.
pub struct Kind {
    name: &'static str,
    id: u64,
    cycle: CycleKind,
}

impl Kind {
    pub const fn new(name: &'static str, cycle: CycleKind) -> Kind {
        Kind { name, id: fnv1a(name.as_bytes()), cycle }
    }

    pub const fn name(&self) -> &'static str {
        self.name
    }

    pub const fn id(&self) -> u64 {
        self.id
    }

    pub const fn cycle(&self) -> CycleKind {
        self.cycle
    }
}

impl fmt::Debug for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)
    }
}

const fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut at = 0;
    while at < bytes.len() {
        hash ^= bytes[at] as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
        at += 1;
    }
    hash
}

/// What a `cycle_fn` is told of the cycle it recovers: the round its head is in.
#[derive(Clone, Copy, Debug)]
pub struct Cycle<'a> {
    head: &'static Kind,
    round: u32,
    _memo: std::marker::PhantomData<&'a Memo>,
}

impl Cycle<'_> {
    /// The head's query.
    pub fn head(&self) -> &'static str {
        self.head.name
    }

    /// The round that just ended, from 0.
    pub fn round(&self) -> u32 {
        self.round
    }
}

/// Why a fetch was given up: thrown through the task by `resume_unwind` (never the panic hook)
/// and caught where the task asks again ([`Cancelled::catch`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cancelled {
    /// Waiting for a key another task runs would have closed a cycle across tasks: this task
    /// gave way (the scheduler's `pump::give_way`), its runs unwound, and the task that holds
    /// the keys goes on and meets the cycle on its own stack.
    GaveWay,
}

impl Cancelled {
    pub fn throw(self) -> ! {
        std::panic::resume_unwind(std::boxed::Box::new_in(self, std::alloc::Global))
    }

    /// Runs `f`, a [`Cancelled`] it throws as `Err`; any other panic goes on unwinding.
    pub fn catch<T>(f: impl FnOnce() -> T) -> Result<T, Cancelled> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
            Ok(value) => Ok(value),
            Err(payload) => match payload.downcast::<Cancelled>() {
                Ok(cancelled) => Err(*cancelled),
                Err(other) => std::panic::resume_unwind(other),
            },
        }
    }
}

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Cancelled::GaveWay => {
                f.write_str("gave way: waiting would have closed a cycle across tasks")
            }
        }
    }
}

/// What generated code reaches the memo by; not for hand-written code.
#[doc(hidden)]
pub mod plumbing {
    pub use crate::attach::{Bounded, attached_interned};
    pub use crate::fetch::{Query, Recovery, fetch};
    pub use crate::home::{Home, Homes};
    pub use crate::image::{Imaged, Stable, StableKey};
    pub use crate::input::{
        Input, input, input_fact, new_input, set_singleton, singleton, try_singleton,
    };
    pub use crate::intern::{Interned, intern, interned, try_interned};
    pub use crate::testing::{Logged, Ran, ran_key, wrong_args};
    pub use crate::{Cycle, CycleKind, Database, Fact, Gid, Id, Inputs, Kind, Memo, Read};
    /// The tag of a query's run frame (`mem_tag = ..`, [`Query::TAG`]).
    pub use blaze_mem::Tag;

    /// The stored key of the boundary entry a [`crate::Read::Query`] row names, when the row's
    /// entry is of `kind` and in `memo` (the generated `key_of`).
    pub fn boundary_key<'m, K: 'static>(memo: &'m Memo, kind: &'static Kind, key: Gid) -> Option<&'m K> {
        memo.boundary_key::<K>(kind, key)
    }

    /// How a generated key prints an argument: its `Debug` when it has one, `_` otherwise
    /// (`(&&KeyText(&x)).write_key(out)`, the first impl that applies).
    pub struct KeyText<'a, T: ?Sized>(pub &'a T);

    pub trait KeyTextDebug {
        fn write_key(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result;
    }

    impl<T: std::fmt::Debug + ?Sized> KeyTextDebug for &KeyText<'_, T> {
        fn write_key(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(out, "{:?}", self.0)
        }
    }

    pub trait KeyTextOther {
        fn write_key(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result;
    }

    impl<T: ?Sized> KeyTextOther for KeyText<'_, T> {
        fn write_key(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            out.write_str("_")
        }
    }

    /// `returns(as_ref)`: what an `Option<T>` hands out for `&'db Option<T>`.
    pub trait ReturnsAsRef {
        type AsRef<'a>
        where
            Self: 'a;
        fn as_ref(&self) -> Self::AsRef<'_>;
    }

    impl<T> ReturnsAsRef for Option<T> {
        type AsRef<'a>
            = Option<&'a T>
        where
            T: 'a;
        fn as_ref(&self) -> Option<&T> {
            Option::as_ref(self)
        }
    }

    /// `returns(as_deref)`: what an `Option<T: Deref>` hands out for `&'db Option<T>`.
    pub trait ReturnsAsDeref {
        type AsDeref<'a>
        where
            Self: 'a;
        fn as_deref(&self) -> Self::AsDeref<'_>;
    }

    impl<T: std::ops::Deref> ReturnsAsDeref for Option<T> {
        type AsDeref<'a>
            = Option<&'a T::Target>
        where
            T: 'a;
        fn as_deref(&self) -> Option<&T::Target> {
            Option::as_deref(self)
        }
    }

    impl<T, E> ReturnsAsRef for Result<T, E> {
        type AsRef<'a>
            = Result<&'a T, &'a E>
        where
            T: 'a,
            E: 'a;
        fn as_ref(&self) -> Result<&T, &E> {
            Result::as_ref(self)
        }
    }

    /// The macros' one crossing between a type's database form (naming `'db`) and its stored
    /// form (`'static` in that position), so the expansion holds no `unsafe` token and a crate
    /// that forbids `unsafe_code` can use the attributes. Only the macros call it, with `A` and
    /// `B` the same type but for that one lifetime (`blaze-memo-macros` `lifetimes.rs`, `Both`);
    /// the sizes are checked here.
    #[doc(hidden)]
    #[inline(always)]
    pub fn relifetime_by_macro<A, B>(value: A) -> B {
        const {
            assert!(size_of::<A>() == size_of::<B>() && align_of::<A>() == align_of::<B>());
        };
        let value = std::mem::ManuallyDrop::new(value);
        // SAFETY: the invariant: `A` and `B` differ in the database lifetime alone (the macro
        // makes both from one written type), so their layouts and validity are the same. A
        // `'db` borrow in a value points into an arena of the snapshot (`keep_*`, an interned
        // field, another query's value), the snapshot frees an arena only after every arena
        // that borrowed from it (the carry closure of `Memo::next`, the declared closure
        // `mark_borrows` checks), and the memo hands a stored value out only under a borrow of
        // the snapshot no longer than the arena's life. `ManuallyDrop` keeps the value from
        // being dropped twice.
        unsafe { std::mem::transmute_copy::<A, B>(&value) }
    }

    /// [`relifetime_by_macro`] for a reference: the stored value read back as its database form,
    /// for no longer than the borrow it was read under.
    #[doc(hidden)]
    #[inline(always)]
    pub fn relifetime_ref_by_macro<A, B>(value: &A) -> &B {
        const {
            assert!(size_of::<A>() == size_of::<B>() && align_of::<A>() == align_of::<B>());
        };
        // SAFETY: as in `relifetime_by_macro`; the reference is no longer than `value`'s.
        unsafe { &*std::ptr::from_ref::<A>(value).cast::<B>() }
    }

    /// Asserts at monomorphization that `T` owns no memory to drop: an arena frees its values
    /// without running a destructor.
    pub const fn assert_no_drop<T>() {
        assert!(
            !std::mem::needs_drop::<T>(),
            "a memo value, interned field or kept value owns no memory to drop (it is freed with \
             its arena): hold `&'db [T]` from `blaze_memo::keep_slice` or ids, never Vec, Box, \
             Arc or String; a real tree is kept with `blaze_memo::keep_owned` (counted debt)"
        );
    }
}
