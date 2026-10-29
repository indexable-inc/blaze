---
status: record
summary: Proc macros built and run as WebAssembly.
date: 2026-10-27
---
# Proc macros run as WebAssembly

The report for the `pmwasm` lane of wasm32: proc macros built by blaze as `wasm32-wasip1`
modules and expanded in blaze's process under Wasmtime and Cranelift. The user's side is in
[cargo.md](../cargo.md#proc-macros-run-as-webassembly).

Every number here was measured on one ix VM: arm64, 16 cores, 16 GiB, nothing else running,
the pinned nightly (`harness/toolchain`), 2026-10-27. They are milliseconds unless they say
otherwise.

## What runs

- `crates/blaze-macro-wasm/guest/proc_macro.rs` is the guest's `proc_macro` crate. blaze's
  build of a proc macro for a wasm target puts it in place of the toolchain's
  (`main.rs`, `harnesses`). `syn`, `quote` and `proc-macro2` build against it unchanged. Its
  `TokenStream`, `Span`, `Ident`, `Literal` and `Group` hold numbers. Each operation the
  guest can't do itself goes to the host through an import of module `blaze_proc_macro`: a
  span's source, line or join, `from_str`, `Literal::subspan`, a tracked path or variable.
  Streams cross as bytes, in `src/wire.rs`'s format.
- blaze (`--target wasm32-wasip1 --crate proc_macro --shared`) links the macro crate and
  everything it uses into a WASI reactor that exports `blaze_proc_macro_list` and
  `blaze_proc_macro_expand`. No rustc-built code is in it.
- rustc builds a proc macro for the host, so crates that link `proc_macro` (the macro crate,
  and those whose root has `extern crate proc_macro`, such as syn, synstructure and
  proc-macro2) are told no `target_os = "wasi"` and no `target_family = "wasm"`
  (`own_macro::host_side_cfg`). synstructure and syn 1 leave out `proc_macro` on wasm32 with
  `target_os = "wasi"`, and synstructure its `decl_derive!` with it: a module had none of the
  derives declared that way, hir-ty's `TypeVisitable` and ICU's `Yokeable` among them (#828).
  Every other crate is told the target: libc, getrandom and wasi pick the WASI library by it.
- The host (`src/lib.rs`, `src/wasi.rs`) compiles the module with Cranelift where blaze builds
  it (`cargo/compile_time.rs`, `cargo/own_macro.rs`), with no unit waiting: the module carries its list of macros in a custom section
  (`blaze_proc_macros`), written to the `.macros` file beside it at once, stamped with the
  module's and its `.made-of`'s length, mtime and inode, so that a load reads no module. The
  first expansion of its macros waits for the compile. The machine code is kept beside the
  module (`<module>.<key>.cwasm`, mmapped by `Module::deserialize_file`) and in the store's
  lasting namespace, under a key of no path or build: the module's sha256, the architecture and
  Wasmtime's `precompile_compatibility_hash` (its version, target, flags, CPU features and
  tunables), so it travels to R2 in the sealed packs. Each expansion gets a fresh
  instance (`InstancePre`), so no state carries over between expansions, and memory is capped
  at 4 GiB.
- WASI is a subset written for this. The package directory is preopened read-only at
  `/blaze/package`. Every path and stat read is recorded relative to it, with what was there.
  The environment holds only cargo's variables for the crate. The clock reads 0 and random
  bytes are fixed. Every other call answers `ENOSYS`.
- Expansions are kept in `cargo/keyed.rs`'s two-level format (`src/keyed.rs`), under
  `Store::{get,insert}_lasting`, which leave out the build of blaze, the toolchain and the
  host:
  - the first key is what the module was built of (its `.made-of` key, since two builds of
    one source are not the same bytes), the macro, its kind, the input and attribute bytes,
    the shape of the input's spans and the variables;
  - the second key adds every recorded read and what it held.

  An expansion that asked the compiler about a span is not kept.

## Corpus: every macro builds and expands as wasm

A workspace depending on serde (derive), tokio (macros), clap (derive), thiserror,
async-trait, pin-project, derive_more (full) and strum (derive). Each proc-macro crate was
built by `blaze <ws> --crate <c> --crate proc_macro --shared --target wasm32-wasip1`, then
one macro of it was expanded with `blaze-engine proc-macro-wasm`. The same input was expanded
natively: rustc's dylib, through the toolchain's `rust-analyzer-proc-macro-srv`. The outputs
were compared as text.

| crate | module | macro | same output as native |
|---|---|---|---|
| serde_derive | 14.0 MB | Serialize, Deserialize | yes |
| tokio_macros | 9.7 MB | main | yes |
| clap_derive | 11.8 MB | Parser | yes |
| thiserror_impl | 11.5 MB | Error | yes |
| async_trait | 11.1 MB | async_trait | yes |
| pin_project_internal | 10.5 MB | pin_project | yes |
| derive_more_impl | 19.3 MB | Display | yes |
| strum_macros | 12.3 MB | EnumString, Display | yes |

None fails. derive_more_impl failed to build at first: `unsupported: callee replace: wasm:
call of LibCall(Memcpy)`. A `memcpy` reached through `try_call` had no wasm lowering, and
`crates/hir-ty-codegen/wasm.rs` `try_call` now lowers it as `call` does.

## Expansion time, native against wasm

`blaze-engine proc-macro-wasm <module> --macro M --input F --runs 20 --native <rustc's dylib>
--server <toolchain's server> --store <dir>`. The rows are:

- **wasm compile**: Cranelift's compile of the module (cold), or the load of its machine code
  from the store by a new process.
- **first**: the first expansion of the process.
- **warm**: the median of 20 more.
- **found**: the median of 20 expansions answered by the store in a new process.
- **native start**: the server's spawn and the dylib's load.

| macro | wasm compile, cold | wasm compile, from store | wasm first | wasm warm | wasm found | native start | native first | native warm |
|---|---|---|---|---|---|---|---|---|
| serde Serialize | 10937 | 3.4 | 2.89 | 2.50 | 0.05 | 13.5 | 1.21 | 0.62 |
| serde Deserialize | 7663 | 3.6 | 9.67 | 8.82 | 0.25 | 4.5 | 2.50 | 2.00 |
| tokio main | 4511 | 2.7 | 0.72 | 0.47 | 0.02 | 11.6 | 0.68 | 0.20 |
| clap Parser | 6742 | 3.5 | 8.56 | 8.09 | 0.18 | 13.7 | 2.20 | 1.78 |
| thiserror Error | 5233 | | 1.75 | 1.46 | | 10.5 | 0.79 | 0.44 |
| async_trait | 4782 | | 3.01 | 2.63 | | 11.0 | 1.12 | 0.66 |
| pin_project | 4433 | | 0.34 | 0.14 | | 10.5 | 0.48 | 0.09 |
| derive_more Display | 7627 | | 0.95 | 0.67 | | 15.0 | 0.70 | 0.16 |
| strum EnumString | 5678 | | 0.98 | 0.72 | | 8.1 | 0.69 | 0.28 |

So:

- **Cold:** compiling a module with Cranelift (4.4 to 10.9 s) costs far more than a native
  server's start (5 to 15 ms). It is paid once per module and machine. A new process loads the
  machine code from the store in about 3 ms, which is less than a server's start.
- **Warm:** wasm expands 2 to 5 times slower than native: serde Deserialize 8.8 against
  2.0 ms, clap Parser 8.1 against 1.8. The code blaze emits for wasm32 is not optimized the
  way rustc's release build of the dylib is, and Cranelift recompiles it from wasm.
- **Found:** an expansion the store kept costs 0.02 to 0.25 ms in a new process, faster than
  native's warm expansion. Over a build, most expansions repeat, so found is the common case.
- **Reads:** a macro that reads a file is found again only while the file is unchanged. The
  tiny workspace's `answer!` reads `answer.txt`. After an edit from 42 to 43, the next run
  missed and expanded `43u32`. Before the edit it was found and gave `42u32`.

## Builds, end to end

`blaze <workspace> --crate <bin> --out <o> --exe <exe>` with `BLAZE_CACHE` set to a store
of the run's own, total wall time and blaze's "load workspace" line, seconds. "cold" is the
first run with an empty store and no module built; "again" is the next run; "native" is
`BLAZE_PROC_MACRO_WASM=0` (rustc's dylibs, through the toolchain's server), after the
others. tiny is one derive and one function-like macro; corpus is the workspace above with
all 8 crates' macros used by one binary. `--own-target` is the build in which blaze builds
the proc macros itself (`cargo/compile_time.rs`).

| workspace | cold | again | native |
|---|---|---|---|
| tiny | 18.2 (load 6.1) | 1.03 (load 0.35) | 1.10 (load 0.17) |
| corpus | 51.0 (load 45.7) | 2.61 (load 1.4) | 4.01 (load 0.19) |
| tiny, `--own-target` | 7.3 (load 3.4) | 1.18 (load 0.44) | |
| corpus, `--own-target` | 43.0 (load 4.4) | 2.30 (load 1.1) | |

The cold run is the module builds (2 to 7 s each, in parallel) and Cranelift's compiles of
them (4 to 11 s each). Again, every module's build and machine code come from the store
(about 0.35 s and 3 ms each) and most expansions are found, so the corpus builds faster than
with native macros, whose server and dylibs start each run.

### Since: compiled in the build, kept path-free (lane pmfast)

The table above has the load compiling each module, one after another, and a key of the
machine code that had the store's build identity in it, which changes with the first toolchain
name a process sets: the run after a cold `--own-target` build compiled every module again
(49.9 s). With the changes above, on the same VM, `--own-target`, the corpus, wall seconds,
medians of alternating runs ([tools/bench/results/2026-10-27-pmfast](../../tools/bench/results/2026-10-27-pmfast/README.md)):

| arm | cold | again | edit | again |
|---|---|---|---|---|
| wasm, before | 80.05 | 49.94 | 5.18 | 2.90 |
| wasm, after | 46.08 | 5.34 | 4.05 | 1.89 |
| native | 45.17 | 13.38 | 3.99 | 2.02 |

A module's compile costs about 5.5 CPU seconds (0.5 to 0.8 s on 16 cores): the 8 of the corpus
take the machine's cores from the build's own units for about 2.5 s in all, which is what is
left of the cold gap. Cranelift's `OptLevel::None` compiles no faster than `Speed` here.

## What is left

- A cheaper first tier (Winch, or Pulley) for the cold compile, with Cranelift's code kept for
  later runs, would take most of the compile's CPU off the cold build.
- A module's own build is kept as a build script's is (`own_script.rs`), keyed by its
  rust-project description, which has the checkout's absolute paths. So another checkout or
  machine builds the module again, and its expansions are found only by the same checkout.
  Taking the paths out of that key, as a build script's run already does, makes both travel.
- Warm expansion is 2 to 5 times slower than native. Optimizing blaze's wasm32 output
  is the lever.
- Line and column of a span, `Span::source_text`, `Span::file` and `local_file` ask the
  database, and an expansion that asked is not kept. The serde, tokio and clap expansions
  above did not ask: each was kept and found.
- `proc_macro::tracked::path` reads are recorded only through WASI. A macro that names a file
  by `tracked::path` without reading it is not keyed by it.
