//! Matches by tests (walk/matches.rs): a reference to a tuple, a `char`, ranges, slice
//! patterns, nested or-patterns, guards, `x @ ..`; built from a body the checker types, run
//! natively, the result compared.

use super::*;

/// A binding with no annotation: the default binding mode applies.
fn plain(b: &mut BodyBuf, name: &str) -> PatId {
    b.pat(Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of(name), sub: None })
}

fn char_pat(b: &mut BodyBuf, c: char) -> PatId {
    let p = b.pat(Pat::Lit(Lit::Char));
    b.pat_lits.push((p, u128::from(u32::from(c))));
    p
}

fn range(b: &mut BodyBuf, lo: PatId, hi: PatId) -> PatId {
    b.pat(Pat::Range { lo: Some(lo), hi: Some(hi), inclusive: true })
}

fn or(b: &mut BodyBuf, alts: &[PatId]) -> PatId {
    let alts = b.pats_list(alts);
    b.pat(Pat::Or { alts })
}

fn tuple_pat(b: &mut BodyBuf, elems: &[PatId]) -> PatId {
    let elems = b.pats_list(elems);
    b.pat(Pat::Tuple { elems, rest: None })
}

fn slice_pat(b: &mut BodyBuf, elems: &[PatId], rest: Option<u32>) -> PatId {
    let elems = b.pats_list(elems);
    b.pat(Pat::Slice { elems, rest })
}

fn deref(b: &mut BodyBuf, p: PatId) -> ExprId {
    let e = b.local(p);
    b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: e })
}

fn arms(b: &mut BodyBuf, list: &[(PatId, Option<ExprId>, ExprId)]) -> List {
    let start = b.arms.len() as u32;
    for &(pat, guard, body) in list {
        b.arms.push(Arm { pat, guard, body });
    }
    List { start, len: list.len() as u32 }
}

/// The Reference, "Match expressions": "the first arm whose pattern matches (and whose guard,
/// if any, is true) is taken"; "Binding modes": "Each time a reference is matched using a
/// non-reference pattern, it will automatically dereference the value and update the default
/// binding mode. References will set the default binding mode to `ref`"; "Range patterns": "An
/// inclusive range pattern matches all values from the lower bound up to and including the
/// upper bound"; "Slice patterns" with `..` and `x @ ..`. The arms overlap, so a wrong order
/// or a guard that does not fall through gives another number.
/// ```text
/// enum E { P(u32), Q }
/// fn main() -> u32 {
///     let t = (3u32, 'q');
///     let a: u32 = match &t {
///         (0..=2, _) => 10,
///         (n, 'a'..='z') if *n == 4 => 20,   // the guard fails: falls to the next arm
///         (n, 'p' | 'q') => *n * 100,        // 300; `n: &u32`
///         _ => 7,
///     };
///     let arr = [5u8, 6, 9];
///     let s: &[u8] = &arr;
///     let c: u32 = match s {
///         [] => 1,
///         [_] => 2,
///         [first, .., _] if *first == 6 => 3,
///         [first, rest @ ..] => match rest { [x, y] => *first as u32 * 100 + *x as u32 * 10 + *y as u32, _ => 4 },
///     };                                      // 569
///     let e = E::P(7);
///     let d: u32 = match e { E::P(0..=5) => 1, E::P(6 | 7) => 2, E::P(_) => 3, E::Q => 4 };
///     a * 10000 + c * 10 + d                  // 3_005_692
/// }
/// ```
#[test]
fn tuples_refs_chars_ranges_slices_and_ors_match_in_order() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    w.enum_(40, &[("P", CtorKind::Tuple, &[("0", U32)]), ("Q", CtorKind::Unit, &[])]);
    let mut b = BodyBuf::new();
    let u32_ty = b.types.intern(U32).expect("a type interns");
    let u8_ty = b.types.intern(U8).expect("a type interns");
    let slice = b.types.intern(PNode::Slice(u8_ty)).expect("a type interns");
    let ref_slice = b.types.intern(PNode::Ref(Mutbl::Not, slice)).expect("a type interns");
    let cast = |b: &mut BodyBuf, x: ExprId| b.e(Expr::Cast { operand: x, ty: u32_ty });
    let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);

    // let t = (3u32, 'q'); let a: u32 = match &t { .. };
    let t = b.bind("t");
    let three = b.int(3, Some(IntSuffix::Uint(UintTy::U32)));
    let q = b.e(Expr::Lit(Lit::Char));
    b.lits.push((q, u128::from(u32::from('q'))));
    let t_init = b.tuple(&[three, q]);
    stmts.push(Stmt::Let { pat: t, ty: None, init: Some(t_init), else_: None });
    let (lo, hi) = (b.pat_int(0), b.pat_int(2));
    let r = range(&mut b, lo, hi);
    let w0 = b.pat(Pat::Wild);
    let p1 = tuple_pat(&mut b, &[r, w0]);
    let n2 = plain(&mut b, "n");
    let (ca, cz) = (char_pat(&mut b, 'a'), char_pat(&mut b, 'z'));
    let rc = range(&mut b, ca, cz);
    let p2 = tuple_pat(&mut b, &[n2, rc]);
    let n2e = deref(&mut b, n2);
    let four = b.int(4, None);
    let g2 = b.bin(BinOp::Eq, n2e, four);
    let n3 = plain(&mut b, "n");
    let (cp, cq) = (char_pat(&mut b, 'p'), char_pat(&mut b, 'q'));
    let pq = or(&mut b, &[cp, cq]);
    let p3 = tuple_pat(&mut b, &[n3, pq]);
    let n3e = deref(&mut b, n3);
    let hundred = b.int(100, None);
    let body3 = b.bin(BinOp::Mul, n3e, hundred);
    let p4 = b.pat(Pat::Wild);
    let (b1, b2, b4) = (b.int(10, None), b.int(20, None), b.int(7, None));
    let list = arms(&mut b, &[(p1, None, b1), (p2, Some(g2), b2), (p3, None, body3), (p4, None, b4)]);
    let te = b.local(t);
    let rt = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: te });
    let ma = b.e(Expr::Match { scrutinee: rt, arms: list });
    let a = b.bind("a");
    stmts.push(Stmt::Let { pat: a, ty: Some(u32_ty), init: Some(ma), else_: None });

    // let arr = [5u8, 6, 9]; let s: &[u8] = &arr; let c: u32 = match s { .. };
    let arr = b.bind("arr");
    let (e5, e6, e9) = (b.int(5, None), b.int(6, None), b.int(9, None));
    let arr_init = b.array(&[e5, e6, e9]);
    stmts.push(Stmt::Let { pat: arr, ty: None, init: Some(arr_init), else_: None });
    let s = b.bind("s");
    let arr_e = b.local(arr);
    let s_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: arr_e });
    stmts.push(Stmt::Let { pat: s, ty: Some(ref_slice), init: Some(s_init), else_: None });
    let q1 = slice_pat(&mut b, &[], None);
    let w1 = b.pat(Pat::Wild);
    let q2 = slice_pat(&mut b, &[w1], None);
    let (f3, dots, w3) = (plain(&mut b, "first"), b.pat(Pat::Wild), b.pat(Pat::Wild));
    let q3 = slice_pat(&mut b, &[f3, dots, w3], Some(1));
    let f3e = deref(&mut b, f3);
    let six = b.int(6, None);
    let g3 = b.bin(BinOp::Eq, f3e, six);
    let (f4, rest) = (plain(&mut b, "first"), plain(&mut b, "rest"));
    let q4 = slice_pat(&mut b, &[f4, rest], Some(1));
    // match rest { [x, y] => .., _ => 4 }
    let (x, y) = (plain(&mut b, "x"), plain(&mut b, "y"));
    let xy = slice_pat(&mut b, &[x, y], None);
    let f4e = deref(&mut b, f4);
    let f4e = cast(&mut b, f4e);
    let k = b.int(100, None);
    let f4k = b.bin(BinOp::Mul, f4e, k);
    let xe = deref(&mut b, x);
    let xe = cast(&mut b, xe);
    let k = b.int(10, None);
    let xk = b.bin(BinOp::Mul, xe, k);
    let ye = deref(&mut b, y);
    let ye = cast(&mut b, ye);
    let sum = b.bin(BinOp::Add, f4k, xk);
    let sum = b.bin(BinOp::Add, sum, ye);
    let w5 = b.pat(Pat::Wild);
    let four = b.int(4, None);
    let inner = arms(&mut b, &[(xy, None, sum), (w5, None, four)]);
    let rest_e = b.local(rest);
    let body4 = b.e(Expr::Match { scrutinee: rest_e, arms: inner });
    let (c1, c2, c3) = (b.int(1, None), b.int(2, None), b.int(3, None));
    let list = arms(&mut b, &[(q1, None, c1), (q2, None, c2), (q3, Some(g3), c3), (q4, None, body4)]);
    let s_e = b.local(s);
    let mc = b.e(Expr::Match { scrutinee: s_e, arms: list });
    let c = b.bind("c");
    stmts.push(Stmt::Let { pat: c, ty: Some(u32_ty), init: Some(mc), else_: None });

    // let e = E::P(7); let d: u32 = match e { .. };
    let e = b.bind("e");
    let seven = b.int(7, None);
    let e_init = b.call("P", &[seven]);
    stmts.push(Stmt::Let { pat: e, ty: None, init: Some(e_init), else_: None });
    let (lo, hi) = (b.pat_int(0), b.pat_int(5));
    let r = range(&mut b, lo, hi);
    let v1 = variant_pat(&mut b, "P", &[r]);
    let (six, seven) = (b.pat_int(6), b.pat_int(7));
    let o = or(&mut b, &[six, seven]);
    let v2 = variant_pat(&mut b, "P", &[o]);
    let w6 = b.pat(Pat::Wild);
    let v3 = variant_pat(&mut b, "P", &[w6]);
    let qp = b.path("Q");
    let v4 = b.pat(Pat::Path(qp));
    let (d1, d2, d3, d4) = (b.int(1, None), b.int(2, None), b.int(3, None), b.int(4, None));
    let list = arms(&mut b, &[(v1, None, d1), (v2, None, d2), (v3, None, d3), (v4, None, d4)]);
    let e_e = b.local(e);
    let md = b.e(Expr::Match { scrutinee: e_e, arms: list });
    let d = b.bind("d");
    stmts.push(Stmt::Let { pat: d, ty: Some(u32_ty), init: Some(md), else_: None });

    let (a_e, c_e, d_e) = (b.local(a), b.local(c), b.local(d));
    let k = b.int(10000, None);
    let a_k = b.bin(BinOp::Mul, a_e, k);
    let k = b.int(10, None);
    let c_k = b.bin(BinOp::Mul, c_e, k);
    let sum = b.bin(BinOp::Add, a_k, c_k);
    let sum = b.bin(BinOp::Add, sum, d_e);
    let root = b.block(&stmts, Some(sum));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    #[cfg(target_arch = "aarch64")]
    assert_eq!(run_native(&[(MAIN, &built)]) as u32, 3_005_692);
    let _ = built;
}
