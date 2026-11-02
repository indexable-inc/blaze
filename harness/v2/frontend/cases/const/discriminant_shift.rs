//@ expect: accept
//@ crate: bin
//@ edition: 2021
// enum discriminants written as `1 << 3` and `(1 << 5) | 1`, with an implicit one following an explicit one
#[derive(Clone, Copy)]
enum Flag {
    A = 1 << 3,
    B = 1 << 4,
    C,
    D = (1 << 5) | 1,
}

fn main() {
    println!("{} {} {} {}", Flag::A as i32, Flag::B as i32, Flag::C as i32, Flag::D as isize);
}
