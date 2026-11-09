//! A token tree's columns as bytes, for the proc-macro wire: the columns themselves, written as
//! they are and read back checked, with no token tree built on either side.
//!
//! The bytes are little-endian `u32` words: a header (`MAGIC`, then the lengths of `nodes`,
//! `wide`, `parts` and `symbols` and the symbols' bytes), the nodes (two words each), the packed
//! spans (two each), the wide spans (three), the parts (three: file, AST id and context, as
//! [`WirePart`]), each symbol's end offset, the top subtree's two spans (five words each: start,
//! length and a [`WirePart`]), then the symbols' UTF-8, padded to a word.
//!
//! The nodes, spans, wide spans and parts are the words of the tree's block as they lie (a
//! part's words are a [`WirePart`]'s), so writing them is one copy; the symbols go as text.
//!
//! Reading checks everything a tree's methods rely on (tags, lengths nesting, ordinals, indices,
//! chars, UTF-8, ranges) and that the columns are the ones the builder makes for those token
//! trees, so that a tree read compares and hashes as the same tree built. Then it writes the
//! tree's block, its one allocation, no larger than the bytes it is read from, in the reader's
//! allocator ([`TopSubtree::read_wire_in`]); the checks' own buffers are kept per thread.
//!
//! What reads the bytes is `proc_macro_api::kept::read`: a kept expansion from the store, which
//! may have come from the store's remote (a macro's own output is built as a tree, never read
//! from bytes). Whoever can write there can already swap in any valid tree, so the checks and
//! [`WireLimits`] are bounds on damage and corruption, not a security boundary; they still hold
//! for bytes from anywhere, because the process's symbol table never shrinks. The header's
//! counts are held to the limits before any column is read (the wire's length, which needs the
//! subtree count, right after the nodes column), and every check, the canonical ones too, runs
//! on the symbols' text as the wire carries it. A symbol is interned only once the whole tree is
//! accepted, as the read's last step, which can't fail: bytes refused for any reason leave
//! nothing in the table and no block allocated, and the buffers the checks keep per thread go
//! back to a bounded size after each read. Bytes accepted add to the table at most
//! [`WireLimits::symbol_bytes`] of text and, for each of at most [`WireLimits::symbols`] new
//! symbols, the table's record and slots ([`WireLimits::DEFAULT`] says how much).

use std::{
    alloc::{Allocator, Global},
    cell::Cell,
    thread::LocalKey,
};

use blaze_format::cursor::{Put as _, Reader};
use blaze_mem::scratch::Scratch as Stack;
use intern::Symbol;
use span::{SpanAnchor, SyntaxContext, TextRange, TextSize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use super::{
    Columns, CompressedSpanPart, HEADER, IDENT, KEEP, LITERAL, Layout, NARROW_LIMIT, Node, PUNCT,
    PackedSpan, SUBTREE, SYMBOL_WINDOW, TopSubtree, Tree, WIDE, WideSpan, has_duplicates, symbol,
};

/// The first word, naming the layout; a change to it changes this.
const MAGIC: u32 = u32::from_le_bytes(*b"tt\x01\x00");
const HEADER_WORDS: usize = 6;

const _: () = assert!(
    cfg!(target_endian = "little"),
    "the wire carries the columns as they are in memory, its little-endian words on a \
     little-endian host only"
);

/// A span's anchor and context as the wire carries them: raw ids, which the side reading them
/// turns back into its own (see [`TopSubtree::read_wire`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WirePart {
    pub file_id: u32,
    pub ast_id: u32,
    pub ctx: u32,
}

impl WirePart {
    fn words(self) -> [u32; 3] {
        [self.file_id, self.ast_id, self.ctx]
    }

    fn of(part: CompressedSpanPart) -> WirePart {
        let [file_id, ast_id, ctx] = part.row();
        WirePart { file_id, ast_id, ctx }
    }
}

/// Why bytes are not a token tree.
#[derive(Debug, PartialEq, Eq)]
pub enum WireError {
    /// The bytes end at `at` where `needed` more were to come.
    Truncated { at: usize, needed: usize },
    /// The first word is not [`MAGIC`].
    Magic(u32),
    /// A column breaks a rule of the layout: what, at which index of its column.
    Malformed { what: &'static str, at: usize },
    /// The side reading refused a part.
    Part { at: usize, part: WirePart, why: String },
    /// Well formed, but not the columns the builder makes for these token trees.
    NotCanonical { what: &'static str, at: usize },
    /// The header asks for more than the reader's [`WireLimits`] allow: what is counted (its
    /// unit), the size the header gives and the limit.
    OverLimit { what: &'static str, size: usize, limit: usize },
}

/// The most one token tree's wire may hold. [`TopSubtree::read_wire`] holds the header's counts
/// to them before it reads a column, and the wire's length to [`WireLimits::bytes`] once the
/// nodes column gives the subtree count, so they bound what a read costs and what it can add to
/// the process's symbol table, which never shrinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireLimits {
    /// The most bytes the tree's wire takes, header to padding. Checked after the nodes column
    /// is read, so it bounds the bytes a read accepts, not what it reads first: every column is
    /// read in place from bytes the caller already holds.
    pub bytes: usize,
    /// The most nodes: the top subtree and every token tree in it.
    pub nodes: usize,
    /// The most symbols: each new one costs the table a record and slots past its text.
    pub symbols: usize,
    /// The most bytes of symbol text.
    pub symbol_bytes: usize,
}

impl WireLimits {
    /// What a kept expansion is read under: 2 GiB of wire, 64 Mi token trees, 16 Mi symbols and
    /// 1 GiB of symbol text, each far past what the largest expansion seen writes (a literal of
    /// an embedded file is one symbol), and each a bound damaged bytes can't pass. One accepted
    /// read then adds to the symbol table at most 1 GiB of text and, per new symbol, a 16-byte
    /// record header, the text's padding to the record's alignment and an index and an ID slot:
    /// under 64 bytes each, so about 2 GiB in all.
    pub const DEFAULT: WireLimits =
        WireLimits { bytes: 2 << 30, nodes: 64 << 20, symbols: 16 << 20, symbol_bytes: 1 << 30 };

    fn check(what: &'static str, size: usize, limit: usize) -> Result<(), WireError> {
        if size > limit {
            return Err(WireError::OverLimit { what, size, limit });
        }
        Ok(())
    }
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Truncated { at, needed } => {
                write!(f, "token tree bytes end at {at}, {needed} more expected")
            }
            WireError::Magic(word) => write!(f, "token tree bytes start with {word:#x}"),
            WireError::Malformed { what, at } => write!(f, "token tree bytes: {what} at {at}"),
            WireError::Part { at, part, why } => {
                write!(f, "token tree bytes: span part {at} ({part:?}) refused: {why}")
            }
            WireError::NotCanonical { what, at } => {
                write!(f, "token tree bytes: {what} at {at} is not what the builder makes")
            }
            WireError::OverLimit { what, size, limit } => {
                write!(f, "token tree bytes: {size} {what}, over the reader's limit of {limit}")
            }
        }
    }
}

impl std::error::Error for WireError {}

fn malformed<T>(what: &'static str, at: usize) -> Result<T, WireError> {
    Err(WireError::Malformed { what, at })
}

impl Tree {
    /// Appends the tree's columns to `out`, as [`TopSubtree::read_wire`] reads them: at most one
    /// allocation, the nodes, spans, wide spans and parts one copy of the block's words, and the
    /// symbols and top spans written as rows.
    pub fn write_wire(&self, out: &mut Vec<u8>) {
        let len = |it: usize| u32::try_from(it).expect("a token tree column fits in u32");
        let columns = self.columns();
        let symbol_bytes: usize = columns.symbols.iter().map(|&it| symbol(it).as_str().len()).sum();
        let rows = &self.block()[HEADER..self.layout.symbols as usize];
        let words = HEADER_WORDS + rows.len() + columns.symbols.len() + 10;
        let base = out.len();
        let end_of_tree = base + 4 * words + symbol_bytes.next_multiple_of(4);
        out.reserve(end_of_tree - base);
        put_rows(
            out,
            [[
                MAGIC,
                len(columns.nodes.len()),
                len(columns.wide.len()),
                len(columns.parts.len()),
                len(columns.symbols.len()),
                len(symbol_bytes),
            ]],
        );
        out.extend_from_slice(rows.as_bytes());
        let mut end = 0;
        put_rows(
            out,
            columns.symbols.iter().map(|&it| {
                end += symbol(it).as_str().len();
                [end as u32]
            }),
        );
        put_rows(
            out,
            [self.top_open, self.top_close].iter().map(|span| {
                let [file_id, ast_id, ctx] =
                    WirePart::of(CompressedSpanPart::from_span(span)).words();
                [span.range.start().into(), span.range.len().into(), file_id, ast_id, ctx]
            }),
        );
        for &id in columns.symbols {
            out.extend_from_slice(symbol(id).as_str().as_bytes());
        }
        debug_assert_eq!(
            out.len(),
            base + 4 * words + symbol_bytes,
            "the rows are the words counted"
        );
        out.resize(end_of_tree, 0);
    }
}

/// `bytes`, or a copy of them in `scratch` starting at a 4-byte boundary when they don't, as
/// [`TopSubtree::read_wire`] takes them: a deserialized `Vec<u8>` or a value the store keeps
/// need not start at a word. `scratch` is kept across reads, so a copy takes no new memory.
pub fn aligned<'a>(bytes: &'a [u8], scratch: &'a mut Vec<u8>) -> &'a [u8] {
    if bytes.as_ptr().addr().is_multiple_of(4) {
        return bytes;
    }
    scratch.clear();
    // alloc: only for bytes off a word, into the caller's buffer, which it reuses.
    scratch.resize(bytes.len() + 3, 0);
    // Not `align_offset`, which may answer `usize::MAX`.
    let at = scratch.as_ptr().addr().wrapping_neg() % 4;
    scratch[at..at + bytes.len()].copy_from_slice(bytes);
    let scratch: &'a [u8] = scratch;
    &scratch[at..at + bytes.len()]
}

impl TopSubtree {
    /// Reads a tree [`Tree::write_wire`] wrote at the start of `bytes`, and how many bytes
    /// it took. `part` turns each [`WirePart`] into the anchor and context it stands for here,
    /// or refuses it: the ids are the writer's, and only the reader knows which it may use.
    /// `bytes` start at a 4-byte boundary, as a heap buffer or a tree after another one does.
    /// A header past `limits` is refused (its counts before any column is read, its length
    /// once the nodes are), and no symbol is interned unless the whole tree is accepted. `part`
    /// runs in the middle of the read and must not wait (rule 27): it is a lookup.
    pub fn read_wire(
        bytes: &[u8],
        limits: WireLimits,
        part: &mut impl FnMut(WirePart) -> Result<(SpanAnchor, SyntaxContext), String>,
    ) -> Result<(TopSubtree, usize), WireError> {
        TopSubtree::read_wire_keeping(bytes, limits, part, true, super::KEPT)
    }
}

impl<A: Allocator> TopSubtree<A> {
    /// [`TopSubtree::read_wire`] with the block in `alloc`: a task's temporary, which keeps no
    /// parser words, as [`TopSubtreeBuilder::build_in`](super::TopSubtreeBuilder::build_in)'s.
    pub fn read_wire_in(
        bytes: &[u8],
        limits: WireLimits,
        part: &mut impl FnMut(WirePart) -> Result<(SpanAnchor, SyntaxContext), String>,
        alloc: A,
    ) -> Result<(TopSubtree<A>, usize), WireError> {
        TopSubtree::read_wire_keeping(bytes, limits, part, false, alloc)
    }

    fn read_wire_keeping(
        bytes: &[u8],
        limits: WireLimits,
        part: &mut impl FnMut(WirePart) -> Result<(SpanAnchor, SyntaxContext), String>,
        keeps_words: bool,
        alloc: A,
    ) -> Result<(TopSubtree<A>, usize), WireError> {
        let mut words = Words { read: Reader::new(bytes) };
        let [magic, nodes_len, wide_len, parts_len, symbols_len, symbol_bytes] =
            words.rows::<6>(1)?.next().expect("one header row is read");
        if magic != MAGIC {
            return Err(WireError::Magic(magic));
        }
        let [nodes_len, wide_len, parts_len, symbols_len, symbol_bytes] =
            [nodes_len, wide_len, parts_len, symbols_len, symbol_bytes].map(|it| it as usize);
        if nodes_len == 0 {
            return malformed("a tree without its top subtree", 0);
        }
        WireLimits::check("nodes (token trees)", nodes_len, limits.nodes)?;
        WireLimits::check("symbols", symbols_len, limits.symbols)?;
        WireLimits::check("bytes of symbol text", symbol_bytes, limits.symbol_bytes)?;
        // Each symbol is some ident's or literal's, and the top subtree is neither.
        if symbols_len >= nodes_len {
            return malformed("more symbols than token trees to use them", symbols_len);
        }

        // Phase one: every check, from the bytes where they lie and the symbols' text, with
        // nothing interned and no block allocated.
        let node_words: &[u32] = words.column(nodes_len.saturating_mul(2))?;
        let subtrees = check_nodes(node_words.as_chunks::<2>().0, symbols_len)?;
        let wire_words = [
            HEADER_WORDS,
            nodes_len.saturating_mul(2),
            (nodes_len + subtrees).saturating_mul(2),
            wide_len.saturating_add(parts_len).saturating_mul(3),
            symbols_len,
            10,
        ]
        .into_iter()
        .fold(0usize, usize::saturating_add);
        let wire_bytes =
            wire_words.saturating_mul(4).saturating_add(symbol_bytes.next_multiple_of(4));
        WireLimits::check("bytes of wire", wire_bytes, limits.bytes)?;
        let span_words: &[u32] = words.column((nodes_len + subtrees).saturating_mul(2))?;
        let wide_words: &[u32] = words.column(wide_len.saturating_mul(3))?;
        for (at, &row) in wide_words.as_chunks::<3>().0.iter().enumerate() {
            let it = WideSpan::from_row(row);
            if it.part as usize >= parts_len {
                return malformed("a wide span's part past the parts", at);
            }
            if it.start.checked_add(it.len).is_none() {
                return malformed("a wide span ending past 4 GiB", at);
            }
        }
        check_spans(span_words.as_chunks::<2>().0, wide_len, parts_len)?;
        let wire_parts = words.rows::<3>(parts_len)?;
        let ends: &[u32] = words.column(symbols_len)?;
        let mut last = 0;
        for (at, &end) in ends.iter().enumerate() {
            let end = end as usize;
            if end < last || end > symbol_bytes {
                return malformed("a symbol ending before the last or past the symbols' bytes", at);
            }
            last = end;
        }
        if last != symbol_bytes {
            return malformed("symbols' bytes past the last symbol", symbols_len);
        }
        let tops = words.rows::<5>(2)?;
        let text = words.bytes(symbol_bytes)?;
        let text = std::str::from_utf8(text).map_err(|e| WireError::Malformed {
            what: "symbols that are not UTF-8",
            at: e.valid_up_to(),
        })?;
        if let Some(at) = ends.iter().position(|&end| !text.is_char_boundary(end as usize)) {
            return malformed("a symbol ending inside a char", at);
        }
        words.pad()?;
        let total =
            HEADER + 2 * (2 * nodes_len + subtrees) + 3 * (wide_len + parts_len) + symbols_len;
        if u32::try_from(total).is_err() {
            return malformed("a tree of 2^32 words or more", 0);
        }

        with_kept(&PARTS, |resolved| {
            with_kept(&HASHES, |hashes| {
                for (at, [file_id, ast_id, ctx]) in wire_parts.enumerate() {
                    resolved.push(resolve(part, WirePart { file_id, ast_id, ctx }, at)?.row());
                }
                let mut top = [None; 2];
                for (at, [start, len, file_id, ast_id, ctx]) in tops.enumerate() {
                    let Some(end) = start.checked_add(len) else {
                        return malformed("a top span ending past 4 GiB", at);
                    };
                    let here = resolve(part, WirePart { file_id, ast_id, ctx }, parts_len + at)?;
                    let range = TextRange::new(TextSize::new(start), TextSize::new(end));
                    top[at] = Some(here.recombine(range));
                }
                let [Some(top_open), Some(top_close)] = top else {
                    unreachable!("two top spans are read")
                };
                let mut from = 0;
                hashes.extend(ends.iter().map(|&end| {
                    let symbol = &text.as_bytes()[from..end as usize];
                    from = end as usize;
                    blaze_phf::name_hash(symbol)
                }));
                let texts = Texts { text, ends, hashes };
                let columns = Columns {
                    nodes: node_words.as_chunks::<2>().0,
                    spans: span_words.as_chunks::<2>().0,
                    wide: wide_words.as_chunks::<3>().0,
                    parts: &resolved[..],
                    // Not interned yet: the checks read `texts`.
                    symbols: &[],
                };
                check_literals(columns, &texts)?;
                check_canonical(columns, &texts)?;

                // Phase two, which can't fail: the block, then the symbols interned into it.
                let layout =
                    Layout::new(nodes_len, nodes_len + subtrees, wide_len, parts_len, symbols_len);
                // alloc: the tree, one block of its exact size, in `alloc`.
                let mut block = Vec::with_capacity_in(layout.end as usize, alloc);
                block.extend_from_slice(&layout.header());
                block.extend_from_slice(node_words);
                block.extend_from_slice(span_words);
                block.extend_from_slice(wide_words);
                block.extend_from_slice(resolved.as_flattened());
                block.extend(
                    (0..symbols_len)
                        .map(|at| Symbol::intern_hashed(texts.get(at), texts.hashes[at]).id()),
                );
                debug_assert_eq!(block.len(), layout.end as usize, "the columns are the layout's");
                let block = block.into_boxed_slice();
                let tree = TopSubtree::from_block(block, layout, top_open, top_close, keeps_words);
                Ok((tree, words.read.at()))
            })
        })
    }
}

thread_local! {
    /// The parts a read resolved, kept for the next read: checked for duplicates before the
    /// block they go in is allocated.
    // alloc: one buffer per thread, kept at most [`KEEP`] parts between reads.
    static PARTS: Cell<Stack<[u32; 3]>> = const { Cell::new(Stack::new()) };
    /// Each symbol's `name_hash` while a read checks them, kept for the next read; interning
    /// takes it, so no symbol is hashed twice.
    // alloc: one buffer per thread, kept at most [`KEEP`] hashes between reads.
    static HASHES: Cell<Stack<u64>> = const { Cell::new(Stack::new()) };
}

/// `f` with the thread's buffer of `key`, emptied; it goes back to the thread `f` ends on, cut
/// back to [`KEEP`] items when a read grew it past them, so bytes refused after filling it (a
/// part repeated millions of times, nesting millions deep) leave no memory held on the worker.
/// The buffer is moved out and back in by two calls that each find the thread's slot afresh,
/// so `f` may resume on another thread without writing this one's (rule 27).
fn with_kept<T: 'static, R>(
    key: &'static LocalKey<Cell<Stack<T>>>,
    f: impl FnOnce(&mut Vec<T>) -> R,
) -> R {
    let mut kept = take_kept(key);
    let result = f(kept.take());
    if kept.high_water() > KEEP {
        kept.shrink(KEEP);
    }
    give_kept(key, kept);
    result
}

#[inline(never)]
fn take_kept<T: 'static>(key: &'static LocalKey<Cell<Stack<T>>>) -> Stack<T> {
    key.replace(Stack::new())
}

#[inline(never)]
fn give_kept<T: 'static>(key: &'static LocalKey<Cell<Stack<T>>>, kept: Stack<T>) {
    key.set(kept);
}

/// The symbols as the wire carries them, before any is interned: symbol `at` is the text up to
/// `ends[at]` from the one before's end, and `hashes[at]` its `name_hash`.
struct Texts<'a> {
    text: &'a str,
    ends: &'a [u32],
    hashes: &'a [u64],
}

impl<'a> Texts<'a> {
    fn len(&self) -> usize {
        self.ends.len()
    }

    /// Symbol `at`'s text: its ends were checked to be in order and on char boundaries.
    fn get(&self, at: usize) -> &'a str {
        let from = match at {
            0 => 0,
            _ => self.ends[at - 1] as usize,
        };
        &self.text[from..self.ends[at] as usize]
    }

    /// Whether symbols `a` and `b` are one text, as their interned ids would be one: the bytes
    /// are compared only when the hashes are equal.
    fn same(&self, a: usize, b: usize) -> bool {
        self.hashes[a] == self.hashes[b] && self.get(a) == self.get(b)
    }
}

fn resolve(
    part: &mut impl FnMut(WirePart) -> Result<(SpanAnchor, SyntaxContext), String>,
    wire: WirePart,
    at: usize,
) -> Result<CompressedSpanPart, WireError> {
    match part(wire) {
        Ok((anchor, ctx)) => Ok(CompressedSpanPart::new(anchor, ctx)),
        Err(why) => Err(WireError::Part { at, part: wire, why }),
    }
}

/// Checks each node is one a constructor makes, every subtree's children end inside its parent,
/// and ordinals count the subtrees in order. Returns how many subtrees there are, the top's
/// aside.
fn check_nodes(nodes: &[[u32; 2]], symbols: usize) -> Result<usize, WireError> {
    with_kept(&ENDS, |ends| check_nesting(nodes, symbols, ends))
}

thread_local! {
    /// [`check_nodes`]' stack of where each open subtree ends, kept for the next read.
    // alloc: one buffer per thread, kept at most [`KEEP`] deep between reads.
    static ENDS: Cell<Stack<usize>> = const { Cell::new(Stack::new()) };
}

fn check_nesting(
    nodes: &[[u32; 2]],
    symbols: usize,
    ends: &mut Vec<usize>,
) -> Result<usize, WireError> {
    let top = Node::from_row(nodes[0]);
    if top.tag() != SUBTREE
        || top != Node::subtree(top.delimiter_kind(), 0, top.data)
        || top.data as usize != nodes.len() - 1
    {
        return malformed("a top subtree not holding the whole tree", 0);
    }
    // Where each open subtree ends, the innermost last.
    ends.push(nodes.len());
    let mut ordinal = 0;
    for (at, &row) in nodes.iter().enumerate().skip(1) {
        let node = Node::from_row(row);
        while ends.last() == Some(&at) {
            ends.pop();
        }
        let same = match node.tag() {
            SUBTREE => {
                ordinal += 1;
                if node.ordinal() != ordinal {
                    return malformed("a subtree out of order", at);
                }
                let end = at + 1 + node.data as usize;
                if ends.last().is_none_or(|&parent| end > parent) {
                    return malformed("a subtree ending past its parent", at);
                }
                ends.push(end);
                Node::subtree(node.delimiter_kind(), ordinal, node.data)
            }
            PUNCT => {
                let Some(char) = char::from_u32(node.data) else {
                    return malformed("a punct that is not a char", at);
                };
                Node::punct(char, node.spacing())
            }
            IDENT => {
                if node.data as usize >= symbols {
                    return malformed("an ident's symbol past the symbols", at);
                }
                Node::ident(node.is_raw(), node.data)
            }
            LITERAL => {
                if node.data as usize >= symbols {
                    return malformed("a literal's symbol past the symbols", at);
                }
                Node::literal(node.lit_kind(), node.suffix_len(), node.data)
            }
            _ => unreachable!("a tag is two bits"),
        };
        if same != node {
            return malformed("a node with bits no constructor sets", at);
        }
    }
    Ok(ordinal as usize)
}

/// Checks each packed span indexes what is there and ends below 4 GiB.
fn check_spans(spans: &[[u32; 2]], wide: usize, parts: usize) -> Result<(), WireError> {
    if PackedSpan::from_row(spans[0]) != PackedSpan::UNUSED {
        return malformed("the top subtree's unused span set", 0);
    }
    for (at, &row) in spans.iter().enumerate().skip(1) {
        let span = PackedSpan::from_row(row);
        if span.len_part == WIDE {
            if span.start as usize >= wide {
                return malformed("a wide span past the wide spans", at);
            }
            continue;
        }
        let (len, part) = (span.len_part & NARROW_LIMIT, span.len_part >> 16);
        if len == NARROW_LIMIT {
            return malformed("a packed span as long as only a wide one is", at);
        }
        if part as usize >= parts {
            return malformed("a span's part past the parts", at);
        }
        if span.start.checked_add(len).is_none() {
            return malformed("a span ending past 4 GiB", at);
        }
    }
    Ok(())
}

/// Checks each literal's suffix is a tail of its text, as a literal's methods slice it.
fn check_literals(tree: Columns<'_>, symbols: &Texts<'_>) -> Result<(), WireError> {
    for (at, &row) in tree.nodes.iter().enumerate() {
        let node = Node::from_row(row);
        if node.tag() != LITERAL {
            continue;
        }
        let text = symbols.get(node.data as usize);
        let suffix = node.suffix_len() as usize;
        if suffix > text.len() || !text.is_char_boundary(text.len() - suffix) {
            return malformed("a literal's suffix longer than its text or inside a char", at);
        }
    }
    Ok(())
}

/// Checks the columns are the builder's for these token trees, from indices and the symbols'
/// text, in the order the builder encodes (each node's span, then a subtree's closing one):
/// each span uses a part already used or the next one, the parts are distinct, a span is wide
/// exactly when it doesn't pack and the wide ones are in order, and each ident or literal has
/// the index the builder's window gives its symbol. No span is unpacked and no part compared
/// but once; a symbol's text is compared with the window's when it is new, its bytes only on
/// an equal hash, so the compares cost at most a window's worth of each symbol's bytes.
fn check_canonical(tree: Columns<'_>, texts: &Texts<'_>) -> Result<(), WireError> {
    let not = |what, at| Err(WireError::NotCanonical { what, at });
    let len = tree.nodes.len();
    let mut order = Order { parts: 0, wide: 0 };
    let mut symbols = 0usize;
    for (at, &row) in tree.nodes.iter().enumerate().skip(1) {
        let node = Node::from_row(row);
        order.span(tree, tree.span(at), at)?;
        match node.tag() {
            SUBTREE => {
                let at = len + node.ordinal() as usize - 1;
                order.span(tree, tree.span(at), at)?;
            }
            PUNCT => {}
            _ => {
                let index = node.data as usize;
                let from = symbols.saturating_sub(SYMBOL_WINDOW);
                if index == symbols {
                    if (from..symbols).any(|it| texts.same(it, index)) {
                        return not("a symbol again in its window", at);
                    }
                    symbols += 1;
                } else if !(from..symbols).contains(&index) {
                    return not("a symbol's index other than its latest in the window", at);
                }
                // An index in the window is its symbol's latest there with no compare: each
                // symbol after it in the window had it in its own window when new, so none is
                // the same text.
            }
        }
    }
    if order.parts != tree.parts.len() {
        return not("a part no span uses", order.parts);
    }
    if order.wide != tree.wide.len() {
        return not("a wide span no span uses", order.wide);
    }
    if symbols != texts.len() {
        return not("a symbol no node uses", symbols);
    }
    if has_duplicates(tree.parts) {
        return not("a part twice", 0);
    }
    Ok(())
}

/// How many parts and wide spans the spans checked so far use.
struct Order {
    parts: usize,
    wide: usize,
}

impl Order {
    #[inline]
    fn span(&mut self, tree: Columns<'_>, span: PackedSpan, at: usize) -> Result<(), WireError> {
        let not = |what| Err(WireError::NotCanonical { what, at });
        let part = if span.len_part == WIDE {
            if span.start as usize != self.wide {
                return not("a wide span out of order");
            }
            self.wide += 1;
            let wide = tree.wide(span.start);
            if wide.len < NARROW_LIMIT && wide.part < NARROW_LIMIT {
                return not("a wide span that packs");
            }
            wide.part
        } else {
            span.len_part >> 16
        };
        match part as usize {
            part if part < self.parts => Ok(()),
            part if part == self.parts => {
                self.parts += 1;
                Ok(())
            }
            _ => not("a part used before the ones before it"),
        }
    }
}

/// Where [`Tree::write_wire`] writes next: the rest of its buffer.
/// Appends `rows` of `N` words to `out`, which has room for them.
#[inline]
fn put_rows<const N: usize>(out: &mut Vec<u8>, rows: impl IntoIterator<Item = [u32; N]>) {
    for row in rows {
        for word in row {
            out.put_u32(word);
        }
    }
}

/// A cursor over the bytes' words.
struct Words<'a> {
    read: Reader<'a>,
}

impl<'a> Words<'a> {
    fn bytes(&mut self, len: usize) -> Result<&'a [u8], WireError> {
        self.read.take(len).map_err(|error| WireError::Truncated { at: error.at, needed: len })
    }

    /// The next `count` rows of `N` words, taken whole first.
    fn rows<const N: usize>(
        &mut self,
        count: usize,
    ) -> Result<impl ExactSizeIterator<Item = [u32; N]> + use<'a, N>, WireError> {
        let bytes = self.bytes(count.saturating_mul(4 * N))?;
        Ok(bytes.chunks_exact(4 * N).map(|row| {
            std::array::from_fn(|i| {
                u32::from_le_bytes(row[4 * i..4 * i + 4].try_into().expect("four bytes"))
            })
        }))
    }

    /// The next `count` items of a column, viewed where they lie: any words are a `T`, which the
    /// checks then hold to what a tree's methods rely on.
    fn column<T: FromBytes + Immutable + KnownLayout>(
        &mut self,
        count: usize,
    ) -> Result<&'a [T], WireError> {
        let at = self.read.at();
        let bytes = self.bytes(count.saturating_mul(size_of::<T>()))?;
        if !bytes.as_ptr().addr().is_multiple_of(align_of::<T>()) {
            return Err(WireError::Malformed {
                what: "bytes not starting at a 4-byte boundary",
                at,
            });
        }
        Ok(<[T]>::ref_from_bytes(bytes).expect("whole rows at their alignment"))
    }

    /// Skips the zeros up to the next word.
    fn pad(&mut self) -> Result<(), WireError> {
        let pad = self.read.at().next_multiple_of(4) - self.read.at();
        if self.bytes(pad)?.iter().any(|&it| it != 0) {
            return malformed("padding that is not zero", self.read.at());
        }
        Ok(())
    }
}

#[cfg(test)]
pub(super) fn parts_of(tree: &Tree) -> Vec<(WirePart, (SpanAnchor, SyntaxContext))> {
    tree.parts()
        .iter()
        .map(|&row| CompressedSpanPart::from_row(row))
        .chain([tree.top_open, tree.top_close].map(|it| CompressedSpanPart::from_span(&it)))
        .map(|it| (WirePart::of(it), (it.anchor(), it.ctx())))
        .collect()
}
