//! `super let` (walk/super_let.rs, async plan P11): a binding that outlives its block, as the
//! library's `pin!` needs.

use super::*;

/// `mark(&G) -> i32` on the host: logs `100 + x` at the moment the built code uses the value.
#[cfg(target_arch = "aarch64")]
extern "C" fn host_mark(g: *const i32) -> i32 {
    // SAFETY: the built code passes the address of a `G` in a slot of `main`'s frame, which
    // lives until `main` returns (with the piece the `G` is live; without it the slot holds a
    // dropped one, whose `x` the test never needs: it reads the order of the log); `x: i32` is
    // at offset 0.
    let x = unsafe { *g };
    DROPPED.with(|d| d.borrow_mut().push(100 + x));
    x
}

const MARK: ItemKey = ItemKey(31);

/// `&local`.
fn ref_local(b: &mut BodyBuf, p: PatId) -> ExprId {
    let l = b.local(p);
    b.e(Expr::Ref { mutbl: Mutbl::Not, operand: l })
}

/// The library's own source, core/src/pin.rs (the pinned toolchain), `pin!`:
/// `{ super let mut pinned: PinMacroHelper<_> = PinMacroHelper { value: $value }; .. }`, with
/// "The value is pinned: it is the local above which cannot be named outside this macro": the
/// block's value points at that local, so the local must outlive the block. Where it goes is
/// where a temporary of the block's value would go: the Reference, "Temporary lifetime
/// extension": "The temporary scopes for expressions in `let` statements are sometimes extended
/// to the scope of the block containing the `let` statement" (the block is the `let`'s
/// initializer, an extending expression); "Temporary scopes": "the temporary scope of an
/// expression is the smallest scope that contains the expression and is one of: ... A
/// statement" (the block is a call's argument); "Drop scopes": dropped "in reverse order of
/// declaration".
/// ```text
/// fn main() {
///     let p = { super let v = G { x: 1 }; &v };  // v lives to main's end
///     mark(p);                                   // logs 101: v is live here
///     mark({ super let w = G { x: 3 }; &w });    // logs 103, then w (3) at this statement's end
///     let a = G { x: 2 };
/// }                                              // a (2), then v (1)
/// ```
/// Log: 101, 103, 3, 2, 1. With `super let` built as a plain `let` each binding is dropped at
/// its block's end, before its block's value is used: 1, 101, 3, 103, 2.
/// Expected: `test blaze-build: exit 0; N passed, 0 failed` with this test among them.
#[test]
fn pin_macro_value_lives_past_its_block() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], UNIT);
    let g = g_world(&mut w);
    let ref_g = w.ref_(Mutbl::Not, g);
    w.func(31, "mark", &[ref_g], I32);
    let mut b = BodyBuf::new();
    let (p, v, wb, a) = (b.bind("p"), b.bind("v"), b.bind("w"), b.bind("a"));
    // `{ super let v = G { x: 1 }; &v }`
    let v_init = g_lit(&mut b, 1);
    let v_ref = ref_local(&mut b, v);
    let super_v = StmtId(b.stmts.len() as u32);
    let p_init = b.block(&[Stmt::Let { pat: v, ty: None, init: Some(v_init), else_: None }], Some(v_ref));
    // `mark(p)`
    let p_use = b.local(p);
    let mark_p = b.call("mark", &[p_use]);
    // `mark({ super let w = G { x: 3 }; &w })`
    let w_init = g_lit(&mut b, 3);
    let w_ref = ref_local(&mut b, wb);
    let super_w = StmtId(b.stmts.len() as u32);
    let w_block = b.block(&[Stmt::Let { pat: wb, ty: None, init: Some(w_init), else_: None }], Some(w_ref));
    let mark_w = b.call("mark", &[w_block]);
    let a_init = g_lit(&mut b, 2);
    let root = b.block(
        &[
            Stmt::Let { pat: p, ty: None, init: Some(p_init), else_: None },
            Stmt::Expr { expr: mark_p, semi: true },
            Stmt::Expr { expr: mark_w, semi: true },
            Stmt::Let { pat: a, ty: None, init: Some(a_init), else_: None },
        ],
        None,
    );
    // Sorted, as `BodyView::super_lets` is: the inner blocks' statements came first.
    let super_lets = [super_v, super_w];
    let view = BodyView { super_lets: &super_lets, ..b.view(MAIN, root) };
    let mut cx = InferCtx::new_in(Alloc::global());
    let checked = match blaze_typeck::check(&view, &w, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        other => panic!("the check did not finish: {:?}", other.err()),
    };
    assert!(checked.diags.is_empty(), "diagnostics: {:?}", checked.diags);
    let sig = w.sig(MAIN).expect("a signature");
    let mut ground = PortableTys::new_in(Alloc::global());
    let main = {
        let mut subst = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
        crate::build(&view, &checked, &sig, &mut subst, &TestAdts { world: &w, body: &b }, Global)
            .unwrap_or_else(|e| panic!("the build failed: {e}"))
    };
    blaze_lir::verify(main.func.view()).unwrap_or_else(|e| panic!("does not verify: {e}"));

    #[cfg(target_arch = "aarch64")]
    {
        let mut gb = BodyBuf::new();
        let to_drop = gb.bind("to_drop");
        gb.params.push(to_drop);
        let groot = gb.block(&[], None);
        let gview = gb.view(DROP_IN_PLACE, groot);
        let mut gcx = InferCtx::new_in(Alloc::global());
        let gchecked = match blaze_typeck::check(&gview, &w, &mut gcx, &Needs::new_in(Alloc::global()), Alloc::global()) {
            Ok(Outcome::Done(c)) => c,
            other => panic!("the check did not finish: {:?}", other.err()),
        };
        let gsig = w.sig(DROP_IN_PLACE).expect("a signature");
        let ty = ground.intern(g).expect("intern");
        let args = ground.intern_list(&[ty]).expect("intern");
        let mut subst = Subst { args: &[ty], ground: &mut ground, scratch: Alloc::global() };
        let glue = crate::build_drop_glue(&gview, &gchecked, &gsig, &mut subst, &TestAdts { world: &w, body: &gb }, Global)
            .unwrap_or_else(|e| panic!("{e}"));
        DROPPED.with(|d| d.borrow_mut().clear());
        run_native_with(
            &[(G_DROP, host_g_drop as usize as u64), (MARK, host_mark as usize as u64)],
            &[(DROP_IN_PLACE, Some(args), &glue), (MAIN, None, &main)],
        );
        let seen = DROPPED.with(|d| d.borrow().clone());
        assert_eq!(
            seen.as_slice(),
            &[101, 103, 3, 2, 1],
            "a super let binding was dropped before its block's value was used (101/103 are the uses, 1/3 the bindings' drops)"
        );
    }
    let _ = (main, MARK);
}
