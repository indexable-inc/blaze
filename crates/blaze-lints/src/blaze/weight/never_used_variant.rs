//! `never_constructed` and `never_matched`: a variant production code never makes, or one it
//! makes and never tells apart from the others.
//!
//! - `never_constructed`: a variant of an exported enum no production body of the workspace
//!   constructs (a call of its constructor, a record literal, a unit path), and no serde impl
//!   reads in. An error variant nobody returns is the common case: every `match` on the error
//!   handles a case that cannot happen, and a reader believes it can. A variant only tests make
//!   is said so. It adds to `blaze::dead`, which finds a never-constructed variant of a private
//!   enum within its crate, the exported enums, across the workspace.
//! - `never_matched`: a variant production code constructs, of an enum whose other variants some
//!   pattern names, that no pattern of the workspace names and that is never told apart with
//!   `==` or an `as` cast: every `match` sends it to a `_` arm or a binding, so it means what
//!   another variant means.
//!
//! Read off the use rows of the whole workspace ([`super::rows`]). Not found: a `#[default]`
//! variant (the derive constructs it), test code, an enum in a macro. No fix: delete the
//! variant, construct it where it was meant to be, or handle it by name.

use hir_def::AdtId;
use syntax::view::{self as ast, AstNode as _, HasName as _};

use super::{
    Lint, facts, has_attr,
    rows::{Role, Subject, Way},
    test_code, variants_of,
};
use crate::check::Cx;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, item: ast::Enum<'t>) {
    let constructed_on = cx.on_blaze(Lint::NeverConstructed);
    let matched_on = cx.on_blaze(Lint::NeverMatched);
    if !(constructed_on || matched_on) || test_code::is_test_code(cx, item.syntax()) {
        return;
    }
    let db = cx.db();
    let Some(hir::Adt::Enum(def)) = cx.adt_def(ast::Adt::Enum(item)) else { return };
    let Some(enum_subject) = Subject::of_adt(db, AdtId::EnumId(def.into())) else { return };
    let compared = facts(cx, enum_subject).all(Way::Compare) > 0;
    let exported = cx.is_exported(item.syntax());
    let enum_name = def.name(db).as_str().to_owned();
    let error = enum_name.ends_with("Error");
    // alloc: an enum's variants with their facts, once per enum linted
    let variants: Vec<_> = variants_of(cx, ast::Adt::Enum(item))
        .into_iter()
        .filter_map(|(variant, node, _)| {
            let seen = facts(cx, Subject::of_variant(db, variant)?);
            Some((node, seen))
        })
        .collect();
    let any_matched = variants
        .iter()
        .any(|(_, seen)| seen.get(Role::Plain, Way::Match) + seen.get(Role::Test, Way::Match) > 0);
    for (node, seen) in variants {
        let Some(variant) = ast::Variant::cast(node) else { continue };
        if has_attr(node, "default") {
            continue;
        }
        let Some(name) = variant.name() else { continue };
        let name = name.text();
        let made = seen.get(Role::Plain, Way::Construct) + seen.get(Role::Stored, Way::Construct);
        let Some(range) = cx.range(variant.name().map_or(node, |it| it.syntax())) else { continue };
        if constructed_on && exported && made == 0 {
            let by_tests = seen.get(Role::Test, Way::Construct) > 0;
            let message = match (error, by_tests) {
                (true, false) => format!("error variant `{enum_name}::{name}` is never constructed: nothing returns it"),
                (_, true) => format!("variant `{enum_name}::{name}` is constructed only by tests"),
                (false, false) => format!("variant `{enum_name}::{name}` is never constructed in the workspace"),
            };
            super::report(
                cx,
                Lint::NeverConstructed,
                node,
                range,
                message,
                "every `match` on the enum handles a case production code never makes, and a reader believes it can happen",
                None,
            );
            continue;
        }
        let named = seen.get(Role::Plain, Way::Match) + seen.get(Role::Test, Way::Match);
        if matched_on && any_matched && !compared && made > 0 && named == 0 {
            let message = format!(
                "variant `{enum_name}::{name}` is constructed and never matched by name: every `match` sends it to a `_` arm or a binding"
            );
            super::report(
                cx,
                Lint::NeverMatched,
                node,
                range,
                message,
                "a variant no code tells apart means what the arm it falls into means: merge it into that variant, or handle it by name",
                None,
            );
        }
    }
}
