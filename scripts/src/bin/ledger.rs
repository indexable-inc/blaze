//! ledger: the objectives ledger. The port of `harness/v2/ledger.py`, deleted once both were run
//! on the same tree and agreed byte for byte (docs/v2/plan/design/scripts.md, "Ports and their
//! parity"): the same one JSON argument, `selftest`, text printed, exit status and bytes
//! written. `DOC` below is what its usage error prints; it says what the script does and what a
//! row is.
//!
//! `run` starts a check that is a bin of this package (`scripts/src/bin/<name>.rs`) with the
//! pinned blaze, `blaze run -q -p scripts --bin <name> --`, refused when `blaze --version` there
//! is not the release `.github/blaze-version` names (the stage-0 rule: a check runs on the
//! release, never on the tree under test); a Python one as the ledger names it, `uv run
//! --frozen python <script>`, until every check is ported.
//!
//! Where this differs from the Python, on purpose: the default `work` directory is
//! `/tmp/ledger-<pid>-<nanoseconds>`, not `mkdtemp`'s random name; the date is `date +%Y-%m-%d`'s.

/// What the usage error prints: harness/v2/ledger.py's docstring, with this script's commands.
const DOC: &str = r#"ledger.py: the objectives ledger (docs/v2/plan.md, "How the work is run"). The rows are data,
`docs/v2/plan/objectives.json`; this script checks them, runs their checks and writes what they
said, picks what to work on next, and writes the table people read, docs/v2/plan/objectives.md.

    blaze run -q -p scripts --bin ledger -- '{"command": "check"}'
    blaze run -q -p scripts --bin ledger -- '{"command": "write"}'
    blaze run -q -p scripts --bin ledger -- '{"command": "pick", "count": 4}'
    blaze run -q -p scripts --bin ledger -- '{"command": "run", "engine": {"kind": "v2", "blaze": "target/release/blaze"}, "ids": ["T5"]}'
    blaze run -q -p scripts --bin ledger -- selftest

The input is one JSON object:

    command   check   the rows are well formed and objectives.md is what `write` would write (CI)
              write   regenerate objectives.md from the rows
              pick    print up to `count` rows to work on now: not met, not blocked, everything
                      they need met, no two touching the same paths
              run     run the checks of the rows in `ids` (all rows that have one when empty; with
                      `only`, those in that state) and write each row's status
    root      the checkout (".")
    engine    `run`: the blaze measured (engine.EngineSpec), handed to each check
    work      `run`: where each check's result goes; a new directory under /tmp when empty
    ids, only, count   as above

A row of objectives.json:

    id          short and never reused (T5, D12)
    group       targets, correctness, design or app
    objective   the outcome, one sentence a check can decide
    checks      what decides it, all of which must pass. Each is a harness script that takes one
                JSON object and exits 0 when it holds: `script`, `input` (its fields but the ones
                below), `command` (its first argument, for a script that has several) and `takes`
                (which of `engine`, `blaze` and `out` this script is handed; engine and out).
                A bin of scripts/src/bin runs on the pinned blaze (.github/blaze-version), a
                Python script under `uv run --frozen python`
    missing     the check still to be written, in words; empty when `checks` decides the row. A
                row with something missing is never met: writing it is the first part of the work
    paths       the directories the work touches, so `pick` never hands out two that collide
    needs       ids that must be met first
    known       the last figure anyone measured, with its date: a lead, not a status
    status      written by `run`, except `blocked`, which a person or an agent writes with why:
                state (`?` not measured, `met`, `open`, `blocked`), commit, date, detail

`run` never trusts a tree it cannot name: the status carries the commit, with `+dirty` when the
checkout had changes. Exit status: `check` 1 on any problem; `run` 1 when a row it ran is not met.
"#;

use std::path::{Path, PathBuf};

use blaze_script::{
    Map, Value,
    engine::EngineSpec,
    fail,
    input::{self, Fields, Input, InputError},
    pyjson::{dumps, dumps_unicode},
    run, run_shown,
};

const SCRIPT: &str = "ledger.py";
const DATA: &str = "docs/v2/plan/objectives.json";
const TABLE: &str = "docs/v2/plan/objectives.md";
const STATES: [&str; 4] = ["?", "met", "open", "blocked"];
const GROUP_NAMES: [&str; 4] = ["targets", "correctness", "design", "app"];
const TAKES: [&str; 3] = ["engine", "blaze", "out"];
const GROUPS: [(&str, &str, &str); 4] = [
    ("targets", "Targets", "What a user feels, on one warm machine."),
    ("correctness", "Correctness", "What must hold for any of the speed to count."),
    (
        "design",
        "The design, part by part",
        "The parts of [the design](../plan.md#the-design) no check has shown yet.",
    ),
    (
        "app",
        "The agent app",
        "The app in `crates/harness/`, where people watch the work ([tasks](design/tasks.md)).",
    ),
];

fn owned(items: &[&str]) -> Vec<String> {
    items.iter().map(|it| (*it).to_owned()).collect()
}

#[derive(Debug, Clone, PartialEq)]
struct Check {
    script: String,
    input: Value,
    command: String,
    takes: Vec<String>,
}

impl Input for Check {
    const FIELDS: &'static [&'static str] = &["script", "input", "command", "takes"];

    fn parse(f: &Fields<'_>) -> Result<Check, InputError> {
        Ok(Check {
            script: f.str("script", None)?,
            input: f.any("input"),
            command: f.str("command", Some(""))?,
            takes: f.ones_of("takes", &TAKES, &["engine", "out"])?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Status {
    state: String,
    commit: String,
    date: String,
    detail: String,
}

impl Status {
    fn new(state: &str, commit: &str, date: &str, detail: &str) -> Status {
        Status { state: state.to_owned(), commit: commit.to_owned(), date: date.to_owned(), detail: detail.to_owned() }
    }
}

impl Input for Status {
    const FIELDS: &'static [&'static str] = &["state", "commit", "date", "detail"];

    fn parse(f: &Fields<'_>) -> Result<Status, InputError> {
        Ok(Status {
            state: f.one_of("state", &STATES, Some("?"))?,
            commit: f.str("commit", Some(""))?,
            date: f.str("date", Some(""))?,
            detail: f.str("detail", Some(""))?,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Row {
    id: String,
    group: String,
    objective: String,
    paths: Vec<String>,
    checks: Vec<Check>,
    missing: String,
    needs: Vec<String>,
    known: String,
    status: Status,
}

impl Input for Row {
    const FIELDS: &'static [&'static str] =
        &["id", "group", "objective", "paths", "checks", "missing", "needs", "known", "status"];

    fn parse(f: &Fields<'_>) -> Result<Row, InputError> {
        Ok(Row {
            id: f.str("id", None)?,
            group: f.one_of("group", &GROUP_NAMES, None)?,
            objective: f.str("objective", None)?,
            paths: f.strs_required("paths")?,
            checks: f.objects("checks", false)?,
            missing: f.str("missing", Some(""))?,
            needs: f.strs("needs")?,
            known: f.str("known", Some(""))?,
            status: match f.get("status") {
                None => Status::new("?", "", "", ""),
                Some(_) => f.object_required("status")?,
            },
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Ledger {
    rows: Vec<Row>,
    questions: Vec<String>,
}

impl Input for Ledger {
    const FIELDS: &'static [&'static str] = &["rows", "questions"];

    fn parse(f: &Fields<'_>) -> Result<Ledger, InputError> {
        Ok(Ledger { rows: f.objects("rows", true)?, questions: f.strs("questions")? })
    }
}

struct Args {
    command: String,
    root: String,
    engine: Option<EngineSpec>,
    work: String,
    ids: Vec<String>,
    only: Option<String>,
    count: i64,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["command", "root", "engine", "work", "ids", "only", "count"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        Ok(Args {
            command: f.one_of("command", &["check", "write", "pick", "run"], None)?,
            root: f.str("root", Some("."))?,
            engine: f.object("engine", false)?,
            work: f.str("work", Some(""))?,
            ids: f.strs("ids")?,
            only: f.one_of_or_null("only", &STATES)?,
            count: f.int("count", Some(4))?,
        })
    }
}

fn object(fields: Vec<(&str, Value)>) -> Value {
    Value::Object(fields.into_iter().map(|(key, value)| (key.to_owned(), value)).collect::<Map<String, Value>>())
}

fn strings(items: &[String]) -> Value {
    Value::Array(items.iter().map(|it| Value::String(it.clone())).collect())
}

/// Python's `asdict(row)`: every field, in the dataclass's order.
fn row_value(r: &Row) -> Value {
    let checks = r
        .checks
        .iter()
        .map(|c| {
            object(vec![
                ("script", Value::String(c.script.clone())),
                ("input", c.input.clone()),
                ("command", Value::String(c.command.clone())),
                ("takes", strings(&c.takes)),
            ])
        })
        .collect();
    object(vec![
        ("id", Value::String(r.id.clone())),
        ("group", Value::String(r.group.clone())),
        ("objective", Value::String(r.objective.clone())),
        ("paths", strings(&r.paths)),
        ("checks", Value::Array(checks)),
        ("missing", Value::String(r.missing.clone())),
        ("needs", strings(&r.needs)),
        ("known", Value::String(r.known.clone())),
        (
            "status",
            object(vec![
                ("state", Value::String(r.status.state.clone())),
                ("commit", Value::String(r.status.commit.clone())),
                ("date", Value::String(r.status.date.clone())),
                ("detail", Value::String(r.status.detail.clone())),
            ]),
        ),
    ])
}

/// One row a line, so a status change is a one-line diff.
fn dump(ledger: &Ledger) -> String {
    let rows: Vec<String> = ledger.rows.iter().map(|r| format!("    {}", dumps_unicode(&row_value(r)))).collect();
    let questions: Vec<String> =
        ledger.questions.iter().map(|q| format!("    {}", dumps_unicode(&Value::String(q.clone())))).collect();
    format!(
        "{{\n  \"rows\": [\n{}\n  ],\n  \"questions\": [\n{}\n  ]\n}}\n",
        rows.join(",\n"),
        questions.join(",\n")
    )
}

/// What is wrong with the rows; `root` also holds their scripts and paths to the tree.
fn problems(ledger: &Ledger, root: Option<&Path>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let ids: Vec<&str> = ledger.rows.iter().map(|r| r.id.as_str()).collect();
    let mut twice: Vec<&str> = ids.iter().copied().filter(|i| ids.iter().filter(|j| j == &i).count() > 1).collect();
    twice.sort_unstable();
    twice.dedup();
    out.extend(twice.iter().map(|i| format!("{i}: the id is used twice")));
    for r in &ledger.rows {
        if r.checks.is_empty() && r.missing.is_empty() {
            out.push(format!("{}: no check and nothing said to be missing: nothing can decide it", r.id));
        }
        if r.status.state == "met" && (!r.missing.is_empty() || r.status.commit.is_empty()) {
            out.push(format!("{}: met needs every check written and the commit it passed on", r.id));
        }
        if r.status.state == "blocked" && r.status.detail.is_empty() {
            out.push(format!("{}: blocked says by what, in status.detail", r.id));
        }
        if r.paths.is_empty() {
            out.push(format!("{}: no paths, so `pick` cannot tell what it collides with", r.id));
        }
        out.extend(
            r.needs.iter().filter(|n| !ids.contains(&n.as_str())).map(|n| format!("{}: needs {n}, which is no row", r.id)),
        );
        if let Some(root) = root {
            out.extend(
                r.checks
                    .iter()
                    .filter(|c| !root.join(&c.script).is_file())
                    .map(|c| format!("{}: {} does not exist", r.id, c.script)),
            );
            out.extend(
                r.paths
                    .iter()
                    .filter(|p| !root.join(p).exists())
                    .map(|p| format!("{}: the path {p} does not exist", r.id)),
            );
        }
    }
    out
}

/// A path's last part, as `Path(script).name`.
fn name_of(script: &str) -> &str {
    script.trim_end_matches('/').rsplit('/').next().unwrap_or(script)
}

fn shown(c: &Check) -> String {
    let none = c.input.is_null() || c.input.as_object().is_some_and(Map::is_empty);
    let given = if none { String::new() } else { format!(" {}", dumps_unicode(&c.input)) };
    let command = if c.command.is_empty() { String::new() } else { format!(" {}", c.command) };
    format!("`{}{command}{given}`", name_of(&c.script)).replace('|', "\\|")
}

fn table(ledger: &Ledger) -> String {
    let count = |state: &str| ledger.rows.iter().filter(|r| r.status.state == state).count();
    let mut lines: Vec<String> = owned(&[
        "---",
        "status: plan",
        "summary: The ledger. Every objective of the plan, the check that decides it and where it stands; \
         generated from objectives.json by scripts/src/bin/ledger.rs.",
        "date: 2026-11-09",
        "---",
        "# Objectives",
        "",
        "<!-- Generated by scripts/src/bin/ledger.rs from objectives.json: edit the rows there, then run `write`. -->",
        "",
        "One row per outcome, decided by its check on a named commit, never by a status someone",
        "remembered. How rows are picked, worked and landed is",
        "[How the work is run](../plan.md#how-the-work-is-run); the fields and the commands are in",
        "`scripts/src/bin/ledger.rs`. A check in *italics* is not written yet, and writing it is the first",
        "part of the work. `?` is not measured; \"last known\" is a lead, not a status.",
        "",
    ]);
    lines.push(format!(
        "{} objectives: {} met, {} open, {} blocked, {} not measured.",
        ledger.rows.len(),
        count("met"),
        count("open"),
        count("blocked"),
        count("?")
    ));
    lines.push(String::new());
    for (group, title, what) in GROUPS {
        lines.extend(owned(&[
            format!("## {title}").as_str(),
            "",
            what,
            "",
            "| id | objective | check | last known | status |",
            "|---|---|---|---|---|",
        ]));
        for r in ledger.rows.iter().filter(|r| r.group == group) {
            let mut checks: Vec<String> = r.checks.iter().map(shown).collect();
            if !r.missing.is_empty() {
                checks.push(format!("*write: {}*", r.missing));
            }
            let s = &r.status;
            let commit: String = s.commit.chars().take(10).collect();
            let status = format!(
                "{}{}{}",
                s.state,
                if s.commit.is_empty() { String::new() } else { format!(" `{commit}`") },
                if s.detail.is_empty() { String::new() } else { format!(": {}", s.detail) }
            );
            let cells = [r.id.clone(), r.objective.clone(), checks.join("; "), r.known.clone(), status];
            let cells: Vec<String> = cells.iter().map(|c| c.replace('\n', " ")).collect();
            lines.push(format!("| {} |", cells.join(" | ")));
        }
        lines.push(String::new());
    }
    lines.extend(owned(&[
        "## Open design questions",
        "",
        "Decide with evidence, fold the answer into the plan, delete the question.",
        "",
    ]));
    lines.extend(ledger.questions.iter().map(|q| format!("- {q}")));
    format!("{}\n", lines.join("\n"))
}

fn collide(a: &Row, b: &Row) -> bool {
    a.paths.iter().any(|p| {
        b.paths.iter().any(|q| {
            p == q
                || p.starts_with(&format!("{}/", q.trim_end_matches('/')))
                || q.starts_with(&format!("{}/", p.trim_end_matches('/')))
        })
    })
}

fn pick(ledger: &Ledger, count: i64) -> Vec<&Row> {
    let met: Vec<&str> = ledger.rows.iter().filter(|r| r.status.state == "met").map(|r| r.id.as_str()).collect();
    let mut picked: Vec<&Row> = Vec::new();
    for r in &ledger.rows {
        let ready = matches!(r.status.state.as_str(), "?" | "open") && r.needs.iter().all(|n| met.contains(&n.as_str()));
        let room = i64::try_from(picked.len()).unwrap_or(i64::MAX) < count;
        if ready && room && !picked.iter().any(|p| collide(r, p)) {
            picked.push(r);
        }
    }
    picked
}

/// Python's `text[:300]`: the first 300 characters.
fn first_300(text: &str) -> String {
    text.chars().take(300).collect()
}

/// A row's status from what each of its checks exited with and said.
fn judged(row: &Row, codes: &[(&Check, i32, String)], commit: &str, date: &str) -> Status {
    let failed: Vec<String> = codes
        .iter()
        .filter(|(_, code, _)| *code != 0)
        .map(|(c, _, said)| format!("{}: {said}", name_of(&c.script)))
        .collect();
    if !failed.is_empty() {
        return Status::new("open", commit, date, &first_300(&failed.join("; ")));
    }
    if !row.missing.is_empty() {
        return Status::new(
            "open",
            commit,
            date,
            &first_300(&format!("its checks pass; still to write: {}", row.missing)),
        );
    }
    Status::new("met", commit, date, "")
}

/// A JSON value inside an f-string: a string as it is, anything else as Python's `str` of it
/// for the values a result holds.
fn py_str(value: &Value) -> String {
    match value {
        Value::String(it) => it.clone(),
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        other => dumps(other),
    }
}

/// Python's truth of a JSON value.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(it) => *it,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(it) => !it.is_empty(),
        Value::Array(it) => !it.is_empty(),
        Value::Object(it) => !it.is_empty(),
    }
}

/// Why a check failed, from the result it wrote (report.py's shape), else its exit status.
fn said(out: &Path, code: i32) -> String {
    let read = || -> Option<String> {
        let result: Value = blaze_script::json::from_str(&std::fs::read_to_string(out).ok()?).ok()?;
        let outcomes = result.as_object()?.get("outcomes")?.as_array()?;
        let mut bad = Vec::new();
        for o in outcomes {
            let o = o.as_object()?;
            if o.get("verdict")?.as_str() == Some("pass") {
                continue;
            }
            let detail = match o.get("detail") {
                Some(detail) if truthy(detail) => format!(" ({})", py_str(detail)),
                _ => String::new(),
            };
            bad.push(format!("{} {}{detail}", py_str(o.get("name")?), py_str(o.get("verdict")?)));
        }
        Some(if bad.is_empty() { format!("exit {code}") } else { bad.join("; ") })
    };
    read().unwrap_or_else(|| format!("exit {code} and no result read"))
}

/// The bin of this package a check's `script` is (`scripts/src/bin/<name>.rs`), else `None`: a
/// Python script.
fn rust_bin(script: &str) -> Option<&str> {
    let name = script.strip_prefix("scripts/src/bin/")?.strip_suffix(".rs")?;
    (!name.is_empty() && !name.contains('/')).then_some(name)
}

/// How a check is started: a bin of this package with the pinned blaze, a Python script under
/// uv. `given` is its JSON input; the command, when the check has one, comes before it.
fn argv_of<'c>(c: &'c Check, given: &'c str) -> Vec<&'c str> {
    let mut argv: Vec<&str> = match rust_bin(&c.script) {
        Some(name) => vec!["blaze", "run", "-q", "-p", "scripts", "--bin", name, "--"],
        None => vec!["uv", "run", "--frozen", "python", c.script.as_str()],
    };
    if !c.command.is_empty() {
        argv.push(c.command.as_str());
    }
    argv.push(given);
    argv
}

/// The line `run` prints as a check starts: its script and its first argument, as the Python
/// printed `argv[4:6]`.
fn starting(id: &str, c: &Check, given: &str) -> String {
    let first = if c.command.is_empty() { given } else { c.command.as_str() };
    format!("{id}: {} {first} ...", c.script)
}

/// The release a bin of this package runs on (the stage-0 rule, docs/v2/plan/design/scripts.md):
/// `blaze --version` in `root` says the version `.github/blaze-version` names, or this is why not.
fn pinned_blaze(root: &Path) -> Result<(), String> {
    let pin = std::fs::read_to_string(root.join(".github/blaze-version"))
        .map_err(|e| format!(".github/blaze-version is not read: {e}"))?;
    let pin = pin.trim();
    let ran = run(&["blaze", "--version"], root)?;
    let said = ran.out();
    let version = said.lines().next().and_then(|line| line.split_whitespace().nth(1)).unwrap_or("");
    if ran.status == 0 && version == pin {
        return Ok(());
    }
    Err(format!(
        "a check of scripts/ runs on the pinned blaze {pin} (.github/blaze-version), and `blaze --version` in {} says {:?} (exit {}): install it with `sh site/install.sh --version {pin}`",
        root.display(),
        said.lines().next().unwrap_or("").trim(),
        ran.status
    ))
}

/// `git -C root <args>`: what it printed, trimmed; empty when it failed to say anything.
fn git_said(root: &Path, args: &[&str]) -> String {
    let mut argv = vec!["git", "-C", root.to_str().unwrap_or(".")];
    argv.extend_from_slice(args);
    run(&argv, root).map(|ran| ran.out().trim().to_owned()).unwrap_or_default()
}

/// The ledger after the chosen rows' checks ran, and which rows those were.
fn run_checks(it: &Args, root: &Path, ledger: &Ledger) -> (Ledger, Vec<usize>) {
    let Some(engine) = &it.engine else { fail(SCRIPT, "run needs `engine`") };
    let dirty = !git_said(root, &["status", "--porcelain", "--untracked-files=no"]).is_empty();
    let commit = format!("{}{}", git_said(root, &["rev-parse", "HEAD"]), if dirty { "+dirty" } else { "" });
    let work = if it.work.is_empty() {
        // tempfile.mkdtemp(prefix="ledger-", dir="/tmp"): a new directory of this run's own.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.subsec_nanos());
        PathBuf::from(format!("/tmp/ledger-{}-{nanos}", std::process::id()))
    } else {
        PathBuf::from(&it.work)
    };
    if let Err(e) = std::fs::create_dir_all(&work) {
        fail(SCRIPT, &format!("{} is not made: {e}", work.display()));
    }
    let mut unknown: Vec<&str> =
        it.ids.iter().map(String::as_str).filter(|id| !ledger.rows.iter().any(|r| r.id == *id)).collect();
    unknown.sort_unstable();
    unknown.dedup();
    if !unknown.is_empty() {
        fail(SCRIPT, &format!("no row {}", unknown.join(", ")));
    }
    let today = match run(&["date", "+%Y-%m-%d"], root) {
        Ok(ran) if ran.status == 0 => ran.out().trim().to_owned(),
        _ => fail(SCRIPT, "`date +%Y-%m-%d` did not run"),
    };
    let engine_value = engine.to_value();
    let mut rows: Vec<Row> = Vec::new();
    let mut touched: Vec<usize> = Vec::new();
    let mut pinned = false;
    for (at, r) in ledger.rows.iter().enumerate() {
        let chosen = !r.checks.is_empty()
            && (it.ids.is_empty() || it.ids.contains(&r.id))
            && it.only.as_ref().is_none_or(|only| *only == r.status.state);
        if !chosen {
            rows.push(r.clone());
            continue;
        }
        let mut codes: Vec<(&Check, i32, String)> = Vec::new();
        for (n, c) in r.checks.iter().enumerate() {
            let out = work.join(format!("{}-{n}.json", r.id));
            let mut given: Map<String, Value> = c.input.as_object().cloned().unwrap_or_default();
            if c.takes.iter().any(|it| it == "engine") {
                given.insert("engine".to_owned(), engine_value.clone());
            }
            if c.takes.iter().any(|it| it == "blaze") {
                given.insert("blaze".to_owned(), Value::String(engine.blaze.clone()));
            }
            if c.takes.iter().any(|it| it == "out") {
                given.insert("out".to_owned(), Value::String(out.to_string_lossy().into_owned()));
            }
            let given = dumps(&Value::Object(given));
            if rust_bin(&c.script).is_some() && !pinned {
                if let Err(why) = pinned_blaze(root) {
                    fail(SCRIPT, &why);
                }
                pinned = true;
            }
            println!("{}", starting(&r.id, c, &given));
            let code = match run_shown(&argv_of(c, &given), root) {
                Ok(code) => code,
                Err(e) => fail(SCRIPT, &e),
            };
            codes.push((c, code, if code == 0 { String::new() } else { said(&out, code) }));
        }
        let status = judged(r, &codes, &commit, &today);
        let detail = if status.detail.is_empty() { String::new() } else { format!(": {}", status.detail) };
        println!("{}: {}{detail}", r.id, status.state);
        rows.push(Row { status, ..r.clone() });
        touched.push(at);
    }
    (Ledger { rows, questions: ledger.questions.clone() }, touched)
}

fn read_or_fail(path: &Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => fail(SCRIPT, &format!("{} is not read: {e}", path.display())),
    }
}

fn write_or_fail(path: &Path, text: &str) {
    if let Err(e) = std::fs::write(path, text) {
        fail(SCRIPT, &format!("{} is not written: {e}", path.display()));
    }
}

fn main_with(it: &Args) {
    // `Path(root).resolve()`: a root that is not there is found out when the rows are read.
    let root = std::fs::canonicalize(&it.root)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(&it.root));
    let ledger: Ledger = input::parse(&root.join(DATA).to_string_lossy(), SCRIPT);
    let mut wrong = problems(&ledger, Some(&root));
    if it.command == "check" {
        if read_or_fail(&root.join(DATA)) != dump(&ledger) {
            wrong.push(format!("{DATA} is not as `write` writes it"));
        }
        if read_or_fail(&root.join(TABLE)) != table(&ledger) {
            wrong.push(format!("{TABLE} is stale: run ledger with command `write`"));
        }
    }
    if !wrong.is_empty() {
        let lines: Vec<String> = wrong.iter().map(|w| format!("{SCRIPT}: {w}")).collect();
        eprintln!("{}", lines.join("\n"));
        std::process::exit(1);
    }
    if it.command == "pick" {
        for r in pick(&ledger, it.count) {
            let first = if r.missing.is_empty() {
                "run its check, then close the gap".to_owned()
            } else {
                format!("write its check ({})", r.missing)
            };
            println!("{}  {}\n    first: {first}\n    paths: {}", r.id, r.objective, r.paths.join(", "));
        }
    }
    let (ran, touched) = match it.command.as_str() {
        "run" => run_checks(it, &root, &ledger),
        _ => (ledger, Vec::new()),
    };
    if matches!(it.command.as_str(), "write" | "run") {
        write_or_fail(&root.join(DATA), &dump(&ran));
        write_or_fail(&root.join(TABLE), &table(&ran));
    }
    if it.command == "run" {
        let unmet = touched.iter().any(|at| ran.rows[*at].status.state != "met");
        std::process::exit(i32::from(unmet));
    }
}

fn expect(holds: bool, what: &str) {
    blaze_script::expect(SCRIPT, holds, what);
}

fn selftest() {
    let check = Check {
        script: "harness/v2/budgets.py".to_owned(),
        input: serde_json::json!({"budgets": ["noop"]}),
        command: String::new(),
        takes: owned(&["engine", "out"]),
    };
    let row = |id: &str, group: &str, paths: &[&str], checks: Vec<Check>, missing: &str, needs: &[&str]| Row {
        id: id.to_owned(),
        group: group.to_owned(),
        objective: id.to_lowercase(),
        paths: owned(paths),
        checks,
        missing: missing.to_owned(),
        needs: owned(needs),
        known: String::new(),
        status: Status::new("?", "", "", ""),
    };
    let a = row("A", "targets", &["crates/x"], vec![check.clone()], "", &[]);
    let b = row("B", "design", &["crates/x/src"], vec![], "a lint", &["A"]);
    let c = row("C", "design", &["crates/y"], vec![check.clone()], "an oracle", &[]);
    let ledger = Ledger { rows: vec![a.clone(), b.clone(), c.clone()], questions: owned(&["q?"]) };
    expect(
        problems(&ledger, None).is_empty(),
        &format!("a sound ledger has no problems: {:?}", problems(&ledger, None)),
    );
    let bad = Ledger {
        rows: vec![
            a.clone(),
            Row { checks: vec![], ..a.clone() },
            Row {
                id: "Z".to_owned(),
                needs: owned(&["N"]),
                paths: vec![],
                status: Status::new("met", "abc", "", ""),
                ..c.clone()
            },
        ],
        questions: vec![],
    };
    let found = problems(&bad, None);
    for what in ["A: the id is used twice", "nothing can decide it", "Z: needs N", "Z: no paths", "Z: met needs"] {
        expect(found.iter().any(|f| f.contains(what)), &format!("`{what}` is reported, of {found:?}"));
    }
    expect(collide(&a, &b) && !collide(&a, &c), "a path under another collides; siblings do not");
    let ids = |rows: Vec<&Row>| -> Vec<String> { rows.iter().map(|r| r.id.clone()).collect() };
    expect(ids(pick(&ledger, 4)) == ["A", "C"], "B waits for A and collides with it");
    let met = Ledger {
        rows: vec![Row { status: Status::new("met", "abc", "2026-11-09", ""), ..a.clone() }, b.clone(), c.clone()],
        questions: ledger.questions.clone(),
    };
    expect(ids(pick(&met, 4)) == ["B", "C"], "once A is met, B is ready and A is not picked");
    expect(ids(pick(&met, 1)) == ["B"], "count bounds the pick");
    expect(
        judged(&a, &[(&check, 0, String::new())], "abc", "d") == Status::new("met", "abc", "d", ""),
        "every check passing is met",
    );
    expect(
        judged(&a, &[(&check, 1, "noop fail".to_owned())], "abc", "d").state == "open",
        "a failing check is open",
    );
    let still = judged(&c, &[(&check, 0, String::new())], "abc", "d");
    expect(
        still.state == "open" && still.detail.contains("an oracle"),
        "a row with a check to write is never met",
    );
    let text = dump(&met);
    let back: Ledger = input::parse(&text, SCRIPT);
    expect(back == met && dump(&back) == text, "the rows round-trip byte for byte");
    let page = table(&met);
    expect(
        page.contains("| A | a | `budgets.py {\"budgets\": [\"noop\"]}` |  | met `abc` |"),
        &format!("A's line: {page}"),
    );
    expect(
        page.contains("*write: a lint*") && page.contains("3 objectives: 1 met, 0 open, 0 blocked, 2 not measured."),
        "the page",
    );
    let tmp = std::env::temp_dir().join(format!("ledger-selftest-{}", std::process::id()));
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        fail(SCRIPT, &format!("{} is not made: {e}", tmp.display()));
    }
    let out = tmp.join("r.json");
    write_or_fail(
        &out,
        r#"{"outcomes": [{"name": "noop", "verdict": "fail", "detail": "11 ms over 5"}, {"name": "x", "verdict": "pass"}]}"#,
    );
    let words = said(&out, 1);
    let none = said(&tmp.join("none.json"), 2);
    // Removed before the checks, so a failing one leaves nothing behind.
    let _ = std::fs::remove_dir_all(&tmp);
    expect(words == "noop fail (11 ms over 5)", &format!("a failure's words: {words}"));
    expect(none == "exit 2 and no result read", "no result is said so");
    let docs = Check {
        script: "scripts/src/bin/docs.rs".to_owned(),
        input: serde_json::json!({"left": 0}),
        command: "python".to_owned(),
        takes: vec![],
    };
    expect(
        rust_bin("scripts/src/bin/docs.rs") == Some("docs")
            && rust_bin("harness/v2/budgets.py").is_none()
            && rust_bin("scripts/src/bin/a/b.rs").is_none()
            && rust_bin("scripts/src/bin/.rs").is_none(),
        "a bin of scripts/src/bin is told from a Python script",
    );
    expect(
        argv_of(&docs, "{}") == ["blaze", "run", "-q", "-p", "scripts", "--bin", "docs", "--", "python", "{}"],
        "a bin runs on blaze, its command before its input",
    );
    expect(
        argv_of(&check, "{}") == ["uv", "run", "--frozen", "python", "harness/v2/budgets.py", "{}"],
        "a Python check runs under uv",
    );
    expect(
        starting("D43", &docs, "{}") == "D43: scripts/src/bin/docs.rs python ..."
            && starting("A", &check, "{}") == "A: harness/v2/budgets.py {} ...",
        "the line a check starts with is its script and its first argument",
    );
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
