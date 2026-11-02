//! Miri's `tests/pass/tree_borrows/*.rs` and `tests/pass/tree_borrows/wildcard/*.rs`: each
//! function a program runs is a test doing the retags and accesses Miri does for it (the rules
//! are `sim.rs`'s), and every step must be `Ok`. A program with the revisions `tree` and
//! `tree_implicit_writes` is a function of `implicit_writes` with a test per revision
//! (`revisions!`); under `-Zmiri-tree-borrows-implicit-writes` a function-entry retag of a
//! `&mut` or `Box` is `NewPermission::with_implicit_writes(true)`, unless the callee is
//! `#[rustc_no_writable]`.
//!
//! An argument of reference type passed to a parameter of reference type is reborrowed first
//! (rustc's coercion: `bar(&mut x)` is `&mut x`, `&mut *` of it, then the argument's retag, the
//! three tags `implicit-writes-permissions.stderr` shows); one passed to a generic `T` is moved.
//! A return value is retagged into the caller after the callee's protectors end (Miri:
//! `before_stack_pop`, then the return value's copy).
//!
//! Left out everywhere, as none of them can change a verdict here: accesses to other
//! allocations (the local holding a pointer, a closure's environment, a `&mut self` to the
//! struct holding the reference), the initializing write of a local before any reference to it
//! exists, and the deallocation of locals when their frame returns (after the last access
//! through any tag of theirs). Layouts are rustc's: fields of one alignment in declaration
//! order, zero-sized fields first.

use super::sim::{Ptr, R, Sim, normalize};
use crate::{
    borrows::{NewPermission, PtrKind, RetagMode},
    ids::Span,
};

/// A test per revision of a program function that takes `implicit_writes`: `tree`, and
/// `tree_implicit_writes` (`-Zmiri-tree-borrows-implicit-writes`).
macro_rules! revisions {
    ($($name:ident),* $(,)?) => {
        $(
            mod $name {
                #[test]
                fn tree() -> super::R {
                    super::$name(false)
                }

                #[test]
                fn tree_implicit_writes() -> super::R {
                    super::$name(true)
                }
            }
        )*
    };
}

/// The cells of an `UnsafeCell<T>` / `Cell<T>` of `len` bytes: all of it.
fn all(len: u64) -> [Span; 1] {
    [Span::new(0, len)]
}

/// The function-entry retag of an argument, with implicit writes `implicit_writes`.
fn entry(s: &mut Sim, kind: PtrKind, freeze: bool, cells: &[Span], implicit_writes: bool, p: Ptr, len: u64) -> R<Ptr> {
    let new = NewPermission::new(kind, RetagMode::FnEntry, freeze, true, cells).with_implicit_writes(implicit_writes);
    s.retag_with(p, new, len)
}

/// The argument `&mut T`, `T: Freeze`.
fn entry_mut(s: &mut Sim, implicit_writes: bool, p: Ptr, len: u64) -> R<Ptr> {
    entry(s, PtrKind::Mut, true, &[], implicit_writes, p, len)
}

/// The argument `&mut UnsafeCell<T>` of `len` bytes.
fn entry_mut_cell(s: &mut Sim, implicit_writes: bool, p: Ptr, len: u64) -> R<Ptr> {
    entry(s, PtrKind::Mut, false, &all(len), implicit_writes, p, len)
}

/// The argument `Box<UnsafeCell<T>>` of `len` bytes.
fn entry_box_cell(s: &mut Sim, implicit_writes: bool, p: Ptr, len: u64) -> R<Ptr> {
    entry(s, PtrKind::Box, false, &all(len), implicit_writes, p, len)
}

/// A `Box<UnsafeCell<T>>` of `len` bytes made from (or copied as) `p`.
fn box_cell(s: &mut Sim, p: Ptr, len: u64) -> R<Ptr> {
    s.retag(p, PtrKind::Box, RetagMode::Default, false, true, &all(len), len)
}

/// `name!(p => nth, name)`.
fn name_nth(s: &mut Sim, p: Ptr, nth: u8, name: &str) {
    assert!(s.b.name_tag(p.alloc, p.tag(), nth, name), "no {nth}th parent of {p:?}");
}

/// `UnsafeCell::get(&self) -> *mut T` on the `UnsafeCell<T>` of `len` bytes `cell` points to:
/// the receiver `&*cell` and its function-entry retag, both `Cell` (no access); the returned
/// `self as *const _ as *mut T` keeps the argument's tag.
fn unsafe_cell_get(s: &mut Sim, cell: Ptr, len: u64) -> R<Ptr> {
    let c = all(len);
    let receiver = s.shared_cell(cell, len, &c)?;
    s.call();
    let this = s.arg_shared_cell(receiver, len, &c)?;
    s.ret()?;
    Ok(this)
}

/// `<[T]>::as_ptr(&self)` on the slice of `len` bytes `r` points to: the receiver `&*r`, its
/// function-entry retag, and the result `self as *const [T] as *const T` with its tag.
fn slice_as_ptr(s: &mut Sim, r: Ptr, len: u64) -> R<Ptr> {
    let receiver = s.shared(r, len)?;
    s.call();
    let this = s.arg_shared(receiver, len)?;
    s.ret()?;
    Ok(this)
}

/// `<[T]>::as_mut_ptr(&mut self)`, `#[rustc_no_writable]`: as [`slice_as_ptr`] with `&mut`.
fn slice_as_mut_ptr(s: &mut Sim, r: Ptr, len: u64) -> R<Ptr> {
    let receiver = s.mutable(r, len)?;
    s.call();
    let this = entry_mut(s, false, receiver, len)?;
    s.ret()?;
    Ok(this)
}

/// `assert_eq!(*r, v)` for a `Freeze` `*r` of `len` bytes: `match (&*r, &v)` reborrows `*r`,
/// and the comparison reads through it.
fn assert_eq_deref(s: &mut Sim, r: Ptr, len: u64) -> R {
    let left = s.shared(r, len)?;
    s.read(left, len)
}

/// Asserts the tree of `p`'s allocation (`miri_print_borrow_state(alloc_id!(p),
/// show_unnamed)`) is `expected`, the block of Miri's `.stderr` (after a leading newline).
#[track_caller]
fn assert_tree(s: &Sim, p: Ptr, show_unnamed: bool, expected: &str) {
    let got = normalize_tree(&s.print(p, show_unnamed));
    let expected = expected.strip_prefix('\n').unwrap_or(expected);
    assert_eq!(got, expected, "tree printed:\n{got}");
}

/// A printed tree as Miri's test filters leave it (`tests/ui.rs`, `stderr_filters`): `<n>`
/// (by `normalize`) and `<n=` erased to `<TAG>` and `<TAG=`, and borders cut to 50 `─`. A
/// border under 50 is as wide as the tree's widest line, which still depends on how many digits
/// the tags have; Miri's count std's allocations too, so they differ from these. The one such
/// border in these files (`implicit-writes-permissions.stderr`) was printed with three-digit
/// tags, as wide as `<TAG>`, so a border here is the widest line's width after erasing, cut at
/// 50.
fn normalize_tree(printed: &str) -> String {
    let lines: Vec<String> = printed.lines().map(|line| erase_named_tags(&normalize(line))).collect();
    let is_border = |line: &str| !line.is_empty() && line.chars().all(|c| c == '─');
    let width = lines.iter().filter(|l| !is_border(l.as_str())).map(|l| l.chars().count()).max().unwrap_or(0).min(50);
    let mut out = String::with_capacity(printed.len());
    for line in &lines {
        if is_border(line.as_str()) {
            out.extend(std::iter::repeat_n('─', width));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// `<n=` to `<TAG=`.
fn erase_named_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let digits = after.bytes().take_while(|b| b.is_ascii_digit()).count();
        if digits > 0 && after[digits..].starts_with('=') {
            out.push_str("<TAG=");
            rest = &after[digits + 1..];
        } else {
            out.push('<');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

#[test]
fn normalize_tree_erases_tags_and_cuts_borders() {
    let printed = "──────\n0..   1\n| Unq |    └─┬──<1=root of the allocation>\n| Unq |      └────<12>\n──────\n";
    let root = "| Unq |    └─┬──<TAG=root of the allocation>";
    let border = "─".repeat(root.chars().count());
    let want = format!("{border}\n0..   1\n{root}\n| Unq |      └────<TAG>\n{border}\n");
    assert_eq!(normalize_tree(printed), want);
    let wide = format!("{}\n{}\n", "─".repeat(70), "x".repeat(70));
    assert_eq!(normalize_tree(&wide), format!("{}\n{}\n", "─".repeat(50), "x".repeat(70)));
}

// Miri: tests/pass/tree_borrows/cell-alternate-writes.rs. No `&mut` or `Box` argument, so both
// revisions do the same.
fn cell_alternate_writes(_implicit_writes: bool) -> R {
    let mut s = Sim::new();
    let c = all(1);
    // let data = &mut UnsafeCell::new(0u8);
    let tmp = s.alloc(1);
    let data = s.mutable_cell(tmp, 1, &c)?;
    // name!(data as *mut _, "data");
    s.name(data, "data");
    // let x = &*data;
    let x = s.shared_cell(data, 1, &c)?;
    // name!(x as *const _, "x");
    s.name(x, "x");
    // let y = &*data;
    let y = s.shared_cell(data, 1, &c)?;
    // name!(y as *const _, "y");
    s.name(y, "y");
    // let alloc_id = alloc_id!(data.get());
    unsafe_cell_get(&mut s, data, 1)?;
    // print_state!(alloc_id);
    assert_tree(
        &s,
        data,
        false,
        r"
──────────────────────────────────────────────────
Warning: this tree is indicative only. Some tags may have been hidden.
0..   1
| Unq |    └─┬──<TAG=root of the allocation>
| ReIM|      └─┬──<TAG=data>
|?Cel |        ├────<TAG=x>
|?Cel |        └────<TAG=y>
──────────────────────────────────────────────────
",
    );
    // *y.get() = 1;
    let p = unsafe_cell_get(&mut s, y, 1)?;
    s.write(p, 1)?;
    // *x.get() = 2;
    let p = unsafe_cell_get(&mut s, x, 1)?;
    s.write(p, 1)?;
    // *y.get() = 3;
    let p = unsafe_cell_get(&mut s, y, 1)?;
    s.write(p, 1)?;
    // *x.get() = 4;
    let p = unsafe_cell_get(&mut s, x, 1)?;
    s.write(p, 1)?;
    // print_state!(alloc_id);
    assert_tree(
        &s,
        data,
        false,
        r"
──────────────────────────────────────────────────
Warning: this tree is indicative only. Some tags may have been hidden.
0..   1
| Unq |    └─┬──<TAG=root of the allocation>
| Unq |      └─┬──<TAG=data>
| Cel |        ├────<TAG=x>
| Cel |        └────<TAG=y>
──────────────────────────────────────────────────
",
    );
    Ok(())
}

// Miri: tests/pass/tree_borrows/cell-inside-box.rs. `UnsafeCell<i32>`: 4 bytes, all a cell.
fn cell_inside_box(implicit_writes: bool) -> R {
    let mut s = Sim::new();
    let c = all(4);
    // let cell = UnsafeCell::new(42);
    // let box1 = Box::new(cell);
    // `Box::new` writes through the new block's raw pointer, `mem::transmute`s it into the
    // `Box` (a Default retag at the `Box` type) and returns it (another).
    let heap = s.alloc(4);
    s.write(heap, 4)?;
    let b = box_cell(&mut s, heap, 4)?;
    let box1 = box_cell(&mut s, b, 4)?;
    // let ptr1: *mut UnsafeCell<i32> = Box::into_raw(box1);
    // `into_raw(b: Self)`: `let mut b = mem::ManuallyDrop::new(b)`, whose
    // `MaybeDangling::new(value)` is not retagged inside once built (both
    // `#[rustc_no_writable]`), then `(&mut **b) as *mut T`.
    s.call();
    let b = entry_box_cell(&mut s, implicit_writes, box1, 4)?;
    s.call();
    let value = entry_box_cell(&mut s, false, b, 4)?;
    s.call();
    let value = entry_box_cell(&mut s, false, value, 4)?;
    s.ret()?;
    s.ret()?;
    let ptr1 = s.mutable_cell(value, 4, &c)?;
    s.ret()?;
    // name!(ptr1);
    s.name(ptr1, "ptr1");
    // let mut box2 = Box::from_raw(ptr1);
    // `from_raw_in` builds the `Box` from its fields and returns it, and `from_raw` returns it
    // again: two Default retags.
    let b = box_cell(&mut s, ptr1, 4)?;
    let box2 = box_cell(&mut s, b, 4)?;
    // let ptr2: *mut UnsafeCell<i32> = Box::as_mut_ptr(&mut box2);
    // `&raw mut **b`: the `Box`'s own tag (`&mut box2` points into the local, not here).
    let ptr2 = box2;
    // name!(ptr2);
    s.name(ptr2, "ptr2");
    // let x = (*ptr1).get();
    let x = unsafe_cell_get(&mut s, ptr1, 4)?;
    // *x = 1;
    s.write(x, 4)?;
    // let val = *(*ptr2).get();
    let p = unsafe_cell_get(&mut s, ptr2, 4)?;
    s.read(p, 4)?;
    // assert_eq!(val, 1);
    // let alloc_id = alloc_id!(ptr1);
    // print_state!(alloc_id);
    assert_tree(
        &s,
        ptr1,
        false,
        r"
──────────────────────────────────────────────────
Warning: this tree is indicative only. Some tags may have been hidden.
0..   4
| Unq |    └─┬──<TAG=root of the allocation>
| Unq |      └─┬──<TAG=ptr1>
| ReIM|        └────<TAG=ptr2>
──────────────────────────────────────────────────
",
    );
    // `box2` is dropped: `Box::drop` takes `Layout::for_value(&**self)` and frees the block
    // through the `Box`'s pointer.
    s.shared_cell(box2, 4, &c)?;
    s.free(box2, 4)
}

/// `Cell::set(&self, val)` called with the receiver `cell` (a `&Cell<T>` of `len` bytes, `T:
/// Freeze`): `self.replace(val)`, which is `mem::replace(unsafe { &mut *self.value.get() },
/// val)`, reading then writing through that `&mut T`.
fn cell_set(s: &mut Sim, implicit_writes: bool, cell: Ptr, len: u64) -> R {
    let c = all(len);
    s.call();
    let this = s.arg_shared_cell(cell, len, &c)?;
    // self.replace(val)
    let receiver = s.shared_cell(this, len, &c)?;
    s.call();
    let this = s.arg_shared_cell(receiver, len, &c)?;
    // mem::replace(unsafe { &mut *self.value.get() }, val)
    let value = unsafe_cell_get(s, this, len)?;
    let dest = s.mutable(value, len)?;
    let dest = s.mutable(dest, len)?;
    s.call();
    let dest = entry_mut(s, implicit_writes, dest, len)?;
    // read_via_copy(dest); write_via_move(dest, src)
    s.read(dest, len)?;
    s.write(dest, len)?;
    s.ret()?;
    s.ret()?;
    s.ret()
}

// Miri: tests/pass/tree_borrows/cell-inside-struct.rs, under
// `-Zmiri-tree-borrows-no-precise-interior-mut`: a reference to a `!Freeze` type has the cell
// permission on all of it, as if all of it were an `UnsafeCell`. `Foo`: `field1` at 0..4,
// `field2` at 4..8.
fn cell_inside_struct(implicit_writes: bool) -> R {
    let mut s = Sim::new();
    // let root = Foo { field1: 42, field2: Cell::new(88) };
    let root = s.alloc(8);
    // let a = &root;
    let a = s.shared_cell(root, 8, &all(8))?;
    // name!(a as *const Foo, "a");
    s.name(a, "a");
    // let a: *const Foo = a as *const Foo;
    // let a: *mut Foo = a as *mut Foo;
    // let alloc_id = alloc_id!(a);
    // print_state!(alloc_id);
    assert_tree(
        &s,
        a,
        false,
        r"
──────────────────────────────────────────────────
Warning: this tree is indicative only. Some tags may have been hidden.
0..   8
| Unq |    └─┬──<TAG=root of the allocation>
|?Cel |      └────<TAG=a>
──────────────────────────────────────────────────
",
    );
    // (*a).field2.set(10);
    let field2 = s.shared_cell(a.add(4), 4, &all(4))?;
    cell_set(&mut s, implicit_writes, field2, 4)?;
    // (*a).field1 = 88;
    s.write(a, 4)
}

/// `fn foo(x: &Cell<i32>) { (x as *const Cell<i32> as *mut Cell<i32> as *mut
/// i32).offset(1).write(0) }` called with `x`.
fn lazy_write_foo(s: &mut Sim, x: Ptr) -> R {
    let c = all(4);
    let x = s.shared_cell(x, 4, &c)?;
    s.call();
    let x = s.arg_shared_cell(x, 4, &c)?;
    s.write(x.add(4), 4)?;
    s.ret()
}

// Miri: tests/pass/tree_borrows/cell-lazy-write-to-surrounding.rs. No `&mut` or `Box`
// argument, so both revisions do the same.
fn cell_lazy_write_to_surrounding(_implicit_writes: bool) -> R {
    let mut s = Sim::new();
    let c = all(4);
    // let arr = [Cell::new(1), Cell::new(1)];
    let arr = s.alloc(8);
    // foo(&arr[0]);
    let x = s.shared_cell(arr, 4, &c)?;
    lazy_write_foo(&mut s, x)?;
    // let pair = (Cell::new(1), 1);
    let pair = s.alloc(8);
    // foo(&pair.0);
    let x = s.shared_cell(pair, 4, &c)?;
    lazy_write_foo(&mut s, x)?;
    // let pair = (Cell::new(()), 1);
    // The zero-sized `Cell<()>` at offset 0, the `i32` at 0..4.
    let pair = s.alloc(4);
    // let x = &pair.0;
    let x = s.shared_cell(pair, 0, &[])?;
    // let ptr = (&raw const *x).cast::<i32>().cast_mut();
    // unsafe { ptr.write(0) };
    s.write(x, 4)
}

// Miri: tests/pass/tree_borrows/copy-nonoverlapping.rs, test_to_from. `as_mut_ptr` is
// `#[rustc_no_writable]`, so both revisions do the same.
fn copy_nonoverlapping_test_to_from(_implicit_writes: bool) -> R {
    let mut s = Sim::new();
    // let data = &mut [0u64, 1];
    let tmp = s.alloc(16);
    let data = s.mutable(tmp, 16)?;
    // let to = data.as_mut_ptr().add(1);
    let to = slice_as_mut_ptr(&mut s, data, 16)?.add(8);
    // let from = data.as_ptr();
    let from = slice_as_ptr(&mut s, data, 16)?;
    // std::ptr::copy_nonoverlapping(from, to, 1);
    s.read(from, 8)?;
    s.write(to, 8)
}

// Miri: tests/pass/tree_borrows/copy-nonoverlapping.rs, test_from_to.
fn copy_nonoverlapping_test_from_to(_implicit_writes: bool) -> R {
    let mut s = Sim::new();
    // let data = &mut [0u64, 1];
    let tmp = s.alloc(16);
    let data = s.mutable(tmp, 16)?;
    // let from = data.as_ptr();
    let from = slice_as_ptr(&mut s, data, 16)?;
    // let to = data.as_mut_ptr().add(1);
    let to = slice_as_mut_ptr(&mut s, data, 16)?.add(8);
    // std::ptr::copy_nonoverlapping(from, to, 1);
    s.read(from, 8)?;
    s.write(to, 8)
}

// Miri: tests/pass/tree_borrows/end-of-protector.rs.
fn end_of_protector(implicit_writes: bool) -> R {
    let mut s = Sim::new();
    // let data = &mut 0u8;
    let tmp = s.alloc(1);
    let data = s.mutable(tmp, 1)?;
    // name!(data);
    s.name(data, "data");
    // let alloc_id = alloc_id!(data);
    // let x = &mut *data;
    let x = s.mutable(data, 1)?;
    // name!(x);
    s.name(x, "x");
    // print_state!(alloc_id);
    assert_tree(
        &s,
        data,
        false,
        r"
──────────────────────────────────────────────────
Warning: this tree is indicative only. Some tags may have been hidden.
0..   1
| Unq |    └─┬──<TAG=root of the allocation>
| Res |      └─┬──<TAG=data>
| Res |        └────<TAG=x>
──────────────────────────────────────────────────
",
    );
    // do_nothing(x); // creates then removes a Protector for a child of x
    let arg = s.mutable(x, 1)?;
    s.call();
    let callee_x = entry_mut(&mut s, implicit_writes, arg, 1)?;
    // unsafe fn do_nothing(x: &mut u8) {
    //     name!(x, "callee:x");
    s.name(callee_x, "callee:x");
    //     name!(x=>1, "caller:x");
    name_nth(&mut s, callee_x, 1, "caller:x");
    //     let alloc_id = alloc_id!(x);
    //     print_state!(alloc_id);
    let in_callee = if implicit_writes {
        r"
──────────────────────────────────────────────────
Warning: this tree is indicative only. Some tags may have been hidden.
0..   1
| Unq |    └─┬──<TAG=root of the allocation>
| Unq |      └─┬──<TAG=data>
| Unq |        └─┬──<TAG=x>
| Unq |          └─┬──<TAG=caller:x>
| Unq |            └────<TAG=callee:x> Strongly protected
──────────────────────────────────────────────────
"
    } else {
        r"
──────────────────────────────────────────────────
Warning: this tree is indicative only. Some tags may have been hidden.
0..   1
| Unq |    └─┬──<TAG=root of the allocation>
| Res |      └─┬──<TAG=data>
| Res |        └─┬──<TAG=x>
| Res |          └─┬──<TAG=caller:x>
| Res |            └────<TAG=callee:x> Strongly protected
──────────────────────────────────────────────────
"
    };
    assert_tree(&s, data, false, in_callee);
    // }
    s.ret()?;
    // let y = &mut *data;
    let y = s.mutable(data, 1)?;
    // name!(y);
    s.name(y, "y");
    // print_state!(alloc_id);
    let after_call = if implicit_writes {
        r"
──────────────────────────────────────────────────
Warning: this tree is indicative only. Some tags may have been hidden.
0..   1
| Unq |    └─┬──<TAG=root of the allocation>
| Unq |      └─┬──<TAG=data>
| Frz |        ├─┬──<TAG=x>
| Frz |        │ └─┬──<TAG=caller:x>
| Frz |        │   └────<TAG=callee:x>
| Res |        └────<TAG=y>
──────────────────────────────────────────────────
"
    } else {
        r"
──────────────────────────────────────────────────
Warning: this tree is indicative only. Some tags may have been hidden.
0..   1
| Unq |    └─┬──<TAG=root of the allocation>
| Res |      └─┬──<TAG=data>
| Res |        ├─┬──<TAG=x>
| Res |        │ └─┬──<TAG=caller:x>
| Res |        │   └────<TAG=callee:x>
| Res |        └────<TAG=y>
──────────────────────────────────────────────────
"
    };
    assert_tree(&s, data, false, after_call);
    // // Invalidates the previous reborrow, but its Protector has been removed.
    // *y = 1;
    s.write(y, 1)?;
    // print_state!(alloc_id);
    assert_tree(
        &s,
        data,
        false,
        r"
──────────────────────────────────────────────────
Warning: this tree is indicative only. Some tags may have been hidden.
0..   1
| Unq |    └─┬──<TAG=root of the allocation>
| Unq |      └─┬──<TAG=data>
| Dis |        ├─┬──<TAG=x>
| Dis |        │ └─┬──<TAG=caller:x>
| Dis |        │   └────<TAG=callee:x>
| Unq |        └────<TAG=y>
──────────────────────────────────────────────────
",
    );
    Ok(())
}

// Miri: tests/pass/tree_borrows/formatting.rs, alignment_check.
#[test]
fn formatting_alignment_check() -> R {
    let mut s = Sim::new();
    // let data: &mut [u8] = &mut [0; 1024];
    let tmp = s.alloc(1024);
    let data = s.mutable(tmp, 1024)?;
    // name!(data.as_ptr()=>2, "data");
    let p = slice_as_ptr(&mut s, data, 1024)?;
    name_nth(&mut s, p, 2, "data");
    // name!(data.as_ptr()=>2, "data");
    let p = slice_as_ptr(&mut s, data, 1024)?;
    name_nth(&mut s, p, 2, "data");
    // let alloc_id = alloc_id!(data.as_ptr());
    slice_as_ptr(&mut s, data, 1024)?;
    for (i, name) in [(1, "data[1]"), (10, "data[10]"), (100, "data[100]")] {
        // let x = &mut data[i];
        let x = s.mutable(data.add(i), 1)?;
        // name!(x as *mut _, "data[i]");
        s.name(x, name);
        // *x = 1;
        s.write(x, 1)?;
    }
    // let _val = data[100]; // So that the above is Frz
    s.read(data.add(100), 1)?;
    // let x = &mut data[1000];
    let x = s.mutable(data.add(1000), 1)?;
    // name!(x as *mut _, "data[1000]");
    s.name(x, "data[1000]");
    // *x = 1;
    s.write(x, 1)?;
    // print_state!(alloc_id);
    assert_tree(
        &s,
        data,
        false,
        r"
──────────────────────────────────────────────────
Warning: this tree is indicative only. Some tags may have been hidden.
0..   1..   2..  10..  11.. 100.. 101..1000..1001..1024
| Unq | Unq | Unq | Unq | Unq | Unq | Unq | Unq | Unq |    └─┬──<TAG=root of the allocation>
| Res | Unq | Res | Unq | Res | Unq | Res | Unq | Res |      └─┬──<TAG=data, data>
|-----| Unq |-----|?Dis |-----|?Dis |-----|?Dis |-----|        ├────<TAG=data[1]>
|-----|-----|-----| Unq |-----|?Dis |-----|?Dis |-----|        ├────<TAG=data[10]>
|-----|-----|-----|-----|-----| Frz |-----|?Dis |-----|        ├────<TAG=data[100]>
|-----|-----|-----|-----|-----|-----|-----| Unq |-----|        └────<TAG=data[1000]>
──────────────────────────────────────────────────
",
    );
    Ok(())
}

// Miri: tests/pass/tree_borrows/formatting.rs, structure_check.
#[test]
fn formatting_structure_check() -> R {
    let mut s = Sim::new();
    // let x = &0u8;
    // A promoted constant: its own allocation, the reference a retag of its root pointer.
    let promoted = s.alloc(1);
    let x = s.shared(promoted, 1)?;
    // name!(x);
    s.name(x, "x");
    // Each `let child = &*parent; name!(child);`, in the program's order.
    let mut ptrs = vec![("x", x)];
    for (child, parent) in [
        ("xa", "x"),
        ("xb", "x"),
        ("xc", "x"),
        ("xaa", "xa"),
        ("xab", "xa"),
        ("xba", "xb"),
        ("xbaa", "xba"),
        ("xbaaa", "xbaa"),
        ("xbaaaa", "xbaaa"),
        ("xca", "xc"),
        ("xcb", "xc"),
        ("xcaa", "xca"),
        ("xcab", "xca"),
        ("xcba", "xcb"),
        ("xcbb", "xcb"),
    ] {
        let parent = ptrs.iter().find(|(name, _)| *name == parent).unwrap().1;
        let p = s.shared(parent, 1)?;
        s.name(p, child);
        ptrs.push((child, p));
    }
    // let alloc_id = alloc_id!(x);
    // print_state!(alloc_id);
    assert_tree(
        &s,
        x,
        false,
        r"
──────────────────────────────────────────────────
Warning: this tree is indicative only. Some tags may have been hidden.
0..   1
| Unq |    └─┬──<TAG=root of the allocation>
| Frz |      └─┬──<TAG=x>
| Frz |        ├─┬──<TAG=xa>
| Frz |        │ ├────<TAG=xaa>
| Frz |        │ └────<TAG=xab>
| Frz |        ├─┬──<TAG=xb>
| Frz |        │ └─┬──<TAG=xba>
| Frz |        │   └─┬──<TAG=xbaa>
| Frz |        │     └─┬──<TAG=xbaaa>
| Frz |        │       └────<TAG=xbaaaa>
| Frz |        └─┬──<TAG=xc>
| Frz |          ├─┬──<TAG=xca>
| Frz |          │ ├────<TAG=xcaa>
| Frz |          │ └────<TAG=xcab>
| Frz |          └─┬──<TAG=xcb>
| Frz |            ├────<TAG=xcba>
| Frz |            └────<TAG=xcbb>
──────────────────────────────────────────────────
",
    );
    Ok(())
}

// Miri: tests/pass/tree_borrows/implicit-writes-permissions.rs, under
// `-Zmiri-tree-borrows-implicit-writes`.
#[test]
fn implicit_writes_permissions() -> R {
    let mut s = Sim::new();
    // let mut x = 0u8;
    let x = s.alloc(1);
    // bar(&mut x);
    let r = s.mutable(x, 1)?;
    let r = s.mutable(r, 1)?;
    s.call();
    let arg = entry_mut(&mut s, true, r, 1)?;
    // fn bar(x: &mut u8) {
    //     miri_print_borrow_state(miri_get_alloc_id(x), true);
    assert_tree(
        &s,
        arg,
        true,
        r"
──────────────────────────────────────────────
0..   1
| Unq |    └─┬──<TAG=root of the allocation>
| Unq |      └─┬──<TAG>
| Unq |        └─┬──<TAG>
| Unq |          └────<TAG> Strongly protected
──────────────────────────────────────────────
",
    );
    // }
    s.ret()
}

// Miri: tests/pass/tree_borrows/mem_forget-implicit_writes.rs, under
// `-Zmiri-tree-borrows-implicit-writes`: `mem::forget`, `ManuallyDrop::new` and
// `MaybeDangling::new` are `#[rustc_no_writable]`.
#[test]
fn mem_forget_implicit_writes() -> R {
    let mut s = Sim::new();
    // let mut val = 10u32;
    let val = s.alloc(4);
    // let r: &mut u32 = &mut val;
    let r = s.mutable(val, 4)?;
    // let to: &mut i32 = unsafe { transmute(r) };
    s.call();
    // unsafe fn transmute<From, To>(from: From) -> To {
    let from = entry_mut(&mut s, true, r, 4)?;
    //     let to = unsafe { mem::transmute_copy(&from) };
    // `transmute_copy` reads the pointer out of `from`'s local and returns it as a `&mut i32`:
    // a Default retag of it.
    let to = s.mutable(from, 4)?;
    //     mem::forget(from);
    s.call();
    let t = entry_mut(&mut s, false, from, 4)?;
    // `let _ = ManuallyDrop::new(t)`, which is `ManuallyDrop { value: MaybeDangling::new(t) }`.
    s.call();
    let value = entry_mut(&mut s, false, t, 4)?;
    s.call();
    entry_mut(&mut s, false, value, 4)?;
    s.ret()?;
    s.ret()?;
    s.ret()?;
    //     to
    // }
    s.ret()?;
    let to = s.mutable(to, 4)?;
    // assert_eq!(*to, 10);
    assert_eq_deref(&mut s, to, 4)
}

/// `fn dereference<T>(x: T, y: *mut u8) -> T { let _ = unsafe { *y }; x }` with `x` the
/// `&mut` (or `Box`, `PtrKind::Box`) `x` to the 1 byte `UnsafeCell<u8>` (`cell`) or `u8`;
/// returns the result in the caller.
fn dereference(s: &mut Sim, implicit_writes: bool, kind: PtrKind, cell: bool, x: Ptr, y: Ptr) -> R<Ptr> {
    let c = all(1);
    let cells: &[Span] = if cell { &c } else { &[] };
    s.call();
    let x = entry(s, kind, !cell, cells, implicit_writes, x, 1)?;
    // let _ = unsafe { *y };
    s.read(y, 1)?;
    // x
    let ret = s.retag(x, kind, RetagMode::Default, !cell, true, cells, 1)?;
    s.ret()?;
    s.retag(ret, kind, RetagMode::Default, !cell, true, cells, 1)
}

// Miri: tests/pass/tree_borrows/no_implicit_writes.rs, normal.
#[test]
fn no_implicit_writes_normal() -> R {
    let mut s = Sim::new();
    // let mut x = 0u8;
    let x = s.alloc(1);
    // let ptr = &raw mut x;
    let ptr = x;
    // let res = dereference(&mut x, ptr);
    let r = s.mutable(x, 1)?;
    let res = dereference(&mut s, false, PtrKind::Mut, false, r, ptr)?;
    // assert_eq!(*res, 0);
    assert_eq_deref(&mut s, res, 1)
}

// Miri: tests/pass/tree_borrows/no_implicit_writes.rs, unsafe_cell.
#[test]
fn no_implicit_writes_unsafe_cell() -> R {
    let mut s = Sim::new();
    let c = all(1);
    // let mut x: UnsafeCell<u8> = 0u8.into();
    let x = s.alloc(1);
    // let ptr = x.get();
    let ptr = unsafe_cell_get(&mut s, x, 1)?;
    // let res = dereference(&mut x, ptr);
    let r = s.mutable_cell(x, 1, &c)?;
    let res = dereference(&mut s, false, PtrKind::Mut, true, r, ptr)?;
    // assert_eq!(*res.get_mut(), 0);
    // `UnsafeCell::get_mut(&mut self) -> &mut T` is `&mut self.value`.
    let receiver = s.mutable_cell(res, 1, &c)?;
    s.call();
    let this = entry_mut_cell(&mut s, false, receiver, 1)?;
    let value = s.mutable(this, 1)?;
    s.ret()?;
    let value = s.mutable(value, 1)?;
    assert_eq_deref(&mut s, value, 1)
}

// Miri: tests/pass/tree_borrows/no_implicit_writes.rs, box_test.
#[test]
fn no_implicit_writes_box_test() -> R {
    let mut s = Sim::new();
    // let mut x: Box<u8> = Box::new(0u8);
    let heap = s.alloc(1);
    s.write(heap, 1)?;
    let b = s.boxed(heap, 1)?;
    let x = s.boxed(b, 1)?;
    // let ptr = &raw mut *x;
    let ptr = x;
    // let res = dereference(x, ptr);
    let res = dereference(&mut s, false, PtrKind::Box, false, x, ptr)?;
    // assert_eq!(*res, 0);
    assert_eq_deref(&mut s, res, 1)?;
    // `res` is dropped: `Box::drop` takes `Layout::for_value(&**self)` and frees the block.
    s.shared(res, 1)?;
    s.free(res, 1)
}

// Miri: tests/pass/tree_borrows/no_implicit_writes.rs, as_mut_ptr_test. `[u8; 3]`: 3 bytes.
#[test]
fn no_implicit_writes_as_mut_ptr_test() -> R {
    let mut s = Sim::new();
    // let mut x: [u8; 3] = [1, 2, 3];
    let x = s.alloc(3);
    // let ptr = std::ptr::from_mut(&mut x);
    let r = s.mutable(x, 3)?;
    let r = s.mutable(r, 3)?;
    s.call();
    let ptr = s.arg_mut(r, 3)?;
    s.ret()?;
    // let a = unsafe { &mut *ptr };
    let a = s.mutable(ptr, 3)?;
    // let b = unsafe { &mut *ptr };
    let b = s.mutable(ptr, 3)?;
    // let _c = as_mut_ptr(a);
    // `const fn as_mut_ptr(x: &mut [u8; 3]) -> *mut u8 { x as *mut [u8] as *mut u8 }`, without
    // `#[rustc_no_writable]`.
    let a = s.mutable(a, 3)?;
    s.call();
    s.arg_mut(a, 3)?;
    s.ret()?;
    // assert_eq!(*b, [1, 2, 3]);
    // `match (&*b, &[1, 2, 3])`, then `<[u8; 3] as PartialEq>::eq(&*left_val, &*right_val)`,
    // which reads both.
    let left = s.shared(b, 3)?;
    let left = s.shared(left, 3)?;
    s.call();
    let left = s.arg_shared(left, 3)?;
    s.read(left, 3)?;
    s.ret()
}

// Miri: tests/pass/tree_borrows/reborrow-is-read.rs. No `&mut` or `Box` argument, so both
// revisions do the same.
fn reborrow_is_read(_implicit_writes: bool) -> R {
    let mut s = Sim::new();
    // let parent = &mut 0u8;
    let tmp = s.alloc(1);
    let parent = s.mutable(tmp, 1)?;
    // name!(parent);
    s.name(parent, "parent");
    // let alloc_id = alloc_id!(parent);
    // let x = &mut *parent;
    let x = s.mutable(parent, 1)?;
    // name!(x);
    s.name(x, "x");
    // *x = 0; // x is now Unique
    s.write(x, 1)?;
    // print_state!(alloc_id);
    assert_tree(
        &s,
        parent,
        false,
        r"
──────────────────────────────────────────────────
Warning: this tree is indicative only. Some tags may have been hidden.
0..   1
| Unq |    └─┬──<TAG=root of the allocation>
| Unq |      └─┬──<TAG=parent>
| Unq |        └────<TAG=x>
──────────────────────────────────────────────────
",
    );
    // let y = &mut *parent;
    let y = s.mutable(parent, 1)?;
    // name!(y);
    s.name(y, "y");
    // // Check in the debug output that x has been Frozen by the reborrow
    // print_state!(alloc_id);
    assert_tree(
        &s,
        parent,
        false,
        r"
──────────────────────────────────────────────────
Warning: this tree is indicative only. Some tags may have been hidden.
0..   1
| Unq |    └─┬──<TAG=root of the allocation>
| Unq |      └─┬──<TAG=parent>
| Frz |        ├────<TAG=x>
| Res |        └────<TAG=y>
──────────────────────────────────────────────────
",
    );
    Ok(())
}

/// `unsafe fn read_second<T>(x: &mut T, y: *mut u8)` of `reserved.rs`, called with `x` (the
/// `&mut` to the 1 byte `T`, an `UnsafeCell<u8>` if `cell`) and `y`.
fn read_second(s: &mut Sim, cell: bool, x: Ptr, y: Ptr) -> R {
    let c = all(1);
    let x = if cell { s.mutable_cell(x, 1, &c)? } else { s.mutable(x, 1)? };
    s.call();
    let x = if cell { s.arg_mut_cell(x, 1, &c)? } else { s.arg_mut(x, 1)? };
    // name!(x as *mut T as *mut u8=>1, "caller:x");
    name_nth(s, x, 1, "caller:x");
    // name!(x as *mut T as *mut u8, "callee:x");
    s.name(x, "callee:x");
    // name!(y, "caller:y");
    s.name(y, "caller:y");
    // name!(y, "callee:y");
    s.name(y, "callee:y");
    // let _val = *y;
    s.read(y, 1)?;
    s.ret()
}

// Miri: tests/pass/tree_borrows/reserved.rs, cell_protected_read.
#[test]
fn reserved_cell_protected_read() -> R {
    let mut s = Sim::new();
    let c = all(1);
    // let base = &mut UnsafeCell::new(0u8);
    let tmp = s.alloc(1);
    let base = s.mutable_cell(tmp, 1, &c)?;
    // name!(base as *mut _, "base");
    s.name(base, "base");
    // let alloc_id = alloc_id!(base.get());
    unsafe_cell_get(&mut s, base, 1)?;
    // let x = &mut *(base as *mut UnsafeCell<u8>);
    let x = s.mutable_cell(base, 1, &c)?;
    // name!(x as *mut _, "x");
    s.name(x, "x");
    // let y = &mut *base as *mut UnsafeCell<u8> as *mut u8;
    let y = s.mutable_cell(base, 1, &c)?;
    // name!(y);
    s.name(y, "y");
    // read_second(x, y); // Foreign Read for callee:x
    read_second(&mut s, true, x, y)?;
    // print_state!(alloc_id);
    assert_tree(
        &s,
        base,
        false,
        r"
──────────────────────────────────────────────────
Warning: this tree is indicative only. Some tags may have been hidden.
0..   1
| Unq |    └─┬──<TAG=root of the allocation>
| ReIM|      └─┬──<TAG=base>
| ReIM|        ├─┬──<TAG=x>
| ReIM|        │ └─┬──<TAG=caller:x>
| ResC|        │   └────<TAG=callee:x>
| ReIM|        └────<TAG=y, caller:y, callee:y>
──────────────────────────────────────────────────
",
    );
    Ok(())
}

// Miri: tests/pass/tree_borrows/reserved.rs, cell_unprotected_read.
#[test]
fn reserved_cell_unprotected_read() -> R {
    let mut s = Sim::new();
    let c = all(8);
    // let base = &mut UnsafeCell::new(0u64);
    let tmp = s.alloc(8);
    let base = s.mutable_cell(tmp, 8, &c)?;
    // name!(base as *mut _, "base");
    s.name(base, "base");
    // let alloc_id = alloc_id!(base.get());
    unsafe_cell_get(&mut s, base, 8)?;
    // let x = &mut *(base as *mut UnsafeCell<_>);
    let x = s.mutable_cell(base, 8, &c)?;
    // name!(x as *mut _, "x");
    s.name(x, "x");
    // let y = &mut *base as *mut UnsafeCell<u64> as *mut u64;
    let y = s.mutable_cell(base, 8, &c)?;
    // name!(y);
    s.name(y, "y");
    // let _val = *y; // Foreign Read for x
    s.read(y, 8)?;
    // print_state!(alloc_id);
    assert_tree(
        &s,
        base,
        false,
        r"
──────────────────────────────────────────────────
Warning: this tree is indicative only. Some tags may have been hidden.
0..   8
| Unq |    └─┬──<TAG=root of the allocation>
| ReIM|      └─┬──<TAG=base>
| ReIM|        ├────<TAG=x>
| ReIM|        └────<TAG=y>
──────────────────────────────────────────────────
",
    );
    Ok(())
}

// Miri: tests/pass/tree_borrows/reserved.rs, cell_unprotected_write.
#[test]
fn reserved_cell_unprotected_write() -> R {
    let mut s = Sim::new();
    let c = all(8);
    // let base = &mut UnsafeCell::new(0u64);
    let tmp = s.alloc(8);
    let base = s.mutable_cell(tmp, 8, &c)?;
    // name!(base as *mut _, "base");
    s.name(base, "base");
    // let alloc_id = alloc_id!(base.get());
    unsafe_cell_get(&mut s, base, 8)?;
    // let x = &mut *(base as *mut UnsafeCell<u64>);
    let x = s.mutable_cell(base, 8, &c)?;
    // name!(x as *mut _, "x");
    s.name(x, "x");
    // let y = &mut *base as *mut UnsafeCell<u64> as *mut u64;
    let y = s.mutable_cell(base, 8, &c)?;
    // name!(y);
    s.name(y, "y");
    // *y = 1; // Foreign Write for x
    s.write(y, 8)?;
    // print_state!(alloc_id);
    assert_tree(
        &s,
        base,
        false,
        r"
──────────────────────────────────────────────────
Warning: this tree is indicative only. Some tags may have been hidden.
0..   8
| Unq |    └─┬──<TAG=root of the allocation>
| Unq |      └─┬──<TAG=base>
| ReIM|        ├────<TAG=x>
| Unq |        └────<TAG=y>
──────────────────────────────────────────────────
",
    );
    Ok(())
}

// Miri: tests/pass/tree_borrows/reserved.rs, int_protected_read.
#[test]
fn reserved_int_protected_read() -> R {
    let mut s = Sim::new();
    // let base = &mut 0u8;
    let tmp = s.alloc(1);
    let base = s.mutable(tmp, 1)?;
    // let alloc_id = alloc_id!(base);
    // name!(base);
    s.name(base, "base");
    // let x = &mut *(base as *mut u8);
    let x = s.mutable(base, 1)?;
    // name!(x);
    s.name(x, "x");
    // let y = (&mut *base) as *mut u8;
    let y = s.mutable(base, 1)?;
    // name!(y);
    s.name(y, "y");
    // read_second(x, y); // Foreign Read for callee:x
    read_second(&mut s, false, x, y)?;
    // print_state!(alloc_id);
    assert_tree(
        &s,
        base,
        false,
        r"
──────────────────────────────────────────────────
Warning: this tree is indicative only. Some tags may have been hidden.
0..   1
| Unq |    └─┬──<TAG=root of the allocation>
| Res |      └─┬──<TAG=base>
| Res |        ├─┬──<TAG=x>
| Res |        │ └─┬──<TAG=caller:x>
| ResC|        │   └────<TAG=callee:x>
| Res |        └────<TAG=y, caller:y, callee:y>
──────────────────────────────────────────────────
",
    );
    Ok(())
}

// Miri: tests/pass/tree_borrows/reserved.rs, int_unprotected_read.
#[test]
fn reserved_int_unprotected_read() -> R {
    let mut s = Sim::new();
    // let base = &mut 0u8;
    let tmp = s.alloc(1);
    let base = s.mutable(tmp, 1)?;
    // name!(base);
    s.name(base, "base");
    // let alloc_id = alloc_id!(base);
    // let x = &mut *(base as *mut u8);
    let x = s.mutable(base, 1)?;
    // name!(x);
    s.name(x, "x");
    // let y = (&mut *base) as *mut u8;
    let y = s.mutable(base, 1)?;
    // name!(y);
    s.name(y, "y");
    // let _val = *y; // Foreign Read for x
    s.read(y, 1)?;
    // print_state!(alloc_id);
    assert_tree(
        &s,
        base,
        false,
        r"
──────────────────────────────────────────────────
Warning: this tree is indicative only. Some tags may have been hidden.
0..   1
| Unq |    └─┬──<TAG=root of the allocation>
| Res |      └─┬──<TAG=base>
| Res |        ├────<TAG=x>
| Res |        └────<TAG=y>
──────────────────────────────────────────────────
",
    );
    Ok(())
}

// Miri: tests/pass/tree_borrows/reserved.rs, int_unprotected_write.
#[test]
fn reserved_int_unprotected_write() -> R {
    let mut s = Sim::new();
    // let base = &mut 0u8;
    let tmp = s.alloc(1);
    let base = s.mutable(tmp, 1)?;
    // name!(base);
    s.name(base, "base");
    // let alloc_id = alloc_id!(base);
    // let x = &mut *(base as *mut u8);
    let x = s.mutable(base, 1)?;
    // name!(x);
    s.name(x, "x");
    // let y = (&mut *base) as *mut u8;
    let y = s.mutable(base, 1)?;
    // name!(y);
    s.name(y, "y");
    // *y = 1; // Foreign Write for x
    s.write(y, 1)?;
    // print_state!(alloc_id);
    assert_tree(
        &s,
        base,
        false,
        r"
──────────────────────────────────────────────────
Warning: this tree is indicative only. Some tags may have been hidden.
0..   1
| Unq |    └─┬──<TAG=root of the allocation>
| Unq |      └─┬──<TAG=base>
| Dis |        ├────<TAG=x>
| Unq |        └────<TAG=y>
──────────────────────────────────────────────────
",
    );
    Ok(())
}

// Miri: tests/pass/tree_borrows/retag_no_race.rs, its Tree Borrows part: the threads' accesses
// and retags in the order the deterministic schedule runs them (`retag_no_race.stderr`). That
// the retag of `y` is no write for the data race detector is `crate::race`'s; the barriers and
// `Arc`s are other allocations.
#[test]
fn retag_no_race() -> R {
    let mut s = Sim::new();
    // let mut data = 0u8;
    // let p = SendPtr(addr_of_mut!(data));
    let data = s.alloc(1);
    let p = data;
    // thread 1: let x = unsafe { &mut *x.0 };
    let x = s.mutable(p, 1)?;
    // thread 2: let y = unsafe { &mut *y.0 };
    let y = s.mutable(p, 1)?;
    // thread 1: let _v = *x;  ("read x || retag y")
    s.read(x, 1)?;
    // thread 2: write(&mut *y, 42, &barrier);
    let arg = s.mutable(y, 1)?;
    let arg = s.mutable(arg, 1)?;
    s.call();
    let y = s.arg_mut(arg, 1)?;
    // fn write(y: &mut u8, v: u8, barrier: &IdxBarrier) {
    //     synchronized!(barrier, "write y");
    //     *y = v;
    s.write(y, 1)?;
    // }
    s.ret()
}

// Miri: tests/pass/tree_borrows/rustc_no_writable.rs, under
// `-Zmiri-tree-borrows-implicit-writes`, with `dereference` `#[rustc_no_writable]`.
#[test]
fn rustc_no_writable() -> R {
    let mut s = Sim::new();
    // let mut x = 0u8;
    let x = s.alloc(1);
    // let ptr = &raw mut x;
    let ptr = x;
    // let res = dereference(&mut x, ptr);
    let r = s.mutable(x, 1)?;
    let res = dereference(&mut s, false, PtrKind::Mut, false, r, ptr)?;
    // assert_eq!(*res, 0);
    assert_eq_deref(&mut s, res, 1)
}

// Miri: tests/pass/tree_borrows/sb_fails.rs, fnentry_invalidation::main.
#[test]
fn sb_fails_fnentry_invalidation() -> R {
    let mut s = Sim::new();
    // let mut x = 0i32;
    let x = s.alloc(4);
    // let z = &mut x as *mut i32;
    let z = s.mutable(x, 4)?;
    // x.do_bad();
    let receiver = s.mutable(x, 4)?;
    s.call();
    s.arg_mut(receiver, 4)?;
    s.ret()?;
    // let _oof = *z;
    s.read(z, 4)
}

// Miri: tests/pass/tree_borrows/sb_fails.rs, pass_invalid_mut::main.
#[test]
fn sb_fails_pass_invalid_mut() -> R {
    let mut s = Sim::new();
    // let x = &mut 42;
    let tmp = s.alloc(4);
    let x = s.mutable(tmp, 4)?;
    // let xraw = x as *mut _;
    let xraw = x;
    // let xref = unsafe { &mut *xraw };
    let xref = s.mutable(xraw, 4)?;
    // let _val = unsafe { *xraw }; // In SB this invalidates xref...
    s.read(xraw, 4)?;
    // foo(xref); // ...which then cannot be reborrowed here.
    let arg = s.mutable(xref, 4)?;
    s.call();
    s.arg_mut(arg, 4)?;
    s.ret()
}

// Miri: tests/pass/tree_borrows/sb_fails.rs, return_invalid_mut::main. `(i32, i32)`: `.0` at
// 0..4, `.1` at 4..8.
#[test]
fn sb_fails_return_invalid_mut() -> R {
    let mut s = Sim::new();
    // foo(&mut (1, 2));
    let tmp = s.alloc(8);
    let arg = s.mutable(tmp, 8)?;
    let arg = s.mutable(arg, 8)?;
    s.call();
    // fn foo(x: &mut (i32, i32)) -> &mut i32 {
    let x = s.arg_mut(arg, 8)?;
    //     let xraw = x as *mut (i32, i32);
    let xraw = x;
    //     let ret = unsafe { &mut (*xraw).1 };
    let ret = s.mutable(xraw.add(4), 4)?;
    //     let _val = unsafe { *xraw }; // In SB this invalidates ret...
    s.read(xraw, 8)?;
    //     ret // ...which then cannot be reborrowed here.
    let ret = s.mutable(ret, 4)?;
    // }
    s.ret()?;
    s.mutable(ret, 4)?;
    Ok(())
}

// Miri: tests/pass/tree_borrows/sb_fails.rs, static_memory_modification::main. `X: usize`: 8
// bytes.
#[test]
fn sb_fails_static_memory_modification() -> R {
    let mut s = Sim::new();
    // static X: usize = 5;
    let static_x = s.alloc(8);
    // let x = unsafe { std::mem::transmute::<&usize, &mut usize>(&X) };
    // The transmuted value is written to `x` at its new type: a Default retag as `&mut usize`.
    let shared = s.shared(static_x, 8)?;
    let x = s.mutable(shared, 8)?;
    // assert_eq!(*&*x, 5);
    let r = s.shared(x, 8)?;
    assert_eq_deref(&mut s, r, 8)
}

// Miri: tests/pass/tree_borrows/sb_fails.rs, interior_mut_reborrow.
#[test]
fn sb_fails_interior_mut_reborrow() -> R {
    let mut s = Sim::new();
    // let mut c = UnsafeCell::new(42);
    let c = s.alloc(4);
    // let ptr = c.get(); // first create interior mutable ptr
    let ptr = unsafe_cell_get(&mut s, c, 4)?;
    // c = UnsafeCell::new(13); // then write to parent
    s.write(c, 4)?;
    // assert_eq!(unsafe { ptr.read() }, 13); // then read through previous ptr
    s.read(ptr, 4)
}

// Miri: tests/pass/tree_borrows/slice_get_mut_no_implicit_write.rs, borrowed_buf, under
// `-Zmiri-tree-borrows-implicit-writes`: `<[T]>::get_unchecked_mut` and `as_mut_ptr` are
// `#[rustc_no_writable]`. The `&mut self` and `&self` of `BorrowedBuf`'s methods point to `buf`,
// another allocation.
#[test]
fn slice_get_mut_no_implicit_write_borrowed_buf() -> R {
    let mut s = Sim::new();
    // let mut arr = [0u8; 4];
    let arr = s.alloc(4);
    // let mut buf = BorrowedBuf { buf: &mut arr };
    // `&mut arr`, and its copy into the field.
    let field = s.mutable(arr, 4)?;
    let field = s.mutable(field, 4)?;
    // let ptr = unsafe { buf.as_mut() }.as_mut_ptr();
    s.call();
    // unsafe fn as_mut(&mut self) -> &mut [u8] {
    //     unsafe { self.buf.get_unchecked_mut(..) }
    let receiver = s.mutable(field, 4)?;
    s.call();
    let this = entry_mut(&mut s, false, receiver, 4)?;
    // `unsafe { &mut *index.get_unchecked_mut(self) }`: `RangeFull`'s takes and returns the raw
    // `*mut [T]`.
    let out = s.mutable(this, 4)?;
    s.ret()?;
    let out = s.mutable(out, 4)?;
    let out = s.mutable(out, 4)?;
    // }
    s.ret()?;
    let out = s.mutable(out, 4)?;
    let ptr = slice_as_mut_ptr(&mut s, out, 4)?;
    // let _ = buf.capacity();
    // `self.buf.len()`: the receiver `&*self.buf` and its argument retag.
    let receiver = s.shared(field, 4)?;
    s.call();
    s.arg_shared(receiver, 4)?;
    s.ret()?;
    // unsafe { ptr.write(42) };
    s.write(ptr, 1)
}

// Miri: tests/pass/tree_borrows/slice_get_mut_no_implicit_write.rs, index_mut, under
// `-Zmiri-tree-borrows-implicit-writes`: `IndexMut::index_mut` for `[T]`, `RangeFull`'s
// `SliceIndex::index_mut` and `as_mut_ptr` are `#[rustc_no_writable]`; `dostuff` is not.
#[test]
fn slice_get_mut_no_implicit_write_index_mut() -> R {
    let mut s = Sim::new();
    // dostuff(&mut [1, 2, 3, 4]);
    let tmp = s.alloc(4);
    let arg = s.mutable(tmp, 4)?;
    let arg = s.mutable(arg, 4)?;
    s.call();
    // fn dostuff(x: &mut [u8]) {
    let x = entry_mut(&mut s, true, arg, 4)?;
    //     let ptr = (&mut x[..]).as_mut_ptr();
    // `&mut *IndexMut::index_mut(&mut *x, ..)`, which is `index.index_mut(self)`, which returns
    // `slice`.
    let receiver = s.mutable(x, 4)?;
    s.call();
    let this = entry_mut(&mut s, false, receiver, 4)?;
    let slice = s.mutable(this, 4)?;
    s.call();
    let slice = entry_mut(&mut s, false, slice, 4)?;
    let out = s.mutable(slice, 4)?;
    s.ret()?;
    let out = s.mutable(out, 4)?;
    let out = s.mutable(out, 4)?;
    s.ret()?;
    let out = s.mutable(out, 4)?;
    let out = s.mutable(out, 4)?;
    let ptr = slice_as_mut_ptr(&mut s, out, 4)?;
    //     let _len = x.len();
    let receiver = s.shared(x, 4)?;
    s.call();
    s.arg_shared(receiver, 4)?;
    s.ret()?;
    //     ptr.write(99);
    s.write(ptr, 1)?;
    // }
    s.ret()
}

// Miri: tests/pass/tree_borrows/spurious_read.rs, retagx_retagy_spuriousx_retx_rety_writey:
// the threads' steps in the order of `spurious_read.stderr` (thread x is 1, thread y is 2).
#[test]
fn spurious_read_retagx_retagy_spuriousx_retx_rety_writey() -> R {
    let mut s = Sim::new();
    // let mut data = 0u8;
    // let ptr = SendPtr(std::ptr::addr_of_mut!(data));
    let data = s.alloc(1);
    let ptr = data;
    // thread x: let _x = as_mut(unsafe { &mut *ptr.0 }, b.clone());  ("retag x")
    s.thread(1);
    let arg = s.mutable(ptr, 1)?;
    let arg = s.mutable(arg, 1)?;
    s.call();
    let x = s.arg_mut(arg, 1)?;
    // thread y: let y = as_mut(unsafe { &mut *ptr.0 }, b.clone());  ("retag y")
    s.thread(2);
    let arg = s.mutable(ptr, 1)?;
    let arg = s.mutable(arg, 1)?;
    s.call();
    let y = s.arg_mut(arg, 1)?;
    // thread x: let _v = *x;  ("spurious read x")
    s.thread(1);
    s.read(x, 1)?;
    // thread x: let x = x as *mut u8; x  ("ret x")
    s.ret()?;
    // thread y: let y = y as *mut u8; y  ("ret y")
    s.thread(2);
    s.ret()?;
    // thread y: unsafe { *y = 2; }  ("write y")
    s.write(y, 1)
}

// Miri: tests/pass/tree_borrows/transmute-unsafecell.rs, ref_to_cell. No `&mut` or `Box`
// argument, so both revisions do the same.
fn transmute_unsafecell_ref_to_cell(_implicit_writes: bool) -> R {
    let mut s = Sim::new();
    // let x = &42i32;
    // A promoted constant: its own allocation.
    let promoted = s.alloc(4);
    let x = s.shared(promoted, 4)?;
    // let cell_x: &UnsafeCell<i32> = mem::transmute(x);
    // The transmuted value is written to `cell_x` at its new type: a Default retag.
    let cell_x = s.shared_cell(x, 4, &all(4))?;
    // let val = *cell_x.get();
    let p = unsafe_cell_get(&mut s, cell_x, 4)?;
    s.read(p, 4)
}

// Miri: tests/pass/tree_borrows/transmute-unsafecell.rs, cell_to_ref.
fn transmute_unsafecell_cell_to_ref(_implicit_writes: bool) -> R {
    let mut s = Sim::new();
    // let x = &UnsafeCell::new(42);
    let tmp = s.alloc(4);
    let x = s.shared_cell(tmp, 4, &all(4))?;
    // let ref_x: &i32 = mem::transmute(x);
    let ref_x = s.shared(x, 4)?;
    // let val = *ref_x;
    s.read(ref_x, 4)
}

// Miri: tests/pass/tree_borrows/tree-borrows.rs, local_addr_of_mut.
fn tree_borrows_local_addr_of_mut(_implicit_writes: bool) -> R {
    let mut s = Sim::new();
    // let mut local = 0;
    let local = s.alloc(4);
    // let ptr = ptr::addr_of_mut!(local);
    let ptr = local;
    // local = 1;
    s.write(local, 4)?;
    // unsafe { *ptr = 2 };
    s.write(ptr, 4)?;
    // local = 3;
    s.write(local, 4)?;
    // unsafe { *ptr = 4 };
    s.write(ptr, 4)
}

// Miri: tests/pass/tree_borrows/tree-borrows.rs, aliasing_read_only_mutable_refs.
fn tree_borrows_aliasing_read_only_mutable_refs(_implicit_writes: bool) -> R {
    let mut s = Sim::new();
    // let base = &mut 42u64;
    let tmp = s.alloc(8);
    let base = s.mutable(tmp, 8)?;
    // let r1 = &mut *(base as *mut u64);
    let r1 = s.mutable(base, 8)?;
    // let r2 = &mut *(base as *mut u64);
    let r2 = s.mutable(base, 8)?;
    // let _l = *r1;
    s.read(r1, 8)?;
    // let _l = *r2;
    s.read(r2, 8)
}

// Miri: tests/pass/tree_borrows/tree-borrows.rs, string_as_mut_ptr: the `String`'s buffer.
// The `String` itself (pointer, capacity, length) is another allocation, and holds the buffer
// as a `NonNull`, which is not retagged.
fn tree_borrows_string_as_mut_ptr(implicit_writes: bool) -> R {
    let mut s = Sim::new();
    // let mut s = String::from("hello");
    let old = s.alloc(5);
    s.write(old, 5)?;
    // s.reserve(1);
    // `RawVec` grows to twice the capacity, 10: `realloc` copies the 5 bytes into a new block
    // and frees the old one, through the raw pointers.
    let buf = s.alloc(10);
    s.read(old, 5)?;
    s.write(buf, 5)?;
    s.free(old, 5)?;
    // let mut s = mem::ManuallyDrop::new(s);
    // let ptr = s.as_mut_ptr();
    // `str::as_mut_ptr` through `DerefMut`: `String::deref_mut` is `self.as_mut_str()`, which is
    // `str::from_utf8_unchecked_mut(self.vec.as_mut_slice())`, and `Vec::as_mut_slice` is
    // `slice::from_raw_parts_mut(self.as_mut_ptr(), self.len)`, which makes a `&mut [u8]` to
    // the string's 5 bytes from the buffer's raw pointer.
    let v = s.mutable(buf, 5)?;
    // returned from `from_raw_parts_mut`, from `as_mut_slice`, and the argument's reborrow
    let v = s.mutable(v, 5)?;
    let v = s.mutable(v, 5)?;
    let v = s.mutable(v, 5)?;
    s.call();
    let v = entry_mut(&mut s, implicit_writes, v, 5)?;
    // `unsafe { &mut *(v as *mut [u8] as *mut str) }`
    let st = s.mutable(v, 5)?;
    s.ret()?;
    // returned from `from_utf8_unchecked_mut`, from `as_mut_str`, from `deref_mut`, and the
    // receiver's reborrow
    let st = s.mutable(st, 5)?;
    let st = s.mutable(st, 5)?;
    let st = s.mutable(st, 5)?;
    let st = s.mutable(st, 5)?;
    // `str::as_mut_ptr(&mut self)` (`#[rustc_no_writable]`): `self as *mut str as *mut u8`.
    s.call();
    let ptr = entry_mut(&mut s, false, st, 5)?;
    s.ret()?;
    // let len = s.len();
    // let capacity = s.capacity();
    // let s = String::from_raw_parts(ptr, len, capacity);
    // assert_eq!(String::from("hello"), s);
    // `String == String` compares `self[..]` and `other[..]`: a `&str` to each buffer, read.
    let r = s.shared(ptr, 5)?;
    s.read(r, 5)?;
    // `s` is dropped, freeing all 10 bytes through `ptr`.
    s.free(ptr, 10)
}

// Miri: tests/pass/tree_borrows/tree-borrows.rs, two_mut_protected_same_alloc. `(u8, u8)`:
// `.0` at 0, `.1` at 1.
fn tree_borrows_two_mut_protected_same_alloc(implicit_writes: bool) -> R {
    let mut s = Sim::new();
    // let mut data = (0u8, 1u8);
    let data = s.alloc(2);
    // write_second(&mut data.0, &mut data.1);
    let a = s.mutable(data, 1)?;
    let a = s.mutable(a, 1)?;
    let b = s.mutable(data.add(1), 1)?;
    let b = s.mutable(b, 1)?;
    s.call();
    // fn write_second(_x: &mut u8, y: &mut u8) {
    entry_mut(&mut s, implicit_writes, a, 1)?;
    let y = entry_mut(&mut s, implicit_writes, b, 1)?;
    //     *y = 1;
    s.write(y, 1)?;
    // }
    s.ret()
}

// Miri: tests/pass/tree_borrows/tree-borrows.rs, returned_mut_is_usable.
fn tree_borrows_returned_mut_is_usable(implicit_writes: bool) -> R {
    let mut s = Sim::new();
    // let mut data = 0;  (a `u8`, from `reborrow`'s signature)
    let data = s.alloc(1);
    // let x = &mut data;
    let x = s.mutable(data, 1)?;
    // let y = reborrow(x);
    let arg = s.mutable(x, 1)?;
    s.call();
    // fn reborrow(x: &mut u8) -> &mut u8 {
    let x = entry_mut(&mut s, implicit_writes, arg, 1)?;
    //     let y = &mut *x;
    let y = s.mutable(x, 1)?;
    //     *y = *y;
    s.read(y, 1)?;
    s.write(y, 1)?;
    //     y
    let ret = s.mutable(y, 1)?;
    //     // An implicit read through `x` is inserted here.
    // }
    s.ret()?;
    let y = s.mutable(ret, 1)?;
    // *y = 1;
    s.write(y, 1)
}

// Miri: tests/pass/tree_borrows/tree-borrows.rs, direct_mut_to_const_raw.
fn tree_borrows_direct_mut_to_const_raw(_implicit_writes: bool) -> R {
    let mut s = Sim::new();
    // let x = &mut 0;
    let tmp = s.alloc(4);
    let x = s.mutable(tmp, 4)?;
    // let y: *const i32 = x;
    let y = x;
    // unsafe { *(y as *mut i32) = 1; }
    s.write(y, 4)?;
    // assert_eq!(*x, 1);
    assert_eq_deref(&mut s, x, 4)
}

/// `fn access_nexts(x: *mut u8, y: *mut u8) -> u8` of `tree-borrows.rs`.
fn access_nexts(s: &mut Sim, x: Ptr, y: Ptr) -> R {
    for i in 0..5 {
        // sum += unsafe { *x.add(2 * i) };
        s.read(x.add(2 * i), 1)?;
        // sum += unsafe { *y.add(2 * i + 1) };
        s.read(y.add(2 * i + 1), 1)?;
    }
    Ok(())
}

// Miri: tests/pass/tree_borrows/tree-borrows.rs, array_pointer_access. `[u8; 11]`.
fn tree_borrows_array_pointer_access(implicit_writes: bool) -> R {
    let mut s = Sim::new();
    // let mut x = [1u8; 11];
    let x = s.alloc(11);
    // let y = (&raw mut x).cast();
    let y = x;
    // let res = foo(&mut x[0], y);
    let arg = s.mutable(x, 1)?;
    let arg = s.mutable(arg, 1)?;
    s.call();
    // fn foo(x: &mut u8, y: *mut u8) -> u8 {
    let x = entry_mut(&mut s, implicit_writes, arg, 1)?;
    //     let x: *mut u8 = x;
    //     let res1 = access_nexts(x.add(1), y.add(1));
    access_nexts(&mut s, x.add(1), y.add(1))?;
    //     let res2 = access_nexts(y.add(1), x.add(1));
    access_nexts(&mut s, y.add(1), x.add(1))?;
    //     res1 + res2
    // }
    s.ret()
}

// Miri: tests/pass/tree_borrows/wildcard/formatting.rs.
#[test]
fn wildcard_formatting() -> R {
    let mut s = Sim::new();
    // let x = &0u8;
    let promoted = s.alloc(1);
    let x = s.shared(promoted, 1)?;
    s.name(x, "x");
    // let xa = &*x;
    let xa = s.shared(x, 1)?;
    s.name(xa, "xa");
    // let xb = &*x;
    let xb = s.shared(x, 1)?;
    s.name(xb, "xb");
    // let wild = xb as *const u8 as usize as *const u8;
    s.expose(xb);
    let wild = xb.wildcard();
    // let y = &*wild;
    let y = s.shared(wild, 1)?;
    s.name(y, "y");
    // let ya = &*y;
    let ya = s.shared(y, 1)?;
    s.name(ya, "ya");
    // let yb = &*y;
    let yb = s.shared(y, 1)?;
    s.name(yb, "yb");
    // let _int = ya as *const u8 as usize;
    s.expose(ya);
    // let z = &*wild;
    let z = s.shared(wild, 1)?;
    s.name(z, "z");
    // let u = &*wild;
    let u = s.shared(wild, 1)?;
    s.name(u, "u");
    // let ua = &*u;
    let ua = s.shared(u, 1)?;
    s.name(ua, "ua");
    // let alloc_id = alloc_id!(x);
    // print_state!(alloc_id);
    assert_tree(
        &s,
        x,
        false,
        r"
──────────────────────────────────────────────────
Warning: this tree is indicative only. Some tags may have been hidden.
0..   1
| Unq |    └─┬──<TAG=root of the allocation>
| Frz |      └─┬──<TAG=x>
| Frz |        ├────<TAG=xa>
| Frz |        └────<TAG=xb> (exposed)
|     |
| Frz |    *─┬──<TAG=y>
| Frz |      ├────<TAG=ya> (exposed)
| Frz |      └────<TAG=yb>
|     |
| Frz |    *────<TAG=z>
|     |
| Frz |    *─┬──<TAG=u>
| Frz |      └────<TAG=ua>
──────────────────────────────────────────────────
",
    );
    Ok(())
}

// Miri: tests/pass/tree_borrows/wildcard/wildcard.rs, multiple_exposed_siblings.
#[test]
fn wildcard_multiple_exposed_siblings() -> R {
    let mut s = Sim::new();
    // let mut x: u32 = 42;
    let x = s.alloc(4);
    // let ptr_base = &mut x as *mut u32;
    let ptr_base = s.mutable(x, 4)?;
    // let ref1 = unsafe { &mut *ptr_base };
    let ref1 = s.mutable(ptr_base, 4)?;
    // let ref2 = unsafe { &mut *ptr_base };
    let ref2 = s.mutable(ptr_base, 4)?;
    // let int1 = ref1 as *mut u32 as usize;
    s.expose(ref1);
    // let _int2 = ref2 as *mut u32 as usize;
    s.expose(ref2);
    // let wild = int1 as *mut u32;
    let wild = ref1.wildcard();
    // unsafe { wild.write(13) };
    s.write(wild, 4)?;
    // assert_eq!(*ref2, 13);
    assert_eq_deref(&mut s, ref2, 4)
}

// Miri: tests/pass/tree_borrows/wildcard/wildcard.rs, multiple_exposed_child.
#[test]
fn wildcard_multiple_exposed_child() -> R {
    let mut s = Sim::new();
    // let mut x: u32 = 42;
    let x = s.alloc(4);
    // let ref1 = &mut x;
    let ref1 = s.mutable(x, 4)?;
    // let int1 = ref1 as *mut u32 as usize;
    s.expose(ref1);
    // let ref2 = &mut *ref1;
    let ref2 = s.mutable(ref1, 4)?;
    // let ref3 = &mut *ref2;
    let ref3 = s.mutable(ref2, 4)?;
    // let _int3 = ref3 as *mut u32 as usize;
    s.expose(ref3);
    // let wild = int1 as *mut u32;
    let wild = ref1.wildcard();
    // unsafe { wild.write(42) };
    s.write(wild, 4)?;
    // let _x = *ref2;
    s.read(ref2, 4)?;
    // unsafe { wild.write(43) };
    s.write(wild, 4)
}

// Miri: tests/pass/tree_borrows/wildcard/wildcard.rs, dealloc.
#[test]
fn wildcard_dealloc() -> R {
    let mut s = Sim::new();
    // let x = unsafe { std::alloc::alloc_zeroed(Layout::new::<u32>()) as *mut u32 };
    let x = s.alloc(4);
    s.write(x, 4)?;
    // let ref1 = unsafe { &mut *x };
    let ref1 = s.mutable(x, 4)?;
    // let int = ref1 as *mut u32 as usize;
    s.expose(ref1);
    // let wild = int as *mut u32;
    let wild = ref1.wildcard();
    // unsafe { std::alloc::dealloc(wild as *mut u8, Layout::new::<u32>()) };
    s.free(wild, 4)
}

// Miri: tests/pass/tree_borrows/wildcard/wildcard.rs, protector.
#[test]
fn wildcard_protector() -> R {
    let mut s = Sim::new();
    // let mut x: u32 = 32;
    let x = s.alloc(4);
    // let ref1 = &mut x;
    let ref1 = s.mutable(x, 4)?;
    // let int = ref1 as *mut u32 as usize;
    s.expose(ref1);
    // let wild = int as *mut u32;
    let wild = ref1.wildcard();
    // let wild_ref = unsafe { &mut *wild };
    let wild_ref = s.mutable(wild, 4)?;
    // protect(wild_ref);
    let arg = s.mutable(wild_ref, 4)?;
    s.call();
    // fn protect(arg: &mut u32) {
    let arg = s.arg_mut(arg, 4)?;
    //     *arg = 4;
    s.write(arg, 4)?;
    // }
    s.ret()?;
    // assert_eq!(*ref1, 4);
    assert_eq_deref(&mut s, ref1, 4)
}

// Miri: tests/pass/tree_borrows/wildcard/wildcard.rs, protector_conflicted_release.
#[test]
fn wildcard_protector_conflicted_release() -> R {
    let mut s = Sim::new();
    // let mut x: u32 = 42;
    let x = s.alloc(4);
    // let ptr_base = &mut x as *mut u32;
    let ptr_base = s.mutable(x, 4)?;
    // let ref1 = unsafe { &mut *ptr_base };
    let ref1 = s.mutable(ptr_base, 4)?;
    // let ref2 = unsafe { &mut *ptr_base };
    let ref2 = s.mutable(ptr_base, 4)?;
    // let protect = |arg: &mut u32| { ... };
    // The closure captures `*ref2` by shared reference: `&*ref2`, copied into the closure.
    let captured = s.shared(ref2, 4)?;
    let captured = s.shared(captured, 4)?;
    // let wild = protect(ref1);
    let arg = s.mutable(ref1, 4)?;
    s.call();
    let arg = s.arg_mut(arg, 4)?;
    //     let int = arg as *mut u32 as usize;
    s.expose(arg);
    //     let wild = int as *mut u32;
    let wild = arg.wildcard();
    //     let _x = *ref2;
    s.read(captured, 4)?;
    //     return wild;
    s.ret()?;
    // unsafe { *wild = 4 };
    s.write(wild, 4)
}

// Miri: tests/pass/tree_borrows/wildcard/wildcard.rs, returned_mut_is_usable.
#[test]
fn wildcard_returned_mut_is_usable() -> R {
    let mut s = Sim::new();
    // let mut x: u8 = 0;
    let x = s.alloc(1);
    // let ref1 = &mut x;
    let ref1 = s.mutable(x, 1)?;
    // let int = ref1 as *mut u8 as usize;
    s.expose(ref1);
    // let wild = int as *mut u8;
    let wild = ref1.wildcard();
    // let wild_ref = unsafe { &mut *wild };
    let wild_ref = s.mutable(wild, 1)?;
    // let y = reborrow(wild_ref);
    let arg = s.mutable(wild_ref, 1)?;
    s.call();
    // fn reborrow(x: &mut u8) -> &mut u8 {
    let x = s.arg_mut(arg, 1)?;
    //     let y = &mut *x;
    let y = s.mutable(x, 1)?;
    //     *y = *y;
    s.read(y, 1)?;
    s.write(y, 1)?;
    //     y
    let ret = s.mutable(y, 1)?;
    // }
    s.ret()?;
    let y = s.mutable(ret, 1)?;
    // *y = 1;
    s.write(y, 1)
}

// Miri: tests/pass/tree_borrows/wildcard/reborrow.rs, multiple_exposed_siblings1. No `&mut` or
// `Box` argument, so both revisions do the same.
fn wildcard_reborrow_multiple_exposed_siblings1(_implicit_writes: bool) -> R {
    let mut s = Sim::new();
    // let mut x: u32 = 42;
    let x = s.alloc(4);
    // let ptr_base = &mut x as *mut u32;
    let ptr_base = s.mutable(x, 4)?;
    // let ref1 = unsafe { &mut *ptr_base };
    let ref1 = s.mutable(ptr_base, 4)?;
    // let int1 = ref1 as *mut u32 as usize;
    s.expose(ref1);
    // let ref2 = unsafe { &mut *ptr_base };
    let ref2 = s.mutable(ptr_base, 4)?;
    // let _int2 = ref2 as *mut u32 as usize;
    s.expose(ref2);
    // let wild = int1 as *mut u32;
    let wild = ref1.wildcard();
    // let reb = unsafe { &mut *wild };
    let reb = s.mutable(wild, 4)?;
    // *reb = 13;
    s.write(reb, 4)?;
    // assert_eq!(*ref2, 13);
    assert_eq_deref(&mut s, ref2, 4)?;
    // assert_eq!(*ref1, 13);
    assert_eq_deref(&mut s, ref1, 4)
}

// Miri: tests/pass/tree_borrows/wildcard/reborrow.rs, multiple_exposed_siblings2.
fn wildcard_reborrow_multiple_exposed_siblings2(_implicit_writes: bool) -> R {
    let mut s = Sim::new();
    // let mut x: u32 = 42;
    let x = s.alloc(4);
    // let ptr_base = &mut x as *mut u32;
    let ptr_base = s.mutable(x, 4)?;
    // let int = ptr_base as usize;
    s.expose(ptr_base);
    // let wild = int as *mut u32;
    let wild = ptr_base.wildcard();
    // let reb_ptr = unsafe { &mut *wild } as *mut u32;
    let reb_ptr = s.mutable(wild, 4)?;
    // let ref1 = unsafe { &mut *reb_ptr };
    let ref1 = s.mutable(reb_ptr, 4)?;
    // let _int1 = ref1 as *mut u32 as usize;
    s.expose(ref1);
    // let ref2 = unsafe { &mut *reb_ptr };
    let ref2 = s.mutable(reb_ptr, 4)?;
    // let _int2 = ref2 as *mut u32 as usize;
    s.expose(ref2);
    // unsafe { wild.write(13) };
    s.write(wild, 4)?;
    // assert_eq!(*ref2, 13);
    assert_eq_deref(&mut s, ref2, 4)?;
    // assert_eq!(*ref1, 13);
    assert_eq_deref(&mut s, ref1, 4)
}

// Miri: tests/pass/tree_borrows/wildcard/reborrow.rs, reborrow3.
fn wildcard_reborrow_reborrow3(_implicit_writes: bool) -> R {
    let mut s = Sim::new();
    // let mut x: u32 = 42;
    let x = s.alloc(4);
    // let ptr_base = &mut x as *mut u32;
    let ptr_base = s.mutable(x, 4)?;
    // let int = ptr_base as usize;
    s.expose(ptr_base);
    // let wild = int as *mut u32;
    let wild = ptr_base.wildcard();
    // let reb1 = unsafe { &mut *wild };
    let reb1 = s.mutable(wild, 4)?;
    // let ref2 = &mut *reb1;
    let ref2 = s.mutable(reb1, 4)?;
    // let _int = ref2 as *mut u32 as usize;
    s.expose(ref2);
    // let reb3 = unsafe { &mut *wild };
    let reb3 = s.mutable(wild, 4)?;
    // *reb3 = 1;
    s.write(reb3, 4)?;
    // *ref2 = 2;
    s.write(ref2, 4)?;
    // *reb1 = 3;
    s.write(reb1, 4)
}

// Miri: tests/pass/tree_borrows/wildcard/reborrow.rs, returned_mut_is_usable.
fn wildcard_reborrow_returned_mut_is_usable(implicit_writes: bool) -> R {
    let mut s = Sim::new();
    // let mut x: u32 = 32;
    let x = s.alloc(4);
    // let ref1 = &mut x;
    let ref1 = s.mutable(x, 4)?;
    // let y = protect(ref1);
    let arg = s.mutable(ref1, 4)?;
    s.call();
    // fn protect(arg: &mut u32) -> &mut u32 {
    let arg = entry_mut(&mut s, implicit_writes, arg, 4)?;
    //     let int = arg as *mut u32 as usize;
    s.expose(arg);
    //     let wild = int as *mut u32;
    let wild = arg.wildcard();
    //     let ref2 = unsafe { &mut *wild };
    let ref2 = s.mutable(wild, 4)?;
    //     *ref2 = 42;
    s.write(ref2, 4)?;
    //     ref2
    let ret = s.mutable(ref2, 4)?;
    //     // An implicit read through `arg` is inserted here.
    // }
    s.ret()?;
    let y = s.mutable(ret, 4)?;
    // *y = 4;
    s.write(y, 4)
}

// Miri: tests/pass/tree_borrows/wildcard/reborrow.rs, only_foreign_is_temporary.
fn wildcard_reborrow_only_foreign_is_temporary(_implicit_writes: bool) -> R {
    let mut s = Sim::new();
    // let mut x = 0u32;
    let x = s.alloc(4);
    // let wild = &mut x as *mut u32 as usize as *mut u32;
    let r = s.mutable(x, 4)?;
    s.expose(r);
    let wild = r.wildcard();
    // let reb1 = unsafe { &mut *wild };
    let reb1 = s.mutable(wild, 4)?;
    // let reb2 = unsafe { &mut *wild };
    let reb2 = s.mutable(wild, 4)?;
    // let ref3 = &mut *reb1;
    let ref3 = s.mutable(reb1, 4)?;
    // let _int = ref3 as *mut u32 as usize;
    s.expose(ref3);
    // let reb4 = unsafe { &mut *wild };
    let reb4 = s.mutable(wild, 4)?;
    // let _x = *reb2;
    s.read(reb2, 4)?;
    // *reb4 = 32;
    s.write(reb4, 4)?;
    // *ref3 = 4;
    s.write(ref3, 4)
}

// Miri: tests/pass/tree_borrows/wildcard/undetected_ub.rs, uncertain_provenance. UB that Miri
// does not detect (the file says so): wildcard provenance does not pick the tag a wildcard
// write went through, so neither exposed reference is disabled. Ported as Miri runs it, passing.
#[test]
fn undetected_ub_uncertain_provenance() -> R {
    let mut s = Sim::new();
    // let mut x: u32 = 42;
    let x = s.alloc(4);
    // let ptr_base = &mut x as *mut u32;
    let ptr_base = s.mutable(x, 4)?;
    // let ref1 = unsafe { &mut *ptr_base };
    let ref1 = s.mutable(ptr_base, 4)?;
    // let ref2 = unsafe { &mut *ptr_base };
    let ref2 = s.mutable(ptr_base, 4)?;
    // let int1 = ref1 as *mut u32 as usize;
    s.expose(ref1);
    // let _int2 = ref2 as *mut u32 as usize;
    s.expose(ref2);
    // let wild = int1 as *mut u32;
    let wild = ref1.wildcard();
    // unsafe { wild.write(13) };
    s.write(wild, 4)?;
    // assert_eq!(*ref1, 13);
    assert_eq_deref(&mut s, ref1, 4)?;
    // assert_eq!(*ref2, 13);
    assert_eq_deref(&mut s, ref2, 4)
}

// Miri: tests/pass/tree_borrows/wildcard/undetected_ub.rs, protected_exposed. UB that Miri does
// not detect (the file says so): a wildcard write while `ref3` is protected must have gone
// through `ref3`, which would disable `ref2`, but wildcard accesses do not narrow their
// candidates by protectors. Ported as Miri runs it, passing.
#[test]
fn undetected_ub_protected_exposed() -> R {
    let mut s = Sim::new();
    // let mut x: u32 = 42;
    let x = s.alloc(4);
    // let ptr_base = &mut x as *mut u32;
    let ptr_base = s.mutable(x, 4)?;
    // let ref1 = unsafe { &mut *ptr_base };
    let ref1 = s.mutable(ptr_base, 4)?;
    // let ref2 = unsafe { &mut *ptr_base };
    let ref2 = s.mutable(ptr_base, 4)?;
    // let _int2 = ref2 as *mut u32 as usize;
    s.expose(ref2);
    // protect(ref1);
    let arg = s.mutable(ref1, 4)?;
    s.call();
    // fn protect(ref3: &mut u32) {
    let ref3 = s.arg_mut(arg, 4)?;
    //     let int3 = ref3 as *mut u32 as usize;
    s.expose(ref3);
    //     let wild = int3 as *mut u32;
    let wild = ref3.wildcard();
    //     unsafe { wild.write(13) }
    s.write(wild, 4)?;
    // }
    s.ret()?;
    // let _fail = *ref2;
    s.read(ref2, 4)
}

// Miri: tests/pass/tree_borrows/wildcard/undetected_ub.rs,
// cross_tree_update_older_invalid_exposed. UB that Miri does not detect (the file says so):
// the read through `reb2_ptr` should freeze `reb1`, since no access could have come from its
// child `ref3`, but `ref3` is newer than `reb2` and not disabled, so `reb1` is left alone.
// Ported as Miri runs it, passing. `[u32; 2]`: 8 bytes.
#[test]
fn undetected_ub_cross_tree_update_older_invalid_exposed() -> R {
    let mut s = Sim::new();
    // let mut x: [u32; 2] = [42, 43];
    let x = s.alloc(8);
    // let ref_base = &mut x;
    let ref_base = s.mutable(x, 8)?;
    // let int0 = ref_base as *mut u32 as usize;
    s.expose(ref_base);
    // let wild = int0 as *mut u32;
    let wild = ref_base.wildcard();
    // let reb1 = unsafe { &mut *wild };
    let reb1 = s.mutable(wild, 4)?;
    // *reb1 = 44;
    s.write(reb1, 4)?;
    // let reb2 = unsafe { &mut *wild.wrapping_add(1) };
    let reb2 = s.mutable(wild.add(4), 4)?;
    // let reb2_ptr = (reb2 as *mut u32).wrapping_sub(1);
    let reb2_ptr = reb2.sub(4);
    // let ref3 = &mut *reb1;
    let ref3 = s.mutable(reb1, 4)?;
    // let _int3 = ref3 as *mut u32 as usize;
    s.expose(ref3);
    // let _y = unsafe { *reb2_ptr };
    s.read(reb2_ptr, 4)?;
    // *reb1 = 4;
    s.write(reb1, 4)
}

revisions! {
    cell_alternate_writes,
    cell_inside_box,
    cell_inside_struct,
    cell_lazy_write_to_surrounding,
    copy_nonoverlapping_test_to_from,
    copy_nonoverlapping_test_from_to,
    end_of_protector,
    reborrow_is_read,
    transmute_unsafecell_ref_to_cell,
    transmute_unsafecell_cell_to_ref,
    tree_borrows_local_addr_of_mut,
    tree_borrows_aliasing_read_only_mutable_refs,
    tree_borrows_string_as_mut_ptr,
    tree_borrows_two_mut_protected_same_alloc,
    tree_borrows_returned_mut_is_usable,
    tree_borrows_direct_mut_to_const_raw,
    tree_borrows_array_pointer_access,
    wildcard_reborrow_multiple_exposed_siblings1,
    wildcard_reborrow_multiple_exposed_siblings2,
    wildcard_reborrow_reborrow3,
    wildcard_reborrow_returned_mut_is_usable,
    wildcard_reborrow_only_foreign_is_temporary,
}
