---
status: plan
summary: Speed of release builds' LLVM path (summaries, import decisions, the pass manager, pre-link and compile per function) against LLVM's ThinLTO and rustc's per-CGU ThinLTO - what each cost before and after the 2026-10-29 pass, and what is still missing.
date: 2026-10-29
---
# Release builds' LLVM path (speed-backend-llvm)

A release build runs LLVM once per function: `v2/release.rs` `summarize` (lower, ThinLTO's
pre-link pipeline, read the summary), then `blaze_codegen::imports::{close, decide}` over every
summary, then `v2/release.rs` `emit` (link the imports in, apply the whole-program facts,
post-link pipeline, emit). The benchmarks are LLVM's ThinLTO thin link
(`ModuleSummaryIndex`, `computeImportForFunction`, `computeDeadSymbols`, function attribute
propagation) and rustc's ThinLTO per codegen unit. For the speed of the code itself the
benchmark is rustc's release output, which the post-link pipelines follow.

## Summaries and their keys (`blaze-codegen/src/summary.rs`)

- Benchmark: ThinLTO keeps summaries in a `ModuleSummaryIndex` keyed by GUID (a 64-bit hash
  of the name) and writes it as bitcode records: fixed-width fields, no text.
- Before: `Imports::blob_key` serialized the whole `Imports` to JSON (`serde_json::to_vec`,
  an allocation and a formatted number per field) and hashed the text. `Arg` was 32 bytes
  (`u128` value), and nothing stated the size of `Arg`, `Attrs` or `Call`.
- Now: `blob_key` lays every field out as fixed-width little-endian bytes in a buffer the
  thread reuses (no allocation once it has grown), and runs one BLAKE3 over it. Each list
  carries its length, so two different `Imports` never give the same bytes. The domain went
  to `"blaze v2 imports 2"` because the hashed bytes changed. `Arg` is 16 bytes (an `Arg::Int`
  is at most 64 bits; `llvm/release.rs` never made a wider one), `Attrs` is 6 bytes and packs
  into one `u8` (`Attrs::bits`), and `Call` is 64 bytes. All three have `const` size
  assertions and tests.
- Still missing: the driver (`blaze-codegen/src/driver.rs`, lane codegen's) stores summaries
  as JSON. A fixed-width encoding like `blob_key`'s would save the parse on a warm build. That
  is the parent's file.

## Import decisions (`blaze-codegen/src/imports.rs`)

- Benchmark: `computeImportForFunction` uses a worklist per importer over the index's call
  edges, with GUID-keyed `DenseMap`s for what each importer has seen and taken. The thin link
  is serial per module but cheap per edge.
- Before: `decide` ran one pass over `BTreeMap<InstanceKey, _>` per importer: a `BTreeSet`
  for `seen` and `taken`, a tree lookup of 32-byte keys per edge, `BTreeMap`s for each
  importer's callees and dead parameters, and `Vec<Vec<usize>>` lists for the SCCs. That is
  O(log n) 32-byte compares per edge, and several allocations per importer and per node. It
  was serial.
- Now: the summaries become columns once (`Graph`). Nodes are numbered in key order, so the
  answers do not depend on the map's history. Calls are a CSR list of node numbers, unused
  parameters are a CSR list, and "every caller has a summary" is a bitset. Keys map to nodes
  through open addressing on the key's first 8 bytes, which are already a content hash and
  are never hashed again (one probe and one 32-byte compare as a rule). Attributes are
  propagated over iterative Tarjan SCCs on the CSR edges, as `u8` bit sets. Agreeing
  constants are per-parameter slots over closed callees. Importers are decided in parallel
  (rayon, `with_min_len(64)`). Each worker reuses a scratch of generation-marked `seen` and
  `listed` arrays, so nothing is cleared or allocated per importer once it has grown. The
  only allocations per importer are its output `Imports`.
- Determinism: node order is key order, every list is sorted, and the output is collected in
  node order. The old `decide` is kept as a test model (`tests::model`). A differential test
  over 2,000 seeded random builds checks that the two give the same answer, errors included:
  keys sharing their first 8 bytes, callees without summaries, cycles, external and addressed
  functions, and inconsistent summaries. A second test checks the SCCs against the model's.
  Every existing test keeps its answers.

## The pass manager (`vendor/llvm-sys/wrappers/passes.cpp`, `llvm.rs`)

- Benchmark: rustc and `opt` build one `PassBuilder` and its analysis managers per module and
  parse the pipeline once. Their modules are large, so that cost is amortized.
- Before: every pass run went through `LLVMRunPasses`. Each call built a `PassBuilder`, four
  analysis managers with every analysis registered, `StandardInstrumentations` (which looks
  up timer groups under a process-wide lock), and parsed the pipeline text. blaze does this
  up to five times per function (pre-link, `cgscc(function-attrs)`, post-link, rebase,
  `elim-avail-extern`), on every core at once.
- Now: `LLVMBlazeRunPasses` keeps all of that per target machine and per SLP setting, on the
  thread that owns the machine (`llvm.rs` `Machinery`, beside each `TargetMachine` in the
  per-thread `Machine`). Parsed pipelines are cached by their text. After each run, every
  analysis result is cleared, so no module sees what another left. Pipelines naming the
  inliner are parsed again for each module, because a stand-alone CGSCC `InlinerPass` keeps
  the advisor of the first module it ran on. A module of another context is refused, not
  run. The test `kept_passes_do_what_passes_built_for_each_run_do` compares the printed IR of
  the kept passes with `LLVMRunPasses` over every pipeline blaze runs, both SLP settings, and
  samples run after one another.

## Pre-link (`llvm/release.rs` `prelink`)

- Before: there were two whole-module clones per function. One copy ran
  `cgscc(function-attrs)` under optimistic callee attributes. The other stripped the line
  table for the export. `own` and `export` were two `Vec<u8>`s even when they held the same
  bytes (`own.clone()`).
- Now: `own` is written first, then the export, by stripping the debug info in place. Then
  `optimistic` takes the module by value and runs `function-attrs` on it, since nothing is
  written from it after. There are no clones. `own` and `export` are `Arc<[u8]>`s that share
  one buffer when there is no line table. `Export::bitcode` is that same `Arc`, so the export
  costs no copy. The line-table builder is dropped before the module is changed, as before.

## Pre-linked module from `summarize` to `emit` (`v2/release.rs`)

- Before: `KEPT` was one process-wide `Mutex<FxHashMap<InstanceKey, Arc<Vec<u8>>>>`, taken
  once in every `summarize` and once in every `emit` on every core, with each key hashed again.
- Now: the module stays in the function itself (`Function::release`, a `Kept`). The trait
  gives `summarize` the prepared function mutably, and `emit` takes it out. There is no lock,
  no table and no hash. `function.hint` (the `#[inline]` the adapter now reads) goes to the
  pre-link pipeline, the summary and the export. Before, all three were told `Hint::None`.

## What is still missing, and what it would take

- Each importer still parses each import's bitcode (`llvm.rs` `link`) and its own module
  (`compile`). ThinLTO does the same: it lazy-loads each source module once per importing
  module, and in a function-per-module design that is once per importer. Beating it means
  keeping parsed modules in the thread's context and cloning from them (`CloneModule` is
  cheaper than a bitcode parse). That needs `emit` to run on the thread that ran
  `summarize`, which is `v2/requests/codegen-release.md` 3 (the driver's).
- v1's tiering and release (`llvm.rs` `compiled`, `imports.rs`, `tier.rs`) get the kept
  passes too, since they go through the same `Machine`. Nothing else in them was changed.

## Sanitizers (`llvm/sanitize.rs`; speed-backend-sanitize, 2026-10-29)

S2 of `v2/requests/codegen.md` and F2 of `v2/requests/safety.md`: rustc's `-Zsanitizer=address,
thread,memory` (with `-Zsanitizer-recover` and `-Zsanitizer-memory-track-origins`) and
SanitizerCoverage on the release path. The set is data, `Spec::sanitize`
(`blaze_codegen::settings::Sanitize`), the same for every function and hashed into the
settings, so the unit and template keys follow it (requested of the lead, codegen.md Z1).
rustc has no SanitizerCoverage flag of its own: it gets it from `-Cpasses=sancov-module` with
`-Cllvm-args=-sanitizer-coverage-*`, which is what cargo-fuzz passes, and cargo reads those
into `Sanitize::coverage` (`v2/requests/cargo.md`, same date).

- **Attributes.** `prelink` gives the function `sanitize_address`, `sanitize_memory`,
  `sanitize_thread`, in rustc's order (`sanitize_attrs`), for each sanitizer on that its
  `#[sanitize(.. = "off")]` (inherited from impl, module and crate, as rustc does) leaves on.
  This happens at lowering, so the pre-link and post-link pipelines see them as rustc's do: LLVM
  builds in only what has the same ones. A function named `__sanitizer_*`, which
  SanitizerCoverage skips by name in rustc's build, gets `nosanitize_coverage`, because here every
  function is named `blaze.own`. The set comes from the front end through the adapter
  (`Function::unsanitized`, codegen.md Z3; the query is in `v2/requests/db.md`).
- **Passes.** `compile` instruments the module after the post-link pipeline, `rebase` and
  `elim-avail-extern`, through `LLVMBlazeRunSanitizers` (`vendor/llvm-sys/wrappers/passes.cpp`)
  on the kept analysis managers. The order is clang's (`addSanitizers`): SanitizerCoverage,
  then MemorySanitizer, ThreadSanitizer, AddressSanitizer. That way coverage traces none of
  ASan's shadow checks, which F2 item 1 needs. rustc's `-Cpasses=sancov-module` would run it
  after them. Each sanitizer gets rustc's options (`LLVMRustOptimize`): MSan with eager checks;
  ASan with use-after-scope, use-after-return at runtime, globals GC, and ODR indicators except
  on Windows. SanitizerCoverage's options are built from the flags the way its `cl::opt`s build
  them (`getOptions`, `OverrideFromCL`), per module rather than per process. The empty set runs
  nothing and changes no byte (test `nothing_asked_changes_nothing`).
- **Where in the pipeline.** Instrumenting the optimized code is rustc's order for one module
  without ThinLTO (`-Ccodegen-units=1`), and clang's. rustc's ThinLTO instead instruments at the
  end of pre-link and then optimizes the instrumented code after importing. Here it would put
  each callee's coverage arrays into its importers' code, which a function-per-module build
  can't relocate. The facts in summaries are those of uninstrumented code. So a caller may take
  a callee to write nothing that writes only shadow memory or counters, and LLVM may then merge
  two calls of it. That changes how often a counter counts, never what the program does.
- **Reading it back.** `emitted::read_covered` places what the code refers to of
  `__sancov_cntrs`, `__sancov_bools` and `__sancov_guards` as relocations in
  `sanitize::COVERAGE` (`u32::MAX - 2`, array 0, 1 or 2). It reads `__sancov_pcs` into
  offsets in the function, with the flags as LLVM wrote them (the entry block 1). Then
  `v2/release.rs` sets `CompiledFunction::coverage` (codegen.md Z2), and the link lays every
  function's arrays out together, builds one PC table and calls the init functions
  (`v2/requests/link.md`, same date). `__sancov_cfs` and COFF objects with coverage stop with
  an error.
- **What is dropped and remade.** Only `blaze.own`'s section is read. So the module
  constructors (`asan.module_ctor` and the rest), ASan's global descriptors and MSan's
  `__msan_track_origins`/`__msan_keep_going` stay behind, and the link makes what rustc gets
  from them, once for the program (link.md 1-5). The thread-locals MSan and
  SanitizerCoverage read of the runtime are declared initial-exec, which the reader has no
  relocations for. They are made general-dynamic, the same values reached through
  `__tls_get_addr` or TLS descriptors.
- **Refused.** MemorySanitizer off Linux, FreeBSD and NetBSD (LLVM would abort), and the
  Cranelift back end with any sanitizer (codegen.md Z3.1). cargo refuses the sanitizers blaze
  does not instrument.
- **`return_address`.** `Opcode::GetReturnAddress` lowers to `llvm.returnaddress(i32 0)`, as
  rustc's intrinsic does (F2 item 2). `mod.rs` still has to emit the opcode for the intrinsic
  (`v2/requests/speed-backend.md`, same date).
- **Not yet as rustc does.** Lowering emits no `llvm.lifetime` markers, which rustc emits
  under ASan and MSan even at `-O0`. So ASan's use-after-scope finds nothing here until lowering
  says where each slot's life ends. Cranelift's IR does not carry that; it has to come from
  MIR's `StorageLive`/`StorageDead`. At level 0 no always-inliner runs, which rustc's `-O0`
  pipeline has (as before this change).
