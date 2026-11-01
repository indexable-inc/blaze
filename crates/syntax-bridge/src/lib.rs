//! Conversions between syntax trees, read through their views (`syntax::view`), and
//! [`tt::TokenTree`]s.

#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
#![forbid(unsafe_code)]
#![feature(allocator_api, allocator_ext)]

#[cfg(feature = "in-rust-tree")]
extern crate rustc_driver as _;

use std::{
    alloc::{Allocator, Global},
    cell::Cell,
    collections::VecDeque,
    fmt,
};

use intern::Symbol;
use smallvec::SmallVec;
use span::{Edition, Span, SpanAnchor, SpanMap, SyntaxContext};
use stdx::never;
use syntax::{
    FlatTree, Parse,
    SyntaxKind::{self, *},
    SyntaxTreeBuilder, T, TextRange, TextSize, WalkEvent, format_smolstr,
    view::Untyped,
};
use tt::{Punct, buffer::Cursor, token_to_literal};

/// The leaves a conversion's callback inserts at one event: at most three, inline.
pub type Inserted = SmallVec<[tt::Leaf; 3]>;
/// The leaves to append after an element (a view's `NodeId`), by element.
pub type Appended<K, A = Global> = hashbrown::HashMap<K, Inserted, blaze_hash::FixedState, A>;
/// The elements to leave out.
pub type Removed<K, A = Global> = hashbrown::HashSet<K, blaze_hash::FixedState, A>;

pub mod prettify_macro_expansion;
#[cfg(any(test, feature = "reference"))]
pub mod reference;
mod to_parser_input;
pub use to_parser_input::{
    to_matcher_input, to_matcher_input_into, to_parser_input, to_parser_input_into, tree_words,
    view_tokens,
};
// FIXME: we probably should re-think  `token_tree_to_syntax_node` interfaces
pub use ::parser::TopEntryPoint;

#[cfg(test)]
mod tests;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

pub trait SpanMapper {
    fn span_for(&self, range: TextRange) -> Span;

    /// The macro fragments that start and that end with the token at `range`, see
    /// [`MacroFragments`].
    fn fragment_bounds(&self, _range: TextRange) -> FragmentBounds {
        FragmentBounds::default()
    }
}

/// How many macro fragments a token starts and how many it ends.
#[derive(Default, Clone, Copy)]
pub struct FragmentBounds {
    pub starting: usize,
    pub ending: usize,
}

/// The spans of a macro expansion for the arguments of a `macro_rules` macro called in it. A
/// fragment that the expansion substituted is one token tree to that macro: an `$e:expr` that was
/// `0` does not match a `0` in a rule, and a `$t:ty` that was `a::B` is one `$t:tt`.
pub struct MacroFragments<'a>(pub &'a SpanMap);

impl SpanMapper for MacroFragments<'_> {
    fn span_for(&self, range: TextRange) -> Span {
        self.0.span_for(range)
    }

    fn fragment_bounds(&self, range: TextRange) -> FragmentBounds {
        FragmentBounds {
            starting: self.0.fragments_starting_at(range.start()),
            ending: self.0.fragments_ending_at(range.end()),
        }
    }
}

impl SpanMapper for SpanMap {
    fn span_for(&self, range: TextRange) -> Span {
        self.span_at(range.start())
    }
}

impl<SM: SpanMapper> SpanMapper for &SM {
    fn span_for(&self, range: TextRange) -> Span {
        SM::span_for(self, range)
    }

    fn fragment_bounds(&self, range: TextRange) -> FragmentBounds {
        SM::fragment_bounds(self, range)
    }
}

/// Dummy things for testing where spans don't matter.
pub mod dummy_test_span_utils {

    use span::{Span, SyntaxContext};

    use super::*;

    pub const DUMMY: Span = Span {
        range: TextRange::empty(TextSize::new(0)),
        anchor: span::SpanAnchor {
            file_id: span::EditionedFileId::new(
                span::FileId::from_raw(0xe4e4e),
                span::Edition::CURRENT,
            ),
            ast_id: span::ROOT_ERASED_FILE_AST_ID,
        },
        ctx: SyntaxContext::root(Edition::CURRENT),
    };

    pub struct DummyTestSpanMap;

    impl SpanMapper for DummyTestSpanMap {
        fn span_for(&self, range: syntax::TextRange) -> Span {
            Span {
                range,
                anchor: span::SpanAnchor {
                    file_id: span::EditionedFileId::new(
                        span::FileId::from_raw(0xe4e4e),
                        span::Edition::CURRENT,
                    ),
                    ast_id: span::ROOT_ERASED_FILE_AST_ID,
                },
                ctx: SyntaxContext::root(Edition::CURRENT),
            }
        }
    }
}

/// Doc comment desugaring differs between mbe and proc-macros.
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum DocCommentDesugarMode {
    /// Desugars doc comments as quoted raw strings
    Mbe,
    /// Desugars doc comments as quoted strings
    ProcMacro,
}

/// A macro's input, or any node's tokens as a token tree, from a view of the node
/// (`syntax::view`, docs/v2/plan/lanes/frontend/tree.md): one pass over the flat tree's tokens
/// in the node's range, through the parts of the lazy nodes in it (an attribute macro's `fn`
/// whose body an outline left unparsed), with each span from `map`, the file's side table, read
/// in order. No syntax handle is made and nothing is counted.
///
/// The tree's parser words ([`tree_words`]) are made the first time a matcher or the parser
/// reads it, not here: a proc macro's input goes over the wire and never needs them.
pub fn call_input<SpanMap>(
    node: syntax::view::Node<'_>,
    map: SpanMap,
    call_site: Span,
    mode: DocCommentDesugarMode,
) -> tt::TopSubtree
where
    SpanMap: SpanMapper,
{
    let (tree, at) = node.flat();
    let mut c = FlatConverter::new(tree, at, map, call_site, mode);
    let len = c.tokens.len_hint();
    convert_tokens_kept(&mut c, len)
}

/// [`call_input`] into a tree in `alloc`: a task's temporary, which keeps no parser words.
pub fn call_input_in<SpanMap, A: Allocator>(
    node: syntax::view::Node<'_>,
    map: SpanMap,
    call_site: Span,
    mode: DocCommentDesugarMode,
    alloc: A,
) -> tt::TopSubtree<A>
where
    SpanMap: SpanMapper,
{
    let (tree, at) = node.flat();
    let mut c = FlatConverter::new(tree, at, map, call_site, mode);
    let len = c.tokens.len_hint();
    convert_tokens(&mut c, len, alloc)
}

/// [`call_input`] with leaves inserted and elements left out (cfg stripping and fixup of a
/// macro's input): elements are removed and appended after by their [`syntax::view::NodeId`], which is
/// unique within the file's tree `node` is in, and `on_event` sees the view's walk. No syntax
/// handle is made and nothing is counted.
pub fn call_input_modified<'t, SpanMap, OnEvent, A: Allocator + Clone>(
    node: syntax::view::Node<'t>,
    map: SpanMap,
    append: Appended<syntax::view::NodeId, A>,
    remove: Removed<syntax::view::NodeId, A>,
    call_site: Span,
    mode: DocCommentDesugarMode,
    on_event: OnEvent,
) -> tt::TopSubtree
where
    SpanMap: SpanMapper,
    OnEvent: FnMut(
        &mut syntax::view::PreorderWithTokens<'t>,
        &WalkEvent<syntax::view::Element<'t>>,
    ) -> (bool, Inserted),
{
    let mut c = ViewConverter::new(node, map, append, remove, call_site, mode, on_event);
    convert_tokens_kept(&mut c, 0)
}

// The following items are what `rustc` macro can be parsed into :
// link: https://github.com/rust-lang/rust/blob/9ebf47851a357faa4cd97f4b1dc7835f6376e639/src/libsyntax/ext/expand.rs#L141
// * Expr(Box<ast::Expr>)                     -> token_tree_to_expr
// * Pat(Box<ast::Pat>)                       -> token_tree_to_pat
// * Ty(Box<ast::Ty>)                         -> token_tree_to_ty
// * Stmts(SmallVec<[ast::Stmt; 1]>)        -> token_tree_to_stmts
// * Items(SmallVec<[Box<ast::Item>; 1]>)     -> token_tree_to_items
//
// * TraitItems(SmallVec<[ast::TraitItem; 1]>)
// * AssocItems(SmallVec<[ast::AssocItem; 1]>)
// * ForeignItems(SmallVec<[ast::ForeignItem; 1]>

/// Converts a [`tt::Subtree`] back to a syntax tree, an untyped parse read through its views
/// ([`Parse::root`]).
/// The produced `SpanMap` contains a mapping from the syntax nodes offsets to the subtree's spans.
///
/// The parser reads the words the tree keeps ([`view_tokens`]; made once, the first time it is
/// parsed), where they are, and the tree is built straight from its events and the tokens:
/// nothing is converted or parsed twice and no text is lexed. The input, whose words are the
/// thread's (`parser::Input`), is only filled when the words must be made afresh for other
/// editions.
pub fn token_tree_to_syntax_node<F: FnMut(SyntaxContext) -> Edition + ?Sized>(
    tt: tt::SubtreeView<'_>,
    entry_point: parser::TopEntryPoint,
    span_to_edition: &mut F,
) -> (Parse<Untyped>, SpanMap) {
    token_tree_to_syntax_node_in(tt, entry_point, span_to_edition, Global)
}

/// [`token_tree_to_syntax_node`] of a view, its span map in `alloc`.
pub fn token_tree_to_syntax_node_in<
    F: FnMut(SyntaxContext) -> Edition + ?Sized,
    A: Allocator + Clone,
>(
    tt: tt::SubtreeView<'_>,
    entry_point: parser::TopEntryPoint,
    span_to_edition: &mut F,
    alloc: A,
) -> (Parse<Untyped>, SpanMap<A>) {
    let buffer = tt.strip_invisible();
    let mut copy = parser::Input::with_capacity(0);
    let tokens = view_tokens(buffer, false, span_to_edition, &mut copy);
    parse_tokens(buffer, entry_point, tokens, alloc)
}

/// The tree of the tokens of `buffer`, `tokens` being their input, its span map in `alloc`.
pub(crate) fn parse_tokens<A: Allocator + Clone>(
    buffer: tt::TokenTreesView<'_>,
    entry_point: parser::TopEntryPoint,
    tokens: parser::Tokens<'_>,
    alloc: A,
) -> (Parse<Untyped>, SpanMap<A>) {
    // It matters what edition we parse with even when we escape all identifiers correctly.
    let parser_output = entry_point.parse(tokens);
    let mut tree_sink = TtTreeSink::new(buffer.cursor(), tokens.len());
    for event in parser_output.iter() {
        match event {
            parser::Step::Token { kind, n_input_tokens: n_raw_tokens } => {
                tree_sink.token(kind, n_raw_tokens)
            }
            parser::Step::FloatSplit { ends_in_dot: has_pseudo_dot } => {
                tree_sink.float_split(has_pseudo_dot)
            }
            parser::Step::Enter { kind } => tree_sink.start_node(kind),
            parser::Step::Exit => tree_sink.finish_node(),
            // alloc: an error's message.
            parser::Step::Error { msg } => tree_sink.error(msg.to_owned()),
        }
    }
    tree_sink.finish(alloc)
}

/// Convert a string to a `TokenTree`. The spans of the subtree will be anchored to the provided
/// anchor with the given context.
pub fn parse_to_token_tree(
    edition: Edition,
    anchor: SpanAnchor,
    ctx: SyntaxContext,
    text: &str,
) -> Option<tt::TopSubtree> {
    let lexed = parser::LexedStr::new_hashed(edition, text);
    if lexed.errors().next().is_some() {
        return None;
    }
    let len = lexed.len();
    let tokens = LexedTokens::new(lexed, text);
    let mut conv = RawConverter { tokens, anchor, ctx, mode: DocCommentDesugarMode::ProcMacro };
    Some(convert_tokens_kept(&mut conv, len))
}

/// Convert a string to a `TokenTree`. The passed span will be used for all spans of the produced subtree.
pub fn parse_to_token_tree_static_span(
    edition: Edition,
    span: Span,
    text: &str,
) -> Option<tt::TopSubtree> {
    let lexed = parser::LexedStr::new_hashed(edition, text);
    if lexed.errors().next().is_some() {
        return None;
    }
    let len = lexed.len();
    let tokens = LexedTokens::new(lexed, text);
    let mut conv = StaticRawConverter { tokens, span, mode: DocCommentDesugarMode::ProcMacro };
    Some(convert_tokens_kept(&mut conv, len))
}

/// Converts the tokens of `conv`, `len` being about how many token trees they make, into a tree
/// that keeps its parser words.
fn convert_tokens_kept<C>(conv: &mut C, len: usize) -> tt::TopSubtree
where
    C: TokenConverter,
    C::Token: fmt::Debug,
{
    convert_into_builder(conv, len).build_skip_top_subtree()
}

/// [`convert_tokens_kept`] into a tree in `alloc`, a task's temporary that keeps no words.
fn convert_tokens<C, A: Allocator>(conv: &mut C, len: usize, alloc: A) -> tt::TopSubtree<A>
where
    C: TokenConverter,
    C::Token: fmt::Debug,
{
    convert_into_builder(conv, len).build_skip_top_subtree_in(alloc)
}

/// The builder holding the tokens of `conv`, every group closed.
fn convert_into_builder<C>(conv: &mut C, len: usize) -> tt::TopSubtreeBuilder
where
    C: TokenConverter,
    C::Token: fmt::Debug,
{
    let mut builder = tt::TopSubtreeBuilder::with_capacity(
        tt::Delimiter::invisible_spanned(conv.call_site()),
        len,
    );

    // The fragments that the last token ended, with its span.
    let mut ended = (0, conv.call_site());
    while let Some((token, abs_range)) = conv.bump() {
        close_fragments(&mut builder, ended);
        let fragments = conv.fragment_bounds(abs_range);
        if fragments.starting > 0 || fragments.ending > 0 {
            let span = conv.span_for(abs_range);
            for _ in 0..fragments.starting {
                builder.open(tt::DelimiterKind::Invisible, span);
            }
            ended = (fragments.ending, span);
        } else {
            ended.0 = 0;
        }
        let tt = match token.as_leaf() {
            // These delimiters are not actually valid punctuation, but we produce them in syntax fixup.
            // So we need to handle them specially here.
            Some(&tt::Leaf::Punct(Punct {
                char: char @ ('(' | ')' | '{' | '}' | '[' | ']'),
                span,
                spacing: _,
            })) => {
                let found_expected_delimiter =
                    builder.expected_delimiters().enumerate().find(|(_, delim)| match delim {
                        tt::DelimiterKind::Parenthesis => char == ')',
                        tt::DelimiterKind::Brace => char == '}',
                        tt::DelimiterKind::Bracket => char == ']',
                        tt::DelimiterKind::Invisible => false,
                    });
                if let Some((idx, _)) = found_expected_delimiter {
                    for _ in 0..=idx {
                        builder.close(span);
                    }
                    continue;
                }

                let delim = match char {
                    '(' => tt::DelimiterKind::Parenthesis,
                    '{' => tt::DelimiterKind::Brace,
                    '[' => tt::DelimiterKind::Bracket,
                    _ => panic!("unmatched closing delimiter from syntax fixup"),
                };

                // Start a new subtree
                builder.open(delim, span);
                continue;
            }
            // clone: a leaf is a symbol, a span and a few bits, with nothing to allocate.
            Some(leaf) => leaf.clone(),
            None => match token.kind(conv) {
                // Desugar doc comments into doc attributes
                kind @ (INNER_DOC_COMMENT | OUTER_DOC_COMMENT) => {
                    let span = conv.span_for(abs_range);
                    conv.convert_doc_comment(&token, kind == INNER_DOC_COMMENT, span, &mut builder);
                    continue;
                }
                kind if kind.is_punct() && kind != UNDERSCORE => {
                    let found_expected_delimiter =
                        builder.expected_delimiters().enumerate().find(|(_, delim)| match delim {
                            tt::DelimiterKind::Parenthesis => kind == T![')'],
                            tt::DelimiterKind::Brace => kind == T!['}'],
                            tt::DelimiterKind::Bracket => kind == T![']'],
                            tt::DelimiterKind::Invisible => false,
                        });

                    // Current token is a closing delimiter that we expect, fix up the closing span
                    // and end the subtree here.
                    // We also close any open inner subtrees that might be missing their delimiter.
                    if let Some((idx, _)) = found_expected_delimiter {
                        for _ in 0..=idx {
                            // FIXME: record an error somewhere if we're closing more than one tree here?
                            builder.close(conv.span_for(abs_range));
                        }
                        continue;
                    }

                    let delim = match kind {
                        T!['('] => Some(tt::DelimiterKind::Parenthesis),
                        T!['{'] => Some(tt::DelimiterKind::Brace),
                        T!['['] => Some(tt::DelimiterKind::Bracket),
                        _ => None,
                    };

                    // Start a new subtree
                    if let Some(kind) = delim {
                        builder.open(kind, conv.span_for(abs_range));
                        continue;
                    }

                    let spacing = match conv.peek().map(|next| next.kind(conv)) {
                        Some(kind) if is_single_token_op(kind) => tt::Spacing::Joint,
                        _ => tt::Spacing::Alone,
                    };
                    let Some(char) = token.to_char(conv) else {
                        panic!("Token from lexer must be single char: token = {token:#?}")
                    };
                    // FIXME: this might still be an unmatched closing delimiter? Maybe we should assert here
                    tt::Leaf::from(tt::Punct { char, spacing, span: conv.span_for(abs_range) })
                }
                kind => {
                    macro_rules! make_ident {
                        () => {
                            tt::Ident {
                                span: conv.span_for(abs_range),
                                sym: intern_name(token.to_text(conv), token.name_hash(conv)),
                                is_raw: tt::IdentIsRaw::No,
                            }
                            .into()
                        };
                    }
                    let leaf: tt::Leaf = match kind {
                        k if k.is_any_identifier() => {
                            let (is_raw, name) = tt::IdentIsRaw::split_from_symbol(token.to_text(conv));
                            let sym = intern_name(name, token.name_hash(conv));
                            tt::Ident { sym, span: conv.span_for(abs_range), is_raw }.into()
                        }
                        UNDERSCORE => make_ident!(),
                        k if k.is_literal() => {
                            // The compiler reads a source file with `\n` line endings.
                            let text = token.to_text(conv);
                            let span = conv.span_for(abs_range);
                            match text.contains('\r') {
                                true => token_to_literal(&text.replace("\r\n", "\n"), span),
                                false => token_to_literal(text, span),
                            }
                            .into()
                        }
                        LIFETIME_IDENT => {
                            let apostrophe = tt::Leaf::from(tt::Punct {
                                char: '\'',
                                spacing: tt::Spacing::Joint,
                                span: conv
                                    .span_for(TextRange::at(abs_range.start(), TextSize::of('\''))),
                            });
                            builder.push(apostrophe);

                            let ident = tt::Leaf::from(tt::Ident {
                                sym: Symbol::intern(&token.to_text(conv)[1..]),
                                span: conv.span_for(TextRange::new(
                                    abs_range.start() + TextSize::of('\''),
                                    abs_range.end(),
                                )),
                                is_raw: tt::IdentIsRaw::No,
                            });
                            builder.push(ident);
                            continue;
                        }
                        _ => continue,
                    };

                    leaf
                }
            },
        };

        builder.push(tt);
    }
    close_fragments(&mut builder, ended);

    while builder.expected_delimiters().next().is_some() {
        // FIXME: record an error somewhere?
        builder.close(conv.call_site());
    }
    builder
}

/// Closes the `count` fragments that the token at `span` ended, of those that started in these
/// tokens: a fragment may be more than the arguments of a macro in it.
fn close_fragments(builder: &mut tt::TopSubtreeBuilder, (count, span): (usize, Span)) {
    for _ in 0..count {
        if builder.expected_delimiters().next() != Some(tt::DelimiterKind::Invisible) {
            break;
        }
        builder.close(span);
    }
}

/// The char at byte `offset` of a punctuation token, whose text is ASCII.
fn punct_char(text: &str, offset: usize) -> Option<char> {
    text.get(offset..)?.chars().next()
}

fn is_single_token_op(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        EQ | L_ANGLE
            | R_ANGLE
            | BANG
            | AMP
            | PIPE
            | TILDE
            | AT
            | DOT
            | COMMA
            | SEMICOLON
            | COLON
            | POUND
            | DOLLAR
            | QUESTION
            | PLUS
            | MINUS
            | STAR
            | SLASH
            | PERCENT
            | CARET
            // LIFETIME_IDENT will be split into a sequence of `'` (a single quote) and an
            // identifier.
            | LIFETIME_IDENT
    )
}

/// Returns the textual content of a doc comment block as a quoted string
/// That is, strips leading `///` (or `/**`, etc)
/// and strips the ending `*/`
/// And then quote the string, which is needed to convert to `tt::Literal`
///
/// Note that proc-macros desugar with string literals where as macro_rules macros desugar with raw string literals.
pub fn desugar_doc_comment_text(text: &str, mode: DocCommentDesugarMode) -> (Symbol, tt::LitKind) {
    match mode {
        DocCommentDesugarMode::Mbe => {
            let mut num_of_hashes = 0;
            let mut count = 0;
            for ch in text.chars() {
                count = match ch {
                    '"' => 1,
                    '#' if count > 0 => count + 1,
                    _ => 0,
                };
                num_of_hashes = num_of_hashes.max(count);
            }

            // Quote raw string with delimiters
            (Symbol::intern(text), tt::LitKind::StrRaw(num_of_hashes))
        }
        // Quote string with delimiters
        DocCommentDesugarMode::ProcMacro => {
            (Symbol::intern(&format_smolstr!("{}", text.escape_debug())), tt::LitKind::Str)
        }
    }
}

/// Desugars the doc comment `text` into the attribute it stands for.
fn convert_doc_comment(
    text: &str,
    is_inner: bool,
    span: Span,
    mode: DocCommentDesugarMode,
    builder: &mut tt::TopSubtreeBuilder,
) {
    let mk_ident = |s: &str| {
        tt::Leaf::from(tt::Ident { sym: Symbol::intern(s), span, is_raw: tt::IdentIsRaw::No })
    };

    let mk_punct =
        |c: char| tt::Leaf::from(tt::Punct { char: c, spacing: tt::Spacing::Alone, span });

    let mk_doc_literal = |text: &str| {
        let from_end = if text.starts_with("/*") && text.ends_with("*/") { 2 } else { 0 };
        let text = &text[3..text.len() - from_end];

        let (text, kind) = desugar_doc_comment_text(text, mode);
        let lit = tt::Literal { text_and_suffix: text, span, kind, suffix_len: 0 };

        tt::Leaf::from(lit)
    };

    // Make `doc="\" Comments\""
    let meta_tkns = [mk_ident("doc"), mk_punct('='), mk_doc_literal(text)];

    // Make `#![]`
    builder.push(mk_punct('#'));
    if is_inner {
        builder.push(mk_punct('!'));
    }
    builder.open(tt::DelimiterKind::Bracket, span);
    builder.extend(meta_tkns);
    builder.close(span);
}

/// A raw token (straight from lexer) converter
struct RawConverter<'a> {
    tokens: LexedTokens<'a>,
    anchor: SpanAnchor,
    ctx: SyntaxContext,
    mode: DocCommentDesugarMode,
}
/// A raw token (straight from lexer) converter that gives every token the same span.
struct StaticRawConverter<'a> {
    tokens: LexedTokens<'a>,
    span: Span,
    mode: DocCommentDesugarMode,
}

trait SrcToken<Ctx> {
    fn kind(&self, ctx: &Ctx) -> SyntaxKind;

    fn to_char(&self, ctx: &Ctx) -> Option<char>;

    fn to_text<'a>(&'a self, ctx: &'a Ctx) -> &'a str;

    fn as_leaf(&self) -> Option<&tt::Leaf> {
        None
    }

    /// The interner's hash of an identifier's name (its text without `r#`), when the lexer took
    /// it ([`parser::LexedStr::new_hashed`]).
    fn name_hash(&self, _ctx: &Ctx) -> Option<u64> {
        None
    }
}

/// `name` interned: by the hash the lexer took of it when there is one, so it is not hashed
/// again.
fn intern_name(name: &str, hash: Option<u64>) -> Symbol {
    match hash {
        Some(hash) => Symbol::intern_hashed(name, hash),
        None => Symbol::intern(name),
    }
}

trait TokenConverter: Sized {
    type Token: SrcToken<Self>;

    fn convert_doc_comment(
        &self,
        token: &Self::Token,
        is_inner: bool,
        span: Span,
        builder: &mut tt::TopSubtreeBuilder,
    );

    fn bump(&mut self) -> Option<(Self::Token, TextRange)>;

    fn peek(&self) -> Option<Self::Token>;

    fn span_for(&self, range: TextRange) -> Span;

    fn fragment_bounds(&self, _range: TextRange) -> FragmentBounds {
        FragmentBounds::default()
    }

    fn call_site(&self) -> Span;
}

/// A token of a lexed text: its kind, its text, and for a name the hash the lexer took of it.
#[derive(Debug, Clone, Copy)]
struct RawToken<'t> {
    kind: SyntaxKind,
    text: &'t str,
    name_hash: Option<u64>,
}

impl<C> SrcToken<C> for RawToken<'_> {
    fn kind(&self, _: &C) -> SyntaxKind {
        self.kind
    }

    fn to_char(&self, _: &C) -> Option<char> {
        self.text.chars().next()
    }

    fn to_text<'a>(&'a self, _: &'a C) -> &'a str {
        self.text
    }

    fn name_hash(&self, _: &C) -> Option<u64> {
        self.name_hash
    }
}

/// The tokens of a lexed text in order, read with a running offset: each token's start and
/// text are one add and one slice, not a walk from the lexer's last checkpoint; and its name's
/// hash the next of [`parser::LexedStr::name_hashes`], `name` counting them.
struct LexedTokens<'t> {
    lexed: parser::LexedStr<'t>,
    text: &'t str,
    pos: usize,
    offset: usize,
    name: usize,
}

impl<'t> LexedTokens<'t> {
    /// The tokens of `lexed`, the [`parser::LexedStr::new_hashed`] lexing of `text`.
    fn new(lexed: parser::LexedStr<'t>, text: &'t str) -> Self {
        debug_assert_eq!(lexed.as_str(), text);
        debug_assert_eq!(
            lexed.name_hashes().len(),
            (0..lexed.len()).filter(|&i| parser::LexedStr::has_name_hash(lexed.kind(i))).count(),
            "lexed by `LexedStr::new_hashed`"
        );
        LexedTokens { lexed, text, pos: 0, offset: 0, name: 0 }
    }

    fn peek(&self) -> Option<RawToken<'t>> {
        if self.pos == self.lexed.len() {
            return None;
        }
        let len = self.lexed.text_len(self.pos);
        let text = &self.text[self.offset..self.offset + len];
        let kind = self.lexed.kind(self.pos);
        let name_hash = parser::LexedStr::has_name_hash(kind).then(|| self.lexed.name_hashes()[self.name]);
        Some(RawToken { kind, text, name_hash })
    }

    fn bump(&mut self) -> Option<(RawToken<'t>, TextRange)> {
        let token = self.peek()?;
        let start = self.offset;
        self.pos += 1;
        self.name += usize::from(token.name_hash.is_some());
        self.offset += token.text.len();
        let range = TextRange::new(
            TextSize::new(u32::try_from(start).ok()?),
            TextSize::new(u32::try_from(self.offset).ok()?),
        );
        Some((token, range))
    }
}

impl<'t> TokenConverter for RawConverter<'t> {
    type Token = RawToken<'t>;

    fn convert_doc_comment(
        &self,
        token: &RawToken<'t>,
        is_inner: bool,
        span: Span,
        builder: &mut tt::TopSubtreeBuilder,
    ) {
        convert_doc_comment(token.text, is_inner, span, self.mode, builder);
    }

    fn bump(&mut self) -> Option<(Self::Token, TextRange)> {
        self.tokens.bump()
    }

    fn peek(&self) -> Option<Self::Token> {
        self.tokens.peek()
    }

    fn span_for(&self, range: TextRange) -> Span {
        Span { range, anchor: self.anchor, ctx: self.ctx }
    }

    fn call_site(&self) -> Span {
        Span { range: TextRange::empty(0.into()), anchor: self.anchor, ctx: self.ctx }
    }
}

impl<'t> TokenConverter for StaticRawConverter<'t> {
    type Token = RawToken<'t>;

    fn convert_doc_comment(
        &self,
        token: &RawToken<'t>,
        is_inner: bool,
        span: Span,
        builder: &mut tt::TopSubtreeBuilder,
    ) {
        convert_doc_comment(token.text, is_inner, span, self.mode, builder);
    }

    fn bump(&mut self) -> Option<(Self::Token, TextRange)> {
        self.tokens.bump()
    }

    fn peek(&self) -> Option<Self::Token> {
        self.tokens.peek()
    }

    fn span_for(&self, _: TextRange) -> Span {
        self.span
    }

    fn call_site(&self) -> Span {
        self.span
    }
}

/// The ends of the dot and of the last of the pieces a float the parser split as a field name
/// (`x.0.1`, `x.0. await`) would be, for an `INT_NUMBER` at `int` and the tokens `next` gives
/// after it: the `.` right after it and the `INT_NUMBER` right after that, if any.
fn float_pieces(
    int: TextRange,
    mut next: impl Iterator<Item = (SyntaxKind, TextRange)>,
) -> Option<[TextSize; 2]> {
    let (_, dot) =
        next.next().filter(|&(kind, range)| kind == T![.] && range.start() == int.end())?;
    let end = match next.next() {
        Some((INT_NUMBER, frac)) if frac.start() == dot.end() => frac.end(),
        _ => dot.end(),
    };
    Some([dot.end(), end])
}

/// Whether a token of `kind` at `range`, after `prev`, may be a later piece of a float the
/// parser split: a `.` or an `INT_NUMBER` right after a token of the other kind.
fn may_continue_float(
    kind: SyntaxKind,
    range: TextRange,
    prev: Option<(SyntaxKind, TextRange)>,
) -> bool {
    prev.is_some_and(|(prev, at)| {
        at.end() == range.start()
            && matches!((prev, kind), (INT_NUMBER, T![.]) | (T![.], INT_NUMBER))
    })
}

/// The float literal starting at `int`, an `INT_NUMBER` that starts a token of the source's
/// lexing, when the parser split it into the pieces ending at `ends` ([`float_pieces`]):
/// `window` is the source from `int` to the char after the pieces, and the lexer's first token
/// there is a float ending at a piece's end. rustc's tokens, which a macro sees, hold the float.
/// A number lexes alike in every edition, and the lexer reads one char past its end at most.
fn split_float(int: TextRange, ends: [TextSize; 2], window: &str) -> Option<TextRange> {
    let lexed = parser::LexedStr::new(Edition::CURRENT, window);
    if lexed.is_empty() || lexed.kind(0) != FLOAT_NUMBER {
        return None;
    }
    let end = int.start() + TextSize::try_from(lexed.text_len(0)).ok()?;
    ends.contains(&end).then(|| TextRange::new(int.start(), end))
}

/// The end of the text a window for [`split_float`] ending at `end` takes: `end` and the char
/// after it.
fn window_end(source: &str, end: TextSize) -> usize {
    let end = usize::from(end);
    end + source[end..].chars().next().map_or(0, char::len_utf8)
}

/// A token of a flat tree, with the source its range indexes.
#[derive(Debug, Clone, Copy)]
struct FlatTok<'t> {
    kind: SyntaxKind,
    range: TextRange,
    text: &'t str,
    source: &'t str,
}

/// The tokens of a node of a flat tree in order, through the parts of the lazy nodes in it
/// (parsing them) and the part of the node itself if it is lazy.
#[derive(Clone)]
struct FlatTokens<'t> {
    /// The trees being walked, the innermost last: each with its next element, the element its
    /// walk ends before, and where the next element starts.
    stack: SmallVec<[(&'t FlatTree, u32, u32, u32); 4]>,
}

impl<'t> FlatTokens<'t> {
    fn new(tree: &'t FlatTree, at: u32) -> FlatTokens<'t> {
        let mut tokens = FlatTokens { stack: SmallVec::new() };
        tokens.enter(tree, at);
        tokens
    }

    /// How many elements the walk has left in its outermost tree: its tokens, its nodes and its
    /// lazy nodes, not what those hold.
    fn len_hint(&self) -> usize {
        self.stack.first().map_or(0, |&(_, next, end, _)| (end - next) as usize)
    }

    fn enter(&mut self, tree: &'t FlatTree, at: u32) {
        let (tree, at): (&'t FlatTree, u32) = match tree.is_lazy(at) {
            true => (&**tree.part(at), 0),
            false => (tree, at),
        };
        let start = u32::from(tree.range(at).start());
        self.stack.push((tree, at, tree.end(at), start));
    }
}

impl<'t> Iterator for FlatTokens<'t> {
    type Item = FlatTok<'t>;

    fn next(&mut self) -> Option<FlatTok<'t>> {
        loop {
            let (tree, next, end, start) = self.stack.last_mut()?;
            if *next >= *end {
                self.stack.pop();
                continue;
            }
            let (tree, at) = (*tree, *next);
            let len = tree.text_len(at);
            let range = TextRange::at(TextSize::from(*start), TextSize::from(len));
            *next += 1;
            *start += len;
            if tree.is_token(at) {
                let kind = SyntaxKind::from(tree.kind(at).0);
                debug_assert_eq!(range, tree.range(at));
                let source = tree.text();
                return Some(FlatTok { kind, range, text: &source[range], source });
            }
            if tree.is_lazy(at) {
                self.enter(tree, at);
            }
        }
    }
}

/// Converts a node of a flat tree, reading its tokens from the tree's arrays.
struct FlatConverter<'t, SpanMap> {
    tokens: FlatTokens<'t>,
    current: Option<FlatTok<'t>>,
    /// The float in the node that the parser split and `current` is the first piece of.
    float: Option<TextRange>,
    /// Whether `current` ends a token of the source's lexing.
    ends_token: bool,
    range: TextRange,
    punct_offset: Option<(FlatTok<'t>, TextSize)>,
    /// Used to make the emitted text ranges in the spans relative to the span anchor.
    map: SpanMap,
    call_site: Span,
    mode: DocCommentDesugarMode,
}

impl<'t, SpanMap> FlatConverter<'t, SpanMap> {
    fn new(
        tree: &'t FlatTree,
        at: u32,
        map: SpanMap,
        call_site: Span,
        mode: DocCommentDesugarMode,
    ) -> Self {
        let mut tokens = FlatTokens::new(tree, at);
        let current = tokens.next();
        let range = tree.range(at);
        let mut converter = FlatConverter {
            tokens,
            current,
            float: None,
            ends_token: false,
            range,
            punct_offset: None,
            map,
            call_site,
            mode,
        };
        converter.settle(Some(range.start()));
        converter
    }

    /// Reads `current` against the source's lexing, `boundary` being where the token before it
    /// ended when that ended a token of the lexing: the walk has no gaps, so `current` starts
    /// one exactly when it starts there.
    fn settle(&mut self, boundary: Option<TextSize>) {
        self.float = None;
        self.ends_token = false;
        let Some(token) = self.current else { return };
        if boundary != Some(token.range.start()) {
            return;
        }
        self.ends_token = true;
        if token.kind != INT_NUMBER
            || token.source.as_bytes().get(usize::from(token.range.end())) != Some(&b'.')
        {
            return;
        }
        let next = self.tokens.clone().map(|it| (it.kind, it.range));
        let Some(ends) = float_pieces(token.range, next) else { return };
        let window =
            &token.source[usize::from(token.range.start())..window_end(token.source, ends[1])];
        match split_float(token.range, ends, window) {
            Some(float) if self.range.contains_range(float) => self.float = Some(float),
            Some(_) => self.ends_token = false,
            None => {}
        }
    }

    /// The float `current` is the first piece of, as one token.
    fn float_token(&self, token: FlatTok<'t>, float: TextRange) -> FlatTok<'t> {
        let source = token.source;
        FlatTok { kind: FLOAT_NUMBER, range: float, text: &source[float], source }
    }
}

#[derive(Debug)]
enum FlatToken<'t> {
    Ordinary(FlatTok<'t>),
    Punct { token: FlatTok<'t>, offset: usize },
}

impl<'t> FlatToken<'t> {
    fn token(&self) -> &FlatTok<'t> {
        match self {
            FlatToken::Ordinary(it) | FlatToken::Punct { token: it, offset: _ } => it,
        }
    }
}

impl<'t, SpanMap> SrcToken<FlatConverter<'t, SpanMap>> for FlatToken<'t> {
    fn kind(&self, _ctx: &FlatConverter<'t, SpanMap>) -> SyntaxKind {
        match self {
            FlatToken::Ordinary(token) => token.kind,
            FlatToken::Punct { token, offset: i } => {
                SyntaxKind::from_char(punct_char(token.text, *i).unwrap()).unwrap()
            }
        }
    }
    fn to_char(&self, _ctx: &FlatConverter<'t, SpanMap>) -> Option<char> {
        match self {
            FlatToken::Ordinary(_) => None,
            FlatToken::Punct { token, offset: i } => punct_char(token.text, *i),
        }
    }
    fn to_text<'a>(&'a self, _ctx: &'a FlatConverter<'t, SpanMap>) -> &'a str {
        self.token().text
    }
}

impl<'t, SpanMap> TokenConverter for FlatConverter<'t, SpanMap>
where
    SpanMap: SpanMapper,
{
    type Token = FlatToken<'t>;
    fn convert_doc_comment(
        &self,
        token: &Self::Token,
        is_inner: bool,
        span: Span,
        builder: &mut tt::TopSubtreeBuilder,
    ) {
        convert_doc_comment(token.token().text, is_inner, span, self.mode, builder);
    }

    fn bump(&mut self) -> Option<(Self::Token, TextRange)> {
        if let Some((punct, offset)) = self.punct_offset
            && usize::from(offset) + 1 < punct.text.len()
        {
            let offset = offset + TextSize::of('.');
            self.punct_offset = Some((punct, offset));
            let range = TextRange::at(punct.range.start() + offset, TextSize::of('.'));
            return Some((FlatToken::Punct { token: punct, offset: usize::from(offset) }, range));
        }

        let curr = self.current?;
        if let Some(float) = self.float {
            while self.current.is_some_and(|it| it.range.start() < float.end()) {
                self.current = self.tokens.next();
            }
            self.settle(Some(float.end()));
            self.punct_offset = None;
            return Some((FlatToken::Ordinary(self.float_token(curr, float)), float));
        }
        self.current = self.tokens.next();
        self.settle(self.ends_token.then_some(curr.range.end()));
        let token = if curr.kind.is_punct() {
            self.punct_offset = Some((curr, 0.into()));
            let range = TextRange::at(curr.range.start(), TextSize::of('.'));
            (FlatToken::Punct { token: curr, offset: 0_usize }, range)
        } else {
            self.punct_offset = None;
            (FlatToken::Ordinary(curr), curr.range)
        };
        Some(token)
    }

    fn peek(&self) -> Option<Self::Token> {
        if let Some((punct, offset)) = self.punct_offset {
            let offset = offset + TextSize::of('.');
            if usize::from(offset) < punct.text.len() {
                return Some(FlatToken::Punct { token: punct, offset: usize::from(offset) });
            }
        }

        let curr = self.current?;
        if let Some(float) = self.float {
            return Some(FlatToken::Ordinary(self.float_token(curr, float)));
        }
        let token = if curr.kind.is_punct() {
            FlatToken::Punct { token: curr, offset: 0_usize }
        } else {
            FlatToken::Ordinary(curr)
        };
        Some(token)
    }

    fn span_for(&self, range: TextRange) -> Span {
        self.map.span_for(range)
    }

    fn fragment_bounds(&self, range: TextRange) -> FragmentBounds {
        self.map.fragment_bounds(range)
    }
    fn call_site(&self) -> Span {
        self.call_site
    }
}

/// A node's walk as a view, with removals and appends keyed by element id.
/// The leaves a modified conversion's callback and appends gave, waiting for their place in
/// its output: as many as the events between two tokens give, which a run of closing nodes
/// makes as many as the tree is deep. Kept per thread at the most it ever held
/// (`blaze_mem::scratch`), so a warm conversion allocates nothing for them.
#[derive(Default)]
struct PendingLeaves(VecDeque<tt::Leaf>);

thread_local! {
    static PENDING_LEAVES: Cell<VecDeque<tt::Leaf>> = const { Cell::new(VecDeque::new()) }; // alloc: none, empty
}

impl blaze_mem::scratch::Shelved for PendingLeaves {
    fn empty(&mut self) {
        self.0.clear();
    }

    fn held(&self) -> usize {
        self.0.capacity()
    }

    fn unshelve() -> Option<Self> {
        let leaves = PENDING_LEAVES.try_with(Cell::take).ok()?;
        (leaves.capacity() != 0).then_some(PendingLeaves(leaves))
    }

    fn shelve(self) {
        // A thread that is ending: the buffer is freed.
        let _ = PENDING_LEAVES.try_with(|shelf| {
            let kept = shelf.take();
            shelf.set(if kept.capacity() >= self.0.capacity() { kept } else { self.0 });
        });
    }
}

struct ViewConverter<'t, SpanMap, OnEvent, A: Allocator + Clone> {
    current: Option<syntax::view::Token<'t>>,
    /// The float in the node that the parser split and `current` is the first piece of.
    float: Option<TextRange>,
    /// Whether `current` ends a token of the source's lexing.
    ends_token: bool,
    current_leaves: blaze_mem::scratch::Lent<PendingLeaves>,
    preorder: syntax::view::PreorderWithTokens<'t>,
    range: TextRange,
    punct_offset: Option<(syntax::view::Token<'t>, TextSize)>,
    map: SpanMap,
    append: Appended<syntax::view::NodeId, A>,
    remove: Removed<syntax::view::NodeId, A>,
    call_site: Span,
    mode: DocCommentDesugarMode,
    on_event: OnEvent,
}

impl<'t, SpanMap, OnEvent, A: Allocator + Clone> ViewConverter<'t, SpanMap, OnEvent, A>
where
    OnEvent: FnMut(
        &mut syntax::view::PreorderWithTokens<'t>,
        &WalkEvent<syntax::view::Element<'t>>,
    ) -> (bool, Inserted),
{
    fn new(
        node: syntax::view::Node<'t>,
        map: SpanMap,
        append: Appended<syntax::view::NodeId, A>,
        remove: Removed<syntax::view::NodeId, A>,
        call_site: Span,
        mode: DocCommentDesugarMode,
        on_event: OnEvent,
    ) -> Self {
        let mut converter = ViewConverter {
            current: None,
            float: None,
            ends_token: false,
            preorder: node.preorder_with_tokens(),
            range: node.text_range(),
            punct_offset: None,
            map,
            append,
            remove,
            call_site,
            current_leaves: blaze_mem::scratch::lend(),
            mode,
            on_event,
        };
        converter.current = converter.next_token();
        converter.settle(Some(converter.range.start()));
        converter
    }

    /// Reads `current` against the source's lexing, `boundary` being where the token before it
    /// ended when that ended a token of the lexing. After a gap in the walk `current` starts a
    /// token of the lexing unless it may be a later piece of a split float: a `.` or an
    /// `INT_NUMBER` right after a token of the other kind.
    fn settle(&mut self, boundary: Option<TextSize>) {
        self.float = None;
        self.ends_token = false;
        let Some(token) = self.current else { return };
        let range = token.text_range();
        let prev = || token.prev_token().map(|it| (it.kind(), it.text_range()));
        if boundary != Some(range.start()) && may_continue_float(token.kind(), range, prev()) {
            return;
        }
        self.ends_token = true;
        let source = token.flat().0.text();
        if token.kind() != INT_NUMBER
            || source.as_bytes().get(usize::from(range.end())) != Some(&b'.')
        {
            return;
        }
        let next = std::iter::successors(token.next_token(), |it| it.next_token())
            .map(|it| (it.kind(), it.text_range()));
        let Some(ends) = float_pieces(range, next) else { return };
        let window = &source[usize::from(range.start())..window_end(source, ends[1])];
        match split_float(range, ends, window) {
            Some(float) if self.range.contains_range(float) => self.float = Some(float),
            Some(_) => self.ends_token = false,
            None => {}
        }
    }

    fn next_token(&mut self) -> Option<syntax::view::Token<'t>> {
        while let Some(ev) = self.preorder.next() {
            let (keep_event, insert_leaves) = (self.on_event)(&mut self.preorder, &ev);
            self.current_leaves.0.extend(insert_leaves);
            if !keep_event {
                continue;
            }
            match ev {
                WalkEvent::Enter(element) => {
                    if self.remove.contains(&element.id()) {
                        if let syntax::NodeOrToken::Node(node) = element {
                            self.preorder.skip_subtree();
                            if let Some(v) = self.append.remove(&node.id()) {
                                self.current_leaves.0.extend(v);
                            }
                        }
                        continue;
                    }
                    if let syntax::NodeOrToken::Token(token) = element {
                        return Some(token);
                    }
                }
                WalkEvent::Leave(element) => {
                    if let Some(v) = self.append.remove(&element.id()) {
                        self.current_leaves.0.extend(v);
                    }
                }
            }
        }
        None
    }
}

#[derive(Debug)]
enum ViewToken<'t> {
    Ordinary(syntax::view::Token<'t>),
    Punct { token: syntax::view::Token<'t>, offset: usize },
    Float(&'t str),
    Leaf(tt::Leaf),
}

impl<'t> ViewToken<'t> {
    fn token(&self) -> syntax::view::Token<'t> {
        match self {
            ViewToken::Ordinary(it) | ViewToken::Punct { token: it, offset: _ } => *it,
            ViewToken::Float(_) | ViewToken::Leaf(_) => unreachable!(),
        }
    }
}

impl<'t, SpanMap, OnEvent, A: Allocator + Clone> SrcToken<ViewConverter<'t, SpanMap, OnEvent, A>>
    for ViewToken<'t>
{
    fn kind(&self, _ctx: &ViewConverter<'t, SpanMap, OnEvent, A>) -> SyntaxKind {
        match self {
            ViewToken::Ordinary(token) => token.kind(),
            ViewToken::Punct { token, offset: i } => {
                SyntaxKind::from_char(punct_char(token.text(), *i).unwrap()).unwrap()
            }
            ViewToken::Float(_) => FLOAT_NUMBER,
            ViewToken::Leaf(_) => {
                never!();
                SyntaxKind::ERROR
            }
        }
    }
    fn to_char(&self, _ctx: &ViewConverter<'t, SpanMap, OnEvent, A>) -> Option<char> {
        match self {
            ViewToken::Ordinary(_) | ViewToken::Float(_) | ViewToken::Leaf(_) => None,
            ViewToken::Punct { token, offset: i } => punct_char(token.text(), *i),
        }
    }
    fn to_text<'a>(&'a self, _ctx: &'a ViewConverter<'t, SpanMap, OnEvent, A>) -> &'a str {
        match self {
            ViewToken::Ordinary(token) | ViewToken::Punct { token, offset: _ } => token.text(),
            ViewToken::Float(text) => text,
            ViewToken::Leaf(_) => {
                never!();
                ""
            }
        }
    }
    fn as_leaf(&self) -> Option<&tt::Leaf> {
        match self {
            ViewToken::Ordinary(_) | ViewToken::Punct { .. } | ViewToken::Float(_) => None,
            ViewToken::Leaf(it) => Some(it),
        }
    }
}

impl<'t, SpanMap, OnEvent, A: Allocator + Clone> TokenConverter
    for ViewConverter<'t, SpanMap, OnEvent, A>
where
    SpanMap: SpanMapper,
    OnEvent: FnMut(
        &mut syntax::view::PreorderWithTokens<'t>,
        &WalkEvent<syntax::view::Element<'t>>,
    ) -> (bool, Inserted),
{
    type Token = ViewToken<'t>;
    fn convert_doc_comment(
        &self,
        token: &Self::Token,
        is_inner: bool,
        span: Span,
        builder: &mut tt::TopSubtreeBuilder,
    ) {
        convert_doc_comment(token.token().text(), is_inner, span, self.mode, builder);
    }

    fn bump(&mut self) -> Option<(Self::Token, TextRange)> {
        if let Some((punct, offset)) = &mut self.punct_offset
            && usize::from(*offset) + 1 < punct.text().len()
        {
            *offset += TextSize::of('.');
            let range = TextRange::at(punct.text_range().start() + *offset, TextSize::of('.'));
            let offset = u32::from(*offset) as usize;
            return Some((ViewToken::Punct { token: *punct, offset }, range));
        }

        if let Some(leaf) = self.current_leaves.0.pop_front() {
            return Some((ViewToken::Leaf(leaf), TextRange::empty(TextSize::new(0))));
        }

        let curr = self.current.take()?;
        if !self.range.contains_range(curr.text_range()) {
            self.current = Some(curr);
            return None;
        }

        let range = curr.text_range();
        if let Some(float) = self.float {
            self.current = self.next_token();
            while self.current.is_some_and(|it| it.text_range().start() < float.end()) {
                self.current = self.next_token();
            }
            self.settle(Some(float.end()));
            self.punct_offset = None;
            return Some((ViewToken::Float(&curr.flat().0.text()[float]), float));
        }
        self.current = self.next_token();
        self.settle(self.ends_token.then_some(range.end()));
        let token = if curr.kind().is_punct() {
            self.punct_offset = Some((curr, 0.into()));
            let range = TextRange::at(range.start(), TextSize::of('.'));
            (ViewToken::Punct { token: curr, offset: 0 }, range)
        } else {
            self.punct_offset = None;
            (ViewToken::Ordinary(curr), range)
        };
        Some(token)
    }

    fn peek(&self) -> Option<Self::Token> {
        if let Some((punct, offset)) = self.punct_offset {
            let offset = offset + TextSize::of('.');
            if usize::from(offset) < punct.text().len() {
                return Some(ViewToken::Punct { token: punct, offset: usize::from(offset) });
            }
        }

        let curr = self.current?;
        if !self.range.contains_range(curr.text_range()) {
            return None;
        }
        if let Some(float) = self.float {
            return Some(ViewToken::Float(&curr.flat().0.text()[float]));
        }
        Some(match curr.kind().is_punct() {
            true => ViewToken::Punct { token: curr, offset: 0 },
            false => ViewToken::Ordinary(curr),
        })
    }

    fn span_for(&self, range: TextRange) -> Span {
        self.map.span_for(range)
    }

    fn fragment_bounds(&self, range: TextRange) -> FragmentBounds {
        self.map.fragment_bounds(range)
    }
    fn call_site(&self) -> Span {
        self.call_site
    }
}

struct TtTreeSink<'a> {
    buf: blaze_mem::scratch::Lent<String>,
    cursor: Cursor<'a>,
    text_pos: TextSize,
    inner: SyntaxTreeBuilder,
    token_map: SpanMap,
    /// Where the token trees without delimiters start that the cursor is in, each with its
    /// group's span: an invisible delimiter has no text to keep it in the map.
    open_fragments: SmallVec<[(TextSize, Span); 4]>,
    /// Where the last token ends, before the whitespace that may follow it.
    token_end: TextSize,
}

impl<'a> TtTreeSink<'a> {
    /// A sink for the parse of the `tokens` input tokens `cursor` walks, filling the worker's map.
    fn new(cursor: Cursor<'a>, tokens: usize) -> Self {
        let token_map = match SINK_MAP.take() {
            Some(mut map) => {
                map.clear();
                map.reserve(tokens);
                map
            }
            None => SpanMap::with_capacity(tokens),
        };
        TtTreeSink {
            buf: blaze_mem::scratch::lend_string(),
            cursor,
            text_pos: 0.into(),
            inner: SyntaxTreeBuilder::default(),
            token_map,
            open_fragments: SmallVec::new(),
            token_end: 0.into(),
        }
    }

    /// Records the token tree without delimiters that the cursor leaves as a macro fragment.
    fn close_fragment(&mut self) {
        if let Some((start, span)) = self.open_fragments.pop()
            && start < self.token_end
        {
            self.token_map.push_fragment(TextRange::new(start, self.token_end), span);
        }
    }

    /// The tree, and the map copied out of the worker's into `alloc` at its length; the worker
    /// keeps its map for the next parse unless it grew past [`KEEP_MAP`].
    fn finish<A: Allocator + Clone>(mut self, alloc: A) -> (Parse<Untyped>, SpanMap<A>) {
        while !self.open_fragments.is_empty() {
            self.close_fragment();
        }
        self.token_map.finish();
        let map = self.token_map.copy_in(alloc);
        if self.token_map.capacity() <= KEEP_MAP {
            SINK_MAP.set(Some(self.token_map));
        }
        (self.inner.finish_view(), map)
    }
}

thread_local! {
    /// The map a parse of a token tree fills, kept by the worker at the size it grew to.
    pub(crate) static SINK_MAP: Cell<Option<SpanMap>> = const { Cell::new(None) };
}

/// The most pieces whose map the worker keeps for the next parse.
const KEEP_MAP: usize = 1 << 16;

fn delim_to_str(d: tt::DelimiterKind, closing: bool) -> Option<&'static str> {
    let texts = match d {
        tt::DelimiterKind::Parenthesis => "()",
        tt::DelimiterKind::Brace => "{}",
        tt::DelimiterKind::Bracket => "[]",
        tt::DelimiterKind::Invisible => return None,
    };

    let idx = closing as usize;
    Some(&texts[idx..texts.len() - (1 - idx)])
}

impl TtTreeSink<'_> {
    /// Parses a float literal as if it was a one to two name ref nodes with a dot inbetween.
    /// This occurs when a float literal is used as a field access.
    fn float_split(&mut self, has_pseudo_dot: bool) {
        let token_tree = self.cursor.token_tree();
        let (text, span) = match &token_tree {
            Some(tt::TokenTree::Leaf(tt::Leaf::Literal(
                lit @ tt::Literal { span, kind: tt::LitKind::Float, .. },
            ))) => (lit.text(), *span),
            tt => unreachable!("{tt:?}"),
        };
        // rustc's `break_up_float`: each piece takes its own part of the span when the span is
        // the literal's text (rustc compares the span's source text; a span here has none, so
        // its length), otherwise the whole span.
        let apart = span.range.len() == TextSize::of(text);
        let piece = |from: TextSize, to: TextSize| match apart {
            true => {
                let start = span.range.start();
                Span { range: TextRange::new(start + from, start + to), ..span }
            }
            false => span,
        };
        match text.split_once('.') {
            Some((left, right)) => {
                assert!(!left.is_empty());
                let dot = TextSize::of(left);

                self.inner.start_node(SyntaxKind::NAME_REF);
                self.inner.token(SyntaxKind::INT_NUMBER, left);
                self.inner.finish_node();
                self.token_map.push(self.text_pos + dot, piece(0.into(), dot));

                // here we move the exit up, the original exit has been deleted in process
                self.inner.finish_node();

                self.inner.token(SyntaxKind::DOT, ".");
                let after_dot = dot + TextSize::of(".");
                self.token_map.push(self.text_pos + after_dot, piece(dot, after_dot));

                if has_pseudo_dot {
                    assert!(right.is_empty(), "{left}.{right}");
                } else {
                    assert!(!right.is_empty(), "{left}.{right}");
                    self.inner.start_node(SyntaxKind::NAME_REF);
                    self.inner.token(SyntaxKind::INT_NUMBER, right);
                    let end = TextSize::of(text);
                    self.token_map.push(self.text_pos + end, piece(after_dot, end));
                    self.inner.finish_node();

                    // the parser creates an unbalanced start node, we are required to close it here
                    self.inner.finish_node();
                }
                self.text_pos += TextSize::of(text);
            }
            None => {
                // alloc: an error's message.
        self.error("illegal float literal".to_owned());
                self.inner.start_node(SyntaxKind::ERROR);
                self.inner.token(SyntaxKind::FLOAT_NUMBER, text);
                self.token_map.push(self.text_pos + TextSize::of(text), span);
                self.inner.finish_node();
                self.inner.finish_node();

                if !has_pseudo_dot {
                    self.inner.finish_node();
                }

                self.text_pos += TextSize::of(text);
            }
        }
        self.cursor.bump();
    }

    fn token(&mut self, kind: SyntaxKind, mut n_tokens: u8) {
        if kind == LIFETIME_IDENT {
            n_tokens = 2;
        }

        let mut last_two = self.cursor.peek_two_leaves();
        let mut combined_span = None;
        'tokens: for _ in 0..n_tokens {
            let tmp: u8;
            if self.cursor.eof() {
                break;
            }
            last_two = self.cursor.peek_two_leaves();
            let (text, span) = loop {
                break match self.cursor.token_tree() {
                    Some(tt::TokenTree::Leaf(leaf)) => match leaf {
                        tt::Leaf::Ident(ident) => {
                            if ident.is_raw.yes() {
                                self.buf.push_str("r#");
                                self.text_pos += TextSize::of("r#");
                            }
                            let text = ident.sym.as_str();
                            self.buf.push_str(text);
                            self.text_pos += TextSize::of(text);
                            combined_span = match combined_span {
                                None => Some(ident.span),
                                Some(prev_span) => Some(Self::merge_spans(prev_span, ident.span)),
                            };
                            self.cursor.bump();
                            continue 'tokens;
                        }
                        tt::Leaf::Punct(punct) => {
                            assert!(punct.char.is_ascii());
                            tmp = punct.char as u8;
                            let r = (
                                std::str::from_utf8(std::slice::from_ref(&tmp)).unwrap(),
                                punct.span,
                            );
                            self.cursor.bump();
                            r
                        }
                        tt::Leaf::Literal(lit) => {
                            let buf_l = self.buf.len();
                            lit.push_to(&mut self.buf);
                            debug_assert_ne!(self.buf.len() - buf_l, 0);
                            self.text_pos += TextSize::new((self.buf.len() - buf_l) as u32);
                            combined_span = match combined_span {
                                None => Some(lit.span),
                                Some(prev_span) => Some(Self::merge_spans(prev_span, lit.span)),
                            };
                            self.cursor.bump();
                            continue 'tokens;
                        }
                    },
                    Some(tt::TokenTree::Subtree(subtree)) => {
                        self.cursor.bump();
                        match delim_to_str(subtree.delimiter.kind, false) {
                            Some(it) => (it, subtree.delimiter.open),
                            None => {
                                self.open_fragments.push((self.text_pos, subtree.delimiter.open));
                                continue;
                            }
                        }
                    }
                    None => {
                        let parent = self.cursor.end();
                        match delim_to_str(parent.delimiter.kind, true) {
                            Some(it) => (it, parent.delimiter.close),
                            None => {
                                self.close_fragment();
                                continue;
                            }
                        }
                    }
                };
            };
            self.buf.push_str(text);
            self.text_pos += TextSize::of(text);
            combined_span = match combined_span {
                None => Some(span),
                Some(prev_span) => Some(Self::merge_spans(prev_span, span)),
            }
        }

        self.token_map.push(self.text_pos, combined_span.expect("expected at least one token"));
        self.token_end = self.text_pos;
        self.inner.token(kind, self.buf.as_str());
        self.buf.clear();
        // FIXME: Emitting whitespace for this is really just a hack, we should get rid of it.
        // Add whitespace between adjoint puncts
        if let Some([tt::Leaf::Punct(curr), tt::Leaf::Punct(next)]) = last_two {
            // Note: We always assume the semi-colon would be the last token in
            // other parts of RA such that we don't add whitespace here.
            //
            // When `next` is a `Punct` of `'`, that's a part of a lifetime identifier so we don't
            // need to add whitespace either.
            if curr.spacing == tt::Spacing::Alone && curr.char != ';' && next.char != '\'' {
                self.inner.token(WHITESPACE, " ");
                self.text_pos += TextSize::of(' ');
                self.token_map.push(self.text_pos, curr.span);
            }
        }
    }

    fn start_node(&mut self, kind: SyntaxKind) {
        self.inner.start_node(kind);
    }

    fn finish_node(&mut self) {
        self.inner.finish_node();
    }

    fn error(&mut self, error: String) {
        self.inner.error(error, self.text_pos)
    }

    fn merge_spans(a: Span, b: Span) -> Span {
        // We don't do what rustc does exactly, rustc does something clever when the spans have different syntax contexts
        // but this runs afoul of our separation between `span` and `hir-expand`.
        Span {
            range: if a.ctx == b.ctx && a.anchor == b.anchor {
                TextRange::new(
                    std::cmp::min(a.range.start(), b.range.start()),
                    std::cmp::max(a.range.end(), b.range.end()),
                )
            } else {
                // Combining ranges make no sense when they come from different syntax contexts.
                a.range
            },
            anchor: a.anchor,
            ctx: a.ctx,
        }
    }
}
