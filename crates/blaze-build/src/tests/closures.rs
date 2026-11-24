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
use crate::layout::{ClosureFacts, LayoutCx, LayoutFacts, Layouts, Missing, ReprAttrs, ValidRange};
use crate::{Abi, Adts, Built, FnTrait, Subst};

struct World {
    fx: Fx,
    main: ItemKey,
    /// `Fn::call`, `FnMut::call_mut`, `FnOnce::call_once`.
    methods: [ItemKey; 3],
    /// `Fn`, `FnMut`, `FnOnce`.
    traits: [ItemKey; 3],
    /// `struct D { x: u64 }`'s item, its host `Drop` fn (counts in [`DROPS`]), `fn mk_d() -> D`
    /// (host: `D { x: 5 }`) and the `drop_in_place` lang fn.
    d: ItemKey,
    drop_d: ItemKey,
    mk_d: ItemKey,
    dip: ItemKey,
    /// `fn drops() -> u64` (host: the drops of `D` counted so far): where a program reads the
    /// ORDER of its drops.
    drops: ItemKey,
    /// `struct S { a: u8, b: u64, c: u8 }` and `fn mk_s() -> S` (host: `S { a: 7, b: 40, c: 2
    /// }` at the offsets [`S_AT`] holds): an aggregate whose `repr(Rust)` layout REORDERS its
    /// fields (`b` first, by decreasing alignment).
    s: ItemKey,
    mk_s: ItemKey,
}

std::thread_local! {
    /// How many `D`s the host's `Drop` saw.
    static DROPS: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
    /// The offsets of `S`'s fields `a`, `b`, `c` as the builder's layout places them (the test
    /// reads them from `Layouts` before it runs: [`s_offsets`]).
    static S_AT: core::cell::Cell<[usize; 3]> = const { core::cell::Cell::new([0; 3]) };
}

/// `mk_s()`: `S { a: 7, b: 40, c: 2 }` through the result pointer, each field at the offset
/// the builder's layout gives it ([`S_AT`]).
extern "C" fn host_mk_s(out: *mut u8) {
    let [a, b, c] = S_AT.with(core::cell::Cell::get);
    // SAFETY: `out` is the caller's result slot of one `S` (16 bytes), and each offset is one
    // of its fields' inside it.
    unsafe {
        out.add(a).write(7);
        out.add(b).cast::<u64>().write_unaligned(40);
        out.add(c).write(2);
    }
}

/// `D`'s `Drop::drop(&mut self)` on the host: counts one drop.
extern "C" fn host_drop_d(p: *const u64) {
    // SAFETY: the glue passes the D's address, a live u64 (its `x`).
    assert_eq!(unsafe { p.read() }, 5, "a drop of a value that is not a live D");
    DROPS.with(|d| d.set(d.get() + 1));
}

/// `mk_d()`: a one-field struct is an aggregate, returned through the result pointer (lib.rs,
/// "Rust ABI").
extern "C" fn host_mk_d(out: *mut u64) {
    // SAFETY: `out` is the caller's result slot of one D.
    unsafe { out.write(5) };
}

/// `drops()`: the drops of `D` counted so far (a `u64` returned in a register).
extern "C" fn host_drops() -> u64 {
    DROPS.with(core::cell::Cell::get)
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
    // `#[lang = "copy"] trait Copy {}`: "Capture modes" captures a value moved out by value
    // only when it is not `Copy`, which the checker asks of this trait.
    let copy = fx.tr("Copy", 1, Vec::new(), &[]);
    fx.lang(LangItem::Copy, copy);
    let (p0, p1) = (fx.param(0), fx.param(1));
    let rust_call = FnPtrHeader { abi: TAbi::RustCall, safety: Safety::Safe, c_variadic: false };
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
    fx.imp("copy_u64", 0, u64_, Some((copy, &[])), &[], Vec::new());
    let d = fx.strukt("D", 0, &[("x", u64_)]);
    let dt = fx.adt(d, &[]);
    let md = fx.r(Mutbl::Mut, dt);
    let unit = fx.unit();
    let drop_d = fx.func("drop_d", 0, &[md], unit, Vec::new());
    let mk_d = fx.func("mk_d", 0, &[], dt, Vec::new());
    let raw = fx.ty(PNode::Ptr(Mutbl::Mut, p0));
    let dip = fx.func("drop_in_place", 1, &[raw], unit, Vec::new());
    fx.unsized_fn_param(dip, 0);
    let drops = fx.func("drops", 0, &[], u64_, Vec::new());
    let u8_ = fx.ty(PNode::Uint(UintTy::U8));
    let s = fx.strukt("S", 0, &[("a", u8_), ("b", u64_), ("c", u8_)]);
    let st = fx.adt(s, &[]);
    let mk_s = fx.func("mk_s", 0, &[], st, Vec::new());
    World { fx, main, methods: [call, call_mut, call_once], traits: [fn_, fn_mut, fn_once], d, drop_d, mk_d, dip, drops, s, mk_s }
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

    fn repr(&self, _: ItemKey) -> Result<ReprAttrs, Missing> {
        Ok(ReprAttrs::RUST)
    }

    fn discriminant(&self, _: ItemKey, _: u32) -> Result<Option<i128>, Missing> {
        Ok(None)
    }

    fn valid_range(&self, _: ItemKey) -> Result<ValidRange, Missing> {
        Ok(ValidRange::default())
    }
}

impl Adts for FxAdts<'_> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.w.fx.adt_of(key)
    }

    /// The fixture's bodies do no arithmetic that overflows: the release setting.
    fn overflow_checks(&self) -> crate::OverflowChecks {
        crate::OverflowChecks::Off
    }

    fn ub_checks(&self) -> crate::UbChecks {
        crate::UbChecks::Off
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

    fn drop_impl(&self, ty: PTy, ground: &mut PortableTys<'_>) -> Result<Option<ItemKey>, crate::BuildError> {
        Ok(match ground.view().node(ty) {
            Ok(PNode::Adt(k, _)) if k == self.w.d => Some(self.w.drop_d),
            _ => None,
        })
    }

    fn lang_fn(&self, name: &str) -> Option<ItemKey> {
        (name == "drop_in_place").then_some(self.w.dip)
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
/// method its item names, a fn item's or fn pointer's `Fn*` shim). Returns `main`'s value and
/// the number of `Fn*` methods built (closure bodies, entries and shims).
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
    for ((p, c), view) in progs.iter().zip(checked.iter()).zip(views.iter()) {
        let s = w.fx.sig_of(p.item).expect("the fn's signature");
        for ordinal in 0..c.closures.len() as u32 {
            let (kind, sig, env) = crate::layout::closure_facts(view, c, ordinal, s.parent_count + s.own_count, &mut table, Alloc::global())
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
    // `drop_in_place`'s written body (never built: each instance is its argument's glue).
    let mut gb = Bb::new();
    gb.param("p");
    let groot = gb.block(&[], None);
    let gview = gb.view(w.dip, groot);
    let mut gty = InferCtx::new_in(Alloc::global());
    let gchecked = match blaze_typeck::check(&gview, &w.fx, &mut gty, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        other => panic!("the check of `drop_in_place` did not finish: {:?}", other.err()),
    };
    let mut todo: StdVec<(ItemKey, List)> = main.refs.iter().map(|r| (r.item, r.args)).collect();
    // An instance is built once per KEY (instance.rs `instance_key`, as the store names it), not
    // per argument list: two instances whose keys collide would share one body here too.
    let key = |ground: &PortableTys<'_>, item: ItemKey, args: List| {
        crate::instance_key(blaze_ids::ContentId::of("test code", &item.0.to_le_bytes()), args, ground.view(), Alloc::global()).expect("an instance key")
    };
    let mut keys: StdVec<crate::InstanceKey> = StdVec::new();
    while let Some((item, args)) = todo.pop() {
        let k = key(&ground, item, args);
        if keys.contains(&k) {
            continue;
        }
        let built = if let Some(m) = w.methods.iter().position(|m| *m == item) {
            let all = ground.view().list(args).expect("the instance's arguments").to_vec();
            let which = [FnTrait::Fn, FnTrait::FnMut, FnTrait::FnOnce][m];
            closures += 1;
            if let Some(c) = crate::closure_of(ground.view(), all[0]) {
                let at = progs.iter().position(|p| p.item == c.owner).expect("the closure's owner among the bodies");
                let oargs = ground.view().list(c.owner_args).expect("the owner's arguments").to_vec();
                let sig = w.fx.sig_of(c.owner).expect("the owner's signature");
                let mut subst = Subst { args: &oargs, ground: &mut ground, scratch: Alloc::global() };
                crate::build_closure(&views[at], &checked[at], c.ordinal, which, &sig, &mut subst, &adts, Global)
                    .unwrap_or_else(|e| panic!("the closure's {which:?} method failed: {e}"))
            } else {
                // A fn item or fn pointer as an `Fn*` value: its shim (lib.rs `build_fn_shim`).
                let (sig, fargs) = match ground.view().node(all[0]).expect("the callee's type") {
                    PNode::FnDef(f, fa) => (w.fx.sig_of(f), ground.view().list(fa).expect("the fn item's arguments").to_vec()),
                    _ => (None, StdVec::new()),
                };
                let mut subst = Subst { args: &fargs, ground: &mut ground, scratch: Alloc::global() };
                crate::build_fn_shim(all[0], all[1], which, sig.as_ref(), &mut subst, &adts, Global)
                    .unwrap_or_else(|e| panic!("the fn shim's {which:?} method failed: {e}"))
            }
        } else if item == w.dip {
            let gargs = ground.view().list(args).expect("the glue's argument").to_vec();
            let sig = w.fx.sig_of(w.dip).expect("`drop_in_place`'s signature");
            let mut subst = Subst { args: &gargs, ground: &mut ground, scratch: Alloc::global() };
            crate::build_drop_glue(&gview, &gchecked, &sig, &mut subst, &adts, Global).unwrap_or_else(|e| panic!("the glue at {gargs:?} failed: {e}"))
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
        keys.push(k);
        bodies.push((item, args, built));
    }
    // Callees before callers (the run places each fn after every fn it names): a pass places
    // each body whose built callees are all placed; a body reached twice (a glue named by two
    // callers) is placed once, before both.
    let mut fns: StdVec<(ItemKey, Option<List>, &Built<Global>)> = StdVec::new();
    let mut placed = std::vec![false; bodies.len()];
    for _ in 0..=bodies.len() {
        for (j, (i, a, b)) in bodies.iter().enumerate() {
            let ready = b.refs.iter().all(|r| {
                (r.item == *i && r.args == *a) || bodies.iter().zip(&placed).all(|((ci, ca, _), &p)| p || *ci != r.item || *ca != r.args)
            });
            if !placed[j] && ready {
                placed[j] = true;
                fns.push((*i, Some(*a), b));
            }
        }
    }
    assert!(placed.iter().all(|&p| p), "a cycle among the built bodies");
    fns.push((w.main, None, &main));
    let host = [
        (w.drop_d, host_drop_d as usize as u64),
        (w.mk_d, host_mk_d as usize as u64),
        (w.drops, host_drops as usize as u64),
        (w.mk_s, host_mk_s as usize as u64),
    ];
    (run_native_with(&host, &fns), closures)
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

/// A capture names the variable, whatever its pattern: chrono's
/// `Parsed::to_naive_date::resolve_year` captures `Some(r @ 0..=99)`'s `r` in
/// `|v| v.checked_add(r)` (tempo's census of 2026-11-17 04:35: refused with "a `ref` binding or a
/// binding with a subpattern"). The Reference, "Identifier patterns": `r @ sub` binds `r` to
/// the value `sub` matched; "Capture modes": the closure captures the variable `r`.
/// `fn main() -> u64 { let a: u64 = 2; match a { r @ _ => { let f = |x| x + r; f(5) } } }` is 7.
/// Fails without walk.rs `bind_capture_at` (the closure's `Fn::call` refused).
#[test]
fn a_closure_captures_a_binding_with_a_subpattern() {
    let w = world();
    let mut b = Bb::new();
    let mut lits = StdVec::new();
    let (s0, a) = let_u64(&mut b, &mut lits, "a", 2);
    let any = b.wild();
    let r = b.bind_sub("r", any);
    let x = b.bind("x");
    let (sv, xe) = param_u64(&mut b, x);
    let re = b.local(r);
    let sum = bin(&mut b, BinOp::Add, xe, re);
    let body = b.block(&[sv], Some(sum));
    let clo = b.closure(&[x], body);
    let f = b.bind("f");
    let s1 = b.let_(f, None, clo);
    let fe = b.local(f);
    let five = lit(&mut b, &mut lits, 5);
    let call = b.call(fe, &[five]);
    let arm = b.block(&[s1], Some(call));
    let ae = b.local(a);
    let m = b.match_(ae, &[(r, arm)]);
    let root = b.block(&[s0], Some(m));
    assert_eq!(run(&w, &b, root, &lits), (7, 1));
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
    // Bound first, so no expected bound reaches the closure expression and its kind is its
    // captures' alone (an argument written in place takes the bound's kind, as rustc does).
    let clo = b.closure(&[x], body);
    let f = b.bind("f");
    let sf = b.let_(f, None, clo);
    let fe = b.local(f);
    let a = lit(&mut b, &mut lits, if writes { 5 } else { 2 });
    let callee = b.path(&["apply"]);
    let call = b.call(callee, &[fe, a]);
    if !writes {
        let root = b.block(&[s0, sf], Some(call));
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
    let root = b.block(&[s0, sf, s1], Some(out));
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

/// core's function.rs: `Args` is a tuple and "rust-call" spreads it, so a closure of NO
/// parameters takes `()`: its body is `(env)` alone. `let y: u64 = 40; let f = || y + 2; f()`
/// is 42.
#[test]
fn a_closure_of_no_parameters_takes_the_empty_tuple() {
    let w = world();
    let mut b = Bb::new();
    let mut lits = StdVec::new();
    let (s0, y) = let_u64(&mut b, &mut lits, "y", 40);
    let ye = b.local(y);
    let two = lit(&mut b, &mut lits, 2);
    let sum = bin(&mut b, BinOp::Add, ye, two);
    let clo = b.closure(&[], sum);
    let f = b.bind("f");
    let s1 = b.let_(f, None, clo);
    let fe = b.local(f);
    let call = b.call(fe, &[]);
    let root = b.block(&[s0, s1], Some(call));
    assert_eq!(run(&w, &b, root, &lits), (42, 1));
}

/// The same with THREE parameters, each a field of `Args` crossing as its own parameter in
/// order: `let f = |a, b, c| a * 100 + b * 10 + c; f(1, 2, 3)` is 123 (an order slip gives 321
/// or another permutation).
#[test]
fn a_closure_of_three_parameters_takes_them_in_order() {
    let w = world();
    let mut b = Bb::new();
    let mut lits = StdVec::new();
    let ps = [b.bind("a"), b.bind("b"), b.bind("c")];
    let mut stmts = StdVec::new();
    let mut uses = StdVec::new();
    for &p in &ps {
        let (s, e) = param_u64(&mut b, p);
        stmts.push(s);
        uses.push(e);
    }
    let h = lit(&mut b, &mut lits, 100);
    let t = lit(&mut b, &mut lits, 10);
    let a100 = bin(&mut b, BinOp::Mul, uses[0], h);
    let b10 = bin(&mut b, BinOp::Mul, uses[1], t);
    let ab = bin(&mut b, BinOp::Add, a100, b10);
    let abc = bin(&mut b, BinOp::Add, ab, uses[2]);
    let body = b.block(&stmts, Some(abc));
    let clo = b.closure(&ps, body);
    let f = b.bind("f");
    let s0 = b.let_(f, None, clo);
    let mut args = StdVec::new();
    let mut lets = std::vec![s0];
    for (name, v) in [("p", 1), ("q", 2), ("r", 3)] {
        let (s, p) = let_u64(&mut b, &mut lits, name, v);
        lets.push(s);
        args.push(b.local(p));
    }
    let fe = b.local(f);
    let call = b.call(fe, &args);
    let root = b.block(&lets, Some(call));
    assert_eq!(run(&w, &b, root, &lits), (123, 1));
}

/// "Closure types": a closure's value is a value like any other, so a closure may return one.
/// `let add = |x| { let v: u64 = x; move |y| { let w: u64 = y; v + w } }; let g = add(40); let
/// a: u64 = 2; g(a)` is 42: the outer body builds the inner closure's environment (its `v` by
/// value, "Capture modes": `move`) and returns it; two closure methods built, one per ordinal.
#[test]
fn a_closure_returns_a_closure_that_owns_its_capture() {
    let w = world();
    let mut b = Bb::new();
    let mut lits = StdVec::new();
    let y = b.bind("y");
    let (sw, we) = param_u64(&mut b, y);
    let x = b.bind("x");
    let vb = b.bind("v");
    let xe = b.local(x);
    let t = b.ty(PNode::Uint(UintTy::U64));
    let sv = b.let_(vb, Some(t), xe);
    let ve = b.local(vb);
    let sum = bin(&mut b, BinOp::Add, ve, we);
    let ibody = b.block(&[sw], Some(sum));
    let inner = b.move_closure(&[y], ibody);
    let obody = b.block(&[sv], Some(inner));
    let outer = b.closure(&[x], obody);
    let add = b.bind("add");
    let s0 = b.let_(add, None, outer);
    let (s1, n) = let_u64(&mut b, &mut lits, "n", 40);
    let (adde, ne) = (b.local(add), b.local(n));
    let made = b.call(adde, &[ne]);
    let g = b.bind("g");
    let s2 = b.let_(g, None, made);
    let (s3, a) = let_u64(&mut b, &mut lits, "a", 2);
    let (ge, ae) = (b.local(g), b.local(a));
    let call = b.call(ge, &[ae]);
    let root = b.block(&[s0, s1, s2, s3], Some(call));
    assert_eq!(run(&w, &b, root, &lits), (42, 2));
}

/// core's function.rs: `impl<A: Tuple, F: ?Sized> Fn<A> for &F where F: Fn<A>`. In a world
/// WITHOUT that impl, `fn apply<F: Fn(u64) -> u64>(f: &F, x: u64) -> u64 { f(x) }` called as
/// `apply(&|x| x + y, 2)` (`y = 40`) is 42: "Call expressions" autoderefs the callee to `F`, so
/// the call is `Fn::call` at `[C, (u64,)]`, the closure's own body.
#[test]
fn a_closure_by_reference_to_a_generic_fn_is_called_through_its_referent() {
    let mut w = open_world();
    let u64_ = w.fx.ty(PNode::Uint(UintTy::U64));
    let p0 = w.fx.param(0);
    let rf = w.fx.r(Mutbl::Not, p0);
    let targs = w.fx.tuple(&[u64_]);
    let bound = w.fx.bound(w.traits[0], &[p0, targs]);
    let out = w.fx.proj(w.traits[2], &[p0, targs], "Output", u64_);
    let item = w.fx.func("apply", 1, &[rf, u64_], u64_, std::vec![bound, out]);
    w.fx.finish();
    let mut ab = Bb::new();
    let (f, x) = (ab.param("f"), ab.param("x"));
    let (fe, xe) = (ab.local(f), ab.local(x));
    let c = ab.call(fe, &[xe]);
    let aroot = ab.block(&[], Some(c));
    let mut b = Bb::new();
    let mut lits = StdVec::new();
    let (s0, y) = let_u64(&mut b, &mut lits, "y", 40);
    let xb = b.bind("x");
    let (sv, xv) = param_u64(&mut b, xb);
    let ye = b.local(y);
    let sum = bin(&mut b, BinOp::Add, xv, ye);
    let body = b.block(&[sv], Some(sum));
    let clo = b.closure(&[xb], body);
    let fb = b.bind("f");
    let s1 = b.let_(fb, None, clo);
    let fl = b.local(fb);
    let rfe = b.borrow(Mutbl::Not, fl);
    let a = lit(&mut b, &mut lits, 2);
    let callee = b.path(&["apply"]);
    let call = b.call(callee, &[rfe, a]);
    let root = b.block(&[s0, s1], Some(call));
    let progs = [Prog { item: w.main, b: &b, root, lits: &lits }, Prog { item, b: &ab, root: aroot, lits: &[] }];
    assert_eq!(run_progs(&w, &progs), (42, 1));
}

/// The Reference, "Type coercions": "Non capturing closures to `fn` pointers". `fn via(x: u64,
/// f: fn(u64) -> u64) -> u64 { f(x) }` called as `via(41, |x| x + 1)` is 42: the pointer is
/// the closure's `call_once` (its zero-sized `self` crosses as nothing), here the entry over
/// its `Fn` body: two closure methods.
#[test]
fn a_closure_that_captures_nothing_coerces_to_a_fn_pointer() {
    let mut w = open_world();
    let u64_ = w.fx.ty(PNode::Uint(UintTy::U64));
    let io = w.fx.list(&[u64_, u64_]);
    let fp = w.fx.ty(PNode::FnPtr(io, FnPtrHeader::RUST));
    let item = w.fx.func("via", 0, &[u64_, fp], u64_, Vec::new());
    w.fx.finish();
    let mut vb = Bb::new();
    let (x, f) = (vb.param("x"), vb.param("f"));
    let (fe, xe) = (vb.local(f), vb.local(x));
    let c = vb.call(fe, &[xe]);
    let vroot = vb.block(&[], Some(c));
    let mut b = Bb::new();
    let mut lits = StdVec::new();
    let xb = b.bind("x");
    let (sv, xv) = param_u64(&mut b, xb);
    let one = lit(&mut b, &mut lits, 1);
    let sum = bin(&mut b, BinOp::Add, xv, one);
    let body = b.block(&[sv], Some(sum));
    let clo = b.closure(&[xb], body);
    let a = lit(&mut b, &mut lits, 41);
    let callee = b.path(&["via"]);
    let call = b.call(callee, &[a, clo]);
    let root = b.block(&[], Some(call));
    let progs = [Prog { item: w.main, b: &b, root, lits: &lits }, Prog { item, b: &vb, root: vroot, lits: &[] }];
    assert_eq!(run_progs(&w, &progs), (42, 2));
}

/// `let d = mk_d();` in `b`.
fn let_d(b: &mut Bb) -> (StmtId, PatId) {
    let d = b.bind("d");
    let callee = b.path(&["mk_d"]);
    let e = b.call(callee, &[]);
    (b.let_(d, None, e), d)
}

/// Runs `main` (one body) and returns its value, the closure methods built and the drops of
/// `D` the host counted.
fn run_counting(w: &World, b: &Bb, root: ExprId, lits: &[(ExprId, u128)]) -> (u64, usize, u64) {
    DROPS.with(|d| d.set(0));
    let (v, n) = run(w, b, root, lits);
    (v, n, DROPS.with(core::cell::Cell::get))
}

/// "Capture modes": "`move` ... captures by value"; "Drop order": a closure's captures are
/// dropped when the closure is. `let d = mk_d(); let f = move || { let r = &d; 7 }; 9` is 9
/// with ONE drop of `D`: the environment owns it, `d` itself was moved and is not dropped, and
/// the closure is dropped uncalled at the end of `main`.
#[test]
fn a_move_closure_owns_its_capture_and_drops_it_once_when_never_called() {
    let w = world();
    let mut b = Bb::new();
    let mut lits = StdVec::new();
    let (s0, d) = let_d(&mut b);
    let de = b.local(d);
    let rd = b.borrow(Mutbl::Not, de);
    let r = b.bind("r");
    let sr = b.let_(r, None, rd);
    let seven = lit(&mut b, &mut lits, 7);
    let body = b.block(&[sr], Some(seven));
    let clo = b.move_closure(&[], body);
    let f = b.bind("f");
    let s1 = b.let_(f, None, clo);
    let nine = lit(&mut b, &mut lits, 9);
    let root = b.block(&[s0, s1], Some(nine));
    assert_eq!(run_counting(&w, &b, root, &lits), (9, 0, 1));
}

/// "Closure types": a closure that moves a capture out is `FnOnce` only, its call consumes
/// it. `let d = mk_d(); let f = || { let e = d; 7 }; f()` is 7 with ONE drop: `d` is captured
/// by value (moved out, "Capture modes"), the body owns its environment and drops `e` at its
/// end; nothing is dropped again after the call.
#[test]
fn an_fn_once_closure_consumes_its_capture_when_called() {
    let w = world();
    let mut b = Bb::new();
    let mut lits = StdVec::new();
    let (s0, d) = let_d(&mut b);
    let de = b.local(d);
    let e = b.bind("e");
    let se = b.let_(e, None, de);
    let seven = lit(&mut b, &mut lits, 7);
    let body = b.block(&[se], Some(seven));
    let clo = b.closure(&[], body);
    let f = b.bind("f");
    let s1 = b.let_(f, None, clo);
    let fe = b.local(f);
    let call = b.call(fe, &[]);
    let root = b.block(&[s0, s1], Some(call));
    assert_eq!(run_counting(&w, &b, root, &lits), (7, 1, 1));
}

/// "Capture modes": a `move` closure over a `Copy` value takes a COPY. `let n: u64 = 40; let f
/// = move |x| x + n; n = 1; f(2) * 100 + n` is 4201: the closure's `n` stays 40 after the
/// variable is assigned (by reference it would read 1: 301).
#[test]
fn a_move_closure_over_a_copy_value_copies_it() {
    let w = world();
    let mut b = Bb::new();
    let mut lits = StdVec::new();
    let (s0, n) = let_u64(&mut b, &mut lits, "n", 40);
    let x = b.bind("x");
    let (sv, xe) = param_u64(&mut b, x);
    let ne = b.local(n);
    let sum = bin(&mut b, BinOp::Add, xe, ne);
    let body = b.block(&[sv], Some(sum));
    let clo = b.move_closure(&[x], body);
    let f = b.bind("f");
    let s1 = b.let_(f, None, clo);
    let nl = b.local(n);
    let one = lit(&mut b, &mut lits, 1);
    let set = assign(&mut b, nl, one);
    let s2 = b.semi(set);
    let (s3, a) = let_u64(&mut b, &mut lits, "a", 2);
    let (fe, ae) = (b.local(f), b.local(a));
    let call = b.call(fe, &[ae]);
    let hundred = lit(&mut b, &mut lits, 100);
    let scaled = bin(&mut b, BinOp::Mul, call, hundred);
    let nr = b.local(n);
    let out = bin(&mut b, BinOp::Add, scaled, nr);
    let root = b.block(&[s0, s1, s2, s3], Some(out));
    assert_eq!(run(&w, &b, root, &lits), (4201, 1));
}

/// `fn double(x: u64) -> u64 { x + x }`, added to `w` (still open). Returns its item, builder
/// and root.
fn double(w: &mut World) -> (ItemKey, Bb, ExprId) {
    let u64_ = w.fx.ty(PNode::Uint(UintTy::U64));
    let item = w.fx.func("double", 0, &[u64_], u64_, Vec::new());
    let mut b = Bb::new();
    let x = b.param("x");
    let (l, r) = (b.local(x), b.local(x));
    let sum = bin(&mut b, BinOp::Add, l, r);
    let root = b.block(&[], Some(sum));
    (item, b, root)
}

/// The Reference, "Function item types": "All function items implement `Fn`, `FnMut`,
/// `FnOnce`, `Copy`, `Clone`, `Send`, and `Sync`", and a fn item's type is zero-sized.
/// `apply<F: Fn(u64) -> u64>(f: F, x: u64) -> u64 { f(x) }` called as `let a: u64 = 21;
/// apply(double, a)` is 42: in `apply`'s instance `f(x)` borrows `f` and calls `Fn::call` at
/// `[fn item double, (u64,)]`, whose instance is the SHIM (lib.rs `build_fn_shim`): it ignores
/// its zero-sized `self` and calls `double(21)` directly. One `Fn*` method built. (Bounded by
/// `FnOnce` the call grounds to the fn item itself and is a plain direct call: no shim.)
#[test]
fn a_fn_item_passed_where_fn_is_bound_is_called_through_its_shim() {
    let mut w = open_world();
    let (item, ab, aroot) = apply(&mut w, 0, 1);
    let (ditem, db, droot) = double(&mut w);
    w.fx.finish();
    let mut b = Bb::new();
    let mut lits = StdVec::new();
    let (s0, a) = let_u64(&mut b, &mut lits, "a", 21);
    let callee = b.path(&["apply"]);
    let d = b.path(&["double"]);
    let ae = b.local(a);
    let call = b.call(callee, &[d, ae]);
    let root = b.block(&[s0], Some(call));
    let progs = [
        Prog { item: w.main, b: &b, root, lits: &lits },
        Prog { item, b: &ab, root: aroot, lits: &[] },
        Prog { item: ditem, b: &db, root: droot, lits: &[] },
    ];
    assert_eq!(run_progs(&w, &progs), (42, 1));
}

/// core's library/core/src/ops/function.rs: "`Fn` is implemented automatically by closures
/// ... as well as (safe) [function pointers]", `FnMut` likewise. `let p: fn(u64) -> u64 =
/// double; let a: u64 = 21; apply(p, a)` with `apply<F: FnMut(u64) -> u64>` calling `f(x)`
/// twice is 42 (its last call's value, `double(21)`): `f(x)` borrows `f` mutably and calls
/// `FnMut::call_mut` at `[fn(u64) -> u64, (u64,)]`, whose SHIM loads the pointer through its
/// `self` and calls it INDIRECTLY; the pointer is `double`'s address (the Reference, "Type
/// coercions": "Function item types to `fn` pointers"). One `Fn*` method built, called twice.
/// The instance `apply::<fn(u64) -> u64>` and the shim are keyed with the pointer type in their
/// arguments (instance.rs: tag 21, the header's byte): without it `instance_key` refuses them
/// ("a type with no ground form in an instance's arguments").
#[test]
fn a_fn_pointer_passed_where_fn_mut_is_bound_is_called_indirectly_through_its_shim() {
    let mut w = open_world();
    let (item, ab, aroot) = apply(&mut w, 1, 2);
    let (ditem, db, droot) = double(&mut w);
    w.fx.finish();
    let mut b = Bb::new();
    let mut lits = StdVec::new();
    let u64_ = b.ty(PNode::Uint(UintTy::U64));
    let io = b.t.intern_list(&[u64_, u64_]).expect("intern");
    let fp = b.ty(PNode::FnPtr(io, FnPtrHeader::RUST));
    let p = b.bind("p");
    let d = b.path(&["double"]);
    let s0 = b.let_(p, Some(fp), d);
    let (s1, a) = let_u64(&mut b, &mut lits, "a", 21);
    let callee = b.path(&["apply"]);
    let (pe, ae) = (b.local(p), b.local(a));
    let call = b.call(callee, &[pe, ae]);
    let root = b.block(&[s0, s1], Some(call));
    let progs = [
        Prog { item: w.main, b: &b, root, lits: &lits },
        Prog { item, b: &ab, root: aroot, lits: &[] },
        Prog { item: ditem, b: &db, root: droot, lits: &[] },
    ];
    assert_eq!(run_progs(&w, &progs), (42, 1));
}

/// typeck.md, "A closure's identity": a closure is named by its owner, its ordinal AND the
/// owner's generic arguments, so a generic owner's closure is one type per owner instance.
/// `fn via<T>(t: T, n: u64) -> u64 { let f = move |x| { let k = &t; let v: u64 = x; v + n };
/// apply(f, 2) }`, the closure built in `via` and called in ANOTHER body (`apply<F: Fn(u64) ->
/// u64>`), and `let a: u8 = 7; let c: u64 = 9; via(a, 40) * 1000 + via(c, 30)` is 42032:
/// `2 + 40 = 42` and `2 + 30 = 32`. Two closure bodies are built, `Fn::call` at
/// `[Closure(via, 0, [u8]), (u64,)]` and at `[Closure(via, 0, [u64]), (u64,)]`, whose keys
/// differ by the owner's arguments (instance.rs, tag 20: the owner's arguments are the node's
/// children); `run_progs` builds once per KEY. Their environments differ too: `(t: u8, n:
/// u64)` lays `n` at offset 0 (falling alignment), `(t: u64, n: u64)` at 8, so had both shared
/// one key only one body would be built (1, not 2), and that body would read the other
/// instance's `t` where it reads `n` (`2 + 9`: 42011).
#[test]
fn a_closure_of_a_generic_owner_is_one_instance_per_owner_argument() {
    let mut w = open_world();
    let (item, ab, aroot) = apply(&mut w, 0, 1);
    let u64_ = w.fx.ty(PNode::Uint(UintTy::U64));
    let p0 = w.fx.param(0);
    let via = w.fx.func("via", 1, &[p0, u64_], u64_, Vec::new());
    w.fx.finish();
    // `via`.
    let mut vb = Bb::new();
    let mut vlits = StdVec::new();
    let (t, n) = (vb.param("t"), vb.param("n"));
    let x = vb.bind("x");
    let k = vb.bind("k");
    let te = vb.local(t);
    let rt = vb.borrow(Mutbl::Not, te);
    let sk = vb.let_(k, None, rt);
    let (sv, xe) = param_u64(&mut vb, x);
    let ne = vb.local(n);
    let sum = bin(&mut vb, BinOp::Add, xe, ne);
    let body = vb.block(&[sk, sv], Some(sum));
    let clo = vb.move_closure(&[x], body);
    let f = vb.bind("f");
    let sf = vb.let_(f, None, clo);
    let fe = vb.local(f);
    let two = lit(&mut vb, &mut vlits, 2);
    let callee = vb.path(&["apply"]);
    let call = vb.call(callee, &[fe, two]);
    let vroot = vb.block(&[sf], Some(call));
    // `main`.
    let mut b = Bb::new();
    let mut lits = StdVec::new();
    let a = b.bind("a");
    let seven = lit(&mut b, &mut lits, 7);
    let u8_ = b.ty(PNode::Uint(UintTy::U8));
    let s0 = b.let_(a, Some(u8_), seven);
    let (s1, c) = let_u64(&mut b, &mut lits, "c", 9);
    let (ae, forty) = (b.local(a), lit(&mut b, &mut lits, 40));
    let v1 = b.path(&["via"]);
    let first = b.call(v1, &[ae, forty]);
    let (ce, thirty) = (b.local(c), lit(&mut b, &mut lits, 30));
    let v2 = b.path(&["via"]);
    let second = b.call(v2, &[ce, thirty]);
    let thousand = lit(&mut b, &mut lits, 1000);
    let scaled = bin(&mut b, BinOp::Mul, first, thousand);
    let out = bin(&mut b, BinOp::Add, scaled, second);
    let root = b.block(&[s0, s1], Some(out));
    let progs = [
        Prog { item: w.main, b: &b, root, lits: &lits },
        Prog { item, b: &ab, root: aroot, lits: &[] },
        Prog { item: via, b: &vb, root: vroot, lits: &vlits },
    ];
    assert_eq!(run_progs(&w, &progs), (42032, 2));
}

// Captured PLACES (RFC 2229, "Capture precision": "a closure captures the precise path used
// ... rather than the entire variable"): stage 2 of blaze's captures, a closure's field holding
// `x.0` rather than `x` (walk/moves/env_places.rs). Ignored until the stage-2 switch sets
// blaze-typeck env_fields.rs `ActiveEnv` to `Places`; the switch removes each `ignore`. Under
// `WholeBindings` each one fails: the first two capture `x` whole, the third reads `x.1` while
// the closure borrows all of `x` mutably.

/// `base.index`, a tuple's field.
fn tuple_field(b: &mut Bb, base: ExprId, index: u32) -> ExprId {
    let at = b.int(None);
    b.exprs[at.ix()] = Expr::TupleField { base, index };
    at
}

/// `base.name`, a struct's field.
fn named_field(b: &mut Bb, base: ExprId, name: &str) -> ExprId {
    let at = b.int(None);
    b.exprs[at.ix()] = Expr::Field { base, name: blaze_typeck::Name::of(name) };
    at
}

/// `let x = (mk_d(), mk_d());` in `b`.
fn let_pair_of_d(b: &mut Bb) -> (StmtId, PatId) {
    let x = b.bind("x");
    let (m0, m1) = (b.path(&["mk_d"]), b.path(&["mk_d"]));
    let (d0, d1) = (b.call(m0, &[]), b.call(m1, &[]));
    let pair = b.tuple(&[d0, d1]);
    (b.let_(x, None, pair), x)
}

/// `move || { let r = &x.0; r.x }` in `b`.
fn move_reads_first(b: &mut Bb, x: PatId) -> ExprId {
    let xe = b.local(x);
    let x0 = tuple_field(b, xe, 0);
    let rx0 = b.borrow(Mutbl::Not, x0);
    let r = b.bind("r");
    let sr = b.let_(r, None, rx0);
    let re = b.local(r);
    let rx = named_field(b, re, "x");
    let body = b.block(&[sr], Some(rx));
    b.move_closure(&[], body)
}

/// "Capture precision" and "Capture modes": a `move` closure captures the place `x.0` by
/// value, so it moves `x.0` alone ("Destructors": "If a variable has been partially
/// initialized, only its initialized fields are dropped") and `x.1` stays usable. `let x =
/// (mk_d(), mk_d()); let c = move || { let r = &x.0; r.x }; c() + x.1.x` is 10 with TWO drops
/// of `D`: `x.0` with `c` and `x.1` with `x`, each once. NEGATIVE: under `WholeBindings` `c`
/// moves `x` whole and `x.1.x` is a use after the move (E0382).
#[test]
fn a_move_closure_takes_one_field_and_the_owner_keeps_using_the_other() {
    let w = world();
    let mut b = Bb::new();
    let lits: StdVec<(ExprId, u128)> = StdVec::new();
    let (s0, x) = let_pair_of_d(&mut b);
    let clo = move_reads_first(&mut b, x);
    let c = b.bind("c");
    let s1 = b.let_(c, None, clo);
    let ce = b.local(c);
    let call = b.call(ce, &[]);
    let xe = b.local(x);
    let x1 = tuple_field(&mut b, xe, 1);
    let x1x = named_field(&mut b, x1, "x");
    let sum = bin(&mut b, BinOp::Add, call, x1x);
    let root = b.block(&[s0, s1], Some(sum));
    assert_eq!(run_counting(&w, &b, root, &lits), (10, 1, 2));
}

/// "Drop order" and "Destructors": the closure's captured field is dropped when the closure
/// is, and the variable's scope end drops only what was not moved out of it. `let x =
/// (mk_d(), mk_d()); let n: u64 = { let c = move || { let r = &x.0; r.x }; c() }; let k: u64
/// = drops(); k * 100 + n` is 105 with two drops in all: at `c`'s scope end exactly ONE `D` was
/// dropped (`x.0`, the closure's), and `x`'s end drops `x.1` alone (not `x.0` again: three
/// drops, and the host's check of a live `D` would see a dead one). NEGATIVE: a closure moving
/// `x` whole drops both at `c`'s end: 205.
#[test]
fn a_captured_field_drops_with_the_closure_and_the_rest_with_its_variable() {
    let w = world();
    let mut b = Bb::new();
    let mut lits = StdVec::new();
    let (s0, x) = let_pair_of_d(&mut b);
    let clo = move_reads_first(&mut b, x);
    let c = b.bind("c");
    let sc = b.let_(c, None, clo);
    let ce = b.local(c);
    let call = b.call(ce, &[]);
    let inner = b.block(&[sc], Some(call));
    let u64_ = b.ty(PNode::Uint(UintTy::U64));
    let n = b.bind("n");
    let s1 = b.let_(n, Some(u64_), inner);
    let k = b.bind("k");
    let callee = b.path(&["drops"]);
    let counted = b.call(callee, &[]);
    let s2 = b.let_(k, Some(u64_), counted);
    let hundred = lit(&mut b, &mut lits, 100);
    let ke = b.local(k);
    let scaled = bin(&mut b, BinOp::Mul, ke, hundred);
    let ne = b.local(n);
    let out = bin(&mut b, BinOp::Add, scaled, ne);
    let root = b.block(&[s0, s1, s2], Some(out));
    assert_eq!(run_counting(&w, &b, root, &lits), (105, 1, 2));
}

/// "Capture modes": a closure assigning `x.0` captures that place by mutable borrow, and the
/// write lands in `x`; "Capture precision": `x.1` is not borrowed, so the owner reads it while
/// the closure lives. `let x: (u64, u64) = (1, 2); let c = || x.0 = x.0 + 5; let k: u64 =
/// x.1; c(); k * 100 + x.0 * 10 + x.1` is 262 (a copy of `x.0` in the closure would leave
/// 212). NEGATIVE: under `WholeBindings` the closure borrows all of `x` mutably and `let k =
/// x.1` while it lives is E0503.
#[test]
fn a_closure_borrowing_one_field_mutably_writes_through_to_its_variable() {
    let w = world();
    let mut b = Bb::new();
    let mut lits = StdVec::new();
    let u64_ = b.ty(PNode::Uint(UintTy::U64));
    let pair_ty = {
        let l = b.t.intern_list(&[u64_, u64_]).expect("intern");
        b.ty(PNode::Tuple(l))
    };
    let x = b.bind("x");
    let (one, two) = (lit(&mut b, &mut lits, 1), lit(&mut b, &mut lits, 2));
    let pair = b.tuple(&[one, two]);
    let s0 = b.let_(x, Some(pair_ty), pair);
    let (xl, xr) = (b.local(x), b.local(x));
    let (lhs, rhs0) = (tuple_field(&mut b, xl, 0), tuple_field(&mut b, xr, 0));
    let five = lit(&mut b, &mut lits, 5);
    let rhs = bin(&mut b, BinOp::Add, rhs0, five);
    let set = assign(&mut b, lhs, rhs);
    let clo = b.closure(&[], set);
    let c = b.bind("c");
    let s1 = b.let_(c, None, clo);
    let k = b.bind("k");
    let xk = b.local(x);
    let x1 = tuple_field(&mut b, xk, 1);
    let s2 = b.let_(k, Some(u64_), x1);
    let ce = b.local(c);
    let call = b.call(ce, &[]);
    let s3 = b.semi(call);
    let (ke, hundred) = (b.local(k), lit(&mut b, &mut lits, 100));
    let a = bin(&mut b, BinOp::Mul, ke, hundred);
    let xa = b.local(x);
    let xa0 = tuple_field(&mut b, xa, 0);
    let ten = lit(&mut b, &mut lits, 10);
    let bb = bin(&mut b, BinOp::Mul, xa0, ten);
    let xb = b.local(x);
    let xb1 = tuple_field(&mut b, xb, 1);
    let ab = bin(&mut b, BinOp::Add, a, bb);
    let out = bin(&mut b, BinOp::Add, ab, xb1);
    let root = b.block(&[s0, s1, s2, s3], Some(out));
    assert_eq!(run(&w, &b, root, &lits), (262, 1));
}

/// `|| { let e = x.0; e.x }` in `b`: a closure that moves the field `x.0` out of its
/// environment (FnOnce: "Closure types", a closure that moves a capture out implements only
/// `FnOnce`).
fn consumes_first(b: &mut Bb, x: PatId) -> ExprId {
    let xe = b.local(x);
    let x0 = tuple_field(b, xe, 0);
    let e = b.bind("e");
    let se = b.let_(e, None, x0);
    let ee = b.local(e);
    let ex = named_field(b, ee, "x");
    let body = b.block(&[se], Some(ex));
    b.closure(&[], body)
}

/// `let k: u64 = drops();` in `b`.
fn let_drops(b: &mut Bb, name: &str) -> (StmtId, PatId) {
    let k = b.bind(name);
    let callee = b.path(&["drops"]);
    let counted = b.call(callee, &[]);
    let u64_ = b.ty(PNode::Uint(UintTy::U64));
    (b.let_(k, Some(u64_), counted), k)
}

/// "Capture precision" and "Capture modes": a closure moving `x.0` out captures that PLACE by
/// value, its environment owns `x.0` alone, and its call (FnOnce) moves the field into `e`,
/// dropped at the body's end ("Destructors"). `let x = (mk_d(), mk_d()); let f = || { let e =
/// x.0; e.x }; let v: u64 = f(); let k: u64 = drops(); v * 100 + k * 10 + x.1.x` is 515 with
/// TWO drops in all: `x.0` once, by the call (k is 1 right after it), and `x.1` once, at `x`'s
/// scope end (not `x.0` again: a third drop, and the host's check of a live `D` would see a
/// moved one). NEGATIVE twin: under `WholeBindings` the closure takes `x` whole and `x.1.x` is a
/// use after the move (E0382); a body that also dropped its env field at its end would count 3.
#[test]
fn an_fn_once_closure_consumes_a_captured_field_and_the_owner_drops_the_rest() {
    let w = world();
    let mut b = Bb::new();
    let mut lits = StdVec::new();
    let (s0, x) = let_pair_of_d(&mut b);
    let clo = consumes_first(&mut b, x);
    let f = b.bind("f");
    let s1 = b.let_(f, None, clo);
    let fe = b.local(f);
    let call = b.call(fe, &[]);
    let v = b.bind("v");
    let u64_ = b.ty(PNode::Uint(UintTy::U64));
    let s2 = b.let_(v, Some(u64_), call);
    let (s3, k) = let_drops(&mut b, "k");
    let (ve, hundred) = (b.local(v), lit(&mut b, &mut lits, 100));
    let vs = bin(&mut b, BinOp::Mul, ve, hundred);
    let (ke, ten) = (b.local(k), lit(&mut b, &mut lits, 10));
    let ks = bin(&mut b, BinOp::Mul, ke, ten);
    let xe = b.local(x);
    let x1 = tuple_field(&mut b, xe, 1);
    let x1x = named_field(&mut b, x1, "x");
    let sum = bin(&mut b, BinOp::Add, vs, ks);
    let out = bin(&mut b, BinOp::Add, sum, x1x);
    let root = b.block(&[s0, s1, s2, s3], Some(out));
    assert_eq!(run_counting(&w, &b, root, &lits), (515, 1, 2));
}

/// The same closure dropped UNCALLED ("Drop order": a closure's captures are dropped with it):
/// its environment's `x.0` is dropped at `f`'s scope end, `x.1` at `x`'s. `let x = (mk_d(),
/// mk_d()); let n: u64 = { let f = || { let e = x.0; e.x }; drops() }; let k: u64 = drops(); k
/// * 100 + n * 10 + x.1.x` is 105 (none dropped while `f` lives, one right after) with TWO drops
/// in all, each `D` once. NEGATIVE twin: an owner that also dropped `x.0` at its end (the field
/// moved into the environment not marked moved) counts 3, and the host's check of a live `D`
/// fails on the second drop of `x.0`; a closure that took `x` whole drops both at `f`'s end
/// (205).
#[test]
fn an_fn_once_closure_dropped_uncalled_drops_its_captured_field_once() {
    let w = world();
    let mut b = Bb::new();
    let mut lits = StdVec::new();
    let (s0, x) = let_pair_of_d(&mut b);
    let clo = consumes_first(&mut b, x);
    let f = b.bind("f");
    let sf = b.let_(f, None, clo);
    let callee = b.path(&["drops"]);
    let counted = b.call(callee, &[]);
    let inner = b.block(&[sf], Some(counted));
    let n = b.bind("n");
    let u64_ = b.ty(PNode::Uint(UintTy::U64));
    let s1 = b.let_(n, Some(u64_), inner);
    let (s2, k) = let_drops(&mut b, "k");
    let (ke, hundred) = (b.local(k), lit(&mut b, &mut lits, 100));
    let ks = bin(&mut b, BinOp::Mul, ke, hundred);
    let (ne, ten) = (b.local(n), lit(&mut b, &mut lits, 10));
    let ns = bin(&mut b, BinOp::Mul, ne, ten);
    let xe = b.local(x);
    let x1 = tuple_field(&mut b, xe, 1);
    let x1x = named_field(&mut b, x1, "x");
    let sum = bin(&mut b, BinOp::Add, ks, ns);
    let out = bin(&mut b, BinOp::Add, sum, x1x);
    let root = b.block(&[s0, s1, s2], Some(out));
    assert_eq!(run_counting(&w, &b, root, &lits), (105, 0, 2));
}

// Field ORDER (walk.rs `decl_field`, the one lookup of a field by declaration index, closure.rs
// `field_offset` and walk/moves/env_places.rs `env_field_offset`): `layout` pushes an
// aggregate's rows in DECLARATION order and gives each the offset `ls` placed it at; a
// `repr(Rust)` aggregate's OFFSETS are reordered by decreasing alignment (layout.rs
// `univariant`, `Order::Decreasing`), never its rows. Each program below reads a `(u8, u64,
// u8)`-shaped value whose `u64` is placed FIRST: an index read as a placement order, or an
// offset taken from the wrong row, reads 40 where 7 is and the sum is wrong.

/// `operand as u64`.
fn as_u64(b: &mut Bb, operand: ExprId) -> ExprId {
    let ty = b.ty(PNode::Uint(UintTy::U64));
    let at = b.int(None);
    b.exprs[at.ix()] = Expr::Cast { operand, ty };
    at
}

/// `x as u64 * 10000 + y * 10 + z as u64` (a `u8` `x` and `z`, a `u64` `y`): 70402 for (7,
/// 40, 2), and a different number for any other order of the three.
fn weigh(b: &mut Bb, lits: &mut StdVec<(ExprId, u128)>, x: ExprId, y: ExprId, z: ExprId) -> ExprId {
    let xw = as_u64(b, x);
    let big = lit(b, lits, 10000);
    let xs = bin(b, BinOp::Mul, xw, big);
    let ten = lit(b, lits, 10);
    let ys = bin(b, BinOp::Mul, y, ten);
    let zw = as_u64(b, z);
    let s = bin(b, BinOp::Add, xs, ys);
    bin(b, BinOp::Add, s, zw)
}

/// The offsets of `S`'s fields `a`, `b`, `c` as the builder's `Layouts` places them.
fn s_offsets(w: &World) -> [usize; 3] {
    let table = PortableTys::new_in(Alloc::global());
    let adts = FxAdts { w, progs: &[], closures: &[], table: table.view() };
    let mut ground = PortableTys::new_in(Alloc::global());
    let none = ground.intern_list(&[]).expect("the empty list");
    let st = ground.intern(PNode::Adt(w.s, none)).expect("`S`");
    let mut ls = Layouts::new_in(Global);
    let id = ls.of(st, &mut LayoutCx { ground: &mut ground, facts: &adts, scratch: Alloc::global() }).unwrap_or_else(|e| panic!("`S` has no layout: {e}"));
    let at = |j: u32| usize::try_from(ls.field_offset(id, j).expect("`S`'s field has an offset")).expect("an offset");
    [at(0), at(1), at(2)]
}

/// Three readers of one reordered shape, each `f()` 70402: (1) a `move` closure over `let a:
/// u8 = 7; let b: u64 = 40; let c: u8 = 2;`, whose ENVIRONMENT is `(u8, u64, u8)`
/// (closure.rs `field_offset`); (2) a `move` closure over `let t: (u8, u64, u8) = (7, 40, 2);`
/// reading `t.0`, `t.1`, `t.2` (a TUPLE: whole, its fields by walk.rs `field`; by places,
/// three environment fields made from `t`'s rows by `env_field_offset`); (3) the same over `let
/// s = mk_s();` reading `s.a`, `s.b`, `s.c` (a STRUCT, `S { a: u8, b: u64, c: u8 }`, written by
/// the host at the offsets `Layouts` gives). First the premise: `S`'s `b` IS placed first (`[8,
/// 0, 9]`), so the order is really changed. NEGATIVE twin: rows pushed in placement order would
/// fail `decl_field`'s name check (`BuildError::FieldOrder`), and offsets read by placement
/// index give 400072 or another number, never 70402.
#[test]
fn a_reordered_aggregate_is_read_by_declaration_index_in_a_closure() {
    let w = world();
    let offsets = s_offsets(&w);
    assert_eq!(offsets, [8, 0, 9], "`S {{ a: u8, b: u64, c: u8 }}` is placed `b`, `a`, `c` (decreasing alignment)");
    S_AT.with(|s| s.set(offsets));
    for form in 0..3 {
        let mut b = Bb::new();
        let mut lits = StdVec::new();
        let u8_ = b.ty(PNode::Uint(UintTy::U8));
        let u64_ = b.ty(PNode::Uint(UintTy::U64));
        let mut stmts = StdVec::new();
        let (x, y, z) = match form {
            0 => {
                let mut put = |b: &mut Bb, lits: &mut StdVec<(ExprId, u128)>, name: &str, ty: PTy, v: u128| {
                    let p = b.bind(name);
                    let e = lit(b, lits, v);
                    stmts.push(b.let_(p, Some(ty), e));
                    p
                };
                let a = put(&mut b, &mut lits, "a", u8_, 7);
                let bp = put(&mut b, &mut lits, "b", u64_, 40);
                let c = put(&mut b, &mut lits, "c", u8_, 2);
                (b.local(a), b.local(bp), b.local(c))
            }
            1 => {
                let l = b.t.intern_list(&[u8_, u64_, u8_]).expect("intern");
                let tt = b.ty(PNode::Tuple(l));
                let t = b.bind("t");
                let (seven, forty, two) = (lit(&mut b, &mut lits, 7), lit(&mut b, &mut lits, 40), lit(&mut b, &mut lits, 2));
                let tup = b.tuple(&[seven, forty, two]);
                stmts.push(b.let_(t, Some(tt), tup));
                let (t0, t1, t2) = (b.local(t), b.local(t), b.local(t));
                (tuple_field(&mut b, t0, 0), tuple_field(&mut b, t1, 1), tuple_field(&mut b, t2, 2))
            }
            _ => {
                let s = b.bind("s");
                let callee = b.path(&["mk_s"]);
                let made = b.call(callee, &[]);
                stmts.push(b.let_(s, None, made));
                let (sa, sb, sc) = (b.local(s), b.local(s), b.local(s));
                (named_field(&mut b, sa, "a"), named_field(&mut b, sb, "b"), named_field(&mut b, sc, "c"))
            }
        };
        let weighed = weigh(&mut b, &mut lits, x, y, z);
        let clo = b.move_closure(&[], weighed);
        let f = b.bind("f");
        stmts.push(b.let_(f, None, clo));
        let fe = b.local(f);
        let call = b.call(fe, &[]);
        let root = b.block(&stmts, Some(call));
        assert_eq!(run(&w, &b, root, &lits), (70402, 1), "form {form}");
    }
}
