//! The schema as a reader asks for it: which tables there are, a table's columns with their
//! types and meaning, the views by name (`blaze debug schema`), the answers to `DESCRIBE` and
//! `SHOW TABLES` (`blaze debug sql`, before DuckDB is started), and, where DuckDB refuses a
//! query for a column it does not know, the columns the query's tables do have, the nearest
//! names first.
//!
//! All of it is made from [`crate::schema`], the one definition, so it is the schema of the
//! blaze that prints it: on 2026-11-10 each query refused for a column the reader guessed
//! (`tid`, `kind`, a quoted `"unit"`) cost a round trip to a VM, and `DESCRIBE` was not
//! supported. docs/profiling.md's tables are written by hand; a test here holds each column
//! they list to the schema.
//!
//! Sans-IO: text in, text out.

use std::fmt::Write as _;

use serde_derive::Serialize;

use crate::{
    schema::{self, Column, Table, Type},
    span, views,
};

/// A column's type as docs/profiling.md writes it: `u64`, `dict`, `stack`, with `?` after a
/// nullable one.
pub fn type_name(column: &Column) -> String {
    let name = match column.ty {
        Type::U16 => "u16",
        Type::U32 => "u32",
        Type::U64 => "u64",
        Type::I64 => "i64",
        Type::F64 => "f64",
        Type::Bool => "bool",
        Type::Utf8 => "utf8",
        Type::Dict => "dict",
        Type::Stack => "stack",
        Type::Pcs => "pcs",
    };
    // alloc: a cell, once a column a listing
    format!("{name}{}", if column.nullable { "?" } else { "" })
}

/// Every table a query may name: the history's and the results made on request.
pub fn tables() -> impl Iterator<Item = &'static Table> {
    schema::TABLES.iter().chain(&schema::RESULTS).copied()
}

/// The table named `name`.
pub fn table(name: &str) -> Option<&'static Table> {
    tables().find(|it| it.name == name)
}

/// One row of a listing: a table or view with what it holds, or a column with its type and
/// meaning. What `--json` prints, a line each.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Row {
    /// `table`, `span view` (a span kind's rows, `span_macro_expand`), `view` (a named query of
    /// `sql/examples.sql`) or `column`.
    pub kind: &'static str,
    /// The table a column is of; none for a table or a view.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
    pub name: String,
    /// A column's type; a view's arguments, `build limit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ty: Option<String>,
    pub says: String,
}

/// Why a listing was not made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoSuchTable {
    pub name: String,
    /// The tables and views nearest that name, nearest first.
    pub nearest: Vec<String>,
}

impl std::fmt::Display for NoSuchTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the profile has no table or view named `{}`; the nearest are {} (`blaze debug schema` lists them all)",
            self.name,
            self.nearest.join(", ")
        )
    }
}

/// The name a span kind's view has: `span_macro_expand` for `macro expand`.
fn span_view(kind: &str) -> String {
    // alloc: a name, once a kind a listing
    format!("span_{}", kind.replace(' ', "_"))
}

/// Every table, span view and named view, each with what it holds: `blaze debug schema`.
pub fn listing() -> Vec<Row> {
    // alloc: the listing, once a command
    let mut out = Vec::new();
    for it in tables() {
        out.push(Row {
            kind: "table",
            table: None,
            name: it.name.to_owned(), // alloc: a cell
            ty: None,
            says: it.doc.to_owned(), // alloc: a cell
        });
    }
    for kind in span::KINDS {
        out.push(Row {
            kind: "span view",
            table: None,
            name: span_view(kind.name),
            ty: None,
            // alloc: a cell
            says: format!("the `{}` spans of `spans`, their fields as columns", kind.name),
        });
    }
    for view in views::VIEWS {
        // alloc: a cell
        let args: Vec<&str> = view.args.iter().map(|it| it.name).collect();
        out.push(Row {
            kind: "view",
            table: None,
            name: view.name.to_owned(), // alloc: a cell
            ty: Some(args.join(" ")),
            says: view.title.to_owned(), // alloc: a cell
        });
    }
    out
}

/// The columns of the table, span view or named view `name`. A table's are the schema's; a
/// span view's are its kind's fields over the columns every such view has; a named view has no
/// declared columns, so its row says its arguments and what it answers.
pub fn columns(name: &str) -> Result<Vec<Row>, NoSuchTable> {
    let column = |table: &str, it: &Column| Row {
        kind: "column",
        table: Some(table.to_owned()), // alloc: a cell
        name: it.name.to_owned(),      // alloc: a cell
        ty: Some(type_name(it)),
        says: it.doc.to_owned(), // alloc: a cell
    };
    if let Some(table) = table(name) {
        return Ok(table.columns.iter().map(|it| column(table.name, it)).collect());
    }
    if let Some(kind) = span::KINDS.iter().find(|it| span_view(it.name) == name) {
        // alloc: the listing, once a command
        let mut out = Vec::new();
        for it in span::VIEW_COLUMNS.split(", ") {
            if let Some(at) = schema::SPANS.column(it) {
                out.push(column(name, &schema::SPANS.columns[at]));
            }
        }
        for (field, filled) in kind.fields {
            if let Some(at) = schema::SPANS.column(filled) {
                let of = &schema::SPANS.columns[at];
                out.push(Row {
                    kind: "column",
                    table: Some(name.to_owned()), // alloc: a cell
                    name: (*field).to_owned(),    // alloc: a cell
                    ty: Some(type_name(of)),
                    // alloc: a cell
                    says: format!("`spans.{filled}`: {}", of.doc),
                });
            }
        }
        return Ok(out);
    }
    if let Some(view) = views::VIEWS.iter().find(|it| it.name == name) {
        // alloc: a cell
        let args: Vec<&str> = view.args.iter().map(|it| it.name).collect();
        // alloc: the listing, once a command
        return Ok(vec![Row {
            kind: "view",
            table: None,
            name: view.name.to_owned(), // alloc: a cell
            ty: Some(args.join(" ")),
            says: view.title.to_owned(), // alloc: a cell
        }]);
    }
    let names = listing();
    Err(NoSuchTable {
        name: name.to_owned(), // alloc: an error
        nearest: nearest(name, names.iter().map(|it| it.name.as_str()), 5)
            .into_iter()
            .map(str::to_owned)
            .collect(),
    })
}

/// `rows` for a person: the answer first (how many of what), then a line each, a name, its
/// type and what it holds.
pub fn text(rows: &[Row], of: Option<&str>) -> String {
    // alloc: the listing's text, once a command
    let mut out = String::new();
    // Writing to a `String` does not fail.
    match of {
        Some(table) => _ = writeln!(out, "{table}: {} columns", rows.len()),
        None => {
            let count = |kind: &str| rows.iter().filter(|it| it.kind == kind).count();
            _ = writeln!(
                out,
                "{} tables, {} span views, {} named views (`blaze debug schema NAME` for one's columns)",
                count("table"),
                count("span view"),
                count("view")
            );
        }
    }
    let wide = rows.iter().map(|it| it.name.len()).max().unwrap_or(0);
    let typed = rows.iter().filter_map(|it| it.ty.as_ref().map(String::len)).max().unwrap_or(0);
    for it in rows {
        let ty = it.ty.as_deref().unwrap_or("");
        match of {
            Some(_) => _ = writeln!(out, "  {:<wide$}  {ty:<typed$}  {}", it.name, it.says),
            None => _ = writeln!(out, "  {:<10} {:<wide$}  {}", it.kind, it.name, it.says),
        }
    }
    out
}

/// `rows` as JSON lines, an object a row.
pub fn json(rows: &[Row]) -> Result<String, serde_json::Error> {
    // alloc: the listing's text, once a command
    let mut out = String::new();
    for it in rows {
        out.push_str(&serde_json::to_string(it)?);
        out.push('\n');
    }
    Ok(out)
}

/// A statement of `blaze debug sql` that blaze answers itself, from the schema, before DuckDB
/// is started: `SHOW TABLES` and `DESCRIBE` alone (every table and view), `DESCRIBE <name>` and
/// `SHOW <name>` (its columns), any case, with or without a `;`. None for any other statement,
/// which is DuckDB's.
pub fn statement(sql: &str) -> Option<Result<(Vec<Row>, Option<String>), NoSuchTable>> {
    let sql = sql.trim().trim_end_matches(';').trim();
    let mut words = sql.split_whitespace();
    let verb = words.next()?.to_ascii_lowercase();
    let rest: Vec<&str> = words.collect();
    match (verb.as_str(), rest.as_slice()) {
        ("show", [it]) if it.eq_ignore_ascii_case("tables") => Some(Ok((listing(), None))),
        ("describe" | "desc", []) => Some(Ok((listing(), None))),
        ("describe" | "desc" | "show", [name]) => {
            let name = name.trim_matches('"');
            // alloc: the table's name, once a command
            Some(columns(name).map(|rows| (rows, Some(name.to_owned()))))
        }
        _ => None,
    }
}

/// How far `a` is from `b`: the insertions, deletions and substitutions that turn one into
/// the other, case aside.
fn distance(a: &str, b: &str) -> usize {
    let (a, b) = (a.to_ascii_lowercase(), b.to_ascii_lowercase());
    let (a, b) = (a.as_bytes(), b.as_bytes());
    // alloc: two rows of the table, once a pair of names
    let mut before: Vec<usize> = (0..=b.len()).collect();
    let mut row = vec![0; b.len() + 1];
    for (i, x) in a.iter().enumerate() {
        row[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            row[j + 1] = (before[j] + usize::from(x != y)).min(before[j + 1] + 1).min(row[j] + 1);
        }
        std::mem::swap(&mut before, &mut row);
    }
    before[b.len()]
}

/// The `most` names of `names` nearest `wanted`, nearest first: one that holds `wanted` or is
/// held by it before one that only looks like it, then by [`distance`], then by name, so the
/// same names give the same order.
pub fn nearest<'a>(wanted: &str, names: impl Iterator<Item = &'a str>, most: usize) -> Vec<&'a str> {
    let lower = wanted.to_ascii_lowercase();
    // alloc: once an error
    let mut ranked: Vec<(bool, usize, &str)> = names
        .map(|it| {
            let other = it.to_ascii_lowercase();
            let holds = !lower.is_empty() && (other.contains(&lower) || lower.contains(&other));
            (!holds, distance(wanted, it), it)
        })
        .collect();
    ranked.sort_unstable();
    ranked.dedup();
    ranked.into_iter().take(most).map(|(_, _, it)| it).collect()
}

/// The column DuckDB's binder error `message` says it could not find: the name in
/// `Referenced column "tid" not found` or `does not have a column named "tid"`.
fn missing_column(message: &str) -> Option<&str> {
    for lead in ["Referenced column \"", "column named \"", "Referenced column '", "column named '"] {
        if let Some(at) = message.find(lead) {
            let rest = &message[at + lead.len()..];
            let end = rest.find(['"', '\''])?;
            return Some(&rest[..end]);
        }
    }
    None
}

/// The words of `sql` outside its string literals: what a table's name is looked for among.
fn words(sql: &str) -> Vec<&str> {
    // alloc: once an error
    let mut out = Vec::new();
    let mut quoted = false;
    for (i, part) in sql.split('\'').enumerate() {
        if i % 2 == 1 {
            quoted = !quoted;
            continue;
        }
        out.extend(
            part.split(|it: char| !(it.is_ascii_alphanumeric() || it == '_'))
                .filter(|it| !it.is_empty()),
        );
    }
    out
}

/// What to say after DuckDB refused `sql` with `message` for a column it does not know: for
/// each table of the schema the query names, its columns, the nearest to the missing name
/// first. None when the message is about something else, or the query names no table of the
/// schema (a view's columns are its SQL's: `blaze debug schema` names the views).
pub fn hint(message: &str, sql: &str) -> Option<String> {
    let missing = missing_column(message)?;
    let words = words(sql);
    // alloc: the hint, once an error
    let mut out = String::new();
    for table in tables() {
        if !words.iter().any(|it| it.eq_ignore_ascii_case(table.name)) {
            continue;
        }
        let names = nearest(missing, table.columns.iter().map(|it| it.name), usize::MAX);
        _ = writeln!(
            out,
            "`{}` has no column `{missing}`; its columns, the nearest first: {}",
            table.name,
            names.join(", ")
        );
    }
    if out.is_empty() {
        return None;
    }
    out.push_str("`blaze debug schema TABLE` says what each column holds");
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_lists_its_columns_with_types_and_meaning() {
        let rows = columns("spans").unwrap();
        assert_eq!(rows.len(), schema::SPANS.columns.len());
        let thread = rows.iter().find(|it| it.name == "thread").unwrap();
        assert_eq!(thread.ty.as_deref(), Some("u32"));
        let build = rows.iter().find(|it| it.name == "build_id").unwrap();
        assert_eq!(build.ty.as_deref(), Some("u64?"), "a nullable column says so");
        let said = text(&rows, Some("spans"));
        assert!(said.starts_with(&format!("spans: {} columns\n", rows.len())), "{said}");
        assert!(said.contains("the operating system's thread id"), "{said}");
        let lines = json(&rows).unwrap();
        assert!(lines.lines().next().unwrap().starts_with(r#"{"kind":"column","table":"spans","name":"build_id","ty":"u64?""#), "{lines}");
    }

    #[test]
    fn the_listing_is_every_table_and_view_and_an_unknown_name_says_the_nearest() {
        let all = listing();
        for table in tables() {
            assert!(all.iter().any(|it| it.kind == "table" && it.name == table.name));
        }
        assert!(all.iter().any(|it| it.kind == "view" && it.name == "problems"));
        assert!(all.iter().any(|it| it.kind == "span view" && it.name == "span_macro_expand"));
        assert!(text(&all, None).starts_with(&format!("{} tables, ", tables().count())));
        let error = columns("span").unwrap_err();
        assert_eq!(error.nearest[0], "spans", "{error}");
        assert!(columns("span_macro_expand").unwrap().iter().any(|it| it.name == "span_id"));
        assert_eq!(columns("problems").unwrap()[0].kind, "view");
    }

    #[test]
    fn describe_and_show_tables_are_answered_from_the_schema() {
        let (rows, of) = statement("describe spans;").unwrap().unwrap();
        assert_eq!((rows.len(), of.as_deref()), (schema::SPANS.columns.len(), Some("spans")));
        assert_eq!(statement("SHOW TABLES").unwrap().unwrap().0, listing());
        assert_eq!(statement(" DESCRIBE ").unwrap().unwrap().0, listing());
        assert!(statement("DESCRIBE nope").unwrap().is_err());
        assert!(statement("SELECT 1").is_none());
        assert!(statement("DESCRIBE SELECT 1").is_none(), "DuckDB's own DESCRIBE of a query");
        assert!(statement("").is_none());
    }

    /// The three guesses of 2026-11-10: each is answered with the table's real columns, the
    /// ones the reader meant first.
    #[test]
    fn a_column_duckdb_does_not_know_is_answered_with_the_tables_columns_nearest_first() {
        let message = "Binder Error: Referenced column \"tid\" not found in FROM clause!\nCandidate bindings: \"thread\"";
        let said = hint(message, "SELECT tid, name FROM spans WHERE build_id = 3").unwrap();
        assert!(said.starts_with("`spans` has no column `tid`; its columns, the nearest first: "), "{said}");
        let message = "Binder Error: Referenced column \"thread_nam\" not found in FROM clause!";
        let said = hint(message, "SELECT thread_nam FROM spans").unwrap();
        assert!(said.contains("nearest first: thread_name, "), "{said}");
        let both = hint(message, "SELECT thread_nam FROM spans JOIN samples USING (span_id)").unwrap();
        assert!(both.contains("`spans` has no column") && both.contains("`samples` has no column"), "{both}");
        // A table named only inside a string is not one the query reads.
        assert_eq!(hint(message, "SELECT thread_nam FROM other WHERE x = 'spans'"), None);
        assert_eq!(hint("Parser Error: syntax error at or near \"FORM\"", "SELECT 1 FORM spans"), None);
        assert_eq!(nearest("kind", ["query_kind", "name", "wait_kind", "k"].into_iter(), 3), ["query_kind", "wait_kind", "k"]);
    }

    /// The invariant: docs/profiling.md lists no column the schema lacks. Its tables are
    /// written by hand; each row of a table under a `### \`<table>\`` heading names columns of
    /// that table (several on a row are `a, b`), and every table of the schema is named there.
    #[test]
    fn profiling_md_lists_only_columns_the_schema_has() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/profiling.md");
        let doc = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(stale_columns(&doc), Vec::<String>::new(), "docs/profiling.md lists columns `blaze debug schema` does not");
        for table in tables() {
            assert!(doc.contains(&format!("`{}`", table.name)), "docs/profiling.md never names the table `{}`", table.name);
        }
    }

    /// The columns `doc`'s tables list that their table lacks, as `table.column`.
    fn stale_columns(doc: &str) -> Vec<String> {
        let mut out = Vec::new();
        let (mut of, mut in_columns): (Option<&Table>, bool) = (None, false);
        for line in doc.lines() {
            if let Some(heading) = line.strip_prefix("### `") {
                of = heading.split('`').next().and_then(table);
                in_columns = false;
            } else if line.starts_with("## ") {
                of = None;
            } else if line.starts_with("| column | type |") {
                in_columns = true;
            } else if !line.starts_with('|') {
                in_columns = false;
            } else if let (Some(table), true) = (of, in_columns) {
                let cell = line.trim_start_matches('|').split('|').next().unwrap_or("").trim();
                for name in cell.split(", ").filter(|it| !it.starts_with("---")) {
                    if table.column(name).is_none() {
                        out.push(format!("{}.{name}", table.name));
                    }
                }
            }
        }
        out
    }

    #[test]
    fn the_oracle_catches_a_column_the_schema_lacks() {
        let doc = "### `spans`: x\n\n| column | type | |\n|---|---|---|\n| build_id | u64? | x |\n| tid, thread | u32 | x |\n\n| WARN | not a column table |\n";
        assert_eq!(stale_columns(doc), ["spans.tid"]);
    }
}
