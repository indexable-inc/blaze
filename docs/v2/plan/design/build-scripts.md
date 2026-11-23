---
status: plan
summary: How blaze obtains a build script's outputs (cfgs, generated files, rustc-env values, link instructions) for `check` without rustc and without running third-party code outside the sandbox. The census of tempo's 106 build-script units by class, from reading alone; the design, decided per class (the script as a wasm32-wasip1 module in the proc-macro runner, the environment Cargo documents computed from the unit graph, the compiler answered by blaze inside the sandbox, C compilation deferred); the record and its key (crates/blaze-build-script); what is built and what is not.
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
0 unavailable: the same graph as `tempo-units-1116.txt`), takes its 106 build-script units
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

**Where it runs.** A build script is user code. It runs only as a `wasm32-wasip1` module in the
proc-macro lane's runner (crates/blaze-proc-macro: Wasmtime, a fresh instance, no ambient
authority, fuel and memory limits; rule 9, rule 10). There is no second sandbox and no native
run, ever. The module comes from the same bootstrap builder as proc macros
(`scripts/src/bin/proc_macro_modules.rs`, decision D52): the script and its build-dependencies
compiled for `wasm32-wasip1` with the pinned nightly, fetched first, built offline in its
sandbox, paths remapped. This is the same named interim exception to rule 1, extended to build
scripts in D52's row; it ends when blaze compiles them itself. A module record absent is a
finding naming the package, never a build blaze starts.

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
  and toolchain, and answered by blaze's own front end checking it once that front end can;
  until then by `blaze_build_script::pinned::TEMPO`: rows answered by reading the pinned
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
  the C mode is in the key, so a `build` never takes a check-mode record. The end state: each C
  request is its own record, compiled by a sandboxed C toolchain (docs/v2/plan/design/toolchain-c.md).
  Tonight a deferred request is a typed "not built yet" finding only when a `build` needs it.
  blake3's `blake3_neon` and the link instructions are recorded either way.
- **C, generators:** run as they are. Their files are the record's; the crate's
  `include!(concat!(env!("OUT_DIR"), "/x.rs"))` reads them through the record by name.
- **E:** tempo-chainspec runs as is (its reads above the package are inside `/workspace`).
  tempo-node and zone-node spawn `git` and read the clock: `git` is refused as above (a typed
  "spawns git" naming the package; the end state is blaze answering `git describe` from the
  checkout's own facts as a tracked read), and the clock's fixed value makes the timestamp
  deterministic.

### Who uses the record

The names-as-tasks lane takes each crate's cfgs and the `include!`d files; the expansion lane
takes `rustc-env` pairs and `OUT_DIR`'s files as tracked reads of `env!` and `include!`; the
driver's successor runs the build-script task kind (unit in, record out).

## Built

- **The runner's build-script mode** (crates/blaze-proc-macro `script.rs`,
  `script_sandbox.rs`): an in-memory `OUT_DIR`, tracked reads (`file:`, `dir:`, `stat:`),
  the process host call, every limit, the record. A second run is answered from it.
  `blaze_build_script::cx::ScriptCx` puts the record, read in place (`OutputsRef`), in front
  of a crate's expansion: `env!("OUT_DIR")` is `/out`, a `rustc-env` pair is its value, and
  `include!` of `/out/x` is the record's file. These are tracked reads of the expansion, as
  agreed with the expansion lane. Proof on oid-registry 0.8.1's `src/lib.rs`: 201 items, 172
  made by expansions, against 35 and 6 without the outputs.
  The extra WASI calls scripts need (`fd_sync`, `fd_datasync`, `path_link` answered ENOTSUP,
  the path calls relative to an open folder's descriptor) are in `script.rs` only:
  blaze-wasi's shared table is macro-eval's, which refuses every call that writes.
- **Units:** `script_unit.rs` finds a unit's module by package, version and features, makes
  its environment and runs it for the unit it serves (`outputs_by_served`).
- **The world's cfg:** blaze-world `ScriptRecords` (src/script.rs) opens each record by its
  served unit and is the world's `ScriptOutputs`. On tempo (`names --scripts`, records from
  `tempo_scripts export`): crates resolved without their script's outputs went from 89 to 10,
  unresolved imports from 10306 to 9810.
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
  identity, and an entry without `features` is refused. Before this, proc-macro2's script took
  its `!cfg!(feature = "proc-macro")` early return and printed no `wrap_proc_macro`.
- **The probe table** `blaze_build_script::pinned::TEMPO` (18 rows, all accepted, each with its
  reason in the pinned library).
- **Outputs layout** `bzbsout2`, with the deferred C requests.
- **Tests:** the hostile-script DST (`script_tests.rs`). It covers fuel, memory, `OUT_DIR`,
  the line limit, a path escape, a spawn of `git`, and invalidation by a file or a variable,
  with two planted bugs caught.

**Runs (2026-11-16, `tempo_scripts`, from the store):** 59 modules, one per package and feature
set. All 65 class C and class A units, evm2 (cfg `tco`) among them, run with no stop, and every second run is
answered from the record; times are below 5 ms first and below 0.15 ms from the record. Some
cfgs: proc-macro2 `wrap_proc_macro`, `proc_macro_span`, `proc_macro_span_location`,
`proc_macro_span_file`; anyhow `error_generic_member_access`; eyre `generic_member_access`,
`track_caller`, `backtrace`; rustix `lower_upper_exp_for_non_zero`, `rustc_diagnostics`, `libc`,
`apple`, `bsd`. libc and zerocopy print none on aarch64-apple-darwin, as their scripts say
(libc's cfgs are for other targets; zerocopy's only below a rustc version). Class C files: serde private.rs 142 B,
serde_core 90 B, thiserror 2 89 B, linkme 89 B, oid-registry oid_db.rs 35510 B,
rustc_version_runtime version.rs 1033 B, tempo-precompiles pcrs.rs 728 B.

## Not built

- The ten tempo scripts still without outputs (2026-11-16): bindgen at build time, which needs
  libclang (libproc on macOS, librocksdb-sys by its `bindgen-runtime` feature, reth-mdbx-sys
  always; none ships bindings pre-generated); `git --version` (reth-node-core, tempo-node,
  zone-node); `perl` (sha3-asm) and `sh configure` (tikv-jemalloc-sys) as tools;
  libusb1-sys's pkg-config gets an empty `--modversion` answer, and so rusb 0.9.4 (its
  direct dependent through `links = "usb-1.0"`) is not run, named. aws-lc-rs runs since
  scripts run in `links` order (2026-11-16): still 96 of 106 give outputs (+aws-lc-rs,
  -rusb, which before ran without its `DEP_USB_1.0_*`).
- The driver's build-script task kind.
- Blaze's front end answering a probe in place of the table.
- C requests as records.

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

  Options, not chosen (the coordinator decides with the owner):
  - **bindgen and libclang as sandboxed tools whose outputs are records**, with the C
    toolchain of [toolchain-c.md](toolchain-c.md) (one pinned C toolchain built from blaze's
    LLVM; its "End state" and "Slices" sections). Cost: the build script's own bindgen call
    becomes a spawn the sandbox answers by running pinned bindgen+libclang over the named
    headers (the SDK's `libproc.h`, the vendored rocksdb and mdbx headers) as a tracked,
    keyed tool run; the headers read join the read list. Needs the pinned toolchain's libclang
    first; the bindgen crate inside the script must be steered to the spawn (its `runtime`
    feature loads libclang with `dlopen`, which wasm has not: clang-sys fails to build for
    wasip1 on `libloading::Library` today).
  - **A wasm build of bindgen and libclang**, run in the same Wasmtime sandbox as the script.
    Cost: libclang is large (a full Clang front end, tens of MB of wasm), needs the C++ runtime
    and a file system view of the SDK headers; first-run compile time and memory are high,
    but it keeps one sandbox and no native tool.
  - Nothing else: until one is built, these three crates and the units above them are out of
    `check`'s reach.

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
  | `log -1 --pretty=format:X` | the HEAD commit object: loose (one zlib inflate) or packed (binary search of the `.idx` fan-out, then the entry and its delta chain in the `.pack`): O(log n) + chain; floor one object read. Placeholders to support: `%cI`, `%cs`, `%ct`, `%H`; any other is refused naming it. Built: refused `CommitObjectNotBuilt` | refused `FetchedKeepsNoCommitObject`: the record keeps only the commit id (main: the fetch record needs the commit object's bytes, a field next to `source`) |
  | `status --porcelain [--untracked-files=no]` | `.git/index` (v2/v3/v4) and a stat of every tracked file: clean when size, mtime and ctime match the entry and the index is not older than the file (git's racy-clean rule), else the content hashed as a blob must equal the entry's id; staged changes: the index's cache-tree root equal to HEAD's tree, else HEAD's tree walked. Untracked files (git's default, `-unormal`, lists each untracked non-ignored file as `??`) need a walk of the tree with the ignore rules, including `core.excludesFile`, default `~/.config/git/ignore`, which is outside the tree. Cost: n stats + hashing of changed files; floor n stats. Built: refused `StatusNotBuilt` | empty: the served files are exactly the commit's tree |
  | `describe [--tags] [--match P] --always` | a walk from HEAD to the nearest matching tag, at most 4096 commits read, refused past it; `--always` with no tag prints the abbreviation, whose length needs every object id. Built: refused `DescribeNotBuilt` | no tag reachable: `--always` prints the commit's first 7 hex digits; without it git's `fatal: No names found` (128) |
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
  fetch record does not yet (main: a field `commit: Vec<u8>` holding the raw commit object, next
  to `source` in the git record written by `unpack::git_record_in`, its SHA-1 checked equal to
  the lock's id when written). This differs from what a full clone with tags would print (a
  tag-based describe): it is the truthful answer for what blaze fetched, and it is
  deterministic. Residual, stated: the shallow clone also holds the commit's trees and blobs, so
  past 2^14 of them git's length would be 8, and git lengthens past any other object sharing the
  7-digit prefix; neither is checked (the record keeps the package's files, not the whole
  repository's objects).

## Native scripts (D65)

Owner, 2026-11-16: scripts that need a C toolchain run native, in the OS sandbox, with blaze's
own pinned clang. This replaces "no native run, ever" above for that class only.

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
secp256k1-sys 0.10/0.11/0.14, sha3-asm, tikv-jemalloc-sys, zstd-sys) with the units behind each;
the run's count is in "Runner 9" below. Native is not enough for five of them: sha3-asm spawns
`perl` and `patch`, tikv-jemalloc-sys `sh` and `make`, aws-lc-sys and zstd-sys may take `cmake`,
libusb1-sys and hidapi-rusb `pkg-config`: each such spawn is refused by name (only the pinned
compiler may be started), so they need their own answers later (pkg-config from a table, as D53
answers git).

**The compiler.** blaze's LLVM fork (`pins/llvm.json`: 23.1.4, indexable-inc/llvm
`ix/23.x` e152d9fa2bc2, archives at blaze-releases.ix.dev/native) builds with
`-DLLVM_ENABLE_PROJECTS=` (`.github/scripts/llvm.py`): no clang, no libclang; the fetched
aarch64-apple-darwin archive (`~/.blaze/native/sha256-e95fc3b4..`) holds only `llvm-config`,
`llvm-dwarfdump`, `llvm-pdbutil`. So until llvm.py adds `clang` (the end state of toolchain-c.md),
the source is the LLVM project's release for the same LLVM the nightly reports (`probe::LLVM`,
23.1.4): `https://github.com/llvm/llvm-project/releases/download/llvmorg-23.1.4/LLVM-23.1.4-{macOS,Linux}-ARM64.tar.xz`
(1,570,051,292 and 1,829,686,400 bytes; sha256 `5d3179db..39ef` and `9b27cf26..3a91`, read from
the GitHub release's asset `digest` and its `.jsonl` attestation, recorded in a pin table beside
llvm.json, fetched to `~/.blaze/v2/toolchain/clang/<sha256>/`). Nothing is fetched yet. Still
missing beside it: C library headers. A macOS clang without an SDK cannot compile `<stdlib.h>`
(toolchain-c.md: the SDK only on the user's explicit acceptance, keyed by `SDKSettings.json`'s
digest); Linux needs a pinned glibc sysroot. Both are owner questions.

**The sandbox.** The tree's confinement (blaze-cargo `sandbox/macos.rs`, `linux.rs`, blaze-usercode
`confine.rs`) gives reads of the WHOLE machine (`(allow default)` with writes denied): the
machine's clang is reachable, so it is not sound for D65 and is not moved. Its raw layers
(`sandbox/macos/raw.rs`, `linux/raw.rs`) are sound but serve a single-threaded launcher, which the
rebuilt path has not. New, in blaze-build-script `native.rs` (sans-IO: the policy) and
blaze-proc-macro `native_sandbox.rs` (the run): macOS Seatbelt `(deny default)`, reads only of
the grant (package folder, dependencies' outputs, the toolchain folder, `OUT_DIR`) plus the OS's
own loader set (`/`, `/usr/lib`, `/System/Library`, `/System/Volumes/Preboot/Cryptexes/OS`: no
compiler there; `/System/Volumes/Data`, `/usr/bin`, `/Library`, `/Applications` are NOT read),
writes only `OUT_DIR`, `process-exec` only of the script and the granted compiler binaries, no
network (denied by default); Linux Landlock (reads, writes, execute by path) plus a new user and
network namespace, set in `pre_exec`. A deadline kills the process group. Refusals: a program or
path the grant cannot say is refused before the run, by name; inside the run the kernel refuses
(EPERM/EACCES); naming each refused path needs a witness (the old interposer's role), not built.

**The script binary.** Built today only for `wasm32-wasip1`, by the interim builder
(`scripts/src/bin/proc_macro_modules.rs`, D52: cargo from the pinned nightly in Seatbelt or bwrap,
paths remapped, `--target wasm32-wasip1`). A native script is the same build with `--target` the
host and no process shim (it must really spawn the pinned clang); its module key adds the target.
Not built in this leg.

**The record.** The same outputs record (`bzbsout3`); the run key adds the runner word, the
compiler shelf digest and the host target. Read list: coarse and correct: the digests of every
granted input folder (a coarse proof covers all under it); per-file reads need the witness, or
`-MD` depfiles (which miss bindgen: libclang runs in the script's own process). Determinism
(rule 25): `CFLAGS`/`CXXFLAGS` carry `-ffile-prefix-map=<OUT_DIR>=/out`,
`-ffile-prefix-map=<package>=/workspace/<package>` and `-fdebug-compilation-dir=.`;
`SOURCE_DATE_EPOCH=0` fixes `__DATE__`/`__TIME__`; `AR` is the shelf's `llvm-ar`, deterministic
by default; bindgen's text holds no paths. Proven by a two-run byte compare once a clang exists.

## For a successor

- **Runner 11: native on the real path, host tools (D71), the builder confined, 2026-11-16.**
  WRITTEN, NEVER COMPILED (the integrator's lines go here when they come back).
  - The real path: `script_unit::outputs` asks `native_unit::placed` (`native::runner` of the
    closure, then `toolchain::placement` by `Setting::mode`) before anything runs. Wasm: as
    before. Native: `native_unit::native_outputs`: the `script.bin` record with exactly the
    unit's features (`module_of_native`, identity line `target <host>`; `module_of` now skips
    those), the env from the same `Given` as wasm (the unit's features, the SERVED unit's cfg:
    no hard-coded table), the key (`RunnerKey::Native` with the shelf digest, `macos-sdk
    <Version>`, host tools), `find`, else the package written out under `work/pkg/<key>` (a
    record's files) or used in place (a member), `OUT_DIR` `work/out/<key>`, `run_script`, and
    `keep_as` with a coarse read list (every package file's digest). `Setting::native: None`
    refuses a native script by name (`NativeUnitError::NoNativeSetting`); never wasm after.
  - D71: `clang.json` `host_tools` (macOS: `perl` at `/usr/bin/perl`, reads
    `/System/Library/Perl`, `needed_by` `sha3-asm`; Linux none), read by `host_tools`. Found
    without running perl: `/usr/bin/perl` and `perl5.34` are both a real perl (`otool -L`: each
    links `/System/Library/Perl/5.34/darwin-thread-multi-2level/CORE/libperl.dylib`; no
    version wrapper on this host); libperl's `Config.pm` says privlib `/System/Library/Perl/5.34`,
    archlib its `darwin-thread-multi-2level`, vendorlib `/System/Library/Perl/Extras/5.34`, site
    `/Library/Perl/5.34`. Granted: the binary (read, execute) and `/System/Library/Perl` (the
    sealed system volume; already inside the loader's `/System/Library`). NOT granted:
    `/Library/Perl`, `/Network/Library/Perl` (user-installed modules: not keyed; perl skips an
    `@INC` folder it cannot read). sha3-asm calls `Command::new("perl")` and reads no variable,
    so `PATH` is exactly the tools' folders (`/usr/bin`): any other program found there is not
    granted and its start is refused. Key: `HostToolKey { name, digest }` (the binary's
    SHA-256), hashed only when a tool is granted (old native keys stand). Mark: the read list
    is filed under the note `build-script run host-tool` (`host_tools::RUN_KIND_HOST_TOOL`,
    `host_tools::marked`); the origin's refusal is the store writer's (below).
  - The builder: `proc_macro_modules`' native mode runs cargo under `bstd::procs::hermetic::confinement`
    (the deny-by-default profile, one home; its helpers add the toolchain's and the shelf's
    `bin` programs and `helper_folders` the target folder): reads the vendored sources, the script's own source folder,
    the pinned toolchain, the shelf, the SDK; writes its workspace, target, `CARGO_HOME`,
    `TMPDIR`. The wasm mode keeps `seatbelt(work)`.
  - Which path was refused (design, not built). (a) The unified log: Seatbelt reports every
    denial not marked `no-report` as a kernel line `Sandbox: <proc>(<pid>) deny(1) <op>
    <path>`; blaze reads it after a failed run with `/usr/bin/log show --start <t0> --predicate
    'sender == "Sandbox"'` run outside the sandbox. Needs: attribution to this run (children's
    pids are short-lived; parallel runs interleave), and it lags by up to a second. (b) A tag:
    `(deny default (with message "<run key>"))` puts the run's key in each line, which solves
    (a)'s attribution if macOS 27 honours it (one hostile test decides). (c) A second run under
    `(allow default (with report))`: rejected: it runs user code wider than its grant (rule 9)
    and is a second run of the script. (d) An interposer (`DYLD_INSERT_LIBRARIES`): rejected:
    SIP strips it for `/usr/bin` tools and a static call escapes it. PICK (a) with (b)'s tag:
    `native_sandbox::run` keeps `t0`, and on a non-zero exit `NativeScriptError::Failed` gains
    `refused: [(op, path)]` from the tagged lines, each printed beside the script's own error.
    Not written: it runs `/usr/bin/log`, which needs the tag test first.
  - First round, integrator's lines: blaze-build-script "check ok (all targets)", "45 passed, 0
    failed"; blaze-proc-macro "check ok", "test (filter native): 13 passed, 0 failed"; scripts
    "check ok", native builder test "1 passed". Real runs: (a) the builder under the new
    profile was refused: cargo's LibreSSL reads `/private/etc/ssl/openssl.cnf` at start ("Auto
    configuration failed ... Operation not permitted"); (b) sha3-asm reached perl, but the
    `.S` was untranslated perlasm: `keccak1600-armv8.pl` opens `| "$^X" arm-xlate.pl ..`, a
    quoted command perl hands to `/bin/sh`, which reads the link `/private/var/select/sh` and
    starts `/bin/bash`; neither was granted. Fixes, second round (NEVER COMPILED): the builder
    reads that one file; a host tool gains `runs` (perl: `/bin/bash /bin/sh`, granted execute,
    each keyed by its digest as `perl+<path>`), and `reads` gains `/private/var/select/sh`.
    The host-tool test's perl now does a piped open of a quoted command.

- **Runner 10: the first native scripts ran (D69), 2026-11-16.** Built: the pin table
  `pins/clang.json` (both 23.1.4 rows; `sysroots`: macOS `macos-sdk`, Linux
  `null`, a typed `PinError::NoSysroot`), read by `blaze_build_script::toolchain` (`clang_pin`,
  `sysroot`, `c_env`, `depfile_reads`, `headers_digest`, `map_paths`, `placement` with
  `NATIVE_EVERY_MODE`); `RunnerKey::Native` adds `sysroot` (`macos-sdk <SDKSettings Version>`);
  the Seatbelt profile grants `file-read-metadata` of each granted path's ancestors (clang's
  `realpath` finds its resource headers so). blaze-proc-macro: `native_script::run_script`
  (cargo's env with real folders, the C env, `PATH` empty, `TMPDIR`=`OUT_DIR`; stdout's real
  paths mapped to the key's; `-MD` files read into the headers digest and removed), examples
  `clang_shelf` (the fetch: blaze-http streaming, sha256 checked before the name is given; 25.2 s)
  and `native_run` (two runs from two folders, records compared). The interim builder takes
  `"native": {"clang", "sdk"}`: host target, no shim, linked by the shelf's clang + `ld64.lld`
  (`bin/clang.cfg` = `-fuse-ld=lld`, added on the shelf), `CARGO_NET_OFFLINE`.
  - Shelf: `~/.blaze/v2/toolchain/clang/5d3179db..39ef/` 577 MB (bin: clang-23, clang, clang++,
    lld, ld64.lld, llvm-ar, clang.cfg, clang++.cfg; lib: libclang.dylib, clang/23/{include,lib,share});
    the archive is kept beside it.
  - Ran confined, byte-equal across folders: fixture (cc, one C file: `static=hello`, 84 SDK
    headers), zstd-sys (libzstd.a 3.9 MB, 9.2 s), libproc (bindgen: osx_libproc_bindings.rs
    313,527 bytes; 0 headers listed: libclang's reads are not in `-MD`). sha3-asm stops: it
    starts `perl` (PATH empty): needs its own answer (perlasm output as a pinned input?).
  - Not done: the mode rule is a unit test, not yet run over tempo's graph; not wired into
    script_unit (the real path still runs only wasm); zstd-sys ran with features `[]`, not
    tempo's; the witness naming refused paths; Linux; compiler-rt for the fork (llvm.py edited
    only: clang;lld projects and components).

- **Runner 9: native scripts (D65), 2026-11-16.** Built: `blaze_build_script::native`
  (`runner`, `TOOLCHAIN_CLASS`, `Grant`, `seatbelt`, `GrantError`), `key::RunnerKey`, census row
  `runners`; blaze-proc-macro `native_sandbox` (`run`, `NativeRefusal`, Linux Landlock + user and
  net namespace + seccomp `socket` refusal, compiled and tested only on macOS so far: the Linux
  tests are `ignore`d off macOS; the coordinator runs them on the VM with the `ignore` removed).
  - tempo, read-only (`mixcheck.sh blaze-build-script run census <tempo> <fetch store>`):
    "runners 20 native, 86 wasm (D65)"; units behind each: aws-lc-sys 189, blst 177, c-kzg
    164, zstd-sys 174, secp256k1-sys 0.11 200, sha3-asm 200, ring 114, reth-mdbx-sys 69,
    librocksdb-sys 62, libproc 22 (the rest in the run's output). Of the six without outputs
    today, five are native (libproc, librocksdb-sys, reth-mdbx-sys, sha3-asm, libusb1-sys);
    rusb is wasm and waits on libusb1-sys's `links`.
  - Hostile tests (macOS 27, arm64; with `BLAZE_D65_QUOTE=1` each passing test prints): "read:
    confined: ATTEMPT read denied: Operation not permitted (os error 1) | planted off: ATTEMPT
    read ok"; the same for write, connect (a loopback listener the test made: no outside network)
    and exec (`/bin/echo`); "deadline: confined: ..: the native build script outlived its
    deadline of 1.5s and was killed (rule 18) | planted off: Ok(\"ATTEMPT sleep ok\")". Inside
    the grant, the package read and the `OUT_DIR` write pass. blaze-proc-macro "59 passed".
  - NOT wired into the runner: 15 of the 20 give outputs today as wasm in check mode (C requests
    deferred); routing them native before a clang is on the shelf would stop them. Owner
    question: check mode keeps wasm-deferred and `build` goes native (a keyed choice by mode,
    not a fallback), or both go native.
  - Next, in order: the fetch of the clang (the pin above) and a C library source (macOS SDK by
    acceptance; a Linux sysroot); the host build of a script in the interim builder; the witness
    that names each refused path and gives per-file reads; the two-run byte compare of a fixture
    compiling one C file.

- **Done 2026-11-16 (relay runner 3):**
  - mime_guess's empty file is right: the target side has features `[]` (reqwest takes it
    with `default-features = false`), and its script writes the file's maps only under `phf`
    or `rev-mappings`. The only zero-length generated file in tempo. It found a real bug: the
    script printed `rustc-env=MIME_TYPES_GENERATED_PATH=<OUT_DIR>/...` and the library reads
    `include!(env!(..))`, while `ScriptCx` told the crate `OUT_DIR` was `/out`. The record
    (`bzbsout3`) now keeps the `OUT_DIR` the script ran with; `ScriptCx` answers it and maps
    any path under it (`a_path_the_script_printed_into_out_dir_names_the_records_file`). The
    run key hashes the layout word (domain `run 2`).
  - blaze-world fills `CrateInput.script` (trait `ScriptOutputs::served` gives the record,
    its cfg and `OutputsRef::crate_digest`: out-dir, cfgs, env, files); the crate key holds
    the digest (`blaze world crate 2`; test `a_crates_key_covers_its_scripts_files_and_env`).
    tempo `names --scripts`: unresolved imports 9810 -> 9296, unresolved macro paths
    5114 -> 162; `names` prints each crate resolved without its outputs (`without` lines).
  - Scripts run in `links` order (`script_unit::links_order`, `links_inputs`,
    `dep_metadata`; a missing input is `UnitScriptError::Dependency`, named). The unit graph
    already carried `links`. `DEP_*` values are in the env, so in the dependent's key.
    Consumers in tempo: aws-lc-rs, rustls (aws-lc / aws-lc-rs), c-kzg and evm2 (blst,
    aws-lc-rs), librocksdb-sys (lz4, zstd), zstd-safe 7.3.0 and 8.2.0 (zstd), rusb (usb-1.0,
    blocked by libusb1-sys).
- **Done 2026-11-16 (relay runner 4):**
  - D53's `git` table: `blaze_build_script::git` (sans-IO; `question`, `answer`, `shell_git`)
    driven by `script_sandbox::GitView`, which makes every `.git` file asked for a `file:`
    read (absent ones too). Answers `--version` (fixed `git version 2.51.0`), `rev-parse HEAD`,
    `--is-inside-work-tree`, `--abbrev-ref [--symbolic-full-name] HEAD`, `branch
    --show-current`, `symbolic-ref HEAD`; loose ref, `packed-refs`, detached head, linked
    worktree (`gitdir:` relative or absolute inside the tree, `commondir`). Refused by name
    (`SpawnRefusal::Git { command, why }`): dirty (`status`, `describe --dirty`), `--short`
    with or without a length (git lengthens until unique: needs the object store; none of the
    three units asks it), no `.git` served, unborn ref, and every other question, including a
    shell `git` command with quotes or globs. Tests: four `git_*` in script_tests.rs on
    checkouts written by hand.
  - The three units: smolgen runs every git call as `sh -c "git ..."`. tempo-node and
    zone-node pass `--version` and `--is-inside-work-tree`, then stop at `git log -1
    --pretty=format:'%cI'` (commit time, CannotAnswerYet); after it come `status --porcelain`
    (dirty: refused) and `describe --tags --match`. reth-node-core is a fetched git package:
    no `.git` in its record, so `NoRepository` (cargo's checkout would have one; the locked rev
    is known from the source id, a possible later answer for `rev-parse HEAD` only).
  - Check-mode deferral of `perl`, `make`, `sh .../configure` (`probe::is_deferred_tool`,
    pkg-config was already a C tool). A script that fails after a deferred spawn is
    `ScriptWhy::NeedsDeferred` naming the last one. tikv-jemalloc-sys now gives outputs
    (`cfg prefixed`, metadata `root`; its record is 3.9 MB because the script copies the
    jemalloc source into `OUT_DIR`). sha3-asm needs perl's file; libusb1-sys needs
    `pkg-config --modversion libusb-1.0`; rusb waits on libusb1-sys.
  - Count: 97 of 106 give outputs (`scripts-1116/export-5.txt`, records in
    `scripts-1116/exported5`, told to the names lane). From the record: median 30 µs, p90
    43 µs, max 2.5 ms (tikv-jemalloc-sys, 1001 reads checked), n=97.
- **Done 2026-11-16 (relay runner 5, D54 started):**
  - D54's record shape: a `git` answer is the read `git:<folder>\n<words>` with its answer's
    digest; `answer_now` recomputes it (early-cutoff test above). `git::shell_git` reads `sh`
    quoting (`'..'`, `".."` without `$`/backtick/backslash/`!`), capped at 4096 bytes.
  - Fetched git sources: `Tree::Mem { files, git: Some(commit) }` (`mem_tree(.., git)`;
    tempo_scripts takes the commit from the lock), `git::Vcs::Fetched`: HEAD, branch
    (detached), `--is-inside-work-tree`, `status --porcelain` (empty) answered; `log` and
    `describe` refused by name. Test `git_answers_a_fetched_source_from_its_commit`.
  - Runs (`scripts-1116/three-6.txt`, `export-6.txt`): all three now stop at `log -1
    --pretty=format:%cI`: reth-node-core `FetchedKeepsNoCommitObject`, tempo-node and
    zone-node `CommitObjectNotBuilt`. 97 of 106 give outputs (unchanged).
- **Done 2026-11-16 (relay runner 6, D54 readers; NOT yet run on tempo):**
  - `blaze_git_fetch::store` (shared with the fetched pack; `pack.rs`'s `entry_header`,
    `ofs_back`, `inflate`, `apply` now `pub(crate)` and used through it): `Idx` (v2: fan-out,
    sorted ids, 4- and 8-byte offsets, every length checked), `entry_head`, `inflate_entry`,
    `apply_delta`, `loose` (zlib, header checked, id checked), `Commit::parse` (tree, parents,
    committer seconds and zone). `blaze-build-script/src/git_objects.rs`: `Store` (loose, then
    each `objects/pack/*.idx`; `info/alternates` one level, nested refused; a
    `multi-pack-index` refused), the delta chain walked back iteratively by ranged reads
    (`GitFiles::read_range`) and applied forward, capped at 4095 (git's `pack.depth`
    ceiling), the result's SHA-1 checked; `%cI` `%cs` `%ct` `%H` (`LogFormat`; any other
    placeholder `Refused::Placeholder`). `%cI` is git 2.51 date.c `DATE_ISO8601_STRICT`
    (read from its source): `YYYY-MM-DDTHH:MM:SS` in the commit's zone, `Z` when the zone is 0,
    else `+HH:MM`. `format:` prints no newline after the one commit.
  - `status --porcelain [--untracked-files=no]`: `parse_index` (v2-4, v4 prefix compression,
    `TREE` root read, `link`/`sdir` refused, extensions skipped by size, checksum not verified:
    a hostile index can lie in its entries as well), stat compare (size, mtime, ctime, inode,
    owner) with git's racy rule (entry mtime not older than the index's: hashed), else the blob
    hash; staged changes proven absent only by the cache tree's root equal to HEAD's tree, else
    `StagedUnproven`. Untracked: a walk of the worktree; any file not in the index is
    `Refused::Untracked` (no ignore rules; smolgen only tests emptiness, but a non-empty answer
    under real git would differ, so refused). Attributes that change bytes (filter, ident,
    encoding, eol) or a CR under `text=auto` refuse a hash. Cost: one index read, one stat per
    entry (two `canonicalize` per stat in `GitView::stat` today: floor is one lstat), plus a
    listing per folder for untracked.
  - `describe [--always] [--tags] [--match P]`: no tag at all (empty `refs/tags`, no
    `refs/tags/` line in `packed-refs`) gives the abbreviation (git 2.51 object-name.c:
    `max(7, ceil((msb(n)+1)/2))`, n = objects of every pack index, multi-pack-index refused,
    then lengthened past every other packed or loose id sharing the prefix); without
    `--always` git's own `fatal: No names found` (128). Tags present: `DescribeNotBuilt` (the
    walk is the next piece; tempo's checkout is shallow with no tags).
  - Fetched source `log`: answered as git's failure (128, `FETCHED_NO_COMMIT_OBJECT`): smolgen
    tolerates a non-zero `log` with no keys and every commit-date key is off, so the script's
    output is what it would be with the date. reth-node-core then stops at `describe
    --always` (`FetchedHasNoHistory`). For main: the fetch record needs the raw commit object
    (a few hundred bytes, next to `source` in `unpack::git_record_in`) to answer `log`, and
    describe needs the abbreviation's object count, which a fetched source cannot give honestly.
  - Checks: blaze-build-script check ok; blaze-git-fetch 12 passed; blaze-proc-macro 42 passed
    (full suite; was 42 before). No new test yet for the readers.
- **Done 2026-11-16 (relay runner 7, handing on at its context limit):**
  - Tests: `blaze-build-script/src/git_tests.rs`, 22 tests, fixtures written by the test (zlib
    stored blocks, a pack and v2 idx writer, an index v2/v3/v4 writer, a `Mem` GitFiles):
    loose commit, packed base + `OFS_DELTA` + `REF_DELTA`, 4- and 8-byte offsets, chain at 4095
    read and 4096 `DepthOver`, alternates, linked worktree (allowed inside the served tree,
    `Outside(Gitdir)` past it), `%cI` for Z/+05:30/-03:30 (no pre-1970: git's timestamp is
    unsigned, `-5` is `Refused::Commit`), describe's 7 digits, index clean/racy ` M`/` D`/v4/
    `Untracked`; hostile: truncated idx, fan-out, offsets (4-, 8-byte, past the table), base
    size, copy past base, inflate past declared, 2^60 (`Header`: varint cap) and 2^40
    (`ObjectOver`), wrong id packed and loose, index paths, v4 varint at the checksum; a
    `blaze_sim::check` property (mutated pack/idx/index: no panic, no object with a wrong id).
    `lanecheck.sh blaze-build-script test git_tests`: 22 passed. No planted-bug control yet.
  - Faults fixed (3): v4 index path varint read past the entries' end (slice panic); index
    entry paths unchecked (`..`, absolute, `.git`: now `Refused::IndexPath { at }`, git's
    `verify_path`); `ConfigNotRead` refused tempo's own config (`filemode = true` with
    `bare = false` matched a substring test; now `config_changes_status`, line by line, also
    refusing `[include]`).
  - Runs (`scripts-1116/three-8.txt`, `export-7.txt`, records `exported7`): tempo-node and
    zone-node give outputs (`VERGEN_GIT_SHA=1200580ff6..`, `DESCRIBE=1200580`, `DIRTY=false`;
    HEAD read from the checkout is `1200580ff62514ecfd77e6787ca590a0d2331bfd`, shallow, no
    tags). 99 of 106: libproc, reth-mdbx-sys, librocksdb-sys (no module in `runstore`: build
    with proc_macro_modules), sha3-asm (perl), libusb1-sys (pkg-config), rusb (waits on it),
    reth-node-core (`describe --always`: `FetchedHasNoHistory`; kept: cargo's git db is a full
    clone, so git there may print a tag-based describe; the lock's id alone cannot say).
  - Floor note: tempo-node "second" (from the record) is now 61 ms, not 30 µs: recomputing
    `status` stats 1670 index entries (two `canonicalize` each in `GitView::stat`). Floor: one
    index read + 1670 lstat (~2-3 ms): ratio ~20-30x, fix `GitView::stat` first.
  - NOT done: messages to the names and expansion lanes (exported7 has the `VERGEN_*`
    records), main's request for the fetch record's commit object, describe walk with tags.
- **Done 2026-11-16 (relay runner 8; this lane pauses here):**
  - Cost on every command: `GitView` no longer calls `canonicalize`. A path is plain (no empty,
    `.`, `..` part) and joined to the root as given; each folder on its way is proven a real
    folder by one `lstat`, remembered for the view (`GitView::inside`); a link standing for a
    folder makes what is under it absent (git's `has_symlink_leading_path`: ` D`), the last part
    is never followed (`read_range` and `list` refuse a link too), so nothing passes through a
    link out of the tree. `list` reads names only (no stat per entry). Test
    `git_view_never_passes_through_a_linked_folder`. tempo-node "second: 61.013834ms"
    (`three-8.txt`) is now "second: 12.272459ms" (`export-8.txt`). Measured in isolation
    (`examples/git_status_floor.rs`, release): floor "one index read + 1670 lstat: median
    1.342625ms"; with git's own folder check "+ 369 folder lstat: median 1.844167ms"; `status
    --porcelain --untracked-files=no` "median 2.36175ms" (1.76x the plain floor, 1.28x git's);
    `status --porcelain` 2.39 ms; the other five questions 0.66 ms together. The 12.27 ms in
    the run against the ~5.4 ms these sum to is not explained yet: profile it before claiming.
    Next to the floor: compute both `status` facts in one pass (one index read, one stat pass);
    blaze-core has no `StatMany` yet: a batched stat (macOS `getattrlistbulk` per folder, 369
    calls in place of 2039) would take the stat pass below the per-file `lstat` floor.
  - Fetched sources (the decision above): `describe --always` the first 7 hex digits,
    `describe` alone `No names found` (128); `Refused::FetchedHasNoHistory` deleted.
    reth-node-core now gives outputs: `VERGEN_GIT_DESCRIBE="31d05e1"`,
    `RETH_SHORT_VERSION="2.11.0 (31d05e1)"`, "second: 52.042µs, from the record: true".
  - Planted-bug controls (`blaze_sim::control`, `git_objects::Plant`, test-only thread-local
    `PLANT`; `planted` is a `const fn` `false` outside tests): `NoObjectHashCheck` was missed in
    256 cases at first (random flips and truncation never gave an object that inflates whole yet
    differs; not traced further), so the property now
    also draws a well-formed lie (the base commit's bytes flipped, same three entries at the
    same offsets): caught. `IndexEntryLengthUnchecked` (the 62-byte entry read before its
    length check): caught as a panic. blaze-build-script 36 passed; blaze-proc-macro 43 passed.
  - Count: 100 of 106 give outputs (`scripts-1116/export-8.txt`, records `exported8`). The six
    left, none ours to fix now: libproc, reth-mdbx-sys and librocksdb-sys have no module in
    `runstore` (bindgen: they wait on the owner's decision about bindgen); sha3-asm needs
    perl's output file (a deferred tool whose file the script reads back); libusb1-sys asks
    `pkg-config --modversion libusb-1.0`, a system library of the machine; rusb waits on it
    (`links = "usb-1.0"`). Names and expansion lanes not told of `exported8` (new: reth-node-core).
  - For the driver's lane (scripts as tasks): (1) key: the run key (module identity, env with
    `DEP_*`, features, cfg, target, layout word `run 2`), computed before the run; (2) needs:
    the module, then every `links` input in `script_unit::links_order` order, a missing one
    `UnitScriptError::Dependency`; (3) record: `bzbsout3`, written once, holding outputs, the
    `OUT_DIR` it ran with, and the reads list; (4) on a repeat, the driver checks the reads:
    `file:`/`stat:` by digest, and each `git:<folder>\n<words>` fact by `answer_now` (D54, early
    cutoff), rerunning the script only when one differs; (5) cost on tempo from the record:
    median 30 µs a script, tempo-node and zone-node ~12 ms each (two `status` facts), so
    compute git facts once per command per folder, shared by every script asking it; (6) the
    crate key takes `OutputsRef::crate_digest`, so a rerun whose outputs are equal stops there.
- **Next (runner 7, older):** (a) a test in script_tests.rs with fixtures written by hand: a loose
  commit (zlib stored block: `78 01`, `01 LEN NLEN`, bytes, Adler-32 big-endian), a pack with a
  whole commit and an `OFS_DELTA`, its v2 idx, an index v2 with a `TREE` root; assert `%cI`
  with zones `+0530` and `+0000`, ` M`/` D`, `Untracked`, describe's 7 digits. (b) Run the
  three units (tempo_scripts with the checkout, the fetch store and the run store; the command
  is in the previous runner's notes) and all 106. (c) describe's walk with tags. (d) floors.
- **Older next (D54, in order):** (1) a commit object reader in a shared module of
  blaze-git-fetch: loose (`blaze-deflate` zlib) and packed (`.idx` v2 fan-out and binary
  search, then `pack.rs`'s `entry_header`, `ofs_back`, `inflate`, `apply` made `pub(crate)`
  and exposed through it, OFS and REF deltas, chain capped); tempo's HEAD commit is packed
  (`.git/objects` holds only `pack` and `info`). `%cI` from the committer line. Then
  `GitFiles` needs the commit to be read through it. (2) Ask main for the fetch record's
  commit-object field (not asked yet; `unpack::git_record_in` keeps `source[..20]` only).
  (3) `.git/index` reader and `status`, per the table; untracked needs the ignore rules and
  the global excludes file, which is outside the tree: decide whether to refuse when it
  exists. (4) `describe` with the 4096-commit cap. Step 4 of the brief (floors: the 30 µs
  median against one key lookup and a mapped read; one `build.rs` edit end to end) is not
  done. The names lane was not told of new records: none changed.
- **Next (older):** the floor of an answered script (one key lookup and a mapped read) is not
  measured: measure it and state the ratio to the 30 µs median; the cost of one `build.rs`
  edit (module rebuilt by `proc_macro_modules`, D52's interim path) is not measured either.
  git's commit time, `describe` and dirty need the object store and the index: a design, not
  a row. Modules are in `scripts-1116/runstore` (copy of the scratch store); the fetch store
  used is the session scratchpad's `fetch-store`.
- **Records:** `/Volumes/Projects/blaze-int-1112-notes/port27/scripts-1116/exported4` (96
  records, layout 3) is what `names --scripts` should read; runs in `scripts-1116/export-4.txt`,
  names in `scripts-1116/names-scripts-3.txt`.
- **Unit reach:** `tempo_scripts <..> dependents <package>...` counts the units that depend on
  a package's library.

- **Modules:** write the input JSON with `checkout`, `store`, `work`, `tree`, `fetch_store`,
  `build_seconds` and `crates`, then run the built `proc_macro_modules` on it under the shared
  lock. A second run in another work folder must print `(already in the store, equal)`.
- **Runs:** `mixcheck.sh blaze-proc-macro run tempo_scripts <checkout> <fetch store>
  <blaze store> [discover] <package>...`. Add `discover` to list every unanswered try-compile
  with its kept arguments and snippet. Add a row to `pinned.rs` only with a reason quoted from
  the pinned library, and copy the snippet's bytes exactly from the Debug output.
- **Traps:**
  - Each `crates` entry needs its unit's exact `features`; take them from the `== name version
    (side, features [..])` lines of a run. A package with two feature sets needs two entries.
  - A module's identity changes with the shim or a patch, so clear old modules from the store
    when you bump it: `tempo_scripts` finds a module by name, version and features.
  - A feature that turns on an optional build-dependency is not handled (none in tempo). The
    module build then fails naming the crate.
  - Host- and target-side units with equal environments share one record.
- **Into the world:** `tempo_scripts <..> export <dir>` with no package writes every script's
  record as `<name>@<version>@<side>.outputs`; `blaze-world` `names <ws> <fetch store>
  --scripts <dir>` reads them. `outline <package>` lowers the served crate's root through
  `ScriptCx` with a control.
- **Fact shim:** `BLAZE_FACT_SHIM_RUN` names the run's facts folder passed into a build script's environment (inherited by children) by `crates/blaze-build-script/src/fact_shim.rs`.

