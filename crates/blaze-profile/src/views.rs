//! The named views over the history, as data: the one list both the explorer's `views` and
//! `view` ops and `blaze explain <view> <args>` read. Each view's SQL is a section of
//! `sql/examples.sql`, cut out at compile time; [`PRELUDE`] (`sql/views.sql`) defines the tables
//! and views that SQL reads, over the directories [`prelude`] is given.

use std::{fmt, fmt::Write as _, path::Path};

use serde_json::{Map, Value};

use crate::schema;

/// The DuckDB views every query runs after: one a table, and `queries`, `passes`,
/// `fiber_stacks`, `job_waits` and `job_spans`. Each
/// `@<table>` in it is a list of globs, filled in by [`prelude`].
pub const PRELUDE: &str = include_str!("../sql/views.sql");

const EXAMPLES: &str = include_str!("../sql/examples.sql");

/// A named query: what `blaze explain <name>` runs and the explorer lists.
#[derive(Debug)]
pub struct View {
    pub name: &'static str,
    /// Its `-- says:` line: what it answers, in a sentence.
    pub title: &'static str,
    /// Its SQL, with `$<arg>` where each argument goes.
    pub sql: &'static str,
    pub args: &'static [Arg],
}

#[derive(Debug)]
pub struct Arg {
    pub name: &'static str,
    pub ty: ArgType,
    /// SQL put in when the argument is not given; `None`: it must be given.
    pub default: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgType {
    U64,
    I64,
    F64,
    Text,
}

impl ArgType {
    pub const fn name(self) -> &'static str {
        match self {
            ArgType::U64 => "u64",
            ArgType::I64 => "i64",
            ArgType::F64 => "f64",
            ArgType::Text => "text",
        }
    }

    /// Its type in JSON Schema, as the explorer's `views` op says it.
    const fn json(self) -> &'static str {
        match self {
            ArgType::U64 | ArgType::I64 => "integer",
            ArgType::F64 => "number",
            ArgType::Text => "string",
        }
    }
}

const BUILD: Arg = Arg {
    name: "build",
    ty: ArgType::U64,
    default: Some("(SELECT max(build_id) FROM spans)"),
};
/// A build for a view over its wall time: by default the last to end, as one still running has
/// no `builds` row to bound it.
const ENDED_BUILD: Arg = Arg {
    name: "build",
    ty: ArgType::U64,
    default: Some("(SELECT max(build_id) FROM builds)"),
};
const LIMIT: Arg = Arg { name: "limit", ty: ArgType::U64, default: Some("20") };
const EARLIER: Arg = Arg { name: "earlier", ty: ArgType::Text, default: None };
const LATER: Arg = Arg { name: "later", ty: ArgType::Text, default: None };

pub const VIEWS: &[View] = &[
    view("slowest_queries", &[BUILD, LIMIT]),
    view("slower_between_commits", &[EARLIER, LATER, LIMIT]),
    view("memo_hit_rate_per_crate", &[BUILD]),
    view("critical_path", &[ENDED_BUILD, LIMIT]),
    view("allocations_per_query_kind", &[BUILD]),
    view("what_reran", &[BUILD, LIMIT]),
    view("busiest_threads", &[BUILD, LIMIT]),
    view("longest_gaps", &[BUILD, LIMIT]),
    view("top_cpu_stacks", &[BUILD, LIMIT]),
    view("time_by_span_name", &[BUILD]),
    view("frame_copies_per_query_kind", &[BUILD, LIMIT]),
    view("jobs_by_stack_high_water", &[BUILD, LIMIT]),
    view("utilization_per_second", &[ENDED_BUILD]),
    view("uncovered_time", &[ENDED_BUILD]),
    view("worker_states", &[ENDED_BUILD]),
    view("uncovered_per_worker", &[ENDED_BUILD]),
    view("raw_blocks", &[ENDED_BUILD, LIMIT]),
    view("reexecution_per_query_kind", &[BUILD, LIMIT]),
    view("footprint_per_build", &[LIMIT]),
    view("derive_panics_per_build", &[LIMIT]),
    view("errors_by_kind", &[ENDED_BUILD]),
];

/// The view named `name`.
pub fn find(name: &str) -> Result<&'static View, ViewError> {
    VIEWS.iter().find(|it| it.name == name).ok_or_else(|| ViewError::NoSuchView {
        // alloc: an error, once a request
        name: name.to_owned(),
    })
}

const NAME: &[u8] = b"-- name: ";
const SAYS: &[u8] = b"-- says: ";

const fn view(name: &'static str, args: &'static [Arg]) -> View {
    let (title, sql) = section(EXAMPLES, name);
    View { name, title, sql, args }
}

const fn starts(hay: &[u8], at: usize, needle: &[u8]) -> bool {
    if at + needle.len() > hay.len() {
        return false;
    }
    let mut i = 0;
    while i < needle.len() {
        if hay[at + i] != needle[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Where the next line starting `needle` begins, from `from` on, or the end of `hay`.
const fn line_starting(hay: &[u8], needle: &[u8], mut from: usize) -> usize {
    while from < hay.len() {
        if (from == 0 || hay[from - 1] == b'\n') && starts(hay, from, needle) {
            return from;
        }
        from += 1;
    }
    hay.len()
}

const fn line_end(hay: &[u8], mut from: usize) -> usize {
    while from < hay.len() && hay[from] != b'\n' {
        from += 1;
    }
    from
}

const fn cut(text: &'static str, from: usize, to: usize) -> &'static str {
    let (_, rest) = text.as_bytes().split_at(from);
    let (it, _) = rest.split_at(to - from);
    match std::str::from_utf8(it) {
        Ok(it) => it,
        Err(_) => panic!("a view's section is cut inside a character"),
    }
}

/// The `-- says:` line and the SQL of the section `-- name: <name>` of `file`: a compile error
/// when there is none, or it has no `-- says:` line first.
const fn section(file: &'static str, name: &'static str) -> (&'static str, &'static str) {
    let bytes = file.as_bytes();
    let mut at = line_starting(bytes, NAME, 0);
    loop {
        if at == bytes.len() {
            panic!("sql/examples.sql has no `-- name:` section for a view of VIEWS");
        }
        let named = at + NAME.len();
        let end = line_end(bytes, named);
        if end - named == name.len() && starts(bytes, named, name.as_bytes()) {
            break;
        }
        at = line_starting(bytes, NAME, end);
    }
    let says = line_end(bytes, at) + 1;
    if !starts(bytes, says, SAYS) {
        panic!("a view's `-- name:` line in sql/examples.sql is not followed by its `-- says:` line");
    }
    let says_end = line_end(bytes, says);
    let sql = says_end + 1;
    let mut sql_end = line_starting(bytes, NAME, sql);
    while sql_end > sql && bytes[sql_end - 1].is_ascii_whitespace() {
        sql_end -= 1;
    }
    (cut(file, says + SAYS.len(), says_end), cut(file, sql, sql_end))
}

#[derive(Debug, PartialEq)]
pub enum ViewError {
    NoSuchView { name: String },
    /// An argument the view does not take.
    UnknownArg { view: &'static str, arg: String },
    /// An argument the view needs, with no default, not given.
    MissingArg { view: &'static str, arg: &'static str },
    /// An argument given as a JSON value its type does not take.
    BadArg { view: &'static str, arg: &'static str, want: ArgType, got: String },
    /// `$<name>` in the view's SQL, or `@<name>` in the prelude, that names no argument or
    /// table: a bug in the SQL files.
    Undeclared { view: &'static str, name: String },
    /// A directory a glob can't be made of.
    BadPath { path: String, why: &'static str },
}

impl ViewError {
    /// The error's kind on the explorer's wire (`explore-wire.md`).
    pub fn kind(&self) -> &'static str {
        match self {
            ViewError::NoSuchView { .. } => "no_such_view",
            ViewError::UnknownArg { .. } | ViewError::MissingArg { .. } | ViewError::BadArg { .. } => "bad_args",
            ViewError::Undeclared { .. } | ViewError::BadPath { .. } => "sql",
        }
    }
}

impl fmt::Display for ViewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ViewError::NoSuchView { name } => {
                write!(f, "no view is named `{name}`; the views are")?;
                for (i, it) in VIEWS.iter().enumerate() {
                    write!(f, "{} {}", if i == 0 { "" } else { "," }, it.name)?;
                }
                Ok(())
            }
            ViewError::UnknownArg { view, arg } => {
                write!(f, "the view {view} takes no argument `{arg}`; it takes")?;
                match find(view).map(|it| it.args) {
                    Ok([]) | Err(_) => write!(f, " none"),
                    Ok(args) => {
                        for (i, it) in args.iter().enumerate() {
                            write!(f, "{} {} ({})", if i == 0 { "" } else { "," }, it.name, it.ty.name())?;
                        }
                        Ok(())
                    }
                }
            }
            ViewError::MissingArg { view, arg } => write!(f, "the view {view} needs the argument `{arg}`"),
            ViewError::BadArg { view, arg, want, got } => {
                write!(f, "the view {view}'s argument `{arg}` is a {}, not {got}", want.name())
            }
            ViewError::Undeclared { view, name } => {
                write!(f, "the SQL of {view} names `{name}`, which is no argument or table of it")
            }
            ViewError::BadPath { path, why } => write!(f, "the directory {path} {why}"),
        }
    }
}

impl std::error::Error for ViewError {}

/// What [`scan`] found: a `$` or `@` and the name after it.
struct Sigil<'a> {
    at: usize,
    end: usize,
    sigil: u8,
    name: &'a str,
}

/// Each `$name` and `@name` of `sql` outside its comments and string literals, in order.
fn scan(sql: &str, mut each: impl FnMut(Sigil<'_>) -> Result<(), ViewError>) -> Result<(), ViewError> {
    let bytes = sql.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'-' if bytes.get(at + 1) == Some(&b'-') => at = line_end(bytes, at),
            b'\'' => {
                at += 1;
                while at < bytes.len() && bytes[at] != b'\'' {
                    at += 1;
                }
                at += 1;
            }
            sigil @ (b'$' | b'@') => {
                let start = at + 1;
                let mut end = start;
                while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                    end += 1;
                }
                each(Sigil { at, end, sigil, name: &sql[start..end] })?;
                at = end.max(at + 1);
            }
            _ => at += 1,
        }
    }
    Ok(())
}

/// The SQL of `view` with each argument bound as an SQL literal: integers as written, floats
/// with an exponent (so DuckDB reads a `DOUBLE`), text single-quoted with each quote doubled.
/// An argument it does not take, one it needs and is not given, and one of the wrong type are
/// errors.
pub fn render(view: &View, args: &Map<String, Value>) -> Result<String, ViewError> {
    if let Some(arg) = args.keys().find(|key| !view.args.iter().any(|it| it.name == key.as_str())) {
        // alloc: an error, once a request
        return Err(ViewError::UnknownArg { view: view.name, arg: arg.to_owned() });
    }
    // alloc: the query, once a request
    let mut out = String::with_capacity(view.sql.len() + 64);
    let mut copied = 0;
    scan(view.sql, |it| {
        if it.sigil != b'$' {
            return Ok(());
        }
        let Some(arg) = view.args.iter().find(|arg| arg.name == it.name) else {
            // alloc: an error, once a request
            return Err(ViewError::Undeclared { view: view.name, name: it.name.to_owned() });
        };
        out.push_str(&view.sql[copied..it.at]);
        literal(view, arg, args.get(arg.name), &mut out)?;
        copied = it.end;
        Ok(())
    })?;
    out.push_str(&view.sql[copied..]);
    Ok(out)
}

fn literal(view: &View, arg: &Arg, value: Option<&Value>, out: &mut String) -> Result<(), ViewError> {
    let bad = |got: &Value| ViewError::BadArg {
        view: view.name,
        arg: arg.name,
        want: arg.ty,
        // alloc: an error, once a request
        got: got.to_string(),
    };
    let Some(value) = value else {
        let default = arg.default.ok_or(ViewError::MissingArg { view: view.name, arg: arg.name })?;
        out.push_str(default);
        return Ok(());
    };
    // Writing to a `String` does not fail.
    match arg.ty {
        ArgType::U64 => _ = write!(out, "{}", value.as_u64().ok_or_else(|| bad(value))?),
        ArgType::I64 => _ = write!(out, "{}", value.as_i64().ok_or_else(|| bad(value))?),
        ArgType::F64 => {
            let it = value.as_f64().filter(|it| it.is_finite()).ok_or_else(|| bad(value))?;
            _ = write!(out, "{it:e}");
        }
        ArgType::Text => {
            let it = value.as_str().filter(|it| !it.contains('\0')).ok_or_else(|| bad(value))?;
            quote(it, out);
        }
    }
    Ok(())
}

/// `text` as an SQL string literal: in single quotes, each quote doubled. DuckDB's plain
/// literals take backslashes as they are.
pub(crate) fn quote(text: &str, out: &mut String) {
    out.push('\'');
    for (i, part) in text.split('\'').enumerate() {
        if i > 0 {
            out.push_str("''");
        }
        out.push_str(part);
    }
    out.push('\'');
}

/// [`PRELUDE`] reading each table from `<dir>/*.<table>.parquet` of every directory of `dirs`,
/// then each view of [`VIEWS`] whose arguments all have defaults as a DuckDB view of that name
/// (the latest build, the default limit), so a query can say `FROM uncovered_time`. Other
/// arguments take `blaze explain <view> <args>`.
pub fn prelude(dirs: &[&Path]) -> Result<String, ViewError> {
    // alloc: the prelude, once a query
    let mut globs: Vec<&str> = Vec::with_capacity(dirs.len());
    for dir in dirs {
        let path = dir.to_str().ok_or_else(|| ViewError::BadPath {
            // alloc: an error, once a request
            path: dir.display().to_string(),
            why: "is not UTF-8, and DuckDB's globs are",
        })?;
        if path.contains(['*', '?', '[', ']', '{', '}']) {
            // alloc: an error, once a request
            return Err(ViewError::BadPath { path: path.to_owned(), why: "has a character DuckDB's globs expand" });
        }
        globs.push(path.trim_end_matches('/'));
    }
    // alloc: the prelude, once a query
    let mut out = String::with_capacity(PRELUDE.len() + 256 * dirs.len());
    let mut copied = 0;
    scan(PRELUDE, |it| {
        if it.sigil != b'@' {
            return Ok(());
        }
        let Some(table) = schema::TABLES.iter().find(|table| table.name == it.name) else {
            // alloc: an error, a bug in views.sql
            return Err(ViewError::Undeclared { view: "views.sql", name: it.name.to_owned() });
        };
        out.push_str(&PRELUDE[copied..it.at]);
        out.push('[');
        for (i, dir) in globs.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            // alloc: the prelude, once a query
            quote(&format!("{dir}/*.{}.parquet", table.name), &mut out);
        }
        out.push(']');
        copied = it.end;
        Ok(())
    })?;
    out.push_str(&PRELUDE[copied..]);
    for view in VIEWS.iter().filter(|it| it.args.iter().all(|arg| arg.default.is_some())) {
        // Writing to a `String` does not fail; a section may end in a comment, so `;` goes on a
        // line of its own.
        _ = write!(out, "\nCREATE OR REPLACE VIEW {} AS\n{}\n;\n", view.name, render(view, &Map::new())?);
    }
    crate::span::views(&mut out);
    Ok(out)
}

/// The JSON Schema of `view`'s arguments, as the explorer's `views` op sends it. A default is
/// SQL, so it is said in `x-default-sql`, not `default`.
pub fn args_schema(view: &View) -> Value {
    let mut properties = Map::new();
    // alloc: the views op's answer, once a request
    let mut required = Vec::new();
    for arg in view.args {
        let mut it = Map::new();
        it.insert("type".into(), Value::from(arg.ty.json()));
        if arg.ty == ArgType::U64 {
            it.insert("minimum".into(), Value::from(0));
        }
        match arg.default {
            Some(default) => _ = it.insert("x-default-sql".into(), Value::from(default)),
            None => required.push(Value::from(arg.name)),
        }
        properties.insert(arg.name.into(), Value::Object(it));
    }
    let mut schema = Map::new();
    schema.insert("type".into(), Value::from("object"));
    schema.insert("properties".into(), Value::Object(properties));
    schema.insert("required".into(), Value::Array(required));
    schema.insert("additionalProperties".into(), Value::Bool(false));
    Value::Object(schema)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::json;

    use super::*;

    fn args(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(it) => it,
            _ => unreachable!(),
        }
    }

    #[test]
    fn every_section_of_examples_sql_is_a_view_with_a_title() {
        let sections = EXAMPLES.lines().filter(|it| it.starts_with("-- name: ")).count();
        assert_eq!(sections, VIEWS.len(), "each `-- name:` section of examples.sql is in VIEWS");
        let names: BTreeSet<&str> = VIEWS.iter().map(|it| it.name).collect();
        assert_eq!(names.len(), VIEWS.len(), "no two views share a name");
        for view in VIEWS {
            assert!(!view.title.is_empty() && !view.title.contains('\n'), "{} says what it is in a line", view.name);
            assert!(view.sql.starts_with("SELECT") || view.sql.starts_with("WITH"), "{}: {}", view.name, view.sql);
            assert!(!view.sql.contains("-- name:"), "{} holds one section", view.name);
            assert!(!view.sql.trim_end().ends_with(';'), "{} is one statement DuckDB can wrap in COPY", view.name);
        }
    }

    #[test]
    fn every_argument_a_view_declares_is_in_its_sql_and_back() {
        for view in VIEWS {
            let mut used = BTreeSet::new();
            scan(view.sql, |it| {
                assert_eq!(it.sigil, b'$', "{} uses only `$` arguments", view.name);
                used.insert(it.name.to_owned());
                Ok(())
            })
            .unwrap();
            let declared: BTreeSet<String> = view.args.iter().map(|it| it.name.to_owned()).collect();
            assert_eq!(used, declared, "{}", view.name);
        }
    }

    #[test]
    fn render_binds_each_type_as_its_literal() {
        let view = find("slower_between_commits").unwrap();
        let sql = render(view, &args(json!({"earlier": "it's", "later": "a\\b", "limit": 5}))).unwrap();
        assert!(sql.contains("starts_with(commit, 'it''s')"), "{sql}");
        assert!(sql.contains("starts_with(commit, 'a\\b')"), "{sql}");
        assert!(sql.ends_with("LIMIT 5"), "{sql}");
        assert!(!sql.contains('$'));

        let view = find("slowest_queries").unwrap();
        let sql = render(view, &args(json!({}))).unwrap();
        assert!(sql.contains("build_id = (SELECT max(build_id) FROM spans)"), "a missing argument takes its default: {sql}");
        assert!(sql.ends_with("LIMIT 20"), "{sql}");
        let sql = render(view, &args(json!({"build": 41}))).unwrap();
        assert!(sql.contains("build_id = 41"), "{sql}");

        const FLOAT: View = View {
            name: "float",
            title: "a float",
            sql: "SELECT $x AS x, '$x' AS text -- $x\n",
            args: &[Arg { name: "x", ty: ArgType::F64, default: None }],
        };
        let sql = render(&FLOAT, &args(json!({"x": 0.5}))).unwrap();
        assert_eq!(sql, "SELECT 5e-1 AS x, '$x' AS text -- $x\n", "only arguments outside strings and comments are bound");
        let sql = render(&FLOAT, &args(json!({"x": -3}))).unwrap();
        assert!(sql.starts_with("SELECT -3e0 AS x"), "{sql}");
    }

    #[test]
    fn render_refuses_unknown_missing_and_mistyped_arguments() {
        let view = find("slower_between_commits").unwrap();
        assert_eq!(
            render(view, &args(json!({"earlier": "a", "later": "b", "bulid": 1}))),
            Err(ViewError::UnknownArg { view: view.name, arg: "bulid".into() })
        );
        assert_eq!(
            render(view, &args(json!({"earlier": "a"}))),
            Err(ViewError::MissingArg { view: view.name, arg: "later" })
        );
        let error = render(view, &args(json!({"earlier": 3, "later": "b"}))).unwrap_err();
        assert!(matches!(error, ViewError::BadArg { arg: "earlier", want: ArgType::Text, .. }), "{error:?}");
        assert_eq!(error.kind(), "bad_args");
        let error = render(view, &args(json!({"earlier": "a\u{0}", "later": "b"}))).unwrap_err();
        assert!(matches!(error, ViewError::BadArg { arg: "earlier", .. }), "{error:?}");

        let view = find("slowest_queries").unwrap();
        for bad in [json!(-1), json!(1.5), json!("1"), json!(null), json!([1])] {
            let error = render(view, &args(json!({ "build": bad }))).unwrap_err();
            assert!(matches!(error, ViewError::BadArg { arg: "build", want: ArgType::U64, .. }), "{bad}: {error:?}");
        }
        assert_eq!(find("nope").unwrap_err().kind(), "no_such_view");
    }

    #[test]
    fn the_prelude_lists_each_tables_files_in_every_directory() {
        let sql = prelude(&[Path::new("/state/profile/"), Path::new("/tmp/it's live")]).unwrap();
        assert!(sql.contains(
            "read_parquet(['/state/profile/*.spans.parquet', '/tmp/it''s live/*.spans.parquet'], union_by_name = true)"
        ));
        assert!(!sql.contains("@spans") && !sql.contains("@heap_by_owner"));
        assert!(sql.contains("`@<table>`"), "a comment's text is left alone");
        let error = prelude(&[Path::new("/tmp/a*b")]).unwrap_err();
        assert!(matches!(error, ViewError::BadPath { .. }), "{error:?}");
    }

    #[test]
    fn the_args_schema_says_what_is_required() {
        let schema = args_schema(find("slower_between_commits").unwrap());
        assert_eq!(schema["required"], json!(["earlier", "later"]));
        assert_eq!(schema["properties"]["limit"]["type"], "integer");
        assert_eq!(schema["properties"]["limit"]["x-default-sql"], "20");
        assert_eq!(schema["additionalProperties"], false);
    }

    /// DuckDB's words the SQL files use: keywords, types and named parameters.
    const KEYWORDS: &[&str] = &[
        "all", "and", "any", "as", "asc", "asof", "between", "by", "case", "cast", "create", "desc",
        "distinct", "double", "else", "end", "exists", "filter", "first", "from", "group",
        "having", "in", "is", "join", "last", "left", "like", "limit", "not", "null", "nulls",
        "on", "or", "order", "over", "partition", "qualify", "recursive", "replace", "select",
        "then", "union", "union_by_name", "using", "view", "when", "where", "with", "true",
        "false", "rows", "unbounded", "preceding", "bigint", "cross", "ubigint", "varchar",
    ];
    /// DuckDB's functions the SQL files call.
    const FUNCTIONS: &[&str] = &[
        "any_value", "arg_max", "array_to_string", "count", "lag", "lead", "max", "median", "min",
        "rank", "read_parquet", "round", "starts_with", "sum", "unnest", "range", "least", "greatest",
        "coalesce", "string_agg", "regexp_matches",
    ];

    /// The identifiers of `sql` outside comments, strings, numbers and `$`/`@` names, each with
    /// whether a `(` follows it.
    fn identifiers(sql: &str) -> Vec<(String, bool)> {
        let bytes = sql.as_bytes();
        let mut out = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let c = bytes[at];
            if c == b'-' && bytes.get(at + 1) == Some(&b'-') {
                at = line_end(bytes, at);
            } else if c == b'\'' {
                at += 1;
                while at < bytes.len() && bytes[at] != b'\'' {
                    at += 1;
                }
                at += 1;
            } else if c == b'"' {
                panic!("quoted identifiers are not used: {sql}");
            } else if c.is_ascii_digit() || c == b'$' || c == b'@' {
                at += 1;
                while at < bytes.len()
                    && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_' || (bytes[at] == b'.' && c.is_ascii_digit()))
                {
                    at += 1;
                }
            } else if c.is_ascii_alphabetic() || c == b'_' {
                let start = at;
                while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_') {
                    at += 1;
                }
                let mut next = at;
                while next < bytes.len() && bytes[next].is_ascii_whitespace() {
                    next += 1;
                }
                out.push((sql[start..at].to_ascii_lowercase(), bytes.get(next) == Some(&b'(')));
            } else {
                at += 1;
            }
        }
        out
    }

    /// The names `sql` gives with `AS`: columns, tables and the common table expressions.
    fn aliases(sql: &str) -> BTreeSet<String> {
        let words = identifiers(sql);
        let mut out = BTreeSet::new();
        for (i, (word, paren)) in words.iter().enumerate() {
            if word != "as" {
                continue;
            }
            // `name AS (` or `name AS SELECT`: a common table expression's or a view's body.
            let body = *paren || words.get(i + 1).is_some_and(|(it, _)| it == "select");
            let named = match body {
                true => i.checked_sub(1).and_then(|it| words.get(it)),
                false => words.get(i + 1),
            };
            if let Some((name, _)) = named {
                out.insert(name.clone());
            }
        }
        out
    }

    fn check(what: &str, sql: &str, known: &BTreeSet<String>) {
        let named = aliases(sql);
        for (word, called) in identifiers(sql) {
            if called {
                assert!(
                    FUNCTIONS.contains(&word.as_str()) || KEYWORDS.contains(&word.as_str()),
                    "{what} calls `{word}`, which is no function the SQL files use (add it to FUNCTIONS if DuckDB has it)"
                );
                continue;
            }
            assert!(
                KEYWORDS.contains(&word.as_str()) || known.contains(&word) || named.contains(&word),
                "{what} names `{word}`, which is no column or table of the schema, keyword, or name it gives"
            );
        }
    }

    #[test]
    fn every_view_names_only_the_schemas_columns() {
        let mut known: BTreeSet<String> = BTreeSet::new();
        for table in schema::TABLES {
            known.insert(table.name.into());
            known.extend(table.columns.iter().map(|it| it.name.to_owned()));
        }
        check("views.sql", PRELUDE, &known);
        // The views of views.sql, and the columns they add, are known to the queries over them.
        known.extend(aliases(PRELUDE));
        for view in VIEWS {
            check(view.name, view.sql, &known);
            for arg in view.args {
                if let Some(default) = arg.default {
                    check(view.name, default, &known);
                }
            }
        }
    }

    #[test]
    fn the_check_finds_a_misspelled_column() {
        let mut known = BTreeSet::new();
        known.extend(["spans".to_owned(), "dur_ns".to_owned()]);
        let caught = std::panic::catch_unwind(|| check("bad", "SELECT dur_sn FROM spans", &known));
        assert!(caught.is_err());
        check("good", "SELECT dur_ns AS ms FROM spans WHERE ms > 1", &known);
    }
}
