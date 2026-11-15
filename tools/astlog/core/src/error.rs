use std::path::PathBuf;

use snafu::Snafu;

/// Every way an astlog run over tree-sitter trees can fail: the engine's errors and this
/// source's own.
pub type Error = astlog_engine::Error<SourceError>;

/// The tree-sitter source's own failures, from reading sources to applying edits.
///
/// Displays never embed `{source}`: callers walk the chain.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum SourceError {
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

    #[snafu(display("rules:{line}: unknown language `{name}`"))]
    UnknownLangName { name: String, line: usize },

    #[snafu(display("rules:{line}: invalid tree-sitter query"))]
    Query { line: usize, source: tree_sitter::QueryError },

    #[snafu(display(
        "rules:{line}: `#` predicates are not supported in match queries; \
         constrain with builtin atoms (text/kind/same-text) instead"
    ))]
    PredicateUnsupported { line: usize },

    #[snafu(display("rules:{line}: capture index {index} out of range for query"))]
    CaptureIndex { line: usize, index: u32 },

    #[snafu(display(
        "rules:{line}: capture `@{capture}` matched nothing in a match of {} (a `?`, `*` or \
         alternation left it out); give the optional part its own match",
        path.display()
    ))]
    CaptureAbsent { line: usize, capture: String, path: PathBuf },

    #[snafu(display("rules:{line}: `{name}` needs a node, but `{arg}` is {what}"))]
    NotNode { line: usize, name: String, arg: String, what: &'static str },

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
}
