---
status: plan
summary: hir-ty-codegen, blaze-codegen and vendor/llvm-sys under docs/v2/plan/design/unsafe.md - where unsafe lives, what each use buys over the safe version, what is untrusted, and the tests that hold each fast path to a safe reference (2026-10-30 pass).
date: 2026-10-30
---
# Unsafe in the code generator (speed-unsafe-back-codegen)

The bar is [unsafe.md](../../design/unsafe.md): unsafe only where it buys speed or is the only
way to reach LLVM, in small modules with a safe API, every block a `// SAFETY:` with one
operation, every `unsafe fn` a `# Safety`, untrusted bytes verified before a view, a safe
reference in tests.

## Where unsafe lives

| Crate | Root | Unsafe modules | Everything else |
|---|---|---|---|
| `hir-ty-codegen` | `mod.rs`: `#![deny(unsafe_code)]` | `llvm` (LLVM's C API), `lent` | each module `#![forbid(unsafe_code)]` |
| `blaze-codegen` | `src/lib.rs`: `#![deny(unsafe_code)]` | `memo` | `summary`, `fold`, `imports` forbid; the rest is denied |
| `vendor/llvm-sys` | `src/lib.rs`: `#![forbid(unsafe_code)]` | none: declarations only | |

Inside `llvm`, unsafe is in three files; the other four forbid it:

| File | Blocks | `unsafe fn` | What |
|---|---|---|---|
| `llvm.rs` | 48 | 2 | passes kept per machine (`Kept`), diagnostic handler, command-line options, host CPU, byte GEPs, inkwell's unsafe deletes and call-site wraps, the test profile writer |
| `llvm/rebase.rs` | 85 | 8 | raw walk of address webs (instructions and constant expressions) |
| `llvm/release.rs` | 33 | 2 | use lists and call operands for summaries, dead-argument poison |
| `llvm/lower.rs`, `pgo.rs`, `sanitize.rs`, `emitted.rs` | 0 | 0 | `#![forbid(unsafe_code)]` |

This pass moved `lower.rs`'s five blocks and `pgo.rs`'s four into `llvm.rs`:
- `!prof` on a function: inkwell's safe `GlobalValue::set_metadata` is the same
  `LLVMValueAsMetadata` + `LLVMGlobalSetMetadata` pair, so the raw calls went.
- the three `i8` GEPs: `llvm::byte_offset` and `llvm::in_bounds_byte_offset`, safe because one
  scalar integer index into `i8` is valid IR for any offset. `inbounds` is a claim about the
  program being compiled, not this process; `lower.rs` keeps the comment saying why it holds.
- the test-only profile writer: `llvm::written_profile`, one op per block (it had three in one),
  the buffer owned by an inkwell `MemoryBuffer` instead of a manual dispose, and a null buffer
  an error instead of a slice of null.

## What each use buys over the safe version

- **`Kept`** (LLVM's pass builder kept per target machine, `vendor/llvm-sys/wrappers/passes.cpp`):
  the safe version is `LLVMRunPasses`, which builds the pass builder, four analysis managers,
  the standard instrumentation and the parsed pipeline on every run, under a process-wide
  lock. blaze runs passes over one small module per function, so that setup dominated
  ([speed-backend-llvm.md](speed-backend-llvm.md)).
- **`rebase`, `release`**: inkwell has no type for a constant expression and no
  `LLVMIsACallInst`, and its operand reads classify each value into an enum, one or two more
  calls into LLVM per operand. The webs `rebase` rewrites mix `ptrtoint @g` constant
  expressions with instructions; `release` reads use lists whose users may be constants.
- **Byte GEPs, deletes, call-site wraps**: inkwell marks these `unsafe` for preconditions the
  wrapper or the site checks (an `i8` element; no use left; an opcode checked first). No safe
  inkwell call does the same.
- **Diagnostic handler, host CPU, command-line options**: the only way to reach them.
- **`lent::Lent`**: hands out `&Layout` for a build's life with no refcount write per lookup
  (an `Arc` clone per lookup writes a line every thread holding it reads).
- **`memo::Memo`**: an append-only lock-free table. The safe `RwLock<HashMap>` writes the lock
  word on every read, so each worker's lookup of every function moves one cache line between
  all cores.

## Untrusted bytes

None of these crates makes a zero-copy view of bytes from the store, the remote cache or
another process, and none maps executable memory:
- template blobs: the store checks each against its key before `blob::decode*` sees it
  (`blaze-cas` `Reader::get`), and the decoders are safe, bounds-checked Rust that refuse
  short counts and lengths (`blob.rs` tests). Fuzz target `fuzz/targets/codegen_blob.rs` is
  filed with the coordinator (`v2/requests/codegen.md`, "From codegen (lead)").
- assembled `asm!` blocks: `asm::Assembled::from_bytes`, safe, fuzzed by
  `fuzz/targets/codegen_asm.rs`.
- object files LLVM writes are read back in the same process (`emitted.rs`, through `object`'s
  bounds-checked readers): not untrusted.
- `cached.rs` reads stored entries in safe code (it forbids unsafe).

## Safe references in tests

- `llvm::tests::byte_offsets_are_the_geps_of_bytes`: the wrappers give the GEPs inkwell's
  unsafe calls give (a parameter offset, a negative constant, `inbounds`), the module verifies,
  and an unpositioned builder is refused as before.
- `llvm::tests::kept_passes_do_what_passes_built_for_each_run_do`: `Kept` against
  `Module::run_passes` (LLVM's own), on four samples, every pipeline, with and without SLP,
  and a module of another context refused.
- `rebase`'s four tests: the rewritten IR for an offset address, a loop's `phi`, a `select`, and
  what is left alone.
- `pgo` tests read back the profile `written_profile` writes and the `!prof` `entry_count`
  sets.
- `lent::tests`: the list against a safe list read by index, on the heap and in a frame.
- `memo::tests::the_memo_answers_as_a_map_of_first_templates` against a `HashMap`; three
  `blaze_check` model tests of its races.

## State of the art

The bar for calling LLVM from Rust is rustc's `rustc_codegen_llvm`: one context per codegen
unit and thread, raw `&'ll Value` handles behind a typed builder, and C++ shims
(`LLVMRust*`) for what the C API lacks. This code matches it: one context per thread, raw
handles only inside `llvm`, C++ shims in `vendor/llvm-sys/wrappers`, and, unlike rustc, a
crate root that denies unsafe outside that module and a written contract for every handle
it passes.

## Left

- `byte_offset` goes through inkwell's `build_gep`, which allocates an index `Vec` and a
  `CString` for the empty name on every call. Calling `LLVMBuildGEP2` with a stack array and
  `c""` would save two allocations per address the lowering builds; it needs inkwell's
  builder position check (private) kept by hand, so it is for the speed lane to measure.
- `rebase`'s `build` iterates an `FxHashMap` keyed by `LLVMValueRef`, so two `phi`s of one block
  can get their `ptrtoint`s in either order between runs (speed-backend-safety-rebase's
  finding, with speed-backend): not a soundness question.
