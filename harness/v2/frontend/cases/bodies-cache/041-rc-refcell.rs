//@ expect: accept
//@ edition: 2021
//@ aux-crate: shared=shared.rs
fn main() { let (a, b) = shared::shared(); shared::hit(&a); shared::hit(&b); println!("{}", a.hits.borrow()); }
