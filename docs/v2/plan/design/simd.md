---
status: plan
summary: Portable SIMD in blaze. The vector types are the pinned nightly's `core::simd`, with no feature gate under `#![blaze::simd]` (the edition turns it on); blaze adds one macro, `simd::native_lanes!`, and `#[blaze::multiversion]`, which compiles a function once per CPU level and picks one when the program starts; how the differentiator and the GPU kernels use the same types.
date: 2026-11-10
---
# Portable SIMD and multiversioning

The vector library is the pinned nightly's `core::simd` (`Simd<T, N>`, `Mask<T, N>`, the `Simd*`
traits, `simd_swizzle!`), unchanged; every build uses that nightly (`own-nightly`), so
"unstable" means only "may change at a bump". blaze writes no vector type, wrapper or operator
trait. It adds three things no library can: the gate, a native width, and multiversioning.
Prior art, one lesson each:

| who | what | lesson |
|---|---|---|
| Zig `@Vector(N, T)` | built in, element-wise operators, every target, `std.simd.suggestVectorLength(T)` | availability is the feature; the width is a compile-time fact of the target |
| Rust `core::simd` | typed masks, traits per element kind | the right API, held back only by the gate |
| `core::arch` | one module per architecture | the escape hatch: code in it is written once per target |
| ISPC | `uniform`/`varying`, scalar-looking code | a loop nobody vectorizes by hand; here that is LLVM's vectorizer and `blaze::vecreport`, not a language mode |
| Highway, the `multiversion` crate | clones per level, dispatch through a pointer | pick once, call through a pointer; a proc macro copies the body, so it is checked N times |
| GCC/Clang `target_clones` | compiler clones, ELF ifunc | compiler clones share one checked body; ifunc ties selection to one object format |

## 1. The gate

`#![blaze::simd]` on a crate root turns on the library features `portable_simd` and `blaze_simd`
for that crate, where `#![feature]` is read (`hir_def::nameres::facts::collect::crate_data`), so
every stability check reads them as if written by hand. Nothing else changes. The edition turns
it on: one more bit of the edition branch's `edition::Extensions` (not on main). A crate that
does not opt in sees today's behaviour exactly: E0658 on each `core::simd` path unless it
writes `#![feature(portable_simd)]`. For the bstd lane: `bstd::simd` is one line,
`pub use core::simd;`, in bstd's facade ([std](std.md)); bstd's crates carry `#![blaze::simd]`.

## 2. What blaze adds beside `core::simd`

**A native width.** `core::simd` has no suggested length. blaze's `core` gains one macro
(`library/core/src/blaze_simd.rs`, re-exported in `core::simd`, unstable under `blaze_simd`):

```rust
use core::simd::{Simd, native_lanes};                // in a crate with #![blaze::simd]
type F32s = Simd<f32, { native_lanes!(f32) }>;   // 4 lanes on x86-64, 8 with avx2, 16 with avx512f
```

It expands to `cfg!(target_feature = ..)` tests, widest first: 64 bytes with `avx512f`; 32 with
`avx2` or `lasx`; 16 with `sse2`, `neon`, `simd128`, `lsx`, `altivec`, `vector` or RISC-V `v`;
divided by the element's size, never less than one lane. Why a macro and no intrinsic: target
features are already a compile-time fact the front end keys (`cfg`), and `cfg!` reads the crate
that expands it, so the library says it and the compiler learns nothing
(`library-over-language`). The answer follows the flags, never the machine a dev build runs
on, so a type's layout is the same in dev and release; `avx` without `avx2` counts as 16 bytes.
Not working yet: `native_lanes!(T)` of a generic `T` in a const argument
(`generic_const_exprs`); write the function over `const N: usize`.

**Nothing else.** Construction and indexing exist (`from_array`, `splat`, `Index`); no literal
syntax. Scalar loops are LLVM's vectorizer in release builds (the dev tier's stencils do not
vectorize), and `blaze::vecreport` ([vecreport](vecreport.md)) says why a loop stayed scalar.

**Gaps found and left out.** (a) A width per clone: a type cannot differ between clones, so
`native_lanes!` in a clone is still the unit's. The primitive that closes it is a clone
choosing its callee's instance (a function over `const N: usize`, each clone naming its
level's width); it waits for a use. (b) Structs of `Simd` fields want `for const`
([comptime](comptime.md), phase 2), not new syntax. (c) The lint `blaze::arch_intrinsic` (a
`core::arch` call with a direct `core::simd` equivalent; allow by default, warn in the edition)
needs an intrinsic-to-method table checked against the evaluator's shims: not built.

## 3. `#[blaze::multiversion]`

```rust
#[blaze::multiversion]                                    // the target's default levels
fn dot(a: &[f32], b: &[f32]) -> f32 { .. }
#[blaze::multiversion("x86-64-v3", "avx512f,avx512vl")]   // explicit, least preferred first
```

**Levels** (`target_features::multiversion`). A level is a CPU of the target's table
(`target_features::cpus`: `x86-64-v2`, `-v3`, `-v4` are there) or rustc's feature names,
comma-separated. Defaults: x86 the three levels; every other target none (AArch64's baseline
has `neon`, and `core::simd` has no scalable vectors for SVE to widen). A level's set is the
unit's code-generation features plus the level's, closed by what each implies. A level that
adds nothing is dropped, so a dev unit, compiled for the host, keeps only levels above it. An
unknown name, an empty level, two levels of one set and more than eight are typed errors.

**Clones.** The function is checked and lowered to MIR once, as written (its body is baseline
code: a `core::arch` call still needs `unsafe`). A clone is that MIR compiled with its level's
set, through what gives a `#[target_feature]` function its set today (`hir-ty-codegen` v2's
`features`), so LLVM inlines baseline callees into it. A generic function has clones per
instance; a trait method may carry the attribute (a `dyn` call reaches the dispatcher);
closures inherit the clone's set; `const` evaluation runs the one MIR. A call from a clone to
another multiversioned function goes straight to that function's `Plan::select` for its set.

**Selection.** Each instance has a pointer-sized data cell, linked holding the baseline's
address, and a dispatcher (load the cell, jump: what a jump-table slot's stub costs), which is
what the function's name, pointers to it and vtable entries are. One initializer per image,
placed as the dev runtime's start hook is, asks std's detection once (`std_detect`, behind
`is_x86_feature_detected!`) and stores in each cell `Plan::select`'s clone: the last declared
whose added features are all present. No call tests a feature; a call before the initializer
runs the baseline, always sound. One mechanism for every output: blaze-link has no ifunc
(`elf/read.rs` refuses one), only an incremental image has a jump table, and an `Abs64` cell
links everywhere. No std means no detection: a typed error. Open: cells after a live patch.

**The invariant (soundness).** A clone runs only on a CPU that has every feature of its set.
A clone has no name and no address a program can take; it is entered only through the cell,
filled with a clone whose added features `std_detect` reported (the unit's own are the
program's precondition), or from a clone whose set contains its own. So the attribute is safe
where calling a `#[target_feature]` function is not.

**Determinism.** Every clone computes the baseline's bits: `core::simd`, integer and IEEE
operations do not depend on the feature set, LLVM does not contract `a * b + c` because `fma`
is on, and `cfg!` is the unit's in every clone. `fast_math`-style attributes are refused here.

**Keys.** A clone's code identity includes its feature set (v2's `shape.rs` hashes an
instance's feature names) and a plan refuses two levels of one set, so two clones never share
one. The dispatch table is keyed by `Plan::key` (the arch, the unit's set, each level's name
and set, in order). An edit recompiles each clone of the edited function.

## 4. The differentiator and GPU kernels

`#[blaze::differentiable(symbolic)]` ([autodiff](../../../future/autodiff.md), layer 2) lifts
`Simd<f32|f64, N>` by scalarizing: a lane-wise `simd_*` intrinsic becomes N scalar nodes, a
reduction a fold, a swizzle a renaming. No vector sort enters the graph, so the derivative
rules and the e-graph stay scalar; lowering emits `Simd` again where N isomorphic lane
expressions remain. Next step: the lift's intrinsic table gains the lane-wise float operations
(blaze-diff is not on main; commit d12e99d15a holds it).

Kernels ([gpu](../../../future/gpu/plan.md)) use the same types: `vec2/3/4` are
`Simd<f32, 2|3|4>`, masks `Mask<i32, N>`. The SPIR-V lowering maps `Simd<T, N>` for N in 2..=4
to `OpTypeVector`, lane-wise intrinsics to the vector forms of the scalar opcodes,
`simd_swizzle!` to `OpVectorShuffle`, `Mask` to a `bool` vector with `select` as `OpSelect`,
reductions to extract-and-fold; N above 4 is a typed error naming the kernel.
`#[blaze::multiversion]` on a kernel is refused: a device has one level.

## Built in slice 1, and what is not

Built: the gate; `native_lanes!`; `target_features::multiversion` (levels, `Plan`, `select`,
`key`); the attribute read in the front end (`AttrFlags::multiversion_levels`,
`hir_ty::multiversion_plan`) and checked in codegen. Not built: the clones, cells, dispatcher
and initializer. Until they are, a multiversioned function whose plan has a clone stops the
build in codegen with an error naming it and its levels (`no-fallback`); a plan with no clone
compiles as the function itself. Tests: the ledger's row E13 names them, and the two still to
write with the clones (the instances and sets a function compiles to; each clone giving the
baseline's bits).
