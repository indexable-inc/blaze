//! A token tree's storage: one block of `u32` words, a header and then columns over `u32`
//! indices, the top subtree first and every token tree after it in preorder.
//!
//! - the header: each column's offset and length in words, in the order below ([`HEADER`]).
//! - `nodes`: one [`Node`] (8 bytes) per token tree: a tag with small fields in one word, and a
//!   symbol index, a character or a subtree's length in the other. A subtree's children are the
//!   `len` nodes after it, so skipping one is one add, and any token tree is one index away.
//! - `spans`: one [`PackedSpan`] (8 bytes) per node, a leaf's span or a subtree's opening one
//!   (the top subtree's slot is unused), then one per subtree but the top for its closing span,
//!   in the subtrees' order. A packed span is a range and an index into `parts`, the (anchor,
//!   context) pairs the tree uses; a range or an index too large to pack goes to `wide`.
//! - `symbols`: the IDs ([`Symbol::id`]) of the symbols of idents and literals.
//!
//! Beside the block, the top subtree's two spans, whole, so that they can be changed in place.
//! Every reference in the block is an offset, never a pointer, and every accessor reads it as
//! `&[u32]`, so the same words can be a tree in memory or a checked view of mapped bytes.
//!
//! A leaf costs 16 bytes and a subtree 24, and a tree is one allocation whatever its shape. The
//! tables list entries in the order the tree first uses them, so the block is a function of the
//! token trees alone: two trees are equal, and hash equal, exactly when their token trees are,
//! whichever builder calls made them.

use std::{
    alloc::{Allocator, Global},
    cell::Cell,
    hash::Hash,
    simd::{Simd, cmp::SimdPartialEq},
    sync::LazyLock,
};

use blaze_hash::HashMap;
use blaze_mem::scratch::Scratch as Stack;
use intern::Symbol;
use span::{
    Edition, EditionedFileId, FileId, ROOT_ERASED_FILE_AST_ID, Span, SpanAnchor, SyntaxContext,
    TextRange, TextSize,
};

use crate::{
    DelimSpan, Delimiter, DelimiterKind, Ident, IdentIsRaw, Leaf, LitKind, Literal, Punct, Spacing,
    Subtree, SubtreeView, TokenTree, TokenTreesView, TtIter,
};

mod owned;
#[cfg(test)]
mod tests;
mod wire;

pub use owned::{TopSubtree, Tree};

pub use wire::{WireError, WirePart, aligned};
mod columns;
use columns::{CompressedSpanPart, Node, PackedSpan, WideSpan};
mod words;
pub use words::{TreeWords, ViewWords};
#[cfg(test)]
mod varint;

const _: () = assert!(size_of::<Node>() == 8);

const TAG: u32 = 0b11;
const SUBTREE: u32 = 0;
const PUNCT: u32 = 1;
const IDENT: u32 = 2;
const LITERAL: u32 = 3;
const ORDINAL_SHIFT: u32 = 4;
const MAX_ORDINAL: u32 = u32::MAX >> ORDINAL_SHIFT;
const SUFFIX_SHIFT: u32 = 14;
/// The longest suffix a literal node holds: 256 KiB, against rustc's none.
const MAX_SUFFIX: u32 = u32::MAX >> SUFFIX_SHIFT;

impl Node {
    #[inline]
    fn tag(self) -> u32 {
        self.head & TAG
    }

    #[inline]
    fn subtree(kind: DelimiterKind, ordinal: u32, len: u32) -> Node {
        assert!(ordinal <= MAX_ORDINAL, "a token tree of more than {MAX_ORDINAL} subtrees");
        Node { head: SUBTREE | (kind as u32) << 2 | ordinal << ORDINAL_SHIFT, data: len }
    }

    #[inline]
    fn ordinal(self) -> u32 {
        self.head >> ORDINAL_SHIFT
    }

    #[inline]
    fn with_ordinal(self, ordinal: u32) -> Node {
        Node::subtree(self.delimiter_kind(), ordinal, self.data)
    }

    #[inline]
    fn delimiter_kind(self) -> DelimiterKind {
        match (self.head >> 2) & 0b11 {
            0 => DelimiterKind::Parenthesis,
            1 => DelimiterKind::Brace,
            2 => DelimiterKind::Bracket,
            _ => DelimiterKind::Invisible,
        }
    }

    #[inline]
    fn punct(char: char, spacing: Spacing) -> Node {
        Node { head: PUNCT | (spacing as u32) << 2, data: char as u32 }
    }

    #[inline]
    fn spacing(self) -> Spacing {
        match (self.head >> 2) & 0b11 {
            0 => Spacing::Alone,
            1 => Spacing::Joint,
            _ => Spacing::JointHidden,
        }
    }

    #[inline]
    fn ident(is_raw: IdentIsRaw, symbol: u32) -> Node {
        Node { head: IDENT | (is_raw as u32) << 2, data: symbol }
    }

    #[inline]
    fn is_raw(self) -> IdentIsRaw {
        match (self.head >> 2) & 1 {
            0 => IdentIsRaw::No,
            _ => IdentIsRaw::Yes,
        }
    }

    #[inline]
    fn literal(kind: LitKind, suffix_len: u32, symbol: u32) -> Node {
        assert!(
            suffix_len <= MAX_SUFFIX,
            "a literal with a suffix of {suffix_len} bytes: a token tree holds at most {MAX_SUFFIX}"
        );
        let (code, hashes) = kind.code();
        let code = u32::from(code);
        Node {
            head: LITERAL | code << 2 | u32::from(hashes) << 6 | suffix_len << SUFFIX_SHIFT,
            data: symbol,
        }
    }

    #[inline]
    fn lit_kind(self) -> LitKind {
        let hashes = (self.head >> 6) as u8;
        // A node's code is one `literal` wrote, so it is at most 10; 11 to 15 read as `Err`, as
        // before.
        LitKind::from_code(((self.head >> 2) & 0xF) as u8, hashes).unwrap_or(LitKind::Err(()))
    }

    #[inline]
    fn suffix_len(self) -> u32 {
        self.head >> SUFFIX_SHIFT
    }
}

const _: () = assert!(size_of::<PackedSpan>() == 8);

const WIDE: u32 = u32::MAX;
const NARROW_LIMIT: u32 = 0xFFFF;

impl PackedSpan {
    const UNUSED: PackedSpan = PackedSpan { start: 0, len_part: 0 };
}

/// Distinct entries in the order they are first asked for, each found by a scan while there are
/// few and through a map after.
#[derive(Debug)]
pub struct IndexTable<T> {
    list: Vec<T>,
    map: HashMap<T, u32>,
}

impl<T> Default for IndexTable<T> {
    fn default() -> Self {
        // alloc: kept per thread with the builder's buffers, grown at the first parts.
        IndexTable { list: Vec::new(), map: HashMap::default() }
    }
}

const SCAN: usize = 8;

impl<T: Copy + Eq + Hash> IndexTable<T> {
    /// Forgets every entry, keeping the room.
    pub fn clear(&mut self) {
        self.list.clear();
        self.map.clear();
    }

    /// The entries, each at its index.
    #[inline]
    pub fn list(&self) -> &[T] {
        &self.list
    }

    /// The index of `value`, the next one when it is new.
    #[inline]
    pub fn index(&mut self, value: T) -> u32 {
        if self.list.len() <= SCAN {
            if let Some(at) = self.list.iter().position(|it| *it == value) {
                return at as u32;
            }
            let at = self.list.len() as u32;
            self.list.push(value);
            if self.list.len() > SCAN {
                self.map.reserve(4 * SCAN);
                self.map.extend(self.list.iter().enumerate().map(|(at, &it)| (it, at as u32)));
            }
            return at;
        }
        let next = self.list.len() as u32;
        let list = &mut self.list;
        *self.map.entry(value).or_insert_with(|| {
            list.push(value);
            next
        })
    }
}

/// How many of the latest symbols a new one is looked for among, once a tree has more than
/// that: symbols repeat close together (`self.a`, `self.b`), and a repeat further back costs 8
/// bytes where finding it would cost a hash per ident.
const SYMBOL_WINDOW: usize = 16;

/// The symbols of a tree, as their IDs, an index each: the same symbol has one while the tree
/// has at most [`SYMBOL_WINDOW`], and after that when it was among the last [`SYMBOL_WINDOW`]
/// added.
#[derive(Debug, Default)]
struct Symbols {
    list: Vec<u32>,
}

impl Symbols {
    /// A full window is one 16-lane compare, so a new symbol costs what a repeat does; a window
    /// never holds one ID twice, as an ID is added only when the window lacks it.
    #[inline]
    fn index(&mut self, symbol: Symbol) -> u32 {
        let id = symbol.id();
        let len = self.list.len();
        let found = match self.list.last_chunk::<SYMBOL_WINDOW>() {
            Some(window) => {
                let hits = Simd::from_array(*window).simd_eq(Simd::splat(id)).to_bitmask();
                (hits != 0).then(|| len - SYMBOL_WINDOW + (u64::BITS - 1 - hits.leading_zeros()) as usize)
            }
            None => self.list.iter().rposition(|&it| it == id),
        };
        if let Some(at) = found {
            return at as u32;
        }
        self.list.push(id);
        len as u32
    }
}

/// The symbol of an ID in a tree's symbols column, which a [`Symbol`] of this process gave.
#[inline]
fn symbol(id: u32) -> Symbol {
    Symbol::from_held_id(id)
}

/// What a tree's columns are built in: the columns, and the tables filled in the order spans
/// and symbols are encoded. Kept per thread with the builder's buffers, so that a build
/// allocates only its tree's block.
#[derive(Debug, Default)]
struct Encoder {
    nodes: Vec<Node>,
    spans: Vec<PackedSpan>,
    parts: IndexTable<CompressedSpanPart>,
    last_part: Option<(CompressedSpanPart, u32)>,
    wide: Vec<WideSpan>,
    symbols: Symbols,
}

impl Encoder {
    fn clear(&mut self) {
        self.nodes.clear();
        self.spans.clear();
        self.parts.clear();
        self.last_part = None;
        self.wide.clear();
        self.symbols.list.clear();
    }

    #[inline]
    fn span(&mut self, span: &Span) -> PackedSpan {
        let part = CompressedSpanPart::from_span(span);
        let index = match self.last_part {
            Some((last, index)) if last == part => index,
            _ => {
                let index = self.parts.index(part);
                self.last_part = Some((part, index));
                index
            }
        };
        let (start, len) = (u32::from(span.range.start()), u32::from(span.range.len()));
        if len < NARROW_LIMIT && index < NARROW_LIMIT {
            PackedSpan { start, len_part: len | index << 16 }
        } else {
            self.wide_span(WideSpan { start, len, part: index })
        }
    }

    #[cold]
    fn wide_span(&mut self, span: WideSpan) -> PackedSpan {
        self.wide.push(span);
        PackedSpan { start: (self.wide.len() - 1) as u32, len_part: WIDE }
    }
}

/// The words of a tree's header: the offset and the length in words of each column, the nodes,
/// spans, wide spans, parts and symbols in that order.
const HEADER: usize = 10;

/// Where each column after the nodes starts in a tree's block, in words, and where the block
/// ends: its header, held beside it so that slicing a column loads no word of the block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    spans: u32,
    wide: u32,
    parts: u32,
    symbols: u32,
    end: u32,
}

impl Layout {
    /// The layout of columns of these many rows.
    fn new(nodes: usize, spans: usize, wide: usize, parts: usize, symbols: usize) -> Layout {
        let at = |words: usize| u32::try_from(words).expect("a token tree of fewer than 2^32 words");
        let spans_at = HEADER + 2 * nodes;
        let wide_at = spans_at + 2 * spans;
        let parts_at = wide_at + 3 * wide;
        let symbols_at = parts_at + 3 * parts;
        Layout {
            spans: at(spans_at),
            wide: at(wide_at),
            parts: at(parts_at),
            symbols: at(symbols_at),
            end: at(symbols_at + symbols),
        }
    }

    fn header(self) -> [u32; HEADER] {
        let nodes = HEADER as u32;
        [
            nodes,
            self.spans - nodes,
            self.spans,
            self.wide - self.spans,
            self.wide,
            self.parts - self.wide,
            self.parts,
            self.symbols - self.parts,
            self.symbols,
            self.end - self.symbols,
        ]
    }

    /// The layout `block`'s header describes, if it describes `block`: the columns one after
    /// the other from the header on, each whole rows, the last ending where the block does.
    #[cfg(test)]
    fn of_header(block: &[u32]) -> Option<Layout> {
        let header: [u32; HEADER] = block.get(..HEADER)?.try_into().ok()?;
        let layout = Layout {
            spans: header[2],
            wide: header[4],
            parts: header[6],
            symbols: header[8],
            end: header[8].checked_add(header[9])?,
        };
        let ordered = HEADER as u32 <= layout.spans
            && layout.spans <= layout.wide
            && layout.wide <= layout.parts
            && layout.parts <= layout.symbols;
        let whole = (layout.spans - HEADER as u32) % 2 == 0
            && (layout.wide - layout.spans) % 2 == 0
            && (layout.parts - layout.wide) % 3 == 0
            && (layout.symbols - layout.parts) % 3 == 0;
        (ordered && whole && layout.header() == header && layout.end as usize == block.len())
            .then_some(layout)
    }
}

/// The block and the top spans: the layout follows from the block, and the words from both.
impl PartialEq for Tree {
    fn eq(&self, other: &Tree) -> bool {
        self.block() == other.block()
            && self.top_open == other.top_open
            && self.top_close == other.top_close
    }
}

impl Eq for Tree {}

impl Hash for Tree {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.block().hash(state);
        self.top_open.hash(state);
        self.top_close.hash(state);
    }
}

/// As their trees: whichever allocators the two blocks are in.
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

/// A tree's columns, sliced from its block once for a walk over many token trees.
#[derive(Clone, Copy)]
struct Columns<'t> {
    nodes: &'t [[u32; 2]],
    spans: &'t [[u32; 2]],
    wide: &'t [[u32; 3]],
    parts: &'t [[u32; 3]],
    symbols: &'t [u32],
}

impl Columns<'_> {
    #[inline]
    fn node(&self, at: usize) -> Node {
        Node::from_row(self.nodes[at])
    }

    #[inline]
    fn span(&self, slot: usize) -> PackedSpan {
        PackedSpan::from_row(self.spans[slot])
    }

    #[inline]
    fn wide(&self, at: u32) -> WideSpan {
        WideSpan::from_row(self.wide[at as usize])
    }

    #[inline]
    fn part(&self, at: u32) -> CompressedSpanPart {
        CompressedSpanPart::from_row(self.parts[at as usize])
    }

    #[inline]
    fn symbol(&self, at: u32) -> Symbol {
        symbol(self.symbols[at as usize])
    }

    /// The context of a packed span: its part's, with no range unpacked.
    #[inline]
    fn ctx_of(&self, span: PackedSpan) -> SyntaxContext {
        let part = match span.len_part {
            WIDE => self.wide(span.start).part,
            len_part => len_part >> 16,
        };
        self.part(part).ctx()
    }
}

/// The tree [`TokenTreesView::empty`] views none of.
static EMPTY: LazyLock<TopSubtree> = LazyLock::new(|| {
    let span = Span {
        range: TextRange::empty(TextSize::new(0)),
        anchor: SpanAnchor {
            file_id: EditionedFileId::new(FileId::from_raw(0), Edition::CURRENT),
            ast_id: ROOT_ERASED_FILE_AST_ID,
        },
        ctx: SyntaxContext::root(Edition::CURRENT),
    };
    TopSubtree::empty(DelimSpan::from_single(span))
});

#[inline]
pub(crate) fn empty_tree() -> &'static Tree {
    &EMPTY
}

impl TopSubtree {
    pub fn empty(span: DelimSpan) -> Self {
        TopSubtreeBuilder::new(Delimiter::invisible_delim_spanned(span)).build()
    }

    pub fn invisible_from_leaves<const N: usize>(delim_span: Span, leaves: [Leaf; N]) -> Self {
        let mut builder =
            TopSubtreeBuilder::with_capacity(Delimiter::invisible_spanned(delim_span), N);
        builder.extend(leaves);
        builder.build()
    }

    pub fn from_token_trees(delimiter: Delimiter, token_trees: TokenTreesView<'_>) -> Self {
        let mut builder = TopSubtreeBuilder::with_capacity(delimiter, token_trees.len());
        builder.extend_with_tt(token_trees);
        builder.build()
    }

    /// The tree of `tts`, the top subtree first and every token tree after it in preorder.
    pub fn from_serialized(tts: Vec<TokenTree>) -> Self {
        serialized(tts).build()
    }

    pub fn from_subtree(subtree: SubtreeView<'_>) -> Self {
        let token_trees = subtree.token_trees();
        let mut builder =
            TopSubtreeBuilder::with_capacity(subtree.top_subtree().delimiter, token_trees.len());
        builder.extend_with_tt(token_trees);
        builder.build()
    }
}

/// A builder holding `tts`, the top subtree first and every token tree after it in preorder.
fn serialized(tts: Vec<TokenTree>) -> TopSubtreeBuilder {
    let mut tts = tts.into_iter();
    let Some(TokenTree::Subtree(top_subtree)) = tts.next() else {
        panic!("must always have a top subtree");
    };
    let mut builder = TopSubtreeBuilder::with_capacity(top_subtree.delimiter, tts.len());
    tts.for_each(|tt| builder.push_token_tree(tt));
    builder
}

/// Trees in any allocator: a task's temporaries, which keep no parser words
/// ([`Tree::keeps_words`]).
impl<A: Allocator> TopSubtree<A> {
    pub fn empty_in(span: DelimSpan, alloc: A) -> Self {
        TopSubtreeBuilder::new(Delimiter::invisible_delim_spanned(span)).build_in(alloc)
    }

    pub fn invisible_from_leaves_in<const N: usize>(
        delim_span: Span,
        leaves: [Leaf; N],
        alloc: A,
    ) -> Self {
        let mut builder =
            TopSubtreeBuilder::with_capacity(Delimiter::invisible_spanned(delim_span), N);
        builder.extend(leaves);
        builder.build_in(alloc)
    }

    pub fn from_token_trees_in(
        delimiter: Delimiter,
        token_trees: TokenTreesView<'_>,
        alloc: A,
    ) -> Self {
        let mut builder = TopSubtreeBuilder::with_capacity(delimiter, token_trees.len());
        builder.extend_with_tt(token_trees);
        builder.build_in(alloc)
    }

    /// [`TopSubtree::from_serialized`] in `alloc`.
    pub fn from_serialized_in(tts: Vec<TokenTree>, alloc: A) -> Self {
        serialized(tts).build_in(alloc)
    }

    pub fn from_subtree_in(subtree: SubtreeView<'_>, alloc: A) -> Self {
        let token_trees = subtree.token_trees();
        let mut builder =
            TopSubtreeBuilder::with_capacity(subtree.top_subtree().delimiter, token_trees.len());
        builder.extend_with_tt(token_trees);
        builder.build_in(alloc)
    }

    pub fn set_top_subtree_delimiter_kind(&mut self, kind: DelimiterKind) {
        let top = Node::subtree(kind, 0, self.node(0).data);
        self.block_mut()[HEADER..HEADER + 2].copy_from_slice(&top.row());
        self.forget_words();
    }

    /// The top delimiters' contexts are their words' editions, so the words kept are dropped.
    pub fn set_top_subtree_delimiter_span(&mut self, span: DelimSpan) {
        self.set_top_spans(span.open, span.close);
    }
}

impl<A: Allocator + Clone> TopSubtree<A> {
    /// [`TopSubtree::from_serialized`] like `like`: in its allocator, keeping words when it does.
    pub(crate) fn from_serialized_like(tts: Vec<TokenTree>, like: &TopSubtree<A>) -> Self {
        serialized(tts).build_like(like)
    }
}

impl TopSubtree {
    /// Puts `leaf` in place of the leaf at `idx` (the top subtree being 0). Rebuilds the tree,
    /// in one pass with no buffer but its own: avoid it in a loop.
    pub fn set_token(&mut self, idx: usize, leaf: Leaf) {
        assert!(idx > 0 && !self.is_subtree(idx as u32), "cannot change a subtree to a leaf");
        let mut builder =
            TopSubtreeBuilder::with_capacity(self.top_subtree().delimiter, self.node_count() - 1);
        let mut leaf = Some(leaf);
        for (at, tt) in (1..).zip(self.token_trees().iter_flat_tokens()) {
            match at == idx {
                true => builder.push(leaf.take().expect("one token tree is at `idx`")),
                false => builder.push_token_tree(tt),
            }
        }
        *self = builder.build();
    }

    /// Changes the AST id of every span in place, and rebuilds the tree only when that made two
    /// of its parts one.
    pub fn change_every_ast_id(&mut self, mut callback: impl FnMut(&mut span::ErasedFileAstId)) {
        let parts = self.layout.parts as usize..self.layout.symbols as usize;
        for row in self.block_mut()[parts].as_chunks_mut::<3>().0 {
            *row = CompressedSpanPart::from_row(*row).with_ast_id(&mut callback).row();
        }
        let (open, close) = self.top_spans_mut();
        callback(&mut open.anchor.ast_id);
        callback(&mut close.anchor.ast_id);
        // Two parts made one would leave the table with a part twice, which a tree built from
        // the same token trees doesn't have.
        if has_duplicates(self.parts()) {
            *self = TopSubtree::from_token_trees(self.top_subtree().delimiter, self.token_trees());
        }
    }
}

/// The tree of these columns and top spans in `alloc`: one allocation, each column one copy.
#[allow(clippy::too_many_arguments)]
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
    block.extend_from_slice(&layout.header());
    block.extend_from_slice(columns::words(nodes));
    block.extend_from_slice(columns::words(spans));
    block.extend_from_slice(columns::words(wide));
    block.extend_from_slice(columns::words(parts));
    block.extend_from_slice(symbols);
    debug_assert_eq!(block.len(), layout.end as usize, "the columns are the layout's");
    TopSubtree::from_block(block.into_boxed_slice(), layout, top_open, top_close, keeps_words)
}

impl Tree {
    pub fn view(&self) -> SubtreeView<'_> {
        SubtreeView(TokenTreesView { tree: self, start: 0, end: self.node_count() as u32 })
    }

    pub fn iter(&self) -> TtIter<'_> {
        self.view().iter()
    }

    pub fn top_subtree(&self) -> Subtree {
        self.subtree_at(0)
    }

    pub fn token_trees(&self) -> TokenTreesView<'_> {
        self.view().token_trees()
    }

    pub fn as_token_trees(&self) -> TokenTreesView<'_> {
        self.view().as_token_trees()
    }

    /// The number of token trees, the top subtree's included.
    #[inline]
    fn node_count(&self) -> usize {
        (self.layout.spans as usize - HEADER) / 2
    }

    /// The number of packed spans: one a node, then one a subtree but the top.
    #[inline]
    fn span_count(&self) -> usize {
        (self.layout.wide - self.layout.spans) as usize / 2
    }

    #[inline]
    fn nodes(&self) -> &[[u32; 2]] {
        self.block()[HEADER..self.layout.spans as usize].as_chunks::<2>().0
    }

    #[inline]
    fn spans(&self) -> &[[u32; 2]] {
        self.block()[self.layout.spans as usize..self.layout.wide as usize].as_chunks::<2>().0
    }

    #[inline]
    fn wide(&self) -> &[[u32; 3]] {
        self.block()[self.layout.wide as usize..self.layout.parts as usize].as_chunks::<3>().0
    }

    #[inline]
    fn parts(&self) -> &[[u32; 3]] {
        self.block()[self.layout.parts as usize..self.layout.symbols as usize].as_chunks::<3>().0
    }

    #[inline]
    fn symbol_ids(&self) -> &[u32] {
        &self.block()[self.layout.symbols as usize..]
    }

    /// Every column, sliced once.
    #[inline]
    fn columns(&self) -> Columns<'_> {
        Columns {
            nodes: self.nodes(),
            spans: self.spans(),
            wide: self.wide(),
            parts: self.parts(),
            symbols: self.symbol_ids(),
        }
    }

    #[inline]
    fn node(&self, at: u32) -> Node {
        Node::from_row(self.nodes()[at as usize])
    }

    #[inline]
    fn packed_span(&self, slot: usize) -> PackedSpan {
        PackedSpan::from_row(self.spans()[slot])
    }

    #[inline]
    fn part(&self, at: u32) -> CompressedSpanPart {
        CompressedSpanPart::from_row(self.parts()[at as usize])
    }

    #[inline]
    fn symbol(&self, at: u32) -> Symbol {
        symbol(self.symbol_ids()[at as usize])
    }

    #[inline]
    fn unpack(&self, span: PackedSpan) -> Span {
        if span.len_part == WIDE {
            return self.unpack_wide(span.start);
        }
        let len = TextSize::new(span.len_part & NARROW_LIMIT);
        self.part(span.len_part >> 16).recombine(TextRange::at(TextSize::new(span.start), len))
    }

    #[cold]
    fn unpack_wide(&self, at: u32) -> Span {
        let WideSpan { start, len, part } = WideSpan::from_row(self.wide()[at as usize]);
        self.part(part).recombine(TextRange::at(TextSize::new(start), TextSize::new(len)))
    }

    #[inline]
    pub(crate) fn is_subtree(&self, at: u32) -> bool {
        self.node(at).tag() == SUBTREE
    }

    /// The length of the subtree at `at`, which is one.
    #[inline]
    pub(crate) fn subtree_len(&self, at: u32) -> u32 {
        self.node(at).data
    }

    /// The delimiter kind of the subtree at `at`, which is one.
    #[inline]
    pub(crate) fn delimiter_kind_at(&self, at: u32) -> DelimiterKind {
        self.node(at).delimiter_kind()
    }

    /// The subtree at `at`, which is one.
    #[inline]
    pub(crate) fn subtree_at(&self, at: u32) -> Subtree {
        let node = self.node(at);
        debug_assert_eq!(node.tag(), SUBTREE);
        let (open, close) = match at {
            0 => (self.top_open, self.top_close),
            _ => (self.unpack(self.packed_span(at as usize)), self.close_span(node)),
        };
        Subtree { delimiter: Delimiter { open, close, kind: node.delimiter_kind() }, len: node.data }
    }

    #[inline]
    fn close_span(&self, node: Node) -> Span {
        self.unpack(self.packed_span(self.node_count() + node.ordinal() as usize - 1))
    }

    /// The leaf at `at`, which is one.
    #[inline]
    pub(crate) fn leaf_at(&self, at: u32) -> Leaf {
        let node = self.node(at);
        let span = self.unpack(self.packed_span(at as usize));
        match node.tag() {
            PUNCT => Leaf::Punct(Punct {
                char: char::from_u32(node.data).expect("a punct's node holds a char"),
                spacing: node.spacing(),
                span,
            }),
            IDENT => Leaf::Ident(Ident { sym: self.symbol(node.data), span, is_raw: node.is_raw() }),
            LITERAL => Leaf::Literal(Literal {
                text_and_suffix: self.symbol(node.data),
                span,
                kind: node.lit_kind(),
                suffix_len: node.suffix_len(),
            }),
            _ => unreachable!("token tree {at} is a subtree"),
        }
    }

    #[inline]
    pub(crate) fn token_tree_at(&self, at: u32) -> TokenTree {
        match self.is_subtree(at) {
            true => TokenTree::Subtree(self.subtree_at(at)),
            false => TokenTree::Leaf(self.leaf_at(at)),
        }
    }

    /// A leaf's span, or a subtree's opening one.
    #[inline]
    pub(crate) fn first_span_at(&self, at: u32) -> Span {
        match at {
            0 => self.top_open,
            _ => self.unpack(self.packed_span(at as usize)),
        }
    }

    /// A leaf's span, or a subtree's closing one.
    #[inline]
    pub(crate) fn last_span_at(&self, at: u32) -> Span {
        let node = self.node(at);
        match (node.tag(), at) {
            (SUBTREE, 0) => self.top_close,
            (SUBTREE, _) => self.close_span(node),
            _ => self.unpack(self.packed_span(at as usize)),
        }
    }
}

thread_local! {
    /// The subtrees a [`Tree::for_each_token`] walk is in, each as the index of the node
    /// after it and its own, kept for the next walk. `Scratch` has no `Default` for a `T`
    /// without one, so it is taken with `replace`, not `take`.
    // alloc: one buffer per thread, grown to the deepest nesting walked.
    static OPEN: Cell<Stack<(u32, u32)>> = const { Cell::new(Stack::new()) };
}

impl Tree {
    /// Calls `f` with each token of the token trees `start..end` in order, a subtree being its
    /// opening delimiter, its token trees and its closing delimiter, with the context of each.
    /// Reads the columns: no span's range and no leaf is unpacked, and nothing is allocated
    /// but the stack of open subtrees, kept per thread.
    #[inline]
    pub(crate) fn for_each_token(
        &self,
        start: u32,
        end: u32,
        mut f: impl FnMut(crate::FlatToken, SyntaxContext),
    ) {
        self.walk_tokens(start, end, |_, token, ctx| f(token, ctx))
    }

    /// The number of token slots: one a node, one a subtree's closing delimiter.
    #[inline]
    pub fn token_slots(&self) -> usize {
        self.span_count() + 1
    }

    /// The slot of the closing delimiter of the subtree at `at`.
    #[inline]
    fn close_slot(&self, at: u32) -> u32 {
        match at {
            0 => self.span_count() as u32,
            _ => (self.node_count() + self.node(at).ordinal() as usize - 1) as u32,
        }
    }

    /// [`Tree::for_each_token`] over the whole tree, with each token's slot: a leaf's or an
    /// opening delimiter's node, or its subtree's [`Tree::token_slots`] entry for a
    /// closing one. What [`TreeWords`] are made from.
    pub fn for_each_slotted_token(&self, f: impl FnMut(u32, crate::FlatToken, SyntaxContext)) {
        self.walk_tokens(0, self.node_count() as u32, f)
    }

    fn walk_tokens(
        &self,
        start: u32,
        end: u32,
        mut f: impl FnMut(u32, crate::FlatToken, SyntaxContext),
    ) {
        use crate::FlatToken;
        let columns = self.columns();
        let node_count = columns.nodes.len();
        // The slot, token and context of the closing delimiter of the subtree at `at`.
        let close = |at: u32| {
            let node = columns.node(at as usize);
            let (slot, ctx) = match at {
                0 => (columns.spans.len(), self.top_close.ctx),
                _ => {
                    let slot = node_count + node.ordinal() as usize - 1;
                    (slot, columns.ctx_of(columns.span(slot)))
                }
            };
            (slot as u32, FlatToken::Close(node.delimiter_kind()), ctx)
        };
        let mut stack = OPEN.replace(Stack::new());
        let open = stack.take();
        let mut from = start as usize;
        if start == 0 && end > 0 {
            let top = columns.node(0);
            open.push((1 + top.data, 0));
            f(0, FlatToken::Open(top.delimiter_kind()), self.top_open.ctx);
            from = 1;
        }
        let end = end as usize;
        let rows = columns.nodes[from..end].iter().zip(&columns.spans[from..end]);
        for (at, (&node, &span)) in (from as u32..).zip(rows) {
            let (node, span) = (Node::from_row(node), PackedSpan::from_row(span));
            while let Some(&(after, subtree)) = open.last()
                && after == at
            {
                open.pop();
                let (slot, token, ctx) = close(subtree);
                f(slot, token, ctx);
            }
            let token = match node.tag() {
                SUBTREE => {
                    open.push((at + 1 + node.data, at));
                    FlatToken::Open(node.delimiter_kind())
                }
                PUNCT => FlatToken::Punct {
                    char: char::from_u32(node.data).expect("a punct's node holds a char"),
                    spacing: node.spacing(),
                },
                IDENT => FlatToken::Ident { sym: columns.symbol(node.data), is_raw: node.is_raw() },
                _ => FlatToken::Literal {
                    sym: columns.symbol(node.data),
                    kind: node.lit_kind(),
                    suffix_len: node.suffix_len(),
                },
            };
            f(at, token, columns.ctx_of(span));
        }
        while let Some((_, subtree)) = open.pop() {
            let (slot, token, ctx) = close(subtree);
            f(slot, token, ctx);
        }
        OPEN.set(stack);
    }
}

thread_local! {
    /// [`has_duplicates`]' set, kept for the next column of more than [`SCAN`] parts.
    // alloc: one set per thread, grown to the most parts checked, up to `KEEP`.
    static SEEN: Cell<Option<blaze_hash::HashSet<[u32; 3]>>> = const { Cell::new(None) };
}

/// Whether the parts column holds a row twice.
fn has_duplicates(parts: &[[u32; 3]]) -> bool {
    if parts.len() <= SCAN {
        return parts.iter().enumerate().any(|(at, part)| parts[..at].contains(part));
    }
    let mut seen = SEEN.take().unwrap_or_default();
    seen.clear();
    seen.reserve(parts.len());
    let twice = !parts.iter().all(|part| seen.insert(*part));
    if seen.capacity() <= KEEP {
        SEEN.set(Some(seen));
    }
    twice
}

/// A builder's buffers and its encoder's, kept per thread between builds, so that building
/// allocates only the tree's block.
#[derive(Default)]
struct Scratch {
    nodes: Vec<Node>,
    spans: Vec<Span>,
    closes: Vec<Span>,
    symbols: Vec<Symbol>,
    unclosed: Vec<u32>,
    encoder: Encoder,
}

/// The most token trees whose buffers are kept for the next build.
const KEEP: usize = 1 << 16;

thread_local! {
    static SCRATCH: Cell<Option<Scratch>> = const { Cell::new(None) };
}

impl Scratch {
    fn take() -> Scratch {
        SCRATCH.with(Cell::take).unwrap_or_default()
    }

    fn give_back(mut self) {
        if self.nodes.capacity() > KEEP || self.closes.capacity() > KEEP {
            return;
        }
        self.nodes.clear();
        self.spans.clear();
        self.closes.clear();
        self.symbols.clear();
        self.unclosed.clear();
        self.encoder.clear();
        SCRATCH.with(|it| it.set(Some(self)));
    }
}

/// Records token trees as they come; [`Self::build`] packs them into a [`TopSubtree`] in one pass.
#[derive(Debug)]
pub struct TopSubtreeBuilder {
    /// The token trees, the top subtree first. An ident's or literal's `data` is its symbol's
    /// index in `symbols`; a subtree's ordinal is its closing span's index in `closes`.
    nodes: Vec<Node>,
    /// Per node, its span or (for a subtree) its opening span.
    spans: Vec<Span>,
    closes: Vec<Span>,
    symbols: Vec<Symbol>,
    unclosed_subtree_indices: Vec<u32>,
    last_closed_subtree: Option<u32>,
    /// Empty until [`Self::build`] packs the token trees into it.
    encoder: Encoder,
}

impl TopSubtreeBuilder {
    pub fn new(top_delimiter: Delimiter) -> Self {
        Self::with_capacity(top_delimiter, 0)
    }

    /// A builder with room for `len` token trees besides the top subtree.
    pub fn with_capacity(top_delimiter: Delimiter, len: usize) -> Self {
        let Scratch { mut nodes, mut spans, mut closes, symbols, unclosed, encoder } =
            Scratch::take();
        nodes.reserve(len + 1);
        spans.reserve(len + 1);
        nodes.push(Node::subtree(top_delimiter.kind, 0, 0));
        spans.push(top_delimiter.open);
        closes.push(top_delimiter.close);
        Self {
            nodes,
            spans,
            closes,
            symbols,
            unclosed_subtree_indices: unclosed,
            last_closed_subtree: None,
            encoder,
        }
    }

    /// Not to be exposed, this assumes the subtree's children will be filled in immediately.
    fn push_subtree(&mut self, subtree: Subtree) {
        let ordinal = self.closes.len() as u32;
        self.nodes.push(Node::subtree(subtree.delimiter.kind, ordinal, subtree.len));
        self.spans.push(subtree.delimiter.open);
        self.closes.push(subtree.delimiter.close);
    }

    pub fn open(&mut self, delimiter_kind: DelimiterKind, open_span: Span) {
        let subtree_idx = self.nodes.len() as u32;
        self.push_subtree(Subtree {
            delimiter: Delimiter { open: open_span, close: open_span, kind: delimiter_kind },
            len: 0, // Will be overwritten on close.
        });
        self.unclosed_subtree_indices.push(subtree_idx);
    }

    pub fn close(&mut self, close_span: Span) {
        let last_unclosed_index = self
            .unclosed_subtree_indices
            .pop()
            .expect("attempt to close a `tt::Subtree` when none is open");
        let len = self.nodes.len() as u32 - last_unclosed_index - 1;
        let node = &mut self.nodes[last_unclosed_index as usize];
        node.data = len;
        self.closes[node.ordinal() as usize] = close_span;
        self.last_closed_subtree = Some(last_unclosed_index);
    }

    /// You cannot call this consecutively, it will only work once after close.
    pub fn remove_last_subtree_if_invisible(&mut self) {
        let Some(last_subtree_idx) = self.last_closed_subtree.take() else { return };
        if self.nodes[last_subtree_idx as usize].delimiter_kind() == DelimiterKind::Invisible {
            self.nodes.remove(last_subtree_idx as usize);
            self.spans.remove(last_subtree_idx as usize);
        }
    }

    pub fn push(&mut self, leaf: Leaf) {
        let (node, span) = match leaf {
            Leaf::Punct(Punct { char, spacing, span }) => (Node::punct(char, spacing), span),
            Leaf::Ident(Ident { sym, span, is_raw }) => {
                self.symbols.push(sym);
                (Node::ident(is_raw, (self.symbols.len() - 1) as u32), span)
            }
            Leaf::Literal(Literal { text_and_suffix, span, kind, suffix_len }) => {
                self.symbols.push(text_and_suffix);
                (Node::literal(kind, suffix_len, (self.symbols.len() - 1) as u32), span)
            }
        };
        self.nodes.push(node);
        self.spans.push(span);
    }

    fn push_token_tree(&mut self, tt: TokenTree) {
        match tt {
            TokenTree::Leaf(leaf) => self.push(leaf),
            TokenTree::Subtree(subtree) => self.push_subtree(subtree),
        }
    }

    pub fn extend(&mut self, leaves: impl IntoIterator<Item = Leaf>) {
        leaves.into_iter().for_each(|leaf| self.push(leaf));
    }

    pub fn extend_with_tt(&mut self, tt: TokenTreesView<'_>) {
        self.nodes.reserve(tt.len());
        self.spans.reserve(tt.len());
        tt.iter_flat_tokens().for_each(|tt| self.push_token_tree(tt));
    }

    /// Like [`Self::extend_with_tt()`], but makes sure the new tokens will never be
    /// joint with whatever comes after them.
    pub fn extend_with_tt_alone(&mut self, tt: TokenTreesView<'_>) {
        self.extend_with_tt(tt);
        if !tt.is_empty()
            && let Some(last) = self.nodes.last_mut()
            && last.tag() == PUNCT
        {
            last.head = PUNCT | (Spacing::Alone as u32) << 2;
        }
    }

    pub fn expected_delimiters(&self) -> impl Iterator<Item = DelimiterKind> {
        self.unclosed_subtree_indices
            .iter()
            .rev()
            .map(|&subtree_idx| self.nodes[subtree_idx as usize].delimiter_kind())
    }

    /// Builds, and remove the top subtree if it has only one subtree child.
    pub fn build_skip_top_subtree(self) -> TopSubtree {
        self.build_skip_top_subtree_keeping(true, Global)
    }

    /// [`Self::build_skip_top_subtree`] in `alloc`: a task's temporary, which keeps no parser
    /// words.
    pub fn build_skip_top_subtree_in<A: Allocator>(self, alloc: A) -> TopSubtree<A> {
        self.build_skip_top_subtree_keeping(false, alloc)
    }

    fn build_skip_top_subtree_keeping<A: Allocator>(
        self,
        keeps_words: bool,
        alloc: A,
    ) -> TopSubtree<A> {
        assert!(
            self.unclosed_subtree_indices.is_empty(),
            "attempt to build an unbalanced `TopSubtreeBuilder`"
        );
        let tt_len = self.nodes.len();
        if let Some(&first) = self.nodes.get(1)
            && first.tag() == SUBTREE
            && first.data as usize == tt_len - 2
        {
            let top = Delimiter {
                open: self.spans[1],
                close: self.closes[first.ordinal() as usize],
                kind: first.delimiter_kind(),
            };
            self.finish(2, top, keeps_words, alloc)
        } else {
            self.build_keeping(keeps_words, alloc)
        }
    }

    pub fn build(self) -> TopSubtree {
        self.build_keeping(true, Global)
    }

    /// [`Self::build`] in `alloc`: a task's temporary, which keeps no parser words.
    pub fn build_in<A: Allocator>(self, alloc: A) -> TopSubtree<A> {
        self.build_keeping(false, alloc)
    }

    /// [`Self::build`] like `like`: in its allocator, keeping words when it does.
    pub(crate) fn build_like<A: Allocator + Clone>(self, like: &TopSubtree<A>) -> TopSubtree<A> {
        // clone: the allocator handle, `Global` or a `&Frame`, not the tree.
        self.build_keeping(like.keeps_words(), like.allocator().clone())
    }

    fn build_keeping<A: Allocator>(self, keeps_words: bool, alloc: A) -> TopSubtree<A> {
        assert!(
            self.unclosed_subtree_indices.is_empty(),
            "attempt to build an unbalanced `TopSubtreeBuilder`"
        );
        let top = Delimiter {
            open: self.spans[0],
            close: self.closes[0],
            kind: self.nodes[0].delimiter_kind(),
        };
        self.finish(1, top, keeps_words, alloc)
    }

    /// The tree of `top` holding the token trees from `first` on, encoded in one pass into the
    /// encoder's columns, then copied into the tree's block in `alloc`, its one allocation.
    fn finish<A: Allocator>(
        self,
        first: usize,
        top: Delimiter,
        keeps_words: bool,
        alloc: A,
    ) -> TopSubtree<A> {
        let TopSubtreeBuilder {
            nodes,
            spans,
            closes,
            symbols,
            unclosed_subtree_indices,
            mut encoder,
            ..
        } = self;
        let len = nodes.len() - first + 1;
        let subtrees = nodes[first..].iter().filter(|it| it.tag() == SUBTREE).count();
        encoder.clear();
        encoder.nodes.reserve(len);
        encoder.spans.resize(len + subtrees, PackedSpan::UNUSED);
        encoder.symbols.list.reserve(symbols.len().min(len));
        encoder.nodes.push(Node::subtree(top.kind, 0, (len - 1) as u32));
        let mut ordinal = 0;
        for (at, (&node, span)) in nodes[first..].iter().zip(&spans[first..]).enumerate() {
            let packed = encoder.span(span);
            encoder.spans[at + 1] = packed;
            let node = match node.tag() {
                SUBTREE => {
                    ordinal += 1;
                    let close = encoder.span(&closes[node.ordinal() as usize]);
                    encoder.spans[len + ordinal - 1] = close;
                    node.with_ordinal(ordinal as u32)
                }
                PUNCT => node,
                _ => Node {
                    head: node.head,
                    data: encoder.symbols.index(symbols[node.data as usize]),
                },
            };
            encoder.nodes.push(node);
        }
        let tree = from_columns_in(
            &encoder.nodes,
            &encoder.spans,
            &encoder.wide,
            &encoder.parts.list,
            &encoder.symbols.list,
            top.open,
            top.close,
            keeps_words,
            alloc,
        );
        Scratch { nodes, spans, closes, symbols, unclosed: unclosed_subtree_indices, encoder }
            .give_back();
        tree
    }

    pub fn restore_point(&mut self) -> SubtreeBuilderRestorePoint {
        // We reset the `last_closed_subtree`, since restoring from a restore point doesn't play well with removing the last subtree.
        self.last_closed_subtree = None;
        SubtreeBuilderRestorePoint {
            unclosed_subtree_indices_len: self.unclosed_subtree_indices.len(),
            token_trees_len: self.nodes.len(),
        }
    }

    /// Drops the token trees pushed since `restore_point`. Their symbols and closing spans stay
    /// in the builder's buffers, unused: no node refers to them, so the tree doesn't hold them.
    pub fn restore(&mut self, restore_point: SubtreeBuilderRestorePoint) {
        if restore_point.token_trees_len >= self.nodes.len() {
            // This means we restored twice, potentially with an earlier restore point first.
            return;
        }
        self.nodes.truncate(restore_point.token_trees_len);
        self.spans.truncate(restore_point.token_trees_len);
        self.unclosed_subtree_indices.truncate(restore_point.unclosed_subtree_indices_len);
        self.last_closed_subtree = None;
    }
}

#[derive(Clone, Copy)]
pub struct SubtreeBuilderRestorePoint {
    unclosed_subtree_indices_len: usize,
    token_trees_len: usize,
}
