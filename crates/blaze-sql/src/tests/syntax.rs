//! The syntax: the round trip of every tree of the subset through the printer and the parser,
//! text of any kind refused without a panic, spans that slice the source, and each refusal by
//! its own kind with a message that quotes the token.

extern crate std;

use alloc::alloc::Global;
use std::boxed::Box;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::string::String;
use std::string::ToString;
use std::vec::Vec;
use std::{eprintln, format};

use hegel::generators as gs;
use hegel::{Hegel, Settings, TestCase};

use crate::ast::{
    self, Assignment, CallArgs, CaseArm, ChainLink, ChainOp, CompareOp, Delete, Expr, ExprKind,
    Insert, Join, JoinKind, Name, OrderTerm, ResultColumn, Select, Span, SqlType, Statement,
    TableRef, UnaryOp, Update,
};
use crate::lex::keyword;
use crate::parse::{MAX_DEPTH, STACK_MOST, SyntaxErrorKind, TEXT_MOST, parse};
use crate::print;
use crate::same::same;
use crate::tests::stack;

// Generated trees. Every string is leaked so the tree can borrow it for `'static`; a test
// process makes a few thousand small ones.

const NOWHERE: Span = Span { start: 0, end: 0 };

/// The deepest expression the generator makes.
const TREE_DEPTH: u32 = 4;

type Tree = Statement<'static, Global>;
type Ex = Expr<'static, Global>;

fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

/// A number in `0..n`, for `n` at least 1.
fn below(tc: &TestCase, n: u32) -> u32 {
    tc.draw(gs::integers::<u32>().max_value(n.saturating_sub(1)))
}

fn coin(tc: &TestCase) -> bool {
    tc.draw(gs::booleans())
}

/// `[A-Za-z_][A-Za-z0-9_]*`, from a small alphabet so names collide with keywords (`by`) and
/// with each other in other cases.
fn identifier(tc: &TestCase) -> String {
    let mut text: String = tc.draw(gs::text().alphabet("abcxyzABCXYZ_").min_size(1).max_size(1));
    let rest: String = tc.draw(gs::text().alphabet("abcxyzABCXYZ_0189").max_size(6));
    text.push_str(&rest);
    text
}

/// A name that is not a keyword: quoted (any text without `"`) or a plain identifier.
fn name(tc: &TestCase) -> Name<'static> {
    if below(tc, 4) == 0 {
        let text: String =
            tc.draw(gs::text().alphabet("ab XY_09-+.,'é;()*$").min_size(1).max_size(6));
        return Name { text: leak(text), quoted: true, span: NOWHERE };
    }
    let mut text = identifier(tc);
    if keyword(&text).is_some() {
        // No keyword ends in `_`.
        text.push('_');
    }
    Name { text: leak(text), quoted: false, span: NOWHERE }
}

/// A parameter's name: any identifier, keywords included (`$select` is a parameter).
fn param(tc: &TestCase) -> Name<'static> {
    Name { text: leak(identifier(tc)), quoted: false, span: NOWHERE }
}

/// Finite and not negative: a sign is a `Neg` around the literal.
fn real(tc: &TestCase) -> f64 {
    let value: f64 =
        tc.draw(gs::floats::<f64>().min_value(0.0).allow_nan(false).allow_infinity(false));
    // `-0.0` would print as `-0.0`, which is `Neg` around `0.0`.
    if value == 0.0 { 0.0 } else { value }
}

/// The source of a text literal: any text with each `'` doubled.
fn text_literal(tc: &TestCase) -> &'static str {
    let raw: String = tc.draw(gs::text().max_size(8));
    leak(raw.replace('\'', "''"))
}

fn blob(tc: &TestCase) -> &'static str {
    let mut digits: String = tc.draw(gs::text().alphabet("0123456789abcdefABCDEF").max_size(8));
    if digits.len() % 2 == 1 {
        digits.pop();
    }
    leak(digits)
}

/// The chain operators, by level: one chain draws its operators from one row.
const CHAIN_LEVELS: [&[ChainOp]; 5] = [
    &[ChainOp::Or],
    &[ChainOp::And],
    &[ChainOp::Add, ChainOp::Sub],
    &[ChainOp::Mul, ChainOp::Div, ChainOp::Rem],
    &[ChainOp::Concat],
];

const COMPARE: [CompareOp; 8] = [
    CompareOp::Eq,
    CompareOp::Ne,
    CompareOp::Lt,
    CompareOp::Le,
    CompareOp::Gt,
    CompareOp::Ge,
    CompareOp::Is,
    CompareOp::IsNot,
];

const TYPES: [SqlType; 5] =
    [SqlType::Boolean, SqlType::Integer, SqlType::Real, SqlType::Text, SqlType::Blob];

/// An expression `depth` levels down (the top is 1); at [`TREE_DEPTH`] only leaves.
fn expr(tc: &TestCase, depth: u32) -> Ex {
    let kinds = if depth >= TREE_DEPTH { 8 } else { 19 };
    let kind = match below(tc, kinds) {
        0 => ExprKind::Null,
        1 => ExprKind::Boolean(coin(tc)),
        2 => ExprKind::Integer(tc.draw(gs::integers::<u64>())),
        3 => ExprKind::Real(real(tc)),
        4 => ExprKind::Text(text_literal(tc)),
        5 => ExprKind::Blob(blob(tc)),
        6 => {
            let table = if coin(tc) { Some(name(tc)) } else { None };
            ExprKind::Column { table, column: name(tc) }
        }
        7 => ExprKind::Param(param(tc)),
        8 => {
            let op = if coin(tc) { UnaryOp::Neg } else { UnaryOp::Not };
            ExprKind::Unary { op, operand: sub(tc, depth) }
        }
        9 => {
            // 1 to 4 links of one level; operands of any kind, nested chains among them.
            let ops = CHAIN_LEVELS[below(tc, 5) as usize];
            let first = sub(tc, depth);
            let mut rest = Vec::new_in(Global);
            for _ in 0..1 + below(tc, 4) {
                let op = ops[below(tc, ops.len() as u32) as usize];
                rest.push(ChainLink { op, operand: expr(tc, depth + 1) });
            }
            ExprKind::Chain { first, rest }
        }
        10 => ExprKind::IsNull { operand: sub(tc, depth), negated: coin(tc) },
        11 => ExprKind::InList {
            operand: sub(tc, depth),
            list: exprs(tc, depth, 1, 3),
            negated: coin(tc),
        },
        12 => ExprKind::InParam { operand: sub(tc, depth), list: param(tc), negated: coin(tc) },
        13 => ExprKind::Between {
            operand: sub(tc, depth),
            low: sub(tc, depth),
            high: sub(tc, depth),
            negated: coin(tc),
        },
        14 => {
            ExprKind::Like { operand: sub(tc, depth), pattern: sub(tc, depth), negated: coin(tc) }
        }
        15 => {
            let operand = if coin(tc) { Some(sub(tc, depth)) } else { None };
            let mut arms = Vec::new_in(Global);
            for _ in 0..1 + below(tc, 2) {
                arms.push(CaseArm { when: expr(tc, depth + 1), then: expr(tc, depth + 1) });
            }
            let otherwise = if coin(tc) { Some(sub(tc, depth)) } else { None };
            ExprKind::Case { operand, arms, otherwise }
        }
        16 => ExprKind::Cast { operand: sub(tc, depth), to: TYPES[below(tc, 5) as usize] },
        17 => {
            let op = COMPARE[below(tc, 8) as usize];
            ExprKind::Compare { op, left: sub(tc, depth), right: sub(tc, depth) }
        }
        _ => {
            let function = name(tc);
            let args = if below(tc, 4) == 0 {
                CallArgs::Star
            } else {
                let distinct = coin(tc);
                let args = exprs(tc, depth, if distinct { 1 } else { 0 }, 3);
                CallArgs::List { distinct, args }
            };
            ExprKind::Call { function, args }
        }
    };
    Expr { kind, span: NOWHERE }
}

fn sub(tc: &TestCase, depth: u32) -> Box<Ex, Global> {
    Box::new_in(expr(tc, depth + 1), Global)
}

/// `least..=most` expressions one level below `depth`.
fn exprs(tc: &TestCase, depth: u32, least: u32, most: u32) -> Vec<Ex, Global> {
    let mut exprs = Vec::new_in(Global);
    for _ in 0..least + below(tc, most - least + 1) {
        exprs.push(expr(tc, depth + 1));
    }
    exprs
}

fn maybe(tc: &TestCase) -> Option<Ex> {
    if coin(tc) { Some(expr(tc, 1)) } else { None }
}

fn result_columns(tc: &TestCase, least: u32) -> Vec<ResultColumn<'static, Global>, Global> {
    let mut columns = Vec::new_in(Global);
    for _ in 0..least + below(tc, 3 - least + 1) {
        columns.push(match below(tc, 4) {
            0 => ResultColumn::Star { span: NOWHERE },
            1 => ResultColumn::TableStar { table: name(tc) },
            _ => {
                let expr = expr(tc, 1);
                let alias = if coin(tc) { Some(name(tc)) } else { None };
                ResultColumn::Expr { expr, alias }
            }
        });
    }
    columns
}

fn table_ref(tc: &TestCase) -> TableRef<'static> {
    let table = name(tc);
    let alias = if coin(tc) { Some(name(tc)) } else { None };
    TableRef { table, alias }
}

fn select(tc: &TestCase) -> Select<'static, Global> {
    let distinct = coin(tc);
    let columns = result_columns(tc, 1);
    let from = if coin(tc) {
        let first = table_ref(tc);
        let mut joins = Vec::new_in(Global);
        for _ in 0..below(tc, 3) {
            let kind = if coin(tc) { JoinKind::Inner } else { JoinKind::Left };
            joins.push(Join { kind, table: table_ref(tc), on: expr(tc, 1) });
        }
        Some(ast::From { first, joins })
    } else {
        None
    };
    let filter = maybe(tc);
    let group_by = exprs(tc, 0, 0, 2);
    let having = maybe(tc);
    let mut order_by = Vec::new_in(Global);
    for _ in 0..below(tc, 3) {
        order_by.push(OrderTerm { expr: expr(tc, 1), descending: coin(tc) });
    }
    let limit = maybe(tc);
    // `offset` stands only after `limit`.
    let offset = if limit.is_some() { maybe(tc) } else { None };
    Select { distinct, columns, from, filter, group_by, having, order_by, limit, offset }
}

/// A statement of every form, with every node kind below it.
fn statement(tc: &TestCase) -> Tree {
    match below(tc, 4) {
        0 => Statement::Select(select(tc)),
        1 => {
            let table = name(tc);
            let mut columns = Vec::new_in(Global);
            for _ in 0..1 + below(tc, 3) {
                columns.push(name(tc));
            }
            let mut rows = Vec::new_in(Global);
            for _ in 0..1 + below(tc, 2) {
                rows.push(exprs(tc, 0, 1, 3));
            }
            let returning = result_columns(tc, 0);
            Statement::Insert(Insert { table, columns, rows, returning })
        }
        2 => {
            let table = name(tc);
            let mut sets = Vec::new_in(Global);
            for _ in 0..1 + below(tc, 2) {
                sets.push(Assignment { column: name(tc), value: expr(tc, 1) });
            }
            let filter = maybe(tc);
            let returning = result_columns(tc, 0);
            Statement::Update(Update { table, sets, filter, returning })
        }
        _ => {
            let table = name(tc);
            let filter = maybe(tc);
            let returning = result_columns(tc, 0);
            Statement::Delete(Delete { table, filter, returning })
        }
    }
}

// The oracle.

/// Spans of a parsed tree: each name's slices the text to the name as written (with its
/// quotes or `$`), and each expression's lies inside the one around it.
struct Spans<'t> {
    text: &'t str,
    wrong: Vec<String>,
}

impl Spans<'_> {
    fn slice(&mut self, span: Span, want: &str) {
        let got = self.text.get(span.start as usize..span.end as usize);
        if got != Some(want) {
            self.wrong.push(format!("{want:?}: span {span:?} slices {got:?}"));
        }
    }

    fn within(&mut self, inner: Span, outer: Span) {
        if !(outer.start <= inner.start && inner.start <= inner.end && inner.end <= outer.end) {
            self.wrong.push(format!("span {inner:?} is not inside {outer:?}"));
        }
    }

    fn name(&mut self, name: &Name<'_>, outer: Span) {
        self.within(name.span, outer);
        if name.quoted {
            let want = format!("\"{}\"", name.text);
            self.slice(name.span, &want);
        } else {
            self.slice(name.span, name.text);
        }
    }

    fn param(&mut self, name: &Name<'_>, outer: Span) {
        self.within(name.span, outer);
        let want = format!("${}", name.text);
        self.slice(name.span, &want);
    }

    fn expr(&mut self, expr: &Expr<'_, Global>, outer: Span) {
        self.within(expr.span, outer);
        let at = expr.span;
        match &expr.kind {
            ExprKind::Null
            | ExprKind::Boolean(_)
            | ExprKind::Integer(_)
            | ExprKind::Real(_)
            | ExprKind::Text(_)
            | ExprKind::Blob(_) => {}
            ExprKind::Column { table, column } => {
                if let Some(table) = table {
                    self.name(table, at);
                }
                self.name(column, at);
            }
            ExprKind::Param(name) => self.param(name, at),
            ExprKind::Unary { operand, .. } | ExprKind::IsNull { operand, .. } => {
                self.expr(operand, at);
            }
            ExprKind::Chain { first, rest } => {
                self.expr(first, at);
                for link in rest {
                    self.expr(&link.operand, at);
                }
            }
            ExprKind::Compare { left, right, .. } => {
                self.expr(left, at);
                self.expr(right, at);
            }
            ExprKind::InList { operand, list, .. } => {
                self.expr(operand, at);
                for entry in list {
                    self.expr(entry, at);
                }
            }
            ExprKind::InParam { operand, list, .. } => {
                self.expr(operand, at);
                self.param(list, at);
            }
            ExprKind::Between { operand, low, high, .. } => {
                self.expr(operand, at);
                self.expr(low, at);
                self.expr(high, at);
            }
            ExprKind::Like { operand, pattern, .. } => {
                self.expr(operand, at);
                self.expr(pattern, at);
            }
            ExprKind::Case { operand, arms, otherwise } => {
                if let Some(operand) = operand {
                    self.expr(operand, at);
                }
                for arm in arms {
                    self.expr(&arm.when, at);
                    self.expr(&arm.then, at);
                }
                if let Some(otherwise) = otherwise {
                    self.expr(otherwise, at);
                }
            }
            ExprKind::Cast { operand, .. } => self.expr(operand, at),
            ExprKind::Call { function, args } => {
                self.name(function, at);
                if let CallArgs::List { args, .. } = args {
                    for arg in args {
                        self.expr(arg, at);
                    }
                }
            }
        }
    }

    fn result_columns(&mut self, columns: &[ResultColumn<'_, Global>], whole: Span) {
        for column in columns {
            match column {
                ResultColumn::Star { span } => self.slice(*span, "*"),
                ResultColumn::TableStar { table } => self.name(table, whole),
                ResultColumn::Expr { expr, alias } => {
                    self.expr(expr, whole);
                    if let Some(alias) = alias {
                        self.name(alias, whole);
                    }
                }
            }
        }
    }

    fn table_ref(&mut self, table: &TableRef<'_>, whole: Span) {
        self.name(&table.table, whole);
        if let Some(alias) = &table.alias {
            self.name(alias, whole);
        }
    }

    fn option(&mut self, expr: Option<&Expr<'_, Global>>, whole: Span) {
        if let Some(expr) = expr {
            self.expr(expr, whole);
        }
    }

    fn statement(&mut self, statement: &Statement<'_, Global>) {
        let whole = Span { start: 0, end: self.text.len() as u32 };
        match statement {
            Statement::Select(select) => {
                self.result_columns(&select.columns, whole);
                if let Some(from) = &select.from {
                    self.table_ref(&from.first, whole);
                    for join in &from.joins {
                        self.table_ref(&join.table, whole);
                        self.expr(&join.on, whole);
                    }
                }
                self.option(select.filter.as_ref(), whole);
                for expr in &select.group_by {
                    self.expr(expr, whole);
                }
                self.option(select.having.as_ref(), whole);
                for term in &select.order_by {
                    self.expr(&term.expr, whole);
                }
                self.option(select.limit.as_ref(), whole);
                self.option(select.offset.as_ref(), whole);
            }
            Statement::Insert(insert) => {
                self.name(&insert.table, whole);
                for column in &insert.columns {
                    self.name(column, whole);
                }
                for row in &insert.rows {
                    for expr in row {
                        self.expr(expr, whole);
                    }
                }
                self.result_columns(&insert.returning, whole);
            }
            Statement::Update(update) => {
                self.name(&update.table, whole);
                for set in &update.sets {
                    self.name(&set.column, whole);
                    self.expr(&set.value, whole);
                }
                self.option(update.filter.as_ref(), whole);
                self.result_columns(&update.returning, whole);
            }
            Statement::Delete(delete) => {
                self.name(&delete.table, whole);
                self.option(delete.filter.as_ref(), whole);
                self.result_columns(&delete.returning, whole);
            }
        }
    }
}

fn spans_hold(text: &str, tree: &Statement<'_, Global>) -> Result<(), String> {
    let mut spans = Spans { text, wrong: Vec::new() };
    spans.statement(tree);
    if spans.wrong.is_empty() {
        Ok(())
    } else {
        Err(format!("spans of `{text}`: {}", spans.wrong.join("; ")))
    }
}

/// The printer's text of `tree`; `planted` uses the printer that drops parentheses.
fn printed(tree: &Statement<'_, Global>, planted: bool) -> Result<String, String> {
    let mut text = String::new();
    let done = if planted {
        print::print_without_parentheses(tree, &mut text)
    } else {
        print::print(tree, &mut text)
    };
    done.map_err(|_| format!("the printer refused {tree:#?}"))?;
    Ok(text)
}

/// `tree` printed parses back to a tree the same as it, which prints to the same text, and
/// whose spans hold.
fn round_trip(tree: &Statement<'_, Global>, planted: bool) -> Result<(), String> {
    let text = printed(tree, planted)?;
    let back = parse(&text, Global).map_err(|error| format!("`{text}` does not parse: {error}"))?;
    if !same(&back, tree) {
        return Err(format!("`{text}` parses to another tree:\n{back:#?}\nnot\n{tree:#?}"));
    }
    let again = printed(&back, planted)?;
    if again != text {
        return Err(format!("`{text}` parsed and printed again is `{again}`"));
    }
    spans_hold(&text, &back)
}

/// Runs `property` over `cases` cases, or `HEGEL_TEST_CASES` of them where that is set.
fn run_cases(cases: u64, property: impl FnMut(TestCase)) {
    let cases =
        std::env::var("HEGEL_TEST_CASES").ok().and_then(|it| it.parse().ok()).unwrap_or(cases);
    Hegel::new(property).settings(Settings::new().test_cases(cases).database(None)).run();
}

/// The invariant: for every tree of the subset (every statement form and every expression
/// kind, four deep), `same(parse(print(s)), s)`; the reparsed tree prints to the same text;
/// and every name of the reparsed tree has a span that slices the text to the name as
/// written. `HEGEL_TEST_CASES=<n>` runs it longer.
#[test]
fn a_printed_tree_parses_back_to_itself() {
    run_cases(512, |tc| {
        let tree = statement(&tc);
        if let Err(why) = round_trip(&tree, false) {
            panic!("{why}");
        }
    });
}

/// Control: without its parentheses the printer's text depends on precedence, and the round
/// trip must fail for some generated tree within the same case count.
#[test]
fn the_oracle_catches_a_printer_that_drops_parentheses() {
    let caught = catch_unwind(AssertUnwindSafe(|| {
        Hegel::new(|tc: TestCase| {
            let tree = statement(&tc);
            if let Err(why) = round_trip(&tree, true) {
                panic!("{why}");
            }
        })
        .settings(Settings::new().test_cases(512).database(None))
        .run();
    }));
    assert!(caught.is_err(), "the round trip passed with a printer that drops parentheses");
}

/// Text parses or is refused, never panics; a refusal's span lies in the text and its message
/// is not empty; and a tree that parsed round-trips.
fn parses_or_is_refused(text: &str) {
    match parse(text, Global) {
        Ok(tree) => {
            if let Err(why) = round_trip(&tree, false) {
                panic!("`{text}` parsed, but: {why}");
            }
            if let Err(why) = spans_hold(text, &tree) {
                panic!("{why}");
            }
        }
        Err(error) => {
            let message = format!("{error}");
            assert!(!message.is_empty(), "`{text}`: an empty message for {error:?}");
            let span = error.span;
            assert!(
                span.start <= span.end && span.end as usize <= text.len(),
                "`{text}`: span {span:?} of {error:?} is outside the text"
            );
        }
    }
}

#[test]
fn any_text_parses_or_is_refused_without_a_panic() {
    run_cases(1024, |tc| {
        let text: String = tc.draw(gs::text().max_size(64));
        parses_or_is_refused(&text);
    });
}

/// Pieces of the subset and of what it refuses, so random sequences reach deep into the
/// parser rather than stopping at the first byte.
const PIECES: &[&str] = &[
    "select",
    "insert",
    "into",
    "update",
    "set",
    "delete",
    "from",
    "where",
    "group",
    "by",
    "having",
    "order",
    "limit",
    "offset",
    "join",
    "left",
    "inner",
    "outer",
    "on",
    "as",
    "values",
    "returning",
    "and",
    "or",
    "not",
    "is",
    "null",
    "in",
    "between",
    "like",
    "case",
    "when",
    "then",
    "else",
    "end",
    "cast",
    "true",
    "false",
    "distinct",
    "asc",
    "desc",
    "a",
    "t",
    "\"q\"",
    "$p",
    "1",
    "2.5",
    "'s'",
    "x'0f'",
    "(",
    ")",
    ",",
    ".",
    ";",
    "*",
    "+",
    "-",
    "/",
    "%",
    "||",
    "=",
    "<>",
    "!=",
    "<",
    "<=",
    ">",
    ">=",
    "count",
    "integer",
    "?",
    "$1",
    "--",
    "\"",
];

#[test]
fn token_soup_parses_or_is_refused_without_a_panic() {
    run_cases(2048, |tc| {
        let picks: Vec<usize> =
            tc.draw(gs::vecs(gs::integers::<usize>().max_value(PIECES.len() - 1)).max_size(24));
        let mut text = String::from(if coin(&tc) { "select " } else { "" });
        for pick in picks {
            text.push_str(PIECES[pick]);
            text.push(' ');
        }
        parses_or_is_refused(&text);
    });
}

// Named refusals: each its own kind, with a message quoting the token.

fn refused(text: &str) -> (SyntaxErrorKind<'_>, String) {
    match parse(text, Global) {
        Ok(tree) => panic!("`{text}` parsed to {tree:?}"),
        Err(error) => {
            let message = format!("{error}");
            (error.kind, message)
        }
    }
}

macro_rules! refuses {
    ($text:expr, $kind:pat, $token:expr) => {{
        let text: &str = $text;
        let (kind, message) = refused(text);
        assert!(matches!(kind, $kind), "`{text}` was refused as {kind:?}");
        assert!(
            message.contains($token),
            "`{text}`: the message `{message}` does not quote {}",
            $token
        );
    }};
}

#[test]
fn comments_are_refused() {
    refuses!("select 1 -- and the rest", SyntaxErrorKind::Comment("--"), "`--`");
    refuses!("select /* a */ 1", SyntaxErrorKind::Comment("/*"), "`/*`");
}

#[test]
fn a_keyword_as_a_name_is_refused_and_told_to_quote() {
    refuses!("select order from t", SyntaxErrorKind::ReservedWord("order"), "`\"order\"`");
    refuses!("select a from Select", SyntaxErrorKind::ReservedWord("Select"), "`Select`");
    refuses!("select a as from from t", SyntaxErrorKind::ReservedWord("from"), "`from`");
    refuses!("insert into t (a, end) values (1, 2)", SyntaxErrorKind::ReservedWord("end"), "`end`");
    assert!(parse("select \"order\" from \"select\"", Global).is_ok());
}

#[test]
fn bad_quoted_names_are_refused() {
    refuses!("select \"a\"\"b\" from t", SyntaxErrorKind::QuoteInName(_), "\"a\"\"b\"");
    refuses!("select \"\" from t", SyntaxErrorKind::EmptyName, "`\"\"`");
    refuses!("select \"abc", SyntaxErrorKind::UnterminatedName(_), "\"abc");
}

#[test]
fn positional_and_foreign_parameters_are_refused_by_name() {
    let (kind, message) = refused("select * from person where id = $1");
    assert!(matches!(kind, SyntaxErrorKind::NumberedDollarParameter("$1")), "{kind:?}");
    assert_eq!(
        message,
        "positional parameter `$1` at byte 32: parameters are named, write `$name`"
    );
    refuses!("select ?", SyntaxErrorKind::AnonymousParameter("?"), "`?`");
    refuses!("select ?1", SyntaxErrorKind::NumberedQuestionParameter("?1"), "`?1`");
    refuses!("select :name", SyntaxErrorKind::ColonParameter(":name"), "`:name`");
    refuses!("select @name", SyntaxErrorKind::AtParameter("@name"), "`@name`");
    refuses!("select $", SyntaxErrorKind::NamelessParameter, "`$`");
}

#[test]
fn numbers_out_of_range_or_malformed_are_refused() {
    refuses!(
        "select 18446744073709551616",
        SyntaxErrorKind::IntegerTooLarge("18446744073709551616"),
        "18446744073709551616"
    );
    refuses!("select 1e400", SyntaxErrorKind::RealNotFinite("1e400"), "1e400");
    refuses!("select 12ab", SyntaxErrorKind::MalformedNumber("12ab"), "12ab");
    refuses!("select 1e", SyntaxErrorKind::MalformedNumber("1e"), "1e");
    refuses!("select 1.2.3", SyntaxErrorKind::MalformedNumber("1.2.3"), "1.2.3");

    let Ok(Statement::Select(select)) =
        parse("select 18446744073709551615, 1.5, 1e300, 2.5e-3", Global)
    else {
        panic!("the literals did not parse");
    };
    let values: Vec<String> = select
        .columns
        .iter()
        .map(|column| match column {
            ResultColumn::Expr { expr, .. } => format!("{:?}", expr.kind),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(
        values,
        ["Integer(18446744073709551615)", "Real(1.5)", "Real(1e300)", "Real(0.0025)"]
    );
}

#[test]
fn bad_text_and_blob_literals_are_refused() {
    refuses!("select 'abc", SyntaxErrorKind::UnterminatedText(_), "'abc");
    refuses!("select x'abc'", SyntaxErrorKind::BlobOddDigits(_), "x'abc'");
    refuses!("select x'zz'", SyntaxErrorKind::BlobNotHex(_), "x'zz'");
    refuses!("select x'ab", SyntaxErrorKind::UnterminatedBlob(_), "x'ab");

    let Ok(Statement::Select(select)) = parse("select 'it''s', X'0aFF'", Global) else {
        panic!("the literals did not parse");
    };
    let kinds: Vec<String> = select
        .columns
        .iter()
        .map(|column| match column {
            ResultColumn::Expr { expr, .. } => format!("{:?}", expr.kind),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(kinds, ["Text(\"it''s\")", "Blob(\"0aFF\")"]);
}

#[test]
fn a_byte_outside_the_subset_is_refused_by_name() {
    refuses!("select #", SyntaxErrorKind::UnexpectedByte(b'#'), "`#`");
    refuses!("select a | b", SyntaxErrorKind::UnexpectedByte(b'|'), "`|`");
    refuses!("select é", SyntaxErrorKind::UnexpectedByte(0xC3), "\\xC3");
    refuses!("select \u{1}", SyntaxErrorKind::UnexpectedByte(1), "\\x01");
}

#[test]
fn a_second_statement_is_refused() {
    refuses!("select 1; select 2", SyntaxErrorKind::SecondStatement("select"), "second statement");
    refuses!("select 1;;", SyntaxErrorKind::SecondStatement(";"), "`;`");
    assert!(parse("select 1;", Global).is_ok());
    assert!(parse("select 1 ; ", Global).is_ok());
}

#[test]
fn comparisons_do_not_chain() {
    refuses!("select a = b = c", SyntaxErrorKind::ChainedComparison("="), "parenthesize");
    refuses!("select a < b is null", SyntaxErrorKind::ChainedComparison("is"), "`is`");
    refuses!("select a like b not in (1)", SyntaxErrorKind::ChainedComparison("not"), "`not`");
    assert!(parse("select (a = b) = c", Global).is_ok());
}

#[test]
fn parentheses_past_the_bound_are_refused() {
    let parens = format!("select {}1{}", "(".repeat(300), ")".repeat(300));
    refuses!(&parens, SyntaxErrorKind::NestedTooDeeply, "nested too deeply");
}

#[test]
fn a_long_run_of_not_is_refused() {
    let nots = format!("select {}a", "not ".repeat(100_000));
    refuses!(&nots, SyntaxErrorKind::NestedTooDeeply, "nested too deeply");
}

#[test]
fn a_long_run_of_negation_is_refused() {
    let negations = format!("select {}1", "- ".repeat(100_000));
    refuses!(&negations, SyntaxErrorKind::NestedTooDeeply, "nested too deeply");
}

/// The texts of the long chains: 20,000 `and`-ed comparisons, and 20,000 additions.
fn long_chains() -> [String; 2] {
    let mut ands = String::from("select a = 1");
    let mut sums = String::from("select 1");
    for _ in 0..20_000 {
        ands.push_str(" and a = 1");
        sums.push_str(" + 1");
    }
    [ands, sums]
}

/// A chain's length costs no nesting: a chain is one level above its deepest operand however
/// long it is, and the parser, printer and [`same`] take its links in a loop.
#[test]
fn a_long_chain_costs_no_nesting() {
    for text in long_chains() {
        let tree = parse(&text, Global).unwrap_or_else(|error| panic!("{error}"));
        let Statement::Select(select) = &tree else {
            panic!("a long chain parsed as another statement");
        };
        let Some(ResultColumn::Expr { expr, .. }) = select.columns.first() else {
            panic!("a long chain is not a column");
        };
        let ExprKind::Chain { rest, .. } = &expr.kind else {
            panic!("a long chain parsed as {:?}", expr.kind);
        };
        assert_eq!(rest.len(), 20_000, "a long chain is not one chain");
        if let Err(why) = round_trip(&tree, false) {
            panic!("{why}");
        }
    }
}

/// A run of one level's operators is one flat chain, in order; a looser operator starts a
/// chain over it, a tighter one is an operand of it, and parentheses keep a chain inside
/// another, never flattened.
#[test]
fn a_run_of_one_level_is_one_chain() {
    fn links(text: &str) -> (bool, Vec<ChainOp>) {
        let tree = parse(text, Global).unwrap_or_else(|error| panic!("`{text}`: {error}"));
        let Statement::Select(select) = &tree else {
            panic!("`{text}` is not a select");
        };
        let Some(ResultColumn::Expr { expr, .. }) = select.columns.first() else {
            panic!("`{text}` has no expression column");
        };
        let ExprKind::Chain { first, rest } = &expr.kind else {
            panic!("`{text}` is not a chain: {:?}", expr.kind);
        };
        let first_is_chain = matches!(first.kind, ExprKind::Chain { .. });
        (first_is_chain, rest.iter().map(|link| link.op).collect())
    }
    assert_eq!(links("select a - b + c"), (false, Vec::from([ChainOp::Sub, ChainOp::Add])));
    assert_eq!(links("select a or b or c"), (false, Vec::from([ChainOp::Or, ChainOp::Or])));
    assert_eq!(links("select a + b * c - d"), (false, Vec::from([ChainOp::Add, ChainOp::Sub])));
    assert_eq!(links("select a * b + c"), (true, Vec::from([ChainOp::Add])));
    assert_eq!(links("select (a + b) + c"), (true, Vec::from([ChainOp::Add])));
    assert_eq!(links("select a = b and c"), (false, Vec::from([ChainOp::And])));
}

/// Text longer than [`TEXT_MOST`] is refused before it is lexed, naming its length and the
/// bound; text of exactly [`TEXT_MOST`] bytes is taken.
#[test]
fn text_over_text_most_is_refused() {
    let long = format!("select 1{}", " ".repeat(TEXT_MOST - "select 1".len() + 1));
    let (kind, message) = refused(&long);
    assert!(
        matches!(kind, SyntaxErrorKind::TextTooLong { bytes } if bytes == TEXT_MOST + 1),
        "{kind:?}"
    );
    let bytes = (TEXT_MOST + 1).to_string();
    let most = TEXT_MOST.to_string();
    assert!(message.contains(&bytes) && message.contains(&most), "{message}");
    assert!(parse(&long[..TEXT_MOST], Global).is_ok());
}

/// Texts of trees exactly [`MAX_DEPTH`] deep, one of each way of nesting: chains inside
/// parentheses inside chains, prefix runs, `-(` (two calls of the parser's recursion per
/// level, its worst case) and `case` inside `case`.
fn texts_at_the_bound() -> [String; 5] {
    let depth = MAX_DEPTH as usize - 1;
    let mut cases = String::from("1");
    let mut sums = String::from("1");
    for _ in 0..depth {
        cases = format!("case when {cases} then 1 end");
        sums = format!("1 + ({sums})");
    }
    [
        format!("select {sums}"),
        format!("select {}1", "- ".repeat(depth)),
        format!("select {}a", "not ".repeat(depth)),
        format!("select {}1{}", "-(".repeat(depth), ")".repeat(depth)),
        format!("select {cases}"),
    ]
}

/// The deepest trees the parser takes print to text it takes again, though the printer
/// parenthesizes every level.
#[test]
fn a_tree_as_deep_as_the_bound_round_trips() {
    for text in texts_at_the_bound() {
        let tree = parse(&text, Global).unwrap_or_else(|error| panic!("{error}"));
        if let Err(why) = round_trip(&tree, false) {
            panic!("{why}");
        }
    }
}

/// What the stack table measures: a name, the text, and the levels of its tree (for bytes
/// per level). The texts at the bound, then the long chains, whose bytes show that a chain's
/// length costs no stack.
fn measured_texts() -> Vec<(&'static str, String, usize)> {
    let at_the_bound = ["nested-sums", "negations", "nots", "neg-parens", "cases"];
    let depth = MAX_DEPTH as usize;
    let mut texts: Vec<(&'static str, String, usize)> = at_the_bound
        .into_iter()
        .zip(texts_at_the_bound())
        .map(|(name, text)| (name, text, depth))
        .collect();
    let [ands, sums] = long_chains();
    // `a = 1 and ..` is a chain over comparisons over leaves: 3 levels; `1 + ..` is 2.
    texts.push(("long-and", ands, 3));
    texts.push(("long-sum", sums, 2));
    texts
}

/// The invariant: parsing, printing, reparsing and comparing ([`same`]) each statement at
/// [`MAX_DEPTH`], and each long chain, use at most [`STACK_MOST`] bytes of stack, as the
/// probe in `tests::stack` measures from the recursive functions' `touch` calls. It runs on
/// a 32 MiB thread so a wrong bound fails this test, naming each phase over it, instead of
/// aborting the process. Prints `name phase bytes bytes_per_level` per measurement. Dropping
/// a tree is not measured by the probe (drop glue calls no `touch`); the drops run on the
/// same big thread, so they are exercised but not bounded here.
#[test]
fn a_tree_at_the_bound_uses_at_most_stack_most() {
    let worker = std::thread::Builder::new()
        .name(String::from("stack-measure"))
        .stack_size(32 * 1024 * 1024)
        .spawn(|| -> Result<Vec<String>, String> {
            let mut over = Vec::new();
            for (name, text, levels) in measured_texts() {
                let mut record = |phase: &str, bytes: usize| {
                    let per_level = bytes / levels;
                    eprintln!("{name} {phase} {bytes} {per_level}");
                    if bytes > STACK_MOST {
                        over.push(format!("{name} {phase}: {bytes} bytes, over {STACK_MOST}"));
                    }
                };
                let (tree, bytes) = stack::used(|| parse(&text, Global));
                record("parse", bytes);
                let Ok(tree) = tree else {
                    return Err(format!("{name}: the text did not parse"));
                };
                let mut printed = String::new();
                let (done, bytes) = stack::used(|| print::print(&tree, &mut printed));
                record("print", bytes);
                if done.is_err() {
                    return Err(format!("{name}: the tree did not print"));
                }
                let (back, bytes) = stack::used(|| parse(&printed, Global));
                record("reparse", bytes);
                let Ok(back) = back else {
                    return Err(format!("{name}: the printed text did not parse"));
                };
                let (equal, bytes) = stack::used(|| same(&back, &tree));
                record("same", bytes);
                if !equal {
                    return Err(format!("{name}: the printed text reads back as another tree"));
                }
                drop(back);
                drop(tree);
            }
            Ok(over)
        })
        .unwrap_or_else(|error| panic!("the measuring thread did not start: {error}"));
    match worker.join() {
        Ok(Ok(over)) => assert!(over.is_empty(), "over STACK_MOST: {}", over.join("; ")),
        Ok(Err(why)) => panic!("{why}"),
        Err(_) => panic!("the measuring thread panicked"),
    }
}

#[test]
fn other_refusals_name_the_token() {
    refuses!("select a in ()", SyntaxErrorKind::EmptyInList, "`in ()`");
    refuses!("select cast(a as varchar)", SyntaxErrorKind::UnknownCastType("varchar"), "`varchar`");
    refuses!("select a from t offset 1", SyntaxErrorKind::OffsetWithoutLimit, "`offset`");
    refuses!("select a b from t", SyntaxErrorKind::Expected { found: "b", .. }, "`b`");
    refuses!(
        "insert into t values (1)",
        SyntaxErrorKind::Expected { found: "values", .. },
        "`values`"
    );
    refuses!("select", SyntaxErrorKind::Expected { found: "", .. }, "the end of the text");
    refuses!("", SyntaxErrorKind::Expected { found: "", .. }, "a statement");
    refuses!("select case end", SyntaxErrorKind::Expected { found: "end", .. }, "`when`");
}

// Accepted spellings and precedence.

fn same_text(a: &str, b: &str) -> bool {
    fn parsed(text: &str) -> Statement<'_, Global> {
        parse(text, Global).unwrap_or_else(|error| panic!("`{text}`: {error}"))
    }
    same(&parsed(a), &parsed(b))
}

#[test]
fn spellings_of_one_statement_are_one_tree() {
    for (a, b) in [
        ("SELECT A FROM T WHERE B IS NOT NULL", "select a from t where b is not null"),
        ("select * from a join b on x", "select * from a inner join b on x"),
        ("select * from a left join b on x", "select * from a left outer join b on x"),
        ("select * from t as u", "select * from t u"),
        ("select a from t where a <> 1", "select a from t where a != 1"),
        ("select a from t order by a asc", "select a from t order by a"),
        ("select 1;", "select 1"),
        ("select x'AB'", "select x'ab'"),
        ("select ((a))", "select a"),
    ] {
        assert!(same_text(a, b), "`{a}` and `{b}` differ");
    }
    for (a, b) in [
        ("select \"A\"", "select a"),
        ("select \"a\"", "select \"A\""),
        ("select $a", "select $A"),
        ("select a in $p", "select a in ($p)"),
        ("select a is null", "select a is (null)"),
        // A chain is flat; parentheses keep a chain inside another.
        ("select a - b - c", "select (a - b) - c"),
        ("select a or b or c", "select (a or b) or c"),
        ("select a + b", "select a - b"),
        ("select a from t order by a desc", "select a from t order by a"),
    ] {
        assert!(!same_text(a, b), "`{a}` and `{b}` are one tree");
    }
}

#[test]
fn precedence_is_the_documented_one() {
    for (a, b) in [
        (
            "select a or b and not c = d + e * f || g",
            "select a or (b and (not (c = (d + (e * (f || g))))))",
        ),
        ("select - a || b", "select (-a) || b"),
        ("select a between 1 and 2 and b", "select (a between 1 and 2) and b"),
        ("select a between b + 1 and c * 2", "select a between (b + 1) and (c * 2)"),
        ("select not a in (1, 2)", "select not (a in (1, 2))"),
        ("select a is not b + 1", "select a is not (b + 1)"),
        ("select a not like b || 'x'", "select a not like (b || 'x')"),
    ] {
        assert!(same_text(a, b), "`{a}` is not `{b}`");
    }
}

#[test]
fn every_name_span_slices_the_name_as_written() {
    for text in [
        "select \"T\".*, t.\"Col\" as \"X y\", count(*), $p from \"order\" as o left outer join u \
         on o.id = u.id where a in $list and b not in ($q, 1) group by a order by a desc limit $n \
         offset 2",
        "insert into \"T\" (a, \"b\") values ($a, 1), (2, $b) returning *",
        "update t set \"a\" = $x where b is not null returning a as \"A\"",
        "delete from t where cast($c as integer) = (1) returning t.*",
        "select CASE $k WHEN 1 THEN Upper(\"N\") ELSE -x END from T",
    ] {
        let tree = parse(text, Global).unwrap_or_else(|error| panic!("`{text}`: {error}"));
        if let Err(why) = spans_hold(text, &tree) {
            panic!("{why}");
        }
    }
}
