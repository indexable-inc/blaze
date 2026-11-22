---
status: plan
summary: How blaze v2 expands proc macros (a wasm32-wasip1 module under Wasmtime behind the proc_macro bridge, the MIR evaluator as its reference), keys and keeps the expansions, and what it does to be the fastest.
date: 2026-11-01
---
# Proc macros

2026-11-02 (lane pm-flip): wasm macros are the default. Every build plans each proc macro,
and what it depends on, as the wasm macros' units (`Kind::MacroWasm`, `schema::Unit::macro_wasm`,
under `target/wasm32-wasip1+macros/`), with no flag; `-Z wasm-macros` is accepted and does
nothing, and there is no way back to the evaluator: a host-built proc macro reaching the daemon
is refused naming it (`compile.rs`, `macros`). A proc macro's own tests stay the host's.
`--target wasm32-wasip1` builds, its units and the macros' told apart by `macro_wasm` in every
key, directory and std. Not done: the evaluator-reference tests (`tests/corpus.rs`,
`tests/wasm.rs`, `hir-firewall/tests/real.rs`, `blaze-stencil/tests/coverage.rs`) load the
crates of the default plan, which are now built for wasm32, with `blaze_macro_eval::load`.

2026-11-02 (lane pm-wasm): `-Z wasm-macros` builds every proc-macro crate as a WASI reactor
with the glue and runs each expansion in it; the corpus workspace's serde_derive,
thiserror-impl, tokio-macros and corpus-macros expand in wasm, and a panicking call fails only
its own expansion. Not done: the host's WASI files (`path_open`, `fd_read` and the rest under
the root fd, onto the evaluator's `os/files.rs` so reads stay keyed), `tests/wasm.rs` run
against a real build (its plan's build-script outputs sit at the virtual `/s/build`), and the
flip of the default (`macro_wasm` in `crates/blaze-units/src/load.rs`).

2026-11-01 (rule `proc-macros-on-wasm`): the wasm host is back, inside `blaze-macro-eval`
(`src/wasm.rs`, `src/wasm/`), and the copy-and-patch tier (`src/tier.rs`, `src/tier/`,
`blaze-mem`'s `vm`) is deleted. A macro's module speaks the pinned bridge to the same server
the evaluator's client does (`src/wire.rs`; a `usize` is eight bytes from every client). Its imports are the
bridge's shims and WASI preview 1 (`src/wasm/abi.rs`); any other import is refused at load,
naming the crate and the import. Each expansion is a fresh instance under a fuel budget,
traced as the evaluator traces (`src/wasm/wasi.rs`), and the module's machine code is kept in
the store under its id and Wasmtime's (`src/wasm/code.rs`). The evaluator stays for const eval
and as the reference: `tests/wasm.rs` holds every serde and thiserror call of the corpus to
the same tokens both ways. Not done: blaze building the module (a wasm32 macro kind in the unit
graph, the crate linked as a WASI reactor with `guest/entry.rs`), so `compile.rs` loads every
macro on the evaluator until then. The user-facing account is [docs/cargo.md](../../../cargo.md#proc-macros-run-as-webassembly).

2026-10-30: the wasm host (`blaze-macro-wasm`), `blaze-wasi`, rust-analyzer's server
(`proc-macro-srv`, `proc-macro-srv-cli`) and `proc-macro-api`'s client are deleted; every proc macro
runs on `blaze-macro-eval` ([dedup.md](data.md)). What
this says of them is the record of the plan at its date.

Part of [usercode](../../plan.md). Code: `crates/blaze-expand/`, `crates/blaze-macro-wasm/`,
`crates/proc-macro-api/` (warm servers, `src/warm.rs`), `crates/proc-macro-srv/` (scope
markers). Requests:
`fed1eb428d:docs/v2/requests/usercode-macros.md`.

## What runs where

Until proc macros run on blaze's own compiler ([compile-time.md](compile-time.md),
change 20, lane fastcompile), a macro runs one of two ways, chosen by `blaze_macro_wasm::rule`.
The native expanders of #1297 are superseded by change 20 and removed; what they had of the
bridge is handed to fastcompile ([below](#for-fastcompile-the-proc-macro-bridge)).

- **As a wasm module in process.** This is the default, and the module may read only its
  package.
- **In a sandboxed warm server.** This is for packages listed in `native-proc-macros`, or for
  every macro under `BLAZE_PROC_MACRO_WASM=0`.

A wasm module that can't be built or run is an error that names the macro. It is never run
natively in its place.

## What is not supported

2026-11-16 (lane proc-macro, `scripts/src/bin/proc_macro_modules.rs` over tempo's 79
proc-macro units): one crate refuses wasm32-wasip1 unchanged, and blaze does not patch it.

- **synstructure's `decl_derive!` on wasm32 + wasi.** synstructure (0.13.2 and 0.14.0 alike)
  compiles its proc_macro use out
  under `#[cfg(all(not(all(target_arch = "wasm32", any(target_os = "unknown", target_os =
  "wasi"))), feature = "proc-macro"))]` (on `decl_derive!` at `src/macros.rs:84` in both,
  and on `extern crate proc_macro` in 0.13.2's `src/lib.rs:159`). A crate that declares its derives with `synstructure::decl_derive!`
  then fails: "error[E0433]: cannot find `decl_derive` in `synstructure`". In tempo that is
  asn1-rs-derive 0.6.0 (BerAlias, DerAlias, BerSequence, DerSequence, BerSet, DerSet,
  ToStatic, ToDerSequence). tempo's other synstructure units (yoke-derive and
  zerofrom-derive, on 0.14.0) write `#[proc_macro_derive]` themselves and call none of the gated
  items, so they build.
- **What tempo loses.** asn1-rs depends on asn1-rs-derive unconditionally. Following the lock's
  `dependencies` lists by name, 13 packages lie above it: asn1-rs, der-parser, oid-registry,
  x509-parser and rcgen, and 8 of tempo's members (tempo-nitro-attestation-fuzz, tempo-xtask,
  tempo-zone, tempo-zone-prover-enclave, tempo-zone-prover-utils, zone-node, zone-prover,
  zone-sequencer). A build that reaches one of them stops, naming the module. The `tempo` node
  binary is not among them.
- **The smallest honest remedy, not built.** A second target description for macro crates that
  call `synstructure::decl_derive!`: wasm32 with an `os` that is neither `unknown` nor
  `wasi` (say `blaze`). The gate is then false and the crate builds unchanged. It would change
  three things:
  - std for that target is the unsupported platform layer, not WASI. `std::env` and `std::fs`
    in such a module return `Unsupported` unless the shim serves them as it serves `getenv`.
    asn1-rs-derive reads neither.
  - Crates with `cfg(target_os = "wasi")` code (libc, getrandom) take other paths or fail to
    build.
  - The module key covers the target's digest, so these modules are keyed apart.

  rustc offers no flag that turns a built-in cfg off: `--cfg` can only add, so
  `target_os = "wasi"` stays true.

## Keys

An expansion is a run of user code: `RunKey { code, given }`, found and kept through
`blaze_usercode::index` in `blaze_cas::Store`.

- `code` is the content ID of the library (a server) or of the module's bytes (wasm).
- `given` holds the name, the kind, the proc-macro ABI, the toolchain (a server), the input and
  attribute tokens with spans relative to the call, the directory, and the crate's variables.
  The directory and the variables' values are written against the calling crate's roots (its
  `CARGO_MANIFEST_DIR` and `OUT_DIR`, `proc_macro_api::kept::roots`), so the same call in
  another checkout has the same key ([store.md](store.md#portable-keys)). A workspace
  member's directory is the workspace root, which is not one of an expansion's roots (no
  variable names it, and a crate's reads are worked out again from its variables alone), so
  its expansions are keyed by that absolute directory and are not shared between checkouts.
- The result is keyed by what the run was traced reading, written against the same roots, and
  kept against where its roots are; it is given to another checkout only once a second one
  expanded it the same (`roots::verify`, [store.md](store.md#portable-keys)). An output, a
  read variable or a given string that still holds a root once ported, a read outside its
  roots and the machine-wide places, or a root's links re-pointed since the memo was made
  leave no copy to verify, so it stays here. The roots' spellings and the cargo home are worked
  out once per calling crate and revision (`Library::kept_roots`), and made again when a keep
  finds a root's links moved:
  - **A warm server:** the lines of the expansion's scope in the sandbox's live record. The
    server marks each scope with `access("/.blaze-test/expand-<n>")`.
  - **A wasm module:** the reads the WASI host recorded, plus each question the module asked
    of a span, with its answer.

Clock and random reads make a run untraceable, and it is not kept. A run is also not kept when
it asked the database what its key doesn't say, or when its output has spans its input
doesn't. `BLAZE_PROC_MACRO_CACHE=check` expands anyway and turns any difference from what was
kept into an error.

## Speed

A hit costs:
- one BLAKE3 of the call: each `Given` part is hashed once, into 32 bytes, and the tokens are
  encoded into a reused per-thread buffer;
- one lookup in the content store, with `index::find` streaming each input of a shape into
  the key;
- a stat of each traced input, with hashes cached by stat in a process-wide lock-free table;
- a decode of the expansion from the store's bytes in place.

It allocates nothing but the decoded expansion, apart from the call's environment, made
only if a shape read a variable. The input crosses as the tree's own columns
(`Tree::write_wire` into the reused buffer, then its span parts rewritten to indices of
the expansion's `Parts`), and the output is read back with `read_wire`, one pass over the
columns into the tree. `check` compares by the hash of the kept bytes, also read in place. A
miss runs the macro once and keeps what it made.

The wire's speed of light is one pass over each tree's columns each way: a `u32` copy per node
and span word, a three-word rewrite per span part, and a module that reads a span with
arithmetic alone (`start | (len | part << 16) << 32`), calling the host only for a span too
wide to pack (`span_pack`, `span_unpack`). The host side is there: `relative` is
`write_wire` plus one pass over the parts table (and over the spans only when an expansion
has more than 65535 parts), and `read_wire` checks the output canonical as it builds it, with
no table of spans or map per span. The guest builds `Rc` streams of what it reads and its
writer keeps its columns in fresh `Vec`s, which a macro's own API (an owned `TokenStream` per
group) needs. The in-process bridge passes no bytes: it walks blaze's tree straight into the
macro's stream and builds the output with `TopSubtreeBuilder`, so a warm expansion allocates
only the macro's streams and the output tree (`native/tests.rs` holds it to that).

| reference | what it does | blaze |
|---|---|---|
| rustc's in-process bridge | runs the macro on every call, through a buffer of `u32` handles | **beats** on a hit: no run. On a miss: wasm is slower than native code, and a warm server adds a socket round trip |
| rust-analyzer's proc-macro-srv | a process per toolchain, a postcard message per call, no cache | **matches** on a miss (warm server, same protocol, socket instead of pipes); **beats** on a hit; and is not keyed by `PATH` (#805) |
| dtolnay's watt | wasm macros in process, interpreted (or JIT), no cache | **beats**: Cranelift machine code kept in the store by module digest, and expansions kept |
| wasmtime `InstancePre` | linking resolved once per module | **matches**: one `InstancePre` per module; each expansion only instantiates |
| wasmtime precompiled `.cwasm` | `Module::deserialize_file` maps the code, and memory images are copy-on-write | **falls short**: the code comes from the store as bytes and is copied in. Needs a mapped file from the store (request) |
| wasmtime pooling allocator | instance slots reserved up front, instantiation in microseconds | **falls short**: on-demand allocation, one `mmap` per instance on a miss. Pooling needs a bound on live instances, and stateful sessions live as long as their crate |
| Relax (makepad) | verified macro crates compiled natively into the compiler | superseded by change 20 (fastcompile). On a miss Relax runs native code where blaze runs wasm. On a hit blaze does less |

Falls short, and what it would take:
- `Given::env` still copies each variable into an `OsString` (parent's `blaze-usercode`).
- The store's read guard (request: a lock-free view).
- The warm record is read from a file after each expansion. The file stays open, the buffer
  is reused, and lines are parsed in place; only this scope's lines and new common lines are
  copied. Streaming it over the socket would remove the read (request).
- The vendored server spawns a thread per expansion (request: a pool).

## For fastcompile: the proc-macro bridge

Change 20 runs a macro's own code on the MIR evaluator and copy-and-patch. Everything below
already exists, and is exact against rustc's proc-macro server, so a port can reuse it.

- **The `proc_macro` a macro is compiled against.** `crates/blaze-expand/src/native/proc_macro/api.rs`
  is blaze's own `proc_macro`: the full public API, with token streams and literals in the
  macro's memory and spans as the host's words. It is one text: the wasm guest is
  `crates/blaze-macro-wasm/guest/proc_macro.rs` (its wasm imports, allocator and exports) with
  `api.rs` appended (`blaze_macro_wasm::GUEST`), and the in-process bridge's
  `proc_macro/lib.rs` `include!`s it after its own `sys`, whose calls go to a thread-local
  `__native::Host` instead of to wasm imports. A test holds the two `sys` modules to the same
  functions. This is the crate to build macros against: its `sys` functions become the
  evaluator's shims, and they are the only way out.
- **The shims.** These are the host functions, one per `sys` import, in `add_host` in
  `crates/blaze-macro-wasm/src/lib.rs` and in `crates/blaze-expand/src/native/host.rs`:
  - sites: `def_site`, `call_site`, `mixed_site`;
  - span operations: `span_join`, `span_parent`, `span_source`, `span_start`, `span_end`,
    `span_byte_range`, `span_subspan`, `span_located_at`, `span_resolved_at`, `span_line`,
    `span_column`, `span_debug`, and `span_pack`, `span_unpack` for a span too wide to pack;
  - questions of the database: `span_file`, `span_local_file`, `span_source_text`;
  - lexing: `ts_from_str`, `literal_from_str`;
  - handing data across: `take`, `output`;
  - outcomes: `panicked`, `diagnostic`;
  - no-ops: `track_path`, `track_env_var`;
  - `retained`, which reports that an expansion left allocations live: a stateful macro.

  The wasm host also answers WASI preview 1 (`crates/blaze-macro-wasm/src/wasi.rs`). It gives
  the package read-only at `/blaze/package`, an empty `OUT_DIR`, cargo's `CARGO_*` variables
  only, and the real clock and randomness, marked untraceable.
- **Span words.** A span is a `u64`: `start | len_part << 32`, `len_part` being
  `len | part << 16` with `part` an index of the expansion's span parts (each distinct anchor
  and context: the def, call and mixed sites', then the input's, the attribute's and what span
  operations make, as `Parts` in `blaze_expand::bridge::wire` gives them out), or `u32::MAX` with `start` an
  index of the spans too wide to pack. Equal spans have equal words; `u64::MAX` is none.
  Keys carry the input as these columns relative to the call, and its parts as indices of the
  input's anchors and contexts (`Shapes`), so one expansion is found again for the same
  tokens in another file or item.
- **The wire format.** A tree crosses as tt's own columns (`crates/tt/src/storage/wire.rs`),
  each span part named by its index. Both hosts use one copy of the host's side,
  `crates/blaze-expand/src/bridge/` (blaze-macro-wasm depends on blaze-expand): `wire.rs`
  (`Parts`, the relative bytes, `read`), `spans.rs` (`join`, `subspan`) and `lex.rs`, which
  lexes literals and `ts_from_str` exactly as proc-macro-srv does, with `rustc_lexer`. The
  guest's reader and canonical writer are in `api.rs`, whose literal kinds are numbered as
  `tt::LitKind::code` numbers them (a test runs every kind through both).
- **Questions are inputs.** A question asked of a span (its source text, file, line and column,
  or byte range) is answered by `load_cargo::answer`. It is recorded with its answer as an
  input of the run (`Value::read` and `Value::said`, as NUL-named entries of `Traced::env`), so
  the run is found again only while the database answers the same. A question the key can't
  say makes the run unkeepable (`State::asked`).
- **Keys and keeping.** A run is `RunKey { code, given }` through `blaze_usercode::index`:
  `code` is the macro's code ID, and `given` is the host version, name, kind, input and
  attribute bytes, span shapes and cargo variables. Untraceable reads, an unkeepable question,
  retained state, or output spans the call doesn't have all mean the run is not kept.
  `BLAZE_PROC_MACRO_CACHE=check` re-runs a kept run and fails on any difference.
  `blaze_expand::native::difference` names the first token or span that differs, which is the
  comparison for the evaluator against the wasm module while change 20 lands.
- **Stateful macros.** A module whose expansion left allocations live gets a verdict in the
  store: stateful or stateless, by module digest. A stateful one runs in a session per calling
  crate and database revision, and nothing it makes is kept (`Session`, `Verdict`). The
  evaluator needs the same detection, because statics in a macro survive between calls under
  rustc.
- **What change 20 makes obsolete here:** the warm servers (`proc-macro-api/src/warm.rs`), the
  scope markers (`proc-macro-srv/src/scope.rs`), the wasm build of macros, and `rule.rs`.
  Remove them once the evaluator runs every macro, with no fallback to them.

## For a successor (2026-11-16, rebuild-1115)

Where the rebuilt pipeline stands, each line from a run on tempo in this session:

- The hook is `crates/blaze-proc-macro/src/hook.rs`: `ProcMacros::run` behind
  `ExpandCx::run_proc_macro` (`ProcCx` wraps any host). It finds the record, runs the module on a
  miss and keeps the answer. A missing module is one `Finding::MissingModule` per crate; a trap
  is `Finding::Call` naming the macro and the base key. Nobody calls the runner directly.
- A caller registers each reached macro crate with `ProcMacros::add_reached(store, lock, &Reached
  { name, version, source, features })`, which keys the module as the builder does and returns
  the `CrateItemKey` its `MacroDefinition::proc_macro` must carry. The names library
  (acd2f3dd892b1b198) and the expansion lane were told this; blaze-scope declares the defs.
- Names reach the hook through `blaze-lower`'s `MacroNames` (`Lists`, `ByName::with_names`).
- The proof run is `examples/tempo_pipeline.rs` (checkout, module store, unit list, out,
  optional `--no-proc-macros`). With the test-time unit list
  (`blaze-int-1112-notes/unit-graph/tempo-units-tests-1116.txt`, 81 proc-macro units, 80
  modules in the scratch store `pm/store-c`): 2,374 calls, 0 refused, limit or failed, 13
  `compile_error!`s. It also checks every body as stage h does (`to_typeck`), split by item
  source; only 1 body stops for anything but an unexpanded `macro_rules` call (std's and
  dependencies' rules are not loaded by the example; the names library carries them).
- `host::HOST_VERSION` (2) is in every base key; bump it when the host's answer to a call
  changes. Tests: an attribute with no arguments gets an empty stream; an answer under another
  host version is not read.
- Expansion is bounded (blaze-lower `EXPANSION_DEPTH` 128, `EXPANSION_BUDGET` 65,536 per
  top-level item and per body; `Refusal` names the macro). Tempo's deepest item chain is 3.
- The 13 errors: 12 are tokio `select!` with `biased;` sent to commonware's `select` by the
  example's by-name resolver (needs real name resolution); 1 is clap `Args` on `TempoArgs`
  (bin/tempo/src/cli.rs:57), a blaze-expand bug: `ItemAttrs::derives` drops a field whose
  `#[cfg]` is false but keeps an attribute written before the `#[cfg]`. Handed to the expansion
  lane (note at the top of crates/blaze-lower/src/lib.rs), not fixed.
- Census gaps explained: 3 `tokio::test` are behind a false cfg (linux target, feature `rpc`);
  9 `#[contract]` sit inside `proptest!` bodies; test-case takes all of a function's
  `#[test_case]`s in one call (164 calls for 167 counted runs: 2 runs miscounted by trailing
  comments, 1 behind `cfg(feature = "reth-codec")`).
- Not built yet: the driver task kind in blaze-drive, the stack-size statement, hostile-module
  and invalidation proofs through the hook, the floor measurement, the typed store error, the
  tool's version check by equality, and the record encoding (answers average about 45 KB:
  per token an 8-byte kind and 4-byte length; proposal: interned text table, varint kinds).
- Compiled records (2026-11-16, host.rs "Compiled records" holds the full rule, trust included).
  The measured cause: the names runs made their `Runtime` over `--procs-out`, which the run
  script deletes, so every run compiled every module, lazily, under the hook's `OnceLock`
  (alloy-primitives 2,947 ms where it takes 33.5 ms loaded). Now: the record is
  `<module dir>/compiled-<code key>` beside the wasm, sealed by `compiled-<code key>.seal`
  (BLAKE3 of the bytes and the key's parts, written after the bytes, each by temp name and
  rename); the code key is the wasm SHA-256, every `host::Settings` field (the one place the
  engine is built), Wasmtime's compatibility hash (triple, compiler flags, host CPU features,
  tunables, wasm features, Wasmtime's version), arch and OS. A record is loaded only from a
  folder group and others cannot write, with the folder's owner, its seal's key equal and its
  BLAKE3 equal, once per runtime; else it is removed, a `CompiledFinding` names it
  (`hook::Finding::Compiled`) and the module is compiled again. A second worker waits on the
  module's slot with a deadline (`LOAD_DEADLINE`, 300 s) and a span `proc_macro_module_wait`.
  `Runtime::new()` no longer takes a folder. `Runtime::load_all` / `ProcMacros::load_all`
  compile or load a list on N workers before expansion; blaze-world's `names` example calls it.
  Measured (examples/compile_modules.rs over a copy of the 80-module store, Mac):
  "first pass: 80 modules on 18 workers in 4554.2 ms; compiled 80; ... compiled records
  279.7 MB (wasm 115.8 MB)"; "second pass: 80 modules on 18 workers in 268.9 ms; compiled 0";
  serially "80 modules on 1 workers in 150.2 ms; ... one load: median 1.87 ms, slowest
  4.00 ms". The alloy-primitives subset (run-ready10.sh, `--roots alloy-primitives`, 18
  threads; world-1116/apc-cold.txt, apc-warm.txt): cold "11 modules ready on 18 workers in
  1389.6 ms, 11 compiled", then "120 crates resolved in 3821.4 ms", "49.0 ms alloy-primitives";
  warm "11 modules ready on 18 workers in 37.4 ms, 0 compiled", "120 crates resolved in
  3785.0 ms", "47.5 ms alloy-primitives" (before: wall 8058.4 ms, alloy-primitives 2756.8 ms).
  evm2 is not in that subset: the whole-tempo run is the names lane's. One module (2a85238058f6) compiles but does not link: it imports WASI
  `path_create_directory`, which blaze_wasi lacks. Left: the driver's `MacroModule` task calls
  `load_all`; the 18-worker warm pass is 1.8x the serial one (contention not profiled);
  `script.rs`'s build-script runtime still has its own unsealed compiled record and should
  take this one; records are 2.4x the wasm (279.7 MB): no cleanup of records of old keys yet.
- Source corrections (D58, built 2026-11-16). The table is `crates/blaze-fetch/corrections.json`
  (frozen `Table`/`Row`, unknown fields refused); the pure function is
  `blaze_fetch::correct` (`Row::correct_text`, `Row::correct_crate`: lexed by blaze-lex, a match
  is the row's predicate as a whole token subtree inside a `cfg`/`cfg_attr` attribute; a listed
  file with no match is `Refused::Stale`). Home: blaze-fetch, where a fetched source becomes
  what readers read, and a dependency of the builder, blaze-proc-macro and blaze-world. One row:
  synstructure `>=0.13, <0.15` (0.14.0 carries the same gate). Keys: `Table::pin` adds
  ` corrected <row digest>` to a corrected package's pin in every module's lock closure (builder
  `closure_lines` and `LockedPackages::closure` alike; test
  `a_source_correction_row_is_in_the_module_key`); blaze-world's crate key takes the digest too.
  The builder does not use the fetch store for registry crates: it corrects its own
  `<work>/vendor` copy in place after `cargo vendor` and rewrites that copy's
  `.cargo-checksum.json` file entries (not a `[patch]`: that would move the lock's source).
  Runs (procs-1116/d58): build.txt "corrected: synstructure 0.13.2: 7 predicates (D58)", "asn1-rs-derive 0.6.0: module
  1cdc4d32..."; six modules re-keyed and added (asn1-rs-derive, yoke-derive, zerofrom-derive,
  i18n-embed-fl, i18n-embed-impl, tempo-precompiles-macros: their lock closures reach
  synstructure); the store holds 86 (the old yoke/zerofrom/i18n/tempo-precompiles keys stay).
  names2.txt: "79 macro crates registered, 79 with a module", "unresolved imports in two
  columns: ours to fix 0 | waiting on a build script that cannot run yet 45". blaze-world reads
  corrected text through `Texts` and prints `corrected:` lines, but the names world holds no
  proc-macro crate's dependencies, so it never reads synstructure: no line there today.
  NOT DONE (step 3 of the brief): `record::DirStore` still writes expansion records with
  ambient `std::fs` (no sync, no whole-record digest, no crash test). Move it onto blaze-core's
  `Dir<ReadWrite>` as crates/blaze-cli/src/store.rs does, verify each record's digest once per
  boot, and prove it with a `blaze_core::sim` crash simulation (oracle: after any crash every
  record read is one that was written, whole; planted bug: no sync before the rename).

## DECIDED 2026-11-16 06:25 (coordinator): the runtime inside `blaze`, the identity without it

- The sandbox runtime (Wasmtime) is linked INTO `blaze`, behind blaze-cli's cargo feature
  `sandbox` (on by default): no helper process, no second executable, one code path. A command
  that expands nothing must not pay for it; the proof is binary size and a bare `blaze` and a
  repeat `check` with and without it (drive.md, the DONE 06:xx entry).
- blaze-proc-macro is split by its feature `runtime` (default on for the crate itself; the
  workspace dependency turns it off, so each user asks): WITHOUT it are `module` (the identity,
  `lock_closure` and `LockedPackages` (the lock read once for many crates), `module_key`,
  `find_module`: the store lookup that reads the record's `wasm-sha256`, not the wasm, and
  `read_module`), `record` (`base_key`, `HOST_VERSION` moved here from host.rs so the key is
  the same with and without the runtime, `find`, `DirStore`) and `sandbox` (no Wasmtime in it).
  WITH it: `host`, `hook`, `server`, `wire`, `protocol`, `script`, `script_sandbox`,
  `script_unit`, and the dependencies wasmtime, blaze-build-script, blaze-unit-graph,
  blaze-format and blaze-id. blaze-real and blaze-world's `procs-run` ask for it.
- The macro crate's identity has ONE home, blaze-world `procs::identify` (feature `procs`, no
  runtime); `register_macro_crates` (feature `procs-run`) is built on it.
- The module store a command looks in is the user's shared store, `<home>/.blaze/v2/
  proc-macro/modules/<key>/`, the home the passed value main.rs reads at the edge: a module is
  keyed by its crate's identity alone, the same for every project. The bootstrap tool's `store`
  input names that folder (`<home>/.blaze/v2`).

### Environment variables

- `BLAZE_NO_COMPTIME_PATCH`: when set, disables the built-in comptime patch library (`serde_derive`, `bytemuck_derive`, `thiserror-impl`, `derive_more`, `strum_macros`, `paste`) in `crates/blaze-proc-macro/src/hook.rs`.
- `BLAZE_PROC_MACRO_SRV`: explicit path to `rust-analyzer-proc-macro-srv` used by `crates/blaze-proc-macro/src/native_bridge.rs` before falling back to `rustc --print sysroot`.

