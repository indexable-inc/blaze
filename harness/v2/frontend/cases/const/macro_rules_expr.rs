//@ expect: accept
//@ crate: bin
//@ edition: 2021
// constants whose expression a `macro_rules!` writes; `twice!(1 + 2)` is 6, since an `$e:expr` stays one operand
macro_rules! twice {
    ($e:expr) => {
        $e * 2
    };
}

macro_rules! sum {
    ($($e:expr),*) => {
        0 $(+ $e)*
    };
}

macro_rules! def {
    ($name:ident = $e:expr) => {
        const $name: i32 = $e;
    };
}

const A: u32 = twice!(21);
const B: u32 = twice!(1 + 2);
const C: u64 = sum!(1, 2, 3, 4);
def!(D = -7 * 3);

fn main() {
    println!("{} {} {} {}", A, B, C, D);
}
