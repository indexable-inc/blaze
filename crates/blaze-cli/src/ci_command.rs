//! `blaze ci [dir]`: built-in strict CI command, LLM-first by default with first-class GitHub
//! Actions integration.
//!
//! Runs `blaze check` (including built-in unused-dependency detection and lints) and `blaze test`
//! (unless `--no-test`) over the exact same engine check answer (no double-checking), fails on
//! any issue (errors, warnings, unused dependencies, test failures, or unchecked items unless
//! `--allow-incomplete`), and always writes a deterministic machine/LLM-readable todo file
//! (default `.blaze/ci/todo.btab`, Blaze Tabular Data via `bstd::tab`, or JSON via
//! `--todo-format=json` / `--todo path.json`).
//!
//! When `GITHUB_ACTIONS=true` or `--format=github`:
//! - Wraps phases in collapsible `::group::...` / `::endgroup::` blocks
//! - Emits native inline workflow annotations (`::error ...` / `::warning ...`)
//! - Writes `.blaze/ci/github-review.json` with one-click PR `suggestion` comments for fixable findings
//! - Appends a concise Markdown summary table to `$GITHUB_STEP_SUMMARY` when set.

use core::fmt::Write as _;
use core::num::NonZeroU32;

use blaze_own::{AString, String, Vec};
use blaze_report::Severity;
use blaze_report::frame::Choice;
use blaze_report::frame::words::{self, Area, Kind as WordKind};
use bstd::Cx;
use bstd::entry::Status;
use bstd::env::VarName;
use bstd::fs::{Content, Dir, Durable, Fs, ReadWrite};
use bstd::mem::{Bytes, Frame, format_in};
use bstd::out::Out;
use bstd::path::Path;

use crate::CliError;
use crate::fix::Listed;
use crate::phases::Phases;

const DEFAULT_TODO_BTAB: &str = ".blaze/ci/todo.btab";
const DEFAULT_TODO_JSON: &str = ".blaze/ci/todo.json";
const DEFAULT_REVIEW_JSON: &str = ".blaze/ci/github-review.json";
const CI_FILE_FRAME: usize = 32 << 20;

/// Output presentation for `blaze ci`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum OutputStyle {
    Auto,
    Plain,
    GitHub,
}

/// Serialization format for the CI todo artifact.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TodoKindFormat {
    Btab,
    Json,
}

/// Parsed arguments of `blaze ci`.
pub(crate) struct CiArgs<'a> {
    pub(crate) dir: Option<&'a str>,
    pub(crate) workers: Option<NonZeroU32>,
    pub(crate) style: OutputStyle,
    pub(crate) color: Choice,
    pub(crate) todo_path: Option<&'a str>,
    pub(crate) todo_format: Option<TodoKindFormat>,
    pub(crate) no_todo: bool,
    pub(crate) no_test: bool,
    pub(crate) fix: bool,
    pub(crate) allow_incomplete: bool,
}

impl<'a> CiArgs<'a> {
    fn empty() -> CiArgs<'a> {
        CiArgs {
            dir: None,
            workers: None,
            style: OutputStyle::Auto,
            color: Choice::Never,
            todo_path: None,
            todo_format: None,
            no_todo: false,
            no_test: false,
            fix: false,
            allow_incomplete: false,
        }
    }

    /// Resolve effective todo format: explicit `--todo-format`, else inferred from `--todo`
    /// extension (`.json` -> Json), else Blaze Tabular Data (`btab`, LLM-first default).
    pub(crate) fn resolved_todo_format(&self) -> TodoKindFormat {
        if let Some(fmt) = self.todo_format {
            return fmt;
        }
        if let Some(path) = self.todo_path
            && path.ends_with(".json")
        {
            return TodoKindFormat::Json;
        }
        TodoKindFormat::Btab
    }

    /// Resolve effective todo file path, or `None` when `--no-todo`.
    pub(crate) fn resolved_todo_path(&self) -> Option<&'a str> {
        if self.no_todo {
            return None;
        }
        if let Some(path) = self.todo_path {
            return Some(path);
        }
        match self.resolved_todo_format() {
            TodoKindFormat::Btab => Some(DEFAULT_TODO_BTAB),
            TodoKindFormat::Json => Some(DEFAULT_TODO_JSON),
        }
    }
}

fn parse_style(word: &str) -> Result<OutputStyle, CliError> {
    match word {
        "auto" => Ok(OutputStyle::Auto),
        "plain" => Ok(OutputStyle::Plain),
        "github" => Ok(OutputStyle::GitHub),
        _ => Err(CliError::Usage("--format is auto, plain or github")),
    }
}

fn parse_todo_format(word: &str) -> Result<TodoKindFormat, CliError> {
    match word {
        "btab" | "tab" => Ok(TodoKindFormat::Btab),
        "json" => Ok(TodoKindFormat::Json),
        _ => Err(CliError::Usage("--todo-format is btab, tab or json")),
    }
}

fn parse_color(word: &str) -> Result<Choice, CliError> {
    match word {
        "auto" => Ok(Choice::Auto),
        "always" => Ok(Choice::Always),
        "never" => Ok(Choice::Never),
        _ => Err(CliError::Usage("--color is auto, always or never")),
    }
}

/// Parse arguments after `blaze ci`.
pub(crate) fn parse_ci_args(args: &bstd::args::Args) -> Result<CiArgs<'_>, CliError> {
    let mut parsed = CiArgs::empty();
    let mut at = 2;
    while let Some(arg) = args.get(at) {
        at += 1;
        let mut next_val = |msg: &'static str| -> Result<&str, CliError> {
            let val = args.get(at).ok_or(CliError::Usage(msg))?;
            at += 1;
            Ok(val)
        };
        match arg {
            "--no-test" => parsed.no_test = true,
            "--no-todo" => parsed.no_todo = true,
            "--fix" => parsed.fix = true,
            "--allow-incomplete" => parsed.allow_incomplete = true,
            "--workers" => {
                let raw = next_val("--workers takes a count of at least 1")?;
                parsed.workers = Some(raw.parse::<NonZeroU32>().map_err(|_| CliError::Usage("--workers takes a count of at least 1"))?);
            }
            _ if let Some(raw) = arg.strip_prefix("--workers=") => {
                parsed.workers = Some(raw.parse::<NonZeroU32>().map_err(|_| CliError::Usage("--workers takes a count of at least 1"))?);
            }
            "--format" => parsed.style = parse_style(next_val("--format is auto, plain or github")?)?,
            _ if let Some(raw) = arg.strip_prefix("--format=") => parsed.style = parse_style(raw)?,
            "--todo" => parsed.todo_path = Some(next_val("--todo takes a file path")?),
            _ if let Some(raw) = arg.strip_prefix("--todo=") => parsed.todo_path = Some(raw),
            "--todo-format" => parsed.todo_format = Some(parse_todo_format(next_val("--todo-format is btab, tab or json")?)?),
            _ if let Some(raw) = arg.strip_prefix("--todo-format=") => parsed.todo_format = Some(parse_todo_format(raw)?),
            "--color" => parsed.color = parse_color(next_val("--color is auto, always or never")?)?,
            _ if let Some(raw) = arg.strip_prefix("--color=") => parsed.color = parse_color(raw)?,
            _ if arg.starts_with("--") => return Err(CliError::Usage("unknown flag for `blaze ci`")),
            _ if parsed.dir.is_some() => return Err(CliError::Usage("one checkout directory at most")),
            _ => parsed.dir = Some(arg),
        }
    }
    Ok(parsed)
}

/// Classification of one CI todo item.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum ItemKind {
    Error,
    UnusedDependency,
    Warning,
    #[allow(dead_code, reason = "reserved for blaze-fmt once wired into blaze-cli")]
    Format,
    TestFailure,
    Incomplete,
}

impl ItemKind {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            ItemKind::Error => "error",
            ItemKind::UnusedDependency => "unused_dependency",
            ItemKind::Warning => "warning",
            ItemKind::Format => "format",
            ItemKind::TestFailure => "test_failure",
            ItemKind::Incomplete => "incomplete",
        }
    }

    pub(crate) const fn github_level(self) -> &'static str {
        match self {
            ItemKind::Error | ItemKind::TestFailure | ItemKind::Incomplete => "error",
            ItemKind::UnusedDependency | ItemKind::Warning | ItemKind::Format => "warning",
        }
    }
}

/// Concrete line-based replacement suggestion for a fixable CI finding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Suggestion<'f> {
    pub(crate) start_line: u32,
    pub(crate) end_line: u32,
    pub(crate) replacement: &'f str,
}

/// One actionable item in the CI todo list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct TodoItem<'f> {
    pub(crate) kind: ItemKind,
    pub(crate) file: &'f str,
    pub(crate) line: u32,
    pub(crate) col: u32,
    pub(crate) code: &'f str,
    pub(crate) message: &'f str,
    pub(crate) suggestion: Option<Suggestion<'f>>,
    pub(crate) command: Option<&'f str>,
}

/// Summary counts of a `blaze ci` run.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Summary {
    pub(crate) errors: u32,
    pub(crate) warnings: u32,
    pub(crate) unused_deps: u32,
    pub(crate) format: u32,
    pub(crate) test_failures: u32,
    pub(crate) incomplete: u32,
    pub(crate) tests_passed: u32,
    pub(crate) tests_ignored: u32,
    pub(crate) tests_ran: bool,
    pub(crate) fixes_applied: u32,
    pub(crate) files_fixed: u32,
}

impl Summary {
    pub(crate) const fn passed(&self, allow_incomplete: bool) -> bool {
        self.errors == 0
            && self.warnings == 0
            && self.unused_deps == 0
            && self.format == 0
            && self.test_failures == 0
            && (allow_incomplete || self.incomplete == 0)
    }
}

/// Compute 1-based `(line, col)` for byte offset `offset` in `text`.
fn line_col(text: &str, offset: u32) -> (u32, u32) {
    let bytes = text.as_bytes();
    let at = usize::try_from(offset).unwrap_or(bytes.len()).min(bytes.len());
    let mut line: u32 = 1;
    let mut line_start: usize = 0;
    for (i, &b) in bytes.get(..at).unwrap_or(bytes).iter().enumerate() {
        if b == b'\n' {
            line = line.saturating_add(1);
            line_start = i + 1;
        }
    }
    let col = u32::try_from(at.saturating_sub(line_start).saturating_add(1)).unwrap_or(u32::MAX);
    (line, col)
}

/// Convert a byte-range [`crate::fix::Change`] in `text` to a line-based [`Suggestion`].
fn suggestion_of<'f>(text: &str, change: crate::fix::Change<'_>, frame: &'f Frame) -> Option<Suggestion<'f>> {
    let bytes = text.as_bytes();
    let s = usize::try_from(change.start).ok()?.min(bytes.len());
    let e = usize::try_from(change.end).ok()?.clamp(s, bytes.len());
    let (start_line, _) = line_col(text, change.start);
    // When deleting a whole line ending with `\n` (like `unused-dependency` or `bstd`), the
    // end offset sits on the first byte of the following line; clamp end_line to the deleted line.
    let end_for_line = if e > s && bytes.get(e.saturating_sub(1)) == Some(&b'\n') {
        e - 1
    } else {
        e
    };
    let (end_line, _) = line_col(text, u32::try_from(end_for_line).ok()?);
    let line_start = bytes.get(..s)?.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let line_end_no_nl = bytes.get(end_for_line..)?.iter().position(|&b| b == b'\n').map_or(bytes.len(), |i| end_for_line + i);
    // Whole-line deletion (`insert == ""` covering full line(s)).
    if change.insert.is_empty() && s == line_start && (e >= line_end_no_nl) {
        return Some(Suggestion { start_line, end_line, replacement: "" });
    }
    // Inline edit: build the full replacement line(s) from `line_start..line_end_no_nl`.
    let prefix = text.get(line_start..s)?;
    let suffix = text.get(e..line_end_no_nl)?;
    let mut replaced = String::with_capacity_in(prefix.len() + change.insert.len() + suffix.len(), frame);
    replaced.push_str(prefix);
    replaced.push_str(change.insert);
    replaced.push_str(suffix);
    Some(Suggestion { start_line, end_line, replacement: frame.alloc_str(replaced.as_str()) })
}

/// Render a finding's headline message from its rule and `0x1F`-delimited words into `frame`.
fn finding_headline<'f>(listed: &Listed<'_>, frame: &'f Frame) -> &'f str {
    let Ok(message) = core::str::from_utf8(listed.words) else {
        return "(non-UTF-8 finding message)";
    };
    if crate::lints::rule_of(listed.rule).is_some() {
        return frame.alloc_str(message);
    }
    let mut args: [&str; 8] = [""; 8];
    let mut count = 0usize;
    for piece in message.split('\x1f') {
        if let Some(slot) = args.get_mut(count) {
            *slot = piece;
            count += 1;
        }
    }
    let erased = blaze_own::Erased::new(frame);
    let mut headline = AString::new_in(erased.handle());
    let mut label = AString::new_in(erased.handle());
    match words::say(listed.area, listed.rule, args.get(..count).unwrap_or(&[]), &mut headline, &mut label) {
        Ok(_) => frame.alloc_str(headline.as_str()),
        Err(_) => frame.alloc_str(message),
    }
}

/// Collect structured CI todo items and summary counts from `shown`.
fn collect_check_items<'f>(
    cx: &Cx,
    checkout: &Dir<ReadWrite>,
    shown: &crate::print::Shown<'_>,
    checked: &crate::engine_check::Checked,
    allow_incomplete: bool,
    frame: &'f Frame,
) -> Result<(Vec<TodoItem<'f>, &'f Frame>, Summary), CliError> {
    let mut items: Vec<TodoItem<'f>, &'f Frame> = Vec::new_in(frame);
    let mut summary = Summary::default();

    // Group listed indices by file path so each file is read at most once into a scratch frame.
    let mut indices: Vec<usize, &Frame> = Vec::with_capacity_in(shown.listed.len(), frame);
    for i in 0..shown.listed.len() {
        indices.push(i);
    }
    indices.sort_by_key(|&i| shown.listed.get(i).and_then(|l| l.path));

    let mut scratch = Frame::with_reserve(CI_FILE_FRAME, bstd::mem::Tag::Other)
        .map_err(|_| CliError::Text { what: "blaze ci: could not reserve file frame" })?;
    let policy = words::Policy { borrows_are_errors: crate::body_types::BORROWS_ARE_ERRORS };

    for run in indices.chunk_by(|&a, &b| {
        shown.listed.get(a).and_then(|l| l.path) == shown.listed.get(b).and_then(|l| l.path)
    }) {
        let path_opt = run.first().and_then(|&i| shown.listed.get(i)).and_then(|l| l.path);

        scratch.reset();
        let mut bytes = Bytes::new_in(&scratch, "blaze ci file");
        let text_opt: Option<&str> = match path_opt {
            Some(path) => match Path::new(path).ok().and_then(|p| checkout.read_all(cx, &p, &mut bytes).ok()) {
                Some(_) => core::str::from_utf8(bytes.as_slice()).ok(),
                None => None,
            },
            None => None,
        };
        let marks = text_opt.map_or(blaze_lint::RootMarks::NONE, |t| crate::lints::marks_of(t, t));

        for &idx in run {
            let Some(l) = shown.listed.get(idx) else {
                continue;
            };
            if l.area == Area::Cli && l.rule == crate::work::NOT_RUN {
                continue;
            }
            let lint_level = crate::lints::rule_of(l.rule).map(|_| crate::lints::level_in(shown.lints, marks, l.rule));
            if lint_level == Some(blaze_lint::Level::Off) {
                continue;
            }
            let word_kind = match lint_level {
                Some(_) => None,
                None => words::wording(l.area, l.rule).map(|w| w.kind),
            };
            // Skip informational/consequence entries and untrusted borrow notes.
            if matches!(word_kind, Some(WordKind::Told | WordKind::Waited)) {
                continue;
            }
            if !policy.borrows_are_errors && matches!(word_kind, Some(WordKind::Borrows | WordKind::BorrowsUnchecked)) {
                continue;
            }
            let kind = if l.area == Area::Cli && l.rule == crate::unused_deps::UNUSED_DEPENDENCY {
                summary.unused_deps = summary.unused_deps.saturating_add(1);
                ItemKind::UnusedDependency
            } else if matches!(word_kind, Some(WordKind::Unchecked | WordKind::Grouped)) {
                summary.incomplete = summary.incomplete.saturating_add(1);
                if allow_incomplete {
                    continue;
                }
                ItemKind::Incomplete
            } else {
                let sev = match (lint_level, word_kind) {
                    (Some(blaze_lint::Level::Error), _) => Severity::Error,
                    (Some(_), _) => Severity::Warning,
                    (None, Some(k)) => k.severity(policy),
                    (None, None) => Severity::Error,
                };
                match sev {
                    Severity::Error => {
                        summary.errors = summary.errors.saturating_add(1);
                        ItemKind::Error
                    }
                    Severity::Warning => {
                        summary.warnings = summary.warnings.saturating_add(1);
                        ItemKind::Warning
                    }
                    Severity::Note => continue,
                }
            };

            let file = match path_opt {
                Some(p) => frame.alloc_str(p),
                None => "",
            };
            let (line, col) = match text_opt {
                Some(t) => line_col(t, l.start),
                None => (0, 0),
            };
            let code = frame.alloc_str(l.code());
            let message = finding_headline(l, frame);
            let suggestion = match (text_opt, l.fix) {
                (Some(t), Some(change)) => suggestion_of(t, change, frame),
                _ => None,
            };
            let command: Option<&'f str> = if l.fix.is_some() && l.id.is_shown() {
                let cmd = format_in!(frame, "blaze check --fix {}", l.id.as_str());
                Some(&*frame.alloc_str(cmd.as_str()))
            } else {
                None
            };
            items.push(TodoItem { kind, file, line, col, code, message, suggestion, command });
        }
    }

    // Account for unchecked items that were counted via `shown.behind` / `shown.not_run` /
    // `UNCHECKED_ITEMS` status even if not listed individually.
    let extra_incomplete = shown.behind.items.saturating_add(u32::try_from(shown.not_run.len()).unwrap_or(u32::MAX));
    if extra_incomplete > 0 && summary.incomplete == 0 {
        summary.incomplete = extra_incomplete;
        if !allow_incomplete {
            let msg = format_in!(frame, "{extra_incomplete} item(s) could not be checked");
            items.push(TodoItem {
                kind: ItemKind::Incomplete,
                file: "",
                line: 0,
                col: 0,
                code: "unchecked-items",
                message: frame.alloc_str(msg.as_str()),
                suggestion: None,
                command: Some("blaze check --each"),
            });
        }
    } else if checked.status == crate::UNCHECKED_ITEMS && summary.incomplete == 0 {
        summary.incomplete = 1;
        if !allow_incomplete {
            items.push(TodoItem {
                kind: ItemKind::Incomplete,
                file: "",
                line: 0,
                col: 0,
                code: "unchecked-items",
                message: "blaze check left items unchecked",
                suggestion: None,
                command: Some("blaze check --each"),
            });
        }
    } else if checked.status == Status::FAILED && summary.errors == 0 && summary.incomplete == 0 {
        summary.errors = 1;
        items.push(TodoItem {
            kind: ItemKind::Error,
            file: "",
            line: 0,
            col: 0,
            code: "check-failed",
            message: "blaze check exited with failure status",
            suggestion: None,
            command: Some("blaze check"),
        });
    }

    // Deterministic ordering (rule 25).
    items.sort_unstable_by(|a, b| {
        (a.kind, a.file, a.line, a.col, a.code, a.message).cmp(&(b.kind, b.file, b.line, b.col, b.code, b.message))
    });
    Ok((items, summary))
}

/// Escape a string for JSON (with enclosing quotes).
fn write_json_str(out: &mut String<&Frame>, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (u32::from(c)) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Build the structured [`bstd::data::Value`] for a `blaze ci` report.
///
/// Absent optional fields (`file` when empty, `line`/`col` when 0, `suggestion` and `command`
/// when `None`) are omitted from each row map so [`bstd::tab`] emits compact sparse union-schema
/// tables using `_` instead of repeating `null`.
fn build_report_value<'a>(
    items: &[TodoItem<'_>],
    summary: &Summary,
    allow_incomplete: bool,
    alloc: blaze_own::Alloc<'a>,
) -> bstd::data::Value<blaze_own::Alloc<'a>> {
    use bstd::data::{Int, Map, Value};

    let status = if summary.passed(allow_incomplete) { "passed" } else { "failed" };

    let mut sum_map = Map::new_in(alloc);
    sum_map.insert(String::from_str_in("errors", alloc), Value::Int(Int::from_u64(u64::from(summary.errors))));
    sum_map.insert(String::from_str_in("warnings", alloc), Value::Int(Int::from_u64(u64::from(summary.warnings))));
    sum_map.insert(
        String::from_str_in("unused_deps", alloc),
        Value::Int(Int::from_u64(u64::from(summary.unused_deps))),
    );
    sum_map.insert(String::from_str_in("format", alloc), Value::Int(Int::from_u64(u64::from(summary.format))));
    sum_map.insert(
        String::from_str_in("test_failures", alloc),
        Value::Int(Int::from_u64(u64::from(summary.test_failures))),
    );
    sum_map.insert(
        String::from_str_in("incomplete", alloc),
        Value::Int(Int::from_u64(u64::from(summary.incomplete))),
    );

    let mut todo = Vec::with_capacity_in(items.len(), alloc);
    for item in items.iter() {
        let mut row = Map::new_in(alloc);
        row.insert(String::from_str_in("kind", alloc), Value::Text(String::from_str_in(item.kind.name(), alloc)));
        if !item.file.is_empty() {
            row.insert(String::from_str_in("file", alloc), Value::Text(String::from_str_in(item.file, alloc)));
        }
        if item.line > 0 {
            row.insert(String::from_str_in("line", alloc), Value::Int(Int::from_u64(u64::from(item.line))));
        }
        if item.col > 0 {
            row.insert(String::from_str_in("col", alloc), Value::Int(Int::from_u64(u64::from(item.col))));
        }
        row.insert(String::from_str_in("code", alloc), Value::Text(String::from_str_in(item.code, alloc)));
        row.insert(String::from_str_in("message", alloc), Value::Text(String::from_str_in(item.message, alloc)));
        if let Some(s) = item.suggestion {
            let mut sug = Map::new_in(alloc);
            sug.insert(
                String::from_str_in("start_line", alloc),
                Value::Int(Int::from_u64(u64::from(s.start_line))),
            );
            sug.insert(String::from_str_in("end_line", alloc), Value::Int(Int::from_u64(u64::from(s.end_line))));
            sug.insert(
                String::from_str_in("replacement", alloc),
                Value::Text(String::from_str_in(s.replacement, alloc)),
            );
            row.insert(String::from_str_in("suggestion", alloc), Value::Map(sug));
        }
        if let Some(cmd) = item.command {
            row.insert(String::from_str_in("command", alloc), Value::Text(String::from_str_in(cmd, alloc)));
        }
        todo.push(Value::Map(row));
    }

    let mut root = Map::new_in(alloc);
    root.insert(String::from_str_in("schema", alloc), Value::Int(Int::from_u64(1)));
    root.insert(String::from_str_in("status", alloc), Value::Text(String::from_str_in(status, alloc)));
    root.insert(String::from_str_in("summary", alloc), Value::Map(sum_map));
    root.insert(String::from_str_in("todo", alloc), Value::List(todo));
    Value::Map(root)
}

/// Serialize the CI report as compact, token-efficient Blaze Tabular Data (`bstd::tab`).
///
/// Uses [`bstd::tab::write_pretty`] over [`build_report_value`] so non-empty `todo` lists compress
/// into homogeneous or sparse union-schema tables (`[[cols...]; [row...]]`) with `_` for omitted
/// optional fields and unquoted bare paths.
pub(crate) fn render_btab<'f>(
    items: &[TodoItem<'_>],
    summary: &Summary,
    allow_incomplete: bool,
    frame: &'f Frame,
) -> Result<String<&'f Frame>, CliError> {
    let erased = blaze_own::Erased::new(frame);
    let alloc = erased.handle();
    let value = build_report_value(items, summary, allow_incomplete, alloc);
    let written = bstd::tab::write_pretty(&value, alloc)
        .map_err(|_| CliError::Text { what: "blaze ci: failed to serialize todo.btab" })?;
    let mut out = String::with_capacity_in(written.len().saturating_add(1), frame);
    out.push_str(written.as_str());
    out.push('\n');
    Ok(out)
}

/// Serialize the CI report as standard deterministic JSON.
pub(crate) fn render_json<'f>(
    items: &[TodoItem<'_>],
    summary: &Summary,
    allow_incomplete: bool,
    frame: &'f Frame,
) -> String<&'f Frame> {
    let status = if summary.passed(allow_incomplete) { "passed" } else { "failed" };
    let mut out = String::new_in(frame);
    let _ = writeln!(out, "{{");
    let _ = writeln!(out, "  \"schema\": 1,");
    let _ = writeln!(out, "  \"status\": \"{status}\",");
    let _ = writeln!(
        out,
        "  \"summary\": {{ \"errors\": {}, \"warnings\": {}, \"unused_deps\": {}, \"format\": {}, \"test_failures\": {}, \"incomplete\": {} }},",
        summary.errors, summary.warnings, summary.unused_deps, summary.format, summary.test_failures, summary.incomplete
    );
    if items.is_empty() {
        let _ = writeln!(out, "  \"todo\": []");
    } else {
        let _ = writeln!(out, "  \"todo\": [");
        for (i, item) in items.iter().enumerate() {
            out.push_str("    {\"kind\":");
            write_json_str(&mut out, item.kind.name());
            out.push_str(",\"file\":");
            write_json_str(&mut out, item.file);
            let _ = write!(out, ",\"line\":{},\"col\":{},\"code\":", item.line, item.col);
            write_json_str(&mut out, item.code);
            out.push_str(",\"message\":");
            write_json_str(&mut out, item.message);
            out.push_str(",\"suggestion\":");
            match item.suggestion {
                Some(s) => {
                    let _ = write!(out, "{{\"start_line\":{},\"end_line\":{},\"replacement\":", s.start_line, s.end_line);
                    write_json_str(&mut out, s.replacement);
                    out.push('}');
                }
                None => out.push_str("null"),
            }
            out.push_str(",\"command\":");
            match item.command {
                Some(cmd) => write_json_str(&mut out, cmd),
                None => out.push_str("null"),
            }
            out.push('}');
            if i + 1 < items.len() {
                out.push(',');
            }
            out.push('\n');
        }
        let _ = writeln!(out, "  ]");
    }
    let _ = writeln!(out, "}}");
    out
}

/// Escape message data per GitHub Actions workflow command spec (`%`, `\r`, `\n`).
pub(crate) fn escape_github_data<'f>(s: &str, frame: &'f Frame) -> String<&'f Frame> {
    let mut out = String::with_capacity_in(s.len(), frame);
    for ch in s.chars() {
        match ch {
            '%' => out.push_str("%25"),
            '\r' => out.push_str("%0D"),
            '\n' => out.push_str("%0A"),
            c => out.push(c),
        }
    }
    out
}

/// Escape property value per GitHub Actions workflow command spec (`%`, `\r`, `\n`, `:`, `,`).
pub(crate) fn escape_github_prop<'f>(s: &str, frame: &'f Frame) -> String<&'f Frame> {
    let mut out = String::with_capacity_in(s.len(), frame);
    for ch in s.chars() {
        match ch {
            '%' => out.push_str("%25"),
            '\r' => out.push_str("%0D"),
            '\n' => out.push_str("%0A"),
            ':' => out.push_str("%3A"),
            ',' => out.push_str("%2C"),
            c => out.push(c),
        }
    }
    out
}

/// Format one GitHub Actions workflow annotation line (`::error ...` / `::warning ...`).
pub(crate) fn format_github_annotation<'f>(item: &TodoItem<'_>, frame: &'f Frame) -> String<&'f Frame> {
    let level = item.kind.github_level();
    let title = escape_github_prop(item.code, frame);
    let msg = escape_github_data(item.message, frame);
    let mut out = String::new_in(frame);
    let _ = write!(out, "::{level} ");
    if !item.file.is_empty() {
        let file = escape_github_prop(item.file, frame);
        let _ = write!(out, "file={},", file.as_str());
        if item.line > 0 {
            let _ = write!(out, "line={},", item.line);
            if item.col > 0 {
                let _ = write!(out, "col={},", item.col);
            }
        }
    }
    let _ = write!(out, "title={}::{}", title.as_str(), msg.as_str());
    out
}

/// Render `.blaze/ci/github-review.json` with ready-to-post GitHub PR review comments (including
/// GitHub `suggestion` fenced code blocks) for every item that carries a concrete [`Suggestion`].
pub(crate) fn render_github_review<'f>(
    items: &[TodoItem<'_>],
    summary: &Summary,
    allow_incomplete: bool,
    frame: &'f Frame,
) -> String<&'f Frame> {
    let mut out = String::new_in(frame);
    let passed = summary.passed(allow_incomplete);
    let _ = writeln!(out, "{{");
    let _ = writeln!(out, "  \"event\": \"COMMENT\",");
    let body = if passed {
        format_in!(frame, "blaze ci passed (0 issues)")
    } else {
        format_in!(
            frame,
            "blaze ci found {} error(s), {} warning(s), {} unused dep(s), {} test failure(s)",
            summary.errors, summary.warnings, summary.unused_deps, summary.test_failures
        )
    };
    out.push_str("  \"body\": ");
    write_json_str(&mut out, body.as_str());
    out.push_str(",\n");

    let mut review_items: Vec<&TodoItem<'_>, &Frame> = Vec::new_in(frame);
    for item in items.iter() {
        if !item.file.is_empty() && item.suggestion.is_some() {
            review_items.push(item);
        }
    }
    if review_items.is_empty() {
        let _ = writeln!(out, "  \"comments\": []");
    } else {
        let _ = writeln!(out, "  \"comments\": [");
        for (i, item) in review_items.iter().enumerate() {
            if let Some(sug) = item.suggestion {
                out.push_str("    {\"path\":");
                write_json_str(&mut out, item.file);
                if sug.start_line < sug.end_line {
                    let _ = write!(out, ",\"start_line\":{},\"start_side\":\"RIGHT\"", sug.start_line);
                }
                let _ = write!(out, ",\"line\":{},\"side\":\"RIGHT\",\"body\":", sug.end_line);
                let mut comment_body = String::new_in(frame);
                let _ = write!(comment_body, "**{}**: {}\n\n```suggestion\n", item.code, item.message);
                if !sug.replacement.is_empty() {
                    comment_body.push_str(sug.replacement);
                    comment_body.push('\n');
                }
                comment_body.push_str("```");
                write_json_str(&mut out, comment_body.as_str());
                out.push('}');
                if i + 1 < review_items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
        }
        let _ = writeln!(out, "  ]");
    }
    let _ = writeln!(out, "}}");
    out
}

/// Render Markdown summary for `$GITHUB_STEP_SUMMARY`.
pub(crate) fn render_step_summary<'f>(
    items: &[TodoItem<'_>],
    summary: &Summary,
    allow_incomplete: bool,
    elapsed_ms: u64,
    todo_path: Option<&str>,
    frame: &'f Frame,
) -> String<&'f Frame> {
    let passed = summary.passed(allow_incomplete);
    let badge = if passed { "PASSED" } else { "FAILED" };
    let check_ok = summary.errors == 0 && summary.warnings == 0 && (allow_incomplete || summary.incomplete == 0);
    let udeps_ok = summary.unused_deps == 0;
    let test_ok = summary.test_failures == 0;

    let mut out = String::new_in(frame);
    let _ = writeln!(out, "## blaze ci: {badge} ({elapsed_ms} ms)\n");
    let _ = writeln!(out, "| Phase | Status | Details |");
    let _ = writeln!(out, "| --- | --- | --- |");
    let _ = writeln!(
        out,
        "| `check` | {} | {} error(s), {} warning(s), {} incomplete |",
        if check_ok { "pass" } else { "fail" },
        summary.errors,
        summary.warnings,
        summary.incomplete
    );
    let _ = writeln!(
        out,
        "| `unused-deps` | {} | {} unused dependency(ies) |",
        if udeps_ok { "pass" } else { "fail" },
        summary.unused_deps
    );
    if summary.tests_ran {
        let _ = writeln!(
            out,
            "| `test` | {} | {} passed, {} failed/unrun, {} ignored |",
            if test_ok { "pass" } else { "fail" },
            summary.tests_passed,
            summary.test_failures,
            summary.tests_ignored
        );
    } else {
        let _ = writeln!(out, "| `test` | skipped | not run |");
    }
    if let Some(path) = todo_path {
        out.push_str("\nTodo artifact: `");
        out.push_str(path);
        let _ = writeln!(out, "` ({} item(s))", items.len());
    }
    if !items.is_empty() {
        let _ = writeln!(out, "\n### Action items\n");
        let cap = items.len().min(25);
        for item in items.iter().take(cap) {
            if !item.file.is_empty() && item.line > 0 {
                let _ = write!(out, "- **{}** `", item.kind.name());
                let _ = writeln!(out, "{}:{}:{}` [{}]: {}", item.file, item.line, item.col, item.code, item.message);
            } else {
                let _ = writeln!(out, "- **{}** [{}]: {}", item.kind.name(), item.code, item.message);
            }
        }
        if items.len() > cap {
            let _ = writeln!(out, "- ... and {} more item(s) in todo artifact", items.len() - cap);
        }
    }
    out
}

/// Write bytes to `path` (checkout-relative or absolute via `fs.home`), optionally appending.
fn write_output_file(
    cx: &Cx,
    fs: &Fs,
    checkout: &Dir<ReadWrite>,
    raw_path: &str,
    bytes: &[u8],
    append: bool,
) -> Result<(), CliError> {
    if raw_path.starts_with('/') {
        let trimmed = raw_path.trim_end_matches('/');
        let Some((dir_part, file_part)) = trimmed.rsplit_once('/') else {
            return Err(CliError::Text { what: "invalid absolute output path" });
        };
        if file_part.is_empty() {
            return Err(CliError::Text { what: "output path names a directory" });
        }
        let (base_abs, folder_rel) = match dir_part.trim_end_matches('/').rsplit_once('/') {
            Some((parent, leaf_dir)) if !leaf_dir.is_empty() => {
                (if parent.is_empty() { "/" } else { parent }, leaf_dir)
            }
            _ => return Err(CliError::Text { what: "absolute output path must lie inside a directory" }),
        };
        let dir = fs
            .home(cx, base_abs, &Path::new(folder_rel)?)
            .map_err(|_| CliError::Text { what: "could not open directory for CI output file" })?;
        let leaf = Path::new(file_part)?;
        if append {
            let file = match dir.open(cx, &leaf) {
                Ok(f) => f,
                Err(_) => dir.create(cx, &leaf)?,
            };
            file.append(cx, bytes, Durable::No)?;
            file.close(cx, Durable::No)?;
        } else {
            dir.publish(cx, &leaf, Content::bytes(bytes), Durable::No)?;
        }
        return Ok(());
    }
    if raw_path.starts_with(".blaze/") {
        crate::print::ensure_checkout_ignore(cx, checkout)?;
    }
    let rel = Path::new(raw_path)?;
    if append {
        let file = match checkout.open(cx, &rel) {
            Ok(f) => f,
            Err(_) => checkout.create(cx, &rel)?,
        };
        file.append(cx, bytes, Durable::No)?;
        file.close(cx, Durable::No)?;
    } else {
        checkout.publish(cx, &rel, Content::bytes(bytes), Durable::No)?;
    }
    Ok(())
}

/// Entry point for `blaze ci`.
#[expect(clippy::too_many_arguments, reason = "top-level command edge: each argument one specific capability")]
pub(crate) fn run(
    cx: &Cx,
    args: &bstd::args::Args,
    fs: &Fs,
    env: Option<&bstd::env::Env>,
    net: Option<&bstd::net::Net>,
    host: &bstd::host::Host,
    clock: &bstd::clock::Clock,
    rng: &mut bstd::rng::Rng,
    out: &Out,
    err: &Out,
    phases: &mut Phases,
    heap: &'static blaze_own::Heap,
) -> Result<Status, CliError> {
    let ci_args = parse_ci_args(args)?;
    let started = clock.now();

    let env_var = |name: &str| -> Option<&str> {
        env?.get(&VarName::new(name).ok()?).ok().flatten()
    };
    let in_github = env_var("GITHUB_ACTIONS") == Some("true");
    let github_mode = match ci_args.style {
        OutputStyle::GitHub => true,
        OutputStyle::Plain => false,
        OutputStyle::Auto => in_github,
    };
    let step_summary_path = env_var("GITHUB_STEP_SUMMARY").filter(|s| !s.is_empty());

    let mut flags = crate::Flags::empty();
    flags.dir = ci_args.dir;
    flags.workers = ci_args.workers;
    flags.color = ci_args.color;
    flags.view = Some(blaze_report::frame::plain::View::Agent);

    let todo_path = ci_args.resolved_todo_path();
    let todo_format = ci_args.resolved_todo_format();

    crate::engine_check::check_for_ci(
        cx,
        None,
        fs,
        env,
        net,
        host,
        rng,
        out,
        err,
        phases,
        heap,
        &flags,
        ci_args.fix,
        |ci, phases, rng| {
            let frame = ci.frame;
            let (mut items, mut summary) = collect_check_items(cx, ci.checkout, ci.shown, ci.checked, ci_args.allow_incomplete, frame)?;
            summary.fixes_applied = ci.fixes_applied;
            summary.files_fixed = ci.files_fixed;

            // Phase 1 output (check + unused-deps).
            if github_mode {
                out.line(cx, "::group::blaze check")?;
                if summary.fixes_applied > 0 {
                    out.line(cx, format_args!("applied {} fix(es) in {} file(s)", summary.fixes_applied, summary.files_fixed))?;
                }
                for item in items.iter() {
                    let ann = format_github_annotation(item, frame);
                    out.line(cx, ann.as_str())?;
                }
                out.line(cx, "::endgroup::")?;
            } else {
                if summary.fixes_applied > 0 {
                    out.line(cx, format_args!("fixed: {} fix(es) in {} file(s)", summary.fixes_applied, summary.files_fixed))?;
                }
                for item in items.iter() {
                    if !item.file.is_empty() && item.line > 0 {
                        out.line(cx, format_args!("{} {}:{}:{} [{}] {}", item.kind.name(), item.file, item.line, item.col, item.code, item.message))?;
                    } else {
                        out.line(cx, format_args!("{} [{}] {}", item.kind.name(), item.code, item.message))?;
                    }
                }
            }

            // Phase 2: tests (unless --no-test or check failed before UnitGraph).
            if !ci_args.no_test
                && let Some(standing) = ci.standing
            {
                if github_mode {
                    out.line(cx, "::group::blaze test")?;
                }
                let select = crate::run_package::Select { member: ci.member, ..crate::run_package::Select::NONE };
                let packages = crate::run_package::test_packages(cx, ci.checkout, select, frame)?;
                let launch = crate::program::Launch {
                    fs,
                    env,
                    host,
                    out,
                    err,
                    select,
                    open: blaze_run::viewer::OpenExplore::No,
                    grants: &[],
                    args: &[],
                };
                let silent = blaze_screen::Unwatched;
                let test_res = crate::engine_start::run_tests_status(
                    cx, standing, &packages, None, &launch, clock, rng, phases, &silent,
                );
                if github_mode {
                    out.line(cx, "::endgroup::")?;
                }
                match test_res {
                    Ok(ts) => {
                        summary.tests_ran = true;
                        summary.tests_passed = ts.passed;
                        summary.tests_ignored = ts.ignored;
                        let tf = ts.failed.saturating_add(ts.not_run);
                        summary.test_failures = tf;
                        if ts.failed > 0 {
                            let msg = format_in!(frame, "{} test(s) failed", ts.failed);
                            let item = TodoItem {
                                kind: ItemKind::TestFailure,
                                file: "",
                                line: 0,
                                col: 0,
                                code: "test-failed",
                                message: frame.alloc_str(msg.as_str()),
                                suggestion: None,
                                command: Some("blaze test"),
                            };
                            if github_mode {
                                let ann = format_github_annotation(&item, frame);
                                out.line(cx, ann.as_str())?;
                            }
                            items.push(item);
                        }
                        if ts.not_run > 0 {
                            let msg = format_in!(frame, "{} test(s) could not be run", ts.not_run);
                            let item = TodoItem {
                                kind: ItemKind::TestFailure,
                                file: "",
                                line: 0,
                                col: 0,
                                code: "test-not-run",
                                message: frame.alloc_str(msg.as_str()),
                                suggestion: None,
                                command: Some("blaze test"),
                            };
                            if github_mode {
                                let ann = format_github_annotation(&item, frame);
                                out.line(cx, ann.as_str())?;
                            }
                            items.push(item);
                        }
                    }
                    Err(error) => {
                        summary.tests_ran = true;
                        summary.test_failures = summary.test_failures.saturating_add(1);
                        let msg = format_in!(frame, "test runner error: {error}");
                        let item = TodoItem {
                            kind: ItemKind::TestFailure,
                            file: "",
                            line: 0,
                            col: 0,
                            code: "test-runner-error",
                            message: frame.alloc_str(msg.as_str()),
                            suggestion: None,
                            command: Some("blaze test"),
                        };
                        if github_mode {
                            let ann = format_github_annotation(&item, frame);
                            out.line(cx, ann.as_str())?;
                        } else {
                            out.line(cx, format_args!("test_failure [test-runner-error] {}", item.message))?;
                        }
                        items.push(item);
                    }
                }
            }

            let elapsed_ms = clock
                .now()
                .since(started)
                .map_or(0u64, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));

            // Write todo artifact (btab default, or JSON).
            if let Some(path) = todo_path {
                let text = match todo_format {
                    TodoKindFormat::Btab => render_btab(&items, &summary, ci_args.allow_incomplete, frame)?,
                    TodoKindFormat::Json => render_json(&items, &summary, ci_args.allow_incomplete, frame),
                };
                write_output_file(cx, fs, ci.checkout, path, text.as_bytes(), false)?;
            }

            // Write GitHub PR review JSON when in GitHub mode.
            if github_mode {
                let review = render_github_review(&items, &summary, ci_args.allow_incomplete, frame);
                write_output_file(cx, fs, ci.checkout, DEFAULT_REVIEW_JSON, review.as_bytes(), false)?;
            }

            // Append Markdown step summary when $GITHUB_STEP_SUMMARY is set.
            if let Some(summary_path) = step_summary_path {
                let md = render_step_summary(&items, &summary, ci_args.allow_incomplete, elapsed_ms, todo_path, frame);
                write_output_file(cx, fs, ci.checkout, summary_path, md.as_bytes(), true)?;
            }

            // Final concise verdict line.
            let passed = summary.passed(ci_args.allow_incomplete);
            if let Some(path) = todo_path {
                out.line(cx, format_args!("todo: {path} ({} item(s))", items.len()))?;
            }
            if passed {
                if summary.tests_ran {
                    out.line(
                        cx,
                        format_args!(
                            "blaze ci: PASSED (0 errors, 0 warnings, 0 unused deps, {} test(s) passed; {elapsed_ms} ms)",
                            summary.tests_passed
                        ),
                    )?;
                } else {
                    out.line(cx, format_args!("blaze ci: PASSED (0 errors, 0 warnings, 0 unused deps; {elapsed_ms} ms)"))?;
                }
                Ok(Status::OK)
            } else {
                out.line(
                    cx,
                    format_args!(
                        "blaze ci: FAILED ({} error(s), {} warning(s), {} unused dep(s), {} test failure(s), {} incomplete; {elapsed_ms} ms)",
                        summary.errors, summary.warnings, summary.unused_deps, summary.test_failures, summary.incomplete
                    ),
                )?;
                Ok(Status::FAILED)
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_frame() -> Frame {
        Frame::with_reserve(4 << 20, bstd::mem::Tag::Other).ok().unwrap_or_else(|| {
            // Unreachable on any host with 4 MiB address space.
            loop {}
        })
    }

    #[test]
    fn btab_and_json_serializers_emit_valid_compact_output() {
        let frame = test_frame();
        let summary = Summary {
            errors: 1,
            warnings: 0,
            unused_deps: 1,
            format: 0,
            test_failures: 1,
            incomplete: 0,
            tests_passed: 0,
            tests_ignored: 0,
            tests_ran: true,
            fixes_applied: 0,
            files_fixed: 0,
        };
        let items = [
            TodoItem {
                kind: ItemKind::Error,
                file: "app/src/lib.rs",
                line: 4,
                col: 18,
                code: "E0308",
                message: "mismatched types: expected u32, found T",
                suggestion: None,
                command: None,
            },
            TodoItem {
                kind: ItemKind::UnusedDependency,
                file: "app/Cargo.toml",
                line: 7,
                col: 1,
                code: "unused-dependency",
                message: "dependency unused_dep is never referenced",
                suggestion: Some(Suggestion { start_line: 7, end_line: 7, replacement: "" }),
                command: Some("blaze check --fix 23456"),
            },
            TodoItem {
                kind: ItemKind::TestFailure,
                file: "",
                line: 0,
                col: 0,
                code: "test-failed",
                message: "1 test(s) failed",
                suggestion: None,
                command: Some("blaze test"),
            },
        ];

        let btab = render_btab(&items, &summary, false, &frame).expect("render_btab failed");
        let btab_s = btab.as_str();
        assert!(btab_s.contains("schema: 1"), "missing schema:\n{btab_s}");
        assert!(btab_s.contains("status: failed"), "missing status:\n{btab_s}");
        assert!(btab_s.contains("todo: [["), "expected compact table syntax:\n{btab_s}");
        assert!(btab_s.contains("];"), "expected table header terminator:\n{btab_s}");
        assert!(btab_s.contains("app/Cargo.toml"), "expected bare path:\n{btab_s}");
        assert!(btab_s.contains("app/src/lib.rs"), "expected bare path:\n{btab_s}");
        assert!(!btab_s.contains("\"app/Cargo.toml\""), "path should be bare, not quoted:\n{btab_s}");
        assert!(btab_s.contains(" _ "), "expected absent-field sentinel `_` in sparse rows:\n{btab_s}");
        assert!(!btab_s.contains("null"), "omitted optional fields must not emit null:\n{btab_s}");

        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        let expected = build_report_value(&items, &summary, false, alloc);
        let parsed = bstd::tab::read(btab_s.as_bytes(), bstd::tab::Limits::SHIPPED, alloc)
            .expect("todo.btab must round-trip via bstd::tab::read");
        assert_eq!(parsed, expected);

        let json = render_json(&items, &summary, false, &frame);
        let json_s = json.as_str();
        assert!(json_s.contains("\"schema\": 1"));
        assert!(json_s.contains("\"status\": \"failed\""));
        assert!(json_s.contains("\"unused_deps\": 1"));
        assert!(json_s.contains("\"kind\":\"unused_dependency\""));
        assert!(json_s.contains("\"replacement\":\"\""));

        // Empty passed report round-trips cleanly too.
        let clean = Summary::default();
        let btab_clean = render_btab(&[], &clean, false, &frame).expect("clean render_btab failed");
        assert!(btab_clean.as_str().contains("status: passed"));
        assert!(btab_clean.as_str().contains("todo: []"));
        let expected_clean = build_report_value(&[], &clean, false, alloc);
        let parsed_clean = bstd::tab::read(btab_clean.as_bytes(), bstd::tab::Limits::SHIPPED, alloc)
            .expect("clean todo.btab must round-trip via bstd::tab::read");
        assert_eq!(parsed_clean, expected_clean);
    }

    #[test]
    fn todo_format_defaults_to_btab_and_infers_json_from_extension() {
        assert_eq!(parse_todo_format("btab").ok(), Some(TodoKindFormat::Btab));
        assert_eq!(parse_todo_format("tab").ok(), Some(TodoKindFormat::Btab));
        assert_eq!(parse_todo_format("json").ok(), Some(TodoKindFormat::Json));
        assert!(parse_todo_format("nuon").is_err());

        let default_args = CiArgs::empty();
        assert_eq!(default_args.resolved_todo_format(), TodoKindFormat::Btab);
        assert_eq!(default_args.resolved_todo_path(), Some(".blaze/ci/todo.btab"));

        let mut json_ext = CiArgs::empty();
        json_ext.todo_path = Some("out/report.json");
        assert_eq!(json_ext.resolved_todo_format(), TodoKindFormat::Json);
        assert_eq!(json_ext.resolved_todo_path(), Some("out/report.json"));

        let mut custom_btab = CiArgs::empty();
        custom_btab.todo_path = Some("out/report.btab");
        assert_eq!(custom_btab.resolved_todo_format(), TodoKindFormat::Btab);
        assert_eq!(custom_btab.resolved_todo_path(), Some("out/report.btab"));
    }

    #[test]
    fn github_annotation_and_review_json_are_properly_escaped() {
        let frame = test_frame();
        let item = TodoItem {
            kind: ItemKind::Error,
            file: "a/b.rs",
            line: 10,
            col: 5,
            code: "types",
            message: "100% wrong\nsecond line",
            suggestion: Some(Suggestion { start_line: 10, end_line: 10, replacement: "let x: u32 = 1;" }),
            command: Some("blaze check --fix 23456"),
        };
        let ann = format_github_annotation(&item, &frame);
        assert_eq!(ann.as_str(), "::error file=a/b.rs,line=10,col=5,title=types::100%25 wrong%0Asecond line");

        let summary = Summary { errors: 1, ..Summary::default() };
        let review = render_github_review(&[item], &summary, false, &frame);
        assert!(review.as_str().contains("\"path\":\"a/b.rs\""));
        assert!(review.as_str().contains("\"line\":10,\"side\":\"RIGHT\""));
        assert!(review.as_str().contains("```suggestion\\nlet x: u32 = 1;\\n```"));
    }

    #[test]
    fn step_summary_markdown_contains_table_and_items() {
        let frame = test_frame();
        let summary = Summary {
            errors: 0,
            warnings: 0,
            unused_deps: 1,
            format: 0,
            test_failures: 0,
            incomplete: 0,
            tests_passed: 3,
            tests_ignored: 0,
            tests_ran: true,
            fixes_applied: 0,
            files_fixed: 0,
        };
        let item = TodoItem {
            kind: ItemKind::UnusedDependency,
            file: "Cargo.toml",
            line: 8,
            col: 1,
            code: "unused-dependency",
            message: "unused dep foo",
            suggestion: None,
            command: None,
        };
        let md = render_step_summary(&[item], &summary, false, 42, Some(".blaze/ci/todo.btab"), &frame);
        assert!(md.as_str().contains("## blaze ci: FAILED (42 ms)"));
        assert!(md.as_str().contains("| `unused-deps` | fail | 1 unused dependency(ies) |"));
        assert!(md.as_str().contains("| `test` | pass | 3 passed, 0 failed/unrun, 0 ignored |"));
        assert!(md.as_str().contains("`Cargo.toml:8:1`"));
    }
}
