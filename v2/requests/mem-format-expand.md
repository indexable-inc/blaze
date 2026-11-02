# Requests from mem-format-expand (lane mem, under mem-format)

`mem-db-def-expand`'s F1 to F9 (`v2/requests/mem-db-def-expand.md`, from `### F1.`), read
against the code on 2026-10-29 and against `mem-format-tokens`' five sections
(`v2/requests/mem-format-tokens.md`, all five applied by frontend-tokens). Where the code moved
since F was filed, the form below replaces F's; where two requests touch the same lines, the form
below is the one merged change. Nothing here changes what any function returns, any hash, ID or
order: only where memory comes from.

Who holds what (checked 2026-10-29):
- `crates/tt/`, `crates/syntax-bridge/`, `crates/mbe/`: frontend-tokens (window
  `frontend-tokens-next1`). Sections 1, 2a, 3b, 5 and the F7 addendum in
  `v2/requests/mem-format-tokens.md`.
- `crates/span/` (all of it): frontend-tree-span (window `frontend-tree-span`; its brief names the
  whole crate, and it has changed `map.rs` today). Sections 2b, 3a, 4.
- `crates/hir-expand/`: db-expand-macros. The callers each section lists there are E's (E2, E3,
  E8, E11, E13), which `mem-db-def-expand` already filed for it.
- `crates/blaze-radix/`: fix-radix and its children, all DONE. F8 asks nothing.

Order: 1 and 3a before E2, E5, E8 and E10 to E14; 2 to 5 are independent of each other.

---

## 1. frontend-tokens (`crates/tt`, `crates/syntax-bridge`, `crates/mbe`): F1 on one block

F1 was filed against five boxed columns. `crates/tt/src/storage.rs` now keeps a tree as **one
block** (`block: Box<[u32]>`, `TopSubtree::from_columns`, `storage.rs:556`), so a tree costs one
allocation, and the views hold `tree: &'a TopSubtree` (`lib.rs:184`, 16 bytes, asserted). A
`TopSubtree<A>` whose views borrow `&TopSubtree<A>` would make every view, iterator and caller
generic. Instead the views borrow a non-generic `Tree`, which the `TopSubtree<A>` owning the
block holds: views stay 16 bytes and non-generic, and every reader works on a tree in any
allocator.

> forwarded to frontend-tokens (frontend-tokens-next1 told by tmux; requests, pass 23).

### 1.1 New file `crates/tt/src/storage/owned.rs`

```rust
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
    block: NonNull<[u32]>,
    pub(super) layout: Layout,
    pub(super) top_open: Span,
    pub(super) top_close: Span,
    pub(super) words: WordsCache,
    /// Whether the parser words made of the tree are kept in `words`: a tree that outlives
    /// its task (built with `build`) keeps them, a task's temporary (built with `build_in`)
    /// makes its parser input in the worker's buffers instead.
    pub(super) keeps_words: bool,
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

    /// Whether the parser words made of the tree are kept with it.
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
        // SAFETY: a box's pointer is never null.
        let block = unsafe { NonNull::new_unchecked(block) };
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

    /// Sets the top subtree's spans and drops the words kept, which follow from them.
    pub(super) fn set_top_spans(&mut self, open: Span, close: Span) {
        self.tree.top_open = open;
        self.tree.top_close = close;
        self.tree.words = WordsCache::default();
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

impl<A: Allocator + Clone> Clone for TopSubtree<A> {
    fn clone(&self) -> Self {
        // alloc: a copy of the block, in the tree's allocator, at its exact length.
        let mut block = Vec::with_capacity_in(self.block().len(), (*self.alloc).clone());
        block.extend_from_slice(self.block());
        let tree = &self.tree;
        let (layout, open, close) = (tree.layout, tree.top_open, tree.top_close);
        TopSubtree::from_block(block.into_boxed_slice(), layout, open, close, tree.keeps_words)
    }
}
```

`unsafe.md`'s safe reference: the tests in 1.6 build every tree of `storage/tests.rs`'
`check` in `Global` and in a `Frame` and compare their blocks word for word.

> forwarded to frontend-tokens (requests, pass 23).
> done by frontend-tokens (frontend-tokens-alloc): `crates/tt/src/storage/owned.rs` as written, every SAFETY comment kept. Adapted: `keeps_words` is private with the `keeps_words()` accessor (nothing outside `owned.rs` writes it), `Tree::block` has a doc line, and `top_spans_mut` (1.2's) is beside `set_top_spans`; `set_top_spans` forgets the words through `forget_words`. Not built (common.md).

### 1.2 `crates/tt/src/storage.rs`

- `mod owned; pub use owned::{TopSubtree, Tree};`; the `struct TopSubtree` (L359-367) and its
  `#[derive(Clone)]` go (1.1 has both).
- `PartialEq`/`Eq`/`Hash` (L370-386) move to `Tree`, reading `self.block()`; and
  ```rust
  impl<A: Allocator, B: Allocator> PartialEq<TopSubtree<B>> for TopSubtree<A> {
      fn eq(&self, other: &TopSubtree<B>) -> bool {
          **self == **other
      }
  }
  impl<A: Allocator> Eq for TopSubtree<A> {}
  impl<A: Allocator> Hash for TopSubtree<A> {
      fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
          (**self).hash(state)
      }
  }
  ```
- Every `&self` method of `impl TopSubtree` (L453 and L761: `view`, `iter`, `top_subtree`,
  `token_trees`, `as_token_trees`, `node_count`, `span_count`, `nodes`, `spans`, `wide`,
  `parts`, `symbol_ids`, `columns`, `for_each_token`, `token_slots`, `close_slot`,
  `for_each_slotted_token`, `walk_tokens` and the rest that only read) moves to `impl Tree`, with
  `self.block[..]` read as `self.block()[..]`; through `Deref` they stay callable on any
  `TopSubtree<A>`. `SubtreeView(TokenTreesView { tree: self, .. })` takes `self: &Tree`.
- `set_top_subtree_delimiter_kind` and `set_top_subtree_delimiter_span` move to
  `impl<A: Allocator> TopSubtree<A>`: the first writes `self.block_mut()[HEADER..HEADER + 2]` then
  `self.forget_words()`, the second is `self.set_top_spans(span.open, span.close)`.
- `set_token` and `change_every_ast_id` stay on `impl TopSubtree` (global: they rebuild with
  `build`); `change_every_ast_id` writes through `self.block_mut()`, then (L547-548)
  `self.tree.top_open`/`top_close` through a new `pub(super) fn top_spans_mut(&mut self) ->
  (&mut Span, &mut Span)` beside `set_top_spans` in `owned.rs`, which also forgets the words.
- `from_columns` (L556) becomes
  ```rust
  /// The tree of these columns and top spans in `alloc`: one allocation, each column one copy.
  fn from_columns_in<A: Allocator>(
      nodes: &[Node],
      spans: &[PackedSpan],
      wide: &[WideSpan],
      parts: &[CompressedSpanPart],
      symbols: &[u32],
      top_open: Span,
      top_close: Span,
      keeps_words: bool,
      alloc: A,
  ) -> TopSubtree<A> {
      let layout = Layout::new(nodes.len(), spans.len(), wide.len(), parts.len(), symbols.len());
      // alloc: the tree, one block of its exact size, in `alloc`.
      let mut block = Vec::with_capacity_in(layout.end as usize, alloc);
      /* the five `extend_from_slice` lines and the `debug_assert_eq!` unchanged */
      TopSubtree::from_block(block.into_boxed_slice(), layout, top_open, top_close, keeps_words)
  }
  ```
- `static EMPTY: LazyLock<TopSubtree>` stays; `empty_tree() -> &'static Tree { &EMPTY }`.
- The builder (L1045-1129): `finish(self, first, top, keeps_words: bool, alloc: A) ->
  TopSubtree<A>` (generic `A: Allocator`), passing both to `from_columns_in`;
  ```rust
  pub fn build(self) -> TopSubtree {
      self.build_in_keeping(true, Global)
  }
  /// [`Self::build`] in `alloc`: a task's temporary, which keeps no parser words.
  pub fn build_in<A: Allocator>(self, alloc: A) -> TopSubtree<A> {
      self.build_in_keeping(false, alloc)
  }
  fn build_in_keeping<A: Allocator>(self, keeps_words: bool, alloc: A) -> TopSubtree<A> {
      /* today's `build` body, ending `self.finish(1, top, keeps_words, alloc)` */
  }
  ```
  and `build_skip_top_subtree` / `build_skip_top_subtree_in(self, alloc: A)` the same way,
  through one `fn build_skip_top_subtree_keeping<A: Allocator>(self, keeps_words, alloc)`.
- Constructors, beside today's (which stay, global and keeping words) on
  `impl<A: Allocator> TopSubtree<A>`: `empty_in(span: DelimSpan, alloc: A)`,
  `invisible_from_leaves_in<const N: usize>(delim_span: Span, leaves: [Leaf; N], alloc: A)`,
  `from_token_trees_in(delimiter, token_trees: TokenTreesView<'_>, alloc: A)`,
  `from_subtree_in(subtree: SubtreeView<'_>, alloc: A)`,
  `from_serialized_in(tts: Vec<TokenTree>, alloc: A)`: today's bodies, ending `build_in(alloc)`.
- `storage.rs` gains `#![feature(allocator_api)]`'s use: `use std::alloc::{Allocator, Global};`.

> forwarded to frontend-tokens (requests, pass 23).
> done by frontend-tokens (frontend-tokens-alloc): applied as written. Adapted: the builder's shared body is `build_keeping` (not `build_in_keeping`), and `from_serialized`/`from_serialized_in`/`from_serialized_like` share one `fn serialized(tts) -> TopSubtreeBuilder`, so the body is not written three times. `from_columns_in` carries `#[allow(clippy::too_many_arguments)]`. Not built.

### 1.3 The rest of `crates/tt`

- `src/lib.rs`: `#![feature(allocator_api)]` after the `cfg_attr` line;
  `pub use self::storage::{.., TopSubtree, Tree, ..}`; `TokenTreesView { tree: &'a Tree, .. }`
  (the size assertion holds: one thin pointer); `fmt::Debug`/`Display` (L697-703) move to `Tree`,
  plus `impl<A: Allocator> fmt::Debug for TopSubtree<A>` and `Display` forwarding to `**self`.
- `transform_tt` (L1104) becomes `pub fn transform_tt<'b, A: Allocator + Clone>(tt: &mut
  TopSubtree<A>, ..)`, and the tree it makes is like the one it replaces (same allocator, keeping
  words or not): in `storage.rs`,
  ```rust
  /// [`Self::build`] like `like`: in its allocator, keeping words when it does.
  pub(crate) fn build_like<A: Allocator + Clone>(self, like: &TopSubtree<A>) -> TopSubtree<A> {
      self.build_in_keeping(like.keeps_words(), like.allocator().clone())
  }
  ```
  `*tt = builder.build_like(tt);` at its end, and in the replace arm
  `*tt = TopSubtree::from_serialized_like(replacement.iter_flat_tokens().collect(), tt);`, where
  `pub(crate) fn from_serialized_like<A: Allocator + Clone>(tts: Vec<TokenTree>, like:
  &TopSubtree<A>) -> TopSubtree<A>` is `from_serialized`'s body ending `builder.build_like(like)`.
- `src/storage/words.rs`: `impl TopSubtree { words }` (L139) moves to `impl Tree`, with
  `make: impl FnOnce(&Tree) -> TreeWords`, and `debug_assert!(self.keeps_words, "a task's tree
  keeps no words")`; `TokenTreesView::tree(&self) -> &'a Tree`; `tree_words(make: impl
  FnOnce(&Tree) -> TreeWords)`; `TreeWords::new(tree: &Tree, ..)`.
- `src/storage/wire.rs`: `write_wire` and `parts_of` read a `&Tree`; `read_wire` builds with
  `TopSubtree::from_block(block.into_boxed_slice(), layout, top_open, top_close, true)` (L241).
- `src/storage/tests.rs`: `built.block == tree.block` (L849) reads `built.block() == tree.block()`.

> forwarded to frontend-tokens (requests, pass 23).
> done by frontend-tokens (frontend-tokens-alloc): applied as written (`lib.rs`, `transform_tt` over `build_like`/`from_serialized_like`, `words.rs` with the `debug_assert!` through `keeps_words()`, `wire.rs`, the tests' `block()` reads). Also: `build_like`'s allocator clone carries `// clone:` for hot_lint. Callers outside tt: none needs an edit. Every moved `&self` method is reached through `Deref`, and no caller names one by path. Two doc links still say `tt::TopSubtree::write_wire`, which is now `tt::Tree::write_wire`: `crates/proc-macro-api/src/wire.rs:2` and `crates/blaze-expand/src/bridge/wire.rs:3`. Not built.

### 1.4 `crates/syntax-bridge`

- `src/to_parser_input.rs`: `tree_words(tree: &tt::Tree, ..)` (L74). `view_tokens` (L108), at its
  top, for a tree that keeps no words: its input made in `copy`, the worker's, by the conversion
  the kept words are made by (the doc of `view_tokens` says the two give the same tokens):
  ```rust
  if !view.tree().keeps_words() {
      match fragments {
          true => to_matcher_input_into(view, span_to_edition, copy),
          false => to_parser_input_into(view, span_to_edition, copy),
      }
      let copy: &'a parser::Input = copy;
      return copy.tokens();
  }
  ```
  `view_tokens`' doc gains: "A tree that keeps no words (a task's, [`tt::Tree::keeps_words`])
  has its view converted into `copy` on every call."
- `src/lib.rs`: `fn convert_tokens<C, A: Allocator>(conv: &mut C, len: usize, alloc: A) ->
  tt::TopSubtree<A>` ending `builder.build_skip_top_subtree_in(alloc)`, with
  `fn convert_tokens_kept<C>(conv, len) -> tt::TopSubtree` ending `build_skip_top_subtree()` for
  today's callers; `syntax_node_to_token_tree_in(node, map, span, mode, alloc: A) ->
  tt::TopSubtree<A>` and `call_input_in(node, map, call_site, mode, alloc: A)`, today's bodies
  with `convert_tokens(.., alloc)`; `syntax_node_to_token_tree` and `call_input` keep their
  signatures over `convert_tokens_kept`. `#![feature(allocator_api)]` in `lib.rs`.

> forwarded to frontend-tokens (requests, pass 23).
> 1.4 `lib.rs` bullet done by frontend-tokens (2026-10-29):
> - The conversion body is `convert_into_builder(conv, len) -> TopSubtreeBuilder`, shared by
>   `convert_tokens_kept` (`build_skip_top_subtree()`) and `convert_tokens(conv, len, alloc)`
>   (`build_skip_top_subtree_in(alloc)`), so the body is not written twice. Today's callers use
>   `_kept`.
> - `call_input_in(node: view::Node, map, call_site, mode, alloc) -> tt::TopSubtree<A>` is added.
> - `syntax_node_to_token_tree_in` is **not** added: it would be new code on `SyntaxNode`, and
>   new code uses `syntax::view` (coordinator). `call_input_in` covers a view.
> - `#![feature(allocator_api)]` is there (from 3b).
> - The `to_parser_input.rs` bullet and 1.1-1.3, 1.5 and 1.6 are child `frontend-tokens-alloc`'s,
>   which will answer under each.
>
> Not built.
> done by frontend-tokens (frontend-tokens-alloc): the `to_parser_input.rs` bullet as written: `tree_words(tree: &tt::Tree, ..)`, `view_tokens`' early conversion into `copy` for a tree that keeps no words, and the doc line. Not built.

### 1.5 `crates/mbe`

`DeclarativeMacro::parse_macro_rules<A: Allocator>(tt: &tt::TopSubtree<A>, ..)` and
`parse_macro2<A: Allocator, B: Allocator>(args: Option<&tt::TopSubtree<A>>, body:
&tt::TopSubtree<B>, ..)`: they read `tt.iter()`/`args.iter()`/`body.iter()` only (`lib.rs`
L204-311), so only the signatures change. (mbe has `#![feature(allocator_api)]` already.)

> forwarded to frontend-tokens (requests, pass 23).
> done by frontend-tokens (frontend-tokens-alloc): both signatures as written, with the F7 addendum's frame-collected rules in the same bodies. Adapted: a bare `None` can't infer `A`, so mbe's own `tests.rs` `check_` passes `None::<&tt::TopSubtree>`. The callers outside (hir-expand `declarative.rs:164` and `:193`) pass concrete types and need no edit. Not built.

### 1.6 Tests, `crates/tt/src/storage/tests.rs`

```rust
/// A tree built in a frame has the block of the same tree built globally, word for word, keeps
/// no words, and costs the frame its block alone: no global allocation, warm.
#[test]
fn a_framed_tree_is_the_global_tree_in_the_frame() {
    let tree = lex(0, "fn f(a: u32) -> u32 { (a + 1) * [2][0] }");
    let delimiter = tree.top_subtree().delimiter;
    let view = tree.token_trees();
    let build = |frame: &blaze_mem::frame::Frame| {
        TopSubtree::from_token_trees_in(delimiter, view, frame)
    };
    // The thread's first task takes the frame's first chunk, and the first build grows the
    // builder's buffers: both global, once.
    blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| drop(build(frame)));
    blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
        let (framed, used) = blaze_alloc_count::measure(|| build(frame));
        assert_eq!(used.allocations, 0, "{used:?}");
        assert_eq!(framed.block(), tree.block());
        assert!(framed == tree && !framed.keeps_words() && tree.keeps_words());
        assert_eq!(framed.clone().block(), tree.block());
    });
}
```

Why: an expansion's tree, a definition's, a derive's pieces and the speculative trees are
temporaries of one run (E2, E5, E10 to E12). Built in the task's frame they cost a bump each,
not a global allocation and free, and parsed they make their input in the worker's buffers,
where today each parse also makes and keeps four boxes of `TreeWords` that nobody reads again.

> done by frontend-tokens (frontend-tokens-alloc): the test as written, and `check` (run on every generated and lexed tree) also builds each tree in a frame (`from_serialized_in`, `from_token_trees_in`, `from_subtree_in`) and compares its block word for word with the global one, which is unsafe.md's safe reference. Adapted: `let build = |frame: &Frame| ..` returns a tree that borrows its argument, and closure inference can't tie that return to a higher-ranked argument, so each task calls `from_token_trees_in(delimiter, view, frame)` inline. Not built.

---

> forwarded to frontend-tokens (requests, pass 23).

## 2. F2: readers take views; `ranges_with_span` lends the worker's `u32`s

> forwarded to frontend-tokens (2a) and frontend-tree-span (2b), both told by tmux (requests, pass 23).

### 2a. frontend-tokens (`crates/syntax-bridge/src/lib.rs` L254)

As filed: `token_tree_to_syntax_node(tt: tt::SubtreeView<'_>, entry_point, span_to_edition)`,
first line `let buffer = tt.strip_invisible();`. The callers outside hir-expand, each passing
`.view()` of what it passes today: `hir-ide/src/const_macro.rs:186`,
`hir-def/src/nameres/facts/tests.rs:1137`, `hir-def/src/macro_expansion_tests/mod.rs:403` and
`:447`, `mbe/src/differential.rs:117`, `mbe/src/tests.rs:71`, `syntax-bridge/src/tests.rs:123`,
`:204`, `:479`, `:713`. `reference::token_tree_to_syntax_node` keeps its `&TopSubtree`. hir-expand's
callers are E2's.

> forwarded to frontend-tokens (requests, pass 23).
> done by frontend-tokens (2026-10-29): `token_tree_to_syntax_node(tt: tt::SubtreeView<'_>, ..)`,
> through `token_tree_to_syntax_node_in(tt, .., Global)` (3b). Updated so far:
> - syntax-bridge's test callers (`tests.rs` 121, 202, 217/219, 529, 760).
>
> Still to change:
> - mbe's callers (`tests.rs:71`, the new test at about 635, `differential.rs:119`) go to child
>   `frontend-tokens-alloc`, which holds mbe now.
> - The hir-expand, hir-def and hir-ide callers are filed in `v2/requests/db-expand-macros.md`,
>   `## frontend-tokens: syntax-bridge's new signatures`.
>
> `reference::token_tree_to_syntax_node` keeps `&TopSubtree`. Not built.

### 2b. frontend-tree-span (`crates/span/src/map.rs` L266-284)

As filed in F2: `ranges_with_span` takes `blaze_mem::scratch::lend_u32s()`, `extend`s it with
the filtered run, `sort_unstable()`s it (the indices are distinct, so the order is the radix
sort's) and returns `(0..found.len()).map(move |at| self.range_of(found[at]))`; the doc's "and one
allocation" reads "and nothing allocated once the worker's buffer has grown". span has
`blaze-mem` and `#![feature(allocator_api)]` already. Test, in `map.rs`' tests:
```rust
/// Mapping a span down allocates nothing once the map's index and the worker's buffer are made.
#[cfg(feature = "salsa")]
#[test]
fn ranges_with_span_allocates_nothing_warm() {
    let mut rng = SplitMix64::new(7);
    let (map, pairs) = spans(&mut rng, 200);
    let span = pairs.0[pairs.0.len() / 2].1;
    let want = map.ranges_with_span(span).collect::<Vec<_>>();
    let got = blaze_alloc_count::assert_at_most(0, "a warm ranges_with_span", || {
        map.ranges_with_span(span).count()
    });
    assert_eq!(got, want.len());
    assert_eq!(want, pairs.ranges_with(span, false));
}
```
(`spans` and `Pairs::ranges_with` are the tests' own, L1015 and L957.)

---

> forwarded to frontend-tree-span (requests, pass 23).

> frontend-tree-span (2026-10-29): 2b applied as filed: `found` is `scratch::lend_u32s()`, lent
> until the iterator drops, `sort_unstable`ed; test `ranges_with_span_allocates_nothing_warm`. Not
> built.

## 3. F3: the expansion's span map built in the worker's map, copied out exact

Merged with frontend-tree-span's open request in `v2/requests/frontend-tokens.md` (`##
frontend-tree-span: SpanMap::push_fragment takes the group's span`) and with the applied
"tree sink's buffers lent" section of `mem-format-tokens.md`: the three change
`TtTreeSink`'s same lines, so apply them as this one change.

> forwarded to frontend-tree-span (3a) and frontend-tokens (3b), both told by tmux (requests, pass 23).

### 3a. frontend-tree-span (`crates/span/src/map.rs`)

- `pub struct SpanMap<A: Allocator + Clone = Global>`, the columns `ends: Vec<TextSize, A>`,
  `spans: Vec<Span, A>`, `fragment_starts`/`fragment_ends: Vec<TextSize, A>`, `fragment_spans:
  Vec<(TextRange, Span), A>`; `by_anchor` stays a global `OnceLock<Box<[u32]>>` (made on first
  map-down, the IDE's path). `PartialEq`, `Eq`, `Hash`, `Debug` for every `A`. Every `&self`
  method (`span_at`, `span_at_from`, `spans_for_range`, `ranges_with_span(_exact)`,
  `fragment_span`, `fragments_starting_at/ending_at`, `iter`) on `impl<A: Allocator + Clone>
  SpanMap<A>`, and `push`, `push_fragment`, `finish`, `merge(.., other: &SpanMap<B>)` too.
  `empty`/`with_capacity` stay global; hir-expand's `type ExpansionSpanMap = span::SpanMap` is
  unchanged.
- `finish` stops calling `shrink_to_fit` (L163-164) and sorts the fragments in the worker's
  scratch, as `merge` (L397-399) does:
  ```rust
  thread_local! {
      /// The scratch of the radix sorts of a map's fragment columns, kept by the worker.
      static OFFSET_SORT: RefCell<Scratch<TextSize>> = const { RefCell::new(Scratch::new()) };
      static FRAGMENT_SORT: RefCell<Scratch<(TextRange, Span)>> =
          const { RefCell::new(Scratch::new()) };
  }

  /// Sorts the fragment columns, each in the worker's scratch.
  fn sort_fragments(&mut self) {
      OFFSET_SORT.with_borrow_mut(|scratch| {
          radix::sort_copy_by_key_in(&mut self.fragment_starts, |it| u32::from(*it), scratch.take());
          radix::sort_copy_by_key_in(&mut self.fragment_ends, |it| u32::from(*it), scratch.take());
      });
      FRAGMENT_SORT.with_borrow_mut(|scratch| {
          radix::sort_copy_by_key_in(
              &mut self.fragment_spans,
              |&(range, _)| fragment_key(range),
              scratch.take(),
          )
      });
  }
  ```
  with `use std::cell::RefCell; use blaze_mem::scratch::Scratch;`. `sort_copy_by_key_in` is
  the same stable sort as `sort_copy_by_key` (`blaze-radix/src/lib.rs:196`), so the order is
  unchanged.
- `pub fn clear(&mut self)`: every column `clear()`ed (capacity kept), `by_anchor = OnceLock::new()`,
  `matched_arm = None`, `dropped_semicolon = None` (E13).
- `pub fn copy_in<B: Allocator + Clone>(&self, alloc: B) -> SpanMap<B>`: each column
  `Vec::with_capacity_in(len, alloc.clone())` + `extend_from_slice` (an empty one allocates
  nothing), `by_anchor` empty, `matched_arm` and `dropped_semicolon` copied. This replaces F3's
  `from_columns_in`: the sink builds in a map of its own, so the columns are a map's already.
- `pub fn reserve(&mut self, pieces: usize)` (`ends` and `spans`) and `pub fn capacity(&self) ->
  usize` (`ends.capacity()`), for the sink's kept map (3b).
- Test, in `map.rs`' tests:
  ```rust
  /// A finished map copied out has its columns at their exact lengths and is the same map, and
  /// finishing a warm map allocates nothing (the fragment sorts are the worker's).
  #[test]
  fn a_copied_map_is_exact_and_finishing_allocates_nothing_warm() {
      let span = |at: u32| Span {
          range: TextRange::at(TextSize::new(at), TextSize::new(1)),
          anchor: crate::SpanAnchor { file_id: file(), ast_id: ROOT_ERASED_FILE_AST_ID },
          ctx: SyntaxContext::root(crate::Edition::CURRENT),
      };
      let fill = |map: &mut SpanMap| {
          map.clear();
          for at in 1..=64u32 {
              map.push(TextSize::new(at), span(at));
          }
          for at in (1..60u32).step_by(7).rev() {
              map.push_fragment(TextRange::new(TextSize::new(at), TextSize::new(at + 3)), span(at));
          }
      };
      let mut map = SpanMap::with_capacity(64);
      fill(&mut map);
      map.finish();
      blaze_alloc_count::assert_at_most(0, "filling and finishing a warm map", || {
          fill(&mut map);
          map.finish();
      });
      let copy = map.copy_in(Global);
      assert!(copy == map);
      assert_eq!((copy.ends.capacity(), copy.fragment_spans.capacity()), (64, 9));
  }
  ```
  (`file()` is the tests' helper, L665; the nine fragments start at 1, 8, .., 57, pushed in
  reverse so that `finish` has to sort them; `#[cfg(feature = "salsa")]` is not needed.)

> forwarded to frontend-tree-span (requests, pass 23).

> frontend-tree-span (2026-10-29): 3a applied. `SpanMap<A: Allocator + Clone = Global>`, every
> column in `A`, `by_anchor` global; `empty`/`with_capacity` on `SpanMap` (global), everything
> else on `impl<A> SpanMap<A>`, `merge(.., other: &SpanMap<B>)`. `finish` no longer shrinks;
> `finish` and `merge` sort the fragments in `OFFSET_SORT`/`FRAGMENT_SORT` (`sort_fragments`).
> `clear`, `reserve`, `capacity`, `copy_in` as written. One difference: the test does need
> `#[cfg(feature = "salsa")]`, since `SyntaxContext::root` exists only with salsa. frontend-tokens
> told 3b can go ahead. Not built.

### 3b. frontend-tokens (`crates/syntax-bridge/src/lib.rs`, `TtTreeSink`, L1413-1456)

The sink pushes into the worker's map and copies it out at its length; the open fragments keep
their group's span, as frontend-tree-span's request asks.

```rust
thread_local! {
    /// The map a parse of a token tree fills, kept by the worker at the size it grew to.
    static SINK_MAP: Cell<Option<SpanMap>> = const { Cell::new(None) };
    /// The fragments a sink is in: where each starts and its invisible group's span.
    static OPEN_FRAGMENTS: RefCell<Scratch<(TextSize, Span)>> =
        const { RefCell::new(Scratch::new()) };
}

/// The most pieces whose map the worker keeps for the next parse.
const KEEP_MAP: usize = 1 << 16;
```
- `TtTreeSink`: `token_map: SpanMap` is the worker's (`SINK_MAP.take()` then `clear()` and
  `reserve(tokens)` (3a), or `SpanMap::with_capacity(tokens)` the first time on the thread);
  `open_fragments:
  Lent<Vec<u32>>` becomes `open_fragments: Vec<(TextSize, Span)>` taken from `OPEN_FRAGMENTS`
  (`std::mem::take(scratch.take())`, given back the same way in `finish`), so the start and the
  span travel together through `finish`'s closing of groups the cursor never left.
- The push (L1340 region, the `None =>` arm of `delim_to_str(subtree.delimiter.kind, false)`):
  `self.open_fragments.push((self.text_pos, subtree.delimiter.open));`.
- `close_fragment`:
  ```rust
  fn close_fragment(&mut self) {
      if let Some((start, span)) = self.open_fragments.pop()
          && start < self.token_end
      {
          self.token_map.push_fragment(TextRange::new(start, self.token_end), span);
      }
  }
  ```
- `finish` becomes `fn finish<A: Allocator + Clone>(mut self, alloc: A) -> (Parse<SyntaxNode>,
  SpanMap<A>)`: closes the open fragments as today, `self.token_map.finish()`, `let map =
  self.token_map.copy_in(alloc);`, gives `open_fragments` back to `OPEN_FRAGMENTS` and
  `token_map` to `SINK_MAP` when its `capacity()` (3a) is at most `KEEP_MAP`, and returns
  `(self.inner.finish(), map)`.
- `token_tree_to_syntax_node` passes `Global`; `token_tree_to_syntax_node_in(tt: SubtreeView<'_>,
  entry_point, span_to_edition, alloc: A) -> (Parse<SyntaxNode>, SpanMap<A>)` passes `alloc`
  (E11's reparse); `parse_tokens` takes the allocator through to the sink.
- Test, `crates/syntax-bridge/src/tests.rs`:
  ```rust
  /// A parse's map is the worker's map copied out: its columns at their exact lengths (glued
  /// punctuation makes fewer pieces than tokens, which `with_capacity(tokens)` overshot), the
  /// same map a cold parse gives, and the worker's map back on the thread for the next parse.
  #[test]
  fn a_parse_gives_its_map_at_its_length() {
      let text = "fn f() { a::b..=c; let d = (1, 2); }".repeat(50);
      let node = ast::SourceFile::parse(&text, Edition::CURRENT).syntax_node();
      let tree = token_tree(&node);
      let mut edition = |_: span::SyntaxContext| Edition::CURRENT;
      let (_, cold) =
          token_tree_to_syntax_node(tree.view(), parser::TopEntryPoint::SourceFile, &mut edition);
      let (_, warm) =
          token_tree_to_syntax_node(tree.view(), parser::TopEntryPoint::SourceFile, &mut edition);
      assert!(warm == cold);
      assert_eq!(warm.capacity(), warm.iter().count(), "the map's columns are exact");
      let kept = SINK_MAP.take().expect("the worker's map is back on the thread");
      assert!(kept.capacity() >= warm.capacity());
  }
  ```
  (`token_tree` is the tests' helper, as in `a_sink_gives_its_text_buffer_back`; `SINK_MAP` is
  `crate::SINK_MAP`, `pub(crate)` for this test.)

Why: `parse_macro_expansion`'s span map is a memo (contract 4: exact, never shrunk), the derive's
and eager's are temporaries (E11, E13). Today the sink's `SpanMap::with_capacity(tokens)` is
sized by input tokens and `finish` shrinks it whenever punctuation was glued (fewer pieces than
tokens), a reallocation per expansion, and the fragment sorts allocate their scratch globally.

---

> forwarded to frontend-tokens (requests, pass 23).
> 3b done by frontend-tokens (2026-10-29), with two differences:
> - `open_fragments` is a `SmallVec<[(TextSize, Span); 4]>` on the sink, not a thread-local
>   `Scratch`. It holds four open groups inline, so it allocates only for deeper nesting, and it
>   needs no thread-local take and give-back.
> - 2a (taking a view) is not applied, so `token_tree_to_syntax_node` keeps its `&TopSubtree`
>   and returns `SpanMap` (Global) through `token_tree_to_syntax_node_in(tt.view(), .., Global)`.
>   The new `token_tree_to_syntax_node_in(tt: SubtreeView<'_>, entry_point, span_to_edition,
>   alloc) -> (Parse, SpanMap<A>)` is ready for E11.
>
> Otherwise as filed:
> - `SINK_MAP` (`pub(crate)`) and `KEEP_MAP = 1 << 16`.
> - The sink takes the worker's map (`clear` + `reserve(tokens)`, or `with_capacity` the first
>   time). `finish<A>(alloc)` closes the open groups, calls `finish`, `copy_in(alloc)`, and gives
>   the map back when its capacity is at most `KEEP_MAP`.
> - `parse_tokens` takes the allocator, and `reference.rs` passes `Global`.
> - `#![feature(allocator_api)]` on syntax-bridge.
>
> Test `a_parse_gives_its_map_at_its_length` (it passes `&tree`, not `tree.view()`). Not built.
> Update (later the same day): 2a is applied too (answered under 2a), and the test now passes
> `tree.view()`.

## 4. frontend-tree-span (`crates/span/src/map.rs`): F4 as filed, with its callers

- `RealSpanMap::from_file(file_id, pairs: &[(TextSize, ErasedFileAstId)], end)`: the body
  unchanged (it reads `pairs` only). Callers in `map.rs`: `within` (L539) passes `&pairs`;
  `without` (L570) builds `pairs` in `blaze_mem::scratch` instead of a `Vec` it boxes:
  there is no shelf kind for `(TextSize, ErasedFileAstId)`, so a `thread_local!`
  `RefCell<Scratch<(TextSize, ErasedFileAstId)>>` `ANCHOR_PAIRS`, `with_borrow_mut(|s| { let
  pairs = s.take(); ..; RealSpanMap::from_file(self.file_id, pairs, ..) })`, and `within` the same
  (its `.collect::<Box<[_]>>()` becomes `pairs.extend(..)`). The tests (L677-978) pass `&pairs`
  / `&*pairs`.
- Callers outside span, each a one-token change: `hir-expand/src/span_map.rs:169`
  (`&pairs` for `pairs.into_boxed_slice()`), `:219` (`&anchors`), `hir-expand/src/budget_tests.rs:77`
  (`&anchors`). They are db-expand-macros' (E8 names the first).
- `pub fn absolute_span(file_id: EditionedFileId, range: TextRange) -> Span` (`#[cfg(feature =
  "salsa")]`, as `span_for_range`):
  ```rust
  /// The span [`RealSpanMap::absolute`]'s map gives `range`, with no map made: relative to the
  /// file's root.
  pub fn absolute_span(file_id: EditionedFileId, range: TextRange) -> Span {
      Span {
          range,
          anchor: crate::SpanAnchor { file_id, ast_id: ROOT_ERASED_FILE_AST_ID },
          ctx: SyntaxContext::root(file_id.edition()),
      }
  }
  ```
  (`absolute`'s map has one anchor at 0 with length `!0`, so `span_for_range` subtracts 0.)
- Test, in `map.rs`' tests:
  ```rust
  /// `absolute_span` is the absolute map's span, allocating nothing; `from_file` allocates its
  /// two columns alone.
  #[test]
  fn absolute_spans_need_no_map() {
      let map = RealSpanMap::absolute(file());
      for range in [TextRange::empty(0.into()), TextRange::new(3.into(), 90.into())] {
          let span = blaze_alloc_count::assert_at_most(0, "an absolute span", || {
              RealSpanMap::absolute_span(file(), range)
          });
          assert_eq!(span, map.span_for_range(range));
      }
      let pairs = [(TextSize::new(0), ROOT_ERASED_FILE_AST_ID), (TextSize::new(9), ErasedFileAstId::from_raw(1))];
      blaze_alloc_count::assert_at_most(2, "a map of two anchors", || {
          RealSpanMap::from_file(file(), &pairs, TextSize::new(20))
      });
  }
  ```

---

> forwarded to frontend-tree-span (told by tmux; requests, pass 23).

> frontend-tree-span (2026-10-29): 4 applied. `from_file` takes `&[_]`; `within` and `without`
> build their pairs in the worker's `ANCHOR_PAIRS` scratch; `absolute_span` as written (under
> `salsa`). The three hir-expand callers are filed in `v2/requests/db-expand-macros.md` (hir-expand
> does not compile until they pass slices). Test `absolute_spans_need_no_map`, which also holds a
> warm `within` + `without` to their four allocations. Not built.

## 5. frontend-tokens (`crates/syntax-bridge/src/lib.rs`): F5 over both converters

F5 named `syntax_node_to_token_tree_modified` and `Converter`. The view converter added since
(`call_input_modified`, L214, and `ViewConverter`, L1201) has the same map, set, deque and
callback, so it changes the same way. hashbrown's `nightly` feature is in the root manifest
already (`Cargo.toml:297`).

```rust
/// The leaves a conversion's callback inserts at one event: at most three, inline.
pub type Inserted = smallvec::SmallVec<[tt::Leaf; 3]>;
/// The leaves to append after an element (a `SyntaxElement`, or a view's `NodeId`), by element.
pub type Appended<K, A = Global> = hashbrown::HashMap<K, Inserted, rustc_hash::FxBuildHasher, A>;
/// The elements to leave out.
pub type Removed<K, A = Global> = hashbrown::HashSet<K, rustc_hash::FxBuildHasher, A>;
```
- `syntax_node_to_token_tree_modified<SpanMap, OnEvent, A: Allocator + Clone>(node, map, append:
  Appended<SyntaxElement, A>, remove: Removed<SyntaxElement, A>, call_site, mode, on_enter)`,
  `OnEvent: FnMut(&mut PreorderWithTokens, &WalkEvent<SyntaxElement>) -> (bool, Inserted)`.
- `call_input_modified<'t, SpanMap, OnEvent, A: Allocator + Clone>(node, map, append:
  Appended<syntax::view::NodeId, A>, remove: Removed<syntax::view::NodeId, A>, call_site, mode,
  on_event)`, `OnEvent: FnMut(&mut syntax::view::PreorderWithTokens<'t>,
  &WalkEvent<syntax::view::Element<'t>>) -> (bool, Inserted)`.
- `Converter` and `ViewConverter`: `append`/`remove` of those types; `current_leaves:
  VecDeque<tt::Leaf>` becomes `current_leaves: SmallVec<[tt::Leaf; 4]>`: `extend` appends as
  today, and the pop (L945, L1362), today `if let Some(leaf) =
  self.current_leaves.pop_front()`, is
  ```rust
  if !self.current_leaves.is_empty() {
      let leaf = self.current_leaves.remove(0);
      /* today's use of `leaf` */
  }
  ```
  The queue holds what one event inserted (at most three leaves, plus an append), so `remove(0)`
  shifts at most three; `tt::Leaf` is `Clone` but not `Copy`, and `remove` moves it out. The
  order of leaves is the deque's: appended at the back, taken from the front.
- `syntax_node_to_token_tree`'s callback (L162) returns `(true, Inserted::new())` (its `//
  alloc:` comment goes: an empty `SmallVec` allocates nothing) and passes `Default::default()`
  for `append`/`remove` (`Appended<SyntaxElement>`'s default is global and empty, no allocation).
- `use std::alloc::{Allocator, Global};`; `VecDeque` leaves the `std::collections` import.
- `crates/syntax-bridge/Cargo.toml`: `hashbrown.workspace = true` under `[dependencies]`
  (smallvec is there). Then `manifest_lint`.
- Callers: hir-expand's `derive_macro.rs:1087`, `cfg_process.rs:359` and `:390`, `fixup.rs:532`
  and `:550` (E3's, db-expand-macros'), and the tests `syntax-bridge/src/tests.rs:265`, `:307`,
  `:393`, `:417`: an `FxHashMap::default()` there becomes `Appended::default()`, an
  `FxHashSet::default()` `Removed::default()`, and a callback's `Vec<tt::Leaf>` an `Inserted`
  (`smallvec![..]` for `vec![..]`).
- Test, `crates/syntax-bridge/src/tests.rs`:
  ```rust
  /// A modified conversion whose callback inserts a leaf at every event allocates the same as
  /// one that inserts none: the inserted leaves are inline, and the maps are the caller's.
  #[test]
  fn inserted_leaves_allocate_nothing() {
      let text = "fn f() { let a = 1; let b = 2; }";
      let node = ast::SourceFile::parse(text, Edition::CURRENT).syntax_node();
      let leaf = tt::Leaf::Punct(tt::Punct { char: '#', spacing: tt::Spacing::Alone, span: DUMMY });
      let convert = |insert: bool| {
          let leaf = &leaf;
          let on_event = move |_: &mut PreorderWithTokens, _: &WalkEvent<SyntaxElement>| {
              // clone: a test's leaf, inline in the `SmallVec`.
              (true, if insert { smallvec::smallvec![leaf.clone()] } else { Inserted::new() })
          };
          syntax_node_to_token_tree_modified(
              &node,
              DummyTestSpanMap,
              Appended::default(),
              Removed::default(),
              DUMMY,
              DocCommentDesugarMode::Mbe,
              on_event,
          )
      };
      drop((convert(false), convert(true)));
      let (_, none) = blaze_alloc_count::measure(|| convert(false));
      let (_, some) = blaze_alloc_count::measure(|| convert(true));
      assert_eq!(none.allocations, some.allocations);
  }
  ```
  (Imports as the tests at L16-26 have them: `DUMMY`, `DummyTestSpanMap`. The output tree is
  longer with the inserted leaves but is one block either way, and the builder's buffers are
  warm from the first two conversions.)

---

> forwarded to frontend-tokens (told by tmux; requests, pass 23).
> done by frontend-tokens (2026-10-29), as filed: `Inserted`, `Appended<K, A>` and `Removed<K, A>`
> are exported from syntax-bridge. Both converters and both `_modified` functions are generic in
> `A`, and `current_leaves` is a `SmallVec<[tt::Leaf; 4]>`, popped with `remove(0)` behind an
> `is_empty` check. `syntax_node_to_token_tree`'s callback returns `(true, Inserted::new())`.
> `VecDeque`, `FxHashMap` and `FxHashSet` left lib.rs's imports, and `hashbrown.workspace = true`
> is in syntax-bridge's `[dependencies]`.
>
> Two differences from the request:
> - A function's type parameter takes no default, so an `Appended::default()` argument leaves
>   `A` to infer. Callers write `Appended::<_, Global>::default()`.
> - Test `inserted_leaves_allocate_nothing` uses `call_input_modified` on a view
>   (`parse.root()`) rather than `syntax_node_to_token_tree_modified` on a `SyntaxNode`, per the
>   coordinator's rule for new code.
>
> syntax-bridge's own test callers are updated. hir-expand's five are filed in
> `v2/requests/db-expand-macros.md`. manifest_lint is clean, and hot_lint finds nothing in
> lib.rs. Not built.

## The rest of F

- **F6**: mem-format's own `AstIdMap` request, `v2/requests/mem-format.md` section 4, with
  frontend-tree-span (item 1 of its brief). Nothing more.
- **F7**: merged into `mem-format-tokens.md`'s frame section, as its addendum (below that
  section's notes): the transcriber builds in the allocator it is given, so the wrapper needs no
  copy.
- **F8**: nothing to do: `blaze_radix::sort_copy_by_key_in` exists (`lib.rs:196`).
- **F9**: already true. Since F9 was filed, tt's one-block rewrite keeps the encoder (its parts
  table, wide spans and symbols) in the builder's per-thread `Scratch` (`storage.rs:878`, cleared
  by `give_back`), and `from_columns` allocates the tree's one block at its exact length
  (`Vec::with_capacity(layout.end)`, then `into_boxed_slice` at `len == capacity`). The budget is
  `walking_and_the_wire_allocate_the_same_for_any_size_of_tree`'s "building a tree with a warm
  builder" (`storage/tests.rs:846`, at most 1). F9's test, which counts four columns, has nothing
  left to pin.

> forwarded to frontend-tokens and frontend-tree-span (requests, pass 23).
> frontend-tokens (2026-10-29): F7 (the addendum in `mem-format-tokens.md`) goes to child
> `frontend-tokens-alloc` with section 1, which it needs. F8 and F9 need nothing. F6 is
> frontend-tree-span's.
