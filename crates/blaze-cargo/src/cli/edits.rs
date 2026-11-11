//! `blaze debug edits`: the edit loop as one command (docs/profiling.md, "The edit loop"). It
//! applies an edit of one kind to one file, runs `blaze build` with this very binary, and does so
//! N times; each run says its wall time, the daemon's time (`builds.dur_ns`), its critical path
//! (`blaze explain timeline`), what the query engine ran (`query_counts`) and what the daemon
//! said was wrong (`blaze explain problems`); then the medians from the second run on, beside
//! the last recorded run of the same edit.
//!
//! What it holds itself to, each a mistake a hand-rolled loop made on 2026-11-10:
//! - **The build's own exit.** A run whose `blaze build` exits non-zero ends the measurement,
//!   naming the run and the code; nothing after it is timed and no median is said.
//! - **The binary that answered.** Each run's `builds.blaze` is compared with this binary's
//!   BLAKE3 ([`blaze_profile::identity::binary`]); a run another blaze answered is a failure.
//! - **The exact tree** (rule measure-the-exact-tree). The report names this binary and the
//!   digest of the directory as it was before the first edit; a previous run of another binary
//!   or tree is shown beside this one only with a line saying which differs.
//! - **The workspace as it was found.** The file's bytes are written back on every way out
//!   ([`Held`]); a process killed mid-run leaves a journal in the state directory, which the
//!   next `blaze debug edits` restores from before it does anything else ([`recover`]).
//!
//! Not `blaze bench edits`: `blaze bench` is cargo's `bench`, and `edits` there is a bench's name.

use std::{
    ffi::OsString,
    fmt::Write as _,
    io::Write as _,
    path::{Path, PathBuf},
};

use blaze_core::exit::Exit;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::say;

#[cfg(test)]
mod tests;

/// The version of the report's JSON and of a record's: bumped when a field changes meaning or
/// goes away.
pub const VERSION: u32 = 1;

pub const USAGE: &str = "blaze debug edits no-op|comment|body|signature|add-item|dep-body [--file PATH] [--fn NAME[:LINE]] \
                         [--runs N] [--json] [-- BUILD ARGS]";

/// The query functions an edit of one body may run again, always listed, zero or not
/// (harness/v2/budgets.py's `EDIT_QUERIES`).
pub const EDIT_QUERIES: [&str; 5] = [
    "infer_body_query",
    "mir_body_query",
    "runtime_mir_body_query",
    "body_check_clean",
    "module_check_clean",
];

/// The most other queries a run lists, those that ran most.
const OTHER_QUERIES: usize = 12;
/// The most timeline rows the text shows of the last run.
const TIMELINE_ROWS: usize = 12;
/// How often, and how many times at most, a run asks for its build's end row (2 s in all).
const ROW_EVERY: std::time::Duration = std::time::Duration::from_millis(50);
const ROW_TRIES: u32 = 40;
/// The bytes of a failed build's stderr the report keeps, from its end.
const STDERR_TAIL: usize = 4096;

/// What is edited.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// Nothing: the build of an unchanged tree.
    NoOp,
    /// A comment that moves no line: in the named function's body, else at the file's end.
    Comment,
    /// A statement at the top of the named function's body.
    Body,
    /// A lifetime parameter added to the named function, which no caller names.
    Signature,
    /// A private function added at the file's end.
    AddItem,
    /// [`Kind::Body`], said of a file of a crate the built one depends on.
    DepBody,
}

impl Kind {
    pub fn named(name: &str) -> Option<Kind> {
        Some(match name {
            "no-op" | "noop" => Kind::NoOp,
            "comment" => Kind::Comment,
            "body" => Kind::Body,
            "signature" => Kind::Signature,
            "add-item" => Kind::AddItem,
            "dep-body" => Kind::DepBody,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::NoOp => "no-op",
            Kind::Comment => "comment",
            Kind::Body => "body",
            Kind::Signature => "signature",
            Kind::AddItem => "add-item",
            Kind::DepBody => "dep-body",
        }
    }

    fn needs_file(self) -> bool {
        self != Kind::NoOp
    }

    fn needs_function(self) -> bool {
        matches!(self, Kind::Body | Kind::Signature | Kind::DepBody)
    }
}

/// Why an edit can't be made, naming the function and the file's line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    NoFunction { name: String, at: Option<usize> },
    Ambiguous { name: String, lines: Vec<usize> },
    NeedsFunction { kind: Kind },
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditError::NoFunction { name, at: None } => {
                write!(f, "no `fn {name}` with a body is in the file")
            }
            EditError::NoFunction { name, at: Some(line) } => {
                write!(f, "no `fn {name}` with a body is in the file at line {line}")
            }
            EditError::Ambiguous { name, lines } => write!(
                f,
                "`fn {name}` with a body is in the file {} times, at lines {}: name one as `--fn {name}:LINE`",
                lines.len(),
                list(lines)
            ),
            EditError::NeedsFunction { kind } => {
                write!(f, "a {} edit takes `--fn NAME`: the function it edits", kind.name())
            }
        }
    }
}

impl std::error::Error for EditError {}

fn list(lines: &[usize]) -> String {
    // alloc: an error's text
    lines.iter().map(usize::to_string).collect::<Vec<_>>().join(", ")
}

/// A function of a file that has a body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Function {
    /// Where its name ends: where generics begin, or its `(`.
    pub name_end: usize,
    /// Its body's `{`.
    pub brace: usize,
    /// The line of its `fn`, from 1.
    pub line: usize,
}

fn ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The function `name` of `text` that has a body, the one on `line` when given. A match after
/// `//` on its line is a comment's, and one whose `;` comes before its `{` (outside brackets)
/// is a declaration: neither counts. It reads text, not syntax: a `fn name` in a string or a
/// block comment counts, and the error names each line so `NAME:LINE` picks one.
pub fn locate(text: &str, name: &str, line: Option<usize>) -> Result<Function, EditError> {
    let bytes = text.as_bytes();
    // alloc: the candidates, once an edit
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find("fn ").map(|it| it + from) {
        from = at + 3;
        if at > 0 && ident(bytes[at - 1]) {
            continue;
        }
        let rest = text[at + 3..].trim_start_matches(' ');
        let name_at = text.len() - rest.len();
        if !rest.starts_with(name) || rest.as_bytes().get(name.len()).copied().is_some_and(ident) {
            continue;
        }
        let line_start = text[..at].rfind('\n').map_or(0, |it| it + 1);
        if text[line_start..at].contains("//") {
            continue;
        }
        let name_end = name_at + name.len();
        let mut depth = 0i32;
        let mut brace = None;
        for (i, &byte) in bytes.iter().enumerate().skip(name_end) {
            match byte {
                b'(' | b'[' => depth += 1,
                b')' | b']' => depth -= 1,
                b'{' if depth == 0 => {
                    brace = Some(i);
                    break;
                }
                b';' if depth == 0 => break,
                _ => {}
            }
        }
        if let Some(brace) = brace {
            let line = text[..at].bytes().filter(|&it| it == b'\n').count() + 1;
            found.push(Function { name_end, brace, line });
        }
    }
    if let Some(line) = line {
        found.retain(|it| it.line == line);
    }
    match found.as_slice() {
        [one] => Ok(*one),
        // alloc: an error
        [] => Err(EditError::NoFunction { name: name.to_owned(), at: line }),
        many => Err(EditError::Ambiguous {
            // alloc: an error
            name: name.to_owned(),
            lines: many.iter().map(|it| it.line).collect(),
        }),
    }
}

/// `text` with the `n`th edit of `kind` made: each `n` gives other bytes, so each run's build
/// sees a change of the same shape as the last. `function` is the function's name and line.
pub fn apply(
    text: &str,
    kind: Kind,
    function: Option<(&str, Option<usize>)>,
    n: u32,
) -> Result<String, EditError> {
    let at = |text: &str| match function {
        Some((name, line)) => locate(text, name, line).map(Some),
        None if kind.needs_function() => Err(EditError::NeedsFunction { kind }),
        None => Ok(None),
    };
    let insert = |at: usize, what: &str| {
        // alloc: the edited file, once a run
        let mut out = String::with_capacity(text.len() + what.len());
        out.push_str(&text[..at]);
        out.push_str(what);
        out.push_str(&text[at..]);
        out
    };
    Ok(match (kind, at(text)?) {
        // clone: the file as it is, once a run
        (Kind::NoOp, _) => text.to_owned(),
        // A block comment right after the `{`: it moves no line and ends on its own, so a
        // one-line body stays whole.
        (Kind::Comment, Some(f)) => {
            insert(f.brace + 1, &format!(" /* blaze debug edits, run {n} */"))
        }
        (Kind::Comment, None) => insert(text.len(), &format!("\n// blaze debug edits, run {n}\n")),
        (Kind::Body | Kind::DepBody, Some(f)) => {
            insert(f.brace + 1, &format!(" let _ = ::core::hint::black_box({n}u64);"))
        }
        (Kind::Signature, Some(f)) => match text[f.name_end..].trim_start().starts_with('<') {
            true => {
                let open = f.name_end + text[f.name_end..].find('<').unwrap_or(0) + 1;
                insert(open, &format!("'blaze_edit_{n}, "))
            }
            false => insert(f.name_end, &format!("<'blaze_edit_{n}>")),
        },
        (Kind::AddItem, _) => insert(
            text.len(),
            &format!("\n#[allow(dead_code)]\nfn blaze_debug_edits_item_{n}() -> u64 {{\n    {n}\n}}\n"),
        ),
        (Kind::Body | Kind::DepBody | Kind::Signature, None) => {
            return Err(EditError::NeedsFunction { kind });
        }
    })
}

/// The journal's two files in the state directory: the bytes, then the file naming whose they
/// are, which is the commit.
const JOURNAL_BYTES: &str = "edits-restore.bin";
const JOURNAL_PATH: &str = "edits-restore.path";

/// A file whose bytes are written back when this is dropped, or by [`Held::restore`], which
/// says how that went. While it lives, the bytes are also in a journal under `state`, so a
/// process that is killed leaves what [`recover`] needs.
#[derive(Debug)]
pub struct Held {
    path: PathBuf,
    original: Vec<u8>,
    state: PathBuf,
    restored: bool,
}

impl Held {
    /// Reads `path` and journals its bytes under `state`, before any edit of it.
    pub fn take(state: &Path, path: &Path) -> Result<Held, String> {
        let original =
            std::fs::read(path).map_err(|error| format!("reading {}: {error}", path.display()))?;
        let said = |what: &Path, error: std::io::Error| {
            format!("journaling {} in {}: {error}", path.display(), what.display())
        };
        std::fs::create_dir_all(state).map_err(|error| said(state, error))?;
        let bytes = state.join(JOURNAL_BYTES);
        std::fs::write(&bytes, &original).map_err(|error| said(&bytes, error))?;
        let named = state.join(JOURNAL_PATH);
        let Some(text) = path.to_str() else {
            return Err(format!("{} is not UTF-8: its journal could not name it", path.display()));
        };
        std::fs::write(&named, text).map_err(|error| said(&named, error))?;
        // clone: the paths, once a measurement
        Ok(Held { path: path.to_owned(), original, state: state.to_owned(), restored: false })
    }

    /// The file's text as it was found.
    pub fn text(&self) -> Result<&str, String> {
        std::str::from_utf8(&self.original)
            .map_err(|error| format!("{} is not UTF-8: {error}", self.path.display()))
    }

    /// Writes `text` over the file: an edit of it.
    pub fn edit(&self, text: &str) -> Result<(), String> {
        std::fs::write(&self.path, text)
            .map_err(|error| format!("writing the edit to {}: {error}", self.path.display()))
    }

    /// Writes the file's bytes back and removes the journal; the error names the file, and the
    /// journal stays for [`recover`].
    pub fn restore(&mut self) -> Result<(), String> {
        if self.restored {
            return Ok(());
        }
        std::fs::write(&self.path, &self.original).map_err(|error| {
            format!(
                "{} was NOT restored ({error}): its bytes are in {}, which the next `blaze debug edits` writes back",
                self.path.display(),
                self.state.join(JOURNAL_BYTES).display()
            )
        })?;
        self.restored = true;
        forget(&self.state)
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        if let Err(why) = self.restore() {
            eprintln!("blaze: {why}");
        }
    }
}

/// Removes the journal: the name first, as it is the commit.
fn forget(state: &Path) -> Result<(), String> {
    for name in [JOURNAL_PATH, JOURNAL_BYTES] {
        let file = state.join(name);
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("removing the journal {}: {error}", file.display())),
        }
    }
    Ok(())
}

/// Writes back the file a `blaze debug edits` that was killed left edited, from the journal
/// under `state`; the file it restored, `None` when no journal names one.
pub fn recover(state: &Path) -> Result<Option<PathBuf>, String> {
    let named = state.join(JOURNAL_PATH);
    let path = match std::fs::read_to_string(&named) {
        Ok(text) => PathBuf::from(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Bytes with no name: a journal cut before its commit, of a file not yet edited.
            forget(state)?;
            return Ok(None);
        }
        Err(error) => return Err(format!("reading the journal {}: {error}", named.display())),
    };
    let bytes = state.join(JOURNAL_BYTES);
    let original = std::fs::read(&bytes).map_err(|error| {
        format!(
            "the journal {} names {} and its bytes {} can't be read ({error}): {} may still hold an edit of an \
             earlier `blaze debug edits`",
            named.display(),
            path.display(),
            bytes.display(),
            path.display()
        )
    })?;
    std::fs::write(&path, original)
        .map_err(|error| format!("restoring {} from the journal {}: {error}", path.display(), bytes.display()))?;
    forget(state)?;
    Ok(Some(path))
}

/// One query function's runs in a run's build.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counter {
    pub query: String,
    pub executed: u64,
}

/// One timed build.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Run {
    /// From 1.
    pub run: u32,
    /// Its `builds.build_id`.
    pub build: u64,
    /// From this process starting `blaze build` to its exit.
    pub wall_ms: f64,
    /// The daemon's own time for it (`builds.dur_ns`).
    pub daemon_ms: f64,
    /// `wall_ms - daemon_ms`: the client's start, its connection and its exit.
    pub client_ms: f64,
    pub outcome: String,
    /// The tree the daemon built (`builds.tree`), the edit in it.
    pub tree: Option<String>,
    /// The blaze that answered (`builds.blaze`).
    pub blaze: Option<String>,
    /// `blaze explain timeline build=N`'s rows: at_ms, ms, cpu_ms, thread, name, what.
    pub timeline: Vec<Map<String, Value>>,
    /// [`EDIT_QUERIES`], then the queries that ran most.
    pub counters: Vec<Counter>,
    /// `blaze explain problems build=N`'s rows: level, source, what, count, first_ms, last_ms.
    pub problems: Vec<Map<String, Value>>,
}

/// A run that ended the measurement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failed {
    pub run: u32,
    /// What failed, in a sentence: the first line of the text.
    pub what: String,
    /// The build's own exit code; `None` when a signal ended it or it was not the build that failed.
    pub code: Option<i32>,
    /// The end of the build's stderr.
    pub stderr: String,
}

/// The medians of the runs counted.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    /// The first run counted: 2 when there are two or more, as the first follows whatever
    /// came before it (a cold build, another edit).
    pub from_run: u32,
    pub runs: u32,
    pub wall_ms: f64,
    pub daemon_ms: f64,
}

/// What is kept of a measurement that ended well, a line of `<state>/edits.jsonl`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub version: u32,
    pub workspace: String,
    pub kind: Kind,
    pub file: Option<String>,
    pub function: Option<String>,
    pub blaze: String,
    pub tree: String,
    pub summary: Summary,
    pub recorded_unix_s: u64,
}

/// The last recorded measurement of the same edit, and how it stands beside this one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Previous {
    pub record: Record,
    pub same_blaze: bool,
    pub same_tree: bool,
    /// This run's median daemon time less that one's.
    pub daemon_ms_change: f64,
    pub wall_ms_change: f64,
}

/// What `blaze debug edits --json` prints.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub version: u32,
    pub workspace: String,
    pub kind: Kind,
    pub file: Option<String>,
    pub function: Option<String>,
    /// The BLAKE3 of this binary, as `builds.blaze` names it.
    pub blaze: String,
    /// The digest of the directory before the first edit, as `builds.tree` is made.
    pub tree: String,
    /// The command each run ran, after `blaze`.
    pub command: Vec<String>,
    pub runs: Vec<Run>,
    pub failed: Option<Failed>,
    /// `None` when a run failed.
    pub summary: Option<Summary>,
    pub previous: Option<Previous>,
    /// The file is as it was found; false only with `failed` saying why.
    pub restored: bool,
}

/// What one run's build gave: the run, or why the measurement ends here.
pub enum Built {
    Ran(Run),
    Failed(Failed),
}

/// The edit to make: what [`measure`] is told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub kind: Kind,
    pub file: Option<PathBuf>,
    pub function: Option<(String, Option<usize>)>,
    pub runs: u32,
}

/// The runs of `edit`: for each, the `n`th edit of the file as it was found, then `build(n)`.
/// It stops at the first run that fails, and the file is written back before it returns,
/// however it returns; `restored` false, with a failure saying so, only when that write failed.
pub fn measure(
    edit: &Edit,
    state: &Path,
    mut build: impl FnMut(u32) -> Result<Built, String>,
) -> Result<(Vec<Run>, Option<Failed>, bool), String> {
    let mut held = match &edit.file {
        Some(file) if edit.kind.needs_file() => Some(Held::take(state, file)?),
        _ => None,
    };
    let function = edit.function.as_ref().map(|(name, line)| (name.as_str(), *line));
    // Before any build: an edit that can't be made is no measurement.
    if let Some(held) = &held {
        apply(held.text()?, edit.kind, function, 1).map_err(|error| {
            format!("{}: {error}", edit.file.as_deref().unwrap_or(Path::new("?")).display())
        })?;
    }
    // alloc: the runs, once a measurement
    let mut runs = Vec::new();
    let mut failed = None;
    for n in 1..=edit.runs {
        let step = (|| {
            if let Some(held) = &held {
                let text = apply(held.text()?, edit.kind, function, n)
                    .map_err(|error| format!("{}: {error}", held.path.display()))?;
                held.edit(&text)?;
            }
            build(n)
        })();
        match step {
            Ok(Built::Ran(run)) => runs.push(run),
            Ok(Built::Failed(it)) => {
                failed = Some(it);
                break;
            }
            Err(what) => {
                // alloc: a failure, once
                failed = Some(Failed { run: n, what, code: None, stderr: String::new() });
                break;
            }
        }
    }
    let mut restored = true;
    if let Some(held) = &mut held
        && let Err(why) = held.restore()
    {
        restored = false;
        // The loss of the user's file is said first, whatever else failed.
        failed = Some(Failed {
            run: failed.as_ref().map_or(edit.runs, |it| it.run),
            what: match failed {
                Some(it) => format!("{why}; before that, {}", it.what),
                None => why,
            },
            code: None,
            stderr: String::new(),
        });
    }
    Ok((runs, failed, restored))
}

/// The median of `xs`: of an even count, the mean of the middle two.
pub fn median(xs: &[f64]) -> Option<f64> {
    // alloc: a copy to sort, once a measurement
    let mut xs = xs.to_vec();
    xs.sort_by(f64::total_cmp);
    match xs.len() {
        0 => None,
        n if n % 2 == 1 => Some(xs[n / 2]),
        n => Some((xs[n / 2 - 1] + xs[n / 2]) / 2.0),
    }
}

/// The medians of `runs` from the second on (of the only one, when there is one).
pub fn summary(runs: &[Run]) -> Option<Summary> {
    let counted = if runs.len() >= 2 { &runs[1..] } else { runs };
    // alloc: the samples, once a measurement
    let wall: Vec<f64> = counted.iter().map(|it| it.wall_ms).collect();
    // alloc: the samples, once a measurement
    let daemon: Vec<f64> = counted.iter().map(|it| it.daemon_ms).collect();
    Some(Summary {
        from_run: counted.first()?.run,
        runs: counted.len() as u32,
        wall_ms: median(&wall)?,
        daemon_ms: median(&daemon)?,
    })
}

/// The last of `records` (oldest first) of the same edit of the same workspace as `now`, set
/// beside it: whether its binary and its tree are this one's is said, never assumed.
pub fn previous(records: &[Record], now: &Record) -> Option<Previous> {
    let record = records.iter().rev().find(|it| {
        it.version == VERSION
            && it.workspace == now.workspace
            && it.kind == now.kind
            && it.file == now.file
            && it.function == now.function
    })?;
    Some(Previous {
        // clone: a record, once a measurement
        record: record.clone(),
        same_blaze: record.blaze == now.blaze,
        same_tree: record.tree == now.tree,
        daemon_ms_change: now.summary.daemon_ms - record.summary.daemon_ms,
        wall_ms_change: now.summary.wall_ms - record.summary.wall_ms,
    })
}

/// The first 12 digits of a hash: what a line names it by.
fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

fn subject(report: &Report) -> String {
    let mut out = format!("{} edit", report.kind.name());
    if let Some(function) = &report.function {
        _ = write!(out, " of `{function}`");
    }
    if let Some(file) = &report.file {
        _ = write!(out, " in {file}");
    }
    out
}

/// `report` for a person: its first line is the answer (the medians, or what failed).
pub fn text(report: &Report) -> String {
    // alloc: the report's text, once
    let mut out = String::new();
    let what = subject(report);
    // Writing to a String cannot fail.
    match (&report.failed, &report.summary) {
        (Some(failed), _) => {
            _ = writeln!(
                out,
                "FAILED: {what}, run {}: {}; nothing is timed past it and no median is said",
                failed.run, failed.what
            );
            for line in failed.stderr.lines() {
                _ = writeln!(out, "  | {line}");
            }
        }
        (None, Some(sum)) => {
            let counted = match sum.from_run {
                1 => "its only run: a first run follows whatever came before it, so run it with --runs 2 or more"
                    .to_owned(),
                from => format!(
                    "median of runs {from}..{}; run 1 follows whatever came before it and is not counted",
                    from + sum.runs - 1
                ),
            };
            _ = writeln!(
                out,
                "{what}: daemon {:.1} ms, wall {:.1} ms ({counted})",
                sum.daemon_ms, sum.wall_ms
            );
        }
        (None, None) => _ = writeln!(out, "{what}: no run"),
    }
    _ = writeln!(
        out,
        "  blaze {} (this binary; every run listed was answered by it), tree {} (before the edit), `blaze {}` in {}",
        short(&report.blaze),
        short(&report.tree),
        report.command.join(" "),
        report.workspace
    );
    if !report.restored {
        _ = writeln!(out, "  THE FILE WAS NOT RESTORED: see the first line");
    }
    if !report.runs.is_empty() {
        _ = write!(out, "  {:>3}  {:>6}  {:>9}  {:>9}  {:>9}  {:>8}", "run", "build", "wall ms", "daemon ms", "client ms", "problems");
        for query in EDIT_QUERIES {
            _ = write!(out, "  {query}");
        }
        out.push('\n');
    }
    for run in &report.runs {
        let problems: u64 = run
            .problems
            .iter()
            .map(|it| it.get("count").and_then(Value::as_u64).unwrap_or(1))
            .sum();
        _ = write!(
            out,
            "  {:>3}  {:>6}  {:>9.1}  {:>9.1}  {:>9.1}  {:>8}",
            run.run, run.build, run.wall_ms, run.daemon_ms, run.client_ms, problems
        );
        for query in EDIT_QUERIES {
            let ran = run.counters.iter().find(|it| it.query == query).map_or(0, |it| it.executed);
            _ = write!(out, "  {ran:>width$}", width = query.len());
        }
        out.push('\n');
    }
    if let Some(last) = report.runs.last() {
        let others: Vec<String> = last
            .counters
            .iter()
            .filter(|it| !EDIT_QUERIES.contains(&it.query.as_str()) && it.executed > 0)
            .map(|it| format!("{} {}", it.query, it.executed))
            .collect();
        if !others.is_empty() {
            _ = writeln!(out, "  run {} also ran: {}", last.run, others.join(", "));
        }
        for it in &last.problems {
            let cell = |key: &str| match it.get(key) {
                Some(Value::String(it)) => it.clone(), // clone: a cell, printed once
                Some(Value::Null) | None => "?".to_owned(),
                Some(it) => it.to_string(),
            };
            _ = writeln!(
                out,
                "  problem ({} from {}, {}x): {}",
                cell("level"),
                cell("source"),
                cell("count"),
                cell("what")
            );
        }
        if !last.timeline.is_empty() {
            _ = writeln!(out, "  run {}'s critical path (build {}; `blaze explain timeline build={}` has every row):", last.run, last.build, last.build);
        }
        for row in last.timeline.iter().take(TIMELINE_ROWS) {
            let ms = |key: &str| row.get(key).and_then(Value::as_f64).unwrap_or(0.0);
            let word = |key: &str| row.get(key).and_then(Value::as_str).unwrap_or("");
            _ = writeln!(
                out,
                "    {:>8.1} ms  +{:<7.1} cpu {:<7.1} {:<16} {} {}",
                ms("at_ms"),
                ms("ms"),
                ms("cpu_ms"),
                word("thread"),
                word("name"),
                word("what")
            );
        }
    }
    match (&report.previous, &report.summary) {
        (Some(prev), Some(_)) => {
            let was = &prev.record;
            match (prev.same_blaze, prev.same_tree) {
                (true, true) => _ = writeln!(
                    out,
                    "  against the last recorded run of this binary on this tree: daemon {:+.1} ms (was {:.1}), wall {:+.1} ms (was {:.1})",
                    prev.daemon_ms_change, was.summary.daemon_ms, prev.wall_ms_change, was.summary.wall_ms
                ),
                (same_blaze, same_tree) => {
                    let mut differs = Vec::new();
                    if !same_blaze {
                        differs.push(format!("ANOTHER BINARY (blaze {}, this is {})", short(&was.blaze), short(&report.blaze)));
                    }
                    if !same_tree {
                        differs.push(format!("ANOTHER TREE ({}, this is {})", short(&was.tree), short(&report.tree)));
                    }
                    _ = writeln!(
                        out,
                        "  the last recorded run of this edit was of {}: not the same pair, so the difference is theirs as much as the edit's: daemon {:+.1} ms (was {:.1}), wall {:+.1} ms (was {:.1})",
                        differs.join(" and "),
                        prev.daemon_ms_change, was.summary.daemon_ms, prev.wall_ms_change, was.summary.wall_ms
                    );
                }
            }
        }
        (None, Some(_)) => _ = writeln!(out, "  no earlier run of this edit is recorded: this one is, for the next"),
        (_, None) => {}
    }
    out
}

/// The rows of `blaze debug sql --json`'s form: an object a line.
fn rows(text: &str, what: &str) -> Result<Vec<Map<String, Value>>, String> {
    text.lines()
        .filter(|it| !it.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<Map<String, Value>>(line)
                .map_err(|error| format!("{what}: a row that is no JSON object ({error}): {line}"))
        })
        .collect()
}

/// A named view's rows for `build`.
fn view(name: &str, build: u64) -> Result<Vec<Map<String, Value>>, String> {
    let view = blaze_profile::views::find(name).map_err(|error| error.to_string())?;
    let mut args = Map::new();
    args.insert("build".to_owned(), Value::from(build));
    let sql = blaze_profile::views::render(view, &args).map_err(|error| error.to_string())?;
    rows(&super::observe::rows_json(&sql)?, name)
}

/// [`EDIT_QUERIES`] (zero when the build has no row for one), then the [`OTHER_QUERIES`] other
/// queries that ran most, of `counts` (each query once).
pub fn counters(counts: &[Counter]) -> Vec<Counter> {
    // alloc: a run's counters
    let mut out: Vec<Counter> = EDIT_QUERIES
        .iter()
        .map(|&query| Counter {
            query: query.to_owned(),
            executed: counts.iter().find(|it| it.query == query).map_or(0, |it| it.executed),
        })
        .collect();
    // alloc: the others, sorted
    let mut others: Vec<&Counter> =
        counts.iter().filter(|it| !EDIT_QUERIES.contains(&it.query.as_str()) && it.executed > 0).collect();
    others.sort_by(|a, b| b.executed.cmp(&a.executed).then_with(|| a.query.cmp(&b.query)));
    // clone: a counter, once a run
    out.extend(others.into_iter().take(OTHER_QUERIES).cloned());
    out
}

/// The build the daemon ran between `t0` and `t1` on the history's clock, as a [`Run`] with
/// its rows. No build there, or more than one, is an error: the measurement needs its daemon
/// to itself, and never guesses which build was its own.
fn facts(run: u32, wall_ms: f64, t0: i64, t1: i64, ours: &str) -> Result<Built, String> {
    let sql = format!(
        "SELECT CAST(build_id AS UBIGINT) AS build_id, CAST(max(dur_ns) AS BIGINT) AS dur_ns, \
         arg_max(CAST(outcome AS VARCHAR), dur_ns) AS outcome, CAST(max(tree) AS VARCHAR) AS tree, \
         CAST(max(blaze) AS VARCHAR) AS blaze FROM builds \
         WHERE started_ns BETWEEN {t0} AND {t1} AND dur_ns IS NOT NULL GROUP BY 1 ORDER BY 1"
    );
    // The build's end row is recorded as its job ends, which can be after its client has its
    // answer and exits: asked again, a bounded number of times, each ask a flush of its own.
    let found = {
        let _span = tracing::info_span!(
            target: "blaze",
            "wait",
            awaits = "the daemon's `builds` row of the run's build"
        )
        .entered();
        let mut tries = 0;
        loop {
            let found = rows(&super::observe::rows_json(&sql)?, "the run's build")?;
            tries += 1;
            if !found.is_empty() || tries == ROW_TRIES {
                break found;
            }
            std::thread::sleep(ROW_EVERY);
        }
    };
    let row = match found.as_slice() {
        [row] => row,
        [] => {
            return Err(
                "`blaze build` exited 0 and, 2 s on, the daemon's history has no ended build that started while it \
                 ran: the daemon whose history this reads did not serve it (another BLAZE_HOME, or no daemon)"
                    .to_owned(),
            );
        }
        many => {
            return Err(format!(
                "{} builds started in the daemon while this run's `blaze build` ran: something else is building on \
                 this daemon, and a measurement needs it alone (a `BLAZE_HOME` of its own)",
                many.len()
            ));
        }
    };
    let text = |key: &str| row.get(key).and_then(Value::as_str).map(str::to_owned);
    let (Some(build), Some(dur_ns)) = (
        row.get("build_id").and_then(Value::as_u64),
        row.get("dur_ns").and_then(Value::as_i64),
    ) else {
        return Err(format!("the run's `builds` row has no build_id or dur_ns: {row:?}"));
    };
    let blaze = text("blaze");
    if let Some(theirs) = &blaze
        && theirs != ours
    {
        return Ok(Built::Failed(Failed {
            run,
            what: format!(
                "build {build} was answered by blaze {}, and this binary is {}: the daemon is another build of \
                 blaze, so its time is not this binary's",
                short(theirs),
                short(ours)
            ),
            code: None,
            stderr: String::new(),
        }));
    }
    let outcome = text("outcome").unwrap_or_default();
    if outcome != "ok" {
        return Ok(Built::Failed(Failed {
            run,
            what: format!("`blaze build` exited 0 and the daemon's build {build} ended `{outcome}`"),
            code: Some(0),
            stderr: String::new(),
        }));
    }
    let counts_sql = format!(
        "SELECT CAST(query AS VARCHAR) AS query, CAST(sum(executed) AS UBIGINT) AS executed FROM query_counts \
         WHERE build_id = {build} GROUP BY 1 ORDER BY 1"
    );
    // alloc: the build's counts, once a run
    let counts: Vec<Counter> = rows(&super::observe::rows_json(&counts_sql)?, "the run's query counts")?
        .iter()
        .filter_map(|it| {
            Some(Counter {
                query: it.get("query")?.as_str()?.to_owned(),
                executed: it.get("executed")?.as_u64()?,
            })
        })
        .collect();
    let daemon_ms = dur_ns as f64 / 1e6;
    Ok(Built::Ran(Run {
        run,
        build,
        wall_ms,
        daemon_ms,
        client_ms: wall_ms - daemon_ms,
        outcome,
        tree: text("tree"),
        blaze,
        timeline: view("timeline", build)?,
        counters: counters(&counts),
        problems: view("problems", build)?,
    }))
}

/// One run's `blaze build`, by this very executable: its own exit code is the run's, and its
/// wall time is from its spawn to its exit.
fn build(exe: &Path, args: &[String], run: u32, ours: &str) -> Result<Built, String> {
    let t0 = blaze_profile::clock::mono_ns();
    let started = std::time::Instant::now();
    let output = std::process::Command::new(exe)
        .arg("build")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("starting `{} build`: {error}", exe.display()))?;
    let wall_ms = started.elapsed().as_secs_f64() * 1e3;
    let t1 = blaze_profile::clock::mono_ns();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut from = stderr.len().saturating_sub(STDERR_TAIL);
        while !stderr.is_char_boundary(from) {
            from += 1;
        }
        return Ok(Built::Failed(Failed {
            run,
            what: match output.status.code() {
                Some(code) => format!("`blaze build` exited {code}"),
                None => format!("`blaze build` was ended by a signal ({})", output.status),
            },
            code: output.status.code(),
            // alloc: the failure's text, once
            stderr: stderr[from..].to_owned(),
        }));
    }
    facts(run, wall_ms, t0, t1, ours)
}

/// What the command line asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asked {
    pub edit: Edit,
    pub json: bool,
    pub build_args: Vec<String>,
}

/// The words after `blaze debug edits`, read; the error is the usage, saying what was wrong.
pub fn parse(words: Vec<String>) -> Result<Asked, String> {
    let usage = |what: &str| format!("{USAGE}: {what}");
    let mut words = words.into_iter();
    let (mut kind, mut file, mut function, mut runs, mut json) = (None, None, None, 5u32, false);
    // alloc: the build's arguments, once
    let mut build_args = Vec::new();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--" => {
                build_args.extend(words.by_ref());
            }
            "--json" => json = true,
            "--file" => {
                file = Some(PathBuf::from(words.next().ok_or_else(|| usage("`--file` takes a path"))?));
            }
            "--fn" => {
                let named = words.next().ok_or_else(|| usage("`--fn` takes a function's name"))?;
                function = Some(match named.rsplit_once(':') {
                    Some((name, line)) if !name.ends_with(':') => (
                        name.to_owned(),
                        Some(line.parse::<usize>().map_err(|_| {
                            usage(&format!("`--fn {named}`: `{line}` is no line number"))
                        })?),
                    ),
                    _ => (named, None),
                });
            }
            "--runs" => {
                runs = words
                    .next()
                    .and_then(|it| it.parse().ok())
                    .filter(|&it| it > 0)
                    .ok_or_else(|| usage("`--runs` takes a number of runs, 1 or more"))?;
            }
            name if kind.is_none() && !name.starts_with('-') => {
                kind = Some(
                    Kind::named(name).ok_or_else(|| usage(&format!("`{name}` is no kind of edit")))?,
                );
            }
            other => return Err(usage(&format!("not `{other}`"))),
        }
    }
    let kind = kind.ok_or_else(|| usage("which kind of edit"))?;
    if kind.needs_file() && file.is_none() {
        return Err(usage(&format!("a {} edit takes `--file PATH`: the file it edits", kind.name())));
    }
    if kind.needs_function() && function.is_none() {
        return Err(usage(&EditError::NeedsFunction { kind }.to_string()));
    }
    if kind == Kind::NoOp && (file.is_some() || function.is_some()) {
        return Err(usage("a no-op edits nothing: it takes no `--file` and no `--fn`"));
    }
    Ok(Asked { edit: Edit { kind, file, function, runs }, json, build_args })
}

/// The records kept in `file`, oldest first; a line that is no record of this version's shape
/// is an error naming the file and the line.
fn records(file: &Path) -> Result<Vec<Record>, String> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("{}: {error}", file.display())),
    };
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(at, line)| {
            serde_json::from_str(line).map_err(|error| {
                format!("{}:{}: not a record `blaze debug edits` keeps ({error}); delete the line", file.display(), at + 1)
            })
        })
        .collect()
}

fn record(file: &Path, record: &Record) -> Result<(), String> {
    let said = |error: &dyn std::fmt::Display| format!("recording the run in {}: {error}", file.display());
    let mut line = serde_json::to_string(record).map_err(|error| said(&error))?;
    line.push('\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .and_then(|mut it| it.write_all(line.as_bytes()))
        .map_err(|error| said(&error))
}

fn run(asked: &Asked) -> Result<Report, String> {
    let state = crate::serve::state().map_err(|error| format!("{error:#}"))?;
    if let Some(path) = recover(&state)? {
        eprintln!(
            "blaze: {} still held the edit of a `blaze debug edits` that was killed: it is restored",
            path.display()
        );
    }
    let cwd = std::env::current_dir().map_err(|error| format!("reading the current directory: {error}"))?;
    let exe = std::env::current_exe().map_err(|error| format!("finding blaze's own executable: {error}"))?;
    let blaze = blaze_profile::identity::binary()
        .map_err(|error| format!("hashing blaze itself, {}: {error}", exe.display()))?;
    let tree = blaze_profile::identity::tree_digest(&cwd)
        .map_err(|error| format!("the digest of the tree at {}: {error}", cwd.display()))?;
    let (runs, failed, restored) =
        measure(&asked.edit, &state, |n| build(&exe, &asked.build_args, n, &blaze))?;
    let summary = if failed.is_none() { summary(&runs) } else { None };
    // alloc: the command's words, once
    let mut command = vec!["build".to_owned()];
    // clone: the build's arguments, once
    command.extend(asked.build_args.iter().cloned());
    let mut report = Report {
        version: VERSION,
        workspace: cwd.display().to_string(),
        kind: asked.edit.kind,
        file: asked.edit.file.as_ref().map(|it| it.display().to_string()),
        function: asked.edit.function.as_ref().map(|(name, _)| name.clone()), // clone: once
        blaze,
        tree,
        command,
        runs,
        failed,
        summary,
        previous: None,
        restored,
    };
    if let Some(summary) = report.summary {
        let kept = state.join("edits.jsonl");
        let now = Record {
            version: VERSION,
            // clone: the report's own fields into its record, once
            workspace: report.workspace.clone(),
            kind: report.kind,
            file: report.file.clone(),
            function: report.function.clone(),
            blaze: report.blaze.clone(),
            tree: report.tree.clone(),
            summary,
            recorded_unix_s: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |it| it.as_secs()),
        };
        report.previous = previous(&records(&kept)?, &now);
        record(&kept, &now)?;
    }
    Ok(report)
}

/// `blaze debug edits ...`: 0 when every run's build ended well and the file is as it was
/// found; the usage's code for a wrong word; failed otherwise, the report still printed.
pub fn main(words: Vec<OsString>, color: bool) -> i32 {
    let words: Result<Vec<String>, _> = words.into_iter().map(OsString::into_string).collect();
    let asked = match words.map_err(|it| format!("{} is not UTF-8", it.to_string_lossy())).and_then(parse) {
        Ok(asked) => asked,
        Err(what) => {
            eprint!("{}", say::error(&what, color));
            return Exit::Usage.code();
        }
    };
    let report = match run(&asked) {
        Ok(report) => report,
        Err(what) => {
            eprint!("{}", say::error(&format!("`blaze debug edits`: {what}"), color));
            return Exit::Failed.code();
        }
    };
    let printed = match asked.json {
        true => match serde_json::to_string(&report) {
            Ok(json) => json + "\n",
            Err(error) => {
                eprint!("{}", say::error(&format!("`blaze debug edits`'s report as JSON: {error}"), color));
                return Exit::Failed.code();
            }
        },
        false => text(&report),
    };
    let mut out = std::io::stdout().lock();
    if let Err(error) = out.write_all(printed.as_bytes()).and_then(|()| out.flush()) {
        eprint!("{}", say::error(&format!("stdout: {error}"), color));
        return Exit::Failed.code();
    }
    match report.failed {
        None => 0,
        Some(_) => Exit::Failed.code(),
    }
}
