//! Each `macro_rules!` and `macro` definition lexed and compiled (blaze-expand's `prepare`) once
//! per build, not at each call: [`Prepares`] holds each by the key of the item that defines it,
//! found before any work. The one home for item-position expansions (resolve.rs) and for the
//! calls in function bodies (blaze-lower's `scoped`).

use std::sync::Mutex;

use blaze_expand::{Call, ExpandCx, ExpandError, Expanded, MacroDefinition, Prepared, expand, expand_prepared, prepare};
use blaze_hash::HashMap;
use blaze_id::ItemKey;
use blaze_own::{Alloc, Box};

/// Each declarative definition a call named, prepared once, by the key of its defining item:
/// its definition does not depend on the call's shape or site, so the key covers every input
/// of the value (rule 21). Bounded by the distinct macros called. The definitions and programs
/// live in `alloc` for `'d` (a build's or a crate's frame). Shared by threads: the lock is held
/// for a lookup or an insert, never across a prepare; two threads preparing one definition at
/// once build equal values and the first kept is the one every call uses.
pub struct Prepares<'d> {
    alloc: Alloc<'d>,
    by_item: Mutex<HashMap<ItemKey, Kept<'d>, Alloc<'d>>>,
}

/// One definition as an expansion takes it: compiled once, or as written (its rules do not
/// compile, so each call reports the error as an unprepared expansion does; or a builtin or a
/// proc macro, which has nothing to compile).
#[derive(Clone, Copy)]
pub enum Kept<'d> {
    Prepared(&'d Prepared<'d>),
    Written(&'d MacroDefinition<'d>),
}

impl<'d> Kept<'d> {
    /// The definition, compiled or not.
    pub fn def(self) -> &'d MacroDefinition<'d> {
        match self {
            Kept::Prepared(prepared) => prepared.def(),
            Kept::Written(def) => def,
        }
    }

    /// `call` of this definition expanded: [`expand_prepared`] when it is compiled, else
    /// [`expand`] (which prepares it for this call only).
    pub fn expand<'a>(self, call: &Call<'_>, cx: &impl ExpandCx, alloc: Alloc<'a>) -> Result<Expanded<'a>, ExpandError> {
        match self {
            Kept::Prepared(prepared) => expand_prepared(prepared, call, cx, alloc),
            Kept::Written(def) => expand(def, call, cx, alloc),
        }
    }
}

impl<'d> Prepares<'d> {
    /// An empty set, itself kept in `alloc` beside what it holds (it lives as long as they do,
    /// and every user borrows it for that long).
    pub fn new_in(alloc: Alloc<'d>) -> &'d Prepares<'d> {
        Box::leak(Box::new_in(Prepares { alloc, by_item: Mutex::new(HashMap::new_in(alloc)) }, alloc))
    }

    /// The allocator the definitions are kept in.
    pub fn alloc(&self) -> Alloc<'d> {
        self.alloc
    }

    /// How many distinct definitions were prepared.
    pub fn len(&self) -> usize {
        self.held().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The definition kept under `item`, if one is.
    pub fn get(&self, item: ItemKey) -> Option<Kept<'d>> {
        self.held().get(&item).copied()
    }

    /// `def` prepared and kept under `item`; the one kept first when another thread kept it
    /// meanwhile.
    pub fn keep(&self, item: ItemKey, def: MacroDefinition<'d>) -> Kept<'d> {
        let def: &'d MacroDefinition<'d> = Box::leak(Box::new_in(def, self.alloc));
        let kept = match prepare(def) {
            Ok(prepared) => Kept::Prepared(Box::leak(Box::new_in(prepared, self.alloc))),
            Err(_) => Kept::Written(def),
        };
        *self.held().entry(item).or_insert(kept)
    }

    fn held(&self) -> std::sync::MutexGuard<'_, HashMap<ItemKey, Kept<'d>, Alloc<'d>>> {
        self.by_item.lock().unwrap_or_else(|poisoned| panic!("the prepared macro definitions' lock was poisoned by a panic while held: {poisoned}"))
    }
}
