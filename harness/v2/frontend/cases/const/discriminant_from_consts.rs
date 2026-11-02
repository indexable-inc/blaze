//@ expect: accept
//@ crate: bin
//@ edition: 2021
// discriminants read from other constants (`A = BASE`, `C = BASE * 2 - 1`), and constants read from discriminants
const BASE: u16 = 0x100;

#[derive(Clone, Copy)]
#[repr(u16)]
enum K {
    A = BASE,
    B,
    C = BASE * 2 - 1,
    D,
}

const DA: u16 = K::A as u16;
const DD: u16 = K::D as u16;
const GAP: u16 = DD - DA;

fn main() {
    println!("{} {} {} {} {}", DA, K::B as u16, K::C as u16, DD, GAP);
}
