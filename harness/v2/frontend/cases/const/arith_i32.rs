//@ expect: accept
//@ crate: bin
//@ edition: 2021
// i32 constants in the shape: MIN and MAX as literals, chains, a constant used twice, unary `-` and `!`, every binary operator and comparison
const MIN: i32 = -2147483648i32;
const MAX: i32 = 2147483647;
const A: i32 = 123456;
const B: i32 = -5;
const C: i32 = A;
const D: i32 = C;
const NEG: i32 = -A;
const NOT: i32 = !B;
const ADD: i32 = A + B;
const SUB: i32 = B - A;
const MUL: i32 = A * B;
const DIV: i32 = A / B;
const REM: i32 = A % -7;
const AND: i32 = A & B;
const OR: i32 = A | B;
const XOR: i32 = A ^ B;
const SHL: i32 = A << 12;
const SHR: i32 = MIN >> 31u8;
const TWICE: i32 = D + D;
const EDGES: i32 = (MIN + 1) + (MAX - 1);
const NEG_MAX: i32 = -MAX;
const EQ: bool = A == D;
const NE: bool = A != B;
const LT: bool = B < A;
const LE: bool = A <= D;
const GT: bool = MIN > MAX;
const GE: bool = MAX >= A;

fn main() {
    println!("{} {} {} {} {} {}", MIN, MAX, A, B, C, D);
    println!("{} {} {} {} {} {} {}", NEG, NOT, ADD, SUB, MUL, DIV, REM);
    println!("{} {} {} {} {} {} {} {}", AND, OR, XOR, SHL, SHR, TWICE, EDGES, NEG_MAX);
    println!("{} {} {} {} {} {}", EQ, NE, LT, LE, GT, GE);
}
