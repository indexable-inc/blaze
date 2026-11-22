//! The query grammar over its workload: every view of blaze-profile's views.sql and every named
//! query of its examples.sql parses (docs/v2/plan/design/query.md, "Census of the workload").

extern crate std;

use alloc::alloc::Global;
use std::string::String;
use std::vec::Vec;
use std::{eprintln, format};

use crate::parse::{parse_query, parse_script};
use crate::query::Statement;

const VIEWS: &str = include_str!("../../../blaze-profile/sql/views.sql");
const EXAMPLES: &str = include_str!("../../../blaze-profile/sql/examples.sql");

/// The named queries of examples.sql: each from its `-- name:` line to the next.
pub(crate) fn examples() -> Vec<(&'static str, &'static str)> {
    let mut out = Vec::new();
    let mut rest = EXAMPLES;
    while let Some(at) = rest.find("-- name: ") {
        let after = &rest[at + "-- name: ".len()..];
        let name_end = after.find('\n').unwrap_or(after.len());
        let name = after[..name_end].trim();
        let body = &after[name_end..];
        let end = body.find("-- name: ").unwrap_or(body.len());
        out.push((name, &body[..end]));
        rest = &body[end..];
    }
    out
}

#[test]
fn every_view_parses() {
    let statements = match parse_script(VIEWS, Global) {
        Ok(it) => it,
        Err(error) => {
            let start = error.span.range().start;
            panic!("views.sql at byte {start}: {error} near {:?}", VIEWS.get(start..start + 80));
        }
    };
    let views = statements.iter().filter(|it| matches!(it, Statement::CreateView { .. })).count();
    assert_eq!(views, statements.len(), "views.sql holds only views");
    assert!(views >= 40, "views.sql has {views} views");
}

#[test]
fn every_named_query_parses() {
    let examples = examples();
    assert!(examples.len() >= 50, "examples.sql has {} named queries", examples.len());
    let mut failed: Vec<String> = Vec::new();
    for (name, text) in &examples {
        if let Err(error) = parse_query(text, Global) {
            let start = error.span.range().start;
            let near = text.get(start..(start + 60).min(text.len())).unwrap_or("");
            failed.push(format!("{name}: {error} near {near:?}"));
        }
    }
    for it in &failed {
        eprintln!("{it}");
    }
    assert!(failed.is_empty(), "{} of {} named queries do not parse", failed.len(), examples.len());
}
