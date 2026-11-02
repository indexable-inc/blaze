---
status: plan
summary: blaze v2 fastcompile change 20 (crates/blaze-macro-eval): proc macros run as their own code with the real `proc_macro` client on the MIR evaluator, in process, answered by blaze's server, every access checked; the expanders behind `load`, the key that misses no input, state across calls, the learned session verdict, isolation, the state of the art, and the speed of light of a hit and a run.
date: 2026-10-29
---
# fastcompile-macros: proc macros on blaze's own compiler

Change 20 of [compile-time.md](../../design/compile-time.md). A proc-macro crate is Rust, so
blaze's front end lowers it as it lowers any crate the build compiles, and each expansion runs
the macro's real `#[proc_macro]`, `#[proc_macro_derive]` or `#[proc_macro_attribute]` function
on the MIR evaluator (`hir_ty_mir::mir::Evaluator`), with the real std and the real
`library/proc_macro` client. Nothing is loaded natively, no process is started, and nothing goes
back to the wasm host or a server process (no-fallback). The compiled tier (change 13's
copy-and-patch, confined as a wasm engine confines a module) is
[macros-tier.md](macros-tier.md).

## The path a build takes

1. The daemon compiles the proc-macro unit's front end (`blaze-cargo` `serve/unit/compile.rs`
   `fn macros`) and calls `blaze_macro_eval::load(db, krate, toolchain)`. `load` checks that
   the crate is a proc-macro crate, finds its `proc_macro` dependency, checks that its bridge
   sources (`library/proc_macro/src/bridge/*.rs`) are byte for byte the ones the server was
   written against (`protocol.rs`, by hash), finds the bridge's `Client::expand1`/`expand2`,
   `Buffer::new` and `Closure::call`, and lists the macros as `__blaze_proc_macros` does
   (`run::functions`). Each is an `Evaluated` expander.
2. A call (`ProcMacroExpander::expand_in`) is keyed ([below](#the-key)); a hit is read from the
   store and nothing runs.
3. A miss runs (`run.rs`): a fresh evaluator for the macro crate, a 4 GiB memory budget (what a
   wasm32 engine gives a module), the macro's client made by the bridge's own `expand1`/`expand2`
   for the macro's function item, its input written into a buffer made by the bridge's own
   `Buffer::new` (grown by its own `reserve`), and `Client::run` called with a `BridgeConfig`
   whose dispatch is a pointer to `Closure::call`. blaze claims `Closure::call`
   (`hir_ty_mir::mir::foreign::Foreign`): each request the client sends is read from the
   evaluator's memory and answered by `server.rs` as `rustc_expand::proc_macro_server` answers
   it, in the client's buffer.
4. The output stream is made a `tt::TopSubtree` with the call's spans, kept in the store when
   the run may be, and given to the front end.

## The server

`server.rs` speaks the pinned bridge's protocol on bytes (`wire.rs`): the 31 methods of
`bridge/mod.rs`, token streams as owned handles freed as the client takes them, spans as
interned handles. Token streams are `stream.rs`'s `Rc`-shared trees, with rustc's spacing
(`Joint`/`JointHidden`/`Alone`, the open delimiter's spacing, doc comments kept as tokens and
desugared only by `into_trees`), so `to_string` prints exactly what rustc prints:
`print.rs` is rustc's `tts_to_string` and its Oppen printer, `lex.rs` rustc's lexer for
`TokenStream::from_str`, `Literal::from_str` and the ident check, with the session errors
rustc reports for them reported as the expansion's errors. A span question only the database
can answer (text, file, line, column, parent, join across anchors) is asked through
`load_cargo::answer`, and the expansion is then not kept. `TokenStream::expand_expr` (unstable,
`proc_macro_expand`) stops the expansion with an error naming the macro: the server does not
expand expressions.

## Isolation

- The evaluator checks every access; the budget bounds memory and the stack depth.
- What the macro reaches outside its own code is a claimed shim of `os.rs` or an error naming
  the macro and the function: files (read-only, under the calling crate's `CARGO_MANIFEST_DIR`
  and `OUT_DIR`, each read traced), the environment (the calling crate's, never blaze's;
  `setenv` stops), the clock and randomness (real, and untraceable, so not kept), `syscall`
  (the numbers std uses; any other stops), std's `hashmap_random_keys` (fixed keys, one valid
  outcome of randomness, so `HashMap` iteration is the same each run). An extern function of
  any other name is a body the evaluator does not have, which stops the expansion naming it.
- The only way back into blaze is the server's `Closure::call`.

## The key

A call is kept under `RunKey { code, given }` plus what it read (`blaze_usercode`'s traced
reads), the same scheme as the wasm host's, so `blaze_expand`'s store, check mode
(`BLAZE_PROC_MACRO_CACHE=check`) and puts serve both:

- `code` (`code.rs`): the macro crate's content ID (data, edition, cfg, environment, workspace,
  its dependencies' content IDs) and every file of every source root of its whole closure by
  path and text, with this server's version. Not a version string: an edited path dependency or
  a patched registry crate is another code. Files a crate of the closure read outside its source
  roots (`include_str!`, `include!` of `OUT_DIR`) are not known before its queries ran, so they
  are traced reads of each kept expansion instead (`code::included`).
- `given` (`blaze_expand::given`): the macro's name and kind, input and attribute tokens, spans
  as shapes relative to the call, the calling crate's environment and directory, the toolchain.
- reads: every file, directory listing and variable the run read, through `os.rs`, and the
  paths given to `tracked_path::path`, made absolute against the calling crate's directory.

Not kept: a run that asked the database about a span, read the clock or randomness, read what
can't be traced, emitted a diagnostic, panicked, or ran with state from the calls before it.

## State across calls

Under rustc a macro crate is loaded once per calling crate's compilation, so statics persist
across its calls in that crate. Here each calling crate's calls of a library are a session, in
the order they expand, started again with each revision of the database
(`expander.rs` `Session`). The evaluator tracks the statics (`Evaluator::track_statics`); a
static outside the sysroot (`CrateOrigin::Lang`) that a run changed is the macro's state:

- one that holds no address is preset into the next call of the session as it was left
  (`Evaluator::preset_static`);
- once one holds an address of the evaluator's memory, which no fresh run can be given, the
  session's earlier calls are run again on one evaluator before the next (exact, not kept).

Whether a crate keeps state is learned, as the wasm host learns it, and kept by code hash in
the store expansions are kept in (`blaze_expand::keeping`): read through its reader, written
as puts the store's owner publishes, one key per verdict, stateful winning. Unseen, its calls are a session (`requires_session`, so the front end
expands them in declaration order) and no kept expansion is used, since a hit skips a run that
may leave state; once a build ran some and none left state (`blaze_macro_eval::flush`, called
on the store owner's thread when a job's build is over, which then publishes that thread's
puts), it is stateless and its expansions are found in the store; once one
left state, it is stateful for good. A stateless verdict that a later run contradicts is said,
and the next load is a session.

## Tiers

`tier::choose` picks the evaluator or the compiled image per expansion. While the compiled
tier's confining pass, loader and runner wait on hir-ty-codegen's IR of an instance and
stencil's frame offsets (requests `fastcompile-macros-tier 1` and `2`), `Policy::compiled` is
false and every expansion runs on the evaluator. A crate the policy sends to the compiled tier
that can't be compiled is an error naming the macro, never a stay in the evaluator.

## State of the art

- **rustc** loads the macro's dylib natively, unsandboxed, once per compilation; its outputs are
  not cached across compilations. blaze runs the same code with every access checked and keeps
  outputs across builds and machines.
- **rust-analyzer's proc-macro server** loads the same dylib natively in a separate process; no
  isolation beyond the process, no cache.
- **watt** (dtolnay) runs proc macros compiled to wasm in an interpreter or wasmtime, given as
  precompiled wasm blobs; blaze's wasm host is that, built from source. Here no second build of
  the macro is needed: the front end's MIR of the crate is what runs.
- **Miri** interprets MIR with every access checked, as this evaluator does; Miri's speed is
  the evaluator tier's, which is why the compiled tier exists.
- **Bazel/Buck remote caching** keys an action by its declared inputs; this key is by the inputs
  actually read, traced, as Shake's and `rattle`'s traced dependencies, so an expansion that
  reads one file of a directory is not invalidated by the others.

## Speed of light

Machine: an Apple M5 Max performance core at about 4.5 GHz, sustained IPC about 3.5, as in
[share.md](share.md#speed-of-light). Nothing here was run.

**A hit** (the warm build: the call's key is in the store). The work that can't be skipped:

| step | cost |
|---|---|
| `code`, memoized per revision (`code::Memo`): one lock and a compare | about 20 ns |
| `given`: BLAKE3 over the call's encoded tokens, about 4 KB for a 100-token derive input at about 1 cycle per byte | about 1 µs |
| `find_reads`: one probe of the mapped store, then the traced reads' values (none for most derives; one `stat` per file read otherwise, about 1 µs each) | 0.5 to 2 µs |
| `read`: the kept output, about 800 tokens of about 16 bytes, decoded into a `tt::TopSubtree` with the call's spans (about 10 ns a token, the front end's own tree type, so no second copy) | about 8 µs |

That is **about 10 µs per expansion**, bound by decoding the output into the tree the front end
takes, which any server pays. The path above is that: no allocation besides the output tree, no
run, no lock held across the lookup. The once-per-revision `code` hash is one pass over the
closure's file content IDs (about 300 files for syn, quote and proc-macro2: about 300 × 40 ns,
12 µs, shared by every call of the crate in that revision).

**A run** (a miss). The bound of the macro's own work is what the native macro does: for
`#[derive(Serialize)]` on a 20-field struct, about 150 µs of code and about 2,500 bridge RPCs at
100 to 150 ns each, 0.4 to 0.55 ms (macros-tier.md, "Speed of light"). On the server's side each
RPC here is a handle lookup and a few bytes written, within that budget: no allocation per
request beyond the trees a request builds (the reply buffer is reused, streams share trees).

The evaluator is not within 10% of that bound: it runs the same work at 100 to 300 ns per MIR
statement, about 0.3 to 0.8 s per such expansion, 200 to 500 times the bound. That gap is the
evaluator tier's by design, as Miri's is; it is closed by the compiled tier
([macros-tier.md](macros-tier.md), 1.1 to 1.7 ms per expansion, within its own 10%), and paid
only on a miss, once per macro version and input across every machine sharing the cache. Until
the compiled tier runs (it waits on requests `fastcompile-macros-tier 1` and `2`), a cold build
of a crate with many derives pays the evaluator's cost on every first expansion: this is the
open bug of this part, named here so it is not forgotten.

**State.** A session whose statics hold no address costs one copy of those statics per call
(bytes, usually under 1 KB). One whose statics hold an address replays the session's earlier
calls before each call, quadratic in the session's length; such macros (an interner of strings
in a `static`) are rare, and are exact rather than fast until the compiled tier keeps one
image's memory across a session's calls.
