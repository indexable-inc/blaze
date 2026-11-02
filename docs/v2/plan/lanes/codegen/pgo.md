---
status: plan
summary: PGO in v2's LLVM back end (fastcompile-sched 3) - `Pgo::Generate` as LLVM's IR instrumentation and `Pgo::Use` as profile use per function, how a function compiled alone in its module gets a name its counts are kept under, what is built (`crates/hir-ty-codegen/llvm/pgo.rs`) and what the parent, cargo and the link still have to add.
date: 2026-10-29
---
# PGO in the LLVM back end (codegen-fc-pgo)

`Spec::pgo` (`blaze_codegen::settings::Pgo`) is `None`, `Generate` (rustc's
`-Cprofile-generate`) or `Use(id)` (`-Cprofile-use`, `id` the content ID of the `.profdata`).
Only LLVM units take one: `check_spec` (`v2/backend.rs`) refuses a Cranelift or stencil unit
with `pgo` other than `None`, naming the unit and its back end.

## How rustc does it

`LLVMRustOptimize` builds its `PassBuilder` with `PGOOptions`: `IRInstr` with `PGOGenPath`
(`<dir>/default_%m.profraw`) or `IRUse` with `PGOUsePath`, and passes LLVM
`-disable-preinline` for both (`llvm_util::configure_llvm`). LLVM runs the passes
in its pre-link pipelines; the profile runtime (`profiler_builtins`) is linked in and
`__llvm_profile_runtime` forced (`-u` on Linux).

## What blaze does differently

blaze compiles one function per module, under the one name `blaze.own`, so the name LLVM keys
counts by would be the same for every function. For the pre-link pipeline only, the function is
renamed to its PGO name, `blaze.pgo.<32 hex>` of its item key and type arguments
(`pgo::name`), and given its own name back after. The name is stable across edits of the body,
so an edited function finds its record with another hash: a mismatch, which LLVM warns of, and
which a diagnostic handler of the wrapper counts (`LLVMBlazeTakeMismatched`).

- Pipelines: a `Machinery` per (level, PGO) on each thread (`Machine::with_pgo`). At `-O2` and
  `-O3` PGO's passes run inside `thinlto-pre-link`, where LLVM puts them; at `-O0` and `-O1`
  they run alone after the level's passes (`LLVMBlazeRunPgo`, LLVM's
  `addPGOInstrPassesForO0`). The post-link pipeline is always built without PGO, so nothing is
  instrumented or annotated twice.
- Use: the profile is given to the process once per build (`pgo::give`, checked against its
  content ID) and read by LLVM from an in-memory file system holding it alone: no file is read
  per function.
- Generate: every function writes `default_%m.profraw` (honouring `LLVM_PROFILE_FILE` at run
  time, as the runtime does); value profiling is off (`-disable-vp`: indirect call targets are
  requests, never names, so a profile of them would name nothing). Off Linux the module declares
  `__llvm_profile_runtime` so LLVM adds no runtime-hook function to each module. Instrumented
  bodies are not importable. The object's counters section (`__llvm_prf_cnts`, `.lprfc$M`) is
  read back as relocations at `pgo::COUNTER_RELOCS` index 0, and the record (`__profd_`: name
  MD5, hash, counter count) and `__llvm_profile_raw_version` from the module (`pgo::counted`).

## What is missing, and whose

- Parent (`blaze-codegen`): done (codegen-rq-built, 2026-10-29). `CompiledFunction::profile`
  is `Profiled::Counted(ProfileRecord)` for an instrumented function, its code's counters
  `Target::ProfileCounters`, and `Profiled::Used { mismatched }` for one built with a profile,
  which `Stats::profile_mismatched` counts, compiled or from the store. All of it is in the
  function's content ID (`v2/release.rs::profiled`). `v2.rs` re-exports
  `release::give_profile`, `profile_id` and `pgo::Profile`.
- Cargo: parse `-Cprofile-generate[=dir]` and `-Cprofile-use=path` into `Policy::pgo` (it
  refuses both today, `blaze-cargo/src/plan.rs`), store the `.profdata` by `profile_id` and call
  `give_profile` before the build compiles.
- Link (sched 4): the profile runtime, `-u __llvm_profile_runtime` (or its Mach-O and COFF
  forms), and from each function's record: the `__llvm_prf_data` records, `__llvm_prf_names`,
  the counters laid out in `__llvm_prf_cnts`, `__llvm_profile_raw_version` and
  `__llvm_profile_filename` (`<dir>/default_%m.profraw`).

## Known gaps

- LLVM does not warn of a mismatch for a comdat or weak function
  (`-no-pgo-warn-mismatch-comdat-weak`), so those are not counted.
- The `check_spec` refusal has no test yet (`v2/tests.rs` is not this lane's).
- None of this has been built or run: the tests are in `llvm/pgo.rs`.

## Speed of light

The cost PGO adds per function is the work LLVM's passes do (instrumentation or reading the
function's record) plus one header parse of the indexed profile per module, since
`PGOInstrumentationUse` opens its reader on every run. A reader kept per thread, handed to the
pass, would remove the parse; it needs a pass of the wrapper's own, as LLVM's takes a path. The
rename costs two symbol-table updates. Nothing is copied per function: the profile's bytes are
shared by every thread's passes.
