//@ expect: accept
//@ crate: bin
//@ edition: 2021
// `!`, `& | ^`, `&& ||` and comparisons on bool constants; comparisons on char constants
const T: bool = true;
const F: bool = !T;
const AND: bool = T & F;
const OR: bool = T | F;
const XOR: bool = T ^ T;
const LAND: bool = T && !F;
const LOR: bool = F || T;
const BLT: bool = F < T;
const BEQ: bool = T == !F;
const CA: char = 'a';
const CZ: char = 'z';
const CLT: bool = CA < CZ;
const CNE: bool = CA != CZ;
const CGE: bool = '\u{10FFFF}' >= CZ;
const CEQ: bool = CA == 'a';

fn main() {
    println!("{} {} {} {} {} {} {} {} {}", T, F, AND, OR, XOR, LAND, LOR, BLT, BEQ);
    println!("{} {} {} {} {} {}", CA, CZ, CLT, CNE, CGE, CEQ);
}
