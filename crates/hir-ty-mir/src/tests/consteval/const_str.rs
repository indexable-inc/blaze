//! `&'static str` const generic arguments evaluated and used (row Q5 of
//! docs/v2/plan/design/sql.md): a function returning its parameter gives the bytes, which const
//! evaluation materialises from the argument's value tree; `S.len()` and `S.as_bytes()[i]` read
//! them; and an argument that is evaluated (a `const`, `concat!`, a const fn's result) is the
//! same type as the literal with the same bytes, as the solver keys the type by the value tree,
//! never by where the string was written or its address. The parameter's rule and the
//! literal's type identity are hir-ty's `tests::const_str`.

use super::{check_number, check_str};

const MINICORE: &str =
    "//- minicore: sized, const_param_ty, slice, index, str, concat, coerce_unsized";

fn fixture(body: &str) -> String {
    format!("{MINICORE}\n#![feature(adt_const_params, unsized_const_params)]\n{body}")
}

#[test]
fn a_function_returning_its_parameter_gives_its_bytes() {
    check_str(
        &fixture(
            r#"
const fn id<const S: &'static str>() -> &'static str { S }
const GOAL: &str = id::<"hello, world">();
"#,
        ),
        "hello, world",
    );
}

#[test]
fn the_parameter_is_read_as_a_value() {
    check_number(
        &fixture(
            r#"
const fn len<const S: &'static str>() -> usize { S.as_bytes().len() }
const fn byte<const S: &'static str, const I: usize>() -> u8 { S.as_bytes()[I] }
const GOAL: usize = len::<"hello">() * 1000 + byte::<"hello", 1>() as usize + len::<"">();
"#,
        ),
        5 * 1000 + b'e' as i128,
    );
}

#[test]
fn an_evaluated_argument_is_the_type_of_its_bytes() {
    check_number(
        &fixture(
            r#"
pub struct S<const X: &'static str>;
pub trait Tag { const N: usize; }
impl Tag for S<"abc"> { const N: usize = 1; }
impl Tag for S<"abd"> { const N: usize = 1000; }
const NAME: &str = "abc";
const fn make() -> &'static str { NAME }
const GOAL: usize = <S<NAME> as Tag>::N
    + <S<{ concat!("a", "bc") }> as Tag>::N * 10
    + <S<{ make() }> as Tag>::N * 100
    + <S<"abd"> as Tag>::N;
"#,
        ),
        1 + 10 + 100 + 1000,
    );
}

#[test]
fn a_byte_slice_parameter_takes_the_same_path() {
    check_number(
        &fixture(
            r#"
const fn sum<const B: &'static [u8]>() -> usize {
    let mut at = 0;
    let mut sum = 0;
    while at < B.len() {
        sum += B[at] as usize;
        at += 1;
    }
    sum
}
const GOAL: usize = sum::<{ &[1, 2, 3] }>() + sum::<{ &[10] }>() * 100;
"#,
        ),
        6 + 1000,
    );
}
