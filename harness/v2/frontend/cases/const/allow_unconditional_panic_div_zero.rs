//@ expect: accept
//@ crate: bin
//@ run: panic
//@ edition: 2021
// `10 / 0` in a fn body under `#[allow(unconditional_panic)]` builds, and panics when the line runs
#[allow(unconditional_panic)]
fn main() {
    let x: i32 = 10 / 0;
    println!("{}", x);
}
