//! `unwired`: `#[allow(dead_code)]`, `#[expect(dead_code)]` or the same of `unused`, on an
//! item, a field or a variant outside test code: a design written and never wired in, kept
//! quiet instead of used or deleted (whole designs sat so: `mod found`, `job/certificate.rs`).
//!
//! A finding is an attribute `#[allow(..)]` or `#[expect(..)]`, outer or inner (`#![..]` of a
//! module or a file), written plainly or in a `cfg_attr` the build keeps, whose list names
//! `dead_code` or `unused` (exactly those: `unused_imports`, `unused_variables` and the rest
//! are other lints), on an item, an associated item, a field or a variant. Not found on a
//! `use` or `extern crate` item, a statement, an expression, a parameter or a local, nor in
//! test code ([`test_code::is_test_code`]).
//!
//! There is no fix: wire the item where it was meant to be used, delete it, give an item used
//! only on some targets the `cfg` of those targets, or keep it with `#[expect(blaze::unwired,
//! reason = "<what wires it, and when>")]` beside the attribute.

use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _},
};

use super::{Lint, test_code};
use crate::check::Cx;

const LINT: Lint = Lint::Unwired;

/// The lints whose silence the finding is about.
const DEAD: &[&str] = &["dead_code", "unused"];

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if node.kind() != SyntaxKind::ATTR || !cx.on_blaze(LINT) {
        return;
    }
    if cx.expansion().is_some() || !cx.is_written(node) {
        return;
    }
    let Some(attr) = ast::Attr::cast(node) else { return };
    let Some(owner) = node.parent() else { return };
    if !owns(owner) {
        return;
    }
    let db = cx.db();
    let mut said = None;
    for (meta, _) in hir_expand::attrs::cfg_attrs_view(std::iter::once(attr), cx.krate().cfg(db)) {
        let ast::Meta::TokenTreeMeta(meta) = meta else { continue };
        let Some(level) = meta.path().and_then(|it| it.as_single_name_ref()) else { continue };
        if !matches!(level.text(), "allow" | "expect") {
            continue;
        }
        let Some(tt) = meta.token_tree() else { continue };
        let text = tt.syntax().text();
        let inner = text.strip_prefix('(').and_then(|it| it.strip_suffix(')')).unwrap_or(text);
        let named = inner
            .split(',')
            .map(str::trim)
            .find(|it| DEAD.contains(it));
        if let Some(name) = named {
            said = Some((level.text(), name));
            break;
        }
    }
    let Some((level, name)) = said else { return };
    if test_code::is_test_code(cx, node) {
        return;
    }
    let Some(range) = cx.range(node) else { return };
    let message = format!(
        "`#[{level}({name})]` keeps {} quiet that nothing outside tests uses",
        what(owner)
    );
    let span = cx.span(range);
    cx.blaze(LINT, node, span, message)
        .note(
            "code that is written and never wired in is a design left half done: the attribute hides that \
             nothing calls it, so it rots unseen",
        )
        .help(LINT.help())
        .help(format!("for further information visit {}", LINT.docs_url()));
}

/// Whether an attribute on `owner` is on an item, a field or a variant: the outer attribute's
/// parent is the item; an inner one's is the file or a module's item list.
fn owns(owner: ast::Node<'_>) -> bool {
    use SyntaxKind as K;
    match owner.kind() {
        K::FN
        | K::STRUCT
        | K::ENUM
        | K::UNION
        | K::CONST
        | K::STATIC
        | K::TRAIT
        | K::IMPL
        | K::TYPE_ALIAS
        | K::MODULE
        | K::MACRO_RULES
        | K::MACRO_DEF
        | K::RECORD_FIELD
        | K::TUPLE_FIELD
        | K::VARIANT
        | K::SOURCE_FILE => true,
        K::ITEM_LIST => owner.parent().is_some_and(|it| it.kind() == K::MODULE),
        _ => false,
    }
}

/// What the attribute is on, as the message names it.
fn what(owner: ast::Node<'_>) -> &'static str {
    use SyntaxKind as K;
    match owner.kind() {
        K::RECORD_FIELD | K::TUPLE_FIELD => "a field",
        K::VARIANT => "a variant",
        K::MODULE | K::ITEM_LIST | K::SOURCE_FILE => "a module",
        K::IMPL => "an impl",
        _ => "an item",
    }
}

#[cfg(test)]
mod tests {
    use crate::tests::lint;

    const FOUND: &str = "quiet that nothing outside tests uses";

    fn found(main: &str) -> String {
        let fixture = format!("//- /src/lib.rs crate:main\n{}", main.trim_start_matches('\n'));
        lint(&fixture, "-A clippy::all --warn=blaze::unwired")
    }

    /// The bug in miniature: a design written and kept quiet, on a module, an item, a field
    /// and a variant, plainly and in a `cfg_attr` the build keeps.
    #[test]
    fn a_silenced_unused_design_is_found() {
        let out = found(
            r#"
#[allow(dead_code)]
mod found {
    pub fn certificate() {}
}
#[expect(unused, reason = "wired in later")]
fn later() {}
pub struct Job {
    #[allow(dead_code)]
    pub witness: u32,
}
pub enum State {
    Done,
    #[cfg_attr(all(), allow(dead_code))]
    Certified,
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 4, "{out}");
        assert!(out.contains("`#[allow(dead_code)]` keeps a module quiet"), "{out}");
        assert!(out.contains("keeps a field quiet"), "{out}");
    }

    /// Not a finding: another lint of the `unused` group, an attribute on a `use`, a local or
    /// a statement, a `cfg_attr` the build drops, and test code.
    #[test]
    fn other_lints_other_places_and_tests_are_left_alone() {
        let out = found(
            r#"
#[allow(unused_imports)]
use core::marker::PhantomData;
#[allow(unused_variables, clippy::all)]
pub fn f() {
    #[allow(unused)]
    let x = 1;
}
#[cfg_attr(any(), allow(dead_code))]
pub fn g() {}
#[cfg(test)]
mod tests {
    #[allow(dead_code)]
    fn helper() {}
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
    }

    /// The one way to keep a silenced item: an expectation of `unwired` beside it, which the
    /// finding fulfils.
    #[test]
    fn an_expectation_beside_it_is_fulfilled() {
        let out = found(
            r#"
#[expect(blaze::unwired, reason = "the certificate is wired into the job when the giver lands")]
#[allow(dead_code)]
fn certificate() {}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
        assert!(!out.contains("unfulfilled"), "{out}");
    }
}
