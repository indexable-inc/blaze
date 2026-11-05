//@ expect: accept
//@ crate: bin
//@ edition: 2021
// u8 constants in the shape: MIN and MAX as literals, chains, a constant used twice, `!`, every binary operator and comparison
const MIN: u8 = 0u8;
const MAX: u8 = 255;
const A: u8 = 12;
const B: u8 = 5;
const C: u8 = A;
const D: u8 = C;
const NOT: u8 = !A;
const ADD: u8 = A + B;
const SUB: u8 = A - B;
const MUL: u8 = A * B;
const DIV: u8 = A / B;
const REM: u8 = A % B;
const AND: u8 = A & B;
const OR: u8 = A | B;
const XOR: u8 = A ^ B;
const SHL: u8 = A << 4;
const SHR: u8 = MAX >> 7u32;
const TWICE: u8 = D + D;
const EDGES: u8 = (MAX - 1) - (MIN + 1);
const BACK: u8 = MAX - (MAX - A);
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
