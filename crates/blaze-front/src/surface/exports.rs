//! The rows of a crate's surface: one per fact of an item, module or the crate that exists now
//! ([`facts::has_row`]), each by [`facts::id_of`] (the one id, `record::fact` with its owner,
//! the id a body asks the same fact by) and [`facts::value_of`].
//!
//! What is walked, once: the crate (`CrateRoot`, `Target`, `CrateNames`, and the build facts
//! `FieldOrder` and `Setting`, owned by `Owner::ASKER`), every module of its def map
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
//! read refuses. A body's steps are no row either: no body's result is a surface's.
//!
//! Floor: one value per row, each computed once (the crate's own scopes once). Costs past it:
//! an item reached from two places (a module's declarations and the impl lists) is looked at
//! twice and its row deduplicated by the caller's sort.

#![blaze::sans_io]

use std::alloc::Global;

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

use crate::facts::{self, Class};

/// Appends to `rows` one row per fact `krate`'s surface holds, as the module doc says. `omit`
/// leaves out every fact of one query kind: the control of the test that every kind has a row,
/// and nothing else.
pub fn collect(db: &Db, krate: Crate, omit: Option<QueryKind>, rows: &mut Vec<Fact, Global>) -> Result<(), TaskFailed> {
    let mut sink = Sink { db, krate, omit, rows };
    let crate_key = key::crate_key(db, krate);
    sink.fact(values::crate_root(db, krate))?;
    sink.fact(plain(items::TARGET, crate_key))?;
    sink.fact(values::crate_names(db, krate))?;
    sink.fact(values::field_order())?;
    if let Some(setting) = values::setting("borrowck_debug") {
        sink.fact(setting)?;
    }

    let def_map = crate_def_map(db, krate);
    for (module, _) in def_map.modules(db) {
        sink.module(module)?;
    }
    // The impls of the crate's bodies (blocks), which no module scope lists; a module's are
    // reached twice and deduplicated by the caller.
    let trait_impls = hir_ty::method_resolution::TraitImpls::for_crate(db, krate);
    for impl_ in trait_impls.impls() {
        sink.impl_(impl_)?;
    }
    let inherent = hir_ty::method_resolution::InherentImpls::for_crate(db, krate);
    for (_, impls) in inherent.iter() {
        for &impl_ in impls {
            sink.impl_(impl_)?;
        }
    }
    Ok(())
}

fn plain(kind: QueryKind, subject: ContentId) -> FactRef {
    FactRef { kind, subject, object: FactObject::None }
}

fn named(kind: QueryKind, subject: ContentId, name: &str) -> FactRef {
    FactRef { kind, subject, object: FactObject::Id(key::name_hash(name).0) }
}

struct Sink<'a> {
    db: &'a Db,
    krate: Crate,
    omit: Option<QueryKind>,
    rows: &'a mut Vec<Fact, Global>,
}

impl Sink<'_> {
    /// The row of `fact`, if it has one; a fact a surface holds no row of by rule (a listing, a
    /// fact a body makes, a refused kind) is a bug of this walk, refused by name
    /// ([`facts::value_of`]).
    fn fact(&mut self, fact: FactRef) -> Result<(), TaskFailed> {
        if self.omit == Some(fact.kind) {
            return Ok(());
        }
        let refused = |error: facts::FactError| TaskFailed::Refused { what: error.what };
        if matches!(facts::class_of(fact.kind.0), Some(Class::Item))
            && !facts::has_row(self.db, self.krate, &fact).map_err(refused)?
        {
            return Ok(());
        }
        let digest = facts::value_of(self.db, self.krate, &fact).map_err(refused)?;
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

    fn module(&mut self, module: ModuleId) -> Result<(), TaskFailed> {
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
        self.fact(plain(items::LAYOUT, subject))?;
        self.fact(plain(items::DROP_CHECK, subject))?;
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
}
