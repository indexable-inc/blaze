//! Moves out of a TEMPORARY's field, and place scrutinees reached through a pointer, which move
//! nothing (walk/moves.rs `move_temp_field`, `scrutinee_in_place`): built from a body the
//! checker types, run natively, the drops compared. `G` appends its `x` when dropped.
//!
//! Found on the hello-world path: bstd's `Io::write_out` (`if let Reach::Hosted = self.reach`,
//! `self: &Io`, `Reach` needing drop) and core's `Result::is_ok` (`matches!(*self, Ok(_))`)
//! were refused as moves; both scrutinees are places, matched where they are.

use super::*;

/// `ref name`.
fn ref_bind(b: &mut BodyBuf, name: &str) -> PatId {
    b.pat(Pat::Bind { ann: BindAnn { by_ref: Some(Mutbl::Not), mutable: false }, name: Name::of(name), sub: None, ty: None })
}

/// A by-value binding with no annotation.
fn plain_bind(b: &mut BodyBuf, name: &str) -> PatId {
    b.pat(Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of(name), sub: None, ty: None })
}

fn tuple_pat(b: &mut BodyBuf, elems: &[PatId]) -> PatId {
    let elems = b.pats_list(elems);
    b.pat(Pat::Tuple { elems, rest: None })
}

fn match_on(b: &mut BodyBuf, scrutinee: ExprId, list: &[(PatId, ExprId)]) -> ExprId {
    let start = b.arms.len() as u32;
    for &(pat, body) in list {
        b.arms.push(Arm { pat, guard: None, body });
    }
    let arms = List { start, len: list.len() as u32 };
    b.e(Expr::Match { scrutinee, arms })
}

/// Builds `main` (checked against `w`), plus an optional generic helper instantiated at `[g]`,
/// runs it natively with `G`'s glue, and returns what was dropped, in order (empty off aarch64,
/// where nothing runs).
fn drops_of(
    w: &TestWorld,
    b: &BodyBuf,
    root: ExprId,
    g: PNode,
    extra: Option<(ItemKey, &BodyBuf, ExprId)>,
) -> Result<Vec<i32, Global>, String> {
    let mut ground = PortableTys::new_in(Alloc::global());
    let g_ty = ground.intern(g).expect("intern");
    let extra_built = match extra {
        Some((item, eb, eroot)) => Some((item, build_instance(w, eb, item, eroot, &[g_ty], &mut ground)?)),
        None => None,
    };
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
        let args = ground.intern_list(&[g_ty]).expect("intern");
        // A local dropped whole (no field flags: `t` below, only borrowed) drops by
        // `drop_in_place::<(i32, G, G)>`, whose glue calls `G`'s: placed after it.
        let i32_ty = ground.intern(I32).expect("intern");
        let elems = ground.intern_list(&[i32_ty, g_ty, g_ty]).expect("intern");
        let tuple_ty = ground.intern(PNode::Tuple(elems)).expect("intern");
        let tuple_args = ground.intern_list(&[tuple_ty]).expect("intern");
        let mut subst = Subst { args: &[g_ty], ground: &mut ground, scratch: Alloc::global() };
        let glue = crate::build_drop_glue(&view, &checked, &sig, &mut subst, &TestAdts { world: w, body: &gb }, Global)
            .unwrap_or_else(|e| panic!("{e}"));
        let mut subst = Subst { args: &[tuple_ty], ground: &mut ground, scratch: Alloc::global() };
        let tuple_glue = crate::build_drop_glue(&view, &checked, &sig, &mut subst, &TestAdts { world: w, body: &gb }, Global)
            .unwrap_or_else(|e| panic!("{e}"));
        DROPPED.with(|d| d.borrow_mut().clear());
        match &extra_built {
            Some((item, built)) => run_native_with(
                &[(G_DROP, host_g_drop as usize as u64)],
                &[
                    (DROP_IN_PLACE, Some(args), &glue),
                    (DROP_IN_PLACE, Some(tuple_args), &tuple_glue),
                    (*item, Some(args), built),
                    (MAIN, None, &main),
                ],
            ),
            None => run_native_with(
                &[(G_DROP, host_g_drop as usize as u64)],
                &[(DROP_IN_PLACE, Some(args), &glue), (DROP_IN_PLACE, Some(tuple_args), &tuple_glue), (MAIN, None, &main)],
            ),
        };
        seen = DROPPED.with(|d| d.borrow().clone());
    }
    let _ = (&main, &extra_built, g);
    Ok(seen)
}

/// A field moved out of a temporary. The Reference, [expr.move.movable-place]: "Only the
/// following place expressions may be moved out of: ... Temporary values. Fields of a place
/// expression which can be moved out of and don't implement `Drop`"; "Temporary scopes": a
/// temporary lives to the end of the smallest enclosing temporary scope (here the `let`
/// statement); "Destructors": "If a variable has been partially initialized, only its
/// initialized fields are dropped"; "The fields of a tuple are dropped in order"; a scope's
/// variables "in reverse order of declaration".
/// ```text
/// fn main() {
///     let a = (G { x: 1 }, G { x: 2 }, G { x: 3 }).1;   // the rest drops here: 1, 3
///     let k = G { x: 9 };
/// }                                                     // k, then a: 9, 2
/// fn main() {
///     let a = ((G { x: 1 }, G { x: 2 }), G { x: 3 }).0.1; // the rest: 1, 3
///     let k = G { x: 9 };
/// }                                                     // 9, 2
/// ```
/// Both: `1, 3, 9, 2`. PLANTED BUGS this catches: the temporary's rest not dropped (no
/// `Held::Moved`, the `Held::Rest` removed) gives `9, 2`; the temporary dropped whole after the
/// move (the bit not cleared) gives `1, 2, 3, 9, 2` (the moved field dropped twice). Before
/// `move_temp_field` the build failed with "a move out of a field of other than a local".
/// Prints nothing; passes as
/// "test tests::moves_places::a_field_moved_out_of_a_temporary_drops_the_rest_at_the_statement_end ... ok".
#[test]
fn a_field_moved_out_of_a_temporary_drops_the_rest_at_the_statement_end() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], UNIT);
    let g = g_world(&mut w);
    for nested in [false, true] {
        let mut b = BodyBuf::new();
        let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
        let (g1, g2, g3) = (g_lit(&mut b, 1), g_lit(&mut b, 2), g_lit(&mut b, 3));
        let moved = if nested {
            let inner = b.tuple(&[g1, g2]);
            let outer = b.tuple(&[inner, g3]);
            let f0 = b.tfield(outer, 0);
            b.tfield(f0, 1)
        } else {
            let t = b.tuple(&[g1, g2, g3]);
            b.tfield(t, 1)
        };
        let a = b.bind("a");
        stmts.push(Stmt::Let { pat: a, ty: None, init: Some(moved), else_: None });
        let k = b.bind("k");
        let g9 = g_lit(&mut b, 9);
        stmts.push(Stmt::Let { pat: k, ty: None, init: Some(g9), else_: None });
        let root = b.block(&stmts, None);
        let seen = drops_of(&w, &b, root, g, None).unwrap_or_else(|e| panic!("nested={nested}: {e}"));
        #[cfg(target_arch = "aarch64")]
        assert_eq!(seen.as_slice(), &[1, 3, 9, 2], "nested={nested}");
        let _ = seen;
    }
}

/// Place scrutinees through a reference move nothing. The Reference, "Match expressions": "If
/// the scrutinee expression is a place expression, the match does not allocate a temporary
/// location; however, a by-value binding may copy or move from the memory location";
/// "Binding modes": `ref` binds a reference to the matched place; "Field access expressions":
/// `r.2` with `r: &T` auto-dereferences `r` (a place behind the reference). Shapes from the
/// hello-world path: `matches!(*self, Ok(_))` (core's `Result::is_ok`) and
/// `if let Reach::Hosted = self.reach` (bstd's `Io::write_out`).
/// ```text
/// fn main() {                                                   // sel = 1   sel = 0
///     let t = (sel, G { x: 1 }, G { x: 2 });
///     let r = &t;
///     let x = match *r { (0, _, _) => 7, (_, ref a, _) => a.x };  // 1         7
///     let y = match r.2 { ref g => g.x };                         // 2         2
///     let z = if let (_, _, ref c) = *r { c.x } else { 0 };       // 2         2
///     let k = G { x: x * 100 + y * 10 + z };
/// }   // k, then t's fields in order:                             122, 1, 2  722, 1, 2
/// ```
/// PLANTED BUGS this catches: a scrutinee held as a temporary (owned, dropped at its
/// statement's end) adds `1, 2` after each match (a double drop); a scrutinee copied out
/// (`ref` binding the copy) still reads the same `x` but the copy's drop adds entries. Before
/// `scrutinee_in_place` the build failed with "a move out of a dereference whose type needs
/// drop" (`*r`) and "a move out of a field of other than a local" (`r.2`). Prints nothing;
/// passes as "test tests::moves_places::place_scrutinees_through_a_reference_move_nothing ... ok".
#[test]
fn place_scrutinees_through_a_reference_move_nothing() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], UNIT);
    let g = g_world(&mut w);
    let cases: [(u128, &[i32]); 2] = [(1, &[122, 1, 2]), (0, &[722, 1, 2])];
    for (sel, want) in cases {
        let mut b = BodyBuf::new();
        let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
        // let t = (sel, G1, G2); let r = &t;
        let s = b.int(sel, None);
        let (g1, g2) = (g_lit(&mut b, 1), g_lit(&mut b, 2));
        let value = b.tuple(&[s, g1, g2]);
        let t = b.bind("t");
        stmts.push(Stmt::Let { pat: t, ty: None, init: Some(value), else_: None });
        let t_e = b.local(t);
        let r_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: t_e });
        let r = b.bind("r");
        stmts.push(Stmt::Let { pat: r, ty: None, init: Some(r_init), else_: None });
        // let x = match *r { (0, _, _) => 7, (_, ref a, _) => a.x };
        let r1 = b.local(r);
        let star_r = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: r1 });
        let zero = b.pat_int(0);
        let (w1, w2) = (b.pat(Pat::Wild), b.pat(Pat::Wild));
        let p0 = tuple_pat(&mut b, &[zero, w1, w2]);
        let seven = b.int(7, None);
        let a = ref_bind(&mut b, "a");
        let (v0, v2) = (b.pat(Pat::Wild), b.pat(Pat::Wild));
        let p1 = tuple_pat(&mut b, &[v0, a, v2]);
        let a_e = b.local(a);
        let a_x = b.field(a_e, "x");
        let m1 = match_on(&mut b, star_r, &[(p0, seven), (p1, a_x)]);
        let x = b.bind("x");
        stmts.push(Stmt::Let { pat: x, ty: None, init: Some(m1), else_: None });
        // let y = match r.2 { ref g => g.x };
        let r2 = b.local(r);
        let r_2 = b.tfield(r2, 2);
        let gp = ref_bind(&mut b, "g");
        let g_e = b.local(gp);
        let g_x = b.field(g_e, "x");
        let m2 = match_on(&mut b, r_2, &[(gp, g_x)]);
        let y = b.bind("y");
        stmts.push(Stmt::Let { pat: y, ty: None, init: Some(m2), else_: None });
        // let z = if let (_, _, ref c) = *r { c.x } else { 0 };
        let r3 = b.local(r);
        let star_r3 = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: r3 });
        let c = ref_bind(&mut b, "c");
        let (u0, u1) = (b.pat(Pat::Wild), b.pat(Pat::Wild));
        let pc = tuple_pat(&mut b, &[u0, u1, c]);
        let cond = b.e(Expr::Let { pat: pc, init: star_r3 });
        let c_e = b.local(c);
        let c_x = b.field(c_e, "x");
        let then = b.block(&[], Some(c_x));
        let zero_e = b.int(0, None);
        let els = b.block(&[], Some(zero_e));
        let iff = b.e(Expr::If { cond, then, else_: Some(els) });
        let z = b.bind("z");
        stmts.push(Stmt::Let { pat: z, ty: None, init: Some(iff), else_: None });
        // let k = G { x: x * 100 + y * 10 + z };
        let (xe, ye, ze) = (b.local(x), b.local(y), b.local(z));
        let (h, ten) = (b.int(100, None), b.int(10, None));
        let x100 = b.bin(BinOp::Mul, xe, h);
        let y10 = b.bin(BinOp::Mul, ye, ten);
        let xy = b.bin(BinOp::Add, x100, y10);
        let sum = b.bin(BinOp::Add, xy, ze);
        let path = b.path("G");
        let start = b.field_inits.len() as u32;
        b.field_inits.push(FieldInit { name: Name::of("x"), expr: sum });
        let k_init = b.e(Expr::Struct { path, fields: List { start, len: 1 }, base: blaze_typeck::StructBase::None });
        let k = b.bind("k");
        stmts.push(Stmt::Let { pat: k, ty: None, init: Some(k_init), else_: None });
        let root = b.block(&stmts, None);
        let seen = drops_of(&w, &b, root, g, None).unwrap_or_else(|e| panic!("sel={sel}: {e}"));
        #[cfg(target_arch = "aarch64")]
        assert_eq!(seen.as_slice(), want, "sel={sel}");
        let _ = (seen, want);
    }
}

/// A by-value binding of a part that needs drop, out of a place behind a reference, is a move
/// Rust refuses (E0507; the Reference, [expr.move.movable-place] lists no place behind a
/// shared reference). Out of a `Box` owned by a local (`match *b { (_, a) => .. }`) it is
/// allowed and built (tests/box_moves/scrutinee.rs); behind a reference no one here owns the
/// place, `move_out` finds no owner. The build must fail naming the move, never build a copy
/// that would be dropped twice:
/// ```text
/// fn main() { let t = (1, G { x: 1 }); let r = &t; let x = match *r { (_, a) => a.x }; }
/// ```
/// PLANTED BUG this catches: a scrutinee owned as a temporary would build (and drop `t.1`
/// twice). Prints nothing; passes as
/// "test tests::moves_places::a_move_out_of_a_place_behind_a_reference_is_refused_by_name ... ok".
#[test]
fn a_move_out_of_a_place_behind_a_reference_is_refused_by_name() {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], UNIT);
    let g = g_world(&mut w);
    let mut b = BodyBuf::new();
    let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
    let one = b.int(1, None);
    let g1 = g_lit(&mut b, 1);
    let value = b.tuple(&[one, g1]);
    let t = b.bind("t");
    stmts.push(Stmt::Let { pat: t, ty: None, init: Some(value), else_: None });
    let t_e = b.local(t);
    let r_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: t_e });
    let r = b.bind("r");
    stmts.push(Stmt::Let { pat: r, ty: None, init: Some(r_init), else_: None });
    let r1 = b.local(r);
    let star_r = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: r1 });
    let w0 = b.pat(Pat::Wild);
    let a = plain_bind(&mut b, "a");
    let p = tuple_pat(&mut b, &[w0, a]);
    let a_e = b.local(a);
    let a_x = b.field(a_e, "x");
    let m = match_on(&mut b, star_r, &[(p, a_x)]);
    let x = b.bind("x");
    stmts.push(Stmt::Let { pat: x, ty: None, init: Some(m), else_: None });
    let root = b.block(&stmts, None);
    let Err(error) = drops_of(&w, &b, root, g, None) else { panic!("a move out of `*r` must be refused") };
    // The checker may refuse it first (a diagnostic); the builder's refusal names the move.
    assert!(error.contains("a move out") || error.starts_with("diagnostics"), "{error}");
}

/// `ManuallyDrop { value }` literal expression.
fn md_lit(b: &mut BodyBuf, value: ExprId) -> ExprId {
    let path = b.path("ManuallyDrop");
    let start = b.field_inits.len() as u32;
    b.field_inits.push(FieldInit { name: Name::of("value"), expr: value });
    b.e(Expr::Struct { path, fields: List { start, len: 1 }, base: blaze_typeck::StructBase::None })
}

/// Moving out of a `ManuallyDrop<T>` field — the exact shape of `core::mem::ManuallyDrop::into_inner`
/// (`fn into_inner<T>(slot: ManuallyDrop<T>) -> T { slot.value }`), plus a local, a nested field
/// (`slot.value.0`), and a temporary (`ManuallyDrop { value: G { x: 1 } }.value`).
///
/// `ManuallyDrop<T>` does not need drop (`!needs_drop`), so `slot` is never dropped at scope end,
/// while its field `value: T` DOES need drop. Moving `slot.value` out hands ownership to the
/// destination (dropped once) without double-dropping or leaking, and any sibling left inside
/// `ManuallyDrop` (`slot.value.1`) stays undropped.
/// ```text
/// fn into_inner<T>(slot: ManuallyDrop<T>) -> T { slot.value }
/// fn main() {
///     let a = into_inner(ManuallyDrop { value: G { x: 1 } }); // a drops 1 at end
///     let m = ManuallyDrop { value: G { x: 2 } };
///     let b = m.value;                                         // b drops 2
///     let c = ManuallyDrop { value: G { x: 3 } }.value;        // c drops 3
///     let p = ManuallyDrop { value: (G { x: 4 }, G { x: 5 }) };
///     let d = p.value.0;                                       // d drops 4; 5 stays in ManuallyDrop
///     let k = G { x: 9 };                                      // k drops 9
/// }   // reverse order of declaration: 9, 4, 3, 2, 1 (no 5, no double-drop)
/// ```
/// Before `KeptOwned`, `into_inner` failed to build with "a move out of a field of other than a
/// temporary or a local with field flags" and the temporary case failed with "a move out of a
/// field of a temporary not held to its statement's end".
#[test]
fn manually_drop_into_inner_and_field_moves_drop_once() {
    const MD: ItemKey = ItemKey(70);
    const INTO_INNER: ItemKey = ItemKey(72);

    let mut w = TestWorld::new();
    w.func(1, "main", &[], UNIT);
    let g = g_world(&mut w);
    w.generic_strukt(MD.0, "ManuallyDrop", 1, &[("value", PNode::Param(0))]);
    w.manually_drop = Some(MD);
    let p0 = w.ty(PNode::Param(0));
    let md_t_args = w.types.intern_list(&[p0]).expect("intern");
    let md_t = PNode::Adt(MD, md_t_args);
    w.generic_func(INTO_INNER.0, "into_inner", 1, &[md_t], PNode::Param(0));

    // fn into_inner<T>(slot: ManuallyDrop<T>) -> T { slot.value }
    let mut ib = BodyBuf::new();
    let slot = ib.bind("slot");
    ib.params.push(slot);
    let slot_e = ib.local(slot);
    let iroot = ib.field(slot_e, "value");

    // main
    let mut b = BodyBuf::new();
    // let a = into_inner(ManuallyDrop { value: G { x: 1 } });
    let g1 = g_lit(&mut b, 1);
    let m1 = md_lit(&mut b, g1);
    let call_a = b.call("into_inner", &[m1]);
    let a = b.bind("a");
    // let m = ManuallyDrop { value: G { x: 2 } }; let b = m.value;
    let g2 = g_lit(&mut b, 2);
    let m2 = md_lit(&mut b, g2);
    let m = b.bind("m");
    let m_e = b.local(m);
    let m_val = b.field(m_e, "value");
    let bb = b.bind("b");
    // let c = ManuallyDrop { value: G { x: 3 } }.value;
    let g3 = g_lit(&mut b, 3);
    let m3 = md_lit(&mut b, g3);
    let m3_val = b.field(m3, "value");
    let c = b.bind("c");
    // let p = ManuallyDrop { value: (G { x: 4 }, G { x: 5 }) }; let d = p.value.0;
    let (g4, g5) = (g_lit(&mut b, 4), g_lit(&mut b, 5));
    let pair = b.tuple(&[g4, g5]);
    let mp = md_lit(&mut b, pair);
    let p = b.bind("p");
    let p_e = b.local(p);
    let p_val = b.field(p_e, "value");
    let p_val_0 = b.tfield(p_val, 0);
    let d = b.bind("d");
    // let k = G { x: 9 };
    let g9 = g_lit(&mut b, 9);
    let k = b.bind("k");
    let root = b.block(
        &[
            Stmt::Let { pat: a, ty: None, init: Some(call_a), else_: None },
            Stmt::Let { pat: m, ty: None, init: Some(m2), else_: None },
            Stmt::Let { pat: bb, ty: None, init: Some(m_val), else_: None },
            Stmt::Let { pat: c, ty: None, init: Some(m3_val), else_: None },
            Stmt::Let { pat: p, ty: None, init: Some(mp), else_: None },
            Stmt::Let { pat: d, ty: None, init: Some(p_val_0), else_: None },
            Stmt::Let { pat: k, ty: None, init: Some(g9), else_: None },
        ],
        None,
    );

    let seen = drops_of(&w, &b, root, g, Some((INTO_INNER, &ib, iroot))).unwrap_or_else(|e| panic!("{e}"));
    #[cfg(target_arch = "aarch64")]
    assert_eq!(seen.as_slice(), &[9, 4, 3, 2, 1]);
    let _ = seen;
}

/// Moving a field out of a `ManuallyDrop` local and then reassigning it (and whole-reassigning
/// the `ManuallyDrop` local) tracks field flags so `assign_over` drops only when initialized and
/// never double-drops a moved-out field, while leaving the final value undropped at scope end.
/// ```text
/// fn main() {                                // take = 1          take = 0
///     let mut m = ManuallyDrop { value: G { x: 1 } };
///     if take != 0 {
///         let a = m.value;                   // drops 1           (not moved)
///     }
///     { let mid = G { x: 5 }; }              // drops 5           drops 5
///     m.value = G { x: 2 };                  // no drop (moved)   drops 1
///     m.value = G { x: 3 };                  // drops 2           drops 2
///     let b = m.value;                       // moves 3 out (dropped at scope end)
///     m = ManuallyDrop { value: G { x: 4 } };// resets field flags (3 not dropped here)
///     m.value = G { x: 6 };                  // drops 4           drops 4
/// }                                          // drops b (3); 6 stays in ManuallyDrop
/// ```
/// take = 1: `[1, 5, 2, 4, 3]`; take = 0: `[5, 1, 2, 4, 3]`.
#[test]
fn manually_drop_field_move_then_reassign_tracks_field_flags() {
    const MD: ItemKey = ItemKey(70);
    let mut w = TestWorld::new();
    w.func(1, "main", &[], UNIT);
    let g = g_world(&mut w);
    w.generic_strukt(MD.0, "ManuallyDrop", 1, &[("value", PNode::Param(0))]);
    w.manually_drop = Some(MD);

    let cases: [(u128, &[i32]); 2] = [(1, &[1, 5, 2, 4, 3]), (0, &[5, 1, 2, 4, 3])];
    for (take, want) in cases {
        let mut b = BodyBuf::new();
        let g1 = g_lit(&mut b, 1);
        let m_init = md_lit(&mut b, g1);
        let m = b.bind("m");
        // if take != 0 { let a = m.value; }
        let t_lit = b.int(take, None);
        let z_lit = b.int(0, None);
        let cond = b.bin(BinOp::Ne, t_lit, z_lit);
        let m_e = b.local(m);
        let m_val = b.field(m_e, "value");
        let a = b.bind("a");
        let then = b.block(&[Stmt::Let { pat: a, ty: None, init: Some(m_val), else_: None }], None);
        let iff = b.e(Expr::If { cond, then, else_: None });
        // { let mid = G { x: 5 }; }
        let g5 = g_lit(&mut b, 5);
        let mid = b.bind("mid");
        let mid_blk = b.block(&[Stmt::Let { pat: mid, ty: None, init: Some(g5), else_: None }], None);
        // m.value = G { x: 2 };
        let m_e2 = b.local(m);
        let lhs2 = b.field(m_e2, "value");
        let g2 = g_lit(&mut b, 2);
        let asg2 = b.e(Expr::Assign { lhs: lhs2, rhs: g2 });
        // m.value = G { x: 3 };
        let m_e3 = b.local(m);
        let lhs3 = b.field(m_e3, "value");
        let g3 = g_lit(&mut b, 3);
        let asg3 = b.e(Expr::Assign { lhs: lhs3, rhs: g3 });
        // let b = m.value;
        let m_e4 = b.local(m);
        let m_val4 = b.field(m_e4, "value");
        let bb = b.bind("b");
        // m = ManuallyDrop { value: G { x: 4 } };
        let m_e5 = b.local(m);
        let g4 = g_lit(&mut b, 4);
        let m4 = md_lit(&mut b, g4);
        let asg_whole = b.e(Expr::Assign { lhs: m_e5, rhs: m4 });
        // m.value = G { x: 6 };
        let m_e6 = b.local(m);
        let lhs6 = b.field(m_e6, "value");
        let g6 = g_lit(&mut b, 6);
        let asg6 = b.e(Expr::Assign { lhs: lhs6, rhs: g6 });
        let root = b.block(
            &[
                Stmt::Let { pat: m, ty: None, init: Some(m_init), else_: None },
                Stmt::Expr { expr: iff, semi: true },
                Stmt::Expr { expr: mid_blk, semi: true },
                Stmt::Expr { expr: asg2, semi: true },
                Stmt::Expr { expr: asg3, semi: true },
                Stmt::Let { pat: bb, ty: None, init: Some(m_val4), else_: None },
                Stmt::Expr { expr: asg_whole, semi: true },
                Stmt::Expr { expr: asg6, semi: true },
            ],
            None,
        );
        let seen = drops_of(&w, &b, root, g, None).unwrap_or_else(|e| panic!("take={take}: {e}"));
        #[cfg(target_arch = "aarch64")]
        assert_eq!(seen.as_slice(), want, "take={take}");
        let _ = (seen, want);
    }
}

/// Matching on a field of a `ManuallyDrop` local keeps it in place (`ref` and `_` drop nothing,
/// while a by-value binding moves it out and drops it once); moving out of a `ManuallyDrop`
/// behind a reference (`(&m).value`) is still refused (E0507).
#[test]
fn manually_drop_field_scrutinee_and_ref_move_control() {
    const MD: ItemKey = ItemKey(70);
    let mut w = TestWorld::new();
    w.func(1, "main", &[], UNIT);
    let g = g_world(&mut w);
    w.generic_strukt(MD.0, "ManuallyDrop", 1, &[("value", PNode::Param(0))]);
    w.manually_drop = Some(MD);

    // match m.value { ref r => r.x } and match m.value { _ => 0 } drop nothing;
    // match m.value { v => v.x } drops G { x: 1 } once at arm end.
    let mut b = BodyBuf::new();
    let g1 = g_lit(&mut b, 1);
    let m_init = md_lit(&mut b, g1);
    let m = b.bind("m");
    // let x = match m.value { ref r => r.x };
    let m1 = b.local(m);
    let m1_val = b.field(m1, "value");
    let rp = ref_bind(&mut b, "r");
    let r_e = b.local(rp);
    let r_x = b.field(r_e, "x");
    let match1 = match_on(&mut b, m1_val, &[(rp, r_x)]);
    let x = b.bind("x");
    // let _y = match m.value { _ => 0 };
    let m2 = b.local(m);
    let m2_val = b.field(m2, "value");
    let wild = b.pat(Pat::Wild);
    let zero = b.int(0, None);
    let match2 = match_on(&mut b, m2_val, &[(wild, zero)]);
    let y = b.bind("y");
    // let z = match m.value { v => v.x };
    let m3 = b.local(m);
    let m3_val = b.field(m3, "value");
    let vp = plain_bind(&mut b, "v");
    let v_e = b.local(vp);
    let v_x = b.field(v_e, "x");
    let match3 = match_on(&mut b, m3_val, &[(vp, v_x)]);
    let z = b.bind("z");
    // let k = G { x: x * 10 + z }; -> 11
    let (xe, ze) = (b.local(x), b.local(z));
    let ten = b.int(10, None);
    let x10 = b.bin(BinOp::Mul, xe, ten);
    let sum = b.bin(BinOp::Add, x10, ze);
    let g_path = b.path("G");
    let start = b.field_inits.len() as u32;
    b.field_inits.push(FieldInit { name: Name::of("x"), expr: sum });
    let k_init = b.e(Expr::Struct { path: g_path, fields: List { start, len: 1 }, base: blaze_typeck::StructBase::None });
    let k = b.bind("k");
    let root = b.block(
        &[
            Stmt::Let { pat: m, ty: None, init: Some(m_init), else_: None },
            Stmt::Let { pat: x, ty: None, init: Some(match1), else_: None },
            Stmt::Let { pat: y, ty: None, init: Some(match2), else_: None },
            Stmt::Let { pat: z, ty: None, init: Some(match3), else_: None },
            Stmt::Let { pat: k, ty: None, init: Some(k_init), else_: None },
        ],
        None,
    );
    let seen = drops_of(&w, &b, root, g, None).unwrap_or_else(|e| panic!("{e}"));
    #[cfg(target_arch = "aarch64")]
    assert_eq!(seen.as_slice(), &[1, 11]);
    let _ = seen;

    // Control: moving out of `r.value` where `r: &ManuallyDrop<G>` is refused (E0507).
    let mut b = BodyBuf::new();
    let g1 = g_lit(&mut b, 1);
    let m_init = md_lit(&mut b, g1);
    let m = b.bind("m");
    let m_e = b.local(m);
    let r_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: m_e });
    let r = b.bind("r");
    let r_e = b.local(r);
    let r_val = b.field(r_e, "value");
    let bad = b.bind("bad");
    let root = b.block(
        &[
            Stmt::Let { pat: m, ty: None, init: Some(m_init), else_: None },
            Stmt::Let { pat: r, ty: None, init: Some(r_init), else_: None },
            Stmt::Let { pat: bad, ty: None, init: Some(r_val), else_: None },
        ],
        None,
    );
    let Err(error) = drops_of(&w, &b, root, g, None) else { panic!("a move out of `(&m).value` must be refused") };
    assert!(error.contains("a move out") || error.starts_with("diagnostics"), "{error}");
}

mod field_scrutinee;
