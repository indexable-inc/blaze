//! Coroutines built, split and run natively (coroutine.rs, "THE EMIT POINTS";
//! docs/v2/plan/design/coroutines.md, "The builder's two emit points"). Each test's program is
//! written in its doc with the rule of the Reference it relies on and the line it prints.
//!
//! ```text
//! #[lang = "sized"] trait Sized {}
//! #[lang = "copy"] trait Copy {}
//! #[lang = "future_trait"] trait Future {
//!     #[lang = "future_output"] type Output;
//!     fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output>;
//! }
//! #[lang = "into_future"] trait IntoFuture {
//!     type Output; type IntoFuture;
//!     fn into_future(self) -> Self::IntoFuture;
//! }
//! impl<F: Future> IntoFuture for F { type Output = F::Output; type IntoFuture = F; .. }
//! enum Poll<T> { Ready(T), Pending }   struct Pin<P> { pointer: P }   struct Context {}
//! struct D { x: u64 }  impl Drop for D   fn mk_d() -> D;                // the host counts drops
//! struct Twice { left: u64, value: u64 }  impl Future for Twice { type Output = u64; }
//! fn twice() -> Twice;                 // host: pending twice, then ready with 2
//! ```
//!
//! The EXECUTOR is the host's (`host_drive`), written by hand as the plan asks: `main` (LIR
//! written here, not Rust) makes the future by calling the `async fn`, and hands the host its
//! address with the addresses of `Future::poll` and `drop_in_place` at the coroutine. The host
//! polls with a `Pin` and a context of its own, reads each `Poll<u64>` by the layout the
//! builder's own `Layouts` gives, and drops the future last, always: after completion that
//! drop must do nothing.
//!
//! The planted bug (test 2's control): a suspend that does not advance the tag. blaze-lir's
//! own plants act only in its test builds, so the control is this file's: after the first
//! `Pending` the host stores `Unresumed` back over the tag, which is what such a suspend
//! leaves; the oracle (three polls, `twice()` called once) must then fail.

use core::cell::{Cell, RefCell};
use std::{alloc::Global, println, thread_local, vec::Vec as StdVec};

use blaze_lir::builder::{Param, Variadic};
use blaze_lir::func::{BlockKind, Conv, RefKind, SlotKind};
use blaze_lir::{Builder, Flags, Full, Ty};
use blaze_own::Alloc;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{
    Adt, BinOp, BodyView, Capture, Def, Expr, ExprId, InferCtx, ItemKey, LangItem, List, Mutbl, Name, Needs, Outcome, PNode, PTy, PTys, PatId,
    PortableTys, StmtId, UintTy,
};

use super::run_native_with;
use crate::layout::{LayoutCx, LayoutFacts, Layouts, Missing, ReprAttrs, ValidRange};
use crate::{Abi, Adts, AsyncItems, BuildError, Built, CoroutineFacts, Instance, StateShape, Subst};

struct World {
    fx: Fx,
    /// The trait `Future` and its method `poll`.
    future: ItemKey,
    poll_fn: ItemKey,
    /// The trait `IntoFuture`, its method `into_future`, and `fn identity<F>(this: F) -> F`,
    /// whose body is the blanket impl's `into_future`.
    into_future_tr: ItemKey,
    into_future_fn: ItemKey,
    identity: ItemKey,
    /// The enum `Poll` (`Ready` 0, `Pending` 1) and the struct `Pin`.
    poll: ItemKey,
    pin: ItemKey,
    /// `struct D { x: u64 }`, its host `Drop` fn, `fn mk_d() -> D` and `drop_in_place`.
    d: ItemKey,
    drop_d: ItemKey,
    mk_d: ItemKey,
    dip: ItemKey,
    /// `struct Twice`, `fn twice() -> Twice` and the host's `<Twice as Future>::poll`.
    twice_adt: ItemKey,
    twice: ItemKey,
    twice_poll: ItemKey,
    /// Names with no body of their own: `main`, the host's executor, and the item a
    /// coroutine's resume fn is named by (`AsyncItems::resume`).
    main: ItemKey,
    drive: ItemKey,
    resume: ItemKey,
    /// Each `async fn` added: its item and its signature's opaque type.
    fns: StdVec<(ItemKey, ItemKey)>,
}

/// `Poll<u64>` as the builder lays it out, for the host: where the tag is and how wide, the
/// tag of each variant, and where `Ready`'s field is.
#[derive(Clone, Copy)]
struct PollLayout {
    tag_at: usize,
    tag_bytes: usize,
    ready: u64,
    pending: u64,
    payload_at: usize,
}

/// What the host's executor does with the future.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Script {
    /// Polls until `Ready` (eight polls at most), then drops the future.
    Complete,
    /// Polls this many times, each `Pending`, then drops the future: a cancellation.
    CancelAfter(u32),
    /// `Complete`, with the planted bug: after the first `Pending` the state's tag is put back
    /// to `Unresumed`, as a suspend that did not advance it leaves it.
    TagNotAdvanced,
}

thread_local! {
    /// How many `D`s the host's `Drop` saw.
    static DROPS: Cell<u64> = const { Cell::new(0) };
    /// How many times `twice()` was called.
    static MADE: Cell<u32> = const { Cell::new(0) };
    /// How many polls the executor made.
    static POLLS: Cell<u32> = const { Cell::new(0) };
    static SCRIPT: Cell<Script> = const { Cell::new(Script::Complete) };
    static POLL: Cell<PollLayout> = const { Cell::new(PollLayout { tag_at: 0, tag_bytes: 0, ready: 0, pending: 0, payload_at: 0 }) };
    /// The driven state's tag: its offset and its width in bytes.
    static TAG: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
}

/// `D`'s `Drop::drop(&mut self)` on the host: counts one drop of a LIVE `D` (its `x` is 5: a
/// `D` whose bytes the state did not keep fails here).
extern "C" fn host_drop_d(p: *const u64) {
    // SAFETY: the glue passes the D's address, a live u64 (its `x`).
    assert_eq!(unsafe { p.read() }, 5, "a drop of a value that is not a live D");
    DROPS.with(|d| d.set(d.get() + 1));
}

/// `mk_d()`: `D { x: 5 }` through the result pointer (lib.rs, "Rust ABI").
extern "C" fn host_mk_d(out: *mut u64) {
    // SAFETY: `out` is the caller's result slot of one D.
    unsafe { out.write(5) };
}

/// `twice()`: a `Twice` whose two words are both 2 (so no field order matters here: the
/// builder never reads them, the host's `poll` takes word 0 as `left`, word 1 as `value`).
extern "C" fn host_twice(out: *mut u64) {
    MADE.with(|m| m.set(m.get() + 1));
    // SAFETY: `out` is the caller's result slot of one `Twice`, two u64s.
    unsafe {
        out.write(2);
        out.add(1).write(2);
    }
}

/// Stores variant tag `value` of a `Poll<u64>` at `out`.
///
/// # Safety
/// `out` is a `Poll<u64>`'s memory, writable.
unsafe fn write_tag(out: *mut u8, l: PollLayout, value: u64) {
    // SAFETY: the tag's `tag_bytes` (at most 8) lie inside the `Poll` at `tag_at`.
    unsafe { core::ptr::copy_nonoverlapping(value.to_le_bytes().as_ptr(), out.add(l.tag_at), l.tag_bytes) };
}

/// The variant tag of the `Poll<u64>` at `out`.
///
/// # Safety
/// `out` is a `Poll<u64>`'s memory, written.
unsafe fn read_tag(out: *const u8, l: PollLayout) -> u64 {
    let mut bytes = [0u8; 8];
    // SAFETY: as `write_tag`.
    unsafe { core::ptr::copy_nonoverlapping(out.add(l.tag_at), bytes.as_mut_ptr(), l.tag_bytes) };
    u64::from_le_bytes(bytes)
}

/// `<Twice as Future>::poll(self: Pin<&mut Twice>, cx) -> Poll<u64>`: `Pending` while `left`
/// is not 0 (counting it down), then `Ready(value)`.
extern "C" fn host_poll_twice(out: *mut u8, pin: *const *mut u64, _cx: *mut u8) {
    let l = POLL.with(Cell::get);
    // SAFETY: `pin` is the caller's `Pin<&mut Twice>`, one pointer to a live `Twice` (two
    // u64s); `out` is its result slot of one `Poll<u64>`.
    unsafe {
        let this = pin.read();
        let left = this.read();
        if left == 0 {
            out.add(l.payload_at).cast::<u64>().write_unaligned(this.add(1).read());
            write_tag(out, l, l.ready);
        } else {
            this.write(left - 1);
            write_tag(out, l, l.pending);
        }
    }
}

/// The executor: polls the future at `state` through `poll` (`Future::poll` at the coroutine)
/// as [`SCRIPT`] says, then drops it through `drop_state` (`drop_in_place` at the coroutine).
/// Returns the `Ready` value, or `u64::MAX` when it never was ready.
extern "C" fn host_drive(state: *mut u8, poll: usize, drop_state: usize) -> u64 {
    // SAFETY: `main` passes the addresses of the two emitted, mapped functions, whose
    // parameters are these: (result pointer, the address of a `Pin`, a context) and (state).
    let poll = unsafe { core::mem::transmute::<usize, extern "C" fn(*mut u8, *const *mut u8, *mut u8)>(poll) };
    // SAFETY: as above.
    let drop_state = unsafe { core::mem::transmute::<usize, extern "C" fn(*mut u8)>(drop_state) };
    let l = POLL.with(Cell::get);
    let script = SCRIPT.with(Cell::get);
    // Room for one `Poll<u64>`, aligned to 8.
    let mut out = [0u64; 4];
    let mut cx = 0u64;
    let mut value = u64::MAX;
    for n in 1..=8u32 {
        let pin: *mut u8 = state;
        poll(out.as_mut_ptr().cast(), &pin, core::ptr::from_mut(&mut cx).cast());
        POLLS.with(|p| p.set(n));
        // SAFETY: `poll` wrote a `Poll<u64>` at `out`.
        let tag = unsafe { read_tag(out.as_ptr().cast(), l) };
        if tag == l.ready {
            // SAFETY: `Ready`'s u64 is at `payload_at` inside `out`.
            value = unsafe { out.as_ptr().cast::<u8>().add(l.payload_at).cast::<u64>().read_unaligned() };
            break;
        }
        assert_eq!(tag, l.pending, "a `Poll` that is neither `Ready` nor `Pending`");
        match script {
            Script::CancelAfter(k) if k == n => break,
            Script::TagNotAdvanced if n == 1 => {
                let (at, bytes) = TAG.with(Cell::get);
                // SAFETY: the state's tag is `bytes` bytes at `at` (its `StateShape`).
                unsafe { core::ptr::write_bytes(state.add(at), 0, bytes) };
            }
            _ => {}
        }
    }
    drop_state(state);
    value
}

/// The world before `finish`: a test adds its `async fn`s ([`async_fn`]).
fn open_world() -> World {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, StdVec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let copy = fx.tr("Copy", 1, StdVec::new(), &[]);
    fx.lang(LangItem::Copy, copy);
    let (p0, unit) = (fx.param(0), fx.unit());
    let u64_ = fx.ty(PNode::Uint(UintTy::U64));
    fx.imp("copy_u64", 0, u64_, Some((copy, &[])), &[], StdVec::new());

    let (ready, pending): (&[PTy], &[PTy]) = (&[p0], &[]);
    let (poll, _) = fx.enum_("Poll", 1, &[("Ready", ready), ("Pending", pending)]);
    let pin = fx.strukt("Pin", 1, &[("pointer", p0)]);
    let context = fx.strukt("Context", 0, &[]);

    let future = fx.tr("Future", 1, StdVec::new(), &["Output"]);
    fx.lang(LangItem::Future, future);
    let fut_out = fx.alias(future, "Output", &[p0]);
    let self_mut = fx.r(Mutbl::Mut, p0);
    let pin_self = fx.adt(pin, &[self_mut]);
    let context_t = fx.adt(context, &[]);
    let cx = fx.r(Mutbl::Mut, context_t);
    let polled = fx.adt(poll, &[fut_out]);
    let poll_fn = fx.trait_fn(future, "poll", true, &[pin_self, cx], polled);

    let into_future_tr = fx.tr("IntoFuture", 1, StdVec::new(), &["Output", "IntoFuture"]);
    fx.lang(LangItem::IntoFuture, into_future_tr);
    let held = fx.alias(into_future_tr, "IntoFuture", &[p0]);
    let into_future_fn = fx.trait_fn(into_future_tr, "into_future", true, &[p0], held);
    fx.lang_def(LangItem::IntoFutureIntoFuture, Def::Fn(into_future_fn));
    let is_future = fx.bound(future, &[p0]);
    fx.imp("impl<F: Future> IntoFuture for F", 1, p0, Some((into_future_tr, &[])), &[("Output", fut_out), ("IntoFuture", p0)], StdVec::from([is_future]));
    let identity = fx.func("identity", 1, &[p0], p0, StdVec::new());

    let d = fx.strukt("D", 0, &[("x", u64_)]);
    let dt = fx.adt(d, &[]);
    let md = fx.r(Mutbl::Mut, dt);
    let drop_d = fx.func("drop_d", 0, &[md], unit, StdVec::new());
    let mk_d = fx.func("mk_d", 0, &[], dt, StdVec::new());
    let raw = fx.ty(PNode::Ptr(Mutbl::Mut, p0));
    let dip = fx.func("drop_in_place", 1, &[raw], unit, StdVec::new());
    fx.unsized_fn_param(dip, 0);

    let twice_adt = fx.strukt("Twice", 0, &[("left", u64_), ("value", u64_)]);
    let twice_t = fx.adt(twice_adt, &[]);
    fx.imp("impl Future for Twice", 0, twice_t, Some((future, &[])), &[("Output", u64_)], StdVec::new());
    let twice = fx.func("twice", 0, &[], twice_t, StdVec::new());
    let twice_poll = fx.func("twice_poll", 0, &[], unit, StdVec::new());

    let main = fx.func("main", 0, &[], u64_, StdVec::new());
    let drive = fx.func("drive", 0, &[], u64_, StdVec::new());
    let resume = fx.func("coroutine_resume", 0, &[], unit, StdVec::new());
    World {
        fx,
        future,
        poll_fn,
        into_future_tr,
        into_future_fn,
        identity,
        poll,
        pin,
        d,
        drop_d,
        mk_d,
        dip,
        twice_adt,
        twice,
        twice_poll,
        main,
        drive,
        resume,
        fns: StdVec::new(),
    }
}

/// `async fn <name>() -> u64`, added to `w` (still open): its signature returns an opaque type
/// bounded `Future<Output = u64>` (the Reference, "Async functions": "An async function is
/// roughly equivalent to a function that returns `impl Future` and with an `async move` block
/// as its body").
fn async_fn(w: &mut World, name: &str, future: &str) -> ItemKey {
    let u64_ = w.fx.ty(PNode::Uint(UintTy::U64));
    // The opaque type's own name in the world (`future`), beside the fn's.
    let opaque = w.fx.opaque_key(future);
    let none = w.fx.list(&[]);
    let opaque_t = w.fx.ty(PNode::Opaque(opaque, none));
    let is_future = w.fx.bound(w.future, &[opaque_t]);
    let output_is = w.fx.proj(w.future, &[opaque_t], "Output", u64_);
    // `Sized` too, as the front end files an async fn's opaque (blaze-typeck suite/opaque.rs
    // `async_world`): a caller's `.await` selects the blanket `impl<F: Future> IntoFuture for
    // F`, whose `F` is implicitly `Sized`.
    let sized = w.fx.key_of("Sized");
    let is_sized = w.fx.bound(sized, &[opaque_t]);
    w.fx.set_opaque_bounds(opaque, StdVec::from([is_future, output_is, is_sized]));
    let item = w.fx.func(name, 0, &[], opaque_t, StdVec::new());
    w.fns.push((item, opaque));
    item
}

/// One `async fn`'s body: its item, its builder, its root (the `async move` block) and its
/// literals.
struct Prog<'a> {
    item: ItemKey,
    b: &'a Bb,
    root: ExprId,
    lits: &'a [(ExprId, u128)],
}

struct FxAdts<'a> {
    w: &'a World,
    progs: &'a [Prog<'a>],
    /// Each coroutine's facts, (owner, ordinal, env, output) of `table`: the seam's answer
    /// made from the owner's body and check record (`coroutine_facts`).
    rows: &'a [(ItemKey, u32, PTy, PTy)],
    table: PTys<'a>,
    /// Each coroutine built and split so far: (owner, ordinal, its state).
    states: &'a RefCell<StdVec<(ItemKey, u32, StateShape)>>,
}

impl LayoutFacts for FxAdts<'_> {
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

    /// The blanket impl, read by hand: `<F as IntoFuture>::IntoFuture` is `F`; every future
    /// of this world has `Output = u64`.
    fn normalize(&self, trait_: ItemKey, name: Name, args: &[PTy], ground: &mut PortableTys<'_>) -> Result<PTy, BuildError> {
        if trait_ == self.w.into_future_tr && name == Name::of("IntoFuture") {
            return args.first().copied().ok_or(BuildError::Projection { trait_, name });
        }
        if name == Name::of("Output") && (trait_ == self.w.into_future_tr || trait_ == self.w.future) {
            return ground.intern(PNode::Uint(UintTy::U64)).map_err(|_| BuildError::Malformed { what: "the ground table is full" });
        }
        Err(BuildError::Projection { trait_, name })
    }

    fn coroutine(&self, owner: ItemKey, ordinal: u32) -> Option<CoroutineFacts<'_>> {
        let &(_, _, env, output) = self.rows.iter().find(|r| r.0 == owner && r.1 == ordinal)?;
        Some(CoroutineFacts { types: self.table, env, output })
    }

    fn coroutine_state(&self, owner: ItemKey, ordinal: u32, args: &[PTy], _: PTys<'_>) -> Result<Option<StateShape>, Missing> {
        if !args.is_empty() || !self.rows.iter().any(|r| r.0 == owner && r.1 == ordinal) {
            return Ok(None);
        }
        self.states.borrow().iter().find(|s| s.0 == owner && s.1 == ordinal).map(|s| Some(s.2)).ok_or(Missing { item: owner })
    }

    /// An `async fn`'s opaque type is its coroutine: the one block of its body.
    fn opaque_hidden(&self, opaque: ItemKey, args: &[PTy], ground: &mut PortableTys<'_>) -> Result<PTy, BuildError> {
        let full = |_| BuildError::Malformed { what: "the ground table is full" };
        let found = self.w.fns.iter().find(|f| f.1 == opaque).and_then(|f| self.rows.iter().find(|r| r.0 == f.0));
        let Some(&(item, ordinal, _, _)) = found else {
            return Err(BuildError::UnsupportedSig { what: "an opaque type that is no `async fn`'s of this test" });
        };
        let list = ground.intern_list(args).map_err(full)?;
        crate::coroutine_identity(ground, item, ordinal, list)
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

    fn abi(&self, _: ItemKey) -> Option<Abi> {
        Some(Abi::Rust)
    }

    fn drop_impl(&self, ty: PTy, ground: &mut PortableTys<'_>) -> Result<Option<ItemKey>, BuildError> {
        Ok(match ground.view().node(ty) {
            Ok(PNode::Adt(k, _)) if k == self.w.d => Some(self.w.drop_d),
            _ => None,
        })
    }

    fn lang_fn(&self, name: &str) -> Option<ItemKey> {
        (name == "drop_in_place").then_some(self.w.dip)
    }

    fn future_poll(&self) -> Option<ItemKey> {
        Some(self.w.poll_fn)
    }

    fn async_items(&self) -> Option<AsyncItems> {
        Some(AsyncItems { into_future: self.w.into_future_tr, poll: self.w.poll, ready: 0, pending: 1, pin: self.w.pin, resume: self.w.resume })
    }
}

/// `lhs op rhs`.
fn bin(b: &mut Bb, op: BinOp, lhs: ExprId, rhs: ExprId) -> ExprId {
    let at = b.int(None);
    b.exprs[at.ix()] = Expr::Binary { op, lhs, rhs };
    at
}

/// An integer literal of value `v`, noted in `lits`.
fn lit(b: &mut Bb, lits: &mut StdVec<(ExprId, u128)>, v: u128) -> ExprId {
    let e = b.int(None);
    lits.push((e, v));
    e
}

/// `let <name>: u64 = <init>;`
fn let_u64(b: &mut Bb, name: &str, init: ExprId) -> (StmtId, PatId) {
    let p = b.bind(name);
    let t = b.ty(PNode::Uint(UintTy::U64));
    (b.let_(p, Some(t), init), p)
}

/// `<callee>().await`.
fn call_await(b: &mut Bb, callee: &str) -> ExprId {
    let f = b.path(&[callee]);
    let call = b.call(f, &[]);
    b.await_(call)
}

/// A new body marked an `async fn`'s returning `u64`.
fn async_body() -> Bb {
    let mut b = Bb::new();
    let out = b.ty(PNode::Uint(UintTy::U64));
    b.async_fn(out);
    b
}

/// A fn `(ptr, ptr, ptr)` that passes its parameters on to `to` and returns: the placed
/// `<Twice as Future>::poll`, whose body is the host's (the run names a host fn by item, and
/// `Future::poll`'s item is the coroutines' entries' too).
fn forward3(to: ItemKey) -> Built<Global> {
    let mut b: Builder<Global, Full> = Builder::new_in(64, Global);
    let own = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::PTR)], &[]);
    b.set_signature(own);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let (x, y, z) = (b.param(Ty::PTR), b.param(Ty::PTR), b.param(Ty::PTR));
    let callee = b.reference(RefKind::Func, 0);
    b.call(Flags::NONE, callee, own, &[x, y, z]);
    b.ret(&[]);
    let func = b.finish().unwrap_or_else(|e| panic!("the forwarder is not LIR: {e:?}"));
    let mut refs = StdVec::new_in(Global);
    refs.push(Instance { item: to, args: List::EMPTY });
    let traps = StdVec::new_in(*refs.allocator());
    let func = blaze_lir::VerifiedFunc::new(func).unwrap_or_else(|e| panic!("the forwarder does not verify: {e}"));
    Built { func, refs, statics: StdVec::new_in(Global), thread_locals: StdVec::new_in(Global), vtables: StdVec::new_in(Global), traps, copy_insts: 0 }
}

/// `main`, in LIR: a slot of the state's size and alignment; the `async fn` called with it as
/// the result pointer (`refs[0]`); the host's executor (`refs[3]`) called with the state's
/// address and the addresses of `Future::poll` (`refs[1]`) and `drop_in_place` (`refs[2]`) at
/// the coroutine; its value returned.
fn main_lir(state: StateShape, refs: [Instance; 4]) -> Built<Global> {
    let mut b: Builder<Global, Full> = Builder::new_in(64, Global);
    let own = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I64)]);
    let made = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[]);
    let driven = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::PTR)], &[Param::of(Ty::I64)]);
    b.set_signature(own);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let slot = b.slot(state.size, state.align, SlotKind::Temp);
    let at = b.slot_addr(slot);
    let make = b.reference(RefKind::Func, 0);
    b.call(Flags::NONE, make, made, &[at]);
    let poll = b.reference(RefKind::Func, 1);
    let poll = b.func_addr(poll);
    let drop_state = b.reference(RefKind::Func, 2);
    let drop_state = b.func_addr(drop_state);
    let drive = b.reference(RefKind::Func, 3);
    let value = b.call(Flags::NONE, drive, driven, &[at, poll, drop_state]);
    b.ret(&[value]);
    let func = b.finish().unwrap_or_else(|e| panic!("main is not LIR: {e:?}"));
    let mut all = StdVec::new_in(Global);
    all.extend(refs);
    let traps = StdVec::new_in(*all.allocator());
    let func = blaze_lir::VerifiedFunc::new(func).unwrap_or_else(|e| panic!("main does not verify: {e}"));
    Built { func, refs: all, statics: StdVec::new_in(Global), thread_locals: StdVec::new_in(Global), vtables: StdVec::new_in(Global), traps, copy_insts: 0 }
}

/// What one run gave: the executor's value (`u64::MAX`: never ready), its polls, the drops of
/// `D` the host counted and the calls of `twice()`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Ran {
    value: u64,
    polls: u32,
    drops: u64,
    made: u32,
}

/// Checks each `async fn` body of `progs` (awaited fns before the fns awaiting them; the last
/// is the one `main` drives), and for each, in that order: builds and splits its coroutine
/// (`build_coroutine`: the state's shape becomes the seam's answer, so the next body may hold
/// that state by value) and builds the fn itself, which makes the state. Then every instance
/// those name is built by a work list (`into_future` at each awaited type: the blanket impl's
/// body; `drop_in_place` at each type that is no coroutine: its glue), all are placed callees
/// first, and `main` runs natively under `script`.
fn run(w: &World, progs: &[Prog<'_>], script: Script) -> Ran {
    run_laid(w, progs, script, None).unwrap_or_else(|e| panic!("the coroutine build failed: {e}"))
}

/// [`run`], the last of `progs` (the one `main` drives) built into the state `laid` gives
/// (coroutines_emit/state_names.rs); a build refused is the error.
fn run_laid(w: &World, progs: &[Prog<'_>], script: Script, laid: Option<&state_names::Laid>) -> Result<Ran, BuildError> {
    let check = |view: &BodyView<'_>| {
        let mut ty = InferCtx::new_in(Alloc::global());
        let c = match blaze_typeck::check(view, &w.fx, &mut ty, &Needs::new_in(Alloc::global()), Alloc::global()) {
            Ok(Outcome::Done(c)) => c,
            Ok(Outcome::Need(n)) => panic!("the check needed {:?}", n.questions),
            Err(error) => panic!("the check failed: {error:?}"),
        };
        assert!(c.diags.is_empty(), "diagnostics: {:?}", c.diags);
        c
    };
    let views: StdVec<_> = progs.iter().map(|p| p.b.view(p.item, p.root)).collect();
    let checked: StdVec<_> = views.iter().map(|view| check(view)).collect();
    let mut table = PortableTys::new_in(Alloc::global());
    let mut rows = StdVec::new();
    for ((p, view), c) in progs.iter().zip(views.iter()).zip(checked.iter()) {
        let s = w.fx.sig_of(p.item).expect("the fn's signature");
        // The root (the fn's own block) first: `opaque_hidden` takes an owner's first row as its
        // fn's future. A body may hold more `async` blocks, nested in the root.
        let mut blocks: StdVec<(u32, ExprId)> = crate::coroutines_in(view).collect();
        let root_at = blocks.iter().position(|c| c.1 == p.root).expect("the body's root is its async block");
        let root = blocks.remove(root_at);
        blocks.insert(0, root);
        for &(ordinal, _) in blocks.iter() {
            let (env, output) = crate::coroutine_facts(view, c, ordinal, s.parent_count + s.own_count, &mut table, Alloc::global())
                .unwrap_or_else(|e| panic!("coroutine {ordinal}'s facts: {e}"));
            rows.push((p.item, ordinal, env, output));
        }
    }
    let states = RefCell::new(StdVec::new());
    let adts = FxAdts { w, progs, rows: &rows, table: table.view(), states: &states };
    let mut ground = PortableTys::new_in(Alloc::global());
    let none = ground.intern_list(&[]).expect("the empty list");
    let u64g = ground.intern(PNode::Uint(UintTy::U64)).expect("u64");

    // `Poll<u64>` as the builder lays it out, for the host.
    let poll_args = ground.intern_list(&[u64g]).expect("a list");
    let poll_u64 = ground.intern(PNode::Adt(w.poll, poll_args)).expect("`Poll<u64>`");
    let mut ls = Layouts::new_in(Global);
    let id = ls
        .of(poll_u64, &mut LayoutCx { ground: &mut ground, facts: &adts, scratch: Alloc::global() })
        .unwrap_or_else(|e| panic!("`Poll<u64>` has no layout: {e}"));
    let ready = ls.tag_write(id, 0).expect("`Poll<u64>` stores a tag for `Ready`");
    let pending = ls.tag_write(id, 1).expect("`Poll<u64>` stores a tag for `Pending`");
    let payload = ls.variant(id, 0).and_then(|v| ls.field_offset(v, 0)).expect("`Ready`'s field has an offset");
    POLL.with(|p| {
        p.set(PollLayout {
            tag_at: ready.offset as usize,
            tag_bytes: 1usize << ready.scalar.prim.align(),
            ready: ready.value as u64,
            pending: pending.value as u64,
            payload_at: payload as usize,
        })
    });

    // Each coroutine, then its fn; the last is the one driven.
    let mut bodies: StdVec<(ItemKey, List, Built<Global>)> = StdVec::new();
    let mut driven = None;
    for (i, p) in progs.iter().enumerate() {
        let ordinal = crate::coroutines_in(&views[i]).find(|c| c.1 == p.root).map(|c| c.0).expect("the body's root is its async block");
        // Each NESTED block first, by ascending expression id: a fixture body makes an inner
        // block before the block holding it, so a state held by value in another's is laid
        // out and built before it (as `progs` orders awaited fns first). Its poll, resume and
        // drop are placed like a fn's future's: the root polls it by `.await`.
        let mut nested: StdVec<(u32, ExprId)> = crate::coroutines_in(&views[i]).filter(|c| c.1 != p.root).collect();
        nested.sort_by_key(|c| c.1.0);
        for &(inner, _) in nested.iter() {
            let mut subst = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
            let co = crate::build_coroutine(&views[i], &checked[i], inner, &mut subst, &adts, None, Global)?;
            states.borrow_mut().push((p.item, inner, co.state));
            let ty = crate::coroutine_identity(&mut ground, p.item, inner, none).unwrap_or_else(|e| panic!("the nested coroutine's type: {e}"));
            let at = ground.intern_list(&[ty]).expect("a list");
            bodies.push((w.resume, at, co.resume));
            bodies.push((w.poll_fn, at, co.poll));
            bodies.push((w.dip, at, co.drop));
        }
        let mut subst = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
        let plan = laid.filter(|_| i + 1 == progs.len()).map(state_names::Laid::plan);
        let co = crate::build_coroutine(&views[i], &checked[i], ordinal, &mut subst, &adts, plan, Global)?;
        states.borrow_mut().push((p.item, ordinal, co.state));
        let ty = crate::coroutine_identity(&mut ground, p.item, ordinal, none).unwrap_or_else(|e| panic!("the coroutine's type: {e}"));
        let at = ground.intern_list(&[ty]).expect("a list");
        let sig = w.fx.sig_of(p.item).expect("the fn's signature");
        let mut subst = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
        let made = crate::build(&views[i], &checked[i], &sig, &mut subst, &adts, Global).unwrap_or_else(|e| panic!("the build of {:?} failed: {e}", p.item));
        driven = Some((p.item, at, co.state));
        bodies.push((w.resume, at, co.resume));
        bodies.push((w.poll_fn, at, co.poll));
        bodies.push((w.dip, at, co.drop));
        bodies.push((p.item, none, made));
    }
    let twice_t = ground.intern(PNode::Adt(w.twice_adt, none)).expect("`Twice`");
    let twice_at = ground.intern_list(&[twice_t]).expect("a list");
    bodies.push((w.poll_fn, twice_at, forward3(w.twice_poll)));
    for (_, _, built) in bodies.iter() {
        super::emits(built, &[]);
    }

    // `drop_in_place`'s written body (never built: each instance is its argument's glue) and
    // `identity`'s, the blanket impl's `into_future`: `fn identity<F>(this: F) -> F { this }`.
    let mut gb = Bb::new();
    gb.param("p");
    let groot = gb.block(&[], None);
    let gview = gb.view(w.dip, groot);
    let gchecked = check(&gview);
    let mut ib = Bb::new();
    let this = ib.param("this");
    let moved = ib.local(this);
    let iroot = ib.block(&[], Some(moved));
    let iview = ib.view(w.identity, iroot);
    let ichecked = check(&iview);
    let mut todo: StdVec<(ItemKey, List)> = bodies.iter().flat_map(|(_, _, b)| b.refs.iter().map(|r| (r.item, r.args))).collect();
    while let Some((item, args)) = todo.pop() {
        if bodies.iter().any(|(i, a, _)| *i == item && *a == args) {
            continue;
        }
        let gargs = ground.view().list(args).expect("the instance's arguments").to_vec();
        let built = if item == w.dip {
            let sig = w.fx.sig_of(w.dip).expect("`drop_in_place`'s signature");
            let mut subst = Subst { args: &gargs, ground: &mut ground, scratch: Alloc::global() };
            crate::build_drop_glue(&gview, &gchecked, &sig, &mut subst, &adts, Global).unwrap_or_else(|e| panic!("the glue at {gargs:?} failed: {e}"))
        } else if item == w.into_future_fn {
            let sig = w.fx.sig_of(w.identity).expect("`identity`'s signature");
            let mut subst = Subst { args: &gargs, ground: &mut ground, scratch: Alloc::global() };
            crate::build(&iview, &ichecked, &sig, &mut subst, &adts, Global).unwrap_or_else(|e| panic!("`into_future` at {gargs:?} failed: {e}"))
        } else {
            // A host fn.
            continue;
        };
        super::emits(&built, &[]);
        todo.extend(built.refs.iter().map(|r| (r.item, r.args)));
        bodies.push((item, args, built));
    }

    let (item, at, state) = driven.expect("one `async fn` to drive");
    TAG.with(|t| t.set((state.tag_offset as usize, (state.tag_ty.bits(64) / 8) as usize)));
    let main = main_lir(
        state,
        [Instance { item, args: none }, Instance { item: w.poll_fn, args: at }, Instance { item: w.dip, args: at }, Instance { item: w.drive, args: none }],
    );
    // Callees before callers (the run places each fn after every fn it names).
    let mut fns: StdVec<(ItemKey, Option<List>, &Built<Global>)> = StdVec::new();
    let mut placed: StdVec<bool> = bodies.iter().map(|_| false).collect();
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
        (w.twice, host_twice as usize as u64),
        (w.twice_poll, host_poll_twice as usize as u64),
        (w.drive, host_drive as usize as u64),
    ];
    DROPS.with(|d| d.set(0));
    MADE.with(|m| m.set(0));
    POLLS.with(|p| p.set(0));
    SCRIPT.with(|s| s.set(script));
    let value = run_native_with(&host, &fns);
    Ok(Ran { value, polls: POLLS.with(Cell::get), drops: DROPS.with(Cell::get), made: MADE.with(Cell::get) })
}

/// `async fn one() -> u64 { 7 }`.
///
/// The Reference, "Async functions": "Async functions do no work when called: instead, they
/// capture their arguments into a future. When polled, that future will execute the function's
/// body." "Async blocks": "An async block is a variant of a block expression which evaluates
/// to a future. The final expression of the block, if present, determines the result value of
/// the future." So calling `one` makes a state whose tag is `Unresumed`, and the first poll
/// runs the body to its end: `Poll::Ready(7)`. The executor's drop after it does nothing.
///
/// Prints: `coroutine emit 1: ready(7) at poll 1`
#[test]
fn an_async_fn_with_no_await_is_ready_at_its_first_poll() {
    let mut w = open_world();
    let one = async_fn(&mut w, "one", "one_future");
    w.fx.finish();
    let mut b = async_body();
    let mut lits = StdVec::new();
    let seven = lit(&mut b, &mut lits, 7);
    let body = b.block(&[], Some(seven));
    let root = b.async_block(body, Capture::Move);
    let ran = run(&w, &[Prog { item: one, b: &b, root, lits: &lits }], Script::Complete);
    assert_eq!(ran, Ran { value: 7, polls: 1, drops: 0, made: 0 });
    println!("coroutine emit 1: ready({}) at poll {}", ran.value, ran.polls);
}

/// `async fn two() -> u64 { let a: u64 = 40; let r: u64 = twice().await; a + r }`, built as
/// `two`'s body in `b`. Returns the root.
fn two_body(b: &mut Bb, lits: &mut StdVec<(ExprId, u128)>) -> ExprId {
    let forty = lit(b, lits, 40);
    let (s0, a) = let_u64(b, "a", forty);
    let awaited = call_await(b, "twice");
    let (s1, r) = let_u64(b, "r", awaited);
    let (ae, re) = (b.local(a), b.local(r));
    let sum = bin(b, BinOp::Add, ae, re);
    let body = b.block(&[s0, s1], Some(sum));
    b.async_block(body, Capture::Move)
}

/// `async fn two() -> u64 { let a: u64 = 40; let r: u64 = twice().await; a + r }`, `twice()`
/// a future that is pending twice and then ready with 2.
///
/// The Reference, "Await expressions": "If the call to `poll` returns `Poll::Pending`, then
/// the future returns `Poll::Pending`, suspending its state so that, when the surrounding
/// async context is re-polled, execution returns to step 3"; "Otherwise the call to `poll`
/// must have returned `Poll::Ready`, in which case the value contained in the `Poll::Ready`
/// variant is used as the result of the `await` expression itself." So polls 1 and 2 return
/// `Pending`, poll 3 returns `Ready(42)`: `a`, written before the first suspend, is read after
/// the last (it is kept in the state), and `twice()` is evaluated once ("Create a future by
/// calling `IntoFuture::into_future` on the future operand" is step 1, not re-run by a
/// re-poll, which "returns to step 3").
///
/// The control (the planted bug: a suspend that does not advance the tag): with the tag put
/// back to `Unresumed` after the first `Pending`, the second poll runs the body from its
/// start: `twice()` is called again and `Ready` comes one poll late. The oracle above (three
/// polls, one call) fails on it.
///
/// Prints: `coroutine emit 2: ready(42) at poll 3, twice() called 1 time; tag not advanced: ready(42) at poll 4, twice() called 2 times (caught)`
#[test]
fn an_await_pending_twice_is_ready_at_the_third_poll_with_its_locals_kept() {
    let mut w = open_world();
    let two = async_fn(&mut w, "two", "two_future");
    w.fx.finish();
    let mut b = async_body();
    let mut lits = StdVec::new();
    let root = two_body(&mut b, &mut lits);
    let progs = [Prog { item: two, b: &b, root, lits: &lits }];
    let ran = run(&w, &progs, Script::Complete);
    assert_eq!(ran, Ran { value: 42, polls: 3, drops: 0, made: 1 });
    let planted = run(&w, &progs, Script::TagNotAdvanced);
    assert_ne!((planted.polls, planted.made), (3, 1), "the control: a tag that is not advanced went unnoticed");
    assert_eq!(planted, Ran { value: 42, polls: 4, drops: 0, made: 2 });
    println!(
        "coroutine emit 2: ready({}) at poll {}, twice() called {} time; tag not advanced: ready({}) at poll {}, twice() called {} times (caught)",
        ran.value, ran.polls, ran.made, planted.value, planted.polls, planted.made
    );
}

/// `async fn hold() -> u64 { let d = mk_d(); let r: u64 = twice().await; r + 1 }`: `d`, whose
/// type has a destructor, is live across the await.
///
/// The Reference, "Destructors": "When control flow leaves a drop scope all variables
/// associated to that scope are dropped in reverse order of declaration (for variables) or
/// creation (for temporaries)." On a future dropped while suspended the Reference is silent
/// (coroutines.md, "Decisions"): the state's drop leaves every scope open at that await as a
/// `return` there would, so `d` is dropped exactly once, by the state's drop, when the future
/// is dropped after one `Pending`. When the future completes, `d` is dropped exactly once by
/// the body, at its block's end, and the drop of the completed future drops nothing (the
/// executor always drops the future last: a second drop of `d` would count 2). The host's
/// `Drop` also checks the `D` it is given is live (`x == 5`): the state kept its bytes.
///
/// Prints: `coroutine emit 3: dropped while suspended after poll 1: D dropped 1 time; completed with 3 at poll 3: D dropped 1 time`
#[test]
fn a_local_live_across_an_await_is_dropped_once_on_cancel_and_once_on_completion() {
    let mut w = open_world();
    let hold = async_fn(&mut w, "hold", "hold_future");
    w.fx.finish();
    let mut b = async_body();
    let mut lits = StdVec::new();
    let d = b.bind("d");
    let mk = b.path(&["mk_d"]);
    let made = b.call(mk, &[]);
    let s0 = b.let_(d, None, made);
    let awaited = call_await(&mut b, "twice");
    let (s1, r) = let_u64(&mut b, "r", awaited);
    let re = b.local(r);
    let one = lit(&mut b, &mut lits, 1);
    let sum = bin(&mut b, BinOp::Add, re, one);
    let body = b.block(&[s0, s1], Some(sum));
    let root = b.async_block(body, Capture::Move);
    let progs = [Prog { item: hold, b: &b, root, lits: &lits }];
    let cancelled = run(&w, &progs, Script::CancelAfter(1));
    assert_eq!(cancelled, Ran { value: u64::MAX, polls: 1, drops: 1, made: 1 });
    let completed = run(&w, &progs, Script::Complete);
    assert_eq!(completed, Ran { value: 3, polls: 3, drops: 1, made: 1 });
    println!(
        "coroutine emit 3: dropped while suspended after poll {}: D dropped {} time; completed with {} at poll {}: D dropped {} time",
        cancelled.polls, cancelled.drops, completed.value, completed.polls, completed.drops
    );
}

/// `async fn inner() -> u64 { let r: u64 = twice().await; r + 39 }` and
/// `async fn outer() -> u64 { let a: u64 = 1; let x: u64 = inner().await; x + a }`.
///
/// The Reference, "Async functions": "When called, an async function evaluates to a future":
/// `inner()` in `outer` makes `inner`'s state, which `outer`'s state holds by value while it
/// awaits it ("Await expressions", step 2: "Evaluate the future to a future `tmp`"), pinned
/// in place (step 3). Each poll of `outer` polls `inner` (step 4), which polls `twice()`: two
/// `Pending`s pass up through both suspends, and the third poll is `Ready(2 + 39 + 1)`.
/// `inner`'s state is dropped by `outer`'s body once it is ready (its drop is the pass's, and
/// does nothing for a state that returned).
///
/// Prints: `coroutine emit 4: ready(42) at poll 3 through two coroutines, twice() called 1 time`
#[test]
fn an_async_fn_awaits_another_async_fn() {
    let mut w = open_world();
    let inner = async_fn(&mut w, "inner", "inner_future");
    let outer = async_fn(&mut w, "outer", "outer_future");
    w.fx.finish();
    let mut ib = async_body();
    let mut ilits = StdVec::new();
    let awaited = call_await(&mut ib, "twice");
    let (s0, r) = let_u64(&mut ib, "r", awaited);
    let re = ib.local(r);
    let add = lit(&mut ib, &mut ilits, 39);
    let sum = bin(&mut ib, BinOp::Add, re, add);
    let body = ib.block(&[s0], Some(sum));
    let iroot = ib.async_block(body, Capture::Move);

    let mut ob = async_body();
    let mut olits = StdVec::new();
    let one = lit(&mut ob, &mut olits, 1);
    let (s0, a) = let_u64(&mut ob, "a", one);
    let awaited = call_await(&mut ob, "inner");
    let (s1, x) = let_u64(&mut ob, "x", awaited);
    let (xe, ae) = (ob.local(x), ob.local(a));
    let sum = bin(&mut ob, BinOp::Add, xe, ae);
    let body = ob.block(&[s0, s1], Some(sum));
    let oroot = ob.async_block(body, Capture::Move);

    let progs = [Prog { item: inner, b: &ib, root: iroot, lits: &ilits }, Prog { item: outer, b: &ob, root: oroot, lits: &olits }];
    let ran = run(&w, &progs, Script::Complete);
    assert_eq!(ran, Ran { value: 42, polls: 3, drops: 0, made: 1 });
    println!("coroutine emit 4: ready({}) at poll {} through two coroutines, twice() called {} time", ran.value, ran.polls, ran.made);
}

// Captured PLACES in an `async move` block (RFC 2229, "Capture precision"; walk/moves/
// env_places.rs): a nested block captures `p.0` alone, its body owns that field
// (`Held::EnvPlace`), and the owner keeps `p.1`. Ignored until the stage-2 switch sets
// blaze-typeck env_fields.rs `ActiveEnv` to `Places`; the switch removes each `ignore`. Under
// `WholeBindings` the block moves `p` whole and the owner's `p.1.x` is a use after the move
// (E0382).

/// `base.index`, a tuple's field.
fn tuple_field(b: &mut Bb, base: ExprId, index: u32) -> ExprId {
    let at = b.int(None);
    b.exprs[at.ix()] = Expr::TupleField { base, index };
    at
}

/// `base.name`, a struct's field.
fn named_field(b: &mut Bb, base: ExprId, name: &str) -> ExprId {
    let at = b.int(None);
    b.exprs[at.ix()] = Expr::Field { base, name: Name::of(name) };
    at
}

/// `async fn owner() -> u64 { let mut k: u64 = 0; let inner = { let p = (mk_d(), mk_d()); let
/// blk = async move { <inner> }; k = p.1.x; blk }; let v: u64 = inner.await; v + k }`, the
/// inner block's body `{ let e = p.0; let r: u64 = twice().await; r + e.x }` (`late`: false)
/// or `{ let r: u64 = twice().await; let e = p.0; r + e.x }` (`late`: true: the field is still
/// the environment's at the suspension). `p` ends before the outer `.await` (its field word is
/// read at its block's end, never after a suspension). Returns the root.
fn place_moving_owner(b: &mut Bb, lits: &mut StdVec<(ExprId, u128)>, late: bool) -> ExprId {
    let u64_ = b.ty(PNode::Uint(UintTy::U64));
    let k = b.bind_mut("k");
    let zero = lit(b, lits, 0);
    let s0 = b.let_(k, Some(u64_), zero);
    // The inner block.
    let p = b.bind("p");
    let (m0, m1) = (b.path(&["mk_d"]), b.path(&["mk_d"]));
    let (d0, d1) = (b.call(m0, &[]), b.call(m1, &[]));
    let pair = b.tuple(&[d0, d1]);
    let sp = b.let_(p, None, pair);
    let pe = b.local(p);
    let p0 = tuple_field(b, pe, 0);
    let e = b.bind("e");
    let se = b.let_(e, None, p0);
    let awaited = call_await(b, "twice");
    let (sr, r) = let_u64(b, "r", awaited);
    let (re, ee) = (b.local(r), b.local(e));
    let ex = named_field(b, ee, "x");
    let sum = bin(b, BinOp::Add, re, ex);
    let stmts = if late { [sr, se] } else { [se, sr] };
    let inner_body = b.block(&stmts, Some(sum));
    let blk_e = b.async_block(inner_body, Capture::Move);
    let blk = b.bind("blk");
    let sb = b.let_(blk, None, blk_e);
    let ke = b.local(k);
    let pe1 = b.local(p);
    let p1 = tuple_field(b, pe1, 1);
    let p1x = named_field(b, p1, "x");
    let set = b.int(None);
    b.exprs[set.ix()] = Expr::Assign { lhs: ke, rhs: p1x };
    let sset = b.semi(set);
    let blk_out = b.local(blk);
    let made = b.block(&[sp, sb, sset], Some(blk_out));
    let inner = b.bind("inner");
    let si = b.let_(inner, None, made);
    let ie = b.local(inner);
    let waited = b.await_(ie);
    let (sv, v) = let_u64(b, "v", waited);
    let (ve, kr) = (b.local(v), b.local(k));
    let out = bin(b, BinOp::Add, ve, kr);
    let body = b.block(&[s0, si, sv], Some(out));
    b.async_block(body, Capture::Move)
}

/// A nested `async move` block that moves the field `p.0` out of its environment into `e`,
/// which is live across its `.await`; the owner reads `p.1` after making the block.
///
/// The Reference, "Capture precision" and "Destructors": the block owns `p.0` alone; `p`'s
/// scope end drops `p.1` alone ("If a variable has been partially initialized, only its
/// initialized fields are dropped"). Completed: `(2 + 5) + 5` is 12 at poll 3, with TWO drops
/// (`p.1` at its block's end, `e` at the inner body's end). Dropped while suspended after poll
/// 1: the outer state's drop drops `inner` (live across the outer `.await`), whose state's drop
/// drops `e` (live across the inner `.await`): again exactly TWO drops. NEGATIVE twin: a field
/// moved into the block and dropped again with `p`, or `e` dropped by both the body and the
/// state, counts 3 (and the host's check of a live `D` fails on the second drop).
#[test]
fn a_nested_async_block_moves_one_field_and_the_owner_keeps_the_other() {
    let mut w = open_world();
    let owner = async_fn(&mut w, "owner", "owner_future");
    w.fx.finish();
    let mut b = async_body();
    let mut lits = StdVec::new();
    let root = place_moving_owner(&mut b, &mut lits, false);
    let progs = [Prog { item: owner, b: &b, root, lits: &lits }];
    let completed = run(&w, &progs, Script::Complete);
    assert_eq!(completed, Ran { value: 12, polls: 3, drops: 2, made: 1 });
    let cancelled = run(&w, &progs, Script::CancelAfter(1));
    assert_eq!(cancelled, Ran { value: u64::MAX, polls: 1, drops: 2, made: 1 });
    println!("coroutine emit 5: a moved field: ready({}) at poll {}, D dropped {} times; cancelled: D dropped {} times", completed.value, completed.polls, completed.drops, cancelled.drops);
}

/// As above, with the field moved AFTER the inner `.await`: at the suspension `p.0` is still
/// the block's environment field, owned by its body (`Held::EnvPlace`). Dropped while
/// suspended after poll 1, the inner state's drop drops that field once (the body's scopes
/// left as a `return` there would, env_places.rs `drop_env_place`), and nothing else drops it:
/// TWO drops in all, as on completion (12). NEGATIVE twin: the unresumed-or-suspended state's
/// drop dropping the environment whole AND the body's held field would count 3.
#[test]
fn a_future_dropped_at_a_suspension_drops_its_captured_field_once() {
    let mut w = open_world();
    let owner = async_fn(&mut w, "owner", "owner_future");
    w.fx.finish();
    let mut b = async_body();
    let mut lits = StdVec::new();
    let root = place_moving_owner(&mut b, &mut lits, true);
    let progs = [Prog { item: owner, b: &b, root, lits: &lits }];
    let cancelled = run(&w, &progs, Script::CancelAfter(1));
    assert_eq!(cancelled, Ran { value: u64::MAX, polls: 1, drops: 2, made: 1 });
    let completed = run(&w, &progs, Script::Complete);
    assert_eq!(completed, Ran { value: 12, polls: 3, drops: 2, made: 1 });
    println!("coroutine emit 6: a field held at a suspension: cancelled: D dropped {} times; ready({}) with {} drops", cancelled.drops, completed.value, completed.drops);
}

/// A nested `async move` block over `let a: u8 = 7; let c: u64 = 40; let z: u8 = 2;`: its
/// ENVIRONMENT is `(u8, u64, u8)`, laid out `u64` first (decreasing alignment), read by
/// declaration index (walk.rs `decl_field`, coroutine.rs's environment offsets).
/// `async fn wide() -> u64 { ..; let blk = async move { (a as u64) * 10000 + c * 10 + (z as
/// u64) }; blk.await }` is 70402 at poll 1, and the nested block is polled to completion by the
/// outer `.await`. NEGATIVE twin: offsets taken by placement index read 40 where 7 is and give
/// another number.
#[test]
fn a_nested_async_block_reads_its_reordered_environment_by_declaration_index() {
    let mut w = open_world();
    let wide = async_fn(&mut w, "wide", "wide_future");
    w.fx.finish();
    let mut b = async_body();
    let mut lits = StdVec::new();
    let (u8_, u64_) = (b.ty(PNode::Uint(UintTy::U8)), b.ty(PNode::Uint(UintTy::U64)));
    let mut stmts = StdVec::new();
    let mut put = |b: &mut Bb, lits: &mut StdVec<(ExprId, u128)>, name: &str, ty: PTy, v: u128| {
        let p = b.bind(name);
        let e = lit(b, lits, v);
        stmts.push(b.let_(p, Some(ty), e));
        p
    };
    let a = put(&mut b, &mut lits, "a", u8_, 7);
    let c = put(&mut b, &mut lits, "c", u64_, 40);
    let z = put(&mut b, &mut lits, "z", u8_, 2);
    let cast = |b: &mut Bb, operand: ExprId| {
        let at = b.int(None);
        b.exprs[at.ix()] = Expr::Cast { operand, ty: u64_ };
        at
    };
    let ae = b.local(a);
    let aw = cast(&mut b, ae);
    let big = lit(&mut b, &mut lits, 10000);
    let xs = bin(&mut b, BinOp::Mul, aw, big);
    let ce = b.local(c);
    let ten = lit(&mut b, &mut lits, 10);
    let ys = bin(&mut b, BinOp::Mul, ce, ten);
    let ze = b.local(z);
    let zw = cast(&mut b, ze);
    let s = bin(&mut b, BinOp::Add, xs, ys);
    let sum = bin(&mut b, BinOp::Add, s, zw);
    let inner_body = b.block(&[], Some(sum));
    let blk_e = b.async_block(inner_body, Capture::Move);
    let blk = b.bind("blk");
    stmts.push(b.let_(blk, None, blk_e));
    let be = b.local(blk);
    let waited = b.await_(be);
    let body = b.block(&stmts, Some(waited));
    let root = b.async_block(body, Capture::Move);
    let ran = run(&w, &[Prog { item: wide, b: &b, root, lits: &lits }], Script::Complete);
    assert_eq!(ran, Ran { value: 70402, polls: 1, drops: 0, made: 0 });
    println!("coroutine emit 7: a nested block over (u8, u64, u8): ready({}) at poll {}", ran.value, ran.polls);
}

mod async_unwind;

/// Every slot kept across a suspension names a flow local (walk/state_names.rs; ASYNC.md P8).
mod state_names;
