//! `iter_over_hash_type`: a `for` loop over a hash map or set, or one of std's iterators of
//! one: it runs in the hasher's and the table's order, so whatever it writes out (an object, a
//! diagnostic's order, a store key) changes with them. AGENTS.md rule 20: nothing iterated into
//! output depends on hash-map order.
//!
//! Follows clippy's `clippy_lints/src/iter_over_hash_type.rs` (MIT or Apache-2.0), read from
//! indexable-inc's fork at c2308e38 (nightly-2026-05-27), the nearest checkout to the pinned
//! nightly's (rust-lang/rust 12433c30). As in clippy: a `for` written in the file whose
//! iterated expression's type, references peeled, is std's `HashMap` or `HashSet` or one of
//! their iterators clippy names by diagnostic item (`Iter`, `IterMut`, `Keys`, `Values`,
//! `ValuesMut` and `Drain` of a map, `Iter` and `Drain` of a set). Unlike clippy, the hash
//! maps and sets of `hashbrown`, `papaya`, `dashmap` and `blaze_hash` count too
//! ([`facts::hash_collection`]), the message names the type, and the span is the loop's head.
//! [`hash_iterated`] is what `blaze::hash_order_output` builds on.

use hir::HirDisplay as _;
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _},
};

use super::Implemented;
use crate::{blaze::facts, check::Cx};

const LINT: Implemented = Implemented::IterOverHashType;

/// std's iterators of a hash map or set, by diagnostic item.
const STD_ITERATORS: &[&str] = &[
    "hashmap_keys_ty",
    "hashmap_values_ty",
    "hashmap_values_mut_ty",
    "hashmap_iter_ty",
    "hashmap_iter_mut_ty",
    "hashmap_drain_ty",
    "hashset_iter_ty",
    "hashset_drain_ty",
];

/// Whether iterating `ty`, its references peeled, goes in hash order: a hash map or set
/// ([`facts::hash_collection`]), or one of std's iterators of one.
pub(crate) fn hash_iterated(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> bool {
    if facts::hash_collection(cx, ty) {
        return true;
    }
    let Some(adt) = ty.strip_references().as_adt() else { return false };
    let db = cx.db();
    let krate = cx.krate();
    STD_ITERATORS
        .iter()
        .any(|it| krate.diagnostic_item_adt(db, &intern::Symbol::intern(it)) == Some(adt))
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, for_expr: ast::ForExpr<'t>) {
    if !cx.on(LINT) || !cx.is_written(for_expr.syntax()) {
        return;
    }
    let Some(iterable) = for_expr.iterable() else { return };
    let Some(ty) = cx.ty(iterable) else { return };
    if !hash_iterated(cx, &ty) {
        return;
    }
    let (Some(start), Some(end)) = (cx.range(for_expr.syntax()), cx.range(iterable.syntax())) else { return };
    let db = cx.db();
    let shown = ty.strip_references().display(db, cx.krate().to_display_target(db)).to_string();
    let span = cx.span(TextRange::new(start.start(), end.end().max(start.start())));
    let message = format!(
        "this `for` goes through a `{shown}` in hash order: what it writes out changes with the hasher and the table"
    );
    cx.lint(LINT, for_expr.syntax(), span, message).help(
        "iterate a `BTreeMap` or `BTreeSet`, or collect and sort first; where the order reaches no output, say why: `#[expect(clippy::iter_over_hash_type, reason = \"..\")]`",
    );
}

#[cfg(test)]
mod tests {
    use super::super::more_rules_tests::lint_only;

    const LINT: &str = "iter_over_hash_type";
    const FOUND: &str = "in hash order";

    #[test]
    fn a_for_over_a_hash_map_set_or_their_iterators_is_found_and_over_a_btree_map_not() {
        let found = lint_only(
            LINT,
            r#"
use std::collections::{BTreeMap, HashMap, HashSet};
pub fn f(m: &HashMap<u8, u8>, s: &HashSet<u8>, b: &BTreeMap<u8, u8>) {
    for _ in m {}
    for _ in m.keys() {}
    for _ in s.iter() {}
    for _ in b.iter() {}
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 3, "{found}");
        assert!(found.contains("goes through a `HashMap<u8, u8"), "{found}");
    }

    #[test]
    fn a_hash_map_through_a_renamed_import_and_a_for_a_macro_is_handed_are_found() {
        let found = lint_only(
            LINT,
            r#"
use std::collections::hash_map::HashMap as Table;
macro_rules! each { ($($t:tt)*) => { $($t)* }; }
pub fn f(m: &Table<u8, u8>) {
    for _ in m {}
    each! { for _ in m {} }
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 2, "{found}");
    }
}
