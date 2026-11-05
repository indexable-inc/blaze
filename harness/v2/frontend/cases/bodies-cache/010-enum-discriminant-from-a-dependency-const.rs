//@ expect: accept
//@ edition: 2021
//@ aux-crate: consts=consts.rs
fn main() { println!("{} {} {}", consts::level_value(consts::Level::Low), consts::level_value(consts::Level::Mid), consts::Level::High as i32); }
