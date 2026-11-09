//! blaze_verify's native specification syntax (docs/v2/plan/design/verus.md): Verus's clauses
//! as blaze grammar, with no `verus! { }` around them.
//!
//! Ported from verus-analyzer's `crates/parser/src/grammar/verus.rs`
//! (<https://github.com/verus-lang/verus-analyzer>, MIT or Apache-2.0, commit
//! 2ac834c8158e0e0969b0e03afe2f27127d6ea9f8), the subset blaze lowers so far: `requires`,
//! `recommends`, `ensures` and `decreases` on a function, `invariant`, `ensures` and
//! `decreases` on a loop, and a named return type `-> (r: T)`.
//!
//! The parser takes the clauses in every crate: each sits where Rust has no valid program (after
//! a function's where clause, after a loop's head, `( name :` in a return type), so no program
//! the pinned nightly accepts parses differently. hir-def's body lowering refuses them in a crate
//! without `#![feature(blaze_verify)]`.

use super::*;

/// Whether the parser is at a clause's keyword, or at the end of a clause list.
fn at_clause_boundary(p: &Parser<'_>) -> bool {
    matches!(p.current(), EOF | T!['{'] | T![;] | T!['}']) || at_clause_kw(p, 0)
}

/// The clause keyword the parser is at.
fn clause_kw(p: &Parser<'_>) -> Option<SyntaxKind> {
    [T![requires], T![recommends], T![ensures], T![decreases], T![invariant]]
        .into_iter()
        .find(|&kw| p.at_contextual_kw(kw))
}

pub(super) fn at_clause_kw(p: &Parser<'_>, n: usize) -> bool {
    p.nth_at_contextual_kw(n, T![requires])
        || p.nth_at_contextual_kw(n, T![recommends])
        || p.nth_at_contextual_kw(n, T![ensures])
        || p.nth_at_contextual_kw(n, T![decreases])
        || p.nth_at_contextual_kw(n, T![invariant])
}

/// The comma-separated expressions of a clause, up to the next clause or the body.
fn expr_list(p: &mut Parser<'_>) {
    while !at_clause_boundary(p) {
        expressions::expr_no_struct(p);
        if !p.eat(T![,]) {
            break;
        }
    }
}

/// One clause: its keyword, then its expressions.
fn clause(p: &mut Parser<'_>, kw: SyntaxKind) {
    let m = p.start();
    p.bump_remap(kw);
    // Clauses are part of the signature: parsed in full in an outline too.
    p.in_full(expr_list);
    m.complete(p, SPEC_CLAUSE);
}

/// A function's clauses, between its where clause and its body.
pub(super) fn fn_clauses(p: &mut Parser<'_>) {
    for kw in [T![requires], T![recommends], T![ensures], T![decreases]] {
        if p.at_contextual_kw(kw) {
            clause(p, kw);
        }
    }
    if at_clause_kw(p, 0) {
        // A clause out of order, or twice: Verus's order is requires, recommends, ensures,
        // decreases.
        let m = p.start();
        p.error("expected the function's body: clauses come in the order `requires`, `recommends`, `ensures`, `decreases`, each once");
        while let Some(kw) = clause_kw(p) {
            clause(p, kw);
        }
        m.complete(p, ERROR);
    }
}

/// A loop's clauses, between its head and its body, in any order, as Verus takes them.
pub(super) fn loop_clauses(p: &mut Parser<'_>) {
    loop {
        let kw = if p.at_contextual_kw(T![invariant]) {
            T![invariant]
        } else if p.at_contextual_kw(T![ensures]) {
            T![ensures]
        } else if p.at_contextual_kw(T![decreases]) {
            T![decreases]
        } else {
            break;
        };
        clause(p, kw);
    }
}

/// `-> (name: Type)`: a function's return type with a name for its `ensures`.
fn at_named_ret_type(p: &Parser<'_>) -> bool {
    p.at(T!['(']) && p.nth_at(1, IDENT) && p.nth_at(2, T![:]) && !p.nth_at(2, T![::])
}

/// A function's return type, named or not. The name is looked for after `->` is bumped: the
/// parser's positions count raw tokens, and `->` is two of them (`-`, `>`).
pub(super) fn fn_ret_type(p: &mut Parser<'_>) -> bool {
    if !p.at(T![->]) {
        return false;
    }
    let m = p.start();
    p.bump(T![->]);
    if at_named_ret_type(p) {
        p.bump(T!['(']);
        patterns::pattern(p);
        p.expect(T![:]);
        types::type_no_bounds(p);
        p.expect(T![')']);
    } else {
        types::type_no_bounds(p);
    }
    m.complete(p, RET_TYPE);
    true
}
