//! Moves out of a local's field by a place expression and assignments over a moved field
//! (walk/moves.rs `move_field`, `assign_over`): built from a body the checker types, run
//! natively, the drops compared.

use super::*;

/// `let a = t.0;` moves a field out of a local; assigning to a moved field does not drop it;
/// a field moved under one branch of an `if` is dropped at its next assignment only when it
/// was not moved (its bit, tested at run time); every assigned field is dropped at the scope's
/// end. `G` appends its `x` when dropped. The Reference: [expr.move.deinitialization] "After
/// moving out of a place expression that evaluates to a local variable, the location is
/// deinitialized and cannot be read from again until it is reinitialized";
/// [expr.assign.drop-target] "It then has the effect of first dropping the value at the
/// assigned place, unless the place is an uninitialized local variable or an uninitialized
/// field of a local variable"; "Destructors": "If a variable has been partially initialized,
/// only its initialized fields are dropped", "The fields of a tuple are dropped in order", and
/// a scope's variables drop "in reverse order of declaration".
/// ```text
/// fn main() {                                    // sel = 1                 sel = 0
///     let t = (G { x: 1 }, G { x: 2 }, G { x: 3 });
///     let c = G { x: 8 };
///     let a = t.0;            // t.0 moved out
///     t.0 = G { x: 4 };       // t.0 uninitialized: no drop
///     if sel == 1 { c = t.1; }// c's old value:     8                   (not run)
///     t.1 = G { x: 5 };       // t.1's old value:   moved: no drop      2
///     let k = G { x: 9 };
/// }   // k, a, c, then t field by field:  9, 1, 2, 4, 5, 3          9, 1, 8, 4, 5, 3
/// ```
/// sel = 1: `8, 9, 1, 2, 4, 5, 3`; sel = 0: `2, 9, 1, 8, 4, 5, 3`. Dropping the moved `t.0`
/// at its assignment (a double drop) adds a `1`; the assignment's bit not set again (a leak of
/// the new value) loses the `4` or the `5`; the old `t.1` dropped by `sel = 1`'s assignment
/// adds a second `2`; not dropped by `sel = 0`'s loses the first `2`. Prints nothing; passes as
/// "test tests::moves::a_field_moved_out_then_assigned_drops_by_its_bit ... ok".
#[test]
fn a_field_moved_out_then_assigned_drops_by_its_bit() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], UNIT);
    let g = g_world(&mut w);
    let cases: [(u128, &[i32]); 2] = [(1, &[8, 9, 1, 2, 4, 5, 3]), (0, &[2, 9, 1, 8, 4, 5, 3])];
    for (sel, want) in cases {
        let mut b = BodyBuf::new();
        let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
        // let t = (G1, G2, G3);
        let (g1, g2, g3) = (g_lit(&mut b, 1), g_lit(&mut b, 2), g_lit(&mut b, 3));
        let value = b.tuple(&[g1, g2, g3]);
        let t = b.bind("t");
        stmts.push(Stmt::Let { pat: t, ty: None, init: Some(value), else_: None });
        // let c = G8;
        let c = b.bind("c");
        let g8 = g_lit(&mut b, 8);
        stmts.push(Stmt::Let { pat: c, ty: None, init: Some(g8), else_: None });
        // let a = t.0;
        let a = b.bind("a");
        let t_a = b.local(t);
        let t0 = b.tfield(t_a, 0);
        stmts.push(Stmt::Let { pat: a, ty: None, init: Some(t0), else_: None });
        // t.0 = G4;
        let t_b = b.local(t);
        let t0_l = b.tfield(t_b, 0);
        let g4 = g_lit(&mut b, 4);
        let set0 = b.e(Expr::Assign { lhs: t0_l, rhs: g4 });
        stmts.push(Stmt::Expr { expr: set0, semi: true });
        // if sel == 1 { c = t.1; }
        let s = b.int(sel, None);
        let one = b.int(1, None);
        let cond = b.bin(BinOp::Eq, s, one);
        let c_l = b.local(c);
        let t_c = b.local(t);
        let t1 = b.tfield(t_c, 1);
        let move1 = b.e(Expr::Assign { lhs: c_l, rhs: t1 });
        let then = b.block(&[Stmt::Expr { expr: move1, semi: true }], None);
        let iff = b.e(Expr::If { cond, then, else_: None });
        stmts.push(Stmt::Expr { expr: iff, semi: true });
        // t.1 = G5;
        let t_d = b.local(t);
        let t1_l = b.tfield(t_d, 1);
        let g5 = g_lit(&mut b, 5);
        let set1 = b.e(Expr::Assign { lhs: t1_l, rhs: g5 });
        stmts.push(Stmt::Expr { expr: set1, semi: true });
        // let k = G9;
        let k = b.bind("k");
        let g9 = g_lit(&mut b, 9);
        stmts.push(Stmt::Let { pat: k, ty: None, init: Some(g9), else_: None });
        let root = b.block(&stmts, None);
        let mut ground = PortableTys::new_in(Alloc::global());
        let main = build_instance(&w, &b, MAIN, root, &[], &mut ground).unwrap_or_else(|e| panic!("sel={sel}: {e}"));
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
            assert_eq!(seen.as_slice(), want, "sel={sel}");
        }
        let _ = (main, want, g);
    }
}
