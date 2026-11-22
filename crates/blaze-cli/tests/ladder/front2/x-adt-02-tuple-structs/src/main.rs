//! x-adt-02: tuple structs, newtypes, tuples, and struct update syntax.
//!
//! The Reference, "Tuple and tuple indexing expressions": `t.0`, `t.1` name fields by position;
//! "Struct expressions", functional update: "`..base` ... the fields not given are moved or
//! copied from the base expression".
//! 1. Rgb(10, 20, 30): .0 + .2 = 40.
//! 2. a newtype Meters(5) with a method `double` -> Meters(10): .0 = 10.
//! 3. a nested tuple ((1, 2), (3, (4, 5))): t.1.1.0 = 4.
//! 4. `Config { verbose: true, ..base }` with base { level: 3, verbose: false, width: 80 }:
//!    level 3, width 80, verbose true.
//! 5. a tuple struct used as a function: `let f = Rgb; f(1, 2, 3).1` = 2.
//! A wrong builder that orders tuple fields wrongly exits 1 or 3.

struct Rgb(u8, u8, u8);

struct Meters(u32);

impl Meters {
    fn double(self) -> Meters {
        Meters(self.0 * 2)
    }
}

struct Config {
    level: u8,
    verbose: bool,
    width: u16,
}

fn main() -> std::entry::Status {
    let c = Rgb(10, 20, 30);
    let t = ((1u8, 2u8), (3u8, (4u8, 5u8)));
    let base = Config { level: 3, verbose: false, width: 80 };
    let cfg = Config { verbose: true, ..base };
    let f = Rgb;
    let status = if c.0 as u32 + c.2 as u32 != 40 || c.1 != 20 {
        1
    } else if Meters(5).double().0 != 10 {
        2
    } else if t.1.1.0 != 4 || t.0.1 != 2 {
        3
    } else if cfg.level != 3 || cfg.width != 80 || !cfg.verbose {
        4
    } else if f(1, 2, 3).1 != 2 {
        5
    } else {
        0
    };
    std::entry::Status(status)
}
