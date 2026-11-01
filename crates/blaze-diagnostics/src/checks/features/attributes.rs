//! The feature gates rustc's attribute parser checks: an attribute whose parser is unstable
//! ([`gate`], `check_attribute_stability`), the gated `cfg` predicates of `#[cfg]`,
//! `#[cfg_attr]`, `cfg!`, `#[doc(cfg(..))]` and `#[link(cfg(..))]` (`cfg`), and the gates of
//! single parsers' arguments (`#[doc(rust_logo)]`, `#[repr(simd)]`, the unstable `#[link]` kinds
//! and modifiers, `#[used(..)]`, `#[target_feature]` on a naked function).
//!
//! - `table`: each parser's path and gate, generated from rustc's parsers by `table.py`.
//! - `cfg`: rustc's `GATED_CFGS` and the gates of a `cfg` predicate.
//!
//! [`expansion`] gives the gates expansion meets as it configures the crate: the `cfg`s, and the
//! attributes of macro calls, which rustc parses there; the rest attribute parsing
//! (`checks::attrs`) gives as it lowers the AST, calling [`gate`] and the functions here.
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/stability.rs` (`check_attribute_stability`,
//! `unstable!`), `compiler/rustc_attr_parsing/src/interface.rs` (`parse_attribute_list`),
//! `compiler/rustc_attr_parsing/src/attributes/{cfg,doc,repr,link_attrs,codegen_attrs}.rs`
//! (their `feature_err`s), `compiler/rustc_feature/src/builtin_attrs.rs` (`AttributeStability`,
//! `GATED_CFGS`), `compiler/rustc_expand/src/expand.rs` (`flat_map_node`, `expand_cfg_true`,
//! `expand_cfg_attr`, `check_attributes`), `compiler/rustc_expand/src/config.rs`
//! (`expand_cfg_attr`) and `compiler/rustc_builtin_macros/src/cfg.rs` (`expand_cfg`) (MIT or
//! Apache-2.0).

mod cfg;
mod table;

pub(crate) use cfg::{cfg_entry, target_compact, try_gate, try_gate_target};

use blaze_hash::HashMap;
use blaze_items::Fact;
use hir::{HirFileId, InFile};
use syntax::{
    SyntaxNodePtr, TextRange, WalkEvent,
    view::{self as ast, AstNode as _},
};

use super::{Features, allows_unstable, feature_err};
use crate::{
    cx::Cx,
    lints::syntax::{
        Walk,
        attrs::{Attr, Owner, Target, table::index},
        source_fact,
    },
    lower::{Found, Phase},
    model::Diag,
};

/// An unstable parser's gate: rustc's `AttributeStability::Unstable`, its `gate_name` and
/// `notes`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Gate {
    pub(crate) feature: &'static str,
    pub(crate) notes: &'static [&'static str],
}

/// The stability of the builtin parser of `path`: `None` when no parser takes it, `Some(None)`
/// when it is stable.
pub(crate) fn stability(path: &[&str]) -> Option<Option<Gate>> {
    Some(table::STABILITY[index::of(path)?].1)
}

/// rustc's `check_attribute_stability` of `attr`: E0658 when its parser is unstable, the
/// feature is not enabled and the expansion it comes from is not allowed it, at its path.
pub(crate) fn gate(cx: &Cx<'_, '_>, features: &Features, attr: &Attr<'_>) -> Option<Diag> {
    let path = attr.meta()?.path()?;
    let mut names: [Option<ast::NameRef<'_>>; 2] = [None, None];
    let mut len = 0;
    for segment in path.segments() {
        *names.get_mut(len)? = Some(segment.name_ref()?);
        len += 1;
    }
    let segments = [segment_text(names[0]), segment_text(names[1])];
    let gate = stability(&segments[..len])??;
    if features.enabled(gate.feature) || allows_unstable(cx, attr.file, attr.range(), gate.feature) {
        return None;
    }
    let name = segments[..len].join("::");
    let span = cx.span(attr.in_file(attr.path_range()));
    let mut diag = feature_err(features, gate.feature, span, explain(gate.feature, &name));
    if gate.feature == "staged_api" {
        diag.children.clear();
    }
    if gate.feature == "rustc_attrs" {
        // alloc: the error's note
        diag.note(format!("the `{name}` attribute is an internal implementation detail that will never be stable"));
    }
    for note in gate.notes {
        diag.note(*note);
    }
    features_fact(cx, &mut diag);
    Some(diag)
}

/// A segment's name as rustc's path has it: a raw identifier's without `r#`.
fn segment_text(name: Option<ast::NameRef<'_>>) -> &str {
    let Some(name) = name else { return "" };
    let text = name.text();
    text.strip_prefix("r#").unwrap_or(text)
}

/// The message of `check_attribute_stability` for an attribute of path `name` gated by
/// `feature`.
fn explain(feature: &str, name: &str) -> String {
    let text = match feature {
        "rustc_attrs" => "use of an internal attribute",
        "staged_api" => "stability attributes may not be used outside of the standard library",
        "custom_mir" => "the `custom_mir` attribute is just used for the Rust test suite",
        "allow_internal_unsafe" => "the `allow_internal_unsafe` attribute side-steps the `unsafe_code` lint",
        "allow_internal_unstable" => {
            "the `allow_internal_unstable` attribute side-steps feature gating and stability checks"
        }
        "compiler_builtins" => {
            "the `compiler_builtins` attribute is used to identify the `compiler_builtins` crate which contains compiler-rt intrinsics and will never be stable"
        }
        "custom_test_frameworks" => "custom test frameworks are an unstable feature",
        "linkage" => "the `linkage` attribute is experimental and not portable across platforms",
        "dropck_eyepatch" => "the `may_dangle` attribute has unstable semantics and may be removed in the future",
        "intrinsics" => "the `rustc_intrinsic` attribute is used to declare intrinsics as function items",
        "lang_items" => "lang items are subject to change",
        "prelude_import" => "the `prelude_import` attribute is for use by rustc only",
        "profiler_runtime" => {
            "the `profiler_runtime` attribute is used to identify the `profiler_builtins` crate which contains the profiler runtime and will never be stable"
        }
        "thread_local" => {
            "the `thread_local` attribute is an experimental feature, and does not currently handle destructors"
        }
        // alloc: the error's message
        _ => return format!("the `{name}` attribute is an experimental feature"),
    };
    // alloc: the error's message
    text.to_owned()
}

/// The gates expansion meets as it configures the crate, at `Phase::Expand`, in the order it
/// meets them: tree by tree (the crate's files, then what macros expand to), each node's
/// `#[cfg]`s and `#[cfg_attr]`s in order (a `cfg_attr`'s attributes when its predicate holds;
/// nothing after a `cfg` that does not, nor in the node), the predicate of each `cfg!`, and the
/// [`gate`]s of a macro call's attributes.
pub(super) fn expansion(cx: &Cx<'_, '_>, walk: &Walk<'_>, owners: &[Owner<'_>], features: &Features) -> Vec<Found> {
    let calls: HashMap<InFile<SyntaxNodePtr>, &Owner<'_>> =
        // alloc: setup, the macro calls' owners, once per check
        owners.iter().filter(|it| it.target == Target::MacroCall).map(|it| (it.at(), it)).collect();
    // alloc: empty, allocates only when a gate is hit
    let mut found = Vec::new();
    // alloc: empty, reused for every node
    let mut diags = Vec::new();
    let trees = walk.files().chain(walk.trees.iter().filter(|it| !it.in_file()));
    for tree in trees {
        let mut events = tree.root.preorder();
        while let Some(event) = events.next() {
            let WalkEvent::Enter(node) = event else { continue };
            let at = tree.ptr(node);
            if let Some(owner) = ast::AnyHasAttrs::cast(node) {
                let kept = configure(cx, walk, features, tree.file, owner, &mut diags);
                let replaced = ast::Item::can_cast(node.kind()) && tree.replaced.contains(&node);
                if !kept || replaced {
                    events.skip_subtree();
                }
                if kept
                    && !replaced
                    && let Some(call) = calls.get(&at)
                {
                    diags.extend(call.attrs.iter().filter_map(|attr| gate(cx, features, attr)));
                }
            } else if let Some(predicate) = builtin_cfg(node) {
                cfg::predicate(cx, features, tree.file, predicate, &mut diags);
            }
            for mut diag in diags.drain(..) {
                source_fact(cx, &mut diag, at);
                found.push(Found::new(diag, Phase::Expand));
            }
        }
    }
    found
}

/// The predicate of what `cfg!` expands to: `builtin # cfg(..)`, an expression or a pattern.
fn builtin_cfg(node: ast::Node<'_>) -> Option<ast::CfgPredicate<'_>> {
    if let Some(expr) = ast::CfgPredExpr::cast(node) {
        return expr.cfg_predicate();
    }
    ast::CfgPredPat::cast(node)?.cfg_predicate()
}

/// rustc's `flat_map_node` of `owner`'s `#[cfg]`s and `#[cfg_attr]`s, in `file`: the gates of
/// each predicate it parses in `diags`, and whether `cfg` keeps the node.
fn configure(
    cx: &Cx<'_, '_>,
    walk: &Walk<'_>,
    features: &Features,
    file: HirFileId,
    owner: ast::AnyHasAttrs<'_>,
    diags: &mut Vec<Diag>,
) -> bool {
    ast::attrs_including_inner(&owner)
        .filter_map(|it| it.meta())
        .all(|meta| configure_meta(cx, walk, features, file, meta, diags))
}

/// One attribute of [`configure`]: a `cfg_attr`'s predicate is parsed and, when it holds, its
/// attributes taken in its place; a `cfg`'s is parsed and decides whether the node is kept.
fn configure_meta(
    cx: &Cx<'_, '_>,
    walk: &Walk<'_>,
    features: &Features,
    file: HirFileId,
    mut meta: ast::Meta<'_>,
    diags: &mut Vec<Diag>,
) -> bool {
    while let ast::Meta::UnsafeMeta(unsafe_meta) = meta {
        let Some(inner) = unsafe_meta.meta() else { return true };
        meta = inner;
    }
    match meta {
        ast::Meta::CfgMeta(cfg_meta) => {
            let Some(predicate) = cfg_meta.cfg_predicate() else { return true };
            cfg::predicate(cx, features, file, predicate, diags);
            holds(walk, predicate)
        }
        ast::Meta::CfgAttrMeta(cfg_attr) => {
            let Some(predicate) = cfg_attr.cfg_predicate() else { return true };
            cfg::predicate(cx, features, file, predicate, diags);
            !holds(walk, predicate) || cfg_attr.metas().all(|it| configure_meta(cx, walk, features, file, it, diags))
        }
        _ => true,
    }
}

/// Whether the crate's cfg makes `predicate` hold, as [`Walk::on`] evaluates it: one it cannot
/// evaluate holds.
fn holds(walk: &Walk<'_>, predicate: ast::CfgPredicate<'_>) -> bool {
    walk.cfg.check(&::cfg::CfgExpr::parse_from_view(predicate)) != Some(false)
}

/// The E0658 of `feature` where the parser checks the feature itself, not the expansion's
/// `allow_internal_unstable`.
fn gated(cx: &Cx<'_, '_>, features: &Features, feature: &str, at: InFile<TextRange>, explain: &str) -> Option<Diag> {
    if features.enabled(feature) {
        return None;
    }
    let mut diag = feature_err(features, feature, cx.span(at), explain);
    features_fact(cx, &mut diag);
    Some(diag)
}

/// `#[doc(rust_logo)]` without `rustdoc_internals`, at the `rust_logo` path (rustc's
/// `DocParser`, once the argument is checked to be a word at the crate level).
pub(crate) fn doc_rust_logo(cx: &Cx<'_, '_>, features: &Features, path: InFile<TextRange>) -> Option<Diag> {
    gated(cx, features, "rustdoc_internals", path, "the `#[doc(rust_logo)]` attribute is used for Rust branding")
}

/// `#[repr(simd)]` without `repr_simd`, at the `simd` item (rustc's `parse_repr`, before it
/// checks the target).
pub(crate) fn repr_simd(cx: &Cx<'_, '_>, features: &Features, item: InFile<TextRange>) -> Option<Diag> {
    gated(cx, features, "repr_simd", item, "SIMD types are experimental and possibly buggy")
}

/// The modifier `+as-needed` or `-as-needed` of a dylib, framework or raw-dylib `#[link]`
/// without `native_link_modifiers_as_needed`, at the `modifiers` string (rustc's
/// `report_unstable_modifier!`, before it assigns the modifier). `modifier` is without its
/// sign.
pub(crate) fn link_modifier(
    cx: &Cx<'_, '_>,
    features: &Features,
    modifier: &str,
    modifiers: InFile<TextRange>,
) -> Option<Diag> {
    if features.enabled("native_link_modifiers_as_needed") {
        return None;
    }
    // alloc: the error's message
    let explain = format!("linking modifier `{modifier}` is unstable");
    gated(cx, features, "native_link_modifiers_as_needed", modifiers, &explain)
}

/// `kind = "raw-dylib"` on an ELF target that is not Windows, without `raw_dylib_elf`, at the
/// kind's value. rustc gives it on a nightly (`features.nightly`) and `RawDylibOnlyWindows`
/// otherwise; `None` when the feature is enabled, where neither is given.
pub(crate) fn link_kind_raw_dylib_elf(cx: &Cx<'_, '_>, features: &Features, value: InFile<TextRange>) -> Option<Diag> {
    gated(cx, features, "raw_dylib_elf", value, "link kind `raw-dylib` is unstable on ELF platforms")
}

/// `kind = "link-arg"` without `link_arg_attribute`, at the kind's value.
pub(crate) fn link_kind_link_arg(cx: &Cx<'_, '_>, features: &Features, value: InFile<TextRange>) -> Option<Diag> {
    gated(cx, features, "link_arg_attribute", value, "link kind `link-arg` is unstable")
}

/// `#[link(.., cfg(..))]` without `link_cfg`, at the `cfg(..)` item, before its predicate is
/// parsed (whose gates are [`cfg_entry`]'s).
pub(crate) fn link_cfg(cx: &Cx<'_, '_>, features: &Features, item: InFile<TextRange>) -> Option<Diag> {
    gated(cx, features, "link_cfg", item, "link cfg is unstable")
}

/// `#[target_feature(..)]` on a `#[naked]` function without `naked_functions_target_feature`,
/// at the `target_feature` attribute (rustc's `NakedParser` check of the function's other
/// attributes).
pub(crate) fn naked_target_feature(cx: &Cx<'_, '_>, features: &Features, attr: InFile<TextRange>) -> Option<Diag> {
    let explain = "`#[target_feature(/* ... */)]` is currently unstable on `#[naked]` functions";
    gated(cx, features, "naked_functions_target_feature", attr, explain)
}

/// `#[used(compiler)]` or `#[used(linker)]` (`which`) without `used_with_arg`, at the
/// attribute (`attr_span`).
pub(crate) fn used_with_arg(cx: &Cx<'_, '_>, features: &Features, which: &str, attr: InFile<TextRange>) -> Option<Diag> {
    if features.enabled("used_with_arg") {
        return None;
    }
    // alloc: the error's message
    let explain = format!("`#[used({which})]` is currently unstable");
    gated(cx, features, "used_with_arg", attr, &explain)
}

/// Records on `diag` what a feature gate reads besides its source: the `#![feature]`s of the
/// crate root.
pub(crate) fn features_fact(cx: &Cx<'_, '_>, diag: &mut Diag) {
    let root = cx.krate.root_module(cx.db);
    diag.fact(Fact::Signature(cx.module_key(root)), "the `#![feature]`s of the crate root");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lints::syntax::attrs::table as builtins;

    #[test]
    fn the_table_is_sorted_and_every_path_is_a_builtin_parsers() {
        assert!(table::STABILITY.windows(2).all(|it| it[0].0 < it[1].0));
        for (path, _) in table::STABILITY {
            assert!(builtins::builtin(path).is_some(), "{path:?} is no builtin parser's path");
        }
    }

    #[test]
    fn the_table_is_by_parser_number() {
        assert_eq!(table::STABILITY.len(), index::COUNT);
        for (at, (path, _)) in table::STABILITY.iter().enumerate() {
            assert_eq!(*path, index::path(at), "the gate numbered {at}");
        }
        let by_search = |path: &[&str]| {
            let at = table::STABILITY.binary_search_by(|(it, _)| (*it).cmp(path)).ok()?;
            Some(table::STABILITY[at].1)
        };
        let misses: [&[&str]; 5] = [&[], &["diagnostic"], &["no_such_attribute"], &["inline", "x"], &["Inline"]];
        for path in table::STABILITY.iter().map(|(path, _)| *path).chain(misses) {
            assert_eq!(stability(path), by_search(path), "{path:?}");
        }
    }

    #[test]
    fn a_parser_is_found_with_its_gate() {
        assert_eq!(stability(&["inline"]), Some(None));
        assert_eq!(stability(&["diagnostic", "on_unimplemented"]), Some(None));
        assert_eq!(stability(&["no_such_attribute"]), None);
        assert_eq!(stability(&["thread_local"]), Some(Some(Gate { feature: "thread_local", notes: &[] })));
        assert_eq!(stability(&["may_dangle"]).flatten().map(|it| it.feature), Some("dropck_eyepatch"));
        assert_eq!(stability(&["stable"]).flatten().map(|it| it.feature), Some("staged_api"));
        assert_eq!(stability(&["lang"]).flatten().map(|it| it.feature), Some("lang_items"));
        let dummy = stability(&["rustc_dummy"]).flatten().unwrap();
        assert_eq!(dummy.feature, "rustc_attrs");
        assert_eq!(dummy.notes, ["the `rustc_dummy` attribute is used for rustc unit tests"]);
        let nonnull = stability(&["rustc_nonnull_optimization_guaranteed"]).flatten().unwrap();
        assert_eq!(nonnull.notes.len(), 2);
    }

    #[test]
    fn the_special_explanations_are_rustcs() {
        assert_eq!(explain("rustc_attrs", "rustc_dummy"), "use of an internal attribute");
        assert_eq!(explain("ffi_const", "ffi_const"), "the `ffi_const` attribute is an experimental feature");
        assert_eq!(explain("dropck_eyepatch", "may_dangle"), "the `may_dangle` attribute has unstable semantics and may be removed in the future");
        assert_eq!(explain("splat", "rustc_splat"), "the `rustc_splat` attribute is an experimental feature");
    }
}
