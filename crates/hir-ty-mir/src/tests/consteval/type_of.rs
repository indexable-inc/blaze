//! `type_of!` (docs/v2/plan/design/type-of.md, row Q7 of docs/v2/plan/design/sql.md): the type
//! a `TypeDesc` constant describes is the trait solver's normal form of
//! `<Described<'a, D, Args> as TypeOf>::Type`, so it selects impls as the type written out
//! does. Each test picks an impl of `Tag` through a described type and evaluates its constant:
//! a described record is core's `Record` of its names sorted by their bytes, whatever order
//! they are listed in; equal descriptions are one type; `Arg`, `FieldOf`, `ArgOf`, `Option`,
//! `Ref` and `Tuple` build what they say; and a description that names no type has no `Tag`.

use super::{check_fail, check_number};

/// What the solver reads of library/core/src/reflect.rs, and `Tag`, the impls a test selects.
const REFLECT: &str = r#"
//- minicore: sized, copy, clone, eq, derive, const_param_ty, option, record, slice, str, coerce_unsized, phantom_data
#![feature(adt_const_params, unsized_const_params, lang_items, blaze_comptime)]
use core::marker::{ConstParamTy, PhantomData};
use core::record::Record;

#[derive(PartialEq, Eq, ConstParamTy)]
pub enum TypeDesc {
    Prim(Prim),
    Arg(u32),
    FieldOf(&'static TypeDesc, u32),
    ArgOf(&'static TypeDesc, u32),
    Option(&'static TypeDesc),
    Ref(&'static TypeDesc),
    Slice(&'static TypeDesc),
    Tuple(&'static [TypeDesc]),
    Record(&'static [FieldDesc]),
}
#[derive(PartialEq, Eq, ConstParamTy)]
pub struct FieldDesc {
    pub name: &'static str,
    pub ty: TypeDesc,
}
#[derive(PartialEq, Eq, ConstParamTy)]
pub enum Prim { Bool, Char, I8, I16, I32, I64, I128, Isize, U8, U16, U32, U64, U128, Usize, F32, F64, Str }

#[lang = "blaze_described"]
pub struct Described<'a, const D: TypeDesc, Args = ()> {
    _never: PhantomData<(&'a (), Args)>,
}
#[lang = "blaze_type_of"]
pub trait TypeOf {
    #[lang = "blaze_type_of_type"]
    type Type: ?Sized;
}
macro_rules! type_of {
    ($desc:expr $(; $($arg:ty),*)?) => {
        <Described<'static, { $desc }, ($($($arg,)*)?)> as TypeOf>::Type
    };
}

pub trait Tag { const N: usize; }
"#;

fn fixture(body: &str) -> String {
    format!("{REFLECT}{body}")
}

#[test]
fn a_described_record_is_the_record_in_any_field_order() {
    check_number(
        &fixture(
            r#"
pub struct Name;
impl Tag for Record<"age,name", (i64, Name)> { const N: usize = 1; }
impl Tag for Record<"age,name", (i64, u8)> { const N: usize = 1000; }
const NAME_AGE: TypeDesc = TypeDesc::Record(&[
    FieldDesc { name: "name", ty: TypeDesc::Arg(0) },
    FieldDesc { name: "age", ty: TypeDesc::Prim(Prim::I64) },
]);
const AGE_NAME: TypeDesc = TypeDesc::Record(&[
    FieldDesc { name: "age", ty: TypeDesc::Prim(Prim::I64) },
    FieldDesc { name: "name", ty: TypeDesc::Arg(0) },
]);
const GOAL: usize = <type_of!(NAME_AGE; Name) as Tag>::N
    + <type_of!(AGE_NAME; Name) as Tag>::N * 10
    + <type_of!(NAME_AGE; u8) as Tag>::N;
"#,
        ),
        1 + 10 + 1000,
    );
}

#[test]
fn each_variant_builds_what_it_names() {
    check_number(
        &fixture(
            r#"
pub struct Person { pub id: u32, pub age: u16, pub city: Option<u64> }
impl Tag for (Option<u8>, &'static str, u16) { const N: usize = 1; }
impl Tag for u64 { const N: usize = 10; }
impl Tag for &'static [bool] { const N: usize = 100; }
const TUPLE: TypeDesc = TypeDesc::Tuple(&[
    TypeDesc::Option(&TypeDesc::Prim(Prim::U8)),
    TypeDesc::Ref(&TypeDesc::Prim(Prim::Str)),
    TypeDesc::FieldOf(&TypeDesc::Arg(0), 1),
]);
const CITY: TypeDesc = TypeDesc::ArgOf(&TypeDesc::FieldOf(&TypeDesc::Arg(0), 2), 0);
const LIST: TypeDesc = TypeDesc::Ref(&TypeDesc::Slice(&TypeDesc::Prim(Prim::Bool)));
const GOAL: usize = <type_of!(TUPLE; Person) as Tag>::N
    + <type_of!(CITY; Person) as Tag>::N
    + <type_of!(LIST) as Tag>::N;
"#,
        ),
        111,
    );
}

#[test]
fn a_description_evaluated_by_a_const_fn_is_the_same_type() {
    check_number(
        &fixture(
            r#"
impl Tag for Record<"a,b", (u8, u16)> { const N: usize = 7; }
const fn pair(first_is_b: bool) -> TypeDesc {
    // Listed in either order; the record sorts them.
    match first_is_b {
        true => TypeDesc::Record(&[
            FieldDesc { name: "b", ty: TypeDesc::Prim(Prim::U16) },
            FieldDesc { name: "a", ty: TypeDesc::Prim(Prim::U8) },
        ]),
        false => TypeDesc::Record(&[
            FieldDesc { name: "a", ty: TypeDesc::Prim(Prim::U8) },
            FieldDesc { name: "b", ty: TypeDesc::Prim(Prim::U16) },
        ]),
    }
}
const GOAL: usize = <type_of!(pair(true)) as Tag>::N + <type_of!(pair(false)) as Tag>::N * 10;
"#,
        ),
        7 + 70,
    );
}

/// Impls of `Tag` for every type a wrong reading of the descriptions below could give.
const NEAR: &str = r#"
impl Tag for u8 { const N: usize = 1; }
impl Tag for u16 { const N: usize = 1; }
impl Tag for str { const N: usize = 1; }
impl Tag for [u8] { const N: usize = 1; }
impl Tag for Record<"a", (u8,)> { const N: usize = 1; }
impl Tag for Record<"a", (u16,)> { const N: usize = 1; }
impl Tag for Record<"a,b", (u8,)> { const N: usize = 1; }
"#;

fn named(desc: &str) -> String {
    fixture(&format!(
        "{NEAR}const BAD: TypeDesc = {desc};\nconst GOAL: usize = <type_of!(BAD; u8) as Tag>::N;\n"
    ))
}

#[test]
fn a_description_that_names_no_type_has_no_impl() {
    // The control: the same fixture with a description that names `u8`.
    check_number(&named("TypeDesc::Arg(0)"), 1);
    for bad in [
        "TypeDesc::Arg(1)",
        "TypeDesc::FieldOf(&TypeDesc::Prim(Prim::U8), 0)",
        "TypeDesc::Prim(Prim::Str)",
        "TypeDesc::Slice(&TypeDesc::Prim(Prim::U8))",
        "TypeDesc::Record(&[FieldDesc { name: \"a\", ty: TypeDesc::Prim(Prim::U8) }, \
         FieldDesc { name: \"a\", ty: TypeDesc::Prim(Prim::U16) }])",
        "TypeDesc::Record(&[FieldDesc { name: \"a,b\", ty: TypeDesc::Prim(Prim::U8) }])",
    ] {
        check_fail(&named(bad), |_| true);
    }
}
