---
status: plan
summary: blaze v2: the bigger changes for the fastest compile times: reachability-only codegen, a persistent trait cache, the nightly's own solver, proc macros on our own compiler, tiered compilation and idle cores always working, lazy per-function compilation, copy-and-patch codegen, no link and no rewrite for dev runs, one global cache, semantic edits and declared intent, shared generic code, exact test selection, hash-consing, premapped dependencies, scheduling from history, PGO and BOLT.
date: 2026-10-29
---
# The fastest compile times

[model.md](model.md) and the lanes make every step incremental. These changes go further: they
remove steps entirely, or make the ones left an order of magnitude cheaper. Lane `fastcompile`
builds them ([building.md](../process/building.md) section 18), asking the owning lanes for
hooks.

## Compile less

1. **Lazy compilation, per function.** A dev run or test compiles only the functions that
   execute. Every other function is a stub that asks the daemon for its code on first call,
   gets it patched in, and jumps. Most of a binary never runs in one test. The live runtime
   (`blaze-live`, lane link) already patches functions into a running process; this makes that
   the normal path for dev and test runs. Release builds and `blaze build` outputs are compiled
   whole, as rustc does.
2. **Shared code across generic instances.** Instances whose type arguments have the same
   layout and the same trait-method resolutions share one compiled body. Rust doesn't
   guarantee distinct function addresses for them. `TypeId`, `type_name`, vtables and anything
   else whose result the program can observe as different stay per instance. A static in a
   generic function is already one item for every instance, so sharing changes nothing about
   it. MIR-level
   analysis decides which instances can share (lane db, codegen).
3. **Exact test selection.** Each test run records the code hashes of the functions it
   executed. After an edit, only tests that executed a changed function, or read a changed
   input, run again. The rest report their cached result, marked as cached (lane usercode).

## Compile cheaper

4. **Copy-and-patch codegen for dev builds.** At blaze's build time, LLVM compiles one stencil
   per MIR operation and operand shape, with holes for constants, offsets and jump targets.
   Compiling a function is then copying stencils and patching holes, roughly 10 to 100 times
   faster than cranelift. The code is close to cranelift's for straight-line code. Hot
   functions, found by the live runtime's counters, are recompiled with cranelift or LLVM and
   patched in. Its output is checked against cranelift's on the corpus, byte for byte in
   behavior.
5. **Hash-consing of query values.** Equal values are stored once, so comparing two is
   comparing IDs, and a value's fingerprint is computed once when it is interned. Early cutoff
   no longer hashes whole values (lane db, format).

## Don't do it at all

6. **No link and no rewrite for dev runs.** The dev executable is written once per content
   key to cargo's path and started by the kernel, so the program sees itself as its own
   executable (`current_exe`, backtraces) as under rustc. Its start hook lays the daemon's code
   memory over its code before `main`: code pages are mapped twice from one memory object,
   writable in the daemon and executable in the process, so a patch is a store with no
   `mprotect`, no link and no file write. Fences make the change safe for running threads: the
   old function stays valid until no thread is in it (plane task boundaries,
   [concurrency.md](concurrency.md)). See [lanes/fastcompile/inproc.md](../lanes/fastcompile/inproc.md).
7. **Dependencies as one premapped image.** Everything below the workspace members is
   prelinked once into one shared image, keyed by the dependency graph's hash and mapped by
   every dev run, so a dev link covers only the workspace's own code.
8. **One global cache.** Results that depend only on public inputs are computed once for
   everyone and fetched instead of built:
   - crates.io crate versions, std, sandboxed proc-macro outputs and build-script outputs;
   - LLVM-optimized functions, keyed by MIR hash, target and settings.

   The remote tier verifies every fetched value by its content hash and serves a cold build of
   the dependencies as a download (lanes store, ci).

## Let the users help

9. **Semantic edits.** The agent API takes edits by meaning: replace the body of `f`, add a
   field to `S`, rename `x`. Each edit is applied to the flat syntax directly and names exactly
   what changed. There is no diff and no reparse, and invalidation starts from the named item.
   Text edits keep working.
10. **Declared intent.** An agent can say what comes next: "I will run test T" or "here is the
    edit I am about to save". The daemon compiles only what T needs, starts speculatively on
    the announced edit at low priority, and drops that work if the edit doesn't come.

## Tier up, and never sit idle

13. **Tiered compilation for dev and test runs**:
    - **Tier 0, copy-and-patch.** Every function starts here, on first call (changes 1 and 4),
      with counters on calls and loop back-edges.
    - **Tier 1, cranelift.** Warm functions are recompiled in the background and patched in
      (change 6).
    - **Tier 2, LLVM at full optimization**, using the profile the counters gathered (branch
      weights, hot call sites), for hot functions.
    - **On-stack replacement at loop headers**, so a long-running test moves to faster code
      partway through.

    No user picks a back end; the profile does, per function. Tiers change only optimization.
    Overflow checks, debug assertions and `cfg` come from the chosen profile as under cargo, so
    a correct program behaves the same at every tier.
14. **Idle cores always work.** With no foreground request, the daemon spends idle cores, at
    the lowest OS priority (`SCHED_IDLE`, background QoS on macOS) and under memory admission,
    on:
    - higher tiers for functions past runs showed hot, hottest first, stored by code hash and
      profile, so the next run starts with them;
    - the release build of what changed, so `blaze build --release` is usually already done;
    - the dependency image (change 7) and global-cache fetches (change 8) for branches likely to
      be built.

    A foreground request preempts all of it at once. `blaze build` outputs stay whole,
    ahead-of-time and byte-identical; tiering applies only to processes the daemon runs.

15. **Profiles follow a function across versions.** Profiles are keyed by identity, not by
    code hash:
    - **Identity:** crate, module path and name; for a method, the impl's self type and trait;
      for an instance, that plus its type arguments by identity. Renames and moves keep it,
      through lane items' rename and move tracking.
    - **Version chain:** each identity keeps its chain of code hashes, with each version's
      profile (call and loop counts, branch weights, tier reached).

    What that gives:
    - **An edited function starts at its old tier.** Its new version inherits its
      predecessor's hotness, and idle cores compile it straight to the tier the old version
      reached, before the next test runs.
    - **Branch weights carry over.** They map onto the new code by aligning basic blocks:
      unchanged blocks keep their weights, and new ones get weights inferred from flow
      conservation (stale-profile matching, as LLVM's `-salvage-stale-profile` and BOLT's
      stale-profile inference do).
    - **The likely tests are compiled ahead.** Each test's last run recorded the identities it
      executed. After an edit, the tests that will rerun (change 3) tell the daemon which
      functions to compile at which tier, hottest first, before anyone asks.
    - **New functions borrow a guess.** A function with no history takes one from its callers'
      hotness.

    Profiles only choose tiers and optimizations, never meaning. A wrong guess costs background
    compile time, and the next run's counters correct it.

## Less work in every build

16. **Compile only what the binary reaches, release builds too.** Before any codegen, a
    reachability pass runs from the roots over the whole crate graph:
    - **Roots:** `main`, the tests, exported symbols, `#[used]` statics, `#[no_mangle]` and
      `export_name` items, and dynamic-library exports.
    - **Edges:** calls, function pointers, vtables, statics, and items named from `asm!` or
      linked by name.

    Only reached functions are compiled. A dependency's public functions that nothing reaches
    are never compiled, though rustc compiles them all. Lazy compilation (change 1) is the dev
    version of this. Linking by name, `#[used]` and `-C link-dead-code` keep rustc's meaning.
17. **A persistent trait-solver cache.** Solved goals in canonical form (goal, param env,
    solver mode, and the solver's version) are pure functions of their inputs. They are stored
    by hash and reused by every crate and every later build. Cycle results and goals whose
    answer depends on inference variables stay per query, as the solver's own global cache
    requires.
18. **The trait solver and `rustc_type_ir` come from the pinned nightly.** They are taken from
    `compiler/rustc_next_trait_solver` and `compiler/rustc_type_ir` at the rustc commit of
    `harness/toolchain`, not rust-analyzer's crates.io release with backports. Our changes
    (`u32`-indexed tables, SoA) are a patch series on top. `tools/v2/solver_sync.py` replays
    them on every daily toolchain bump, so trait resolution is rustc's at the pinned nightly
    by construction.
19. **JIT for const eval.** Const evaluation goes through the tiers of change 13: the MIR
    evaluator first, then copy-and-patch for hot const functions. The const-eval rules (UB
    checks, limits, `const_eval_select`) are kept exactly, in both tiers.
20. **Proc macros run on our own compiler, in process, isolated by construction.** A proc
    macro is Rust, so blaze compiles it with its own front end and runs it on the MIR
    evaluator and the copy-and-patch tier, the same machinery as const eval. It's the real
    macro's code, so its output is exactly the macro's, with no hand-written fast paths.
    Isolation comes from the runtime:
    - the evaluator checks every access;
    - the compiled tiers bound memory the way wasm engines do;
    - the only way out is the proc-macro bridge's shims, plus traced reads of files the
      sandbox allows;
    - anything else (a raw syscall, FFI to an unknown symbol) stops with an error naming the
      macro.

    It replaces spawning a sandboxed process per macro server, which keeps sandbox-everything
    with nothing outside the sandbox. Outputs are cached by input tokens and macro version.
21. **Generic instances shared across the workspace.** An instance is compiled once per
    workspace, keyed by its instance key, and every crate that uses it links to that copy:
    rustc's `-Zshare-generics`, always on. Symbol names and visibility follow what rustc does
    with share-generics, so linking by name and dynamic libraries behave the same.
22. **Debug info on demand.** Builds emit what backtraces and panics need (line tables and
    symbols). Full DWARF (types, variables, scopes) is generated from MIR and the debug side
    table when a debugger attaches, `blaze build` is asked for it, or the profile's `debug`
    setting requires it in the output file. An output file always has what its profile says.
23. **The critical path runs on performance cores.** Critical-path tasks run at the highest
    QoS (`QOS_CLASS_USER_INTERACTIVE` on macOS, high priority with the performance cores'
    affinity on hybrid Intel), and background tiers and prefetching at the lowest, so the
    operating system places them on the right cores.

24. **Dev builds carry the least for the linker.**
    - **Symbols:** symbols nobody links by name get short hash-based names. The demangled
      names go in the debug side table, for backtraces, debuggers and profilers.
    - **Debug info:** split and compressed (DWARF 5 `.dwo` and compressed sections, or the
      Mach-O and PDB equivalents), so the linker never copies it.
    - Release builds, exported and `#[no_mangle]` symbols, and dynamic-library interfaces keep
      rustc's v0 mangling.

## Make blaze itself faster

11. **Scheduling from history.** Each task's past duration and peak memory, by query kind and
    key, feed the plane's critical-path ordering and memory admission, which today use
    estimates.
12. **blaze built with PGO and BOLT** on profiles of its own daemon doing real builds
    (`tools/pgo`), every release.
