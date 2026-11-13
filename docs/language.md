---
status: record
summary: Each language change of Rust 1.80 to 1.100 checked against blaze's front end on 2026-11-12, with the test that holds it and what was fixed or is still missing.
date: 2026-11-12
---
# The Rust language, release by release

What `superset-of-rust` asks of the front end for stable Rust: each entry of the "Language"
sections (and the language entries of "Compatibility Notes") of rustc's
[RELEASES.md](https://github.com/rust-lang/rust/blob/master/RELEASES.md) from 1.80.0 to 1.100.0
(released 2026-11-12), and `compiler/rustc_feature/src/accepted.rs` at the pin
(`library/UPSTREAM`: rustc 1.101.0-nightly 16d1ffac1, which has all of 1.100). Checked on
136432aa02 plus the branch `rust-1100-lang`. No rustc was run: the references are rustc's source
and UI tests at the pinned commit, and the Reference.

The tests:

- **D**: `crates/blaze-diagnostics/src/tests/stabilized.rs`, the program checked as `blaze build`
  checks it, with nothing said (or rustc's own `.stderr` at the pin where a release added a
  diagnostic). `D:v1_99_c_variadic_definitions` is the test of that name there.
- **R**: `crates/hir-ty-mir/src/tests/mir/stabilized.rs`, the program run on the MIR evaluator at
  every optimization level, checking itself (drop order, layout, const evaluation, coercions).
- another path is an existing test.

Status is what blaze did before this branch: **ok** (accepted and, where it matters, run),
**fixed** (wrong or missing, fixed here), **more** (blaze accepts what rustc refuses or warns
about: allowed by `superset-of-rust`, listed for objective C6), **open** (still missing),
**n/a** (documentation, a guarantee with nothing to accept, or a target blaze does not build
for), **not checked**.

## 1.100

| change | status | evidence |
|---|---|---|
| never type `!` stable (#155499): any type position, impls for `!` | ok | D:v1_100_the_never_type_is_a_type_anywhere, R:v1_100_never_type_layout_patterns_and_fallback |
| never type fallback is `!` in every edition | ok | hir-ty `infer/fallback.rs` `calculate_diverging_fallback`; D:v1_100_the_never_fallback_is_never_in_every_edition; R (2021: `T::tag()` is `!`'s) |
| `Infallible` is an alias of `!` | ok | `library/core/src/convert/mod.rs:915`; D (an alias of `!` in a fixture). minicore's `Infallible` is still an enum: open, it is read by tests of nine crates |
| `dependency_on_unit_never_type_fallback` removed, `never_type_fallback_flowing_into_unsafe` no longer an edition lint | fixed | blaze-lint-defs; D:v1_100_the_unit_fallback_lint_is_removed |
| `extern "custom"` (#158504) | ok | D:v1_100_extern_custom; codegen: `hir-ty-codegen/mod.rs:4537` |
| `aligned_fields_in_packed`, deny by default, where E0588 was (#162160) | fixed | blaze had neither the error nor the lint, and the lint's name was unknown. `checks/defs/packed.rs`; D:v1_100_an_aligned_type_in_a_packed_one_is_a_lint (rustc's test, byte for byte); R: the packed layout |
| `repeated_reprs` lint (#157036) | fixed (name), open (the lint is not given) | blaze-lint-defs; D: lint names |
| `dropping_references`, `dropping_copy_types`, `undropped_manually_drops` on `drop_in_place` (#160229) | open | the registry's text is the pin's; the lints do not look at `drop_in_place` |
| `#[expect(dead_code)]` under a dead parent is fulfilled (#161005) | fixed | `lints/names/dead/report.rs`; `tests/lints/names/dead/expect_inside_dead_code.rs` (rustc's test) |
| volatile accesses may trap (#160564) | n/a | a guarantee |
| closures capture what `if let` moves (#154210) | ok | R:v1_100_if_let_in_a_closure_captures_only_the_moved_field |
| `let ... else` takes an `expr` metavariable as grouped (#158515) | ok | D:v1_100_let_else_takes_an_expr_metavariable_as_grouped (rustc's test) |
| `box` patterns are no syntax (#156749) | more | `#![feature(box_patterns)]` is E0557 (removed), but a `box` pattern still parses and rustc's error for it is not given: `a_box_pattern_is_removed_syntax` (rustc's test) fails on main |
| dyn-compatibility checked in more places (#154992), layout cycles of enums (#157562), `ambiguous_glob_imported_traits` in more cases (#161805) | not checked | each refuses or warns about more |
| nine lints of the pin unknown by name | fixed | blaze-lint-defs; D:v1_100_the_lints_of_1_80_to_1_100_are_known_by_name |

## 1.99

| change | status | evidence |
|---|---|---|
| C-variadic function definitions (#155697) | ok | hir-ty `infer.rs:3092`, hir-ty-mir `mir/lower.rs:5834`, codegen `hir-ty-codegen/mod.rs:5755`; D:v1_99_c_variadic_definitions. Not run: the MIR evaluator has no `va_arg` |
| naked C-variadic functions of any variadic ABI (#159746) | fixed | E0658 before; `checks/ast_validation/fns.rs`; D |
| a method on a diverging variable is `!`'s, with `method_call_on_diverging_infer_var` (#156047) | fixed | E0282 before; hir-ty `method_resolution/probe.rs`; D:v1_99_a_method_on_a_diverging_variable_is_nevers (rustc's two tests, byte for byte); hir-ty `tests/probe_errors.rs` with its control; R |
| a variable `Sized` through subtyping coerces to a trait object (#157820) | ok | hir-ty `infer/unify.rs:206`; D |
| `#[my_macro] mod foo;` (#157857) | ok | D:v1_99_an_attribute_macro_on_an_outlined_module |
| `overflowing_literals` under repeated negation (#158302) | fixed | `lints/bodies/types/limits.rs`; `tests/lints/bodies/types/limits/ui/lint-type-overflow3.rs` (rustc's test) |
| anonymous lifetimes of associated consts are `'static` (#156508); `u128` in `xmm` registers of `asm!` (#159525) | ok | D:v1_99_anonymous_lifetimes_of_associated_consts_and_u128_in_xmm |
| `no_mangle_generic_items` is an error (#154585) | fixed | a lint before; `lints/names/items/mangle.rs`; `generic-no-mangle` (rustc's test at the pin) |
| `raw_borrows_via_references`, POSIX runtime symbols, `unreachable_cfg_select_predicates` in `unused`, `semicolon_in_expressions_from_non_local_macros` | ok (names and groups) | D: lint names; the registry |
| `unconditional_panic` on `chunks(0)` (#153563), unused `#[path]` (#158835), `doc` on a macro call (#161003), `invalid_reference_casting` (#159960) | more / not checked | lints; `#[path]` on an inline module is not warned about |
| `global_asm!` and global target features (#160594), `UnsafeCell` and allocation guarantees | n/a | |
| compatibility: references to extern statics not promoted (#157641), `let` pattern types (#157841), generics in const-eval types (#159504), `repr(simd)` on macro calls (#158523) | not checked | each refuses more |

## 1.96 to 1.98

| change | status | evidence |
|---|---|---|
| 1.98 unsizing `&mut` may shorten its lifetime, also in an invariant position (#149219) | ok | `library/core/src/ops/unsize.rs:42`; minicore's impl brought to it; D:v1_98 |
| 1.98 `invalid_runtime_symbol_definitions`, `suspicious_runtime_symbol_definitions`, `c_void_returns` | ok (names) | D: lint names |
| 1.97 trailing `self` in imports (#155137); `cfg(target_has_atomic_equal_alignment)` (#155006) | ok | D:v1_97 |
| 1.97 `Result<T, Uninhabited>` is `T` to `unused_must_use` (#148214), `dead_code_pub_in_binary` (#149509) | ok (names) / not checked | lints |
| 1.97 LoongArch target features (#154510) | n/a | no such target |
| 1.96 an `expr` metavariable in `cfg` (#146961); `!` coerces in tuples (#147834) | ok | D:v1_96, R:v1_96_never_coerces_in_a_tuple |
| 1.96 `ManuallyDrop` constants as patterns (#154891) | ok | the library's fix; the rule: D:v1_96 and hir-ty-matchck `a_constant_holding_a_pattern_type_is_a_pattern` |
| 1.96 inference guidance of arguments (#150316); s390x vector registers (#154184) | not checked / n/a | |

## 1.91 to 1.95

| change | status | evidence |
|---|---|---|
| 1.95 `if let` guards (#141295) | ok | D:v1_95, R:v1_95_if_let_guards (the guard's temporaries dropped when the arm is left) |
| 1.95 keywords imported under a name (#146972); no `irrefutable_let_patterns` in chains (#146832) | ok | D:v1_95 |
| 1.95 `cfg_select!` | ok | hir-expand `builtin/fn_macro.rs:675`; hir-def `macro_expansion_tests/builtin_fn_macro.rs` `cfg_select` |
| 1.95 const blocks and promotion (#150557), padding in typed copies (#148967), pattern matching independent of the crate (#150681) | not checked | |
| 1.95 PowerPC `asm!`; 1.94 RISC-V features; 1.93 s390x features; 1.91 LoongArch32 `asm!` | n/a | no such target |
| 1.94 Unicode 17 identifiers (#148321) | ok | `unicode-ident = "=1.0.24"` (17.0.0), rustc's own version at the pin; D:v1_94 |
| 1.94 `dead_code` levels of impls (#144113), `unused_visibilities` (#147136), closure lifetime errors (#148329) | ok (names) / not checked | |
| 1.93 `system` variadic declarations (#145954), `asm_cfg` (#147736), consts with `&mut` to a static (#148746) | ok | D:v1_93 |
| 1.93 LUB of function items of differing safety (#148602); pointers copied byte by byte in const (#148259) | ok | D:v1_93, R:v1_93 |
| 1.93 keywords as `cfg` predicates are errors (#146978); `const_item_interior_mutations`, `function_casts_as_integer` | not checked / ok (names) | |
| 1.92 `&raw` of a union field is safe (#141469), several bounds on one associated item (#146593), `track_caller` with `no_mangle` (#145724), item bounds before where-bounds (#144064) | ok | D:v1_92, R:v1_92 |
| 1.92 `[X; 0]` makes no `X` (#145277) | ok | R:v1_92_zero_length_repeats_and_raw_borrows_of_union_fields |
| 1.92 never-type lints deny by default (#146167), coherence and higher-ranked regions (#146725), `unused_must_use` and `Result<(), !>` (#147382) | not checked | |
| 1.91 bindings lowered in written order, dropped by the first alternative's (#143764) | ok | R:v1_91_bindings_drop_in_the_order_of_the_first_alternative |
| 1.91 variadic declarations for `sysv64`, `win64`, `efiapi`, `aapcs` (#144066); `target_env = "macabi"`, `"sim"` (#139451); `sse4a`, `tbm` | ok | D:v1_89_to_1_91 |
| 1.91 `dangling_pointers_from_locals`, `integer_to_ptr_transmutes`, `semicolon_in_expressions_from_macros` deny | ok (names) | |

## 1.85 to 1.90

| change | status | evidence |
|---|---|---|
| 1.90 constants referring to mutable or external memory (#140942); diagnostic-attribute lints split (#140717) | ok | D:v1_89_to_1_91; D: lint names |
| 1.90 volatile access to non-Rust memory (#141260) | n/a | |
| 1.89 inferred const arguments `_` (#141610), `repr(u128)`/`repr(i128)` (#138285), `#![doc(test(attr(..)))]` anywhere (#140560), temporaries extended through constructors (#140593), avx512, `kl`, `widekl`, `sha512`, `sm3`, `sm4` | ok | D:v1_89_to_1_91, R:v1_89 |
| 1.89 `mismatched_lifetime_syntaxes`, `dangerous_implicit_autorefs` deny, `i128` in `improper_ctypes`, fn-pointer comparisons in external macros | ok (names) / not checked | |
| 1.89 the wasm32 C ABI | not checked | codegen |
| 1.88 `let` chains in 2024 (#132833) | ok; more | D:v1_88, R:v1_88_let_chains. blaze takes them in every edition, rustc only from 2024 |
| 1.88 naked functions (#134213), `cfg(true)`/`cfg(false)` (#138632) | ok | D:v1_88 |
| 1.88 `#[bench]` an error, impl candidate preference (#138176), const parameter default types (#139646) | not checked | |
| 1.87 `asm_goto`, `..EXPR` after unary operators, `Self: Sized` methods left out of unsized impls, `use<..>` in trait methods | ok | D:v1_86_and_1_87, R:v1_86_and_1_87 |
| 1.86 trait upcasting (#134367), safe `#[target_feature]` functions (#134090) | ok | D, R:v1_86_and_1_87_upcasting_and_sized_methods_left_out |
| 1.86 `missing_abi` warns, `double_negations`; non-null by alignment in const eval (#133700); empty `repr()` (#133925), `#![test]` (#134276) | ok (names) / not checked | |
| 1.85 the 2024 edition: `unsafe extern`, unsafe attributes, `unsafe_op_in_unsafe_fn`, RPIT capture and `use<>`, `gen`, the `expr` fragment, `&raw` of `static mut` | ok | D:v1_85 |
| 1.85 the 2024 edition's scopes of `if let` and tail-expression temporaries | ok | R:v1_85_edition_2024_temporary_scopes (the same program in 2021 and 2024) |
| 1.85 async closures (#132706), `#[diagnostic::do_not_recommend]` (#132056) | ok | D:v1_85. Not run: nothing polls a future in these tests |
| 1.85 the 2024 prelude's `Future`, `IntoFuture` | ok | `library/core/src/prelude/mod.rs:56`, hir-def `nameres/facts/collect.rs:997`. minicore's `rust_2024` has neither: with them one existing test (`auto_sized_async_block`) fails, so it is left |

## 1.80 to 1.84

| change | status | evidence |
|---|---|---|
| 1.84 `#[deny]` in `#[forbid]` (#121560), dropping the principal (#131857), `&raw` of `*ptr` is safe (#129248) | ok | D:v1_83_and_1_84, R:v1_84 |
| 1.84 drop glue when unwinding out of `extern "C"` (#129582), next solver in coherence (#130654), `include!` separators on Windows, s390x and Arm64EC `asm!` | not checked / n/a | |
| 1.83 `&mut` and `&Cell` in const, statics in const initializers, raw lifetimes, empty `#[non_exhaustive]` structs, `const extern fn`, `expr_2021` | ok | D:v1_83_and_1_84, R:v1_80_to_1_83 |
| 1.83 a place of type `!` does not coerce (#129392) | ok (refused, E0308) | D:v1_83_a_place_of_type_never_does_not_coerce |
| 1.82 empty patterns left out, `&raw` and of statics, `use<..>`, `unsafe extern`, nested `offset_of!`, `asm!` const operands, floats in `const fn`, unsafe attributes, binder order, statement fragments | ok | D:v1_80_to_1_82, R:v1_80_to_1_83 |
| 1.82 supertrait outlives (#124336), `[T; 0]` drop liveness (#128438), `size_of_val_raw` | not checked | |
| 1.81 `#[expect]` (#120924), `C-unwind` ABIs | ok | D:v1_80_to_1_82 |
| 1.81 a panic out of `extern "C"` aborts (#116088) | ok | hir-ty-mir `mir/eval/unwind.rs:201`; R:v1_81_a_panic_out_of_an_extern_c_function_aborts |
| 1.81 elided self lifetimes (#117967), opaque types in method resolution and unsizing (#123962, #125447, #125610), keyword lifetimes (#126762), `offset_from` | not checked | |
| 1.80 exclusive range patterns, variadics with no named parameter | ok | D:v1_80_to_1_82, R:v1_80_to_1_83 |
| 1.80 zero-sized accesses through any pointer (#117329) | ok | R:v1_80_zero_sized_accesses_through_any_pointer |
| 1.80 `Result` layout guarantee (#124870), maximum allocation size | not checked / n/a | |

## What a bump should do

The lint registry (`crates/blaze-lint-defs/src/lints.rs`) and the feature gates are hand copies
of rustc's and had drifted from the pin (agents row `lang1100-2`). Three differences from the
pinned nightly are left, each with new wording in rustc that blaze-diagnostics does not have:
`linker_info` and `linker_messages` are crate-level lints there, `rust_2021_prefixes_incompatible_syntax`
a semantics change, and `semicolon_in_expressions_from_non_local_macros` not in the dependency
report.
