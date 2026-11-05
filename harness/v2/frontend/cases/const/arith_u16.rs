//@ expect: accept
//@ crate: bin
//@ edition: 2021
// u16 constants in the shape: MIN and MAX as literals, chains, a constant used twice, `!`, every binary operator and comparison
const MIN: u16 = 0u16;
const MAX: u16 = 65535;
const A: u16 = 1200;
const B: u16 = 7;
const C: u16 = A;
const D: u16 = C;
const NOT: u16 = !A;
const ADD: u16 = A + B;
const SUB: u16 = A - B;
const MUL: u16 = A * B;
const DIV: u16 = A / B;
const REM: u16 = A % B;
const AND: u16 = A & B;
const OR: u16 = A | B;
const XOR: u16 = A ^ B;
const SHL: u16 = A << 5;
const SHR: u16 = MAX >> 15u8;
const TWICE: u16 = D + D;
const EDGES: u16 = (MAX - 1) - (MIN + 1);
const BACK: u16 = MAX - (MAX - A);
const EQ: bool = A == D;
const NE: bool = A != B;
const LT: bool = B < A;
const LE: bool = A <= D;
const GT: bool = MIN > MAX;
const GE: bool = MAX >= A;

fn main() {
    println!("{} {} {} {} {} {}", MIN, MAX, A, B, C, D);
    println!("{} {} {} {} {} {}", NOT, ADD, SUB, MUL, DIV, REM);
    println!("{} {} {} {} {} {} {} {}", AND, OR, XOR, SHL, SHR, TWICE, EDGES, BACK);
    println!("{} {} {} {} {} {}", EQ, NE, LT, LE, GT, GE);
}
