//@ expect: accept
//@ crate: bin
//@ edition: 2021
// i16 constants in the shape: MIN and MAX as literals, chains, a constant used twice, unary `-` and `!`, every binary operator and comparison
const MIN: i16 = -32768i16;
const MAX: i16 = 32767;
const A: i16 = 1200;
const B: i16 = -5;
const C: i16 = A;
const D: i16 = C;
const NEG: i16 = -A;
const NOT: i16 = !B;
const ADD: i16 = A + B;
const SUB: i16 = B - A;
const MUL: i16 = A * B;
const DIV: i16 = A / B;
const REM: i16 = A % -7;
const AND: i16 = A & B;
const OR: i16 = A | B;
const XOR: i16 = A ^ B;
const SHL: i16 = A << 4;
const SHR: i16 = B >> 1u8;
const TWICE: i16 = D + D;
const EDGES: i16 = (MIN + 1) + (MAX - 1);
const NEG_MAX: i16 = -MAX;
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
