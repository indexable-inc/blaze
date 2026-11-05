//@ expect: accept
//@ edition: 2021
//@ aux-crate: consts=consts.rs
fn main() { println!("{} {} {} {}", consts::FIB10, consts::fib(12), consts::table_sum(), consts::inline_const()); }
