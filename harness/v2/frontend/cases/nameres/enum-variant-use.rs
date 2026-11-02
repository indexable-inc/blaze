//@ expect: accept
//@ edition: 2021
#[derive(Debug)]
enum E {
    First,
    Second(u8),
}

use E::*;

fn pick(n: u8) -> E {
    if n == 0 { First } else { Second(n) }
}

fn main() {
    println!("{:?} {:?}", pick(0), pick(2));
}
