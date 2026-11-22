//! Written visibilities (R4): `pub`, `pub(crate)`, `pub(super)`, `pub(self)` and `pub(in path)`.
//!
//! Reference `vis.scoped.in`: "`pub(in path)` makes an item visible within the provided `path`.
//! `path` must be a simple path which resolves to an ancestor module of the item whose
//! visibility is being declared." The path is first walked down the item's own module chain by
//! the modules' own names, which reads no entries. A path that leaves the chain may still
//! resolve to an ancestor ("resolves to": through an import, `use crate::a as x;` then
//! `pub(in crate::x)`), so the rest of it is read through the cells like any path
//! ([`Cells::entry_vis`]): an entry's cell waits on the cells its path reads.
//!
//! A visibility naming no ancestor (`pub(in path)` resolving elsewhere or nowhere, `pub(super)`
//! at the root, a `super` above it, a path from no module, `pub(path)` without `in`) is an
//! error AT THAT ITEM (`ScopeError::RestrictedNotAncestor` in its module's scope), never a
//! refusal of the crate's load: the rest of the crate still resolves and reports its own
//! errors. The item is then taken as private to its module (Reference `vis.private`: "visible
//! within the current module and its descendants"): every written visibility contains its
//! own module, so this is the one visibility no reading of the bad path can be narrower than;
//! no other module is shown a name through a restriction nobody checked, and the scope's error
//! keeps any program from being built on the choice.
//!
//! A `use` or `extern crate` whose `pub(in path)` leaves the chain is refused by name
//! (`CellError::BadVisibility` with `BadVis::Segment`): an import's own visibility is read when
//! the import settles, and reading it through the cells there is not built (rule 3).

use blaze_id::ItemKey;
use blaze_own::{AVec, Alloc};
use blaze_surface::ScopeError;

use super::{BadVis, CellError, CellPlant, Cells, FragId, Guess, Layer, ModId, Node, Step, Value, cell_slot};
use crate::{Def, DefKind, Edition, Namespace, Origin, Path, PathKind, Visibility, WrittenVis};

/// Where a refused or read-through visibility is written: its module, its fragment, and its
/// place among the fragment's declarations (a kind tag, then the index): the order its errors
/// are written in, the same on every schedule.
#[derive(Clone, Copy)]
pub(super) struct VisAt {
    pub(super) module: ModId,
    pub(super) frag: FragId,
    pub(super) ord: (u8, u32),
}

/// The kind tags of [`VisAt::ord`], in the order a fragment's declarations are read.
pub(super) const ORD_ITEM: u8 = 0;
pub(super) const ORD_MOD: u8 = 1;
pub(super) const ORD_EXTERN_CRATE: u8 = 2;
pub(super) const ORD_USE: u8 = 3;

/// A refused visibility: the written path (`super` alone for `pub(super)`) for its error.
pub(super) struct BadVisAt<'a, N> {
    pub(super) at: VisAt,
    pub(super) kind: PathKind,
    pub(super) segments: AVec<'a, N>,
}

/// An explicit entry's `pub(in path)` that left the module chain at segment `skip`, in module
/// `from`: the rest is read through the cells.
pub(super) struct VisPath<'a, N> {
    pub(super) at: VisAt,
    pub(super) from: ModId,
    pub(super) skip: u32,
    pub(super) kind: PathKind,
    pub(super) segments: AVec<'a, N>,
}

/// What a written visibility's walk down the chain gave.
enum VisOf {
    Known(Visibility),
    /// Names no ancestor wherever names resolve (`BadVis` names the cases; the error is the
    /// scope's `RestrictedNotAncestor` with the written path).
    Bad,
    /// Left the chain at segment `skip`, in module `from`.
    Through { from: ModId, skip: usize },
}

/// One segment of a read-through path.
enum Seg {
    Module(ModId),
    NoAncestor,
    /// A cell on the way is outside what the core follows.
    Unknown,
}

/// Where a read-through path ends once its cells are decided.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum VisEnd {
    Ancestor(Visibility),
    NoAncestor,
    Unknown,
}

/// The written path of a visibility, for its error: `super` alone for `pub(super)`.
fn written_path<'v, N>(v: &'v WrittenVis<Path<'_, N>>) -> (PathKind, &'v [N]) {
    match v {
        WrittenVis::In(p) | WrittenVis::WithoutIn(p) => (p.kind, &p.segments[..]),
        WrittenVis::Super => (PathKind::Super(1), &[]),
        // Never refused: these name the crate, the module itself or nothing.
        WrittenVis::Pub | WrittenVis::Crate | WrittenVis::Private | WrittenVis::SelfOnly => (PathKind::SelfMod, &[]),
    }
}

impl<'a, N: crate::Name> Cells<'a, N> {
    /// Visible within module `at` and its descendants: the crate at the root.
    pub(super) fn vis_at(&self, at: ModId) -> Visibility {
        if at == self.root { Visibility::Crate(self.krate) } else { Visibility::Restricted(self.modules[at as usize].key) }
    }

    /// Private to module `m`: to the nearest module around it that is no block's (a block is
    /// no privacy boundary: `vis.scoped`, the Reference "Block expressions").
    fn private_to(&self, m: ModId) -> Visibility {
        self.vis_at(self.normal(m))
    }

    /// The walk of `v` down module `m`'s own chain by the modules' names (Reference
    /// `vis.scoped.in`): it reads no entries.
    fn walk_written(&self, m: ModId, v: &WrittenVis<Path<'_, N>>) -> VisOf {
        let module = &self.modules[m as usize];
        let at = match v {
            WrittenVis::Pub => return VisOf::Known(Visibility::Public),
            WrittenVis::Crate => return VisOf::Known(Visibility::Crate(self.krate)),
            // `self` and `super` pass over blocks' modules ([`Cells::up`]).
            WrittenVis::Private | WrittenVis::SelfOnly => self.normal(m),
            WrittenVis::Super => match self.up(m, 1) {
                Some(p) => p,
                // `BadVis::AboveRoot`.
                None => return VisOf::Bad,
            },
            // `BadVis::WithoutIn`.
            WrittenVis::WithoutIn(_) => return VisOf::Bad,
            WrittenVis::In(p) => {
                let mut at = match p.kind {
                    PathKind::Crate => self.root,
                    PathKind::SelfMod => self.normal(m),
                    PathKind::Super(n) => match self.up(m, n) {
                        Some(a) => a,
                        None => return VisOf::Bad,
                    },
                    // Reference `vis.scoped.edition2018`: "The 2015 edition may also use paths
                    // starting with `::` or modules from the crate root."
                    PathKind::Plain | PathKind::Abs if self.edition == Edition::E2015 && self.plant != CellPlant::Use2015Lexical => self.root,
                    // `BadVis::Start`.
                    PathKind::Plain | PathKind::Abs | PathKind::DollarCrate(_) => return VisOf::Bad,
                };
                // The start is the module itself or one of its ancestors: on the chain.
                let start = self.modules[at as usize].key;
                let Some(mut i) = module.chain.iter().position(|&k| k == start) else {
                    return VisOf::Through { from: at, skip: 0 };
                };
                for (j, s) in p.segments.iter().enumerate() {
                    let down = i.checked_sub(1).and_then(|next| self.by_key.get(&module.chain[next]).map(|&d| (next, d)));
                    match down {
                        Some((next, d)) if self.modules[d as usize].name == Some(*s) => {
                            at = d;
                            i = next;
                        }
                        // Not the next module down by its own name: the rest may still name an
                        // ancestor through an import, or name another module (an error).
                        _ => return VisOf::Through { from: at, skip: j },
                    }
                }
                at
            }
        };
        VisOf::Known(self.vis_at(at))
    }

    fn record_bad(&mut self, at: VisAt, v: &WrittenVis<Path<'_, N>>) {
        let (kind, segments) = written_path(v);
        let segments = AVec::from_slice_in(segments, self.alloc);
        self.bad_vis.push(BadVisAt { at, kind, segments });
    }

    /// An item or `mod` of fragment `fid` of module `m` with written visibility `v`, added as
    /// an explicit entry. A visibility naming no ancestor is the item's error and the item is
    /// private to `m` (the module doc says why); one read through the cells is recorded, and
    /// the entry's own cell is made now so that the run reads its path even when nothing else
    /// asks for the name.
    pub(super) fn add_declared(
        &mut self,
        m: ModId,
        ns: Namespace,
        name: N,
        def: Def,
        v: &WrittenVis<Path<'_, N>>,
        fid: FragId,
        ord: (u8, u32),
    ) -> Result<(), CellError> {
        let at = VisAt { module: m, frag: fid, ord };
        let mut through = false;
        let vis = match self.walk_written(m, v) {
            VisOf::Known(vis) => vis,
            VisOf::Bad => {
                self.record_bad(at, v);
                self.private_to(m)
            }
            VisOf::Through { from, skip } => {
                // The first entry of a name stays (`add_explicit`): only it is ever read. A
                // later one of the name is the scope's `Duplicate` error already.
                if !self.modules[m as usize].explicit.contains_key(&(ns, name)) {
                    let (kind, segments) = written_path(v);
                    let segments = AVec::from_slice_in(segments, self.alloc);
                    self.vis_paths.insert((m, ns, name), VisPath { at, from, skip: skip as u32, kind, segments });
                    through = true;
                }
                self.private_to(m)
            }
        };
        self.add_explicit(m, ns, name, (def, vis, Origin::Declared, fid))?;
        if through {
            self.cell(m, ns, name, Layer::Full);
        }
        Ok(())
    }

    /// The visibility of a `use` or `extern crate` of module `m` written as `v`. One naming no
    /// ancestor is its error and the import is private to `m`; one that leaves the chain is
    /// refused by name (the module doc says why).
    pub(super) fn import_vis(&mut self, m: ModId, v: &WrittenVis<Path<'_, N>>, at: VisAt) -> Result<Visibility, CellError> {
        match self.walk_written(m, v) {
            VisOf::Known(vis) => Ok(vis),
            VisOf::Bad => {
                self.record_bad(at, v);
                Ok(self.private_to(m))
            }
            VisOf::Through { skip, .. } => Err(CellError::BadVisibility { module: self.modules[m as usize].key, why: BadVis::Segment(skip) }),
        }
    }

    /// One read segment of a read-through path of an entry of module `m`.
    fn vis_segment(&self, m: ModId, v: Value) -> Seg {
        match v {
            Value::Found(def, vis) if def.kind == DefKind::Module && self.visible(vis, m) => match self.by_key.get(&def.key) {
                // Another crate's module is no ancestor, and nothing in it names one of ours.
                Some(&d) if self.modules[d as usize].foreign => Seg::NoAncestor,
                Some(&d) => Seg::Module(d),
                // A module of this crate not made here (a block's not read yet): unknown.
                None if def.key.crate_prefix() == self.krate.prefix() => Seg::Unknown,
                None => Seg::NoAncestor,
            },
            Value::Outside => Seg::Unknown,
            // Nothing, an item that is no module, a module `m` cannot see, or two items: no
            // module the path names.
            Value::Found(..) | Value::NotFound | Value::Ambiguous(..) => Seg::NoAncestor,
        }
    }

    /// The visibility a path ending at `at` gives an entry of `m`: `None` when `at` is no
    /// ancestor of `m` (itself included).
    fn end_vis(&self, m: ModId, at: ModId) -> Option<Visibility> {
        let key = self.modules[at as usize].key;
        self.modules[m as usize].chain.contains(&key).then(|| self.vis_at(at))
    }

    /// The visibility explicit entry `(m, ns, name)` is read with: `vis` as stored, or for a
    /// read-through `pub(in path)` the ancestor its path names by the cells' current values
    /// (`guess` gives those of a cycle). `Done(None)`: a cell on the path is unknown. A path
    /// naming no ancestor leaves `vis` (private to `m`, as stored).
    pub(super) fn entry_vis(&mut self, m: ModId, ns: Namespace, name: N, vis: Visibility, guess: Option<&Guess<'a>>) -> Step<Option<Visibility>> {
        let Some(p) = self.vis_paths.get(&(m, ns, name)) else { return Step::Done(Some(vis)) };
        let (mut at, skip, len) = (p.from, p.skip as usize, p.segments.len());
        for j in skip..len {
            let s = self.vis_paths[&(m, ns, name)].segments[j];
            let c = self.cell(at, Namespace::Types, s, Layer::Full);
            let Some(v) = self.current(c, guess) else { return Step::Wait(Node::Cell(c)) };
            // A module of this crate not made yet (a body solve makes the modules outside its
            // chain when a lookup reaches them, `Cells::load_body`): reached now, as a path's
            // own segment would be.
            if let Value::Found(def, _) = v
                && def.kind == DefKind::Module
                && !self.by_key.contains_key(&def.key)
            {
                let _ = self.reach_outside(def.key);
            }
            match self.vis_segment(m, v) {
                Seg::Module(d) => at = d,
                Seg::NoAncestor => return Step::Done(Some(vis)),
                Seg::Unknown => return Step::Done(None),
            }
        }
        Step::Done(Some(self.end_vis(m, at).unwrap_or(vis)))
    }

    /// The undecided cell entry `(m, ns, name)`'s read-through visibility waits on, if any:
    /// an input of its `Full` cell at a quiet point.
    pub(super) fn vis_wait(&mut self, m: ModId, ns: Namespace, name: N) -> Option<Node> {
        let vis = self.vis_at(m);
        match self.entry_vis(m, ns, name, vis, None) {
            Step::Wait(on) => Some(on),
            Step::Done(_) => None,
        }
    }

    /// Where read-through path `p` ends by the decided cells, after the run.
    pub(super) fn vis_final(&self, p: &VisPath<'a, N>) -> VisEnd {
        let m = p.at.module;
        let mut at = p.from;
        for &s in p.segments[p.skip as usize..].iter() {
            let Some(&c) = self.cell_ids.get(&(at, cell_slot(Namespace::Types, Layer::Full), s)) else { return VisEnd::Unknown };
            let Some(v) = self.cells[c as usize].value else { return VisEnd::Unknown };
            match self.vis_segment(m, v) {
                Seg::Module(d) => at = d,
                Seg::NoAncestor => return VisEnd::NoAncestor,
                Seg::Unknown => return VisEnd::Unknown,
            }
        }
        match self.end_vis(m, at) {
            Some(v) => VisEnd::Ancestor(v),
            None => VisEnd::NoAncestor,
        }
    }

    /// The final visibility of explicit entry `(m, ns, name)` stored as `vis`, after the run;
    /// `None`: its path reads a cell that is unknown.
    pub(super) fn entry_vis_final(&self, m: ModId, ns: Namespace, name: N, vis: Visibility) -> Option<Visibility> {
        match self.vis_paths.get(&(m, ns, name)).map(|p| self.vis_final(p)) {
            None | Some(VisEnd::NoAncestor) => Some(vis),
            Some(VisEnd::Ancestor(v)) => Some(v),
            Some(VisEnd::Unknown) => None,
        }
    }

    /// Module `m`'s `RestrictedNotAncestor` errors, ordered by the call each was written by
    /// (`call_of`: each fragment's, `None` for a module's text) and then by their place in its
    /// declarations: the same on every schedule.
    pub(super) fn vis_errors<'o>(&self, m: ModId, call_of: &[Option<ItemKey>], alloc: Alloc<'o>, out: &mut AVec<'o, ScopeError<N, Path<'o, N>>>) {
        let mut found: AVec<'o, (Option<ItemKey>, (u8, u32), PathKind, &[N])> = AVec::new_in(alloc);
        for b in self.bad_vis.iter() {
            if b.at.module == m {
                found.push((call_of[b.at.frag as usize], b.at.ord, b.kind, &b.segments[..]));
            }
        }
        for (&(pm, _, _), p) in self.vis_paths.iter() {
            if pm == m && self.vis_final(p) == VisEnd::NoAncestor {
                found.push((call_of[p.at.frag as usize], p.at.ord, p.kind, &p.segments[..]));
            }
        }
        // (call, place) is one declaration's own: no two rows share it.
        found.sort_unstable_by(|x, y| (x.0, x.1).cmp(&(y.0, y.1)));
        for &(_, _, kind, segments) in found.iter() {
            out.push(ScopeError::RestrictedNotAncestor { path: Path { kind, segments: AVec::from_slice_in(segments, alloc) } });
        }
    }
}
