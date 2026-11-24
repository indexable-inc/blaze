//! gate: runs tools/gate.txt, the one list of the tree's checks, for `just lint` and for
//! blaze.yaml's `scripts` job alike, after checking that neither runs a check outside it. `DOC`
//! below is what its usage error prints.
//!
//!     blaze run -q -p scripts --bin gate -- '{"command": "run", "scripts": "target/debug"}'
//!     blaze run -q -p scripts --bin gate -- '{"command": "check"}'
//!     blaze run -q -p scripts --bin gate -- selftest
//!
//! Why (2026-11-20): the justfile's `lint` and CI's steps were two lists, and a gate that runs
//! different things in two places is two gates. Now both name this runner and nothing else; the
//! check below refuses a check added beside the list in either.

/// What the usage error prints.
const DOC: &str = r#"gate: runs tools/gate.txt, the one list of the tree's checks.

    blaze run -q -p scripts --bin gate -- '{"command": "run", "scripts": "<dir>", "base": "<sha>"}'
    blaze run -q -p scripts --bin gate -- '{"command": "check"}'
    blaze run -q -p scripts --bin gate -- selftest

    command   run    `check`, then every row of tools/gate.txt in order, each under its timeout:
                     one line a row (`ok` or `FAIL`, its name, its seconds; a failure's last lines
                     after it); exits 1 naming each row that failed
              check  the list parses, the justfile's `lint` recipe runs this runner and no other
                     check, and blaze.yaml's `scripts` job runs this runner and no other check
                     but the named CI-only steps (`CI_ONLY_STEPS`), and no other job calls a
                     script but the named CI-only ones (`CI_ONLY_SCRIPTS`)
    scripts   the built scripts' directory, `$BLAZE_SCRIPTS` in each row (required by `run`)
    base      a commit (hex): added as `"base"` to each `base` row's JSON (null)
    root      the checkout (".")
"#;

use std::{
    fmt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use blaze_script::{
    fail,
    input::{self, Fields, Input, InputError},
    read_text, run_within,
};

const SCRIPT: &str = "gate";
/// The list, from the root.
const LIST: &str = "tools/gate.txt";
const JUSTFILE: &str = "justfile";
const WORKFLOW: &str = ".github/workflows/blaze.yaml";
/// The job of the workflow that runs the list.
const JOB: &str = "scripts";
/// What a row's separator is: ` | ` (a command may hold more; only the first three split).
const SEP: &str = " | ";
/// The longest a row may run (rule 69: sized to the row, never unbounded).
const MOST_SECONDS: u64 = 3600;
/// How many of a failure's last lines are printed under it.
const TAIL: usize = 12;

/// The steps of the `scripts` job that run outside the list, each with why it cannot be a row.
const CI_ONLY_STEPS: [(&str, &str); 0] = [];

/// The scripts another job of the workflow calls, each with why it is not a row.
const CI_ONLY_SCRIPTS: [(&str, &str); 5] = [
    ("gate", "the runner itself, called by the `scripts` job's one gate step"),
    ("lean_gate", "the `lean` job: builds Lean packages and verifies `#print axioms`"),
    ("toolchain_cut", "the `cargo` job: cuts the pinned toolchain shelf before running workspace tests"),
    ("ci_test", "the `cargo` job: runs workspace tests in parallel with bounded timeouts"),
    ("sys_imports", "the `cargo` job: checks blaze-sys object imports across targets"),
];

/// Why the list, the justfile or the workflow is refused (rule 19: each names the file and line).
#[derive(Debug, Clone, PartialEq, Eq)]
enum GateError {
    /// A file could not be read.
    Unread { path: &'static str },
    /// A row that is not `name | seconds | base | command`.
    Row { line: usize, what: &'static str },
    /// Two rows of one name.
    TwoRows { line: usize, name: String },
    /// The list has no row.
    Empty,
    /// The base is not a commit's hex name, so it cannot be put in a command.
    Base { base: String },
    /// The justfile has no `lint` recipe.
    NoLint,
    /// The `lint` recipe runs something beside the runner.
    Justfile { line: usize, what: &'static str, text: String },
    /// The workflow has no `scripts` job.
    NoJob,
    /// A step of the `scripts` job runs something beside the runner.
    Step { line: usize, what: &'static str, text: String },
    /// A job calls a script that is neither the runner nor a named CI-only one.
    Script { line: usize, script: String },
    /// The `scripts` job runs the runner in no step, or in two.
    GateSteps { count: usize },
}

impl fmt::Display for GateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GateError::Unread { path } => write!(f, "{path} could not be read"),
            GateError::Row { line, what } => write!(f, "{LIST}:{line}: {what} (a row is `name | seconds | base | command`)"),
            GateError::TwoRows { line, name } => write!(f, "{LIST}:{line}: a second row named {name:?}"),
            GateError::Empty => write!(f, "{LIST} has no row"),
            GateError::Base { base } => write!(f, "base {base:?} is not a commit's hex name"),
            GateError::NoLint => write!(f, "{JUSTFILE} has no `lint:` recipe"),
            GateError::Justfile { line, what, text } => write!(f, "{JUSTFILE}:{line}: the `lint` recipe {what}: {text}; make it a row of {LIST}"),
            GateError::NoJob => write!(f, "{WORKFLOW} has no `{JOB}:` job"),
            GateError::Step { line, what, text } => {
                write!(f, "{WORKFLOW}:{line}: a step of the `{JOB}` job {what}: {text}; make it a row of {LIST}, or a row of CI_ONLY_STEPS (scripts/src/bin/gate.rs) saying why it cannot be one")
            }
            GateError::Script { line, script } => {
                write!(f, "{WORKFLOW}:{line}: calls the script {script:?} outside the gate; make it a row of {LIST}, or a row of CI_ONLY_SCRIPTS (scripts/src/bin/gate.rs) saying why it cannot be one")
            }
            GateError::GateSteps { count } => write!(f, "{WORKFLOW}: the `{JOB}` job runs the gate in {count} steps, not one"),
        }
    }
}

/// One check of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    name: String,
    seconds: u64,
    /// Whether a run's base is added to its JSON.
    base: bool,
    command: String,
    line: usize,
}

/// The rows of the list's `text`, in order.
fn parse_list(text: &str) -> Result<Vec<Row>, GateError> {
    let mut rows: Vec<Row> = Vec::new();
    for (at, raw) in text.lines().enumerate() {
        let line = at + 1;
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut cols = raw.splitn(4, SEP);
        let (Some(name), Some(seconds), Some(base), Some(command)) = (cols.next(), cols.next(), cols.next(), cols.next()) else {
            return Err(GateError::Row { line, what: "fewer than four columns" });
        };
        let (name, command) = (name.trim(), command.trim());
        if name.is_empty() || command.is_empty() {
            return Err(GateError::Row { line, what: "an empty name or command" });
        }
        let seconds: u64 = seconds.trim().parse().map_err(|_| GateError::Row { line, what: "seconds that are no whole number" })?;
        if seconds == 0 || seconds > MOST_SECONDS {
            return Err(GateError::Row { line, what: "seconds not within 1 to 3600" });
        }
        let base = match base.trim() {
            "base" => true,
            "-" => false,
            _ => return Err(GateError::Row { line, what: "a base column that is neither `base` nor `-`" }),
        };
        if base && !command.ends_with("}'") {
            return Err(GateError::Row { line, what: "a `base` row whose last word is no JSON object in single quotes" });
        }
        if rows.iter().any(|it| it.name == name) {
            return Err(GateError::TwoRows { line, name: name.to_owned() });
        }
        rows.push(Row { name: name.to_owned(), seconds, base, command: command.to_owned(), line });
    }
    if rows.is_empty() {
        return Err(GateError::Empty);
    }
    Ok(rows)
}

/// `row`'s command as run: `$BLAZE_SCRIPTS` set to `scripts`, and `base` added to its JSON.
fn command_of(row: &Row, scripts: &str, base: Option<&str>) -> Result<String, GateError> {
    let mut command = row.command.clone();
    if let (true, Some(base)) = (row.base, base) {
        if base.len() < 7 || base.len() > 64 || !base.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(GateError::Base { base: base.to_owned() });
        }
        command.truncate(command.len() - 2);
        command.push_str(&format!(", \"base\": \"{base}\"}}'"));
    }
    // The directory in single quotes, each `'` closed, escaped and reopened.
    let quoted = scripts.replace('\'', r"'\''");
    Ok(format!("BLAZE_SCRIPTS='{quoted}'; export BLAZE_SCRIPTS; {command}"))
}

/// Whether `text` starts a check: a script, Python, bun or cargo.
fn runs_a_check(text: &str) -> Option<&'static str> {
    if text.contains("BLAZE_SCRIPTS/") || text.contains("$bin/") {
        return Some("calls a script");
    }
    if text.contains("uv run") || text.contains("python") {
        return Some("runs Python");
    }
    if text.split_whitespace().any(|word| word == "bun" || word == "bunx") {
        return Some("runs bun");
    }
    if text.split_whitespace().any(|word| word == "cargo") {
        return Some("runs cargo");
    }
    None
}

/// Whether `text` is a call of the runner's `run`.
fn is_gate_run(text: &str) -> bool {
    (text.contains("$bin/gate\"") || text.contains("$BLAZE_SCRIPTS/gate\"")) && text.contains(r#"\"command\": \"run\""#)
}

/// The `lint` recipe of the justfile's `text` runs the runner once and no check of its own.
fn check_justfile(text: &str) -> Result<(), GateError> {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.iter().position(|line| line.starts_with("lint:") || line.starts_with("lint ")).ok_or(GateError::NoLint)?;
    let mut runs = 0;
    for (at, line) in lines.iter().enumerate().skip(start + 1) {
        // The recipe ends at the next line in column 0 that is not a comment.
        if !line.is_empty() && !line.starts_with([' ', '\t']) && !line.starts_with('#') {
            break;
        }
        let body = line.trim();
        if body.starts_with('#') {
            continue;
        }
        if is_gate_run(body) {
            runs += 1;
            continue;
        }
        if let Some(what) = runs_a_check(body) {
            return Err(GateError::Justfile { line: at + 1, what, text: body.to_owned() });
        }
    }
    match runs {
        1 => Ok(()),
        _ => Err(GateError::Justfile { line: start + 1, what: "does not run the gate exactly once", text: format!("{runs} calls of `$bin/gate` with `run`") }),
    }
}

/// A step of a job: its first line, its name and its run text (empty for a `uses:` step).
struct Step {
    line: usize,
    name: Option<String>,
    run: String,
}

/// The indentation of `line`.
fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The value of `key:` on `line` (after an optional `- `), when the line holds it.
fn key_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let body = line.trim_start();
    let body = body.strip_prefix("- ").unwrap_or(body);
    body.strip_prefix(key)?.strip_prefix(':').map(str::trim)
}

/// The steps of job `job` of the workflow's `lines`: its lines from its `  job:` to the next job.
fn steps_of(lines: &[&str], job: &str) -> Result<Vec<Step>, GateError> {
    let head = format!("  {job}:");
    let start = lines.iter().position(|line| line.trim_end() == head).ok_or(GateError::NoJob)?;
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, line)| indent(line) == 2 && !line.trim_start().starts_with('#') && line.trim_end().ends_with(':'))
        .map_or(lines.len(), |(at, _)| at);
    let mut steps: Vec<Step> = Vec::new();
    let mut at = start + 1;
    while at < end {
        let line = lines[at];
        if indent(line) == 6 && line.trim_start().starts_with("- ") {
            steps.push(Step { line: at + 1, name: None, run: String::new() });
        }
        let Some(step) = steps.last_mut() else {
            at += 1;
            continue;
        };
        if indent(line) <= 8 && !line.trim_start().starts_with('#') {
            if let Some(name) = key_value(line, "name") {
                step.name = Some(name.trim_matches(['\'', '"']).to_owned());
            }
            if let Some(value) = key_value(line, "run") {
                let key_indent = indent(line) + if line.trim_start().starts_with("- ") { 2 } else { 0 };
                if value == "|" || value == ">" {
                    let mut body = Vec::new();
                    at += 1;
                    while at < end && (lines[at].trim().is_empty() || indent(lines[at]) > key_indent) {
                        body.push(lines[at].trim());
                        at += 1;
                    }
                    step.run = body.join("\n");
                    continue;
                }
                step.run = match value.strip_prefix('\'').and_then(|it| it.strip_suffix('\'')) {
                    Some(quoted) => quoted.replace("''", "'"),
                    None => value.to_owned(),
                };
            }
        }
        at += 1;
    }
    Ok(steps)
}

/// The workflow's `text`: the `scripts` job runs the runner in one step and nothing else but the
/// named CI-only steps; no job calls another script but the named CI-only ones.
fn check_workflow(text: &str) -> Result<(), GateError> {
    let lines: Vec<&str> = text.lines().collect();
    let mut gate_steps = 0;
    for step in steps_of(&lines, JOB)? {
        if step.run.is_empty() {
            continue;
        }
        if is_gate_run(&step.run) {
            gate_steps += 1;
            continue;
        }
        if step.name.as_deref().is_some_and(|name| CI_ONLY_STEPS.iter().any(|(it, _)| *it == name)) {
            continue;
        }
        let first = step.run.lines().next().unwrap_or_default().to_owned();
        let what = runs_a_check(&step.run).unwrap_or("runs a command");
        return Err(GateError::Step { line: step.line, what, text: first });
    }
    if gate_steps != 1 {
        return Err(GateError::GateSteps { count: gate_steps });
    }
    for (at, line) in lines.iter().enumerate() {
        let mut rest = *line;
        while let Some(found) = rest.find("$BLAZE_SCRIPTS/") {
            rest = &rest[found + "$BLAZE_SCRIPTS/".len()..];
            let script: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
            if !CI_ONLY_SCRIPTS.iter().any(|(it, _)| *it == script) {
                return Err(GateError::Script { line: at + 1, script });
            }
        }
    }
    Ok(())
}

/// Reads `path` under `root`.
fn read(root: &Path, path: &'static str) -> Result<String, GateError> {
    read_text(&root.join(path)).ok_or(GateError::Unread { path })
}

/// The list, after the justfile and the workflow are checked against it.
fn checked(root: &Path) -> Result<Vec<Row>, GateError> {
    let rows = parse_list(&read(root, LIST)?)?;
    check_justfile(&read(root, JUSTFILE)?)?;
    check_workflow(&read(root, WORKFLOW)?)?;
    Ok(rows)
}

struct Args {
    command: String,
    scripts: Option<String>,
    base: Option<String>,
    root: String,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["command", "scripts", "base", "root"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        Ok(Args {
            command: f.one_of("command", &["run", "check"], None)?,
            scripts: f.str_or_null("scripts")?,
            base: f.str_or_null("base")?,
            root: f.str("root", Some("."))?,
        })
    }
}

fn main_with(it: &Args) {
    let root: PathBuf =
        std::fs::canonicalize(&it.root).unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(&it.root));
    let rows = checked(&root).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
    if it.command == "check" {
        println!("{SCRIPT}: {} rows; the justfile's `lint` and the `{JOB}` job run them and no other check", rows.len());
        return;
    }
    let Some(scripts) = it.scripts.as_deref() else {
        fail(SCRIPT, "`run` needs `scripts`, the built scripts' directory");
    };
    let mut failed: Vec<&str> = Vec::new();
    for row in &rows {
        let command = command_of(row, scripts, it.base.as_deref()).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
        let start = Instant::now();
        let ran = run_within(&["bash", "-euo", "pipefail", "-c", &command], &root, Duration::from_secs(row.seconds));
        let took = start.elapsed().as_secs_f64();
        let why = match ran {
            Ok(Some(ran)) if ran.status == 0 => None,
            Ok(Some(ran)) => Some((format!("exit {}", ran.status), format!("{}{}", ran.out(), ran.err()))),
            Ok(None) => Some((format!("passed its {} s", row.seconds), String::new())),
            Err(e) => Some(("did not start".to_owned(), e)),
        };
        match why {
            None => println!("ok    {:<28} {took:6.1} s", row.name),
            Some((what, output)) => {
                println!("FAIL  {:<28} {took:6.1} s  ({what}; {LIST}:{})", row.name, row.line);
                let lines: Vec<&str> = output.lines().collect();
                for line in &lines[lines.len().saturating_sub(TAIL)..] {
                    println!("      {line}");
                }
                failed.push(&row.name);
            }
        }
    }
    if !failed.is_empty() {
        fail(SCRIPT, &format!("{} of {} failed: {}", failed.len(), rows.len(), failed.join(", ")));
    }
    println!("{SCRIPT}: every one of {} rows passed", rows.len());
}

fn expect(holds: bool, what: &str) {
    blaze_script::expect(SCRIPT, holds, what);
}

fn selftest() {
    // The list: a row of each kind, and each refusal.
    let list = "# a comment\n\nrustc sites | 300 | - | \"$BLAZE_SCRIPTS/rustcsites\"\nstops | 300 | base | \"$BLAZE_SCRIPTS/stops\" '{\"command\": \"check\"}'\n";
    let rows = parse_list(list).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
    expect(rows.len() == 2 && rows[1].base && rows[1].line == 4 && rows[0].seconds == 300, "two rows read, with their lines");
    expect(matches!(parse_list("a | 1 | -\n"), Err(GateError::Row { line: 1, .. })), "three columns are refused");
    expect(matches!(parse_list("a | 0 | - | x\n"), Err(GateError::Row { .. })), "no timeout is refused");
    expect(matches!(parse_list("a | 3601 | - | x\n"), Err(GateError::Row { .. })), "a timeout past an hour is refused");
    expect(matches!(parse_list("a | 1 | yes | x\n"), Err(GateError::Row { .. })), "a base column of another word is refused");
    expect(matches!(parse_list("a | 1 | base | x selftest\n"), Err(GateError::Row { .. })), "a base row with no JSON is refused");
    expect(matches!(parse_list("a | 1 | - | x\na | 1 | - | y\n"), Err(GateError::TwoRows { line: 2, .. })), "two rows of one name are refused");
    expect(parse_list("# only\n") == Err(GateError::Empty), "an empty list is refused");
    // A command as run: the base in the JSON, the directory quoted.
    let run = command_of(&rows[1], "/t/it's", Some("abc1234")).unwrap_or_default();
    expect(run == "BLAZE_SCRIPTS='/t/it'\\''s'; export BLAZE_SCRIPTS; \"$BLAZE_SCRIPTS/stops\" '{\"command\": \"check\", \"base\": \"abc1234\"}'", "the base is added and the directory quoted");
    expect(command_of(&rows[0], "d", Some("abc1234")).is_ok_and(|it| it.ends_with("\"$BLAZE_SCRIPTS/rustcsites\"")), "a row of no base takes none");
    expect(matches!(command_of(&rows[1], "d", Some("main; rm -rf /")), Err(GateError::Base { .. })), "a base that is no hex name is refused");
    // The justfile: the runner once and nothing else; a check beside it is refused by line.
    let just = "scripts:\n    echo\n\nlint:\n    #!/usr/bin/env bash\n    bin=$(just scripts)\n    \"$bin/gate\" \"{\\\"command\\\": \\\"run\\\", \\\"scripts\\\": \\\"$bin\\\"}\"\n\nlint-write:\n    \"$bin/stops\" x\n";
    expect(check_justfile(just).is_ok(), "a lint recipe that runs the gate alone holds (a later recipe is not read)");
    let beside = just.replace("    bin=$(just scripts)\n", "    bin=$(just scripts)\n    timeout 9 \"$bin/stops\" '{}'\n");
    expect(matches!(check_justfile(&beside), Err(GateError::Justfile { line: 7, what: "calls a script", .. })), "a script beside the gate is refused");
    let cargo = just.replace("    bin=$(just scripts)\n", "    bin=$(just scripts)\n    cargo test -p blaze-lint\n");
    expect(matches!(check_justfile(&cargo), Err(GateError::Justfile { what: "runs cargo", .. })), "cargo beside the gate is refused");
    expect(check_justfile("scripts:\n    echo\n") == Err(GateError::NoLint), "no lint recipe is refused");
    expect(matches!(check_justfile("lint:\n    echo\n"), Err(GateError::Justfile { what: "does not run the gate exactly once", .. })), "a lint recipe with no gate is refused");
    // The workflow: the job's one gate step and uses steps; any extra command is refused.
    let flow = "jobs:\n  scripts:\n    name: s\n    steps:\n      - uses: actions/checkout@v7\n      # the gate\n      - name: the gate\n        env:\n          BASE: x\n        run: |\n          \"$BLAZE_SCRIPTS/gate\" \"{\\\"command\\\": \\\"run\\\"}\"\n  # the next job\n  lean:\n    steps:\n      - run: '\"$BLAZE_SCRIPTS/lean_gate\" x'\n";
    expect(check_workflow(flow).is_ok(), "the gate step and uses steps hold; another job's named CI-only script holds");
    let python = flow.replace("      - uses: actions/checkout@v7\n", "      - uses: actions/checkout@v7\n      - run: uv run --frozen ruff check\n");
    expect(matches!(check_workflow(&python), Err(GateError::Step { line: 6, what: "runs Python", .. })), "a Python step beside the gate is refused by its line");
    let inline = flow.replace("      - uses: actions/checkout@v7\n", "      - uses: actions/checkout@v7\n      - run: '\"$BLAZE_SCRIPTS/rustcsites\"'\n");
    expect(matches!(check_workflow(&inline), Err(GateError::Step { what: "calls a script", .. })), "an inline script step is refused");
    let elsewhere = flow.replace("lean_gate", "docs");
    expect(matches!(check_workflow(&elsewhere), Err(GateError::Script { line: 15, .. })), "a script another job calls is refused unless named");
    let twice = flow.replace("  # the next job", "      - run: '\"$BLAZE_SCRIPTS/gate\" \"{\\\"command\\\": \\\"run\\\"}\"'\n  # the next job");
    expect(check_workflow(&twice) == Err(GateError::GateSteps { count: 2 }), "two gate steps are refused");
    expect(check_workflow("jobs:\n  lean:\n") == Err(GateError::NoJob), "no scripts job is refused");
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The script's own cases run under `blaze test` and `cargo test` too.
    #[test]
    fn the_selftest_holds() {
        selftest();
    }
}
