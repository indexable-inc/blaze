//@ expect: accept
//@ crate: bin
//@ edition: 2021
// usize constants in the shape (64-bit target): MIN and MAX as literals, chains, a constant used twice, `!`, every binary operator and comparison
const MIN: usize = 0usize;
const MAX: usize = 18446744073709551615;
const A: usize = 98765;
const B: usize = 7;
const C: usize = A;
const D: usize = C;
const NOT: usize = !A;
const ADD: usize = A + B;
const SUB: usize = A - B;
const MUL: usize = A * B;
const DIV: usize = A / B;
const REM: usize = A % B;
const AND: usize = A & B;
const OR: usize = A | B;
const XOR: usize = A ^ B;
const SHL: usize = A << 40;
const SHR: usize = MAX >> 60i32;
const TWICE: usize = D + D;
const EDGES: usize = (MAX - 1) - (MIN + 1);
const BACK: usize = MAX - (MAX - A);
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
