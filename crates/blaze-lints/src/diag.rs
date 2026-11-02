//! A lint's diagnostic as data: [`blaze_diagnostics::model`]'s, as every diagnostic blaze gives
//! is, and the naming of the spans a lint query makes. It crosses the daemon's socket as it is,
//! so the source it points into is read where it is written, by its path.

use syntax::TextRange;

pub use blaze_diagnostics::model::{
    Applicability, Child, Code, Diag, Expansion, MacroKind, MultiSpan, Span, SpanLabel, Sublevel,
    Suggestion, SuggestionStyle,
};

/// What a span made in a lint query has for its file, before the driver names it: the empty
/// name for the tree the query walks (its range moved into the file by the driver), or this
/// byte and the file's number for a span of another file.
const FILE_ID: char = '\0';

/// A span of the tree a lint query walks, which the driver moves into the file and names.
pub(crate) fn in_query(range: TextRange) -> Span {
    Span { file: String::new(), lo: range.start().into(), hi: range.end().into(), expansion: None }
}

/// A span of `file` as a lint query makes it, which the driver names.
pub(crate) fn of_file(file: span::FileId, range: TextRange) -> Span {
    let mut name = String::with_capacity(11);
    name.push(FILE_ID);
    name.push_str(&file.index().to_string());
    Span { file: name, lo: range.start().into(), hi: range.end().into(), expansion: None }
}

/// A file of the crate the caller's `check::NameOf` gives no name: what a lint finds in it
/// can't be said where rustc says it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Unnamed {
    pub file: span::FileId,
}

impl std::fmt::Display for Unnamed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the lints' file {} has no name to report a diagnostic in", self.file.index())
    }
}

impl std::error::Error for Unnamed {}

/// Names the files of every span of `diag` a lint query made ([`place`]).
pub(crate) fn place_diag(
    diag: &mut Diag,
    offset: u32,
    name: &str,
    // dyn: `check::NameOf`, asked once per diagnostic span that leaves its file.
    name_of: &dyn Fn(span::FileId) -> Option<String>,
) -> Result<(), Unnamed> {
    place_multi(&mut diag.span, offset, name, name_of)?;
    for child in &mut diag.children {
        place_multi(&mut child.span, offset, name, name_of)?;
    }
    for suggestion in &mut diag.suggestions {
        for (span, _) in suggestion.substitutions.iter_mut().flatten() {
            place(span, offset, name, name_of)?;
        }
    }
    Ok(())
}

fn place_multi(
    span: &mut MultiSpan,
    offset: u32,
    name: &str,
    // dyn: `check::NameOf`, asked once per diagnostic span that leaves its file.
    name_of: &dyn Fn(span::FileId) -> Option<String>,
) -> Result<(), Unnamed> {
    for span in span.primary.iter_mut().chain(span.labels.iter_mut().map(|it| &mut it.0)) {
        place(span, offset, name, name_of)?;
    }
    Ok(())
}

/// Gives a span a lint query made its file's name, moving it by `offset` when it is of the
/// tree the query walked, named `name`.
pub(crate) fn place(
    span: &mut Span,
    offset: u32,
    name: &str,
    // dyn: `check::NameOf`, asked once per diagnostic span that leaves its file.
    name_of: &dyn Fn(span::FileId) -> Option<String>,
) -> Result<(), Unnamed> {
    if span.file.is_empty() {
        span.lo += offset;
        span.hi += offset;
        name.clone_into(&mut span.file);
    } else if let Some(id) = span.file.strip_prefix(FILE_ID) {
        let id = span::FileId::from_raw(id.parse().expect("a span's file number, as `of_file` wrote it"));
        span.file = name_of(id).ok_or(Unnamed { file: id })?;
    }
    match &mut span.expansion {
        Some(expansion) => place_expansion(expansion, offset, name, name_of),
        None => Ok(()),
    }
}

fn place_expansion(
    expansion: &mut Expansion,
    offset: u32,
    name: &str,
    // dyn: `check::NameOf`, asked once per diagnostic span that leaves its file.
    name_of: &dyn Fn(span::FileId) -> Option<String>,
) -> Result<(), Unnamed> {
    if let Some(def_site) = expansion.def_site.as_mut() {
        place(def_site, offset, name, name_of)?;
    }
    place(&mut expansion.call_site, offset, name, name_of)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_span_of_the_query_is_moved_and_named_and_one_of_another_file_is_named() {
        let range = TextRange::new(2.into(), 5.into());
        let mut diag = Diag::lint("clippy::needless_bool", "x", in_query(range));
        diag.span_label(of_file(span::FileId::from_raw(7), range), "there");
        let name_of = |file: span::FileId| (file.index() == 7).then(|| "src/other.rs".to_owned());
        assert_eq!(place_diag(&mut diag, 100, "src/lib.rs", &name_of), Ok(()));
        assert_eq!((diag.span.primary[0].file.as_str(), diag.span.primary[0].lo, diag.span.primary[0].hi), ("src/lib.rs", 102, 105));
        assert_eq!((diag.span.labels[0].0.file.as_str(), diag.span.labels[0].0.lo), ("src/other.rs", 2));
        let mut unnamed = Diag::lint("clippy::needless_bool", "x", of_file(span::FileId::from_raw(8), range));
        assert_eq!(
            place_diag(&mut unnamed, 0, "src/lib.rs", &name_of),
            Err(Unnamed { file: span::FileId::from_raw(8) }),
            "an unnamed file is the error, never a diagnostic left out"
        );
    }
}
