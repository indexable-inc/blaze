// Support program s1: layout (crates/blaze-build/src/layout.rs). Each check reads bytes at the
// offsets a layout rule fixes and exits with the check's number if they are not there; 42 when
// all hold.
// - repr(C): declaration order, each field at its alignment (Reference, "The C Representation"):
//   `C { a: u8, b: u32, c: u16 }` has `b` at 4 and `c` at 8.
// - repr(Rust): blaze's rule, decreasing alignment: `R { a: u8, b: u32, c: u16 }` has `b` at
//   0, `c` at 4, `a` at 6.
// - The null-pointer optimisation Rust guarantees (std's `option` doc, "Representation"):
//   `Option<&u32>` is one pointer, `None` is null, `Some(r)` is `r`'s address.
// - A slice reference is (address, length) (Reference, "Pointers and references layout").
#![feature(no_core, lang_items, intrinsics, rustc_attrs)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

// core's own impls (core/src/marker.rs `marker_impls!`): a primitive is `Copy` only by an impl.
impl Copy for usize {}
impl Copy for u8 {}
impl Copy for u16 {}
impl Copy for u32 {}

#[rustc_intrinsic]
unsafe fn transmute<T, U>(x: T) -> U;

// What an index past an array's end calls (nightly's MIR asserts every index); never returns.
#[lang = "panic_bounds_check"]
fn panic_bounds_check(index: usize, len: usize) -> ! {
    loop {}
}

#[repr(C)]
struct C {
    a: u8,
    b: u32,
    c: u16,
}

struct R {
    a: u8,
    b: u32,
    c: u16,
}

enum Option<T> {
    None,
    Some(T),
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    unsafe {
        let c = C { a: 1, b: 2, c: 3 };
        let p = &c as *const C as usize;
        if *(p as *const u8) != 1 {
            return 1;
        }
        if *((p + 4) as *const u32) != 2 {
            return 2;
        }
        if *((p + 8) as *const u16) != 3 {
            return 3;
        }
        let r = R { a: 7, b: 9, c: 11 };
        let q = &r as *const R as usize;
        if *(q as *const u32) != 9 {
            return 4;
        }
        if *((q + 4) as *const u16) != 11 {
            return 5;
        }
        if *((q + 6) as *const u8) != 7 {
            return 6;
        }
        let x: u32 = 5;
        let none: Option<&u32> = Option::None;
        let some: Option<&u32> = Option::Some(&x);
        let n: usize = transmute::<Option<&u32>, usize>(none);
        if n != 0 {
            return 7;
        }
        let s: usize = transmute::<Option<&u32>, usize>(some);
        if s != &x as *const u32 as usize {
            return 8;
        }
        let bytes: [u8; 3] = [4, 5, 6];
        let wide: &[u8] = &bytes;
        let parts: [usize; 2] = transmute::<&[u8], [usize; 2]>(wide);
        if parts[0] != &bytes as *const [u8; 3] as usize {
            return 9;
        }
        if parts[1] != 3 {
            return 10;
        }
    }
    42
}
