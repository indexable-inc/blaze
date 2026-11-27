---
status: plan
summary: How blaze obtains a build script's outputs (cfgs, generated files, rustc-env values, link instructions) for `check` without rustc and without running third-party code outside the sandbox. The census of tempo's 106 build-script units by class, from reading alone; the design, decided per class (a pure-Rust script as a wasm module under Wasmtime, a C-linking script native and confined, the environment Cargo documents computed from the unit graph, the compiler answered by blaze, `git` answered from the checkout, C compilation deferred in `check`); the record and its key (crates/blaze-build-script); what is built and what is not.
date: 2026-11-16
---
# Build scripts

A build script decides which `cfg`s its crate is compiled with, writes Rust files the crate
`include!`s, and sets values the crate reads with `env!`. Without its outputs, `check` cannot
resolve names in that crate. Code: `crates/blaze-build-script` (sans-IO: the instruction parser,
the environment, the outputs record, the key; the census as `examples/census.rs`).

## Census (tempo, 2026-11-16, from reading alone)

`census /Volumes/Projects/ref-tempo/tempo <fetch store>` computes blaze's own unit graph with
every dependency's manifest read from its record in the fetch store (1263 units, 1066 packages,
0 unavailable), takes its 106 build-script units
(94 packages; 85 units serve a target-side unit, 21 a host-side one; anyhow, libc, mime_guess,
num-traits, parking_lot_core, ruint, serde, serde_core, serde_json, thiserror 1 and 2 and zmij
run twice, once per feature set or side), reads each script and the crate's sources in place
from the records (`PackView`; a member's from the checkout), and classes them. Nothing was
executed and cargo's caches were not read. An earlier census read 46 units from a graph that
lacked 711 packages; every one of its 40 packages is here again, in the same class. Columns:
units; the instructions the script can print (rerun advice left out); the compiler probes it
makes; the files the crate includes from `OUT_DIR`; processes other than the compiler;
build-dependencies.

| class | package | units | prints | probes | crate includes | other processes | build-deps |
|---|---|---|---|---|---|---|---|
| A | ahash 0.8.12 | 1 | rustc-cfg rustc-check-cfg | version | | | version_check 0.9.5 |
| D | alloca 0.4.0 | 1 | | | | cc (C compiler) | cc 1.6.0 |
| D | antithesis-instrumentation 0.1.1 | 1 | | | | cc (C compiler) | cc 1.6.0 |
| A | anyhow 1.0.104 | 2 | rustc-cfg rustc-check-cfg | try-compile+version | | | |
| E | aws-lc-rs 1.19.0 | 1 | rustc-cfg rustc-check-cfg rustc-link-arg warning {formatted} | | | | |
| D | aws-lc-sys 0.47.0 | 1 | metadata:conf metadata:include metadata:libcrypto metadata:libcrypto_path metadata:libdir metadata:libssl metadata:libssl_path metadata:link_kind metadata:root metadata:system_libs rustc-cfg rustc-check-cfg rustc-link-arg rustc-link-lib rustc-link-search warning | try-compile+version | bindings.rs | Command::new(executable); bindgen (libclang); cc (C compiler); cmake; pkg-config | cc 1.6.0, cmake 0.1.58, dunce 1.0.5, fs_extra 1.3.0, pkg-config 0.3.34 |
| C | aws-types 1.5.0 | 1 | | version | build_env.rs | | rustc_version 0.4.1 |
| A | bindgen 0.72.1 | 1 | | | | | |
| D | blake3 1.8.7 | 1 | rustc-cfg rustc-check-cfg warning | | | cc (C compiler) | cc 1.6.0 |
| D | blst 0.3.17 | 1 | metadata:BINDINGS metadata:C_SRC rustc-cfg rustc-link-lib rustc-link-search warning | | | Command::new(clang); Command::new(clang-cl); cc (C compiler) | cc 1.6.0 |
| D | c-kzg 2.1.8 | 1 | rustc-link-lib | | | bindgen (libclang); cc (C compiler) | cc 1.6.0 |
| E | clang-sys 1.9.1 | 1 | metadata:include rustc-flags rustc-link-lib rustc-link-search warning | | | Command::new(path) | glob 0.3.4 |
| A | commonware-macros-impl 2026.10.0 | 1 | warning | | | | |
| E | crc32fast 1.5.2 | 1 | rustc-cfg rustc-check-cfg | | | Command::new(env::var_os("") | |
| A | crossbeam-deque 0.8.8 | 1 | rustc-cfg rustc-check-cfg | | | | |
| A | crossbeam-epoch 0.9.21 | 1 | rustc-cfg rustc-check-cfg | | | | |
| A | crossbeam-utils 0.8.23 | 1 | rustc-cfg rustc-check-cfg warning | | | | |
| C | crunchy 0.2.4 | 1 | rustc-env | | CRUNCHY_LIB_SUFFIX | | |
| A | curve25519-dalek 4.1.3 | 1 | rustc-cfg warning | version | | | rustc_version 0.4.1 |
| A | curve25519-dalek 5.0.0 | 1 | rustc-cfg warning | version | | | rustc_version 0.4.1 |
| A | evm2 0.1.0 | 1 | rustc-cfg rustc-check-cfg | | | | |
| A | eyre 0.6.14 | 1 | rustc-check-cfg | try-compile+version | | | autocfg 1.5.1 |
| A | figment 0.10.19 | 1 | rustc-cfg | version | | | version_check 0.9.5 |
| A | flatbuffers 25.12.19 | 1 | rustc-cfg rustc-check-cfg | version | | | rustc_version 0.4.1 |
| A | generic-array 0.14.7 | 1 | rustc-cfg | version | | | version_check 0.9.5 |
| A | getrandom 0.3.4 | 1 | rustc-cfg warning | version | | | |
| A | getrandom 0.4.3 | 1 | rustc-cfg | | | | |
| D | hidapi-rusb 1.3.3 | 1 | rustc-link-lib | | | cc (C compiler); pkg-config | cc 1.6.0, pkg-config 0.3.34 |
| A | httparse 1.10.1 | 1 | rustc-cfg warning | version | | | |
| A | icu_normalizer_data 2.3.0 | 1 | rustc-cfg rustc-check-cfg | | | | |
| A | icu_properties_data 2.3.0 | 1 | rustc-cfg rustc-check-cfg | | | | |
| A | libc 0.2.190 | 2 | rustc-cfg rustc-check-cfg warning | version | | Command::new(emcc); Command::new(freebsd-version) | |
| D | libfuzzer-sys 0.4.13 | 1 | rustc-link-lib rustc-link-search | | | cc (C compiler) | cc 1.6.0 |
| E | libm 0.2.16 | 1 | rustc-cfg rustc-check-cfg rustc-env | | | | |
| D | libproc 0.14.11 | 1 | | | osx_libproc_bindings.rs | bindgen (libclang) | bindgen 0.72.1 |
| D | librocksdb-sys 0.17.3 | 1 | metadata:cargo_manifest_dir metadata:out_dir rustc-link-lib rustc-link-search | | bindings.rs | Command::new(program); bindgen (libclang); cc (C compiler); pkg-config | bindgen 0.72.1, cc 1.6.0 |
| D | libusb1-sys 0.7.0 | 1 | metadata:include metadata:static metadata:vendored metadata:version_number rustc-link-lib rustc-link-search | | | Command::new(sw_vers); cc (C compiler); pkg-config | cc 1.6.0, pkg-config 0.3.34 |
| C | linkme 0.3.37 | 1 | | | private.rs | | |
| A | linkme-impl 0.3.37 | 1 | rustc-cfg rustc-check-cfg | version | | | |
| D | lz4-sys 1.11.1 | 1 | metadata:include metadata:root | | | cc (C compiler) | cc 1.6.0 |
| A | memoffset 0.9.1 | 1 | rustc-cfg | try-compile+version | | | autocfg 1.5.1 |
| C | metrics-exporter-prometheus 0.18.3 | 1 | | | io.prometheus.client.rs | protoc (prost-build) | |
| E | mime_guess 2.0.5 | 2 | rustc-env | | | | unicase 2.10.0 |
| A | nix 0.31.3 | 1 | rustc-check-cfg | | | | cfg_aliases 0.2.2 |
| A | num-traits 0.2.19 | 2 | | try-compile | | | autocfg 1.5.1 |
| A | objc2 0.6.5 | 1 | rustc-cfg rustc-check-cfg | | | | |
| C | oid-registry 0.8.1 | 1 | | | oid_db.rs | | |
| A | parity-scale-codec 3.7.5 | 1 | rustc-cfg rustc-check-cfg | | | | rustversion 1.0.23 |
| A | parking_lot_core 0.9.12 | 2 | rustc-cfg rustc-check-cfg | | | | |
| A | paste 1.0.15 | 1 | rustc-cfg rustc-check-cfg | version | | | |
| A | portable-atomic 1.15.0 | 1 | rustc-cfg rustc-check-cfg warning | version | | | |
| A | proc-macro2 1.0.107 | 1 | rustc-cfg rustc-check-cfg | try-compile+version | | | |
| A | proc-macro2-diagnostics 0.10.1 | 1 | rustc-cfg | version | | | version_check 0.9.5 |
| C | prometheus-client 0.25.1 | 1 | | | io.prometheus.client.rs openmetrics.rs | protoc (prost-build) | |
| C | protobuf 3.7.2 | 1 | rustc-cfg | | version.rs | | |
| A | quote 1.0.47 | 1 | rustc-cfg rustc-check-cfg | version | | | |
| A | radium 0.7.0 | 1 | rustc-cfg | | | | |
| A | rayon-core 1.13.0 | 1 | | | | | |
| D | reth-mdbx-sys 2.11.0 | 1 | | | bindings.rs | bindgen (libclang); cc (C compiler) | bindgen 0.72.1, cc 1.6.0 |
| E | reth-node-core 2.11.0 | 1 | rustc-env | | | git (smolgen/vergen) | smolgen 0.1.1 |
| D | ring 0.17.14 | 1 | rustc-link-lib rustc-link-search | | | Command::new(./target/tools/windows/nasm/nasm); Command::new(command_name); cc (C compiler) | cc 1.6.0 |
| A | rlimit 0.11.0 | 1 | rustc-cfg rustc-check-cfg | | | | |
| A | ruint 1.21.1 | 2 | | | | | |
| A | rusb 0.9.4 | 1 | rustc-cfg | | | | |
| C | rust_decimal 1.43.0 | 1 | | | README-lib.md | | |
| C | rustc_version_runtime 0.3.0 | 1 | | version | version.rs | | rustc_version 0.4.1, semver 1.0.28 |
| A | rustix 1.1.5 | 1 | rustc-cfg | try-compile | | | |
| A | rustls 0.23.46 | 1 | rustc-cfg | | | | |
| C | rustversion 1.0.23 | 1 | rustc-cfg rustc-check-cfg | version | \\version.expr version.expr | | |
| D | secp256k1-sys 0.10.1 | 1 | | | | cc (C compiler) | cc 1.6.0 |
| D | secp256k1-sys 0.11.0 | 1 | | | | cc (C compiler) | cc 1.6.0 |
| D | secp256k1-sys 0.14.1 | 1 | | | | cc (C compiler) | cc 1.6.0 |
| C | serde 1.0.229 | 2 | rustc-cfg rustc-check-cfg | version | private.rs | | |
| C | serde_core 1.0.229 | 2 | rustc-cfg rustc-check-cfg | version | private.rs | | |
| A | serde_json 1.0.151 | 2 | rustc-cfg rustc-check-cfg | | | | |
| D | sha3-asm 0.1.9 | 1 | rustc-env | | | Command::new(patch); Command::new(perl); cc (C compiler) | cc 1.6.0 |
| D | signal-hook 0.3.18 | 1 | | | | cc (C compiler) | |
| C | snap 1.1.2 | 1 | | | crc32_table.rs tag.rs | | |
| B | system-configuration-sys 0.6.0 | 1 | rustc-link-lib | | | | |
| E | tempo-chainspec 1.16.0 | 1 | rustc-env | | | | |
| E | tempo-node 1.18.3 | 1 | rustc-env | | | git (smolgen/vergen) | smolgen 0.1.1 |
| C | tempo-precompiles 1.18.3 | 1 | | | pcrs.rs | | serde 1.0.229, serde_json 1.0.151, thiserror 2.0.21 |
| A | thiserror 1.0.69 | 2 | rustc-cfg rustc-check-cfg | try-compile | | | |
| C | thiserror 2.0.21 | 2 | rustc-cfg rustc-check-cfg | try-compile+version | private.rs | | |
| D | tikv-jemalloc-sys 0.6.1 | 1 | metadata:root rustc-cfg rustc-link-arg rustc-link-lib rustc-link-search warning | | | Command::new(cat); Command::new(make); Command::new(make_cmd); Command::new(sh); Command::new(tail); cc (C compiler) | cc 1.6.0 |
| A | typeid 1.0.3 | 1 | rustc-cfg rustc-check-cfg | version | | | |
| A | uncased 0.9.10 | 1 | rustc-cfg | version | | | version_check 0.9.5 |
| C | wincode 0.6.3 | 1 | | | tuples.rs | | proc-macro2 1.0.107, quote 1.0.47 |
| A | zerocopy 0.8.60 | 1 | rustc-cfg rustc-check-cfg | version | | | |
| A | zmij 1.0.23 | 2 | rustc-cfg rustc-check-cfg | version | | | |
| E | zone-node 0.5.0 | 1 | rustc-env | | | git (smolgen/vergen) | smolgen 0.1.1 |
| A | zstd-safe 7.3.0 | 1 | rustc-cfg | | | | |
| A | zstd-safe 8.2.0 | 1 | rustc-cfg | | | | |
| D | zstd-sys 2.1.2 | 1 | rustc-cfg rustc-link-lib rustc-link-search warning {formatted} | | bindings.rs | bindgen (libclang); cc (C compiler); cmake; pkg-config | cc 1.6.0, pkg-config 0.3.34 |

By class (packages, as the scan sorts them): **A** 47 (cfgs only), **C** 16, **D** 21, **B** 1,
**E** 9. Read against the scan:

- **C** (16): serde, serde_core, thiserror 2, linkme, oid-registry, rustc_version_runtime and
  tempo-precompiles as before; new are aws-types (`build_env.rs`, from `rustc_version`),
  crunchy (`lib.rs` through `CRUNCHY_LIB_SUFFIX`), protobuf (`version.rs`), rust_decimal
  (`README-lib.md`, doc text), rustversion (`version.expr`, from `$RUSTC --version`), snap
  (`crc32_table.rs`, `tag.rs`) and wincode (`tuples.rs`). mime_guess, sorted E for its
  `rustc-env`, also generates (its table, found through that variable): a generator too.
  prometheus-client and metrics-exporter-prometheus are `C*`: the generator (prost-build,
  protoc) sits behind protobuf features tempo leaves off, so under tempo they print nothing.
- **A** (47): new are ahash (`version_check::supports_feature("specialize")`), anyhow and
  thiserror 1 (try-compiles of a probe file, like thiserror 2), curve25519-dalek 4 and 5 and
  flatbuffers (`rustc_version::version_meta`), figment and uncased
  (`is_feature_flaggable`, `is_min_version("1.56.0")`), proc-macro2-diagnostics
  (`supports_features`), typeid and zerocopy (`--version`), evm2 (a `rustc` spawn the scan
  could not read the arguments of), and nine that ask the compiler nothing (bindgen,
  commonware-macros-impl, objc2, parity-scale-codec, radium, rlimit, ruint, rusb, zstd-safe 7).
  Of the 47, 22 ask the compiler nothing the scan saw (evm2's spawn aside), 18 only its
  version, 7 try-compile (anyhow, eyre, memoffset, num-traits, proc-macro2, rustix, thiserror 1;
  autocfg also compiles an empty crate first).
- **D** (21): blake3, ring and zstd-sys as before; new are alloca, antithesis-instrumentation,
  aws-lc-sys (cmake, bindgen, `-vV`), blst, c-kzg, hidapi-rusb, libfuzzer-sys, libproc
  (bindgen), librocksdb-sys, libusb1-sys (`sw_vers`), lz4-sys, reth-mdbx-sys, secp256k1-sys
  0.10, 0.11 and 0.14, sha3-asm (`perl`, `patch`), signal-hook and tikv-jemalloc-sys (`sh`,
  `make`). In `check` each is deferred: its C requests recorded, its cfgs and link lines kept.
- **B** (1): system-configuration-sys: one `rustc-link-lib` line for a macOS framework.
- **E** (9): tempo-chainspec, tempo-node and zone-node as before; reth-node-core (vergen: git,
  the clock, `PROFILE`); libm (`rustc-env` of `OPT_LEVEL`-derived values); aws-lc-rs (`DEP_`
  values of aws-lc-sys, a link argument); crc32fast (`$RUSTC` spawned for its version: an A
  by its outputs); clang-sys (runs `llvm-config` and `clang` when found: needs answers blaze
  does not give in a sandbox, behind `runtime`, which only bindgen's scripts load).

Compiler questions the 106 ask that the first 46 did not: try-compiles with
`--emit=dep-info,metadata --cap-lints=allow --edition=2018 --cfg=anyhow_build_probe`
(anyhow), the same without the `--cfg` (thiserror 1); `version_check`'s
`supports_feature`/`is_feature_flaggable`/`is_min_version` (all answered from `-vV` and the
channel: the answers come from the version text, not a compile); `rustc_version`'s
`version_meta` (`-vV`); rustversion's `--version` read through `RUSTC_WRAPPER`; aws-lc-sys's
`-vV` and its C compiler's `--version`. Every version question is the pinned text
(`probe::version`); the try-compiles are the table's rows.

Not reached by the graph: 282 of the lock's 1348 packages; 65 of those have a build script in
their record, 217 do not, none is missing from the store.

## Design

**Where it runs.** A build script is user code. A pure-Rust script runs as a wasm module
in-process under Wasmtime (crates/blaze-proc-macro `script.rs`: a fresh instance, no ambient
authority, fuel and memory limits; rule 9, rule 10). Two paths make the module today. blaze
compiles the script itself to a `wasm64` (`memory64`) module on a module-store miss (blaze-cli
`wasm64_compile.rs`, blaze-export `wasm64.rs`). The interim path is a `wasm32-wasip1` module;
`script.rs` runs both. The module comes from the same bootstrap builder as proc macros
(`scripts/src/bin/proc_macro_modules.rs`, decision D52): the script and its build-dependencies
compiled for `wasm32-wasip1` with the pinned nightly, fetched first, built offline in its
sandbox, paths remapped. This is the same named interim exception to rule 1, extended to build
scripts in D52's row; it ends when blaze compiles them itself. A script that starts a C
compiler or another native tool runs native and confined (D65, "Native scripts" below); native
tools and C-linking scripts are the remaining gap and go to the hypervisor micro-VM sandbox
([hvbox.md](hvbox.md)).

**What it is given** (`blaze_build_script::env`): exactly the environment the Cargo Book
documents, computed from the unit graph and nothing of the machine. `CARGO_MANIFEST_DIR` is the
package inside the sandbox's view (`/workspace/<folder>`: the workspace root for a member, the
unpacked package for a dependency), read-only, and the working directory. `OUT_DIR` is an
in-memory folder granted writable, at `/target/debug/build/<name>-<config>/out` (the shape
scripts parse). `CARGO_CFG_*` are the target unit's cfg atoms (multi-valued joined by `,`;
`test` and `proc_macro` left out), `CARGO_FEATURE_*` its features, `TARGET`, `HOST`, `PROFILE`
`debug`, `OPT_LEVEL` `0`, `DEBUG` `true`, `RUSTC` `rustc`, `RUSTDOC`
`rustdoc`, `CARGO` `cargo`, `CARGO_ENCODED_RUSTFLAGS` empty, the `CARGO_PKG_*` values from the
manifest, `CARGO_MANIFEST_LINKS`, and `DEP_<links>_<key>` from the direct dependencies'
records. No `PATH`, `HOME`, wrapper or user variable: a switch like `LIBC_CI` is unset.
The job count is outside the key (a hint about the machine, `env::Env::for_run`): a wasm run is
told `NUM_JOBS` `1`; a native run `NUM_JOBS` the command's workers and `CARGO_MAKEFLAGS` naming
the command's jobserver fifo (`-j --jobserver-fds=fifo:P --jobserver-auth=fifo:P`, cargo's
text), which holds workers minus one tokens: the script's own worker is its implicit token
(blaze-proc-macro `jobserver.rs`).

**What it may do.** Read files under `/workspace` (each read tracked: path and digest), write
only under `OUT_DIR`, print to stdout (the instructions) and stderr (dropped). No network; the
clock reads a fixed time and randomness is zeros (the macro sandbox's rules), so a run is a
function of its inputs. A path leaving `/workspace` stops the run, naming the package.

**What it gives** (`blaze_build_script::outputs`): one record holding the instruction lines
parsed into typed values (cfgs, check-cfgs, `rustc-env` pairs, link libraries, search paths
and arguments, `links` metadata, warnings, errors) and the files it wrote into `OUT_DIR`, by
name, sorted. `rerun-if-changed` and `rerun-if-env-changed` are parsed and dropped: they are
advice for a builder that cannot see what a script reads, and blaze tracks every real read.
An unknown `cargo::` instruction is a typed warning; an unknown old-style `cargo:` key is
`links` metadata, as Cargo reads it.

**The key** (`blaze_build_script::key`): a blaze-record base key over the module's digest, the
whole environment given, the compiler answers' identity (the pinned version text, the probe
table) and the C mode; the read list is the tracked reads; the answer is the outputs record. So
a script runs once per configuration, and never again on any machine that fetches the record.
The environment is whole in the key rather than tracked per variable: wasi-libc copies it at
start, and the unit's configuration is what it varies with anyway.

**Limits, each a typed error naming the package:** fuel (an endless loop), memory, total
`OUT_DIR` bytes (64 MiB) and file count, stdout bytes (16 MiB), an instruction line (256 KiB),
and a path leaving the package's view.

### What blaze does, per class

- **A, cfgs from the environment** (8 of 26): run as they are; nothing else is needed.
- **A, probing the compiler** (18 of 26): blaze is the compiler. The bootstrap builds the
  script's std with blaze's process shim in place of wasip1's unsupported `Command`: a spawn is
  a host call the runner answers, and only the name `rustc` is answered. `--version`, `-V`,
  `-vV` and `--version --verbose` get the pinned nightly's text (`rustc 1.101.0-nightly
  (16d1ffac1 2026-10-26)`, its commit, date, `release`, `LLVM version: 23.1.4`, with `host`
  the unit's host), held beside the pin. A try-compile (a snippet with `--emit`,
  `--crate-type`, stdin or a probe file) is keyed by the snippet's digest, its flags, target
  and toolchain, and answered by blaze's own front end when the command hands the run a
  checker (blaze-build-script `probe_check.rs`, blaze-proc-macro `script_probes.rs`; built, not
  wired: no command hands one yet); otherwise by `blaze_build_script::pinned::TEMPO`: rows answered by reading the pinned
  library, each with its reason (a stability attribute's file and line), never by running
  rustc. A probe that neither answers is a typed "cannot answer yet" naming the package, the
  kept arguments and the snippet, failing that one script: never a guess, never the real
  rustc, and never an error handed back to a script that would swallow it. Tempo's
  try-compiles are 14 fixed snippets from five scripts' families (autocfg's empty crate,
  `no_std` and two expressions; thiserror 1 and 2's and eyre's member-access probes; rustix's
  seven), all accepted by the pinned nightly.
- **B and D, C libraries:** not needed for `check`. Any other spawn (a C compiler, `ar`,
  `cmake`, `pkg-config`) is answered in check mode as deferred: the request (argv, the digests
  of the files it names) is kept in the record, the output file it would make is not made, and
  the C mode is in the key, so a `build` never takes a check-mode record. Each C request is its
  own task and record, compiled by the pinned C toolchain (D174, [c.md](c.md); blaze-build-script
  `c_tasks.rs`, blaze-proc-macro `native_c_tasks.rs`). blake3's `blake3_neon` and the link
  instructions are recorded either way.
- **C, generators:** run as they are. Their files are the record's; the crate's
  `include!(concat!(env!("OUT_DIR"), "/x.rs"))` reads them through the record by name.
- **E:** tempo-chainspec runs as is (its reads above the package are inside `/workspace`).
  tempo-node and zone-node spawn `git` and read the clock: `git` is answered from the checkout
  (D53, D54 below), and the clock's fixed value makes the timestamp deterministic.

### Who uses the record

Name resolution takes each crate's cfgs and the `include!`d files; expansion takes `rustc-env`
pairs and `OUT_DIR`'s files as tracked reads of `env!` and `include!`. The engine's `Script`
task runs each script, unit in, record out, after its `links` producers (blaze-cli
`engine_scripts.rs`).

## Built

- **The runner's build-script mode** (crates/blaze-proc-macro `script.rs`,
  `script_sandbox.rs`): an in-memory `OUT_DIR`, tracked reads (`file:`, `dir:`, `stat:`),
  the process host call, every limit, the record. A second run is answered from it.
  `blaze_build_script::cx::ScriptCx` puts the record, read in place (`OutputsRef`), in front
  of a crate's expansion: `env!("OUT_DIR")` is `/out`, a `rustc-env` pair is its value, and
  `include!` of `/out/x` is the record's file. These are tracked reads of the expansion.
  The extra WASI calls scripts need (`fd_sync`, `fd_datasync`, `path_link` answered ENOTSUP,
  the path calls relative to an open folder's descriptor) are in `script.rs` only:
  blaze-wasi's shared table is macro-eval's, which refuses every call that writes.
- **Units:** `script_unit.rs` finds a unit's module by package, version and features, makes
  its environment and runs it for the unit it serves (`outputs_by_served`).
- **The world's cfg:** blaze-world `ScriptRecords` (src/script.rs) opens each record by its
  served unit and is the world's `ScriptOutputs`.
- **Self-compiled modules:** blaze-cli `wasm64_compile.rs` (above, "Where it runs").
- **Native scripts:** blaze-build-script `native.rs` (the runner choice and the grant),
  blaze-proc-macro `native_unit.rs` and `native_script.rs` (the run).
- **`git` answers:** blaze-build-script `git.rs` (questions, answers, `sh` quoting) and
  `git_objects.rs` (objects, packs, the index); blaze-proc-macro `script_sandbox.rs` (`answer_now`).
- **C requests as tasks:** blaze-build-script `c_tasks.rs`, blaze-proc-macro `native_c_tasks.rs`.
- **The bootstrap's build-script mode**
  (`scripts/src/bin/proc_macro_modules/build_scripts.rs`, input `{"build_scripts": {...}}`).
  It compiles std with the process shim and two patches: `rt.rs` enters
  `CARGO_MANIFEST_DIR`, and the fs layer's `perm()` reports the owner-write bit, because
  wasi-libc's `st_mode` has no permission bits and `autocfg::new()` would otherwise refuse
  every `OUT_DIR`. The std copy lives at `/tmp/blaze-script-std-<content digest>`: cargo hashes
  a build-std path into every symbol, so a path under the work folder gave other bytes per
  folder. With it, all 8 class C modules came out byte-equal from two work folders and stores.
  A workspace member's manifest is cut to its build-dependencies. A module is per feature set:
  cargo compiles a script with `--cfg feature="x"` for each of its unit's features, so
  `cfg!(feature = ...)` in build.rs is fixed at compile time. The set is in the module's
  identity, and an entry without `features` is refused.
- **The probe table** `blaze_build_script::pinned::TEMPO` (18 rows, all accepted, each with its
  reason in the pinned library).
- **Outputs layout** `bzbsout7`, with the deferred C requests.
- **Tests:** the hostile-script DST (`script_tests.rs`). It covers fuel, memory, `OUT_DIR`,
  the line limit, a path escape, a spawn of `git`, and invalidation by a file or a variable,
  with two planted bugs caught.

## Not built

- Native tools and C-linking scripts in the hypervisor micro-VM sandbox ([hvbox.md](hvbox.md)).
- The front end answering a probe by default in place of the table.
- `pkg-config` answers: libusb1-sys gets an empty `--modversion` answer, so rusb (its direct
  dependent through `links = "usb-1.0"`) is not run, named.
- `git describe` with tags present (`Refused::DescribeNotBuilt`), and `git log` on a fetched
  source (the fetch record keeps no commit object).
- The witness that names each path a native run was refused, and per-file reads for it.

## Decisions (2026-11-16, D53)

- **`git` from the checkout.** reth-node-core, tempo-node and zone-node spawn `git` for version
  strings. Blaze answers from the checkout, read-only, with a small table shaped like the
  rustc probe table: `rev-parse HEAD` (long and `--short`), the branch, and `--version` with
  a stated fixed string. The files of `.git` it reads are tracked reads in the record's read
  list, so a new commit reruns exactly the scripts that asked. Whether the tree is dirty is
  answered only if a script asks, and only if cheap: it needs the index and a status walk
  over the tree (every tracked file's stat against the index), which is not cheap on a large
  checkout and reads the whole tree into the read list; until shown cheap it is refused by
  name. Any other question is the typed "cannot answer yet" naming it. The clock stays fixed:
  a build timestamp a script prints is the sandbox's fixed instant, by design, since a
  reproducible answer is the point.
- **Other tools in check mode** (`perl` for sha3-asm, `sh configure` for tikv-jemalloc-sys,
  `pkg-config` for libusb1-sys) are deferred like the C compiler: recorded as requests and the
  script continued. A script that needs their output to print what `check` reads cannot
  produce outputs in check mode, and the unit says so with the tool named (libusb1-sys:
  pkg-config's `--modversion` answer is unwrapped).
- **Bindgen at build time is the one real wall for `check`, and is not worked around.** The
  generated bindings are Rust the rest of the crate is checked against; making them needs
  libclang. No pre-generated copies fetched from elsewhere, no native tool outside a sandbox.
  Units and their reach in tempo's unit graph (`tempo_scripts ... dependents`, 2026-11-16):

  | Unit | When bindgen runs | Library units depending on it | Units of any kind |
  |---|---|---|---|
  | libproc 0.14.11 | always on macOS (`#[cfg(target_os = "macos")] fn main`) | 15 | 21 |
  | librocksdb-sys 0.17.3 | its `bindgen-runtime` feature, on in tempo | 52 | 61 |
  | reth-mdbx-sys 2.11.0 | always (`bindgen` with `runtime`) | 59 | 68 |

  Decided: these scripts run native with the pinned clang and libclang (D65, "Native scripts"
  below); their `bindgen` calls are answered by the C machinery of [c.md](c.md) (D174), with no
  WebAssembly build of the C toolchain.

## Decisions (2026-11-16, D54): `git` answers are derived facts

The coordinator's decision, recorded as D54 in decisions.json; it refines D53's `git` row.

- **Early cutoff.** The record lists the question (`git:<folder>\n<words>`) and the digest of
  its answer (exit code, stdout, stderr), not the `.git` files behind it. A later command
  recomputes the fact (`script_sandbox::answer_now`) and reruns the script only when the
  answer changed; a question refused now is an answer that differs. So "is the tree dirty"
  reruns a script when the bit flips, not on every edit. Built: `git_fact`, `git_subject`;
  test `a_git_fact_reruns_its_script_only_when_its_answer_changes`.
- **Every question the three units can ask, in order** (smolgen 0.1.1 `gitcl.rs`, read from
  the registry source; reth-node-core's `build.rs` read from `ref-reth` at 2.11.0). smolgen
  runs each as `sh -c "git ..."`; blaze reads the quoting as `sh` does (`git::shell_git`).
  1. `git --version` (`check_git`). Fails: `Gitcl` errors.
  2. `git rev-parse --is-inside-work-tree`. Fails or not `true`: `Gitcl` errors.
  3. Only when `PROFILE` is not `debug` (`rerun_on_head_change`): `git rev-parse --git-path
     HEAD --git-common-dir`, then `git symbolic-ref HEAD`. Failure of the first is tolerated.
  4. `git log -1 --pretty=format:'%cI'`, asked always (smolgen runs it even with every
     commit-date key off, as here). A non-zero exit is tolerated (no keys); a zero exit whose
     last line is not RFC 3339 makes `Gitcl` error.
  5. `git status --porcelain` (`dirty(true)`: untracked files count). Fails: `Gitcl` errors.
  6. `git describe --always --tags --match "tempo/v[0-9]*"` (zone-node: `"zones/v[0-9]*"`;
     reth-node-core: `git describe --always`, no tags, no match). Fails: `Gitcl` errors.
  7. `git status --porcelain --untracked-files=no` (`describe(.., dirty = true, ..)`: the
     `-dirty` suffix; the cache of step 5 is not reused when untracked files counted).
  8. `git rev-parse HEAD` (`sha(false)`: long). Fails: `Gitcl` errors.

  When `Gitcl` errors, smolgen's emitter (`fail_on_error` off) prints a warning and default
  values (`VERGEN_IDEMPOTENT_OUTPUT`); the scripts then build version strings from them
  (tempo-node and zone-node: `VERGEN_GIT_SHA` read with `?`, so the default is used; reth-node-core
  reads `VERGEN_GIT_DIRTY` and `VERGEN_GIT_DESCRIBE` with `?` too). That is a wrong output, so
  blaze never lets a git failure stand in for an answer it cannot give: an unanswered question
  stops the script by name. What the scripts use: the SHA (full and first 7/8 hex), whether
  dirty, and whether `describe` ends in `-g<sha prefix>` (not on a tag); `VERGEN_GIT_DESCRIBE`
  and `VERGEN_GIT_DIRTY` are also emitted as `rustc-env`.
- **The facts** (source, what is read, cost, floor, when refused):

  | Question | Checkout (`.git` in the served tree) | Fetched git source |
  |---|---|---|
  | `rev-parse HEAD`, branch, `symbolic-ref`, `--is-inside-work-tree` | HEAD, one ref (loose or `packed-refs`), `gitdir`/`commondir`: 2 to 4 small reads; floor the same | the lock's commit; nothing read |
  | `log -1 --pretty=format:X` | the HEAD commit object: loose (one zlib inflate) or packed (binary search of the `.idx` fan-out, then the entry and its delta chain in the `.pack`): O(log n) + chain; floor one object read. Placeholders: `%cI`, `%cs`, `%ct`, `%H`; any other is refused naming it. Built (`git_objects.rs`) | git's failure (128): the record keeps only the commit id |
  | `status --porcelain [--untracked-files=no]` | `.git/index` (v2/v3/v4) and a stat of every tracked file: clean when size, mtime and ctime match the entry and the index is not older than the file (git's racy-clean rule), else the content hashed as a blob must equal the entry's id; staged changes: the index's cache-tree root equal to HEAD's tree, else HEAD's tree walked. Untracked files (git's default, `-unormal`, lists each untracked non-ignored file as `??`) need a walk of the tree with the ignore rules, including `core.excludesFile`, default `~/.config/git/ignore`, which is outside the tree. Cost: n stats + hashing of changed files; floor n stats. Built, untracked files refused (no ignore rules yet) | empty: the served files are exactly the commit's tree |
  | `describe [--tags] [--match P] --always` | a walk from HEAD to the nearest matching tag, at most 4096 commits read, refused past it; `--always` with no tag prints the abbreviation, whose length needs every object id. Built with no tag; tags present refused `DescribeNotBuilt` | no tag reachable: `--always` prints the commit's first 7 hex digits; without it git's `fatal: No names found` (128) |
  | `--version` | fixed `git version 2.51.0` | same |
- **Fetched git sources** (coordinator, 2026-11-16 05:50). A fetched git source is, by
  construction, a shallow, tagless checkout of the locked commit, and every `git` question about
  it is answered as git answers in that state: inside a work tree; `rev-parse HEAD` the lock's
  id; `status --porcelain` empty (the tree is the commit's, digest-verified); `describe --always`
  with no tag reachable prints the abbreviated commit (git's describe documentation: "--always:
  show uniquely abbreviated commit object as fallback"), 7 hex digits: with `core.abbrev` unset
  (`auto`, git-config's documentation: "an appropriate value is computed based on the
  approximate number of packed objects") git takes `max(7, ceil((msb(n) + 1) / 2))`
  (object-name.c, `FALLBACK_DEFAULT_ABBREV` = 7), which is 7 while the clone holds fewer than
  2^14 objects; `describe` without `--always` fails as git does (`No names found`). `log -1`
  stays git's failure (128) and a named gap: a real shallow clone has the commit object, the
  fetch record does not (the fix: a field holding the raw commit object next to `source` in the
  git record blaze-fetch `unpack.rs` writes, its SHA-1 checked equal to the lock's id). This differs from what a full clone with tags would print (a
  tag-based describe): it is the truthful answer for what blaze fetched, and it is
  deterministic. Residual, stated: the shallow clone also holds the commit's trees and blobs, so
  past 2^14 of them git's length would be 8, and git lengthens past any other object sharing the
  7-digit prefix; neither is checked (the record keeps the package's files, not the whole
  repository's objects).

## Native scripts (D65)

Owner, 2026-11-16: scripts that need a C toolchain run native, in the OS sandbox, with blaze's
own pinned clang.

**Which (from text, in the key).** `blaze_build_script::native::runner(graph, script)` walks the
script unit's dependency closure (iteratively); a unit of a package in `native::TOOLCHAIN_CLASS`
(`cc`, `cmake`, `bindgen`, `clang-sys`, `pkg-config`) makes it `Runner::Native { because }`,
else `Runner::Wasm`. The runner is in the run key (`key::RunnerKey`: a native key adds the
parts `native`, the compiler's shelf digest and the host target; a wasm key adds nothing, so
the 100 wasm records stand); a wasm failure is never retried native (rule 3). `autocfg` is NOT in the class: it spawns `rustc`, which the wasm
probe shim answers (eyre, memoffset, num-traits stay wasm and give outputs today). signal-hook
names `cc` in its source but has no `cc` in its closure under tempo's features: wasm. clang-sys's
and bindgen's own scripts depend on no class crate: wasm. The census row `native` lists the 20
expected (alloca, antithesis-instrumentation, aws-lc-sys, blake3, blst, c-kzg, hidapi-rusb,
libfuzzer-sys, libproc, librocksdb-sys, libusb1-sys, lz4-sys, reth-mdbx-sys, ring,
secp256k1-sys 0.10/0.11/0.14, sha3-asm, tikv-jemalloc-sys, zstd-sys) with the units behind each
(on tempo: 20 native, 86 wasm). Five spawn more than the compiler: sha3-asm `perl` and `patch`,
tikv-jemalloc-sys `sh` and `make`, aws-lc-sys and zstd-sys may take `cmake`, libusb1-sys and
hidapi-rusb `pkg-config`. Those programs come only from the base tool set
([base-tools.md](base-tools.md), D192); any other spawn is refused by name.

**The compiler** (D69). The LLVM project's release of the LLVM the nightly reports
(`probe::LLVM`, 23.1.4), pinned by sha256 in `pins/clang.json` and fetched once to
`~/.blaze/v2/toolchain/clang/<sha256>/`; the machine's clang is never used. blaze's LLVM fork
(`pins/llvm.json`, `.github/scripts/llvm.py`) carries no clang yet; when its archive does, the
pin moves to it. C library headers: on macOS the installed SDK, granted read-only and keyed by
its version and the headers read; on Linux a pinned glibc sysroot.

**The sandbox.** A native run is a hermetic run (bstd `procs::hermetic`, D177; the policy in
blaze-build-script `native.rs`, the run in blaze-proc-macro `native_script.rs`): macOS Seatbelt `(deny default)`, reads only of
the grant (package folder, dependencies' outputs, the toolchain folder, `OUT_DIR`) plus the OS's
own loader set (`/`, `/usr/lib`, `/System/Library`, `/System/Volumes/Preboot/Cryptexes/OS`: no
compiler there; `/System/Volumes/Data`, `/usr/bin`, `/Library`, `/Applications` are NOT read),
writes only `OUT_DIR`, `process-exec` only of the script, the granted compiler and the base tools, no
network (denied by default); Linux Landlock (reads, writes, execute by path) plus a new user and
network namespace, set in `pre_exec`. A deadline kills the process group. Refusals: a program or
path the grant cannot say is refused before the run, by name; inside the run the kernel refuses
(EPERM/EACCES); naming each refused path needs a witness, not built.

**The script binary.** On the interim path, built for `wasm32-wasip1` by the interim builder
(`scripts/src/bin/proc_macro_modules.rs`, D52: cargo from the pinned nightly in Seatbelt or bwrap,
paths remapped, `--target wasm32-wasip1`). A native script is the same build with `--target` the
host and no process shim (it must really spawn the pinned clang); its module key adds the target.

**The record.** The same outputs record (`bzbsout7`); the run key adds the runner word, the
compiler shelf digest and the host target. Read list: coarse and correct: the digests of every
granted input folder (a coarse proof covers all under it); per-file reads need the witness, or
`-MD` depfiles (which miss bindgen: libclang runs in the script's own process). Determinism
(rule 25): `CFLAGS`/`CXXFLAGS` carry `-ffile-prefix-map=<OUT_DIR>=/out`,
`-ffile-prefix-map=<package>=/workspace/<package>` and `-fdebug-compilation-dir=.`;
`SOURCE_DATE_EPOCH=0` fixes `__DATE__`/`__TIME__`; `AR` is the shelf's `llvm-ar`, deterministic
by default; bindgen's text holds no paths. A two-run byte compare from two folders proves it
(blaze-proc-macro `examples/native_run.rs`).

