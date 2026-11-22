//! Struct update expressions, `P { a: x, ..base }` (walk/moves/struct_update.rs): built from a
//! body the checker types, run natively, the drops compared. `G` appends its `x` when dropped.
//!
//! Found on the hello-world path: `<impl Cx>::span` stopped with "a struct update (`..base`) is
//! not built to LIR yet".

use super::*;

/// `struct P { a: G, n: i32, c: G }`.
const P: ItemKey = ItemKey(60);
/// `fn make() -> P`.
const MAKE: ItemKey = ItemKey(61);

/// `P { a: G { x: a }, n: n, c: G { x: c } }`.
fn p_lit(b: &mut BodyBuf, a: u128, n: u128, c: u128) -> ExprId {
    let path = b.path("P");
    let (ga, gc) = (g_lit(b, a), g_lit(b, c));
    let nv = b.int(n, None);
    let start = b.field_inits.len() as u32;
    b.field_inits.push(FieldInit { name: Name::of("a"), expr: ga });
    b.field_inits.push(FieldInit { name: Name::of("n"), expr: nv });
    b.field_inits.push(FieldInit { name: Name::of("c"), expr: gc });
    b.e(Expr::Struct { path, fields: List { start, len: 3 }, base: blaze_typeck::StructBase::None })
}

/// `G { x: from }`.
fn g_of(b: &mut BodyBuf, from: ExprId) -> ExprId {
    let path = b.path("G");
    let start = b.field_inits.len() as u32;
    b.field_inits.push(FieldInit { name: Name::of("x"), expr: from });
    b.e(Expr::Struct { path, fields: List { start, len: 1 }, base: blaze_typeck::StructBase::None })
}

/// Builds `main` and `make` (checked against `w`), runs `main` natively with the glue of `G`
/// and of `P`, and returns what was dropped, in order (empty off aarch64, where nothing runs).
fn drops_of(w: &TestWorld, b: &BodyBuf, root: ExprId, mb: &BodyBuf, mroot: ExprId, g: PNode) -> Result<Vec<i32, Global>, String> {
    let mut ground = PortableTys::new_in(Alloc::global());
    let main = build_instance(w, b, MAIN, root, &[], &mut ground)?;
    let make = build_instance(w, mb, MAKE, mroot, &[], &mut ground)?;
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
        let gty = ground.intern(g).expect("intern");
        let g_args = ground.intern_list(&[gty]).expect("intern");
        // `P` dropped whole (the update's result) by `drop_in_place::<P>`, whose glue calls G's.
        let pty = ground.intern(PNode::Adt(P, List::EMPTY)).expect("intern");
        let p_args = ground.intern_list(&[pty]).expect("intern");
        let mut subst = Subst { args: &[gty], ground: &mut ground, scratch: Alloc::global() };
        let g_glue = crate::build_drop_glue(&view, &checked, &sig, &mut subst, &TestAdts { world: w, body: &gb }, Global)
            .unwrap_or_else(|e| panic!("{e}"));
        let mut subst = Subst { args: &[pty], ground: &mut ground, scratch: Alloc::global() };
        let p_glue = crate::build_drop_glue(&view, &checked, &sig, &mut subst, &TestAdts { world: w, body: &gb }, Global)
            .unwrap_or_else(|e| panic!("{e}"));
        DROPPED.with(|d| d.borrow_mut().clear());
        run_native_with(
            &[(G_DROP, host_g_drop as usize as u64)],
            &[(DROP_IN_PLACE, Some(g_args), &g_glue), (DROP_IN_PLACE, Some(p_args), &p_glue), (MAKE, None, &make), (MAIN, None, &main)],
        );
        seen = DROPPED.with(|d| d.borrow().clone());
    }
    let _ = (&main, &make, g);
    Ok(seen)
}

/// One field written, two taken: `n` (`i32`, copied) and `c` (`G`, moved). The Reference,
/// "Functional update syntax": the update "uses the given values for the fields that were
/// specified and moves or copies the remaining fields from the base expression"; "Destructors":
/// "If a variable has been partially initialized, only its initialized fields are dropped"; a
/// temporary drops at the end of its statement; a scope's variables "in reverse order of
/// declaration", a struct's fields in declaration order.
/// ```text
/// fn main() {                                  // local base       call-result base
///     let base = P { a: G{x:1}, n: 5, c: G{x:3} }; // (local only)
///     let q = P { a: G { x: 4 }, ..base };     //                   ..make(): make's a: 1
///     let k1 = G { x: q.c.x };                 // 3: c taken
///     let k2 = G { x: q.n };                   // 5: n taken
/// }   // k2, k1, q (4, 3), base's rest (1)     5, 3, 4, 3, 1       1, 5, 3, 4, 3
/// fn make() -> P { P { a: G { x: 1 }, n: 5, c: G { x: 3 } } }
/// ```
/// PLANTED BUGS this catches: the base dropped whole after the update (no bit cleared) adds a
/// second `3` (the moved `c` dropped twice); the base's rest never dropped loses the `1`; a
/// taken field not copied gives `k1` and `k2` other values than 3 and 5. Before
/// walk/moves/struct_update.rs the build failed with "a struct update (`..base`) is not built
/// to LIR yet". Prints nothing; passes as
/// "test tests::struct_update::a_struct_update_copies_and_moves_the_fields_it_does_not_name ... ok".
#[test]
fn a_struct_update_copies_and_moves_the_fields_it_does_not_name() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], UNIT);
    let g = g_world(&mut w);
    w.strukt(60, "P", &[("a", g), ("n", I32), ("c", g)]);
    let p = PNode::Adt(P, List::EMPTY);
    w.func(61, "make", &[], p);
    let mut mb = BodyBuf::new();
    let made = p_lit(&mut mb, 1, 5, 3);
    let mroot = mb.block(&[], Some(made));
    let cases: [(bool, &[i32]); 2] = [(false, &[5, 3, 4, 3, 1]), (true, &[1, 5, 3, 4, 3])];
    for (call, want) in cases {
        let mut b = BodyBuf::new();
        let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
        let base = if call {
            b.call("make", &[])
        } else {
            let lit = p_lit(&mut b, 1, 5, 3);
            let x = b.bind("base");
            stmts.push(Stmt::Let { pat: x, ty: None, init: Some(lit), else_: None });
            b.local(x)
        };
        // let q = P { a: G { x: 4 }, ..base };
        let path = b.path("P");
        let g4 = g_lit(&mut b, 4);
        let start = b.field_inits.len() as u32;
        b.field_inits.push(FieldInit { name: Name::of("a"), expr: g4 });
        let update = b.e(Expr::Struct { path, fields: List { start, len: 1 }, base: blaze_typeck::StructBase::Expr(base) });
        let q = b.bind("q");
        stmts.push(Stmt::Let { pat: q, ty: None, init: Some(update), else_: None });
        // let k1 = G { x: q.c.x }; let k2 = G { x: q.n };
        let lq = b.local(q);
        let qc = b.field(lq, "c");
        let qcx = b.field(qc, "x");
        let k1v = g_of(&mut b, qcx);
        let k1 = b.bind("k1");
        stmts.push(Stmt::Let { pat: k1, ty: None, init: Some(k1v), else_: None });
        let lq = b.local(q);
        let qn = b.field(lq, "n");
        let k2v = g_of(&mut b, qn);
        let k2 = b.bind("k2");
        stmts.push(Stmt::Let { pat: k2, ty: None, init: Some(k2v), else_: None });
        let root = b.block(&stmts, None);
        let seen = drops_of(&w, &b, root, &mb, mroot, g).unwrap_or_else(|e| panic!("call={call}: {e}"));
        #[cfg(target_arch = "aarch64")]
        assert_eq!(seen.as_slice(), want, "call={call}");
        let _ = (seen, want);
    }
}
