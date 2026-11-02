//! The lints of rustc's `declare_lint!`s, in the order `register_builtins` registers them: the
//! pre-expansion pass's, the early pass's, the late module pass's (each in its passes' order),
//! `foreign_modules`', then `rustc_lint_defs`' hardwired ones.
//!
//! These are `const`s so that the registry numbers and indexes them as blaze is built; the
//! lints with their ids are `index::ALL`'s, which is where every `&'static Lint` points.

use edition::Edition::{Edition2018, Edition2021, Edition2024};

use super::{
    Level::{Allow, Deny, Warn},
    Lint,
    Reason::{
        Custom, EditionAndFutureReleaseError, EditionAndFutureReleaseSemanticsChange,
        EditionError, EditionSemanticsChange, FutureReleaseError,
    },
};

const fn l(name: &'static str, level: super::Level, desc: &'static str) -> Lint {
    Lint::new(name, level, desc)
}

pub(super) const BUILTIN: &[Lint] = &[
    // BuiltinCombinedPreExpansionLintPass
    l("keyword_idents_2018", Allow, "detects edition keywords being used as an identifier")
        .fcw(EditionError(Edition2018, "new-keywords")),
    l("keyword_idents_2024", Allow, "detects edition keywords being used as an identifier")
        .fcw(EditionError(Edition2024, "gen-keyword")),
    // BuiltinCombinedEarlyLintPass
    l("unused_parens", Warn, "`if`, `match`, `while` and `return` do not need parentheses"),
    l("unused_braces", Warn, "unnecessary braces around an expression"),
    l("unused_import_braces", Allow, "unnecessary braces around an imported item"),
    l("unsafe_code", Allow, "usage of `unsafe` code and other potentially unsound constructs")
        .eval_always(),
    l("special_module_name", Warn, "module declarations for files with a special meaning"),
    l("anonymous_parameters", Warn, "detects anonymous parameters")
        .fcw(EditionError(Edition2018, "trait-fn-parameters")),
    l("ellipsis_inclusive_range_patterns", Warn, "`...` range patterns are deprecated")
        .fcw(EditionError(Edition2021, "warnings-promoted-to-error")),
    l(
        "non_camel_case_types",
        Warn,
        "types, variants, traits and type parameters should have camel case names",
    ),
    l("while_true", Warn, "suggest using `loop { }` instead of `while true { }`"),
    l("non_ascii_idents", Allow, "detects non-ASCII identifiers").crate_level(),
    l("uncommon_codepoints", Warn, "detects uncommon Unicode codepoints in identifiers")
        .crate_level(),
    l("confusable_idents", Warn, "detects visually confusable pairs between identifiers")
        .crate_level(),
    l(
        "mixed_script_confusables",
        Warn,
        "detects Unicode scripts whose mixed script confusables codepoints are solely used",
    )
    .crate_level(),
    l(
        "incomplete_features",
        Warn,
        "incomplete features that may function improperly in some or all cases",
    ),
    l("internal_features", Warn, "internal features are not supposed to be used"),
    l("redundant_semicolons", Warn, "detects unnecessary trailing semicolons"),
    l("unused_doc_comments", Warn, "detects doc comments that aren't used by rustdoc"),
    l(
        "edition_2024_expr_fragment_specifier",
        Allow,
        "The `expr` fragment specifier will accept more expressions in the 2024 edition. To keep the existing behavior, use the `expr_2021` fragment specifier.",
    )
    .fcw(EditionSemanticsChange(Edition2024, "macro-fragment-specifiers")),
    l("ambiguous_negative_literals", Allow, "ambiguous negative literals operations").external(),
    l("double_negations", Warn, "detects expressions of the form `--x`"),
    // BuiltinCombinedLateLintModPass
    l(
        "for_loops_over_fallibles",
        Warn,
        "for-looping over an `Option` or a `Result`, which is more clearly expressed as an `if let`",
    ),
    l(
        "default_overrides_default_fields",
        Deny,
        "detect `Default` impl that should use the type's default field values",
    )
    .gate("default_field_values", Some(132162)),
    l(
        "deref_into_dyn_supertrait",
        Allow,
        "`Deref` implementation with a supertrait trait object for output is shadowed by trait upcasting",
    ),
    l(
        "dropping_references",
        Warn,
        "calls to `std::mem::drop` with a reference instead of an owned value",
    ),
    l(
        "forgetting_references",
        Warn,
        "calls to `std::mem::forget` with a reference instead of an owned value",
    ),
    l("dropping_copy_types", Warn, "calls to `std::mem::drop` with a value that implements Copy"),
    l(
        "forgetting_copy_types",
        Warn,
        "calls to `std::mem::forget` with a value that implements Copy",
    ),
    l(
        "undropped_manually_drops",
        Deny,
        "calls to `std::mem::drop` with `std::mem::ManuallyDrop` instead of it's inner value",
    ),
    l("improper_ctypes", Warn, "proper use of libc types in foreign modules"),
    l("improper_ctypes_definitions", Warn, "proper use of libc types in foreign item definitions"),
    l(
        "uses_power_alignment",
        Warn,
        "Structs do not follow the power alignment rule under repr(C)",
    ),
    l("improper_gpu_kernel_arg", Warn, "GPU kernel entry points have a limited ABI"),
    l("missing_gpu_kernel_export_name", Warn, "mangled gpu-kernel function"),
    l(
        "invalid_from_utf8_unchecked",
        Deny,
        "using a non UTF-8 literal in `std::str::from_utf8_unchecked`",
    ),
    l("invalid_from_utf8", Warn, "using a non UTF-8 literal in `std::str::from_utf8`"),
    l("variant_size_differences", Allow, "detects enums with widely varying variant sizes"),
    l("path_statements", Warn, "path statements with no effect"),
    l("let_underscore_drop", Allow, "non-binding let on a type that has a destructor"),
    l("let_underscore_lock", Deny, "non-binding let on a synchronization lock"),
    l(
        "invalid_reference_casting",
        Deny,
        "casts of `&T` to `&mut T` without interior mutability",
    ),
    l(
        "dangerous_implicit_autorefs",
        Deny,
        "implicit reference to a dereference of a raw pointer",
    )
    .external(),
    l("unused_must_use", Warn, "unused result of a type flagged as `#[must_use]`").external(),
    l("unused_results", Allow, "unused result of an expression in a statement"),
    l("unit_bindings", Allow, "binding is useless because it has the unit `()` type"),
    l("non_upper_case_globals", Warn, "static constants should have uppercase identifiers"),
    l(
        "non_shorthand_field_patterns",
        Warn,
        "using `Struct { x: x }` instead of `Struct { x }` in a pattern",
    ),
    l("unused_allocation", Warn, "detects unnecessary allocations that can be eliminated"),
    l(
        "missing_copy_implementations",
        Allow,
        "detects potentially-forgotten implementations of `Copy`",
    ),
    l("useless_ptr_null_checks", Warn, "useless checking of non-null-typed pointer"),
    l("invalid_null_arguments", Deny, "invalid null pointer in arguments"),
    l(
        "mutable_transmutes",
        Deny,
        "transmuting &T to &mut T is undefined behavior, even if the reference is unused",
    ),
    l("type_alias_bounds", Warn, "bounds in type aliases are not enforced"),
    l("trivial_bounds", Warn, "these bounds don't depend on an type parameters"),
    l("unused_comparisons", Warn, "comparisons made useless by limits of the types involved"),
    l("overflowing_literals", Deny, "literal out of range for its type"),
    l("invalid_nan_comparisons", Warn, "detects invalid floating point NaN comparisons"),
    l("ambiguous_wide_pointer_comparisons", Warn, "detects ambiguous wide pointer comparisons"),
    l(
        "unpredictable_function_pointer_comparisons",
        Warn,
        "detects unpredictable function pointer comparisons",
    )
    .external(),
    l(
        "non_snake_case",
        Warn,
        "variables, methods, functions, lifetime parameters and modules should have snake case names",
    ),
    l("no_mangle_const_items", Deny, "const items will not have their symbols exported"),
    l("no_mangle_generic_items", Warn, "generic items must be mangled"),
    l("unreachable_pub", Allow, "`pub` items not reachable from crate root"),
    l("explicit_outlives_requirements", Allow, "outlives requirements can be inferred"),
    l(
        "invalid_value",
        Warn,
        "an invalid value is being created (such as a null reference)",
    ),
    l("deref_nullptr", Deny, "detects when an null pointer is dereferenced"),
    l("unstable_features", Allow, "enabling unstable features"),
    l(
        "ungated_async_fn_track_caller",
        Warn,
        "enabling track_caller on an async fn is a no-op unless the async_fn_track_caller feature is enabled",
    ),
    l("array_into_iter", Warn, "detects calling `into_iter` on arrays in Rust 2015 and 2018")
        .fcw(EditionSemanticsChange(Edition2021, "IntoIterator-for-arrays")),
    l(
        "boxed_slice_into_iter",
        Warn,
        "detects calling `into_iter` on boxed slices in Rust 2015, 2018, and 2021",
    )
    .fcw(EditionSemanticsChange(Edition2024, "intoiterator-box-slice")),
    l("drop_bounds", Warn, "bounds of the form `T: Drop` are most likely incorrect"),
    // dyn: rustc's text of the lint, no trait object
    l("dyn_drop", Warn, "trait objects of the form `dyn Drop` are useless"),
    l("dangling_pointers_from_temporaries", Warn, "detects getting a pointer from a temporary"),
    l(
        "dangling_pointers_from_locals",
        Warn,
        "detects returning a pointer from a local variable",
    ),
    l(
        "non_fmt_panics",
        Warn,
        "detect single-argument panic!() invocations in which the argument is not a format string",
    )
    .fcw(EditionSemanticsChange(Edition2021, "panic-macro-consistency"))
    .unexplained()
    .external(),
    l("noop_method_call", Warn, "detects the use of well-known noop methods"),
    l("suspicious_double_ref_op", Warn, "suspicious call of trait method on `&&T`"),
    l(
        "enum_intrinsics_non_enums",
        Deny,
        "detects calls to `core::mem::discriminant` and `core::mem::variant_count` with non-enum types",
    ),
    l(
        "invalid_atomic_ordering",
        Deny,
        "usage of invalid atomic ordering in atomic operations and memory fences",
    ),
    l("named_asm_labels", Deny, "named labels in inline assembly"),
    l("binary_asm_labels", Deny, "labels in inline assembly containing only 0 or 1 digits"),
    l(
        "opaque_hidden_inferred_bound",
        Warn,
        "detects the use of nested `impl Trait` types in associated type bounds that are not general enough",
    ),
    l(
        "multiple_supertrait_upcastable",
        Allow,
        "detect when a dyn-compatible trait has multiple supertraits",
    )
    .gate("multiple_supertrait_upcastable", Some(150833)),
    l("map_unit_fn", Warn, "`Iterator::map` call that discard the iterator's values"),
    l("missing_debug_implementations", Allow, "detects missing implementations of Debug"),
    l("missing_docs", Allow, "detects missing documentation for public members").external(),
    l(
        "closure_returning_async_block",
        Allow,
        "closure that returns `async {}` could be rewritten as an async closure",
    ),
    l(
        "async_fn_in_trait",
        Warn,
        "use of `async fn` in definition of a publicly-reachable trait",
    ),
    l("non_local_definitions", Warn, "checks for non-local definitions").external(),
    l(
        "const_item_interior_mutations",
        Warn,
        "checks for calls which mutates a interior mutable const-item",
    ),
    l(
        "invalid_runtime_symbol_definitions",
        Deny,
        "invalid definition of a symbol used by the standard library",
    ),
    l(
        "suspicious_runtime_symbol_definitions",
        Warn,
        "suspicious definition of a symbol used by the standard library",
    ),
    l(
        "impl_trait_overcaptures",
        Allow,
        "`impl Trait` will capture more lifetimes than possibly intended in edition 2024",
    )
    .fcw(EditionSemanticsChange(Edition2024, "rpit-lifetime-capture")),
    l(
        "impl_trait_redundant_captures",
        Allow,
        "redundant precise-capturing `use<...>` syntax on an `impl Trait`",
    ),
    l(
        "if_let_rescope",
        Allow,
        "`if let` assigns a shorter lifetime to temporary values being pattern-matched against in Edition 2024 and rewriting in `match` is an option to preserve the semantics up to Edition 2021",
    )
    .fcw(EditionSemanticsChange(Edition2024, "temporary-if-let-scope")),
    l("static_mut_refs", Warn, "creating a shared reference to mutable static")
        .fcw(EditionError(Edition2024, "static-mut-references"))
        .unexplained()
        .edition(Edition2024, Deny),
    l(
        "unqualified_local_imports",
        Allow,
        "`use` of a local item without leading `self::`, `super::`, or `crate::`",
    )
    .gate("unqualified_local_imports", Some(138299)),
    l("function_casts_as_integer", Warn, "casting a function into an integer"),
    l(
        "ptr_to_integer_transmute_in_consts",
        Warn,
        "detects pointer to integer transmutes in const functions and associated constants",
    ),
    l(
        "unnecessary_transmutes",
        Warn,
        "detects transmutes that can also be achieved by other operations",
    ),
    l("integer_to_ptr_transmutes", Warn, "detects integer to pointer transmutes"),
    l(
        "mismatched_lifetime_syntaxes",
        Warn,
        "detects when a lifetime uses different syntax between arguments and return values",
    ),
    l(
        "internal_eq_trait_method_impls",
        Warn,
        "manual implementation of the internal `Eq::assert_receiver_is_total_eq` method",
    )
    .fcw(FutureReleaseError(152336)),
    l("implicit_provenance_casts", Allow, "an `as` cast relying on exposed provenance is used")
        .gate("strict_provenance_lints", Some(130351)),
    l("c_void_returns", Warn, "detects use of `c_void` as a return type"),
    l(
        "raw_borrows_via_references",
        Allow,
        "creating raw borrows via references is discouraged",
    ),
    // foreign_modules
    l(
        "clashing_extern_declarations",
        Warn,
        "detects when an extern fn has been declared with the same name but different types",
    ),
    // rustc_lint_defs::builtin::hardwired
    l(
        "aarch64_softfloat_neon",
        Warn,
        "detects code that could be affected by ABI issues on aarch64 softfloat targets",
    )
    .fcw_deps(FutureReleaseError(134375)),
    l(
        "absolute_paths_not_starting_with_crate",
        Allow,
        "fully qualified paths that start with a module name instead of `crate`, `self`, or an extern crate name",
    )
    .fcw(EditionError(Edition2018, "path-changes")),
    l("ambiguous_associated_items", Deny, "ambiguous associated items")
        .fcw(FutureReleaseError(57644)),
    l(
        "ambiguous_derive_helpers",
        Warn,
        "detects derive helper attributes that are ambiguous with built-in attributes",
    )
    .fcw(FutureReleaseError(151276)),
    l(
        "ambiguous_glob_imported_traits",
        Warn,
        "detects uses of ambiguously glob imported traits",
    )
    .fcw(FutureReleaseError(152822)),
    l(
        "ambiguous_glob_imports",
        Deny,
        "detects certain glob imports that require reporting an ambiguity error",
    )
    .fcw_deps(FutureReleaseError(114095)),
    l("ambiguous_glob_reexports", Warn, "ambiguous glob re-exports"),
    l(
        "ambiguous_import_visibilities",
        Warn,
        "detects certain glob imports that require reporting an ambiguity error",
    )
    .fcw(FutureReleaseError(149145)),
    l("ambiguous_panic_imports", Warn, "detects ambiguous core and std panic imports")
        .fcw(FutureReleaseError(147319)),
    l("arithmetic_overflow", Deny, "arithmetic operation overflows").eval_always(),
    l("asm_sub_register", Warn, "using only a subset of a register for inline asm inputs"),
    l("bad_asm_style", Warn, "incorrect use of inline assembly"),
    // dyn: rustc's text of the lint, no trait object
    l("bare_trait_objects", Warn, "suggest using `dyn Trait` for trait objects")
        .fcw(EditionError(Edition2021, "warnings-promoted-to-error")),
    l(
        "bindings_with_variant_name",
        Deny,
        "detects pattern bindings with the same name as one of the matched variants",
    ),
    l(
        "break_with_label_and_loop",
        Warn,
        "`break` expression with label and unlabeled loop as value expression",
    ),
    l(
        "coherence_leak_check",
        Warn,
        "distinct impls distinguished only by the leak-check code",
    )
    .fcw(Custom("the behavior may change in a future release", 56105)),
    l(
        "conflicting_repr_hints",
        Deny,
        "conflicts between `#[repr(..)]` hints that were previously accepted and used in practice",
    )
    .fcw_deps(FutureReleaseError(68585)),
    l(
        "const_evaluatable_unchecked",
        Warn,
        "detects a generic constant is used in a type without a emitting a warning",
    )
    .fcw(FutureReleaseError(76200)),
    l("const_item_mutation", Warn, "detects attempts to mutate a `const` item"),
    l("dead_code", Warn, "detect unused, unexported items"),
    l(
        "dead_code_pub_in_binary",
        Allow,
        "detect public items in executable crates that are never used",
    )
    .crate_level(),
    l(
        "dependency_on_unit_never_type_fallback",
        Deny,
        "never type fallback affecting unsafe function calls",
    )
    .fcw_deps(EditionAndFutureReleaseError(Edition2024, "never-type-fallback"))
    .external(),
    l("deprecated", Warn, "detects use of deprecated items").external(),
    l(
        "deprecated_in_future",
        Allow,
        "detects use of items that will be deprecated in a future version",
    )
    .external(),
    l("deprecated_llvm_intrinsic", Allow, "detects uses of deprecated LLVM intrinsics")
        .gate("link_llvm_intrinsics", Some(29602)),
    l("deprecated_safe_2024", Allow, "detects unsafe functions being used as safe functions")
        .fcw(EditionError(Edition2024, "newly-unsafe-functions")),
    l("deprecated_where_clause_location", Warn, "deprecated where clause location"),
    l(
        "duplicate_features",
        Deny,
        "duplicate features found in crate-level `#[feature]` directives",
    ),
    l("duplicate_macro_attributes", Warn, "duplicated attribute"),
    l("elided_lifetimes_in_paths", Allow, "hidden lifetime parameters in types are deprecated"),
    l("explicit_builtin_cfgs_in_flags", Deny, "detects builtin cfgs set via the `--cfg`"),
    l(
        "exported_private_dependencies",
        Warn,
        "public interface leaks type from a private dependency",
    ),
    l(
        "ffi_unwind_calls",
        Allow,
        "call to foreign functions or function pointers with FFI-unwind ABI",
    ),
    l(
        "float_literal_f32_fallback",
        Warn,
        "detects unsuffixed floating point literals whose type fallback to `f32`",
    )
    .fcw(FutureReleaseError(154024)),
    l("forbidden_lint_groups", Warn, "applying forbid to lint-groups")
        .fcw_deps(FutureReleaseError(81670))
        .ignore_deny_warnings(),
    l(
        "function_item_references",
        Warn,
        "suggest casting to a function pointer when attempting to take references to function items",
    ),
    l(
        "hidden_glob_reexports",
        Warn,
        "name introduced by a private item shadows a name introduced by a public glob re-export",
    ),
    l(
        "ill_formed_attribute_input",
        Deny,
        "ill-formed attribute inputs that were previously accepted and used in practice",
    )
    .fcw_deps(FutureReleaseError(57571))
    .crate_level(),
    l("incomplete_include", Deny, "trailing content in included file"),
    l(
        "ineffective_unstable_trait_impl",
        Deny,
        "detects `#[unstable]` on stable trait implementations for stable types",
    ),
    l(
        "inline_no_sanitize",
        Warn,
        r#"detects incompatible use of `#[inline(always)]` and `#[sanitize(... = "off")]`"#,
    ),
    l("invalid_doc_attributes", Warn, "detects invalid `#[doc(...)]` attributes"),
    l(
        "invalid_macro_export_arguments",
        Deny,
        "\"invalid_parameter\" isn't a valid argument for `#[macro_export]`",
    )
    .fcw_deps(FutureReleaseError(57571)),
    l(
        "invalid_type_param_default",
        Deny,
        "type parameter default erroneously allowed in invalid location",
    )
    .fcw_deps(FutureReleaseError(36887)),
    l(
        "irrefutable_let_patterns",
        Warn,
        "detects irrefutable patterns in `if let` and `while let` statements",
    ),
    l("large_assignments", Warn, "detects large moves or copies"),
    l(
        "late_bound_lifetime_arguments",
        Warn,
        "detects generic lifetime arguments in path segments with late bound lifetime parameters",
    )
    .fcw(FutureReleaseError(42868)),
    l(
        "legacy_derive_helpers",
        Deny,
        "detects derive helper attributes that are used before they are introduced",
    )
    .fcw_deps(FutureReleaseError(79202)),
    l(
        "linker_info",
        Allow,
        "linker warnings known to be informational-only and not indicative of a problem",
    ),
    l(
        "linker_messages",
        Warn,
        "warnings emitted at runtime by the target-specific linker program",
    )
    .ignore_deny_warnings(),
    l("long_running_const_eval", Deny, "detects long const eval operations").external(),
    l(
        "macro_expanded_macro_exports_accessed_by_absolute_paths",
        Deny,
        "macro-expanded `macro_export` macros from the current crate cannot be referred to by absolute paths",
    )
    .fcw_deps(FutureReleaseError(52234))
    .crate_level(),
    l(
        "macro_use_extern_crate",
        Allow,
        "the `#[macro_use]` attribute is now deprecated in favor of using macros via the module system",
    ),
    l("malformed_diagnostic_attributes", Warn, "detects malformed diagnostic attributes"),
    l(
        "malformed_diagnostic_filters",
        Warn,
        "detects malformed filters in diagnostic attributes",
    )
    .gate("rustc_attrs", None),
    l(
        "malformed_diagnostic_format_literals",
        Warn,
        "detects diagnostic attribute with malformed diagnostic format literals",
    ),
    l("meta_variable_misuse", Allow, "possible meta-variable misuse at macro definition"),
    l(
        "method_call_on_diverging_infer_var",
        Warn,
        "detects method calls on a result of never-to-any coercion",
    )
    .fcw_deps(FutureReleaseError(156047)),
    l(
        "misplaced_diagnostic_attributes",
        Warn,
        "detects diagnostic attributes that are placed on the wrong item",
    ),
    l("missing_abi", Warn, "No declared ABI for extern declaration"),
    l(
        "missing_unsafe_on_extern",
        Allow,
        "detects missing unsafe keyword on extern declarations",
    )
    .fcw(EditionError(Edition2024, "unsafe-extern")),
    l("must_not_suspend", Allow, "use of a `#[must_not_suspend]` value across a yield point")
        .gate("must_not_suspend", Some(83310)),
    l(
        "named_arguments_used_positionally",
        Warn,
        "named arguments in format used positionally",
    ),
    l(
        "never_type_fallback_flowing_into_unsafe",
        Deny,
        "never type fallback affecting unsafe function calls",
    )
    .fcw_deps(EditionAndFutureReleaseSemanticsChange(Edition2024, "never-type-fallback"))
    .edition(Edition2024, Deny)
    .external(),
    l(
        "non_contiguous_range_endpoints",
        Warn,
        "detects off-by-one errors with exclusive range patterns",
    ),
    l(
        "non_exhaustive_omitted_patterns",
        Allow,
        "detect when patterns of types marked `non_exhaustive` are missed",
    )
    .gate("non_exhaustive_omitted_patterns_lint", Some(89554)),
    l(
        "out_of_scope_macro_calls",
        Deny,
        "detects out of scope calls to `macro_rules` in key-value attributes",
    )
    .fcw_deps(FutureReleaseError(124535)),
    l(
        "overlapping_range_endpoints",
        Warn,
        "detects range patterns with overlapping endpoints",
    ),
    l(
        "patterns_in_fns_without_body",
        Deny,
        "patterns in functions without body were erroneously allowed",
    )
    .fcw(FutureReleaseError(35203)),
    l("private_bounds", Warn, "private type in secondary interface of an item"),
    l("private_interfaces", Warn, "private type in primary interface of an item"),
    l(
        "proc_macro_derive_resolution_fallback",
        Deny,
        "detects proc macro derives using inaccessible names from parent modules",
    )
    .fcw_deps(FutureReleaseError(83583)),
    l(
        "pub_use_of_private_extern_crate",
        Deny,
        "detect public re-exports of private extern crates",
    )
    .fcw_deps(FutureReleaseError(127909)),
    l(
        "recursion_depth_exceeding_limit",
        Warn,
        "detects trait solving overflow that only happens with the next solver",
    )
    .fcw(FutureReleaseError(159228)),
    l(
        "redundant_imports",
        Allow,
        "imports that are redundant due to being imported already",
    ),
    l(
        "redundant_lifetimes",
        Allow,
        "detects lifetime parameters that are redundant because they are equal to some other named lifetime",
    ),
    l(
        "refining_impl_trait_internal",
        Warn,
        "impl trait in impl method signature does not match trait method signature",
    ),
    l(
        "refining_impl_trait_reachable",
        Warn,
        "impl trait in impl method signature does not match trait method signature",
    ),
    l("renamed_and_removed_lints", Warn, "lints that have been renamed or removed"),
    l(
        "repr_c_enums_larger_than_int",
        Warn,
        "repr(C) enums with discriminant values that do not fit into a C int",
    )
    .fcw(FutureReleaseError(124403)),
    l(
        "resolving_to_items_shadowing_supertrait_items",
        Allow,
        "detects when a supertrait item is shadowed by a subtrait item",
    )
    .gate("supertrait_item_shadowing", Some(89151)),
    l(
        "rtsan_nonblocking_async",
        Warn,
        r#"detects incompatible uses of `#[sanitize(realtime = "nonblocking")]` on async functions"#,
    ),
    l(
        "rust_2021_incompatible_closure_captures",
        Allow,
        "detects closures affected by Rust 2021 changes",
    )
    .fcw(EditionSemanticsChange(Edition2021, "disjoint-capture-in-closures"))
    .unexplained(),
    l(
        "rust_2021_incompatible_or_patterns",
        Allow,
        "detects usage of old versions of or-patterns",
    )
    .fcw(EditionError(Edition2021, "or-patterns-macro-rules")),
    l(
        "rust_2021_prefixes_incompatible_syntax",
        Allow,
        "identifiers that will be parsed as a prefix in Rust 2021",
    )
    .fcw(EditionError(Edition2021, "reserving-syntax"))
    .crate_level(),
    l(
        "rust_2021_prelude_collisions",
        Allow,
        "detects the usage of trait methods which are ambiguous with traits added to the prelude in future editions",
    )
    .fcw(EditionError(Edition2021, "prelude")),
    l(
        "rust_2024_guarded_string_incompatible_syntax",
        Allow,
        "will be parsed as a guarded string in Rust 2024",
    )
    .fcw(EditionError(Edition2024, "reserved-syntax"))
    .crate_level(),
    l(
        "rust_2024_incompatible_pat",
        Allow,
        "detects patterns whose meaning will change in Rust 2024",
    )
    .fcw(EditionSemanticsChange(Edition2024, "match-ergonomics")),
    l(
        "rust_2024_prelude_collisions",
        Allow,
        "detects the usage of trait methods which are ambiguous with traits added to the prelude in future editions",
    )
    .fcw(EditionError(Edition2024, "prelude")),
    l(
        "self_constructor_from_outer_item",
        Warn,
        "detect unsupported use of `Self` from outer item",
    )
    .fcw(FutureReleaseError(124186)),
    l(
        "semicolon_in_expressions_from_macros",
        Deny,
        "trailing semicolon in macro body used as expression",
    )
    .fcw_deps(FutureReleaseError(79813)),
    l(
        "semicolon_in_expressions_from_non_local_macros",
        Warn,
        "trailing semicolon in macro body used as expression",
    )
    .fcw_deps(FutureReleaseError(79813)),
    l(
        "shadowing_supertrait_items",
        Allow,
        "detects when a supertrait item is shadowed by a subtrait item",
    )
    .gate("supertrait_item_shadowing", Some(89151)),
    l("single_use_lifetimes", Allow, "detects lifetime parameters that are only used once"),
    l("stable_features", Warn, "stable features found in `#[feature]` directive"),
    l(
        "tail_call_track_caller",
        Warn,
        "detects tail calls of functions marked with `#[track_caller]`",
    )
    .gate("explicit_tail_calls", Some(112788)),
    l(
        "tail_expr_drop_order",
        Allow,
        "Detect and warn on significant change in drop order in tail expression location",
    )
    .fcw(EditionSemanticsChange(Edition2024, "temporary-tail-expr-scope")),
    l("test_unstable_lint", Deny, "this unstable lint is only for testing")
        .gate("test_unstable_lint", None),
    l(
        "text_direction_codepoint_in_comment",
        Deny,
        "invisible directionality-changing codepoints in comment",
    )
    .crate_level(),
    l(
        "text_direction_codepoint_in_literal",
        Deny,
        "detect special Unicode codepoints that affect the visual representation of text on screen, changing the direction in which text flows",
    )
    .crate_level(),
    l("trivial_casts", Allow, "detects trivial casts which could be removed"),
    l(
        "trivial_numeric_casts",
        Allow,
        "detects trivial casts of numeric types which could be removed",
    ),
    l("tyvar_behind_raw_pointer", Warn, "raw pointer to an inference variable")
        .fcw(EditionError(Edition2018, "tyvar-behind-raw-pointer")),
    l("unconditional_panic", Deny, "operation will cause a panic at runtime").eval_always(),
    l(
        "unconditional_recursion",
        Warn,
        "functions that cannot return without calling themselves",
    ),
    l(
        "uncovered_param_in_projection",
        Warn,
        "impl contains type parameters that are not covered",
    )
    .fcw(FutureReleaseError(124559)),
    l(
        "unexpected_cfgs",
        Warn,
        "detects unexpected names and values in `#[cfg]` conditions",
    )
    .external(),
    l("unfulfilled_lint_expectations", Warn, "unfulfilled lint expectation"),
    l("uninhabited_static", Deny, "uninhabited static").fcw_deps(FutureReleaseError(74840)),
    l(
        "unknown_crate_types",
        Deny,
        "unknown crate type found in `#[crate_type]` directive",
    )
    .crate_level(),
    l("unknown_diagnostic_attributes", Warn, "detects unknown diagnostic attributes"),
    l("unknown_lints", Warn, "unrecognized lint attribute").eval_always(),
    l(
        "unnameable_test_items",
        Warn,
        "detects an item that cannot be named being marked as `#[test_case]`",
    )
    .external(),
    l(
        "unnameable_types",
        Allow,
        "effective visibility of a type is larger than the area in which it can be named",
    ),
    l(
        "unreachable_cfg_select_predicates",
        Warn,
        "detects unreachable configuration predicates in the cfg_select macro",
    ),
    l("unreachable_code", Warn, "detects unreachable code paths").external(),
    l("unreachable_patterns", Warn, "detects unreachable patterns"),
    l("unsafe_attr_outside_unsafe", Allow, "detects unsafe attributes outside of unsafe")
        .fcw(EditionError(Edition2024, "unsafe-attributes")),
    l(
        "unsafe_op_in_unsafe_fn",
        Allow,
        "unsafe operations in unsafe functions without an explicit unsafe block are deprecated",
    )
    .fcw(EditionError(Edition2024, "unsafe-op-in-unsafe-fn"))
    .unexplained()
    .edition(Edition2024, Warn),
    l(
        "unstable_name_collisions",
        Warn,
        "detects name collision with an existing but unstable method",
    )
    .fcw(Custom(
        "once this associated item is added to the standard library, the ambiguity may cause an error or change in behavior!",
        48919,
    )),
    l(
        "unstable_syntax_pre_expansion",
        Warn,
        "unstable syntax can change at any point in the future, causing a hard error!",
    )
    .fcw(FutureReleaseError(154045)),
    l("unsupported_calling_conventions", Warn, "use of unsupported calling convention")
        .fcw(FutureReleaseError(137018)),
    l("unused_assignments", Warn, "detect assignments that will never be read"),
    l(
        "unused_associated_type_bounds",
        Warn,
        // dyn: rustc's text of the lint, no trait object
        "detects unused `Foo = Bar` bounds in `dyn Trait<Foo = Bar>`",
    ),
    l("unused_attributes", Warn, "detects attributes that were not used by the compiler"),
    l("unused_crate_dependencies", Allow, "crate dependencies that are never used").crate_level(),
    l("unused_extern_crates", Allow, "extern crates that are never used"),
    l(
        "unused_features",
        Warn,
        "unused features found in crate-level `#[feature]` directives",
    ),
    l("unused_imports", Warn, "imports that are never used"),
    l("unused_labels", Warn, "detects labels that are never used"),
    l("unused_lifetimes", Allow, "detects lifetime parameters that are never used"),
    l("unused_macros", Warn, "detects macros that were not used"),
    l("unused_macro_rules", Allow, "detects macro rules that were not used"),
    l("unused_mut", Warn, "detect mut variables which don't need to be mutable"),
    l("unused_qualifications", Allow, "detects unnecessarily qualified names"),
    l("unused_unsafe", Warn, "unnecessary use of an `unsafe` block"),
    l("unused_variables", Warn, "detect variables which are not used in any way"),
    l("unused_visibilities", Warn, "detect visibility qualifiers on `const _` items"),
    l("useless_deprecated", Deny, "detects deprecation attributes with no effect"),
    l(
        "varargs_without_pattern",
        Deny,
        "detects usage of `...` arguments without a pattern in non-foreign items",
    )
    .fcw_deps(FutureReleaseError(145544)),
    l("warnings", Warn, "mass-change the level for lints which produce warnings"),
];

/// The lints of the tool `blaze`, blaze's own, always registered: rustc has none of them. The
/// front end's (docs/fn-cache.md), then blaze-lints' in the order of its `blaze::Lint::ALL`,
/// which its tests hold to this table; those run under `blaze check` and are allowed unless a
/// level is set (`[workspace.metadata.blaze.lints]`).
pub(super) const BLAZE: &[Lint] = &[
    l(
        "blaze::const_impure",
        Warn,
        "detects a `#[blaze::cache]` function whose result depends on more than its arguments",
    ),
    l("blaze::hot_clone", Allow, "detects a clone in hot code that costs more than a copy")
        .externally_loaded(),
    l("blaze::hot_alloc", Allow, "detects a call in hot code that reaches the global allocator")
        .externally_loaded(),
    l("blaze::hot_dyn", Allow, "detects a call in hot code that goes through a vtable")
        .externally_loaded(),
    l("blaze::std_hashmap", Allow, "detects std's `HashMap` or `HashSet` hashing with `RandomState`")
        .externally_loaded(),
    l("blaze::parallelizable", Allow, "detects a loop whose iterations are independent and could run in parallel")
        .externally_loaded(),
    l("blaze::serial_dependence", Allow, "detects a loop kept serial by exactly one dependence")
        .externally_loaded(),
    l("blaze::fiber_boundary", Allow, "detects code that reaches past `blaze_graph::sched` into fibers")
        .externally_loaded(),
    l("blaze::hot_alloc_call", Allow, "detects a call in hot code that allocates through the workspace's code")
        .externally_loaded(),
    l("blaze::error_debug", Allow, "detects an error's message that can panic or names no item")
        .externally_loaded(),
    l("blaze::wall_clock", Allow, "detects a read of the wall clock outside the injected clock")
        .externally_loaded(),
    l("blaze::untyped_error", Allow, "detects a function returning an error that is not an enum of what fails")
        .externally_loaded(),
    l("blaze::dropped_error", Allow, "detects an error thrown away for a value")
        .externally_loaded(),
    l("blaze::hash_order_output", Allow, "detects output that depends on hash-map iteration order")
        .externally_loaded(),
    l("blaze::held_across_wait", Allow, "detects a thread-tied value held across a wait")
        .externally_loaded(),
    l("blaze::lock_across_query", Allow, "detects a lock guard held across a query call")
        .externally_loaded(),
    l("blaze::blocking_on_pool", Allow, "detects a blocking call on a pool thread or fiber")
        .externally_loaded(),
    l("blaze::unnamed_wait", Allow, "detects a wait with no span naming what it waits for")
        .externally_loaded(),
    l("blaze::shared_collection_lock", Allow, "detects a locked map or `Vec` shared between parallel items")
        .externally_loaded(),
    l("blaze::open_wait_loop", Allow, "detects a loop that waits for progress with no bound")
        .externally_loaded(),
];

/// Lints rustc declares and never registers: an attribute can't name them, but
/// `private_macro_use` is still emitted by name resolution at its default level.
pub(super) const UNREGISTERED: &[Lint] = &[
    l(
        "private_macro_use",
        Deny,
        "detects certain macro bindings that should not be re-exported",
    )
    .fcw_deps(FutureReleaseError(120192)),
    l(
        "unsupported_fn_ptr_calling_conventions",
        Warn,
        "use of unsupported calling convention for function pointer",
    )
    .fcw_deps(FutureReleaseError(130260)),
];

/// `register_internals`: the internal early pass's, then the internal late pass's.
pub(super) const INTERNAL: &[Lint] = &[
    l(
        "rustc::lint_pass_impl_without_macro",
        Allow,
        "`impl LintPass` without the `declare_lint_pass!` or `impl_lint_pass!` macros",
    ),
    l(
        "rustc::implicit_sysroot_crate_import",
        Allow,
        "Forbid uses of non-sysroot crates in `extern crate`",
    )
    .external(),
    l(
        "rustc::bad_use_of_find_attr",
        Allow,
        "Forbid `AttributeKind::` as a prefix in `find_attr!` macros.",
    )
    .external(),
    l(
        "rustc::default_hash_types",
        Allow,
        "forbid HashMap and HashSet and suggest the FxHash* variants",
    )
    .external(),
    l(
        "rustc::potential_query_instability",
        Allow,
        "require explicit opt-in when using potentially unstable methods or functions",
    )
    .external(),
    l(
        "rustc::untracked_query_information",
        Allow,
        "require explicit opt-in when accessing information not tracked by the query system",
    )
    .external(),
    l("rustc::usage_of_ty_tykind", Allow, "usage of `ty::TyKind` outside of the `ty::sty` module")
        .external(),
    l("rustc::usage_of_qualified_ty", Allow, "using `ty::{Ty,TyCtxt}` instead of importing it")
        .external(),
    l(
        "rustc::direct_use_of_rustc_type_ir",
        Allow,
        "usage `rustc_type_ir` abstraction outside of trait system",
    )
    .external(),
    l(
        "rustc::non_glob_import_of_type_ir_inherent",
        Allow,
        "non-glob import of `rustc_type_ir::inherent`",
    )
    .external(),
    l(
        "rustc::usage_of_type_ir_inherent",
        Allow,
        "usage `rustc_type_ir::inherent` outside of trait system",
    )
    .external(),
    l(
        "rustc::usage_of_type_ir_traits",
        Allow,
        "usage `rustc_type_ir`-specific abstraction traits outside of trait system",
    )
    .external(),
    l(
        "rustc::bad_opt_access",
        Deny,
        "prevent using options by field access when there is a wrapper function",
    )
    .external(),
    l(
        "rustc::disallowed_pass_by_ref",
        Warn,
        "pass by reference of a type flagged as `#[rustc_pass_by_value]`",
    )
    .external(),
    l(
        "rustc::span_use_eq_ctxt",
        Allow,
        "forbid uses of `==` with `Span::ctxt`, suggest `Span::eq_ctxt` instead",
    )
    .external(),
    l(
        "rustc::symbol_intern_string_literal",
        Allow,
        "Forbid uses of string literals in `Symbol::intern`, suggesting preinterning instead",
    )
    .external(),
    l(
        "rustc::rustc_must_match_exhaustively",
        Allow,
        "Forbids matches with wildcards, or if-let matching on enums marked with `#[rustc_must_match_exhaustively]`",
    )
    .external(),
];
