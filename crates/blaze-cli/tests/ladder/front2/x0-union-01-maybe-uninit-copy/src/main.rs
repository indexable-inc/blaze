//! x0-union-01: a `MaybeUninit`-shaped union, made uninitialised, returned, copied, then written
//! and read back through the copy (no_core; success 42, else the failed check's number). Written
//! by the wrong-answers lane (2026-11-16, WRONG-NOTES.md "Runner 6", Class M of hello-1116
//! stops-3-briefs.md): the real `blaze run` of hello world trapped (`brk #0xff02`) in a built
//! `MaybeUninit::<Later>::uninit`, because the builder copied a union field by field and
//! switched on an enum tag read from bytes nothing wrote.
//!
//! The Reference, "Unions": "Unions have no notion of an 'active field'. Instead, every union
//! access just interprets the storage as the type of the field used for the access"; core's
//! `MaybeUninit<T>` is `union { uninit: (), value: ManuallyDrop<T> }` and `uninit()` is
//! `MaybeUninit { uninit: () }`. So copying `MU { uninit: () }` reads no field: it is its bytes.
//! The twin's `MU<T: Copy>` stands in for `MaybeUninit<Option<Box<u8>>>` (no allocator in a
//! twin): `Opt<E>` is a tagged enum whose payload is itself a tagged enum, so a field-wise copy
//! switches twice on unwritten tags.
//! 1. `let m = mk(); let n = m;` (mk returns `MU { uninit: () }`) runs with no trap.
//! 2. writing `Opt::Some(E::B(40))` through `n`'s address, then reading it back, gives 40.
//! 3. a copy of the written `n` reads back the same.
//! 6. `Raw { raw: [!0; 3] }` copied reads back all ones: its `value` field's tag bytes are 0xFF,
//!    no tag of `Opt` or `E`. Checks 1 to 4 depend on what the stack held (a field-wise copy
//!    passed them when the unwritten bytes happened to hold tag 0); check 6 does not: with the
//!    builder's union arm planted out it is "CRASHED (signal 5)", the `brk #0xff02` trap.
#![feature(no_core, lang_items, intrinsics, rustc_attrs)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

enum E {
    A(u64),
    B(u32),
}

impl Clone for E {
    fn clone(&self) -> E {
        *self
    }
}
impl Copy for E {}

enum Opt<T> {
    None,
    Some(T),
}

impl<T: Copy> Clone for Opt<T> {
    fn clone(&self) -> Opt<T> {
        *self
    }
}
impl<T: Copy> Copy for Opt<T> {}

union MU<T: Copy> {
    uninit: (),
    value: T,
}

impl<T: Copy> Clone for MU<T> {
    fn clone(&self) -> MU<T> {
        *self
    }
}
impl<T: Copy> Copy for MU<T> {}

/// The deterministic form of the same class: `raw`'s bytes are all 0xFF, which is no tag of
/// `Opt<E>` or `E`; a copy that walked `value` switches on them and traps (`brk #0xff02`).
union Raw {
    raw: [u64; 3],
    value: Opt<E>,
}

impl Clone for Raw {
    fn clone(&self) -> Raw {
        *self
    }
}
impl Copy for Raw {}

fn raw_copy() -> i32 {
    let r = Raw { raw: [!0u64; 3] };
    let s = r;
    let back = unsafe { s.raw };
    if back[0] != !0u64 || back[1] != !0u64 || back[2] != !0u64 {
        return 6;
    }
    0
}

fn mk() -> MU<Opt<E>> {
    MU { uninit: () }
}

fn read(p: *const Opt<E>) -> i32 {
    match unsafe { *p } {
        Opt::Some(E::B(x)) => x as i32,
        Opt::Some(E::A(_)) => 101,
        Opt::None => 102,
    }
}

fn check() -> i32 {
    let m = mk();
    let mut n = m;
    let p = &mut n as *mut MU<Opt<E>> as *mut Opt<E>;
    unsafe {
        *p = Opt::Some(E::B(40));
    }
    if read(p) != 40 {
        return 2;
    }
    let c = n;
    if read(&c as *const MU<Opt<E>> as *const Opt<E>) != 40 {
        return 3;
    }
    if read(unsafe { &n.value }) != 40 {
        return 4;
    }
    raw_copy()
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
