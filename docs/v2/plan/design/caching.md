---
status: plan
summary: blaze v2's cache keys name content and relative layout only, never where a checkout, home, target directory, registry or toolchain is; every layer (toolchain, load, macro expansions, build scripts, codegen, the link manifest) is looked up before the work it saves, and a hit is what a rebuild would make.
date: 2026-10-30
---
# Caching: keys of content, never of places

Lane warm-store. A second checkout of the same commit, with a fresh home and an empty target
directory but a full store, has to build in about the time of fetching and linking. On the
2026-10-29 gate it took 549 s on macOS (cold 1017 s, cargo 156 s). Measured on the Mac with the
cargo app and ripgrep, the reasons were all of one kind: keys that name where something is.

| what missed | the place in its key | cost in the warm-store run |
|---|---|---|
| every function of every non-std crate (186,590 of 228,285 compiled again) | the checkout, through cargo's `[env]` `CARGO_RUSTC_CURRENT_DIR` (`relative = true`), which every crate gets, a registry one too | most of codegen |
| every build-script and proc-macro build (73 steps, each a process) | `sysroot_src` under `BLAZE_HOME`, dependencies' `OUT_DIR`s under the target directory | the compile-time critical path |
| the build manifest (load + link only) | the checkout, outputs, `BLAZE_LIBRARY` under `BLAZE_HOME`, std crate roots under `BLAZE_HOME` | everything after the load |
| the toolchain | kept per `BLAZE_HOME` | one 123 MB download |
| wasm proc-macro expansions | not kept at all | 763 expansions |

## The rules

1. **A key is built of typed fields only.** `blaze-cas`'s key builder takes content hashes,
   stable names and `RelPath`s (a path relative to a declared root: the checkout, an out-dir,
   the registry, the toolchain, the library). No constructor takes a `Path`, `PathBuf` or
   `OsStr`, so an absolute path in a key does not compile. Every key moves onto it: unit and
   crate content ids, function and instance keys, the manifest, build-script runs and builds,
   macro expansions, frozen maps, std units.
2. **Roots are mapped at the edges.** Inside blaze the checkout is `/blaze/checkout`, out-dirs
   `/blaze/out/<unit>`, the registry `/blaze/registry`, the toolchain `/blaze/toolchain`: in
   the VFS, file ids, spans, `file!()` and panic `Location` (relative to the workspace, as
   cargo gives rustc paths), and `include!`. Real paths appear only where a person or the OS
   sees them: diagnostics, debug info (written relative and remapped at link, as
   `--remap-path-prefix` does), the sandbox's mounts. The sandbox mounts each build script
   and proc macro at the canonical paths, so their outputs are path-independent by
   construction, with no rewriting of their text. Each canonicalization is proven not to change
   program output by a differential test against rustc (rule superset-of-rust).
3. **Identity is deterministic.** Function and instance keys come from content-derived
   identities (`blaze-item-hash`), never process ids, `FileId` or `Crate` numbering, interning
   order, hash-map order, worker or scratch directories or times. A test computes every key in
   two fresh processes, two homes and two paths and asserts they are equal.
   `blaze cache explain <key|fn>` prints a key's fields, so a miss is diffed, not guessed.
4. **One cache, every layer, each asked before the work.** Toolchain and std once per machine
   by content; load results (front-end queries pure in their inputs); macro expansions (wasm
   macros keyed by the module's content, the input and what they read through WASI, which the
   sandbox records); build-script runs by what the script was built from plus its recorded
   reads; builds of scripts and macros; codegen per function; the link manifest.
5. **A hit is a rebuild.** Every layer has a check mode (`BLAZE_MANIFEST=check`'s kind) that
   builds anyway and fails when the result differs, run in CI. There is no fallback: a key that
   cannot be computed is an error. The cache-key review rule holds for each key (source, cfg,
   features, target, toolchain, layout). Until rule 2 is in, an output that names a place (debug
   info naming the checkout, an `env!("OUT_DIR")`) is kept under its exact places as well, and
   only a place-free output is kept under the place-free key.
6. **The gate shows it.** `harness/bench.py`'s warm-store run reports functions compiled and
   reused, compile-time steps kept, and whether the manifest was used;
   `harness/warm_store.py` builds two checkouts at different paths with two homes against one
   store and fails when the second compiles anything or makes any step again.
   `harness/keys.py` adds two homes and two machines (different `HOME` and registry) to its
   cases, and every single-input change still misses.

## What landed on main first

On main, before v2 merges, the same faults were fixed inside today's keys (text rewriting of
places, as `script::Places` already did for runs), so the gate improves now. The fixes and their
pull requests are in the lane's report, `/tmp/blaze-lanes/v2-warm-store.md`. The v2 crates
take rules 1 to 6 whole.
