// Rung 4: a second file and a module, a struct with fields, a `match` on an integer.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

mod util;

struct Point {
    x: i32,
    y: i32,
}

fn classify(n: i32) -> i32 {
    match n {
        0 => 10,
        1 | 2 => 20,
        _ => 30,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let mut p = Point { x: 3, y: 4 };
    p.x = util::double(p.x);
    // 6 + 4 = 10; classify(0) + classify(2) + classify(7) = 60; 10 + 60 - 28 = 42.
    let s = util::add(p.x, p.y);
    s + classify(0) + classify(2) + classify(7) - 28
}
