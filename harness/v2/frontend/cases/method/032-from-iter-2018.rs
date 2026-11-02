//@ expect: reject E0599
//@ edition: 2018
// `FromIterator` is not in the 2018 prelude.
fn main() {
    let v = Vec::<u8>::from_iter(0..3u8);
    println!("{:?}", v);
}
