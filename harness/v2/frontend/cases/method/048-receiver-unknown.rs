//@ expect: reject E0282
//@ edition: 2021
// A method call on a receiver of unknown type needs an annotation.
fn main() {
    let x = Default::default();
    let _ = x.len();
    let _: String = x;
}
