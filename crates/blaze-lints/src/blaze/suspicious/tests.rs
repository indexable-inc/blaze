//! The suspicious-code lints over a small `std` with the shapes they look at, each alone on the
//! command line: where each fires, where it does not, what its fix writes and that the fixed
//! file lints clean and fixes to itself. One file of tests per group of lints under `tests/`;
//! this one holds the fixture and the helpers they share.
//!
//! minicore has no `assert_eq!`, no `String`, no `Vec` and no `io`: the fixture's `std` gives
//! them by the paths the real ones have, as `macro_rules` and plain structs, enough for the
//! lints' name resolution and inference. `assert_eq!` is in the prelude, as std's is, so a
//! fixture's call of it resolves to std's macro; it fails through a `-> !` function of std.

mod errors;
mod flow;
mod names;
mod tests_;

use base_db::FileId;
use hir_expand::fixture;
use syntax::{TextRange, TextSize};

use crate::{
    check::{Setup, check},
    conf::Conf,
    fix::fix,
    level::CommandLine,
    tests::{lint, main_crate},
};

pub(super) const MINICORE: &str = "//- minicore: sized, copy, clone, option, result, fn, try, from, drop, default, iterator, iterators, slice, index, range, deref, eq, ord, fmt, panic, assert, builtin_impls, str, reach\n";

/// The `std` every fixture here builds on.
pub(super) const STD: &str = r#"
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod rust_2015 { pub use core::prelude::rust_2015::*; pub use crate::{string::String, vec::Vec, boxed::Box, assert_eq}; }
    pub mod rust_2018 { pub use core::prelude::rust_2018::*; pub use crate::{string::String, vec::Vec, boxed::Box, assert_eq}; }
    pub mod rust_2021 { pub use core::prelude::rust_2021::*; pub use crate::{string::String, vec::Vec, boxed::Box, assert_eq}; }
    pub mod rust_2024 { pub use core::prelude::rust_2024::*; pub use crate::{string::String, vec::Vec, boxed::Box, assert_eq}; }
}
pub use core::{option, result, convert, default, iter, ops, fmt, cmp, marker, mem};
pub fn assert_failed() -> ! { loop {} }
#[macro_export]
macro_rules! assert_eq {
    ($left:expr, $right:expr $(,)?) => {
        match (&$left, &$right) {
            (left, right) => {
                if !(*left == *right) {
                    $crate::assert_failed()
                }
            }
        }
    };
    ($left:expr, $right:expr, $($arg:tt)+) => {
        match (&$left, &$right) {
            (left, right) => {
                if !(*left == *right) {
                    $crate::assert_failed()
                }
            }
        }
    };
}
pub mod vec {
    pub struct Vec<T> { ptr: *const T, len: usize }
    impl<T> Vec<T> {
        pub const fn new() -> Vec<T> { loop {} }
        pub fn len(&self) -> usize { self.len }
        pub fn is_empty(&self) -> bool { self.len == 0 }
        pub fn push(&mut self, value: T) {}
        pub fn pop(&mut self) -> Option<T> { None }
        pub fn clear(&mut self) {}
        pub fn first(&self) -> Option<&T> { None }
    }
    impl<T> Default for Vec<T> {
        fn default() -> Vec<T> { Vec::new() }
    }
    impl<T> core::iter::FromIterator<T> for Vec<T> {
        fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Vec<T> { loop {} }
    }
}
pub mod string {
    pub struct String { len: usize }
    impl String {
        pub const fn new() -> String { String { len: 0 } }
        pub fn len(&self) -> usize { self.len }
        pub fn push_str(&mut self, s: &str) {}
    }
    impl Default for String {
        fn default() -> String { String::new() }
    }
    impl From<&str> for String {
        fn from(s: &str) -> String { String::new() }
    }
    impl core::cmp::PartialEq for String {
        fn eq(&self, other: &String) -> bool { true }
    }
}
pub mod boxed {
    pub struct Box<T: ?Sized> { ptr: *const T }
    impl<T> Box<T> {
        pub fn new(value: T) -> Box<T> { loop {} }
    }
}
pub mod error {
    pub trait Error: core::fmt::Debug + core::fmt::Display {}
    impl<'a, E: Error + 'a> From<E> for crate::boxed::Box<dyn Error + 'a> {
        fn from(e: E) -> crate::boxed::Box<dyn Error + 'a> { loop {} }
    }
}
pub mod io {
    #[derive(Debug)]
    pub struct Error { code: i32 }
    impl core::fmt::Display for Error {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { Ok(()) }
    }
    impl crate::error::Error for Error {}
    pub type Result<T> = core::result::Result<T, Error>;
    pub fn read(path: &str) -> Result<crate::vec::Vec<u8>> { Ok(crate::vec::Vec::new()) }
}
pub mod num {
    #[derive(Debug)]
    pub struct ParseIntError(());
    impl core::fmt::Display for ParseIntError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { Ok(()) }
    }
    impl crate::error::Error for ParseIntError {}
    pub fn parse_u32(text: &str) -> Result<u32, ParseIntError> { Ok(0) }
}
"#;

/// The header of the crate linted: a library, its tests compiled in.
pub(super) const MAIN: &str = "//- /src/lib.rs crate:main deps:std cfg:test\n";

/// The fixture of `main` (which starts with its own `//- /src/lib.rs ..` header when it is not
/// [`MAIN`]'s library) over [`STD`] and `extra` (more crates, each with its header).
pub(super) fn fixture_of(extra: &str, main: &str) -> String {
    let main = main.trim_start_matches('\n');
    let header = if main.starts_with("//- ") { "" } else { MAIN };
    format!("{MINICORE}{}{}{header}{main}", STD.trim_start_matches('\n'), extra.trim_start_matches('\n'))
}

pub(super) fn flags(lint_name: &str) -> String {
    format!("-A clippy::all --warn=blaze::{lint_name}")
}

/// What `blaze::<lint_name>` alone says of `main`.
pub(super) fn found(lint_name: &str, main: &str) -> String {
    found_with(lint_name, "", main)
}

/// What `blaze::<lint_name>` alone says of `main` with the crates `extra` beside `std`.
pub(super) fn found_with(lint_name: &str, extra: &str, main: &str) -> String {
    lint(&fixture_of(extra, main), &flags(lint_name))
}

/// `src/lib.rs` of `main` after `blaze check --fix` with `blaze::<lint_name>` alone on, after
/// checking that the fix changed it, that linting it again finds nothing, and that fixing it
/// again changes nothing.
pub(super) fn fixed(lint_name: &str, main: &str) -> String {
    let fx = fixture::db(&fixture_of("", main));
    let file = *fx.files().last().expect("src/lib.rs");
    let mut db = fx.into_db();
    let krate = blaze_query::db::attach(&db, || main_crate(&db));
    let file = file.file_id(&db);
    let name_of = |it: FileId| (it == file).then(|| "src/lib.rs".to_owned());
    let flags: Vec<String> = flags(lint_name).split_whitespace().map(str::to_owned).collect();
    let setup = Setup {
        command_line: CommandLine::parse(&flags).expect("the flags read"),
        conf: Conf::default(),
    };
    let before = db.file_text(file).text(&db).to_string();
    let made = fix(&mut db, krate, &setup, &name_of).unwrap_or_else(|error| panic!("{error}"));
    let after = db.file_text(file).text(&db).to_string();
    assert_ne!(after, before, "the fix of `{lint_name}` changed the file");
    assert!(made.applied > 0, "the fix of `{lint_name}` made a suggestion");
    blaze_query::db::attach(&db, || {
        let found = check(&db, krate, &setup, &name_of).expect("the file is named");
        let left: Vec<&str> = found.iter().map(|it| it.diagnostic.message.as_str()).collect();
        assert!(left.is_empty(), "the fixed file lints clean:\n{after}\n{left:#?}");
    });
    let again = fix(&mut db, krate, &setup, &name_of).unwrap_or_else(|error| panic!("{error}"));
    let twice = db.file_text(file).text(&db).to_string();
    assert_eq!(again.applied, 0, "the fixed file has nothing left to fix:\n{twice}");
    assert_eq!(twice, after, "fixing the fixed file changes nothing");
    after
}

/// The text of `src/lib.rs`, the messages `blaze::<lint_name>` says of it, and the parts of the
/// first suggestion of each, in the order of the file.
fn suggested(lint_name: &str, main: &str) -> (String, Vec<String>, Vec<(TextRange, String)>) {
    let fx = fixture::db(&fixture_of("", main));
    let file = *fx.files().last().expect("src/lib.rs");
    let db = fx.into_db();
    let krate = blaze_query::db::attach(&db, || main_crate(&db));
    let file = file.file_id(&db);
    let name_of = |it: FileId| (it == file).then(|| "src/lib.rs".to_owned());
    let flags: Vec<String> = flags(lint_name).split_whitespace().map(str::to_owned).collect();
    let setup = Setup {
        command_line: CommandLine::parse(&flags).expect("the flags read"),
        conf: Conf::default(),
    };
    let text = db.file_text(file).text(&db).to_string();
    let found = blaze_query::db::attach(&db, || {
        check(&db, krate, &setup, &name_of).expect("the file is named")
    });
    let messages = found.iter().map(|it| it.diagnostic.message.clone()).collect();
    let mut parts: Vec<(TextRange, String)> = found
        .iter()
        .filter_map(|it| it.diagnostic.suggestions.first()?.substitutions.first())
        .flatten()
        .map(|(span, text)| {
            (TextRange::new(TextSize::new(span.lo), TextSize::new(span.hi)), text.clone())
        })
        .collect();
    parts.sort_by_key(|(range, _)| (range.start(), range.end()));
    parts.dedup();
    (text, messages, parts)
}

/// `src/lib.rs` of `main` with every suggestion `blaze::<lint_name>` makes applied, `MaybeIncorrect`
/// ones too, after checking that the result lints clean and is offered nothing more.
pub(super) fn offered(lint_name: &str, main: &str) -> String {
    let (before, _, parts) = suggested(lint_name, main);
    assert!(!parts.is_empty(), "`{lint_name}` offers a fix of:\n{before}");
    let mut after = before.clone();
    for (range, text) in parts.iter().rev() {
        after.replace_range(std::ops::Range::<usize>::from(*range), text);
    }
    assert_ne!(after, before, "the suggestions of `{lint_name}` changed the file");
    let (_, left, more) = suggested(lint_name, &after);
    assert!(left.is_empty(), "the file the suggestions made lints clean:\n{after}\n{left:#?}");
    assert!(more.is_empty(), "the file the suggestions made has nothing left to offer");
    after
}
