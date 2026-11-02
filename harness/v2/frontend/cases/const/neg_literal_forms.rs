//@ expect: accept
//@ crate: bin
//@ edition: 2021
// negative literals: MIN of every signed type written out (unsuffixed, typed by the constant), `- -x`, `-(lit)`, `!-1`
const I8: i8 = -128;
const I16: i16 = -32768;
const I32: i32 = -2147483648;
const I64: i64 = -9223372036854775808;
const I128: i128 = -170141183460469231731687303715884105728;
const ISIZE: isize = -9223372036854775808;
const DOUBLE: i8 = - -5;
const PAREN: i32 = -(7);
const MIN_BY_SUB: i64 = -9223372036854775807 - 1;
const BACK: i16 = -(-32767i16);
const NOT_NEG: isize = !-1;

fn main() {
    println!("{} {} {} {} {} {}", I8, I16, I32, I64, I128, ISIZE);
    println!("{} {} {} {} {}", DOUBLE, PAREN, MIN_BY_SUB, BACK, NOT_NEG);
}
