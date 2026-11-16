//! x-match-05: nested enum patterns: a variant inside a variant, struct-like and tuple-like.
//!
//! The Reference, "Struct patterns" / "Tuple struct patterns": a pattern matches when its
//! path names the value's variant and every subpattern matches the field.
//! 1. Draw(Circle(3)): `Draw(Shape::Circle(r))` gives r * r = 9.
//! 2. Draw(Rect { w: 2, h: 5 }) with w != h: `Rect { w, h }` gives w * h = 10.
//! 3. Draw(Rect { w: 4, h: 4 }): the earlier arm `Rect { w, h } if w == h` gives 100 + 4 = 104.
//! 4. Move(-3): `Move(d) if d < 0` gives 200 + 3 = 203.   5. Quit: 7.
//! A wrong builder that reads the inner discriminant at the outer's offset exits 1 or 2.

enum Shape {
    Circle(u32),
    Rect { w: u32, h: u32 },
}

enum Msg {
    Draw(Shape),
    Move(i32),
    Quit,
}

fn eval(m: Msg) -> u32 {
    match m {
        Msg::Draw(Shape::Circle(r)) => r * r,
        Msg::Draw(Shape::Rect { w, h }) if w == h => 100 + w,
        Msg::Draw(Shape::Rect { w, h }) => w * h,
        Msg::Move(d) if d < 0 => 200 + (-d) as u32,
        Msg::Move(d) => d as u32,
        Msg::Quit => 7,
    }
}

fn main() -> std::entry::Status {
    let status = if eval(Msg::Draw(Shape::Circle(3))) != 9 {
        1
    } else if eval(Msg::Draw(Shape::Rect { w: 2, h: 5 })) != 10 {
        2
    } else if eval(Msg::Draw(Shape::Rect { w: 4, h: 4 })) != 104 {
        3
    } else if eval(Msg::Move(-3)) != 203 {
        4
    } else if eval(Msg::Quit) != 7 {
        5
    } else {
        0
    };
    std::entry::Status(status)
}
