//! Closures built and run natively (codegen.md, "Closures and the Fn traits"). Each test's
//! program is written in its doc; each rule it checks is quoted from the Reference or core.
//!
//! ```text
//! #[lang = "sized"] trait Sized {}
//! #[lang = "fn_once"] trait FnOnce<Args> { type Output; extern "rust-call" fn call_once(self, args: Args) -> Self::Output; }
//! #[lang = "fn_mut"] trait FnMut<Args>: FnOnce<Args> { extern "rust-call" fn call_mut(&mut self, args: Args) -> Self::Output; }
//! #[lang = "fn"] trait Fn<Args>: FnMut<Args> { extern "rust-call" fn call(&self, args: Args) -> Self::Output; }
//! fn main() -> u64;
//! ```

use std::alloc::Global;
use std::vec::Vec as StdVec;

use blaze_own::Alloc;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{
    Abi as TAbi, Adt, BinOp, Expr, ExprId, FnPtrHeader, InferCtx, ItemKey, LangItem, List, Mutbl, Needs, Outcome, PNode, PTy, PTys, PatId, PortableTys,
    Safety, StmtId, UintTy,
};

use super::run_native_with;
use crate::layout::{ClosureFacts, LayoutFacts, Missing, ReprAttrs, ValidRange};
use crate::{Abi, Adts, Built, FnTrait, Subst};

struct World {
    fx: Fx,
    main: ItemKey,
    /// `Fn::call`, `FnMut::call_mut`, `FnOnce::call_once`.
    methods: [ItemKey; 3],
    /// `Fn`, `FnMut`, `FnOnce`.
    traits: [ItemKey; 3],
}

fn world() -> World {
    let mut w = open_world();
    w.fx.finish();
    w
}

/// The world before `finish`: a test adds its own items.
fn open_world() -> World {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, p1) = (fx.param(0), fx.param(1));
    let rust_call = FnPtrHeader { abi: TAbi::RustCall, safety: Safety::Safe };
    let fn_once = fx.tr("FnOnce", 2, Vec::new(), &["Output"]);
    fx.lang(LangItem::FnOnce, fn_once);
    let out = fx.alias(fn_once, "Output", &[p0, p1]);
    let call_once = fx.trait_fn(fn_once, "call_once", true, &[p0, p1], out);
    fx.set_header(call_once, rust_call);
    let sup = fx.bound(fn_once, &[p0, p1]);
    let fn_mut = fx.tr("FnMut", 2, std::vec![sup], &[]);
    fx.lang(LangItem::FnMut, fn_mut);
    let m_self = fx.r(Mutbl::Mut, p0);
    let call_mut = fx.trait_fn(fn_mut, "call_mut", true, &[m_self, p1], out);
    fx.set_header(call_mut, rust_call);
    let sup = fx.bound(fn_mut, &[p0, p1]);
    let fn_ = fx.tr("Fn", 2, std::vec![sup], &[]);
    fx.lang(LangItem::Fn, fn_);
    let r_self = fx.r(Mutbl::Not, p0);
    let call = fx.trait_fn(fn_, "call", true, &[r_self, p1], out);
    fx.set_header(call, rust_call);
    let u64_ = fx.ty(PNode::Uint(UintTy::U64));
    let main = fx.func("main", 0, &[], u64_, Vec::new());
    World { fx, main, methods: [call, call_mut, call_once], traits: [fn_, fn_mut, fn_once] }
}

/// One fn item's body: its item, its builder, its root and its literals.
struct Prog<'a> {
    item: ItemKey,
    b: &'a Bb,
    root: ExprId,
    lits: &'a [(ExprId, u128)],
}

struct FxAdts<'a> {
    w: &'a World,
    progs: &'a [Prog<'a>],
    /// Each closure's facts, (owner, ordinal, kind, sig, env) of `table`: the seam's answer
    /// made from the owner's check record (`layout::closure_facts`).
    closures: &'a [(ItemKey, u32, FnTrait, PTy, PTy)],
    table: PTys<'a>,
}

impl LayoutFacts for FxAdts<'_> {
    fn closure(&self, owner: ItemKey, ordinal: u32) -> Option<ClosureFacts<'_>> {
        let &(_, _, kind, sig, env) = self.closures.iter().find(|r| r.0 == owner && r.1 == ordinal)?;
        Some(ClosureFacts { types: self.table, kind, sig, env })
    }

    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.w.fx.adt_of(key)
    }

    fn repr(&self, _: ItemKey) -> ReprAttrs {
        ReprAttrs::RUST
    }

    fn discriminant(&self, _: ItemKey, _: u32) -> Result<Option<i128>, Missing> {
        Ok(None)
    }

    fn valid_range(&self, _: ItemKey) -> ValidRange {
        ValidRange::default()
    }
}

impl Adts for FxAdts<'_> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.w.fx.adt_of(key)
    }

    fn facts(&self) -> &dyn LayoutFacts {
        self
    }

    fn int_lit(&self, item: ItemKey, expr: ExprId) -> Option<u128> {
        let p = self.progs.iter().find(|p| p.item == item)?;
        p.lits.iter().find(|(e, _)| *e == expr).map(|&(_, v)| v)
    }

    fn abi(&self, item: ItemKey) -> Option<Abi> {
        if self.w.methods.contains(&item) {
            return Some(Abi::RustCall);
        }
        self.w.fx.sig_of(item).map(|_| Abi::Rust)
    }

    fn drop_impl(&self, _: PTy, _: &mut PortableTys<'_>) -> Result<Option<ItemKey>, crate::BuildError> {
        Ok(None)
    }

    fn fn_trait_method(&self, which: FnTrait) -> Option<ItemKey> {
        Some(self.w.methods[which as usize])
    }
}

/// `lhs op rhs`.
fn bin(b: &mut Bb, op: BinOp, lhs: ExprId, rhs: ExprId) -> ExprId {
    let at = b.int(None);
    b.exprs[at.ix()] = Expr::Binary { op, lhs, rhs };
    at
}

/// `lhs = rhs`.
fn assign(b: &mut Bb, lhs: ExprId, rhs: ExprId) -> ExprId {
    let at = b.int(None);
    b.exprs[at.ix()] = Expr::Assign { lhs, rhs };
    at
}

/// An integer literal of value `v`, noted in `lits`.
fn lit(b: &mut Bb, lits: &mut StdVec<(ExprId, u128)>, v: u128) -> ExprId {
    let e = b.int(None);
    lits.push((e, v));
    e
}

/// `let <name>: u64 = <v>;`
fn let_u64(b: &mut Bb, lits: &mut StdVec<(ExprId, u128)>, name: &str, v: u128) -> (StmtId, PatId) {
    let p = b.bind(name);
    let e = lit(b, lits, v);
    let t = b.ty(PNode::Uint(UintTy::U64));
    (b.let_(p, Some(t), e), p)
}

/// `let v: u64 = x;` inside a closure's body: fixes the unannotated parameter's type before an
/// operator meets it (the fixture writes closure parameters unannotated). Returns the
/// statement and a use of `v`.
fn param_u64(b: &mut Bb, x: PatId) -> (StmtId, ExprId) {
    let v = b.bind("v");
    let xe = b.local(x);
    let t = b.ty(PNode::Uint(UintTy::U64));
    let s = b.let_(v, Some(t), xe);
    (s, b.local(v))
}

/// Checks body `b` of `main`, builds it and, by a work list, every closure body its calls
/// reach (the instance of `Fn::call`/`call_mut`/`call_once` at a closure type), then runs
/// `main` natively. Returns its value and the number of closure bodies built.
fn run(w: &World, b: &Bb, root: ExprId, lits: &[(ExprId, u128)]) -> (u64, usize) {
    run_progs(w, &[Prog { item: w.main, b, root, lits }])
}

/// [`run`] over several bodies, `main`'s first: every instance the calls reach is built (a
/// generic fn at its ground arguments, a closure's `Fn*` method by its ordinal, the trait
/// method its item names). Returns `main`'s value and the number of closure methods built.
fn run_progs(w: &World, progs: &[Prog<'_>]) -> (u64, usize) {
    let views: StdVec<_> = progs.iter().map(|p| p.b.view(p.item, p.root)).collect();
    let checked: StdVec<_> = views
        .iter()
        .map(|view| {
            let mut ty = InferCtx::new_in(Alloc::global());
            let c = match blaze_typeck::check(view, &w.fx, &mut ty, &Needs::new_in(Alloc::global()), Alloc::global()) {
                Ok(Outcome::Done(c)) => c,
                Ok(Outcome::Need(n)) => panic!("the check needed {:?}", n.questions),
                Err(error) => panic!("the check failed: {error:?}"),
            };
            assert!(c.diags.is_empty(), "diagnostics: {:?}", c.diags);
            c
        })
        .collect();
    let mut table = PortableTys::new_in(Alloc::global());
    let mut rows = StdVec::new();
    for (p, c) in progs.iter().zip(checked.iter()) {
        let s = w.fx.sig_of(p.item).expect("the fn's signature");
        for ordinal in 0..c.closures.len() as u32 {
            let (kind, sig, env) = crate::layout::closure_facts(c, ordinal, s.parent_count + s.own_count, &mut table, Alloc::global())
                .unwrap_or_else(|e| panic!("closure {ordinal}'s facts: {e}"));
            rows.push((p.item, ordinal, kind, sig, env));
        }
    }
    let mut ground = PortableTys::new_in(Alloc::global());
    let adts = FxAdts { w, progs, closures: &rows, table: table.view() };
    let sig = w.fx.sig_of(w.main).expect("main's signature");
    let mut subst = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
    let main = crate::build(&views[0], &checked[0], &sig, &mut subst, &adts, Global).unwrap_or_else(|e| panic!("the build of main failed: {e}"));
    super::emits(&main, &[]);
    let mut bodies: StdVec<(ItemKey, List, Built<Global>)> = StdVec::new();
    let mut closures = 0;
    let mut todo: StdVec<(ItemKey, List)> = main.refs.iter().map(|r| (r.item, r.args)).collect();
    while let Some((item, args)) = todo.pop() {
        if bodies.iter().any(|(i, a, _)| *i == item && *a == args) {
            continue;
        }
        let built = if let Some(m) = w.methods.iter().position(|m| *m == item) {
            let first = ground.view().list(args).expect("the instance's arguments")[0];
            let c = crate::closure_of(ground.view(), first).unwrap_or_else(|| panic!("{item:?} at a type that is not a closure"));
            let at = progs.iter().position(|p| p.item == c.owner).expect("the closure's owner among the bodies");
            let which = [FnTrait::Fn, FnTrait::FnMut, FnTrait::FnOnce][m];
            let oargs = ground.view().list(c.owner_args).expect("the owner's arguments").to_vec();
            let sig = w.fx.sig_of(c.owner).expect("the owner's signature");
            let mut subst = Subst { args: &oargs, ground: &mut ground, scratch: Alloc::global() };
            closures += 1;
            crate::build_closure(&views[at], &checked[at], c.ordinal, which, &sig, &mut subst, &adts, Global)
                .unwrap_or_else(|e| panic!("the closure's {which:?} method failed: {e}"))
        } else if let Some(at) = progs.iter().position(|p| p.item == item) {
            let gargs = ground.view().list(args).expect("the instance's arguments").to_vec();
            let sig = w.fx.sig_of(item).expect("the fn's signature");
            let mut subst = Subst { args: &gargs, ground: &mut ground, scratch: Alloc::global() };
            crate::build(&views[at], &checked[at], &sig, &mut subst, &adts, Global).unwrap_or_else(|e| panic!("the build of {item:?} failed: {e}"))
        } else {
            continue;
        };
        super::emits(&built, &[]);
        todo.extend(built.refs.iter().map(|r| (r.item, r.args)));
        bodies.push((item, args, built));
    }
    // Callees before callers: each body was found by a caller built before it.
    let mut fns: StdVec<(ItemKey, Option<List>, &Built<Global>)> = bodies.iter().rev().map(|(i, a, b)| (*i, Some(*a), b)).collect();
    fns.push((w.main, None, &main));
    (run_native_with(&[], &fns), closures)
}

/// The Reference, "Closure types": "A closure expression produces a closure value with a
/// unique, anonymous type"; "Call expressions": a closure operand is called through its `Fn*`
/// impl. `fn main() -> u64 { let f = |x| x + 1; let a: u64 = 2; f(a) }` is 3: the closure
/// captures nothing (its environment is zero-sized) and its body is one function.
#[test]
fn a_closure_that_captures_nothing_is_called_directly() {
    let w = world();
    let mut b = Bb::new();
    let mut lits = StdVec::new();
    let x = b.bind("x");
    let (sv, xe) = param_u64(&mut b, x);
    let one = lit(&mut b, &mut lits, 1);
    let sum = bin(&mut b, BinOp::Add, xe, one);
    let body = b.block(&[sv], Some(sum));
    let clo = b.closure(&[x], body);
    let f = b.bind("f");
    let s0 = b.let_(f, None, clo);
    let (s1, a) = let_u64(&mut b, &mut lits, "a", 2);
    let fe = b.local(f);
    let ae = b.local(a);
    let call = b.call(fe, &[ae]);
    let root = b.block(&[s0, s1], Some(call));
    assert_eq!(run(&w, &b, root, &lits), (3, 1));
}

/// `fn apply<F: <tr>(u64) -> u64>(f: F, x: u64) -> u64 { f(x); ..; f(x) }`, `f` called `calls`
/// times, added to `w` (still open): `tr` is 0 `Fn`, 1 `FnMut`, 2 `FnOnce`. Returns its item,
/// builder and root.
fn apply(w: &mut World, tr: usize, calls: usize) -> (ItemKey, Bb, ExprId) {
    let u64_ = w.fx.ty(PNode::Uint(UintTy::U64));
    let p0 = w.fx.param(0);
    let args = w.fx.tuple(&[u64_]);
    let bound = w.fx.bound(w.traits[tr], &[p0, args]);
    let out = w.fx.proj(w.traits[2], &[p0, args], "Output", u64_);
    let item = w.fx.func("apply", 1, &[p0, u64_], u64_, std::vec![bound, out]);
    let mut b = Bb::new();
    let (f, x) = (b.param("f"), b.param("x"));
    let mut stmts = StdVec::new();
    for _ in 1..calls {
        let (fe, xe) = (b.local(f), b.local(x));
        let c = b.call(fe, &[xe]);
        stmts.push(b.semi(c));
    }
    let (fe, xe) = (b.local(f), b.local(x));
    let c = b.call(fe, &[xe]);
    let root = b.block(&stmts, Some(c));
    (item, b, root)
}

/// `main` calling `apply(|x| x + y, 2)` with `let y: u64 = 40;` (a closure of kind `Fn`), or,
/// with `writes`, `let n: u64 = 1; let r = apply(|x| { n = n + x; n }, 5); r * 100 + n` (kind
/// `FnMut`). Returns main's builder, root and literals.
fn main_applying(writes: bool) -> (Bb, ExprId, StdVec<(ExprId, u128)>) {
    let mut b = Bb::new();
    let mut lits = StdVec::new();
    let (s0, y) = let_u64(&mut b, &mut lits, if writes { "n" } else { "y" }, if writes { 1 } else { 40 });
    let x = b.bind("x");
    let (sv, xe) = param_u64(&mut b, x);
    let ye = b.local(y);
    let body = if writes {
        let ye2 = b.local(y);
        let sum = bin(&mut b, BinOp::Add, ye2, xe);
        let set = assign(&mut b, ye, sum);
        let set = b.semi(set);
        let tail = b.local(y);
        b.block(&[sv, set], Some(tail))
    } else {
        let sum = bin(&mut b, BinOp::Add, xe, ye);
        b.block(&[sv], Some(sum))
    };
    let clo = b.closure(&[x], body);
    let a = lit(&mut b, &mut lits, if writes { 5 } else { 2 });
    let callee = b.path(&["apply"]);
    let call = b.call(callee, &[clo, a]);
    if !writes {
        let root = b.block(&[s0], Some(call));
        return (b, root, lits);
    }
    let r = b.bind("r");
    let t = b.ty(PNode::Uint(UintTy::U64));
    let s1 = b.let_(r, Some(t), call);
    let re = b.local(r);
    let hundred = lit(&mut b, &mut lits, 100);
    let scaled = bin(&mut b, BinOp::Mul, re, hundred);
    let ne = b.local(y);
    let out = bin(&mut b, BinOp::Add, scaled, ne);
    let root = b.block(&[s0, s1], Some(out));
    (b, root, lits)
}

/// Runs `main_applying(writes)` against `apply` bounded by trait `tr`, calling `f` `calls` times.
fn run_apply(tr: usize, calls: usize, writes: bool) -> (u64, usize) {
    let mut w = open_world();
    let (item, ab, aroot) = apply(&mut w, tr, calls);
    w.fx.finish();
    let (mb, mroot, mlits) = main_applying(writes);
    run_progs(&w, &[Prog { item: w.main, b: &mb, root: mroot, lits: &mlits }, Prog { item, b: &ab, root: aroot, lits: &[] }])
}

/// The Reference, "Call expressions": "for closures and other types implementing the `Fn*`
/// traits, the call is resolved to the trait's method". `apply<F: Fn(u64) -> u64>(f: F, x:
/// u64) -> u64 { f(x) }` called as `apply(|x| x + y, 2)` with `y = 40` is 42: the call through
/// `Fn::call` at `[C, (u64,)]` in `apply`'s instance reaches the closure's own body (one
/// closure method built).
#[test]
fn a_closure_passed_to_a_generic_fn_bounded_by_fn_is_called_there() {
    assert_eq!(run_apply(0, 1, false), (42, 1));
}

/// "Capture modes": a closure that assigns a captured variable is `FnMut`. With `F: FnMut(u64)
/// -> u64` and `f(x); f(x)` in `apply`, `let n = 1; let r = apply(|x| { n = n + x; n }, 5); r *
/// 100 + n` is 1111: each call sees the last one's write (6, then 11) and `n` after is 11.
#[test]
fn a_closure_passed_to_a_generic_fn_bounded_by_fn_mut_writes_its_capture_each_call() {
    assert_eq!(run_apply(1, 2, true), (1111, 1));
}

/// core's `trait Fn<Args>: FnMut<Args>` and `trait FnMut<Args>: FnOnce<Args>`: an `Fn` closure
/// is `FnOnce` too. With `F: FnOnce(u64) -> u64`, `apply(|x| x + y, 2)` is 42: `call_once` on
/// it is a thin ENTRY of its own (the environment taken by value, the body called on its
/// address, the environment dropped after), so two closure methods are built.
#[test]
fn an_fn_closure_used_as_fn_once_goes_through_its_call_once_entry() {
    assert_eq!(run_apply(2, 1, false), (42, 2));
}

/// The same with `F: FnMut(u64) -> u64` called twice: `call_mut` on an `Fn` closure is an
/// ENTRY that passes its pointer on to the body; 42 again, two closure methods built.
#[test]
fn an_fn_closure_used_as_fn_mut_goes_through_its_call_mut_entry() {
    assert_eq!(run_apply(1, 2, false), (42, 2));
}

/// The Reference, "Closure types": "A closure expression produces a closure value with a
/// unique, anonymous type". Two closures of one body with one signature are two types and two
/// bodies, named by their ORDINAL in the owner (typeck.md, "A closure's identity"): `let f =
/// |x| x + 1; let g = |x| x + 2; let a: u64 = 10; f(a) * 100 + g(a)` is 1112, two bodies built.
#[test]
fn two_closures_of_one_body_are_two_instances_by_ordinal() {
    let w = world();
    let mut b = Bb::new();
    let mut lits = StdVec::new();
    let mut stmts = StdVec::new();
    let mut fs = StdVec::new();
    for (name, k) in [("f", 1), ("g", 2)] {
        let x = b.bind("x");
        let (sv, xe) = param_u64(&mut b, x);
        let k = lit(&mut b, &mut lits, k);
        let sum = bin(&mut b, BinOp::Add, xe, k);
        let body = b.block(&[sv], Some(sum));
        let clo = b.closure(&[x], body);
        let f = b.bind(name);
        stmts.push(b.let_(f, None, clo));
        fs.push(f);
    }
    let (s, a) = let_u64(&mut b, &mut lits, "a", 10);
    stmts.push(s);
    let (fe, ae) = (b.local(fs[0]), b.local(a));
    let c1 = b.call(fe, &[ae]);
    let hundred = lit(&mut b, &mut lits, 100);
    let scaled = bin(&mut b, BinOp::Mul, c1, hundred);
    let (ge, ae) = (b.local(fs[1]), b.local(a));
    let c2 = b.call(ge, &[ae]);
    let out = bin(&mut b, BinOp::Add, scaled, c2);
    let root = b.block(&stmts, Some(out));
    assert_eq!(run(&w, &b, root, &lits), (1112, 2));
}

/// The Reference, "Closure types", "Capture modes": "Closures capture variables by the least
/// needed mode: by immutable borrow ... by unique immutable borrow ... by mutable borrow ... by
/// move". `let y: u64 = 40; let f = |x| x + y; let a: u64 = 2; f(a)` is 42: `y` is read
/// through the environment's pointer (`ByRef`, so the closure is `Fn`).
#[test]
fn a_closure_reads_a_capture_by_shared_reference() {
    let w = world();
    let mut b = Bb::new();
    let mut lits = StdVec::new();
    let (s0, y) = let_u64(&mut b, &mut lits, "y", 40);
    let x = b.bind("x");
    let (sv, xe) = param_u64(&mut b, x);
    let ye = b.local(y);
    let sum = bin(&mut b, BinOp::Add, xe, ye);
    let body = b.block(&[sv], Some(sum));
    let clo = b.closure(&[x], body);
    let f = b.bind("f");
    let s1 = b.let_(f, None, clo);
    let (s2, a) = let_u64(&mut b, &mut lits, "a", 2);
    let fe = b.local(f);
    let ae = b.local(a);
    let call = b.call(fe, &[ae]);
    let root = b.block(&[s0, s1, s2], Some(call));
    assert_eq!(run(&w, &b, root, &lits), (42, 1));
}

/// "Capture modes": a closure that assigns a captured variable captures it by mutable
/// borrow, so it is `FnMut` and the write is the variable's. `let n: u64 = 1; let f = |x| { n
/// = n + x; n }; let a: u64 = 5; f(a); let c: u64 = 7; f(c) * 100 + n` is 1313: the second
/// call sees the first's write (13), and `n` after the calls is the closure's write too.
#[test]
fn a_closure_writes_a_capture_by_mutable_reference_and_the_second_call_sees_it() {
    let w = world();
    let mut b = Bb::new();
    let mut lits = StdVec::new();
    let (s0, n) = let_u64(&mut b, &mut lits, "n", 1);
    let x = b.bind("x");
    let (sv, xe) = param_u64(&mut b, x);
    let (ne, ne2) = (b.local(n), b.local(n));
    let sum = bin(&mut b, BinOp::Add, ne2, xe);
    let set = assign(&mut b, ne, sum);
    let set = b.semi(set);
    let tail = b.local(n);
    let body = b.block(&[sv, set], Some(tail));
    let clo = b.closure(&[x], body);
    let f = b.bind("f");
    let s1 = b.let_(f, None, clo);
    let (s2, a) = let_u64(&mut b, &mut lits, "a", 5);
    let (fe, ae) = (b.local(f), b.local(a));
    let c1 = b.call(fe, &[ae]);
    let s3 = b.semi(c1);
    let (s4, c) = let_u64(&mut b, &mut lits, "c", 7);
    let (fe, ce) = (b.local(f), b.local(c));
    let c2 = b.call(fe, &[ce]);
    let hundred = lit(&mut b, &mut lits, 100);
    let scaled = bin(&mut b, BinOp::Mul, c2, hundred);
    let nl = b.local(n);
    let out = bin(&mut b, BinOp::Add, scaled, nl);
    let root = b.block(&[s0, s1, s2, s3, s4], Some(out));
    assert_eq!(run(&w, &b, root, &lits), (1313, 1));
}
