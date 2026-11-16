//! What the cell core decided, in the scope record's own forms (blaze-surface's `ScopeEntry` and
//! `GlobEdge`): the first part of the switch, where the cell core fills the scopes the one record
//! writer reads. A module's own bindings are its explicit entries (items, `mod`s, `extern crate`
//! items, the root's `#[macro_export]` macros) and what its named imports bind; its globs are
//! edges, one per source. Sorted as the record is: bindings by namespace then name, edges by
//! source key. Not written here yet: the errors, the textual list and its marks, the `use .. as
//! _` traits and the expansions' macro exports (the seam counts the modules equal without them).

use blaze_id::ItemKey;
use blaze_own::{AVec, Alloc};
use blaze_surface::{Binding, GlobEdge, ScopeEntry};

use super::{Cells, FragId, NAMESPACES, Outcome, ns_at};
use crate::{BindingState, Def, DefKind, Namespace, Origin};

/// One module's bindings and glob edges as the cell core decided them.
pub(crate) struct Written<'a, N> {
    pub(crate) bindings: AVec<'a, ScopeEntry<N>>,
    pub(crate) globs: AVec<'a, GlobEdge>,
    /// The named imports and globs of the module the core holds as outside what it follows
    /// (`Outcome::Outside`) or left undecided: with one, the lists above are not the answer.
    pub(crate) unknown: u32,
}

impl<'a, N: crate::Name> Cells<'a, N> {
    /// How deep in macro expansions fragment `f` is written: 0 for a module's own text.
    fn depth(&self, f: FragId) -> u16 {
        let mut depth = 0u16;
        let mut at = self.frags[f as usize].up;
        while let Some((up, _)) = at {
            depth = depth.saturating_add(1);
            at = self.frags[up as usize].up;
        }
        depth
    }

    /// The bindings and glob edges of `module` after the run; `None` for a key that is no
    /// module of this crate.
    pub(crate) fn written(&self, module: ItemKey, alloc: Alloc<'a>) -> Option<Written<'a, N>> {
        let &m = self.by_key.get(&module)?;
        let md = &self.modules[m as usize];
        if md.foreign || md.unread {
            return None;
        }
        let mut unknown = 0u32;
        let mut bindings: AVec<'a, ScopeEntry<N>> = AVec::with_capacity_in(md.explicit.len() + md.named.len(), alloc);
        for (&(ns, name), &(def, vis, origin, frag)) in md.explicit.iter() {
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
                    Some(Outcome::Failed(_) | Outcome::Glob(_) | Outcome::Called(_)) => {}
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
        let mut globs: AVec<'a, GlobEdge> = AVec::with_capacity_in(md.edges.len(), alloc);
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
}

/// The namespaces in the record's order are the order [`NAMESPACES`] lists them in.
const _: () = assert!(matches!(NAMESPACES, [Namespace::Types, Namespace::Values, Namespace::Macros]));
