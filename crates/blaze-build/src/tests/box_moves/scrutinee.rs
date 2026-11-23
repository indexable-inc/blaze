//! A match by tests whose scrutinee is a `Box`'s contents owned by a local (`match *b`, `if let
//! P { a, .. } = *s.bx`): tested where they are, a by-value binding moving its part out by the
//! local's field flags, the box freed alone at the local's drop. Found on tempo's path: syn
//! 2.0.119 src/ty.rs:332 (`ambig_ty`, `group: TypeGroup` by value, `group.elem: Box<Type>`),
//! `if let Type::Path(mut ty) = *group.elem { .. return Ok(Type::Path(ty)); } else { .. ty:
//! group.elem .. }`, refused with "pattern 9: a binding by value of a part that needs drop, in a
//! match by tests (a move out)": walk/moves.rs `own_scrutinee` asked `scrutinee_in_place`
//! first, which takes every dereference as behind a pointer, so no one owned the scrutinee.
//!
//! The Reference, [expr.move.movable-place]: "Only the following place expressions may be
//! moved out of: ... Fields of a place expression which can be moved out of and don't implement
//! `Drop`. The result of dereferencing an expression with type `Box<T>` and that can also be
//! moved out of"; "Match expressions": "If the scrutinee expression is a place expression, the
//! match does not allocate a temporary location; however, a by-value binding may copy or move
//! from the memory location"; "Destructors": "If a variable has been partially initialized,
//! only its initialized fields are dropped", a struct's fields in declaration order, a scope's
//! variables in reverse order of declaration, a match arm's bindings when the arm ends.
//!
//! The world is box_moves.rs's with `Box<P>` (`P { a: G, c: G }`, `mk_p(a, c)` on the host)
//! and one struct more: `struct S { bx: Box<P>, k: G }` [41].

use super::*;

const S: ItemKey = ItemKey(41);

/// box_moves.rs's world with `Deref` for `Box<P>`, and `S`.
fn s_world() -> TestWorld {
    let (mut w, g) = box_world(true);
    let pt = w.ty(PNode::Adt(P, List::EMPTY));
    let lp = w.types.intern_list(&[pt]).expect("intern");
    w.strukt(41, "S", &[("bx", PNode::Adt(BOX, lp)), ("k", g)]);
    w
}

/// box_moves.rs `run`, with `S`'s glue placed too (a local `S` with no field flags drops by
/// it).
fn run_s(w: &TestWorld, body: impl FnOnce(&mut BodyBuf) -> ExprId) -> Vec<i32, Global> {
    let mut b = BodyBuf::new();
    let root = body(&mut b);
    let mut ground = PortableTys::new_in(Alloc::global());
    let main = build_instance(w, &b, MAIN, root, &[], &mut ground).unwrap_or_else(|e| panic!("the build of main: {e}"));
    let text = blaze_lir::print(main.func.view());
    assert!(main.refs.iter().all(|r| r.item != BOX_DEREF), "a `Box`'s dereference called `Deref::deref`:\n{text}");
    let gg = ground.intern(PNode::Adt(ItemKey(9), List::EMPTY)).expect("intern");
    let pg = ground.intern(PNode::Adt(P, List::EMPTY)).expect("intern");
    let lp = ground.intern_list(&[pg]).expect("intern");
    let bp = ground.intern(PNode::Adt(BOX, lp)).expect("intern");
    let sg = ground.intern(PNode::Adt(S, List::EMPTY)).expect("intern");
    // Callees before their callers: G's glue, P's, the box's, S's.
    let glues = [glue(w, &mut ground, gg), glue(w, &mut ground, pg), glue(w, &mut ground, bp), glue(w, &mut ground, sg)];
    let mut fns: Vec<(ItemKey, Option<List>, &Built<Global>), Global> = Vec::new_in(Global);
    for (args, built) in glues.iter() {
        fns.push((DROP_IN_PLACE, Some(*args), built));
    }
    fns.push((MAIN, None, &main));
    let host = [(G_DROP, host_g_drop as usize as u64), (BOX_DROP, host_box_drop as usize as u64), (MK_P, host_mk_p as usize as u64)];
    DROPPED.with(|d| d.borrow_mut().clear());
    run_native_with(&host, &fns);
    DROPPED.with(|d| d.borrow().clone())
}

/// `P { <field>: <pat>, .. }`.
fn p_pat(b: &mut BodyBuf, field: &str, pat: PatId) -> PatId {
    let start = b.field_pats.len() as u32;
    b.field_pats.push(FieldPat { name: Name::of(field), pat });
    let path = b.path("P");
    b.pat(Pat::Struct { path, fields: List { start, len: 1 }, rest: true })
}

/// A by-value binding (`a`), or `ref a`.
fn bind_a(b: &mut BodyBuf, by_ref: bool) -> PatId {
    let by_ref = if by_ref { Some(Mutbl::Not) } else { None };
    b.pat(Pat::Bind { ann: BindAnn { by_ref, mutable: false }, name: Name::of("a"), sub: None, ty: None })
}

/// `match <scrutinee> { P { a, .. } [if <guard>] => a.x, [_ => 0] }` (the wildcard arm only
/// with a guard).
fn match_a(b: &mut BodyBuf, scrutinee: ExprId, by_ref: bool, guard: Option<ExprId>) -> ExprId {
    let a = bind_a(b, by_ref);
    let pat = p_pat(b, "a", a);
    let a_e = b.local(a);
    let body = b.field(a_e, "x");
    let start = b.arms.len() as u32;
    b.arms.push(Arm { pat, guard, body });
    let mut len = 1;
    if guard.is_some() {
        let wild = b.pat(Pat::Wild);
        let zero = b.int(0, None);
        b.arms.push(Arm { pat: wild, guard: None, body: zero });
        len = 2;
    }
    b.e(Expr::Match { scrutinee, arms: List { start, len } })
}

/// `G { x: <x> * 10 }`.
fn g_times_ten(b: &mut BodyBuf, x: PatId) -> ExprId {
    let xe = b.local(x);
    let ten = b.int(10, None);
    let v = b.bin(BinOp::Mul, xe, ten);
    let path = b.path("G");
    let start = b.field_inits.len() as u32;
    b.field_inits.push(FieldInit { name: Name::of("x"), expr: v });
    b.e(Expr::Struct { path, fields: List { start, len: 1 }, base: blaze_typeck::StructBase::None })
}

/// `{ let b = mk_p(5, 6); let y = match *b { P { a, .. } [if sel == 1] => a.x, [_ => 0] };
/// let k = G { x: y * 10 }; }`.
fn match_star_b(by_ref: bool, guard: Option<u128>) -> impl FnOnce(&mut BodyBuf) -> ExprId {
    move |b: &mut BodyBuf| {
        let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
        let bx = let_mk(b, &mut stmts, "b", "mk_p", &[5, 6]);
        let s = star(b, bx);
        let guard = guard.map(|sel| {
            let s = b.int(sel, None);
            let one = b.int(1, None);
            b.bin(BinOp::Eq, s, one)
        });
        let m = match_a(b, s, by_ref, guard);
        let y = b.bind("y");
        stmts.push(Stmt::Let { pat: y, ty: None, init: Some(m), else_: None });
        let k = g_times_ten(b, y);
        stmts.push(let_(b, "k", k));
        b.block(&stmts, None)
    }
}

/// `match *b` moving a part of the box's contents out by value. The arm's `a` drops at the
/// arm's end (5); then, in reverse order, `k` (50) and `b`: its contents minus `a` (`c`, 6),
/// then the allocation freed alone (100).
/// ```text
/// (a) { let b = mk_p(5, 6); let y = match *b { P { a, .. } => a.x }; let k = G { x: y * 10 }; }
///                                                                         [5, 50, 6, 100]
/// (b) the same arm guarded by `if sel == 1`, a wildcard arm `_ => 0` after it:
///        sel 1: the move after the guard passed                          [5, 50, 6, 100]
///        sel 0: nothing moves; k is G { x: 0 }, b's glue whole           [0, 5, 6, 100]
/// (c) THE CONTROL, `ref a`: nothing moves                                [50, 5, 6, 100]
/// ```
/// PLANTED BUGS this catches (the path state ignored at the box, tests/box_moves.rs's
/// controls): `Planted::MovedBoxDroppedWhole` gives (a) `[5, 50, 5, 6, 100]` (the moved `a`
/// dropped twice); `Planted::MovedBoxNotFreed` gives (a) `[5, 50, 6]` (a leak). Before the fix
/// (a) and (b) failed the build with "a binding by value of a part that needs drop, in a match
/// by tests (a move out)"; (c) built. Prints nothing; passes as
/// "test tests::box_moves::scrutinee::a_match_on_a_boxs_contents_moves_a_part_by_the_boxs_word ... ok".
#[test]
fn a_match_on_a_boxs_contents_moves_a_part_by_the_boxs_word() {
    let w = s_world();
    assert_eq!(run_s(&w, match_star_b(false, None)).as_slice(), &[5, 50, 6, FREED], "(a) moved out of `*b`");
    assert_eq!(run_s(&w, match_star_b(false, Some(1))).as_slice(), &[5, 50, 6, FREED], "(b) guarded, sel 1");
    assert_eq!(run_s(&w, match_star_b(false, Some(0))).as_slice(), &[0, 5, 6, FREED], "(b) guarded, sel 0");
    assert_eq!(run_s(&w, match_star_b(true, None)).as_slice(), &[50, 5, 6, FREED], "(c) the control: `ref a`");
    let mut planted = s_world();
    planted.planted = crate::Planted::MovedBoxDroppedWhole;
    assert_eq!(run_s(&planted, match_star_b(false, None)).as_slice(), &[5, 50, 5, 6, FREED], "control: the box dropped whole");
    planted.planted = crate::Planted::MovedBoxNotFreed;
    assert_eq!(run_s(&planted, match_star_b(false, None)).as_slice(), &[5, 50, 6], "control: the box never freed");
}

/// `let s = S { bx: mk_p(5, 6), k: G { x: 7 } };`, its binding.
fn let_s(b: &mut BodyBuf, stmts: &mut Vec<Stmt, Global>) -> PatId {
    let five = b.int(5, None);
    let six = b.int(6, None);
    let bx = b.call("mk_p", &[five, six]);
    let k = g_lit(b, 7);
    let path = b.path("S");
    let start = b.field_inits.len() as u32;
    b.field_inits.push(FieldInit { name: Name::of("bx"), expr: bx });
    b.field_inits.push(FieldInit { name: Name::of("k"), expr: k });
    let value = b.e(Expr::Struct { path, fields: List { start, len: 2 }, base: blaze_typeck::StructBase::None });
    let s = b.bind("s");
    stmts.push(Stmt::Let { pat: s, ty: None, init: Some(value), else_: None });
    s
}

/// syn's shape, `if let P { a, .. } = *s.bx { .. }` with `s` owned by value: the part `[Field
/// bx, Deref, Field a]` of `s` moves by `s`'s word. `a` drops at the `if`'s end (5); `s` at the
/// block's end, its fields in declaration order: `bx` (its contents minus `a`: `c`, 6; then the
/// free, 100), then `k` (7).
/// ```text
/// (a) { let s = S { .. }; if let P { a, .. } = *s.bx { let u = a.x; } }   [5, 6, 100, 7]
/// (b) { let s = S { .. }; if let P { a, .. } = *s.bx { (*s.bx).a = G { x: a.x * 10 }; } }
///        syn's second shape (`*group.elem = Type::Path(ty)` puts the part back): the
///        assignment over the moved part drops nothing and sets its bit again; `a` (5) at the
///        `if`'s end, then `s`: `bx`'s `a` (50), `c` (6), the free, `k` (7)  [5, 50, 6, 100, 7]
/// (c) THE CONTROL, `ref a`: nothing moves; `s` whole by its glue          [5, 6, 100, 7]
/// ```
/// THE NEGATIVE TWIN (the cause kept: the box behind a reference, `let r = &s; match (*r).bx`
/// written `r.bx`): `match *r.bx { P { a, .. } => a.x }` is E0507 and must stay refused by name
/// ("a move out", or the checker's diagnostic), never built as a copy dropped twice. PLANTED
/// BUG: `Planted::MovedBoxDroppedWhole` gives (a) `[5, 5, 6, 100, 7]`. Prints nothing; passes
/// as "test tests::box_moves::scrutinee::an_if_let_on_a_fields_box_contents_moves_by_the_locals_word ... ok".
#[test]
fn an_if_let_on_a_fields_box_contents_moves_by_the_locals_word() {
    let w = s_world();
    // `*s.bx`.
    let star_s_bx = |b: &mut BodyBuf, s: PatId| {
        let se = b.local(s);
        let bx = b.field(se, "bx");
        b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: bx })
    };
    let if_let = |by_ref: bool, put_back: bool| {
        move |b: &mut BodyBuf| {
            let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
            let s = let_s(b, &mut stmts);
            let init = star_s_bx(b, s);
            let a = bind_a(b, by_ref);
            let pat = p_pat(b, "a", a);
            let cond = b.e(Expr::Let { pat, init });
            let mut then: Vec<Stmt, Global> = Vec::new_in(Global);
            if put_back {
                // (*s.bx).a = G { x: a.x * 10 };
                let target = star_s_bx(b, s);
                let paren = b.e(Expr::Paren(target));
                let lhs = b.field(paren, "a");
                let a_e = b.local(a);
                let ax = b.field(a_e, "x");
                let ten = b.int(10, None);
                let v = b.bin(BinOp::Mul, ax, ten);
                let path = b.path("G");
                let start = b.field_inits.len() as u32;
                b.field_inits.push(FieldInit { name: Name::of("x"), expr: v });
                let rhs = b.e(Expr::Struct { path, fields: List { start, len: 1 }, base: blaze_typeck::StructBase::None });
                let set = b.e(Expr::Assign { lhs, rhs });
                then.push(Stmt::Expr { expr: set, semi: true });
            } else {
                let a_e = b.local(a);
                let ax = b.field(a_e, "x");
                then.push(let_(b, "u", ax));
            }
            let then = b.block(&then, None);
            let iff = b.e(Expr::If { cond, then, else_: None });
            stmts.push(Stmt::Expr { expr: iff, semi: true });
            b.block(&stmts, None)
        }
    };
    assert_eq!(run_s(&w, if_let(false, false)).as_slice(), &[5, 6, FREED, 7], "(a) moved out of `*s.bx`");
    assert_eq!(run_s(&w, if_let(false, true)).as_slice(), &[5, 50, 6, FREED, 7], "(b) moved out, then put back");
    assert_eq!(run_s(&w, if_let(true, false)).as_slice(), &[5, 6, FREED, 7], "(c) the control: `ref a`");
    let mut planted = s_world();
    planted.planted = crate::Planted::MovedBoxDroppedWhole;
    assert_eq!(run_s(&planted, if_let(false, false)).as_slice(), &[5, 5, 6, FREED, 7], "control: the box dropped whole");
    // The negative twin: the box behind a shared reference.
    let mut b = BodyBuf::new();
    let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
    let s = let_s(&mut b, &mut stmts);
    let se = b.local(s);
    let r_init = b.e(Expr::Ref { mutbl: Mutbl::Not, operand: se });
    let r = b.bind("r");
    stmts.push(Stmt::Let { pat: r, ty: None, init: Some(r_init), else_: None });
    let init = star_s_bx(&mut b, r);
    let m = match_a(&mut b, init, false, None);
    let y = b.bind("y");
    stmts.push(Stmt::Let { pat: y, ty: None, init: Some(m), else_: None });
    let root = b.block(&stmts, None);
    let mut ground = PortableTys::new_in(Alloc::global());
    let Err(error) = build_instance(&w, &b, MAIN, root, &[], &mut ground) else {
        panic!("a move out of `*r.bx` behind a shared reference must be refused (E0507)")
    };
    assert!(error.contains("a move out") || error.starts_with("diagnostics"), "{error}");
}
