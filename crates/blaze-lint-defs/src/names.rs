//! The names of `register_builtins` that are not lints: its groups, the alias `bad_style`, the
//! renamed, removed and ignored names; and `register_internals`' group. `const`s, which the
//! registry's perfect hash of names is built from.

/// `add_lint_group!`, in order.
pub(super) const GROUPS: &[(&str, &[&str])] = &[
    ("nonstandard_style", &["non_camel_case_types", "non_snake_case", "non_upper_case_globals"]),
    (
        "unused",
        &[
            "unused_imports",
            "unused_variables",
            "unused_visibilities",
            "unused_assignments",
            "dead_code",
            "unused_mut",
            "unreachable_cfg_select_predicates",
            "unreachable_code",
            "unreachable_patterns",
            "unused_must_use",
            "unused_unsafe",
            "path_statements",
            "unused_attributes",
            "unused_macros",
            "unused_macro_rules",
            "unused_allocation",
            "unused_doc_comments",
            "unused_extern_crates",
            "unused_features",
            "unused_labels",
            "unused_parens",
            "unused_braces",
            "redundant_semicolons",
            "map_unit_fn",
        ],
    ),
    ("let_underscore", &["let_underscore_drop", "let_underscore_lock"]),
    (
        "rust_2018_idioms",
        &[
            "bare_trait_objects",
            "unused_extern_crates",
            "ellipsis_inclusive_range_patterns",
            "elided_lifetimes_in_paths",
            "explicit_outlives_requirements",
        ],
    ),
    ("keyword_idents", &["keyword_idents_2018", "keyword_idents_2024"]),
    ("refining_impl_trait", &["refining_impl_trait_reachable", "refining_impl_trait_internal"]),
    ("deprecated_safe", &["deprecated_safe_2024"]),
    (
        "unknown_or_malformed_diagnostic_attributes",
        &[
            "malformed_diagnostic_attributes",
            "malformed_diagnostic_format_literals",
            "misplaced_diagnostic_attributes",
            "unknown_diagnostic_attributes",
        ],
    ),
];

/// `register_group_alias`: the alias and the group, a silent one.
pub(super) const ALIASES: &[(&str, &str)] = &[("bad_style", "nonstandard_style")];

/// The groups of blaze's own lints, after rustc's: `blaze::hot`, the lints of hot code
/// (blaze-lints' `blaze::HOT`, which its tests hold to this); `blaze::general`, the lints on
/// at `warn` in every workspace; `blaze::internal`, the ones of blaze's own types and
/// conventions, allowed until a workspace turns them on (blaze-lints' `blaze::Kind`);
/// `blaze::reach`, the cross-configuration reachability lints, which a workspace makes strict.
pub(super) const BLAZE_GROUPS: &[(&str, &[&str])] = &[
    (
        "blaze::hot",
        &[
            "blaze::hot_clone",
            "blaze::hot_alloc",
            "blaze::hot_alloc_call",
            "blaze::hot_dyn",
            "blaze::default_hasher",
            "blaze::implicit_allocator",
            "blaze::large_move",
            "blaze::cold_error",
        ],
    ),
    (
        "blaze::general",
        &[
            "blaze::false_sharing",
            "blaze::seqcst_unexplained",
            "blaze::collect_then_consume",
            "blaze::hash_twice",
            "blaze::outline_generic_body",
            "blaze::cold_error",
            "blaze::frozen_collection",
            "blaze::intern_candidate",
            "blaze::small_map",
            "blaze::arc_clone_only_borrowed",
            "blaze::blocking_in_async",
            "blaze::relaxed_publish",
            "blaze::unwind_across_ffi",
            "blaze::unbounded_from_input",
            "blaze::dos_hasher_on_input",
            "blaze::panic_in_drop",
            "blaze::bounds_check_in_loop",
            "blaze::unbuffered_io",
            "blaze::stdout_lock_in_loop",
            "blaze::front_vec_ops",
            "blaze::missing_size_hint",
            "blaze::fmt_allocates",
            "blaze::jagged_vec",
            "blaze::stable_sort_needless",
            "blaze::dyn_in_hot_field",
            "blaze::profile_hot",
            "blaze::stale_hot_marker",
            "blaze::hot_marked_cold",
            "blaze::push_without_reserve",
            "blaze::scratch_in_loop",
            "blaze::boxed_recursive_node",
            "blaze::wide_index",
            "blaze::bounded_heap_field",
            "blaze::enum_vec_padded",
            "blaze::dyn_fn_param",
            "blaze::box_large_new",
            "blaze::checkfree",
            "blaze::nopanic",
            "blaze::noalloc",
            "blaze::nonblocking",
            "blaze::pure",
            "blaze::borrow_workaround",
            "blaze::native_contracts",
            "blaze::dead",
            "blaze::test_only",
            "blaze::cfg_only",
        ],
    ),
    (
        "blaze::pedantic_perf",
        &[
            "blaze::deep_drop_at_exit",
            "blaze::pod_serialized_by_field",
            "blaze::packable",
            "blaze::soa_candidate",
            "blaze::vecreport",
        ],
    ),
    (
        "blaze::internal",
        &[
            "blaze::hot_clone",
            "blaze::hot_alloc",
            "blaze::hot_dyn",
            "blaze::default_hasher",
            "blaze::parallelizable",
            "blaze::serial_dependence",
            "blaze::fiber_boundary",
            "blaze::hot_alloc_call",
            "blaze::error_debug",
            "blaze::wall_clock",
            "blaze::untyped_error",
            "blaze::dropped_error",
            "blaze::hash_order_output",
            "blaze::held_across_wait",
            "blaze::thread_local_in_waiting_fn",
            "blaze::lock_across_query",
            "blaze::lock_held_across_slow",
            "blaze::blocking_on_pool",
            "blaze::unnamed_wait",
            "blaze::shared_collection_lock",
            "blaze::open_wait_loop",
            "blaze::unrecorded_span",
            "blaze::implicit_allocator",
            "blaze::fixed_key_lookup",
            "blaze::struct_update",
            "blaze::bool_param",
            "blaze::sans_io_core",
            "blaze::key_ignores_field",
            "blaze::wildcard_arm",
            "blaze::large_move",
            "blaze::syscall_in_loop",
            "blaze::query_in_loop",
            "blaze::string_key",
            "blaze::nll_workaround",
            "blaze::lenient_deserialize",
        ],
    ),
    // The cross-configuration reachability lints (docs/v2/plan/design/multiconfig-check.md), also in
    // `blaze::general`: one name to make them strict, `reach = "deny"`.
    ("blaze::reach", &["blaze::dead", "blaze::test_only", "blaze::cfg_only"]),
];

/// `register_renamed`: the old name and the new.
pub(super) const RENAMED: &[(&str, &str)] = &[
    // blaze-lints' `blaze::RENAMED`.
    ("blaze::std_hashmap", "blaze::default_hasher"),
    ("single_use_lifetime", "single_use_lifetimes"),
    ("elided_lifetime_in_path", "elided_lifetimes_in_paths"),
    ("bare_trait_object", "bare_trait_objects"),
    ("unstable_name_collision", "unstable_name_collisions"),
    ("unused_doc_comment", "unused_doc_comments"),
    ("async_idents", "keyword_idents_2018"),
    ("exceeding_bitshifts", "arithmetic_overflow"),
    ("redundant_semicolon", "redundant_semicolons"),
    ("overlapping_patterns", "overlapping_range_endpoints"),
    ("disjoint_capture_migration", "rust_2021_incompatible_closure_captures"),
    ("or_patterns_back_compat", "rust_2021_incompatible_or_patterns"),
    ("non_fmt_panic", "non_fmt_panics"),
    ("unused_tuple_struct_fields", "dead_code"),
    ("static_mut_ref", "static_mut_refs"),
    ("temporary_cstring_as_ptr", "dangling_pointers_from_temporaries"),
    ("elided_named_lifetimes", "mismatched_lifetime_syntaxes"),
    ("fuzzy_provenance_casts", "implicit_provenance_casts"),
    ("lossy_provenance_casts", "implicit_provenance_casts"),
];

/// `register_ignored`: rustdoc's lints of old, before they were `rustdoc::`.
pub(super) const IGNORED: &[&str] = &[
    "broken_intra_doc_links",
    "private_intra_doc_links",
    "missing_crate_level_docs",
    "missing_doc_code_examples",
    "private_doc_tests",
    "invalid_codeblock_attributes",
    "invalid_html_tags",
    "non_autolinks",
];

/// `register_removed`: the name and why.
pub(super) const REMOVED: &[(&str, &str)] = &[
    ("intra_doc_link_resolution_failure", "use `rustdoc::broken_intra_doc_links` instead"),
    ("rustdoc", "use `rustdoc::all` instead"),
    ("unknown_features", "replaced by an error"),
    ("unsigned_negation", "replaced by negate_unsigned feature gate"),
    ("negate_unsigned", "cast a signed value instead"),
    ("raw_pointer_derive", "using derive with raw pointers is ok"),
    ("raw_pointer_deriving", "using derive with raw pointers is ok"),
    ("drop_with_repr_extern", "drop flags have been removed"),
    ("fat_ptr_transmutes", "was accidentally removed back in 2014"),
    ("deprecated_attr", "use `deprecated` instead"),
    ("transmute_from_fn_item_types", "always cast functions before transmuting them"),
    (
        "hr_lifetime_in_assoc_type",
        "converted into hard error, see issue #33685 <https://github.com/rust-lang/rust/issues/33685> for more information",
    ),
    (
        "inaccessible_extern_crate",
        "converted into hard error, see issue #36886 <https://github.com/rust-lang/rust/issues/36886> for more information",
    ),
    (
        "super_or_self_in_global_path",
        "converted into hard error, see issue #36888 <https://github.com/rust-lang/rust/issues/36888> for more information",
    ),
    (
        "overlapping_inherent_impls",
        "converted into hard error, see issue #36889 <https://github.com/rust-lang/rust/issues/36889> for more information",
    ),
    (
        "illegal_floating_point_constant_pattern",
        "converted into hard error, see issue #36890 <https://github.com/rust-lang/rust/issues/36890> for more information",
    ),
    (
        "illegal_struct_or_enum_constant_pattern",
        "converted into hard error, see issue #36891 <https://github.com/rust-lang/rust/issues/36891> for more information",
    ),
    (
        "lifetime_underscore",
        "converted into hard error, see issue #36892 <https://github.com/rust-lang/rust/issues/36892> for more information",
    ),
    (
        "extra_requirement_in_impl",
        "converted into hard error, see issue #37166 <https://github.com/rust-lang/rust/issues/37166> for more information",
    ),
    (
        "legacy_imports",
        "converted into hard error, see issue #38260 <https://github.com/rust-lang/rust/issues/38260> for more information",
    ),
    (
        "coerce_never",
        "converted into hard error, see issue #48950 <https://github.com/rust-lang/rust/issues/48950> for more information",
    ),
    (
        "resolve_trait_on_defaulted_unit",
        "converted into hard error, see issue #48950 <https://github.com/rust-lang/rust/issues/48950> for more information",
    ),
    ("private_no_mangle_fns", "no longer a warning, `#[no_mangle]` functions always exported"),
    ("private_no_mangle_statics", "no longer a warning, `#[no_mangle]` statics always exported"),
    ("bad_repr", "replaced with a generic attribute input check"),
    (
        "duplicate_matcher_binding_name",
        "converted into hard error, see issue #57742 <https://github.com/rust-lang/rust/issues/57742> for more information",
    ),
    (
        "incoherent_fundamental_impls",
        "converted into hard error, see issue #46205 <https://github.com/rust-lang/rust/issues/46205> for more information",
    ),
    (
        "legacy_constructor_visibility",
        "converted into hard error, see issue #39207 <https://github.com/rust-lang/rust/issues/39207> for more information",
    ),
    (
        "legacy_directory_ownership",
        "converted into hard error, see issue #37872 <https://github.com/rust-lang/rust/issues/37872> for more information",
    ),
    (
        "safe_extern_statics",
        "converted into hard error, see issue #36247 <https://github.com/rust-lang/rust/issues/36247> for more information",
    ),
    (
        "parenthesized_params_in_types_and_modules",
        "converted into hard error, see issue #42238 <https://github.com/rust-lang/rust/issues/42238> for more information",
    ),
    (
        "duplicate_macro_exports",
        "converted into hard error, see issue #35896 <https://github.com/rust-lang/rust/issues/35896> for more information",
    ),
    (
        "nested_impl_trait",
        "converted into hard error, see issue #59014 <https://github.com/rust-lang/rust/issues/59014> for more information",
    ),
    ("plugin_as_library", "plugins have been deprecated and retired"),
    (
        "unsupported_naked_functions",
        "converted into hard error, see RFC 2972 <https://github.com/rust-lang/rfcs/blob/master/text/2972-constrained-naked.md> for more information",
    ),
    (
        "mutable_borrow_reservation_conflict",
        "now allowed, see issue #59159 <https://github.com/rust-lang/rust/issues/59159> for more information",
    ),
    (
        "const_err",
        "converted into hard error, see issue #71800 <https://github.com/rust-lang/rust/issues/71800> for more information",
    ),
    (
        "safe_packed_borrows",
        "converted into hard error, see issue #82523 <https://github.com/rust-lang/rust/issues/82523> for more information",
    ),
    (
        "unaligned_references",
        "converted into hard error, see issue #82523 <https://github.com/rust-lang/rust/issues/82523> for more information",
    ),
    (
        "private_in_public",
        "replaced with another group of lints, see RFC <https://rust-lang.github.io/rfcs/2145-type-privacy.html> for more information",
    ),
    (
        "invalid_alignment",
        "converted into hard error, see PR #104616 <https://github.com/rust-lang/rust/pull/104616> for more information",
    ),
    (
        "implied_bounds_entailment",
        "converted into hard error, see PR #117984 <https://github.com/rust-lang/rust/pull/117984> for more information",
    ),
    (
        "coinductive_overlap_in_coherence",
        "converted into hard error, see PR #118649 <https://github.com/rust-lang/rust/pull/118649> for more information",
    ),
    (
        "illegal_floating_point_literal_pattern",
        "no longer a warning, float patterns behave the same as `==`",
    ),
    (
        "nontrivial_structural_match",
        "no longer needed, see RFC #3535 <https://rust-lang.github.io/rfcs/3535-constants-in-patterns.html> for more information",
    ),
    (
        "suspicious_auto_trait_impls",
        "no longer needed, see issue #93367 <https://github.com/rust-lang/rust/issues/93367> for more information",
    ),
    (
        "const_patterns_without_partial_eq",
        "converted into hard error, see RFC #3535 <https://rust-lang.github.io/rfcs/3535-constants-in-patterns.html> for more information",
    ),
    (
        "indirect_structural_match",
        "converted into hard error, see RFC #3535 <https://rust-lang.github.io/rfcs/3535-constants-in-patterns.html> for more information",
    ),
    (
        "deprecated_cfg_attr_crate_type_name",
        "converted into hard error, see issue #91632 <https://github.com/rust-lang/rust/issues/91632> for more information",
    ),
    (
        "pointer_structural_match",
        "converted into hard error, see RFC #3535 <https://rust-lang.github.io/rfcs/3535-constants-in-patterns.html> for more information",
    ),
    (
        "box_pointers",
        "it does not detect other kinds of allocations, and existed only for historical reasons",
    ),
    (
        "byte_slice_in_packed_struct_with_derive",
        "converted into hard error, see issue #107457 <https://github.com/rust-lang/rust/issues/107457> for more information",
    ),
    ("writes_through_immutable_pointer", "converted into hard error"),
    (
        "const_eval_mutable_ptr_in_final_value",
        "partially allowed now, otherwise turned into a hard error",
    ),
    (
        "where_clauses_object_safety",
        "converted into hard error, see PR #125380 <https://github.com/rust-lang/rust/pull/125380> for more information",
    ),
    (
        "cenum_impl_drop_cast",
        "converted into hard error, see <https://github.com/rust-lang/rust/issues/73333> for more information",
    ),
    (
        "ptr_cast_add_auto_to_object",
        "converted into hard error, see issue #127323 <https://github.com/rust-lang/rust/issues/127323> for more information",
    ),
    ("unsupported_fn_ptr_calling_conventions", "converted into hard error"),
    (
        "undefined_naked_function_abi",
        "converted into hard error, see PR #139001 <https://github.com/rust-lang/rust/issues/139001> for more information",
    ),
    (
        "abi_unsupported_vector_types",
        "converted into hard error, see <https://github.com/rust-lang/rust/issues/116558> for more information",
    ),
    (
        "missing_fragment_specifier",
        "converted into hard error, see <https://github.com/rust-lang/rust/issues/40107> for more information",
    ),
    ("wasm_c_abi", "the wasm C ABI has been fixed"),
    ("soft_unstable", "the general soft-unstable mechanism has been removed"),
    (
        "inline_always_mismatching_target_features",
        "replaced by a hard error for `#[inline(always)]` with `#[target_feature]`",
    ),
    (
        "repr_transparent_external_private_fields",
        "converted into hard error, see <https://github.com/rust-lang/rust/issues/78586> for more information",
    ),
    (
        "repr_transparent_non_zst_fields",
        "converted into hard error, see <https://github.com/rust-lang/rust/issues/78586> for more information",
    ),
];

/// `rustc::internal`, without `rustc::symbol_intern_string_literal`, as rustc leaves it out.
pub(super) static INTERNAL_GROUP: &[&str] = &[
    "rustc::lint_pass_impl_without_macro",
    "rustc::implicit_sysroot_crate_import",
    "rustc::bad_use_of_find_attr",
    "rustc::default_hash_types",
    "rustc::potential_query_instability",
    "rustc::untracked_query_information",
    "rustc::usage_of_ty_tykind",
    "rustc::usage_of_qualified_ty",
    "rustc::direct_use_of_rustc_type_ir",
    "rustc::non_glob_import_of_type_ir_inherent",
    "rustc::usage_of_type_ir_inherent",
    "rustc::usage_of_type_ir_traits",
    "rustc::bad_opt_access",
    "rustc::disallowed_pass_by_ref",
    "rustc::span_use_eq_ctxt",
    "rustc::rustc_must_match_exhaustively",
];
