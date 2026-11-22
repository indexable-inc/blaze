//! A callee whose kind the world could not read (`FnKind::Missing(fact)`) is refused as
//! `Refusal::FnFact` naming the callee and the fact, never as `NotConstFn`; and that refusal is
//! PENDING (machine/late.rs `Wait::FnFact`): the code holding it is compiled again once the
//! world answers, while a callee whose head says it is not const stays `NotConstFn`.
//!
//! The Reference, "Constant evaluation", "Const functions": a call in a constant context must
//! be of a `const fn` (or a tuple constructor); whether `unread` is one is a fact the world has
//! not read, so neither "it is not const" nor any value can be said until it does.
//!
//! The fixture keeps `Codes` across evaluations (as the driver's `Consts` does), so `g`'s code
//! compiled by the first root is the code the second root finds. PLANTED BUG this catches: a
//! `Wait::of` that answers `None` for `FnFact` (kept as final) makes `M2` refuse again.

use super::{eval_int, refusal, world};
use crate::{Fact, FnKind, Refusal};

/// Prints `test tests::fn_fact::a_missing_fact_is_a_pending_typed_refusal ... ok`.
#[test]
fn a_missing_fact_is_a_pending_typed_refusal() {
    let mut w = world();
    w.unread_fn("unread", &[], "u8", "{ 1 }", Fact::Signature);
    w.const_fn("g", &[], "u8", "{ unread() }");
    w.konst("M1", "u8", "g()");
    w.konst("M2", "u8", "g()");
    w.build();
    let item = w.key("unread");
    assert_eq!(
        refusal(&mut w, "M1"),
        std::format!("{:?}", Refusal::FnFact { item, fact: Fact::Signature }),
        "the gap names the fact and the callee, never `NotConstFn`"
    );
    // The world reads the fact: `unread` is a `const fn`.
    w.set_kind("unread", FnKind::Const);
    assert_eq!(eval_int(&mut w, "M2"), Ok(1), "`g`'s code waited for the fact: compiled again, it calls `unread`");
}

/// Control, the old form: a callee whose head was read and has no `const` is refused as not
/// const, in every root (a final refusal, kept).
/// Prints `test tests::fn_fact::a_read_head_without_const_is_not_const ... ok`.
#[test]
fn a_read_head_without_const_is_not_const() {
    let mut w = world();
    w.plain_fn("not_const", &[], "u8", "{ 1 }");
    w.const_fn("h", &[], "u8", "{ not_const() }");
    w.konst("N1", "u8", "h()");
    w.konst("N2", "u8", "h()");
    w.build();
    let item = w.key("not_const");
    let want = std::format!("{:?}", Refusal::NotConstFn { item });
    assert_eq!(refusal(&mut w, "N1"), want, "the first root");
    assert_eq!(refusal(&mut w, "N2"), want, "the second root, from the kept code");
}
