//! The key of an item's record: what is asked of it and where it is written, in terms that are
//! the same in every process (no query database id), hashed into a [`ContentId`].
//!
//! An item is named by its container and its ast id. A module is its index in the crate's
//! `DefMap` (which the image's own def map part gives in the same order); an impl, a trait or an
//! extern block by its module and ast id. An ast id's file is either the file of its module (a
//! file is written by its edition alone: the container says which file it is) or a macro
//! file, written as the call: its kind, the ast id of the call site (again a file and an id),
//! and the derive's place or whether it is eager. Nothing here asks for a parse or an
//! expansion: every piece is a field of an interned location.
//!
//! An item in a block (of a body or of a signature) has no key: its module is made when that
//! block is lowered, and the part holds the items of the crate's modules only.

use base_db::{Crate, Db};
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Domain};
use hir_expand::{HirFileId, MacroCallId, MacroCallKind};
use smallvec::SmallVec;

use crate::{
    AdtId, AstId, ConstId, EnumId, FunctionId, HasModule, ImplId, ItemContainerId, Lookup,
    ModuleId, StaticId, StructId, TraitId, TypeAliasId, UnionId, VariantId, nameres::crate_def_map,
};

/// The domain of an item's key. Its version goes up whenever what [`of`] hashes changes: the
/// keys of values filed by an item's key (`deps::const_key`, `hir_ty::consteval::deps`) take
/// the key as it is, so a new encoding must give new keys.
pub(super) const KEY_DOMAIN: &str = "blaze v2 dependency signature key 1";

/// [`KEY_DOMAIN`] with its context key derived once, not on every key a serve hashes.
static DOMAIN: Domain = Domain::new(KEY_DOMAIN);

/// One record of the part: a signature query and the item it is asked of.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Item {
    Struct(StructId),
    Union(UnionId),
    Enum(EnumId),
    Const(ConstId),
    Static(StaticId),
    Impl(ImplId),
    Trait(TraitId),
    Function(FunctionId),
    TypeAlias(TypeAliasId),
    /// `VariantFields::of`.
    Fields(VariantId),
    /// `EnumVariants::of`.
    Variants(EnumId),
}

impl Item {
    /// Which query, the first byte of the key and the record's `what`.
    pub(super) fn what(self) -> u8 {
        match self {
            Item::Struct(_) => 0,
            Item::Union(_) => 1,
            Item::Enum(_) => 2,
            Item::Const(_) => 3,
            Item::Static(_) => 4,
            Item::Impl(_) => 5,
            Item::Trait(_) => 6,
            Item::Function(_) => 7,
            Item::TypeAlias(_) => 8,
            Item::Fields(_) => 9,
            Item::Variants(_) => 10,
        }
    }

    /// The module the item is in.
    pub(super) fn module(self, db: &Db) -> ModuleId {
        match self {
            Item::Struct(it) => it.lookup(db).container,
            Item::Union(it) => it.lookup(db).container,
            Item::Enum(it) | Item::Variants(it) => it.lookup(db).container,
            Item::Impl(it) => it.lookup(db).container,
            Item::Trait(it) => it.lookup(db).container,
            Item::Const(it) => it.module(db),
            Item::Static(it) => it.module(db),
            Item::Function(it) => it.module(db),
            Item::TypeAlias(it) => it.module(db),
            Item::Fields(it) => it.module(db),
        }
    }
}

/// The modules of `krate`'s `DefMap` by their index in it.
#[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
pub(super) fn module_indexes(db: &Db, krate: Crate) -> HashMap<ModuleId, u32> {
    let modules = crate_def_map(db, krate).modules(db);
    // alloc: once per crate, the map every key of the crate reads.
    modules.enumerate().map(|(index, (module, _))| (module, index as u32)).collect()
}

/// The key of `item`'s record, and its crate; `None` for an item in a block.
pub(super) fn of(db: &Db, item: Item) -> Option<(Crate, ContentId)> {
    let module = item.module(db);
    if module.block(db).is_some() {
        return None;
    }
    let krate = module.krate(db);
    let mut key = Key { db, indexes: module_indexes(db, krate), bytes: SmallVec::new() };
    key.u8(item.what());
    match item {
        Item::Struct(it) => key.adt(AdtId::StructId(it)),
        Item::Union(it) => key.adt(AdtId::UnionId(it)),
        Item::Enum(it) | Item::Variants(it) => key.adt(AdtId::EnumId(it)),
        Item::Impl(it) => key.impl_(it),
        Item::Trait(it) => key.trait_(it),
        Item::Const(it) => {
            let loc = it.lookup(db);
            key.container(loc.container);
            key.ast(loc.id);
        }
        Item::Static(it) => {
            let loc = it.lookup(db);
            key.container(loc.container);
            key.ast(loc.id);
        }
        Item::Function(it) => {
            let loc = it.lookup(db);
            key.container(loc.container);
            key.ast(loc.id);
        }
        Item::TypeAlias(it) => {
            let loc = it.lookup(db);
            key.container(loc.container);
            key.ast(loc.id);
        }
        Item::Fields(VariantId::StructId(it)) => {
            key.u8(0);
            key.adt(AdtId::StructId(it));
        }
        Item::Fields(VariantId::UnionId(it)) => {
            key.u8(1);
            key.adt(AdtId::UnionId(it));
        }
        Item::Fields(VariantId::EnumVariantId(it)) => {
            key.u8(2);
            let loc = it.lookup(db);
            key.adt(AdtId::EnumId(loc.parent));
            key.ast(loc.id);
        }
    }
    Some((krate, ContentId::of_domain(&DOMAIN, &key.bytes)))
}

struct Key<'a> {
    db: &'a Db,
    indexes: &'a HashMap<ModuleId, u32>,
    bytes: SmallVec<[u8; 64]>,
}

impl Key<'_> {
    fn u8(&mut self, it: u8) {
        self.bytes.push(it);
    }

    fn u32(&mut self, it: u32) {
        self.bytes.extend_from_slice(&it.to_le_bytes());
    }

    fn module(&mut self, module: ModuleId) {
        let index = *self.indexes.get(&module).unwrap_or_else(|| {
            panic!("module {module:?} of an item is not in its crate's def map")
        });
        self.u32(index);
    }

    fn adt(&mut self, it: AdtId) {
        let db = self.db;
        let (container, file, id) = match it {
            AdtId::StructId(it) => {
                let loc = it.lookup(db);
                (loc.container, loc.id.file_id, loc.id.value.erase())
            }
            AdtId::UnionId(it) => {
                let loc = it.lookup(db);
                (loc.container, loc.id.file_id, loc.id.value.erase())
            }
            AdtId::EnumId(it) => {
                let loc = it.lookup(db);
                (loc.container, loc.id.file_id, loc.id.value.erase())
            }
        };
        self.module(container);
        self.file(file);
        self.u32(id.into_raw());
    }

    fn impl_(&mut self, it: ImplId) {
        let loc = it.lookup(self.db);
        self.module(loc.container);
        self.ast(loc.id);
    }

    fn trait_(&mut self, it: TraitId) {
        let loc = it.lookup(self.db);
        self.module(loc.container);
        self.ast(loc.id);
    }

    fn container(&mut self, it: ItemContainerId) {
        match it {
            ItemContainerId::ModuleId(module) => {
                self.u8(0);
                self.module(module);
            }
            ItemContainerId::ImplId(it) => {
                self.u8(1);
                self.impl_(it);
            }
            ItemContainerId::TraitId(it) => {
                self.u8(2);
                self.trait_(it);
            }
            ItemContainerId::ExternBlockId(it) => {
                self.u8(3);
                let loc = it.lookup(self.db);
                self.module(loc.container);
                self.ast(loc.id);
            }
        }
    }

    fn ast<N>(&mut self, id: AstId<N>) {
        self.file(id.file_id);
        self.u32(id.value.erase().into_raw());
    }

    fn file(&mut self, file: HirFileId) {
        match file {
            HirFileId::FileId(file) => {
                self.u8(0);
                self.u8(file.unpack(self.db).1 as u8);
            }
            HirFileId::MacroFile(call) => {
                self.u8(1);
                self.call(call);
            }
        }
    }

    fn call(&mut self, call: MacroCallId) {
        let loc = call.lookup(self.db);
        match &loc.kind {
            MacroCallKind::FnLike { ast_id, eager, .. } => {
                self.u8(0);
                self.ast(*ast_id);
                self.u8(eager.is_some() as u8);
            }
            MacroCallKind::Derive { ast_id, derive_attr_index, derive_index, .. } => {
                self.u8(1);
                self.ast(*ast_id);
                self.u32(derive_attr_index.item_tree_index());
                self.u32(*derive_index);
            }
            MacroCallKind::Attr { ast_id, .. } => {
                self.u8(2);
                self.ast(*ast_id);
            }
        }
    }
}
