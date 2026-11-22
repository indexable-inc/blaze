//! Borrowed temporaries whose type needs drop (walk/temps.rs): dropped at the end of their
//! temporary scope, in reverse order of creation; an extended one at its block's end, with a
//! drop flag when made under a branch.

use super::*;

/// `peek(&G) -> i32` on the host: reads `x`.
extern "C" fn host_peek(g: *const i32) -> i32 {
    // SAFETY: the built code passes the address of a live `G`, whose `x: i32` is at 0.
    unsafe { *g }
}

const PEEK: ItemKey = ItemKey(30);

fn peek_of(b: &mut BodyBuf, v: u128) -> ExprId {
    let g = g_lit(b, v);
    let r = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: g });
    b.call("peek", &[r])
}

/// The Reference, "Temporary scopes": "the temporary scope of an expression is the smallest
/// scope that contains the expression and is one of: ... A statement"; "Drop scopes": dropped
/// "in reverse order of declaration"; "Temporary lifetime extension": "The temporary scopes for
/// expressions in `let` statements are sometimes extended to the scope of the block".
/// ```text
/// fn main() {
///     let k = peek(&G { x: 1 });              // G(1) dropped at this statement's end
///     let a = G { x: 2 };                     // dropped at main's end
///     peek(&G { x: 3 }) + peek(&G { x: 4 });  // dropped 4 then 3, at this statement's end
///     let b = G { x: 5 };                     // dropped first at main's end: 5, then 2
/// }
/// ```
/// Drops: 1, 4, 3, 5, 2. A temporary held to its block's end, or dropped in creation order,
/// gives another list.
#[test]
fn borrowed_temporaries_drop_at_their_statements_end_in_reverse() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], UNIT);
    let g = g_world(&mut w);
    let ref_g = w.ref_(Mutbl::Not, g);
    w.func(30, "peek", &[ref_g], I32);
    let mut b = BodyBuf::new();
    let (k, a, bb) = (b.bind("k"), b.bind("a"), b.bind("b"));
    let k_init = peek_of(&mut b, 1);
    let a_init = g_lit(&mut b, 2);
    let (p3, p4) = (peek_of(&mut b, 3), peek_of(&mut b, 4));
    let sum = b.bin(BinOp::Add, p3, p4);
    let b_init = g_lit(&mut b, 5);
    let root = b.block(
        &[
            Stmt::Let { pat: k, ty: None, init: Some(k_init), else_: None },
            Stmt::Let { pat: a, ty: None, init: Some(a_init), else_: None },
            Stmt::Expr { expr: sum, semi: true },
            Stmt::Let { pat: bb, ty: None, init: Some(b_init), else_: None },
        ],
        None,
    );
    let mut ground = PortableTys::new_in(Alloc::global());
    let main = build_instance(&w, &b, MAIN, root, &[], &mut ground).unwrap_or_else(|e| panic!("{e}"));

    // An extended temporary (`let r = &G { x: 6 };`) is built now: see the next test.

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
        run_native_with(
            &[(G_DROP, host_g_drop as usize as u64), (PEEK, host_peek as usize as u64)],
            &[(DROP_IN_PLACE, Some(args), &glue), (MAIN, None, &main)],
        );
        let seen = DROPPED.with(|d| d.borrow().clone());
        assert_eq!(seen.as_slice(), &[1, 4, 3, 5, 2]);
    }
    let _ = (main, PEEK);
}

/// `&G { x: v }`.
fn ref_g(b: &mut BodyBuf, v: u128) -> ExprId {
    let g = g_lit(b, v);
    b.e(Expr::Ref { mutbl: Mutbl::Not, operand: g })
}

/// `l == v`.
fn eq_int(b: &mut BodyBuf, l: ExprId, v: u128) -> ExprId {
    let n = b.int(v, None);
    b.bin(BinOp::Eq, l, n)
}

/// Every temporary scope and lifetime extension at once (edition 2024, the view's), each
/// position of the expected list from the Reference ("Destructors"):
/// ```text
/// fn main() {
///     let r = &G { x: 1 };          // "The operand of an extending borrow expression has its
///                                   // temporary scope extended": dropped at main's end
///     let t = (&G { x: 2 }, 0);     // "The operand(s) of an extending ... tuple expression"
///     let f = if peek(&G { x: 3 }) == 0 { &G { x: 4 } } else { &G { x: 5 } };
///         // 3: "The non-pattern matching condition expression of an `if`": dropped before a
///         // branch runs; 5: "The final expression of an extending `if` expression's
///         // consequent, `else if`, or `else` block": extended, its drop flag set; 4 never made,
///         // its flag clear: never dropped
///     let k = peek(&G { x: 6 }) == 0 || peek(&G { x: 7 }) == 7;
///         // "Each operand of a lazy boolean expression": 6 before the right side runs, then 7
///     let m = match peek(&G { x: 8 }) { 8 if peek(&G { x: 9 }) == 0 => 0, _ => peek(&G { x: 10 }) };
///         // 9: "a non-pattern-matching match guard condition operand"; 10: "The body
///         // expression for a match arm"; 8: "The scrutinee of a `match` expression is not a
///         // temporary scope": at the statement's end
///     let n = { let a = G { x: 11 }; peek(&G { x: 12 }) };
///         // 2024: "temporaries of tail expressions of blocks are dropped immediately after the
///         // tail expression is evaluated": 12 before the block's local 11
///     let i = 0;
///     while peek(&G { x: 13 }) + i < 15 { peek(&G { x: 14 }); i += 1; }
///         // the `while` condition's scope, then the body's statement, per iteration: 13, 14,
///         // 13, 14, 13
/// }   // "in reverse order of declaration": the hidden locals before f, t, r: 5, 2, 1
/// ```
/// Drops: 3, 6, 7, 9, 10, 8, 12, 11, 13, 14, 13, 14, 13, 5, 2, 1. A wrong extension drops 1, 2
/// or 5 at their statements' ends; a missing flag drops the never-made 4 (whatever its slot
/// holds) beside 5; a late drop puts 3 after 5, 7 before 6, 8 before 9 or 11 before 12, or
/// drops 13 once.
#[test]
fn temporaries_drop_where_their_scope_ends_extended_ones_at_the_block_end_flagged_under_a_branch() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], UNIT);
    let g = g_world(&mut w);
    let ref_g_ty = w.ref_(Mutbl::Not, g);
    w.func(30, "peek", &[ref_g_ty], I32);
    let mut b = BodyBuf::new();
    let (r, t, f, k) = (b.bind("r"), b.bind("t"), b.bind("f"), b.bind("k"));
    let (m, n, a, i) = (b.bind("m"), b.bind("n"), b.bind("a"), b.bind("i"));
    // let r = &G { x: 1 };
    let r_init = ref_g(&mut b, 1);
    // let t = (&G { x: 2 }, 0);
    let r2 = ref_g(&mut b, 2);
    let zero = b.int(0, None);
    let t_init = b.tuple(&[r2, zero]);
    // let f = if peek(&G { x: 3 }) == 0 { &G { x: 4 } } else { &G { x: 5 } };
    let p3 = peek_of(&mut b, 3);
    let c3 = eq_int(&mut b, p3, 0);
    let r4 = ref_g(&mut b, 4);
    let then4 = b.block(&[], Some(r4));
    let r5 = ref_g(&mut b, 5);
    let else5 = b.block(&[], Some(r5));
    let f_init = b.e(Expr::If { cond: c3, then: then4, else_: Some(else5) });
    // let k = peek(&G { x: 6 }) == 0 || peek(&G { x: 7 }) == 7;
    let p6 = peek_of(&mut b, 6);
    let l6 = eq_int(&mut b, p6, 0);
    let p7 = peek_of(&mut b, 7);
    let r7 = eq_int(&mut b, p7, 7);
    let k_init = b.bin(BinOp::Or, l6, r7);
    // let m = match peek(&G { x: 8 }) { 8 if peek(&G { x: 9 }) == 0 => 0, _ => peek(&G { x: 10 }) };
    let p8 = peek_of(&mut b, 8);
    let pat8 = b.pat_int(8);
    let p9 = peek_of(&mut b, 9);
    let g9 = eq_int(&mut b, p9, 0);
    let arm0 = b.int(0, None);
    let wild = b.pat(Pat::Wild);
    let p10 = peek_of(&mut b, 10);
    let start = b.arms.len() as u32;
    b.arms.push(Arm { pat: pat8, guard: Some(g9), body: arm0 });
    b.arms.push(Arm { pat: wild, guard: None, body: p10 });
    let m_init = b.e(Expr::Match { scrutinee: p8, arms: List { start, len: 2 } });
    // let n = { let a = G { x: 11 }; peek(&G { x: 12 }) };
    let a_init = g_lit(&mut b, 11);
    let p12 = peek_of(&mut b, 12);
    let n_init = b.block(&[Stmt::Let { pat: a, ty: None, init: Some(a_init), else_: None }], Some(p12));
    // let i = 0; while peek(&G { x: 13 }) + i < 15 { peek(&G { x: 14 }); i += 1; }
    let i_init = b.int(0, None);
    let p13 = peek_of(&mut b, 13);
    let i_e = b.local(i);
    let sum = b.bin(BinOp::Add, p13, i_e);
    let fifteen = b.int(15, None);
    let cond = b.bin(BinOp::Lt, sum, fifteen);
    let p14 = peek_of(&mut b, 14);
    let i_e = b.local(i);
    let one = b.int(1, None);
    let inc = b.e(Expr::AssignOp { op: BinOp::Add, lhs: i_e, rhs: one });
    let lp = b.while_(cond, &[Stmt::Expr { expr: p14, semi: true }, Stmt::Expr { expr: inc, semi: true }]);
    let let_ = |pat: PatId, init: ExprId| Stmt::Let { pat, ty: None, init: Some(init), else_: None };
    let root = b.block(
        &[
            let_(r, r_init),
            let_(t, t_init),
            let_(f, f_init),
            let_(k, k_init),
            let_(m, m_init),
            let_(n, n_init),
            let_(i, i_init),
            Stmt::Expr { expr: lp, semi: true },
        ],
        None,
    );
    let mut ground = PortableTys::new_in(Alloc::global());
    let main = build_instance(&w, &b, MAIN, root, &[], &mut ground).unwrap_or_else(|e| panic!("{e}"));

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
        run_native_with(
            &[(G_DROP, host_g_drop as usize as u64), (PEEK, host_peek as usize as u64)],
            &[(DROP_IN_PLACE, Some(args), &glue), (MAIN, None, &main)],
        );
        let seen = DROPPED.with(|d| d.borrow().clone());
        assert_eq!(seen.as_slice(), &[3, 6, 7, 9, 10, 8, 12, 11, 13, 14, 13, 14, 13, 5, 2, 1]);
    }
    let _ = main;
}
