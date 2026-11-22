//! Moves out of a `Box` (walk/moves/boxes.rs): `let x = *b;`, `let y = (*b).a;`, under a
//! branch, in a loop, the box assigned again, and a call that unwinds after the move. Built
//! from bodies the checker types over a world with a `#[lang = "owned_box"]` box, emitted and
//! run natively; each host `Drop` logs, and the log is compared.
//!
//! The world (keys in brackets):
//! ```text
//! struct G { x: i32 }                       // [9]  impl Drop: the host logs `x`        [21]
//! #[lang = "owned_box"] struct Box<T> { p: *mut T }                                     [30]
//! impl<T> Drop for Box<T>                   // the host logs 100 (std's: only frees)    [31]
//! impl Deref for Box<C> { type Target = C; fn deref(&self) -> &C; }  // [32], NEVER built
//! fn mk(x: i32) -> Box<G>;                  // host: a G in a cell of its own           [33]
//! struct P { a: G, c: G }                   // [40], no `Drop` of its own
//! fn mk_p(a: i32, c: i32) -> Box<P>;        // host                                     [34]
//! fn f();                                   // may unwind (tests of the cleanup)        [35]
//! ```
//! The Reference: [expr.move.movable-place] "The result of dereferencing an expression with
//! type `Box<T>` and that can also be moved out of"; "Destructors": "If a variable has been
//! partially initialized, only its initialized fields are dropped", "The fields of a struct
//! are dropped in declaration order", a scope's variables drop "in reverse order of
//! declaration". The pinned library (library/alloc/src/boxed.rs 2126..): `Box`'s `Drop` runs
//! after "the T in the Box is dropped by the compiler" and only deallocates; so a box whose
//! contents moved out runs that `Drop` alone. `Deref::deref` of a box is `&**self`, the
//! builtin dereference: the build never calls it (it is not even built here: a call of it
//! would leave a callee unplaced and the run would stop).

use super::*;

const BOX: ItemKey = ItemKey(30);
const BOX_DROP: ItemKey = ItemKey(31);
const BOX_DEREF: ItemKey = ItemKey(32);
const MK: ItemKey = ItemKey(33);
const MK_P: ItemKey = ItemKey(34);
const P: ItemKey = ItemKey(40);

/// What the box's `Drop` logs.
const FREED: i32 = 100;

thread_local! {
    /// The host's "heap": `mk` puts its G in cell 0, `mk_p` its P in cells 2 and 3.
    static CELLS: core::cell::UnsafeCell<[i32; 8]> = const { core::cell::UnsafeCell::new([0; 8]) };
}

/// `mk(x)`: a `Box<G>` is a one-field struct, an aggregate, returned through the result
/// pointer, the first argument (lib.rs, "Rust ABI").
extern "C" fn host_mk(out: *mut *mut i32, x: i32) {
    CELLS.with(|c| {
        let p = c.get().cast::<i32>();
        // SAFETY: `p` is cell 0 of this thread's array, alive for the thread; `out` is the
        // caller's result slot of one pointer.
        unsafe {
            p.write(x);
            out.write(p);
        }
    });
}

/// `mk_p(a, c)`: `P { a: G { x: a }, c: G { x: c } }` in cells 2 and 3 (`P`'s fields in
/// declaration order at 0 and 4: two `i32`-sized structs, `repr(Rust)` may reorder; the
/// test reads which is which from the log, each `G` logging its own `x`).
extern "C" fn host_mk_p(out: *mut *mut i32, a: i32, c: i32) {
    CELLS.with(|cells| {
        // SAFETY: cells 2 and 3 of this thread's array; `out` as `host_mk`'s.
        unsafe {
            let p = cells.get().cast::<i32>().add(2);
            p.write(a);
            p.add(1).write(c);
            out.write(p);
        }
    });
}

/// `<Box<T> as Drop>::drop(&mut self)` on the host: frees nothing (the cells are the test's),
/// logs [`FREED`].
extern "C" fn host_box_drop(_: *mut *mut i32) {
    DROPPED.with(|d| d.borrow_mut().push(FREED));
}

/// The world above, with `Deref` implemented for `Box<contents>` (`contents` G or P).
fn box_world(contents_is_p: bool) -> (TestWorld, PNode) {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], UNIT);
    let g = g_world(&mut w);
    let gt = w.ty(g);
    w.strukt(40, "P", &[("a", g), ("c", g)]);
    let p = PNode::Adt(P, List::EMPTY);
    let pt = w.ty(p);
    let to = w.ty(T);
    w.generic_strukt(30, "Box", 1, &[("p", PNode::Ptr(Mutbl::Mut, to))]);
    w.owned_box = Some(BOX);
    w.drops.push((BOX, BOX_DROP));
    let lg = w.types.intern_list(&[gt]).expect("intern");
    let lp = w.types.intern_list(&[pt]).expect("intern");
    let (box_g, box_p) = (PNode::Adt(BOX, lg), PNode::Adt(BOX, lp));
    w.func(33, "mk", &[I32], box_g);
    w.func(34, "mk_p", &[I32, I32], box_p);
    w.func(35, "f", &[], UNIT);
    let (self_, target) = if contents_is_p { (box_p, p) } else { (box_g, g) };
    let r_self = w.ref_(Mutbl::Not, self_);
    let r_target = w.ref_(Mutbl::Not, target);
    w.func(32, "box_deref", &[r_self], r_target);
    w.deref_impl(self_, target, BOX_DEREF);
    (w, g)
}

/// The drop glue of ground type `t`: `drop_in_place::<t>`'s body and its argument list.
fn glue(w: &TestWorld, ground: &mut PortableTys<'static>, t: PTy) -> (List, Built<Global>) {
    let mut gb = BodyBuf::new();
    let p = gb.bind("to_drop");
    gb.params.push(p);
    let groot = gb.block(&[], None);
    let view = gb.view(DROP_IN_PLACE, groot);
    let mut cx = InferCtx::new_in(Alloc::global());
    let checked = match blaze_typeck::check(&view, w, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        other => panic!("the check of `drop_in_place` did not finish: {:?}", other.err()),
    };
    let sig = w.sig(DROP_IN_PLACE).expect("a signature");
    let args = ground.intern_list(&[t]).expect("intern");
    let mut subst = Subst { args: &[t], ground: &mut *ground, scratch: Alloc::global() };
    let built = crate::build_drop_glue(&view, &checked, &sig, &mut subst, &TestAdts { world: w, body: &gb }, Global)
        .unwrap_or_else(|e| panic!("the glue of {t:?}: {e}"));
    (args, built)
}

/// Builds `main` from `body` in `w` (the body's root), asserts it names no `Deref::deref`,
/// runs it natively with the glue of G, P, `Box<G>` and `Box<P>`, and gives the log.
fn run(w: &TestWorld, body: impl FnOnce(&mut BodyBuf) -> ExprId) -> Vec<i32, Global> {
    let mut b = BodyBuf::new();
    let root = body(&mut b);
    let mut ground = PortableTys::new_in(Alloc::global());
    let main = build_instance(w, &b, MAIN, root, &[], &mut ground).unwrap_or_else(|e| panic!("the build of main: {e}"));
    let text = blaze_lir::print(main.func.view());
    assert!(main.refs.iter().all(|r| r.item != BOX_DEREF), "a `Box`'s dereference called `Deref::deref`:\n{text}");
    let gg = ground.intern(PNode::Adt(ItemKey(9), List::EMPTY)).expect("intern");
    let pg = ground.intern(PNode::Adt(P, List::EMPTY)).expect("intern");
    let lg = ground.intern_list(&[gg]).expect("intern");
    let lp = ground.intern_list(&[pg]).expect("intern");
    let bg = ground.intern(PNode::Adt(BOX, lg)).expect("intern");
    let bp = ground.intern(PNode::Adt(BOX, lp)).expect("intern");
    // Callees before their callers: G's glue, P's (calls G's), the boxes' (call theirs).
    let glues = [glue(w, &mut ground, gg), glue(w, &mut ground, pg), glue(w, &mut ground, bg), glue(w, &mut ground, bp)];
    let mut fns: Vec<(ItemKey, Option<List>, &Built<Global>), Global> = Vec::new_in(Global);
    for (args, built) in glues.iter() {
        fns.push((DROP_IN_PLACE, Some(*args), built));
    }
    fns.push((MAIN, None, &main));
    let host = [
        (G_DROP, host_g_drop as usize as u64),
        (BOX_DROP, host_box_drop as usize as u64),
        (MK, host_mk as usize as u64),
        (MK_P, host_mk_p as usize as u64),
    ];
    DROPPED.with(|d| d.borrow_mut().clear());
    run_native_with(&host, &fns);
    DROPPED.with(|d| d.borrow().clone())
}

/// `let <name> = <callee>(<lits>);`, its binding.
fn let_mk(b: &mut BodyBuf, stmts: &mut Vec<Stmt, Global>, name: &str, callee: &str, lits: &[u128]) -> PatId {
    let mut args: Vec<ExprId, Global> = Vec::new_in(Global);
    for &v in lits {
        args.push(b.int(v, None));
    }
    let call = b.call(callee, &args);
    let p = b.bind(name);
    stmts.push(Stmt::Let { pat: p, ty: None, init: Some(call), else_: None });
    p
}

/// `*<local>`.
fn star(b: &mut BodyBuf, p: PatId) -> ExprId {
    let e = b.local(p);
    b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: e })
}

/// `let <name> = <init>;`.
fn let_(b: &mut BodyBuf, name: &str, init: ExprId) -> Stmt {
    let p = b.bind(name);
    Stmt::Let { pat: p, ty: None, init: Some(init), else_: None }
}

/// `if <sel> == 1 { <then> }`.
fn if_sel(b: &mut BodyBuf, sel: u128, then: &[Stmt]) -> Stmt {
    let s = b.int(sel, None);
    let one = b.int(1, None);
    let cond = b.bin(BinOp::Eq, s, one);
    let then = b.block(then, None);
    let iff = b.e(Expr::If { cond, then, else_: None });
    Stmt::Expr { expr: iff, semi: true }
}

/// The whole contents moved out (`let x = *b;`): the allocation is freed at `b`'s scope's
/// end, the contents dropped once, by their new owner.
/// ```text
/// (a) { let b = mk(3); let x = *b; }                          x (3), then b's free     [3, 100]
/// (b) { let b = mk(7); }                                      not moved: the glue      [7, 100]
/// (c) { let b = mk(4); let k = G{9}; if sel == 1 { let x = *b; } }
///        sel 1: x at the `if`'s end (4), k (9), b freed alone                     [4, 9, 100]
///        sel 0: k (9), b's contents (4, by its bit) then its free                 [9, 4, 100]
/// (d) { let b = mk(5); let x = *b; b = mk(6); }   the old box freed alone, then x (5), then the
///        new b whole (6, 100)                                                [100, 5, 6, 100]
/// (e) { let i = 0; while i < 2 { let b = mk(i); let x = *b; i = i + 1; } }   each turn: x, the
///        free; the word set again at each turn's `let`                       [0, 100, 1, 100]
/// ```
/// CONTROLS (the path state ignored): `Planted::MovedBoxDroppedWhole` (the moved-out box
/// dropped by its glue) gives (a) `[3, 3, 100]`, a double drop; `Planted::MovedBoxNotFreed`
/// gives (a) `[3]`, a leak. Prints nothing; passes as
/// "test tests::box_moves::a_box_moved_out_whole_frees_without_dropping_the_contents ... ok".
#[test]
fn a_box_moved_out_whole_frees_without_dropping_the_contents() {
    let (w, _) = box_world(false);
    // (a)
    let moved_out = |b: &mut BodyBuf| {
        let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
        let bx = let_mk(b, &mut stmts, "b", "mk", &[3]);
        let s = star(b, bx);
        stmts.push(let_(b, "x", s));
        b.block(&stmts, None)
    };
    assert_eq!(run(&w, moved_out).as_slice(), &[3, FREED], "(a) moved out whole");
    // (b)
    let not_moved = |b: &mut BodyBuf| {
        let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
        let_mk(b, &mut stmts, "b", "mk", &[7]);
        b.block(&stmts, None)
    };
    assert_eq!(run(&w, not_moved).as_slice(), &[7, FREED], "(b) not moved");
    // (c)
    for (sel, want) in [(1u128, [4, 9, FREED]), (0, [9, 4, FREED])] {
        let branch = |b: &mut BodyBuf| {
            let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
            let bx = let_mk(b, &mut stmts, "b", "mk", &[4]);
            let g9 = g_lit(b, 9);
            stmts.push(let_(b, "k", g9));
            let s = star(b, bx);
            let inner = let_(b, "x", s);
            stmts.push(if_sel(b, sel, &[inner]));
            b.block(&stmts, None)
        };
        assert_eq!(run(&w, branch).as_slice(), &want, "(c) moved under a branch, sel={sel}");
    }
    // (d)
    let assigned_again = |b: &mut BodyBuf| {
        let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
        let bx = let_mk(b, &mut stmts, "b", "mk", &[5]);
        let s = star(b, bx);
        stmts.push(let_(b, "x", s));
        let lhs = b.local(bx);
        let six = b.int(6, None);
        let rhs = b.call("mk", &[six]);
        let set = b.e(Expr::Assign { lhs, rhs });
        stmts.push(Stmt::Expr { expr: set, semi: true });
        b.block(&stmts, None)
    };
    assert_eq!(run(&w, assigned_again).as_slice(), &[FREED, 5, 6, FREED], "(d) assigned again");
    // (e)
    let in_a_loop = |b: &mut BodyBuf| {
        let i = b.bind("i");
        let zero = b.int(0, None);
        let init = Stmt::Let { pat: i, ty: None, init: Some(zero), else_: None };
        let ie = b.local(i);
        let two = b.int(2, None);
        let cond = b.bin(BinOp::Lt, ie, two);
        let mut turn: Vec<Stmt, Global> = Vec::new_in(Global);
        let ie = b.local(i);
        let call = b.call("mk", &[ie]);
        let bx = b.bind("b");
        turn.push(Stmt::Let { pat: bx, ty: None, init: Some(call), else_: None });
        let s = star(b, bx);
        turn.push(let_(b, "x", s));
        let (il, ir) = (b.local(i), b.local(i));
        let one = b.int(1, None);
        let next = b.bin(BinOp::Add, ir, one);
        let step = b.e(Expr::Assign { lhs: il, rhs: next });
        turn.push(Stmt::Expr { expr: step, semi: true });
        let w_ = b.while_(cond, &turn);
        b.block(&[init, Stmt::Expr { expr: w_, semi: true }], None)
    };
    assert_eq!(run(&w, in_a_loop).as_slice(), &[0, FREED, 1, FREED], "(e) in a loop");
    // The controls.
    let mut planted = box_world(false).0;
    planted.planted = crate::Planted::MovedBoxDroppedWhole;
    assert_eq!(run(&planted, moved_out).as_slice(), &[3, 3, FREED], "control: the moved box dropped whole");
    planted.planted = crate::Planted::MovedBoxNotFreed;
    assert_eq!(run(&planted, moved_out).as_slice(), &[3], "control: the moved box never freed");
}

/// A field moved out of a box's contents (`let y = (*b).a;`): the other field is dropped,
/// then the allocation freed; the moved field is dropped once, by its new owner.
/// ```text
/// (a) { let b = mk_p(5, 6); let y = (*b).a; }        y (5), b's `c` (6), b's free  [5, 6, 100]
/// (b) { let b = mk_p(5, 6); let k = G{9}; if sel == 1 { let y = (*b).a; } }
///        sel 1: y (5), k (9), b's `c` (6), the free                           [5, 9, 6, 100]
///        sel 0: k (9), b's `a` (5, by its bit), `c` (6), the free              [9, 5, 6, 100]
/// (c) { let b = mk_p(5, 6); }        not moved: the glue, P's fields in order  [5, 6, 100]
/// ```
/// CONTROLS: `MovedBoxDroppedWhole` gives (a) `[5, 5, 6, 100]` (the moved `a` dropped
/// again); `MovedBoxNotFreed` gives (a) `[5, 6]`. Prints nothing; passes as
/// "test tests::box_moves::a_field_moved_out_of_a_box_drops_the_rest_then_frees ... ok".
#[test]
fn a_field_moved_out_of_a_box_drops_the_rest_then_frees() {
    let (w, _) = box_world(true);
    let field_a = |b: &mut BodyBuf, bx: PatId| {
        let s = star(b, bx);
        let paren = b.e(Expr::Paren(s));
        b.field(paren, "a")
    };
    let moved = |b: &mut BodyBuf| {
        let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
        let bx = let_mk(b, &mut stmts, "b", "mk_p", &[5, 6]);
        let a = field_a(b, bx);
        stmts.push(let_(b, "y", a));
        b.block(&stmts, None)
    };
    assert_eq!(run(&w, moved).as_slice(), &[5, 6, FREED], "(a) a field moved out");
    for (sel, want) in [(1u128, [5, 9, 6, FREED]), (0, [9, 5, 6, FREED])] {
        let branch = |b: &mut BodyBuf| {
            let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
            let bx = let_mk(b, &mut stmts, "b", "mk_p", &[5, 6]);
            let g9 = g_lit(b, 9);
            stmts.push(let_(b, "k", g9));
            let a = field_a(b, bx);
            let inner = let_(b, "y", a);
            stmts.push(if_sel(b, sel, &[inner]));
            b.block(&stmts, None)
        };
        assert_eq!(run(&w, branch).as_slice(), &want, "(b) a field moved under a branch, sel={sel}");
    }
    let not_moved = |b: &mut BodyBuf| {
        let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
        let_mk(b, &mut stmts, "b", "mk_p", &[5, 6]);
        b.block(&stmts, None)
    };
    assert_eq!(run(&w, not_moved).as_slice(), &[5, 6, FREED], "(c) not moved");
    let mut planted = box_world(true).0;
    planted.planted = crate::Planted::MovedBoxDroppedWhole;
    assert_eq!(run(&planted, moved).as_slice(), &[5, 5, 6, FREED], "control: the box dropped whole");
    planted.planted = crate::Planted::MovedBoxNotFreed;
    assert_eq!(run(&planted, moved).as_slice(), &[5, 6], "control: the box never freed");
}

/// The contents moved out of a TEMPORARY box (`let x = *mk(3);`, bytes' `let shared =
/// *Box::from_raw(shared);`): the box is freed alone at its statement's end (its temporary
/// scope), the contents dropped once, by their new owner, at the block's end.
/// ```text
/// (a) { let x = *mk(3); let k = G{9}; }       the free, then k (9), then x (3)    [100, 9, 3]
/// (b) { let x = *mk(3); let y = *mk(4); }     each box freed at its own statement's end:
///        free, free, then y (4), x (3)                                        [100, 100, 4, 3]
/// ```
/// CONTROLS: `Planted::MovedBoxDroppedWhole` (the temporary box dropped by its whole glue)
/// gives (a) `[3, 100, 9, 3]`, a double drop of what moved out; `Planted::MovedBoxNotFreed`
/// gives (a) `[9, 3]`, a leak. Prints nothing; passes as
/// "test tests::box_moves::a_temporary_box_moved_out_frees_at_its_statement_end ... ok".
#[test]
fn a_temporary_box_moved_out_frees_at_its_statement_end() {
    let (w, _) = box_world(false);
    // `*mk(<x>)`.
    let star_mk = |b: &mut BodyBuf, x: u128| {
        let lit = b.int(x, None);
        let call = b.call("mk", &[lit]);
        b.e(Expr::Unary { op: blaze_typeck::UnOp::Deref, operand: call })
    };
    let then_k = |b: &mut BodyBuf| {
        let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
        let s = star_mk(b, 3);
        stmts.push(let_(b, "x", s));
        let g9 = g_lit(b, 9);
        stmts.push(let_(b, "k", g9));
        b.block(&stmts, None)
    };
    assert_eq!(run(&w, then_k).as_slice(), &[FREED, 9, 3], "(a) a temporary box moved out");
    let two = |b: &mut BodyBuf| {
        let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
        let s = star_mk(b, 3);
        stmts.push(let_(b, "x", s));
        let s = star_mk(b, 4);
        stmts.push(let_(b, "y", s));
        b.block(&stmts, None)
    };
    assert_eq!(run(&w, two).as_slice(), &[FREED, FREED, 4, 3], "(b) two temporary boxes moved out");
    let mut planted = box_world(false).0;
    planted.planted = crate::Planted::MovedBoxDroppedWhole;
    assert_eq!(run(&planted, then_k).as_slice(), &[3, FREED, 9, 3], "control: the temporary box dropped whole");
    planted.planted = crate::Planted::MovedBoxNotFreed;
    assert_eq!(run(&planted, then_k).as_slice(), &[9, 3], "control: the temporary box never freed");
}

/// The cleanup of a call that unwinds after a move out of a box drops what the normal exit
/// drops at that point, by the same walk: `{ let b = mk(3); let x = *b; f(); }` under
/// `Panic::Unwind`. In `main`'s LIR: `drop_in_place::<Box<G>>` is never named (the box is
/// never dropped whole: its contents moved out), `Box`'s `Drop` is called three times (the normal
/// exit, f()'s cleanup, and x's exit-drop landing, each freeing alone), and `drop_in_place::<G>` four times (x and the
/// contents' bit-tested drop, on each path). CONTROL: `MovedBoxDroppedWhole` names
/// `drop_in_place::<Box<G>>` (the whole glue on each path: the moved contents dropped
/// twice). Prints "cleanup after a box move: Box::drop 2 calls, drop_in_place::<G> 4, the
/// box's glue none".
#[test]
fn a_call_that_unwinds_after_a_box_move_frees_the_box_alone() {
    let build = |planted: crate::Planted| {
        let (mut w, _) = box_world(false);
        w.panic = crate::Panic::Unwind;
        w.planted = planted;
        let mut b = BodyBuf::new();
        let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
        let bx = let_mk(&mut b, &mut stmts, "b", "mk", &[3]);
        let s = star(&mut b, bx);
        stmts.push(let_(&mut b, "x", s));
        let call = b.call("f", &[]);
        stmts.push(Stmt::Expr { expr: call, semi: true });
        let root = b.block(&stmts, None);
        let mut ground = PortableTys::new_in(Alloc::global());
        // Built as tests/unwind.rs builds (not `build_instance`): the baseline tier's
        // literal-word form refuses unwinding, so this one is not emitted.
        let view = b.view(MAIN, root);
        let mut cx = InferCtx::new_in(Alloc::global());
        let checked = match blaze_typeck::check(&view, &w, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
            Ok(Outcome::Done(c)) => c,
            other => panic!("the check did not finish: {:?}", other.err()),
        };
        assert!(checked.diags.is_empty(), "diagnostics: {:?}", checked.diags);
        let sig = w.sig(MAIN).expect("a signature");
        let mut subst = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
        let main = crate::build(&view, &checked, &sig, &mut subst, &TestAdts { world: &w, body: &b }, Global)
            .unwrap_or_else(|e| panic!("the build of main: {e}"));
        blaze_lir::verify(main.func.view()).unwrap_or_else(|e| panic!("does not verify: {e}"));
        let gg = ground.intern(PNode::Adt(ItemKey(9), List::EMPTY)).expect("intern");
        let lg = ground.intern_list(&[gg]).expect("intern");
        let bg = ground.intern(PNode::Adt(BOX, lg)).expect("intern");
        let (lgg, lbg) = (ground.intern_list(&[gg]).expect("intern"), ground.intern_list(&[bg]).expect("intern"));
        let count = |item: ItemKey, args: List| {
            main.refs.iter().enumerate().filter(|(_, r)| r.item == item && r.args == args).map(|(i, _)| calls_of(&main, i)).sum::<usize>()
        };
        let text = blaze_lir::print(main.func.view());
        (count(BOX_DROP, lbg), count(DROP_IN_PLACE, lgg), count(DROP_IN_PLACE, lbg), text)
    };
    let (frees, contents, whole, text) = build(crate::Planted::Nothing);
    assert!(text.contains("invoke") && text.contains(" cleanup:"), "f() lands in a cleanup:\n{text}");
    // Plain calls only (`calls_of`): f()'s cleanup (x, b's contents, Box::drop), x's exit drop is an invoke
    // whose ladder landing drops b (contents, Box::drop), then b's exit drop (contents, Box::drop).
    assert_eq!((frees, contents, whole), (3, 4, 0), "Box::drop, drop_in_place::<G>, drop_in_place::<Box<G>>:\n{text}");
    let (_, _, whole, text) = build(crate::Planted::MovedBoxDroppedWhole);
    assert!(whole > 0, "control: the moved-out box dropped whole names its glue:\n{text}");
    println!("cleanup after a box move: Box::drop {frees} calls, drop_in_place::<G> {contents}, the box's glue none");
}

mod scrutinee;
