//! Errors cargo shows as a snippet of the manifest with the place marked, instead of as a chain
//! of causes: a `Cargo.toml` that does not parse or does not fit the schema.

use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};

use annotate_snippets::{AnnotationKind, Group, Level, Renderer, Snippet};

/// A manifest error with the span it is about. Whatever wraps it, cargo prints only this, and
/// a front end finds it with `anyhow::Error::downcast_ref`.
#[derive(Debug)]
pub struct ManifestDiagnostic {
    message: String,
    path: PathBuf,
    contents: String,
    span: Range<usize>,
}

impl ManifestDiagnostic {
    pub(crate) fn from_toml(error: toml::de::Error, contents: &str, path: &Path) -> anyhow::Error {
        match error.span() {
            Some(span) => ManifestDiagnostic {
                message: error.message().to_owned(),
                path: path.to_owned(),
                contents: contents.to_owned(),
                span,
            }
            .into(),
            None => error.into(),
        }
    }

    /// The diagnostic as cargo prints it, the manifest's path relative to `cwd`.
    pub fn render(&self, cwd: &Path) -> String {
        let path = pathdiff::diff_paths(&self.path, cwd).unwrap_or_else(|| self.path.clone());
        let group = Group::with_title(Level::ERROR.primary_title(&self.message)).element(
            Snippet::source(&self.contents)
                .path(path.display().to_string())
                .annotation(AnnotationKind::Primary.span(self.span.clone())),
        );
        Renderer::plain().render(&[group])
    }
}

impl fmt::Display for ManifestDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ManifestDiagnostic {}

/// An error as cargo prints it to stderr: the diagnostic if there is one, else `error:` and
/// each cause under `Caused by:`.
pub fn report(err: &anyhow::Error, cwd: &Path) -> String {
    match err.downcast_ref::<ManifestDiagnostic>() {
        Some(diagnostic) => diagnostic.render(cwd),
        // alloc: an error's text, once, at the edge
        None => format!("error: {}", rendered(err, cwd)),
    }
}

/// An error as cargo writes it after `error:`: the diagnostic if there is one, else each
/// cause under `Caused by:`.
pub fn rendered(err: &anyhow::Error, cwd: &Path) -> String {
    if let Some(diagnostic) = err.downcast_ref::<ManifestDiagnostic>() {
        return diagnostic.render(cwd);
    }
    let mut chain = err.chain();
    // alloc: an error's text, once, at the edge
    let mut out = chain.next().unwrap().to_string();
    for cause in chain {
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
