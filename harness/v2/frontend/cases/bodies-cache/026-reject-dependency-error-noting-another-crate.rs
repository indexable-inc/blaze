//@ expect: reject E0308
//@ edition: 2021
//@ aux-crate: bad_cross=bad_cross.rs
// The error's "function defined here" note points into `arith`'s file, outside `bad_cross`'s
// package: its outcome holds a path of the machine, so it is refused; its bodies are kept.
//@ refused: bad_cross
fn main() { println!("{}", bad_cross::broken()); }
