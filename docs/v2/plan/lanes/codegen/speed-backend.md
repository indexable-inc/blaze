---
status: plan
summary: Speed of codegen's back end (hir-ty-codegen outside the adapter's v2 files, release summaries and imports, fold, llvm-sys) after the 2026-10-29 speed pass - the benchmark each part is held to, what changed, and what still falls short and whose it is. Detail per part is in the six speed-backend-*.md notes beside this one.
date: 2026-10-29
---
# speed-backend

The pass was split by file into six parts, each with its own notes beside this file. This
page is the index: per part, the benchmark, the change in one or two lines, and the gap left.
Nothing was built or run (principles.md); every claim below is from reading the code, and each
part names the tests and measurements the coordinator's first build should run.

| part | benchmark | before | after |
|---|---|---|---|
| IR builder, `mod.rs` ([lower](speed-backend-lower.md)) | Cranelift's `Context` reuse; Zig's self-hosted backend; copy-and-patch and TPDE for cost per instruction | 3 fresh Cranelift contexts per function; a layout lookup cloned an `Arc` (shared refcount write); variant layouts rebuilt per use; up to 17 environment reads per function; a `format!` per overflow check; v2 paid Cranelift's incremental-cache key (serialize and hash the whole function) under its own memo | per-thread contexts reused; layouts lent by reference from a per-build list; variant layouts once per variant; environment read once; panic names whole; v2 compiles without the incremental cache, v1 keeps it (`emit_cached`) |
| code and passes, `fragment.rs` and siblings ([code](speed-backend-code.md)) | Cranelift `MachBuffer` relocations; lld and mold relocation arrays; LLVM mem2reg and StackColoring | `ModuleReloc` 32 bytes each with an owned name, ELF types in a separate list searched linearly; request tables hashed twice and cloned on a hit; promote and overlap over hash maps per function | 16-byte `Relocs` rows (const-asserted) with the ELF type in the row and wide addends and namespaces in side tables; one hash and one probe per request (`HashTable`), no clone on a hit; promote and overlap on dense entity maps and bitsets, reused per thread |
| side tables and settings, `debuginfo`, `unwind`, `backend.rs`, `target.rs` ([debug](speed-backend-debug.md)) | gimli and LLVM `DwarfDebug`; Cranelift's unwind emission; per-CPU counters | about 10 locked `fetch_add`s per function on lines every core wrote; two clock reads and a boxed token per Cranelift pass; Cranelift's verifier on in every build; LSDA built in three tables and written twice | per-thread counters summed on report; no pass timing unless `BLAZE_STATS`; verifier only in tests or `BLAZE_VERIFY=1` (same bytes); LSDA written in one pass, exact size; LSE atomics on `apple-m1` |
| release LLVM, summaries and imports ([llvm](speed-backend-llvm.md)) | LLVM ThinLTO (`ModuleSummaryIndex`, `computeImportForFunction`); rustc's per-CGU ThinLTO | `PassBuilder` and pipelines built on every pass run; a global mutex map holding pre-linked bitcode; two module clones per pre-link; `decide` over `BTreeMap`s per node; `blob_key` hashed JSON | pass managers and parsed pipelines kept per target machine (new `wrappers/passes.cpp`); pre-linked module carried in the prepared function, no global lock; no module clones; `decide` over CSR columns and bitsets, parallel per importer, differential-tested against the old one; `blob_key` one BLAKE3 over fixed-width fields (domain `imports 2`) |
| fold ([fold](speed-backend-fold.md)) | LLVM MergeFunctions; lld, mold and GCC ICF | Cranelift's printer per candidate: 3 to 6 allocations and two SipHash lookups per instruction, a function-sized `String`; shared data hashed once per path (exponential in chain depth); a BLAKE3 per place constant per bucket member | a structural walk into one reused buffer, BLAKE3 of the words (domain `fold ir 1`), differential-tested against the text; shared data hashed once; targets compared whole |
| intrinsics, `asm`, x86, NEON, wasm ([intrinsics](speed-backend-intrinsics.md)) | LLVM's lowering of the same intrinsics; Cranelift's SIMD lowering | generic `simd_*` one lane at a time off wasm; NEON `tbl1` and pairwise ops 16 to 32 scalar loads, selects and stores, others a call of an assembled stub through memory; `asm!` assembled under a global lock with a SHA-256 per lookup | `simd_*` as Cranelift vectors on NEON and SSE2 where Cranelift's rules are exact; NEON `tbl1`, pairwise, saturating, rounding ops as single vector instructions; SSE2/AVX2 `min`/`max`/`pack`/`pmaddwd`/`pavg` as one instruction per 16 bytes; assembler table per thread in front of a sharded shared one |

## Where it still falls short, and what it would take

- **One code buffer per unit** (data.md): each function's bytes are still copied out of
  Cranelift's `MachBuffer` into a `Vec` of their own. It needs `Code` to borrow the kept
  context's buffer until the unit is written, which is the adapter's `convert` and the lead's
  `CompiledFunction`.
- **Dev code speed against cargo debug on ripgrep**: the two largest open shares are not in
  these paths. A v2 unit builds no `#[inline(always)]` callee in (devspeed request 6, the
  adapter's unit key), and std's non-generic code is built at `opt_level=none` (requested of
  the lead in `v2/requests/codegen.md`: a level in `Backend::Cranelift` and the level in the
  key).
- **x86-64 SIMD past SSE2** (`pshufb` in Teddy, SSE4, AVX2 shuffles): the process has one
  Cranelift ISA, built at the target's baseline. It needs a per-function ISA from
  `#[target_feature]`, filed in `v2/requests/speed.md`. Vector reductions on NEON
  (`umaxv`) need Cranelift ops of their own (vendor cranelift-codegen, also filed there).
- **Thunks of a fold** are a call and a return, not a jump: Cranelift has sibling calls only
  under its `tail` convention (`v2/requests/codegen.md` section 5).
- **Release imports** are written to bitcode and parsed again in every importer. It needs
  `emit` scheduled on the thread that ran `summarize` (`v2/requests/codegen-release.md` 3,
  the driver's).
- **v2's unwind conversion** writes and parses a whole `.eh_frame` with its CIE per function
  (`v2/unwind.rs`, the adapter's; requested by debug).
- **Scheduling**: the lead's driver runs each phase on rayon's pool, and `decide` follows it.
  On the plane (concurrency.md, "no thread waits on another's work") those become plane tasks;
  that is the lead's change, not this pass's.

## Unsafe ([unsafe.md](../../design/unsafe.md))

Unsafe in these paths is LLVM's C API (`llvm.rs`, `llvm/`), one per-thread counter list
(`backend.rs`), salsa's `to_static` (`fncache/dispatch.rs`) and the layout list `lent.rs`, the one
that buys speed: lending `&Layout` instead of cloning an `Arc` per lookup removes a shared
refcount write from every layout read. Every block is one operation with its `// SAFETY:`; every
other module forbids `unsafe_code`. `lent.rs` keeps a safe reference version in its test.
Sanitizers for the code blaze emits (ASan, TSan, MSan, SanitizerCoverage) are in the LLVM back end
(`llvm/sanitize.rs`; see [speed-backend-llvm.md](speed-backend-llvm.md), "Sanitizers").

v1-only files (`cached.rs`, `patch.rs`, `units.rs`, `stable.rs`, `later.rs`, `flight.rs`,
`counted.rs`, `watch.rs`) were left alone as crates.md replaces them with `blaze-engine`,
except where the new `Relocs` rows required `cached.rs` to follow (same bytes in the store).
