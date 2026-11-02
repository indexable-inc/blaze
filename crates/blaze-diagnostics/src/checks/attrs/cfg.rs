//! The errors of a `cfg` predicate a parser reads (`#[link(.., cfg(..))]`, `#[doc(cfg(..))]`):
//! rustc's `parse_cfg_entry`, with the feature gates of each predicate
//! (`checks::features::attributes::cfg_entry`) where rustc gives them. The `unexpected_cfgs`
//! lint of a predicate is `lints::syntax::check_cfg`'s.
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/attributes/cfg.rs` (`parse_cfg_entry`,
//! `parse_cfg_entry_version`, `parse_cfg_entry_target`, `parse_name_value`) (MIT or
//! Apache-2.0).

use super::{
    accept::Accept,
    args::{ArgParser, LitKind, MetaItem, MetaItemOrLit, MetaList, NameValue},
    parsers::parse_version,
};
use syntax::TextRange;
use crate::{
    checks::features::attributes,
    model::{Diag, Level, MultiSpan},
};

/// rustc's `parse_cfg_entry` of `item`: whether it is a predicate (`false` once it gave an
/// error).
pub(crate) fn entry(cx: &mut Accept<'_, '_, '_>, item: &MetaItemOrLit) -> bool {
    let meta = match item {
        MetaItemOrLit::Item(meta) => meta,
        MetaItemOrLit::Lit(lit) if matches!(lit.kind, LitKind::Bool(_)) => return true,
        MetaItemOrLit::Lit(lit) => {
            cx.expected_identifier(lit.range);
            return false;
        }
    };
    match &meta.args {
        ArgParser::List(list) => match meta.path.word_sym() {
            Some("not") => {
                let Some(single) = list.as_single() else {
                    cx.expected_single_argument(list.range, list.len());
                    return false;
                };
                entry(cx, single)
            }
            Some("any" | "all") => {
                for sub in list.mixed() {
                    entry(cx, sub);
                }
                true
            }
            Some("target") => target(cx, meta, list),
            Some("version") => version(cx, item, list),
            _ => {
                // alloc: the error's expected names
                let mut possibilities = vec!["any", "all", "not", "target"];
                if cx.feature("cfg_version") {
                    possibilities.push("version");
                }
                cx.expected_specific_argument(meta.range(), &possibilities);
                false
            }
        },
        ArgParser::NoArgs | ArgParser::NameValue(_) => {
            if meta.path.word_sym().is_none_or(is_path_segment_keyword) {
                cx.expected_identifier(meta.path.range);
                return false;
            }
            gates(cx, item);
            string_value(cx, meta.args.as_name_value())
        }
    }
}

/// `parse_cfg_entry_version`.
fn version(cx: &mut Accept<'_, '_, '_>, item: &MetaItemOrLit, list: &MetaList) -> bool {
    gates(cx, item);
    let Some(version) = list.as_single() else {
        let diag = cx.err(None, "expected single version literal", list.range);
        cx.emit_err(diag);
        return false;
    };
    let MetaItemOrLit::Lit(lit) = version else {
        let diag = cx.err(None, "expected a version literal", version.range());
        cx.emit_err(diag);
        return false;
    };
    let LitKind::Str(text) = &lit.kind else {
        let diag = cx.err(None, "expected a version literal", lit.range);
        cx.emit_err(diag);
        return false;
    };
    if parse_version(text).is_none() {
        let mut diag =
            Diag::new(Level::Warning, None, "unknown version literal format, assuming it refers to a future version");
        diag.span = MultiSpan::of(cx.span(lit.range));
        cx.emit_err(diag);
    }
    true
}

/// `parse_cfg_entry_target`: `target(os = "..", ..)`, each a `target_os = ".."`. The gate of
/// `target(..)` itself is that of an empty one; each entry's, that of its `target_` name.
fn target(cx: &mut Accept<'_, '_, '_>, meta: &MetaItem, list: &MetaList) -> bool {
    // alloc: empty, allocates only when a gate is hit
    let mut out = Vec::new();
    attributes::target_compact(cx.cx, &cx.session.features, cx.attr.file, meta.range(), &mut out);
    for diag in out.drain(..) {
        cx.emit_err(diag);
    }
    for sub in list.mixed() {
        let Some((name, value)) = cx.expect_name_value_item(sub, None) else { continue };
        if is_path_segment_keyword(&name.name) {
            cx.expected_identifier(name.range);
            return false;
        }
        let range = TextRange::new(name.range.start(), value.args_range().end());
        attributes::try_gate_target(cx.cx, &cx.session.features, cx.attr.file, &name.name, range, &mut out);
        for diag in out.drain(..) {
            cx.emit_err(diag);
        }
        string_value(cx, Some(value));
    }
    true
}

/// The value check of `parse_name_value`, after its gate.
fn string_value(cx: &mut Accept<'_, '_, '_>, value: Option<&NameValue>) -> bool {
    match value {
        Some(value) if !matches!(value.value.kind, LitKind::Str(_)) => {
            cx.expected_string_literal(value.value.range, Some(&value.value));
            false
        }
        _ => true,
    }
}

/// The gates of the predicate `item` alone (a name, an empty `target()` or a `version(..)`).
fn gates(cx: &mut Accept<'_, '_, '_>, item: &MetaItemOrLit) {
    for diag in attributes::cfg_entry(cx.cx, &cx.session.features, cx.attr.file, item) {
        cx.emit_err(diag);
    }
}

/// rustc's `Symbol::is_path_segment_keyword`.
fn is_path_segment_keyword(name: &str) -> bool {
    matches!(name, "{{root}}" | "crate" | "self" | "Self" | "super" | "$crate")
}
