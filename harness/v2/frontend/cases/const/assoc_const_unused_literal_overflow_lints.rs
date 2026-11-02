//@ expect: reject arithmetic_overflow
//@ crate: bin
//@ edition: 2021
// an unused associated constant `255 + 1`: never evaluated, but rustc's arithmetic_overflow lint runs on associated constants' bodies
struct S;

#[allow(dead_code)]
impl S {
    const BAD: u8 = 255 + 1;
}

fn main() {
    println!("ok");
}
