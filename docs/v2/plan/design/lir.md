---
status: plan
summary: "LIR, blaze's own low-level IR (Andrew's decisions, 2026-11-09), built in crates/blaze-lir and read by blaze-emit (the baseline tier) and blaze-export's wasm64 emitter, whose modules run in-process under Wasmtime. Keeps the decisions it rests on: defined by default, facts as promises, meanings and hints; exactly rustc's facts, no TBAA; the unwinding model; const eval not on LIR; a new IR, not lowered MIR. Still to build: LIR to LLVM IR directly, LLVM's own allocations through blaze's allocator with a context per compile, the copy-and-patch stencil tier, kept LIR records; ranked risks."
date: 2026-11-09
---
# LIR: blaze's low-level IR

Andrew's decisions (2026-11-09):

1. Code generation takes blaze's own low-level IR, LIR, as its one input.
2. LLVM release code is generated from LIR directly, LIR to LLVM IR.
3. LLVM's own allocations (C++ `new` and `malloc`) go through blaze's allocator and are charged
   to the unit's task ([below](#llvms-memory)).

## What exists

`crates/blaze-lir` (`no_std`, `#![blaze::sans_io]`) is the IR: one monomorphized function after
layout, the calling convention, drop elaboration and the UB checks, before instruction
selection, as columns (Zig's tag plus one word), SSA with block parameters, machine-typed with a
real `ptr`. The crate's module comment is the home of its encoding, types and operations; beside
them are `verify`, the text form (`print`/`parse`, byte for byte), `persist` (the blaze-format
record), the simplify passes, the coroutine split, the inliner, and a reference evaluator
(`eval`, for tests only).

Its readers:

- `crates/blaze-emit`: the baseline tier, one LIR function to AArch64 machine code in one
  forward pass ([backend-next.md](backend-next.md)).
- `crates/blaze-export/src/lir_wasm64.rs` and `wasm64.rs`: LIR to wasm64 (`memory64`) modules.
  blaze compiles proc-macro crates and pure-Rust build scripts itself to these and runs them
  in-process under Wasmtime, sandboxed. Native tools and C-linking build scripts go to the
  hypervisor micro-VM sandbox instead.
- Cranelift is in blaze only as Wasmtime's compiler (`vendor/cranelift-codegen`, pinned for
  Wasmtime alone).

## Semantics and facts

### Defined by default, facts are opt-in UB

A flag is one of three kinds, and the kind says what a consumer may do with it:

- **Promises** are opt-in UB, and a consumer may drop them.
- **Meanings** must be honoured or strengthened, never dropped. `volatile` stays volatile for
  any consumer that optimizes. An atomic's ordering may only be raised, up to `seq_cst`, which
  is also the ordering of an atomic op that names none. A single-thread fence may only become a
  full fence.
- **Hints** (`cold`, branch weights) may be dropped.

Every operation without promises is fully defined, the way Cranelift's are:

- A shift masks its amount.
- Division by zero traps with its code (`int_divz`).
- `fptosi` saturates.
- A load reads the bytes there.

Each promise is one the builder proves from Rust's semantics. Breaking it is UB, as in LLVM's
poison model:

- `nsw`/`nuw` for `unchecked_add`.
- `in_range` on a shift after its overflow check.
- `inbounds` on `ptr::add` and field projections.
- `exact` for `exact_div`.
- `nonzero` on a division whose divisor was checked.
- A `MemAttrs` range on a load of `bool`.

So a consumer may ignore any promise or hint and still compile a correct program, because
dropping a promise refines the program. The baseline tier and wasm ignore every promise and hint, keep
`volatile` (they never merge or remove an access anyway) and strengthen orderings. LLVM maps all
three kinds. The builder can add a promise later without changing what any promise-free
program means, so a fact can be landed, measured and reverted by itself.

The one exception is memory. Bytes may be uninitialized and pointers carry provenance, as in
Rust's abstract machine, which rustc maps onto LLVM's. A load of uninitialized bytes as a
scalar is UB in Rust and poison in LIR. Every consumer but LLVM gives it the bytes that are
there.

### Rust's facts, and where each goes

LIR carries exactly what rustc's `rustc_codegen_llvm` tells LLVM for the pinned nightly, and no
more by default (rule `superset-of-rust`: a program that runs correctly when rustc builds it
must not be miscompiled by a stronger claim). The rules are ported from rustc's source by
reading it, never by running it: `rustc_ty_utils::abi`'s `arg_attrs_for_rust_scalar`,
`rustc_codegen_llvm::abi`, `builder.rs`'s load metadata, and `rustc_codegen_ssa::mir`. Each rule
is a function of the builder named after the rustc function it ports.

| Rust fact | where the builder knows it | LIR | LLVM |
|---|---|---|---|
| `&T`, `&mut T` parameters: non-null, aligned | the layout of `T` | parameter attributes | `nonnull`, `align` |
| dereferenceable: `&T` with `T: Freeze`, `&mut T` with `T: Unpin` (rustc gives the others, and `Box`, a pointee size of 0) | `is_freeze`, `Unpin` | `deref(n)` | `dereferenceable(n)`, `dereferenceable_or_null(n)` for `Option<&T>` |
| `&mut T` with `T: Unpin`; `&T` with `T: Freeze`; `Box<T, Global>` with `T: Unpin` | the same, and the allocator | `noalias`; `readonly` for `&T` alone | `noalias`, `readonly` |
| valid range of a scalar argument or return (`bool`, `char`, enums, `NonZero`) | `Scalar::valid_range` | `range(lo, hi)` | `range` attribute, `noundef` |
| a loaded scalar with a valid range | the place's layout | `MemAttrs.range` | `!range`, `!noundef` |
| a loaded reference | the place's type | `MemAttrs.nonnull`, `.align` | `!nonnull`, `!align`, `!noundef` |
| alignment of every access | the place's layout (1 under `repr(packed)`) | `MemAttrs.align` | `align n` |
| vtable slots | vtable loads | `MemAttrs.invariant` | `!invariant.load` |
| `unchecked_*`, the non-overflowing arm of checked arithmetic | the intrinsic, the `Assert` | `nsw`/`nuw` | `nsw`/`nuw` |
| `exact_div`, `unchecked_shl`/`shr`, `disjoint_bitor` | the intrinsic | `exact`, `in_range`, `disjoint` | `exact`, no mask, `or disjoint` |
| field projections, `ptr::add` (a `usize` count) | the projection, the intrinsic | `ptr_add inbounds nuw` | `getelementptr inbounds nuw` |
| `ptr::offset` (an `isize` count) | the intrinsic | `ptr_add inbounds` | `getelementptr inbounds` |
| `wrapping_offset`, `wrapping_add` on pointers | the intrinsic | `ptr_add` | `getelementptr i8` |
| atomic orderings, weak `compare_exchange` | `AtomicOrdering` of the intrinsic | the op's ordering | the ordering, `cmpxchg weak` |
| `read_volatile`, `write_volatile` | the intrinsic | `volatile` | `load volatile`/`store volatile` |
| `StorageLive`, `StorageDead` | MIR | `slot_live`/`slot_dead` | `llvm.lifetime.start`/`end` |
| `assume`, `assert_unchecked`, `likely`, `unlikely`, `cold_path`, `#[cold]` callees | the intrinsic, the callee's `#[cold]` | `assume`, branch weights, cold blocks | `llvm.assume`, `!prof`, `cold` |
| `fadd_fast` and the rest, `*_algebraic` | the intrinsic | float flags | fast-math flags |
| `-> !`, nounwind (panic=abort, `extern "C"` without `-unwind`), `#[inline]`, `#[track_caller]` | the signature, the panic strategy, the inline attribute | function attributes | `noreturn`, `nounwind`, `inlinehint`/`alwaysinline`/`noinline` |

**No TBAA.** Rust has no type-based aliasing: a program may read an `f32` through a `*const
u32`, and rustc emits no `!tbaa`. The type information LLVM gets is what the table gives:
alignment, dereferenceability, ranges, and `noalias` from references. A stronger alias model
(scopes for bodies blaze's MIR inliner builds in, as LLVM's inliner makes them from `noalias`
parameters) comes later, each fact its own measured change (Andrew, 2026-11-09).

### Unwinding

- **Calls that can unwind:** `invoke` and `invoke_indirect`, a call with an unwind edge to a
  block of kind `cleanup`, whose one parameter is the exception. An invoke ends its block; its
  result is a value of the block it returns to, whose one predecessor it is. A `call` cannot
  unwind.
- **`catch_unwind`:** the `try` intrinsic has a `catch` edge.
- **`UnwindAction::Terminate`:** a `terminate` edge, a cleanup that calls `panic_cannot_unwind`.
- **`UnwindResume`:** `resume`.
- **`panic = "abort"`:** no edges.

That is the whole model in LIR, the same on every target. How a target unwinds is the
consumer's.

### Calls and the ABI

A LIR signature is already lowered: parameters and returns are machine types, each parameter
carries its extension, `sret` or `byval(size, align)` and its fact row, and the convention is
the target's `C` (System V, Win64, AAPCS64 or Apple's) or `Wasm`. A Rust-ABI function uses its
target's C convention over its lowered signature, so code of every tier and C call each other
with no adapter, and a tier-up is one slot store. The reference for where a signature's
arguments go is LLVM's lowering of it, since release code and C interoperate through LLVM.

## Not built

### LLVM

LIR to LLVM IR directly over `vendor/llvm-sys`, in one crate that depends on blaze-lir and
llvm-sys and never on the front end. One LIR block is one LLVM block, and block parameters are
phis. `ptr` is `ptr` and `ptr_add` is `getelementptr i8`, so every address has its base. Each
fact maps by the [table](#rusts-facts-and-where-each-goes): parameter and return attributes,
load and store metadata, instruction flags, orderings, lifetime markers, branch weights. No
crate uses llvm-sys yet.

### LLVM's memory

Every C and C++ allocation of LLVM (the malloc family, `operator new` and `operator delete` in
every form) goes to blaze's allocator for the running task and is part of that task's
footprint. A block another image's allocator made (`strdup` in libSystem, then freed by LLVM)
is found by its page and handed to the system's `free`. Each LLVM compile makes its own
`Context` inside the unit's task and disposes of it before the task ends; between `summarize`
and `emit` the module is bitcode, so nothing of a context outlives its compile.

### Stencils

A copy-and-patch tier keyed on LIR ops (`Shape { op, ty, aux, srcs, dst }`; promises and hints
not in the key, so a fact never multiplies the library), each op's stencil generated through
the LLVM lowering of a one-instruction LIR function, so stencil code and LLVM code agree on
every op by construction. Target intrinsics get stencils the same way; an intrinsic with no
stencil is a typed error naming it.

### Kept LIR

No build stores LIR; the record exists for the round-trip tests. Keeping it for release units
(cross-unit imports) and for tier-up comes once measured, as does folding near-equal functions
on LIR ([icf.md](icf.md)).

## LIR is not the const evaluator

Const eval runs on the checker's typed bodies in `crates/blaze-consteval`
([consteval.md](consteval.md)), on Rust's abstract machine: allocations with provenance,
uninitialized bytes, alignment and validity checked, its errors those of the pinned nightly.
LIR has lowered all of that away: it is per target and after layout, its pointers are
addresses and its memory is bytes. A constant evaluated on LIR, or by running native code, would
accept what rustc refuses and could not say why. blaze-lir's `eval` is a reference for backend
tests and never runs in a build.

## A new IR, not lowered MIR

The options, against what LIR has to be: read by every backend with no front end; cheap to build
and to scan twice; flat, so it can be hashed and kept; one lowering, so the ABI and drops are
decided once; able to carry the facts; no heavy new dependency.

- **rustc's `rustc_codegen_ssa`** lowers MIR once through `BuilderMethods`, the ABI decided once
  by `FnAbi`, but materializes no IR to hash, keep or scan before emitting. Taken: the ABI
  decided once, and `lir::Builder`'s surface shaped like `BuilderMethods`.
- **Cranelift's CLIF**: no pointer type, a `MemFlags` of a few bits (no ranges, `noalias`,
  orderings or volatile), no target intrinsics or scalable vectors. Taken: block parameters,
  defined-by-default semantics, the op vocabulary to start from.
- **LLVM IR** carries every fact, but as the one IR every dev compile would pay for its
  in-memory IR and link LLVM; it has no stable serialization but version-bound bitcode. Taken:
  the facts' names and meaning, and the poison model for flags.
- **GCC's GIMPLE and RTL.** LIR is low GIMPLE plus the calling convention; RTL's job is each
  consumer's. Rejected: GIMPLE's type-based alias sets, which Rust's semantics forbid.
- **Zig's AIR** is the closest in form (the encoding LIR takes), but before layout and the ABI,
  so each Zig backend has its own ABI code. Rejected: the level.
- **Go's SSA.** Taken: target operations as operations of the same IR (`target_intrinsic`), and
  legalization as table-driven rewrites in one place.
- **MLIR**: a large C++ dependency whose generality costs what a fast tier can't pay. Taken:
  typed attributes per op class, and `lir::verify` per op from `Op::info`.
- **A lowered phase of MIR**: one IR fewer, but every consumer would need the front end's
  interned types, one type would carry two keys (a generic body's and an instance's), and every
  MIR pass would have to handle lowered statements.

## Risks, ranked

1. **A fact that is false is a silent miscompile.** A `noalias`, `range`, `inbounds` or `nsw`
   claimed where Rust does not guarantee it changes a correct program's behaviour only under
   optimization, and only sometimes. *Settled by:* every fact's rule being a port of one named
   rustc function for the pinned nightly; a planted-bug test per fact; real projects' release
   test suites. A release failure is bisected by masking facts (a test-only setting, never a
   fallback).
2. **Two tiers' ABIs split.** Baseline and LLVM code call each other at tier-up, so a mismatch
   corrupts values silently. *Settled by:* a blaze-sim signature generator, with cross-calls
   both ways on each architecture and OS.
3. **The stencil library grows** with target intrinsics, 256- and 512-bit operands and scalable
   registers. *Settled by:* counting them in the pinned library, generating the ones real
   projects reach, and measuring the library's bytes.
4. **Malloc routing in the process.** A block another image made, freed into blaze's allocator,
   corrupts the heap. *Settled by:* the foreign-free check and its tests on macOS and Linux.
5. **A context per LLVM compile costs its creation** and the type tables LLVM made once per
   thread before. *Settled by:* the `llvm compile` span before and after on nushell's release
   build. A context of a few hundred bytes made per function that compiles in milliseconds
   should not show.
6. **Windows unwinding** moves from the builder into the stencil glue (the guarded call).
   *Settled by:* `harness/unwinds.py` and `harness/unwindsplice.py` on a Windows ix VM, mixing
   stencil and LLVM frames in one unwind.

Gate for the LLVM path:

- **The real-project harness passes** on `-Z lir` for every project, debug and release, on
   macOS AArch64, Linux x86-64 and Windows x86-64 (ix VMs).

## Related

- [icf.md](icf.md): folding equal records.
