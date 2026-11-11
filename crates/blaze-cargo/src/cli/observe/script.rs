//! `blaze explain script UNIT [--json]`: what the last sandboxed run of a build script did, from
//! the profile history: how it ended and how long it took (its `processes` row), the files it
//! read, the directories it listed and the variables it read, what it read that can't be
//! traced with the process that read it (its program and `argv[0]`, the system call, where in
//! the process the call came from and whether it was before `main`), and the programs it
//! started.
//!
//! The rows are the events the daemon says as a sandboxed run ends
//! (crates/blaze-cargo/src/serve/unit/script/said.rs, [`calls::EVENT_RUN`] and
//! [`calls::EVENT_CALL`]), read through the same DuckDB as every other `blaze explain`: so
//! the same data is `blaze debug sql` over `events` and `event_values`, in the explorer, and in
//! a history copied from another machine. A run reused from the store ran nothing and says
//! nothing: the run named is the last one that ran.

use std::fmt::Write as _;

use arrow_array::RecordBatch;
use blaze_usercode::calls;
use serde::Serialize;
use serde_json::{Map, Value};

use super::Refusal;

const USAGE: &str = "blaze explain script UNIT [--json]: UNIT is a build script's package (`libc`), \
                     or its unit as the profile names it (`libc v0.2.155 (build script run)`)";

/// The version of [`Trace`]'s JSON: bumped when a field changes meaning or goes away.
const VERSION: u32 = 1;

/// The last run of one build script, as `blaze explain script --json` prints it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(super) struct Trace {
    pub version: u32,
    /// The unit, as `spans.unit` and `processes.unit` name it.
    pub unit: String,
    /// The build it ran in; `None` for a run of no build.
    pub build: Option<u64>,
    pub program: String,
    /// How the run's process ended and what it cost (its `processes` row); `None` when the
    /// history has no row of it (it was not reaped in that build's rows).
    pub process: Option<Process>,
    pub files: Listed,
    pub dirs: Listed,
    pub env: Listed,
    /// What it read that can't be traced, by kind: `random`, `clock`, `network`, `descriptor`.
    pub untraceable: Vec<String>,
    /// Who made each kind of untraceable read, as the sandbox described the caller.
    pub calls: Vec<Caller>,
    /// The programs it started.
    pub children: Vec<Caller>,
    /// Calls and programs past the sandbox's bound, counted and not described.
    pub not_described: u64,
}

/// How a run's process ended, and what the OS says it cost.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(super) struct Process {
    pub pid: u64,
    /// Its exit code; `None` when a signal ended it.
    pub code: Option<i64>,
    pub signal: Option<i64>,
    pub wall_ms: f64,
    pub user_ms: f64,
    pub system_ms: f64,
    pub peak_rss: Option<u64>,
}

/// How many of a kind a run read, and the first of them by name (the event's bound).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(super) struct Listed {
    pub count: u64,
    pub named: Vec<String>,
}

/// A process as it made a call the sandbox described ([`calls::Made`], as the event's fields
/// carry it).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(super) struct Caller {
    /// `random`, `clock`, `network`, or `exec` for a program started.
    pub what: String,
    pub pid: i64,
    /// Its id as the sandbox numbers it; `None` when `/proc` did not say.
    pub inside_pid: Option<i64>,
    /// The program the process ran; for `exec`, the program it started.
    pub program: String,
    pub argv0: String,
    pub syscall: String,
    pub number: i64,
    /// `loader`, `loading`, `loaded` or `not known` ([`calls::Phase`]).
    pub phase: String,
    /// `yes (a heuristic)` or `not known`: never a guess said as a fact.
    pub before_main: String,
    pub ip: String,
    pub module: String,
    pub module_kind: String,
}

/// `text` as an SQL string literal.
fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// The events of message `message`, a row each with its fields `keys` as columns of text
/// (`ints` as integers), as a subquery.
fn pivot(message: &str, keys: &[&str], ints: &[&str]) -> String {
    let mut columns = String::new();
    for key in keys {
        _ = write!(columns, ", max(v.value_text) FILTER (WHERE v.key = '{key}') AS \"{key}\"");
    }
    for key in ints {
        _ = write!(
            columns,
            ", CAST(max(v.value_int) FILTER (WHERE v.key = '{key}') AS BIGINT) AS \"{key}\""
        );
    }
    format!(
        "(SELECT CAST(e.build_id AS BIGINT) AS build_id, CAST(e.process AS BIGINT) AS process, \
         e.event_id AS event_id, CAST(e.t_ns AS BIGINT) AS t_ns{columns} \
         FROM events AS e JOIN event_values AS v ON v.process = e.process AND v.event_id = e.event_id \
         WHERE e.message = {} GROUP BY e.build_id, e.process, e.event_id, e.t_ns)",
        quoted(message)
    )
}

/// The runs' events as rows.
fn runs() -> String {
    pivot(
        calls::EVENT_RUN,
        &["unit", "program", "untraceable", "files_named", "dirs_named", "env_named"],
        &["files", "dirs", "env", "calls", "execs"],
    )
}

/// The units of the runs in the history that `asked` names (the unit itself, or a package
/// whose unit starts with it), each with its last run's time, newest first.
fn units_sql(asked: Option<&str>) -> String {
    let which = match asked {
        Some(asked) => format!(
            "WHERE unit = {0} OR starts_with(unit, {0} || ' ')",
            quoted(asked)
        ),
        None => String::new(),
    };
    format!(
        "SELECT CAST(unit AS VARCHAR) AS unit, CAST(max(t_ns) AS BIGINT) AS t_ns FROM {} {which} \
         GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT 21",
        runs()
    )
}

/// The last run of `unit`, with its `processes` row beside it.
fn run_sql(unit: &str) -> String {
    format!(
        "SELECT r.build_id, r.process, r.t_ns, CAST(r.unit AS VARCHAR) AS unit, CAST(r.program AS VARCHAR) AS program, \
         CAST(r.untraceable AS VARCHAR) AS untraceable, CAST(r.files_named AS VARCHAR) AS files_named, \
         CAST(r.dirs_named AS VARCHAR) AS dirs_named, CAST(r.env_named AS VARCHAR) AS env_named, \
         r.files, r.dirs, r.env, \
         CAST(p.pid AS BIGINT) AS pid, CAST(p.code AS BIGINT) AS code, CAST(p.signal AS BIGINT) AS signal, \
         CAST((p.exited_ns - p.spawned_ns) / 1e6 AS DOUBLE) AS wall_ms, CAST(p.user_ns / 1e6 AS DOUBLE) AS user_ms, \
         CAST(p.system_ns / 1e6 AS DOUBLE) AS system_ms, CAST(p.peak_rss AS BIGINT) AS peak_rss \
         FROM {} AS r LEFT JOIN processes AS p \
         ON p.build_id IS NOT DISTINCT FROM r.build_id AND p.kind = 'build_script' AND p.unit = r.unit \
         WHERE r.unit = {} ORDER BY r.t_ns DESC, p.exited_ns DESC NULLS LAST LIMIT 1",
        runs(),
        quoted(unit)
    )
}

/// The calls the sandbox described of `unit`'s run in `build` by the daemon `process`, in the
/// order it said them.
fn calls_sql(unit: &str, build: Option<u64>, process: u64) -> String {
    let build = build.map_or("NULL".to_owned(), |it| it.to_string());
    format!(
        "SELECT CAST(what AS VARCHAR) AS what, pid, inside_pid, CAST(program AS VARCHAR) AS program, \
         CAST(argv0 AS VARCHAR) AS argv0, CAST(syscall AS VARCHAR) AS syscall, number, CAST(phase AS VARCHAR) AS phase, \
         CAST(before_main AS VARCHAR) AS before_main, CAST(ip AS VARCHAR) AS ip, CAST(module AS VARCHAR) AS module, \
         CAST(module_kind AS VARCHAR) AS module_kind FROM {} \
         WHERE unit = {} AND build_id IS NOT DISTINCT FROM {build} AND process = {process} ORDER BY t_ns, event_id",
        pivot(
            calls::EVENT_CALL,
            &[
                "unit", "what", "program", "argv0", "syscall", "phase", "before_main", "ip", "module",
                "module_kind"
            ],
            &["pid", "inside_pid", "number"],
        ),
        quoted(unit)
    )
}

/// How many calls the sandbox did not describe of that run.
fn cut_sql(unit: &str, build: Option<u64>, process: u64) -> String {
    let build = build.map_or("NULL".to_owned(), |it| it.to_string());
    format!(
        "SELECT CAST(coalesce(sum(more), 0) AS BIGINT) AS more FROM {} \
         WHERE unit = {} AND build_id IS NOT DISTINCT FROM {build} AND process = {process}",
        pivot(calls::EVENT_CUT, &["unit"], &["more"]),
        quoted(unit)
    )
}

/// Each row of `batches` as its columns by name.
fn rows(batches: &[RecordBatch]) -> Result<Vec<Map<String, Value>>, String> {
    let mut out = Vec::new();
    for batch in batches {
        let schema = batch.schema();
        for row in 0..batch.num_rows() {
            let mut cells = Map::new();
            for (field, column) in schema.fields().iter().zip(batch.columns()) {
                cells.insert(field.name().to_owned(), super::cell(column.as_ref(), row)?);
            }
            out.push(cells);
        }
    }
    Ok(out)
}

fn text_of(row: &Map<String, Value>, key: &str) -> String {
    row.get(key).and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn int_of(row: &Map<String, Value>, key: &str) -> Option<i64> {
    row.get(key).and_then(Value::as_i64)
}

/// An event's list of names (a line each, perhaps ending in `(N more)`), with its count.
fn listed(row: &Map<String, Value>, count: &str, named: &str) -> Listed {
    Listed {
        count: int_of(row, count).and_then(|it| u64::try_from(it).ok()).unwrap_or(0),
        named: text_of(row, named).lines().map(str::to_owned).collect(),
    }
}

/// The run `row` ([`run_sql`]) with the calls `described` of it ([`calls_sql`]) and how many
/// were `not_described`.
fn trace(row: &Map<String, Value>, described: &[Map<String, Value>], not_described: u64) -> Trace {
    let process = int_of(row, "pid").map(|pid| Process {
        pid: u64::try_from(pid).unwrap_or(0),
        code: int_of(row, "code"),
        signal: int_of(row, "signal"),
        wall_ms: row.get("wall_ms").and_then(Value::as_f64).unwrap_or(0.0),
        user_ms: row.get("user_ms").and_then(Value::as_f64).unwrap_or(0.0),
        system_ms: row.get("system_ms").and_then(Value::as_f64).unwrap_or(0.0),
        peak_rss: int_of(row, "peak_rss").and_then(|it| u64::try_from(it).ok()),
    });
    let caller = |it: &Map<String, Value>| Caller {
        what: text_of(it, "what"),
        pid: int_of(it, "pid").unwrap_or(0),
        inside_pid: int_of(it, "inside_pid").filter(|it| *it >= 0),
        program: text_of(it, "program"),
        argv0: text_of(it, "argv0"),
        syscall: text_of(it, "syscall"),
        number: int_of(it, "number").unwrap_or(-1),
        phase: text_of(it, "phase"),
        before_main: text_of(it, "before_main"),
        ip: text_of(it, "ip"),
        module: text_of(it, "module"),
        module_kind: text_of(it, "module_kind"),
    };
    let (children, made): (Vec<Caller>, Vec<Caller>) =
        described.iter().map(caller).partition(|it| it.what == calls::EXEC);
    Trace {
        version: VERSION,
        unit: text_of(row, "unit"),
        build: int_of(row, "build_id").and_then(|it| u64::try_from(it).ok()),
        program: text_of(row, "program"),
        process,
        files: listed(row, "files", "files_named"),
        dirs: listed(row, "dirs", "dirs_named"),
        env: listed(row, "env", "env_named"),
        untraceable: text_of(row, "untraceable")
            .split(", ")
            .filter(|it| !it.is_empty())
            .map(str::to_owned)
            .collect(),
        calls: made,
        children,
        not_described,
    }
}

/// `it`, or `?` for what `/proc` did not give.
fn shown(it: &str) -> &str {
    if it.is_empty() { "?" } else { it }
}

/// A run for a person: the answer on the first line (how it ended, what it read, what can't
/// be traced), then who made each untraceable read, the programs started, and what was read.
pub(super) fn text(trace: &Trace) -> String {
    let mut out = String::new();
    // Writing to a `String` does not fail.
    _ = write!(out, "{}", trace.unit);
    match trace.build {
        Some(build) => _ = write!(out, ", build {build}: "),
        None => out.push_str(", in no build: "),
    }
    match &trace.process {
        Some(Process { code: Some(code), wall_ms, .. }) => {
            _ = write!(out, "exited {code} after {wall_ms:.0} ms")
        }
        Some(Process { signal: Some(signal), wall_ms, .. }) => {
            _ = write!(out, "ended by signal {signal} after {wall_ms:.0} ms")
        }
        Some(Process { wall_ms, .. }) => _ = write!(out, "ended after {wall_ms:.0} ms"),
        None => out.push_str("its end is not in the history (no `processes` row of it)"),
    }
    _ = write!(
        out,
        "; read {} files, {} directories, {} variables; ",
        trace.files.count, trace.dirs.count, trace.env.count
    );
    match trace.untraceable.is_empty() {
        true => out.push_str("nothing it read can't be traced\n"),
        false => _ = writeln!(out, "can't be traced: {}", trace.untraceable.join(", ")),
    }
    _ = writeln!(out, "  program {}", shown(&trace.program));
    if let Some(Process { pid, user_ms, system_ms, peak_rss, .. }) = &trace.process {
        _ = write!(out, "  process {pid}: user {user_ms:.0} ms, system {system_ms:.0} ms");
        match peak_rss {
            Some(bytes) => _ = writeln!(out, ", peak {:.1} MiB", *bytes as f64 / 1048576.0),
            None => out.push('\n'),
        }
    }
    if !trace.untraceable.is_empty() || !trace.calls.is_empty() {
        _ = writeln!(out, "who read what can't be traced ({}):", trace.calls.len());
        if trace.calls.is_empty() {
            _ = writeln!(
                out,
                "  no caller was described: on Linux the sandbox's supervisor describes each process's first call of a kind; \
                 macOS's and Windows's sandboxes say what was read, not who (`descriptor` has no caller: it is a file read through a descriptor the sandbox did not see opened)"
            );
        }
    }
    for it in &trace.calls {
        _ = write!(out, "  {}: `{}` ({}) by process {}", it.what, it.syscall, it.number, it.pid);
        if let Some(inside) = it.inside_pid {
            _ = write!(out, " ({inside} in the sandbox)");
        }
        _ = write!(out, " `{}` (argv[0] `{}`)", shown(&it.program), shown(&it.argv0));
        match it.module.is_empty() {
            true => _ = write!(out, ", from {} at {}", it.module_kind, it.ip),
            false => _ = write!(out, ", from {} `{}` at {}", it.module_kind, it.module, it.ip),
        }
        _ = writeln!(out, "; before main: {} (phase: {})", it.before_main, it.phase);
    }
    if !trace.children.is_empty() {
        _ = writeln!(out, "programs started ({}):", trace.children.len());
    }
    for it in &trace.children {
        _ = write!(out, "  `{}` by process {}", shown(&it.program), it.pid);
        if let Some(inside) = it.inside_pid {
            _ = write!(out, " ({inside} in the sandbox)");
        }
        _ = writeln!(out, " `{}`", shown(&it.argv0));
    }
    if trace.not_described > 0 {
        _ = writeln!(
            out,
            "  ({} more calls or programs were not described: a run's first {} calls and {} programs are)",
            trace.not_described,
            calls::MOST,
            calls::EXECS_MOST
        );
    }
    for (what, listed) in [
        ("files read", &trace.files),
        ("directories listed", &trace.dirs),
        ("variables read", &trace.env),
    ] {
        if listed.count == 0 {
            continue;
        }
        _ = writeln!(out, "{what} ({}):", listed.count);
        for name in &listed.named {
            _ = writeln!(out, "  {name}");
        }
    }
    out
}

/// `blaze explain script UNIT [--json]`.
pub(super) fn explain(words: impl Iterator<Item = String>) -> Result<(), Refusal> {
    let (mut asked, mut json) = (None, false);
    for word in words {
        match word.as_str() {
            "--json" => json = true,
            flag if flag.starts_with("--") => {
                return Err(Refusal::Usage(format!("{USAGE}, not `{flag}`")));
            }
            _ if asked.is_some() => {
                return Err(Refusal::Usage(format!("{USAGE}: one unit, quoted, not `{word}` too")));
            }
            _ => asked = Some(word),
        }
    }
    let Some(asked) = asked else {
        // With no unit: the scripts whose runs the history holds, so the next command is one.
        let units = rows(&super::run(&units_sql(None))?)?;
        let names: Vec<String> = units.iter().map(|it| text_of(it, "unit")).collect();
        return Err(Refusal::Usage(match names.is_empty() {
            true => format!(
                "{USAGE}; the profile history holds no sandboxed build script run (a run reused from the store ran nothing)"
            ),
            false => format!("{USAGE}; the history's runs are of: {}", names.join("; ")),
        }));
    };
    let units = rows(&super::run(&units_sql(Some(&asked)))?)?;
    let names: Vec<String> = units.iter().map(|it| text_of(it, "unit")).collect();
    let unit = match names.as_slice() {
        [unit] => unit,
        [] => {
            let all = rows(&super::run(&units_sql(None))?)?;
            let all: Vec<String> = all.iter().map(|it| text_of(it, "unit")).collect();
            return Err(Refusal::Failed(match all.is_empty() {
                true => format!(
                    "`blaze explain script`: the profile history holds no sandboxed run of a build script, so none of `{asked}`: \
                     a run reused from the store ran nothing; a build that runs it (after `blaze clean`, or a change to what it read) records it"
                ),
                false => format!(
                    "`blaze explain script`: the profile history holds no sandboxed run of a build script named `{asked}` \
                     (a run reused from the store ran nothing); it holds runs of: {}",
                    all.join("; ")
                ),
            }));
        }
        several => {
            return Err(Refusal::Usage(format!(
                "`blaze explain script`: `{asked}` names {} build scripts' runs; say which: {}",
                several.len(),
                several.join("; ")
            )));
        }
    };
    let found = rows(&super::run(&run_sql(unit))?)?;
    let Some(row) = found.first() else {
        return Err(Refusal::Failed(format!(
            "`blaze explain script`: the run of `{unit}` the history listed is gone from it (its build was deleted under the history's cap meanwhile): ask again"
        )));
    };
    let build = int_of(row, "build_id").and_then(|it| u64::try_from(it).ok());
    let process = int_of(row, "process").and_then(|it| u64::try_from(it).ok()).unwrap_or(0);
    let made = rows(&super::run(&calls_sql(unit, build, process))?)?;
    let cut = rows(&super::run(&cut_sql(unit, build, process))?)?;
    let not_described = cut
        .first()
        .and_then(|it| int_of(it, "more"))
        .and_then(|it| u64::try_from(it).ok())
        .unwrap_or(0);
    let trace = trace(row, &made, not_described);
    let said = match json {
        true => {
            serde_json::to_string(&trace)
                .map_err(|error| format!("`blaze explain script`'s answer as JSON: {error}"))?
                + "\n"
        }
        false => text(&trace),
    };
    super::stdout(&said)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn object(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(it) => it,
            other => panic!("{other}"),
        }
    }

    /// The rows a fixture build's history gives of one run: its event, its `processes` row,
    /// and the two calls its sandbox described.
    fn fixture() -> Trace {
        let row = object(json!({
            "build_id": 12, "process": 900, "t_ns": 5,
            "unit": "libc v0.2.155 (build script run)",
            "program": "/ws/target/debug/build/libc-1f/build-script-build",
            "untraceable": "random",
            "files_named": "/ws/libc/build.rs.in\n/usr/lib/libc.so.6\n(39 more)",
            "dirs_named": "/ws/libc/src", "env_named": "CARGO_CFG_TARGET_OS\nTARGET",
            "files": 41, "dirs": 1, "env": 2,
            "pid": 4700, "code": 0, "signal": null,
            "wall_ms": 412.4, "user_ms": 9.2, "system_ms": 4.0, "peak_rss": 6_396_314,
        }));
        let calls = [
            object(json!({
                "what": "random", "pid": 4711, "inside_pid": 7,
                "program": "/ws/target/debug/build/libc-1f/build-script-build",
                "argv0": "build-script-build", "syscall": "getrandom", "number": 318,
                "phase": "loading", "before_main": "yes (a heuristic)", "ip": "0x7f1c2a0e4b2d",
                "module": "/usr/lib/libc.so.6", "module_kind": "libc",
            })),
            object(json!({
                "what": "exec", "pid": 4711, "inside_pid": -1, "program": "/usr/bin/cc",
                "argv0": "build-script-build", "syscall": "execve", "number": 59,
                "phase": "loaded", "before_main": "not known", "ip": "0x7f1c2a0e0000",
                "module": "", "module_kind": "not known",
            })),
        ];
        trace(&row, &calls, 3)
    }

    #[test]
    fn a_runs_first_line_is_the_answer_and_each_caller_is_named() {
        let said = text(&fixture());
        assert_eq!(
            said,
            "libc v0.2.155 (build script run), build 12: exited 0 after 412 ms; read 41 files, 1 directories, 2 variables; can't be traced: random\n\
             \x20 program /ws/target/debug/build/libc-1f/build-script-build\n\
             \x20 process 4700: user 9 ms, system 4 ms, peak 6.1 MiB\n\
             who read what can't be traced (1):\n\
             \x20 random: `getrandom` (318) by process 4711 (7 in the sandbox) `/ws/target/debug/build/libc-1f/build-script-build` (argv[0] `build-script-build`), from libc `/usr/lib/libc.so.6` at 0x7f1c2a0e4b2d; before main: yes (a heuristic) (phase: loading)\n\
             programs started (1):\n\
             \x20 `/usr/bin/cc` by process 4711 `build-script-build`\n\
             \x20 (3 more calls or programs were not described: a run's first 32 calls and 32 programs are)\n\
             files read (41):\n\
             \x20 /ws/libc/build.rs.in\n\
             \x20 /usr/lib/libc.so.6\n\
             \x20 (39 more)\n\
             directories listed (1):\n\
             \x20 /ws/libc/src\n\
             variables read (2):\n\
             \x20 CARGO_CFG_TARGET_OS\n\
             \x20 TARGET\n"
        );
    }

    #[test]
    fn the_json_has_a_version_and_says_what_is_not_known_as_null() {
        let trace = fixture();
        let json: Value = serde_json::from_str(&serde_json::to_string(&trace).unwrap()).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["build"], 12);
        assert_eq!(json["process"]["code"], 0);
        assert_eq!(json["process"]["signal"], Value::Null);
        assert_eq!(json["calls"][0]["what"], "random");
        assert_eq!(json["calls"][0]["inside_pid"], 7);
        assert_eq!(json["children"][0]["program"], "/usr/bin/cc");
        assert_eq!(json["children"][0]["inside_pid"], Value::Null, "not said, never -1");
        assert_eq!(json["files"]["count"], 41);
        assert_eq!(json["not_described"], 3);
    }

    #[test]
    fn a_run_with_no_process_row_or_caller_says_so() {
        let row = object(json!({
            "build_id": null, "process": 900, "unit": "x v1.0.0 (build script run)",
            "program": "", "untraceable": "descriptor", "files_named": "", "dirs_named": "",
            "env_named": "", "files": 0, "dirs": 0, "env": 0, "pid": null,
        }));
        let said = text(&trace(&row, &[], 0));
        assert!(said.starts_with("x v1.0.0 (build script run), in no build: its end is not in the history"), "{said}");
        assert!(said.contains("can't be traced: descriptor"), "{said}");
        assert!(said.contains("program ?") && said.contains("no caller was described"), "{said}");
    }

    #[test]
    fn a_unit_is_matched_whole_or_by_its_package_and_quoted() {
        let sql = units_sql(Some("it's"));
        assert!(sql.contains("unit = 'it''s' OR starts_with(unit, 'it''s' || ' ')"), "{sql}");
        assert!(sql.contains("e.message = 'script run traced'"), "{sql}");
        assert!(calls_sql("u", None, 9).contains("build_id IS NOT DISTINCT FROM NULL AND process = 9"));
        assert!(cut_sql("u", Some(3), 9).contains("e.message = 'script calls not described'"));
    }
}
