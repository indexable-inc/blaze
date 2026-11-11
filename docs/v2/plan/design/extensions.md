---
status: plan
summary: Extensions. People write MIR lints, facts and transforms as ordinary no_std Rust crates; blaze compiles each from source against its own API crates, checks it is capability-safe, loads it natively into the daemon and runs it under fuel and a capped arena. Slice 1 (the API, the checks, the host loop, the facts fixpoint) is built; the daemon's build of an extension is slice 2.
date: 2026-11-10
---
# Extensions: analyses, lints and transforms as ordinary crates

Someone who wants a lint, a per-function analysis or a MIR transform that blaze lacks writes a Rust crate,
names it in their workspace, and `blaze check` runs it: no fork, no dylib built elsewhere, no ABI, no plugin
protocol. An extension is a `#![no_std]` crate whose only dependency is `blaze-ext`. blaze compiles it itself,
natively, against the API crates' source this blaze was built from (types match by construction), keys the
build by its own code identity, and loads it into the daemon with no OS sandbox: its safety is the language's.
Code: `crates/blaze-ext` (the API), `crates/blaze-ext-host` (the host), `extensions/panic-in-drop` (example).

## Prior art, and the lesson of each

- **clippy, Dylint**: native, welded to one rustc build: native is right, and blaze owns the build, so the
  weld is a cache key. **Stable MIR, Marker**: a stable API bolted on late lags the real IR: expose the real
  one, versioned. **Roslyn**: a designed API; analyzers ship with libraries. **Go `go/analysis`**: `Requires`
  and `Facts`, run by a driver: the model for facts. **Charon/Aeneas, CodeQL**: the program as data: right
  for provers, no edit loop, no fixes. **swc, Zed**: wasm plugins: a copy of the tree per call, and swc's
  unversioned AST broke plugins silently. **Lean, Zig**: extensions in the language: the model here.

## Using one

```toml
[workspace.metadata.blaze.extensions.panic-in-drop]
path = "extensions/panic-in-drop"       # or version = "0.3.1", or git = ".." with rev = ".."
grants = []                             # capabilities by name; none by default
lints = { panic_in_drop_deep = "warn" } # levels; `replay = true` runs every job twice (below)
```

A `metadata` table, not a dependency kind (rule `useful-not-parity`): manifest tools skip `metadata` and
reject an unknown table, and an extension is no crate's dependency (nothing links it). Registry and git
sources are pinned in `blaze.lock` (slice 2; slice 1 builds from `path`). Unknown keys are refused.

## Writing one: a lint and a fact

```rust
#![no_std]
use blaze_ext::{Analysis, Body, Bytes, Cx, Extension, Fact, FactDecl, Level, Lint};
pub fn extension() -> Extension {
    Extension { name: "demo", api: blaze_ext::VERSION, lints: &[UNWRAPS], facts: &[FactDecl::of::<Calls>()] }
}
const UNWRAPS: Lint = Lint { name: "unwraps", level: Level::Warn, summary: "a call of `unwrap`",
                             requires: &[Depth::NAME], run: unwraps };
fn unwraps(cx: &mut Cx<'_>, body: &Body<'_, '_>) {
    for (block, data) in body.mir().basic_blocks.iter() {
        let (Some(end), Some(callee)) = (&data.terminator, body.callee(block)) else { continue };
        let below = callee.item().and_then(|it| cx.fact::<Depth>(it)).map_or(0, |it| it.0);
        if callee.is_std_method("option", "Option") && callee.name() == "unwrap" {
            cx.report(end.span, format_args!("`unwrap` in `{}`, {below} calls deep", body.name()));
        }
    }
}
pub struct Depth(pub u8);             // a fact: how many calls deep an item goes, capped
impl Fact for Depth {
    const NAME: &'static str = "depth";
    fn bottom() -> Self { Depth(0) }
    fn encode(&self, out: &mut Bytes<'_>) { out.push(self.0) }
    fn decode(bytes: &[u8]) -> Option<Self> { Some(Depth(*bytes.first()?)) }
}
struct Calls;
impl Analysis for Calls {
    type Fact = Depth;
    const REQUIRES: &'static [&'static str] = &[];
    fn run(cx: &mut Cx<'_>, body: &Body<'_, '_>) -> Depth {            // monotone from `bottom`
        let calls = body.mir().basic_blocks.iter().filter_map(|(block, _)| body.callee(block));
        let deepest = calls.filter_map(|it| cx.fact::<Depth>(it.item()?)).map(|it| it.0).max();
        Depth(deepest.map_or(0, |it| it.saturating_add(1).min(100)))
    }
}
```

## The `blaze-ext` surface (version 0.1.0)

`Extension`, `Lint`, `LintFn`, `Level`; `Cx` (`arena`, `vec`, `report`, `fact`), `Report` (`note`, `help`,
`fix`), `Arena`, `Vec`, `Bytes`, `Applicability`; `Body` (`item`, `name`, `mir`, `impl_of`, `callee`), `Item`,
`ImplOf`, `Callee`; `Fact`, `Analysis`, `FactDecl`, `FactFn`; `VERSION`, `CHANGES`; `mir::{MirBody,
BasicBlock, BasicBlockId, Statement, StatementKind, Terminator, TerminatorKind, Rvalue, Operand, OperandKind,
Place, ProjectionElem, Local, LocalId, MirSpan, MirPhase, AssertKind, AggregateKind, BinOp, UnOp, CastKind,
BorrowKind, MutBorrowKind, SwitchTargets}`: `hir_ty_mir::mir`'s own types, not a copy. `blaze_ext::host` is
the host's side, unforgeable (it takes the database). Not exposed yet: the database, `Ty`, layouts, HIR. Small
so internals can move, unstable by design: a mismatch with the version an extension states is refused by name
with `CHANGES`' entries since.

## The checks (all at compile time, over the extension's source)

| rule | refuses | error (after ``extension `x`: src/lib.rs:6:35: ``) |
|---|---|---|
| `ext::deps` | a dependency other than `blaze-ext`; a build script; `proc-macro` | ``the dependency `libc`: an extension may depend only on blaze-ext, which blaze itself vouches for; ..`` |
| `ext::api` | another surface version, or none stated | ``it is written for blaze-ext 0.7, and this blaze has blaze-ext 0.1.0: the API is not stable .. ; changed since: ..`` |
| `ext::no-std` | no `#![no_std]`; a path through `std` | ``a path through `std`: an extension may not use `std`, whose functions reach files, sockets, the clock ..`` |
| `ext::unsafe` | the keyword, anywhere | `` `unsafe`: an extension is safe Rust only; what needs `unsafe` belongs behind `blaze-ext` `` |
| `ext::extern` | foreign blocks and ABIs, `extern crate` | `` `extern`: an extension declares no foreign function, block or ABI `` |
| `ext::asm` | `asm!`, `global_asm!`, `naked_asm!` | `` `asm!`: an extension has no inline assembly `` |
| `ext::global-state` | `static`, `thread_local!` | `` `static`: an extension keeps no state between jobs, so two runs give one result; use a `const` `` |
| `ext::attribute` | `no_mangle`, `link_section`, `export_name`, `used`, `panic_handler`, .. | ``the attribute `no_mangle`: it names a symbol, a section or a runtime hook, ..`` |
| `ext::feature` | a feature gate other than `allocator_api` | ``the feature `rustc_attrs`: an extension may turn on only allocator_api; ..`` |
| `ext::include` | `include!`, `include_str!`, `include_bytes!`, `#[path]` | `` `include_str!`: an extension reads no file, when compiled or when run; .. `` |
| `ext::entry` | no `pub fn extension` in `src/lib.rs` | ``the crate root has no `pub fn extension() -> blaze_ext::Extension`: ..`` |
| `ext::grant` | a grant (until bstd's capability types exist) | ``it is granted `clock`, a capability this blaze cannot hand out yet: ..`` |

They read tokens, not the expanded tree: a keyword is refused wherever it is written (a macro's definition, a
`cfg` that is off), and with no proc macro nothing can make the token `unsafe` from others. The global heap is
out by construction: `alloc` is not nameable, `blaze_ext::Vec` comes only from `cx.vec()`. With bstd
([std.md](std.md)) the allowed dependencies gain it and a grant becomes a field of `Cx`: a manifest change.
**What this rests on:** the soundness of these checks and of blaze's own front end, nothing else. A hole in
either (a rustc soundness bug blaze copies, a `blaze-ext` method that lets safe code forge a capability, a
form the token check misses) is arbitrary code in the daemon. An **out-of-process mode** for untrusted
extensions would build the same source for `wasm32` and run it as proc macros run, at the cost of a serialized
mirror of MIR and every query, a copy of each body per call, and live patching. Not in slice 1.

## Built, loaded, bounded, deterministic

- **Built** as a proc macro's crate is planned (its own unit, for the host), but for the daemon's triple by
  stencils or LLVM (never Cranelift), metered (`Spec::fuel`), against the API crates' metadata from this
  blaze's own build, linked against the daemon's image. **Keyed** by `build_key`: blaze's code identity,
  toolchain, triple, `blaze_ext::VERSION`, the manifest, every source file, the grants. **Cached** in the store.
- **Loaded** by `load(image) -> Loaded` into `blaze_inproc::exec::Executable` (written once, then
  read-execute, unmapped on drop), the fuel runtime's symbols resolved to `blaze_ext::host::{fuel_cell,
  fuel_out}`; `Loaded::linked` for one linked in. **Patched**: an edit is a new key and a new image beside the
  old; slice 3 patches one function in place through blaze-live's slots. **Unloaded** when the daemon drops
  the `Loaded` and its last job has ended.

A job is one call of one lint or fact function on one body. **Fuel** is a count of steps, not a deadline:
metered code ticks at every function entry and loop head (`blaze_stencil::fuel`, as `blaze test --fuel`),
every API call costs a step, and at zero the job unwinds with `Stop::Fuel`: the same step on every machine,
where a deadline would make a diagnostic depend on load. **Stack**: the tick at a function's entry holds the
depth since the job began to a cap. **Memory**: `Arena`, findings and their text count against caps (64 MiB).
A **panic** or a stop fails that job with a typed error (``extension `trouble`: lint `trouble` panicked on
`boom`: ..``), drops what it reported, and the check goes on. A fact that failed stops whoever asks for it.

**Determinism.** An extension has no clock, randomness, environment or state between jobs; what is left is
addresses (`{:p}`): with `replay = true` every job runs twice, its arena moved, and two results that differ
are an error naming the job. A diagnostic's lint is `<extension>::<lint>`; its help names the extension.

## Facts, `blaze why`, transforms, and blaze's own passes

The host orders the call graph into strongly connected components, callees first, computes an acyclic one once
and runs a cycle from each member's `bottom` until a round changes nothing, at most `8 + 64 x members` rounds,
then refuses it naming the fact and an item. A fact's key is the extension's build key, the fact's name, the
item's identity (its code hash and what the host told the extension of it) and the value of every fact it may
ask for: nothing is invalidated by hand. `blaze why` says it by name: ``fact `may_panic` of `f1` (extension
`demo`) was computed again: the fact `may_panic` of `f2`, which `f1` calls, changed``. Slice 1 keeps values in
memory; slice 2 writes each as a blaze-format record in the store (rule `outputs-are-records`).

**Transforms (slice 2).** `#[blaze::transform(ext::name)]` on a function is the only way one runs. It takes
the built MIR, before borrow checking, and returns MIR, on which blaze runs what it runs on written code: the
MIR validator (structure), type checking of every assignment, call and projection (to write: the validator
checks shape today), unsafety checking (no operation needing `unsafe` the input lacked), borrow checking
(`borrowck_rewritten`) and its attributes' effect checks; not that the output means what its author intended.
The transform's build key is in the item's code hash, so every key downstream carries it.

| blaze's own pass, the proof the API suffices | as an extension | what the API must offer (beyond 0.1.0) |
|---|---|---|
| effect checker (`hir-ty-pure`) | a fact (`Summary`) with a fixpoint, plus a lint on marked fns | monomorphized bodies and dispatch (closed-world callees), attributes of an item |
| `blaze-lints`' `blaze::` lints | lints; shared helpers as facts | types and layouts (`Ty`, size), HIR expressions or source ranges for fixes, config values |
| symbolic differentiator (`blaze-diff`) | a transform: `lift(body)`, `lower(expr, signature)` | MIR construction (a builder), the type queries, fresh locals |
| MIR-to-Lean emitter | a fact (the Lean term) and an output file | the `out:<dir>` grant: a write-only directory capability |

## Rule changes proposed (for Andrew; nothing is edited)

- `sandbox-everything`, `line`: "Build scripts and proc macros always run sandboxed; where no sandbox can be
  made, blaze stops; the one exception is a checked native extension." `info`, appended: "A checked native
  extension (`checked-native-extensions`) is the one class of user code that runs with no OS sandbox."
- `proc-macros-on-wasm`, `info`, appended: "An extension (`checked-native-extensions`) is not a proc macro: it
  is native code from stencils or LLVM like any other, and never a way to run a proc macro natively."
- New rule `checked-native-extensions` (skill `compiler-correctness`), `line`: "An extension (a user's lint,
  fact or MIR transform) runs natively in the daemon only when blaze compiled it from source and its checks
  passed: no `std`, `unsafe`, FFI, asm or global state, every capability a `Cx` argument." `info`: "A third
  class of user code beside build scripts and proc macros. blaze compiles it itself against its own API
  crates, keyed by blaze's code identity, and loads it in process; its safety is the language's, enforced by
  the `ext::` checks, with fuel, a stack cap and a capped arena per job. A hole in a check is arbitrary code in
  the daemon: each check has a test of a refused extension, and a new way out is a new check first."

## Phases (each with its check), the further direction, what is not decided

1. **Slice 1 (built).** The API, the checks, keys, `load`, jobs, lints, the facts fixpoint. Check: `blaze test
   -p blaze-ext -p blaze-ext-host` passes on a named commit.
2. **Slice 2.** The daemon's `Toolchain` (the unit kind, the link against its image), `blaze check` running a
   manifest's extensions, sources in `blaze.lock`, facts as store records, `Ty`, transforms. Check: an
   edit-harness step: nushell names `panic-in-drop`, `blaze check` prints its findings, a new blaze rebuilds it.
3. **Slice 3.** Live patching, grants on bstd, `blaze why` wired, `blaze::panic_in_drop` deleted for the
   extension (rule `one-concept-one-home`), then the effect checker. Check: the body-edit budget holds for an
   extension's own source.

**Further direction: reflection of function bodies.** [comptime.md](comptime.md) reflects a type's fields and
a function's `fn_info`. Extending it to a function's body makes an analysis compile-time Rust over reflected
code: lints, derives, proc macros and analyses become one mechanism (rule `library-over-language`). Carried
over unchanged: the manifest table, `Fact`/`Analysis` and the fixpoint with its keys, `Cx` as the only
authority, the diagnostics path. Replaced: the native load and fuel runtime, `blaze_ext::mir` (becomes
`core::reflect`'s body types) and the token checks (const code has no IO to refuse).

**Not decided.** `{:?}` of a MIR type can show names and interned ids the item's identity does not hash (wrap
MIR in facade types without `Debug`, or hash the body's print into a fact's key). Non-generic `core`
functions an extension calls are the daemon's unmetered copies: they end, but their steps are not counted.
`alloc`'s types on the arena; lint results kept as facts are; jobs on `blaze_graph::par`; a `Loaded`'s
descriptor says `'static` for what lives as long as its image.
