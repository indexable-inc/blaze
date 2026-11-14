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
//! Name resolution has no boundary of its own (hir-def keeps a module's scope in the value of
//! the query that collected its crate's def map, which reads the crate's files whole): its
//! reads flatten into the step as the files' text input rows, each refused by its kind's name.
//! So a step that resolves a name of its own crate is refused until hir-def has name-grained
//! boundaries again (port report, decision needed).

use base_db::{Crate, Db};
use blaze_ids::ContentId;
use blaze_items::{
    ItemId,
    goals::{self, FactRefKey, Goals},
    key, kinds as items,
};
use blaze_memo::{Builder, KindId, Memo, Read};
use hir_def::{
    AdtId, AttrDefId, BlockIdLt, DefWithBodyId, ExpressionStoreOwnerId, GenericDefId, HasModule,
    ItemContainerId, Lookup, ModuleId, TraitId, TypeAliasId,
    hir::{ClosureKind, CoroutineKind},
    kinds as def,
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
    BOUNDARIES.iter().map(|row| row.kind).chain(goals::QUERIES.iter().copied())
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
    ty::explicit_implied_clauses_of | ty::explicit_super_clauses_of => |a, (it,)| {
        a.signature(ItemId::Trait(it))
    },
    def::VariantFields_field_visibilities => |a, (it,)| a.signature(ItemId::Adt(it.adt_id(a.db))),

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

    // One fact of one item.
    ty::destructor => |a, (it,)| a.item(items::DROP_CHECK, ItemId::Adt(it)),
    ty::orphan_rules_hold => |a, (it,)| a.item(items::COHERENCE, ItemId::Impl(it)),
    ty::builtin_derive_predicates => |a, (it,)| {
        a.item(items::DERIVE, ItemId::Adt(it.loc(a.db).adt))
    },
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
    // its `CrateRoot` fact, its `Crate` fact, or lane items' fact.
    def::lang_items
    | def::crate_recursion_limit
    | def::UnstableFeatures_query
    | ty::loaded_crates => |a, (krate,)| {
        a.own_or(krate, || a.fact(values::crate_root(a.db, krate)))
    },
    goals::goal_crate_key | key::crate_key | def::crate_lang_items => |a, (krate,)| {
        a.own_or(krate, || a.fact(whole_crate(a.db, krate)))
    },
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
    hir_ty_matchck::match_check => |a, (body,)| match body == a.def {
        true => Classified::Fact(values::match_check()),
        false => a.refused(),
    },

    // A step of a body.
    ty::infer_body_query => |a, (body,)| a.step(StepOf::of(a.def, body), Step::Infer),
    ty::mir_body_query => |a, (body,)| a.step(StepOf::of_infer(a.db, a.def, body), Step::Mir),
    ty::mir_body_for_closure_query => |a, (closure,)| {
        a.step(StepOf::of_closure(a.db, a.def, closure), Step::Mir)
    },
    ty::mir_body_for_coroutine_query => |a, (coroutine,)| {
        a.step(StepOf::of_coroutine(a.db, a.def, coroutine), Step::Mir)
    },

    // A body's syntax: covered when it is the body's own, as the key hashes the item's own
    // text; refused for another body's.
    def::item_body_syntax | def::placed_body_syntax => |a, (body,)| match body == a.def {
        true => Classified::Covered,
        false => a.refused(),
    },
};

/// What `read`, a row of the list of `step` of `def`, stands for. A row no rule answers is
/// refused by its kind's name.
pub fn fact_of(db: &Db, def: DefWithBodyId, step: Step, read: Read) -> Classified {
    if let Read::Input { kind, .. } = read {
        return input(kind);
    }
    let asker = Asker { db, def, own: def.krate(db), step, read };
    BOUNDARIES.iter().find_map(|row| (row.answer)(&asker)).unwrap_or_else(|| asker.items())
}

/// An input's row: the stored answer a hit took is covered (its fact is the step's own base
/// key); the field order plan is the build's `FieldOrder` fact. Every other input row (a
/// fact read around the inputs, an untracked read, a served image entry's fact, a file's
/// text) is refused by its kind's name, as the classifier refused a read no rule named. The
/// body's own file text is never a row here: the body's syntax is a boundary, so its text is
/// behind it.
fn input(kind: KindId) -> Classified {
    if kind == STORED_ANSWER {
        return Classified::Covered;
    }
    if kind == KindId::of(FieldOrder::INPUT_KIND) {
        return Classified::Fact(values::field_order());
    }
    Classified::Refused(kind.name())
}

/// What a `const_eval_query` row of a generic evaluation is refused as: the classifier read it
/// through, and a boundary has no list of its own in the asker's.
pub const GENERIC_CONST_EVAL: &str =
    "hir_ty::consteval::const_eval_query: a generic evaluation, read through before";

/// What a step row of an inline constant of the body's own is refused as: the classifier read
/// it through, and a step query is a boundary.
pub const OWN_CONST_STEP: &str = "a step of an inline constant of the body's own, read through before";

/// Who asks what a row stands for: the body, its crate and the step whose list holds `read`.
pub struct Asker<'db> {
    db: &'db Db,
    def: DefWithBodyId,
    own: Crate,
    step: Step,
    read: Read,
}

impl<'db> Asker<'db> {
    fn memo(&self) -> &'db Memo {
        blaze_memo::Database::memo(self.db)
    }

    /// The row refused by its kind's name.
    fn refused(&self) -> Classified {
        Classified::Refused(self.read.kind().name())
    }

    /// `fact`, with the body's own key as its subject made relative (`values::relative`).
    fn fact(&self, fact: FactRef) -> Classified {
        Classified::Fact(values::relative(self.db, self.def, fact))
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

    /// Lane items' fact of the row as the body's crate sees it; refused by the name lane
    /// items gives where it has none.
    fn items(&self) -> Classified {
        match Goals.fact_of(self.db, Some(self.own), self.read) {
            Ok(fact) => self.fact(fact),
            Err(name) => Classified::Refused(name),
        }
    }

    /// Covered for a table of the body's own crate, whose identity hashes it; `other` for
    /// another crate's.
    fn own_or(&self, krate: Crate, other: impl FnOnce() -> Classified) -> Classified {
        match krate == self.own {
            true => Classified::Covered,
            false => other(),
        }
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
            StepOf::Own if self.step.reads(earlier) => {
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
    fn of(def: DefWithBodyId, body: DefWithBodyId) -> StepOf {
        match body == def {
            true => StepOf::Own,
            false => StepOf::Other(body),
        }
    }

    fn of_infer(db: &Db, def: DefWithBodyId, body: InferBodyId<'_>) -> StepOf {
        match body {
            InferBodyId::DefWithBodyId(body) => StepOf::of(def, body),
            InferBodyId::AnonConstId(it) => match it.loc(db).owner {
                ExpressionStoreOwnerId::Body(body) if body == def => StepOf::OwnConst,
                _ => StepOf::Not,
            },
        }
    }

    /// A closure's or coroutine's MIR is in its body's step value only for the kinds
    /// `body_store::mir::owners` lists; and a constant's closure is in no step value.
    fn covered(db: &Db, def: DefWithBodyId, owner: InferBodyId<'_>, yes: bool) -> StepOf {
        match (StepOf::of_infer(db, def, owner), yes) {
            (it @ (StepOf::Own | StepOf::Other(_)), true) => it,
            _ => StepOf::Not,
        }
    }

    fn of_closure(db: &Db, def: DefWithBodyId, closure: InternedClosureId<'_>) -> StepOf {
        let loc = closure.loc(db);
        let kind = matches!(
            loc.kind,
            ClosureKind::Closure | ClosureKind::CoroutineClosure(CoroutineKind::Async)
        );
        StepOf::covered(db, def, loc.owner, kind)
    }

    fn of_coroutine(db: &Db, def: DefWithBodyId, coroutine: InternedCoroutineId<'_>) -> StepOf {
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
}
