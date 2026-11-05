//@ expect: accept
//@ crate: bin
//@ edition: 2021
// isize constants in the shape (64-bit target): MIN and MAX as literals, chains, a constant used twice, unary `-` and `!`, every binary operator and comparison
const MIN: isize = -9223372036854775808isize;
const MAX: isize = 9223372036854775807;
const A: isize = 98765;
const B: isize = -5;
const C: isize = A;
const D: isize = C;
const NEG: isize = -A;
const NOT: isize = !B;
const ADD: isize = A + B;
const SUB: isize = B - A;
const MUL: isize = A * B;
const DIV: isize = A / B;
const REM: isize = A % -7;
const AND: isize = A & B;
const OR: isize = A | B;
const XOR: isize = A ^ B;
const SHL: isize = A << 33;
const SHR: isize = MIN >> 62usize;
const TWICE: isize = D + D;
const EDGES: isize = (MIN + 1) + (MAX - 1);
const NEG_MAX: isize = -MAX;
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
