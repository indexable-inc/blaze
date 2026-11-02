//@ expect: reject E0277
//@ edition: 2021
// `Vec<T>` needs `T: Sized`: a signature naming `Vec<[u8]>` is not well formed.
fn first(_v: &Vec<[u8]>) {}

fn main() {}
