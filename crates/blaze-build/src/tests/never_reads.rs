//! A value of type `!` read in live code and coerced (walk.rs `adjust`, `NeverToAny`): the
//! block ends `unreachable` there.
//!
//! Found on the hello-world path (walk list-AJ, P15 (a)): core's
//! ```text
//! impl Debug for ! {
//!     fn fmt(&self, _: &mut Formatter<'_>) -> Result {
//!         *self
//!     }
//! }
//! ```
//! (library/core/src/fmt/mod.rs; `Display for !` the same, and `Infallible` is `!`:
//! library/core/src/convert/mod.rs `pub type Infallible = !;`) was refused with "an expression
//! with no value where its type has one": `*self` reads a `!` place, which has no value
//! (`Repr::None`), while nothing had ended the block, so the coercion to `Result` asked a
//! scalar of a live `V::None`.

use super::*;

/// `fn fmt(p: &!) -> i32 { *p }`, the shape of core's `impl Debug for !`. The rule, from the
/// pinned library's own source (library/core/src/primitive_docs.rs, the `!` type): "`!`
/// represents the type of diverging computations -- computations which never resolve to any
/// value." and "since `!` has no values (since it is uninhabited), it is a marker for
/// unreachable code." So control never passes the read of `*p`: what follows it, the coercion
/// to `i32` and the return, is unreachable code, and the built function ends its live block in
/// `unreachable`.
///
/// Expected: the build succeeds, verifies, is emitted by the baseline tier, and its text holds
/// `unreachable`. Prints nothing; `test tests::never_reads::a_read_of_never_coerced_ends_the_block ... ok`.
/// PLANTED BUG this catches: without the `NeverToAny` arm ending a live block, the build fails
/// with "an expression with no value where its type has one" (the `expect` below panics with it).
#[test]
fn a_read_of_never_coerced_ends_the_block() {
    let mut w = TestWorld::new();
    let r_never = w.ref_(Mutbl::Not, PNode::Never);
    w.func(1, "fmt", &[r_never], I32);
    let mut b = BodyBuf::new();
    let p = b.bind("p");
    b.params.push(p);
    let p_e = b.local(p);
    let star = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: p_e });
    let root = b.block(&[], Some(star));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("`*p` of `p: &!` as an `i32`: {e}"));
    let text = blaze_lir::print(built.func.view());
    assert!(text.contains("unreachable"), "the read of a `!` place ends its block:\n{text}");
}
