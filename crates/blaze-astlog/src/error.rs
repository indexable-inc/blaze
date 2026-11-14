//! blaze's own failures, carried by the engine's [`astlog_engine::Error::Source`]. Each names
//! the rules line and what it could not do.

use snafu::Snafu;

/// An error of a blaze rules file or run.
pub type Error = astlog_engine::Error<SourceError>;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum SourceError {
    #[snafu(display("rules:{line}: the {form} pattern `{text}` does not parse as Rust: {error}"))]
    PatternParse { line: usize, form: &'static str, text: String, error: String },

    #[snafu(display("rules:{line}: the {form} pattern `{text}`: {message}"))]
    PatternShape { line: usize, form: &'static str, text: String, message: String },

    #[snafu(display("rules:{line}: `{kind}` is no syntax kind (they are spelled `METHOD_CALL_EXPR`)"))]
    UnknownKind { line: usize, kind: String },

    #[snafu(display("rules:{line}: `{path}`: {message}"))]
    DefPath { line: usize, path: String, message: String },

    #[snafu(display("rules:{line}: `{name}` needs {needs}, but `{arg}` is {what}"))]
    WrongValue { line: usize, name: &'static str, needs: &'static str, arg: String, what: &'static str },

    #[snafu(display("{place}: rewrite `{rule}` (rules:{line}): {message}"))]
    Unplaceable { place: String, rule: String, line: usize, message: String },

    #[snafu(display(
        "{place}: rewrites `{first}` (rules:{first_line}) and `{second}` (rules:{second_line}) edit overlapping text"
    ))]
    Overlap { place: String, first: String, first_line: usize, second: String, second_line: usize },

    #[snafu(display("{what}"))]
    Edit { what: String, at: Option<String> },
}
