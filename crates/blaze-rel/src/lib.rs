//! blaze's own query engine for the build profile (docs/v2/plan/design/query.md), in place of
//! the DuckDB binary `blaze debug sql`, `blaze explain`, `blaze slow` and `blaze why` ran.
//!
//! A query goes through three pure steps:
//!
//! 1. blaze-sql's query grammar parses it ([`blaze_sql::parse::parse_query`]); the views of the
//!    profile (views.sql) are parsed once into a [`Prelude`].
//! 2. [`bind`] resolves every name against the tables of a [`Catalog`], the prelude's views and
//!    the query's CTEs, and plans it into the operators of [`plan`].
//! 3. [`exec`] runs the plan over the tables' chunks, which a [`Table`] hands over as
//!    blaze-columns batches; the answer is [`Rows`] of blaze-data values, which print as the
//!    aligned table or the JSON lines `blaze debug sql` prints.
//!
//! The engine owns no IO, clock or thread and allocates only in the allocator it is handed.
//! Every query is bounded ([`Bounds`], rule 18); a refusal names the statement, the bytes and
//! the rule ([`QueryError`]); what it does not run yet it refuses by name (rule 3).

// Under test the crate has std, which its tests and blaze-sim's checks run on.
#![cfg_attr(not(test), no_std)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![forbid(unsafe_code)]

pub mod agg;
pub mod bind;
pub mod datum;
pub mod error;
pub mod eval;
pub mod exec;
pub mod out;
pub mod plan;
pub mod table;

#[cfg(test)]
mod tests;

use core::alloc::Allocator;

use blaze_own::Vec;
use blaze_sql::parse::{parse_query, parse_script};
use blaze_sql::query::Statement;

pub use crate::error::{ErrorKind, QueryError};
pub use crate::out::Rows;
pub use crate::table::{Catalog, MemTable, Stats, Table};

/// The bounds of one query (rule 18): past one, the query is refused with
/// [`ErrorKind::Bound`] or [`ErrorKind::Recursion`], naming it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    /// Rounds of one recursive CTE.
    pub rounds: u64,
    /// Rows of one relation: a scan's, a join's, a recursive CTE's whole result.
    pub rows: u64,
    /// Bytes of distinct text a query holds.
    pub text_bytes: u64,
}

impl Bounds {
    /// A profile of four million span rows is about a gigabyte of datums; these are a few
    /// times that (query.md, "Memory").
    pub const PROFILE: Bounds = Bounds { rounds: 1_000_000, rows: 16 << 20, text_bytes: 1 << 30 };
}

/// A parameter's value: what `$name` stands for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Arg<'a> {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(&'a str),
}

/// The views every query may name (views.sql), parsed once.
pub struct Prelude<'s, A: Allocator> {
    pub(crate) text: &'s str,
    pub(crate) statements: Vec<Statement<'s, A>, A>,
}

impl<'s, A: Allocator + Copy> Prelude<'s, A> {
    /// No views.
    pub fn empty(alloc: A) -> Prelude<'s, A> {
        Prelude { text: "", statements: Vec::new_in(alloc) }
    }

    /// The views of `text`: statements `create [or replace] view name as query`, each ended by
    /// `;`. A statement of another kind is refused.
    pub fn parse(text: &'s str, alloc: A) -> Result<Prelude<'s, A>, QueryError<'s, A>> {
        let parsed = parse_script(text, alloc).map_err(|error| QueryError {
            statement: "the views' prelude",
            at: Some((error.span.start, error.span.end)),
            kind: ErrorKind::Syntax(error),
        })?;
        let mut statements = Vec::with_capacity_in(parsed.len(), alloc);
        for statement in parsed {
            if let Statement::Query(query) = &statement {
                return Err(QueryError {
                    statement: "the views' prelude",
                    at: Some((query.span.start, query.span.end)),
                    kind: ErrorKind::NotOneQuery { found: 0 },
                });
            }
            statements.push(statement);
        }
        Ok(Prelude { text, statements })
    }

    /// The views' names, in order.
    pub(crate) fn names(&self, alloc: A) -> Vec<&'s str, A> {
        let mut out = Vec::with_capacity_in(self.statements.len(), alloc);
        for statement in self.statements.iter() {
            out.push(match statement {
                Statement::CreateView { name, .. } => name.text,
                Statement::Query(_) => "a query",
            });
        }
        out
    }
}

/// What a query runs with besides its tables: the views, the parameters and the bounds.
pub struct Session<'s, 'p, A: Allocator> {
    pub prelude: &'p Prelude<'s, A>,
    pub params: &'p [(&'s str, Arg<'s>)],
    pub bounds: Bounds,
}

/// Runs one query over `tables`, with no views or parameters and the profile's bounds.
pub fn run<'s, B: Allocator, A: Allocator + Copy>(
    sql: &'s str,
    tables: &Catalog<'s, B, A>,
    alloc: A,
) -> Result<Rows<A>, QueryError<'s, A>> {
    let prelude = Prelude::empty(alloc);
    let session = Session { prelude: &prelude, params: &[], bounds: Bounds::PROFILE };
    run_with(sql, &session, tables, alloc)
}

/// Runs one query over `tables` with `session`'s views, parameters and bounds.
pub fn run_with<'s, B: Allocator, A: Allocator + Copy>(
    sql: &'s str,
    session: &Session<'s, '_, A>,
    tables: &Catalog<'s, B, A>,
    alloc: A,
) -> Result<Rows<A>, QueryError<'s, A>> {
    let statement_name = first_line(sql);
    let views = session.prelude.names(alloc);
    let statement = parse_query(sql, alloc).map_err(|error| QueryError {
        statement: statement_name,
        at: Some((error.span.start, error.span.end)),
        kind: ErrorKind::Syntax(error),
    })?;
    let Statement::Query(query) = &statement else {
        return Err(QueryError {
            statement: statement_name,
            at: None,
            kind: ErrorKind::Unsupported("`create view` in a query (views belong in the prelude)"),
        });
    };
    let mut store =
        datum::Store::new_in(usize::try_from(session.bounds.text_bytes).unwrap_or(usize::MAX), alloc);
    let planned = bind::plan(query, sql, session, tables, &mut store, alloc)
        .map_err(|fail| fail.named(statement_name, &views))?;
    let mut ctx = exec::Ctx::new(&planned, tables, store, session.bounds, alloc);
    let rel = ctx.exec(&planned.root).map_err(|fail| fail.named(statement_name, &views))?;
    Ok(out::Rows::of(&planned.names, &rel, &ctx.store, alloc))
}

/// A query's name in a message: its first line that is not blank or a comment.
fn first_line(sql: &str) -> &str {
    sql.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("--"))
        .unwrap_or("the query")
}
