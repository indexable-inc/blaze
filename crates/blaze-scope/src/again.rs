//! Resolving again after an edit, soundly inside cycles (docs/v2/plan/design/bottom.md,
//! section 6, "Resolving again").
//!
//! A driver reruns the edited modules, then every module whose read list no longer holds, and
//! keeps every other record. That is exact for a module whose reads see what they saw, with
//! one hole: a kept record can be justified only by the record being rerun. With `c: pub fn
//! x`, `p: pub use c::x`, `q: pub use p::x`, editing `p` to `pub use q::x` reruns `p`, which
//! reads `q`'s kept record (`x` is `c::x`, by way of `p`'s old import), binds `c::x` again, and
//! `q`'s read of `p` still sees what it saw: both keep a binding a cold fill does not make,
//! because nothing grounds it any more. A read list compares values; it cannot see that the
//! derivation of a value went around a cycle.
//!
//! The rule that closes it: a rerun may read a kept record only if that record does not reach
//! back to a binding the rerun derives anew. [`Touched`] is what a module's rerun may derive
//! anew (the names its edited text binds, and the names of the imports whose paths mention
//! them; all of them for a rerun forced by its reads, or an edit of its globs, macro calls or
//! `macro_rules!`). [`Again::suspects`] is the kept records that both read such a binding (by
//! name, then transitively by module) and are read by the rerun (transitively through kept
//! records): those are solved again from their text together with the rerun, which then
//! groups any cycle among them and reaches its least fixpoint. A record outside that
//! intersection keeps a derivation that never went through the rerun, so its value stands.
//!
//! The precision this keeps: a `pub use` added inside a cycle touches one new name, which no
//! kept record read, so it reruns alone. Sans-IO; the walks are explicit stacks.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use blaze_hash::{HashMap, HashSet};
use blaze_id::ItemKey;
use blaze_own::{AVec, Alloc, Vec};
use blaze_surface::{DeclsBuilder, Rename, ScopeBuilder, UseForm};

use crate::facts::Name;
use crate::read::Read;
use crate::scope::ReadList;

/// The names of a module whose bindings a rerun may derive anew (perhaps to the same target):
/// `wild`, every name and the module's glob edges and textual scope.
#[derive(Clone, Debug)]
pub struct Touched<'a, N> {
    pub names: AVec<'a, N>,
    pub wild: bool,
}

impl<'a, N: Name> Touched<'a, N> {
    /// Every binding of the module: a rerun forced by its reads, or a module new in the edit.
    pub fn wild(alloc: Alloc<'a>) -> Touched<'a, N> {
        Touched { names: Vec::new_in(alloc), wild: true }
    }

    /// What an edit of a module's text from `old` to `new` may derive anew: the names of the
    /// declarations, `mod`s, `extern crate`s and named `use`s on one side and not the other,
    /// then the names of the named `use`s whose paths mention one (a lookup of a name reads only
    /// that name's binding); every name when a glob, a macro call or a `macro_rules!` changed,
    /// or a glob's or macro call's path mentions a touched name.
    pub fn of_text<'d>(old: &DeclsBuilder<'d, N>, new: &DeclsBuilder<'d, N>, alloc: Alloc<'a>) -> Touched<'a, N> {
        let mut t = Touched { names: Vec::new_in(alloc), wild: false };
        let globs = |d: &DeclsBuilder<'_, N>| d.uses.iter().filter(|u| u.leaf == UseForm::Glob).count();
        let calls_same = old.calls.len() == new.calls.len()
            && old.calls.iter().zip(new.calls.iter()).all(|(a, b)| a.call == b.call && a.path == b.path && a.kind == b.kind && a.pos == b.pos && a.item == b.item);
        if !calls_same || old.macro_rules[..] != new.macro_rules[..] || globs(old) != globs(new) {
            t.wild = true;
            return t;
        }
        for (a, b) in [(old, new), (new, old)] {
            for item in a.items.iter() {
                if !b.items.iter().any(|o| o.name == item.name && o.ns == item.ns && o.def == item.def && o.vis == item.vis) {
                    t.names.push(item.name);
                }
            }
            for m in a.mods.iter() {
                if !b.mods.iter().any(|o| o.name == m.name && o.module == m.module && o.vis == m.vis && o.macro_use == m.macro_use) {
                    t.names.push(m.name);
                }
            }
            for e in a.extern_crates.iter() {
                if !b.extern_crates.iter().any(|o| o == e) {
                    match e.rename {
                        Rename::To(n) => t.names.push(n),
                        // An `extern crate` binds its rename or its crate's name; either way the
                        // module's textual and prelude reads may move.
                        Rename::Same | Rename::Underscore => {
                            t.wild = true;
                            return t;
                        }
                    }
                }
            }
            for u in a.uses.iter() {
                let same = b.uses.iter().any(|o| o.path == u.path && o.leaf == u.leaf && o.vis == u.vis && o.pos == u.pos);
                if same {
                    continue;
                }
                if u.leaf == UseForm::Glob {
                    t.wild = true;
                    return t;
                }
                if let Some(n) = bound(u) {
                    t.names.push(n);
                }
            }
        }
        t.names.sort_unstable();
        t.names.dedup();
        // The imports that look a touched name up, until none is added: at most one round per
        // import, so bounded by the module's uses.
        let mut moved = true;
        while moved && !t.wild {
            moved = false;
            for d in [old, new] {
                for u in d.uses.iter() {
                    if !u.path.segments.iter().any(|s| t.names.binary_search(s).is_ok()) {
                        continue;
                    }
                    if u.leaf == UseForm::Glob {
                        t.wild = true;
                        break;
                    }
                    if let Some(n) = bound(u) {
                        if let Err(at) = t.names.binary_search(&n) {
                            t.names.insert(at, n);
                            moved = true;
                        }
                    }
                }
                if d.calls.iter().any(|c| c.path.segments.iter().any(|s| t.names.binary_search(s).is_ok())) {
                    t.wild = true;
                }
            }
        }
        t
    }

    /// Adds the names whose bindings differ between a module's `old` and `new` scopes; every
    /// name when its glob edges or textual scope differ.
    pub fn add_changed(&mut self, old: &ScopeBuilder<'_, N>, new: &ScopeBuilder<'_, N>) {
        if old.globs[..] != new.globs[..] || old.textual[..] != new.textual[..] || old.marks[..] != new.marks[..] {
            self.wild = true;
            return;
        }
        for (a, b) in [(old, new), (new, old)] {
            for e in a.bindings.iter() {
                if b.get(e.ns, e.name) != Some(&e.binding) {
                    self.names.push(e.name);
                }
            }
        }
        self.names.sort_unstable();
        self.names.dedup();
    }

    /// Whether `read` of the module's record reads a binding this touches.
    fn reads(&self, read: &Read<N>) -> bool {
        match *read {
            Read::Name { name, .. } => self.wild || self.names.binary_search(&name).is_ok(),
            Read::Globs { .. } | Read::TextualEntry { .. } | Read::TextualOut { .. } | Read::DeriveHelpers { .. } => self.wild,
        }
    }
}

/// The name a named `use` binds; none for `as _`.
fn bound<N: Copy>(u: &blaze_surface::UseDecl<N, blaze_surface::Path<'_, N>>) -> Option<N> {
    match u.leaf {
        UseForm::Named { rename: Rename::To(n), .. } => Some(n),
        UseForm::Named { rename: Rename::Same, .. } => u.path.segments.last().copied(),
        UseForm::Named { rename: Rename::Underscore, .. } | UseForm::Glob => None,
    }
}

/// The read graph of the records before an edit, both ways, by module.
pub struct Again<'a, N> {
    /// For each module, who read its record and what of it.
    readers: HashMap<ItemKey, AVec<'a, (ItemKey, Read<N>)>, Alloc<'a>>,
    /// For each module, the modules whose records it read.
    reads: HashMap<ItemKey, AVec<'a, ItemKey>, Alloc<'a>>,
    alloc: Alloc<'a>,
}

impl<'a, N: Name> Again<'a, N> {
    /// The graph of `records` (each module with its read list); `parent` names a module's
    /// parent, the owner of its `TextualEntry`.
    pub fn new_in<'r>(
        records: impl Iterator<Item = (ItemKey, &'r ReadList<'r, N>)>,
        parent: impl Fn(ItemKey) -> Option<ItemKey>,
        alloc: Alloc<'a>,
    ) -> Again<'a, N>
    where
        N: 'r,
    {
        let mut readers: HashMap<ItemKey, AVec<'a, (ItemKey, Read<N>)>, Alloc<'a>> = HashMap::new_in(alloc);
        let mut reads: HashMap<ItemKey, AVec<'a, ItemKey>, Alloc<'a>> = HashMap::new_in(alloc);
        for (m, list) in records {
            let mut owners = Vec::new_in(alloc);
            for e in list.entries.iter() {
                let Some(owner) = owner(&e.read, &parent) else { continue };
                readers.entry(owner).or_insert_with(|| Vec::new_in(alloc)).push((m, e.read));
                owners.push(owner);
            }
            owners.sort_unstable();
            owners.dedup();
            reads.insert(m, owners);
        }
        Again { readers, reads, alloc }
    }

    /// The kept records to solve again with `rerun` (each module just solved, its new read list
    /// and what it may derive anew): those that read a touched binding, transitively, and that
    /// the rerun reads, transitively through kept records; sorted. `kept` says whether a
    /// module's record is the one from before the edit.
    pub fn suspects(
        &self,
        rerun: &[(ItemKey, &ReadList<'_, N>, &Touched<'_, N>)],
        kept: impl Fn(ItemKey) -> bool,
        parent: impl Fn(ItemKey) -> Option<ItemKey>,
    ) -> AVec<'a, ItemKey> {
        let a = self.alloc;
        // Kept records reached backwards from a touched binding.
        let mut touched: HashSet<ItemKey, Alloc<'a>> = HashSet::new_in(a);
        let mut stack = Vec::new_in(a);
        for (m, _, t) in rerun.iter() {
            for (reader, read) in self.readers.get(m).map(|v| v.as_slice()).unwrap_or(&[]) {
                if kept(*reader) && t.reads(read) && touched.insert(*reader) {
                    stack.push(*reader);
                }
            }
        }
        while let Some(m) = stack.pop() {
            for (reader, _) in self.readers.get(&m).map(|v| v.as_slice()).unwrap_or(&[]) {
                if kept(*reader) && touched.insert(*reader) {
                    stack.push(*reader);
                }
            }
        }
        if touched.is_empty() {
            return Vec::new_in(a);
        }
        // Kept records the rerun reads, transitively.
        let mut read: HashSet<ItemKey, Alloc<'a>> = HashSet::new_in(a);
        for (_, list, _) in rerun.iter() {
            for e in list.entries.iter() {
                if let Some(o) = owner(&e.read, &parent) {
                    if kept(o) && read.insert(o) {
                        stack.push(o);
                    }
                }
            }
        }
        while let Some(m) = stack.pop() {
            for &o in self.reads.get(&m).map(|v| v.as_slice()).unwrap_or(&[]) {
                if kept(o) && read.insert(o) {
                    stack.push(o);
                }
            }
        }
        let mut out = Vec::new_in(a);
        out.extend(touched.iter().copied().filter(|m| read.contains(m)));
        out.sort_unstable();
        out
    }
}

/// The module whose record answers `read`.
fn owner<N: Copy>(read: &Read<N>, parent: &impl Fn(ItemKey) -> Option<ItemKey>) -> Option<ItemKey> {
    match *read {
        Read::TextualEntry { module } => parent(module),
        Read::Name { module, .. } | Read::Globs { module } | Read::TextualOut { module } => Some(module),
        Read::DeriveHelpers { .. } => None,
    }
}
