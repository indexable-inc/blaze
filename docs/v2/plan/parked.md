---
status: current
summary: The crates kept on disk but out of the build (the root Cargo.toml's `[workspace] exclude`) since the old item layer, the query database, the daemon and the lints were deleted on 2026-11-15, each with what it waits on.
---
# Parked crates

On 2026-11-15 the old item layer was deleted to be rebuilt from scratch
([design/bottom.md](design/bottom.md)): the daemon (blaze-cargo's serve, blaze-api, blaze-client),
the query database (blaze-query), the lints (blaze-lints), the old name resolution and stored
pages of hir-def, most of hir-expand and base-db's input graph. A second pass the same day
deleted the old path's binaries and driver (blaze-engine, blaze-thin, blaze-command, blaze-front)
and the IDE facade `hir`; each is in git history at 71767c46ce. A crate kept that no longer
compiles is parked: it stays on disk, unedited but for dependency lines on deleted crates, and is
listed in the root Cargo.toml's `exclude`. A crate leaves this list when it compiles again on
the new layer; delete its line here then.

Many of them used blaze-query only for its fact kinds (`QueryKind`, `QueryKey`) or its `carry`;
those uses wait on the records of the new layer's keys. A test that linked `hir` for hir-ty-mir's
MIR table links hir-ty-mir itself now.

## Waiting on the new item layer (Items, Source; design/bottom.md)

- `crates/base-db`: rewritten over the records (design/reconnect.md): the task's `Db`,
  `Records`, `Need`, the `Crate` input, gifts and the include table; awaiting its first compile.
- `crates/hir-expand`: its lib, files, attrs, proc-macro and cfg modules restored and moved onto
  the new `Db` (no expansion-read log, no edit, proc macros as gifts); its first compile's 13
  errors (batch 13) fixed against mbe without `db` and `Crate::proc_macro_cwd`, a base-db
  input; the hygiene and span-map tests are `cfg(any())` until `fixture::db` over an in-memory
  `Records` exists.
- `crates/hir-def`: on the records: ids from item keys (`key`), the `nameres` facade (scope
  probes, path resolution, extern prelude and lang items from `CrateGraph`/`LangItems`), the
  resolver and expression scopes, trait and impl items (the `KeyIndex` stand-in), module
  impls, crate attributes, signatures and bodies from text with a typed stop for a crate
  without it, def-site hygiene with the context table passed in. Awaiting its first compile.
  Stops, each naming a missing record (design/reconnect.md): block scopes, expansions, the
  textual macro scope at a position, `as _` imports, attributes and signatures of a crate
  without text, diagnostic items outside module scopes. Parked tests: the persisted-tree
  test that read the deleted signature table (item_tree/persist.rs).
- `crates/mbe`: takes `&base_db::Db`; waits on base-db's replacement.
- `crates/proc-macro-api`: the expansion store and bridge blaze-macro-eval speaks; on base-db,
  hir-expand.
- `crates/hir-lint-levels`: lint levels that change what a program does (const eval), read by
  hir-ty-mir and hir-ty-unsafeck; on base-db, hir-def, hir-expand.
- `crates/hir-ty` and every checker on it (`crates/hir-ty-borrowck`, `crates/hir-ty-coherence`,
  `crates/hir-ty-comptime`, `crates/hir-ty-identity`, `crates/hir-ty-lean`, `crates/hir-ty-linear`,
  `crates/hir-ty-matchck`, `crates/hir-ty-mir`, `crates/hir-ty-print`, `crates/hir-ty-pure`,
  `crates/hir-ty-share`, `crates/hir-ty-specialization`, `crates/hir-ty-suspend`,
  `crates/hir-ty-tailck`, `crates/hir-ty-ub`, `crates/hir-ty-unsafeck`, `crates/hir-ty-wfcheck`):
  hir-def, hir-expand and blaze-query's fact kinds; to ask through `Items` and `Source`.
- `crates/hir-ty-codegen`: hir-ty, blaze-codegen, blaze-stencil, blaze-query.
- `crates/blaze-items`: item keys, layouts and code hashes that hir-ty-identity, hir-ty-pure,
  hir-ty-share, hir-ty-codegen, blaze-codegen and the launcher crates read; on hir-def, hir-ty,
  blaze-query. Its keys move to blaze-id; what codegen keys on stays until then.
- `crates/blaze-item-hash`: `identity`, `reads` and `values`, which hir-ty-codegen and
  hir-ty-pure read; on hir-def, hir-ty, blaze-query.
- `crates/blaze-shader`: hir-def, hir-ty-mir.
- `crates/blaze-doc`: written over `hir` (deleted); waits on `Items` for its places and model.
- `crates/blaze-diagnostics`: the rendering of the type checker's errors (`lower/`, `checks/`,
  emit, print, levels); waits on hir-ty and on the request types blaze-api had.

## Waiting on the build graph off blaze-query

- `crates/blaze-units`: blaze-query's fact kinds.
- `crates/blaze-registry`, `crates/blaze-metadata`, `crates/blaze-tree`: blaze-units.
- `crates/blaze-usercode`: blaze-query, blaze-items.
- `crates/blaze-testsel`: blaze-usercode.
- `crates/blaze-macro-eval`: blaze-query, blaze-lower (deleted), hir-def, hir-ty-mir.
- `crates/blaze-global`: blaze-codegen, blaze-diagnostics, hir-ty.

## Waiting on code records (the back end without the daemon)

- `crates/blaze-codegen`: blaze-api, blaze-query, blaze-items.
- `crates/blaze-stencil` (the copy-and-patch stencils): blaze-codegen (`CompiledFunction`,
  `InstanceKey` throughout); its coverage test also hir-ty-codegen.
- `crates/blaze-lazy` (compile on first call): blaze-codegen, blaze-items, blaze-stencil. The
  daemon's hook, compiler and answers are deleted; a stub is to call a resolver that reads the
  code record by its key, made on a miss by the program's helper.
- `crates/blaze-link`: blaze-live (deleted: the jump table's layout, the descriptor, the
  messages to running programs), blaze-codegen, blaze-items.
- `crates/blaze-launch-host`: blaze-codegen, blaze-items, blaze-link.

## Waiting on a command line without the daemon

- `crates/blaze-cargo`: its `cli/` routes through the deleted daemon, client and blaze-command.
  What stands alone and the new command line takes: `sandbox/` (the only sandbox of build
  scripts and proc macros), `toolchain/`, `raw/`, `as_rustc`, `directives`, `unstable`,
  `release`. Waits on the CLI made over blaze-run.
- `crates/blaze-certify`: the request certificate, on blaze-api's request types (deleted); its
  `slot` is the daemon's file for its client.
