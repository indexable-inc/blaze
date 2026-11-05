//! `struct_update`: a struct literal that takes the fields it does not write from a base,
//! `..base`, `..Default::default()`, `..Self::default()` (or leaves them to their default
//! values, `..` alone), for a struct of the workspace. A field added to the struct then takes
//! its value from the base at every such literal, with no one deciding it there; written out,
//! each literal is a compile error until its author does.
//!
//! Only a struct of a workspace member is checked: another crate's may be `#[non_exhaustive]`
//! or change its fields under a version bump, and its literal is not the workspace's to spell
//! out. A literal a macro wrote (a derive, a `macro_rules!` adding `..Default::default()`) is
//! not checked, nor one with every field written (clippy's `needless_update`).
//!
//! The fix writes each field taken as `field: base.field`, machine-applicable when the base is
//! a place: a local (`self` included), or a field or parenthesis of one. `..base` moves or
//! copies each field it takes out of the place, one at a time, which is what `field:
//! base.field` does, so the rewrite is sound whether or not `base` is used after and whether
//! or not the fields are `Copy`; a place has no side effect to run once instead of once per
//! field. Any other base (a call, `Default::default()`, a `const`, an index) is evaluated once
//! and is left to the author, with the fields to write named.

use syntax::{TextRange, view as ast};

use ast::AstNode as _;

use super::Lint;
use crate::{check::Cx, diag::Applicability};

const LINT: Lint = Lint::StructUpdate;

/// How many fields and parentheses deep a base is followed to its local.
const PLACE_DEPTH: u32 = 16;
/// How many of the fields taken the message names.
const NAMED: usize = 8;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, record: ast::RecordExpr<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let node = record.syntax();
    // What a derive or a macro wrote is not the author's to spell out.
    if cx.expansion().is_some() || !cx.is_written(node) {
        return;
    }
    let Some(list) = record.record_expr_field_list() else { return };
    let Some(dotdot) = list.dotdot_token() else { return };
    let Some(ty) = cx.ty(ast::Expr::RecordExpr(record)) else { return };
    let Some(hir::Adt::Struct(strukt)) = ty.as_adt() else { return };
    let db = cx.db();
    if !strukt.module(db).krate(db).base().data(db).origin.is_local() {
        return;
    }
    let Some(range) = cx.range(node) else { return };
    let edition = cx.edition();
    // alloc: the names the literal writes, once per finding's literal
    let written: Vec<String> = list
        .fields()
        .filter_map(|field| match field.name_ref() {
            Some(name) => Some(name.text().trim_start_matches("r#").to_owned()),
            // Shorthand, `Config { a, ..base }`: the field is the path's name.
            None => {
                field.expr().map(|it| cx.source_of(it.syntax()).trim_start_matches("r#").to_owned())
            }
        })
        .collect();
    // alloc: the names of the fields taken, for the message and the fix
    let taken: Vec<String> = strukt
        .fields_iter(db)
        .map(|it| it.name(db))
        .filter(|name| !written.iter().any(|it| it == name.as_str()))
        .map(|name| name.display(db, edition).to_string())
        .collect();
    if taken.is_empty() {
        return;
    }
    let name = strukt.name(db).display(db, edition).to_string();
    let base = list.spread();
    let base_text = base.map(|it| cx.source_of(it.syntax()).into_owned());
    let mut shown =
        taken.iter().take(NAMED).map(|it| format!("`{it}`")).collect::<Vec<_>>().join(", ");
    if taken.len() > NAMED {
        shown.push_str(&format!(" and {} more", taken.len() - NAMED));
    }
    let from = match &base_text {
        Some(base) => format!("from `{base}`"),
        None => "from their default values".to_owned(), // alloc: a message's part
    };
    let message = format!(
        "this `{name}` literal takes {shown} {from}: a field added to `{name}` is filled here without a decision"
    );
    // The fix's range is of the tree walked: only where no macro call maps it.
    let place = base.filter(|&it| cx.in_file() && is_place(cx, it, PLACE_DEPTH));
    let fix = place.zip(base_text.as_deref()).map(|(it, text)| {
        let at = TextRange::new(dotdot.text_range().start(), it.syntax().text_range().end());
        let fields = taken
            .iter()
            .map(|field| format!("{field}: {text}.{field}"))
            .collect::<Vec<_>>()
            .join(", ");
        (cx.span(at), fields)
    });
    let span = match cx.in_file() {
        true => cx.span(TextRange::new(
            dotdot.text_range().start(),
            base.map_or(dotdot.text_range().end(), |it| it.syntax().text_range().end()),
        )),
        false => cx.span(range),
    };
    let diagnostic = cx.blaze(LINT, node, span, message);
    diagnostic.note(format!(
        "every field written out makes a new field of `{name}` a compile error at this literal until its value is chosen"
    ));
    match fix {
        Some(part) => {
            diagnostic.multipart_suggestion(
                format!(
                    "write out the fields taken from `{}`",
                    base_text.as_deref().unwrap_or_default()
                ),
                // alloc: the fix's one part
                vec![part],
                Applicability::MachineApplicable,
            );
        }
        None => {
            diagnostic.help(format!(
                "write {shown} out, each with the value this literal means (`Default::default()` where the default is meant), \
                 or bind the base to a local first and take each field from it"
            ));
        }
    }
    diagnostic.help(LINT.help()).help(format!("for further information visit {}", LINT.docs_url()));
}

/// Whether `expr` is a place with no side effect: a local (`self` included), or a field or a
/// parenthesis of one, at most `depth` deep.
fn is_place<'t>(cx: &Cx<'_, 't>, expr: ast::Expr<'t>, depth: u32) -> bool {
    if depth == 0 {
        return false;
    }
    match expr {
        ast::Expr::PathExpr(it) => it
            .path()
            .and_then(|path| cx.path(path))
            .is_some_and(|it| matches!(it, hir::PathResolution::Local(_))),
        ast::Expr::FieldExpr(it) => it.expr().is_some_and(|inner| is_place(cx, inner, depth - 1)),
        ast::Expr::ParenExpr(it) => it.expr().is_some_and(|inner| is_place(cx, inner, depth - 1)),
        _ => false,
    }
}
