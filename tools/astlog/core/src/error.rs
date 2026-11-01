use std::path::PathBuf;

use snafu::Snafu;

/// Every way an astlog run can fail, from reading sources to applying edits.
///
/// Displays never embed `{source}`: callers walk the chain.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    #[snafu(display("read {}", path.display()))]
    ReadFile { path: PathBuf, source: std::io::Error },

    #[snafu(display("walk {}", path.display()))]
    Walk { path: PathBuf, source: ignore::Error },

    #[snafu(display("no tree-sitter grammar for {}", path.display()))]
    UnknownLanguage { path: PathBuf },

    #[snafu(display("grammar rejected by tree-sitter for {}", path.display()))]
    Language { path: PathBuf, source: tree_sitter::LanguageError },

    #[snafu(display("tree-sitter failed to parse {}", path.display()))]
    ParseFile { path: PathBuf },

    #[snafu(display("rules:{line}: {message}"))]
    Dsl { line: usize, message: String },

    #[snafu(display("rules:{line}: unknown language `{name}`"))]
    UnknownLangName { name: String, line: usize },

    #[snafu(display("rules:{line}: invalid tree-sitter query"))]
    Query { line: usize, source: tree_sitter::QueryError },

    #[snafu(display(
        "rules:{line}: `#` predicates are not supported in match queries; \
         constrain with builtin atoms (text/kind/same-text) instead"
    ))]
    PredicateUnsupported { line: usize },

    #[snafu(display("rules:{line}: invalid regex `{pattern}` in text-match"))]
    Regex { line: usize, pattern: String, source: regex::Error },

    #[snafu(display("rules:{line}: head variable `{var}` is not bound by the body"))]
    UnboundHeadVar { line: usize, var: String },

    #[snafu(display("rules:{line}: builtin `{name}` needs argument `{arg}` bound at this point"))]
    UnboundBuiltinArg { line: usize, name: String, arg: String },

    #[snafu(display("rules:{line}: builtin `{name}` takes {expected} arguments, got {got}"))]
    BuiltinArity { line: usize, name: String, expected: usize, got: usize },

    #[snafu(display("rules:{line}: builtin `{name}` needs a node, but `{arg}` is text"))]
    BuiltinNotNode { line: usize, name: String, arg: String },

    #[snafu(display("rules:{line}: relation `{name}` is not defined by any rule"))]
    UnknownRelation { name: String, line: usize },

    #[snafu(display(
        "rules:{line}: relation `{name}` used with arity {got}, defined with arity {expected}"
    ))]
    ArityMismatch { name: String, expected: usize, got: usize, line: usize },

    #[snafu(display("rules:{line}: capture index {index} out of range for query"))]
    CaptureIndex { line: usize, index: u32 },

    #[snafu(display("rewrite `{name}`: replacement target `{var}` is bound to text, not a node"))]
    ReplaceNotNode { name: String, var: String },

    #[snafu(display("rules:{line}: template references unbound variable `{var}`"))]
    TemplateVar { var: String, line: usize },

    #[snafu(display("rules:{line}: lint severity must be `error` or `warning`, got `{got}`"))]
    LintSeverity { got: String, line: usize },

    #[snafu(display(
        "rules:{line}: lint message references `{{{var}}}`, which is not a head variable of `{relation}`"
    ))]
    LintVar { relation: String, var: String, line: usize },

    #[snafu(display(
        "lint `{rule}` (rules:{line}): relation row has no node-valued column to locate the finding"
    ))]
    LintNoNode { rule: String, line: usize },

    #[snafu(display(
        "overlapping rewrites in {}: bytes {first_start}..{first_end} and {second_start}..{second_end}",
        path.display()
    ))]
    OverlappingEdits {
        path: PathBuf,
        first_start: usize,
        first_end: usize,
        second_start: usize,
        second_end: usize,
    },

    #[snafu(display(
        "rules:{line}: variable `{var}` is used only inside `(not ...)`; \
         negation can only filter variables a positive atom already binds"
    ))]
    UnsafeNegation { var: String, line: usize },

    #[snafu(display(
        "rules: negation through recursion has no stratification (a relation \
         depends on itself via `(not ...)`); first rule `{rule}` at rules:{line}"
    ))]
    UnstratifiableProgram { rule: String, line: usize },

    #[snafu(display("internal invariant broken: {what}"))]
    Internal { what: String },
}
