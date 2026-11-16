use std::fmt;
use std::path::PathBuf;

/// Every way an astlog run over tree-sitter trees can fail: the engine's errors and this
/// source's own.
pub type Error = astlog_engine::Error<SourceError>;

/// The tree-sitter source's own failures, from reading sources to applying edits.
///
/// Displays never embed the source error: callers walk the chain.
#[derive(Debug)]
pub enum SourceError {
    ReadFile { path: PathBuf, source: std::io::Error },

    /// Listing a directory or reading a `.gitignore` while walking a directory argument;
    /// `path` is the directory or file that could not be read.
    Walk { path: PathBuf, source: std::io::Error },

    UnknownLanguage { path: PathBuf },

    Language { path: PathBuf, source: tree_sitter::LanguageError },

    ParseFile { path: PathBuf },

    UnknownLangName { name: String, line: usize },

    Query { line: usize, source: tree_sitter::QueryError },

    PredicateUnsupported { line: usize },

    CaptureIndex { line: usize, index: u32 },

    CaptureAbsent { line: usize, capture: String, path: PathBuf },

    NotNode { line: usize, name: &'static str, arg: &'static str, what: &'static str },

    OverlappingEdits {
        path: PathBuf,
        first_start: usize,
        first_end: usize,
        second_start: usize,
        second_end: usize,
    },
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadFile { path, .. } => write!(f, "read {}", path.display()),
            Self::Walk { path, .. } => write!(f, "walk {}", path.display()),
            Self::UnknownLanguage { path } => {
                write!(f, "no tree-sitter grammar for {}", path.display())
            }
            Self::Language { path, .. } => {
                write!(f, "grammar rejected by tree-sitter for {}", path.display())
            }
            Self::ParseFile { path } => write!(f, "tree-sitter failed to parse {}", path.display()),
            Self::UnknownLangName { name, line } => {
                write!(f, "rules:{line}: unknown language `{name}`")
            }
            Self::Query { line, .. } => write!(f, "rules:{line}: invalid tree-sitter query"),
            Self::PredicateUnsupported { line } => write!(
                f,
                "rules:{line}: `#` predicates are not supported in match queries; constrain with \
                 builtin atoms (text/kind/same-text) instead"
            ),
            Self::CaptureIndex { line, index } => {
                write!(f, "rules:{line}: capture index {index} out of range for query")
            }
            Self::CaptureAbsent { line, capture, path } => write!(
                f,
                "rules:{line}: capture `@{capture}` matched nothing in a match of {} (a `?`, `*` \
                 or alternation left it out); give the optional part its own match",
                path.display()
            ),
            Self::NotNode { line, name, arg, what } => {
                write!(f, "rules:{line}: `{name}` needs a node, but `{arg}` is {what}")
            }
            Self::OverlappingEdits { path, first_start, first_end, second_start, second_end } => {
                write!(
                    f,
                    "overlapping rewrites in {}: bytes {first_start}..{first_end} and \
                     {second_start}..{second_end}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for SourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ReadFile { source, .. } | Self::Walk { source, .. } => Some(source),
            Self::Language { source, .. } => Some(source),
            Self::Query { source, .. } => Some(source),
            Self::UnknownLanguage { .. }
            | Self::ParseFile { .. }
            | Self::UnknownLangName { .. }
            | Self::PredicateUnsupported { .. }
            | Self::CaptureIndex { .. }
            | Self::CaptureAbsent { .. }
            | Self::NotNode { .. }
            | Self::OverlappingEdits { .. } => None,
        }
    }
}
