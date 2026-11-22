//! x-adt-03: enums carrying data of different sizes: the right payload is read per variant.
//!
//! The Reference, "Enumerations": "Enum constructors can have either named or unnamed fields";
//! a value holds one variant at a time, and a match on it reads that variant's fields.
//! 1. Shape::Circle { r: 3 }: area as r * r * 3 = 27.
//! 2. Shape::Rect(4, 5): 20.      3. Shape::Square(6): 36.     4. Shape::Empty: 0.
//! 5. an array of all four, summed in a loop by index: 27 + 20 + 36 + 0 = 83.
//! 6. a method that changes the variant through `&mut self`: Square(6).grow() becomes
//!    Rect(6, 7): area 42.
//! A wrong builder that reads a payload at another variant's offset exits 2 or 3.

enum Shape {
    Circle { r: u64 },
    Rect(u16, u32),
    Square(u8),
    Empty,
}

impl Shape {
    fn area(&self) -> u64 {
        match self {
            Shape::Circle { r } => r * r * 3,
            Shape::Rect(w, h) => *w as u64 * *h as u64,
            Shape::Square(s) => *s as u64 * *s as u64,
            Shape::Empty => 0,
        }
    }

    fn grow(&mut self) {
        if let Shape::Square(s) = *self {
            *self = Shape::Rect(s as u16, s as u32 + 1);
        }
    }
}

fn main() -> std::entry::Status {
    let all = [Shape::Circle { r: 3 }, Shape::Rect(4, 5), Shape::Square(6), Shape::Empty];
    let (mut total, mut i) = (0, 0);
    while i < all.len() {
        total += all[i].area();
        i += 1;
    }
    let mut g = Shape::Square(6);
    g.grow();
    let status = if all[0].area() != 27 {
        1
    } else if all[1].area() != 20 {
        2
    } else if all[2].area() != 36 {
        3
    } else if all[3].area() != 0 {
        4
    } else if total != 83 {
        5
    } else if g.area() != 42 {
        6
    } else {
        0
    };
    std::entry::Status(status)
}
