//! `upper_case_acronyms`: a type, trait or variant named all in capitals, as `JSON`.
//!
//! Follows clippy's `clippy_lints/src/upper_case_acronyms.rs` at rust-lang/rust
//! 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0 (MIT or Apache-2.0). An item is taken to be
//! exported, as `avoid-breaking-exported-api` leaves alone, when it is `pub`: rustc's
//! effective visibilities, which also leave out `pub` items no path outside the crate reaches,
//! are not computed.

use syntax::view::{self as ast, AstNode as _, HasName as _, HasVisibility as _};

use crate::{check::Cx, diag::Applicability};

const LINT: super::Implemented = super::Implemented::UpperCaseAcronyms;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, item: ast::Item<'t>) {
    if !cx.on(LINT)
        || !matches!(
            item,
            ast::Item::Struct(_) | ast::Item::Trait(_) | ast::Item::TypeAlias(_) | ast::Item::Enum(_)
        )
    {
        return;
    }
    let aggressive = cx.conf().bool("upper-case-acronyms-aggressive", false);
    let avoid_exported = cx.conf().bool("avoid-breaking-exported-api", true);
    if avoid_exported && exported(item) {
        return;
    }
    match item {
        ast::Item::Struct(it) => check_name(cx, it.syntax(), it.name(), aggressive),
        ast::Item::Trait(it) => check_name(cx, it.syntax(), it.name(), aggressive),
        ast::Item::TypeAlias(it) => check_name(cx, it.syntax(), it.name(), aggressive),
        ast::Item::Enum(it) => {
            check_name(cx, it.syntax(), it.name(), aggressive);
            for variant in it.variant_list().into_iter().flat_map(|it| it.variants()) {
                check_name(cx, variant.syntax(), variant.name(), aggressive);
            }
        }
        _ => {}
    }
}

fn exported(item: ast::Item<'_>) -> bool {
    let visibility = match item {
        ast::Item::Struct(it) => it.visibility(),
        ast::Item::Trait(it) => it.visibility(),
        ast::Item::TypeAlias(it) => it.visibility(),
        ast::Item::Enum(it) => it.visibility(),
        _ => None,
    };
    visibility.is_some_and(|it| it.syntax().text() == "pub")
}

fn check_name<'t>(
    cx: &mut Cx<'_, 't>,
    at: ast::Node<'t>,
    name: Option<ast::Name<'t>>,
    aggressive: bool,
) {
    let Some(name) = name else { return };
    let ident = name.text();
    // Every name it lints has three capitals in a row, or ends in two.
    if ident.bytes().filter(u8::is_ascii_uppercase).nth(1).is_none() {
        return;
    }
    let Some(range) = cx.range(name.syntax()) else { return };
    let Some(replacement) = replacement(ident, aggressive) else { return };
    let span = cx.span(range);
    cx.lint(LINT, at, span.clone(), format!("name `{ident}` contains a capitalized acronym"))
        .suggestion(
            span,
            "consider making the acronym lowercase, except the initial letter",
            replacement,
            Applicability::MaybeIncorrect,
        );
}

fn replacement(s: &str, aggressive: bool) -> Option<String> {
    if s.len() > 2 && s.bytes().all(|c| c.is_ascii_uppercase()) {
        let mut chars = s.chars();
        let mut r = String::with_capacity(s.len());
        r.extend(chars.next());
        r.extend(chars.map(|c| c.to_ascii_lowercase()));
        return Some(r);
    }
    let unprefixed = s.trim_start_matches('_');
    if !aggressive
        || !unprefixed.starts_with(|c: char| c.is_ascii_uppercase())
        || !contains_acronym(unprefixed)
    {
        return None;
    }
    let mut r = String::with_capacity(s.len());
    let mut chars = s.chars();
    let mut prev_upper = false;
    while let Some(c) = chars.next() {
        let was_upper = std::mem::replace(&mut prev_upper, c.is_ascii_uppercase());
        r.push(if was_upper && chars.clone().next().is_none_or(|c| c.is_ascii_uppercase()) {
            c.to_ascii_lowercase()
        } else {
            c
        });
    }
    Some(r)
}

fn contains_acronym(s: &str) -> bool {
    let mut count = 0;
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            count += 1;
            if count == 3 {
                return true;
            }
        } else {
            count = 0;
        }
    }
    count == 2
}
