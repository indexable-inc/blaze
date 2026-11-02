//@ expect: accept
//@ edition: 2018
// Before 2021 `array.into_iter()` autorefs: the items are references.
#![allow(array_into_iter)]

fn main() {
    let a = [1, 2, 3];
    let v: Vec<&i32> = a.into_iter().collect();
    println!("{:?}", v);
}
