//! The def map's accessors, narrowed to record reads (docs/v2/plan/design/reconnect.md,
//! "Narrow def-map accessors").
//!
//! No whole-crate map is built. [`crate_def_map`] gives a small handle per crate with the old
//! accessor names, each a read of one record: the crate's root module (`CrateRoot`), one
//! module's names (`ModuleScope`, read per name), its entry (`KeyIndex`). What only a walk of
//! every module answers ([`DefMap::modules`]) is the union of the crate's key-index pages; the
//! uses that need it are listed in the design.
//!
//! Every read of another crate's record that the task lacks stops it with a [`base_db::Need`].

use base_db::{Crate, Db, ItemKey, OrNeed, SurfaceError};
use blaze_surface::{Binding, Items, Namespace, NotBuilt, Target};
use hir_expand::name::Name;

use crate::{BlockId, ModuleId, key};

/// A crate's (or a block's) names, as a handle: what it reads is per question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DefMap {
    krate: Crate,
    block: Option<BlockId>,
    root: ModuleId,
}

/// The crate's handle. Its one read is the crate's `CrateRoot` record, for the root module.
#[blaze_memo::tracked(returns(ref), mem_tag = Nameres)]
pub fn crate_def_map(db: &Db, krate: Crate) -> DefMap {
    DefMap { krate, block: None, root: key::crate_root_module(db, krate) }
}

/// A block's handle: its scope is the block's own items, then its module's.
#[blaze_memo::tracked(returns(ref), mem_tag = Nameres)]
pub fn block_def_map(db: &Db, block: BlockId) -> DefMap {
    let module = block.module(db).to_static();
    DefMap { krate: module.krate(db), block: Some(block), root: module }
}

/// One module, as the old map's entry for it was asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleData {
    pub id: ModuleId,
}

impl ModuleData {
    /// The module's names, read one at a time.
    pub fn scope(self, _db: &Db) -> ModuleScope {
        ModuleScope { module: self.id }
    }

    /// The module that holds this one, `None` for a root.
    pub fn parent(self, db: &Db) -> Option<ModuleId> {
        self.id.containing_module_inside_def_map(db).map(crate::ModuleIdLt::to_static)
    }
}

/// A module's names over its `ModuleScope` record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleScope {
    module: ModuleId,
}

impl ModuleScope {
    fn key(self, db: &Db) -> ItemKey {
        match self.module.key(db) {
            Some(key) => key,
            None => Err(SurfaceError::NotBuilt(NotBuilt {
                method: "the scope of a block's module",
                item: ItemKey(blaze_id::ContentId([0; 32])),
            }))
            .or_need(),
        }
    }

    /// What `name` is bound to in `ns`: a typed refusal for a name the scope leaves
    /// undetermined or ambiguous, as blaze-scope wrote it.
    pub fn get(self, db: &Db, ns: Namespace, name: &Name) -> Option<Target<ItemKey>> {
        let view = db.items().scope(self.key(db)).or_need();
        view.get(ns, name.as_str()).map(|binding: Binding<'_>| binding.target)
    }

    /// Every binding of the module, for a caller that needs them all (a glob, a listing).
    pub fn for_each(self, db: &Db, mut each: impl FnMut(Binding<'_>)) {
        let view = db.items().scope(self.key(db)).or_need();
        view.bindings().for_each(&mut each);
    }
}

impl DefMap {
    pub fn krate(&self) -> Crate {
        self.krate
    }

    pub fn block_id(&self) -> Option<BlockId> {
        self.block
    }

    /// The crate's root module (a block's: its module).
    pub fn root_module_id(&self) -> ModuleId {
        self.root
    }

    pub fn crate_root(&self, db: &Db) -> ModuleId {
        match self.block {
            None => self.root,
            Some(_) => crate_def_map(db, self.krate).root,
        }
    }

    pub fn module(&self, _db: &Db, id: ModuleId) -> ModuleData {
        ModuleData { id }
    }

    /// Every module of the crate: the modules of its key-index pages, walked from the root by
    /// each module's `ModuleDecls`. Not built: the uses that need it (design, "Narrow def-map
    /// accessors") move to per-module or per-trait records first.
    pub fn modules(&self, db: &Db) -> std::iter::Empty<(ModuleId, ModuleData)> {
        let item = self.root.key(db).unwrap_or(ItemKey(blaze_id::ContentId([0; 32])));
        Err::<(), _>(SurfaceError::NotBuilt(NotBuilt { method: "DefMap::modules", item })).or_need();
        std::iter::empty()
    }

    /// The prelude module the crate's modules start from, from its crate root record; `None`
    /// under `#![no_core]` or `#![no_implicit_prelude]`.
    pub fn prelude(&self, db: &Db) -> Option<ModuleId> {
        let root = db.items().crate_root(self.krate.key(db)).or_need();
        root.head().prelude.map(|module| key::module_of_key(db, module))
    }

    /// The crate's `#![recursion_limit]`. Not built: the `CrateRoot` record has no field for
    /// it yet (design, "What the new-layer crates lack").
    pub fn recursion_limit(&self, db: &Db) -> u32 {
        let item = crate_def_map(db, self.krate).root.key(db).unwrap_or(ItemKey(blaze_id::ContentId([0; 32])));
        Err(SurfaceError::NotBuilt(NotBuilt { method: "DefMap::recursion_limit", item })).or_need()
    }
}
