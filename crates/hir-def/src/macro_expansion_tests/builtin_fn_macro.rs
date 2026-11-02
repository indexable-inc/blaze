//! Tests for `builtin_fn_macro.rs` from `hir_expand`.

use expect_test::expect;
use hir_expand::fixture;

use crate::{
    ModuleDefId, expr_store::Body, macro_expansion_tests::check, nameres::crate_def_map,
    test_db::TestDBExt,
};

#[test]
fn test_column_expand() {
    check(
        r#"
#[rustc_builtin_macro]
macro_rules! column {() => {}}

fn main() { column!(); }
"#,
        expect![[r#"
#[rustc_builtin_macro]
macro_rules! column {() => {}}

fn main() { 13u32; }
"#]],
    );
}

#[test]
fn test_asm_expand() {
    check(
        r#"
#[rustc_builtin_macro]
macro_rules! asm {() => {}}
#[rustc_builtin_macro]
macro_rules! global_asm {() => {}}
#[rustc_builtin_macro]
macro_rules! naked_asm {() => {}}

global_asm! {
    ""
}

#[unsafe(naked)]
extern "C" fn foo() {
    naked_asm!("");
}

fn main() {
    let i: u64 = 3;
    let o: u64;
    unsafe {
        asm!(
            "mov {0}, {1}",
            "add {0}, 5",
            out(reg) o,
            in(reg) i,
        );
    }
}
"#,
        expect![[r##"
#[rustc_builtin_macro]
macro_rules! asm {() => {}}
#[rustc_builtin_macro]
macro_rules! global_asm {() => {}}
#[rustc_builtin_macro]
macro_rules! naked_asm {() => {}}

builtin #global_asm ("")

#[unsafe(naked)]
extern "C" fn foo() {
    builtin #naked_asm ("");
}

fn main() {
    let i: u64 = 3;
    let o: u64;
    unsafe {
        builtin #asm ("mov {0}, {1}", "add {0}, 5", out (reg)o, in (reg)i, );
    }
}
"##]],
    );
}

#[test]
fn test_line_expand() {
    check(
        r#"
#[rustc_builtin_macro]
macro_rules! line {() => {}}

fn main() { line!() }
"#,
        expect![[r#"
#[rustc_builtin_macro]
macro_rules! line {() => {}}

fn main() { 4u32 }
"#]],
    );
}

/// rustc's `expansion_cause`: a call that a macro's body writes is where that macro is called, a
/// call passed to a macro is where it is written, and a column counts code points.
#[test]
fn line_and_column_are_where_the_call_is_written() {
    let db = fixture::db(
        r#"
#[rustc_builtin_macro]
macro_rules! line {() => {}}
#[rustc_builtin_macro]
macro_rules! column {() => {}}
macro_rules! here { () => { (line!(), column!()) } }
macro_rules! pass { ($e:expr) => { $e } }

fn main() {
    let 人之初 = here!();
    let _ = pass!(column!());
    let _ = line!(column);
}
"#,
    )
    .into_db();
    let krate = db.fetch_test_crate();
    let def_map = crate_def_map(&db, krate);
    let main = def_map.module(&db, def_map.root).scope(&db).declarations().find_map(|it| match it {
        ModuleDefId::FunctionId(it) => Some(it),
        _ => None,
    });
    let (_, source_map) = Body::with_source_map(&db, main.unwrap().into());
    let mut placed = source_map
        .macro_calls()
        .map(|(_, call)| {
            let written = call.loc(&db).to_node_view(&db).value.to_string();
            let expansion = call.parse_macro_expansion(&db);
            (written, expansion.value.0.root().to_string(), expansion.err.is_some())
        })
        .filter(|(_, expanded, _)| expanded.ends_with("u32"))
        .collect::<Vec<_>>();
    placed.sort();
    let placed = placed.iter().map(|(a, b, c)| (a.as_str(), b.as_str(), *c)).collect::<Vec<_>>();
    assert_eq!(
        placed,
        [
            ("column!()", "15u32", false),
            ("column!()", "19u32", false),
            ("line!()", "9u32", false),
            ("line!(column)", "11u32", true),
        ]
    );
}

/// rustc's `lookup_char_pos`: a byte order mark that starts the file is no column, and a `\r\n`
/// ends a line as a `\n` does.
#[test]
fn line_and_column_after_a_byte_order_mark_and_crlf_lines() {
    let first = "#[rustc_builtin_macro] macro_rules! column {() => {}} \
        #[rustc_builtin_macro] macro_rules! line {() => {}} fn a() -> u32 { ";
    let second = "fn b() -> (u32, u32) { (line!(), ";
    let text = format!("\u{feff}{first}column!() }}\r\n{second}column!()) }}\r\n");
    let db = fixture::db(&text).into_db();
    let mut placed = super::builtin_errors::macro_calls(&db)
        .into_iter()
        .map(|call| {
            let written = call.loc(&db).to_node_view(&db).value.to_string();
            (written, call.parse_macro_expansion(&db).value.0.root().to_string())
        })
        .collect::<Vec<_>>();
    placed.sort();
    let column = |before: &str| format!("{}u32", before.chars().count() + 1);
    let mut expected = vec![
        ("column!()".to_owned(), column(first)),
        ("column!()".to_owned(), column(second)),
        ("line!()".to_owned(), "2u32".to_owned()),
    ];
    expected.sort();
    assert_eq!(placed, expected);
}

#[test]
fn test_stringify_expand() {
    check(
        r#"
#[rustc_builtin_macro]
macro_rules! stringify {() => {}}

fn main() {
    stringify!(
        a
        b
        c
    );
}
"#,
        expect![[r##"
#[rustc_builtin_macro]
macro_rules! stringify {() => {}}

fn main() {
    "a b c";
}
"##]],
    );
}

#[test]
fn test_stringify_spacing() {
    check(
        r#"
#[rustc_builtin_macro]
macro_rules! stringify {() => {}}

fn main() {
    stringify!(0 as int! {...} != 0);
    stringify!(1 + 'a: loop { break 'a 1 } == 1);
    stringify!(E::U::<>>E::U::<u8>);
    stringify!(a +-b; x<<=y; &&z; {} { } {a} { a } {a } { a}; f::<'a>(); |&'a x| 1);
}
"#,
        expect![[r##"
#[rustc_builtin_macro]
macro_rules! stringify {() => {}}

fn main() {
    "0 as int! {...} != 0";
    "1 + \'a: loop { break \'a 1 } == 1";
    "E::U::<>>E::U::<u8>";
    "a +-b; x<<=y; &&z; {} {} {a} { a } {a} { a }; f::<\'a>(); |&\'a x| 1";
}
"##]],
    );
}

#[test]
fn test_env_expand() {
    check(
        r#"
#[rustc_builtin_macro]
macro_rules! env {() => {}}

fn main() {
    env!("TEST_ENV_VAR");
    env!("TEST_ENV_VAR",);
    env!("TEST_ENV_VAR", "error");
    env!("TEST_ENV_VAR", "error",);
}
"#,
        expect![[r##"
#[rustc_builtin_macro]
macro_rules! env {() => {}}

fn main() {
    /* error: environment variable `TEST_ENV_VAR` not defined at compile time */"UNRESOLVED_ENV_VAR";
    /* error: environment variable `TEST_ENV_VAR` not defined at compile time */"UNRESOLVED_ENV_VAR";
    /* error: error */"UNRESOLVED_ENV_VAR";
    /* error: error */"UNRESOLVED_ENV_VAR";
}
"##]],
    );
}

#[test]
fn test_option_env_expand() {
    check(
        r#"
#[rustc_builtin_macro]
macro_rules! option_env {() => {}}

fn main() {
    option_env!("TEST_ENV_VAR");
    option_env!("TEST_ENV_VAR",);
    option_env!("TEST_ENV_VAR", "invalid");
}
"#,
        expect![[r#"
#[rustc_builtin_macro]
macro_rules! option_env {() => {}}

fn main() {
    $crate::option::Option::None:: < &str>;
    $crate::option::Option::None:: < &str>;
    /* error: option_env! takes 1 argument */$crate::option::Option::None:: < &str>;
}
"#]],
    );
}

#[test]
fn test_file_expand() {
    check(
        r#"
#[rustc_builtin_macro]
macro_rules! file {() => {}}

fn main() { file!(); }
"#,
        expect![[r##"
#[rustc_builtin_macro]
macro_rules! file {() => {}}

fn main() { "file"; }
"##]],
    );
}

#[test]
fn test_assert_expand() {
    check(
        r#"
//- minicore: assert
fn main() {
    assert!(true, "{} {:?}", arg1(a, b, c), arg2);
}
"#,
        expect![[r#"
fn main() {
     {
        if !(true ) {
            $crate::panic::panic_2021!("{} {:?}", arg1(a, b, c), arg2);
        }
    };
}
"#]],
    );
}

#[test]
fn test_assert_expand_2015() {
    check(
        r#"
//- minicore: assert
//- /main.rs edition:2015
fn main() {
    assert!(true, "{} {:?}", arg1(a, b, c), arg2);
}
"#,
        expect![[r#"
fn main() {
     {
        if !(true ) {
            panic!("{} {:?}", arg1(a, b, c), arg2);
        }
    };
}
"#]],
    );
}

#[test]
fn test_compile_error_expand() {
    check(
        r#"
#[rustc_builtin_macro]
macro_rules! compile_error {
    ($msg:expr) => ({ /* compiler built-in */ });
    ($msg:expr,) => ({ /* compiler built-in */ })
}

// This expands to nothing (since it's in item position), but emits an error.
compile_error!("error, with an escaped quote: \"");
compile_error!(r"this is a raw string");
"#,
        expect![[r##"
#[rustc_builtin_macro]
macro_rules! compile_error {
    ($msg:expr) => ({ /* compiler built-in */ });
    ($msg:expr,) => ({ /* compiler built-in */ })
}

/* error: error, with an escaped quote: " */
/* error: this is a raw string */
"##]],
    );
}

#[test]
fn test_format_args_expand() {
    check(
        r#"
#[rustc_builtin_macro]
macro_rules! format_args {
    ($fmt:expr) => ({ /* compiler built-in */ });
    ($fmt:expr, $($args:tt)*) => ({ /* compiler built-in */ })
}

fn main() {
    format_args!("{} {:?}", arg1(a, b, c), arg2);
}
"#,
        expect![[r##"
#[rustc_builtin_macro]
macro_rules! format_args {
    ($fmt:expr) => ({ /* compiler built-in */ });
    ($fmt:expr, $($args:tt)*) => ({ /* compiler built-in */ })
}

fn main() {
    builtin #format_args ("{} {:?}", arg1(a, b, c), arg2);
}
"##]],
    );
}

#[test]
fn regression_15002() {
    check(
        r#"
#[rustc_builtin_macro]
macro_rules! format_args {
    ($fmt:expr) => ({ /* compiler built-in */ });
    ($fmt:expr, $($args:tt)*) => ({ /* compiler built-in */ })
}

fn main() {
    format_args!(x = 2);
    format_args!/*+errors*/(x =);
    format_args!/*+errors*/(x =, x = 2);
    format_args!/*+errors*/("{}", x =);
    format_args!/*+errors*/(=, "{}", x =);
    format_args!(x = 2, "{}", 5);
}
"#,
        expect![[r##"
#[rustc_builtin_macro]
macro_rules! format_args {
    ($fmt:expr) => ({ /* compiler built-in */ });
    ($fmt:expr, $($args:tt)*) => ({ /* compiler built-in */ })
}

fn main() {
    builtin #format_args (x = 2);
    /* parse error: expected expression */
builtin #format_args (x = );
    /* parse error: expected expression */
builtin #format_args (x = , x = 2);
    /* parse error: expected expression */
builtin #format_args ("{}", x = );
    /* parse error: expected expression */
/* parse error: expected expression */
builtin #format_args ( = , "{}", x = );
    builtin #format_args (x = 2, "{}", 5);
}
"##]],
    );
}

#[test]
fn test_format_args_expand_with_comma_exprs() {
    check(
        r#"
#[rustc_builtin_macro]
macro_rules! format_args {
    ($fmt:expr) => ({ /* compiler built-in */ });
    ($fmt:expr, $($args:tt)*) => ({ /* compiler built-in */ })
}

fn main() {
    format_args!("{} {:?}", a::<A,B>(), b);
}
"#,
        expect![[r##"
#[rustc_builtin_macro]
macro_rules! format_args {
    ($fmt:expr) => ({ /* compiler built-in */ });
    ($fmt:expr, $($args:tt)*) => ({ /* compiler built-in */ })
}

fn main() {
    builtin #format_args ("{} {:?}", a::<A, B>(), b);
}
"##]],
    );
}

#[test]
fn test_format_args_expand_with_raw_strings() {
    check(
        r##"
#[rustc_builtin_macro]
macro_rules! format_args {
    ($fmt:expr) => ({ /* compiler built-in */ });
    ($fmt:expr, $($args:tt)*) => ({ /* compiler built-in */ })
}

fn main() {
    format_args!(
        r#"{},mismatch,"{}","{}""#,
        location_csv_pat(db, &analysis, vfs, &sm, pat_id),
        mismatch.expected.display(db),
        mismatch.actual.display(db)
    );
}
"##,
        expect![[r##"
#[rustc_builtin_macro]
macro_rules! format_args {
    ($fmt:expr) => ({ /* compiler built-in */ });
    ($fmt:expr, $($args:tt)*) => ({ /* compiler built-in */ })
}

fn main() {
    builtin #format_args (r#"{},mismatch,"{}","{}""#, location_csv_pat(db, &analysis, vfs, &sm, pat_id), mismatch.expected.display(db), mismatch.actual.display(db));
}
"##]],
    );
}

#[test]
fn test_format_args_expand_eager() {
    check(
        r#"
#[rustc_builtin_macro]
macro_rules! concat {}

#[rustc_builtin_macro]
macro_rules! format_args {
    ($fmt:expr) => ({ /* compiler built-in */ });
    ($fmt:expr, $($args:tt)*) => ({ /* compiler built-in */ })
}

fn main() {
    format_args!(concat!("xxx{}y", "{:?}zzz"), 2, b);
}
"#,
        expect![[r##"
#[rustc_builtin_macro]
macro_rules! concat {}

#[rustc_builtin_macro]
macro_rules! format_args {
    ($fmt:expr) => ({ /* compiler built-in */ });
    ($fmt:expr, $($args:tt)*) => ({ /* compiler built-in */ })
}

fn main() {
    builtin #format_args (concat!("xxx{}y", "{:?}zzz"), 2, b);
}
"##]],
    );
}

#[test]
fn test_format_args_expand_with_broken_member_access() {
    check(
        r#"
#[rustc_builtin_macro]
macro_rules! format_args {
    ($fmt:expr) => ({ /* compiler built-in */ });
    ($fmt:expr, $($args:tt)*) => ({ /* compiler built-in */ })
}

fn main() {
    let _ =
        format_args!/*+errors*/("{} {:?}", a.);
}
"#,
        expect![[r##"
#[rustc_builtin_macro]
macro_rules! format_args {
    ($fmt:expr) => ({ /* compiler built-in */ });
    ($fmt:expr, $($args:tt)*) => ({ /* compiler built-in */ })
}

fn main() {
    let _ =
        /* parse error: expected field name or number */
builtin #format_args ("{} {:?}", a.);
}
"##]],
    );
}

#[test]
fn test_include_bytes_expand() {
    check(
        r#"
#[rustc_builtin_macro]
macro_rules! include_bytes {
    ($file:expr) => {{ /* compiler built-in */ }};
    ($file:expr,) => {{ /* compiler built-in */ }};
}

fn main() { include_bytes("foo");include_bytes(r"foo"); }
"#,
        expect![[r##"
#[rustc_builtin_macro]
macro_rules! include_bytes {
    ($file:expr) => {{ /* compiler built-in */ }};
    ($file:expr,) => {{ /* compiler built-in */ }};
}

fn main() { include_bytes("foo");include_bytes(r"foo"); }
"##]],
    );
}

#[test]
fn test_concat_expand() {
    check(
        r##"
#[rustc_builtin_macro]
macro_rules! concat {}

fn main() { concat!("fo", "o", 0, r#""bar""#, "\n", false, '"', -4, - 4, '\0'); }
"##,
        expect![[r##"
#[rustc_builtin_macro]
macro_rules! concat {}

fn main() { "foo0\"bar\"\nfalse\"-4-4\u{0}"; }
"##]],
    );
}

#[test]
fn test_concat_bytes_expand() {
    check(
        r##"
#[rustc_builtin_macro]
macro_rules! concat_bytes {}

fn main() { concat_bytes!(b'A', b"BC\"", [68, b'E', 70], br#"G""#,b'\0'); }
"##,
        expect![[r#"
#[rustc_builtin_macro]
macro_rules! concat_bytes {}

fn main() { b"ABC\"DEFG\"\x00"; }
"#]],
    );
}

#[test]
fn test_concat_with_captured_expr() {
    check(
        r##"
#[rustc_builtin_macro]
macro_rules! concat {}

macro_rules! surprise {
    () => { "s" };
}

fn main() { concat!(surprise!()); }
"##,
        expect![[r##"
#[rustc_builtin_macro]
macro_rules! concat {}

macro_rules! surprise {
    () => { "s" };
}

fn main() { "s"; }
"##]],
    );
}

#[test]
fn test_quote_string() {
    check(
        r##"
#[rustc_builtin_macro]
macro_rules! stringify {}

fn main() { stringify!("hello"); }
"##,
        expect![[r##"
#[rustc_builtin_macro]
macro_rules! stringify {}

fn main() { "\"hello\""; }
"##]],
    );
}

#[test]
fn cfg_select() {
    check(
        r#"
#[rustc_builtin_macro]
pub macro cfg_select($($tt:tt)*) {}

cfg_select! {
    false => { fn false_1() {} }
    any(false, true) => { fn true_1() {} }
}

cfg_select! {
    false => { fn false_2() {} }
    _ => { fn true_2() {} }
}

const _: ((),) = cfg_select! { _ => ((), ) };
const _: i32 = cfg_select! { true => 2 + 3, _ => 3 + 4 };
const _: i32 = cfg_select! { false => 2 + 3, _ => 3 + 4 };
const _: bool = cfg_select! { _ => 2 < 3 };
const _: bool = cfg_select! { true => foo::<(), fn() -> Foo<i32, i64>>(1,), _ => false };

cfg_select! {
    false => { fn false_3() {} }
}

cfg_select! {
    false
}

cfg_select! {
    false =>
}

    "#,
        expect![[r#"
#[rustc_builtin_macro]
pub macro cfg_select($($tt:tt)*) {}

fn true_1() {}

fn true_2() {}

const _: ((),) = ((), );
const _: i32 = 2+3;
const _: i32 = 3+4;
const _: bool = 2<3;
const _: bool = foo::<(), fn() -> Foo<i32, i64>>(1, );

/* error: none of the predicates in this `cfg_select` evaluated to true */

/* error: expected `=>` after cfg expression */

/* error: expected a token tree after `=>` */

    "#]],
    );
}

#[test]
fn eager_recursion_limit() {
    check(
        r#"
//- minicore: concat

macro_rules! concat_separator {
    () => {
        concat!("", concat_separator!())
    };
}

fn main() {
    concat_separator!()
}
    "#,
        expect![[r#"

macro_rules! concat_separator {
    () => {
        concat!("", concat_separator!())
    };
}

fn main() {
    concat!("", concat_separator!())
}
    "#]],
    );
}
