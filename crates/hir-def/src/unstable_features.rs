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
            all: HashSet<Symbol>,

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
}
