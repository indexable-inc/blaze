//@ expect: accept
//@ crate: bin
//@ edition: 2021
// constants whose body is a block with `let`s, including a `let mut` updated by compound assignment
const X: u64 = {
    let a = 3;
    let b = a * a;
    b + 1
};
const Y: i32 = {
    let mut v = 1;
    v <<= 4;
    v |= 1;
    v -= 20;
    v
};

fn main() {
    println!("{} {}", X, Y);
}
