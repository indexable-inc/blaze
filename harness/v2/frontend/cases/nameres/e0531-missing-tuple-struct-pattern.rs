//@ expect: reject E0531
//@ edition: 2021
fn main() {
    let Nope(_x) = 5;
}
