//! The language features of the pinned nightly: the one home (AGENTS.md rule 30) of what
//! `#![feature(name)]` means when `name` is a language feature, read by the build-script probe
//! decider (blaze-build-script `probe_ask`) and by the front end when it gates features.
//!
//! The list is rustc's `compiler/rustc_feature/src/{unstable,accepted,removed}.rs` at the
//! pinned nightly's commit (`pins/rust.txt`'s `commit`, `blaze_pins::COMMIT_HASH`; MIT or
//! Apache-2.0), as the generator `table.py` of the old diagnostics layer read it (its
//! `table/lang.rs`, blaze main 56c9fef80e): every name with what it is. A pin bump regenerates
//! it with the toolchain's other facts; a key that holds this list's identity holds the pinned
//! commit from blaze-pins.
//!
//! Rust's rule (the Unstable Book; rustc_expand `features`): a name `#![feature]` gives that is
//! a language feature enables it; an accepted one only warns (`stable_features`); a removed one
//! is E0557; any other name is taken as a library feature, which some loaded crate's stability
//! attributes must declare (else E0635).

/// What a language feature is in the pinned nightly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    /// In `accepted.rs`: stable; `#![feature]` of it is the `stable_features` warning.
    Accepted,
    /// In `unstable.rs` (unstable, incomplete or internal): enabled on the nightly.
    Unstable,
    /// In `removed.rs`: `#![feature]` of it is E0557.
    Removed,
}

/// The language feature `name`, if the pinned nightly has one.
pub fn lang(name: &str) -> Option<Lang> {
    LANG.binary_search_by(|(it, _)| (*it).cmp(name)).ok().map(|at| LANG[at].1)
}

use Lang::{Accepted as A, Removed as R, Unstable as U};

/// Every language feature, sorted by name.
#[rustfmt::skip]
pub static LANG: &[(&str, Lang)] = &[
    ("aarch64_target_feature", A), ("aarch64_unstable_target_feature", U), ("aarch64_ver_target_feature", U), ("abi_amdgpu_kernel", R),
    ("abi_avr_interrupt", U), ("abi_c_cmse_nonsecure_call", R), ("abi_cmse_nonsecure_call", U), ("abi_custom", A),
    ("abi_efiapi", A), ("abi_gpu_kernel", U), ("abi_msp430_interrupt", U), ("abi_ptx", U),
    ("abi_riscv_interrupt", U), ("abi_swift", U), ("abi_sysv64", A), ("abi_thiscall", A),
    ("abi_unadjusted", R), ("abi_vectorcall", U), ("abi_x86_interrupt", U), ("adt_const_params", U),
    ("advanced_slice_patterns", R), ("adx_target_feature", A), ("alloc_error_handler", U), ("allocator", R),
    ("allocator_internals", U), ("allow_fail", R), ("allow_internal_unsafe", U), ("allow_internal_unstable", U),
    ("anonymous_lifetime_in_impl_trait", U), ("apx_target_feature", U), ("arbitrary_enum_discriminant", A), ("arbitrary_self_types", U),
    ("arbitrary_self_types_pointers", U), ("arm_target_feature", U), ("asm_cfg", A), ("asm_const", A),
    ("asm_const_ptr", U), ("asm_experimental_arch", U), ("asm_experimental_reg", U), ("asm_goto", A),
    ("asm_goto_with_outputs", U), ("asm_sym", A), ("asm_unwind", U), ("associated_const_equality", R),
    ("associated_consts", A), ("associated_type_bounds", A), ("associated_type_defaults", U), ("associated_types", A),
    ("async_await", A), ("async_closure", A), ("async_drop", U), ("async_fn_in_dyn_trait", U),
    ("async_fn_in_trait", A), ("async_fn_track_caller", U), ("async_for_loop", U), ("async_trait_bounds", U),
    ("attr_literals", A), ("augmented_assignments", A), ("auto_traits", U), ("avr_target_feature", U),
    ("avx10_target_feature", U), ("avx512_target_feature", A), ("await_macro", R), ("bind_by_move_pattern_guards", A),
    ("bindings_after_at", A), ("box_patterns", R), ("box_syntax", R), ("bpf_target_feature", U),
    ("braced_empty_structs", A), ("builtin_syntax", U), ("c_str_literals", A), ("c_unwind", A),
    ("c_variadic", A), ("c_variadic_experimental_arch", U), ("c_variadic_naked_functions", A), ("capture_disjoint_fields", R),
    ("cfg_attr_multi", A), ("cfg_boolean_literals", A), ("cfg_contract_checks", U), ("cfg_doctest", A),
    ("cfg_overflow_checks", U), ("cfg_panic", A), ("cfg_relocation_model", U), ("cfg_sanitize", U),
    ("cfg_sanitizer_cfi", U), ("cfg_select", A), ("cfg_target_abi", A), ("cfg_target_compact", U),
    ("cfg_target_feature", A), ("cfg_target_has_atomic", U), ("cfg_target_has_atomic_equal_alignment", A), ("cfg_target_has_reliable_f16_f128", U),
    ("cfg_target_has_reliable_f16b", U), ("cfg_target_has_threads", U), ("cfg_target_object_format", U), ("cfg_target_thread_local", U),
    ("cfg_target_vendor", A), ("cfg_ub_checks", U), ("cfg_version", U), ("cfi_encoding", U),
    ("checked_type_aliases", U), ("clflushopt_target_feature", U), ("clone_closures", A), ("closure_lifetime_binder", U),
    ("closure_to_fn_coercion", A), ("closure_track_caller", U), ("cmpxchg16b_target_feature", A), ("cmse_nonsecure_entry", U),
    ("collapse_debuginfo", A), ("compile_error", A), ("compiler_builtins", U), ("conservative_impl_trait", A),
    ("const_async_blocks", U), ("const_block_items", U), ("const_c_variadic", U), ("const_closures", U),
    ("const_compare_raw_pointers", R), ("const_constructor", A), ("const_destruct", U), ("const_eval_limit", R),
    ("const_evaluatable_checked", R), ("const_extern_fn", A), ("const_fn", R), ("const_fn_floating_point_arithmetic", A),
    ("const_fn_fn_ptr_basics", A), ("const_fn_trait_bound", A), ("const_fn_transmute", A), ("const_fn_union", A),
    ("const_fn_unsize", A), ("const_for", U), ("const_generics", R), ("const_generics_defaults", A),
    ("const_if_match", A), ("const_impl_trait", A), ("const_in_array_repeat_expressions", R), ("const_indexing", A),
    ("const_let", A), ("const_loop", A), ("const_mut_refs", A), ("const_panic", A),
    ("const_param_ty_unchecked", U), ("const_precise_live_drops", U), ("const_raw_ptr_deref", A), ("const_raw_ptr_to_usize_cast", R),
    ("const_refs_to_cell", A), ("const_refs_to_static", A), ("const_trait_bound_opt_out", R), ("const_trait_impl", U),
    ("const_try", U), ("contracts", U), ("contracts_internals", U), ("copy_closures", A),
    ("coroutine_clone", U), ("coroutines", U), ("coverage_attribute", U), ("crate_in_paths", A),
    ("crate_visibility_modifier", R), ("csky_target_feature", U), ("custom_attribute", R), ("custom_code_classes_in_docs", A),
    ("custom_derive", R), ("custom_inner_attributes", U), ("custom_mir", U), ("custom_test_frameworks", U),
    ("debugger_visualizer", A), ("decl_macro", U), ("default_alloc_error_handler", A), ("default_field_values", U),
    ("default_type_parameter_fallback", R), ("default_type_params", A), ("deprecated", A), ("deprecated_safe", R),
    ("deprecated_suggestion", U), ("deref_patterns", U), ("derive_default_enum", A), ("derive_from", U),
    ("derive_smart_pointer", R), ("destructuring_assignment", A), ("diagnostic_namespace", A), ("diagnostic_on_const", U),
    ("diagnostic_on_move", U), ("diagnostic_on_type_error", U), ("diagnostic_on_unknown", U), ("diagnostic_on_unmatch_args", R),
    ("diagnostic_on_unmatched_args", U), ("diagnostic_opaque", U), ("do_not_recommend", A), ("doc_alias", A),
    ("doc_auto_cfg", R), ("doc_cfg", U), ("doc_cfg_hide", R), ("doc_keyword", R),
    ("doc_masked", U), ("doc_notable_trait", U), ("doc_primitive", R), ("doc_spotlight", R),
    ("dotdot_in_tuple_patterns", A), ("dotdoteq_in_patterns", A), ("drop_types_in_const", A), ("dropck_eyepatch", U),
    ("dropck_parametricity", R), ("dyn_compatible_for_dispatch", R), ("dyn_star", R), ("dyn_trait", A),
    ("effective_target_features", U), ("effects", R), ("eii_internals", U), ("ergonomic_clones", U),
    ("ermsb_target_feature", U), ("exclusive_range_pattern", A), ("exhaustive_integer_patterns", A), ("exhaustive_patterns", U),
    ("existential_type", R), ("explicit_extern_abis", U), ("explicit_generic_args_with_impl_trait", A), ("explicit_tail_calls", U),
    ("export_stable", U), ("expr_fragment_specifier_2024", A), ("extended_key_value_attributes", A), ("extended_varargs_abi_support", A),
    ("extern_absolute_paths", A), ("extern_crate_item_prelude", A), ("extern_crate_self", A), ("extern_in_paths", R),
    ("extern_item_impls", U), ("extern_prelude", A), ("extern_system_varargs", A), ("extern_types", U),
    ("external_doc", R), ("f128", U), ("f16", U), ("f16b", U),
    ("f16c_target_feature", A), ("ffi_const", U), ("ffi_pure", U), ("ffi_returns_twice", R),
    ("field_init_shorthand", A), ("field_projections", U), ("field_representing_type_raw", U), ("final_associated_functions", U),
    ("fma4_target_feature", U), ("fmt_debug", U), ("fn_align", U), ("fn_delegation", U),
    ("fn_must_use", A), ("fn_static", U), ("forced_keywords", U), ("format_args_capture", A),
    ("freeze_impls", U), ("frontmatter", U), ("function_arg_const_generics", U), ("fundamental", U),
    ("gca_adts", U), ("gca_const_items", U), ("gca_macroless_args", U), ("gca_macroless_items", U),
    ("gca_min_const_items", U), ("gen_blocks", U), ("generator_clone", R), ("generators", R),
    ("generic_arg_infer", A), ("generic_assert", U), ("generic_associated_types", A), ("generic_associated_types_extended", R),
    ("generic_const_args", R), ("generic_const_exprs", U), ("generic_const_items", U), ("generic_const_parameter_types", U),
    ("generic_param_attrs", A), ("generic_pattern_types", U), ("global_allocator", A), ("global_registration", U),
    ("globs", A), ("guard_patterns", U), ("half_open_range_patterns", A), ("half_open_range_patterns_in_slices", U),
    ("hexagon_target_feature", U), ("i128_type", A), ("if_let", A), ("if_let_guard", A),
    ("if_let_rescope", A), ("if_while_or_patterns", A), ("impl_header_lifetime_elision", A), ("impl_restriction", U),
    ("impl_trait_in_assoc_type", U), ("impl_trait_in_bindings", U), ("impl_trait_in_fn_trait_return", U), ("impl_trait_projections", A),
    ("import_shadowing", R), ("import_trait_associated_functions", U), ("imported_main", A), ("in_band_lifetimes", R),
    ("inclusive_range_syntax", A), ("infer_outlives_requirements", A), ("infer_static_outlives_requirements", R), ("inherent_associated_types", U),
    ("inline_const", A), ("inline_const_pat", R), ("instrument_fn", U), ("intra_doc_pointers", U),
    ("intrinsics", U), ("irrefutable_let_patterns", A), ("isa_attribute", A), ("issue_5723_bootstrap", R),
    ("item_like_imports", A), ("keylocker_x86", A), ("label_break_value", A), ("lahfsahf_target_feature", U),
    ("lang_items", U), ("large_assignments", U), ("lazy_normalization_consts", R), ("lazy_type_alias", R),
    ("let_chains", A), ("let_else", A), ("lifetime_capture_rules_2024", R), ("link_arg_attribute", U),
    ("link_args", R), ("link_cfg", U), ("link_enzyme_intrinsics", U), ("link_llvm_intrinsics", U),
    ("linkage", U), ("lint_reasons", A), ("loongarch_target_feature", U), ("loop_break_value", A),
    ("loop_hints", U), ("loop_match", U), ("m68k_target_feature", U), ("macro_at_most_once_rep", A),
    ("macro_attr", U), ("macro_attributes_in_derive_output", A), ("macro_derive", U), ("macro_guard_matcher", U),
    ("macro_lifetime_matcher", A), ("macro_literal_matcher", A), ("macro_metavar_expr", U), ("macro_metavar_expr_concat", U),
    ("macro_reexport", R), ("macro_rules", A), ("macro_vis_matcher", A), ("macroless_const_item_generic_const_args", R),
    ("macroless_generic_const_args", R), ("macros_in_extern", A), ("main", R), ("managed_boxes", R),
    ("marker_trait_attr", U), ("match_beginning_vert", A), ("match_default_bindings", A), ("member_constraints", A),
    ("min_adt_const_params", U), ("min_const_fn", A), ("min_const_generics", A), ("min_const_unsafe_fn", A),
    ("min_exhaustive_patterns", A), ("min_generic_const_args", R), ("min_specialization", U), ("min_type_alias_impl_trait", R),
    ("mips_target_feature", U), ("more_maybe_bounds", U), ("more_qualified_paths", U), ("more_struct_aliases", A),
    ("movbe_target_feature", A), ("movdir64b_target_feature", U), ("movdiri_target_feature", U), ("move_expr", U),
    ("move_ref_pattern", A), ("movrs_target_feature", U), ("multiple_supertrait_upcastable", U), ("must_not_suspend", U),
    ("mut_preserve_binding_mode_2024", R), ("mut_ref", U), ("mut_restriction", U), ("naked_functions", A),
    ("naked_functions_rustic_abi", U), ("naked_functions_target_feature", U), ("named_fn_trait_parameters", U), ("native_link_modifiers", A),
    ("native_link_modifiers_as_needed", U), ("native_link_modifiers_bundle", A), ("native_link_modifiers_verbatim", A), ("native_link_modifiers_whole_archive", A),
    ("needs_allocator", R), ("needs_panic_runtime", U), ("negate_unsigned", R), ("negative_bounds", U),
    ("negative_impls", U), ("never_patterns", U), ("never_type", A), ("never_type_fallback", R),
    ("new_range", U), ("nll", A), ("no_core", U), ("no_coverage", R),
    ("no_debug", R), ("no_sanitize", R), ("no_stack_check", R), ("no_std", A),
    ("non_ascii_idents", A), ("non_exhaustive", A), ("non_exhaustive_omitted_patterns_lint", U), ("non_lifetime_binders", U),
    ("non_modrs_mods", A), ("nvptx_target_feature", U), ("object_safe_for_dispatch", R), ("offset_of_enum", U),
    ("offset_of_nested", A), ("offset_of_slice", U), ("omit_gdb_pretty_printer_section", R), ("on_unimplemented", R),
    ("opt_out_copy", R), ("optimize_attribute", A), ("optin_builtin_traits", R), ("or_patterns", A),
    ("overlapping_marker_traits", R), ("packed_bundled_libs", A), ("panic_handler", A), ("panic_implementation", R),
    ("panic_runtime", U), ("param_attrs", A), ("patchable_function_entry", U), ("pattern_complexity_limit", U),
    ("pattern_parentheses", A), ("pattern_types", U), ("pin_ergonomics", U), ("platform_intrinsics", R),
    ("plugin", R), ("plugin_registrar", R), ("postfix_match", U), ("powerpc_target_feature", U),
    ("precise_capturing", A), ("precise_capturing_in_traits", A), ("precise_pointer_size_matching", R), ("pref_align_of", R),
    ("prelude_import", U), ("prfchw_target_feature", U), ("proc_macro", A), ("proc_macro_expr", R),
    ("proc_macro_gen", R), ("proc_macro_hygiene", U), ("proc_macro_mod", R), ("proc_macro_non_items", R),
    ("proc_macro_path_invoc", A), ("profiler_runtime", U), ("pub_macro_rules", R), ("pub_restricted", A),
    ("pushpop_unsafe", R), ("quad_precision_float", R), ("question_mark", A), ("quote", R),
    ("raw_dylib", A), ("raw_dylib_elf", U), ("raw_identifiers", A), ("raw_ref_op", A),
    ("re_rebalance_coherence", A), ("reborrow", U), ("ref_pat_eat_one_layer_2024", U), ("ref_pat_eat_one_layer_2024_structural", U),
    ("ref_pat_everywhere", R), ("reflect", R), ("register_attr", R), ("register_tool", U),
    ("relaxed_adts", A), ("relaxed_struct_unsize", A), ("repr128", A), ("repr_align", A),
    ("repr_align_enum", A), ("repr_packed", A), ("repr_simd", U), ("repr_transparent", A),
    ("result_ffi_guarantees", A), ("return_position_impl_trait_in_trait", A), ("return_type_notation", U), ("riscv_target_feature", U),
    ("rtm_target_feature", U), ("rust_2018_preview", R), ("rust_cold_cc", U), ("rust_preserve_none_cc", U),
    ("rust_tail_cc", U), ("rustc_attrs", U), ("rustc_diagnostic_macros", R), ("rustc_private", U),
    ("rustdoc_internals", U), ("rustdoc_missing_doc_code_examples", U), ("rvalue_static_promotion", A), ("s390x_target_feature", U),
    ("s390x_target_feature_vector", A), ("sanitize", U), ("sanitizer_runtime", R), ("self_in_typedefs", A),
    ("self_struct_ctor", A), ("sha512_sm_x86", A), ("shorter_tail_lifetimes", A), ("simd", R),
    ("simd_ffi", U), ("sized_hierarchy", U), ("slice_patterns", A), ("slicing_syntax", A),
    ("sparc_target_feature", U), ("specialization", U), ("splat", U), ("sse4a_target_feature", A),
    ("staged_api", U), ("start", R), ("static_align", U), ("static_in_const", A),
    ("static_nobundle", R), ("static_recursion", A), ("stmt_expr_attributes", U), ("strict_provenance_lints", U),
    ("string_deref_patterns", R), ("struct_field_attributes", A), ("struct_inherit", R), ("struct_variant", A),
    ("structural_match", U), ("super_let", U), ("supertrait_item_shadowing", U), ("target_feature", A),
    ("target_feature_11", A), ("target_feature_inline_always", R), ("tbm_target_feature", A), ("termination_trait", A),
    ("termination_trait_test", A), ("test_accepted_feature", A), ("test_binder_constraints", U), ("test_incomplete_feature", U),
    ("test_removed_feature", R), ("test_unstable_lint", U), ("thread_local", U), ("tool_attributes", A),
    ("tool_lints", A), ("track_caller", A), ("trait_alias", U), ("trait_upcasting", A),
    ("transmute_generic_consts", U), ("transparent_enums", A), ("transparent_unions", U), ("trivial_bounds", U),
    ("try_blocks", U), ("try_blocks_heterogeneous", U), ("tuple_indexing", A), ("type_alias_enum_variants", A),
    ("type_alias_impl_trait", U), ("type_changing_struct_update", U), ("type_macros", A), ("type_privacy_lints", A),
    ("unboxed_closures", U), ("underscore_const_names", A), ("underscore_imports", A), ("underscore_lifetimes", A),
    ("uniform_paths", A), ("universal_impl_trait", A), ("unmarked_api", R), ("unnamed_enum_variants", U),
    ("unnamed_fields", R), ("unqualified_local_imports", U), ("unrestricted_attribute_tokens", A), ("unsafe_attributes", A),
    ("unsafe_binders", U), ("unsafe_block_in_unsafe_fn", A), ("unsafe_extern_blocks", A), ("unsafe_fields", U),
    ("unsafe_no_drop_flag", R), ("unsized_const_params", U), ("unsized_fn_params", U), ("unsized_locals", R),
    ("unsized_tuple_coercion", R), ("untagged_unions", R), ("unwind_attributes", R), ("use_extern_macros", A),
    ("use_nested_groups", A), ("used", A), ("used_with_arg", U), ("view_types", U),
    ("visible_private_types", R), ("wasm_abi", R), ("wasm_target_feature", U), ("where_clause_attrs", U),
    ("while_let", A), ("windows_subsystem", A), ("with_negative_coherence", U), ("x86_amx_intrinsics", U),
    ("x87_target_feature", U), ("xop_target_feature", U), ("xtensa_target_feature", U), ("yeet_expr", U),
    ("yield_expr", U),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The list is sorted with each name once (the binary search's precondition), and holds the
    /// generator's counts: 214 accepted, 262 unstable (190 + 38 incomplete + 34 internal), 109
    /// removed. Control: drop a row and the counts fail.
    #[test]
    fn lang_features_are_sorted_and_counted() {
        assert!(LANG.windows(2).all(|it| it[0].0 < it[1].0), "the list is not sorted");
        let count = |kind: Lang| LANG.iter().filter(|it| it.1 == kind).count();
        assert_eq!((count(Lang::Accepted), count(Lang::Unstable), count(Lang::Removed)), (214, 262, 109));
        assert_eq!(lang("default_field_values"), Some(Lang::Unstable));
        assert_eq!(lang("never_type"), Some(Lang::Accepted));
        assert_eq!(lang("box_syntax"), Some(Lang::Removed));
        assert_eq!(lang("wasi_ext"), None, "a library feature is no language feature");
        println!("lang features: {} rows", LANG.len());
    }
}
