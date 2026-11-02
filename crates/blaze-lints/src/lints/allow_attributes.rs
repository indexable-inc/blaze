//! `allow_attributes`: an outer `#[allow(..)]`, which stays silent after the warning it was for
//! is gone; `#[expect(..)]` says so. The fix rewrites `allow` to `expect`; the reason it needs
//! ([`super::allow_attributes_without_reason`]) is written by hand.
//!
//! Follows clippy's `clippy_lints/src/attrs/allow_attributes.rs` (MIT or Apache-2.0), read from
//! indexable-inc's fork at c2308e38 (nightly-2026-05-27), the nearest checkout to the pinned
//! nightly's (rust-lang/rust 12433c30). As in clippy: outer attributes only, those a
//! `cfg_attr` the crate's `cfg` keeps included, from the lint-reasons MSRV on. Unlike clippy's,
//! the message names what the attribute is on and the lints it allows.

use syntax::view::{self as ast, AstNode as _, AttrKind, HasName as _};

use super::Implemented;
use crate::{check::Cx, diag::Applicability, msrvs};

const LINT: Implemented = Implemented::AllowAttributes;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, attr: ast::Attr<'t>) {
    if !cx.on(LINT) || attr.kind() != AttrKind::Outer || !cx.conf().meets(msrvs::LINT_REASONS_STABILIZATION) {
        return;
    }
    for meta in level_metas(cx, attr, &["allow"]) {
        let Some(path) = meta.path() else { continue };
        let Some(range) = cx.range(path.syntax()) else { continue };
        let span = cx.span(range);
        let message = format!(
            "`#[allow({})]` on {}: an `allow` stays after its warning is gone, an `expect` says when it is no longer needed",
            lints_of(meta),
            owner_of(attr),
        );
        cx.lint(LINT, attr.syntax(), span.clone(), message)
            .suggestion(span, "replace it with", "expect", Applicability::MachineApplicable)
            .help("then give it `reason = \"..\"` saying why the lint is wrong here");
    }
}

/// The lint-level metas of `attr` named one of `levels`, those of the `cfg_attr`s the crate's
/// `cfg` keeps included, as rustc reads them after expanding `cfg_attr`.
pub(super) fn level_metas<'q, 't>(
    cx: &Cx<'q, 't>,
    attr: ast::Attr<'t>,
    levels: &'static [&'static str],
) -> impl Iterator<Item = ast::TokenTreeMeta<'t>> + use<'q, 't> {
    let db = cx.db();
    hir_expand::attrs::cfg_attrs_view(std::iter::once(attr), cx.krate().cfg(db)).filter_map(move |(meta, _)| {
        let ast::Meta::TokenTreeMeta(meta) = meta else { return None };
        let name = meta.path()?.as_single_name_ref()?;
        levels.contains(&name.text()).then_some(meta)
    })
}

/// The lints a level meta names, as written between its parentheses, its `reason` left out.
pub(super) fn lints_of(meta: ast::TokenTreeMeta<'_>) -> String {
    let Some(tt) = meta.token_tree() else { return String::new() };
    let text = tt.syntax().text();
    let inner = text.strip_prefix('(').and_then(|it| it.strip_suffix(')')).unwrap_or(text);
    // alloc: the lint's message, once per finding
    let names: Vec<&str> =
        inner.split(',').map(str::trim).filter(|it| !it.is_empty() && !it.starts_with("reason")).collect();
    names.join(", ")
}

/// What `attr` is on, as a message names it: the item's kind and name, or the kind of node.
pub(super) fn owner_of(attr: ast::Attr<'_>) -> String {
    use syntax::SyntaxKind as K;
    let Some(owner) = attr.syntax().parent() else { return "this item".to_owned() };
    let kind = match owner.kind() {
        K::FN => "fn",
        K::STRUCT => "struct",
        K::ENUM => "enum",
        K::UNION => "union",
        K::TRAIT => "trait",
        K::IMPL => return "this impl".to_owned(),
        K::MODULE => "mod",
        K::CONST => "const",
        K::STATIC => "static",
        K::TYPE_ALIAS => "type",
        K::MACRO_RULES => "macro",
        K::USE => return "this `use`".to_owned(),
        K::RECORD_FIELD | K::TUPLE_FIELD => "field",
        K::VARIANT => "variant",
        K::SOURCE_FILE => return "the crate".to_owned(),
        K::LET_STMT => return "this `let`".to_owned(),
        _ => return "this expression".to_owned(),
    };
    match ast::AnyHasName::cast(owner).and_then(|it| it.name()) {
        Some(name) => format!("{kind} `{}`", name.text()),
        None => format!("this {kind}"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::more_rules_tests::{fixed, lint_only};

    #[test]
    fn an_outer_allow_is_found_and_named_and_an_expect_or_inner_allow_is_not() {
        let found = lint_only(
            "allow_attributes",
            r#"
#![allow(unused)]
#[allow(dead_code)]
fn f() {}
#[expect(dead_code, reason = "kept for the fixture")]
fn g() {}
#[cfg_attr(all(), allow(non_camel_case_types))]
struct s;
"#,
        );
        assert_eq!(found.matches("an `allow` stays after its warning is gone").count(), 2, "{found}");
        assert!(found.contains("`#[allow(dead_code)]` on fn `f`"), "{found}");
        assert!(found.contains("`#[allow(non_camel_case_types)]` on struct `s`"), "{found}");
        assert!(!found.contains("`#[allow(unused)]`"), "an inner attribute is not found:\n{found}");
    }

    #[test]
    fn an_allow_a_macro_writes_from_its_own_tokens_is_not_found_but_one_it_is_handed_is() {
        let found = lint_only(
            "allow_attributes",
            r#"
macro_rules! quiet { () => { #[allow(dead_code)] fn made() {} }; }
macro_rules! keep { ($i:item) => { $i }; }
quiet!();
keep! { #[allow(dead_code)] fn handed() {} }
"#,
        );
        assert_eq!(found.matches("an `allow` stays after its warning is gone").count(), 1, "{found}");
        assert!(found.contains("on fn `handed`"), "{found}");
    }

    #[test]
    fn the_fix_writes_expect_and_leaves_nothing_to_fix() {
        let after = fixed(
            "allow_attributes",
            r#"
#[allow(dead_code, reason = "kept for the fixture")]
fn f() {}
"#,
        );
        assert_eq!(after, "#[expect(dead_code, reason = \"kept for the fixture\")]\nfn f() {}\n");
    }
}
