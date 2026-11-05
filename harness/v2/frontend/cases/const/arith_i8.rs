//@ expect: accept
//@ crate: bin
//@ edition: 2021
// i8 constants in the shape: MIN and MAX as literals, chains, a constant used twice, unary `-` and `!`, every binary operator and comparison
const MIN: i8 = -128i8;
const MAX: i8 = 127;
const A: i8 = 12;
const B: i8 = -5;
const C: i8 = A;
const D: i8 = C;
const NEG: i8 = -A;
const NOT: i8 = !B;
const ADD: i8 = A + B;
const SUB: i8 = B - A;
const MUL: i8 = A * B;
const DIV: i8 = A / B;
const REM: i8 = A % B;
const AND: i8 = A & B;
const OR: i8 = A | B;
const XOR: i8 = A ^ B;
const SHL: i8 = A << 3;
const SHR: i8 = B >> 1u8;
const TWICE: i8 = D + D;
const EDGES: i8 = (MIN + 1) + (MAX - 1);
const NEG_MAX: i8 = -MAX;
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
