---
status: record
summary: Rust Glancer, matklad's three tiers, rustc's .rmeta MIR and salsa's persistence, and what they mean for blaze.
date: 2026-10-21
---
# Rust Glancer, matklad's three tiers, and what they mean for blaze

2026-10-21. What was fetched is in [docs/upstream/glancer/](../upstream/glancer/SOURCE.md). Each
claim is marked **read** (in a source we read, or measured), **reported** (someone says so and we
did not check) or **unverified** (our inference). The plan this feeds is
[docs/glancer.md](../glancer.md).

## What Rust Glancer is

- An experimental Rust language server by a developer other than matklad, announced August 2026,
  at v0.2.0 on 2026-08-31; repository `rust-glancer/rust-glancer`, read at `3c36614b2b` (read:
  its docs and changelog).
- Goals, in its own order: under 100 MB RSS per active engine "for any realistic project",
  fast indexing and near-instant restart with an existing index, maintainable "stupid" code,
  and about 90% of a complete LSP (read: `docs/intro/SCOPE.html`).
- What it gives up for that: type inference and trait solving "not expected to be complete",
  no proc-macro execution, no build-script running (v0.2.0 reuses outputs cargo already left),
  and module-level changes only after save (read: SCOPE, changelog #180).
- It uses Chalk for trait solving and covers most ordinary syntax (reported: a news summary,
  not checked in the code).

## Its model: eager, frozen, offloaded

- **Frozen, not incremental.** It indexes the workspace once into a `Project`, "a frozen
  snapshot", and answers queries against it; a change means reindexing at least the package
  that changed, never updating in place (read: `ARCHITECTURE.html`, "Frozen workspaces").
- **Phases over the whole workspace:** `cargo metadata`, parse, item tree, def map (with
  declarative macro expansion), semantic IR (items, impls), body IR (types inside bodies).
  Bodies of dependencies are not analysed by default (read: ARCHITECTURE, "Engine").
- **Offloading per package.** After indexing, each package's def maps, semantic IR and body IR
  are written to disk and loaded only for the duration of one query, then dropped. This is
  "the biggest win" for idle memory, and the same files make a restart near-instant (read:
  ARCHITECTURE, "Package offloading"; MEMORY.html).
- **Cache key.** A BLAKE3 fingerprint built from tagged, length-prefixed fields: package name,
  source, edition, targets, dependencies, cfg options, and a per-file source revision; plus a
  schema version in the header. It deliberately has no "analysis producer" version: the author
  rebuilds the cache by hand when the analysis changes (read:
  `storage/cache/fingerprint.rs`, `storage/cache/header.rs`). blaze cannot take that shortcut:
  the store keys on its binary's hash (read: [cache.md](../cache.md)).
- **Per-crate shards.** v0.2.0 split def-map and semantic artifacts per crate because packages
  with many targets (bevy, tokio) made one artifact per package enormous, and made bodies of
  auxiliary targets lazy; "indexing time, used RAM and artifact sizes for tokio/bevy drop
  drastically" (reported: PR #172 and changelog #191, no numbers given).
- **Startup probing is opportunistic:** a package whose cache fails to match is reindexed from
  source, others load (read: MEMORY.html, "Startup cache loading").

## Its memory discipline

All read from `docs/development/MEMORY.html` unless marked.

- **Layered allocation.** One phase is run for every package before the next phase starts,
  then compacted, so transient data of a phase is freed as one contiguous region rather than as
  holes between longer-lived allocations of the next phase. Parsing is not interleaved with
  item-tree lowering for the same reason.
- **Compaction.** Shrink every builder, allocate the frozen vector in one piece while the old
  objects are alive, then drop the old ones. Bump allocators were tried and gave similar results
  with worse ergonomics.
- **Eager eviction.** Syntax is dropped once item trees exist; body lowering reparses one file,
  lowers its bodies and drops its syntax.
- **Purging and allocator choice.** mimalloc by default (faster, as compact as jemalloc);
  jemalloc purging is optional. Purging lowers RSS between phases, not peak within one.
- **Parallelism costs peak memory.** Declarative macro expansion on 16 threads: def map in about
  3.2 s at about 3.1 GB peak; capped at 2 threads, about 3.9 s at about 1.8 GB (reported: their
  measurement on rust-analyzer's own tree).
- **Their profile of indexing rust-analyzer's tree** (reported, their table): parse 150 ms,
  item tree 1.2 s, def map 3.8 s, semantic IR 0.25 s, body IR 1.9 s, cache write 1.3 s, 9.5 s in
  all; retained memory about 600 MB before offload and 7 MB after.
- **One engine per process**, partly so that two engines' allocations never interleave (read:
  ARCHITECTURE, "Engines in separate processes").

## matklad's post on it (2026-08-21)

Read in full: `matklad/2026-08-21-rust-glancer.html`.

- **Rowan is for the 1%.** Rowan's design serves incremental reparsing and DOM-style
  refactoring edits; "the 99% use case is all the code in your 6666 dependencies which you won't
  ever look at, but which needs to be at least shallowly analyzed", for which "the primary AST
  structure should be just a list of arrays" (read).
- **Bodies lazily, not incrementally:** index all items, analyse function bodies only for the
  open file (read). For blaze the analogue is that only bodies whose code is needed are lowered,
  which is already how its MIR queries are driven (unverified).
- **Proc macros made rust-analyzer bulky.** Expanding them runs real code, so the usual IDE
  shortcuts do not apply, and they generate a lot of code; at one point he measured about 30% of
  rust-analyzer's binary size attributed to JSON parsing code (reported, his recollection). He
  suggests the Sorbet approach: do not run metaprogramming, have plugins "explain" its effects
  (inject an empty `impl Serialize for T {}` instead of running serde) (read). blaze cannot do
  that: it needs the real bodies to generate code (unverified, but follows from "rustc is the
  reference").
- **The LSP race.** rust-analyzer's core insists on consistent snapshots; LSP only allows being
  heuristically right, unlike the Dart Analyzer protocol's sound synchronisation; and
  rust-analyzer's own file watcher "was fundamentally racy" with the platform-specific work to
  make it correct never done (read). For blaze, which builds what cargo would build, a racy
  watcher is a correctness problem, not a cosmetic one (unverified).
- **The three backends of IntelliJ's PSI**, the "half-drawn horse": open files backed by
  concrete syntax trees; other project files by stub trees (compact on-disk external
  declarations, no bodies), switching to syntax when opened; dependencies by compiled `.class`
  files. rust-analyzer "shouldn't use salsa for all those 6666 dependencies"; it should read
  rustc's `.rmeta` and switch to salsa "transparently, only when the user starts messing around"
  in `~/.cargo/registry/src` (read). The prerequisite is an abstract API over Rust code with an
  "rmeta-transparent" requirement, started in a HackMD design and "never completed" (read; the
  HackMD was not fetched).

## matklad, earlier

- **2020, "Next Few Years":** rust-analyzer not persisting caches was "a very valuable asset"
  that keeps the from-scratch path fast; "it is plausible that we don't actually need persistent
  caches at all" for tens of megabytes of input; and "if Rust stuck with `.crate` files,
  implementing IDE support would have been much easier" (read:
  `matklad/2020-05-18-next-few-years.html`). The 2026 post moves away from the first point for
  dependencies.
- **2018, internals thread on RLS and IDE support:** sources as an in-memory set of file trees
  the compiler does no IO around, plus `add_crate_meta(CrateId, CrateMeta)` so that
  "google-scale codebases" can download pre-analysed dependencies from a build server (reported:
  the excerpt Exa returned; the thread was not saved).
- **2024, "What is a dependency?":** a dependency's primary identity is a checksum over its
  file tree; locations (registry, global content-addressed cache, local cache, vendored copy)
  are interchangeable once the checksum matches (reported: the excerpt Exa returned).

## What rustc keeps for a dependency

From rustc 1.99.0's `rustc_metadata/src/rmeta/encoder.rs`, `should_encode_mir` (read, saved as
`rustc/1.99.0_encoder_should_encode_mir.rs`):

- Optimized MIR is encoded for functions, methods and closures that are reachable and either
  need monomorphization (generic) or are cross-crate inlinable (`#[inline]`, and small leaf
  functions rustc decides on), and for every coroutine (its layout needs it).
- `mir_for_ctfe` is encoded for `const fn`s, constants and constructors.
- Everything else has no MIR in `.rmeta`; its code exists only in the rlib's objects.

So a frozen dependency artifact that holds MIR for that set holds what rustc itself needs to
compile code against the dependency, and nothing blaze's code generator needs is missing
(unverified: blaze also needs the MIR of non-generic functions it compiles itself, but it
compiles only instances homed in a dependency, which are exactly the generic and inlinable ones;
see `home.rs`).

## salsa's persistence

- salsa 0.28.5, the version blaze pins, has a `persistence` feature: inputs, interned values,
  tracked structs and tracked functions marked `persist` serialize with serde, with their
  revisions and durabilities, and the database can be restored from that (read:
  `salsa/0.28.5_tests_persistence.rs`, `Cargo.toml` features).
- rust-analyzer does not use it: no `persist` in `vendor/rust-analyzer/crates` (read: `rg`).
- Every persisted value type must implement serde, which for rust-analyzer means hundreds of
  types across `hir-def`, `hir-expand`, `hir-ty` and the next solver's interned types
  (unverified: not counted).
