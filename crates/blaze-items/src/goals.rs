//! Lane items' part of the persistent goal cache (`hir_ty::next_solver::goal_store`, notes in
//! docs/v2/plan/design/frontend.md section 3): item keys for a goal's
//! definitions, its crate's key, and the facts its reads stand for.
//!
//! A goal's read list is the memo's (`blaze_memo::record`, port decision 2): one
//! `Read::Query` row for each fact boundary query it asked (the kinds
//! `blaze_item_hash::reads::BOUNDARIES` marks), every other query flattened into it, and one
//! `Read::Input` row for each input it read. A row of a query keyed by one item whose answer is
//! part of what that item's fact hashes (the notes' table, section 3.4) stands for that fact
//! ([`fact_of_read`]). A row of a dependency's batch (its impls' traits, self types and
//! predicates, its fields' types, lowered for the whole crate at once) stands for the crate's
//! [`Fact::Crate`]. A row of the impls of one type or one trait, or of the traits with an item
//! of one name, in a crate the goal's crate sees, stands for that listing's fact as the goal's
//! crate sees it ([`Fact::InherentImpls`], [`Fact::TraitImpls`], [`Fact::TraitsNamed`]); a row
//! of a module's traits in scope by name, or of a block's impls, for [`Fact::ScopeTraitsNamed`]
//! or [`Fact::BlockImpls`]. Any other row, an input's among them, stands for no fact, and the
//! goal is not stored.

use base_db::{Crate, Db};
use blaze_ids::{ContentId, Hasher};
use hir_def::{
    AdtId, AttrDefId, ItemContainerId, Lookup,
    lang_item::{LangItemEnum, lang_items},
};
use hir_ty::{
    method_resolution::{SelfTyKey, loaded_crates},
    next_solver::{
        SolverDefId,
        goal_store::{FactRef, GoalFacts},
        util::ItemWithBounds,
    },
};

use crate::{Fact, ItemId, facts, key, table};

const GOAL_CRATE_DOMAIN: &str = "blaze v2 goal crate 1";

/// What the driver gives `hir_ty::next_solver::goal_store::install`.
pub struct Goals;

impl GoalFacts for Goals {
    fn def_key<'db>(&self, db: &'db Db, def: SolverDefId<'db>) -> ContentId {
        key::def_key(db, def).0
    }

    fn crate_key(&self, db: &Db, krate: Option<Crate>) -> ContentId {
        match krate {
            Some(krate) => goal_crate_key(db, krate),
            None => ContentId::of(GOAL_CRATE_DOMAIN, b"no crate"),
        }
    }

    /// An interner of no crate can't solve (`DbInterner::new_no_crate`), so it has no goal to
    /// store: refused, as is a row that stands for no fact, by its kind's name.
    fn fact_of(
        &self,
        db: &Db,
        krate: Option<Crate>,
        read: blaze_memo::Read,
    ) -> Result<FactRef, &'static str> {
        let name = read.kind().name();
        let fact = krate.and_then(|krate| fact_of_read(db, krate, read)).ok_or(name)?;
        Ok(table::fact_ref(&fact))
    }

    fn value(&self, db: &Db, krate: Option<Crate>, fact: FactRef) -> Option<ContentId> {
        fact_value(db, fact_ref_key(db, krate?, fact))
    }
}

/// A fact as the goal store keeps it, with the one crate whose data its value is: the key of
/// [`fact_value`], interned in that crate's arena. Made by [`fact_ref_key`].
///
/// `krate` is the crate the value is read in, and so the query's home:
/// - for a fact of which impls and traits a crate sees (`facts::asked_by_crate`: `Implements`,
///   `Method`, `Vtable`, ...), the asking crate, its `seen_by`: the value covers the crates it
///   loads, all in its arena's closure;
/// - for every other fact, whose value is one for every asker, the crate the fact's subject is
///   of (an item key's crate prefix, or a crate key: `key::crate_of_key`), so one memo serves
///   every asker; where no crate of this snapshot has that prefix, the asking crate: the value
///   is "absent", read from the crate list (`base_db::listed_by_prefix`'s miss, the list's own
///   arena, which a crate's arena may read and a session-homed run may not), so a crate that
///   takes the prefix drops it with the list.
///
/// `None` only in a key made by hand, which [`fact_value`] answers as no fact.
#[blaze_memo::interned(debug)]
pub struct FactRefKey<'db> {
    #[returns(copy)]
    pub krate: Option<Crate>,
    #[returns(copy)]
    pub fact: FactRef,
}

impl<'db> FactRefKey<'db> {
    /// The asking crate of a fact whose value depends on it (`facts::asker`); `None` for every
    /// other fact, whose `krate` is its subject's crate, not an asker.
    pub fn seen_by(self, db: &'db Db) -> Option<Crate> {
        if facts::asked_by_crate(self.fact(db).kind) { self.krate(db) } else { None }
    }
}

/// The crate `fact`, asked by `krate`, is read in ([`FactRefKey`]'s `krate`).
fn fact_home(db: &Db, krate: Crate, fact: FactRef) -> Option<Crate> {
    if facts::asked_by_crate(fact.kind) {
        Some(krate)
    } else {
        // A miss is homed at the asker: never the session, which may not read the list's arena.
        key::crate_of_key(db, &fact.subject).or(Some(krate))
    }
}

/// The key of `fact` asked by `krate`: the asker kept only where the value depends on it, so a
/// fact like a signature or a layout is one memo, homed at its item's crate, not one per crate
/// that asks it.
pub fn fact_ref_key(db: &Db, krate: Crate, fact: FactRef) -> FactRefKey<'_> {
    FactRefKey::new(db, fact_home(db, krate, fact), fact)
}

/// The value of a fact now as `key.seen_by` sees it (`facts::value`), `None` for parts that are
/// no fact or a fact that cannot be hashed. One query, a fact boundary, so that a body step that
/// asks a value lists `fact_value` of that fact, never the queries the value is computed from: a
/// reader of the step's list names the fact and its asker exactly. Homed at `key.krate`, the
/// crate whose data the value is.
#[blaze_memo::tracked(returns(copy))]
pub fn fact_value<'db>(db: &'db Db, key: FactRefKey<'db>) -> Option<ContentId> {
    let parts = key.fact(db);
    let fact = table::fact_of_ref(parts)?;
    // A key made other than by `fact_ref_key` (a fact asked by a crate with none, a fact homed
    // at a crate its subject is not of, or one of a subject no crate has homed at none) is no
    // fact.
    // A key of no crate is homed at the session, which reads no crate and not the list: it is
    // answered before the lookup.
    let made = match key.krate(db) {
        None => false,
        Some(_) if facts::asked_by_crate(fact.kind()) => true,
        Some(krate) => key::crate_of_key(db, &parts.subject).is_none_or(|home| home == krate),
    };
    if !made {
        return None;
    }
    facts::value_as(db, key.seen_by(db), &fact).ok()
}

/// A crate's identity and its lang items by item key: what an interner of it reads when it
/// is made, outside a goal's evaluation. Then the crates it loads, in order, by crate key:
/// which crates' impls a `relevant_impls` read of it searches, the same ones its `Implements`
/// facts cover (`facts::value` as it sees them).
#[blaze_memo::tracked(returns(copy))]
pub fn goal_crate_key(db: &Db, krate: Crate) -> ContentId {
    let lang_items = lang_items(db, krate);
    let mut h = Hasher::new(GOAL_CRATE_DOMAIN);
    h.id(&key::crate_key(db, krate));
    for (at, item) in LangItemEnum::ALL.iter().enumerate() {
        let Some(target) = item.from_lang_items(lang_items) else { continue };
        h.update(&(at as u32).to_le_bytes()).id(&key::lang_item_key(db, target).0);
    }
    let loaded = loaded_crates(db, krate);
    h.part(b"loaded").update(&(loaded.len() as u64).to_le_bytes());
    for &it in loaded {
        h.id(&key::crate_key(db, it));
    }
    h.finish()
}

/// The fact the boundary query row `read`, listed by a goal or a body of `krate`, stands for,
/// its value asked as `krate` sees it; `None` for a row of no query here. Each query here is
/// keyed by one item (or a crate, or the arguments a listing is made of) and returns part of
/// an item's interface (its signature, bounds, fields, items, variances, attribute flags:
/// `crate::form`'s signature form), the hidden types of an item's `impl Trait`s, an impl
/// method's types for the trait method's `impl Trait`s, the crate's target, the impls of one
/// trait for one simplified self type, whether an impl may specialize and what it specializes,
/// or a dependency's batch, which stands for its crate. [`QUERIES`] names them, for
/// `blaze_item_hash::reads` to mark as fact boundaries.
///
/// `ImplItems::of` and `TraitItems::query_with_diagnostics` are read by name where the solver
/// finds the impl item of a trait item (`fetch_eligible_assoc_item`): the impl's signature form
/// writes, for each trait item in order, the impl item that name finds, so a rename that
/// changes what it finds changes the impl's signature.
pub fn fact_of_read(db: &Db, krate: Crate, read: blaze_memo::Read) -> Option<Fact> {
    use hir_def::{attrs as at, nameres::assoc as items, signatures as sig};
    use hir_ty::kinds as ty;
    let memo = blaze_memo::Database::memo(db);
    let signature = |id: ItemId| Some(Fact::Signature(key::item_key(db, id)));
    // The arguments of `read` as the boundary query `$q` stores them, a tuple, when `read` is a
    // row of `$q` (its generated `key_of`); `None` for any other row. A row is decoded by the
    // query it names, never by a name written as text.
    macro_rules! of {
        ($($q:ident)::+) => { $($q)::+::key_of(memo, read) };
    }
    if let Some((imp,)) = of!(sig::ImplSignature_served)
        .or_else(|| of!(sig::ImplSignature_lowered))
        .or_else(|| of!(items::ImplItems_of))
        .or_else(|| of!(ty::impl_trait_of_impl))
        .or_else(|| of!(ty::impl_self_ty_of_impl))
        .or_else(|| of!(ty::impl_has_unconstrained_params))
    {
        return signature(ItemId::Impl(imp));
    }
    if let Some((it,)) = of!(sig::StructSignature_served)
        .or_else(|| of!(sig::StructSignature_lowered))
        .or_else(|| of!(ty::fn_sig_for_struct_constructor))
    {
        return signature(ItemId::Adt(AdtId::StructId(it)));
    }
    if let Some((it,)) = of!(sig::EnumSignature_served).or_else(|| of!(sig::EnumSignature_lowered))
    {
        return signature(ItemId::Adt(AdtId::EnumId(it)));
    }
    if let Some((it,)) =
        of!(sig::UnionSignature_served).or_else(|| of!(sig::UnionSignature_lowered))
    {
        return signature(ItemId::Adt(AdtId::UnionId(it)));
    }
    if let Some((adt,)) = of!(ty::representability) {
        return signature(ItemId::Adt(adt));
    }
    if let Some((variant,)) =
        of!(ty::field_types_of_variant)
            .or_else(|| of!(ty::batched_field_types))
            .or_else(|| of!(ty::ctor_variances_query))
    {
        return signature(ItemId::Adt(variant.adt_id(db)));
    }
    if let Some((variant,)) = of!(ty::fn_sig_for_enum_variant_constructor) {
        return signature(ItemId::Adt(AdtId::EnumId(variant.lookup(db).parent)));
    }
    if let Some((it,)) = of!(sig::TraitSignature_served)
        .or_else(|| of!(sig::TraitSignature_lowered))
        .or_else(|| of!(items::TraitItems_query_with_diagnostics))
        .or_else(|| of!(ty::supertraits_info))
        .or_else(|| of!(ty::dyn_compatibility_of_trait_query))
    {
        return signature(ItemId::Trait(it));
    }
    if let Some((it,)) = of!(sig::FunctionSignature_served)
        .or_else(|| of!(sig::FunctionSignature_lowered))
        .or_else(|| of!(ty::fn_sig_for_fn))
    {
        return signature(ItemId::Function(it));
    }
    // An impl method's types for the `impl Trait`s of the trait method it implements: a fact
    // of their own, never its signature, which the goals finding them read (the bounds of its
    // own `impl Trait`s): see `Fact::RpititTys`.
    if let Some((func,)) = of!(ty::impl_rpitit_tys) {
        return Some(facts::rpitit_tys(db, func));
    }
    // The bounds of a trait method's `impl Trait`, its trait item's: those its signature writes
    // on the opaque type. An impl's item has none.
    if let Some((func, _)) = of!(ty::rpitit_item_bounds) {
        return signature(ItemId::Function(func));
    }
    // A hidden type is what its defining bodies leak, which the signature hash leaves out.
    if let Some((func,)) = of!(ty::rpit_hidden_types) {
        return Some(facts::hidden_types(db, ItemId::Function(func)));
    }
    if let Some((alias,)) = of!(ty::tait_hidden_types) {
        return Some(facts::hidden_types(db, ItemId::Alias(alias)));
    }
    if let Some((def,)) = of!(ty::generic_predicates_of)
        .or_else(|| of!(ty::variances_of_query))
        .or_else(|| of!(ty::const_conditions_query))
    {
        return signature(key::generic_def_item(def));
    }
    if let Some((of,)) = of!(ty::ItemBounds_of) {
        return signature(bounds_owner(db, of)?);
    }
    if let Some((owner,)) = of!(at::AttrFlags_query) {
        return signature(attrs_owner(db, owner)?);
    }
    if let Some((it,)) = of!(ty::target_data_layout_query) {
        return Some(Fact::Target(key::crate_key(db, it)));
    }
    if let Some((it,)) = of!(ty::crate_impl_traits)
        .or_else(|| of!(ty::crate_impl_self_tys))
        .or_else(|| of!(ty::crate_field_types))
        .or_else(|| of!(ty::crate_impl_predicates))
    {
        return Some(facts::whole_crate(db, it));
    }
    if let Some((search,)) = of!(ty::relevant_impls) {
        // A search of a crate `krate` loads (an impl's own crate, where `specialized_ancestor`
        // looks for the impl it specializes) covers crates `krate` loads too, as the loaded set
        // is closed: `krate`'s fact covers it, more widely. Another crate's search may cover
        // crates `krate` never loads: no fact.
        if !loads(db, krate, search.krate(db)) {
            return None;
        }
        let trait_ = search.trait_(db);
        return Some(match search.self_ty(db) {
            Some(SelfTyKey(self_ty)) => facts::implements(db, trait_, &self_ty),
            None => Fact::Implements {
                trait_: key::item_key(db, ItemId::Trait(trait_)),
                self_ty: key::unsimplified(),
            },
        });
    }
    // A crate's tables of impls and of traits by item name, read of `krate` or a crate it
    // loads: the fact of `krate`'s view, which covers every crate it loads (more than the read,
    // never less, as the loaded set is closed). Another crate's view may cover crates `krate`
    // never loads: no fact.
    if let Some((of, SelfTyKey(self_ty))) = of!(ty::InherentImpls_in_crate_for_self_ty) {
        return loads(db, krate, of).then(|| facts::inherent_impls(db, &self_ty));
    }
    if let Some((of, trait_)) = of!(ty::TraitImpls_for_crate_and_trait) {
        return loads(db, krate, of).then(|| facts::trait_impls(db, trait_));
    }
    if let Some((of, name)) = of!(ty::in_crate_trait_value_items) {
        return loads(db, krate, of).then(|| facts::traits_named(&name));
    }
    // One module's traits in scope, the same whoever asks.
    if let Some((module, name)) = of!(ty::module_items_named) {
        return Some(facts::scope_traits_named(db, module, &name));
    }
    // A block's impls, all of them: a read of one type's or one trait's there is covered.
    if let Some((block,)) =
        of!(ty::TraitImpls_for_block).or_else(|| of!(ty::InherentImpls_for_block))
    {
        return Some(facts::block_impls(db, block));
    }
    if let Some((block, _)) = of!(ty::InherentImpls_in_block_for_self_ty) {
        return Some(facts::block_impls(db, block));
    }
    if let Some((block, _, _)) = of!(ty::TraitImpls_in_block_for_trait_and_self_ty) {
        return Some(facts::block_impls(db, block));
    }
    if let Some((imp,)) = of!(ty::may_specialize) {
        return Some(facts::specializes(db, imp));
    }
    // Whether one impl specializes another is solved in the specializing impl's crate, over
    // the parent's where clauses: it reads impls of any trait those name, which no fact of
    // either impl covers. The crate fact covers everything that crate sees.
    if let Some((pair,)) = of!(ty::specializes_query) {
        let specializing = pair.specializing(db);
        return Some(Fact::Crate(key::crate_key(db, hir_def::HasModule::krate(&specializing, db))));
    }
    None
}

/// The kind names of the queries [`fact_of_read`] answers: each is a fact boundary
/// (`blaze_item_hash::reads::mark_boundaries` marks them with its own). hir-ty's are named
/// through `hir_ty::kinds`, their one public path.
pub const QUERIES: &[&str] = {
    use hir_def::{attrs as at, nameres::assoc as items, signatures as sig};
    use hir_ty::kinds as ty;
    &[
        sig::ImplSignature_served::KIND_NAME,
        sig::ImplSignature_lowered::KIND_NAME,
        items::ImplItems_of::KIND_NAME,
        ty::impl_trait_of_impl::KIND_NAME,
        ty::impl_self_ty_of_impl::KIND_NAME,
        ty::impl_has_unconstrained_params::KIND_NAME,
        sig::StructSignature_served::KIND_NAME,
        sig::StructSignature_lowered::KIND_NAME,
        ty::fn_sig_for_struct_constructor::KIND_NAME,
        sig::EnumSignature_served::KIND_NAME,
        sig::EnumSignature_lowered::KIND_NAME,
        sig::UnionSignature_served::KIND_NAME,
        sig::UnionSignature_lowered::KIND_NAME,
        ty::representability::KIND_NAME,
        ty::field_types_of_variant::KIND_NAME,
        ty::batched_field_types::KIND_NAME,
        ty::ctor_variances_query::KIND_NAME,
        ty::fn_sig_for_enum_variant_constructor::KIND_NAME,
        sig::TraitSignature_served::KIND_NAME,
        sig::TraitSignature_lowered::KIND_NAME,
        items::TraitItems_query_with_diagnostics::KIND_NAME,
        ty::supertraits_info::KIND_NAME,
        ty::dyn_compatibility_of_trait_query::KIND_NAME,
        sig::FunctionSignature_served::KIND_NAME,
        sig::FunctionSignature_lowered::KIND_NAME,
        ty::fn_sig_for_fn::KIND_NAME,
        ty::impl_rpitit_tys::KIND_NAME,
        ty::rpitit_item_bounds::KIND_NAME,
        ty::rpit_hidden_types::KIND_NAME,
        ty::tait_hidden_types::KIND_NAME,
        ty::generic_predicates_of::KIND_NAME,
        ty::variances_of_query::KIND_NAME,
        ty::const_conditions_query::KIND_NAME,
        ty::ItemBounds_of::KIND_NAME,
        at::AttrFlags_query::KIND_NAME,
        ty::target_data_layout_query::KIND_NAME,
        ty::crate_impl_traits::KIND_NAME,
        ty::crate_impl_self_tys::KIND_NAME,
        ty::crate_field_types::KIND_NAME,
        ty::crate_impl_predicates::KIND_NAME,
        ty::relevant_impls::KIND_NAME,
        ty::InherentImpls_in_crate_for_self_ty::KIND_NAME,
        ty::TraitImpls_for_crate_and_trait::KIND_NAME,
        ty::in_crate_trait_value_items::KIND_NAME,
        ty::module_items_named::KIND_NAME,
        ty::TraitImpls_for_block::KIND_NAME,
        ty::InherentImpls_for_block::KIND_NAME,
        ty::InherentImpls_in_block_for_self_ty::KIND_NAME,
        ty::TraitImpls_in_block_for_trait_and_self_ty::KIND_NAME,
        ty::may_specialize::KIND_NAME,
        ty::specializes_query::KIND_NAME,
    ]
};

/// Whether `krate` sees the tables of `read`: it is `krate` or a crate `krate` loads.
fn loads(db: &Db, krate: Crate, read: Crate) -> bool {
    read == krate || loaded_crates(db, krate).contains(&read)
}

/// The item whose interface `ItemBounds::of` is part of: an associated type's trait, which
/// hashes its items, another type alias itself, or the item an `impl Trait` is written in.
fn bounds_owner(db: &Db, of: ItemWithBounds<'_>) -> Option<ItemId> {
    Some(match of {
        ItemWithBounds::TypeAliasId(alias) => match alias.lookup(db).container {
            ItemContainerId::TraitId(it) => ItemId::Trait(it),
            ItemContainerId::ModuleId(_)
            | ItemContainerId::ImplId(_)
            | ItemContainerId::ExternBlockId(_) => ItemId::Alias(alias),
        },
        ItemWithBounds::InternedOpaqueTyId(opaque) => key::opaque_of(db, opaque).0,
    })
}

/// The item whose interface hashes `owner`'s attribute flags: the item, or a variant's enum.
fn attrs_owner(db: &Db, owner: AttrDefId) -> Option<ItemId> {
    Some(match owner {
        AttrDefId::AdtId(it) => ItemId::Adt(it),
        AttrDefId::FunctionId(it) => ItemId::Function(it),
        AttrDefId::EnumVariantId(it) => ItemId::Adt(AdtId::EnumId(it.lookup(db).parent)),
        AttrDefId::StaticId(it) => ItemId::Static(it),
        AttrDefId::ConstId(it) => ItemId::Const(it),
        AttrDefId::TraitId(it) => ItemId::Trait(it),
        AttrDefId::TypeAliasId(it) => ItemId::Alias(it),
        AttrDefId::ImplId(it) => ItemId::Impl(it),
        AttrDefId::ModuleId(_)
        | AttrDefId::MacroId(_)
        | AttrDefId::ExternBlockId(_)
        | AttrDefId::ExternCrateId(_)
        | AttrDefId::UseId(_) => return None,
    })
}
