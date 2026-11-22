//! A module's names at module grain (drive.md "Names at module grain", D116 points 1 and 4),
//! from what the cell core loaded and decided: its two tables of LINKS (blaze-surface
//! `LinksBuilder`) and its `Imports` (`ImportsBuilder`).
//!
//! - The OUTLINE: what the module's own text writes (fragment depth 0): each declared name as
//!   a definition by key (a `mod` by its identity, never its contents), each named `use` as its
//!   written path, each glob as its written path. Nothing here is resolved, so nothing waits.
//! - The MADE table (the `ModuleScope` of D116): the same links for what the module's macro calls
//!   wrote (fragment depth above 0); `None` for a module with no item-position call, which has no
//!   such task (rule 77).
//! - `Imports`: the module's own `use` items as the core resolved them (RESOLVED), its traits in
//!   scope (TRAITS: declared, imported, `as _`; a glob's by its source, read by a part read of
//!   the source's TRAITS), and the findings.
//!
//! One binding a cell, as the record writer keeps it (write.rs `written`): an item or `mod` of
//! the name stands, and an import beside it is the solve's `Duplicate`, not a row; of two imports
//! of one name each namespace's row is the import that binds it there (`Cells::row_import`).

use blaze_id::ItemKey;
use blaze_own::{ABTreeMap, ABTreeSet, AVec, Alloc};
use blaze_surface::{BuildError, DECLARED_VIS, ImportsBuilder, LinksBuilder, ScopeBuilder, Table, TextualDef};

use super::{Cells, FragId, ItemId, ModId, NAMESPACES, Outcome, ns_at};
use crate::cell_key::{cell_key, textual_cut_key};
use crate::{Def, DefKind, Namespace, Origin, PathKind};

/// One module's tables as builders, its names borrowed from the cell core or its written
/// scope (`'n`).
pub(crate) struct Tables<'n, 'o> {
    pub(crate) module: ItemKey,
    pub(crate) outline: LinksBuilder<'n, 'o>,
    /// `None`: no item-position macro call in the module, so no `ModuleScope`.
    pub(crate) made: Option<LinksBuilder<'n, 'o>>,
    pub(crate) imports: ImportsBuilder<'o>,
    /// The textual `macro_rules!` scope; `None` for a module whose sequence is empty.
    pub(crate) textual: Option<LinksBuilder<'n, 'o>>,
    /// The crate's `#[macro_export]` macros: `Some` at the crate root alone.
    pub(crate) exported: Option<LinksBuilder<'n, 'o>>,
}

/// A module's `Textual` table from its written scope: one row a name (the last definition of
/// it, at its place in the sequence), a mark row a `mod` child, and a cut row
/// ([`textual_cut_key`]) for each child and each name defined both before and after the child's
/// `mod` line: the last definition before it, which the child sees.
fn textual_table<'n, 'o, N: crate::Name>(scope: &'n ScopeBuilder<'_, N>, alloc: Alloc<'o>) -> Option<LinksBuilder<'n, 'o>> {
    if scope.textual.is_empty() {
        return None;
    }
    // Each name's places in the sequence, in order.
    let mut places: ABTreeMap<'o, [u8; 16], AVec<'o, usize>> = ABTreeMap::new_in(alloc);
    for (i, t) in scope.textual.iter().enumerate() {
        places.entry(cell_key(Namespace::Macros, t.name.as_ref()).0).or_insert_with(|| AVec::new_in(alloc)).push(i);
    }
    let def_of = |t: &TextualDef<N>| Def { key: t.def, kind: DefKind::Macro(t.shapes) };
    let mut b = LinksBuilder::new_in(scope.module, Table::Textual, alloc);
    for (&cell, at) in places.iter() {
        // Never empty: an entry is made with its first place.
        let (first, last) = (at[0], at[at.len() - 1]);
        let t = &scope.textual[last];
        b.textual(cell, t.name.as_ref(), def_of(t), t.expansion, last as u32, first as u32);
    }
    for m in scope.marks.iter() {
        b.mark(m.child, m.len);
        let len = m.len as usize;
        for at in places.values() {
            // Seen before the line: the places below `len`; a cut only where some are not.
            let seen = at.partition_point(|&i| i < len);
            if seen == 0 || seen == at.len() {
                continue;
            }
            let j = at[seen - 1];
            let t = &scope.textual[j];
            b.textual(textual_cut_key(m.child, t.name.as_ref()).0, t.name.as_ref(), def_of(t), t.expansion, j as u32, at[0] as u32);
        }
    }
    Some(b)
}

/// Why the tables of a module were not written: the module and the builder's refusal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TablesError {
    pub module: ItemKey,
    pub error: BuildError,
}

impl<'a, N: crate::Name> Cells<'a, N> {
    /// The namespaces a named import's written path can bind in.
    pub(super) fn import_namespaces(&self, j: ItemId) -> &'static [Namespace] {
        let it = &self.items[j as usize];
        if it.types_only {
            &[Namespace::Types]
        } else if it.of_enum {
            // A variant is never a macro.
            &[Namespace::Types, Namespace::Values]
        } else {
            &NAMESPACES
        }
    }

    /// Of `ids`, the imports of one name in one module (in index order), the one whose row holds
    /// the name's cell in `ns`: the import the core decided binds the name in `ns`, else the first
    /// not decided to lack it (the lookup follows its path), else the first. The Reference,
    /// "Use declarations" and "Namespaces": a `use` binds its name in each namespace its target
    /// has, and two names in different namespaces may overlap; two imports of one name bind one
    /// namespace only in error (E0252), so at most one is decided to bind it. Writing the first
    /// import's path in every namespace lost the second (core's `slice` `use crate::{range};` then
    /// `pub use index::{range};`; der's `pub use crate::ord::ValueOrd;` then `pub use
    /// der_derive::ValueOrd;`: the derive answered absent; tempo, 21 Nov 05:33).
    fn row_import(&self, ids: &[ItemId], ns: Namespace) -> Option<ItemId> {
        let can = || ids.iter().copied().filter(move |&j| self.import_namespaces(j).contains(&ns));
        let binds = |j: ItemId| match &self.items[j as usize].outcome {
            Some(Outcome::Named(b)) => Some(b[ns_at(ns)].is_some()),
            _ => None,
        };
        can().find(|&j| binds(j) == Some(true)).or_else(|| can().find(|&j| binds(j).is_none())).or_else(|| can().next())
    }

    /// The path kind a row holds for import `j`: as written, except in a block's module, where a
    /// written `self` or `super` passes over the blocks around it ([`Cells::up`]) and the row
    /// holds its steps up the module's own chain, which the lookup (links.rs `walk`) takes as
    /// they stand. The Reference, `paths.qualifiers.mod-self` ("`self` resolves the path relative
    /// to the current module") and `paths.qualifiers.super` ("`super` in a path resolves to the
    /// parent module"); a block is an anonymous scope, no module (tests/super_in_block.rs). The
    /// block's own lexical `use super::*;` keeps its one step ([`Cells::lexical_glob`]). Without
    /// it the lookup took `super` from a block as the body's module (h2's `use
    /// super::stream::ContentLength;` in `fn send_request`, tokio's `use super::block::Read;`,
    /// alloy's derive-written `const _: () = { use super::*; .. }`: absent; tempo, 21 Nov).
    fn table_kind(&self, j: ItemId) -> PathKind {
        let it = &self.items[j as usize];
        let n = match it.kind {
            PathKind::SelfMod => 0,
            PathKind::Super(n) => n,
            kind => return kind,
        };
        if !self.anonymous(it.module) || self.lexical_glob(j) {
            return it.kind;
        }
        let chain = &self.modules[it.module as usize].chain;
        // Past the root: a step count past the chain, which the lookup answers as nowhere.
        let past = PathKind::Super(u8::try_from(chain.len()).unwrap_or(u8::MAX));
        let Some(to) = self.up(it.module, n) else { return past };
        let to = self.modules[to as usize].key;
        match chain.iter().position(|&k| k == to) {
            Some(0) => PathKind::SelfMod,
            Some(steps) => u8::try_from(steps).map_or(past, PathKind::Super),
            // `up` walks parents, each on the chain: never.
            None => past,
        }
    }

    /// Each module of this crate whose text was read, in key order, lent to `visit` as its
    /// tables. The `as _` imports are found in one pass over the items; every other list is the
    /// module's own.
    ///
    /// The `Textual` table is read off the module's written scope (write.rs `scope`, the textual
    /// walk's one home), so each module's scope is written once more here.
    pub(crate) fn tables<'o>(
        &self,
        alloc: Alloc<'o>,
        visit: &mut dyn FnMut(Tables<'_, 'o>) -> Result<(), BuildError>,
    ) -> Result<(), TablesError> {
        let ix = self.write_index(self.alloc);
        let mut anon: AVec<'o, AVec<'o, ItemId>> = AVec::with_capacity_in(self.modules.len(), alloc);
        for _ in 0..self.modules.len() {
            anon.push(AVec::new_in(alloc));
        }
        for (j, it) in self.items.iter().enumerate() {
            if it.call.is_none() && !it.glob && it.binds.is_none() {
                anon[it.module as usize].push(j as ItemId);
            }
        }
        for (&key, &m) in self.by_key.iter() {
            let md = &self.modules[m as usize];
            if md.foreign || md.unread || md.text == FragId::MAX {
                continue;
            }
            let t = self.module_tables(m, &anon[m as usize], alloc).map_err(|error| TablesError { module: key, error })?;
            let scope = self.scope(&ix, key, alloc);
            let textual = scope.as_ref().and_then(|(s, _)| textual_table(s, alloc));
            let Tables { module, outline, made, imports, exported, .. } = t;
            visit(Tables { module, outline, made, imports, textual, exported }).map_err(|error| TablesError { module: key, error })?;
        }
        Ok(())
    }

    fn module_tables<'s, 'o>(&'s self, m: ModId, anon: &[ItemId], alloc: Alloc<'o>) -> Result<Tables<'s, 'o>, BuildError> {
        let md = &self.modules[m as usize];
        let mut outline = LinksBuilder::new_in(md.key, Table::Outline, alloc);
        let mut made = if md.calls.is_empty() { None } else { Some(LinksBuilder::new_in(md.key, Table::Made, alloc)) };
        let mut imports = ImportsBuilder::new_in(md.key, alloc);
        let mut exported = if m == self.root { Some(LinksBuilder::new_in(md.key, Table::Exported, alloc)) } else { None };
        // The cells given a row, in either table: one binding a cell.
        let mut taken: ABTreeSet<'o, [u8; 16]> = ABTreeSet::new_in(alloc);
        for (&(ns, ref name), &(def, vis, origin, frag)) in md.explicit.iter() {
            let written_name: N = *name;
            let name: &'s str = name.as_ref();
            let cell = cell_key(ns, name).0;
            taken.insert(cell);
            let depth = self.depth(frag);
            // A `#[macro_export]` of the crate is a binding of the root for other crates, at any
            // depth and from any module (one table a crate, D116 point 6).
            if origin == Origin::MacroExport
                && let Some(table) = exported.as_mut()
            {
                table.def(cell, ns, name, def, vis, origin, depth);
                continue;
            }
            let table = if depth == 0 { &mut outline } else { made.get_or_insert_with(|| LinksBuilder::new_in(md.key, Table::Made, alloc)) };
            // A `pub(in path)` read through the cells (it left the module chain) is written as its
            // path; its decided value is the `Imports` row of the cell (rule 21).
            if let Some(vp) = self.vis_paths.get(&(m, ns, written_name)) {
                table.def_written_vis(cell, ns, name, def, vp.kind, vp.segments.iter().map(|s| s.as_ref()), origin, depth);
                imports.resolved(cell, ns, def, vis, DECLARED_VIS);
            } else {
                table.def(cell, ns, name, def, vis, origin, depth);
            }
            if ns == Namespace::Types && def.kind == DefKind::Trait {
                imports.trait_in_scope(def.key);
            }
        }
        // The cells given an `Imports` row: the first import of a name by index is its row.
        let mut resolved: ABTreeSet<'o, [u8; 16]> = ABTreeSet::new_in(alloc);
        for (bound, ids) in md.named.iter() {
            let name: &'s str = bound.as_ref();
            for &j in ids.iter() {
                let it = &self.items[j as usize];
                let depth = self.depth(it.at);
                for &ns in self.import_namespaces(j) {
                    // Each namespace's row is its own import's ([`Cells::row_import`]).
                    if self.row_import(ids, ns) != Some(j) {
                        continue;
                    }
                    let cell = cell_key(ns, name).0;
                    if !taken.insert(cell) {
                        continue;
                    }
                    let table = if depth == 0 { &mut outline } else { made.get_or_insert_with(|| LinksBuilder::new_in(md.key, Table::Made, alloc)) };
                    table.path(cell, ns, name, self.table_kind(j), it.segments.iter().map(|s| s.as_ref()), it.vis, Origin::Import, depth);
                }
                match it.outcome {
                    Some(Outcome::Named(b)) => {
                        for ns in NAMESPACES {
                            let Some((def, vis)) = b[ns_at(ns)] else { continue };
                            let cell = cell_key(ns, name).0;
                            if md.explicit.contains_key(&(ns, *bound)) || !resolved.insert(cell) {
                                continue;
                            }
                            imports.resolved(cell, ns, def, vis, it.index);
                            if ns == Namespace::Types && def.kind == DefKind::Trait {
                                imports.trait_in_scope(def.key);
                            }
                        }
                    }
                    Some(Outcome::Failed(why)) => imports.finding(it.index, it.seg, why),
                    Some(Outcome::Outside) | None => imports.undecided(),
                    Some(Outcome::Glob(_) | Outcome::Called(_) | Outcome::Inert) => {}
                }
            }
        }
        for &j in md.glob_items.iter() {
            let it = &self.items[j as usize];
            let depth = self.depth(it.at);
            let table = if depth == 0 { &mut outline } else { made.get_or_insert_with(|| LinksBuilder::new_in(md.key, Table::Made, alloc)) };
            table.glob_path(self.table_kind(j), it.segments.iter().map(|s| s.as_ref()), it.vis, depth);
            match (md.edges.get(&it.index), it.outcome) {
                (Some(&(source, _)), _) => imports.glob_source(self.modules[source as usize].key),
                (None, Some(Outcome::Failed(why))) => imports.finding(it.index, it.seg, why),
                (None, _) => imports.undecided(),
            }
        }
        for &j in anon.iter() {
            let it = &self.items[j as usize];
            match it.outcome {
                Some(Outcome::Named(b)) => {
                    if let Some((def, _)) = b[ns_at(Namespace::Types)]
                        && def.kind == DefKind::Trait
                    {
                        imports.trait_in_scope(def.key);
                    }
                }
                Some(Outcome::Failed(why)) => imports.finding(it.index, it.seg, why),
                Some(Outcome::Outside) | None => imports.undecided(),
                Some(Outcome::Glob(_) | Outcome::Called(_) | Outcome::Inert) => {}
            }
        }
        Ok(Tables { module: md.key, outline, made, imports, textual: None, exported })
    }
}
