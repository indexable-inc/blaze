//! The lints run over crates written out as rust-analyzer's fixtures write them, their
//! diagnostics compared with what clippy writes of the same source.

use base_db::{Db, FileId};
use blaze_diagnostics::emit::{Sources, Style};
use blaze_hash::HashMap;
use hir_expand::fixture;

use crate::{
    check::{Setup, check},
    conf::Conf,
    level::CommandLine,
};

/// What `std` is to the fixtures: the macros the lints look for, as `std` writes them.
pub(crate) const STD: &str = r#"
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod v1 {
        pub use crate::{assert_eq, assert_ne, debug_assert_eq, test, vec};
        pub use core::prelude::v1::*;
    }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
#[macro_export]
macro_rules! assert_eq {
    ($left:expr, $right:expr $(,)?) => {
        match (&$left, &$right) {
            (left_val, right_val) => {
                if !(*left_val == *right_val) {
                    loop {}
                }
            }
        }
    };
}
#[macro_export]
macro_rules! assert_ne {
    ($left:expr, $right:expr $(,)?) => {
        match (&$left, &$right) {
            (left_val, right_val) => {
                if *left_val == *right_val {
                    loop {}
                }
            }
        }
    };
}
#[macro_export]
macro_rules! debug_assert_eq {
    ($($arg:tt)*) => {
        if true {
            $crate::assert_eq!($($arg)*);
        }
    };
}
#[rustc_builtin_macro]
pub macro test($item:item) {}
#[macro_export]
macro_rules! vec {
    ($($x:expr),* $(,)?) => {
        [$($x),*]
    };
}
"#;

/// The diagnostics clippy's lints have in the crate `main` of `fixture`, rustc's flags `flags`
/// given, each as rustc writes it.
pub(crate) fn lint(fixture: &str, flags: &str) -> String {
    lint_with(fixture, flags, Conf::default())
}

/// [`lint`] with the `clippy.toml` configuration `conf`. Each fixture is linted twice, on two
/// databases, with the lints dispatched on the node's kind and as each cast the node for
/// itself before ([`crate::lints::check_node_by_casts`]): the two write the same.
pub(crate) fn lint_with(fixture: &str, flags: &str, conf: Conf) -> String {
    lint_given(fixture, flags, conf, |_| {})
}

/// [`lint_with`], `given` done to each database's fixture first (a proc-macro crate given its
/// macros, [`fixture::Fixture::set_proc_macros`]).
pub(crate) fn lint_given(fixture: &str, flags: &str, conf: Conf, given: fn(&mut fixture::Fixture)) -> String {
    let by_kind = lint_once(fixture, flags, conf.clone(), given);
    crate::lints::BY_CASTS.set(true);
    let by_casts = lint_once(fixture, flags, conf, given);
    crate::lints::BY_CASTS.set(false);
    assert_eq!(by_kind, by_casts, "the dispatch on the kind finds what the casts found");
    by_kind
}

fn lint_once(fixture: &str, flags: &str, conf: Conf, given: fn(&mut fixture::Fixture)) -> String {
    let mut built = fixture::db(fixture);
    given(&mut built);
    let db = built.into_db();
    hir::attach_db(&db, || {
        let krate = main_crate(&db);
        let mut names: HashMap<FileId, String> = HashMap::default();
        let mut texts = Vec::new();
        for krate in hir::Crate::all(&db) {
            for module in krate.modules(&db) {
                let Some(file) = module.definition_source_file_id(&db).file_id() else { continue };
                let file = file.file_id(&db);
                let root = db.file_source_root(file).source_root_id(&db);
                let root = db.source_root(root).source_root(&db);
                let Some(path) = root.path_for_file(&file) else { continue };
                let name = path.to_string().trim_start_matches('/').to_owned();
                if names.insert(file, name.clone()).is_none() {
                    texts.push((name, db.file_text(file).text(&db).to_string()));
                }
            }
        }
        let flags: Vec<String> = flags.split_whitespace().map(str::to_owned).collect();
        let setup = Setup { command_line: CommandLine::parse(&flags).expect("the flags read"), conf };
        let name_of = |file: FileId| names.get(&file).cloned();
        let found = check(&db, krate, &setup, &name_of).expect("every file of the crates is named");
        let mut sources = Sources::of(texts);
        crate::serve::written(found.iter(), &mut sources, |_| None, Style::default())
            .expect("every file a lint points into is given")
            .into_iter()
            .map(|it| it.rendered)
            .collect()
    })
}

/// The crate `main` of a fixture's database.
pub(crate) fn main_crate(db: &Db) -> hir::Crate {
    hir::Crate::all(db)
        .into_iter()
        .find(|it| it.display_name(db).is_some_and(|it| it.to_string() == "main"))
        .expect("the fixture has a crate `main`")
}

#[test]
fn lints_what_an_attribute_macro_is_handed() {
    let fixture = format!(
        r#"
//- minicore: copy, unary_ops, bool_impl
//- /src/lib.rs crate:main deps:std cfg:test
#[cfg(test)]
mod tests {{
    #[test]
    fn asserts() {{
        assert_eq!(true, 1 > 0);
    }}
}}
{STD}"#
    );
    expect_test::expect![[r#"
        warning: used `assert_eq!` with a literal bool
         --> src/lib.rs:5:9
          |
        5 |         assert_eq!(true, 1 > 0);
          |         ^^^^^^^^^^^^^^^^^^^^^^^
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#bool_assert_comparison
          = note: `#[warn(clippy::bool_assert_comparison)]` on by default
        help: replace it with `assert!(..)`
          |
        5 -         assert_eq!(true, 1 > 0);
        5 +         assert!(1 > 0);
          |

    "#]].assert_eq(&lint(&fixture, ""));
}

#[test]
fn bool_assert_comparison() {
    let fixture = format!(
        r#"
//- minicore: copy, unary_ops, bool_impl
//- /src/lib.rs crate:main deps:std
fn f() {{
    assert_eq!(true, 1 > 0);
    assert_ne!(1 > 0, false);
}}
{STD}"#
    );
    expect_test::expect![[r#"
        warning: used `assert_eq!` with a literal bool
         --> src/lib.rs:2:5
          |
        2 |     assert_eq!(true, 1 > 0);
          |     ^^^^^^^^^^^^^^^^^^^^^^^
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#bool_assert_comparison
          = note: `#[warn(clippy::bool_assert_comparison)]` on by default
        help: replace it with `assert!(..)`
          |
        2 -     assert_eq!(true, 1 > 0);
        2 +     assert!(1 > 0);
          |

        warning: used `assert_ne!` with a literal bool
         --> src/lib.rs:3:5
          |
        3 |     assert_ne!(1 > 0, false);
          |     ^^^^^^^^^^^^^^^^^^^^^^^^
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#bool_assert_comparison
        help: replace it with `assert!(..)`
          |
        3 -     assert_ne!(1 > 0, false);
        3 +     assert!(1 > 0);
          |

    "#]].assert_eq(&lint(&fixture, ""));
}

#[test]
fn useless_borrows_in_formatting() {
    let fixture = format!(
        r#"
//- minicore: fmt, sized, builtin_impls
//- /src/lib.rs crate:main deps:std
fn f(s: &str, n: i32, m: &mut i32) {{
    print!("{{}} {{:?}}", &s, &&n);
    print!("{{0:1$}}", &n, &5usize);
    print!("{{}}", &mut *m);
    print!("{{:p}}", &n);
    print!("{{n}} {{n}}", n = &(n));
}}
{STD}"#
    );
    expect_test::expect![[r#"
        warning: redundant reference in `print!` argument
         --> src/lib.rs:2:23
          |
        2 |     print!("{} {:?}", &s, &&n);
          |                       ^^ help: remove the redundant `&`: `s`
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#useless_borrows_in_formatting
          = note: `#[warn(clippy::useless_borrows_in_formatting)]` on by default

        warning: redundant reference in `print!` argument
         --> src/lib.rs:2:27
          |
        2 |     print!("{} {:?}", &s, &&n);
          |                           ^^^ help: remove the redundant `&`: `n`
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#useless_borrows_in_formatting

        warning: redundant reference in `print!` argument
         --> src/lib.rs:3:22
          |
        3 |     print!("{0:1$}", &n, &5usize);
          |                      ^^ help: remove the redundant `&`: `n`
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#useless_borrows_in_formatting

        warning: redundant reference in `print!` argument
         --> src/lib.rs:3:26
          |
        3 |     print!("{0:1$}", &n, &5usize);
          |                          ^^^^^^^ help: remove the redundant `&`: `5usize`
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#useless_borrows_in_formatting

        warning: redundant reference in `print!` argument
         --> src/lib.rs:4:18
          |
        4 |     print!("{}", &mut *m);
          |                  ^^^^^^^ help: remove the redundant `&mut`: `*m`
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#useless_borrows_in_formatting

        warning: redundant reference in `print!` argument
         --> src/lib.rs:6:27
          |
        6 |     print!("{n} {n}", n = &(n));
          |                           ^^^^ help: remove the redundant `&`: `(n)`
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#useless_borrows_in_formatting

    "#]].assert_eq(&lint(&fixture, ""));
}

#[test]
fn map_clone() {
    let fixture = format!(
        r#"
//- minicore: iterators, clone, copy, builtin_impls
//- /src/lib.rs crate:main deps:std
struct I<'a>(&'a u8);
impl<'a> Iterator for I<'a> {{
    type Item = &'a u8;
    fn next(&mut self) -> Option<&'a u8> {{
        None
    }}
}}
fn f(a: I<'_>, b: I<'_>, c: I<'_>) {{
    let _ = a.map(|x| *x);
    let _ = b.map(|&x| x);
    let _ = c.map(|x| x.clone());
}}
{STD}"#
    );
    expect_test::expect![[r#"
        warning: you are using an explicit closure for copying elements
         --> src/lib.rs:9:13
          |
        9 |     let _ = a.map(|x| *x);
          |             ^^^^^^^^^^^^^ help: consider calling the dedicated `copied` method: `a.copied()`
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#map_clone
          = note: `#[warn(clippy::map_clone)]` on by default

        warning: you are using an explicit closure for copying elements
          --> src/lib.rs:10:13
           |
        10 |     let _ = b.map(|&x| x);
           |             ^^^^^^^^^^^^^ help: consider calling the dedicated `copied` method: `b.copied()`
           |
           = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#map_clone

        warning: you are using an explicit closure for copying elements
          --> src/lib.rs:11:13
           |
        11 |     let _ = c.map(|x| x.clone());
           |             ^^^^^^^^^^^^^^^^^^^^ help: consider calling the dedicated `copied` method: `c.copied()`
           |
           = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#map_clone

        warning: using `clone` on type `u8` which implements the `Copy` trait
          --> src/lib.rs:11:23
           |
        11 |     let _ = c.map(|x| x.clone());
           |                       ^^^^^^^^^ help: try dereferencing it: `*x`
           |
           = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#clone_on_copy
           = note: `#[warn(clippy::clone_on_copy)]` on by default

    "#]].assert_eq(&lint(&fixture, ""));
}

#[test]
fn match_like_matches_macro() {
    let fixture = format!(
        r#"
//- minicore: option, copy, builtin_impls
//- /src/lib.rs crate:main deps:std
fn f(x: Option<u8>) -> (bool, bool, bool) {{
    let a = match x {{
        Some(0) => true,
        _ => false,
    }};
    let b = if let Some(1) = x {{ false }} else {{ true }};
    let c = match x {{
        Some(_) => true,
        _ => false,
    }};
    (a, b, c)
}}
{STD}"#
    );
    expect_test::expect![[r#"
        warning: match expression looks like `matches!` macro
         --> src/lib.rs:2:13
          |
        2 |       let a = match x {
          |  _____________^
        3 | |         Some(0) => true,
        4 | |         _ => false,
        5 | |     };
          | |_____^
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#match_like_matches_macro
          = note: `#[warn(clippy::match_like_matches_macro)]` on by default
        help: use `matches!` directly
          |
        2 -     let a = match x {
        3 -         Some(0) => true,
        4 -         _ => false,
        5 -     };
        2 +     let a = matches!(x, Some(0));
          |

        warning: `if let .. else` expression looks like `matches!` macro
         --> src/lib.rs:6:13
          |
        6 |     let b = if let Some(1) = x { false } else { true };
          |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#match_like_matches_macro
        help: use `matches!` directly
          |
        6 -     let b = if let Some(1) = x { false } else { true };
        6 +     let b = !matches!(x, Some(1));
          |

    "#]].assert_eq(&lint(&fixture, ""));
}

#[test]
fn needless_borrow_of_a_ref_pattern() {
    let fixture = format!(
        r#"
//- minicore: option, copy, builtin_impls, deref
//- /src/lib.rs crate:main deps:std
fn f(x: Option<&u8>) -> u8 {{
    match x {{
        Some(ref y) => **y,
        None => 0,
    }}
}}
fn g(x: Option<&u8>) -> Option<&&u8> {{
    match x {{
        Some(ref y) => Some(y),
        None => None,
    }}
}}
{STD}"#
    );
    expect_test::expect![[r#"
        warning: this pattern creates a reference to a reference
         --> src/lib.rs:3:14
          |
        3 |         Some(ref y) => **y,
          |              ^^^^^
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#needless_borrow
          = note: `#[warn(clippy::needless_borrow)]` on by default
        help: try
          |
        3 -         Some(ref y) => **y,
        3 +         Some(y) => *y,
          |

    "#]].assert_eq(&lint(&fixture, ""));
}

#[test]
fn new_without_default() {
    let fixture = format!(
        r#"
//- minicore: default
//- /src/lib.rs crate:main deps:std
pub struct Foo;
impl Foo {{
    /// Makes one.
    pub fn new() -> Self {{
        Foo
    }}
}}
{STD}"#
    );
    expect_test::expect![[r#"
        warning: you should consider adding a `Default` implementation for `Foo`
          --> src/lib.rs:4:5
           |
         4 | /     pub fn new() -> Self {
         5 | |         Foo
         6 | |     }
           | |_____^
           |
           = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#new_without_default
           = note: `#[warn(clippy::new_without_default)]` on by default
        help: try adding this
           |
         7 ~ }
         8 + 
         9 + impl Default for Foo {
        10 +     fn default() -> Self {
        11 +         Self::new()
        12 +     }
        13 + }
           |

    "#]].assert_eq(&lint(&fixture, ""));
}

#[test]
fn needless_return() {
    let fixture = format!(
        r#"
//- minicore: copy, builtin_impls
//- /src/lib.rs crate:main deps:std
fn f(x: bool) -> u8 {{
    if x {{
        return 1;
    }} else {{
        return 2;
    }}
}}
{STD}"#
    );
    expect_test::expect![[r#"
        warning: unneeded `return` statement
         --> src/lib.rs:3:9
          |
        3 |         return 1;
          |         ^^^^^^^^
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#needless_return
          = note: `#[warn(clippy::needless_return)]` on by default
        help: remove `return`
          |
        3 -         return 1;
        3 +         1
          |

        warning: unneeded `return` statement
         --> src/lib.rs:5:9
          |
        5 |         return 2;
          |         ^^^^^^^^
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#needless_return
        help: remove `return`
          |
        5 -         return 2;
        5 +         2
          |

    "#]].assert_eq(&lint(&fixture, ""));
}
