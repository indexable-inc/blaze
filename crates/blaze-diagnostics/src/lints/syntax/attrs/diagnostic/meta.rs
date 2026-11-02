//! The arguments of a diagnostic attribute as rustc's attribute parser reads them: a list of
//! meta items and literals, parsed without recovery, any error making it the empty list.
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/parser.rs` (`ArgParser::from_attr_args`,
//! `MetaItemListParserContext`, `expr_to_lit`) (MIT or Apache-2.0).

use std::borrow::Cow;

use smallvec::SmallVec;
use syntax::{
    NodeOrToken, SyntaxKind, TextRange,
    view::{self as ast, AstNode, AstToken},
};

use super::super::Args as Written;

/// rustc's `ArgParser`.
#[derive(Clone, Debug)]
pub(super) enum Args<'t> {
    NoArgs,
    List(List<'t>),
    /// `= value`; `range` is from the `=` to the end of the value.
    NameValue { range: TextRange, value: Lit<'t> },
}

impl Args<'_> {
    /// rustc's `ArgParser::span`.
    pub(super) fn range(&self) -> Option<TextRange> {
        match self {
            Args::NoArgs => None,
            Args::List(list) => Some(list.range),
            Args::NameValue { range, .. } => Some(*range),
        }
    }
}

/// rustc's `MetaItemListParser`: its items and where its delimiters are.
#[derive(Clone, Debug)]
pub(super) struct List<'t> {
    pub(super) items: Vec<Item<'t>>,
    pub(super) range: TextRange,
}

#[derive(Clone, Debug)]
pub(super) enum Item<'t> {
    Meta(MetaItem<'t>),
    Lit(Lit<'t>),
}

impl Item<'_> {
    pub(super) fn range(&self) -> TextRange {
        match self {
            Item::Meta(it) => it.range(),
            Item::Lit(it) => it.range(),
        }
    }
}

/// rustc's `MetaItemParser`.
#[derive(Clone, Debug)]
pub(super) struct MetaItem<'t> {
    /// Whether the path starts with `::`.
    rooted: bool,
    /// Each segment's name (a raw one's without the `r#`) and where it is written.
    pub(super) path: SmallVec<[(&'t str, TextRange); 2]>,
    pub(super) path_range: TextRange,
    pub(super) args: Args<'t>,
}

impl<'t> MetaItem<'t> {
    /// The name of a path of one segment.
    pub(super) fn ident(&self) -> Option<&'t str> {
        match &self.path[..] {
            [(name, _)] if !self.rooted => Some(*name),
            _ => None,
        }
    }

    pub(super) fn range(&self) -> TextRange {
        match self.args.range() {
            Some(args) => self.path_range.cover(args),
            None => self.path_range,
        }
    }

    /// The path as rustc prints it.
    pub(super) fn path_text(&self) -> String {
        // alloc: a diagnostic's text (`Invalid::ExpectedIdentifier`)
        let mut text = String::new();
        for (i, (name, _)) in self.path.iter().enumerate() {
            if i > 0 || self.rooted {
                text.push_str("::");
            }
            text.push_str(name);
        }
        text
    }
}

/// An unsuffixed literal: its token, and a string literal's value (borrowing the literal's
/// text unless it has escapes).
#[derive(Clone, Debug)]
pub(super) struct Lit<'t> {
    pub(super) token: ast::Token<'t>,
    pub(super) str: Option<Cow<'t, str>>,
}

impl<'t> Lit<'t> {
    pub(super) fn range(&self) -> TextRange {
        self.token.text_range()
    }

    /// The literal as it is written: rustc's snippet of its span.
    pub(super) fn text(&self) -> &'t str {
        self.token.text()
    }

    /// The token `token`, when it is an unsuffixed literal rustc takes (a string literal's
    /// escapes valid).
    fn of(token: ast::Token<'t>) -> Option<Lit<'t>> {
        let text = token.text();
        let quoted = || text.ends_with(['"', '#']);
        let (valid, str) = match token.kind() {
            SyntaxKind::STRING => match ast::String::cast(token)?.value() {
                Ok(value) => (quoted(), Some(value)),
                Err(_) => (false, None),
            },
            SyntaxKind::BYTE_STRING => (quoted() && ast::ByteString::cast(token)?.value().is_ok(), None),
            SyntaxKind::C_STRING => (quoted() && ast::CString::cast(token)?.value().is_ok(), None),
            SyntaxKind::CHAR => (text.ends_with('\'') && ast::Char::cast(token)?.value().is_ok(), None),
            SyntaxKind::BYTE => (text.ends_with('\'') && ast::Byte::cast(token)?.value().is_ok(), None),
            SyntaxKind::INT_NUMBER => (ast::IntNumber::cast(token)?.suffix().is_none(), None),
            SyntaxKind::FLOAT_NUMBER => (ast::FloatNumber::cast(token)?.suffix().is_none(), None),
            SyntaxKind::TRUE_KW | SyntaxKind::FALSE_KW => (true, None),
            _ => (false, None),
        };
        valid.then_some(Lit { token, str })
    }

    /// Whether `token` is a literal token, suffixed or not.
    fn is_literal(token: ast::Token<'_>) -> bool {
        matches!(
            token.kind(),
            SyntaxKind::STRING
                | SyntaxKind::BYTE_STRING
                | SyntaxKind::C_STRING
                | SyntaxKind::CHAR
                | SyntaxKind::BYTE
                | SyntaxKind::INT_NUMBER
                | SyntaxKind::FLOAT_NUMBER
                | SyntaxKind::TRUE_KW
                | SyntaxKind::FALSE_KW
        )
    }
}

/// The arguments of an attribute: `None` when rustc drops the attribute, its value not an
/// unsuffixed literal. A `#[diagnostic::..]` attribute's list may have any delimiter and
/// is empty where it does not parse; another's (`#[rustc_on_unimplemented]`) is dropped then.
pub(super) fn args(written: Written<'_>, diagnostic: bool) -> Option<Args<'_>> {
    Some(match written {
        Written::Word => Args::NoArgs,
        Written::List(tt) => {
            let range = tt.syntax().text_range();
            let items = if diagnostic {
                items(tt).unwrap_or_default()
            } else {
                tt.left_delimiter_token().filter(|it| it.kind() == SyntaxKind::L_PAREN)?;
                items(tt)?
            };
            Args::List(List { items, range })
        }
        Written::NameValue(expr) => {
            let ast::Expr::Literal(lit) = expr? else { return None };
            let value = Lit::of(lit.token())?;
            let eq = lit.syntax().parent().and_then(|it| {
                it.children_with_tokens().filter_map(NodeOrToken::into_token).find(|it| it.kind() == SyntaxKind::EQ)
            });
            let range = match eq {
                Some(eq) => eq.text_range().cover(value.range()),
                None => value.range(),
            };
            Args::NameValue { range, value }
        }
    })
}

/// A token of a token tree, trivia left out, or a delimited group in it.
type Tree<'t> = NodeOrToken<ast::TokenTree<'t>, ast::Token<'t>>;

/// The items of the delimited `tt`, `None` where rustc's parser stops with an error: rustc's
/// `MetaItemListParserContext::parse` and, for a nested list, `parse_paren_comma_seq`.
fn items(tt: ast::TokenTree<'_>) -> Option<Vec<Item<'_>>> {
    let mut parser = inside(tt);
    // alloc: once per list of a diagnostic attribute
    let mut items = Vec::with_capacity(1);
    while !parser.at_end() {
        items.push(parser.item()?);
        if parser.eat(SyntaxKind::COMMA).is_none() && !parser.at_end() {
            return None;
        }
    }
    Some(items)
}

/// What is between the delimiters of a token tree: its trees after an opening delimiter,
/// read with the last one left out when it is a closing delimiter.
struct Parser<'t, I> {
    rest: I,
    /// The next three trees: one more than is ever looked at, to know the last.
    ahead: [Option<Tree<'t>>; 3],
}

/// The parser of what is between the delimiters of `tt`.
fn inside<'t>(tt: ast::TokenTree<'t>) -> Parser<'t, impl Iterator<Item = Tree<'t>> + use<'t>> {
    let mut rest = tt.token_trees_and_tokens().filter(|it| match it {
        NodeOrToken::Token(token) => !token.kind().is_trivia(),
        NodeOrToken::Node(_) => true,
    });
    let ahead = [rest.next(), rest.next(), rest.next()];
    let mut parser = Parser { rest, ahead };
    if let Some(NodeOrToken::Token(first)) = parser.ahead[0]
        && first.kind().is_l_delimiter()
    {
        parser.bump(1);
    }
    parser
}

impl<'t, I: Iterator<Item = Tree<'t>>> Parser<'t, I> {
    /// The tree `n` after the next one, unless it is the last and a closing delimiter.
    fn get(&self, n: usize) -> Option<Tree<'t>> {
        let tree = self.ahead[n]?;
        match tree {
            NodeOrToken::Token(it) if self.ahead[n + 1].is_none() && it.kind().is_r_delimiter() => None,
            _ => Some(tree),
        }
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

    fn token(&self) -> Option<ast::Token<'t>> {
        self.get(0)?.into_token()
    }

    fn eat(&mut self, kind: SyntaxKind) -> Option<ast::Token<'t>> {
        let token = self.token().filter(|it| it.kind() == kind)?;
        self.bump(1);
        Some(token)
    }

    /// `::`, written as one token or two `:` next to each other.
    fn eat_path_sep(&mut self) -> Option<TextRange> {
        if let Some(token) = self.eat(SyntaxKind::COLON2) {
            return Some(token.text_range());
        }
        let first = self.token().filter(|it| it.kind() == SyntaxKind::COLON)?;
        let second = match self.get(1)? {
            NodeOrToken::Token(it) if it.kind() == SyntaxKind::COLON => it,
            _ => return None,
        };
        if first.text_range().end() != second.text_range().start() {
            return None;
        }
        let range = first.text_range().cover(second.text_range());
        self.bump(2);
        Some(range)
    }

    /// rustc's `parse_meta_item_inner`.
    fn item(&mut self) -> Option<Item<'t>> {
        if let Some(token) = self.token()
            && Lit::is_literal(token)
        {
            let lit = Lit::of(token)?;
            self.bump(1);
            return Some(Item::Lit(lit));
        }
        self.meta_item().map(Item::Meta)
    }

    /// rustc's `parse_meta_item`, its path parsed as `PathStyle::Mod`. A `None` ends the
    /// whole list (`items`), so nothing read is put back.
    fn meta_item(&mut self) -> Option<MetaItem<'t>> {
        let leading = self.eat_path_sep();
        let mut path: SmallVec<[(&'t str, TextRange); 2]> = SmallVec::new();
        loop {
            path.push(self.segment()?);
            if self.eat_path_sep().is_none() {
                break;
            }
            if !self.token().is_some_and(is_segment) {
                return None;
            }
        }
        let start = leading.unwrap_or(path[0].1);
        let path_range = start.cover(path[path.len() - 1].1);
        let args = match self.get(0) {
            Some(NodeOrToken::Node(group))
                if group.left_delimiter_token().is_some_and(|it| it.kind() == SyntaxKind::L_PAREN) =>
            {
                self.bump(1);
                let range = group.syntax().text_range();
                let items = items(group)?;
                Args::List(List { items, range })
            }
            _ => match self.eat(SyntaxKind::EQ) {
                Some(eq) => {
                    let value = Lit::of(self.token()?)?;
                    self.bump(1);
                    Args::NameValue { range: eq.text_range().cover(value.range()), value }
                }
                None => Args::NoArgs,
            },
        };
        Some(MetaItem { rooted: leading.is_some(), path, path_range, args })
    }

    /// A path segment: an identifier that is not a reserved keyword, or `self`, `Self`,
    /// `super` or `crate`.
    fn segment(&mut self) -> Option<(&'t str, TextRange)> {
        let token = self.token().filter(|it| is_segment(*it))?;
        self.bump(1);
        let text = token.text();
        let name = text.strip_prefix("r#").unwrap_or(text);
        Some((name, token.text_range()))
    }
}

fn is_segment(token: ast::Token<'_>) -> bool {
    matches!(
        token.kind(),
        SyntaxKind::IDENT | SyntaxKind::SELF_KW | SyntaxKind::SELF_TYPE_KW | SyntaxKind::SUPER_KW | SyntaxKind::CRATE_KW
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_items<R>(text: &str, check: impl FnOnce(Option<Vec<Item<'_>>>) -> R) -> R {
        let source = format!("#[diagnostic::on_unimplemented{text}] trait T {{}}");
        let parse = syntax::SourceFile::parse(&source, span::Edition::CURRENT);
        let tt = parse.root().descendants().find_map(ast::TokenTree::cast).unwrap();
        check(items(tt))
    }

    /// The items between any delimiters, their paths and string values read in place.
    #[test]
    fn a_list_is_read_between_its_delimiters() {
        with_items(r#"(a, ::b::c = "x", r#d(e), 1,)"#, |it| {
            let it = it.unwrap();
            let paths: Vec<Option<String>> = it
                .iter()
                .map(|it| match it {
                    Item::Meta(meta) => Some(meta.path_text()),
                    Item::Lit(_) => None,
                })
                .collect();
            assert_eq!(paths, [Some("a".to_owned()), Some("::b::c".to_owned()), Some("d".to_owned()), None]);
            let Item::Meta(MetaItem { args: Args::NameValue { value, .. }, .. }) = &it[1] else { panic!() };
            assert!(matches!(value.str, Some(Cow::Borrowed("x"))));
            assert!(matches!(&it[2], Item::Meta(MetaItem { args: Args::List(list), .. }) if list.items.len() == 1));
        });
        with_items("[a]", |it| assert_eq!(it.unwrap().len(), 1));
        with_items("()", |it| assert!(it.unwrap().is_empty()));
        with_items("(a b)", |it| assert!(it.is_none()));
        with_items("(a::)", |it| assert!(it.is_none()));
    }
}
