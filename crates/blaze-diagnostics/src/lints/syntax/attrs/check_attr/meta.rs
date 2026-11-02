//! The arguments of an attribute as rustc parses them into meta items (`MetaItemInner`,
//! `MetaItem`, `MetaItemKind`), from the tokens of `#[path(..)]`: what `meta_item_list` and the
//! attribute parsers' `MetaItemListParser` see.
//!
//! Follows rustc's `compiler/rustc_parse/src/parser/attr.rs` (`parse_meta_seq_top`,
//! `parse_meta_item_inner`, `parse_meta_item`, `parse_meta_item_kind`) and
//! `compiler/rustc_ast/src/attr/mod.rs` (`MetaItemKind::list_from_tokens`) (MIT or Apache-2.0).

use smallvec::{SmallVec, smallvec};
use syntax::{
    NodeOrToken, SyntaxKind, TextRange,
    view::{self as ast, AstNode},
};

/// A meta-item list: inline up to four items.
pub(super) type List<'t> = SmallVec<[Inner<'t>; 4]>;

/// One element of a meta-item list: rustc's `MetaItemInner`.
#[derive(Clone, Debug)]
pub(super) enum Inner<'t> {
    Item(Item<'t>),
    /// A literal: `8`, `"x"`, `true`.
    Lit(ast::Token<'t>),
}

/// `path`, `path(..)` or `path = lit`: rustc's `MetaItem`.
#[derive(Clone, Debug)]
pub(super) struct Item<'t> {
    /// Its path's segments, without a leading `::`.
    segments: SmallVec<[ast::Token<'t>; 2]>,
    /// Whether its path starts with `::`.
    rooted: bool,
    pub(super) args: ItemArgs<'t>,
    /// From its path to the end of its arguments: rustc's `MetaItem::span`.
    pub(super) range: TextRange,
}

/// rustc's `MetaItemKind`.
#[derive(Clone, Copy, Debug)]
pub(super) enum ItemArgs<'t> {
    Word,
    List(ast::TokenTree<'t>),
    NameValue(ast::Token<'t>),
}

impl<'t> Inner<'t> {
    pub(super) fn item(&self) -> Option<&Item<'t>> {
        match self {
            Inner::Item(item) => Some(item),
            Inner::Lit(_) => None,
        }
    }

    pub(super) fn range(&self) -> TextRange {
        match self {
            Inner::Item(item) => item.range,
            Inner::Lit(lit) => lit.text_range(),
        }
    }
}

impl<'t> Item<'t> {
    /// Its path when it is one plain word: rustc's `word_sym` of it.
    pub(super) fn word(&self) -> Option<&'t str> {
        match self.segments.as_slice() {
            [it] if !self.rooted => Some(it.text()),
            _ => None,
        }
    }

    /// Whether its path is the one word `name` (rustc's `path == sym::name`).
    pub(super) fn is(&self, name: &str) -> bool {
        self.word() == Some(name)
    }

    pub(super) fn is_word(&self) -> bool {
        matches!(self.args, ItemArgs::Word)
    }
}

/// The items of `tt` when it is `(..)` and its tokens are a meta-item list, as rustc's
/// `meta_item_list` gives them; `None` for `[..]`, `{..}` or tokens that are not one.
pub(super) fn list<'t>(tt: ast::TokenTree<'t>) -> Option<List<'t>> {
    let mut elements = tt.syntax().children_with_tokens().filter(|it| !it.kind().is_trivia());
    if elements.next()?.kind() != SyntaxKind::L_PAREN {
        return None;
    }
    let mut cursor = Cursor::new(elements);
    let mut items = List::new();
    while !cursor.done() {
        items.push(cursor.inner()?);
        if cursor.done() {
            break;
        }
        cursor.token(SyntaxKind::COMMA)?;
    }
    // The last element, which the items are read before, is the list's `)`.
    if cursor.ahead[0]?.kind() != SyntaxKind::R_PAREN {
        return None;
    }
    Some(items)
}

/// The elements of a list after its `(`, read with the last one (its `)`) left out.
struct Cursor<'t, I> {
    rest: I,
    /// The next three elements: one more than is ever looked at, to know the last.
    ahead: [Option<ast::Element<'t>>; 3],
}

impl<'t, I: Iterator<Item = ast::Element<'t>>> Cursor<'t, I> {
    fn new(mut rest: I) -> Self {
        let ahead = [rest.next(), rest.next(), rest.next()];
        Cursor { rest, ahead }
    }

    /// The element `n` after the next one, when it is not the last.
    fn get(&self, n: usize) -> Option<ast::Element<'t>> {
        self.ahead[n + 1]?;
        self.ahead[n]
    }

    fn bump(&mut self) {
        self.ahead.rotate_left(1);
        self.ahead[2] = self.rest.next();
    }

    fn done(&self) -> bool {
        self.peek().is_none()
    }

    fn peek(&self) -> Option<ast::Element<'t>> {
        self.get(0)
    }

    fn peek_token(&self) -> Option<ast::Token<'t>> {
        self.peek()?.into_token()
    }

    /// The next token, when it is of `kind`.
    fn token(&mut self, kind: SyntaxKind) -> Option<ast::Token<'t>> {
        let token = self.peek_token().filter(|it| it.kind() == kind)?;
        self.bump();
        Some(token)
    }

    /// `::`, written as one token or as two `:` with nothing between them.
    fn path_sep(&mut self) -> Option<()> {
        let first = self.peek_token()?;
        match first.kind() {
            SyntaxKind::COLON2 => {
                self.bump();
                Some(())
            }
            SyntaxKind::COLON => {
                let second = self.get(1)?.into_token()?;
                if second.kind() != SyntaxKind::COLON || second.text_range().start() != first.text_range().end() {
                    return None;
                }
                self.bump();
                self.bump();
                Some(())
            }
            _ => None,
        }
    }

    fn at_path_sep(&self) -> bool {
        self.peek_token().is_some_and(|it| matches!(it.kind(), SyntaxKind::COLON2 | SyntaxKind::COLON))
    }

    /// rustc's `parse_meta_item_inner`: a literal, else a meta item.
    fn inner(&mut self) -> Option<Inner<'t>> {
        if let Some(lit) = self.peek_token().filter(|it| is_lit(it.kind())) {
            self.bump();
            return Some(Inner::Lit(lit));
        }
        self.item().map(Inner::Item)
    }

    /// rustc's `parse_meta_item`: a path, then `(..)`, `= lit` or nothing.
    fn item(&mut self) -> Option<Item<'t>> {
        let start = self.peek()?.text_range().start();
        let rooted = self.at_path_sep();
        if rooted {
            self.path_sep()?;
        }
        // alloc: inline for up to two segments, as nearly every path in a lint attribute is
        let mut segments: SmallVec<[ast::Token<'t>; 2]> = smallvec![self.segment()?];
        while self.at_path_sep() {
            self.path_sep()?;
            segments.push(self.segment()?);
        }
        let mut end = segments.last()?.text_range().end();
        let args = match self.peek() {
            Some(NodeOrToken::Node(node)) if node.kind() == SyntaxKind::TOKEN_TREE => {
                let tt = ast::TokenTree::cast(node)?;
                tt.l_paren_token()?;
                end = node.text_range().end();
                self.bump();
                ItemArgs::List(tt)
            }
            Some(NodeOrToken::Token(token)) if token.kind() == SyntaxKind::EQ => {
                self.bump();
                let lit = self.peek_token().filter(|it| is_lit(it.kind()))?;
                self.bump();
                end = lit.text_range().end();
                ItemArgs::NameValue(lit)
            }
            _ => ItemArgs::Word,
        };
        Some(Item { segments, rooted, args, range: TextRange::new(start, end) })
    }

    /// One segment of a path: an identifier, or `self`, `super`, `crate`, `Self`.
    fn segment(&mut self) -> Option<ast::Token<'t>> {
        let token = self.peek_token()?;
        match token.kind() {
            SyntaxKind::IDENT
            | SyntaxKind::SELF_KW
            | SyntaxKind::SUPER_KW
            | SyntaxKind::CRATE_KW
            | SyntaxKind::SELF_TYPE_KW => {
                self.bump();
                Some(token)
            }
            _ => None,
        }
    }
}

/// Whether a token is a literal rustc takes as a `MetaItemLit`.
fn is_lit(kind: SyntaxKind) -> bool {
    kind.is_literal() || matches!(kind, SyntaxKind::TRUE_KW | SyntaxKind::FALSE_KW)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str, check: impl FnOnce(Option<List<'_>>)) {
        let source = format!("#[a{text}] fn f() {{}}");
        let parse = syntax::SourceFile::parse(&source, span::Edition::CURRENT);
        let tt = parse.root().descendants().find_map(ast::TokenTree::cast).unwrap();
        check(list(tt));
    }

    /// A list's items are read up to its `)`, `::` written either way, the rest rejected.
    #[test]
    fn a_meta_item_list_is_read_up_to_its_paren() {
        parsed("(a, b::c, ::d, e = \"f\", g(h), 1, x: :y)", |it| assert!(it.is_none()));
        parsed("(a, b::c, ::d, e = \"f\", g(h), 1,)", |it| {
            let it = it.unwrap();
            let words: Vec<Option<&str>> = it.iter().map(|it| it.item().and_then(Item::word)).collect();
            assert_eq!(words, [Some("a"), None, None, Some("e"), Some("g"), None]);
            assert!(matches!(it[3].item().unwrap().args, ItemArgs::NameValue(_)));
            assert!(matches!(it[4].item().unwrap().args, ItemArgs::List(_)));
            assert!(matches!(it[5], Inner::Lit(_)));
        });
        parsed("()", |it| assert!(it.unwrap().is_empty()));
        parsed("(a b)", |it| assert!(it.is_none()));
        parsed("[a]", |it| assert!(it.is_none()));
    }
}
