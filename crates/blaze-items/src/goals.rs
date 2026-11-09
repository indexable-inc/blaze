//! Lane items' part of the persistent goal cache (`hir_ty::next_solver::goal_store`, notes in
//! docs/v2/plan/design/frontend.md section 3): item keys for a goal's
//! definitions, its crate's key, and the facts its reads stand for.
//!
//! A read stands for a fact when its query is keyed by one item and what the query returns for
//! that item is part of what the fact hashes (the notes' table, section 3.4). A read of a
//! dependency's batch (its impls' traits, self types and predicates, its fields' types, lowered
//! for the whole crate at once) stands for the crate's [`Fact::Crate`]. A read of another
//! crate-wide table, or of a query keyed by several arguments, names no one item: it stands for
//! no fact, and the goal is not stored.

use base_db::{Crate, Db};
use blaze_ids::{ContentId, Hasher};
use blaze_query::db::{Database as _, FromIdIn, Id};
use hir_def::{
    AdtId, AttrDefId, EnumVariantId, GenericDefId, HasModule, ItemContainerId, Lookup, TypeAliasId,
    VariantId,
    lang_item::{LangItemEnum, lang_items},
};
use hir_ty::{
    db::InternedOpaqueTyId,
    method_resolution::{ImplPair, RelevantImplsKey, SelfTyKey, loaded_crates},
    next_solver::{
        SolverDefId,
        goal_store::{FactRef, GoalFacts},
    },
};

use crate::{Fact, ItemId, ItemKey, facts, key, table};

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
    /// store: `None`.
    fn fact_of(
        &self,
        db: &Db,
        krate: Option<Crate>,
        read: blaze_query::db::DatabaseKeyIndex,
        name: &'static str,
    ) -> Option<FactRef> {
        Some(table::fact_ref(&fact_of_read(db, krate?, name, read.key_index())?))
    }

    fn value(&self, db: &Db, krate: Option<Crate>, fact: FactRef) -> Option<ContentId> {
        fact_value(db, fact_ref_key(db, krate?, fact))
    }
}

/// A fact as the goal store keeps it, with the crate that asks it where its value depends on
/// the asker (`facts::asker`), interned: the key of [`fact_value`]. Made by [`fact_ref_key`].
#[blaze_query::interned(debug)]
pub struct FactRefKey<'db> {
    /// The asking crate for a fact of which impls and traits it sees (`Implements`, `Method`,
    /// `Vtable`); `None` for every other fact, whose value is one for every asker.
    #[returns(copy)]
    pub seen_by: Option<Crate>,
    #[returns(copy)]
    pub fact: FactRef,
}

/// The key of `fact` asked by `krate`: the asker kept only where the value depends on it, so a
/// fact like a signature or a layout is one memo, not one per crate that asks it.
pub fn fact_ref_key(db: &Db, krate: Crate, fact: FactRef) -> FactRefKey<'_> {
    FactRefKey::new(db, facts::asker(krate, fact.kind), fact)
}

/// The value of a fact now as `key.seen_by` sees it (`facts::value`), `None` for parts that are
/// no fact or a fact that cannot be hashed. One query, so that a body step that asks a value
/// reads `fact_value` of that fact, never the queries the value is computed from: a reader of
/// the step's reads names the fact and its asker exactly.
#[blaze_query::tracked(returns(copy))]
pub fn fact_value<'db>(db: &'db Db, key: FactRefKey<'db>) -> Option<ContentId> {
    let fact = table::fact_of_ref(key.fact(db))?;
    // A key made other than by `fact_ref_key` (a fact asked by a crate with none) is no fact.
    if key.seen_by(db).is_some() != facts::asked_by_crate(fact.kind()) {
        return None;
    }
    facts::value_as(db, key.seen_by(db), &fact).ok()
}

/// A crate's identity and its lang items by item key: what an interner of it reads when it
/// is made, outside a goal's evaluation. Then the crates it loads, in order, by crate key:
/// which crates' impls a `relevant_impls` read of it searches, the same ones its `Implements`
/// facts cover (`facts::value` as it sees them).
#[blaze_query::tracked(returns(copy))]
fn goal_crate_key(db: &Db, krate: Crate) -> ContentId {
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

/// The fact a read of the query `name` (its ingredient's debug name) of `key`, made by a goal of
/// `krate`, stands for, its value asked as `krate` sees it. Each
/// query here is keyed by one interned or input struct, taken for one only when the key's page
/// holds that struct, and returns part of an item's interface (its signature, bounds, fields, items,
/// variances, attribute flags: `crate::form`'s signature form), the hidden types of an item's
/// `impl Trait`s, an impl method's types for the trait method's `impl Trait`s, the crate's
/// target, the impls of one trait for one simplified self type, whether an impl may specialize
/// and what it specializes, or a dependency's batch, which stands for its crate.
///
/// `ImplItems::of` and `TraitItems::query_with_diagnostics` are read by name where the solver
/// finds the impl item of a trait item (`fetch_eligible_assoc_item`): the impl's signature form
/// writes, for each trait item in order, the impl item that name finds, so a rename that
/// changes what it finds changes the impl's signature.
pub(crate) fn fact_of_read(db: &Db, krate: Crate, name: &str, key: Id) -> Option<Fact> {
    let signature = |id: ItemId| Some(Fact::Signature(key::item_key(db, id)));
    match name {
        "ImplSignature::kept"
        | "ImplItems::of"
        | "impl_trait_of_impl"
        | "impl_self_ty_of_impl"
        | "impl_has_unconstrained_params" => signature(ItemId::Impl(arg(db, key)?)),
        "StructSignature::kept" | "fn_sig_for_struct_constructor" => {
            signature(ItemId::Adt(AdtId::StructId(arg(db, key)?)))
        }
        "EnumSignature::kept" => signature(ItemId::Adt(AdtId::EnumId(arg(db, key)?))),
        "UnionSignature::kept" => signature(ItemId::Adt(AdtId::UnionId(arg(db, key)?))),
        "representability" => signature(ItemId::Adt(arg(db, key)?)),
        "field_types_of_variant" | "ctor_variances_query" => {
            signature(ItemId::Adt(arg::<VariantId>(db, key)?.adt_id(db)))
        }
        "fn_sig_for_enum_variant_constructor" => {
            let variant: EnumVariantId = arg(db, key)?;
            signature(ItemId::Adt(AdtId::EnumId(variant.lookup(db).parent)))
        }
        "TraitSignature::kept"
        | "TraitItems::query_with_diagnostics"
        | "supertraits_info"
        | "dyn_compatibility_of_trait_query" => signature(ItemId::Trait(arg(db, key)?)),
        "FunctionSignature::kept" | "fn_sig_for_fn" => signature(ItemId::Function(arg(db, key)?)),
        // An impl method's types for the `impl Trait`s of the trait method it implements: a
        // fact of their own, never its signature, which the goals finding them read (the
        // bounds of its own `impl Trait`s): see `Fact::RpititTys`.
        "impl_rpitit_tys" => Some(facts::rpitit_tys(db, arg(db, key)?)),
        // The bounds of a trait method's `impl Trait`, its trait item's: those its signature
        // writes on the opaque type. An impl's item has none.
        "rpitit_item_bounds" => signature(ItemId::Function(
            hir_ty::rpitit::rpitit_item_bounds_of_key(db, key)?.function,
        )),
        // A hidden type is what its defining bodies leak, which the signature hash leaves out.
        "rpit_hidden_types" => Some(facts::hidden_types(db, ItemId::Function(arg(db, key)?))),
        "tait_hidden_types" => Some(facts::hidden_types(db, ItemId::Alias(arg(db, key)?))),
        "generic_predicates_of" | "variances_of_query" | "const_conditions_query" => {
            signature(key::generic_def_item(arg::<GenericDefId>(db, key)?))
        }
        "ItemBounds::of" => signature(bounds_owner(db, key)?),
        "AttrFlags::query" => signature(attrs_owner(db, arg(db, key)?)?),
        "target_data_layout_query" => Some(Fact::Target(key::crate_key(db, arg(db, key)?))),
        "crate_impl_traits"
        | "crate_impl_self_tys"
        | "crate_field_types"
        | "crate_impl_predicates" => Some(facts::whole_crate(db, arg(db, key)?)),
        "relevant_impls" => {
            let read: RelevantImplsKey<'_> = arg(db, key)?;
            // A search of a crate `krate` loads (an impl's own crate, where `specialized_ancestor`
            // looks for the impl it specializes) covers crates `krate` loads too, as the loaded
            // set is closed: `krate`'s fact covers it, more widely. Another crate's search may
            // cover crates `krate` never loads: no fact.
            if read.krate(db) != krate && !loaded_crates(db, krate).contains(&read.krate(db)) {
                return None;
            }
            let trait_ = read.trait_(db);
            Some(match read.self_ty(db) {
                Some(SelfTyKey(self_ty)) => facts::implements(db, trait_, &self_ty),
                None => Fact::Implements {
                    trait_: key::item_key(db, ItemId::Trait(trait_)),
                    self_ty: key::unsimplified(),
                },
            })
        }
        "may_specialize" => Some(facts::specializes(db, arg(db, key)?)),
        // Whether one impl specializes another is solved in the specializing impl's crate, over
        // the parent's where clauses: it reads impls of any trait those name, which no fact of
        // either impl covers. The crate fact covers everything that crate sees.
        "specializes_query" => {
            let pair: ImplPair = arg(db, key)?;
            Some(Fact::Crate(key::crate_key(db, pair.specializing(db).krate(db))))
        }
        _ => None,
    }
}

/// `key` as a `T`, if its page holds `T`s.
fn arg<T: FromIdIn>(db: &Db, key: Id) -> Option<T> {
    T::from_id_in(db.storage(), key)
}

/// The item whose interface `ItemBounds::of(key)` is part of: an associated type's trait, which
/// hashes its items, another type alias itself, or the item an `impl Trait` is written in.
fn bounds_owner(db: &Db, key: Id) -> Option<ItemId> {
    let storage = db.storage();
    if let Some(alias) = TypeAliasId::from_id_in(storage, key) {
        return Some(match alias.lookup(db).container {
            ItemContainerId::TraitId(it) => ItemId::Trait(it),
            ItemContainerId::ModuleId(_)
            | ItemContainerId::ImplId(_)
            | ItemContainerId::ExternBlockId(_) => ItemId::Alias(alias),
        });
    }
    let opaque = InternedOpaqueTyId::from_id_in(storage, key)?;
    Some(key::opaque_of(db, opaque).0)
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
