//! The columns against the old encoding ([`VarintTree`]) and against what the token trees say:
//! the same flat token trees, spans, `Display` output and equality answers, over seeded random
//! trees and over the parser's test data lexed into token trees.

use std::{fmt::Write as _, hash::BuildHasher, path::Path};

use blaze_hash::FixedState;
use blaze_ids::rng::SplitMix64;
use hegel::generators as gs;
use span::ErasedFileAstId;

use super::{varint::VarintTree, *};
use crate::{FlatToken, TtElement, buffer::Cursor, token_to_literal};

fn anchor(file: u32, ast_id: u32) -> SpanAnchor {
    SpanAnchor {
        file_id: EditionedFileId::new(FileId::from_raw(file), Edition::CURRENT),
        ast_id: ErasedFileAstId::from_raw(ast_id),
    }
}

const EDITIONS: [Edition; 4] =
    [Edition::Edition2015, Edition::Edition2018, Edition::Edition2021, Edition::Edition2024];

/// Random token trees: spans from a few parts, with now and then a range too long or too far
/// to pack; symbols from a small pool or new; every kind of leaf and delimiter.
struct Gen {
    rng: SplitMix64,
    parts: Vec<(SpanAnchor, SyntaxContext)>,
    pool: Vec<Symbol>,
}

impl Gen {
    fn new(seed: u64, parts: usize) -> Gen {
        let mut rng = SplitMix64::new(seed);
        let parts = (0..parts)
            .map(|_| {
                let edition = EDITIONS[rng.below(4) as usize];
                (anchor(rng.below(3) as u32, rng.below(5) as u32), SyntaxContext::root(edition))
            })
            .collect();
        let pool = ["self", "a", "foo", "Bar", "_", "r", "fn", "x", "\"str\"", "1u8", "0", "1.5"]
            .iter()
            .map(|it| Symbol::intern(it))
            .collect();
        Gen { rng, parts, pool }
    }

    fn span(&mut self) -> Span {
        let (anchor, ctx) = self.parts[self.rng.below(self.parts.len() as u64) as usize];
        let start = match self.rng.below(8) {
            0 => self.rng.below(1 << 30),
            1 => self.rng.below(1 << 18),
            _ => self.rng.below(4096),
        } as u32;
        let len = match self.rng.below(16) {
            0 => 0xFFFF + self.rng.below(1 << 18),
            1 => 0xFFFE + self.rng.below(3),
            _ => self.rng.below(40),
        } as u32;
        Span { range: TextRange::at(TextSize::new(start), TextSize::new(len)), anchor, ctx }
    }

    fn symbol(&mut self) -> Symbol {
        match self.rng.below(3) {
            0 => Symbol::intern(&format!("s{}", self.rng.below(200_000))),
            _ => self.pool[self.rng.below(self.pool.len() as u64) as usize],
        }
    }

    fn spacing(&mut self) -> Spacing {
        [Spacing::Alone, Spacing::Joint, Spacing::JointHidden][self.rng.below(3) as usize]
    }

    fn leaf(&mut self) -> Leaf {
        let span = self.span();
        match self.rng.below(3) {
            0 => {
                const PUNCTS: &[u8] = b"+-*/%^!&|=<>@.,;:#$?~'";
                let char = match self.rng.one_in(30) {
                    true => ['\u{a7}', '\u{3bb}', '\u{1f980}'][self.rng.below(3) as usize],
                    false => char::from(PUNCTS[self.rng.below(PUNCTS.len() as u64) as usize]),
                };
                Leaf::Punct(Punct { char, spacing: self.spacing(), span })
            }
            1 => {
                let is_raw = [IdentIsRaw::No, IdentIsRaw::Yes][self.rng.below(2) as usize];
                Leaf::Ident(Ident { sym: self.symbol(), span, is_raw })
            }
            _ => {
                let hashes = match self.rng.below(3) {
                    0 => 0,
                    1 => 1,
                    _ => self.rng.below(256) as u8,
                };
                let kind = [
                    LitKind::Byte,
                    LitKind::Char,
                    LitKind::Integer,
                    LitKind::Float,
                    LitKind::Str,
                    LitKind::StrRaw(hashes),
                    LitKind::ByteStr,
                    LitKind::ByteStrRaw(hashes),
                    LitKind::CStr,
                    LitKind::CStrRaw(hashes),
                    LitKind::Err(()),
                ][self.rng.below(11) as usize];
                // Now and then a suffix past a byte's 255, the length a `u8` once held (queue
                // 00023): `1` and 256 or more letters is a literal rustc lexes.
                let len = match self.rng.below(50) {
                    0 => 256 + self.rng.below(300) as usize,
                    _ => self.rng.below(40) as usize,
                };
                let text: String =
                    (0..len).map(|_| char::from(b'a' + self.rng.below(26) as u8)).collect();
                let suffix_len = match len > 255 {
                    true => len as u32 - self.rng.below(2) as u32,
                    false => self.rng.below(len.min(30) as u64 + 1) as u32,
                };
                Leaf::Literal(Literal { text_and_suffix: Symbol::intern(&text), span, kind, suffix_len })
            }
        }
    }

    fn delimiter(&mut self) -> Delimiter {
        let kind = [
            DelimiterKind::Parenthesis,
            DelimiterKind::Brace,
            DelimiterKind::Bracket,
            DelimiterKind::Invisible,
        ][self.rng.below(4) as usize];
        let open = self.span();
        let close = match self.rng.below(3) {
            0 => open,
            1 => self.span(),
            _ => Span {
                range: open.range + TextSize::new(self.rng.below(3000) as u32),
                ..open
            },
        };
        Delimiter { open, close, kind }
    }

    fn token_trees(&mut self, out: &mut Vec<TokenTree>, budget: u64, depth: u32) {
        for _ in 0..self.rng.below(budget + 1) {
            if depth < 6 && self.rng.one_in(5) {
                let at = out.len();
                out.push(TokenTree::Leaf(self.leaf()));
                self.token_trees(out, budget / 2, depth + 1);
                let len = (out.len() - at - 1) as u32;
                out[at] = TokenTree::Subtree(Subtree { delimiter: self.delimiter(), len });
            } else {
                out.push(TokenTree::Leaf(self.leaf()));
            }
        }
    }

    /// A tree's token trees, the top subtree first.
    fn tree(&mut self, budget: u64) -> Vec<TokenTree> {
        let mut out = vec![TokenTree::Leaf(self.leaf())];
        self.token_trees(&mut out, budget, 0);
        out[0] = TokenTree::Subtree(Subtree { delimiter: self.delimiter(), len: out.len() as u32 - 1 });
        out
    }
}

fn flat(tree: &TopSubtree) -> Vec<TokenTree> {
    tree.as_token_trees().iter_flat_tokens().collect()
}

/// `Display` of token trees, as the old one printed them, from the flat token trees alone.
fn reference_display(tokens: &[TokenTree]) -> String {
    fn write_seq(mut tokens: &[TokenTree], out: &mut String) {
        let mut needs_space = false;
        while let Some((first, rest)) = tokens.split_first() {
            if needs_space {
                out.push(' ');
            }
            needs_space = true;
            tokens = rest;
            match first {
                TokenTree::Leaf(Leaf::Punct(punct)) => {
                    needs_space = punct.spacing == Spacing::Alone;
                    out.push(punct.char);
                }
                TokenTree::Leaf(leaf) => write!(out, "{leaf}").unwrap(),
                TokenTree::Subtree(subtree) => {
                    let (open, close) = match subtree.delimiter.kind {
                        DelimiterKind::Parenthesis => ("(", ")"),
                        DelimiterKind::Brace => ("{", "}"),
                        DelimiterKind::Bracket => ("[", "]"),
                        DelimiterKind::Invisible => ("", ""),
                    };
                    let (children, after) = rest.split_at(subtree.usize_len());
                    out.push_str(open);
                    write_seq(children, out);
                    out.push_str(close);
                    tokens = after;
                }
            }
        }
    }
    let mut out = String::new();
    write_seq(tokens, &mut out);
    out
}

fn reference_last_span(tokens: &[TokenTree]) -> Option<Span> {
    tokens.last().map(|last| match last {
        TokenTree::Leaf(leaf) => *leaf.span(),
        TokenTree::Subtree(subtree) => {
            assert_eq!(subtree.len, 0);
            subtree.delimiter.close
        }
    })
}

/// The flat token trees, read back through `TtIter`.
fn walk_iter(tree: &TopSubtree) -> Vec<TokenTree> {
    fn walk(mut iter: TtIter<'_>, out: &mut Vec<TokenTree>) {
        loop {
            let peeked = iter.peek().map(|it| it.first_span());
            let remaining = iter.remaining().len();
            let Some(element) = iter.next() else { break };
            assert_eq!(peeked, Some(element.first_span()));
            match element {
                TtElement::Leaf(leaf) => out.push(leaf.into()),
                TtElement::Subtree(subtree, inner) => {
                    assert_eq!(remaining, iter.remaining().len() + 1 + subtree.usize_len());
                    out.push(subtree.into());
                    walk(inner, out);
                }
            }
        }
    }
    let mut out = vec![TokenTree::Subtree(tree.top_subtree())];
    walk(tree.iter(), &mut out);
    out
}

/// The flat token trees, read back through a `Cursor`.
fn walk_cursor(tree: &TopSubtree) -> Vec<TokenTree> {
    let tokens = flat(tree);
    let mut cursor = Cursor::new(tree.as_token_trees());
    let mut out = Vec::new();
    while !cursor.eof() {
        while let Some(tt) = cursor.token_tree() {
            if let Some([a, b]) = cursor.peek_two_leaves() {
                assert_eq!(TokenTree::Leaf(a), tt);
                assert_eq!(TokenTree::Leaf(b), tokens[out.len() + 1]);
            }
            out.push(tt);
            cursor.bump();
        }
        cursor.bump_or_end();
    }
    assert_eq!(cursor.crossed().len(), tokens.len());
    out
}

/// The tokens of flat token trees as a parser reads them, each with its context, from the
/// token trees alone.
fn reference_tokens(tokens: &[TokenTree]) -> Vec<(FlatToken, SyntaxContext)> {
    fn seq(mut tokens: &[TokenTree], out: &mut Vec<(FlatToken, SyntaxContext)>) {
        while let Some((first, rest)) = tokens.split_first() {
            tokens = rest;
            let token = match first {
                TokenTree::Leaf(Leaf::Punct(it)) => {
                    (FlatToken::Punct { char: it.char, spacing: it.spacing }, it.span.ctx)
                }
                TokenTree::Leaf(Leaf::Ident(it)) => {
                    (FlatToken::Ident { sym: it.sym, is_raw: it.is_raw }, it.span.ctx)
                }
                TokenTree::Leaf(Leaf::Literal(it)) => {
                    let sym = it.text_and_suffix;
                    let (kind, suffix_len) = (it.kind, it.suffix_len);
                    (FlatToken::Literal { sym, kind, suffix_len }, it.span.ctx)
                }
                TokenTree::Subtree(subtree) => {
                    let (children, after) = rest.split_at(subtree.usize_len());
                    let delimiter = subtree.delimiter;
                    out.push((FlatToken::Open(delimiter.kind), delimiter.open.ctx));
                    seq(children, out);
                    tokens = after;
                    (FlatToken::Close(delimiter.kind), delimiter.close.ctx)
                }
            };
            out.push(token);
        }
    }
    let mut out = Vec::new();
    seq(tokens, &mut out);
    out
}

/// The tokens of `view`, through [`TokenTreesView::for_each_token`].
fn walk_tokens(view: TokenTreesView<'_>) -> Vec<(FlatToken, SyntaxContext)> {
    let mut out = Vec::new();
    view.for_each_token(|token, ctx| out.push((token, ctx)));
    out
}

/// A tree of `tokens` built with `open`, `push` and `close` like a converter does: through an
/// invisible top that is skipped, or not; with tokens pushed and then dropped by `restore`, and
/// leaves in invisible subtrees taken away again, now and then.
fn rebuild(tokens: &[TokenTree], skip: bool, rng: &mut SplitMix64) -> TopSubtree {
    let TokenTree::Subtree(top) = &tokens[0] else { unreachable!("a tree starts with a subtree") };
    let junk = Span {
        range: TextRange::at(TextSize::new(7), TextSize::new(1 << 20)),
        anchor: anchor(9, 9),
        ctx: SyntaxContext::root(Edition::Edition2015),
    };
    let mut builder = match skip {
        true => {
            let mut builder = TopSubtreeBuilder::new(Delimiter::invisible_spanned(junk));
            builder.open(top.delimiter.kind, top.delimiter.open);
            builder
        }
        false => TopSubtreeBuilder::new(top.delimiter),
    };
    let mut open: Vec<(usize, Span)> = Vec::new();
    for (at, token) in tokens.iter().enumerate().skip(1) {
        while let Some(&(end, close)) = open.last()
            && end == at
        {
            builder.close(close);
            open.pop();
        }
        if rng.one_in(7) {
            let point = builder.restore_point();
            builder.push(Leaf::Ident(Ident { sym: Symbol::intern("junk"), span: junk, is_raw: IdentIsRaw::No }));
            builder.open(DelimiterKind::Brace, junk);
            builder.push(Leaf::Punct(Punct { char: '!', spacing: Spacing::Joint, span: junk }));
            builder.close(junk);
            builder.restore(point);
        }
        match token {
            TokenTree::Subtree(subtree) => {
                builder.open(subtree.delimiter.kind, subtree.delimiter.open);
                open.push((at + 1 + subtree.usize_len(), subtree.delimiter.close));
            }
            TokenTree::Leaf(leaf) if rng.one_in(5) => {
                builder.open(DelimiterKind::Invisible, junk);
                builder.push(leaf.clone());
                builder.close(junk);
                builder.remove_last_subtree_if_invisible();
            }
            TokenTree::Leaf(leaf) => builder.push(leaf.clone()),
        }
    }
    while let Some((_, close)) = open.pop() {
        builder.close(close);
    }
    match skip {
        true => {
            builder.close(top.delimiter.close);
            builder.build_skip_top_subtree()
        }
        false => builder.build(),
    }
}

/// Checks the tree of `tokens` against the old encoding and against `tokens`, and returns it.
fn check(tokens: &[TokenTree], rng: &mut SplitMix64) -> TopSubtree {
    let tree = TopSubtree::from_serialized(tokens.to_vec());
    let model = VarintTree::from_serialized(tokens.to_vec());
    assert_eq!(model.flat(), tokens, "the old encoding");
    assert_eq!(flat(&tree), tokens);
    assert_eq!(tree.to_string(), reference_display(tokens));
    assert_eq!(walk_iter(&tree), tokens);
    assert_eq!(walk_cursor(&tree), tokens);
    assert!(rebuild(tokens, false, rng) == tree, "built by `build`");
    assert!(rebuild(tokens, true, rng) == tree, "built by `build_skip_top_subtree`");

    let all = tree.as_token_trees();
    assert_eq!(all.first_span(), Some(tokens[0].first_span()));
    assert_eq!(all.last_span(), reference_last_span(tokens));
    let inner = tree.token_trees();
    assert_eq!(inner.len(), tokens.len() - 1);
    assert_eq!(inner.first_span(), tokens.get(1).map(TokenTree::first_span));
    assert_eq!(inner.last_span(), reference_last_span(&tokens[1..]));
    assert_eq!(walk_tokens(all), reference_tokens(tokens), "the tokens of the whole tree");
    assert_eq!(walk_tokens(inner), reference_tokens(&tokens[1..]), "the tokens in the top");
    assert!(all.try_into_subtree().is_some());
    assert_eq!(TopSubtree::from_subtree(tree.view()), tree);
    assert_eq!(TopSubtree::from_token_trees(tree.top_subtree().delimiter, inner), tree);
    blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
        let framed = TopSubtree::from_serialized_in(tokens.to_vec(), frame);
        assert_eq!(framed.block(), tree.block(), "built in a frame");
        assert!(framed == tree && !framed.keeps_words());
        assert_eq!(flat(&TopSubtree::from_subtree(framed.view())), tokens);
        let delimiter = tree.top_subtree().delimiter;
        assert_eq!(TopSubtree::from_token_trees_in(delimiter, inner, frame).block(), tree.block());
        assert_eq!(TopSubtree::from_subtree_in(tree.view(), frame).block(), tree.block());
    });
    // Damaging every byte costs a read a byte; a lexed file is checked whole only.
    check_wire(&tree, tokens.len() <= DAMAGED_TREES_UP_TO);
    tree
}

const DAMAGED_TREES_UP_TO: usize = 300;

/// Writes `tree` to the wire and reads it back as the same tree; with `damage`, also reads every
/// prefix of its bytes as an error, and each byte with a bit flipped as an error or as a tree
/// that writes exactly those bytes, so a tree read is always the builder's.
fn check_wire(tree: &TopSubtree, damage: bool) {
    let mut bytes = Vec::new();
    tree.write_wire(&mut bytes);
    assert_eq!(bytes.len() % 4, 0);
    let parts = wire::parts_of(tree);
    let mut resolve = |part: WirePart| {
        parts
            .iter()
            .find(|(it, _)| *it == part)
            .map(|&(_, it)| it)
            .ok_or_else(|| "a part the tree didn't have".to_owned())
    };
    let (read, used) = TopSubtree::read_wire(&bytes, &mut resolve).expect("a tree's own bytes");
    assert_eq!(used, bytes.len());
    assert!(read == *tree, "read back");
    assert_eq!(flat(&read), flat(tree));
    if !damage {
        return;
    }
    for len in 0..bytes.len() {
        assert!(TopSubtree::read_wire(&bytes[..len], &mut resolve).is_err(), "a prefix of {len}");
    }
    for at in 0..bytes.len() {
        let mut damaged = bytes.clone();
        damaged[at] ^= 1 << (at % 8);
        if let Ok((read, used)) = TopSubtree::read_wire(&damaged, &mut resolve) {
            let mut again = Vec::new();
            read.write_wire(&mut again);
            assert_eq!(again, damaged[..used], "byte {at} flipped");
        }
    }
}

/// The window as one compare numbers symbols as the scan of the last [`SYMBOL_WINDOW`] did,
/// over IDs that repeat at every distance around the window's edge.
#[test]
fn the_symbol_window_numbers_as_its_scan_did() {
    for seed in 0..40 {
        let mut rng = SplitMix64::new(seed);
        let ids: Vec<u32> = (0..400).map(|_| 1 + rng.below(4 + seed * 3) as u32).collect();
        let (mut symbols, mut scanned) = (Symbols::default(), Vec::<u32>::new());
        for &id in &ids {
            let from = scanned.len().saturating_sub(SYMBOL_WINDOW);
            let expected = match scanned[from..].iter().rposition(|&it| it == id) {
                Some(at) => (from + at) as u32,
                None => {
                    scanned.push(id);
                    (scanned.len() - 1) as u32
                }
            };
            assert_eq!(symbols.index(Symbol::from_held_id(id)), expected, "seed {seed}, id {id}");
        }
        assert_eq!(symbols.list, scanned, "seed {seed}");
    }
}

#[test]
fn a_leaf_is_16_bytes_and_a_subtree_24() {
    assert_eq!(size_of::<Node>() + size_of::<PackedSpan>(), 16);
    assert_eq!(size_of::<Node>() + 2 * size_of::<PackedSpan>(), 24);
    let mut trees = Gen::new(3, 2);
    let tokens = trees.tree(200);
    let tree = TopSubtree::from_serialized(tokens.clone());
    let subtrees = tokens.iter().filter(|it| matches!(it, TokenTree::Subtree(_))).count();
    assert_eq!(tree.nodes().len(), tokens.len());
    assert_eq!(tree.spans().len(), tokens.len() + subtrees - 1);
    let rows = 2 * tree.nodes().len() + 2 * tree.spans().len();
    let tables = 3 * tree.wide().len() + 3 * tree.parts().len() + tree.symbol_ids().len();
    assert_eq!(tree.block().len(), HEADER + rows + tables, "a block is its header and columns");
}

#[test]
fn random_trees_match_the_old_encoding() {
    for seed in 0..300 {
        let mut trees = Gen::new(seed, 1 + (seed % 12) as usize);
        let tokens = trees.tree(1 + seed % 60);
        check(&tokens, &mut trees.rng);
    }
}

#[test]
fn equality_and_hash_are_those_of_the_token_trees() {
    let mut trees = Gen::new(77, 3);
    let mut cases: Vec<Vec<TokenTree>> = Vec::new();
    for _ in 0..30 {
        let tokens = trees.tree(12);
        let mut changed = tokens.clone();
        let at = trees.rng.below(changed.len() as u64) as usize;
        match &mut changed[at] {
            TokenTree::Leaf(Leaf::Punct(punct)) => punct.spacing = trees.spacing(),
            TokenTree::Leaf(Leaf::Ident(ident)) => ident.sym = trees.symbol(),
            TokenTree::Leaf(Leaf::Literal(literal)) => literal.span = trees.span(),
            TokenTree::Subtree(subtree) => subtree.delimiter.close = trees.span(),
        }
        cases.push(tokens.clone());
        cases.push(tokens);
        cases.push(changed);
    }
    let built: Vec<(TopSubtree, VarintTree)> = cases
        .iter()
        .map(|it| (TopSubtree::from_serialized(it.clone()), VarintTree::from_serialized(it.clone())))
        .collect();
    for (a, (tree_a, model_a)) in cases.iter().zip(&built) {
        for (b, (tree_b, model_b)) in cases.iter().zip(&built) {
            let same = a == b;
            assert_eq!(tree_a == tree_b, same);
            assert_eq!(model_a == model_b, same);
            if same {
                assert_eq!(FixedState.hash_one(tree_a), FixedState.hash_one(tree_b));
            }
        }
    }
}

#[test]
fn spans_too_wide_to_pack_and_more_parts_than_fit() {
    let ctx = SyntaxContext::root(Edition::Edition2021);
    let mut tokens = vec![TokenTree::Leaf(Leaf::Punct(Punct {
        char: '+',
        spacing: Spacing::Alone,
        span: Span { range: TextRange::empty(TextSize::new(0)), anchor: anchor(0, 0), ctx },
    }))];
    for n in 0..70_000u32 {
        let span = Span {
            range: TextRange::at(TextSize::new(n * 3), TextSize::new(n % 3 * 0x8000)),
            anchor: anchor(n % 4, n),
            ctx,
        };
        tokens.push(TokenTree::Leaf(Leaf::Ident(Ident {
            sym: Symbol::intern(&format!("w{}", n % 5000)),
            span,
            is_raw: IdentIsRaw::No,
        })));
    }
    let top_span = Span { range: TextRange::at(TextSize::new(1), TextSize::new(1 << 24)), anchor: anchor(1, 1), ctx };
    tokens[0] = TokenTree::Subtree(Subtree {
        delimiter: Delimiter { open: top_span, close: top_span, kind: DelimiterKind::Brace },
        len: 70_000,
    });
    let tree = check(&tokens, &mut SplitMix64::new(5));
    assert!(tree.parts().len() > NARROW_LIMIT as usize);
    assert!(!tree.wide().is_empty());
}

#[test]
fn a_literal_keeps_a_suffix_of_any_length_a_node_holds() {
    let ctx = SyntaxContext::root(Edition::Edition2021);
    let span = Span { range: TextRange::empty(TextSize::new(0)), anchor: anchor(0, 0), ctx };
    let mut tokens = vec![TokenTree::Subtree(Subtree {
        delimiter: Delimiter { open: span, close: span, kind: DelimiterKind::Invisible },
        len: 4,
    })];
    for len in [255, 256, 70_000, MAX_SUFFIX as usize] {
        let suffix = "s".repeat(len);
        let literal = token_to_literal(&format!("1{suffix}"), span);
        assert_eq!((literal.text(), literal.suffix().len()), ("1", len));
        assert_eq!(Literal::new("ab", span, LitKind::Str, &suffix).suffix(), suffix);
        tokens.push(TokenTree::Leaf(Leaf::Literal(literal)));
    }
    check(&tokens, &mut SplitMix64::new(9));
}

#[test]
fn changing_a_tree_in_place_gives_the_tree_of_its_new_token_trees() {
    let mut trees = Gen::new(11, 4);
    for _ in 0..40 {
        let tokens = trees.tree(20);

        let mut tree = TopSubtree::from_serialized(tokens.clone());
        let span = DelimSpan { open: trees.span(), close: trees.span() };
        tree.set_top_subtree_delimiter_span(span);
        tree.set_top_subtree_delimiter_kind(DelimiterKind::Bracket);
        let mut expected = tokens.clone();
        let TokenTree::Subtree(top) = &mut expected[0] else { unreachable!() };
        top.delimiter = Delimiter { open: span.open, close: span.close, kind: DelimiterKind::Bracket };
        assert!(tree == TopSubtree::from_serialized(expected.clone()));
        assert_eq!(flat(&tree), expected);

        if let Some(at) = tokens.iter().position(|it| matches!(it, TokenTree::Leaf(_))) {
            let leaf = trees.leaf();
            let mut tree = TopSubtree::from_serialized(tokens.clone());
            tree.set_token(at, leaf.clone());
            let mut expected = tokens.clone();
            expected[at] = leaf.into();
            assert!(tree == TopSubtree::from_serialized(expected));
        }

        // Every ast id made one: the parts that differed only by theirs become one part.
        let mut tree = TopSubtree::from_serialized(tokens.clone());
        tree.change_every_ast_id(|it| *it = ErasedFileAstId::from_raw(1));
        let one = |span: &mut Span| span.anchor.ast_id = ErasedFileAstId::from_raw(1);
        let mut expected = tokens.clone();
        for token in &mut expected {
            match token {
                TokenTree::Leaf(Leaf::Punct(Punct { span, .. }))
                | TokenTree::Leaf(Leaf::Ident(Ident { span, .. }))
                | TokenTree::Leaf(Leaf::Literal(Literal { span, .. })) => one(span),
                TokenTree::Subtree(subtree) => {
                    one(&mut subtree.delimiter.open);
                    one(&mut subtree.delimiter.close);
                }
            }
        }
        assert!(tree == TopSubtree::from_serialized(expected.clone()));
        assert_eq!(flat(&tree), expected);
    }
}

#[test]
fn extending_alone_makes_the_last_punct_alone() {
    let span = Span {
        range: TextRange::empty(TextSize::new(0)),
        anchor: anchor(0, 0),
        ctx: SyntaxContext::root(Edition::CURRENT),
    };
    let joint = Leaf::Punct(Punct { char: ':', spacing: Spacing::Joint, span });
    let source = TopSubtree::invisible_from_leaves(span, [joint.clone(), joint.clone()]);
    let mut builder = TopSubtreeBuilder::new(Delimiter::invisible_spanned(span));
    builder.extend_with_tt_alone(source.token_trees());
    let alone = Leaf::Punct(Punct { char: ':', spacing: Spacing::Alone, span });
    assert!(builder.build() == TopSubtree::invisible_from_leaves(span, [joint, alone]));
    assert_eq!(TokenTreesView::empty().len(), 0);
    assert_eq!(TokenTreesView::empty().last_span(), None);
}

/// The token trees of a file, lexed as a converter makes them: each punctuation character a
/// punct, joint to a punct right after it, and delimiters as subtrees.
fn lex(file: u32, text: &str) -> TopSubtree {
    use rustc_lexer::TokenKind;

    let anchor = anchor(file, 0);
    let ctx = SyntaxContext::root(Edition::CURRENT);
    let span = |start: usize, len: usize| Span {
        range: TextRange::at(TextSize::new(start as u32), TextSize::new(len as u32)),
        anchor,
        ctx,
    };
    let mut lexed = Vec::new();
    let mut offset = 0;
    for token in rustc_lexer::tokenize(text, rustc_lexer::FrontmatterAllowed::No) {
        let len = token.len as usize;
        if !matches!(
            token.kind,
            TokenKind::Whitespace | TokenKind::LineComment { .. } | TokenKind::BlockComment { .. }
        ) {
            lexed.push((token.kind, offset, len));
        }
        offset += len;
    }
    let mut builder =
        TopSubtreeBuilder::with_capacity(Delimiter::invisible_spanned(span(0, 0)), lexed.len());
    for (at, &(kind, start, len)) in lexed.iter().enumerate() {
        let token = &text[start..start + len];
        let delimiter = |it: &str| match it {
            "(" | ")" => DelimiterKind::Parenthesis,
            "[" | "]" => DelimiterKind::Bracket,
            _ => DelimiterKind::Brace,
        };
        match kind {
            TokenKind::Ident | TokenKind::RawIdent => {
                builder.push(Ident::new(token, span(start, len)).into())
            }
            TokenKind::Literal { .. } => builder.push(token_to_literal(token, span(start, len)).into()),
            _ => match token {
                "(" | "[" | "{" => builder.open(delimiter(token), span(start, len)),
                ")" | "]" | "}" => {
                    if builder.expected_delimiters().next().is_some() {
                        builder.close(span(start, len));
                    }
                }
                _ if token.len() == 1 && token.is_ascii() => {
                    let joint = lexed.get(at + 1).is_some_and(|&(_, next, next_len)| {
                        next == start + 1
                            && next_len == 1
                            && text.as_bytes()[next].is_ascii_punctuation()
                            && !b"()[]{}".contains(&text.as_bytes()[next])
                    });
                    let spacing = if joint { Spacing::Joint } else { Spacing::Alone };
                    let char = char::from(token.as_bytes()[0]);
                    builder.push(Punct { char, spacing, span: span(start, len) }.into());
                }
                _ => builder.push(Ident::new(token, span(start, len)).into()),
            },
        }
    }
    while builder.expected_delimiters().next().is_some() {
        builder.close(span(text.len(), 0));
    }
    builder.build()
}

fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|it| it.expect("a directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|it| it == "rs") {
            out.push(path);
        }
    }
}

/// The parser's test data without errors, and this crate's sources.
fn corpus() -> Vec<(std::path::PathBuf, String)> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut paths = Vec::new();
    for dir in ["parser/test_data/parser/ok", "parser/test_data/parser/inline/ok", "tt/src"] {
        rust_files(&crates.join(dir), &mut paths);
    }
    assert!(paths.len() > 300, "the corpus is {} files", paths.len());
    paths
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            (path, text)
        })
        .collect()
}

#[test]
fn lexed_files_match_the_old_encoding() {
    let mut rng = SplitMix64::new(99);
    for (file, (path, text)) in corpus().iter().enumerate() {
        let tree = lex(file as u32, text);
        let tokens = flat(&tree);
        let checked = check(&tokens, &mut rng);
        assert!(checked == tree, "{}", path.display());
    }
}

/// `token_to_literal` as it was before it stopped joining a number's text and suffix again.
fn old_token_to_literal(text: &str, span: Span) -> Literal {
    use rustc_lexer::LiteralKind;
    use stdx::itertools::Itertools as _;

    let token = rustc_lexer::tokenize(text, rustc_lexer::FrontmatterAllowed::No).next_tuple();
    let Some((rustc_lexer::Token {
        kind: rustc_lexer::TokenKind::Literal { kind, suffix_start },
        ..
    },)) = token
    else {
        return Literal::new_no_suffix(text, span, LitKind::Err(()));
    };
    let (kind, start_offset, end_offset) = match kind {
        LiteralKind::Int { .. } => (LitKind::Integer, 0, 0),
        LiteralKind::Float { .. } => (LitKind::Float, 0, 0),
        LiteralKind::Char { terminated } => (LitKind::Char, 1, terminated as usize),
        LiteralKind::Byte { terminated } => (LitKind::Byte, 2, terminated as usize),
        LiteralKind::Str { terminated } => (LitKind::Str, 1, terminated as usize),
        LiteralKind::ByteStr { terminated } => (LitKind::ByteStr, 2, terminated as usize),
        LiteralKind::CStr { terminated } => (LitKind::CStr, 2, terminated as usize),
        LiteralKind::RawStr { n_hashes } => (
            LitKind::StrRaw(n_hashes.unwrap_or_default()),
            2 + n_hashes.unwrap_or_default() as usize,
            1 + n_hashes.unwrap_or_default() as usize,
        ),
        LiteralKind::RawByteStr { n_hashes } => (
            LitKind::ByteStrRaw(n_hashes.unwrap_or_default()),
            3 + n_hashes.unwrap_or_default() as usize,
            1 + n_hashes.unwrap_or_default() as usize,
        ),
        LiteralKind::RawCStr { n_hashes } => (
            LitKind::CStrRaw(n_hashes.unwrap_or_default()),
            3 + n_hashes.unwrap_or_default() as usize,
            1 + n_hashes.unwrap_or_default() as usize,
        ),
    };
    let (lit, suffix) = text.split_at(suffix_start as usize);
    let lit = &lit[start_offset..lit.len() - end_offset];
    // Quoted literals keep their suffix as rustc's lexer does, not `Err` as they once did.
    let suffix = match suffix {
        "" | "_" => "",
        suffix => suffix,
    };
    Literal::new(lit, span, kind, suffix)
}

#[test]
fn literals_are_what_they_were() {
    let span = Span {
        range: TextRange::empty(TextSize::new(0)),
        anchor: anchor(0, 0),
        ctx: SyntaxContext::root(Edition::CURRENT),
    };
    let mut texts: Vec<String> = [
        "1", "1u8", "1_", "1_u8", "0x1Fusize", "1.0", "1.0f32", "1e3", "1e3_f64", "1.", "'a'",
        "b'a'", "\"s\"", "\"s\"suffix", "r\"x\"", "r#\"x\"#", "br\"x\"", "c\"x\"", "cr#\"x\"#",
        "\"unterminated", "'", "x", "", "1.5e10suffix_long_enough_to_spill_the_inline_buffer",
    ]
    .iter()
    .map(|it| it.to_string())
    .collect();
    for (_, text) in corpus() {
        let mut offset = 0;
        for token in rustc_lexer::tokenize(&text, rustc_lexer::FrontmatterAllowed::No) {
            if matches!(token.kind, rustc_lexer::TokenKind::Literal { .. }) {
                texts.push(text[offset..offset + token.len as usize].to_owned());
            }
            offset += token.len as usize;
        }
    }
    for text in &texts {
        assert_eq!(token_to_literal(text, span), old_token_to_literal(text, span), "{text:?}");
    }
}

/// What `Literal`'s `Display` wrote before `Literal::push_to`.
fn old_literal_display(lit: &Literal) -> String {
    let (text, suffix) = lit.text_and_suffix();
    let mut out = match lit.kind {
        LitKind::Byte => format!("b'{}'", text),
        LitKind::Char => format!("'{}'", text),
        LitKind::Integer | LitKind::Float | LitKind::Err(_) => format!("{}", text),
        LitKind::Str => format!("\"{}\"", text),
        LitKind::ByteStr => format!("b\"{}\"", text),
        LitKind::CStr => format!("c\"{}\"", text),
        LitKind::StrRaw(num_of_hashes) => {
            let num_of_hashes = num_of_hashes as usize;
            format!(r#"r{0:#<num_of_hashes$}"{text}"{0:#<num_of_hashes$}"#, "", text = text)
        }
        LitKind::ByteStrRaw(num_of_hashes) => {
            let num_of_hashes = num_of_hashes as usize;
            format!(r#"br{0:#<num_of_hashes$}"{text}"{0:#<num_of_hashes$}"#, "", text = text)
        }
        LitKind::CStrRaw(num_of_hashes) => {
            let num_of_hashes = num_of_hashes as usize;
            format!(r#"cr{0:#<num_of_hashes$}"{text}"{0:#<num_of_hashes$}"#, "", text = text)
        }
    };
    out.push_str(suffix);
    out
}

#[test]
fn literals_display_as_they_did() {
    let mut generator = Gen::new(17, 3);
    let mut seen = 0;
    while seen < 5000 {
        let Leaf::Literal(lit) = generator.leaf() else { continue };
        seen += 1;
        let mut pushed = "before".to_owned();
        lit.push_to(&mut pushed);
        assert_eq!(pushed, format!("before{}", old_literal_display(&lit)), "{lit:?}");
        assert_eq!(lit.to_string(), old_literal_display(&lit), "{lit:?}");
    }
}

/// Walking a built tree allocates nothing, building one with a warm builder allocates its block
/// only, writing it allocates nothing when the buffer has room, and reading one allocates its
/// block and the checks' buffers only, however big it is.
#[test]
fn walking_and_the_wire_allocate_the_same_for_any_size_of_tree() {
    let small = lex(0, "fn f() {}");
    // With a literal longer than `NARROW_LIMIT`, so that a wide span is written and read too.
    let text = "fn f(a: u32) -> u32 { (a + 1) * [2][0] }\n".repeat(2000);
    let big = lex(0, &format!("{text} const S: &str = \"{}\";", "x".repeat(0x1_0000)));
    assert!(!big.wide().is_empty());
    for tree in [&small, &big] {
        let view = tree.token_trees();
        view.for_each_token(|_, _| ());
        let walked = blaze_alloc_count::assert_at_most(0, "walking a tree's tokens", || {
            let mut tokens = 0;
            view.for_each_token(|_, _| tokens += 1);
            tokens
        });
        assert!(walked >= view.len());
        blaze_alloc_count::assert_at_most(0, "a tree's token trees", || {
            view.iter_flat_tokens().count()
        });

        // The first build leaves the builder's and the encoder's buffers at this size.
        let delimiter = tree.top_subtree().delimiter;
        let warm = TopSubtree::from_token_trees(delimiter, view);
        let built = blaze_alloc_count::assert_at_most(1, "building a tree with a warm builder", || {
            TopSubtree::from_token_trees(delimiter, view)
        });
        assert!(built == warm && built == *tree);
        assert_eq!(built.block(), tree.block());

        let mut bytes = Vec::new();
        tree.write_wire(&mut bytes);
        let written = bytes.len();
        bytes.clear();
        blaze_alloc_count::assert_at_most(0, "writing a tree into a buffer with room", || {
            tree.write_wire(&mut bytes)
        });
        assert_eq!(bytes.len(), written);

        let parts = wire::parts_of(tree);
        let mut resolve = |part: WirePart| {
            parts
                .iter()
                .find(|(it, _)| *it == part)
                .map(|&(_, it)| it)
                .ok_or_else(|| "a part the tree didn't have".to_owned())
        };
        // The first read grows the nodes' check's stack and the canonical check's set of parts,
        // both kept per thread; a warm read allocates the block alone, and none in a frame.
        drop(TopSubtree::read_wire(&bytes, &mut resolve).expect("a tree's own bytes"));
        let (read, _) = blaze_alloc_count::assert_at_most(1, "reading a tree", || {
            TopSubtree::read_wire(&bytes, &mut resolve)
        })
        .expect("a tree's own bytes");
        assert!(read == *tree && read.keeps_words());
        blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
            drop(TopSubtree::read_wire_in(&bytes, &mut resolve, frame).expect("a tree's own bytes"))
        });
        blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
            let (framed, _) = blaze_alloc_count::assert_at_most(0, "reading a tree in a frame", || {
                TopSubtree::read_wire_in(&bytes, &mut resolve, frame)
            })
            .expect("a tree's own bytes");
            assert_eq!(framed.block(), read.block());
            assert!(framed == read && !framed.keeps_words());
        });
    }
}

/// A tree built in a frame has the block of the same tree built globally, word for word, keeps
/// no words, and costs the frame its block alone: no global allocation, warm.
#[test]
fn a_framed_tree_is_the_global_tree_in_the_frame() {
    let tree = lex(0, "fn f(a: u32) -> u32 { (a + 1) * [2][0] }");
    let delimiter = tree.top_subtree().delimiter;
    let view = tree.token_trees();
    // The thread's first task takes the frame's first chunk, and the first build grows the
    // builder's buffers: both global, once.
    blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
        drop(TopSubtree::from_token_trees_in(delimiter, view, frame))
    });
    blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
        let (framed, used) =
            blaze_alloc_count::measure(|| TopSubtree::from_token_trees_in(delimiter, view, frame));
        assert_eq!(used.allocations, 0, "{used:?}");
        assert_eq!(framed.block(), tree.block());
        assert!(framed == tree && !framed.keeps_words() && tree.keeps_words());
        assert_eq!(framed.copy().block(), tree.block());
    });
}

/// A tree built, written to the wire and read back has the very block it was built with,
/// and its header describes that block.
#[hegel::test]
fn a_tree_read_from_the_wire_has_the_built_block(tc: hegel::TestCase) {
    let seed: u64 = tc.draw(gs::integers());
    let parts: usize = tc.draw(gs::integers().min_value(1).max_value(11));
    let budget: u64 = tc.draw(gs::integers().max_value(79));
    let mut trees = Gen::new(seed, parts);
    let tree = TopSubtree::from_serialized(trees.tree(budget));
    assert_eq!(Layout::of_header(tree.block()), Some(tree.layout));
    let mut bytes = Vec::new();
    tree.write_wire(&mut bytes);
    let parts = wire::parts_of(&tree);
    let mut resolve = |part: WirePart| {
        parts
            .iter()
            .find(|(it, _)| *it == part)
            .map(|&(_, it)| it)
            .ok_or_else(|| "a part the tree didn't have".to_owned())
    };
    let (read, used) =
        TopSubtree::read_wire(&bytes, &mut resolve).expect("a tree's own bytes");
    assert_eq!(used, bytes.len());
    assert_eq!(read.block(), tree.block());
    assert_eq!(read.layout, tree.layout);
    assert_eq!((read.top_open, read.top_close), (tree.top_open, tree.top_close));
}

/// Every kind, with 0, 1 and 255 `#`s for a raw one, comes back from its number.
#[test]
fn a_literal_kind_round_trips_through_its_code() {
    for hashes in [0, 1, 255] {
        for kind in [
            LitKind::Byte,
            LitKind::Char,
            LitKind::Integer,
            LitKind::Float,
            LitKind::Str,
            LitKind::StrRaw(hashes),
            LitKind::ByteStr,
            LitKind::ByteStrRaw(hashes),
            LitKind::CStr,
            LitKind::CStrRaw(hashes),
            LitKind::Err(()),
        ] {
            let (code, kept) = kind.code();
            assert_eq!(LitKind::from_code(code, kept), Some(kind), "{kind:?}");
        }
    }
    for code in 11..=255 {
        for hashes in [0, 1, 255] {
            assert_eq!(LitKind::from_code(code, hashes), None, "code {code}");
        }
    }
}

/// The numbering as `Node::literal` and `Node::lit_kind` wrote it out before they shared
/// [`LitKind::code`]: the reference the shared one is held to.
fn old_code(kind: LitKind) -> (u8, u8) {
    match kind {
        LitKind::Byte => (0, 0),
        LitKind::Char => (1, 0),
        LitKind::Integer => (2, 0),
        LitKind::Float => (3, 0),
        LitKind::Str => (4, 0),
        LitKind::StrRaw(hashes) => (5, hashes),
        LitKind::ByteStr => (6, 0),
        LitKind::ByteStrRaw(hashes) => (7, hashes),
        LitKind::CStr => (8, 0),
        LitKind::CStrRaw(hashes) => (9, hashes),
        LitKind::Err(()) => (10, 0),
    }
}

fn old_lit_kind(code: u8, hashes: u8) -> LitKind {
    match code {
        0 => LitKind::Byte,
        1 => LitKind::Char,
        2 => LitKind::Integer,
        3 => LitKind::Float,
        4 => LitKind::Str,
        5 => LitKind::StrRaw(hashes),
        6 => LitKind::ByteStr,
        7 => LitKind::ByteStrRaw(hashes),
        8 => LitKind::CStr,
        9 => LitKind::CStrRaw(hashes),
        _ => LitKind::Err(()),
    }
}

/// `code`, and a node's literal kind read back, are the old `match`es for any number and
/// count of `#`s.
#[hegel::test]
fn literal_codes_are_the_old_ones(tc: hegel::TestCase) {
    let code: u8 = tc.draw(gs::integers().max_value(15));
    let hashes: u8 = tc.draw(gs::integers());
    let kind = old_lit_kind(code, hashes);
    assert_eq!(kind.code(), old_code(kind));
    let node = Node::literal(kind, 0, 0);
    assert_eq!(node.lit_kind(), kind);
    let head = LITERAL | u32::from(code) << 2 | u32::from(hashes) << 6;
    assert_eq!(Node { head, data: 0 }.lit_kind(), old_lit_kind(code, hashes));
}

/// The scan-then-map index the in-process bridge's `Parts::index` had, before it took
/// [`IndexTable`].
fn old_index(list: &mut Vec<u32>, map: &mut blaze_hash::HashMap<u32, u32>, value: u32) -> u32 {
    if list.len() <= SCAN {
        if let Some(at) = list.iter().position(|it| *it == value) {
            return at as u32;
        }
        let at = list.len() as u32;
        list.push(value);
        if list.len() > SCAN {
            map.extend(list.iter().enumerate().map(|(at, &it)| (it, at as u32)));
        }
        return at;
    }
    let next = list.len() as u32;
    *map.entry(value).or_insert_with(|| {
        list.push(value);
        next
    })
}

#[hegel::test]
fn index_table_numbers_as_the_old_parts_did(tc: hegel::TestCase) {
    let values: Vec<u32> = tc.draw(gs::vecs(gs::integers().max_value(39)).max_size(199));
    let mut table = IndexTable::default();
    let (mut list, mut map) = (Vec::new(), blaze_hash::HashMap::default());
    for &value in &values {
        assert_eq!(table.index(value), old_index(&mut list, &mut map, value));
    }
    assert_eq!(table.list(), &list[..]);
    table.clear();
    assert!(table.list().is_empty());
}

#[hegel::test]
fn aligned_bytes_are_the_bytes_at_a_word(tc: hegel::TestCase) {
    let bytes: Vec<u8> = tc.draw(gs::binary().max_size(63));
    let skip: usize = tc.draw(gs::integers().max_value(3));
    let backing = [&[0u8; 4][..skip], &bytes].concat();
    let given = &backing[skip..];
    let mut scratch = Vec::new();
    let out = aligned(given, &mut scratch);
    assert_eq!(out, given);
    assert!(out.as_ptr().addr().is_multiple_of(4));
}
