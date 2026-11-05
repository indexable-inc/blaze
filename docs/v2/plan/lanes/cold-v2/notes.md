---
status: plan
summary: lane cold-v2: cold builds that do almost nothing. Dependencies' results come from the global cache by content hash, and of std and lint-capped dependencies only what the build's programs reach is type-checked and compiled; what is done, what each piece waits on, and how the cold time is measured.
date: 2026-10-30
---
# cold-v2: cold builds that do almost nothing

Andrew: cold builds must beat cargo. Two pieces, both in v2. Measured cold before v2 (coldfast,
`bench.py cargo`, 16 vCPU): blaze 126-148 s against cargo's 48.6 s. Build scripts and proc macros
take 52 s, name resolution 14.9 s, load 13.8 s and codegen 24.6 s.

## Piece 2: only what is reachable (written)

Superset-of-rust lets blaze accept more than rustc. A program must build and behave as under
rustc, but code it can never reach needn't be type-checked, borrow-checked or compiled.

**Which crates.** `reached_only` (`crates/blaze-cargo/src/serve/unit/compile.rs`) covers a crate
of the standard library and any package cargo caps the lints of (registry, git: `capped`). These
are exactly the crates whose diagnostics cargo never shows a user, so only an error could
differ. A workspace member or a path package is checked and compiled whole, since users want
their own errors and warnings.

**What changes for such a crate's unit:**
- **No check runs.** If the store serves a whole-crate check outcome (a full build here, or the
  global cache's builders), it is replayed as before.
- **A library unit compiles no roots of its own.** It writes its placeholder `rlib` and is done.
- **Its build scripts and proc macros are still built.** They are executables with roots of their
  own, but they run no check.

**Why the program stays the same.** A program's codegen walk already compiles every function it
reaches in any crate, from `HirFrontend::roots` (`crates/hir-ty-codegen/v2/roots.rs`). Those roots
are rustc's collector's:
- the C `main` over `lang_start`;
- tests and benches;
- the allocator shim (alloc, OOM);
- every linked crate's kept symbols: `#[no_mangle]`, `export_name`, std's linked-by-name symbols,
  `#[panic_handler]`, the personality, and `#[used]` and exported statics, including those
  declared in bodies.

ctor, inventory and linkme register through `#[used]` statics in a `#[link_section]`. rustc keeps
a bare `#[link_section]` item only through those indicators too. The walk's edges are:
- calls and reified function pointers, which covers functions handed to C as callbacks;
- vtables, with every method of every vtable the program makes;
- statics, and `asm!` symbol operands.

A library unit's own roots never reached the program's link: the program links what its walk
reached (`Built::inputs`). So dropping them changes no program's code. Only the skipped checks
widen what blaze accepts. A reached body that does not type-check still stops the walk, with an
error naming the function (MIR build refuses a body with errors).

The gap in roots is `#[linkage]` definitions and EII implementations (request 00843 to codegen).
It is older than this change, but reachable-only makes the roots the only way in.

**Cache keys.** The unit's record key hashes the mode, so a record made reachable-only is never
replayed for `BLAZE_DEPS=full`, and the reverse. Per-function codegen keys are unchanged: a
function is the same compiled code whichever unit compiled it.

**`BLAZE_DEPS=full`** builds every dependency whole. It exists for the differential run and for
the builders that publish to the global cache (request 00844). Any other value is an error
naming it. The `-v` served report lists a reachable-only crate that was not served as refused,
with the reason.

**Proof** (`harness/v2/reachable.py`, by child cold-v2-reachdiff). For each harness app and
profile it makes two cold builds, `BLAZE_DEPS=full` and default, and compares them:
- each program's output for every command line in the app's `runs` (status, stdout, stderr);
- the tests and their outcomes;
- the exported and dynamic symbols of each delivered executable and shared library.

It also times both builds: those are the before and after cold times.

## Piece 1: dependency results from the global cache (others' code; what it waits on)

The protocol, trust and publicness are written in `crates/blaze-global` (lane fastcompile,
`docs/v2/plan/lanes/fastcompile/global.md`):
- Ed25519 records by a pinned publisher, then the content hash, before the store keeps a value;
- bounded, size-checked HTTP bodies;
- a lock file's cold build as one signed bundle.

`blaze_cas::Store::fetch` verifies again. A value that fails either check is
`Fetched::Refused`: an error of the unit naming the key, never a rebuild (no-fallback). An
unreachable cache is refused too. Only a cache that answers "missing" leads to computing locally.
That is a miss, not a fallback.

| part | where | state |
|---|---|---|
| dependency name resolution (def maps, lang items, signatures) by image key | `hir_def::deps`, keyed machine-independently (path markers, `Refused::MachinePath`) | written; reads the local store only. `ReadThrough` over the global client is request 00693 (cargo-inbox) |
| type check (check outcomes, bodies), constants, goals, item-hash's infer/MIR/borrowck records | `blaze_diagnostics::cache`, `consteval::deps`, `goal_store`, `hir_ty::body_store` | stored locally; no public rule, so neither fetched nor published (request 00842, fastcompile) |
| codegen | release (LLVM) functions: `Rule::function`; dev functions are never public | dev functions asked in 00842 |
| build scripts, proc macros | `Rule::run`, `serve/unit/script/global.rs` | written, undeclared: waits on usercode's `Prepared.made`; run keys hold absolute paths (cargo.md, fastcompile-global 3 item 3) |
| the daemon's client, completion routing, cold-start bundle | cargo-inbox, fastcompile-global 3 | not written: no client can exist while `sign::PINNED` is empty |
| the publisher key and the builders' publish job | 00680 / 00844, coordinator and ci | needs Andrew: the key is a secret no agent can make |

**The design the pieces add up to.** At a cold start the daemon downloads the lock's bundle:
- every key the builders' whole build of the lock's crates.io dependencies made;
- with `BLAZE_DEPS=full`: images, check outcomes and bodies, constants, goals, item-hash
  records, library functions, build-script and proc-macro runs;
- in parallel, while planning.

The front end then reads them from the local store, as it reads its own (`Images`). A per-key
fetch (`Client::fetch` on a public miss) covers what the bundle lacks.

A worker never waits on the network for a key it could have had in the bundle. A cold build is
then the bundle's bytes over the link, plus the app's own crates, plus the generic instances only
its code makes. Piece 2 bounds those instances to what the program reaches.

## Measuring (the coordinator runs it)

- **Piece 2, before and after**, one app (ripgrep, dev): `harness/v2/reachable.py`'s two cold
  builds give both times (command in the lane report).
- **Piece 1**: nothing can be measured until 00844's key exists. Then run the same app cold with
  an empty store, against a cache the builders filled, beside cargo's cold build:
  `harness/coldbuild.py bench ripgrep`, which prints blaze's cold time against cargo's.
