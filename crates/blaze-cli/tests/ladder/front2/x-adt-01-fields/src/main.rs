//! x-adt-01: struct field reads and writes, in nested structs, through values and references.
//!
//! The Reference, "Field access expressions": a field expression is a place for that field;
//! writing it changes only that field.
//! Line { a: Point { x: 1, y: 2 }, b: Point { x: 30, y: 40 }, tag: 7 }:
//! 1. l.a.y + l.b.x = 2 + 30 = 32.
//! 2. `l.b.y = 41`: l.b.y = 41; l.a.y still 2; l.tag still 7.
//! 3. through `&mut Line`: `p.a.x += 100`: l.a.x = 101.
//! 4. a struct copied out (`let q = l.b;` Point is Copy) then written: l.b.x stays 30, q.x = 31.
//! 5. a struct returned by value from a function: mid(&l).x = (101 + 30) / 2 = 65.
//! A wrong builder that writes the wrong offset (a field of the other point) exits 2.

#[derive(Clone, Copy)]
struct Point {
    x: u32,
    y: u32,
}

struct Line {
    a: Point,
    b: Point,
    tag: u8,
}

fn shift(p: &mut Line) {
    p.a.x += 100;
}

fn mid(l: &Line) -> Point {
    Point { x: (l.a.x + l.b.x) / 2, y: (l.a.y + l.b.y) / 2 }
}

fn main() -> std::entry::Status {
    let mut l = Line { a: Point { x: 1, y: 2 }, b: Point { x: 30, y: 40 }, tag: 7 };
    let one = l.a.y + l.b.x;
    l.b.y = 41;
    let two = l.b.y == 41 && l.a.y == 2 && l.tag == 7;
    shift(&mut l);
    let mut q = l.b;
    q.x += 1;
    let status = if one != 32 {
        1
    } else if !two {
        2
    } else if l.a.x != 101 {
        3
    } else if l.b.x != 30 || q.x != 31 {
        4
    } else if mid(&l).x != 65 || mid(&l).y != 21 {
        5
    } else {
        0
    };
    std::entry::Status(status)
}
