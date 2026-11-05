//@ expect: accept
//@ crate: bin
//@ edition: 2021
// i64 constants in the shape: MIN and MAX as literals, chains, a constant used twice, unary `-` and `!`, every binary operator and comparison
const MIN: i64 = -9223372036854775808i64;
const MAX: i64 = 9223372036854775807;
const A: i64 = 1234567890123;
const B: i64 = -5;
const C: i64 = A;
const D: i64 = C;
const NEG: i64 = -A;
const NOT: i64 = !B;
const ADD: i64 = A + B;
const SUB: i64 = B - A;
const MUL: i64 = A * B;
const DIV: i64 = A / B;
const REM: i64 = A % -7;
const AND: i64 = A & B;
const OR: i64 = A | B;
const XOR: i64 = A ^ B;
const SHL: i64 = A << 20;
const SHR: i64 = MIN >> 63u8;
const TWICE: i64 = D + D;
const EDGES: i64 = (MIN + 1) + (MAX - 1);
const NEG_MAX: i64 = -MAX;
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
