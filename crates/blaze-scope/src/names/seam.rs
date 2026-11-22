//! What the path walk ([`super::path`]) reads of a module's scope and of a crate's root: two
//! small traits at the one seam where the working forms and the records meet (rule 78, a
//! strategy is a type parameter at one seam). A fresh solve's `Names` hands out its builders
//! (`&ScopeBuilder`, `&CrateRootBuilder`); a names record hands out views of its own bytes
//! (`ScopeView`, `CrateRootView`), made in O(1) with no allocation and searched in place (D114
//! rule 1). One walk reads both; the test in tests/names_inplace.rs holds their answers equal.
//!
//! NEVER COMPILED (lane d114a-inplace, 2026-11-16): inert until `mod seam;` is added to
//! names.rs by the wiring.

use blaze_id::ItemKey;
use blaze_surface::{
    Binding, CrateRootBuilder, CrateRootHead, CrateRootView, DefKind, Globs, MacroShapes, Namespace, RefusedWhy, ScopeBuilder,
    ScopeError, ScopeView,
};

use crate::facts::Name;
use crate::task::Variant;

/// A module's finished scope, as the walk reads it. `Copy`: a reference or a view.
pub trait ScopeRead<'w, N>: Copy {
    /// The module's own binding of `name` in `ns`.
    fn get(self, ns: Namespace, name: N) -> Option<Binding>;
    /// Its glob edges, for a lookup through them.
    fn globs(self) -> Globs<'w>;
    /// Its first refused macro call and why.
    fn refused(self) -> Option<(ItemKey, RefusedWhy)>;
    /// The latest `macro_rules!` named `name` visible at the end of its textual scope (`None`)
    /// or at the `mod` line of `child`: the definition and its call shapes.
    fn textual(self, child: Option<ItemKey>, name: N) -> Option<(ItemKey, MacroShapes)>;
    /// Whether it marks a place in its textual sequence for `child`: a `mod` line of it. A
    /// body's outermost written block has none (blocks.rs: its `mod` is dropped).
    fn marked(self, child: ItemKey) -> bool;
    /// Each trait it binds in the type namespace and each `use .. as _` import's trait.
    fn each_trait(self, each: &mut dyn FnMut(ItemKey));
}

/// A crate's root record, as the walk reads it. `Copy`: a reference or a view.
pub trait RootRead<N>: Copy {
    fn head(self) -> CrateRootHead;
    /// The root module the extern prelude names `name`.
    fn extern_crate(self, name: N) -> Option<ItemKey>;
    /// The `i`th crate of the `#[macro_use]` prelude, by its root module.
    fn macro_use(self, i: usize) -> Option<ItemKey>;
    /// The `#[macro_export]` macro written in source named `name`.
    fn macro_export(self, name: N) -> Option<(ItemKey, MacroShapes)>;
}

/// A variant found by name: its key, and whether it is a record variant (a type only).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VariantHit {
    pub key: ItemKey,
    pub record: bool,
}

/// `name` among a working form's variants.
pub fn variant_in<N: Name>(variants: &[Variant<N>], name: N) -> Option<VariantHit> {
    variants.iter().find(|v| v.name.as_ref() == name.as_ref()).map(|v| VariantHit { key: v.key, record: v.record })
}

impl<'w, N: Name> ScopeRead<'w, N> for &'w ScopeBuilder<'w, N> {
    fn get(self, ns: Namespace, name: N) -> Option<Binding> {
        ScopeBuilder::get(self, ns, name).copied()
    }

    fn globs(self) -> Globs<'w> {
        Globs::of(&self.globs)
    }

    fn refused(self) -> Option<(ItemKey, RefusedWhy)> {
        self.errors.iter().find_map(|e| match e {
            ScopeError::Refused { call, why, .. } => Some((*call, *why)),
            _ => None,
        })
    }

    fn textual(self, child: Option<ItemKey>, name: N) -> Option<(ItemKey, MacroShapes)> {
        let end = match child {
            None => self.textual.len(),
            Some(c) => self.mark(c).map_or(0, |len| len as usize),
        };
        self.textual[..end.min(self.textual.len())].iter().rev().find(|d| d.name == name).map(|d| (d.def, d.shapes))
    }

    fn marked(self, child: ItemKey) -> bool {
        ScopeBuilder::mark(self, child).is_some()
    }

    fn each_trait(self, each: &mut dyn FnMut(ItemKey)) {
        for entry in self.bindings.iter() {
            if entry.ns == Namespace::Types && entry.binding.def.kind == DefKind::Trait {
                each(entry.binding.def.key);
            }
        }
        for anon in self.traits.iter() {
            each(anon.def.key);
        }
    }
}

impl<'w, N: Name> ScopeRead<'w, N> for ScopeView<'w> {
    fn get(self, ns: Namespace, name: N) -> Option<Binding> {
        ScopeView::get(&self, ns, name.as_ref())
    }

    fn globs(self) -> Globs<'w> {
        self.edges()
    }

    fn refused(self) -> Option<(ItemKey, RefusedWhy)> {
        ScopeView::refused(&self)
    }

    fn textual(self, child: Option<ItemKey>, name: N) -> Option<(ItemKey, MacroShapes)> {
        self.textual_before(child, name.as_ref()).map(|d| (d.def, d.shapes))
    }

    fn marked(self, child: ItemKey) -> bool {
        ScopeView::mark(&self, child).is_some()
    }

    fn each_trait(self, each: &mut dyn FnMut(ItemKey)) {
        for entry in self.bindings() {
            if entry.ns == Namespace::Types && entry.binding.def.kind == DefKind::Trait {
                each(entry.binding.def.key);
            }
        }
        for anon in self.traits() {
            each(anon.def.key);
        }
    }
}

impl<'w, N: Name> RootRead<N> for &'w CrateRootBuilder<'w, N> {
    fn head(self) -> CrateRootHead {
        self.head
    }

    fn extern_crate(self, name: N) -> Option<ItemKey> {
        let at = self.extern_prelude.binary_search_by(|(n, _)| n.as_ref().cmp(name.as_ref())).ok()?;
        Some(self.extern_prelude[at].1)
    }

    fn macro_use(self, i: usize) -> Option<ItemKey> {
        self.macro_use.get(i).copied()
    }

    fn macro_export(self, name: N) -> Option<(ItemKey, MacroShapes)> {
        let at = self.macro_exports.binary_search_by(|(n, _, _)| n.as_ref().cmp(name.as_ref())).ok()?;
        let (_, key, shapes) = self.macro_exports[at];
        Some((key, shapes))
    }
}

impl<'w, N: Name> RootRead<N> for CrateRootView<'w> {
    fn head(self) -> CrateRootHead {
        CrateRootView::head(&self)
    }

    fn extern_crate(self, name: N) -> Option<ItemKey> {
        CrateRootView::extern_crate(&self, name.as_ref())
    }

    fn macro_use(self, i: usize) -> Option<ItemKey> {
        self.macro_use_at(i)
    }

    fn macro_export(self, name: N) -> Option<(ItemKey, MacroShapes)> {
        CrateRootView::macro_export(&self, name.as_ref())
    }
}
