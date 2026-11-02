---
status: plan
summary: How blaze v2 expands proc macros (warm sandboxed servers and wasm modules, until change 20's evaluator replaces both), keys and keeps the expansions, and what it does to be the fastest.
date: 2026-10-29
---
# Proc macros

2026-10-30: the wasm host (`blaze-macro-wasm`), `blaze-wasi`, rust-analyzer's server
(`proc-macro-srv`, `proc-macro-srv-cli`) and `proc-macro-api`'s client are deleted; every proc macro
runs on `blaze-macro-eval` ([dedup.md](../dedup.md#proc-macros-one-engine-blaze-macro-eval)). What
this says of them is the record of the plan at its date.

Part of [usercode](../usercode.md). Code: `crates/blaze-expand/`, `crates/blaze-macro-wasm/`,
`crates/proc-macro-api/` (warm servers, `src/warm.rs`), `crates/proc-macro-srv/` (scope
markers). Requests:
`v2/requests/usercode-macros.md`.

## What runs where

Until proc macros run on blaze's own compiler ([compile-time.md](../../design/compile-time.md),
change 20, lane fastcompile), a macro runs one of two ways, chosen by `blaze_macro_wasm::rule`.
The native expanders of #1297 are superseded by change 20 and removed; what they had of the
bridge is handed to fastcompile ([below](#for-fastcompile-the-proc-macro-bridge)).

- **As a wasm module in process.** This is the default, and the module may read only its
  package.
- **In a sandboxed warm server.** This is for packages listed in `native-proc-macros`, or for
  every macro under `BLAZE_PROC_MACRO_WASM=0`.

A wasm module that can't be built or run is an error that names the macro. It is never run
natively in its place.

## Keys

An expansion is a run of user code: `RunKey { code, given }`, found and kept through
`blaze_usercode::index` in `blaze_cas::Store`.

- `code` is the content ID of the library (a server) or of the module's bytes (wasm).
- `given` holds the name, the kind, the proc-macro ABI, the toolchain (a server), the input and
  attribute tokens with spans relative to the call, the directory, and the crate's variables.
- The result is keyed by what the run was traced reading:
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
