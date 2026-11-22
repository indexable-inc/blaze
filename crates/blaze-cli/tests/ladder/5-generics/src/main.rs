// Rung 5: generics, monomorphized. A generic fn, a generic struct laid out per its argument
// (`Pair<u8>` and `Pair<i64>` differ), and a generic fn calling generic fns. Two instances of
// `pick` (i32; u8, asked from `main` and from `pick_second::<u8>`: one instance, not two), two
// of `second` (i64, u8), one of `pick_second`.
// The struct stays inside a function: aggregates across calls are rung 6.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

struct Pair<T> {
    a: T,
    b: T,
}

fn pick<T>(c: bool, a: T, b: T) -> T {
    if c { a } else { b }
}

fn second<T>(a: T, b: T) -> T {
    let p = Pair { a, b };
    let q = Pair { a: p.b, b: p.a };
    q.a
}

fn pick_second<T>(c: bool, a: T, b: T, d: T) -> T {
    let s = second(a, b);
    pick(c, s, d)
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    // 30 + 5 + 7 = 42.
    let x: i32 = pick(true, 30, 1);
    let y: i64 = second(9, 5);
    let z: u8 = pick_second(true, 0, 7, 1);
    let w: u8 = pick(true, 0, 99);
    x + y as i32 + z as i32 + w as i32
}
