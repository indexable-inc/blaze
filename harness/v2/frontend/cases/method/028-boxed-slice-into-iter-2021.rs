//@ expect: accept
//@ edition: 2021
// Before 2024 a boxed slice's `into_iter()` autorefs.
#![allow(boxed_slice_into_iter)]

fn main() {
    let b: Box<[i32]> = vec![1, 2].into_boxed_slice();
    let v: Vec<&i32> = b.into_iter().collect();
    println!("{:?}", v);
}
