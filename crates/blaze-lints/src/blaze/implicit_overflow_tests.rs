//! `implicit_overflow` (docs/v2/plan/design/explicit-overflow.md): each operator and cast is
//! found where nothing bounds it, and not where a check bounds it or the code wrote the form
//! that says what overflow means; the verdict is the same with and without overflow checks;
//! the fix rewrites the clear cases.

use crate::tests::lint;

const MINICORE: &str = "//- minicore: sized, copy, clone, drop, option, result, fn, panic, index, slice, builtin_impls, range, iterator, non_zero, reach, int_wrapping, int_overflow, from, try\n";

const FLAGS: &str = "-A clippy::all --warn=blaze::implicit_overflow";

fn linted_in(main: &str, meta: &str) -> String {
    let fixture =
        format!("{MINICORE}//- /src/lib.rs crate:main{meta}\n{}", main.trim_start_matches('\n'));
    lint(&fixture, FLAGS)
}

/// The messages of `implicit_overflow` for `main`, the same with and without overflow checks.
fn found(main: &str) -> Vec<String> {
    let found = |meta: &str| -> Vec<String> {
        linted_in(main, meta)
            .split("\n\n")
            .filter(|block| block.contains("#implicit_overflow"))
            .filter_map(|block| block.lines().next())
            .map(str::to_owned)
            .collect()
    };
    let (unchecked, checked) = (found(""), found(" cfg:overflow_checks"));
    assert_eq!(unchecked, checked, "the verdict is the profile's:\n{main}");
    checked
}

fn assert_found(main: &str, wanted: &[&str]) {
    let found = found(main);
    assert_eq!(found.len(), wanted.len(), "{found:#?}");
    for (line, want) in found.iter().zip(wanted) {
        assert!(line.contains(want), "{line}\nwanted: {want}");
    }
}

/// Each operator, its compound form and unary `-`, found where nothing bounds the operands,
/// with the operand nothing bounds named.
#[test]
fn each_operator_is_found_when_nothing_bounds_it() {
    assert_found(
        r#"
pub fn add(a: u32, b: u32) -> u32 { a + b }
pub fn sub(a: u32, b: u32) -> u32 { a - b }
pub fn mul(a: i64, b: i64) -> i64 { a * b }
pub fn shl(a: u64, b: u32) -> u64 { a << b }
pub fn shr(a: u64, b: u32) -> u64 { a >> b }
pub fn neg(a: i32) -> i32 { -a }
pub fn add_assign(mut a: u8, b: u8) -> u8 { a += b; a }
"#,
        &[
            "can't show `a + b` fits in `u32` here: nothing bounds `a` or `b`",
            "can't show `a - b` fits in `u32` here: nothing shows `b <= a`",
            "can't show `a * b` fits in `i64` here: nothing bounds `a` or `b`",
            "can't show `a << b` shifts by less than the bits of `u64` here: nothing shows `b < 64`",
            "can't show `a >> b` shifts by less than the bits of `u64` here: nothing shows `b < 64`",
            "can't show `-a` fits in `i32` here: nothing shows `a != i32::MIN`",
            "can't show `a += b` fits in `u8` here: nothing bounds `a` or `b`",
        ],
    );
}

/// The operations a check bounds: `i + 1` under `i < v.len()`, `a - b` under `b <= a`, a sum of
/// two bounded values, a shift by a bounded amount, `-a` under `a > 0`, a constant, a mask.
#[test]
fn a_guarded_operation_is_not_found() {
    assert_found(
        r#"
pub fn next(v: &[u32], i: usize) -> usize { if i < v.len() { i + 1 } else { 0 } }
pub fn diff(a: u32, b: u32) -> u32 { if b <= a { a - b } else { 0 } }
pub fn small(a: u8, b: u8) -> u32 { if a < 10 && b < 10 { (a as u32) + (b as u32) * 3 } else { 0 } }
pub fn shift(a: u64, b: u32) -> u64 { if b < 64 { a << b } else { 0 } }
pub fn neg(a: i32) -> i32 { if a > 0 { -a } else { 0 } }
pub fn constant() -> u32 { 3 + 4 * 5 }
pub fn low(x: u32) -> u8 { (x & 0xff) as u8 }
pub fn slot(i: usize, len: usize) -> usize { if len > 0 { (i % len) + 0 } else { 0 } }
"#,
        &[],
    );
}

/// The near misses: the bound is of another value, or too wide.
#[test]
fn a_bound_of_another_value_or_too_wide_is_found() {
    assert_found(
        r#"
pub fn other(a: u32, b: u32, c: u32) -> u32 { if c < 10 { a + 1 } else { b } }
pub fn wide(a: u8) -> u8 { if a < 200 { a + 100 } else { 0 } }
"#,
        &[
            "can't show `a + 1` fits in `u32` here: nothing bounds `a`",
            "can't show `a + 100` fits in `u8` here: what bounds `a` and `100` here lets it reach 299",
        ],
    );
}

/// Each lossy integer cast, found where nothing bounds the value; not where a check does,
/// nor a widening one, nor a literal that fits. A float cast to an integer is always found.
#[test]
fn a_lossy_cast_is_found_unless_bounded() {
    assert_found(
        r#"
pub fn narrow(x: u32) -> u8 { x as u8 }
pub fn sign(x: i32) -> u32 { x as u32 }
pub fn pointer(x: u64) -> usize { x as usize }
pub fn float(x: f64) -> i32 { x as i32 }
pub fn bounded(x: u32) -> u8 { if x < 256 { x as u8 } else { 0 } }
pub fn natural(x: i32) -> u32 { if x >= 0 { x as u32 } else { 0 } }
pub fn widen(x: u32) -> u64 { x as u64 }
pub fn literal() -> u8 { 200 as u8 }
"#,
        &[
            "can't show `x as u8` keeps its value in `u8` here: nothing shows `x <= 255`",
            "can't show `x as u32` keeps its value in `u32` here: nothing shows `0 <= x`",
            "can't show `x as usize` keeps its value in `usize` here: nothing shows `x <= 4294967295`",
            "`x as i32` turns a float into `i32`",
        ],
    );
}

/// The forms that say what overflow means are not found; `strict_*` panics, and `pow` and
/// `abs` overflow as the profile says: found.
#[test]
fn each_explicit_form_passes_and_strict_is_found() {
    assert_found(
        r#"
pub fn checked(a: u32, b: u32) -> Option<u32> { a.checked_add(b) }
pub fn wrapping(a: u32, b: u32) -> u32 { a.wrapping_mul(b) }
pub fn saturating(a: u32, b: u32) -> u32 { a.saturating_sub(b) }
pub fn overflowing(a: u32, b: u32) -> (u32, bool) { a.overflowing_add(b) }
pub fn converted(x: u64) -> Result<u8, core::num::TryFromIntError> { u8::try_from(x) }
pub fn strict(a: u32, b: u32) -> u32 { a.strict_add(b) }
pub fn power(a: u32) -> u32 { a.pow(3) }
pub fn absolute(a: i32) -> i32 { a.abs() }
"#,
        &[
            "`a.strict_add(b)` panics when it overflows, in every profile",
            "can't show `a.pow(3)` fits in `u32` here: blaze does not follow `pow`",
            "can't show `a.abs()` fits in `i32` here: blaze does not follow `abs`",
        ],
    );
}

/// `/` and `%` are not this lint's: their panics are the same in every profile.
#[test]
fn division_is_not_found() {
    assert_found(
        "pub fn div(a: i32, b: i32) -> i32 { a / b }\npub fn rem(a: i32, b: i32) -> i32 { a % b }\n",
        &[],
    );
}

/// Arithmetic in a closure is judged in the closure's own body.
#[test]
fn a_closure_is_judged() {
    assert_found(
        r#"
pub fn sums(a: u32) -> u32 {
    let add = |b: u32| a + b;
    let small = |b: u8| if b < 10 { b + 1 } else { 0 };
    let _ = small(2);
    add(1)
}
"#,
        &["can't show `a + b` fits in `u32` here: nothing bounds `a` or `b`"],
    );
}

/// Arithmetic written in a macro call's arguments is found where it is written.
#[test]
fn arithmetic_in_a_macro_argument_is_found() {
    assert_found(
        r#"
macro_rules! id { ($e:expr) => { $e }; }
pub fn f(a: u32) -> u32 { id!(a + 1) }
"#,
        &["can't show `a + 1` fits in `u32` here: nothing bounds `a`"],
    );
}

/// The file after `blaze check --fix`, and that nothing is left to fix in it.
fn fixed(main: &str) -> String {
    use base_db::FileId;
    use hir_expand::fixture;

    use crate::{
        check::{Setup, check},
        conf::Conf,
        fix::fix,
        level::CommandLine,
    };

    let text = format!("{MINICORE}//- /src/lib.rs crate:main\n{}", main.trim_start_matches('\n'));
    let fx = fixture::db(&text);
    let file = *fx.files().last().expect("src/lib.rs");
    let mut db = fx.into_db();
    let krate = blaze_query::db::attach(&db, || crate::tests::main_crate(&db));
    let file = file.file_id(&db);
    let name_of = |it: FileId| (it == file).then(|| "src/lib.rs".to_owned());
    let flags: Vec<String> = FLAGS.split_whitespace().map(str::to_owned).collect();
    let setup = Setup {
        command_line: CommandLine::parse(&flags).expect("the flags read"),
        conf: Conf::default(),
    };
    fix(&mut db, krate, &setup, &name_of).unwrap_or_else(|error| panic!("{error}"));
    let after = db.file_text(file).text(&db).to_string();
    blaze_query::db::attach(&db, || {
        let found = check(&db, krate, &setup, &name_of).expect("the file is named");
        let left: Vec<&str> = found
            .iter()
            .map(|it| it.diagnostic.message.as_str())
            .filter(|it| it.contains("can't show"))
            .collect();
        assert!(left.is_empty(), "the fixed file has nothing left:\n{after}\n{left:#?}");
    });
    after
}

/// In a function returning `Option`, an operation becomes its `checked_*` form with `?`, and a
/// cast `try_from` with `.ok()?`; in one returning a `Result` whose error takes a
/// `TryFromIntError`, a cast becomes `try_from` with `?`.
#[test]
fn the_fix_says_overflow_is_a_failure() {
    let after = fixed(
        r#"
pub fn add(a: u32, b: u32) -> Option<u32> { Some(a + b) }
pub fn neg(a: i32) -> Option<i32> { Some(-a) }
pub fn grow(mut a: u64, b: u64) -> Option<u64> { a += b; Some(a) }
pub fn shift(a: u64, b: u32) -> Option<u64> { Some(a << b) }
pub fn one(a: u32) -> Option<u32> { Some(1 + a) }
pub fn narrow(x: u32) -> Option<u8> { Some(x as u8) }
pub fn convert(x: u64) -> Result<u16, core::num::TryFromIntError> { Ok(x as u16) }
"#,
    );
    for want in [
        "Some(a.checked_add(b)?)",
        "Some(a.checked_neg()?)",
        "a = a.checked_add(b)?;",
        "Some(a.checked_shl(b)?)",
        "Some(1u32.checked_add(a)?)",
        "Some(u8::try_from(x).ok()?)",
        "Ok(u16::try_from(x)?)",
    ] {
        assert!(after.contains(want), "{want}\n{after}");
    }
}

