//! Lint findings: every row of a `(lint ...)`-declared relation becomes one
//! located finding, minus the rows an `astlog-ignore` comment suppresses.
//!
//! Suppression is an emission-time filter only: the underlying Datalog rows
//! still exist for joins and `query` output. A comment node (any tree-sitter
//! kind containing "comment") whose text contains `astlog-ignore` suppresses
//! findings on the lines the comment spans (trailing comment) and the line
//! immediately below it; `astlog-ignore: name1, name2` limits suppression to
//! those rule names, a bare `astlog-ignore` suppresses every rule there.

use std::path::PathBuf;

use blaze_hash::HashMap;

use crate::corpus::{Corpus, Value};
use crate::error::{Error, InternalSnafu, LintNoNodeSnafu};
use crate::eval::{Database, Row};
use crate::program::{Lint, Program, Segment, Severity};

/// One scan finding: a lint-declared relation row located at its first
/// node-valued column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub rule: String,
    pub severity: Severity,
    pub message: String,
    pub file: PathBuf,
    /// 1-based start position of the located node.
    pub line: usize,
    pub column: usize,
    /// 1-based position just past the located node.
    pub end_line: usize,
    pub end_column: usize,
    /// The node's source text, collapsed to one bounded line.
    pub text: String,
}

/// A finding an `astlog-ignore` comment suppressed, paired with the comment
/// that did the suppressing so an audit can answer "what, where, and why".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppressedFinding {
    pub finding: Finding,
    /// 1-based line of the suppressing comment (in the finding's file).
    pub comment_line: usize,
    /// The comment's source text, collapsed to one bounded line.
    pub comment_text: String,
}

/// Findings for every lint declaration, suppression applied, sorted by
/// (file, line, column, rule).
///
/// # Errors
///
/// Fails with [`Error::LintNoNode`] when a lint relation derives a row with
/// no node-valued column to locate the finding at.
pub(crate) fn findings(
    program: &Program,
    corpus: &Corpus,
    db: &Database,
) -> Result<Vec<Finding>, Error> {
    let suppressions = Suppressions::collect(corpus);
    let mut findings = Vec::new();
    for_each_candidate(program, corpus, db, |candidate| {
        if suppressions
            .source(candidate.file_index, candidate.finding.line, &candidate.finding.rule)
            .is_none()
        {
            findings.push(candidate.finding);
        }
        Ok(())
    })?;
    findings.sort_by(|a, b| {
        (&a.file, a.line, a.column, &a.rule).cmp(&(&b.file, b.line, b.column, &b.rule))
    });
    Ok(findings)
}

/// Every finding an `astlog-ignore` comment suppressed, paired with that
/// comment, sorted by (file, line, column, rule) like [`findings`].
///
/// # Errors
///
/// Fails with [`Error::LintNoNode`] when a lint relation derives a row with
/// no node-valued column to locate the finding at.
pub(crate) fn suppressed(
    program: &Program,
    corpus: &Corpus,
    db: &Database,
) -> Result<Vec<SuppressedFinding>, Error> {
    let suppressions = Suppressions::collect(corpus);
    let mut suppressed = Vec::new();
    for_each_candidate(program, corpus, db, |candidate| {
        if let Some(source) = suppressions.source(
            candidate.file_index,
            candidate.finding.line,
            &candidate.finding.rule,
        ) {
            suppressed.push(SuppressedFinding {
                finding: candidate.finding,
                comment_line: source.line,
                comment_text: source.text.clone(),
            });
        }
        Ok(())
    })?;
    suppressed.sort_by(|a, b| {
        (&a.finding.file, a.finding.line, a.finding.column, &a.finding.rule).cmp(&(
            &b.finding.file,
            b.finding.line,
            b.finding.column,
            &b.finding.rule,
        ))
    });
    Ok(suppressed)
}

/// A located candidate finding before suppression is applied, carrying the
/// corpus file index so the caller can look the suppression up.
struct Candidate {
    finding: Finding,
    file_index: usize,
}

/// Build every candidate finding once and hand each to `sink`; the one place
/// the candidate-locating logic lives, shared by [`findings`] and
/// [`suppressed`].
fn for_each_candidate(
    program: &Program,
    corpus: &Corpus,
    db: &Database,
    mut sink: impl FnMut(Candidate) -> Result<(), Error>,
) -> Result<(), Error> {
    for lint in &program.lints {
        let relation = db.relations.get(&lint.relation).ok_or_else(|| {
            InternalSnafu {
                what: format!("lint relation `{}` missing from database", lint.relation),
            }
            .build()
        })?;
        for row in relation.rows() {
            let node = row
                .iter()
                .find_map(|value| match value {
                    Value::Node(node) => Some(*node),
                    Value::Text(_) => None,
                })
                .ok_or_else(|| {
                    LintNoNodeSnafu { rule: lint.relation.clone(), line: lint.line }.build()
                })?;
            let info = corpus.node_info(node);
            let start = corpus.position(node.file, info.start);
            let end = corpus.position(node.file, info.end);
            sink(Candidate {
                finding: Finding {
                    rule: lint.relation.clone(),
                    severity: lint.severity,
                    message: render_message(lint, &relation.columns, row, corpus)?,
                    file: corpus.files[node.file].path.clone(),
                    line: start.line,
                    column: start.column,
                    end_line: end.line,
                    end_column: end.column,
                    text: one_line(corpus.node_text(node)),
                },
                file_index: node.file,
            })?;
        }
    }
    Ok(())
}

/// Instantiate a lint message template against one relation row. Spliced
/// values are flattened with [`one_line`] so a message never spans lines.
fn render_message(
    lint: &Lint,
    columns: &[String],
    row: &Row,
    corpus: &Corpus,
) -> Result<String, Error> {
    let mut message = String::new();
    for segment in &lint.message.segments {
        match segment {
            Segment::Lit(lit) => message.push_str(lit),
            Segment::Var(var) => {
                let index = columns.iter().position(|column| column == var).ok_or_else(|| {
                    InternalSnafu {
                        what: format!(
                            "lint `{}` message variable `{var}` survived checking \
                                 without a relation column",
                            lint.relation
                        ),
                    }
                    .build()
                })?;
                message.push_str(&one_line(corpus.value_text(&row[index])));
            }
        }
    }
    Ok(message)
}

/// Collapse a node's source text to one bounded line for terminal output.
#[must_use]
pub fn one_line(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().take(60).collect();
    if out.chars().count() < flat.chars().count() {
        out.push('…');
    }
    out
}

/// The comment that issued a suppression: its 1-based line and collapsed text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Source {
    line: usize,
    text: String,
}

/// What an `astlog-ignore` comment suppresses on one line, remembering the
/// comment behind each directive so an audit can report why. First writer
/// wins when several comments target the same line and rule.
#[derive(Debug, Default)]
struct LineSuppression {
    all: Option<Source>,
    rules: HashMap<String, Source>,
}

/// Per-file, per-line suppression entries gathered from comment nodes.
struct Suppressions {
    by_file: Vec<HashMap<usize, LineSuppression>>,
}

impl Suppressions {
    fn collect(corpus: &Corpus) -> Self {
        let by_file = corpus
            .files
            .iter()
            .enumerate()
            .map(|(file_index, file)| {
                let mut lines: HashMap<usize, LineSuppression> = HashMap::default();
                for info in &file.nodes {
                    if !info.kind.contains("comment") {
                        continue;
                    }
                    let text = &file.text[info.start..info.end];
                    let Some(directive) = parse_directive(text) else {
                        continue;
                    };
                    let first = corpus.position(file_index, info.start).line;
                    // `end` is exclusive; the comment's last line is the line
                    // of its final byte, and suppression extends one further.
                    let last = corpus.position(file_index, info.end.saturating_sub(1)).line;
                    let source = Source { line: first, text: one_line(text) };
                    for line in first..=last + 1 {
                        let entry = lines.entry(line).or_default();
                        match &directive {
                            Directive::All => {
                                entry.all.get_or_insert_with(|| source.clone());
                            }
                            Directive::Rules(rules) => {
                                for rule in rules {
                                    entry
                                        .rules
                                        .entry(rule.clone())
                                        .or_insert_with(|| source.clone());
                                }
                            }
                        }
                    }
                }
                lines
            })
            .collect();
        Self { by_file }
    }

    /// The comment suppressing `rule` on `line` of `file`, if any. A bare
    /// `astlog-ignore` (the `all` directive) takes precedence over a named one.
    fn source(&self, file: usize, line: usize, rule: &str) -> Option<&Source> {
        let entry = self.by_file[file].get(&line)?;
        entry.all.as_ref().or_else(|| entry.rules.get(rule))
    }
}

enum Directive {
    All,
    Rules(Vec<String>),
}

/// Parse a comment's text into a suppression directive. `astlog-ignore`
/// anywhere in the comment suppresses everything; `astlog-ignore: a, b`
/// limits suppression to those rule names. Name tokens stop at the first
/// character outside `[A-Za-z0-9_-]` so block-comment terminators never leak
/// into a name.
fn parse_directive(comment: &str) -> Option<Directive> {
    let at = comment.find("astlog-ignore")?;
    let rest = comment[at + "astlog-ignore".len()..].trim_start();
    let Some(names) = rest.strip_prefix(':') else {
        return Some(Directive::All);
    };
    let rules: Vec<String> = names
        .split(',')
        .filter_map(|token| {
            let name: String = token
                .trim_start()
                .chars()
                .take_while(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
                .collect();
            (!name.is_empty()).then_some(name)
        })
        .collect();
    if rules.is_empty() { Some(Directive::All) } else { Some(Directive::Rules(rules)) }
}
