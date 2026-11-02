//! The shape of a vtable, which has to agree with rustc's `vtable_entries` slot for slot:
//! objects cross between rustc-compiled code and ours in both directions.

#![forbid(unsafe_code)]

use blaze_hash::HashSet;
use hir_def::{AssocItemId, FunctionId, TraitId};
use rustc_type_ir::{
    Binder, Interner, RegionKind, TypeFoldable, TypeFolder, TypeSuperFoldable,
    inherent::{Clause as _, IntoKind as _},
};

use hir_ty::{
    dyn_compatibility::generics_require_sized_self,
    next_solver::{DbInterner, Region, RegionExt, TraitRef},
};

/// `drop_in_place`, size and align come before the slots.
pub(super) const HEADER: usize = 3;

#[derive(Debug, Clone, Copy)]
pub(super) enum Slot<'db> {
    Method(FunctionId, TraitRef<'db>),
    /// The vtable of a supertrait, for upcasting to one that is not a prefix of this vtable.
    Vptr(TraitRef<'db>),
}

pub(super) struct VtableLayout<'db> {
    pub(super) slots: Vec<Slot<'db>>,
    /// The supertraits whose vtable is a prefix of this one.
    prefixes: Vec<TraitRef<'db>>,
}

impl<'db> VtableLayout<'db> {
    /// The vtable of `trait_ref`. `plain` gives the one form of a trait among those the
    /// program can write it in, which is what tells two supertraits apart.
    pub(super) fn of(
        interner: DbInterner<'db>,
        trait_ref: TraitRef<'db>,
        plain: &impl Fn(TraitRef<'db>) -> TraitRef<'db>,
    ) -> Self {
        let mut walk = Walk {
            interner,
            plain,
            visited: HashSet::from_iter([trait_ref]),
            // alloc: the layout's own lists, returned in it
            slots: Vec::new(),
            // alloc: the layout's own lists, returned in it
            prefixes: Vec::new(),
            emit_vptr: false,
        };
        walk.visit(trait_ref, false);
        VtableLayout { slots: walk.slots, prefixes: walk.prefixes }
    }

    /// The slot index, header included, of a trait method.
    pub(super) fn method(&self, func: FunctionId) -> Option<usize> {
        let index = self.slots.iter().position(|slot| match slot {
            Slot::Method(it, _) => *it == func,
            Slot::Vptr(_) => false,
        })?;
        Some(HEADER + index)
    }

    /// How a pointer to this vtable becomes one to the vtable of supertrait `target`.
    pub(super) fn upcast(&self, target: TraitRef<'db>) -> Option<Upcast> {
        let slot = self.slots.iter().position(|slot| match slot {
            Slot::Vptr(it) => *it == target,
            Slot::Method(..) => false,
        });
        match slot {
            Some(index) => Some(Upcast::Load(HEADER + index)),
            None if self.prefixes.contains(&target) => Some(Upcast::Same),
            None => None,
        }
    }
}

pub(super) enum Upcast {
    /// This vtable starts with the supertrait's.
    Same,
    /// The supertrait's vtable is behind the pointer in this slot.
    Load(usize),
}

struct Walk<'a, 'db, P> {
    interner: DbInterner<'db>,
    plain: &'a P,
    visited: HashSet<TraitRef<'db>>,
    slots: Vec<Slot<'db>>,
    prefixes: Vec<TraitRef<'db>>,
    /// Set once a trait with methods has been laid out: every later supertrait is no longer
    /// at the start of the vtable and needs a pointer to its own.
    emit_vptr: bool,
}

impl<'db, P: Fn(TraitRef<'db>) -> TraitRef<'db>> Walk<'_, 'db, P> {
    /// Supertraits first, depth first in declaration order, then the trait's own methods.
    fn visit(&mut self, trait_ref: TraitRef<'db>, vptr: bool) {
        let interner = self.interner;
        let supertraits: Vec<_> = interner
            .explicit_super_clauses_of(trait_ref.def_id)
            .iter_instantiated(interner, trait_ref.args.as_slice())
            .filter_map(|clause| clause.skip_norm_wip().0.as_trait_clause())
            .map(|pred| interner.instantiate_bound_regions_with_erased(pred).trait_ref)
            .map(self.plain)
            // alloc: all made plain (normalized by the caller's inference) before any is visited
            .collect();
        for supertrait in supertraits {
            if self.visited.insert(supertrait) {
                self.visit(supertrait, self.emit_vptr);
            }
        }
        let mut methods = own_methods(interner, trait_ref.def_id.0).peekable();
        let has_methods = methods.peek().is_some();
        self.emit_vptr |= has_methods;
        self.slots.extend(methods.map(|func| Slot::Method(func, trait_ref)));
        if !vptr {
            self.prefixes.push(trait_ref);
        } else if has_methods {
            // rustc gives a supertrait without methods no slot.
            self.slots.push(Slot::Vptr(trait_ref));
        }
    }
}

/// The methods of `trait_` that can be called on a trait object, in declaration order.
fn own_methods<'db>(
    interner: DbInterner<'db>,
    trait_: TraitId,
) -> impl Iterator<Item = FunctionId> + 'db {
    let db = interner.db;
    trait_
        .trait_items(db)
        .items
        .iter()
        .filter_map(|&(_, item)| match item {
            AssocItemId::FunctionId(func) => Some(func),
            _ => None,
        })
        .filter(move |&func| !generics_require_sized_self(db, func.into()))
}

/// `trait_ref` with every lifetime erased and the lifetimes of a `for<'a>` unnamed, as rustc
/// has a trait when it lays out a vtable.
pub(super) fn without_lifetimes<'db>(
    interner: DbInterner<'db>,
    trait_ref: TraitRef<'db>,
) -> TraitRef<'db> {
    trait_ref.fold_with(&mut LifetimeEraser { interner })
}

struct LifetimeEraser<'db> {
    interner: DbInterner<'db>,
}

impl<'db> TypeFolder<DbInterner<'db>> for LifetimeEraser<'db> {
    fn cx(&self) -> DbInterner<'db> {
        self.interner
    }

    fn fold_binder<T: TypeFoldable<DbInterner<'db>>>(
        &mut self,
        binder: Binder<DbInterner<'db>, T>,
    ) -> Binder<DbInterner<'db>, T> {
        self.interner.anonymize_bound_vars(binder).super_fold_with(self)
    }

    fn fold_region(&mut self, region: Region<'db>) -> Region<'db> {
        match region.kind() {
            RegionKind::ReBound(..) => region,
            _ => Region::new_erased(self.interner),
        }
    }
}
