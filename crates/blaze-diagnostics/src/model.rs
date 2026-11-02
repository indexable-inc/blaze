//! A diagnostic as data: what rustc's `DiagInner` holds of one (`compiler/rustc_errors/src/
//! diagnostic.rs`), which is what [`crate::emit`] writes as rustc does, with the facts the
//! query that found it read. It is kept and sent as it is; the text of the files it points
//! into is read where it is written, by its path.

use blaze_items::Fact;
use serde::{Deserialize, Serialize};
use syntax::TextRange;

/// The level of a diagnostic: rustc's `Level`, of those a diagnostic of the compiler is
/// written at.
/// Not [`blaze_api::Level`]: that is the agent API's versioned schema, which changes only with
/// its version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Level {
    Error,
    Warning,
    Note,
    Help,
    /// rustc's `FailureNote`: "For more information about this error, ...", written with no
    /// `note: ` before it.
    FailureNote,
}

impl Level {
    /// As rustc's JSON and its text name it.
    pub fn name(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warning => "warning",
            Level::Note => "note",
            Level::Help => "help",
            Level::FailureNote => "failure-note",
        }
    }
}

/// What a diagnostic is: rustc's error code, or the lint it is of.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Code {
    /// `E0308`.
    Error(String),
    /// As rustc's JSON names it: `unused_variables`, `clippy::needless_bool`.
    Lint(String),
    /// A lint whose diagnostic has an error code too (`unsafe_op_in_unsafe_fn`'s `E0133`): a
    /// lint for its level, its code in its title, its JSON and the closing lines.
    LintWithCode { lint: String, code: String },
}

impl Code {
    /// The code rustc shows: the error code, else the lint's name.
    pub fn as_str(&self) -> &str {
        match self {
            Code::Error(it) | Code::Lint(it) | Code::LintWithCode { code: it, .. } => it,
        }
    }

    /// The error code, if it has one.
    pub fn error(&self) -> Option<&str> {
        match self {
            Code::Error(it) | Code::LintWithCode { code: it, .. } => Some(it),
            Code::Lint(_) => None,
        }
    }
}

/// A range of a file: bytes from 0. `file` is the path as rustc shows it: from the root of the
/// workspace when the file is below it (cargo runs rustc there), else the whole path; or, for a
/// file with no path, the name it is shown by and its text (`emit::name_with_text`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Span {
    pub file: String,
    pub lo: u32,
    pub hi: u32,
    /// The macro whose expansion it is in, when it is in one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expansion: Option<Box<Expansion>>,
}

/// A macro's expansion a span is in: rustc's `ExpnData`, as much of it as diagnostics show.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Expansion {
    /// Where the macro was called: its call, or for a derive the path in `#[derive]`.
    pub call_site: Span,
    pub kind: MacroKind,
    pub name: String,
    /// Where the macro is defined, when rustc says.
    pub def_site: Option<Span>,
    /// rustc's `ExpnData::diagnostic_opaque`: the macro is a builtin one (or marked
    /// `#[diagnostic::opaque]`), which no note names as where a diagnostic originates.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub opaque: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MacroKind {
    Bang,
    Attr,
    Derive,
}

impl MacroKind {
    /// As rustc's notes name it.
    pub fn descr(self) -> &'static str {
        match self {
            MacroKind::Bang => "macro",
            MacroKind::Attr => "attribute macro",
            MacroKind::Derive => "derive macro",
        }
    }

    /// rustc's `MacroKind::descr_expected`.
    pub(crate) fn expected(self) -> &'static str {
        match self {
            MacroKind::Bang => "macro",
            MacroKind::Attr => "attribute",
            MacroKind::Derive => "derive macro",
        }
    }

    /// rustc's `MacroKind::article`.
    pub(crate) fn article(self) -> &'static str {
        match self {
            MacroKind::Attr => "an",
            MacroKind::Bang | MacroKind::Derive => "a",
        }
    }
}

impl Expansion {
    /// As rustc's JSON names it: `name!`, `#[name]`, `#[derive(Name)]`.
    pub fn decl_name(&self) -> String {
        match self.kind {
            MacroKind::Bang => format!("{}!", self.name), // alloc: the JSON of a diagnostic written
            MacroKind::Attr => format!("#[{}]", self.name), // alloc: the JSON of a diagnostic written
            MacroKind::Derive => format!("#[derive({})]", self.name), // alloc: the JSON of a diagnostic written
        }
    }
}

impl Span {
    pub fn new(file: &str, range: TextRange) -> Span {
        Span {
            // alloc: a diagnostic's span; its callers make one for what they report
            file: file.to_owned(),
            lo: range.start().into(),
            hi: range.end().into(),
            expansion: None,
        }
    }

    pub fn range(&self) -> TextRange {
        TextRange::new(self.lo.into(), self.hi.into())
    }

    pub fn to(&self, other: &Span) -> Span {
        // clone: a new span of a diagnostic being built, the edge
        Span { lo: self.lo.min(other.lo), hi: self.hi.max(other.hi), ..self.clone() }
    }

    pub fn with_lo(&self, lo: u32) -> Span {
        Span { lo, ..self.clone() } // clone: a new span of a diagnostic being built, the edge
    }

    pub fn with_hi(&self, hi: u32) -> Span {
        Span { hi, ..self.clone() } // clone: a new span of a diagnostic being built, the edge
    }

    pub fn shrink_to_lo(&self) -> Span {
        Span { hi: self.lo, ..self.clone() } // clone: a new span of a diagnostic being built, the edge
    }

    pub fn shrink_to_hi(&self) -> Span {
        Span { lo: self.hi, ..self.clone() } // clone: a new span of a diagnostic being built, the edge
    }

    pub fn is_empty(&self) -> bool {
        self.lo == self.hi
    }

    /// rustc's `Span::macro_backtrace`: the expansions it is in, innermost first, leaving out
    /// a macro's recursive calls of itself.
    pub fn macro_backtrace(&self) -> impl Iterator<Item = &Expansion> {
        let mut prev: Option<&Span> = None;
        let mut at = self;
        std::iter::from_fn(move || {
            loop {
                let expansion = at.expansion.as_deref()?;
                let recursive = prev.is_some_and(|it| it.source_equal(&expansion.call_site));
                prev = Some(at);
                at = &expansion.call_site;
                if !recursive {
                    return Some(expansion);
                }
            }
        })
    }

    /// rustc's `Span::source_equal`: the same bytes of the same file.
    pub fn source_equal(&self, other: &Span) -> bool {
        self.file == other.file && self.lo == other.lo && self.hi == other.hi
    }
}

/// rustc's `MultiSpan`: the places a diagnostic is of, and the labels on places.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MultiSpan {
    pub primary: Vec<Span>,
    pub labels: Vec<(Span, String)>,
}

/// A span with whether it is one of the primary ones and its label: rustc's `SpanLabel`.
#[derive(Clone, Debug, PartialEq)]
pub struct SpanLabel<'a> {
    pub span: &'a Span,
    pub is_primary: bool,
    pub label: Option<&'a str>,
}

impl From<Span> for MultiSpan {
    fn from(span: Span) -> MultiSpan {
        MultiSpan::of(span)
    }
}

impl MultiSpan {
    pub fn of(span: Span) -> MultiSpan {
        MultiSpan { primary: vec![span], labels: Vec::new() } // alloc: a diagnostic's span, the edge
    }

    pub fn is_empty(&self) -> bool {
        self.primary.is_empty() && self.labels.is_empty()
    }

    pub fn primary_span(&self) -> Option<&Span> {
        self.primary.first()
    }

    /// rustc's `MultiSpan::span_labels`: the labelled spans in the order they were labelled,
    /// then the primary spans that have no label.
    pub fn span_labels(&self) -> Vec<SpanLabel<'_>> {
        let mut labels: Vec<SpanLabel<'_>> = self
            .labels
            .iter()
            .map(|(span, label)| SpanLabel {
                span,
                is_primary: self.primary.contains(span),
                label: Some(label),
            })
            .collect(); // alloc: a diagnostic's labels, written or rendered, the edge
        for span in &self.primary {
            if !labels.iter().any(|it| it.span == span) {
                labels.push(SpanLabel { span, is_primary: true, label: None });
            }
        }
        labels
    }
}

/// The level of a part of a diagnostic: rustc's levels of a `Subdiag`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Sublevel {
    Note,
    Help,
    Warning,
}

impl Sublevel {
    pub fn name(self) -> &'static str {
        match self {
            Sublevel::Note => "note",
            Sublevel::Help => "help",
            Sublevel::Warning => "warning",
        }
    }
}

/// A note, a help or a warning under a diagnostic: rustc's `Subdiag`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Child {
    pub level: Sublevel,
    pub message: String,
    pub span: MultiSpan,
    /// rustc's `note_once` and `help_once`: written with the first diagnostic of a crate that
    /// has it, and left out of the others.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub once: bool,
}

/// How sure a suggestion is: rustc's `Applicability`, the agent API's type.
pub use blaze_api::Applicability;

/// How a suggestion is shown: rustc's `SuggestionStyle`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SuggestionStyle {
    /// Its message only, never its code.
    HideCodeInline,
    HideCodeAlways,
    /// Only for tools: not written.
    CompletelyHidden,
    /// Inline when it is short, else as a patch.
    ShowCode,
    /// Always as a patch.
    ShowAlways,
}

impl SuggestionStyle {
    pub fn hide_inline(self) -> bool {
        !matches!(self, SuggestionStyle::ShowCode)
    }
}

/// A suggested change: one or more ways to substitute parts of the source: rustc's
/// `CodeSuggestion`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Suggestion {
    pub message: String,
    /// Each way to substitute, each a set of disjoint parts.
    pub substitutions: Vec<Vec<(Span, String)>>,
    pub applicability: Applicability,
    pub style: SuggestionStyle,
}

/// A diagnostic, as rustc emits it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Diag {
    /// For a lint, the level it is written at once [`crate::lints`] decided it: `Warning` or
    /// `Error`.
    pub level: Level,
    pub code: Option<Code>,
    pub message: String,
    pub span: MultiSpan,
    pub children: Vec<Child>,
    pub suggestions: Vec<Suggestion>,
    /// rustc turns off the suggestions of a diagnostic in code of another crate's macro.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub suggestions_disabled: bool,
    /// The facts the query that found it read, narrowest first: what to change to make it go
    /// away, and why it came back after an edit (`blaze_api::Diagnostic::facts`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<Read>,
    /// rustc's `emit_fatal` (`FatalError`, `ShouldEmit::EarlyFatal`): nothing after it, in
    /// rustc's order, is written.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fatal: bool,
    /// rustc's `has_future_breakage`: a lint whose future-incompatibility is reported
    /// (`report_in_deps`), and where it is written.
    #[serde(default, skip_serializing_if = "Breakage::is_none")]
    pub breakage: Breakage,
}

/// Where a diagnostic of rustc's future-incompatibility report is written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Breakage {
    /// Not in the report.
    #[default]
    None,
    /// To stderr, and in the report.
    Written,
    /// Allowed or expected: only in the report, as a warning.
    Unwritten,
    /// The report's copy of one (`--error-format=json`'s `future_incompat` message), written
    /// after the closing lines.
    Reported,
}

impl Breakage {
    pub fn is_none(&self) -> bool {
        *self == Breakage::None
    }
}

/// A fact a diagnostic read, and what it stands for in words, written where the names are
/// known (a fact's key is a hash): "what `len` is on `Vec<u8>`".
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Read {
    pub fact: Fact,
    pub what: String,
}

impl Diag {
    pub fn new(level: Level, code: Option<Code>, message: impl Into<String>) -> Diag {
        Diag {
            level,
            code,
            message: message.into(),
            span: MultiSpan::default(),
            children: Vec::new(),    // alloc: empty, allocates nothing
            suggestions: Vec::new(), // alloc: empty, allocates nothing
            suggestions_disabled: false,
            facts: Vec::new(), // alloc: empty, allocates nothing
            fatal: false,
            breakage: Breakage::None,
        }
    }

    /// Marks this error fatal: rustc stops at it.
    pub fn stop(&mut self) -> &mut Diag {
        assert!(self.is_error() && !self.is_lint(), "only an error rustc gives is fatal: {:?}", self.message);
        self.fatal = true;
        self
    }

    /// An error with rustc's code `code` (`"E0308"`), or none.
    pub fn error(code: Option<&str>, message: impl Into<String>, span: Span) -> Diag {
        // alloc: the code of an error being built, the edge
        let mut diag = Diag::new(Level::Error, code.map(|it| Code::Error(it.to_owned())), message);
        diag.span = MultiSpan::of(span);
        diag
    }

    /// A diagnostic of the lint `lint`, at `warn` until [`crate::lints`] decides its level.
    pub fn lint(lint: &str, message: impl Into<String>, span: Span) -> Diag {
        // alloc: the name of a lint being built, the edge
        let mut diag = Diag::new(Level::Warning, Some(Code::Lint(lint.to_owned())), message);
        diag.span = MultiSpan::of(span);
        diag
    }

    pub fn is_error(&self) -> bool {
        self.level == Level::Error
    }

    /// A lint of rustc's whose diagnostic has error code `code` (rustc's `.with_code` on a
    /// lint's diagnostic).
    pub fn lint_with_code(lint: &str, code: &str, message: impl Into<String>, span: Span) -> Diag {
        // alloc: the name and code of a lint being built, the edge
        let code = Code::LintWithCode { lint: lint.to_owned(), code: code.to_owned() };
        let mut diag = Diag::new(Level::Warning, Some(code), message);
        diag.span = MultiSpan::of(span);
        diag
    }

    pub fn is_lint(&self) -> bool {
        self.lint_name().is_some()
    }

    /// The lint it is of, if it is a lint's.
    pub fn lint_name(&self) -> Option<&str> {
        match &self.code {
            Some(Code::Lint(it) | Code::LintWithCode { lint: it, .. }) => Some(it),
            _ => None,
        }
    }

    pub fn note(&mut self, message: impl Into<String>) -> &mut Diag {
        self.sub(Sublevel::Note, message, MultiSpan::default(), false)
    }

    pub fn help(&mut self, message: impl Into<String>) -> &mut Diag {
        self.sub(Sublevel::Help, message, MultiSpan::default(), false)
    }

    pub fn warn(&mut self, message: impl Into<String>) -> &mut Diag {
        self.sub(Sublevel::Warning, message, MultiSpan::default(), false)
    }

    pub fn span_note(&mut self, span: impl Into<MultiSpan>, message: impl Into<String>) -> &mut Diag {
        self.sub(Sublevel::Note, message, span.into(), false)
    }

    pub fn span_help(&mut self, span: impl Into<MultiSpan>, message: impl Into<String>) -> &mut Diag {
        self.sub(Sublevel::Help, message, span.into(), false)
    }

    pub fn sub(
        &mut self,
        level: Sublevel,
        message: impl Into<String>,
        span: MultiSpan,
        once: bool,
    ) -> &mut Diag {
        self.children.push(Child { level, message: message.into(), span, once });
        self
    }

    /// A label on `span`, which is primary when it is one of the primary spans.
    pub fn span_label(&mut self, span: Span, label: impl Into<String>) -> &mut Diag {
        self.span.labels.push((span, label.into()));
        self
    }

    /// Another primary span, as rustc's `MultiSpan::from_spans`.
    pub fn primary(&mut self, span: Span) -> &mut Diag {
        self.span.primary.push(span);
        self
    }

    /// rustc's `span_suggestion`: shown inline when it is short.
    pub fn suggestion(
        &mut self,
        span: Span,
        message: impl Into<String>,
        code: impl Into<String>,
        applicability: Applicability,
    ) -> &mut Diag {
        self.suggestion_with_style(
            vec![(span, code.into())], // alloc: a suggestion being built, the edge
            message,
            applicability,
            SuggestionStyle::ShowCode,
        )
    }

    /// rustc's `span_suggestion_verbose`: always shown as a patch.
    pub fn suggestion_verbose(
        &mut self,
        span: Span,
        message: impl Into<String>,
        code: impl Into<String>,
        applicability: Applicability,
    ) -> &mut Diag {
        self.suggestion_with_style(
            vec![(span, code.into())], // alloc: a suggestion being built, the edge
            message,
            applicability,
            SuggestionStyle::ShowAlways,
        )
    }

    /// rustc's `span_suggestions_with_style`: one message, a way to substitute for each of
    /// `codes`. rustc's `span_suggestions` is this with [`SuggestionStyle::ShowAlways`].
    pub fn suggestions(
        &mut self,
        span: Span,
        message: impl Into<String>,
        codes: impl IntoIterator<Item = String>,
        applicability: Applicability,
        style: SuggestionStyle,
    ) -> &mut Diag {
        // alloc: the substitutions of a suggestion being built, the edge
        let substitutions = codes.into_iter().map(|code| vec![(span.clone(), code)]).collect();
        self.suggestions.push(Suggestion {
            message: message.into(),
            substitutions,
            applicability,
            style,
        });
        self
    }

    /// rustc's `multipart_suggestion`: the parts are one change, shown as a patch even when
    /// there is one.
    pub fn multipart_suggestion(
        &mut self,
        message: impl Into<String>,
        parts: Vec<(Span, String)>,
        applicability: Applicability,
    ) -> &mut Diag {
        self.suggestion_with_style(parts, message, applicability, SuggestionStyle::ShowAlways)
    }

    pub fn suggestion_with_style(
        &mut self,
        mut parts: Vec<(Span, String)>,
        message: impl Into<String>,
        applicability: Applicability,
        style: SuggestionStyle,
    ) -> &mut Diag {
        let mut seen = blaze_hash::HashSet::default();
        // clone: the parts of a suggestion being built, the edge; `retain` keeps no borrow of them
        parts.retain(|(span, code)| seen.insert((span.lo, span.hi, code.clone())));
        self.suggestions.push(Suggestion {
            message: message.into(),
            substitutions: vec![parts], // alloc: a suggestion being built, the edge
            applicability,
            style,
        });
        self
    }

    /// Records a fact the query that found it read, and what it stands for.
    pub fn fact(&mut self, fact: Fact, what: impl Into<String>) -> &mut Diag {
        if !self.facts.iter().any(|it| it.fact == fact) {
            self.facts.push(Read { fact, what: what.into() });
        }
        self
    }

    /// [`Diag::fact`] of a fact that may not be had (a type with no simplified form).
    pub fn fact_opt(&mut self, fact: Option<Fact>, what: impl Into<String>) -> &mut Diag {
        match fact {
            Some(fact) => self.fact(fact, what),
            None => self,
        }
    }

    /// rustc's note on a diagnostic in a macro's expansion, as a child: which macro it came
    /// from (`fix_multispans_in_extern_macros_and_render_macro_backtrace` without
    /// `-Z macro-backtrace`). The emitter writes it itself when no child has it.
    pub fn originates_in(&mut self) -> &mut Diag {
        match self.origin_note() {
            Some(note) => self.note(note),
            None => self,
        }
    }

    /// The text of [`Diag::originates_in`]'s note, when a primary span of it or of a child is
    /// in a macro's expansion: the innermost macro, and the outermost when it is another.
    pub fn origin_note(&self) -> Option<String> {
        let spans = self.span.primary.iter().chain(self.children.iter().flat_map(|it| &it.span.primary));
        let mut macros = spans.flat_map(Span::macro_backtrace);
        let first = macros.next()?;
        let last = macros.last().unwrap_or(first);
        if last.opaque {
            return None;
        }
        let and_then = match last.name != first.name {
            // alloc: the note of a diagnostic in a macro, the edge
            true => format!(" which comes from the expansion of the {} `{}`", last.kind.descr(), last.name),
            false => String::new(), // alloc: empty, allocates nothing
        };
        // alloc: the note of a diagnostic in a macro, the edge
        Some(format!(
            "this {} originates in the {} `{}`{and_then} (in Nightly builds, run with -Z macro-backtrace for more info)",
            self.level.name(),
            first.kind.descr(),
            first.name
        ))
    }

    /// Whether cargo counts it as one `cargo fix` (`cargo clippy --fix`) applies: it has a
    /// suggestion rustfix takes as it is.
    pub fn is_fixable(&self) -> bool {
        !self.suggestions_disabled
            && self.suggestions.iter().any(|it| it.applicability == Applicability::MachineApplicable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(lo: u32, hi: u32) -> Span {
        Span { file: "src/lib.rs".to_owned(), lo, hi, expansion: None }
    }

    #[test]
    fn labels_come_before_unlabelled_primaries() {
        let mut diag = Diag::error(Some("E0308"), "mismatched types", span(4, 9));
        diag.primary(span(20, 22));
        diag.span_label(span(20, 22), "expected `u32`");
        diag.span_label(span(0, 3), "expected due to this");
        let labels = diag.span.span_labels();
        let seen: Vec<_> = labels.iter().map(|it| (it.span.lo, it.is_primary, it.label)).collect();
        assert_eq!(
            seen,
            [
                (20, true, Some("expected `u32`")),
                (0, false, Some("expected due to this")),
                (4, true, None)
            ]
        );
    }

    #[test]
    fn a_suggestion_drops_repeated_parts() {
        let mut diag = Diag::error(None, "x", span(0, 1));
        diag.multipart_suggestion(
            "add them",
            vec![(span(0, 0), "a".to_owned()), (span(0, 0), "a".to_owned())],
            Applicability::MachineApplicable,
        );
        assert_eq!(diag.suggestions[0].substitutions[0].len(), 1);
    }

    #[test]
    fn a_lint_with_an_error_code_is_a_lint_that_shows_the_code() {
        let lint = Diag::lint_with_code("unsafe_op_in_unsafe_fn", "E0133", "call to unsafe function", span(0, 1));
        assert!(lint.is_lint());
        assert_eq!(lint.lint_name(), Some("unsafe_op_in_unsafe_fn"));
        assert_eq!(lint.code.as_ref().map(Code::as_str), Some("E0133"));
        assert_eq!(lint.code.as_ref().and_then(Code::error), Some("E0133"));
        let plain = Diag::lint("unused_variables", "unused variable: `x`", span(0, 1));
        assert_eq!(plain.code.as_ref().and_then(Code::error), None);
    }

    #[test]
    fn a_diagnostic_round_trips_in_json() {
        let mut diag = Diag::lint("unused_variables", "unused variable: `x`", span(8, 9));
        diag.fact(Fact::Code(blaze_items::ItemKey(blaze_ids::ContentId([1; 32]))), "the code of `f`");
        let text = serde_json::to_string(&diag).unwrap();
        let back: Diag = serde_json::from_str(&text).unwrap();
        assert_eq!(back, diag);
    }

    #[test]
    fn the_note_of_a_macro_names_the_outermost_one_it_comes_from() {
        let inner = Expansion {
            call_site: span(30, 40),
            kind: MacroKind::Bang,
            name: "assert_eq".to_owned(),
            def_site: None,
            opaque: false,
        };
        let mut at = span(30, 40);
        at.expansion = Some(Box::new(inner));
        let mut diag = Diag::error(Some("E0277"), "x", at);
        diag.originates_in();
        assert_eq!(
            diag.children[0].message,
            "this error originates in the macro `assert_eq` (in Nightly builds, run with -Z macro-backtrace for more info)"
        );
    }

    /// rustc leaves the note out when the outermost macro is a builtin one, as `#[derive(Copy)]`.
    #[test]
    fn no_note_names_an_opaque_macro() {
        let mut at = span(30, 40);
        at.expansion = Some(Box::new(Expansion {
            call_site: span(0, 4),
            kind: MacroKind::Derive,
            name: "Copy".to_owned(),
            def_site: None,
            opaque: true,
        }));
        assert_eq!(Diag::error(Some("E0184"), "x", at).origin_note(), None);
    }

    fn expanded(lo: u32, hi: u32, name: &str, call_site: Span) -> Span {
        Span {
            file: "a.rs".to_owned(),
            lo,
            hi,
            expansion: Some(Box::new(Expansion {
                call_site,
                kind: MacroKind::Bang,
                name: name.to_owned(),
                def_site: None,
                opaque: false,
            })),
        }
    }

    #[test]
    fn the_backtrace_is_innermost_first() {
        let root = Span { file: "a.rs".to_owned(), lo: 0, hi: 10, expansion: None };
        let at = expanded(3, 4, "inner", expanded(20, 30, "outer", root));
        let names: Vec<&str> = at.macro_backtrace().map(|it| it.name.as_str()).collect();
        assert_eq!(names, ["inner", "outer"]);
    }

    #[test]
    fn a_call_where_the_span_two_steps_back_is_is_left_out_of_the_backtrace() {
        let root = Span { file: "a.rs".to_owned(), lo: 3, hi: 4, expansion: None };
        let at = expanded(3, 4, "m", expanded(20, 30, "m", root));
        let names: Vec<&str> = at.macro_backtrace().map(|it| it.name.as_str()).collect();
        assert_eq!(names, ["m"]);
    }

    #[test]
    fn the_note_leaves_out_a_recursive_call_as_the_emitter_does() {
        let root = Span { file: "a.rs".to_owned(), lo: 3, hi: 4, expansion: None };
        let at = expanded(3, 4, "a", expanded(20, 30, "b", root));
        let mut diag = Diag::error(None, "x", at);
        diag.originates_in();
        assert_eq!(
            diag.children[0].message,
            "this error originates in the macro `a` (in Nightly builds, run with -Z macro-backtrace for more info)"
        );
        assert_eq!(diag.origin_note().as_deref(), Some(diag.children[0].message.as_str()));
    }

    #[test]
    fn the_note_names_the_level_as_rustc_does() {
        let root = Span { file: "a.rs".to_owned(), lo: 0, hi: 10, expansion: None };
        let mut diag = Diag::new(Level::Note, None, "x");
        diag.span = MultiSpan::of(expanded(3, 4, "m", root));
        assert!(diag.origin_note().unwrap().starts_with("this note originates in the macro `m`"));
    }

    #[test]
    fn a_machine_applicable_suggestion_is_fixable_unless_suggestions_are_off() {
        let mut diag = Diag::lint("clippy::needless_bool", "x", span(0, 1));
        assert!(!diag.is_fixable());
        diag.suggestion(span(0, 1), "y", "z", Applicability::MaybeIncorrect);
        assert!(!diag.is_fixable());
        diag.suggestion(span(0, 1), "y", "z", Applicability::MachineApplicable);
        assert!(diag.is_fixable());
        diag.suggestions_disabled = true;
        assert!(!diag.is_fixable());
    }
}
