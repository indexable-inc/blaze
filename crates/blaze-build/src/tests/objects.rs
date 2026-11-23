//! Trait objects built from the checker's own fixture world (`blaze_typeck::fixture`), emitted
//! and run natively: unsizing to `dyn`, `size_of_val` through the vtable, dynamic calls.
//! codegen.md, "Trait objects: vtables and dynamic calls".

use std::alloc::Global;
use std::vec::Vec as StdVec;

use blaze_ids::{ContentId, Hasher};
use blaze_own::Alloc;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{Adt, ExprId, InferCtx, ItemKey, LangItem, Mutbl, Needs, Outcome, PNode, PTy, PatId, PortableTys, UintTy};

use super::{Tables, run_native_vt};
use crate::layout::{LayoutFacts, Missing, ReprAttrs, ValidRange};
use crate::vtable::{ObjectMethod, ObjectSuper, ObjectTrait};
use crate::{Abi, Adts, Built, Intrinsic, Subst};

mod boxed_slice;
mod nested_dyn_tail;
mod nested_unsize;
mod rust_call;

/// ```text
/// #[lang = "sized"] trait Sized {}  #[lang = "unsize"] trait Unsize<U: ?Sized> {}
/// trait Tr { fn m(&self) -> u64; }        trait Sub: Tr { fn n(&self) -> u64; }
/// trait Other { fn o(&self) -> u64; }     trait Both: Tr + Other {}
/// impl Tr for u64 { fn m(&self) -> u64 { 7 } }   impl Tr for u8 { fn m(&self) -> u64 { 8 } }
/// impl Sub for u64 { fn n(&self) -> u64 { 11 } } impl Other for u64 { fn o(&self) -> u64 { 13 } }
/// impl Both for u64 {}
/// fn size_of_val<T: ?Sized>(p: &T) -> usize;  fn align_of_val<T: ?Sized>(p: &T) -> usize;
/// ```
struct World {
    fx: Fx,
    traits: StdVec<(ItemKey, StdVec<ObjectSuper>, StdVec<ObjectMethod>)>,
    /// (trait method, concrete type of `fx.t`, impl fn, the value its body returns).
    impls: StdVec<(ItemKey, PTy, ItemKey, u128)>,
    intrinsics: StdVec<(ItemKey, Intrinsic)>,
    /// `fn main() -> u64` and `fn main_us() -> usize`: the run's entry.
    main: ItemKey,
    main_us: ItemKey,
    tr: ItemKey,
    sub: ItemKey,
    other: ItemKey,
    both: ItemKey,
    u64_: PTy,
    u8_: PTy,
    boxes: Boxes,
    /// The fns of `extern "rust-call"` (a submodule's own items; none in the base world).
    rust_call: StdVec<ItemKey>,
}

fn id(s: &str) -> ContentId {
    let mut h = Hasher::new("blaze-build objects test shape");
    h.update(s.as_bytes());
    h.finish()
}

fn world() -> World {
    world_with(BoxField::Ptr)
}

/// Where the world's `Box<T>` holds its pointer: a plain `*const T`, or core's own shape on the
/// pinned nightly, `NonNull<T> { pointer: pattern_type!(*const T is !null) }`
/// (core/src/ptr/non_null.rs), whose field coerces by core/src/pat.rs's `impl<T, U>
/// CoerceUnsized<pattern_type!(*const U is !null)> for pattern_type!(*const T is !null) where
/// T: Unsize<U>`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BoxField {
    Ptr,
    NonNull,
}

fn world_with(field: BoxField) -> World {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let unsize = fx.tr("Unsize", 2, Vec::new(), &[]);
    fx.lang(LangItem::Unsize, unsize);
    let p0 = fx.param(0);
    let r_self = fx.r(Mutbl::Not, p0);
    let (u64_, u8_, usize_) = (fx.ty(PNode::Uint(UintTy::U64)), fx.u8(), fx.usize());
    let tr = fx.tr("Tr", 1, Vec::new(), &[]);
    let m = fx.trait_fn(tr, "m", true, &[r_self], u64_);
    let tr_self = fx.bound(tr, &[p0]);
    let sub = fx.tr("Sub", 1, std::vec![tr_self], &[]);
    let n = fx.trait_fn(sub, "n", true, &[r_self], u64_);
    let other = fx.tr("Other", 1, Vec::new(), &[]);
    let o = fx.trait_fn(other, "o", true, &[r_self], u64_);
    let other_self = fx.bound(other, &[p0]);
    let both = fx.tr("Both", 1, std::vec![tr_self, other_self], &[]);
    let mut impls = StdVec::new();
    for (label, tr_k, method, c, v) in [
        ("tr_u64", tr, m, u64_, 7),
        ("tr_u8", tr, m, u8_, 8),
        ("sub_u64", sub, n, u64_, 11),
        ("other_u64", other, o, u64_, 13),
    ] {
        let i = fx.imp(label, 0, c, Some((tr_k, &[])), &[], Vec::new());
        let rc = fx.r(Mutbl::Not, c);
        let name = if method == m { "m" } else if method == n { "n" } else { "o" };
        let f = fx.impl_fn(i, name, true, &[rc], u64_);
        impls.push((method, c, f, v));
    }
    fx.imp("both_u64", 0, u64_, Some((both, &[])), &[], Vec::new());
    let mut intrinsics = StdVec::new();
    for (name, which) in [("size_of_val", crate::intrinsics::Which::SizeOfVal), ("align_of_val", crate::intrinsics::Which::AlignOfVal)] {
        let f = fx.func(name, 1, &[r_self], usize_, Vec::new());
        fx.unsized_fn_param(f, 0);
        intrinsics.push((f, Intrinsic::Table(which)));
    }
    let main = fx.func("main", 0, &[], u64_, Vec::new());
    let main_us = fx.func("main_us", 0, &[], usize_, Vec::new());
    let boxes = boxes(&mut fx, p0, u64_, &mut impls, field);
    fx.finish();
    let method = |item| ObjectMethod { item, dispatch: true };
    let traits = std::vec![
        (tr, StdVec::new(), std::vec![method(m)]),
        (sub, std::vec![ObjectSuper::plain(tr)], std::vec![method(n)]),
        (other, StdVec::new(), std::vec![method(o)]),
        (both, std::vec![ObjectSuper::plain(tr), ObjectSuper::plain(other)], StdVec::new()),
        (boxes.ob, StdVec::new(), std::vec![method(boxes.get), method(boxes.take)]),
    ];
    World { fx, traits, impls, intrinsics, main, main_us, tr, sub, other, both, u64_, u8_, boxes, rust_call: StdVec::new() }
}

/// The boxed part of the world:
/// ```text
/// #[lang = "coerce_unsized"] trait CoerceUnsized<U> {}   #[lang = "deref"] trait Deref { type Target; }
/// #[lang = "owned_box"] struct Box<T: ?Sized> { p: *const T }
/// impl<T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<Box<U>> for Box<T> {}
/// impl<T: ?Sized> Deref for Box<T> { type Target = T; }
/// impl<T: ?Sized> Drop for Box<T> { fn drop(&mut self) }   // host: frees, as std's
/// trait Ob { fn get(&self) -> u64; fn take(self: Box<Self>) -> u64; }
/// struct A { x: u64 }  struct B { x: u64 }  struct Z;      // each a host `Drop` that logs
/// impl Ob for A { get: 1, take: 21 }  impl Ob for B { 2, 22 }  impl Ob for Z { 3, 23 }
/// fn mk_a() -> Box<A>; fn mk_b() -> Box<B>; fn mk_z() -> Box<Z>;   // host: allocate
/// fn raw_a() -> *mut A; fn ref_a() -> &'static A;                  // host
/// #[lang = "drop_in_place"] fn drop_in_place<T: ?Sized>(p: *mut T);
/// ```
struct Boxes {
    box_k: ItemKey,
    ob: ItemKey,
    get: ItemKey,
    take: ItemKey,
    dip: ItemKey,
    /// (ADT, its `Drop::drop` fn, run on the host).
    drops: StdVec<(ItemKey, ItemKey)>,
    /// (host fn, its address).
    host: StdVec<(ItemKey, u64)>,
    /// A, B, Z.
    concrete: [(ItemKey, PTy); 3],
}

fn boxes(fx: &mut Fx, p0: PTy, u64_: PTy, impls: &mut StdVec<(ItemKey, PTy, ItemKey, u128)>, field: BoxField) -> Boxes {
    let unsize = fx.key_of("Unsize");
    let coerce = fx.tr("CoerceUnsized", 2, Vec::new(), &[]);
    fx.lang(LangItem::CoerceUnsized, coerce);
    let deref = fx.tr("Deref", 1, Vec::new(), &["Target"]);
    fx.lang(LangItem::Deref, deref);
    let ptr_t = fx.ty(PNode::Ptr(Mutbl::Not, p0));
    let field_t = match field {
        BoxField::Ptr => ptr_t,
        BoxField::NonNull => {
            // `!null` is the address range `1 ..= usize::MAX` (typeck.md, "Pattern types").
            let usize_ = fx.usize();
            let lo = fx.ty(PNode::ConstValue { ty: usize_, bits: 1 });
            let hi = fx.ty(PNode::ConstValue { ty: usize_, bits: u128::from(u64::MAX) });
            let not_null = fx.t.intern_list(&[lo, hi]).expect("the world's table is full");
            let pat = fx.ty(PNode::Pat(ptr_t, not_null));
            let non_null = fx.strukt("NonNull", 1, &[("pointer", pat)]);
            fx.adt(non_null, &[p0])
        }
    };
    let box_k = fx.strukt("Box", 1, &[("p", field_t)]);
    fx.lang(LangItem::OwnedBox, box_k);
    let (box_t, p1) = (fx.adt(box_k, &[p0]), fx.param(1));
    let box_u = fx.adt(box_k, &[p1]);
    let t_unsize_u = fx.bound(unsize, &[p0, p1]);
    let cu = fx.imp("cu_box", 2, box_t, Some((coerce, &[box_u])), &[], std::vec![t_unsize_u]);
    fx.unsized_param(cu, 0);
    fx.unsized_param(cu, 1);
    let dr = fx.imp("deref_box", 1, box_t, Some((deref, &[])), &[("Target", p0)], Vec::new());
    fx.unsized_param(dr, 0);
    let unit = fx.unit();
    let ob = fx.tr("Ob", 1, Vec::new(), &[]);
    let r_self = fx.r(Mutbl::Not, p0);
    let get = fx.trait_fn(ob, "get", true, &[r_self], u64_);
    let take = fx.trait_fn(ob, "take", true, &[box_t], u64_);
    let mut drops = StdVec::new();
    let mut host = StdVec::new();
    let mut concrete = StdVec::new();
    let logs: [(&str, extern "C" fn(*const u64)); 3] = [("A", host_drop_a), ("B", host_drop_b), ("Z", host_drop_z)];
    for (i, (name, log)) in logs.into_iter().enumerate() {
        let k = if name == "Z" { fx.strukt(name, 0, &[]) } else { fx.strukt(name, 0, &[("x", u64_)]) };
        let c = fx.adt(k, &[]);
        let i = i as u128;
        let im = fx.imp(&std::format!("ob_{name}"), 0, c, Some((ob, &[])), &[], Vec::new());
        let rc = fx.r(Mutbl::Not, c);
        let g = fx.impl_fn(im, "get", true, &[rc], u64_);
        impls.push((get, c, g, 1 + i));
        let bc = fx.adt(box_k, &[c]);
        let t = fx.impl_fn(im, "take", true, &[bc], u64_);
        impls.push((take, c, t, 21 + i));
        let mc = fx.r(Mutbl::Mut, c);
        let d = fx.func(&std::format!("drop_{name}"), 0, &[mc], unit, Vec::new());
        drops.push((k, d));
        host.push((d, log as usize as u64));
        concrete.push((k, c));
    }
    let mut_box = fx.r(Mutbl::Mut, box_t);
    let box_drop = fx.func("box_drop", 1, &[mut_box], unit, Vec::new());
    fx.unsized_fn_param(box_drop, 0);
    drops.push((box_k, box_drop));
    host.push((box_drop, host_box_drop as usize as u64));
    let makers: [(&str, extern "C" fn(*mut *mut u64)); 3] = [("mk_a", host_mk_a), ("mk_b", host_mk_b), ("mk_z", host_mk_z)];
    for (j, (name, mk)) in makers.into_iter().enumerate() {
        let bc = fx.adt(box_k, &[concrete[j].1]);
        let f = fx.func(name, 0, &[], bc, Vec::new());
        host.push((f, mk as usize as u64));
    }
    let a = concrete[0].1;
    let raw_a = fx.ty(PNode::Ptr(Mutbl::Mut, a));
    let f = fx.func("raw_a", 0, &[], raw_a, Vec::new());
    host.push((f, host_raw_a as usize as u64));
    let ref_a = fx.r(Mutbl::Not, a);
    let f = fx.func("ref_a", 0, &[], ref_a, Vec::new());
    host.push((f, host_ref_a as usize as u64));
    let raw_t = fx.ty(PNode::Ptr(Mutbl::Mut, p0));
    let dip = fx.func("drop_in_place", 1, &[raw_t], unit, Vec::new());
    fx.unsized_fn_param(dip, 0);
    let concrete = [concrete[0], concrete[1], concrete[2]];
    Boxes { box_k, ob, get, take, dip, drops, host, concrete }
}

std::thread_local! {
    /// What the host saw, in order: a drop of A, B or Z logs its `x` (1, 2) or 3; a free logs
    /// 100 + the allocation's size; a `Box` of nothing allocated logs 99.
    static LOG: core::cell::RefCell<StdVec<u64>> = const { core::cell::RefCell::new(StdVec::new()) };
    /// The host allocator's live allocations (address, size): malloc-like, so `box_drop`
    /// frees by address.
    static LIVE: core::cell::RefCell<StdVec<(u64, u64)>> = const { core::cell::RefCell::new(StdVec::new()) };
}

fn log(v: u64) {
    LOG.with(|l| l.borrow_mut().push(v));
}

fn host_alloc(x: u64) -> *mut u64 {
    let layout = std::alloc::Layout::from_size_align(8, 8).expect("a layout");
    // SAFETY: the layout is of nonzero size.
    let p = unsafe { std::alloc::alloc(layout) }.cast::<u64>();
    assert!(!p.is_null(), "the host allocation failed");
    // SAFETY: `p` is a fresh allocation of one u64.
    unsafe { p.write(x) };
    LIVE.with(|l| l.borrow_mut().push((p as u64, 8)));
    p
}

/// A `Box<T>` is a one-field struct, an aggregate: the Rust ABI returns it through the result
/// pointer, the first argument (lib.rs, "Rust ABI").
fn host_box(out: *mut *mut u64, data: *mut u64) {
    // SAFETY: `out` is the caller's result slot of one pointer.
    unsafe { out.write(data) };
}

extern "C" fn host_mk_a(out: *mut *mut u64) {
    host_box(out, host_alloc(1));
}

extern "C" fn host_mk_b(out: *mut *mut u64) {
    host_box(out, host_alloc(2));
}

/// A `Box` of a zero-sized type allocates nothing: it holds a dangling, aligned pointer (std's
/// `NonNull::dangling`, the alignment), which its `Drop` never frees.
extern "C" fn host_mk_z(out: *mut *mut u64) {
    host_box(out, core::ptr::without_provenance_mut(1));
}

extern "C" fn host_raw_a() -> *mut u64 {
    host_alloc(1)
}

extern "C" fn host_ref_a() -> *mut u64 {
    static A: u64 = 1;
    (&raw const A).cast_mut()
}

extern "C" fn host_drop_a(p: *const u64) {
    // SAFETY: the glue passes the A's address, a live u64.
    log(unsafe { p.read() });
}

extern "C" fn host_drop_b(p: *const u64) {
    // SAFETY: as `host_drop_a`.
    log(unsafe { p.read() });
}

extern "C" fn host_drop_z(_: *const u64) {
    log(3);
}

/// std's `impl Drop for Box` (`Global.deallocate(ptr, Layout::for_value_raw(ptr))` when the size
/// is not 0), by a malloc-like table: `p` is the box, its word 0 the data pointer.
extern "C" fn host_box_drop(p: *const u64) {
    // SAFETY: the glue passes the box's address; its first word is the data pointer.
    let data = unsafe { p.read() };
    let found = LIVE.with(|l| {
        let mut l = l.borrow_mut();
        let at = l.iter().position(|&(a, _)| a == data)?;
        Some(l.swap_remove(at))
    });
    match found {
        Some((a, size)) => {
            let layout = std::alloc::Layout::from_size_align(size as usize, 8).expect("a layout");
            // SAFETY: `a` was allocated by `host_alloc` with this layout and is freed once.
            unsafe { std::alloc::dealloc(core::ptr::with_exposed_provenance_mut::<u8>(a as usize), layout) };
            log(100 + size);
        }
        // A panic here would abort (an `extern "C"` fn): an address the host never allocated
        // is logged as itself, for the test's assertion to show.
        None if data == 1 => log(99),
        None => log(data),
    }
}

impl World {
    fn object_trait(&self, t: ItemKey) -> Option<ObjectTrait<'_>> {
        let (_, supers, methods) = self.traits.iter().find(|(k, _, _)| *k == t)?;
        // The hash covers the trait and every trait above it (a stack, each once).
        let mut text = std::string::String::new();
        let mut stack = std::vec![t];
        let mut seen = StdVec::new();
        while let Some(k) = stack.pop() {
            if seen.contains(&k) {
                continue;
            }
            seen.push(k);
            let (_, s, m) = self.traits.iter().find(|(x, _, _)| *x == k)?;
            text.push_str(&std::format!("{k:?} {s:?} {m:?};"));
            stack.extend(s.iter().rev().map(|x| x.trait_));
        }
        Some(ObjectTrait { supers, tys: blaze_typeck::PTys::EMPTY, methods, shape: id(&text) })
    }
}

struct FxAdts<'a> {
    w: &'a World,
    lits: &'a [(ExprId, u128)],
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

    fn int_lit(&self, _: ItemKey, expr: ExprId) -> Option<u128> {
        self.lits.iter().find(|(e, _)| *e == expr).map(|&(_, v)| v)
    }

    fn int_pat(&self, _: ItemKey, _: PatId) -> Option<u128> {
        None
    }

    fn intrinsic(&self, item: ItemKey) -> Option<Intrinsic> {
        self.w.intrinsics.iter().find(|(k, _)| *k == item).map(|&(_, i)| i)
    }

    fn abi(&self, item: ItemKey) -> Option<Abi> {
        self.w.fx.sig_of(item).map(|_| if self.w.rust_call.contains(&item) { Abi::RustCall } else { Abi::Rust })
    }

    fn drop_impl(&self, ty: PTy, ground: &mut PortableTys<'_>) -> Result<Option<ItemKey>, crate::BuildError> {
        Ok(match ground.view().node(ty) {
            Ok(PNode::Adt(k, _)) => self.w.boxes.drops.iter().find(|(a, _)| *a == k).map(|&(_, d)| d),
            _ => None,
        })
    }

    fn lang_fn(&self, name: &str) -> Option<ItemKey> {
        (name == "drop_in_place").then_some(self.w.boxes.dip)
    }

    fn lang_adt(&self, item: LangItem) -> Option<ItemKey> {
        (item == LangItem::OwnedBox).then_some(self.w.boxes.box_k)
    }

    fn object_trait(&self, t: ItemKey) -> Option<ObjectTrait<'_>> {
        self.w.object_trait(t)
    }
}

/// Checks body `b` of `owner` (no generics) and builds it.
fn build(w: &World, b: &Bb, owner: ItemKey, root: ExprId, lits: &[(ExprId, u128)], ground: &mut PortableTys<'static>) -> Built<Global> {
    let view = b.view(owner, root);
    let mut ty = InferCtx::new_in(Alloc::global());
    let checked = match blaze_typeck::check(&view, &w.fx, &mut ty, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        Ok(Outcome::Need(n)) => panic!("the check of {owner:?} needed {:?}", n.questions),
        Err(error) => panic!("the check of {owner:?} failed: {error:?}"),
    };
    assert!(checked.diags.is_empty(), "diagnostics: {:?}", checked.diags);
    let sig = w.fx.sig_of(owner).expect("the owner's signature");
    let mut subst = Subst { args: &[], ground, scratch: Alloc::global() };
    let built = crate::build(&view, &checked, &sig, &mut subst, &FxAdts { w, lits }, Global).unwrap_or_else(|e| panic!("the build of {owner:?} failed: {e}"));
    super::emits(&built, &[]);
    built
}

/// `let x: T = <lit>; let r: &dyn <obj> = &x; <tail(r)>` as `main`, built with every impl fn
/// (`fn m(&self) -> u64 { <its value> }`) and run natively.
fn run_main(w: &World, x_ty: PTy, obj: ItemKey, tail: impl FnOnce(&mut Bb, ExprId) -> ExprId, owner: ItemKey) -> u64 {
    run_body(w, owner, |b| {
        let x = b.bind("x");
        let lit = b.int(None);
        let xt = map_ty(&w.fx, b, x_ty);
        let s0 = b.let_(x, Some(xt), lit);
        let r = b.bind("r");
        let r_ty = dyn_ty(b, obj, |b, d| b.ty(PNode::Ref(Mutbl::Not, d)));
        let xe = b.local(x);
        let borrow = b.borrow(Mutbl::Not, xe);
        let s1 = b.let_(r, Some(r_ty), borrow);
        let re = b.local(r);
        let out = tail(b, re);
        (b.block(&[s0, s1], Some(out)), std::vec![(lit, 5u128)])
    })
}

/// `wrap(dyn <obj>)` in the body's table.
fn dyn_ty(b: &mut Bb, obj: ItemKey, wrap: impl FnOnce(&mut Bb, PTy) -> PTy) -> PTy {
    let ex = b.ty(PNode::ExTrait(obj, blaze_typeck::List::EMPTY));
    let dyn_ = b.t.dynamic(&[ex]).expect("dyn");
    wrap(b, dyn_)
}

/// `Box<dyn Ob>` in the body's table.
fn box_dyn(w: &World, b: &mut Bb) -> PTy {
    let k = w.boxes.box_k;
    dyn_ty(b, w.boxes.ob, |b, d| {
        let l = b.t.intern_list(&[d]).expect("list");
        b.ty(PNode::Adt(k, l))
    })
}

/// `owner`'s body from `body` (its root and literal values), built with every impl fn (`{ <its
/// value> }`) and the drop glue of A, B, Z, their boxes, `Box<dyn Ob>` and `dyn Ob`, and run
/// natively; the host's log cleared first.
fn run_body(w: &World, owner: ItemKey, body: impl FnOnce(&mut Bb) -> (ExprId, StdVec<(ExprId, u128)>)) -> u64 {
    let mut ground = PortableTys::new_in(Alloc::global());
    let bx = &w.boxes;
    let full = "the ground table is full";
    let conc: StdVec<PTy> = bx.concrete.iter().map(|&(_, c)| ground_of(&w.fx, &mut ground, c)).collect();
    let ex = ground.intern(PNode::ExTrait(bx.ob, blaze_typeck::List::EMPTY)).expect(full);
    let dyn_ob = ground.dynamic(&[ex]).expect(full);
    let mut glue_tys: StdVec<PTy> = conc.clone();
    for &t in conc.iter().chain([dyn_ob].iter()) {
        let l = ground.intern_list(&[t]).expect(full);
        glue_tys.push(ground.intern(PNode::Adt(bx.box_k, l)).expect(full));
    }
    glue_tys.push(dyn_ob);
    let mut gb = Bb::new();
    gb.param("p");
    let groot = gb.block(&[], None);
    let view = gb.view(bx.dip, groot);
    let mut ty = InferCtx::new_in(Alloc::global());
    let checked = match blaze_typeck::check(&view, &w.fx, &mut ty, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        other => panic!("the check of `drop_in_place` did not finish: {:?}", other.err()),
    };
    let sig = w.fx.sig_of(bx.dip).expect("`drop_in_place`'s signature");
    let mut glues = StdVec::new();
    for &t in glue_tys.iter() {
        let args = ground.intern_list(&[t]).expect(full);
        let mut subst = Subst { args: &[t], ground: &mut ground, scratch: Alloc::global() };
        let built = crate::build_drop_glue(&view, &checked, &sig, &mut subst, &FxAdts { w, lits: &[] }, Global)
            .unwrap_or_else(|e| panic!("the glue at {t:?} failed: {e}"));
        super::emits(&built, &[]);
        glues.push((args, built));
    }
    let mut impl_built = StdVec::new();
    for &(_, _, f, v) in w.impls.iter() {
        let mut b = Bb::new();
        b.param("self");
        let e = b.int(None);
        let root = b.block(&[], Some(e));
        impl_built.push((f, build(w, &b, f, root, &[(e, v)], &mut ground)));
    }
    let mut b = Bb::new();
    let (root, lits) = body(&mut b);
    let main = build(w, &b, owner, root, &lits, &mut ground);
    let mut fns: StdVec<(ItemKey, Option<blaze_typeck::List>, &Built<Global>)> = glues.iter().map(|(a, g)| (bx.dip, Some(*a), g)).collect();
    fns.extend(impl_built.iter().map(|(f, b)| (*f, None, b)));
    fns.push((owner, None, &main));
    let impls: StdVec<(ItemKey, PTy, ItemKey)> = w.impls.iter().map(|&(m, c, f, _)| (m, ground_of(&w.fx, &mut ground, c), f)).collect();
    let (gu64, gu8) = (ground_of(&w.fx, &mut ground, w.u64_), ground_of(&w.fx, &mut ground, w.u8_));
    let d = Some(bx.dip);
    let sizes = [(gu64, 8, 8, None), (gu8, 1, 1, None), (conc[0], 8, 8, d), (conc[1], 8, 8, d), (conc[2], 0, 1, d)];
    let facts = |t| w.object_trait(t);
    let mut tables = Tables { facts: &facts, impls: &impls, sizes: &sizes, ground: &mut ground, placed: StdVec::new_in(Global) };
    LOG.with(|l| l.borrow_mut().clear());
    run_native_vt(&bx.host, &fns, Some(&mut tables))
}

fn logged() -> StdVec<u64> {
    LOG.with(|l| l.borrow().clone())
}

/// A primitive type of the world's table in the body's table (`let` annotations are the body's).
fn map_ty(fx: &Fx, b: &mut Bb, t: PTy) -> PTy {
    let node = fx.t.view().node(t).expect("a world type");
    b.ty(node)
}

fn ground_of(fx: &Fx, g: &mut PortableTys<'static>, t: PTy) -> PTy {
    g.intern(fx.t.view().node(t).expect("a world type")).expect("interned")
}

/// `<path>(r)`.
fn call_fn(b: &mut Bb, path: &str, r: ExprId) -> ExprId {
    let f = b.path(&[path]);
    b.call(f, &[r])
}

/// The Reference, "Type coercions", "Unsized Coercions" (`T` to `dyn U` when `T: U + Sized`)
/// and "Trait objects" (the pointer carries the vtable); `core::mem::size_of_val` /
/// `align_of_val`: "the size of the pointed-to value". Two objects of different sizes:
/// `&5u64 as &dyn Tr` gives 8 and 8, `&5u8 as &dyn Tr` 1 and 1, each read from its own table.
#[test]
fn size_of_val_reads_each_objects_own_table() {
    let w = world();
    let got = [
        run_main(&w, w.u64_, w.tr, |b, r| call_fn(b, "size_of_val", r), w.main_us),
        run_main(&w, w.u64_, w.tr, |b, r| call_fn(b, "align_of_val", r), w.main_us),
        run_main(&w, w.u8_, w.tr, |b, r| call_fn(b, "size_of_val", r), w.main_us),
        run_main(&w, w.u8_, w.tr, |b, r| call_fn(b, "align_of_val", r), w.main_us),
    ];
    assert_eq!(got, [8, 8, 1, 1]);
}

/// The Reference, "Trait objects": "a call to a method on a trait object is dispatched
/// dynamically, through the vtable"; and the trait object "implements ... any supertraits of
/// the base trait". `r.m()` on `&dyn Tr` reaches `u64`'s impl (7) and `u8`'s (8); on
/// `&dyn Sub`, Tr's `m` (7) and Sub's `n` (11) through the one table.
#[test]
fn a_dynamic_call_reaches_the_concrete_types_impl() {
    let w = world();
    let m = |b: &mut Bb, r| b.method(r, "m", &[]);
    let n = |b: &mut Bb, r| b.method(r, "n", &[]);
    let got = [
        run_main(&w, w.u64_, w.tr, m, w.main),
        run_main(&w, w.u8_, w.tr, m, w.main),
        run_main(&w, w.u64_, w.sub, m, w.main),
        run_main(&w, w.u64_, w.sub, n, w.main),
    ];
    assert_eq!(got, [7, 8, 7, 11]);
}

/// The Reference, "Unsized Coercions": "`dyn T` to `dyn U`, when `U` is one of `T`'s
/// supertraits". `&dyn Sub` to `&dyn Tr` (the first chain: the same table) then `.m()` is 7;
/// `&dyn Both` to `&dyn Other` (off the first chain: the table's embedded pointer) then `.o()`
/// is 13, and `size_of_val` through the loaded table is still 8.
#[test]
fn an_upcast_then_a_call() {
    let w = world();
    let upcast = |to: ItemKey, then: fn(&mut Bb, ExprId) -> ExprId| {
        move |b: &mut Bb, r: ExprId| {
            let t = b.bind("t");
            let ex = b.ty(PNode::ExTrait(to, blaze_typeck::List::EMPTY));
            let dyn_ = b.t.dynamic(&[ex]).expect("dyn");
            let t_ty = b.ty(PNode::Ref(Mutbl::Not, dyn_));
            let s = b.let_(t, Some(t_ty), r);
            let te = b.local(t);
            let out = then(b, te);
            b.block(&[s], Some(out))
        }
    };
    let got = [
        run_main(&w, w.u64_, w.sub, upcast(w.tr, |b, t| b.method(t, "m", &[])), w.main),
        run_main(&w, w.u64_, w.both, upcast(w.other, |b, t| b.method(t, "o", &[])), w.main),
        run_main(&w, w.u64_, w.both, upcast(w.other, |b, t| call_fn(b, "size_of_val", t)), w.main_us),
    ];
    assert_eq!(got, [7, 13, 8]);
}

/// `let <name>: <ty> = <maker>();`, its local.
fn let_call(b: &mut Bb, name: &str, ty: PTy, maker: &str) -> (blaze_typeck::StmtId, PatId) {
    let p = b.bind(name);
    let f = b.path(&[maker]);
    let call = b.call(f, &[]);
    (b.let_(p, Some(ty), call), p)
}

/// `{ let x: Box<dyn Ob> = <maker>(); ..; <tail> }` as `main`, with one binding per maker.
fn run_boxes(w: &World, makers: &[&str], tail: impl FnOnce(&mut Bb, &[PatId]) -> (ExprId, StdVec<(ExprId, u128)>)) -> u64 {
    run_body(w, w.main, |b| {
        let mut stmts = StdVec::new();
        let mut pats = StdVec::new();
        for (i, m) in makers.iter().enumerate() {
            let t = box_dyn(w, b);
            let (s, p) = let_call(b, &std::format!("x{i}"), t, m);
            stmts.push(s);
            pats.push(p);
        }
        let (out, lits) = tail(b, &pats);
        (b.block(&stmts, Some(out)), lits)
    })
}

/// A literal 5 as the block's value.
fn five(b: &mut Bb, _: &[PatId]) -> (ExprId, StdVec<(ExprId, u128)>) {
    let e = b.int(None);
    (e, std::vec![(e, 5)])
}

/// The Reference, "Type coercions", "Unsized Coercions" (`Box<T>` to `Box<U>` through
/// `CoerceUnsized`) and "Destructors" (a binding is dropped when its scope ends; a `Box`'s
/// destructor drops its contents and frees). `let x: Box<dyn Ob> = mk_a();` at the end of
/// `main`: A's own `Drop` once (logs its `x`, 1, read through the data word), then the free of
/// its 8 bytes (108); B the same with 2.
#[test]
fn a_boxed_object_is_dropped_once_through_its_table() {
    let w = world();
    assert_eq!(run_boxes(&w, &["mk_a"], five), 5);
    assert_eq!(logged(), [1, 108]);
    assert_eq!(run_boxes(&w, &["mk_b"], five), 5);
    assert_eq!(logged(), [2, 108]);
}

/// The Reference, "Destructors": "variables ... are dropped in reverse order of declaration".
/// `let x0 = mk_a(); let x1 = mk_b();` as `Box<dyn Ob>`: B (2) and its free, then A (1) and
/// its free, each through its own table's word 0.
#[test]
fn two_boxed_objects_drop_in_reverse_order() {
    let w = world();
    assert_eq!(run_boxes(&w, &["mk_a", "mk_b"], five), 5);
    assert_eq!(logged(), [2, 108, 1, 108]);
}

/// The Reference, "Dyn compatibility" (a `self: Box<Self>` receiver is dispatchable) and
/// "Destructors", "moved" values are not dropped: `x0.take()` on `Box<dyn Ob>` passes the data
/// word as the callee's `Box<A>` (the allocation, not a copy); the callee owns it and drops it at
/// its end (A once, then the free), and `main` drops nothing after the move. 21 is A's `take`.
#[test]
fn a_box_moved_into_a_self_box_call_is_dropped_by_the_callee() {
    let w = world();
    let got = run_boxes(&w, &["mk_a"], |b, p| {
        let x = b.local(p[0]);
        (b.method(x, "take", &[]), StdVec::new())
    });
    assert_eq!(got, 21);
    assert_eq!(logged(), [1, 108]);
}

/// core's own `Box` shape on the pinned nightly: the pointer is held in `NonNull<T> { pointer:
/// pattern_type!(*const T is !null) }` (`BoxField::NonNull`), so `Box<A>` to `Box<dyn Ob>`
/// passes through a pattern type on its way to the pointer: core/src/pat.rs's
/// `CoerceUnsized` impl for `pattern_type!(*const T is !null)` coerces it as its base (tempo:
/// der's `BytesOwned::default`, `Box::new([])` to `Box<[u8]>`, and every `Box`, `Rc`, `Arc`
/// unsizing through `NonNull`). `x0.take()` runs A's `take` (21) through the table the
/// coercion wrote, and the callee drops the box (A, then the free); a `Box<dyn Ob>` of B left
/// to scope drops B through its table (2) and frees (108).
/// Prints `test tests::objects::a_box_over_a_pattern_typed_pointer_unsizes_and_dispatches ... ok`.
/// Fails without walk/unsize_nested.rs's pattern-type step, and without walk.rs `layout`
/// laying out a pattern type's wide base before the row that holds it (run 042632-UZ: the
/// target `NonNull<dyn Ob>`'s row then began at the wide pointer's address field): the build
/// of `main` is refused ("an unsizing of other than a pointer or a struct holding one").
#[test]
fn a_box_over_a_pattern_typed_pointer_unsizes_and_dispatches() {
    let w = world_with(BoxField::NonNull);
    let got = run_boxes(&w, &["mk_a"], |b, p| {
        let x = b.local(p[0]);
        (b.method(x, "take", &[]), StdVec::new())
    });
    assert_eq!(got, 21);
    assert_eq!(logged(), [1, 108]);
    assert_eq!(run_boxes(&w, &["mk_b"], five), 5);
    assert_eq!(logged(), [2, 108]);
}

/// std's `Box`: a zero-sized `T` is never allocated; the box holds a dangling, aligned pointer
/// and its `Drop` frees nothing (`Layout::for_value_raw` is of size 0, read from the table's
/// word 1). `Box<Z>` as `Box<dyn Ob>`: Z's `Drop` runs once (3) through word 0; nothing is
/// freed (99).
#[test]
fn a_boxed_zero_sized_object_drops_and_frees_nothing() {
    let w = world();
    assert_eq!(run_boxes(&w, &["mk_z"], five), 5);
    assert_eq!(logged(), [3, 99]);
}

/// The Reference, "Destructors": a reference owns nothing, so a `&dyn Ob` going out of scope
/// drops nothing, though A has a `Drop` impl.
#[test]
fn a_reference_to_an_object_drops_nothing() {
    let w = world();
    let got = run_body(&w, w.main, |b| {
        let t = dyn_ty(b, w.boxes.ob, |b, d| b.ty(PNode::Ref(Mutbl::Not, d)));
        let (s, _) = let_call(b, "r", t, "ref_a");
        let (out, lits) = five(b, &[]);
        (b.block(&[s], Some(out)), lits)
    });
    assert_eq!(got, 5);
    assert_eq!(logged(), StdVec::<u64>::new());
}

/// `core::ptr::drop_in_place::<dyn Ob>` ("Executes the destructor (if any) of the pointed-to
/// value"): through the table's word 0 at the data word; it frees nothing (the allocation is the
/// host's, left as is). `let p: *mut dyn Ob = raw_a(); drop_in_place(p);`: A once.
#[test]
fn drop_in_place_of_an_object_runs_its_glue() {
    let w = world();
    let got = run_body(&w, w.main, |b| {
        let t = dyn_ty(b, w.boxes.ob, |b, d| b.ty(PNode::Ptr(Mutbl::Mut, d)));
        let (s, p) = let_call(b, "p", t, "raw_a");
        let f = b.path(&["drop_in_place"]);
        let pe = b.local(p);
        let call = b.call(f, &[pe]);
        let s1 = b.semi(call);
        let (out, lits) = five(b, &[]);
        (b.block(&[s, s1], Some(out)), lits)
    });
    assert_eq!(got, 5);
    assert_eq!(logged(), [1]);
}

/// What an edit rebuilds, as records: `main` (`let x = 5u64; let r: &dyn <obj> = &x; r.<method>()`)
/// as LIR text, and `u64`'s table of `obj`: its key, bytes and relocation targets.
struct Records {
    main: std::string::String,
    /// `main`'s record key, made before its build.
    main_key: crate::vtable::RecordKey,
    key: crate::vtable::VtableKey,
    bytes: StdVec<u8>,
    relocs: StdVec<(u32, crate::vtable::VtableTarget)>,
}

fn records(w: &World, obj: ItemKey, method: &str) -> Records {
    let mut ground = PortableTys::new_in(Alloc::global());
    let mut b = Bb::new();
    let x = b.bind("x");
    let lit = b.int(None);
    let xt = map_ty(&w.fx, &mut b, w.u64_);
    let s0 = b.let_(x, Some(xt), lit);
    let r = b.bind("r");
    let r_ty = dyn_ty(&mut b, obj, |b, d| b.ty(PNode::Ref(Mutbl::Not, d)));
    let xe = b.local(x);
    let borrow = b.borrow(Mutbl::Not, xe);
    let s1 = b.let_(r, Some(r_ty), borrow);
    let re = b.local(r);
    let out = b.method(re, method, &[]);
    let root = b.block(&[s0, s1], Some(out));
    let main = build(w, &b, w.main, root, &[(lit, 5)], &mut ground);
    let main_key = record_key_of(w, &b, w.main, root, &[(lit, 5)], &mut ground);
    let gu64 = ground_of(&w.fx, &mut ground, w.u64_);
    let facts = w.object_trait(obj).expect("the trait's facts");
    let alloc = blaze_own::testing::heap().alloc();
    let key = crate::vtable::vtable_key(gu64, obj, &[], facts.shape, &mut ground, alloc).expect("the key");
    let adts = FxAdts { w, lits: &[] };
    let shape = crate::vtable::Shape::of(obj, |t| w.object_trait(t), alloc).expect("the shape");
    let table = crate::vtable::Vtable::build(&shape, gu64, &[], 8, 8, None, &mut ground, &adts, alloc, Global).expect("the table");
    Records { main: blaze_lir::print(main.func.view()), main_key, key, bytes: table.bytes.to_vec(), relocs: table.relocs.to_vec() }
}

/// The key of `owner`'s record, made before its build: its instance key (code identity `owner`'s
/// alone: a trait edit changes none of its text) and its body's object reads.
fn record_key_of(w: &World, b: &Bb, owner: ItemKey, root: ExprId, lits: &[(ExprId, u128)], ground: &mut PortableTys<'static>) -> crate::vtable::RecordKey {
    let view = b.view(owner, root);
    let mut ty = InferCtx::new_in(Alloc::global());
    let Ok(Outcome::Done(checked)) = blaze_typeck::check(&view, &w.fx, &mut ty, &Needs::new_in(Alloc::global()), Alloc::global()) else {
        panic!("the check of {owner:?}");
    };
    let code = ContentId::of("objects test code", &owner.0.to_le_bytes());
    let instance = crate::instance_key(code, blaze_typeck::List::EMPTY, ground.view(), Alloc::global()).expect("an instance key");
    let mut subst = Subst { args: &[], ground, scratch: Alloc::global() };
    let reads = crate::object_reads(&view, &checked, &mut subst, &FxAdts { w, lits }, Global).expect("the object reads");
    crate::vtable::record_key(instance, &reads)
}

/// codegen.md, "What a function's key holds of a trait": a caller's record key holds the slot
/// it calls, so an edit to a trait rekeys exactly the callers whose slot moved. With `Tr { m }`
/// edited to `Tr { m, e }` (at the end) no existing caller's key changes; to `Tr { e, m }` (in
/// the middle) every caller's does (`m` 0 to 1, Sub's `n` 1 to 2). `impl Tr for u64`'s `m`
/// makes no dynamic call: its key changes under neither.
#[test]
fn a_trait_edit_rekeys_exactly_the_callers_whose_slot_moved() {
    let calls = |w: &World| [records(w, w.tr, "m").main_key, records(w, w.sub, "m").main_key, records(w, w.sub, "n").main_key];
    let callee = |w: &World| {
        let mut ground = PortableTys::new_in(Alloc::global());
        let &(_, _, f, _) = w.impls.first().expect("`impl Tr for u64`");
        let mut b = Bb::new();
        b.param("self");
        let e = b.int(None);
        let root = b.block(&[], Some(e));
        record_key_of(w, &b, f, root, &[(e, 7)], &mut ground)
    };
    let w = world();
    let (base, base_callee) = (calls(&w), callee(&w));
    let (tr, e) = (w.tr, w.boxes.get);
    let edit = |at_end: bool| {
        let mut w = world();
        let methods = &mut w.traits.iter_mut().find(|(k, _, _)| *k == tr).expect("Tr").2;
        let new = ObjectMethod { item: e, dispatch: true };
        if at_end { methods.push(new) } else { methods.insert(0, new) }
        w
    };
    let (end, mid) = (edit(true), edit(false));
    assert_eq!(callee(&end), base_callee, "a fn with no dynamic call keeps its key: added at the end");
    assert_eq!(callee(&mid), base_callee, "a fn with no dynamic call keeps its key: inserted in the middle");
    let mid = calls(&mid);
    for (i, (b, a)) in base.iter().zip(mid.iter()).enumerate() {
        assert_ne!(b, a, "caller {i} calls a slot the insertion moved: its key changes");
    }
    let end = calls(&end);
    assert_eq!(base[0], end[0], "`m` through `dyn Tr` keeps slot 0: same key");
    assert_eq!(base[1], end[1], "`m` through `dyn Sub` keeps slot 0: same key");
    assert_ne!(base[2], end[2], "Sub's `n` follows Tr's words: slot 1 to 2, a new key");
}

/// `fn m(&self) -> u64 { <v> }` for `u64`, as LIR text.
fn impl_text(w: &World, v: u128) -> std::string::String {
    let mut ground = PortableTys::new_in(Alloc::global());
    let &(_, _, f, _) = w.impls.first().expect("`impl Tr for u64`");
    let mut b = Bb::new();
    b.param("self");
    let e = b.int(None);
    let root = b.block(&[], Some(e));
    blaze_lir::print(build(w, &b, f, root, &[(e, v)], &mut ground).func.view())
}

/// codegen.md, "Trait objects": a table is keyed by the trait's shape and the concrete type,
/// never by a method's body. Editing `impl Tr for u64 { fn m(&self) -> u64 { 7 } }` to 9
/// changes that fn's record only: `main` (which calls `m` through the table) and the table's
/// key and bytes are equal before and after.
#[test]
fn an_impl_body_edit_rebuilds_only_that_fn() {
    let w = world();
    let (before, after) = (impl_text(&w, 7), impl_text(&w, 9));
    assert_ne!(before, after, "the edited fn's record changes");
    let (a, b) = (records(&w, w.tr, "m"), records(&w, w.tr, "m"));
    assert_eq!(a.main, b.main);
    assert_eq!((a.key, &a.bytes, &a.relocs), (b.key, &b.bytes, &b.relocs));
}

/// codegen.md, "Trait objects": a slot is the method's place in declaration order, the
/// supertraits' first. With `Tr { m }` edited to `Tr { m, e }` (a method added at the end;
/// `e`'s key a stand-in) the tables of `Tr` and of `Sub: Tr` change (key, words); a call of
/// `m` (slot 0) keeps its code, but a subtrait's own methods follow its supertraits' words, so
/// a call of Sub's `n` moves from slot 1 to 2 and is rebuilt. With
/// `Tr { e, m }` (inserted before `m`), `m` moves to slot 1: every caller of `m` through a
/// table and every fn calling a later slot (Sub's `n`, now slot 2) changes too.
#[test]
fn a_trait_method_added_rebuilds_tables_and_only_callers_of_moved_slots() {
    let mut w = world();
    let base = [records(&w, w.tr, "m"), records(&w, w.sub, "m"), records(&w, w.sub, "n")];
    let (tr, e) = (w.tr, w.boxes.get);
    let edit = |w: &mut World, at_end: bool| {
        let methods = &mut w.traits.iter_mut().find(|(k, _, _)| *k == tr).expect("Tr").2;
        let new = ObjectMethod { item: e, dispatch: true };
        if at_end { methods.push(new) } else { methods.insert(0, new) }
    };
    edit(&mut w, true);
    let end = [records(&w, w.tr, "m"), records(&w, w.sub, "m"), records(&w, w.sub, "n")];
    for (b, a) in base.iter().zip(end.iter()) {
        assert_ne!(b.key, a.key, "every table of the trait or below it is rekeyed");
        assert_ne!(b.relocs, a.relocs, "and holds the new method's word");
    }
    assert_eq!(base[0].main, end[0].main, "`m` through `dyn Tr` keeps slot 0: not rebuilt");
    assert_eq!(base[1].main, end[1].main, "`m` through `dyn Sub` keeps slot 0: not rebuilt");
    assert_ne!(base[2].main, end[2].main, "Sub's `n` follows Tr's words: slot 1 to 2, rebuilt");
    let mut w = world();
    edit(&mut w, false);
    let mid = [records(&w, w.tr, "m"), records(&w, w.sub, "m"), records(&w, w.sub, "n")];
    for (b, a) in base.iter().zip(mid.iter()) {
        assert_ne!(b.main, a.main, "a caller of a moved slot is rebuilt");
        assert_ne!(b.key, a.key);
    }
}

/// The Reference, "Trait objects": a method call on a trait object dispatches through the
/// vtable to the concrete type's implementation; no impl of the object's own trait for the
/// object type exists (E0371), so a call of such a method with `Self = dyn Tr` is never a direct
/// call: built as one it would run the trait's own (default) body, the wrong function. A
/// method the world marks with no slot (`self` by value, `where Self: Sized`, generic) is
/// therefore refused by name; here Tr's `m` (and, through `dyn Sub`, the supertrait's `m`) is
/// marked so and `r.m()` is built from `main`. A method of another trait, implemented FOR the
/// object type, stays an ordinary call: not tested here (the fixture world has no such impl).
///
/// Prints `test tests::objects::a_slotless_method_on_an_object_is_refused_not_called_directly ... ok`;
/// without the refusal in walk.rs `dyn_slot` it fails with "built as a direct call".
#[test]
fn a_slotless_method_on_an_object_is_refused_not_called_directly() {
    for obj in ["tr", "sub"] {
        let mut w = world();
        let tr = w.tr;
        let obj = if obj == "tr" { w.tr } else { w.sub };
        for m in w.traits.iter_mut().find(|(k, _, _)| *k == tr).expect("Tr").2.iter_mut() {
            m.dispatch = false;
        }
        let mut b = Bb::new();
        let x = b.bind("x");
        let lit = b.int(None);
        let xt = map_ty(&w.fx, &mut b, w.u64_);
        let s0 = b.let_(x, Some(xt), lit);
        let r = b.bind("r");
        let r_ty = dyn_ty(&mut b, obj, |b, d| b.ty(PNode::Ref(Mutbl::Not, d)));
        let xe = b.local(x);
        let borrow = b.borrow(Mutbl::Not, xe);
        let s1 = b.let_(r, Some(r_ty), borrow);
        let re = b.local(r);
        let call = b.method(re, "m", &[]);
        let root = b.block(&[s0, s1], Some(call));
        let lits = [(lit, 5u128)];
        let view = b.view(w.main, root);
        let mut ty = InferCtx::new_in(Alloc::global());
        let checked = match blaze_typeck::check(&view, &w.fx, &mut ty, &Needs::new_in(Alloc::global()), Alloc::global()) {
            Ok(Outcome::Done(c)) => c,
            other => panic!("the check of `main` did not finish: {:?}", other.err()),
        };
        assert!(checked.diags.is_empty(), "diagnostics: {:?}", checked.diags);
        let sig = w.fx.sig_of(w.main).expect("main's signature");
        let mut ground = PortableTys::new_in(Alloc::global());
        let mut subst = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
        match crate::build(&view, &checked, &sig, &mut subst, &FxAdts { w: &w, lits: &lits }, Global) {
            Err(crate::BuildError::Unsupported { what, .. }) => {
                assert!(what.contains("no vtable slot"), "refused for another reason: {what}");
            }
            Err(other) => panic!("refused for another reason: {other}"),
            Ok(_) => panic!("`r.m()` on `&dyn` with no slot for `m` was built as a direct call of the trait's `m`"),
        }
    }
}
