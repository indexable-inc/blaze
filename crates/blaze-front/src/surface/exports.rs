//! The rows of a crate's surface: one per fact of an item, module or the crate that exists now
//! ([`facts::has_row`]), each by [`facts::id_of`] (the one id, `record::fact` with its owner,
//! the id a body asks the same fact by) and [`facts::value_of`].
//!
//! What is walked, once: the crate (`CrateRoot`, `Target`, `CrateNames`, `CrateSettings`,
//! `Modules`, and the build facts `FieldOrder` and `Setting`, owned by `Owner::ASKER`), every
//! module of its def map
//! (`TraitsInScope`, `WholeScope`, `Attrs`, a `Resolves` per name of its scope, and its
//! declarations, impls and unnamed consts), every item reached so (`Signature`, `Attrs`,
//! `Name`, and by kind: an ADT's `Layout`, `DropCheck`, `InherentAssocTypes`, `Fields`, a
//! `Field` and a `Resolves` per field, an enum's variants with theirs; a trait's or impl's
//! `Fields`, a `Resolves` per associated item and the items' own facts; an impl's `Coherence`
//! and `Specializes`; a method's `RpititTys`), and the impls of the crate's bodies
//! (`TraitImpls` and `InherentImpls` list them).
//!
//! What is no row, by rule: a fact a body makes (`ConstValue`, `HiddenTypes`: its body's
//! record holds it, `crate::body::exports`); a listing across crates (`Method`, `Implements`,
//! `Vtable`, `InherentImpls`, `TraitImpls`, `TraitsNamed`, `ScopeTraitsNamed`, `BlockImpls`),
//! answered by the asker's seen row; and `Captures`, `AutoTraits`, `Derive`, `Crate`, which a
//! read refuses. A body's steps are no row either: no body's result is a surface's. A block's
//! scope is no fact at all: it follows the text of the body the block is in, so the body that
//! reads its own block lists the block's lookups as ordinary rows of its own record (kind
//! 0x180e, once `BlockScope`, is retired).
//!
//! Floor: one value per row, each computed once (the crate's own scopes once); the two crate
//! rows are one hash each (`CrateSettings` of the crate's content id, `Modules` one pass over
//! the module table the walk below makes anyway). Costs past it:
//! an item reached from two places (a module's declarations and the impl lists) is looked at
//! twice and its row deduplicated by the caller's sort.
//!
//! The maker rows ([`facts::maker_id`]): one per body owner of the crate
//! (`hir_def::nameres::demand::crate_body_owners`) the command names a text id for
//! ([`TextIds`]), its id of the body's item key (`values::body_key`, the subject every made
//! fact about it names), its digest the text id. Cost: one row per such body on disk, one pass
//! over the body owners (the def map's, which lowers no body) and one `text_id` each.

#![blaze::sans_io]

use std::alloc::{Allocator, Global};

use base_db::{Crate, Db};
use blaze_ids::ContentId;
use blaze_item_hash::values;
use blaze_items::{ItemId, key, kinds as items};
use blaze_keyed::{driver::TaskFailed, record::Fact};
use blaze_query::QueryKind;
use hir_def::{
    AdtId, AssocItemId, AttrDefId, EnumVariantId, ImplId, ModuleDefId, ModuleId, TraitId, VariantId,
    nameres::crate_def_map,
};
use hir_ty::next_solver::goal_store::{FactObject, FactRef};

use crate::{
    facts::{self, Class},
    surface::TextIds,
};

/// Appends to `rows` one row per fact `krate`'s surface holds, as the module doc says, and one
/// maker row per body owner `text_ids` names. `omit` leaves out every fact of one query kind:
/// the control of the test that every kind has a row, and nothing else. Where a row is refused
/// for a fact with no value ([`facts::UNHASHED`]), that fact is put in `unvalued`, so the
/// caller can say the item and why ([`facts::unhashed_line`]).
pub fn collect<T: TextIds, A: Allocator>(
    db: &Db,
    krate: Crate,
    omit: Option<QueryKind>,
    text_ids: &T,
    rows: &mut Vec<Fact, A>,
    unvalued: &mut Option<FactRef>,
) -> Result<(), TaskFailed> {
    // One span a phase, so a profile says which part of a slow surface is which (rules 15,
    // 16). The phases are the walk's own order but for layouts and drop checks, which are
    // asked last ([`Sink::layouts`]): the rows are sorted by the caller, so the order asked
    // changes no record.
    // Which body makes the facts about each body-bearing definition: a body with no text id
    // (a discriminant, a body a macro made) has none, and its made facts are answered
    // `absent` where its unit's bodies are not checked.
    {
        let span = tracing::info_span!(target: "blaze", "surface: maker rows of the crate's body owners", owners = tracing::field::Empty)
            .entered();
        let owners = hir_def::nameres::demand::crate_body_owners(db, krate);
        span.record("owners", owners.len() as u64);
        for &def in owners.iter() {
            if let Some(text) = text_ids.text_id(def) {
                rows.push(Fact { id: facts::maker_id(&values::body_key(db, def)), digest: text.0 });
            }
        }
    }
    let mut sink = Sink { db, krate, omit, rows, layouts: Vec::new_in(Global), items: 0, unvalued };
    {
        let _span = tracing::info_span!(target: "blaze", "surface: the crate's own facts").entered();
        let crate_key = key::crate_key(db, krate);
        sink.fact(values::crate_root(db, krate))?;
        sink.fact(plain(items::TARGET, crate_key))?;
        sink.fact(values::crate_names(db, krate))?;
        // The crate's settings as another crate's body reads them, and the walk of its modules:
        // both follow the crate's settings and module tree, which its surface key covers.
        sink.fact(values::crate_settings(db, krate))?;
        sink.fact(values::modules(db, krate))?;
        sink.fact(values::field_order())?;
        // Each setting is a fact of its own field, so a body that read one field is not moved
        // by another's change.
        for field in base_db::SettingField::ALL {
            sink.fact(values::setting(field))?;
        }
    }

    // The def map is resolved already where the image parts ran first (`surface::surface`);
    // this span is its share otherwise.
    let def_map = {
        let _span = tracing::info_span!(target: "blaze", "surface: the crate's def map").entered();
        crate_def_map(db, krate)
    };
    {
        let span = tracing::info_span!(target: "blaze", "surface: module scopes", modules = tracing::field::Empty)
            .entered();
        let mut modules = 0_u64;
        for (module, _) in def_map.modules(db) {
            sink.scope(module)?;
            modules += 1;
        }
        span.record("modules", modules);
    }
    {
        let span = tracing::info_span!(target: "blaze", "surface: signatures, names and attributes of module items", items = tracing::field::Empty)
            .entered();
        for (module, _) in def_map.modules(db) {
            sink.module_items(module)?;
        }
        span.record("items", sink.items);
    }
    // The impls of the crate's bodies (blocks), which no module scope lists; a module's are
    // reached twice and deduplicated by the caller. Listing them lowers the bodies with blocks
    // that hold impls.
    {
        let span = tracing::info_span!(target: "blaze", "surface: trait impls of the crate, its bodies' included", impls = tracing::field::Empty)
            .entered();
        let trait_impls = hir_ty::method_resolution::TraitImpls::for_crate(db, krate);
        let mut impls = 0_u64;
        for impl_ in trait_impls.impls() {
            sink.impl_(impl_)?;
            impls += 1;
        }
        span.record("impls", impls);
    }
    {
        let span = tracing::info_span!(target: "blaze", "surface: inherent impls of the crate, its bodies' included", impls = tracing::field::Empty)
            .entered();
        let inherent = hir_ty::method_resolution::InherentImpls::for_crate(db, krate);
        let mut count = 0_u64;
        for (_, impls) in inherent.iter() {
            for &impl_ in impls {
                sink.impl_(impl_)?;
                count += 1;
            }
        }
        span.record("impls", count);
    }
    {
        let span = tracing::info_span!(target: "blaze", "surface: layouts and drop checks of the crate's ADTs", adts = tracing::field::Empty)
            .entered();
        span.record("adts", sink.layouts.len() as u64);
        sink.layouts()?;
    }
    Ok(())
}

fn plain(kind: QueryKind, subject: ContentId) -> FactRef {
    FactRef { kind, subject, object: FactObject::None }
}

fn named(kind: QueryKind, subject: ContentId, name: &str) -> FactRef {
    FactRef { kind, subject, object: FactObject::Id(key::name_hash(name).0) }
}

struct Sink<'a, A: Allocator> {
    db: &'a Db,
    krate: Crate,
    omit: Option<QueryKind>,
    rows: &'a mut Vec<Fact, A>,
    /// The subjects of the ADTs reached, whose `Layout` and `DropCheck` rows [`Self::layouts`]
    /// writes after the walk, under a span of their own: layouts are the costly kind, and
    /// asked inline they would hide inside the items' span. Each ADT reached is pushed once a
    /// time it is reached, as its rows were written inline; the caller's sort deduplicates.
    layouts: Vec<ContentId, Global>,
    /// The items whose signature, name and attributes were asked ([`Self::item`]), counted for
    /// the items' span.
    items: u64,
    /// The fact whose row was refused for having no value ([`facts::UNHASHED`]), if one was.
    unvalued: &'a mut Option<FactRef>,
}

impl<A: Allocator> Sink<'_, A> {
    /// The row of `fact`, if it has one; a fact a surface holds no row of by rule (a listing, a
    /// fact a body makes, a refused kind) is a bug of this walk, refused by name
    /// ([`facts::value_of`]).
    fn fact(&mut self, fact: FactRef) -> Result<(), TaskFailed> {
        if self.omit == Some(fact.kind) {
            return Ok(());
        }
        let unvalued = &mut *self.unvalued;
        let mut refused = |error: facts::FactError| {
            if error.what == facts::UNHASHED {
                *unvalued = Some(fact);
            }
            error.failed()
        };
        if matches!(facts::class_of(fact.kind.0), Some(Class::Item))
            && !facts::has_row(self.db, self.krate, &fact).map_err(&mut refused)?
        {
            return Ok(());
        }
        let digest = facts::value_of(self.db, self.krate, &fact).map_err(&mut refused)?;
        let id = facts::id_of(&fact).map_err(refused)?;
        self.rows.push(Fact { id, digest });
        Ok(())
    }

    fn attrs(&mut self, owner: AttrDefId) -> Result<(), TaskFailed> {
        match values::attrs(self.db, owner) {
            Some(fact) => self.fact(fact),
            None => Ok(()),
        }
    }

    /// A module's own facts: the traits in its scope, its whole scope, its attributes and a
    /// `Resolves` per name its scope holds.
    fn scope(&mut self, module: ModuleId) -> Result<(), TaskFailed> {
        let db = self.db;
        self.fact(values::traits_in_scope(db, module))?;
        self.fact(values::whole_scope(db, module))?;
        self.attrs(AttrDefId::ModuleId(module))?;
        let scope = module.def_map(db).module(db, module).scope(db);
        for (name, per_ns) in scope.entries() {
            if !per_ns.is_none() {
                self.fact(values::resolves(db, module, name))?;
            }
        }
        for (name, _) in scope.legacy_macros() {
            self.fact(values::resolves(db, module, name))?;
        }
        Ok(())
    }

    /// The items a module declares, its impls and its unnamed consts, each with its own facts
    /// (an ADT's layout and drop check deferred to [`Self::layouts`]).
    fn module_items(&mut self, module: ModuleId) -> Result<(), TaskFailed> {
        let db = self.db;
        let scope = module.def_map(db).module(db, module).scope(db);
        for def in scope.declarations() {
            self.declaration(def)?;
        }
        for impl_ in scope.impls() {
            self.impl_(impl_)?;
        }
        for const_ in scope.unnamed_consts() {
            self.item(ItemId::Const(const_))?;
        }
        Ok(())
    }

    fn declaration(&mut self, def: ModuleDefId) -> Result<(), TaskFailed> {
        match def {
            // A module's own facts come from the def map's walk of every module.
            ModuleDefId::ModuleId(_) | ModuleDefId::BuiltinType(_) => Ok(()),
            // A variant is declared by its enum and walked there.
            ModuleDefId::EnumVariantId(_) => Ok(()),
            ModuleDefId::FunctionId(it) => self.item(ItemId::Function(it)),
            ModuleDefId::AdtId(it) => self.adt(it),
            ModuleDefId::ConstId(it) => self.item(ItemId::Const(it)),
            ModuleDefId::StaticId(it) => self.item(ItemId::Static(it)),
            ModuleDefId::TraitId(it) => self.trait_(it),
            ModuleDefId::TypeAliasId(it) => self.item(ItemId::Alias(it)),
            ModuleDefId::MacroId(it) => self.item(ItemId::Macro(it)),
        }
    }

    /// The facts every item has: its signature, attributes and name.
    fn item(&mut self, item: ItemId) -> Result<(), TaskFailed> {
        self.items += 1;
        let subject = key::item_key(self.db, item).0;
        self.fact(plain(items::SIGNATURE, subject))?;
        self.fact(plain(items::NAME, subject))?;
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
        self.attrs(owner)
    }

    /// A struct's, union's or variant's fields: a `Field` by position and a `Resolves` by name
    /// in the ADT's (or variant's) scope.
    fn fields(&mut self, variant: VariantId, subject: ContentId) -> Result<(), TaskFailed> {
        let db = self.db;
        for (field, data) in variant.fields(db).fields().iter() {
            let position = u32::from(field.into_raw());
            self.fact(FactRef { kind: items::FIELD, subject, object: FactObject::Field(position) })?;
            self.fact(named(items::RESOLVES, subject, data.name.as_str()))?;
        }
        Ok(())
    }

    fn adt(&mut self, adt: AdtId) -> Result<(), TaskFailed> {
        let db = self.db;
        let item = ItemId::Adt(adt);
        self.item(item)?;
        let subject = key::item_key(db, item).0;
        self.layouts.push(subject);
        self.fact(values::inherent_assoc_types(db, adt))?;
        self.fact(values::fields(db, item))?;
        match adt {
            AdtId::StructId(it) => self.fields(it.into(), subject),
            AdtId::UnionId(it) => self.fields(it.into(), subject),
            AdtId::EnumId(it) => {
                for (name, &(variant, _)) in it.enum_variants(db).variants.iter() {
                    self.fact(named(items::RESOLVES, subject, name.as_str()))?;
                    self.variant(variant)?;
                }
                Ok(())
            }
        }
    }

    /// A variant's attributes and fields. A variant is no item (blaze-items' `Resolved::Variant`):
    /// it has no `Signature` or `Name` fact of its own; what a body reads of its shape is its
    /// enum's `Signature` and `Fields` (blaze-item-hash `reads`, lane items' `fact_of_read`),
    /// written by [`Self::adt`], and its discriminant is a `ConstValue` a body makes.
    fn variant(&mut self, variant: EnumVariantId) -> Result<(), TaskFailed> {
        let subject = key::variant_key(self.db, variant).0;
        self.attrs(AttrDefId::EnumVariantId(variant))?;
        self.fields(variant.into(), subject)
    }

    fn assoc(&mut self, subject: ContentId, items_: &[(hir_expand::name::Name, AssocItemId)]) -> Result<(), TaskFailed> {
        for (name, item) in items_ {
            self.fact(named(items::RESOLVES, subject, name.as_str()))?;
            match *item {
                AssocItemId::FunctionId(it) => {
                    self.item(ItemId::Function(it))?;
                    let method = key::item_key(self.db, ItemId::Function(it)).0;
                    self.fact(plain(items::RPITIT_TYS, method))?;
                }
                AssocItemId::ConstId(it) => self.item(ItemId::Const(it))?,
                AssocItemId::TypeAliasId(it) => self.item(ItemId::Alias(it))?,
            }
        }
        Ok(())
    }

    fn trait_(&mut self, trait_: TraitId) -> Result<(), TaskFailed> {
        let db = self.db;
        let item = ItemId::Trait(trait_);
        self.item(item)?;
        self.fact(values::fields(db, item))?;
        let subject = key::item_key(db, item).0;
        self.assoc(subject, &trait_.trait_items(db).items)
    }

    fn impl_(&mut self, impl_: ImplId) -> Result<(), TaskFailed> {
        let db = self.db;
        let item = ItemId::Impl(impl_);
        self.item(item)?;
        self.fact(values::fields(db, item))?;
        let subject = key::item_key(db, item).0;
        self.fact(plain(items::COHERENCE, subject))?;
        self.fact(plain(items::SPECIALIZES, subject))?;
        self.assoc(subject, &impl_.impl_items(db).items)
    }

    /// The `Layout` and `DropCheck` rows of every ADT the walk reached, in the order reached.
    fn layouts(&mut self) -> Result<(), TaskFailed> {
        let subjects = core::mem::replace(&mut self.layouts, Vec::new_in(Global));
        for &subject in subjects.iter() {
            self.fact(plain(items::LAYOUT, subject))?;
            self.fact(plain(items::DROP_CHECK, subject))?;
        }
        Ok(())
    }
}
