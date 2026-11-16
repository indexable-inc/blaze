// Own program x0-fpat-01 (front2): float and string LITERAL PATTERNS, each pattern's value read
// from its text by the seam (`float_pat`: the magnitude rounded once to the nearest `f64` and
// `f32`, picked by the pattern's type; `str_pat`: the unescaped text).
//
// The Reference, "Literal patterns": a literal pattern matches a value equal to the literal;
// "Floating-point literal expressions": the literal's value is its text rounded to the nearest
// value of its type, so `0.1` at `f32` matches the `f32` expression `0.1` and not the `f64`
// one's bits; a `-2.25` pattern is the negation of `2.25`.
//
// The status is the sum of every call's arm: f(1.5) 10, f(-2.25) 20, f(0.1) 30, f(2.0) 0,
// g(0.1) 5, s("abc") 2, s("ab") 1, s("") 3, s("abd") 0: 71. A pattern read as 0, as an integer's
// bits, at the wrong width or without its sign, or a string compared by length or by prefix
// only, moves the sum.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

fn f(x: f64) -> i32 {
    match x {
        1.5 => 10,
        -2.25 => 20,
        0.1 => 30,
        _ => 0,
    }
}

fn g(x: f32) -> i32 {
    match x {
        0.2 => 7,
        0.1 => 5,
        _ => 0,
    }
}

fn s(t: &str) -> i32 {
    match t {
        "ab" => 1,
        "abc" => 2,
        "" => 3,
        _ => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let floats = f(1.5) + f(-2.25) + f(0.1) + f(2.0) + g(0.1);
    let strings = s("abc") + s("ab") + s("") + s("abd");
    floats + strings
}
