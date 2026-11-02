//! A tree's block in the allocator it was built in, read through one [`Tree`] whatever that
//! allocator is, so that a view ([`crate::TokenTreesView`]) stays one non-generic pointer.
#![allow(unsafe_code)]

use std::{
    alloc::{Allocator, Global},
    mem::ManuallyDrop,
    ops::Deref,
    ptr::NonNull,
};

use span::Span;

use super::{Layout, words::WordsCache};

/// What every view of a tree reads: its block, the block's layout, the top subtree's two spans
/// and the parser words kept. Only ever a field of the [`TopSubtree`] that owns the block, and
/// handed out only as `&Tree`, a borrow of that `TopSubtree`.
pub struct Tree {
    /// `HEADER`, then the columns at the offsets `layout` gives.
    block: NonNull<[u32]>,
    pub(super) layout: Layout,
    pub(super) top_open: Span,
    pub(super) top_close: Span,
    pub(super) words: WordsCache,
    /// Whether the parser words made of the tree are kept in `words`: a tree that outlives its
    /// task (built with `build`) keeps them, a task's temporary (built with `build_in`) has its
    /// parser input made in the worker's buffers instead.
    keeps_words: bool,
}

// SAFETY: for every `&self` a `Tree` is a `&[u32]` to its block, which is written only through
// `&mut` the `TopSubtree` owning it (`TopSubtree::block_mut`), as a `Box<[u32]>` is; its other
// fields are `Send + Sync`.
unsafe impl Send for Tree {}
// SAFETY: as for `Send`.
unsafe impl Sync for Tree {}

impl Tree {
    /// The block: `HEADER`, then the columns at the offsets `layout` gives.
    #[inline]
    pub(super) fn block(&self) -> &[u32] {
        // SAFETY: `block` is the allocation of the `TopSubtree` this `Tree` is a field of (made
        // in `TopSubtree::from_block`, freed only in its `Drop`), written only through `&mut`
        // that `TopSubtree`, which no `&self` overlaps.
        unsafe { self.block.as_ref() }
    }

    /// Whether the parser words made of the tree are kept with it: true for a tree that
    /// outlives its task, false for a task's temporary ([`super::TopSubtreeBuilder::build_in`]).
    #[inline]
    pub fn keeps_words(&self) -> bool {
        self.keeps_words
    }
}

/// A token tree: its block in `A` (the global allocator for a tree that is kept, a task's
/// `Frame` for one that is a temporary of the task), read through [`Tree`].
pub struct TopSubtree<A: Allocator = Global> {
    tree: Tree,
    alloc: ManuallyDrop<A>,
}

impl<A: Allocator> TopSubtree<A> {
    pub(super) fn from_block(
        block: Box<[u32], A>,
        layout: Layout,
        top_open: Span,
        top_close: Span,
        keeps_words: bool,
    ) -> Self {
        let (block, alloc) = Box::into_raw_with_allocator(block);
        let block = NonNull::new(block).expect("a box's pointer is never null");
        let words = WordsCache::default();
        let tree = Tree { block, layout, top_open, top_close, words, keeps_words };
        TopSubtree { tree, alloc: ManuallyDrop::new(alloc) }
    }

    /// The allocator the block is in.
    #[inline]
    pub fn allocator(&self) -> &A {
        &self.alloc
    }

    #[inline]
    pub(super) fn block_mut(&mut self) -> &mut [u32] {
        // SAFETY: the block is `self`'s (`from_block`), and `&mut self` excludes every `&Tree`,
        // each of which borrows `self`.
        unsafe { self.tree.block.as_mut() }
    }

    /// Sets the top subtree's spans and drops the words kept: the top delimiters' contexts are
    /// their words' editions.
    pub(super) fn set_top_spans(&mut self, open: Span, close: Span) {
        self.tree.top_open = open;
        self.tree.top_close = close;
        self.forget_words();
    }

    /// The top subtree's spans, to change in place where the words kept stay true of them (an
    /// AST id, which no word holds).
    pub(super) fn top_spans_mut(&mut self) -> (&mut Span, &mut Span) {
        (&mut self.tree.top_open, &mut self.tree.top_close)
    }

    /// Drops the words kept, after the block changed.
    pub(super) fn forget_words(&mut self) {
        self.tree.words = WordsCache::default();
    }
}

impl<A: Allocator> Drop for TopSubtree<A> {
    fn drop(&mut self) {
        // SAFETY: `alloc` is taken here, once, and not used after.
        let alloc = unsafe { ManuallyDrop::take(&mut self.alloc) };
        // SAFETY: `block` and `alloc` are the pair `Box::into_raw_with_allocator` gave in
        // `from_block`, and nothing but this `drop` frees the block.
        drop(unsafe { Box::from_raw_in(self.tree.block.as_ptr(), alloc) });
    }
}

impl<A: Allocator> Deref for TopSubtree<A> {
    type Target = Tree;

    #[inline]
    fn deref(&self) -> &Tree {
        &self.tree
    }
}

impl<A: Allocator + Clone> TopSubtree<A> {
    /// A copy of the tree in its allocator: a named cost, as the type is not `Clone`. It keeps
    /// the parser's words when `self` does, which a rebuild from its token trees need not.
    pub fn copy(&self) -> Self {
        // alloc: a copy of the block, in the tree's allocator, at its exact length.
        let mut block = Vec::with_capacity_in(self.block().len(), (*self.alloc).clone());
        block.extend_from_slice(self.block());
        let tree = &self.tree;
        let (layout, open, close) = (tree.layout, tree.top_open, tree.top_close);
        TopSubtree::from_block(block.into_boxed_slice(), layout, open, close, tree.keeps_words)
    }
}
