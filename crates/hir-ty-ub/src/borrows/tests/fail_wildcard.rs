//! Miri's `tests/fail/tree_borrows/wildcard/` programs (all run with
//! `-Zmiri-permissive-provenance`), as the retags and accesses Miri does for them. `p as usize`
//! is `Sim::expose`; `int as *mut T` is `Ptr::wildcard` on the exposed pointer (the same
//! allocation and offset). Tags are numbered in the comments in the order the model makes them
//! (`t1` is the allocation's root).
//!
//! Every `u32`/`i32` is 4 bytes; `UnsafeCell<[u32; 2]>` is 8 bytes, its element `[1]` at 4.
//!
//! Closures: a capture of `*r` by reference is the reborrow `&mut *r` / `&*r` (made where the
//! closure is), then the closure aggregate's typed copy of it, a Default retag (Miri validates
//! aggregates, `write_aggregate`). A call `c(r)` of a closure is `&mut *r` (the argument's
//! reborrow), the argument tuple's copy, then the FnEntry retag of the parameter. The closure's
//! own environment and its `&self` argument are a separate allocation; leaving them out changes
//! no verdict here.

use super::sim::{Ptr, Sim, expect_detail, expect_history, expect_ub};
use crate::{
    borrows::{AliasError, AliasErrorKind},
    ids::Span,
};

const DISABLED_READ: &str = "the accessed tag <TAG> has state Disabled which forbids this child read access";
const NO_EXPOSED: &str = "there are no exposed tags which may perform this access here";

/// The error of a child read through a tag created `initial` and disabled once, by `cause`, the
/// transition `summary` (Miri's three `help:` lines of its history).
#[track_caller]
fn expect_disabled(e: &AliasError, initial: &str, cause: &str, summary: &str) {
    expect_detail(e, DISABLED_READ);
    let created = format!("the accessed tag <TAG> was created here, in the initial state {initial}");
    let disabled = format!("the accessed tag <TAG> later transitioned to Disabled due to a {cause}");
    expect_history(e, &[created.as_str(), disabled.as_str(), summary]);
}

/// A `Reserved` tag disabled by a foreign write of the whole `u32` at 0.
#[track_caller]
fn expect_reserved_disabled_by_write(e: &AliasError) {
    expect_disabled(
        e,
        "Reserved",
        "foreign write access at offsets [0x0..0x4]",
        "this transition corresponds to a loss of read and write permissions",
    );
}

/// A wildcard access no exposed tag may do.
#[track_caller]
fn expect_no_exposed(e: &AliasError) {
    expect_detail(e, NO_EXPOSED);
    expect_history(e, &[]);
}

#[test]
fn cross_tree_from_main() {
    // Miri: tests/fail/tree_borrows/wildcard/cross_tree_from_main.rs
    let mut s = Sim::new();
    // let mut x: u32 = 42;
    let x = s.alloc(4); // t1
    // let ptr_base = &mut x as *mut u32;
    let ptr_base = s.mutable(x, 4).unwrap(); // t2
    // let ref1 = unsafe { &mut *ptr_base };
    let ref1 = s.mutable(ptr_base, 4).unwrap(); // t3
    // let ref2 = unsafe { &mut *ptr_base };
    let ref2 = s.mutable(ptr_base, 4).unwrap(); // t4
    // let int1 = ref1 as *mut u32 as usize;
    s.expose(ref1);
    // let wild = int1 as *mut u32;
    let wild = ref1.wildcard();
    // let reb3 = unsafe { &mut *wild };
    let reb3 = s.mutable(wild, 4).unwrap(); // t5, a wildcard root
    // *ref2 = 13;
    s.write(ref2, 4).unwrap();
    // let _fail = *reb3; //~ ERROR: /read access through .* is forbidden/
    let e = expect_ub(s.read(reb3, 4), AliasErrorKind::ChildAccessForbidden, "read access through");
    expect_reserved_disabled_by_write(&e);
}

#[test]
fn cross_tree_update_main() {
    // Miri: tests/fail/tree_borrows/wildcard/cross_tree_update_main.rs
    let mut s = Sim::new();
    // let mut x: u32 = 42;
    let x = s.alloc(4); // t1
    // let ptr_base = &mut x as *mut u32;
    let ptr_base = s.mutable(x, 4).unwrap(); // t2
    // let ref1 = unsafe { &mut *ptr_base };
    let ref1 = s.mutable(ptr_base, 4).unwrap(); // t3
    // let ref2 = unsafe { &mut *ptr_base };
    let ref2 = s.mutable(ptr_base, 4).unwrap(); // t4
    // let int1 = ref1 as *mut u32 as usize;
    s.expose(ref1);
    // let wild = int1 as *mut u32;
    let wild = ref1.wildcard();
    // let reb = unsafe { &mut *wild };
    let reb = s.mutable(wild, 4).unwrap(); // t5, a wildcard root
    // *reb = 13;
    s.write(reb, 4).unwrap();
    // let _fail = *ref2; //~ ERROR: /read access through .* is forbidden/
    let e = expect_ub(s.read(ref2, 4), AliasErrorKind::ChildAccessForbidden, "read access through");
    expect_reserved_disabled_by_write(&e);
}

#[test]
fn cross_tree_update_main_invalid_exposed() {
    // Miri: tests/fail/tree_borrows/wildcard/cross_tree_update_main_invalid_exposed.rs
    let mut s = Sim::new();
    // let mut x: u32 = 42;
    let x = s.alloc(4); // t1
    // let ptr_base = &mut x as *mut u32;
    let ptr_base = s.mutable(x, 4).unwrap(); // t2
    // let ref1 = unsafe { &mut *ptr_base };
    let ref1 = s.mutable(ptr_base, 4).unwrap(); // t3
    // *ref1 = 4;
    s.write(ref1, 4).unwrap();
    // let int1 = ref1 as *mut u32 as usize;
    s.expose(ref1);
    // let wild = int1 as *mut u32;
    let wild = ref1.wildcard();
    // let ref2 = unsafe { &mut *wild };
    let ref2 = s.mutable(wild, 4).unwrap(); // t4, a wildcard root
    // let ref3 = unsafe { &mut *ptr_base };
    let ref3 = s.mutable(ptr_base, 4).unwrap(); // t5; freezes t3
    // let _int3 = ref3 as *mut u32 as usize;
    s.expose(ref3);
    // *ref2 = 13; //~ ERROR: /write access through .* is forbidden/
    // The only exposed writer, t5, is newer than t4, so it can't be t4's parent.
    let e = expect_ub(s.write(ref2, 4), AliasErrorKind::NoExposedTag, "write access through <wildcard>");
    expect_no_exposed(&e);
}

#[test]
fn cross_tree_update_main_invalid_exposed2() {
    // Miri: tests/fail/tree_borrows/wildcard/cross_tree_update_main_invalid_exposed2.rs
    let mut s = Sim::new();
    let cell = [Span::new(0, 4)];
    // let mut x: u32 = 42;
    let x = s.alloc(4); // t1
    // let ptr_base = &mut x as *mut u32;
    let ptr_base = s.mutable(x, 4).unwrap(); // t2
    // let ref1 = unsafe { &mut *ptr_base };
    let ref1 = s.mutable(ptr_base, 4).unwrap(); // t3
    // let int1 = ref1 as *mut u32 as usize;
    s.expose(ref1);
    // let wild = int1 as *mut u32;
    let wild = ref1.wildcard();
    // *ref1 = 4;
    s.write(ref1, 4).unwrap();
    // let ref2 = unsafe { &mut *wild };
    let ref2 = s.mutable(wild, 4).unwrap(); // t4, a wildcard root
    // let ref3 = unsafe { &mut *(ptr_base as *mut Cell<u32>) };
    let ref3 = s.mutable_cell(ptr_base, 4, &cell).unwrap(); // t5, ReservedIM; freezes t3
    // let _int3 = ref3 as *mut Cell<u32> as usize;
    s.expose(ref3);
    // let ref4 = unsafe { &mut *(ptr_base as *mut Cell<u32>) };
    let ref4 = s.mutable_cell(ptr_base, 4, &cell).unwrap(); // t6, ReservedIM
    // let _int4 = ref4 as *mut Cell<u32> as usize;
    s.expose(ref4);
    // *ref2 = 13; //~ ERROR: /write access through .* is forbidden/
    let e = expect_ub(s.write(ref2, 4), AliasErrorKind::NoExposedTag, "write access through <wildcard>");
    expect_no_exposed(&e);
}

#[test]
fn cross_tree_update_newer() {
    // Miri: tests/fail/tree_borrows/wildcard/cross_tree_update_newer.rs
    let mut s = Sim::new();
    // let mut x: u32 = 42;
    let x = s.alloc(4); // t1
    // let ref_base = &mut x;
    let ref_base = s.mutable(x, 4).unwrap(); // t2
    // let int0 = ref_base as *mut u32 as usize;
    s.expose(ref_base);
    // let wild = int0 as *mut u32;
    let wild = ref_base.wildcard();
    // let reb1 = unsafe { &mut *wild };
    let reb1 = s.mutable(wild, 4).unwrap(); // t3, a wildcard root
    // let reb2 = unsafe { &mut *wild };
    let reb2 = s.mutable(wild, 4).unwrap(); // t4, a wildcard root
    // let ref3 = &mut *reb1;
    let ref3 = s.mutable(reb1, 4).unwrap(); // t5
    // let _int3 = ref3 as *mut u32 as usize;
    s.expose(ref3);
    // *ref3 = 13;
    s.write(ref3, 4).unwrap();
    // let _fail = *reb2; //~ ERROR: /read access through .* is forbidden/
    let e = expect_ub(s.read(reb2, 4), AliasErrorKind::ChildAccessForbidden, "read access through");
    expect_reserved_disabled_by_write(&e);
}

#[test]
fn cross_tree_update_newer_exposed() {
    // Miri: tests/fail/tree_borrows/wildcard/cross_tree_update_newer_exposed.rs
    let mut s = Sim::new();
    // let mut x: u32 = 42;
    let x = s.alloc(4); // t1
    // let ref_base = &mut x;
    let ref_base = s.mutable(x, 4).unwrap(); // t2
    // let int0 = ref_base as *mut u32 as usize;
    s.expose(ref_base);
    // let wild = int0 as *mut u32;
    let wild = ref_base.wildcard();
    // let reb1 = unsafe { &mut *wild };
    let reb1 = s.mutable(wild, 4).unwrap(); // t3, a wildcard root
    // let reb2 = unsafe { &mut *wild };
    let reb2 = s.mutable(wild, 4).unwrap(); // t4, a wildcard root
    // let _int2 = reb2 as *mut u32 as usize;
    s.expose(reb2);
    // let ref3 = &mut *reb1;
    let ref3 = s.mutable(reb1, 4).unwrap(); // t5
    // let _int3 = ref3 as *mut u32 as usize;
    s.expose(ref3);
    // *ref3 = 13;
    s.write(ref3, 4).unwrap();
    // let _fail = *reb2; //~ ERROR: /read access through .* is forbidden/
    let e = expect_ub(s.read(reb2, 4), AliasErrorKind::ChildAccessForbidden, "read access through");
    expect_reserved_disabled_by_write(&e);
}

#[test]
fn cross_tree_update_older() {
    // Miri: tests/fail/tree_borrows/wildcard/cross_tree_update_older.rs
    let mut s = Sim::new();
    // let mut x: u32 = 42;
    let x = s.alloc(4); // t1
    // let ref_base = &mut x;
    let ref_base = s.mutable(x, 4).unwrap(); // t2
    // let int0 = ref_base as *mut u32 as usize;
    s.expose(ref_base);
    // let wild = int0 as *mut u32;
    let wild = ref_base.wildcard();
    // let reb1 = unsafe { &mut *wild };
    let reb1 = s.mutable(wild, 4).unwrap(); // t3, a wildcard root
    // let reb2 = unsafe { &mut *wild };
    let reb2 = s.mutable(wild, 4).unwrap(); // t4, a wildcard root
    // let ref3 = &mut *reb2;
    let ref3 = s.mutable(reb2, 4).unwrap(); // t5
    // let _int3 = ref3 as *mut u32 as usize;
    s.expose(ref3);
    // *ref3 = 13;
    s.write(ref3, 4).unwrap();
    // let _fail = *reb1; //~ ERROR: /read access through .* is forbidden/
    let e = expect_ub(s.read(reb1, 4), AliasErrorKind::ChildAccessForbidden, "read access through");
    expect_reserved_disabled_by_write(&e);
}

/// The program both `cross_tree_update_older_invalid_exposed` tests share, up to `*ref4 = 13`;
/// returns `(reb1, ref3)`.
fn cross_tree_update_older_invalid_exposed_prefix(s: &mut Sim) -> (Ptr, Ptr) {
    // let mut x: u32 = 42;
    let x = s.alloc(4); // t1
    // let ref_base = &mut x;
    let ref_base = s.mutable(x, 4).unwrap(); // t2
    // let int0 = ref_base as *mut u32 as usize;
    s.expose(ref_base);
    // let wild = int0 as *mut u32;
    let wild = ref_base.wildcard();
    // let reb1 = unsafe { &mut *wild };
    let reb1 = s.mutable(wild, 4).unwrap(); // t3, a wildcard root
    // let reb2 = unsafe { &mut *wild };
    let reb2 = s.mutable(wild, 4).unwrap(); // t4, a wildcard root
    // let ref3 = &mut *reb1;
    let ref3 = s.mutable(reb1, 4).unwrap(); // t5
    // let _int3 = ref3 as *mut u32 as usize;
    s.expose(ref3);
    // let ref4 = &mut *reb2;
    let ref4 = s.mutable(reb2, 4).unwrap(); // t6
    // let _int4 = ref4 as *mut u32 as usize;
    s.expose(ref4);
    // *ref4 = 13;
    // t5 is newer than t4, so not an ancestor of t6: the write is foreign to t5, which then
    // leaves t3 no exposed writer below it, so it is foreign to t3 too.
    s.write(ref4, 4).unwrap();
    (reb1, ref3)
}

#[test]
fn cross_tree_update_older_invalid_exposed() {
    // Miri: tests/fail/tree_borrows/wildcard/cross_tree_update_older_invalid_exposed.rs
    let mut s = Sim::new();
    let (_reb1, ref3) = cross_tree_update_older_invalid_exposed_prefix(&mut s);
    // let _fail = *ref3; //~ ERROR: /read access through .* is forbidden/
    let e = expect_ub(s.read(ref3, 4), AliasErrorKind::ChildAccessForbidden, "read access through");
    expect_reserved_disabled_by_write(&e);
}

#[test]
fn cross_tree_update_older_invalid_exposed2() {
    // Miri: tests/fail/tree_borrows/wildcard/cross_tree_update_older_invalid_exposed2.rs
    let mut s = Sim::new();
    let (reb1, _ref3) = cross_tree_update_older_invalid_exposed_prefix(&mut s);
    // let _fail = *reb1; //~ ERROR: /read access through .* is forbidden/
    let e = expect_ub(s.read(reb1, 4), AliasErrorKind::ChildAccessForbidden, "read access through");
    expect_reserved_disabled_by_write(&e);
}

#[test]
fn dealloc() {
    // Miri: tests/fail/tree_borrows/wildcard/dealloc.rs
    let mut s = Sim::new();
    // let x = unsafe { std::alloc::alloc_zeroed(Layout::new::<u32>()) as *mut u32 };
    let x = s.alloc(4); // t1: the allocator hands out the root pointer
    // let ref1 = unsafe { &mut *x };
    let ref1 = s.mutable(x, 4).unwrap(); // t2
    // let ref2 = unsafe { &mut *x };
    let ref2 = s.mutable(x, 4).unwrap(); // t3
    // let int = ref1 as *mut u32 as usize;
    s.expose(ref1);
    // let wild = int as *mut u32;
    let wild = ref1.wildcard();
    // *ref2 = 14;
    s.write(ref2, 4).unwrap();
    // unsafe { std::alloc::dealloc(wild as *mut u8, Layout::new::<u32>()) };
    //~ ERROR: /deallocation through <wildcard> .* is forbidden/
    let e = expect_ub(s.free(wild, 4), AliasErrorKind::NoExposedTag, "deallocation through <wildcard>");
    expect_no_exposed(&e);
}

#[test]
fn gc() {
    // Miri: tests/fail/tree_borrows/wildcard/gc.rs
    let mut s = Sim::new();
    // let mut _x: u32 = 4;
    let x = s.alloc(4); // t1
    // let int = {
    //     let y = &_x;
    let y = s.shared(x, 4).unwrap(); // t2
    //     y as *const u32 as usize
    s.expose(y);
    // };
    // utils::run_provenance_gc();
    // `y`'s local is dead; `int` has no provenance; `_x`'s root is kept anyway.
    s.gc(&[]);
    assert_eq!(s.b.tag_count(x.alloc), Some(2), "the GC dropped the exposed tag");
    // _x = 5;
    s.write(x, 4).unwrap();
    // let wild = int as *const u32;
    let wild = y.wildcard();
    // let _fail = unsafe { *wild }; //~ ERROR: /read access through <wildcard> at .* is forbidden/
    let e = expect_ub(s.read(wild, 4), AliasErrorKind::NoExposedTag, "read access through <wildcard>");
    expect_no_exposed(&e);
}

/// The control of `gc`: without the expose, the GC drops `y`.
#[test]
fn gc_drops_unexposed() {
    let mut s = Sim::new();
    let x = s.alloc(4);
    let _y = s.shared(x, 4).unwrap();
    s.gc(&[]);
    assert_eq!(s.b.tag_count(x.alloc), Some(1));
}

#[test]
fn multi_exposed_child() {
    // Miri: tests/fail/tree_borrows/wildcard/multi_exposed_child.rs
    let mut s = Sim::new();
    // let mut x: u32 = 42;
    let x = s.alloc(4); // t1
    // let ref1 = &mut x;
    let ref1 = s.mutable(x, 4).unwrap(); // t2
    // let int1 = ref1 as *mut u32 as usize;
    s.expose(ref1);
    // let ref2 = &mut *ref1;
    let ref2 = s.mutable(ref1, 4).unwrap(); // t3
    // let ref3 = &mut *ref2;
    let ref3 = s.mutable(ref2, 4).unwrap(); // t4
    // let _int3 = ref3 as *mut u32 as usize;
    s.expose(ref3);
    // *ref3 = 43;
    s.write(ref3, 4).unwrap();
    // let wild = int1 as *mut u32;
    let wild = ref1.wildcard();
    // unsafe { wild.write(42) };
    s.write(wild, 4).unwrap();
    // let _x = *ref2;
    s.read(ref2, 4).unwrap();
    // unsafe { wild.write(43) };
    s.write(wild, 4).unwrap();
    // let _fail = *ref2; //~ ERROR: /read access through .* is forbidden/
    let e = expect_ub(s.read(ref2, 4), AliasErrorKind::ChildAccessForbidden, "read access through");
    // Its earlier Reserved -> Unique is not relevant to a Disabled error.
    expect_reserved_disabled_by_write(&e);
}

#[test]
fn multi_exposed_child_unique_writer() {
    // Miri: tests/fail/tree_borrows/wildcard/multi_exposed_child_unique_writer.rs
    let mut s = Sim::new();
    // let mut x: u32 = 42;
    let x = s.alloc(4); // t1
    // let ref1 = &mut x;
    let ref1 = s.mutable(x, 4).unwrap(); // t2
    // let int1 = ref1 as *mut u32 as usize;
    s.expose(ref1);
    // let ref2 = &mut *ref1;
    let ref2 = s.mutable(ref1, 4).unwrap(); // t3
    // let ref3 = &*ref2;
    let ref3 = s.shared(ref2, 4).unwrap(); // t4
    // let _int3 = ref3 as *const u32 as usize;
    s.expose(ref3);
    // let wild = int1 as *mut u32;
    let wild = ref1.wildcard();
    // unsafe { wild.write(42) };
    s.write(wild, 4).unwrap();
    // let _fail = *ref2; //~ ERROR: /read access through .* is forbidden/
    let e = expect_ub(s.read(ref2, 4), AliasErrorKind::ChildAccessForbidden, "read access through");
    expect_reserved_disabled_by_write(&e);
}

#[test]
fn multi_exposed_siblings_disable() {
    // Miri: tests/fail/tree_borrows/wildcard/multi_exposed_siblings_disable.rs
    let mut s = Sim::new();
    // let mut x: u32 = 42;
    let x = s.alloc(4); // t1
    // let ptr_base = &mut x as *mut u32;
    let ptr_base = s.mutable(x, 4).unwrap(); // t2
    // let ref1 = unsafe { &mut *ptr_base };
    let ref1 = s.mutable(ptr_base, 4).unwrap(); // t3
    // let ref2 = unsafe { &mut *ptr_base };
    let ref2 = s.mutable(ptr_base, 4).unwrap(); // t4
    // let ref3 = unsafe { &mut *ptr_base };
    let ref3 = s.mutable(ptr_base, 4).unwrap(); // t5
    // let int1 = ref1 as *mut u32 as usize;
    s.expose(ref1);
    // let _int2 = ref2 as *mut u32 as usize;
    s.expose(ref2);
    // let wild = int1 as *mut u32;
    let wild = ref1.wildcard();
    // *ref3 = 13;
    s.write(ref3, 4).unwrap();
    // let _fail = unsafe { *wild }; //~ ERROR: /read access through .* is forbidden/
    let e = expect_ub(s.read(wild, 4), AliasErrorKind::NoExposedTag, "read access through <wildcard>");
    expect_no_exposed(&e);
}

#[test]
fn multi_exposed_siblings_foreign() {
    // Miri: tests/fail/tree_borrows/wildcard/multi_exposed_siblings_foreign.rs
    let mut s = Sim::new();
    // let mut x: u32 = 42;
    let x = s.alloc(4); // t1
    // let ptr_base = &mut x as *mut u32;
    let ptr_base = s.mutable(x, 4).unwrap(); // t2
    // let ref1 = unsafe { &mut *ptr_base };
    let ref1 = s.mutable(ptr_base, 4).unwrap(); // t3
    // let ref2 = unsafe { &mut *ptr_base };
    let ref2 = s.mutable(ptr_base, 4).unwrap(); // t4
    // let ref3 = unsafe { &mut *ptr_base };
    let ref3 = s.mutable(ptr_base, 4).unwrap(); // t5
    // let int1 = ref1 as *mut u32 as usize;
    s.expose(ref1);
    // let _int2 = ref2 as *mut u32 as usize;
    s.expose(ref2);
    // let wild = int1 as *mut u32;
    let wild = ref1.wildcard();
    // unsafe { wild.write(13) };
    s.write(wild, 4).unwrap();
    // let _fail = *ref3; //~ ERROR: /read access through .* is forbidden/
    let e = expect_ub(s.read(ref3, 4), AliasErrorKind::ChildAccessForbidden, "read access through");
    expect_reserved_disabled_by_write(&e);
}

#[test]
fn multi_exposed_siblings_local() {
    // Miri: tests/fail/tree_borrows/wildcard/multi_exposed_siblings_local.rs
    let mut s = Sim::new();
    // let mut x: u32 = 42;
    let x = s.alloc(4); // t1
    // let ptr_base = &mut x as *mut u32;
    let ptr_base = s.mutable(x, 4).unwrap(); // t2
    // let ref1 = unsafe { &mut *ptr_base };
    let ref1 = s.mutable(ptr_base, 4).unwrap(); // t3
    // let ref2 = unsafe { &mut *ptr_base };
    let ref2 = s.mutable(ptr_base, 4).unwrap(); // t4
    // let int1 = ref1 as *mut u32 as usize;
    s.expose(ref1);
    // let _int2 = ref2 as *mut u32 as usize;
    s.expose(ref2);
    // let wild = int1 as *mut u32;
    let wild = ref1.wildcard();
    // unsafe { wild.write(41) };
    s.write(wild, 4).unwrap();
    // let _y = x;
    s.read(x, 4).unwrap();
    // unsafe { wild.write(0) }; //~ ERROR: /write access through <wildcard> at .* is forbidden/
    let e = expect_ub(s.write(wild, 4), AliasErrorKind::ChildAccessForbidden, "write access through <wildcard>");
    expect_detail(&e, "the accessed tag <wildcard> is a child of the conflicting tag <TAG>");
    expect_detail(&e, "the conflicting tag <TAG> has state Frozen which forbids this child write access");
    expect_history(
        &e,
        &[
            "the conflicting tag <TAG> was created here, in the initial state Reserved",
            "the conflicting tag <TAG> later transitioned to Unique due to a child write access at offsets [0x0..0x4]",
            "this transition corresponds to the first write to a 2-phase borrowed mutable reference",
            "the conflicting tag <TAG> later transitioned to Frozen due to a foreign read access at offsets [0x0..0x4]",
            "this transition corresponds to a loss of write permissions",
        ],
    );
}

#[test]
fn multi_exposed_siblings_unique_writer() {
    // Miri: tests/fail/tree_borrows/wildcard/multi_exposed_siblings_unique_writer.rs
    let mut s = Sim::new();
    // let mut x: u32 = 42;
    let x = s.alloc(4); // t1
    // let ptr_base = &mut x as *mut u32;
    let ptr_base = s.mutable(x, 4).unwrap(); // t2
    // let ref1 = unsafe { &mut *ptr_base };
    let ref1 = s.mutable(ptr_base, 4).unwrap(); // t3
    // let ref2 = unsafe { &*ptr_base };
    let ref2 = s.shared(ptr_base, 4).unwrap(); // t4
    // let int1 = ref1 as *mut u32 as usize;
    s.expose(ref1);
    // let _int2 = ref2 as *const u32 as usize;
    s.expose(ref2);
    // let wild = int1 as *mut u32;
    let wild = ref1.wildcard();
    // unsafe { wild.write(13) };
    s.write(wild, 4).unwrap();
    // let _fail = *ref2; //~ ERROR: /read access through .* is forbidden/
    let e = expect_ub(s.read(ref2, 4), AliasErrorKind::ChildAccessForbidden, "read access through");
    expect_disabled(
        &e,
        "Frozen",
        "foreign write access at offsets [0x0..0x4]",
        "this transition corresponds to a loss of read permissions",
    );
}

#[test]
fn protected_wildcard() {
    // Miri: tests/fail/tree_borrows/wildcard/protected_wildcard.rs
    let mut s = Sim::new();
    // let mut x: u32 = 32;
    let x = s.alloc(4); // t1
    // let ref1 = &mut x;
    let ref1 = s.mutable(x, 4).unwrap(); // t2
    // let ref2 = &mut *ref1;
    let ref2 = s.mutable(ref1, 4).unwrap(); // t3
    // let int2 = ref2 as *mut u32 as usize;
    s.expose(ref2);
    // let wild = int2 as *mut u32;
    let wild = ref2.wildcard();
    // let wild_ref = unsafe { &mut *wild };
    let wild_ref = s.mutable(wild, 4).unwrap(); // t4, a wildcard root
    // let mut protect = |_arg: &mut u32| {
    // The capture of `*ref1`, `&mut *ref1`, and the closure's copy of it.
    let captured = s.mutable(ref1, 4).unwrap(); // t5
    let captured = s.mutable(captured, 4).unwrap(); // t6
    // protect(wild_ref);
    let arg = s.mutable(wild_ref, 4).unwrap(); // t7
    let arg = s.mutable(arg, 4).unwrap(); // t8, in the argument tuple
    s.call();
    let _arg = s.arg_mut(arg, 4).unwrap(); // t9, protected
    //     *ref1 = 13; //~ ERROR: /write access through .* is forbidden/
    // Foreign to the whole wildcard subtree of t4, which holds the protected t9.
    let e = expect_ub(s.write(captured, 4), AliasErrorKind::ProtectedDisabled, "write access through");
    expect_detail(&e, "the accessed tag <TAG> is foreign to the protected tag <TAG> (i.e., it is not a child)");
    expect_detail(
        &e,
        "this foreign write access would cause the protected tag <TAG> (currently Reserved) to become Disabled",
    );
    expect_detail(&e, "protected tags must never be Disabled");
    expect_history(
        &e,
        &[
            "the accessed tag <TAG> was created here",
            "the protected tag <TAG> was created here, in the initial state Reserved",
        ],
    );
}

#[test]
fn protector_conflicted() {
    // Miri: tests/fail/tree_borrows/wildcard/protector_conflicted.rs
    let mut s = Sim::new();
    // let mut x: u32 = 42;
    let x = s.alloc(4); // t1
    // let ptr_base = &mut x as *mut u32;
    let ptr_base = s.mutable(x, 4).unwrap(); // t2
    // let ref1 = unsafe { &mut *ptr_base };
    let ref1 = s.mutable(ptr_base, 4).unwrap(); // t3
    // let ref2 = unsafe { &mut *ptr_base };
    let ref2 = s.mutable(ptr_base, 4).unwrap(); // t4
    // let protect = |arg: &mut u32| {
    // The capture of `*ref2`, `&*ref2`, and the closure's copy of it.
    let captured = s.shared(ref2, 4).unwrap(); // t5
    let captured = s.shared(captured, 4).unwrap(); // t6
    // protect(ref1);
    let arg = s.mutable(ref1, 4).unwrap(); // t7
    let arg = s.mutable(arg, 4).unwrap(); // t8, in the argument tuple
    s.call();
    let arg = s.arg_mut(arg, 4).unwrap(); // t9, protected
    //     let int = arg as *mut u32 as usize;
    s.expose(arg);
    //     let wild = int as *mut u32;
    let wild = arg.wildcard();
    //     let _x = *ref2;
    // A foreign read: t9 becomes Reserved (conflicted), which a protector forbids writes to.
    s.read(captured, 4).unwrap();
    //     unsafe { *wild = 4 }; //~ ERROR: /write access through <wildcard> at .* is forbidden/
    let e = expect_ub(s.write(wild, 4), AliasErrorKind::NoExposedTag, "write access through <wildcard>");
    expect_no_exposed(&e);
}

/// `UnsafeCell::get(&self)` called with the reference `cell` to `len` cell bytes: the call's
/// FnEntry retag, and the raw pointer it returns, with that tag.
fn unsafe_cell_get(s: &mut Sim, cell: Ptr, len: u64) -> Ptr {
    s.call();
    let this = s.arg_shared_cell(cell, len, &[Span::new(0, len)]).unwrap();
    s.ret().unwrap();
    this
}

#[test]
fn protector_release() {
    // Miri: tests/fail/tree_borrows/wildcard/protector_release.rs
    let mut s = Sim::new();
    let all = [Span::new(0, 8)];
    let one = [Span::new(0, 4)];
    // let mut x: UnsafeCell<[u32; 2]> = UnsafeCell::new([32, 33]);
    let x = s.alloc(8); // t1
    // let ref1 = &mut x;
    let ref1 = s.mutable_cell(x, 8, &all).unwrap(); // t2
    // let cell_ptr = ref1.get() as *mut u32;
    let recv = s.shared_cell(ref1, 8, &all).unwrap(); // t3, the receiver's `&*ref1`
    let cell_ptr = unsafe_cell_get(&mut s, recv, 8); // t4
    // let int = ref1 as *mut UnsafeCell<[u32; 2]> as usize;
    s.expose(ref1);
    // let wild = int as *mut UnsafeCell<u32>;
    let wild = ref1.wildcard();
    // let ref2 = unsafe { &mut *cell_ptr };
    let ref2 = s.mutable(cell_ptr, 4).unwrap(); // t5
    // let ref3 = unsafe { &mut *wild.wrapping_add(1) };
    let ref3 = s.mutable_cell(wild.add(4), 4, &one).unwrap(); // t6, a wildcard root
    // let protect = |arg4: &mut u32| {
    // The capture of `*ref3`, `&*ref3`, and the closure's copy of it.
    let captured = s.shared_cell(ref3, 4, &one).unwrap(); // t7
    let captured = s.shared_cell(captured, 4, &one).unwrap(); // t8
    // let ptr = protect(ref2);
    let arg = s.mutable(ref2, 4).unwrap(); // t9
    let arg = s.mutable(arg, 4).unwrap(); // t10, in the argument tuple
    s.call();
    let arg4 = s.arg_mut(arg, 4).unwrap(); // t11, protected
    //     *arg4 = 41;
    s.write(arg4, 4).unwrap();
    //     let ref5 = &mut *arg4;
    let ref5 = s.mutable(arg4, 4).unwrap(); // t12
    //     let _int = ref5 as *mut u32 as usize;
    s.expose(ref5);
    //     let ref6 = unsafe { &mut *ref3.get() };
    let recv = s.shared_cell(captured, 4, &one).unwrap(); // t13
    let raw = unsafe_cell_get(&mut s, recv, 4); // t14
    let ref6 = s.mutable(raw, 4).unwrap(); // t15
    //     return (ref6 as *mut u32).wrapping_sub(1);
    let ptr = ref6.sub(4);
    // };
    // t11's protector ends with a write at [0]; t6 is older than t11, so t15 can't be its child.
    s.ret().unwrap();
    // let _fail = unsafe { *ptr }; //~ ERROR: /read access through .* is forbidden/
    let e = expect_ub(s.read(ptr, 4), AliasErrorKind::ChildAccessForbidden, "read access through");
    expect_disabled(
        &e,
        "Reserved",
        "protector release (acting as a foreign write access) on every location previously accessed by this tag",
        "this transition corresponds to a loss of read and write permissions",
    );
}

#[test]
fn protector_release2() {
    // Miri: tests/fail/tree_borrows/wildcard/protector_release2.rs
    let mut s = Sim::new();
    let all = [Span::new(0, 8)];
    let one = [Span::new(0, 4)];
    // let mut x: UnsafeCell<[u32; 2]> = UnsafeCell::new([32, 33]);
    let x = s.alloc(8); // t1
    // let ref1 = &mut x;
    let ref1 = s.mutable_cell(x, 8, &all).unwrap(); // t2
    // let cell_ptr = ref1.get() as *mut u32;
    let recv = s.shared_cell(ref1, 8, &all).unwrap(); // t3, the receiver's `&*ref1`
    let cell_ptr = unsafe_cell_get(&mut s, recv, 8); // t4
    // let int = ref1 as *mut UnsafeCell<[u32; 2]> as usize;
    s.expose(ref1);
    // let wild = int as *mut UnsafeCell<u32>;
    let wild = ref1.wildcard();
    // let ref2 = unsafe { &mut *cell_ptr };
    let ref2 = s.mutable(cell_ptr, 4).unwrap(); // t5
    // let protect = |arg3: &mut u32| {
    // It captures only the raw `wild` (by reference, another allocation).
    // let ptr = protect(ref2);
    let arg = s.mutable(ref2, 4).unwrap(); // t6
    let arg = s.mutable(arg, 4).unwrap(); // t7, in the argument tuple
    s.call();
    let arg3 = s.arg_mut(arg, 4).unwrap(); // t8, protected
    //     let ref4 = unsafe { &mut *wild.wrapping_add(1) };
    let ref4 = s.mutable_cell(wild.add(4), 4, &one).unwrap(); // t9, a wildcard root
    //     *arg3 = 41;
    s.write(arg3, 4).unwrap();
    //     let ref5 = &mut *arg3;
    let ref5 = s.mutable(arg3, 4).unwrap(); // t10
    //     let _int = ref5 as *mut u32 as usize;
    s.expose(ref5);
    //     let ref6 = unsafe { &mut *ref4.get() };
    let recv = s.shared_cell(ref4, 4, &one).unwrap(); // t11
    let raw = unsafe_cell_get(&mut s, recv, 4); // t12
    let ref6 = s.mutable(raw, 4).unwrap(); // t13
    //     return (ref6 as *mut u32).wrapping_sub(1);
    let ptr = ref6.sub(4);
    // };
    // t8's protector ends with a write at [0]. t9 is newer than t8 but older than t8's smallest
    // exposed child t10, so t13 can't be t8's child.
    s.ret().unwrap();
    // let _fail = unsafe { *ptr }; //~ ERROR: /read access through .* is forbidden/
    let e = expect_ub(s.read(ptr, 4), AliasErrorKind::ChildAccessForbidden, "read access through");
    expect_disabled(
        &e,
        "Reserved",
        "protector release (acting as a foreign write access) on every location previously accessed by this tag",
        "this transition corresponds to a loss of read and write permissions",
    );
}

#[test]
fn single_exposed_disable() {
    // Miri: tests/fail/tree_borrows/wildcard/single_exposed_disable.rs
    let mut s = Sim::new();
    // let mut x: u32 = 42;
    let x = s.alloc(4); // t1
    // let ptr_base = &mut x as *mut u32;
    let ptr_base = s.mutable(x, 4).unwrap(); // t2
    // let ref1 = unsafe { &mut *ptr_base };
    let ref1 = s.mutable(ptr_base, 4).unwrap(); // t3
    // let ref2 = unsafe { &mut *ptr_base };
    let ref2 = s.mutable(ptr_base, 4).unwrap(); // t4
    // let int1 = ref1 as *mut u32 as usize;
    s.expose(ref1);
    // let wild = int1 as *mut u32;
    let wild = ref1.wildcard();
    // *ref2 = 13;
    s.write(ref2, 4).unwrap();
    // let _fail = unsafe { *wild }; //~ ERROR: /read access through .* is forbidden/
    let e = expect_ub(s.read(wild, 4), AliasErrorKind::NoExposedTag, "read access through <wildcard>");
    expect_no_exposed(&e);
}

#[test]
fn single_exposed_foreign() {
    // Miri: tests/fail/tree_borrows/wildcard/single_exposed_foreign.rs
    let mut s = Sim::new();
    // let mut x: u32 = 42;
    let x = s.alloc(4); // t1
    // let ptr_base = &mut x as *mut u32;
    let ptr_base = s.mutable(x, 4).unwrap(); // t2
    // let ref1 = unsafe { &mut *ptr_base };
    let ref1 = s.mutable(ptr_base, 4).unwrap(); // t3
    // let ref2 = unsafe { &mut *ptr_base };
    let ref2 = s.mutable(ptr_base, 4).unwrap(); // t4
    // let int1 = ref1 as *mut u32 as usize;
    s.expose(ref1);
    // let wild = int1 as *mut u32;
    let wild = ref1.wildcard();
    // unsafe { wild.write(13) };
    s.write(wild, 4).unwrap();
    // let _fail = *ref2; //~ ERROR: /read access through .* is forbidden/
    let e = expect_ub(s.read(ref2, 4), AliasErrorKind::ChildAccessForbidden, "read access through");
    expect_reserved_disabled_by_write(&e);
}

#[test]
fn single_exposed_local() {
    // Miri: tests/fail/tree_borrows/wildcard/single_exposed_local.rs
    let mut s = Sim::new();
    // let mut x: u32 = 0;
    let x = s.alloc(4); // t1
    // let ref1 = &mut x;
    let ref1 = s.mutable(x, 4).unwrap(); // t2
    // let int = ref1 as *mut u32 as usize;
    s.expose(ref1);
    // let wild = int as *mut u32;
    let wild = ref1.wildcard();
    // unsafe { wild.write(41) };
    s.write(wild, 4).unwrap();
    // let _y = x;
    s.read(x, 4).unwrap();
    // unsafe { wild.write(0) }; //~ ERROR: /write access through <wildcard> at .* is forbidden/
    let e = expect_ub(s.write(wild, 4), AliasErrorKind::NoExposedTag, "write access through <wildcard>");
    expect_no_exposed(&e);
}

#[test]
fn single_exposed_only_ro() {
    // Miri: tests/fail/tree_borrows/wildcard/single_exposed_only_ro.rs
    let mut s = Sim::new();
    // let mut x = 0;
    let x = s.alloc(4); // t1, an i32
    // let _fool = &mut x as *mut i32;
    let _fool = s.mutable(x, 4).unwrap(); // t2
    // let addr = (&x as *const i32).expose_provenance();
    let shared = s.shared(x, 4).unwrap(); // t3
    s.expose(shared);
    // let ptr = std::ptr::with_exposed_provenance_mut::<i32>(addr);
    let ptr = shared.wildcard();
    // unsafe { *ptr = 0 }; //~ ERROR: /write access through <wildcard> at .* is forbidden/
    let e = expect_ub(s.write(ptr, 4), AliasErrorKind::NoExposedTag, "write access through <wildcard>");
    expect_no_exposed(&e);
}

#[test]
fn strongly_protected_wildcard() {
    // Miri: tests/fail/tree_borrows/wildcard/strongly_protected_wildcard.rs
    // std's own copies of a `Box` or reference inside `Box::new`, `Box::leak` and
    // `Box::from_raw` (through `ManuallyDrop`, `Unique`, return places) only add children in a
    // chain under the tags below, each made by a read; they change nothing here.
    let mut s = Sim::new();
    // inner(Box::leak(Box::new(0)), |raw| { ... });
    let heap = s.alloc(4); // t1, the heap block's root
    let b = s.boxed(heap, 4).unwrap(); // t2, the `Box` `Box::new` returns
    s.call();
    let leak_arg = s.arg_box(b, 4).unwrap(); // t3, `Box::leak`'s argument
    let leaked = s.mutable(leak_arg, 4).unwrap(); // t4, `&mut *Box::into_raw(b)`
    s.ret().unwrap();
    let leaked = s.mutable(leaked, 4).unwrap(); // t5, the return value
    let arg = s.mutable(leaked, 4).unwrap(); // t6, the argument's reborrow
    // fn inner(x: &mut i32, f: fn(usize)) {
    s.call();
    let x = s.arg_mut(arg, 4).unwrap(); // t7, strongly protected
    //     f(x as *mut i32 as usize)
    s.expose(x);
    // |raw| { drop(unsafe { Box::from_raw(raw as *mut i32) }); }
    let wild = x.wildcard();
    let b = s.boxed(wild, 4).unwrap(); // t8, a wildcard root: `from_raw`'s `Box`
    let b = s.boxed(b, 4).unwrap(); // t9, the return value
    s.call();
    let dropped = s.arg_box(b, 4).unwrap(); // t10, `drop`'s argument, weakly protected
    // `drop`'s `Box` drop glue frees through it. //~ ERROR: /deallocation through .* is forbidden/
    let e = expect_ub(s.free(dropped, 4), AliasErrorKind::ProtectedDealloc, "deallocation through");
    expect_detail(&e, "the allocation of the accessed tag <TAG> also contains the strongly protected tag <TAG>");
    expect_detail(&e, "the strongly protected tag <TAG> disallows deallocations");
    expect_history(
        &e,
        &[
            "the accessed tag <TAG> was created here",
            "the strongly protected tag <TAG> was created here, in the initial state Reserved",
        ],
    );
}

#[test]
fn subtree_internal_relatedness() {
    // Miri: tests/fail/tree_borrows/wildcard/subtree_internal_relatedness.rs
    let mut s = Sim::new();
    // let mut x: u32 = 42;
    let x = s.alloc(4); // t1
    // let ref_base = &mut x;
    let ref_base = s.mutable(x, 4).unwrap(); // t2
    // let int1 = ref_base as *mut u32 as usize;
    s.expose(ref_base);
    // let wild = int1 as *mut u32;
    let wild = ref_base.wildcard();
    // let reb = unsafe { &mut *wild };
    let reb = s.mutable(wild, 4).unwrap(); // t3, a wildcard root
    // let ptr_reb = reb as *mut u32;
    let ptr_reb = reb;
    // let ref1 = unsafe { &mut *ptr_reb };
    let ref1 = s.mutable(ptr_reb, 4).unwrap(); // t4
    // let ref2 = unsafe { &mut *ptr_reb };
    let ref2 = s.mutable(ptr_reb, 4).unwrap(); // t5
    // *ref1 = 13;
    s.write(ref1, 4).unwrap();
    // let _fail = *ref2; //~ ERROR: /read access through .* is forbidden/
    let e = expect_ub(s.read(ref2, 4), AliasErrorKind::ChildAccessForbidden, "read access through");
    expect_reserved_disabled_by_write(&e);
}

#[test]
fn subtree_internal_relatedness_wildcard() {
    // Miri: tests/fail/tree_borrows/wildcard/subtree_internal_relatedness_wildcard.rs
    let mut s = Sim::new();
    // let mut x: u32 = 42;
    let x = s.alloc(4); // t1
    // let ref_base = &mut x;
    let ref_base = s.mutable(x, 4).unwrap(); // t2
    // let int = ref_base as *mut u32 as usize;
    s.expose(ref_base);
    // let wild = int as *mut u32;
    let wild = ref_base.wildcard();
    // let reb = unsafe { &mut *wild };
    let reb = s.mutable(wild, 4).unwrap(); // t3, a wildcard root
    // let ptr_reb = reb as *mut u32;
    let ptr_reb = reb;
    // let ref1 = unsafe { &mut *ptr_reb };
    let ref1 = s.mutable(ptr_reb, 4).unwrap(); // t4
    // let _int1 = ref1 as *mut u32 as usize;
    s.expose(ref1);
    // let ref2 = unsafe { &mut *ptr_reb };
    let ref2 = s.mutable(ptr_reb, 4).unwrap(); // t5
    // unsafe { *wild = 13 };
    s.write(wild, 4).unwrap();
    // let _succ = *ref1;
    s.read(ref1, 4).unwrap();
    // let _fail = *ref2; //~ ERROR: /read access through .* is forbidden/
    let e = expect_ub(s.read(ref2, 4), AliasErrorKind::ChildAccessForbidden, "read access through");
    expect_reserved_disabled_by_write(&e);
}
