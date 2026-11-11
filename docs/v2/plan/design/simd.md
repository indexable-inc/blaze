---
status: plan
summary: Portable SIMD in blaze. The vector types are the pinned nightly's `core::simd`, with no feature gate under `#![blaze::simd]` (the edition turns it on); blaze adds one macro, `simd::native_lanes!`, and `#[blaze::multiversion]`, which compiles a function once per CPU level and picks one on its first call; how the differentiator and the GPU kernels use the same types.
date: 2026-11-10
---
# Portable SIMD and multiversioning

The vector library is the pinned nightly's `core::simd` (`Simd<T, N>`, `Mask<T, N>`, the `Simd*`
traits, `simd_swizzle!`, `Select`), unchanged. Every blaze build uses the same nightly
(`own-nightly`), so "unstable" there means only "may change at a bump", and a bump is one commit
that fixes blaze's own uses. blaze writes no vector type, wrapper or operator trait. It adds
three things, each because no library can: the gate, a native width, and multiversioning.

## Prior art, one lesson each

| who | what | lesson |
|---|---|---|
| Zig `@Vector(N, T)` | built in, operators element-wise, works on every target, `std.simd.suggestVectorLength(T)` | availability is the feature: vectors people can use without asking get used; the width is a compile-time fact of the target |
| Rust `core::simd` | typed masks, traits per element kind | the right API; held back only by the gate |
| `core::arch` | one module per architecture | the escape hatch, not the default: code written in it is written once per target |
| ISPC | `uniform`/`varying`, scalar-looking code | a loop the author need not vectorize by hand; in blaze that is LLVM's vectorizer plus `blaze::vecreport`, not a new language mode |
| Highway, the `multiversion` crate | clones per level, dispatch through a pointer | pick once and call through a pointer; a proc macro copies the body, so it is checked N times and its inner statics are N statics |
| GCC/Clang `target_clones` | compiler clones, ELF ifunc | compiler clones share one checked body; ifunc ties selection to one object format and to code that runs before relocation |

## 1. The gate

`#![blaze::simd]` on a crate root turns on the library features `portable_simd` and `blaze_simd`
for that crate (`hir_def::nameres::facts::collect::crate_data`, where `#![feature]` is read:
both are entered into `UnstableFeatures`, so every stability check reads them as written by
hand). Nothing else changes. The edition turns it on with the other changes: it is one more
bit of the edition branch's `edition::Extensions` (not on main), set by
`edition = "blaze-2026.11"`. A crate that does not opt in sees today's behaviour exactly: E0658
on each `core::simd` path, unless it writes `#![feature(portable_simd)]` itself.

For the bstd lane: `bstd::simd` is one line, `pub use core::simd;`, in bstd's facade
([std](std.md), "What is in it"); bstd's crates carry `#![blaze::simd]`.

## 2. What blaze adds beside `core::simd`

**A native width.** `core::simd` has no suggested length. blaze's `core` gains one macro
(`library/core/src/simd/native.rs`, unstable under `blaze_simd`):

```rust
#![blaze::simd]
use core::simd::{Simd, native_lanes};
type F32s = Simd<f32, { native_lanes!(f32) }>;   // 4 on x86-64's baseline, 8 with avx2, 16 with avx512f
```

It expands to `cfg!(target_feature = ..)` tests, widest first, over the element's size: 64
bytes with `avx512f`; 32 with `avx2` or `lasx`; 16 with `sse2`, `neon`, `simd128`, `lsx`,
`altivec`, `vector` or RISC-V `v`; else one lane. Why a macro and not an intrinsic: the unit's
target features are already a compile-time fact the front end keys (`cfg`), and `cfg!` reads the
crate that expands it, so the library can say it and the compiler learns nothing new
(`library-over-language`). Consequences, all intended: the answer follows the flags
(`-C target-cpu`, `-C target-feature`), never the machine a dev build happens to run on, so a
type's layout is the same in a dev and a release build; `avx` without `avx2` counts as 16 bytes
(integer vectors would split). What does not work: `native_lanes!(T)` with `T` a generic
parameter in a const argument (needs `generic_const_exprs`); write the function over
`const N: usize` and name the width where the element type is known.

**Nothing else.** Construction and indexing exist (`from_array`, `splat`, `Index`,
`as_array`). No literal syntax. Scalar loops are LLVM's vectorizer in release builds; the dev
tier's stencils do not vectorize, and `blaze::vecreport` ([vecreport](vecreport.md)) says why a
release loop stayed scalar.

**Gaps found and left out.** (a) A width per clone: a type cannot differ between clones of one
function, so inside a clone `native_lanes!` is still the unit's. The smallest primitive is none
new: a multiversioned function generic over `const N: usize`, with each clone's callers naming
the level's width; it needs clone-specific callees and waits for use. (b) Mapping over lanes of
unlike types and deriving `Simd` structs-of-arrays want `for const` ([comptime](comptime.md),
phase 2), not new syntax. (c) The lint `blaze::arch_intrinsic` (a `core::arch` call with a direct
`core::simd` equivalent; allow by default, warn in the edition) needs an intrinsic-to-method
table checked against the evaluator's shims; specified here, not built.

## 3. `#[blaze::multiversion]`

```rust
#[blaze::multiversion]                          // the target's default levels
fn dot(a: &[f32], b: &[f32]) -> f32 { .. }
#[blaze::multiversion("x86-64-v3", "avx512f,avx512vl")]   // explicit, least preferred first
```

**Levels.** A level is a CPU of the target's table (`target_features::cpus`, the one home of
CPUs: `x86-64-v2`, `-v3`, `-v4` are there) or a comma-separated list of rustc's feature names.
Defaults: x86 `x86-64-v2`, `x86-64-v3`, `x86-64-v4`; every other target none (AArch64's
baseline has `neon`; `core::simd` has no scalable vectors, so SVE is no default). A level's set
is the unit's plus the level's, closed by what each implies. A level the unit already covers
is dropped; an unknown name, an empty level and two levels with one set are typed errors naming
the level (`target_features::multiversion::Plan`).

**Clones.** The function is type-checked, borrow-checked and lowered to MIR once, as written
(its body is ordinary baseline code: a `core::arch` call still needs `unsafe`). A clone is the
same MIR compiled with its level's set, through the machinery `#[target_feature]` uses today
(`hir-ty-codegen` v2's `features` gives an instance its set; LLVM inlines baseline callees into
it). A generic function has clones per instance; a trait method may carry the attribute
(clones per impl instance; a `dyn` call reaches the dispatcher); closures and nested functions
inside inherit the clone's set, as they do a `#[target_feature]` function's. A call from a
clone to another multiversioned function goes straight to that function's best clone whose set
is within the caller's. `const` evaluation runs the one MIR.

**Selection.** Each instance has a pointer-sized cell and a dispatcher (load the cell, jump:
what a slot's stub costs). Function pointers and vtables hold the dispatcher. The cell is
linked pointing at a resolver, which on the first call asks std's detection (`std_detect`,
what `is_x86_feature_detected!` reads), takes the last declared clone whose added features are
all present, else the baseline, stores it, and calls it. No call tests a feature. The same
mechanism serves every output kind: blaze-link has no ifunc (`elf/read.rs` refuses one) and a
file output has no jump table, while an `Abs64` data cell links everywhere, and resolving on
first call has no initializer-order hazard. A unit that runs where it is built (`Runs::Here`:
dev, tests) is already compiled with the host's features, so it should keep no clone and an
edit compiles the function once; that needs `Runs` in the toolchain's settings. Elsewhere an
edit recompiles each clone of the edited function. A crate that links no std has no detection:
a typed error naming the function.

**The invariant (soundness).** A clone runs only on a CPU that has every feature of its set.
It holds by construction: a clone has no name and no address a program can take; it is entered
only from the cell, which the resolver fills with a clone whose added features `std_detect`
reported (the unit's own are the program's precondition), or directly from a clone whose set
contains it, which by induction already runs on such a CPU. That is why the attribute is safe
where calling a `#[target_feature]` function is not.

**Determinism.** Every clone computes the baseline's bits. `core::simd`, integer and IEEE
operations do not depend on the feature set; LLVM does not contract `a * b + c` because `fma`
is on, and `mul_add` is correctly rounded with or without it. Out of scope and refused on a
multiversioned function: `fast_math`-style attributes. What a clone may not read: `cfg!` (it
is the unit's in every clone) is fine; `is_*_feature_detected!` answers the CPU, the same in
every clone.

**Keys.** A clone's code identity includes its feature set (v2's `shape.rs` hashes an
instance's feature names), so two clones never share one, and a plan refuses two equal sets.
The dispatch table is keyed by `Plan::key`: the arch and each level's name and set. The
levels come from the attribute and the CPU table, both inputs of the front end.

## 4. The differentiator

`#[blaze::differentiable(symbolic)]` ([autodiff](../../../future/autodiff.md), layer 2) lifts
`Simd<f32|f64, N>` by scalarizing: each lane-wise `simd_*` intrinsic becomes N scalar nodes of
the existing graph, a reduction a fold, a swizzle a renaming. No vector sort enters the graph,
so the derivative rules and the e-graph stay scalar, and lowering emits `Simd` again where N
isomorphic lane expressions remain (else scalars, which LLVM may re-vectorize). Next step: the
lift's intrinsic table gains the lane-wise float ops (blaze-diff is not on main today; commit
d12e99d15a holds it).

## 5. GPU kernels

Kernels ([gpu](../../../future/gpu/plan.md)) use the same types: `vec2/3/4` are
`Simd<f32, 2|3|4>`, masks are `Mask<i32, N>`. The SPIR-V lowering maps `Simd<T, N>` for N in
2..=4 to `OpTypeVector`, lane-wise intrinsics to the vector forms of the scalar opcodes,
`simd_swizzle!` to `OpVectorShuffle`, `Mask` to a `bool` vector with `select` as `OpSelect`,
and reductions to the GLSL.std.450 or explicit extract-and-fold forms; N above 4 is a typed
error naming the kernel until arrays-of-vectors are chosen. `#[blaze::multiversion]` on a
kernel is refused: a device has one level.

## Built in slice 1, and what is not

Built: the gate; `native_lanes!`; `target_features::multiversion` (levels, the plan, selection,
the table's key) with its property test; the attribute read in the front end
(`AttrFlags::BLAZE_MULTIVERSION`, `hir_ty::multiversion_plan`). Not built: the clones, cells,
dispatcher and resolver in codegen and std. Until they are, a multiversioned function whose
plan has a clone stops the build in codegen with an error naming it (`no-fallback`); a plan
with no clone (every default target but x86) compiles as the function itself.

Tests: `hir-def` `blaze_simd_turns_on_portable_simd_and_blaze_simd`; `blaze-diagnostics`
`core_simd_needs_no_feature_under_blaze_simd` and
`core_simd_without_blaze_simd_is_gated_as_before`; `hir-ty-mir` consteval
`native_lanes_is_the_widest_enabled_vector`; `target-features` `multiversion::tests`
(`the_selected_clone_is_supported_and_the_best`, its two controls, the exhaustive lattice, the
plan's refusals, `clones_never_share_a_feature_set`). For the main agent, once clones exist: a
release build of a `dot` over drawn inputs asserting each clone's bits equal the baseline's.
