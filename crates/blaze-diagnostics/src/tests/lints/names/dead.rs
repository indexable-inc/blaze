//! `dead_code` against rustc's UI tests (`tests/ui/lint/dead-code/`), each its source and its
//! `.stderr` byte for byte (a test that passes has none: its stderr is empty), and a library
//! whose `pub` items another crate may use.

use std::fmt::Write as _;

use crate::{
    Options,
    emit::Style,
    tests::{Mode, assert_reports, checked, checked_with, compiletest, error, normalized},
};

/// A UI test: what its crate is built with (minicore's flags for core, std's prelude beyond
/// core's and its items) and how compiletest runs it.
struct Ui<'a> {
    name: &'a str,
    edition: &'a str,
    mode: Mode,
    core: &'a str,
    std: Option<(&'a str, &'a str)>,
    source: &'a str,
}

impl Default for Ui<'_> {
    fn default() -> Self {
        Ui { name: "", edition: "2015", mode: Mode::Other, core: "", std: None, source: "" }
    }
}

/// What rustc writes to stderr for `ui`, as compiletest runs it.
fn ui(ui: Ui<'_>) -> String {
    let mut fixture = String::new();
    if !ui.core.is_empty() {
        writeln!(fixture, "//- minicore: {}", ui.core).unwrap();
    }
    let deps = match ui.std {
        Some((prelude, items)) => {
            assert!(!ui.core.is_empty(), "std's prelude is core's: give minicore's flags");
            fixture.push_str("//- /std.rs crate:std\npub mod prelude {\n");
            for edition in ["rust_2015", "rust_2018", "rust_2021", "rust_2024"] {
                writeln!(
                    fixture,
                    "    pub mod {edition} {{ pub use core::prelude::{edition}::*; pub use crate::__prelude::*; }}"
                )
                .unwrap();
            }
            writeln!(fixture, "}}\n#[doc(hidden)]\npub mod __prelude {{\n{prelude}\n}}\n{items}").unwrap();
            " deps:std"
        }
        None => "",
    };
    write!(fixture, "//- /{} crate:other:main edition:{}{deps}\n{}", ui.name, ui.edition, ui.source).unwrap();
    let options = compiletest(ui.mode, &Options::default());
    let stderr = checked_with(&fixture, "$DIR/", &options).stderr_with(Style { ui_testing: true, ..Style::default() });
    normalized(&stderr)
}

/// A test of edition 2015 with no core.
fn plain(name: &str, source: &str) -> String {
    crate::tests::ui(name, "2015", source)
}

/// std's `println!` and what it calls.
const PRINTLN: &str = r#"pub mod io {
    pub fn _print(_args: core::fmt::Arguments<'_>) {}
}

#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {{
        $crate::io::_print(core::format_args_nl!($($arg)*));
    }};
}
"#;

/// std's `String` (of a size, as a field of it is not a ZST), `ToString` of `str`, and
/// `From<&str>`.
const STRING: &str = r#"pub mod string {
    pub struct String(pub usize);
    pub trait ToString {
        fn to_string(&self) -> String;
    }
    impl ToString for str {
        fn to_string(&self) -> String {
            String(0)
        }
    }
    impl<'a> core::convert::From<&'a str> for String {
        fn from(_: &'a str) -> String {
            String(0)
        }
    }
}
"#;

const STRING_PRELUDE: &str = "pub use crate::string::{String, ToString};";

macro_rules! test_case {
    ($test:ident, $name:literal) => {
        #[test]
        fn $test() {
            let source = include_str!(concat!("dead/", $name, ".rs"));
            let expected = include_str!(concat!("dead/", $name, ".stderr"));
            assert_eq!(plain(concat!($name, ".rs"), source), expected);
        }
    };
    ($test:ident, $name:literal, core: $core:literal) => {
        #[test]
        fn $test() {
            let source = include_str!(concat!("dead/", $name, ".rs"));
            let expected = include_str!(concat!("dead/", $name, ".stderr"));
            assert_eq!(ui(Ui { name: concat!($name, ".rs"), core: $core, source, ..Ui::default() }), expected);
        }
    };
}

test_case!(lint_dead_code_1, "lint-dead-code-1", core: "copy");
test_case!(lint_dead_code_2, "lint-dead-code-2");
test_case!(lint_dead_code_5, "lint-dead-code-5");
test_case!(multiple_dead_codes_in_the_same_struct, "multiple-dead-codes-in-the-same-struct", core: "fmt");
test_case!(unused_variant, "unused-variant", core: "clone, fmt");
test_case!(unused_struct_variant, "unused-struct-variant");
test_case!(issue_85255, "issue-85255");
test_case!(unused_assoc_fns, "unused-assoc-fns");
test_case!(type_alias, "type-alias");
test_case!(unused_enum, "unused-enum");
test_case!(empty_unused_enum, "empty-unused-enum");
test_case!(unused_trait_with_assoc_ty, "unused-trait-with-assoc-ty");
test_case!(unused_trait_with_assoc_const, "unused-trait-with-assoc-const");
test_case!(const_underscore_issue_101532, "const-underscore-issue-101532");
test_case!(unused_adt_impls_trait, "unused-adt-impls-trait");
test_case!(unused_adt_impls_pub_trait, "unused-adt-impls-pub-trait", core: "clone");
test_case!(lint_unused_adt_appeared_in_pattern, "lint-unused-adt-appeared-in-pattern");
test_case!(allow_trait_or_impl, "allow-trait-or-impl");
test_case!(unused_struct_derive_default, "unused-struct-derive-default", core: "default, derive");
test_case!(pub_field_in_priv_mod, "pub-field-in-priv-mod");
test_case!(issue_41883, "issue-41883", core: "fmt");
test_case!(impl_trait, "impl-trait");
test_case!(unused_adt_impl_pub_trait_with_assoc_const, "unused-adt-impl-pub-trait-with-assoc-const");

#[test]
fn in_closure() {
    let source = include_str!("dead/in-closure.rs");
    let expected = include_str!("dead/in-closure.stderr");
    assert_eq!(ui(Ui { name: "in-closure.rs", edition: "2021", core: "future, fn", source, ..Ui::default() }), expected);
}

#[test]
fn lint_dead_code_4() {
    let source = include_str!("dead/lint-dead-code-4.rs");
    let prelude = format!("{STRING_PRELUDE}\npub use crate::num::Pow;");
    let items = format!(
        "{STRING}\npub mod num {{\n    pub trait Pow {{ fn pow(self, exp: u32) -> Self; }}\n    impl Pow for usize {{ fn pow(self, _: u32) -> usize {{ self }} }}\n}}\n"
    );
    let ui_ = Ui {
        name: "lint-dead-code-4.rs",
        core: "from, unary_ops, builtin_impls",
        std: Some((&prelude, &items)),
        source,
        ..Ui::default()
    };
    assert_reports(
        &ui(ui_),
        &[
            error(7, &["`b`"]),
            error(15, &["`X`", "`Y`"]),
            error(24, &["`ABC`"]),
            error(39, &["`b`", "`c`"]),
            error(36, &["`I`", "`K`"]),
            error(61, &["`c`", "`x`"]),
        ],
    );
}

#[test]
fn lint_dead_code_6() {
    let source = include_str!("dead/lint-dead-code-6.rs");
    let expected = include_str!("dead/lint-dead-code-6.stderr");
    let ui_ = Ui {
        name: "lint-dead-code-6.rs",
        core: "fmt, builtin_impls",
        std: Some(("pub use crate::println;", PRINTLN)),
        source,
        ..Ui::default()
    };
    assert_eq!(ui(ui_), expected);
}

#[test]
fn tuple_struct_field() {
    let source = include_str!("dead/tuple-struct-field.rs");
    let expected = include_str!("dead/tuple-struct-field.stderr");
    let items = format!("{STRING}\npub mod marker {{\n    pub use core::marker::*;\n}}\n");
    let ui_ = Ui {
        name: "tuple-struct-field.rs",
        core: "from, phantom_data, copy",
        std: Some((STRING_PRELUDE, &items)),
        source,
        ..Ui::default()
    };
    assert_eq!(ui(ui_), expected);
}

/// The tests that pass with no warning (`check-pass`, `build-pass`, `run-pass`): no
/// `.stderr`.
#[test]
fn passes() {
    let run_pass = |name: &str, source: &str| crate::tests::ui_run_pass(name, "2015", source, &Options::default());
    let source = include_str!("dead/trait-impl.rs");
    assert_eq!(plain("trait-impl.rs", source), "");
    let source = include_str!("dead/issue-68408-false-positive.rs");
    assert_eq!(plain("issue-68408-false-positive.rs", source), "");
    let source = include_str!("dead/unused-variant-pub.rs");
    assert_eq!(plain("unused-variant-pub.rs", source), "");
    let source = include_str!("dead/leading-underscore.rs");
    assert_eq!(run_pass("leading-underscore.rs", source), "");
    let source = include_str!("dead/enum-variants.rs");
    assert_eq!(run_pass("enum-variants.rs", source), "");
    let source = include_str!("dead/with-impl.rs");
    let with_impl = Ui {
        name: "with-impl.rs",
        mode: Mode::RunPass,
        core: "fmt, builtin_impls",
        std: Some(("pub use crate::println;", PRINTLN)),
        source,
        ..Ui::default()
    };
    assert_eq!(ui(with_impl), "");
}

/// The messages of the dead-code lints of `fixture`'s crates, as `blaze build` checks them.
fn dead(fixture: &str) -> Vec<String> {
    checked(fixture).messages().into_iter().filter(|it| it.starts_with("dead_code")).collect()
}

/// A library's `pub` items are live, whether or not the crate using it calls them; a private
/// one nothing calls is not.
#[test]
fn a_library_and_its_user() {
    let fixture = r#"
//- /main.rs crate:other:main deps:dep
fn main() {
    dep::called();
}
//- /dep.rs crate:dep
#![crate_type = "lib"]
pub fn called() {}
pub fn not_called() {
    helper();
}
fn helper() {}
fn unused() {}
pub struct Public;
struct Private;
"#;
    assert_eq!(
        dead(fixture),
        ["dead_code: function `unused` is never used", "dead_code: struct `Private` is never constructed"]
    );
}

/// In a binary, the same `pub` items are `dead_code_pub_in_binary` when it is set.
#[test]
fn pub_in_a_binary() {
    let fixture = r#"
//- /main.rs crate:other:main
#![warn(dead_code_pub_in_binary)]
pub fn not_called() {}
pub fn called() {}
fn main() {
    called();
}
"#;
    assert_eq!(dead(fixture), ["dead_code_pub_in_binary: function `not_called` is never used"]);
}

/// `x = x` and `s.f = s.f` in a body that is live.
#[test]
fn self_assignment() {
    let fixture = r#"
//- /main.rs crate:other:main
struct S { f: i32 }
fn main() {
    let mut x = 1;
    x = x;
    let mut s = S { f: 0 };
    s.f = s.f;
}
"#;
    assert_eq!(
        dead(fixture),
        [
            "dead_code: useless assignment of variable of type `i32` to itself",
            "dead_code: useless assignment of field of type `i32` to itself",
        ]
    );
}

/// `r#x = x` assigns the same local: names are compared without `r#`.
#[test]
fn self_assignment_raw() {
    let fixture = r#"
//- /main.rs crate:other:main
fn main() {
    let mut r#x = 1;
    r#x = x;
}
"#;
    assert_eq!(dead(fixture), ["dead_code: useless assignment of variable of type `i32` to itself"]);
}

/// `Self(a, _)` in an impl of a tuple struct reads the fields it binds, as `S(a, _)` does
/// (rustc's `variant_of_res` of the pattern's type).
#[test]
fn self_tuple_struct_pattern() {
    let fixture = r#"
//- minicore: copy
//- /main.rs crate:other:main
struct S(i32, i32);
impl S {
    fn first(&self) -> i32 {
        let Self(a, _) = *self;
        a
    }
}
fn main() {
    S(1, 2).first();
}
"#;
    assert_eq!(dead(fixture), ["dead_code: field `1` is never read"]);
}

/// A pattern's qualifier is a use: the alias in `Alias::A(x)` is live (rustc's `visit_qpath`
/// of the pattern's path).
#[test]
fn alias_in_pattern_qualifier() {
    let fixture = r#"
//- /main.rs crate:other:main
enum E { A(i32) }
type Alias = E;
fn main() {
    let Alias::A(x) = E::A(1);
    let _ = x;
}
"#;
    assert_eq!(dead(fixture), Vec::<String>::new());
}

/// The assignee of a destructuring assignment is a pattern: `E::A = e` does not construct `A`.
#[test]
fn assignee_variant_not_constructed() {
    let fixture = r#"
//- /main.rs crate:other:main
enum E { A }
fn take(e: E) {
    E::A = e;
}
fn main() {
    let _f: fn(E) = take;
}
"#;
    assert_eq!(dead(fixture), ["dead_code: variant `A` is never constructed"]);
}

#[test]
fn basic() {
    let source = include_str!("dead/basic.rs");
    assert_reports(
        &ui(Ui { name: "basic.rs", core: "panic", source, ..Ui::default() }),
        &[
            error(4, &["`foo`"]),
        ],
    );
}

#[test]
fn lint_dead_code_3() {
    let source = include_str!("dead/lint-dead-code-3.rs");
    assert_reports(
        &ui(Ui { name: "lint-dead-code-3.rs", core: "coerce_unsized", source, ..Ui::default() }),
        &[
            error(14, &["`Foo`"]),
            error(16, &["`foo`"]),
            error(21, &["`bar`"]),
            error(62, &["`c_void`"]),
            error(81, &["`blah`"]),
            error(85, &["`blah`"]),
            error(64, &["`my_free`"]),
        ],
    );
}

/// Our own: with derives expanded, as a build has them, a field the derived `PartialEq` reads is
/// live, and one only `Clone`'s and `Debug`'s read is not, with rustc's note
/// (`rustc_trivial_field_reads`). A check records the builtin derives instead, as the tests
/// above have them.
#[test]
fn what_expanded_derives_read() {
    let fixture = r#"//- minicore: clone, copy, eq, ord, fmt, derive, dispatch_from_dyn
//- /lib.rs crate:other edition:2024
#[derive(Clone, Debug, PartialEq)]
pub struct Compared(u64);
#[derive(Clone, Debug)]
pub struct Shown(u64);
"#;
    let checked = crate::tests::checked_expanding_derives(fixture, &crate::Options::default());
    let stderr = checked.stderr();
    let dead: Vec<_> = checked.diags.iter().filter(|it| it.lint_name() == Some("dead_code")).collect();
    assert_eq!(dead.len(), 1, "only `Shown`'s field: {stderr}");
    assert!(stderr.contains("`Shown` has derived impls for the traits"), "{stderr}");
    assert!(!checked.diags.iter().any(|it| it.is_error()), "{stderr}");
}
