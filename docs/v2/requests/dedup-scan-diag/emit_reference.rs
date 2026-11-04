//! The reference for [`super`]: `blaze-lints`' emitter of clippy's lints as it was before it
//! went (`crates/blaze-lints/src/emit.rs` at the v2 branch's `HEAD` of 2026-10-29), taking a
//! [`Diag`] in place of its `Diagnostic` and written at `Error` or `Warning`. It knows no
//! byte order mark, `\r\n`, error code, failure note, `-Z ui-testing` nor macro expansion, and
//! copies every text it shows; on what it knows, [`super::written`] and [`super::json`] must
//! give what it gives, byte for byte and key for key.

use std::sync::Arc;

use annotate_snippets::{AnnotationKind, Group, Patch, Renderer, Snippet};
use blaze_ids::radix;
use proptest::prelude::*;
use rustc_hash::FxHashMap;
use serde::Serialize;

use super::{Style, WIDTH};
use crate::model::{Applicability, Child, Code, Diag, Level, MultiSpan, Span, Sublevel, Suggestion, SuggestionStyle};

const MAX_SUGGESTIONS: usize = 4;

pub(super) struct Sources {
    files: FxHashMap<String, Option<Arc<File>>>,
}

pub(super) struct File {
    text: String,
    lines: Vec<usize>,
}

impl File {
    fn new(text: String) -> File {
        let mut lines = vec![0];
        lines.extend(text.match_indices('\n').map(|(at, _)| at + 1).filter(|it| *it < text.len()));
        File { text, lines }
    }

    fn line_of(&self, at: usize) -> usize {
        self.lines.partition_point(|it| *it <= at).saturating_sub(1)
    }

    fn line_bounds(&self, line: usize) -> std::ops::Range<usize> {
        let start = self.lines.get(line).copied().unwrap_or(self.text.len());
        let end = self.lines.get(line + 1).copied().unwrap_or(self.text.len());
        start..end
    }

    fn line_text(&self, line: usize) -> &str {
        let bounds = self.line_bounds(line);
        let text = &self.text[bounds];
        text.strip_suffix('\n').map(|it| it.strip_suffix('\r').unwrap_or(it)).unwrap_or(text)
    }

    fn position(&self, at: usize) -> (usize, usize) {
        let at = floor(&self.text, at);
        let line = self.line_of(at);
        (line + 1, self.text[self.lines[line]..at].chars().count())
    }

    fn snippet(&self, span: &Span) -> Option<&str> {
        self.text.get(span.lo as usize..span.hi as usize)
    }
}

impl Sources {
    pub(super) fn of(files: impl IntoIterator<Item = (String, String)>) -> Sources {
        let files = files.into_iter().map(|(name, text)| (name, Some(Arc::new(File::new(text)))));
        Sources { files: files.collect() }
    }

    fn file(&mut self, name: &str) -> Option<Arc<File>> {
        self.files.get(name).cloned().flatten()
    }

    fn snippet(&mut self, span: &Span) -> Option<String> {
        Some(self.file(&span.file)?.snippet(span)?.to_owned())
    }
}

pub(super) fn rendered(diagnostic: &Diag, sources: &mut Sources, style: Style) -> String {
    let mut span = diagnostic.span.clone();
    let mut suggestions: Vec<Suggestion> = match diagnostic.suggestions_disabled {
        true => Vec::new(),
        false => diagnostic.suggestions.clone(),
    };
    primary_span_formatted(&mut span, &mut suggestions, sources);
    let children = diagnostic.children.clone();
    let renderer = match style.ansi {
        true => Renderer::styled(),
        false => Renderer::plain(),
    };
    let renderer = renderer
        .term_width(style.width)
        .decor_style(annotate_snippets::renderer::DecorStyle::Ascii)
        .short_message(style.short);
    let level = level_of(diagnostic.level);
    let mut report: Vec<Group<'_>> = Vec::new();
    let mut group = Group::with_title(level.clone().primary_title(diagnostic.message.clone()));

    for (file, annotations) in primary_first(collect_annotations(&span), &span) {
        if let Some(snippet) = annotated_snippet(&file, annotations, sources) {
            group = group.element(snippet);
        }
    }

    for child in &children {
        let level = sublevel_of(child.level);
        if child.span.is_empty() {
            group = group.element(level.message(child.message.clone()));
            continue;
        }
        report.push(std::mem::replace(&mut group, Group::with_title(level.secondary_title(child.message.clone()))));
        for (file, annotations) in primary_first(collect_annotations(&child.span), &child.span) {
            if let Some(snippet) = annotated_snippet(&file, annotations, sources) {
                group = group.element(snippet);
            }
        }
    }

    for suggestion in suggestions {
        match suggestion.style {
            SuggestionStyle::CompletelyHidden => {}
            SuggestionStyle::HideCodeAlways => {
                group = group.element(annotate_snippets::Level::HELP.message(suggestion.message));
            }
            SuggestionStyle::HideCodeInline | SuggestionStyle::ShowCode | SuggestionStyle::ShowAlways => {
                let substitutions: Vec<Vec<(Span, String)>> = suggestion
                    .substitutions
                    .into_iter()
                    .filter_map(|mut parts| {
                        radix::sort_by_key(&mut parts, |(span, _)| span.lo);
                        let first = parts.first()?.0.file.clone();
                        if parts.iter().any(|(span, _)| span.file != first) {
                            return None;
                        }
                        sources.file(&first)?;
                        parts.retain(|(span, snippet)| is_different(sources, snippet, span));
                        (!parts.is_empty()).then_some(parts)
                    })
                    .collect();
                if substitutions.is_empty() {
                    continue;
                }
                let mut message = suggestion.message;
                let file_name = substitutions[0][0].0.file.clone();
                let Some(file) = sources.file(&file_name) else { continue };
                let others = substitutions.len().saturating_sub(MAX_SUGGESTIONS);
                let mut subs = Vec::new();
                for parts in substitutions.into_iter().take(MAX_SUGGESTIONS) {
                    let mut confusion = Confusion::None;
                    for (span, snippet) in &parts {
                        confusion = confusion.combine(detect_confusion_type(sources, snippet, span));
                    }
                    if confusion != Confusion::None {
                        message.push_str(confusion.label_text());
                    }
                    let mut parts: Vec<(Span, String)> =
                        parts.into_iter().filter(|(span, snippet)| is_different(sources, snippet, span)).collect();
                    if parts.is_empty() {
                        continue;
                    }
                    let fold = !attribute_suggestion(&file, &mut parts);
                    let spans: Vec<Span> = parts.iter().map(|(span, _)| span.clone()).collect();
                    let Some((lo, source, line)) = shrink_file(&file, &spans) else { continue };
                    subs.push(Snippet::source(source).line_start(line).path(file_name.clone()).fold(fold).patches(
                        parts.into_iter().map(move |(span, replacement)| {
                            let from = (span.lo as usize).saturating_sub(lo);
                            let to = (span.hi as usize).saturating_sub(lo);
                            Patch::new(from..to, replacement)
                        }),
                    ));
                }
                if !subs.is_empty() {
                    report.push(std::mem::replace(
                        &mut group,
                        Group::with_title(annotate_snippets::Level::HELP.secondary_title(message)),
                    ));
                    group = group.elements(subs);
                    if others > 0 {
                        let plural = if others == 1 { "" } else { "s" };
                        group = group.element(
                            annotate_snippets::Level::NOTE.no_name().message(format!("and {others} other candidate{plural}")),
                        );
                    }
                }
            }
        }
    }
    if !group.is_empty() {
        report.push(group);
    }
    renderer.render(&report)
}

pub(super) fn written(diagnostic: &Diag, sources: &mut Sources, style: Style) -> String {
    let mut text = rendered(diagnostic, sources, style);
    text.push('\n');
    if !style.short {
        text.push('\n');
    }
    text
}

fn level_of(level: Level) -> annotate_snippets::Level<'static> {
    match level {
        Level::Error => annotate_snippets::Level::ERROR,
        _ => annotate_snippets::Level::WARNING,
    }
}

fn sublevel_of(level: Sublevel) -> annotate_snippets::Level<'static> {
    match level {
        Sublevel::Note => annotate_snippets::Level::NOTE,
        Sublevel::Help => annotate_snippets::Level::HELP,
        Sublevel::Warning => annotate_snippets::Level::WARNING,
    }
}

struct Annotation {
    kind: AnnotationKind,
    span: Span,
    label: Option<String>,
}

fn collect_annotations(span: &MultiSpan) -> Vec<(String, Vec<Annotation>)> {
    let mut output: Vec<(String, Vec<Annotation>)> = Vec::new();
    for label in span.span_labels() {
        let kind = if label.is_primary { AnnotationKind::Primary } else { AnnotationKind::Context };
        let annotation = Annotation { kind, span: label.span.clone(), label: label.label.map(normalize_whitespace) };
        match output.iter_mut().find(|(file, _)| *file == label.span.file) {
            Some((_, annotations)) => annotations.push(annotation),
            None => output.push((label.span.file.clone(), vec![annotation])),
        }
    }
    output
}

fn primary_first(mut files: Vec<(String, Vec<Annotation>)>, span: &MultiSpan) -> Vec<(String, Vec<Annotation>)> {
    if let Some(primary) = span.primary_span()
        && let Ok(at) = files.binary_search_by(|(file, _)| file.as_str().cmp(&primary.file))
    {
        files.swap(0, at);
    }
    files
}

fn annotated_snippet<'a>(
    file_name: &str,
    mut annotations: Vec<Annotation>,
    sources: &mut Sources,
) -> Option<Snippet<'a, annotate_snippets::Annotation<'a>>> {
    let file = sources.file(file_name)?;
    radix::sort_by_key(&mut annotations, |it| file.line_of(it.span.lo as usize) as u64);
    let spans: Vec<Span> = annotations.iter().map(|it| it.span.clone()).collect();
    let (lo, source, line) = shrink_file(&file, &spans)?;
    Some(Snippet::source(source).line_start(line).path(file_name.to_owned()).annotations(annotations.into_iter().map(
        move |it| {
            let from = (it.span.lo as usize).saturating_sub(lo);
            let to = (it.span.hi as usize).saturating_sub(lo);
            let annotation = it.kind.span(from..to);
            match it.label {
                Some(label) => annotation.label(label),
                None => annotation,
            }
        },
    )))
}

fn shrink_file(file: &File, spans: &[Span]) -> Option<(usize, String, usize)> {
    let lo = spans.iter().map(|it| it.lo as usize).min()?;
    let hi = spans.iter().map(|it| it.hi as usize).max()?;
    let (lo_line, hi_line) = (file.line_of(lo), file.line_of(hi));
    let start = file.line_bounds(lo_line).start;
    let end = file.line_bounds(hi_line).end;
    Some((start, file.text.get(start..end)?.to_owned(), lo_line + 1))
}

fn attribute_suggestion(file: &File, parts: &mut [(Span, String)]) -> bool {
    let [(span, snippet)] = parts else { return false };
    let trimmed = snippet.trim();
    if !(trimmed.starts_with("#[") && trimmed.ends_with(']') && snippet.ends_with('\n')) {
        return false;
    }
    if span.lo != span.hi {
        return false;
    }
    let before = &file.text[..span.lo as usize];
    let before = before.rsplit_once('\n').map_or(before, |(_, it)| it);
    if !before.trim().is_empty() {
        return false;
    }
    if !before.is_empty() && !snippet.ends_with(before) {
        snippet.insert_str(0, before);
        span.lo -= before.len() as u32;
        span.hi = span.lo;
    }
    true
}

fn primary_span_formatted(span: &mut MultiSpan, suggestions: &mut Vec<Suggestion>, sources: &mut Sources) {
    let [suggestion] = &suggestions[..] else { return };
    let [substitution] = &suggestion.substitutions[..] else { return };
    let [(part_span, snippet)] = &substitution[..] else { return };
    let inline = suggestion.message.split_whitespace().count() < 10
        && !snippet.contains('\n')
        && !matches!(
            suggestion.style,
            SuggestionStyle::HideCodeAlways | SuggestionStyle::CompletelyHidden | SuggestionStyle::ShowAlways
        );
    if !inline {
        return;
    }
    let snippet = snippet.trim();
    let label = if snippet.is_empty() || suggestion.style.hide_inline() {
        format!("help: {}", suggestion.message)
    } else {
        let confusion = detect_confusion_type(sources, snippet, part_span);
        format!("help: {}{}: `{snippet}`", suggestion.message, confusion.label_text())
    };
    span.labels.push((part_span.clone(), label));
    suggestions.clear();
}

fn is_different(sources: &mut Sources, suggested: &str, span: &Span) -> bool {
    sources.snippet(span).is_none_or(|found| found != suggested)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Confusion {
    None,
    Case,
    DigitLetter,
    Both,
}

impl Confusion {
    fn label_text(self) -> &'static str {
        match self {
            Confusion::None => "",
            Confusion::Case => " (notice the capitalization)",
            Confusion::DigitLetter => " (notice the digit/letter confusion)",
            Confusion::Both => " (notice the capitalization and digit/letter confusion)",
        }
    }

    fn combine(self, other: Confusion) -> Confusion {
        match (self, other) {
            (Confusion::None, other) => other,
            (this, Confusion::None) => this,
            (Confusion::Both, _) | (_, Confusion::Both) => Confusion::Both,
            (Confusion::Case, Confusion::DigitLetter) | (Confusion::DigitLetter, Confusion::Case) => Confusion::Both,
            (this, _) => this,
        }
    }
}

fn detect_confusion_type(sources: &mut Sources, suggested: &str, span: &Span) -> Confusion {
    let Some(found) = sources.snippet(span) else { return Confusion::None };
    if found.len() != suggested.len() {
        return Confusion::None;
    }
    const ASCII_CONFUSABLES: &[char] = &['c', 'f', 'i', 'k', 'o', 's', 'u', 'v', 'w', 'x', 'y', 'z'];
    const DIGIT_LETTER: [(char, char); 5] = [('0', 'O'), ('1', 'l'), ('5', 'S'), ('8', 'B'), ('9', 'g')];
    let (mut case, mut digit_letter, mut other) = (false, false, false);
    for (f, s) in std::iter::zip(found.chars(), suggested.chars()) {
        if f == s {
            continue;
        }
        if f.eq_ignore_ascii_case(&s) {
            if ASCII_CONFUSABLES.contains(&f) || ASCII_CONFUSABLES.contains(&s) {
                case = true;
            } else {
                other = true;
            }
        } else if DIGIT_LETTER.contains(&(f, s)) || DIGIT_LETTER.contains(&(s, f)) {
            digit_letter = true;
        } else {
            other = true;
        }
    }
    let differs = found != suggested;
    match (case && !other && differs, digit_letter && !other && differs) {
        (true, true) => Confusion::Both,
        (true, false) => Confusion::Case,
        (false, true) => Confusion::DigitLetter,
        (false, false) => Confusion::None,
    }
}

const OUTPUT_REPLACEMENTS: &[(char, &str)] = &[
    ('\0', "␀"),
    ('\u{0001}', "␁"),
    ('\u{0002}', "␂"),
    ('\u{0003}', "␃"),
    ('\u{0004}', "␄"),
    ('\u{0005}', "␅"),
    ('\u{0006}', "␆"),
    ('\u{0007}', "␇"),
    ('\u{0008}', "␈"),
    ('\t', "    "),
    ('\u{000b}', "␋"),
    ('\u{000c}', "␌"),
    ('\u{000d}', "␍"),
    ('\u{000e}', "␎"),
    ('\u{000f}', "␏"),
    ('\u{0010}', "␐"),
    ('\u{0011}', "␑"),
    ('\u{0012}', "␒"),
    ('\u{0013}', "␓"),
    ('\u{0014}', "␔"),
    ('\u{0015}', "␕"),
    ('\u{0016}', "␖"),
    ('\u{0017}', "␗"),
    ('\u{0018}', "␘"),
    ('\u{0019}', "␙"),
    ('\u{001a}', "␚"),
    ('\u{001b}', "␛"),
    ('\u{001c}', "␜"),
    ('\u{001d}', "␝"),
    ('\u{001e}', "␞"),
    ('\u{001f}', "␟"),
    ('\u{007f}', "␡"),
    ('\u{200d}', ""),
    ('\u{202a}', "�"),
    ('\u{202b}', "�"),
    ('\u{202c}', "�"),
    ('\u{202d}', "�"),
    ('\u{202e}', "�"),
    ('\u{2066}', "�"),
    ('\u{2067}', "�"),
    ('\u{2068}', "�"),
    ('\u{2069}', "�"),
];

fn normalize_whitespace(text: &str) -> String {
    text.chars().fold(String::with_capacity(text.len()), |mut out, c| {
        match OUTPUT_REPLACEMENTS.binary_search_by_key(&c, |(it, _)| *it) {
            Ok(at) => out.push_str(OUTPUT_REPLACEMENTS[at].1),
            Err(_) => out.push(c),
        }
        out
    })
}

fn floor(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// The JSON as `blaze-lints` wrote it, compared as a value: its keys were in cargo's order.
#[derive(Serialize)]
pub(super) struct Json {
    rendered: String,
    #[serde(rename = "$message_type")]
    message_type: &'static str,
    children: Vec<JsonChild>,
    level: &'static str,
    message: String,
    spans: Vec<JsonSpan>,
    code: Option<JsonCode>,
}

#[derive(Serialize)]
struct JsonChild {
    children: Vec<JsonChild>,
    code: Option<JsonCode>,
    level: &'static str,
    message: String,
    rendered: Option<String>,
    spans: Vec<JsonSpan>,
}

#[derive(Serialize)]
struct JsonCode {
    code: String,
    explanation: Option<String>,
}

#[derive(Serialize)]
struct JsonSpan {
    byte_end: u32,
    byte_start: u32,
    column_end: usize,
    column_start: usize,
    expansion: Option<()>,
    file_name: String,
    is_primary: bool,
    label: Option<String>,
    line_end: usize,
    line_start: usize,
    suggested_replacement: Option<String>,
    suggestion_applicability: Option<&'static str>,
    text: Vec<JsonLine>,
}

#[derive(Serialize)]
struct JsonLine {
    highlight_end: usize,
    highlight_start: usize,
    text: String,
}

pub(super) fn json(diagnostic: &Diag, sources: &mut Sources, rendered: String) -> Json {
    let suggestions = match diagnostic.suggestions_disabled {
        true => &[][..],
        false => &diagnostic.suggestions[..],
    };
    let children = diagnostic.children.iter().map(|child| JsonChild {
        children: Vec::new(),
        code: None,
        level: child.level.name(),
        message: child.message.clone(),
        rendered: None,
        spans: json_spans(&child.span, sources),
    });
    let children: Vec<JsonChild> = children.collect();
    let suggested = suggestions.iter().map(|suggestion| {
        let spans = suggestion.substitutions.iter().flatten().filter_map(|(span, replacement)| {
            let mut it = json_span(span, true, None, sources)?;
            it.suggested_replacement = Some(replacement.clone());
            it.suggestion_applicability = Some(suggestion.applicability.name());
            Some(it)
        });
        JsonChild {
            children: Vec::new(),
            code: None,
            level: "help",
            message: suggestion.message.clone(),
            rendered: None,
            spans: spans.collect(),
        }
    });
    Json {
        rendered,
        message_type: "diagnostic",
        children: children.into_iter().chain(suggested).collect(),
        level: if diagnostic.is_error() { "error" } else { "warning" },
        message: diagnostic.message.clone(),
        spans: json_spans(&diagnostic.span, sources),
        code: diagnostic.code.as_ref().map(|it| JsonCode { code: it.as_str().to_owned(), explanation: None }),
    }
}

fn json_spans(span: &MultiSpan, sources: &mut Sources) -> Vec<JsonSpan> {
    span.span_labels().into_iter().filter_map(|it| json_span(it.span, it.is_primary, it.label, sources)).collect()
}

fn json_span(span: &Span, is_primary: bool, label: Option<&str>, sources: &mut Sources) -> Option<JsonSpan> {
    let file = sources.file(&span.file)?;
    let (line_start, column_start) = file.position(span.lo as usize);
    let (line_end, column_end) = file.position(span.hi as usize);
    let text = (line_start..=line_end).map(|line| {
        let text = file.line_text(line - 1);
        let highlight_start = if line == line_start { column_start + 1 } else { 1 };
        let highlight_end = if line == line_end { column_end + 1 } else { text.chars().count() + 1 };
        JsonLine { highlight_end, highlight_start, text: text.to_owned() }
    });
    Some(JsonSpan {
        byte_end: span.hi,
        byte_start: span.lo,
        column_end: column_end + 1,
        column_start: column_start + 1,
        expansion: None,
        file_name: span.file.clone(),
        is_primary,
        label: label.map(str::to_owned),
        line_end,
        line_start,
        suggested_replacement: None,
        suggestion_applicability: None,
        text: text.collect(),
    })
}

const FILE: &str = "src/lib.rs";
const OTHER: &str = "src/other.rs";

/// ASCII lines with `\n` ends, the last with or without one.
fn text() -> impl Strategy<Value = String> {
    (proptest::collection::vec("[ -~]{0,30}", 1..8), any::<bool>()).prop_map(|(lines, end)| {
        let mut text = lines.join("\n");
        if end {
            text.push('\n');
        }
        text
    })
}

fn span(len: u32, other: u32) -> impl Strategy<Value = Span> {
    (prop::bool::weighted(0.2), 0..=len, 0..=len, 0..=other, 0..=other).prop_map(|(in_other, a, b, c, d)| {
        let (file, lo, hi) = match in_other {
            false => (FILE, a.min(b), a.max(b)),
            true => (OTHER, c.min(d), c.max(d)),
        };
        Span { file: file.to_owned(), lo, hi, expansion: None }
    })
}

fn label() -> impl Strategy<Value = String> {
    "[ -~\t]{0,20}"
}

/// At least one primary span, some labels of their own and some on a primary span.
fn multi(len: u32, other: u32) -> impl Strategy<Value = MultiSpan> {
    (
        proptest::collection::vec(span(len, other), 1..3),
        proptest::collection::vec((span(len, other), label()), 0..3),
        proptest::collection::vec((any::<prop::sample::Index>(), label()), 0..2),
    )
        .prop_map(|(primary, mut labels, on_primary)| {
            for (at, label) in on_primary {
                labels.push((at.get(&primary).clone(), label));
            }
            MultiSpan { primary, labels }
        })
}

fn child(len: u32, other: u32) -> impl Strategy<Value = Child> {
    let level = prop_oneof![Just(Sublevel::Note), Just(Sublevel::Help), Just(Sublevel::Warning)];
    (level, "[ -~]{1,30}", prop::option::of(multi(len, other))).prop_map(|(level, message, span)| Child {
        level,
        message,
        span: span.unwrap_or_default(),
        once: false,
    })
}

/// Never empty: the JSON of an empty replacement of whole lines removes the line, as rustc's
/// does and the reference's did not.
fn replacement() -> impl Strategy<Value = String> {
    prop_oneof!["[ -~]{1,12}", "[ -~]{1,6}\n[ -~]{0,6}", Just("#[inline]\n".to_owned())]
}

/// One or two disjoint parts of the main file.
fn substitution(len: u32) -> impl Strategy<Value = Vec<(Span, String)>> {
    (proptest::collection::vec(0..=len, 4), replacement(), replacement(), any::<bool>()).prop_map(
        |(mut at, first, second, two)| {
            at.sort_unstable();
            let at_file = |lo: u32, hi: u32| Span { file: FILE.to_owned(), lo, hi, expansion: None };
            let mut parts = vec![(at_file(at[0], at[1]), first)];
            if two && at[2] > at[1] {
                parts.push((at_file(at[2], at[3]), second));
            }
            parts
        },
    )
}

fn suggestion(len: u32) -> impl Strategy<Value = Suggestion> {
    let style = prop_oneof![
        Just(SuggestionStyle::HideCodeInline),
        Just(SuggestionStyle::HideCodeAlways),
        Just(SuggestionStyle::CompletelyHidden),
        Just(SuggestionStyle::ShowCode),
        Just(SuggestionStyle::ShowAlways),
    ];
    let applicability = prop_oneof![
        Just(Applicability::MachineApplicable),
        Just(Applicability::MaybeIncorrect),
        Just(Applicability::HasPlaceholders),
        Just(Applicability::Unspecified),
    ];
    ("[a-z]{1,8}( [a-z]{1,8}){0,12}", proptest::collection::vec(substitution(len), 1..7), applicability, style)
        .prop_map(|(message, substitutions, applicability, style)| Suggestion { message, substitutions, applicability, style })
}

fn diag(len: u32, other: u32) -> impl Strategy<Value = Diag> {
    let level = prop_oneof![Just(Level::Error), Just(Level::Warning)];
    (
        level,
        "[ -~]{1,40}",
        multi(len, other),
        proptest::collection::vec(child(len, other), 0..3),
        proptest::collection::vec(suggestion(len), 0..3),
        any::<bool>(),
    )
        .prop_map(|(level, message, span, children, suggestions, disabled)| {
            let mut diag = Diag::new(level, Some(Code::Lint("clippy::needless_bool".to_owned())), message);
            diag.span = span;
            diag.children = children;
            diag.suggestions = suggestions;
            diag.suggestions_disabled = disabled;
            diag
        })
}

fn case() -> impl Strategy<Value = (String, String, Diag)> {
    (text(), text()).prop_flat_map(|(main, other)| {
        let diag = diag(main.len() as u32, other.len() as u32);
        (Just(main), Just(other), diag)
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn the_emitter_writes_what_the_reference_wrote(
        (main, other, diag) in case(),
        short in any::<bool>(),
        ansi in any::<bool>(),
        narrow in any::<bool>(),
    ) {
        let style = Style { short, ansi, width: if narrow { 40 } else { WIDTH }, ui_testing: false };
        let files = [(FILE.to_owned(), main), (OTHER.to_owned(), other)];
        let mut ours = super::Sources::of(files.clone());
        let mut theirs = Sources::of(files);
        let rendered = written(&diag, &mut theirs, style);
        prop_assert_eq!(&super::written(&diag, &mut ours, style), &rendered);
        let json: serde_json::Value = serde_json::from_str(&super::json(&diag, &mut ours, style)).unwrap();
        let expected = serde_json::to_value(self::json(&diag, &mut theirs, rendered.clone())).unwrap();
        prop_assert_eq!(&json, &expected);
        let given: serde_json::Value = serde_json::from_str(&super::json_rendered(&diag, &mut ours, rendered)).unwrap();
        prop_assert_eq!(given, json);
    }
}
