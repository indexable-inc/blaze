//! guards: the guards of the budget smoke. The port of `harness/v2/guards.py`, line for line:
//! the same one JSON argument, `selftest`, text printed and exit status
//! (docs/v2/plan/design/scripts.md, "Ports and their parity"). `DOC` below is the Python's
//! docstring, which its usage error prints.
//!
//!     blaze run -q -p scripts --bin guards -- '{"command": "held", "result": "/tmp/budgets-smoke.json"}'
//!     blaze run -q -p scripts --bin guards -- '{"command": "table"}'
//!     blaze run -q -p scripts --bin guards -- selftest
//!
//! The table, the facts' query and the edits are here whole, for `selftest` and for the day
//! budgets.py is ported; until then harness/v2/budgets.py imports guards.py's, so the two
//! tables are one only while both are edited together (the switch deletes guards.py once
//! budgets.py no longer imports it).
//!
//! Where this differs from the Python, on purpose: the words after "is no JSON:" are
//! serde_json's, and a failing selftest names a set of checks sorted where Python's set had
//! its own order.

/// harness/v2/guards.py's docstring, as its usage error prints it.
const DOC: &str = r#"guards.py: the guards of the budget smoke (harness/v2/budgets.py, `smoke`): what a warm build
must not cost, held on two fixed fixtures by the merge gate with the blaze the change built.

    uv run --frozen python harness/v2/guards.py '{"command": "held", "result": "/tmp/budgets-smoke.json"}'
    uv run --frozen python harness/v2/guards.py '{"command": "table"}'
    uv run --frozen python harness/v2/guards.py selftest

The rule it enforces: work that should be zero is counted, and what is counted is held by the
gate, so a regression of ten times is a red run at the change that made it and not a figure
someone finds a week later (rule guard-the-cause; docs/harness.md, "The smoke's guards").

The fixtures are a one-file program (`onefile_source`) and fixture.py's workspace. On each the
smoke times three warm builds, one of each `Step`:

    noop      `blaze build` with nothing changed
    comment   a comment added at the end of a line (no line moves, no token changes), then a build
    body      a statement added to a function's body, then a build

and after each asks the daemon's profile for what that build did (`facts_sql`,
`uncovered_sql`, through `blaze debug sql --json`) and reads the daemon's log. `BOUNDS` is the one
table of what each may cost; `judge` holds a step's facts to it. The bounds are loose in absolute
terms: they are for the gate's machine class (16 cores, a blaze the tree built) and catch a
regression of an order of magnitude, not of 10%. They tighten as the numbers fall: edit the table.

A bound that fails today is listed in harness/v2/guards-known.toml with the number measured and
its cause, as harness/known-failures.toml lists the unit tests that fail: a listed check may fail
by no more than what is recorded (so it cannot get worse), one listed that passes fails the run
until it is taken out (so the list only shrinks), and one not listed that fails fails the run.
`budgets.py '{"command": "smoke", "record": true, ...}'` writes the list again from the run,
keeping the cause of an entry that was there.

Nothing asked for is dropped: `names()` is every outcome the smoke has to give, and `held`
(the gate's `ci` job runs it on the smoke's result) fails when one is missing or did not pass, or
when there is no result at all. A gate run whose smoke did not run is red.

`held`'s input is one JSON object: `result`, the path of the smoke's result (report.py's JSON).
`table` prints `BOUNDS` as docs/harness.md shows it. `selftest` needs no blaze: it replays the
finding each bound is for, in miniature, as facts the bound has to refuse, and the fixed state as
facts it has to pass.
"#;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use blaze_script::{
    Value, fail,
    input::{self, Fields, Input, InputError},
    pyfmt::{g, py_str, repr, repr_str, repr_strs},
    pyjson::dumps,
    pystr::{splitlines, strip},
    read_text,
};

const SCRIPT: &str = "guards.py";
const STEPS: [&str; 3] = ["noop", "comment", "body"];
const FIXTURES: [&str; 2] = ["onefile", "workspace"];

/// One thing counted on a warm build and the most it may be on each step (None: not held on
/// that step). `needs_task`: the fact is of the build's tasks, so a build that ran none has
/// none to give, and passes.
struct Bound {
    check: &'static str,
    unit: &'static str,
    noop: Option<f64>,
    comment: Option<f64>,
    body: Option<f64>,
    what: &'static str,
    catches: &'static str,
    needs_task: bool,
}

impl Bound {
    fn limit(&self, step: &str) -> Option<f64> {
        match step {
            "noop" => self.noop,
            "comment" => self.comment,
            _ => self.body,
        }
    }
}

const fn bound(
    check: &'static str,
    unit: &'static str,
    limits: [Option<f64>; 3],
    what: &'static str,
    catches: &'static str,
) -> Bound {
    Bound { check, unit, noop: limits[0], comment: limits[1], body: limits[2], what, catches, needs_task: false }
}

// The one table. The times are for the gate's machine class and loose on purpose; the counts
// are what the design says: zero where nothing changed. A comment at a line's end is held to
// what a no-op is (2026-11-10): the no-op lane answers it at the file, with no job, no
// lowering and no query (blaze-cargo's `serve::job::noop`, "An edit that changes no code").
const BOUNDS: [Bound; 11] = [
    bound(
        "ms",
        "ms",
        [Some(300.0), Some(300.0), Some(2000.0)],
        "the client's wait: `blaze build` from its spawn to its exit",
        "edits that took 25 to 39 s for days (2026-11-10)",
    ),
    bound(
        "let_go",
        "spans",
        [Some(0.0), Some(0.0), Some(0.0)],
        "`kept front ends let go for memory` spans since the fixture was warm",
        "every build dropped the kept front end and lowered its program anew",
    ),
    bound(
        "let_go_logged",
        "lines",
        [Some(0.0), Some(0.0), Some(0.0)],
        "lines of the daemon's log, since the fixture was warm, saying a kept front end is let go",
        "the same, from the log: \"a task waits for 0.03 GiB\"",
    ),
    bound(
        "ran_new",
        "runs",
        [Some(0.0), Some(0.0), Some(256.0)],
        "`query_counts.ran_new` summed: first runs of a row, in a database that was warm",
        "a warm edit that ran the library's queries anew, as a new front end does",
    ),
    bound(
        "ran_untracked",
        "runs",
        [Some(0.0), Some(0.0), None],
        "`query_counts.ran_untracked` summed: runs because of an untracked read",
        "`identity_of`, untracked, ran about 10,000 times on every build",
    ),
    bound(
        "executed",
        "runs",
        [Some(0.0), Some(0.0), None],
        "`query_counts.executed` summed: every query run",
        "work on a build that changed nothing",
    ),
    bound(
        "lower_ms",
        "ms",
        [Some(5.0), Some(5.0), None],
        "the `lower workspace` spans of the build, summed",
        "a comment edit spent about 200 ms lowering a workspace that had not changed",
    ),
    bound(
        "maintain_ms",
        "ms",
        [Some(5.0), Some(5.0), None],
        "the `maintain` spans of the build, summed (`maintainer reads` is inside them)",
        "a comment edit spent about 190 ms in maintainer reads though nothing they read changed",
    ),
    Bound {
        needs_task: true,
        ..bound(
            "answer_delay_ms",
            "ms",
            [Some(100.0), Some(100.0), Some(100.0)],
            "from the end of the build's last `task` span to the end of its `request: read to reply` span",
            "a build's answer held 4 s for a disk sync of derived data",
        )
    },
    bound(
        "turn_uncovered_ms",
        "ms",
        [Some(50.0), Some(50.0), Some(50.0)],
        "the most time one `loop turn` span has that no child span of it covers",
        "the daemon's loop thread spent 0.8 to 1.1 s in one turn with no span",
    ),
    bound(
        "uncovered_ms",
        "ms",
        [Some(100.0), Some(100.0), Some(100.0)],
        "the view `uncovered_time` of the build: wall time inside no span",
        "time the profile cannot name (every-wait-says-what, slow-is-p0-traced)",
    ),
];
// The other outcome the smoke has to give: the build-script sandbox's (`probe_source`).
const PROBE: &str = "guard: sandbox: a build script that runs rustc ends, and its probe compiles";
/// What a known failure may exceed its recorded number by: a count is exact, a time is noisy.
fn slack(unit: &str) -> f64 {
    if unit == "ms" { 1.5 } else { 1.0 }
}
const LET_GO_LINE: &str = "is let go, and its next build lowers anew";

fn name(fixture: &str, step: &str, check: &str) -> String {
    format!("guard: {fixture}: {step}: {check}")
}

/// Every outcome the smoke has to give: a bound on a step of a fixture, and the probe.
fn names() -> Vec<String> {
    let mut out = Vec::new();
    for fixture in FIXTURES {
        for step in STEPS {
            for b in &BOUNDS {
                if b.limit(step).is_some() {
                    out.push(name(fixture, step, b.check));
                }
            }
        }
    }
    out.push(PROBE.to_owned());
    out
}

// What the profile says of the last build. `warm_from` is the daemon's clock (ns) when the
// fixture was warm (`mark_sql`). A span is taken by when it began, not by its `build_id`: the
// loop thread's spans are of no build. The reply's span may end after the build's row does (an
// answer held back), so the window runs to the later of the two.
const FACTS_SQL: &str = "
WITH b AS (
    SELECT build_id, started_ns AS t0, started_ns + coalesce(dur_ns, 0) AS t1
    FROM builds ORDER BY build_id DESC LIMIT 1
), reply AS (
    SELECT max(s.start_ns + s.dur_ns) AS at
    FROM spans s, b
    WHERE s.name = 'request: read to reply' AND s.start_ns <= b.t1 AND s.start_ns + s.dur_ns >= b.t0
), w AS (
    SELECT b.build_id, b.t0, greatest(b.t1, coalesce(reply.at, b.t1)) AS t1, reply.at AS replied
    FROM b, reply
), tasks AS (
    SELECT max(s.start_ns + s.dur_ns) AS last_end, count(*) AS n
    FROM spans s, w
    WHERE s.name = 'task' AND s.start_ns >= w.t0 AND s.start_ns <= w.t1
), turns AS (
    SELECT any_value(t.dur_ns) - coalesce(sum(c.dur_ns), 0) AS own_ns
    FROM spans t
    JOIN w ON t.start_ns >= w.t0 AND t.start_ns <= w.t1
    LEFT JOIN spans c ON c.parent = t.span_id AND c.process = t.process
    WHERE t.name = 'loop turn'
    GROUP BY t.process, t.span_id
), worst AS (
    SELECT coalesce(max(own_ns), 0) AS own_ns FROM turns
), named AS (
    SELECT
        coalesce(sum(s.dur_ns) FILTER (WHERE s.name = 'lower workspace'), 0) AS lower_ns,
        coalesce(sum(s.dur_ns) FILTER (WHERE s.name = 'maintain'), 0) AS maintain_ns
    FROM spans s, w
    WHERE s.start_ns >= w.t0 AND s.start_ns <= w.t1
), counts AS (
    SELECT coalesce(sum(q.executed), 0) AS executed, coalesce(sum(q.ran_new), 0) AS ran_new,
        coalesce(sum(q.ran_untracked), 0) AS ran_untracked
    FROM query_counts q, w
    WHERE q.build_id = w.build_id
), gone AS (
    SELECT count(*) AS n FROM spans s
    WHERE s.name = 'kept front ends let go for memory' AND s.start_ns >= @WARM_FROM@
)
SELECT w.build_id AS build_id,
    counts.executed AS executed, counts.ran_new AS ran_new, counts.ran_untracked AS ran_untracked,
    gone.n AS let_go,
    named.lower_ns / 1e6 AS lower_ms, named.maintain_ns / 1e6 AS maintain_ms,
    CASE WHEN tasks.n > 0 AND w.replied IS NOT NULL THEN (w.replied - tasks.last_end) / 1e6 END AS answer_delay_ms,
    worst.own_ns / 1e6 AS turn_uncovered_ms,
    tasks.n AS tasks
FROM w, counts, gone, named, tasks, worst
";
// UNCOVERED_SQL and MARK_SQL, and the fixtures' files, are budgets.py's alone: they come with
// its port.
// The facts `FACTS_SQL` and `UNCOVERED_SQL` give, by column; `ms` and `let_go_logged` are the harness's own.
const PROFILE_FACTS: [&str; 10] = [
    "executed",
    "ran_new",
    "ran_untracked",
    "let_go",
    "lower_ms",
    "maintain_ms",
    "answer_delay_ms",
    "turn_uncovered_ms",
    "tasks",
    "build_id",
];

fn facts_sql(warm_from: i64) -> String {
    FACTS_SQL.replace("@WARM_FROM@", &warm_from.to_string())
}

/// The first 200 characters, as Python's `line[:200]`.
fn first_200(line: &str) -> String {
    line.chars().take(200).collect()
}

/// The rows `blaze debug sql --json` printed: an object a line. A line that is none is an
/// error naming `what`.
fn rows(stdout: &str, what: &str) -> Result<Vec<blaze_script::Map<String, Value>>, String> {
    let mut out = Vec::new();
    for line in splitlines(stdout) {
        if strip(line).is_empty() {
            continue;
        }
        let row: Value = blaze_script::json::from_str(line).map_err(|e| {
            format!(
                "{what}: `blaze debug sql --json` printed a line that is no JSON ({e}): {}",
                repr_str(&first_200(line))
            )
        })?;
        let Value::Object(row) = row else {
            return Err(format!(
                "{what}: `blaze debug sql --json` printed a line that is no object: {}",
                repr_str(&first_200(line))
            ));
        };
        out.push(row);
    }
    Ok(out)
}

fn one_row(stdout: &str, what: &str) -> Result<blaze_script::Map<String, Value>, String> {
    let mut got = rows(stdout, what)?;
    if got.len() != 1 {
        return Err(format!("{what}: {} rows, where the query gives one (the history has no build?)", got.len()));
    }
    Ok(got.remove(0))
}

/// `column` of `row` as a number, None where it is null. A row without it, or with something
/// else in it, is an error: the profile's schema moved and the query has to follow it.
fn number(row: &blaze_script::Map<String, Value>, column: &str, what: &str) -> Result<Option<f64>, String> {
    let Some(value) = row.get(column) else {
        let mut keys: Vec<&str> = row.keys().map(String::as_str).collect();
        keys.sort_unstable();
        return Err(format!("{what}: the row has no column `{column}`: {}", repr_strs(&keys)));
    };
    match value {
        Value::Null => Ok(None),
        Value::Number(n) => Ok(Some(n.as_f64().unwrap_or(0.0))),
        other => Err(format!("{what}: `{column}` is {}, no number", repr(other))),
    }
}

/// How many lines of the daemon's log say a kept front end was let go.
fn let_go_lines(log: &str) -> usize {
    splitlines(log).iter().filter(|line| line.contains(LET_GO_LINE)).count()
}

/// A bound known to fail: which, the number measured when it was recorded, and why.
#[derive(Debug, Clone, PartialEq)]
struct Known {
    fixture: String,
    step: String,
    check: String,
    measured: f64,
    cause: String,
}

impl Known {
    fn new(fixture: &str, step: &str, check: &str, measured: f64, cause: &str) -> Known {
        Known {
            fixture: fixture.to_owned(),
            step: step.to_owned(),
            check: check.to_owned(),
            measured,
            cause: cause.to_owned(),
        }
    }

    fn name(&self) -> String {
        name(&self.fixture, &self.step, &self.check)
    }
}

/// A TOML value as the JSON value of the same Python value, for its `repr`.
fn toml_json(value: &toml::Value) -> Value {
    match value {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => Value::from(*i),
        toml::Value::Float(f) => Value::from(*f),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(items) => Value::Array(items.iter().map(toml_json).collect()),
        toml::Value::Table(t) => Value::Object(t.iter().map(|(k, v)| (k.clone(), toml_json(v))).collect()),
    }
}

fn sorted_keys(t: &toml::Table) -> String {
    let mut keys: Vec<&str> = t.keys().map(String::as_str).collect();
    keys.sort_unstable();
    repr_strs(&keys)
}

/// The list harness/v2/guards-known.toml holds. An entry of no bound, a field missing or of
/// another type, and the same bound twice are each an error naming the entry; so is TOML that
/// does not parse (Python's `TOMLDecodeError`).
fn read_known(text: &str) -> Result<Vec<Known>, String> {
    let data: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
    let empty = toml::Value::Array(Vec::new());
    let entries = data.get("known").unwrap_or(&empty);
    let extra = data.keys().any(|k| k != "known" && k != "recorded");
    let toml::Value::Array(entries) = entries else {
        return Err(format!(
            "guards-known.toml: its tables are [[known]] and [recorded], not {}",
            sorted_keys(&data)
        ));
    };
    if extra {
        return Err(format!(
            "guards-known.toml: its tables are [[known]] and [recorded], not {}",
            sorted_keys(&data)
        ));
    }
    let mut out: Vec<Known> = Vec::new();
    let wanted: BTreeSet<String> = names().into_iter().collect();
    for (at, raw) in entries.iter().enumerate() {
        let place = format!("guards-known.toml: [[known]] {}", at + 1);
        let toml::Value::Table(entry) = raw else { return Err(format!("{place}: not a table")) };
        let fields: BTreeSet<&str> = entry.keys().map(String::as_str).collect();
        if fields != BTreeSet::from(["fixture", "step", "check", "measured", "cause"]) {
            return Err(format!(
                "{place}: its fields are fixture, step, check, measured and cause, not {}",
                sorted_keys(entry)
            ));
        }
        let text_of = |key: &str| entry.get(key).and_then(toml::Value::as_str);
        let fixture = text_of("fixture").filter(|it| FIXTURES.contains(it));
        let step = text_of("step").filter(|it| STEPS.contains(it));
        let (Some(fixture), Some(step), Some(check)) = (fixture, step, text_of("check")) else {
            return Err(format!(
                "{place}: no fixture {} with a step {}",
                repr(&toml_json(&entry["fixture"])),
                repr(&toml_json(&entry["step"]))
            ));
        };
        let measured = match &entry["measured"] {
            toml::Value::Integer(i) => *i as f64,
            toml::Value::Float(f) => *f,
            other => return Err(format!("{place}: `measured` is a number, not {}", repr(&toml_json(other)))),
        };
        let cause = match text_of("cause") {
            Some(cause) if !strip(cause).is_empty() => cause,
            _ => return Err(format!("{place}: `cause` says in a line what fails and why, and is empty")),
        };
        let known = Known::new(fixture, step, check, measured, cause);
        if !wanted.contains(&known.name()) {
            return Err(format!("{place}: `{}` is no bound of guards.py's table", known.name()));
        }
        if out.iter().any(|it| it.name() == known.name()) {
            return Err(format!("{place}: `{}` is listed twice", known.name()));
        }
        out.push(known);
    }
    Ok(out)
}

/// Python's `round(x, 3)`: the nearest number of three decimals.
fn round3(x: f64) -> f64 {
    format!("{x:.3}").parse().unwrap_or(x)
}

/// guards-known.toml's text: the entries sorted by name, then what they were recorded from.
fn render_known(known: &[Known], recorded: &BTreeMap<String, String>) -> String {
    let mut lines: Vec<String> = [
        "# The budget smoke's bounds known to fail (harness/v2/guards.py; docs/harness.md, \"The smoke's",
        "# guards\"). Each entry is one bound on one step of one fixture, the number measured when it was",
        "# recorded, and the cause in a line. A listed bound may fail by no more than `measured` (a time by",
        "# half as much again, for noise); one listed that passes fails the run until it is taken out; one",
        "# not listed that fails fails the run. The list only shrinks. Written by:",
        "#   uv run --frozen python harness/v2/budgets.py '{\"command\": \"smoke\", \"record\": true, \"engine\": {...}}'",
        "",
    ]
    .iter()
    .map(|it| (*it).to_owned())
    .collect();
    let mut sorted: Vec<&Known> = known.iter().collect();
    sorted.sort_by_key(|it| it.name());
    let quoted = |s: &str| dumps(&Value::String(s.to_owned()));
    for it in sorted {
        let measured = if it.measured == it.measured.trunc() && it.measured.is_finite() {
            format!("{}", it.measured as i64)
        } else {
            blaze_script::pyjson::float(round3(it.measured))
        };
        lines.extend([
            "[[known]]".to_owned(),
            format!("fixture = {}", quoted(&it.fixture)),
            format!("step = {}", quoted(&it.step)),
            format!("check = {}", quoted(&it.check)),
            format!("measured = {measured}"),
            format!("cause = {}", quoted(&it.cause)),
            String::new(),
        ]);
    }
    lines.push("[recorded]".to_owned());
    lines.extend(recorded.iter().map(|(key, value)| format!("{key} = {}", quoted(value))));
    format!("{}\n", lines.join("\n"))
}

/// One bound held to one step's facts: the outcome's name, its verdict and what to say.
#[derive(Debug, Clone)]
struct Judged {
    name: String,
    verdict: &'static str,
    #[expect(dead_code, reason = "budgets.py reads it; here only the selftest judges")]
    detail: String,
    measured: Option<f64>,
    limit: f64,
}

/// The facts of one build: a number a check, None where the profile gave none.
type Facts = BTreeMap<String, Option<f64>>;

/// Every bound of `step` held to `facts`, with the known failures of the list allowed as far
/// as they were recorded.
fn judge(fixture: &str, step: &str, facts: &Facts, known: &[Known]) -> Vec<Judged> {
    let mut out = Vec::new();
    let listed: BTreeMap<String, &Known> = known.iter().map(|it| (it.name(), it)).collect();
    for b in &BOUNDS {
        let Some(limit) = b.limit(step) else { continue };
        let at = name(fixture, step, b.check);
        let was = listed.get(&at);
        let value = facts.get(b.check).copied().flatten();
        let tail = format!(" ({}; it catches: {})", b.what, b.catches);
        let judged = |verdict: &'static str, detail: String, measured: Option<f64>| Judged {
            name: at.clone(),
            verdict,
            detail,
            measured,
            limit,
        };
        let Some(value) = value else {
            if b.needs_task && facts.get("tasks").copied().flatten() == Some(0.0) {
                let detail = "the build ran no task, so nothing was held back after one".to_owned();
                out.push(match was {
                    None => judged("pass", detail, None),
                    Some(was) => judged(
                        "fail",
                        format!("{detail}; it is listed as a known failure ({}): take it out", was.cause),
                        None,
                    ),
                });
            } else {
                out.push(judged("unmeasured", format!("the profile gave no number for it{tail}"), None));
            }
            continue;
        };
        let said = format!("{} {} against at most {}", g(value), b.unit, g(limit));
        out.push(if value <= limit {
            match was {
                None => judged("pass", said, Some(value)),
                Some(was) => judged(
                    "fail",
                    format!(
                        "{said}: it passes and is listed as a known failure ({}); take it \
                         out of harness/v2/guards-known.toml, the list only shrinks",
                        was.cause
                    ),
                    Some(value),
                ),
            }
        } else {
            match was {
                None => judged("fail", format!("{said}{tail}"), Some(value)),
                Some(was) if value <= was.measured * slack(b.unit) => judged(
                    "pass",
                    format!("{said}: a known failure, recorded at {} ({})", g(was.measured), was.cause),
                    Some(value),
                ),
                Some(was) => judged(
                    "fail",
                    format!(
                        "{said}: a known failure that got worse than the {} recorded ({}){tail}",
                        g(was.measured),
                        was.cause
                    ),
                    Some(value),
                ),
            }
        });
    }
    out
}

/// The known list a run gives: each bound that is over its limit, with the number measured,
/// keeping the cause of an entry that was there.
fn recorded_from(judged: &[(&str, &str, Judged)], before: &[Known]) -> Vec<Known> {
    let causes: BTreeMap<String, &str> = before.iter().map(|it| (it.name(), it.cause.as_str())).collect();
    let mut out = Vec::new();
    for (fixture, step, it) in judged {
        let Some(measured) = it.measured.filter(|m| *m > it.limit) else { continue };
        let check = it.name.rsplit_once(": ").map_or("", |(_, check)| check);
        let catches = BOUNDS.iter().find(|b| b.check == check).map_or("", |b| b.catches);
        let cause = match causes.get(&it.name) {
            Some(cause) if !cause.is_empty() => (*cause).to_owned(),
            _ => format!("not yet said; it catches: {catches}"),
        };
        out.push(Known::new(fixture, step, check, measured, &cause));
    }
    out
}

/// What is wrong with a smoke's result (report.py's JSON) as the gate reads it: an outcome of
/// `names()` that is missing, there twice, or not a pass. Empty when the guards ran and held.
fn held(result: &Value) -> Vec<String> {
    let Value::Object(result) = result else { return vec!["the result is no JSON object".to_owned()] };
    let Some(Value::Array(outcomes)) = result.get("outcomes") else {
        return vec!["the result has no `outcomes`".to_owned()];
    };
    let mut seen: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for raw in outcomes {
        if let Value::Object(outcome) = raw {
            let field = |key: &str| py_str(outcome.get(key).unwrap_or(&Value::Null));
            seen.entry(field("name")).or_default().push(field("verdict"));
        }
    }
    let mut problems = Vec::new();
    for wanted in names() {
        let got = seen.get(&wanted).map_or(&[][..], Vec::as_slice);
        if got.is_empty() {
            problems.push(format!("`{wanted}` did not run: the smoke gave no outcome for it"));
        } else if got.len() > 1 {
            problems.push(format!("`{wanted}` has {} outcomes", got.len()));
        } else if got[0] != "pass" {
            problems.push(format!("`{wanted}`: {}", got[0]));
        }
    }
    problems
}

// The fixtures and their edits.

const ONEFILE_OPENING: &str = "fn double(x: u32) -> u32 {";
/// The one-file program's `src/main.rs` (`ONEFILE[ONEFILE_FILE]`).
fn onefile_main() -> String {
    format!("{ONEFILE_OPENING}\n    x.wrapping_mul(2)\n}}\n\nfn main() {{\n    println!(\"{{}}\", double(21));\n}}\n")
}

/// Where the one line `opening` is in `text`, and where it ends.
fn opening_line(text: &str, opening: &str) -> Result<(usize, usize), String> {
    let start = text.find(opening);
    // `text.find(opening, start + 1)`: from the character after the first match.
    let twice = start.is_some_and(|start| {
        let next = start + text[start..].chars().next().map_or(1, char::len_utf8);
        text[next..].contains(opening)
    });
    let Some(start) = start.filter(|_| !twice) else {
        return Err(format!(
            "the text has {} of {}, where an edit needs one",
            text.matches(opening).count(),
            repr_str(opening)
        ));
    };
    let end = text[start..].find('\n').map(|at| start + at);
    match end {
        Some(end) if strip(&text[start + opening.len()..end]).is_empty() => Ok((start, end)),
        _ => Err(format!("{} does not end its line", repr_str(opening))),
    }
}

/// A comment at the end of the line `opening` is: no line moves and no token changes.
fn eol_comment(text: &str, opening: &str, n: i64) -> Result<String, String> {
    let (_, end) = opening_line(text, opening)?;
    Ok(format!("{} // blaze guards: comment {n}{}", &text[..end], &text[end..]))
}

/// A statement as the first line of the body `opening` opens.
fn body_statement(text: &str, opening: &str, n: i64) -> Result<String, String> {
    let (_, end) = opening_line(text, opening)?;
    Ok(format!("{}    let _blaze_guard: u32 = {n};\n{}", &text[..=end], &text[end + 1..]))
}

/// `BOUNDS` as a Markdown table (docs/harness.md's).
fn table() -> String {
    let cell = |limit: Option<f64>| limit.map_or(String::new(), g);
    let mut lines = vec![
        "| Check | Unit | No-op | Comment | Body | What is counted | The finding it catches |".to_owned(),
        "|---|---|---|---|---|---|---|".to_owned(),
    ];
    lines.extend(BOUNDS.iter().map(|b| {
        format!(
            "| `{}` | {} | {} | {} | {} | {} | {} |",
            b.check,
            b.unit,
            cell(b.noop),
            cell(b.comment),
            cell(b.body),
            b.what,
            b.catches
        )
    }));
    lines.join("\n")
}

struct Args {
    command: String,
    result: String,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["command", "result"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        Ok(Args { command: f.one_of("command", &["held", "table"], None)?, result: f.str("result", Some(""))? })
    }
}

fn main_with(args: &Args) {
    if args.command == "table" {
        println!("{}", table());
        return;
    }
    if args.result.is_empty() {
        fail(SCRIPT, "result: command `held` reads the smoke's result there, and was given none");
    }
    let path = Path::new(&args.result);
    if !path.is_file() {
        println!(
            "{SCRIPT}: the budget smoke did not run: there is no result at {}. A gate run that did not run \
             it is red (docs/harness.md, \"The smoke's guards\")",
            path.display()
        );
        std::process::exit(1);
    }
    let Some(text) = read_text(path) else { fail(SCRIPT, &format!("{} is not read as UTF-8", path.display())) };
    let problems = match blaze_script::json::from_str::<Value>(&text) {
        Ok(result) => held(&result),
        Err(e) => vec![format!("the result at {} is no JSON: {e}", path.display())],
    };
    for problem in &problems {
        println!("{SCRIPT}: {problem}");
    }
    let all = names().len();
    println!("{SCRIPT}: {} of {all} guards ran and held", all as i64 - problems.len() as i64);
    std::process::exit(i32::from(!problems.is_empty()));
}

// The self-test: no blaze. Each finding in miniature, as the facts its build gave.

fn expect(holds: bool, what: &str) {
    blaze_script::expect(SCRIPT, holds, what);
}

fn refused(what: &str, text: &str) {
    if read_known(text).is_err() {
        return;
    }
    expect(false, &format!("{what} is refused"));
}

/// A warm build of a fixture as it should be: nothing dropped, nothing run that need not be.
fn fixed(step: &str) -> Facts {
    let facts: [(&str, Option<f64>); 12] = match step {
        "noop" => [
            ("ms", Some(40.0)),
            ("let_go", Some(0.0)),
            ("let_go_logged", Some(0.0)),
            ("ran_new", Some(0.0)),
            ("ran_untracked", Some(0.0)),
            ("executed", Some(0.0)),
            ("lower_ms", Some(0.0)),
            ("maintain_ms", Some(0.0)),
            ("answer_delay_ms", None),
            ("turn_uncovered_ms", Some(2.0)),
            ("uncovered_ms", Some(0.0)),
            ("tasks", Some(0.0)),
        ],
        "comment" => [
            ("ms", Some(80.0)),
            ("let_go", Some(0.0)),
            ("let_go_logged", Some(0.0)),
            ("ran_new", Some(0.0)),
            ("ran_untracked", Some(0.0)),
            ("executed", Some(0.0)),
            ("lower_ms", Some(0.0)),
            ("maintain_ms", Some(0.0)),
            ("answer_delay_ms", Some(1.5)),
            ("turn_uncovered_ms", Some(3.0)),
            ("uncovered_ms", Some(10.0)),
            ("tasks", Some(2.0)),
        ],
        _ => [
            ("ms", Some(300.0)),
            ("let_go", Some(0.0)),
            ("let_go_logged", Some(0.0)),
            ("ran_new", Some(20.0)),
            ("ran_untracked", Some(0.0)),
            ("executed", Some(90.0)),
            ("lower_ms", Some(40.0)),
            ("maintain_ms", Some(30.0)),
            ("answer_delay_ms", Some(4.0)),
            ("turn_uncovered_ms", Some(8.0)),
            ("uncovered_ms", Some(10.0)),
            ("tasks", Some(6.0)),
        ],
    };
    facts.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect()
}

/// The checks that fail (or are unmeasured) on `step` with `facts` in place of the fixed ones.
fn failing(step: &str, known: &[Known], facts: &[(&str, Option<f64>)]) -> BTreeSet<String> {
    let mut all = fixed(step);
    all.extend(facts.iter().map(|(k, v)| ((*k).to_owned(), *v)));
    judge("onefile", step, &all, known)
        .into_iter()
        .filter(|it| it.verdict != "pass")
        .map(|it| it.name.rsplit_once(": ").map_or(String::new(), |(_, check)| check.to_owned()))
        .collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|it| (*it).to_owned()).collect()
}

/// A set as Python prints one: `{'a', 'b'}`, or `set()`.
fn shown(items: &BTreeSet<String>) -> String {
    if items.is_empty() {
        return "set()".to_owned();
    }
    format!("{{{}}}", items.iter().map(|it| repr_str(it)).collect::<Vec<_>>().join(", "))
}

fn selftest() {
    let none: &[Known] = &[];
    for step in STEPS {
        let found = failing(step, none, &[]);
        expect(found.is_empty(), &format!("the fixed {step} passes every bound: {}", shown(&found)));
    }
    // Finding 1: the kept front end let go on every build, so a one-line edit lowered anew (25 s).
    let dropped = failing(
        "body",
        none,
        &[
            ("ms", Some(25_000.0)),
            ("let_go", Some(1.0)),
            ("let_go_logged", Some(1.0)),
            ("ran_new", Some(41_000.0)),
            ("lower_ms", Some(24_000.0)),
        ],
    );
    expect(
        dropped == set(&["ms", "let_go", "let_go_logged", "ran_new"]),
        &format!("a dropped front end on a body edit: {}", shown(&dropped)),
    );
    expect(
        failing("noop", none, &[("let_go_logged", Some(1.0))]).contains("let_go_logged")
            && failing("comment", none, &[("let_go", Some(2.0))]).contains("let_go"),
        "one front end let go on a warm build fails it, by the log and by the span",
    );
    expect(
        let_go_lines(
            "blaze serve: the kept front end of /w is let go, and its next build lowers anew: a task waits \
             for 0.03 GiB the process holds besides its tasks\nblaze serve: ready\n",
        ) == 1,
        "the daemon's line is counted",
    );
    // Finding 2: the answer held 4 s for a sync; a loop turn of 0.9 s with no span.
    expect(
        failing("body", none, &[("ms", Some(4300.0)), ("answer_delay_ms", Some(4000.0))])
            == set(&["ms", "answer_delay_ms"]),
        "an answer held for a sync",
    );
    expect(
        failing("comment", none, &[("answer_delay_ms", Some(4000.0))]) == set(&["answer_delay_ms"]),
        "the delay alone fails, whatever the wall",
    );
    expect(
        failing("body", none, &[("turn_uncovered_ms", Some(900.0))]) == set(&["turn_uncovered_ms"]),
        "a loop turn with 0.9 s no span covers",
    );
    expect(
        failing("noop", none, &[("answer_delay_ms", None), ("tasks", Some(0.0))]).is_empty(),
        "a build with no task has no delay to hold",
    );
    expect(
        failing("comment", none, &[("answer_delay_ms", None), ("tasks", Some(3.0))]) == set(&["answer_delay_ms"]),
        "a build with tasks and no reply span is unmeasured, never passed",
    );
    // Finding 4: an untracked query run 10,000 times a build; 200 ms lowering and 190 ms maintaining nothing.
    expect(
        failing("noop", none, &[("ran_untracked", Some(10_000.0)), ("executed", Some(10_000.0))])
            == set(&["ran_untracked", "executed"]),
        "untracked reruns",
    );
    expect(
        failing("comment", none, &[("lower_ms", Some(200.0)), ("maintain_ms", Some(190.0))])
            == set(&["lower_ms", "maintain_ms"]),
        "a comment edit's lowering",
    );
    expect(
        failing("body", none, &[("lower_ms", Some(200.0)), ("ran_untracked", Some(5.0))]).is_empty(),
        "a body edit's lowering is not held (yet)",
    );
    expect(
        failing("comment", none, &[("executed", None)]).contains("executed"),
        "a fact the profile did not give is unmeasured",
    );
    // The known list: a failure recorded passes as far as recorded, no further, and not once fixed.
    let known = vec![
        Known::new("onefile", "noop", "ran_untracked", 10_000.0, "identity_of is untracked"),
        Known::new("onefile", "comment", "lower_ms", 200.0, "lowering is not incremental"),
    ];
    expect(
        failing("noop", &known, &[("ran_untracked", Some(10_000.0)), ("executed", Some(0.0))]).is_empty(),
        "a known failure at its number passes",
    );
    expect(
        failing("noop", &known, &[("ran_untracked", Some(10_001.0))]) == set(&["ran_untracked"]),
        "a count that got worse fails",
    );
    expect(failing("comment", &known, &[("lower_ms", Some(290.0))]).is_empty(), "a known time has half again for noise");
    expect(failing("comment", &known, &[("lower_ms", Some(301.0))]) == set(&["lower_ms"]), "and fails past it");
    expect(
        failing("noop", &known, &[]) == set(&["ran_untracked"]),
        "a known failure that passes fails until it is taken out",
    );
    let mut judged: Vec<(&str, &str, Judged)> = Vec::new();
    for step in STEPS {
        let mut facts = fixed(step);
        if step == "noop" {
            facts.insert("ran_untracked".to_owned(), Some(9000.0));
        }
        judged.extend(judge("onefile", step, &facts, &known).into_iter().map(|it| ("onefile", step, it)));
    }
    let again = recorded_from(&judged, &known);
    let summary: Vec<(String, f64, String)> =
        again.iter().map(|it| (it.name(), it.measured, it.cause.clone())).collect();
    expect(
        summary
            == [(
                "guard: onefile: noop: ran_untracked".to_owned(),
                9000.0,
                "identity_of is untracked".to_owned(),
            )],
        &format!("a record keeps what fails with its new number and its cause, and drops what passes: {again:?}"),
    );
    let text = render_known(&again, &BTreeMap::from([("tree".to_owned(), "abc".to_owned())]));
    expect(read_known(&text).as_ref() == Ok(&again), "the list reads back as it was written");
    refused("an entry of no bound", &text.replace("\"ran_untracked\"", "\"ran_sideways\""));
    let first = text.split("[recorded]").next().unwrap_or("");
    let entry = first.split_once("\n\n").map_or("", |(_, rest)| rest);
    refused("an entry twice", &text.replace("[recorded]", &format!("{entry}[recorded]")));
    refused("an entry with no cause", &text.replace("\"identity_of is untracked\"", "\"\""));
    let known_file = Path::new(env!("CARGO_MANIFEST_DIR")).join("../harness/v2/guards-known.toml");
    let Some(listed_text) = read_text(&known_file) else {
        fail(SCRIPT, &format!("{} is not read", known_file.display()))
    };
    let listed = match read_known(&listed_text) {
        Ok(it) => it.iter().map(Known::name).collect::<Vec<_>>(),
        Err(e) => fail(SCRIPT, &e),
    };
    let mut sorted = listed.clone();
    sorted.sort();
    expect(listed == sorted, "harness/v2/guards-known.toml reads, its entries sorted as `render_known` writes them");
    // Nothing asked for is dropped.
    let wanted = names();
    let unique: BTreeSet<&String> = wanted.iter().collect();
    expect(unique.len() == wanted.len() && wanted.iter().any(|it| it == PROBE), "each outcome is named once");
    let bounded = BOUNDS.iter().map(|b| STEPS.iter().filter(|step| b.limit(step).is_some()).count()).sum::<usize>();
    expect(wanted.len() == 2 * bounded + 1, "every bound of every step of both fixtures is an outcome");
    let outcomes = |names: &[String], first_verdict: Option<&str>| -> Value {
        let items: Vec<Value> = names
            .iter()
            .enumerate()
            .map(|(i, it)| {
                let verdict = if i == 0 { first_verdict.unwrap_or("pass") } else { "pass" };
                serde_json::json!({"name": it, "verdict": verdict})
            })
            .collect();
        serde_json::json!({ "outcomes": items })
    };
    let full = outcomes(&wanted, None);
    expect(held(&full).is_empty(), "a result with every guard passed is held");
    let empty = held(&serde_json::json!({"outcomes": []}));
    expect(
        held(&outcomes(&wanted[1..], None)).len() == 1 && empty.first().is_some_and(|it| it.contains("did not run")),
        "a guard with no outcome is named: it did not run",
    );
    expect(held(&outcomes(&wanted, Some("fail"))).len() == 1, "a guard that failed is named");
    expect(
        !held(&serde_json::json!([])).is_empty() && !held(&serde_json::json!({})).is_empty(),
        "a result of another shape is refused",
    );
    // The edits apply to the fixtures, and move no line where they say so.
    let source = onefile_main();
    let edit = |it: Result<String, String>| it.unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e));
    let commented = edit(eol_comment(&source, ONEFILE_OPENING, 3));
    expect(
        commented.matches('\n').count() == source.matches('\n').count()
            && commented != edit(eol_comment(&source, ONEFILE_OPENING, 4))
            && splitlines(&commented).first().copied()
                == Some(format!("{ONEFILE_OPENING} // blaze guards: comment 3").as_str()),
        "the comment ends the line",
    );
    let stated = edit(body_statement(&source, ONEFILE_OPENING, 7));
    expect(
        splitlines(&stated).get(1).copied() == Some("    let _blaze_guard: u32 = 7;")
            && stated.matches('\n').count() == source.matches('\n').count() + 1,
        "the statement opens the body",
    );
    if eol_comment(&format!("{source}{source}"), ONEFILE_OPENING, 0).is_ok() {
        expect(false, "an opening that is there twice is refused");
    }
    expect(!facts_sql(12).contains('@') && facts_sql(12).contains(">= 12\n"), "the mark is written into the query");
    let tail = FACTS_SQL.split_once("SELECT w.build_id").map_or("", |(_, rest)| rest);
    let head = tail.split("FROM").next().unwrap_or("");
    let sql_columns: BTreeSet<String> = head
        .replace('\n', " ")
        .split(',')
        .map(|line| line.rsplit(" AS ").next().unwrap_or("").trim_matches([' ', ',']).to_owned())
        .collect();
    expect(
        PROFILE_FACTS.iter().all(|it| *it == "build_id" || sql_columns.contains(*it)),
        &format!("the query gives every fact read from it: {}", shown(&sql_columns)),
    );
    expect(
        BOUNDS.iter().all(|b| PROFILE_FACTS.contains(&b.check) || ["ms", "let_go_logged", "uncovered_ms"].contains(&b.check)),
        "every bound is of a fact the smoke gathers",
    );
    let numbers = one_row("{\"build_id\":3,\"executed\":0,\"answer_delay_ms\":null}\n", "a row").and_then(|one| {
        Ok((number(&one, "executed", "a row")?, number(&one, "answer_delay_ms", "a row")?))
    });
    expect(numbers == Ok((Some(0.0), None)), "a row's numbers");
    for bad in ["{\"executed\":\"many\"}", "{\"other\":1}"] {
        if one_row(bad, "a row").and_then(|row| number(&row, "executed", "a row")).is_ok() {
            expect(false, &format!("{bad} is refused"));
        }
    }
    expect(table().matches('\n').count() == BOUNDS.len() + 1, "the table has a line a bound");
    println!("{SCRIPT} selftest: ok");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [one] if one == "selftest" => selftest(),
        [one] => main_with(&input::parse::<Args>(one, SCRIPT)),
        _ => {
            eprintln!("usage: {SCRIPT} '<json>' | selftest\n\n{DOC}");
            std::process::exit(1);
        }
    }
}
