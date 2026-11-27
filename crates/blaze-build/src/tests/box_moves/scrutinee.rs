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

const E: ItemKey = ItemKey(50);
const BOX_E: ItemKey = ItemKey(53);
const BOX_BP: ItemKey = ItemKey(54);
const MK_REF: ItemKey = ItemKey(55);
const D_WRAP: ItemKey = ItemKey(56);
const D_WRAP_DROP: ItemKey = ItemKey(57);

/// What `D`'s `Drop` logs.
const D_DROPPED: i32 = 200;

thread_local! {
    /// Separate host buffers for `box_e` and `box_bp` so temporary boxes do not collide.
    static E_CELL: core::cell::UnsafeCell<[u64; 4]> = const { core::cell::UnsafeCell::new([0; 4]) };
    static BP_CELL: core::cell::UnsafeCell<[*mut i32; 2]> = const { core::cell::UnsafeCell::new([core::ptr::null_mut(); 2]) };
}

/// `box_e(val: E) -> Box<E>`: copies `E` (<= 16 bytes) into `E_CELL` and returns a pointer to it.
extern "C" fn host_box_e(out: *mut *mut u8, val: *const u8) {
    E_CELL.with(|cell| {
        // SAFETY: `E` is `enum E { A(P), B(G) }` (<= 16 bytes); `E_CELL` is 32 bytes aligned to 8.
        unsafe {
            let dst = cell.get().cast::<u8>();
            core::ptr::copy_nonoverlapping(val, dst, 16);
            out.write(dst);
        }
    });
}

/// `box_bp(bx: Box<P>) -> Box<Box<P>>`: stores the inner `Box<P>` pointer in `BP_CELL`.
extern "C" fn host_box_bp(out: *mut *mut *mut i32, bx: *const *mut i32) {
    BP_CELL.with(|cell| {
        // SAFETY: `bx` points to a one-pointer `Box<P>` aggregate; `BP_CELL` holds 2 pointers.
        unsafe {
            let dst = cell.get().cast::<*mut i32>();
            dst.write(bx.read());
            out.write(dst);
        }
    });
}

/// `mk_ref() -> &'static P`: returns a reference to `CELLS[2..4]`.
extern "C" fn host_mk_ref() -> *const i32 {
    CELLS.with(|cells| {
        // SAFETY: cells 2..4 of this thread's array.
        unsafe { cells.get().cast::<i32>().add(2) }
    })
}

/// `<D as Drop>::drop(&mut self)`: logs [`D_DROPPED`].
extern "C" fn host_d_drop(_: *mut u8) {
    DROPPED.with(|d| d.borrow_mut().push(D_DROPPED));
}

/// `s_world` extended with `enum E { A(P), B(G) }`, generic `Deref` for `Box<T>`,
/// `box_e(E) -> Box<E>`, `box_bp(Box<P>) -> Box<Box<P>>`, `mk_ref() -> &P`, and
/// `struct D { bx: Box<P> }` implementing `Drop`.
fn temp_box_world() -> TestWorld {
    let mut w = s_world();
    let g = PNode::Adt(ItemKey(9), List::EMPTY);
    let p = PNode::Adt(P, List::EMPTY);
    let pt = w.ty(p);
    let lp = w.types.intern_list(&[pt]).expect("intern");
    let box_p = PNode::Adt(BOX, lp);
    let bpt = w.ty(box_p);
    let lbp = w.types.intern_list(&[bpt]).expect("intern");
    let box_box_p = PNode::Adt(BOX, lbp);

    w.enum_(50, &[("A", CtorKind::Tuple, &[("0", p)]), ("B", CtorKind::Tuple, &[("0", g)])]);
    let e = PNode::Adt(E, List::EMPTY);
    let et = w.ty(e);
    let le = w.types.intern_list(&[et]).expect("intern");
    let box_e = PNode::Adt(BOX, le);

    w.func(53, "box_e", &[e], box_e);
    w.func(54, "box_bp", &[box_p], box_box_p);
    let r_p = w.ref_(Mutbl::Not, p);
    w.func(55, "mk_ref", &[], r_p);
    w.strukt(56, "D", &[("bx", box_p)]);
    w.drops.push((D_WRAP, D_WRAP_DROP));

    let t_param = w.ty(T);
    let lt = w.types.intern_list(&[t_param]).expect("intern");
    let box_t = PNode::Adt(BOX, lt);
    w.deref_generic_impl(1, box_t, T, BOX_DEREF);
    w
}

/// Runs `body` natively in `temp_box_world` (with glues for `G`, `P`, `E`, `Box<P>`, `Box<E>`,
/// `Box<Box<P>>`, `S`, `D`) and returns the drop log.
fn run_temp(w: &TestWorld, body: impl FnOnce(&mut BodyBuf) -> ExprId) -> Vec<i32, Global> {
    let mut b = BodyBuf::new();
    let root = body(&mut b);
    let mut ground = PortableTys::new_in(Alloc::global());
    let main = build_instance(w, &b, MAIN, root, &[], &mut ground).unwrap_or_else(|e| panic!("the build of main: {e}"));
    let text = blaze_lir::print(main.func.view());
    assert!(main.refs.iter().all(|r| r.item != BOX_DEREF), "a `Box`'s dereference called `Deref::deref`:\n{text}");
    let gg = ground.intern(PNode::Adt(ItemKey(9), List::EMPTY)).expect("intern");
    let pg = ground.intern(PNode::Adt(P, List::EMPTY)).expect("intern");
    let eg = ground.intern(PNode::Adt(E, List::EMPTY)).expect("intern");
    let lp = ground.intern_list(&[pg]).expect("intern");
    let bp = ground.intern(PNode::Adt(BOX, lp)).expect("intern");
    let le = ground.intern_list(&[eg]).expect("intern");
    let be = ground.intern(PNode::Adt(BOX, le)).expect("intern");
    let lbp = ground.intern_list(&[bp]).expect("intern");
    let bbp = ground.intern(PNode::Adt(BOX, lbp)).expect("intern");
    let sg = ground.intern(PNode::Adt(S, List::EMPTY)).expect("intern");
    let dg = ground.intern(PNode::Adt(D_WRAP, List::EMPTY)).expect("intern");
    let glues = [
        glue(w, &mut ground, gg),
        glue(w, &mut ground, pg),
        glue(w, &mut ground, eg),
        glue(w, &mut ground, bp),
        glue(w, &mut ground, be),
        glue(w, &mut ground, bbp),
        glue(w, &mut ground, sg),
        glue(w, &mut ground, dg),
    ];
    let mut fns: Vec<(ItemKey, Option<List>, &Built<Global>), Global> = Vec::new_in(Global);
    for (args, built) in glues.iter() {
        fns.push((DROP_IN_PLACE, Some(*args), built));
    }
    fns.push((MAIN, None, &main));
    let host = [
        (G_DROP, host_g_drop as usize as u64),
        (BOX_DROP, host_box_drop as usize as u64),
        (MK_P, host_mk_p as usize as u64),
        (BOX_E, host_box_e as usize as u64),
        (BOX_BP, host_box_bp as usize as u64),
        (MK_REF, host_mk_ref as usize as u64),
        (D_WRAP_DROP, host_d_drop as usize as u64),
    ];
    DROPPED.with(|d| d.borrow_mut().clear());
    run_native_with(&host, &fns);
    DROPPED.with(|d| d.borrow().clone())
}

/// `P { a: G { x: a }, c: G { x: c } }`.
fn p_lit(b: &mut BodyBuf, a: u128, c: u128) -> ExprId {
    let ga = g_lit(b, a);
    let gc = g_lit(b, c);
    let path = b.path("P");
    let start = b.field_inits.len() as u32;
    b.field_inits.push(FieldInit { name: Name::of("a"), expr: ga });
    b.field_inits.push(FieldInit { name: Name::of("c"), expr: gc });
    b.e(Expr::Struct { path, fields: List { start, len: 2 }, base: blaze_typeck::StructBase::None })
}

/// `*box_e(A(P { a: G{5}, c: G{6} }))` (when `is_a`), or `*box_e(B(G{7}))`.
fn star_box_e(b: &mut BodyBuf, is_a: bool) -> ExprId {
    let val = if is_a {
        let p = p_lit(b, 5, 6);
        b.call("A", &[p])
    } else {
        let g = g_lit(b, 7);
        b.call("B", &[g])
    };
    let bx = b.call("box_e", &[val]);
    b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: bx })
}

/// `A(<inner>)`.
fn a_pat(b: &mut BodyBuf, inner: PatId) -> PatId {
    let start = b.pat_lists.len() as u32;
    b.pat_lists.push(inner);
    let path = b.path("A");
    b.pat(Pat::TupleStruct { path, elems: List { start, len: 1 }, rest: None })
}

/// `B(<inner>)`.
fn b_pat(b: &mut BodyBuf, inner: PatId) -> PatId {
    let start = b.pat_lists.len() as u32;
    b.pat_lists.push(inner);
    let path = b.path("B");
    b.pat(Pat::TupleStruct { path, elems: List { start, len: 1 }, rest: None })
}

/// `if let` moving a drop-needing binding out of a dereferenced temporary `Box<E>` (the
/// `wasm-bindgen-macro` shape: `if let A(P { a, .. }) = *box_e(..)`).
/// ```text
/// (a) matching arm moving `a` out: unmoved `c` (6), box freed (100), then `k` (10), `out` (5)
///                                                                         [6, 100, 10, 5]
/// (b) non-matching arm (`*box_e(B(G{7}))`): all fields dropped (7), box freed (100), then
///     `k` (10), `out` (99)                                                [7, 100, 10, 99]
/// (c) moving the entire `*box_e(..)` out (`if let whole = *box_e(..)`): box freed alone (100),
///     then `k` (10), `whole`'s `a` (5), `c` (6)                           [100, 10, 5, 6]
/// ```
#[test]
fn an_if_let_on_a_temporary_box_enum_moves_out_and_drops_the_rest() {
    let w = temp_box_world();

    let if_let_enum = |is_a: bool| {
        move |b: &mut BodyBuf| {
            let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
            let init = star_box_e(b, is_a);
            let a = bind_a(b, false);
            let pp = p_pat(b, "a", a);
            let pat = a_pat(b, pp);
            let cond = b.e(Expr::Let { pat, init });
            let a_e = b.local(a);
            let then = b.block(&[], Some(a_e));
            let g99 = g_lit(b, 99);
            let else_ = b.block(&[], Some(g99));
            let iff = b.e(Expr::If { cond, then, else_: Some(else_) });
            stmts.push(let_(b, "out", iff));
            let k = g_lit(b, 10);
            stmts.push(let_(b, "k", k));
            b.block(&stmts, None)
        }
    };

    assert_eq!(run_temp(&w, if_let_enum(true)).as_slice(), &[6, FREED, 10, 5], "(a) matching if let *box_e(A(..))");
    assert_eq!(run_temp(&w, if_let_enum(false)).as_slice(), &[7, FREED, 10, 99], "(b) non-matching if let *box_e(B(..))");

    // (c) Moving the entire `*box_e(..)` out.
    let if_let_whole = |b: &mut BodyBuf| {
        let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
        let init = star_box_e(b, true);
        let whole = b.bind("whole");
        let cond = b.e(Expr::Let { pat: whole, init });
        let we = b.local(whole);
        let then = b.block(&[], Some(we));
        let g99 = g_lit(b, 99);
        let b99 = b.call("B", &[g99]);
        let else_ = b.block(&[], Some(b99));
        let iff = b.e(Expr::If { cond, then, else_: Some(else_) });
        stmts.push(let_(b, "out", iff));
        let k = g_lit(b, 10);
        stmts.push(let_(b, "k", k));
        b.block(&stmts, None)
    };
    assert_eq!(run_temp(&w, if_let_whole).as_slice(), &[FREED, 10, 5, 6], "(c) whole *box_e(..) moved out");

    // Planted controls verify the drop counter actually catches double-drops and leaks.
    let mut planted = temp_box_world();
    planted.planted = crate::Planted::MovedBoxDroppedWhole;
    assert_eq!(run_temp(&planted, if_let_enum(true)).as_slice(), &[5, 6, FREED, 10, 5], "control: temp box dropped whole");
    planted.planted = crate::Planted::MovedBoxNotFreed;
    assert_eq!(run_temp(&planted, if_let_enum(true)).as_slice(), &[6, 10, 5], "control: temp box never freed");
}

/// `match` on temporary `Box` scrutinees: multi-arm with match guard, `(*mk_p(..)).a`,
/// nested `**box_bp(mk_p(..))`, and `*(S { .. }.bx)`.
#[test]
fn a_match_on_a_temporary_box_handles_guards_arms_fields_and_nesting() {
    let w = temp_box_world();

    // (a) Multi-arm match with a guard on `*box_e(..)`:
    //   match *box_e(..) { A(P { a, .. }) if sel == 1 => a, A(P { c, .. }) => c, B(g) => g }
    let match_guarded = |is_a: bool, sel: u128| {
        move |b: &mut BodyBuf| {
            let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
            let scrutinee = star_box_e(b, is_a);
            let a = bind_a(b, false);
            let pp1 = p_pat(b, "a", a);
            let pat1 = a_pat(b, pp1);
            let s_lit = b.int(sel, None);
            let one = b.int(1, None);
            let guard = b.bin(BinOp::Eq, s_lit, one);
            let body1 = b.local(a);

            let c = b.bind("c");
            let pp2 = p_pat(b, "c", c);
            let pat2 = a_pat(b, pp2);
            let body2 = b.local(c);

            let g = b.bind("g");
            let pat3 = b_pat(b, g);
            let body3 = b.local(g);

            let start = b.arms.len() as u32;
            b.arms.push(Arm { pat: pat1, guard: Some(guard), body: body1 });
            b.arms.push(Arm { pat: pat2, guard: None, body: body2 });
            b.arms.push(Arm { pat: pat3, guard: None, body: body3 });
            let m = b.e(Expr::Match { scrutinee, arms: List { start, len: 3 } });
            stmts.push(let_(b, "out", m));
            let k = g_lit(b, 10);
            stmts.push(let_(b, "k", k));
            b.block(&stmts, None)
        }
    };
    assert_eq!(run_temp(&w, match_guarded(true, 1)).as_slice(), &[6, FREED, 10, 5], "(a1) guard true: moves a, drops c + frees");
    assert_eq!(run_temp(&w, match_guarded(true, 0)).as_slice(), &[5, FREED, 10, 6], "(a2) guard false: falls to arm 2, moves c, drops a + frees");
    assert_eq!(run_temp(&w, match_guarded(false, 0)).as_slice(), &[FREED, 10, 7], "(a3) arm 3 B(g): moves g, frees alone");

    // (b) `match (*mk_p(5, 6)).a { g => g }` (field of dereferenced temp Box).
    let match_field_of_box = |b: &mut BodyBuf| {
        let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
        let five = b.int(5, None);
        let six = b.int(6, None);
        let call = b.call("mk_p", &[five, six]);
        let deref = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: call });
        let paren = b.e(Expr::Paren(deref));
        let scrutinee = b.field(paren, "a");
        let g = b.bind("g");
        let body = b.local(g);
        let start = b.arms.len() as u32;
        b.arms.push(Arm { pat: g, guard: None, body });
        let m = b.e(Expr::Match { scrutinee, arms: List { start, len: 1 } });
        stmts.push(let_(b, "out", m));
        let k = g_lit(b, 10);
        stmts.push(let_(b, "k", k));
        b.block(&stmts, None)
    };
    assert_eq!(run_temp(&w, match_field_of_box).as_slice(), &[6, FREED, 10, 5], "(b) match (*mk_p(5, 6)).a");

    // (c) `match **box_bp(mk_p(5, 6)) { P { a, .. } => a }` (nested temporary Box<Box<P>>).
    let match_nested_box = |b: &mut BodyBuf| {
        let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
        let five = b.int(5, None);
        let six = b.int(6, None);
        let inner = b.call("mk_p", &[five, six]);
        let outer = b.call("box_bp", &[inner]);
        let d1 = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: outer });
        let scrutinee = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: d1 });
        let a = bind_a(b, false);
        let pat = p_pat(b, "a", a);
        let body = b.local(a);
        let start = b.arms.len() as u32;
        b.arms.push(Arm { pat, guard: None, body });
        let m = b.e(Expr::Match { scrutinee, arms: List { start, len: 1 } });
        stmts.push(let_(b, "out", m));
        let k = g_lit(b, 10);
        stmts.push(let_(b, "k", k));
        b.block(&stmts, None)
    };
    assert_eq!(run_temp(&w, match_nested_box).as_slice(), &[6, FREED, FREED, 10, 5], "(c) match **box_bp(mk_p(5, 6))");

    // (d) `if let P { a, .. } = *(S { bx: mk_p(5, 6), k: G { x: 7 } }.bx) { a } else { G { x: 99 } }`.
    let if_let_struct_box_field = |b: &mut BodyBuf| {
        let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
        let five = b.int(5, None);
        let six = b.int(6, None);
        let bx = b.call("mk_p", &[five, six]);
        let g7 = g_lit(b, 7);
        let spath = b.path("S");
        let fstart = b.field_inits.len() as u32;
        b.field_inits.push(FieldInit { name: Name::of("bx"), expr: bx });
        b.field_inits.push(FieldInit { name: Name::of("k"), expr: g7 });
        let s_expr = b.e(Expr::Struct { path: spath, fields: List { start: fstart, len: 2 }, base: blaze_typeck::StructBase::None });
        let s_bx = b.field(s_expr, "bx");
        let paren = b.e(Expr::Paren(s_bx));
        let init = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: paren });
        let a = bind_a(b, false);
        let pat = p_pat(b, "a", a);
        let cond = b.e(Expr::Let { pat, init });
        let ae = b.local(a);
        let then = b.block(&[], Some(ae));
        let g99 = g_lit(b, 99);
        let else_ = b.block(&[], Some(g99));
        let iff = b.e(Expr::If { cond, then, else_: Some(else_) });
        stmts.push(let_(b, "out", iff));
        let k = g_lit(b, 10);
        stmts.push(let_(b, "k", k));
        b.block(&stmts, None)
    };
    assert_eq!(run_temp(&w, if_let_struct_box_field).as_slice(), &[6, FREED, 7, 10, 5], "(d) if let *(S {{ .. }}.bx)");
}

/// Negative controls and unwind cleanup for temporary `Box` scrutinees:
/// - `if let P { a, .. } = *mk_ref()` (`&P`) must remain refused (E0507).
/// - `if let P { a, .. } = *(D { bx: mk_p(..) }.bx)` (`D: Drop`) must be refused (E0509).
/// - `if let P { ref a, .. } = *(D { bx: mk_p(..) }.bx)` (by-ref) must succeed and drop `D` whole.
/// - `if let P { a, .. } = *mk_p(5, 6) { f(); }` under `Panic::Unwind` frees `Box` without
///   calling whole `drop_in_place::<Box<P>>`.
#[test]
fn a_temporary_box_scrutinee_negative_controls_and_unwind() {
    let w = temp_box_world();

    // (a) Negative control: `*mk_ref()` where `mk_ref() -> &P` must NOT be treated as an owned temp box.
    {
        let mut b = BodyBuf::new();
        let call = b.call("mk_ref", &[]);
        let init = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: call });
        let a = bind_a(&mut b, false);
        let pat = p_pat(&mut b, "a", a);
        let cond = b.e(Expr::Let { pat, init });
        let ae = b.local(a);
        let ax = b.field(ae, "x");
        let u_stmt = let_(&mut b, "u", ax);
        let then = b.block(&[u_stmt], None);
        let iff = b.e(Expr::If { cond, then, else_: None });
        let root = b.block(&[Stmt::Expr { expr: iff, semi: true }], None);
        let mut ground = PortableTys::new_in(Alloc::global());
        let Err(error) = build_instance(&w, &b, MAIN, root, &[], &mut ground) else {
            panic!("a move out of `*mk_ref()` behind a shared reference must be refused (E0507)")
        };
        assert!(error.contains("a move out") || error.starts_with("diagnostics"), "{error}");
    }

    // (b) Negative control: moving through `D { bx: Box<P> }` where `D: Drop` (E0509).
    let d_scrutinee = |by_ref: bool| {
        move |b: &mut BodyBuf| {
            let five = b.int(5, None);
            let six = b.int(6, None);
            let bx = b.call("mk_p", &[five, six]);
            let dpath = b.path("D");
            let fstart = b.field_inits.len() as u32;
            b.field_inits.push(FieldInit { name: Name::of("bx"), expr: bx });
            let d_expr = b.e(Expr::Struct { path: dpath, fields: List { start: fstart, len: 1 }, base: blaze_typeck::StructBase::None });
            let d_bx = b.field(d_expr, "bx");
            let paren = b.e(Expr::Paren(d_bx));
            let init = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: paren });
            let a = bind_a(b, by_ref);
            let pat = p_pat(b, "a", a);
            let cond = b.e(Expr::Let { pat, init });
            let ae = b.local(a);
            let ax = b.field(ae, "x");
            let u_stmt = let_(b, "u", ax);
            let then = b.block(&[u_stmt], None);
            let iff = b.e(Expr::If { cond, then, else_: None });
            b.block(&[Stmt::Expr { expr: iff, semi: true }], None)
        }
    };
    {
        let mut b = BodyBuf::new();
        let root = d_scrutinee(false)(&mut b);
        let mut ground = PortableTys::new_in(Alloc::global());
        let Err(error) = build_instance(&w, &b, MAIN, root, &[], &mut ground) else {
            panic!("a move out of `*(D {{ bx: .. }}.bx)` through a `Drop` struct must be refused (E0509)")
        };
        assert!(error.contains("E0509") || error.contains("Drop"), "{error}");
    }
    // (c) Positive control: by-ref match on `*(D { bx: .. }.bx)` succeeds and drops `D` whole
    // (`D::drop` logs 200, then `bx` drops `a` 5, `c` 6, frees 100).
    assert_eq!(run_temp(&w, d_scrutinee(true)).as_slice(), &[D_DROPPED, 5, 6, FREED], "(c) ref match on Drop wrapper");

    // (d) Unwind cleanup: `if let P { a, .. } = *mk_p(5, 6) { f(); }` under `Panic::Unwind`.
    // The miss path (`link_fail_end`, where nothing moved) drops `Box<P>` whole once; the matched
    // normal exit, `f()`'s cleanup, and `a`'s exit-drop landing each test `a`'s bit, drop `c`,
    // and call `Box::drop` alone (3 calls). Control `MovedBoxDroppedWhole` drops whole on all 4.
    let build_unwind = |planted: crate::Planted| {
        let mut uw = temp_box_world();
        uw.panic = crate::Panic::Unwind;
        uw.planted = planted;
        let mut b = BodyBuf::new();
        let five = b.int(5, None);
        let six = b.int(6, None);
        let call = b.call("mk_p", &[five, six]);
        let init = b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: call });
        let a = bind_a(&mut b, false);
        let pat = p_pat(&mut b, "a", a);
        let cond = b.e(Expr::Let { pat, init });
        let f_call = b.call("f", &[]);
        let then = b.block(&[Stmt::Expr { expr: f_call, semi: true }], None);
        let iff = b.e(Expr::If { cond, then, else_: None });
        let root = b.block(&[Stmt::Expr { expr: iff, semi: true }], None);
        let mut ground = PortableTys::new_in(Alloc::global());
        let view = b.view(MAIN, root);
        let mut cx = InferCtx::new_in(Alloc::global());
        let checked = match blaze_typeck::check(&view, &uw, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
            Ok(Outcome::Done(c)) => c,
            other => panic!("the check did not finish: {:?}", other.err()),
        };
        assert!(checked.diags.is_empty(), "diagnostics: {:?}", checked.diags);
        let sig = uw.sig(MAIN).expect("a signature");
        let mut subst = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
        let main = crate::build(&view, &checked, &sig, &mut subst, &TestAdts { world: &uw, body: &b }, Global)
            .unwrap_or_else(|e| panic!("the build of main: {e}"));
        blaze_lir::verify(main.func.view()).unwrap_or_else(|e| panic!("does not verify: {e}"));
        let pg = ground.intern(PNode::Adt(P, List::EMPTY)).expect("intern");
        let lp = ground.intern_list(&[pg]).expect("intern");
        let bp = ground.intern(PNode::Adt(BOX, lp)).expect("intern");
        let lbp = ground.intern_list(&[bp]).expect("intern");
        let count = |item: ItemKey, args: List| {
            main.refs.iter().enumerate().filter(|(_, r)| r.item == item && r.args == args).map(|(i, _)| calls_of(&main, i)).sum::<usize>()
        };
        let text = blaze_lir::print(main.func.view());
        (count(BOX_DROP, lbp), count(DROP_IN_PLACE, lbp), text)
    };
    let (frees, whole, text) = build_unwind(crate::Planted::Nothing);
    assert!(text.contains("invoke") && text.contains(" cleanup:"), "f() lands in a cleanup:\n{text}");
    assert_eq!((frees, whole), (3, 1), "3 Box::drop frees (normal + f() cleanup + a-drop cleanup) and 1 miss-path whole drop:\n{text}");
    let (planted_frees, planted_whole, text) = build_unwind(crate::Planted::MovedBoxDroppedWhole);
    assert_eq!((planted_frees, planted_whole), (0, 4), "control: MovedBoxDroppedWhole drops whole on all 4 paths:\n{text}");
}
