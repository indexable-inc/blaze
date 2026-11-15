//! The queries of this crate that a reader outside it names by kind, in one place (as
//! `hir_ty::kinds` is hir-ty's): each is re-exported here with what `#[blaze_memo::tracked]`
//! generates for it, its `KIND_NAME` (what `blaze_memo::Builder::mark_boundary` takes) and its
//! `key_of` (the typed arguments of a `blaze_memo::Read::Query` row of it, `None` for a row of
//! another kind). A row is decoded by the query it names, never by a name written as text.
//!
//! Each is a fact boundary: lane items maps a row of a signature's to the item's `Signature`
//! fact (`blaze_items::goals::fact_of_read`), and lane item-hash the rest
//! (`blaze_item_hash::reads::BOUNDARIES`, one table of each kind it marks and the rule that
//! answers a row of it). The queries live in their own modules; this is their one public path
//! for a reader by kind. Read a query's answer through its module's own API, not this one.
//!
//! Not here, and why: name resolution has no row per name any more. A module's scope is part
//! of the value of the query that collected its def map (`nameres::crate_resolution`, a block's
//! `block_def_map`), which reads its crate's files whole, so no query of it stands for one
//! name; the classifier's `NAME_QUERIES` went with the rows. Nor are the queries that read a
//! file's syntax whole (`expr_store::body`'s `file_body_syntax`): a body reads its own syntax
//! through [`item_body_syntax`] or [`placed_body_syntax`], keyed by the body.

pub use crate::{
    attrs::{
        AttrFlags_query, AttrFlags_repr_assume_has, blaze_bits, generic_params_attr_flags,
        target_features, unstable_feature,
    },
    diagnostic_item::diagnostic_items,
    expr_store::body::{item_body_syntax, placed_body_syntax},
    lang_item::{crate_lang_items, lang_items},
    nameres::{
        assoc::{ImplItems_of, TraitItems_query_with_diagnostics},
        assoc_item_key, crate_recursion_limit, module_impls,
    },
    signatures::{
        ConstSignature_lowered, ConstSignature_relative, ConstSignature_served,
        ConstSignature_with_source_map, EnumSignature_lowered, EnumSignature_served,
        EnumVariants_of, FunctionSignature_lowered, FunctionSignature_relative,
        FunctionSignature_served, FunctionSignature_with_source_map, ImplSignature_lowered,
        ImplSignature_served, StaticSignature_lowered, StaticSignature_relative,
        StaticSignature_served, StaticSignature_with_source_map, StructSignature_lowered,
        StructSignature_served, TraitSignature_lowered, TraitSignature_served,
        TypeAliasSignature_lowered, TypeAliasSignature_relative, TypeAliasSignature_served,
        TypeAliasSignature_with_source_map, UnionSignature_lowered, UnionSignature_served,
        VariantFields_lowered, VariantFields_served,
    },
    unstable_features::UnstableFeatures_query,
    visibility::{AssocItemId_assoc_visibility, VariantFields_field_visibilities},
};
