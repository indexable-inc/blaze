//@ expect: reject E0422
//@ edition: 2021
fn main() {
    let _ = Nope { x: 1 };
}
