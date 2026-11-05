//@ expect: accept
//@ crate: bin
//@ edition: 2021
// u128 constants in the shape: MIN and MAX as literals, chains, a constant used twice, `!`, every binary operator and comparison
const MIN: u128 = 0u128;
const MAX: u128 = 340282366920938463463374607431768211455;
const A: u128 = 12345678901234567890123;
const B: u128 = 7;
const C: u128 = A;
const D: u128 = C;
const NOT: u128 = !A;
const ADD: u128 = A + B;
const SUB: u128 = A - B;
const MUL: u128 = A * B;
const DIV: u128 = A / B;
const REM: u128 = A % B;
const AND: u128 = A & B;
const OR: u128 = A | B;
const XOR: u128 = A ^ B;
const SHL: u128 = A << 50;
const SHR: u128 = MAX >> 127u8;
const TWICE: u128 = D + D;
const EDGES: u128 = (MAX - 1) - (MIN + 1);
const BACK: u128 = MAX - (MAX - A);
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
