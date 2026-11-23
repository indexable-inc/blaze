//! Errors cargo shows as a snippet of the manifest with the place marked, instead of as a chain
//! of causes: a `Cargo.toml` that does not parse or does not fit the schema.

use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::error::ManifestError;
use crate::snippet;
use crate::schema::SchemaError;

/// What a diagnostic is about.
#[derive(Debug)]
pub enum DiagnosticWhat {
    /// The text is no TOML.
    Toml(blaze_toml::What),
    /// The TOML does not fit the schema; the error names the key path.
    Schema(SchemaError),
}

impl fmt::Display for DiagnosticWhat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiagnosticWhat::Toml(what) => write!(f, "{what}"),
            DiagnosticWhat::Schema(error) => write!(f, "{error}"),
        }
    }
}

/// A manifest error with the file, its text and the byte range it is about. Whatever wraps
/// it, cargo prints only this; [`ManifestError::diagnostic`] finds it.
#[derive(Debug)]
pub struct ManifestDiagnostic {
    pub what: DiagnosticWhat,
    pub path: PathBuf,
    pub contents: String,
    pub span: Range<usize>,
}

impl ManifestDiagnostic {
    /// `contents`, the manifest at `path`, is no TOML.
    pub(crate) fn toml(error: blaze_toml::Error, contents: &str, path: &Path) -> ManifestError {
        let start = error.at.min(contents.len());
        // The character at the offset, or the end of the text.
        let end = contents[start..].chars().next().map_or(start, |c| start + c.len_utf8());
        ManifestDiagnostic {
            what: DiagnosticWhat::Toml(error.what),
            path: path.to_owned(),
            contents: contents.to_owned(),
            span: start..end,
        }
        .into()
    }

    /// `contents`, the manifest at `path`, does not fit the schema.
    pub(crate) fn schema(error: SchemaError, contents: &str, path: &Path) -> ManifestError {
        ManifestDiagnostic {
            span: error.span.start..error.span.end,
            what: DiagnosticWhat::Schema(error),
            path: path.to_owned(),
            contents: contents.to_owned(),
        }
        .into()
    }

    /// The key path a schema error is at; `None` for TOML that does not parse.
    pub fn key(&self) -> Option<&str> {
        match &self.what {
            DiagnosticWhat::Schema(error) => Some(&error.key),
            DiagnosticWhat::Toml(_) => None,
        }
    }

    /// The diagnostic as cargo prints it, the manifest's path relative to `cwd`.
    pub fn render(&self, cwd: &Path) -> String {
        let path = crate::paths::relative_to(&self.path, cwd).unwrap_or_else(|| self.path.clone());
        let message = self.what.to_string();
        let path = path.display().to_string();
        snippet::with_span(snippet::Level::Error, &message, &path, &self.contents, self.span.start..self.span.end)
    }
}

impl fmt::Display for ManifestDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.what)
    }
}

impl core::error::Error for ManifestDiagnostic {}

/// An error as cargo prints it to stderr: the diagnostic if there is one, else `error:` and
/// each cause under `Caused by:`.
pub fn report(err: &ManifestError, cwd: &Path) -> String {
    match err.diagnostic() {
        Some(diagnostic) => diagnostic.render(cwd),
        // alloc: an error's text, once, at the edge
        None => format!("error: {}", rendered(err, cwd)),
    }
}

/// An error as cargo writes it after `error:`: the diagnostic if there is one, else each
/// cause under `Caused by:`.
pub fn rendered(err: &ManifestError, cwd: &Path) -> String {
    if let Some(diagnostic) = err.diagnostic() {
        return diagnostic.render(cwd);
    }
    // alloc: an error's text, once, at the edge
    let mut out = String::new();
    for (i, cause) in err.chain().enumerate() {
        if i == 0 {
            out.push_str(&cause.to_string());
            continue;
        }
        out.push_str("\n\nCaused by:");
        // alloc: each cause's text, on the error path
        for line in cause.to_string().lines() {
            out.push('\n');
            if !line.is_empty() {
                out.push_str("  ");
                out.push_str(line);
            }
        }
    }
    out
}
