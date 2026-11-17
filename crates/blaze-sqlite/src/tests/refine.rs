//! The checker's inference held to real SQLite. On generated schemas, rows and statements of the
//! subset, every statement the checker accepts is one SQLite prepares as described (the driver's
//! `prepare` compares placeholder count, result width and names), and every value of every row
//! SQLite returns is admitted by the checker's description: null only where it says nullable,
//! and otherwise of the storage it names. `Statement::column` makes that check too; this file
//! makes it again from the description, so the oracle does not rest on the driver alone.
//!
//! Statements are built by construction so that they type: every expression is generated for a
//! storage it is known to have. A statement the checker refuses is a bug in the generator, and
//! the test stops naming the text and the checker's error.
//!
//! Values stay small (integers within a few thousand, reals on a grid of eighths, expressions at
//! most two operators deep) because SQLite turns an integer overflow into a real and an infinity
//! minus an infinity into a null, which no description of the subset can say.

use std::alloc::Global;
use std::fmt::{self, Write as _};
use std::ops::RangeInclusive;

use blaze_sql::Checked;
use blaze_sql::desc::{ResultCol, Returns, Slot, Storage, Ty, ValueRef};
use blaze_sql::infer::Plant;
use blaze_sql::schema::{Column, Schema, Table};
use blaze_sim::{Broke, Config, Tape};

use crate::{Connection, Statement, Step};

/// Table names; none is a keyword of the subset.
const TABLES: [&str; 5] = ["apple", "birch", "cedar", "dune", "ember"];

/// Column names besides each table's `id`; none is a keyword, and none looks like an alias
/// (`r0`, `r1`, ..), so a plain column's name never meets an alias's.
const COLUMNS: [&str; 10] =
    ["alpha", "beta", "gamma", "delta", "kappa", "sigma", "omega", "zeta", "theta", "lambda"];

const STORAGES: [Storage; 5] =
    [Storage::Boolean, Storage::Integer, Storage::Real, Storage::Text, Storage::Blob];

/// The storages `min` and `max` take.
const ORDERED: [Storage; 4] = [Storage::Integer, Storage::Real, Storage::Text, Storage::Blob];

const COMPARISONS: [&str; 7] = ["=", "<>", "!=", "<", "<=", ">", ">="];

/// Integer arithmetic; `%` is Integer only (the checker's rule: SQLite and Postgres compute a
/// real remainder differently).
const INTEGER_ARITHMETIC: [&str; 5] = ["+", "-", "*", "/", "%"];
const REAL_ARITHMETIC: [&str; 4] = ["+", "-", "*", "/"];

const PATTERNS: [&str; 5] = ["'%'", "'a%'", "'%b'", "'_'", "''"];

/// Text values that `like` patterns and equalities above tend to meet.
const SHORT_TEXTS: [&str; 5] = ["", "a", "ab", "%", "a_"];

/// The most rows one statement may return: three joined tables of eight rows are 512.
const MAX_STEPS: u32 = 4096;

/// How deep a result or filter expression nests.
const MAX_DEPTH: u32 = 2;

const CONTROL_CASES: u32 = 40;

/// What a run generates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    /// Any schema and statements of the subset.
    Any,
    /// Two or more tables, the first with rows and the second empty, and one `select` that
    /// left-joins the second to the first and returns all of the second's columns: every row
    /// has nulls in columns the schema declares non-null.
    LeftJoinUnmatched,
}

/// A generated table: its name and columns, one of them the Integer primary key `id`.
#[derive(Debug)]
struct TableSpec {
    name: &'static str,
    columns: Vec<Column<'static>>,
}

/// One generated run: the schema, how many rows each table gets and the `insert` that puts
/// them there, and the statements run after.
#[derive(Debug)]
struct Case {
    tables: Vec<TableSpec>,
    rows: Vec<usize>,
    inserts: Vec<String>,
    statements: Vec<String>,
}

impl Case {
    fn schema_text(&self) -> String {
        let mut out = String::new();
        for table in &self.tables {
            let _ = write!(out, "\n    {} (", table.name);
            for (at, column) in table.columns.iter().enumerate() {
                let sep = if at == 0 { "" } else { ", " };
                let null = if column.nullable { " null" } else { "" };
                let key = if column.primary_key { " pk" } else { "" };
                let _ = write!(out, "{sep}{} {:?}{null}{key}", column.name, column.storage);
            }
            out.push(')');
        }
        out
    }
}

/// A column in a statement's scope, as the statement writes it.
#[derive(Clone, Debug)]
struct ColRef {
    /// `t0.alpha`, `t1."beta"`, `GAMMA`: the reference as written.
    text: String,
    /// The schema's name.
    name: &'static str,
    storage: Storage,
    nullable: bool,
    primary_key: bool,
}

/// The statement generator: one test case's choices, and counters for the parameters and
/// aliases of the statement being written.
struct Gen<'t> {
    tape: &'t Tape,
    params: u32,
    aliases: u32,
}

impl Gen<'_> {
    /// A number in `0..n`; `n` is at least 1.
    fn below(&self, n: usize) -> usize {
        assert!(n > 0, "a choice among no options");
        self.tape.index(n, "below")
    }

    fn chance(&self, p: f64) -> bool {
        self.tape.chance(p, "chance")
    }

    fn storage(&self) -> Storage {
        STORAGES[self.below(STORAGES.len())]
    }

    fn numeric(&self) -> Storage {
        if self.chance(0.5) { Storage::Integer } else { Storage::Real }
    }

    /// A fresh parameter of this statement.
    fn param(&mut self) -> String {
        let name = format!("$p{}", self.params);
        self.params += 1;
        name
    }

    /// A fresh result alias, recorded as used.
    fn alias(&mut self, used: &mut Vec<String>) -> String {
        let name = format!("r{}", self.aliases);
        self.aliases += 1;
        used.push(name.clone());
        name
    }

    /// `name` as a statement may write it: as is, upper case, capitalized, or quoted.
    fn spell(&self, name: &str) -> String {
        match self.below(7) {
            0 => format!("\"{name}\""),
            1 => name.to_ascii_uppercase(),
            2 => {
                let mut spelled = name.to_owned();
                if let Some(first) = spelled.get_mut(0..1) {
                    first.make_ascii_uppercase();
                }
                spelled
            }
            _ => name.to_owned(),
        }
    }

    /// ` as t0` or ` t0`.
    fn alias_clause(&self, alias: &str) -> String {
        if self.chance(0.5) { format!(" as {alias}") } else { format!(" {alias}") }
    }

    fn scope_of(&self, table: &TableSpec, qualifier: Option<&str>) -> Vec<ColRef> {
        table
            .columns
            .iter()
            .map(|column| {
                let spelled = self.spell(column.name);
                ColRef {
                    text: match qualifier {
                        Some(qualifier) => format!("{qualifier}.{spelled}"),
                        None => spelled,
                    },
                    name: column.name,
                    storage: column.storage,
                    nullable: column.nullable,
                    primary_key: column.primary_key,
                }
            })
            .collect()
    }

    /// A column of `scope` of storage `storage`, if there is one.
    fn column_of(&self, scope: &[ColRef], storage: Storage) -> Option<ColRef> {
        let matching: Vec<&ColRef> = scope.iter().filter(|it| it.storage == storage).collect();
        if matching.is_empty() { None } else { Some(matching[self.below(matching.len())].clone()) }
    }

    fn literal(&self, storage: Storage) -> String {
        match storage {
            Storage::Boolean => (if self.chance(0.5) { "true" } else { "false" }).to_owned(),
            Storage::Integer => {
                let value = self.tape.int_in(0..=1000u32, "integer literal");
                if self.chance(0.2) { format!("(-{value})") } else { format!("{value}") }
            }
            Storage::Real => {
                let eighths = self.tape.int_in(0..=8000u32, "real literal");
                let value = f64::from(eighths) / 8.0;
                if self.chance(0.2) { format!("(-{value:?})") } else { format!("{value:?}") }
            }
            Storage::Text => {
                let text = text_without_nul(self.tape, 0..=4, "text literal");
                format!("'{}'", text.replace('\'', "''"))
            }
            Storage::Blob => {
                let bytes = self.tape.bytes(0..=4, "blob literal");
                let mut out = String::from("x'");
                for byte in bytes {
                    let _ = write!(out, "{byte:02x}");
                }
                out.push('\'');
                out
            }
        }
    }

    /// An expression with no operator: a column, a cast parameter, or a literal.
    fn leaf(&mut self, scope: &[ColRef], storage: Storage) -> String {
        let roll = self.below(8);
        if roll < 4
            && let Some(column) = self.column_of(scope, storage)
        {
            return column.text;
        }
        if roll == 4 {
            let param = self.param();
            return format!("cast({param} as {})", sql_type(storage));
        }
        self.literal(storage)
    }

    /// An expression of storage `storage` over `scope`, at most `depth` operators deep. It
    /// holds no aggregate.
    fn expr(&mut self, scope: &[ColRef], storage: Storage, depth: u32) -> String {
        if depth == 0 || self.chance(0.3) {
            return self.leaf(scope, storage);
        }
        let d = depth - 1;
        match storage {
            Storage::Boolean => self.predicate(scope, depth),
            Storage::Integer => match self.below(6) {
                0 => self.arithmetic(scope, storage, d),
                1 => {
                    let operand = self.expr(scope, storage, d);
                    format!("abs({operand})")
                }
                2 => {
                    let operand = self.expr(scope, Storage::Text, d);
                    format!("length({operand})")
                }
                3 => self.coalesce(scope, storage, d),
                4 => self.case(scope, storage, d),
                _ => {
                    let operand = self.expr(scope, Storage::Real, d);
                    format!("cast({operand} as integer)")
                }
            },
            Storage::Real => match self.below(5) {
                0 => self.arithmetic(scope, storage, d),
                1 => {
                    let operand = self.expr(scope, storage, d);
                    format!("abs({operand})")
                }
                2 => self.coalesce(scope, storage, d),
                3 => self.case(scope, storage, d),
                _ => {
                    let operand = self.expr(scope, Storage::Integer, d);
                    format!("cast({operand} as real)")
                }
            },
            Storage::Text => match self.below(6) {
                0 => {
                    let left = self.expr(scope, storage, d);
                    let right = self.expr(scope, storage, d);
                    format!("({left} || {right})")
                }
                1 => {
                    let operand = self.expr(scope, storage, d);
                    format!("lower({operand})")
                }
                2 => {
                    let operand = self.expr(scope, storage, d);
                    format!("upper({operand})")
                }
                3 => self.coalesce(scope, storage, d),
                4 => self.case(scope, storage, d),
                _ => {
                    let from = self.numeric();
                    let operand = self.expr(scope, from, d);
                    format!("cast({operand} as text)")
                }
            },
            Storage::Blob => match self.below(3) {
                0 => self.coalesce(scope, storage, d),
                1 => self.case(scope, storage, d),
                _ => {
                    let operand = self.expr(scope, Storage::Text, d);
                    format!("cast({operand} as blob)")
                }
            },
        }
    }

    fn arithmetic(&mut self, scope: &[ColRef], storage: Storage, d: u32) -> String {
        let left = self.expr(scope, storage, d);
        let ops: &[&str] =
            if storage == Storage::Real { &REAL_ARITHMETIC } else { &INTEGER_ARITHMETIC };
        let op = ops[self.below(ops.len())];
        let right = self.expr(scope, storage, d);
        format!("({left} {op} {right})")
    }

    fn coalesce(&mut self, scope: &[ColRef], storage: Storage, d: u32) -> String {
        let count = 2 + self.below(2);
        let mut args = Vec::new();
        for _ in 0..count {
            args.push(self.expr(scope, storage, d));
        }
        format!("coalesce({})", args.join(", "))
    }

    /// `case` with or without an operand; its branches have storage `storage`.
    fn case(&mut self, scope: &[ColRef], storage: Storage, d: u32) -> String {
        let mut text = String::from("(case");
        let arms = 1 + self.below(2);
        if self.chance(0.3) {
            let of = self.storage();
            let operand = self.expr(scope, of, d);
            let _ = write!(text, " {operand}");
            for _ in 0..arms {
                let when = self.operand_or_param(scope, of, d);
                let then = self.expr(scope, storage, d);
                let _ = write!(text, " when {when} then {then}");
            }
        } else {
            for _ in 0..arms {
                let when = self.expr(scope, Storage::Boolean, d);
                let then = self.expr(scope, storage, d);
                let _ = write!(text, " when {when} then {then}");
            }
        }
        if self.chance(0.6) {
            let otherwise = self.expr(scope, storage, d);
            let _ = write!(text, " else {otherwise}");
        }
        text.push_str(" end)");
        text
    }

    /// The far side of a comparison with something of storage `storage`: a parameter, which
    /// takes that storage, or an expression of it.
    fn operand_or_param(&mut self, scope: &[ColRef], storage: Storage, d: u32) -> String {
        if self.chance(0.4) { self.param() } else { self.expr(scope, storage, d) }
    }

    fn not(&self) -> &'static str {
        if self.chance(0.3) { "not " } else { "" }
    }

    /// A Boolean expression with an operator, its operands at most `depth - 1` deep.
    fn predicate(&mut self, scope: &[ColRef], depth: u32) -> String {
        let d = depth.saturating_sub(1);
        let of = self.storage();
        match self.below(10) {
            0 => {
                let left = self.expr(scope, of, d);
                let op = COMPARISONS[self.below(COMPARISONS.len())];
                let right = self.operand_or_param(scope, of, d);
                format!("({left} {op} {right})")
            }
            1 => {
                let left = self.expr(scope, of, d);
                let op = if self.chance(0.5) { "is" } else { "is not" };
                let right = self.operand_or_param(scope, of, d);
                format!("({left} {op} {right})")
            }
            2 => {
                let operand = self.expr(scope, of, d);
                let op = if self.chance(0.5) { "is null" } else { "is not null" };
                format!("({operand} {op})")
            }
            3 => {
                let operand = self.expr(scope, of, d);
                let not = self.not();
                let count = 1 + self.below(3);
                let mut entries = Vec::new();
                for _ in 0..count {
                    entries.push(self.operand_or_param(scope, of, d));
                }
                format!("({operand} {not}in ({}))", entries.join(", "))
            }
            4 => {
                let operand = self.expr(scope, of, d);
                let not = self.not();
                let list = self.param();
                format!("({operand} {not}in {list})")
            }
            5 => {
                let operand = self.expr(scope, of, d);
                let not = self.not();
                let low = self.operand_or_param(scope, of, d);
                let high = self.operand_or_param(scope, of, d);
                format!("({operand} {not}between {low} and {high})")
            }
            6 => {
                let operand = self.expr(scope, Storage::Text, d);
                let not = self.not();
                let pattern = match self.below(3) {
                    0 => self.param(),
                    1 => PATTERNS[self.below(PATTERNS.len())].to_owned(),
                    _ => self.expr(scope, Storage::Text, d),
                };
                format!("({operand} {not}like {pattern})")
            }
            7 => {
                let left = self.expr(scope, Storage::Boolean, d);
                let op = if self.chance(0.5) { "and" } else { "or" };
                let right = self.expr(scope, Storage::Boolean, d);
                format!("({left} {op} {right})")
            }
            8 => {
                let operand = self.expr(scope, Storage::Boolean, d);
                format!("(not {operand})")
            }
            _ => {
                // One parameter at two positions of one type: it binds once.
                let operand = self.leaf(scope, of);
                let param = self.param();
                format!("({operand} >= {param} and {operand} <= {param})")
            }
        }
    }

    fn aggregate(&mut self, scope: &[ColRef]) -> String {
        let numeric = self.numeric();
        match self.below(7) {
            0 => "count(*)".to_owned(),
            1 => {
                let of = self.storage();
                let operand = self.expr(scope, of, 1);
                format!("count({operand})")
            }
            2 => {
                let of = self.storage();
                let operand = self.expr(scope, of, 1);
                format!("count(distinct {operand})")
            }
            3 => {
                let operand = self.expr(scope, numeric, 1);
                format!("sum({operand})")
            }
            4 => {
                let of = ORDERED[self.below(ORDERED.len())];
                let operand = self.expr(scope, of, 1);
                let function = if self.chance(0.5) { "min" } else { "max" };
                format!("{function}({operand})")
            }
            5 => {
                let operand = self.expr(scope, numeric, 1);
                format!("avg({operand})")
            }
            _ => {
                let operand = self.expr(scope, numeric, 1);
                let zero = if numeric == Storage::Integer { "0" } else { "0.0" };
                format!("coalesce(sum({operand}), {zero})")
            }
        }
    }

    /// A plain column as a result: under its own name when no result has it yet, else under
    /// an alias.
    fn plain_result(&mut self, column: &ColRef, used: &mut Vec<String>) -> String {
        let lower = column.name.to_ascii_lowercase();
        if !used.contains(&lower) && self.chance(0.7) {
            used.push(lower);
            column.text.clone()
        } else {
            let alias = self.alias(used);
            format!("{} as {alias}", column.text)
        }
    }

    /// `count` result columns over `scope`: plain columns and aliased expressions.
    fn result_items(
        &mut self,
        scope: &[ColRef],
        count: usize,
        used: &mut Vec<String>,
    ) -> Vec<String> {
        let mut items = Vec::new();
        for _ in 0..count {
            if !scope.is_empty() && self.chance(0.45) {
                let column = scope[self.below(scope.len())].clone();
                items.push(self.plain_result(&column, used));
            } else {
                let of = self.storage();
                let depth = 1 + u32::try_from(self.below(MAX_DEPTH as usize)).unwrap_or(0);
                let expr = self.expr(scope, of, depth);
                let alias = self.alias(used);
                items.push(format!("{expr} as {alias}"));
            }
        }
        items
    }

    fn filter(&mut self, scope: &[ColRef]) -> String {
        if self.chance(0.6) {
            let depth = 1 + u32::try_from(self.below(MAX_DEPTH as usize)).unwrap_or(0);
            format!(" where {}", self.predicate(scope, depth))
        } else {
            String::new()
        }
    }

    fn limit(&mut self) -> String {
        if !self.chance(0.3) {
            return String::new();
        }
        let mut text = String::from(" limit ");
        if self.chance(0.5) {
            text.push_str(&self.param());
        } else {
            let _ = write!(text, "{}", self.below(10));
        }
        if self.chance(0.3) {
            text.push_str(" offset ");
            if self.chance(0.5) {
                text.push_str(&self.param());
            } else {
                let _ = write!(text, "{}", self.below(4));
            }
        }
        text
    }

    fn returning(&mut self, scope: &[ColRef]) -> String {
        if !self.chance(0.6) {
            return String::new();
        }
        let mut used = Vec::new();
        let mut items = Vec::new();
        let star = self.chance(0.2);
        if star {
            items.push("*".to_owned());
            used.extend(scope.iter().map(|it| it.name.to_ascii_lowercase()));
        }
        let count = if star { self.below(3) } else { 1 + self.below(3) };
        items.extend(self.result_items(scope, count, &mut used));
        format!(" returning {}", items.join(", "))
    }

    fn statement(&mut self, tables: &[TableSpec]) -> String {
        self.params = 0;
        self.aliases = 0;
        match self.below(10) {
            0..=5 => self.select(tables),
            6 | 7 => self.update(tables),
            _ => {
                let table = &tables[self.below(tables.len())];
                let scope = self.scope_of(table, None);
                self.delete(table, &scope)
            }
        }
    }

    fn select(&mut self, tables: &[TableSpec]) -> String {
        let first = &tables[self.below(tables.len())];
        let mut from = format!(" from {}{}", self.spell(first.name), self.alias_clause("t0"));
        let mut scope = self.scope_of(first, Some("t0"));
        let mut joined: Vec<(&TableSpec, String)> = vec![(first, "t0".to_owned())];
        for j in 1..=self.below(3) {
            let table = &tables[self.below(tables.len())];
            let alias = format!("t{j}");
            let kind = match self.below(4) {
                0 => "left join",
                1 => "left outer join",
                2 => "inner join",
                _ => "join",
            };
            let new = self.scope_of(table, Some(&alias));
            // Every table has its Integer `id`, so both sides have an Integer column.
            let left = self.column_of(&scope, Storage::Integer).expect("every table has an id");
            let right = self.column_of(&new, Storage::Integer).expect("every table has an id");
            let spelled = self.spell(table.name);
            let alias_clause = self.alias_clause(&alias);
            scope.extend(new);
            let mut on = format!("({} = {})", left.text, right.text);
            if self.chance(0.25) {
                let more = self.predicate(&scope, 1);
                on = format!("({on} and {more})");
            }
            let _ = write!(from, " {kind} {spelled}{alias_clause} on {on}");
            joined.push((table, alias));
        }

        let mut used: Vec<String> = Vec::new();
        let mut items: Vec<String> = Vec::new();
        let mut text = String::from("select ");
        if self.chance(0.25) {
            // Grouped: the grouping columns as they are, then aggregates.
            let mut groups: Vec<ColRef> = Vec::new();
            for _ in 0..self.below(3) {
                let column = scope[self.below(scope.len())].clone();
                if !groups.iter().any(|it| it.text == column.text) {
                    groups.push(column);
                }
            }
            for column in &groups {
                items.push(self.plain_result(column, &mut used));
            }
            for _ in 0..1 + self.below(3) {
                let aggregate = self.aggregate(&scope);
                let alias = self.alias(&mut used);
                items.push(format!("{aggregate} as {alias}"));
            }
            text.push_str(&items.join(", "));
            text.push_str(&from);
            text.push_str(&self.filter(&scope));
            if !groups.is_empty() {
                let list: Vec<&str> = groups.iter().map(|it| it.text.as_str()).collect();
                let _ = write!(text, " group by {}", list.join(", "));
            }
            if self.chance(0.3) {
                let bound =
                    if self.chance(0.5) { self.param() } else { format!("{}", self.below(3)) };
                let _ = write!(text, " having (count(*) >= {bound})");
            }
            if !groups.is_empty() && self.chance(0.4) {
                let terms: Vec<String> =
                    groups.iter().map(|it| format!("{}{}", it.text, self.direction())).collect();
                let _ = write!(text, " order by {}", terms.join(", "));
            }
        } else {
            let distinct = self.chance(0.15);
            if distinct {
                text.push_str("distinct ");
            }
            let roll = self.below(7);
            let star = if roll == 0 && joined.len() == 1 {
                used.extend(first.columns.iter().map(|it| it.name.to_ascii_lowercase()));
                Some("*".to_owned())
            } else if roll == 1 {
                let (table, alias) = &joined[self.below(joined.len())];
                used.extend(table.columns.iter().map(|it| it.name.to_ascii_lowercase()));
                Some(format!("{alias}.*"))
            } else {
                None
            };
            let count = if star.is_some() { self.below(3) } else { 1 + self.below(4) };
            items.extend(star);
            items.extend(self.result_items(&scope, count, &mut used));
            text.push_str(&items.join(", "));
            text.push_str(&from);
            text.push_str(&self.filter(&scope));
            if !distinct && self.chance(0.4) {
                let mut terms = Vec::new();
                for _ in 0..1 + self.below(2) {
                    let column = scope[self.below(scope.len())].text.clone();
                    terms.push(format!("{column}{}", self.direction()));
                }
                let _ = write!(text, " order by {}", terms.join(", "));
            }
        }
        text.push_str(&self.limit());
        text
    }

    fn direction(&self) -> &'static str {
        match self.below(3) {
            0 => " asc",
            1 => " desc",
            _ => "",
        }
    }

    /// `update` of one table: its key is never set (it would collide), and a column is set to
    /// a parameter, a literal, null where nullable, or another column that is null no more
    /// often than it may be. No arithmetic: a value grown by one update would overflow in the
    /// next.
    fn update(&mut self, tables: &[TableSpec]) -> String {
        let table = &tables[self.below(tables.len())];
        let scope = self.scope_of(table, None);
        let mut targets: Vec<ColRef> = scope.iter().filter(|it| !it.primary_key).cloned().collect();
        if targets.is_empty() {
            return self.delete(table, &scope);
        }
        let mut sets = Vec::new();
        for _ in 0..1 + self.below(targets.len().min(2)) {
            let target = targets.remove(self.below(targets.len()));
            let roll = self.below(8);
            let value = if roll == 0 && target.nullable {
                "null".to_owned()
            } else if roll <= 3 {
                self.param()
            } else if roll <= 5 {
                let sources: Vec<&ColRef> = scope
                    .iter()
                    .filter(|it| {
                        it.storage == target.storage
                            && (target.nullable || !it.nullable)
                            && it.name != target.name
                    })
                    .collect();
                if sources.is_empty() {
                    self.literal(target.storage)
                } else {
                    sources[self.below(sources.len())].text.clone()
                }
            } else {
                self.literal(target.storage)
            };
            sets.push(format!("{} = {value}", target.text));
        }
        let mut text = format!("update {} set {}", self.spell(table.name), sets.join(", "));
        text.push_str(&self.filter(&scope));
        text.push_str(&self.returning(&scope));
        text
    }

    fn delete(&mut self, table: &TableSpec, scope: &[ColRef]) -> String {
        let mut text = format!("delete from {}", self.spell(table.name));
        text.push_str(&self.filter(scope));
        text.push_str(&self.returning(scope));
        text
    }

    /// The control's statement: the second table, which has no rows, left-joined to the
    /// first on the keys, with all its columns and some of the first's among the results. It
    /// holds only columns and the join, nothing of the expression generator, so the honest
    /// checker accepts it whatever is drawn.
    fn unmatched_left_join(&mut self, tables: &[TableSpec]) -> String {
        let (left_table, right_table) = (&tables[0], &tables[1]);
        let mut items = vec!["t1.*".to_owned()];
        for (at, column) in left_table.columns.iter().enumerate() {
            if self.chance(0.5) {
                items.push(format!("t0.{} as r{at}", column.name));
            }
        }
        format!(
            "select {} from {} as t0 left join {} as t1 on (t0.id = t1.id)",
            items.join(", "),
            left_table.name,
            right_table.name,
        )
    }
}

fn sql_type(storage: Storage) -> &'static str {
    match storage {
        Storage::Boolean => "boolean",
        Storage::Integer => "integer",
        Storage::Real => "real",
        Storage::Text => "text",
        Storage::Blob => "blob",
    }
}

/// `sizes` characters of `Tape::char`, none of them NUL: a NUL drawn is redrawn, in its own
/// span, at most eight times before the case is discarded.
fn text_without_nul(tape: &Tape, sizes: RangeInclusive<usize>, label: &'static str) -> String {
    let characters = tape.vec(sizes, label, |tape| {
        for _ in 0..8 {
            match tape.char(label) {
                '\0' => {}
                character => return character,
            }
        }
        tape.discard()
    });
    characters.into_iter().collect()
}

fn draw_case(tape: &Tape, shape: Shape) -> Case {
    let mut g = Gen { tape, params: 0, aliases: 0 };
    let table_count = match shape {
        Shape::Any => 1 + g.below(3),
        Shape::LeftJoinUnmatched => 2 + g.below(2),
    };
    let mut free_tables: Vec<&'static str> = TABLES.to_vec();
    let mut tables = Vec::new();
    for _ in 0..table_count {
        let name = free_tables.remove(g.below(free_tables.len()));
        let mut free_columns: Vec<&'static str> = COLUMNS.to_vec();
        let mut columns = Vec::new();
        for _ in 0..g.below(5) {
            let column = free_columns.remove(g.below(free_columns.len()));
            let storage = g.storage();
            columns.push(Column {
                name: column,
                storage,
                rust: storage.plain(),
                nullable: g.chance(0.4),
                primary_key: false,
            });
        }
        let at = g.below(columns.len() + 1);
        columns.insert(
            at,
            Column {
                name: "id",
                storage: Storage::Integer,
                rust: Storage::Integer.plain(),
                nullable: false,
                primary_key: true,
            },
        );
        tables.push(TableSpec { name, columns });
    }
    let rows: Vec<usize> = (0..table_count)
        .map(|at| match (shape, at) {
            (Shape::LeftJoinUnmatched, 0) => 1 + g.below(8),
            (Shape::LeftJoinUnmatched, 1) => 0,
            _ => g.below(9),
        })
        .collect();
    let inserts: Vec<String> = tables
        .iter()
        .map(|table| {
            // The control's inserts are written plainly, so nothing drawn can make them refused.
            let names: Vec<String> = table
                .columns
                .iter()
                .map(|it| if shape == Shape::Any { g.spell(it.name) } else { it.name.to_owned() })
                .collect();
            let params: Vec<String> =
                table.columns.iter().map(|it| format!("${}", it.name)).collect();
            let table_name =
                if shape == Shape::Any { g.spell(table.name) } else { table.name.to_owned() };
            format!(
                "insert into {} ({}) values ({})",
                table_name,
                names.join(", "),
                params.join(", ")
            )
        })
        .collect();
    let statements = match shape {
        Shape::Any => {
            let count = 1 + g.below(4);
            let mut statements = Vec::new();
            for _ in 0..count {
                statements.push(g.statement(&tables));
            }
            statements
        }
        Shape::LeftJoinUnmatched => vec![g.unmatched_left_join(&tables)],
    };
    Case { tables, rows, inserts, statements }
}

/// A value owned, to bind.
#[derive(Clone, Debug, PartialEq)]
enum Owned {
    Null,
    Boolean(bool),
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Owned {
    fn as_ref(&self) -> ValueRef<'_> {
        match self {
            Owned::Null => ValueRef::Null,
            Owned::Boolean(value) => ValueRef::Boolean(*value),
            Owned::Integer(value) => ValueRef::Integer(*value),
            Owned::Real(value) => ValueRef::Real(*value),
            Owned::Text(value) => ValueRef::Text(value),
            Owned::Blob(value) => ValueRef::Blob(value),
        }
    }
}

/// What one parameter binds: its value, or a list parameter's items.
#[derive(Debug)]
enum Bound {
    Scalar(Owned),
    List(Vec<Owned>),
}

/// What a run asked a value for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Want {
    Value { storage: Storage, nullable: bool },
    ListLen,
}

/// The values a run binds: drawn from the test case and logged, or replayed from a log, so a
/// second run of one case (the control's planted one) binds what the first bound.
struct Values<'t> {
    tape: Option<&'t Tape>,
    log: Vec<(Want, Owned)>,
    at: usize,
}

impl<'t> Values<'t> {
    fn drawing(tape: &'t Tape) -> Values<'t> {
        Values { tape: Some(tape), log: Vec::new(), at: 0 }
    }

    fn replaying(self) -> Values<'static> {
        Values { tape: None, log: self.log, at: 0 }
    }

    fn next(&mut self, want: Want) -> Owned {
        if let Some(tape) = self.tape {
            let value = draw_wanted(tape, want);
            self.log.push((want, value.clone()));
            return value;
        }
        let (logged, value) = match self.log.get(self.at) {
            Some((logged, value)) => (*logged, value.clone()),
            None => panic!(
                "the replayed run asked for value {} of a run that drew {}",
                self.at,
                self.log.len()
            ),
        };
        assert_eq!(logged, want, "the replayed run asked value {} for something else", self.at);
        self.at += 1;
        value
    }

    fn value(&mut self, ty: Ty<'_>) -> Owned {
        self.next(Want::Value { storage: ty.storage, nullable: ty.nullable })
    }

    fn list_len(&mut self) -> u32 {
        match self.next(Want::ListLen) {
            Owned::Integer(len) => u32::try_from(len).expect("a list length is drawn in 0..=3"),
            other => panic!("a list length logged as {other:?}"),
        }
    }
}

fn draw_wanted(tape: &Tape, want: Want) -> Owned {
    match want {
        Want::ListLen => Owned::Integer(tape.int_in(0..=3i64, "list length")),
        Want::Value { storage, nullable } => {
            if nullable && tape.chance(0.25, "null") {
                return Owned::Null;
            }
            match storage {
                Storage::Boolean => Owned::Boolean(tape.bool("boolean")),
                Storage::Integer => {
                    // Mostly near the keys, so joins and equalities meet.
                    if tape.chance(0.6, "near a key") {
                        Owned::Integer(tape.int_in(0..=9i64, "integer"))
                    } else {
                        Owned::Integer(tape.int_in(-1000..=1000i64, "integer"))
                    }
                }
                Storage::Real => {
                    let eighths = tape.int_in(-8000..=8000i32, "real");
                    Owned::Real(f64::from(eighths) / 8.0)
                }
                Storage::Text => {
                    if tape.chance(0.3, "short text") {
                        Owned::Text((*tape.one_of(&SHORT_TEXTS, "short text")).to_owned())
                    } else {
                        Owned::Text(tape.text(0..=4, "text"))
                    }
                }
                Storage::Blob => Owned::Blob(tape.bytes(0..=4, "blob")),
            }
        }
    }
}

/// Where in a run something went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Open,
    Prepare,
    Bind,
    Step,
    /// The driver's `column` refused a value.
    Column,
    /// This file's own check of a value against the description refused it.
    Admit,
}

#[derive(Debug)]
struct Failure {
    stage: Stage,
    context: String,
    message: String,
}

impl Failure {
    fn new(stage: Stage, context: &str, message: impl Into<String>) -> Failure {
        Failure { stage, context: context.to_owned(), message: message.into() }
    }

    fn driver(stage: Stage, context: &str, error: impl fmt::Display) -> Failure {
        Failure::new(stage, context, error.to_string())
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}\n  statement: {}", self.stage, self.message, self.context)
    }
}

/// `text` checked against `schema`, with `plant` planted when there is one. A refusal is a
/// generator bug: the generator only builds statements that type.
fn checked<'s>(schema: &Schema<'s>, text: &'s str, plant: Option<Plant>) -> Checked<'s, Global> {
    let result = match plant {
        None => blaze_sql::check(schema, text, Global),
        Some(plant) => blaze_sql::check_planted(schema, text, plant, Global),
    };
    match result {
        Ok(checked) => checked,
        Err(error) => {
            panic!("the generator built a statement the checker refuses:\n  {text}\n  {error}")
        }
    }
}

/// Binds every placeholder of `statement` from `bound`, indexed by parameter.
fn bind(statement: &mut Statement<'_>, bound: &[Bound], context: &str) -> Result<(), Failure> {
    let slots: Vec<Slot> = statement.slots().to_vec();
    for (at, slot) in slots.iter().enumerate() {
        let k = u32::try_from(at + 1).expect("fewer than 2^32 placeholders");
        let value = match *slot {
            Slot::Scalar { param } => match bound.get(param as usize) {
                Some(Bound::Scalar(value)) => value,
                other => {
                    return Err(Failure::new(
                        Stage::Bind,
                        context,
                        format!(
                            "placeholder {k} binds parameter {param} whole, which is {other:?}"
                        ),
                    ));
                }
            },
            Slot::ListItem { param, index } => match bound.get(param as usize) {
                Some(Bound::List(items)) => match items.get(index as usize) {
                    Some(value) => value,
                    None => {
                        return Err(Failure::new(
                            Stage::Bind,
                            context,
                            format!(
                                "placeholder {k} binds item {index} of parameter {param}, which has {}",
                                items.len()
                            ),
                        ));
                    }
                },
                other => {
                    return Err(Failure::new(
                        Stage::Bind,
                        context,
                        format!(
                            "placeholder {k} binds an item of parameter {param}, which is {other:?}"
                        ),
                    ));
                }
            },
            Slot::Array { param } => {
                return Err(Failure::new(
                    Stage::Bind,
                    context,
                    format!(
                        "placeholder {k} binds parameter {param} as an array, which SQLite has no bind for"
                    ),
                ));
            }
        };
        statement
            .bind(k, value.as_ref())
            .map_err(|error| Failure::driver(Stage::Bind, context, error))?;
    }
    Ok(())
}

/// The description admits `value` in `column`: null only where nullable, and otherwise of the
/// described storage (a Boolean is an integer 0 or 1 in SQLite).
fn admits(column: &ResultCol<'_>, value: ValueRef<'_>) -> Result<(), String> {
    let ty = column.ty;
    let fits = match value {
        ValueRef::Null => ty.nullable,
        ValueRef::Integer(0 | 1) if ty.storage == Storage::Boolean => true,
        other => other.storage() == Some(ty.storage),
    };
    if fits {
        return Ok(());
    }
    let null = if ty.nullable { "nullable" } else { "non-null" };
    Err(format!(
        "result column `{}` is described as {null} {:?} and SQLite returned {value:?}",
        column.name, ty.storage
    ))
}

/// Steps `statement` to its end, holding every value of every row to `columns`. Returns the
/// rows it changed.
fn drain(
    statement: &mut Statement<'_>,
    columns: &[ResultCol<'_>],
    context: &str,
) -> Result<u64, Failure> {
    for _ in 0..MAX_STEPS {
        match statement.step().map_err(|error| Failure::driver(Stage::Step, context, error))? {
            Step::Done { changed } => return Ok(changed),
            Step::Row => {
                if columns.is_empty() {
                    return Err(Failure::new(
                        Stage::Admit,
                        context,
                        "SQLite returned a row of a statement described as returning a count",
                    ));
                }
                for (index, column) in columns.iter().enumerate() {
                    let index = u32::try_from(index).expect("fewer than 2^32 columns");
                    let value = statement
                        .column(index)
                        .map_err(|error| Failure::driver(Stage::Column, context, error))?;
                    admits(column, value)
                        .map_err(|why| Failure::new(Stage::Admit, context, why))?;
                }
            }
        }
    }
    Err(Failure::new(Stage::Step, context, format!("more than {MAX_STEPS} rows")))
}

/// Puts `rows` rows into a table through its checked `insert`, prepared once and reset
/// between rows. The key `id` is the row's number; every other value is drawn for its
/// parameter's described type.
fn fill(
    connection: &Connection,
    schema: &Schema<'_>,
    text: &str,
    rows: usize,
    values: &mut Values<'_>,
    plant: Option<Plant>,
) -> Result<(), Failure> {
    let checked = checked(schema, text, plant);
    let description = checked.description();
    if !matches!(description.returns, Returns::Count) {
        return Err(Failure::new(
            Stage::Prepare,
            text,
            "an insert without returning is described as returning rows",
        ));
    }
    let mut statement = connection
        .prepare(&checked, &[])
        .map_err(|error| Failure::driver(Stage::Prepare, text, error))?;
    for row in 0..rows {
        let mut bound = Vec::new();
        for param in &description.params {
            if param.list {
                return Err(Failure::new(
                    Stage::Bind,
                    text,
                    format!("`${}` of an insert is described as a list", param.name),
                ));
            }
            let value = if param.name == "id" {
                Owned::Integer(i64::try_from(row + 1).expect("at most eight rows"))
            } else {
                values.value(param.ty)
            };
            bound.push(Bound::Scalar(value));
        }
        let context = format!("{text}\n  bound: {bound:?}");
        bind(&mut statement, &bound, &context)?;
        let changed = drain(&mut statement, &[], &context)?;
        if changed != 1 {
            return Err(Failure::new(
                Stage::Step,
                &context,
                format!("an insert of one row changed {changed}"),
            ));
        }
        statement.reset().map_err(|error| Failure::driver(Stage::Step, &context, error))?;
    }
    Ok(())
}

/// Checks, prepares, binds and runs one statement, reading every column of every row.
fn run_statement(
    connection: &Connection,
    schema: &Schema<'_>,
    text: &str,
    values: &mut Values<'_>,
    plant: Option<Plant>,
) -> Result<(), Failure> {
    let checked = checked(schema, text, plant);
    let description = checked.description();
    let mut bound = Vec::new();
    let mut list_lens = Vec::new();
    for param in &description.params {
        if param.list {
            let len = values.list_len();
            let mut items = Vec::new();
            for _ in 0..len {
                items.push(values.value(param.ty));
            }
            list_lens.push(len);
            bound.push(Bound::List(items));
        } else {
            bound.push(Bound::Scalar(values.value(param.ty)));
        }
    }
    let context = format!("{text}\n  bound: {bound:?}");
    let mut statement = connection
        .prepare(&checked, &list_lens)
        .map_err(|error| Failure::driver(Stage::Prepare, &context, error))?;
    bind(&mut statement, &bound, &context)?;
    let columns: &[ResultCol<'_>] = match &description.returns {
        Returns::Rows(columns) => columns,
        Returns::Count => &[],
    };
    drain(&mut statement, columns, &context)?;
    Ok(())
}

/// Runs `case` on a fresh in-memory database: the schema, the rows, then every statement.
fn run_case(case: &Case, values: &mut Values<'_>, plant: Option<Plant>) -> Result<(), Failure> {
    let tables: Vec<Table<'_>> = case
        .tables
        .iter()
        .map(|table| Table { name: table.name, columns: &table.columns })
        .collect();
    let refs: Vec<&Table<'_>> = tables.iter().collect();
    let schema = Schema { tables: &refs };
    let connection =
        Connection::open_memory().map_err(|error| Failure::driver(Stage::Open, "", error))?;
    connection.create(&schema).map_err(|error| Failure::driver(Stage::Open, "create", error))?;
    for (insert, &rows) in case.inserts.iter().zip(&case.rows) {
        if rows > 0 {
            fill(&connection, &schema, insert, rows, values, plant)?;
        }
    }
    for text in &case.statements {
        run_statement(&connection, &schema, text, values, plant)?;
    }
    Ok(())
}

/// The refinement: for a generated schema, generated rows and generated statements of the
/// subset that the checker accepts, real SQLite prepares each statement as its description
/// says (placeholders, result width and names), and every value of every row it returns is
/// admitted by the description: null only where nullable, otherwise of the described storage.
#[test]
fn every_row_sqlite_returns_is_admitted_by_the_description() {
    blaze_sim::check(Config::cases(300), |tape| {
        let case = draw_case(tape, Shape::Any);
        let mut values = Values::drawing(tape);
        run_case(&case, &mut values, None)
            .map_err(|failure| format!("{failure}\n  schema:{}", case.schema_text()))
    });
}

/// A checker that keeps a left join's right side non-null describes columns SQLite fills
/// with null when the right side has no matching row; the oracle must see that within the
/// case count. Each case runs honestly first (which must pass) and then planted, binding the
/// same values; a planted run refused by `column` or by this file's admission check is the
/// catch, and one refused at any other stage is the harness's fault.
#[test]
fn the_oracle_catches_a_left_join_kept_non_null() {
    blaze_sim::control(
        Config::cases(CONTROL_CASES),
        None,
        Some(Plant::LeftJoinKeepsNonNull),
        "",
        |tape, plant| {
            let case = draw_case(tape, Shape::LeftJoinUnmatched);
            let mut values = Values::drawing(tape);
            run_case(&case, &mut values, None).map_err(|failure| {
                Broke::Harness(format!(
                    "the honest checker failed the control's case: {failure}\n  schema:{}",
                    case.schema_text()
                ))
            })?;
            let mut replay = values.replaying();
            match run_case(&case, &mut replay, plant) {
                Ok(()) => Ok(()),
                Err(failure) if matches!(failure.stage, Stage::Column | Stage::Admit) => {
                    Err(Broke::Oracle(format!("{failure}\n  schema:{}", case.schema_text())))
                }
                Err(failure) => Err(Broke::Harness(format!(
                    "the planted run failed for another reason: {failure}"
                ))),
            }
        },
    );
}
