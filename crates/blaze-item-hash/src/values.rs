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
//! - `TraitsInScope(module)`: the traits in scope in a module (its blocks', its own and the
//!   prelude's, as `module_trait_set` gives them), by key, sorted: what a read of a scope's
//!   traits by a body's step stands for (docs/v2/plan/design/restore.md, "One form, a
//!   function: the end state").
//! - `WholeScope(module)`: every name of a module's scope with what it is there (each name's
//!   `Resolves` value): what a read of a whole scope stands for, an iteration and never a
//!   lookup. Any name added to the module changes it; it is per module, never per crate.
//! - `FieldOrder`: the field order plan every layout reads (`hir_ty::layout::field_order`), by
//!   its content ID, or that there is none: one fact for the build, no subject but a tag.
//! - `Setting(field)`: a field of the front end's settings (`base_db::FrontSettings`) a step
//!   reads, by the field's name: one fact per field for the build, each read listed under the
//!   field's own kind (`base_db::SettingField`), so a step is moved by the settings it read
//!   and by no other (the borrow check by its mode and `borrowck_debug`, a body's tree by
//!   `check_relative_bodies`).
//! - `CrateNames(crate)`: what a crate's def map holds outside its modules' scopes (its
//!   preludes, root flags, features, recursion limit, exported derives and proc macros, blaze
//!   edition): what a read of the crate's resolution or of its blaze edition stands for.
//! - No fact of a block's scope: a block's def map is no boundary
//!   (`hir_def::nameres::boundary`), so what its resolution read is in its reader's list as
//!   the rows above, and the block's own text is the text of the body that holds it, covered
//!   by that body's key. Kind `0x180e` was that fact (`BlockScope`) and is retired: never
//!   given again, so a record that names it is never read as another fact.
//! - `Modules(crate)`: every module of a crate in order, by key, visibility and origin kind:
//!   what a walk of the crate's modules read.
//! - `InherentAssocTypes(adt)`: the associated types of an ADT's inherent impls a path's
//!   lowering looks `Type::Name` up in, each by name, alias key and impl key: a list made by
//!   iterating every module of the ADT's crate, which no lookup fact stands for.
//!
//! A body also makes a value no other body's step reads as a fact of this crate: [`escaped`],
//! the digest of what its declared items show outside it (the keyed record's
//! `FactKind::Escaped`, made by the body's task).
//!
//! A name looked up in a module, found or not, in any namespace or in textual macro scope, is
//! lane items' `Resolves { scope, name }` ("nothing" is a value), and a macro's definition is
//! its `Signature`: neither has a kind here.
//!
//! A fact whose subject no longer resolves has the value [`absent`]; one that names what is not
//! its kind's (a record is data, which this build may not have written) has none.
//!
//! A fact about the body's own item names it as [`OWN`], never by its item key ([`relative`]):
//! a body's identity writes its own item as "self" (section 3.2), so two items alike but for
//! their names share one identity and one base key, and a shape that named each by its key
//! would be another shape for each of them (and for the same body after a rename). Its value
//! is asked for a body ([`value_in`]), which puts the body's own key back.

use std::alloc::Global;

use base_db::{Crate, Db, SettingField};
use blaze_ids::{ContentId, Hasher};
use blaze_items::{ItemId, ItemKey, Resolved, goals::Goals, key};
use hir_def::{
    AdtId, AssocItemId, AttrDefId, DefWithBodyId, HasModule as _, ModuleDefId, ModuleId,
    VariantId,
    attrs::AttrFlags,
    expr_store::Body,
    lang_item::{LangItemEnum, lang_items},
    nameres::{CrateName, ModuleOrigin, crate_def_map, crate_recursion_limit},
    unstable_features::UnstableFeatures,
    visibility::{Visibility, VisibilityExplicitness},
};
use hir_expand::name::Name;
use hir_ty::{
    body_store::{Step, step_value},
    layout::field_order::FieldOrder,
    method_resolution::{loaded_crates, scope_sets::module_trait_set},
    next_solver::goal_store::{FactObject, FactRef, GoalFacts},
};
use smallvec::SmallVec;

/// This crate's facts' query kinds, in `0x1800..0x1820` of [`blaze_query::kinds::FACTS`]
/// (`0x1800..0x1810` reserved for lane item-hash by request 00814, `0x1810..0x1820` taken
/// next to it once the first sixteen were all given; nothing else is in `0x1800..0x1820`).
pub mod kinds {
    use blaze_query::QueryKind;

    /// Every kind this crate may give a fact: a kind in it no constant below names has no
    /// value ([`super::value`]).
    pub const RANGE: std::ops::Range<u16> = 0x1800..0x1820;

    /// Every kind this crate gives a fact, once each.
    pub const ALL: &[QueryKind] = &[
        FIELDS,
        ATTRS,
        CRATE_ROOT,
        STEP_INFER,
        STEP_MIR,
        MATCH_CHECK,
        TRAITS_IN_SCOPE,
        WHOLE_SCOPE,
        CRATE_NAMES,
        FIELD_ORDER,
        SETTING,
        INHERENT_ASSOC_TYPES,
        STEP_LOWER,
        CRATE_SETTINGS,
        MODULES,
    ];

    pub const FIELDS: QueryKind = QueryKind(0x1800);
    pub const ATTRS: QueryKind = QueryKind(0x1801);
    pub const CRATE_ROOT: QueryKind = QueryKind(0x1802);
    /// A body's inference, as a later step of it reads it.
    pub const STEP_INFER: QueryKind = QueryKind(0x1803);
    /// A body's built MIR with its closures' and async coroutines', as its borrow check reads it.
    pub const STEP_MIR: QueryKind = QueryKind(0x1804);
    /// Whether a body's match checking failed, which keeps its borrow check from running.
    pub const MATCH_CHECK: QueryKind = QueryKind(0x1805);
    /// The traits in scope in a module.
    pub const TRAITS_IN_SCOPE: QueryKind = QueryKind(0x1806);
    /// Every name of a module's scope: what an iteration of the scope read.
    pub const WHOLE_SCOPE: QueryKind = QueryKind(0x1807);
    /// What a crate's def map holds outside its modules' scopes: its preludes, what its
    /// root's attributes decide for name resolution, whether it shadows a primitive type.
    pub const CRATE_NAMES: QueryKind = QueryKind(0x1808);
    /// The build's field order plan, which every ADT's layout reads.
    pub const FIELD_ORDER: QueryKind = QueryKind(0x1809);
    /// A field of the build's front-end settings, which a step reads.
    pub const SETTING: QueryKind = QueryKind(0x180a);
    /// The associated types of an ADT's inherent impls, by binding name, that a path's
    /// lowering looks a name up in (`Type::Name` of an ADT).
    pub const INHERENT_ASSOC_TYPES: QueryKind = QueryKind(0x180b);
    /// A body's own lowering, as each other step of it reads it: valued by the digest of
    /// what the lowering read (`hir_ty::body_store::Lowered::value`), not by its answer.
    pub const STEP_LOWER: QueryKind = QueryKind(0x180c);
    /// Another crate's settings, as a read of a field of its `Crate` input (`data`,
    /// `extra_data`, `cfg_options`, `workspace_data`) stands for them: its content ID
    /// (`base_db::Crate::content`), never its sources, so a body edit in it moves nothing.
    pub const CRATE_SETTINGS: QueryKind = QueryKind(0x180d);
    // 0x180e is retired (it was `BlockScope`, a block's def map as one fact; a block's
    // resolution is now read row by row, `hir_def::nameres::boundary`): never given again.
    /// Every module of a crate in its def map's order, each by key with its visibility and
    /// the kind of its origin: what a walk of the crate's modules read
    /// (`hir_def::nameres::boundary::all_modules`). No scope of any of them: each scope a
    /// walker reads is a row of its own.
    pub const MODULES: QueryKind = QueryKind(0x180f);
}

const FIELDS_DOMAIN: &str = "blaze v2 item-hash fields 1";
const ATTRS_DOMAIN: &str = "blaze v2 item-hash attrs 1";
const CRATE_ROOT_DOMAIN: &str = "blaze v2 item-hash crate root 1";
const MATCH_CHECK_DOMAIN: &str = "blaze v2 item-hash match check 1";
const ABSENT_DOMAIN: &str = "blaze v2 item-hash absent 1";
const TRAITS_DOMAIN: &str = "blaze v2 item-hash traits in scope 1";
const WHOLE_SCOPE_DOMAIN: &str = "blaze v2 item-hash whole scope 1";
const CRATE_NAMES_DOMAIN: &str = "blaze v2 item-hash crate names 1";
const FIELD_ORDER_DOMAIN: &str = "blaze v2 item-hash field order 1";
const SETTING_DOMAIN: &str = "blaze v2 item-hash setting 1";
const INHERENT_ASSOC_TYPES_DOMAIN: &str = "blaze v2 item-hash inherent assoc types 1";
const CRATE_SETTINGS_DOMAIN: &str = "blaze v2 item-hash crate settings 1";
const MODULES_DOMAIN: &str = "blaze v2 item-hash modules 1";
const ESCAPED_DOMAIN: &str = "blaze v2 item-hash escaped 1";
const ESCAPED_ITEM_DOMAIN: &str = "blaze v2 item-hash escaped item 1";

/// Why a body has no [`escaped`] digest: an item it declares has no signature hash.
pub const ESCAPED_UNHASHED: &str = "a body's escaped items (`blaze_item_hash::values::escaped`): an item declared in the body has no signature hash (`blaze_items::signature_hash` refused it)";

/// The subject of the [`kinds::SETTING`] fact of `FrontSettings::borrowck_debug`: a tag, which
/// no key is. The settings are one for the build, so the fact names nothing else.
const BORROWCK_DEBUG_SUBJECT: ContentId = ContentId(*b"blaze v2 item-hash: borrowck dbg");
/// The subject of the [`kinds::SETTING`] fact of `FrontSettings::check_relative_bodies`, as
/// [`BORROWCK_DEBUG_SUBJECT`].
const CHECK_RELATIVE_SUBJECT: ContentId = ContentId(*b"blaze v2 item-hash: check rel bd");

/// The subject of the [`kinds::SETTING`] fact of `FrontSettings::borrowck`, as
/// [`BORROWCK_DEBUG_SUBJECT`].
const BORROWCK_SUBJECT: ContentId = ContentId(*b"blaze v2 item-hash: borrowck mod");
/// The subject of the [`kinds::SETTING`] fact of `FrontSettings::trace_holders`, as
/// [`BORROWCK_DEBUG_SUBJECT`].
const TRACE_HOLDERS_SUBJECT: ContentId = ContentId(*b"blaze v2 item-hash: trace holder");

/// The subject of the [`kinds::FIELD_ORDER`] fact: a tag, which no key is. The plan is one
/// for the build, so the fact names nothing else.
const FIELD_ORDER_SUBJECT: ContentId = ContentId(*b"blaze v2 item-hash: field order!");

/// The subject of a fact about the body's own item (a function, const or static, or a
/// discriminant's variant), in place of its key: a tag, which no hash is. What it stands for
/// is known only with the body ([`value_in`]).
pub const OWN: ContentId = ContentId(*b"blaze v2 item-hash: a body's own");

/// `fact` as a shape of `def`'s steps lists it: a fact whose subject is `def`'s own key names
/// it as [`OWN`]. Its object is left as it is.
pub fn relative(db: &Db, def: DefWithBodyId, fact: FactRef) -> FactRef {
    match fact.subject == body_key(db, def) {
        true => FactRef { subject: OWN, ..fact },
        false => fact,
    }
}

/// The value `fact`, of a shape of `def`'s steps, has now: [`value`] as `def`'s crate sees it,
/// of `def`'s own item where the fact names [`OWN`].
pub fn value_in(db: &Db, def: DefWithBodyId, fact: FactRef) -> Option<ContentId> {
    let fact = match fact.subject == OWN {
        true => FactRef { subject: body_key(db, def), ..fact },
        false => fact,
    };
    value(db, def.krate(db), fact)
}

/// The value `fact` has now as a body of `krate` sees it (lane items' facts about impls cover
/// the crates it loads), `None` when it cannot be hashed or names what is no fact. A fact
/// that names [`OWN`] has no value here, where no body says whose it is ([`value_in`]): it
/// must never read as [`absent`], which every body's would then share.
///
/// [`value_or_why`] with the reason dropped: the one place a value is made.
pub fn value(db: &Db, krate: Crate, fact: FactRef) -> Option<ContentId> {
    value_or_why(db, krate, fact).ok()
}

/// Why a fact has no value now ([`value_or_why`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NoValue {
    /// Its value is a hash of an item that was refused: the refusal, which names the item and
    /// what could not be hashed ([`blaze_items::Unhashable::describe`]).
    Unhashable(blaze_items::Unhashable),
    /// Every other reason, by rule: what the fact names is not its kind's, or a value it is
    /// made of has none.
    Why(&'static str),
}

/// Why: a fact naming [`OWN`] asked with no body to say whose it is.
pub const NO_VALUE_OWN: &str = "a fact naming a body's own item (`OWN`), asked with no body (`values::value_in` asks it)";
/// Why: a fact of this crate's kinds asked with an object, which none of them has.
pub const NO_VALUE_OBJECT: &str = "a fact of blaze-item-hash's kinds asked with an object, which none of its kinds has";
/// Why: a subject that resolves to what is not of the fact's kind (a record is data, which this
/// build may not have written).
pub const NO_VALUE_NOT_ITS_KIND: &str = "a fact whose subject resolves to what is not of its kind";
/// Why: a query kind no fact has.
pub const NO_VALUE_NO_KIND: &str = "a query kind no fact of blaze-items or blaze-item-hash has";
/// Why: a lane items' fact `blaze_items::goals::fact_value` gave none, of a kind whose refusal
/// is not taken again here (only `Signature` and `Layout` are, by their hashes).
pub const NO_VALUE_LANE: &str = "a fact of blaze-items gave no value (`goals::fact_value`: no fact, or a hash refused that only blaze-items can say)";
/// Why: an item's member names with no signature hash refusal behind them.
pub const NO_VALUE_FIELDS: &str = "an item's member names (`Fields`): it has no members of this kind";
/// Why: a step of a body whose run was refused.
pub const NO_VALUE_STEP: &str = "a step of a body (`hir_ty::body_store::step_value`): the step's run was refused";
/// Why: a scope one of whose names has no `Resolves` value.
pub const NO_VALUE_SCOPE: &str = "a module's whole scope (`WholeScope`): one of its names has no `Resolves` value";

/// [`value`], or why it has none: the refusal of the item whose hash the value is
/// ([`NoValue::Unhashable`]), or the rule ([`NoValue::Why`]). Only a fact with no value is
/// asked again for its reason: a lane items' `Signature` or `Layout` asks its item's hash once
/// more (a memoized query), and keeps its refusal.
pub fn value_or_why(db: &Db, krate: Crate, fact: FactRef) -> Result<ContentId, NoValue> {
    use NoValue::Why;
    let not_its_kind = Why(NO_VALUE_NOT_ITS_KIND);
    if fact.subject == OWN {
        return Err(Why(NO_VALUE_OWN));
    }
    if !kinds::RANGE.contains(&fact.kind.0) {
        return Goals.value(db, Some(krate), fact).ok_or_else(|| lane_why(db, fact));
    }
    let FactObject::None = fact.object else { return Err(Why(NO_VALUE_OBJECT)) };
    let kind = fact.kind;
    if kind == kinds::FIELDS {
        match key::resolve(db, ItemKey(fact.subject)) {
            None => Ok(absent(kind)),
            Some(Resolved::Item(item)) => fields_of(db, item).ok_or_else(|| {
                match blaze_items::signature_hash(db, item) {
                    Err(refused) => NoValue::Unhashable(refused),
                    Ok(_) => Why(NO_VALUE_FIELDS),
                }
            }),
            Some(_) => Err(not_its_kind),
        }
    } else if kind == kinds::ATTRS {
        match key::resolve(db, ItemKey(fact.subject)) {
            None => Ok(absent(kind)),
            Some(resolved) => Ok(attrs_of(db, attr_owner(resolved).ok_or(not_its_kind)?)),
        }
    } else if kind == kinds::CRATE_ROOT {
        match key::resolve_crate(db, &fact.subject) {
            None => Ok(absent(kind)),
            Some(krate) => Ok(crate_root_of(db, krate)),
        }
    } else if kind == kinds::CRATE_SETTINGS {
        match key::resolve_crate(db, &fact.subject) {
            None => Ok(absent(kind)),
            Some(krate) => Ok(Hasher::new(CRATE_SETTINGS_DOMAIN).id(&krate.content(db)).finish()),
        }
    } else if kind == kinds::STEP_INFER || kind == kinds::STEP_MIR || kind == kinds::STEP_LOWER {
        let step = if kind == kinds::STEP_INFER {
            Step::Infer
        } else if kind == kinds::STEP_MIR {
            Step::Mir
        } else {
            Step::Lower
        };
        match body_of(db, fact.subject).ok_or(not_its_kind)? {
            None => Ok(absent(kind)),
            Some(body) => step_value(db, body, step).ok_or(Why(NO_VALUE_STEP)),
        }
    } else if kind == kinds::MATCH_CHECK {
        match body_of(db, fact.subject).ok_or(not_its_kind)? {
            None => Ok(absent(kind)),
            Some(body) => {
                let failed = hir_ty_matchck::match_check(db, body).failed;
                Ok(Hasher::new(MATCH_CHECK_DOMAIN).update(&[u8::from(failed)]).finish())
            }
        }
    } else if kind == kinds::CRATE_NAMES {
        match key::resolve(db, ItemKey(fact.subject)) {
            None => Ok(absent(kind)),
            Some(Resolved::Module(root)) => Ok(crate_names_of(db, root.krate(db))),
            Some(_) => Err(not_its_kind),
        }
    } else if kind == kinds::FIELD_ORDER {
        (fact.subject == FIELD_ORDER_SUBJECT).then(|| field_order_of(db)).ok_or(not_its_kind)
    } else if kind == kinds::SETTING {
        match fact.subject {
            BORROWCK_SUBJECT => Ok(setting_of(db, SettingField::Borrowck)),
            BORROWCK_DEBUG_SUBJECT => Ok(setting_of(db, SettingField::BorrowckDebug)),
            TRACE_HOLDERS_SUBJECT => Ok(setting_of(db, SettingField::TraceHolders)),
            CHECK_RELATIVE_SUBJECT => Ok(setting_of(db, SettingField::CheckRelativeBodies)),
            _ => Err(not_its_kind),
        }
    } else if kind == kinds::INHERENT_ASSOC_TYPES {
        match key::resolve(db, ItemKey(fact.subject)) {
            None => Ok(absent(kind)),
            Some(Resolved::Item(ItemId::Adt(adt))) => Ok(inherent_assoc_types_of(db, adt)),
            Some(_) => Err(not_its_kind),
        }
    } else if kind == kinds::TRAITS_IN_SCOPE || kind == kinds::WHOLE_SCOPE {
        match key::resolve(db, ItemKey(fact.subject)) {
            None => Ok(absent(kind)),
            Some(Resolved::Module(module)) if kind == kinds::TRAITS_IN_SCOPE => {
                Ok(traits_in_scope_of(db, module))
            }
            Some(Resolved::Module(module)) => whole_scope_of(db, module).ok_or(Why(NO_VALUE_SCOPE)),
            Some(_) => Err(not_its_kind),
        }
    } else if kind == kinds::MODULES {
        match key::resolve(db, ItemKey(fact.subject)) {
            None => Ok(absent(kind)),
            Some(Resolved::Module(root)) => Ok(modules_of(db, root.krate(db))),
            Some(_) => Err(not_its_kind),
        }
    } else {
        Err(Why(NO_VALUE_NO_KIND))
    }
}

/// Why a lane items' fact has no value: for a `Signature` or a `Layout`, its item's hash asked
/// again (a memoized query), whose refusal names the item and why; for every other kind the
/// rule alone ([`NO_VALUE_LANE`]), since only blaze-items can turn its parts into its fact.
fn lane_why(db: &Db, fact: FactRef) -> NoValue {
    let refused = match key::resolve(db, ItemKey(fact.subject)) {
        Some(Resolved::Item(item)) if fact.kind == blaze_items::kinds::SIGNATURE => {
            blaze_items::signature_hash(db, item).err()
        }
        Some(Resolved::Item(ItemId::Adt(adt))) if fact.kind == blaze_items::kinds::LAYOUT => {
            blaze_items::layout_hash(db, adt).err()
        }
        _ => None,
    };
    refused.map_or(NoValue::Why(NO_VALUE_LANE), NoValue::Unhashable)
}

/// The `FieldOrder` fact: what a read of the field order plan's input stands for.
pub fn field_order() -> FactRef {
    FactRef { kind: kinds::FIELD_ORDER, subject: FIELD_ORDER_SUBJECT, object: FactObject::None }
}

/// The plan's content ID, or that the build has none: what every layout reads of it. Equal
/// plans are equal IDs (`FieldOrderPlan`'s own equality).
fn field_order_of(db: &Db) -> ContentId {
    let plan = FieldOrder::try_get(db).and_then(|it| it.plan(db).as_ref().map(|plan| plan.id()));
    let mut h = Hasher::new(FIELD_ORDER_DOMAIN);
    match plan {
        Some(id) => h.update(&[1]).id(&id),
        None => h.update(&[0]),
    };
    h.finish()
}

/// The `Setting` fact a read of the front end's setting `field` stands for (a `Read::Input`
/// row of the field's own kind, `base_db::SettingField::kind`): one fact per field, so a step
/// is moved by the settings it read and by no other.
pub fn setting(field: SettingField) -> FactRef {
    let subject = match field {
        SettingField::Borrowck => BORROWCK_SUBJECT,
        SettingField::BorrowckDebug => BORROWCK_DEBUG_SUBJECT,
        SettingField::TraceHolders => TRACE_HOLDERS_SUBJECT,
        SettingField::CheckRelativeBodies => CHECK_RELATIVE_SUBJECT,
    };
    FactRef { kind: kinds::SETTING, subject, object: FactObject::None }
}

/// The value of `field` as the build has it: what a step reads of it. The borrow checker's mode
/// by its name, `BLAZE_BORROWCK_DEBUG` with whether it is set, the two switches by their bit.
/// A build without settings (no singleton) is the defaults' value.
fn setting_of(db: &Db, field: SettingField) -> ContentId {
    let mut h = Hasher::new(SETTING_DOMAIN);
    h.part(field.name().as_bytes());
    let Some(settings) = base_db::FrontSettings::try_get(db) else {
        h.update(&[0xff]);
        return h.finish();
    };
    match field {
        SettingField::Borrowck => {
            feed_debug(&mut h, &settings.borrowck(db));
        }
        SettingField::BorrowckDebug => match settings.borrowck_debug(db).as_deref() {
            Some(it) => {
                h.update(&[1]).part(it.as_bytes());
            }
            None => {
                h.update(&[0]);
            }
        },
        SettingField::TraceHolders => {
            h.update(&[u8::from(settings.trace_holders(db))]);
        }
        SettingField::CheckRelativeBodies => {
            h.update(&[u8::from(settings.check_relative_bodies(db))]);
        }
    }
    h.finish()
}

/// The `TraitsInScope` fact of `module`.
pub fn traits_in_scope(db: &Db, module: ModuleId) -> FactRef {
    FactRef {
        kind: kinds::TRAITS_IN_SCOPE,
        subject: key::module_key(db, module).0,
        object: FactObject::None,
    }
}

/// The `WholeScope` fact of `module`.
pub fn whole_scope(db: &Db, module: ModuleId) -> FactRef {
    FactRef {
        kind: kinds::WHOLE_SCOPE,
        subject: key::module_key(db, module).0,
        object: FactObject::None,
    }
}

/// The `CrateNames` fact of `krate`, named by its root module's key.
pub fn crate_names(db: &Db, krate: Crate) -> FactRef {
    FactRef {
        kind: kinds::CRATE_NAMES,
        subject: key::module_key(db, crate_def_map(db, krate).root).0,
        object: FactObject::None,
    }
}

/// What `krate`'s def map holds outside its modules' scopes (`hir_def::nameres::crate_names`),
/// each module and macro by key.
#[blaze_memo::tracked(returns(copy))]
fn crate_names_of(db: &Db, krate: Crate) -> ContentId {
    let mut h = Hasher::new(CRATE_NAMES_DOMAIN);
    // The rows the def map gives in its maps' order, each by keys, sorted below: an exported
    // derive and its helpers' names, a proc macro function and its macro.
    let mut keyed: Vec<(ContentId, ContentId), Global> = Vec::new_in(Global);
    hir_def::nameres::crate_names(db, krate, &mut |row| {
        match row {
            CrateName::Prelude(module) => {
                h.part(b"prelude").id(&key::module_key(db, module).0);
            }
            CrateName::Extern(name, root) => {
                h.part(b"extern")
                    .id(&key::name_hash(name.as_str()).0)
                    .id(&key::module_key(db, root).0);
            }
            CrateName::MacroUse(name, it) => {
                h.part(b"macro use")
                    .id(&key::name_hash(name.as_str()).0)
                    .id(&key::item_key(db, ItemId::Macro(it)).0);
            }
            CrateName::Tool(tool) => {
                h.part(b"tool").part(tool.as_bytes());
            }
            CrateName::Flags(flags, edition) => {
                h.part(b"flags").update(&(flags.len() as u64).to_le_bytes());
                for &flag in flags {
                    h.update(&[u8::from(flag)]);
                }
                h.update(&[edition as u8]);
            }
            CrateName::Feature(feature) => {
                h.part(b"feature").part(feature.as_bytes());
            }
            CrateName::Limit(limit) => {
                h.part(b"limit").update(&limit.to_le_bytes());
            }
            CrateName::Derive(it, helpers) => {
                let mut names = Hasher::new(CRATE_NAMES_DOMAIN);
                names.part(b"derive").update(&(helpers.len() as u64).to_le_bytes());
                for helper in helpers {
                    names.id(&key::name_hash(helper.as_str()).0);
                }
                keyed.push((key::item_key(db, ItemId::Macro(it)).0, names.finish()));
            }
            CrateName::ProcMacro(function, it) => {
                keyed.push((
                    key::item_key(db, ItemId::Function(function)).0,
                    key::item_key(db, ItemId::Macro(it.into())).0,
                ));
            }
            CrateName::BlazeEdition(edition) => {
                // The manifest's version, as the attribute says it: no number of an enum's.
                match edition {
                    None => h.part(b"blaze edition").update(&[0]),
                    Some(it) => h.part(b"blaze edition").update(&[1]).part(it.version().as_bytes()),
                };
            }
        }
    });
    keyed.sort_unstable();
    h.part(b"keyed").update(&(keyed.len() as u64).to_le_bytes());
    for (subject, value) in &keyed {
        h.id(subject).id(value);
    }
    h.finish()
}

/// The `Modules` fact of `krate`, named by its root module's key.
pub fn modules(db: &Db, krate: Crate) -> FactRef {
    FactRef {
        kind: kinds::MODULES,
        subject: key::module_key(db, crate_def_map(db, krate).root).0,
        object: FactObject::None,
    }
}

/// `krate`'s modules in its def map's order, each by key, its visibility (by the key of the
/// module it is restricted to) and the kind of its origin.
#[blaze_memo::tracked(returns(copy))]
fn modules_of(db: &Db, krate: Crate) -> ContentId {
    let mut h = Hasher::new(MODULES_DOMAIN);
    for (module, data) in crate_def_map(db, krate).modules(db) {
        h.id(&key::module_key(db, module).0);
        match data.visibility {
            Visibility::Public => {
                h.update(&[0]);
            }
            Visibility::PubCrate(_) => {
                h.update(&[1]);
            }
            Visibility::Module(to, explicitness) => {
                h.update(&[2, u8::from(explicitness == VisibilityExplicitness::Explicit)])
                    .id(&key::module_key(db, to).0);
            }
        }
        let origin = match data.origin {
            ModuleOrigin::CrateRoot { .. } => 0,
            ModuleOrigin::File { .. } => 1,
            ModuleOrigin::Inline { .. } => 2,
            ModuleOrigin::BlockExpr { .. } => 3,
        };
        h.update(&[origin]);
    }
    h.finish()
}

/// Lane items' `Resolves` fact of `name` in `module`: what the name is there in each
/// namespace and in textual macro scope, "nothing" a value like any.
pub fn resolves(db: &Db, module: ModuleId, name: &Name) -> FactRef {
    FactRef {
        kind: blaze_items::kinds::RESOLVES,
        subject: key::module_key(db, module).0,
        object: FactObject::Id(key::name_hash(name.as_str()).0),
    }
}

/// The `InherentAssocTypes` fact of `adt`: what a step's read of
/// `hir_ty::lower::path::inherent_assoc_types(adt)` stands for.
pub fn inherent_assoc_types(db: &Db, adt: AdtId) -> FactRef {
    FactRef {
        kind: kinds::INHERENT_ASSOC_TYPES,
        subject: key::item_key(db, ItemId::Adt(adt)).0,
        object: FactObject::None,
    }
}

/// The list `inherent_assoc_types(adt)` gives, in its order, each row by its binding name's
/// text and whether a `macro`'s context keys it, the alias's key and the impl's key. Sound as a
/// fact (keys-cover-every-input): the value is the query's whole result with each id written
/// as its item key, the same in every database, so two values are equal exactly when the
/// results are; what a reader does with a row (the impl's self type, the alias's type) is a
/// read of its own, recorded by its own rule. Not read through: the query iterates every
/// module of the ADT's crate (`Modules::table`, each scope's inherent impls), reads no single
/// fact names.
#[blaze_memo::tracked(returns(copy))]
fn inherent_assoc_types_of(db: &Db, adt: AdtId) -> ContentId {
    let rows = hir_ty::body_store::inherent_assoc_types_of(db, adt);
    let mut h = Hasher::new(INHERENT_ASSOC_TYPES_DOMAIN);
    h.update(&(rows.len() as u64).to_le_bytes());
    for (name, alias, impl_) in rows {
        h.part(name.as_str().as_bytes())
            .update(&[u8::from(name.key().is_some())])
            .id(&key::item_key(db, ItemId::Alias(*alias)).0)
            .id(&key::item_key(db, ItemId::Impl(*impl_)).0);
    }
    h.finish()
}

/// The traits in scope in `module`, by key, sorted.
#[blaze_memo::tracked(returns(copy))]
fn traits_in_scope_of(db: &Db, module: ModuleId) -> ContentId {
    let traits = module_trait_set(db, module).traits();
    // alloc: none for up to 32 traits in scope; a module past that spills once per change of
    // its set.
    let mut keys: SmallVec<[ContentId; 32]> =
        traits.iter().map(|&it| key::item_key(db, ItemId::Trait(it)).0).collect();
    keys.sort_unstable();
    let mut h = Hasher::new(TRAITS_DOMAIN);
    h.update(&(keys.len() as u64).to_le_bytes());
    for key in &keys {
        h.id(key);
    }
    h.finish()
}

/// Every name of `module`'s scope (its items and imports in each namespace, its textual
/// macros) with its `Resolves` value, sorted by the name's hash; `None` when one of them
/// cannot be hashed.
#[blaze_memo::tracked(returns(copy))]
fn whole_scope_of(db: &Db, module: ModuleId) -> Option<ContentId> {
    let scope = module.def_map(db).module(db, module).scope(db);
    let subject = key::module_key(db, module).0;
    let krate = module.krate(db);
    // alloc: one row per name of the scope, once per change of a scope something iterated
    let mut rows: Vec<(ContentId, ContentId)> = Vec::new();
    let named = scope.entries().filter(|(_, per_ns)| !per_ns.is_none()).map(|(name, _)| name);
    for name in named.chain(scope.legacy_macros().map(|(name, _)| name)) {
        let name = key::name_hash(name.as_str()).0;
        let fact = FactRef {
            kind: blaze_items::kinds::RESOLVES,
            subject,
            object: FactObject::Id(name),
        };
        rows.push((name, Goals.value(db, Some(krate), fact)?));
    }
    rows.sort_unstable();
    rows.dedup();
    let mut h = Hasher::new(WHOLE_SCOPE_DOMAIN);
    h.update(&(rows.len() as u64).to_le_bytes());
    for (name, value) in &rows {
        h.id(name).id(value);
    }
    Some(h.finish())
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

/// The fact of `step` of a body, as a later step of the same body reads it, the body named as
/// [`OWN`]; `None` for the borrow check, which no step reads, and a constant's value, which is
/// read as its `ConstValue` fact.
pub fn step_fact(step: Step) -> Option<FactRef> {
    let kind = match step {
        Step::Infer => kinds::STEP_INFER,
        Step::Mir => kinds::STEP_MIR,
        Step::Lower => kinds::STEP_LOWER,
        Step::Borrowck | Step::Const => return None,
    };
    Some(FactRef { kind, subject: OWN, object: FactObject::None })
}

/// The fact of `step` of another body `body`, by its key, as a step of some other body reads
/// it (a `const fn` evaluated, a closure's signature, a coroutine's witnesses); `None` where
/// [`step_fact`] has none. Its value is [`step_value`] of `body` ([`value`]'s arm for the
/// step kinds, through [`body_of`]): the hash of its answer, so two of these facts are equal
/// exactly when the answers are. A reader of `body` itself names it [`OWN`] ([`relative`]).
pub fn step_fact_of(db: &Db, body: DefWithBodyId, step: Step) -> Option<FactRef> {
    Some(FactRef { subject: body_key(db, body), ..step_fact(step)? })
}

/// The `MatchCheck` fact of a body, as its own borrow check reads it, the body named as
/// [`OWN`].
pub fn match_check() -> FactRef {
    FactRef { kind: kinds::MATCH_CHECK, subject: OWN, object: FactObject::None }
}

/// The key of `def`'s own item: what [`OWN`] stands for in a shape of `def`'s steps.
pub fn body_key(db: &Db, def: DefWithBodyId) -> ContentId {
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
    FactRef {
        kind: kinds::CRATE_ROOT,
        subject: key::crate_key(db, krate),
        object: FactObject::None,
    }
}

/// The `CrateSettings` fact of `krate`: what a read of a field of its `Crate` input stands
/// for, from a body of another crate.
pub fn crate_settings(db: &Db, krate: Crate) -> FactRef {
    FactRef {
        kind: kinds::CRATE_SETTINGS,
        subject: key::crate_key(db, krate),
        object: FactObject::None,
    }
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
#[blaze_memo::tracked(returns(copy))]
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
        ItemId::Trait(it) => member_names(db, &mut h, &it.trait_items(db).items),
        ItemId::Impl(it) => member_names(db, &mut h, &it.impl_items(db).items),
        ItemId::Function(_)
        | ItemId::Const(_)
        | ItemId::Static(_)
        | ItemId::Alias(_)
        | ItemId::Macro(_) => return None,
    }
    Some(h.finish())
}

/// A variant's field names in order, whether each has a default (`field: T = value`), which a
/// struct expression's `..` reads, and whether each is keyed by a `macro`'s context (which a
/// field's lookup compares). The key's context is this process's, so only that it has one is
/// hashed; which `macro` wrote it is of the signature's hash.
fn variant_names(db: &Db, h: &mut Hasher, variant: VariantId) {
    let fields = variant.fields(db).fields();
    h.update(&(fields.len() as u64).to_le_bytes());
    for (_, field) in fields.iter() {
        let flags = field.default_value.is_some() as u8 | (field.name.key().is_some() as u8) << 1;
        h.part(field.name.as_str().as_bytes()).update(&[flags]);
    }
}

/// An impl's or trait's items in order: the kind, the name and, as of a field, whether the
/// name is keyed (`hir_def::nameres::assoc_item_key`), which a method call's lookup compares.
fn member_names(db: &Db, h: &mut Hasher, items: &[(Name, AssocItemId)]) {
    h.update(&(items.len() as u64).to_le_bytes());
    for &(name, item) in items {
        let kind = match item {
            AssocItemId::FunctionId(_) => 0u8,
            AssocItemId::ConstId(_) => 1,
            AssocItemId::TypeAliasId(_) => 2,
        };
        let keyed = (hir_def::nameres::assoc_item_key(db, item).is_some() as u8) << 2;
        h.update(&[kind | keyed]).part(name.as_str().as_bytes());
    }
}

/// Everything a read of `owner`'s attributes in a step can give: the flags, the `#[unstable]`
/// feature, and a function's target features, sorted.
#[blaze_memo::tracked(returns(copy))]
fn attrs_of(db: &Db, owner: AttrDefId) -> ContentId {
    let flags = AttrFlags::query(db, owner);
    let mut h = Hasher::new(ATTRS_DOMAIN);
    h.update(&flags.bits().to_le_bytes());
    match flags.unstable_feature(db, owner) {
        Some(feature) => h.update(&[1]).part(feature.as_str().as_bytes()),
        None => h.update(&[0]),
    };
    // What a layout reads besides the flags: the `#[repr]` an ADT's flags say it has, and
    // what its `#[blaze::bits]` says of its backing, each as its value's `Debug` text, fed to
    // the hasher as it is written (both values are made of the attributes' tokens alone, so
    // the text is the same in every process).
    if let AttrDefId::AdtId(adt) = owner
        && flags.contains(AttrFlags::HAS_REPR)
    {
        h.update(b"repr");
        feed_debug(&mut h, &AttrFlags::repr_assume_has(db, adt));
    }
    if let Some(bits) = AttrFlags::blaze_bits(db, owner) {
        h.update(b"bits");
        feed_debug(&mut h, &bits);
    }
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
    // The flags of its generic parameters (`#[may_dangle]` on a `Drop` impl's, which the borrow
    // check reads): `generic_params_attr_flags`, whose reads are this fact.
    let generic = match owner {
        AttrDefId::AdtId(it) => Some(hir_def::GenericDefId::AdtId(it)),
        AttrDefId::FunctionId(it) => Some(hir_def::GenericDefId::FunctionId(it)),
        AttrDefId::TraitId(it) => Some(hir_def::GenericDefId::TraitId(it)),
        AttrDefId::TypeAliasId(it) => Some(hir_def::GenericDefId::TypeAliasId(it)),
        AttrDefId::ImplId(it) => Some(hir_def::GenericDefId::ImplId(it)),
        _ => None,
    };
    if let Some(def) = generic {
        let (lifetimes, types) = AttrFlags::query_generic_params(db, def);
        h.update(b"params").update(&(lifetimes.iter().count() as u64).to_le_bytes());
        for (param, flags) in lifetimes.iter() {
            h.update(&u32::from(param.into_raw()).to_le_bytes()).update(&flags.bits().to_le_bytes());
        }
        h.update(&(types.iter().count() as u64).to_le_bytes());
        for (param, flags) in types.iter() {
            h.update(&u32::from(param.into_raw()).to_le_bytes()).update(&flags.bits().to_le_bytes());
        }
    }
    h.finish()
}

/// Hashes `value`'s `Debug` text into `h` as it is written, with no buffer, then an end mark
/// no text holds (`0xff` is no UTF-8 byte).
fn feed_debug(h: &mut Hasher, value: &impl std::fmt::Debug) {
    struct Fed<'a>(&'a mut Hasher);
    impl std::fmt::Write for Fed<'_> {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            self.0.update(text.as_bytes());
            Ok(())
        }
    }
    std::fmt::Write::write_fmt(&mut Fed(&mut *h), format_args!("{value:?}"))
        .expect("a `Debug` into a hasher does not fail");
    h.update(&[0xff]);
}

/// A crate's lang items by item key, the crates it loads and depends on by crate key (itself
/// as a mark: its key is of all its files), its features, sorted, and its recursion limit.
#[blaze_memo::tracked(returns(copy))]
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

/// The digest of what `def`'s body lets escape: every item declared inside it (after
/// expansion, in any of its blocks and in any module written in one), by what a reader outside
/// the body can observe of it, never by what is inside a body of it. `Ok(None)` when the body
/// declares nothing that escapes.
///
/// The set, from Rust's semantics:
///
/// - Every impl, written or a built-in derive's: a trait impl joins coherence for the whole
///   crate graph, and an inherent impl's methods are found on its type from anywhere the type
///   is. Its row is its header and its items' signatures (`blaze_items::signature_hash` of the
///   impl and of each item), its items' names (`Fields`) and each item's attributes, of which a
///   method's `#[no_mangle]` and `#[export_name]` export a symbol. A built-in derive's impl is
///   its key (its type, trait and derive) over its type's signature, which is all it is made of.
/// - Every `#[macro_export]` `macro_rules!`: it is placed at the crate's root, and its
///   definition is its signature. A macro not exported is visible only inside the body, whose
///   uses of it are the body's own text, so it does not escape.
/// - Every function, static, const, ADT, trait and type alias: one with `#[no_mangle]`,
///   `#[export_name]`, `#[used]`, `#[link_section]`, `#[linkage]`, `#[panic_handler]` or a
///   lang item is seen by the linker and the runtime under its symbol; any of them can be
///   named outside through what does escape (an escaped impl's signature, an `impl Trait` of
///   the return type whose hidden type is a local type or a local function's item type, a
///   local trait an escaped impl implements), which names it by key and reads its signature.
///   Its row is its signature hash, its members' names, its attribute flags and the values of
///   its symbol attributes (export name, link section, linkage, link name, lang item); a
///   trait's also its items' signatures and attributes, as an impl's. Its body is not in it:
///   that body makes its own facts.
/// - Every `extern` block's `#[link]` libraries and `#[wasm_import_module]`: they join the
///   link of everything that links the crate. Its items are rows of their own, above.
///
/// Not in it: the body's `impl Trait` hidden types, and the closures and coroutines they name,
/// which are the body's `HiddenType` fact (`blaze_items::facts::hidden_types`, one concept with
/// one home), whose making needs the body's inference, which this does not; `use` and
/// `extern crate` items, which bind names inside the body alone; unnamed `const _` items,
/// which bind no name, and whose own bodies' items are their own bodies' `escaped`.
///
/// Rows are by item key, sorted, so the digest has no `Gid` and no position: an item's key is
/// its path through the body's blocks (`blaze_items::key`), which a statement edited, added or
/// removed around it does not move.
///
/// Cost: one pass over the scopes of the body's block def maps (which its lowering and
/// inference build in any case), and per declared item its signature hash, member names and
/// attributes, each a memoized query its own `Signature`, `Fields` and `Attrs` facts already
/// ask; a body with no item is the pass over its blocks alone (none for a body with no block
/// that declares an item). Floor: reading each declared item's observable form once and one
/// hash per item; this is at the floor.
pub fn escaped(db: &Db, def: DefWithBodyId) -> Result<Option<ContentId>, &'static str> {
    let body = Body::of(db, def);
    let mut rows: Vec<(ContentId, ContentId), _> = Vec::new_in(std::alloc::Global);
    for (_, block) in body.blocks(db) {
        for (_, module) in block.modules(db) {
            let scope = module.scope(db);
            for declared in scope.declarations() {
                let item = match declared {
                    ModuleDefId::FunctionId(it) => ItemId::Function(it),
                    ModuleDefId::AdtId(it) => ItemId::Adt(it),
                    ModuleDefId::ConstId(it) => ItemId::Const(it),
                    ModuleDefId::StaticId(it) => ItemId::Static(it),
                    ModuleDefId::TraitId(it) => ItemId::Trait(it),
                    ModuleDefId::TypeAliasId(it) => ItemId::Alias(it),
                    ModuleDefId::MacroId(it) if exported(db, it) => ItemId::Macro(it),
                    ModuleDefId::MacroId(_) => continue,
                    ModuleDefId::ModuleId(_)
                    | ModuleDefId::EnumVariantId(_)
                    | ModuleDefId::BuiltinType(_) => continue,
                };
                rows.push((key::item_key(db, item).0, escaped_item(db, item)?));
            }
            for (_, macros) in scope.legacy_macros() {
                for &it in macros {
                    if exported(db, it) {
                        let item = ItemId::Macro(it);
                        rows.push((key::item_key(db, item).0, escaped_item(db, item)?));
                    }
                }
            }
            for it in scope.impls() {
                let item = ItemId::Impl(it);
                rows.push((key::item_key(db, item).0, escaped_item(db, item)?));
            }
            for it in scope.builtin_derive_impls() {
                let adt = it.loc(db).adt;
                let signature =
                    blaze_items::signature_hash(db, ItemId::Adt(adt)).map_err(|_| ESCAPED_UNHASHED)?;
                let mut h = Hasher::new(ESCAPED_ITEM_DOMAIN);
                h.update(b"derive").id(&signature.0);
                rows.push((key::builtin_derive_key(db, it).0, h.finish()));
            }
            for it in scope.extern_blocks() {
                let libs = AttrFlags::native_libs(db, it);
                let wasm = AttrFlags::wasm_import_module(db, it);
                if libs.is_empty() && wasm.is_none() {
                    continue;
                }
                let mut h = Hasher::new(ESCAPED_ITEM_DOMAIN);
                h.update(b"link").update(&(libs.len() as u64).to_le_bytes());
                for lib in libs.iter() {
                    feed_debug(&mut h, lib);
                }
                h.opt_part(wasm.as_ref().map(|it| it.as_str()));
                // An extern block has no key of its own: its row is under its module's, the
                // block's module's key, with the block's content after it, so two blocks of one
                // module are two rows.
                rows.push((key::module_key(db, it.module(db)).0, h.finish()));
            }
        }
    }
    if rows.is_empty() {
        return Ok(None);
    }
    rows.sort_unstable();
    rows.dedup();
    let mut h = Hasher::new(ESCAPED_DOMAIN);
    h.update(&(rows.len() as u64).to_le_bytes());
    for (key, row) in &rows {
        h.id(key).id(row);
    }
    Ok(Some(h.finish()))
}

/// Whether `it` is a `#[macro_export]` macro: one placed at its crate's root.
fn exported(db: &Db, it: hir_def::MacroId) -> bool {
    AttrFlags::query(db, AttrDefId::MacroId(it)).contains(AttrFlags::IS_MACRO_EXPORT)
}

/// What a reader outside a body observes of `item` declared in it (see [`escaped`]): its
/// signature hash, its members' names, its attributes and its symbol attributes' values; an
/// impl's or trait's items' the same.
fn escaped_item(db: &Db, item: ItemId) -> Result<ContentId, &'static str> {
    let mut h = Hasher::new(ESCAPED_ITEM_DOMAIN);
    item_form(db, &mut h, item)?;
    h.opt_part(fields_of(db, item).as_ref().map(|it| &it.0));
    let members: Option<&[(Name, AssocItemId)]> = match item {
        ItemId::Impl(it) => Some(&it.impl_items(db).items),
        ItemId::Trait(it) => Some(&it.trait_items(db).items),
        ItemId::Function(_)
        | ItemId::Adt(_)
        | ItemId::Const(_)
        | ItemId::Static(_)
        | ItemId::Alias(_)
        | ItemId::Macro(_) => None,
    };
    if let Some(members) = members {
        h.update(&(members.len() as u64).to_le_bytes());
        for &(_, member) in members.iter() {
            let member = match member {
                AssocItemId::FunctionId(it) => ItemId::Function(it),
                AssocItemId::ConstId(it) => ItemId::Const(it),
                AssocItemId::TypeAliasId(it) => ItemId::Alias(it),
            };
            item_form(db, &mut h, member)?;
        }
    }
    Ok(h.finish())
}

/// `item`'s signature hash, attributes and symbol attributes' values, into `h`.
fn item_form(db: &Db, h: &mut Hasher, item: ItemId) -> Result<(), &'static str> {
    let signature = blaze_items::signature_hash(db, item).map_err(|_| ESCAPED_UNHASHED)?;
    let owner = match item {
        ItemId::Function(it) => AttrDefId::FunctionId(it),
        ItemId::Adt(it) => AttrDefId::AdtId(it),
        ItemId::Trait(it) => AttrDefId::TraitId(it),
        ItemId::Impl(it) => AttrDefId::ImplId(it),
        ItemId::Const(it) => AttrDefId::ConstId(it),
        ItemId::Static(it) => AttrDefId::StaticId(it),
        ItemId::Alias(it) => AttrDefId::TypeAliasId(it),
        ItemId::Macro(it) => AttrDefId::MacroId(it),
    };
    h.id(&signature.0).id(&attrs_of(db, owner));
    for value in [
        AttrFlags::export_name(db, owner),
        AttrFlags::link_section(db, owner),
        AttrFlags::linkage(db, owner),
        AttrFlags::link_name(db, owner),
        AttrFlags::lang_item(db, owner),
    ] {
        h.opt_part(value.as_ref().map(|it| it.as_str()));
    }
    Ok(())
}
