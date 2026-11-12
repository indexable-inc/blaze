//! Which attributes no crate resolves as a macro: the built-in (inert) attributes by name, and
//! every `rustc_` attribute.
//!
//! The names are those of `hir_expand::inert_attr_macro::INERT_ATTRIBUTES` (rustc's
//! `builtin_attrs.rs`) that do not start with `rustc_`; the test
//! `the_built_in_names_are_the_old_front_ends` holds the two lists equal, so this one cannot
//! drift while the old table lives. This crate is under the old front end and cannot read its
//! table; when the old engine goes, this list is the one home.
//!
//! Cost: one binary search over [`NAMES`] an attribute, no allocation.

#![blaze::sans_io]

/// The built-in attributes that do not start with `rustc_`, in byte order.
pub const NAMES: &[&str] = &[
    "allow",
    "allow_internal_unsafe",
    "allow_internal_unstable",
    "automatically_derived",
    "cfg",
    "cfg_attr",
    "cfi_encoding",
    "cmse_nonsecure_entry",
    "cold",
    "collapse_debuginfo",
    "compiler_builtins",
    "const_trait",
    "coverage",
    "crate_id",
    "crate_name",
    "crate_type",
    "custom_mir",
    "debugger_visualizer",
    "default_lib_allocator",
    "deny",
    "deprecated",
    "deprecated_safe",
    "do_not_recommend",
    "doc",
    "expect",
    "export_name",
    "feature",
    "ffi_const",
    "ffi_pure",
    "forbid",
    "fundamental",
    "ignore",
    "inline",
    "instruction_set",
    "lang",
    "link",
    "link_name",
    "link_ordinal",
    "link_section",
    "linkage",
    "macro_escape",
    "macro_export",
    "macro_use",
    "marker",
    "may_dangle",
    "move_size_limit",
    "must_not_suspend",
    "must_use",
    "naked",
    "needs_allocator",
    "needs_panic_runtime",
    "no_builtins",
    "no_core",
    "no_implicit_prelude",
    "no_link",
    "no_main",
    "no_mangle",
    "no_start",
    "no_std",
    "non_exhaustive",
    "omit_gdb_pretty_printer_section",
    "optimize",
    "panic_handler",
    "panic_runtime",
    "path",
    "proc_macro",
    "proc_macro_attribute",
    "proc_macro_derive",
    "profiler_runtime",
    "recursion_limit",
    "reexport_test_harness_main",
    "register_tool",
    "repr",
    "sanitize",
    "should_panic",
    "stable",
    "start",
    "target_feature",
    "test_runner",
    "thread_local",
    "track_caller",
    "type_length_limit",
    "unstable",
    "unstable_feature_bound",
    "used",
    "warn",
    "windows_subsystem",
];

/// Whether `word`, an attribute's whole path, names an attribute that is never a macro: one
/// word, built in or `rustc_`'s.
pub fn is_built_in(word: &str) -> bool {
    !word.is_empty()
        && word.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && (word.starts_with("rustc_") || NAMES.binary_search(&word).is_ok())
}
