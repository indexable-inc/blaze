//! Matches by tests (walk/matches.rs): a reference to a tuple, a `char`, ranges, slice
//! patterns, nested or-patterns, guards, `x @ ..`; built from a body the checker types, run
//! natively, the result compared.

use super::*;

/// A binding with no annotation: the default binding mode applies.
fn plain(b: &mut BodyBuf, name: &str) -> PatId {
    b.pat(Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of(name), sub: None, ty: None })
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

fn f64_expr(b: &mut BodyBuf, x: f64) -> ExprId {
    let e = b.e(Expr::Lit(Lit::Float(Some(blaze_typeck::FloatTy::F64))));
    b.lits.push((e, u128::from(x.to_bits())));
    e
}

/// A float literal pattern of `bits` (tests.rs `float_pat` reads `pat_lits`).
fn f64_pat(b: &mut BodyBuf, bits: u64) -> PatId {
    let p = b.pat(Pat::Lit(Lit::Float(Some(blaze_typeck::FloatTy::F64))));
    b.pat_lits.push((p, u128::from(bits)));
    p
}

fn str_pat(b: &mut BodyBuf, text: &'static str) -> PatId {
    let p = b.pat(Pat::Lit(Lit::Str));
    b.pat_strs.push((p, text));
    p
}

fn str_expr(b: &mut BodyBuf, text: &'static str) -> ExprId {
    let e = b.e(Expr::Lit(Lit::Str));
    b.strs.push((e, text));
    e
}

/// walk/matches/forms.rs. The Reference, "Or-patterns": every alternative binds the same
/// names at the same types, and the first alternative whose tests pass binds them; "Path
/// patterns": a path to a constant is refutable, compared with the constant's value;
/// "Literal patterns": "Literal patterns match exactly the same value as what is created by
/// the literal", a float compared as `==` compares (IEEE 754: a NaN equals nothing, `-0.0 ==
/// 0.0`), a string by its bytes. (These quotes are from memory: no copy of the Reference was
/// reachable from this lane.) The NaN arm stands for a pattern of NaN's bits, which the
/// language refuses as a constant pattern; built, it must still never match.
/// ```text
/// const K: u32 = 5;
/// fn main() -> u32 {
///     let t = (7u32, 1u32);
///     let a: u32 = match t { (1, n) | (n, 1) => n, _ => 9 };          // 7: the second alternative
///     let c1: u32 = match 6u32 { K => 10, _ => 20 };                   // 20
///     let c2: u32 = match 5u32 { K => 1, _ => 2 };                     // 1
///     let n = 0.0f64 / 0.0;
///     let z = -0.0f64;
///     let f1: u32 = match n { <NaN's bits> => 1, 0.0 => 2, _ => 3 };   // 3
///     let f2: u32 = match z { 0.0 => 4, _ => 5 };                      // 4
///     let s = "ab";
///     let g1: u32 = match s { "ac" => 1, "abc" => 2, "ab" => 3, _ => 4 };            // 3
///     let s2 = "0123456789";
///     let g2: u32 = match s2 { "0123456788" => 1, "0123456789" => 2, _ => 3 };      // 2
///     a * 1_000_000 + (c1 + c2) * 10_000 + (f1 * 10 + f2) * 100 + g1 * 10 + g2
/// }   // 7_000_000 + 210_000 + 3_400 + 32 = 7_213_432
/// ```
/// Each wrong reading gives another number: the binding read from the first alternative's
/// place (`a` 1) or its slot left unwritten (`a` 0 or garbage); a constant as a catch-all
/// binding (`c1` 10: 7_113_432); NaN equal to a NaN pattern by bits or by an unordered
/// compare (`f1` 1: 7_211_432); `-0.0` compared by bits (`f2` 5: 7_213_532); a string of
/// equal length and other bytes matching (`g1` 1: 7_213_412; `g2` 1, a word compare that
/// skips the tail: 7_213_431).
#[test]
fn binding_ors_consts_floats_and_strings_match_by_value() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    w.func(30, "K", &[], U32);
    w.consts.push((ItemKey(30), Some(record(&[5, 0, 0, 0], 2))));
    let mut b = BodyBuf::new();
    let u32_ty = b.types.intern(U32).expect("a type interns");
    let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
    let let_u32 =|b: &mut BodyBuf, stmts: &mut Vec<Stmt, Global>, name: &str, init: ExprId| -> PatId {
        let p = b.bind(name);
        stmts.push(Stmt::Let { pat: p, ty: Some(u32_ty), init: Some(init), else_: None });
        p
    };

    // let t = (7u32, 1u32); let a: u32 = match t { (1, n) | (n, 1) => n, _ => 9 };
    let t = b.bind("t");
    let (seven, one_) = (b.int(7, SU32), b.int(1, SU32));
    let t_init = b.tuple(&[seven, one_]);
    stmts.push(Stmt::Let { pat: t, ty: None, init: Some(t_init), else_: None });
    let (k1, n1) = (b.pat_int(1), plain(&mut b, "n"));
    let alt1 = tuple_pat(&mut b, &[k1, n1]);
    let (n2, k2) = (plain(&mut b, "n"), b.pat_int(1));
    let alt2 = tuple_pat(&mut b, &[n2, k2]);
    let o = or(&mut b, &[alt1, alt2]);
    // A use of `n` resolves to its first binding (blaze-lower machine.rs `activate`).
    let n_e = b.local(n1);
    let w0 = b.pat(Pat::Wild);
    let nine = b.int(9, None);
    let list = arms(&mut b, &[(o, None, n_e), (w0, None, nine)]);
    let t_e = b.local(t);
    let ma = b.e(Expr::Match { scrutinee: t_e, arms: list });
    let a = let_u32(&mut b, &mut stmts, "a", ma);

    // match 6u32 { K => 10, _ => 20 } and match 5u32 { K => 1, _ => 2 }
    let const_match =|b: &mut BodyBuf, x: u128, hit: u128, miss: u128| -> ExprId {
        let s = b.int(x, SU32);
        let kp = b.path("K");
        let kpat = b.pat(Pat::Path(kp));
        let wp = b.pat(Pat::Wild);
        let (h, m) = (b.int(hit, None), b.int(miss, None));
        let list = arms(b, &[(kpat, None, h), (wp, None, m)]);
        b.e(Expr::Match { scrutinee: s, arms: list })
    };
    let m1 = const_match(&mut b, 6, 10, 20);
    let c1 = let_u32(&mut b, &mut stmts, "c1", m1);
    let m2 = const_match(&mut b, 5, 1, 2);
    let c2 = let_u32(&mut b, &mut stmts, "c2", m2);

    // let n = 0.0f64 / 0.0; let z = -0.0f64;
    let n = b.bind("n");
    let (z1, z2) = (f64_expr(&mut b, 0.0), f64_expr(&mut b, 0.0));
    let n_init = b.bin(BinOp::Div, z1, z2);
    stmts.push(Stmt::Let { pat: n, ty: None, init: Some(n_init), else_: None });
    let z = b.bind("z");
    let zero = f64_expr(&mut b, 0.0);
    let z_init = b.e(Expr::Unary { op: blaze_typeck::UnOp::Neg, operand: zero });
    stmts.push(Stmt::Let { pat: z, ty: None, init: Some(z_init), else_: None });
    // match n { <NaN> => 1, 0.0 => 2, _ => 3 }
    let (pn, p0, wn) = (f64_pat(&mut b, f64::NAN.to_bits()), f64_pat(&mut b, 0.0f64.to_bits()), b.pat(Pat::Wild));
    let (r1, r2, r3) = (b.int(1, None), b.int(2, None), b.int(3, None));
    let list = arms(&mut b, &[(pn, None, r1), (p0, None, r2), (wn, None, r3)]);
    let n_e = b.local(n);
    let mf1 = b.e(Expr::Match { scrutinee: n_e, arms: list });
    let f1 = let_u32(&mut b, &mut stmts, "f1", mf1);
    // match z { 0.0 => 4, _ => 5 }
    let (pz, wz) = (f64_pat(&mut b, 0.0f64.to_bits()), b.pat(Pat::Wild));
    let (r4, r5) = (b.int(4, None), b.int(5, None));
    let list = arms(&mut b, &[(pz, None, r4), (wz, None, r5)]);
    let z_e = b.local(z);
    let mf2 = b.e(Expr::Match { scrutinee: z_e, arms: list });
    let f2 = let_u32(&mut b, &mut stmts, "f2", mf2);

    // let s = "ab"; match s { "ac" => 1, "abc" => 2, "ab" => 3, _ => 4 }
    let s = b.bind("s");
    let s_init = str_expr(&mut b, "ab");
    stmts.push(Stmt::Let { pat: s, ty: None, init: Some(s_init), else_: None });
    let (q1, q2, q3, q4) = (str_pat(&mut b, "ac"), str_pat(&mut b, "abc"), str_pat(&mut b, "ab"), b.pat(Pat::Wild));
    let (r1, r2, r3, r4) = (b.int(1, None), b.int(2, None), b.int(3, None), b.int(4, None));
    let list = arms(&mut b, &[(q1, None, r1), (q2, None, r2), (q3, None, r3), (q4, None, r4)]);
    let s_e = b.local(s);
    let mg1 = b.e(Expr::Match { scrutinee: s_e, arms: list });
    let g1 = let_u32(&mut b, &mut stmts, "g1", mg1);
    // let s2 = "0123456789"; match s2 { "0123456788" => 1, "0123456789" => 2, _ => 3 }
    let s2 = b.bind("s2");
    let s2_init = str_expr(&mut b, "0123456789");
    stmts.push(Stmt::Let { pat: s2, ty: None, init: Some(s2_init), else_: None });
    let (q1, q2, q3) = (str_pat(&mut b, "0123456788"), str_pat(&mut b, "0123456789"), b.pat(Pat::Wild));
    let (r1, r2, r3) = (b.int(1, None), b.int(2, None), b.int(3, None));
    let list = arms(&mut b, &[(q1, None, r1), (q2, None, r2), (q3, None, r3)]);
    let s2_e = b.local(s2);
    let mg2 = b.e(Expr::Match { scrutinee: s2_e, arms: list });
    let g2 = let_u32(&mut b, &mut stmts, "g2", mg2);

    // a * 1_000_000 + (c1 + c2) * 10_000 + (f1 * 10 + f2) * 100 + g1 * 10 + g2
    let times = |b: &mut BodyBuf, x: ExprId, k: u128| {
        let k = b.int(k, None);
        b.bin(BinOp::Mul, x, k)
    };
    let a_e = b.local(a);
    let a_k = times(&mut b, a_e, 1_000_000);
    let (c1_e, c2_e) = (b.local(c1), b.local(c2));
    let c = b.bin(BinOp::Add, c1_e, c2_e);
    let c_k = times(&mut b, c, 10_000);
    let (f1_e, f2_e) = (b.local(f1), b.local(f2));
    let f1_k = times(&mut b, f1_e, 10);
    let f = b.bin(BinOp::Add, f1_k, f2_e);
    let f_k = times(&mut b, f, 100);
    let (g1_e, g2_e) = (b.local(g1), b.local(g2));
    let g1_k = times(&mut b, g1_e, 10);
    let g = b.bin(BinOp::Add, g1_k, g2_e);
    let sum = b.bin(BinOp::Add, a_k, c_k);
    let sum = b.bin(BinOp::Add, sum, f_k);
    let sum = b.bin(BinOp::Add, sum, g);
    let root = b.block(&stmts, Some(sum));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    #[cfg(target_arch = "aarch64")]
    assert_eq!(run_native(&[(MAIN, &built)]) as u32, 7_213_432);
    let _ = built;
}

/// Moves out of a match by tests' scrutinee (walk/moves.rs). The Reference, "Identifier
/// patterns": "By default, identifier patterns bind a variable to a copy of or move from the
/// matched value depending on whether the matched value implements `Copy`"; "Destructors":
/// "If a variable has been partially initialized, only its initialized fields are dropped";
/// "The fields of a tuple are dropped in order"; a scope's variables "in reverse order of
/// declaration"; "Temporary scopes": the temporary scope of an expression is the smallest of a
/// list that does not name a `match` scrutinee, so a temporary scrutinee lives to the end of
/// its statement. A tuple of three droppable fields (`G` appends its `x` when dropped) and a
/// selector; the second arm moves the first `G` out and returns it, the first moves none:
/// ```text
/// fn main() {                                             // sel = 1          sel = 0
///     let t = (sel, G { x: 1 }, G { x: 2 }, G { x: 3 });
///     let r = match t { (0, _, _, _) => G { x: 7 }, (_, a, _, _) => a };
///     let k = G { x: 9 };
/// }   // k, r, then what is left of t, field by field:      9, 1, 2, 3       9, 7, 1, 2, 3
/// fn main() {
///     let r = match (sel, G { x: 1 }, G { x: 2 }, G { x: 3 }) { /* the same arms */ };
///     // the temporary's rest drops here, at the `let`'s end:  2, 3             1, 2, 3
///     let k = G { x: 9 };
/// }   // then k and r:                                      9, 1             9, 7
/// ```
/// A double drop (the scrutinee dropped whole after the move) gives `9, 1, 1, 2, 3`; a missed
/// drop (the rest never dropped, or the moved part only) gives a shorter list; a temporary held
/// to the block's end gives `9, 1, 2, 3`. Only `G`'s glue is given: the rest drops field by
/// field. Prints nothing; passes as "test ... ok".
#[test]
fn a_match_by_tests_moves_a_part_out_and_drops_the_rest_once() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], UNIT);
    let g = g_world(&mut w);
    let cases: [(bool, u128, &[i32]); 4] =
        [(false, 1, &[9, 1, 2, 3]), (false, 0, &[9, 7, 1, 2, 3]), (true, 1, &[2, 3, 9, 1]), (true, 0, &[1, 2, 3, 9, 7])];
    for (temp, sel, want) in cases {
        let mut b = BodyBuf::new();
        let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
        let s = b.int(sel, None);
        let (g1, g2, g3) = (g_lit(&mut b, 1), g_lit(&mut b, 2), g_lit(&mut b, 3));
        let value = b.tuple(&[s, g1, g2, g3]);
        let scrutinee = if temp {
            value
        } else {
            let t = b.bind("t");
            stmts.push(Stmt::Let { pat: t, ty: None, init: Some(value), else_: None });
            b.local(t)
        };
        let zero = b.pat_int(0);
        let (w1, w2, w3) = (b.pat(Pat::Wild), b.pat(Pat::Wild), b.pat(Pat::Wild));
        let p0 = tuple_pat(&mut b, &[zero, w1, w2, w3]);
        let g7 = g_lit(&mut b, 7);
        let a = plain(&mut b, "a");
        let (v0, v2, v3) = (b.pat(Pat::Wild), b.pat(Pat::Wild), b.pat(Pat::Wild));
        let p1 = tuple_pat(&mut b, &[v0, a, v2, v3]);
        let a_e = b.local(a);
        let list = arms(&mut b, &[(p0, None, g7), (p1, None, a_e)]);
        let m = b.e(Expr::Match { scrutinee, arms: list });
        let r = b.bind("r");
        stmts.push(Stmt::Let { pat: r, ty: None, init: Some(m), else_: None });
        let k = b.bind("k");
        let k_init = g_lit(&mut b, 9);
        stmts.push(Stmt::Let { pat: k, ty: None, init: Some(k_init), else_: None });
        let root = b.block(&stmts, None);
        let mut ground = PortableTys::new_in(Alloc::global());
        let main = build_instance(&w, &b, MAIN, root, &[], &mut ground).unwrap_or_else(|e| panic!("temp={temp} sel={sel}: {e}"));
        #[cfg(target_arch = "aarch64")]
        {
            let mut gb = BodyBuf::new();
            let p = gb.bind("to_drop");
            gb.params.push(p);
            let groot = gb.block(&[], None);
            let view = gb.view(DROP_IN_PLACE, groot);
            let mut cx = InferCtx::new_in(Alloc::global());
            let checked = match blaze_typeck::check(&view, &w, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
                Ok(Outcome::Done(c)) => c,
                other => panic!("the check did not finish: {:?}", other.err()),
            };
            let sig = w.sig(DROP_IN_PLACE).expect("a signature");
            let ty = ground.intern(g).expect("intern");
            let args = ground.intern_list(&[ty]).expect("intern");
            let mut subst = Subst { args: &[ty], ground: &mut ground, scratch: Alloc::global() };
            let glue = crate::build_drop_glue(&view, &checked, &sig, &mut subst, &TestAdts { world: &w, body: &gb }, Global)
                .unwrap_or_else(|e| panic!("{e}"));
            DROPPED.with(|d| d.borrow_mut().clear());
            run_native_with(&[(G_DROP, host_g_drop as usize as u64)], &[(DROP_IN_PLACE, Some(args), &glue), (MAIN, None, &main)]);
            let seen = DROPPED.with(|d| d.borrow().clone());
            assert_eq!(seen.as_slice(), want, "temp={temp} sel={sel}");
        }
        let _ = (main, want, g);
    }
}
