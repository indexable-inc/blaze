//! Handling of unstable features.
//!
//! We define two kinds of handling: we have a map of all unstable features for a crate
//! as `Symbol`s. This is mostly for external consumers.
//!
//! For analysis, we store them as a struct of bools, for fast access.

use std::fmt;

use base_db::{Crate, Db};
use blaze_hash::HashSet;
use intern::{Symbol, sym};
use std::alloc::Global;

impl fmt::Debug for UnstableFeatures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(&self.all).finish()
    }
}

impl PartialEq for UnstableFeatures {
    fn eq(&self, other: &Self) -> bool {
        self.all == other.all
    }
}

impl Eq for UnstableFeatures {}

impl UnstableFeatures {
    #[inline]
    pub fn is_enabled(&self, feature: &Symbol) -> bool {
        self.all.contains(feature)
    }

    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = Symbol> {
        self.all.iter().cloned()
    }

    pub(crate) fn shrink_to_fit(&mut self) {
        self.all.shrink_to_fit();
    }
}

#[blaze_query::tracked]
impl UnstableFeatures {
    /// Query unstable features for a crate.
    ///
    /// This is also available as `DefMap::features()`. Use that if you have a DefMap available.
    /// Otherwise, use this, to not draw a dependency to the def map. While the crate's own def
    /// map is being built (a proc macro it calls), use [`UnstableFeatures::of_root`].
    #[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
    pub fn query(db: &Db, krate: Crate) -> UnstableFeatures {
        // clone: once per def-map change of the crate; the copy lets its readers backdate
        crate::crate_def_map(db, krate).features().clone()
    }
}

macro_rules! define_unstable_features {
    ( $( $feature:ident, )* ) => {
        #[derive(Clone, Default)]
        pub struct UnstableFeatures {
            all: HashSet<Symbol, Global>,

            $( pub $feature: bool, )*
        }

        impl UnstableFeatures {
            pub(crate) fn enable(&mut self, feature: Symbol) {
                match () {
                    $( () if feature == sym::$feature => self.$feature = true, )*
                    _ => {}
                }

                self.all.insert(feature);
            }
        }
    };
}

define_unstable_features! {
    lang_items,
    exhaustive_patterns,
    generic_associated_type_extended,
    arbitrary_self_types,
    arbitrary_self_types_pointers,
    supertrait_item_shadowing,
    new_range,
    never_type_fallback,
    specialization,
    min_specialization,
    ref_pat_eat_one_layer_2024,
    ref_pat_eat_one_layer_2024_structural,
    deref_patterns,
    never_patterns,
    mut_ref,
    type_changing_struct_update,
    staged_api,
    unsized_fn_params,
    coroutine_clone,
    gca_const_items,
    generic_const_exprs,
    sized_hierarchy,
    forced_keywords,
    // blaze's compile-time reflection (`docs/v2/plan/design/comptime.md`).
    blaze_comptime,
    // blaze's generic const expressions (`docs/v2/plan/design/ownership.md`): its own gate, not
    // `generic_const_exprs`, whose unfinished rustc rules (`thir_abstract_const`) blaze does
    // not have and the solver's own code paths for it must not take.
    blaze_const_exprs,
    // blaze's native verification clauses (`docs/v2/plan/design/verus.md`).
    blaze_verify,
    // blaze's implied bounds: the bounds a type's or a trait's definition declares hold where
    // an item's header names it (`docs/v2/plan/design/implied-bounds.md`).
    blaze_implied_bounds,
    // blaze's anonymous parameters: a `_` in a function's parameter types or an impl's header
    // is a type parameter of the item (`docs/v2/plan/design/implied-bounds.md`); it implies
    // `blaze_implied_bounds` ([`UnstableFeatures::implied_bounds`]).
    blaze_anon_params,
}

impl UnstableFeatures {
    /// Whether the crate's items get the bounds their headers imply: `blaze_implied_bounds`,
    /// or `blaze_anon_params`, whose `_` is bounded by them and nothing else.
    #[inline]
    pub fn implied_bounds(&self) -> bool {
        self.blaze_implied_bounds || self.blaze_anon_params
    }
}
