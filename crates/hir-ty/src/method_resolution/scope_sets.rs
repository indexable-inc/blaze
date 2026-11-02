//! The traits in scope of a method probe as IDs: each scope's set of traits, sorted and
//! deduplicated, interned once to a number, and for each (scope, method name) the set's traits
//! with a value item of that name, interned the same way.
//!
//! A probe carries one [`TraitsInScope`] (a module and a trait, or a set's number) and asks it for the [`ScopeItems`] of its
//! name: the extension candidates it assembles, and the part of the pick cache's key that stands
//! for the scope. A body depends on the traits of its scope only through
//! [`module_items_named`], which backdates when a trait without an item of the name comes into or
//! leaves scope.

use std::num::NonZeroU32;

use base_db::Db;
use blaze_mem::Frame;
use hir_def::{
    GenericDefId, ModuleId, TraitId,
    resolver::{HasResolver, Resolver},
};
use hir_expand::name::Name;
use smallvec::SmallVec;

use super::{CandidateId, TraitValueItems, trait_value_item};
use crate::{
    db::HirDatabase,
    next_solver::interner::table::{Erase, Lists},
};

// SAFETY: `TraitId` has no lifetime: `Static` is `TraitId` itself.
unsafe impl Erase for TraitId {
    type Static = TraitId;
}

// SAFETY: `ScopeItem` has no lifetime: `Static` is `ScopeItem` itself.
unsafe impl Erase for ScopeItem {
    type Static = ScopeItem;
}

/// Every [`TraitSet`]. Rows are the same for equal sets on every thread for the life of the
/// process; a `TraitId` names a trait of one database, so a row means a set of that database's
/// traits only to a key that also names the database (the pick cache's does).
static TRAIT_SETS: Lists<TraitId> = Lists::new("TraitSet");

/// Every [`ScopeItems`].
static SCOPE_ITEMS: Lists<ScopeItem> = Lists::new("ScopeItems");

/// A set of traits, sorted by ID and without duplicates, as one number.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TraitSet(NonZeroU32);

impl TraitSet {
    /// The set of `traits`, which this sorts and deduplicates in place.
    pub fn new(traits: &mut Vec<TraitId>) -> TraitSet {
        traits.sort_unstable();
        traits.dedup();
        TraitSet(TRAIT_SETS.intern::<TraitId>(traits.as_slice()))
    }

    /// The set of the traits a caller's iterator gives, in any order and with repeats.
    pub fn collect(traits: impl IntoIterator<Item = TraitId>) -> TraitSet {
        // alloc: a caller's own set (an IDE query or an error path), once per probe.
        let mut traits: Vec<TraitId> = traits.into_iter().collect();
        TraitSet::new(&mut traits)
    }

    /// The traits, sorted by ID.
    #[inline]
    pub fn traits(self) -> &'static [TraitId] {
        TRAIT_SETS.get::<TraitId>(self.0)
    }

    /// The set's number, dense from 1.
    #[inline]
    pub fn id(self) -> u32 {
        self.0.get()
    }
}

/// A trait in scope and its value item of the probe's name: what
/// [`trait_value_item`] answers for the two.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ScopeItem {
    pub trait_: TraitId,
    pub item: CandidateId,
}

/// The traits of a scope that have a value item of one name, sorted by trait, as one number.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ScopeItems(NonZeroU32);

impl ScopeItems {
    fn new(items: &[ScopeItem]) -> ScopeItems {
        debug_assert!(items.windows(2).all(|it| it[0].trait_ < it[1].trait_));
        ScopeItems(SCOPE_ITEMS.intern::<ScopeItem>(items))
    }

    /// The items of `traits` (sorted, deduplicated) named `name`.
    fn of_traits(db: &Db, traits: &[TraitId], name: Name, frame: &Frame) -> ScopeItems {
        let mut values = TraitValueItems::new_in(frame);
        let mut items: SmallVec<[ScopeItem; 8]> = SmallVec::new();
        for &trait_ in traits {
            if let Some(item) = values.get(db, trait_, name) {
                // alloc: only past eight traits with an item of one name, on a miss of the query
                // or for a caller's own set.
                items.push(ScopeItem { trait_, item });
            }
        }
        ScopeItems::new(&items)
    }

    /// These items, and `trait_`'s of `name` if it has one and is not among them.
    fn with_trait(self, db: &Db, trait_: TraitId, name: Name) -> ScopeItems {
        let items = self.items();
        let Err(at) = items.binary_search_by_key(&trait_, |it| it.trait_) else {
            return self;
        };
        let Some(item) = trait_value_item(db, trait_, name) else {
            return self;
        };
        let mut merged: SmallVec<[ScopeItem; 8]> = SmallVec::new();
        // alloc: only past eight traits with an item of one name.
        merged.extend_from_slice(&items[..at]);
        merged.push(ScopeItem { trait_, item });
        merged.extend_from_slice(&items[at..]);
        ScopeItems::new(&merged)
    }

    /// The items, sorted by trait.
    #[inline]
    pub fn items(self) -> &'static [ScopeItem] {
        SCOPE_ITEMS.get::<ScopeItem>(self.0)
    }

    /// The list's number, dense from 1.
    #[inline]
    pub fn id(self) -> u32 {
        self.0.get()
    }
}

/// The traits a probe looks in.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TraitsInScope {
    /// At a position in a body: the traits in scope in `module` (the innermost block or module,
    /// with the blocks around it and the prelude) and the trait the body's impl implements,
    /// rustc's `current_trait_ref`.
    Scope { module: ModuleId, impl_trait: Option<TraitId> },
    /// A caller's own set.
    Set(TraitSet),
}

impl TraitsInScope {
    /// The traits in scope where `resolver` is.
    pub fn of_resolver(db: &Db, resolver: &Resolver<'_>) -> TraitsInScope {
        let impl_trait = resolver
            .all_generic_params()
            .find_map(|(_, def)| match def {
                GenericDefId::ImplId(it) => Some(it),
                _ => None,
            })
            .and_then(|impl_| db.impl_trait(impl_))
            .map(|trait_ref| trait_ref.skip_binder().def_id.0);
        TraitsInScope::Scope { module: resolver.module(), impl_trait }
    }

    /// The traits of `name` among these: the extension candidates of a probe for it.
    pub fn named(self, db: &Db, name: Name, frame: &Frame) -> ScopeItems {
        match self {
            TraitsInScope::Scope { module, impl_trait } => {
                let items = module_items_named(db, module, name);
                match impl_trait {
                    Some(trait_) => items.with_trait(db, trait_, name),
                    None => items,
                }
            }
            TraitsInScope::Set(set) => ScopeItems::of_traits(db, set.traits(), name, frame),
        }
    }

    /// Every trait, sorted by ID, for a probe that looks at all names.
    pub fn all(self, db: &Db) -> impl Iterator<Item = TraitId> {
        let (set, extra) = match self {
            TraitsInScope::Scope { module, impl_trait } => {
                let set = module_trait_set(db, module).traits();
                (set, impl_trait.filter(|it| set.binary_search(it).is_err()))
            }
            TraitsInScope::Set(set) => (set.traits(), None),
        };
        set.iter().copied().chain(extra)
    }
}

/// The traits in scope in `module`: its blocks', its module's and the prelude's, as
/// `Resolver::for_each_trait_in_scope` walks them.
#[blaze_query::tracked(returns(copy), mem_tag = Infer)]
pub fn module_trait_set(db: &Db, module: ModuleId) -> TraitSet {
    // alloc: once per module and revision.
    let mut traits = Vec::new();
    module.resolver(db).for_each_trait_in_scope(db, |it| traits.push(it));
    TraitSet::new(&mut traits)
}

/// The traits in scope in `module` with a value item named `name`, and those items. What a body
/// reads of its scope's traits, so it is checked again only when this changes.
#[blaze_query::tracked(returns(copy), mem_tag = Infer)]
pub fn module_items_named(db: &Db, module: ModuleId, name: Name) -> ScopeItems {
    blaze_mem::frame::current(|frame| {
        ScopeItems::of_traits(db, module_trait_set(db, module).traits(), name, frame)
    })
}
