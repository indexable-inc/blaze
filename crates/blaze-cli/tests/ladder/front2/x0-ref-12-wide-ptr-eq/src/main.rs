//! x0-ref-12: `==` and `!=` on wide raw pointers compare the address AND the metadata.
//!
//! The Reference ("Comparison operators") is silent on wide pointers; the pinned library states
//! it, core/src/ptr/mod.rs, `ptr::eq`: "This is the same as using the `==` operator" and "When
//! comparing wide pointers, both the address and the metadata are tested for equality." Its
//! `impl<T: PointeeSized> PartialEq for *const T` is `*self == *other` (core/src/ptr/const_ptr.rs).
//! a = [1, 2, 3]; p = &a[..] and r = &a[..] (len 3), q = &a[..2] (same address, len 2):
//! 1. `p != q` and not `p == q`: the lengths differ.
//! 2. `p == r`, not `p != r`, and `p == (&a[..]) as *const [u8]`.
//! 3. `&a[1..]` and `&a[..2]` (both len 2, other addresses) are `!=`.
//! 4. `*const dyn Tr` to `pp` and to `pp.x` (one address, `P`'s and `X`'s vtables) are `!=`.
//! 5. two `*const dyn Tr` to `pp.x` made apart (one vtable) are `==`.
//! 6. through the library: `core::ptr::eq(p, r)` and not `core::ptr::eq(p, q)`.
//! A builder comparing addresses only fails 1 (or 4); one comparing lengths only fails 3.
//! Exit 42.

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

fn main() -> std::entry::Status {
    let a = [1u8, 2, 3];
    let p: *const [u8] = &a[..];
    let q: *const [u8] = &a[..2];
    let r: *const [u8] = &a[..];
    let head: *const [u8] = &a[..2];
    let tail: *const [u8] = &a[1..];
    let pp = P { x: X(7) };
    let whole: &dyn Tr = &pp;
    let inner: &dyn Tr = &pp.x;
    let inner_again: &dyn Tr = &pp.x;
    let dw = whole as *const dyn Tr;
    let di = inner as *const dyn Tr;
    let di2 = inner_again as *const dyn Tr;
    let status = if !(p != q) || p == q {
        1
    } else if !(p == r) || p != r || !(p == (&a[..]) as *const [u8]) {
        2
    } else if !(head != tail) {
        3
    } else if !(dw != di) || whole.v() != 107 || inner.v() != 7 {
        4
    } else if !(di == di2) {
        5
    } else if !core::ptr::eq(p, r) || core::ptr::eq(p, q) {
        6
    } else {
        42
    };
    std::entry::Status(status)
}
