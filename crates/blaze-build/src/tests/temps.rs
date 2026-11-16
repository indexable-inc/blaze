//! Borrowed temporaries whose type needs drop (walk/temps.rs): dropped at the end of their
//! statement, in reverse order of creation; an extended one refused by name.

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

    // `let r = &G { x: 6 };`: extended to the block's end, refused by name.
    let mut b2 = BodyBuf::new();
    let r = b2.bind("r");
    let g6 = g_lit(&mut b2, 6);
    let r_init = b2.e(Expr::Ref { mutbl: Mutbl::Not, operand: g6 });
    let root2 = b2.block(&[Stmt::Let { pat: r, ty: None, init: Some(r_init), else_: None }], None);
    let mut ground2 = PortableTys::new_in(Alloc::global());
    let Err(extended) = build_instance(&w, &b2, MAIN, root2, &[], &mut ground2) else { panic!("an extended temporary is refused") };
    assert!(extended.contains("an extended temporary whose type needs drop"), "{extended}");

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
