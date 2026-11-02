//! Handling of opaque types, detection of defining scope and hidden type.

use base_db::{Db, Crate};
use blaze_mem::Frame;
use hir_def::{
    AssocItemId, AssocItemLoc, DefWithBodyId, FunctionId, HasModule, ImplId, ItemContainerId,
    ModuleDefId, TraitId, TypeAliasId,
    attrs::AttrFlags,
    expr_store::Body,
    nameres::{DefMap, crate_def_map},
    resolver::HasResolver,
    signatures::ImplSignature,
};
use hir_expand::name::Name;
use la_arena::ArenaMap;
use rustc_type_ir::inherent::Ty as _;
use syntax::{TextSize, ast};

use crate::{
    ImplTraitId, InferBodyId, InferenceResult,
    db::{HirDatabase, InternedOpaqueTyId},
    lower::{ImplTraitIdx, SelfAndAssocBounds},
    method_resolution::may_hold,
    next_solver::{
        DbInterner, ErrorGuaranteed, SolverDefId, StoredEarlyBinder, StoredTy, Ty, TypingMode,
        infer::{DbInternerInferExt, traits::ObligationCause},
        obligation_ctxt::ObligationCtxt,
    },
};

pub(crate) fn opaque_types_defined_by<'db>(
    db: &'db Db,
    def_id: InferBodyId<'_>,
    result: &mut Vec<SolverDefId<'db>>,
) {
    if let Some(func) = def_id.as_function() {
        // A function may define its own RPITs.
        extend_with_opaques(
            db,
            SelfAndAssocBounds::return_type_impl_traits(db, func),
            |opaque_idx| ImplTraitId::ReturnTypeImplTrait(func, opaque_idx),
            result,
        );
    }

    let extend_with_taits = |type_alias| {
        extend_with_opaques(
            db,
            SelfAndAssocBounds::type_alias_impl_traits(db, type_alias),
            |opaque_idx| ImplTraitId::TypeAliasImplTrait(type_alias, opaque_idx),
            result,
        );
    };

    // Collect opaques from assoc items.
    let extend_with_atpit_from_assoc_items = |type_aliases: &[TypeAliasId]| {
        type_aliases.iter().copied().for_each(extend_with_taits);
    };
    let extend_with_atpit_from_container = |container| match container {
        ItemContainerId::ImplId(impl_id)
            if ImplSignature::of(db, impl_id).target_trait.is_some() =>
        {
            extend_with_atpit_from_assoc_items(impl_type_aliases(db, impl_id));
        }
        ItemContainerId::TraitId(trait_id) => {
            extend_with_atpit_from_assoc_items(trait_type_aliases(db, trait_id));
        }
        _ => {}
    };
    match def_id {
        InferBodyId::DefWithBodyId(DefWithBodyId::ConstId(id)) => {
            extend_with_atpit_from_container(id.loc(db).container)
        }
        InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(id)) => {
            extend_with_atpit_from_container(id.loc(db).container)
        }
        InferBodyId::DefWithBodyId(DefWithBodyId::StaticId(_))
        | InferBodyId::DefWithBodyId(DefWithBodyId::VariantId(_))
        | InferBodyId::AnonConstId(_) => {}
    }

    if let Some(func) = def_id.as_function() {
        for type_alias in defined_type_aliases(db, func) {
            extend_with_opaques(
                db,
                SelfAndAssocBounds::type_alias_impl_traits(db, type_alias),
                |opaque_idx| ImplTraitId::TypeAliasImplTrait(type_alias, opaque_idx),
                result,
            );
        }
    }

    fn extend_with_opaques<'db>(
        db: &'db Db,
        opaques: &[SelfAndAssocBounds],
        mut make_impl_trait: impl FnMut(ImplTraitIdx) -> ImplTraitId,
        result: &mut Vec<SolverDefId<'db>>,
    ) {
        for (opaque_idx, _) in opaques.iter().enumerate() {
            let opaque_id = InternedOpaqueTyId::new(
                db,
                make_impl_trait(ImplTraitIdx::from_raw(la_arena::RawIdx::from_u32(
                    opaque_idx as u32,
                ))),
            );
            result.push(opaque_id.into());
        }
    }
}

impl SelfAndAssocBounds {
    /// How many opaque types (`impl Trait`) the type of `type_alias` defines, numbered
    /// `0..count` by their [`ImplTraitIdx`].
    pub fn type_alias_opaque_count(db: &Db, type_alias: TypeAliasId) -> usize {
        SelfAndAssocBounds::type_alias_impl_traits(db, type_alias).len()
    }
}

/// The associated types of an impl, in order: what a body in it reads of its items for the
/// opaque types it may define. A function or constant added to the impl leaves this equal.
#[blaze_query::tracked(returns(deref), mem_tag = Infer)]
fn impl_type_aliases(db: &Db, impl_id: ImplId) -> Box<[TypeAliasId]> {
    blaze_mem::frame::current(|frame| type_aliases_of(frame, &impl_id.impl_items(db).items))
}

/// As [`impl_type_aliases`], of a trait.
#[blaze_query::tracked(returns(deref), mem_tag = Infer)]
fn trait_type_aliases(db: &Db, trait_id: TraitId) -> Box<[TypeAliasId]> {
    blaze_mem::frame::current(|frame| type_aliases_of(frame, &trait_id.trait_items(db).items))
}

fn type_aliases_of(frame: &Frame, items: &[(Name, AssocItemId)]) -> Box<[TypeAliasId]> {
    let mut aliases = Vec::new_in(frame);
    aliases.extend(items.iter().filter_map(|&(_, assoc_id)| match assoc_id {
        AssocItemId::TypeAliasId(it) => Some(it),
        AssocItemId::FunctionId(_) | AssocItemId::ConstId(_) => None,
    }));
    // alloc: the memo, one block at its size
    aliases.iter().copied().collect()
}

// These are firewall queries to prevent drawing dependencies between infers:

#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
pub fn rpit_hidden_types(
    db: &Db,
    function: FunctionId,
) -> ArenaMap<ImplTraitIdx, StoredEarlyBinder<StoredTy>> {
    let infer = InferenceResult::of(db, DefWithBodyId::from(function));
    // `insert` grows the map to the index it is given: sized once to the highest, nothing is
    // shrunk after.
    let len = infer
        .return_position_impl_trait_types(db)
        .map(|(opaque, _)| opaque.into_raw().into_u32() as usize + 1)
        .max()
        .unwrap_or(0);
    let interner = DbInterner::new_no_crate(db);
    let mut result = ArenaMap::with_capacity(len);
    for (opaque, hidden_type) in infer.return_position_impl_trait_types(db) {
        result.insert(opaque, StoredEarlyBinder::bind(interner, hidden_type.store()));
    }
    result
}

#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
pub fn tait_hidden_types(
    db: &Db,
    type_alias: TypeAliasId,
) -> ArenaMap<ImplTraitIdx, StoredEarlyBinder<StoredTy>> {
    // Call this first, to not perform redundant work if there are no TAITs.
    let taits_count = SelfAndAssocBounds::type_alias_impl_traits(db, type_alias).len();
    // An alias without `impl Trait` has no opaque a body could define, so inferring its
    // defining bodies would find nothing to keep.
    if taits_count == 0 {
        return ArenaMap::new();
    }

    let loc = type_alias.loc(db);
    let module = loc.module(db);
    let interner = DbInterner::new_with(db, module.krate(db));
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    let mut ocx = ObligationCtxt::new(&infcx);
    let cause = ObligationCause::dummy();
    let param_env = db.trait_environment(type_alias.into());

    let mut result = ArenaMap::with_capacity(taits_count);
    for_each_defining_body(db, type_alias, loc, |defining_body| {
        let infer = InferenceResult::of(db, defining_body);
        for (&opaque, hidden_type) in &infer.type_of_opaque {
            let ImplTraitId::TypeAliasImplTrait(opaque_owner, opaque_idx) = opaque.loc(db) else {
                continue;
            };
            if opaque_owner != type_alias {
                continue;
            }
            // In the presence of errors, we attempt to create a unified type from all
            // types. rustc doesn't do that, but this should improve the experience.
            let hidden_type = infcx.insert_type_vars(hidden_type.as_ref());
            match result.entry(opaque_idx) {
                la_arena::Entry::Vacant(entry) => {
                    entry.insert(StoredEarlyBinder::bind(interner, hidden_type.store()));
                }
                la_arena::Entry::Occupied(entry) => {
                    _ = ocx.eq(
                        &cause,
                        param_env,
                        entry.get().get().instantiate_identity().skip_norm_wip(),
                        hidden_type,
                    );
                }
            }
        }
    });

    _ = ocx.try_evaluate_obligations();

    // Fill missing entries.
    for idx in 0..taits_count {
        let idx = la_arena::Idx::from_raw(la_arena::RawIdx::from_u32(idx as u32));
        match result.entry(idx) {
            la_arena::Entry::Vacant(entry) => {
                entry.insert(StoredEarlyBinder::bind(
                    interner,
                    Ty::new_error(interner, ErrorGuaranteed).store(),
                ));
            }
            la_arena::Entry::Occupied(mut entry) => {
                let hidden_type = entry.get().get().skip_binder();
                let hidden_type = infcx
                    .deeply_resolve_ignoring_regions(hidden_type)
                    .replace_infer_with_error(interner);
                *entry.get_mut() = StoredEarlyBinder::bind(interner, hidden_type.store());
            }
        }
    }

    result
}

/// Calls `f` with each body that may define the type alias's opaques, in order.
fn for_each_defining_body(
    db: &Db,
    type_alias: TypeAliasId,
    loc: &AssocItemLoc<ast::TypeAlias>,
    mut f: impl FnMut(DefWithBodyId),
) {
    match loc.container {
        ItemContainerId::ImplId(impl_id)
            if ImplSignature::of(db, impl_id).target_trait.is_some() =>
        {
            from_assoc_items(&impl_id.impl_items(db).items, &mut f)
        }
        ItemContainerId::TraitId(trait_id) => {
            from_assoc_items(&trait_id.trait_items(db).items, &mut f)
        }
        _ => defining_functions(db, type_alias).iter().for_each(|&func| f(func.into())),
    }

    // Associated Type Position Impl Trait.
    fn from_assoc_items(
        assoc_items: &[(Name, AssocItemId)],
        f: &mut impl FnMut(DefWithBodyId),
    ) {
        for &(_, assoc_id) in assoc_items {
            match assoc_id {
                AssocItemId::FunctionId(it) => f(it.into()),
                AssocItemId::ConstId(it) => f(it.into()),
                AssocItemId::TypeAliasId(_) => {}
            }
        }
    }
}

/// The functions of the type alias's crate whose `#[define_opaque(..)]` names it: what its
/// hidden types read of [`define_opaque_functions`], so another type alias's defining
/// function added or removed leaves them as they were.
#[blaze_query::tracked(returns(deref), mem_tag = Infer)]
fn defining_functions(db: &Db, type_alias: TypeAliasId) -> Box<[FunctionId]> {
    blaze_mem::frame::current(|frame| {
        let mut found = Vec::new_in(frame);
        found.extend(
            define_opaque_functions(db, type_alias.loc(db).module(db).krate(db))
                .iter()
                .copied()
                .filter(|&func| defined_type_aliases(db, func).any(|alias| alias == type_alias)),
        );
        // alloc: the memo, one block at its size
        found.iter().copied().collect()
    })
}

/// The type aliases `func` names in its `#[define_opaque(..)]`.
fn defined_type_aliases(
    db: &Db,
    func: FunctionId,
) -> impl Iterator<Item = TypeAliasId> {
    let paths = AttrFlags::define_opaque(db, func);
    let resolver = (!paths.is_empty()).then(|| func.resolver(db));
    paths.iter().filter_map(move |path| {
        match resolver.as_ref()?.resolve_module_path_in_items(db, path).take_types()? {
            ModuleDefId::TypeAliasId(it) => Some(it),
            _ => None,
        }
    })
}

/// Every function of the crate with a `#[define_opaque(..)]`: in a module, in an impl, and in
/// a block of a body, as std's `sys::paths::unix::split_paths_ref` has the function that
/// defines `SplitPathsRef` inside it. Of a crate that is not local, a body is lowered for its
/// blocks when its text says `define_opaque` or may have it written by a macro or an attribute
/// ([`may_hold`]), and always when a macro wrote the body itself.
#[blaze_query::tracked(returns(deref), mem_tag = Infer)]
fn define_opaque_functions(db: &Db, krate: Crate) -> Box<[FunctionId]> {
    let local = krate.data(db).origin.is_local();
    return blaze_mem::frame::current(|frame| {
        let mut result = Vec::new_in(frame);
        collect(db, krate, crate_def_map(db, krate), local, &mut result);
        // alloc: the memo, one block at its size
        result.iter().copied().collect()
    });

    fn collect(
        db: &Db,
        krate: Crate,
        def_map: &DefMap,
        local: bool,
        result: &mut Vec<FunctionId, &Frame>,
    ) {
        for (_, module) in def_map.modules(db) {
            let scope = module.scope(db);
            for def in scope.declarations() {
                match def {
                    ModuleDefId::FunctionId(it) => holder(db, krate, it.into(), local, result),
                    ModuleDefId::ConstId(it) => holder(db, krate, it.into(), local, result),
                    ModuleDefId::StaticId(it) => holder(db, krate, it.into(), local, result),
                    _ => {}
                }
            }
            for impl_id in scope.impls() {
                for &(_, item) in &impl_id.impl_items(db).items {
                    match item {
                        AssocItemId::FunctionId(it) => holder(db, krate, it.into(), local, result),
                        AssocItemId::ConstId(it) => holder(db, krate, it.into(), local, result),
                        AssocItemId::TypeAliasId(_) => {}
                    }
                }
            }
        }
    }

    fn holder(
        db: &Db,
        krate: Crate,
        owner: DefWithBodyId,
        local: bool,
        result: &mut Vec<FunctionId, &Frame>,
    ) {
        if let DefWithBodyId::FunctionId(func) = owner
            && !AttrFlags::define_opaque(db, func).is_empty()
        {
            result.push(func);
        }
        if local || may_define_opaque(db, krate, owner) {
            for (_, block) in Body::of(db, owner).blocks(db) {
                collect(db, krate, block, local, result);
            }
        }
    }
}

/// Whether a block of `owner` may have a function with `#[define_opaque]`: its text says
/// `define_opaque`, or [`may_hold`] finds a macro call or an attribute that may write one.
fn may_define_opaque(db: &Db, krate: Crate, owner: DefWithBodyId) -> bool {
    match owner {
        DefWithBodyId::FunctionId(it) => text_may_define_opaque(db, krate, it.loc(db).id),
        DefWithBodyId::ConstId(it) => text_may_define_opaque(db, krate, it.loc(db).id),
        DefWithBodyId::StaticId(it) => text_may_define_opaque(db, krate, it.loc(db).id),
        DefWithBodyId::VariantId(_) => false,
    }
}

fn text_may_define_opaque<N: syntax::AstNode>(
    db: &Db,
    krate: Crate,
    id: hir_expand::AstId<N>,
) -> bool {
    let Some(file) = id.file_id.file_id() else { return true };
    let says = || {
        let at = define_opaque_at(db, file);
        !at.is_empty() && {
            let range = id.file_id.ast_id_map(db).get(id.value).text_range();
            at.iter().any(|&at| range.contains(at))
        }
    };
    says() || may_hold(db, krate, id, false)
}

/// Where the text of `file` says `define_opaque`.
#[blaze_query::tracked(returns(deref), mem_tag = Infer)]
fn define_opaque_at(db: &Db, file: base_db::EditionedFileId) -> Box<[TextSize]> {
    let text = db.file_text(file.file_id(db)).text(db);
    blaze_mem::frame::current(|frame| {
        let mut found = Vec::new_in(frame);
        found.extend(text.match_indices("define_opaque").map(|(at, _)| TextSize::from(at as u32)));
        // alloc: the memo, one block at its size
        found.iter().copied().collect()
    })
}
