//! The value of each fact a body's shape lists (docs/v2/plan/design/item-hash.md section 5):
//! lane items' for their kinds ([`blaze_items::kinds`]), this crate's for its own ([`kinds`]).
//!
//! This crate's facts cover what an item's interface hash leaves out and a body's steps read:
//!
//! - `Fields(item)`: the names of a struct's or union's fields, of an enum's variants and each
//!   one's fields, of a trait's or impl's items, in order, over the item's signature hash. The
//!   signature hash names members by place (`blaze_items::form`), so a renamed field, variant
//!   or method leaves it as it was.
//! - `Attrs(owner)`: all of an item's, variant's or module's attribute flags, its `#[unstable]`
//!   feature and a function's target features. The interface hash leaves out the flags it
//!   calls unobserved, and inference reads one of them (`#[diagnostic::do_not_recommend]`,
//!   in the trait error it reports).
//! - `CrateRoot(crate)`: what a crate's root decides for every body in it: its lang items by
//!   item key, the crates it loads and depends on by crate key, its `#![feature]`s and its
//!   recursion limit. Unlike `blaze_items::Fact::Crate` it holds no file of the crate, so an edit
//!   of a body leaves it as it was.
//!
//! A fact whose subject no longer resolves has the value [`absent`]; one that names what is not
//! its kind's (a record is data, which this build may not have written) has none.

use base_db::{Crate, Db};
use blaze_ids::{ContentId, Hasher};
use blaze_items::{ItemId, ItemKey, Resolved, goals::Goals, key};
use hir_def::{
    AdtId, AssocItemId, AttrDefId, DefWithBodyId, VariantId,
    attrs::AttrFlags,
    lang_item::{LangItemEnum, lang_items},
    nameres::crate_recursion_limit,
    unstable_features::UnstableFeatures,
};
use hir_expand::name::Name;
use hir_ty::{
    body_store::{Step, step_value},
    method_resolution::loaded_crates,
    next_solver::goal_store::{FactObject, FactRef, GoalFacts},
};
use smallvec::SmallVec;

/// This crate's facts' query kinds, in `0x1800..0x1810` of [`blaze_query::kinds::FACTS`]
/// (reserved for lane item-hash by request 00814).
pub mod kinds {
    use blaze_query::QueryKind;

    /// Every kind this crate may give a fact.
    pub const RANGE: std::ops::Range<u16> = 0x1800..0x1810;

    pub const FIELDS: QueryKind = QueryKind(0x1800);
    pub const ATTRS: QueryKind = QueryKind(0x1801);
    pub const CRATE_ROOT: QueryKind = QueryKind(0x1802);
    /// A body's inference, as a later step of it reads it.
    pub const STEP_INFER: QueryKind = QueryKind(0x1803);
    /// A body's built MIR with its closures' and async coroutines', as its borrow check reads it.
    pub const STEP_MIR: QueryKind = QueryKind(0x1804);
    /// Whether a body's match checking failed, which keeps its borrow check from running.
    pub const MATCH_CHECK: QueryKind = QueryKind(0x1805);
}

const FIELDS_DOMAIN: &str = "blaze v2 item-hash fields 1";
const ATTRS_DOMAIN: &str = "blaze v2 item-hash attrs 1";
const CRATE_ROOT_DOMAIN: &str = "blaze v2 item-hash crate root 1";
const MATCH_CHECK_DOMAIN: &str = "blaze v2 item-hash match check 1";
const ABSENT_DOMAIN: &str = "blaze v2 item-hash absent 1";

/// The value `fact` has now, `None` when it cannot be hashed or names what is no fact.
pub fn value(db: &Db, fact: FactRef) -> Option<ContentId> {
    if !kinds::RANGE.contains(&fact.kind.0) {
        return Goals.value(db, fact);
    }
    let FactObject::None = fact.object else { return None };
    let kind = fact.kind;
    if kind == kinds::FIELDS {
        match key::resolve(db, ItemKey(fact.subject)) {
            None => Some(absent(kind)),
            Some(Resolved::Item(item)) => fields_of(db, item),
            Some(_) => None,
        }
    } else if kind == kinds::ATTRS {
        match key::resolve(db, ItemKey(fact.subject)) {
            None => Some(absent(kind)),
            Some(resolved) => Some(attrs_of(db, attr_owner(resolved)?)),
        }
    } else if kind == kinds::CRATE_ROOT {
        match key::resolve_crate(db, &fact.subject) {
            None => Some(absent(kind)),
            Some(krate) => Some(crate_root_of(db, krate)),
        }
    } else if kind == kinds::STEP_INFER || kind == kinds::STEP_MIR {
        let step = if kind == kinds::STEP_INFER { Step::Infer } else { Step::Mir };
        match body_of(db, fact.subject)? {
            None => Some(absent(kind)),
            Some(body) => step_value(db, body, step),
        }
    } else if kind == kinds::MATCH_CHECK {
        match body_of(db, fact.subject)? {
            None => Some(absent(kind)),
            Some(body) => {
                let failed = hir_ty_matchck::match_check(db, body).failed;
                Some(Hasher::new(MATCH_CHECK_DOMAIN).update(&[u8::from(failed)]).finish())
            }
        }
    } else {
        None
    }
}

/// The body a step fact's subject names: `Some(None)` when it no longer resolves, `None` when
/// it names what has no body.
fn body_of(db: &Db, subject: ContentId) -> Option<Option<DefWithBodyId>> {
    Some(Some(match key::resolve(db, ItemKey(subject)) {
        None => return Some(None),
        Some(Resolved::Item(ItemId::Function(it))) => DefWithBodyId::FunctionId(it),
        Some(Resolved::Item(ItemId::Const(it))) => DefWithBodyId::ConstId(it),
        Some(Resolved::Item(ItemId::Static(it))) => DefWithBodyId::StaticId(it),
        Some(Resolved::Variant(it)) => DefWithBodyId::VariantId(it),
        Some(_) => return None,
    }))
}

/// The fact of `step` of `def`, as a later step of it reads it; `None` for the borrow check,
/// which no step reads.
pub fn step_fact(db: &Db, def: DefWithBodyId, step: Step) -> Option<FactRef> {
    let kind = match step {
        Step::Infer => kinds::STEP_INFER,
        Step::Mir => kinds::STEP_MIR,
        Step::Borrowck => return None,
    };
    Some(FactRef { kind, subject: body_key(db, def), object: FactObject::None })
}

/// The `MatchCheck` fact of `def`, as its borrow check reads it.
pub fn match_check(db: &Db, def: DefWithBodyId) -> FactRef {
    FactRef { kind: kinds::MATCH_CHECK, subject: body_key(db, def), object: FactObject::None }
}

fn body_key(db: &Db, def: DefWithBodyId) -> ContentId {
    match def {
        DefWithBodyId::FunctionId(it) => key::item_key(db, ItemId::Function(it)),
        DefWithBodyId::ConstId(it) => key::item_key(db, ItemId::Const(it)),
        DefWithBodyId::StaticId(it) => key::item_key(db, ItemId::Static(it)),
        DefWithBodyId::VariantId(it) => key::variant_key(db, it),
    }
    .0
}

/// The value of a fact of `kind` whose subject this database has no item for.
pub fn absent(kind: blaze_query::QueryKind) -> ContentId {
    let mut h = Hasher::new(ABSENT_DOMAIN);
    h.update(&kind.0.to_le_bytes());
    h.finish()
}

/// The `Fields` fact of `item`: a struct, union, enum, trait or impl.
pub fn fields(db: &Db, item: ItemId) -> FactRef {
    FactRef { kind: kinds::FIELDS, subject: key::item_key(db, item).0, object: FactObject::None }
}

/// The `Attrs` fact of `owner`, if its attributes have a key: an extern block's, an
/// `extern crate`'s or a `use`'s have none.
pub fn attrs(db: &Db, owner: AttrDefId) -> Option<FactRef> {
    let subject = match owner {
        AttrDefId::ModuleId(it) => key::module_key(db, it),
        AttrDefId::EnumVariantId(it) => key::variant_key(db, it),
        AttrDefId::AdtId(it) => key::item_key(db, ItemId::Adt(it)),
        AttrDefId::FunctionId(it) => key::item_key(db, ItemId::Function(it)),
        AttrDefId::StaticId(it) => key::item_key(db, ItemId::Static(it)),
        AttrDefId::ConstId(it) => key::item_key(db, ItemId::Const(it)),
        AttrDefId::TraitId(it) => key::item_key(db, ItemId::Trait(it)),
        AttrDefId::TypeAliasId(it) => key::item_key(db, ItemId::Alias(it)),
        AttrDefId::MacroId(it) => key::item_key(db, ItemId::Macro(it)),
        AttrDefId::ImplId(it) => key::item_key(db, ItemId::Impl(it)),
        AttrDefId::ExternBlockId(_) | AttrDefId::ExternCrateId(_) | AttrDefId::UseId(_) => {
            return None;
        }
    };
    Some(FactRef { kind: kinds::ATTRS, subject: subject.0, object: FactObject::None })
}

/// The `CrateRoot` fact of `krate`.
pub fn crate_root(db: &Db, krate: Crate) -> FactRef {
    FactRef { kind: kinds::CRATE_ROOT, subject: key::crate_key(db, krate), object: FactObject::None }
}

/// The attribute owner a key resolved to, if attributes have one there.
fn attr_owner(resolved: Resolved) -> Option<AttrDefId> {
    Some(match resolved {
        Resolved::Module(it) => AttrDefId::ModuleId(it),
        Resolved::Variant(it) => AttrDefId::EnumVariantId(it),
        Resolved::Item(ItemId::Function(it)) => AttrDefId::FunctionId(it),
        Resolved::Item(ItemId::Adt(it)) => AttrDefId::AdtId(it),
        Resolved::Item(ItemId::Trait(it)) => AttrDefId::TraitId(it),
        Resolved::Item(ItemId::Impl(it)) => AttrDefId::ImplId(it),
        Resolved::Item(ItemId::Const(it)) => AttrDefId::ConstId(it),
        Resolved::Item(ItemId::Static(it)) => AttrDefId::StaticId(it),
        Resolved::Item(ItemId::Alias(it)) => AttrDefId::TypeAliasId(it),
        Resolved::Item(ItemId::Macro(it)) => AttrDefId::MacroId(it),
        Resolved::Closure { .. }
        | Resolved::Opaque { .. }
        | Resolved::BuiltinDerive(_)
        | Resolved::Ctor(_)
        | Resolved::AnonConst { .. } => return None,
    })
}

/// The names of `item`'s members, in order, over its signature hash, which infers no body, so
/// a body's inference may ask it of its own owner; `None` for an item with no members of this
/// kind or no signature hash.
#[blaze_query::tracked(returns(copy))]
fn fields_of(db: &Db, item: ItemId) -> Option<ContentId> {
    let signature = blaze_items::signature_hash(db, item).ok()?;
    let mut h = Hasher::new(FIELDS_DOMAIN);
    h.id(&signature.0);
    match item {
        ItemId::Adt(AdtId::StructId(it)) => variant_names(db, &mut h, it.into()),
        ItemId::Adt(AdtId::UnionId(it)) => variant_names(db, &mut h, it.into()),
        ItemId::Adt(AdtId::EnumId(it)) => {
            let variants = &it.enum_variants(db).variants;
            h.update(&(variants.len() as u64).to_le_bytes());
            for (name, &(variant, _)) in variants.iter() {
                h.part(name.as_str().as_bytes());
                variant_names(db, &mut h, variant.into());
            }
        }
        ItemId::Trait(it) => member_names(&mut h, &it.trait_items(db).items),
        ItemId::Impl(it) => member_names(&mut h, &it.impl_items(db).items),
        ItemId::Function(_)
        | ItemId::Const(_)
        | ItemId::Static(_)
        | ItemId::Alias(_)
        | ItemId::Macro(_) => return None,
    }
    Some(h.finish())
}

/// A variant's field names in order, and whether each has a default (`field: T = value`),
/// which a struct expression's `..` reads.
fn variant_names(db: &Db, h: &mut Hasher, variant: VariantId) {
    let fields = variant.fields(db).fields();
    h.update(&(fields.len() as u64).to_le_bytes());
    for (_, field) in fields.iter() {
        h.part(field.name.as_str().as_bytes()).update(&[field.default_value.is_some() as u8]);
    }
}

fn member_names(h: &mut Hasher, items: &[(Name, AssocItemId)]) {
    h.update(&(items.len() as u64).to_le_bytes());
    for (name, item) in items {
        let kind = match item {
            AssocItemId::FunctionId(_) => 0u8,
            AssocItemId::ConstId(_) => 1,
            AssocItemId::TypeAliasId(_) => 2,
        };
        h.update(&[kind]).part(name.as_str().as_bytes());
    }
}

/// Everything a read of `owner`'s attributes in a step can give: the flags, the `#[unstable]`
/// feature, and a function's target features, sorted.
#[blaze_query::tracked(returns(copy))]
fn attrs_of(db: &Db, owner: AttrDefId) -> ContentId {
    let flags = AttrFlags::query(db, owner);
    let mut h = Hasher::new(ATTRS_DOMAIN);
    h.update(&flags.bits().to_le_bytes());
    match flags.unstable_feature(db, owner) {
        Some(feature) => h.update(&[1]).part(feature.as_str().as_bytes()),
        None => h.update(&[0]),
    };
    if let AttrDefId::FunctionId(function) = owner {
        let features = AttrFlags::target_features(db, function).iter().map(|it| it.as_str());
        // alloc: none for up to 8 target features, the most a function names in practice.
        let mut features: SmallVec<[&str; 8]> = features.collect();
        features.sort_unstable();
        h.update(&(features.len() as u64).to_le_bytes());
        for feature in features {
            h.part(feature.as_bytes());
        }
    }
    h.finish()
}

/// A crate's lang items by item key, the crates it loads and depends on by crate key (itself
/// as a mark: its key is of all its files), its features, sorted, and its recursion limit.
#[blaze_query::tracked(returns(copy))]
fn crate_root_of(db: &Db, krate: Crate) -> ContentId {
    let mut h = Hasher::new(CRATE_ROOT_DOMAIN);
    let lang = lang_items(db, krate);
    for (at, item) in LangItemEnum::ALL.iter().enumerate() {
        let Some(target) = item.from_lang_items(lang) else { continue };
        h.update(&(at as u32).to_le_bytes()).id(&key::lang_item_key(db, target).0);
    }
    let crates = |h: &mut Hasher, tag: &[u8], list: &[Crate]| {
        h.part(tag).update(&(list.len() as u64).to_le_bytes());
        for &it in list {
            if it == krate {
                h.update(&[0]);
            } else {
                h.update(&[1]).id(&key::crate_key(db, it));
            }
        }
    };
    crates(&mut h, b"loaded", loaded_crates(db, krate));
    crates(&mut h, b"deps", krate.transitive_deps(db));
    let features = UnstableFeatures::query(db, krate).iter().map(|it| it.as_str());
    // alloc: none for up to 16 features; a crate past that spills once per crate root change.
    let mut features: SmallVec<[&str; 16]> = features.collect();
    features.sort_unstable();
    h.part(b"features").update(&(features.len() as u64).to_le_bytes());
    for feature in features {
        h.part(feature.as_bytes());
    }
    h.part(b"recursion limit").update(&crate_recursion_limit(db, krate).to_le_bytes());
    h.finish()
}
