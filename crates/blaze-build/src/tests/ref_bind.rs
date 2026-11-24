//! `ref` bindings over a dereference's place, in a `match` arm and in a `let`
//! (walk/matches/ref_bind.rs; the match side matches.rs `bind_with` on the place walk.rs
//! `Expr::Match` pushes): built from a body the checker types, run natively with `G`'s glue,
//! the result and the drops compared.

use super::*;

/// `ref name`.
fn ref_bind(b: &mut BodyBuf, name: &str) -> PatId {
    b.pat(Pat::Bind { ann: BindAnn { by_ref: Some(Mutbl::Not), mutable: false }, name: Name::of(name), sub: None, ty: None })
}

/// `ref mut name`.
fn ref_mut_bind(b: &mut BodyBuf, name: &str) -> PatId {
    b.pat(Pat::Bind { ann: BindAnn { by_ref: Some(Mutbl::Mut), mutable: false }, name: Name::of(name), sub: None, ty: None })
}

fn let_stmt(pat: PatId, init: ExprId) -> Stmt {
    Stmt::Let { pat, ty: None, init: Some(init), else_: None }
}

/// `match scrutinee { A(ref name) => name.x, B => 0 }`.
fn match_a_ref(b: &mut BodyBuf, scrutinee: ExprId, name: &str) -> ExprId {
    let s = ref_bind(b, name);
    let a_path = b.path("A");
    let subs = b.pats_list(&[s]);
    let a_pat = b.pat(Pat::TupleStruct { path: a_path, elems: subs, rest: None });
    let s_e = b.local(s);
    let s_x = b.field(s_e, "x");
    let b_path = b.path("B");
    let b_pat = b.pat(Pat::Path(b_path));
    let zero = b.int(0, None);
    let start = b.arms.len() as u32;
    b.arms.push(Arm { pat: a_pat, guard: None, body: s_x });
    b.arms.push(Arm { pat: b_pat, guard: None, body: zero });
    b.e(Expr::Match { scrutinee, arms: List { start, len: 2 } })
}

/// Runs `main` with `G`'s drop glue for each type in `glue_tys`, asserting `expected_dropped`
/// and returning `main`'s return word.
fn run_with_g_glues(
    w: &TestWorld,
    ground: &mut PortableTys,
    main: &Built<Global>,
    glue_tys: &[PNode],
    expected_dropped: &[i32],
) -> u64 {
    let heap = blaze_own::testing::heap().alloc();
    let mut gb = BodyBuf::new();
    let to_drop = gb.bind("to_drop");
    gb.params.push(to_drop);
    let groot = gb.block(&[], None);
    let view = gb.view(DROP_IN_PLACE, groot);
    let mut cx = InferCtx::new_in(heap);
    let checked = match blaze_typeck::check(&view, w, &mut cx, &Needs::new_in(heap), heap) {
        Ok(Outcome::Done(c)) => c,
        other => panic!("the check did not finish: {:?}", other.err()),
    };
    let sig = w.sig(DROP_IN_PLACE).expect("a signature");
    let mut built_glues = blaze_own::FixedVec::<_, 4>::new();
    for &t in glue_tys {
        let ty = ground.intern(t).expect("intern");
        let args = ground.intern_list(&[ty]).expect("intern");
        let mut subst = Subst { args: &[ty], ground, scratch: heap };
        let built = crate::build_drop_glue(&view, &checked, &sig, &mut subst, &TestAdts { world: w, body: &gb }, Global)
            .unwrap_or_else(|e| panic!("{e}"));
        let _ = built_glues.push((args, built));
    }
    let mut funcs = blaze_own::FixedVec::<_, 5>::new();
    for (args, built) in built_glues.as_slice() {
        let _ = funcs.push((DROP_IN_PLACE, Some(*args), built));
    }
    let _ = funcs.push((MAIN, None, main));
    DROPPED.with(|d| d.borrow_mut().clear());
    let ret = run_native_with(&[(G_DROP, host_g_drop as usize as u64)], funcs.as_slice());
    DROPPED.with(|d| assert_eq!(d.borrow().as_slice(), expected_dropped));
    ret
}

/// A dereference in a place expression context is a place, and `ref` binds a reference to it.
/// The Reference, "Place Expressions and Value Expressions": "The following contexts are place
/// expression contexts: ... The initializer of a `let` statement. The scrutinee of an `if
/// let`, `match`, or `while let` expression"; "Identifier patterns": "`ref` ... binds to a
/// reference to the matched place"; "Destructors": a scope's variables drop "in reverse order
/// of declaration", a tuple's fields in order. The shape of alloc's `Cow` `fmt`
/// (`match *self { Borrowed(ref b) => .., Owned(ref o) => .. }`), over an enum that needs drop:
/// ```text
/// enum E { A(G), B }
/// fn main() -> i32 {
///     let e = A(G { x: 4 });
///     let r = &e;
///     let n = match *r { A(ref s) => s.x, B => 0 };   // 4: `*r` matched in place
///     let t = (G { x: 5 }, 6);
///     let p = &t;
///     let (ref a, k) = *p;                            // a = &t.0 (5), k = 6: nothing moves
///     let ref c = e;                                  // c = &e: e is not moved
///     let m = match *c { A(ref h) => h.x, B => 0 };   // 4
///     n * 1000 + a.x * 100 + k * 10 + m               // 4564
/// }   // t (5), then e (4)
/// ```
/// Expected: returns 4564 and drops `5, 4`; `cargo test` prints
/// `test tests::ref_bind::match_on_deref_with_ref_binding ... ok`. Without walk/matches/
/// ref_bind.rs the build fails at `let (ref a, k) = *p;` ("a move out of a dereference whose
/// type needs drop": `*p` loaded as a value) and at `let ref c = e;` ("a `ref` binding or a
/// binding with a subpattern"). PLANTED BUGS this catches: `*p` or `e` copied to a temporary
/// that is dropped gives an extra `5` or `4`; `e` moved into `c`'s initializer drops nothing
/// for `e` (`5` alone); a wrong place for `a` gives another hundreds digit.
#[test]
fn match_on_deref_with_ref_binding() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], I32);
    let g = g_world(&mut w);
    w.enum_(40, &[("A", CtorKind::Tuple, &[("0", g)]), ("B", CtorKind::Unit, &[])]);
    let e_ty = PNode::Adt(ItemKey(40), List::EMPTY);

    let mut b = BodyBuf::new();
    let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
    // let e = A(G { x: 4 }); let r = &e;
    let e = b.bind("e");
    let g4 = g_lit(&mut b, 4);
    let e_init = b.call("A", &[g4]);
    stmts.push(let_stmt(e, e_init));
    let r = b.bind("r");
    let e_use = b.local(e);
    let r_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: e_use });
    stmts.push(let_stmt(r, r_init));
    // let n = match *r { A(ref s) => s.x, B => 0 };
    let r_use = b.local(r);
    let star_r = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: r_use });
    let m1 = match_a_ref(&mut b, star_r, "s");
    let n = b.bind("n");
    stmts.push(let_stmt(n, m1));
    // let t = (G { x: 5 }, 6); let p = &t;
    let t = b.bind("t");
    let g5 = g_lit(&mut b, 5);
    let six = b.int(6, None);
    let t_init = b.tuple(&[g5, six]);
    stmts.push(let_stmt(t, t_init));
    let p = b.bind("p");
    let t_use = b.local(t);
    let p_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: t_use });
    stmts.push(let_stmt(p, p_init));
    // let (ref a, k) = *p;
    let a = ref_bind(&mut b, "a");
    let k = b.bind("k");
    let ak = b.pats_list(&[a, k]);
    let ak_pat = b.pat(Pat::Tuple { elems: ak, rest: None });
    let p_use = b.local(p);
    let star_p = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: p_use });
    stmts.push(let_stmt(ak_pat, star_p));
    // let ref c = e;
    let c = ref_bind(&mut b, "c");
    let e_use2 = b.local(e);
    stmts.push(let_stmt(c, e_use2));
    // let m = match *c { A(ref h) => h.x, B => 0 };
    let c_use = b.local(c);
    let star_c = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: c_use });
    let m2 = match_a_ref(&mut b, star_c, "h");
    let m = b.bind("m");
    stmts.push(let_stmt(m, m2));
    // n * 1000 + a.x * 100 + k * 10 + m
    let (n_e, k_e, m_e, a_e) = (b.local(n), b.local(k), b.local(m), b.local(a));
    let a_x = b.field(a_e, "x");
    let (thousand, hundred, ten) = (b.int(1000, None), b.int(100, None), b.int(10, None));
    let n1000 = b.bin(BinOp::Mul, n_e, thousand);
    let a100 = b.bin(BinOp::Mul, a_x, hundred);
    let k10 = b.bin(BinOp::Mul, k_e, ten);
    let s1 = b.bin(BinOp::Add, n1000, a100);
    let s2 = b.bin(BinOp::Add, s1, k10);
    let sum = b.bin(BinOp::Add, s2, m_e);
    let root = b.block(&stmts, Some(sum));

    let mut ground = PortableTys::new_in(blaze_own::testing::heap().alloc());
    let main = build_instance(&w, &b, MAIN, root, &[], &mut ground).unwrap_or_else(|e| panic!("{e}"));
    let g_ty = ground.intern(g).expect("intern");
    let i32_ty = ground.intern(I32).expect("intern");
    let elems = ground.intern_list(&[g_ty, i32_ty]).expect("intern");
    let pair = PNode::Tuple(elems);
    let ret = run_with_g_glues(&w, &mut ground, &main, &[g, e_ty, pair], &[5, 4]);
    assert_eq!(ret as u32, 4564);
}

/// `let ref` and `let ref mut` over VALUE initializers (block expression as in Linux `std`'s
/// `weak!` macro `let ref gettid: ExternWeak<_> = { ... };`, scalar literal, drop-needing
/// struct literal whose temporary extends to the enclosing block, and a tuple with a `ref`
/// binding alongside a `Copy` binding), plus a `match` arm with a `ref` binding:
/// ```text
/// fn main() -> i32 {
///     let ref b = { 3i32 };                // block value init -> 3
///     let ref mut m = 1i32;                // scalar value init, mutated through ref mut
///     *m = 4;                              // *m -> 4
///     let ref g = G { x: 5 };              // extended drop-needing temporary
///     let (ref a, k) = (G { x: 6 }, 7);    // extended drop-needing tuple temporary
///     let r = match *g { ref h => h.x };   // match ref binding -> 5
///     *b * 10000 + *m * 1000 + r * 100 + a.x * 10 + k   // 34567
/// }   // drops (G { x: 6 }, 7) -> 6, then G { x: 5 } -> 5
/// ```
#[test]
fn let_ref_and_ref_mut_over_value_initializers_extend_temporaries() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], I32);
    let g = g_world(&mut w);

    let mut b = BodyBuf::new();

    // let ref b = { 3i32 };
    let b_pat = ref_bind(&mut b, "b");
    let three = b.int(3, Some(blaze_typeck::IntSuffix::Int(IntTy::I32)));
    let blk = b.block(&[], Some(three));
    let s_b = let_stmt(b_pat, blk);

    // let ref mut m = 1i32; *m = 4;
    let m_pat = ref_mut_bind(&mut b, "m");
    let one = b.int(1, Some(blaze_typeck::IntSuffix::Int(IntTy::I32)));
    let s_m = let_stmt(m_pat, one);
    let m_use1 = b.local(m_pat);
    let star_m1 = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: m_use1 });
    let four = b.int(4, Some(blaze_typeck::IntSuffix::Int(IntTy::I32)));
    let assign = b.e(Expr::Assign { lhs: star_m1, rhs: four });
    let s_assign = Stmt::Expr { expr: assign, semi: true };

    // let ref g = G { x: 5 };
    let g_pat = ref_bind(&mut b, "g");
    let g5 = g_lit(&mut b, 5);
    let s_g = let_stmt(g_pat, g5);

    // let (ref a, k) = (G { x: 6 }, 7);
    let a_pat = ref_bind(&mut b, "a");
    let k_pat = b.bind("k");
    let ak = b.pats_list(&[a_pat, k_pat]);
    let ak_pat = b.pat(Pat::Tuple { elems: ak, rest: None });
    let g6 = g_lit(&mut b, 6);
    let seven = b.int(7, Some(blaze_typeck::IntSuffix::Int(IntTy::I32)));
    let tup = b.tuple(&[g6, seven]);
    let s_ak = let_stmt(ak_pat, tup);

    // let r = match *g { ref h => h.x };
    let g_use = b.local(g_pat);
    let star_g = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: g_use });
    let h_pat = ref_bind(&mut b, "h");
    let h_use = b.local(h_pat);
    let h_x = b.field(h_use, "x");
    let start = u32::try_from(b.arms.len()).expect("arms fit u32");
    b.arms.push(Arm { pat: h_pat, guard: None, body: h_x });
    let m_expr = b.e(Expr::Match { scrutinee: star_g, arms: List { start, len: 1 } });
    let r_pat = b.bind("r");
    let s_r = let_stmt(r_pat, m_expr);

    // *b * 10000 + *m * 1000 + r * 100 + a.x * 10 + k
    let b_use = b.local(b_pat);
    let star_b = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: b_use });
    let m_use2 = b.local(m_pat);
    let star_m2 = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: m_use2 });
    let r_use = b.local(r_pat);
    let a_use = b.local(a_pat);
    let a_x = b.field(a_use, "x");
    let k_use = b.local(k_pat);
    let (c10000, c1000, c100, c10) = (b.int(10000, None), b.int(1000, None), b.int(100, None), b.int(10, None));
    let t1 = b.bin(BinOp::Mul, star_b, c10000);
    let t2 = b.bin(BinOp::Mul, star_m2, c1000);
    let t3 = b.bin(BinOp::Mul, r_use, c100);
    let t4 = b.bin(BinOp::Mul, a_x, c10);
    let s1 = b.bin(BinOp::Add, t1, t2);
    let s2 = b.bin(BinOp::Add, s1, t3);
    let s3 = b.bin(BinOp::Add, s2, t4);
    let sum = b.bin(BinOp::Add, s3, k_use);
    let root = b.block(&[s_b, s_m, s_assign, s_g, s_ak, s_r], Some(sum));

    let mut ground = PortableTys::new_in(blaze_own::testing::heap().alloc());
    let main = build_instance(&w, &b, MAIN, root, &[], &mut ground).unwrap_or_else(|e| panic!("{e}"));
    let g_ty = ground.intern(g).expect("intern");
    let i32_ty = ground.intern(I32).expect("intern");
    let elems = ground.intern_list(&[g_ty, i32_ty]).expect("intern");
    let pair = PNode::Tuple(elems);
    let ret = run_with_g_glues(&w, &mut ground, &main, &[g, pair], &[6, 5]);
    assert_eq!(ret, 34567);
}
