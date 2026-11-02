//! What each read a body's step makes stands for (docs/v2/plan/design/item-hash.md section 6):
//! covered by the body's identity, one fact, or refused. A step with a refused read is not
//! stored. The audit behind each rule, and the reads refused on purpose, are in
//! docs/v2/plan/lanes/item-hash/reads.md.
//!
//! A read is found by its ingredient's debug name in [`RULES`], sorted, by binary search, then
//! told apart from a same-named ingredient by the file its ingredient was declared in: a field
//! of an input or tracked struct is named by the field alone (`data`, `scope`), so the name says little.
//! A read no rule names is refused.

use std::cmp::Ordering;

use base_db::{Crate, Db};
use blaze_ids::ContentId;
use blaze_items::{
    ItemId,
    goals::{FactRefKey, Goals},
    key, kinds as items,
};
use blaze_query::db::{Database as _, FromIdIn, Id, ReadKind};
use hir_def::{
    AdtId, AssocItemId, AttrDefId, BuiltinDeriveImplId, DefWithBodyId, EnumId, EnumVariantId,
    ExpressionStoreOwnerId, FunctionId, GenericDefId, HasModule, ImplId, ItemContainerId, Lookup,
    ModuleId, StaticId, TraitId, TypeAliasId, VariantId,
    hir::{ClosureKind, CoroutineKind},
};
use hir_ty::{
    InferBodyId,
    body_store::{Classified, Step},
    db::{InternedClosureId, InternedCoroutineId},
    generics::generics,
    method_resolution::{
        InherentImpls_in_crate_for_self_ty, TraitImpls_blanket_in_crate_and_deps,
        TraitImpls_for_crate_and_trait, TraitImpls_in_crate_and_deps_for_trait_and_self_ty,
        TraitImpls_in_crate_for_trait_and_self_ty, impl_value_item, in_crate_trait_value_items,
        scope_sets::module_items_named, trait_value_item,
    },
    next_solver::goal_store::{FactObject, FactRef, GoalFacts},
};

use crate::values;

/// What a read of one ingredient stands for. Each rule says what its key is; a key of another
/// type (a supertype's other variant) is refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Rule {
    /// The identity covers it for any key: what name resolution reads (the def maps, module
    /// scopes and their names, the traits in scope), which the identity hashes as what each of
    /// the body's paths resolves to, its traits in scope and its module chain.
    Names,
    /// Keyed by a body, as a `DefWithBodyId` or its `InferBodyId`: covered when it is the
    /// body's own (its HIR, scopes and upvars are of its codec bytes and resolutions, and
    /// whether its macro calls expanded as rustc parses them is of the calls the identity
    /// hashes; a body whose expansion is refused builds no MIR to store), refused for
    /// another's.
    OwnBody,
    /// A field of the `Crate` input: covered for the body's crate (the identity hashes its
    /// inputs), another crate's `Crate` fact.
    CrateInput,
    /// Keyed by a crate: covered for the body's crate, whose identity hashes it
    /// (`identity::crate_inputs`); for another crate, what `Else` says.
    OwnCrate(Else),
    /// Keyed by a crate, a function of what its root decides: its `CrateRoot` fact.
    CrateRoot,
    /// Keyed by a crate, a table of all of it: its `Crate` fact.
    WholeCrate,
    /// Keyed by a module, a table of all of its impls: its crate's `Crate` fact.
    ModuleCrate,
    /// Lane items' `fact_of_read` (`blaze_items::goals`).
    Items,
    /// Part of an item's interface: its `Signature` fact.
    Signature(Of),
    /// Member names: the `Fields` fact of the struct, union, enum, trait or impl.
    Fields(Of),
    /// Attributes: the `Attrs` fact of their owner.
    Attrs(Of),
    /// `destructor(adt)`: the ADT's `DropCheck` fact.
    DropCheck,
    /// `orphan_rules_hold(impl)`: the impl's `Coherence` fact.
    Coherence,
    /// A built-in derive impl's predicates: its ADT's `Derive` fact.
    Derive,
    /// A static's value or an enum variant's discriminant, by const eval: lane items'
    /// `ConstValue` fact of the static or the variant (a leak the signature hash marks).
    ConstValue,
    /// `blaze_items::goals::fact_value`: the value of the fact its key names, which is that fact.
    FactValue,
    /// Keyed by several arguments, a crate first (or a module's): a table of that crate's
    /// items, or of it and its dependencies. Another crate's `Crate` fact, which hashes it and
    /// every crate it depends on; refused for the body's own, whose impls and traits no fact
    /// lists.
    DepCrate(Args),
    /// Keyed by an impl or a trait and a name: its item of that name, of its `Fields` fact as
    /// `ImplItems::of` and `TraitItems` are.
    FieldNamed(Of),
    /// `closure_exprs(owner, ())`: covered for the body's own store (its closures are of its
    /// HIR), refused for another's.
    OwnStore,
    /// `const_eval_query(const, args, env)`: the const's `ConstValue` fact when it is the
    /// evaluation lane items hashes, of a const without generics, with no arguments and no
    /// environment; refused for any other.
    ConstEval,
    /// A step of the body itself (its inference; its MIR, or that of a closure or an async
    /// coroutine written in it): that step's fact, for a later step of the same body. Refused
    /// for another body's, for an inline const's, for a coroutine kind whose MIR the step's
    /// value leaves out, and for the same or a later step.
    OwnStep(Step),
    /// `match_check(def)` of the body itself, which the borrow check reads to skip a body
    /// whose patterns failed: its `MatchCheck` fact. Refused for another body's.
    OwnMatchCheck,
}

/// The arguments of a [`Rule::DepCrate`] read, as the query's key interns them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Args {
    /// `(Crate, SelfTyKey)`.
    SelfTy,
    /// `(Crate, TraitId)`.
    Trait,
    /// `(Crate, TraitId, SelfTyKey)`.
    TraitSelfTy,
    /// `(Crate, Name)`.
    Name,
    /// `(ModuleId, Name)`: the module's crate.
    ModuleName,
}

/// What a [`Rule::OwnCrate`] read of another crate stands for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Else {
    CrateRoot,
    WholeCrate,
    Items,
}

/// What a rule's key is, and so the item its fact names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Of {
    Const,
    Static,
    Alias,
    /// A type alias's bounds: those of an associated type are part of its trait's interface.
    AliasBounds,
    Function,
    GenericDef,
    /// A `VariantId`: its struct or union, or an enum variant's enum.
    Variant,
    Enum,
    Trait,
    Impl,
    Assoc,
    /// An `AttrDefId`.
    AttrOwner,
}

/// Every ingredient a rule is had for, by debug name then the end of the path of the file it
/// is declared in, sorted by name.
#[rustfmt::skip]
pub(crate) const RULES: &[(&str, &str, Rule)] = &[
    ("AssocItemId::assoc_visibility", "hir-def/src/visibility.rs", Rule::Signature(Of::Assoc)),
    ("AttrFlags::query", "hir-def/src/attrs.rs", Rule::Attrs(Of::AttrOwner)),
    ("BlockIdLt", "hir-def/src/lib.rs", Rule::Names),
    ("Body::has_lowering_errors", "hir-def/src/expr_store/body.rs", Rule::OwnBody),
    ("Body::of", "hir-def/src/expr_store/body.rs", Rule::OwnBody),
    ("ConstSignature::kept", "hir-def/src/signatures.rs", Rule::Signature(Of::Const)),
    ("CrateResolution::module", "hir-def/src/nameres/delta/publish.rs", Rule::Names),
    ("DefMapPair", "hir-def/src/nameres.rs", Rule::Names),
    ("EnumSignature::kept", "hir-def/src/signatures.rs", Rule::Items),
    ("EnumVariants::of", "hir-def/src/signatures.rs", Rule::Fields(Of::Enum)),
    ("ExprScopes::body_expr_scopes", "hir-def/src/expr_store/scope.rs", Rule::OwnBody),
    ("FunctionSignature::kept", "hir-def/src/signatures.rs", Rule::Items),
    ("ImplItems::of", "hir-def/src/nameres/assoc.rs", Rule::Fields(Of::Impl)),
    ("ImplSignature::kept", "hir-def/src/signatures.rs", Rule::Items),
    ("InherentImpls::for_crate", "hir-ty/src/method_resolution.rs", Rule::WholeCrate),
    ("InherentImpls::for_module", "hir-ty/src/method_resolution.rs", Rule::ModuleCrate),
    ("InherentImpls::in_crate_for_self_ty", "hir-ty/src/method_resolution.rs", Rule::DepCrate(Args::SelfTy)),
    ("ItemBounds::of", "hir-ty/src/next_solver/util.rs", Rule::Items),
    ("ModuleIdLt", "hir-def/src/lib.rs", Rule::Names),
    ("ModuleScope", "hir-def/src/nameres.rs", Rule::Names),
    ("ModuleScope::legacy_macro", "hir-def/src/nameres.rs", Rule::Names),
    ("ModuleScope::name", "hir-def/src/nameres.rs", Rule::Names),
    ("ModuleScope::traits", "hir-def/src/nameres.rs", Rule::Names),
    ("Modules", "hir-def/src/nameres.rs", Rule::Names),
    ("Modules::module", "hir-def/src/nameres.rs", Rule::Names),
    ("Modules::name", "hir-def/src/nameres.rs", Rule::Names),
    ("ScopeInput::legacy_macro", "hir-def/src/nameres/delta/publish.rs", Rule::Names),
    ("ScopeInput::name", "hir-def/src/nameres/delta/publish.rs", Rule::Names),
    ("ScopeInput::traits", "hir-def/src/nameres/delta/publish.rs", Rule::Names),
    ("StaticSignature::kept", "hir-def/src/signatures.rs", Rule::Signature(Of::Static)),
    ("StructSignature::kept", "hir-def/src/signatures.rs", Rule::Items),
    ("TraitImpls::blanket_in_crate_and_deps", "hir-ty/src/method_resolution.rs", Rule::DepCrate(Args::Trait)),
    ("TraitImpls::for_crate", "hir-ty/src/method_resolution.rs", Rule::WholeCrate),
    ("TraitImpls::for_crate_and_deps", "hir-ty/src/method_resolution.rs", Rule::WholeCrate),
    ("TraitImpls::for_crate_and_trait", "hir-ty/src/method_resolution.rs", Rule::DepCrate(Args::Trait)),
    ("TraitImpls::for_module", "hir-ty/src/method_resolution.rs", Rule::ModuleCrate),
    ("TraitImpls::in_crate_and_deps_for_trait_and_self_ty", "hir-ty/src/method_resolution.rs", Rule::DepCrate(Args::TraitSelfTy)),
    ("TraitImpls::in_crate_for_trait_and_self_ty", "hir-ty/src/method_resolution.rs", Rule::DepCrate(Args::TraitSelfTy)),
    ("TraitItems::query_with_diagnostics", "hir-def/src/nameres/assoc.rs", Rule::Fields(Of::Trait)),
    ("TraitSignature::kept", "hir-def/src/signatures.rs", Rule::Items),
    ("TypeAliasSignature::kept", "hir-def/src/signatures.rs", Rule::Signature(Of::Alias)),
    ("UnionSignature::kept", "hir-def/src/signatures.rs", Rule::Items),
    ("UnstableFeatures::query", "hir-def/src/unstable_features.rs", Rule::OwnCrate(Else::CrateRoot)),
    ("VariantFields::field_visibilities", "hir-def/src/visibility.rs", Rule::Signature(Of::Variant)),
    ("VariantFields::kept", "hir-def/src/signatures.rs", Rule::Fields(Of::Variant)),
    ("block_def_map", "hir-def/src/nameres.rs", Rule::Names),
    ("body_upvars_mentioned", "hir-def/src/upvars.rs", Rule::OwnBody),
    ("cfg_options", "base-db/src/input.rs", Rule::CrateInput),
    ("children", "hir-def/src/nameres.rs", Rule::Names),
    ("closure_exprs", "hir-ty/src/db.rs", Rule::OwnStore),
    ("const_conditions_query", "hir-ty/src/lower.rs", Rule::Items),
    ("const_eval_discriminant_variant", "hir-ty/src/consteval.rs", Rule::ConstValue),
    ("const_eval_query", "hir-ty/src/consteval.rs", Rule::ConstEval),
    ("const_eval_static_query", "hir-ty/src/consteval.rs", Rule::ConstValue),
    ("const_param_types_with_diagnostics", "hir-ty/src/lower.rs", Rule::Signature(Of::GenericDef)),
    ("crate_field_types", "hir-ty/src/lower/batch.rs", Rule::Items),
    ("crate_impl_predicates", "hir-ty/src/lower/batch.rs", Rule::Items),
    ("crate_impl_self_tys", "hir-ty/src/lower/batch.rs", Rule::Items),
    ("crate_impl_traits", "hir-ty/src/lower/batch.rs", Rule::Items),
    ("crate_key", "blaze-items/src/key.rs", Rule::OwnCrate(Else::WholeCrate)),
    ("crate_lang_items", "hir-def/src/lang_item.rs", Rule::OwnCrate(Else::WholeCrate)),
    ("crate_local_def_map", "hir-def/src/nameres.rs", Rule::Names),
    ("crate_recursion_limit", "hir-def/src/nameres.rs", Rule::OwnCrate(Else::CrateRoot)),
    ("crate_shadows_primitive_type", "hir-def/src/nameres.rs", Rule::Names),
    ("crates_containing_incoherent_inherent_impls", "hir-ty/src/method_resolution.rs", Rule::WholeCrate),
    ("ctor_variances_query", "hir-ty/src/variance.rs", Rule::Items),
    ("data", "base-db/src/input.rs", Rule::CrateInput),
    ("def_map", "hir-def/src/nameres.rs", Rule::Names),
    ("destructor", "hir-ty/src/drop.rs", Rule::DropCheck),
    ("diagnostic_items", "hir-def/src/diagnostic_item.rs", Rule::WholeCrate),
    ("dyn_compatibility_of_trait_query", "hir-ty/src/dyn_compatibility.rs", Rule::Items),
    ("elided_lifetimes", "hir-ty/src/lower.rs", Rule::Signature(Of::Function)),
    ("extra_data", "base-db/src/input.rs", Rule::CrateInput),
    ("fact_value", "blaze-items/src/goals.rs", Rule::FactValue),
    ("failed_expansion", "hir-ty-mir/src/mir/lower.rs", Rule::OwnBody),
    ("field_types_of_variant", "hir-ty/src/lower.rs", Rule::Items),
    ("fn_sig_for_enum_variant_constructor", "hir-ty/src/lower.rs", Rule::Items),
    ("fn_sig_for_fn", "hir-ty/src/lower.rs", Rule::Items),
    ("fn_sig_for_struct_constructor", "hir-ty/src/lower.rs", Rule::Items),
    ("generic_defaults_with_diagnostics", "hir-ty/src/lower.rs", Rule::Signature(Of::GenericDef)),
    ("generic_predicates_of", "hir-ty/src/lower.rs", Rule::Items),
    ("generics_require_sized_self", "hir-ty/src/dyn_compatibility.rs", Rule::Signature(Of::GenericDef)),
    ("goal_crate_key", "blaze-items/src/goals.rs", Rule::OwnCrate(Else::WholeCrate)),
    ("impl_has_unconstrained_params", "hir-ty/src/specialization.rs", Rule::Items),
    ("impl_self_ty_of_impl", "hir-ty/src/lower.rs", Rule::Items),
    ("impl_trait_of_impl", "hir-ty/src/lower.rs", Rule::Items),
    ("impl_type_aliases", "hir-ty/src/opaques.rs", Rule::Fields(Of::Impl)),
    ("impl_value_item", "hir-ty/src/method_resolution.rs", Rule::FieldNamed(Of::Impl)),
    ("in_crate_trait_value_items", "hir-ty/src/method_resolution.rs", Rule::DepCrate(Args::Name)),
    ("infer_body_query", "hir-ty/src/infer.rs", Rule::OwnStep(Step::Infer)),
    ("lang_items", "hir-def/src/lang_item.rs", Rule::OwnCrate(Else::CrateRoot)),
    ("line_literals", "hir-ty-mir/src/mir/location.rs", Rule::OwnBody),
    ("loaded_crates", "hir-ty/src/method_resolution.rs", Rule::OwnCrate(Else::CrateRoot)),
    ("macros_writing_impls", "hir-ty/src/method_resolution.rs", Rule::WholeCrate),
    ("match_check", "hir-ty-matchck/src/lib.rs", Rule::OwnMatchCheck),
    ("may_specialize", "hir-ty/src/specialization.rs", Rule::Items),
    ("mir_body_for_closure_query", "hir-ty/src/providers.rs", Rule::OwnStep(Step::Mir)),
    ("mir_body_for_coroutine_query", "hir-ty/src/providers.rs", Rule::OwnStep(Step::Mir)),
    ("mir_body_query", "hir-ty/src/providers.rs", Rule::OwnStep(Step::Mir)),
    ("module_impls", "hir-def/src/nameres/facts/impls.rs", Rule::ModuleCrate),
    ("module_items_named", "hir-ty/src/method_resolution/scope_sets.rs", Rule::DepCrate(Args::ModuleName)),
    ("module_trait_set", "hir-ty/src/method_resolution/scope_sets.rs", Rule::Names),
    ("orphan_rules_hold", "hir-ty/src/traits.rs", Rule::Coherence),
    ("predicates", "hir-ty/src/builtin_derive.rs", Rule::Derive),
    ("relevant_impls", "hir-ty/src/method_resolution.rs", Rule::Items),
    ("representability", "hir-ty/src/representability.rs", Rule::Items),
    ("row", "hir-def/src/nameres/facts/queries.rs", Rule::Names),
    ("rpit_hidden_types", "hir-ty/src/opaques.rs", Rule::Items),
    ("scope", "hir-def/src/nameres.rs", Rule::Names),
    ("specializes_query", "hir-ty/src/specialization.rs", Rule::Items),
    ("supertraits_info", "hir-ty/src/lower.rs", Rule::Items),
    ("tait_hidden_types", "hir-ty/src/opaques.rs", Rule::Items),
    ("target_data_layout_query", "hir-ty/src/layout/target.rs", Rule::OwnCrate(Else::Items)),
    ("target_features", "hir-def/src/attrs.rs", Rule::Attrs(Of::Function)),
    ("trait_environment_query", "hir-ty/src/lower.rs", Rule::Signature(Of::GenericDef)),
    ("trait_type_aliases", "hir-ty/src/opaques.rs", Rule::Fields(Of::Trait)),
    ("trait_value_item", "hir-ty/src/method_resolution.rs", Rule::FieldNamed(Of::Trait)),
    ("transitive_deps", "base-db/src/input.rs", Rule::CrateRoot),
    ("type_alias_bounds_with_diagnostics", "hir-ty/src/lower.rs", Rule::Signature(Of::AliasBounds)),
    ("type_for_const_with_diagnostics", "hir-ty/src/lower.rs", Rule::Signature(Of::Const)),
    ("type_for_static_with_diagnostics", "hir-ty/src/lower.rs", Rule::Signature(Of::Static)),
    ("type_for_type_alias_with_diagnostics", "hir-ty/src/lower.rs", Rule::Signature(Of::Alias)),
    ("unstable_feature", "hir-def/src/attrs.rs", Rule::Attrs(Of::AttrOwner)),
    ("variances_of_query", "hir-ty/src/variance.rs", Rule::Items),
    ("workspace_data", "base-db/src/input.rs", Rule::CrateInput),
];

/// What `read`, made by `step` of `def`, stands for. `name` is its ingredient's debug name.
pub fn classify(
    db: &Db,
    def: DefWithBodyId,
    step: Step,
    read: blaze_query::db::DatabaseKeyIndex,
    kind: ReadKind,
    name: &'static str,
) -> Classified {
    match kind {
        // An interned value is a function of its fields, which whoever interned it read.
        ReadKind::Interned => return Classified::Covered,
        ReadKind::Function | ReadKind::InputField(_) | ReadKind::TrackedField(_) => {}
    }
    // Lane items' table lists names of types without generics, as they are written.
    let name = match listed_parts(name) {
        [head, ""] => head,
        _ => name,
    };
    match rule_of(db, read, name) {
        Some(rule) => apply(db, def, step, read, name, rule),
        None => Classified::Refused,
    }
}

/// `name` as [`RULES`] lists it, as the two parts it is the concatenation of. The query engine
/// names a method of a `#[blaze_query::tracked] impl` `Type::method`, by the type's name
/// without its generics, unless the impl gives a `self_ty`, whose text is kept as written,
/// generics and all (`TraitImpls < 'db >::for_crate`); the listed name leaves the generics
/// out (`TraitImpls::for_crate`). A free tracked function or a field keeps its name.
pub(crate) fn listed_parts(name: &str) -> [&str; 2] {
    let Some(open) = name.find('<') else { return [name, ""] };
    let mut depth = 0usize;
    let close = name[open..].char_indices().find_map(|(at, c)| {
        match c {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + at);
                }
            }
            _ => {}
        }
        None
    });
    match close {
        Some(close) => [name[..open].trim_end(), name[close + 1..].trim_start()],
        None => [name, ""],
    }
}

/// How `listed`, a name as [`RULES`] lists it, orders against `name`, an ingredient's debug
/// name as [`listed_parts`] reads it.
pub(crate) fn cmp_listed(listed: &str, name: &str) -> Ordering {
    let [head, tail] = listed_parts(name);
    listed.bytes().cmp(head.bytes().chain(tail.bytes()))
}

/// The rule for `name`, if one is had for the ingredient `read` is of.
pub(crate) fn rule_of(
    db: &Db,
    read: blaze_query::db::DatabaseKeyIndex,
    name: &str,
) -> Option<Rule> {
    let first = RULES.partition_point(|&(it, _, _)| cmp_listed(it, name) == Ordering::Less);
    let same =
        RULES[first..].iter().take_while(|&&(it, _, _)| cmp_listed(it, name) == Ordering::Equal);
    let mut file = None;
    for &(_, at, rule) in same {
        let file =
            *file.get_or_insert_with(|| db.storage().ingredient_file(read.ingredient_index()));
        if ends_with_path(file, at) {
            return Some(rule);
        }
    }
    None
}

/// Whether `file` is `at` or ends in `/` then `at`.
fn ends_with_path(file: &str, at: &str) -> bool {
    match file.strip_suffix(at) {
        Some(rest) => rest.is_empty() || rest.ends_with('/') || rest.ends_with('\\'),
        None => false,
    }
}

fn apply(
    db: &Db,
    def: DefWithBodyId,
    step: Step,
    read: blaze_query::db::DatabaseKeyIndex,
    name: &'static str,
    rule: Rule,
) -> Classified {
    let key = read.key_index();
    let found = match rule {
        Rule::Names => return Classified::Covered,
        Rule::OwnBody => {
            let own = match arg::<DefWithBodyId>(db, key) {
                Some(body) => body == def,
                None => arg::<InferBodyId<'_>>(db, key) == Some(InferBodyId::DefWithBodyId(def)),
            };
            return match own {
                true => Classified::Covered,
                false => Classified::Refused,
            };
        }
        Rule::CrateInput => match arg::<Crate>(db, key) {
            Some(krate) if krate == def.krate(db) => return Classified::Covered,
            Some(krate) => Some(whole_crate(db, krate)),
            None => None,
        },
        Rule::OwnCrate(other) => match arg::<Crate>(db, key) {
            Some(krate) if krate == def.krate(db) => return Classified::Covered,
            Some(krate) => match other {
                Else::CrateRoot => Some(values::crate_root(db, krate)),
                Else::WholeCrate => Some(whole_crate(db, krate)),
                Else::Items => Goals.fact_of(db, read, name),
            },
            None => None,
        },
        Rule::CrateRoot => arg::<Crate>(db, key).map(|krate| values::crate_root(db, krate)),
        Rule::WholeCrate => arg::<Crate>(db, key).map(|krate| whole_crate(db, krate)),
        Rule::ModuleCrate => {
            arg::<ModuleId>(db, key).map(|module| whole_crate(db, module.krate(db)))
        }
        Rule::Items => Goals.fact_of(db, read, name),
        Rule::Signature(of) => item_of(db, key, of).map(|item| signature(db, item)),
        Rule::Fields(of) => item_of(db, key, of).map(|item| values::fields(db, item)),
        Rule::Attrs(Of::Function) => {
            arg::<FunctionId>(db, key).and_then(|it| values::attrs(db, it.into()))
        }
        Rule::Attrs(_) => arg::<AttrDefId>(db, key).and_then(|it| values::attrs(db, it)),
        Rule::DropCheck => {
            arg::<AdtId>(db, key).map(|adt| item_fact(db, items::DROP_CHECK, ItemId::Adt(adt)))
        }
        Rule::Coherence => {
            arg::<ImplId>(db, key).map(|imp| item_fact(db, items::COHERENCE, ItemId::Impl(imp)))
        }
        Rule::Derive => arg::<BuiltinDeriveImplId>(db, key)
            .map(|imp| item_fact(db, items::DERIVE, ItemId::Adt(imp.loc(db).adt))),
        Rule::ConstValue => match arg::<StaticId>(db, key) {
            Some(it) => Some(item_fact(db, items::CONST_VALUE, ItemId::Static(it))),
            None => arg::<EnumVariantId>(db, key).map(|it| FactRef {
                kind: items::CONST_VALUE,
                subject: key::variant_key(db, it).0,
                object: FactObject::None,
            }),
        },
        Rule::FactValue => arg::<FactRefKey<'_>>(db, key).map(|it| it.fact(db)),
        Rule::DepCrate(of) => match crate_of_args(db, key, of) {
            Some(krate) if krate == def.krate(db) => return Classified::Refused,
            krate => krate.map(|krate| whole_crate(db, krate)),
        },
        Rule::FieldNamed(of) => match of {
            Of::Impl => impl_value_item::arguments(db, key).map(|(it, _)| ItemId::Impl(it)),
            Of::Trait => trait_value_item::arguments(db, key).map(|(it, _)| ItemId::Trait(it)),
            _ => None,
        }
        .map(|item| values::fields(db, item)),
        Rule::OwnStore => {
            return match hir_ty::db::closure_exprs::arguments(db, key) {
                Some((owner, ())) if owner == ExpressionStoreOwnerId::Body(def) => {
                    Classified::Covered
                }
                _ => Classified::Refused,
            };
        }
        Rule::OwnStep(earlier) => {
            if earlier >= step || !is_own_step(db, def, key) {
                return Classified::Refused;
            }
            values::step_fact(db, def, earlier)
        }
        Rule::OwnMatchCheck => match arg::<DefWithBodyId>(db, key) {
            Some(body) if body == def => Some(values::match_check(db, def)),
            _ => None,
        },
        Rule::ConstEval => match hir_ty::consteval::const_eval_query::arguments(db, key) {
            Some((konst, subst, None))
                if subst.as_ref().is_empty()
                    && generics(db, konst.into()).iter(true).next().is_none() =>
            {
                Some(item_fact(db, items::CONST_VALUE, ItemId::Const(konst)))
            }
            _ => None,
        },
    };
    match found {
        None => Classified::Refused,
        Some(fact) if fact.kind == items::SIGNATURE && is_owner(db, def, fact.subject) => {
            Classified::Covered
        }
        Some(fact) if fact.kind == values::kinds::ATTRS && is_own_attrs(db, def, fact.subject) => {
            Classified::Covered
        }
        Some(fact) => Classified::Fact(fact),
    }
}

/// The item a key of `of` names, for its `Signature` or `Fields` fact.
fn item_of(db: &Db, key: Id, of: Of) -> Option<ItemId> {
    Some(match of {
        Of::Const => ItemId::Const(arg(db, key)?),
        Of::Static => ItemId::Static(arg::<StaticId>(db, key)?),
        Of::Alias => ItemId::Alias(arg(db, key)?),
        Of::AliasBounds => {
            let alias: TypeAliasId = arg(db, key)?;
            match alias.lookup(db).container {
                ItemContainerId::TraitId(it) => ItemId::Trait(it),
                ItemContainerId::ModuleId(_)
                | ItemContainerId::ImplId(_)
                | ItemContainerId::ExternBlockId(_) => ItemId::Alias(alias),
            }
        }
        Of::Function => ItemId::Function(arg(db, key)?),
        Of::GenericDef => generic_def_item(arg::<GenericDefId>(db, key)?),
        Of::Variant => match arg::<VariantId>(db, key)? {
            VariantId::StructId(it) => ItemId::Adt(it.into()),
            VariantId::UnionId(it) => ItemId::Adt(it.into()),
            VariantId::EnumVariantId(it) => ItemId::Adt(it.lookup(db).parent.into()),
        },
        Of::Enum => ItemId::Adt(arg::<EnumId>(db, key)?.into()),
        Of::Trait => ItemId::Trait(arg::<TraitId>(db, key)?),
        Of::Impl => ItemId::Impl(arg::<ImplId>(db, key)?),
        Of::Assoc => ItemId::from(arg::<AssocItemId>(db, key)?),
        Of::AttrOwner => return None,
    })
}

/// Whether `key`, of a [`Rule::OwnStep`] read, names `def` itself or a closure or async
/// coroutine written in it whose MIR is part of `def`'s MIR step value
/// (`hir_ty::body_store::step_value`).
fn is_own_step(db: &Db, def: DefWithBodyId, key: Id) -> bool {
    let own = InferBodyId::DefWithBodyId(def);
    if let Some(body) = arg::<DefWithBodyId>(db, key) {
        return body == def;
    }
    if let Some(body) = arg::<InferBodyId<'_>>(db, key) {
        return body == own;
    }
    if let Some(closure) = arg::<InternedClosureId<'_>>(db, key) {
        let loc = closure.loc(db);
        return loc.owner == own
            && matches!(
                loc.kind,
                ClosureKind::Closure | ClosureKind::CoroutineClosure(CoroutineKind::Async)
            );
    }
    if let Some(coroutine) = arg::<InternedCoroutineId<'_>>(db, key) {
        let loc = coroutine.loc(db);
        return loc.owner == own
            && matches!(loc.kind, ClosureKind::Coroutine { kind: CoroutineKind::Async, .. });
    }
    false
}

/// The crate a [`Rule::DepCrate`] read's arguments name.
fn crate_of_args(db: &Db, key: Id, of: Args) -> Option<Crate> {
    Some(match of {
        Args::SelfTy => InherentImpls_in_crate_for_self_ty::arguments(db, key)?.0,
        Args::Trait => {
            TraitImpls_for_crate_and_trait::arguments(db, key)
                .or_else(|| TraitImpls_blanket_in_crate_and_deps::arguments(db, key))?
                .0
        }
        Args::TraitSelfTy => {
            TraitImpls_in_crate_for_trait_and_self_ty::arguments(db, key)
                .or_else(|| TraitImpls_in_crate_and_deps_for_trait_and_self_ty::arguments(db, key))?
                .0
        }
        Args::Name => in_crate_trait_value_items::arguments(db, key)?.0,
        Args::ModuleName => module_items_named::arguments(db, key)?.0.krate(db),
    })
}

fn generic_def_item(def: GenericDefId) -> ItemId {
    match def {
        GenericDefId::AdtId(it) => ItemId::Adt(it),
        GenericDefId::ConstId(it) => ItemId::Const(it),
        GenericDefId::FunctionId(it) => ItemId::Function(it),
        GenericDefId::ImplId(it) => ItemId::Impl(it),
        GenericDefId::StaticId(it) => ItemId::Static(it),
        GenericDefId::TraitId(it) => ItemId::Trait(it),
        GenericDefId::TypeAliasId(it) => ItemId::Alias(it),
    }
}

fn signature(db: &Db, item: ItemId) -> FactRef {
    item_fact(db, items::SIGNATURE, item)
}

/// A lane items fact of one item and no object.
fn item_fact(db: &Db, kind: blaze_query::QueryKind, item: ItemId) -> FactRef {
    FactRef { kind, subject: key::item_key(db, item).0, object: FactObject::None }
}

fn whole_crate(db: &Db, krate: Crate) -> FactRef {
    FactRef { kind: items::CRATE, subject: key::crate_key(db, krate), object: FactObject::None }
}

/// Whether `subject` is the key of `def`'s owner or of the impl or trait that holds it, whose
/// signature hashes the identity has (section 3.2). A read whose value is a leak the signature
/// hash marks (a hidden type: `rpit_hidden_types`) must never reach here as a `Signature` fact.
fn is_owner(db: &Db, def: DefWithBodyId, subject: ContentId) -> bool {
    let (owner, parent) = owner_keys(db, def);
    subject == owner || Some(subject) == parent
}

/// Whether `subject` is the key of the item `def` is the body of, whose attributes the
/// identity hashes (`identity::own_attrs`): not a variant's discriminant's enum.
fn is_own_attrs(db: &Db, def: DefWithBodyId, subject: ContentId) -> bool {
    !matches!(def, DefWithBodyId::VariantId(_)) && subject == owner_keys(db, def).0
}

#[blaze_query::tracked(returns(copy))]
fn owner_keys(db: &Db, def: DefWithBodyId) -> (ContentId, Option<ContentId>) {
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

/// `key` as a `T`, if its page holds `T`s.
fn arg<T: FromIdIn>(db: &Db, key: Id) -> Option<T> {
    T::from_id_in(db.storage(), key)
}
