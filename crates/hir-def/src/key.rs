//! Ids from item keys and back (docs/v2/plan/design/reconnect.md, "Ids";
//! docs/v2/plan/design/identity.md).
//!
//! Every id of an item is interned from its [`ItemKey`] and the container its record names
//! (`Items::parent`), read once when the id is made. The one pair of functions between the two
//! forms is blaze-id's: [`blaze_id::KeyOf`] and [`blaze_id::Resolve`], implemented here for
//! `Db` and every id type (the ids are this crate's, so the orphan rule allows it).
//!
//! Every read here is of a record; one the task lacks stops the task with a [`base_db::Need`]
//! naming its key ([`OrNeed`]).

use base_db::{Crate, Db, ItemKey, OrNeed, SurfaceError};
use blaze_id::{CrateItemKey, KeyOf, NoItem, Resolve};
use blaze_surface::{ItemKind, Items, Place};
use hir_expand::{AstId, HirFileId, name::Name};
use intern::sym;
use span::AstIdNode;
use syntax::{AstNode, AstPtr, TextRange, TextSize};

use std::alloc::Global;

use hir_expand::MacroCallStyles;

use crate::{
    AdtId, AssocItemLoc, ConstId, EnumId, EnumVariantId, EnumVariantLoc, ExternCrateId,
    FunctionId, ImplId, Intern, ItemContainerId, ItemLoc, Lookup, Macro2Loc, MacroExpander,
    MacroId, MacroRulesLoc, MacroRulesLocFlags, ModuleDefId, ModuleId, ModuleIdLt, ProcMacroLoc,
    StaticId, StructId, TraitId, TypeAliasId, UnionId, UseId,
};

/// The crate an item key is of, which the task holds; one it does not is a stop naming the
/// item (a key carries only its crate's prefix, so the item is what the stop can name: the
/// crate graph record that lists its crate is the one missing).
pub fn crate_of(db: &Db, key: ItemKey) -> Crate {
    match db.crate_of_item(key) {
        Some(krate) => krate,
        None => Err(SurfaceError::NoRecord { fact: "CrateGraph", item: key }).or_need(),
    }
}

/// The module of a module's item key: its crate, its parent module and its name, each read
/// from the records once.
pub fn module_of_key(db: &Db, key: ItemKey) -> ModuleId {
    let krate = crate_of(db, key);
    let items = db.items();
    let parent = items.parent(key).or_need();
    let (parent, name) = match parent {
        None => (None, Name::new_symbol_root(sym::__empty)),
        Some(parent) => (Some(module_of_key(db, parent)), item_name(db, key)),
    };
    ModuleIdLt::new(db, krate, None, parent, name, Some(key))
}

/// The root module of a crate: the module whose key the crate's `CrateRoot` record names.
pub fn crate_root_module(db: &Db, krate: Crate) -> ModuleId {
    let root = db.items().crate_root(krate.key(db)).or_need();
    module_of_key(db, root.head().root_module)
}

/// An item's name, as its file's items record holds it.
pub fn item_name(db: &Db, key: ItemKey) -> Name {
    let items = db.items();
    match items.place(key).or_need() {
        Place::File { file, index } => {
            let view = items.file_items(file).or_need();
            let Some(item) = view.item(index) else {
                panic!("item {key:x}: its key index names row {} of a file with fewer", index.0)
            };
            Name::new_symbol_root(intern::Symbol::intern(item.name))
        }
        Place::Expansion { .. } => {
            Err(SurfaceError::NotBuilt(blaze_surface::NotBuilt { method: "item_name of an expansion's item", item: key }))
                .or_need()
        }
    }
}

/// The container of an associated item or a module-level one, from its record's parent.
pub fn container_of(db: &Db, key: ItemKey) -> ItemContainerId {
    let items = db.items();
    let Some(parent) = items.parent(key).or_need() else {
        panic!("item {key:x} has no parent: only a crate root module is so, and it is no member")
    };
    match items.kind(parent).or_need() {
        ItemKind::Trait => ItemContainerId::TraitId(TraitId::of_key(db, parent)),
        ItemKind::Impl => ItemContainerId::ImplId(ImplId::of_key(db, parent)),
        ItemKind::Module => ItemContainerId::ModuleId(module_of_key(db, parent)),
        other => panic!(
            "item {key:x} is inside a {}: a record names no such container",
            other.name()
        ),
    }
}

/// The module an item is declared in: its container's module.
fn module_container_of(db: &Db, key: ItemKey) -> ModuleId {
    let Some(parent) = db.items().parent(key).or_need() else {
        panic!("item {key:x} has no parent: only a crate root module is so")
    };
    module_of_key(db, parent)
}

/// An id made from an item key, and the key it is made from.
pub trait FromKey: Sized + Copy {
    const KIND: ItemKind;
    fn of_key(db: &Db, key: ItemKey) -> Self;
    fn key(self, db: &Db) -> ItemKey;
}

macro_rules! module_item {
    ($($id:ident => $kind:ident),* $(,)?) => {$(
        impl FromKey for $id {
            const KIND: ItemKind = ItemKind::$kind;
            fn of_key(db: &Db, key: ItemKey) -> Self {
                ItemLoc::new(module_container_of(db, key), key).intern(db)
            }
            fn key(self, db: &Db) -> ItemKey {
                self.lookup(db).key
            }
        }
    )*};
}

macro_rules! assoc_item {
    ($($id:ident => $kind:ident),* $(,)?) => {$(
        impl FromKey for $id {
            const KIND: ItemKind = ItemKind::$kind;
            fn of_key(db: &Db, key: ItemKey) -> Self {
                AssocItemLoc::new(container_of(db, key), key).intern(db)
            }
            fn key(self, db: &Db) -> ItemKey {
                self.lookup(db).key
            }
        }
    )*};
}

module_item!(
    StructId => Struct, UnionId => Union, EnumId => Enum, TraitId => Trait, ImplId => Impl,
    UseId => Use, ExternCrateId => ExternCrate,
);
assoc_item!(FunctionId => Function, ConstId => Const, StaticId => Static, TypeAliasId => TypeAlias);

impl FromKey for EnumVariantId {
    const KIND: ItemKind = ItemKind::Variant;
    fn of_key(db: &Db, key: ItemKey) -> Self {
        let Some(parent) = db.items().parent(key).or_need() else {
            panic!("variant {key:x} has no enum")
        };
        let loc = EnumVariantLoc { key, parent: EnumId::of_key(db, parent), name: item_name(db, key) };
        loc.intern(db)
    }
    fn key(self, db: &Db) -> ItemKey {
        self.lookup(db).key
    }
}

impl FromKey for ModuleId {
    const KIND: ItemKind = ItemKind::Module;
    fn of_key(db: &Db, key: ItemKey) -> Self {
        module_of_key(db, key)
    }
    fn key(self, db: &Db) -> ItemKey {
        match ModuleIdLt::key(self, db) {
            Some(key) => key,
            None => panic!("a block's module is no item and has no key"),
        }
    }
}

macro_rules! key_of_and_resolve {
    ($($id:ty),* $(,)?) => {$(
        impl KeyOf<$id> for Db {
            fn key_of(&self, id: $id) -> ItemKey {
                FromKey::key(id, self)
            }
        }

        impl Resolve<$id> for Db {
            /// The id of the item `key` names in `krate`: its kind is checked against the id's,
            /// and a key of another kind is no item of this type.
            fn resolve(&self, krate: CrateItemKey, key: ItemKey) -> Result<$id, NoItem> {
                let no_item = NoItem { krate, key };
                if !key.is_of(krate) {
                    return Err(no_item);
                }
                match self.items().kind(key) {
                    Ok(kind) if kind == <$id as FromKey>::KIND => Ok(<$id as FromKey>::of_key(self, key)),
                    Ok(_) | Err(SurfaceError::NoItem(_)) => Err(no_item),
                    Err(other) => Err(other).or_need(),
                }
            }
        }
    )*};
}

key_of_and_resolve!(
    FunctionId, StructId, UnionId, EnumId, EnumVariantId, ConstId, StaticId, TraitId,
    TypeAliasId, ImplId, UseId, ExternCrateId, ModuleId,
);

/// The [`AstId`] of an item's node: found by the item's range in its file's items record, in
/// the parse of that file. Only the crate whose text the task holds has syntax; asking it of
/// another crate's item is a typed stop (`NotBuilt`), never a read of that crate's text.
pub fn ast_id_of<N: AstIdNode>(db: &Db, key: ItemKey) -> AstId<N> {
    let not_built = |method| {
        Err::<AstId<N>, _>(SurfaceError::NotBuilt(blaze_surface::NotBuilt { method, item: key })).or_need()
    };
    let krate = crate_of(db, key);
    if db.source_of(krate.key(db)).is_none() {
        return not_built("ast_id_of an item of a crate without text");
    }
    let items = db.items();
    let (file, index) = match items.place(key).or_need() {
        Place::File { file, index } => (file, index),
        Place::Expansion { .. } => return not_built("ast_id_of an expansion's item"),
    };
    let view = items.file_items(file).or_need();
    let Some(item) = view.item(index) else {
        panic!("item {key:x}: its key index names row {} of a file with fewer", index.0)
    };
    let Some(file_id) = db.file_id(file) else {
        panic!("item {key:x}: its file {file:x} is not of the task's file table")
    };
    let file_id = base_db::EditionedFileId::new_with_extensions(
        db,
        file_id,
        krate.data(db).edition,
        krate.extensions(db),
    );
    let range = TextRange::at(TextSize::new(item.range.start), TextSize::new(item.range.len()));
    let tree = file_id.parse(db).tree();
    let node = tree
        .syntax()
        .covering_element(range)
        .ancestors()
        .filter_map(N::cast)
        .find(|node| node.syntax().text_range() == range);
    let Some(node) = node else {
        panic!("item {key:x}: no {} node covers {range:?} of its file", std::any::type_name::<N>())
    };
    let hir_file = HirFileId::from(file_id);
    let id = hir_file.ast_id_map(db).ast_id_for_ptr(AstPtr::new(&node));
    AstId::new(hir_file, id)
}

/// What a scope binding's target is, as the type checker names it: by the item's kind. An
/// `impl`, a `use` and a field are bound in no namespace; one a record names so is a broken
/// record, which this names.
pub fn module_def_of_key(db: &Db, key: ItemKey) -> ModuleDefId {
    match db.items().kind(key).or_need() {
        ItemKind::Function => ModuleDefId::FunctionId(FunctionId::of_key(db, key)),
        ItemKind::Struct => ModuleDefId::AdtId(AdtId::StructId(StructId::of_key(db, key))),
        ItemKind::Enum => ModuleDefId::AdtId(AdtId::EnumId(EnumId::of_key(db, key))),
        ItemKind::Union => ModuleDefId::AdtId(AdtId::UnionId(UnionId::of_key(db, key))),
        ItemKind::Trait => ModuleDefId::TraitId(TraitId::of_key(db, key)),
        ItemKind::Const => ModuleDefId::ConstId(ConstId::of_key(db, key)),
        ItemKind::Static => ModuleDefId::StaticId(StaticId::of_key(db, key)),
        ItemKind::TypeAlias => ModuleDefId::TypeAliasId(TypeAliasId::of_key(db, key)),
        ItemKind::Module => ModuleDefId::ModuleId(module_of_key(db, key)),
        ItemKind::Variant => ModuleDefId::EnumVariantId(EnumVariantId::of_key(db, key)),
        ItemKind::MacroRules | ItemKind::Macro2 => ModuleDefId::MacroId(macro_of_key(db, key)),
        // A trait alias has no id of the type checker's yet: a typed stop, not a wrong id.
        ItemKind::TraitAlias => Err(SurfaceError::NotBuilt(blaze_surface::NotBuilt {
            method: "a trait alias as the type checker names it",
            item: key,
        }))
        .or_need(),
        kind @ (ItemKind::Impl
        | ItemKind::Use
        | ItemKind::ExternCrate
        | ItemKind::Field
        | ItemKind::ExternBlock
        | ItemKind::MacroCall
        | ItemKind::Error
        | ItemKind::Unhandled) => {
            panic!("item {key:x}: a scope binds it, but a {} is bound in no namespace", kind.name())
        }
    }
}

/// The macro a `MacroDef` record describes, as the id the expander is asked through. The
/// flags a macro's attributes set (`#[allow_internal_unsafe]`, `#[rustc_macro_edition_2021]`)
/// are not in the record's head yet (reconnect.md, "What the new-layer crates lack"); they are
/// read by expansion, which runs from `Expansion` records, never from these ids.
pub fn macro_of_key(db: &Db, key: ItemKey) -> MacroId {
    use hir_expand::{
        builtin::{find_builtin_attr, find_builtin_derive, find_builtin_macro},
        proc_macro::{CustomProcMacroExpander, ProcMacroKind, ProcMacros},
    };
    use blaze_surface::MacroKind;

    let def = db.items().macro_def(key).or_need();
    let head = def.head();
    let container = module_container_of(db, key);
    let krate = container.krate(db);
    let edition = krate.data(db).edition;
    let name = Name::new_symbol_root(intern::Symbol::intern(head.name));
    let declarative = MacroExpander::Declarative { styles: MacroCallStyles::FN_LIKE };
    let proc_macro = |kind: ProcMacroKind| {
        let listed = ProcMacros::get_for_crate(db, krate)
            .and_then(|macros| macros.macros())
            .and_then(|macros| macros.iter().position(|it| it.name == *name.symbol()));
        let expander = match listed {
            Some(at) => CustomProcMacroExpander::new(at as u32),
            None => CustomProcMacroExpander::missing_expander(),
        };
        MacroId::ProcMacroId(ProcMacroLoc { container, key, expander, kind, edition }.intern(db))
    };
    match head.kind {
        MacroKind::Rules => {
            let mut flags = MacroRulesLocFlags::empty();
            flags.set(MacroRulesLocFlags::LOCAL_INNER, head.local_inner);
            let loc = MacroRulesLoc { container, key, expander: declarative, flags, edition };
            MacroId::MacroRulesId(loc.intern(db))
        }
        MacroKind::Macro2 => {
            let loc = Macro2Loc { container, key, expander: declarative, allow_internal_unsafe: false, edition };
            MacroId::Macro2Id(loc.intern(db))
        }
        // The record names which builtin; the expander is found by the definition's name, of
        // the kind the record says (a name the expander lacks is an unimplemented builtin).
        kind @ (MacroKind::BuiltinFn(_) | MacroKind::BuiltinAttr(_) | MacroKind::BuiltinDerive(_)) => {
            let expander = match kind {
                MacroKind::BuiltinFn(_) => match find_builtin_macro(&name) {
                    Some(either::Either::Left(it)) => MacroExpander::BuiltIn(it),
                    Some(either::Either::Right(it)) => MacroExpander::BuiltInEager(it),
                    None => MacroExpander::UnimplementedBuiltIn,
                },
                MacroKind::BuiltinAttr(_) => match find_builtin_attr(&name) {
                    Some(it) => MacroExpander::BuiltInAttr(it),
                    None => MacroExpander::UnimplementedBuiltIn,
                },
                _ => match find_builtin_derive(&name) {
                    Some(it) => MacroExpander::BuiltInDerive(it),
                    None => MacroExpander::UnimplementedBuiltIn,
                },
            };
            let loc = Macro2Loc { container, key, expander, allow_internal_unsafe: false, edition };
            MacroId::Macro2Id(loc.intern(db))
        }
        // The proc macro's crate is the record's `krate`, which is the crate its key is of:
        // the definition is an item of the proc-macro crate's root.
        MacroKind::ProcMacro { kind, .. } => proc_macro(match kind {
            blaze_surface::ProcKind::FnLike => ProcMacroKind::Bang,
            blaze_surface::ProcKind::Attr => ProcMacroKind::Attr,
            blaze_surface::ProcKind::Derive => ProcMacroKind::CustomDerive,
        }),
    }
}

/// The module an item is in: itself for a module, else the nearest module above it.
pub fn module_around(db: &Db, key: ItemKey) -> ItemKey {
    let items = db.items();
    let mut at = key;
    // Each turn moves one parent up, ending at a module (a crate root has no parent and is one).
    loop {
        if items.kind(at).or_need() == ItemKind::Module {
            return at;
        }
        match items.parent(at).or_need() {
            Some(parent) => at = parent,
            None => panic!("item {key:x}: its parents end at {at:x}, which is no module"),
        }
    }
}

/// The items inside `parent` (a module's, a trait's or an impl's associated items, an enum's
/// variants, a struct's fields), in the order they are written, onto `out`: one read of the
/// key-index page of the module around it ([`base_db::Records::members`]).
pub fn members_into(db: &Db, parent: ItemKey, out: &mut Vec<(Place, ItemKey), Global>) {
    let module = module_around(db, parent);
    db.items().members(module, parent, out).or_need();
}

/// [`members_into`] into a new list, keys only.
pub fn members(db: &Db, parent: ItemKey) -> Vec<ItemKey, Global> {
    let mut placed = Vec::new_in(Global);
    members_into(db, parent, &mut placed);
    let mut keys = Vec::with_capacity_in(placed.len(), Global);
    keys.extend(placed.into_iter().map(|(_, key)| key));
    keys
}
