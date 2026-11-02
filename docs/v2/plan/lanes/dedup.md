---
status: plan
summary: lane dedup (2026-10-29): one implementation per primitive (bit sets, CRC32C, radix sort, LEB128, timing wheels, pools, arenas, alignment, file mapping, interners, and every copy-paste run the scan finds), in the crate that owns that kind of thing, every caller moved onto it and the copies deleted. Round 2 (2026-10-30): one crate, engine and library per job, the v2 one.
date: 2026-10-30
---
# dedup

Lanes wrote the same primitive several times. This lane picks the best implementation of
each (or writes one that beats them all), puts it in the one crate that owns that kind of
thing, gives it the union of what callers need, moves every caller to it and deletes the
copies. Its children's notes, with each part's speed of light, are in
[dedup/](dedup/).

## Where each kind of thing lives

| kind | crate | module |
|---|---|---|
| bit sets, bit matrices | `blaze-ids` | `blaze_ids::bits` |
| radix sort | `blaze-radix`, re-exported as `blaze_ids::radix` (already the one sort; it stays its own crate because `blaze-core` and `blaze-graph` use it without `blake3`) | `blaze_radix` |
| CRC32C and other checksums | `blaze-ids` | `blaze_ids::crc32c` |
| seeded RNGs (SplitMix64) | `blaze-rng`, re-exported as `blaze_ids::rng` (its own crate, no dependencies, because `blaze-core` and `blaze-check` sit below `blaze-ids`) | `blaze_rng` |
| LEB128 and varints | `blaze-format` | `blaze_format::leb` |
| little-endian integers at an offset (`u32_at` and friends, checked and panicking) | `blaze-bytes` (no dependencies, so the crates below `blaze-format` use it too) | `blaze_bytes::le` |
| timing wheels | `blaze-core` | `blaze_core::wheel` |
| file mapping | `blaze-core` | `blaze_core::fs` (its `Mapped`) |
| object pools, arenas, `align_up` and friends | `blaze-mem` | `blaze_mem::{pool, arena, align}` |
| interners and their hash-table probes | `intern` | `intern` |
| edit distance and other small text helpers | `stdx` | `stdx::edit_distance` |
| a panic payload's text | `stdx` | `stdx::panic_text` (each caller keeps its own fallback) |
| frame vectors collected from an iterator | `blaze-mem` | `blaze_mem::frame::collect` |
| an integer in decimal with no allocation | `blaze-core` | `blaze_core::decimal` |
| bytes as lossy UTF-8 into a kept `String` | `blaze-core` | `blaze_core::utf8::push_lossy` |
| a column written into a `blaze-format` buffer and its `Slice` | `blaze-format` | `blaze_format::Placed` |
| the thread pool (a worker per CPU, work stealing) | `blaze-graph` | `blaze_graph::pool::Pool` (`stdx::thread::Pool` is gone) |
| warm process pools (proc-macro servers, test fork servers) | `blaze-usercode` | `blaze_usercode::pool::Pool` |
| work queues | none shared: each differs on purpose | `blaze-graph` `exec.rs` (a per-subsystem list in a slab), `blaze-usercode` `test/queue.rs` (the sans-IO test queue), `hir` `check/bodies.rs` (a lock-free claim ring), `blaze-macro-wasm` (a ticket lock), `blaze-dbsp` `Worklist` (single-threaded) |
| deadline timers | `blaze-core` | `blaze_core::wheel` (`hir-ty-codegen/watch.rs`' and blaze-fmt's `Timer`s time spans, not deadlines) |
| editions | none shared: three value sets | `edition` (the language's, `repr(u8)`), `blaze-manifest` `features.rs` (cargo's, with `future`), blaze-fmt (rustfmt's config values) |

Thread pools and process pools are not object pools: they hold running threads or children,
not memory, and are compared only with each other.

## Rules for every part

- A copy that differs on purpose (another invariant, a hot specialization) stays only with a
  comment saying why, under a different name.
- The shared implementation gets every test the copies had, plus a `proptest` differential
  against the simplest copy as the reference, kept as a test-only module until that copy is
  deleted (then the reference stays in the test, as the spec).
- A caller is edited only when every agent that owns it reports `STATUS: DONE` (the "free" rule
  of the mem lane's brief). Otherwise the exact edit goes to `v2/requests/<owner>.md` under
  `## dedup: <title>`, and the owner hears of it by tmux.

## Parts

| part | primitives |
|---|---|
| `dedup-bits` | bit sets |
| `dedup-codec` | CRC32C, LEB128 and varints, SHA-256 in build scripts, hasher and RNG helpers |
| `dedup-order` | radix sort copies, timing wheels |
| `dedup-mem` | pools, arenas, alignment |
| `dedup-map` | file mapping |
| `dedup-intern` | interners, hash-table probe copies |
| `dedup-macros` | `blaze-expand` against `blaze-macro-wasm`: proc-macro library, lexer, wire, host |
| `dedup-cursor` | the checked little-endian byte cursor and writer |
| `dedup-names` | the rustc-symbol path resolver in `blaze-engine` and `blaze-wasm` |
| `dedup-sim-front`, `dedup-sim-edge`, `dedup-sim-diag` | the semantic duplicate report's cross-crate groups (`tools/similar`), by area |
| `dedup-sim-rest` | the 20 cross-crate groups of 15+ lines no other part owned after the report's rerun on 2026-10-29 |
| `dedup-scan` | every other copy-paste run (diagnostics against `blaze-lints`, cargo crates against `blaze-units`, edit distance, test databases, code-identity hashing, small helpers) |

## Round 2 (2026-10-30): one crate, engine and library per job

Andrew, 2026-10-30: "anything else that is duplicate? deduplicate and always do the modern
way." Each job done twice gets one owner, the v2 design's; every user moves onto it and the
other is deleted in the same change, with no re-export under the old name. Two crates that do
different jobs are listed as such, with the evidence. Progress and per-group detail:
`/tmp/blaze-lanes/v2-dedup.md`.

### Proc macros: one engine, `blaze-macro-eval`

Four hosts run a proc macro today:

| host | crates | called by |
|---|---|---|
| in process on blaze's own compiler (MIR evaluator, the real `library/proc_macro`) | `blaze-macro-eval` | the build: `blaze-cargo/src/serve/unit/compile.rs` `macros` calls `blaze_macro_eval::load` for every proc-macro unit |
| WebAssembly under Wasmtime | `blaze-macro-wasm` (`lib.rs`, `wasi.rs`, `keyed.rs`, `rule.rs`, `bench.rs`, `guest/`) | nothing on the build path: `blaze-cargo` lists it in its manifest and names it nowhere in its source; `blaze-macro-eval`'s corpus test compares against it; fuzz targets `macro_wasm_*` |
| native in process, against blaze's `proc_macro` | `blaze-macro-wasm/src/native.rs`, `native/proc_macro` | `blaze-macro-eval`'s corpus and `kept` tests, as a second host to compare with; `proc-macro-api::bridge::wire` takes its span-part constants |
| rust-analyzer's out-of-process server, loading rustc-built dylibs | `proc-macro-srv`, `proc-macro-srv-cli`, the client half of `proc-macro-api` (`process.rs`, `legacy_protocol/`, `bidirectional_protocol/` but its `msg` types, the server pool in `warm.rs`, `ProcMacroClient`) | `blaze-macro-wasm/src/bench.rs` and fuzz targets `proc_macro_*` only |

v2's is `blaze-macro-eval`: `plan/design/compile-time.md` change 20 ("proc macros run on our
own compiler, in process"), which supersedes usercode's "wasm where the crate allows, native in
the sandbox otherwise" (`lanes/usercode.md`, 6.5, and its #1297 note), and it is the only one
the build calls. The others are second engines for the same job: a macro `blaze-macro-eval`
can't run stops with its error naming the macro (no-fallback), it does not move to another
host. The out-of-process server also runs code rustc compiled (no-rustc).

What goes: `proc-macro-srv`, `proc-macro-srv-cli`, `blaze-macro-wasm` and `blaze-wasi` (whose
wasm build scripts nothing calls either: `blaze-cargo`'s `wasi` feature names it and no source
does), the client half of `proc-macro-api`, the fuzz targets of the deleted code, and
`blaze-cargo`'s pin of the `wasm32-wasip1` standard library for proc macros
(`toolchain/own.rs`). That takes Wasmtime out of the graph, which is what brings most of the
lock's second versions (`wasmparser`, `wit-*`, `wasm-*`, `object`, `gimli`).

What stays, and where: `proc-macro-api`'s `kept` (the store of expansions, which the build and
`blaze-macro-eval` use), `bridge` (`wire`, `lex`, `spans`: `blaze-macro-eval`'s server speaks
them) and the `bidirectional_protocol::msg` span questions `blaze-lower/src/answer.rs` answers.
The span-part constants `bridge::wire` takes from `native/proc_macro` move into `bridge::wire`
itself. `blaze-macro-eval`'s corpus test keeps its own oracle, the fixture's tests
(`tests/corpus/ws/calls/src/tests.rs`), and loses only the comparisons with the deleted hosts.

Agreed by the coordinator before any deletion (asked 2026-10-30 in the queue). Done
2026-10-30 (dedup-engines): the crates above are deleted with their fuzz targets, the
`wasm32-wasip1` pin, `build_release.py --server` and the workflows' server steps; a load error
is `ProcMacroLoadingError::Refused`, naming the crate, where it said "proc macro server error".

### Hashes: BLAKE3 for blaze's own data

Every hash blaze makes of its own data is BLAKE3 through `blaze_ids::b3`: the assembly cache
key and closure and `hashed` symbol names in `hir-ty-codegen`, `library/DIGEST` (Rust's
`digest_of` and `harness/library.py`, which runs `b3sum`), the sandbox library's cache name
(hashed at run time, so `blaze-cargo/build.rs` has no build dependency), Windows sandbox temp
directories, `STD_SCRIPT` in `blaze-engine`, and the release check of a pinned blaze, which
reads `B3SUMS`. SHA-256 is `blaze-sha256`, only where an outside format names it: crates.io
checksums, the Rust release's channel-manifest pins (`toolchain/own.rs`), Mach-O code signatures
and `install.sh`'s `SHA256SUMS`. SHA-1 is only the WebSocket accept key (`blaze-web`).

### Lexers: the pinned nightly's `rustc_lexer`

`crates/rustc_lexer` is `compiler/rustc_lexer` at the pinned nightly, re-vendored on every bump;
crates.io's `ra-ap-rustc_lexer` 0.166 is an older snapshot whose one difference is that it
does not lex `k#ident` (`ForcedKeywordIdent`). The pinned one is kept; its nine users move to it
once the front end has rustc's rule for the new token (queued to lane format). `parser`'s fast
lexer is not a second lexer: it handles the common token classes and asks `rustc_lexer` for
every other token, and a differential test holds the two equal.

### Two crates, two jobs

| crates | why both stay |
|---|---|
| `blaze-build-script`, `blaze-usercode` | cargo's build-script protocol (environment, directives); the keying, tracing and store of a run above it. `blaze-wasi`, a second runner nothing calls, goes with the proc-macro engines. |
| `blaze-fuzz`, `fuzz/` | corpora replayed inside ordinary tests, with an allocation guard; the coverage-guided engine that writes corpora in the same format. |
| `blaze-codegen`, `hir-ty-codegen` | function instances, keys and compiled code (no backend); the backends that make the code. |
`blaze-cache` and `blaze-cache-macros` (the `#[blaze::cache]` library, docs/fn-cache.md) were
deleted on 2026-10-30 (Andrew): no crate depended on them, and the query database and
`blaze-cas` keep blaze's own results across runs.

Patching a running program is done twice (`blaze-live` and `blaze-inproc`); `blaze-inproc` is
v2's, and moving `blaze-live` onto it is with lane link.

### One place for every dependency

The root `[workspace.dependencies]` is the only place a version, path or git source is written;
members say `workspace = true` and add only `features`, `optional` or `default-features`, and a
root entry no member uses is deleted. `tools/v2/manifest_lint.py` holds the tree to both. A
second version left in `Cargo.lock` is forced by an upstream's newest release, as follows:

| duplicate | forced by |
|---|---|
| `object` 0.39 beside 0.40 | `ar_archive_writer` 0.5.3, the build-dependency of `psm` 0.1.32 (under `stacker`, in `stdx`) that writes its assembly archive; 0.40 is the workspace's and cranelift 0.137's |
| `hashbrown` 0.16 beside 0.17 | `gimli` 0.33, the one cranelift 0.137 pins (`gimli` itself is a single version) |
| `getrandom` 0.2 | `ring` 0.17.14 (under `rustls`, in blaze-ci and blaze-global), its newest |
| `rand_core` 0.9 beside 0.10 | `imbl` 7.0.2 (`rand_xoshiro` 0.7, for its vectors' quicksort pivots), blaze-resolve's persistent maps, its newest; 0.10 is `rand`'s under `hegeltest-c`. `im-rc` 15 (unmaintained, `rand_core` 0.6, `sized-chunks`, `bitmaps`, `typenum`) went for it (2026-10-30) |
| `getrandom` 0.3 beside 0.4, `r-efi` 5 beside 6 | `ahash` 0.8.12 under `sonic-rs` (blaze-api, blaze-ci); goes if `sonic-rs` gives way to `serde_json` (below) |
| `bitflags` 1 beside 2 | `defmt` 1.2 under `jiff`, under `cargo-util-schemas` =0.14.3 (cargo 1.99's pin) |
| `windows-sys` 0.52 | `ring` 0.17.14 |
| `embedded-io` 0.4 beside 0.6 | `postcard` 1.1.3, which names both |
| `syn` 2 beside 3 | both majors are in use upstream (syn 2: `thiserror-impl`, `tracing-attributes`, `zerocopy-derive`, `wasm-bindgen-macro-support`, `ra-ap-rustc_index_macros`, `synstructure` 0.13 under `yoke-derive`/`zerofrom-derive`; syn 3: `serde_derive`, `rkyv_derive`, `ref-cast-impl`, `defmt-macros`). The workspace's own `syn` stays 2 for now: moving `blaze-query-macros` and `macros` to 3 removes no copy and adds `synstructure` 0.14 beside 0.13 |
| `unicode-width` 0.1 | `blaze-fmt` keeps the pinned nightly rustfmt's versions (`rustfmt-unicode-width`) |
| `wit-bindgen` 0.57 (no dependencies of its own) | `wasip2` 1.0.3, under `getrandom` 0.3.4 (`ahash`, in `sonic-rs`) |
| `wasmparser` 0.261 beside 0.263 | `wasmtime` =50.0.1 (blaze-macro-eval's wasm host), the release whose Cranelift is the workspace's 0.137.1 |
| `serde_derive` (no second copy; it stays after blaze-serde-derive replaces the workspace's derives) | `wasmtime` and `wasmtime-environ` 50.0.1, which depend on it unconditionally, and cranelift's `enable-serde` (hir-ty-codegen) |

Gone from the lock as of f92a85458d (2026-10-31): the second `sha2`/`digest` family, `miniz_oxide` 0.8, `rustc-literal-escaper` 0.0.7, `hashbrown` 0.15, `foldhash` 0.1, `wasmparser`/`wasm-encoder` 0.244 with `wasm-metadata`, `wit-component`, `wit-parser` and `wit-bindgen` 0.51, `wasip3`, and tango-bench's `rand` 0.8, `thiserror` 1, `nix` 0.23, `memoffset` 0.6, `windows` 0.34 and `colored` 2.

Where two libraries do one job, one stays: seeded randomness is `blaze-rng` (hir-ty-mir's
`oorandom` went). blaze-resolve's persistent maps are `imbl` (im-rc's maintained fork, with `Rc` counts as
im-rc's). Still to decide, each with its owner: `sonic-rs` (blaze-api, blaze-ci; SIMD) against
`serde_json` everywhere else; `crossbeam-channel` against the nightly `std::sync::mpmc`.
