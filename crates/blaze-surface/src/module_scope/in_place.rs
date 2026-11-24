//! A [`ModuleScope`](super) record read in place with no check (D114 rule 1, "a record is its
//! own index"; rule 74, verify once): [`ScopeView::trusted`] views bytes that
//! [`ScopeView::read`] accepted before in this process, in O(1) and with no allocation, and
//! [`Globs`] hands a lookup a scope's glob edges as the working form's slice or the record's
//! rows alike, so one walk reads both.
//!
//! NEVER COMPILED (lane d114a-inplace, 2026-11-16): inert until `mod in_place;` is added to
//! module_scope.rs by the wiring.

use blaze_format::field;
use blaze_id::{ContentId, ItemKey};

use super::{ErrorTag, GlobEdge, GlobRow, ModuleScopeRoot, RefusedWhy, ScopeView, TextualDef, def_of, vis_of};
use crate::column::text_of;
use crate::path::Paths;

/// A scope's glob edges as a lookup through globs walks them: the working form's slice
/// ([`Globs::of`]) or a record's rows read in place ([`ScopeView::edges`]). Each edge is made
/// when asked, from 40 bytes of the row: nothing is decoded ahead.
#[derive(Clone, Copy, Debug)]
pub struct Globs<'w>(Of<'w>);

#[derive(Clone, Copy, Debug)]
enum Of<'w> {
    Edges(&'w [GlobEdge]),
    Rows(&'w [GlobRow], &'w [ContentId]),
}

fn edge_of(row: &GlobRow, vis_keys: &[ContentId]) -> GlobEdge {
    GlobEdge {
        source: def_of(row.source, row.def_kind, row.shape),
        vis: vis_of(vis_keys, row.vis, row.vis_key),
        expansion: row.expansion,
    }
}

impl<'w> Globs<'w> {
    /// The working form's edges.
    pub fn of(edges: &'w [GlobEdge]) -> Globs<'w> {
        Globs(Of::Edges(edges))
    }

    pub fn len(&self) -> usize {
        match self.0 {
            Of::Edges(edges) => edges.len(),
            Of::Rows(rows, _) => rows.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Edge `i`, by source order.
    pub fn get(&self, i: usize) -> Option<GlobEdge> {
        match self.0 {
            Of::Edges(edges) => edges.get(i).copied(),
            Of::Rows(rows, vis_keys) => rows.get(i).map(|row| edge_of(row, vis_keys)),
        }
    }

    /// Every edge, by source.
    pub fn iter(&self) -> GlobIter<'w> {
        match self.0 {
            Of::Edges(edges) => GlobIter::Edges(edges.iter()),
            Of::Rows(rows, vis_keys) => GlobIter::Rows(rows.iter(), vis_keys),
        }
    }
}

/// Iterator returned by [`Globs::iter`].
pub enum GlobIter<'w> {
    Edges(core::slice::Iter<'w, GlobEdge>),
    Rows(core::slice::Iter<'w, GlobRow>, &'w [ContentId]),
}

impl Iterator for GlobIter<'_> {
    type Item = GlobEdge;

    fn next(&mut self) -> Option<GlobEdge> {
        match self {
            GlobIter::Edges(it) => it.next().copied(),
            GlobIter::Rows(it, vis_keys) => it.next().map(|row| edge_of(row, vis_keys)),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            GlobIter::Edges(it) => it.size_hint(),
            GlobIter::Rows(it, _) => it.size_hint(),
        }
    }
}

impl ExactSizeIterator for GlobIter<'_> {}

impl<'a> ScopeView<'a> {
    /// The record in `buffer` with no check at all: O(1), no allocation. Each column is a
    /// projection of the proof `blaze_format` made when the record was opened, so a lookup
    /// touches only the pages it reads.
    ///
    /// # Safety
    ///
    /// [`ScopeView::read`] returned `Ok` for these same bytes, at this same address, earlier in
    /// this process, and they have not changed since (they are borrowed immutably for `'a`, and
    /// a mapped segment is mapped read-only). The one caller is blaze-scope's names record,
    /// whose `Verified` proof is made only by running `read` on every module scope it holds.
    pub unsafe fn trusted(buffer: &'a [u8]) -> ScopeView<'a> {
        // SAFETY: `read` began with `blaze_format::open::<ModuleScopeRoot>(buffer)`, which
        // returned `Ok` for these unchanged bytes at this address (the caller's promise): what
        // `reopen` requires.
        let root = unsafe { blaze_format::reopen::<ModuleScopeRoot>(buffer) };
        let text: &'a [u8] = field!(root, ModuleScopeRoot, strings).items().as_slice();
        // SAFETY: `read` took the strings column with `Slice::as_str`, which refuses bytes that
        // are not UTF-8; the bytes are the same.
        let strings = unsafe { core::str::from_utf8_unchecked(text) };
        let paths = Paths::trusted(
            field!(root, ModuleScopeRoot, paths).items().as_slice(),
            field!(root, ModuleScopeRoot, segments).items().as_slice(),
            strings,
        );
        ScopeView {
            module: ItemKey(root.module),
            rows: field!(root, ModuleScopeRoot, bindings).items().as_slice(),
            globs: field!(root, ModuleScopeRoot, globs).items().as_slice(),
            errors: field!(root, ModuleScopeRoot, errors).items().as_slice(),
            textual: field!(root, ModuleScopeRoot, textual).items().as_slice(),
            marks: field!(root, ModuleScopeRoot, marks).items().as_slice(),
            traits: field!(root, ModuleScopeRoot, traits).items().as_slice(),
            macro_exports: field!(root, ModuleScopeRoot, macro_exports).items().as_slice(),
            paths,
            strings,
            vis_keys: field!(root, ModuleScopeRoot, vis_keys).items().as_slice(),
        }
    }

    /// The glob edges as a lookup walks them, read in place.
    pub fn edges(&self) -> Globs<'a> {
        Globs(Of::Rows(self.globs, self.vis_keys))
    }

    /// The first refused macro call of the scope (in the order the errors are written) and why:
    /// the rows' tags are read, nothing else of an error is decoded.
    pub fn refused(&self) -> Option<(ItemKey, RefusedWhy)> {
        self.errors.iter().find_map(|row| match row.tag {
            ErrorTag::RefusedScript => Some((ItemKey(row.site_key), RefusedWhy::BuildScriptOutputs)),
            ErrorTag::RefusedEnv => Some((ItemKey(row.site_key), RefusedWhy::UnknownEnv)),
            ErrorTag::RefusedExpansion => Some((ItemKey(row.site_key), RefusedWhy::Expansion)),
            _ => None,
        })
    }

    /// The latest `macro_rules!` named `name` visible at the end of the module's textual
    /// scope (`child: None`) or at the `mod` line of `child` (none: nothing of the module is).
    pub fn textual_before(&self, child: Option<ItemKey>, name: &str) -> Option<TextualDef<&'a str>> {
        let end = match child {
            None => self.textual.len(),
            Some(c) => self.mark(c).map_or(0, |len| len as usize),
        };
        let strings = self.strings;
        self.textual[..end.min(self.textual.len())].iter().rev().find(|row| text_of(strings, row.name) == name).map(|row| TextualDef {
            name: text_of(strings, row.name),
            def: ItemKey(row.def),
            expansion: row.expansion,
            shapes: row.shapes,
        })
    }
}
