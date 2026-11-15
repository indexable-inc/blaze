//! blaze's own failures, carried by the engine's [`astlog_engine::Error::Source`]. Each names
//! the rules line (`rules:N`, or an included file's path and line) and what it could not do.

use astlog_engine::Line;
use snafu::Snafu;

/// An error of a blaze rules file or run.
pub type Error = astlog_engine::Error<SourceError>;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum SourceError {
    #[snafu(display("{line}: the {form} pattern `{text}` does not parse as Rust: {error}"))]
    PatternParse { line: Line, form: &'static str, text: String, error: String },

    #[snafu(display("{line}: the {form} pattern `{text}`: {message}"))]
    PatternShape { line: Line, form: &'static str, text: String, message: String },

    #[snafu(display(
        "{line}: the {form} pattern `{text}` reads as other Rust in edition {edition}, the edition of \
         {place}, than in edition {parsed}, the one patterns are parsed in, so it cannot match that file's code"
    ))]
    PatternEdition { line: Line, form: &'static str, text: String, parsed: usize, edition: usize, place: String },

    #[snafu(display("{line}: `{kind}` is no syntax kind (they are spelled `METHOD_CALL_EXPR`)"))]
    UnknownKind { line: Line, kind: String },

    #[snafu(display("{line}: `{path}`: {message}"))]
    DefPath { line: Line, path: String, message: String },

    #[snafu(display(
        "{line}: `cfg-off` reads the `#[cfg]` of an item, a variant or a field, and `{attr}` at {place} is on a {kind}"
    ))]
    CfgUnread { line: Line, place: String, attr: String, kind: String },

    #[snafu(display("{line}: `{name}` needs {needs}, but `{arg}` is {what}"))]
    WrongValue { line: Line, name: &'static str, needs: &'static str, arg: String, what: &'static str },

    #[snafu(display("{place}: rewrite `{rule}` ({line}): {message}"))]
    Unplaceable { place: String, rule: String, line: Line, message: String },

    #[snafu(display(
        "{place}: rewrites `{first}` ({first_line}) and `{second}` ({second_line}) edit overlapping text"
    ))]
    Overlap { place: String, first: String, first_line: Line, second: String, second_line: Line },

    #[snafu(display("{what}"))]
    Edit { what: String, at: Option<String> },

    #[snafu(display("{place}: the view's file has no text at the range {start}..{end} the front end gives a node"))]
    ViewText { place: String, start: u32, end: u32 },
}

/// `error` as the API reports it: its words, and its place in the view when it has one (an
/// unplaceable match's or an overlap's `path:line`, the edits' own); a rules file's own errors
/// name their rules line in the words.
pub fn api_error(error: &Error) -> blaze_api::ApiError {
    let at = match error {
        astlog_engine::Error::Source { source } => match source {
            SourceError::Unplaceable { place, .. }
            | SourceError::Overlap { place, .. }
            | SourceError::ViewText { place, .. } => {
                Some(place.clone())
            }
            SourceError::Edit { at, .. } => at.clone(),
            SourceError::PatternParse { .. }
            | SourceError::PatternShape { .. }
            | SourceError::PatternEdition { .. }
            | SourceError::UnknownKind { .. }
            | SourceError::DefPath { .. }
            | SourceError::CfgUnread { .. }
            | SourceError::WrongValue { .. } => None,
        },
        _ => None,
    };
    blaze_api::ApiError::new(error.to_string(), at)
}
