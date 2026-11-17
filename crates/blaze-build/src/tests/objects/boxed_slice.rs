//! The drop glue of `Box<[T]>` and `Box<str>` (walk.rs `drop_box_contents` over
//! walk/slice_glue.rs), built from the objects fixture world plus two makers, emitted and run
//! natively.
//!
//! The rule. The Reference, "Destructors" (`destructors.operation`): "The destructor of a type
//! T consists of: If T: Drop, calling <T as std::ops::Drop>::drop; Recursively running the
//! destructor of all of its fields", and "The elements of an array or owned slice are dropped
//! from the first element to the last." The pinned library (library/alloc/src/boxed.rs, `impl
//! Drop for Box`): "the T in the Box is dropped by the compiler before the destructor is run",
//! and the destructor only deallocates, by `Layout::for_value_raw(ptr)`: for `[T]` of length
//! `len` that is `len * size_of::<T>()` bytes at `T`'s alignment, read from the same wide
//! pointer. So `Box<[A]>`'s glue is A's glue on each element, first to last, over the (data,
//! len) the box holds, then the box's `Drop` (the free); `Box<str>`'s is the free alone (`str`
//! is bytes).
//!
//! The world adds, beside objects.rs's `Box`, `A` (a host `Drop` that logs its `x`) and the
//! malloc-like host `box_drop` (logs 100 + the freed size):
//! ```text
//! fn mk_as() -> Box<[A]>;   // host: three A { x: 4 }, { x: 5 }, { x: 6 } in 24 bytes, len 3
//! fn mk_s() -> Box<str>;    // host: "abc" in an 8-byte allocation, len 3
//! ```
//! `main` is `{ let x = mk_as(); let y = mk_s(); 5 }`: at its end `y` drops (the free of 8
//! bytes: 108), then `x` (4, 5, 6, then the free of 24 bytes: 124).
//! Without the fix the glue of `Box<[A]>` is refused: "the drop of a boxed slice's or `str`'s
//! elements".

use std::alloc::Global;
use std::vec::Vec as StdVec;

use blaze_own::Alloc;
use blaze_typeck::fixture::Bb;
use blaze_typeck::{InferCtx, ItemKey, List, Needs, Outcome, PNode, PTy, PortableTys};

use super::super::run_native_vt;
use super::{FxAdts, LIVE, LOG, World, build, ground_of, logged, world};
use crate::{Built, Subst};

/// `fn mk_as() -> Box<[A]>`: the box is two words (data, length), an aggregate, returned
/// through the result pointer, the first argument (lib.rs, "Rust ABI").
extern "C" fn host_mk_as(out: *mut [u64; 2]) {
    let layout = std::alloc::Layout::from_size_align(24, 8).expect("a layout");
    // SAFETY: the layout is of nonzero size.
    let p = unsafe { std::alloc::alloc(layout) }.cast::<u64>();
    assert!(!p.is_null(), "the host allocation failed");
    // SAFETY: `p` is a fresh allocation of three u64s, each an `A { x }`; `out` is the caller's
    // result slot of two words.
    unsafe {
        p.write(4);
        p.add(1).write(5);
        p.add(2).write(6);
        out.write([p as u64, 3]);
    }
    LIVE.with(|l| l.borrow_mut().push((p as u64, 24)));
}

/// `fn mk_s() -> Box<str>`: "abc" in one 8-byte host allocation, length 3.
extern "C" fn host_mk_s(out: *mut [u64; 2]) {
    let data = super::host_alloc(u64::from_le_bytes(*b"abc\0\0\0\0\0"));
    // SAFETY: `out` is the caller's result slot of two words.
    unsafe { out.write([data as u64, 3]) };
}

/// objects.rs's world with `mk_as` and `mk_s` (the module doc), and A's ground key.
fn world_slices() -> (World, PTy) {
    let mut w = world();
    let box_k = w.boxes.box_k;
    let a = w.boxes.concrete[0].1;
    let fx = &mut w.fx;
    let slice_a = fx.ty(PNode::Slice(a));
    let box_slice = fx.adt(box_k, &[slice_a]);
    let mk_as = fx.func("mk_as", 0, &[], box_slice, StdVec::new());
    let str_ = fx.ty(PNode::Str);
    let box_str = fx.adt(box_k, &[str_]);
    let mk_s = fx.func("mk_s", 0, &[], box_str, StdVec::new());
    fx.finish();
    w.boxes.host.push((mk_as, host_mk_as as usize as u64));
    w.boxes.host.push((mk_s, host_mk_s as usize as u64));
    (w, a)
}

/// The glue of each of `tys` (in order: callees first), as `drop_in_place::<t>`'s instance.
fn glues(w: &World, ground: &mut PortableTys<'static>, tys: &[PTy]) -> StdVec<(List, Built<Global>)> {
    let bx = &w.boxes;
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
    let mut out = StdVec::new();
    for &t in tys {
        let args = ground.intern_list(&[t]).expect("the ground table is full");
        let mut subst = Subst { args: &[t], ground: &mut *ground, scratch: Alloc::global() };
        let built = crate::build_drop_glue(&view, &checked, &sig, &mut subst, &FxAdts { w, lits: &[] }, Global)
            .unwrap_or_else(|e| panic!("the glue at {t:?} failed: {e}"));
        super::super::emits(&built, &[]);
        out.push((args, built));
    }
    out
}

/// `{ let x = mk_as(); let y = mk_s(); 5 }` at its end: `y`'s free (108), then `x`'s elements
/// first to last (4, 5, 6) and its free of 24 bytes (124).
/// Prints `test tests::objects::boxed_slice::a_boxed_slice_drops_its_elements_then_frees ... ok`.
/// Fails without the fix: "the glue at .. failed: the signature: the drop of a boxed slice's
/// or `str`'s elements"; a loop that skips the length gives [108, 124], one that runs last to
/// first [108, 6, 5, 4, 124].
#[test]
fn a_boxed_slice_drops_its_elements_then_frees() {
    let (w, a) = world_slices();
    let bx = &w.boxes;
    let mut ground = PortableTys::new_in(Alloc::global());
    let full = "the ground table is full";
    let ga = ground_of(&w.fx, &mut ground, a);
    let slice_a = ground.intern(PNode::Slice(ga)).expect(full);
    let str_ = ground.intern(PNode::Str).expect(full);
    let (ls, lt) = (ground.intern_list(&[slice_a]).expect(full), ground.intern_list(&[str_]).expect(full));
    let box_slice = ground.intern(PNode::Adt(bx.box_k, ls)).expect(full);
    let box_str = ground.intern(PNode::Adt(bx.box_k, lt)).expect(full);
    let built = glues(&w, &mut ground, &[ga, box_slice, box_str]);
    let mut b = Bb::new();
    let x = b.bind("x");
    let f = b.path(&["mk_as"]);
    let call = b.call(f, &[]);
    let s0 = b.let_(x, None, call);
    let y = b.bind("y");
    let f = b.path(&["mk_s"]);
    let call = b.call(f, &[]);
    let s1 = b.let_(y, None, call);
    let five = b.int(None);
    let root = b.block(&[s0, s1], Some(five));
    let main = build(&w, &b, w.main, root, &[(five, 5)], &mut ground);
    let mut fns: StdVec<(ItemKey, Option<List>, &Built<Global>)> = built.iter().map(|(l, g)| (bx.dip, Some(*l), g)).collect();
    fns.push((w.main, None, &main));
    LOG.with(|l| l.borrow_mut().clear());
    assert_eq!(run_native_vt(&bx.host, &fns, None), 5);
    assert_eq!(logged(), [108, 4, 5, 6, 124]);
}
