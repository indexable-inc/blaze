//! Thin CLI over `astlog-core`: evaluate a rules file against paths, print
//! derived relations, emit lint findings, or apply rewrites.
#![expect(clippy::print_stdout, clippy::print_stderr, reason = "astlog CLI output")]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use astlog_core::{Analysis, Finding, Severity, SuppressedFinding, Value, one_line};
use error_chain::args::{ArgError, Args};

enum Command {
    /// Evaluate rules and print derived relations (pure inspection; `scan`
    /// is the lint gate).
    Query { rules: PathBuf, paths: Vec<PathBuf>, relation: Option<String>, json: bool },
    /// Evaluate rules and emit one finding per row of each `(lint ...)`
    /// relation, minus `astlog-ignore` suppressions. Exits nonzero when any
    /// error-severity finding survives.
    Scan { rules: PathBuf, paths: Vec<PathBuf>, json: bool, error: bool },
    /// Evaluate rules and list every finding an `astlog-ignore` comment
    /// suppresses, with the comment behind each (pure inspection; exits zero).
    Suppressions { rules: PathBuf, paths: Vec<PathBuf>, json: bool },
    /// Evaluate rules and apply `(rewrite ...)` edits.
    Fix { rules: PathBuf, paths: Vec<PathBuf>, write: bool },
}

const COMMANDS: &str = "`query`, `scan`, `suppressions`, `fix`, `help`";

const VERSION: &str = concat!("astlog ", env!("CARGO_PKG_VERSION"), "\n");

const HELP: &str = concat!(
    env!("CARGO_PKG_DESCRIPTION"),
    "

Usage: astlog <COMMAND>

Commands:
  query         Evaluate rules and print derived relations (pure inspection; `scan` is the lint
                gate)
  scan          Evaluate rules and emit one finding per row of each `(lint ...)` relation, minus
                `astlog-ignore` suppressions. Exits nonzero when any error-severity finding
                survives
  suppressions  Evaluate rules and list every finding an `astlog-ignore` comment suppresses, with
                the comment behind each (pure inspection; exits zero)
  fix           Evaluate rules and apply `(rewrite ...)` edits
  help          Print this message or the help of the given command

Options:
  -h, --help     Print help
  -V, --version  Print version
"
);

const QUERY_HELP: &str = "\
Evaluate rules and print derived relations (pure inspection; `scan` is the lint gate).

Usage: astlog query [OPTIONS] <RULES> <PATHS>...

Arguments:
  <RULES>     Rules file (`.astlog`)
  <PATHS>...  Files or directories to load (directories walk gitignore-aware)

Options:
      --relation <RELATION>  Print only this relation
      --json                 Emit JSON instead of text
  -h, --help                 Print help
";

const SCAN_HELP: &str = "\
Evaluate rules and emit one finding per row of each `(lint ...)` relation, minus `astlog-ignore`
suppressions. Exits nonzero when any error-severity finding survives.

Usage: astlog scan [OPTIONS] <RULES> [PATHS]...

Arguments:
  <RULES>     Rules file (`.astlog`)
  [PATHS]...  Files or directories to scan (default: the current directory)

Options:
      --json   Emit a JSON array of findings instead of text
      --error  Promote warnings to errors for the exit-code decision
  -h, --help   Print help
";

const SUPPRESSIONS_HELP: &str = "\
Evaluate rules and list every finding an `astlog-ignore` comment suppresses, with the comment
behind each (pure inspection; exits zero).

Usage: astlog suppressions [OPTIONS] <RULES> [PATHS]...

Arguments:
  <RULES>     Rules file (`.astlog`)
  [PATHS]...  Files or directories to scan (default: the current directory)

Options:
      --json  Emit a JSON array of suppressed findings instead of text
  -h, --help  Print help
";

const FIX_HELP: &str = "\
Evaluate rules and apply `(rewrite ...)` edits.

Usage: astlog fix [OPTIONS] <RULES> <PATHS>...

Arguments:
  <RULES>     Rules file (`.astlog`)
  <PATHS>...  Files or directories to load

Options:
      --write  Write changes to disk instead of printing the diff
  -h, --help   Print help
";

/// What the command line asks for: a command to run, or text to print (help, the version).
enum Parsed {
    Run(Command),
    Print(&'static str),
}

/// Read the command line. A `--help` (or `-h`) wins over every other refusal of its command.
fn parse(mut args: Args) -> Result<Parsed, ArgError> {
    let Some(first) = args.first() else {
        return Err(ArgError::MissingCommand { commands: COMMANDS });
    };
    let help = |args: &mut Args, text| args.switch(&["-h", "--help"]).map(|it| it.then_some(text));
    let command = match first.as_str() {
        "-h" | "--help" => return Ok(Parsed::Print(HELP)),
        "-V" | "--version" => return Ok(Parsed::Print(VERSION)),
        "help" => {
            let text = match args.positional().as_deref() {
                None => HELP,
                Some(name) => command_help(name)?,
            };
            if let Some(extra) = args.finish()?.into_iter().next() {
                return Err(ArgError::Unexpected { arg: extra });
            }
            return Ok(Parsed::Print(text));
        }
        name => command_help(name)?,
    };
    if let Some(text) = help(&mut args, command)? {
        return Ok(Parsed::Print(text));
    }
    let command = match first.as_str() {
        "query" => {
            let relation = args.value("--relation")?;
            let json = args.switch(&["--json"])?;
            let (rules, paths) = rules_and_paths(args, true)?;
            Command::Query { rules, paths, relation, json }
        }
        "scan" => {
            let json = args.switch(&["--json"])?;
            let error = args.switch(&["--error"])?;
            let (rules, paths) = rules_and_paths(args, false)?;
            Command::Scan { rules, paths, json, error }
        }
        "suppressions" => {
            let json = args.switch(&["--json"])?;
            let (rules, paths) = rules_and_paths(args, false)?;
            Command::Suppressions { rules, paths, json }
        }
        _ => {
            let write = args.switch(&["--write"])?;
            let (rules, paths) = rules_and_paths(args, true)?;
            Command::Fix { rules, paths, write }
        }
    };
    Ok(Parsed::Run(command))
}

/// The help of the command `name`, which also checks that it is one.
fn command_help(name: &str) -> Result<&'static str, ArgError> {
    match name {
        "query" => Ok(QUERY_HELP),
        "scan" => Ok(SCAN_HELP),
        "suppressions" => Ok(SUPPRESSIONS_HELP),
        "fix" => Ok(FIX_HELP),
        flag if flag.starts_with('-') => Err(ArgError::UnknownFlag { flag: flag.to_owned() }),
        _ => Err(ArgError::UnknownCommand { name: name.to_owned(), commands: COMMANDS }),
    }
}

/// The `<RULES>` positional and the paths after it, which `required` says may not be none.
fn rules_and_paths(mut args: Args, required: bool) -> Result<(PathBuf, Vec<PathBuf>), ArgError> {
    let rules = args.positional().ok_or(ArgError::Missing { what: "<RULES>" })?;
    let paths: Vec<PathBuf> = args.finish()?.into_iter().map(PathBuf::from).collect();
    if required && paths.is_empty() {
        return Err(ArgError::Missing { what: "<PATHS>..." });
    }
    Ok((PathBuf::from(rules), paths))
}

#[derive(Debug)]
enum Error {
    ReadRules { path: PathBuf, source: std::io::Error },

    /// Displays as the core error itself.
    Core { source: astlog_core::Error },

    NoRelation { name: String, available: String },

    WriteFixed { path: PathBuf, source: std::io::Error },

    Findings { count: usize },
}

impl From<astlog_core::Error> for Error {
    fn from(source: astlog_core::Error) -> Self {
        Self::Core { source }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadRules { path, .. } => write!(f, "read rules {}", path.display()),
            Self::Core { source } => std::fmt::Display::fmt(source, f),
            Self::NoRelation { name, available } => {
                write!(f, "relation `{name}` is not defined; available: {available}")
            }
            Self::WriteFixed { path, .. } => write!(f, "write {}", path.display()),
            Self::Findings { count } => write!(f, "{count} blocking finding(s)"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ReadRules { source, .. } | Self::WriteFixed { source, .. } => Some(source),
            // Transparent: the core error displays as this one, so its own source is next.
            Self::Core { source } => std::error::Error::source(source),
            Self::NoRelation { .. } | Self::Findings { .. } => None,
        }
    }
}

fn main() -> ExitCode {
    let command = match Args::from_env().and_then(parse) {
        Ok(Parsed::Run(command)) => command,
        Ok(Parsed::Print(text)) => {
            print!("{text}");
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            // A command line the command refuses exits 2, as a usage error does.
            eprintln!("astlog: {error}\n\nFor more information, try `astlog --help`.");
            return ExitCode::from(2);
        }
    };
    error_chain::main("astlog", || run(&command))
}

fn run(command: &Command) -> Result<(), Error> {
    match command {
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
                count => Err(Error::Findings { count }),
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
                    if let Err(source) = std::fs::write(&fixed.path, fixed.content) {
                        return Err(Error::WriteFixed { path: fixed.path, source });
                    }
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
    let source = std::fs::read_to_string(rules)
        .map_err(|source| Error::ReadRules { path: rules.to_path_buf(), source })?;
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
        Some(name) => Err(Error::NoRelation { name: name.to_owned(), available: all.join(", ") }),
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
        Value::Int(int) => int.to_string(),
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
        Value::Int(int) => serde_json::Value::from(*int),
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
