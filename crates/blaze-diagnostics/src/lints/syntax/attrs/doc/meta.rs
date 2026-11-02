//! rustc's meta-item parser over an attribute's token tree, as `#[doc(..)]`'s parser reads it:
//! literals, paths, `path(..)` lists and `path = literal`. Where rustc's parser gives an error
//! (a missing comma, a non-literal value, a keyword for a name) the list is `None`: rustc gives
//! that attribute no lints.
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/parser.rs` (`MetaItemListParserContext`:
//! `parse_meta_item_inner`, `parse_meta_item`, `should_continue_parsing_meta_items`;
//! `ArgParser::span`, `MetaItemParser::span`, `PathParser::word_sym`) (MIT or Apache-2.0).

use std::borrow::Cow;

use smallvec::SmallVec;
use syntax::{
    NodeOrToken, SyntaxKind, TextRange,
    view::{self as ast, AstNode, AstToken, IsString as _},
};

/// A literal: rustc's `MetaItemLit`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Lit<'t> {
    pub(crate) token: ast::Token<'t>,
}

impl<'t> Lit<'t> {
    fn of(token: ast::Token<'t>) -> Option<Lit<'t>> {
        is_lit(token.kind()).then_some(Lit { token })
    }

    pub(crate) fn range(&self) -> TextRange {
        self.token.text_range()
    }

    /// rustc's `value_as_str`: the value of a string literal, unescaped; `None` for any other
    /// literal and for one that does not unescape.
    pub(crate) fn str(&self) -> Option<Cow<'t, str>> {
        if self.token.kind() != SyntaxKind::STRING || self.suffixed() {
            return None;
        }
        ast::String::cast(self.token)?.value().ok()
    }

    pub(crate) fn is_bool(&self) -> bool {
        matches!(self.token.kind(), SyntaxKind::TRUE_KW | SyntaxKind::FALSE_KW)
    }

    /// rustc's `MetaItemLit::symbol`: the literal as written, without its quotes, prefix and
    /// suffix.
    pub(crate) fn symbol(&self) -> String {
        let text = self.token.text();
        let token = self.token;
        match self.token.kind() {
            // alloc: `symbol` is a diagnostic's text, as is all below
            SyntaxKind::STRING => ast::String::cast(token).map(|it| it.text_without_quotes().to_owned()),
            // alloc: as above
            SyntaxKind::BYTE_STRING => ast::ByteString::cast(token).map(|it| it.text_without_quotes().to_owned()),
            // alloc: as above
            SyntaxKind::C_STRING => ast::CString::cast(token).map(|it| it.text_without_quotes().to_owned()),
            SyntaxKind::CHAR | SyntaxKind::BYTE => {
                // alloc: as above
                let (Some(open), Some(close)) = (text.find('\''), text.rfind('\'')) else { return text.to_owned() };
                text.get(open + 1..close).map(str::to_owned)
            }
            SyntaxKind::INT_NUMBER => ast::IntNumber::cast(token).map(|it| {
                let (prefix, digits, _) = it.split_into_parts();
                // alloc: as above
                format!("{prefix}{digits}")
            }),
            // alloc: as above
            SyntaxKind::FLOAT_NUMBER => ast::FloatNumber::cast(token).map(|it| it.split_into_parts().0.to_owned()),
            _ => None,
        }
        // alloc: as above
        .unwrap_or_else(|| text.to_owned())
    }

    /// Whether it has a suffix (`1u8`, `"a"x`): rustc's `!is_unsuffixed()`.
    pub(crate) fn suffixed(&self) -> bool {
        let text = self.token.text();
        let token = self.token;
        match self.token.kind() {
            SyntaxKind::INT_NUMBER => ast::IntNumber::cast(token).is_some_and(|it| it.suffix().is_some()),
            SyntaxKind::FLOAT_NUMBER => ast::FloatNumber::cast(token).is_some_and(|it| it.suffix().is_some()),
            SyntaxKind::STRING | SyntaxKind::BYTE_STRING | SyntaxKind::C_STRING => {
                text.rfind('"').is_some_and(|close| !text[close + 1..].trim_start_matches('#').is_empty())
            }
            SyntaxKind::CHAR | SyntaxKind::BYTE => text.rfind('\'').is_some_and(|close| close + 1 < text.len()),
            _ => false,
        }
    }
}

/// A meta item's path: rustc's `PathParser`.
#[derive(Clone, Debug)]
pub(crate) struct Path<'t> {
    /// Its segments, as written without `r#`; a leading `::` is rustc's `{{root}}` segment.
    pub(crate) segments: SmallVec<[&'t str; 2]>,
    pub(crate) range: TextRange,
}

impl<'t> Path<'t> {
    /// rustc's `word_sym`: the name of a path of one segment.
    pub(crate) fn word(&self) -> Option<&'t str> {
        match self.segments.as_slice() {
            [it] => Some(*it),
            _ => None,
        }
    }

    /// The path as rustc's messages print it: its segments joined by `::`.
    pub(crate) fn name(&self) -> String {
        // alloc: a diagnostic's text
        self.segments.join("::")
    }
}

/// A meta item's arguments: rustc's `ArgParser`.
#[derive(Clone, Debug)]
pub(crate) enum MetaArgs<'t> {
    None,
    List(List<'t>),
    NameValue { eq: TextRange, value: Lit<'t> },
}

impl MetaArgs<'_> {
    /// rustc's `ArgParser::span`: the list with its parentheses, or `= value`.
    pub(crate) fn range(&self) -> Option<TextRange> {
        match self {
            MetaArgs::None => None,
            MetaArgs::List(list) => Some(list.range),
            MetaArgs::NameValue { eq, value } => Some(TextRange::new(eq.start(), value.range().end())),
        }
    }
}

/// `path`, `path(..)` or `path = literal`: rustc's `MetaItemParser`.
#[derive(Clone, Debug)]
pub(crate) struct MetaItem<'t> {
    pub(crate) path: Path<'t>,
    pub(crate) args: MetaArgs<'t>,
}

impl MetaItem<'_> {
    /// rustc's `MetaItemParser::span`: from its path to the end of its arguments.
    pub(crate) fn range(&self) -> TextRange {
        match self.args.range() {
            Some(args) => TextRange::new(self.path.range.start(), args.end()),
            None => self.path.range,
        }
    }
}

/// An item of a list: rustc's `MetaItemOrLitParser`.
#[derive(Clone, Debug)]
pub(crate) enum Item<'t> {
    Meta(MetaItem<'t>),
    Lit(Lit<'t>),
}

impl Item<'_> {
    pub(crate) fn range(&self) -> TextRange {
        match self {
            Item::Meta(it) => it.range(),
            Item::Lit(it) => it.range(),
        }
    }
}

/// `(..)` and the items in it: rustc's `MetaItemListParser`.
#[derive(Clone, Debug)]
pub(crate) struct List<'t> {
    pub(crate) items: Vec<Item<'t>>,
    /// With its parentheses.
    pub(crate) range: TextRange,
}

/// The list `tree` is, or `None` when it is not one rustc parses: another delimiter than `(`,
/// or anything but meta items and literals separated by commas in it.
pub(crate) fn list<'t>(tree: ast::TokenTree<'t>) -> Option<List<'t>> {
    let mut elements = tree.syntax().children_with_tokens().filter(|it| !it.kind().is_trivia());
    if elements.next()?.kind() != SyntaxKind::L_PAREN {
        return None;
    }
    let mut parser = Parser::new(elements);
    // alloc: once per `#[doc(..)]` list; a `Vec`, as a list's items nest lists
    let mut items = Vec::new();
    while !parser.at_end() {
        items.push(parser.item()?);
        if parser.at_end() {
            break;
        }
        if !parser.eat(SyntaxKind::COMMA) {
            return None;
        }
    }
    // The last element, which the items are read before, is the list's `)`.
    if parser.ahead[0]?.kind() != SyntaxKind::R_PAREN {
        return None;
    }
    Some(List { items, range: tree.syntax().text_range() })
}

/// Whether a token is a literal of a meta item: rustc's `eat_token_lit` (`true` and `false`
/// included).
fn is_lit(kind: SyntaxKind) -> bool {
    kind.is_literal() || matches!(kind, SyntaxKind::TRUE_KW | SyntaxKind::FALSE_KW)
}

/// Whether a token is a segment of a path rustc's `parse_path` takes: an identifier or a path
/// keyword (`self`, `super`, `crate`, `Self`).
fn is_segment(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::IDENT | SyntaxKind::SELF_KW | SyntaxKind::SUPER_KW | SyntaxKind::CRATE_KW | SyntaxKind::SELF_TYPE_KW
    )
}

/// The elements of a list after its `(`, read with the last one (its `)`) left out.
struct Parser<'t, I> {
    rest: I,
    /// The next three elements: one more than is ever looked at, to know the last.
    ahead: [Option<ast::Element<'t>>; 3],
}

impl<'t, I: Iterator<Item = ast::Element<'t>>> Parser<'t, I> {
    fn new(mut rest: I) -> Self {
        let ahead = [rest.next(), rest.next(), rest.next()];
        Parser { rest, ahead }
    }

    /// The element `n` after the next one, when it is not the last.
    fn get(&self, n: usize) -> Option<ast::Element<'t>> {
        self.ahead[n + 1]?;
        self.ahead[n]
    }

    fn bump(&mut self, n: usize) {
        for _ in 0..n {
            self.ahead.rotate_left(1);
            self.ahead[2] = self.rest.next();
        }
    }

    fn at_end(&self) -> bool {
        self.get(0).is_none()
    }

    fn token(&self, n: usize) -> Option<ast::Token<'t>> {
        self.get(n)?.into_token()
    }

    fn eat(&mut self, kind: SyntaxKind) -> bool {
        let at = self.token(0).is_some_and(|it| it.kind() == kind);
        self.bump(usize::from(at));
        at
    }

    /// The range of a `::` at the cursor, written as one token or as two `:` with nothing
    /// between them, and how many tokens it is.
    fn colon2(&self) -> Option<(TextRange, usize)> {
        let first = self.token(0)?;
        match first.kind() {
            SyntaxKind::COLON2 => Some((first.text_range(), 1)),
            SyntaxKind::COLON => {
                let second = self.token(1)?;
                (second.kind() == SyntaxKind::COLON && second.text_range().start() == first.text_range().end())
                    .then(|| (first.text_range().cover(second.text_range()), 2))
            }
            _ => None,
        }
    }

    fn segment(&mut self) -> Option<(&'t str, TextRange)> {
        let token = self.token(0).filter(|it| is_segment(it.kind()))?;
        self.bump(1);
        let text = token.text();
        Some((text.strip_prefix("r#").unwrap_or(text), token.text_range()))
    }

    /// rustc's `parse_path(PathStyle::Mod)`.
    fn path(&mut self) -> Option<Path<'t>> {
        let mut segments = SmallVec::new();
        let start = match self.colon2() {
            Some((range, len)) => {
                self.bump(len);
                segments.push("{{root}}");
                range.start()
            }
            None => self.token(0)?.text_range().start(),
        };
        let (name, mut end) = self.segment()?;
        segments.push(name);
        while let Some((_, len)) = self.colon2() {
            self.bump(len);
            let (name, range) = self.segment()?;
            segments.push(name);
            end = range;
        }
        Some(Path { segments, range: TextRange::new(start, end.end()) })
    }

    /// rustc's `parse_meta_item_inner`.
    fn item(&mut self) -> Option<Item<'t>> {
        if let Some(lit) = self.token(0).and_then(Lit::of) {
            self.bump(1);
            return Some(Item::Lit(lit));
        }
        let path = self.path()?;
        let args = match self.get(0) {
            Some(NodeOrToken::Node(node)) if node.first_token().is_some_and(|it| it.kind() == SyntaxKind::L_PAREN) => {
                let tree = ast::TokenTree::cast(node)?;
                self.bump(1);
                MetaArgs::List(list(tree)?)
            }
            Some(NodeOrToken::Token(token)) if token.kind() == SyntaxKind::EQ => {
                let eq = token.text_range();
                let value = self.token(1).and_then(Lit::of)?;
                self.bump(2);
                MetaArgs::NameValue { eq, value }
            }
            _ => MetaArgs::None,
        };
        Some(Item::Meta(MetaItem { path, args }))
    }
}

#[cfg(test)]
mod tests {
    use syntax::{Edition, SourceFile};

    use super::*;

    fn first_list<R>(text: &str, check: impl FnOnce(Option<List<'_>>) -> R) -> R {
        let parse = SourceFile::parse(text, Edition::CURRENT);
        let tree = parse.root().descendants().find_map(ast::TokenTree::cast).unwrap();
        check(list(tree))
    }

    fn shape(list: &List<'_>) -> Vec<String> {
        list.items
            .iter()
            .map(|it| match it {
                Item::Lit(lit) => format!("lit {}", lit.symbol()),
                Item::Meta(meta) => match &meta.args {
                    MetaArgs::None => meta.path.name(),
                    MetaArgs::List(list) => format!("{}({})", meta.path.name(), shape(list).join(", ")),
                    MetaArgs::NameValue { value, .. } => format!("{} = {}", meta.path.name(), value.symbol()),
                },
            })
            .collect()
    }

    #[test]
    fn a_list_parses_as_rustcs_meta_items() {
        let shapes = first_list(r#"#[doc(foo::bar, crate::bar::baz = "bye", test(attr(allow(x))), 123,)] fn f() {}"#, |it| {
            shape(&it.unwrap())
        });
        assert_eq!(shapes, ["foo::bar", "crate::bar::baz = bye", "test(attr(allow(x)))", "lit 123"]);
        let rooted = first_list("#[doc(::a, r#b::c)] fn f() {}", |it| shape(&it.unwrap()));
        assert_eq!(rooted, ["{{root}}::a", "b::c"]);
    }

    #[test]
    fn spans_are_rustcs() {
        let text = r#"#[doc(hidden = true, inline("x"))] fn f() {}"#;
        first_list(text, |list| {
            let list = list.unwrap();
            let ranges: Vec<&str> = list
                .items
                .iter()
                .map(|it| match it {
                    Item::Meta(meta) => &text[meta.args.range().unwrap()],
                    Item::Lit(_) => unreachable!(),
                })
                .collect();
            assert_eq!(ranges, ["= true", "(\"x\")"]);
            assert_eq!(&text[list.range], r#"(hidden = true, inline("x"))"#);
        });
    }

    #[test]
    fn what_rustc_cannot_parse_is_none() {
        assert!(first_list("#[doc(a b)] fn f() {}", |it| it.is_none()));
        assert!(first_list("#[doc(a = b)] fn f() {}", |it| it.is_none()));
        assert!(first_list("#[doc(-1)] fn f() {}", |it| it.is_none()));
        assert!(first_list("#[doc[a]] fn f() {}", |it| it.is_none()));
        assert!(first_list("#[doc(type)] fn f() {}", |it| it.is_none()));
        assert!(first_list("#[doc(a: :b)] fn f() {}", |it| it.is_none()));
        assert!(first_list("#[doc(a =)] fn f() {}", |it| it.is_none()));
        assert!(first_list("#[doc()] fn f() {}", |it| it.is_some_and(|it| it.items.is_empty())));
    }

    #[test]
    fn a_literal_is_read_as_rustc_reads_it() {
        let values = first_list(r#"#[doc(a = "x\n", b = r"y", c = 1u8, d = true)] fn f() {}"#, |list| {
            list.unwrap()
                .items
                .iter()
                .map(|it| match it {
                    Item::Meta(MetaItem { args: MetaArgs::NameValue { value, .. }, .. }) => {
                        (value.symbol(), value.str().map(Cow::into_owned), value.suffixed())
                    }
                    _ => unreachable!(),
                })
                .collect::<Vec<(String, Option<String>, bool)>>()
        });
        assert_eq!(
            values,
            [
                ("x\\n".to_owned(), Some("x\n".to_owned()), false),
                ("y".to_owned(), Some("y".to_owned()), false),
                ("1".to_owned(), None, true),
                ("true".to_owned(), None, false),
            ]
        );
    }
}
