//! Miri's `tests/fail/tree_borrows/*.rs`, `reserved/*.rs` and `implicit_writes/*.rs` (the
//! `wildcard/` ones are `fail_wildcard.rs`), each as the retags, accesses, calls and returns
//! Miri does when it runs the program, with Miri's verdict: the step its `//~ ERROR` names is
//! the first `Err`, with the title, helps and history of its `.stderr`.
//!
//! Miri at `ad3d0bc141a0` retags inside typed copies: a `&`/`&mut` rvalue, every copy or move
//! of a reference or `Box` (a `Use`, a cast's result, a field of an aggregate, a return value
//! copied into the caller after the callee's protectors end) is a `Default` retag, and a
//! call's arguments are `FnEntry` retags. A reference cast to a raw pointer makes no tag.
//! Layouts are rustc's: `u8` 1, `u32`/`i32` 4, `u64`/`usize` 8, tuples and structs of
//! equally aligned fields in field order.

use super::sim::{Ptr, R, Sim, expect_detail, expect_history, expect_ub, normalize};
use crate::{
    borrows::{
        AliasError,
        AliasErrorKind::{ChildAccessForbidden, ProtectedDealloc, ProtectedDisabled},
        NewPermission, PtrKind, RetagMode,
    },
    ids::{AccessKind, SiteId, Span},
};

/// Asserts the title of `e`, normalized as Miri's `.stderr` (`<TAG>`, `ALLOC`).
#[track_caller]
fn expect_title(e: &AliasError, title: &str) {
    assert_eq!(normalize(e.title()), title, "{e}");
}

/// Asserts where each line of the history of `e` points: the `-->` of Miri's `.stderr`, as
/// the sites of the steps that made the tag or the transition (`None` for a "this transition
/// corresponds to" note).
#[track_caller]
fn expect_sites(e: &AliasError, sites: &[Option<SiteId>]) {
    let got: Vec<Option<SiteId>> = e.history().iter().map(|ev| ev.site).collect();
    assert_eq!(got, sites, "history of:\n{e}");
}

/// A printed tree as Miri's test normalization writes it: tags `<n=name>` as `<TAG=name>`, and
/// rules of more than 50 `─` cut to 50.
fn normalize_tree(tree: &str) -> String {
    let mut out = String::with_capacity(tree.len());
    let mut run = 0;
    let mut rest = tree;
    while let Some(c) = rest.chars().next() {
        rest = &rest[c.len_utf8()..];
        if c == '─' {
            run += 1;
            if run <= 50 {
                out.push(c);
            }
            continue;
        }
        run = 0;
        if c == '<' {
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            if digits > 0 && rest.as_bytes().get(digits) == Some(&b'=') {
                out.push_str("<TAG=");
                rest = &rest[digits + 1..];
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// The callee's argument of `kind` to `len` bytes at `p` (`FnEntry`), under
/// `-Zmiri-tree-borrows-implicit-writes` when `implicit_writes`.
fn arg(
    s: &mut Sim,
    p: Ptr,
    kind: PtrKind,
    freeze: bool,
    cells: &[Span],
    len: u64,
    implicit_writes: bool,
) -> R<Ptr> {
    let new = NewPermission::new(kind, RetagMode::FnEntry, freeze, true, cells).with_implicit_writes(implicit_writes);
    s.retag_with(p, new, len)
}

/// The callee's argument `&mut T` (`Freeze`) under `-Zmiri-tree-borrows-implicit-writes`.
fn iw_arg_mut(s: &mut Sim, p: Ptr, len: u64) -> R<Ptr> {
    arg(s, p, PtrKind::Mut, true, &[], len, true)
}

/// `Box::new(v)` of a `len`-byte `v`: `box_new_uninit` hands out the heap block's root
/// pointer, `write_via_move` writes `v` through it, `transmute(ptr)` writes the `Box` into the
/// return place, and the return value is copied into the caller; each typed write of the
/// `Box` retags it.
fn box_new(s: &mut Sim, len: u64) -> R<Ptr> {
    s.call();
    let heap = s.alloc(len);
    s.write(heap, len)?;
    let b = s.boxed(heap, len)?;
    s.ret()?;
    s.boxed(b, len)
}

/// `Box::leak(b)`: `Box::into_raw_with_allocator(b)` moves the `Box` into
/// `ManuallyDrop::new(b)` and takes `&raw mut **b`, then `leak` returns `&mut *ptr`.
fn box_leak(s: &mut Sim, b: Ptr, len: u64) -> R<Ptr> {
    s.call();
    let b = s.arg_box(b, len)?;
    //   let (ptr, alloc) = Box::into_raw_with_allocator(b);
    s.call();
    let b = s.arg_box(b, len)?;
    //     let mut b = mem::ManuallyDrop::new(b);
    s.call();
    let b = s.arg_box(b, len)?;
    let b = s.boxed(b, len)?;
    s.ret()?;
    let b = s.boxed(b, len)?;
    //     let ptr = &raw mut **b;
    s.ret()?;
    //   unsafe { &mut *ptr }
    let r = s.mutable(b, len)?;
    s.ret()?;
    s.mutable(r, len)
}

/// `&mut (*p)[start..end]` for `p: &mut [u8; n]`, as the overloaded index runs:
/// `<[u8; n] as IndexMut<Range<usize>>>::index_mut(&mut *p, start..end)`, which calls
/// `<[u8] as IndexMut<_>>::index_mut(self as &mut [u8], index)`, which calls
/// `<Range<usize> as SliceIndex<[u8]>>::index_mut(index, self)`, which returns
/// `&mut *self.get_unchecked_mut(slice)`; each return value is copied into its caller.
fn index_mut_range(s: &mut Sim, p: Ptr, n: u64, start: u64, end: u64) -> R<Ptr> {
    let len = end - start;
    let r = s.mutable(p, n)?;
    s.call();
    let this = s.arg_mut(r, n)?;
    //   IndexMut::index_mut(self as &mut [T], index): the unsizing cast's result
    let r = s.mutable(this, n)?;
    s.call();
    let this = s.arg_mut(r, n)?;
    //     index.index_mut(self)
    let r = s.mutable(this, n)?;
    s.call();
    let slice = s.arg_mut(r, n)?;
    //       unsafe { &mut *self.get_unchecked_mut(slice) }
    let out = s.mutable(slice.add(start), len)?;
    s.ret()?;
    let out = s.mutable(out, len)?;
    s.ret()?;
    let out = s.mutable(out, len)?;
    s.ret()?;
    s.mutable(out, len)
}

/// Miri: tests/fail/tree_borrows/alternate-read-write.rs
#[test]
fn alternate_read_write() {
    let mut s = Sim::new();
    // let x = &mut 0u8;
    let tmp = s.alloc(1);
    let x = s.mutable(tmp, 1).unwrap();
    // let y = unsafe { &mut *(x as *mut u8) };
    let y = s.mutable(x, 1).unwrap();
    let created = s.last_site();
    // let _val = *x;
    s.read(x, 1).unwrap();
    // *y += 1; // Success
    s.read(y, 1).unwrap();
    s.write(y, 1).unwrap();
    let unique = s.last_site();
    // let _val = *x;
    s.read(x, 1).unwrap();
    let frozen = s.last_site();
    // *y += 1; // Failure
    s.read(y, 1).unwrap();
    let e = expect_ub(s.write(y, 1), ChildAccessForbidden, "write access through");
    expect_title(&e, "write access through <TAG> at ALLOC[0x0] is forbidden");
    expect_detail(&e, "the accessed tag <TAG> has state Frozen which forbids this child write access");
    expect_history(
        &e,
        &[
            "the accessed tag <TAG> was created here, in the initial state Reserved",
            "the accessed tag <TAG> later transitioned to Unique due to a child write access at offsets [0x0..0x1]",
            "this transition corresponds to the first write to a 2-phase borrowed mutable reference",
            "the accessed tag <TAG> later transitioned to Frozen due to a foreign read access at offsets [0x0..0x1]",
            "this transition corresponds to a loss of write permissions",
        ],
    );
    expect_sites(&e, &[Some(created), Some(unique), None, Some(frozen), None]);
}

/// `(*a).field2.set(v)` on the `Cell<u32>` at `cell`: `Cell::set(&self)` calls
/// `self.replace(val)`, which calls `mem::replace(unsafe { &mut *self.value.get() }, val)`,
/// which does `ptr::read(dest)` and `ptr::write(dest, src)`.
fn cell_set_u32(s: &mut Sim, cell: Ptr, implicit_writes: bool) -> R {
    let cells = [Span::new(0, 4)];
    // &(*a).field2, then Cell::set(&self, val)
    let r = s.shared_cell(cell, 4, &cells)?;
    s.call();
    let this = s.arg_shared_cell(r, 4, &cells)?;
    //   self.replace(val)
    let r = s.shared_cell(this, 4, &cells)?;
    s.call();
    let this = s.arg_shared_cell(r, 4, &cells)?;
    //     self.value.get(): UnsafeCell::get(&self) -> *mut T
    let r = s.shared_cell(this, 4, &cells)?;
    s.call();
    let raw = s.arg_shared_cell(r, 4, &cells)?;
    s.ret()?;
    //     mem::replace(unsafe { &mut *raw }, val)
    let m = s.mutable(raw, 4)?;
    s.call();
    let dest = arg(s, m, PtrKind::Mut, true, &[], 4, implicit_writes)?;
    //       let result = ptr::read(dest); ptr::write(dest, src);
    s.read(dest, 4)?;
    s.write(dest, 4)?;
    s.ret()?;
    s.ret()?;
    s.ret()
}

const CELL_INSIDE_STRUCT_TREE: &str = "\
──────────────────────────────────────────────────
Warning: this tree is indicative only. Some tags may have been hidden.
0..   4..   8
| Unq | Unq |    └─┬──<TAG=root of the allocation>
| Frz |?Cel |      └────<TAG=a>
──────────────────────────────────────────────────
";

/// Miri: tests/fail/tree_borrows/cell-inside-struct.rs, both revisions (`tree`,
/// `tree_implicit_writes`): the implicit writes only change the protected `&mut u32` of
/// `mem::replace`, inside the cell.
fn cell_inside_struct(implicit_writes: bool) {
    let mut s = Sim::new();
    // let root = Foo { field1: 42, field2: Cell::new(88) };   field1 at 0, field2 at 4
    let root = s.alloc(8);
    // let a = &root;
    let a = s.shared_cell(root, 8, &[Span::new(4, 4)]).unwrap();
    let created = s.last_site();
    // name!(a as *const Foo, "a");
    s.name(a, "a");
    // let a: *const Foo = a as *const Foo;
    // let a: *mut Foo = a as *mut Foo;
    // print_state!(alloc_id);
    assert_eq!(normalize_tree(&s.print(a, false)), CELL_INSIDE_STRUCT_TREE);
    // (*a).field2.set(10);
    cell_set_u32(&mut s, a.add(4), implicit_writes).unwrap();
    // (*a).field1 = 88;
    let e = expect_ub(s.write(a, 4), ChildAccessForbidden, "write access through");
    expect_title(&e, "write access through <TAG> (a) at ALLOC[0x0] is forbidden");
    expect_detail(&e, "the accessed tag <TAG> (a) has state Frozen which forbids this child write access");
    expect_history(&e, &["the accessed tag <TAG> was created here, in the initial state Cell"]);
    expect_sites(&e, &[Some(created)]);
}

#[test]
fn cell_inside_struct_tree() {
    cell_inside_struct(false);
}

#[test]
fn cell_inside_struct_tree_implicit_writes() {
    cell_inside_struct(true);
}

/// Miri: tests/fail/tree_borrows/error-range.rs
#[test]
fn error_range() {
    let mut s = Sim::new();
    // let data = &mut [0u8; 16];
    let tmp = s.alloc(16);
    let data = s.mutable(tmp, 16).unwrap();
    // let rmut = &mut *addr_of_mut!(data[0..6]);
    let slice = index_mut_range(&mut s, data, 16, 0, 6).unwrap();
    let rmut = s.mutable(slice, 6).unwrap();
    let created = s.last_site();
    // rmut[3] += 1;
    // rmut[4] += 1;
    // rmut[5] += 1;
    for i in 3..6 {
        s.read(rmut.add(i), 1).unwrap();
        s.write(rmut.add(i), 1).unwrap();
    }
    // let _v = data[3];
    // let _v = data[4];
    // let _v = data[5];
    for i in 3..6 {
        s.read(data.add(i), 1).unwrap();
    }
    // data[4] = 1;
    s.write(data.add(4), 1).unwrap();
    // data[5] = 1;
    s.write(data.add(5), 1).unwrap();
    let disabled = s.last_site();
    // rmut[5] += 1;
    let e = expect_ub(s.read(rmut.add(5), 1), ChildAccessForbidden, "read access through");
    expect_title(&e, "read access through <TAG> at ALLOC[0x5] is forbidden");
    expect_detail(&e, "the accessed tag <TAG> has state Disabled which forbids this child read access");
    expect_history(
        &e,
        &[
            "the accessed tag <TAG> was created here, in the initial state Reserved",
            "the accessed tag <TAG> later transitioned to Disabled due to a foreign write access at offsets [0x5..0x6]",
            "this transition corresponds to a loss of read permissions",
        ],
    );
    expect_sites(&e, &[Some(created), Some(disabled), None]);
}

/// Miri: tests/fail/tree_borrows/fnentry_invalidation.rs
#[test]
fn fnentry_invalidation() {
    let mut s = Sim::new();
    // let mut x = 0i32;
    let x = s.alloc(4);
    // let z = &mut x as *mut i32;
    let z = s.mutable(x, 4).unwrap();
    let created = s.last_site();
    // *z = 1;
    s.write(z, 4).unwrap();
    let unique = s.last_site();
    // x.do_bad();   the receiver `&two_phase mut x`, then `Bad::do_bad(&mut self)`
    let this = s.two_phase(x, 4).unwrap();
    let frozen = s.last_site();
    s.call();
    s.arg_mut(this, 4).unwrap();
    s.ret().unwrap();
    // *z = 2;
    let e = expect_ub(s.write(z, 4), ChildAccessForbidden, "write access through");
    expect_title(&e, "write access through <TAG> at ALLOC[0x0] is forbidden");
    expect_detail(&e, "the accessed tag <TAG> has state Frozen which forbids this child write access");
    expect_history(
        &e,
        &[
            "the accessed tag <TAG> was created here, in the initial state Reserved",
            "the accessed tag <TAG> later transitioned to Unique due to a child write access at offsets [0x0..0x4]",
            "this transition corresponds to the first write to a 2-phase borrowed mutable reference",
            "the accessed tag <TAG> later transitioned to Frozen due to a reborrow (acting as a foreign read access) at offsets [0x0..0x4]",
            "this transition corresponds to a loss of write permissions",
        ],
    );
    expect_sites(&e, &[Some(created), Some(unique), None, Some(frozen), None]);
}

/// Miri: tests/fail/tree_borrows/frozen-lazy-write-to-surrounding.rs
#[test]
fn frozen_lazy_write_to_surrounding() {
    let mut s = Sim::new();
    // let pair = ((), 1);   `()` at 0, the `i32` at 0
    let pair = s.alloc(4);
    // let x = &pair.0;   zero bytes: `Frozen` only lazily, outside its range
    let x = s.shared(pair, 0).unwrap();
    let created = s.last_site();
    // let ptr = (&raw const *x).cast::<i32>().cast_mut();
    // unsafe { ptr.write(0) };
    let e = expect_ub(s.write(x, 4), ChildAccessForbidden, "write access");
    expect_title(&e, "write access through <TAG> at ALLOC[0x0] is forbidden");
    expect_detail(&e, "the accessed tag <TAG> has state Frozen which forbids this child write access");
    expect_history(&e, &["the accessed tag <TAG> was created here, in the initial state Frozen"]);
    expect_sites(&e, &[Some(created)]);
}

/// Miri: tests/fail/tree_borrows/outside-range.rs
#[test]
fn outside_range() {
    let mut s = Sim::new();
    // let data = &mut [0u8, 1, 2, 3];
    let tmp = s.alloc(4);
    let data = s.mutable(tmp, 4).unwrap();
    // let raw = data.as_mut_ptr();
    //   `<[u8]>::as_mut_ptr(&mut *data as &mut [u8])`: the receiver, the unsizing cast's
    //   result, and the argument; `self as *mut [T] as *mut T` keeps the argument's tag.
    let r = s.two_phase(data, 4).unwrap();
    let r = s.mutable(r, 4).unwrap();
    s.call();
    let raw = s.arg_mut(r, 4).unwrap();
    let raw_created = s.last_site();
    s.ret().unwrap();
    // stuff(&mut *raw, raw);
    let x = s.mutable(raw, 1).unwrap();
    s.call();
    let x = s.arg_mut(x, 1).unwrap();
    let x_created = s.last_site();
    //   let xraw = x as *mut u8;
    //   *y.add(1) = 42;
    s.write(raw.add(1), 1).unwrap();
    //   let _val = *xraw.add(2);
    s.read(x.add(2), 1).unwrap();
    //   let _val = *xraw.add(3);
    s.read(x.add(3), 1).unwrap();
    //   *y.add(3) = 42;
    let e = expect_ub(s.write(raw.add(3), 1), ProtectedDisabled, "write access through");
    expect_title(&e, "write access through <TAG> at ALLOC[0x3] is forbidden");
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
    expect_sites(&e, &[Some(raw_created), Some(x_created)]);
}

/// Miri: tests/fail/tree_borrows/parent_read_freezes_raw_mut.rs
#[test]
fn parent_read_freezes_raw_mut() {
    let mut s = Sim::new();
    // let mut root = 6u8;
    let root = s.alloc(1);
    // let mref = &mut root;
    let mref = s.mutable(root, 1).unwrap();
    let created = s.last_site();
    // let ptr = mref as *mut u8;
    // *ptr = 0; // Write
    s.write(mref, 1).unwrap();
    let unique = s.last_site();
    // assert_eq!(root, 0); // Parent Read
    //   `match (&root, &0) { (left_val, right_val) => if !(*left_val == *right_val) .. }`:
    //   `&root`, the tuple's field, the binding, then the read (`&0` is a promoted constant
    //   of its own).
    let left = s.shared(root, 1).unwrap();
    let frozen = s.last_site();
    let left = s.shared(left, 1).unwrap();
    let left = s.shared(left, 1).unwrap();
    s.read(left, 1).unwrap();
    // *ptr = 0;
    let e = expect_ub(s.write(mref, 1), ChildAccessForbidden, "write access through");
    expect_title(&e, "write access through <TAG> at ALLOC[0x0] is forbidden");
    expect_detail(&e, "the accessed tag <TAG> has state Frozen which forbids this child write access");
    expect_history(
        &e,
        &[
            "the accessed tag <TAG> was created here, in the initial state Reserved",
            "the accessed tag <TAG> later transitioned to Unique due to a child write access at offsets [0x0..0x1]",
            "this transition corresponds to the first write to a 2-phase borrowed mutable reference",
            "the accessed tag <TAG> later transitioned to Frozen due to a reborrow (acting as a foreign read access) at offsets [0x0..0x1]",
            "this transition corresponds to a loss of write permissions",
        ],
    );
    expect_sites(&e, &[Some(created), Some(unique), None, Some(frozen), None]);
}

/// Miri: tests/fail/tree_borrows/pass_invalid_mut.rs
#[test]
fn pass_invalid_mut() {
    let mut s = Sim::new();
    // let x = &mut 42;
    let tmp = s.alloc(4);
    let x = s.mutable(tmp, 4).unwrap();
    // let xraw = x as *mut _;
    // let xref = unsafe { &mut *xraw };
    let xref = s.mutable(x, 4).unwrap();
    let conflicting = s.last_site();
    // *xref = 18; // activate xref
    s.write(xref, 4).unwrap();
    let unique = s.last_site();
    // let _val = unsafe { *xraw }; // invalidate xref for writing
    s.read(x, 4).unwrap();
    let frozen = s.last_site();
    // foo(xref);   the reborrow `&mut *xref`, then the argument
    let r = s.mutable(xref, 4).unwrap();
    s.call();
    let nope = s.arg_mut(r, 4).unwrap();
    let accessed = s.last_site();
    //   *nope = 31;
    let e = expect_ub(s.write(nope, 4), ChildAccessForbidden, "write access through");
    expect_title(&e, "write access through <TAG> at ALLOC[0x0] is forbidden");
    expect_detail(&e, "the accessed tag <TAG> is a child of the conflicting tag <TAG>");
    expect_detail(&e, "the conflicting tag <TAG> has state Frozen which forbids this child write access");
    expect_history(
        &e,
        &[
            "the accessed tag <TAG> was created here",
            "the conflicting tag <TAG> was created here, in the initial state Reserved",
            "the conflicting tag <TAG> later transitioned to Unique due to a child write access at offsets [0x0..0x4]",
            "this transition corresponds to the first write to a 2-phase borrowed mutable reference",
            "the conflicting tag <TAG> later transitioned to Frozen due to a foreign read access at offsets [0x0..0x4]",
            "this transition corresponds to a loss of write permissions",
        ],
    );
    expect_sites(&e, &[Some(accessed), Some(conflicting), Some(unique), None, Some(frozen), None]);
}

/// Miri: tests/fail/tree_borrows/protector-write-lazy.rs
#[test]
fn protector_write_lazy() {
    let mut s = Sim::new();
    // let mut v = vec![0, 1];
    //   `<[i32]>::into_vec(Box::new([0, 1]))`: the `Vec` keeps the raw pointer
    //   `Box::into_raw(b)` makes, `(&mut **b) as *mut T`. (`v` itself is a local of its own
    //   allocation; the `&mut v` of `v.as_mut_ptr()` retags only that one.)
    let b = box_new(&mut s, 8).unwrap();
    s.call();
    let b = s.arg_box(b, 8).unwrap();
    s.call();
    let b = s.arg_box(b, 8).unwrap();
    let buf = s.mutable(b, 8).unwrap();
    s.ret().unwrap();
    s.ret().unwrap();
    // let ptr_to_vec = v.as_mut_ptr();   `self.buf.ptr()`: the pointer the `Vec` holds
    let ptr_to_vec = buf;
    // let ref_to_fst_elem = unsafe { &mut *ptr_to_vec };
    let r = s.mutable(ptr_to_vec, 4).unwrap();
    // let funky_ptr_lazy_on_fst_elem = the_other_function(ref_to_fst_elem, ptr_to_vec);
    let r = s.mutable(r, 4).unwrap();
    s.call();
    let r = s.arg_mut(r, 4).unwrap();
    //   *ref_to_fst_elem = 0;
    s.write(r, 4).unwrap();
    //   *ref_to_fst_elem = 42;
    s.write(r, 4).unwrap();
    //   let funky_ptr_lazy_on_fst_elem =
    //       unsafe { (&mut *(ptr_to_vec.wrapping_add(1))) as *mut i32 }.wrapping_sub(1);
    let funky = s.mutable(ptr_to_vec.add(4), 4).unwrap();
    let created = s.last_site();
    let funky = funky.sub(4);
    //   return funky_ptr_lazy_on_fst_elem;
    s.ret().unwrap();
    let released = s.last_site();
    // unsafe { println!("Value of funky: {}", *funky_ptr_lazy_on_fst_elem) }   `&*funky`
    let e = expect_ub(s.shared(funky, 4), ChildAccessForbidden, "reborrow through");
    expect_title(&e, "reborrow through <TAG> at ALLOC[0x0] is forbidden");
    expect_detail(
        &e,
        "the accessed tag <TAG> has state Disabled which forbids this reborrow (acting as a child read access)",
    );
    expect_history(
        &e,
        &[
            "the accessed tag <TAG> was created here, in the initial state Reserved",
            "the accessed tag <TAG> later transitioned to Disabled due to a protector release (acting as a foreign write access) on every location previously accessed by this tag",
            "this transition corresponds to a loss of read and write permissions",
        ],
    );
    expect_sites(&e, &[Some(created), Some(released), None]);
}

/// Miri: tests/fail/tree_borrows/repeated_foreign_read_lazy_conflicted.rs
#[test]
fn repeated_foreign_read_lazy_conflicted() {
    let mut s = Sim::new();
    // let mut alloc = [0u8, 0u8];
    let alloc = s.alloc(2);
    // let orig_ptr = addr_of_mut!(alloc) as *mut u8;
    let orig_ptr = alloc;
    // let foo = &mut *orig_ptr;
    let foo = s.mutable(orig_ptr, 1).unwrap();
    // do_something(alloc[0]);
    s.read(alloc, 1).unwrap();
    // access_after_sub_1(&mut *(foo as *mut u8).byte_add(1), orig_ptr);
    let x = s.mutable(foo.add(1), 1).unwrap();
    s.call();
    let x = s.arg_mut(x, 1).unwrap();
    let created = s.last_site();
    //   do_something(*orig_ptr);
    s.read(orig_ptr, 1).unwrap();
    let conflicted = s.last_site();
    //   *(x as *mut u8).byte_sub(1) = 42;
    let e = expect_ub(s.write(x.sub(1), 1), ChildAccessForbidden, "write access through");
    expect_title(&e, "write access through <TAG> at ALLOC[0x0] is forbidden");
    expect_detail(
        &e,
        "the accessed tag <TAG> has state Reserved (conflicted) which forbids this child write access",
    );
    expect_history(
        &e,
        &[
            "the accessed tag <TAG> was created here, in the initial state Reserved",
            "the accessed tag <TAG> later transitioned to Reserved (conflicted) due to a foreign read access at offsets [0x0..0x1]",
            "this transition corresponds to a temporary loss of write permissions until function exit",
        ],
    );
    expect_sites(&e, &[Some(created), Some(conflicted), None]);
}

/// Miri: tests/fail/tree_borrows/reservedim_spurious_write.rs, revisions `without` and
/// `with` (the spurious write `*x = 64`). The two threads run in the order the barriers
/// force (the `.stderr`'s "Thread n executing" lines). The barriers, `Arc`s and `eprintln!`s
/// touch other allocations only.
fn reservedim_spurious_write(with: bool) {
    let mut s = Sim::new();
    // let mut data = 0u8;
    let data = s.alloc(1);
    // let ptr = SendPtr(std::ptr::addr_of_mut!(data));
    // thread 1: inner(unsafe { &mut *ptr.0 }, b.clone());   (retag x)
    s.thread(1);
    let x = s.mutable(data, 1).unwrap();
    s.call();
    let x = s.arg_mut(x, 1).unwrap();
    //   *x = 42; // activate immediately
    s.write(x, 1).unwrap();
    // thread 2: let y_zst = unsafe { &mut *(ptr.0 as *mut Cell<()>) };   ([lazy] retag y)
    s.thread(2);
    let y = s.mutable_cell(data, 0, &[]).unwrap();
    // thread 2: let y = inner(y_zst, b.clone());
    s.call();
    let y = s.arg_mut_cell(y, 0, &[]).unwrap();
    let created = s.last_site();
    // thread 1: if cfg!(with) { *x = 64; }   (spurious write x)
    s.thread(1);
    let spurious = with.then(|| {
        s.write(x, 1).unwrap();
        s.last_site()
    });
    // thread 2: `y as *mut Cell<()> as *mut u8`, returned   (ret y)
    s.thread(2);
    s.ret().unwrap();
    // thread 1: `inner` returns   (ret x)
    s.thread(1);
    s.ret().unwrap();
    let released = s.last_site();
    // thread 2: unsafe { *y = 13 }   (write y)
    s.thread(2);
    let e = expect_ub(s.write(y, 1), ChildAccessForbidden, "write access through");
    expect_title(&e, "write access through <TAG> at ALLOC[0x0] is forbidden");
    expect_detail(&e, "the accessed tag <TAG> has state Disabled which forbids this child write access");
    match spurious {
        Some(spurious) => {
            expect_history(
                &e,
                &[
                    "the accessed tag <TAG> was created here, in the initial state Reserved",
                    "the accessed tag <TAG> later transitioned to Disabled due to a foreign write access at offsets [0x0..0x1]",
                    "this transition corresponds to a loss of read and write permissions",
                ],
            );
            expect_sites(&e, &[Some(created), Some(spurious), None]);
        }
        None => {
            expect_history(
                &e,
                &[
                    "the accessed tag <TAG> was created here, in the initial state Reserved",
                    "the accessed tag <TAG> later transitioned to Disabled due to a protector release (acting as a foreign write access) on every location previously accessed by this tag",
                    "this transition corresponds to a loss of read and write permissions",
                ],
            );
            expect_sites(&e, &[Some(created), Some(released), None]);
        }
    }
}

#[test]
fn reservedim_spurious_write_without() {
    reservedim_spurious_write(false);
}

#[test]
fn reservedim_spurious_write_with() {
    reservedim_spurious_write(true);
}

/// Miri: tests/fail/tree_borrows/return_invalid_mut.rs
#[test]
fn return_invalid_mut() {
    let mut s = Sim::new();
    // let arg = &mut (1, 2);
    let tmp = s.alloc(8);
    let arg = s.mutable(tmp, 8).unwrap();
    // let ret = foo(arg);   the reborrow `&mut *arg`, then the argument
    let r = s.mutable(arg, 8).unwrap();
    s.call();
    let x = s.arg_mut(r, 8).unwrap();
    //   let xraw = x as *mut (i32, i32);
    //   let ret = unsafe { &mut (*xraw).1 };
    let ret = s.mutable(x.add(4), 4).unwrap();
    let conflicting = s.last_site();
    //   *ret = *ret; // activate
    s.read(ret, 4).unwrap();
    s.write(ret, 4).unwrap();
    let unique = s.last_site();
    //   let _val = unsafe { *xraw }; // invalidate xref for writing
    s.read(x, 8).unwrap();
    let frozen = s.last_site();
    //   ret   (`_0 = move ret`)
    let ret = s.mutable(ret, 4).unwrap();
    s.ret().unwrap();
    // the return value copied into `ret`
    let ret = s.mutable(ret, 4).unwrap();
    let accessed = s.last_site();
    // *ret = 3;
    let e = expect_ub(s.write(ret, 4), ChildAccessForbidden, "write access through");
    expect_title(&e, "write access through <TAG> at ALLOC[0x4] is forbidden");
    expect_detail(&e, "the accessed tag <TAG> is a child of the conflicting tag <TAG>");
    expect_detail(&e, "the conflicting tag <TAG> has state Frozen which forbids this child write access");
    expect_history(
        &e,
        &[
            "the accessed tag <TAG> was created here",
            "the conflicting tag <TAG> was created here, in the initial state Reserved",
            "the conflicting tag <TAG> later transitioned to Unique due to a child write access at offsets [0x4..0x8]",
            "this transition corresponds to the first write to a 2-phase borrowed mutable reference",
            "the conflicting tag <TAG> later transitioned to Frozen due to a foreign read access at offsets [0x0..0x8]",
            "this transition corresponds to a loss of write permissions",
        ],
    );
    expect_sites(&e, &[Some(accessed), Some(conflicting), Some(unique), None, Some(frozen), None]);
}

/// Miri: tests/fail/tree_borrows/spurious_read.rs (`retagx_retagy_retx_writey_rety`). The
/// threads run in the order the barriers force; thread 1's `as_mut` returns while thread 2's
/// is still running.
#[test]
fn spurious_read() {
    let mut s = Sim::new();
    // let mut data = 0u8;
    let data = s.alloc(1);
    // let ptr = SendPtr(std::ptr::addr_of_mut!(data));
    // thread 1: let _x = as_mut(unsafe { &mut *ptr.0 }, b.clone());   (retag x)
    s.thread(1);
    let x = s.mutable(data, 1).unwrap();
    s.call();
    s.arg_mut(x, 1).unwrap();
    // thread 2: let _y = as_mut(unsafe { &mut *ptr.0 }, b.clone());   (retag y)
    s.thread(2);
    let y = s.mutable(data, 1).unwrap();
    s.call();
    let y = s.arg_mut(y, 1).unwrap();
    let created = s.last_site();
    // thread 1: `let x = x as *mut u8; x`, and `as_mut` returns   (ret x)
    s.thread(1);
    s.ret().unwrap();
    let released = s.last_site();
    s.thread(2);
    // thread 2: let y = y as *mut u8;
    //           unsafe { *y = 2; }   (write y)
    let e = expect_ub(s.write(y, 1), ChildAccessForbidden, "write access through");
    expect_title(&e, "write access through <TAG> at ALLOC[0x0] is forbidden");
    expect_detail(
        &e,
        "the accessed tag <TAG> has state Reserved (conflicted) which forbids this child write access",
    );
    expect_history(
        &e,
        &[
            "the accessed tag <TAG> was created here, in the initial state Reserved",
            "the accessed tag <TAG> later transitioned to Reserved (conflicted) due to a protector release (acting as a foreign read access) on every location previously accessed by this tag",
            "this transition corresponds to a temporary loss of write permissions until function exit",
        ],
    );
    expect_sites(&e, &[Some(created), Some(released), None]);
}

/// Miri: tests/fail/tree_borrows/strongly-protected.rs
#[test]
fn strongly_protected() {
    let mut s = Sim::new();
    // inner(Box::leak(Box::new(0)), |raw| { drop(unsafe { Box::from_raw(raw) }); });
    let b = box_new(&mut s, 4).unwrap();
    let leaked = box_leak(&mut s, b, 4).unwrap();
    //   the reborrow of the `&mut i32` argument, then `inner(x: &mut i32, f: fn(*mut i32))`
    let x = s.mutable(leaked, 4).unwrap();
    s.call();
    let x = s.arg_mut(x, 4).unwrap();
    let protected = s.last_site();
    //   f(x)   `x as *mut i32`, through the closure's shim: no reference arguments
    s.call();
    //     Box::from_raw(raw) -> Box::from_raw_in(raw, Global): `Box(Unique::new_unchecked(raw), alloc)`
    s.call();
    s.call();
    let bx = s.boxed(x, 4).unwrap();
    s.ret().unwrap();
    let bx = s.boxed(bx, 4).unwrap();
    s.ret().unwrap();
    let bx = s.boxed(bx, 4).unwrap();
    //     drop(..): `fn drop<T>(_x: T)`
    s.call();
    let dropped = s.arg_box(bx, 4).unwrap();
    let accessed = s.last_site();
    //       the drop of `_x`: `<Box<i32> as Drop>::drop` frees through the `Box`'s pointer
    let e = expect_ub(s.free(dropped, 4), ProtectedDealloc, "deallocation through");
    expect_title(&e, "deallocation through <TAG> at ALLOC[0x0] is forbidden");
    expect_detail(&e, "the allocation of the accessed tag <TAG> also contains the strongly protected tag <TAG>");
    expect_detail(&e, "the strongly protected tag <TAG> disallows deallocations");
    expect_history(
        &e,
        &[
            "the accessed tag <TAG> was created here",
            "the strongly protected tag <TAG> was created here, in the initial state Reserved",
        ],
    );
    expect_sites(&e, &[Some(accessed), Some(protected)]);
}

/// Miri: tests/fail/tree_borrows/subtree_traversal_skipping_diagnostics.rs. The foreign read
/// of `*other_ptr` skips the subtree under the `Frozen` intermediary, so `m` is not
/// conflicted and the error names the intermediary.
#[test]
fn subtree_traversal_skipping_diagnostics() {
    let mut s = Sim::new();
    // let root = 42u8;
    let root = s.alloc(1);
    // let intermediary = &root;
    let intermediary = s.shared(root, 1).unwrap();
    let conflicting = s.last_site();
    // let data = &mut *(core::ptr::addr_of!(*intermediary) as *mut u8);
    let data = s.mutable(intermediary, 1).unwrap();
    // write_to_mut(data, core::ptr::addr_of!(root));   the reborrow, then the argument
    let m = s.mutable(data, 1).unwrap();
    s.call();
    let m = s.arg_mut(m, 1).unwrap();
    let accessed = s.last_site();
    //   std::hint::black_box(*other_ptr);
    s.read(root, 1).unwrap();
    //   *m = 42;
    let e = expect_ub(s.write(m, 1), ChildAccessForbidden, "write access through");
    expect_title(&e, "write access through <TAG> at ALLOC[0x0] is forbidden");
    expect_detail(&e, "the accessed tag <TAG> is a child of the conflicting tag <TAG>");
    expect_detail(&e, "the conflicting tag <TAG> has state Frozen which forbids this child write access");
    expect_history(
        &e,
        &[
            "the accessed tag <TAG> was created here",
            "the conflicting tag <TAG> was created here, in the initial state Frozen",
        ],
    );
    expect_sites(&e, &[Some(accessed), Some(conflicting)]);
}

/// Miri: tests/fail/tree_borrows/write-during-2phase.rs
#[test]
fn write_during_2phase() {
    let mut s = Sim::new();
    // let mut f = Foo(0);
    let f = s.alloc(8);
    // let alias = &mut f.0 as *mut u64;
    let alias = s.mutable(f, 8).unwrap();
    // let res = f.add(unsafe { *alias = 42; 0 });   the receiver `&two_phase mut f` first
    let this = s.two_phase(f, 8).unwrap();
    let created = s.last_site();
    //   *alias = 42;
    s.write(alias, 8).unwrap();
    let disabled = s.last_site();
    //   Foo::add(&mut self, n)
    s.call();
    let e = expect_ub(s.arg_mut(this, 8), ChildAccessForbidden, "reborrow through");
    expect_title(&e, "reborrow through <TAG> at ALLOC[0x0] is forbidden");
    expect_detail(
        &e,
        "the accessed tag <TAG> has state Disabled which forbids this reborrow (acting as a child read access)",
    );
    expect_history(
        &e,
        &[
            "the accessed tag <TAG> was created here, in the initial state Reserved",
            "the accessed tag <TAG> later transitioned to Disabled due to a foreign write access at offsets [0x0..0x8]",
            "this transition corresponds to a loss of read and write permissions",
        ],
    );
    expect_sites(&e, &[Some(created), Some(disabled), None]);
}

/// Miri: tests/fail/tree_borrows/reserved/*.rs (run with `-Zmiri-provenance-gc=0`; the model
/// only collects when asked).
mod reserved {
    use super::*;

    const CELL_PROTECTED_WRITE_TREE: &str = "\
──────────────────────────────────────────────────
Warning: this tree is indicative only. Some tags may have been hidden.
0..   1
| Unq |    └─┬──<TAG=root of the allocation>
| ReIM|      └─┬──<TAG=base>
| ReIM|        ├─┬──<TAG=x>
| ReIM|        │ └─┬──<TAG=caller:x>
| Res |        │   └────<TAG=callee:x> Strongly protected
| ReIM|        └────<TAG=y, callee:y, caller:y>
──────────────────────────────────────────────────
";

    /// Miri: tests/fail/tree_borrows/reserved/cell-protected-write.rs
    #[test]
    fn cell_protected_write() {
        let mut s = Sim::new();
        let cell = [Span::new(0, 1)];
        // let n = &mut UnsafeCell::new(0u8);
        let tmp = s.alloc(1);
        let n = s.mutable_cell(tmp, 1, &cell).unwrap();
        // name!(n as *mut _, "base");
        s.name(n, "base");
        // let x = &mut *(n as *mut UnsafeCell<_>);
        let x = s.mutable_cell(n, 1, &cell).unwrap();
        // name!(x as *mut _, "x");
        s.name(x, "x");
        // let y = (&mut *n) as *mut UnsafeCell<_> as *mut _;
        let y = s.mutable_cell(n, 1, &cell).unwrap();
        let accessed = s.last_site();
        // name!(y);
        s.name(y, "y");
        // write_second(x, y);   the reborrow, then the argument
        let x = s.mutable_cell(x, 1, &cell).unwrap();
        s.call();
        let x = s.arg_mut_cell(x, 1, &cell).unwrap();
        let protected = s.last_site();
        //   let alloc_id = alloc_id!(x.get());   `UnsafeCell::get(&self)`: hidden `Cell` tags
        let r = s.shared_cell(x, 1, &cell).unwrap();
        s.call();
        s.arg_shared_cell(r, 1, &cell).unwrap();
        s.ret().unwrap();
        //   name!(x as *mut _, "callee:x");
        s.name(x, "callee:x");
        //   name!((x as *mut _)=>1, "caller:x");
        assert!(s.b.name_tag(x.alloc, x.tag(), 1, "caller:x"));
        //   name!(y, "callee:y");
        s.name(y, "callee:y");
        //   name!(y, "caller:y");
        s.name(y, "caller:y");
        //   print_state!(alloc_id);
        assert_eq!(normalize_tree(&s.print(x, false)), CELL_PROTECTED_WRITE_TREE);
        //   *y = 1;
        let e = expect_ub(s.write(y, 1), ProtectedDisabled, "write access through");
        expect_protected_write_error(&e, accessed, protected);
    }

    const INT_PROTECTED_WRITE_TREE: &str = "\
──────────────────────────────────────────────────
Warning: this tree is indicative only. Some tags may have been hidden.
0..   1
| Unq |    └─┬──<TAG=root of the allocation>
| Res |      └─┬──<TAG=n>
| Res |        ├─┬──<TAG=x>
| Res |        │ └─┬──<TAG=caller:x>
| Res |        │   └────<TAG=callee:x> Strongly protected
| Res |        └────<TAG=y, callee:y, caller:y>
──────────────────────────────────────────────────
";

    /// Miri: tests/fail/tree_borrows/reserved/int-protected-write.rs
    #[test]
    fn int_protected_write() {
        let mut s = Sim::new();
        // let n = &mut 0u8;
        let tmp = s.alloc(1);
        let n = s.mutable(tmp, 1).unwrap();
        // name!(n);
        s.name(n, "n");
        // let x = &mut *(n as *mut _);
        let x = s.mutable(n, 1).unwrap();
        // name!(x);
        s.name(x, "x");
        // let y = (&mut *n) as *mut _;
        let y = s.mutable(n, 1).unwrap();
        let accessed = s.last_site();
        // name!(y);
        s.name(y, "y");
        // write_second(x, y);   the reborrow, then the argument
        let x = s.mutable(x, 1).unwrap();
        s.call();
        let x = s.arg_mut(x, 1).unwrap();
        let protected = s.last_site();
        //   let alloc_id = alloc_id!(x);
        //   name!(x, "callee:x");
        s.name(x, "callee:x");
        //   name!(x=>1, "caller:x");
        assert!(s.b.name_tag(x.alloc, x.tag(), 1, "caller:x"));
        //   name!(y, "callee:y");
        s.name(y, "callee:y");
        //   name!(y, "caller:y");
        s.name(y, "caller:y");
        //   print_state!(alloc_id);
        assert_eq!(normalize_tree(&s.print(x, false)), INT_PROTECTED_WRITE_TREE);
        //   *y = 0;
        let e = expect_ub(s.write(y, 1), ProtectedDisabled, "write access through");
        expect_protected_write_error(&e, accessed, protected);
    }

    /// The error both programs end with: the write through `y` would disable `callee:x`.
    #[track_caller]
    fn expect_protected_write_error(e: &AliasError, accessed: SiteId, protected: SiteId) {
        expect_title(e, "write access through <TAG> (y, callee:y, caller:y) at ALLOC[0x0] is forbidden");
        expect_detail(
            e,
            "the accessed tag <TAG> (y, callee:y, caller:y) is foreign to the protected tag <TAG> (callee:x) (i.e., it is not a child)",
        );
        expect_detail(
            e,
            "this foreign write access would cause the protected tag <TAG> (callee:x) (currently Reserved) to become Disabled",
        );
        expect_detail(e, "protected tags must never be Disabled");
        expect_history(
            e,
            &[
                "the accessed tag <TAG> was created here",
                "the protected tag <TAG> was created here, in the initial state Reserved",
            ],
        );
        expect_sites(e, &[Some(accessed), Some(protected)]);
    }
}

/// Miri: tests/fail/tree_borrows/implicit_writes/*.rs, run with
/// `-Zmiri-tree-borrows-implicit-writes`: a protected `&mut` or `Box` argument is `Unique` at
/// once and its retag a write. Only function-entry retags of `&mut` and `Box` change, so only
/// those use [`iw_arg_mut`] / [`arg`]; none of the callees is `#[rustc_no_writable]`.
mod implicit_writes {
    use super::*;

    /// Miri: tests/fail/tree_borrows/implicit_writes/as_mut_ptr.rs
    #[test]
    fn as_mut_ptr() {
        let mut s = Sim::new();
        // let mut x: [u8; 3] = [1, 2, 3];
        let x = s.alloc(3);
        // let ptr = std::ptr::from_mut(&mut x);   `from_mut(r: &mut T) -> *mut T { r }`
        let r = s.mutable(x, 3).unwrap();
        s.call();
        let ptr = iw_arg_mut(&mut s, r, 3).unwrap();
        s.ret().unwrap();
        // let a = unsafe { &mut *ptr };
        let a = s.mutable(ptr, 3).unwrap();
        // let b = unsafe { &mut *ptr };
        let b = s.mutable(ptr, 3).unwrap();
        let created = s.last_site();
        // let _c = as_mut_ptr(a);   the reborrow, then the argument
        let a = s.mutable(a, 3).unwrap();
        s.call();
        iw_arg_mut(&mut s, a, 3).unwrap();
        let disabled = s.last_site();
        //   x as *mut [u8] as *mut u8
        s.ret().unwrap();
        // println!("{:?}", *b);   `&*b`
        let e = expect_ub(s.shared(b, 3), ChildAccessForbidden, "reborrow through");
        expect_title(&e, "reborrow through <TAG> at ALLOC[0x0] is forbidden");
        expect_detail(
            &e,
            "the accessed tag <TAG> has state Disabled which forbids this reborrow (acting as a child read access)",
        );
        expect_history(
            &e,
            &[
                "the accessed tag <TAG> was created here, in the initial state Reserved",
                "the accessed tag <TAG> later transitioned to Disabled due to a reborrow (acting as a foreign write access) at offsets [0x0..0x3]",
                "this transition corresponds to a loss of read and write permissions",
            ],
        );
        expect_sites(&e, &[Some(created), Some(disabled), None]);
    }

    /// Miri: tests/fail/tree_borrows/implicit_writes/fnentry_invalidation.rs
    #[test]
    fn fnentry_invalidation() {
        let mut s = Sim::new();
        // let mut x = 0i32;
        let x = s.alloc(4);
        // let z = &mut x as *mut i32;
        let z = s.mutable(x, 4).unwrap();
        let created = s.last_site();
        // x.do_bad();   the receiver `&two_phase mut x`, then `Bad::do_bad(&mut self)`
        let this = s.two_phase(x, 4).unwrap();
        s.call();
        iw_arg_mut(&mut s, this, 4).unwrap();
        let disabled = s.last_site();
        s.ret().unwrap();
        // let _oof = *z;
        let e = expect_ub(s.read(z, 4), ChildAccessForbidden, "read access through");
        expect_title(&e, "read access through <TAG> at ALLOC[0x0] is forbidden");
        expect_detail(&e, "the accessed tag <TAG> has state Disabled which forbids this child read access");
        expect_history(
            &e,
            &[
                "the accessed tag <TAG> was created here, in the initial state Reserved",
                "the accessed tag <TAG> later transitioned to Disabled due to a reborrow (acting as a foreign write access) at offsets [0x0..0x4]",
                "this transition corresponds to a loss of read and write permissions",
            ],
        );
        expect_sites(&e, &[Some(created), Some(disabled), None]);
    }

    /// The error the three `ptr_write` programs end with: the read through `y` would disable
    /// the `Unique` protected argument `x`.
    #[track_caller]
    fn expect_ptr_write_error(e: &AliasError, accessed_name: &str, accessed: SiteId, protected: SiteId) {
        expect_title(e, &format!("read access through <TAG>{accessed_name} at ALLOC[0x0] is forbidden"));
        expect_detail(
            e,
            &format!("the accessed tag <TAG>{accessed_name} is foreign to the protected tag <TAG> (i.e., it is not a child)"),
        );
        expect_detail(
            e,
            "this foreign read access would cause the protected tag <TAG> (currently Unique) to become Disabled",
        );
        expect_detail(e, "protected tags must never be Disabled");
        expect_history(
            e,
            &[
                "the accessed tag <TAG> was created here",
                "the protected tag <TAG> was created here, in the initial state Reserved",
            ],
        );
        expect_sites(e, &[Some(accessed), Some(protected)]);
    }

    /// Miri: tests/fail/tree_borrows/implicit_writes/ptr_write.rs
    #[test]
    fn ptr_write() {
        let mut s = Sim::new();
        // let mut x = 0u8;
        let x = s.alloc(1);
        let root = s.last_site();
        // let ptr = &raw mut x;   the root's tag
        // let res = dereference(&mut x, ptr);
        let r = s.mutable(x, 1).unwrap();
        s.call();
        iw_arg_mut(&mut s, r, 1).unwrap();
        let protected = s.last_site();
        //   let _ = unsafe { *y };
        let e = expect_ub(s.read(x, 1), ProtectedDisabled, "read access");
        expect_ptr_write_error(&e, " (root of the allocation)", root, protected);
    }

    /// Miri: tests/fail/tree_borrows/implicit_writes/ptr_write_box.rs
    #[test]
    fn ptr_write_box() {
        let mut s = Sim::new();
        // let mut x: Box<u8> = Box::new(0u8);
        let x = box_new(&mut s, 1).unwrap();
        let accessed = s.last_site();
        // let ptr = &raw mut *x;
        // let res = dereference(x, ptr);   the `Box` moved into the argument
        s.call();
        arg(&mut s, x, PtrKind::Box, true, &[], 1, true).unwrap();
        let protected = s.last_site();
        //   let _ = unsafe { *y };
        let e = expect_ub(s.read(x, 1), ProtectedDisabled, "read access");
        expect_ptr_write_error(&e, "", accessed, protected);
    }

    /// Miri: tests/fail/tree_borrows/implicit_writes/ptr_write_unsafe_cell.rs
    #[test]
    fn ptr_write_unsafe_cell() {
        let mut s = Sim::new();
        let cell = [Span::new(0, 1)];
        // let mut x: UnsafeCell<u8> = 0u8.into();
        let x = s.alloc(1);
        // let ptr = x.get();   `UnsafeCell::get(&self) -> *mut T`
        let r = s.shared_cell(x, 1, &cell).unwrap();
        s.call();
        let ptr = s.arg_shared_cell(r, 1, &cell).unwrap();
        let accessed = s.last_site();
        s.ret().unwrap();
        // let res = dereference(&mut x, ptr);
        let r = s.mutable_cell(x, 1, &cell).unwrap();
        s.call();
        arg(&mut s, r, PtrKind::Mut, false, &cell, 1, true).unwrap();
        let protected = s.last_site();
        //   let _ = unsafe { *y };
        let e = expect_ub(s.read(ptr, 1), ProtectedDisabled, "read access");
        expect_ptr_write_error(&e, "", accessed, protected);
    }

    /// Miri: tests/fail/tree_borrows/implicit_writes/retag_is_race.rs. Miri's error is a data
    /// race between thread 1's read `*x` and the write of thread 2's retag of `y` on entry to
    /// `write`; reporting it is the race detector's (`crate::race`, `Races::retag`). Tree
    /// Borrows allows the retag and must report its implicit access as a write, which is what
    /// the race detector is given. The threads run in the order of the `.stderr`
    /// (`yield_now` lets thread 1 read first).
    #[test]
    fn retag_is_race() {
        let mut s = Sim::new();
        // let mut data = 0u8;
        let data = s.alloc(1);
        // let p = SendPtr(addr_of_mut!(data));
        // thread 1: let x = unsafe { &mut *x.0 };
        let x = s.mutable(data, 1).unwrap();
        // thread 2: let y = unsafe { &mut *y.0 };
        let y = s.mutable(data, 1).unwrap();
        // thread 1: let _v = *x;
        s.read(x, 1).unwrap();
        // thread 2: write(&mut *y, 42, &barrier);   the reborrow (a read), then the argument
        s.thread(2);
        let y = s.mutable(y, 1).unwrap();
        assert_eq!(s.b.retag_accesses(), &[(Span::new(0, 1), AccessKind::Read)]);
        s.call();
        iw_arg_mut(&mut s, y, 1).unwrap();
        assert_eq!(s.b.retag_accesses(), &[(Span::new(0, 1), AccessKind::Write)]);
    }

    /// Miri: tests/fail/tree_borrows/implicit_writes/static_memory_modification.rs. Miri
    /// refuses `foo(x)`'s retag as a write to read-only memory (`tb_reborrow` checks the
    /// allocation's mutability before the tree sees the access; validity reports it as
    /// "mutable reference pointing to read-only memory"). Tree Borrows doesn't know which
    /// memory is read-only: here the retag's implicit access must be a write, which the tree
    /// refuses too, through the `Frozen` `&X` it descends from. The reborrow before it only
    /// reads, which read-only memory allows.
    #[test]
    fn static_memory_modification() {
        let mut s = Sim::new();
        // static X: usize = 5;
        let x_static = s.alloc(8);
        // let x = unsafe { std::mem::transmute::<&usize, &mut usize>(&X) };
        //   `&X`: the constant `&usize` copied into a temporary, then `&*` of it; the
        //   transmute's `&mut usize` result is a typed write too
        let c = s.shared(x_static, 8).unwrap();
        let r = s.shared(c, 8).unwrap();
        let conflicting = s.last_site();
        let x = s.mutable(r, 8).unwrap();
        // foo(x);   the reborrow, then the argument
        let x = s.mutable(x, 8).unwrap();
        let accessed = s.last_site();
        assert_eq!(s.b.retag_accesses(), &[(Span::new(0, 8), AccessKind::Read)]);
        // What `check` asks before the retag, to refuse it as `WriteToReadOnly` as Miri does.
        let entry = NewPermission::new(PtrKind::Mut, RetagMode::FnEntry, true, true, &[]);
        assert!(entry.clone().with_implicit_writes(true).writes());
        assert!(!entry.writes());
        s.call();
        let e = expect_ub(iw_arg_mut(&mut s, x, 8), ChildAccessForbidden, "reborrow through");
        expect_title(&e, "reborrow through <TAG> at ALLOC[0x0] is forbidden");
        expect_detail(&e, "the accessed tag <TAG> is a child of the conflicting tag <TAG>");
        expect_detail(
            &e,
            "the conflicting tag <TAG> has state Frozen which forbids this reborrow (acting as a child write access)",
        );
        expect_history(
            &e,
            &[
                "the accessed tag <TAG> was created here",
                "the conflicting tag <TAG> was created here, in the initial state Frozen",
            ],
        );
        expect_sites(&e, &[Some(accessed), Some(conflicting)]);
    }
}
