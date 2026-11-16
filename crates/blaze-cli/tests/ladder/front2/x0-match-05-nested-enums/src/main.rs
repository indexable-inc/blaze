//! x0-match-05: the runnable twin of x-match-05-nested-enums (no_core; success 42, else the
//! failed check's number). The Reference, "Struct patterns" / "Tuple struct patterns", and arms
//! in order ("The first arm with a matching pattern is chosen"):
//! 1. Draw(Circle(3)): 3 * 3 = 9.     2. Draw(Rect { w: 2, h: 5 }): 2 * 5 = 10.
//! 3. Draw(Rect { 4, 4 }): the guarded arm first: 100 + 4 = 104.
//! 4. Move(-3): 200 + 3 = 203.        5. Quit: 7.
//! 6. an array of all five summed by index: 9 + 10 + 104 + 203 + 7 = 333.
//! A wrong builder that reads the inner discriminant at the outer's offset returns 1 or 2.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

enum Shape {
    Circle(u32),
    Rect { w: u32, h: u32 },
}

enum Msg {
    Draw(Shape),
    Move(i32),
    Quit,
}

fn eval(m: &Msg) -> u32 {
    match m {
        // Bindings are references here (default binding modes); the prelude has no operator
        // impls on references, so each is dereferenced.
        Msg::Draw(Shape::Circle(r)) => *r * *r,
        Msg::Draw(Shape::Rect { w, h }) if w == h => 100 + *w,
        Msg::Draw(Shape::Rect { w, h }) => *w * *h,
        Msg::Move(d) if *d < 0 => 200 + (-*d) as u32,
        Msg::Move(d) => *d as u32,
        Msg::Quit => 7,
    }
}

fn check() -> i32 {
    let all = [
        Msg::Draw(Shape::Circle(3)),
        Msg::Draw(Shape::Rect { w: 2, h: 5 }),
        Msg::Draw(Shape::Rect { w: 4, h: 4 }),
        Msg::Move(-3),
        Msg::Quit,
    ];
    let (mut total, mut i) = (0, 0);
    while i < 5 {
        total += eval(&all[i]);
        i += 1;
    }
    if eval(&all[0]) != 9 {
        1
    } else if eval(&all[1]) != 10 {
        2
    } else if eval(&all[2]) != 104 {
        3
    } else if eval(&all[3]) != 203 {
        4
    } else if eval(&all[4]) != 7 {
        5
    } else if total != 333 {
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
