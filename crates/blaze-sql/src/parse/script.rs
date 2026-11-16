//! The query grammar (docs/v2/plan/design/query.md): statements of [`crate::query`], parsed by
//! the subset's [`Parser`] with its lexer in [`Grammar::Query`]. Expressions are the subset's,
//! through the same precedence climbing; the forms only this grammar has (subqueries, `exists`,
//! `filter`, `over`, `::`, `@table`, `is [not] distinct from`) come in through
//! [`Parser::query_operand`], which [`Parser::operand`] hands every operand to in this grammar.
//!
//! The words of this grammar that the subset does not reserve (`with`, `union`, `over`,
//! `qualify`, `asof`, ..) are names to the lexer and words here, by text; [`CONTEXTUAL`] keeps
//! them from being taken as an implicit alias.
//!
//! Nesting: a query inside a query opens a level of [`Parser::enter`], as an expression does, so
//! the one bound on recursion covers both.

#![blaze::sans_io]

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::alloc::Allocator;

use super::{
    ADD, NEG, NOT, Node, OR, Parsed, Parser, SyntaxError, SyntaxErrorKind, TEXT_MOST, cover, error,
};
use crate::ast::{CallArgs, CompareOp, ExprKind, Name, ResultColumn, Span, UnaryOp};
use crate::lex::{Keyword, Lexer, Punct, Tok};
use crate::query::{
    Aggregate, Analytic, Bound, Constraint, Core, Cte, Frame, FrameUnits, FromItem, GroupBy, Join,
    JoinKind, Order, Query, SetExpr, SetOp, Statement, TypeName, Window, With,
};

/// Words of the query grammar that are names to the lexer and are never an implicit alias:
/// after a table or a result column, each starts the next clause or join.
const CONTEXTUAL: [&str; 24] = [
    "with",
    "recursive",
    "union",
    "except",
    "intersect",
    "all",
    "qualify",
    "window",
    "using",
    "asof",
    "cross",
    "full",
    "right",
    "natural",
    "lateral",
    "positional",
    "semi",
    "anti",
    "filter",
    "over",
    "partition",
    "rows",
    "range",
    "exclude",
];

/// Parses `text` as statements of the query grammar, each ended by `;` (the last may end at
/// the end of the text instead).
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn parse_script<'s, A: Allocator + Copy>(
    text: &'s str,
    alloc: A,
) -> Result<Vec<Statement<'s, A>, A>, SyntaxError<'s>> {
    let mut parser = Parser::for_queries(text, alloc)?;
    let mut statements = Vec::new_in(alloc);
    // Every turn takes a statement or a `;`, so the text bounds the loop.
    loop {
        while parser.eat_punct(Punct::Semicolon)?.is_some() {}
        if parser.at_end() {
            return Ok(statements);
        }
        statements.push(parser.query_statement()?);
        if parser.eat_punct(Punct::Semicolon)?.is_none() && !parser.at_end() {
            return Err(parser.expected("`;` after the statement"));
        }
    }
}

/// Parses `text` as exactly one statement of the query grammar, with at most one `;` after it.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn parse_query<'s, A: Allocator + Copy>(
    text: &'s str,
    alloc: A,
) -> Result<Statement<'s, A>, SyntaxError<'s>> {
    let mut parser = Parser::for_queries(text, alloc)?;
    let statement = parser.query_statement()?;
    parser.end()?;
    Ok(statement)
}

impl<'s, A: Allocator + Copy> Parser<'s, A> {
    fn for_queries(text: &'s str, alloc: A) -> Parsed<'s, Parser<'s, A>> {
        if text.len() > TEXT_MOST {
            return Err(error(
                SyntaxErrorKind::TextTooLong { bytes: text.len() },
                Span { start: 0, end: 0 },
            ));
        }
        let mut lex = Lexer::for_queries(text);
        let tok = lex.next_token()?;
        Ok(Parser { text, lex, tok, alloc, depth: 0 })
    }

    // Words.

    fn at_word(&self, word: &str) -> bool {
        matches!(self.tok.tok, Tok::Ident(it) if it.eq_ignore_ascii_case(word))
    }

    fn eat_word(&mut self, word: &str) -> Parsed<'s, Option<Span>> {
        if self.at_word(word) { Ok(Some(self.bump()?.span)) } else { Ok(None) }
    }

    fn expect_word(&mut self, word: &str, expected: &'static str) -> Parsed<'s, Span> {
        match self.eat_word(word)? {
            Some(span) => Ok(span),
            None => Err(self.expected(expected)),
        }
    }

    /// Whether the current token can be an alias written without `as`.
    fn at_alias(&self) -> bool {
        match self.tok.tok {
            Tok::Quoted(_) => true,
            Tok::Ident(word) => !CONTEXTUAL.iter().any(|it| it.eq_ignore_ascii_case(word)),
            _ => false,
        }
    }

    /// `as name`, or a name that is not a word of the grammar, or nothing.
    fn alias(&mut self) -> Parsed<'s, Option<Name<'s>>> {
        if self.eat_keyword(Keyword::As)?.is_some() {
            return Ok(Some(self.name("a name after `as`")?));
        }
        if self.at_alias() { Ok(Some(self.name("an alias")?)) } else { Ok(None) }
    }

    /// Whether the current token is `(` and a query starts after it.
    pub(super) fn query_ahead(&self) -> Parsed<'s, bool> {
        if !self.at_punct(Punct::LParen) {
            return Ok(false);
        }
        let next = self.peek()?.tok;
        Ok(matches!(next, Tok::Keyword(Keyword::Select))
            || matches!(next, Tok::Ident(word) if word.eq_ignore_ascii_case("with")))
    }

    /// The span from `start` to the start of the current token.
    fn since(&self, start: Span) -> Span {
        Span { start: start.start, end: self.tok.span.start.max(start.end) }
    }

    // Statements.

    fn query_statement(&mut self) -> Parsed<'s, Statement<'s, A>> {
        let start = self.tok.span;
        if self.eat_word("create")?.is_some() {
            let replace = if self.eat_keyword(Keyword::Or)?.is_some() {
                self.expect_word("replace", "`replace` after `create or`")?;
                true
            } else {
                false
            };
            self.expect_word("view", "`view` after `create`: the query grammar makes only views")?;
            let name = self.name("the view's name")?;
            self.expect_keyword(Keyword::As, "`as` and the view's query")?;
            let query = self.query()?;
            return Ok(Statement::CreateView { name, replace, query, span: self.since(start) });
        }
        Ok(Statement::Query(self.query()?))
    }

    /// `[with ..] body [order by ..] [limit ..] [offset ..]`.
    fn query(&mut self) -> Parsed<'s, Query<'s, A>> {
        #[cfg(test)]
        crate::tests::stack::touch();
        self.enter()?;
        let start = self.tok.span;
        let with = if self.eat_word("with")?.is_some() {
            let recursive = self.eat_word("recursive")?.is_some();
            let mut ctes = Vec::new_in(self.alloc);
            loop {
                let name = self.name("a name for the `with` query")?;
                let mut columns = Vec::new_in(self.alloc);
                if self.eat_punct(Punct::LParen)?.is_some() {
                    loop {
                        columns.push(self.name("a column name")?);
                        if self.eat_punct(Punct::Comma)?.is_none() {
                            break;
                        }
                    }
                    self.expect_punct(Punct::RParen, "`,` or `)` in the column list")?;
                }
                self.expect_keyword(Keyword::As, "`as` and the `with` query in parentheses")?;
                self.expect_punct(Punct::LParen, "`(` and the `with` query")?;
                let query = self.query()?;
                self.expect_punct(Punct::RParen, "`)` to close the `with` query")?;
                ctes.push(Cte { name, columns, query });
                if self.eat_punct(Punct::Comma)?.is_none() {
                    break;
                }
            }
            Some(With { recursive, ctes })
        } else {
            None
        };
        let body = self.set_expr()?;
        let order_by = if self.eat_keyword(Keyword::Order)?.is_some() {
            self.expect_keyword(Keyword::By, "`by` after `order`")?;
            self.orders()?
        } else {
            Vec::new_in(self.alloc)
        };
        let limit = self.clause(Keyword::Limit)?;
        let offset = self.clause(Keyword::Offset)?;
        self.leave();
        Ok(Query { with, body, order_by, limit, offset, span: self.since(start) })
    }

    /// Terms joined by `union`, `except` and `intersect`, left to right.
    fn set_expr(&mut self) -> Parsed<'s, SetExpr<'s, A>> {
        let mut left = self.set_term()?;
        // Every turn takes an operator and a term, so the text bounds the loop.
        loop {
            let op = if self.eat_word("union")?.is_some() {
                SetOp::Union
            } else if self.eat_word("except")?.is_some() {
                SetOp::Except
            } else if self.eat_word("intersect")?.is_some() {
                SetOp::Intersect
            } else {
                return Ok(left);
            };
            let all = self.eat_word("all")?.is_some();
            let by_name = if self.at_keyword(Keyword::By) {
                self.bump()?;
                self.expect_word("name", "`name` after `union .. by`")?;
                true
            } else {
                false
            };
            let right = self.set_term()?;
            left = SetExpr::SetOp {
                op,
                all,
                by_name,
                left: Box::new_in(left, self.alloc),
                right: Box::new_in(right, self.alloc),
            };
        }
    }

    fn set_term(&mut self) -> Parsed<'s, SetExpr<'s, A>> {
        if self.query_ahead()? {
            self.bump()?;
            let query = self.query()?;
            self.expect_punct(Punct::RParen, "`)` to close the query")?;
            return Ok(SetExpr::Nested(Box::new_in(query, self.alloc)));
        }
        if !self.at_keyword(Keyword::Select) {
            return Err(self.expected("`select` or a query in parentheses"));
        }
        Ok(SetExpr::Core(Box::new_in(self.core()?, self.alloc)))
    }

    fn core(&mut self) -> Parsed<'s, Core<'s, A>> {
        let start = self.bump()?.span;
        let distinct = self.eat_keyword(Keyword::Distinct)?.is_some();
        let mut columns = Vec::new_in(self.alloc);
        loop {
            columns.push(self.query_result_column()?);
            if self.eat_punct(Punct::Comma)?.is_none() {
                break;
            }
        }
        let mut from = Vec::new_in(self.alloc);
        if self.eat_keyword(Keyword::From)?.is_some() {
            loop {
                from.push(self.join_tree()?);
                if self.eat_punct(Punct::Comma)?.is_none() {
                    break;
                }
            }
        }
        let filter = self.clause(Keyword::Where)?;
        let group_by = if self.eat_keyword(Keyword::Group)?.is_some() {
            self.expect_keyword(Keyword::By, "`by` after `group`")?;
            if let Some(span) = self.eat_word("all")? {
                GroupBy::All(span)
            } else {
                let mut exprs = Vec::new_in(self.alloc);
                loop {
                    exprs.push(self.top()?);
                    if self.eat_punct(Punct::Comma)?.is_none() {
                        break;
                    }
                }
                GroupBy::Exprs(exprs)
            }
        } else {
            GroupBy::None
        };
        let having = self.clause(Keyword::Having)?;
        let qualify = if self.eat_word("qualify")?.is_some() { Some(self.top()?) } else { None };
        Ok(Core {
            distinct,
            columns,
            from,
            filter,
            group_by,
            having,
            qualify,
            span: self.since(start),
        })
    }

    /// `*`, `t.*`, or an expression with an optional alias, `as` or not.
    fn query_result_column(&mut self) -> Parsed<'s, ResultColumn<'s, A>> {
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
        let alias = self.alias()?;
        Ok(ResultColumn::Expr { expr, alias })
    }

    /// A `from` item and the joins after it, left to right.
    fn join_tree(&mut self) -> Parsed<'s, FromItem<'s, A>> {
        let mut left = self.from_primary()?;
        // Every turn takes a join, so the text bounds the loop.
        loop {
            let span = self.tok.span;
            let kind = if self.eat_keyword(Keyword::Join)?.is_some() {
                JoinKind::Inner
            } else if self.eat_keyword(Keyword::Inner)?.is_some() {
                self.expect_keyword(Keyword::Join, "`join` after `inner`")?;
                JoinKind::Inner
            } else if self.eat_keyword(Keyword::Left)?.is_some() {
                self.eat_keyword(Keyword::Outer)?;
                self.expect_keyword(Keyword::Join, "`join` after `left` or `left outer`")?;
                JoinKind::Left
            } else if self.eat_word("right")?.is_some() {
                self.eat_keyword(Keyword::Outer)?;
                self.expect_keyword(Keyword::Join, "`join` after `right`")?;
                JoinKind::Right
            } else if self.eat_word("full")?.is_some() {
                self.eat_keyword(Keyword::Outer)?;
                self.expect_keyword(Keyword::Join, "`join` after `full`")?;
                JoinKind::Full
            } else if self.eat_word("cross")?.is_some() {
                self.expect_keyword(Keyword::Join, "`join` after `cross`")?;
                JoinKind::Cross
            } else if self.eat_word("asof")?.is_some() {
                if self.eat_keyword(Keyword::Left)?.is_some() {
                    self.expect_keyword(Keyword::Join, "`join` after `asof left`")?;
                    JoinKind::AsofLeft
                } else {
                    self.expect_keyword(Keyword::Join, "`join` or `left join` after `asof`")?;
                    JoinKind::Asof
                }
            } else {
                return Ok(left);
            };
            let right = self.from_primary()?;
            let constraint = if kind == JoinKind::Cross {
                Constraint::None
            } else if self.eat_keyword(Keyword::On)?.is_some() {
                Constraint::On(self.top()?)
            } else if self.eat_word("using")?.is_some() {
                self.expect_punct(Punct::LParen, "`(` and the columns of `using`")?;
                let mut names = Vec::new_in(self.alloc);
                loop {
                    names.push(self.name("a column name")?);
                    if self.eat_punct(Punct::Comma)?.is_none() {
                        break;
                    }
                }
                self.expect_punct(Punct::RParen, "`,` or `)` in `using`")?;
                Constraint::Using(names)
            } else {
                return Err(self.expected("`on` or `using` and the join's condition"));
            };
            let join = Join { kind, left, right, constraint, span };
            left = FromItem::Join(Box::new_in(join, self.alloc));
        }
    }

    /// A table, a table function or a query in parentheses, with an optional alias.
    fn from_primary(&mut self) -> Parsed<'s, FromItem<'s, A>> {
        if self.query_ahead()? {
            self.bump()?;
            let query = self.query()?;
            self.expect_punct(Punct::RParen, "`)` to close the query")?;
            let alias = self.alias()?;
            return Ok(FromItem::Subquery { query: Box::new_in(query, self.alloc), alias });
        }
        let name = self.name("a table name or a query in parentheses")?;
        if self.eat_punct(Punct::LParen)?.is_some() {
            let mut args = Vec::new_in(self.alloc);
            if !self.at_punct(Punct::RParen) {
                loop {
                    args.push(self.top()?);
                    if self.eat_punct(Punct::Comma)?.is_none() {
                        break;
                    }
                }
            }
            self.expect_punct(Punct::RParen, "`,` or `)` in the arguments of a table function")?;
            let span = self.since(name.span);
            let alias = self.alias()?;
            return Ok(FromItem::Function { function: name, args, alias, span });
        }
        let alias = self.alias()?;
        Ok(FromItem::Table { name, alias })
    }

    /// `expr [asc|desc] [nulls first|last], ..`.
    fn orders(&mut self) -> Parsed<'s, Vec<Order<'s, A>, A>> {
        let mut orders = Vec::new_in(self.alloc);
        loop {
            let expr = self.top()?;
            let descending = if self.eat_keyword(Keyword::Desc)?.is_some() {
                true
            } else {
                self.eat_keyword(Keyword::Asc)?;
                false
            };
            let nulls_first = if self.eat_word("nulls")?.is_some() {
                if self.eat_word("first")?.is_some() {
                    Some(true)
                } else {
                    self.expect_word("last", "`first` or `last` after `nulls`")?;
                    Some(false)
                }
            } else {
                None
            };
            orders.push(Order { expr, descending, nulls_first });
            if self.eat_punct(Punct::Comma)?.is_none() {
                return Ok(orders);
            }
        }
    }

    // Expressions.

    fn analytic(
        &self,
        analytic: Analytic<'s, A>,
        span: Span,
        below: u32,
    ) -> Parsed<'s, Node<'s, A>> {
        self.node(ExprKind::Analytic(Box::new_in(analytic, self.alloc)), span, below)
    }

    /// An operand in the query grammar: the subset's, the forms only this grammar has, and
    /// any number of `::type` after a primary.
    pub(super) fn query_operand(&mut self, min: u8) -> Parsed<'s, Node<'s, A>> {
        #[cfg(test)]
        crate::tests::stack::touch();
        let start = self.tok.span;
        if self.at_keyword(Keyword::Not) && min <= NOT {
            self.bump()?;
            let operand = self.expr_at(NOT)?;
            return self.unary(UnaryOp::Not, start, operand);
        }
        if self.at_punct(Punct::Minus) {
            self.bump()?;
            let operand = self.expr_at(NEG)?;
            return self.unary(UnaryOp::Neg, start, operand);
        }
        let mut primary = if self.query_ahead()? {
            self.bump()?;
            let query = self.query()?;
            let close = self.expect_punct(Punct::RParen, "`)` to close the subquery")?;
            self.analytic(Analytic::Subquery(query), cover(start, close), 0)?
        } else if self.at_punct(Punct::LParen) {
            self.bump()?;
            let mut inner = self.expr_at(OR)?;
            let close = self.expect_punct(Punct::RParen, "`)`")?;
            inner.expr.span = cover(start, close);
            inner
        } else if self.at_word("exists") && matches!(self.peek()?.tok, Tok::Punct(Punct::LParen)) {
            self.bump()?;
            if !self.query_ahead()? {
                return Err(self.expected("`(` and a query after `exists`"));
            }
            self.bump()?;
            let query = self.query()?;
            let close = self.expect_punct(Punct::RParen, "`)` to close the `exists` query")?;
            self.analytic(Analytic::Exists(query), cover(start, close), 0)?
        } else if self.at_keyword(Keyword::Case) {
            self.case()?
        } else if self.at_keyword(Keyword::Cast) {
            self.query_cast()?
        } else if let Tok::At(text) = self.tok.tok {
            let span = self.bump()?.span;
            self.analytic(Analytic::Files(Name { text, quoted: false, span }), span, 0)?
        } else if self.at_name() {
            let first = self.name("a name")?;
            if self.eat_punct(Punct::LParen)?.is_some() {
                self.query_call(first)?
            } else if self.eat_punct(Punct::Dot)?.is_some() {
                let column = self.name("a column name after `.`")?;
                let kind = ExprKind::Column { table: Some(first), column };
                self.leaf(kind, cover(first.span, column.span))
            } else {
                self.leaf(ExprKind::Column { table: None, column: first }, first.span)
            }
        } else {
            self.literal()?
        };
        // Every turn takes `::` and a type.
        while self.eat_punct(Punct::DoubleColon)?.is_some() {
            let to = self.type_name()?;
            let span = self.since(start);
            let height = primary.height;
            let operand = *primary.expr;
            primary = self.analytic(Analytic::Convert { operand, to }, span, height)?;
        }
        Ok(primary)
    }

    /// A type of `cast` or `::`.
    fn type_name(&mut self) -> Parsed<'s, TypeName> {
        let span = self.tok.span;
        let Tok::Ident(name) = self.tok.tok else {
            return Err(error(SyntaxErrorKind::UnknownQueryType(self.source(span)), span));
        };
        let Some(to) = TypeName::named(name) else {
            return Err(error(SyntaxErrorKind::UnknownQueryType(name), span));
        };
        self.bump()?;
        Ok(to)
    }

    /// `cast(operand as type)` to a type of this grammar.
    fn query_cast(&mut self) -> Parsed<'s, Node<'s, A>> {
        let start = self.bump()?.span;
        self.expect_punct(Punct::LParen, "`(` after `cast`")?;
        let operand = self.expr_at(OR)?;
        self.expect_keyword(Keyword::As, "`as` and a type in `cast`")?;
        let to = self.type_name()?;
        let close = self.expect_punct(Punct::RParen, "`)` to close the `cast`")?;
        let height = operand.height;
        self.analytic(Analytic::Convert { operand: *operand.expr, to }, cover(start, close), height)
    }

    /// A call's arguments after its `(`, then an inner `order by`, `filter (where ..)` and
    /// `over (..)`. Without any of the three it is the subset's [`ExprKind::Call`].
    fn query_call(&mut self, function: Name<'s>) -> Parsed<'s, Node<'s, A>> {
        let mut below = 0;
        let args = if self.eat_punct(Punct::Star)?.is_some() {
            CallArgs::Star
        } else {
            let distinct = self.eat_keyword(Keyword::Distinct)?.is_some();
            let mut args = Vec::new_in(self.alloc);
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
            CallArgs::List { distinct, args }
        };
        let order_by = if self.eat_keyword(Keyword::Order)?.is_some() {
            self.expect_keyword(Keyword::By, "`by` after `order`")?;
            self.orders()?
        } else {
            Vec::new_in(self.alloc)
        };
        let mut close =
            self.expect_punct(Punct::RParen, "`,`, `order by` or `)` in the arguments of a call")?;
        let filter = if self.at_word("filter")
            && matches!(self.peek()?.tok, Tok::Punct(Punct::LParen))
        {
            self.bump()?;
            self.bump()?;
            self.expect_keyword(Keyword::Where, "`where` in `filter (..)`")?;
            let filter = self.top()?;
            close = self.expect_punct(Punct::RParen, "`)` to close `filter`")?;
            Some(filter)
        } else {
            None
        };
        let over = if self.eat_word("over")?.is_some() {
            self.expect_punct(Punct::LParen, "`(` and the window after `over`")?;
            let window = self.window()?;
            close = self.expect_punct(Punct::RParen, "`)` to close the window")?;
            Some(window)
        } else {
            None
        };
        let span = cover(function.span, close);
        if order_by.is_empty() && filter.is_none() && over.is_none() {
            return self.node(ExprKind::Call { function, args }, span, below);
        }
        let aggregate = Aggregate { function, args, order_by, filter, over };
        self.analytic(Analytic::Aggregate(aggregate), span, below)
    }

    /// `[partition by ..] [order by ..] [frame]`, inside `over (..)`.
    fn window(&mut self) -> Parsed<'s, Window<'s, A>> {
        let mut partition_by = Vec::new_in(self.alloc);
        if self.eat_word("partition")?.is_some() {
            self.expect_keyword(Keyword::By, "`by` after `partition`")?;
            loop {
                partition_by.push(self.top()?);
                if self.eat_punct(Punct::Comma)?.is_none() {
                    break;
                }
            }
        }
        let order_by = if self.eat_keyword(Keyword::Order)?.is_some() {
            self.expect_keyword(Keyword::By, "`by` after `order`")?;
            self.orders()?
        } else {
            Vec::new_in(self.alloc)
        };
        let units = if self.eat_word("rows")?.is_some() {
            Some(FrameUnits::Rows)
        } else if self.eat_word("range")?.is_some() {
            Some(FrameUnits::Range)
        } else {
            None
        };
        let frame = match units {
            None => None,
            Some(units) => {
                if self.eat_keyword(Keyword::Between)?.is_some() {
                    let start = self.bound()?;
                    self.expect_keyword(Keyword::And, "`and` between the frame's bounds")?;
                    let end = self.bound()?;
                    Some(Frame { units, start, end })
                } else {
                    let start = self.bound()?;
                    Some(Frame { units, start, end: Bound::CurrentRow })
                }
            }
        };
        Ok(Window { partition_by, order_by, frame })
    }

    /// `unbounded preceding`, `n preceding`, `current row`, `n following`, `unbounded following`.
    fn bound(&mut self) -> Parsed<'s, Bound<'s, A>> {
        if self.eat_word("unbounded")?.is_some() {
            if self.eat_word("preceding")?.is_some() {
                return Ok(Bound::UnboundedPreceding);
            }
            self.expect_word("following", "`preceding` or `following` after `unbounded`")?;
            return Ok(Bound::UnboundedFollowing);
        }
        if self.eat_word("current")?.is_some() {
            self.expect_word("row", "`row` after `current`")?;
            return Ok(Bound::CurrentRow);
        }
        let offset = *self.expr_at(ADD)?.expr;
        if self.eat_word("preceding")?.is_some() {
            return Ok(Bound::Preceding(offset));
        }
        self.expect_word("following", "`preceding` or `following` after a frame offset")?;
        Ok(Bound::Following(offset))
    }

    /// `left is [not] distinct from right`, from `distinct`; `not_` says whether `not` came
    /// before it. `is not distinct from` is the subset's `is`, `is distinct from` its `is not`.
    pub(super) fn distinct_from(
        &mut self,
        left: Node<'s, A>,
        not_: bool,
    ) -> Parsed<'s, Node<'s, A>> {
        self.expect_keyword(Keyword::Distinct, "`distinct`")?;
        self.expect_keyword(Keyword::From, "`from` after `is distinct`")?;
        let right = self.expr_at(ADD)?;
        self.compare(if not_ { CompareOp::Is } else { CompareOp::IsNot }, left, right)
    }

    /// `left [not] in (query)`, from the `(`.
    pub(super) fn in_query(
        &mut self,
        left: Node<'s, A>,
        negated: bool,
    ) -> Parsed<'s, Node<'s, A>> {
        self.bump()?;
        let query = self.query()?;
        let close = self.expect_punct(Punct::RParen, "`)` to close the `in` query")?;
        let span = cover(left.expr.span, close);
        let height = left.height;
        let analytic = Analytic::InQuery { operand: *left.expr, query, negated };
        self.analytic(analytic, span, height)
    }
}
