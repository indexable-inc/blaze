---
status: plan
summary: blaze v2's crates: which crate holds which interface, which lane owns which paths (never overlapping), and what v2 does to each existing crate.
date: 2026-10-29
---
# blaze v2: crates and owners

Step 0 of [building.md](../process/building.md) section 12, as of 2026-10-29. The interfaces every lane programs
against are the public items of the crates below as step 0 left them: types, traits and doc
comments; step 0 left each body a lane writes as a panic naming the lane.

## Rules

- **A lane owns paths, listed below; no path has two owners.** It writes bodies, private
  items, tests, new public items and its crates' own `Cargo.toml` (with `workspace = true`
  dependencies) freely.
- **The interface items** (every public item step 0 wrote, listed per crate below) change
  only through a request: append what, where and why to `v2/requests/<lane>.md`; the
  coordinator changes it for every lane at once. The root `Cargo.toml` and `Cargo.lock` are
  the coordinator's too: a new workspace dependency is a request.
- **A new crate's `src/lib.rs` comes first**, before its `Cargo.toml`, even as one doc
  comment: a workspace member whose manifest has no library stops cargo loading the whole
  workspace.
- **IDs**: hashes, keys and outputs use [`ContentId`](../../../../crates/blaze-ids/src/lib.rs) only,
  each made with a domain string naming what it is and its schema version
  (`"blaze v2 <what> <n>"`). A dense `Idx<T>` never goes into a hash, a key or an output.
- **Step 0's unfinished bodies are the owning lane's to finish**, every one: a lane is not
  done while the check of `common.md` (no `unimplemented` or `todo` macro) finds anything in
  its paths. What it can't finish for a real reason goes in its request file and its report;
  never a default, never a path back to v1.
- A lane's design notes go in `docs/v2/plan/lanes/<lane>/` (`notes.md` and any others) (front matter `status: plan`), its
  own; its plan, `docs/v2/plan/lanes/<lane>.md`, changes by request.

## The v2 crates

Each crate depends only on crates above it in this table (and on existing crates such as
`blaze-protocol`).

| crate | holds | owner after step 0 |
|---|---|---|
| `blaze-core` (changed) | effects, completions, `Driver`, the drivers (simulation, threads, io_uring); step 0 added `Map`, `Spawn` (with `Command`, `Sandbox`, `Traced`, `Exit`), `Kill`, `Listen`, `Unlisten`, `Connect`, `Recv`, `Send`, `Hangup` | core |
| `blaze-graph` (changed) | `plane` (requests, `RequestId`, `Key`, `Budget`, `PlaneConfig`, `PlaneInput`, `PlaneOutput`: memo, single flight, fair share, critical path, admission), `admission` (`TaskKind`, `Estimate`, `Estimates`, `Arena`), `machine` (`Spec` gained `key` and `memory`), `exec` | core |
| `blaze-ids` | `ContentId` and its `Hasher` (BLAKE3, domain-separated), `Fingerprint` (128 bits), dense `Idx<T>`, the `Interner` trait mapping one to the other | format |
| `blaze-format` | the zero-copy format: `Record` (unsafe trait), `Rel`, `Slice`, `Str`, `Header`, `SchemaHash`, `Writer`, `open`, `Persist` (what a query returns, read in place as `Persist::Ref`), `SpanRef`, `FormatError` | format |
| `blaze-vfs` | the Merkle VFS (port blaze-next's `crates/blaze-vfs` into it): `View`, `ViewId`, `File`, `Mode` (regular, executable, symlink, hashed into the tree as git's 100644, 100755 and 120000), `Op`, `OpError`, `Change`, `ChangeKind`, `Commit`, `CommitError`, `Ticket`, `Recovered`, `Vfs` (heads, the empty root, views, reads, symlinks followed, diff, journal through effects, collection), `Watcher` and `Watched` (the disk as transactions), `Reader` (`Vfs::reader`: any root's files from any thread; blaze-query implements its `Tree` for it). Its view arena is the one every crate uses: a `ViewId` is a `Vfs::open` of a root, and no other crate keeps its own | vfs |
| `blaze-query` | the query database's surface: `QueryKind` and `kinds` (number ranges per lane), `QueryKey` (and its `id`, the plane's task key), `Edge`, `Memo`, `Stored`, `Input`, the `Query` trait, `Cx` (records edges), `Cancelled`, `Why` and `Changed`, `DbError`, `Database` | db |
| `blaze-items` | item identity and content: `ItemKey`, `ItemKind`, `InterfaceHash`, `LayoutHash`, `CodeHash`, `DeepHash`, `ItemHashes`, `SimplifiedType`, `NameHash`, `Fact` (section 6.2's table and the cross-item rules) with `Fact::key`, the fact `kinds`, `ImplIndexKey`, `ScopeIndexKey`. A front-end crate: it gains `hir-ty` dependencies for the hashing (port blaze-next's `crates/blaze-items`) | items |
| `blaze-cas` | the store: `Key`, `Kind`, `Entry`, `Ticket`, `Corrupt`, `Mismatch`, `Config`, `Store` (open, get verified, has, put, flush, verify, place, completions), the `Remote` tier | store |
| `blaze-codegen` | the unit of codegen: `Instance`, `InstanceKey`, `Settings`, `Backend`, `Arch`, `Unit`, `CompiledFunction` with `Reloc`, `RelocKind`, `Target`, `Data`, `CodegenError`, the `Compile` trait | codegen |
| `hir-ty-ub` | the checker of undefined behavior over hir-ty-mir's evaluator: `Checker`, `Config`, `Outcome`, `run_test`. A front-end crate | safety |
| `blaze-link` | the in-place linker: `Slot`, `Change`, `Staged`, `LinkError`, `Image` (stage, commit, roll back) | link |
| `blaze-usercode` | user code: `RunKind`, `RunKey` (code and given, then traced inputs), `Reached` (a test's code hashes) | usercode |
| `blaze-api` | the agent API: `SCHEMA` (5), `Envelope`, `ViewRef`, `Targets` (with `Cargo`), `Request` (commit, check, build, test, run, open, close, reach, best-of, verify, snapshot, why, diff, status, cancel, codegen inputs, trace, tables), `Response`, `Diagnostic` (with `FactRead` facts and rustc's `rendered` text), `Level`, `Span`, `Artifact`, `TestResult`, `TestOutcome`, `Reach`, `Ranked`, `Mismatch`, `Worked`, `Status`, `ApiError`, in `blaze-protocol`'s deterministic CBOR and frames; the daemon's side `build::{Builder, Work, JobId, Opened}`; `session`, `server`, `http`, `client` | ci |
| `blaze-ci` | the VM as CI: `Event`, `CheckRun`, `CheckStatus`, `Train`, `Checkout` | ci |
| `blaze-v2-proof` | the oracle, the corpus differential, budgets (created by proof) | proof |

How they meet: a request comes in through `blaze-api`, opens a `blaze-vfs` view, and asks
`blaze-query` queries; each query execution is a plane task (`QueryKey::task_key`), admitted
against `admission::Estimates`; queries read files through `Cx::file` and facts through
`blaze-items`' `Fact::key`; results live in `blaze-format` buffers and, once cold, in
`blaze-cas` under their key; codegen queries return `CompiledFunction`s keyed by
`InstanceKey`, which `blaze-link` places and commits; build scripts and proc macros are
`Effect::Spawn`s in a tracing `Sandbox`, keyed by `blaze-usercode`'s `RunKey`.

## Paths each lane owns

| lane | paths |
|---|---|
| coordinator (`v2`) | root `Cargo.toml`, `Cargo.lock`, `docs/v2/` but the lanes' notes folders (`docs/v2/plan/lanes/<lane>/`), `v2/requests/` (applying them) |
| cargo | `crates/blaze-cargo/` but store's and usercode's paths, `crates/blaze-lower/`, `crates/blaze-units/`, `crates/blaze-tree/`, `crates/blaze-manifest/`, `crates/blaze-metadata/`, `crates/blaze-resolve/`, `crates/blaze-registry/`, `crates/blaze-source/`, `library/` |
| diagnostics | `crates/blaze-diagnostics/` (new), `crates/ide-diagnostics/` |
| core | `crates/blaze-core/`, `crates/blaze-graph/`, `docs/v2/plan/lanes/core/plane.md` |
| vfs | `crates/blaze-vfs/` (v1 `vfs` and `vfs-notify` deleted 2026-10-30) |
| format | `crates/blaze-ids/`, `crates/blaze-format/`, `crates/syntax-tree/`, `crates/syntax/`, `crates/parser/`, `crates/span/`, `crates/intern/`, `crates/tt/`, `crates/mbe/`, `crates/syntax-bridge/`, `crates/const-ast/`; in `crates/hir-def/src/`: `expr_store.rs`, `expr_store/`, `hir.rs`, `hir/`, `item_tree.rs`, `item_tree/` |
| db | `crates/blaze-query/`, `crates/blaze-query-macros/`, `vendor/rustc_type_ir/`, `vendor/rustc_next_trait_solver/`, `crates/base-db/`, `crates/hir-expand/`, `crates/hir/`, `crates/hir-def/` but format's files, `crates/hir-ty/` but items' and codegen's files, `crates/hir-ty-borrowck/`, `crates/hir-ty-wfcheck/` |
| items | `crates/blaze-items/`; in `crates/hir-ty/src/`: `method_resolution.rs`, `method_resolution/`, `traits.rs`, `specialization.rs`, `opaques.rs`, `layout.rs`, `layout/`, `drop.rs` |
| store | `crates/blaze-cas/` |
| codegen | `crates/blaze-codegen/`, `crates/hir-ty-codegen/`, `crates/hir-ty-mir/`, `crates/blaze-engine/`, `vendor/llvm-sys/`; in `crates/hir-ty/src/`: `mir.rs`, `mir/` (the MIR types, whose layout is codegen's, [data.md](../design/data.md)) |
| link | `crates/blaze-link/`, `crates/blaze-live/`, `vendor/cranelift-object/` |
| usercode | `crates/blaze-usercode/`, `crates/blaze-build-script/`, `crates/proc-macro-api/`, `crates/blaze-cargo/src/sandbox.rs`, `crates/blaze-cargo/src/sandbox/` |
| ci | `crates/blaze-api/`, `crates/blaze-ci/`, `crates/blaze-protocol/`, `crates/blaze-ui/`, `crates/blaze-web/` |
| observe | `crates/blaze-profile/`, `crates/blaze-explore/`, `docs/profiling.md` |
| proof | `crates/blaze-v2-proof/`, `harness/v2/` |
| safety | `crates/hir-ty-ub/`, `crates/blaze-check/`, `fuzz/` |

Every other path (the cargo crate `blaze-new`; `cfg`,
`edition`, `paths`, `stdx`, `macros`, `rustc_abi`, `test-utils`,
`blaze-doc`, `blaze-fmt`, `blaze-lints`, `blaze-fuzz`, `blaze-bench`, `blaze-wasm`,
`library/`, `harness/` but `harness/v2/`, `.github/`, `docs/` but the lanes' notes) is the
coordinator's: a change there is a request.

## What v2 does to each existing crate

| crate | v2 | by |
|---|---|---|
| `blaze-core` | changed: the new effects' drivers (Linux, macOS, simulation) | core |
| `blaze-graph` | changed: the plane becomes the one scheduler, `Machine` its one-request case; the executor's cache-aware groups | core |
| `blaze-store` | replaced by `blaze-cas`; deleted at integration, 2026-10-30, with blaze-cargo's v1 remote (`src/remote.rs`, `src/remote/`): the read-through tier is `blaze_cas::remote` | store |
| `blaze-cargo` | changed: its daemon and client move onto `blaze-core`, `blaze-graph`'s plane and `blaze-api` at integration; its sandbox gains tracing (usercode); its remote store becomes the read-through tier (store) | coordinator, usercode, store |
| `blaze-engine` | replaced: the crate-at-a-time codegen tail becomes per-function units; tail deleted at integration, 2026-10-30. Stays as the `blaze` program only, the binary the release ships: `program.rs` (the sandbox launcher, the build-script `rustc`/`rustdoc` stand-ins on v2's front end in `as_rustc`, the pinned release, `pin`/`version-*`), `pin.rs`, `versions.rs`, in front of `blaze_cargo::cli` | codegen |
| `hir-ty-codegen` | changed: compiles one `Unit` to one `CompiledFunction` | codegen |
| `hir-ty-mir` | changed: instances keyed by `Instance`, code dedupe, inlining edges | codegen |
| `blaze-splice` | replaced by `blaze-link`; deleted at integration, 2026-10-30 | link |
| `blaze-in-place` | replaced by `blaze-link`; deleted at integration, 2026-10-30 | link |
| `blaze-live` | changed: patches through `blaze-link`'s slots and commit | link |
| `blaze-expand` | replaced: expansions keyed by `RunKey` with traced inputs; deleted at integration, 2026-10-30 (its code is now `proc_macro_api::{kept, bridge}`) | usercode |
| `blaze-build-script` | changed: run in tracing sandboxes, from a warm pool | usercode |
| `proc-macro-api` | changed: the store of expansions (`kept`) and the bridge blaze-macro-eval's server speaks (`bridge`); its client half deleted 2026-10-30 | usercode |
| `blaze-macro-wasm`, `blaze-wasi`, `proc-macro-srv`, `proc-macro-srv-cli` | replaced by `blaze-macro-eval`, the one proc-macro engine ([compile-time.md](../design/compile-time.md) change 20); deleted 2026-10-30 ([dedup](../lanes/dedup.md#proc-macros-one-engine-blaze-macro-eval)) | usercode |
| `vfs`, `vfs-notify` | replaced by `blaze-vfs` and `blaze-core`'s watch; deleted at integration, 2026-10-30 (`span` owns `FileId`, base-db the file sets and source paths) | vfs |
| `syntax-tree`, `syntax`, `parser` | changed: one flat tree per file replaces rowan (#42) | format |
| `span`, `intern` | changed: spans in side tables; sharded interners | format |
| `tt`, `mbe`, `syntax-bridge`, `const-ast` | changed: onto the flat tree | format |
| `hir-def` | changed: SoA HIR and outlines (format); signature and body split, parallel expansion (db) | format, db |
| `hir-expand`, `base-db`, `hir` | changed: queries keyed by input hashes with fingerprints, inputs per view | db |
| `vendor/salsa*` | removed (query-unify, 2026-10-31): the front end runs on `blaze_query::db` and `blaze-query-macros` (docs/v2/plan/lanes/db/query-unify.md) | db |
| `hir-ty` | changed: facts recorded where they are read (items); queries split and fingerprinted (db); the MIR types' columnar layout (codegen) | items, db, codegen |
| `hir-ty-borrowck`, `hir-ty-wfcheck` | changed only as `hir-ty` moves | db |
| `blaze-protocol` | left: `blaze-api` uses its CBOR and frames; its `events` stay for the build view | ci |
| `blaze-ui`, `blaze-web` | changed: clients of `blaze-api` | ci |
| `blaze-profile` | changed: blaze's one profiler: the flight recorder, the stall watchdog, the CPU sampler, the Parquet history (thread `blaze-history` in `blaze serve`), SQL over it through the pinned DuckDB (`nix run .#duckdb`, `blaze debug sql`) and the live feed (`blaze debug live`); [docs/profiling.md](../../../profiling.md) | observe |
| `blaze-explore` | new: the HTTP/3 WebTransport server on 127.0.0.1 that https://ix.dev/explore connects to, pinning its 13-day self-signed P-256 certificate by hash; builds, spans, tiles, views, SQL over the history and the live feed as Arrow IPC; the `blaze-explore-probe` echo server. Started in `blaze serve` at the first `blaze explore` | observe |
| `hir-ide` | deleted at integration, 2026-10-30: its display and diagnostics moved to `hir-ty`, its tests to `hir-ty-mir` | coordinator |
| `ide-db` | deleted at integration, 2026-10-30: callers import from `base-db`, `RootDatabase` became `base_db::Db` | coordinator |
| `test-fixture` | deleted at integration, 2026-10-30: its users run on the v2 fixture, `hir_expand::fixture` (feature `fixture`), which builds a `base_db::Db` through a `blaze-vfs` view | coordinator |
| `ide-diagnostics` | replaced by `blaze-diagnostics` ([building.md](../process/building.md) section 18), then removed | diagnostics |
| `load-cargo`, `project-model` | replaced by the workspace load as queries ([building.md](../process/building.md) section 18), the units lowered into base-db's crate graph by the new crate `blaze-lower`; deleted at integration, 2026-10-30 | cargo |
| `blaze-wasm-pack` | new at integration, 2026-10-30: a front end `blaze serve` lowered (blaze-lower) recorded as one file for `blaze-wasm`, the in-browser compiler; made by the agent API's `pack` request. A root member, so its tests run with the workspace's; no code generator, so blaze-wasm's `wasm` feature never reaches the native build | coordinator |
| `blaze-fmt`, `blaze-lints` | left; moved onto the flat tree at integration | coordinator |
| every other crate | left | coordinator |

## Left for the lanes to decide

- The flat syntax tree's and SoA HIR's types are format's own, in the crates it owns; no
  other lane programs against them before integration.
- `Query::Key`'s shape per query, the db's memo table layout, the plane's learning rule, the
  store's pack and index layout, the linker's chunking: the owning lane's.
- `Cargo.lock` gains `blake3` (a new workspace dependency, for `blaze-ids`) and the new path
  crates at the coordinator's first build.
