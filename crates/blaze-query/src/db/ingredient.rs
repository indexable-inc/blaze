//! Ingredients: each tracked function, input, interned and tracked struct of the binary,
//! numbered on first use, process-wide.

use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use super::{
    Id, Storage,
    storage::{Relocate, Restamp},
};
use crate::sync::Mutex;

/// Ingredients a binary may have: the front end has ~600. Node keys give an ingredient 12 bits.
pub(crate) const MAX_INGREDIENTS: usize = 2048;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Function,
    Input,
    Interned,
    Tracked,
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
    /// What its table holds ([`Storage::memory_usage`]).
    pub(crate) memory: fn(storage: &Storage, ingredient: u32) -> super::MemoryUsage,
    /// Brings `id`'s row up to the storage's view (verifying it, running it if it changed),
    /// for a reader that verifies an edge on it. `db` is the database that owns `storage`, as
    /// the ingredient's own database type.
    pub(crate) verify:
        Option<unsafe fn(db: *const (), storage: &Storage, id: Id) -> super::function::Verified>,
    /// Empties a function's provisional row (a cycle's round or its unwinding): only the
    /// thread of the row's cycle calls it.
    pub(crate) reset: Option<unsafe fn(storage: &Storage, ingredient: u32, id: Id)>,
    /// At a view boundary, with no reader: evicts past the function's `lru` cap.
    pub(crate) sweep: Option<fn(storage: &mut Storage, ingredient: u32)>,
    /// Moves each of its rows' edge spans into the compacted array.
    pub(crate) relocate: Option<fn(storage: &mut Storage, ingredient: u32, moved: &mut Relocate)>,
    /// Passes each view stamp its rows hold through `restamp`, storing what it gives: the
    /// renumbering at the view limit, under `&mut`.
    pub(crate) restamp: Option<fn(storage: &Storage, ingredient: u32, restamp: &mut Restamp<'_>)>,
    /// Drops its rows and its table.
    pub(crate) drop: fn(storage: &mut Storage, ingredient: u32),
}

/// An ingredient's static: its number, given on first use, and its [`Vtable`].
pub struct Ingredient {
    index: AtomicU32,
    pub vtable: &'static Vtable,
}

static REGISTERED: Mutex<u32> = Mutex::new(0);
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
