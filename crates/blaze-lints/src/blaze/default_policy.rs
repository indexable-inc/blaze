//! `default_policy`: a type with a field marked `#[blaze::policy]` (a cost or a guarantee its
//! maker decides: a durability, a bound, a buffer's size) that can be made without saying it,
//! through `impl Default`, `#[derive(Default)]` or a `new()` that takes nothing
//! (docs/v2/plan/design/std.md, "The second rule": a cost or a guarantee the caller should
//! decide is an argument or a type, never a default).
//!
//! The mark is on the field, where the cost is; the lint finds the three ways a value of the
//! type is made with the field chosen for the caller:
//!
//! - `impl Default for T`, found at the impl, by the trait it resolves to;
//! - `#[derive(Default)]` on `T`, found at the derive;
//! - an inherent `fn new()` with no parameter, found at the function.
//!
//! Not checked: a constructor that takes other arguments and still picks the marked field
//! itself (that is `implicit_policy`'s, which reads the effect summaries), a `const` of the
//! type (a named value written at the call, as `Limits::USUAL` is), and code under
//! `#[cfg(test)]` or a macro's expansion. There is no fix: the argument is the author's.

use syntax::view::{self as ast, AstNode as _, HasAttrs as _, HasName as _};

use super::Lint;
use crate::check::{self, Cx};

const LINT: Lint = Lint::DefaultPolicy;

/// The attribute that marks a field as a cost or a guarantee.
const MARK: &str = "blaze::policy";

/// The name of the first field of `adt` marked [`MARK`], if it is a struct with one. `adt`
/// may be written in another file than the one being linted, so the name is read from its
/// own text.
fn policy_field(adt: ast::Adt<'_>) -> Option<String> {
    let ast::Adt::Struct(item) = adt else { return None };
    let Some(ast::FieldList::RecordFieldList(fields)) = item.field_list() else { return None };
    let marked = fields.fields().find(|field| {
        field.attrs().any(|it| it.path().is_some_and(|path| path.syntax().text() == MARK))
    })?;
    // alloc: the field's name, once per finding
    Some(marked.name()?.text().to_string())
}

fn report<'t>(cx: &mut Cx<'_, 't>, at: ast::Node<'t>, message: String, field: &str) {
    let Some(range) = cx.range(at) else { return };
    let span = cx.span(range);
    cx.blaze(LINT, at, span, message)
        .note(format!(
            "`{field}` is marked `#[blaze::policy]`: a cost or a guarantee the maker of the value decides, so every \
             way to make one takes it"
        ))
        .help(LINT.help())
        .help(format!("for further information visit {}", LINT.docs_url()));
}

/// `#[derive(Default)]` on a type with a marked field.
pub(super) fn check_adt<'t>(cx: &mut Cx<'_, 't>, adt: ast::Adt<'t>) {
    if !cx.on_blaze(LINT)
        || cx.expansion().is_some()
        || !cx.is_written(adt.syntax())
        || cx.is_in_test(adt.syntax())
    {
        return;
    }
    let Some(field) = policy_field(adt) else { return };
    let ast::Adt::Struct(item) = adt else { return };
    let Some(name) = item.name() else { return };
    // alloc: the type's name, once per finding
    let name = name.text().to_string();
    let derives: Vec<ast::Attr<'t>> = item
        .attrs()
        .filter(|attr| {
            attr.simple_name() == Some("derive")
                && check::derive_paths(*attr)
                    .iter()
                    .any(|(_, path)| path.rsplit("::").next() == Some("Default"))
        })
        .collect();
    for derive in derives {
        let message = format!("`{name}` derives `Default`, which picks its `{field}` for whoever makes one");
        report(cx, derive.syntax(), message, &field);
    }
}

/// `impl Default for T`, and an inherent `fn new()` with no parameter, of a type with a
/// marked field.
pub(super) fn check_impl<'t>(cx: &mut Cx<'_, 't>, item: ast::Impl<'t>) {
    if !cx.on_blaze(LINT)
        || cx.expansion().is_some()
        || !cx.is_written(item.syntax())
        || cx.is_in_test(item.syntax())
    {
        return;
    }
    let Some(imp) = cx.impl_def(item) else { return };
    let db = cx.db();
    let Some(adt) = imp.self_ty(db).as_adt() else { return };
    let Some(source) = cx.adt_source(adt) else { return };
    let Some(field) = policy_field(source) else { return };
    // alloc: the type's name, once per finding
    let name = adt.name(db).as_str().to_owned();
    match imp.trait_(db) {
        Some(found) => {
            if Some(found) == cx.famous().core_default_Default() {
                let message =
                    format!("`impl Default for {name}` picks its `{field}` for whoever makes one");
                report(cx, item.syntax(), message, &field);
            }
        }
        None => {
            let Some(list) = item.assoc_item_list() else { return };
            let news: Vec<ast::Fn<'t>> = list
                .assoc_items()
                .filter_map(|it| match it {
                    ast::AssocItem::Fn(function) => Some(function),
                    _ => None,
                })
                .filter(|function| {
                    function.name().is_some_and(|it| it.text() == "new")
                        && function.param_list().is_some_and(|params| {
                            params.self_param().is_none() && params.params().next().is_none()
                        })
                })
                .collect();
            for function in news {
                let message =
                    format!("`{name}::new()` takes nothing, so it picks `{field}` for its caller");
                report(cx, function.syntax(), message, &field);
            }
        }
    }
}
