//@ expect: accept
//@ edition: 2021
//@ aux-crate: floats=floats.rs
fn main() { println!("{} {} {}", floats::hyp(3.0, 4.0), floats::mean(&[1.0, 2.0, 4.5]), floats::round_trip(2.345)); }
