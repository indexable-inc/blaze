//! The session memo the front end's queries run on (docs/v2/plan/design/target.md, design 4):
//! a plain table per crate arena, with no dependency tracking.
//!
//! A [`Memo`] is one snapshot. Its inputs never change while it lives; there are no revisions,
//! no edges, no durabilities, no summaries and no seals, and nothing stops a reader. A change
//! makes the next snapshot ([`Memo::next`]): the arenas of changed crates are dropped
//! wholesale and every other arena is carried, entries and all.
//!
//! - **Arenas.** An [`ArenaId`] is a crate's (the driver gives each crate a surface arena and a
//!   bodies arena, plus the session arena [`ArenaId::SESSION`] for singletons). Every entry,
//!   trie node, read list and interned value of an arena is bump-allocated in its
//!   [`blaze_mem::Arena`]; dropping the arena frees it all at once and runs no destructor of an
//!   entry. The rule that makes this sound: **a value owns no memory to drop**
//!   (`!needs_drop`), checked at compile time for each query, interned struct and
//!   [`keep`]ed value (`const { assert!(!needs_drop::<V>()) }`, an error after
//!   monomorphization naming the instantiation). A value that needs a slice keeps one in its
//!   run's arena ([`keep_slice`], [`keep_iter`], [`keep_str`]) and holds `&'db [T]`.
//! - **A key's home.** Every key has one arena, its home ([`Home`]): the greatest [`ArenaId`]
//!   among the ids it holds (the session arena when it holds none). The driver numbers arenas
//!   so that a crate's comes after its dependencies', so the home of a key that mixes crates is
//!   the most downstream one. The home is a function of the key alone, so one key has one
//!   entry in a snapshot.
//! - **The carry invariant.** *An arena is carried into the next snapshot only if no input any
//!   of its entries could have read has changed.* What an entry could have read is bounded by
//!   what its run touched: every fetch, intern, lookup and input read of a run homed at `A`
//!   that reaches arena `B != A` sets bit `B` in `A`'s borrow set (written once per pair, read
//!   on the hit path but never written there). [`Memo::next`] drops the arenas the driver names
//!   (decided from record keys and digests: a crate whose inputs' digests moved) and then,
//!   to a fixpoint, every arena that borrowed from a dropped one. A dropped arena's ids are
//!   dead: its arena id's generation moves, and a dead id asked of a later snapshot is refused
//!   by name. The same closure is what keeps references sound: a value can hold `&'db`
//!   borrows only into its own arena and arenas it borrowed from, so an arena never outlives
//!   what its values point into.
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
//! - **Cycles** keep blaze-query's observable results: `cycle_result` gives the head and every
//!   participant its own fallback; `cycle_initial` with an optional `cycle_fn` iterates the
//!   head to a fixpoint (`MAX_ROUNDS`, then a panic naming it); a query with neither panics
//!   naming the cycle's path.
//! - **The read recorder** ([`record`]) is off unless the snapshot was made with
//!   [`Reads::Kept`]: then each entry keeps the sorted, deduplicated list of [`Fact`]s its run
//!   read, directly or through anything it called, and a hit replays the entry's list into
//!   the asker, so a list is exactly what the result depends on whether it was computed or
//!   found.
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
mod input;
mod intern;
mod keep;
mod memo;
mod record;
mod resident;
mod task;
mod trie;

use std::fmt;

pub use blaze_keyed::record::Fact;
pub use blaze_memo_macros::{Home, input, interned, tracked};

pub use attach::{attach, with_attached};
pub use counts::{Counts, take_counts};
pub use fetch::{Query, Recovery, fetch};
pub use flight::{Alone, Flight, FlightKey, Flown, Hold, Kept, Scheduled};
pub use home::{Home, Homes};
pub use input::{Input, input, new_input, set_singleton, singleton, try_singleton};
pub use intern::{Interned, intern, interned};
pub use keep::{keep, keep_iter, keep_slice, keep_str};
pub use memo::{Builder, Config, Inputs, Memo, Next, Plant, Reads};
pub use record::{RecorderOff, read, record, record_in};
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

/// The most arena indices a session has: the index's bits.
pub const MAX_ARENAS: u32 = 1 << (32 - GENERATION_BITS);

/// An arena of a session: its index (the driver's, a crate's surface or bodies, ordered so a
/// crate comes after its dependencies) and the generation of its making.
///
/// Ordered by index first, so the [`Home`] of a key is the most downstream arena it names.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArenaId(u32);

impl ArenaId {
    /// The session's own arena: singletons, and keys that name no arena.
    pub const SESSION: ArenaId = ArenaId(0);

    pub(crate) const fn new(index: u32, generation: u32) -> ArenaId {
        ArenaId((index << GENERATION_BITS) | (generation & GENERATION_MASK))
    }

    pub const fn index(self) -> u32 {
        self.0 >> GENERATION_BITS
    }

    pub const fn generation(self) -> u32 {
        self.0 & GENERATION_MASK
    }

    pub(crate) const fn bits(self) -> u32 {
        self.0
    }
}

impl fmt::Debug for ArenaId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "arena {}.{}", self.index(), self.generation())
    }
}

/// The bit of [`Gid::local`] that says the id is of a snapshot's spill table, not of its
/// arena (made by a snapshot after it had a successor, see [`Memo::next`]).
pub(crate) const SPILL: u32 = 1 << 31;

/// An interned value's or an input's id: its arena and its index there.
///
/// Ids are handed out in the order values are first interned, which depends on thread timing:
/// never order output by one (rule deterministic-output). `Ord` is for maps that need an order
/// of their own, never for what a user sees.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Gid {
    arena: ArenaId,
    local: u32,
}

impl Gid {
    pub(crate) const fn new(arena: ArenaId, local: u32) -> Gid {
        Gid { arena, local }
    }

    pub const fn arena(self) -> ArenaId {
        self.arena
    }

    pub const fn local(self) -> u32 {
        self.local
    }

    pub(crate) const fn is_spill(self) -> bool {
        self.local & SPILL != 0
    }
}

impl fmt::Debug for Gid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}:{}", self.arena.index(), self.arena.generation(), self.local)
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
    pub use crate::attach::{attached_input, attached_interned};
    pub use crate::fetch::{Query, Recovery, fetch};
    pub use crate::home::{Home, Homes};
    pub use crate::input::{Input, input, new_input, set_singleton, singleton, try_singleton};
    pub use crate::intern::{Interned, intern, interned};
    pub use crate::{Cycle, CycleKind, Database, Fact, Gid, Id, Inputs, Kind, Memo};

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

    /// Asserts at monomorphization that `T` owns no memory to drop: an arena frees its values
    /// without running a destructor.
    pub const fn assert_no_drop<T>() {
        assert!(
            !std::mem::needs_drop::<T>(),
            "a memo value, interned field or kept value owns no memory to drop (it is freed with \
             its arena): hold `&'db [T]` from `blaze_memo::keep_slice` or ids, never Vec, Box, \
             Arc or String"
        );
    }
}
