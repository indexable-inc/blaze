//! `if let`, `while let` and `let` chains on a scrutinee whose type needs drop
//! (walk/lets.rs, walk/lets/scope.rs): a PLACE scrutinee is tested where it is; a VALUE
//! scrutinee's temporary is held by the one mechanism every temporary has (walk/temps.rs
//! `hold_temp`, through walk/moves.rs `own_scrutinee`), a by-value binding moves its part out
//! (the scrutinee's word), and the rest drops at the rule's point.
//!
//! The Reference, "Destructors":
//! - "Temporary scopes": the temporary scope of an expression is the smallest scope that
//!   contains it of a list naming "The pattern-matching condition(s) and consequent body of
//!   `if`" and "The pattern-matching condition and loop body of `while`"; edition 2024 ("if let
//!   temporary scope"): the scrutinee's temporaries of an `if let` are dropped before the `else`
//!   block (2021: at the end of the enclosing statement). These bodies are edition 2024
//!   (tests.rs `BodyBuf::view`), so the 2024 rule is what they check.
//! - "Place expressions and value expressions": the scrutinee of `if let` / `while let` is a
//!   place expression context: a place (a local, a field, a deref) is not a temporary at all:
//!   nothing is created, nothing is dropped.
//! - "Drop scopes": "When control flow leaves a drop scope all variables associated to that
//!   scope are dropped in reverse order of declaration (for variables) or creation (for
//!   temporaries)."
//!
//! `G { x }` logs `x` when dropped; the world is tests.rs `match_world` (`enum E { A(G, G),
//! B(G), C, W(N, G) }`, `enum N { P(G), Q }`). Without the fix of 2026-11-16 (lets.rs
//! `let_link` refusing with `tested_place`) (b), (c) and (d) fail with "an `if let` or `while
//! let` on a temporary that needs drop (its drop point is not built)"; (a) is the place form
//! `bstd::out::Held::flush` has, pinned so a fix that took a place for a temporary (and dropped
//! it) fails.

use super::*;

/// `G { x: <value> }`.
fn g_of(b: &mut BodyBuf, value: ExprId) -> ExprId {
    let path = b.path("G");
    let start = b.field_inits.len() as u32;
    b.field_inits.push(FieldInit { name: Name::of("x"), expr: value });
    b.e(Expr::Struct { path, fields: List { start, len: 1 }, base: blaze_typeck::StructBase::None })
}

/// `let <name> = G { x: v };` as a statement.
fn let_g(b: &mut BodyBuf, name: &str, v: u128) -> Stmt {
    let p = b.bind(name);
    let init = g_lit(b, v);
    Stmt::Let { pat: p, ty: None, init: Some(init), else_: None }
}

/// A mutable binding.
fn bind_mut(b: &mut BodyBuf, name: &str) -> PatId {
    b.pat(Pat::Bind { ann: BindAnn { by_ref: None, mutable: true }, name: Name::of(name), sub: None, ty: None })
}

/// `A(<name>, _)` as a pattern.
fn a_bind(b: &mut BodyBuf, name: &str) -> PatId {
    let (x, w) = (b.bind(name), b.pat(Pat::Wild));
    variant_pat(b, "A", &[x, w])
}

/// (a) The place form of `bstd::out::Held::flush`:
/// ```text
/// struct S { failed: N, len: u32 }
/// fn main() {
///     let mut s = S { failed: P(G{5}), len: 1 };
///     let r = &mut s;
///     if let <pat> = r.failed && r.len > 0 { let t = G{7}; }
/// }
/// ```
/// `r.failed` is a place behind a pointer (the Reference: "a place expression context ... does
/// not allocate a temporary location"): tested in place, never dropped by the `if let`. With
/// `P(_)` the branch is taken: `t` (7) at its block's end, then `s` at main's end drops its
/// `P(G5)`: [7, 5]. With `Q` not taken: only `s`: [5]. A place taken for a temporary shows 5
/// twice or 5 before 7.
fn place_scrutinee(taken: bool) -> Vec<i32, Global> {
    let mut w = TestWorld::new();
    let (g, n, _) = match_world(&mut w);
    w.strukt(120, "S", &[("failed", n), ("len", U32)]);
    let s_ty = PNode::Adt(ItemKey(120), List::EMPTY);
    let mut b = BodyBuf::new();
    let g5 = g_lit(&mut b, 5);
    let p5 = b.call("P", &[g5]);
    let one = b.int(1, None);
    let s_path = b.path("S");
    let start = b.field_inits.len() as u32;
    b.field_inits.push(FieldInit { name: Name::of("failed"), expr: p5 });
    b.field_inits.push(FieldInit { name: Name::of("len"), expr: one });
    let s_init = b.e(Expr::Struct { path: s_path, fields: List { start, len: 2 }, base: blaze_typeck::StructBase::None });
    let s = bind_mut(&mut b, "s");
    let s_l = b.local(s);
    let r_init = b.e(Expr::Ref { mutbl: Mutbl::Mut, operand: s_l });
    let r = b.bind("r");
    let pat = if taken {
        let w0 = b.pat(Pat::Wild);
        variant_pat(&mut b, "P", &[w0])
    } else {
        let q = b.path("Q");
        b.pat(Pat::Path(q))
    };
    let r1 = b.local(r);
    let failed = b.field(r1, "failed");
    let link = b.e(Expr::Let { pat, init: failed });
    let r2 = b.local(r);
    let len = b.field(r2, "len");
    let zero = b.int(0, None);
    let gt = b.bin(BinOp::Gt, len, zero);
    let cond = b.bin(BinOp::And, link, gt);
    let t = let_g(&mut b, "t", 7);
    let then = b.block(&[t], None);
    let iff = b.e(Expr::If { cond, then, else_: None });
    let root = b.block(
        &[
            Stmt::Let { pat: s, ty: None, init: Some(s_init), else_: None },
            Stmt::Let { pat: r, ty: None, init: Some(r_init), else_: None },
            Stmt::Expr { expr: iff, semi: true },
        ],
        None,
    );
    g_drops(&w, &b, root, [g, n, s_ty]).unwrap_or_else(|e| panic!("{e}"))
}

/// (a) prints nothing; expects [7, 5] taken and [5] not taken.
#[test]
fn an_if_let_on_a_field_behind_a_pointer_is_tested_in_place_and_drops_nothing() {
    let taken = place_scrutinee(true);
    let missed = place_scrutinee(false);
    assert_eq!(taken.as_slice(), &[7, 5], "taken");
    assert_eq!(missed.as_slice(), &[5], "not taken");
}

/// (b) `fn main() { if let A(x, _) = <A(G{1}, G{2}) | B(G{3})> { let t = G{7}; } else { let u =
/// G{8}; }; let k = G{9}; }`. The rule (2024): the scrutinee's temporary lives through the
/// consequent and drops at its end, after the binding (made later: "reverse order of ...
/// creation"); on a failed test it drops BEFORE the `else` block.
/// - taken: `t` (7) at the block's end; then `x` (1); then the temporary's rest (2, never 1
///   again); then `k` (9): [7, 1, 2, 9].
/// - not taken: `B(G3)` drops before `else` (3), then `u` (8), then `k`: [3, 8, 9]. (The 2021
///   rule would give [8, 3, 9]; these bodies are 2024.)
fn if_let_value(taken: bool) -> Vec<i32, Global> {
    let mut w = TestWorld::new();
    let (g, n, e) = match_world(&mut w);
    let mut b = BodyBuf::new();
    let value = scrut_value(&mut b, if taken { Scrut::A } else { Scrut::B });
    let pat = a_bind(&mut b, "x");
    let cond = b.e(Expr::Let { pat, init: value });
    let t = let_g(&mut b, "t", 7);
    let then = b.block(&[t], None);
    let u = let_g(&mut b, "u", 8);
    let els = b.block(&[u], None);
    let iff = b.e(Expr::If { cond, then, else_: Some(els) });
    let k = let_g(&mut b, "k", 9);
    let root = b.block(&[Stmt::Expr { expr: iff, semi: true }, k], None);
    g_drops(&w, &b, root, [g, n, e]).unwrap_or_else(|e| panic!("{e}"))
}

/// (b) expects [7, 1, 2, 9] taken and [3, 8, 9] not taken.
#[test]
fn an_if_let_temporary_drops_after_the_consequent_or_before_the_else_block() {
    let taken = if_let_value(true);
    let missed = if_let_value(false);
    assert_eq!(taken.as_slice(), &[7, 1, 2, 9], "taken");
    assert_eq!(missed.as_slice(), &[3, 8, 9], "not taken");
}

/// (c) `while let`, the lowering's `loop { if let .. { body } else { break } }`:
/// ```text
/// fn main() {
///     let mut i = 1;
///     while let A(d, _) = (if i < 4 { A(G{i}, G{i + 10}) } else { C }) {
///         if i == <stop> { break; }
///         i += 1;
///     }
///     let k = G{9};
/// }
/// ```
/// The rule: "The pattern-matching condition and loop body of `while`" is the temporary scope,
/// so each iteration's scrutinee is made and dropped in that iteration: `d` then its rest.
/// - `stop` 2: iterations 1 and 2, the `break` in the second drops `d` (2) and the rest (12)
///   once: [1, 11, 2, 12, 9].
/// - no `break` (stop 0): iterations 1 to 3, then `C` misses and the loop ends:
///   [1, 11, 2, 12, 3, 13, 9].
fn while_let(stop: u128) -> Vec<i32, Global> {
    let mut w = TestWorld::new();
    let (g, n, e) = match_world(&mut w);
    let mut b = BodyBuf::new();
    let i = bind_mut(&mut b, "i");
    let i_init = b.int(1, None);
    // The scrutinee: `(if i < 4 { A(G{i}, G{i + 10}) } else { C })`.
    let i1 = b.local(i);
    let four = b.int(4, None);
    let lt = b.bin(BinOp::Lt, i1, four);
    let i2 = b.local(i);
    let g_i = g_of(&mut b, i2);
    let i3 = b.local(i);
    let ten = b.int(10, None);
    let i10 = b.bin(BinOp::Add, i3, ten);
    let g_i10 = g_of(&mut b, i10);
    let a = b.call("A", &[g_i, g_i10]);
    let a_block = b.block(&[], Some(a));
    let c_path = b.path("C");
    let c = b.e(Expr::Path(c_path));
    let c_block = b.block(&[], Some(c));
    let pick = b.e(Expr::If { cond: lt, then: a_block, else_: Some(c_block) });
    let value = b.e(Expr::Paren(pick));
    let pat = a_bind(&mut b, "d");
    let cond = b.e(Expr::Let { pat, init: value });
    // The body: `if i == stop { break; } i += 1;` (`break`'s target is set below).
    let mut body: Vec<Stmt, Global> = Vec::new_in(Global);
    // Made only when used: an expression no statement reaches is refused by the checker.
    let brk = (stop != 0).then(|| b.e(Expr::Missing));
    if let Some(brk) = brk {
        let i4 = b.local(i);
        let s = b.int(stop, None);
        let eq = b.bin(BinOp::Eq, i4, s);
        let brk_s = b.block(&[Stmt::Expr { expr: brk, semi: true }], None);
        let stop_if = b.e(Expr::If { cond: eq, then: brk_s, else_: None });
        body.push(Stmt::Expr { expr: stop_if, semi: true });
    }
    let i5 = b.local(i);
    let one = b.int(1, None);
    let inc = b.e(Expr::AssignOp { op: BinOp::Add, lhs: i5, rhs: one });
    body.push(Stmt::Expr { expr: inc, semi: true });
    let lp = b.while_(cond, &body);
    if let Some(brk) = brk {
        b.set(brk, Expr::Break { value: None, target: lp });
    }
    let k = let_g(&mut b, "k", 9);
    let root = b.block(&[Stmt::Let { pat: i, ty: None, init: Some(i_init), else_: None }, Stmt::Expr { expr: lp, semi: true }, k], None);
    g_drops(&w, &b, root, [g, n, e]).unwrap_or_else(|e| panic!("{e}"))
}

/// (c) expects [1, 11, 2, 12, 9] with the `break` and [1, 11, 2, 12, 3, 13, 9] without.
#[test]
fn a_while_let_drops_each_iterations_scrutinee_and_a_break_drops_it_once() {
    let broken = while_let(2);
    let ran_out = while_let(0);
    assert_eq!(broken.as_slice(), &[1, 11, 2, 12, 9], "break at 2");
    assert_eq!(ran_out.as_slice(), &[1, 11, 2, 12, 3, 13, 9], "until `C`");
}

/// Which link of (d) fails.
#[derive(Clone, Copy, Debug)]
enum Chain {
    Both,
    Second,
    First,
}

/// (d) A chain with two `let` links, edition 2024 ("each operand is evaluated from
/// left-to-right until ... a `let` match fails"):
/// `if let A(x, _) = <first> && let B(y) = <second> { let t = G{7}; } else { let u = G{8}; };
/// let k = G{9};`. Both links' temporaries and bindings are in the one scope of "the
/// pattern-matching condition(s) and consequent body", dropped in reverse order of creation
/// (temporary 1, `x`, temporary 2, `y`); a failed link drops what exists at that link before
/// the `else` block.
/// - both match (`A(G1, G2)`, `B(G3)`): 7; then `y` (3), the second's rest (nothing), `x` (1),
///   the first's rest (2); then 9: [7, 3, 1, 2, 9].
/// - the second fails (`C`): `C` (nothing), `x` (1), the first's rest (2), then `u` (8), `k`:
///   [1, 2, 8, 9].
/// - the first fails (`B(G3)`): it drops whole (3), the second is never made; then 8, 9:
///   [3, 8, 9].
fn chain(c: Chain) -> Vec<i32, Global> {
    let mut w = TestWorld::new();
    let (g, n, e) = match_world(&mut w);
    let mut b = BodyBuf::new();
    let first = scrut_value(&mut b, if let Chain::First = c { Scrut::B } else { Scrut::A });
    let pa = a_bind(&mut b, "x");
    let l1 = b.e(Expr::Let { pat: pa, init: first });
    let second = match c {
        Chain::Second => scrut_value(&mut b, Scrut::C),
        _ => scrut_value(&mut b, Scrut::B),
    };
    let y = b.bind("y");
    let pb = variant_pat(&mut b, "B", &[y]);
    let l2 = b.e(Expr::Let { pat: pb, init: second });
    let cond = b.bin(BinOp::And, l1, l2);
    let t = let_g(&mut b, "t", 7);
    let then = b.block(&[t], None);
    let u = let_g(&mut b, "u", 8);
    let els = b.block(&[u], None);
    let iff = b.e(Expr::If { cond, then, else_: Some(els) });
    let k = let_g(&mut b, "k", 9);
    let root = b.block(&[Stmt::Expr { expr: iff, semi: true }, k], None);
    g_drops(&w, &b, root, [g, n, e]).unwrap_or_else(|e| panic!("{e}"))
}

/// (d) expects [7, 3, 1, 2, 9], [1, 2, 8, 9] and [3, 8, 9].
#[test]
fn a_let_chain_drops_each_links_temporary_and_binding_in_reverse_order_of_creation() {
    assert_eq!(chain(Chain::Both).as_slice(), &[7, 3, 1, 2, 9], "both match");
    assert_eq!(chain(Chain::Second).as_slice(), &[1, 2, 8, 9], "the second fails");
    assert_eq!(chain(Chain::First).as_slice(), &[3, 8, 9], "the first fails");
}
