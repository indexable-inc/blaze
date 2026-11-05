//@ expect: accept
//@ crate: bin
//@ edition: 2021
// constants whose body is a `match` (with a range and a binding) or an `if`
const N: u32 = 7;
const B: i8 = -9;
const M: u32 = match N {
    0 => 1,
    1..=5 => 2,
    n => n * 3,
};
const ABS: i8 = if B < 0 { -B } else { B };
const KIND: &str = match B {
    i8::MIN..=-1 => "negative",
    0 => "zero",
    _ => "positive",
};

fn main() {
    println!("{} {} {}", M, ABS, KIND);
}
