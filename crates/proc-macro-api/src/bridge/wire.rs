//! The encoding of a stream blaze-macro-eval's server gives a macro and reads back: a token
//! tree's columns as `tt::Tree::write_wire` lays them out, each span part named by its index in
//! the [`Parts`] of the expansion, and each span a word of those parts. Trees pass as they are;
//! bytes are written only of text a macro asks to be tokens of.

use blaze_hash::HashMap;
use span::{EditionedFileId, ErasedFileAstId, Span, SpanAnchor, SyntaxContext};
use tt::{DelimiterKind, Leaf, LitKind, Spacing, Subtree, TokenTree, TopSubtree, TopSubtreeBuilder, WirePart};

/// A packed span's `len_part` that makes its `start` an index of the wide spans.
pub const WIDE: u32 = u32::MAX;
/// What a packed span's length and part each stay under.
pub const NARROW: u32 = 0xFFFF;

/// A span's anchor and context: what the spans of an expansion share.
pub type Part = (SpanAnchor, SyntaxContext);

/// The span parts of an expansion, in the order first given out (the def, call and mixed
/// sites, the input's, the attribute's, then what span operations make), and its spans too
/// wide to pack. A span is a word: `start | len_part << 32`, `len_part` being `len | part << 16`
/// when both are under [`NARROW`], else [`WIDE`] with `start` an index of the wide spans, as
/// tt packs a span. Both tables hold each entry once and a span packs whenever it can, so
/// equal spans have equal words.
#[derive(Default)]
pub struct Parts {
    parts: tt::IndexTable<Part>,
    /// The part asked for last and its index: tokens near each other share one.
    last: Option<(Part, u32)>,
    wide: Vec<[u32; 3]>,
    wides: HashMap<[u32; 3], u32>,
}

impl Parts {
    /// Forgets every part and wide span, keeping the room, for the next expansion.
    pub fn clear(&mut self) {
        self.parts.clear();
        self.last = None;
        self.wide.clear();
        self.wides.clear();
    }

    pub fn len(&self) -> usize {
        self.parts.list().len()
    }

    pub fn all(&self) -> &[Part] {
        self.parts.list()
    }

    /// The index of the part `(anchor, ctx)`, the next one when it is new.
    #[inline]
    pub fn part(&mut self, anchor: SpanAnchor, ctx: SyntaxContext) -> u32 {
        let part = (anchor, ctx);
        if let Some((last, index)) = self.last
            && last == part
        {
            return index;
        }
        let index = self.parts.index(part);
        self.last = Some((part, index));
        index
    }

    /// The word of the span `start..start + len` of the part at `part`, which is one.
    #[inline]
    pub fn word(&mut self, start: u32, len: u32, part: u32) -> u64 {
        if len < NARROW && part < NARROW {
            return u64::from(start) | u64::from(len | part << 16) << 32;
        }
        self.wide_word([start, len, part])
    }

    #[cold]
    fn wide_word(&mut self, span: [u32; 3]) -> u64 {
        let next = self.wide.len() as u32;
        let wide = &mut self.wide;
        let at = *self.wides.entry(span).or_insert_with(|| {
            wide.push(span);
            next
        });
        u64::from(at) | u64::from(WIDE) << 32
    }

    /// The part a tree names: `{ file_id: index, ast_id: 0, ctx: 0 }` with an index of this
    /// expansion's parts, and nothing else.
    pub fn named(&self, part: WirePart) -> Result<Part, String> {
        match part {
            WirePart { file_id, ast_id: 0, ctx: 0 } if (file_id as usize) < self.len() => {
                Ok(self.all()[file_id as usize])
            }
            // alloc: the message of a tree refused.
            _ => Err(format!("the expansion has {} span parts, each named by its index alone", self.len())),
        }
    }

    /// The index of the part of the raw ids `write_wire` wrote of it.
    pub fn part_written(&mut self, file_id: u32, ast_id: u32, ctx: u32) -> u32 {
        let anchor = SpanAnchor {
            file_id: EditionedFileId::from_raw(file_id),
            ast_id: ErasedFileAstId::from_raw(ast_id),
        };
        // `ctx` is what `write_wire` wrote of a part of a tree of this process the moment
        // before (`relative`); a wrong value panics in `from_u32` or in the query engine's
        // checked lookup, or names another context's hygiene.
        let ctx = SyntaxContext::from_u32(ctx);
        self.part(anchor, ctx)
    }
}

/// The words of a tree's header.
const HEADER: usize = 6;

/// Where a tree's bytes hold its spans and what names its span parts: byte offsets, and counts.
pub struct Sections {
    /// How many nodes there are, the top subtree's included.
    pub nodes: usize,
    pub spans: usize,
    pub wide: usize,
    pub wides: usize,
    pub parts: usize,
    pub count: usize,
    /// The top subtree's two spans, five words each.
    pub top: usize,
}

fn get(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn put(bytes: &mut [u8], at: usize, words: [u32; 3]) {
    for (i, word) in words.into_iter().enumerate() {
        bytes[at + 4 * i..at + 4 * i + 4].copy_from_slice(&word.to_le_bytes());
    }
}

/// The sections of `bytes`, exactly one tree `write_wire` wrote. The header doesn't count the
/// subtrees, whose closing spans the span column has after the nodes': the length gives them.
pub fn sections(bytes: &[u8]) -> Option<Sections> {
    let word = |at: usize| {
        Some(u32::from_le_bytes(bytes.get(4 * at..4 * at + 4)?.try_into().ok()?) as usize)
    };
    let (nodes, wides, count, symbols, text) = (word(1)?, word(2)?, word(3)?, word(4)?, word(5)?);
    let fixed = HEADER + 4 * nodes + 3 * wides + 3 * count + symbols + 10;
    let closes = (bytes.len() / 4).checked_sub(text.div_ceil(4))?.checked_sub(fixed)? / 2;
    let spans = 4 * (HEADER + 2 * nodes);
    let wide = spans + 8 * (nodes + closes);
    let parts = wide + 12 * wides;
    let top = parts + 12 * count + 4 * symbols;
    Some(Sections { nodes, spans, wide, wides, parts, count, top })
}

/// Appends `tree`'s columns to `out` with each span part named by its index in `parts`
/// (`{ file_id: index, ast_id: 0, ctx: 0 }`), putting its parts and the spans of it that don't
/// pack in `parts` in the order the tree uses them, then its top subtree's: bytes free of file
/// ids and contexts, which the module reads and an expansion's key hashes.
pub fn relative(tree: &TopSubtree, parts: &mut Parts, out: &mut Vec<u8>) {
    let base = out.len();
    tree.write_wire(out);
    let bytes = &mut out[base..];
    let at = sections(bytes).expect("`write_wire` wrote one whole tree");
    for row in 0..at.count {
        let it = at.parts + 12 * row;
        let index = parts.part_written(get(bytes, it), get(bytes, it + 4), get(bytes, it + 8));
        put(bytes, it, [index, 0, 0]);
    }
    let read: &[u8] = bytes;
    let part = |local: u32| get(read, at.parts + 12 * local as usize);
    if parts.len() < NARROW as usize {
        // No span the tree packs fails to pack among the expansion's parts.
        for row in 0..at.wides {
            let it = at.wide + 12 * row;
            parts.word(get(read, it), get(read, it + 4), part(get(read, it + 8)));
        }
    } else {
        // Each span in the order the tree uses it: a node's, then a subtree's closing one.
        let mut register = |span: usize| {
            let (start, len_part) = (get(read, span), get(read, span + 4));
            let [start, len, local] = match len_part {
                WIDE => {
                    let it = at.wide + 12 * start as usize;
                    [get(read, it), get(read, it + 4), get(read, it + 8)]
                }
                _ => [start, len_part & NARROW, len_part >> 16],
            };
            parts.word(start, len, part(local));
        };
        let mut ordinal = 0;
        for node in 1..at.nodes {
            register(at.spans + 8 * node);
            if get(read, 4 * HEADER + 8 * node) & 0b11 == 0 {
                ordinal += 1;
                register(at.spans + 8 * (at.nodes + ordinal - 1));
            }
        }
    }
    for row in 0..2 {
        let it = at.top + 20 * row;
        let index =
            parts.part_written(get(bytes, it + 8), get(bytes, it + 12), get(bytes, it + 16));
        parts.word(get(bytes, it), get(bytes, it + 4), index);
        put(bytes, it + 8, [index, 0, 0]);
    }
}

/// The tree of `bytes`, all of them, each span part it names one of `parts`: an error saying
/// what is wrong with them otherwise.
pub fn read(bytes: &[u8], parts: &Parts) -> Result<TopSubtree, String> {
    // alloc: none: `room` takes memory only for bytes `aligned` copies.
    read_in(bytes, parts, &mut Vec::new())
}

/// [`read`], copying bytes off a 4-byte boundary into the caller's `room`, kept across
/// reads, so a hit read from wherever the store has it takes no new memory for the copy.
pub fn read_in(bytes: &[u8], parts: &Parts, room: &mut Vec<u8>) -> Result<TopSubtree, String> {
    let bytes = tt::aligned(bytes, room);
    let (tree, used) = TopSubtree::read_wire(bytes, &mut |part| parts.named(part))
        // alloc: the message of a tree refused.
        .map_err(|e| e.to_string())?;
    match bytes.len() - used {
        0 => Ok(tree),
        // alloc: the message of a tree refused.
        left => Err(format!("{left} bytes after the token tree")),
    }
}

/// `trees` in a top subtree without delimiters at `at`, as they are: what text the module asks
/// to be tokens of is given back as.
pub fn invisible(trees: impl IntoIterator<Item = Tree>, at: Span) -> TopSubtree {
    let mut builder = TopSubtreeBuilder::new(tt::Delimiter::invisible_spanned(at));
    push(&mut builder, trees);
    builder.build()
}

fn push(builder: &mut TopSubtreeBuilder, trees: impl IntoIterator<Item = Tree>) {
    for tree in trees {
        match tree {
            Tree::Leaf(leaf) => builder.push(leaf),
            Tree::Group { kind, open, close, entire: _, trees } => {
                builder.open(kind, open);
                push(builder, trees);
                builder.close(close);
            }
        }
    }
}

/// A stream the host made: what the lexer makes of text (`lex.rs`).
pub enum Tree {
    Group { kind: DelimiterKind, open: Span, close: Span, entire: Span, trees: Vec<Tree> },
    Leaf(Leaf),
}

/// rust-analyzer's tree of a stream: its one group, or the stream in a group without
/// delimiters at `call_site`, as its server writes a macro's output.
pub fn top_subtree(trees: Vec<Tree>, call_site: Span) -> TopSubtree {
    // alloc: the tree's own token column.
    let mut flat = Vec::new();
    match <[Tree; 1]>::try_from(trees) {
        Ok([group @ Tree::Group { .. }]) => flatten(group, &mut flat),
        // alloc: the one leaf's top subtree, of text lexed outside an expansion.
        Ok([leaf]) => flatten(top_of(vec![leaf], call_site), &mut flat),
        Err(trees) => flatten(top_of(trees, call_site), &mut flat),
    }
    TopSubtree::from_serialized(flat)
}

fn top_of(trees: Vec<Tree>, call_site: Span) -> Tree {
    let (open, close, entire) = (call_site, call_site, call_site);
    Tree::Group { kind: DelimiterKind::Invisible, open, close, entire, trees }
}

fn flatten(tree: Tree, flat: &mut Vec<TokenTree>) {
    match tree {
        // `Literal::i32_suffixed(-1)` is one token to the macro and a minus sign and a literal
        // to the compiler, as rust-analyzer's proc-macro server's reader split it.
        Tree::Leaf(Leaf::Literal(literal))
            if matches!(literal.kind, LitKind::Integer | LitKind::Float)
                && literal.text().starts_with('-') =>
        {
            let (text, suffix) = literal.text_and_suffix();
            let span = literal.span;
            let minus = tt::Punct { char: '-', spacing: Spacing::Alone, span };
            let magnitude = tt::Literal::new(&text[1..], span, literal.kind, suffix);
            flat.push(TokenTree::Leaf(Leaf::Punct(minus)));
            flat.push(TokenTree::Leaf(Leaf::Literal(magnitude)));
        }
        Tree::Leaf(leaf) => flat.push(TokenTree::Leaf(leaf)),
        Tree::Group { kind, open, close, entire: _, trees } => {
            let at = flat.len();
            flat.push(TokenTree::Subtree(Subtree {
                delimiter: tt::Delimiter { open, close, kind },
                len: 0,
            }));
            for tree in trees {
                flatten(tree, flat);
            }
            let len = (flat.len() - at - 1) as u32;
            if let TokenTree::Subtree(subtree) = &mut flat[at] {
                subtree.len = len;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::{at, tokens},
        *,
    };

    #[test]
    fn a_negative_number_comes_out_as_a_minus_and_a_number() {
        let number = |text: &str, kind: tt::LitKind, suffix: &str| {
            Tree::Leaf(tt::Leaf::Literal(tt::Literal::new(text, at(0, 0), kind, suffix)))
        };
        let trees = vec![
            number("-1", tt::LitKind::Integer, "i8"),
            number("-1.5", tt::LitKind::Float, ""),
            number("2", tt::LitKind::Integer, ""),
        ];
        let want = tokens("-1i8 -1.5 2", at(0, 0)).unwrap();
        assert_eq!(top_subtree(trees, at(0, 0)).to_string(), want.to_string());
    }
}
