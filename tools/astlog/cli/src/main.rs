//! Thin CLI over `astlog-core`: evaluate a rules file against paths, print
//! derived relations, emit lint findings, or apply rewrites.
#![expect(clippy::print_stdout, clippy::print_stderr, reason = "astlog CLI output")]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use astlog_core::{Analysis, Finding, Severity, SuppressedFinding, Value, one_line};
use clap::{Parser, Subcommand};
use snafu::{ResultExt as _, Snafu};

#[derive(Parser)]
#[command(name = "astlog", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Evaluate rules and print derived relations (pure inspection; `scan`
    /// is the lint gate).
    Query {
        /// Rules file (`.astlog`).
        rules: PathBuf,
        /// Files or directories to load (directories walk gitignore-aware).
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Print only this relation.
        #[arg(long)]
        relation: Option<String>,
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Evaluate rules and emit one finding per row of each `(lint ...)`
    /// relation, minus `astlog-ignore` suppressions. Exits nonzero when any
    /// error-severity finding survives.
    Scan {
        /// Rules file (`.astlog`).
        rules: PathBuf,
        /// Files or directories to scan (default: the current directory).
        paths: Vec<PathBuf>,
        /// Emit a JSON array of findings instead of text.
        #[arg(long)]
        json: bool,
        /// Promote warnings to errors for the exit-code decision.
        #[arg(long)]
        error: bool,
    },
    /// Evaluate rules and list every finding an `astlog-ignore` comment
    /// suppresses, with the comment behind each (pure inspection; exits zero).
    Suppressions {
        /// Rules file (`.astlog`).
        rules: PathBuf,
        /// Files or directories to scan (default: the current directory).
        paths: Vec<PathBuf>,
        /// Emit a JSON array of suppressed findings instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Evaluate rules and apply `(rewrite ...)` edits.
    Fix {
        /// Rules file (`.astlog`).
        rules: PathBuf,
        /// Files or directories to load.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Write changes to disk instead of printing the diff.
        #[arg(long)]
        write: bool,
    },
}

#[derive(Debug, Snafu)]
enum Error {
    #[snafu(display("read rules {}", path.display()))]
    ReadRules { path: PathBuf, source: std::io::Error },

    #[snafu(transparent)]
    Core { source: astlog_core::Error },

    #[snafu(display("relation `{name}` is not defined; available: {available}"))]
    NoRelation { name: String, available: String },

    #[snafu(display("write {}", path.display()))]
    WriteFixed { path: PathBuf, source: std::io::Error },

    #[snafu(display("{count} blocking finding(s)"))]
    Findings { count: usize },
}

fn main() -> ExitCode {
    error_chain::main("astlog", || run(&Cli::parse()))
}

fn run(cli: &Cli) -> Result<(), Error> {
    match &cli.command {
        Command::Query { rules, paths, relation, json } => {
            let analysis = load(rules, paths)?;
            let selected = select(&analysis, relation.as_deref())?;
            if *json {
                print_json(&analysis, &selected);
            } else {
                print_text(&analysis, &selected);
            }
            Ok(())
        }
        Command::Scan { rules, paths, json, error } => {
            let paths = if paths.is_empty() { vec![PathBuf::from(".")] } else { paths.clone() };
            let analysis = load(rules, &paths)?;
            let findings = analysis.findings()?;
            if *json {
                print_findings_json(&findings);
            } else {
                print_findings_text(&findings);
            }
            match blocking_count(&findings, *error) {
                0 => Ok(()),
                count => FindingsSnafu { count }.fail(),
            }
        }
        Command::Suppressions { rules, paths, json } => {
            let paths = if paths.is_empty() { vec![PathBuf::from(".")] } else { paths.clone() };
            let analysis = load(rules, &paths)?;
            let suppressed = analysis.suppressed()?;
            if *json {
                print_suppressed_json(&suppressed);
            } else {
                print_suppressed_text(&suppressed);
            }
            Ok(())
        }
        Command::Fix { rules, paths, write } => {
            let analysis = load(rules, paths)?;
            if *write {
                for fixed in analysis.rewritten() {
                    std::fs::write(&fixed.path, fixed.content)
                        .context(WriteFixedSnafu { path: &fixed.path })?;
                }
                eprintln!("astlog: applied {} edit(s)", analysis.edits.len());
            } else {
                print!("{}", analysis.diff());
            }
            Ok(())
        }
    }
}

fn load(rules: &Path, paths: &[PathBuf]) -> Result<Analysis, Error> {
    let source = std::fs::read_to_string(rules).context(ReadRulesSnafu { path: rules })?;
    Ok(astlog_core::analyze(&source, paths)?)
}

/// How many findings gate the exit code: every error, plus every warning when
/// `--error` promotes warnings (parity with `ast-grep scan --error`).
fn blocking_count(findings: &[Finding], promote_warnings: bool) -> usize {
    findings
        .iter()
        .filter(|finding| match finding.severity {
            Severity::Error => true,
            Severity::Warning => promote_warnings,
        })
        .count()
}

fn print_findings_text(findings: &[Finding]) {
    for finding in findings {
        println!(
            "{file}:{line}:{column}: {severity}[{rule}]: {message} `{text}`",
            file = finding.file.display(),
            line = finding.line,
            column = finding.column,
            severity = finding.severity.as_str(),
            rule = finding.rule,
            message = finding.message,
            text = finding.text,
        );
    }
}

/// The `scan --json` contract consumed by CI and sibling repos: an array of
/// `{"rule","severity","message","file","line","column","endLine",
/// "endColumn","text"}` objects.
fn print_findings_json(findings: &[Finding]) {
    let rows: Vec<serde_json::Value> = findings
        .iter()
        .map(|finding| {
            serde_json::json!({
                "rule": finding.rule,
                "severity": finding.severity.as_str(),
                "message": finding.message,
                "file": finding.file,
                "line": finding.line,
                "column": finding.column,
                "endLine": finding.end_line,
                "endColumn": finding.end_column,
                "text": finding.text,
            })
        })
        .collect();
    println!("{}", serde_json::Value::Array(rows));
}

fn print_suppressed_text(suppressed: &[SuppressedFinding]) {
    for entry in suppressed {
        let finding = &entry.finding;
        // The suppressing comment is always in the finding's own file, so the
        // location reads `{file}:{commentLine}`.
        println!(
            "{file}:{line}:{column}: ignored[{rule}]: {message} `{text}` \
             (by {file}:{comment_line} `{comment_text}`)",
            file = finding.file.display(),
            line = finding.line,
            column = finding.column,
            rule = finding.rule,
            message = finding.message,
            text = finding.text,
            comment_line = entry.comment_line,
            comment_text = entry.comment_text,
        );
    }
}

/// The `suppressions --json` shape: the `scan --json` fields plus the
/// suppressing comment's line and text.
fn print_suppressed_json(suppressed: &[SuppressedFinding]) {
    let rows: Vec<serde_json::Value> = suppressed
        .iter()
        .map(|entry| {
            let finding = &entry.finding;
            serde_json::json!({
                "rule": finding.rule,
                "severity": finding.severity.as_str(),
                "message": finding.message,
                "file": finding.file,
                "line": finding.line,
                "column": finding.column,
                "endLine": finding.end_line,
                "endColumn": finding.end_column,
                "text": finding.text,
                "commentLine": entry.comment_line,
                "commentText": entry.comment_text,
            })
        })
        .collect();
    println!("{}", serde_json::Value::Array(rows));
}

fn select(analysis: &Analysis, relation: Option<&str>) -> Result<Vec<String>, Error> {
    let all: Vec<String> = analysis.database.relations.keys().cloned().collect();
    match relation {
        None => Ok(all),
        Some(name) if all.iter().any(|key| key == name) => Ok(vec![name.to_owned()]),
        Some(name) => NoRelationSnafu { name, available: all.join(", ") }.fail(),
    }
}

fn render_value(analysis: &Analysis, value: &Value) -> String {
    match value {
        Value::Node(node) => {
            let info = analysis.corpus.node_info(*node);
            let at = analysis.corpus.position(node.file, info.start);
            let path = analysis.corpus.files[node.file].path.display();
            let text = one_line(analysis.corpus.node_text(*node));
            format!("{path}:{line}:{column} `{text}`", line = at.line, column = at.column)
        }
        Value::Text(text) => format!("\"{text}\""),
    }
}

fn print_text(analysis: &Analysis, selected: &[String]) {
    for name in selected {
        let relation = &analysis.database.relations[name.as_str()];
        for row in relation.rows() {
            let cells: Vec<String> = relation
                .columns
                .iter()
                .zip(row)
                .map(|(column, value)| format!("{column} = {}", render_value(analysis, value)))
                .collect();
            println!("{name}({cells})", cells = cells.join(", "));
        }
    }
}

fn print_json(analysis: &Analysis, selected: &[String]) {
    let mut output = serde_json::Map::new();
    for name in selected {
        let relation = &analysis.database.relations[name.as_str()];
        let rows: Vec<serde_json::Value> = relation
            .rows()
            .iter()
            .map(|row| {
                let cells: serde_json::Map<String, serde_json::Value> = relation
                    .columns
                    .iter()
                    .zip(row)
                    .map(|(column, value)| (column.clone(), json_value(analysis, value)))
                    .collect();
                serde_json::Value::Object(cells)
            })
            .collect();
        output.insert(name.clone(), serde_json::Value::Array(rows));
    }
    println!("{}", serde_json::Value::Object(output));
}

fn json_value(analysis: &Analysis, value: &Value) -> serde_json::Value {
    match value {
        Value::Node(node) => {
            let info = analysis.corpus.node_info(*node);
            let at = analysis.corpus.position(node.file, info.start);
            serde_json::json!({
                "path": analysis.corpus.files[node.file].path,
                "kind": info.kind,
                "start": info.start,
                "end": info.end,
                "line": at.line,
                "column": at.column,
                "text": analysis.corpus.node_text(*node),
            })
        }
        Value::Text(text) => serde_json::Value::String(text.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use astlog_core::{Finding, Severity};

    use super::blocking_count;

    fn finding(severity: Severity) -> Finding {
        Finding {
            rule: "r".to_owned(),
            severity,
            message: "m".to_owned(),
            file: PathBuf::from("f.nix"),
            line: 1,
            column: 1,
            end_line: 1,
            end_column: 2,
            text: "t".to_owned(),
        }
    }

    #[test]
    fn errors_always_block_warnings_only_when_promoted() {
        let findings = [finding(Severity::Error), finding(Severity::Warning)];
        assert_eq!(blocking_count(&findings, false), 1);
        assert_eq!(blocking_count(&findings, true), 2);
    }

    #[test]
    fn warnings_alone_pass_without_promotion() {
        let findings = [finding(Severity::Warning)];
        assert_eq!(blocking_count(&findings, false), 0);
        assert_eq!(blocking_count(&findings, true), 1);
    }
}
