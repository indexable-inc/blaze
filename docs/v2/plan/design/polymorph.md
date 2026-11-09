---
status: plan
summary: Polymorphization in blaze v2 (lane polymorph, 2026-11-02) - one copy of a generic instance for every set of type arguments its code can't tell apart (unused, used only by size and alignment, used only through a table), decided by the share key before codegen; ADT facts become structural shapes, codegen-answered calls are keyed by full identity, what rustc's polymorphization did and why it was removed, the guarantee relied on, the dictionary-passing design left for later, and how it is measured.
date: 2026-11-02
---
# Polymorphization

A generic instance is compiled once for every set of type arguments its code cannot tell
apart. Its identity is the generic body with only what it reads of its arguments put in, so
instances that share are one InstanceKey: one compile, one store entry, one function. Callers
of either instance name the one function.

The key was `hir_ty_share::share_key` when this lane started. Lane `identity` has since moved
its walk into `hir_ty_identity::instance_identity` (`crates/hir-ty-identity/src/instance.rs`,
[../design/code-identity.md](code-identity.md)), which is now both the compile
identity and what `Described::args` holds. Types' facts stay in `hir-ty-share` (`fact.rs`).
Below, "share key" means that query.

Three kinds of dependence on a type parameter:

| the body reads `T` | example | shares across | where |
|---|---|---|---|
| not at all | `fn len<T>(v: &Vec<T>) -> usize` | every `T` | share key (before this lane) |
| only its size, alignment and ABI shape | `fn id<T>(x: T) -> T`, `mem::swap` | every `T` of one shape | share key, structural facts (this lane) |
| through a table (a resolved method, a vtable, a drop) | `fn sum<T: Add>(..)` | instances whose tables differ only in constants | `fold.rs` after codegen, dev builds; dictionary passing before codegen is designed below, not built |

## What was there

- `hir_ty_share::share_key` (crates/hir-ty-share, plan
  [fastcompile/share.md](compile-time.md)) walks the generic body and records what each
  site reads of the instance's arguments: type facts, constant values, each callee by its own
  share key, drops by drop glue, intrinsics by class, unsizing, reifying and vtables by full
  identity. The code generator's frontend (`crates/hir-ty-codegen/v2/frontend.rs`, `shared_args`)
  puts the key in `Described::args`, so equal keys are one InstanceKey. An unused parameter is
  never read, so it never splits instances.
- A type's fact held its ADT definition's identity, so `struct A(u32)` and `struct B(u32)` did
  not share, and integers were keyed by identity (`usize` and `u64` did not share).
- `crates/blaze-codegen/src/fold.rs` (#1292) folds functions whose prepared IR differs only in
  constants (sizes, the addresses of the functions, statics and vtables they name) into one body
  that takes the constants as trailing parameters, with each member a thunk. Dev builds only.
- `crates/blaze-codegen/src/dedupe.rs` folds byte-equal functions after codegen
  ([codegen/icf.md](icf.md)).

## What this lane changed

1. **ADT facts are shapes** (`crates/hir-ty-share/src/fact.rs`). An ADT's fact is its kind
   (struct, enum, union), its `repr` (flags, integer, `align`, `packed`, scalable; not the field
   shuffle seed, whose effect is in the layout's offsets), its layout, its number of variants,
   each variant's discriminant and field count, and each field's fact with the arguments put in.
   `A(u32)` and `B(u32)` now have one fact, and so do `Vec<A>` and `Vec<B>`, `Option<A>` and
   `Option<B>`. Kept as definition identity: lang items, `#[rustc_pass_indirectly_in_non_rustic_abis]`,
   `#[rustc_layout_scalar_valid_range_*]`, SIMD and scalable vectors (see the audit below).
2. **Numbers by class and layout.** An integer or float is one byte (signed, unsigned, float)
   and its layout: `usize` and `u64` share on a 64-bit target; `u32` and `i32` do not, since the
   layout's scalar carries its signedness and codegen reads it (extension, comparisons).
   `bool`, `char`, `str` and `!` stay their identity.
3. **`FACT_DOMAIN`** is `"blaze v2 share fact 2"`, so no key made under the old facts is reused.
4. **Codegen-answered calls by full identity** (`hir_ty_mir::mir::answered_by_codegen`, the one
   name list the identity walk, the specializer and `fncache::is_code` share). `__blaze_cache_code::<F>` is answered inline by
   the code generator with the code identity of their type argument. The walk
   keyed them by the callee's share key, which equals across closures of one shape that capture
   different types, so two `#[blaze::cache]` functions could have shared one cache code: a wrong
   cache hit. That was already unsound for `Vec<&A>`-style sharing and would have widened with
   shapes; they are now keyed by full identity.

## Why a shape is all the code reads: the audit

Wherever the code generator (`crates/hir-ty-codegen/mod.rs`, `v2/`) reads an ADT of a value it
handles, it reads something in its shape or something kept as identity:

| codegen reads | where (mod.rs, by function) | covered by |
|---|---|---|
| layout, field offsets, variants, niche | throughout | layout |
| `repr(packed)` for field access | `pack` | repr in the shape |
| the C ABI's scalar leaves, kind and fields | `scalar_leaves`, `lone_scalar`, `classify` | kind, fields' facts |
| `decays_in_c`: `rustc_pass_indirectly_in_non_rustic_abis`, `transparent` | `decays_in_c` | identity; `transparent` in repr flags |
| `UnsafeCell` (noalias, Freeze) | `is_plain_value` | lang item: identity; `Freeze` ends every fact |
| `Box` | `place_ref`, `drop_contents`, v2/refs/shims.rs `drop_contents` | lang item: identity |
| a constant built by variant | `eval_const`, `relocate` | discriminants and fields in the shape |
| fncache name checks | fncache | purity analysis only; not code |

What tells two shapes apart is never read through the fact: a method call is keyed by the
resolved callee's key, a drop by the drop glue's identity, `TypeId`, `type_name`, vtables and
unsizing by full identity, `__blaze_cache_code` by full identity.

## The guarantee relied on

Two instances becoming one function makes their addresses equal. Rust does not promise
distinct addresses for distinct functions:

- std, primitive `fn`: "pointers to different functions can compare equal (since identical
  functions can be deduplicated within a codegen unit)".
- `core::ptr::fn_addr_eq`: "May return `true` unexpectedly ... particularly likely with generic
  functions".
- rustc_codegen_llvm `declare::declare_fn` declares every Rust function `unnamed_addr`:
  "Function addresses in Rust are never significant, allowing functions to be merged".

Generic instances have no exported symbol, so no other language compares their addresses.
Debug info: a shared instance carries the representative's type names; this was already the
case for pointer-only sharing and changes no program's behavior.

Note (2026-11-02, shape-key lane): this holds only where the unit tells a debugger of no named
locals. At `DebugInfo::Limited`/`Full` (`DebugInfo::tells_locals`; cargo's dev `debug = 2`, the
blaze repo's `debug = 1`) a told local's type is the instance's own and sits in the shape and
the side table, so a shared instance is split: its key mixes in the exact arguments
(`HirFrontend::generic_args`, domain `blaze v2 told args 1`) and each instance is compiled
apart. Post-codegen dedupe still merges the copies (it compares `code_id`, not the side table),
so the image keeps one instance's local types. The follow-up is a code-key / node-key split:
share the template by compile identity and attach each instance's told types per node.

## rustc's polymorphization and what blaze does differently

- Added in rust-lang/rust#69749 (2020) behind `-Zpolymorphize`: an analysis marked unused
  generic parameters, and codegen replaced them with the parameter itself in the instance, so
  one polymorphic instance stood for all.
- Tracking issue rust-lang/rust#124962 (2024-05): nobody had worked on it for four years; it
  mostly worked but had bugs and was "very limited, not really useful".
- MCP rust-lang/compiler-team#810 (saethlin, 2024-11-22) proposed removal: polymorphized
  instances must never be re-monomorphized, and after monomorphization the compiler can no
  longer use `TypingEnv::fully_monomorphized`, so every later query has to carry the
  polymorphic case; no users were found. The discussion said a new implementation should start
  from scratch.
- Removed in rust-lang/rust#133883 (merged 2024-12-07, +59 -2643 in 92 files); dropping the
  analysis queries made the compiler about 0.5% faster. Its tests covered ABI mismatches,
  promoted functions, early-bound inlining, symbol ambiguity, unsized casts and drop shims, the
  places it went wrong.

blaze differs in what the sharing is:

- **A key, not a type transform.** blaze never builds a polymorphic instance. Codegen always
  compiles one fully monomorphized representative; sharing is equality of a key over what that
  monomorphized body reads. No later stage sees a parameter, so none of rustc's
  re-monomorphization bugs can arise: a wrong key is the only failure mode, and the key is
  checked (below).
- **Layout-only dependence**, not only unused parameters. rustc never shared `id::<A>` with
  `id::<B>`.
- **Callees by key.** A call is keyed by the callee's own share key, so sharing is transitive
  through call graphs without an analysis that propagates "used" bits.

## Prior art

- Swift `lib/LLVMPasses/LLVMMergeFunctions.cpp` merges functions that differ only in a few
  constants, "specialized functions of layout compatible types", passing the constants as
  parameters with thunks; ported to LLVM as MergeFunctionsIgnoringConst (llvm/llvm-project
  #68235, #71584; RFC discourse 73778 reports 1-2% size on iOS). This is `fold.rs`'s design.
- LLVM MergeFunctions, COMDAT folding and linker ICF (lld, mold, gold, MSVC `/OPT:ICF`) fold
  byte-equal code after it exists: `dedupe.rs`.
- github.com/kateinoigakukun/llvm-next-function-merging: sequence-alignment function merging
  (HyFM, F3M), which merges functions that differ in more than constants.
- .NET shared generics (`__Canon` with a runtime dictionary) and Go 1.18 GC-shape stenciling
  (one body per GC shape plus a dictionary): the closest to the shape key. blaze puts what a
  dictionary would hold in the key instead, so shared code looks nothing up at run time.

## Dictionary passing before codegen (designed, not built)

The third kind of dependence, a body that differs between instances only in which method or
vtable it calls, is shared today only after codegen and only in dev builds (`fold.rs`). Before
codegen it would be: the walk records each table read (a resolved callee, a vtable, a drop glue)
as a slot instead of keying it; instances whose keys are equal apart from slots share one body
that takes a hidden table argument; each instance is a thunk passing its table. That needs a
hidden-parameter calling convention and thunks in `hir-ty-codegen`, and it turns direct calls
into indirect ones, so it trades speed for size: it would be on only for code a profile marks
cold, or at `opt-level = "s"`/`"z"`. Not built in this lane.

## Determinism

Among the instances that share one InstanceKey, the driver (crates/blaze-codegen/src/driver.rs,
`walk`) keeps the one with the lowest `Owner`, so an image's describer wins over the
workspace's; among equal owners, the first the walk inserts. The walk order is deterministic for
a given program, so the representative, and the debug names it carries, are too; an edit that
changes walk order can change which instance's name, `identity` and `bound` the shared node
carries. A second instance whose describe disagrees with the kept one (its code hash, its number
of built-in callees or of references) is a `CodegenError` naming both and the key: the key
missed an input (rule 18), and neither is kept silently.

Note (2026-11-02, shape-key lane): at `Limited`/`Full` debuginfo instances with told locals
no longer share a key (see the note under the guarantee above), so there is no representative
among them; the `Occupied` branch also prefers the lower `owner`, not only the first inserted.

## Tests

- `hir-ty-identity` (`src/tests/compile.rs`, moved there from `hir-ty-share`), `values_of_one_shape_share`: `id`, `outer`, `size`, `nd` and a closure share
  across two ADTs of one shape; two fieldless enums with one discriminant set share; `u64` and
  `usize` share on 64-bit.
- `hir-ty-identity`, `what_a_shape_holds_tells_values_apart`: `u32`/`i32`, an ADT and its field
  type, a `transparent` wrapper, a union, enums with other discriminants, other layouts.
- `hir-ty-identity`, `what_tells_one_shape_apart_is_read_where_it_is_used`: `TypeId`, `type_name`,
  a resolved method, an unsizing cast, a drop with other glue, and `__blaze_cache_code` keep two
  shapes apart.
- `hir-ty-identity`, `what_the_code_generator_reads_off_a_definition_tells_values_apart`: a
  struct of one `u32` with `repr(packed(4))`, `rustc_pass_indirectly_in_non_rustic_abis`, a
  `#[lang]` attribute or `repr(C)` is apart from a plain one of the same layout, and
  `repr(C)` from `repr(C, packed(4))`. Each case pins one bit of `IDENTIFYING` or
  `write_shape`.
- `hir-ty-codegen`, `equal_share_keys_compile_to_one_shape`: for `id`, `swap` and `wrap`
  over eight types, every pair of instances with one InstanceKey prepares to one shape, and the
  pairs that share are exactly the expected ones.

## Measured by

Before and after (rule 19: each number names the tree digest and the blaze binary's hash):
build blaze from this tree and from a copy with this lane's diff reversed (`git apply -R`),
then with each run `blaze build -p blaze-engine` into an empty target directory, and record

- the driver's `Stats.instances` against `Stats.compiled` (instances compiled once for many);
- the `codegen dedupe` span's `functions`, `folded` and `bytes` (what was left for ICF to fold:
  it should fall by what the share key now folds first);
- the binary's size and the wall time, and the codegen spans' `self_cpu_ns`.

## Untested

- None of the tests have run; they type-check.
- No measurement yet.
- No C-ABI test: an `extern "C"` generic over two shapes is covered only by the audit
  (`decays_in_c` and the scalar leaves read only what the shape holds).
- Store entries follow the compile identity, which is now the share walk itself, so instances
  of one shape have one InstanceKey and one compile identity; no run has shown it yet.
