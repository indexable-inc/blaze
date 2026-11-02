//! A diagnostic as the agent API gives it ([`blaze_api::Diagnostic`]): rustc's JSON shape
//! (`rustc_errors/src/json.rs`, MIT or Apache-2.0), every span labelled and placed, each
//! suggestion a `help` child whose spans carry their replacement, with the facts it read and
//! its text as rustc prints it.

use blaze_api::{Diagnostic, FactRead, UnitOf};

use crate::{
    emit::{self, Sources, Style, Unplaced, codes},
    model::{Child, Diag, Level, MultiSpan, Sublevel, Suggestion},
};

/// `diag` for the agent API, its `rendered` as rustc writes it in `style`, of `unit` when it is
/// a cargo unit's. Its `explanation` is rustc's for its code, as rustc's JSON gives it, in
/// every format: only a JSON writer prints it. A span in a file `sources` lacks, or past its
/// text's end, is an error naming the file.
pub fn diagnostic(
    diag: &Diag,
    sources: &Sources,
    style: Style,
    unit: Option<UnitOf>,
) -> Result<Diagnostic, Unplaced> {
    sources.check(diag)?;
    let mut children: Vec<Diagnostic> =
        // alloc: the API's owned copy of a diagnostic written, the edge
        diag.children.iter().map(|child| self::child(child, sources)).collect::<Result<_, _>>()?;
    if !diag.suggestions_disabled {
        for it in &diag.suggestions {
            children.push(suggestion(it, sources)?);
        }
    }
    Ok(Diagnostic {
        level: level(diag.level),
        // alloc: the API's owned code, of a diagnostic written
        code: diag.code.as_ref().map(|code| code.as_str().to_owned()),
        // clone: the API's owned message, of a diagnostic written
        message: diag.message.clone(),
        spans: spans(&diag.span, sources)?,
        children,
        facts: diag
            .facts
            .iter()
            // clone: the API's owned text of each fact, of a diagnostic written
            .map(|read| FactRead { key: read.fact.key(), what: read.what.clone() })
            .collect(), // alloc: the API's facts, of a diagnostic written
        rendered: emit::written(diag, sources, style)?,
        unit,
        // alloc: the API's owned explanation, of a diagnostic written
        explanation: diag.code.as_ref().and_then(|code| codes::explanation(code.as_str())).map(str::to_owned),
    })
}

fn level(level: Level) -> blaze_api::Level {
    match level {
        Level::Error => blaze_api::Level::Error,
        Level::Warning => blaze_api::Level::Warning,
        Level::Note => blaze_api::Level::Note,
        Level::Help => blaze_api::Level::Help,
        Level::FailureNote => blaze_api::Level::FailureNote,
    }
}

fn child(child: &Child, sources: &Sources) -> Result<Diagnostic, Unplaced> {
    let level = match child.level {
        Sublevel::Note => blaze_api::Level::Note,
        Sublevel::Help => blaze_api::Level::Help,
        Sublevel::Warning => blaze_api::Level::Warning,
    };
    Ok(Diagnostic {
        level,
        code: None,
        // clone: the API's owned message, of a diagnostic written
        message: child.message.clone(),
        spans: spans(&child.span, sources)?,
        children: Vec::new(), // alloc: empty, allocates nothing
        facts: Vec::new(),    // alloc: empty, allocates nothing
        rendered: String::new(), // alloc: empty, allocates nothing
        unit: None,
        explanation: None,
    })
}

/// rustc's JSON gives every part of every substitution of a suggestion as one span of its
/// child.
fn suggestion(suggestion: &Suggestion, sources: &Sources) -> Result<Diagnostic, Unplaced> {
    let spans = suggestion
        .substitutions
        .iter()
        .flatten()
        .map(|(span, replacement)| {
            let mut it = place(span, sources, true, None)?;
            // clone: the API's owned replacement, of a suggestion written
            it.replacement = Some(replacement.clone());
            it.applicability = Some(suggestion.applicability);
            Ok(it)
        })
        .collect::<Result<_, _>>()?; // alloc: the API's spans of a suggestion written
    Ok(Diagnostic {
        level: blaze_api::Level::Help,
        code: None,
        // clone: the API's owned message, of a suggestion written
        message: suggestion.message.clone(),
        spans,
        children: Vec::new(), // alloc: empty, allocates nothing
        facts: Vec::new(),    // alloc: empty, allocates nothing
        rendered: String::new(), // alloc: empty, allocates nothing
        unit: None,
        explanation: None,
    })
}

fn spans(span: &MultiSpan, sources: &Sources) -> Result<Vec<blaze_api::Span>, Unplaced> {
    span.span_labels()
        .into_iter()
        // alloc: the API's owned labels, of a diagnostic written
        .map(|label| place(label.span, sources, label.is_primary, label.label.map(str::to_owned)))
        .collect() // alloc: the API's spans, of a diagnostic written
}

/// A span of a file blaze can't read is a bug of whoever made it, or of whoever gathered the
/// files' text: it points into nothing.
fn place(
    span: &crate::model::Span,
    sources: &Sources,
    primary: bool,
    label: Option<String>,
) -> Result<blaze_api::Span, Unplaced> {
    let (line, column) = sources.placed(&span.file, span.lo)?;
    let (end_line, end_column) = sources.placed(&span.file, span.hi)?;
    Ok(blaze_api::Span {
        // clone: the API's owned file name, of a span written
        file: crate::emit::shown(&span.file).to_owned(),
        line,
        column,
        end_line,
        end_column,
        label,
        byte_start: span.lo,
        byte_end: span.hi,
        primary,
        replacement: None,
        applicability: None,
    })
}

#[cfg(test)]
mod tests {
    use blaze_items::{Fact, ItemKey};
    use syntax::{TextRange, TextSize};

    use super::*;
    use crate::model::{Applicability, Span};

    fn span(lo: u32, hi: u32) -> Span {
        Span::new("src/main.rs", TextRange::new(TextSize::from(lo), TextSize::from(hi)))
    }

    fn sources() -> Sources {
        Sources::of([("src/main.rs".to_owned(), "fn main() {\n    let x: u32 = \"a\";\n}\n".to_owned())])
    }

    #[test]
    fn applicability_names_and_writes_as_the_model_copy_did() {
        let kept = ["MachineApplicable", "MaybeIncorrect", "HasPlaceholders", "Unspecified"];
        for (it, name) in Applicability::ALL.into_iter().zip(kept) {
            assert_eq!(it.name(), name);
            assert_eq!(serde_json::to_string(&it).unwrap(), format!("\"{name}\""));
            assert_eq!(serde_json::from_str::<Applicability>(&format!("\"{name}\"")).unwrap(), it);
        }
    }

    #[test]
    fn spans_are_placed_by_line_and_column_of_characters() {
        let mut diag = Diag::error(Some("E0308"), "mismatched types", span(29, 32));
        diag.span_label(span(29, 32), "expected `u32`, found `&str`");
        diag.span_label(span(23, 26), "expected due to this");
        let it = diagnostic(&diag, &sources(), Style::default(), None).unwrap();
        assert_eq!(it.code.as_deref(), Some("E0308"));
        assert_eq!(it.level, blaze_api::Level::Error);
        let placed: Vec<_> = it
            .spans
            .iter()
            .map(|s| (s.line, s.column, s.end_line, s.end_column, s.primary, s.label.as_deref()))
            .collect();
        assert_eq!(placed, [
            (2, 18, 2, 21, true, Some("expected `u32`, found `&str`")),
            (2, 12, 2, 15, false, Some("expected due to this")),
        ]);
        assert!(it.rendered.starts_with("error[E0308]: mismatched types\n"), "{}", it.rendered);
        assert_eq!(it.explanation.as_deref(), Some(include_str!("emit/codes/E0308.md")));
    }

    #[test]
    fn a_suggestion_is_a_help_child_whose_spans_carry_the_replacement() {
        let mut diag = Diag::error(Some("E0308"), "mismatched types", span(29, 32));
        diag.note("a note");
        diag.suggestion(span(29, 32), "use a number", "1", Applicability::MaybeIncorrect);
        let it = diagnostic(&diag, &sources(), Style::default(), None).unwrap();
        assert_eq!(it.children.len(), 2);
        assert_eq!((it.children[0].level, it.children[0].message.as_str()), (blaze_api::Level::Note, "a note"));
        assert!(it.children[0].spans.is_empty());
        let help = &it.children[1];
        assert_eq!((help.level, help.message.as_str()), (blaze_api::Level::Help, "use a number"));
        assert_eq!(help.spans[0].replacement.as_deref(), Some("1"));
        assert_eq!(help.spans[0].applicability, Some(blaze_api::Applicability::MaybeIncorrect));
        assert!(help.rendered.is_empty());
        assert_eq!(help.explanation, None);
    }

    #[test]
    fn facts_are_their_query_keys_with_what_they_stand_for() {
        let fact = Fact::Signature(ItemKey(blaze_ids::ContentId([7; 32])));
        let mut diag = Diag::error(Some("E0061"), "this function takes 1 argument", span(29, 32));
        diag.fact(fact, "the signature of `f`");
        let it = diagnostic(&diag, &sources(), Style::default(), None).unwrap();
        assert_eq!(it.facts, [FactRead { key: fact.key(), what: "the signature of `f`".to_owned() }]);
    }

    #[test]
    fn a_span_of_a_file_not_read_or_past_its_end_is_an_error_naming_the_file() {
        let mut unread = Diag::error(None, "unused", span(1, 2));
        unread.span_label(Span::new("out/gen.rs", TextRange::new(TextSize::from(5), TextSize::from(9))), "here");
        let mut sources = sources();
        sources.add("out/gen.rs", None);
        assert_eq!(
            diagnostic(&unread, &sources, Style::default(), None).unwrap_err(),
            Unplaced::Unread { file: "out/gen.rs".to_owned(), byte: 5 },
        );
        let past = Diag::error(None, "unused", span(40, 90));
        let why = diagnostic(&past, &sources, Style::default(), None).unwrap_err();
        assert_eq!(why, Unplaced::PastEnd { file: "src/main.rs".to_owned(), byte: 40, len: 36 });
        assert_eq!(why.to_string(), "a diagnostic's span at byte 40 of `src/main.rs`, which is 36 bytes long");
    }
}
