//! x0-ref-12t: the runnable no_core twin of x0-ref-12-wide-ptr-eq (success 42, else the failed
//! check's number): `==` and `!=` on wide raw pointers compare the address AND the metadata.
//! `src/prelude.rs` is the twins' one prelude, byte-equal; `PartialEq for *const T` is declared
//! here as the pinned core writes it (library/core/src/ptr/const_ptr.rs: `impl<T: PointeeSized>
//! PartialEq for *const T { fn eq(&self, other: &*const T) -> bool { *self == *other } }`, the
//! body the walk's class sample was), with `ne` as `*self != *other`.
//!
//! The Reference ("Comparison operators") is silent on wide pointers; the pinned library states
//! it, core/src/ptr/mod.rs, `ptr::eq`: "This is the same as using the `==` operator" and "When
//! comparing wide pointers, both the address and the metadata are tested for equality."
//! a = [1, 2, 3]; p and r are `&a` as `*const [u8]` (len 3); q is the same address as a
//! `[u8; 2]` (len 2); b = [1, 2] elsewhere (len 2):
//! 1. `p != q` and not `p == q`: the lengths differ.
//! 2. `p == r` and not `p != r`.
//! 3. q and b's pointer (both len 2, other addresses) are `!=`.
//! 4. `*const dyn Tr` to `pp` and to `pp.x` (one address, `P`'s and `X`'s vtables) are `!=`.
//! 5. two `*const dyn Tr` to `pp.x` made apart (one vtable) are `==`.
//! 6. through a generic fn (`ptr::eq`'s form, an instance at `[u8]`): `ptr_eq(p, r)` and not
//!    `ptr_eq(p, q)`.
//! WITHOUT THE BUILDER CHANGE (walk.rs `binary`, walk/wide_cmp.rs) this twin does not build:
//! every check's `==`/`!=` is refused ("a binary operator on a non-scalar"). A builder comparing
//! addresses only exits 1 (or 4); one comparing lengths only exits 3.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

impl<T: PointeeSized> PartialEq for *const T {
    fn eq(&self, other: &*const T) -> bool {
        *self == *other
    }
    fn ne(&self, other: &*const T) -> bool {
        *self != *other
    }
}

trait Tr {
    fn v(&self) -> u8;
}

struct X(u8);

struct P {
    x: X,
}

impl Tr for X {
    fn v(&self) -> u8 {
        self.0
    }
}

impl Tr for P {
    fn v(&self) -> u8 {
        self.x.0 + 100
    }
}

fn ptr_eq<T: PointeeSized>(a: *const T, b: *const T) -> bool {
    a == b
}

fn check() -> i32 {
    let a = [1u8, 2, 3];
    let b = [1u8, 2];
    let whole_a: &[u8] = &a;
    let again_a: &[u8] = &a;
    // The first two elements of `a`, at `a`'s address: a `[u8; 3]` read as a `[u8; 2]`.
    let a2: &[u8; 2] = unsafe { &*(&a as *const [u8; 3] as *const [u8; 2]) };
    let head_a: &[u8] = a2;
    let other_b: &[u8] = &b;
    let p = whole_a as *const [u8];
    let r = again_a as *const [u8];
    let q = head_a as *const [u8];
    let o = other_b as *const [u8];
    let pp = P { x: X(7) };
    let whole: &dyn Tr = &pp;
    let inner: &dyn Tr = &pp.x;
    let inner_again: &dyn Tr = &pp.x;
    let dw = whole as *const dyn Tr;
    let di = inner as *const dyn Tr;
    let di2 = inner_again as *const dyn Tr;
    if !(p != q) || p == q {
        1
    } else if !(p == r) || p != r {
        2
    } else if !(q != o) || q == o {
        3
    } else if !(dw != di) || whole.v() != 107 || inner.v() != 7 {
        4
    } else if !(di == di2) || di != di2 {
        5
    } else if !ptr_eq(p, r) || ptr_eq(p, q) {
        6
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
