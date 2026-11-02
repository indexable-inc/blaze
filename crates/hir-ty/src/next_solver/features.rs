//! The crate's `#![feature(..)]`s, as the solver asks for them (rustc's
//! `impl inherent::Features<TyCtxt> for &rustc_feature::Features`).

use base_db::{Crate, Db};
use hir_def::unstable_features::UnstableFeatures;
use rustc_type_ir::inherent;

use super::{DbInterner, Symbol};

/// The features of the crate an interner solves for, read once when the interner is made
/// (`DbInterner::new_with`).
#[derive(Debug, Copy, Clone)]
pub struct Features<'db> {
    unstable: &'db UnstableFeatures,
}

impl<'db> Features<'db> {
    #[inline]
    pub(crate) fn of(db: &'db Db, krate: Crate) -> Features<'db> {
        Features { unstable: UnstableFeatures::query(db, krate) }
    }

    /// `#![feature(sized_hierarchy)]`.
    #[inline]
    pub fn sized_hierarchy(self) -> bool {
        self.unstable.sized_hierarchy
    }
}

impl<'db> inherent::Features<DbInterner<'db>> for Features<'db> {
    #[inline]
    fn generic_const_exprs(self) -> bool {
        self.unstable.generic_const_exprs
    }

    #[inline]
    fn gca_const_items(self) -> bool {
        self.unstable.gca_const_items
    }

    #[inline]
    fn coroutine_clone(self) -> bool {
        self.unstable.coroutine_clone
    }

    /// A feature bound never holds in a `staged_api` crate, even one that enables the feature,
    /// so that an unstable impl does not leak into stable API.
    fn feature_bound_holds_in_crate(self, symbol: Symbol) -> bool {
        !self.unstable.staged_api && self.unstable.is_enabled(&symbol.0)
    }
}

#[cfg(test)]
mod tests {
    use base_db::{Crate, Db};
    use hir_def::{FunctionId, ModuleDefId, TraitId, nameres::crate_def_map};
    use rustc_type_ir::{
        Interner as _,
        inherent::{Clause as _, Features as _},
    };
    use hir_expand::fixture;

    use crate::{
        lower::{GenericPredicates, trait_environment},
        next_solver::{
            DbInterner, GenericArgs, Symbol, TraitRef,
            infer::traits::{Obligation, ObligationCause},
            util::sizedness_fast_path,
        },
    };

    const ITEMS: &str = r#"
use core::marker::PointeeSized;
trait Tr: PointeeSized {}
fn sized_param<T: Tr>() {}
fn unsized_param<T: ?Sized + Tr>() {}
fn pointee_param<T: PointeeSized + Tr>() {}
"#;

    const FEATURES: [&str; 4] =
        ["coroutine_clone", "generic_const_exprs", "gca_const_items", "sized_hierarchy"];

    /// The fixture crate with `#![feature(<features>)]` at its root (none when empty).
    fn fixture(features: &str) -> (Db, Crate) {
        let attr =
            if features.is_empty() { String::new() } else { format!("#![feature({features})]\n") };
        let db = fixture::db(&format!("//- minicore: sized\n{attr}{ITEMS}")).into_db();
        let krate = fixture::test_crate(&db);
        (db, krate)
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Answers {
        coroutine_clone: bool,
        generic_const_exprs: bool,
        gca_const_items: bool,
        sized_hierarchy: bool,
    }

    impl Answers {
        fn only(feature: &str) -> Answers {
            Answers {
                coroutine_clone: feature == "coroutine_clone",
                generic_const_exprs: feature == "generic_const_exprs",
                gca_const_items: feature == "gca_const_items",
                sized_hierarchy: feature == "sized_hierarchy",
            }
        }
    }

    fn answers(features: &str) -> Answers {
        let (db, krate) = fixture(features);
        let features = DbInterner::new_with(&db, krate).features();
        Answers {
            coroutine_clone: features.coroutine_clone(),
            generic_const_exprs: features.generic_const_exprs(),
            gca_const_items: features.gca_const_items(),
            sized_hierarchy: features.sized_hierarchy(),
        }
    }

    #[test]
    fn coroutine_clone_on_and_off() {
        assert_eq!(answers("coroutine_clone"), Answers::only("coroutine_clone"));
        assert_eq!(answers(""), Answers::only(""));
    }

    #[test]
    fn each_feature_alone() {
        for feature in FEATURES {
            assert_eq!(answers(feature), Answers::only(feature), "#![feature({feature})]");
        }
    }

    #[test]
    fn every_feature_together() {
        let all = Answers {
            coroutine_clone: true,
            generic_const_exprs: true,
            gca_const_items: true,
            sized_hierarchy: true,
        };
        assert_eq!(answers(&FEATURES.join(", ")), all);
    }

    fn feature_bound_holds(features: &str, symbol: &str) -> bool {
        let (db, krate) = fixture(features);
        DbInterner::new_with(&db, krate)
            .features()
            .feature_bound_holds_in_crate(Symbol(intern::Symbol::intern(symbol)))
    }

    #[test]
    fn feature_bound_holds_where_the_crate_enables_it() {
        assert!(feature_bound_holds("unstable_impls", "unstable_impls"));
        assert!(!feature_bound_holds("unstable_impls", "other_impls"));
        assert!(!feature_bound_holds("", "unstable_impls"));
    }

    #[test]
    fn feature_bound_never_holds_in_a_staged_api_crate() {
        assert!(!feature_bound_holds("staged_api, unstable_impls", "unstable_impls"));
    }

    #[test]
    #[should_panic(expected = "without a crate")]
    fn an_interner_without_a_crate_has_no_features() {
        let (db, _) = fixture("");
        DbInterner::new_no_crate(&db).features();
    }

    fn function(db: &Db, krate: Crate, name: &str) -> FunctionId {
        let def_map = crate_def_map(db, krate);
        let scope = def_map.module(db, def_map.root_module_id()).scope(db);
        scope
            .values()
            .find_map(|(it, item)| match item.def {
                ModuleDefId::FunctionId(id) if it.as_str() == name => Some(id),
                _ => None,
            })
            .unwrap_or_else(|| panic!("the fixture defines `fn {name}`"))
    }

    fn trait_tr(db: &Db, krate: Crate) -> TraitId {
        let def_map = crate_def_map(db, krate);
        let scope = def_map.module(db, def_map.root_module_id()).scope(db);
        scope
            .types()
            .find_map(|(it, item)| match item.def {
                ModuleDefId::TraitId(id) if it.as_str() == "Tr" => Some(id),
                _ => None,
            })
            .expect("the fixture defines `trait Tr`")
    }

    /// Whether the fast path proves `T: MetaSized` for the `T` of `fn <function>` in its own
    /// environment.
    fn meta_sized_fast_path(features: &str, function_name: &str) -> bool {
        let (db, krate) = fixture(features);
        let interner = DbInterner::new_with(&db, krate);
        let def = function(&db, krate, function_name).into();
        let tr = trait_tr(&db, krate);
        let param_env = trait_environment(&db, def);
        let t = GenericPredicates::query_all(&db, def)
            .skip_binder()
            .filter_map(|clause| clause.as_trait_clause())
            .find(|clause| clause.skip_binder().def_id().0 == tr)
            .map(|clause| clause.skip_binder().self_ty())
            .unwrap_or_else(|| panic!("`{function_name}` has `T: Tr`"));
        let meta_sized = interner.lang_items().MetaSized.expect("minicore has `MetaSized`");
        let trait_ref = TraitRef::new_from_args(
            interner,
            meta_sized.into(),
            GenericArgs::new_from_slice(&[t.into()]),
        );
        let goal =
            Obligation::new(interner, ObligationCause::dummy(), param_env, trait_ref).as_goal();
        sizedness_fast_path(interner, goal.predicate, param_env)
    }

    #[test]
    fn meta_sized_on_a_param_without_sized_hierarchy_always_holds() {
        assert!(meta_sized_fast_path("", "sized_param"));
        assert!(meta_sized_fast_path("", "unsized_param"));
    }

    /// rustc's `add_sizedness_bounds`: `?Sized` is a `MetaSized` bound, and only
    /// `PointeeSized` leaves a parameter none.
    #[test]
    fn meta_sized_on_a_param_with_sized_hierarchy_needs_a_bound() {
        assert!(meta_sized_fast_path("sized_hierarchy", "sized_param"));
        assert!(meta_sized_fast_path("sized_hierarchy", "unsized_param"));
        assert!(!meta_sized_fast_path("sized_hierarchy", "pointee_param"));
    }
}
