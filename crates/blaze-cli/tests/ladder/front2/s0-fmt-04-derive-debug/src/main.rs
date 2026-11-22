//! s0-fmt-04: `#[derive(Debug)]` on structs, tuple structs, unit structs and enums, and the
//! pretty form `{:#?}`.
//!
//! First to need: the `Debug` derive's expansion (`Formatter::debug_struct_field2_finish`,
//! `debug_tuple_field1_finish`, `write_str` for a fieldless variant; library/core/src/fmt/
//! builders.rs) and the pretty printer's `PadAdapter` (each field on its own line, indented four
//! spaces, a trailing comma). The `Debug` docs (core/src/fmt/mod.rs, "Derivable"): a struct prints
//! `Name { field: value }`, a tuple struct `Name(value)`, a unit struct `Name`.

#[derive(Debug)]
struct Point {
    x: i32,
    y: i32,
}

#[derive(Debug)]
struct Meters(u32);

#[derive(Debug)]
struct Unit;

#[derive(Debug)]
enum Shape {
    Circle { r: u8 },
    Square(u8),
    Empty,
}

#[derive(Debug)]
struct Line {
    from: Point,
    to: Point,
    tag: Option<&'static str>,
}

fn main() {
    let p = Point { x: 1, y: -2 };
    println!("{:?}", p);
    println!("{:?} {:?}", Meters(5), Unit);
    println!("{:?}", [Shape::Circle { r: 2 }, Shape::Square(3), Shape::Empty]);
    println!("{:?}", (1, "a", 'c', 2.5, true, ()));
    println!("{:?}", vec![Some(1), None]);
    println!("{:?} {:?}", Vec::<u8>::new(), Ok::<i32, String>(3));
    let l = Line { from: Point { x: 0, y: 0 }, to: p, tag: Some("t") };
    println!("{:#?}", l);
}
