//! rustc's table of language features, and what it says of a feature `#![feature]` names: a
//! language one or a library one, incomplete, internal, its tracking issue. One table for the
//! feature lints and the errors of `#![feature]` (E0554, E0557, E0635).
//!
//! Follows rustc's `compiler/rustc_feature/src/{lib,unstable,accepted,removed}.rs`
//! (`Features::incomplete`, `Features::internal`, `find_lang_feature_issue`) and
//! `compiler/rustc_expand/src/config.rs` (`features`: which features are language ones) (MIT or
//! Apache-2.0).

mod lang;

/// What rustc's feature lists say of a language feature: the list it is in, and for an
/// unstable one its status there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Kind {
    /// `(unstable, ..)` in `unstable.rs`.
    Unstable,
    /// `(incomplete, ..)` in `unstable.rs`.
    Incomplete,
    /// `(internal, ..)` in `unstable.rs`.
    Internal,
    /// In `accepted.rs`: stable.
    Accepted,
    /// In `removed.rs`.
    Removed,
}

impl Kind {
    /// Whether it is one of `unstable.rs`.
    pub(crate) fn is_unstable(self) -> bool {
        matches!(self, Kind::Unstable | Kind::Incomplete | Kind::Internal)
    }
}

/// A language feature of rustc's lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Lang {
    pub(crate) name: &'static str,
    pub(crate) kind: Kind,
    /// The version it got its status in, as the list writes it (`CURRENT_RUSTC_VERSION` is
    /// the toolchain's own).
    pub(crate) since: &'static str,
    /// Its tracking issue.
    pub(crate) issue: Option<u32>,
}

/// The language feature `name`, if rustc has one.
pub(crate) fn lang(name: &str) -> Option<Lang> {
    let at = lang::LANG.binary_search_by(|it| it.0.cmp(name)).ok()?;
    let (name, kind, since, issue) = lang::LANG[at];
    Some(Lang { name, kind, since, issue })
}

/// rustc's `UNSTABLE_LANG_FEATURES`: the unstable language features in the order `unstable.rs`
/// declares them, the order `find_best_match_for_name` breaks ties in.
pub(crate) fn unstable_declared() -> impl ExactSizeIterator<Item = &'static str> {
    lang::DECLARED.iter().map(|&at| lang::LANG[usize::from(at)].0)
}

/// Why the removed feature `name` was removed and the pull request that removed it, if it is
/// one.
pub(crate) fn removed(name: &str) -> Option<(Option<&'static str>, Option<u32>)> {
    let at = lang::REMOVED.binary_search_by(|it| it.0.cmp(name)).ok()?;
    let (_, reason, pull) = lang::REMOVED[at];
    Some((reason, pull))
}

/// Whether `#![feature(name)]` enables a language feature (`enabled_lang_features`): one rustc
/// accepted or has unstable; any other it enables as a library feature. A removed one it does
/// not enable at all.
pub(crate) fn is_lang(name: &str) -> bool {
    lang(name).is_some_and(|it| it.kind != Kind::Removed)
}

/// rustc's `Features::incomplete`: an unstable language feature marked incomplete.
pub(crate) fn incomplete(name: &str) -> bool {
    lang(name).is_some_and(|it| it.kind == Kind::Incomplete)
}

/// rustc's `Features::internal`: an unstable language feature marked internal, or any other
/// feature whose name ends in `_internal` or `_internals`, or `core_intrinsics`.
pub(crate) fn internal(name: &str) -> bool {
    match lang(name) {
        Some(it) if it.kind.is_unstable() => it.kind == Kind::Internal,
        _ => name == "core_intrinsics" || name.ends_with("_internal") || name.ends_with("_internals"),
    }
}

/// rustc's `find_lang_feature_issue`: the tracking issue of the language feature `name`.
pub(crate) fn issue(name: &str) -> Option<u32> {
    lang(name).and_then(|it| it.issue)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lists_are_sorted_and_each_name_is_in_one() {
        assert!(lang::LANG.windows(2).all(|it| it[0].0 < it[1].0));
        assert!(lang::REMOVED.windows(2).all(|it| it[0].0 < it[1].0));
        let removed = lang::LANG.iter().filter(|it| it.1 == Kind::Removed).count();
        assert_eq!(removed, lang::REMOVED.len());
        assert!(lang::REMOVED.iter().all(|it| lang(it.0).is_some_and(|it| it.kind == Kind::Removed)));
    }

    #[test]
    fn the_declared_order_has_each_unstable_feature_once() {
        let unstable = lang::LANG.iter().filter(|it| it.1.is_unstable()).count();
        let mut seen = vec![false; lang::LANG.len()];
        for &at in lang::DECLARED {
            assert!(lang::LANG[usize::from(at)].1.is_unstable() && !seen[usize::from(at)]);
            seen[usize::from(at)] = true;
        }
        assert_eq!(lang::DECLARED.len(), unstable);
        let declared: Vec<_> = unstable_declared().take(3).collect();
        assert_eq!(declared, ["allocator_internals", "allow_internal_unsafe", "allow_internal_unstable"]);
    }

    #[test]
    fn a_language_feature_is_found_with_its_status_and_issue() {
        assert_eq!(
            lang("specialization"),
            Some(Lang { name: "specialization", kind: Kind::Incomplete, since: "1.7.0", issue: Some(31844) })
        );
        assert_eq!(lang("intrinsics").map(|it| it.kind), Some(Kind::Internal));
        assert_eq!(lang("rustc_attrs").map(|it| it.kind), Some(Kind::Internal));
        assert_eq!(lang("min_specialization").map(|it| it.kind), Some(Kind::Unstable));
        assert_eq!(lang("aarch64_target_feature").map(|it| it.kind), Some(Kind::Accepted));
        assert_eq!(lang("unsized_locals").map(|it| it.kind), Some(Kind::Removed));
        assert_eq!(lang("panic_internals"), None);
        assert_eq!(issue("contracts"), Some(128044));
        assert_eq!(issue("test_incomplete_feature"), None);
    }

    #[test]
    fn incomplete_and_internal_are_as_rustc_decides() {
        assert!(incomplete("generic_const_exprs") && !internal("generic_const_exprs"));
        assert!(!incomplete("min_specialization") && !internal("min_specialization"));
        assert!(internal("intrinsics") && internal("lang_items") && internal("staged_api"));
        assert!(internal("panic_internals") && internal("fmt_internals") && internal("core_intrinsics"));
        assert!(internal("eii_internals") && !incomplete("eii_internals"));
        assert!(!internal("unstable_test_feature") && !incomplete("unstable_test_feature"));
        assert!(!incomplete("aarch64_target_feature") && !internal("aarch64_target_feature"));
    }

    #[test]
    fn a_removed_feature_is_not_enabled_and_has_its_reason() {
        assert!(!is_lang("unsized_locals"));
        assert!(is_lang("aarch64_target_feature") && is_lang("specialization"));
        assert!(!is_lang("panic_internals"));
        assert_eq!(removed("start"), Some((Some("not portable enough and never RFC'd"), Some(134299))));
        assert_eq!(removed("quote"), Some((None, None)));
        assert_eq!(removed("specialization"), None);
    }
}
