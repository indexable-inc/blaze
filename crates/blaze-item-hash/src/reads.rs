//! What each read a body's step makes stands for (docs/v2/plan/design/item-hash.md section 6):
//! covered by the body's identity, one fact, or refused. A step with a refused read is not
//! stored. The audit behind each rule, and the reads refused on purpose, are in
//! docs/v2/plan/design/item-hash.md.
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
    AdtId, AssocItemId, AttrDefId, BlockIdLt, BuiltinDeriveImplId, DefWithBodyId, EnumId,
    EnumVariantId,
    ExpressionStoreOwnerId, FunctionId, GenericDefId, HasModule, ImplId, ItemContainerId, Lookup,
    ModuleId, StaticId, TraitId, TypeAliasId, VariantId,
    hir::{ClosureKind, CoroutineKind},
    nameres::{NameRead, name_read},
};
use hir_ty::{
    InferBodyId,
    body_store::{Classified, Step},
    db::{InternedClosureId, InternedCoroutineId},
    generics::generics,
    method_resolution::{
        InherentImpls_in_block_for_self_ty, InherentImpls_in_crate_for_self_ty, SelfTyKey,
        TraitImpls_blanket_in_crate_and_deps, TraitImpls_for_crate_and_trait,
        TraitImpls_in_block_for_trait_and_self_ty,
        TraitImpls_in_crate_and_deps_for_trait_and_self_ty,
        TraitImpls_in_crate_for_trait_and_self_ty, impl_value_item, in_crate_trait_value_items,
        inherent_value_item, loaded_crates, scope_sets::module_items_named, trait_value_item,
    },
    next_solver::goal_store::{FactObject, FactRef, GoalFacts},
};

use crate::values;

/// What a read of one ingredient stands for. Each rule says what its key is; a key of another
/// type (a supertype's other variant) is refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Rule {
    /// What name resolution reads (the def maps, module scopes and their names, the traits in
    /// scope): each is the fact `hir_def::nameres::name_read` says it was a read of
    /// ([`name_fact`]): a name looked up, found or not, is its `Resolves` fact, whose value
    /// for a name the scope lacks is "nothing", so an item that later supplies or shadows the
    /// name changes it.
    Names,
    /// Keyed by a body, as a `DefWithBodyId` or its `InferBodyId`: refused for another's.
    /// The body's own, whose key is made of the item's text and so covers nothing the
    /// lowering resolved:
    /// - read by the lowering step itself (`Step::Lower`): read through, so what the lowering
    ///   read (its names, macros, the item's own syntax) is each a fact or covered by its own
    ///   rule, and anything else refuses the lowering by name;
    /// - read by any other step, a row the lowering reads and nothing else does
    ///   (`Body::of`, `Body::has_lowering_errors`, the lowering itself; [`Rule::OwnStore`]'s
    ///   closures): the body's `STEP_LOWER` fact, valued by the digest of what the lowering
    ///   read, so it changes exactly when something the lowering read does;
    /// - any other such row (the scopes, the upvars, failed expansions, line literals, which
    ///   read more than the lowering): read through, each of its reads by its own rule.
    OwnBody,
    /// A row of a body's own syntax that its lowering reads (`hir_def::expr_store::body`'s
    /// `file_body_syntax`, an item's `syntax`, `placed_body_syntax`): covered when it is the
    /// body's own (`own_syntax_read`; `placed_body_syntax` keyed by the body itself), as the
    /// key hashes the item's text; refused for another's.
    OwnSyntax,
    /// A row of the body's own that only its lowering step may read: `Body::relative`, which
    /// `has_lowering_errors` reads for the diagnostics of its source map. Its spans are
    /// relative to the body's anchor, and the lowering's answer holds none of them. Read
    /// through for the lowering step of the body itself; refused for any other step and any other body: a position reaching
    /// an answer is what no fact covers.
    LowerOnly,
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
    /// items, or of it and its dependencies. A read of the body's crate or a crate it loads is
    /// a fact per item as the body's crate sees it ([`own_item_read`]): the impls of one trait
    /// for one simplified self type, or its blanket impls, are lane items' `Implements { trait,
    /// self type }` fact; the inherent impls of one type its `InherentImpls`; all the impls of
    /// one trait its `TraitImpls`; the traits with a value item of one name its `TraitsNamed`;
    /// and a module's traits in scope with an item of one name its `ScopeTraitsNamed`, of any
    /// crate. Each is valued by what it lists, each by its own key and signature, in every
    /// crate the body's crate sees, so an item of it added or removed anywhere there (a `const
    /// _` block's included) changes it, and nothing else does. Any other read of another crate
    /// is that crate's `Crate` fact, which hashes it and every crate it depends on; any other
    /// read of the body's own crate is refused, naming the read.
    DepCrate(Args),
    /// Keyed by a block (`TraitImpls::for_block`, `InherentImpls::for_block`) or a block first
    /// (`InherentImpls::in_block_for_self_ty`, `TraitImpls::in_block_for_trait_and_self_ty`):
    /// lane items' `BlockImpls` fact of the block, each impl there by key and signature, for
    /// the body's own block as for another's. The own block's is not covered: the key hashes
    /// the body's text, not what its paths resolve to, and an impl in a block is an item of the
    /// block's item tree, whose paths (its trait, its self type) are not the body's. A block of
    /// no body's (a signature's array length) is refused.
    Block,
    /// Keyed by an impl or a trait and a name: its item of that name, of its `Fields` fact as
    /// `ImplItems::of` and `TraitItems` are.
    FieldNamed(Of),
    /// `closure_exprs(owner, ())`: covered for the body's own store (its closures are of its
    /// HIR), refused for another's.
    OwnStore,
    /// `const_eval_query(const, args, env)`: the const's `ConstValue` fact when it is the
    /// evaluation lane items hashes, of a const without generics, with no arguments and no
    /// environment. Any other (an associated const in generic code, evaluated with arguments or
    /// in an environment) is read through ([`Classified::Through`]), as [`Rule::Through`]'s
    /// rows are and for the same reason: `const_eval_query` is a tracked function, so its kept
    /// reads are every input its value has besides its key; its key (the const, the arguments,
    /// the environment) is made by the step from what the step read; and what it reads is the
    /// const's own inference and MIR (another body's step, [`Rule::OwnStep`]'s fact of that
    /// body), the callees' MIR the interpreter runs (the same), layouts and solver goals (read
    /// through again). The one input it has outside the engine, a dependency's kept value
    /// (`consteval::deps::cached`), is taken only with no arguments, and its read of the kept
    /// set (`KeptConsts`) has no rule, so such a row is refused by that name, never kept
    /// under facts that do not cover the store's answer.
    ConstEval,
    /// A step of a body, as a read of `infer_body_query`, `mir_body_query` or the MIR of a
    /// closure or coroutine. Of the body itself (its inference; its MIR, or that of a closure
    /// or an async coroutine written in it): that step's fact, the body named as
    /// `values::OWN`, for a later step of the same body; refused for the same or a later step,
    /// and for a coroutine kind whose MIR the step's value leaves out. Of another body (a
    /// `const fn` evaluated, a closure's signature, a coroutine's witnesses): that body's
    /// step fact by its key, whose value is the hash of its answer in the item store's
    /// encoding (`body_store::step_value`), equal in two databases exactly when the answers
    /// are, so a change to that body that changes its answer misses the reader, and no other
    /// does. Of an inline or array-length constant written in the body itself: read through,
    /// its row's reads classified for this body (its own text, its paths, the facts its
    /// inference and MIR read), as it is part of the body's text and its own inference and
    /// MIR are tracked functions with every input kept. Refused for a constant of another
    /// body or of a signature, whose paths are not the body's.
    OwnStep(Step),
    /// `inherent_assoc_types(adt)`: the ADT's `InherentAssocTypes` fact
    /// ([`values::inherent_assoc_types`]), whose value is the query's whole result by item
    /// key. Not read through: the query iterates every module of the ADT's crate.
    InherentAssocTypes,
    /// `match_check(def)` of the body itself, which the borrow check reads to skip a body
    /// whose patterns failed: its `MatchCheck` fact. Refused for another body's.
    OwnMatchCheck,
    /// `module_trait_set(module)`: as [`Rule::Names`], the module's `TraitsInScope` fact.
    TraitsInScope,
    /// `Modules`' `table`, every module of a crate: what `name_read` says of it
    /// (`NameRead::AllModules`, a block's `WholeScope`) in either mode, a [`Classified::Scan`].
    /// Never covered: an iteration of every module is no name the body looks up.
    AllModules,
    /// A row whose value is a function of what it read and nothing else, with no fact of its
    /// own: a type's layout (`layout_of_ty_query`, `layout_of_adt_query`, keyed by a type and
    /// an environment, which no fact names and whose interned ids are one process's), and the
    /// derived tables a layout reads (a struct's flags, an enum's discriminants, a `bits`
    /// struct's packing, a field's bit width, the plan's ranks of an ADT's fields). The store
    /// reads the row's own reads in its place (`Classified::Through`), each by its own rule
    /// here, through again where that is `Through`: what decides a layout is traced as facts
    /// keyed by items and crates. A read below a layout row is classified
    /// for the same body and step as one the step made itself; a layout row reads no
    /// name-resolution row of its own (it asks queries that do: field types, attributes); a
    /// [`Rule::Names`] read below one is a fact as anywhere else.
    ///
    /// The same holds of the queries that run the trait solver in an inference context of
    /// their own, keyed by a type, a goal or a trait method and an environment (the borrow
    /// check's `is_copy_query`, `drop_glue_query`, `prove_query`, `normalize_query`,
    /// `normalizes_to_error_query`; the solver's shape table, `copy_clone_impl` and
    /// `auto_trait_depth`; codegen's and const eval's `lookup_impl_method_stored`), and of the
    /// borrow check's tables of one ADT
    /// (`drop_constraint`). Why reading through them is sound (keys-cover-every-input): each
    /// is a tracked function, so its kept reads are every input its value has besides its key
    /// (the engine reruns it on nothing else); the solver records a goal it takes from the
    /// shared cache by replaying the goal's reads into the row, and one from a query's own
    /// cache was solved by that row, so its reads are among the row's
    /// (`next_solver::interner`, `with_cached_task`, `get_tracked`); the key is made by the
    /// step from what the step read, so the step's other facts cover it. Each read below is
    /// then a fact keyed by an item (the impls of a trait for a self type, `Implements`; an
    /// impl's or a field's signature; an ADT's `DropCheck`; a const's value) and a change to
    /// any of them misses the step, and a change to none of them leaves the row's value as it
    /// was. A fact of its own would be coarser and unsound: `Implements { Copy, W }` alone does
    /// not say whether `W<T>: Copy`, which needs `T: Copy` too, nor what the environment's
    /// where clauses prove. The solver resolves no path, so [`Rule::Names`] is met below none
    /// of them but `path_bindings`, whose reads are of the body's own paths and scopes, each
    /// a name fact.
    Through,
    /// The field order plan's input (`hir_ty::layout::field_order::FieldOrder`), which every
    /// ADT's layout reads: the build's `FieldOrder` fact.
    FieldOrder,
    /// A field of the front end's settings (`base_db::FrontSettings`), one for the build and
    /// the same for every body: the build's `Setting` fact of that field, by its name.
    Setting,
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
    /// An `AdtId`.
    Adt,
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
    ("AttrFlags::repr_assume_has", "hir-def/src/attrs.rs", Rule::Attrs(Of::Adt)),
    ("BlockIdLt", "hir-def/src/lib.rs", Rule::Names),
    ("Body::has_lowering_errors", "hir-def/src/expr_store/body.rs", Rule::OwnBody),
    ("Body::of", "hir-def/src/expr_store/body.rs", Rule::OwnBody),
    ("Body::relative", "hir-def/src/expr_store/body.rs", Rule::LowerOnly),
    ("ConstSignature::kept", "hir-def/src/signatures.rs", Rule::Signature(Of::Const)),
    // A signature lowered again with its source map (`relative`, `with_source_map`): the
    // signature is `kept`'s (`Kept::with_map`), which the `Signature` fact hashes; the map is
    // where each part is written, and no answer holds a position (inference and MIR name
    // expressions and type references by id, `mir::persist`'s spans are ids, a location
    // literal is a slot), so nothing of it reaches a kept value.
    ("ConstSignature::relative", "hir-def/src/signatures.rs", Rule::Signature(Of::Const)),
    ("ConstSignature::with_source_map", "hir-def/src/signatures.rs", Rule::Signature(Of::Const)),
    ("CrateResolution::module", "hir-def/src/nameres/delta/publish.rs", Rule::Names),
    ("DefMapPair", "hir-def/src/nameres.rs", Rule::Names),
    ("EnumSignature::kept", "hir-def/src/signatures.rs", Rule::Items),
    ("EnumVariants::of", "hir-def/src/signatures.rs", Rule::Fields(Of::Enum)),
    ("ExprScopes::body_expr_scopes", "hir-def/src/expr_store/scope.rs", Rule::OwnBody),
    ("FunctionSignature::kept", "hir-def/src/signatures.rs", Rule::Items),
    ("FunctionSignature::relative", "hir-def/src/signatures.rs", Rule::Signature(Of::Function)),
    ("FunctionSignature::with_source_map", "hir-def/src/signatures.rs", Rule::Signature(Of::Function)),
    ("ImplItems::of", "hir-def/src/nameres/assoc.rs", Rule::Fields(Of::Impl)),
    ("ImplSignature::kept", "hir-def/src/signatures.rs", Rule::Items),
    ("InherentImpls::for_block", "hir-ty/src/method_resolution.rs", Rule::Block),
    ("InherentImpls::for_crate", "hir-ty/src/method_resolution.rs", Rule::WholeCrate),
    ("InherentImpls::for_module", "hir-ty/src/method_resolution.rs", Rule::ModuleCrate),
    ("InherentImpls::in_block_for_self_ty", "hir-ty/src/method_resolution.rs", Rule::Block),
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
    ("StaticSignature::relative", "hir-def/src/signatures.rs", Rule::Signature(Of::Static)),
    ("StaticSignature::with_source_map", "hir-def/src/signatures.rs", Rule::Signature(Of::Static)),
    ("StructSignature::kept", "hir-def/src/signatures.rs", Rule::Items),
    ("TraitImpls::blanket_in_crate_and_deps", "hir-ty/src/method_resolution.rs", Rule::DepCrate(Args::Trait)),
    ("TraitImpls::for_block", "hir-ty/src/method_resolution.rs", Rule::Block),
    ("TraitImpls::for_crate", "hir-ty/src/method_resolution.rs", Rule::WholeCrate),
    ("TraitImpls::for_crate_and_deps", "hir-ty/src/method_resolution.rs", Rule::WholeCrate),
    ("TraitImpls::for_crate_and_trait", "hir-ty/src/method_resolution.rs", Rule::DepCrate(Args::Trait)),
    ("TraitImpls::for_module", "hir-ty/src/method_resolution.rs", Rule::ModuleCrate),
    ("TraitImpls::in_block_for_trait_and_self_ty", "hir-ty/src/method_resolution.rs", Rule::Block),
    ("TraitImpls::in_crate_and_deps_for_trait_and_self_ty", "hir-ty/src/method_resolution.rs", Rule::DepCrate(Args::TraitSelfTy)),
    ("TraitImpls::in_crate_for_trait_and_self_ty", "hir-ty/src/method_resolution.rs", Rule::DepCrate(Args::TraitSelfTy)),
    ("TraitItems::query_with_diagnostics", "hir-def/src/nameres/assoc.rs", Rule::Fields(Of::Trait)),
    ("TraitSignature::kept", "hir-def/src/signatures.rs", Rule::Items),
    ("TypeAliasSignature::kept", "hir-def/src/signatures.rs", Rule::Signature(Of::Alias)),
    ("TypeAliasSignature::relative", "hir-def/src/signatures.rs", Rule::Signature(Of::Alias)),
    ("TypeAliasSignature::with_source_map", "hir-def/src/signatures.rs", Rule::Signature(Of::Alias)),
    ("UnionSignature::kept", "hir-def/src/signatures.rs", Rule::Items),
    ("UnstableFeatures::query", "hir-def/src/unstable_features.rs", Rule::OwnCrate(Else::CrateRoot)),
    ("VariantFields::field_visibilities", "hir-def/src/visibility.rs", Rule::Signature(Of::Variant)),
    ("VariantFields::kept", "hir-def/src/signatures.rs", Rule::Fields(Of::Variant)),
    // What dropping an ADT needs live, walked to a recursion limit: its fields' types, its
    // `Drop` impl (`destructor`, the ADT's `DropCheck`) and that impl's arguments, and the
    // solver's drop glue of each field, each a fact read through.
    ("adt_dtorck_constraint", "hir-ty/src/dropck.rs", Rule::Through),
    // Whether an ADT with its identity arguments is an auto trait by the shape walk, in a
    // crate: the walk reads the ADT's impls of the trait (`Implements`), its fields'
    // types and the impls' where clauses, each a fact read through.
    // An anonymous constant evaluated (an array length, a const argument, a `const { .. }`
    // block) with arguments and an environment: a tracked function, read through as
    // `const_eval_query` with arguments is ([`Rule::ConstEval`]). Its reads are the constant's
    // inference and MIR, read through again when the constant is written in the body itself
    // and refused when it is another body's or a signature's ([`Rule::OwnStep`]), and what the
    // interpreter reads (callees' MIR, another body's step fact; layouts and goals, through).
    ("anon_const_eval_query", "hir-ty/src/consteval.rs", Rule::Through),
    ("auto_trait_depth", "hir-ty/src/next_solver/global_goals/shape.rs", Rule::Through),
    ("bit_width_of", "hir-ty/src/layout/bits.rs", Rule::Through),
    ("bits_layout_query", "hir-ty/src/layout/bits.rs", Rule::Through),
    ("blaze_bits", "hir-def/src/attrs.rs", Rule::Attrs(Of::AttrOwner)),
    ("block_def_map", "hir-def/src/nameres.rs", Rule::Names),
    ("body_upvars_mentioned", "hir-def/src/upvars.rs", Rule::OwnBody),
    // Whether the borrow check prints its regions (`BLAZE_BORROWCK_DEBUG`): the build's
    // setting, a fact of its own, so a verdict kept without it is checked again, and printed,
    // with it.
    ("borrowck_debug", "base-db/src/settings.rs", Rule::Setting),
    ("cfg_options", "base-db/src/input.rs", Rule::CrateInput),
    ("children", "hir-def/src/nameres.rs", Rule::Names),
    ("children", "hir-def/src/nameres/delta/publish.rs", Rule::Names),
    ("closure_exprs", "hir-ty/src/db.rs", Rule::OwnStore),
    ("const_conditions_query", "hir-ty/src/lower.rs", Rule::Items),
    ("const_eval_discriminant_variant", "hir-ty/src/consteval.rs", Rule::ConstValue),
    ("const_eval_query", "hir-ty/src/consteval.rs", Rule::ConstEval),
    ("const_eval_static_query", "hir-ty/src/consteval.rs", Rule::ConstValue),
    ("const_param_types_with_diagnostics", "hir-ty/src/lower.rs", Rule::Signature(Of::GenericDef)),
    // Whether a non-generic ADT has one plain impl of `Copy` or `Clone`, in a crate: it reads
    // the impls of the trait for the ADT (`Implements`) and that impl's signature.
    ("copy_clone_impl", "hir-ty/src/next_solver/global_goals/shape.rs", Rule::Through),
    // The types an `async` block holds across a yield (an auto-trait goal on a future): a
    // tracked function of the coroutine, whose reads are the MIR of the coroutine's body
    // (another body's `STEP_MIR` fact, [`Rule::OwnStep`]; the body's own is refused below
    // its MIR step), the solver's goals and the layouts it meets (read through again). Its
    // key, the interned coroutine, came to the step through what the step read (a hidden
    // type, `rpit_hidden_types`, a leak fact).
    ("coroutine_witnesses_query", "hir-ty/src/providers.rs", Rule::Through),
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
    // What dropping an ADT needs of the regions: its `Drop` impl (`destructor`, `DropCheck`),
    // that impl's generics, and its fields' types, each a fact read through.
    ("drop_constraint", "hir-ty-borrowck/src/liveness.rs", Rule::Through),
    // Whether dropping a type runs code, in an environment: the solver normalizing it (the
    // impls of the traits whose associated types it names), each ADT's `Drop` impl
    // (`destructor`, the ADT's `DropCheck`), its flags and its fields' types, through every
    // field and element. A new `Drop` impl changes the `DropCheck` of its ADT, and so the
    // step; another ADT's does not.
    ("drop_glue_query", "hir-ty-borrowck/src/lib.rs", Rule::Through),
    ("dyn_compatibility_of_trait_query", "hir-ty/src/dyn_compatibility.rs", Rule::Items),
    // The groups of a function's elided lifetimes: of its signature as written.
    ("elided_groups", "hir-ty/src/lower.rs", Rule::Signature(Of::Function)),
    ("elided_lifetimes", "hir-ty/src/lower.rs", Rule::Signature(Of::Function)),
    ("enum_discriminants", "hir-ty/src/consteval/discriminants.rs", Rule::Through),
    ("enum_variant_ids", "hir-ty/src/layout.rs", Rule::Fields(Of::Enum)),
    ("explicit_discriminant", "hir-ty/src/consteval/discriminants.rs", Rule::ConstValue),
    ("explicit_discriminant_indices", "hir-ty/src/consteval/discriminants.rs", Rule::Through),
    // A trait's where clauses on `Self` and its associated types, and its supertraits: of its
    // header, which its signature hashes (as `generic_predicates_of` and `supertraits_info`).
    ("explicit_implied_clauses_of", "hir-ty/src/lower.rs", Rule::Signature(Of::Trait)),
    ("explicit_super_clauses_of", "hir-ty/src/lower.rs", Rule::Signature(Of::Trait)),
    ("extra_data", "base-db/src/input.rs", Rule::CrateInput),
    ("fact_value", "blaze-items/src/goals.rs", Rule::FactValue),
    ("failed_expansion", "hir-ty-mir/src/mir/lower.rs", Rule::OwnBody),
    ("field_ranks", "hir-ty/src/layout/field_order.rs", Rule::Through),
    ("field_types_of_variant", "hir-ty/src/lower.rs", Rule::Items),
    ("file_body_syntax", "hir-def/src/expr_store/body.rs", Rule::OwnSyntax),
    ("fn_sig_for_enum_variant_constructor", "hir-ty/src/lower.rs", Rule::Items),
    ("fn_sig_for_fn", "hir-ty/src/lower.rs", Rule::Items),
    ("fn_sig_for_struct_constructor", "hir-ty/src/lower.rs", Rule::Items),
    ("generic_defaults_with_diagnostics", "hir-ty/src/lower.rs", Rule::Signature(Of::GenericDef)),
    ("generic_predicates_of", "hir-ty/src/lower.rs", Rule::Items),
    ("generics_require_sized_self", "hir-ty/src/dyn_compatibility.rs", Rule::Signature(Of::GenericDef)),
    ("goal_crate_key", "blaze-items/src/goals.rs", Rule::OwnCrate(Else::WholeCrate)),
    ("impl_has_unconstrained_params", "hir-ty/src/specialization.rs", Rule::Items),
    ("impl_rpitit_tys", "hir-ty/src/rpitit.rs", Rule::Items),
    ("impl_self_ty_of_impl", "hir-ty/src/lower.rs", Rule::Items),
    // The outlives clauses an impl's trait reference implies: the impl's trait reference
    // (`impl_trait_of_impl`) and the trait's super clauses, each a fact read through.
    ("impl_super_outlives", "hir-ty/src/next_solver/util.rs", Rule::Through),
    ("impl_trait_of_impl", "hir-ty/src/lower.rs", Rule::Items),
    ("impl_type_aliases", "hir-ty/src/opaques.rs", Rule::Fields(Of::Impl)),
    ("impl_value_item", "hir-ty/src/method_resolution.rs", Rule::FieldNamed(Of::Impl)),
    ("in_crate_trait_value_items", "hir-ty/src/method_resolution.rs", Rule::DepCrate(Args::Name)),
    ("infer_body_query", "hir-ty/src/infer.rs", Rule::OwnStep(Step::Infer)),
    // The outlives requirements an ADT's fields infer for its parameters: its fields' types and
    // the requirements of the ADTs they name, each a fact read through.
    ("inferred_outlives", "hir-ty-wfcheck/src/outlives.rs", Rule::Through),
    // `Type::Name` of an ADT: the associated types of its inherent impls, a list made by
    // iterating every module of its crate (`Modules::table` and each scope's impls), which
    // no lookup fact covers: a fact of the list itself.
    ("inherent_assoc_types", "hir-ty/src/lower/path.rs", Rule::InherentAssocTypes),
    ("inherent_value_item", "hir-ty/src/method_resolution.rs", Rule::FieldNamed(Of::Impl)),
    // Which of an impl's parameters are arbitrary-width integers: its trait reference and
    // where clauses (`impl_trait_of_impl`, `generic_predicates_of`) and its crate's lang items,
    // each a fact read through.
    ("int_gates", "hir-ty/src/arbitrary_ints.rs", Rule::Through),
    // Whether a type is `Copy` in an environment: the solver's reads, the impls of `Copy`
    // for each type shape it meets (`Implements { Copy, self_ty }`, the blanket impls'
    // too) with their where clauses, the nested goals those give (`T: Copy` of `W<T>`), and
    // the environment's clauses, which the step's `trait_environment_query` read gave. A
    // new `impl Copy` for a shape the goal met changes that `Implements`, and so the step.
    ("is_copy_query", "hir-ty-borrowck/src/lib.rs", Rule::Through),
    ("lang_items", "hir-def/src/lang_item.rs", Rule::OwnCrate(Else::CrateRoot)),
    ("layout_of_adt_query", "hir-ty/src/layout/adt.rs", Rule::Through),
    ("layout_of_ty_query", "hir-ty/src/layout.rs", Rule::Through),
    ("legacy_macro_memo", "hir-def/src/nameres/frozen/pages.rs", Rule::Names),
    ("line_literals", "hir-ty-mir/src/mir/location.rs", Rule::OwnBody),
    ("loaded_crates", "hir-ty/src/method_resolution.rs", Rule::OwnCrate(Else::CrateRoot)),
    // The impl method a trait method with arguments selects in an environment: the
    // solver's reads (the trait's impls for the self type, `Implements`, their where
    // clauses), the selected impl's items (`ImplItems::of`, `Fields`), and with opaque
    // types revealed, their hidden types (`rpit_hidden_types`, a leak fact).
    ("lookup_impl_method_stored", "hir-ty/src/method_resolution.rs", Rule::Through),
    // The body's lowering step (`hir_ty::body_store::lowering`): as its rows, `Body::of`'s.
    ("lowering", "hir-ty/src/body_store/lower.rs", Rule::OwnBody),
    ("macros_writing_impls", "hir-ty/src/method_resolution.rs", Rule::WholeCrate),
    ("match_check", "hir-ty-matchck/src/lib.rs", Rule::OwnMatchCheck),
    ("may_specialize", "hir-ty/src/specialization.rs", Rule::Items),
    ("mir_body_for_closure_query", "hir-ty/src/providers.rs", Rule::OwnStep(Step::Mir)),
    ("mir_body_for_coroutine_query", "hir-ty/src/providers.rs", Rule::OwnStep(Step::Mir)),
    ("mir_body_query", "hir-ty/src/providers.rs", Rule::OwnStep(Step::Mir)),
    ("module", "hir-def/src/nameres/delta/publish.rs", Rule::Names),
    ("module_impls", "hir-def/src/nameres/facts/impls.rs", Rule::ModuleCrate),
    ("module_items_named", "hir-ty/src/method_resolution/scope_sets.rs", Rule::DepCrate(Args::ModuleName)),
    ("module_trait_set", "hir-ty/src/method_resolution/scope_sets.rs", Rule::TraitsInScope),
    ("name_memo", "hir-def/src/nameres/frozen/pages.rs", Rule::Names),
    // An associated type normalized in an environment, with the region constraints that
    // records: the impl that gives it (the impls of its trait for the self type,
    // `Implements`, each with its where clauses and its associated types' values, all in the
    // impl's signature), the nested projections it gives, and the environment's clauses. A
    // changed associated type, or a changed where clause of its impl, changes that
    // `Implements`, and so the step.
    ("normalize_query", "hir-ty-borrowck/src/regions.rs", Rule::Through),
    // Whether a function's signature, deeply normalized in its environment, has an error type
    // in it (rustc's writeback taint): the same reads as `normalize_query`'s, for every
    // projection the signature names.
    ("normalizes_to_error_query", "hir-ty-borrowck/src/regions.rs", Rule::Through),
    // An `impl Trait`'s variances (a goal relating two opaque types): a tracked function of
    // the interned opaque, whose reads are its function's signature (generics, `use<..>`,
    // the opaque's bounds) and its container's trait reference, each a `Signature` or
    // `Items` fact; it resolves no path.
    ("opaque_variances_query", "hir-ty/src/next_solver/opaque_variances.rs", Rule::Through),
    ("orphan_rules_hold", "hir-ty/src/traits.rs", Rule::Coherence),
    // The variable each path of a body's store names: the body's own HIR and scopes
    // (`Body::of`, `ExprScopes`, covered for the body's own and refused for another's) and
    // name resolution of its own paths, each a name fact ([`Rule::Names`]).
    ("path_bindings", "hir-ty-borrowck/src/regions.rs", Rule::Through),
    ("placed_body_syntax", "hir-def/src/expr_store/body.rs", Rule::OwnSyntax),
    ("plan", "hir-ty/src/layout/field_order.rs", Rule::FieldOrder),
    ("predicates", "hir-ty/src/builtin_derive.rs", Rule::Derive),
    // A trait goal proved in an environment, with the region constraints that records: the
    // impls of the trait for the goal's self type shape (`Implements`; when the self type
    // is no shape, a parameter or an alias, the trait's blanket and whole impl set, which
    // the solver reads as `TraitImpls` and `Implements` of no shape), each candidate's where
    // clauses and the nested goals they give, and the environment's clauses. A new impl
    // that makes the goal hold, or a changed one it was proved by, changes one of those
    // facts, and so the step.
    ("prove_query", "hir-ty-borrowck/src/regions.rs", Rule::Through),
    ("relevant_impls", "hir-ty/src/method_resolution.rs", Rule::Items),
    ("representability", "hir-ty/src/representability.rs", Rule::Items),
    ("row", "hir-def/src/nameres/facts/queries.rs", Rule::Names),
    ("rpit_hidden_types", "hir-ty/src/opaques.rs", Rule::Items),
    ("rpitit_item_bounds", "hir-ty/src/rpitit.rs", Rule::Items),
    ("scope", "hir-def/src/nameres.rs", Rule::Names),
    ("scope", "hir-def/src/nameres/delta/publish.rs", Rule::Names),
    ("scope_memo", "hir-def/src/nameres/frozen/pages.rs", Rule::Names),
    ("specializes_query", "hir-ty/src/specialization.rs", Rule::Items),
    ("struct_flags", "hir-ty/src/layout.rs", Rule::Through),
    ("struct_tail_may_be_unsized", "hir-ty/src/layout.rs", Rule::Through),
    ("supertraits_info", "hir-ty/src/lower.rs", Rule::Items),
    ("syntax", "hir-def/src/expr_store/body.rs", Rule::OwnSyntax),
    // The table of every module of a crate: an iteration of all of them, a scan of the
    // crate in either mode (`hir_def::nameres::NameRead::AllModules`).
    ("table", "hir-def/src/nameres.rs", Rule::AllModules),
    ("tait_hidden_types", "hir-ty/src/opaques.rs", Rule::Items),
    ("target_data_layout_query", "hir-ty/src/layout/target.rs", Rule::OwnCrate(Else::Items)),
    ("target_features", "hir-def/src/attrs.rs", Rule::Attrs(Of::Function)),
    ("trait_environment_query", "hir-ty/src/lower.rs", Rule::Signature(Of::GenericDef)),
    ("trait_type_aliases", "hir-ty/src/opaques.rs", Rule::Fields(Of::Trait)),
    ("trait_value_item", "hir-ty/src/method_resolution.rs", Rule::FieldNamed(Of::Trait)),
    ("traits_memo", "hir-def/src/nameres/frozen/pages.rs", Rule::Names),
    ("transitive_deps", "base-db/src/input.rs", Rule::CrateRoot),
    ("type_alias_bounds_with_diagnostics", "hir-ty/src/lower.rs", Rule::Signature(Of::AliasBounds)),
    ("type_for_const_with_diagnostics", "hir-ty/src/lower.rs", Rule::Signature(Of::Const)),
    ("type_for_static_with_diagnostics", "hir-ty/src/lower.rs", Rule::Signature(Of::Static)),
    ("type_for_type_alias_with_diagnostics", "hir-ty/src/lower.rs", Rule::Signature(Of::Alias)),
    ("unstable_feature", "hir-def/src/attrs.rs", Rule::Attrs(Of::AttrOwner)),
    ("variances_of_query", "hir-ty/src/variance.rs", Rule::Items),
    ("variant_field_ids", "hir-ty/src/layout.rs", Rule::Fields(Of::Variant)),
    ("workspace_data", "base-db/src/input.rs", Rule::CrateInput),
];

/// What `read`, made by `step` of `def`, stands for: covered by the body's key, a fact, read
/// through, or refused. A read of name resolution is the fact `hir_def::nameres::name_read`
/// says it was a read of, found or not. `name` is its ingredient's debug name.
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
        Some(rule) => apply(db, def, step, read, kind, name, rule),
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
    kind: ReadKind,
    name: &'static str,
    rule: Rule,
) -> Classified {
    let key = read.key_index();
    let found = match rule {
        Rule::Names | Rule::AllModules => return name_fact(db, def, read, kind),
        Rule::Through => {
            #[cfg(test)]
            if planted::QUERIES_COVERED.get() && planted::QUERIES.contains(&name) {
                return Classified::Covered;
            }
            return Classified::Through;
        }
        Rule::FieldOrder => Some(values::field_order()),
        Rule::Setting => values::setting(name),
        Rule::TraitsInScope => {
            arg::<ModuleId>(db, key).map(|module| values::traits_in_scope(db, module))
        }
        Rule::OwnBody => {
            let own = match arg::<DefWithBodyId>(db, key) {
                Some(body) => body == def,
                None => arg::<InferBodyId<'_>>(db, key) == Some(InferBodyId::DefWithBodyId(def)),
            };
            return match own {
                false => Classified::Refused,
                true => own_lowering(step, name),
            };
        }
        Rule::OwnSyntax => {
            let own = match name {
                "placed_body_syntax" => arg::<DefWithBodyId>(db, key) == Some(def),
                _ => hir_def::expr_store::body::own_syntax_read(db, def, name, key),
            };
            return match own {
                true => Classified::Covered,
                false => Classified::Refused,
            };
        }
        Rule::LowerOnly => {
            return match (step, arg::<DefWithBodyId>(db, key) == Some(def)) {
                (Step::Lower, true) => Classified::Through,
                _ => Classified::Refused,
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
                Else::Items => Goals.fact_of(db, Some(def.krate(db)), read, name),
            },
            None => None,
        },
        Rule::CrateRoot => arg::<Crate>(db, key).map(|krate| values::crate_root(db, krate)),
        Rule::WholeCrate => arg::<Crate>(db, key).map(|krate| whole_crate(db, krate)),
        Rule::ModuleCrate => {
            arg::<ModuleId>(db, key).map(|module| whole_crate(db, module.krate(db)))
        }
        Rule::Items => Goals.fact_of(db, Some(def.krate(db)), read, name),
        Rule::Signature(of) => item_of(db, key, of).map(|item| signature(db, item)),
        Rule::Fields(of) => item_of(db, key, of).map(|item| values::fields(db, item)),
        Rule::Attrs(Of::Function) => {
            arg::<FunctionId>(db, key).and_then(|it| values::attrs(db, it.into()))
        }
        Rule::Attrs(Of::Adt) => arg::<AdtId>(db, key).and_then(|it| values::attrs(db, it.into())),
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
        // The body's value of the fact is its crate's (`Bodies::value`): a value another crate
        // asked of which impls it sees may cover other crates. Any other fact is keyed by no
        // asker (`goals::fact_ref_key`), its value one for every crate.
        Rule::FactValue => match arg::<FactRefKey<'_>>(db, key) {
            Some(it) if it.seen_by(db).is_none_or(|krate| krate == def.krate(db)) => {
                Some(it.fact(db))
            }
            _ => None,
        },
        Rule::DepCrate(of) => match own_item_read(db, def.krate(db), read, name, of) {
            Some(fact) => Some(fact),
            None => match crate_of_args(db, key, of) {
                Some(krate) if krate == def.krate(db) => return Classified::Refused,
                krate => krate.map(|krate| whole_crate(db, krate)),
            },
        },
        Rule::Block => match block_of(db, key).map(|block| block.owner(db)) {
            // A block of no body's (an array length's in a signature) is no body's text.
            None | Some(None) => None,
            Some(Some(_)) => Goals.fact_of(db, Some(def.krate(db)), read, name),
        },
        Rule::FieldNamed(of) => match of {
            Of::Impl => impl_value_item::arguments(db, key)
                .or_else(|| inherent_value_item::arguments(db, key))
                .map(|(it, _)| ItemId::Impl(it)),
            Of::Trait => trait_value_item::arguments(db, key).map(|(it, _)| ItemId::Trait(it)),
            _ => None,
        }
        .map(|item| values::fields(db, item)),
        Rule::OwnStore => {
            return match hir_ty::db::closure_exprs::arguments(db, key) {
                Some((owner, ())) if owner == ExpressionStoreOwnerId::Body(def) => {
                    own_lowering(step, name)
                }
                _ => Classified::Refused,
            };
        }
        Rule::OwnStep(earlier) => match step_of(db, def, key) {
            StepOf::Own if step.reads(earlier) => values::step_fact(earlier),
            StepOf::Own | StepOf::Not => return Classified::Refused,
            StepOf::Other(body) => values::step_fact_of(db, body, earlier),
            StepOf::OwnConst => return Classified::Through,
        },
        Rule::InherentAssocTypes => {
            arg::<AdtId>(db, key).map(|adt| values::inherent_assoc_types(db, adt))
        }
        Rule::OwnMatchCheck => match arg::<DefWithBodyId>(db, key) {
            Some(body) if body == def => Some(values::match_check()),
            _ => None,
        },
        Rule::ConstEval => match hir_ty::consteval::const_eval_query::arguments(db, key) {
            Some((konst, subst, None))
                if subst.as_ref().is_empty()
                    && generics(db, konst.into()).iter(true).next().is_none() =>
            {
                Some(item_fact(db, items::CONST_VALUE, ItemId::Const(konst)))
            }
            Some(_) => return Classified::Through,
            None => None,
        },
    };
    match found {
        None => Classified::Refused,
        // The base key (`identity::early_identity`) names the owner and its container by key
        // and hashes neither's signature or attributes: a read of one is a fact like any
        // other item's. The body's own item as "self", as its identity writes it: two bodies of one
        // identity that read the same of themselves list the same shape (`values::OWN`).
        Some(fact) => Classified::Fact(values::relative(db, def, fact)),
    }
}

/// What a read by a step of `def` of a row behind the seal of the library group `group`
/// stands for, the row having let go of its reads (`hir_ty::body_store::BodyFacts::sealed`):
/// the `Crate` fact of the crate whose group, or whose source root's group, `group` is, among
/// `def`'s crate and the crates it depends on, and the build's facts of the inputs no group
/// holds (the field order plan, the borrow check's debug setting). `false` when no such crate
/// has the group: the read is refused by the row's name.
///
/// Why this covers every input of the row's value (keys-cover-every-input): the row is a
/// tracked function, so its value is a function of its key and its reads at its last run; its
/// key is made by the step from what the step read, so the step's other facts cover it. The
/// engine put the row in `group` because every read of it in a group is in `group` or in a
/// group `group` depends on (`blaze_query::db::seal`'s `join_of`; a row whose reads no one
/// group covers is loose and never sealed). A crate's group depends on its dependencies'
/// groups, and a source root's group holds that root's files, so every grouped read is of an
/// input `crate_inputs` hashes into the crate's `Crate` fact: each crate it depends on, its
/// content, its source root's files and its proc macros, with the files and variables its
/// macros read outside the database. The reads in no group are the build's inputs, each a
/// fact of its own added here. The seal is what lets the row go unread in this database; the
/// facts are what make the step's key say so in another, so a change to the library (a new
/// version, another toolchain) changes its `Crate` fact and misses the step, as the seal
/// breaking reruns the row here.
pub fn sealed(
    db: &Db,
    def: DefWithBodyId,
    group: blaze_query::db::SealGroup,
    facts: &mut Vec<FactRef>,
) -> bool {
    let krate = def.krate(db);
    let found = std::iter::once(&krate).chain(krate.transitive_deps(db)).find(|&&it| {
        it.seal_group_of(db) == Some(group) || it.seal_root_group_of(db) == Some(group)
    });
    let Some(&found) = found else { return false };
    facts.push(whole_crate(db, found)); // alloc: the step's scratch list, reused
    facts.push(values::field_order()); // alloc: as above
    match values::setting("borrowck_debug") {
        Some(fact) => facts.push(fact), // alloc: as above
        None => return false,
    }
    true
}

/// The rows of a body's own that its lowering step (`hir_ty::body_store::lowering`) reads and
/// whose value is a function of what it read: a read of one by another step of the body is
/// the body's `STEP_LOWER` fact ([`own_lowering`]). `closure_exprs` is the closures of the
/// store `Body::of` made.
const LOWERING_ROWS: [&str; 4] =
    ["Body::of", "Body::has_lowering_errors", "closure_exprs", "lowering"];

/// What a read of a row of the body's own ([`Rule::OwnBody`], [`Rule::OwnStore`]) made by
/// `step` stands for: read through by the lowering itself, so each of
/// its reads is classified in its place; by another step, the lowering's fact for a row of
/// [`LOWERING_ROWS`], and read through for any other (the scopes and upvars, which read
/// name resolution of their own).
fn own_lowering(step: Step, name: &str) -> Classified {
    match step {
        Step::Lower => Classified::Through,
        _ if LOWERING_ROWS.contains(&name) => match values::step_fact(Step::Lower) {
            Some(fact) => Classified::Fact(fact),
            None => Classified::Refused,
        },
        _ => Classified::Through,
    }
}

/// The fact a read of one of name resolution's rows stands for: refused
/// when `hir_def::nameres::name_read` does not know the row, so a row added there is listed
/// by the flush until it is answered for.
fn name_fact(
    db: &Db,
    def: DefWithBodyId,
    read: blaze_query::db::DatabaseKeyIndex,
    kind: ReadKind,
) -> Classified {
    let fact = match name_read(db, read, kind) {
        None => return Classified::Refused,
        Some(NameRead::Handle) => return Classified::Covered,
        Some(NameRead::Name(module, name)) => values::resolves(db, module, &name),
        Some(NameRead::Traits(module)) => values::traits_in_scope(db, module),
        // The modules declared in a module are names of its scope, each a type: no fact of
        // them alone is had, so a read of them is a read of the scope.
        Some(NameRead::WholeScope(module) | NameRead::Children(module)) => {
            return Classified::Scan(values::relative(db, def, values::whole_scope(db, module)));
        }
        Some(NameRead::Crate(krate)) => values::crate_names(db, krate),
        Some(NameRead::AllModules(krate)) => {
            return Classified::Scan(values::relative(db, def, whole_crate(db, krate)));
        }
    };
    Classified::Fact(values::relative(db, def, fact))
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
        Of::Adt => ItemId::Adt(arg::<AdtId>(db, key)?),
        Of::Trait => ItemId::Trait(arg::<TraitId>(db, key)?),
        Of::Impl => ItemId::Impl(arg::<ImplId>(db, key)?),
        Of::Assoc => ItemId::from(arg::<AssocItemId>(db, key)?),
        Of::AttrOwner => return None,
    })
}

/// Whose step a [`Rule::OwnStep`] read is of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StepOf {
    /// `def` itself, or a closure or async coroutine written in it whose MIR is part of
    /// `def`'s MIR step value (`hir_ty::body_store::step_value`).
    Own,
    /// Another body, or such a closure or coroutine of it: that body's step value covers it.
    Other(DefWithBodyId),
    /// An anonymous constant written in `def`'s own body: read through.
    OwnConst,
    /// Anything else: a constant of another body or of a signature, a coroutine kind no step
    /// value covers, a key of no body.
    Not,
}

/// Whose step `key`, of a [`Rule::OwnStep`] read made by a step of `def`, is of.
fn step_of(db: &Db, def: DefWithBodyId, key: Id) -> StepOf {
    let of = |body: InferBodyId<'_>| match body {
        InferBodyId::DefWithBodyId(body) if body == def => StepOf::Own,
        InferBodyId::DefWithBodyId(body) => StepOf::Other(body),
        InferBodyId::AnonConstId(it) => match it.loc(db).owner {
            ExpressionStoreOwnerId::Body(body) if body == def => StepOf::OwnConst,
            _ => StepOf::Not,
        },
    };
    // A closure's or coroutine's MIR is in its body's step value only for the kinds
    // `body_store::mir::owners` lists; and a constant's closure is in no step value.
    let covered = |owner: InferBodyId<'_>, yes: bool| match (of(owner), yes) {
        (StepOf::Own | StepOf::Other(_), true) => of(owner),
        _ => StepOf::Not,
    };
    if let Some(body) = arg::<DefWithBodyId>(db, key) {
        return of(InferBodyId::DefWithBodyId(body));
    }
    if let Some(body) = arg::<InferBodyId<'_>>(db, key) {
        return of(body);
    }
    if let Some(closure) = arg::<InternedClosureId<'_>>(db, key) {
        let loc = closure.loc(db);
        let kind = matches!(
            loc.kind,
            ClosureKind::Closure | ClosureKind::CoroutineClosure(CoroutineKind::Async)
        );
        return covered(loc.owner, kind);
    }
    if let Some(coroutine) = arg::<InternedCoroutineId<'_>>(db, key) {
        let loc = coroutine.loc(db);
        let kind = matches!(loc.kind, ClosureKind::Coroutine { kind: CoroutineKind::Async, .. });
        return covered(loc.owner, kind);
    }
    StepOf::Not
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

/// The block a [`Rule::Block`] read is of.
fn block_of(db: &Db, key: Id) -> Option<BlockIdLt<'_>> {
    arg::<BlockIdLt<'_>>(db, key)
        .or_else(|| InherentImpls_in_block_for_self_ty::arguments(db, key).map(|(it, _)| it))
        .or_else(|| {
            TraitImpls_in_block_for_trait_and_self_ty::arguments(db, key).map(|(it, _, _)| it)
        })
}

/// The fact per item a [`Rule::DepCrate`] read stands for, made by a body of `own`, of `own` or
/// a crate `own` loads; its value is as `own` sees it (`blaze_items::goals::fact_value`), over
/// every crate `own` loads: what the read can give, as the loaded set is closed (a crate `own`
/// loads loads only crates `own` does), and more, never less.
///
/// - The impls of one trait for one simplified self type
///   (`TraitImpls::in_crate_and_deps_for_trait_and_self_ty`, `in_crate_for_trait_and_self_ty`)
///   or one trait's blanket impls (`TraitImpls::blanket_in_crate_and_deps`): the `Implements`
///   fact, each impl by its key and signature (`blaze_items::implements_value`). The blanket
///   impls alone are the fact of no simplified self type (`blaze_items::key::unsimplified`), as
///   lane items' `relevant_impls` reads are.
/// - `InherentImpls::in_crate_for_self_ty`, `TraitImpls::for_crate_and_trait`,
///   `in_crate_trait_value_items` and `module_items_named`: lane items' fact of that listing
///   (`blaze_items::goals::fact_of_read`, the one home of the mapping), `module_items_named`
///   of a module of any crate, as one module's traits in scope are the same whoever asks.
///
/// `None` for one of a crate `own` does not load, whose tables no fact of `own` covers.
fn own_item_read(
    db: &Db,
    own: Crate,
    read: blaze_query::db::DatabaseKeyIndex,
    name: &'static str,
    of: Args,
) -> Option<FactRef> {
    let key = read.key_index();
    let (krate, trait_, self_ty) = match of {
        Args::TraitSelfTy => {
            let (krate, trait_, SelfTyKey(self_ty)) =
                TraitImpls_in_crate_for_trait_and_self_ty::arguments(db, key).or_else(|| {
                    TraitImpls_in_crate_and_deps_for_trait_and_self_ty::arguments(db, key)
                })?;
            #[cfg(test)]
            if planted::SELF_TY_IGNORED.get() {
                return Some(implements_fact(db, trait_, key::unsimplified()));
            }
            (krate, trait_, key::simplified(db, &self_ty))
        }
        Args::Trait => match TraitImpls_blanket_in_crate_and_deps::arguments(db, key) {
            Some((krate, trait_)) => (krate, trait_, key::unsimplified()),
            None => return Goals.fact_of(db, Some(own), read, name),
        },
        Args::SelfTy | Args::Name | Args::ModuleName => {
            return Goals.fact_of(db, Some(own), read, name);
        }
    };
    if krate != own && !loaded_crates(db, own).contains(&krate) {
        return None;
    }
    Some(implements_fact(db, trait_, self_ty))
}

/// The planted bug of the own-crate impls oracle's control
/// (`tests::classify::the_oracle_catches_an_implements_fact_that_ignores_an_added_impl`).
#[cfg(test)]
pub(crate) mod planted {
    use std::cell::Cell;

    thread_local! {
        /// A read of one trait's impls for one self type stands for the trait's blanket impls
        /// alone, so its value ignores an impl for the type added or removed.
        pub(crate) static SELF_TY_IGNORED: Cell<bool> = const { Cell::new(false) };
        /// The borrow check's solver queries are covered with no fact, as though they read
        /// nothing outside the body: a trace that read one is kept under none of the impls,
        /// `Drop` impls and associated types it saw
        /// (`tests::classify::the_oracle_catches_solver_queries_kept_under_no_fact`).
        pub(crate) static QUERIES_COVERED: Cell<bool> = const { Cell::new(false) };
    }

    /// The queries [`QUERIES_COVERED`] covers.
    pub(crate) const QUERIES: [&str; 5] = [
        "is_copy_query",
        "drop_glue_query",
        "prove_query",
        "normalize_query",
        "normalizes_to_error_query",
    ];
}

/// Lane items' `Implements { trait_, self_ty }` fact as hir-ty's stores keep it: the trait by
/// key, the simplified self type as its object (`blaze_items::table`'s split of it).
pub(crate) fn implements_fact(
    db: &Db,
    trait_: TraitId,
    self_ty: blaze_items::SimplifiedType,
) -> FactRef {
    FactRef {
        kind: items::IMPLEMENTS,
        subject: key::item_key(db, ItemId::Trait(trait_)).0,
        object: FactObject::Id(self_ty.0),
    }
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

#[blaze_query::tracked(returns(copy))]
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

/// `key` as a `T`, if its page holds `T`s.
fn arg<T: FromIdIn>(db: &Db, key: Id) -> Option<T> {
    T::from_id_in(db.storage(), key)
}
