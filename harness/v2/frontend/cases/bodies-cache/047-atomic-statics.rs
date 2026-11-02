//@ expect: accept
//@ edition: 2021
//@ aux-crate: statics=statics.rs
fn main() { statics::call(); println!("{} {}", statics::call(), statics::GREETING); }
