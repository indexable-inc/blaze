//! The parser: text to exactly one [`Statement`] of the subset, by recursive descent over the
//! lexer's tokens with one token of lookahead (two where `t.*`, `not in`, `not between` and
//! `not like` need it).
//!
//! Precedence, loosest first: `or`; `and`; prefix `not`; one comparison (`= <> != < <= > >=`,
//! `is [not] null`, `is [not] expr`, `[not] in`, `[not] between`, `[not] like`), which does not
//! chain; `+ -`; `* / %`; `||`; prefix `-`; a primary. A run of one level's operators
//! (`or`; `and`; `+ -`; `* / %`; `||`) is one flat [`ExprKind::Chain`], meaning the
//! left-nested tree; parentheses keep a chain inside another. Expressions are parsed by
//! precedence climbing in one recursive function, `expr_at`, whose calls are counted against
//! [`MAX_NESTING`]; a tree is at most
//! [`MAX_DEPTH`] deep. Both bounds are set so that parsing, printing, comparing and dropping
//! a tree fit a small stack (see [`MAX_DEPTH`]).

#![blaze::sans_io]

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::alloc::Allocator;
use core::fmt;

use crate::ast::{
    self, Assignment, CallArgs, CaseArm, ChainLink, ChainOp, CompareOp, Delete, Expr, ExprKind,
    Insert, Join, JoinKind, Name, OrderTerm, ResultColumn, Select, Span, Statement, TableRef,
    UnaryOp, Update,
};
use crate::desc::Storage;
use crate::lex::{Keyword, Lexer, Punct, Tok, Token};

/// The deepest expression tree the subset has: a leaf is 1 deep, and a node one more than its
/// deepest operand. Deeper text is refused as [`SyntaxErrorKind::NestedTooDeeply`].
///
/// A run of one level's left-associative operators is one [`ExprKind::Chain`], one level above
/// its deepest operand however long it is, and the parser takes it in a loop: a chain's length
/// costs neither depth nor stack (only [`TEXT_MOST`] bounds it). Depth comes only from operands
/// inside operands: prefixes, parentheses, comparisons, `case`, `cast` and calls.
///
/// The bound is set by stack: a statement at it parses, prints and compares within
/// [`STACK_MOST`], measured by the test `a_tree_at_the_bound_uses_at_most_stack_most`. Measured
/// at 64 in a debug build, the worst phase was a parse of nested `-(` at about 7.9 KiB a level
/// (two calls of `expr_at` a level), `case` in `case` about 7 KiB, printing about 0.6 KiB and
/// [`crate::same`] about 1 KiB; at 32 the worst is about half of [`STACK_MOST`], which leaves
/// room for the readers of the tree (inference, rendering) that measured more. Dropping a tree
/// is drop glue of a few tens of bytes a level and is not measured by the probe.
pub const MAX_DEPTH: u32 = 32;

/// The longest text [`parse`] takes, in bytes; longer is refused first, as
/// [`SyntaxErrorKind::TextTooLong`]. With [`MAX_DEPTH`] bounding nesting, this is what bounds
/// the length of a chain, an `in` list or a list of columns.
pub const TEXT_MOST: usize = 1 << 20;

/// The stack, in bytes, that parsing, printing or comparing ([`crate::same`]) one statement
/// at [`MAX_DEPTH`] may use in an unoptimized build: a thread that runs them needs this much
/// beyond its own frames. The test `a_tree_at_the_bound_uses_at_most_stack_most` measures each
/// phase on texts at the bound, and on long chains to show their length costs no stack, and
/// fails naming every one over it. Dropping a tree is not measured by the probe.
pub const STACK_MOST: usize = 512 * 1024;

/// The deepest the parser's one recursion, `expr_at`, goes. Printed text nests at most two
/// calls per level of its tree (an operand and the parenthesis around it), plus the top one,
/// so every tree of [`MAX_DEPTH`] reads back.
const MAX_NESTING: u32 = 2 * MAX_DEPTH + 1;

// How tightly each operator binds, loosest first.
const OR: u8 = 1;
const AND: u8 = 2;
const NOT: u8 = 3;
const COMPARE: u8 = 4;
const ADD: u8 = 5;
const MUL: u8 = 6;
const CONCAT: u8 = 7;
const NEG: u8 = 8;

/// Why text is not one statement of the subset: what, where, and the rule.
#[derive(Debug)]
pub struct SyntaxError<'s> {
    pub kind: SyntaxErrorKind<'s>,
    pub span: Span,
}

/// Each refusal, with the source text it is about where there is one.
#[derive(Debug)]
pub enum SyntaxErrorKind<'s> {
    /// The text is longer than [`TEXT_MOST`] bytes.
    TextTooLong { bytes: usize },
    /// `--` or `/*`: comments are not part of the subset.
    Comment(&'s str),
    /// A byte that starts no token of the subset.
    UnexpectedByte(u8),
    /// `"name` with no closing `"`; the text from the quote to the end.
    UnterminatedName(&'s str),
    /// `""`.
    EmptyName,
    /// `"a""b"`: a quoted name has no way to hold a `"`.
    QuoteInName(&'s str),
    /// `'text` with no closing `'`; the text from the quote to the end.
    UnterminatedText(&'s str),
    /// `x'..` with no closing `'`.
    UnterminatedBlob(&'s str),
    /// `x'..'` holding a byte that is not a hex digit.
    BlobNotHex(&'s str),
    /// `x'..'` with an odd count of hex digits.
    BlobOddDigits(&'s str),
    /// An integer literal above `u64::MAX`.
    IntegerTooLarge(&'s str),
    /// A real literal whose value is not finite (`1e400`).
    RealNotFinite(&'s str),
    /// Digits run into a name, a second `.`, or an exponent without digits (`12ab`, `1e`).
    MalformedNumber(&'s str),
    /// A real literal of the lexer's shape that `f64`'s parser still refused, with its error.
    RealUnreadable { text: &'s str, error: core::num::ParseFloatError },
    /// `?`
    AnonymousParameter(&'s str),
    /// `?1`
    NumberedQuestionParameter(&'s str),
    /// `$1`
    NumberedDollarParameter(&'s str),
    /// `:name`
    ColonParameter(&'s str),
    /// `@name`
    AtParameter(&'s str),
    /// `$` with no name after it.
    NamelessParameter,
    /// A keyword where a name or an expression was expected; quoted it is a name.
    ReservedWord(&'s str),
    /// The grammar wanted `expected` and the text has `found` (empty at the end of the text).
    Expected { expected: &'static str, found: &'s str },
    /// Anything after the statement's `;`: the text is one statement.
    SecondStatement(&'s str),
    /// A second comparison on a comparison, such as the second `=` of `a = b = c`.
    ChainedComparison(&'s str),
    /// An expression deeper than [`MAX_DEPTH`].
    NestedTooDeeply,
    /// `in ()`.
    EmptyInList,
    /// A `cast` to a type that is not `boolean`, `integer`, `real`, `text` or `blob`.
    UnknownCastType(&'s str),
    /// `offset` without `limit`: SQLite has no `offset` alone.
    OffsetWithoutLimit,
}

/// Writes `text` cut at 40 bytes (on a char boundary), so a message about an unterminated
/// literal does not repeat the rest of the statement.
fn excerpt(f: &mut fmt::Formatter<'_>, text: &str) -> fmt::Result {
    const MOST: usize = 40;
    if text.len() <= MOST {
        return f.write_str(text);
    }
    let mut cut = MOST;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    f.write_str(text.get(..cut).unwrap_or(""))?;
    f.write_str("...")
}

/// The end of a refusal of a text or blob literal with no closing `'`, after its excerpt.
fn no_closing_quote(f: &mut fmt::Formatter<'_>, at: u32) -> fmt::Result {
    write!(f, "` at byte {at} has no closing `'`")
}

impl fmt::Display for SyntaxError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let at = self.span.start;
        match self.kind {
            SyntaxErrorKind::TextTooLong { bytes } => {
                write!(f, "the text is {bytes} bytes: a statement is at most {TEXT_MOST} bytes")
            }
            SyntaxErrorKind::Comment(token) => {
                write!(f, "comment `{token}` at byte {at}: comments are not part of the subset")
            }
            SyntaxErrorKind::UnexpectedByte(byte) => {
                if byte.is_ascii_graphic() {
                    write!(f, "`{}` at byte {at} is not a token of the subset", char::from(byte))
                } else {
                    write!(f, "byte `\\x{byte:02X}` at byte {at} is not a token of the subset")
                }
            }
            SyntaxErrorKind::UnterminatedName(text) => {
                f.write_str("quoted name `")?;
                excerpt(f, text)?;
                write!(f, "` at byte {at} has no closing `\"`")
            }
            SyntaxErrorKind::EmptyName => {
                write!(
                    f,
                    "quoted name `\"\"` at byte {at} is empty: a name has at least one character"
                )
            }
            SyntaxErrorKind::QuoteInName(text) => {
                f.write_str("quoted name `")?;
                excerpt(f, text)?;
                write!(f, "` at byte {at} holds a `\"`: a quoted name cannot hold one")
            }
            SyntaxErrorKind::UnterminatedText(text) => {
                f.write_str("text literal `")?;
                excerpt(f, text)?;
                no_closing_quote(f, at)
            }
            SyntaxErrorKind::UnterminatedBlob(text) => {
                f.write_str("blob literal `")?;
                excerpt(f, text)?;
                no_closing_quote(f, at)
            }
            SyntaxErrorKind::BlobNotHex(text) => {
                f.write_str("blob literal `")?;
                excerpt(f, text)?;
                write!(f, "` at byte {at} holds a character that is not a hex digit")
            }
            SyntaxErrorKind::BlobOddDigits(text) => {
                f.write_str("blob literal `")?;
                excerpt(f, text)?;
                write!(f, "` at byte {at} has an odd count of hex digits: two make a byte")
            }
            SyntaxErrorKind::IntegerTooLarge(text) => {
                f.write_str("integer `")?;
                excerpt(f, text)?;
                write!(f, "` at byte {at} is above {}", u64::MAX)
            }
            SyntaxErrorKind::RealNotFinite(text) => {
                f.write_str("real `")?;
                excerpt(f, text)?;
                write!(f, "` at byte {at} is not a finite number")
            }
            SyntaxErrorKind::MalformedNumber(text) => {
                f.write_str("number `")?;
                excerpt(f, text)?;
                write!(
                    f,
                    "` at byte {at} is malformed: a number is digits, an optional `.` and digits, \
                     and an optional exponent, followed by neither a name nor a `.`"
                )
            }
            SyntaxErrorKind::RealUnreadable { text, ref error } => {
                f.write_str("real `")?;
                excerpt(f, text)?;
                write!(f, "` at byte {at} does not read as a number: {error}")
            }
            SyntaxErrorKind::AnonymousParameter(token)
            | SyntaxErrorKind::NumberedQuestionParameter(token)
            | SyntaxErrorKind::NumberedDollarParameter(token) => write!(
                f,
                "positional parameter `{token}` at byte {at}: parameters are named, write `$name`"
            ),
            SyntaxErrorKind::ColonParameter(token) => {
                write!(
                    f,
                    "parameter `{token}` at byte {at}: parameters are written `$name`, not `:name`"
                )
            }
            SyntaxErrorKind::AtParameter(token) => {
                write!(
                    f,
                    "parameter `{token}` at byte {at}: parameters are written `$name`, not `@name`"
                )
            }
            SyntaxErrorKind::NamelessParameter => {
                write!(f, "`$` at byte {at} has no name: parameters are named, write `$name`")
            }
            SyntaxErrorKind::ReservedWord(word) => write!(
                f,
                "`{word}` at byte {at} is a reserved word where a name or an expression was \
                 expected: to use it as a name, quote it as `\"{word}\"`"
            ),
            SyntaxErrorKind::Expected { expected, found } => {
                write!(f, "expected {expected} at byte {at}, found ")?;
                if found.is_empty() {
                    f.write_str("the end of the text")
                } else {
                    f.write_str("`")?;
                    excerpt(f, found)?;
                    f.write_str("`")
                }
            }
            SyntaxErrorKind::SecondStatement(token) => {
                f.write_str("second statement at byte ")?;
                write!(f, "{at}, starting `")?;
                excerpt(f, token)?;
                f.write_str("`: the text is one statement, with at most one `;` after it")
            }
            SyntaxErrorKind::ChainedComparison(token) => write!(
                f,
                "comparison `{token}` at byte {at} follows another comparison: comparisons do not \
                 chain, parenthesize one of them"
            ),
            SyntaxErrorKind::NestedTooDeeply => write!(
                f,
                "expression at byte {at} is nested too deeply: the subset allows {MAX_DEPTH} levels"
            ),
            SyntaxErrorKind::EmptyInList => {
                write!(f, "`in ()` at byte {at} has no entries: an `in` list has at least one")
            }
            SyntaxErrorKind::UnknownCastType(name) => write!(
                f,
                "`{name}` at byte {at} is not a type of `cast`: write `boolean`, `integer`, \
                 `real`, `text` or `blob`"
            ),
            SyntaxErrorKind::OffsetWithoutLimit => write!(
                f,
                "`offset` at byte {at} has no `limit` before it: write `limit .. offset ..`"
            ),
        }
    }
}

/// Parses `text` as exactly one statement of the subset.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn parse<'s, A: Allocator + Copy>(
    text: &'s str,
    alloc: A,
) -> Result<Statement<'s, A>, SyntaxError<'s>> {
    // Before anything else, and below `u32::MAX`, so every span fits a `u32`.
    if text.len() > TEXT_MOST {
        return Err(SyntaxError {
            kind: SyntaxErrorKind::TextTooLong { bytes: text.len() },
            span: Span { start: 0, end: 0 },
        });
    }
    let mut lex = Lexer::new(text);
    let tok = lex.next_token()?;
    let mut parser = Parser { text, lex, tok, alloc, depth: 0 };
    let statement = parser.statement()?;
    parser.end()?;
    Ok(statement)
}

type Parsed<'s, T> = Result<T, SyntaxError<'s>>;

fn error(kind: SyntaxErrorKind<'_>, span: Span) -> SyntaxError<'_> {
    SyntaxError { kind, span }
}

fn cover(first: Span, last: Span) -> Span {
    Span { start: first.start, end: last.end }
}

/// An expression, in the box it was built in, and how deep its tree is (a leaf is 1).
struct Node<'s, A: Allocator> {
    expr: Box<Expr<'s, A>, A>,
    height: u32,
}

/// The value `key` maps to in `table`, if any.
fn lookup<K: PartialEq + Copy, V: Copy>(table: &[(K, V)], key: K) -> Option<V> {
    table.iter().find(|&&(it, _)| it == key).map(|&(_, value)| value)
}

/// Where a comparison starts, before its tokens are taken.
#[derive(Clone, Copy)]
enum Comparison {
    Op(CompareOp),
    Is,
    In { negated: bool },
    Between { negated: bool },
    Like { negated: bool },
}

impl Comparison {
    fn negated(self) -> bool {
        match self {
            Comparison::In { negated }
            | Comparison::Between { negated }
            | Comparison::Like { negated } => negated,
            Comparison::Op(
                CompareOp::Eq
                | CompareOp::Ne
                | CompareOp::Lt
                | CompareOp::Le
                | CompareOp::Gt
                | CompareOp::Ge
                | CompareOp::Is
                | CompareOp::IsNot,
            )
            | Comparison::Is => false,
        }
    }
}

struct Parser<'s, A: Allocator + Copy> {
    text: &'s str,
    lex: Lexer<'s>,
    /// The current token, not yet taken.
    tok: Token<'s>,
    alloc: A,
    /// Calls of [`Parser::expr_at`] now open: the parser's only recursion.
    depth: u32,
}

impl<'s, A: Allocator + Copy> Parser<'s, A> {
    // Tokens.

    /// Takes the current token and lexes the next.
    fn bump(&mut self) -> Parsed<'s, Token<'s>> {
        let token = self.tok;
        self.tok = self.lex.next_token()?;
        Ok(token)
    }

    /// The token after the current one.
    fn peek(&self) -> Parsed<'s, Token<'s>> {
        let mut lex = self.lex;
        lex.next_token()
    }

    fn source(&self, span: Span) -> &'s str {
        self.text.get(span.range()).unwrap_or("")
    }

    fn at_keyword(&self, keyword: Keyword) -> bool {
        matches!(self.tok.tok, Tok::Keyword(it) if it == keyword)
    }

    fn at_punct(&self, punct: Punct) -> bool {
        matches!(self.tok.tok, Tok::Punct(it) if it == punct)
    }

    fn at_name(&self) -> bool {
        matches!(self.tok.tok, Tok::Ident(_) | Tok::Quoted(_))
    }

    fn at_end(&self) -> bool {
        matches!(self.tok.tok, Tok::End)
    }

    /// Takes the current token if it is `keyword`, and gives its span.
    fn eat_keyword(&mut self, keyword: Keyword) -> Parsed<'s, Option<Span>> {
        if self.at_keyword(keyword) { Ok(Some(self.bump()?.span)) } else { Ok(None) }
    }

    fn eat_punct(&mut self, punct: Punct) -> Parsed<'s, Option<Span>> {
        if self.at_punct(punct) { Ok(Some(self.bump()?.span)) } else { Ok(None) }
    }

    fn expect_keyword(&mut self, keyword: Keyword, expected: &'static str) -> Parsed<'s, Span> {
        match self.eat_keyword(keyword)? {
            Some(span) => Ok(span),
            None => Err(self.expected(expected)),
        }
    }

    fn expect_punct(&mut self, punct: Punct, expected: &'static str) -> Parsed<'s, Span> {
        match self.eat_punct(punct)? {
            Some(span) => Ok(span),
            None => Err(self.expected(expected)),
        }
    }

    fn expected(&self, expected: &'static str) -> SyntaxError<'s> {
        let found = self.source(self.tok.span);
        error(SyntaxErrorKind::Expected { expected, found }, self.tok.span)
    }

    fn reserved(&self) -> SyntaxError<'s> {
        error(SyntaxErrorKind::ReservedWord(self.source(self.tok.span)), self.tok.span)
    }

    /// An identifier, quoted or not.
    fn name(&mut self, expected: &'static str) -> Parsed<'s, Name<'s>> {
        let token = self.tok;
        let (text, quoted) = if let Tok::Ident(text) = token.tok {
            (text, false)
        } else if let Tok::Quoted(text) = token.tok {
            (text, true)
        } else if token.tok.keyword().is_some() {
            return Err(self.reserved());
        } else {
            return Err(self.expected(expected));
        };
        self.bump()?;
        Ok(Name { text, quoted, span: token.span })
    }

    // Statements.

    fn statement(&mut self) -> Parsed<'s, Statement<'s, A>> {
        if self.at_keyword(Keyword::Select) {
            Ok(Statement::Select(self.select()?))
        } else if self.at_keyword(Keyword::Insert) {
            Ok(Statement::Insert(self.insert()?))
        } else if self.at_keyword(Keyword::Update) {
            Ok(Statement::Update(self.update()?))
        } else if self.at_keyword(Keyword::Delete) {
            Ok(Statement::Delete(self.delete()?))
        } else {
            Err(self.expected("a statement: `select`, `insert`, `update` or `delete`"))
        }
    }

    /// The end of the text, after at most one `;`.
    fn end(&mut self) -> Parsed<'s, ()> {
        if self.eat_punct(Punct::Semicolon)?.is_some() {
            if self.at_end() {
                return Ok(());
            }
            return Err(error(
                SyntaxErrorKind::SecondStatement(self.source(self.tok.span)),
                self.tok.span,
            ));
        }
        if self.at_end() {
            return Ok(());
        }
        Err(self.expected("the end of the statement (`;` or the end of the text)"))
    }

    fn select(&mut self) -> Parsed<'s, Select<'s, A>> {
        self.bump()?;
        let distinct = self.eat_keyword(Keyword::Distinct)?.is_some();
        let columns = self.result_columns()?;
        let from =
            if self.eat_keyword(Keyword::From)?.is_some() { Some(self.from()?) } else { None };
        let filter = self.clause(Keyword::Where)?;
        let mut group_by = Vec::new_in(self.alloc);
        if self.eat_keyword(Keyword::Group)?.is_some() {
            self.expect_keyword(Keyword::By, "`by` after `group`")?;
            loop {
                group_by.push(self.top()?);
                if self.eat_punct(Punct::Comma)?.is_none() {
                    break;
                }
            }
        }
        let having = self.clause(Keyword::Having)?;
        let mut order_by = Vec::new_in(self.alloc);
        if self.eat_keyword(Keyword::Order)?.is_some() {
            self.expect_keyword(Keyword::By, "`by` after `order`")?;
            loop {
                let expr = self.top()?;
                let descending = if self.eat_keyword(Keyword::Desc)?.is_some() {
                    true
                } else {
                    self.eat_keyword(Keyword::Asc)?;
                    false
                };
                order_by.push(OrderTerm { expr, descending });
                if self.eat_punct(Punct::Comma)?.is_none() {
                    break;
                }
            }
        }
        let limit = self.clause(Keyword::Limit)?;
        let offset = match self.eat_keyword(Keyword::Offset)? {
            Some(span) if limit.is_none() => {
                return Err(error(SyntaxErrorKind::OffsetWithoutLimit, span));
            }
            Some(_) => Some(self.top()?),
            None => None,
        };
        Ok(Select { distinct, columns, from, filter, group_by, having, order_by, limit, offset })
    }

    /// `keyword expr`, if the current token is `keyword`.
    fn clause(&mut self, keyword: Keyword) -> Parsed<'s, Option<Expr<'s, A>>> {
        if self.eat_keyword(keyword)?.is_some() { Ok(Some(self.top()?)) } else { Ok(None) }
    }

    /// A `select` or `returning` list: one entry or more.
    fn result_columns(&mut self) -> Parsed<'s, Vec<ResultColumn<'s, A>, A>> {
        let mut columns = Vec::new_in(self.alloc);
        loop {
            columns.push(self.result_column()?);
            if self.eat_punct(Punct::Comma)?.is_none() {
                return Ok(columns);
            }
        }
    }

    fn result_column(&mut self) -> Parsed<'s, ResultColumn<'s, A>> {
        if let Some(span) = self.eat_punct(Punct::Star)? {
            return Ok(ResultColumn::Star { span });
        }
        if self.at_name() {
            let mut ahead = self.lex;
            if matches!(ahead.next_token()?.tok, Tok::Punct(Punct::Dot))
                && matches!(ahead.next_token()?.tok, Tok::Punct(Punct::Star))
            {
                let table = self.name("a table name")?;
                self.bump()?;
                self.bump()?;
                return Ok(ResultColumn::TableStar { table });
            }
        }
        let expr = self.top()?;
        let alias = if self.eat_keyword(Keyword::As)?.is_some() {
            Some(self.name("a name after `as`")?)
        } else {
            None
        };
        Ok(ResultColumn::Expr { expr, alias })
    }

    fn from(&mut self) -> Parsed<'s, ast::From<'s, A>> {
        let first = self.table_ref()?;
        let mut joins = Vec::new_in(self.alloc);
        loop {
            let kind = if self.eat_keyword(Keyword::Join)?.is_some() {
                JoinKind::Inner
            } else if self.eat_keyword(Keyword::Inner)?.is_some() {
                self.expect_keyword(Keyword::Join, "`join` after `inner`")?;
                JoinKind::Inner
            } else if self.eat_keyword(Keyword::Left)?.is_some() {
                self.eat_keyword(Keyword::Outer)?;
                self.expect_keyword(Keyword::Join, "`join` after `left` or `left outer`")?;
                JoinKind::Left
            } else {
                return Ok(ast::From { first, joins });
            };
            let table = self.table_ref()?;
            self.expect_keyword(Keyword::On, "`on` and the join's condition")?;
            let on = self.top()?;
            joins.push(Join { kind, table, on });
        }
    }

    /// `table`, `table alias` or `table as alias`.
    fn table_ref(&mut self) -> Parsed<'s, TableRef<'s>> {
        let table = self.name("a table name")?;
        let alias = if self.eat_keyword(Keyword::As)?.is_some() {
            Some(self.name("an alias after `as`")?)
        } else if self.at_name() {
            Some(self.name("an alias")?)
        } else {
            None
        };
        Ok(TableRef { table, alias })
    }

    fn insert(&mut self) -> Parsed<'s, Insert<'s, A>> {
        self.bump()?;
        self.expect_keyword(Keyword::Into, "`into` after `insert`")?;
        let table = self.name("a table name")?;
        self.expect_punct(Punct::LParen, "`(` and the column list: an `insert` names its columns")?;
        let mut columns = Vec::new_in(self.alloc);
        loop {
            columns.push(self.name("a column name")?);
            if self.eat_punct(Punct::Comma)?.is_none() {
                break;
            }
        }
        self.expect_punct(Punct::RParen, "`,` or `)` in the column list")?;
        self.expect_keyword(Keyword::Values, "`values` after the column list")?;
        let mut rows = Vec::new_in(self.alloc);
        loop {
            self.expect_punct(Punct::LParen, "`(` and a row of values")?;
            let mut row = Vec::new_in(self.alloc);
            loop {
                row.push(self.top()?);
                if self.eat_punct(Punct::Comma)?.is_none() {
                    break;
                }
            }
            self.expect_punct(Punct::RParen, "`,` or `)` in a row of values")?;
            rows.push(row);
            if self.eat_punct(Punct::Comma)?.is_none() {
                break;
            }
        }
        let returning = self.returning()?;
        Ok(Insert { table, columns, rows, returning })
    }

    fn update(&mut self) -> Parsed<'s, Update<'s, A>> {
        self.bump()?;
        let table = self.name("a table name")?;
        self.expect_keyword(Keyword::Set, "`set` after the table")?;
        let mut sets = Vec::new_in(self.alloc);
        loop {
            let column = self.name("a column name")?;
            self.expect_punct(Punct::Eq, "`=` after the column of a `set`")?;
            let value = self.top()?;
            sets.push(Assignment { column, value });
            if self.eat_punct(Punct::Comma)?.is_none() {
                break;
            }
        }
        let filter = self.clause(Keyword::Where)?;
        let returning = self.returning()?;
        Ok(Update { table, sets, filter, returning })
    }

    fn delete(&mut self) -> Parsed<'s, Delete<'s, A>> {
        self.bump()?;
        self.expect_keyword(Keyword::From, "`from` after `delete`")?;
        let table = self.name("a table name")?;
        let filter = self.clause(Keyword::Where)?;
        let returning = self.returning()?;
        Ok(Delete { table, filter, returning })
    }

    fn returning(&mut self) -> Parsed<'s, Vec<ResultColumn<'s, A>, A>> {
        if self.eat_keyword(Keyword::Returning)?.is_some() {
            self.result_columns()
        } else {
            Ok(Vec::new_in(self.alloc))
        }
    }

    // Expressions, by precedence climbing: `expr_at` is the one recursive function, and every
    // tree is built in boxes where its node is made, so a level of nesting moves a `Node` (a
    // box and a height) and never an `Expr`.

    /// A node over operands at most `below` deep.
    fn node(&self, kind: ExprKind<'s, A>, span: Span, below: u32) -> Parsed<'s, Node<'s, A>> {
        let height = below.saturating_add(1);
        if height > MAX_DEPTH {
            return Err(error(SyntaxErrorKind::NestedTooDeeply, span));
        }
        Ok(Node { expr: Box::new_in(Expr { kind, span }, self.alloc), height })
    }

    fn leaf(&self, kind: ExprKind<'s, A>, span: Span) -> Node<'s, A> {
        Node { expr: Box::new_in(Expr { kind, span }, self.alloc), height: 1 }
    }

    fn compare(
        &self,
        op: CompareOp,
        left: Node<'s, A>,
        right: Node<'s, A>,
    ) -> Parsed<'s, Node<'s, A>> {
        let span = cover(left.expr.span, right.expr.span);
        let below = left.height.max(right.height);
        self.node(ExprKind::Compare { op, left: left.expr, right: right.expr }, span, below)
    }

    /// `first` and the operators of `level` (a binding power) that follow it, as one
    /// [`ExprKind::Chain`]; the current token is the chain's first operator. Each operand
    /// binds strictly tighter than `level`, so the chain stops at the first operator of
    /// another level, and the loop, not recursion, takes its length.
    fn chain(&mut self, first: Node<'s, A>, level: u8) -> Parsed<'s, Node<'s, A>> {
        #[cfg(test)]
        crate::tests::stack::touch();
        let mut below = first.height;
        let mut end = first.expr.span;
        let mut rest = Vec::new_in(self.alloc);
        while let Some(op) = self.chain_op() {
            if power(op) != level {
                break;
            }
            self.bump()?;
            let operand = self.expr_at(level + 1)?;
            below = below.max(operand.height);
            end = operand.expr.span;
            rest.push(ChainLink { op, operand: *operand.expr });
        }
        let span = cover(first.expr.span, end);
        self.node(ExprKind::Chain { first: first.expr, rest }, span, below)
    }

    fn unary(&self, op: UnaryOp, start: Span, operand: Node<'s, A>) -> Parsed<'s, Node<'s, A>> {
        let span = cover(start, operand.expr.span);
        self.node(ExprKind::Unary { op, operand: operand.expr }, span, operand.height)
    }

    /// Opens one level of [`Parser::expr_at`], refusing past [`MAX_NESTING`] of them.
    fn enter(&mut self) -> Parsed<'s, ()> {
        if self.depth >= MAX_NESTING {
            return Err(error(SyntaxErrorKind::NestedTooDeeply, self.tok.span));
        }
        self.depth += 1;
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// An expression where a whole one stands (a clause, a list entry), out of its box.
    fn top(&mut self) -> Parsed<'s, Expr<'s, A>> {
        let node = self.expr_at(OR)?;
        Ok(*node.expr)
    }

    /// An expression whose operators all bind at least as tightly as `min`.
    fn expr_at(&mut self, min: u8) -> Parsed<'s, Node<'s, A>> {
        #[cfg(test)]
        crate::tests::stack::touch();
        self.enter()?;
        let mut left = self.operand(min)?;
        // Every turn takes at least one token, so the text's length bounds the loop.
        loop {
            if let Some(op) = self.chain_op() {
                let level = power(op);
                if level < min {
                    break;
                }
                // Left associative, and flat: every operator of this level after `left`
                // goes into one chain. Its operands bind strictly tighter, so what stops it
                // is a looser operator, which the next turn of this loop takes.
                left = self.chain(left, level)?;
            } else if let Some(comparison) = self.comparison_start()? {
                if COMPARE < min {
                    break;
                }
                left = self.finish_comparison(left, comparison)?;
                self.after_comparison()?;
            } else {
                break;
            }
        }
        self.leave();
        Ok(left)
    }

    /// After a comparison only a looser operator (`and`, `or`) may follow: a second
    /// comparison is a chain, and `a is null + 1` has no reading in the subset.
    fn after_comparison(&self) -> Parsed<'s, ()> {
        if self.comparison_start()?.is_some() {
            let span = self.tok.span;
            return Err(error(SyntaxErrorKind::ChainedComparison(self.source(span)), span));
        }
        if self.chain_op().is_some_and(|op| power(op) >= COMPARE) {
            return Err(
                self.expected("`and`, `or` or the end of the expression after a comparison")
            );
        }
        Ok(())
    }

    /// The chain operator the current token is, if any; takes nothing. A token is either a
    /// chain operator or starts a comparison ([`Parser::comparison_start`]), never both.
    fn chain_op(&self) -> Option<ChainOp> {
        const BY_KEYWORD: [(Keyword, ChainOp); 2] =
            [(Keyword::Or, ChainOp::Or), (Keyword::And, ChainOp::And)];
        const BY_PUNCT: [(Punct, ChainOp); 6] = [
            (Punct::Plus, ChainOp::Add),
            (Punct::Minus, ChainOp::Sub),
            (Punct::Star, ChainOp::Mul),
            (Punct::Slash, ChainOp::Div),
            (Punct::Percent, ChainOp::Rem),
            (Punct::Concat, ChainOp::Concat),
        ];
        let tok = self.tok.tok;
        if let Some(keyword) = tok.keyword() {
            lookup(&BY_KEYWORD, keyword)
        } else if let Some(punct) = tok.punct() {
            lookup(&BY_PUNCT, punct)
        } else {
            None
        }
    }

    /// The start of an expression: a prefix operator and its operand, a parenthesized
    /// expression, or a primary.
    fn operand(&mut self, min: u8) -> Parsed<'s, Node<'s, A>> {
        #[cfg(test)]
        crate::tests::stack::touch();
        let start = self.tok.span;
        // `not` binds looser than a comparison, so it starts an operand only where nothing
        // tighter than `not` is required (`a = not b` is refused).
        if self.at_keyword(Keyword::Not) && min <= NOT {
            self.bump()?;
            let operand = self.expr_at(NOT)?;
            self.unary(UnaryOp::Not, start, operand)
        } else if self.at_punct(Punct::Minus) {
            self.bump()?;
            let operand = self.expr_at(NEG)?;
            self.unary(UnaryOp::Neg, start, operand)
        } else if self.at_punct(Punct::LParen) {
            self.bump()?;
            let mut inner = self.expr_at(OR)?;
            let close = self.expect_punct(Punct::RParen, "`)`")?;
            inner.expr.span = cover(start, close);
            Ok(inner)
        } else if self.at_keyword(Keyword::Case) {
            self.case()
        } else if self.at_keyword(Keyword::Cast) {
            self.cast()
        } else if self.at_name() {
            self.column_or_call()
        } else {
            self.literal()
        }
    }

    /// A literal or a parameter: a leaf, taken without recursion.
    fn literal(&mut self) -> Parsed<'s, Node<'s, A>> {
        #[cfg(test)]
        crate::tests::stack::touch();
        let token = self.tok;
        let tok = token.tok;
        let kind = if let Tok::Integer(value) = tok {
            ExprKind::Integer(value)
        } else if let Tok::Real(value) = tok {
            ExprKind::Real(value)
        } else if let Tok::Text(text) = tok {
            ExprKind::Text(text)
        } else if let Tok::Blob(digits) = tok {
            ExprKind::Blob(digits)
        } else if let Tok::Param(text) = tok {
            ExprKind::Param(Name { text, quoted: false, span: token.span })
        } else if self.at_keyword(Keyword::Null) {
            ExprKind::Null
        } else if self.at_keyword(Keyword::True) {
            ExprKind::Boolean(true)
        } else if self.at_keyword(Keyword::False) {
            ExprKind::Boolean(false)
        } else if tok.keyword().is_some() {
            return Err(self.reserved());
        } else {
            return Err(self.expected("an expression"));
        };
        self.bump()?;
        Ok(self.leaf(kind, token.span))
    }

    /// The comparison the current tokens start, if any; takes nothing.
    fn comparison_start(&self) -> Parsed<'s, Option<Comparison>> {
        const BY_PUNCT: [(Punct, CompareOp); 6] = [
            (Punct::Eq, CompareOp::Eq),
            (Punct::Ne, CompareOp::Ne),
            (Punct::Lt, CompareOp::Lt),
            (Punct::Le, CompareOp::Le),
            (Punct::Gt, CompareOp::Gt),
            (Punct::Ge, CompareOp::Ge),
        ];
        const BY_KEYWORD: [(Keyword, Comparison); 4] = [
            (Keyword::Is, Comparison::Is),
            (Keyword::In, Comparison::In { negated: false }),
            (Keyword::Between, Comparison::Between { negated: false }),
            (Keyword::Like, Comparison::Like { negated: false }),
        ];
        // After `not`: `is` takes its `not` after it, so it is not here.
        const AFTER_NOT: [(Keyword, Comparison); 3] = [
            (Keyword::In, Comparison::In { negated: true }),
            (Keyword::Between, Comparison::Between { negated: true }),
            (Keyword::Like, Comparison::Like { negated: true }),
        ];
        let tok = self.tok.tok;
        if let Some(punct) = tok.punct() {
            return Ok(lookup(&BY_PUNCT, punct).map(Comparison::Op));
        }
        let Some(keyword) = tok.keyword() else {
            return Ok(None);
        };
        if keyword != Keyword::Not {
            return Ok(lookup(&BY_KEYWORD, keyword));
        }
        Ok(self.peek()?.tok.keyword().and_then(|next| lookup(&AFTER_NOT, next)))
    }

    /// The rest of a comparison on `left`, from its first token. Its operands bind at the
    /// `+ -` level, so `between`'s `and` is its own.
    fn finish_comparison(
        &mut self,
        left: Node<'s, A>,
        comparison: Comparison,
    ) -> Parsed<'s, Node<'s, A>> {
        #[cfg(test)]
        crate::tests::stack::touch();
        let negated = comparison.negated();
        if negated {
            self.bump()?;
        }
        self.bump()?;
        let start = left.expr.span;
        match comparison {
            Comparison::Op(
                op @ (CompareOp::Eq
                | CompareOp::Ne
                | CompareOp::Lt
                | CompareOp::Le
                | CompareOp::Gt
                | CompareOp::Ge
                | CompareOp::Is
                | CompareOp::IsNot),
            ) => {
                let right = self.expr_at(ADD)?;
                self.compare(op, left, right)
            }
            Comparison::Is => {
                let not = self.eat_keyword(Keyword::Not)?.is_some();
                if let Some(null) = self.eat_keyword(Keyword::Null)? {
                    let kind = ExprKind::IsNull { operand: left.expr, negated: not };
                    return self.node(kind, cover(start, null), left.height);
                }
                let right = self.expr_at(ADD)?;
                self.compare(if not { CompareOp::IsNot } else { CompareOp::Is }, left, right)
            }
            Comparison::In { .. } => {
                if let Tok::Param(text) = self.tok.tok {
                    let span = self.bump()?.span;
                    let list = Name { text, quoted: false, span };
                    let kind = ExprKind::InParam { operand: left.expr, list, negated };
                    return self.node(kind, cover(start, span), left.height);
                }
                self.expect_punct(
                    Punct::LParen,
                    "`(` and a list, or a `$list` parameter, after `in`",
                )?;
                if self.at_punct(Punct::RParen) {
                    return Err(error(SyntaxErrorKind::EmptyInList, self.tok.span));
                }
                let mut below = left.height;
                let mut list = Vec::new_in(self.alloc);
                loop {
                    let entry = self.expr_at(OR)?;
                    below = below.max(entry.height);
                    list.push(*entry.expr);
                    if self.eat_punct(Punct::Comma)?.is_none() {
                        break;
                    }
                }
                let close = self.expect_punct(Punct::RParen, "`,` or `)` in the `in` list")?;
                let kind = ExprKind::InList { operand: left.expr, list, negated };
                self.node(kind, cover(start, close), below)
            }
            Comparison::Between { .. } => {
                let low = self.expr_at(ADD)?;
                self.expect_keyword(Keyword::And, "`and` between the bounds of `between`")?;
                let high = self.expr_at(ADD)?;
                let span = cover(start, high.expr.span);
                let below = left.height.max(low.height).max(high.height);
                let kind = ExprKind::Between {
                    operand: left.expr,
                    low: low.expr,
                    high: high.expr,
                    negated,
                };
                self.node(kind, span, below)
            }
            Comparison::Like { .. } => {
                let pattern = self.expr_at(ADD)?;
                let span = cover(start, pattern.expr.span);
                let below = left.height.max(pattern.height);
                let kind = ExprKind::Like { operand: left.expr, pattern: pattern.expr, negated };
                self.node(kind, span, below)
            }
        }
    }

    /// `column`, `table.column` or `function(args)`.
    fn column_or_call(&mut self) -> Parsed<'s, Node<'s, A>> {
        #[cfg(test)]
        crate::tests::stack::touch();
        let first = self.name("a name")?;
        if self.eat_punct(Punct::LParen)?.is_some() {
            return self.call(first);
        }
        if self.eat_punct(Punct::Dot)?.is_some() {
            let column = self.name("a column name after `.`")?;
            let kind = ExprKind::Column { table: Some(first), column };
            return Ok(self.leaf(kind, cover(first.span, column.span)));
        }
        Ok(self.leaf(ExprKind::Column { table: None, column: first }, first.span))
    }

    /// The arguments of a call, after its `(`.
    fn call(&mut self, function: Name<'s>) -> Parsed<'s, Node<'s, A>> {
        #[cfg(test)]
        crate::tests::stack::touch();
        if self.eat_punct(Punct::Star)?.is_some() {
            let close = self.expect_punct(Punct::RParen, "`)` after `*`")?;
            let kind = ExprKind::Call { function, args: CallArgs::Star };
            return self.node(kind, cover(function.span, close), 0);
        }
        let distinct = self.eat_keyword(Keyword::Distinct)?.is_some();
        let mut args = Vec::new_in(self.alloc);
        let mut below = 0;
        if distinct || !self.at_punct(Punct::RParen) {
            loop {
                let arg = self.expr_at(OR)?;
                below = below.max(arg.height);
                args.push(*arg.expr);
                if self.eat_punct(Punct::Comma)?.is_none() {
                    break;
                }
            }
        }
        let close = self.expect_punct(Punct::RParen, "`,` or `)` in the arguments of a call")?;
        let kind = ExprKind::Call { function, args: CallArgs::List { distinct, args } };
        self.node(kind, cover(function.span, close), below)
    }

    /// `case [operand] when .. then .. [when .. then ..]* [else ..] end`.
    fn case(&mut self) -> Parsed<'s, Node<'s, A>> {
        #[cfg(test)]
        crate::tests::stack::touch();
        let start = self.bump()?.span;
        if self.at_keyword(Keyword::End) || self.at_keyword(Keyword::Else) {
            return Err(self.expected("`when`: a `case` has at least one arm"));
        }
        let mut below = 0;
        let operand = if self.at_keyword(Keyword::When) {
            None
        } else {
            let operand = self.expr_at(OR)?;
            below = operand.height;
            Some(operand.expr)
        };
        let mut arms = Vec::new_in(self.alloc);
        while self.eat_keyword(Keyword::When)?.is_some() {
            let when = self.expr_at(OR)?;
            self.expect_keyword(Keyword::Then, "`then` after the condition of a `when`")?;
            let then = self.expr_at(OR)?;
            below = below.max(when.height).max(then.height);
            arms.push(CaseArm { when: *when.expr, then: *then.expr });
        }
        if arms.is_empty() {
            return Err(self.expected("`when`: a `case` has at least one arm"));
        }
        let otherwise = if self.eat_keyword(Keyword::Else)?.is_some() {
            let otherwise = self.expr_at(OR)?;
            below = below.max(otherwise.height);
            Some(otherwise.expr)
        } else {
            None
        };
        let end = self.expect_keyword(Keyword::End, "`when`, `else` or `end` in a `case`")?;
        self.node(ExprKind::Case { operand, arms, otherwise }, cover(start, end), below)
    }

    /// `cast(operand as type)`.
    fn cast(&mut self) -> Parsed<'s, Node<'s, A>> {
        #[cfg(test)]
        crate::tests::stack::touch();
        let start = self.bump()?.span;
        self.expect_punct(Punct::LParen, "`(` after `cast`")?;
        let operand = self.expr_at(OR)?;
        self.expect_keyword(Keyword::As, "`as` and a type in `cast`")?;
        let span = self.tok.span;
        let tok = self.tok.tok;
        let to = if let Tok::Ident(name) = tok {
            let Some(to) = Storage::named(name) else {
                return Err(error(SyntaxErrorKind::UnknownCastType(name), span));
            };
            to
        } else if matches!(tok, Tok::Quoted(_)) || tok.keyword().is_some() {
            return Err(error(SyntaxErrorKind::UnknownCastType(self.source(span)), span));
        } else {
            return Err(self.expected("a type: `boolean`, `integer`, `real`, `text` or `blob`"));
        };
        self.bump()?;
        let close = self.expect_punct(Punct::RParen, "`)` to close the `cast`")?;
        let kind = ExprKind::Cast { operand: operand.expr, to };
        self.node(kind, cover(start, close), operand.height)
    }
}

/// How tightly a chain's operator binds, by its level.
fn power(op: ChainOp) -> u8 {
    match op {
        ChainOp::Or => OR,
        ChainOp::And => AND,
        ChainOp::Add | ChainOp::Sub => ADD,
        ChainOp::Mul | ChainOp::Div | ChainOp::Rem => MUL,
        ChainOp::Concat => CONCAT,
    }
}
