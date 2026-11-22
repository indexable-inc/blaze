//! The oracle of the `macro_rules!` engine: two expansions of one call compared token for token
//! (kind, spacing, text, origin, the context's marks), with their nested calls, their
//! diagnostics' words and the arm that matched; and the rendering of one, which the recorded
//! baselines hold. The engine was switched from mbe's with no difference on these; the
//! baselines are mbe's output, so the engine is still compared with the one it replaced.
//! Shared by `tests/rules_diff.rs` and the `rules_core` example.

use std::fmt;

use blaze_expand::{
    Ctx, Diagnostic, ExpandError, Expanded, Expansion, Mark, Text, TokenStream,
};

/// The first place two expansions differ: `then` the expected, `now` the one checked.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Difference {
    /// One expanded and the other stopped, or they stopped with different errors.
    Outcome { then: String, now: String },
    Len { then: usize, now: usize },
    /// Token `at` differs in its kind (spacing included) or text.
    Token { at: usize, then: String, now: String },
    Origin { at: usize, then: String, now: String },
    /// Token `at`'s context has other marks.
    Context { at: usize },
    Nested,
    Diagnostics { then: String, now: String },
    Arm { then: Option<u32>, now: Option<u32> },
}

impl fmt::Display for Difference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Difference::Outcome { then, now } => write!(f, "outcome: then {then}; now {now}"),
            Difference::Len { then, now } => write!(f, "length: then {then}, now {now}"),
            Difference::Token { at, then, now } => write!(f, "token {at}: then {then}; now {now}"),
            Difference::Origin { at, then, now } => {
                write!(f, "origin of token {at}: then {then}; now {now}")
            }
            Difference::Context { at } => write!(f, "the context of token {at}"),
            Difference::Nested => f.write_str("the nested calls"),
            Difference::Diagnostics { then, now } => {
                write!(f, "diagnostics: then [{then}]; now [{now}]")
            }
            Difference::Arm { then, now } => write!(f, "arm: then {then:?}, now {now:?}"),
        }
    }
}

/// The marks of `ctx`, innermost first, and its root's edition.
fn chain(stream: &TokenStream<'_>, mut ctx: Ctx) -> (Vec<Mark>, Option<blaze_expand::Edition>) {
    let mut marks = Vec::new();
    while let Some(node) = stream.node(ctx) {
        marks.push(node.mark);
        ctx = node.parent;
    }
    (marks, ctx.root_edition())
}

fn text(exp: &Expansion<'_>, text: Text) -> String {
    exp.note_text(text).to_owned()
}

/// A diagnostic in words, its notes read.
pub fn diagnostic(exp: &Expansion<'_>, d: &Diagnostic) -> String {
    match *d {
        Diagnostic::NoRuleMatched { rules, token, expected, at, .. } => format!(
            "no rule of {rules} matched at {:?}: found {:?}, expected {:?}",
            at,
            token.map(|it| text(exp, it)),
            text(exp, expected)
        ),
        Diagnostic::Binding { message, at } => format!("binding at {at:?}: {}", text(exp, message)),
        Diagnostic::BadDefinition { message } => format!("bad definition: {}", text(exp, message)),
        other => format!("{other:?}"),
    }
}

fn outcome(result: &Result<Expanded<'_>, ExpandError>) -> String {
    match result {
        Ok(Expanded::Expansion(exp)) => format!("an expansion of {} tokens", exp.tokens.len()),
        Ok(Expanded::NeedsExpanded(calls)) => format!("needs {} calls first", calls.len()),
        Err(error) => format!("error: {error}"),
    }
}

/// The first difference of `now` from `then`, if any.
pub fn compare(
    then: &Result<Expanded<'_>, ExpandError>,
    now: &Result<Expanded<'_>, ExpandError>,
) -> Option<Difference> {
    let (a, b) = match (then, now) {
        (Ok(Expanded::Expansion(a)), Ok(Expanded::Expansion(b))) => (a, b),
        (Err(a), Err(b)) if a == b => return None,
        _ => return Some(Difference::Outcome { then: outcome(then), now: outcome(now) }),
    };
    let words = |exp: &Expansion<'_>| {
        exp.diagnostics.iter().map(|d| diagnostic(exp, d)).collect::<Vec<_>>().join("; ")
    };
    let (da, db) = (words(a), words(b));
    if da != db {
        return Some(Difference::Diagnostics { then: da, now: db });
    }
    if a.tokens.len() != b.tokens.len() {
        return Some(Difference::Len { then: a.tokens.len(), now: b.tokens.len() });
    }
    for at in 0..a.tokens.len() {
        let (ta, tb) = (a.tokens.tokens()[at], b.tokens.tokens()[at]);
        let (sa, sb) = (a.tokens.text_at(at), b.tokens.text_at(at));
        if ta.kind != tb.kind || sa != sb {
            return Some(Difference::Token {
                at,
                then: format!("{:?} `{sa}`", ta.kind),
                now: format!("{:?} `{sb}`", tb.kind),
            });
        }
        if a.origins[at] != b.origins[at] {
            return Some(Difference::Origin {
                at,
                then: format!("{:?}", a.origins[at]),
                now: format!("{:?}", b.origins[at]),
            });
        }
        if chain(&a.tokens, ta.ctx) != chain(&b.tokens, tb.ctx) {
            return Some(Difference::Context { at });
        }
    }
    if a.nested[..] != b.nested[..] {
        return Some(Difference::Nested);
    }
    if a.arm != b.arm {
        return Some(Difference::Arm { then: a.arm, now: b.arm });
    }
    None
}

/// Everything [`compare`] looks at, and the contexts' numbering (so equal renderings are equal
/// records in bytes), as text: what a recorded baseline holds.
pub fn render(result: &Result<Expanded<'_>, ExpandError>) -> String {
    use std::fmt::Write as _;
    let exp = match result {
        Ok(Expanded::Expansion(exp)) => exp,
        other => return format!("{}\n", outcome(other)),
    };
    let mut out = String::new();
    for d in exp.diagnostics.iter() {
        let _ = writeln!(out, "! {}", diagnostic(exp, d));
    }
    for at in 0..exp.tokens.len() {
        let token = exp.tokens.tokens()[at];
        let _ = writeln!(
            out,
            "{:?} `{}` {:?} {:?} ctx{}",
            token.kind,
            exp.tokens.text_at(at),
            exp.origins[at],
            chain(&exp.tokens, token.ctx),
            token.ctx.raw()
        );
    }
    let _ = writeln!(out, "contexts {:?}", exp.tokens.contexts());
    for nested in exp.nested.iter() {
        let _ = writeln!(out, "nested {nested:?}");
    }
    let _ = writeln!(out, "arm {:?}", exp.arm);
    out
}

/// FNV-1a over `bytes` from `hash`: a baseline's digest, the same on every machine.
pub fn fnv(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// FNV-1a's starting value.
pub const FNV_START: u64 = 0xcbf2_9ce4_8422_2325;
