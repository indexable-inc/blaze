//! The token streams blaze's server holds for a macro's client, as rustc's server holds them
//! (`rustc_ast::tokenstream`): trees shared by reference, each token with the spacing rustc
//! gives it. `Joint` and `JointHidden` say that the next token follows with nothing between;
//! `JointHidden` is what the client sees as not joint, and what `ts_to_string` prints without
//! a space (`print.rs`).
//!
//! A stream is the call's input, made from the database's tree ([`Stream::of_call`]), or what
//! the client built of trees it was given back. The output goes back as the tree the kept layer
//! writes ([`Stream::into_top`]).

use std::rc::Rc;

use proc_macro_api::bridge::wire;
use intern::Symbol;
use span::Span;

/// Whether the next token follows this one with nothing between, as rustc's lexer says it
/// of source (`rustc_ast::tokenstream::Spacing`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Spacing {
    Alone,
    /// A punctuation mark followed by another.
    Joint,
    /// A token followed by one that is not a punctuation mark, or a punctuation mark the
    /// client made not joint.
    JointHidden,
}

/// tt's kinds are the bridge's, numbered in its order (`rpc_encode_decode!(enum Delimiter ..)`),
/// with `Invisible` its `None`.
pub(crate) use tt::DelimiterKind as Delimiter;

/// In the bridge's order (`rpc_encode_decode!(enum LitKind ..)`), each raw kind with its `#`s:
/// the lexer's own.
pub(crate) use parser::cook::LitKind;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct DelimSpan {
    pub(crate) open: Span,
    pub(crate) close: Span,
    pub(crate) entire: Span,
}

/// In the bridge's order (`rpc_encode_decode!(enum IdentKind ..)`): `r#` and `k#` idents.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum IdentKind {
    Normal,
    Raw,
    ForcedKeyword,
}

/// A doc comment's form, `///` and `//!` or `/** */` and `/*! */`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DocKind {
    Line,
    Block,
}

#[derive(Clone, Debug)]
pub(crate) enum Tree {
    /// `open` is the spacing of its opening delimiter, `spacing` of its closing one
    /// (`rustc_ast::tokenstream::DelimSpacing`).
    Group { delimiter: Delimiter, stream: Stream, span: DelimSpan, open: Spacing, spacing: Spacing },
    Punct { ch: u8, spacing: Spacing, span: Span },
    Ident { sym: Symbol, kind: IdentKind, span: Span, spacing: Spacing },
    /// `symbol` is the literal's text without its suffix, as the bridge has it.
    Literal { kind: LitKind, symbol: Symbol, suffix: Option<Symbol>, span: Span, spacing: Spacing },
    /// A doc comment, kept as one token as rustc's streams keep it: printed as written, and
    /// made `#[doc = ".."]` only as the client takes the trees apart (`lex::desugar_doc`).
    DocComment { kind: DocKind, inner: bool, data: Symbol, span: Span, spacing: Spacing },
}

impl Tree {
    pub(crate) fn spacing(&self) -> Spacing {
        match self {
            Tree::Group { spacing, .. }
            | Tree::Punct { spacing, .. }
            | Tree::Ident { spacing, .. }
            | Tree::Literal { spacing, .. }
            | Tree::DocComment { spacing, .. } => *spacing,
        }
    }

    /// The span a token starts at and the one it ends at: a group's are its delimiters'.
    pub(crate) fn ends(&self) -> (Span, Span) {
        match self {
            Tree::Group { span, .. } => (span.open, span.close),
            Tree::Punct { span, .. }
            | Tree::Ident { span, .. }
            | Tree::Literal { span, .. }
            | Tree::DocComment { span, .. } => (*span, *span),
        }
    }
}

/// A token stream: shared, so that the client's `clone` and the trees of a group given back
/// copy nothing. The empty stream holds nothing.
#[derive(Clone, Default, Debug)]
pub(crate) struct Stream(Option<Rc<Vec<Tree>>>);

impl Stream {
    pub(crate) fn of(trees: Vec<Tree>) -> Stream {
        match trees.is_empty() {
            true => Stream(None),
            false => Stream(Some(Rc::new(trees))), // alloc: one shared list per stream the macro builds
        }
    }

    pub(crate) fn trees(&self) -> &[Tree] {
        self.0.as_deref().map_or(&[], Vec::as_slice)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.trees().is_empty()
    }

    /// Its trees to add to, copied only when another handle shares them, as rustc's
    /// `Lrc::make_mut` does.
    pub(crate) fn trees_mut(&mut self) -> &mut Vec<Tree> {
        // alloc: none for a stream given back alone; the edge of a shared one.
        Rc::make_mut(self.0.get_or_insert_with(Default::default))
    }

    /// Its trees, taken from it: copied only when another handle shares them.
    pub(crate) fn into_trees(self) -> Vec<Tree> {
        match self.0 {
            None => Vec::new(), // alloc: none: an empty `Vec`
            Some(trees) => Rc::try_unwrap(trees).unwrap_or_else(|shared| (*shared).clone()), // clone: a shared stream taken apart
        }
    }

    /// The stream a macro is given for `tree`, a call's input or attribute: its trees, when
    /// it has no delimiters (a derive's or an attribute's item, an attribute's arguments),
    /// else its one group. Spacing is the source's, as rustc's lexer gives it: a token
    /// followed at once by the next token of its group has the next one's start where it
    /// ends.
    pub(crate) fn of_call(tree: &tt::TopSubtree) -> Stream {
        let top = tree.top_subtree();
        let mut iter = tree.iter();
        let trees = trees_of(&mut iter);
        if top.delimiter.kind == tt::DelimiterKind::Invisible {
            return Stream::of(trees);
        }
        let delimiter = top.delimiter.kind;
        let span = DelimSpan {
            open: top.delimiter.open,
            close: top.delimiter.close,
            entire: top.delimiter.open.cover_or_self(top.delimiter.close),
        };
        let open = open_spacing(&span, &trees);
        // alloc: the call's input, made once per expansion that runs.
        Stream::of(vec![Tree::Group {
            delimiter,
            stream: Stream::of(trees),
            span,
            open,
            spacing: Spacing::Alone,
        }])
    }

    /// The tree the kept layer reads an output as: the stream's one group, or the stream in a
    /// group without delimiters at `call_site` (`wire::top_subtree`, as the wasm host gives
    /// its output back).
    pub(crate) fn into_top(self, call_site: Span) -> tt::TopSubtree {
        wire::top_subtree(wire_trees(self), call_site)
    }
}

trait CoverOrSelf {
    fn cover_or_self(self, other: Span) -> Span;
}

impl CoverOrSelf for Span {
    /// The span over both, when they are of one anchor and context, as a group's entire span
    /// is; else the first.
    fn cover_or_self(self, other: Span) -> Span {
        match self.anchor == other.anchor && self.ctx == other.ctx {
            true => Span { range: self.range.cover(other.range), ..self },
            false => self,
        }
    }
}

pub(crate) fn lit_kind_of(kind: tt::LitKind) -> LitKind {
    match kind {
        tt::LitKind::Byte => LitKind::Byte,
        tt::LitKind::Char => LitKind::Char,
        tt::LitKind::Integer => LitKind::Integer,
        tt::LitKind::Float => LitKind::Float,
        tt::LitKind::Str => LitKind::Str,
        tt::LitKind::StrRaw(n) => LitKind::StrRaw(n),
        tt::LitKind::ByteStr => LitKind::ByteStr,
        tt::LitKind::ByteStrRaw(n) => LitKind::ByteStrRaw(n),
        tt::LitKind::CStr => LitKind::CStr,
        tt::LitKind::CStrRaw(n) => LitKind::CStrRaw(n),
        tt::LitKind::Err(()) => LitKind::Err,
    }
}

pub(crate) fn tt_lit_kind(kind: LitKind) -> tt::LitKind {
    match kind {
        LitKind::Byte => tt::LitKind::Byte,
        LitKind::Char => tt::LitKind::Char,
        LitKind::Integer => tt::LitKind::Integer,
        LitKind::Float => tt::LitKind::Float,
        LitKind::Str => tt::LitKind::Str,
        LitKind::StrRaw(n) => tt::LitKind::StrRaw(n),
        LitKind::ByteStr => tt::LitKind::ByteStr,
        LitKind::ByteStrRaw(n) => tt::LitKind::ByteStrRaw(n),
        LitKind::CStr => tt::LitKind::CStr,
        LitKind::CStrRaw(n) => tt::LitKind::CStrRaw(n),
        LitKind::Err => tt::LitKind::Err(()),
    }
}

/// The trees of a group, each with its spacing from where the next one starts.
fn trees_of(iter: &mut tt::iter::TtIter<'_>) -> Vec<Tree> {
    // alloc: the call's input, made once per expansion that runs.
    let mut trees: Vec<Tree> = Vec::new();
    for element in iter {
        let tree = match element {
            tt::iter::TtElement::Leaf(leaf) => leaf_tree(leaf),
            tt::iter::TtElement::Subtree(subtree, mut inner) => {
                let open = subtree.delimiter.open;
                let close = subtree.delimiter.close;
                let span = DelimSpan { open, close, entire: open.cover_or_self(close) };
                let inner = trees_of(&mut inner);
                Tree::Group {
                    delimiter: subtree.delimiter.kind,
                    open: open_spacing(&span, &inner),
                    stream: Stream::of(inner),
                    span,
                    spacing: Spacing::Alone,
                }
            }
        };
        if let Some(last) = trees.last_mut() {
            let follows = follows(last.ends().1, tree.ends().0);
            let next_is_punct = matches!(tree, Tree::Punct { .. });
            set_spacing(last, follows, next_is_punct);
        }
        trees.push(tree);
    }
    trees
}

/// The lexer's spacing of a group's opening delimiter: `JointHidden` when its first tree
/// follows at once (a delimiter is no punctuation mark, so never `Joint`), else `Alone`.
fn open_spacing(span: &DelimSpan, inner: &[Tree]) -> Spacing {
    match inner.first() {
        Some(first) if follows(span.open, first.ends().0) => Spacing::JointHidden,
        Some(_) => Spacing::Alone,
        None if follows(span.open, span.close) => Spacing::JointHidden,
        None => Spacing::Alone,
    }
}

/// Whether a token starting at `next` follows one ending at `this` with nothing between.
fn follows(this: Span, next: Span) -> bool {
    this.anchor == next.anchor && this.ctx == next.ctx && this.range.end() == next.range.start()
}

/// rustc's lexer's rule: a punctuation mark followed at once by another is `Joint`; any
/// token followed at once by one that is not is `JointHidden`; else `Alone`. A punctuation
/// mark keeps the spacing the database gave it when that says it is joint.
fn set_spacing(tree: &mut Tree, follows: bool, next_is_punct: bool) {
    let spacing = match (follows, next_is_punct) {
        (false, _) => Spacing::Alone,
        (true, true) if matches!(tree, Tree::Punct { .. }) => Spacing::Joint,
        (true, _) => Spacing::JointHidden,
    };
    match tree {
        Tree::Punct { spacing: Spacing::Joint, .. } => {}
        Tree::Group { spacing: at, .. }
        | Tree::Punct { spacing: at, .. }
        | Tree::Ident { spacing: at, .. }
        | Tree::Literal { spacing: at, .. }
        | Tree::DocComment { spacing: at, .. } => *at = spacing,
    }
}

fn leaf_tree(leaf: tt::Leaf) -> Tree {
    match leaf {
        tt::Leaf::Punct(punct) => Tree::Punct {
            ch: punct.char as u8,
            spacing: match punct.spacing {
                tt::Spacing::Joint => Spacing::Joint,
                tt::Spacing::Alone | tt::Spacing::JointHidden => Spacing::Alone,
            },
            span: punct.span,
        },
        tt::Leaf::Ident(ident) => Tree::Ident {
            sym: ident.sym,
            kind: match ident.is_raw {
                tt::IdentIsRaw::Yes => IdentKind::Raw,
                tt::IdentIsRaw::No => IdentKind::Normal,
            },
            span: ident.span,
            spacing: Spacing::Alone,
        },
        tt::Leaf::Literal(literal) => {
            let (text, suffix) = literal.text_and_suffix();
            Tree::Literal {
                kind: lit_kind_of(literal.kind),
                symbol: Symbol::intern(text),
                suffix: (!suffix.is_empty()).then(|| Symbol::intern(suffix)),
                span: literal.span,
                spacing: Spacing::Alone,
            }
        }
    }
}

/// A stream as the bridge's trees, for the output.
fn wire_trees(stream: Stream) -> Vec<wire::Tree> {
    // alloc: the output, made once per expansion that runs.
    let mut out = Vec::with_capacity(stream.trees().len());
    wire_into(stream.into_trees(), &mut out);
    out
}

fn wire_into(trees: Vec<Tree>, out: &mut Vec<wire::Tree>) {
    for tree in trees {
        out.push(match tree {
            Tree::DocComment { kind, inner, data, span, .. } => {
                // alloc: a doc comment of the output, as the trees the database reads it as.
                let mut desugared = Vec::with_capacity(3);
                crate::lex::desugar_doc(kind, inner, &data, span, &mut desugared);
                wire_into(desugared, out);
                continue;
            }
            Tree::Group { delimiter, stream, span, .. } => wire::Tree::Group {
                kind: delimiter,
                open: span.open,
                close: span.close,
                entire: span.entire,
                trees: wire_trees(stream),
            },
            Tree::Punct { ch, spacing, span } => wire::Tree::Leaf(tt::Leaf::Punct(tt::Punct {
                char: ch as char,
                spacing: match spacing {
                    Spacing::Joint => tt::Spacing::Joint,
                    Spacing::Alone => tt::Spacing::Alone,
                    Spacing::JointHidden => tt::Spacing::JointHidden,
                },
                span,
            })),
            Tree::Ident { sym, kind, span, .. } => wire::Tree::Leaf(tt::Leaf::Ident(tt::Ident {
                sym,
                span,
                is_raw: match kind {
                    IdentKind::Raw => tt::IdentIsRaw::Yes,
                    IdentKind::Normal | IdentKind::ForcedKeyword => tt::IdentIsRaw::No,
                },
            })),
            Tree::Literal { kind, symbol, suffix, span, .. } => {
                let suffix = suffix.as_ref().map_or("", Symbol::as_str);
                wire::Tree::Leaf(tt::Leaf::Literal(tt::Literal::new(
                    symbol.as_str(),
                    span,
                    tt_lit_kind(kind),
                    suffix,
                )))
            }
        });
    }
}

/// A tree a client gave the server, with the spacing rustc's `ToInternal` gives it: a
/// punctuation mark the client made joint is `Joint`, every other token `Alone`, a group
/// `Alone` at both delimiters, and a negative number's literal a `-` that is `JointHidden`
/// before the literal without its sign.
pub(crate) fn push_given(tree: Tree, out: &mut Vec<Tree>) {
    match tree {
        Tree::Literal { kind: kind @ (LitKind::Integer | LitKind::Float), symbol, suffix, span, .. }
            if symbol.as_str().starts_with('-') =>
        {
            out.push(Tree::Punct { ch: b'-', spacing: Spacing::JointHidden, span });
            let symbol = Symbol::intern(&symbol.as_str()[1..]);
            out.push(Tree::Literal { kind, symbol, suffix, span, spacing: Spacing::Alone });
        }
        Tree::Group { delimiter, stream, span, .. } => out.push(Tree::Group {
            delimiter,
            stream,
            span,
            open: Spacing::Alone,
            spacing: Spacing::Alone,
        }),
        Tree::Punct { ch, spacing, span } => out.push(Tree::Punct {
            ch,
            spacing: match spacing {
                Spacing::Joint => Spacing::Joint,
                Spacing::Alone | Spacing::JointHidden => Spacing::Alone,
            },
            span,
        }),
        Tree::Ident { sym, kind, span, .. } => {
            out.push(Tree::Ident { sym, kind, span, spacing: Spacing::Alone })
        }
        Tree::Literal { kind, symbol, suffix, span, .. } => {
            out.push(Tree::Literal { kind, symbol, suffix, span, spacing: Spacing::Alone })
        }
        Tree::DocComment { kind, inner, data, span, .. } => {
            out.push(Tree::DocComment { kind, inner, data, span, spacing: Spacing::Alone })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use span::{EditionedFileId, SpanAnchor, SyntaxContext, TextRange};

    pub(crate) fn at(start: u32, end: u32) -> Span {
        let file_id = EditionedFileId::new(span::FileId::from_raw(7), span::Edition::CURRENT);
        Span {
            range: TextRange::new(start.into(), end.into()),
            anchor: SpanAnchor { file_id, ast_id: span::ROOT_ERASED_FILE_AST_ID },
            ctx: SyntaxContext::root(span::Edition::CURRENT),
        }
    }

    fn ident(text: &str, span: Span) -> tt::Leaf {
        tt::Leaf::Ident(tt::Ident { sym: Symbol::intern(text), span, is_raw: tt::IdentIsRaw::No })
    }

    fn punct(ch: char, span: Span) -> tt::Leaf {
        tt::Leaf::Punct(tt::Punct { char: ch, spacing: tt::Spacing::Alone, span })
    }

    fn input(leaves: Vec<tt::Leaf>) -> tt::TopSubtree {
        let mut builder = tt::TopSubtreeBuilder::new(tt::Delimiter::invisible_spanned(at(0, 0)));
        for leaf in leaves {
            builder.push(leaf);
        }
        builder.build()
    }

    #[test]
    fn spacing_is_the_lexers_of_where_the_next_token_starts() {
        // `a::b c`: `a` touches `:`, the first `:` touches the second, `b` is followed by a
        // space.
        let tree = input(vec![
            ident("a", at(0, 1)),
            punct(':', at(1, 2)),
            punct(':', at(2, 3)),
            ident("b", at(3, 4)),
            ident("c", at(5, 6)),
        ]);
        let stream = Stream::of_call(&tree);
        let spacing: Vec<Spacing> = stream.trees().iter().map(Tree::spacing).collect();
        assert_eq!(
            spacing,
            [
                Spacing::JointHidden,
                Spacing::Joint,
                Spacing::JointHidden,
                Spacing::Alone,
                Spacing::Alone
            ]
        );
    }

    #[test]
    fn a_stream_of_one_group_goes_back_as_that_group() {
        let mut builder = tt::TopSubtreeBuilder::new(tt::Delimiter {
            open: at(0, 1),
            close: at(4, 5),
            kind: tt::DelimiterKind::Brace,
        });
        builder.push(ident("x", at(2, 3)));
        let tree = builder.build();
        let stream = Stream::of_call(&tree);
        assert_eq!(stream.trees().len(), 1);
        let back = stream.into_top(at(9, 9));
        assert_eq!(back.top_subtree().delimiter.kind, tt::DelimiterKind::Brace);
        assert_eq!(back.top_subtree().delimiter.open, at(0, 1));
    }

    #[test]
    fn a_shared_stream_is_copied_only_when_changed() {
        let tree = input(vec![ident("a", at(0, 1))]);
        let stream = Stream::of_call(&tree);
        let mut other = stream.clone();
        other.trees_mut().push(Tree::Punct { ch: b';', spacing: Spacing::Alone, span: at(1, 2) });
        assert_eq!(stream.trees().len(), 1);
        assert_eq!(other.trees().len(), 2);
    }
}
