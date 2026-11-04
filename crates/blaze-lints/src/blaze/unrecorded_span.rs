//! `unrecorded_span`: a `tracing` span at a level the flight recorder keeps whose target it
//! drops ([`blaze_profile::keeps`]), or an INFO event it drops
//! ([`blaze_profile::logs::keeps_event`]: every WARN and ERROR event is kept, of any target).
//! The recorder is a blaze process's only subscriber, so such a span or event costs its call
//! and is never seen: no profile, flight dump, `events` row or watchdog line has it. A target
//! is its module's path unless the call names one, so what this finds is a call outside a
//! `blaze*` crate (a rust-analyzer crate's) that names no target and, for a span, whose name is
//! no query's (`*_query`). The fix names `target: "blaze"` (`target = "blaze"` in
//! `#[instrument]`).
//!
//! Read from the call as written: the span macros of the crate `tracing` (`info_span!`,
//! `span!(Level::INFO, ..)`, ..), its event macros (`warn!`, `event!(Level::INFO, ..)`, ..) and
//! `#[instrument]`, whose path is all there is to go by. A call below the recorder's level
//! (`debug_span!`, `debug!`, `level = "debug"`) is that level's choice, not this lint's; one
//! whose target, level or name is not a literal is left alone, as is one a macro writes.

use syntax::{
    SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _, HasName as _},
};

use super::Lint;
use crate::{check::Cx, diag::Applicability};

const LINT: Lint = Lint::UnrecordedSpan;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    if let Some(call) = ast::MacroCall::cast(node) {
        check_call(cx, node, call);
    } else if let Some(attr) = ast::Attr::cast(node) {
        check_attr(cx, node, attr);
    }
}

/// What a macro of `tracing` writes: a span or an event.
#[derive(Clone, Copy)]
enum Made {
    Span,
    Event,
}

fn check_call<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>, call: ast::MacroCall<'t>) {
    let Some(called) = call.path().and_then(|it| it.segment()).and_then(|it| it.name_ref()) else {
        return;
    };
    let (made, level) = match called.text() {
        "error_span" => (Made::Span, Some("error")),
        "warn_span" => (Made::Span, Some("warn")),
        "info_span" => (Made::Span, Some("info")),
        "debug_span" => (Made::Span, Some("debug")),
        "trace_span" => (Made::Span, Some("trace")),
        "span" => (Made::Span, None),
        "error" => (Made::Event, Some("error")),
        "warn" => (Made::Event, Some("warn")),
        "info" => (Made::Event, Some("info")),
        "debug" => (Made::Event, Some("debug")),
        "trace" => (Made::Event, Some("trace")),
        "event" => (Made::Event, None),
        _ => return,
    };
    let db = cx.db();
    if cx
        .macro_crate(call)
        .and_then(|it| it.display_name(db))
        .is_none_or(|it| it.crate_name().as_str() != "tracing")
    {
        return;
    }
    let Some(display) = cx.krate().display_name(db) else { return };
    let Some(tt) = call.token_tree() else { return };
    let mut args = [Arg::NONE; 4];
    let mut count = 0;
    each_arg(tt, |arg| {
        if let Some(slot) = args.get_mut(count) {
            *slot = *arg;
        }
        count += 1;
    });
    let (args, krate) = (&args[..count.min(args.len())], display.crate_name().as_str());
    let dropped = match made {
        Made::Span => dropped_call(level, args, krate),
        Made::Event => dropped_event(level, args, krate),
    };
    let Some(dropped) = dropped else { return };
    let open = tt.syntax().text_range().start();
    report(
        cx,
        node,
        display.crate_name().as_str(),
        dropped,
        Fix::Open { at: open, key: "target: \"blaze\", " },
    );
}

fn check_attr<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>, attr: ast::Attr<'t>) {
    let (path, tt) = match attr.meta() {
        Some(ast::Meta::TokenTreeMeta(meta)) => (meta.path(), meta.token_tree()),
        Some(ast::Meta::PathMeta(meta)) => (meta.path(), None),
        _ => return,
    };
    let Some(path) = path else { return };
    if !matches!(path.syntax().text(), "instrument" | "tracing::instrument") {
        return;
    }
    let Some(f) = attr.syntax().parent().and_then(ast::Fn::cast) else { return };
    let Some(fn_name) = f.name() else { return };
    let Some(display) = cx.krate().display_name(cx.db()) else { return };
    let krate = display.crate_name().as_str();
    let (mut target, mut level, mut name) = (None, true, fn_name.text());
    if let Some(tt) = tt {
        let mut literal = true;
        each_arg(tt, |arg| match arg.key(SyntaxKind::EQ) {
            Some("target") => match arg.value() {
                Some(it) => target = Some(it),
                None => literal = false,
            },
            Some("level") => {
                level = arg.last.is_some_and(|it| blaze_profile::keeps_level(unquoted(it.text())))
            }
            Some("name") => match arg.value() {
                Some(it) => name = unquoted(it.text()),
                None => literal = false,
            },
            _ => {}
        });
        if !literal {
            return;
        }
    }
    if !level
        || blaze_profile::keeps(
            target.map_or(krate, |it: ast::Token<'t>| unquoted(it.text())),
            name,
        )
    {
        return;
    }
    let fix = match tt {
        Some(tt) => Fix::Open { at: tt.syntax().text_range().start(), key: "target = \"blaze\", " },
        None => Fix::Path { path: path.syntax().text_range() },
    };
    report(cx, node, krate, Dropped { target, what: "span" }, fix);
}

/// A span or event the recorder drops, as written: the target it names, none when it leaves
/// the target to its module's path.
#[derive(Clone, Copy)]
struct Dropped<'t> {
    target: Option<ast::Token<'t>>,
    /// `span` or `event`, as the message says it.
    what: &'static str,
}

/// Where the fix names the target, in the tree's offsets: after the list's open bracket at
/// `at`, or as a list after an attribute's bare path.
enum Fix {
    Open { at: TextSize, key: &'static str },
    Path { path: TextRange },
}

/// What a span macro call writes, its arguments `args` (the first few) and `level` the level
/// its macro names (none for `span!`, which takes it first): the span when the recorder drops
/// it in the crate `krate`. None for one it keeps, or one whose target, level or name is not
/// a literal.
fn dropped_call<'t>(level: Option<&str>, args: &[Arg<'t>], krate: &str) -> Option<Dropped<'t>> {
    let mut kept_level = level.map(blaze_profile::keeps_level);
    let mut target = None;
    for arg in args {
        match arg.key(SyntaxKind::COLON) {
            Some("target") => target = Some(arg.value()?),
            Some("parent") => {}
            _ => match kept_level {
                None => {
                    kept_level = Some(
                        arg.last.is_some_and(|it| blaze_profile::keeps_level(unquoted(it.text()))),
                    )
                }
                Some(level) => {
                    let name = unquoted(arg.string()?.text());
                    let target_text =
                        target.map_or(krate, |it: ast::Token<'t>| unquoted(it.text()));
                    let kept = !level || blaze_profile::keeps(target_text, name);
                    return (!kept).then_some(Dropped { target, what: "span" });
                }
            },
        }
    }
    None
}

/// What an event macro call writes, as [`dropped_call`] for a span: its `target:`, `parent:`
/// and `name:` come first, then (for `event!`) its level, then its fields and message. An
/// event's name is `event <file>:<line>` unless it names one, which is no query's.
fn dropped_event<'t>(level: Option<&str>, args: &[Arg<'t>], krate: &str) -> Option<Dropped<'t>> {
    // The level as written: the macro's, or `event!`'s first argument's last token (`INFO` of
    // `tracing::Level::INFO`); `Some(None)` for one with no last token.
    let mut written: Option<Option<&str>> = level.map(Some);
    let (mut target, mut name) = (None, "event");
    for arg in args {
        match arg.key(SyntaxKind::COLON) {
            Some("target") => target = Some(arg.value()?),
            Some("name") => name = unquoted(arg.value()?.text()),
            Some("parent") => {}
            _ if written.is_none() => written = Some(arg.last.map(|it| unquoted(it.text()))),
            _ => break,
        }
    }
    let level = written??;
    let target_text = target.map_or(krate, |it: ast::Token<'t>| unquoted(it.text()));
    // Below the recorder's level (DEBUG, or a level not written) is that level's choice.
    let kept = !blaze_profile::keeps_level(level)
        || blaze_profile::logs::keeps_event_named(target_text, name, level);
    (!kept).then_some(Dropped { target, what: "event" })
}

fn report<'t>(
    cx: &mut Cx<'_, 't>,
    node: ast::Node<'t>,
    krate: &str,
    dropped: Dropped<'t>,
    fix: Fix,
) {
    // The tree's offsets are the file's only where the node is written as it is in the tree.
    let Some(range) = cx.range(node) else { return };
    let written = node.text_range();
    if range.len() != written.len() {
        return;
    }
    let in_file = |at: TextSize| range.start() + (at - written.start());
    let what = dropped.what;
    let message = match dropped.target {
        None => format!(
            "the flight recorder drops this {what}: its target is its module's path, in `{krate}`, which does not start with `{}`",
            blaze_profile::TARGET
        ),
        Some(target) => format!(
            "the flight recorder drops this {what}: its target {} does not start with `{}`",
            target.text(),
            blaze_profile::TARGET
        ),
    };
    let (at, code, applicability) = match (dropped.target, fix) {
        // Someone chose that target, so naming another is theirs to confirm.
        (Some(target), _) => (
            TextRange::at(in_file(target.text_range().start()), target.text_range().len()),
            format!("\"{}\"", blaze_profile::TARGET),
            Applicability::MaybeIncorrect,
        ),
        (None, Fix::Open { at, key }) => {
            let open = TextRange::at(in_file(at), TextSize::from(1));
            (open, format!("{}{key}", cx.snippet(open)), Applicability::MachineApplicable)
        }
        (None, Fix::Path { path }) => {
            let path = TextRange::at(in_file(path.start()), path.len());
            (
                path,
                format!("{}(target = \"blaze\")", cx.snippet(path)),
                Applicability::MachineApplicable,
            )
        }
    };
    let span = cx.span(range);
    let at = cx.span(at);
    cx.blaze(LINT, node, span, message)
        .suggestion_verbose(at, "name the target the flight recorder keeps", code, applicability)
        .help(LINT.help())
        .help(format!("for further information visit {}", LINT.docs_url()));
}

/// One argument of a span macro or of `#[instrument]`, split at the list's own commas: its
/// first three tokens, its last, and how many it has, a group in brackets counted as one with
/// no token.
#[derive(Clone, Copy)]
struct Arg<'t> {
    first: [Option<ast::Token<'t>>; 3],
    last: Option<ast::Token<'t>>,
    len: usize,
}

impl<'t> Arg<'t> {
    const NONE: Self = Arg { first: [None; 3], last: None, len: 0 };

    fn push(&mut self, token: Option<ast::Token<'t>>) {
        if let Some(slot) = self.first.get_mut(self.len) {
            *slot = token;
        }
        self.last = token;
        self.len += 1;
    }

    /// The key of `key <separator> ..`: `target: ".."` in a macro, `level = ".."` in an
    /// attribute. A path (`Level::INFO`) has none.
    fn key(&self, separator: SyntaxKind) -> Option<&'t str> {
        match self.first {
            [Some(key), Some(sep), third]
                if key.kind() == SyntaxKind::IDENT
                    && sep.kind() == separator
                    && third.is_none_or(|it| it.kind() != SyntaxKind::COLON) =>
            {
                Some(key.text())
            }
            _ => None,
        }
    }

    /// The string literal of `key <separator> ".."`.
    fn value(&self) -> Option<ast::Token<'t>> {
        let [_, _, Some(value)] = self.first else { return None };
        (self.len == 3 && value.kind() == SyntaxKind::STRING).then_some(value)
    }

    /// The argument when it is a string literal alone: a span's name.
    fn string(&self) -> Option<ast::Token<'t>> {
        let [Some(it), None, None] = self.first else { return None };
        (self.len == 1 && it.kind() == SyntaxKind::STRING).then_some(it)
    }
}

/// Calls `each` with each argument of the list `tt`, in order.
fn each_arg<'t>(tt: ast::TokenTree<'t>, mut each: impl FnMut(&Arg<'t>)) {
    let mut arg = Arg::NONE;
    for child in tt.syntax().children_with_tokens() {
        let token = match child.into_token() {
            None => None,
            Some(token) => match token.kind() {
                SyntaxKind::L_PAREN
                | SyntaxKind::R_PAREN
                | SyntaxKind::L_BRACK
                | SyntaxKind::R_BRACK
                | SyntaxKind::L_CURLY
                | SyntaxKind::R_CURLY => continue,
                SyntaxKind::COMMA => {
                    if arg.len > 0 {
                        each(&arg);
                    }
                    arg = Arg::NONE;
                    continue;
                }
                kind if kind.is_trivia() => continue,
                _ => Some(token),
            },
        };
        arg.push(token);
    }
    if arg.len > 0 {
        each(&arg);
    }
}

/// A string literal's text without its quotes; anything else as it is.
fn unquoted(text: &str) -> &str {
    text.strip_prefix('"').and_then(|it| it.strip_suffix('"')).unwrap_or(text)
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
macro_rules! warn { ($($t:tt)*) => { $crate::event!($($t)*) }; }
#[macro_export]
macro_rules! info { ($($t:tt)*) => { $crate::event!($($t)*) }; }
#[macro_export]
macro_rules! debug { ($($t:tt)*) => { $crate::event!($($t)*) }; }
"#;

    const FLAGS: &str = "-A clippy::all -W blaze::unrecorded_span";

    const DROPS: &str = "the flight recorder drops this span";

    fn fixture(main: &str) -> String {
        format!("//- minicore: sized\n//- /src/lib.rs crate:main deps:tracing\n{main}\n{TRACING}")
    }

    /// The crate `main` is no `blaze*` crate, so a span there that names no target is dropped
    /// unless it is a query's; the fix names `target: "blaze"`.
    #[test]
    fn a_span_macro_the_recorder_drops_is_found_and_given_the_blaze_target() {
        let found = lint(
            &fixture(
                r#"
fn spans(level: tracing::Level) {
    let _a = tracing::info_span!("macro_expand");
    let _b = tracing::info_span!(target: "blaze", "kept by its target");
    let _c = tracing::info_span!("infer_query");
    let _d = tracing::debug_span!("below the level");
    let _e = tracing::span!(tracing::Level::INFO, "spanned");
    let _f = tracing::span!(level, "a level not written");
    let _g = tracing::info_span!(target: "perf", "elsewhere");
    let _h = tracing::info_span!(parent: (), "orphan");
}
macro_rules! info_span { ($($t:tt)*) => { () }; }
fn local() {
    let _ = info_span!("not tracing's");
}
"#,
            ),
            FLAGS,
        );
        assert_eq!(found.matches(DROPS).count(), 4, "{found}");
        assert!(found.contains("info_span!(target: \"blaze\", \"macro_expand\")"), "{found}");
        assert!(
            found.contains("span!(target: \"blaze\", tracing::Level::INFO, \"spanned\")"),
            "{found}"
        );
        assert!(found.contains("info_span!(target: \"blaze\", \"elsewhere\")"), "{found}");
        assert!(found.contains("its target \"perf\" does not start with `blaze`"), "{found}");
        assert!(found.contains("info_span!(target: \"blaze\", parent: (), \"orphan\")"), "{found}");
        assert!(found.contains("its module's path, in `main`"), "{found}");
        for kept in [
            "kept by its target",
            "infer_query",
            "below the level",
            "a level not written",
            "not tracing's",
        ] {
            assert!(!found.contains(kept), "{kept}: {found}");
        }
    }

    /// An INFO event of `main` that names no target is dropped, whatever its message, unless it
    /// names itself as a query's, as the recorder's rule keeps; a WARN or ERROR one is kept,
    /// of any target. The fix is a span's.
    #[test]
    fn an_event_macro_the_recorder_drops_is_found_and_given_the_blaze_target() {
        let found = lint(
            &fixture(
                r#"
fn events(level: tracing::Level, n: u32) {
    tracing::warn!("no unit's step ended for {n} s");
    tracing::info!(n, "fields first");
    tracing::warn!(target: "blaze", "kept by its target");
    tracing::debug!("below the level");
    tracing::event!(tracing::Level::INFO, "evented");
    tracing::event!(level, "a level not written");
    tracing::info!(target: "perf", "elsewhere");
    tracing::error!(target: "perf", "an error of any target");
    tracing::info!(name: "named_query", "a query's name");
    tracing::info!(parent: None, "orphan");
}
macro_rules! warn { ($($t:tt)*) => { () }; }
fn local() {
    warn!("not tracing's");
}
"#,
            ),
            FLAGS,
        );
        const DROPS_EVENT: &str = "the flight recorder drops this event";
        assert_eq!(found.matches(DROPS_EVENT).count(), 4, "{found}");
        assert!(!found.contains(DROPS), "{found}");
        assert!(found.contains("info!(target: \"blaze\", n, \"fields first\")"), "{found}");
        assert!(
            found.contains("event!(target: \"blaze\", tracing::Level::INFO, \"evented\")"),
            "{found}"
        );
        assert!(found.contains("its target \"perf\" does not start with `blaze`"), "{found}");
        assert!(found.contains("info!(target: \"blaze\", parent: None, \"orphan\")"), "{found}");
        for kept in [
            "no unit's step ended",
            "kept by its target",
            "below the level",
            "a level not written",
            "a query's name",
            "an error of any target",
            "not tracing's",
        ] {
            assert!(!found.contains(kept), "{kept}: {found}");
        }
    }

    #[test]
    fn an_instrument_the_recorder_drops_is_found_and_given_the_blaze_target() {
        let found = lint(
            &fixture(
                r#"
#[tracing::instrument(skip(x), ret)]
fn instrumented(x: u32) {}
#[tracing::instrument]
fn bare() {}
#[tracing::instrument(level = "debug", skip_all)]
fn quiet() {}
#[tracing::instrument(target = "blaze")]
fn targeted() {}
#[tracing::instrument(name = "bare_query")]
fn queried() {}
"#,
            ),
            FLAGS,
        );
        assert_eq!(found.matches(DROPS).count(), 2, "{found}");
        assert!(
            found.contains("#[tracing::instrument(target = \"blaze\", skip(x), ret)]"),
            "{found}"
        );
        // `targeted` writes this too, and is not found: only the fix of `bare` shows it.
        assert!(found.contains("#[tracing::instrument(target = \"blaze\")]"), "{found}");
        for kept in ["level = \"debug\"", "bare_query"] {
            assert!(!found.contains(kept), "{kept}: {found}");
        }
    }

    /// The lint asks the recorder's own rule, with the crate's name for a span that names no
    /// target: its module's path starts with the crate's name.
    #[test]
    fn a_blaze_crate_keeps_a_span_with_no_target_and_another_crate_does_not() {
        assert!(blaze_profile::keeps("blaze_cargo", "unit record lookup"));
        assert!(!blaze_profile::keeps("hir_def", "macro_expand"));
        assert!(blaze_profile::keeps("hir_def", "infer_query"));
        assert!(blaze_profile::keeps_level("INFO") && blaze_profile::keeps_level("warn"));
        assert!(!blaze_profile::keeps_level("debug") && !blaze_profile::keeps_level("level"));
    }
}
