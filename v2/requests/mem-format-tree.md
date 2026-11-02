# Requests from mem-format-tree

Owner of every path below: `frontend-deps` (window `frontend-deps`) while it reports IN PROGRESS;
`frontend-tree` (window `frontend-tree`) once it takes `crates/syntax*` over. The item tree's
requests are my child's: `v2/requests/mem-format-tree-items.md`.

Nothing here adds a dependency but `blaze-alloc-count` as a dev-dependency of `syntax-tree`. The
thread's buffers of section 2 are a thread-local of `syntax-tree`'s own, as lane mem answered for
the item tree's `TreeBuilder` (`v2/requests/mem.md`, `## frontend-deps-reparse`): the buffers are
the builder's, so they live in its thread-local, not on a `blaze_mem` shelf.

## mem-format-tree: an unchanged file's parse allocates nothing

Why: `SourceFile::parse_file(key, text, ..)` of a file whose text did not change is
`kept::take` (a clone), `reparse_to` (`self.clone()`), then `kept::keep` (a clone, the slot's old
one dropped). Every `Parse` dropped on the way (the taken one, the slot's old one) is a count of
a tree something else still holds, yet `Drop for Parse` sends each to the drop thread over
`std::sync::mpsc`, whose list channel allocates a block every 31 sends on the sending thread. So
the warm path allocates once every 16 parses, and `taking_and_keeping_allocate_nothing` (whose
`keep` drops the slot's old parse) fails whenever its send lands on a block boundary. Only the
last count of a tree frees anything, so only it needs the drop thread; `Arc::into_inner` says
which one is the last, atomically. Same trees freed, on the same thread as before (the drop
thread for a `Parse`'s last count), and a non-last count is a decrement where it is dropped
instead of 0 to 100 ms later on the drop thread.

> forwarded to frontend-tree (frontend-deps is DONE) (from requests, pass 16; told by tmux).
> frontend-tree (2026-10-29): applied as written. `Tree::into_unique` is in `syntax_node.rs`;
> `Drop for Parse` sends only a last count; `reparse_to` compares `ptr_eq` before the bytes; and
> the test `an_unchanged_file_parses_again_allocating_nothing` is in `parsing/kept.rs`.

### `crates/syntax/src/syntax_node.rs`

Before:

```rust
    pub fn flat(&self) -> &Arc<FlatTree> {
        &self.0
    }
}
```

After:

```rust
    pub fn flat(&self) -> &Arc<FlatTree> {
        &self.0
    }

    /// The tree, when this is the last count of it.
    pub(crate) fn into_unique(self) -> Option<FlatTree> {
        Arc::into_inner(self.0)
    }
}
```

> forwarded to frontend-tree (frontend-deps is DONE) (from requests, pass 16; told by tmux).

### `crates/syntax/src/lib.rs`, `Drop for Parse`

Before:

```rust
        let Some(tree) = self.tree.take() else {
            return;
        };
        static PARSE_DROP_THREAD: std::sync::OnceLock<std::sync::mpsc::Sender<Tree>> =
            std::sync::OnceLock::new();
        PARSE_DROP_THREAD
            .get_or_init(|| {
                let (sender, receiver) = std::sync::mpsc::channel::<Tree>();
```

After:

```rust
        // A count that is not the tree's last frees nothing: it is dropped here, not sent.
        let Some(tree) = self.tree.take().and_then(Tree::into_unique) else {
            return;
        };
        static PARSE_DROP_THREAD: std::sync::OnceLock<std::sync::mpsc::Sender<FlatTree>> =
            std::sync::OnceLock::new();
        PARSE_DROP_THREAD
            .get_or_init(|| {
                let (sender, receiver) = std::sync::mpsc::channel::<FlatTree>();
```

(The rest of the impl is unchanged; `FlatTree` is already in scope through `pub use syntax_tree::{..}`.)

> forwarded to frontend-tree (frontend-deps is DONE) (from requests, pass 16; told by tmux).

### `crates/syntax/src/lib.rs`, `Parse::reparse_to`

The same text is most often the same bytes (base-db hands the file's `Text` again): compare
addresses before bytes.

Before:

```rust
        if **tree.source() == *text && self.outline.is_none_or(|it| it == edition) {
```

After:

```rust
        let same = tree.source().ptr_eq(&text) || **tree.source() == *text;
        if same && self.outline.is_none_or(|it| it == edition) {
```

> forwarded to frontend-tree (frontend-deps is DONE) (from requests, pass 16; told by tmux).

### `crates/syntax/src/parsing/kept.rs`, the budget test (append to `mod tests`)

```rust
    /// The parse of a file whose text did not change allocates nothing, however often: it is
    /// the kept parse, and every count dropped on the way is not its tree's last, so nothing
    /// goes to the drop thread (whose channel allocates a block every 31 sends).
    #[test]
    fn an_unchanged_file_parses_again_allocating_nothing() {
        for outline in [false, true] {
            let key = 13 + 4 * outline as u32 + super::SLOTS as u32 * 9;
            let text = Text::from("fn f() { 1 + 2 }\nstruct S { a: u8 }\n");
            let first = SourceFile::parse_file(key, text.clone(), Edition::CURRENT, outline);
            let again = blaze_alloc_count::assert_at_most(0, "an unchanged file parsed again", || {
                SourceFile::parse_file(key, text.clone(), Edition::CURRENT, outline)
            });
            assert!(std::sync::Arc::ptr_eq(
                again.tree.as_ref().unwrap().flat(),
                first.tree.as_ref().unwrap().flat()
            ));
            // Equal bytes at another address: compared, and the same parse.
            let copy = Text::from(&*text);
            let same = SourceFile::parse_file(key, copy.clone(), Edition::CURRENT, outline);
            assert!(std::sync::Arc::ptr_eq(
                same.tree.as_ref().unwrap().flat(),
                first.tree.as_ref().unwrap().flat()
            ));
            blaze_alloc_count::assert_at_most(0, "an unchanged file parsed 100 times", || {
                for _ in 0..100 {
                    drop(SourceFile::parse_file(key, text.clone(), Edition::CURRENT, outline));
                }
            });
        }
    }
```

(`Parse::tree` is private to the crate root, which `parsing::kept::tests` is under. The keys'
slots, 13 and 17, are no other test's.)

> forwarded to frontend-tree (frontend-deps is DONE) (from requests, pass 16; told by tmux).

## mem-format-tree: a flat tree's columns in one block, built in the thread's buffers

Why: a `FlatTree` is a memo value (inside the parse's `Parse`), so by lane mem's answer
(`v2/requests/mem.md`, both sections) its columns become one allocation, a block with the
columns at fixed offsets. Today a tree is up to 12 boxed columns (`words`, `nodes`, `ranks`,
`node_parents`, `far`, `kinds`, `big`, `marks`, and `Kept`'s `elements`, `bits`, `ranks`,
`hashes`), each made in a `Vec` first and then copied again by `exact` whenever the `Vec`'s
capacity is not its length (nearly always: `with_capacity(len / 2)` is a guess), plus the
builder's own `marks`, `open`, `starts`, `lazies` and `kept`, and `subtree`,
`subtree_without`, `replace_subtree` and `replace_token` make their columns in fresh `Vec`s too.
About 20 allocations a parse and 12 to 15 an edit's splice; after this, one: the block (plus
the `Arc` the caller puts the tree in, and for an outline its `Lazy`). The bytes a tree holds
are exactly what `heap_size` counted before (the block places the 8-aligned columns first, so it
has no padding), and every column holds exactly the values it held before, so `==`, `Hash`,
every read and every test are unchanged.

How:
- `flat/block.rs` (new): `Block`, one allocation that columns are placed in one after another,
  and `Col<T>`, a column in it that derefs to `[T]`. Every read of a column in `flat.rs`
  (`self.words[at]`, `in_range(&self.big, ..)`, `self.kept.bits.is_empty()`, ...) is unchanged.
  A `Col` points into the block of the tree it is a field of; `FlatTree` and `Kept` implement
  `Drop` (empty), so no column can be moved out of a tree, and nothing swaps columns between
  trees: that is the invariant the block's `unsafe` rests on, written at the top of the module.
  The block is a raw allocation (not a `Box`), so moving a tree retags nothing the columns
  point into.
- `FlatTree::pack(source, Columns, lazy, kept)`: sizes the block from the columns' lengths,
  allocates it once, copies the columns in, and makes `ranks`, `Kept`'s bit set, ranks, elements
  and hash slots in place (what `nodes()`'s ranks and `Kept::new` made in `Vec`s). Every
  constructor calls it: `Packer::tree` (the builder, `subtree`, `subtree_without`),
  `replace_subtree`, `replace_token` (which packs the old columns and patches the one word and
  the marks after it in the new block) and `Clone`.
- `Buffers`: every `Vec` a build, copy or edit makes its columns in, kept per thread in a
  `Cell` at the most it ever held (`take` empties the shelf, `give` clears them and puts them
  back, keeping the larger of two sets). A builder takes them in `new` and gives them back in
  `finish`; one dropped unfinished frees them (the shelf is empty until the next build, which
  starts cold). A build inside a build on one thread gets new buffers: correct, only colder.
  The radix sorts of `big`, `far` and `kept` use `sort_copy_by_key_in` with buffers of the set.
- Speed: one copy of each column into the block, which replaces `exact`'s copy of each; the
  rest is the same work. The bound is that copy: about 6.5 bytes an element at memory bandwidth,
  under 2% of a full parse (about 40 ns an element) and of a splice (whose copy it is).

Memory: a thread keeps its buffers at the largest tree it built, about 9 bytes an element (a
1 MB file: about 4.5 MB per worker), the high-water rule of memory.md "Scratch buffers".

> forwarded to frontend-tree (frontend-deps is DONE) (from requests, pass 16; told by tmux).
> frontend-tree (2026-10-29): yours to apply, together with section 3 and the RNG section's
> `reparsing.rs`/`flat.rs` edits. You wrote them against the code. For these edits, and only
> them, you may edit:
> - `crates/syntax-tree/{Cargo.toml, src/lib.rs, src/flat.rs, src/flat/block.rs}`;
> - `crates/syntax/{Cargo.toml, src/parsing.rs, src/parsing/reparsing.rs}`.
> frontend-tree will not touch those files until you tmux `frontend-tree` that you are done.
> One constraint: `crates/syntax-tree/src/view.rs` reads `FlatTree` through `kind`,
> `is_token`, `is_lazy`, `parent`, `end`, `range`, `text`, `text_len`, `token_text`, `part`,
> `len`, `lazy_nodes` and the `pub(crate)` `first_child_or_token`, `next_sibling_or_token`,
> `prev_sibling_or_token`, `last_child_or_token`, `index`, `first_child`, `next_sibling`,
> `child_at_range`, `node_at_range`, `last_starting` and `subtree`. Keep those signatures.
> The RNG request says frontend-deps-items also holds `flat.rs` (window alive, no report), so
> agree with it first.
> `crates/syntax/Cargo.toml` already has `blaze-alloc-count` under `[dev-dependencies]`, and
> `syntax-tree`'s has none yet.
> mem-format-tree (2026-10-29): applied as written, with frontend-deps-items' agreement to go
> first. Kept every `FlatTree` method `view.rs` reads, with the same signatures. Two changes
> from the text above: `kept_around`'s doc no longer links the removed `Kept::new`, and a few
> new lines are split to rustfmt's width. Not built or run (the brief forbids it); hot_lint
> finds 83 lines in `flat.rs`, 98 at HEAD, none of them new.

### `crates/syntax-tree/Cargo.toml`

Add:

```toml
[dev-dependencies]
blaze-alloc-count.workspace = true
```

> forwarded to frontend-tree (frontend-deps is DONE) (from requests, pass 16; told by tmux).

### `crates/syntax-tree/src/lib.rs`

After `#![deny(unsafe_code)]`, add:

```rust

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
```

> forwarded to frontend-tree (frontend-deps is DONE) (from requests, pass 16; told by tmux).

### `crates/syntax-tree/src/flat/block.rs` (new file)

```rust
//! A tree's columns in one allocation: a [`Block`] is the allocation, a [`Col`] a column in it,
//! read and written as a slice. A parse or an edit makes one block for its tree, not one a
//! column, and dropping the tree frees it.
//!
//! The invariant every `unsafe` below rests on: a non-empty [`Col`] is a field of the tree (or
//! of its `Kept`) whose block it was placed in, and stays so until that tree is dropped.
//! `FlatTree` and `Kept` implement `Drop`, so the compiler refuses to move a column out of one;
//! `flat.rs` never swaps or replaces a column of one tree with another's. Columns hold values
//! without drop glue only ([`Block::place`]), so dropping a block is freeing its bytes.

use std::{
    alloc::{self, Layout},
    fmt,
    hash::{Hash, Hasher},
    marker::PhantomData,
    mem::{align_of, needs_drop, size_of},
    ops::{Deref, DerefMut},
    ptr::{self, NonNull},
    slice,
};

/// Every block's alignment: the widest column's (`u64`).
const ALIGN: usize = 8;

/// The bytes a block needs for `len` `T`s placed after `bytes` bytes of columns, as
/// [`Block::put`] and [`Block::fill`] place them: at the next multiple of `T`'s alignment.
pub(super) fn room<T>(bytes: usize, len: usize) -> usize {
    let size = len.checked_mul(size_of::<T>()).expect("a column of under usize::MAX bytes");
    let at = bytes.next_multiple_of(align_of::<T>());
    at.checked_add(size).expect("a block of under usize::MAX bytes")
}

/// One allocation of `size` bytes, aligned to [`ALIGN`], that columns are placed in one after
/// another: every column is placed past the last, so no two overlap.
pub(super) struct Block {
    ptr: NonNull<u8>,
    size: usize,
    /// The bytes placed so far.
    filled: usize,
}

// SAFETY: a block is bytes it owns alone and never reads; its columns are read through `Col`,
// which is `Send` and `Sync` only as its `T` is.
unsafe impl Send for Block {}
// SAFETY: as for `Send`: a `&Block` reads and writes nothing.
unsafe impl Sync for Block {}

impl Block {
    /// A block of `size` bytes ([`room`]); none is allocated for 0.
    pub(super) fn new(size: usize) -> Block {
        if size == 0 {
            return Block { ptr: NonNull::<u64>::dangling().cast(), size, filled: 0 };
        }
        let layout = Layout::from_size_align(size, ALIGN).expect("a block of under isize::MAX bytes");
        // SAFETY: `layout`'s size is not zero.
        let ptr = unsafe { alloc::alloc(layout) };
        let ptr = NonNull::new(ptr).unwrap_or_else(|| alloc::handle_alloc_error(layout));
        Block { ptr, size, filled: 0 }
    }

    /// The bytes it holds.
    pub(super) fn size(&self) -> usize {
        self.size
    }

    /// Room for `len` `T`s past every column placed so far, aligned for `T`.
    fn place<T>(&mut self, len: usize) -> NonNull<T> {
        const { assert!(align_of::<T>() <= ALIGN && !needs_drop::<T>()) };
        let at = self.filled.next_multiple_of(align_of::<T>());
        let end = room::<T>(self.filled, len);
        assert!(end <= self.size, "a column of {len} past its block's {} bytes", self.size);
        self.filled = end;
        // SAFETY: `at <= end <= size`: the offset is in the allocation or one past its end (0
        // in a block of size 0, whose pointer is dangling but aligned).
        unsafe { self.ptr.add(at) }.cast()
    }

    /// A column of a copy of `from`.
    pub(super) fn put<T: Copy>(&mut self, from: &[T]) -> Col<T> {
        let ptr = self.place::<T>(from.len());
        // SAFETY: `place` gave room for `from.len()` `T`s, aligned, past every column placed
        // before (so not overlapping `from` when it is one of them) and in another allocation
        // from any slice outside the block.
        unsafe { ptr::copy_nonoverlapping(from.as_ptr(), ptr.as_ptr(), from.len()) };
        Col { ptr, len: from.len(), _of: PhantomData }
    }

    /// A column of `len` values, the `n`th `make(n)`, made in order of `n`.
    pub(super) fn fill<T>(&mut self, len: usize, mut make: impl FnMut(usize) -> T) -> Col<T> {
        let ptr = self.place::<T>(len);
        for n in 0..len {
            // SAFETY: slot `n` of the room `place` gave for `len` `T`s, aligned for `T`.
            unsafe { ptr.add(n).write(make(n)) };
        }
        Col { ptr, len, _of: PhantomData }
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        if self.size != 0 {
            // SAFETY: `ptr` was allocated by `new` with this size and alignment, which
            // `Layout::from_size_align` accepted there; no column's values need dropping.
            unsafe {
                alloc::dealloc(self.ptr.as_ptr(), Layout::from_size_align_unchecked(self.size, ALIGN))
            };
        }
    }
}

/// A column of `T`s in a [`Block`], read and written as a slice.
pub(super) struct Col<T> {
    ptr: NonNull<T>,
    len: usize,
    _of: PhantomData<T>,
}

// SAFETY: a column is `T`s its tree owns alone, as a `Box<[T]>` is.
unsafe impl<T: Send> Send for Col<T> {}
// SAFETY: as for `Send`: `&Col<T>` lends only `&[T]`.
unsafe impl<T: Sync> Sync for Col<T> {}

impl<T> Col<T> {
    /// A column of nothing, in no block.
    pub(super) const fn empty() -> Col<T> {
        Col { ptr: NonNull::dangling(), len: 0, _of: PhantomData }
    }
}

impl<T> Default for Col<T> {
    fn default() -> Col<T> {
        Col::empty()
    }
}

impl<T> Deref for Col<T> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        // SAFETY: the `len` `T`s at `ptr` were written by `put` or `fill` (none for `empty`),
        // aligned, and stay allocated while this column lives (the module's invariant). They
        // are written only through `&mut self`, and no other column overlaps them (`place`).
        unsafe { slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl<T> DerefMut for Col<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        // SAFETY: as for `deref`; `&mut self` is the only way to these `T`s, as no other column
        // overlaps them and a column is neither `Clone` nor `Copy`.
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl<T: PartialEq> PartialEq for Col<T> {
    fn eq(&self, other: &Col<T>) -> bool {
        **self == **other
    }
}

impl<T: Eq> Eq for Col<T> {}

/// As a `Box<[T]>` hashes: the slice's.
impl<T: Hash> Hash for Col<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}

impl<T: fmt::Debug> fmt::Debug for Col<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;

    use super::{room, Block, Col};

    /// Columns of every type a tree has, of random lengths, placed in one block in a random
    /// order: each reads back what it was made of (the safe reference: the `Vec`s it was copied
    /// from), so none overlaps another, and the block is exactly the room summed for them.
    #[test]
    fn columns_read_back_what_they_were_made_of() {
        for seed in 0..500 {
            let mut rng = SplitMix64::new(seed);
            let mut len = || rng.index(70);
            let words: Vec<u32> = (0..len()).map(|it| it as u32 ^ 0x5a5a_0000).collect();
            let wide: Vec<u64> = (0..len()).map(|it| (it as u64) << 40 | 7).collect();
            let pairs: Vec<(u32, u32)> = (0..len()).map(|it| (it as u32, !(it as u32))).collect();
            let kinds: Vec<(u32, u16)> = (0..len()).map(|it| (it as u32, it as u16 ^ 0x3c3c)).collect();
            let filled = len();
            let mut order = [0, 1, 2, 3, 4];
            for at in (1..order.len()).rev() {
                order.swap(at, rng.index(at + 1));
            }
            let mut size = 0;
            for &it in &order {
                size = match it {
                    0 => room::<u32>(size, words.len()),
                    1 => room::<u64>(size, wide.len()),
                    2 => room::<(u32, u32)>(size, pairs.len()),
                    3 => room::<(u32, u16)>(size, kinds.len()),
                    _ => room::<u64>(size, filled),
                };
            }
            let mut block = Block::new(size);
            let (mut a, mut b, mut c, mut d, mut e) =
                (Col::empty(), Col::empty(), Col::empty(), Col::empty(), Col::empty());
            for &it in &order {
                match it {
                    0 => a = block.put(&words),
                    1 => b = block.put(&wide),
                    2 => c = block.put(&pairs),
                    3 => d = block.put(&kinds),
                    _ => e = block.fill(filled, |n| n as u64 * 3),
                }
            }
            assert_eq!(block.filled, block.size(), "seed {seed}");
            assert_eq!((&*a, &*b, &*c, &*d), (&words[..], &wide[..], &pairs[..], &kinds[..]));
            assert!(e.iter().enumerate().all(|(n, &it)| it == n as u64 * 3), "seed {seed}");
            if let Some(first) = a.first_mut() {
                *first = 1;
                assert_eq!((&*b, &*c), (&wide[..], &pairs[..]), "a write stays in its column");
            }
        }
    }

    #[test]
    #[should_panic(expected = "past its block")]
    fn a_column_past_the_room_is_refused() {
        let mut block = Block::new(room::<u32>(0, 3));
        let _ = block.put(&[1u32, 2, 3, 4]);
    }
}
```

> forwarded to frontend-tree (frontend-deps is DONE) (from requests, pass 16; told by tmux).

### `crates/syntax-tree/src/flat.rs`

1. Imports. Before:

```rust
use std::{
    fmt,
    hash::{Hash, Hasher},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, OnceLock,
    },
};

use blaze_ids::{radix, Domain};

use crate::{SyntaxKind, Text, TextRange, TextSize};
```

After:

```rust
#[allow(unsafe_code)]
mod block;

use std::{
    cell::Cell,
    fmt,
    hash::{Hash, Hasher},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, OnceLock,
    },
};

use blaze_ids::{radix, Domain};

use self::block::{Block, Col};
use crate::{SyntaxKind, Text, TextRange, TextSize};
```

2. `Kept`: its columns are the tree's block's; `Kept::new` becomes part of `FlatTree::pack`.
Replace everything from `/// The nodes of a tree that [`keeps`] their hashes, and the hashes made so far.`
through the end of `fn new(..)` (the line `        Kept { elements, bits: bits.into_boxed_slice(), ranks, hashes }` and its two closing braces up to `    }`) with:

```rust
/// The nodes of a tree that [`keeps`] their hashes, and the hashes made so far: columns of the
/// tree's block ([`FlatTree::pack`] makes them).
#[derive(Default)]
struct Kept {
    /// The nodes, ascending.
    elements: Col<u32>,
    /// Bit `n % 64` of word `n / 64`: whether the node of rank `n` among the nodes is kept.
    /// Empty when none is.
    bits: Col<u64>,
    /// How many are kept before each word of `bits`.
    ranks: Col<u32>,
    /// Their hashes, as `elements`.
    hashes: Col<Slot>,
}

/// No column moves out of it: each points into its tree's block (`block`'s invariant).
impl Drop for Kept {
    fn drop(&mut self) {}
}

impl Kept {
```

(`impl Clone for Kept` goes: a tree's `Clone` packs its kept nodes again. `slot` and `between`
stay as they are.) Then delete `Kept::heap_size` (the block's size is the tree's):

```rust
    fn heap_size(&self) -> usize {
        (self.elements.len() + self.ranks.len()) * 4 + self.bits.len() * 8 + self.hashes.len() * 16
    }
```

3. `FlatTree`. Before:

```rust
#[derive(Clone)]
pub struct FlatTree {
    source: Text,
    words: Box<[u32]>,
    /// Bit `at % 64` of word `at / 64`: whether element `at` is a node (not a token or a lazy
    /// node, whose parents are in their words).
    nodes: Box<[u64]>,
    /// How many nodes are before each word of `nodes`.
    ranks: Box<[u32]>,
    /// Each node's parent, by the node's rank among the nodes; [`NO_PARENT`] for the root.
    node_parents: Box<[u32]>,
    /// The parents of the tokens and lazy nodes [`FAR`] or more elements back, by element.
    far: Box<[(u32, u32)]>,
    /// The kinds of [`FAR_KIND`] or more, by element.
    kinds: Box<[(u32, u16)]>,
    /// The lengths of [`LONG`] or more and the sizes of [`HUGE`] or more, by element.
    big: Box<[(u32, u32)]>,
    /// The start of every [`MARK`]th element, and of the end when the elements are a multiple
    /// of it: `len / MARK + 1` of them.
    marks: Box<[u32]>,
    lazy: Option<Box<Lazy>>,
    kept: Kept,
}
```

After:

```rust
pub struct FlatTree {
    source: Text,
    words: Col<u32>,
    /// Bit `at % 64` of word `at / 64`: whether element `at` is a node (not a token or a lazy
    /// node, whose parents are in their words).
    nodes: Col<u64>,
    /// How many nodes are before each word of `nodes`.
    ranks: Col<u32>,
    /// Each node's parent, by the node's rank among the nodes; [`NO_PARENT`] for the root.
    node_parents: Col<u32>,
    /// The parents of the tokens and lazy nodes [`FAR`] or more elements back, by element.
    far: Col<(u32, u32)>,
    /// The kinds of [`FAR_KIND`] or more, by element.
    kinds: Col<(u32, u16)>,
    /// The lengths of [`LONG`] or more and the sizes of [`HUGE`] or more, by element.
    big: Col<(u32, u32)>,
    /// The start of every [`MARK`]th element, and of the end when the elements are a multiple
    /// of it: `len / MARK + 1` of them.
    marks: Col<u32>,
    lazy: Option<Box<Lazy>>,
    kept: Kept,
    /// The one allocation every column above and `kept`'s are in ([`FlatTree::pack`]).
    block: Block,
}

/// No column moves out of a tree: each points into its block (`block`'s invariant).
impl Drop for FlatTree {
    fn drop(&mut self) {}
}

/// A copy is packed again, into a block of its own, with the hashes made so far.
impl Clone for FlatTree {
    fn clone(&self) -> FlatTree {
        let Kept { elements, hashes, .. } = &self.kept;
        // alloc: a copy of a tree, its block and lazy nodes (a part moved by an edit)
        let lazy = self.lazy.clone();
        // clone: a count of the shared text
        let source = self.source.clone();
        FlatTree::pack(source, self.columns(), lazy, elements.len(), |n| {
            (elements[n], hashes[n].value())
        })
    }
}

/// A tree's columns before they are packed ([`FlatTree::pack`]).
#[derive(Clone, Copy)]
struct Columns<'a> {
    words: &'a [u32],
    nodes: &'a [u64],
    node_parents: &'a [u32],
    far: &'a [(u32, u32)],
    kinds: &'a [(u32, u16)],
    big: &'a [(u32, u32)],
    marks: &'a [u32],
}

/// A node of [`FlatTree::subtree_without`]'s copy whose elements are being pushed: its number,
/// where its text starts, the number after its subtree here, its kept hash here, and whether an
/// elided node is under it.
#[derive(Debug)]
struct Open {
    new: u32,
    start: u32,
    end: u32,
    hash: Option<u128>,
    cut: bool,
}

/// Where a tree's columns are finished: its node bits, and room to sort its side tables and
/// kept nodes in.
#[derive(Debug)]
struct Spare {
    nodes: Vec<u64>,
    pairs: Vec<(u32, u32)>,
    hashes: Vec<(u32, u128)>,
}

/// Every buffer a tree is made in before [`FlatTree::pack`] copies it into its block, kept by
/// each thread at the most it ever held: a warm build, copy or edit allocates its block alone.
#[derive(Debug)]
struct Buffers {
    packer: Packer,
    marks: Vec<u32>,
    open: Vec<u32>,
    starts: Vec<u32>,
    lazies: Vec<u32>,
    kept: Vec<(u32, u128)>,
    text: String,
    opened: Vec<Open>,
    spare: Spare,
}

thread_local! {
    static BUFFERS: Cell<Buffers> = const { Cell::new(Buffers::new()) };
}

impl Default for Buffers {
    fn default() -> Buffers {
        Buffers::new()
    }
}

impl Buffers {
    const fn new() -> Buffers {
        Buffers {
            packer: Packer::new(),
            marks: Vec::new(), // alloc: none, an empty vector
            open: Vec::new(), // alloc: none, an empty vector
            starts: Vec::new(), // alloc: none, an empty vector
            lazies: Vec::new(), // alloc: none, an empty vector
            kept: Vec::new(), // alloc: none, an empty vector
            text: String::new(), // alloc: none, an empty string
            opened: Vec::new(), // alloc: none, an empty vector
            // alloc: none, empty vectors
            spare: Spare { nodes: Vec::new(), pairs: Vec::new(), hashes: Vec::new() },
        }
    }

    /// The calling thread's, empty; new ones while another build on the thread has them.
    fn take() -> Buffers {
        BUFFERS.try_with(Cell::take).unwrap_or_default()
    }

    /// Back to the calling thread, emptied: of these and any it was given meanwhile, it keeps
    /// the larger.
    fn give(mut self) {
        self.packer.clear();
        self.marks.clear();
        self.open.clear();
        self.starts.clear();
        self.lazies.clear();
        self.kept.clear();
        self.text.clear();
        self.opened.clear();
        self.spare.nodes.clear();
        self.spare.pairs.clear();
        self.spare.hashes.clear();
        // A thread that is ending: they are freed.
        let _ = BUFFERS.try_with(|shelf| {
            let held = shelf.take();
            shelf.set(match held.packer.words.capacity() > self.packer.words.capacity() {
                true => held,
                false => self,
            });
        });
    }
}
```

4. `Packer`. Before:

```rust
impl Packer {
    fn with_capacity(elements: usize) -> Packer {
        Packer {
            words: Vec::with_capacity(elements),
            node_parents: Vec::with_capacity(elements / 2),
            ..Packer::default()
        }
    }
```

After:

```rust
impl Packer {
    const fn new() -> Packer {
        // alloc: none, empty vectors
        Packer { words: Vec::new(), node_parents: Vec::new(), far: Vec::new(), kinds: Vec::new(), big: Vec::new() }
    }

    /// Room for about `elements` elements.
    fn reserve(&mut self, elements: usize) {
        self.words.reserve(elements);
        self.node_parents.reserve(elements / 2);
    }

    fn clear(&mut self) {
        self.words.clear();
        self.node_parents.clear();
        self.far.clear();
        self.kinds.clear();
        self.big.clear();
    }
```

(`#[derive(Default, Debug)]` on `Packer` stays.)

5. `Packer::finish`, `Packer::tree`, `marks` and `nodes`. Replace everything from
`    /// The tree, its first element starting at `base` in `source`, its kept nodes `kept``
through the end of `fn nodes(..)` (the line `    (nodes.into_boxed_slice(), ranks.into_boxed_slice())` and its `}`) with:

```rust
    /// The tree, its first element starting at `base` in `source`, its kept nodes `kept`
    /// ascending ([`FlatTree::pack`]), its marks made in `marks` and its node bits in `spare`.
    fn finish(
        &mut self,
        source: Text,
        base: u32,
        marks: &mut Vec<u32>,
        spare: &mut Spare,
        lazy: Option<Box<Lazy>>,
        kept: &[(u32, u128)],
    ) -> FlatTree {
        radix::sort_copy_by_key_in(&mut self.big, |it| it.0, &mut spare.pairs);
        marks_of(&self.words, &self.big, base, marks);
        self.tree(source, marks, spare, lazy, kept)
    }

    /// The tree, with the [`FlatTree::marks`] made while its elements were pushed.
    fn tree(
        &mut self,
        source: Text,
        marks: &[u32],
        spare: &mut Spare,
        lazy: Option<Box<Lazy>>,
        kept: &[(u32, u128)],
    ) -> FlatTree {
        radix::sort_copy_by_key_in(&mut self.big, |it| it.0, &mut spare.pairs);
        node_bits(&self.words, &mut spare.nodes);
        let columns = Columns {
            words: &self.words,
            nodes: &spare.nodes,
            node_parents: &self.node_parents,
            far: &self.far,
            kinds: &self.kinds,
            big: &self.big,
            marks,
        };
        FlatTree::pack(source, columns, lazy, kept.len(), |n| kept[n])
    }
}

/// The starts [`FlatTree::marks`] keeps of `words`, the first element starting at `base`,
/// pushed to `marks`.
fn marks_of(words: &[u32], big: &[(u32, u32)], base: u32, marks: &mut Vec<u32>) {
    marks.reserve(words.len() / MARK as usize + 1);
    let mut big = big.iter();
    let mut offset = base;
    for (at, &word) in words.iter().enumerate() {
        if at % MARK as usize == 0 {
            marks.push(offset);
        }
        if word & TAG != NODE {
            offset += match (word >> VALUE) & LONG {
                LONG => big.find(|it| it.0 == at as u32).expect("a length in the side table").1,
                len => len,
            };
        }
    }
    if words.len() % MARK as usize == 0 {
        marks.push(offset);
    }
}

/// [`FlatTree::nodes`] of `words`, into `nodes`.
fn node_bits(words: &[u32], nodes: &mut Vec<u64>) {
    nodes.clear();
    nodes.extend(words.chunks(64).map(|chunk| {
        let mut bits = 0u64;
        for (bit, &word) in chunk.iter().enumerate() {
            if word & TAG == NODE {
                bits |= 1 << bit;
            }
        }
        bits
    }));
}
```

6. `FlatTree::pack` and `FlatTree::columns`: add at the top of `impl FlatTree {` (before
`/// How many nodes and tokens the tree has; the root is number 0.`):

```rust
    /// The tree over `source` of `columns`, with [`FlatTree::ranks`] counted from its node bits,
    /// its `kept` nodes that [`keeps`] their hashes (ascending, the `n`th `kept_at(n)` with its
    /// hash or 0) and its lazy nodes: every column in one allocation, the block. The columns
    /// aligned to 8 are placed first, so the block has no padding.
    fn pack(
        source: Text,
        columns: Columns<'_>,
        lazy: Option<Box<Lazy>>,
        kept: usize,
        kept_at: impl Fn(usize) -> (u32, u128),
    ) -> FlatTree {
        let Columns { words, nodes, node_parents, far, kinds, big, marks } = columns;
        debug_assert_eq!(nodes.len(), words.len().div_ceil(64), "a word of node bits for 64 elements");
        debug_assert!((1..kept).all(|n| kept_at(n - 1).0 < kept_at(n).0), "kept nodes ascending");
        let kept_words = match kept {
            0 => 0,
            _ => node_parents.len().div_ceil(64),
        };
        let mut room = block::room::<u64>(0, nodes.len());
        room = block::room::<u64>(room, kept_words);
        room = block::room::<Slot>(room, kept);
        room = block::room::<u32>(room, words.len());
        room = block::room::<u32>(room, nodes.len());
        room = block::room::<u32>(room, node_parents.len());
        room = block::room::<(u32, u32)>(room, far.len());
        room = block::room::<(u32, u16)>(room, kinds.len());
        room = block::room::<(u32, u32)>(room, big.len());
        room = block::room::<u32>(room, marks.len());
        room = block::room::<u32>(room, kept_words);
        room = block::room::<u32>(room, kept);
        // alloc: the tree's one block
        let mut block = Block::new(room);
        let nodes = block.put(nodes);
        let mut bits = block.fill(kept_words, |_| 0u64);
        let hashes = block.fill(kept, |n| Slot::new(kept_at(n).1));
        let words = block.put(words);
        let mut before = 0;
        let ranks = block.fill(nodes.len(), |word| {
            let rank = before;
            before += nodes[word].count_ones();
            rank
        });
        debug_assert_eq!(before as usize, node_parents.len(), "a node bit for each node");
        let node_parents = block.put(node_parents);
        let far = block.put(far);
        let kinds = block.put(kinds);
        let big = block.put(big);
        let marks = block.put(marks);
        for n in 0..kept {
            let at = kept_at(n).0;
            debug_assert!((nodes[at as usize / 64] >> (at % 64)) & 1 == 1, "element {at} is a node");
            let rank = rank_in(&nodes, &ranks, at);
            bits[rank / 64] |= 1 << (rank % 64);
        }
        let mut before = 0;
        let kept_ranks = block.fill(kept_words, |word| {
            let rank = before;
            before += bits[word].count_ones();
            rank
        });
        let elements = block.fill(kept, |n| kept_at(n).0);
        let kept = Kept { elements, bits, ranks: kept_ranks, hashes };
        FlatTree { source, words, nodes, ranks, node_parents, far, kinds, big, marks, lazy, kept, block }
    }

    /// The tree's columns, to pack into another block.
    fn columns(&self) -> Columns<'_> {
        Columns {
            words: &self.words,
            nodes: &self.nodes,
            node_parents: &self.node_parents,
            far: &self.far,
            kinds: &self.kinds,
            big: &self.big,
            marks: &self.marks,
        }
    }

```

7. `FlatTree::heap_size`. Before:

```rust
        let columns = self.words.len() + self.ranks.len() + self.node_parents.len() + self.marks.len();
        let tables = self.far.len() + self.kinds.len() + self.big.len();
        columns * 4 + self.nodes.len() * 8 + tables * 8 + lazy + self.kept.heap_size()
```

After (the same number: the block is exactly those bytes):

```rust
        self.block.size() + lazy
```

8. `FlatTree::kept_around`: it pushes into the caller's buffer. Before:

```rust
        with: Option<&FlatTree>,
    ) -> Vec<(u32, u128)> {
        let added = with.map_or(hi - lo, FlatTree::len);
        // alloc: the new tree's kept nodes, once per edit
        let theirs = with.map_or(0, |it| it.kept.elements.len());
        let mut kept = Vec::with_capacity(self.kept.elements.len() + theirs + 64);
        // Before `lo`, a node whose subtree ends past it is an ancestor.
        kept.extend(self.kept.between(0, lo).filter(|&(it, _)| self.end(it) <= lo));
        let mut up = self.parent(at);
        while let Some(ancestor) = up {
            self.keep_grown(ancestor, grown, hi - lo, added, &mut kept);
            up = self.parent(ancestor);
        }
        radix::sort_copy_by_key(&mut kept, |it| it.0);
        match with {
            Some(with) => kept.extend(with.kept.between(0, with.len()).map(|(it, hash)| (it + lo, hash))),
            None => kept.extend(self.kept.between(lo, hi)),
        }
        kept.extend(self.kept.between(hi, self.len()).map(|(it, hash)| (it - (hi - lo) + added, hash)));
        kept
    }
```

After (and the doc's first line becomes `/// Pushes to `kept`, empty, the kept nodes, each with its hash or 0, of the tree an edit of element`; `sorting` is room to sort them in):

```rust
        with: Option<&FlatTree>,
        kept: &mut Vec<(u32, u128)>,
        sorting: &mut Vec<(u32, u128)>,
    ) {
        let added = with.map_or(hi - lo, FlatTree::len);
        let theirs = with.map_or(0, |it| it.kept.elements.len());
        kept.reserve(self.kept.elements.len() + theirs + 64);
        // Before `lo`, a node whose subtree ends past it is an ancestor.
        kept.extend(self.kept.between(0, lo).filter(|&(it, _)| self.end(it) <= lo));
        let mut up = self.parent(at);
        while let Some(ancestor) = up {
            self.keep_grown(ancestor, grown, hi - lo, added, kept);
            up = self.parent(ancestor);
        }
        radix::sort_copy_by_key_in(kept, |it| it.0, sorting);
        match with {
            Some(with) => kept.extend(with.kept.between(0, with.len()).map(|(it, hash)| (it + lo, hash))),
            None => kept.extend(self.kept.between(lo, hi)),
        }
        kept.extend(self.kept.between(hi, self.len()).map(|(it, hash)| (it - (hi - lo) + added, hash)));
    }
```

9. `FlatTree::subtree`. Replace its body from `        let source = Text::from(&self.source[range]);`
through `        FlatTree { source, words, nodes, ranks, node_parents, far, kinds, big, marks, lazy, kept }` with:

```rust
        // alloc: the copy's text
        let source = Text::from(&self.source[range]);
        let mut buffers = Buffers::take();
        let Buffers { packer, marks, kept, spare, .. } = &mut buffers;
        packer.words.extend_from_slice(&self.words[lo as usize..hi as usize]);
        if packer.words[0] & TAG != NODE {
            packer.words[0] &= !(FAR << UP);
        }
        packer.big.extend(in_range(&self.big, lo, hi).iter().map(|&(it, value)| (it - at, value)));
        packer.kinds.extend(in_range(&self.kinds, lo, hi).iter().map(|&(it, kind)| (it - at, kind)));
        packer.far.extend(in_range(&self.far, lo + 1, hi).iter().map(|&(it, p)| (it - at, p - at)));
        let parents = &self.node_parents[self.nodes_before(lo)..self.nodes_before(hi)];
        packer.node_parents.extend(parents.iter().map(|&it| match it == NO_PARENT || it < at {
            true => NO_PARENT,
            false => it - at,
        }));
        marks_of(&packer.words, &packer.big, 0, marks);
        kept.extend(self.kept.between(lo, hi).map(|(it, hash)| (it - at, hash)));
        let lazy = self.lazy.as_ref().and_then(|lazy| {
            let from = lazy.nodes.partition_point(|&it| it < lo);
            let to = lazy.nodes.partition_point(|&it| it < hi);
            (from < to).then(|| {
                // alloc: an outline's lazy nodes in the copy, once per copy
                Box::new(Lazy {
                    // alloc: as the `Lazy`
                    nodes: lazy.nodes[from..to].iter().map(|it| it - at).collect(),
                    parts: lazy.parts[from..to]
                        .iter()
                        .map(|part| moved_part(part, &source, range.start().into(), 0))
                        .collect(), // alloc: as the `Lazy`
                    expand: lazy.expand,
                    data: lazy.data,
                })
            })
        });
        let tree = packer.tree(source, marks, spare, lazy, kept);
        buffers.give();
        tree
```

10. `FlatTree::subtree_without`. Replace its body from `        let range = self.range(at);`
(the first statement after the `for &it in elided { .. }` check loop) through
`        packer.finish(source, 0, lazy, &kept)` with:

```rust
        let range = self.range(at);
        let cut: u32 = elided.iter().map(|&it| u32::from(self.range(it).len())).sum();
        let mut buffers = Buffers::take();
        let Buffers { packer, marks, kept, text, opened: open, spare, .. } = &mut buffers;
        text.reserve((u32::from(range.len()) - cut) as usize);
        let mut start = range.start();
        for &it in elided {
            let it = self.range(it);
            text.push_str(&self.source[TextRange::new(start, it.start())]);
            start = it.end();
        }
        text.push_str(&self.source[TextRange::new(start, range.end())]);
        // alloc: the copy's text
        let source = Text::from(text.as_str());

        fn close(top: Open, open: &mut [Open], packer: &mut Packer, kept: &mut Vec<(u32, u128)>, offset: u32) {
            let size = packer.words.len() as u32 - top.new;
            packer.set_size(top.new, size);
            if let Some(parent) = open.last_mut() {
                parent.cut |= top.cut;
            }
            match (top.cut, top.hash) {
                (true, _) if keeps(offset - top.start, size) => kept.push((top.new, 0)),
                (false, Some(hash)) => kept.push((top.new, hash)),
                _ => {}
            }
        }

        let removed: u32 = elided.iter().map(|&it| self.end(it) - it - 1).sum();
        packer.reserve((hi - lo - removed) as usize);
        // alloc: an outline's lazy nodes in the copy, once per copy
        let (mut lazy_nodes, mut lazy_parts) = (Vec::new(), Vec::new());
        let mut olds = self.kept.between(lo, hi).peekable();
        let base = u32::from(range.start());
        // The next elided node, where the pass is in the copy's text, and the bytes cut before.
        let (mut next, mut offset, mut cut) = (0, 0u32, 0u32);
        let mut it = lo;
        while it < hi {
            while open.last().is_some_and(|top: &Open| top.end <= it) {
                let top = open.pop().expect("a node open");
                close(top, open, packer, kept, offset);
            }
            let parent = open.last().map_or(NO_PARENT, |top| top.new);
            let kind = self.kind(it);
            if elided.get(next) == Some(&it) {
                packer.node(kind, 1, parent);
                open.last_mut().expect("an elided node is under the root").cut = true;
                cut += u32::from(self.range(it).len());
                next += 1;
                it = self.end(it);
                continue;
            }
            match self.words[it as usize] & TAG {
                NODE => {
                    while olds.next_if(|&(old, _)| old < it).is_some() {}
                    let hash = olds.next_if(|&(old, _)| old == it).map(|(_, hash)| hash);
                    let new = packer.node(kind, 0, parent);
                    open.push(Open { new, start: offset, end: self.end(it), hash, cut: false });
                }
                tag => {
                    let len = self.text_len(it);
                    let new = packer.leaf(kind, tag, len, parent);
                    if tag == LAZY {
                        let lazy = self.lazy.as_deref().expect("a lazy node's tree has its lazy nodes");
                        let n = lazy.nodes.binary_search(&it).expect("a lazy node is among the lazy nodes");
                        lazy_nodes.push(new);
                        lazy_parts.push(moved_part(&lazy.parts[n], &source, base + offset + cut, offset));
                    }
                    offset += len;
                }
            }
            it += 1;
        }
        while let Some(top) = open.pop() {
            close(top, open, packer, kept, offset);
        }
        radix::sort_copy_by_key_in(kept, |it| it.0, &mut spare.hashes);
        let lazy = self.lazy.as_deref().filter(|_| !lazy_nodes.is_empty()).map(|lazy| {
            // alloc: the copy's lazy nodes, once per copy
            Box::new(Lazy {
                nodes: lazy_nodes.into(),
                parts: lazy_parts.into(),
                expand: lazy.expand,
                data: lazy.data,
            })
        });
        let tree = packer.finish(source, 0, marks, spare, lazy, kept);
        buffers.give();
        tree
```

(The fn-local `struct Open` and `fn close` it replaces go: `Open` is now at module level, for
`Buffers`.)

11. `FlatTree::replace_subtree`. Replace from
`        // Copied around `at`, not repacked: what changes is the ancestors' sizes, how far back`
through the end of the function (`            kept,\n        }\n    }`) with:

```rust
        // Copied around `at`, not repacked: what changes is the ancestors' sizes, how far back
        // the parent is of their tokens after `at`, and the numbers past it in the tables.
        let mut buffers = Buffers::take();
        let Buffers { packer, marks, kept: hashes, spare, .. } = &mut buffers;
        let Packer { words, node_parents, far, kinds, big } = packer;
        words.reserve(len as usize);
        words.extend_from_slice(&self.words[..lo as usize]);
        words.extend_from_slice(&with.words);
        words.extend_from_slice(&self.words[hi as usize..]);
        far.reserve(self.far.len() + with.far.len() + 1);
        far.extend_from_slice(in_range(&self.far, 0, lo));
        let root = &mut words[lo as usize];
        if *root & TAG != NODE {
            let distance = match up {
                None => 0,
                Some(parent) if lo - parent < FAR => lo - parent,
                Some(parent) => {
                    far.push((lo, parent));
                    FAR
                }
            };
            *root = (*root & !(FAR << UP)) | distance << UP;
        }
        far.extend(with.far.iter().map(|&(it, parent)| (it + lo, parent + lo)));
        // Those after `hi` under an ancestor are pushed as the ancestors are walked.
        let rest = in_range(&self.far, hi, self.len()).iter().filter(|it| it.1 >= lo);
        far.extend(rest.map(|&(it, parent)| (after(it), after(parent))));
        big.reserve(self.big.len() + with.big.len());
        // Before `lo`, a node whose subtree ends past it is an ancestor, its size set below.
        big.extend(in_range(&self.big, 0, lo).iter().filter(|&&(it, _)| self.end(it) <= lo));
        big.extend(with.big.iter().map(|&(it, value)| (it + lo, value)));
        big.extend(in_range(&self.big, hi, self.len()).iter().map(|&(it, value)| (after(it), value)));
        let (mut child, mut ancestor) = (at, up);
        while let Some(it) = ancestor {
            let end = self.end(it);
            let size = after(end) - it;
            let stored = match size < HUGE {
                true => size,
                false => {
                    big.push((it, size));
                    HUGE
                }
            };
            let word = &mut words[it as usize];
            *word = (*word & (FAR_KIND | TAG)) | stored << VALUE;
            let mut next = self.end(child);
            while next < end {
                if self.words[next as usize] & TAG != NODE {
                    let distance = after(next) - it;
                    let back = match distance < FAR {
                        true => distance,
                        false => {
                            far.push((after(next), it));
                            FAR
                        }
                    };
                    let word = &mut words[after(next) as usize];
                    *word = (*word & !(FAR << UP)) | back << UP;
                }
                next = self.end(next);
            }
            (child, ancestor) = (it, self.parent(it));
        }
        radix::sort_copy_by_key_in(far, |it| it.0, &mut spare.pairs);
        radix::sort_copy_by_key_in(big, |it| it.0, &mut spare.pairs);
        kinds.reserve(self.kinds.len() + with.kinds.len());
        kinds.extend_from_slice(in_range(&self.kinds, 0, lo));
        kinds.extend(with.kinds.iter().map(|&(it, kind)| (it + lo, kind)));
        kinds.extend(in_range(&self.kinds, hi, self.len()).iter().map(|&(it, kind)| (after(it), kind)));

        let (ranked, past) = (self.rank(lo), self.nodes_before(hi));
        let parent = up.unwrap_or(NO_PARENT);
        let count = ranked + with.node_parents.len() + self.node_parents.len() - past;
        node_parents.reserve(count);
        node_parents.extend_from_slice(&self.node_parents[..ranked]);
        node_parents.extend(with.node_parents.iter().map(|&it| match it {
            NO_PARENT => parent,
            it => it + lo,
        }));
        node_parents.extend(self.node_parents[past..].iter().map(|&it| match it {
            it if it < lo || it == NO_PARENT => it,
            it => after(it),
        }));
        spare.nodes.reserve((len as usize).div_ceil(64));
        let mut bits = 0;
        append_bits(&mut spare.nodes, &mut bits, &self.nodes, 0, lo as usize);
        append_bits(&mut spare.nodes, &mut bits, &with.nodes, 0, added as usize);
        append_bits(&mut spare.nodes, &mut bits, &self.nodes, hi as usize, self.len() as usize);

        // A start past `lo` is `with`'s, moved to `at`'s start, or that of the element it was
        // before the edit, moved by what the text grew: a mark and up to seven lengths.
        let first = lo.div_ceil(MARK);
        let with_start = u32::from(new.start());
        marks.reserve((len / MARK + 1) as usize);
        marks.extend_from_slice(&self.marks[..first as usize]);
        marks.extend((first..=len / MARK).map(|mark| match mark * MARK {
            it if it < lo + added => with.offset(it - lo) - with_start + start,
            it => self.offset(before(it)) - old_end + new_end,
        }));

        let ours = self.lazy.as_ref().map(|lazy| {
            let before = lazy.nodes.partition_point(|&it| it < lo);
            let past = lazy.nodes.partition_point(|&it| it < hi);
            (lazy, before, past)
        });
        let kept = ours.map_or(0, |(lazy, before, past)| before + lazy.nodes.len() - past);
        let theirs = with.lazy.as_ref().filter(|it| !it.nodes.is_empty());
        let expander = match (ours.filter(|_| kept > 0), theirs) {
            (Some((ours, ..)), Some(theirs)) => {
                assert_eq!(
                    ours.data, theirs.data,
                    "a tree's lazy nodes and those replacing {kind:?} at {old:?} have different expanders",
                );
                Some((ours.expand, ours.data))
            }
            (Some((ours, ..)), None) => Some((ours.expand, ours.data)),
            (None, Some(theirs)) => Some((theirs.expand, theirs.data)),
            (None, None) => None,
        };
        let lazy = expander.map(|(expand, data)| {
            // alloc: an outline's lazy nodes, once per edit
            let (mut nodes, mut parts) = (Vec::new(), Vec::new());
            if let Some((ours, before, _)) = ours {
                for (&it, part) in ours.nodes[..before].iter().zip(&ours.parts[..before]) {
                    nodes.push(it);
                    parts.push(moved_part(part, &source, 0, 0));
                }
            }
            if let Some(theirs) = theirs {
                for (&it, part) in theirs.nodes.iter().zip(theirs.parts.iter()) {
                    nodes.push(it + lo);
                    parts.push(moved_part(part, &source, new.start().into(), start));
                }
            }
            if let Some((ours, _, past)) = ours {
                for (&it, part) in ours.nodes[past..].iter().zip(&ours.parts[past..]) {
                    nodes.push(after(it));
                    parts.push(moved_part(part, &source, old_end, new_end));
                }
            }
            // alloc: as `nodes` above
            Box::new(Lazy { nodes: nodes.into(), parts: parts.into(), expand, data })
        });

        let grown = u32::from(new.len()) as i64 - u32::from(old.len()) as i64;
        self.kept_around(at, grown, (lo, hi), Some(with), hashes, &mut spare.hashes);
        let columns = Columns {
            words: words.as_slice(),
            nodes: &spare.nodes,
            node_parents: node_parents.as_slice(),
            far: far.as_slice(),
            kinds: kinds.as_slice(),
            big: big.as_slice(),
            marks: marks.as_slice(),
        };
        let tree = FlatTree::pack(source, columns, lazy, hashes.len(), |n| hashes[n]);
        buffers.give();
        tree
    }
```

(What goes: the `ranks` made here, which `pack` counts from the node bits, with its
`debug_assert_eq!`, which `pack` has.)

12. `FlatTree::replace_token`. Replace from `        let mut words = self.words.clone();`
through the end of the function with:

```rust
        let mut buffers = Buffers::take();
        let Buffers { packer, kept, spare, .. } = &mut buffers;
        let big = &mut packer.big;
        big.extend_from_slice(&self.big);
        if let Ok(it) = big.binary_search_by_key(&at, |it| it.0) {
            big.remove(it);
        }
        let stored = match len < LONG {
            true => len,
            false => {
                big.insert(big.partition_point(|it| it.0 < at), (at, len));
                LONG
            }
        };
        let lazy = self.lazy.as_ref().map(|lazy| {
            let parts = lazy.nodes.iter().zip(lazy.parts.iter()).map(|(&it, part)| match it < at {
                true => moved_part(part, &source, 0, 0),
                false => moved_part(part, &source, old_end, new_end),
            });
            // alloc: an outline's lazy nodes, once per edit
            Box::new(Lazy {
                nodes: lazy.nodes.clone(), // alloc: as the `Lazy`
                parts: parts.collect(), // alloc: as the `Lazy`
                expand: lazy.expand,
                data: lazy.data,
            })
        });
        let grown = len as i64 - u32::from(old.len()) as i64;
        self.kept_around(at, grown, (at, at + 1), None, kept, &mut spare.hashes);
        let columns = Columns { big: big.as_slice(), ..self.columns() };
        let mut tree = FlatTree::pack(source, columns, lazy, kept.len(), |n| kept[n]);
        buffers.give();
        // No element but the token changes: its word, and the starts kept after it.
        let word = &mut tree.words[at as usize];
        *word = (*word & !(LONG << VALUE)) | stored << VALUE;
        for (mark, start) in tree.marks.iter_mut().enumerate() {
            if mark as u32 * MARK > at {
                *start = *start - old_end + new_end;
            }
        }
        tree
    }
```

13. Delete `fn exact` (with its doc comment): nothing calls it.

14. `FlatTreeBuilder`. Replace from `/// Makes a [`FlatTree`] in preorder: nodes are started and finished around their children.`
through the end of `sourced_range` with:

```rust
/// Makes a [`FlatTree`] in preorder: nodes are started and finished around their children.
/// Its buffers are the calling thread's ([`Buffers`]), given back by `finish`.
#[derive(Debug)]
pub struct FlatTreeBuilder {
    /// The text appended so far, when the builder has no source.
    text: String,
    source: Option<Text>,
    /// The next byte of `source`.
    next: usize,
    /// Where in `source` the tree ends.
    end: usize,
    packer: Packer,
    marks: Vec<u32>,
    open: Vec<u32>,
    /// Where each node of `open` starts.
    starts: Vec<u32>,
    lazies: Vec<u32>,
    expand: Option<(Expand, u32)>,
    /// The nodes finished that [`keeps`] their hashes, in the order they were finished.
    kept: Vec<(u32, u128)>,
    /// The rest of the thread's buffers, given back with the others.
    opened: Vec<Open>,
    spare: Spare,
}

impl Default for FlatTreeBuilder {
    fn default() -> FlatTreeBuilder {
        FlatTreeBuilder::new()
    }
}

impl FlatTreeBuilder {
    pub fn new() -> FlatTreeBuilder {
        let Buffers { packer, marks, open, starts, lazies, kept, text, opened, spare } = Buffers::take();
        FlatTreeBuilder {
            text,
            source: None,
            next: 0,
            end: 0,
            packer,
            marks,
            open,
            starts,
            lazies,
            expand: None,
            kept,
            opened,
            spare,
        }
    }

    /// A builder for a text of about `len` bytes.
    pub fn with_capacity(len: usize) -> FlatTreeBuilder {
        // About one element for every two bytes of Rust.
        let elements = len / 2;
        let mut builder = FlatTreeBuilder::new();
        builder.text.reserve(len);
        builder.packer.reserve(elements);
        builder.marks.reserve(elements / MARK as usize + 1);
        builder
    }

    /// A builder whose tokens are consecutive slices of `source`, which the tree keeps instead
    /// of a copy: `token` checks its text is the next bytes of `source` and moves past them,
    /// and `finish` checks every byte was reached.
    pub fn sourced(source: Text) -> FlatTreeBuilder {
        let range = TextRange::up_to(TextSize::of(&*source));
        FlatTreeBuilder::sourced_range(source, range)
    }

    /// A sourced builder ([`Self::sourced`]) of a tree over `range` of `source` only: its first
    /// token starts at `range.start()`, and `finish` checks every byte to `range.end()` was
    /// reached. The tree's ranges are in `source`, as a lazy node's part's are.
    pub fn sourced_range(source: Text, range: TextRange) -> FlatTreeBuilder {
        let (start, end) = (usize::from(range.start()), usize::from(range.end()));
        assert!(
            end <= source.len() && source.is_char_boundary(start) && source.is_char_boundary(end),
            "{range:?} is not a range of a source of {} bytes",
            source.len(),
        );
        let elements = (end - start) / 2;
        let mut builder = FlatTreeBuilder::new();
        builder.packer.reserve(elements);
        builder.marks.reserve(elements / MARK as usize + 1);
        builder.source = Some(source);
        (builder.next, builder.end) = (start, end);
        builder
    }
```

15. `FlatTreeBuilder::finish`. Replace the whole function with:

```rust
    pub fn finish(mut self) -> FlatTree {
        let len = self.packer.words.len();
        assert!(self.open.is_empty() && len > 0, "the tree is not finished");
        if len % MARK as usize == 0 {
            self.marks.push(self.offset() as u32);
        }
        let source = match self.source.take() {
            Some(source) => {
                assert!(
                    self.next == self.end,
                    "the tree ends at byte {} of a source it should cover to {} (of {})",
                    self.next,
                    self.end,
                    source.len(),
                );
                source
            }
            // alloc: the tree's own text, appended (one copy, as `Text::from(String)` makes)
            None => Text::from(self.text.as_str()),
        };
        let lazy = match self.lazies.is_empty() {
            true => None,
            false => {
                let (expand, data) = self.expand.expect("a tree with lazy nodes has no expander");
                // alloc: an outline's lazy nodes and their parts' slots, once a tree
                Some(Box::new(Lazy {
                    parts: self.lazies.iter().map(|_| OnceLock::new()).collect(), // alloc: as the `Lazy`
                    nodes: self.lazies.as_slice().into(),
                    expand,
                    data,
                }))
            }
        };
        radix::sort_copy_by_key_in(&mut self.kept, |it| it.0, &mut self.spare.hashes);
        let tree = self.packer.tree(source, &self.marks, &mut self.spare, lazy, &self.kept);
        let FlatTreeBuilder { text, packer, marks, open, starts, lazies, kept, opened, spare, .. } = self;
        Buffers { packer, marks, open, starts, lazies, kept, text, opened, spare }.give();
        tree
    }
```

16. The budget test (append to `mod tests` in `flat.rs`):

```rust
    /// With the thread's buffers warm, a build, a copy of a subtree, a splice and a token's
    /// replacement each allocate the new tree's block alone (and a copy its text): small trees
    /// and one with every side table and kept hashes, whose arrays are those of the trees made
    /// cold.
    #[test]
    fn a_warm_build_copy_or_edit_allocates_its_block_alone() {
        for seed in 0..40 {
            let shape = match seed % 4 {
                3 => big_shape(seed),
                _ => random_shape(seed),
            };
            let mut text = String::new();
            text_of(&shape, &mut text);
            let source = Text::from(text.as_str());
            let build = || {
                let mut builder = FlatTreeBuilder::sourced(source.clone());
                to_flat(&shape, &mut builder, false);
                builder.finish()
            };
            let cold = build();
            let (tree, used) = blaze_alloc_count::measure(build);
            assert_eq!(used.allocations, 1, "seed {seed}: a warm build, {} bytes", used.bytes);
            same_arrays(&tree, &cold, seed);
            assert_eq!(tree.heap_size(), used.bytes as usize, "seed {seed}: the block is the tree");

            let at = (0..tree.len()).rev().find(|&it| !tree.is_token(it)).expect("a node");
            let copy = tree.subtree(at);
            let (again, used) = blaze_alloc_count::measure(|| tree.subtree(at));
            assert_eq!(used.allocations, 2, "seed {seed}: a warm copy, its text and its block");
            same_arrays(&again, &copy, seed);

            let spliced = tree.replace_subtree(at, &copy, source.clone());
            let (again, used) =
                blaze_alloc_count::measure(|| tree.replace_subtree(at, &copy, source.clone()));
            assert_eq!(used.allocations, 1, "seed {seed}: a warm splice, {} bytes", used.bytes);
            same_arrays(&again, &spliced, seed);
            same_arrays(&again, &tree, seed);

            if let Some(token) = (0..tree.len()).find(|&it| tree.is_token(it)) {
                let replaced = tree.replace_token(token, tree.token_text(token), source.clone());
                let (again, used) = blaze_alloc_count::measure(|| {
                    tree.replace_token(token, tree.token_text(token), source.clone())
                });
                assert_eq!(used.allocations, 1, "seed {seed}: a warm token, {} bytes", used.bytes);
                same_arrays(&again, &replaced, seed);
                same_arrays(&again, &tree, seed);
            }
        }
    }

    /// A tree and its clone hold the same columns in two blocks, and the clone keeps the hashes
    /// made before it.
    #[test]
    fn a_clone_is_the_tree_in_a_block_of_its_own() {
        let tree = build_flat(&big_shape(5), true);
        hash_all(&tree);
        let clone = tree.clone();
        same_arrays(&clone, &tree, 5);
        assert_eq!(clone.kept.elements, tree.kept.elements);
        assert!(clone.kept.hashes.iter().zip(tree.kept.hashes.iter()).all(|(a, b)| a.value() == b.value()));
        assert!(!std::ptr::eq(clone.words.as_ptr(), tree.words.as_ptr()));
    }
```

(`hash_all` is the existing helper that makes every kept hash; `build_flat`, `big_shape`,
`random_shape`, `text_of`, `to_flat` and `same_arrays` are the existing test helpers.)

Everything else in `flat.rs` (every read of a column, `slot`, `between`, `moved`'s
`self.marks.iter_mut()`, `PartialEq`, `Hash`, `Debug`, the tests' `tree.big.iter()` and
`assert_eq!((&spliced.nodes, ..), ..)`) is unchanged: `Col<T>` derefs to `[T]` and implements
`PartialEq`, `Eq`, `Hash` and `Debug` as the slice does.

hot_lint: the `// alloc:` lines above replace the ones they stand beside today; `Vec::new()` in
`Buffers::new`, `Packer::new` and `Spare` are empty vectors (no allocation), in `const fn`s.

> forwarded to frontend-tree (frontend-deps is DONE) (from requests, pass 16; told by tmux).

## mem-format-tree: an edit's reparse makes no buffer of its own

Why: on a changed file, `reparsing.rs` makes per-call buffers on every keystroke: two
`format!` strings in `Edit::token` (the token with the next char, the previous token with it),
one in `lexes_alone` (the text from the token before to the token after), the ancestors `Vec`
and a `vec![bool]` in `Edit::node`, `first_tokens`' `Vec`, and a copy of the node's new text
(`Text::from(text)`) for every body, block or item parsed again, which its tree is built over
and `replace_subtree` then moves to the new source anyway. The strings become the thread's
`blaze_mem::scratch::with_string` buffer; the two walks need no buffer (whether an ancestor is
below the outermost `fn` body is one compare once that body's depth is known); the first
tokens are compared as iterators; and the node's tree is built over its range of the new source
(`FlatTreeBuilder::sourced_range`, which a lazy part's tree already uses), so nothing is copied.
Same checks, same order, same trees and errors (a node's errors are still given from its start).
After this and section 2, a warm edit allocates its trees alone: the reparsed node's block, the
spliced tree's block and its `Arc` (a token's edit: the last two), once the lexer's and parser
input's columns are on their shelves (`v2/requests/mem-format-tokens.md`).

> forwarded to frontend-tree (frontend-deps is DONE) (from requests, pass 16; told by tmux).
> frontend-tree (2026-10-29): yours to apply, under the grant in section 2.
> mem-format-tree (2026-10-29): applied as written in `Cargo.toml`, `parsing.rs` and
> `reparsing.rs`; item 10 (`syntax_node.rs`) was applied by frontend-tree.

### `crates/syntax/Cargo.toml`

Under `[dependencies]`, add (it is a root workspace dependency):

```toml
blaze-mem.workspace = true
```

> forwarded to frontend-tree (frontend-deps is DONE) (from requests, pass 16; told by tmux).

### `crates/syntax/src/parsing.rs`, `build_node`

Before:

```rust
/// [`build_tree`] for a node of `kind` parsed again alone: `None` unless the parse took every
/// token into one node of `kind`, its root.
pub(crate) fn build_node(
    source: Text,
    lexed: &parser::LexedStr<'_>,
```

After:

```rust
/// [`build_tree`] for a node of `kind` parsed again alone, over `range` of `source` (`lexed`
/// is its lexing), its errors' ranges from the node's start: `None` unless the parse took
/// every token into one node of `kind`, its root.
pub(crate) fn build_node(
    source: Text,
    range: TextRange,
    lexed: &parser::LexedStr<'_>,
```

and its last two lines, before:

```rust
    let (tree, errors, _eof) = build_tree(source, lexed, parser_output, edition);
    Some((tree, errors))
}
```

after:

```rust
    let (tree, errors, _eof) = build_parsed(source, range, lexed, parser_output, edition);
    let start = range.start();
    // A parser error is at its offset in `source`; the node's are given from its start.
    let mut errors: Vec<SyntaxError> = errors
        .into_iter()
        .map(|it| {
            let range = it.range() - start;
            it.with_range(range)
        })
        .collect(); // alloc: none, collected in place (and none without errors)
    errors.extend(lexer_errors(lexed, TextSize::new(0)));
    Some((tree, errors))
}
```

> forwarded to frontend-tree (frontend-deps is DONE) (from requests, pass 16; told by tmux).

### `crates/syntax/src/parsing/reparsing.rs`

1. `Edit::new_text`. Before:

```rust
    /// The text of the old `range`, which holds the edit, after it.
    fn new_text(&self, range: TextRange) -> &str {
        let len = range.len() + TextSize::of(self.insert) - self.delete.len();
        &self.new_source[TextRange::at(range.start(), len)]
    }
```

After:

```rust
    /// Where the old `range`, which holds the edit, is in the new text.
    fn new_range(&self, range: TextRange) -> TextRange {
        TextRange::at(range.start(), range.len() + TextSize::of(self.insert) - self.delete.len())
    }

    /// The text of the old `range`, which holds the edit, after it.
    fn new_text(&self, range: TextRange) -> &str {
        &self.new_source[self.new_range(range)]
    }
```

2. `Edit::token`. Before:

```rust
        if let Some(next_char) = self.tree.source()[usize::from(range.end())..].chars().next() {
            let with_next = format!("{new_text}{next_char}");
            if LexedStr::single_token(self.edition, &with_next).is_some() {
                return None;
            }
        }
        if let Some(prev) = prev.filter(|&it| tree.is_token(it)) {
            let prev_text = tree.token_text(prev);
            let alone = LexedStr::new(self.edition, prev_text).len();
            let joint = format!("{prev_text}{new_text}");
            let joint = LexedStr::new(self.edition, &joint);
            if joint.len() != alone + 1 || joint.text(joint.len() - 1) != new_text {
                return None;
            }
        }
```

After:

```rust
        if let Some(next_char) = self.tree.source()[usize::from(range.end())..].chars().next() {
            let takes_next = blaze_mem::scratch::with_string(|with_next| {
                with_next.push_str(new_text);
                with_next.push(next_char);
                LexedStr::single_token(self.edition, with_next).is_some()
            });
            if takes_next {
                return None;
            }
        }
        if let Some(prev) = prev.filter(|&it| tree.is_token(it)) {
            let prev_text = tree.token_text(prev);
            let alone = LexedStr::new(self.edition, prev_text).len();
            let joins = blaze_mem::scratch::with_string(|joint| {
                joint.push_str(prev_text);
                joint.push_str(new_text);
                let joint = LexedStr::new(self.edition, joint);
                joint.len() == alone + 1 && joint.text(joint.len() - 1) == new_text
            });
            if !joins {
                return None;
            }
        }
```

3. `Edit::node`. Before:

```rust
        let ancestors: Vec<u32> =
            std::iter::successors(Some(start), |&it| tree.parent(it)).collect();
        // In an outline, what is in a `fn` body that is not lazy is parsed in full.
        let mut in_full = vec![!self.outline; ancestors.len()];
        let mut in_body = false;
        for (i, &it) in ancestors.iter().enumerate().rev() {
            in_full[i] |= in_body;
            in_body |= self.is_fn_body(it);
        }
        ancestors.iter().zip(in_full).find_map(|(&at, in_full)| {
            let kind = self.kind(at);
```

After:

```rust
        let ancestors = || std::iter::successors(Some(start), |&it| tree.parent(it));
        // In an outline, what is in a `fn` body that is not lazy is parsed in full: the
        // ancestors below the outermost body.
        let outermost = ancestors().enumerate().filter(|&(_, it)| self.is_fn_body(it)).last();
        ancestors().enumerate().find_map(|(i, at)| {
            let in_full = !self.outline || outermost.is_some_and(|(body, _)| i < body);
            let kind = self.kind(at);
```

(The rest of the closure is unchanged. Same values: the old loop set `in_full[i]` when an
ancestor past `i` is a body, which is `i` below the outermost one.)

4. `Edit::body`. Before:

```rust
    fn body(&self, at: u32) -> Option<Spliced> {
        let text = self.new_text(self.tree.range(at));
        let lexed = LexedStr::new(self.edition, text);
```

After:

```rust
    fn body(&self, at: u32) -> Option<Spliced> {
        let range = self.new_range(self.tree.range(at));
        let text = &self.new_source[range];
        let lexed = LexedStr::new(self.edition, text);
```

and before:

```rust
        let mut builder = SyntaxTreeBuilder::sourced(Text::from(text));
        builder.lazy(BLOCK_EXPR, TextSize::of(text));
```

after:

```rust
        // clone: a count of the new text, which the node's tree shares
        let mut builder = SyntaxTreeBuilder::sourced_range(self.new_source.clone(), range);
        builder.lazy(BLOCK_EXPR, TextSize::of(text));
```

5. `Edit::block`. Before:

```rust
        let kind = self.kind(at);
        let text = self.new_text(self.tree.range(at));
        let lexed = LexedStr::new(self.edition, text);
        // An expression's parse looks into the block or list after it (a `{` after a path is
```

After:

```rust
        let kind = self.kind(at);
        let range = self.new_range(self.tree.range(at));
        let lexed = LexedStr::new(self.edition, &self.new_source[range]);
        // An expression's parse looks into the block or list after it (a `{` after a path is
```

and before:

```rust
        let (with, errors) = build_node(Text::from(text), &lexed, &output, self.edition, kind)?;
        self.splice(at, with, errors)
    }

    /// An item of the source file, parsed as the file's top level parses it: with the tokens
```

after:

```rust
        // clone: a count of the new text, which the node's tree shares
        let source = self.new_source.clone();
        let (with, errors) = build_node(source, range, &lexed, &output, self.edition, kind)?;
        self.splice(at, with, errors)
    }

    /// An item of the source file, parsed as the file's top level parses it: with the tokens
```

6. `Edit::item`. Before:

```rust
        let range = tree.range(at);
        let text = self.new_text(range);
        let lexed = LexedStr::new(self.edition, text);
        // Trivia after an item are its parent's; a parse of the item alone would take them.
```

After:

```rust
        let range = tree.range(at);
        let new = self.new_range(range);
        let lexed = LexedStr::new(self.edition, &self.new_source[new]);
        // Trivia after an item are its parent's; a parse of the item alone would take them.
```

and before:

```rust
        let (with, errors) = build_node(Text::from(text), &lexed, &output, self.edition, kind)?;
        self.splice(at, with, errors)
    }

    fn splice(
```

after:

```rust
        // clone: a count of the new text, which the node's tree shares
        let source = self.new_source.clone();
        let (with, errors) = build_node(source, new, &lexed, &output, self.edition, kind)?;
        self.splice(at, with, errors)
    }

    fn splice(
```

(`item` keeps `range`, the old one, for `ahead`, as today.)

7. `Edit::same_start`'s last line, before:

```rust
        first_tokens(&old) == first_tokens(new)
```

after:

```rust
        first_tokens(&old).eq(first_tokens(new))
```

and `first_tokens`, before:

```rust
fn first_tokens<'a>(lexed: &'a LexedStr<'_>) -> Vec<(SyntaxKind, &'a str)> {
    (0..lexed.len())
        .filter(|&it| !lexed.kind(it).is_trivia())
        .take(LOOKAHEAD)
        .map(|it| (lexed.kind(it), lexed.text(it)))
        .collect()
}
```

after:

```rust
fn first_tokens<'a>(lexed: &'a LexedStr<'_>) -> impl Iterator<Item = (SyntaxKind, &'a str)> + 'a {
    (0..lexed.len())
        .filter(|&it| !lexed.kind(it).is_trivia())
        .take(LOOKAHEAD)
        .map(|it| (lexed.kind(it), lexed.text(it)))
}
```

8. `Edit::lexes_alone`'s end, before:

```rust
        let joint = format!("{before}{}{after}", new.as_str());
        let joint = LexedStr::new(self.edition, &joint);
        let (before, after) =
            (LexedStr::new(self.edition, before), LexedStr::new(self.edition, after));
        let alone = [&before, new, &after].into_iter().flat_map(token_lengths);
        alone.eq(token_lengths(&joint))
    }
```

after:

```rust
        blaze_mem::scratch::with_string(|joint| {
            joint.push_str(before);
            joint.push_str(new.as_str());
            joint.push_str(after);
            let joint = LexedStr::new(self.edition, joint);
            let (before, after) =
                (LexedStr::new(self.edition, before), LexedStr::new(self.edition, after));
            let alone = [&before, new, &after].into_iter().flat_map(token_lengths);
            alone.eq(token_lengths(&joint))
        })
    }
```

9. `use` line: `Text` is still used (`new_source: &Text`); nothing to change.

10. `crates/syntax/src/syntax_node.rs`: `body` was `SyntaxTreeBuilder::sourced`'s last caller,
so delete it (the 8 lines from `/// A builder whose tokens are consecutive slices of
`source`, which the tree shares.` through its closing `}`). `sourced_range`'s doc then reads
"over `range` of `source` only": change it to `/// A builder whose tokens are consecutive
slices of `range` of `source`, which the tree shares.`

   > frontend-tree (2026-10-29): applied: `SyntaxTreeBuilder::sourced` deleted and `sourced_range`'s
   > doc reworded.

11. The budget test (append to `mod tests`; `Text` already comes in by `use super::*`):

```rust
    /// A warm edit allocates the trees it makes alone: for a block parsed again, the block's
    /// tree, the spliced tree and the parse's `Arc`; for a token, the last two. No buffer of
    /// the reparse itself (the lexer's and parser input's columns are their shelves'), and the
    /// same parse as the whole text's.
    #[test]
    fn a_warm_reparse_allocates_its_trees_alone() {
        let before = "fn f() {\n    let x = 1 + 2;\n}\nfn foo() {}\n";
        let edits = [
            ("fn f() {\n    let x = 1 + 3;\n}\nfn foo() {}\n", 3, "a block parsed again"),
            ("fn f() {\n    let x = 1 + 2;\n}\nfn fob() {}\n", 2, "a token replaced"),
        ];
        let parse = SourceFile::parse(before, Edition::CURRENT);
        for (after, budget, what) in edits {
            let after = Text::from(after);
            let cold = parse.reparse_to(after.clone(), Edition::CURRENT);
            let warm = blaze_alloc_count::assert_at_most(budget, what, || {
                parse.reparse_to(after.clone(), Edition::CURRENT)
            });
            let whole = SourceFile::parse(&after, Edition::CURRENT);
            assert_eq!(format!("{:#?}", warm.tree().syntax()), format!("{:#?}", cold.tree().syntax()));
            assert_eq!(format!("{:#?}", warm.tree().syntax()), format!("{:#?}", whole.tree().syntax()));
        }
    }
```

> forwarded to frontend-tree (frontend-deps is DONE) (from requests, pass 16; told by tmux).

## dedup: seeded RNG onto blaze_ids::rng

From `dedup-codec-rng-front` (report `/tmp/blaze-lanes/v2-dedup-codec-rng-front.md`). Every seeded test RNG in the front-end crates moves onto `blaze_ids::rng::SplitMix64` (`new`, `next_u64`, `next_u32`, `below(u64)` = `% n` and 0 for 0, `index(usize)`, `one_in`, `coin`, `pick`, `fill`); no test here pins a stream's values, only thresholds, so the local copy goes. The crate already depends on `blaze-ids`.

Two files (also tell `frontend-deps-items`, which holds `flat.rs` too).

`crates/syntax/src/parsing/reparsing.rs`, `mod tests`:

1. Add `use blaze_ids::rng::SplitMix64;` before `use parser::Edition;`.
2. Delete the doc line `/// A small deterministic generator, for the edits of [`reparse_random_edits`].`, `struct Rng(u64);` and `impl Rng { below }` (~853-863).
3. `Rng(0x2545_f491_4f6c_dd1d)` (~922) and `Rng(0x9e37_79b9_7f4a_7c15)` (~963) become `SplitMix64::new(..)` with the same literal.
4. `fn random_edit(rng: &mut Rng, ...)` becomes `rng: &mut SplitMix64`, and its six `rng.below(` become `rng.index(` (the old `below` took `usize` and mapped 0 to 1, as `index` does).

`crates/syntax-tree/src/flat.rs`, `mod tests`:

1. Add `use blaze_ids::rng::SplitMix64;` after the `use std::{...};` group.
2. Delete `struct Rng(u64);` and `impl Rng { new, below }` (~1726-1737, the LCG; its `new` only stepped the LCG once, which SplitMix64 doesn't need).
3. Every `Rng::new(` becomes `SplitMix64::new(` (same seeds, the `seed ^ 0x....` XORs kept), and every `&mut Rng` (`make_lazy`, `grow`, the `&mut Rng::new(..)` arguments) becomes `&mut SplitMix64`.
4. `rng.below(n)` stays (same `u64 -> u64`). Optional: `x[rng.below(x.len() as u64) as usize]` reads better as `x[rng.index(x.len())]`.

> forwarded to frontend-tree (frontend-deps is DONE) (from requests, pass 16; told by tmux).
> frontend-tree (2026-10-29): `reparsing.rs` and `flat.rs` are under the grant to mem-format-tree
> (section 2), which applies this with its own edits there.
> mem-format-tree (2026-10-29): applied in both files as written. `flat/block.rs`'s new test
> uses `SplitMix64` too.

## mem-format: a parse's errors at their exact length, its lazies list kept (to frontend-tree; from mem-db-def B7; 2026-10-29)

`base-db`'s `EditionedFileId::parse_errors` (and its with-bodies twin) turns `Parse::errors()`'s
`Vec` into a `Box<[SyntaxError]>` with `.into()`, which reallocates whenever validation left spare
capacity: on each edit of a file with errors, a common state while typing. `errors_of` also
collects `lazies` into a fresh `Vec` per call of an outline. Depends on this file's section 3
(`syntax` gets `blaze-mem`).

`crates/syntax/src/lib.rs`: `errors_of` becomes `errors_of_into`, filling the caller's list, with
`lazies` on a kept buffer; `errors_of` and two exact forms wrap it.

```rust
thread_local! {
    /// The lists a parse's errors are gathered in, kept at the largest they grew to.
    static ERRORS: RefCell<Scratch<SyntaxError>> = const { RefCell::new(Scratch::new()) };
    static LAZIES: RefCell<Scratch<(TextRange, u32)>> = const { RefCell::new(Scratch::new()) };
}

    /// [`Parse::errors`] at its exact length, `None` for none: what a memo keeps.
    pub fn errors_exact(&self) -> Option<Box<[SyntaxError]>> {
        self.exact_of(false)
    }

    /// [`Parse::errors_with_bodies`] at its exact length, `None` for none.
    pub fn errors_with_bodies_exact(&self) -> Option<Box<[SyntaxError]>> {
        self.exact_of(true)
    }

    fn errors_of(&self, all_bodies: bool) -> Vec<SyntaxError> {
        let mut errors = Vec::new();
        self.errors_of_into(all_bodies, &mut errors);
        errors
    }

    fn exact_of(&self, all_bodies: bool) -> Option<Box<[SyntaxError]>> {
        // Taken, not borrowed: a body's parse inside validation then starts from an empty list.
        let mut kept = ERRORS.take();
        let errors = kept.take();
        self.errors_of_into(all_bodies, errors);
        // alloc: the result, at its exact length; the errors move, none is cloned
        let exact = (!errors.is_empty()).then(|| errors.drain(..).collect());
        ERRORS.set(kept);
        exact
    }

    fn errors_of_into(&self, all_bodies: bool, errors: &mut Vec<SyntaxError>) {
        if let Some(e) = self.errors.as_deref() {
            errors.extend_from_slice(e);
        }
        let root = self.syntax_node();
        let Some(edition) = self.outline else {
            validation::validate(&root, errors);
            return;
        };
        let tree = self.tree.as_ref().unwrap().flat();
        let mut kept = LAZIES.take();
        let lazies = kept.take();
        lazies.extend(tree.lazies().map(|at| (tree.range(at), at)));
        validation::validate_outline(
            &root,
            lazies,
            &mut |at, errors| {
                if !all_bodies && !tree.is_parsed(at) {
                    return false;
                }
                errors.extend(parsing::body_errors(tree, at, edition));
                true
            },
            errors,
        );
        LAZIES.set(kept);
    }
```

(Imports: `std::cell::RefCell`, `blaze_mem::scratch::Scratch`. `Scratch` derives `Default`, which
`LocalKey::take` needs. `extend_from_slice` clones the stored errors as `to_vec` did. Check
`validate_outline`'s parameter types against the tree: `&lazies` was a `&Vec<(TextRange, u32)>`
and `lazies` is now `&mut Vec<..>`, so pass `&lazies[..]` if it takes a slice.)

The callers then make one allocation, the exact box: in `crates/base-db/src/editioned_file_id.rs`
(lane db; mem-db-def's to forward), `parse_errors` becomes
`self.parse(db).errors_exact()`, and the with-bodies query `...errors_with_bodies_exact()`.

Budget test, in `crates/syntax` beside section 1's:

```rust
    /// A warm exact list of a parse's errors is one allocation (the box), however many there
    /// are; none for a parse without errors.
    #[test]
    fn exact_errors_allocate_only_their_box() {
        let text = "fn f() { let = ; } fn g( { } struct S { a: }";
        let parse = SourceFile::parse(text, Edition::CURRENT);
        let want = parse.errors();
        assert!(!want.is_empty());
        assert_eq!(parse.errors_exact().as_deref(), Some(&want[..]));
        blaze_alloc_count::assert_at_most(1 + want.len() as u64, "exact errors", || {
            std::hint::black_box(parse.errors_exact());
        });
        let clean = SourceFile::parse("fn f() {}", Edition::CURRENT);
        assert_eq!(clean.errors_exact(), None);
        blaze_alloc_count::assert_at_most(0, "no errors", || {
            std::hint::black_box(clean.errors_exact());
        });
    }
```

(The `want.len()` is each stored error's message, cloned as today; the list itself is the one
box. Validation's own errors move.)

> forwarded to frontend-tree (holds `crates/syntax/`; the `base-db` callers are db's; told by tmux; requests, pass 22).
> frontend-tree (2026-10-30): applied in `crates/syntax/src/lib.rs` on the view-based `errors_of`
> (`validate`/`validate_outline` take `view::Node` now): `errors_exact`,
> `errors_with_bodies_exact`, `errors_of_into` with `ERRORS`/`LAZIES` kept `Scratch` lists. One
> change: `Scratch<T>`'s derived `Default` needs `T: Default`, which `SyntaxError` isn't, so
> both lists are taken with `LocalKey::replace(Scratch::new())`, not `take()`. Test
> `exact_errors_allocate_only_their_box` in `src/tests.rs` as written, plus the outline's exact
> forms against `errors()`/`errors_with_bodies()`. The `base-db` callers
> (`editioned_file_id.rs` L46, L57 -> `errors_exact()` / `errors_with_bodies_exact()`) are db's to apply.
> frontend-tree-go-base (2026-10-30): applied in `crates/base-db/src/editioned_file_id.rs`:
> `parse_errors` is `self.parse(db).errors_exact()`, `parse_errors_with_bodies` is
> `self.parse(db).errors_with_bodies_exact()`; the `match` and `.into()` are gone.
