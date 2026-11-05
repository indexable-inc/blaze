//@ expect: accept
//@ crate: bin
//@ run: panic
//@ edition: 2021
// `1i32 << 40` in a fn body under `#[allow(arithmetic_overflow)]` builds, and panics when the line runs
#[allow(arithmetic_overflow)]
fn shift() -> i32 {
    1i32 << 40
}

fn main() {
    println!("{}", shift());
}
