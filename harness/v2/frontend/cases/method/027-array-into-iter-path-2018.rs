//@ expect: accept
//@ edition: 2018
// The path form is by value in every edition.
fn main() {
    let a = [1, 2, 3];
    let v: Vec<i32> = IntoIterator::into_iter(a).collect();
    println!("{:?}", v);
}
