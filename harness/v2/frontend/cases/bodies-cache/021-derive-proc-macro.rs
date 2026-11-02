//@ expect: accept
//@ edition: 2021
//@ aux-crate: pmuser=pmuser.rs
fn main() { println!("{}", pmuser::widget_name()); }
