//@ expect: accept
//@ edition: 2021
// From 2021 `array.into_iter()` is by value.
fn main() {
    let a = [1, 2, 3];
    let v: Vec<i32> = a.into_iter().collect();
    println!("{:?}", v);
}
