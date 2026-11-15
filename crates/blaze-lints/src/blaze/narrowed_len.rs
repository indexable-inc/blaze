//! `narrowed_len`: a length narrowed to a smaller integer type with a panic for the case it
//! does not fit (a 4 GiB file panicked the daemon's main thread at "a record under 4 GiB").
//!
//! A finding, outside test code ([`test_code::is_test_code`]), is a conversion
//! `T::try_from(x)` or `x.try_into()` to an integer type `T` narrower than `x`'s, whose result
//! is at once `.expect(..)`ed or `.unwrap()`ped, where `x` names a length: it is or holds a call
//! of a method or function named `len`, `count`, `size` or `size_of`, or a path or field whose
//! name has one of the `_`-separated words `len`, `length`, `size`, `offset`, `count` or
//! `bytes`. `usize` and `isize` count as 32 bits wide as a target and 64 as a source, so
//! `usize` to `u32` and `u64` to `usize` narrow and `u32` to `usize` does not. An `as` cast is
//! `blaze::as_cast`'s; a conversion whose error is handled (`map_err(..)?`, `unwrap_or(..)`)
//! is not a finding.
//!
//! There is no fix the lint can write: the function returns a typed error naming the thing and
//! its size (`RecordTooLarge { what, len }`), or, where a check just before bounds the value,
//! the function says where with `#[expect(blaze::narrowed_len, reason = "..")]`.

use syntax::view::{self as ast, AstNode as _, HasArgList as _};

use super::{Lint, test_code};
use crate::check::Cx;

const LINT: Lint = Lint::NarrowedLen;

/// The `_`-separated words of a name that make it a length's.
const LENGTH_WORDS: &[&str] = &["len", "length", "size", "offset", "count", "bytes"];

/// The calls whose value is a length.
const LENGTH_CALLS: &[&str] = &["len", "count", "size", "size_of"];

/// How many nodes of `x` are looked at for a length's name.
const LOOKED_AT: usize = 64;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    let Some(call) = ast::MethodCallExpr::cast(node) else { return };
    if !cx.on_blaze(LINT) || cx.expansion().is_some() || !cx.is_written(node) {
        return;
    }
    let Some(panics) = call.name_ref() else { return };
    if !matches!(panics.text(), "expect" | "unwrap") {
        return;
    }
    let Some(conversion) = call.receiver() else { return };
    let Some((x, written)) = converted(conversion) else { return };
    let Some(to) = cx.ty(conversion).and_then(|it| it.type_arguments().next()).and_then(|it| width(&it, false))
    else {
        return;
    };
    let Some(from) = cx.ty(x).and_then(|it| width(&it.strip_references(), true)) else { return };
    if to.1 >= from.1 || !names_length(x) {
        return;
    }
    if test_code::is_test_code(cx, node) {
        return;
    }
    let Some(range) = cx.range(node) else { return };
    let message = format!(
        "a length (`{}`, a `{}`) is narrowed to `{}` by `{written}` and `{}` panics when it does not fit",
        cx.source_of(x.syntax()),
        from.0,
        to.0,
        panics.text()
    );
    let span = cx.span(range);
    cx.blaze(LINT, node, span, message)
        .note(
            "a length is as large as its input: a file, a record or a buffer past the narrower type's range \
             panics the thread that reads it, where a typed error naming the thing and its size would refuse it",
        )
        .help(LINT.help())
        .help(format!("for further information visit {}", LINT.docs_url()));
}

/// The value `expr` converts and how it is written, when it is `T::try_from(x)` or
/// `x.try_into()`.
fn converted(expr: ast::Expr<'_>) -> Option<(ast::Expr<'_>, &'static str)> {
    match expr {
        ast::Expr::CallExpr(call) => {
            let ast::Expr::PathExpr(callee) = call.expr()? else { return None };
            let name = callee.path()?.segment()?.name_ref()?;
            if name.text() != "try_from" {
                return None;
            }
            let mut args = call.arg_list()?.args();
            let x = args.next()?;
            args.next().is_none().then_some((x, "try_from"))
        }
        ast::Expr::MethodCallExpr(call) => {
            let name = call.name_ref()?;
            let no_args = call.arg_list().is_some_and(|it| it.args().next().is_none());
            (name.text() == "try_into" && no_args).then_some((call.receiver()?, "try_into"))
        }
        _ => None,
    }
}

/// The integer type `ty` is and its width in bits: `usize` and `isize` as 64 when `source`,
/// else as 32.
fn width(ty: &hir::Type<'_>, source: bool) -> Option<(&'static str, u32)> {
    let builtin = ty.as_builtin()?;
    let name = builtin.name();
    let pointer = if source { 64 } else { 32 };
    let found = match name.as_str() {
        "u8" => ("u8", 8),
        "i8" => ("i8", 8),
        "u16" => ("u16", 16),
        "i16" => ("i16", 16),
        "u32" => ("u32", 32),
        "i32" => ("i32", 32),
        "u64" => ("u64", 64),
        "i64" => ("i64", 64),
        "u128" => ("u128", 128),
        "i128" => ("i128", 128),
        "usize" => ("usize", pointer),
        "isize" => ("isize", pointer),
        _ => return None,
    };
    Some(found)
}

/// Whether `x` names a length: a call of [`LENGTH_CALLS`], or a path or field one of whose
/// name's words is in [`LENGTH_WORDS`], anywhere in it.
fn names_length(x: ast::Expr<'_>) -> bool {
    x.syntax().descendants().take(LOOKED_AT).any(|node| {
        if let Some(call) = ast::MethodCallExpr::cast(node) {
            return call.name_ref().is_some_and(|it| LENGTH_CALLS.contains(&it.text()));
        }
        if let Some(call) = ast::CallExpr::cast(node) {
            return matches!(call.expr(), Some(ast::Expr::PathExpr(it))
                if it.path().and_then(|it| it.segment()).and_then(|it| it.name_ref())
                    .is_some_and(|it| LENGTH_CALLS.contains(&it.text())));
        }
        if let Some(field) = ast::FieldExpr::cast(node) {
            return field.name_ref().is_some_and(|it| has_word(it.text(), LENGTH_WORDS));
        }
        if let Some(path) = ast::PathExpr::cast(node) {
            return path
                .path()
                .and_then(|it| it.segment())
                .and_then(|it| it.name_ref())
                .is_some_and(|it| has_word(it.text(), LENGTH_WORDS));
        }
        false
    })
}

/// Whether one of the `_`-separated words of `name`, any case, is one of `words`.
pub(super) fn has_word(name: &str, words: &[&str]) -> bool {
    name.split('_').any(|part| words.iter().any(|word| part.eq_ignore_ascii_case(word)))
}

#[cfg(test)]
mod tests {
    use crate::tests::lint;

    const FOUND: &str = "panics when it does not fit";

    const CRATES: &str = r#"
//- minicore: sized, copy, clone, option, result, fn, try, from, slice, index
//- /src/lib.rs crate:main
mod conversions {
    use core::convert::TryFrom;
    impl TryFrom<usize> for u32 { type Error = (); fn try_from(_: usize) -> Result<u32, ()> { loop {} } }
    impl TryFrom<usize> for u16 { type Error = (); fn try_from(_: usize) -> Result<u16, ()> { loop {} } }
    impl TryFrom<usize> for u8 { type Error = (); fn try_from(_: usize) -> Result<u8, ()> { loop {} } }
    impl TryFrom<u64> for u16 { type Error = (); fn try_from(_: u64) -> Result<u16, ()> { loop {} } }
    impl TryFrom<u64> for u32 { type Error = (); fn try_from(_: u64) -> Result<u32, ()> { loop {} } }
    impl TryFrom<u32> for usize { type Error = (); fn try_from(_: u32) -> Result<usize, ()> { loop {} } }
}
"#;

    fn found(main: &str) -> String {
        let fixture = format!("{}{}", CRATES.trim_start_matches('\n'), main.trim_start_matches('\n'));
        lint(&fixture, "-A clippy::all --warn=blaze::narrowed_len")
    }

    /// The bug in miniature: a record's length narrowed to the format's `u32` with a panic,
    /// written both ways.
    #[test]
    fn a_length_narrowed_with_a_panic_is_found() {
        let out = found(
            r#"
use core::convert::{TryFrom, TryInto};
pub struct Record { pub data_len: u64 }
pub fn header(bytes: &[u8], record: &Record) -> (u32, u16) {
    let len = u32::try_from(bytes.len()).expect("a record under 4 GiB");
    let short: u16 = record.data_len.try_into().unwrap();
    (len, short)
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 2, "{out}");
        assert!(out.contains("a `usize`) is narrowed to `u32` by `try_from`"), "{out}");
    }

    /// Not a finding: a widening, a value that names no length, a handled error, and a test.
    #[test]
    fn a_widening_an_unnamed_value_a_handled_error_and_a_test_are_left_alone() {
        let out = found(
            r#"
use core::convert::{TryFrom, TryInto};
pub fn ok(n: u32, id: u64, bytes: &[u8]) -> Option<(usize, u32, u32)> {
    let wide = usize::try_from(n).unwrap();
    let id: u32 = id.try_into().unwrap();
    let handled = u32::try_from(bytes.len()).ok()?;
    Some((wide, id, handled))
}
#[cfg(test)]
mod tests {
    pub fn small(bytes: &[u8]) -> u8 {
        u8::try_from(bytes.len()).unwrap()
    }
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
    }

    /// A bound checked just before keeps the conversion, said by an expectation on the fn,
    /// which the finding fulfils.
    #[test]
    fn an_expectation_on_the_fn_is_fulfilled() {
        let out = found(
            r#"
use core::convert::TryFrom;
#[expect(blaze::narrowed_len, reason = "the caller's assert bounds a frame to 64 KiB")]
pub fn frame(bytes: &[u8]) -> u16 {
    u16::try_from(bytes.len()).expect("a frame under 64 KiB")
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
        assert!(!out.contains("unfulfilled"), "{out}");
    }
}
