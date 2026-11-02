---
status: plan
date: 2026-10-30
summary: no-fallback in hir-ty-mir and hir (2026-10-30): every fallback site in MIR building, the passes, the interpreter and hir's diagnostics, classified error, rule or dead, with what each becomes.
---
# no-fallback: hir-ty-mir and hir

Child `no-fallback-front-mir` of [no-fallback](../../no-fallback.md), area front. Kinds are the
lane's: **error** (a real fallback, now a typed error or the exact value), **rule** (rustc does
the same; it stays, with the rustc function it matches), **dead** (nothing reaches it; deleted
or queued for deletion). "Fixed" means changed in this lane; line numbers are as of the change.

## Fixed

| site | kind | what it did | rustc | becomes |
|---|---|---|---|---|
| `hir-ty-mir/src/mir/eval.rs` `size_align_of` (was :3897-3904) | error | in an enum's discriminant body, a value of that enum got a made-up size and alignment of 16 so the layout's cycle never showed | `layout_of` of the enum asks `AdtDef::discriminants`, which evaluates every variant's (`eval_explicit_discr`): E0391 cycle | deleted: the layout query cycles and `const_eval_discriminant_cycle_result` gives `MirLowerError::Loop` (or `layout_of_adt_cycle_result` `RecursiveTypeWithoutIndirection`). `E::F1 as isize` in a sibling's discriminant is no such value: as rustc's THIR (`discriminant_def_for_variant`), lowering reads the variant's discriminant constant, so test `enums` evaluates it |
| `hir-ty-mir/src/mir/eval.rs` `layout_of_variant` (was :2908-2918) | error | the same hack for a variant's layout: size 16, tag at 0 of 16 bytes | as above | deleted with it |
| `hir-ty-mir/src/mir/eval.rs` `allocate_allocation_in_heap`, thread-local and plain static allocation, inline const (`unwrap_or((memory.len(), 1))`, `map_or(1, ..)`) | error | a constant's or static's value whose type was unsized got its byte length and alignment 1 | values, statics and a constant's return slot are `Sized` (E0277 before MIR) | `size_align_of_sized`: `MirEvalError::TypeIsUnsized(ty, "the value of a constant"/"the value of a static")` |
| `hir-ty-mir/src/mir/eval.rs` `IntValue::from_bytes` (was :5819) | error | `never!` and then `I32(0)` for an operand of another width | — | `Result`: `MirEvalError::InternalError` naming the width (`MirEvalError` has no typed variant for an interpreter bug; it is hir-ty's) |
| `hir-ty-mir/src/mir/eval/shim.rs` `Intrinsic::NeedsDrop` (was :807) | error | `never!` and then `true` for a type still generic | `needs_drop` after monomorphization | `MirEvalError::InternalError` naming the type |
| `hir-ty-mir/src/mir/eval/shim.rs` `Intrinsic::TypeName` (was :781-783) | error | where rustc's printer (`rustc_type_name`) had no answer, rust-analyzer's `HirDatabase::type_name`, another spelling: a `const` `type_name::<T>()` could differ from the run-time one, which codegen refuses | `type_name` (`rustc_const_eval::util::type_name`) | the `NotSupported` error it already had for neither |
| `hir-ty-mir/src/mir/transform/asserts.rs` `binary_op` shift bound, `int_const`, `int_value` (`size_of(ty).unwrap_or(8)`) | error | an integer whose layout did not come back was taken as 8 bytes: the bounds of `isize`/`usize` checks on a 32-bit target | `Ty::primitive_size` | `ty.primitive_size(interner)`, which cannot fail for an integer |
| `hir-ty-mir/src/mir/transform/move_paths.rs` discriminant mask (`size_of(..)` `None` → `u128::MAX`) | error | a discriminant whose layout did not come back was compared unmasked | `Ty::primitive_size` of the discriminant type | `primitive_size`; a discriminant type that is not an integer is not a switch on it |
| `hir-ty-mir/src/mir/transform.rs` `Ctx::normalize` `Err(_) => Ty::new_error` | error | an alias that did not normalize became the error type, which `has_drop_glue` takes as nothing to drop: drop elaboration could lose a drop | `Ty::needs_drop`: `try_normalize_erasing_regions(..).unwrap_or(query_ty)` | the alias is kept, as for an undecided one: maybe needs drop |
| `hir-ty-mir/src/mir/lower.rs` `division_promotable` (`bit_width().unwrap_or(64)`) | error | `isize`/`usize` taken as 64 bits when deciding whether `isize::MIN / -1` is promoted; `-(1 << 127)` also overflowed for `i128` | `Validator::validate_rvalue` (promote_consts) | `primitive_size(..).bits()`, `i128::MIN >> (128 - bits)`; test `a_promoted_division_by_minus_one_of_every_width` |
| `hir/src/diagnostics.rs` `emit_inference_errors` (`filter_map`) | error | a type-checking diagnostic whose expression, pattern or type had no syntax (desugared) was dropped (`never!` then `.ok()`): the build went on with no error shown, or a check passed | rustc reports at the desugaring's span | `AnyDiagnostic::UnplacedDiagnostic { owner, what: Unplaced::Inference(diag) }` at the item the body is of; test `an_unplaced_diagnostic_is_at_the_item_of_its_body` |
| `hir/src/diagnostics.rs` `check_body` missing unsafe (`Err(SyntheticSyntax) => {}`) | error | an operation needing `unsafe` in desugared code: no E0133 | `check_unsafety` at the desugaring's span | `Unplaced::MissingUnsafe { reason, lint }` |
| `hir/src/diagnostics.rs` `collect_borrowck`, `collect_anon_const_borrowck` (`let Some(node) = .. else { continue }`) | error | a borrow-check error with no syntax: dropped | `mir_borrowck` | `Unplaced::Borrowck { code, message }` |
| `hir/src/diagnostics.rs` `collect_borrowck` const check | error | a const-check error with no syntax: dropped | `check_consts` | `Unplaced::ConstCheck { code, message }` |
| `hir/src/semantics/source_to_def.rs` `modules_in` ("`mod` declarations in block modules cannot be supported here") | error | the file of a `mod name;` declared in a block (`fn main() { #[path = "m.rs"] mod m; }`) had no module, so every path in it resolved to nothing and the stability check passed its uses of deprecated items | rustc resolves the block's `mod` like any other | `ViewCtx::block_declared_module`: from the crate's module files down through the out-of-line modules their blocks declare, resolving each `mod` as `module_to_def` does; the view's `file_to_module_def(s)`, `hir_file_to_module_def(s)` and `find_parent_file` use it (`Semantics::file_modules`); test `a_file_only_a_block_declares_is_a_module_whose_paths_resolve`. The silent `else { return }` in blaze-diagnostics' stability `walk.rs` `path()` is that lane's |
| `hir-ty-mir/src/mir/eval.rs` extern static allocation and `operand_ty` of a static (type from the static's body) | error | every extern static was zero-filled: a constant read `0` for one (test `extern_weak_statics`), a run read `0`/null for a strong one such as `environ`; and its type came from a body an extern static doesn't have, so its layout was `{unknown}` | const eval: `ExternStatic` ("cannot access extern static"), its address alone allowed; Miri: null only for weak statics it knows are unresolved | type from `type_for_static`; at compile time (a constant's or static's body, a const macro) the address is given and a read of it is `NotSupported` naming the static (`check_static_is_evaluated`); when run, an `extern_weak` one is null (initialized) and any other is `NotSupported` naming it. Tests `an_extern_static_is_not_read_at_compile_time`, `the_address_of_an_extern_static_is_known_at_compile_time`, `an_unresolved_weak_extern_static_is_null_when_run`, `reading_a_strong_extern_static_when_run_is_refused_naming_it` |
| `hir-ty-mir/src/tests/consteval.rs` `unsized_local` (now `an_unknown_type_in_a_constant_stops_its_lowering`) | error | expected `TypeIsUnsized` from evaluating past an unknown type | E0412 before MIR | expects `MirLowerError::HasErrors`, which lowering already gives |
| `hir/src/diagnostics.rs` `expr_syntax`/`pat_syntax`/`type_syntax`/lifetime/`Span::Dummy` `never!` | error | logged (debug: panicked) and dropped | — | plain `None`, which the callers above now turn into `UnplacedDiagnostic` |

blaze-diagnostics must lower the new variant (queue 01171, no-fallback-tools: its `lower`
match is exhaustive).

## Rules (kept)

| site | rustc |
|---|---|
| `hir-ty-mir/src/mir/eval.rs` `checks_overflow` `unwrap_or(true)` for a body inheriting the setting in a sysroot crate | CTFE's `CompileTimeMachine::ignore_optional_overflow_checks` is `false`: const eval always checks |
| `hir-ty-mir/src/mir/optimize/inline.rs:192` `crate_checks_overflow(..).unwrap_or(false)` | `None` is a sysroot crate, built without `-C overflow-checks` (`Session::overflow_checks` of its session) |
| `hir-ty-mir/src/mir/eval.rs` int/uint constant `bit_width().unwrap_or_else(ptr_size)` | `Integer::from_int_ty`: `isize`/`usize` are the pointer width, exactly |
| `hir-ty-mir/src/mir/transform/packed_drops.rs:83` `None => true` | `is_disaligned`: a layout that fails is taken as disaligned |
| `hir-ty-mir/src/mir/optimize/check_enums.rs` `find` `layout_of .. else return Ok(None)`, `offset.unwrap_or(ZERO)` | `EnumFinder::visit_rvalue` returns on a layout error; `insert_discr_cast_to_u128`'s offset of a direct tag |
| `hir-ty-mir/src/mir/optimize/inline.rs` `optimized(..).ok()`, `monomorphized(..).ok()?`; `inline/cycle.rs` `Err(_) => Vec::new()` | `Inliner::try_inlining`, `try_instantiate_mir_and_normalize_erasing_regions`: a callee whose MIR fails is not inlined; it is compiled (and its error reported) on its own |
| `hir-ty-mir/src/mir/monomorphization.rs` `normalize_or_keep`, `impl_opaque` `unwrap_or(ty)`, `try_fold_region` erased | `try_normalize_erasing_regions(..).unwrap_or(ty)`; the opaque stays for `TypingMode::PostAnalysis` to reveal; codegen erases regions |
| `hir-ty-mir/src/mir/lower/deref_pattern.rs:97` `unwrap_or(target)` | as `normalize_or_keep`: the projection itself, not a guess |
| `hir-ty-mir/src/mir/eval/threads.rs` `result.unwrap_or(Address::invalid(0))` | `pthread_join` writes null for a thread that returned none |
| `hir-ty-mir/src/mir/eval/shim.rs:612`, `eval.rs:4997`, `const_check/calls.rs:319` (intrinsic fallback bodies) | `#[rustc_intrinsic]` fallback bodies (`intrinsic_fallback_is_spec`) |
| `hir/src/match_lints.rs:102` | `find_fallback_pattern_typo` |
| `hir/src/diagnostics.rs` `{integer}`/`{float}` "after fallback" docs | integer/float literal fallback |
| `hir-ty-mir/src/mir/lower.rs` `implementation_error!` | logs, then returns `MirLowerError::ImplementationError`: an error, not a fallback |
| `hir-ty-mir/src/mir/lower.rs` adjustments `unwrap_or_default`, `legacy_const_generics_indices` `unwrap_or_default`, `block_edition` `unwrap_or_else(edition)` | true optionals: no adjustment, no `#[rustc_legacy_const_generics]`, a block outside a macro |
| `hir-ty-mir/src/mir/eval.rs:964` `trait_env.unwrap_or_else` | the body's own environment when none is given |
| `hir-ty-mir/src/mir/type_name.rs` `write!(..).ok()?` | writing to a `String` cannot fail |
| `hir-ty-mir/src/mir/location.rs`, `mir_pretty/errors.rs`, `const_check.rs` names, `purity.rs` names, `const_macro.rs:289`, `eval/unwind.rs:209` | the text or place of an error already being reported, not what the program means |
| scratch buffers (`spare_*.pop().unwrap_or_default()`), `check/bodies.rs` poisoned locks | reuse of buffers; poisoned-mutex `into_inner` |

## Dead

| site | becomes |
|---|---|
| `hir-ty-mir/src/mir/eval.rs` `assert_placeholder_ty_is_unused` (`size_align_of`'s `(0, 1)` for a placeholder) | every caller passes `false`; the parameter's removal spans hir-ty, hir-ty-codegen, hir-ty-share and hir: queue 01173 (no-fallback-front) |

## Left, with why

| site | why |
|---|---|
| `hir/src/diagnostics.rs` `collect_anon_consts_and_ty_diagnostics` `filter_map(ty_diagnostic)` | a signature's type-lowering diagnostic with no syntax is still dropped: these have no body to name, and a written type always has syntax; a desugared signature type would need an `Unplaced::TyLowering` placed at the signature's item |
| `hir/src/lib.rs` `Function::is_unsafe_to_call` target `Err(_) => No` | read only by blaze-diagnostics' `unused_unsafe` lint; a target that fails to load stops the build at layout first |
| `hir/src/lib.rs` `TupleField::ty`, `ModuleDef::krate`, `Type` rebasing `instantiate_with_errors` | IDE API, read by no build path |
