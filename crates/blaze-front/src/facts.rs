//! The one mapping from what the front end reads, a fact reference of blaze-item-hash and lane
//! items (`hir_ty::next_solver::goal_store::FactRef`), to a fact id of
//! `blaze_keyed::record` ([`id_of`]), and the value a surface exports for one ([`value_of`]).
//! The surface task ([`crate::surface`]) writes its rows through these functions and the body
//! task ([`crate::body`]) asks through the same ones, so the two sides name one fact by one id
//! by construction, and `blaze_keyed::front::Universe::answer` finds its row by the id alone.
//!
//! **The id.** Every id is [`record::fact`]`(kind, owner, code, subject, what)`, the one
//! constructor (crates/blaze-keyed/src/record.rs, "The fact id"): bytes `0..8` the owner, `8..10`
//! the query code (`QueryKind.0`, or one of this crate's own codes: [`EXPORTS`], [`ESCAPED`],
//! [`EFFECTS`]), byte `10` the [`FactKind`], the rest a hash. The owner is:
//!
//! - for a fact about an item, module or crate ([`Class::Item`]): the crate that owns the
//!   subject, its key's first eight bytes ([`Owner::of_subject`]; an item key, a module key and
//!   a crate key all begin with their crate key's, `blaze_items::key`). Whether the fact is a
//!   row of that crate's surface or a fact one of its bodies makes is the kind's
//!   ([`FactKind::made_by_body`]): a const's value and an opaque type's hidden type route to
//!   the owner's made table, so the same id names the fact whichever body of whichever unit
//!   asks it, and the body that makes it ([`crate::body::exports`]) writes the same id.
//! - for what the asking unit sees ([`Class::Asker`]): [`Owner::ASKER`], answered from the
//!   asking unit's own surface (its row, else its seen row, `record::seen_id`).
//!
//! An outside read (`env!`, `include!`) is [`Owner::OUTSIDE`] and never reaches this mapping:
//! it is in the unit's key and answered by the host (`record::late_id`).
//!
//! The subject is the subject's key itself (an item key, a module key, a crate key:
//! blaze-items' path-and-kind hashes with the crate's identity, never a position, a dense id or
//! a `Gid`), and `what` is the object: nothing, a field's position, or an id (a name's hash, a
//! simplified self type).
//!
//! **The classes.** [`Class::Item`]: one row per fact that exists in the owner's surface (or in
//! its made table); a fact that does not exist has no row, and every side answers it by
//! `record::absent(&id)`. [`Class::Asker`]: [`Asked::Seen`], a listing across the crates the
//! asker sees (`Implements`, `Method`, `Vtable`, `InherentImpls`, `TraitImpls`, `TraitsNamed`,
//! `ScopeTraitsNamed`, `BlockImpls`), with no row: answered by the asker's seen row, which
//! digests the asker's exports and those of every surface it read (coarse; the end state is a
//! row per (trait, self head) and holder: see [`crate::surface`]); [`Asked::Build`], one fact
//! of the whole build (`FieldOrder`, `Setting`), a row of every surface. [`Class::Refused`]:
//! what no row stands for, refused by name: a [`FactError`] names the rule, the query kind and
//! the subject asked, and [`FactError::failed`] carries all three into the task's failure.
//! Refused today: `Captures`, `AutoTraits`, `Derive` and `Crate` (each constant says what it
//! would take). A crate's `CrateSettings` and `Modules` are item rows of the crate's own
//! surface. A block's scope is no fact of its own: a body that reads its own block lists the
//! block's lookups as ordinary rows (`Resolves`, `TraitsInScope`, `WholeScope`, `Modules`,
//! `CrateNames`); kind 0x180e (`BlockScope`) is retired.
//!
//! **Floor (rule 67).** An id is one hash of the subject and object, no query: at the floor.
//! A value is the fact's value (lane items' hash, memoized per fact) plus, for a `Resolves`
//! fact, one more lookup of the same scope ([`has_row`]): at the floor of one value per row.

#![blaze::sans_io]

use base_db::{Crate, Db};
use blaze_ids::ContentId;
use blaze_item_hash::values;
use blaze_items::{ItemId, ItemKey, Resolved, key};
use blaze_keyed::{
    driver::TaskFailed,
    record::{self, FactKind, Owner, StableId},
};
use core::fmt;
use hir_def::{AdtId, DefWithBodyId, VariantId};
use hir_ty::next_solver::goal_store::{FactObject, FactRef};

/// The code of a surface's exports row: the digest of every other row of the surface. What a
/// surface's shape holds of each surface it read.
pub const EXPORTS: u16 = 0xf001;
/// The code of a body's escaped-items fact ([`FactKind::Escaped`]): made by the body.
pub const ESCAPED: u16 = 0xf003;
/// The code of a body's own effect summary ([`FactKind::Effects`]): made by the body.
pub const EFFECTS: u16 = 0xf004;
/// The code of a surface's maker row ([`FactKind::Maker`], [`maker_id`]): which body makes the
/// facts about a subject. Read by the driver, never by a task.
pub const MAKER: u16 = record::MAKER_CODE;
/// The code of the fact "the body of this item" ([`FactKind::Mir`], `record::mir_id`): what a
/// plan reads of a body, its digest the body record's result digest. No front-end query makes
/// it and no surface or made table holds it: the driver answers it from the bodies it settled,
/// so no rule here maps it and no task's read asks it through this mapping.
pub const MIR: u16 = record::MIR_CODE;
/// The code of the fact "a file's printed name and image key" ([`FactKind::SpanFile`],
/// owned by the unit): what a span table reads of a file. No front-end query makes it: the
/// driver answers it from its units' key rows, so no rule here maps it.
pub const SPAN_FILE: u16 = record::SPAN_FILE_CODE;

/// The id of the maker row of the body-bearing definition whose item key is `subject`: a row
/// of its crate's surface, named by the subject as every made fact about it is
/// (`record::maker_row`), so the driver forms it from a made fact's id.
pub fn maker_id(subject: &ContentId) -> ContentId {
    record::maker_row(&StableId(*subject))
}

/// How a fact is owned and answered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    /// About one item, module or crate: a row of its owner's surface, or of its owner's made
    /// table for a kind a body makes, when it exists.
    Item,
    /// What the asking unit sees: answered from the asking unit's own surface.
    Asker(Asked),
    /// What no row stands for, named.
    Refused(&'static str),
}

/// What an asker fact's answer is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Asked {
    /// A listing across the crates the asker sees: no row; the asker's seen row answers it.
    Seen,
    /// One fact of the whole build: a row of every surface, its value the fact's.
    Build,
}

/// A read of a whole crate (`blaze_items::Fact::Crate`): a dependency's batch. No row stands
/// for all of a crate, and none ever will (rule granular-caching): the reader is to ask the
/// per-item facts it uses.
pub const WHOLE_CRATE: &str = "a read of all of a crate (blaze-items `Crate`): no fact stands for a whole crate";
/// A closure's inferred signature and captures, of a closure of another body. What it takes:
/// a `Captures` fact made by the enclosing body (a [`FactKind`] that is
/// [`FactKind::made_by_body`], written by `crate::body::exports` per closure key).
pub const CAPTURES: &str = "a read of another body's closure captures (blaze-items `Captures`): no fact a body makes holds them yet";
/// An opaque type's auto traits. What it takes: the defining body making them as a fact of
/// their own, apart from the hidden type, so a reader of `Send` alone is not moved by the
/// hidden type's other changes.
pub const AUTO_TRAITS: &str =
    "a read of an opaque type's auto traits (blaze-items `AutoTraits`): no fact a body makes holds them apart from its hidden type yet";
/// A derive's output: not a row of any surface. What it takes: the derive's expansion record
/// (blaze-expansion's macro-call record) named by a fact of the expanding item's surface.
pub const DERIVE: &str = "a read of a derive's output (blaze-items `Derive`): not a row of any surface yet";
/// An item's hashes (blaze-items `Hashes`): no surface writes the row (`exports::collect`) and
/// no body makes it, so answered from the tables it would be `absent` forever, and a reader
/// keyed by it never moves when the item does. Its interface hash folds in what bodies leak
/// (a const's value, a `const fn`'s and a static's code, hidden types), transitively over every
/// item it names, which no one row stands for: a reader of `blaze_items::hash::interface_of`
/// lists the per-item rows of what the hash read instead (`blaze_item_hash::reads`, which
/// marks no boundary on it), each leak behind its own fact.
pub const HASHES: &str = "a read of an item's hashes (blaze-items `Hashes`): no surface row or made fact holds them; list what they read instead";
/// A fact a body makes, asked with an object (a field, an id): a made fact is one per item,
/// and its id names its maker by the item alone (`record::maker_tag`), so a driver can find the
/// body that makes it when that body's unit is not checked. What it would take: a made kind
/// with an object would need its maker in the id some other way.
pub const MADE_OBJECT: &str = "a fact a body makes, asked with an object: a made fact's id names its maker by its item alone";
/// A fact that names a body's own item as `OWN`: made absolute before it is mapped.
pub const RELATIVE: &str = "a fact naming `OWN`: a fact is mapped absolute (`values::relative` undone first)";
/// A kind this mapping has no rule for.
pub const UNKNOWN: &str = "a fact of a query kind no rule maps to a fact kind";
/// A fact whose value cannot be hashed now, or names what is not its kind's.
pub const UNHASHED: &str = "a fact whose value cannot be hashed now (`values::value` gave none)";
/// A listing's value asked of a surface: it has no row (the seen row answers it).
pub const LISTING: &str = "the value of a listing across crates (`facts::Asked::Seen`): no row holds it, the asker's seen row answers it";
/// A made fact's value asked of a surface: the body that makes it writes it.
pub const MADE: &str = "a surface row of a fact a body makes (`FactKind::made_by_body`): its body's record holds it, never a surface";

/// Why a fact has no id or no value: what was refused (the rule), the query kind it was of and
/// the subject asked (an item, module or crate key), so a refusal names the item it is about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FactError {
    pub what: &'static str,
    pub kind: u16,
    pub subject: ContentId,
}

impl FactError {
    /// The refusal as a task's failure: the rule, the kind and the subject, by name.
    pub fn failed(&self) -> TaskFailed {
        TaskFailed::RefusedFact { what: self.what, kind: self.kind, subject: self.subject }
    }
}

impl fmt::Display for FactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (query kind {:#06x}, {}, of {:?})", self.what, self.kind, name_of(self.kind), self.subject)
    }
}

/// One rule: a query kind, the fact kind its id is stamped with, its class and its name.
struct Rule {
    code: u16,
    kind: FactKind,
    class: Class,
    name: &'static str,
}

const fn item(code: u16, kind: FactKind, name: &'static str) -> Rule {
    Rule { code, kind, class: Class::Item, name }
}

const fn asker(code: u16, kind: FactKind, asked: Asked, name: &'static str) -> Rule {
    Rule { code, kind, class: Class::Asker(asked), name }
}

const fn refused(code: u16, kind: FactKind, why: &'static str, name: &'static str) -> Rule {
    Rule { code, kind, class: Class::Refused(why), name }
}

use blaze_item_hash::values::kinds as own;
use blaze_items::kinds as items;

/// Every query kind the front end's facts are of (lane items' `0x1000..0x101a`, blaze-item-hash's
/// `values::kinds::ALL`), once. [`tests`] holds it to both crates' lists.
const RULES: &[Rule] = &[
    item(items::SIGNATURE.0, FactKind::Signature, "Signature"),
    item(items::LAYOUT.0, FactKind::Layout, "Layout"),
    item(items::FIELD.0, FactKind::Fields, "Field"),
    asker(items::METHOD.0, FactKind::Method, Asked::Seen, "Method"),
    asker(items::IMPLEMENTS.0, FactKind::ImplRows, Asked::Seen, "Implements"),
    item(items::RESOLVES.0, FactKind::Binding, "Resolves"),
    item(items::CODE.0, FactKind::InlineCode, "Code"),
    item(items::CONST_VALUE.0, FactKind::ConstValue, "ConstValue"),
    item(items::NAME.0, FactKind::Name, "Name"),
    refused(items::CAPTURES.0, FactKind::StepInfer, CAPTURES, "Captures"),
    refused(items::AUTO_TRAITS.0, FactKind::StepInfer, AUTO_TRAITS, "AutoTraits"),
    item(items::COHERENCE.0, FactKind::Coherence, "Coherence"),
    item(items::SPECIALIZES.0, FactKind::Specializes, "Specializes"),
    item(items::DROP_CHECK.0, FactKind::DropCheck, "DropCheck"),
    asker(items::VTABLE.0, FactKind::Vtable, Asked::Seen, "Vtable"),
    refused(items::DERIVE.0, FactKind::StepInfer, DERIVE, "Derive"),
    refused(items::HASHES.0, FactKind::Hashes, HASHES, "Hashes"),
    item(items::TARGET.0, FactKind::Target, "Target"),
    refused(items::CRATE.0, FactKind::StepInfer, WHOLE_CRATE, "Crate"),
    item(items::HIDDEN_TYPES.0, FactKind::HiddenType, "HiddenTypes"),
    item(items::RPITIT_TYS.0, FactKind::RpititTys, "RpititTys"),
    asker(items::INHERENT_IMPLS.0, FactKind::InherentImpls, Asked::Seen, "InherentImpls"),
    asker(items::TRAIT_IMPLS.0, FactKind::TraitImpls, Asked::Seen, "TraitImpls"),
    asker(items::TRAITS_NAMED.0, FactKind::TraitsNamed, Asked::Seen, "TraitsNamed"),
    asker(items::SCOPE_TRAITS_NAMED.0, FactKind::ScopeTraitsNamed, Asked::Seen, "ScopeTraitsNamed"),
    asker(items::BLOCK_IMPLS.0, FactKind::BlockImpls, Asked::Seen, "BlockImpls"),
    item(own::FIELDS.0, FactKind::Fields, "Fields"),
    item(own::ATTRS.0, FactKind::Attrs, "Attrs"),
    item(own::CRATE_ROOT.0, FactKind::CrateRoot, "CrateRoot"),
    item(own::STEP_INFER.0, FactKind::StepInfer, "StepInfer"),
    item(own::STEP_MIR.0, FactKind::StepMir, "StepMir"),
    item(own::MATCH_CHECK.0, FactKind::MatchCheck, "MatchCheck"),
    item(own::TRAITS_IN_SCOPE.0, FactKind::TraitsInScope, "TraitsInScope"),
    item(own::WHOLE_SCOPE.0, FactKind::WholeScope, "WholeScope"),
    item(own::CRATE_NAMES.0, FactKind::CrateNames, "CrateNames"),
    asker(own::FIELD_ORDER.0, FactKind::FieldOrder, Asked::Build, "FieldOrder"),
    asker(own::SETTING.0, FactKind::Setting, Asked::Build, "Setting"),
    item(own::INHERENT_ASSOC_TYPES.0, FactKind::InherentAssocTypes, "InherentAssocTypes"),
    item(own::STEP_LOWER.0, FactKind::StepLower, "StepLower"),
    // A crate's settings and its module walk are rows of its own surface: its surface key
    // covers its settings and its module tree, and no body's text moves either.
    item(own::CRATE_SETTINGS.0, FactKind::CrateSettings, "CrateSettings"),
    item(own::MODULES.0, FactKind::Modules, "Modules"),
];

fn rule(code: u16) -> Option<&'static Rule> {
    RULES.iter().find(|rule| rule.code == code)
}

/// The name of query kind `code`, for messages.
pub fn name_of(code: u16) -> &'static str {
    match code {
        EXPORTS => "a surface's exports",
        ESCAPED => "a body's escaped items",
        EFFECTS => "a body's own effects",
        MAKER => "the body that makes a subject's facts",
        MIR => "the body of an item, as a plan reads it",
        SPAN_FILE => "a file's printed name and image key, as a span table reads it",
        _ => rule(code).map_or("no kind this mapping knows", |rule| rule.name),
    }
}

/// How a fact of query kind `code` is owned and answered; `None` for a kind no rule maps.
pub fn class_of(code: u16) -> Option<Class> {
    rule(code).map(|rule| rule.class)
}

/// The fact kind a fact of query kind `code` is stamped with; `None` for a kind no rule maps.
pub fn kind_of(code: u16) -> Option<FactKind> {
    rule(code).map(|rule| rule.kind)
}

/// The bytes of a fact's object as its id's `what`: nothing, a field's position, an id.
fn what_of(object: &FactObject, out: &mut [u8; 32]) -> usize {
    match *object {
        FactObject::None => 0,
        FactObject::Field(field) => {
            out[..4].copy_from_slice(&field.to_le_bytes());
            4
        }
        FactObject::Id(it) => {
            out.copy_from_slice(&it.0);
            32
        }
    }
}

/// The id of `fact`, a fact reference made absolute (no [`values::OWN`] subject), by
/// [`record::fact`] with its owner: the subject's crate for an item fact (its surface's row,
/// or its made table's for a kind a body makes), [`Owner::ASKER`] for what the asker sees.
pub fn id_of(fact: &FactRef) -> Result<ContentId, FactError> {
    let code = fact.kind.0;
    let refuse = |what| FactError { what, kind: code, subject: fact.subject };
    if fact.subject == values::OWN {
        return Err(refuse(RELATIVE));
    }
    let rule = rule(code).ok_or(refuse(UNKNOWN))?;
    let owner = match rule.class {
        Class::Item => Owner::of_subject(&fact.subject),
        Class::Asker(_) => Owner::ASKER,
        Class::Refused(why) => return Err(refuse(why)),
    };
    let mut what = [0; 32];
    let len = what_of(&fact.object, &mut what);
    // A made fact's id names its maker by its subject (`record::maker_tag`), which an object
    // would hash away: a driver could then not find the body that makes it.
    if rule.kind.made_by_body() && len > 0 {
        return Err(refuse(MADE_OBJECT));
    }
    Ok(record::fact(rule.kind, owner, code, &StableId(fact.subject), &what[..len]))
}

/// The id of a surface's exports row ([`EXPORTS`]), of the crate of key `crate_key`: a row of
/// that crate's surface.
pub fn exports_id(crate_key: &ContentId) -> ContentId {
    record::fact(FactKind::Exports, Owner::of_subject(crate_key), EXPORTS, &StableId(*crate_key), b"")
}

/// A name no identifier is: its hash finds nothing in any scope, so the value of its
/// `Resolves` fact in a scope is that scope's "nothing" ([`has_row`]).
const NO_NAME: &str = "\0blaze-front: a name no identifier is";

/// Whether a surface holds a row of `fact`, an absolute [`Class::Item`] fact: its subject is
/// there now (an item, module or crate a key of this build resolves to), a field by position
/// is one of its type's, and a name looked up in a scope finds something. The surface writes a
/// row of exactly these ([`crate::surface`]); every other fact of an item has no row, and every
/// side answers it by `record::absent`.
pub fn has_row(db: &Db, krate: Crate, fact: &FactRef) -> Result<bool, FactError> {
    let code = fact.kind.0;
    let refuse = |what| FactError { what, kind: code, subject: fact.subject };
    // The facts whose subject is a crate key, not an item's or a module's.
    if code == own::CRATE_ROOT.0 || code == items::TARGET.0 || code == own::CRATE_SETTINGS.0 {
        return Ok(key::resolve_crate(db, &fact.subject).is_some());
    }
    let Some(resolved) = key::resolve(db, ItemKey(fact.subject)) else { return Ok(false) };
    if code == items::FIELD.0 {
        let FactObject::Field(field) = fact.object else {
            return Err(refuse(UNHASHED));
        };
        let variant = match resolved {
            Resolved::Item(ItemId::Adt(AdtId::StructId(it))) => VariantId::from(it),
            Resolved::Item(ItemId::Adt(AdtId::UnionId(it))) => VariantId::from(it),
            Resolved::Variant(it) => VariantId::from(it),
            _ => return Ok(false),
        };
        let fields = variant.fields(db).fields();
        return Ok(fields.iter().any(|(it, _)| u32::from(it.into_raw()) == field));
    }
    if code == items::RESOLVES.0 {
        let value = values::value(db, krate, *fact).ok_or(refuse(UNHASHED))?;
        let none = FactRef { object: FactObject::Id(key::name_hash(NO_NAME).0), ..*fact };
        let nothing = values::value(db, krate, none).ok_or(refuse(UNHASHED))?;
        return Ok(value != nothing);
    }
    Ok(true)
}

/// The value a surface row of `fact` holds, exported by crate `krate`'s surface: the fact's
/// value (`values::value`) for an item fact with a row ([`has_row`]) or a build fact. A fact a
/// body makes, a listing and a refused kind have no surface row and are refused by name: a
/// surface that wrote one would hold a row no answer reads (rule: one concept, one home).
pub fn value_of(db: &Db, krate: Crate, fact: &FactRef) -> Result<ContentId, FactError> {
    let code = fact.kind.0;
    let refuse = |what| FactError { what, kind: code, subject: fact.subject };
    if fact.subject == values::OWN {
        return Err(refuse(RELATIVE));
    }
    let rule = rule(code).ok_or(refuse(UNKNOWN))?;
    if rule.kind.made_by_body() {
        return Err(refuse(MADE));
    }
    match rule.class {
        Class::Refused(why) => Err(refuse(why)),
        Class::Asker(Asked::Seen) => Err(refuse(LISTING)),
        Class::Asker(Asked::Build) | Class::Item => values::value(db, krate, *fact).ok_or(refuse(UNHASHED)),
    }
}

/// The line a surface task tells where a row of `fact` has no value ([`UNHASHED`]), rule 19:
/// the fact's kind by name ([`name_of`]); what its subject is in this snapshot (an item by kind
/// and path with the file and line it is written at, a module by its crate and path, a crate by
/// name, else its key); and why it has no value (`values::value_or_why`): an item's hash refused,
/// said by the refusal itself (`blaze_items::Unhashable::describe`, the item and what could not
/// be hashed), or the rule.
#[blaze::renders_positions]
pub fn unhashed_line<W: core::fmt::Write + ?Sized>(db: &Db, krate: Crate, fact: &FactRef, said: &mut W) {
    use hir_def::{Lookup as _, MacroId};
    _ = write!(said, "{} (query kind {:#06x}) of ", name_of(fact.kind.0), fact.kind.0);
    let site = |ast| hir_def::deps::ast_site(db, ast);
    match key::resolve(db, ItemKey(fact.subject)) {
        Some(Resolved::Item(item)) => {
            let at = match item {
                ItemId::Function(it) => site(it.lookup(db).id.erase()),
                ItemId::Adt(AdtId::StructId(it)) => site(it.lookup(db).id.erase()),
                ItemId::Adt(AdtId::UnionId(it)) => site(it.lookup(db).id.erase()),
                ItemId::Adt(AdtId::EnumId(it)) => site(it.lookup(db).id.erase()),
                ItemId::Trait(it) => site(it.lookup(db).id.erase()),
                ItemId::Impl(it) => site(it.lookup(db).id.erase()),
                ItemId::Const(it) => site(it.lookup(db).id.erase()),
                ItemId::Static(it) => site(it.lookup(db).id.erase()),
                ItemId::Alias(it) => site(it.lookup(db).id.erase()),
                ItemId::Macro(MacroId::Macro2Id(it)) => site(it.lookup(db).id.erase()),
                ItemId::Macro(MacroId::MacroRulesId(it)) => site(it.lookup(db).id.erase()),
                ItemId::Macro(MacroId::ProcMacroId(it)) => site(it.lookup(db).id.erase()),
            };
            _ = write!(said, "{:?} `{}` at {at}", item.kind(), key::path(db, item));
        }
        Some(Resolved::Variant(variant)) => {
            _ = write!(said, "a variant at {}", site(variant.lookup(db).id.erase()));
        }
        Some(Resolved::Module(module)) => {
            let path = hir_def::deps::module_path(db, module);
            _ = write!(said, "module `{path}` of `{}`", module.krate(db).named(db));
        }
        Some(other) => _ = write!(said, "{other:?}"),
        None => match key::resolve_crate(db, &fact.subject) {
            Some(it) => _ = write!(said, "crate `{}`", it.named(db)),
            None => _ = write!(said, "the subject {:?} (not in this snapshot)", fact.subject),
        },
    }
    match values::value_or_why(db, krate, *fact) {
        Err(values::NoValue::Unhashable(refused)) => _ = write!(said, ": {}", refused.describe(db)),
        Err(values::NoValue::Why(why)) => _ = write!(said, ": {why}"),
        // Asked again, it has one: the first ask was refused and this one was not, which is a
        // bug of its own (a value is a function of the snapshot); said so, never hidden.
        Ok(_) => _ = said.write_str(": it has a value when asked again, in the same snapshot"),
    }
}

/// The body the item key `subject` names in this build: a function, const, static, or a
/// variant (its discriminant).
pub fn body_of(db: &Db, subject: &ContentId) -> Option<DefWithBodyId> {
    match key::resolve(db, ItemKey(*subject))? {
        Resolved::Item(ItemId::Function(it)) => Some(DefWithBodyId::FunctionId(it)),
        Resolved::Item(ItemId::Const(it)) => Some(DefWithBodyId::ConstId(it)),
        Resolved::Item(ItemId::Static(it)) => Some(DefWithBodyId::StaticId(it)),
        Resolved::Variant(it) => Some(DefWithBodyId::VariantId(it)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::exports::Export;

    /// Every query kind of lane items and of blaze-item-hash has exactly one rule: a kind added
    /// to either crate fails here until it is mapped.
    #[test]
    fn every_kind_has_one_rule() {
        let lane: u16 = 0x101a - 0x1000;
        // blaze-item-hash's own list of its kinds, read from it, so a kind it adds fails here
        // unmapped (its `RANGE` is a reservation, wider than the kinds it gives).
        let expected = (0..lane).map(|it| 0x1000 + it).chain(own::ALL.iter().map(|kind| kind.0));
        for code in expected {
            assert_eq!(RULES.iter().filter(|rule| rule.code == code).count(), 1, "{code:#06x}");
        }
        assert_eq!(RULES.len(), usize::from(lane) + own::ALL.len());
        // Every fact kind byte is its own kind back; the retired byte (42, once `BlockScope`)
        // and the bytes past the last are none.
        for byte in (1..=46u8).filter(|&byte| byte != 42) {
            assert_eq!(FactKind::of_byte(byte).map(|kind| kind as u8), Some(byte), "byte {byte}");
        }
        assert_eq!(FactKind::of_byte(42), None);
        assert_eq!(FactKind::of_byte(47), None);
        // The kinds no front-end query makes (the maker row, the body of an item, a span
        // table's file) have a code of their own and a name, and no rule: the driver answers
        // them, never this mapping.
        for code in [MAKER, MIR, SPAN_FILE] {
            assert!(rule(code).is_none(), "{code:#06x}");
            assert_ne!(name_of(code), "no kind this mapping knows", "{code:#06x}");
        }
        // 0x180e (`BlockScope`) is retired in blaze-item-hash: no rule maps it.
        assert!(rule(0x180e).is_none());
    }

    /// The new kinds route where their rule says: a crate's settings and its modules to the
    /// crate's surface; the refused kinds name their rule, kind and the subject asked.
    /// Control: the refusal's subject is the one asked, so a refusal that named no item (or
    /// another) is caught.
    #[test]
    fn the_crate_kinds_route_to_their_surface() {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        bytes[8..].copy_from_slice(&[7; 24]);
        let subject = ContentId(bytes);
        let owner = Owner::of_subject(&subject);
        for code in [own::CRATE_SETTINGS, own::MODULES] {
            let id = id_of(&FactRef { kind: code, subject, object: FactObject::None }).expect("an id");
            assert_eq!(record::route(&id), Ok(record::Route::Surface(owner)), "{code:?}");
        }
        let refused = FactError { what: DERIVE, kind: items::DERIVE.0, subject };
        assert_eq!(refused.failed(), TaskFailed::RefusedFact { what: DERIVE, kind: items::DERIVE.0, subject });
        for (code, why) in [(items::CAPTURES, CAPTURES), (items::AUTO_TRAITS, AUTO_TRAITS), (items::DERIVE, DERIVE), (items::CRATE, WHOLE_CRATE)] {
            let error = id_of(&FactRef { kind: code, subject, object: FactObject::None }).expect_err("refused");
            assert_eq!((error.what, error.kind, error.subject), (why, code.0, subject), "{code:?}");
        }
    }

    /// The control of [`every_kind_has_one_rule`]: a table missing one kind is caught.
    #[test]
    fn a_missing_rule_is_caught() {
        let without: Vec<u16> = RULES.iter().skip(1).map(|rule| rule.code).collect();
        assert!(!without.contains(&items::SIGNATURE.0));
    }

    /// The invariant: a reader's id of a fact a body makes (a const's value, an opaque type's
    /// hidden type) is the id the maker exports, and routes to the owner's made table; an item
    /// fact a surface holds routes to the owner's surface; an asker fact to the asker.
    #[test]
    fn a_readers_id_is_the_makers() {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        bytes[8..].copy_from_slice(&[9; 24]);
        let subject = ContentId(bytes);
        let owner = Owner::of_subject(&subject);
        for (code, export) in [(items::CONST_VALUE, Export::ConstValue), (items::HIDDEN_TYPES, Export::HiddenType)] {
            let read = id_of(&FactRef { kind: code, subject, object: FactObject::None }).expect("an id");
            assert_eq!(read, export.id(&subject), "{code:?}");
            assert_eq!(record::route(&read), Ok(record::Route::Made(owner)), "{code:?}");
            // The driver forms the subject's maker row from the made fact's id alone: the
            // surface's row ([`maker_id`]), a row of the owner's surface.
            assert_eq!(record::maker_row_of(&read), Some(maker_id(&subject)), "{code:?}");
        }
        assert_eq!(record::route(&maker_id(&subject)), Ok(record::Route::Surface(owner)));
        // Control: another subject of the crate, another maker row.
        let mut other = bytes;
        other[20] ^= 1;
        assert_ne!(maker_id(&ContentId(other)), maker_id(&subject));
        let signature = id_of(&FactRef { kind: items::SIGNATURE, subject, object: FactObject::None }).expect("an id");
        assert_eq!(record::route(&signature), Ok(record::Route::Surface(owner)));
        let listing = id_of(&FactRef { kind: items::IMPLEMENTS, subject, object: FactObject::None }).expect("an id");
        assert_eq!(record::route(&listing), Ok(record::Route::Asker));
        // Control: two objects, two ids.
        let a = id_of(&FactRef { kind: items::FIELD, subject, object: FactObject::Field(0) }).expect("an id");
        let b = id_of(&FactRef { kind: items::FIELD, subject, object: FactObject::Field(1) }).expect("an id");
        assert_ne!(a, b);
    }
}
