//! Owned operands waiting on the value stack join the cleanup (walk/val_stack.rs; UNWIND.md
//! piece P5), read from the built LIR's unwind edges: each `invoke`, whom it calls, and what its
//! landing block drops. No test here counts on a callee being a plain `call`: every call may
//! unwind, and what each landing block must drop is stated from the rule.
//!
//! The Reference, "Destructors" (temporaries): "Temporaries are also created to hold the result
//! of operands to an expression while the other operands are evaluated. ... Since the
//! temporaries are moved from once the expression is evaluated, dropping them has no effect
//! unless one of the operands to an expression breaks out of the expression, returns, or
//! panics." Its tuple example: `(PrintOnDrop("first operand"), PrintOnDrop("second operand"),
//! loop { break; })`: "the first two operands are dropped in reverse order". "Panic"
//! ([panic.unwind.destruction]): "as the panic traverses Rust frames, live objects in those
//! frames that implement Drop will have their drop methods called". A scope's variables drop
//! "in reverse order of declaration", after the temporaries of the expression inside it (the
//! operand's temporary scope is the innermost one), so the waiting operands drop before the
//! locals. A value moved into a call (an argument) is the callee's: "Destructors", moved-from
//! values are not dropped, so a landing block of that call never drops it.
//!
//! The test world's `G` (`struct G { x: i32 }` with a `Drop` impl) is the `S: Drop` of the
//! piece's text. Every value here is statically live or dead at each call (no drop flag), so a
//! landing block is one straight block of drops ending in `resume`; one that branches is
//! refused by name below rather than read.
//!
//! Replaces tests/operand_cleanup.rs, whose second test was wrong for correct LIR: it counted
//! one `invoke`, but `a` is live across `take2(..)`, so by the rule above `take2` itself must
//! land (a plain call would skip `a`'s drop when it unwinds).

use blaze_lir::func::{RefId, RefKind};
use blaze_lir::{Block, BlockKind, Op, Value, View};

use super::*;

/// `fn f() -> i32`.
const F: ItemKey = ItemKey(33);
/// `fn take(G, i32)`.
const TAKE: ItemKey = ItemKey(34);
/// `fn take2(G, G, i32)`.
const TAKE2: ItemKey = ItemKey(36);
/// `fn poke(&mut G, i32)`.
const POKE: ItemKey = ItemKey(37);

/// One `invoke` of the built function: whom it calls and what its landing block drops.
struct Landing {
    /// The callee's item (`DROP_IN_PLACE` for drop glue).
    callee: ItemKey,
    /// The landing block.
    pad: Block,
    /// The slots the landing block's drops take, in the order it calls them.
    drops: Vec<u32, Global>,
}

/// The built function as its edges show it.
struct Edges {
    /// Every `invoke`, in layout order.
    landings: Vec<Landing, Global>,
    /// The slots dropped outside landing blocks (the normal path), in layout order.
    normal_drops: Vec<u32, Global>,
    /// Every callee a `call` or `invoke` names, drop glue left out, in layout order.
    callees: Vec<ItemKey, Global>,
    /// The printed function, for messages.
    text: String,
}

impl Edges {
    /// The landings of the invokes of `callee`.
    fn of(&self, callee: ItemKey) -> Vec<&Landing, Global> {
        let mut v = Vec::new_in(Global);
        v.extend(self.landings.iter().filter(|l| l.callee == callee));
        v
    }

    /// How many calls (plain or invoked) name `callee`.
    fn calls(&self, callee: ItemKey) -> usize {
        self.callees.iter().filter(|&&c| c == callee).count()
    }
}

/// Checks `body` as `main` in `w` and builds it.
fn build_main(w: &TestWorld, b: &BodyBuf, root: ExprId) -> Result<Built<Global>, BuildError> {
    let view = b.view(MAIN, root);
    let mut cx = InferCtx::new_in(Alloc::global());
    let checked = match blaze_typeck::check(&view, w, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        other => panic!("the check did not finish: {:?}", other.err()),
    };
    assert!(checked.diags.is_empty(), "diagnostics: {:?}", checked.diags);
    let sig = w.sig(MAIN).expect("a signature");
    let mut ground = PortableTys::new_in(Alloc::global());
    let mut subst = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
    crate::build(&view, &checked, &sig, &mut subst, &TestAdts { world: w, body: b }, Global)
}

/// [`build_main`], which must build and verify.
fn built(w: &TestWorld, b: &BodyBuf, root: ExprId) -> Built<Global> {
    let built = build_main(w, b, root).unwrap_or_else(|e| panic!("the build failed: {e}"));
    blaze_lir::verify(built.func.view()).unwrap_or_else(|e| panic!("does not verify: {e}"));
    built
}

/// The item a function reference `r` names, through `Built::refs`.
fn callee(built: &Built<Global>, view: &View<'_>, r: RefId, text: &str) -> ItemKey {
    let rec = view.reference(r);
    assert!(matches!(RefKind::from_word(rec.kind), Some(RefKind::Func)), "{r} is not a function reference:\n{text}");
    built.refs.get(rec.place as usize).unwrap_or_else(|| panic!("{r} names place {} past `Built::refs`:\n{text}", rec.place)).item
}

/// The slot whose address `v` is: a drop's pointer must be a `slot_addr`.
fn slot_of(view: &View<'_>, v: Value, text: &str) -> u32 {
    let at = v.index();
    assert_eq!(view.op(at), Op::SlotAddr, "the drop's pointer {v} is not a slot's address:\n{text}");
    view.operands(at).slot().0
}

/// Reads every call and invoke of `built`, then each invoke's landing block: it must be a
/// `cleanup` block that only drops (plain calls of drop glue) and ends in `resume`, and every
/// `cleanup` block must be the landing block of exactly one invoke.
fn edges(built: &Built<Global>) -> Edges {
    let view = built.func.view();
    let text = blaze_lir::print(view);
    let mut landings: Vec<Landing, Global> = Vec::new_in(Global);
    let mut normal_drops: Vec<u32, Global> = Vec::new_in(Global);
    let mut callees: Vec<ItemKey, Global> = Vec::new_in(Global);
    for n in 0..view.blocks() {
        let block = Block(n as u32);
        let kind = view.block_kind(block);
        for at in view.block(block) {
            match view.op(at) {
                Op::Invoke => {
                    assert!(!kind.is_landing(), "an invoke in landing block {block} (a drop there is a plain call: walk/unwind.rs):\n{text}");
                    let mut o = view.operands(at);
                    let item = callee(built, &view, o.reference(), &text);
                    let _sig = o.sig();
                    let _normal = o.block();
                    let pad = o.block();
                    if item == DROP_IN_PLACE {
                        let p = o.values().get(0);
                        normal_drops.push(slot_of(&view, p, &text));
                    } else {
                        callees.push(item);
                    }
                    landings.push(Landing { callee: item, pad, drops: Vec::new_in(Global) });
                }
                Op::Call => {
                    let mut o = view.operands(at);
                    let item = callee(built, &view, o.reference(), &text);
                    let _sig = o.sig();
                    if item == DROP_IN_PLACE {
                        let p = o.values().get(0);
                        let slot = slot_of(&view, p, &text);
                        // A landing block's drops are read below, from its invoke.
                        if !kind.is_landing() {
                            normal_drops.push(slot);
                        }
                    } else {
                        assert!(!kind.is_landing(), "landing block {block} calls {item:?}, not drop glue:\n{text}");
                        callees.push(item);
                    }
                }
                Op::CallIndirect | Op::InvokeIndirect => panic!("these bodies make no call through a pointer, yet v{at} is one:\n{text}"),
                _ => {}
            }
        }
    }
    for l in landings.iter_mut() {
        assert_eq!(view.block_kind(l.pad), BlockKind::Cleanup, "the invoke of {:?} lands in {}, not a cleanup block:\n{text}", l.callee, l.pad);
        let range = view.block(l.pad);
        assert!(
            range.end > range.start && view.op(range.end - 1) == Op::Resume,
            "cleanup {} does not end in `resume` (a branching cleanup is not what these statically live values need):\n{text}",
            l.pad
        );
        for at in range {
            if view.op(at) == Op::Call {
                let mut o = view.operands(at);
                let item = callee(built, &view, o.reference(), &text);
                let _sig = o.sig();
                assert_eq!(item, DROP_IN_PLACE, "cleanup {} calls {item:?}, not drop glue:\n{text}", l.pad);
                let p = o.values().get(0);
                l.drops.push(slot_of(&view, p, &text));
            }
        }
    }
    for n in 0..view.blocks() {
        let block = Block(n as u32);
        if view.block_kind(block) == BlockKind::Cleanup {
            let from = landings.iter().filter(|l| l.pad == block).count();
            assert_eq!(from, 1, "cleanup {block} is the landing block of {from} invokes, not one:\n{text}");
        }
    }
    Edges { landings, normal_drops, callees, text }
}

/// Whether `built` has any unwind edge: an invoke or a landing block.
fn lands(built: &Built<Global>) -> bool {
    let view = built.func.view();
    let invokes = (0..view.len()).any(|at| matches!(view.op(at), Op::Invoke | Op::InvokeIndirect));
    let pads = (0..view.blocks()).any(|n| view.block_kind(Block(n as u32)).is_landing());
    invokes || pads
}

/// `fn main() { take(G { x: 1 }, f()); }` under `panic`, `take: fn(G, i32)`, `f: fn() -> i32`.
fn one_waiting_operand(panic: crate::Panic) -> Built<Global> {
    let mut w = TestWorld::new();
    w.panic = panic;
    w.func(1, "main", &[], UNIT);
    let g = g_world(&mut w);
    w.func(33, "f", &[], I32);
    w.func(34, "take", &[g, I32], UNIT);
    let mut b = BodyBuf::new();
    let s = g_lit(&mut b, 1);
    let fc = b.call("f", &[]);
    let call = b.call("take", &[s, fc]);
    let stmts = [Stmt::Expr { expr: call, semi: true }];
    let root = b.block(&stmts, None);
    built(&w, &b, root)
}

/// `take(G { x: 1 }, f())` under `Unwind`. While `f()` runs the `G` waits, owned by this frame:
/// if `f` unwinds the `G` is dropped ("dropping them has no effect unless one of the operands
/// ... panics"), so `f()` MUST be an invoke whose cleanup drops exactly that one slot and
/// resumes. When `take(..)` runs, the `G` has moved into it: the callee's to drop, so if `take`
/// lands at all its cleanup drops nothing; and nothing drops the `G` on the normal path. Under
/// `Abort` nothing unwinds: no invoke, no landing block. A walk that refuses (`NoCleanup`, the
/// state before the piece), leaves the operand out of `f`'s cleanup, or drops it again when
/// `take` unwinds or returns fails. Prints "operand_cleanup: f() lands dropping [S], take's
/// landings drop nothing, no drop on the normal path; none under abort".
#[test]
fn operand_cleanup_a_waiting_owned_operand_is_dropped_on_unwind() {
    let built = one_waiting_operand(crate::Panic::Unwind);
    let e = edges(&built);
    let text = &e.text;
    assert_eq!(e.calls(F), 1, "one call of f:\n{text}");
    assert_eq!(e.calls(TAKE), 1, "one call of take:\n{text}");
    let f = e.of(F);
    assert_eq!(f.len(), 1, "f() lands: the G waits, owned, while it runs:\n{text}");
    assert_eq!(f[0].drops.len(), 1, "f's cleanup drops the waiting G and nothing else: {:?}\n{text}", f[0].drops);
    for l in e.of(TAKE) {
        assert!(l.drops.is_empty(), "take's cleanup drops {:?}, but the G moved into take is the callee's:\n{text}", l.drops);
    }
    assert!(e.normal_drops.is_empty(), "the normal path drops {:?}, but the G moved into take:\n{text}", e.normal_drops);
    let abort = one_waiting_operand(crate::Panic::Abort);
    assert!(!lands(&abort), "no unwind edge under abort:\n{}", blaze_lir::print(abort.func.view()));
    println!("operand_cleanup: f() lands dropping {:?}, take's landings drop nothing, no drop on the normal path; none under abort", f[0].drops);
}

/// `fn main() { let a = G { x: 0 }; take2(G { x: 1 }, G { x: 2 }, f()); }` under `Unwind`,
/// `take2: fn(G, G, i32)`. `a`'s slot is the one the normal path drops, once, at the block's
/// end. The cleanup of `f()` drops three values in this order: the second operand, the first
/// operand (the tuple example: earlier operands in reverse order), then `a` (the local's scope
/// encloses the operands' temporary scope). The operands' places are slots made in evaluation
/// order (blaze-lir's `Builder::slot` appends a row), so the second operand's slot is the
/// larger. `take2(..)` runs with `a` live: if it unwinds `a` must drop, so it MUST be an invoke
/// too, and its cleanup drops `a` alone: both `G` operands moved into `take2` are the callee's.
/// A cleanup that drops the operands bottom first, drops `a` before them, misses one, or drops
/// a moved operand when `take2` unwinds fails. The old test's "one invoke, for f()" was wrong:
/// it took `take2` for a plain call while `a` is live across it. Prints "operand_cleanup: f()
/// drops [S2, S1, Sa], take2 drops [Sa]".
#[test]
fn operand_cleanup_drops_waiting_operands_top_first_then_locals() {
    let mut w = TestWorld::new();
    w.panic = crate::Panic::Unwind;
    w.func(1, "main", &[], UNIT);
    let g = g_world(&mut w);
    w.func(33, "f", &[], I32);
    w.func(36, "take2", &[g, g, I32], UNIT);
    let mut b = BodyBuf::new();
    let a = b.bind("a");
    let g0 = g_lit(&mut b, 0);
    let g1 = g_lit(&mut b, 1);
    let g2 = g_lit(&mut b, 2);
    let fc = b.call("f", &[]);
    let call = b.call("take2", &[g1, g2, fc]);
    let stmts = [Stmt::Let { pat: a, ty: None, init: Some(g0), else_: None }, Stmt::Expr { expr: call, semi: true }];
    let root = b.block(&stmts, None);
    let built = built(&w, &b, root);
    let e = edges(&built);
    let text = &e.text;
    assert_eq!(e.calls(F), 1, "one call of f:\n{text}");
    assert_eq!(e.calls(TAKE2), 1, "one call of take2:\n{text}");
    assert_eq!(e.normal_drops.len(), 1, "the normal path drops `a` once, at the block's end: {:?}\n{text}", e.normal_drops);
    let sa = e.normal_drops[0];
    let f = e.of(F);
    assert_eq!(f.len(), 1, "f() lands: two G operands and `a` are live while it runs:\n{text}");
    let d = &f[0].drops;
    assert_eq!(d.len(), 3, "f's cleanup drops both operands and `a`: {d:?}\n{text}");
    assert_eq!(d[2], sa, "f's cleanup drops `a` last, after the operands: {d:?}\n{text}");
    assert!(d[0] != sa && d[1] != sa && d[0] != d[1], "f's cleanup drops two operand slots besides `a`: {d:?}\n{text}");
    assert!(d[0] > d[1], "the second operand (the later slot) drops before the first: {d:?}\n{text}");
    let t = e.of(TAKE2);
    assert_eq!(t.len(), 1, "take2(..) lands: `a` is live while it runs:\n{text}");
    assert_eq!(t[0].drops.as_slice(), &[sa], "take2's cleanup drops `a` alone (its operands moved into it):\n{text}");
    println!("operand_cleanup: f() drops {d:?}, take2 drops {:?}", t[0].drops);
}

/// `fn main(v: &mut G) { poke(v, f()); }` under `Unwind`, `poke: fn(&mut G, i32)` (the
/// `v.push(f())` shape: the receiver is a borrow). A reference is not owned and implements no
/// `Drop`: nothing of this frame needs drop while `f()` or `poke(..)` runs, so whatever landing
/// blocks the build makes drop nothing, and no drop runs anywhere. A walk that refuses the
/// waiting pointer (`NoCleanup`, the state before the piece) or drops through it fails. Prints
/// "operand_cleanup: a waiting borrow joins no cleanup".
#[test]
fn operand_cleanup_a_waiting_borrow_is_not_dropped() {
    let mut w = TestWorld::new();
    w.panic = crate::Panic::Unwind;
    let g = g_world(&mut w);
    let r = w.ref_(Mutbl::Mut, g);
    w.func(1, "main", &[r], UNIT);
    w.func(33, "f", &[], I32);
    w.func(37, "poke", &[r, I32], UNIT);
    let mut b = BodyBuf::new();
    let v = b.bind("v");
    b.params.push(v);
    let ve = b.local(v);
    let fc = b.call("f", &[]);
    let call = b.call("poke", &[ve, fc]);
    let stmts = [Stmt::Expr { expr: call, semi: true }];
    let root = b.block(&stmts, None);
    let built = built(&w, &b, root);
    let e = edges(&built);
    let text = &e.text;
    assert_eq!(e.calls(F), 1, "one call of f:\n{text}");
    assert_eq!(e.calls(POKE), 1, "one call of poke:\n{text}");
    for l in &e.landings {
        assert!(l.drops.is_empty(), "the cleanup of {:?} drops {:?}, but only a borrow waits:\n{text}", l.callee, l.drops);
    }
    assert!(e.normal_drops.is_empty(), "nothing of this frame needs drop, yet {:?} drop:\n{text}", e.normal_drops);
    println!("operand_cleanup: a waiting borrow joins no cleanup");
}

/// `fn main() { let h = H { g: G { x: 1 }, n: f() }; }`, `struct H { g: G, n: i32 }`. While
/// `f()` runs, `h`'s place holds `g` but not yet `n`: dropping the whole `H` would read `n`'s
/// uninitialized bytes, dropping nothing would skip `g`'s destructor (the Reference,
/// "Destructors": a struct drops its fields), so under `Unwind` the walk cannot make an exact
/// cleanup and refuses by name (walk/val_stack.rs: `BuildError::NoCleanup`, "an aggregate
/// literal being built whose fields need drop"). Under `Abort` nothing unwinds and it builds.
/// A walk that builds a cleanup for the half-built literal, or refuses some other way, fails.
/// Prints "operand_cleanup: the half-built literal is refused by name under unwind, built under
/// abort".
#[test]
fn operand_cleanup_a_half_built_literal_is_refused_by_name() {
    let half_built = |panic: crate::Panic| {
        let mut w = TestWorld::new();
        w.panic = panic;
        w.func(1, "main", &[], UNIT);
        let g = g_world(&mut w);
        w.strukt(10, "H", &[("g", g), ("n", I32)]);
        w.func(33, "f", &[], I32);
        let mut b = BodyBuf::new();
        let h = b.bind("h");
        let gl = g_lit(&mut b, 1);
        let fc = b.call("f", &[]);
        let path = b.path("H");
        let start = b.field_inits.len() as u32;
        b.field_inits.push(FieldInit { name: Name::of("g"), expr: gl });
        b.field_inits.push(FieldInit { name: Name::of("n"), expr: fc });
        let lit = b.e(Expr::Struct { path, fields: List { start, len: 2 }, base: blaze_typeck::StructBase::None });
        let stmts = [Stmt::Let { pat: h, ty: None, init: Some(lit), else_: None }];
        let root = b.block(&stmts, None);
        build_main(&w, &b, root)
    };
    let what = match half_built(crate::Panic::Unwind) {
        Ok(built) => panic!("built a cleanup for a half-built `H`:\n{}", blaze_lir::print(built.func.view())),
        Err(BuildError::NoCleanup { what }) => what,
        Err(other) => panic!("refused, but not as `NoCleanup`: {other}"),
    };
    assert!(what.contains("aggregate literal being built"), "the refusal does not name the half-built literal: {what}");
    let abort = half_built(crate::Panic::Abort).unwrap_or_else(|e| panic!("under abort the build failed: {e}"));
    blaze_lir::verify(abort.func.view()).unwrap_or_else(|e| panic!("does not verify: {e}"));
    assert!(!lands(&abort), "no unwind edge under abort:\n{}", blaze_lir::print(abort.func.view()));
    println!("operand_cleanup: the half-built literal is refused by name under unwind, built under abort");
}
