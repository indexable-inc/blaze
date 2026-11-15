//! The queries of this crate that a reader outside it names by kind, in one place: each is
//! re-exported here with what `#[blaze_memo::tracked]` generates for it, its `KIND_NAME` (what
//! `blaze_memo::Builder::mark_boundary` takes) and its `key_of` (the typed arguments of a
//! `blaze_memo::Read::Query` row of it, `None` for a row of another kind). A row is decoded by
//! the query it names, never by a name written as text.
//!
//! Each is a fact boundary: lane items maps a row of it to the fact it stands for
//! (`blaze_items::goals::fact_of_read`, whose `QUERIES` lists the kinds it decodes, hir-def's
//! with these), and lane item-hash the rest (`blaze_item_hash::reads::BOUNDARIES`, one table
//! of each kind it marks and the rule that answers a row of it). The queries live in their own
//! modules, most of them private; this is their public path. Read a query's answer through its
//! module's own API, not through this one.

pub use crate::{
    builtin_derive::predicates as builtin_derive_predicates,
    consteval::{
        const_eval_discriminant_variant, const_eval_query, const_eval_static_query,
        discriminants::explicit_discriminant,
    },
    drop::destructor,
    dyn_compatibility::{dyn_compatibility_of_trait_query, generics_require_sized_self},
    infer::infer_body_query,
    layout::{enum_variant_ids, target::target_data_layout_query, variant_field_ids},
    lower::{
        batch::{crate_field_types, crate_impl_predicates, crate_impl_self_tys, crate_impl_traits},
        batched_field_types, batched_impl_predicates, batched_impl_self_ty, batched_impl_trait,
        const_conditions_query, declared_trait_predicates_of, const_param_types_with_diagnostics, elided_groups,
        elided_lifetimes, explicit_implied_clauses_of, explicit_super_clauses_of,
        field_types_of_variant, fn_sig_for_enum_variant_constructor, fn_sig_for_fn,
        fn_sig_for_struct_constructor, generic_defaults_with_diagnostics, generic_predicates_of,
        impl_self_ty_of_impl, impl_trait_of_impl, path::inherent_assoc_types, supertraits_info,
        trait_environment_query, type_alias_bounds_with_diagnostics,
        type_for_const_with_diagnostics, type_for_static_with_diagnostics,
        type_for_type_alias_with_diagnostics,
    },
    method_resolution::{
        InherentImpls_for_block, InherentImpls_for_crate, InherentImpls_for_module,
        InherentImpls_in_block_for_self_ty, InherentImpls_in_crate_for_self_ty,
        TraitImpls_blanket_in_crate_and_deps, TraitImpls_for_block, TraitImpls_for_crate,
        TraitImpls_for_crate_and_trait, TraitImpls_for_module,
        TraitImpls_in_block_for_trait_and_self_ty,
        TraitImpls_in_crate_and_deps_for_trait_and_self_ty,
        TraitImpls_in_crate_for_trait_and_self_ty, crates_containing_incoherent_inherent_impls,
        impl_value_item, in_crate_trait_value_items, inherent_value_item, loaded_crates,
        macros_writing_impls, relevant_impls,
        scope_sets::{module_items_named, module_trait_set},
        trait_value_item,
    },
    next_solver::util::ItemBounds_of,
    opaques::{impl_type_aliases, rpit_hidden_types, tait_hidden_types, trait_type_aliases},
    providers::{mir_body_for_closure_query, mir_body_for_coroutine_query, mir_body_query},
    representability::representability,
    rpitit::{impl_rpitit_tys, rpitit_item_bounds},
    specialization::{impl_has_unconstrained_params, may_specialize, specializes_query},
    traits::orphan_rules_hold,
    variance::{ctor_variances_query, variances_of_query},
};
