//! The arguments of an attribute as rustc's attribute parser reads them (`ArgParser`): none,
//! `= literal`, or a list of meta items and literals, each meta item a path with arguments of
//! its own; and the errors of reading them: list delimiters that are not `(..)`, something that
//! is not a literal where one must be, a suffixed literal, items not separated with `,`, a
//! token where a meta item cannot go on, a literal rustc cannot make a value of.
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/parser.rs` (`ArgParser::from_attr_args`,
//! `expr_to_lit`, `MetaItemListParserContext`: `parse_meta_item_inner`, `parse_meta_item`,
//! `expected_lit`, `should_continue_parsing_meta_items`), the texts of
//! `compiler/rustc_attr_parsing/src/session_diagnostics.rs` (`MetaBadDelim`, `InvalidMetaItem`,
//! `SuffixedLiteralInAttribute`, `ExpectedComma`), `compiler/rustc_ast/src/util/literal.rs`
//! (`LitKind::from_token_lit`, `integer_lit`, `float_lit`), `create_lit_error` in
//! `compiler/rustc_session/src/diagnostics.rs` and `token_descr` in
//! `compiler/rustc_parse/src/parser/mod.rs` (MIT or Apache-2.0).

use std::borrow::Cow;

use smallvec::SmallVec;
use span::Edition;
use syntax::{
    NodeOrToken, SmolStr, SyntaxKind, TextRange,
    view::{self as ast, AstNode as _, AstToken as _, HasGenericArgs as _, IsString as _},
};

use crate::{
    cx::Cx,
    lints::syntax::attrs::{Args, Attr},
    model::{Applicability, Diag, Span, SuggestionStyle},
};

/// An attribute's arguments, or a meta item's: rustc's `ArgParser`.
#[derive(Clone, Debug)]
pub(crate) enum ArgParser {
    NoArgs,
    List(MetaList),
    NameValue(NameValue),
}

/// `(a, b = "c", 1)`: rustc's `MetaItemListParser`.
#[derive(Clone, Debug)]
pub(crate) struct MetaList {
    pub(crate) items: Vec<MetaItemOrLit>,
    /// From `(` to `)`.
    pub(crate) range: TextRange,
}

/// An element of a list: rustc's `MetaItemOrLitParser`.
#[derive(Clone, Debug)]
pub(crate) enum MetaItemOrLit {
    Item(MetaItem),
    Lit(Lit),
}

/// `path`, `path(..)` or `path = lit`: rustc's `MetaItemParser`.
#[derive(Clone, Debug)]
pub(crate) struct MetaItem {
    pub(crate) path: Path,
    pub(crate) args: ArgParser,
}

/// A path of identifiers: rustc's `PathParser`.
#[derive(Clone, Debug)]
pub(crate) struct Path {
    pub(crate) segments: SmallVec<[Ident; 1]>,
    /// Written with a leading `::`: rustc's `kw::PathRoot` first segment, so never a word.
    pub(crate) rooted: bool,
    pub(crate) range: TextRange,
}

#[derive(Clone, Debug)]
pub(crate) struct Ident {
    pub(crate) name: SmolStr,
    pub(crate) range: TextRange,
}

/// `= lit`: rustc's `NameValueParser`.
#[derive(Clone, Debug)]
pub(crate) struct NameValue {
    /// The `=`.
    pub(crate) eq: TextRange,
    pub(crate) value: Lit,
}

/// A literal: rustc's `MetaItemLit`.
#[derive(Clone, Debug)]
pub(crate) struct Lit {
    pub(crate) kind: LitKind,
    /// As written, without its suffix: rustc's `symbol` (for a string, between the quotes).
    pub(crate) symbol: SmolStr,
    pub(crate) suffix: Option<SmolStr>,
    pub(crate) range: TextRange,
}

/// rustc's `LitKind`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum LitKind {
    /// The string's value, unescaped: the same copy as the literal's symbol when it has no
    /// escapes.
    Str(SmolStr),
    ByteStr(Vec<u8>),
    CStr(Vec<u8>),
    Byte(u8),
    Char(char),
    Int(u128),
    Float,
    Bool(bool),
    /// A literal rustc cannot make a value of (an error was given).
    Err,
}

impl ArgParser {
    /// rustc's `ArgParser::span`: the list with its parentheses, or from `=` to the value.
    pub(crate) fn range(&self) -> Option<TextRange> {
        match self {
            ArgParser::NoArgs => None,
            ArgParser::List(list) => Some(list.range),
            ArgParser::NameValue(nv) => Some(nv.args_range()),
        }
    }

    pub(crate) fn as_list(&self) -> Option<&MetaList> {
        match self {
            ArgParser::List(list) => Some(list),
            _ => None,
        }
    }

    pub(crate) fn as_name_value(&self) -> Option<&NameValue> {
        match self {
            ArgParser::NameValue(nv) => Some(nv),
            _ => None,
        }
    }

    /// rustc's `as_no_args`: `Err` with the arguments' range when there are some.
    pub(crate) fn as_no_args(&self) -> Result<(), TextRange> {
        match self {
            ArgParser::NoArgs => Ok(()),
            ArgParser::List(list) => Err(list.range),
            ArgParser::NameValue(nv) => Err(nv.args_range()),
        }
    }
}

impl MetaList {
    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The one element, when there is exactly one.
    pub(crate) fn as_single(&self) -> Option<&MetaItemOrLit> {
        match self.items.as_slice() {
            [single] => Some(single),
            _ => None,
        }
    }

    /// rustc's `mixed`: the elements in order.
    pub(crate) fn mixed(&self) -> std::slice::Iter<'_, MetaItemOrLit> {
        self.items.iter()
    }
}

impl MetaItemOrLit {
    pub(crate) fn range(&self) -> TextRange {
        match self {
            MetaItemOrLit::Item(item) => item.range(),
            MetaItemOrLit::Lit(lit) => lit.range,
        }
    }

    pub(crate) fn as_lit(&self) -> Option<&Lit> {
        match self {
            MetaItemOrLit::Lit(lit) => Some(lit),
            MetaItemOrLit::Item(_) => None,
        }
    }

    pub(crate) fn meta_item(&self) -> Option<&MetaItem> {
        match self {
            MetaItemOrLit::Item(item) => Some(item),
            MetaItemOrLit::Lit(_) => None,
        }
    }

    /// The meta item, when it has no arguments.
    pub(crate) fn meta_item_no_args(&self) -> Option<&MetaItem> {
        self.meta_item().filter(|it| matches!(it.args, ArgParser::NoArgs))
    }
}

impl MetaItem {
    /// From its path to the end of its arguments: rustc's `MetaItemParser::span`.
    pub(crate) fn range(&self) -> TextRange {
        match self.args.range() {
            Some(args) => TextRange::new(self.path.range.start(), args.end()),
            None => self.path.range,
        }
    }

    /// Its path's one segment, when it has one: rustc's `ident`.
    pub(crate) fn ident(&self) -> Option<&Ident> {
        self.path.word()
    }

    /// Its arguments, when its path is the one word `name`: rustc's `word_is`.
    pub(crate) fn word_is(&self, name: &str) -> Option<&ArgParser> {
        self.path.word_is(name).then_some(&self.args)
    }
}

impl Path {
    /// Its one segment, when it has one: rustc's `word`.
    pub(crate) fn word(&self) -> Option<&Ident> {
        match self.segments.as_slice() {
            [ident] if !self.rooted => Some(ident),
            _ => None,
        }
    }

    pub(crate) fn word_sym(&self) -> Option<&str> {
        self.word().map(|it| it.name.as_str())
    }

    pub(crate) fn word_is(&self, name: &str) -> bool {
        self.word_sym() == Some(name)
    }

    pub(crate) fn segments_is(&self, names: &[&str]) -> bool {
        !self.rooted
            && self.segments.len() == names.len()
            && self.segments.iter().zip(names).all(|(a, b)| a.name == *b)
    }

    /// As rustc prints it: `a::b`, `::a::b`.
    pub(crate) fn text(&self) -> String {
        // alloc: the path as an error prints it
        let mut text = String::new();
        for (i, segment) in self.segments.iter().enumerate() {
            if i > 0 || self.rooted {
                text.push_str("::");
            }
            text.push_str(&segment.name);
        }
        text
    }
}

impl NameValue {
    pub(crate) fn value_as_lit(&self) -> &Lit {
        &self.value
    }

    /// The value, when it is a string: rustc's `value_as_str`.
    pub(crate) fn value_as_str(&self) -> Option<&str> {
        self.value.str()
    }

    pub(crate) fn value_range(&self) -> TextRange {
        self.value.range
    }

    /// From `=` to the end of the value: rustc's `args_span`.
    pub(crate) fn args_range(&self) -> TextRange {
        TextRange::new(self.eq.start(), self.value.range.end())
    }
}

impl Lit {
    /// The value of a string literal (raw or not): rustc's `LitKind::str`.
    pub(crate) fn str(&self) -> Option<&str> {
        match &self.kind {
            LitKind::Str(it) => Some(it.as_str()),
            _ => None,
        }
    }

    pub(crate) fn is_str(&self) -> bool {
        matches!(self.kind, LitKind::Str(_))
    }

    pub(crate) fn is_bytestr(&self) -> bool {
        matches!(self.kind, LitKind::ByteStr(_))
    }

    /// An integer's value, suffixed or not (rustc's `LitKind::Int`).
    pub(crate) fn int(&self) -> Option<u128> {
        match self.kind {
            LitKind::Int(it) => Some(it),
            _ => None,
        }
    }

    pub(crate) fn is_bool(&self) -> bool {
        matches!(self.kind, LitKind::Bool(_))
    }
}

/// What reading arguments needs: the crate, where the errors go, and whether to say them.
struct Reader<'a, 'c, 'db> {
    cx: &'a Cx<'c, 'db>,
    attr: &'a Attr<'db>,
    edition: Edition,
    /// `Recovery::Forbidden`: `#[diagnostic::..]` and `#[rustc_dummy]` read without saying
    /// anything, and stop at the first error.
    forbidden: bool,
    errors: &'a mut Vec<Diag>,
}

/// rustc's `ArgParser::from_attr_args` of `attr`, whose path is `path`: its arguments, or `None`
/// for arguments rustc cannot read, with the errors of reading them in `errors`.
pub(crate) fn parse<'db>(cx: &Cx<'_, 'db>, attr: &Attr<'db>, path: &[&str], errors: &mut Vec<Diag>) -> Option<ArgParser> {
    let edition = attr.file.edition(cx.db);
    let lenient = matches!(path, ["rustc_dummy"] | ["diagnostic", ..]);
    let mut reader = Reader { cx, attr, edition, forbidden: false, errors };
    match attr.args() {
        Args::Word => Some(ArgParser::NoArgs),
        Args::List(tt) => {
            let range = tt.syntax().text_range();
            if lenient {
                // alloc: empty, allocates nothing: a forbidden reader pushes no error
                let mut quiet = Vec::new();
                let mut reader = Reader { cx, attr, edition, forbidden: true, errors: &mut quiet };
                let items = reader.list(tt).unwrap_or_default();
                return Some(ArgParser::List(MetaList { items, range }));
            }
            if !is_paren(tt) {
                reader.bad_delim(tt);
                return None;
            }
            let items = reader.list(tt)?;
            Some(ArgParser::List(MetaList { items, range }))
        }
        Args::NameValue(value) => {
            let eq = name_value_eq(attr)?;
            let value = reader.expr_to_lit(value?)?;
            Some(ArgParser::NameValue(NameValue { eq, value }))
        }
    }
}

/// The `=` of `#[path = value]`.
fn name_value_eq(attr: &Attr<'_>) -> Option<TextRange> {
    let meta = attr.meta()?;
    let ast::Meta::KeyValueMeta(kv) = meta else { return None };
    Some(kv.eq_token()?.text_range())
}

fn is_paren(tt: ast::TokenTree<'_>) -> bool {
    tt.l_paren_token().is_some()
}

/// The tokens between a token tree's delimiters, trivia left out: a nested token tree is one
/// element.
fn inside<'t>(tt: ast::TokenTree<'t>) -> SmallVec<[ast::Element<'t>; 8]> {
    let mut elements = tt.syntax().children_with_tokens().filter(|it| !it.kind().is_trivia()).peekable();
    let _open = elements.next_if(|it| it.kind().is_l_delimiter());
    // alloc: inline up to 8 elements; spills only for a longer attribute argument list
    let mut elements: SmallVec<[ast::Element<'t>; 8]> = elements.collect();
    if elements.last().is_some_and(|it| it.kind().is_r_delimiter()) {
        elements.pop();
    }
    elements
}

/// Where a cursor in a list stopped, and what rustc's parser then expected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum After {
    /// A path of one segment, with no arguments: `(`, `,`, `::` and `=` could follow.
    Word,
    /// A path of more segments: `(`, `,`, `::` and `=`.
    Path,
    /// A list, a value or a literal: only `,`.
    Done,
}

struct Cursor<'e, 't> {
    elements: &'e [ast::Element<'t>],
    at: usize,
    /// The end of the last element taken (rustc's `prev_token.span`).
    prev_end: Option<TextRange>,
}

impl<'t> Cursor<'_, 't> {
    fn peek(&self) -> Option<ast::Element<'t>> {
        self.elements.get(self.at).copied()
    }

    fn peek_token(&self) -> Option<ast::Token<'t>> {
        self.peek()?.into_token()
    }

    fn bump(&mut self) {
        if let Some(it) = self.peek() {
            self.prev_end = Some(it.text_range());
        }
        self.at += 1;
    }

    fn done(&self) -> bool {
        self.at >= self.elements.len()
    }

    fn at_kind(&self, kind: SyntaxKind) -> bool {
        self.peek_token().is_some_and(|it| it.kind() == kind)
    }

    /// `::`, as one token or as two `:` with nothing between them.
    fn at_path_sep(&self) -> Option<usize> {
        let first = self.peek_token()?;
        match first.kind() {
            SyntaxKind::COLON2 => Some(1),
            SyntaxKind::COLON => {
                let second = self.elements.get(self.at + 1).copied()?.into_token()?;
                (second.kind() == SyntaxKind::COLON && second.text_range().start() == first.text_range().end())
                    .then_some(2)
            }
            _ => None,
        }
    }

    fn eat_path_sep(&mut self) -> bool {
        let Some(len) = self.at_path_sep() else { return false };
        for _ in 0..len {
            self.bump();
        }
        true
    }
}

/// Why a meta item could not be read.
enum Fail {
    /// The error is said (or, forbidden, would be).
    Said,
    /// Nothing was taken: the caller says "expected a literal".
    NoProgress,
}

impl Reader<'_, '_, '_> {
    fn span(&self, range: TextRange) -> Span {
        self.cx.span(self.attr.in_file(range))
    }

    fn error(&mut self, diag: Diag) {
        if !self.forbidden {
            self.errors.push(diag);
        }
    }

    /// `MetaBadDelim`: `#[path[..]]` or `#[path{..}]`.
    fn bad_delim(&mut self, tt: ast::TokenTree<'_>) {
        let range = tt.syntax().text_range();
        let (Some(open), Some(close)) = (tt.syntax().first_child_or_token(), tt.syntax().last_child_or_token()) else {
            return;
        };
        let mut diag = Diag::error(None, "wrong meta list delimiters", self.span(range));
        diag.multipart_suggestion(
            "the delimiters should be `(` and `)`",
            // alloc: the error's suggestion
            vec![(self.span(open.text_range()), "(".to_owned()), (self.span(close.text_range()), ")".to_owned())],
            Applicability::MachineApplicable,
        );
        self.error(diag);
    }

    /// rustc's `MetaItemListParserContext::parse`: the elements of a list.
    fn list(&mut self, tt: ast::TokenTree<'_>) -> Option<Vec<MetaItemOrLit>> {
        let elements = inside(tt);
        let mut cursor = Cursor { elements: &elements, at: 0, prev_end: tt.l_paren_token().map(|it| it.text_range()) };
        let mut items = Vec::with_capacity(1);
        while !cursor.done() {
            let (item, after) = match self.inner(&mut cursor) {
                Ok(it) => it,
                Err(Fail::Said) => return None,
                Err(Fail::NoProgress) => {
                    self.expected_lit(&cursor);
                    return None;
                }
            };
            items.push(item);
            if !self.should_continue(&mut cursor, after)? {
                break;
            }
        }
        Some(items)
    }

    /// rustc's `should_continue_parsing_meta_items`: a `,` goes on to the next element, the
    /// end stops; anything else is an error: items without a `,` between them, or a token that
    /// cannot follow.
    fn should_continue(&mut self, cursor: &mut Cursor<'_, '_>, after: After) -> Option<bool> {
        if cursor.at_kind(SyntaxKind::COMMA) {
            cursor.bump();
            return Some(true);
        }
        if cursor.done() {
            return Some(false);
        }
        if !self.forbidden {
            let mut probe = Cursor { elements: cursor.elements, at: cursor.at, prev_end: cursor.prev_end };
            // alloc: on the error path: neither a `,` nor the end follows, so an error is said
            let mut missing = Vec::new();
            let mut found_comma = false;
            // alloc: empty, allocates nothing: a forbidden reader pushes no error
            let mut quiet = Vec::new();
            let mut probing = Reader {
                cx: self.cx,
                attr: self.attr,
                edition: self.edition,
                forbidden: true,
                errors: &mut quiet,
            };
            while !probe.done() {
                let before = probe.prev_end.map(|it| TextRange::empty(it.end()));
                if probing.inner(&mut probe).is_err() {
                    break;
                }
                if !found_comma && let Some(at) = before {
                    missing.push(at);
                }
                found_comma = probe.at_kind(SyntaxKind::COMMA);
                if found_comma {
                    probe.bump();
                }
            }
            let mut missing = missing.into_iter();
            if let Some(first) = missing.next() {
                let span = self.span(first);
                // clone: the error's span, used again for its suggestion
                let mut diag = Diag::error(None, "attribute items not separated with `,`", span.clone());
                diag.suggestion_with_style(
                    // alloc: the error's suggestion
                    vec![(span, ",".to_owned())],
                    "try adding `,` here",
                    Applicability::MaybeIncorrect,
                    SuggestionStyle::HideCodeInline,
                );
                for more in missing {
                    diag.suggestion_with_style(
                        // alloc: the error's suggestion
                        vec![(self.span(more), ",".to_owned())],
                        "try adding `,` here",
                        Applicability::MaybeIncorrect,
                        SuggestionStyle::HideCodeInline,
                    );
                }
                self.error(diag);
                return None;
            }
        }
        self.unexpected(cursor, after);
        None
    }

    /// rustc's `unexpected_any` after an element: "expected one of `(`, `,`, `::`, or `=`,
    /// found `-`".
    fn unexpected(&mut self, cursor: &Cursor<'_, '_>, after: After) {
        let Some(found) = cursor.peek() else { return };
        let expected: &[&str] = match after {
            After::Word | After::Path => &["`(`", "`,`", "`::`", "`=`"],
            After::Done => &["`,`"],
        };
        let expected = match expected {
            // alloc: the error's message
            [one] => one.to_string(),
            // alloc: the error's message
            [init @ .., last] => format!("one of {}, or {last}", init.join(", ")),
            [] => return,
        };
        let descr = self.descr(found);
        let span = self.span(found.text_range());
        // alloc: the error's message; clone: its span, used again for its label
        let mut diag = Diag::error(None, format!("expected {expected}, found {descr}"), span.clone());
        // alloc: the error's label
        diag.span_label(span, format!("expected {expected}"));
        self.error(diag);
    }

    /// rustc's `parse_meta_item_inner`: a literal, else a meta item.
    fn inner(&mut self, cursor: &mut Cursor<'_, '_>) -> Result<(MetaItemOrLit, After), Fail> {
        if let Some(token) = cursor.peek_token()
            && is_lit(token.kind())
        {
            cursor.bump();
            let lit = self.lit(token).ok_or(Fail::Said)?;
            if lit.suffix.is_some() && lit.kind != LitKind::Err {
                if self.forbidden {
                    return Err(Fail::Said);
                }
                self.suffixed(lit.range);
            }
            return Ok((MetaItemOrLit::Lit(lit), After::Done));
        }
        let (item, after) = self.item(cursor)?;
        Ok((MetaItemOrLit::Item(item), after))
    }

    /// rustc's `parse_meta_item`: a path (`PathStyle::Mod`), then `(..)`, `= lit` or nothing.
    fn item(&mut self, cursor: &mut Cursor<'_, '_>) -> Result<(MetaItem, After), Fail> {
        let start = cursor.at;
        let rooted = cursor.eat_path_sep();
        let mut segments: SmallVec<[Ident; 1]> = SmallVec::new();
        loop {
            let Some(token) = cursor.peek_token() else {
                return Err(self.expected_ident(cursor, start));
            };
            if !is_segment(token.kind()) {
                return Err(self.expected_ident(cursor, start));
            }
            segments.push(Ident { name: SmolStr::new(token.text()), range: token.text_range() });
            cursor.bump();
            if !cursor.eat_path_sep() {
                break;
            }
        }
        let first = cursor.elements[start].text_range().start();
        let last = segments.last().map(|it| it.range.end()).unwrap_or(first);
        let path = Path { segments, rooted, range: TextRange::new(first, last) };
        let one = path.word().is_some();
        let (args, after) = match cursor.peek() {
            Some(NodeOrToken::Node(node)) if node.kind() == SyntaxKind::TOKEN_TREE => {
                let tt = ast::TokenTree::cast(node).ok_or(Fail::Said)?;
                if !is_paren(tt) {
                    if one {
                        return Ok((MetaItem { path, args: ArgParser::NoArgs }, After::Word));
                    }
                    return Ok((MetaItem { path, args: ArgParser::NoArgs }, After::Path));
                }
                cursor.bump();
                let items = self.list(tt).ok_or(Fail::Said)?;
                (ArgParser::List(MetaList { items, range: tt.syntax().text_range() }), After::Done)
            }
            Some(NodeOrToken::Token(token)) if token.kind() == SyntaxKind::EQ => {
                let eq = token.text_range();
                cursor.bump();
                let value = self.unsuffixed_lit(cursor)?;
                (ArgParser::NameValue(NameValue { eq, value }), After::Done)
            }
            _ => (ArgParser::NoArgs, if one { After::Word } else { After::Path }),
        };
        Ok((MetaItem { path, args }, after))
    }

    /// A path that does not go on with an identifier: nothing taken is for the caller to say;
    /// after a `::`, rustc's "expected identifier, found ..".
    fn expected_ident(&mut self, cursor: &Cursor<'_, '_>, start: usize) -> Fail {
        if cursor.at == start {
            return Fail::NoProgress;
        }
        let Some(found) = cursor.peek() else {
            return Fail::Said;
        };
        let descr = self.descr(found);
        let span = self.span(found.text_range());
        // alloc: the error's message; clone: its span, used again for its label
        let mut diag = Diag::error(None, format!("expected identifier, found {descr}"), span.clone());
        diag.span_label(span, "expected identifier");
        self.error(diag);
        Fail::Said
    }

    /// rustc's `parse_unsuffixed_meta_item_lit`: the literal after `=`.
    fn unsuffixed_lit(&mut self, cursor: &mut Cursor<'_, '_>) -> Result<Lit, Fail> {
        let Some(token) = cursor.peek_token().filter(|it| is_lit(it.kind())) else {
            self.expected_lit(cursor);
            return Err(Fail::Said);
        };
        cursor.bump();
        let lit = self.lit(token).ok_or(Fail::Said)?;
        if lit.suffix.is_some() && lit.kind != LitKind::Err {
            if self.forbidden {
                return Err(Fail::Said);
            }
            self.suffixed(lit.range);
        }
        Ok(lit)
    }

    /// `SuffixedLiteralInAttribute`.
    fn suffixed(&mut self, range: TextRange) {
        let mut diag = Diag::error(None, "suffixed literals are not allowed in attributes", self.span(range));
        diag.help("instead of using a suffixed literal (`1u8`, `1.0f32`, etc.), use an unsuffixed version (`1`, `1.0`, etc.)");
        self.error(diag);
    }

    /// rustc's `expected_lit`: `InvalidMetaItem` at the token where a literal or meta item was
    /// expected, with what rustc makes of what is written there as a statement.
    fn expected_lit(&mut self, cursor: &Cursor<'_, '_>) {
        if self.forbidden {
            return;
        }
        let Some(found) = cursor.peek() else {
            if let Some(prev) = cursor.prev_end {
                let span = self.span(TextRange::empty(prev.end()));
                let diag = Diag::error(
                    None,
                    "expected a literal (`1u8`, `1.0f32`, `\"string\"`, etc.) here, found end of input",
                    span,
                );
                self.error(diag);
            }
            return;
        };
        let elements = &cursor.elements[cursor.at..];
        match statement(elements) {
            Some(Statement { descr, range, negated, sentence_end }) => {
                let span = self.span(range);
                let mut diag = Diag::error(
                    None,
                    // alloc: the error's message
                    format!("expected a literal (`1u8`, `1.0f32`, `\"string\"`, etc.) here, found {descr}"),
                    // clone: the error's span, used again for its label and suggestion
                    span.clone(),
                );
                // alloc: the error's label; clone: its span, used again for the suggestion
                diag.span_label(span.clone(), format!("{descr}s are not allowed here"));
                if let Some(minus) = negated {
                    diag.multipart_suggestion(
                        "negative numbers are not literals, try removing the `-` sign",
                        // alloc: the error's suggestion
                        vec![(self.span(minus), String::new())],
                        Applicability::MachineApplicable,
                    );
                }
                if let Some(end) = sentence_end {
                    diag.multipart_suggestion(
                        "surround the identifier with quotation marks to make it into a string literal",
                        // alloc: the error's suggestion
                        vec![(span.shrink_to_lo(), "\"".to_owned()), (self.span(TextRange::empty(end)), "\"".to_owned())],
                        Applicability::MachineApplicable,
                    );
                }
                self.error(diag);
            }
            None => {
                let descr = self.descr(found);
                let diag = Diag::error(
                    None,
                    // alloc: the error's message
                    format!("expected a literal (`1u8`, `1.0f32`, `\"string\"`, etc.) here, found {descr}"),
                    self.span(found.text_range()),
                );
                self.error(diag);
            }
        }
    }

    /// rustc's `expr_to_lit`: the value of `#[path = value]`.
    fn expr_to_lit(&mut self, expr: ast::Expr<'_>) -> Option<Lit> {
        match expr {
            ast::Expr::Literal(literal) => {
                let lit = self.lit(literal.token())?;
                if lit.kind == LitKind::Err {
                    return Some(lit);
                }
                if lit.suffix.is_some() {
                    self.suffixed(lit.range);
                    return None;
                }
                Some(lit)
            }
            // Expansion makes a literal of a macro call (`#[doc = include_str!("..")]`) before
            // the attribute is parsed; what it makes is read where the call is expanded.
            ast::Expr::MacroExpr(_) => None,
            _ => {
                let range = expr.syntax().text_range();
                let span = self.span(range);
                // clone: the error's span, used again for its suggestion
                let mut diag = Diag::error(None, "attribute value must be a literal", span.clone());
                if let ast::Expr::PathExpr(path) = expr
                    && let Some(path) = path.path()
                    && path.qualifier().is_none()
                    && path.segment().is_some_and(|it| it.generic_arg_list().is_none())
                {
                    diag.multipart_suggestion(
                        "you might have meant to write a string literal",
                        // alloc: the error's suggestion
                        vec![(span.shrink_to_lo(), "\"".to_owned()), (span.shrink_to_hi(), "\"".to_owned())],
                        Applicability::MaybeIncorrect,
                    );
                }
                self.error(diag);
                None
            }
        }
    }

    /// rustc's `MetaItemLit::from_token_lit` of a literal token, with `create_lit_error` when
    /// rustc cannot make a value of it (the literal is then `LitKind::Err`).
    fn lit(&mut self, token: ast::Token<'_>) -> Option<Lit> {
        let range = token.text_range();
        let text = token.text();
        let (kind, symbol, suffix) = match token.kind() {
            SyntaxKind::TRUE_KW => (LitKind::Bool(true), SmolStr::new(text), None),
            SyntaxKind::FALSE_KW => (LitKind::Bool(false), SmolStr::new(text), None),
            SyntaxKind::INT_NUMBER => {
                let int = ast::IntNumber::cast(token)?;
                let suffix = int.suffix().map(SmolStr::new);
                let symbol = SmolStr::new(&text[..text.len() - suffix.as_deref().map_or(0, str::len)]);
                match suffix.as_deref() {
                    None | Some("i8" | "i16" | "i32" | "i64" | "i128" | "isize" | "u8" | "u16" | "u32" | "u64" | "u128" | "usize") => {
                        match int.value() {
                            Ok(value) => (LitKind::Int(value), symbol, suffix),
                            Err(_) => {
                                self.int_too_large(range, int.radix() as u32);
                                (LitKind::Err, symbol, suffix)
                            }
                        }
                    }
                    Some("f16" | "f32" | "f64" | "f128") if int.radix() as u32 == 10 => (LitKind::Float, symbol, suffix),
                    Some(other) => {
                        self.invalid_int_suffix(range, &symbol, other);
                        (LitKind::Err, symbol, suffix)
                    }
                }
            }
            SyntaxKind::FLOAT_NUMBER => {
                let float = ast::FloatNumber::cast(token)?;
                let suffix = float.suffix().map(SmolStr::new);
                let symbol = SmolStr::new(&text[..text.len() - suffix.as_deref().map_or(0, str::len)]);
                match suffix.as_deref() {
                    None | Some("f16" | "f32" | "f64" | "f128") => (LitKind::Float, symbol, suffix),
                    Some(other) => {
                        self.invalid_float_suffix(range, other);
                        (LitKind::Err, symbol, suffix)
                    }
                }
            }
            SyntaxKind::STRING => {
                let string = ast::String::cast(token)?;
                let symbol = SmolStr::new(string.text_without_quotes());
                match suffix(text) {
                    Some(suffix) => {
                        self.invalid_suffix(range, "string", suffix);
                        (LitKind::Err, symbol, Some(SmolStr::new(suffix)))
                    }
                    None => match string.value() {
                        // clone: inline, or a refcount on the one copy the symbol and value share
                        Ok(Cow::Borrowed(value)) if value == symbol => (LitKind::Str(symbol.clone()), symbol, None),
                        Ok(value) => (LitKind::Str(SmolStr::from(value)), symbol, None),
                        Err(_) => (LitKind::Err, symbol, None),
                    },
                }
            }
            SyntaxKind::BYTE_STRING => {
                let string = ast::ByteString::cast(token)?;
                let symbol = SmolStr::new(string.text_without_quotes());
                match suffix(text) {
                    Some(suffix) => {
                        self.invalid_suffix(range, "byte string", suffix);
                        (LitKind::Err, symbol, Some(SmolStr::new(suffix)))
                    }
                    None => match string.value() {
                        Ok(value) => (LitKind::ByteStr(value.into_owned()), symbol, None),
                        Err(_) => (LitKind::Err, symbol, None),
                    },
                }
            }
            SyntaxKind::C_STRING => {
                let string = ast::CString::cast(token)?;
                let symbol = SmolStr::new(string.text_without_quotes());
                match suffix(text) {
                    Some(suffix) => {
                        self.invalid_suffix(range, "C string", suffix);
                        (LitKind::Err, symbol, Some(SmolStr::new(suffix)))
                    }
                    None => match string.value() {
                        Ok(value) => (LitKind::CStr(value.into_owned()), symbol, None),
                        Err(_) => (LitKind::Err, symbol, None),
                    },
                }
            }
            SyntaxKind::CHAR => {
                let char = ast::Char::cast(token)?;
                match suffix(text) {
                    Some(suffix) => {
                        self.invalid_suffix(range, "char", suffix);
                        (LitKind::Err, SmolStr::new(text), Some(SmolStr::new(suffix)))
                    }
                    None => match char.value() {
                        Ok(value) => (LitKind::Char(value), SmolStr::new(text), None),
                        Err(_) => (LitKind::Err, SmolStr::new(text), None),
                    },
                }
            }
            SyntaxKind::BYTE => {
                let byte = ast::Byte::cast(token)?;
                match suffix(text) {
                    Some(suffix) => {
                        self.invalid_suffix(range, "byte", suffix);
                        (LitKind::Err, SmolStr::new(text), Some(SmolStr::new(suffix)))
                    }
                    None => match byte.value() {
                        Ok(value) => (LitKind::Byte(value), SmolStr::new(text), None),
                        Err(_) => (LitKind::Err, SmolStr::new(text), None),
                    },
                }
            }
            _ => return None,
        };
        Some(Lit { kind, symbol, suffix, range })
    }

    /// `IntLiteralTooLarge`.
    fn int_too_large(&mut self, range: TextRange, radix: u32) {
        let max = u128::MAX;
        let limit = match radix {
            // alloc: the error's note
            2 => format!("{max:#b}"),
            // alloc: the error's note
            8 => format!("{max:#o}"),
            // alloc: the error's note
            16 => format!("{max:#x}"),
            // alloc: the error's note
            _ => format!("{max}"),
        };
        let mut diag = Diag::error(None, "integer literal is too large", self.span(range));
        // alloc: the error's note
        diag.note(format!("value exceeds limit of `{limit}`"));
        self.error(diag);
    }

    /// `InvalidIntLiteralWidth`, `InvalidNumLiteralBasePrefix` or `InvalidNumLiteralSuffix`.
    fn invalid_int_suffix(&mut self, range: TextRange, symbol: &str, suffix: &str) {
        let span = self.span(range);
        let looks_like_width = suffix.len() > 1
            && suffix.starts_with(['i', 'u'])
            && suffix[1..].chars().all(|it| it.is_ascii_digit());
        if looks_like_width {
            // alloc: the error's message
            let mut diag = Diag::error(None, format!("invalid width `{}` for integer literal", &suffix[1..]), span);
            diag.help("valid widths are 8, 16, 32, 64 and 128");
            self.error(diag);
            return;
        }
        if let Some(fixed) = fix_base_capitalisation(symbol, suffix) {
            // clone: the error's span, used again for its suggestion
            let mut diag = Diag::error(None, "invalid base prefix for number literal", span.clone());
            diag.note("base prefixes (`0xff`, `0b1010`, `0o755`) are lowercase");
            diag.suggestion(span, "try making the prefix lowercase", fixed, Applicability::MaybeIncorrect);
            self.error(diag);
            return;
        }
        // alloc: the error's message; clone: its span, used again for its label
        let mut diag = Diag::error(None, format!("invalid suffix `{suffix}` for number literal"), span.clone());
        // alloc: the error's label
        diag.span_label(span, format!("invalid suffix `{suffix}`"));
        diag.help("the suffix must be one of the numeric types (`u32`, `isize`, `f32`, etc.)");
        self.error(diag);
    }

    /// `InvalidFloatLiteralWidth` or `InvalidFloatLiteralSuffix`.
    fn invalid_float_suffix(&mut self, range: TextRange, suffix: &str) {
        let span = self.span(range);
        if suffix.len() > 1 && suffix.starts_with('f') && suffix[1..].chars().all(|it| it.is_ascii_digit()) {
            // alloc: the error's message
            let mut diag = Diag::error(None, format!("invalid width `{}` for float literal", &suffix[1..]), span);
            diag.help("valid widths are 32 and 64");
            self.error(diag);
            return;
        }
        // alloc: the error's message; clone: its span, used again for its label
        let mut diag = Diag::error(None, format!("invalid suffix `{suffix}` for float literal"), span.clone());
        // alloc: the error's label
        diag.span_label(span, format!("invalid suffix `{suffix}`"));
        diag.help("valid suffixes are `f32` and `f64`");
        self.error(diag);
    }

    /// `InvalidLiteralSuffix`: a suffix on a literal that cannot have one.
    fn invalid_suffix(&mut self, range: TextRange, kind: &str, suffix: &str) {
        let span = self.span(range);
        // alloc: the error's message; clone: its span, used again for its label
        let mut diag = Diag::error(None, format!("suffixes on {kind} literals are invalid"), span.clone());
        // alloc: the error's label
        diag.span_label(span, format!("invalid suffix `{suffix}`"));
        self.error(diag);
    }

    /// rustc's `token_descr` of an element: "`-`", "keyword `fn`", "reserved identifier `_`".
    fn descr(&self, element: ast::Element<'_>) -> String {
        let text = match element {
            NodeOrToken::Token(token) => token.text(),
            NodeOrToken::Node(node) => {
                return match node.first_token() {
                    // alloc: an error's message
                    Some(open) => format!("`{}`", open.text()),
                    // alloc: an error's message
                    None => "`<eof>`".to_owned(),
                };
            }
        };
        if element.kind() == SyntaxKind::UNDERSCORE {
            // alloc: an error's message
            return format!("reserved identifier `{text}`");
        }
        if is_reserved_keyword(text, self.edition) {
            // alloc: an error's message
            return format!("reserved keyword `{text}`");
        }
        if SyntaxKind::from_keyword(text, self.edition).is_some_and(|it| it.is_strict_keyword(self.edition)) {
            // alloc: an error's message
            return format!("keyword `{text}`");
        }
        // alloc: an error's message
        format!("`{text}`")
    }
}

/// What rustc's `parse_stmt_without_recovery` makes of the tokens where a literal was
/// expected, as `expected_lit` says it: its description, its range, the `-` of a negated
/// literal, and for one identifier the end of the "sentence" it starts.
struct Statement {
    descr: &'static str,
    range: TextRange,
    negated: Option<TextRange>,
    sentence_end: Option<syntax::TextSize>,
}

fn statement(elements: &[ast::Element<'_>]) -> Option<Statement> {
    let first = elements.first()?.as_token()?;
    if first.kind() == SyntaxKind::MINUS
        && let Some(lit) = elements.get(1).and_then(|it| it.as_token())
        && is_lit(lit.kind())
    {
        return Some(Statement {
            descr: "expression",
            range: TextRange::new(first.text_range().start(), lit.text_range().end()),
            negated: Some(TextRange::new(first.text_range().start(), lit.text_range().start())),
            sentence_end: None,
        });
    }
    if !is_segment(first.kind()) {
        return None;
    }
    let mut at = 1;
    let mut end = first.text_range().end();
    let mut segments = 1;
    loop {
        let (Some(a), Some(b)) = (elements.get(at).and_then(|it| it.as_token()), elements.get(at + 1)) else { break };
        let sep = match a.kind() {
            SyntaxKind::COLON2 => 1,
            SyntaxKind::COLON
                if b.as_token().is_some_and(|it| it.kind() == SyntaxKind::COLON && it.text_range().start() == a.text_range().end()) =>
            {
                2
            }
            _ => break,
        };
        let Some(next) = elements.get(at + sep).and_then(|it| it.as_token()).filter(|it| is_segment(it.kind())) else {
            break;
        };
        end = next.text_range().end();
        at += sep + 1;
        segments += 1;
    }
    if let Some(bang) = elements.get(at).and_then(|it| it.as_token())
        && bang.kind() == SyntaxKind::BANG
        && let Some(NodeOrToken::Node(args)) = elements.get(at + 1)
        && args.kind() == SyntaxKind::TOKEN_TREE
    {
        return Some(Statement {
            descr: "macro call",
            range: TextRange::new(first.text_range().start(), args.text_range().end()),
            negated: None,
            sentence_end: None,
        });
    }
    let range = TextRange::new(first.text_range().start(), end);
    if segments > 1 {
        return Some(Statement { descr: "expression", range, negated: None, sentence_end: None });
    }
    let mut sentence_end = end;
    for element in &elements[at..] {
        let Some(token) = element.as_token() else { break };
        if token.kind() == SyntaxKind::IDENT || is_lit(token.kind()) || token.kind() == SyntaxKind::DOT {
            sentence_end = token.text_range().end();
        } else {
            break;
        }
    }
    Some(Statement { descr: "expression", range, negated: None, sentence_end: Some(sentence_end) })
}

/// Whether a token can be a segment of a meta item's path: an identifier (a keyword written
/// as one: `r#fn`), or `self`, `super`, `crate`, `Self`.
fn is_segment(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::IDENT | SyntaxKind::SELF_KW | SyntaxKind::SUPER_KW | SyntaxKind::CRATE_KW | SyntaxKind::SELF_TYPE_KW
    )
}

/// Whether a token is a literal: rustc's `eat_token_lit`.
fn is_lit(kind: SyntaxKind) -> bool {
    kind.is_literal() || matches!(kind, SyntaxKind::TRUE_KW | SyntaxKind::FALSE_KW)
}

/// The keywords rustc reserves and does not use (`TokenDescription::ReservedKeyword`).
fn is_reserved_keyword(text: &str, edition: Edition) -> bool {
    matches!(
        text,
        "abstract" | "become" | "box" | "do" | "final" | "macro" | "override" | "priv" | "typeof" | "unsized" | "virtual" | "yield"
    ) || (edition >= Edition::Edition2018 && text == "try")
        || (edition >= Edition::Edition2024 && text == "gen")
}

/// The suffix written after a string's or char's closing quote (and a raw string's `#`s).
pub(super) fn suffix(text: &str) -> Option<&str> {
    let close = text.rfind(['"', '\''])?;
    let mut rest = &text[close + 1..];
    if let Some(raw) = text.trim_start_matches(['b', 'c']).strip_prefix('r') {
        let hashes = raw.len() - raw.trim_start_matches('#').len();
        rest = &rest[hashes.min(rest.len())..];
    }
    (!rest.is_empty()).then_some(rest)
}

/// rustc's `fix_base_capitalisation`: `0X1F` written for `0x1F`.
fn fix_base_capitalisation(symbol: &str, suffix: &str) -> Option<String> {
    let mut chars = suffix.chars();
    let base_char = chars.next()?;
    let base = match base_char {
        'B' => 2,
        'O' => 8,
        'X' => 16,
        _ => return None,
    };
    let valid = symbol == "0"
        && chars.filter(|it| *it != '_').take_while(|it| *it != 'i' && *it != 'u').all(|it| it.to_digit(base).is_some());
    // alloc: the error's suggestion
    valid.then(|| format!("0{}{}", base_char.to_ascii_lowercase(), &suffix[1..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_capitalised_base_prefix_is_fixed() {
        assert_eq!(fix_base_capitalisation("0", "X1F").as_deref(), Some("0x1F"));
        assert_eq!(fix_base_capitalisation("0", "B12"), None);
        assert_eq!(fix_base_capitalisation("1", "X1"), None);
    }

    #[test]
    fn a_suffix_after_a_raw_strings_hashes_is_its_suffix() {
        assert_eq!(suffix(r#""a"suf"#), Some("suf"));
        assert_eq!(suffix(r#""a""#), None);
        assert_eq!(suffix(r##"r#"a"#"##), None);
        assert_eq!(suffix(r##"br#"a"#x"##), Some("x"));
        assert_eq!(suffix("'c'"), None);
    }

    #[test]
    /// Each token tree's elements between its delimiters, trivia left out and a nested tree one
    /// element, and whether it is in parentheses, in the order of the source.
    #[test]
    fn a_token_trees_elements_are_those_between_its_delimiters() {
        use syntax::{SourceFile, view::AstNode as _};

        let text = "#[a(b = 1, c::d(e), -2 x, [f], m!(g))]\n#[h[i]]\n#[j{ k }]\n#[l()]\nfn f() {}\n";
        let parse = SourceFile::parse(text, Edition::Edition2021);
        let trees: Vec<_> = parse
            .root()
            .descendants()
            .filter_map(ast::TokenTree::cast)
            .map(|tt| {
                let elements = inside(tt);
                let joined: String = elements
                    .iter()
                    .map(|it| match it {
                        NodeOrToken::Token(token) => token.text(),
                        NodeOrToken::Node(node) => node.text(),
                    })
                    .collect();
                let nested = elements.iter().filter(|it| it.kind() == SyntaxKind::TOKEN_TREE).count();
                assert!(elements.iter().all(|it| !it.kind().is_trivia()));
                (tt.syntax().text(), joined, nested, is_paren(tt))
            })
            .collect();
        let expected = [
            ("(b = 1, c::d(e), -2 x, [f], m!(g))", "b=1,c::d(e),-2x,[f],m!(g)", 3, true),
            ("(e)", "e", 0, true),
            ("[f]", "f", 0, false),
            ("(g)", "g", 0, true),
            ("[i]", "i", 0, false),
            ("{ k }", "k", 0, false),
            ("()", "", 0, true),
        ];
        let expected: Vec<_> = expected.iter().map(|&(tt, inside, nested, paren)| (tt, inside.to_owned(), nested, paren)).collect();
        assert_eq!(trees, expected);
    }

    #[test]
    fn reserved_keywords_follow_the_edition() {
        assert!(is_reserved_keyword("box", Edition::Edition2015));
        assert!(!is_reserved_keyword("try", Edition::Edition2015));
        assert!(is_reserved_keyword("try", Edition::Edition2018));
        assert!(!is_reserved_keyword("fn", Edition::Edition2021));
    }
}
