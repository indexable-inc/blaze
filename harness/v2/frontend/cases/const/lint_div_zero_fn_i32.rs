//@ expect: reject unconditional_panic
//@ crate: bin
//@ edition: 2021
// `10 / 0` on i32 inside a fn body is rustc's deny-by-default unconditional_panic lint
fn main() {
    let x: i32 = 10 / 0;
    println!("{}", x);
}
