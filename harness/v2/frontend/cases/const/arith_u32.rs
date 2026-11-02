//@ expect: accept
//@ crate: bin
//@ edition: 2021
// u32 constants in the shape: MIN and MAX as literals, chains, a constant used twice, `!`, every binary operator and comparison
const MIN: u32 = 0u32;
const MAX: u32 = 4294967295;
const A: u32 = 123456;
const B: u32 = 7;
const C: u32 = A;
const D: u32 = C;
const NOT: u32 = !A;
const ADD: u32 = A + B;
const SUB: u32 = A - B;
const MUL: u32 = A * B;
const DIV: u32 = A / B;
const REM: u32 = A % B;
const AND: u32 = A & B;
const OR: u32 = A | B;
const XOR: u32 = A ^ B;
const SHL: u32 = A << 14;
const SHR: u32 = MAX >> 31u8;
const TWICE: u32 = D + D;
const EDGES: u32 = (MAX - 1) - (MIN + 1);
const BACK: u32 = MAX - (MAX - A);
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
