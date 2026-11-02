//! `rc_buffer`: `Rc` or `Arc` of a buffer that owns and can grow (`String`, `Vec<T>`,
//! `PathBuf`, `OsString`), where the borrowed form (`str`, `[T]`) serves. Run by clippy's
//! `Types` pass, in [`super::box_collection`].
//!
//! Follows clippy's `clippy_lints/src/types/rc_buffer.rs` (MIT or Apache-2.0), ported from what
//! is known of its source at rust-lang/rust's nightly of 2026-10: no reference checkout of
//! clippy was at hand.

use syntax::view::{self as ast, AstNode as _};

use super::{
    Implemented,
    box_collection::{Std, generic_tys, std_of},
};
use crate::{check::Cx, diag::Applicability};

const LINT: Implemented = Implemented::RcBuffer;

/// clippy's `rc_buffer::check` of the path type `path`, which names `outer`: whether the walk
/// stops there. As in clippy, `Rc<String>` is reported and the walk goes on into it; `Rc<Vec<T>>`
/// is reported and stops it.
pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, path: ast::PathType<'t>, outer: Std) -> bool {
    let name = match outer {
        Std::Rc => "Rc",
        Std::Arc => "Arc",
        _ => return false,
    };
    let first = generic_tys(path).next();
    if let Some(alternate) = first.and_then(|it| buffer_type(cx, it)) {
        report(cx, path, name, format!("{name}<{alternate}>"));
        return false;
    }
    let Some(first) = first else { return false };
    if std_of(cx, first) != Some(Std::Vec) {
        return false;
    }
    let ast::Type::PathType(vec) = first else { return false };
    let Some(inner) = generic_tys(vec).next() else { return false };
    let element = cx.range(inner.syntax()).map_or("..", |it| cx.snippet(it));
    report(cx, path, name, format!("{name}<[{element}]>"));
    true
}

/// clippy's `match_buffer_type`: what the buffer `ty` borrows as.
fn buffer_type<'t>(cx: &Cx<'_, 't>, ty: ast::Type<'t>) -> Option<&'static str> {
    match std_of(cx, ty)? {
        Std::OsString => Some("std::ffi::OsStr"),
        Std::PathBuf => Some("std::path::Path"),
        Std::String => Some("str"),
        _ => None,
    }
}

fn report<'t>(cx: &mut Cx<'_, 't>, path: ast::PathType<'t>, name: &str, code: String) {
    if !cx.on(LINT) {
        return;
    }
    let Some(range) = cx.range(path.syntax()) else { return };
    let span = cx.span(range);
    cx.lint(LINT, path.syntax(), span.clone(), format!("usage of `{name}<T>` when T is a buffer type"))
        .suggestion(span, "try", code, Applicability::Unspecified);
}
