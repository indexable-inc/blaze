use base_db::all_crates;
use hir_def::signatures::ConstSignature;
use hir_expand::EditionedFileId;
use hir_expand::fixture;
use rustc_apfloat::{
    Float,
    ieee::{Half as f16, Quad as f128},
};
use test_utils::skip_slow_tests;

use crate::{
    MemoryMap,
    consteval::allocation_as_usize,
    db::HirDatabase,
    display::DisplayTarget,
    mir::{IsSigned, pad16},
    mir_pretty::{MirEvalErrorPretty as _, MirLowerErrorPretty as _},
    next_solver::{Allocation, GenericArgs},
    setup_tracing,
    test_db::{TestDB, TestDBExt as _},
};

use crate::{
    consteval::ConstEvalError,
    mir::{Budget, MirEvalError, MirLowerError},
};

mod arch;
mod comptime;
mod const_ast;
mod const_macro;
mod const_traits;
mod deps;
mod intrinsics;
mod memo;
mod trivial;
mod word;

fn simplify(e: ConstEvalError<'_>) -> ConstEvalError<'_> {
    match e {
        ConstEvalError::MirEvalError(MirEvalError::InFunction(e, _)) => {
            simplify(ConstEvalError::MirEvalError(*e))
        }
        _ => e,
    }
}

#[track_caller]
fn check_fail(
    #[rust_analyzer::rust_fixture] ra_fixture: &str,
    error: impl FnOnce(ConstEvalError<'_>) -> bool,
) {
    let fx = fixture::db(ra_fixture);
    let file_id = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || match eval_goal(&db, file_id) {
        Ok(_) => panic!("Expected fail, but it succeeded"),
        Err(e) => {
            assert!(
                error(simplify(e.clone())),
                "Actual error was: {}\n{e:?}",
                pretty_print_err(e.clone(), &db)
            )
        }
    })
}

#[track_caller]
fn check_number(#[rust_analyzer::rust_fixture] ra_fixture: &str, answer: i128) {
    let fx = fixture::db(ra_fixture);
    let file_ids = fx.files().to_vec();
    let db = fx.into_db();
    check_number_in(&db, *file_ids.last().unwrap(), answer);
}

/// [`check_number`] of the goal in `file_id` of a database the caller set up.
#[track_caller]
fn check_number_in(db: &TestDB, file_id: EditionedFileId, answer: i128) {
    check_answer_in(db, file_id, |b, _| {
        assert_eq!(
            b,
            &answer.to_le_bytes()[0..b.len()],
            "Bytes differ. In decimal form: actual = {}, expected = {answer}",
            i128::from_le_bytes(pad16(b, IsSigned::Yes))
        );
    });
}

#[track_caller]
fn check_str(#[rust_analyzer::rust_fixture] ra_fixture: &str, answer: &str) {
    check_answer(ra_fixture, |b, mm| {
        let addr = usize::from_le_bytes(b[0..b.len() / 2].try_into().unwrap());
        let size = usize::from_le_bytes(b[b.len() / 2..].try_into().unwrap());
        let Some(bytes) = mm.get(addr, size) else {
            panic!("string data missed in the memory map");
        };
        assert_eq!(
            bytes,
            answer.as_bytes(),
            "Bytes differ. In string form: actual = {}, expected = {answer}",
            String::from_utf8_lossy(bytes)
        );
    });
}

#[track_caller]
fn check_answer(
    #[rust_analyzer::rust_fixture] ra_fixture: &str,
    check: impl FnOnce(&[u8], &MemoryMap<'_>),
) {
    let fx = fixture::db(ra_fixture);
    let file_ids = fx.files().to_vec();
    let db = fx.into_db();
    check_answer_in(&db, *file_ids.last().unwrap(), check);
}

#[track_caller]
fn check_answer_in(
    db: &TestDB,
    file_id: EditionedFileId,
    check: impl FnOnce(&[u8], &MemoryMap<'_>),
) {
    blaze_query::db::attach(db, || {
        let r = match eval_goal(db, file_id) {
            Ok(t) => t,
            Err(e) => {
                let err = pretty_print_err(e, db);
                panic!("Error in evaluating goal: {err}");
            }
        };
        check(&r.memory, &r.memory_map);
    });
}

fn pretty_print_err(e: ConstEvalError<'_>, db: &TestDB) -> String {
    let mut err = String::new();
    let span_formatter = |file, range| format!("{file:?} {range:?}");
    let display_target =
        DisplayTarget::from_crate(db, *all_crates(db).last().expect("no crate graph present"));
    match e {
        ConstEvalError::MirLowerError(e) => {
            e.pretty_print(&mut err, db, span_formatter, display_target)
        }
        ConstEvalError::MirEvalError(e) => {
            e.pretty_print(&mut err, db, span_formatter, display_target)
        }
    }
    .unwrap();
    err
}

fn eval_goal(db: &TestDB, file_id: EditionedFileId) -> Result<Allocation<'_>, ConstEvalError<'_>> {
    let _tracing = setup_tracing();
    let module_id = db.module_for_file(file_id.file_id(db));
    let def_map = module_id.def_map(db);
    let scope = &def_map.module(db, module_id).scope(db);
    let const_id = scope
        .declarations()
        .find_map(|x| match x {
            hir_def::ModuleDefId::ConstId(x) => {
                if ConstSignature::of(db, x)
                    .name
                    .as_ref()?
                    .display(db, file_id.edition(db))
                    .to_string()
                    == "GOAL"
                {
                    Some(x)
                } else {
                    None
                }
            }
            _ => None,
        })
        .expect("No const named GOAL found in the test");
    db.const_eval(const_id, GenericArgs::empty(), None)
}

#[test]
fn generic_const_argument_macro() {
    check_number(
        r#"
#[rustc_builtin_macro]
macro_rules! gca {}
const fn add1(n: usize) -> usize { n + 1 }
const fn len<const N: usize>(_: [u8; N]) -> usize { N }
const ADD: usize = gca!(add1(2));
const GOAL: usize = gca!({ ADD * 10 }) + len([0u8; gca!(ADD)]);
"#,
        33,
    );
}

#[test]
fn add() {
    check_number(r#"const GOAL: usize = 2 + 2;"#, 4);
    check_number(r#"const GOAL: i32 = -2 + --5;"#, 3);
    check_number(r#"const GOAL: i32 = 7 - 5;"#, 2);
    check_number(r#"const GOAL: i32 = 7 + (1 - 5);"#, 3);
}

#[test]
fn bit_op() {
    check_number(r#"const GOAL: u8 = !0 & !(!0 >> 1)"#, 128);
    check_number(r#"const GOAL: i8 = !0 & !(!0 >> 1)"#, 0);
    check_number(r#"const GOAL: i8 = 1 << 7"#, (1i8 << 7) as i128);
    check_number(r#"const GOAL: i8 = -1 << 2"#, (-1i8 << 2) as i128);
    check_fail(r#"const GOAL: i8 = 1 << 8"#, |e| {
        e == ConstEvalError::MirEvalError(MirEvalError::Panic("Overflow in Shl".to_owned()))
    });
    check_number(r#"const GOAL: i32 = 100000000i32 << 11"#, (100000000i32 << 11) as i128);
}

#[test]
fn overflow_in_a_constant() {
    // The test crate has no `-C overflow-checks`: a constant's own arithmetic is checked
    // regardless, and a `const fn`'s wraps, as rustc evaluates both.
    let overflow = |op: &str| {
        let op = format!("Overflow in {op}");
        move |e: ConstEvalError<'_>| e == ConstEvalError::MirEvalError(MirEvalError::Panic(op))
    };
    check_fail(r#"const GOAL: u8 = { let x = 255u8; x + 1 };"#, overflow("Add"));
    check_fail(r#"const GOAL: i8 = { let x = -128i8; -x };"#, overflow("Neg"));
    check_fail(r#"const GOAL: u32 = { let x = 1u32; x << 32 };"#, overflow("Shl"));
    check_fail(r#"const GOAL: u8 = { let x = 0u8; x - 1 };"#, overflow("Sub"));
    check_number(r#"const fn add(a: u8, b: u8) -> u8 { a + b } const GOAL: u8 = add(255, 1);"#, 0);
}

/// `u3` and `i7` (`#![blaze::arbitrary_ints]`) compute at their width. A constant's own
/// arithmetic is checked and panics naming the operation and the type; a function wraps at the
/// width in a crate without `overflow-checks` and panics in one with them; division panics
/// under both, as at every width.
#[test]
fn arbitrary_ints_overflow_at_their_width() {
    let panic = |message: &'static str| {
        move |e: ConstEvalError<'_>| {
            e == ConstEvalError::MirEvalError(MirEvalError::Panic(message.to_owned()))
        }
    };
    let opted = |body: &str| format!("#![blaze::arbitrary_ints]\n{body}");
    let checking = |body: &str| {
        format!("//- /main.rs crate:main cfg:overflow_checks\n#![blaze::arbitrary_ints]\n{body}")
    };
    // In range: the values of the width.
    check_number(&opted("const GOAL: u3 = { let x: u3 = 5; x + 2 };"), 7);
    check_number(&opted("const GOAL: i7 = { let x: i7 = -60; x - 4 };"), -64);
    check_number(&opted("const GOAL: i7 = { let x: i7 = -21; x * 3 };"), -63);
    check_number(&opted("const GOAL: u3 = { let x = 6u3; x / 4 + x % 4 };"), 3);
    check_number(&opted("const GOAL: i7 = { let x: i7 = 63; !x };"), -64);
    check_number(&opted("const GOAL: u3 = { let x: u3 = 2; !x };"), 5);
    // A constant checks its own arithmetic.
    let constant = [
        ("const GOAL: u3 = { let x = 7u3; x + 1 };", "attempt to add with overflow at type `u3`"),
        (
            "const GOAL: u3 = { let x = 0u3; x - 1 };",
            "attempt to subtract with overflow at type `u3`",
        ),
        (
            "const GOAL: u3 = { let x = 3u3; x * 3 };",
            "attempt to multiply with overflow at type `u3`",
        ),
        ("const GOAL: i7 = { let x = 63i7; x + 1 };", "attempt to add with overflow at type `i7`"),
        (
            "const GOAL: i7 = { let x = -64i7; x - 1 };",
            "attempt to subtract with overflow at type `i7`",
        ),
        ("const GOAL: i7 = { let x = -64i7; -x };", "attempt to negate with overflow at type `i7`"),
        (
            "const GOAL: u3 = { let x = 1u3; x << 3 };",
            "attempt to shift left with overflow at type `u3`",
        ),
        (
            "const GOAL: i7 = { let x = 1i7; x >> 7 };",
            "attempt to shift right with overflow at type `i7`",
        ),
    ];
    for (body, message) in constant {
        check_fail(&opted(body), panic(message));
    }
    // A function wraps at the width without `overflow-checks`, and panics with them.
    let functions = [
        (
            "const fn f(a: u3, b: u3) -> u3 { a + b } const GOAL: u3 = f(7, 1);",
            0,
            "attempt to add with overflow at type `u3`",
        ),
        (
            "const fn f(a: u3, b: u3) -> u3 { a - b } const GOAL: u3 = f(0, 1);",
            7,
            "attempt to subtract with overflow at type `u3`",
        ),
        (
            "const fn f(a: u3, b: u3) -> u3 { a * b } const GOAL: u3 = f(3, 3);",
            1,
            "attempt to multiply with overflow at type `u3`",
        ),
        (
            "const fn f(a: i7, b: i7) -> i7 { a + b } const GOAL: i7 = f(63, 1);",
            -64,
            "attempt to add with overflow at type `i7`",
        ),
        (
            "const fn f(a: i7, b: i7) -> i7 { a - b } const GOAL: i7 = f(-64, 1);",
            63,
            "attempt to subtract with overflow at type `i7`",
        ),
        (
            "const fn f(a: i7, b: i7) -> i7 { a * b } const GOAL: i7 = f(-64, -1);",
            -64,
            "attempt to multiply with overflow at type `i7`",
        ),
        (
            "const fn f(a: i7) -> i7 { -a } const GOAL: i7 = f(-64);",
            -64,
            "attempt to negate with overflow at type `i7`",
        ),
        // Unchecked, a shift is by its amount mod N: 4 mod 3 is 1, and 13 mod 7 is 6.
        (
            "const fn f(a: u3, b: u32) -> u3 { a << b } const GOAL: u3 = f(3, 4);",
            6,
            "attempt to shift left with overflow at type `u3`",
        ),
        (
            "const fn f(a: i7, b: u32) -> i7 { a << b } const GOAL: i7 = f(1, 13);",
            -64,
            "attempt to shift left with overflow at type `i7`",
        ),
    ];
    for (body, wrapped, message) in functions {
        check_number(&opted(body), wrapped);
        check_fail(&checking(body), panic(message));
    }
    // Division overflows under both settings.
    for fixture in [opted, checking] {
        let body = "const fn f(a: i7, b: i7) -> i7 { a / b } const GOAL: i7 = f(-64, -1);";
        check_fail(&fixture(body), panic("attempt to divide with overflow at type `i7`"));
        let body = "const fn f(a: u3, b: u3) -> u3 { a % b } const GOAL: u3 = f(5, 0);";
        check_fail(
            &fixture(body),
            panic("attempt to calculate the remainder with a divisor of zero at type `u3`"),
        );
    }
}

/// `as` to an arbitrary width truncates to it and extends from it; from a float it saturates
/// at the width.
#[test]
fn arbitrary_ints_casts() {
    let opted = |body: &str| format!("#![blaze::arbitrary_ints]\n{body}");
    check_number(&opted("const GOAL: u3 = 13u8 as u3;"), 5);
    check_number(&opted("const GOAL: i3 = 7u8 as i3;"), -1);
    check_number(&opted("const GOAL: u8 = { let x: i3 = -1; x as u8 };"), 255);
    check_number(&opted("const GOAL: u3 = { let x: i7 = -3; x as u3 };"), 5);
    check_number(&opted("const GOAL: i7 = { let x: u3 = 7; x as i7 };"), 7);
    check_number(&opted("const GOAL: u3 = 9.5f64 as u3;"), 7);
    check_number(&opted("const GOAL: i7 = -1000.0f32 as i7;"), -64);
    check_number(&opted("const GOAL: i7 = 63.9f64 as i7;"), 63);
}

#[test]
fn floating_point() {
    check_number(
        r#"const GOAL: f128 = 2.0 + 3.0 * 5.5 - 8.;"#,
        "10.5".parse::<f128>().unwrap().to_bits() as i128,
    );
    check_number(
        r#"const GOAL: f128 = -90.0 + 36.0;"#,
        "-54.0".parse::<f128>().unwrap().to_bits() as i128,
    );
    check_number(
        r#"const GOAL: f64 = 2.0 + 3.0 * 5.5 - 8.;"#,
        i128::from_le_bytes(pad16(&f64::to_le_bytes(10.5), IsSigned::Yes)),
    );
    check_number(
        r#"const GOAL: f32 = 2.0 + 3.0 * 5.5 - 8.;"#,
        i128::from_le_bytes(pad16(&f32::to_le_bytes(10.5), IsSigned::Yes)),
    );
    check_number(
        r#"const GOAL: f32 = -90.0 + 36.0;"#,
        i128::from_le_bytes(pad16(&f32::to_le_bytes(-54.0), IsSigned::Yes)),
    );
    check_number(
        r#"const GOAL: f16 = 2.0 + 3.0 * 5.5 - 8.;"#,
        i128::from_le_bytes(pad16(
            &u16::try_from("10.5".parse::<f16>().unwrap().to_bits()).unwrap().to_le_bytes(),
            IsSigned::Yes,
        )),
    );
    check_number(
        r#"const GOAL: f16 = -90.0 + 36.0;"#,
        i128::from_le_bytes(pad16(
            &u16::try_from("-54.0".parse::<f16>().unwrap().to_bits()).unwrap().to_le_bytes(),
            IsSigned::Yes,
        )),
    );
}

#[test]
fn casts() {
    check_number(
        r#"
    //- minicore: sized
    const GOAL: usize = 12 as *const i32 as usize
        "#,
        12,
    );
    check_number(
        r#"
    //- minicore: coerce_unsized, index, slice
    const GOAL: i32 = {
        let a = [10, 20, 3, 15];
        let x: &[i32] = &a;
        let y: *const [i32] = x;
        let z = y as *const i32;
        unsafe { *z }
    };
        "#,
        10,
    );
    check_number(
        r#"
    //- minicore: coerce_unsized, index, slice
    const GOAL: i16 = {
        let a = &mut 5_i16;
        let z = a as *mut _;
        unsafe { *z }
    };
        "#,
        5,
    );
    check_number(
        r#"
    //- minicore: coerce_unsized, index, slice
    const GOAL: usize = {
        let a = &[10, 20, 30, 40] as &[i32];
        a.len()
    };
        "#,
        4,
    );
    check_number(
        r#"
    //- minicore: coerce_unsized, index, slice
    struct X {
        unsize_field: [u8],
    }

    const GOAL: usize = {
        let a = [10, 20, 3, 15];
        let x: &[i32] = &a;
        let x: *const [i32] = x;
        let x = x as *const [u8]; // slice fat pointer cast don't touch metadata
        let x = x as *const str;
        let x = x as *const X;
        let x = x as *const [i16];
        let x = x as *const X;
        let x = x as *const [u8];
        let w = unsafe { &*x };
        w.len()
    };
        "#,
        4,
    );
    check_number(
        r#"
    //- minicore: sized
    const GOAL: i32 = -12i8 as i32
        "#,
        -12,
    );
}

#[test]
fn floating_point_casts() {
    check_number(r#"const GOAL: usize = 12i32 as f32 as usize"#, 12);
    check_number(r#"const GOAL: i8 = -12i32 as f64 as i8"#, -12);
    check_number(r#"const GOAL: i32 = (-1ui8 as f32 + 2u64 as f32) as i32"#, 1);
    check_number(r#"const GOAL: i8 = (0./0.) as i8"#, 0);
    check_number(r#"const GOAL: i8 = (1./0.) as i8"#, 127);
    check_number(r#"const GOAL: i8 = (-1./0.) as i8"#, -128);
    check_number(r#"const GOAL: u8 = (1./0.) as u8"#, 255);
    check_number(r#"const GOAL: u8 = 256.0f32 as u8"#, 255);
    check_number(r#"const GOAL: u16 = 1e10f32 as u16"#, 65535);
    check_number(r#"const GOAL: i64 = 1e18f64 as f32 as i64"#, 999999984306749440);
}

#[test]
fn raw_pointer_equality() {
    check_number(
        r#"
        //- minicore: copy, eq
        const GOAL: bool = {
            let a = 2;
            let p1 = a as *const i32;
            let p2 = a as *const i32;
            p1 == p2
        };
        "#,
        1,
    );
}

#[test]
fn alignment() {
    check_answer(
        r#"
//- minicore: transmute
use core::mem::transmute;
const GOAL: usize = {
    let x: i64 = 2;
    transmute(&x)
}
        "#,
        |b, _| assert_eq!(b[0] % 8, 0),
    );
    check_answer(
        r#"
//- minicore: transmute
use core::mem::transmute;
static X: i64 = 12;
const GOAL: usize = transmute(&X);
        "#,
        |b, _| assert_eq!(b[0] % 8, 0),
    );
}

#[test]
fn locals() {
    check_number(
        r#"
    const GOAL: usize = {
        let a = 3 + 2;
        let b = a * a;
        b
    };
    "#,
        25,
    );
}

#[test]
fn references() {
    check_number(
        r#"
    const GOAL: usize = {
        let x = 3;
        let y = &mut x;
        *y = 5;
        x
    };
    "#,
        5,
    );
    check_number(
        r#"
    struct Foo(i32);
    impl Foo {
        fn method(&mut self, x: i32) {
            self.0 = 2 * self.0 + x;
        }
    }
    const GOAL: i32 = {
        let mut x = Foo(3);
        x.method(5);
        x.0
    };
    "#,
        11,
    );
}

#[test]
fn reference_autoderef() {
    check_number(
        r#"
    const GOAL: usize = {
        let x = 3;
        let y = &mut x;
        let y: &mut usize = &mut y;
        *y = 5;
        x
    };
    "#,
        5,
    );
    check_number(
        r#"
    const GOAL: usize = {
        let x = 3;
        let y = &&&&&&&x;
        let z: &usize = &y;
        *z
    };
    "#,
        3,
    );
    check_number(
        r#"
    struct Foo<T> { x: T }
    impl<T> Foo<T> {
        fn foo(&mut self) -> T { self.x }
    }
    fn f(i: &mut &mut Foo<Foo<i32>>) -> i32 {
        ((**i).x).foo()
    }
    fn g(i: Foo<Foo<i32>>) -> i32 {
        i.x.foo()
    }
    const GOAL: i32 = f(&mut &mut Foo { x: Foo { x: 3 } }) + g(Foo { x: Foo { x: 5 } });
    "#,
        8,
    );
}

#[test]
fn overloaded_deref() {
    check_number(
        r#"
    //- minicore: deref_mut
    struct Foo;

    impl core::ops::Deref for Foo {
        type Target = i32;
        fn deref(&self) -> &i32 {
            &5
        }
    }

    const GOAL: i32 = {
        let x = Foo;
        let y = &*x;
        *y + *x
    };
    "#,
        10,
    );
}

#[test]
fn overloaded_deref_autoref() {
    check_number(
        r#"
    //- minicore: deref_mut
    struct Foo;
    struct Bar;

    impl core::ops::Deref for Foo {
        type Target = Bar;
        fn deref(&self) -> &Bar {
            &Bar
        }
    }

    impl Bar {
        fn method(&self) -> i32 {
            5
        }
    }

    const GOAL: i32 = Foo.method();
    "#,
        5,
    );
}

#[test]
fn overloaded_index() {
    check_number(
        r#"
    //- minicore: index
    struct Foo(i32, i32);

    impl core::ops::Index<usize> for Foo {
        type Output = i32;
        fn index(&self, index: usize) -> &i32 {
            if index == 7 {
                &700
            } else {
                &1000
            }
        }
    }

    impl core::ops::IndexMut<usize> for Foo {
        fn index_mut(&mut self, index: usize) -> &mut i32 {
            if index == 7 {
                &mut self.0
            } else {
                &mut self.1
            }
        }
    }

    const GOAL: i32 = {
        (Foo(7, 10)[2]) + (Foo(7, 10)[7]) + (*&Foo(7, 10)[2]) + (*&Foo(7, 10)[7])
            + (*&mut Foo(7, 10)[2]) + (*&mut Foo(7, 10)[7])
    };
    "#,
        3417,
    );
}

#[test]
fn overloaded_binop() {
    check_number(
        r#"
    //- minicore: add
    enum Color {
        Red,
        Green,
        Yellow,
    }

    use Color::*;

    impl core::ops::Add for Color {
        type Output = Color;
        fn add(self, rhs: Color) -> Self::Output {
            Yellow
        }
    }

    impl core::ops::AddAssign for Color {
        fn add_assign(&mut self, rhs: Color) {
            *self = Red;
        }
    }

    const GOAL: bool = {
        let x = Red + Green;
        let mut y = Green;
        y += x;
        x == Yellow && y == Red && Red + Green == Yellow && Red + Red == Yellow && Yellow + Green == Yellow
    };
    "#,
        1,
    );
    check_number(
        r#"
    //- minicore: add
    impl core::ops::Add for usize {
        type Output = usize;
        fn add(self, rhs: usize) -> Self::Output {
            self + rhs
        }
    }

    impl core::ops::AddAssign for usize {
        fn add_assign(&mut self, rhs: usize) {
            *self += rhs;
        }
    }

    #[lang = "shl"]
    pub trait Shl<Rhs = Self> {
        type Output;

        fn shl(self, rhs: Rhs) -> Self::Output;
    }

    impl Shl<u8> for usize {
        type Output = usize;

        fn shl(self, rhs: u8) -> Self::Output {
            self << rhs
        }
    }

    const GOAL: usize = {
        let mut x = 10;
        x += 20;
        2 + 2 + (x << 1u8)
    };"#,
        64,
    );
}

#[test]
fn function_call() {
    check_number(
        r#"
    const fn f(x: usize) -> usize {
        2 * x + 5
    }
    const GOAL: usize = f(3);
    "#,
        11,
    );
    check_number(
        r#"
    const fn add(x: usize, y: usize) -> usize {
        x + y
    }
    const GOAL: usize = add(add(1, 2), add(3, add(4, 5)));
    "#,
        15,
    );
}

#[test]
fn trait_basic() {
    check_number(
        r#"
    trait Foo {
        fn f(&self) -> u8;
    }

    impl Foo for u8 {
        fn f(&self) -> u8 {
            *self + 33
        }
    }

    const GOAL: u8 = {
        let x = 3;
        Foo::f(&x)
    };
    "#,
        36,
    );
}

#[test]
fn trait_method() {
    check_number(
        r#"
    trait Foo {
        fn f(&self) -> u8;
    }

    impl Foo for u8 {
        fn f(&self) -> u8 {
            *self + 33
        }
    }

    const GOAL: u8 = {
        let x = 3;
        x.f()
    };
    "#,
        36,
    );
}

#[test]
fn trait_method_inside_block() {
    check_number(
        r#"
trait Twait {
    fn a(&self) -> i32;
}

fn outer() -> impl Twait {
    struct Stwuct;

    impl Twait for Stwuct {
        fn a(&self) -> i32 {
            5
        }
    }
    fn f() -> impl Twait {
        let s = Stwuct;
        s
    }
    f()
}

const GOAL: i32 = outer().a();
        "#,
        5,
    );
}

#[test]
fn generic_fn() {
    check_number(
        r#"
    trait Foo {
        fn f(&self) -> u8;
    }

    impl Foo for () {
        fn f(&self) -> u8 {
            0
        }
    }

    struct Succ<S>(S);

    impl<T: Foo> Foo for Succ<T> {
        fn f(&self) -> u8 {
            self.0.f() + 1
        }
    }

    const GOAL: u8 = Succ(Succ(())).f();
    "#,
        2,
    );
    check_number(
        r#"
    trait Foo {
        fn f(&self) -> u8;
    }

    impl Foo for u8 {
        fn f(&self) -> u8 {
            *self + 33
        }
    }

    fn foof<T: Foo>(x: T, y: T) -> u8 {
        x.f() + y.f()
    }

    const GOAL: u8 = foof(2, 5);
    "#,
        73,
    );
    check_number(
        r#"
    fn bar<A, B>(a: A, b: B) -> B {
        b
    }
        const GOAL: u8 = bar("hello", 12);
        "#,
        12,
    );
    check_number(
        r#"
        const fn y<T>(b: T) -> (T, ) {
            let alloc = b;
            (alloc, )
        }
        const GOAL: u8 = y(2).0;
        "#,
        2,
    );
    check_number(
        r#"
    //- minicore: coerce_unsized, index, slice
    fn bar<A, B>(a: A, b: B) -> B {
        b
    }
    fn foo<T>(x: [T; 2]) -> T {
        bar(x[0], x[1])
    }

    const GOAL: u8 = foo([2, 5]);
    "#,
        5,
    );
}

#[test]
fn impl_trait() {
    check_number(
        r#"
    trait Foo {
        fn f(&self) -> u8;
    }

    impl Foo for u8 {
        fn f(&self) -> u8 {
            *self + 33
        }
    }

    fn foof(x: impl Foo, y: impl Foo) -> impl Foo {
        x.f() + y.f()
    }

    const GOAL: u8 = foof(2, 5).f();
    "#,
        106,
    );
    check_number(
        r#"
        struct Foo<T>(T, T, (T, T));
        trait S {
            fn sum(&self) -> i64;
        }
        impl S for i64 {
            fn sum(&self) -> i64 {
                *self
            }
        }
        impl<T: S> S for Foo<T> {
            fn sum(&self) -> i64 {
                self.0.sum() + self.1.sum() + self.2 .0.sum() + self.2 .1.sum()
            }
        }

        fn foo() -> Foo<impl S> {
            Foo(
                Foo(1i64, 2, (3, 4)),
                Foo(5, 6, (7, 8)),
                (
                    Foo(9, 10, (11, 12)),
                    Foo(13, 14, (15, 16)),
                ),
            )
        }
        const GOAL: i64 = foo().sum();
    "#,
        136,
    );
}

#[test]
fn ifs() {
    check_number(
        r#"
    const fn f(b: bool) -> u8 {
        if b { 1 } else { 10 }
    }

    const GOAL: u8 = f(true) + f(true) + f(false);
        "#,
        12,
    );
    check_number(
        r#"
    const fn max(a: i32, b: i32) -> i32 {
        if a < b { b } else { a }
    }

    const GOAL: i32 = max(max(1, max(10, 3)), 0-122);
        "#,
        10,
    );

    check_number(
        r#"
    const fn max(a: &i32, b: &i32) -> &i32 {
        if *a < *b { b } else { a }
    }

    const GOAL: i32 = *max(max(&1, max(&10, &3)), &5);
        "#,
        10,
    );
}

#[test]
fn loops() {
    check_number(
        r#"
    //- minicore: add, builtin_impls
    const GOAL: u8 = {
        let mut x = 0;
        loop {
            x = x + 1;
            while true {
                break;
            }
            x = x + 1;
            if x == 2 {
                continue;
            }
            break;
        }
        x
    };
        "#,
        4,
    );
    check_number(
        r#"
    //- minicore: add, builtin_impls
    const GOAL: u8 = {
        let mut x = 0;
        loop {
            x = x + 1;
            if x == 5 {
                break x + 2;
            }
        }
    };
        "#,
        7,
    );
    check_number(
        r#"
    //- minicore: add, builtin_impls
    const GOAL: u8 = {
        'a: loop {
            let x: u8 = 'b: loop {
                let x = 'c: loop {
                    let x = 'd: loop {
                        let x: u8 = 'e: loop {
                            break 'd 1;
                        };
                        break 2 + x;
                    };
                    break 3 + x;
                };
                break 'a 4 + x;
            };
            break 5 + x;
        }
    };
        "#,
        8,
    );
    check_number(
        r#"
    //- minicore: add, builtin_impls
    const GOAL: u8 = {
        let mut x = 0;
        'a: loop {
            'b: loop {
                'c: while x < 20 {
                    'd: while x < 5 {
                        'e: loop {
                            x += 1;
                            continue 'c;
                        };
                    };
                    x += 1;
                };
                break 'a;
            };
        }
        x
    };
        "#,
        20,
    );
}

#[test]
fn for_loops() {
    check_number(
        r#"
    //- minicore: iterator

    struct Range {
        start: u8,
        end: u8,
    }

    impl Iterator for Range {
        type Item = u8;
        fn next(&mut self) -> Option<u8> {
            if self.start >= self.end {
                None
            } else {
                let r = self.start;
                self.start = self.start + 1;
                Some(r)
            }
        }
    }

    const GOAL: u8 = {
        let mut sum = 0;
        let ar = Range { start: 1, end: 11 };
        for i in ar {
            sum = sum + i;
        }
        sum
    };
        "#,
        55,
    );
}

#[test]
fn ranges() {
    check_number(
        r#"
    //- minicore: range
    const GOAL: i32 = (1..2).start + (20..10).end + (10000..).start + (..100000).end
        + (..=1000000).end;
        "#,
        1110011,
    );
}

#[test]
fn recursion() {
    check_number(
        r#"
    const fn fact(k: i32) -> i32 {
        if k > 0 { fact(k - 1) * k } else { 1 }
    }

    const GOAL: i32 = fact(5);
        "#,
        120,
    );
}

#[test]
fn recursion_past_the_limit() {
    check_number(
        r#"
    const fn depth(k: i32) -> i32 { if k > 0 { depth(k - 1) + 1 } else { 0 } }
    const GOAL: i32 = depth(100);
        "#,
        100,
    );
    check_fail(
        r#"
    const fn f<T>(x: T) { f(x); }
    const GOAL: () = f(1);
        "#,
        |e| matches!(e, ConstEvalError::MirEvalError(MirEvalError::StackOverflow)),
    );
    check_number(
        r#"
    #![recursion_limit = "300"]
    const fn depth(k: i32) -> i32 { if k > 0 { depth(k - 1) + 1 } else { 0 } }
    const GOAL: i32 = depth(200);
        "#,
        200,
    );
}

#[test]
fn structs() {
    check_number(
        r#"
        struct Point {
            x: i32,
            y: i32,
        }

        const GOAL: i32 = {
            let p = Point { x: 5, y: 2 };
            let y = 1;
            let x = 3;
            let q = Point { y, x };
            p.x + p.y + p.x + q.y + q.y + q.x
        };
        "#,
        17,
    );
    check_number(
        r#"
        struct Point {
            x: i32,
            y: i32,
        }

        const GOAL: i32 = {
            let p = Point { x: 5, y: 2 };
            let p2 = Point { x: 3, ..p };
            p.x * 1000 + p.y * 100 + p2.x * 10 + p2.y
        };
        "#,
        5232,
    );
    check_number(
        r#"
        struct Point {
            x: i32,
            y: i32,
        }

        const GOAL: i32 = {
            let p = Point { x: 5, y: 2 };
            let Point { x, y } = p;
            let Point { x: x2, .. } = p;
            let Point { y: y2, .. } = p;
            x * 1000 + y * 100 + x2 * 10 + y2
        };
        "#,
        5252,
    );
}

#[test]
fn unions() {
    check_number(
        r#"
        union U {
            f1: i64,
            f2: (i32, i32),
        }

        const GOAL: i32 = {
            let p = U { f1: 0x0123ABCD0123DCBA };
            let p = unsafe { p.f2 };
            p.0 + p.1 + p.1
        };
        "#,
        0x0123ABCD * 2 + 0x0123DCBA,
    );
}

#[test]
fn tuples() {
    check_number(
        r#"
    const GOAL: u8 = {
        let a = (10, 20, 3, 15);
        a.1
    };
        "#,
        20,
    );
    check_number(
        r#"
    const GOAL: u8 = {
        let mut a = (10, 20, 3, 15);
        a.1 = 2;
        a.0 + a.1 + a.2 + a.3
    };
        "#,
        30,
    );
    check_number(
        r#"
    struct TupleLike(i32, i64, u8, u16);
    const GOAL: i64 = {
        let a = TupleLike(10, 20, 3, 15);
        let TupleLike(b, .., c) = a;
        a.1 * 100 + b as i64 + c as i64
    };
        "#,
        2025,
    );
    check_number(
        r#"
    const GOAL: u8 = {
        match (&(2 + 2), &4) {
            (left_val, right_val) => {
                if !(*left_val == *right_val) {
                    2
                } else {
                    5
                }
            }
        }
    };
        "#,
        5,
    );
}

#[test]
fn path_pattern_matching() {
    check_number(
        r#"
    enum Season {
        Spring,
        Summer,
        Fall,
        Winter,
    }

    use Season::*;

    const MY_SEASON: Season = Summer;

    impl Season {
        const FALL: Season = Fall;
    }

    const fn f(x: Season) -> i32 {
        match x {
            Spring => 1,
            MY_SEASON => 2,
            Season::FALL => 3,
            Winter => 4,
        }
    }
    const GOAL: i32 = f(Spring) + 10 * f(Summer) + 100 * f(Fall) + 1000 * f(Winter);
        "#,
        4321,
    );
}

#[test]
fn pattern_matching_literal() {
    check_number(
        r#"
    const fn f(x: i32) -> i32 {
        match x {
            -1 => 1,
            1 => 10,
            _ => 100,
        }
    }
    const GOAL: i32 = f(-1) + f(1) + f(0) + f(-5);
        "#,
        211,
    );
    check_number(
        r#"
    const fn f(x: &str) -> i32 {
        match x {
            "f" => 1,
            "foo" => 10,
            "" => 100,
            "bar" => 1000,
            _ => 10000,
        }
    }
    const GOAL: i32 = f("f") + f("foo") * 2 + f("") * 3 + f("bar") * 4;
        "#,
        4321,
    );
}

#[test]
fn pattern_matching_range() {
    check_number(
        r#"
    pub const L: i32 = 6;
    mod x {
        pub const R: i32 = 100;
    }
    const fn f(x: i32) -> i32 {
        match x {
            -1..=5 => x * 10,
            L..=x::R => x * 100,
            _ => x,
        }
    }
    const GOAL: i32 = f(-1) + f(2) + f(100) + f(-2) + f(1000);
        "#,
        11008,
    );
}

#[test]
fn pattern_matching_slice() {
    check_number(
        r#"
    //- minicore: slice, index, coerce_unsized, copy
    const fn f(x: &[usize]) -> usize {
        match x {
            [a, b @ .., c, d] => *a + b.len() + *c + *d,
        }
    }
    const GOAL: usize = f(&[10, 20, 3, 15, 1000, 60, 16]);
        "#,
        10 + 4 + 60 + 16,
    );
    check_number(
        r#"
    //- minicore: slice, index, coerce_unsized, copy
    const fn f(x: &[usize]) -> usize {
        match x {
            [] => 0,
            [a] => *a,
            &[a, b] => a + b,
            [a, b @ .., c, d] => *a + b.len() + *c + *d,
        }
    }
    const GOAL: usize = f(&[]) + f(&[10]) + f(&[100, 100])
        + f(&[1000, 1000, 1000]) + f(&[10000, 57, 34, 46, 10000, 10000]);
        "#,
        33213,
    );
    check_number(
        r#"
    //- minicore: slice, index, coerce_unsized, copy
    const fn f(mut slice: &[u32]) -> usize {
        slice = match slice {
            [0, rest @ ..] | rest => rest,
        };
        slice.len()
    }
    const GOAL: usize = f(&[]) + f(&[10]) + f(&[0, 100])
        + f(&[1000, 1000, 1000]) + f(&[0, 57, 34, 46, 10000, 10000]);
        "#,
        10,
    );
}

#[test]
fn pattern_matching_ergonomics() {
    check_number(
        r#"
    const fn f(x: &(u8, u8)) -> u8 {
        match x {
            (a, b) => *a + *b
        }
    }
    const GOAL: u8 = f(&(2, 3));
        "#,
        5,
    );
    check_number(
        r#"
    const GOAL: u8 = {
        let a = &(2, 3);
        let &(x, y) = a;
        x + y
    };
        "#,
        5,
    );
}

#[test]
fn destructing_assignment() {
    check_number(
        r#"
    //- minicore: add, builtin_impls
    const fn f(i: &mut u8) -> &mut u8 {
        *i += 1;
        i
    }
    const GOAL: u8 = {
        let mut i = 4;
        _ = f(&mut i);
        i
    };
        "#,
        5,
    );
    check_number(
        r#"
    const GOAL: u8 = {
        let (mut a, mut b) = (2, 5);
        (a, b) = (b, a);
        a * 10 + b
    };
        "#,
        52,
    );
    check_number(
        r#"
    struct Point { x: i32, y: i32 }
    const GOAL: i32 = {
        let mut p = Point { x: 5, y: 6 };
        (p.x, _) = (p.y, p.x);
        p.x * 10 + p.y
    };
        "#,
        66,
    );
}

#[test]
fn let_else() {
    check_number(
        r#"
    const fn f(x: &(u8, u8)) -> u8 {
        let (a, b) = x;
        *a + *b
    }
    const GOAL: u8 = f(&(2, 3));
        "#,
        5,
    );
    check_number(
        r#"
    enum SingleVariant {
        Var(u8, u8),
    }
    const fn f(x: &&&&&SingleVariant) -> u8 {
        let SingleVariant::Var(a, b) = x;
        *a + *b
    }
    const GOAL: u8 = f(&&&&&SingleVariant::Var(2, 3));
        "#,
        5,
    );
    check_number(
        r#"
    //- minicore: option
    const fn f(x: Option<i32>) -> i32 {
        let Some(x) = x else { return 10 };
        2 * x
    }
    const GOAL: i32 = f(Some(1000)) + f(None);
        "#,
        2010,
    );
}

#[test]
fn function_param_patterns() {
    check_number(
        r#"
    const fn f((a, b): &(u8, u8)) -> u8 {
        *a + *b
    }
    const GOAL: u8 = f(&(2, 3));
        "#,
        5,
    );
    check_number(
        r#"
    const fn f(c @ (a, b): &(u8, u8)) -> u8 {
        *a + *b + c.0 + (*c).1
    }
    const GOAL: u8 = f(&(2, 3));
        "#,
        10,
    );
    check_number(
        r#"
    const fn f(ref a: u8) -> u8 {
        *a
    }
    const GOAL: u8 = f(2);
        "#,
        2,
    );
    check_number(
        r#"
    struct Foo(u8);
    impl Foo {
        const fn f(&self, (a, b): &(u8, u8)) -> u8 {
            self.0 + *a + *b
        }
    }
    const GOAL: u8 = Foo(4).f(&(2, 3));
        "#,
        9,
    );
}

#[test]
fn match_guards() {
    check_number(
        r#"
    //- minicore: option
    fn f(x: Option<i32>) -> i32 {
        match x {
            y if let Some(42) = y => 42000,
            Some(y) => y,
            None => 10
        }
    }
    const GOAL: i32 = f(Some(42)) + f(Some(2)) + f(None);
        "#,
        42012,
    );
}

#[test]
fn result_layout_niche_optimization() {
    check_number(
        r#"
    //- minicore: option, result
    const GOAL: i32 = match Some(2).ok_or(Some(2)) {
        Ok(x) => x,
        Err(_) => 1000,
    };
        "#,
        2,
    );
    check_number(
        r#"
    //- minicore: result
    pub enum AlignmentEnum64 {
        _Align1Shl0 = 1 << 0,
        _Align1Shl1 = 1 << 1,
        _Align1Shl2 = 1 << 2,
        _Align1Shl3 = 1 << 3,
        _Align1Shl4 = 1 << 4,
        _Align1Shl5 = 1 << 5,
    }
    const GOAL: Result<AlignmentEnum64, ()> = {
        let align = Err(());
        align
    };
    "#,
        0, // It is 0 since result is niche encoded and 1 is valid for `AlignmentEnum64`
    );
    check_number(
        r#"
    //- minicore: result
    pub enum AlignmentEnum64 {
        _Align1Shl0 = 1 << 0,
        _Align1Shl1 = 1 << 1,
        _Align1Shl2 = 1 << 2,
        _Align1Shl3 = 1 << 3,
        _Align1Shl4 = 1 << 4,
        _Align1Shl5 = 1 << 5,
    }
    const GOAL: i32 = {
        let align = Ok::<_, ()>(AlignmentEnum64::_Align1Shl0);
        match align {
            Ok(_) => 2,
            Err(_) => 1,
        }
    };
    "#,
        2,
    );
}

#[test]
fn options() {
    check_number(
        r#"
    //- minicore: option, add, builtin_impls
    const GOAL: u8 = {
        let x = Some(2);
        match x {
            Some(y) => 2 + y,
            _ => 10,
        }
    };
        "#,
        4,
    );
    check_number(
        r#"
    //- minicore: option, add, builtin_impls
    fn f(x: Option<Option<i32>>) -> i32 {
        if let Some(y) = x && let Some(z) = y {
            z
        } else if let Some(y) = x {
            1
        } else {
            0
        }
    }
    const GOAL: i32 = f(Some(Some(10))) + f(Some(None)) + f(None);
        "#,
        11,
    );
    check_number(
        r#"
    //- minicore: option, add, builtin_impls
    const GOAL: u8 = {
        let x: Option<u8> = None;
        match x {
            Some(y) => 2 + y,
            _ => 10,
        }
    };
        "#,
        10,
    );
    check_number(
        r#"
    //- minicore: option
    const GOAL: Option<&u8> = None;
        "#,
        0,
    );
}

#[test]
fn from_trait() {
    check_number(
        r#"
    //- minicore: from
    struct E1(i32);
    struct E2(i32);

    impl From<E1> for E2 {
        fn from(E1(x): E1) -> Self {
            E2(1000 * x)
        }
    }
    const GOAL: i32 = {
        let x: E2 = E1(2).into();
        x.0
    };
    "#,
        2000,
    );
}

#[test]
fn closure_clone() {
    check_number(
        r#"
//- minicore: clone, fn
struct S(u8);

impl Clone for S(u8) {
    fn clone(&self) -> S {
        S(self.0 + 5)
    }
}

const GOAL: u8 = {
    let s = S(3);
    let cl = move || s;
    let cl = cl.clone();
    cl().0
}
    "#,
        8,
    );
}

#[test]
#[ignore = "builtin derive macros are currently not working with MIR eval"]
fn builtin_derive_macro() {
    check_number(
        r#"
    //- minicore: clone, derive, builtin_impls
    #[derive(Clone)]
    enum Z {
        Foo(Y),
        Bar,
    }
    #[derive(Clone)]
    struct X(i32, Z, i64);
    #[derive(Clone)]
    struct Y {
        field1: i32,
        field2: ((i32, u8), i64),
    }

    const GOAL: u8 = {
        let x = X(2, Z::Foo(Y { field1: 4, field2: ((32, 5), 12) }), 8);
        let x = x.clone();
        let Z::Foo(t) = x.1;
        t.field2.0 .1
    };
    "#,
        5,
    );
    check_number(
        r#"
//- minicore: default, derive, builtin_impls
#[derive(Default)]
struct X(i32, Y, i64);
#[derive(Default)]
struct Y {
    field1: i32,
    field2: u8,
}

const GOAL: u8 = {
    let x = X::default();
    x.1.field2
};
"#,
        0,
    );
}

#[test]
fn try_operator() {
    check_number(
        r#"
    //- minicore: option, try
    const fn f(x: Option<i32>, y: Option<i32>) -> Option<i32> {
        Some(x? * y?)
    }
    const fn g(x: Option<i32>, y: Option<i32>) -> i32 {
        match f(x, y) {
            Some(k) => k,
            None => 5,
        }
    }
    const GOAL: i32 = g(Some(10), Some(20)) + g(Some(30), None) + g(None, Some(40)) + g(None, None);
        "#,
        215,
    );
    check_number(
        r#"
    //- minicore: result, try, from
    struct E1(i32);
    struct E2(i32);

    impl From<E1> for E2 {
        fn from(E1(x): E1) -> Self {
            E2(1000 * x)
        }
    }

    const fn f(x: Result<i32, E1>) -> Result<i32, E2> {
        Ok(x? * 10)
    }
    const fn g(x: Result<i32, E1>) -> i32 {
        match f(x) {
            Ok(k) => 7 * k,
            Err(E2(k)) => 5 * k,
        }
    }
    const GOAL: i32 = g(Ok(2)) + g(Err(E1(3)));
        "#,
        15140,
    );
}

#[test]
fn try_block() {
    check_number(
        r#"
    //- minicore: option, try
    const fn g(x: Option<i32>, y: Option<i32>) -> i32 {
        let r = try { x? * y? };
        match r {
            Some(k) => k,
            None => 5,
        }
    }
    const GOAL: i32 = g(Some(10), Some(20)) + g(Some(30), None) + g(None, Some(40)) + g(None, None);
        "#,
        215,
    );
}

#[test]
fn closures() {
    check_number(
        r#"
    //- minicore: fn, copy
    const GOAL: i32 = {
        let y = 5;
        let c = |x| x + y;
        c(2)
    };
        "#,
        7,
    );
    check_number(
        r#"
    //- minicore: fn, copy
    const GOAL: i32 = {
        let y = 5;
        let c = |(a, b): &(i32, i32)| *a + *b + y;
        c(&(2, 3))
    };
        "#,
        10,
    );
    check_number(
        r#"
    //- minicore: fn, copy
    const GOAL: i32 = {
        let mut y = 5;
        let c = |x| {
            y = y + x;
        };
        c(2);
        c(3);
        y
    };
        "#,
        10,
    );
    check_number(
        r#"
    //- minicore: fn, copy
    const GOAL: i32 = {
        let c: fn(i32) -> i32 = |x| 2 * x;
        c(2) + c(10)
    };
        "#,
        24,
    );
    check_number(
        r#"
    //- minicore: fn, copy
    struct X(i32);
    impl X {
        fn mult(&mut self, n: i32) {
            self.0 = self.0 * n
        }
    }
    const GOAL: i32 = {
        let x = X(1);
        let c = || {
            x.mult(2);
            || {
                x.mult(3);
                || {
                    || {
                        x.mult(4);
                        || {
                            x.mult(x.0);
                            || {
                                x.0
                            }
                        }
                    }
                }
            }
        };
        let r = c()()()()()();
        r + x.0
    };
        "#,
        24 * 24 * 2,
    );
}

#[test]
fn manual_fn_trait_impl() {
    check_number(
        r#"
//- minicore: fn, copy
struct S(i32);

impl FnOnce<(i32, i32)> for S {
    type Output = i32;

    extern "rust-call" fn call_once(self, arg: (i32, i32)) -> i32 {
        arg.0 + arg.1 + self.0
    }
}

const GOAL: i32 = {
    let s = S(1);
    s(2, 3)
};
"#,
        6,
    );
}

#[test]
fn closure_capture_unsized_type() {
    check_number(
        r#"
    //- minicore: fn, copy, slice, index, coerce_unsized, sized
    fn f<T: A>(x: &<T as A>::Ty) -> &<T as A>::Ty {
        let c = || &*x;
        c()
    }

    trait A {
        type Ty: ?Sized;
    }

    impl A for i32 {
        type Ty = [u8];
    }

    const GOAL: u8 = {
        let k: &[u8] = &[1, 2, 3];
        let k = f::<i32>(k);
        k[0] + k[1] + k[2]
    };
    "#,
        6,
    );
}

#[test]
fn closure_and_impl_fn() {
    check_number(
        r#"
    //- minicore: fn, copy
    fn closure_wrapper<F: FnOnce() -> i32>(c: F) -> impl FnOnce() -> F {
        || c
    }

    const GOAL: i32 = {
        let y = 5;
        let c = closure_wrapper(|| y);
        c()()
    };
        "#,
        5,
    );
    check_number(
        r#"
    //- minicore: fn, copy
    fn f<T, F: Fn() -> T>(t: F) -> impl Fn() -> T {
        move || t()
    }

    const GOAL: i32 = f(|| 2)();
        "#,
        2,
    );
}

#[test]
fn or_pattern() {
    check_number(
        r#"
    const GOAL: u8 = {
        let (a | a) = 2;
        a
    };
        "#,
        2,
    );
    check_number(
        r#"
    //- minicore: option
    const fn f(x: Option<i32>) -> i32 {
        let (Some(a) | Some(a)) = x else { return 2; };
        a
    }
    const GOAL: i32 = f(Some(10)) + f(None);
        "#,
        12,
    );
    check_number(
        r#"
    //- minicore: option
    const fn f(x: Option<i32>, y: Option<i32>) -> i32 {
        match (x, y) {
            (Some(x), Some(y)) => x * y,
            (Some(a), _) | (_, Some(a)) => a,
            _ => 10,
        }
    }
    const GOAL: i32 = f(Some(10), Some(20)) + f(Some(30), None) + f(None, Some(40)) + f(None, None);
        "#,
        280,
    );
}

#[test]
fn function_pointer_in_constants() {
    check_number(
        r#"
    struct Foo {
        f: fn(u8) -> u8,
    }
    const FOO: Foo = Foo { f: add2 };
    fn add2(x: u8) -> u8 {
        x + 2
    }
    const GOAL: u8 = (FOO.f)(3);
        "#,
        5,
    );
}

#[test]
fn function_pointer_and_niche_optimization() {
    check_number(
        r#"
    //- minicore: option
    const GOAL: i32 = {
        let f: fn(i32) -> i32 = |x| x + 2;
        let init = Some(f);
        match init {
            Some(t) => t(3),
            None => 222,
        }
    };
        "#,
        5,
    );
}

#[test]
fn function_pointer() {
    check_number(
        r#"
    fn add2(x: u8) -> u8 {
        x + 2
    }
    const GOAL: u8 = {
        let plus2 = add2;
        plus2(3)
    };
        "#,
        5,
    );
    check_number(
        r#"
    fn add2(x: u8) -> u8 {
        x + 2
    }
    const GOAL: u8 = {
        let plus2: fn(u8) -> u8 = add2;
        plus2(3)
    };
        "#,
        5,
    );
    check_number(
        r#"
    //- minicore: sized
    fn add2(x: u8) -> u8 {
        x + 2
    }
    const GOAL: u8 = {
        let plus2 = add2 as fn(u8) -> u8;
        plus2(3)
    };
        "#,
        5,
    );
    check_number(
        r#"
    //- minicore: coerce_unsized, index, slice
    fn add2(x: u8) -> u8 {
        x + 2
    }
    fn mult3(x: u8) -> u8 {
        x * 3
    }
    const GOAL: u8 = {
        let x = [add2, mult3];
        x[0](1) + x[1](5)
    };
        "#,
        18,
    );
}

#[test]
fn enum_variant_as_function() {
    check_number(
        r#"
    //- minicore: option
    const GOAL: u8 = {
        let f = Some;
        f(3).unwrap_or(2)
    };
        "#,
        3,
    );
    check_number(
        r#"
    //- minicore: option
    const GOAL: u8 = {
        let f: fn(u8) -> Option<u8> = Some;
        f(3).unwrap_or(2)
    };
        "#,
        3,
    );
    check_number(
        r#"
    //- minicore: coerce_unsized, index, slice
    enum Foo {
        Add2(u8),
        Mult3(u8),
    }
    use Foo::*;
    const fn f(x: Foo) -> u8 {
        match x {
            Add2(x) => x + 2,
            Mult3(x) => x * 3,
        }
    }
    const GOAL: u8 = {
        let x = [Add2, Mult3];
        f(x[0](1)) + f(x[1](5))
    };
        "#,
        18,
    );
}

#[test]
fn function_traits() {
    check_number(
        r#"
    //- minicore: fn
    fn add2(x: u8) -> u8 {
        x + 2
    }
    fn call(f: impl Fn(u8) -> u8, x: u8) -> u8 {
        f(x)
    }
    fn call_mut(mut f: impl FnMut(u8) -> u8, x: u8) -> u8 {
        f(x)
    }
    fn call_once(f: impl FnOnce(u8) -> u8, x: u8) -> u8 {
        f(x)
    }
    const GOAL: u8 = call(add2, 3) + call_mut(add2, 3) + call_once(add2, 3);
        "#,
        15,
    );
    check_number(
        r#"
    //- minicore: coerce_unsized, fn, dispatch_from_dyn
    fn add2(x: u8) -> u8 {
        x + 2
    }
    fn call(f: &dyn Fn(u8) -> u8, x: u8) -> u8 {
        f(x)
    }
    fn call_mut(f: &mut dyn FnMut(u8) -> u8, x: u8) -> u8 {
        f(x)
    }
    const GOAL: u8 = call(&add2, 3) + call_mut(&mut add2, 3);
        "#,
        10,
    );
    check_number(
        r#"
    //- minicore: fn
    fn add2(x: u8) -> u8 {
        x + 2
    }
    fn call(f: impl Fn(u8) -> u8, x: u8) -> u8 {
        f(x)
    }
    fn call_mut(mut f: impl FnMut(u8) -> u8, x: u8) -> u8 {
        f(x)
    }
    fn call_once(f: impl FnOnce(u8) -> u8, x: u8) -> u8 {
        f(x)
    }
    const GOAL: u8 = {
        let add2: fn(u8) -> u8 = add2;
        call(add2, 3) + call_mut(add2, 3) + call_once(add2, 3)
    };
        "#,
        15,
    );
    check_number(
        r#"
    //- minicore: fn
    fn add2(x: u8) -> u8 {
        x + 2
    }
    fn call(f: &&&&&impl Fn(u8) -> u8, x: u8) -> u8 {
        f(x)
    }
    const GOAL: u8 = call(&&&&&add2, 3);
        "#,
        5,
    );
}

#[test]
fn dyn_trait() {
    check_number(
        r#"
    //- minicore: coerce_unsized, index, slice, dispatch_from_dyn
    trait Foo {
        fn foo(&self) -> u8 { 10 }
    }
    struct S1;
    struct S2;
    struct S3;
    impl Foo for S1 {
        fn foo(&self) -> u8 { 1 }
    }
    impl Foo for S2 {
        fn foo(&self) -> u8 { 2 }
    }
    impl Foo for S3 {}
    const GOAL: u8 = {
        let x: &[&dyn Foo] = &[&S1, &S2, &S3];
        x[0].foo() + x[1].foo() + x[2].foo()
    };
        "#,
        13,
    );
    check_number(
        r#"
    //- minicore: coerce_unsized, index, slice, dispatch_from_dyn
    trait Foo {
        fn foo(&self) -> i32 { 10 }
    }
    trait Bar {
        fn bar(&self) -> i32 { 20 }
    }

    struct S;
    impl Foo for S {
        fn foo(&self) -> i32 { 200 }
    }
    impl Bar for dyn Foo {
        fn bar(&self) -> i32 { 700 }
    }
    const GOAL: i32 = {
        let x: &dyn Foo = &S;
        x.bar() + x.foo()
    };
        "#,
        900,
    );
    check_number(
        r#"
    //- minicore: coerce_unsized, index, slice, dispatch_from_dyn
    trait A {
        fn x(&self) -> i32;
    }

    trait B: A {}

    impl A for i32 {
        fn x(&self) -> i32 {
            5
        }
    }

    impl B for i32 {

    }

    const fn f(x: &dyn B) -> i32 {
        x.x()
    }

    const GOAL: i32 = f(&2i32);
        "#,
        5,
    );
}

#[test]
fn coerce_unsized() {
    check_number(
        r#"
//- minicore: coerce_unsized, deref_mut, slice, index, transmute, non_null
use core::ops::{Deref, DerefMut, CoerceUnsized};
use core::{marker::Unsize, mem::transmute, ptr::NonNull};

struct ArcInner<T: ?Sized> {
    strong: usize,
    weak: usize,
    data: T,
}

pub struct Arc<T: ?Sized> {
    inner: NonNull<ArcInner<T>>,
}

impl<T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<Arc<U>> for Arc<T> {}

const GOAL: usize = {
    let x = transmute::<usize, Arc<[i32; 3]>>(12);
    let y: Arc<[i32]> = x;
    let z = transmute::<Arc<[i32]>, (usize, usize)>(y);
    z.1
};

        "#,
        3,
    );
}

#[test]
fn boxes() {
    check_number(
        r#"
//- minicore: coerce_unsized, deref_mut, slice
#![feature(lang_items)]
use core::ops::{Deref, DerefMut};
use core::{marker::Unsize, ops::CoerceUnsized};

#[rustc_intrinsic]
#[rustc_intrinsic_must_be_overridden]
pub fn box_new<T>(_x: T) -> Box<T>;

#[lang = "owned_box"]
pub struct Box<T: ?Sized> {
    inner: *mut T,
}
impl<T> Box<T> {
    fn new(t: T) -> Self {
        box_new(t)
    }
}

impl<T: ?Sized> Deref for Box<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &**self
    }
}

impl<T: ?Sized> DerefMut for Box<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut **self
    }
}

impl<T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<Box<U>> for Box<T> {}

const GOAL: usize = {
    let x = Box::new(5);
    let y: Box<[i32]> = Box::new([1, 2, 3]);
    *x + y.len()
};
"#,
        8,
    );
}

#[test]
fn array_and_index() {
    check_number(
        r#"
    //- minicore: coerce_unsized, index, slice
    const GOAL: u8 = {
        let a = [10, 20, 3, 15];
        let x: &[u8] = &a;
        x[1]
    };
        "#,
        20,
    );
    check_number(
        r#"
    //- minicore: coerce_unsized, index, slice
    const GOAL: usize = [1, 2, 3][2];"#,
        3,
    );
    check_number(
        r#"
    //- minicore: coerce_unsized, index, slice
    const GOAL: usize = { let a = [1, 2, 3]; let x: &[i32] = &a; x.len() };"#,
        3,
    );
    check_number(
        r#"
    //- minicore: coerce_unsized, index, slice
    const GOAL: usize = {
        let a = [1, 2, 3];
        let x: &[i32] = &a;
        let y = &*x;
        y.len()
    };"#,
        3,
    );
    check_number(
        r#"
    //- minicore: coerce_unsized, index, slice
    const GOAL: usize = [1, 2, 3, 4, 5].len();"#,
        5,
    );
    check_number(
        r#"
    //- minicore: coerce_unsized, index, slice
    const GOAL: [u16; 5] = [1, 2, 3, 4, 5];"#,
        1 + (2 << 16) + (3 << 32) + (4 << 48) + (5 << 64),
    );
    check_number(
        r#"
    //- minicore: coerce_unsized, index, slice
    const GOAL: [u16; 5] = [12; 5];"#,
        12 + (12 << 16) + (12 << 32) + (12 << 48) + (12 << 64),
    );
    check_number(
        r#"
    //- minicore: coerce_unsized, index, slice
    const LEN: usize = 4;
    const GOAL: u16 = {
        let x = [7; LEN];
        x[2]
    }"#,
        7,
    );
}

#[test]
fn string() {
    check_str(
        r#"
    //- minicore: coerce_unsized, index, slice
    const GOAL: &str = "hello";
        "#,
        "hello",
    );
}

#[test]
fn byte_string() {
    check_number(
        r#"
    //- minicore: coerce_unsized, index, slice
    const GOAL: u8 = {
        let a = b"hello";
        let x: &[u8] = a;
        x[0]
    };
        "#,
        104,
    );
}

#[test]
fn c_string() {
    check_number(
        r#"
//- minicore: index, slice
#![feature(lang_items)]
#[lang = "CStr"]
pub struct CStr {
    inner: [u8]
}
const GOAL: u8 = {
    let a = c"hello";
    a.inner[0]
};
    "#,
        104,
    );
    check_number(
        r#"
//- minicore: index, slice
#![feature(lang_items)]
#[lang = "CStr"]
pub struct CStr {
    inner: [u8]
}
const GOAL: u8 = {
    let a = c"hello";
    a.inner[5]
};
    "#,
        0,
    );
}

#[test]
fn consts() {
    check_number(
        r#"
    const F1: i32 = 1;
    const F3: i32 = 3 * F2;
    const F2: i32 = 2 * F1;
    const GOAL: i32 = F3;
    "#,
        6,
    );

    check_number(
        r#"
    const F1: i32 = 2147483647;
    const F2: i32 = F1 - 25;
    const GOAL: i32 = F2;
    "#,
        2147483622,
    );

    check_number(
        r#"
    const F1: i32 = -2147483648;
    const F2: i32 = F1 + 18;
    const GOAL: i32 = F2;
    "#,
        -2147483630,
    );

    check_number(
        r#"
    const F1: i32 = 10;
    const F2: i32 = F1 - 20;
    const GOAL: i32 = F2;
    "#,
        -10,
    );

    check_number(
        r#"
    const F1: i32 = 25;
    const F2: i32 = F1 - 25;
    const GOAL: i32 = F2;
    "#,
        0,
    );

    check_number(
        r#"
    const A: i32 = -2147483648;
    const GOAL: bool = A > 0;
    "#,
        0,
    );

    check_number(
        r#"
    const GOAL: i64 = (-2147483648_i32) as i64;
    "#,
        -2147483648,
    );
}

#[test]
fn statics() {
    check_number(
        r#"
    //- minicore: cell
    use core::cell::Cell;
    fn f() -> i32 {
        static S: Cell<i32> = Cell::new(10);
        S.set(S.get() + 1);
        S.get()
    }
    const GOAL: i32 = f() + f() + f();
    "#,
        36,
    );
}

#[test]
fn statics_pointing_at_each_other() {
    check_number(
        r#"
    //- minicore: sized
    struct Site {
        meta: &'static Meta,
        id: i32,
    }
    struct Meta {
        site: &'static Site,
        level: i32,
    }
    static SITE: Site = Site { meta: &META, id: 7 };
    static META: Meta = Meta { site: &SITE, level: 30 };
    const GOAL: i32 = SITE.meta.level + META.site.id + META.site.meta.level;
    "#,
        67,
    );
}

#[test]
fn static_read_through_another() {
    check_number(
        r#"
    //- minicore: sized
    static BASE: i32 = 5;
    static POINTER: &i32 = &BASE;
    static COPY: i32 = *POINTER + 1;
    const GOAL: i32 = COPY + *POINTER;
    "#,
        11,
    );
}

/// A constant may read a static (rustc's `const_refs_to_static`, stable since Rust 1.83): the
/// const argument `{ S }` and the array length `S` are anon consts whose evaluation reads `S`,
/// with no E0013 and no other diagnostic. Type checking `m::<{ S }>()` normalizes `{ S }`, which
/// evaluates it, so this needs MIR: it lives here, not in hir-ty's tests, which link none.
#[test]
fn a_static_as_a_const_arg_is_an_anon_const_that_reads_it() {
    let fixture = r#"
//- minicore: sized
struct T;
impl T {
    fn m<const N: usize>(&self) {}
}
static S: usize = 5;
fn statik() {
    T.m::<{ S }>();
}
const fn n<const N: usize>() -> usize { N }
const fn len<const N: usize>(_: [u8; N]) -> usize { N }
const fn of_s(a: [u8; S]) -> usize { len(a) }
const GOAL: usize = n::<{ S }>() * 10 + of_s([0; 5]);
"#;
    let fx = fixture::db(fixture);
    let file_id = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let def_map = hir_def::nameres::crate_def_map(&db, fixture::test_crate(&db));
        for (_, module) in def_map.modules(&db) {
            for def in module.scope(&db).declarations() {
                let body = match def {
                    hir_def::ModuleDefId::FunctionId(it) => hir_def::DefWithBodyId::FunctionId(it),
                    hir_def::ModuleDefId::ConstId(it) => hir_def::DefWithBodyId::ConstId(it),
                    _ => continue,
                };
                let infer =
                    crate::InferenceResult::of(&db, crate::InferBodyId::DefWithBodyId(body));
                assert!(
                    infer.diagnostics().is_empty(),
                    "{body:?} reads a static: {:?}",
                    infer.diagnostics()
                );
            }
        }
    });
    check_number_in(&db, file_id, 55);
}

#[test]
fn static_written_by_a_static() {
    let modifying = |e: ConstEvalError<'_>| {
        if let ConstEvalError::MirEvalError(MirEvalError::ConstEvalError(_, inner)) = e
            && let ConstEvalError::MirEvalError(MirEvalError::UndefinedBehavior(it)) =
                simplify(*inner)
        {
            it.starts_with("modifying the initial value of static")
        } else {
            false
        }
    };
    check_fail(
        r#"
    static mut A: u32 = 0;
    static B: u32 = unsafe { A = 1; 2 };
    const GOAL: u32 = B;
    "#,
        modifying,
    );
    check_fail(
        r#"
    static mut C: u32 = unsafe { C = 1; 0 };
    const GOAL: u32 = unsafe { C };
    "#,
        modifying,
    );
    check_number(
        r#"
    static mut D: u32 = 4;
    static E: u32 = { let mut x = unsafe { D }; x += 1; x };
    const GOAL: u32 = E;
    "#,
        5,
    );
}

#[test]
fn an_extern_static_is_not_read_at_compile_time() {
    // rustc's `ExternStatic`: only the linked program has its value, weak or not.
    check_fail(
        r#"
    //- minicore: sized
    extern "C" {
        #[linkage = "extern_weak"]
        static __dso_handle: *mut u8;
    }
    const GOAL: usize = unsafe { __dso_handle } as usize;
    "#,
        |e| {
            matches!(&e, ConstEvalError::MirEvalError(MirEvalError::NotSupported(it))
                if it.contains("extern static `__dso_handle`"))
        },
    );
}

#[test]
fn the_address_of_an_extern_static_is_known_at_compile_time() {
    check_number(
        r#"
    //- minicore: sized
    extern "C" {
        static __dso_handle: *mut u8;
    }
    const GOAL: usize = {
        let _at = &raw const __dso_handle;
        1
    };
    "#,
        1,
    );
}

#[test]
fn from_ne_bytes() {
    check_number(
        r#"
//- minicore: int_impl
const GOAL: u32 = u32::from_ne_bytes([44, 1, 0, 0]);
        "#,
        300,
    );
}

#[test]
fn enums() {
    check_number(
        r#"
    enum E {
        F1 = 1,
        F2 = 2 * E::F1 as isize, // Rustc expects an isize here
        F3 = 3 * E::F2 as isize,
    }
    const GOAL: u8 = E::F3 as u8;
    "#,
        6,
    );
    check_number(
        r#"
    enum E { F1 = 1, F2, }
    const GOAL: u8 = E::F2 as u8;
    "#,
        2,
    );
    check_number(
        r#"
    enum E { F1, }
    const GOAL: u8 = E::F1 as u8;
    "#,
        0,
    );
    let fx = fixture::db(
        r#"
        enum E { A = 1, B }
        const GOAL: E = E::A;
        "#,
    );
    let file_id = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let r = eval_goal(&db, file_id).unwrap();
        assert_eq!(allocation_as_usize(r), 1);
    })
}

#[test]
fn enum_of_many_implicit_discriminants() {
    let variants: String = (1..20_000).map(|i| format!("V{i}, ")).collect();
    check_number(
        &format!("enum E {{ V0 = 7, {variants} }}\nconst GOAL: i32 = E::V19999 as i32;"),
        7 + 19_999,
    );
    check_number(&format!("enum E {{ {variants} }}\nconst GOAL: i32 = E::V19999 as i32;"), 19_998);
    check_number(
        r#"
    enum E { A, B, C = 10, D, E, F = -3, G }
    const GOAL: i32 = E::B as i32 * 1000 + E::E as i32 * 10 + E::G as i32;
    "#,
        1000 + 12 * 10 - 2,
    );
}

/// The one enum declared in `file_id`'s module.
fn enum_in(db: &TestDB, file_id: EditionedFileId) -> hir_def::EnumId {
    let module_id = db.module_for_file(file_id.file_id(db));
    let def_map = module_id.def_map(db);
    let scope = &def_map.module(db, module_id).scope(db);
    scope
        .declarations()
        .find_map(|it| match it {
            hir_def::ModuleDefId::AdtId(hir_def::AdtId::EnumId(it)) => Some(it),
            _ => None,
        })
        .expect("no enum in the test")
}

#[test]
fn discriminants_count_in_signed_and_unsigned_reprs() {
    check_number(
        r#"
    #[repr(i8)]
    enum E { A = -128, B, C = -1, D }
    const GOAL: i32 = E::B as i32 * 10 + E::D as i32;
    "#,
        -127 * 10,
    );
    check_number(
        r#"
    #[repr(i64)]
    enum E { A = -2, B, C, D }
    const GOAL: i64 = E::D as i64;
    "#,
        1,
    );
    check_number(
        r#"
    #[repr(u8)]
    enum E { A = 254, B }
    const GOAL: u8 = E::B as u8;
    "#,
        255,
    );
    check_number(
        r#"
    #[repr(u64)]
    enum E { A = 18446744073709551614, B }
    const GOAL: u64 = E::B as u64;
    "#,
        u64::MAX as i128,
    );
    // An explicit discriminant may count from an earlier variant's, as rustc lowers the cast:
    // the earlier one's value never waits on the later ones.
    check_number(
        r#"
    enum E { A = 3, B, C = E::B as isize + 10, D }
    const GOAL: isize = E::D as isize;
    "#,
        15,
    );
}

#[test]
fn implicit_discriminant_past_the_repr_type_is_an_error() {
    // rustc's E0370, which blaze-diagnostics reports: `B` is not silently 256.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "a `check_fail` callback, which `check_fail` hands the error by value"
    )]
    fn overflowed(e: ConstEvalError<'_>) -> bool {
        matches!(e, ConstEvalError::MirLowerError(MirLowerError::HasErrors))
    }
    check_fail(
        r#"
    #[repr(u8)]
    enum E { A = 255, B }
    const GOAL: u8 = E::B as u8;
    "#,
        overflowed,
    );
    check_fail(
        r#"
    #[repr(i8)]
    enum E { A = 127, B }
    const GOAL: i8 = E::B as i8;
    "#,
        overflowed,
    );
    check_fail(
        r#"
    #[repr(u64)]
    enum E { A = 18446744073709551615, B, C }
    const GOAL: u64 = E::C as u64;
    "#,
        overflowed,
    );
    // An explicit discriminant after the overflow counts afresh.
    check_number(
        r#"
    #[repr(u8)]
    enum E { A = 255, B, C = 3, D }
    const GOAL: u8 = E::D as u8;
    "#,
        4,
    );
    let fx = fixture::db(
        r#"
    #[repr(u8)]
    enum E { A = 255, B, C }
    "#,
    );
    let file_id = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let table = crate::consteval::discriminants::enum_discriminants(&db, enum_in(&db, file_id));
        assert_eq!(table.overflowed(), [1]);
        assert!(table.discriminant(1).is_err() && table.discriminant(2).is_err());
        assert_eq!((table.value(1), table.value(2)), (0, 1));
        assert_eq!(table.variant_of(255), Some(0));
        assert_eq!(table.variant_of(0), None);
    });
}

#[test]
fn duplicate_discriminants_are_found_in_one_pass() {
    // rustc's E0081, which blaze-diagnostics reports: `D` counts to `A`'s 1.
    let fx = fixture::db(
        r#"
    #[repr(i8)]
    enum E { A = 1, B = -1, C, D }
    "#,
    );
    let file_id = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let table = crate::consteval::discriminants::enum_discriminants(&db, enum_in(&db, file_id));
        assert!(table.has_duplicates());
        let groups: Vec<Vec<u32>> = table.duplicates().iter().map(|it| it.to_vec()).collect();
        assert_eq!(groups, [vec![0, 3]]);
        assert_eq!(table.variant_of(1), Some(0));
        assert_eq!(table.variant_of(0), Some(2));
        assert_eq!(table.variant_of(-1), Some(1));
        assert!(table.overflowed().is_empty());
    });
}

#[test]
fn every_discriminant_and_the_layout_of_a_big_enum_take_one_pass() {
    use hir_def::HasModule as _;

    use crate::{
        ParamEnvAndCrate,
        layout::{Layout, RustcEnumVariantIdx},
    };

    /// Asks a fresh database every variant's discriminant and layout of an enum of `n`
    /// variants with one explicit discriminant: how long that took, and the queries it ran.
    fn ask(n: usize) -> (std::time::Duration, Vec<String>) {
        let variants: String = (1..n).map(|i| format!("V{i}, ")).collect();
        let fx = fixture::db(&format!("enum E {{ V0 = 7, {variants} }}"));
        let file_id = fx.only_file();
        let db = fx.into_db();
        let e = enum_in(&db, file_id);
        let ids: Vec<_> = e.enum_variants(&db).variants.values().map(|it| it.0).collect();
        blaze_query::db::attach(&db, || {
            let start = std::time::Instant::now();
            let executed = db.log_executed(|| {
                for (index, &variant) in ids.iter().enumerate() {
                    assert_eq!(db.const_eval_discriminant(variant), Ok(7 + index as i128));
                }
                let adt = hir_def::AdtId::EnumId(e);
                let env = ParamEnvAndCrate {
                    param_env: db.trait_environment(hir_def::GenericDefId::AdtId(adt)),
                    krate: adt.krate(&db),
                };
                let layout =
                    db.layout_of_adt(adt, GenericArgs::empty().store(), env.store()).unwrap();
                for index in 0..n {
                    let _ = Layout::for_variant(&layout, RustcEnumVariantIdx(index));
                }
            });
            (start.elapsed(), executed)
        })
    }

    let (_, executed) = ask(20_000);
    let runs = |name: &str| executed.iter().filter(|it| *it == name).count();
    assert_eq!(runs("explicit_discriminant"), 1);
    assert_eq!(runs("explicit_discriminant_indices"), 1);
    assert_eq!(runs("enum_discriminants"), 1);
    assert_eq!(runs("const_eval_discriminant_variant"), 20_000);
    if skip_slow_tests() {
        return;
    }
    // Four times the variants is four times the work; counting back to the explicit
    // discriminant would be sixteen.
    let best = |n| (0..3).map(|_| ask(n).0).min().unwrap();
    let (small, big) = (best(5_000), best(20_000));
    assert!(
        big < small * 10 + std::time::Duration::from_millis(50),
        "not linear: {small:?} for 5,000 variants, {big:?} for 20,000"
    );
}

#[test]
fn unsupported_str_binary_op_in_eval_rvalue() {
    check_fail(
        r#"
        enum E {
            A = 1,
            B = "a" + 1,
        }
        const GOAL: u8 = E::B as u8;
        "#,
        |e| matches!(e, ConstEvalError::MirEvalError(_)),
    );
}

#[test]
fn enum_variant_as_usize_in_const() {
    check_number(
        r#"
enum Foo {
    Bar = 4,
}
const GOAL: usize = Foo::Bar as usize;
    "#,
        4,
    );
}

#[test]
fn const_loop() {
    check_fail(
        r#"
    const F1: i32 = 1 * F3;
    const F3: i32 = 3 * F2;
    const F2: i32 = 2 * F1;
    const GOAL: i32 = F3;
    "#,
        |e| {
            if let ConstEvalError::MirEvalError(MirEvalError::ConstEvalError(_, inner)) = e
                && let ConstEvalError::MirLowerError(MirLowerError::Loop) = *inner
            {
                true
            } else {
                false
            }
        },
    );
}

#[test]
fn const_transfer_memory() {
    check_number(
        r#"
    //- minicore: slice, index, coerce_unsized, option
    const A1: &i32 = &1;
    const A2: &i32 = &10;
    const A3: [&i32; 3] = [&1, &2, &100];
    const A4: (i32, &i32, Option<&i32>) = (1, &1000, Some(&10000));
    const GOAL: i32 = *A1 + *A2 + *A3[2] + *A4.1 + *A4.2.unwrap_or(&5);
    "#,
        11111,
    );
}

#[test]
fn anonymous_const_block() {
    check_number(
        r#"
    #[rustc_intrinsic]
    pub fn size_of<T>() -> usize;

    const fn f<T>() -> usize {
        let r = const { size_of::<T>() };
        r
    }

    const GOAL: usize = {
        let x = const { 2 + const { 3 } };
        let y = f::<i32>();
        x + y
    };
    "#,
        9,
    );
}

#[test]
fn const_impl_assoc() {
    check_number(
        r#"
    struct U5;
    impl U5 {
        const VAL: usize = 5;
    }
    const GOAL: usize = U5::VAL + <U5>::VAL;
    "#,
        10,
    );
}

#[test]
fn inline_const_of_a_generic_function() {
    check_number(
        r#"
//- minicore: sized
struct Wide<T>(T);
impl<T> Wide<T> {
    const N: usize = 3;
}
fn sized<T>(extra: usize) -> usize {
    let block = const {
        let mut total = 0;
        let mut i = 0;
        while i < Wide::<T>::N {
            total += i + 1;
            i += 1;
        }
        total
    };
    block * extra
}
const GOAL: usize = sized::<u8>(2) + sized::<u32>(10);
"#,
        12 + 60,
    );
}

#[test]
fn impl_of_a_macro_in_the_body_of_a_dependency() {
    check_number(
        r#"
//- minicore: sized, tuple
//- /dtoa/lib.rs crate:dtoa library
#[macro_use]
mod diyfp;
macro_rules! dtoa {
    (value: $v:expr, significand_type: $sigty:ty, $($param:ident: $value:expr,)*) => {
        diyfp! { significand_type: $sigty, $($param: $value,)* };
        let (plus, minus) = DiyFp::new($v, 0).boundaries();
        plus.f * 10 + minus.f
    };
}
pub trait Write { fn write(self) -> u32; }
impl Write for u32 {
    fn write(self) -> u32 {
        dtoa! { value: self, significand_type: u32, factor: 3, }
    }
}
//- /dtoa/diyfp.rs
pub struct DiyFp<F, E> { pub f: F, pub e: E }
impl<F, E> DiyFp<F, E> {
    pub const fn new(f: F, e: E) -> Self { DiyFp { f, e } }
}
macro_rules! diyfp {
    (significand_type: $sigty:ty, factor: $factor:expr,) => {
        type DiyFp = diyfp::DiyFp<$sigty, i32>;
        impl DiyFp {
            const fn normalize(self) -> DiyFp { DiyFp::new(self.f * $factor, self.e) }
            const fn boundaries(self) -> (DiyFp, DiyFp) {
                let plus = DiyFp::new(self.f + 1, self.e - 1).normalize();
                (plus, self)
            }
        }
    };
}
//- /main.rs crate:main deps:dtoa
use dtoa::Write;
const GOAL: u32 = 4u32.write();
"#,
        154,
    );
}

#[test]
fn macro_defined_by_cfg_if_in_a_macro_use_module() {
    check_number(
        r#"
//- minicore: option, index, slice
//- /cfg_if.rs crate:cfg_if
#[macro_export]
macro_rules! cfg_if {
    (
        if #[cfg( $($i_meta:tt)+ )] { $( $i_tokens:tt )* }
        $( else if #[cfg( $($ei_meta:tt)+ )] { $( $ei_tokens:tt )* } )*
        $( else { $( $e_tokens:tt )* } )?
    ) => {
        $crate::cfg_if! {
            @__items () ;
            (( $($i_meta)+ ) ( $( $i_tokens )* )),
            $( (( $($ei_meta)+ ) ( $( $ei_tokens )* )), )*
            $( (() ( $( $e_tokens )* )), )?
        }
    };
    (@__items ( $( ($($_:tt)*) , )* ) ; ) => {};
    (
        @__items ( $( ($($no:tt)+) , )* ) ;
        (( $( $($yes:tt)+ )? ) ( $( $tokens:tt )* )),
        $( $rest:tt , )*
    ) => {
        #[cfg(all(
            $( $($yes)+ , )?
            not(any( $( $($no)+ ),* ))
        ))]
        $crate::cfg_if! { @__temp_group $( $tokens )* }
        $crate::cfg_if! {
            @__items ( $( ($($no)+) , )* $( ($($yes)+) , )? ) ;
            $( $rest , )*
        }
    };
    (@__temp_group $( $tokens:tt )* ) => {
        $( $tokens )*
    };
}
//- /main.rs crate:main deps:cfg_if
#[macro_use]
mod base {
    #[macro_use]
    mod debug_trace {
        use cfg_if::cfg_if;
        cfg_if! {
            if #[cfg(feature = "debug_trace")] {
                macro_rules! trace { ( @$ty:ident $($args:tt)* ) => { panic!() }; }
            } else {
                macro_rules! trace {
                    ( @$ty:ident $($args:tt)* ) => {};
                    ( @$ty:ident $($args:tt),* ) => {};
                }
            }
        }
    }
}
mod parser {
    pub struct Cursor { pub next_pos: usize }
    impl Cursor {
        pub const fn consume_ch(&mut self, input: &[u8]) -> Option<u8> {
            let ch = if self.next_pos < input.len() { Some(input[self.next_pos]) } else { None };
            self.next_pos += 1;
            trace!(@chars "consume", ch);
            ch
        }
    }
}
const GOAL: u8 = {
    let mut cursor = parser::Cursor { next_pos: 1 };
    match cursor.consume_ch(b"xyz") { Some(ch) => ch, None => 0 }
};
"#,
        b'y' as i128,
    );
}

#[test]
fn expression_fragment_is_one_operand_to_the_next_matcher() {
    // bun's `task_tag::tags!`: `tags!(@count (0u8 + 1u8) + 1u8,)` is not `@ $n:expr,` of a call
    // of `count`, as the fragment is one whole.
    check_number(
        r#"
//- minicore: index, slice, option
#[rustc_builtin_macro]
macro_rules! stringify {}
pub struct TaskTag(pub u8);
pub mod task_tag {
    use super::TaskTag;
    macro_rules! tags {
        ($($name:ident),* $(,)?) => {
            tags!(@ 0u8, $($name,)*);
            pub const COUNT: u8 = tags!(@count 0u8, $($name,)*);
            pub const NAMES: [&str; COUNT as usize] = [$(stringify!($name)),*];
        };
        (@ $n:expr, $head:ident, $($rest:ident,)*) => {
            pub const $head: TaskTag = TaskTag($n);
            tags!(@ $n + 1u8, $($rest,)*);
        };
        (@ $n:expr,) => {};
        (@count $n:expr, $head:ident, $($rest:ident,)*) => { tags!(@count $n + 1u8, $($rest,)*) };
        (@count $n:expr,) => { $n };
    }
    tags! {
        AnyTaskJob,
        AsyncModule,
        BakeHotReloadEvent,
        ChromePipeEvent,
        Close,
    }
}
const GOAL: usize = task_tag::NAMES.len() * 1000 + task_tag::Close.0 as usize;
"#,
        5004,
    );
}

#[test]
fn adt_const_params() {
    check_number(
        r#"
//- minicore: const_param_ty
#![feature(adt_const_params)]
use core::marker::ConstParamTy;
#[derive(PartialEq, Eq, ConstParamTy)]
enum Requirement { Connected, Not(u8), Pair { a: u16, b: bool } }
#[derive(PartialEq, Eq, ConstParamTy)]
struct Point(u8, u32);
const fn check<const R: Requirement>() -> u32 {
    match R {
        Requirement::Connected => 1,
        Requirement::Not(x) => x as u32 * 10,
        Requirement::Pair { a, b } => a as u32 * 100 + b as u32,
    }
}
const fn sum<const P: Point, const T: (u8, [u16; 2])>() -> u32 {
    let (a, [b, c]) = T;
    P.0 as u32 + P.1 + a as u32 + b as u32 + c as u32
}
const GOAL: u32 = check::<{ Requirement::Connected }>()
    + check::<{ Requirement::Not(3) }>()
    + check::<{ Requirement::Pair { a: 4, b: true } }>()
    + sum::<{ Point(1, 1000) }, { (2, [20, 300]) }>();
"#,
        1 + 30 + 401 + 1323,
    );
}

#[test]
fn const_generic_subst_fn() {
    check_number(
        r#"
    const fn f<const A: usize>(x: usize) -> usize {
        A * x + 5
    }
    const GOAL: usize = f::<2>(3);
    "#,
        11,
    );
    check_number(
        r#"
    fn f<const N: usize>(x: [i32; N]) -> usize {
        N
    }

    trait ArrayExt {
        fn f(self) -> usize;
    }

    impl<T, const N: usize> ArrayExt for [T; N] {
        fn g(self) -> usize {
            f(self)
        }
    }

    const GOAL: usize = f([1, 2, 5]);
    "#,
        3,
    );
}

#[test]
fn const_generic_fixed_width() {
    check_number(
        r#"
    const fn m<const N: u64>() -> u64 {
        N
    }
    const GOAL: u64 = m::<0>();
    "#,
        0,
    );
}

#[test]
fn layout_of_type_with_associated_type_field_defined_inside_body() {
    check_number(
        r#"
trait Tr {
    type Ty;
}

struct St<T: Tr>(T::Ty);

const GOAL: i64 = {
    // if we move `St2` out of body, the test will fail, as we don't see the impl anymore. That
    // case will probably be rejected by rustc in some later edition, but we should support this
    // case.
    struct St2;

    impl Tr for St2 {
        type Ty = i64;
    }

    struct Goal(St<St2>);

    let x = Goal(St(5));
    x.0.0
};
"#,
        5,
    );
}

#[test]
fn const_generic_subst_assoc_const_impl() {
    check_number(
        r#"
    struct Adder<const N: usize, const M: usize>;
    impl<const N: usize, const M: usize> Adder<N, M> {
        const VAL: usize = N + M;
    }
    const GOAL: usize = Adder::<2, 3>::VAL;
    "#,
        5,
    );
}

#[test]
fn associated_types() {
    check_number(
        r#"
    trait Tr {
        type Item;
        fn get_item(&self) -> Self::Item;
    }

    struct X(i32);
    struct Y(i32);

    impl Tr for X {
        type Item = Y;
        fn get_item(&self) -> Self::Item {
            Y(self.0 + 2)
        }
    }

    fn my_get_item<T: Tr>(x: T) -> <T as Tr>::Item {
        x.get_item()
    }

    const GOAL: i32 = my_get_item(X(3)).0;
    "#,
        5,
    );
}

#[test]
fn const_trait_assoc() {
    check_number(
        r#"
    struct U0;
    trait ToConst {
        const VAL: usize;
    }
    impl ToConst for U0 {
        const VAL: usize = 0;
    }
    impl ToConst for i32 {
        const VAL: usize = 32;
    }
    const GOAL: usize = U0::VAL + i32::VAL;
    "#,
        32,
    );
    check_number(
        r#"
    //- /a/lib.rs crate:a
    pub trait ToConst {
        const VAL: usize;
    }
    pub const fn to_const<T: ToConst>() -> usize {
        T::VAL
    }
    //- /main.rs crate:main deps:a
    use a::{ToConst, to_const};
    struct U0;
    impl ToConst for U0 {
        const VAL: usize = 5;
    }
    const GOAL: usize = to_const::<U0>();
    "#,
        5,
    );
    check_number(
        r#"
    //- minicore: size_of, fn
    //- /a/lib.rs crate:a
    pub struct S<T>(T);
    impl<T> S<T> {
        pub const X: usize = {
            let k: T;
            let f = || size_of::<T>();
            f()
        };
    }
    //- /main.rs crate:main deps:a
    use a::S;
    trait Tr {
        type Ty;
    }
    impl Tr for i32 {
        type Ty = u64;
    }
    struct K<T: Tr>(<T as Tr>::Ty);
    const GOAL: usize = S::<K<i32>>::X;
    "#,
        8,
    );
    check_number(
        r#"
    //- minicore: sized
    struct S<T>(*mut T);

    trait MySized: Sized {
        const SIZE: S<Self> = S(1 as *mut Self);
    }

    impl MySized for i32 {
        const SIZE: S<i32> = S(10 as *mut i32);
    }

    impl MySized for i64 {
    }

    const fn f<T: MySized>() -> usize {
        T::SIZE.0 as usize
    }

    const GOAL: usize = f::<i32>() + f::<i64>() * 2;
    "#,
        12,
    );
}

#[test]
fn exec_limits() {
    if skip_slow_tests() {
        return;
    }

    // `long_running_const_eval` is deny by default: rustc stops at 2,000,000 steps.
    check_fail(
        r#"
    const GOAL: usize = loop {};
    "#,
        |e| matches!(e, ConstEvalError::MirEvalError(MirEvalError::LongRunning { at: Some(_) })),
    );
    // Allowed or warned, the evaluation goes on to the budget.
    check_fail(
        r#"
    #[allow(long_running_const_eval)]
    const GOAL: usize = loop {};
    "#,
        |e| {
            matches!(
                e,
                ConstEvalError::MirEvalError(MirEvalError::OverBudget {
                    spent: Budget::Steps(10_000_000),
                    at: Some(_),
                })
            )
        },
    );
    check_fail(
        r#"
    #![warn(long_running_const_eval)]
    const GOAL: usize = loop {};
    "#,
        |e| {
            matches!(
                e,
                ConstEvalError::MirEvalError(MirEvalError::OverBudget {
                    spent: Budget::Steps(10_000_000),
                    at: Some(_),
                })
            )
        },
    );
    // The level is the item's whose body the outermost frame is, not the callee's.
    check_fail(
        r#"
    #[allow(long_running_const_eval)]
    const fn spin() -> usize { loop {} }
    const GOAL: usize = spin();
    "#,
        |e| matches!(e, ConstEvalError::MirEvalError(MirEvalError::LongRunning { at: Some(_) })),
    );
    check_fail(
        r#"
    const fn f(x: i32) -> i32 {
        f(x + 1)
    }
    const GOAL: i32 = f(0);
    "#,
        |e| matches!(e, ConstEvalError::MirEvalError(MirEvalError::StackOverflow)),
    );
    // Reasonable code should still work
    check_number(
        r#"
    const fn nth_odd(n: i32) -> i32 {
        2 * n - 1
    }
    const fn f(n: i32) -> i32 {
        let sum = 0;
        let i = 0;
        while i < n {
            i = i + 1;
            sum = sum + nth_odd(i);
        }
        sum
    }
    const GOAL: i32 = f(1000);
    "#,
        1000 * 1000,
    );
}

/// `long_running_const_eval` counts what rustc's `CtfeLimit` pass counts, a call or a loop's
/// back edge, not every block: 300,000 turns of a loop of 15 or so blocks each run more than
/// 2,000,000 blocks but are 300,000 steps, which rustc takes (blaze-profile's `VIEWS` is such a
/// loop over its SQL).
#[test]
fn long_running_lint_counts_loop_turns_not_blocks() {
    check_number(
        r#"
    const GOAL: usize = {
        let mut i = 0;
        let mut n = 0;
        while i < 300_000 {
            if i % 3 == 0 {
                n += 1;
            }
            i += 1;
        }
        n
    };
    "#,
        100_000,
    );
}

/// Each `const { .. }` block of a constant's value is a constant of its own to
/// `long_running_const_eval`, as rustc evaluates an inline const by an evaluation of its own:
/// three blocks of 700,000 steps each (350,000 turns of a loop that makes a call) are 2,100,000
/// steps together and none of them is long, where the same calls written in the constant's
/// own body are one evaluation of 2,100,000 steps, which rustc denies too (blaze-profile's
/// `VIEWS`, a `const { .. }` per view, was refused while the blocks spent `VIEWS`'s count).
#[test]
fn long_running_lint_counts_each_inline_const_on_its_own() {
    let turns = r#"
    const fn same(x: usize) -> usize { x }
    const fn turns(n: usize) -> usize {
        let mut i = 0;
        while i < n {
            i = same(i) + 1;
        }
        i
    }
    "#;
    check_number(
        &format!(
            "{turns}
    const GOAL: usize =
        const {{ turns(350_000) }} + const {{ turns(350_001) }} + const {{ turns(350_002) }};
    "
        ),
        1_050_003,
    );
    // In an array of another constant, as `VIEWS` has them.
    check_number(
        &format!(
            "{turns}
    const PARTS: [usize; 3] =
        [const {{ turns(350_000) }}, const {{ turns(350_001) }}, const {{ turns(350_002) }}];
    const GOAL: usize = PARTS[0] + PARTS[1] + PARTS[2];
    "
        ),
        1_050_003,
    );
    // The control: without the blocks it is one constant, and long.
    check_fail(
        &format!(
            "{turns}
    const GOAL: usize = turns(350_000) + turns(350_001) + turns(350_002);
    "
        ),
        |e| matches!(e, ConstEvalError::MirEvalError(MirEvalError::LongRunning { at: Some(_) })),
    );
}

#[test]
fn memory_limit() {
    check_fail(
        r#"
        extern "Rust" {
            #[rustc_allocator]
            fn __rust_alloc(size: usize, align: usize) -> *mut u8;
        }

        const GOAL: u8 = unsafe {
            __rust_alloc(30_000_000_000, 1); // 30GB
            2
        };
        "#,
        |e| {
            matches!(
                e,
                ConstEvalError::MirEvalError(MirEvalError::OverBudget {
                    spent: Budget::Memory(1_073_741_824),
                    at: Some(_),
                })
            )
        },
    );
}

#[test]
fn inline_const_in_a_loop_is_evaluated_once() {
    // 3000 copies of the block's 512 KiB would be past the default 1 GiB.
    check_number(
        r#"
    //- minicore: index, slice
    const fn f() -> usize {
        let mut i = 0;
        let mut sum = 0;
        while i < 3000 {
            let [first, ..] = const { [1u8; 1 << 19] };
            sum += first as usize;
            i += 1;
        }
        sum
    }
    const GOAL: usize = f();
    "#,
        3000,
    );
}

/// The expansion's error is reported at the call; as rustc's `ExprKind::Err`, it taints the
/// body, which is not refused as something blaze does not support.
#[test]
fn a_body_whose_macro_failed_to_expand_has_errors() {
    check_fail(
        r#"
    #[rustc_builtin_macro]
    macro_rules! compile_error { () => {} }
    const GOAL: u8 = { compile_error!("no"); 1 };
    "#,
        |e| matches!(e, ConstEvalError::MirLowerError(MirLowerError::HasErrors)),
    );
}

#[test]
fn type_error() {
    check_fail(
        r#"
    const GOAL: u8 = {
        let x: u16 = 2;
        let y: (u8, u8) = x;
        y.0
    };
    "#,
        |e| matches!(e, ConstEvalError::MirLowerError(MirLowerError::HasErrors)),
    );
}

#[test]
fn unsized_field() {
    check_number(
        r#"
    //- minicore: coerce_unsized, index, slice, transmute
    use core::mem::transmute;

    struct Slice([usize]);
    struct Slice2(Slice);

    impl Slice2 {
        fn as_inner(&self) -> &Slice {
            &self.0
        }

        fn as_bytes(&self) -> &[usize] {
            &self.as_inner().0
        }
    }

    const GOAL: usize = unsafe {
        let x: &[usize] = &[1, 2, 3];
        let x: &Slice2 = transmute(x);
        let x = x.as_bytes();
        x[0] + x[1] + x[2] + x.len() * 100
    };
        "#,
        306,
    );
}

#[test]
fn an_unknown_type_in_a_constant_stops_its_lowering() {
    check_fail(
        r#"
    //- minicore: coerce_unsized, index, slice
    const fn x() -> SomeUnknownTypeThatDereferenceToSlice {
        SomeUnknownTypeThatDereferenceToSlice
    }

    const GOAL: u16 = {
        let y = x();
        let z: &[u16] = &y;
        z[1]
    };
    "#,
        // rustc stops on the unknown type (E0412); nothing of `GOAL` is evaluated past it.
        |e| matches!(e, ConstEvalError::MirLowerError(MirLowerError::HasErrors)),
    );
}

#[test]
fn recursive_adt() {
    check_fail(
        r#"
        //- minicore: coerce_unsized, index, slice
        pub enum TagTree {
            Leaf,
            Choice(&'static [TagTree]),
        }
        const GOAL: TagTree = {
            const TAG_TREE: TagTree = TagTree::Choice(&[
                {
                    const VARIANT_TAG_TREE: TagTree = TagTree::Choice(
                        &[
                            TAG_TREE,
                        ],
                    );
                    VARIANT_TAG_TREE
                },
            ]);
            TAG_TREE
        };
    "#,
        |e| {
            if let ConstEvalError::MirEvalError(MirEvalError::ConstEvalError(_, inner)) = e
                && let ConstEvalError::MirLowerError(MirLowerError::Loop) = *inner
            {
                true
            } else {
                false
            }
        },
    );
}

#[test]
fn uninhabited_enum_discriminant_read() {
    check_fail(
        r#"
    #![feature(never_type)]
    enum E { A(!), }
    union U { u: (), e: E, }
    const GOAL: () = {
        let E::A(a) = unsafe { &(&U { u: () }).e };
    };
    "#,
        |e| matches!(e, ConstEvalError::MirEvalError(MirEvalError::UndefinedBehavior(_))),
    );
}

#[test]
fn checked_u128_sub_without_i128_alignment() {
    // The fixtures' data layout gives `u128` an alignment of 8, so `(u128, bool)` is 24 bytes.
    check_number("const GOAL: u128 = (1 << 61) - 1;", (1i128 << 61) - 1);
}

/// A constant loaded into another evaluation keeps the vtable it was made with when its
/// pointer's pointee is an alias that normalizes to `dyn` (`&'static <H as Has>::Obj` in a
/// field) or ends in one (`&Wrap<dyn Tr>`): the id is remapped to the one this evaluation
/// gives the type, where `B` took the first id, so the call is `A`'s.
/// A constant loaded into another evaluation whose pointee is a struct with an unsized tail
/// (`&Wrap<dyn Tr>`, `&Wrap<[&i32]>`) has the pointers in its sized fields and in its tail (a
/// `dyn`'s concrete value, a slice's elements) moved to where this evaluation keeps what they
/// point to; the evaluation allocates first, so the constant's own addresses are elsewhere.
#[test]
fn a_loaded_constants_pointers_in_an_unsized_tail_are_its_own() {
    check_number(
        r#"
    //- minicore: coerce_unsized, dispatch_from_dyn, index, slice
    trait Tr { fn f(&self) -> i32; }
    struct P(&'static i32);
    impl Tr for P { fn f(&self) -> i32 { *self.0 } }
    struct Wrap<X: ?Sized>(&'static i32, X);
    const WP: &Wrap<dyn Tr> = &Wrap(&5, P(&7));
    const WS: &Wrap<[&'static i32]> = &Wrap(&1, [&2, &3]);
    const GOAL: i32 = {
        let pad: &[i32; 8] = &[9, 9, 9, 9, 9, 9, 9, 9];
        let more: &[i32; 8] = &[8, 8, 8, 8, 8, 8, 8, 8];
        *WP.0 * 10000 + WP.1.f() * 1000 + *WS.0 * 100 + *WS.1[0] * 10 + *WS.1[1]
            + (pad[0] - 9) + (more[0] - 8)
    };
        "#,
        57123,
    );
}

/// A loaded constant's piece that two of its pointers share (`Fill::RECIPE`, named by both of
/// `Inks`'s fields; `f32::RECIPE`, by all of `Rgba`'s) is relocated once: walked again, its
/// pointers are this evaluation's and were moved a second time, onto whichever piece had that
/// address in the constant's evaluation, or onto no memory (harness-native-ui-look-api's
/// `LAYOUT_HASH`: "read invalid memory address 4 with size 56", a `&Recipe` moved onto an
/// empty piece of alignment 4). The shape is hyperion-layout's recipes under `api_layouts!`.
#[test]
fn a_loaded_constants_shared_pointee_is_relocated_once() {
    check_number(
        r#"
    //- minicore: coerce_unsized, index, slice, str
    pub enum Recipe {
        Prim { size: usize, align: usize },
        Struct { fields: &'static [Field], size: usize, align: usize },
    }
    pub struct Field { pub offset: usize, pub ty: &'static Recipe }
    pub trait Layout { const RECIPE: &'static Recipe; }
    impl Layout for f32 { const RECIPE: &'static Recipe = &Recipe::Prim { size: 4, align: 4 }; }
    impl Layout for u8 { const RECIPE: &'static Recipe = &Recipe::Prim { size: 1, align: 1 }; }
    pub struct Rgba;
    pub struct Fill;
    pub struct Inks;
    pub struct HoverInk;
    impl Layout for Rgba {
        const RECIPE: &'static Recipe = &Recipe::Struct {
            fields: &[
                Field { offset: 0, ty: <f32 as Layout>::RECIPE },
                Field { offset: 4, ty: <f32 as Layout>::RECIPE },
                Field { offset: 8, ty: <f32 as Layout>::RECIPE },
                Field { offset: 12, ty: <f32 as Layout>::RECIPE },
            ],
            size: 16,
            align: 4,
        };
    }
    impl Layout for Fill {
        const RECIPE: &'static Recipe = &Recipe::Struct {
            fields: &[
                Field { offset: 0, ty: <Rgba as Layout>::RECIPE },
                Field { offset: 16, ty: <Rgba as Layout>::RECIPE },
            ],
            size: 32,
            align: 4,
        };
    }
    impl Layout for Inks {
        const RECIPE: &'static Recipe = &Recipe::Struct {
            fields: &[
                Field { offset: 0, ty: <Fill as Layout>::RECIPE },
                Field { offset: 32, ty: <Fill as Layout>::RECIPE },
            ],
            size: 64,
            align: 4,
        };
    }
    impl Layout for HoverInk {
        const RECIPE: &'static Recipe = &Recipe::Struct {
            fields: &[
                Field { offset: 0, ty: <Fill as Layout>::RECIPE },
                Field { offset: 32, ty: <Fill as Layout>::RECIPE },
                Field { offset: 64, ty: <u8 as Layout>::RECIPE },
            ],
            size: 68,
            align: 4,
        };
    }
    pub struct ApiType { pub rust: &'static str, pub recipe: &'static Recipe }
    pub const fn api_type<T: Layout>(rust: &'static str) -> ApiType {
        ApiType { rust, recipe: T::RECIPE }
    }
    const fn feed(r: &Recipe) -> usize {
        match r {
            Recipe::Prim { size, align } => *size + *align,
            Recipe::Struct { fields, size, align } => {
                let mut h = *size + *align;
                let mut i = 0;
                while i < fields.len() {
                    h += fields[i].offset + feed(fields[i].ty);
                    i += 1;
                }
                h
            }
        }
    }
    const fn fold(types: &[ApiType]) -> usize {
        let mut sum = 0;
        let mut i = 0;
        while i < types.len() {
            let t = &types[i];
            sum += t.rust.as_bytes().len() + feed(t.recipe);
            i += 1;
        }
        sum
    }
    pub const API_TYPES: &[ApiType] = &[
        api_type::<Rgba>("Rgba"),
        api_type::<Fill>("Fill"),
        api_type::<Inks>("Inks"),
        api_type::<HoverInk>("HoverInk"),
    ];
    const GOAL: usize = fold(API_TYPES);
        "#,
        // Rgba 76, Fill 204, Inks 508, HoverInk 578 (f32 8, u8 2), and 20 bytes of names.
        1386,
    );
}

#[test]
fn a_loaded_constants_vtable_behind_an_alias_or_a_tail_is_its_own() {
    check_number(
        r#"
    //- minicore: coerce_unsized, dispatch_from_dyn
    trait Tr { fn f(&self) -> i32; }
    struct A;
    impl Tr for A { fn f(&self) -> i32 { 1 } }
    struct B;
    impl Tr for B { fn f(&self) -> i32 { 2 } }
    trait Has { type Obj: ?Sized + 'static; }
    struct H;
    impl Has for H { type Obj = dyn Tr; }
    struct W<U: Has>(&'static U::Obj);
    const WC: W<H> = W(&A as &dyn Tr);
    const GOAL: i32 = { let b: &dyn Tr = &B; b.f() * 10 + WC.0.f() };
        "#,
        21,
    );
    check_number(
        r#"
    //- minicore: coerce_unsized, dispatch_from_dyn
    trait Tr { fn f(&self) -> i32; }
    struct A;
    impl Tr for A { fn f(&self) -> i32 { 1 } }
    struct B;
    impl Tr for B { fn f(&self) -> i32 { 2 } }
    struct Wrap<X: ?Sized>(i32, X);
    const WT: &Wrap<dyn Tr> = &Wrap(3, A);
    const GOAL: i32 = { let b: &dyn Tr = &B; b.f() * 100 + WT.0 * 10 + WT.1.f() };
        "#,
        231,
    );
}

/// A constant loaded into another evaluation keeps the vtable it was made with where no type
/// says the vtable is there (a `&dyn Tr` behind a `*const u8`): the word's provenance says it
/// is a vtable, and its id is remapped to the one this evaluation gives `A` for `Tr`, where
/// `B` took the first id, so the call is `A`'s.
#[test]
fn a_loaded_constants_vtable_behind_a_cast_is_its_own() {
    check_number(
        r#"
    //- minicore: coerce_unsized, dispatch_from_dyn
    trait Tr { fn f(&self) -> i32; }
    struct A;
    impl Tr for A { fn f(&self) -> i32 { 7 } }
    struct B;
    impl Tr for B { fn f(&self) -> i32 { 1000 } }
    struct RawU8(*const u8);
    const CAST: RawU8 = RawU8(&(&A as &dyn Tr) as *const &dyn Tr as *const u8);
    const GOAL: i32 = {
        let b: &dyn Tr = &B;
        let a: &dyn Tr = unsafe { *(CAST.0 as *const &dyn Tr) };
        b.f() - 1000 + a.f()
    };
        "#,
        7,
    );
}

/// rustc's `unsize_into_ptr`: a `dyn`-to-`dyn` cast between two types that are not one checks
/// that the vtable is one of the source type's trait before giving it one of the target's. A
/// `Tr` vtable transmuted to `&dyn Sub` and upcast to `&dyn Display` is refused as undefined
/// behavior naming the constant and the rule, never relabeled a valid `Display` vtable (the
/// round-3 review's C1).
#[test]
fn an_upcast_of_a_vtable_of_another_trait_is_refused() {
    check_fail(
        r#"
    //- minicore: coerce_unsized, dispatch_from_dyn, transmute, fmt
    use core::{fmt::{Display, Formatter, Result}, mem::transmute};
    trait Tr { fn f(&self) -> i32; }
    trait Sub: Display {}
    struct A(i32);
    impl Tr for A { fn f(&self) -> i32 { self.0 } }
    impl Display for A { fn fmt(&self, _: &mut Formatter<'_>) -> Result { Ok(()) } }
    const GOAL: &dyn Display = {
        let t: &dyn Tr = &A(1);
        let s: &dyn Sub = unsafe { transmute::<&dyn Tr, &dyn Sub>(t) };
        s
    };
    "#,
        |e| {
            matches!(e, ConstEvalError::MirEvalError(MirEvalError::UndefinedBehavior(it))
                if it.contains("the constant `GOAL`") && it.contains("InvalidVTableTrait"))
        },
    );
}

/// What the check of [`an_upcast_of_a_vtable_of_another_trait_is_refused`] lets through: a
/// real upcast, and a cast that drops an auto trait (checked too, and of the right trait),
/// each calling through its new vtable.
#[test]
fn an_upcast_and_a_dropped_auto_trait_keep_their_vtables_right() {
    check_number(
        r#"
    //- minicore: coerce_unsized, dispatch_from_dyn, send
    trait Base { fn b(&self) -> i32; }
    trait Sub: Base { fn s(&self) -> i32; }
    struct A(i32);
    impl Base for A { fn b(&self) -> i32 { self.0 } }
    impl Sub for A { fn s(&self) -> i32 { self.0 * 10 } }
    const GOAL: i32 = {
        let s: &dyn Sub = &A(3);
        let b: &dyn Base = s;
        let sent: &(dyn Base + Send) = &A(4);
        let unsent: &dyn Base = sent;
        b.b() * 100 + s.s() + unsent.b()
    };
    "#,
        334,
    );
}

/// A constant whose value is an empty slice one past its block's end (`split_at(len).1`), or
/// a reference to an unsized struct of no size, keeps an alignment for that empty piece (or
/// no piece, where it is at no allocation), so another constant that reads it copies it,
/// with no "no alignment its evaluation recorded" (the round-3 review's C4).
#[test]
fn a_constant_read_by_another_keeps_its_empty_pieces_aligned() {
    check_number(
        r#"
    //- minicore: coerce_unsized, slice, transmute
    use core::mem::transmute;
    #[rustc_intrinsic]
    pub fn offset<Ptr, Delta>(dst: Ptr, offset: Delta) -> Ptr;
    #[repr(C)]
    struct Parts(*const u8, usize);
    const fn split_at(s: &'static [u8], mid: usize) -> (&'static [u8], &'static [u8]) {
        unsafe {
            let Parts(p, n) = transmute::<&'static [u8], Parts>(s);
            (
                transmute::<Parts, &'static [u8]>(Parts(p, mid)),
                transmute::<Parts, &'static [u8]>(Parts(offset(p, mid), n - mid)),
            )
        }
    }
    const A: &[u8] = &[1, 2, 3];
    const H: &[u8] = split_at(A, 3).0;
    const T: &[u8] = split_at(A, 3).1;
    const N: usize = T.len();
    struct W<X: ?Sized>(X);
    const E: &W<[u8]> = &W([]);
    const GOAL: usize = H.len() * 100 + N * 10 + E.0.len() + 7;
    "#,
        307,
    );
}
