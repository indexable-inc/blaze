//! One body's blocks read as names (D189 step 4): [`BodyWorld`] is the crate's finished names
//! with the body's `BodyScope` on top, one [`World`] every path walk reads (crate::names). A
//! block's module is answered from the body's record (its scope, its chain, its place); every
//! other module of the crate from the crate's finished scopes; a module of another crate is not
//! held, so a walk into one stops with `Why::NotRead`, as [`OwnNames`] does (never another
//! layer's answer). Under `BlockScopes::Modules` the same modules are the crate's own and
//! [`OwnNames`] alone answers them: the two worlds answer one question about one block, so a
//! differential compares them (super::impl_table).

#![blaze::sans_io]

use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::Alloc;
use blaze_surface::{CrateRootBuilder, ItemKind, ModuleInfo, Namespace, ScopeBuilder};

use super::BodyScope;
use crate::items_record::buckets::{HeadNames, OwnNames};
use crate::names::{self, PathError, Resolved, VariantHit, World, Written, variant_in};
use crate::resolve::{At, CrateNames};

/// The crate `krate`'s finished names with one body's blocks (`body`) on top.
#[derive(Clone, Copy)]
pub struct BodyWorld<'r, 'a> {
    pub krate: &'r CrateNames<'a>,
    pub body: &'r BodyScope<'a>,
}

impl<'r, 'a> BodyWorld<'r, 'a> {
    /// A module's place: the body's record's for a module its blocks made, else the crate's.
    fn info(&self, module: ItemKey) -> Option<ModuleInfo<&'a str>> {
        match self.body.solved.made.get(&module) {
            Some(m) => Some(m.info),
            None => self.krate.info(module),
        }
    }

    fn own(&self, key: ItemKey) -> bool {
        key.crate_prefix() == self.krate.key.prefix()
    }
}

impl<'r, 'a> World<&'a str> for BodyWorld<'r, 'a> {
    type Scope<'w>
        = &'w ScopeBuilder<'w, &'a str>
    where
        Self: 'w;
    type Root<'w>
        = &'w CrateRootBuilder<'w, &'a str>
    where
        Self: 'w;

    fn scope(&self, module: ItemKey) -> Option<&ScopeBuilder<'_, &'a str>> {
        match self.body.scope(module) {
            Some(s) => Some(s),
            None => self.krate.done.get(&module).map(|r| &r.scope),
        }
    }

    fn chain(&self, module: ItemKey) -> Option<&[ItemKey]> {
        match self.body.solved.made.get(&module) {
            Some(m) => Some(m.chain.as_slice()),
            None => self.krate.chains.get(&module).map(|c| c.as_slice()),
        }
    }

    fn root_of(&self, key: ItemKey) -> Option<&CrateRootBuilder<'_, &'a str>> {
        self.own(key).then_some(&self.krate.crate_root)
    }

    fn root(&self, krate: CrateItemKey) -> Option<&CrateRootBuilder<'_, &'a str>> {
        (krate == self.krate.key).then_some(&self.krate.crate_root)
    }

    fn variant(&self, owner: ItemKey, name: &'a str) -> Option<Option<VariantHit>> {
        let variants = match self.body.solved.tables.enums.get(&owner) {
            Some(v) => v,
            None => self.krate.tables.enums.get(&owner)?,
        };
        Some(variant_in(variants.as_slice(), name))
    }

    fn no_implicit_prelude(&self, module: ItemKey) -> bool {
        let Some(chain) = self.chain(module) else { return false };
        chain.iter().any(|m| self.info(*m).is_some_and(|info| info.no_implicit_prelude))
    }

    fn anonymous(&self, module: ItemKey) -> bool {
        self.info(module).is_some_and(|info| info.name.is_some_and(crate::blocks::is_block_module))
    }
}

impl HeadNames for BodyWorld<'_, '_> {
    fn type_path(&self, module: ItemKey, written: Written<'_, &str>, alloc: Alloc<'_>) -> Result<Resolved, PathError> {
        // The records are read as of the segments' shorter borrow (as `OwnNames` reads its).
        fn walk<'s>(world: BodyWorld<'s, 's>, module: ItemKey, written: Written<'_, &'s str>, alloc: Alloc<'_>) -> Result<Resolved, PathError> {
            names::path(&world, module, Namespace::Types, written, alloc)
        }
        walk(*self, module, written, alloc)
    }

    fn item_kind(&self, key: ItemKey) -> Option<ItemKind> {
        // An item the body's blocks declare: its kind is in the outline of the text it is in
        // (the crate's file, or an expansion the body's solve made).
        if let Some((it, at)) = self.body.solved.tables.texts.get(&key) {
            let outline = match *at {
                At::Expansion(call) => match self.body.solved.expansions.get(&call).and_then(|e| e.made) {
                    Some(made) => &made.outline,
                    None => self.krate.outline_at(*at)?.1,
                },
                At::File(_) => self.krate.outline_at(*at)?.1,
            };
            return Some(outline.items().get(it.item.0 as usize)?.detail.kind());
        }
        OwnNames(self.krate).item_kind(key)
    }
}
