//! What each row of a body step's read list stands for (docs/v2/plan/design/item-hash.md
//! section 6; port decision 2): covered by the body's identity, one fact, or refused. A step
//! with a refused row is not stored. And the keys of a body's owner and of the impl or trait it
//! is in, which its early identity hashes (`crate::identity::early_parts`, its "owner" part).
//!
//! A step's list is the memo's (`blaze_memo::record`): one `Read::Query` row per call of a fact
//! boundary query, whose own reads are not in the list (the firewall), every other query
//! flattened into the step, and one `Read::Input` row per input read. The boundaries are the
//! kinds of one table, [`BOUNDARIES`], with lane items' (`blaze_items::goals::QUERIES`): each
//! row of it is a kind and the rule that answers a row of that kind, both written once, from
//! the one path of the query (`hir_ty::kinds`, `hir_def::kinds`, blaze-items' own), so the
//! name marked and the key decoded cannot name two queries. [`mark_boundaries`] marks exactly
//! the kinds of that table and of lane items' list, and [`fact_of`] answers a query row by the
//! first rule of that table whose kind it is, then by lane items' (`Goals::fact_of`); the test
//! `every_boundary_row_of_a_step_has_a_rule` checks the two agree on a real step's list.
//!
//! Flattened, not boundaries, and why flattening gives the classifier's answer:
//! - the rows the classifier read through (`Rule::Through`): layouts, the derived tables a
//!   layout reads, and the queries that run the solver in an inference context of their own
//!   (`layout_of_ty_query`, `layout_of_adt_query`, `struct_flags`, `struct_tail_may_be_unsized`,
//!   `bits_layout_query`, `bit_width_of`, `field_ranks`, `enum_discriminants`,
//!   `explicit_discriminant_indices`, `anon_const_eval_query`, `adt_dtorck_constraint`,
//!   `coroutine_witnesses_query`, `copy_clone_impl`, `auto_trait_depth`, `impl_super_outlives`,
//!   `inferred_outlives`, `int_gates`, `opaque_variances_query`, `lookup_impl_method_stored`,
//!   the borrow check's `is_copy_query`, `drop_glue_query`, `drop_constraint`, `prove_query`,
//!   `normalize_query`, `normalizes_to_error_query`, `path_bindings`): reading through was
//!   classifying each of their reads for this body and step, which is what a flattened list
//!   holds;
//! - the body's own rows (`Body::of`, `Body::has_lowering_errors`, `Body::relative`, the
//!   `lowering` step, `closure_exprs`, `ExprScopes::body_expr_scopes`, `body_upvars_mentioned`,
//!   `failed_expansion`, `line_literals`): the lowering step read them through; what they read
//!   of the body's text is behind its syntax, a boundary (`item_body_syntax`,
//!   `placed_body_syntax`, keyed by the body): covered for the body's own, as its key hashes the
//!   item's own text, and refused for another body's.
//!
//! Name resolution's boundaries are hir-def's own (`hir_def::nameres::boundary`): a crate's
//! resolution and one row per name, scope or walk a step reads of a finished def map, never a
//! file. A block's def map is no boundary: what it read is the step's (its owner's syntax, and
//! those rows for what its imports looked up outside it). [`mark_boundaries`] marks their kinds with this table's, and
//! [`fact_of`] answers a row of them by `boundary::name_read` ([`name_fact`]).
//!
//! The front end's settings are read by field (`base_db::SettingField`): each read is an input
//! row of the field's own kind, answered by that field's `Setting` fact ([`input`]).
//!
//! One classifier, two kinds of reader ([`fact_of_in`]): a body record, whose key covers its
//! body ([`Cover`]: its text, owner and crate inputs, its earlier steps), and a record whose
//! key covers no body (an instance's plan: drop glue and shims have no body at all), asked by
//! the crate the instance is worked out in. [`fact_of`] is the first's instance.

use base_db::{Crate, Db};
use blaze_ids::ContentId;
use blaze_items::{
    ItemId,
    goals::{self, FactRefKey, Goals},
    key, kinds as items,
};
use blaze_memo::{Builder, Fact, KindId, Memo, Read};
use hir_def::{
    AdtId, AssocItemId, AttrDefId, BlockIdLt, DefWithBodyId, ExpressionStoreOwnerId, GenericDefId,
    HasModule, ItemContainerId, Lookup, ModuleId, TraitId, TypeAliasId,
    hir::{ClosureKind, CoroutineKind},
    kinds as def,
    nameres::boundary::{self, NameRead},
};
use hir_ty::{
    InferBodyId,
    body_store::{Classified, STORED_ANSWER, Step},
    db::{InternedClosureId, InternedCoroutineId},
    generics::generics,
    kinds as ty,
    layout::field_order::FieldOrder,
    method_resolution::SelfTyKey,
    next_solver::goal_store::{FactObject, FactRef, GoalFacts},
};

use crate::values;

/// Marks every fact boundary on the session's first builder (`Builder::mark_boundary`): the
/// kinds of [`BOUNDARIES`] and of lane items' list, the kinds [`fact_of`] answers by a rule.
/// The driver calls it once, where it installs [`crate::Bodies`].
pub fn mark_boundaries(builder: &mut Builder) {
    for name in marked() {
        builder.mark_boundary(name);
    }
}

/// Every kind [`mark_boundaries`] marks: this crate's table, then lane items' list. A kind in
/// both (a row this crate answers first) is marked twice, which `Builder::mark_boundary` takes
/// as once.
fn marked() -> impl Iterator<Item = &'static str> {
    BOUNDARIES
        .iter()
        .map(|row| row.kind)
        .chain(goals::QUERIES.iter().copied())
        .chain(boundary::KINDS.iter().copied())
}

/// A fact boundary this crate answers: the kind's name (what `Builder::mark_boundary` takes)
/// and the rule that answers a row of it, `None` for a row of another kind.
pub struct Boundary {
    pub kind: &'static str,
    answer: fn(&Asker<'_>) -> Option<Classified>,
}

/// The rows of [`BOUNDARIES`]: for each query path `Q` (several share one rule), its
/// `KIND_NAME` and a rule that decodes the row's key with `Q::key_of` and answers it. The name
/// and the decoder come from the one path, so the two cannot disagree.
macro_rules! boundaries {
    ($( $($q:ty)|+ => |$asker:ident, $key:pat_param| $body:expr ),* $(,)?) => {
        &[$($(
            Boundary {
                kind: <$q>::KIND_NAME,
                answer: |$asker| {
                    let $key = <$q>::key_of($asker.memo(), $asker.read)?;
                    Some($body)
                },
            },
        )+)*]
    };
}

/// The one table of the boundaries this crate answers, in the order [`fact_of`] tries them,
/// before lane items' (`Goals::fact_of`, the one home of the rest).
pub const BOUNDARIES: &[Boundary] = boundaries! {
    // Part of an item's interface: its `Signature` fact.
    def::TypeAliasSignature_served
    | def::TypeAliasSignature_lowered
    | def::TypeAliasSignature_relative
    | def::TypeAliasSignature_with_source_map
    | ty::type_for_type_alias_with_diagnostics => |a, (it,)| a.signature(ItemId::Alias(it)),
    // A type alias's bounds: those of an associated type are part of its trait's interface.
    ty::type_alias_bounds_with_diagnostics => |a, (alias,)| {
        a.signature(alias_bounds_owner(a.db, alias))
    },
    def::AssocItemId_assoc_visibility => |a, (it,)| a.signature(ItemId::from(it)),
    def::ConstSignature_served
    | def::ConstSignature_lowered
    | def::ConstSignature_relative
    | def::ConstSignature_with_source_map
    | ty::type_for_const_with_diagnostics => |a, (it,)| a.signature(ItemId::Const(it)),
    def::FunctionSignature_relative
    | def::FunctionSignature_with_source_map
    | ty::elided_groups
    | ty::elided_lifetimes => |a, (it,)| a.signature(ItemId::Function(it)),
    ty::generics_require_sized_self
    | ty::const_param_types_with_diagnostics
    | ty::generic_defaults_with_diagnostics
    | ty::trait_environment_query => |a, (it,)| a.signature(blaze_items::key::generic_def_item(it)),
    def::StaticSignature_served
    | def::StaticSignature_lowered
    | def::StaticSignature_relative
    | def::StaticSignature_with_source_map
    | ty::type_for_static_with_diagnostics => |a, (it,)| a.signature(ItemId::Static(it)),
    // A trait's predicates as declared (`blaze_implied_bounds`): a part of the predicates its
    // `Signature` form hashes (`GenericPredicates::query_all`, which adds the implied bounds
    // to these), as the rule for `generic_predicates_of` of a trait gives.
    ty::explicit_implied_clauses_of
    | ty::explicit_super_clauses_of
    | ty::declared_trait_predicates_of => |a, (it,)| a.signature(ItemId::Trait(it)),
    def::VariantFields_field_visibilities => |a, (it,)| a.signature(ItemId::Adt(it.adt_id(a.db))),
    // A dependency's impl's row of its crate's impl batch, copied out by a query of the impl
    // (`hir_ty::lower::batched_impl_trait`): the impl's `Signature` fact, as its own query's
    // row (`impl_trait_of_impl`) is. Read through the batch itself it was the crate's
    // whole-crate fact, which moved with any impl of the crate and no record can hold.
    ty::batched_impl_self_ty | ty::batched_impl_trait | ty::batched_impl_predicates => |a, (it,)| {
        a.signature(ItemId::Impl(it))
    },

    // Member names: the `Fields` fact of the struct, union, enum, trait or impl.
    def::EnumVariants_of | ty::enum_variant_ids => |a, (it,)| a.fields(ItemId::Adt(it.into())),
    def::ImplItems_of | ty::impl_type_aliases => |a, (it,)| a.fields(ItemId::Impl(it)),
    def::TraitItems_query_with_diagnostics | ty::trait_type_aliases => |a, (it,)| {
        a.fields(ItemId::Trait(it))
    },
    def::VariantFields_served | def::VariantFields_lowered | ty::variant_field_ids => |a, (it,)| {
        a.fields(ItemId::Adt(it.adt_id(a.db)))
    },
    // Keyed by an impl or a trait and a name: its item of that name, of its `Fields` fact.
    ty::impl_value_item | ty::inherent_value_item => |a, (it, _)| a.fields(ItemId::Impl(it)),
    ty::trait_value_item => |a, (it, _)| a.fields(ItemId::Trait(it)),

    // Attributes: the `Attrs` fact of their owner.
    def::AttrFlags_repr_assume_has => |a, (it,)| a.attrs(AdtId::from(it).into()),
    def::target_features => |a, (it,)| a.attrs(it.into()),
    def::AttrFlags_query | def::blaze_bits | def::unstable_feature => |a, (it,)| a.attrs(it),
    // The attribute flags of an item's generic parameters, which the borrow check reads for a
    // `Drop` impl's `#[may_dangle]`: the item's `Attrs` fact, which hashes them
    // (`values::attrs_of`); the signature form does not.
    def::generic_params_attr_flags => |a, (it,)| a.attrs(generic_attr_owner(it)),

    // The hygiene key of an associated item's name: the `Fields` fact of its impl or trait
    // where the key is none (a name a real file or a `macro_rules!` writes), as that fact
    // spells for each member whether its name is keyed (`values::member_names`); refused by
    // its kind's name where the key is a context (a `macro` 2.0 name, a proc macro's
    // `Span::def_site`), which no fact spells stably yet.
    def::assoc_item_key => |a, (item,)| a.assoc_key(item),

    // One fact of one item.
    ty::destructor => |a, (it,)| a.item(items::DROP_CHECK, ItemId::Adt(it)),
    ty::orphan_rules_hold => |a, (it,)| a.item(items::COHERENCE, ItemId::Impl(it)),
    ty::builtin_derive_predicates => |a, (it,)| {
        a.item(items::DERIVE, ItemId::Adt(it.loc(a.db).adt))
    },
    // No row for `blaze_items::hash::interface_of` or `layout_of`, on purpose: an item's
    // interface hash folds in what bodies leak (a const's value, a `const fn`'s and an
    // immutable static's code, hidden types: `form::Cx::new`), and fills every item it names
    // with that item's interface hash, so it reads the leaks of its whole closure of refs; an
    // ADT's layout hash fills its non-ADT refs (an array length's const) the same way. No one
    // row stands for that: `Hashes` is no surface's row and answers `absent` forever
    // (`blaze_front::facts::HASHES` refuses it), and the surface's `Layout`, made where no
    // body's text is keyed, holds no const's value a body makes. So both flatten into their
    // asker and the boundary is one level down: each form's reads are its own rows (the
    // signature queries above), each leak its own fact (`const_eval_query` and
    // `const_eval_static_query` as `ConstValue`, a body's MIR and inference as its step facts),
    // and anything else read is refused by its kind's name, never answered `absent`.
    // A static's value or an enum variant's discriminant, by const eval: lane items'
    // `ConstValue` fact (a leak the signature hash marks).
    ty::const_eval_static_query => |a, (it,)| a.item(items::CONST_VALUE, ItemId::Static(it)),
    ty::const_eval_discriminant_variant | ty::explicit_discriminant => |a, (it,)| {
        a.fact(FactRef {
            kind: items::CONST_VALUE,
            subject: key::variant_key(a.db, it).0,
            object: FactObject::None,
        })
    },
    // The const's `ConstValue` fact when it is the evaluation lane items hashes: a const
    // without generics, with no arguments and no environment. Any other the classifier read
    // through, which a boundary cannot: refused, named (port report, decision needed).
    ty::const_eval_query => |a, (konst, subst, env)| {
        match env.is_none()
            && subst.as_ref().is_empty()
            && generics(a.db, konst.into()).iter(true).next().is_none()
        {
            true => a.item(items::CONST_VALUE, ItemId::Const(konst)),
            false => Classified::Refused(GENERIC_CONST_EVAL),
        }
    },
    // The value of the fact its key names, which is that fact. The body's value of the fact is
    // its crate's (`Bodies::value`): a value another crate asked of which impls it sees may
    // cover other crates. Any other fact is keyed by no asker, its value one for every crate.
    goals::fact_value => |a, (it,)| {
        let it: FactRefKey<'_> = it;
        match it.seen_by(a.db).is_none_or(|krate| krate == a.own) {
            true => a.fact(it.fact(a.db)),
            false => a.refused(),
        }
    },
    ty::inherent_assoc_types => |a, (adt,)| a.fact(values::inherent_assoc_types(a.db, adt)),
    ty::module_trait_set => |a, (module,)| a.fact(values::traits_in_scope(a.db, module)),

    // Keyed by a crate: covered for the body's crate, whose identity hashes it; for another,
    // its `CrateRoot` fact, its `CrateSettings` fact, or lane items' fact. Never its `Crate`
    // fact: none of these reads any file of the crate, so a body edit there moves none of
    // them, and a whole crate is no fact a record can name (blaze-front's `WHOLE_CRATE`).
    //
    // A crate's own lang items (`crate_lang_items`), and what a goal of the crate is keyed
    // by (`goal_crate_key`: the crate's key, its lang items by item key, the crates it loads
    // by key), are each part of its `CrateRoot`: its lang items are `lang_items`, which
    // prefers the crate's own to a dependency's, so a change of its own moves them; the
    // loaded crates are hashed by key; and the crate's key is the fact's subject, so the
    // fact's id moves with it.
    def::lang_items
    | def::crate_recursion_limit
    | def::UnstableFeatures_query
    | ty::loaded_crates => |a, (krate,)| {
        a.own_or(krate, || a.fact(values::crate_root(a.db, krate)))
    },
    goals::goal_crate_key | def::crate_lang_items => |a, (krate,)| {
        a.of_crate(krate, values::crate_root)
    },
    // A crate's key (`base_db::crate_item_key`) is of its settings and its dependencies' keys,
    // never of a file: its `CrateSettings` fact, valued by the settings' content ID
    // (`base_db::Crate::content`, which holds every field the key hashes and each dependency's
    // content) and subjected on the key itself, so a dependency's key moving moves its id.
    key::crate_key => |a, (krate,)| a.of_crate(krate, values::crate_settings),
    ty::target_data_layout_query => |a, (krate,)| a.own_or(krate, || a.items()),
    // Keyed by a crate, a table of all of it: its `Crate` fact.
    def::diagnostic_items
    | ty::InherentImpls_for_crate
    | ty::TraitImpls_for_crate
    | ty::crates_containing_incoherent_inherent_impls
    | ty::macros_writing_impls => |a, (krate,)| a.fact(whole_crate(a.db, krate)),
    // Keyed by a module, a table of all of its impls: its crate's `Crate` fact.
    def::module_impls | ty::InherentImpls_for_module | ty::TraitImpls_for_module => |a, (module,)| {
        let module: ModuleId = module;
        a.fact(whole_crate(a.db, module.krate(a.db)))
    },

    // A crate's tables by item, of the body's crate or a crate it loads: a fact per item as
    // the body's crate sees it, over every crate it loads (what the read can give, as the
    // loaded set is closed, and more, never less); of another crate, its `Crate` fact.
    // The impls of one trait for one simplified self type, or one trait's blanket impls: the
    // `Implements` fact, each impl by its key and signature. The blanket impls alone are the
    // fact of no simplified self type (`blaze_items::key::unsimplified`), as `relevant_impls`
    // reads are.
    ty::TraitImpls_in_crate_for_trait_and_self_ty
    | ty::TraitImpls_in_crate_and_deps_for_trait_and_self_ty => |a, (krate, trait_, SelfTyKey(self_ty))| {
        a.seen(krate, || implements_fact(a.db, trait_, key::simplified(a.db, &self_ty)))
    },
    ty::TraitImpls_blanket_in_crate_and_deps => |a, (krate, trait_)| {
        a.seen(krate, || implements_fact(a.db, trait_, key::unsimplified()))
    },
    // A listing: lane items' fact of it (`blaze_items::goals::fact_of_read`, the one home of
    // the mapping).
    ty::InherentImpls_in_crate_for_self_ty
    | ty::TraitImpls_for_crate_and_trait
    | ty::in_crate_trait_value_items => |a, (krate, _)| match a.sees(krate) {
        true => a.items(),
        false => a.fact(whole_crate(a.db, krate)),
    },
    // Of a module of any crate, as one module's traits in scope are the same whoever asks.
    ty::module_items_named => |a, _| a.items(),

    // A block's impls: lane items' `BlockImpls` fact, for the body's own block as for
    // another's. A block of no body's (a signature's array length) is no body's text: refused.
    ty::TraitImpls_for_block | ty::InherentImpls_for_block => |a, (block,)| a.block(block),
    ty::InherentImpls_in_block_for_self_ty => |a, (block, _)| a.block(block),
    ty::TraitImpls_in_block_for_trait_and_self_ty => |a, (block, _, _)| a.block(block),

    // The body's own match check, which the borrow check reads to skip a body whose patterns
    // failed: its `MatchCheck` fact. Refused for another body's.
    hir_ty_matchck::match_check => |a, (body,)| match a.covers(body) {
        true => Classified::Fact(values::match_check()),
        false => a.refused(),
    },

    // A step of a body.
    ty::infer_body_query => |a, (body,)| a.step(StepOf::of(a.def(), body), Step::Infer),
    ty::mir_body_query => |a, (body,)| a.step(StepOf::of_infer(a.db, a.def(), body), Step::Mir),
    ty::mir_body_for_closure_query => |a, (closure,)| {
        a.step(StepOf::of_closure(a.db, a.def(), closure), Step::Mir)
    },
    ty::mir_body_for_coroutine_query => |a, (coroutine,)| {
        a.step(StepOf::of_coroutine(a.db, a.def(), coroutine), Step::Mir)
    },

    // A fn-like macro call's argument, of a call written in a file
    // (`hir_expand::MacroCallId::call_input`, a boundary so the file's tree it reads stays
    // behind it): covered where the call is in the covered body's own syntax, whose text the
    // key hashes; refused otherwise ([`Asker::call_input`]'s rules).
    hir_expand::MacroCallId_call_input => |a, (call,)| a.call_input(call),

    // A body's syntax: covered when it is the body's own, as the key hashes the item's own
    // text; refused for another body's; where the key covers no body (a plan), that body's
    // `StepMir` fact ([`fact_of_in`]'s doc says why).
    def::item_body_syntax | def::placed_body_syntax => |a, (body,)| match (a.covers(body), a.covers) {
        (true, _) => Classified::Covered,
        (false, Some(_)) => a.refused(),
        (false, None) => values::step_fact_of(a.db, body, Step::Mir)
            .map_or_else(|| a.refused(), Classified::Fact),
    },
};

/// What `read`, a row of the list of `step` of `def`, stands for. A row no rule answers is
/// refused by its kind's name. A body record's instance of [`fact_of_in`]: asked by `def`'s
/// crate, covering `def` at `step`.
pub fn fact_of(db: &Db, def: DefWithBodyId, step: Step, read: Read) -> Classified {
    fact_of_in(db, def.krate(db), Some(Cover { body: def, step }), read)
}

/// What a record's key covers of the bodies it reads, beside the facts its read list holds:
/// one body's text, owner and crate inputs (its identity, `crate::identity::early_identity`)
/// and its steps before `step`. A body record's key covers its body; a plan's covers none
/// (its key is the instance and the stage, never a body's text).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cover {
    pub body: DefWithBodyId,
    pub step: Step,
}

/// What `read`, a row of the read list of a record asked by a body or instance of `asker`
/// whose key covers `covers`, stands for. A row no rule answers is refused by its kind's name.
///
/// `asker` is whose view the facts that depend on the asker are of (the impls it sees, its
/// crate's tables): a body's crate, or the crate an instance is worked out in (its home). With
/// `covers` `None` nothing is covered: what a body record takes as a function of its own key
/// is a fact here, never [`Classified::Covered`]:
///
/// - a table of the asker's own crate is that crate's fact, as another crate's is
///   ([`Asker::own_or`]), and a read of its `Crate` input its `CrateSettings` fact;
/// - a body's syntax, read by a reader that is no body of it, is that body's `StepMir` fact
///   (`values::step_fact_of`): what a plan holds of a body is what its MIR holds, its positions
///   item-relative and filled at link from the side table (rule 71), so a text edit that
///   moves what the plan holds moves its MIR; an edit that moves neither (a comment) moves
///   nothing;
/// - a stored answer read outside a step's own run, and a body's own match check, are refused
///   by name ([`UNCOVERED_ANSWER`], the match check's kind): no fact stands for them there;
/// - no subject is made relative: every fact names its subject by key.
pub fn fact_of_in(db: &Db, asker: Crate, covers: Option<Cover>, read: Read) -> Classified {
    if let Read::Input { kind, fact } = read {
        return input(db, asker, covers, kind, fact);
    }
    if let Some(of) = boundary::name_read(db, read) {
        return name_fact(db, covers, of);
    }
    let asker = Asker { db, covers, own: asker, read };
    BOUNDARIES.iter().find_map(|row| (row.answer)(&asker)).unwrap_or_else(|| asker.items())
}

/// What a stored answer's row (`hir_ty::body_store::STORED_ANSWER`) is refused as in a read
/// list whose key covers no body: it is a function of the covering step's own base key, and
/// there is none.
pub const UNCOVERED_ANSWER: &str =
    "hir_ty::body_store::stored_answer: a stored answer read outside a step's own run, which a key that covers no body does not hold";

/// What a row of name resolution's boundaries (`hir_def::nameres::boundary`) stands for:
///
/// - a name looked up in a module, found or not: lane items' `Resolves` fact of the module and
///   the name, which names what the name is in each namespace and in textual macro scope (each
///   target by its key: what the target is, its signature, is a row of its own, D51); a new
///   item that binds or shadows the name there moves it, an edit of a body's text does not;
/// - the traits in scope in a module: its `TraitsInScope` fact;
/// - a module's whole scope or the modules declared in it: its `WholeScope` fact, a scan;
/// - a walk of a crate's modules: its `Modules` fact, a scan;
/// - a crate's resolution, or its blaze edition (`boundary::blaze_edition`, said in its root's
///   text): its `CrateNames` fact, which holds both;
/// - a block's def map: no row of its own; it is no boundary, so its reads are the reader's,
///   the rows above for what it looked up outside the block and its owner's syntax for its
///   text (covered for the body's own block, refused for another body's);
/// - a keyed name ([`KEYED_NAME`]) or a scope no finished def map holds ([`UNPLACED_SCOPE`]):
///   refused, as no fact spells them.
fn name_fact(db: &Db, covers: Option<Cover>, of: NameRead) -> Classified {
    let relative = |fact| relative_to(db, covers, fact);
    match of {
        NameRead::Name(module, name) => Classified::Fact(relative(values::resolves(db, module, &name))),
        NameRead::Traits(module) => Classified::Fact(relative(values::traits_in_scope(db, module))),
        NameRead::WholeScope(module) => {
            Classified::Scan(relative(values::whole_scope(db, module)), "a module's whole scope")
        }
        NameRead::AllModules(krate) => {
            Classified::Scan(relative(values::modules(db, krate)), "every module of a crate")
        }
        NameRead::Crate(krate) => Classified::Fact(relative(values::crate_names(db, krate))),
        NameRead::Keyed(_) => Classified::Refused(KEYED_NAME),
        NameRead::Unplaced => Classified::Refused(UNPLACED_SCOPE),
    }
}

/// What a lookup of a keyed name (a `macro` 2.0's, a proc macro's `Span::def_site`) is refused
/// as: `Resolves` names a name by its text, and no fact spells its key.
pub const KEYED_NAME: &str = "hir_def::nameres::boundary: a keyed name's lookup, which no fact spells";

/// What a read of a scope no crate's finished def map or pages hold for its module is refused
/// as (a round's view's): no fact stands for it.
pub const UNPLACED_SCOPE: &str =
    "hir_def::nameres::boundary: a scope no finished def map holds, which no fact stands for";

/// `fact` with the covered body's key as its subject made relative (`values::relative`); as it
/// is where nothing is covered.
fn relative_to(db: &Db, covers: Option<Cover>, fact: FactRef) -> FactRef {
    match covers {
        Some(cover) => values::relative(db, cover.body, fact),
        None => fact,
    }
}

/// An input's row: the stored answer a hit took is covered (its fact is the step's own base
/// key), and refused where nothing is ([`UNCOVERED_ANSWER`]); the field order plan is the
/// build's `FieldOrder` fact; a crate's `Crate` input is [`crate_row`]'s. Every other input
/// row (a fact read around the inputs, an untracked read, a served image entry's fact, a file's
/// text, a crate's gift) is refused by its kind's name, as the classifier refused a read no
/// rule named. The body's own file text is never a row here: the body's syntax is a boundary,
/// so its text is behind it.
fn input(db: &Db, asker: Crate, covers: Option<Cover>, kind: KindId, fact: Fact) -> Classified {
    if kind == STORED_ANSWER {
        return match covers {
            Some(_) => Classified::Covered,
            None => Classified::Refused(UNCOVERED_ANSWER),
        };
    }
    if kind == KindId::of(FieldOrder::INPUT_KIND) {
        return Classified::Fact(values::field_order());
    }
    if kind == KindId::of(Crate::INPUT_KIND) {
        return crate_row(db, asker, covers.is_some(), fact).unwrap_or(Classified::Refused(kind.name()));
    }
    // One field of the front end's settings: that field's `Setting` fact. The row's kind is
    // the field's own (`base_db::SettingField::kind`), so a read of one setting is never a
    // read of the others.
    if let Some(field) = base_db::SettingField::of_kind(kind) {
        return Classified::Fact(values::setting(field));
    }
    Classified::Refused(kind.name())
}

/// A read of a field of a crate's `Crate` input (`data`, `extra_data`, `cfg_options`,
/// `workspace_data`, `env`, the list of its gifts): one row per crate, its fact the one the
/// world made the crate with, whose digest is the crate's content ID (`base_db::Crate::content`).
/// Covered for the body's own crate, whose identity hashes its inputs
/// (`identity::crate_inputs`); for a crate the body's crate loads, that crate's
/// `CrateSettings` fact ([`values::crate_settings`]), valued by its content ID, which holds no
/// source: a body edit in it moves nothing, where its `Crate` fact hashes every file of it and
/// of the crates below it (rule 65). `None` (refused by the caller) for a crate the body's
/// crate does not load. A gift's value is a `CrateSlot` input of its own, refused by that name.
/// Where the key covers no body (`covered` false), the asker's own crate is no cover either:
/// its `CrateSettings` fact, as a loaded crate's.
fn crate_row(db: &Db, own: Crate, covered: bool, fact: Fact) -> Option<Classified> {
    let memo = blaze_memo::Database::memo(db);
    if own.input_fact(memo) == fact {
        return Some(match covered {
            true => Classified::Covered,
            false => Classified::Fact(values::crate_settings(db, own)),
        });
    }
    let other = ty::loaded_crates(db, own).iter().copied().find(|it| it.input_fact(memo) == fact)?;
    Some(Classified::Fact(values::crate_settings(db, other)))
}

/// What a `const_eval_query` row of a generic evaluation is refused as: the classifier read it
/// through, and a boundary has no list of its own in the asker's.
pub const GENERIC_CONST_EVAL: &str =
    "hir_ty::consteval::const_eval_query: a generic evaluation, read through before";

/// What a macro call's argument read where the record's key covers no body (a plan's) is
/// refused as: a plan reads a body's MIR, never lowers one.
pub const CALL_UNCOVERED: &str = "hir_expand::MacroCallId::call_input: a macro call's argument read by a record that covers no body's text";
/// What a macro call's argument read by a body record is refused as where the call is not in
/// that body's own syntax: no fact stands for another place's tokens.
pub const CALL_OF_ANOTHER: &str = "hir_expand::MacroCallId::call_input: the argument of a macro call outside the body the record covers";

/// Which file or source root `read` is of, where it is an input row of a file's text
/// (`VfsText`), a file's source root (`VfsFileRoot`) or a source root (`VfsRoot`): the file's
/// path as its source root names it (`file 12` where none does), or the root's number and the
/// path of its first file. `None` for a row of another kind, or one no root of a crate of this
/// snapshot holds. For a told line of a refused row and a test's printout (rule 19): such a
/// row's fact is of the file's number alone (`base_db::world::facts`), which names nothing a
/// reader knows. Walks every crate's source root and its files, comparing input facts: only an
/// error path pays for it. Reads nothing into a list ([`blaze_memo::unrecorded`]).
pub fn file_of_row(db: &Db, read: Read) -> Option<String> {
    let Read::Input { kind, fact } = read else { return None };
    let text = kind == KindId::of(base_db::VfsText::INPUT_KIND);
    let file_root = kind == KindId::of(base_db::VfsFileRoot::INPUT_KIND);
    let root = kind == KindId::of(base_db::VfsRoot::INPUT_KIND);
    if !(text || file_root || root) {
        return None;
    }
    let memo = blaze_memo::Database::memo(db);
    blaze_memo::unrecorded(memo, || {
        let files = db.files();
        // alloc: the roots walked, once per told row
        let mut seen = Vec::new_in(std::alloc::Global);
        for krate in base_db::all_crates(db).iter() {
            let root_file = krate.root_file_id(db).file_id(db);
            let id = files.file_source_root(db, root_file).source_root_id(db);
            if seen.contains(&id) {
                continue;
            }
            seen.push(id);
            let input = files.source_root(db, id);
            let set = input.source_root_ref(db);
            let named = |file: base_db::FileId| match set.path_for_file(&file) {
                // alloc: the told name
                Some(path) => path.to_string(),
                None => format!("file {}", file.index()),
            };
            if root && let base_db::SourceRootInput::Vfs(it) = input && it.input_fact(memo) == fact {
                let first = set.iter().next().map(named).unwrap_or_default();
                return Some(format!("source root {} (holding `{first}`)", id.0));
            }
            for file in set.iter() {
                let found = match (text, file_root) {
                    (true, _) => {
                        files.has_file_text(db, file)
                            && matches!(files.file_text(db, file), base_db::FileText::Vfs(it) if it.input_fact(memo) == fact)
                    }
                    (_, true) => {
                        matches!(files.file_source_root(db, file), base_db::FileSourceRootInput::Vfs(it) if it.input_fact(memo) == fact)
                    }
                    _ => false,
                };
                if found {
                    return Some(named(file));
                }
            }
        }
        None
    })
}

/// What a step row of an inline constant of the body's own is refused as: the classifier read
/// it through, and a step query is a boundary.
pub const OWN_CONST_STEP: &str = "a step of an inline constant of the body's own, read through before";

/// Who asks what a row stands for: the asking crate, what the record's key covers (the body
/// and the step whose list holds `read`, or nothing) and the row.
pub struct Asker<'db> {
    db: &'db Db,
    covers: Option<Cover>,
    own: Crate,
    read: Read,
}

impl<'db> Asker<'db> {
    fn memo(&self) -> &'db Memo {
        blaze_memo::Database::memo(self.db)
    }

    /// The body the key covers, if any.
    fn def(&self) -> Option<DefWithBodyId> {
        self.covers.map(|it| it.body)
    }

    /// Whether the key covers `body`.
    fn covers(&self, body: DefWithBodyId) -> bool {
        self.def() == Some(body)
    }

    /// The row refused by its kind's name.
    fn refused(&self) -> Classified {
        Classified::Refused(self.read.kind().name())
    }

    /// A `call_input` row of `call`: covered where the key covers a body whose own syntax
    /// (`hir_def::expr_store::body::body_syntax`, the tree and maps of its anchor) holds the
    /// call within the body's item: the key hashes the body's text, the call's tokens in it,
    /// and the argument's spans are relative to the anchor. Refused, by rule, where the key
    /// covers no body ([`CALL_UNCOVERED`]) or the call is not in it ([`CALL_OF_ANOTHER`]).
    fn call_input(&self, call: hir_expand::MacroCallId) -> Classified {
        let Some(cover) = self.covers else { return Classified::Refused(CALL_UNCOVERED) };
        let hir_expand::MacroCallKind::FnLike { ast_id, .. } = &call.loc(self.db).kind else {
            return Classified::Refused(CALL_OF_ANOTHER);
        };
        let Some(syntax) = hir_def::expr_store::body::body_syntax(self.db, cover.body) else {
            return Classified::Refused(CALL_OF_ANOTHER);
        };
        let inside = syntax.file_id == ast_id.file_id
            && syntax
                .ast_id_map
                .try_get_erased(ast_id.value.erase())
                .is_some_and(|ptr| syntax.item.text_range().contains_range(ptr.text_range()));
        match inside {
            true => Classified::Covered,
            false => Classified::Refused(CALL_OF_ANOTHER),
        }
    }

    /// `fact`, with the covered body's key as its subject made relative (`values::relative`).
    /// A crate's whole `Crate` fact, from whichever rule chose it (this table's or lane
    /// items'), is a read of a whole table: [`Classified::Scan`] with the name of the query
    /// that read it, so a reader that refuses a whole crate (blaze-front's `WHOLE_CRATE`)
    /// names the query (rule 19) and the store counts who makes them.
    fn fact(&self, fact: FactRef) -> Classified {
        let fact = relative_to(self.db, self.covers, fact);
        match fact.kind == items::CRATE {
            true => Classified::Scan(fact, self.read.kind().name()),
            false => Classified::Fact(fact),
        }
    }

    fn item(&self, kind: blaze_query::QueryKind, item: ItemId) -> Classified {
        self.fact(item_fact(self.db, kind, item))
    }

    fn signature(&self, item: ItemId) -> Classified {
        self.item(items::SIGNATURE, item)
    }

    fn fields(&self, item: ItemId) -> Classified {
        self.fact(values::fields(self.db, item))
    }

    /// The `Attrs` fact of `owner`; refused for an owner no key names.
    fn attrs(&self, owner: AttrDefId) -> Classified {
        values::attrs(self.db, owner).map_or_else(|| self.refused(), |it| self.fact(it))
    }

    /// An `assoc_item_key` row of `item`: the `Fields` fact of its impl or trait where its
    /// name's key is none; refused by the row's kind's name where it is a context, or for an
    /// item in neither.
    fn assoc_key(&self, item: AssocItemId) -> Classified {
        if hir_def::nameres::assoc_item_key(self.db, item).is_some() {
            return self.refused();
        }
        let container = match item {
            AssocItemId::FunctionId(it) => it.lookup(self.db).container,
            AssocItemId::ConstId(it) => it.lookup(self.db).container,
            AssocItemId::TypeAliasId(it) => it.lookup(self.db).container,
        };
        match container {
            ItemContainerId::ImplId(it) => self.fields(ItemId::Impl(it)),
            ItemContainerId::TraitId(it) => self.fields(ItemId::Trait(it)),
            ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_) => self.refused(),
        }
    }

    /// Lane items' fact of the row as the body's crate sees it; refused by the name lane
    /// items gives where it has none.
    fn items(&self) -> Classified {
        match Goals.fact_of(self.db, Some(self.own), self.read) {
            Ok(fact) => self.fact(fact),
            Err(name) => Classified::Refused(name),
        }
    }

    /// Covered for a table of the body's own crate, whose identity hashes it; `other` for
    /// another crate's, and for the asker's own where the key covers no body.
    fn own_or(&self, krate: Crate, other: impl FnOnce() -> Classified) -> Classified {
        match krate == self.own && self.covers.is_some() {
            true => Classified::Covered,
            false => other(),
        }
    }

    /// A read of a crate's key, its own lang items or its goals' key: covered for the body's
    /// own crate, else `fact` of `krate`, a fact of the crate that holds no file of it. Under
    /// the test plant `WHOLE_CRATE_PLANT`, the classification these rows had before: the
    /// crate's `Crate` fact, which no record can name.
    fn of_crate(&self, krate: Crate, fact: fn(&Db, Crate) -> FactRef) -> Classified {
        #[cfg(test)]
        if tests::WHOLE_CRATE_PLANT.with(std::cell::Cell::get) {
            return self.own_or(krate, || self.fact(whole_crate(self.db, krate)));
        }
        self.own_or(krate, || self.fact(fact(self.db, krate)))
    }

    /// Whether the body's crate sees `krate`'s tables: it is that crate or loads it.
    fn sees(&self, krate: Crate) -> bool {
        krate == self.own || ty::loaded_crates(self.db, self.own).contains(&krate)
    }

    /// `fact` of a table of a crate the body's crate sees; another crate's `Crate` fact.
    fn seen(&self, krate: Crate, fact: impl FnOnce() -> FactRef) -> Classified {
        match self.sees(krate) {
            true => self.fact(fact()),
            false => self.fact(whole_crate(self.db, krate)),
        }
    }

    /// A block's impls: lane items' fact of the row for a block of a body, refused for a block
    /// of none.
    fn block(&self, block: BlockIdLt<'_>) -> Classified {
        match block.owner(self.db) {
            Some(_) => self.items(),
            None => self.refused(),
        }
    }

    /// A step row, of the step `earlier` of the body `of` names.
    fn step(&self, of: StepOf, earlier: Step) -> Classified {
        match of {
            StepOf::Own if self.covers.is_some_and(|it| it.step.reads(earlier)) => {
                values::step_fact(earlier).map_or_else(|| self.refused(), |it| self.fact(it))
            }
            StepOf::Own | StepOf::Not => self.refused(),
            StepOf::Other(body) => values::step_fact_of(self.db, body, earlier)
                .map_or_else(|| self.refused(), |it| self.fact(it)),
            StepOf::OwnConst => Classified::Refused(OWN_CONST_STEP),
        }
    }
}

/// The item whose interface a type alias's bounds are part of: an associated type's trait,
/// another alias itself.
fn alias_bounds_owner(db: &Db, alias: TypeAliasId) -> ItemId {
    match alias.lookup(db).container {
        ItemContainerId::TraitId(it) => ItemId::Trait(it),
        ItemContainerId::ModuleId(_) | ItemContainerId::ImplId(_) | ItemContainerId::ExternBlockId(_) => {
            ItemId::Alias(alias)
        }
    }
}

/// The item whose attributes a generic definition's parameters' flags are part of.
fn generic_attr_owner(def: GenericDefId) -> AttrDefId {
    match def {
        GenericDefId::AdtId(it) => it.into(),
        GenericDefId::ConstId(it) => it.into(),
        GenericDefId::FunctionId(it) => it.into(),
        GenericDefId::ImplId(it) => it.into(),
        GenericDefId::StaticId(it) => it.into(),
        GenericDefId::TraitId(it) => it.into(),
        GenericDefId::TypeAliasId(it) => it.into(),
    }
}

/// Whose step a step row is of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StepOf {
    /// `def` itself, or a closure or async coroutine written in it whose MIR is part of
    /// `def`'s MIR step value (`hir_ty::body_store::step_value`).
    Own,
    /// Another body, or such a closure or coroutine of it: that body's step value covers it.
    Other(DefWithBodyId),
    /// An anonymous constant written in `def`'s own body.
    OwnConst,
    /// Anything else: a constant of another body or of a signature, a coroutine kind no step
    /// value covers.
    Not,
}

impl StepOf {
    /// Whose step a row of `body`'s is, read by a record covering `def` (none: a plan, for
    /// which every body is another's).
    fn of(def: Option<DefWithBodyId>, body: DefWithBodyId) -> StepOf {
        match Some(body) == def {
            true => StepOf::Own,
            false => StepOf::Other(body),
        }
    }

    fn of_infer(db: &Db, def: Option<DefWithBodyId>, body: InferBodyId<'_>) -> StepOf {
        match body {
            InferBodyId::DefWithBodyId(body) => StepOf::of(def, body),
            InferBodyId::AnonConstId(it) => match it.loc(db).owner {
                ExpressionStoreOwnerId::Body(body) if Some(body) == def => StepOf::OwnConst,
                _ => StepOf::Not,
            },
        }
    }

    /// A closure's or coroutine's MIR is in its body's step value only for the kinds
    /// `body_store::mir::owners` lists; and a constant's closure is in no step value.
    fn covered(db: &Db, def: Option<DefWithBodyId>, owner: InferBodyId<'_>, yes: bool) -> StepOf {
        match (StepOf::of_infer(db, def, owner), yes) {
            (it @ (StepOf::Own | StepOf::Other(_)), true) => it,
            _ => StepOf::Not,
        }
    }

    fn of_closure(db: &Db, def: Option<DefWithBodyId>, closure: InternedClosureId<'_>) -> StepOf {
        let loc = closure.loc(db);
        let kind = matches!(
            loc.kind,
            ClosureKind::Closure | ClosureKind::CoroutineClosure(CoroutineKind::Async)
        );
        StepOf::covered(db, def, loc.owner, kind)
    }

    fn of_coroutine(db: &Db, def: Option<DefWithBodyId>, coroutine: InternedCoroutineId<'_>) -> StepOf {
        let loc = coroutine.loc(db);
        let kind = matches!(loc.kind, ClosureKind::Coroutine { kind: CoroutineKind::Async, .. });
        StepOf::covered(db, def, loc.owner, kind)
    }
}

/// Lane items' `Implements { trait_, self_ty }` fact as hir-ty's stores keep it: the trait by
/// key, the simplified self type as its object (`blaze_items::table`'s split of it).
fn implements_fact(db: &Db, trait_: TraitId, self_ty: blaze_items::SimplifiedType) -> FactRef {
    FactRef {
        kind: items::IMPLEMENTS,
        subject: key::item_key(db, ItemId::Trait(trait_)).0,
        object: FactObject::Id(self_ty.0),
    }
}

/// A lane items fact of one item and no object.
fn item_fact(db: &Db, kind: blaze_query::QueryKind, item: ItemId) -> FactRef {
    FactRef { kind, subject: key::item_key(db, item).0, object: FactObject::None }
}

fn whole_crate(db: &Db, krate: Crate) -> FactRef {
    FactRef { kind: items::CRATE, subject: key::crate_key(db, krate), object: FactObject::None }
}

/// The key of `def`'s item and, for one in an impl or a trait, of that impl or trait.
#[blaze_memo::tracked(returns(copy))]
pub(crate) fn owner_keys(db: &Db, def: DefWithBodyId) -> (ContentId, Option<ContentId>) {
    let owner = key::item_key(db, ItemId::of_body(db, def)).0;
    let container = match def {
        DefWithBodyId::FunctionId(it) => it.lookup(db).container,
        DefWithBodyId::ConstId(it) => it.lookup(db).container,
        DefWithBodyId::StaticId(_) | DefWithBodyId::VariantId(_) => return (owner, None),
    };
    let parent = match container {
        ItemContainerId::ImplId(it) => Some(ItemId::Impl(it)),
        ItemContainerId::TraitId(it) => Some(ItemId::Trait(it)),
        ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_) => None,
    };
    (owner, parent.map(|it| key::item_key(db, it).0))
}

#[cfg(test)]
mod tests {
    use base_db::all_crates;
    use hir_def::ModuleDefId;
    use hir_expand::{fixture, name::Name};
    use hir_ty::{LoweringMode, infer_query_with_inspect};

    use super::*;

    /// The invariant: no kind is in this crate's table twice, and every name it marks is a
    /// query's full path (`module::f`, `module::Type::method`), as `Builder::mark_boundary`
    /// takes; lane items' list is disjoint from it but for the rows this crate answers first
    /// (an impl's or a trait's items, attribute flags, a block's impls, a crate's tables by
    /// item, the target), which are listed here.
    #[test]
    fn the_boundaries_are_named_once_each() {
        let mut own: Vec<&str> = BOUNDARIES.iter().map(|row| row.kind).collect();
        own.sort_unstable();
        let before = own.len();
        own.dedup();
        assert_eq!(own.len(), before, "a kind twice in BOUNDARIES");
        for name in marked() {
            assert!(name.contains("::"), "{name} is no full path");
        }
        let mut shared: Vec<&str> =
            goals::QUERIES.iter().copied().filter(|it| own.binary_search(it).is_ok()).collect();
        shared.sort_unstable();
        let mut expected = [
            def::ImplItems_of::KIND_NAME,
            def::TraitItems_query_with_diagnostics::KIND_NAME,
            def::AttrFlags_query::KIND_NAME,
            ty::target_data_layout_query::KIND_NAME,
            ty::TraitImpls_for_block::KIND_NAME,
            ty::InherentImpls_for_block::KIND_NAME,
            ty::InherentImpls_in_block_for_self_ty::KIND_NAME,
            ty::TraitImpls_in_block_for_trait_and_self_ty::KIND_NAME,
            ty::InherentImpls_in_crate_for_self_ty::KIND_NAME,
            ty::TraitImpls_for_crate_and_trait::KIND_NAME,
            ty::in_crate_trait_value_items::KIND_NAME,
            ty::module_items_named::KIND_NAME,
        ];
        expected.sort_unstable();
        assert_eq!(shared, expected);
    }

    const FIXTURE: &str = r#"
//- /main.rs crate:main
pub struct S { pub a: u32 }
impl S { pub fn get(&self) -> u32 { self.a } }
pub trait Twice { fn twice(&self) -> u32; }
impl Twice for S { fn twice(&self) -> u32 { self.get() * 2 } }
pub fn k(s: S) -> u32 { s.get() + s.twice() + s.a }
"#;

    /// The body of the local crate's function named `name`.
    fn function(db: &Db, name: &str) -> DefWithBodyId {
        let krate = *all_crates(db)
            .iter()
            .find(|krate| krate.data(db).origin.is_local())
            .expect("a local crate");
        for (_, module) in hir_def::nameres::crate_def_map(db, krate).modules(db) {
            for item in module.scope(db).declarations() {
                if let ModuleDefId::FunctionId(it) = item
                    && hir_def::signatures::FunctionSignature::of(db, it).name
                        == Name::new_root(name)
                {
                    return DefWithBodyId::FunctionId(it);
                }
            }
        }
        panic!("no function `{name}`")
    }

    /// The invariant: [`mark_boundaries`] and [`fact_of`] read one table, so every
    /// `Read::Query` row a step lists (the memo lists one only for a marked kind) is of a kind
    /// `mark_boundaries` marks and is answered by a rule of that kind: covered or a fact, never
    /// refused by its kind's name as a row no rule names is. The list is that of `k`'s
    /// inference run outside any query, as the item store records it, in a database whose
    /// boundaries `mark_boundaries` marked. The control: the same list in a database with no
    /// boundary marked holds no `Read::Query` row at all, so the rows checked are there because
    /// they were marked.
    #[test]
    fn every_boundary_row_of_a_step_has_a_rule() {
        let rows = |boundaries: fn(&mut Builder)| {
            let db = fixture::db_with_boundaries(FIXTURE, boundaries, blaze_memo::Reads::Kept)
                .into_db();
            let def = function(&db, "k");
            let ((), reads) = blaze_memo::record(blaze_memo::Database::memo(&db), || {
                let _ = infer_query_with_inspect(&db, def, None, LoweringMode::Analysis);
            })
            .expect("the test database keeps read lists");
            let mut answered = Vec::new_in(std::alloc::Global);
            for read in reads.iter().copied() {
                if let Read::Query { .. } = read {
                    answered.push((read.kind().name(), fact_of(&db, def, Step::Infer, read)));
                }
            }
            answered
        };

        assert!(rows(|_| {}).is_empty(), "a query row listed with no boundary marked");
        let answered = rows(mark_boundaries);
        assert!(!answered.is_empty(), "the step lists no boundary row: nothing was checked");
        for (name, answer) in answered {
            assert!(marked().any(|it| it == name), "{name}: listed but not marked");
            assert!(
                matches!(answer, Classified::Covered | Classified::Fact(_) | Classified::Scan(..)),
                "{name}: a marked kind no rule answers ({answer:?})"
            );
        }
    }

    /// What a reader that covers no body takes a row as, or why it breaks the invariant: never
    /// covered, and no subject relative (every fact by key).
    fn uncovered(answer: Classified) -> Result<(), String> {
        match answer {
            Classified::Covered => Err("a row covered where the key covers no body".to_owned()),
            Classified::Fact(fact) | Classified::Scan(fact, _) if fact.subject == values::OWN => {
                Err(format!("a fact relative to no body: {fact:?}"))
            }
            _ => Ok(()),
        }
    }

    /// The invariant: classified for a record whose key covers no body ([`fact_of_in`] with
    /// `None`, a plan's), no row of a real step's list is covered and no fact names its subject
    /// relative to a body, so every row is a fact the driver can ask by key. The control: the
    /// same list classified for the body itself ([`fact_of`]) has covered or relative rows,
    /// which the oracle names, so it checks something.
    #[test]
    fn a_reader_covering_no_body_takes_no_row_as_covered() {
        let db = fixture::db_with_boundaries(FIXTURE, mark_boundaries, blaze_memo::Reads::Kept).into_db();
        let def = function(&db, "k");
        let ((), reads) = blaze_memo::record(blaze_memo::Database::memo(&db), || {
            let _ = infer_query_with_inspect(&db, def, None, LoweringMode::Analysis);
        })
        .expect("the test database keeps read lists");
        assert!(!reads.is_empty(), "the step read nothing: nothing was checked");
        for read in reads.iter().copied() {
            let answer = fact_of_in(&db, def.krate(&db), None, read);
            uncovered(answer).unwrap_or_else(|why| panic!("{}: {why}", read.kind().name()));
        }
        let caught = (reads.iter().copied())
            .map(|read| uncovered(fact_of(&db, def, Step::Infer, read)))
            .find_map(Result::err);
        assert!(caught.is_some(), "a body record's own classification has no covered or relative row");
    }

    std::thread_local! {
        /// The plant of [`Asker::of_crate`]: a read of another crate's key, own lang items or
        /// goals' key classified as before, as the crate's whole `Crate` fact.
        pub(super) static WHOLE_CRATE_PLANT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// `main`, whose body reads a function of `dep`, a library crate.
    const TWO_CRATES: &str = r#"
//- /dep.rs crate:dep new_source_root:library
pub fn d() -> u32 { 1 }
//- /main.rs crate:main deps:dep new_source_root:local
pub fn k() -> u32 { dep::d() }
"#;

    fn crate_named(db: &Db, name: &str) -> Crate {
        *all_crates(db)
            .iter()
            .find(|krate| krate.extra_data(db).display_name.as_ref().map(|it| it.canonical_name().as_str()) == Some(name))
            .unwrap_or_else(|| panic!("a crate named {name}"))
    }

    /// Each query row of a read of `dep`'s key, own lang items and goals' key, by its kind's
    /// name, classified for `main`'s body `k`.
    fn crate_rows(db: &Db) -> Vec<(&'static str, Classified)> {
        let (def, dep) = (function(db, "k"), crate_named(db, "dep"));
        let ((), reads) = blaze_memo::record(blaze_memo::Database::memo(db), || {
            let _ = key::crate_key(db, dep);
            let _ = goals::goal_crate_key(db, dep);
            let _ = hir_def::lang_item::crate_lang_items(db, dep);
        })
        .expect("the test database keeps read lists");
        let mut rows = Vec::new_in(std::alloc::Global);
        for read in reads.iter().copied() {
            if let Read::Query { .. } = read {
                rows.push((read.kind().name(), fact_of(db, def, Step::Infer, read)));
            }
        }
        rows
    }

    /// The invariant: a body's read of another crate's key is that crate's `CrateSettings`
    /// fact, and of its own lang items or its goals' key that crate's `CrateRoot` fact, each a
    /// row of the crate's surface; never the crate's whole `Crate` fact, which no record can
    /// name (blaze-front refuses it, `facts::WHOLE_CRATE`), so a body of any crate over `core`
    /// would be refused. The control: under the plant, the rows classified as before are each
    /// the whole `Crate` fact, which the oracle tells apart.
    #[test]
    fn a_read_of_another_crates_key_or_lang_items_is_a_fact_of_its_surface() {
        let db = fixture::db_with_boundaries(TWO_CRATES, mark_boundaries, blaze_memo::Reads::Kept).into_db();
        let dep = crate_named(&db, "dep");
        let expected = |name: &str| -> Option<FactRef> {
            if name == <key::crate_key>::KIND_NAME {
                Some(values::crate_settings(&db, dep))
            } else if name == <goals::goal_crate_key>::KIND_NAME || name == <def::crate_lang_items>::KIND_NAME {
                Some(values::crate_root(&db, dep))
            } else {
                None
            }
        };
        let rows = crate_rows(&db);
        let mut seen = 0;
        for &(name, answer) in rows.iter() {
            let Some(fact) = expected(name) else { continue };
            seen += 1;
            assert_eq!(answer, Classified::Fact(fact), "{name}");
        }
        assert_eq!(seen, 3, "the three reads were not each listed: {rows:?}");

        // Control: classified as before, each is the whole crate.
        WHOLE_CRATE_PLANT.with(|it| it.set(true));
        let planted = crate_rows(&db);
        WHOLE_CRATE_PLANT.with(|it| it.set(false));
        // Each a whole-table read, named by the query that read it (rule 19).
        let whole = whole_crate(&db, dep);
        let caught = (planted.iter())
            .filter(|&&(name, answer)| expected(name).is_some() && answer == Classified::Scan(whole, name))
            .count();
        assert_eq!(caught, 3, "the planted rows were not the whole crate: {planted:?}");
        assert_eq!(whole_crate(&db, dep).kind, items::CRATE);
    }

    /// The invariant (rule 21): the facts those reads stand for hold no file of the crate, so
    /// an edit of a body of `dep` moves neither, nor `dep`'s key (their subject). The control:
    /// an edit that can change `dep`'s lang items' or features' answer (a `#![feature]` in its
    /// root) moves its `CrateRoot` fact, so the comparison sees a change where there is one.
    #[test]
    fn a_body_edit_in_the_other_crate_moves_neither_fact() {
        let mut fixture = fixture::db_with_boundaries(TWO_CRATES, mark_boundaries, blaze_memo::Reads::Kept);
        let facts = |db: &Db| {
            let (main, dep) = (crate_named(db, "main"), crate_named(db, "dep"));
            let root = values::value(db, main, values::crate_root(db, dep)).expect("dep's crate root");
            let settings = values::value(db, main, values::crate_settings(db, dep)).expect("dep's settings");
            (key::crate_key(db, dep), root, settings)
        };
        let before = facts(fixture.db());
        let file = fixture.file_at("/dep.rs").file_id(fixture.db());
        _ = fixture.edit(|edit| _ = edit.set_file_text(file, "pub fn d() -> u32 { 2 }\n"));
        assert_eq!(facts(fixture.db()), before, "a body edit in dep moved a fact of its crate");

        // Control: a feature in dep's root moves its `CrateRoot`.
        let file = fixture.file_at("/dep.rs").file_id(fixture.db());
        _ = fixture.edit(|edit| _ = edit.set_file_text(file, "#![feature(never_type)]\npub fn d() -> u32 { 2 }\n"));
        let (key, root, settings) = facts(fixture.db());
        assert_eq!((key, settings), (before.0, before.2));
        assert_ne!(root, before.1, "a feature in dep's root left its crate root as it was");
    }

    /// `main`, whose body uses an impl of `dep`, a library crate, whose impls are lowered a
    /// crate at a time (`hir_ty::lower::batch`). The where clause names `dep`'s own traits: the
    /// fixture has no `core`, so a bound on `Copy` or `Sync` resolves to nothing and lowers to
    /// no clause, and an edit between the two would change no predicate.
    const DEP_IMPLS: &str = r#"
//- /dep.rs crate:dep new_source_root:library
pub trait T { fn t(&self) -> u32; }
pub trait W {}
pub trait V {}
pub struct A;
impl W for u32 {}
impl V for u32 {}
impl T for A where u32: W { fn t(&self) -> u32 { 1 } }
//- /main.rs crate:main deps:dep new_source_root:local
pub fn k(a: dep::A) -> u32 { dep::T::t(&a) }
"#;

    /// `dep`'s `impl T for A`: its one trait impl with an item.
    fn dep_trait_impl(db: &Db) -> hir_def::ImplId {
        use hir_ty::db::HirDatabase as _;
        let dep = crate_named(db, "dep");
        let mut found = None;
        for (_, module) in hir_def::nameres::crate_def_map(db, dep).modules(db) {
            for imp in module.scope(db).impls() {
                if db.impl_trait(imp).is_some() && !imp.impl_items(db).items.is_empty() {
                    assert!(found.replace(imp).is_none(), "dep has one trait impl with an item");
                }
            }
        }
        found.expect("`impl T for A` in dep")
    }

    /// Each query row of what `read` read, by its kind's name, classified for `main`'s `k`.
    fn rows_of(db: &Db, read: impl FnOnce()) -> Vec<(&'static str, Classified)> {
        let def = function(db, "k");
        let ((), reads) = blaze_memo::record(blaze_memo::Database::memo(db), read)
            .expect("the test database keeps read lists");
        let mut rows = Vec::new_in(std::alloc::Global);
        for read in reads.iter().copied() {
            if let Read::Query { .. } = read {
                rows.push((read.kind().name(), fact_of(db, def, Step::Mir, read)));
            }
        }
        rows
    }

    /// The invariant: a body's read of a dependency's impl's trait, self type or where clauses
    /// is that impl's `Signature` fact, through the impl's row of its crate's batch
    /// (`hir_ty::lower::batched_impl_*`), and no row is the dependency's whole `Crate` fact,
    /// which no record can name. The control removes the read-through: the batches read
    /// directly are each the whole crate, named by their query.
    #[test]
    fn a_read_of_a_dependency_impls_signature_is_that_impls_fact() {
        use hir_ty::db::HirDatabase as _;
        let db = fixture::db_with_boundaries(DEP_IMPLS, mark_boundaries, blaze_memo::Reads::Kept).into_db();
        let (dep, imp) = (crate_named(&db, "dep"), dep_trait_impl(&db));
        let signature = item_fact(&db, items::SIGNATURE, ItemId::Impl(imp));
        let whole = whole_crate(&db, dep);
        let rows = rows_of(&db, || {
            let _ = db.impl_trait(imp);
            let _ = db.impl_self_ty(imp);
            let _ = hir_ty::GenericPredicates::query_with_diagnostics(&db, imp.into());
        });
        let read_through =
            [ty::batched_impl_trait::KIND_NAME, ty::batched_impl_self_ty::KIND_NAME, ty::batched_impl_predicates::KIND_NAME];
        let mut seen = 0;
        for &(name, answer) in rows.iter() {
            assert!(
                !matches!(answer, Classified::Fact(it) | Classified::Scan(it, _) if it == whole),
                "{name}: a read of all of dep: {rows:?}"
            );
            if read_through.contains(&name) {
                seen += 1;
                assert_eq!(answer, Classified::Fact(signature), "{name}");
            }
        }
        assert_eq!(seen, 3, "the impl's rows were not each read through its batch: {rows:?}");

        // Control: the batches themselves, read whole.
        let planted = rows_of(&db, || {
            let _ = hir_ty::CrateImplTraits::of(&db, dep);
            let _ = hir_ty::CrateImplPredicates::of(&db, dep);
        });
        let batches = [ty::crate_impl_traits::KIND_NAME, ty::crate_impl_predicates::KIND_NAME];
        let caught = (planted.iter())
            .filter(|&&(name, answer)| batches.contains(&name) && answer == Classified::Scan(whole, name))
            .count();
        assert_eq!(caught, 2, "the batches read whole were not the whole crate: {planted:?}");
    }

    /// The invariant (rule 21): the fact a read of a dependency's impl stands for, and its
    /// value, do not move when another impl is added to the dependency. The control: an edit
    /// of the impl's where clause moves it, so the comparison sees a change where there is one.
    #[test]
    fn another_impl_added_to_the_dependency_moves_no_used_impls_fact() {
        let mut fixture = fixture::db_with_boundaries(DEP_IMPLS, mark_boundaries, blaze_memo::Reads::Kept);
        let fact = |db: &Db| {
            let main = crate_named(db, "main");
            let signature = item_fact(db, items::SIGNATURE, ItemId::Impl(dep_trait_impl(db)));
            (signature, values::value(db, main, signature).expect("the impl's signature value"))
        };
        let before = fact(fixture.db());
        // `dep.rs` of the fixture, as it is.
        let (_, after_header) = DEP_IMPLS.split_once("new_source_root:library\n").expect("dep's header");
        let (used, _) = after_header.split_once("//- /main.rs").expect("main's header");
        let file = fixture.file_at("/dep.rs").file_id(fixture.db());
        let added = format!("{used}pub struct B;\nimpl B {{ pub fn b() {{}} }}\n");
        _ = fixture.edit(|edit| _ = edit.set_file_text(file, &added));
        assert_eq!(fact(fixture.db()), before, "an impl added to dep moved the used impl's fact");

        // Control: the used impl's where clause, edited.
        let file = fixture.file_at("/dep.rs").file_id(fixture.db());
        let edited = added.replace("where u32: W", "where u32: V");
        assert_ne!(edited, added, "premise: the edit applies");
        _ = fixture.edit(|edit| _ = edit.set_file_text(file, &edited));
        assert_ne!(fact(fixture.db()), before, "an edit of the impl's where clause left its fact as it was");
    }
}
