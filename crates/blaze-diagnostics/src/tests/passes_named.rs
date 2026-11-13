//! The crate's passes are named: each pass a check runs over the whole crate opens one span
//! `crate pass` with its name (`function`) and the crate's (`krate`), so a build's profile says
//! which pass holds the time a body edit spends outside its bodies.

use std::sync::atomic::{AtomicU64, Ordering};

use blaze_core::lock::Lock;

use super::checked;

/// The `function` of each `crate pass` span a check opens, in the order it opened them.
struct Seen {
    next: AtomicU64,
    passes: Lock<Vec<String>>,
}

impl Default for Seen {
    fn default() -> Seen {
        Seen {
            next: AtomicU64::new(0),
            passes: Lock::new("test.diagnostics.passes_named.passes", Vec::new()),
        }
    }
}

/// The value of the field `function` of one span.
struct Function(Option<String>);

impl tracing::field::Visit for Function {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "function" {
            self.0 = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "function" {
            self.0 = Some(format!("{value:?}"));
        }
    }
}

impl tracing::Subscriber for &'static Seen {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        if span.metadata().name() == "crate pass" {
            let mut function = Function(None);
            span.record(&mut function);
            let function = function.0.expect("a `crate pass` span names its pass in `function`");
            self.passes.lock().push(function);
        }
        tracing::span::Id::from_u64(id)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// Every pass a check of a crate with nothing to say runs, in the order it opens them: the
/// parse, `checks::check`'s statement groups, the early lints, the late ones, the levels.
const PASSES: [&str; 28] = [
    "parsed",
    "checks",
    "owners and features",
    "macros",
    "feature expansion",
    "ast validation",
    "defs",
    "names",
    "feature gates",
    "attributes",
    "crate",
    "checked attributes",
    "unused features",
    "impls",
    "lints early",
    "syntax lints",
    "imports",
    "stability",
    "items before",
    "bodies before",
    "lints late",
    "items and visibilities",
    "private in public",
    "dead code",
    "items late",
    "clashing externs",
    "bodies late",
    "levels",
];

/// The planted bug of the control: what a pass with no span would leave in the record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    /// The `names` pass runs under no span.
    Unnamed,
}

/// The passes a check of `fixture`'s one crate opened a `crate pass` span for, in order, with
/// `plant` applied to the record.
fn opened(fixture: &str, plant: Plant) -> Vec<String> {
    let seen: &'static Seen = Box::leak(Box::default());
    tracing::subscriber::with_default(seen, || {
        checked(fixture);
    });
    // clone: the record outlives the lock's guard
    let mut passes = seen.passes.lock().clone();
    if plant == Plant::Unnamed {
        passes.retain(|it| it != "names");
    }
    passes
}

/// The oracle: `opened` is [`PASSES`], each once and in order; else the first pass that ran
/// under no span of its own, or the span no pass of the list opens.
fn named(opened: &[String]) -> Result<(), String> {
    for (at, pass) in PASSES.iter().enumerate() {
        match opened.get(at) {
            Some(it) if it == pass => {}
            Some(it) => {
                return Err(format!(
                    "pass {at} is `{pass}`, but the span opened there is `{it}`: `{pass}` ran \
                     under no span of its own, or out of order (opened: {opened:?})"
                ));
            }
            None => return Err(format!("`{pass}` ran under no span (opened: {opened:?})")),
        }
    }
    match opened.get(PASSES.len()..).unwrap_or_default() {
        [] => Ok(()),
        more => Err(format!("spans no pass of the list opens, or a pass opened twice: {more:?}")),
    }
}

const ONE_FUNCTION: &str = r#"
//- /lib.rs crate:one
pub fn one() -> u32 { 1 }
"#;

/// The invariant: a check of a crate with one function opens a `crate pass` span for each pass
/// of [`PASSES`] exactly once, in that order, and none other; so no pass over the whole crate
/// spends its time under no span of its own.
/// Control: `Plant::Unnamed` (the `names` pass run under no span) fails, naming `names`.
#[test]
fn every_crate_pass_runs_under_its_span() {
    let opened_now = opened(ONE_FUNCTION, Plant::None);
    if let Err(why) = named(&opened_now) {
        panic!("{why}");
    }
    let planted = opened(ONE_FUNCTION, Plant::Unnamed);
    let caught = named(&planted).expect_err("the oracle catches a pass run under no span");
    assert!(caught.contains("`names`"), "the oracle names the unnamed pass: {caught}");
}
