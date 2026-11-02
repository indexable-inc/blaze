//! blaze's looks at itself (docs/profiling.md): `blaze explore`, the explorer's link to the
//! daemon (docs/v2/plan/lanes/observe/explore.md); `blaze debug profile`, `blaze debug live`
//! and `blaze debug sql`, the daemon's flight recorder and its profile history; and `blaze
//! explain`, a named view of that history.
//!
//! `blaze debug sql` and `blaze explain` run the DuckDB [`observe::DUCKDB`] names, here in
//! the CLI over the history's files; `--live` asks the daemon, whose flight recorder it reads
//! too. Unset, they fail naming `nix run .#duckdb`: nothing looks for a DuckDB on `PATH`.

use std::{ffi::OsString, fmt::Write as _, io::Write as _, path::PathBuf, time::Duration};

use arrow_array::{Array, RecordBatch, cast::AsArray, types};
use blaze_api::{Request, ResponseRef};
use blaze_profile::{history, live, sql, views};
use serde_json::{Map, Value};

use super::{
    FAILED, USAGE,
    daemon::{self, Api, Start},
    say,
};
use crate::serve::observe;

const EXPLORE_USAGE: &str = "blaze explore [--dev] [--print]: the explorer's link, opened in the browser (`--print`: printed only; \
                             `--dev`: the page's development server, http://localhost:5173, may connect too)";
const DEBUG_USAGE: &str = "blaze debug profile [--seconds N] | blaze debug live | blaze debug sql [--live] [--last SECONDS] [--json] QUERY \
                           | blaze debug hot [--top N] [--last-build]";
const EXPLAIN_USAGE: &str = "blaze explain VIEW [NAME=VALUE ...] [--json]";

/// `blaze explore`: the daemon's explorer started if it is not, its link printed (it carries
/// the session's token: on this terminal only) and opened.
pub fn explore(words: Vec<OsString>, color: bool) -> i32 {
    let (mut dev, mut print) = (false, false);
    for word in words {
        match word.to_str() {
            Some("--dev") => dev = true,
            Some("--print") => print = true,
            _ => return refused(&format!("{EXPLORE_USAGE}, not `{}`", word.to_string_lossy()), USAGE, color),
        }
    }
    let mut link = None;
    let asked = ask(Request::Explore { dev }, Start::IfMissing, |response| {
        if let ResponseRef::Explored { link: it } = response {
            link = Some(it.to_owned());
        }
        Ok(())
    });
    if let Err(what) = asked {
        return refused(&what, FAILED, color);
    }
    let Some(link) = link else { return refused("the daemon answered `blaze explore` with no link", FAILED, color) };
    println!("{link}");
    if !print && let Err(why) = open(&link) {
        eprint!("{}", say::warning(&format!("the link was not opened ({why}): open it in a browser"), color));
    }
    0
}

/// The browser asked to open `link`: `open` on macOS, `xdg-open` elsewhere.
fn open(link: &str) -> Result<(), String> {
    let program = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let status = std::process::Command::new(program).arg(link).status().map_err(|error| format!("running `{program}`: {error}"))?;
    match status.success() {
        true => Ok(()),
        false => Err(format!("`{program}` exited {status}")),
    }
}

/// `blaze debug profile`, `blaze debug live` or `blaze debug sql`.
pub fn debug(words: Vec<OsString>, color: bool) -> i32 {
    let mut words = words.into_iter();
    let verb = words.next();
    let result = match verb.as_ref().and_then(|it| it.to_str()) {
        Some("profile") => profile(words.collect()),
        Some("live") => match words.next() {
            None => watch(),
            Some(word) => Err(Refusal::Usage(format!("blaze debug live takes nothing, not `{}`", word.to_string_lossy()))),
        },
        Some("sql") => query(words.collect()),
        Some("hot") => hot(words.collect()),
        _ => Err(Refusal::Usage(DEBUG_USAGE.to_owned())),
    };
    said(result, color)
}

/// `blaze debug hot [--top N] [--last-build]`: the N keys (20) that took most time and each
/// kind of wait's quantiles, from the daemon's live sketches, since it began or over its last
/// build.
fn hot(words: Vec<OsString>) -> Result<(), Refusal> {
    let (mut top, mut since) = (20u32, blaze_api::HotSince::Start);
    let mut words = texts(words)?.into_iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--last-build" => since = blaze_api::HotSince::LastBuild,
            "--top" => {
                top = words
                    .next()
                    .and_then(|it| it.parse().ok())
                    .ok_or_else(|| Refusal::Usage("`--top` takes a number of keys".to_owned()))?;
            }
            _ => return Err(Refusal::Usage(format!("blaze debug hot [--top N] [--last-build], not `{word}`"))),
        }
    }
    let mut text = String::new();
    ask(Request::Hot { top, since }, Start::Never, |response| {
        if let ResponseRef::Hot(hot) = response {
            blaze_ui::api::hot_lines(&mut text, &hot);
        }
        Ok(())
    })?;
    if text.is_empty() {
        return Err(Refusal::Failed("the daemon answered `blaze debug hot` with no answer".to_owned()));
    }
    print!("{text}");
    Ok(())
}

/// `blaze explain VIEW [NAME=VALUE ...] [--json]`: the view's SQL with its arguments bound,
/// run over the history.
pub fn explain(words: Vec<OsString>, color: bool) -> i32 {
    said(explained(words), color)
}

enum Refusal {
    Usage(String),
    Failed(String),
}

fn said(result: Result<(), Refusal>, color: bool) -> i32 {
    match result {
        Ok(()) => 0,
        Err(Refusal::Usage(what)) => refused(&what, USAGE, color),
        Err(Refusal::Failed(what)) => refused(&what, FAILED, color),
    }
}

impl From<String> for Refusal {
    fn from(what: String) -> Refusal {
        Refusal::Failed(what)
    }
}

fn refused(what: &str, code: i32, color: bool) -> i32 {
    eprint!("{}", say::error(what, color));
    code
}

/// The words as text: a request carries no other.
fn texts(words: Vec<OsString>) -> Result<Vec<String>, Refusal> {
    words
        .into_iter()
        .map(|it| it.into_string().map_err(|it| Refusal::Usage(format!("{} is not UTF-8", it.to_string_lossy()))))
        .collect()
}

/// `request` asked of the daemon, each response but its errors handed to `on`; the daemon's
/// errors, and a Done not ok, are the error. With [`Start::Never`], no daemon running is one.
pub(super) fn ask(request: Request, start: Start, mut on: impl FnMut(ResponseRef<'_>) -> Result<(), String>) -> Result<(), String> {
    let mut client = match daemon::connect(start) {
        Ok(Some(client)) => client,
        Ok(None) => return Err("no daemon is running: a build, or `blaze serve`, starts one".to_owned()),
        Err(error) => return Err(format!("{error:#}")),
    };
    let mut errors = Vec::new();
    let mut failed = None;
    let ok = Api::call(&mut client, request, |response, _| match response {
            // alloc: error path, the daemon's errors, said once it is done
            ResponseRef::Error(error) => errors.push(error.what.to_owned()),
            response => {
                if failed.is_none()
                    && let Err(why) = on(response)
                {
                    failed = Some(why);
                }
            }
    })
    .map_err(|error| error.to_string())?;
    if let Some(why) = failed {
        return Err(why);
    }
    match (ok, errors.is_empty()) {
        (true, true) => Ok(()),
        (_, false) => Err(errors.join("\n")),
        (false, true) => Err("the daemon answered not ok and said no error".to_owned()),
    }
}

/// `blaze debug profile [--seconds N]`: the last N seconds (60) of the daemon's flight
/// recorder as a Chrome trace in its state directory, whose path is printed.
fn profile(words: Vec<OsString>) -> Result<(), Refusal> {
    let mut seconds = 60u32;
    let mut words = texts(words)?.into_iter();
    while let Some(word) = words.next() {
        let value = match word.strip_prefix("--seconds") {
            Some("") => words.next(),
            Some(rest) => rest.strip_prefix('=').map(str::to_owned),
            None => None,
        };
        seconds = value
            .and_then(|it| it.parse().ok())
            .ok_or_else(|| Refusal::Usage(format!("blaze debug profile takes `--seconds N`, not `{word}`")))?;
    }
    let mut trace = None;
    ask(Request::Profile { seconds }, Start::Never, |response| {
        if let ResponseRef::Profiled { trace: it } = response {
            trace = Some(it.to_vec());
        }
        Ok(())
    })?;
    let trace = trace.ok_or_else(|| "the daemon answered `blaze debug profile` with no trace".to_owned())?;
    let state = crate::serve::state().map_err(|error| format!("{error:#}"))?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |it| it.as_secs());
    let path = state.join(format!("profile-{now}.json"));
    std::fs::write(&path, trace).map_err(|error| format!("writing {}: {error}", path.display()))?;
    println!("{}", path.display());
    Ok(())
}

/// `blaze debug live`: the daemon's threads as they run, redrawn with each frame, until
/// Ctrl-C.
fn watch() -> Result<(), Refusal> {
    let width = super::place::stderr_columns().map_or(100, usize::from);
    let mut top = live::Top::default();
    let mut text = String::new();
    ask(Request::Live, Start::Never, |response| {
        let ResponseRef::Live(frame) = response else { return Ok(()) };
        let decoded = |what: &str, bytes: &[u8]| live::decode(bytes).map_err(|error| format!("the live feed's {what}: {error}"));
        let (spans, counters, open) =
            (decoded("spans", frame.spans)?, decoded("counters", frame.counters)?, decoded("open spans", frame.open)?);
        top.take(frame.at_ns, frame.skipped, frame.lost, &spans, &counters, &open)
            .map_err(|error| format!("the live feed: {error}"))?;
        top.render(width, &mut text);
        let mut out = std::io::stdout().lock();
        write!(out, "\x1b[2J\x1b[H{text}").and_then(|()| out.flush()).map_err(|error| format!("the terminal: {error}"))
    })?;
    Ok(())
}

/// How `blaze debug sql` and `blaze explain` print their answer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Table,
    /// A JSON object a row, a line each, its columns in order.
    Json,
}

/// `blaze debug sql [--live] [--last SECONDS] [--json] QUERY`.
fn query(words: Vec<OsString>) -> Result<(), Refusal> {
    let (mut live, mut last_s, mut shape, mut sql) = (false, 30u32, Shape::Table, None);
    let mut words = texts(words)?.into_iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--live" => live = true,
            "--json" => shape = Shape::Json,
            "--last" => {
                last_s = words
                    .next()
                    .and_then(|it| it.parse().ok())
                    .ok_or_else(|| Refusal::Usage("`--last` takes a number of seconds".to_owned()))?;
            }
            flag if flag.starts_with("--") => return Err(Refusal::Usage(format!("{DEBUG_USAGE}, not `{flag}`"))),
            _ if sql.is_some() => return Err(Refusal::Usage(format!("{DEBUG_USAGE}: one query, quoted"))),
            _ => sql = Some(word),
        }
    }
    let sql = sql.ok_or_else(|| Refusal::Usage(DEBUG_USAGE.to_owned()))?;
    let batches = match live {
        false => run(&sql)?,
        true => {
            let mut batches = None;
            ask(Request::Sql { sql, last_s }, Start::Never, |response| {
                if let ResponseRef::Queried { ipc, .. } = response {
                    batches = Some(match ipc.is_empty() {
                        true => Vec::new(),
                        false => live::decode(ipc).map_err(|error| format!("the daemon's answer: {error}"))?,
                    });
                }
                Ok(())
            })?;
            batches.ok_or_else(|| "the daemon answered `blaze debug sql --live` with no answer".to_owned())?
        }
    };
    print(&batches, shape)
}

fn explained(words: Vec<OsString>) -> Result<(), Refusal> {
    let mut words = texts(words)?.into_iter();
    let name = words.next().ok_or_else(|| {
        let names: Vec<_> = views::VIEWS.iter().map(|it| it.name).collect();
        Refusal::Usage(format!("{EXPLAIN_USAGE}; the views are {}", names.join(", ")))
    })?;
    let view = views::find(&name).map_err(|error| error.to_string())?;
    let mut shape = Shape::Table;
    let mut args = Map::new();
    for word in words {
        if word == "--json" {
            shape = Shape::Json;
            continue;
        }
        let Some((key, value)) = word.split_once('=') else {
            return Err(Refusal::Usage(format!("{EXPLAIN_USAGE}, not `{word}`")));
        };
        args.insert(key.to_owned(), argument(view, key, value)?);
    }
    let sql = views::render(view, &args).map_err(|error| error.to_string())?;
    print(&run(&sql)?, shape)
}

/// `value` as the JSON of argument `key`'s type; a key the view does not take is passed as
/// text, for the view to refuse by name.
fn argument(view: &views::View, key: &str, value: &str) -> Result<Value, Refusal> {
    let Some(arg) = view.args.iter().find(|it| it.name == key) else { return Ok(Value::from(value)) };
    let wrong = || Refusal::Usage(format!("{}'s `{key}` is a {}, and `{value}` is none", view.name, arg.ty.name()));
    Ok(match arg.ty {
        views::ArgType::U64 => Value::from(value.parse::<u64>().map_err(|_| wrong())?),
        views::ArgType::I64 => Value::from(value.parse::<i64>().map_err(|_| wrong())?),
        views::ArgType::F64 => {
            serde_json::Number::from_f64(value.parse().map_err(|_| wrong())?).map(Value::Number).ok_or_else(wrong)?
        }
        views::ArgType::Text => Value::from(value),
    })
}

/// `sql` over the history in the daemon's state directory, by the DuckDB [`observe::DUCKDB`]
/// names.
fn run(sql: &str) -> Result<Vec<RecordBatch>, Refusal> {
    let duckdb = observe::duckdb(std::env::var_os(observe::DUCKDB))?.ok_or_else(|| {
        format!("{} is unset: it names the DuckDB `nix run .#duckdb` pins, which blaze's own package sets", observe::DUCKDB)
    })?;
    let state: PathBuf = crate::serve::state().map_err(|error| format!("{error:#}"))?;
    let dir = history::dir_in(&state);
    let query = sql::Query { duckdb: &duckdb, history: &dir, sql };
    Ok(sql::run(&query).map_err(|error| error.to_string())?)
}

fn print(batches: &[RecordBatch], shape: Shape) -> Result<(), Refusal> {
    let text = match shape {
        Shape::Table => table(batches)?,
        Shape::Json => json(batches)?,
    };
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes()).and_then(|()| out.flush()).map_err(|error| Refusal::Failed(format!("stdout: {error}")))
}

/// The batches as a table: a header of the columns' names, a rule, a line a row, then the
/// count of rows.
fn table(batches: &[RecordBatch]) -> Result<String, String> {
    let Some(first) = batches.first() else { return Ok("0 rows\n".to_owned()) };
    let names: Vec<String> = first.schema().fields().iter().map(|it| it.name().to_owned()).collect();
    let mut rows = Vec::new();
    for batch in batches {
        for row in 0..batch.num_rows() {
            let cells = batch.columns().iter().map(|column| cell(column.as_ref(), row).map(|it| text(&it)));
            rows.push(cells.collect::<Result<Vec<_>, _>>()?);
        }
    }
    let mut widths: Vec<usize> = names.iter().map(|it| it.chars().count()).collect();
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    let line = |out: &mut String, cells: &[String]| {
        for (at, (cell, &width)) in cells.iter().zip(&widths).enumerate() {
            let sep = if at == 0 { "" } else { "  " };
            _ = write!(out, "{sep}{cell:<width$}");
        }
        let kept = out.trim_end_matches(' ').len();
        out.truncate(kept);
        out.push('\n');
    };
    line(&mut out, &names);
    let rule: Vec<String> = widths.iter().map(|it| "-".repeat(*it)).collect();
    line(&mut out, &rule);
    for row in &rows {
        line(&mut out, row);
    }
    _ = writeln!(out, "{} rows", rows.len());
    Ok(out)
}

/// The batches as JSON lines: an object a row, its columns in the query's order.
fn json(batches: &[RecordBatch]) -> Result<String, String> {
    let mut out = String::new();
    for batch in batches {
        let schema = batch.schema();
        for row in 0..batch.num_rows() {
            out.push('{');
            for (at, (field, column)) in schema.fields().iter().zip(batch.columns()).enumerate() {
                if at > 0 {
                    out.push(',');
                }
                let key = serde_json::to_string(field.name()).map_err(|error| error.to_string())?;
                let value = cell(column.as_ref(), row)?;
                _ = write!(out, "{key}:{value}");
            }
            out.push_str("}\n");
        }
    }
    Ok(out)
}

fn text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(it) => it.clone(), // clone: a cell, printed once
        it => it.to_string(),
    }
}

/// Row `row` of `array` as JSON. A column of a type this does not print is an error naming it.
// dyn: a column of a query's answer, read once a cell
fn cell(array: &dyn Array, row: usize) -> Result<Value, String> {
    if array.is_null(row) {
        return Ok(Value::Null);
    }
    macro_rules! primitive {
        ($($ty:ty),*) => {
            $(if let Some(it) = array.as_primitive_opt::<$ty>() {
                return Ok(Value::from(it.value(row)));
            })*
        };
    }
    primitive!(
        types::Int8Type,
        types::Int16Type,
        types::Int32Type,
        types::Int64Type,
        types::UInt8Type,
        types::UInt16Type,
        types::UInt32Type,
        types::UInt64Type,
        types::Float32Type,
        types::Float64Type,
        types::TimestampSecondType,
        types::TimestampMillisecondType,
        types::TimestampMicrosecondType,
        types::TimestampNanosecondType,
        types::Date32Type,
        types::Date64Type,
        types::DurationSecondType,
        types::DurationMillisecondType,
        types::DurationMicrosecondType,
        types::DurationNanosecondType
    );
    if let Some(it) = array.as_string_opt::<i32>() {
        return Ok(Value::from(it.value(row)));
    }
    if let Some(it) = array.as_string_opt::<i64>() {
        return Ok(Value::from(it.value(row)));
    }
    if let Some(it) = array.as_string_view_opt() {
        return Ok(Value::from(it.value(row)));
    }
    if let Some(it) = array.as_boolean_opt() {
        return Ok(Value::from(it.value(row)));
    }
    if let Some(dictionary) = array.as_any_dictionary_opt() {
        let key = dictionary.normalized_keys()[row];
        return cell(dictionary.values().as_ref(), key);
    }
    Err(format!("a column of type {} can't be printed: cast it in the query (`::VARCHAR`)", array.data_type()))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_array::{ArrayRef, DictionaryArray, Float64Array, Int64Array, StringArray};

    use super::*;

    fn batch() -> RecordBatch {
        let names: DictionaryArray<types::Int32Type> = vec![Some("parse"), None, Some("parse")].into_iter().collect();
        let columns: Vec<(&str, ArrayRef)> = vec![
            ("name", Arc::new(names)),
            ("count", Arc::new(Int64Array::from(vec![3, 10, -1]))),
            ("share", Arc::new(Float64Array::from(vec![0.5, 1.0, 0.25]))),
            ("note", Arc::new(StringArray::from(vec![Some("a \"quoted\" one"), Some(""), None]))),
        ];
        RecordBatch::try_from_iter(columns).unwrap()
    }

    #[test]
    fn a_table_lines_up_its_columns_and_counts_its_rows() {
        let table = table(&[batch()]).unwrap();
        assert_eq!(
            table,
            "name   count  share  note\n\
             -----  -----  -----  --------------\n\
             parse  3      0.5    a \"quoted\" one\n\
             \x20      10     1.0\n\
             parse  -1     0.25\n\
             3 rows\n"
        );
        assert_eq!(super::table(&[]).unwrap(), "0 rows\n");
    }

    #[test]
    fn json_is_an_object_a_row_in_the_columns_order() {
        let json = json(&[batch()]).unwrap();
        let lines: Vec<&str> = json.lines().collect();
        assert_eq!(lines[0], r#"{"name":"parse","count":3,"share":0.5,"note":"a \"quoted\" one"}"#);
        assert_eq!(lines[1], r#"{"name":null,"count":10,"share":1.0,"note":""}"#);
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn an_argument_is_read_as_its_views_type() {
        let view = views::find("slowest_queries").unwrap();
        assert_eq!(argument(view, "limit", "5").ok(), Some(Value::from(5u64)));
        assert!(matches!(argument(view, "limit", "five"), Err(Refusal::Usage(it)) if it.contains("`limit` is a u64")));
        assert_eq!(argument(view, "nope", "x").ok(), Some(Value::from("x")), "left for the view to refuse");
    }
}
