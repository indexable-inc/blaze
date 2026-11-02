//@ expect: accept
//@ crate: bin
//@ edition: 2021
// i128 constants in the shape: MIN and MAX as literals, chains, a constant used twice, unary `-` and `!`, every binary operator and comparison
const MIN: i128 = -170141183460469231731687303715884105728i128;
const MAX: i128 = 170141183460469231731687303715884105727;
const A: i128 = 12345678901234567890123;
const B: i128 = -5;
const C: i128 = A;
const D: i128 = C;
const NEG: i128 = -A;
const NOT: i128 = !B;
const ADD: i128 = A + B;
const SUB: i128 = B - A;
const MUL: i128 = A * B;
const DIV: i128 = A / B;
const REM: i128 = A % -7;
const AND: i128 = A & B;
const OR: i128 = A | B;
const XOR: i128 = A ^ B;
const SHL: i128 = A << 40;
const SHR: i128 = MIN >> 127u8;
const TWICE: i128 = D + D;
const EDGES: i128 = (MIN + 1) + (MAX - 1);
const NEG_MAX: i128 = -MAX;
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
