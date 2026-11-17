//! `_ = e` built as `let _ = e;` (walk/discard.rs `discard`): built from a body the checker
//! types, run natively, the drops compared. `G` appends its `x` when dropped.
//!
//! Found on the hello-world path: std's `read`, `disconnect` and `unlock_with` hold `_ = e`
//! statements, which the build refused with "`_` (`_ = e` is not built yet)".

use super::*;

/// Builds `main` (checked against `w`), runs it natively with the glue of `G` and of `(G, G)`,
/// and returns what was dropped, in order (empty off aarch64, where nothing runs).
fn drops_of(w: &TestWorld, b: &BodyBuf, root: ExprId, g: PNode) -> Result<Vec<i32, Global>, String> {
    let mut ground = PortableTys::new_in(Alloc::global());
    let main = build_instance(w, b, MAIN, root, &[], &mut ground)?;
    #[cfg_attr(not(target_arch = "aarch64"), allow(unused_mut))]
    let mut seen: Vec<i32, Global> = Vec::new_in(Global);
    #[cfg(target_arch = "aarch64")]
    {
        let mut gb = BodyBuf::new();
        let p = gb.bind("to_drop");
        gb.params.push(p);
        let groot = gb.block(&[], None);
        let view = gb.view(DROP_IN_PLACE, groot);
        let mut cx = InferCtx::new_in(Alloc::global());
        let checked = match blaze_typeck::check(&view, w, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
            Ok(Outcome::Done(c)) => c,
            other => panic!("the check did not finish: {:?}", other.err()),
        };
        let sig = w.sig(DROP_IN_PLACE).expect("a signature");
        let ty = ground.intern(g).expect("intern");
        let args = ground.intern_list(&[ty]).expect("intern");
        // `t: (G, G)` dropped whole goes through `drop_in_place::<(G, G)>`, whose glue calls
        // `G`'s; dropped by its field bits it calls `G`'s directly.
        let elems = ground.intern_list(&[ty, ty]).expect("intern");
        let tuple_ty = ground.intern(PNode::Tuple(elems)).expect("intern");
        let tuple_args = ground.intern_list(&[tuple_ty]).expect("intern");
        let mut subst = Subst { args: &[ty], ground: &mut ground, scratch: Alloc::global() };
        let glue = crate::build_drop_glue(&view, &checked, &sig, &mut subst, &TestAdts { world: w, body: &gb }, Global)
            .unwrap_or_else(|e| panic!("{e}"));
        let mut subst = Subst { args: &[tuple_ty], ground: &mut ground, scratch: Alloc::global() };
        let tuple_glue = crate::build_drop_glue(&view, &checked, &sig, &mut subst, &TestAdts { world: w, body: &gb }, Global)
            .unwrap_or_else(|e| panic!("{e}"));
        DROPPED.with(|d| d.borrow_mut().clear());
        run_native_with(
            &[(G_DROP, host_g_drop as usize as u64)],
            &[(DROP_IN_PLACE, Some(args), &glue), (DROP_IN_PLACE, Some(tuple_args), &tuple_glue), (MAIN, None, &main)],
        );
        seen = DROPPED.with(|d| d.borrow().clone());
    }
    let _ = (&main, g);
    Ok(seen)
}

/// `_ = e;` as a statement.
fn underscore_assign(b: &mut BodyBuf, rhs: ExprId) -> Stmt {
    let lhs = b.e(Expr::Underscore);
    let expr = b.e(Expr::Assign { lhs, rhs });
    Stmt::Expr { expr, semi: true }
}

/// `_ = e` is `let _ = e;`. The Reference, "Destructuring assignments": a destructuring
/// assignment "is desugared to a pattern match followed by sequential assignment" (`(a, b) =
/// (3, 4);` is `{ let (_a, _b) = (3, 4); a = _a; b = _b; }`), so `_ = e` is `{ let _ = e; }`;
/// "Wildcard pattern": "Unlike identifier patterns, it does not copy, move or borrow the value
/// it matches"; "Temporary scopes": a statement is a temporary scope, so a value matched by `_`
/// drops at its statement's end; "Destructors": a scope's variables drop "in reverse order of
/// declaration", a tuple's fields in order.
/// ```text
/// fn main() {
///     let a = G { x: 1 };
///     _ = a;                          // a place: not moved, nothing dropped
///     _ = G { x: 2 };                 // a value: dropped here: 2
///     let t = (G { x: 3 }, G { x: 4 });
///     _ = t.1;                        // a field place: not moved
///     let k = G { x: 9 };
/// }                                   // k, t, a: 9, 3, 4, 1
/// ```
/// Drops `2, 9, 3, 4, 1`. PLANTED BUGS this catches: `_ = a` built as a move (`Eval` then
/// `DropValue`, as an expression statement's value) gives `1, 2, 9, 3, 4`; `_ = t.1` built as a
/// move gives `2, 4, 9, 3, 1`; `_ = G { x: 2 }` left undropped gives no `2`. Before
/// walk/discard.rs the build failed with "`_` (`_ = e` is not built yet)". Prints nothing;
/// passes as "test tests::discard::underscore_assign_is_let_underscore ... ok".
#[test]
fn underscore_assign_is_let_underscore() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], UNIT);
    let g = g_world(&mut w);
    let mut b = BodyBuf::new();
    let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
    let a = b.bind("a");
    let g1 = g_lit(&mut b, 1);
    stmts.push(Stmt::Let { pat: a, ty: None, init: Some(g1), else_: None });
    let a_place = b.local(a);
    stmts.push(underscore_assign(&mut b, a_place));
    let g2 = g_lit(&mut b, 2);
    stmts.push(underscore_assign(&mut b, g2));
    let t = b.bind("t");
    let (g3, g4) = (g_lit(&mut b, 3), g_lit(&mut b, 4));
    let pair = b.tuple(&[g3, g4]);
    stmts.push(Stmt::Let { pat: t, ty: None, init: Some(pair), else_: None });
    let t_place = b.local(t);
    let t1 = b.tfield(t_place, 1);
    stmts.push(underscore_assign(&mut b, t1));
    let k = b.bind("k");
    let g9 = g_lit(&mut b, 9);
    stmts.push(Stmt::Let { pat: k, ty: None, init: Some(g9), else_: None });
    let root = b.block(&stmts, None);
    let seen = drops_of(&w, &b, root, g).unwrap_or_else(|e| panic!("{e}"));
    #[cfg(target_arch = "aarch64")]
    assert_eq!(seen.as_slice(), &[2, 9, 3, 4, 1]);
    let _ = seen;
}
