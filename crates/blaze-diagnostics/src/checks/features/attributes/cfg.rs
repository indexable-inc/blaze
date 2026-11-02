//! The gates of a `cfg` predicate: rustc's `GATED_CFGS` (`cfg(overflow_checks)`,
//! `cfg(version(..))`, ..) and the compact `cfg(target(..))`, as `parse_cfg_entry` meets them,
//! in source order: every predicate of an `any` or `all`, the one of a `not`, none in a list
//! rustc rejects.
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/attributes/cfg.rs` (`parse_cfg_entry`,
//! `parse_cfg_entry_version`, `parse_cfg_entry_target`, `parse_name_value`, `try_gate_cfg`,
//! `gate_cfg`) and `compiler/rustc_feature/src/builtin_attrs.rs` (`GATED_CFGS`) (MIT or
//! Apache-2.0).

use hir::{HirFileId, InFile};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _},
};

use super::{Features, allows_unstable, feature_err, features_fact};
use crate::{
    checks::attrs::args::{ArgParser, MetaItemOrLit},
    cx::Cx,
    model::Diag,
};

/// rustc's `GATED_CFGS`: the feature a `cfg` name needs.
fn gated_cfg(name: &str) -> Option<&'static str> {
    Some(match name {
        "overflow_checks" => "cfg_overflow_checks",
        "ub_checks" => "cfg_ub_checks",
        "contract_checks" => "cfg_contract_checks",
        "target_thread_local" => "cfg_target_thread_local",
        "target_has_atomic_load_store" => "cfg_target_has_atomic",
        "sanitize" => "cfg_sanitize",
        "version" => "cfg_version",
        "relocation_model" => "cfg_relocation_model",
        "sanitizer_cfi_generalize_pointers" | "sanitizer_cfi_normalize_integers" => "cfg_sanitizer_cfi",
        "fmt_debug" => "fmt_debug",
        "target_has_reliable_f16"
        | "target_has_reliable_f16_math"
        | "target_has_reliable_f128"
        | "target_has_reliable_f128_math" => "cfg_target_has_reliable_f16_f128",
        "target_has_threads" => "cfg_target_has_threads",
        "target_object_format" => "cfg_target_object_format",
        _ => return None,
    })
}

/// rustc's `try_gate_cfg` of the `cfg` name `name` at `range` of `file`: E0658 when it is gated,
/// its feature is not enabled and the expansion it comes from is not allowed it.
pub(crate) fn try_gate(cx: &Cx<'_, '_>, features: &Features, file: HirFileId, name: &str, range: TextRange, out: &mut Vec<Diag>) {
    let Some(feature) = gated_cfg(name) else { return };
    if features.enabled(feature) || allows_unstable(cx, file, range, feature) {
        return;
    }
    // alloc: the error's message
    let explain = format!("`cfg({name})` is experimental and subject to change");
    let mut diag = feature_err(features, feature, cx.span(InFile::new(file, range)), explain);
    features_fact(cx, &mut diag);
    out.push(diag);
}

/// [`try_gate`] of `target_{key}`, the name a `key = ..` of a compact `target(..)` stands for.
pub(crate) fn try_gate_target(cx: &Cx<'_, '_>, features: &Features, file: HirFileId, key: &str, range: TextRange, out: &mut Vec<Diag>) {
    const PREFIX: &str = "target_";
    // Longer than every gated name, so a name that does not fit is not gated.
    let mut buf = [0u8; 48];
    let Some(name) = buf.get_mut(..PREFIX.len() + key.len()) else { return };
    name[..PREFIX.len()].copy_from_slice(PREFIX.as_bytes());
    name[PREFIX.len()..].copy_from_slice(key.as_bytes());
    let name = std::str::from_utf8(name).expect("two `str`s joined are UTF-8");
    try_gate(cx, features, file, name, range, out);
}

/// `parse_cfg_entry_target`'s gate of `target(..)` at `range`: the feature only, not the
/// expansion.
pub(crate) fn target_compact(cx: &Cx<'_, '_>, features: &Features, file: HirFileId, range: TextRange, out: &mut Vec<Diag>) {
    if features.enabled("cfg_target_compact") {
        return;
    }
    let explain = "compact `cfg(target(..))` is experimental and subject to change";
    let mut diag = feature_err(features, "cfg_target_compact", cx.span(InFile::new(file, range)), explain);
    features_fact(cx, &mut diag);
    out.push(diag);
}

/// The gates of `predicate`, written in `file`, of a `#[cfg]`, a `#[cfg_attr]` or what `cfg!`
/// expands to, in `out`.
pub(super) fn predicate(
    cx: &Cx<'_, '_>,
    features: &Features,
    file: HirFileId,
    predicate: ast::CfgPredicate<'_>,
    out: &mut Vec<Diag>,
) {
    match predicate {
        ast::CfgPredicate::CfgAtom(atom) => {
            if let Some(ast::CfgAtomKey::Ident(key)) = atom.key() {
                try_gate(cx, features, file, key.text(), atom.syntax().text_range(), out);
            }
        }
        ast::CfgPredicate::CfgComposite(composite) => {
            let Some(keyword) = composite.keyword() else { return };
            let range = composite.syntax().text_range();
            match keyword.text() {
                "not" => {
                    let mut inner = composite.cfg_predicates();
                    if let (Some(single), None) = (inner.next(), inner.next()) {
                        self::predicate(cx, features, file, single, out);
                    }
                }
                "any" | "all" => {
                    for sub in composite.cfg_predicates() {
                        self::predicate(cx, features, file, sub, out);
                    }
                }
                "target" => {
                    target_compact(cx, features, file, range, out);
                    for sub in composite.cfg_predicates() {
                        let ast::CfgPredicate::CfgAtom(atom) = sub else { continue };
                        let Some(ast::CfgAtomKey::Ident(key)) = atom.key() else { continue };
                        if atom.eq_token().is_none() {
                            continue;
                        }
                        try_gate_target(cx, features, file, key.text(), atom.syntax().text_range(), out);
                    }
                }
                "version" => try_gate(cx, features, file, "version", range, out),
                _ => {}
            }
        }
    }
}

/// rustc's `parse_cfg_entry` of `item`, an element of the list of `#[doc(cfg(..))]` or
/// `#[link(.., cfg(..))]` in `file`: its gates, in order. Called where the attribute's parser
/// parses the predicate, after the list is checked to have one element; the errors of an
/// ill-formed predicate are the parser's.
pub(crate) fn cfg_entry(cx: &Cx<'_, '_>, features: &Features, file: HirFileId, item: &MetaItemOrLit) -> Vec<Diag> {
    // alloc: empty, allocates only when a gate is hit
    let mut out = Vec::new();
    entry(cx, features, file, item, &mut out);
    out
}

fn entry(cx: &Cx<'_, '_>, features: &Features, file: HirFileId, item: &MetaItemOrLit, out: &mut Vec<Diag>) {
    let MetaItemOrLit::Item(meta) = item else { return };
    match &meta.args {
        ArgParser::List(list) => match meta.path.word_sym() {
            Some("not") => {
                if let Some(single) = list.as_single() {
                    entry(cx, features, file, single, out);
                }
            }
            Some("any" | "all") => {
                for sub in list.mixed() {
                    entry(cx, features, file, sub, out);
                }
            }
            Some("target") => {
                target_compact(cx, features, file, meta.range(), out);
                for sub in list.mixed() {
                    let Some(sub_meta) = sub.meta_item() else { continue };
                    let (Some(name), ArgParser::NameValue(_)) = (sub_meta.path.word_sym(), &sub_meta.args) else {
                        continue;
                    };
                    if is_path_segment_keyword(name) {
                        return;
                    }
                    try_gate_target(cx, features, file, name, sub.range(), out);
                }
            }
            Some("version") => try_gate(cx, features, file, "version", meta.range(), out),
            _ => {}
        },
        ArgParser::NoArgs | ArgParser::NameValue(_) => {
            let Some(name) = meta.path.word_sym().filter(|it| !is_path_segment_keyword(it)) else { return };
            try_gate(cx, features, file, name, meta.range(), out);
        }
    }
}

/// rustc's `Symbol::is_path_segment_keyword`.
fn is_path_segment_keyword(name: &str) -> bool {
    matches!(name, "{{root}}" | "crate" | "self" | "Self" | "super" | "$crate")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_gated_cfg_has_its_feature() {
        assert_eq!(gated_cfg("overflow_checks"), Some("cfg_overflow_checks"));
        assert_eq!(gated_cfg("target_has_atomic_load_store"), Some("cfg_target_has_atomic"));
        assert_eq!(gated_cfg("target_has_atomic"), None);
        assert_eq!(gated_cfg("sanitizer_cfi_normalize_integers"), Some("cfg_sanitizer_cfi"));
        assert_eq!(gated_cfg("target_has_reliable_f128_math"), Some("cfg_target_has_reliable_f16_f128"));
        assert_eq!(gated_cfg("fmt_debug"), Some("fmt_debug"));
        assert_eq!(gated_cfg("unix"), None);
        let features = crate::lints::syntax::attrs::features::table::lang;
        for name in [
            "overflow_checks",
            "ub_checks",
            "contract_checks",
            "target_thread_local",
            "target_has_atomic_load_store",
            "sanitize",
            "version",
            "relocation_model",
            "sanitizer_cfi_generalize_pointers",
            "fmt_debug",
            "target_has_reliable_f16",
            "target_has_threads",
            "target_object_format",
        ] {
            let feature = gated_cfg(name).unwrap();
            assert!(features(feature).is_some_and(|it| it.kind.is_unstable()), "`{feature}` is no unstable feature");
        }
    }

    /// `try_gate_target`'s buffer holds every gated name, the longest being
    /// `sanitizer_cfi_normalize_integers`.
    #[test]
    fn every_gated_cfg_fits_the_targets_buffer() {
        assert_eq!(gated_cfg("sanitizer_cfi_normalize_integers"), Some("cfg_sanitizer_cfi"));
        assert!("sanitizer_cfi_normalize_integers".len() <= 48);
        assert_eq!(gated_cfg("target_has_reliable_f128_math"), Some("cfg_target_has_reliable_f16_f128"));
    }
}
