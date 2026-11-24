//! `let` in a condition and `let`-`else` (walk/lets.rs): `if let`, a let chain, `while let`
//! (the lowering's `loop { if let .. { .. } else { break } }`) and `let`-`else` on the pass
//! and on the diverging path; built from a body the checker types, run natively, the result
//! compared.

use super::*;

/// A binding with no annotation.
fn plain(b: &mut BodyBuf, name: &str) -> PatId {
    b.pat(Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of(name), sub: None, ty: None })
}

/// `S(name)` as a pattern, and the binding.
fn s_of(b: &mut BodyBuf, name: &str) -> (PatId, PatId) {
    let x = plain(b, name);
    (variant_pat(b, "S", &[x]), x)
}

fn let_(b: &mut BodyBuf, pat: PatId, scrutinee: PatId) -> ExprId {
    let init = b.local(scrutinee);
    b.e(Expr::Let { pat, init })
}

/// `if cond { then } else { else_ }`, each branch a block with the tail given.
fn if_else(b: &mut BodyBuf, cond: ExprId, then: ExprId, else_: ExprId) -> ExprId {
    let then = b.block(&[], Some(then));
    let else_ = b.block(&[], Some(else_));
    b.e(Expr::If { cond, then, else_: Some(else_) })
}

/// The Reference, "If expressions": "If the value of the scrutinee matches the pattern, the
/// corresponding block will execute. Otherwise, flow proceeds to the following `else` block";
/// chains (edition 2024): "each operand is evaluated from left-to-right until an operand
/// evaluates as `false` or a `let` match fails"; "The bindings of each pattern are put into
/// scope to be available for the next condition operand and the consequent block"; "let
/// statements": "If the pattern does not match ..., the `else` block is executed. The `else`
/// block must always diverge". Expected 340_760_448: `c1` 340 only if the chain's later links
/// see `x` = 3 and `y` = 40 (a wrong binding gives another number, a wrong `else` 1); `c2` 7
/// only if a failed LATER link goes to the `else`; `c3` 5 only if a false `bool` link after a
/// `let` goes to the `else`; `while let` sums 4 + 3 + 2 + 1 = 10 in exactly 4 runs (`i`: a run
/// too many gives 5); `let`-`else` binds `z` = 40 on a match and runs `break 8` on a miss.
/// ```text
/// enum O { S(u32), N }
/// fn main() -> u32 {
///     let p = S(3); let q = S(40); let n = N;
///     let c1: u32 = if let S(x) = p && x == 3 && let S(y) = q { x * 100 + y } else { 1 };  // 340
///     let c2: u32 = if let S(x) = p && let S(y) = n { x + y } else { 7 };                  // 7
///     let c3: u32 = if let S(x) = q && x < 10 { 9 } else { 5 };                            // 5
///     let mut k = S(4); let mut sum = 0; let mut i = 0;
///     while let S(v) = k { sum += v; i += 1; k = if v == 1 { N } else { S(v - 1) }; }     // 10, 4
///     let S(z) = q else { return 0 };                                                      // 40
///     let r: u32 = loop { let S(m) = n else { break 8 }; break m; };                       // 8
///     c1 * 1_000_000 + c2 * 100_000 + c3 * 10_000 + sum * 1_000 + i * 100 + z + r
/// }
/// ```
#[test]
fn let_chains_while_let_and_let_else_take_the_right_branch() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    w.enum_(40, &[("S", CtorKind::Tuple, &[("0", U32)]), ("N", CtorKind::Unit, &[])]);
    let mut b = BodyBuf::new();
    let u32_ty = b.types.intern(U32).expect("a type interns");
    let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);

    // let p = S(3); let q = S(40); let n = N;
    let (p, q, n) = (b.bind("p"), b.bind("q"), b.bind("n"));
    let three = b.int(3, None);
    let p_init = b.call("S", &[three]);
    let forty = b.int(40, None);
    let q_init = b.call("S", &[forty]);
    let n_path = b.path("N");
    let n_init = b.e(Expr::Path(n_path));
    stmts.push(Stmt::Let { pat: p, ty: None, init: Some(p_init), else_: None });
    stmts.push(Stmt::Let { pat: q, ty: None, init: Some(q_init), else_: None });
    stmts.push(Stmt::Let { pat: n, ty: None, init: Some(n_init), else_: None });

    // let c1: u32 = if let S(x) = p && x == 3 && let S(y) = q { x * 100 + y } else { 1 };
    let (sx, x) = s_of(&mut b, "x");
    let l1 = let_(&mut b, sx, p);
    let xe = b.local(x);
    let k3 = b.int(3, None);
    let x3 = b.bin(BinOp::Eq, xe, k3);
    let (sy, y) = s_of(&mut b, "y");
    let l2 = let_(&mut b, sy, q);
    let and1 = b.bin(BinOp::And, l1, x3);
    let cond1 = b.bin(BinOp::And, and1, l2);
    let xe = b.local(x);
    let k100 = b.int(100, None);
    let x100 = b.bin(BinOp::Mul, xe, k100);
    let ye = b.local(y);
    let then1 = b.bin(BinOp::Add, x100, ye);
    let one = b.int(1, None);
    let if1 = if_else(&mut b, cond1, then1, one);
    let c1 = b.bind("c1");
    stmts.push(Stmt::Let { pat: c1, ty: Some(u32_ty), init: Some(if1), else_: None });

    // let c2: u32 = if let S(x) = p && let S(y) = n { x + y } else { 7 };
    let (sx, x) = s_of(&mut b, "x");
    let l1 = let_(&mut b, sx, p);
    let (sy, y) = s_of(&mut b, "y");
    let l2 = let_(&mut b, sy, n);
    let cond2 = b.bin(BinOp::And, l1, l2);
    let (xe, ye) = (b.local(x), b.local(y));
    let then2 = b.bin(BinOp::Add, xe, ye);
    let seven = b.int(7, None);
    let if2 = if_else(&mut b, cond2, then2, seven);
    let c2 = b.bind("c2");
    stmts.push(Stmt::Let { pat: c2, ty: Some(u32_ty), init: Some(if2), else_: None });

    // let c3: u32 = if let S(x) = q && x < 10 { 9 } else { 5 };
    let (sx, x) = s_of(&mut b, "x");
    let l1 = let_(&mut b, sx, q);
    let xe = b.local(x);
    let ten = b.int(10, None);
    let lt = b.bin(BinOp::Lt, xe, ten);
    let cond3 = b.bin(BinOp::And, l1, lt);
    let (nine, five) = (b.int(9, None), b.int(5, None));
    let if3 = if_else(&mut b, cond3, nine, five);
    let c3 = b.bind("c3");
    stmts.push(Stmt::Let { pat: c3, ty: Some(u32_ty), init: Some(if3), else_: None });

    // let mut k = S(4); let mut sum = 0; let mut i = 0;
    let (k, sum, i) = (b.bind("k"), b.bind("sum"), b.bind("i"));
    let four = b.int(4, None);
    let k_init = b.call("S", &[four]);
    let (z0, z1) = (b.int(0, None), b.int(0, None));
    stmts.push(Stmt::Let { pat: k, ty: None, init: Some(k_init), else_: None });
    stmts.push(Stmt::Let { pat: sum, ty: Some(u32_ty), init: Some(z0), else_: None });
    stmts.push(Stmt::Let { pat: i, ty: Some(u32_ty), init: Some(z1), else_: None });
    // while let S(v) = k { sum += v; i += 1; k = if v == 1 { N } else { S(v - 1) }; }
    let (sv, v) = s_of(&mut b, "v");
    let wl = let_(&mut b, sv, k);
    let (sum_l, ve) = (b.local(sum), b.local(v));
    let add_sum = b.e(Expr::AssignOp { op: BinOp::Add, lhs: sum_l, rhs: ve });
    let i_l = b.local(i);
    let one = b.int(1, None);
    let add_i = b.e(Expr::AssignOp { op: BinOp::Add, lhs: i_l, rhs: one });
    let ve = b.local(v);
    let one = b.int(1, None);
    let last = b.bin(BinOp::Eq, ve, one);
    let n_path = b.path("N");
    let none = b.e(Expr::Path(n_path));
    let ve = b.local(v);
    let one = b.int(1, None);
    let less = b.bin(BinOp::Sub, ve, one);
    let next = b.call("S", &[less]);
    let k_next = if_else(&mut b, last, none, next);
    let k_l = b.local(k);
    let set_k = b.e(Expr::Assign { lhs: k_l, rhs: k_next });
    let wh = b.while_(wl, &[
        Stmt::Expr { expr: add_sum, semi: true },
        Stmt::Expr { expr: add_i, semi: true },
        Stmt::Expr { expr: set_k, semi: true },
    ]);
    stmts.push(Stmt::Expr { expr: wh, semi: true });

    // let S(z) = q else { return 0 };
    let (sz, z) = s_of(&mut b, "z");
    let q_e = b.local(q);
    let zero = b.int(0, None);
    let ret = b.e(Expr::Return { value: Some(zero) });
    let else_z = b.block(&[], Some(ret));
    stmts.push(Stmt::Let { pat: sz, ty: None, init: Some(q_e), else_: Some(else_z) });

    // let r: u32 = loop { let S(m) = n else { break 8 }; break m; };
    let lp = b.e(Expr::Missing);
    let (sm, m) = s_of(&mut b, "m");
    let n_e = b.local(n);
    let eight = b.int(8, None);
    let brk8 = b.e(Expr::Break { value: Some(eight), target: lp });
    let else_m = b.block(&[], Some(brk8));
    let me = b.local(m);
    let brk_m = b.e(Expr::Break { value: Some(me), target: lp });
    let body = b.block(
        &[Stmt::Let { pat: sm, ty: None, init: Some(n_e), else_: Some(else_m) }, Stmt::Expr { expr: brk_m, semi: true }],
        None,
    );
    b.set(lp, Expr::Loop { body });
    let r = b.bind("r");
    stmts.push(Stmt::Let { pat: r, ty: Some(u32_ty), init: Some(lp), else_: None });

    // c1 * 1_000_000 + c2 * 100_000 + c3 * 10_000 + sum * 1_000 + i * 100 + z + r
    let mut total = b.int(0, None);
    for (local, weight) in [(c1, 1_000_000), (c2, 100_000), (c3, 10_000), (sum, 1_000), (i, 100), (z, 1), (r, 1)] {
        let le = b.local(local);
        let kw = b.int(weight, None);
        let term = b.bin(BinOp::Mul, le, kw);
        total = b.bin(BinOp::Add, total, term);
    }
    let root = b.block(&stmts, Some(total));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    #[cfg(target_arch = "aarch64")]
    assert_eq!(run_native(&[(MAIN, &built)]) as u32, 340_760_448);
    let _ = built;
}

/// `if let` and `let`-`else` with a dereferenced slice scrutinee (`*s` where `s: &[u32]`): the
/// scrutinee is an unsized place (`V::Unsized`), never loaded by value.
/// ```text
/// fn main() -> u32 {
///     let arr2 = [3u32, 4u32];
///     let s2: &[u32] = &arr2;
///     let hit = if let [a, b] = *s2 { a * 10 + b } else { 1 };   // 34
///     let arr3 = [1u32, 2u32, 3u32];
///     let s3: &[u32] = &arr3;
///     let miss = if let [a, b] = *s3 { a } else { 7 };           // 7
///     let [p, q] = *s2 else { return 0 };                        // 3, 4
///     hit * 1000 + miss * 100 + p * 10 + q
/// }
/// ```
#[test]
fn if_let_and_let_else_destructure_a_dereferenced_slice() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U32);
    let mut b = BodyBuf::new();
    let u32_ty = b.types.intern(U32).expect("u32 interns");
    let slice_u32 = b.types.intern(PNode::Slice(u32_ty)).expect("[u32] interns");
    let ref_slice = b.types.intern(PNode::Ref(Mutbl::Not, slice_u32)).expect("&[u32] interns");

    // let arr2 = [3u32, 4u32]; let s2: &[u32] = &arr2;
    let arr2 = b.bind("arr2");
    let (v3, v4) = (b.int(3, Some(IntSuffix::Uint(UintTy::U32))), b.int(4, Some(IntSuffix::Uint(UintTy::U32))));
    let arr2_init = b.array(&[v3, v4]);
    let st0 = Stmt::Let { pat: arr2, ty: None, init: Some(arr2_init), else_: None };
    let s2 = b.bind("s2");
    let arr2_e = b.local(arr2);
    let s2_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: arr2_e });
    let st1 = Stmt::Let { pat: s2, ty: Some(ref_slice), init: Some(s2_init), else_: None };

    // let hit = if let [a, b] = *s2 { a * 10 + b } else { 1 };
    let (pa, pb) = (plain(&mut b, "a"), plain(&mut b, "b"));
    let ab_pats = b.pats_list(&[pa, pb]);
    let ab_slice = b.pat(Pat::Slice { elems: ab_pats, rest: None });
    let s2_e = b.local(s2);
    let deref_s2 = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: s2_e });
    let cond_hit = b.e(Expr::Let { pat: ab_slice, init: deref_s2 });
    let (ae, be, k10, k1) = (b.local(pa), b.local(pb), b.int(10, None), b.int(1, None));
    let a10 = b.bin(BinOp::Mul, ae, k10);
    let then_hit = b.bin(BinOp::Add, a10, be);
    let if_hit = if_else(&mut b, cond_hit, then_hit, k1);
    let hit = b.bind("hit");
    let st2 = Stmt::Let { pat: hit, ty: Some(u32_ty), init: Some(if_hit), else_: None };

    // let arr3 = [1u32, 2u32, 3u32]; let s3: &[u32] = &arr3;
    let arr3 = b.bind("arr3");
    let elems3 = [1, 2, 3].map(|n| b.int(n, Some(IntSuffix::Uint(UintTy::U32))));
    let arr3_init = b.array(&elems3);
    let st3 = Stmt::Let { pat: arr3, ty: None, init: Some(arr3_init), else_: None };
    let s3 = b.bind("s3");
    let arr3_e = b.local(arr3);
    let s3_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: arr3_e });
    let st4 = Stmt::Let { pat: s3, ty: Some(ref_slice), init: Some(s3_init), else_: None };

    // let miss = if let [a2, b2] = *s3 { a2 } else { 7 };
    let (pa2, pb2) = (plain(&mut b, "a2"), plain(&mut b, "b2"));
    let ab2_pats = b.pats_list(&[pa2, pb2]);
    let ab2_slice = b.pat(Pat::Slice { elems: ab2_pats, rest: None });
    let s3_e = b.local(s3);
    let deref_s3 = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: s3_e });
    let cond_miss = b.e(Expr::Let { pat: ab2_slice, init: deref_s3 });
    let (a2e, k7) = (b.local(pa2), b.int(7, None));
    let if_miss = if_else(&mut b, cond_miss, a2e, k7);
    let miss = b.bind("miss");
    let st5 = Stmt::Let { pat: miss, ty: Some(u32_ty), init: Some(if_miss), else_: None };

    // let [p, q] = *s2 else { return 0 };
    let (pp, pq) = (plain(&mut b, "p"), plain(&mut b, "q"));
    let pq_pats = b.pats_list(&[pp, pq]);
    let pq_slice = b.pat(Pat::Slice { elems: pq_pats, rest: None });
    let s2_e2 = b.local(s2);
    let deref_s2_2 = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: s2_e2 });
    let k0 = b.int(0, None);
    let ret0 = b.e(Expr::Return { value: Some(k0) });
    let else_blk = b.block(&[], Some(ret0));
    let st6 = Stmt::Let { pat: pq_slice, ty: None, init: Some(deref_s2_2), else_: Some(else_blk) };

    // hit * 1000 + miss * 100 + p * 10 + q
    let mut total = b.int(0, None);
    for (local, weight) in [(hit, 1000), (miss, 100), (pp, 10), (pq, 1)] {
        let le = b.local(local);
        let kw = b.int(weight, None);
        let term = b.bin(BinOp::Mul, le, kw);
        total = b.bin(BinOp::Add, total, term);
    }
    let root = b.block(&[st0, st1, st2, st3, st4, st5, st6], Some(total));
    let built = build(&w, &b, root).unwrap_or_else(|e| panic!("{e}"));
    #[cfg(target_arch = "aarch64")]
    assert_eq!(run_native(&[(MAIN, &built)]) as u32, 34_734);
    let _ = built;
}
