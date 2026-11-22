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
    /// Row for row with `drops`: each drop's slot, offset and type.
    parts: Vec<PartDrop, Global>,
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
    let mut ground = PortableTys::new_in(Alloc::global());
    build_main_in(w, b, root, &mut ground)
}

/// [`build_main`] over the ground table `ground`, kept so a test can name a drop's type: the
/// argument list of `drop_in_place::<T>` for a `T` interned again is the one the build made.
fn build_main_in(w: &TestWorld, b: &BodyBuf, root: ExprId, ground: &mut PortableTys<'static>) -> Result<Built<Global>, BuildError> {
    let view = b.view(MAIN, root);
    let mut cx = InferCtx::new_in(Alloc::global());
    let checked = match blaze_typeck::check(&view, w, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        other => panic!("the check did not finish: {:?}", other.err()),
    };
    assert!(checked.diags.is_empty(), "diagnostics: {:?}", checked.diags);
    let sig = w.sig(MAIN).expect("a signature");
    let mut subst = Subst { args: &[], ground, scratch: Alloc::global() };
    crate::build(&view, &checked, &sig, &mut subst, &TestAdts { world: w, body: b }, Global)
}

/// The argument list of `drop_in_place::<t>` in `ground`.
fn glue_args(ground: &mut PortableTys<'static>, t: PNode) -> List {
    let ty = ground.intern(t).expect("a type interns");
    ground.intern_list(&[ty]).expect("a list interns")
}

/// [`build_main`], which must build and verify.
fn built(w: &TestWorld, b: &BodyBuf, root: ExprId) -> Built<Global> {
    let built = build_main(w, b, root).unwrap_or_else(|e| panic!("the build failed: {e}"));
    blaze_lir::verify(built.func.view()).unwrap_or_else(|e| panic!("does not verify: {e}"));
    built
}

/// The instance a function reference `r` names, through `Built::refs`.
fn instance(built: &Built<Global>, view: &View<'_>, r: RefId, text: &str) -> Instance {
    let rec = view.reference(r);
    assert!(matches!(RefKind::from_word(rec.kind), Some(RefKind::Func)), "{r} is not a function reference:\n{text}");
    *built.refs.get(rec.place as usize).unwrap_or_else(|| panic!("{r} names place {} past `Built::refs`:\n{text}", rec.place))
}

/// The item a function reference `r` names, through `Built::refs`.
fn callee(built: &Built<Global>, view: &View<'_>, r: RefId, text: &str) -> ItemKey {
    instance(built, view, r, text).item
}

/// The slot whose address `v` is: a drop's pointer must be a `slot_addr`, or a `ptr_add` of
/// one and a constant (a part of an aggregate: [`place_of`]).
fn slot_of(view: &View<'_>, v: Value, text: &str) -> u32 {
    place_of(view, v, text).0
}

/// The slot and byte offset whose address `v` is: a `slot_addr`, or a `ptr_add` of one and an
/// `iconst`.
fn place_of(view: &View<'_>, v: Value, text: &str) -> (u32, u32) {
    let at = v.index();
    match view.op(at) {
        Op::SlotAddr => (view.operands(at).slot().0, 0),
        Op::PtrAdd => {
            let mut o = view.operands(at);
            let (base, by) = (o.value(), o.value());
            assert_eq!(view.op(base.index()), Op::SlotAddr, "the drop's pointer {v} adds to {base}, not a slot's address:\n{text}");
            assert_eq!(view.op(by.index()), Op::Iconst, "the drop's pointer {v} adds {by}, not a constant:\n{text}");
            (view.operands(base.index()).slot().0, view.operands(by.index()).imm())
        }
        other => panic!("the drop's pointer {v} is a {other:?}, not a slot's address or a part of one:\n{text}"),
    }
}

/// One drop a landing block calls: the slot and offset dropped, and the glue's argument list
/// (its type, compared with [`glue_args`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct PartDrop {
    slot: u32,
    off: u32,
    ty: List,
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
                    landings.push(Landing { callee: item, pad, drops: Vec::new_in(Global), parts: Vec::new_in(Global) });
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
                let Instance { item, args } = instance(built, &view, o.reference(), &text);
                let _sig = o.sig();
                assert_eq!(item, DROP_IN_PLACE, "cleanup {} calls {item:?}, not drop glue:\n{text}", l.pad);
                let p = o.values().get(0);
                let (slot, off) = place_of(&view, p, &text);
                l.drops.push(slot);
                l.parts.push(PartDrop { slot, off, ty: args });
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

// Aggregate literals being built (walk/val_stack.rs, `Owns::Parts`). The Reference,
// "Destructors" (temporaries): an aggregate's operands are temporaries until it is made, and
// when a later operand panics the earlier ones are dropped in reverse order (the tuple example
// above). A literal whose place is half written owns exactly the parts stored so far: dropping
// the whole would read the unwritten parts' bytes (and run the type's own `Drop`, which is only
// for a value that was made), dropping nothing would skip the stored parts' destructors. bstd's
// `checked` (`Run { table, limits, heap, later: UnsafeCell::new(..), made: Cell::new(false) }`)
// is this shape; before the fix every such literal was refused with "an operand evaluated before
// this call waits on the value stack and the walk could not ground what of it is owned (an
// aggregate literal being built whose fields need drop, ..)".

/// `struct K { x: i32 }`, with a `Drop` impl of its own (keyed `H_DROP`) beside `G`'s.
const K_KEY: u128 = 11;
/// `struct S { g: G, k: K, n: i32 }`.
const S_KEY: u128 = 12;
/// `enum E3 { V(G, K, i32) }`; its variant `V` (key 121) is its ctor fn.
const E3_KEY: u128 = 120;

/// The literal tests' world under `panic`: `main: fn()`, `G`, `K`, `S`, `E3` and
/// `f: fn() -> i32`; gives `G` and `K`.
fn literal_world(panic: crate::Panic) -> (TestWorld, PNode, PNode) {
    let mut w = TestWorld::new();
    w.panic = panic;
    w.func(1, "main", &[], UNIT);
    let g = g_world(&mut w);
    w.strukt(K_KEY, "K", &[("x", I32)]);
    w.drops.push((ItemKey(K_KEY), H_DROP));
    let k = PNode::Adt(ItemKey(K_KEY), List::EMPTY);
    w.strukt(S_KEY, "S", &[("g", g), ("k", k), ("n", I32)]);
    w.enum_(E3_KEY, &[("V", CtorKind::Tuple, &[("0", g), ("1", k), ("2", I32)])]);
    w.func(33, "f", &[], I32);
    (w, g, k)
}

/// `K { x: v }`.
fn k_lit(b: &mut BodyBuf, v: u128) -> ExprId {
    let x = b.int(v, None);
    struct_lit(b, "K", &[("x", x)], None)
}

/// `name { fields.. }`, with `..base` when given.
fn struct_lit(b: &mut BodyBuf, name: &str, fields: &[(&str, ExprId)], base: Option<ExprId>) -> ExprId {
    let path = b.path(name);
    let start = b.field_inits.len() as u32;
    for &(n, expr) in fields {
        b.field_inits.push(FieldInit { name: Name::of(n), expr });
    }
    let base = match base {
        Some(x) => blaze_typeck::StructBase::Expr(x),
        None => blaze_typeck::StructBase::None,
    };
    b.e(Expr::Struct { path, fields: List { start, len: fields.len() as u32 }, base })
}

/// `main` from `root` in `w`, built and verified, with the ground table it built over.
fn built_from(w: &TestWorld, b: &BodyBuf, root: ExprId) -> Result<(Built<Global>, PortableTys<'static>), BuildError> {
    let mut ground = PortableTys::new_in(Alloc::global());
    let built = build_main_in(w, b, root, &mut ground)?;
    blaze_lir::verify(built.func.view()).unwrap_or_else(|e| panic!("does not verify: {e}"));
    Ok((built, ground))
}

/// The drops `p` as `[slot+offset, ..]`, for the printed line.
fn places(p: &[PartDrop]) -> String {
    let mut s = String::from("[");
    for (i, d) in p.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&format!("s{}+{}", d.slot, d.off));
    }
    s.push(']');
    s
}

/// The literal forms that build their place part by part.
#[derive(Clone, Copy, Debug)]
enum LitForm {
    /// `S { g: G{1}, k: K{2}, n: f() }`
    Struct,
    /// `(G{1}, K{2}, f())`
    Tuple,
    /// `V(G{1}, K{2}, f())` (an enum variant's ctor: walk.rs `construct`)
    Ctor,
}

/// `fn main() { let x = <form>; }` built under `panic`, with its ground table.
fn half_built(form: LitForm, panic: crate::Panic) -> Result<(Built<Global>, PortableTys<'static>, PNode, PNode), BuildError> {
    let (w, g, k) = literal_world(panic);
    let mut b = BodyBuf::new();
    let x = b.bind("x");
    let gl = g_lit(&mut b, 1);
    let kl = k_lit(&mut b, 2);
    let fc = b.call("f", &[]);
    let lit = match form {
        LitForm::Struct => struct_lit(&mut b, "S", &[("g", gl), ("k", kl), ("n", fc)], None),
        LitForm::Tuple => b.tuple(&[gl, kl, fc]),
        LitForm::Ctor => b.call("V", &[gl, kl, fc]),
    };
    let stmts = [Stmt::Let { pat: x, ty: None, init: Some(lit), else_: None }];
    let root = b.block(&stmts, None);
    let (built, ground) = built_from(&w, &b, root)?;
    Ok((built, ground, g, k))
}

/// `let x = S { g: G{1}, k: K{2}, n: f() };`, and its tuple and enum-ctor forms, under
/// `Unwind`. While `f()` runs, `x`'s place holds `g` and `k` but not `n`: `f()` MUST be an
/// invoke whose cleanup drops exactly the two stored parts, `K` (stored last) first, then `G`,
/// each at its own offset in the one literal's place, and nothing else (never the whole `S`,
/// whose `n` is unwritten). The normal path drops `x` once. Under `Abort` each builds with no
/// unwind edge. A walk that refuses (the state before the fix), drops the whole literal, drops
/// the parts first-stored first, or misses one fails. Prints "operand_cleanup: a half-built
/// literal drops its stored parts, last first: Struct [..] Tuple [..] Ctor [..]".
#[test]
fn operand_cleanup_a_half_built_literal_drops_its_stored_parts_last_first() {
    let mut seen = String::new();
    for form in [LitForm::Struct, LitForm::Tuple, LitForm::Ctor] {
        let (built, mut ground, g, k) = half_built(form, crate::Panic::Unwind).unwrap_or_else(|e| panic!("{form:?}: the build failed: {e}"));
        let (gt, kt) = (glue_args(&mut ground, g), glue_args(&mut ground, k));
        let e = edges(&built);
        let text = &e.text;
        assert_eq!(e.calls(F), 1, "{form:?}: one call of f:\n{text}");
        let f = e.of(F);
        assert_eq!(f.len(), 1, "{form:?}: f() lands: G and K are stored in the literal while it runs:\n{text}");
        let p = &f[0].parts;
        assert_eq!(p.len(), 2, "{form:?}: f's cleanup drops the two stored parts and nothing else: {p:?}\n{text}");
        assert_eq!((p[0].ty, p[1].ty), (kt, gt), "{form:?}: K (stored last) drops first, then G: {p:?}\n{text}");
        assert_eq!(p[0].slot, p[1].slot, "{form:?}: both parts are of the one literal's place: {p:?}\n{text}");
        assert_ne!(p[0].off, p[1].off, "{form:?}: each part at its own offset: {p:?}\n{text}");
        assert_eq!(e.normal_drops.len(), 1, "{form:?}: the normal path drops `x` once: {:?}\n{text}", e.normal_drops);
        let (abort, _, _, _) = half_built(form, crate::Panic::Abort).unwrap_or_else(|e| panic!("{form:?}: under abort the build failed: {e}"));
        assert!(!lands(&abort), "{form:?}: no unwind edge under abort:\n{}", blaze_lir::print(abort.func.view()));
        seen.push_str(&format!(" {form:?} {}", places(p)));
    }
    println!("operand_cleanup: a half-built literal drops its stored parts, last first:{seen}");
}

/// `fn main() { let a = G{8}; let t = (G{1}, (K{2}, f()), f()); }` under `Unwind`: literals
/// nest, and a finished one is one value. At the first `f()` two literals wait: the outer with
/// `G{1}` stored, the inner (its own place) with `K{2}`; its cleanup drops the inner's `K`, then
/// the outer's `G`, then the local `a` (the operands' scope is inside the block's). At the
/// second `f()` the inner tuple is made and stored into the outer: the cleanup drops it as one
/// `(K, i32)` (its own type: neither `G` nor `K`) at the outer's place, then the outer's `G`
/// (at the same place and offset as before), then `a`. A walk that keeps the inner literal's
/// parts after it is made (dropping its `K` twice), loses the outer's `G` under the inner, or
/// drops the outer whole fails. Prints "operand_cleanup: nested literals: first f() drops [..],
/// second f() drops [..]".
#[test]
fn operand_cleanup_nested_literals_drop_each_part_once_inner_first() {
    let (w, g, k) = literal_world(crate::Panic::Unwind);
    let mut b = BodyBuf::new();
    let a = b.bind("a");
    let g8 = g_lit(&mut b, 8);
    let t = b.bind("t");
    let g1 = g_lit(&mut b, 1);
    let k2 = k_lit(&mut b, 2);
    let f1 = b.call("f", &[]);
    let inner = b.tuple(&[k2, f1]);
    let f2 = b.call("f", &[]);
    let outer = b.tuple(&[g1, inner, f2]);
    let stmts = [Stmt::Let { pat: a, ty: None, init: Some(g8), else_: None }, Stmt::Let { pat: t, ty: None, init: Some(outer), else_: None }];
    let root = b.block(&stmts, None);
    let (built, mut ground) = built_from(&w, &b, root).unwrap_or_else(|e| panic!("the build failed: {e}"));
    let (gt, kt) = (glue_args(&mut ground, g), glue_args(&mut ground, k));
    let e = edges(&built);
    let text = &e.text;
    assert_eq!(e.calls(F), 2, "two calls of f:\n{text}");
    assert_eq!(e.normal_drops.len(), 2, "the normal path drops `t` then `a`, once each: {:?}\n{text}", e.normal_drops);
    let sa = e.normal_drops[1];
    let f = e.of(F);
    assert_eq!(f.len(), 2, "both f() calls land:\n{text}");
    let (p1, p2) = (&f[0].parts, &f[1].parts);
    assert_eq!(p1.len(), 3, "the first f()'s cleanup drops the inner K, the outer G and `a`: {p1:?}\n{text}");
    assert_eq!((p1[0].ty, p1[1].ty, p1[2].ty), (kt, gt, gt), "the first f(): K, then G, then `a`: {p1:?}\n{text}");
    assert_ne!(p1[0].slot, p1[1].slot, "the inner literal has a place of its own: {p1:?}\n{text}");
    assert_eq!(p1[2].slot, sa, "the first f()'s cleanup drops `a` last: {p1:?}\n{text}");
    assert_eq!(p2.len(), 3, "the second f()'s cleanup drops the made inner tuple, the G and `a`: {p2:?}\n{text}");
    assert!(p2[0].ty != gt && p2[0].ty != kt, "the made inner tuple drops as one `(K, i32)`, not a part of it: {p2:?}\n{text}");
    assert_eq!(p2[0].slot, p2[1].slot, "the inner tuple is stored in the outer's place: {p2:?}\n{text}");
    assert_eq!((p2[1].ty, p2[1].slot, p2[1].off), (gt, p1[1].slot, p1[1].off), "the outer's G, as at the first f(): {p2:?}\n{text}");
    assert_eq!(p2[2].slot, sa, "the second f()'s cleanup drops `a` last: {p2:?}\n{text}");
    println!("operand_cleanup: nested literals: first f() drops {}, second f() drops {}", places(p1), places(p2));
}

/// `fn main() { let s0 = S { g: G{7}, k: K{8}, n: 0 }; let s = S { g: G{1}, k: K{2}, n: f(), ..s0 }; }`
/// under `Unwind` (walk/moves/struct_update.rs; every field written, so the base gives none and
/// `s0` keeps no field flags, and its drop is one straight call). While `f()` runs the new
/// value holds `g` and `k`, and `s0` is whole: the cleanup drops the new value's `K`, then its
/// `G`, then `s0` as one `S`. A walk that refuses, drops the new value whole or leaves a part
/// fails. Prints "operand_cleanup: struct update: f() drops [..]".
#[test]
fn operand_cleanup_a_half_built_struct_update_drops_its_written_parts() {
    let (w, g, k) = literal_world(crate::Panic::Unwind);
    let mut b = BodyBuf::new();
    let s0 = b.bind("s0");
    let g7 = g_lit(&mut b, 7);
    let k8 = k_lit(&mut b, 8);
    let zero = b.int(0, None);
    let s0_init = struct_lit(&mut b, "S", &[("g", g7), ("k", k8), ("n", zero)], None);
    let s = b.bind("s");
    let g1 = g_lit(&mut b, 1);
    let k2 = k_lit(&mut b, 2);
    let fc = b.call("f", &[]);
    let base = b.local(s0);
    let update = struct_lit(&mut b, "S", &[("g", g1), ("k", k2), ("n", fc)], Some(base));
    let stmts = [Stmt::Let { pat: s0, ty: None, init: Some(s0_init), else_: None }, Stmt::Let { pat: s, ty: None, init: Some(update), else_: None }];
    let root = b.block(&stmts, None);
    let (built, mut ground) = built_from(&w, &b, root).unwrap_or_else(|e| panic!("the build failed: {e}"));
    let (gt, kt) = (glue_args(&mut ground, g), glue_args(&mut ground, k));
    let st = glue_args(&mut ground, PNode::Adt(ItemKey(S_KEY), List::EMPTY));
    let e = edges(&built);
    let text = &e.text;
    let f = e.of(F);
    assert_eq!(f.len(), 1, "f() lands: the new value's parts and `s0` are live while it runs:\n{text}");
    let p = &f[0].parts;
    assert_eq!(p.len(), 3, "f's cleanup drops the written K and G, and `s0`: {p:?}\n{text}");
    assert_eq!((p[0].ty, p[1].ty, p[2].ty), (kt, gt, st), "the new value's K, its G, then `s0` whole: {p:?}\n{text}");
    assert_eq!(p[0].slot, p[1].slot, "both parts of the new value's place: {p:?}\n{text}");
    assert_ne!(p[1].slot, p[2].slot, "the new value has a place of its own: {p:?}\n{text}");
    println!("operand_cleanup: struct update: f() drops {}", places(p));
}
