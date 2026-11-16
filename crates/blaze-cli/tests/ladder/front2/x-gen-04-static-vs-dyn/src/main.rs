//! x-gen-04: the same impl called by static dispatch and through `&dyn Trait` gives equal
//! answers.
//!
//! The Reference, "Trait objects": "a trait object ... the methods are called through a
//! virtual method table (vtable)" pointing at the same implementations a direct call reaches.
//! trait Eval { fn eval(&self, x: u32) -> u32; } for Lin { a, b } (a * x + b) and Sq (x * x):
//! 1. Lin { a: 3, b: 4 } at 5: static 19, dyn 19.
//! 2. Sq at 7: static 49, dyn 49.
//! 3. dyn through a function taking `&dyn Eval`, and through a generic `T: Eval + ?Sized`
//!    instantiated at `dyn Eval`: Lin at 2 gives 10 both ways.
//! 4. the data pointer is the value's: a dyn call that reads `self.a` reads 3 (a vtable with
//!    the wrong impl would compute 2 * 2 = 4, not 10).
//! A wrong builder that puts the methods in the wrong vtable slot exits 1 or 2.

trait Eval {
    fn eval(&self, x: u32) -> u32;
}

struct Lin {
    a: u32,
    b: u32,
}

struct Sq;

impl Eval for Lin {
    fn eval(&self, x: u32) -> u32 {
        self.a * x + self.b
    }
}

impl Eval for Sq {
    fn eval(&self, x: u32) -> u32 {
        x * x
    }
}

fn by_dyn(e: &dyn Eval, x: u32) -> u32 {
    e.eval(x)
}

fn by_generic<T: Eval + ?Sized>(e: &T, x: u32) -> u32 {
    e.eval(x)
}

fn main() -> std::entry::Status {
    let lin = Lin { a: 3, b: 4 };
    let d: &dyn Eval = &lin;
    let status = if lin.eval(5) != 19 || by_dyn(&lin, 5) != 19 {
        1
    } else if Sq.eval(7) != 49 || by_dyn(&Sq, 7) != 49 {
        2
    } else if by_generic(d, 2) != 10 || by_generic(&lin, 2) != 10 {
        3
    } else if d.eval(2) != 10 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
