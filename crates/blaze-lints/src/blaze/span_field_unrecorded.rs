//! `span_field_unrecorded`: a field of a `tracing` span or event of the target `blaze` that
//! the profile keeps nothing of. An ad hoc span's fields reach the profile only through the
//! columns of `spans` that hold a name ([`blaze_profile::span::ad_hoc_fields`]: `crate`,
//! `function`, `unit` and their other field names, `awaits`, and `held`, which the recorder
//! reads): a field of any other name is formatted at every call and never seen, so a span
//! written to say what a wait is for (`what = ..`) says nothing in the profile. An event's
//! fields are rows of `event_fields`, each by its place among them
//! ([`blaze_profile::flight::event_field_recorded`]), whatever its name.
//!
//! The list of field names is the profile's own, made from the declaration of the `spans`
//! columns: this lint holds none.
//!
//! Read from the call as written, its arguments as tokens (a macro's arguments have no other
//! form): the span and event macros of the crate `tracing` ([`tracing_macro`]) whose target
//! is the literal `"blaze"`. After `target:`, `parent:` and `name:`, the level of `span!` and
//! `event!` and a span's name, each argument is a field: `name = value`, `name`, `%name` or
//! `?name`, the name maybe dotted (`a.b`). An event's fields end at its message. A call below
//! the level the recorder keeps is not read (nothing of it is kept, which is its level's
//! choice), nor is one whose target, level or span name is not a literal, a `{ .. }` group of
//! fields, or a call a macro writes.

use syntax::{
    SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _},
};

use super::{
    Lint,
    unrecorded_span::{Arg, Made, each_arg, tracing_macro, unquoted},
};
use crate::check::Cx;

const LINT: Lint = Lint::SpanFieldUnrecorded;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let Some(call) = ast::MacroCall::cast(node) else { return };
    let Some((made, level)) = tracing_macro(cx, call) else { return };
    let Some(tt) = call.token_tree() else { return };
    // The tree's offsets are the file's only where the call is written as it is in the tree.
    let Some(range) = cx.range(node) else { return };
    let written = node.text_range();
    if range.len() != written.len() {
        return;
    }
    let mut read = Read {
        made,
        kept: level.map(|it| keeps_level(made, it)),
        blaze: false,
        name: None,
        fields: 0,
        over: false,
    };
    each_arg(tt, |arg| {
        let Some(field) = read.field(arg) else { return };
        let name = text_in(tt, field.name);
        // A raw identifier names the field without its `r#`.
        let name = name.strip_prefix("r#").unwrap_or(name);
        let message = match made {
            Made::Span if blaze_profile::span::ad_hoc_field_recorded(name) => return,
            Made::Event if blaze_profile::flight::event_field_recorded(field.at) => return,
            Made::Span => {
                // alloc: a message's part
                let mut kept = String::new();
                for (at, field) in blaze_profile::span::ad_hoc_fields().enumerate() {
                    if at > 0 {
                        kept.push_str(", ");
                    }
                    kept.push('`');
                    kept.push_str(field);
                    kept.push('`');
                }
                let span = read.name.map_or("this span", |it| it.text());
                format!(
                    "the profile keeps nothing of the field `{name}` of the span {span}: `spans` has no column a span's field of that name fills (the fields that reach one: {kept})"
                )
            }
            Made::Event => format!(
                "the profile keeps nothing of the field `{name}` of this event: `event_fields` keeps an event's first {} fields",
                blaze_profile::flight::EVENT_FIELDS
            ),
        };
        let at = TextRange::at(
            range.start() + (field.name.start() - written.start()),
            field.name.len(),
        );
        let span = cx.span(at);
        cx.blaze(LINT, node, span, message)
            .help(LINT.help())
            .help(format!("for further information visit {}", LINT.docs_url()));
    });
}

/// Whether the recorder keeps a span or an event of the target `blaze` at the level `tracing`
/// names `level` (`"info"`, `"DEBUG"`); false for a name that is no level's.
fn keeps_level(made: Made, level: &str) -> bool {
    match made {
        Made::Span => blaze_profile::keeps_level(level),
        Made::Event => {
            blaze_profile::logs::keeps_event_named(blaze_profile::TARGET, "event", level)
        }
    }
}

/// The text of `range`, a range of the tree `tt` is in, inside `tt`.
fn text_in<'t>(tt: ast::TokenTree<'t>, range: TextRange) -> &'t str {
    let start = tt.syntax().text_range().start();
    let from = usize::from(range.start() - start);
    let to = usize::from(range.end() - start);
    tt.syntax().text().get(from..to).unwrap_or_default()
}

/// A field as a call names it.
struct Field {
    /// Where its name is written, in the tree's offsets: `a`, or all of a dotted `a.b`.
    name: TextRange,
    /// Its place among the call's fields, from 0.
    at: usize,
}

/// What was read of a call's arguments so far, in their order.
struct Read<'t> {
    made: Made,
    /// Whether the recorder keeps its level: none until the level `span!` or `event!` takes
    /// as an argument is read.
    kept: Option<bool>,
    /// Whether it names `target: "blaze"`.
    blaze: bool,
    /// A span's name, once read.
    name: Option<ast::Token<'t>>,
    /// The fields read.
    fields: usize,
    /// Nothing more of the call is read: it is not of the target `blaze`, below the level
    /// kept, its fields ended (an event's message), or an argument was none of the forms this
    /// reads.
    over: bool,
}

impl<'t> Read<'t> {
    /// Takes the call's next argument: the field it names, once the arguments before its
    /// fields are past.
    fn field(&mut self, arg: &Arg<'t>) -> Option<Field> {
        if self.over {
            return None;
        }
        match arg.key(SyntaxKind::COLON) {
            Some("target") => {
                self.blaze =
                    arg.value().is_some_and(|it| unquoted(it.text()) == blaze_profile::TARGET);
                return None;
            }
            Some("parent" | "name") => return None,
            _ => {}
        }
        // A target comes before everything read from here.
        if !self.blaze {
            self.over = true;
            return None;
        }
        let Some(kept) = self.kept else {
            // The level `span!` and `event!` take first, by its last token (`INFO` of
            // `tracing::Level::INFO`).
            let kept = arg.last.is_some_and(|it| keeps_level(self.made, unquoted(it.text())));
            self.kept = Some(kept);
            self.over = !kept;
            return None;
        };
        if !kept {
            self.over = true;
            return None;
        }
        if matches!(self.made, Made::Span) && self.name.is_none() {
            match arg.string() {
                Some(name) => self.name = Some(name),
                None => self.over = true,
            }
            return None;
        }
        let (first, sigil) = match arg.first {
            [Some(first), ..] if first.kind() == SyntaxKind::IDENT => (first, false),
            [Some(sigil), Some(first), _]
                if matches!(sigil.kind(), SyntaxKind::PERCENT | SyntaxKind::QUESTION)
                    && first.kind() == SyntaxKind::IDENT =>
            {
                (first, true)
            }
            // An event's message, and what it formats after it; or a form not read.
            _ => {
                self.over = true;
                return None;
            }
        };
        let (name, after) = name_from(first);
        let named = match after.map(|it| it.kind()) {
            // `name = value`; a sigil goes on a value or on a name that is its own value.
            Some(SyntaxKind::EQ) => !sigil,
            // `name` alone, the argument's end after it.
            Some(
                SyntaxKind::COMMA
                | SyntaxKind::R_PAREN
                | SyntaxKind::R_BRACK
                | SyntaxKind::R_CURLY,
            )
            | None => true,
            Some(_) => false,
        };
        if !named {
            self.over = true;
            return None;
        }
        let at = self.fields;
        self.fields += 1;
        Some(Field { name, at })
    }
}

/// The field name that begins at the identifier `first`, `a` or a dotted `a.b.c`: where it is
/// written, and the token after it.
fn name_from<'t>(first: ast::Token<'t>) -> (TextRange, Option<ast::Token<'t>>) {
    let mut end: TextSize = first.text_range().end();
    let mut after = next(first);
    while let Some(dot) = after
        && dot.kind() == SyntaxKind::DOT
        && let Some(part) = next(dot)
        && part.kind() == SyntaxKind::IDENT
    {
        end = part.text_range().end();
        after = next(part);
    }
    (TextRange::new(first.text_range().start(), end), after)
}

/// The token after `token`, trivia skipped.
fn next<'t>(token: ast::Token<'t>) -> Option<ast::Token<'t>> {
    let mut next = token.next_token();
    while let Some(it) = next
        && it.kind().is_trivia()
    {
        next = it.next_token();
    }
    next
}

#[cfg(test)]
mod tests {
    use crate::tests::lint;

    const TRACING: &str = r#"
//- /tracing/lib.rs crate:tracing
pub struct Span;
pub struct Level;
impl Level {
    pub const INFO: Level = Level;
    pub const DEBUG: Level = Level;
}
#[macro_export]
macro_rules! span { ($($t:tt)*) => { $crate::Span }; }
#[macro_export]
macro_rules! info_span { ($($t:tt)*) => { $crate::span!($($t)*) }; }
#[macro_export]
macro_rules! debug_span { ($($t:tt)*) => { $crate::span!($($t)*) }; }
#[macro_export]
macro_rules! event { ($($t:tt)*) => { () }; }
#[macro_export]
macro_rules! info { ($($t:tt)*) => { $crate::event!($($t)*) }; }
"#;

    const FLAGS: &str = "-A clippy::all -W blaze::span_field_unrecorded";

    const NOTHING: &str = "the profile keeps nothing of the field";

    fn fixture(main: &str) -> String {
        format!("//- minicore: sized\n//- /src/lib.rs crate:main deps:tracing\n{main}\n{TRACING}")
    }

    /// Every field a span names that fills a column of `spans` passes, however it is written;
    /// one that fills none is found by its name, with the span's and the fields that do.
    #[test]
    fn a_spans_field_with_no_column_is_found_by_name_and_one_with_a_column_is_not() {
        let found = lint(
            &fixture(
                r#"
fn spans(what: &'static str, krate: &str, placing: u32, job: Job) {
    let _a = tracing::info_span!(target: "blaze", "every field has a column", function = what, awaits = what, krate, %krate, unit = ?placing, held = true);
    let _b = tracing::info_span!(target: "blaze", "the answer waits", function = what, what, placing = placing + 1);
    let _c = tracing::span!(target: "blaze", tracing::Level::INFO, "spanned", runs = 3);
    let _d = tracing::info_span!(target: "blaze", "dotted and marked", job.id = 4, ?job, owner = %job.owner);
    let _e = tracing::info_span!(target: "blaze", "no fields");
}
pub struct Job { owner: u32 }
"#,
            ),
            FLAGS,
        );
        assert!(!found.contains("every field has a column"), "{found}");
        assert!(!found.contains("no fields"), "{found}");
        assert!(found.contains("the field `what` of the span \"the answer waits\""), "{found}");
        assert!(found.contains("the field `placing` of the span \"the answer waits\""), "{found}");
        assert!(found.contains("the field `runs` of the span \"spanned\""), "{found}");
        assert!(found.contains("the field `job.id` of the span \"dotted and marked\""), "{found}");
        assert!(found.contains("the field `job` of the span \"dotted and marked\""), "{found}");
        // A column of numbers is a span kind's to write, never an ad hoc span's.
        assert!(found.contains("the field `owner` of the span \"dotted and marked\""), "{found}");
        assert_eq!(found.matches(NOTHING).count(), 6, "{found}");
        // The message lists the profile's own list, whatever it holds.
        let mut kept = String::new();
        for field in blaze_profile::span::ad_hoc_fields() {
            kept.push_str(if kept.is_empty() { "`" } else { ", `" });
            kept.push_str(field);
            kept.push('`');
        }
        assert!(found.contains(&format!("(the fields that reach one: {kept})")), "{found}");
        assert!(kept.contains("`function`") && kept.contains("`awaits`"), "{kept}");
    }

    /// Only a call of the crate `tracing` that names the literal target `blaze`, at a level
    /// the recorder keeps, is read.
    #[test]
    fn a_span_of_another_target_or_below_the_level_kept_is_not_read() {
        let found = lint(
            &fixture(
                r#"
const TARGET: &str = "blaze";
fn spans(level: tracing::Level, what: u32) {
    let _a = tracing::info_span!("its module's path", what);
    let _b = tracing::info_span!(target: "perf", "elsewhere", what);
    let _c = tracing::info_span!(target: "blaze::store", "a path under blaze", what);
    let _d = tracing::info_span!(target: TARGET, "a target not written", what);
    let _e = tracing::debug_span!(target: "blaze", "below the level", what);
    let _f = tracing::span!(target: "blaze", tracing::Level::DEBUG, "below the level too", what);
    let _g = tracing::span!(target: "blaze", level, "a level not written", what);
}
macro_rules! info_span { ($($t:tt)*) => { () }; }
fn local(what: u32) {
    let _ = info_span!(target: "blaze", "not tracing's", what);
}
"#,
            ),
            FLAGS,
        );
        assert!(!found.contains(NOTHING), "{found}");
    }

    /// An event is read as a span is, against its own table: each of its fields is a row of
    /// `event_fields` whatever its name, up to the fields an event keeps, and what its
    /// message formats is no field.
    #[test]
    fn an_events_field_is_held_to_event_fields_as_a_spans_is_to_spans() {
        let kept = blaze_profile::flight::EVENT_FIELDS;
        let many: Vec<String> = (0..=kept).map(|it| format!("f{it} = {it}")).collect();
        let found = lint(
            &fixture(&format!(
                r#"
fn events(what: u32, n: u32) {{
    tracing::info!(target: "blaze", what, placing = n, "an event keeps a field of any name: {{}}", n);
    tracing::info!(target: "blaze", {many}, "one field too many");
    tracing::info!(target: "perf", {many}, "elsewhere");
}}
"#,
                many = many.join(", ")
            )),
            FLAGS,
        );
        assert_eq!(found.matches(NOTHING).count(), 1, "{found}");
        assert!(
            found.contains(&format!("the field `f{kept}` of this event")),
            "the field past the {kept} an event keeps: {found}"
        );
    }

    /// The control: the lint finds a field exactly when the profile's own rule says it reaches
    /// no column, so a field the recorder learns to keep stops being found with no change here.
    #[test]
    fn the_lint_asks_the_profile_which_fields_it_keeps() {
        for field in blaze_profile::span::ad_hoc_fields() {
            assert!(blaze_profile::span::ad_hoc_field_recorded(field), "`{field}`");
        }
        assert!(!blaze_profile::span::ad_hoc_field_recorded("what"));
        assert!(blaze_profile::flight::event_field_recorded(0));
        assert!(!blaze_profile::flight::event_field_recorded(blaze_profile::flight::EVENT_FIELDS));
    }
}
