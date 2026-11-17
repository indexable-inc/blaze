//! refused: blaze_sim::AnyIn<&Frame>
// A test's parameter after `cx` is a capability (`bstd::grant::Grant`) or a value drawn from
// the tape (`blaze_sim::AnyIn<&Frame>` and `Debug`) (D109). A type that is neither is refused
// as the crate compiles by `bstd::test::Param`'s message, which names both traits (with the
// feature `sim-draws` on or off: the substring above is in both messages).
#![feature(macro_attr)]
use bstd::Cx;

struct Ledger;

#[bstd::test]
fn keeps_what_it_committed(_cx: &Cx, ledger: Ledger) {
    let _ = ledger;
}
