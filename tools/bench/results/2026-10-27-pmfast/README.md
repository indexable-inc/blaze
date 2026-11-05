# Proc macros as wasm: the machine code kept path-free, compiled off the load (lane pmfast)

ix VM, arm64, 16 vCPU, cranelift, the flake's nightly. The corpus is one binary using the
macros of serde (Serialize, Deserialize), clap (Parser), tokio (main), thiserror (Error),
async-trait, pin-project, derive_more (Display) and strum (EnumString), with serde_json:
8 proc-macro modules. Each run is one `blaze <ws> --own-target --crate pmcorpus --out ..
--exe ..` (the build `blaze build`'s daemon makes) with `BLAZE_CACHE` set to the
arm's own store and `BLAZE_STATS=1`, timed whole. native is `BLAZE_PROC_MACRO_WASM=0`
(blaze builds the macro crates as native libraries for its server).

States: **cold** an empty store and target; **again** the next run with nothing changed;
**edit** a changed string in `main.rs`'s body; then **again** after it.

## Before and after, wall seconds

base is main at 855f615913; patched is this branch. One run of each, in this order.

| arm | cold | again | edit | again |
|---|---|---|---|---|
| wasm, base | 80.05 | 49.94 | 5.18 | 2.90 |
| wasm, patched | 48.09 | 4.88 | 4.42 | 1.92 |
| native, base | 46.07 | 14.47 | 3.98 | 1.99 |
| native, patched | 43.12 | 15.19 | 3.76 | 2.03 |

## wasm against native, alternating (`abab.sh`)

Per round, each arm: cold, again, then three times edit and again; three rounds, the arms
taking turns so that neither has the machine's later state. Medians of the samples (cold 3
each, again-after-cold 3 each, edit and steady again 9 each).

| arm | cold | again | edit | again |
|---|---|---|---|---|
| wasm (all of this branch) | 46.08 (44.83, 47.25, 46.08) | 5.34 | 4.05 | 1.89 |
| native | 45.17 (45.17, 42.58, 46.16) | 13.38 | 3.99 | 2.02 |

The same after the branch was rebased on main (63aaa237db), two rounds; edit and steady again
are medians of 6:

| arm | cold | again | edit | again |
|---|---|---|---|---|
| wasm | 44.28, 46.67 | 4.15, 4.56 | 3.4 | 1.23 |
| native | 42.63, 45.73 | 15.43, 15.95 | 3.5 | 1.24 |

## Expansions, wasm against native, byte for byte

`blaze-engine proc-macro-wasm <module> --macro M --input F --runs 20 --native <lib>.so --server
blaze-proc-macro-srv`, the module and the library both blaze's builds of the crate from the
corpus's cold runs, no `.cwasm` and no store: milliseconds. Every output was the same text.

| macro | wasm compile | wasm first | wasm warm | native start | native first | native warm |
|---|---|---|---|---|---|---|
| serde Serialize | 792 | 1.31 | 1.04 | 15.9 | 3.66 | 1.98 |
| serde Deserialize | 656 | 4.29 | 3.98 | 1.7 | 10.03 | 8.19 |
| clap Parser | 894 | 4.21 | 3.86 | 16.4 | 9.92 | 8.09 |
| thiserror Error | 592 | 1.21 | 0.81 | 15.2 | 3.32 | 1.62 |
| async_trait | 622 | 1.06 | 0.78 | 16.9 | 2.99 | 1.29 |
| pin_project | 477 | 0.27 | 0.14 | 13.3 | 2.21 | 0.67 |
| derive_more Display | 690 | 0.85 | 0.62 | 24.6 | 2.86 | 1.12 |
| strum EnumString | 597 | 0.89 | 0.69 | 15.3 | 3.56 | 1.66 |
| tokio main | 523 | 0.55 | 0.37 | 13.9 | 2.75 | 1.12 |

One compile of serde_derive's module is 0.9 s of wall time and 5.5 s of CPU (`time`): the 8
modules of a cold build take about 44 CPU seconds from its own units, which is the cold gap.
`BLAZE_WASM_OPT=none` (an experiment's `cranelift_opt_level(OptLevel::None)`) compiled and
expanded within noise of `Speed`; compiling on a rayon pool of `nice 10` threads gave cold 44.54
and 47.08 against native's 42.48 and 44.77, no better.

## Where the cold wasm build's time went, base

- Cargo's step: 6.7 s, the same in both arms.
- The load compiled every module with Cranelift, one after another (load-cargo loads them in
  order), to read its list of macros: 4.4 to 10.9 s each, 21 to 50 s in all.
- The run after it ("again", 49.9 s) compiled them all again: the store's key of the machine
  code had the store's build identity in it, which changes with the first toolchain name the
  process sets (`compiles_against`), so the next process looked under another key.

## What the branch changes

- The machine code is kept under a key of no path and no build: the module's sha256, the
  architecture, and Wasmtime's `precompile_compatibility_hash` (its version, target, flags,
  CPU features and tunables), in the store's lasting namespace (`insert_lasting`), so it rides
  the sealed packs to R2 as the rest. A copy beside the module (`<module>.<key>.cwasm`) is
  mmapped with `Module::deserialize_file`. A mismatch compiles again; Wasmtime refuses code of
  another configuration.
- The module is compiled where it is built (`compile_time.rs`, `own_macro.rs`), beside the
  other units, with Cranelift's parallel compilation on (the `parallel-compilation` feature).
- The module carries its list of macros in a custom section (`blaze_proc_macros`), so the list
  is written beside it at once and the compile runs on a thread of its own; the first
  expansion of its macros waits for it.
- A list beside the module, stamped with the module's and its `.made-of`'s length, mtime and
  inode, lets the load read no module at all.
