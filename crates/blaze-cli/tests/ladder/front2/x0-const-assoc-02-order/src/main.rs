// Own program x0-const-assoc-02-order (front2; blaze-ladder front2/const_at.rs): one trait
// constant read in one GENERIC body at two `Self` types, each instance its own value. The
// Reference, "Associated Items", "Associated constants": "An associated constant definition
// defines a constant associated with a type", and "Associated constant definitions undergo
// constant evaluation only when referenced. Further, definitions that include generic
// parameters are evaluated after monomorphization." The Reference, "Constant evaluation":
// "Constant evaluation is the process of computing the result of expressions during
// compilation." So `<T as W>::N` is 31 in `n::<u16>` and 11 in `n::<A>`, whatever order the
// instances are built in.
//
// `n::<u16>` is reached first, `n::<A>` second, then `n::<u16>` again through `twice::<u16>`.
// Status 42 when all hold. 2: the build's table of constants was keyed without the arguments
// (the first value filed, 31, read at `A`). 3: a value filed later replaced an earlier one, or
// the table was read by the order instances were built in (11 read at `u16`). 1: the value at
// the first instance is wrong.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for usize {}
impl Copy for i32 {}

trait W {
    const N: usize;
}

struct A;

impl W for u16 {
    const N: usize = 31;
}

impl W for A {
    const N: usize = 11;
}

fn n<T: W>() -> usize {
    <T as W>::N
}

fn twice<T: W>() -> usize {
    n::<T>()
}

fn check() -> i32 {
    match n::<u16>() {
        31 => {}
        _ => return 1,
    }
    match n::<A>() {
        11 => {}
        _ => return 2,
    }
    match twice::<u16>() {
        31 => {}
        _ => return 3,
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    match check() {
        0 => 42,
        failed => failed,
    }
}
