//! The lints of errors and order (`error_debug`, `dropped_error`, `untyped_error`,
//! `hash_order_output`) over fixtures of their own: a `hir_ty` whose `Ty` needs the attached
//! database to format, and a `main` crate that says what it expects. A line of `main` marked
//! `//~ error_debug` has that lint's finding; `//~^ error_debug` the line above has one.

use crate::tests::lint;

mod clock;
mod dropped;
mod order;
mod untyped;

/// `ok`, `map_err` and `unwrap_or_default` of `Result` are minicore's under `option`, `fn` and
/// `default` (with `builtin_impls` for `u32: Default`).
const MINICORE: &str =
    "//- minicore: sized, copy, clone, derive, default, builtin_impls, fn, option, result, fmt, write, error, iterators\n";

const HIR_TY: &str = r#"
//- /hir_ty/lib.rs crate:hir_ty
use core::fmt;
pub struct Ty;
impl fmt::Debug for Ty {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}
impl fmt::Display for Ty {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}
pub fn attach_db<R>(f: impl FnOnce() -> R) -> R {
    f()
}
"#;

const MAIN: &str = "//- /src/lib.rs crate:main deps:hir_ty\n";

/// What every `main` of these tests starts with: a `Formatter` to `write!` to, a `Path` that
/// formats with both traits, and a `Holds` that holds `hir_ty`'s `Ty`.
const PRELUDE: &str = r#"
use core::error::Error;
use core::fmt::{self, Arguments, Debug, Display, Formatter};
pub trait Sink {
    fn write_fmt(&mut self, args: Arguments<'_>) -> fmt::Result;
}
impl Sink for Formatter<'_> {
    fn write_fmt(&mut self, _args: Arguments<'_>) -> fmt::Result {
        Ok(())
    }
}
#[derive(Debug)]
pub struct Path;
impl Display for Path {
    fn fmt(&self, _f: &mut Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}
#[derive(Debug)]
pub struct Holds {
    ty: hir_ty::Ty,
}
impl Display for Holds {
    fn fmt(&self, _f: &mut Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}
"#;

const LINTS: &[&str] = &["error_debug", "dropped_error", "untyped_error", "hash_order_output", "wall_clock"];

/// The lint a message is of.
fn lint_of(message: &str) -> Option<&'static str> {
    if message.starts_with("the message of error `") {
        Some("error_debug")
    } else if message.starts_with("this `Err` arm throws away")
        || (message.starts_with("`.") && message.contains("` throws away the "))
    {
        Some("dropped_error")
    } else if message.ends_with(": an error the caller can't match on") {
        Some("untyped_error")
    } else if message.contains("` is iterated in its hasher's order") {
        Some("hash_order_output")
    } else if message.starts_with("use of a disallowed method `std::time::") {
        Some("wall_clock")
    } else {
        None
    }
}

/// The findings of these lints in `src/lib.rs` of `out`, as (line, lint).
fn found(out: &str) -> Vec<(u32, &'static str)> {
    let mut found = Vec::new();
    let mut lint = None;
    for line in out.lines() {
        if let Some(message) = line.strip_prefix("error: ").or_else(|| line.strip_prefix("warning: ")) {
            lint = lint_of(message);
            continue;
        }
        let Some(at) = line.trim_start().strip_prefix("--> src/lib.rs:") else { continue };
        let Some(name) = lint.take() else { continue };
        let number = at.split(':').next().and_then(|it| it.parse().ok());
        found.push((number.expect("a line number after the path"), name));
    }
    found.sort_unstable();
    found
}

/// What the markers of `source`, the whole of `src/lib.rs`, expect, as (line, lint).
fn expected(source: &str) -> Vec<(u32, &'static str)> {
    let mut expected = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let number = index as u32 + 1;
        let Some((_, marks)) = line.split_once("//~") else { continue };
        let (number, marks) = match marks.strip_prefix('^') {
            Some(marks) => (number - 1, marks),
            None => (number, marks),
        };
        for mark in marks.split_whitespace() {
            let lint = LINTS.iter().find(|&&it| it == mark).unwrap_or_else(|| panic!("`{mark}` is not a lint of these tests"));
            expected.push((number, *lint));
        }
    }
    expected.sort_unstable();
    expected
}

/// Lints `main`, after [`PRELUDE`], with `flags`, and checks the findings are what its markers
/// say; returns the output.
fn check(main: &str, flags: &str) -> String {
    check_in(MAIN, main, flags)
}

/// [`check`] with `crates`, the crates beside [`HIR_TY`] and the meta line of `main` last.
fn check_in(crates: &str, main: &str, flags: &str) -> String {
    let source = format!("{}{}", PRELUDE.trim_start_matches('\n'), main.trim_start_matches('\n'));
    let fixture = format!("{MINICORE}{HIR_TY}{crates}{source}");
    let out = lint(&fixture, flags);
    assert_eq!(found(&out), expected(&source), "with `{flags}`, the findings of:\n{source}\nwere:\n{out}");
    out
}

/// An error's own `Display` that formats with `{:?}` (written, captured, through a renamed
/// `format_args!`) or formats `hir_ty`'s `Ty` (itself or held in a field) is a finding; the
/// `Display` of a type that is no error is none.
#[test]
fn error_debug_in_an_errors_display() {
    let out = check(
        r#"
use core::format_args as args;
#[derive(Debug)]
pub struct ParseError {
    path: Path,
    ty: hir_ty::Ty,
    holds: Holds,
}
impl Display for ParseError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let path = &self.path;
        let _ = write!(f, "cannot parse {}", self.path);
        let _ = write!(f, " ({:?})", self.path); //~ error_debug
        let _ = write!(f, " ({path:?})"); //~ error_debug
        let _ = f.write_fmt(args!(" in {:?}", path)); //~ error_debug
        let _ = write!(f, " holding {}", self.holds); //~ error_debug
        write!(f, " of {}", self.ty) //~ error_debug
    }
}
impl Error for ParseError {}

pub struct Report;
impl Display for Report {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} {}", Path, hir_ty::Ty)
    }
}
"#,
        "-A clippy::all --deny=blaze::error_debug",
    );
    assert!(out.contains("it holds `hir_ty`'s `Ty`"), "a holder names what it holds:\n{out}");
    let errors = out.lines().filter(|it| it.starts_with("error: the message of error `ParseError`")).count();
    assert_eq!(errors, 5, "each finding is an error naming the error type:\n{out}");
}

/// A `format_args!` whose value makes an error (through a call, or a macro that writes both) is
/// that error's message; one inside a closure handed to `hir_ty::attach_db` may format a `Ty`,
/// and one that makes no error is none.
#[test]
fn error_debug_in_a_message_made_into_an_error() {
    check(
        r#"
#[derive(Debug)]
pub struct Failed;
impl Display for Failed {
    fn fmt(&self, _f: &mut Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}
impl Error for Failed {}
impl Failed {
    pub fn new(_message: Arguments<'_>) -> Failed {
        Failed
    }
}
pub fn log(_message: Arguments<'_>) {}
macro_rules! fail {
    ($e:expr) => {
        Failed::new(format_args!("bad {:?}", $e))
    };
}
pub fn check(path: &Path, ty: &hir_ty::Ty) -> Result<(), Failed> {
    let _ = Failed::new(format_args!("bad {:?}", path)); //~ error_debug
    let _ = Failed::new(format_args!("bad {}", ty)); //~ error_debug
    let _ = fail!(path); //~ error_debug
    let _ = hir_ty::attach_db(|| Failed::new(format_args!("bad {}", ty)));
    let _ = Failed::new(format_args!("bad {}", path));
    log(format_args!("{:?} {}", path, ty));
    Ok(())
}
"#,
        "-A clippy::all --deny=blaze::error_debug",
    );
}

/// thiserror's `#[error("..")]` templates are an error's message: `{:?}` of a field, or a field
/// that needs the attached database, is a finding at the attribute.
#[test]
fn error_debug_in_an_error_attribute() {
    check(
        r#"
#[derive(Debug)]
#[error("cannot read {path:?}")] //~ error_debug
pub struct ReadError {
    path: Path,
}
impl Display for ReadError {
    fn fmt(&self, _f: &mut Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}
impl Error for ReadError {}

#[derive(Debug)]
pub enum LoadError {
    #[error("no type {0}")] //~ error_debug
    Type(hir_ty::Ty),
    #[error("no file {0}")]
    File(Path),
}
impl Display for LoadError {
    fn fmt(&self, _f: &mut Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}
impl Error for LoadError {}

#[derive(Debug)]
#[error("not an error {0:?}")]
pub struct NotAnError(Path);
"#,
        "-A clippy::all --deny=blaze::error_debug",
    );
}
