// Own program x0-const-assoc-01 (front2; walk list-AH class 12, briefs 8 and 9): a trait's
// constant read in a GENERIC body is the value at the instance's `Self`. The Reference,
// "Associated Items", "Associated Constants": "An associated constant definition defines a
// constant associated with a type"; its example "Using default values": an impl that writes no
// definition has the trait's default. So `<T as P>::S` at `T = u8` is the blanket impl's, i.e.
// the default's, 7; `<T as P2>::S` is 30 at `Q` and 5 at `u8`, each impl's own, never P2's
// default 1. (core's `size_of::<T>()` is `<T as SizedTypeProperties>::SIZE` through
// `impl<T> SizedTypeProperties for T {}`: the same form.)
//
// Status 42 when all three hold; 1, 2 or 3 names the first wrong value: a value taken from
// the wrong impl (5 at `Q`, 30 at `u8`), or the trait's default where an impl defines it (1),
// gives 2 or 3; a default not taken through the blanket impl gives 1.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for usize {}
impl Copy for i32 {}

trait P {
    const S: usize = 7;
}

impl<T> P for T {}

trait P2 {
    const S: usize = 1;
}

struct Q;

impl P2 for Q {
    const S: usize = 30;
}

impl P2 for u8 {
    const S: usize = 5;
}

fn s<T>() -> usize {
    <T as P>::S
}

fn s2<T: P2>() -> usize {
    <T as P2>::S
}

fn check() -> i32 {
    match s::<u8>() {
        7 => {}
        _ => return 1,
    }
    match s2::<Q>() {
        30 => {}
        _ => return 2,
    }
    match s2::<u8>() {
        5 => {}
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
