---
status: plan
summary: Polymorphization - one copy of a generic instance for every set of type arguments its code cannot tell apart (unused, used only behind thin pointers, used only by size, alignment and ABI shape), decided by canonical arguments before code is made. Built in crates/blaze-ladder/src/front2/poly_canon.rs and used by the ladder's code records; also what rustc's polymorphization did and why it was removed, the guarantee relied on, and the dictionary-passing design left for later.
date: 2026-11-02
---
# Polymorphization

A generic instance is made once for every set of type arguments its code cannot tell apart. Its
identity is the generic body with only what it reads of its arguments put in, so instances that
share are one code record and one function. Callers of either instance name the one function.

Built in `crates/blaze-ladder/src/front2/poly_canon.rs` (its module doc is the specification):
a use per generic parameter from each body, propagated over the call graph to a fixpoint, then
each ground argument mapped to its canonical form and the instance encoded and named from that
(`canonical_fn_symbol`). The code records read it (`ReadKind::PolyBodyText`).

| the body reads `T` | example | canonical argument |
|---|---|---|
| not at all, or only in `PhantomData` | `fn len<T>(v: &Vec<T>) -> usize` | `()` (`CanonLevel::Unused`) |
| only behind thin pointers, never by value | `fn ptr_addr<T>(p: &T) -> usize` | an opaque thin pointee (`OpaquePtr`) |
| only its size, alignment and ABI shape, and `T` is sized and needs no drop | `fn id<T>(x: T) -> T`, `mem::swap` | its `LayoutShape` (size, align, ABI class, niche, uninhabited) (`LayoutOnly`) |
| through a table (a resolved method, a vtable, a drop), or anything not analyzed | `fn sum<T: Add>(..)` | itself (`Concrete`); dictionary passing is designed below, not built |

**What keeps an argument concrete.** An unanalyzed callee, a trait method dispatch, an inline
`const { .. }` block, inline `asm!`, an unsizing coercion, a non-Rust ABI or a reflection
intrinsic (`TypeId`, `type_name`). Layout shapes keep apart what code can tell apart: `f64` from
`u64`, `i8` from `u8` (sign or zero extension), a non-transparent `struct S(u64)` from `u64`,
two niches, and a type that needs drop from one that does not.

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

- **Canonical ground arguments, not a parameter.** blaze never builds a polymorphic instance:
  each ground argument is replaced by its canonical form (`()`, an opaque thin pointee, a layout
  shape, or itself) and the instance is made once for that canonical argument list. No later
  stage sees a parameter, so none of rustc's re-monomorphization bugs can arise: a canonical
  argument too coarse is the only failure mode, and anything the analysis cannot see keeps the
  argument concrete.
- **Layout-only dependence**, not only unused parameters. rustc never shared `id::<A>` with
  `id::<B>`.
- **Through the call graph.** A parameter's use is propagated over direct calls, drops and
  recursive cycles to a fixpoint, so a body that hands `T` to a callee that reads it is not
  shared by `T`.

## Prior art

- Swift `lib/LLVMPasses/LLVMMergeFunctions.cpp` merges functions that differ only in a few
  constants, "specialized functions of layout compatible types", passing the constants as
  parameters with thunks; ported to LLVM as MergeFunctionsIgnoringConst (llvm/llvm-project
  #68235, #71584; RFC discourse 73778 reports 1-2% size on iOS).
- LLVM MergeFunctions, COMDAT folding and linker ICF (lld, mold, gold, MSVC `/OPT:ICF`) fold
  byte-equal code after it exists.
- github.com/kateinoigakukun/llvm-next-function-merging: sequence-alignment function merging
  (HyFM, F3M), which merges functions that differ in more than constants.
- .NET shared generics (`__Canon` with a runtime dictionary) and Go 1.18 GC-shape stenciling
  (one body per GC shape plus a dictionary): the closest to the shape key. blaze puts what a
  dictionary would hold in the key instead, so shared code looks nothing up at run time.

## Dictionary passing before codegen (designed, not built)

The third kind of dependence, a body that differs between instances only in which method or
vtable it calls, is not shared (its arguments stay concrete). Before codegen it could be: the walk records each table read (a resolved callee, a vtable, a drop glue)
as a slot instead of keying it; instances whose keys are equal apart from slots share one body
that takes a hidden table argument; each instance is a thunk passing its table. That needs a
hidden-parameter calling convention and thunks in the code generator, and it turns direct calls
into indirect ones, so it trades speed for size: it would be on only for code a profile marks
cold, or at `opt-level = "s"`/`"z"`.

## Determinism

The canonical instance is encoded independently of table indices and named by a BLAKE3 symbol
in its own domain, so canonical symbols never collide with uncanonical ones and two builds of
one tree name one instance alike (rule 25).

## Tests

`crates/blaze-ladder/tests/poly_canon_pipeline.rs` runs the ladder's walk and code records on
fixtures: references and opaque pointers share one code record while distinct types stay apart;
reflection and trait dispatch stay concrete; a multi-type fixture reports generic instances
before and after. `poly_canon.rs` has unit tests of the analysis and the shapes.
