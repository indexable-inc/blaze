//! `path_to_str_unwrap`: a path made a `&str` in a way that is wrong on a path that is not
//! UTF-8, which every Unix allows and Windows makes of unpaired surrogates:
//!
//! - `Path::to_str(..)` or `OsStr::to_str(..)` then `.unwrap()` or `.expect(..)`: a panic on
//!   the first such name (a file a user made, a checkout of someone else's repository).
//! - `Path::to_string_lossy(..)` or `OsStr::to_string_lossy(..)` outside display code: the name
//!   is changed (each bad byte becomes U+FFFD) and nothing says so, so a path built from it
//!   names another file, and two names can become one key. Display code is a `fmt` of
//!   `Display` or `Debug`, or a macro's arguments (`format!`, `println!`, `tracing`'s), which
//!   this lint never looks into.
//!
//! The right forms: keep the `Path`/`OsStr` (`join`, `as_os_str`, `as_encoded_bytes`) where the
//! name goes back to the OS or into a key; `to_str().ok_or_else(..)?` with an error naming the
//! path where UTF-8 is truly needed; `.display()` where it is shown. Outside test code. No fix:
//! which of these is right is the author's choice.

use syntax::view::{self as ast, AstNode as _};

use super::{in_test, inherent_of, receiver_of, report, unwrap_of};
use crate::{blaze::Lint, check::Cx};

const LINT: Lint = Lint::PathToStrUnwrap;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let Some(call) = ast::MethodCallExpr::cast(node) else { return };
    let Some(f) = cx.method(call) else { return };
    let Some(of) = path_like(cx, f) else { return };
    let (message, note, range) = match f.name(cx.db()).as_str() {
        "to_str" => {
            let Some(unwrap) = receiver_of(node) else { return };
            let Some(how) = unwrap_of(cx, unwrap) else { return };
            let Some(range) = cx.range(unwrap.syntax()) else { return };
            (
                format!(
                    "`{of}::to_str` is `None` for a name that is not UTF-8, and `{how}` panics there"
                ),
                "keep the path where it goes back to the OS (`join`, `as_os_str`); where UTF-8 is truly needed, `to_str().ok_or_else(..)?` with an error naming the path",
                range,
            )
        }
        "to_string_lossy" => {
            if display_code(cx, node) {
                return;
            }
            let Some(range) = cx.range(node) else { return };
            (
                format!(
                    "`{of}::to_string_lossy` changes a name that is not UTF-8 and says nothing, so what is built from it can name another file"
                ),
                "keep the path (`join`, `as_os_str`, `as_encoded_bytes`) where it goes back to the OS or into a key; `.display()` where it is shown",
                range,
            )
        }
        _ => return,
    };
    if in_test(cx, node) {
        return;
    }
    report(cx, LINT, node, range, message, note, None);
}

/// Which of `Path` and `OsStr` `f` is an inherent method of.
fn path_like(cx: &Cx<'_, '_>, f: hir::Function) -> Option<&'static str> {
    if inherent_of(cx, f, &["std", "path", "Path"]) {
        return Some("Path");
    }
    inherent_of(cx, f, &["std", "ffi", "OsStr"]).then_some("OsStr")
}

/// Whether `node` is in display code the lint can see: a `fmt` of an impl of `Display` or
/// `Debug`. A macro's arguments are never looked at, so `format!("{}", p.to_string_lossy())`
/// is not found at all.
fn display_code<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>) -> bool {
    let db = cx.db();
    let Some(f) = node.ancestors().find_map(ast::Fn::cast) else { return false };
    let Some(def) = cx.fn_def(f) else { return false };
    if def.name(db).as_str() != "fmt" {
        return false;
    }
    let Some(hir::AssocItemContainer::Impl(imp)) =
        hir::AsAssocItem::as_assoc_item(def, db).map(|it| it.container(db))
    else {
        return false;
    };
    let display = cx.std_trait(&["core", "fmt", "Display"]);
    let debug = cx.std_trait(&["core", "fmt", "Debug"]);
    let trait_ = imp.trait_(db);
    trait_.is_some() && (trait_ == display || trait_ == debug)
}
