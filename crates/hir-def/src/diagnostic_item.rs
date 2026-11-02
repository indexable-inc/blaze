//! Diagnostic items: the items `#[rustc_diagnostic_item = ".."]` names, by name, as rustc's
//! `diagnostic_items` (one crate) and `all_diagnostic_items` (a crate and those it depends on)
//! collect them. The name one item has is [`AttrFlags::diagnostic_item`].

use base_db::{Crate, Db};
use blaze_hash::{FixedState, HashMap};
use blaze_mem::frame::Frame;
use hashbrown::hash_map::Entry;
use intern::Symbol;

use crate::{
    AssocItemId, AttrDefId, HasModule, ModuleDefId, ModuleId,
    attrs::AttrFlags,
    item_scope::ItemScope,
    nameres::{
        assoc::{ImplItems, TraitItems},
        crate_def_map, crate_local_def_map,
        demand::{self, Look},
    },
};

#[cfg(test)]
mod tests;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DiagnosticItems {
    /// The item each diagnostic item names.
    pub by_name: HashMap<Symbol, AttrDefId>,
    /// Each name given to a second item, which rustc refuses ("duplicate diagnostic item"):
    /// the name, the item `by_name` keeps for it, and the other. In a crate's own items, the
    /// first in the def map's order is kept; across crates, the crate asked about, then its
    /// dependencies nearest first.
    pub duplicates: Vec<(Symbol, AttrDefId, AttrDefId)>,
}

impl DiagnosticItems {
    #[inline]
    pub fn get(&self, name: &Symbol) -> Option<AttrDefId> {
        self.by_name.get(name).copied()
    }
}

type FrameMap<'f> = hashbrown::HashMap<Symbol, AttrDefId, FixedState, &'f Frame>;
type Duplicates<'f> = Vec<(Symbol, AttrDefId, AttrDefId), &'f Frame>;

/// Gives `name` to `owner` unless an item has it already; a second item is a duplicate.
fn insert(
    by_name: &mut FrameMap<'_>,
    duplicates: &mut Duplicates<'_>,
    name: Symbol,
    owner: AttrDefId,
) {
    match by_name.entry(name) {
        Entry::Vacant(it) => {
            it.insert(owner);
        }
        Entry::Occupied(it) => {
            if *it.get() != owner {
                duplicates.push((*it.key(), *it.get(), owner));
            }
        }
    }
}

/// The memo's value, made once at its size from what a run gathered in its frame.
fn settle(mut by_name: FrameMap<'_>, mut duplicates: Duplicates<'_>) -> DiagnosticItems {
    // alloc: the memo's map, exact
    let mut result = HashMap::with_capacity_and_hasher(by_name.len(), FixedState);
    result.extend(by_name.drain());
    // alloc: the memo's duplicates, exact
    DiagnosticItems { by_name: result, duplicates: duplicates.drain(..).collect() }
}

/// The diagnostic items of `krate`'s own items: every item rustc's `diagnostic_items` visits
/// (the crate's HIR owners: its modules, their items, the items of traits, impls and `extern`
/// blocks, macros, `use`s and `extern crate`s, and those declared in bodies). Lowers only the
/// bodies that may declare one ([`demand::walk_bodies_saying`]): those whose text says
/// `rustc_diagnostic_item` or names a macro that may write it.
#[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
pub fn crate_diagnostic_items(db: &Db, krate: Crate) -> DiagnosticItems {
    let _p = tracing::info_span!("crate_diagnostic_items_query").entered();
    blaze_mem::frame::current(|frame| {
        let mut collect = Collect {
            db,
            by_name: hashbrown::HashMap::with_hasher_in(FixedState, frame),
            duplicates: Vec::new_in(frame),
        };
        for (module, data) in crate_def_map(db, krate).modules(db) {
            collect.module(module, data.scope(db));
        }
        demand::walk_bodies_saying(db, krate, Look::DiagnosticItems, |_, blocks| {
            for &(_, def_map) in &blocks.blocks {
                for (module, data) in def_map.modules(db) {
                    collect.module(module, data.scope(db));
                }
            }
        });
        settle(collect.by_name, collect.duplicates)
    })
}

/// The diagnostic items `krate` sees, rustc's `all_diagnostic_items`: its own, then those of the
/// crates in its extern prelude and, in turn, theirs, breadth first. A name given in two crates
/// keeps the first; `duplicates` lists `krate`'s own and those across crates.
#[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
pub fn diagnostic_items(db: &Db, krate: Crate) -> DiagnosticItems {
    let _p = tracing::info_span!("diagnostic_items_query").entered();
    blaze_mem::frame::current(|frame| {
        let own = crate_diagnostic_items(db, krate);
        // `CrateData::dependencies` lists the sysroot crates even for `#![no_std]` and
        // `#![no_core]` crates, which rustc does not load; the extern prelude is what name
        // resolution settled on.
        let mut seen = hashbrown::HashSet::with_hasher_in(FixedState, frame);
        seen.insert(krate);
        let mut queue = Vec::new_in(frame);
        queue.push(krate);
        let mut at = 0;
        while let Some(&from) = queue.get(at) {
            at += 1;
            for (_, (module, _)) in crate_local_def_map(db, from).local(db).extern_prelude() {
                let dep = module.krate(db);
                if seen.insert(dep) {
                    queue.push(dep);
                }
            }
        }
        let mut by_name = hashbrown::HashMap::with_hasher_in(FixedState, frame);
        by_name.extend(own.by_name.iter().map(|(&name, &owner)| (name, owner)));
        let mut duplicates = Vec::new_in(frame);
        duplicates.extend(own.duplicates.iter().copied());
        for &dep in &queue[1..] {
            for (&name, &owner) in &crate_diagnostic_items(db, dep).by_name {
                insert(&mut by_name, &mut duplicates, name, owner);
            }
        }
        settle(by_name, duplicates)
    })
}

struct Collect<'db, 'f> {
    db: &'db Db,
    by_name: FrameMap<'f>,
    duplicates: Duplicates<'f>,
}

impl Collect<'_, '_> {
    fn item(&mut self, owner: impl Into<AttrDefId>) {
        let owner = owner.into();
        if let Some(name) = AttrFlags::diagnostic_item(self.db, owner) {
            insert(&mut self.by_name, &mut self.duplicates, name, owner);
        }
    }

    fn assoc(&mut self, items: &[(hir_expand::name::Name, AssocItemId)]) {
        for &(_, item) in items {
            match item {
                AssocItemId::FunctionId(it) => self.item(it),
                AssocItemId::ConstId(it) => self.item(it),
                AssocItemId::TypeAliasId(it) => self.item(it),
            }
        }
    }

    /// `module` itself (unless it is a block's, which is no item) and what it declares. A
    /// `macro_rules!` is declared in its module's textual scope, which its child modules copy, so
    /// it is taken from the module it names as its own.
    fn module(&mut self, module: ModuleId, scope: &ItemScope) {
        let db = self.db;
        if !module.is_block_module(db) {
            self.item(module);
        }
        for def in scope.declarations() {
            match def {
                ModuleDefId::FunctionId(it) => self.item(it),
                ModuleDefId::AdtId(it) => self.item(it),
                ModuleDefId::ConstId(it) => self.item(it),
                ModuleDefId::StaticId(it) => self.item(it),
                ModuleDefId::TraitId(it) => {
                    self.item(it);
                    self.assoc(&TraitItems::query(db, it).items);
                }
                ModuleDefId::TypeAliasId(it) => self.item(it),
                ModuleDefId::MacroId(it) => self.item(it),
                // A module is visited as one of its def map's; a variant is no HIR owner, so
                // rustc's walk does not reach it.
                ModuleDefId::ModuleId(_)
                | ModuleDefId::EnumVariantId(_)
                | ModuleDefId::BuiltinType(_) => {}
            }
        }
        for it in scope.unnamed_consts() {
            self.item(it);
        }
        for it in scope.impls() {
            self.item(it);
            self.assoc(&ImplItems::of(db, it).0.items);
        }
        for it in scope.extern_blocks() {
            self.item(AttrDefId::ExternBlockId(it));
        }
        for it in scope.extern_crate_decls() {
            self.item(it);
        }
        for it in scope.use_decls() {
            self.item(it);
        }
        for (_, macros) in scope.legacy_macros() {
            for &it in macros {
                if it.module(db) == module {
                    self.item(it);
                }
            }
        }
    }
}
