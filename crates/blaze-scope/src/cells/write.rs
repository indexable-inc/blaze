//! What the cell core decided, in the scope record's own forms (blaze-surface's `ScopeBuilder`):
//! the writer of the switch, where the cell core fills the scopes the one record writer reads.
//!
//! A module's own bindings are its explicit entries (items, `mod`s, `extern crate` items, the
//! root's `#[macro_export]` macros of the walked tree's texts) and what its named imports bind;
//! its globs are edges, one per source. Sorted as the record is: bindings by namespace then
//! name, edges by source key. The rest of the scope is one walk of the module's fragments in
//! tree order (its text, each call's expansion where the call is written), as the old core's
//! final pass walks them (finish.rs `finish_frag`, `flatten`, `anon_and_exports`): the errors,
//! the textual list with each child's mark, the `use .. as _` traits, and the `#[macro_export]`
//! macros an expansion wrote (published to the root by the record writer, as before).
//!
//! A visibility naming no ancestor is its module's `RestrictedNotAncestor` errors, first, in a
//! fixed order (vis.rs `vis_errors`).
//!
//! Not written (counted by the seam's whole-scope comparison, c10-r17a): `Inconsistent`, a second item of one name (`Duplicate` of explicit entries:
//! see `add_explicit`). R6's errors (cells/early.rs) are written at their use or call, R8's at
//! the expansion that wrote the `extern crate`, as `Ambiguous`.

use blaze_id::ItemKey;
use blaze_own::{ABTreeMap, AVec, Alloc};
use blaze_surface::{AnonImport, Binding, GlobEdge, Path, ScopeBuilder, ScopeEntry, ScopeError, Site, TextualDef, TextualMark, Unresolved, UseSite};

use super::{CellPlant, Cells, FragId, ItemId, NAMESPACES, Outcome, Slot, ns_at};
use crate::{AmbiguityKind, BindingState, Def, DefKind, Namespace, Origin};

/// One module's bindings and glob edges as the cell core decided them.
pub(crate) struct Written<'a, N> {
    pub(crate) bindings: AVec<'a, ScopeEntry<N>>,
    pub(crate) globs: AVec<'a, GlobEdge>,
    /// The named imports and globs of the module the core holds as outside what it follows
    /// (`Outcome::Outside`) or left undecided: with one, the lists above are not the answer.
    pub(crate) unknown: u32,
}

/// What the writer's walk needs of every fragment, made once per crate after the run.
pub(crate) struct WriteIndex<'a, N> {
    /// Each fragment's imports, in the order its declarations list them (a `UseSite`'s index).
    uses: AVec<'a, AVec<'a, ItemId>>,
    /// The call each fragment is the expansion of (`None`: a module's own text).
    call: AVec<'a, Option<ItemKey>>,
    /// Each fragment's explicit entries (no `#[macro_export]`): the non-glob bindings a later
    /// import of the same name is a `Duplicate` of.
    explicit: AVec<'a, AVec<'a, (Namespace, N, Def)>>,
}

impl<'a, N: crate::Name> Cells<'a, N> {
    /// How deep in macro expansions fragment `f` is written: 0 for a module's own text.
    pub(super) fn depth(&self, f: FragId) -> u16 {
        let mut depth = 0u16;
        let mut at = self.frags[f as usize].up;
        while let Some((up, _)) = at {
            depth = depth.saturating_add(1);
            at = self.frags[up as usize].up;
        }
        depth
    }

    /// A `#[macro_export]` macro of the root that the record writer publishes after the solve
    /// (task/exports.rs): one an expansion wrote, or one in the text of a module an expansion
    /// or a block made. The root's own scope is written without it, as the old core's is.
    pub(super) fn published_later(&self, origin: Origin, frag: FragId) -> bool {
        origin == Origin::MacroExport && (self.depth(frag) > 0 || self.frags[frag as usize].module >= self.walked)
    }

    /// The bindings and glob edges of `module` after the run; `None` for a key that is no
    /// module of this crate.
    pub(crate) fn written<'o>(&self, module: ItemKey, alloc: Alloc<'o>) -> Option<Written<'o, N>> {
        let &m = self.by_key.get(&module)?;
        let md = &self.modules[m as usize];
        if md.foreign || md.unread {
            return None;
        }
        let mut unknown = 0u32;
        let mut bindings: AVec<'o, ScopeEntry<N>> = AVec::with_capacity_in(md.explicit.len() + md.named.len(), alloc);
        for (&(ns, name), &(def, vis, origin, frag)) in md.explicit.iter() {
            if self.published_later(origin, frag) {
                continue;
            }
            // A `pub(in path)` read through the cells: where it ended (vis.rs); a cell on its
            // path unknown leaves the scope not the answer.
            let vis = match self.entry_vis_final(m, ns, name, vis) {
                Some(v) => v,
                None => {
                    unknown += 1;
                    vis
                }
            };
            let binding = Binding { def, vis, origin, expansion: self.depth(frag), state: BindingState::Determined };
            bindings.push(ScopeEntry { ns, name, binding });
        }
        for (&name, imports) in md.named.iter() {
            let mut bound: [Option<Binding>; 3] = [None; 3];
            for &j in imports.iter() {
                let it = &self.items[j as usize];
                match it.outcome {
                    Some(Outcome::Named(b)) => {
                        for ns in NAMESPACES {
                            let at = ns_at(ns);
                            if let (None, Some((def, vis))) = (bound[at], b[at]) {
                                bound[at] = Some(Binding { def, vis, origin: Origin::Import, expansion: self.depth(it.at), state: BindingState::Determined });
                            }
                        }
                    }
                    Some(Outcome::Outside) | None => unknown += 1,
                    Some(Outcome::Failed(_) | Outcome::Glob(_) | Outcome::Called(_) | Outcome::Inert) => {}
                }
            }
            for ns in NAMESPACES {
                // An item or `mod` of the name stands; the import beside it is the old core's
                // `Duplicate` error, not a binding.
                if let Some(binding) = bound[ns_at(ns)]
                    && !md.explicit.contains_key(&(ns, name))
                {
                    bindings.push(ScopeEntry { ns, name, binding });
                }
            }
        }
        bindings.sort_unstable_by(|a, b| (a.ns, a.name).cmp(&(b.ns, b.name)));
        let mut globs: AVec<'o, GlobEdge> = AVec::with_capacity_in(md.edges.len(), alloc);
        for &j in md.glob_items.iter() {
            let it = &self.items[j as usize];
            let Some(&(source, vis)) = md.edges.get(&it.index) else {
                if !matches!(it.outcome, Some(Outcome::Failed(_))) {
                    unknown += 1;
                }
                continue;
            };
            // An enum's edge names the enum (its variants are the names it brings).
            let kind = if self.enum_mods.contains(&source) { DefKind::Enum } else { DefKind::Module };
            let source = self.modules[source as usize].key;
            let expansion = self.depth(it.at);
            match globs.iter_mut().find(|e| e.source.key == source) {
                Some(e) => {
                    e.vis = self.wider(e.vis, vis, m);
                    e.expansion = e.expansion.min(expansion);
                }
                None => globs.push(GlobEdge { source: Def { key: source, kind }, vis, expansion }),
            }
        }
        globs.sort_unstable_by_key(|e| e.source.key);
        Some(Written { bindings, globs, unknown })
    }

    /// The writer's index over the fragments read so far (made after the run, once).
    pub(crate) fn write_index(&self, alloc: Alloc<'a>) -> WriteIndex<'a, N> {
        let n = self.frags.len();
        let mut ix = WriteIndex { uses: AVec::with_capacity_in(n, alloc), call: AVec::with_capacity_in(n, alloc), explicit: AVec::with_capacity_in(n, alloc) };
        for _ in 0..n {
            ix.uses.push(AVec::new_in(alloc));
            ix.call.push(None);
            ix.explicit.push(AVec::new_in(alloc));
        }
        for (i, it) in self.items.iter().enumerate() {
            match (it.call, it.expansion) {
                (None, _) => ix.uses[it.at as usize].push(i as ItemId),
                (Some(at), Some(f)) => ix.call[f as usize] = Some(at.key),
                (Some(_), None) => {}
            }
        }
        // A module answered from its record has no fragment here (its scope is the record's).
        for md in self.modules.iter().filter(|md| md.answered == super::Answered::Text) {
            for (&(ns, name), &(def, _, origin, frag)) in md.explicit.iter() {
                if origin != Origin::MacroExport {
                    ix.explicit[frag as usize].push((ns, name, def));
                }
            }
        }
        ix
    }

    /// The whole scope of `module` as the record holds it, and how many of its imports and
    /// calls the core left outside what it follows or undecided (with one, the scope is not
    /// the answer); `None` for a key that is no module of this crate.
    pub(crate) fn scope<'o>(&self, ix: &WriteIndex<'_, N>, module: ItemKey, alloc: Alloc<'o>) -> Option<(ScopeBuilder<'o, N>, u32)> {
        let w = self.written(module, alloc)?;
        let &m = self.by_key.get(&module)?;
        let mut out = ScopeBuilder::new_in(module, alloc);
        out.bindings = w.bindings;
        out.globs = w.globs;
        let mut unknown = w.unknown;
        // The non-glob bindings met so far in tree order: a second target is `Duplicate`.
        let mut seen: ABTreeMap<'o, (Namespace, N), Def> = ABTreeMap::new_in(alloc);
        let mut bind = |ns: Namespace, name: N, def: Def, errors: &mut AVec<'o, ScopeError<N, Path<'o, N>>>| {
            let first = *seen.entry((ns, name)).or_insert(def);
            if first != def {
                errors.push(ScopeError::Duplicate { ns, name, first, second: def });
            }
        };
        let path = |i: ItemId| {
            let it = &self.items[i as usize];
            Path { kind: it.kind, segments: AVec::from_slice_in(&it.segments, alloc) }
        };
        for &(k, name) in self.bad_extern.iter() {
            if k == module {
                out.errors.push(ScopeError::ExternCrate { name });
            }
        }
        self.vis_errors(m, &ix.call, alloc, &mut out.errors);
        // The walk: (fragment, next slot, next import, the module's own fragment or a
        // `#[macro_use]` child's carried out). Slots and imports merge by text position.
        let mut stack: AVec<'o, (FragId, usize, usize, bool)> = AVec::new_in(alloc);
        // The fragment each `macro_exports` entry was written in (the plant's order only).
        let mut export_frags: AVec<'o, FragId> = AVec::new_in(alloc);
        let text = self.modules[m as usize].text;
        stack.push((text, 0, 0, true));
        for &(ns, name, def) in ix.explicit[text as usize].iter() {
            bind(ns, name, def, &mut out.errors);
        }
        while let Some(&(f, si, ui, own)) = stack.last() {
            let frag = &self.frags[f as usize];
            let next_slot = frag.slots.get(si).copied();
            let next_use = if own { ix.uses[f as usize].get(ui).copied() } else { None };
            let take_use = match (next_slot, next_use) {
                (None, None) => {
                    stack.pop();
                    continue;
                }
                (Some((pos, _)), Some(i)) => self.items[i as usize].pos < pos,
                (None, Some(_)) => true,
                (Some(_), None) => false,
            };
            let top = stack.len() - 1;
            if take_use {
                stack[top].2 += 1;
                let Some(i) = next_use else { continue };
                let it = &self.items[i as usize];
                let site = UseSite { expansion: ix.call[f as usize], index: ui as u32 };
                // R6: the use's first segment took one candidate and an expansion not enclosing
                // it wrote another. Its error stands in for a later segment's failure: that path
                // read an ambiguous first segment, so the failure says nothing (r6e, D56).
                let early = self.r6.get(&i).copied();
                if let Some(e) = early {
                    out.errors.push(e.error(Site::Import(site), path(i)));
                }
                match it.outcome {
                    Some(Outcome::Failed(_)) if early.is_some() => {}
                    Some(Outcome::Failed(why)) => out.errors.push(ScopeError::Import { site, path: path(i), segment: it.seg, why }),
                    Some(Outcome::Named(b)) if b.iter().all(Option::is_none) => {
                        let last = it.segments.len().saturating_sub(1) as u32;
                        out.errors.push(ScopeError::Import { site, path: path(i), segment: last, why: Unresolved::NotFound });
                    }
                    Some(Outcome::Named(b)) => {
                        // E0364/E0365 (the Reference, `items.use.visibility`): a `use` re-exports
                        // a binding at most as visible as the binding is. The outcome holds each
                        // namespace's binding as the import makes it, its visibility the
                        // narrower of the import's and the target's (an `extern crate` item's
                        // under a `pub use` excepted, as rustc's `Resolver::import` does): one
                        // narrower than the import's own is the error, beside the binding.
                        let leaf = it.segments.last().copied().or(it.binds);
                        for ns in NAMESPACES {
                            let Some((def, vis)) = b[ns_at(ns)] else { continue };
                            if let Some(name) = it.binds {
                                bind(ns, name, def, &mut out.errors);
                            }
                            if vis != it.vis
                                && self.plant != CellPlant::NoWiderReexport
                                && let Some(name) = leaf
                            {
                                out.errors.push(ScopeError::WiderReexport { site, ns, name, def });
                            }
                        }
                        if it.binds.is_none()
                            && let Some((def, _)) = b[ns_at(Namespace::Types)]
                            && def.kind == DefKind::Trait
                        {
                            out.traits.push(AnonImport { site, def, vis: it.vis });
                        }
                    }
                    // `written` counted the named imports and globs; an `as _` import here.
                    Some(Outcome::Outside) | None if it.binds.is_none() && !it.glob => unknown += 1,
                    Some(Outcome::Outside | Outcome::Glob(_) | Outcome::Called(_) | Outcome::Inert) | None => {}
                }
                // GlobVsExpanded at the use's first segment (`Cells::validate_expanded`).
                for ns in NAMESPACES {
                    if let Some(&(name, first, second)) = self.glob_vs_expanded.get(&(i, ns)) {
                        out.errors.push(ScopeError::Ambiguous { site: Site::Import(site), ns, name, kind: AmbiguityKind::GlobVsExpanded, first, second });
                    }
                }
                continue;
            }
            stack[top].1 += 1;
            let Some((_, slot)) = next_slot else { continue };
            match slot {
                Slot::Rules(name, def, export) => {
                    let DefKind::Macro(shapes) = def.kind else { continue };
                    let d = TextualDef { name, def: def.key, expansion: self.depth(f), shapes };
                    out.textual.push(d);
                    if own && export && d.expansion > 0 {
                        out.macro_exports.push(d);
                        export_frags.push(f);
                    }
                }
                Slot::Mod(child, macro_use) => {
                    let c = &self.modules[child as usize];
                    if own {
                        out.marks.push(TextualMark { child: c.key, len: out.textual.len() as u32 });
                    }
                    if macro_use && !c.foreign && !c.unread && c.text != FragId::MAX {
                        stack.push((c.text, 0, 0, false));
                    } else if macro_use && !c.foreign && !c.unread {
                        // A `#[macro_use]` child outside a one-group solve's members: its
                        // flattened sequence as its record holds it; none held is unknown here.
                        match self.recorded_textual.get(&child) {
                            Some(r) => out.textual.extend(r.defs.iter().copied()),
                            None => unknown += 1,
                        }
                    }
                }
                Slot::Call(c) => {
                    let it = &self.items[c as usize];
                    if own && it.deep {
                        if let Some(at) = it.call {
                            out.errors.push(ScopeError::RecursionLimit { call: at.key });
                        }
                    } else if own {
                        // R6 for a macro call's first segment.
                        if let (Some(e), Some(at)) = (self.r6.get(&c), it.call) {
                            out.errors.push(e.error(Site::Call(at.key), path(c)));
                        }
                        match it.outcome {
                            Some(Outcome::Failed(why)) => {
                                if let Some(at) = it.call {
                                    out.errors.push(ScopeError::Macro { call: at.key, path: path(c), segment: it.seg, why });
                                }
                            }
                            Some(Outcome::Outside) | None => unknown += 1,
                            Some(Outcome::Called(_) | Outcome::Named(_) | Outcome::Glob(_) | Outcome::Inert) => {}
                        }
                        // GlobVsExpanded at the call path's first segment.
                        if let Some(at) = it.call {
                            for ns in NAMESPACES {
                                if let Some(&(name, first, second)) = self.glob_vs_expanded.get(&(c, ns)) {
                                    out.errors.push(ScopeError::Ambiguous { site: Site::Call(at.key), ns, name, kind: AmbiguityKind::GlobVsExpanded, first, second });
                                }
                            }
                        }
                    }
                    if let Some(e) = it.expansion {
                        stack.push((e, 0, 0, own));
                        if own {
                            for &(ns, name, def) in ix.explicit[e as usize].iter() {
                                bind(ns, name, def, &mut out.errors);
                            }
                            // R8: a root `extern crate .. as n;` this expansion wrote beside a
                            // dependency's `n`, at the call that wrote it.
                            if let Some(at) = it.call {
                                for &(_, r8) in self.r8.iter().filter(|r| r.0 == e) {
                                    out.errors.push(r8.error(Site::Call(at.key), path(c)));
                                }
                            }
                        }
                    }
                }
            }
        }
        out.marks.sort_unstable_by_key(|k| k.child);
        // The `#[macro_export]` macros expansions wrote are listed in the walk's tree order: where
        // each is written in the module's text with every call's expansion at the call, a
        // function of the text alone (rule 25). Fragment ids are given as expansions are read,
        // so the plant's order by fragment is the order the work happened to finish in.
        if self.plant == CellPlant::ExportsByExpansionTime {
            let mut by_frag: AVec<'o, (FragId, TextualDef<N>)> = AVec::with_capacity_in(export_frags.len(), alloc);
            for (k, &f) in export_frags.iter().enumerate() {
                by_frag.push((f, out.macro_exports[k]));
            }
            by_frag.sort_by_key(|&(f, _)| f);
            for (k, &(_, d)) in by_frag.iter().enumerate() {
                out.macro_exports[k] = d;
            }
        }
        Some((out, unknown))
    }

    /// Which kind of unresolved item caused `unknown > 0` for `module` (rule 19: name the cause).
    pub(crate) fn first_unknown(&self, ix: &WriteIndex<'_, N>, module: ItemKey) -> &'static str {
        let Some(&m) = self.by_key.get(&module) else { return "no_module" };
        let Some(md) = usize::try_from(m).ok().and_then(|i| self.modules.get(i)) else { return "no_module" };
        for (&(ns, name), &(_, vis, origin, frag)) in md.explicit.iter() {
            if !self.published_later(origin, frag) && self.entry_vis_final(m, ns, name, vis).is_none() {
                return "entry_vis_final";
            }
        }
        for imports in md.named.values() {
            for &j in imports.iter() {
                if let Some(it) = usize::try_from(j).ok().and_then(|i| self.items.get(i)) {
                    match it.outcome {
                        Some(Outcome::Outside) => return "named_outside",
                        None => return "named_none",
                        _ => {}
                    }
                }
            }
        }
        for &j in md.glob_items.iter() {
            if let Some(it) = usize::try_from(j).ok().and_then(|i| self.items.get(i))
                && !md.edges.contains_key(&it.index)
                && !matches!(it.outcome, Some(Outcome::Failed(_)))
            {
                return match it.outcome {
                    Some(Outcome::Outside) => "glob_outside",
                    None => "glob_none",
                    _ => "glob_other",
                };
            }
        }
        let mut stack: AVec<'_, (FragId, usize, usize, bool)> = AVec::new_in(self.alloc);
        stack.push((md.text, 0, 0, true));
        while let Some(&(f, si, ui, own)) = stack.last() {
            let Some(fi) = usize::try_from(f).ok() else { stack.pop(); continue };
            let Some(frag) = self.frags.get(fi) else { stack.pop(); continue };
            let next_slot = frag.slots.get(si).copied();
            let next_use = if own { ix.uses.get(fi).and_then(|u| u.get(ui).copied()) } else { None };
            let take_use = match (next_slot, next_use) {
                (None, None) => { stack.pop(); continue }
                (Some((pos, _)), Some(i)) => usize::try_from(i).ok().and_then(|k| self.items.get(k)).is_some_and(|it| it.pos < pos),
                (None, Some(_)) => true,
                (Some(_), None) => false,
            };
            let top = stack.len().saturating_sub(1);
            if take_use {
                if let Some(slot) = stack.get_mut(top) { slot.2 += 1; }
                if let Some(i) = next_use
                    && let Some(it) = usize::try_from(i).ok().and_then(|k| self.items.get(k))
                    && it.binds.is_none()
                    && !it.glob
                {
                    match it.outcome {
                        Some(Outcome::Outside) => return "anon_outside",
                        None => return "anon_none",
                        _ => {}
                    }
                }
                continue;
            }
            if let Some(slot) = stack.get_mut(top) { slot.1 += 1; }
            let Some((_, slot)) = next_slot else { continue };
            match slot {
                Slot::Rules(..) => {}
                Slot::Mod(child, macro_use) => {
                    if let Some(c) = usize::try_from(child).ok().and_then(|k| self.modules.get(k)) {
                        if macro_use && !c.foreign && !c.unread && c.text != FragId::MAX {
                            stack.push((c.text, 0, 0, false));
                        } else if macro_use && !c.foreign && !c.unread && !self.recorded_textual.contains_key(&child) {
                            return "macro_use_textual";
                        }
                    }
                }
                Slot::Call(c) => {
                    if let Some(it) = usize::try_from(c).ok().and_then(|k| self.items.get(k)) {
                        if own && !it.deep {
                            match it.outcome {
                                Some(Outcome::Outside) => return "call_outside",
                                None => return "call_none",
                                _ => {}
                            }
                        }
                        if let Some(e) = it.expansion {
                            stack.push((e, 0, 0, own));
                        }
                    }
                }
            }
        }
        "other"
    }
}

/// The namespaces in the record's order are the order [`NAMESPACES`] lists them in.
const _: () = assert!(matches!(NAMESPACES, [Namespace::Types, Namespace::Values, Namespace::Macros]));
