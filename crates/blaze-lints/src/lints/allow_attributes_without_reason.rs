//! `allow_attributes_without_reason`: an `allow` or `expect` with no `reason = ".."` last in its
//! list, which leaves the next reader to guess why the lint was wrong there.
//!
//! Follows clippy's `clippy_lints/src/attrs/allow_attributes_without_reason.rs` (MIT or
//! Apache-2.0), read from indexable-inc's fork at c2308e38 (nightly-2026-05-27), the nearest
//! checkout to the pinned nightly's (rust-lang/rust 12433c30). As in clippy: inner and outer
//! attributes, those a `cfg_attr` the crate's `cfg` keeps included, the reason counted only when
//! it is the list's last element, from the lint-reasons MSRV on. Unlike clippy's, the message
//! names what the attribute is on and the lints it sets. No fix: only a person knows the reason.

use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _},
};

use super::{
    Implemented,
    allow_attributes::{level_metas, lints_of, owner_of},
};
use crate::{check::Cx, msrvs};

const LINT: Implemented = Implemented::AllowAttributesWithoutReason;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, attr: ast::Attr<'t>) {
    if !cx.on(LINT) || !cx.conf().meets(msrvs::LINT_REASONS_STABILIZATION) {
        return;
    }
    for meta in level_metas(cx, attr, &["allow", "expect"]) {
        if has_reason(meta) {
            continue;
        }
        let Some(level) = meta.path().and_then(|it| it.as_single_name_ref()) else { continue };
        let Some(range) = cx.range(attr.syntax()) else { continue };
        let span = cx.span(range);
        let message = format!(
            "`{}({})` on {} gives no reason",
            level.text(),
            lints_of(meta),
            owner_of(attr),
        );
        cx.lint(LINT, attr.syntax(), span, message)
            .help("end the list with `reason = \"..\"` saying why the lint is wrong here");
    }
}

/// clippy's check: the list's last element is `reason = <literal>`.
fn has_reason(meta: ast::TokenTreeMeta<'_>) -> bool {
    let Some(tt) = meta.token_tree() else { return false };
    // The elements, split at the list's own commas, each kept as its first three tokens and
    // whether it has more (or a node); a trailing comma ends no element.
    let mut current: ([Option<ast::Token<'_>>; 3], usize) = ([None; 3], 0);
    let mut previous = current;
    for child in tt.syntax().children_with_tokens() {
        let Some(token) = child.into_token() else {
            current.1 = 4;
            continue;
        };
        match token.kind() {
            SyntaxKind::L_PAREN | SyntaxKind::R_PAREN => {}
            SyntaxKind::COMMA => {
                previous = current;
                current = ([None; 3], 0);
            }
            kind if kind.is_trivia() => {}
            _ => {
                if let Some(slot) = current.0.get_mut(current.1) {
                    *slot = Some(token);
                }
                current.1 += 1;
            }
        }
    }
    let (last, len) = if current.1 == 0 { previous } else { current };
    let [Some(name), Some(eq), Some(value)] = last else { return false };
    len == 3 && name.text() == "reason" && eq.kind() == SyntaxKind::EQ && value.kind() == SyntaxKind::STRING
}

#[cfg(test)]
mod tests {
    use super::super::more_rules_tests::lint_only;

    #[test]
    fn an_allow_or_expect_without_a_reason_is_found_and_one_with_is_not() {
        let found = lint_only(
            "allow_attributes_without_reason",
            r#"
#![allow(unused)]
#[allow(dead_code)]
fn f() {}
#[expect(dead_code)]
fn g() {}
#[allow(dead_code, reason = "kept for the fixture",)]
fn h() {}
#[cfg_attr(all(), expect(dead_code))]
fn i() {}
#[warn(dead_code)]
fn j() {}
"#,
        );
        assert_eq!(found.matches("gives no reason").count(), 4, "{found}");
        assert!(found.contains("`allow(unused)` on the crate gives no reason"), "{found}");
        assert!(found.contains("`allow(dead_code)` on fn `f` gives no reason"), "{found}");
        assert!(found.contains("`expect(dead_code)` on fn `g` gives no reason"), "{found}");
        assert!(found.contains("`expect(dead_code)` on fn `i` gives no reason"), "{found}");
        assert!(!found.contains("fn `h`") && !found.contains("fn `j`"), "{found}");
    }

    #[test]
    fn a_reason_that_is_not_last_is_no_reason() {
        let found = lint_only(
            "allow_attributes_without_reason",
            r#"
#[allow(reason = "first", dead_code)]
fn f() {}
"#,
        );
        assert_eq!(found.matches("gives no reason").count(), 1, "{found}");
    }

    #[test]
    fn an_allow_a_macro_is_handed_is_found() {
        let found = lint_only(
            "allow_attributes_without_reason",
            r#"
macro_rules! keep { ($i:item) => { $i }; }
macro_rules! quiet { () => { #[allow(dead_code)] fn made() {} }; }
keep! { #[allow(dead_code)] fn handed() {} }
quiet!();
"#,
        );
        assert_eq!(found.matches("gives no reason").count(), 1, "{found}");
        assert!(found.contains("fn `handed`"), "{found}");
    }
}
