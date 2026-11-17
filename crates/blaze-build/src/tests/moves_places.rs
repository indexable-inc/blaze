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
    b.pat(Pat::Bind { ann: BindAnn { by_ref: Some(Mutbl::Not), mutable: false }, name: Name::of(name), sub: None })
}

/// A by-value binding with no annotation.
fn plain_bind(b: &mut BodyBuf, name: &str) -> PatId {
    b.pat(Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of(name), sub: None })
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

/// Builds `main` (checked against `w`), runs it natively with `G`'s glue, and returns what was
/// dropped, in order (empty off aarch64, where nothing runs).
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
        // A local dropped whole (no field flags: `t` below, only borrowed) drops by
        // `drop_in_place::<(i32, G, G)>`, whose glue calls `G`'s: placed after it.
        let i32_ty = ground.intern(I32).expect("intern");
        let elems = ground.intern_list(&[i32_ty, ty, ty]).expect("intern");
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
        let seen = drops_of(&w, &b, root, g).unwrap_or_else(|e| panic!("nested={nested}: {e}"));
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
        let k_init = b.e(Expr::Struct { path, fields: List { start, len: 1 }, base: None });
        let k = b.bind("k");
        stmts.push(Stmt::Let { pat: k, ty: None, init: Some(k_init), else_: None });
        let root = b.block(&stmts, None);
        let seen = drops_of(&w, &b, root, g).unwrap_or_else(|e| panic!("sel={sel}: {e}"));
        #[cfg(target_arch = "aarch64")]
        assert_eq!(seen.as_slice(), want, "sel={sel}");
        let _ = (seen, want);
    }
}

/// A by-value binding of a part that needs drop, out of a place behind a reference, is a move
/// Rust refuses (E0507; the Reference, [expr.move.movable-place] lists no place behind a
/// shared reference). Out of a `Box` (`match *b { (_, a) => .. }`) it is allowed, and stays
/// REFUSED BY NAME here: the box's partial move (its allocation freed without the moved part's
/// drop) is not built; both reach `move_out` with no owner. The build must fail naming the
/// move, never build a copy that would be dropped twice:
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
    let Err(error) = drops_of(&w, &b, root, g) else { panic!("a move out of `*r` must be refused") };
    // The checker may refuse it first (a diagnostic); the builder's refusal names the move.
    assert!(error.contains("a move out") || error.starts_with("diagnostics"), "{error}");
}

mod field_scrutinee;
