//! The attribute names the toolchain reads itself, by what each one is to name resolution: the
//! one home (AGENTS.md rule 30) of the lists blaze-scope's lowering (the taker test), its crate
//! root (the built-in attribute scope the cells answer from), blaze-file-items' names digest and
//! blaze-real's census read (names-attr-3).
//!
//! Rust's rules, which this follows: a built-in attribute (the Reference, "Attributes", "Built-in
//! attributes index") is inert and is found in rustc's built-in attribute scope, after the
//! module's own names and the `#[macro_use]` prelude, so an attribute macro a nearer scope binds
//! under its name is ambiguous with it (E0659). The prelude's attribute MACROS are not built-in
//! attributes: the pinned library defines each as a macro (core/src/macros/mod.rs
//! `#[rustc_builtin_macro] pub macro test($item:item)`, `derive`, `global_allocator`,
//! `derive_const`, `cfg_eval`, `define_opaque`) re-exported by core/src/prelude/v1.rs, so an
//! import of one shadows the prelude's without ambiguity. A built-in derive's helper (`default`
//! of `#[rustc_builtin_macro(Default, attributes(default))]`, core/src/default.rs; `pointee` of
//! `CoercePointee`, core/src/marker.rs) is neither: it is in scope only on an item the derive is
//! applied to (the Reference, "Derive macro helper attributes"), so it is in no list here.

/// The built-in attributes: inert, answered by the built-in attribute scope. Sorted.
pub const BUILTIN_ATTRS: &[&str] = &[
    "align", "allow", "allow_internal_unsafe", "allow_internal_unstable", "automatically_derived",
    "cfg", "cfg_attr", "clippy", "cold", "collapse_debuginfo", "const_continue", "const_trait",
    "coroutine", "coverage", "crate_name", "crate_type", "custom_mir", "debugger_visualizer", "deny",
    "deprecated", "diagnostic", "do_not_recommend", "doc", "expect", "export_name", "feature",
    "ffi_const", "ffi_pure", "forbid", "fundamental", "ignore", "inline", "instruction_set", "lang",
    "link", "link_name", "link_ordinal", "link_section", "linkage", "loop_match", "macro_escape",
    "macro_export", "macro_use", "marker", "may_dangle", "must_not_suspend", "must_use", "naked",
    "no_builtins", "no_core", "no_implicit_prelude", "no_link", "no_main", "no_mangle", "no_std",
    "non_exhaustive", "omit_gdb_pretty_printer_section", "on_unimplemented", "optimize",
    "panic_handler", "patchable_function_entry", "path", "prelude_import", "proc_macro",
    "proc_macro_attribute", "proc_macro_derive", "recursion_limit", "register_tool", "repr",
    "rustfmt", "sanitize", "should_panic", "stable", "target_feature", "thread_local",
    "track_caller", "type_const", "type_length_limit", "unsafe", "unstable",
    "unstable_feature_bound", "used", "warn", "windows_subsystem",
];

/// The prelude's attribute macros (the pinned library's `#[rustc_builtin_macro] pub macro`s that
/// core/src/prelude/v1.rs re-exports): read by the lowering itself, resolved as macros, never in
/// the built-in attribute scope. Sorted.
/// (`bench` is one: core/src/prelude/v1.rs re-exports `bench` beside `test`, the pinned
/// library's `#[rustc_builtin_macro] pub macro bench($item:item)` under `feature(test)`.)
pub const PRELUDE_ATTR_MACROS: &[&str] = &["bench", "cfg_eval", "define_opaque", "derive", "derive_const", "global_allocator", "test"];

/// A one-segment attribute name the toolchain reads itself: a built-in attribute, a prelude
/// attribute macro, or a `rustc_` name (reserved to the toolchain's crates; no table lists them).
pub fn read_by_toolchain(name: &str) -> bool {
    name.starts_with("rustc_") || BUILTIN_ATTRS.binary_search(&name).is_ok() || PRELUDE_ATTR_MACROS.binary_search(&name).is_ok()
}

#[cfg(test)]
mod tests {
    use super::{BUILTIN_ATTRS, PRELUDE_ATTR_MACROS, read_by_toolchain};

    /// The lists are sorted (they are searched by halves) and share no name: a prelude macro is
    /// never a built-in attribute, and no derive helper is either. Expected: "test blaze-surface:
    /// exit 0; 1 passed, 0 failed".
    #[test]
    fn builtin_attribute_lists_are_sorted_and_apart() {
        assert!(BUILTIN_ATTRS.windows(2).all(|w| w[0] < w[1]), "BUILTIN_ATTRS is sorted, no repeat");
        assert!(PRELUDE_ATTR_MACROS.windows(2).all(|w| w[0] < w[1]), "PRELUDE_ATTR_MACROS is sorted, no repeat");
        assert!(PRELUDE_ATTR_MACROS.iter().all(|m| BUILTIN_ATTRS.binary_search(m).is_err()), "a prelude macro is no built-in attribute");
        for helper in ["default", "pointee", "coerce_shared"] {
            assert!(!read_by_toolchain(helper), "`{helper}` is a built-in derive's helper, not the toolchain's");
        }
        assert!(read_by_toolchain("inline") && read_by_toolchain("test") && read_by_toolchain("rustc_nounwind"));
    }
}
