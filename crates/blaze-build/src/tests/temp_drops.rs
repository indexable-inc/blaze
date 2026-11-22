//! A temporary's drop point, one mechanism for all (walk/temps.rs `hold_temp`): a `match`
//! scrutinee temporary inside a larger expression and a `let`-`else` initializer's temporary
//! are held in their statement's temporary scope like every temporary, a by-value binding
//! moves its part out (walk/moves.rs, the scrutinee's word), and the rest drops once.
//!
//! The Reference, "Destructors":
//! - `[destructors.scope.temporary]`: the temporary scope of an expression is "the smallest
//!   scope that contains the expression and is one of" a list naming "A statement" and "The
//!   body expression for a match arm", and "The scrutinee of a `match` expression is not a
//!   temporary scope, so temporaries in the scrutinee can be dropped after the `match`
//!   expression".
//! - `[destructors.scope.intro]`: "When control flow leaves a drop scope all variables
//!   associated to that scope are dropped in reverse order of declaration (for variables) or
//!   creation (for temporaries)."
//! - `let`-`else`: the copy on this machine (nightly-2026-10-27) states no drop point for its
//!   initializer's temporaries; rustc's own test tests/ui/let-else/let-else-temporary-lifetime.rs
//!   does: `let Foo(0) = Foo(&mut foo) else { *&mut foo = 1; todo!() };` compiles only if the
//!   temporary is dropped before the `else` block runs, and `let 0 = Droppy::default().inner
//!   else { return };` asserts it has dropped by the next statement.
//!
//! `G { x }` logs `x` when dropped; the world is tests.rs `match_world` (`enum E { A(G, G),
//! B(G), C, W(N, G) }`).

use std::vec::Vec;

use blaze_typeck::{Arm, BinOp, Expr, ExprId, List, Pat, Stmt};

use super::{BodyBuf, Global, TestWorld, g_drops, g_lit, match_world, scrut_value, variant_pat, Scrut};

/// `match <value> { <name>(<subs>) => <hit>, _ => <miss> }`.
fn match_expr(b: &mut BodyBuf, value: ExprId, pat: blaze_typeck::PatId, hit: u128, miss: u128) -> ExprId {
    let w = b.pat(Pat::Wild);
    let (one, two) = (b.int(hit, None), b.int(miss, None));
    let start = b.arms.len() as u32;
    b.arms.push(Arm { pat, guard: None, body: one });
    b.arms.push(Arm { pat: w, guard: None, body: two });
    b.e(Expr::Match { scrutinee: value, arms: List { start, len: 2 } })
}

/// `?`'s shape, twice in one statement:
/// ```text
/// fn main() {
///     let v = (match A(G{1}, G{2}) { A(a, _) => 1, _ => 2 })
///         + (match B(G{3}) { B(_) => 10, _ => 20 });
///     let k = G{9};
/// }
/// ```
/// `a` (G1) is moved out and dropped at its arm's end: 1. Both scrutinees are temporaries of
/// the `let` statement: dropped at its end, AFTER the later operand, in reverse order of
/// creation: `B(G3)` (3), then `A`'s rest (G2, never G1 again): 3, 2. Then `k`: 9.
#[test]
fn a_match_temporary_inside_an_expression_drops_at_the_statements_end() {
    let mut w = TestWorld::new();
    let (g, n, e) = match_world(&mut w);
    let mut b = BodyBuf::new();
    let first = scrut_value(&mut b, Scrut::A);
    let (a, w0) = (b.bind("a"), b.pat(Pat::Wild));
    let pa = variant_pat(&mut b, "A", &[a, w0]);
    let m1 = match_expr(&mut b, first, pa, 1, 2);
    let p1 = b.e(Expr::Paren(m1));
    let second = scrut_value(&mut b, Scrut::B);
    let w1 = b.pat(Pat::Wild);
    let pb = variant_pat(&mut b, "B", &[w1]);
    let m2 = match_expr(&mut b, second, pb, 10, 20);
    let p2 = b.e(Expr::Paren(m2));
    let sum = b.bin(BinOp::Add, p1, p2);
    let v = b.bind("v");
    let k = b.bind("k");
    let k_init = g_lit(&mut b, 9);
    let root = b.block(
        &[Stmt::Let { pat: v, ty: None, init: Some(sum), else_: None }, Stmt::Let { pat: k, ty: None, init: Some(k_init), else_: None }],
        None,
    );
    let seen = g_drops(&w, &b, root, [g, n, e]).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(seen.as_slice(), &[1, 3, 2, 9]);
}

/// `fn main() { let <pat> = A(G{1}, G{2}) else { let z = G{8}; return }; let k = G{9}; }`,
/// the order of the drops.
fn let_else_drops(matched: bool) -> Vec<i32, Global> {
    let mut w = TestWorld::new();
    let (g, n, e) = match_world(&mut w);
    let mut b = BodyBuf::new();
    let value = scrut_value(&mut b, Scrut::A);
    let pat = if matched {
        let (a, w0) = (b.bind("a"), b.pat(Pat::Wild));
        variant_pat(&mut b, "A", &[a, w0])
    } else {
        let x = b.bind("b");
        variant_pat(&mut b, "B", &[x])
    };
    let z = b.bind("z");
    let z_init = g_lit(&mut b, 8);
    let ret = b.e(Expr::Return { value: None });
    let els = b.block(&[Stmt::Let { pat: z, ty: None, init: Some(z_init), else_: None }], Some(ret));
    let k = b.bind("k");
    let k_init = g_lit(&mut b, 9);
    let root = b.block(
        &[Stmt::Let { pat, ty: None, init: Some(value), else_: Some(els) }, Stmt::Let { pat: k, ty: None, init: Some(k_init), else_: None }],
        None,
    );
    g_drops(&w, &b, root, [g, n, e]).unwrap_or_else(|e| panic!("{e}"))
}

/// Matched, `let A(a, _) = A(G{1}, G{2}) else { .. };`: the temporary's rest (G2) drops at
/// the statement's end: 2; at the block's end `k` then `a`, reverse order of declaration: 9, 1.
/// Not matched, `let B(b) = A(G{1}, G{2}) else { let z = G{8}; return };`: the whole temporary
/// drops BEFORE the `else` block runs (1, 2: its fields in declaration order), then the
/// `return` drops `z`: 8; nothing twice, and never the binding `b`, which was never made.
#[test]
fn a_let_else_temporary_drops_before_the_else_block_or_at_the_statements_end() {
    let pass = let_else_drops(true);
    let fail = let_else_drops(false);
    #[cfg(target_arch = "aarch64")]
    {
        assert_eq!(pass.as_slice(), &[2, 9, 1], "matched");
        assert_eq!(fail.as_slice(), &[1, 2, 8], "not matched");
    }
    let _ = (pass, fail);
}
