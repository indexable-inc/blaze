//@ expect: accept
//@ crate: bin
//@ edition: 2021
// u64 constants in the shape: MIN and MAX as literals, chains, a constant used twice, `!`, every binary operator and comparison
const MIN: u64 = 0u64;
const MAX: u64 = 18446744073709551615;
const A: u64 = 1234567890123;
const B: u64 = 7;
const C: u64 = A;
const D: u64 = C;
const NOT: u64 = !A;
const ADD: u64 = A + B;
const SUB: u64 = A - B;
const MUL: u64 = A * B;
const DIV: u64 = A / B;
const REM: u64 = A % B;
const AND: u64 = A & B;
const OR: u64 = A | B;
const XOR: u64 = A ^ B;
const SHL: u64 = A << 22;
const SHR: u64 = MAX >> 63u8;
const TWICE: u64 = D + D;
const EDGES: u64 = (MAX - 1) - (MIN + 1);
const BACK: u64 = MAX - (MAX - A);
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
