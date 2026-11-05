//@ expect: accept
//@ edition: 2021
//@ aux-crate: unsafes=unsafes.rs
fn main() { let (mut a, mut b) = (1, 2); unsafes::swap_raw(&mut a, &mut b); println!("{a} {b} {}", unsafes::sum_ptr(&[1, 2, 3])); }
