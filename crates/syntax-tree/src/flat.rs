//! A syntax tree as arrays, immutable: an edit makes a new tree.
//!
//! Every node and token is an element, numbered in preorder. Nothing is allocated per element,
//! and walking is stepping through the arrays. The cursor ([`crate::cursor`]) is an element of
//! one of these trees; a walk that reads many elements reads them here, by number, and makes
//! a cursor only of what it keeps ([`crate::cursor::SyntaxNode::flat`]).
//!
//! What an element costs: a token 4 bytes, a node 8, and every element 0.7 more:
//!
//! - one `u32` word each. Its low 10 bits are the kind (Rust's are fewer than 400) and the
//!   next 2 say whether it is a node, a token or a lazy node. A token's or lazy node's top 20
//!   are its length in bytes (10 bits) and how many elements back its parent is (10 bits); a
//!   node's are its subtree size in elements. A value that does not fit is in a side table
//!   sorted by element, which a file has few entries in: tokens of 1 KiB or more, nodes of a
//!   million elements or more, and tokens whose parent is a thousand elements or more back
//!   (those between large items or statements: the whitespace of a file's top level, the
//!   brace closing a long block). Every step to a token's parent is a subtraction.
//! - a node's parent's number (`u32`), in a column of nodes only, found by the node's rank:
//!   a bit per element says which are nodes, and a `u32` per 64 elements counts those before
//!   it (1.5 bits an element), so a rank is one load and a popcount.
//! - an eighth of a `u32`: the start of every eighth element, from which an element's start
//!   is that plus the lengths of up to seven elements before it, so [`FlatTree::range`] reads
//!   at most fifteen words. The binary searches of the child at a range search these starts
//!   and then step through at most eight elements, so they need no column of their own.
//!
//! That is about 6.5 bytes an element in Rust, where a little over half are tokens, against
//! 14 for a kind, a start, an end and a parent each; and no copy of the text: a tree's ranges
//! index a source it shares ([`FlatTree::source`]), the whole file's for a file's tree and its
//! parts.
//!
//! A lazy node is a node whose children are not in the arrays (a body an outline parse left
//! unparsed): its text is parsed by the tree's [`Expand`] the first time its part is asked for
//! ([`FlatTree::part`]), once, whichever thread asks. The arrays show a lazy node as a node
//! without children, so array walks never parse; the cursor walks into parts as if they were
//! in the arrays.
//!
//! A node whose subtree is long to hash keeps its [`FlatTree::subtree_hash`] once made, and
//! an edit carries it over to the subtrees it does not change. A hash of a parent is not made
//! of its children's (BLAKE3 of a text is not made of its parts' hashes), so it is made when
//! asked for, not while the tree is built: an edit would make every ancestor's again, the
//! root's the whole file.

#[allow(unsafe_code)]
mod block;
mod budget;
mod image;

use std::{
    cell::Cell,
    fmt,
    hash::{Hash, Hasher},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, OnceLock,
    },
};

use blaze_ids::{
    bits::{rank_in, BitSlice, BitSliceMut},
    radix, Domain,
};

use self::block::{Block, Col};
pub use self::image::{ImageError, IMAGE_LAYOUT};
use crate::{SyntaxKind, Text, TextRange, TextSize};

/// Every how many elements a start is kept.
const MARK: u32 = 8;
const NO_PARENT: u32 = u32::MAX;
/// The kind's bits, and the kind of an element whose kind is in [`FlatTree::kinds`].
const FAR_KIND: u32 = (1 << 10) - 1;
const TAG: u32 = 0b11 << 10;
const NODE: u32 = 0;
const TOKEN: u32 = 1 << 10;
const LAZY: u32 = 2 << 10;
/// Where a token's length and a node's size start.
const VALUE: u32 = 12;
/// A token's length's bits, and the length of one whose length is in [`FlatTree::big`].
const LONG: u32 = (1 << 10) - 1;
/// Where a token's distance back to its parent starts.
const UP: u32 = 22;
/// The distance of a token whose parent is in [`FlatTree::far`]; 0 is no parent.
const FAR: u32 = (1 << 10) - 1;
/// The size of a node whose size is in [`FlatTree::big`].
const HUGE: u32 = (1 << 20) - 1;
const _: () = assert!(
    FAR_KIND & TAG == 0 && (LONG << VALUE) & (FAR << UP) == 0 && UP + 10 == 32 && VALUE + 20 == 32
);

/// How many bytes [`FlatTree::hash_subtree`] writes of a node, about, from which the node
/// keeps its hash ([`keeps`]). Under it the hash is at most four BLAKE3 chunks, about 2 µs,
/// made again when asked. Rust keeps a few hashes in every 4 KiB of text, about 3% on top of
/// the tree's arrays; a slot for every node would be more than doubling them.
const KEEP: u64 = 4096;

/// Whether a node of `text` bytes and `size` elements keeps its hash: its text and a word an
/// element is what hashing it reads.
#[inline]
fn keeps(text: u32, size: u32) -> bool {
    text as u64 + 4 * size as u64 >= KEEP
}

/// The domain of [`FlatTree::subtree_hash`], which `hir-def` reads as a body's or a
/// signature's tree hash (`expr_store::body::subtree_hash`): renaming it changes every hash
/// stored under it.
static SUBTREE: Domain = Domain::new("blaze v2 hir-def body tree 1");

/// A kept hash, its high half 0 while not made. Whichever thread makes it writes it, and every
/// one writes the same value; a hash whose high half is 0 is made again each time.
struct Slot([AtomicU64; 2]);

impl Slot {
    /// A slot holding `hash`, 0 for none.
    fn new(hash: u128) -> Slot {
        Slot([AtomicU64::new(hash as u64), AtomicU64::new((hash >> 64) as u64)])
    }

    #[inline]
    fn get(&self) -> Option<u128> {
        // The acquire load of the high half sees a write of it, after which that writer's
        // low half (or another's, the same) is seen.
        let high = self.0[1].load(Ordering::Acquire);
        (high != 0).then(|| ((high as u128) << 64) | self.0[0].load(Ordering::Relaxed) as u128)
    }

    fn set(&self, hash: u128) {
        self.0[0].store(hash as u64, Ordering::Relaxed);
        self.0[1].store((hash >> 64) as u64, Ordering::Release);
    }

    /// The hash, or 0 while not made.
    fn value(&self) -> u128 {
        self.get().unwrap_or(0)
    }
}

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
    /// The slot of the node of rank `rank`, when it is kept.
    #[inline]
    fn slot(&self, rank: usize) -> Option<&Slot> {
        BitSlice::<usize>::from_words(&self.bits[..])
            .contains(rank)
            .then(|| &self.hashes[rank_in(&self.bits, &self.ranks, rank)])
    }

    /// The kept nodes from `lo` to `hi`, each with its hash or 0.
    fn between(&self, lo: u32, hi: u32) -> impl Iterator<Item = (u32, u128)> + '_ {
        let from = self.elements.partition_point(|&it| it < lo);
        let to = self.elements.partition_point(|&it| it < hi);
        self.elements[from..to].iter().zip(&self.hashes[from..to]).map(|(&it, slot)| (it, slot.value()))
    }
}

/// What parses a lazy node: called with the node's kind, the tree's source, the node's range
/// in it and the tree's `data`, it gives a tree over that source whose root is a node of that
/// kind and range ([`FlatTreeBuilder::sourced_range`] makes one without copying the text), and
/// the errors parsing it found, their ranges in the source.
pub type Expand = fn(
    kind: SyntaxKind,
    source: &Text,
    range: TextRange,
    data: u32,
) -> (FlatTree, Vec<(TextRange, Box<str>)>);

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

/// A tree's columns before they are packed ([`FlatTree::pack`]): slices, or for an edit's
/// words, node parents, kinds and marks the values made as they are written ([`Fill`]).
#[derive(Clone, Copy)]
struct Columns<'a, W = &'a [u32], P = &'a [u32], K = &'a [(u32, u16)], M = &'a [u32]> {
    words: W,
    nodes: &'a [u64],
    node_parents: P,
    far: &'a [(u32, u32)],
    kinds: K,
    big: &'a [(u32, u32)],
    marks: M,
}

/// A column [`FlatTree::pack`] writes into its block: how many values it has, and writing
/// them there.
trait Fill<T> {
    fn count(&self) -> usize;
    fn place(self, block: &mut Block) -> Col<T>;
}

impl<T: Copy> Fill<T> for &[T] {
    fn count(&self) -> usize {
        self.len()
    }

    fn place(self, block: &mut Block) -> Col<T> {
        block.put(self)
    }
}

/// A column of the `.0` values of `.1`, made in order.
struct Made<I>(usize, I);

impl<T, I: Iterator<Item = T>> Fill<T> for Made<I> {
    fn count(&self) -> usize {
        self.0
    }

    fn place(self, block: &mut Block) -> Col<T> {
        let Made(len, mut values) = self;
        let col = block.fill(len, |_| values.next().expect("as many values as counted"));
        assert!(values.next().is_none(), "more values than the {len} counted");
        col
    }
}

/// A column of the slices of `.1` one after another, `.0` values in all.
struct Joined<I>(usize, I);

impl<'a, T: Copy + 'a, I: Iterator<Item = &'a [T]>> Fill<T> for Joined<I> {
    fn count(&self) -> usize {
        self.0
    }

    fn place(self, block: &mut Block) -> Col<T> {
        block.put_joined(self.0, self.1)
    }
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
    splicing: Splicing,
}

/// Where [`FlatTree::splice_all`] works out an edit.
#[derive(Debug)]
struct Splicing {
    /// The splices, placed.
    placed: Vec<Placed>,
    /// Their ancestors, each with its child on the way to the first splice under it.
    ancestors: Vec<(u32, u32)>,
    /// The words of the edited tree written after it is packed, by element.
    patches: Vec<(u32, u32)>,
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
            marks: Vec::new(),   // alloc: none, an empty vector
            open: Vec::new(),    // alloc: none, an empty vector
            starts: Vec::new(),  // alloc: none, an empty vector
            lazies: Vec::new(),  // alloc: none, an empty vector
            kept: Vec::new(),    // alloc: none, an empty vector
            text: String::new(), // alloc: none, an empty string
            opened: Vec::new(),  // alloc: none, an empty vector
            // alloc: none, empty vectors
            spare: Spare { nodes: Vec::new(), pairs: Vec::new(), hashes: Vec::new() },
            splicing: Splicing {
                placed: Vec::new(),    // alloc: none, an empty vector
                ancestors: Vec::new(), // alloc: none, an empty vector
                patches: Vec::new(),   // alloc: none, an empty vector
            },
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
        self.splicing.placed.clear();
        self.splicing.ancestors.clear();
        self.splicing.patches.clear();
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

/// A tree's lazy nodes, in preorder, and their parts.
#[derive(Clone)]
struct Lazy {
    nodes: Box<[u32]>,
    parts: Box<[OnceLock<Part>]>,
    expand: Expand,
    data: u32,
}

/// A lazy node's part once parsed: its tree and the errors parsing it found, both in the
/// file's source.
#[derive(Clone)]
struct Part {
    tree: Arc<FlatTree>,
    errors: Box<[(TextRange, Box<str>)]>,
}

impl Part {
    /// This part with its text at `to` in `source` instead of at `from`.
    fn moved(&self, source: &Text, from: u32, to: u32) -> Part {
        let moved = |range: TextRange| {
            TextRange::at((u32::from(range.start()) - from + to).into(), range.len())
        };
        Part {
            // clone: a parsed part kept across an edit, copied to move its marks, once per edit
            tree: Arc::new(FlatTree::clone(&self.tree).moved(source.clone(), from, to)),
            // alloc: the part's errors, once per part per edit
            errors: self.errors.iter().map(|(range, it)| (moved(*range), it.clone())).collect(),
        }
    }

    fn heap_size(&self) -> usize {
        let errors = self.errors.len() * std::mem::size_of::<(TextRange, Box<str>)>()
            + self.errors.iter().map(|it| it.1.len()).sum::<usize>();
        std::mem::size_of::<FlatTree>() + self.tree.heap_size() + errors
    }
}

impl fmt::Debug for FlatTree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlatTree")
            .field("elements", &self.words.len())
            .field("range", &self.range(0))
            .field("lazies", &self.lazy.as_ref().map_or(0, |it| it.nodes.len()))
            .finish()
    }
}

/// Two trees are equal when they are the same elements over the same text with the same
/// expander's data; which parts were parsed does not matter.
impl PartialEq for FlatTree {
    fn eq(&self, other: &FlatTree) -> bool {
        self.words == other.words
            && self.big == other.big
            && self.kinds == other.kinds
            && self.marks.first() == other.marks.first()
            && self.lazy.as_ref().map(|it| it.data) == other.lazy.as_ref().map(|it| it.data)
            && (self.source.ptr_eq(&other.source) || self.source == other.source)
    }
}

impl Eq for FlatTree {}

impl Hash for FlatTree {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.source.hash(state);
        self.words.hash(state);
        self.big.hash(state);
        self.kinds.hash(state);
        self.marks.first().hash(state);
        self.lazy.as_ref().map(|it| it.data).hash(state);
    }
}

/// The arrays of a tree being made, element by element in preorder.
#[derive(Default, Debug)]
struct Packer {
    words: Vec<u32>,
    node_parents: Vec<u32>,
    far: Vec<(u32, u32)>,
    kinds: Vec<(u32, u16)>,
    big: Vec<(u32, u32)>,
}

impl Packer {
    const fn new() -> Packer {
        Packer {
            words: Vec::new(),        // alloc: none, an empty vector
            node_parents: Vec::new(), // alloc: none, an empty vector
            far: Vec::new(),          // alloc: none, an empty vector
            kinds: Vec::new(),        // alloc: none, an empty vector
            big: Vec::new(),          // alloc: none, an empty vector
        }
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

    /// The next element's number and its kind's bits.
    fn start(&mut self, kind: SyntaxKind) -> (u32, u32) {
        assert!(self.words.len() < NO_PARENT as usize, "a tree of 2^32 - 1 elements or more");
        let at = self.words.len() as u32;
        let kind = match (kind.0 as u32) < FAR_KIND {
            true => kind.0 as u32,
            false => {
                self.kinds.push((at, kind.0));
                FAR_KIND
            }
        };
        (at, kind)
    }

    /// A node of `size` elements (0 while not known), under `parent` ([`NO_PARENT`] for none).
    fn node(&mut self, kind: SyntaxKind, size: u32, parent: u32) -> u32 {
        let (at, kind) = self.start(kind);
        self.words.push(kind | NODE);
        self.node_parents.push(parent);
        self.set_size(at, size);
        at
    }

    fn set_size(&mut self, at: u32, size: u32) {
        let size = match size < HUGE {
            true => size,
            false => {
                self.big.push((at, size));
                HUGE
            }
        };
        let word = &mut self.words[at as usize];
        *word = (*word & (FAR_KIND | TAG)) | size << VALUE;
    }

    /// A token or a lazy node of `len` bytes, under `parent` ([`NO_PARENT`] for none).
    fn leaf(&mut self, kind: SyntaxKind, tag: u32, len: u32, parent: u32) -> u32 {
        let (at, kind) = self.start(kind);
        let len = match len < LONG {
            true => len,
            false => {
                self.big.push((at, len));
                LONG
            }
        };
        let up = match parent {
            NO_PARENT => 0,
            parent if at - parent < FAR => at - parent,
            parent => {
                self.far.push((at, parent));
                FAR
            }
        };
        self.words.push(kind | tag | len << VALUE | up << UP);
        at
    }

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
        let columns: Columns<'_> = Columns {
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

/// The entries of a side table sorted by element of the elements from `lo` to `hi`.
fn in_range<T>(table: &[(u32, T)], lo: u32, hi: u32) -> &[(u32, T)] {
    let from = table.partition_point(|it| it.0 < lo);
    let to = table.partition_point(|it| it.0 < hi);
    &table[from..to]
}

/// `part`, parsed or not, as the part of a lazy node whose text moved from `from` to `to`,
/// in `source`.
fn moved_part(part: &OnceLock<Part>, source: &Text, from: u32, to: u32) -> OnceLock<Part> {
    let moved = OnceLock::new();
    if let Some(part) = part.get() {
        let _ = moved.set(part.moved(source, from, to));
    }
    moved
}

/// What [`FlatTree::splice_all`] puts in place of an element.
#[derive(Debug, Clone, Copy)]
pub enum Splice<'a> {
    /// A node's subtree, by this tree's, whose root is a node of its kind
    /// ([`FlatTree::replace_subtree`]).
    Subtree(&'a FlatTree),
    /// A token's text, its kind staying ([`FlatTree::replace_token`]).
    Token(&'a str),
}

/// A splice of [`FlatTree::splice_all`], placed: the old elements `lo..hi` it replaces, over
/// `start..end` of the old text, and the `added` elements it puts in their place, from `new_lo`
/// on, over `new_start..new_end` of the new.
#[derive(Debug, Clone, Copy)]
struct Placed {
    lo: u32,
    hi: u32,
    added: u32,
    new_lo: u32,
    start: u32,
    end: u32,
    new_start: u32,
    new_end: u32,
}

impl Placed {
    /// Where old element `it`, past this splice and before the next, is in the new tree.
    fn after(&self, it: u32) -> u32 {
        it - self.hi + self.new_lo + self.added
    }
}

/// How many of `placed`'s splices old element `it` is past: the gap it is in, or the splice it
/// is the first element of.
fn gap(placed: &[Placed], it: u32) -> usize {
    placed.partition_point(|p| p.hi <= it)
}

/// Old element `it`, past `g` of `placed`'s splices and before the next, in the new tree.
fn moved(placed: &[Placed], g: usize, it: u32) -> u32 {
    match g {
        0 => it,
        g => placed[g - 1].after(it),
    }
}

/// Where the text past `g` of `placed`'s splices (and before the next) was, and is: the end
/// of the last of them in the old text and in the new, as [`moved_part`] takes them.
fn text_shift(placed: &[Placed], g: usize) -> (u32, u32) {
    match g {
        0 => (0, 0),
        g => (placed[g - 1].end, placed[g - 1].new_end),
    }
}

impl FlatTree {
    /// The tree over `source` of `columns`, with [`FlatTree::ranks`] counted from its node bits,
    /// its `kept` nodes that [`keeps`] their hashes (ascending, the `n`th `kept_at(n)` with its
    /// hash or 0) and its lazy nodes: every column in one allocation, the block. The columns
    /// aligned to 8 are placed first, so the block has no padding.
    fn pack<W, P, K, M>(
        source: Text,
        columns: Columns<'_, W, P, K, M>,
        lazy: Option<Box<Lazy>>,
        kept: usize,
        kept_at: impl Fn(usize) -> (u32, u128),
    ) -> FlatTree
    where
        W: Fill<u32>,
        P: Fill<u32>,
        K: Fill<(u32, u16)>,
        M: Fill<u32>,
    {
        let Columns { words, nodes, node_parents, far, kinds, big, marks } = columns;
        let (elements, parents) = (words.count(), node_parents.count());
        debug_assert_eq!(nodes.len(), elements.div_ceil(64), "node bits for every 64 elements");
        debug_assert!((1..kept).all(|n| kept_at(n - 1).0 < kept_at(n).0), "kept nodes ascending");
        let kept_words = match kept {
            0 => 0,
            _ => parents.div_ceil(64),
        };
        let mut room = block::room::<u64>(0, nodes.len());
        room = block::room::<u64>(room, kept_words);
        room = block::room::<Slot>(room, kept);
        room = block::room::<u32>(room, elements);
        room = block::room::<u32>(room, nodes.len());
        room = block::room::<u32>(room, parents);
        room = block::room::<(u32, u32)>(room, far.len());
        room = block::room::<(u32, u16)>(room, kinds.count());
        room = block::room::<(u32, u32)>(room, big.len());
        room = block::room::<u32>(room, marks.count());
        room = block::room::<u32>(room, kept_words);
        room = block::room::<u32>(room, kept);
        // alloc: the tree's one block
        let mut block = Block::new(room);
        let nodes = block.put(nodes);
        let mut bits = block.fill(kept_words, |_| 0u64);
        let hashes = block.fill(kept, |n| Slot::new(kept_at(n).1));
        let words = words.place(&mut block);
        let mut before = 0;
        let ranks = block.fill(nodes.len(), |word| {
            let rank = before;
            before += nodes[word].count_ones();
            rank
        });
        debug_assert_eq!(before as usize, parents, "a node bit for each node");
        let node_parents = node_parents.place(&mut block);
        let far = block.put(far);
        let kinds = kinds.place(&mut block);
        let big = block.put(big);
        let marks = marks.place(&mut block);
        for n in 0..kept {
            let at = kept_at(n).0;
            let node = BitSlice::<usize>::from_words(&nodes[..]).contains(at as usize);
            debug_assert!(node, "element {at} is a node");
            let rank = rank_in(&nodes, &ranks, at as usize);
            BitSliceMut::<usize>::from_words(&mut bits[..]).insert(rank);
        }
        let mut before = 0;
        let kept_ranks = block.fill(kept_words, |word| {
            let rank = before;
            before += bits[word].count_ones();
            rank
        });
        let elements = block.fill(kept, |n| kept_at(n).0);
        let kept = Kept { elements, bits, ranks: kept_ranks, hashes };
        FlatTree {
            source,
            words,
            nodes,
            ranks,
            node_parents,
            far,
            kinds,
            big,
            marks,
            lazy,
            kept,
            block,
        }
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

    /// How many nodes and tokens the tree has; the root is number 0.
    #[inline]
    pub fn len(&self) -> u32 {
        self.words.len() as u32
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// The text the tree's ranges index: [`Self::source`].
    #[inline]
    pub fn text(&self) -> &str {
        &self.source
    }

    /// The text the tree's ranges index: the whole file's source for a file's tree and its
    /// parts, the tree's own text for one built by appending text.
    #[inline]
    pub fn source(&self) -> &Text {
        &self.source
    }

    /// The bytes the tree holds besides itself: its arrays and the parts parsed so far with
    /// their errors, not the source it shares.
    pub fn heap_size(&self) -> usize {
        let lazy = self.lazy.as_ref().map_or(0, |lazy| {
            std::mem::size_of::<Lazy>()
                + lazy.nodes.len() * 4
                + lazy.parts.len() * std::mem::size_of::<OnceLock<Part>>()
                + lazy.parts.iter().filter_map(|it| it.get()).map(Part::heap_size).sum::<usize>()
        });
        self.block.size() + lazy
    }

    #[inline]
    pub fn kind(&self, at: u32) -> SyntaxKind {
        match self.words[at as usize] & FAR_KIND {
            FAR_KIND => self.far_kind(at),
            kind => SyntaxKind(kind as u16),
        }
    }

    #[cold]
    fn far_kind(&self, at: u32) -> SyntaxKind {
        match self.kinds.binary_search_by_key(&at, |it| it.0) {
            Ok(it) => SyntaxKind(self.kinds[it].1),
            Err(_) => unreachable!("element {at}'s kind is not in the side table"),
        }
    }

    #[inline]
    pub fn is_token(&self, at: u32) -> bool {
        self.words[at as usize] & TAG == TOKEN
    }

    /// Whether `at` is a lazy node: a node whose children are in its part, not the arrays.
    #[inline]
    pub fn is_lazy(&self, at: u32) -> bool {
        self.words[at as usize] & TAG == LAZY
    }

    /// How many nodes come before element `at`, which is an element.
    #[inline]
    fn rank(&self, at: u32) -> usize {
        rank_in(&self.nodes, &self.ranks, at as usize)
    }

    /// How many nodes come before element `at`, or are in the tree for `len()`.
    fn nodes_before(&self, at: u32) -> usize {
        match at == self.len() {
            true => self.node_parents.len(),
            false => self.rank(at),
        }
    }

    #[inline]
    pub fn parent(&self, at: u32) -> Option<u32> {
        let word = self.words[at as usize];
        let parent = match word & TAG {
            NODE => self.node_parents[self.rank(at)],
            _ => match word >> UP {
                0 => NO_PARENT,
                FAR => self.far_parent(at),
                up => at - up,
            },
        };
        (parent != NO_PARENT).then_some(parent)
    }

    #[cold]
    fn far_parent(&self, at: u32) -> u32 {
        match self.far.binary_search_by_key(&at, |it| it.0) {
            Ok(it) => self.far[it].1,
            Err(_) => unreachable!("element {at}'s parent is not in the side table"),
        }
    }

    #[cold]
    fn big_value(&self, at: u32) -> u32 {
        match self.big.binary_search_by_key(&at, |it| it.0) {
            Ok(it) => self.big[it].1,
            Err(_) => unreachable!("element {at}'s length or size is not in the side table"),
        }
    }

    /// The bytes of text the element holds itself: a token's or lazy node's, none for a node.
    /// A walk of the elements in order adds these to the first one's start to have each one's
    /// range in O(1), where [`FlatTree::range`] counts from the last mark.
    #[inline]
    pub fn text_len(&self, at: u32) -> u32 {
        let word = self.words[at as usize];
        if word & TAG == NODE {
            return 0;
        }
        match (word >> VALUE) & LONG {
            LONG => self.big_value(at),
            len => len,
        }
    }

    /// The number after the element's subtree.
    #[inline]
    pub fn end(&self, at: u32) -> u32 {
        let word = self.words[at as usize];
        if word & TAG != NODE {
            return at + 1;
        }
        match word >> VALUE {
            HUGE => at + self.big_value(at),
            size => at + size,
        }
    }

    /// The children and tokens of `at`, in order.
    #[inline]
    pub fn children_with_tokens(&self, at: u32) -> impl Iterator<Item = u32> + '_ {
        let end = self.end(at);
        let mut next = at + 1;
        std::iter::from_fn(move || {
            let it = next;
            (it < end).then(|| {
                next = self.end(it);
                it
            })
        })
    }

    /// The children of `at` that are nodes, in order.
    #[inline]
    pub fn children(&self, at: u32) -> impl Iterator<Item = u32> + '_ {
        self.children_with_tokens(at).filter(|&it| !self.is_token(it))
    }

    /// `at` and the nodes under it, in preorder.
    #[inline]
    pub fn descendants(&self, at: u32) -> impl Iterator<Item = u32> + '_ {
        (at..self.end(at)).filter(|&it| !self.is_token(it))
    }

    /// Hashes what the subtree of `at` is made of, as the same subtree is hashed anywhere in
    /// any tree: its text, and its elements' kinds and shape, numbered from `at`. A lazy node
    /// is hashed as its kind and text, so an outline's hash is not a full tree's. A token's
    /// word holds how far back its parent is, which is the same wherever the subtree is, but
    /// for the root's, which is its parent's in place and none in a copy, so it is left out.
    pub fn hash_subtree<H: Hasher>(&self, at: u32, state: &mut H) {
        let end = self.end(at);
        self.source[self.range(at)].hash(state);
        let root = self.words[at as usize];
        match root & TAG {
            NODE => root.hash(state),
            _ => (root & !(FAR << UP)).hash(state),
        }
        self.words[at as usize + 1..end as usize].hash(state);
        for &(it, value) in in_range(&self.big, at, end) {
            (it - at, value).hash(state);
        }
        for &(it, kind) in in_range(&self.kinds, at, end) {
            (it - at, kind).hash(state);
        }
    }

    /// [`Self::hash_subtree`] of `at` into one BLAKE3 pass, its first 128 bits: equal for the
    /// same subtree anywhere in any tree. A node that [`keeps`] its hash makes it the first
    /// time it is asked for and reads it after, and the trees an edit makes keep it for the
    /// subtrees the edit does not change. Any other node is hashed when asked, at most four
    /// BLAKE3 chunks, and so is a token or a lazy node, its kind and text.
    pub fn subtree_hash(&self, at: u32) -> u128 {
        let slot = match self.words[at as usize] & TAG {
            NODE if !self.kept.bits.is_empty() => self.kept.slot(self.rank(at)),
            _ => None,
        };
        if let Some(hash) = slot.and_then(Slot::get) {
            return hash;
        }
        let mut hasher = blaze_ids::Hasher::of(&SUBTREE);
        self.hash_subtree(at, &mut blaze_hash::Blake3Hasher(&mut hasher));
        let hash = hasher.finish().fingerprint().0;
        if let Some(slot) = slot {
            slot.set(hash);
        }
        hash
    }

    /// Where element `at` starts; `len()` for where the last ends.
    #[inline]
    pub(crate) fn offset(&self, at: u32) -> u32 {
        let mark = at / MARK;
        let mut offset = self.marks[mark as usize];
        for it in mark * MARK..at {
            offset += self.text_len(it);
        }
        offset
    }

    #[inline]
    pub fn range(&self, at: u32) -> TextRange {
        let start = self.offset(at);
        let end = match self.words[at as usize] & TAG == NODE {
            false => start + self.text_len(at),
            true => {
                let end = self.end(at);
                match end / MARK == at / MARK {
                    true => (at..end).fold(start, |offset, it| offset + self.text_len(it)),
                    false => self.offset(end),
                }
            }
        };
        TextRange::new(TextSize::from(start), TextSize::from(end))
    }

    /// Where node `at` ends, given where it starts: [`Self::range`]'s end without counting its
    /// start again.
    #[inline]
    pub(crate) fn node_end(&self, at: u32, start: u32) -> u32 {
        let end = self.end(at);
        match end / MARK == at / MARK {
            true => (at..end).fold(start, |offset, it| offset + self.text_len(it)),
            false => self.offset(end),
        }
    }

    #[inline]
    pub fn token_text(&self, at: u32) -> &str {
        &self.source[self.range(at)]
    }

    /// The first element from `lo` to `hi` whose start `pred` refuses, `pred` accepting the
    /// starts up to some offset and none after: a binary search of the kept starts, then at
    /// most [`MARK`] steps.
    fn partition_point(&self, lo: u32, hi: u32, pred: impl Fn(u32) -> bool) -> u32 {
        let first = lo.div_ceil(MARK);
        let last = hi.div_ceil(MARK).max(first);
        let n = self.marks[first as usize..last as usize].partition_point(|&it| pred(it)) as u32;
        let (mut it, mut offset) = match n {
            0 => (lo, self.offset(lo)),
            n => ((first + n - 1) * MARK, self.marks[(first + n - 1) as usize]),
        };
        while it < hi && pred(offset) {
            offset += self.text_len(it);
            it += 1;
        }
        it
    }

    /// The last of `at` and the elements under it to start at or before `offset`.
    #[inline]
    pub(crate) fn last_starting(&self, at: u32, offset: TextSize) -> Option<u32> {
        let offset = u32::from(offset);
        let n = self.partition_point(at, self.end(at), |it| it <= offset);
        (n > at).then(|| n - 1)
    }

    #[inline]
    pub(crate) fn first_child_or_token(&self, at: u32) -> Option<u32> {
        (self.end(at) > at + 1).then_some(at + 1)
    }

    #[inline]
    pub(crate) fn next_sibling_or_token(&self, at: u32) -> Option<u32> {
        let parent = self.parent(at)?;
        let next = self.end(at);
        (next < self.end(parent)).then_some(next)
    }

    /// The child of `parent` that `at` is in. Up from `at` is as many steps as `at` is deep
    /// below `parent`, which an `else if` chain makes as many as it has arms, and across
    /// `parent`'s children as many as come before it, which a long block makes many: a step
    /// of each at a time takes the fewer.
    #[inline]
    fn child_containing(&self, parent: u32, at: u32) -> u32 {
        let (mut up, mut across) = (at, parent + 1);
        loop {
            match self.parent(up) {
                Some(it) if it != parent => up = it,
                _ => return up,
            }
            if at < self.end(across) {
                return across;
            }
            across = self.end(across);
        }
    }

    #[inline]
    pub(crate) fn prev_sibling_or_token(&self, at: u32) -> Option<u32> {
        let parent = self.parent(at)?;
        (at > parent + 1).then(|| self.child_containing(parent, at - 1))
    }

    #[inline]
    pub(crate) fn last_child_or_token(&self, at: u32) -> Option<u32> {
        self.first_child_or_token(at).map(|_| self.child_containing(at, self.end(at) - 1))
    }

    /// The element's position among its parent's children and tokens.
    pub(crate) fn index(&self, at: u32) -> usize {
        let Some(parent) = self.parent(at) else { return 0 };
        let mut child = parent + 1;
        let mut index = 0;
        while child != at {
            child = self.end(child);
            index += 1;
        }
        index
    }

    /// The first child node of `at` from `from` on, `from` being `at`'s child or its end.
    #[inline]
    fn node_from(&self, at: u32, mut from: u32) -> Option<u32> {
        let end = self.end(at);
        // A token's subtree is itself.
        while from < end {
            if !self.is_token(from) {
                return Some(from);
            }
            from += 1;
        }
        None
    }

    #[inline]
    pub(crate) fn first_child(&self, at: u32) -> Option<u32> {
        self.node_from(at, at + 1)
    }

    #[inline]
    pub(crate) fn next_sibling(&self, at: u32) -> Option<u32> {
        self.node_from(self.parent(at)?, self.end(at))
    }

    /// The child or token of `at` that holds `range`, as a binary search of the children by
    /// overlap finds it: the one that holds `range` and overlaps it, else the last that ends
    /// at or before its start, else the first.
    pub(crate) fn child_at_range(&self, at: u32, range: TextRange) -> Option<u32> {
        let first = self.first_child_or_token(at)?;
        let (lo, hi) = (at + 1, self.end(at));
        let start = u32::from(range.start());
        let contains = |it: u32| self.range(it).contains_range(range);
        if !range.is_empty() {
            // Elements are in preorder, so the last to start at or before `start` is in the
            // only child that can hold a range that is not empty.
            let n = self.partition_point(lo, hi, |it| it <= start);
            if n == lo {
                return None;
            }
            let child = self.child_containing(at, n - 1);
            return contains(child).then_some(child);
        }
        let n = self.partition_point(lo, hi, |it| it < start);
        let mut last_less = None;
        if n > lo {
            let child = self.child_containing(at, n - 1);
            if u32::from(self.range(child).end()) > start {
                return Some(child);
            }
            last_less = Some(child);
        }
        // Empty children at `start` end at or before it too.
        let mut next = last_less.map_or(Some(first), |it| self.next_sibling_or_token(it));
        while let Some(it) = next.filter(|&it| self.range(it) == range) {
            last_less = Some(it);
            next = self.next_sibling_or_token(it);
        }
        last_less.or(Some(first)).filter(|&it| contains(it))
    }

    /// The first node of `kind` and `range` that `child_at_range` steps down to from `at`, for
    /// a range that is not empty: those steps go through the ancestors of the last element to
    /// start at or before the range, and stop at the first that does not hold it or is a
    /// token. Found from the bottom up, up to the first ancestor larger than the range, above
    /// which every one is larger too. A lazy node is a node without children here.
    pub(crate) fn node_at_range(&self, at: u32, range: TextRange, kind: SyntaxKind) -> Option<u32> {
        debug_assert!(!range.is_empty());
        let mut it = self.last_starting(at, range.start())?;
        let mut found = None;
        loop {
            let of_it = self.range(it);
            if of_it == range {
                if !self.is_token(it) && self.kind(it) == kind {
                    found = Some(it);
                }
            } else if of_it.contains_range(range) {
                return found;
            }
            if it == at {
                return found;
            }
            it = self.parent(it)?;
        }
    }

    /// The subtree of node `at`, as a tree of its own over a copy of its text, starting at 0.
    /// Its lazy nodes stay lazy, and the parts already parsed are kept, moved to the copy's
    /// text: copying a part's arrays is cheaper than parsing it again.
    pub(crate) fn subtree(&self, at: u32) -> FlatTree {
        let (lo, hi) = (at, self.end(at));
        let range = self.range(at);
        // alloc: the copy's text
        let source = Text::from(&self.source[range]);
        let mut buffers = Buffers::take();
        let Buffers { packer, marks, kept, spare, .. } = &mut buffers;
        packer.words.extend_from_slice(&self.words[lo as usize..hi as usize]);
        if packer.words[0] & TAG != NODE {
            packer.words[0] &= !(FAR << UP);
        }
        packer.big.extend(in_range(&self.big, lo, hi).iter().map(|&(it, value)| (it - at, value)));
        let kinds = in_range(&self.kinds, lo, hi).iter().map(|&(it, kind)| (it - at, kind));
        packer.kinds.extend(kinds);
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
    }

    /// [`Self::subtree`] of node `at` with the nodes `elided` under it, ascending and
    /// disjoint, each replaced by an empty node of its kind, or of `placeholder` when given:
    /// the copy `replace_subtree` of each in turn would make, over the text left. Made in one
    /// pass over the elements outside them, never reading their elements, text, lazy nodes or
    /// parts. Lazy nodes outside them stay lazy with their parsed parts moved, and the kept
    /// hashes outside them are carried but for their ancestors', which are made again when
    /// asked. With one `placeholder` kind, what an elided node was is nowhere in the copy.
    pub(crate) fn subtree_without(&self, at: u32, elided: &[u32], placeholder: Option<SyntaxKind>) -> FlatTree {
        let (lo, hi) = (at, self.end(at));
        assert!(!self.is_token(at), "element {at} ({:?}) is not a node", self.kind(at));
        let mut from = lo + 1;
        for &it in elided {
            assert!(
                it >= from && it < hi && !self.is_token(it),
                "the elided element {it} is not a node under {at} (to {hi}) after those before it",
            );
            from = self.end(it);
        }
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

        fn close(top: &Open, open: &mut [Open], packer: &mut Packer, kept: &mut Vec<(u32, u128)>, offset: u32) {
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
                close(&top, open, packer, kept, offset);
            }
            let parent = open.last().map_or(NO_PARENT, |top| top.new);
            let kind = self.kind(it);
            if elided.get(next) == Some(&it) {
                packer.node(placeholder.unwrap_or(kind), 1, parent);
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
            close(&top, open, packer, kept, offset);
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
    }

    /// This tree, which has no lazy nodes, with its text at `to` in `source` instead of at
    /// `from` in its own.
    fn moved(mut self, source: Text, from: u32, to: u32) -> FlatTree {
        debug_assert!(self.lazy.is_none());
        assert!(!self.block.is_held(), "a tree read in place is copied before it is written");
        for mark in self.marks.iter_mut() {
            *mark = *mark - from + to;
        }
        self.source = source;
        self
    }

    /// The tree with node `at`'s subtree replaced by `with`, whose root is a node of the same
    /// kind, over `source`, the new whole text: the text of `with` is `source` at `at`'s
    /// start, and the rest of the root's text is as it was around it. Lazy nodes outside `at`
    /// stay lazy and those of `with` come along; the parts already parsed of either are kept,
    /// moved to `source` (their text did not change, only where it is). A tree has one
    /// expander, so when both keep lazy nodes they must have the same `data`.
    pub fn replace_subtree(&self, at: u32, with: &FlatTree, source: Text) -> FlatTree {
        self.splice_all(&[(at, Splice::Subtree(with))], source)
    }

    /// The tree with each of `splices` made at once, over `source`, the new whole text: each
    /// one's element replaced as [`Self::replace_subtree`] or [`Self::replace_token`] replaces
    /// it, and the root's text between them as it was. They are in order of element and none
    /// is in another's subtree. The tree is the one those replacements made one after another
    /// give, made in one pass: its columns are written into its block as they are made, the
    /// copies around the splices with them, and the words that change after.
    pub fn splice_all(&self, splices: &[(u32, Splice<'_>)], source: Text) -> FlatTree {
        assert!(!splices.is_empty(), "no splices to make");
        let mut buffers = Buffers::take();
        self.place(splices, &mut buffers.splicing.placed);
        let last = *buffers.splicing.placed.last().expect("a splice placed");
        let old_root_end = u32::from(self.range(0).end());
        let root_end = old_root_end as u64 - last.end as u64 + last.new_end as u64;
        assert!(
            root_end <= u32::MAX as u64
                && root_end <= source.len() as u64
                && (old_root_end as usize != self.source.len() || root_end == source.len() as u64),
            "the new text, of {} bytes, is not the root's after the splices, to byte {root_end}",
            source.len(),
        );
        debug_assert!(
            self.spliced_text(&buffers.splicing.placed, splices, &source),
            "the new text is not the old one with the splices given made",
        );
        let len = self.len() as u64 - last.hi as u64 + (last.new_lo + last.added) as u64;
        assert!(len < NO_PARENT as u64, "a tree of 2^32 - 1 elements or more");
        let tree = match splices.iter().all(|(_, it)| matches!(it, Splice::Token(_))) {
            true => self.splice_tokens(splices, &mut buffers, source),
            false => self.splice_subtrees(splices, len as u32, &mut buffers, source),
        };
        buffers.give();
        tree
    }

    /// Pushes to `placed`, empty, where each of `splices` is in this tree and in the one they
    /// make.
    fn place(&self, splices: &[(u32, Splice<'_>)], placed: &mut Vec<Placed>) {
        placed.reserve(splices.len());
        for &(at, splice) in splices {
            let kind = self.kind(at);
            let (hi, added, len) = match splice {
                Splice::Subtree(with) => {
                    assert!(
                        !self.is_token(at) && !with.is_token(0) && with.kind(0) == kind,
                        "element {at} is replaced by a {:?} that is not a node of its kind, {kind:?}",
                        with.kind(0),
                    );
                    (self.end(at), with.len(), u32::from(with.range(0).len()))
                }
                Splice::Token(text) => {
                    assert!(self.is_token(at), "element {at} ({kind:?}) is not a token");
                    (at + 1, 1, u32::try_from(text.len()).expect("a token of under 4 GiB"))
                }
            };
            let old = self.range(at);
            let (start, end) = (u32::from(old.start()), u32::from(old.end()));
            let (new_lo, new_start) = match placed.last() {
                None => (at as u64, start as u64),
                Some(before) => {
                    assert!(
                        before.hi <= at,
                        "the splice of element {at} is in or before that of element {}",
                        before.lo,
                    );
                    let new_lo = at as u64 - before.hi as u64 + (before.new_lo + before.added) as u64;
                    (new_lo, start as u64 - before.end as u64 + before.new_end as u64)
                }
            };
            let new_end = new_start + len as u64;
            assert!(new_lo + (added as u64) < NO_PARENT as u64, "a tree of 2^32 - 1 elements or more");
            assert!(new_end <= u32::MAX as u64, "a text of 4 GiB or more");
            let (new_lo, new_start, new_end) = (new_lo as u32, new_start as u32, new_end as u32);
            placed.push(Placed { lo: at, hi, added, new_lo, start, end, new_start, new_end });
        }
    }

    /// Whether `source` is this tree's text with `placed`'s `splices` made: the text around
    /// them as it was, and each one's its own.
    fn spliced_text(&self, placed: &[Placed], splices: &[(u32, Splice<'_>)], source: &str) -> bool {
        fn bytes(text: &str, from: u32, to: u32) -> Option<&[u8]> {
            text.as_bytes().get(from as usize..to as usize)
        }
        let same = |new: u32, old: u32, to: u32| {
            let it = bytes(source, new, new + (to - old));
            it.is_some() && it == bytes(&self.source, old, to)
        };
        let root = self.range(0);
        let (mut old, mut new) = (u32::from(root.start()), u32::from(root.start()));
        for (p, &(_, splice)) in placed.iter().zip(splices) {
            let theirs = match splice {
                Splice::Subtree(with) => &with.source[with.range(0)],
                Splice::Token(text) => text,
            };
            if !same(new, old, p.start) || bytes(source, p.new_start, p.new_end) != Some(theirs.as_bytes()) {
                return false;
            }
            (old, new) = (p.end, p.new_end);
        }
        same(new, old, u32::from(root.end()))
    }

    /// The old elements between `placed`'s splices `g - 1` and `g`: after the one before
    /// (from the root for the first) to the next (to the end for the last).
    fn gap_of(&self, placed: &[Placed], g: usize) -> (u32, u32) {
        let from = match g {
            0 => 0,
            g => placed[g - 1].hi,
        };
        (from, placed.get(g).map_or(self.len(), |it| it.lo))
    }

    /// Pushes to `ancestors`, empty, every ancestor of `placed`'s elements once, each with
    /// its child on the way to the first of them under it: walked up from each splice to
    /// where the walk from the one before joins it.
    fn ancestors_of(&self, placed: &[Placed], ancestors: &mut Vec<(u32, u32)>) {
        let mut before = None;
        for it in placed {
            let (mut child, mut up) = (it.lo, self.parent(it.lo));
            while let Some(ancestor) = up {
                if before.is_some_and(|lo| ancestor <= lo && lo < self.end(ancestor)) {
                    break;
                }
                ancestors.push((ancestor, child));
                (child, up) = (ancestor, self.parent(ancestor));
            }
            before = Some(it.lo);
        }
    }

    /// Ancestor `at` of `placed`'s splices in the tree they make: its number, size and the
    /// length of its text.
    fn grown(&self, placed: &[Placed], at: u32) -> (u32, u32, u32) {
        let (end, range) = (self.end(at), self.range(at));
        let (g, past) = (gap(placed, at), gap(placed, end));
        let new = moved(placed, g, at);
        let offset = |g: usize, at: TextSize| {
            let (from, to) = text_shift(placed, g);
            u32::from(at) - from + to
        };
        (new, moved(placed, past, end) - new, offset(past, range.end()) - offset(g, range.start()))
    }

    /// Pushes to `kept`, empty, the kept nodes, each with its hash or 0, of the tree
    /// `placed`'s `splices` make, `ancestors` their ancestors; `sorting` is room to sort them
    /// in. The ancestors are hashed again when asked; every other kept node keeps its hash.
    fn kept_spliced(
        &self,
        placed: &[Placed],
        splices: &[(u32, Splice<'_>)],
        ancestors: &[(u32, u32)],
        kept: &mut Vec<(u32, u128)>,
        sorting: &mut Vec<(u32, u128)>,
    ) {
        let theirs: usize = splices
            .iter()
            .map(|(_, it)| match it {
                Splice::Subtree(with) => with.kept.elements.len(),
                Splice::Token(_) => 0,
            })
            .sum();
        kept.reserve(self.kept.elements.len() + theirs + ancestors.len());
        for g in 0..=placed.len() {
            let (from, to) = self.gap_of(placed, g);
            // A node of a gap whose subtree ends past the next splice's start is an ancestor.
            let ours = self.kept.between(from, to).filter(|&(it, _)| self.end(it) <= to);
            kept.extend(ours.map(|(it, hash)| (moved(placed, g, it), hash)));
            if let (Some(it), Some((_, Splice::Subtree(with)))) = (placed.get(g), splices.get(g)) {
                kept.extend(with.kept.between(0, with.len()).map(|(at, hash)| (at + it.new_lo, hash)));
            }
        }
        for &(at, _) in ancestors {
            let (new, size, text) = self.grown(placed, at);
            if keeps(text, size) {
                kept.push((new, 0));
            }
        }
        radix::sort_copy_by_key_in(kept, |it| it.0, sorting);
    }

    /// The word of token splice `it` in the tree it makes, its length past the word pushed
    /// to `big`.
    fn token_word(&self, it: &Placed, big: &mut Vec<(u32, u32)>) -> u32 {
        let len = it.new_end - it.new_start;
        if len >= LONG {
            big.push((it.new_lo, len));
        }
        (self.words[it.lo as usize] & !(LONG << VALUE)) | len.min(LONG) << VALUE
    }

    /// The kinds of splice `it`, `with` for a subtree, and the number of the first element
    /// they are numbered from.
    fn spliced_kinds<'a>(&'a self, it: &Placed, with: Option<&'a FlatTree>) -> (&'a [(u32, u16)], u32) {
        match with {
            Some(with) => (&with.kinds[..], 0),
            None => (in_range(&self.kinds, it.lo, it.hi), it.lo),
        }
    }

    /// [`Self::splice_all`] of tokens alone (`buffers.splicing.placed`): no element but the tokens
    /// changes, so the tree's columns are copied as they are, and only the tokens' words and
    /// the starts kept after the first are written.
    fn splice_tokens(&self, splices: &[(u32, Splice<'_>)], buffers: &mut Buffers, source: Text) -> FlatTree {
        let Buffers { packer, splicing: Splicing { placed, ancestors, .. }, kept, spare, .. } = buffers;
        let (placed, big) = (&placed[..], &mut packer.big);
        big.reserve(self.big.len() + placed.len());
        for g in 0..=placed.len() {
            let (from, to) = self.gap_of(placed, g);
            big.extend_from_slice(in_range(&self.big, from, to));
            if let Some(it) = placed.get(g).filter(|it| it.new_end - it.new_start >= LONG) {
                big.push((it.lo, it.new_end - it.new_start));
            }
        }
        let lazy = self.lazy.as_ref().map(|lazy| {
            let parts = lazy.nodes.iter().zip(lazy.parts.iter()).map(|(&it, part)| {
                let (from, to) = text_shift(placed, gap(placed, it));
                moved_part(part, &source, from, to)
            });
            // alloc: an outline's lazy nodes, once per edit
            Box::new(Lazy {
                nodes: lazy.nodes.clone(), // alloc: as the `Lazy`
                parts: parts.collect(),    // alloc: as the `Lazy`
                expand: lazy.expand,
                data: lazy.data,
            })
        });
        self.ancestors_of(placed, ancestors);
        self.kept_spliced(placed, splices, ancestors, kept, &mut spare.hashes);
        let columns = Columns { big: big.as_slice(), ..self.columns() };
        let mut tree = FlatTree::pack(source, columns, lazy, kept.len(), |n| kept[n]);
        for it in placed {
            let len = it.new_end - it.new_start;
            let word = &mut tree.words[it.lo as usize];
            *word = (*word & !(LONG << VALUE)) | len.min(LONG) << VALUE;
        }
        for (mark, start) in tree.marks.iter_mut().enumerate() {
            let (from, to) = text_shift(placed, gap(placed, mark as u32 * MARK));
            *start = *start - from + to;
        }
        tree
    }

    /// [`Self::splice_all`] with a subtree among `splices` (`buffers.splicing`), of `len`
    /// elements: the copies between the splices and the splices' own columns are written into
    /// the block one after another, numbered as they are made; the tables are merged in
    /// order; the ancestors' words, and those of the leaves under them after a splice, are
    /// written last.
    fn splice_subtrees(
        &self,
        splices: &[(u32, Splice<'_>)],
        len: u32,
        buffers: &mut Buffers,
        source: Text,
    ) -> FlatTree {
        let Buffers { packer, splicing, kept, spare, .. } = buffers;
        let Splicing { placed, ancestors, patches } = splicing;
        let (placed, far, big) = (&placed[..], &mut packer.far, &mut packer.big);
        let subtree = move |g: usize| match splices.get(g) {
            Some(&(_, Splice::Subtree(with))) => Some(with),
            _ => None,
        };
        let gaps = move |g: usize| self.gap_of(placed, g);
        let splices_and_gaps = move || 0..=placed.len();
        self.ancestors_of(placed, ancestors);

        // The copies' entries, moved: a far parent of a leaf with a splice between them, and
        // an ancestor's size, are pushed as the ancestors are walked.
        for g in splices_and_gaps() {
            let (from, to) = gaps(g);
            let near = in_range(&self.far, from, to).iter().filter(|it| it.1 >= from);
            far.extend(near.map(|&(it, parent)| (moved(placed, g, it), moved(placed, g, parent))));
            let sized = in_range(&self.big, from, to).iter().filter(|&&(it, _)| self.end(it) <= to);
            big.extend(sized.map(|&(it, value)| (moved(placed, g, it), value)));
            if let (Some(it), Some(with)) = (placed.get(g), subtree(g)) {
                far.extend(with.far.iter().map(|&(at, parent)| (at + it.new_lo, parent + it.new_lo)));
                big.extend(with.big.iter().map(|&(at, value)| (at + it.new_lo, value)));
            }
        }
        for &(at, child) in ancestors.iter() {
            let (new, size, _) = self.grown(placed, at);
            let stored = match size < HUGE {
                true => size,
                false => {
                    big.push((new, size));
                    HUGE
                }
            };
            patches.push((new, (self.words[at as usize] & (FAR_KIND | TAG)) | stored << VALUE));
            let end = self.end(at);
            let mut next = child;
            while next < end {
                let g = gap(placed, next);
                let spliced = placed.get(g).filter(|it| it.lo == next);
                let word = match (spliced, subtree(g)) {
                    (Some(_), Some(with)) => with.words[0],
                    (Some(it), None) => self.token_word(it, big),
                    (None, _) => self.words[next as usize],
                };
                if word & TAG != NODE {
                    let it = spliced.map_or_else(|| moved(placed, g, next), |it| it.new_lo);
                    let back = match it - new < FAR {
                        true => it - new,
                        false => {
                            far.push((it, new));
                            FAR
                        }
                    };
                    let under = (word & !(FAR << UP)) | back << UP;
                    if spliced.is_some() || under != word {
                        patches.push((it, under));
                    }
                }
                next = spliced.map_or_else(|| self.end(next), |it| it.hi);
            }
        }
        radix::sort_copy_by_key_in(far, |it| it.0, &mut spare.pairs);
        radix::sort_copy_by_key_in(big, |it| it.0, &mut spare.pairs);

        spare.nodes.resize((len as usize).div_ceil(64), 0);
        let mut nodes = BitSliceMut::<usize>::from_words(&mut spare.nodes[..]);
        let ours = BitSlice::from_words(&self.nodes[..]);
        for g in splices_and_gaps() {
            let (from, to) = gaps(g);
            nodes.or_range_at(moved(placed, g, from) as usize, &ours, from as usize..to as usize);
            if let (Some(it), Some(with)) = (placed.get(g), subtree(g)) {
                let theirs = BitSlice::from_words(&with.nodes[..]);
                nodes.or_range_at(it.new_lo as usize, &theirs, 0..it.added as usize);
            }
        }

        let ours = self.lazy.as_deref();
        let lazies = |lazy: &Lazy, (from, to): (u32, u32)| {
            lazy.nodes.partition_point(|&it| it < from)..lazy.nodes.partition_point(|&it| it < to)
        };
        let kept_ours: usize = ours.map_or(0, |lazy| {
            splices_and_gaps().map(|g| lazies(lazy, gaps(g)).len()).sum()
        });
        let mut expander = ours.filter(|_| kept_ours > 0).map(|it| (it.expand, it.data));
        for (g, it) in placed.iter().enumerate() {
            let theirs = subtree(g).and_then(|with| with.lazy.as_ref()).filter(|it| !it.nodes.is_empty());
            match (expander, theirs) {
                (Some((_, data)), Some(theirs)) => assert_eq!(
                    data, theirs.data,
                    "a tree's lazy nodes and those replacing element {} have different expanders",
                    it.lo,
                ),
                (None, Some(theirs)) => expander = Some((theirs.expand, theirs.data)),
                (_, None) => (),
            }
        }
        let lazy = expander.map(|(expand, data)| {
            // alloc: an outline's lazy nodes, once per edit
            let (mut nodes, mut parts) = (Vec::new(), Vec::new());
            for g in splices_and_gaps() {
                if let Some(ours) = ours {
                    let range = lazies(ours, gaps(g));
                    let (from, to) = text_shift(placed, g);
                    for (&it, part) in ours.nodes[range.start..range.end].iter().zip(&ours.parts[range]) {
                        nodes.push(moved(placed, g, it));
                        parts.push(moved_part(part, &source, from, to));
                    }
                }
                let theirs = placed.get(g).zip(subtree(g)).and_then(|(it, with)| Some((it, with, with.lazy.as_ref()?)));
                if let Some((it, with, theirs)) = theirs {
                    let start = u32::from(with.range(0).start());
                    for (&at, part) in theirs.nodes.iter().zip(theirs.parts.iter()) {
                        nodes.push(at + it.new_lo);
                        parts.push(moved_part(part, &source, start, it.new_start));
                    }
                }
            }
            // alloc: as `nodes` above
            Box::new(Lazy { nodes: nodes.into(), parts: parts.into(), expand, data })
        });
        self.kept_spliced(placed, splices, ancestors, kept, &mut spare.hashes);

        // The columns made as they are written: each copy between the splices, then the
        // splice's own, numbered in the new tree.
        let words = splices_and_gaps().flat_map(move |g| {
            let (from, to) = gaps(g);
            let spliced = placed.get(g).map(|it| match subtree(g) {
                Some(with) => &with.words[..],
                None => &self.words[it.lo as usize..it.hi as usize],
            });
            std::iter::once(&self.words[from as usize..to as usize]).chain(spliced)
        });
        let parents = splices_and_gaps().map(|g| {
            let (from, to) = gaps(g);
            self.nodes_before(to) - self.nodes_before(from) + subtree(g).map_or(0, |it| it.node_parents.len())
        });
        let node_parents = splices_and_gaps().flat_map(move |g| {
            let (from, to) = gaps(g);
            let ours = self.node_parents[self.nodes_before(from)..self.nodes_before(to)].iter();
            let ours = ours.map(move |&it| match it {
                NO_PARENT => NO_PARENT,
                it if it >= from => moved(placed, g, it),
                it => moved(placed, gap(placed, it), it),
            });
            let theirs = placed.get(g).zip(subtree(g)).into_iter().flat_map(move |(it, with)| {
                let up = self.parent(it.lo).map_or(NO_PARENT, |up| moved(placed, gap(placed, up), up));
                with.node_parents.iter().map(move |&at| match at {
                    NO_PARENT => up,
                    at => at + it.new_lo,
                })
            });
            ours.chain(theirs)
        });
        let kinds_of = move |g: usize| {
            let (from, to) = gaps(g);
            let spliced = placed.get(g).map(|it| (it, self.spliced_kinds(it, subtree(g))));
            (in_range(&self.kinds, from, to), spliced)
        };
        let kinds_count = splices_and_gaps().map(|g| {
            let (ours, spliced) = kinds_of(g);
            ours.len() + spliced.map_or(0, |(_, (theirs, _))| theirs.len())
        });
        let kinds = splices_and_gaps().flat_map(move |g| {
            let (ours, spliced) = kinds_of(g);
            let ours = ours.iter().map(move |&(it, kind)| (moved(placed, g, it), kind));
            let theirs = spliced.into_iter().flat_map(|(it, (theirs, base))| {
                theirs.iter().map(move |&(at, kind)| (at - base + it.new_lo, kind))
            });
            ours.chain(theirs)
        });
        // A start is a splice's, from its own tree's, or that of the element it was in the
        // copies: a mark and up to seven lengths.
        let marks = (0..=len / MARK).map(move |mark| {
            let at = mark * MARK;
            let g = placed.partition_point(|it| it.new_lo + it.added <= at);
            match (placed.get(g).filter(|it| it.new_lo <= at), subtree(g)) {
                (Some(it), Some(with)) => {
                    with.offset(at - it.new_lo) - u32::from(with.range(0).start()) + it.new_start
                }
                (Some(it), None) => it.new_start,
                (None, _) => {
                    let old = match g {
                        0 => at,
                        g => at - (placed[g - 1].new_lo + placed[g - 1].added) + placed[g - 1].hi,
                    };
                    let (from, to) = text_shift(placed, g);
                    self.offset(old) - from + to
                }
            }
        });
        let columns = Columns {
            words: Joined(len as usize, words),
            nodes: &spare.nodes,
            node_parents: Made(parents.sum(), node_parents),
            far: far.as_slice(),
            kinds: Made(kinds_count.sum(), kinds),
            big: big.as_slice(),
            marks: Made((len / MARK + 1) as usize, marks),
        };
        let mut tree = FlatTree::pack(source, columns, lazy, kept.len(), |n| kept[n]);
        for &(at, word) in patches.iter() {
            tree.words[at as usize] = word;
        }
        tree
    }

    /// The tree with token `at`'s text replaced by `text` (its kind stays), over `source`, the
    /// new whole text: `text` is `source` at the token's start, and the rest of the root's text
    /// is as it was around it. No element but the token changes, so only its length and the
    /// starts kept after it are written: every lazy node stays lazy and every part already
    /// parsed is kept, those after the token moved to `source`.
    pub fn replace_token(&self, at: u32, text: &str, source: Text) -> FlatTree {
        self.splice_all(&[(at, Splice::Token(text))], source)
    }

    /// The lazy nodes, in preorder.
    pub fn lazies(&self) -> impl Iterator<Item = u32> + '_ {
        self.lazy_nodes().iter().copied()
    }

    /// The lazy nodes, in preorder, so ascending: a `partition_point` finds the first at or
    /// after an element.
    pub fn lazy_nodes(&self) -> &[u32] {
        match &self.lazy {
            Some(lazy) => &lazy.nodes,
            None => &[],
        }
    }

    fn slot(&self, at: u32) -> &OnceLock<Part> {
        let lazy = match (self.is_lazy(at), &self.lazy) {
            (true, Some(lazy)) => lazy,
            _ => panic!("element {at} ({:?}) is not a lazy node", self.kind(at)),
        };
        let it = lazy.nodes.binary_search(&at).expect("a lazy node is among the lazy nodes");
        &lazy.parts[it]
    }

    /// The parsed part of lazy node `at`, parsed on the first call (on one thread, the others
    /// waiting for it), with its errors ([`Self::part_errors`]): a tree whose root is `at`'s
    /// kind and range, its ranges in the same source (so absolute in the file), without lazy
    /// nodes of its own.
    pub fn part(&self, at: u32) -> &Arc<FlatTree> {
        &self.parsed(at).tree
    }

    /// The errors parsing lazy node `at`'s part found, their ranges in the same source (so
    /// absolute in the file): the part is parsed if it was not yet, as by [`Self::part`].
    pub fn part_errors(&self, at: u32) -> &[(TextRange, Box<str>)] {
        &self.parsed(at).errors
    }

    fn parsed(&self, at: u32) -> &Part {
        self.slot(at).get_or_init(|| self.expand(at))
    }

    /// Whether `at` is a lazy node whose part was parsed already.
    pub fn is_parsed(&self, at: u32) -> bool {
        self.is_lazy(at) && self.slot(at).get().is_some()
    }

    fn expand(&self, at: u32) -> Part {
        let lazy = self.lazy.as_deref().expect("a lazy node's tree has an expander");
        let (kind, range) = (self.kind(at), self.range(at));
        let (part, errors) = budget::charging(self.block.tag(), || {
            (lazy.expand)(kind, &self.source, range, lazy.data)
        });
        let root = part.range(0);
        // Over the same source at the same range, the part's text is the node's: no compare.
        assert!(
            !part.is_token(0)
                && part.kind(0) == kind
                && part.lazy.is_none()
                && part.source.ptr_eq(&self.source)
                && root == range,
            "the part of the lazy {kind:?} at {range:?} is not one {kind:?} over its text in \
             the tree's source without lazy nodes: its root is a {:?} at {root:?}",
            part.kind(0),
        );
        if let Some((error, _)) = errors.iter().find(|it| !range.contains_range(it.0)) {
            panic!("the part of the lazy {kind:?} at {range:?} has an error outside it, at {error:?}");
        }
        // alloc: once per lazy node, when its part is first parsed; shared by its cursors
        Part { tree: Arc::new(part), errors: errors.into() }
    }
}

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
    splicing: Splicing,
}

impl Default for FlatTreeBuilder {
    fn default() -> FlatTreeBuilder {
        FlatTreeBuilder::new()
    }
}

impl FlatTreeBuilder {
    pub fn new() -> FlatTreeBuilder {
        let Buffers { packer, marks, open, starts, lazies, kept, text, opened, spare, splicing } =
            Buffers::take();
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
            splicing,
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

    /// Where the next element starts.
    #[inline]
    fn offset(&self) -> usize {
        match self.source {
            Some(_) => self.next,
            None => self.text.len(),
        }
    }

    /// Pushes a node (`NODE`, its size set when it is finished) or a token or lazy node of
    /// `len` bytes.
    fn push(&mut self, kind: SyntaxKind, tag: u32, len: u32) -> u32 {
        if self.packer.words.len() % MARK as usize == 0 {
            self.marks.push(self.offset() as u32);
        }
        let parent = self.open.last().copied().unwrap_or(NO_PARENT);
        match tag {
            NODE => self.packer.node(kind, 0, parent),
            _ => self.packer.leaf(kind, tag, len, parent),
        }
    }

    pub fn start_node(&mut self, kind: SyntaxKind) {
        assert!(!self.open.is_empty() || self.packer.words.is_empty(), "a tree has one root");
        let at = self.push(kind, NODE, 0);
        self.open.push(at);
        self.starts.push(self.offset() as u32);
    }

    pub fn token(&mut self, kind: SyntaxKind, text: &str) {
        assert!(!self.open.is_empty(), "a token outside every node");
        let end = self.offset() + text.len();
        assert!(end <= u32::MAX as usize, "a text of 4 GiB or more");
        if let Some(source) = &self.source {
            // A token that is a slice of the source at the next byte is those bytes: only a
            // token copied from another text has its bytes compared.
            let here = text.as_ptr() == source.as_ptr().wrapping_add(self.next);
            assert!(
                end <= self.end
                    && (here || source.as_bytes().get(self.next..end) == Some(text.as_bytes())),
                "the {kind:?} {text:?} is not the source's next bytes, at {}",
                self.next,
            );
        }
        self.push(kind, TOKEN, text.len() as u32);
        match self.source {
            Some(_) => self.next = end,
            None => self.text.push_str(text),
        }
    }

    /// The source's next `len` bytes as one lazy node of `kind` (a body not parsed), with no
    /// children in the arrays: inside a node, or as the whole tree. Only for a builder with a
    /// source, and the tree must be given its expander before `finish`.
    pub fn lazy(&mut self, kind: SyntaxKind, len: u32) {
        assert!(!self.open.is_empty() || self.packer.words.is_empty(), "a tree has one root");
        let end = self.next + len as usize;
        match &self.source {
            None => panic!("a lazy {kind:?} in a tree built without a source"),
            Some(source) => assert!(
                end <= self.end && source.is_char_boundary(end),
                "a lazy {kind:?} of {len} bytes at {} does not end in the source",
                self.next,
            ),
        }
        let at = self.push(kind, LAZY, len);
        self.lazies.push(at);
        self.next = end;
    }

    /// What parses the tree's lazy nodes, and the `data` it is called with.
    pub fn expand_with(&mut self, expand: Expand, data: u32) {
        self.expand = Some((expand, data));
    }

    pub fn finish_node(&mut self) {
        let at = self.open.pop().expect("a node finished that was not started");
        let start = self.starts.pop().expect("a start for every node started");
        let size = self.packer.words.len() as u32 - at;
        self.packer.set_size(at, size);
        if keeps(self.offset() as u32 - start, size) {
            self.kept.push((at, 0));
        }
    }

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
        let FlatTreeBuilder {
            text, packer, marks, open, starts, lazies, kept, opened, spare, splicing, ..
        } = self;
        Buffers { packer, marks, open, starts, lazies, kept, text, opened, spare, splicing }.give();
        tree
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, hash_map::DefaultHasher},
        hash::{Hash, Hasher},
        ops::Range,
        sync::{
            Arc, Barrier,
            atomic::{AtomicU32, AtomicUsize, Ordering},
        },
    };

    use blaze_ids::rng::SplitMix64;
    use parking_lot::Mutex;

    use super::{keeps, Expand, FlatTree, Splice, HUGE, LONG};
    use crate::{
        cursor::{SyntaxElement, SyntaxNode},
        FlatTreeBuilder, NodeOrToken, SyntaxKind, Text, TextRange, TextSize, TokenAtOffset,
        WalkEvent,
    };

    /// A tree to build each way: a node is its kind, its children and, when it is lazy in an
    /// outline, the number of its part in [`PARTS`].
    #[derive(Clone, Debug)]
    pub(super) enum Shape {
        Node(SyntaxKind, Vec<Shape>, Option<usize>),
        Token(SyntaxKind, String),
    }

    /// A tree from a seed: nodes up to five deep, some empty, tokens of zero to three bytes.
    pub(super) fn random_shape(seed: u64) -> Shape {
        fn close(open: &mut Vec<(SyntaxKind, Vec<Shape>)>) {
            let (kind, children) = open.pop().unwrap();
            open.last_mut().unwrap().1.push(Shape::Node(kind, children, None));
        }
        let mut rng = SplitMix64::new(seed);
        let mut open = vec![(SyntaxKind(100), Vec::new())];
        for _ in 0..rng.below(60) {
            match rng.below(4) {
                0 if open.len() < 5 => {
                    open.push((SyntaxKind(100 + rng.below(5) as u16), Vec::new()))
                }
                1 if open.len() > 1 => close(&mut open),
                _ => {
                    let kind = SyntaxKind(rng.below(5) as u16);
                    let text = "abc"[..rng.below(4) as usize].to_owned();
                    open.last_mut().unwrap().1.push(Shape::Token(kind, text));
                }
            }
        }
        while open.len() > 1 {
            close(&mut open);
        }
        let (kind, children) = open.pop().unwrap();
        Shape::Node(kind, children, None)
    }

    /// A tree past what an element's word holds: rows of 170 tokens with a token between
    /// them, so most of those are more than a thousand elements after their parent; a few
    /// tokens of more than a thousand bytes; and some kinds past the word's.
    pub(super) fn big_shape(seed: u64) -> Shape {
        let mut rng = SplitMix64::new(seed);
        let mut rows = Vec::new();
        for row in 0..100u64 {
            let mut tokens = Vec::new();
            for i in 0..170u16 {
                let text = "abc"[..rng.below(4) as usize].to_owned();
                let kind = match rng.below(50) {
                    0 => SyntaxKind(1023 + rng.below(2000) as u16),
                    _ => SyntaxKind(i % 5),
                };
                tokens.push(Shape::Token(kind, text));
            }
            if row % 37 == seed % 37 {
                let long = "x".repeat(LONG as usize + rng.below(3000) as usize);
                tokens.insert(rng.below(170) as usize, Shape::Token(SyntaxKind(3), long));
            }
            rows.push(Shape::Node(SyntaxKind(101 + (row % 4) as u16), tokens, None));
            rows.push(Shape::Token(SyntaxKind(0), " ".into()));
        }
        let rows = Shape::Node(SyntaxKind(102), rows, None);
        Shape::Node(SyntaxKind(100), vec![rows, Shape::Token(SyntaxKind(1), "ab".into())], None)
    }

    /// The parts of the lazy nodes of every outline the tests build, by the number in their
    /// first token.
    static PARTS: Mutex<BTreeMap<usize, FlatTree>> = Mutex::new(BTreeMap::new());
    static NEXT_PART: AtomicUsize = AtomicUsize::new(0);
    const DATA: u32 = 0x5eed;

    /// A lazy node: its first token is `<n>`, `n` the number of its part.
    fn lazy_node(kind: SyntaxKind, mut children: Vec<Shape>) -> Shape {
        let n = NEXT_PART.fetch_add(1, Ordering::Relaxed);
        children.insert(0, Shape::Token(SyntaxKind(4), format!("<{n}>")));
        Shape::Node(kind, children, Some(n))
    }

    /// Makes some nodes below the root of `shape` lazy, none inside another.
    pub(super) fn make_lazy(shape: &mut Shape, rng: &mut SplitMix64) {
        let Shape::Node(_, children, _) = shape else { return };
        for child in children.iter_mut() {
            if let Shape::Node(kind, grandchildren, None) = child {
                if rng.below(3) == 0 {
                    *child = lazy_node(*kind, std::mem::take(grandchildren));
                    continue;
                }
            }
            make_lazy(child, rng);
        }
    }

    /// The expander of the tests' outlines: the part kept in [`PARTS`], rebuilt over the
    /// source at `range`. Its error is the part's number, at the number.
    pub(super) fn expand(
        kind: SyntaxKind,
        source: &Text,
        range: TextRange,
        data: u32,
    ) -> (FlatTree, Vec<(TextRange, Box<str>)>) {
        assert_eq!(data, DATA);
        let text = &source[range];
        let end = text.find('>').expect("a part's number");
        let n: usize = text[1..end].parse().unwrap();
        let part = PARTS.lock()[&n].clone();
        assert_eq!(part.kind(0), kind);
        let mut builder = FlatTreeBuilder::sourced_range(source.clone(), range);
        let part = SyntaxNode::new_flat_root(Arc::new(part));
        crate::cursor::copy_element(&mut builder, &NodeOrToken::Node(part));
        let at = TextRange::new(1.into(), (end as u32).into()) + range.start();
        (builder.finish(), vec![(at, n.to_string().into())])
    }

    /// The errors [`expand`] gives lazy node `at` of `tree`, absolute.
    fn expanded_errors(tree: &FlatTree, at: u32) -> Vec<(TextRange, Box<str>)> {
        let range = tree.range(at);
        let text = &tree.text()[range];
        let end = text.find('>').unwrap();
        let at = TextRange::new(1.into(), (end as u32).into()) + range.start();
        vec![(at, text[1..end].into())]
    }

    fn text_of(shape: &Shape, text: &mut String) {
        match shape {
            Shape::Node(_, children, _) => children.iter().for_each(|it| text_of(it, text)),
            Shape::Token(_, it) => text.push_str(it),
        }
    }

    /// The model the cursor is checked against, independent of it: every element of a
    /// [`Shape`] in preorder with its range, parent and children, each move read off by
    /// indexing vectors.
    struct Model {
        elements: Vec<Modelled>,
    }

    struct Modelled {
        kind: SyntaxKind,
        /// A token's text; `None` for a node.
        text: Option<String>,
        range: TextRange,
        parent: Option<usize>,
        /// The number among its parent's children.
        index: usize,
        children: Vec<usize>,
        /// The number after its subtree.
        end: usize,
    }

    type Described = Option<(SyntaxKind, TextRange, bool)>;

    impl Model {
        fn new(shape: &Shape) -> Model {
            fn push(
                shape: &Shape,
                parent: Option<usize>,
                index: usize,
                offset: &mut u32,
                elements: &mut Vec<Modelled>,
            ) {
                let (at, start) = (elements.len(), *offset);
                let (kind, text) = match shape {
                    Shape::Node(kind, ..) => (*kind, None),
                    Shape::Token(kind, text) => (*kind, Some(text.clone())),
                };
                let range = TextRange::empty(start.into());
                let children = Vec::new();
                elements.push(Modelled { kind, text, range, parent, index, children, end: at + 1 });
                match shape {
                    Shape::Node(_, children, _) => {
                        for (i, child) in children.iter().enumerate() {
                            let next = elements.len();
                            elements[at].children.push(next);
                            push(child, Some(at), i, offset, elements);
                        }
                    }
                    Shape::Token(_, text) => *offset += text.len() as u32,
                }
                elements[at].range = TextRange::new(start.into(), (*offset).into());
                elements[at].end = elements.len();
            }
            let mut elements = Vec::new();
            push(shape, None, 0, &mut 0, &mut elements);
            Model { elements }
        }

        fn describe(&self, at: Option<usize>) -> Described {
            at.map(|it| {
                let element = &self.elements[it];
                (element.kind, element.range, element.text.is_some())
            })
        }

        fn is_token(&self, at: usize) -> bool {
            self.elements[at].text.is_some()
        }

        fn sibling(&self, at: usize, next: bool) -> Option<usize> {
            let element = &self.elements[at];
            let siblings = &self.elements[element.parent?].children;
            match next {
                true => siblings.get(element.index + 1).copied(),
                false => element.index.checked_sub(1).map(|it| siblings[it]),
            }
        }

        fn node_sibling(&self, at: usize, next: bool) -> Option<usize> {
            let mut sibling = self.sibling(at, next);
            while let Some(it) = sibling {
                if !self.is_token(it) {
                    return Some(it);
                }
                sibling = self.sibling(it, next);
            }
            None
        }

        fn first_token(&self, at: usize) -> Option<usize> {
            match self.is_token(at) {
                true => Some(at),
                false => self.first_token(*self.elements[at].children.first()?),
            }
        }

        fn last_token(&self, at: usize) -> Option<usize> {
            match self.is_token(at) {
                true => Some(at),
                false => self.last_token(*self.elements[at].children.last()?),
            }
        }

        fn ancestors(&self, from: Option<usize>) -> impl Iterator<Item = usize> + '_ {
            std::iter::successors(from, |&it| self.elements[it].parent)
        }

        /// The token after (`next`) or before a token, its first or last token of the first
        /// sibling or token there is, its own or an ancestor's.
        fn token(&self, at: usize, next: bool) -> Option<usize> {
            let edge = |it| match next {
                true => self.first_token(it),
                false => self.last_token(it),
            };
            match self.sibling(at, next) {
                Some(it) => edge(it),
                None => self
                    .ancestors(self.elements[at].parent)
                    .find_map(|it| self.sibling(it, next))
                    .and_then(edge),
            }
        }

        fn text(&self, at: usize) -> String {
            (at..self.elements[at].end).filter_map(|it| self.elements[it].text.clone()).collect()
        }

        /// The child that holds `range` and is the one child that overlaps it; else, when
        /// none overlaps it, the last that ends at or before its start (or the first), if it
        /// holds `range`.
        fn child_at_range(&self, at: usize, range: TextRange) -> Option<usize> {
            let children = &self.elements[at].children;
            let range_of = |it: usize| self.elements[it].range;
            let overlaps = |it: usize| {
                let of = range_of(it);
                of.end() > range.start() && range.end() > of.start()
            };
            let overlapping: Vec<usize> = children.iter().copied().filter(|&it| overlaps(it)).collect();
            let found = match overlapping[..] {
                [] => {
                    let before =
                        children.iter().filter(|&&it| range_of(it).end() <= range.start()).count();
                    children.get(before.saturating_sub(1)).copied()
                }
                [one] => Some(one),
                _ => None,
            };
            found.filter(|&it| range_of(it).contains_range(range))
        }

        fn covering(&self, at: usize, range: TextRange) -> usize {
            let mut it = at;
            while !self.is_token(it) {
                match self.child_at_range(it, range) {
                    Some(child) => it = child,
                    None => break,
                }
            }
            it
        }

        /// The tokens not empty that hold `offset`, at either end or inside.
        fn tokens_at(&self, at: usize, offset: TextSize) -> Vec<usize> {
            if self.elements[at].range.is_empty() {
                return Vec::new();
            }
            (at..self.elements[at].end)
                .filter(|&it| {
                    let range = self.elements[it].range;
                    self.is_token(it)
                        && !range.is_empty()
                        && range.start() <= offset
                        && offset <= range.end()
                })
                .collect()
        }

        /// What stepping down by [`Self::child_at_range`] from `at` finds first of `kind` at
        /// `range`.
        fn stepped(&self, at: usize, range: TextRange, kind: SyntaxKind) -> Option<usize> {
            std::iter::successors(Some(at), |&it| {
                self.child_at_range(it, range).filter(|&child| !self.is_token(child))
            })
            .find(|&it| self.elements[it].range == range && self.elements[it].kind == kind)
        }

        /// Gives `builder` element `at` and its subtree.
        fn build(&self, at: usize, builder: &mut FlatTreeBuilder) {
            let element = &self.elements[at];
            match &element.text {
                Some(text) => builder.token(element.kind, text),
                None => {
                    builder.start_node(element.kind);
                    element.children.iter().for_each(|&it| self.build(it, builder));
                    builder.finish_node();
                }
            }
        }
    }

    /// Builds `shape`, its lazy nodes lazy when `outline`, keeping their parts in [`PARTS`].
    fn to_flat(shape: &Shape, builder: &mut FlatTreeBuilder, outline: bool) {
        fn node(kind: SyntaxKind, children: &[Shape], builder: &mut FlatTreeBuilder, outline: bool) {
            builder.start_node(kind);
            children.iter().for_each(|it| to_flat(it, builder, outline));
            builder.finish_node();
        }
        match shape {
            Shape::Node(kind, children, Some(n)) if outline => {
                let mut text = String::new();
                text_of(shape, &mut text);
                builder.lazy(*kind, text.len() as u32);
                let mut part = FlatTreeBuilder::new();
                node(*kind, children, &mut part, false);
                PARTS.lock().insert(*n, part.finish());
            }
            Shape::Node(kind, children, _) => node(*kind, children, builder, outline),
            Shape::Token(kind, text) => builder.token(*kind, text),
        }
    }

    /// `shape` as a flat tree in full, appending its text or over its source.
    pub(super) fn build_flat(shape: &Shape, sourced: bool) -> FlatTree {
        let mut builder = match sourced {
            true => {
                let mut text = String::new();
                text_of(shape, &mut text);
                FlatTreeBuilder::sourced(text.into())
            }
            false => FlatTreeBuilder::new(),
        };
        to_flat(shape, &mut builder, false);
        builder.finish()
    }

    /// `shape` as an outline over its source.
    pub(super) fn build_outline(shape: &Shape, expand: Expand) -> Arc<FlatTree> {
        let mut text = String::new();
        text_of(shape, &mut text);
        let mut builder = FlatTreeBuilder::sourced(text.into());
        to_flat(shape, &mut builder, true);
        builder.expand_with(expand, DATA);
        Arc::new(builder.finish())
    }

    fn describe(element: Option<SyntaxElement>) -> Described {
        element.map(|it| (it.kind(), it.text_range(), it.as_token().is_some()))
    }

    fn elements(root: &SyntaxNode) -> Vec<SyntaxElement> {
        root.preorder_with_tokens()
            .filter_map(|it| match it {
                WalkEvent::Enter(it) => Some(it),
                WalkEvent::Leave(_) => None,
            })
            .collect()
    }

    fn hash_of(node: &SyntaxNode) -> u64 {
        let mut state = DefaultHasher::new();
        node.hash(&mut state);
        state.finish()
    }

    /// The ranges a node's checks look at when not every range: its own, its children's,
    /// those one byte shorter at either end, and the empty ones at their starts.
    fn around(model: &Model, at: usize) -> Vec<TextRange> {
        let one = TextSize::from(1);
        let node = &model.elements[at];
        let mut ranges = vec![node.range, TextRange::empty(node.range.end())];
        for &child in &node.children {
            let range = model.elements[child].range;
            ranges.extend([range, TextRange::empty(range.start())]);
            if range.len() > one {
                ranges.extend([
                    TextRange::new(range.start() + one, range.end()),
                    TextRange::new(range.start(), range.end() - one),
                ]);
            }
        }
        ranges
    }

    /// Every function of the cursor on every element of `flat` against `model`, the same tree:
    /// with every range in every node when `exhaustive`, else the ranges [`around`] it.
    fn compare(model: &Model, flat: &SyntaxNode, exhaustive: bool, seed: u64) {
        assert_eq!(model.text(0), flat.to_string());
        let flats = elements(flat);
        assert_eq!(model.elements.len(), flats.len());
        for (m, f) in flats.iter().enumerate() {
            let element = &model.elements[m];
            assert_eq!(model.describe(Some(m)), describe(Some(f.clone())), "seed {seed}");
            assert_eq!(element.index, f.index());
            assert_eq!(model.describe(element.parent), describe(f.parent().map(Into::into)));
            assert_eq!(model.describe(model.sibling(m, true)), describe(f.next_sibling_or_token()));
            assert_eq!(model.describe(model.sibling(m, false)), describe(f.prev_sibling_or_token()));
            assert_eq!(model.describe(model.first_token(m)), describe(f.first_token().map(Into::into)));
            assert_eq!(model.describe(model.last_token(m)), describe(f.last_token().map(Into::into)));
            match f {
                NodeOrToken::Node(f) => {
                    let node = |it: Option<SyntaxNode>| describe(it.map(Into::into));
                    let children = &element.children;
                    let first = children.iter().copied().find(|&it| !model.is_token(it));
                    let last = children.iter().copied().rev().find(|&it| !model.is_token(it));
                    assert_eq!(model.describe(first), node(f.first_child()));
                    assert_eq!(model.describe(last), node(f.last_child()));
                    assert_eq!(model.describe(model.node_sibling(m, true)), node(f.next_sibling()));
                    assert_eq!(model.describe(model.node_sibling(m, false)), node(f.prev_sibling()));
                    assert_eq!(model.describe(children.first().copied()), describe(f.first_child_or_token()));
                    assert_eq!(model.describe(children.last().copied()), describe(f.last_child_or_token()));
                    assert_eq!(model.text(m), f.text().to_string());
                    let first_text = children.first().and_then(|&it| model.elements[it].text.as_deref());
                    assert_eq!(first_text, f.first_child_token_text());
                    let range = element.range;
                    let (start, end) = (u32::from(range.start()), u32::from(range.end()));
                    let ranges: Vec<TextRange> = match exhaustive {
                        true => (start..=end)
                            .flat_map(|from| {
                                (from..=end).map(move |to| TextRange::new(from.into(), to.into()))
                            })
                            .collect(),
                        false => around(model, m),
                    };
                    for &at in &ranges {
                        assert_eq!(
                            model.describe(model.child_at_range(m, at)),
                            describe(f.child_or_token_at_range(at)),
                            "seed {seed}, {at:?} in {range:?}"
                        );
                        assert_eq!(
                            model.describe(Some(model.covering(m, at))),
                            describe(Some(f.covering_element(at)))
                        );
                        for kind in (0..5).chain(100..105).map(SyntaxKind) {
                            let stepped = |from: &SyntaxNode| {
                                std::iter::successors(Some(from.clone()), |it| {
                                    it.child_or_token_at_range(at)?.into_node()
                                })
                                .find(|it| it.text_range() == at && it.kind() == kind)
                            };
                            let found =
                                f.flat_node_at_range(at, kind).unwrap_or_else(|| stepped(f));
                            assert_eq!(
                                model.describe(model.stepped(m, at, kind)),
                                found.and_then(|it| describe(Some(it.into()))),
                                "seed {seed}, {kind:?} at {at:?}"
                            );
                        }
                    }
                    let offsets: Vec<u32> = match exhaustive {
                        true => (start..=end).collect(),
                        false => ranges.iter().map(|it| u32::from(it.start())).collect(),
                    };
                    for offset in offsets {
                        let offset = TextSize::from(offset);
                        let expected: Vec<Described> =
                            model.tokens_at(m, offset).into_iter().map(|it| model.describe(Some(it))).collect();
                        let found = f.token_at_offset(offset);
                        assert_eq!(found.clone().count(), expected.len(), "seed {seed}, {offset:?}");
                        assert!(matches!(
                            (&found, expected.len()),
                            (TokenAtOffset::None, 0) | (TokenAtOffset::Single(_), 1) | (TokenAtOffset::Between(..), 2)
                        ));
                        let found: Vec<Described> =
                            found.map(|it| describe(Some(NodeOrToken::Token(it)))).collect();
                        assert_eq!(found, expected, "seed {seed}, {offset:?}");
                    }
                }
                NodeOrToken::Token(f) => {
                    assert_eq!(element.text.as_deref(), Some(f.text()));
                    let token = |it: Option<_>| describe(it.map(NodeOrToken::Token));
                    assert_eq!(model.describe(model.token(m, true)), token(f.next_token()));
                    assert_eq!(model.describe(model.token(m, false)), token(f.prev_token()));
                }
            }
        }
        let hash = |it: &SyntaxNode| {
            let (tree, at) = it.flat().unwrap();
            let mut state = DefaultHasher::new();
            tree.hash_subtree(at, &mut state);
            state.finish()
        };
        // Whether the arrays hold all of a node's subtree, no lazy node in it.
        let full = |it: &SyntaxNode| {
            let (tree, at) = it.flat().unwrap();
            (at..tree.end(at)).all(|it| !tree.is_lazy(it))
        };
        for (m, f) in flats.iter().enumerate().filter(|(_, f)| f.as_node().is_some()) {
            let (element, f) = (&model.elements[m], f.as_node().unwrap());
            let alone = f.clone_subtree();
            assert_eq!(alone.to_string(), model.text(m));
            let start = element.range.start();
            let copied: Vec<Described> = elements(&alone).into_iter().map(|it| describe(Some(it))).collect();
            let expected: Vec<Described> = (m..element.end)
                .map(|it| model.describe(Some(it)).map(|(kind, range, token)| (kind, range - start, token)))
                .collect();
            assert_eq!(copied, expected, "seed {seed}");
            assert!(alone.parent().is_none());
            assert_eq!(hash(f), hash(&alone), "seed {seed}");
            if full(f) {
                let mut again = FlatTreeBuilder::new();
                model.build(m, &mut again);
                assert_eq!(alone.flat_tree().map(|it| &**it), Some(&again.finish()));
            }
            assert_eq!(f.to_flat(), *f);
            let of_kind = |it: &usize, kind: SyntaxKind| model.elements[*it].kind == kind;
            for kind in (0..5).chain(100..105).map(SyntaxKind) {
                let ancestor = model.ancestors(Some(m)).find(|it| of_kind(it, kind));
                let node = |it: Option<SyntaxNode>| describe(it.map(Into::into));
                assert_eq!(model.describe(ancestor), node(f.ancestor_where(|it| it == kind)));
                let child = element.children.iter().find(|&it| !model.is_token(*it) && of_kind(it, kind));
                assert_eq!(model.describe(child.copied()), node(f.child_where(|it| it == kind)));
                let token = element.children.iter().find(|&it| model.is_token(*it) && of_kind(it, kind));
                let found = f.child_token(kind).map(NodeOrToken::Token);
                assert_eq!(model.describe(token.copied()), describe(found));
            }
            let mut odd = f.children_where();
            let by_kind = std::iter::from_fn(|| odd.next_where(|it| it.0 % 2 == 1));
            let by_kind: Vec<Described> = by_kind.map(|it| describe(Some(it.into()))).collect();
            let children: Vec<Described> = (element.children.iter().copied())
                .filter(|&it| !model.is_token(it) && model.elements[it].kind.0 % 2 == 1)
                .map(|it| model.describe(Some(it)))
                .collect();
            assert_eq!(by_kind, children);
            let (tree, at) = f.flat().unwrap();
            if !tree.is_lazy(at) {
                let children: Vec<_> = f.children().collect();
                let read: Vec<_> = tree.children(at).map(|it| f.flat_element(it)).collect();
                assert_eq!(read, children.into_iter().map(NodeOrToken::Node).collect::<Vec<_>>());
            }
            if full(f) {
                let descendants = f.descendants().map(|it| (it.kind(), it.text_range()));
                assert!(
                    descendants.eq(tree.descendants(at).map(|it| (tree.kind(it), tree.range(it))))
                );
            }
        }
    }

    #[test]
    fn a_flat_tree_answers_as_its_model() {
        for seed in 0..2000 {
            let shape = random_shape(seed);
            let flat = SyntaxNode::new_flat_root(Arc::new(build_flat(&shape, seed % 2 == 1)));
            compare(&Model::new(&shape), &flat, true, seed);
        }
        for seed in 0..4 {
            let shape = big_shape(seed);
            let tree = build_flat(&shape, seed % 2 == 1);
            assert!(tree.big.iter().any(|&(it, _)| tree.is_token(it)), "a token past the word");
            assert!(tree.far.len() >= 50, "parents past the word: {}", tree.far.len());
            assert!(!tree.kinds.is_empty(), "kinds past the word");
            compare(&Model::new(&shape), &SyntaxNode::new_flat_root(Arc::new(tree)), false, seed);
        }
    }

    #[test]
    fn a_tree_of_millions_of_elements_and_a_token_of_megabytes() {
        const ROWS: u32 = 30_000;
        let long = "y".repeat(5 << 20);
        let mut builder = FlatTreeBuilder::new();
        builder.start_node(SyntaxKind(100));
        for _ in 0..ROWS {
            builder.start_node(SyntaxKind(101));
            for _ in 0..99 {
                builder.token(SyntaxKind(1), "ab");
            }
            builder.finish_node();
        }
        builder.token(SyntaxKind(2), &long);
        builder.finish_node();
        let tree = builder.finish();
        assert_eq!(tree.len(), 1 + ROWS * 100 + 1);
        assert!(tree.len() > HUGE && tree.big.iter().any(|&(it, _)| it == 0), "a size past the word");
        assert_eq!(tree.end(0), tree.len());
        assert_eq!(tree.parent(tree.len() - 1), Some(0));
        assert_eq!(tree.far.len(), 1, "only the last token is far from its parent");
        let (nodes, tokens) = (ROWS as usize + 1, tree.len() as usize - ROWS as usize - 1);
        let budget = tokens * 4 + nodes * 8 + tree.len() as usize * 3 / 4 + 64;
        assert!(tree.heap_size() <= budget, "{} bytes, past the budget of {budget}", tree.heap_size());
        for row in [0, 1, 7, 12_345, ROWS - 1] {
            let at = 1 + row * 100;
            assert_eq!((tree.kind(at), tree.end(at), tree.parent(at)), (SyntaxKind(101), at + 100, Some(0)));
            assert_eq!(tree.range(at), TextRange::at((row * 198).into(), 198.into()));
            assert_eq!(tree.parent(at + 99), Some(at));
            assert_eq!(tree.range(at + 99), TextRange::at((row * 198 + 196).into(), 2.into()));
        }
        let last = tree.len() - 1;
        assert_eq!(tree.range(last), TextRange::at((ROWS * 198).into(), (5u32 << 20).into()));
        assert_eq!(tree.token_text(last), long);
        assert_eq!(tree.range(0).end(), TextSize::of(tree.text()));
        let root = SyntaxNode::new_flat_root(Arc::new(tree));
        let found = root.covering_element(TextRange::at((12_345 * 198 + 3).into(), 1.into()));
        assert_eq!(found.text_range(), TextRange::at((12_345 * 198 + 2).into(), 2.into()));
        let row = found.parent().unwrap();
        assert_eq!(row.text_range(), TextRange::at((12_345 * 198).into(), 198.into()));
        assert_eq!(row.index(), 12_345);
        let in_long = TextRange::at((ROWS * 198 + 1_000_000).into(), 5.into());
        assert_eq!(root.covering_element(in_long).kind(), SyntaxKind(2));
        let alone = row.clone_subtree();
        assert_eq!(alone.text_range(), TextRange::up_to(198.into()));
        assert_eq!(alone.to_string(), "ab".repeat(99));
    }

    #[test]
    fn an_outline_answers_as_the_tree_in_full() {
        for seed in 0..2004 {
            let big = seed >= 2000;
            let mut shape = match big {
                true => big_shape(seed),
                false => random_shape(seed),
            };
            make_lazy(&mut shape, &mut SplitMix64::new(seed ^ 0x1a2b));
            let tree = build_outline(&shape, expand);
            let lazies: Vec<u32> = tree.lazies().collect();
            let outline = SyntaxNode::new_flat_root(Arc::clone(&tree));
            for &at in &lazies {
                let node = outline.flat_element(at).into_node().unwrap();
                assert!(tree.is_lazy(at) && node.is_lazy() && !tree.is_parsed(at));
                assert_eq!(tree.end(at), at + 1);
                let (of, n) = node.flat().unwrap();
                assert!(Arc::ptr_eq(of, &tree) && n == at);
            }
            // Reading the arrays parses nothing.
            tree.hash_subtree(0, &mut DefaultHasher::new());
            assert_eq!(tree.descendants(0).filter(|&it| tree.is_lazy(it)).count(), lazies.len());
            let _ = tree.subtree(0);
            assert!(lazies.iter().all(|&at| !tree.is_parsed(at)));
            compare(&Model::new(&shape), &outline, !big, seed);
            for &at in &lazies {
                let node = outline.flat_element(at).into_node().unwrap();
                assert!(!node.is_lazy() && tree.is_parsed(at));
                let part = tree.part(at);
                assert_eq!(tree.part_errors(at), &expanded_errors(&tree, at)[..]);
                assert!(part.source().ptr_eq(tree.source()));
                assert_eq!((part.kind(0), part.range(0)), (tree.kind(at), tree.range(at)));
                assert_eq!(part.lazies().count(), 0);
                for child in node.children_with_tokens() {
                    let parent = child.parent().unwrap();
                    assert_eq!(parent, node);
                    assert_eq!(hash_of(&parent), hash_of(&node));
                    assert_eq!(parent.index(), node.index());
                    let (of, n) = parent.flat().unwrap();
                    assert!(Arc::ptr_eq(of, &tree) && n == at);
                    if let NodeOrToken::Node(child) = &child {
                        let (of, n) = child.flat().unwrap();
                        assert!(Arc::ptr_eq(of, part));
                        assert_eq!(part.parent(n), Some(0));
                        assert_eq!(child.flat_element(0), NodeOrToken::Node(node.clone()));
                        assert!(child.ancestors().any(|it| it == node));
                        assert_eq!(child.tree_top(), outline);
                    }
                }
            }
        }
    }

    #[test]
    fn a_part_is_parsed_once_whichever_thread_asks() {
        static CALLS: AtomicU32 = AtomicU32::new(0);
        fn counting(
            kind: SyntaxKind,
            source: &Text,
            range: TextRange,
            data: u32,
        ) -> (FlatTree, Vec<(TextRange, Box<str>)>) {
            CALLS.fetch_add(1, Ordering::SeqCst);
            blaze_spin::busy(std::time::Duration::from_millis(20));
            expand(kind, source, range, data)
        }
        let body = vec![
            Shape::Token(SyntaxKind(2), "{".into()),
            Shape::Node(SyntaxKind(102), vec![Shape::Token(SyntaxKind(3), "x".into())], None),
            Shape::Token(SyntaxKind(2), "}".into()),
        ];
        let children = vec![Shape::Token(SyntaxKind(1), "fn".into()), lazy_node(SyntaxKind(101), body)];
        let tree = build_outline(&Shape::Node(SyntaxKind(100), children.clone(), None), counting);
        let at = tree.lazies().next().unwrap();
        let barrier = Barrier::new(4);
        let parts: Vec<Arc<FlatTree>> = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        Arc::clone(tree.part(at))
                    })
                })
                .collect();
            threads.into_iter().map(|it| it.join().unwrap()).collect()
        });
        assert!(parts.iter().all(|it| Arc::ptr_eq(it, &parts[0])));
        assert!(Arc::ptr_eq(&parts[0], tree.part(at)));
        assert_eq!(tree.part_errors(at), &expanded_errors(&tree, at)[..]);
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
        assert!(tree.is_parsed(at));

        // Asking for the errors first parses the part, once.
        let tree = build_outline(&Shape::Node(SyntaxKind(100), children, None), counting);
        let at = tree.lazies().next().unwrap();
        let before = tree.heap_size();
        assert_eq!(tree.part_errors(at), &expanded_errors(&tree, at)[..]);
        assert!(tree.is_parsed(at) && tree.heap_size() > before);
        let _ = tree.part(at);
        assert_eq!(CALLS.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_sourced_tree_and_its_parts_share_the_source() {
        let body = vec![
            Shape::Token(SyntaxKind(2), "{ ".into()),
            Shape::Token(SyntaxKind(3), "body".into()),
            Shape::Token(SyntaxKind(2), " }".into()),
        ];
        let children = vec![Shape::Token(SyntaxKind(1), "fn f() ".into()), lazy_node(SyntaxKind(101), body)];
        let shape = Shape::Node(SyntaxKind(100), children, None);
        let mut text = String::new();
        text_of(&shape, &mut text);
        let source = Text::from(text);
        let mut builder = FlatTreeBuilder::sourced(source.clone());
        to_flat(&shape, &mut builder, true);
        builder.expand_with(expand, DATA);
        let tree = Arc::new(builder.finish());
        assert!(tree.source().ptr_eq(&source));
        let at = tree.lazies().next().unwrap();
        let mut builder = FlatTreeBuilder::sourced(source.clone());
        to_flat(&shape, &mut builder, false);
        let full = builder.finish();
        assert!(full.source().ptr_eq(&source));
        assert_eq!(full, build_flat(&shape, false));
        assert!(full.heap_size() <= full.len() as usize * 9 + 8, "arrays only, no text");
        let root = SyntaxNode::new_flat_root(Arc::clone(&tree));
        let body = root.last_child().unwrap();
        assert!(body.is_lazy());
        let token = body.last_token().unwrap();
        assert!(!body.is_lazy());
        assert_eq!(token.text(), " }");
        assert_eq!(token.text_range().end(), TextSize::of(&*source));
        assert_eq!(token.parent(), Some(body));
        let (of, n) = token.parent().unwrap().flat().map(|(it, n)| (Arc::clone(it), n)).unwrap();
        assert!(Arc::ptr_eq(&of, &tree) && n == at);
        assert!(tree.part(at).source().ptr_eq(&source));
    }

    #[test]
    #[should_panic(expected = "is not the source's next bytes")]
    fn a_sourced_builder_takes_only_its_source() {
        let mut builder = FlatTreeBuilder::sourced("ab".into());
        builder.start_node(SyntaxKind(100));
        builder.token(SyntaxKind(1), "b");
    }

    #[test]
    fn a_tree_over_a_range_of_a_source_shares_it() {
        let source = Text::from("fn f() { body }");
        let range = TextRange::new(7.into(), 15.into());
        let mut builder = FlatTreeBuilder::sourced_range(source.clone(), range);
        builder.start_node(SyntaxKind(101));
        // Slices of the source are taken by address, a copy by its bytes.
        builder.token(SyntaxKind(2), &source[7..9]);
        builder.token(SyntaxKind(3), "body");
        builder.token(SyntaxKind(2), &source[13..15]);
        builder.finish_node();
        let tree = builder.finish();
        assert!(tree.source().ptr_eq(&source));
        assert_eq!(tree.range(0), range);
        assert_eq!(tree.range(2), TextRange::new(9.into(), 13.into()));
        assert_eq!(tree.token_text(3), " }");
    }

    #[test]
    #[should_panic(expected = "the tree ends at byte 9")]
    fn a_tree_over_a_range_reaches_its_end() {
        let source = Text::from("fn f() { body }");
        let mut builder = FlatTreeBuilder::sourced_range(source.clone(), TextRange::new(7.into(), 15.into()));
        builder.start_node(SyntaxKind(101));
        builder.token(SyntaxKind(2), &source[7..9]);
        builder.finish_node();
        let _ = builder.finish();
    }

    #[test]
    #[should_panic(expected = "is not the source's next bytes")]
    fn a_tree_over_a_range_stays_in_it() {
        let source = Text::from("fn f() { body }");
        let mut builder = FlatTreeBuilder::sourced_range(source.clone(), TextRange::new(7.into(), 9.into()));
        builder.start_node(SyntaxKind(101));
        builder.token(SyntaxKind(2), &source[7..10]);
    }

    #[test]
    #[should_panic(expected = "the part of the lazy")]
    fn a_part_must_be_its_lazy_node_over_its_text() {
        fn short(
            kind: SyntaxKind,
            source: &Text,
            range: TextRange,
            _: u32,
        ) -> (FlatTree, Vec<(TextRange, Box<str>)>) {
            let range = TextRange::new(range.start() + TextSize::from(1), range.end());
            let mut builder = FlatTreeBuilder::sourced_range(source.clone(), range);
            builder.start_node(kind);
            builder.token(SyntaxKind(1), &source[range]);
            builder.finish_node();
            (builder.finish(), Vec::new())
        }
        let mut builder = FlatTreeBuilder::sourced("{x}".into());
        builder.start_node(SyntaxKind(100));
        builder.lazy(SyntaxKind(101), 3);
        builder.finish_node();
        builder.expand_with(short, 0);
        let _ = builder.finish().part(1);
    }

    /// The elements of `shape`'s outline: a lazy node is one.
    fn count(shape: &Shape) -> u32 {
        match shape {
            Shape::Node(_, children, None) => 1 + children.iter().map(count).sum::<u32>(),
            _ => 1,
        }
    }

    /// `shape` with the node that is element `at` of its outline replaced by `with`, `next`
    /// the number of `shape` in it.
    fn replaced(shape: &Shape, at: u32, with: &Shape, next: &mut u32) -> Shape {
        if *next == at {
            *next += count(shape);
            return with.clone();
        }
        *next += 1;
        match shape {
            Shape::Node(kind, children, None) => {
                let children = children.iter().map(|it| replaced(it, at, with, next)).collect();
                Shape::Node(*kind, children, None)
            }
            _ => shape.clone(),
        }
    }

    #[test]
    fn replacing_a_subtree_is_building_the_edited_tree() {
        for seed in 0..1000 {
            let mut rng = SplitMix64::new(seed ^ 0x9e37);
            let mut shape = random_shape(seed);
            make_lazy(&mut shape, &mut rng);
            let tree = build_outline(&shape, expand);
            for (i, at) in tree.lazies().enumerate() {
                if i % 2 == 0 {
                    let _ = tree.part(at);
                }
            }
            let nodes: Vec<u32> = (0..tree.len()).filter(|&it| !tree.is_token(it)).collect();
            let at = nodes[rng.below(nodes.len() as u64) as usize];
            let mut with = random_shape(seed + 10_000);
            if let Shape::Node(kind, ..) = &mut with {
                *kind = tree.kind(at);
            }
            if rng.below(2) == 0 {
                make_lazy(&mut with, &mut rng);
            }
            let edited = replaced(&shape, at, &with, &mut 0);
            let mut text = String::new();
            text_of(&edited, &mut text);
            let (lo, hi) = (at, tree.end(at));
            let carried =
                tree.lazies().filter(|&it| (it < lo || it >= hi) && tree.is_parsed(it)).count();
            let spliced = tree.replace_subtree(at, &build_outline(&with, expand), text.into());
            let built = build_outline(&edited, expand);
            same_arrays(&spliced, &built, seed);
            for it in 0..built.len() {
                assert_eq!(
                    (spliced.parent(it), spliced.range(it), spliced.end(it), spliced.is_lazy(it)),
                    (built.parent(it), built.range(it), built.end(it), built.is_lazy(it)),
                    "seed {seed}, element {it}"
                );
            }
            assert!(spliced.lazies().eq(built.lazies()));
            assert_eq!(spliced.lazies().filter(|&it| spliced.is_parsed(it)).count(), carried);
            for it in spliced.lazies() {
                let parsed = spliced.is_parsed(it);
                assert_eq!(spliced.part(it), built.part(it), "seed {seed}, lazy {it}");
                assert_eq!(spliced.part_errors(it), built.part_errors(it), "seed {seed}, {parsed}");
                assert_eq!(spliced.part_errors(it), &expanded_errors(&spliced, it)[..]);
            }
            // A copy of a subtree keeps the parts parsed, their errors moved with them.
            let copy = spliced.subtree(at);
            for it in copy.lazies() {
                assert!(copy.is_parsed(it), "seed {seed}");
                assert_eq!(copy.part_errors(it), &expanded_errors(&copy, it)[..], "seed {seed}");
            }
            compare(&Model::new(&edited), &SyntaxNode::new_flat_root(Arc::new(spliced)), true, seed);
        }
    }

    /// `spliced`, made by copying around an edit, is `built`, the edited tree built anew, in
    /// every array, those equality leaves out too.
    pub(super) fn same_arrays(spliced: &FlatTree, built: &FlatTree, seed: u64) {
        assert_eq!(spliced, built, "seed {seed}");
        assert_eq!(
            (&spliced.nodes, &spliced.ranks, &spliced.node_parents, &spliced.far, &spliced.marks),
            (&built.nodes, &built.ranks, &built.node_parents, &built.far, &built.marks),
            "seed {seed}"
        );
    }

    #[test]
    fn replacing_a_token_is_building_the_edited_tree() {
        for seed in 0..1000 {
            let mut rng = SplitMix64::new(seed ^ 0x7f4a);
            let mut shape = random_shape(seed);
            let outline = seed % 2 == 1;
            if outline {
                make_lazy(&mut shape, &mut rng);
            }
            let mut tree = match outline {
                true => build_outline(&shape, expand),
                false => Arc::new(build_flat(&shape, seed % 4 == 2)),
            };
            for (i, at) in tree.lazies().enumerate() {
                if i % 2 == 0 {
                    let _ = tree.part(at);
                }
            }
            // A few replacements in a row, so a long token is also made short again.
            for step in 0..3 {
                let tokens: Vec<u32> = (0..tree.len()).filter(|&it| tree.is_token(it)).collect();
                if tokens.is_empty() {
                    break;
                }
                let at = tokens[rng.below(tokens.len() as u64) as usize];
                let text = match rng.below(5) {
                    0 => String::new(),
                    1 => "x".into(),
                    2 => "abc".into(),
                    3 => "y".repeat(LONG as usize + rng.below(3) as usize),
                    _ => "é".into(),
                };
                let edited = replaced(&shape, at, &Shape::Token(tree.kind(at), text.clone()), &mut 0);
                let mut source = String::new();
                text_of(&edited, &mut source);
                let parsed: Vec<u32> = tree.lazies().filter(|&it| tree.is_parsed(it)).collect();
                let new = tree.replace_token(at, &text, source.into());
                let built = match outline {
                    true => build_outline(&edited, expand),
                    false => Arc::new(build_flat(&edited, false)),
                };
                assert_eq!(new, *built, "seed {seed}, step {step}");
                for it in 0..built.len() {
                    assert_eq!(
                        (new.parent(it), new.range(it), new.end(it), new.is_lazy(it)),
                        (built.parent(it), built.range(it), built.end(it), built.is_lazy(it)),
                        "seed {seed}, step {step}, element {it}"
                    );
                }
                assert!(new.lazies().eq(built.lazies()));
                assert!(new.lazies().filter(|&it| new.is_parsed(it)).eq(parsed.iter().copied()));
                for &it in &parsed {
                    assert_eq!(new.part(it), built.part(it), "seed {seed}, lazy {it}");
                    assert_eq!(new.part_errors(it), &expanded_errors(&new, it)[..]);
                    assert!(new.part(it).source().ptr_eq(new.source()));
                }
                compare(&Model::new(&edited), &SyntaxNode::new_flat_root(Arc::new(new.clone())), false, seed);
                assert!(new.lazies().filter(|&it| !parsed.contains(&it)).all(|it| !new.is_parsed(it)));
                (shape, tree) = (edited, Arc::new(new));
            }
        }
    }

    #[test]
    #[cfg_attr(not(debug_assertions), ignore = "the text is compared under debug assertions")]
    #[should_panic(expected = "is not the old one")]
    fn a_token_is_replaced_only_by_its_text() {
        let shape = Shape::Node(
            SyntaxKind(100),
            vec![Shape::Token(SyntaxKind(1), "ab".into()), Shape::Token(SyntaxKind(2), "c".into())],
            None,
        );
        let tree = build_flat(&shape, false);
        let _ = tree.replace_token(1, "zz", "zzc".into());
        let _ = tree.replace_token(1, "zz", tree.source().clone());
    }

    #[test]
    fn replacing_in_a_big_tree_moves_what_is_past_the_word() {
        for seed in 0..3 {
            let shape = big_shape(seed);
            let tree = build_flat(&shape, true);
            let row = tree.children(tree.first_child(0).unwrap()).nth(seed as usize * 40).unwrap();
            let with = Shape::Node(tree.kind(row), vec![Shape::Token(SyntaxKind(3), "z".repeat(LONG as usize + 5))], None);
            let edited = replaced(&shape, row, &with, &mut 0);
            let mut text = String::new();
            text_of(&edited, &mut text);
            let spliced = tree.replace_subtree(row, &build_flat(&with, false), text.into());
            let built = build_flat(&edited, true);
            same_arrays(&spliced, &built, seed);
            assert!((0..built.len()).all(|it| spliced.parent(it) == built.parent(it)));
            compare(&Model::new(&edited), &SyntaxNode::new_flat_root(Arc::new(spliced)), false, seed);
        }
    }

    /// Disjoint subtrees and tokens spliced at once make the tree built anew from the edited
    /// shape, and the tree the same replacements make one by one.
    #[test]
    fn splicing_many_at_once_is_replacing_them_one_by_one() {
        for seed in 0..1000 {
            let mut rng = SplitMix64::new(seed ^ 0x3c6e);
            let mut shape = random_shape(seed);
            let outline = seed % 2 == 1;
            if outline {
                make_lazy(&mut shape, &mut rng);
            }
            let tree = match outline {
                true => build_outline(&shape, expand),
                false => Arc::new(build_flat(&shape, seed % 4 == 2)),
            };
            for (i, at) in tree.lazies().enumerate() {
                if i % 2 == 0 {
                    let _ = tree.part(at);
                }
            }
            // Each pick past the subtree of the one before, the first maybe the root: always one,
            // as `splice_all` refuses none.
            let (mut picks, mut next) = (Vec::new(), (rng.below(3) as u32).min(tree.len() - 1));
            while next < tree.len() && picks.len() < 6 {
                picks.push(next);
                next = tree.end(next) + rng.below(4) as u32;
            }
            let edits: Vec<(u32, Shape, Option<Arc<FlatTree>>)> = (picks.iter())
                .map(|&at| match tree.is_token(at) {
                    true => {
                        let text = match rng.below(4) {
                            0 => String::new(),
                            1 => "x".into(),
                            2 => "y".repeat(LONG as usize + rng.below(3) as usize),
                            _ => "é".into(),
                        };
                        (at, Shape::Token(tree.kind(at), text), None)
                    }
                    false => {
                        let mut with = random_shape(seed * 8 + at as u64 + 40_000);
                        if let Shape::Node(kind, ..) = &mut with {
                            *kind = tree.kind(at);
                        }
                        if outline && rng.below(2) == 0 {
                            make_lazy(&mut with, &mut rng);
                        }
                        let built = build_outline(&with, expand);
                        (at, with, Some(built))
                    }
                })
                .collect();
            let splices: Vec<(u32, Splice<'_>)> = (edits.iter())
                .map(|(at, with, built)| match (with, built) {
                    (_, Some(built)) => (*at, Splice::Subtree(built)),
                    (Shape::Token(_, text), None) => (*at, Splice::Token(text)),
                    (Shape::Node(..), None) => panic!("a node's edit is built"),
                })
                .collect();
            let mut edited = shape.clone();
            for (at, with, _) in edits.iter().rev() {
                edited = replaced(&edited, *at, with, &mut 0);
            }
            let mut text = String::new();
            text_of(&edited, &mut text);
            let at_once = tree.splice_all(&splices, text.into());
            let built = match outline {
                true => build_outline(&edited, expand),
                false => Arc::new(build_flat(&edited, false)),
            };
            same_arrays(&at_once, &built, seed);
            for it in 0..built.len() {
                assert_eq!(
                    (at_once.parent(it), at_once.range(it), at_once.end(it), at_once.is_lazy(it)),
                    (built.parent(it), built.range(it), built.end(it), built.is_lazy(it)),
                    "seed {seed}, element {it}"
                );
            }
            assert!(at_once.lazies().eq(built.lazies()));
            for it in at_once.lazies() {
                assert_eq!(at_once.part(it), built.part(it), "seed {seed}, lazy {it}");
                assert_eq!(at_once.part_errors(it), &expanded_errors(&at_once, it)[..]);
            }
            let (mut step, mut stepped) = (shape, tree);
            for &(at, splice) in splices.iter().rev() {
                let (_, with, _) = edits.iter().find(|it| it.0 == at).unwrap();
                step = replaced(&step, at, with, &mut 0);
                let mut text = String::new();
                text_of(&step, &mut text);
                stepped = Arc::new(match splice {
                    Splice::Subtree(with) => stepped.replace_subtree(at, with, text.into()),
                    Splice::Token(with) => stepped.replace_token(at, with, text.into()),
                });
            }
            same_arrays(&at_once, &stepped, seed);
            compare(&Model::new(&edited), &SyntaxNode::new_flat_root(Arc::new(at_once)), false, seed);
        }
    }

    #[test]
    fn equal_cursors_are_the_same_element() {
        let flat = SyntaxNode::new_flat_root(Arc::new(build_flat(&random_shape(7), false)));
        let all = elements(&flat);
        let again = elements(&SyntaxNode::new_flat_root(Arc::clone(flat.flat_tree().unwrap())));
        for (i, a) in all.iter().enumerate() {
            for (j, b) in again.iter().enumerate() {
                assert_eq!(a == b, i == j);
            }
        }
    }

    /// Every element of `shape` in preorder, as a tree of its own.
    fn preorder(shape: &Shape, out: &mut Vec<Shape>) {
        out.push(shape.clone());
        if let Shape::Node(_, children, _) = shape {
            children.iter().for_each(|it| preorder(it, out));
        }
    }

    /// `shape` in full with the children `range` of its element `at` replaced by `with`,
    /// `next` the number of `shape` in it.
    fn spliced(shape: &Shape, at: usize, range: &Range<usize>, with: &[Shape], next: &mut usize) -> Shape {
        let here = *next;
        *next += 1;
        match shape {
            Shape::Node(kind, children, _) => {
                let mut children: Vec<Shape> =
                    children.iter().map(|it| spliced(it, at, range, with, next)).collect();
                if here == at {
                    children.splice(range.clone(), with.iter().cloned());
                }
                Shape::Node(*kind, children, None)
            }
            token => token.clone(),
        }
    }

    #[test]
    fn splicing_children_is_building_the_edited_tree() {
        for seed in 0..1000 {
            let mut rng = SplitMix64::new(seed ^ 0x5bd1);
            let mut shape = random_shape(seed);
            let outline = seed % 2 == 1;
            if outline {
                make_lazy(&mut shape, &mut rng);
            }
            let root = match outline {
                true => SyntaxNode::new_flat_root(build_outline(&shape, expand)),
                false => SyntaxNode::new_flat_root(Arc::new(build_flat(&shape, seed % 4 == 2))),
            };
            let all = elements(&root);
            let nodes: Vec<usize> = (0..all.len()).filter(|&it| all[it].as_node().is_some()).collect();
            let at = nodes[rng.below(nodes.len() as u64) as usize];
            let node = all[at].as_node().unwrap().clone();
            let n = node.children_with_tokens().count();
            let from = rng.below(n as u64 + 1) as usize;
            let to = from + rng.below((n - from) as u64 + 1) as usize;
            // What goes in: elements of this tree (even `node` or the root) and of another.
            let other_shape = random_shape(seed + 20_000);
            let other = SyntaxNode::new_flat_root(Arc::new(build_flat(&other_shape, false)));
            let (mut pool, mut shapes) = (all, Vec::new());
            pool.extend(elements(&other));
            preorder(&shape, &mut shapes);
            preorder(&other_shape, &mut shapes);
            let picks: Vec<usize> = (0..rng.below(4)).map(|_| rng.below(pool.len() as u64) as usize).collect();
            let with: Vec<SyntaxElement> = picks.iter().map(|&it| pool[it].clone()).collect();
            let with_shapes: Vec<Shape> = picks.iter().map(|&it| shapes[it].clone()).collect();
            let new = node.splice_children(from..to, &with);
            let edited = spliced(&shape, at, &(from..to), &with_shapes, &mut 0);
            assert!(new.parent().is_none() && new != root);
            assert_eq!(**new.flat_tree().unwrap(), build_flat(&edited, false), "seed {seed}");
            compare(&Model::new(&edited), &new, true, seed);
            // The tree spliced is as it was.
            compare(&Model::new(&shape), &root, false, seed);
        }
    }

    /// A tree from a seed whose nodes are on both sides of what keeps a hash: up to five deep,
    /// each node with up to 20 children, a quarter of them nodes, and tokens of up to 60 bytes.
    fn nested_shape(seed: u64) -> Shape {
        fn grow(rng: &mut SplitMix64, depth: u32) -> Shape {
            let mut children = Vec::new();
            for _ in 0..rng.below(20) {
                match rng.below(4) {
                    0 if depth < 5 => children.push(grow(rng, depth + 1)),
                    _ => {
                        let text = "abcdefghij".repeat(6)[..rng.below(61) as usize].to_owned();
                        children.push(Shape::Token(SyntaxKind(rng.below(5) as u16), text));
                    }
                }
            }
            Shape::Node(SyntaxKind(100 + rng.below(5) as u16), children, None)
        }
        grow(&mut SplitMix64::new(seed), 0)
    }

    /// One BLAKE3 pass over [`FlatTree::hash_subtree`]'s writes from `at`, made apart from
    /// [`FlatTree::subtree_hash`] and its kept slots, to hold them to.
    fn fresh_hash(tree: &FlatTree, at: u32) -> u128 {
        static TREE: blaze_ids::Domain = blaze_ids::Domain::new("blaze v2 hir-def body tree 1");
        let mut hasher = blaze_ids::Hasher::of(&TREE);
        tree.hash_subtree(at, &mut blaze_hash::Blake3Hasher(&mut hasher));
        let id = hasher.finish();
        u128::from_le_bytes(id.0[..16].try_into().unwrap())
    }

    /// Asks every element of `tree` for its hash, so every kept one is made.
    fn hash_all(tree: &FlatTree) {
        for at in 0..tree.len() {
            let _ = tree.subtree_hash(at);
        }
    }

    fn ancestors_in(tree: &FlatTree, at: u32) -> Vec<u32> {
        std::iter::successors(tree.parent(at), |&it| tree.parent(it)).collect()
    }

    /// Whether node `at` of `tree` keeps its hash and has made it.
    fn made(tree: &FlatTree, at: u32) -> bool {
        tree.kept.slot(tree.rank(at)).is_some_and(|it| it.get().is_some())
    }

    /// `tree` keeps the hashes of the nodes [`keeps`] says, and every element's
    /// `subtree_hash`, asked twice (the second time read from its slot when kept), is a
    /// fresh pass from it.
    fn check_hashes(tree: &FlatTree, seed: u64) {
        let kept: Vec<u32> = (0..tree.len())
            .filter(|&it| !tree.is_token(it) && !tree.is_lazy(it))
            .filter(|&it| keeps(u32::from(tree.range(it).len()), tree.end(it) - it))
            .collect();
        assert_eq!(&*tree.kept.elements, &kept[..], "seed {seed}");
        for _ in 0..2 {
            for at in 0..tree.len() {
                assert_eq!(tree.subtree_hash(at), fresh_hash(tree, at), "seed {seed}, element {at}");
            }
        }
        assert!(kept.iter().all(|&it| made(tree, it)), "seed {seed}");
    }

    #[test]
    fn a_subtree_hash_is_one_pass_of_its_writes() {
        let mut kept = 0;
        for seed in 0..150 {
            let mut shape = match seed % 3 {
                0 => random_shape(seed),
                _ => nested_shape(seed),
            };
            let tree = build_flat(&shape, seed % 2 == 1);
            check_hashes(&tree, seed);
            kept += tree.kept.elements.len();
            make_lazy(&mut shape, &mut SplitMix64::new(seed ^ 0x51ab));
            let outline = build_outline(&shape, expand);
            check_hashes(&outline, seed);
            for at in outline.lazies() {
                check_hashes(outline.part(at), seed);
            }
            // A copy carries the hashes made.
            let copy = FlatTree::clone(&outline);
            assert!(copy.kept.elements.iter().all(|&it| made(&copy, it)), "seed {seed}");
        }
        assert!(kept > 150, "nodes keep their hashes: {kept}");
        for seed in 0..3 {
            check_hashes(&build_flat(&big_shape(seed), true), seed);
        }
    }

    /// `spliced`, made by an edit, keeps what `built`, the edited tree built anew, keeps, and
    /// their hashes are the same and fresh passes.
    fn check_edited(spliced: &FlatTree, built: &FlatTree, seed: u64) {
        assert_eq!(spliced.kept.elements, built.kept.elements, "seed {seed}");
        check_hashes(spliced, seed);
        for at in 0..built.len() {
            assert_eq!(spliced.subtree_hash(at), built.subtree_hash(at), "seed {seed}, element {at}");
        }
    }

    #[test]
    fn an_edit_keeps_the_hashes_of_what_it_does_not_change() {
        for seed in 0..100 {
            let mut rng = SplitMix64::new(seed ^ 0x3c6e);
            let shape = nested_shape(seed);
            let tree = build_flat(&shape, true);
            hash_all(&tree);

            let nodes: Vec<u32> = (0..tree.len()).filter(|&it| !tree.is_token(it)).collect();
            let at = nodes[rng.below(nodes.len() as u64) as usize];
            let mut with = nested_shape(seed + 50_000);
            if let Shape::Node(kind, ..) = &mut with {
                *kind = tree.kind(at);
            }
            let with_tree = build_flat(&with, false);
            let with_made = seed % 2 == 0;
            if with_made {
                hash_all(&with_tree);
            }
            let mut edited = replaced(&shape, at, &with, &mut 0);
            let mut text = String::new();
            text_of(&edited, &mut text);
            let mut spliced = tree.replace_subtree(at, &with_tree, text.into());
            let ancestors = ancestors_in(&tree, at);
            let (lo, hi) = (at, tree.end(at));
            for &it in tree.kept.elements.iter() {
                let (new, carried) = match it {
                    it if it < lo => (it, !ancestors.contains(&it)),
                    it if it >= hi => (it - (hi - lo) + with_tree.len(), true),
                    _ => continue,
                };
                assert_eq!(made(&spliced, new), carried, "seed {seed}, element {it}");
            }
            for &it in with_tree.kept.elements.iter() {
                assert_eq!(made(&spliced, it + lo), with_made, "seed {seed}, {it} of the new");
            }
            let built = build_flat(&edited, true);
            same_arrays(&spliced, &built, seed);
            check_edited(&spliced, &built, seed);

            // Tokens replaced, one making its ancestors keep their hashes and one not again.
            for step in 0..3 {
                let tokens: Vec<u32> = (0..spliced.len()).filter(|&it| spliced.is_token(it)).collect();
                if tokens.is_empty() {
                    break;
                }
                let at = tokens[rng.below(tokens.len() as u64) as usize];
                let text = match (step, rng.below(3)) {
                    (0, _) | (_, 0) => "y".repeat(KEEP_TEXT),
                    (_, 1) => String::new(),
                    _ => "xyz".into(),
                };
                edited = replaced(&edited, at, &Shape::Token(spliced.kind(at), text.clone()), &mut 0);
                let mut source = String::new();
                text_of(&edited, &mut source);
                let ancestors = ancestors_in(&spliced, at);
                let new = spliced.replace_token(at, &text, source.into());
                for &it in spliced.kept.elements.iter() {
                    let carried = !ancestors.contains(&it);
                    assert_eq!(made(&new, it), carried, "seed {seed}, step {step}, element {it}");
                }
                check_edited(&new, &build_flat(&edited, true), seed);
                spliced = new;
            }
        }
    }

    /// A token long enough that every node holding it keeps its hash.
    const KEEP_TEXT: usize = super::KEEP as usize;

    #[test]
    fn equal_subtrees_have_equal_hashes_wherever_they_are() {
        for seed in 0..40 {
            let (inner, other) = (nested_shape(seed), nested_shape(seed + 1000));
            // `inner` under the root, and again deeper, after `other` and a token.
            let deeper = vec![other, Shape::Token(SyntaxKind(2), "x".repeat(seed as usize)), inner.clone()];
            let children = vec![
                Shape::Token(SyntaxKind(1), "ab".into()),
                inner.clone(),
                Shape::Node(SyntaxKind(101), deeper, None),
            ];
            let tree = build_flat(&Shape::Node(SyntaxKind(100), children, None), seed % 2 == 0);
            let alone = build_flat(&inner, false);
            let (first, other_at) = (2, tree.end(2) + 1);
            let second = tree.end(other_at) + 1;
            assert_eq!(tree.end(second) - second, alone.len());
            hash_all(&tree);
            let copy = tree.subtree(first);
            for it in 0..alone.len() {
                let hash = alone.subtree_hash(it);
                assert_eq!(tree.subtree_hash(first + it), hash, "seed {seed}, element {it}");
                assert_eq!(tree.subtree_hash(second + it), hash, "seed {seed}, element {it}");
                assert_eq!(copy.subtree_hash(it), hash, "seed {seed}, element {it}");
            }
            assert!(copy.kept.elements.iter().all(|&it| made(&copy, it)), "a copy keeps them");
            if tree.subtree(other_at) != copy {
                assert_ne!(tree.subtree_hash(first), tree.subtree_hash(other_at), "seed {seed}");
            }
            assert_ne!(tree.subtree_hash(0), tree.subtree_hash(first), "seed {seed}");
        }
    }

    /// `tree.subtree(at)` with the nodes `elided` replaced in turn, the last first, by empty
    /// nodes of their kinds.
    fn without_in_turn(tree: &FlatTree, at: u32, elided: &[u32]) -> FlatTree {
        let mut copy = tree.subtree(at);
        for &it in elided.iter().rev() {
            let it = it - at;
            let mut empty = FlatTreeBuilder::new();
            empty.start_node(copy.kind(it));
            empty.finish_node();
            let range = copy.range(it);
            let text = copy.text();
            let text = format!("{}{}", &text[..usize::from(range.start())], &text[usize::from(range.end())..]);
            copy = copy.replace_subtree(it, &empty.finish(), text.into());
        }
        copy
    }

    #[test]
    fn a_subtree_without_nodes_is_the_subtree_with_them_replaced() {
        for seed in 0..200 {
            let mut rng = SplitMix64::new(seed ^ 0x2f1d);
            let nested = seed % 2 == 1;
            let mut shape = match nested {
                true => nested_shape(seed),
                false => random_shape(seed),
            };
            let outline = seed % 3 != 0;
            if outline {
                make_lazy(&mut shape, &mut rng);
            }
            let tree = match outline {
                true => build_outline(&shape, expand),
                false => Arc::new(build_flat(&shape, seed % 4 == 1)),
            };
            // Some parts parsed and some not, some hashes made and some not.
            for (i, it) in tree.lazies().enumerate() {
                if i % 2 == 0 {
                    let _ = tree.part(it);
                }
            }
            if seed % 5 != 0 {
                hash_all(&tree);
            }
            let nodes: Vec<u32> = (0..tree.len()).filter(|&it| !tree.is_token(it)).collect();
            let ats: Vec<u32> = match nested {
                true => std::iter::once(0)
                    .chain((0..6).map(|_| nodes[rng.below(nodes.len() as u64) as usize]))
                    .collect(),
                false => nodes,
            };
            for at in ats {
                let under: Vec<u32> =
                    (at + 1..tree.end(at)).filter(|&it| !tree.is_token(it)).collect();
                let mut lists = vec![Vec::new()];
                for _ in 0..under.len().min(4) {
                    lists.push(vec![under[rng.below(under.len() as u64) as usize]]);
                }
                let mut list = Vec::new();
                let mut from = at + 1;
                for &it in &under {
                    if it >= from && rng.below(3) == 0 {
                        list.push(it);
                        from = tree.end(it);
                    }
                }
                lists.push(list);
                for elided in &lists {
                    check_without(&tree, at, elided, seed);
                }
            }
        }
    }

    /// `tree.subtree_without(at, elided)` is [`without_in_turn`]: the same elements and text,
    /// parts and kept hashes, and the hashes made outside the elided nodes and their ancestors
    /// carried.
    fn check_without(tree: &FlatTree, at: u32, elided: &[u32], seed: u64) {
        let context = format!("seed {seed}, {at} without {elided:?}");
        let without = tree.subtree_without(at, elided, None);
        let expected = without_in_turn(tree, at, elided);
        assert_eq!(without, expected, "{context}");
        for it in 0..expected.len() {
            assert_eq!(
                (without.parent(it), without.range(it), without.end(it), without.is_lazy(it)),
                (expected.parent(it), expected.range(it), expected.end(it), expected.is_lazy(it)),
                "{context}, element {it}"
            );
        }
        assert!(without.lazies().eq(expected.lazies()), "{context}");
        let parsed: Vec<u32> = without.lazies().filter(|&it| without.is_parsed(it)).collect();
        assert!(parsed.iter().copied().eq(expected.lazies().filter(|&it| expected.is_parsed(it))));
        for &it in &parsed {
            assert_eq!(without.part(it), expected.part(it), "{context}, lazy {it}");
            assert_eq!(without.part_errors(it), expected.part_errors(it), "{context}, lazy {it}");
            assert!(without.part(it).source().ptr_eq(without.source()));
        }
        // Where each element outside the elided nodes is in the copy.
        let moved = |it: u32| {
            let removed: u32 =
                elided.iter().filter(|&&e| tree.end(e) <= it).map(|&e| tree.end(e) - e - 1).sum();
            it - at - removed
        };
        let inside = |it: u32| elided.iter().any(|&e| e <= it && it < tree.end(e));
        let holds = |it: u32| elided.iter().any(|&e| it < e && e < tree.end(it));
        for &it in tree.kept.elements.iter().filter(|&&it| at <= it && it < tree.end(at)) {
            if !inside(it) && !holds(it) {
                assert_eq!(made(&without, moved(it)), made(tree, it), "{context}, element {it}");
            }
        }
        for &it in elided {
            assert!(!made(&without, moved(it)), "{context}: an elided node is empty");
        }
        check_edited(&without, &expected, seed);
    }

    /// The cursor's `clone_subtree_without` is `subtree_without` of its nodes' numbers.
    #[test]
    fn a_cursor_clones_a_subtree_without_nodes() {
        let tree = Arc::new(build_flat(&big_shape(1), true));
        let root = SyntaxNode::new_flat_root(Arc::clone(&tree));
        let rows = root.first_child().unwrap();
        let elided: Vec<SyntaxNode> = rows.children().step_by(2).collect();
        assert_eq!(elided.len(), 50);
        let numbers: Vec<u32> = elided.iter().map(|it| it.flat().unwrap().1).collect();
        let at = rows.flat().unwrap().1;
        let copy = rows.clone_subtree_without(elided.iter());
        assert_eq!(**copy.flat_tree().unwrap(), tree.subtree_without(at, &numbers, None));
        assert_eq!(**copy.flat_tree().unwrap(), without_in_turn(&tree, at, &numbers));
        let alone = rows.clone_subtree_without(std::iter::empty::<&SyntaxNode>());
        assert_eq!(**alone.flat_tree().unwrap(), tree.subtree(at));
    }

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
        let hashes =
            |tree: &FlatTree| tree.kept.hashes.iter().map(|it| it.value()).collect::<Vec<_>>();
        assert_eq!(hashes(&clone), hashes(&tree));
        assert!(!std::ptr::eq(clone.words.as_ptr(), tree.words.as_ptr()));
    }
}
