// Own program x0-static-01 (front2): statics on the new front end (front2/statics.rs). The
// Reference, "Static items": "Static initializers are constant expressions evaluated at
// compile time", and a static "represents an allocation in the program that is initialized
// with the initializer expression. All references and raw pointers to the static refer to the
// same allocation."
//
// - `A`, an array static, read by a body at index 3: its initializer's bytes in the image.
// - `N`, a `static mut` starting at 5, incremented twice by `bump` through `unsafe`: writes
//   to the one allocation, read back as 7 (a copy per use would read 5).
// - `P`, a static holding `&A[1]`: its value is a relocation to `A`'s allocation plus 1, so
//   `*P` reads `A[1]`, 20 (the image's `DataTarget::Static`: `A` is reached both by a body's
//   path and by `P`'s record, and placed once).
//
// Status: N + *P + A[3] = 7 + 20 + 15 = 42. A wrong `N` (5: no write seen, or 6) gives 40 or
// 41; a relocation without its addend reads A[0] = 10, giving 32; an unrelocated word faults.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

// core's own impls (core/src/marker.rs `marker_impls!`): a primitive is `Copy` only by an impl.
impl Copy for u32 {}
impl Copy for u8 {}

#[lang = "sync"]
unsafe trait Sync {}

unsafe impl Sync for [u8; 4] {}
unsafe impl<'a> Sync for &'a u8 {}

#[lang = "panic_bounds_check"]
fn panic_bounds_check(index: usize, len: usize) -> ! {
    loop {}
}

static A: [u8; 4] = [10, 20, 30, 15];
static mut N: u32 = 5;
static P: &u8 = &A[1];

fn bump() {
    unsafe {
        N = N + 1;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    bump();
    bump();
    unsafe { (N + *P as u32 + A[3] as u32) as i32 }
}
