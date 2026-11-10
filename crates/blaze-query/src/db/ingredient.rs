//! Ingredients: each tracked function, input, interned and tracked struct of the binary,
//! numbered on first use, process-wide.

use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use blaze_mem::Tag;

use super::{
    Id, Storage,
    budget::Held,
    storage::{Relocate, Restamp},
};
use crate::table::ShortLock;

/// Ingredients a binary may have: the front end has ~600. Node keys give an ingredient 12 bits.
pub(crate) const MAX_INGREDIENTS: usize = 2048;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Function,
    Input,
    Interned,
    Tracked,
}

/// What a view boundary asks of a function kept by its values' bytes ([`super::budget`]), with
/// no reader.
#[derive(Clone, Copy)]
pub(crate) struct Weighed {
    /// The tag its values count to: the function's `mem_tag`.
    pub(crate) tag: Tag,
    /// What its values weigh, as counted when each was made and let go; also folds the keys
    /// that took a value since into its index of values held.
    pub(crate) weight: fn(storage: &Storage, ingredient: u32) -> u64,
    /// Pushes each value it holds, weighed now, onto `out`, makes that sum its weight, and
    /// returns it.
    pub(crate) held: fn(storage: &Storage, ingredient: u32, out: &mut Vec<Held>) -> u64,
    /// Lets go of the values of `gone` (its own, each once); returns how many held one and what
    /// they weighed.
    pub(crate) evict: fn(storage: &Storage, ingredient: u32, gone: &[Held]) -> (usize, u64),
}

/// What the storage asks of a function whose values are let go only when asked (`lru = 0`),
/// by the tag they count to ([`Storage::evict_tag`]): a caller lets go of a whole tag's values
/// at once (every MIR, as a build ends), never of a list of functions it would have to keep in
/// step with the queries that exist.
#[derive(Clone, Copy)]
pub(crate) struct Explicit {
    /// The tag its values count to: the function's `mem_tag`.
    pub(crate) tag: Tag,
    /// How many of its rows hold a value.
    pub(crate) values: fn(storage: &Storage, ingredient: u32) -> usize,
    /// Lets go of every value it holds, keeping each row's stamp, edges and hash; returns how
    /// many held one.
    pub(crate) evict: fn(storage: &mut Storage, ingredient: u32) -> usize,
}

/// What the storage does with an ingredient's table without knowing its types: the cold
/// paths (verifying a dependency by its node, emptying a cycle's row, a view's sweep,
/// compaction, drop). Made by [`Vtable::function`], [`Vtable::input`], [`Vtable::interned`]
/// and [`Vtable::tracked`]; the fields are the engine's.
pub struct Vtable {
    pub name: &'static str,
    /// `file!()` where it is defined.
    pub file: &'static str,
    pub kind: Kind,
    /// The name of each field a read of it names by number ([`super::ReadKind::InputField`],
    /// [`super::ReadKind::TrackedField`]), in that order: an input's fields, a tracked
    /// struct's `#[tracked]` ones; none for a function or an interned struct.
    pub fields: &'static [&'static str],
    /// A function's row size, so the verify walk reads its headers untyped.
    pub(crate) row_size: usize,
    /// An interned row's fields' IDs ([`Storage::ids_of`]).
    pub(crate) ids_of: Option<fn(storage: &Storage, ingredient: u32, id: Id, out: &mut Vec<Id>)>,
    /// Whether an interned row's value was interned ([`Storage::made`]).
    pub(crate) made: Option<fn(storage: &Storage, ingredient: u32, id: Id) -> bool>,
    /// What its table holds ([`Storage::memory_usage`]), from counters: no row is read.
    pub(crate) memory: fn(storage: &Storage, ingredient: u32) -> super::MemoryUsage,
    /// A function's rows in use, a walk of every row made ([`Storage::memory_usage`]); every
    /// row of another ingredient is in use, as its `memory` says.
    pub(crate) used: Option<fn(storage: &Storage, ingredient: u32) -> u64>,
    /// Brings `id`'s row up to the storage's view (verifying it, running it if it changed),
    /// for a reader that verifies an edge on it. `db` is the database that owns `storage`, as
    /// the ingredient's own database type.
    pub(crate) verify:
        Option<unsafe fn(db: *const (), storage: &Storage, id: Id) -> super::function::Verified>,
    /// Empties a function's provisional row (a cycle's round or its unwinding): only the
    /// thread of the row's cycle calls it.
    pub(crate) reset: Option<unsafe fn(storage: &Storage, ingredient: u32, id: Id)>,
    /// A function whose values are kept by their bytes (`lru`): what the storage's view
    /// boundary asks of it ([`super::budget`]).
    pub(crate) weighed: Option<Weighed>,
    /// A function whose values are let go only when asked (`lru = 0`): what a caller letting go
    /// of a tag's values asks of it ([`Storage::evict_tag`]).
    pub(crate) explicit: Option<Explicit>,
    /// A function whose values may be let go (`lru` or `lru = 0`): lets go of each value last
    /// verified in a view before `before`, keeping its row's stamp, edges and hash; returns
    /// how many went and what they weighed ([`Storage::end_epoch`]).
    pub(crate) unread: Option<fn(storage: &Storage, ingredient: u32, before: u32) -> (usize, u64)>,
    /// Moves each of its rows' edge spans into the compacted array.
    pub(crate) relocate: Option<fn(storage: &mut Storage, ingredient: u32, moved: &mut Relocate)>,
    /// Passes each view stamp its rows hold through `restamp`, storing what it gives: the
    /// renumbering at the view limit, under `&mut`.
    pub(crate) restamp: Option<fn(storage: &Storage, ingredient: u32, restamp: &mut Restamp<'_>)>,
    /// Writes a function's key `id` as its type shows it, through the attached database, for
    /// an error that names a row without knowing its function's type ([`super::Runaway`]).
    pub(crate) write_key: Option<fn(storage: &Storage, id: Id, out: &mut String)>,
    /// Drops its rows and its table.
    pub(crate) drop: fn(storage: &mut Storage, ingredient: u32),
}

/// An ingredient's static: its number, given on first use, and its [`Vtable`].
pub struct Ingredient {
    index: AtomicU32,
    pub vtable: &'static Vtable,
}

static REGISTERED: ShortLock<u32> = ShortLock::new("query.ingredient.registered", 0);
static VTABLES: [AtomicPtr<Vtable>; MAX_INGREDIENTS] =
    [const { AtomicPtr::new(std::ptr::null_mut()) }; MAX_INGREDIENTS];

impl Ingredient {
    pub const fn new(vtable: &'static Vtable) -> Ingredient {
        Ingredient { index: AtomicU32::new(0), vtable }
    }

    /// Its number: one relaxed load once it has one.
    #[inline]
    pub fn index(&'static self) -> u32 {
        match self.index.load(Ordering::Relaxed) {
            0 => self.register(),
            index => index,
        }
    }

    #[cold]
    #[inline(never)]
    fn register(&'static self) -> u32 {
        let mut count = REGISTERED.lock();
        // Relaxed: under the lock, which ordered every store of it before us.
        if let index @ 1.. = self.index.load(Ordering::Relaxed) {
            return index;
        }
        *count += 1;
        let index = *count;
        assert!(
            (index as usize) < MAX_INGREDIENTS,
            "more than {MAX_INGREDIENTS} ingredients (at {})",
            self.vtable.name
        );
        VTABLES[index as usize]
            .store(std::ptr::from_ref(self.vtable).cast_mut(), Ordering::Release);
        // Relaxed: a reader that sees the number reads the vtable through `vtable`, which
        // the release above published.
        self.index.store(index, Ordering::Relaxed);
        index
    }
}

/// The vtable of ingredient `index`, once registered.
#[inline]
pub(crate) fn vtable(index: u32) -> Option<&'static Vtable> {
    let at = VTABLES.get(index as usize)?.load(Ordering::Acquire);
    // SAFETY: a published pointer came from a `&'static Vtable`.
    unsafe { at.as_ref() }
}

/// How many ingredients are registered: every number is below this plus one.
pub(crate) fn registered() -> u32 {
    *REGISTERED.lock()
}

/// The name of ingredient `index`, as its macro gave it.
pub(crate) fn name(index: u32) -> &'static str {
    vtable(index).map_or("<unregistered>", |it| it.name)
}

/// The kind of ingredient `index`.
pub(crate) fn kind(index: u32) -> Option<Kind> {
    vtable(index).map(|it| it.kind)
}
