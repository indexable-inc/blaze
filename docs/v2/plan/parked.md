---
status: current
summary: The crates kept on disk but out of the build (the root Cargo.toml's `[workspace] exclude`) since the old item layer, the query database, the daemon and the lints were deleted on 2026-11-15, each with what it waits on.
---
# Parked crates

On 2026-11-15 the old item layer was deleted to be rebuilt from scratch
([design/bottom.md](design/bottom.md)): the daemon (blaze-cargo's serve, blaze-api, blaze-client),
the query database (blaze-query), the lints (blaze-lints), the old name resolution and stored
pages of hir-def, most of hir-expand and base-db's input graph. A crate kept that no longer
compiles is parked: it stays on disk, unedited but for dependency lines on deleted crates, and is
listed in the root Cargo.toml's `exclude`. A crate leaves this list when it compiles again on
the new layer; delete its line here then.

Many of them used blaze-query only for its fact kinds (`QueryKind`, `QueryKey`) or its `carry`;
those uses wait on the records of the new layer's keys.

## Waiting on the new item layer (Items, Source; design/bottom.md)

- `crates/base-db`: its input graph, file roots and database were deleted; what is left (crate
  attributes, editioned file ids, reparse, settings) waits on the `Source` trait.
- `crates/hir-expand`: its database and file layer were deleted; `builtin`, `declarative`,
  `eager`, `hygiene`, `span_map`, `name`, `mod_path`, `fixup` wait on base-db's replacement.
- `crates/hir-def`: name resolution, `DefMap`, the resolver, item scopes, import map, find path,
  lang items and stored pages were deleted; body lowering and the item tree wait on `Items`.
- `crates/mbe`: takes `&base_db::Db` (lib.rs, expander.rs); waits on base-db's replacement.
- `crates/proc-macro-api`: base-db, hir-expand.
- `crates/hir-lint-levels`: base-db, hir-def, hir-expand (read by hir-ty-mir and hir-ty-unsafeck).
- `crates/hir`: hir-def, hir-ty and the checkers.
- `crates/hir-ty` and every checker on it (`crates/hir-ty-borrowck`, `crates/hir-ty-coherence`,
  `crates/hir-ty-comptime`, `crates/hir-ty-identity`, `crates/hir-ty-lean`, `crates/hir-ty-linear`,
  `crates/hir-ty-matchck`, `crates/hir-ty-mir`, `crates/hir-ty-print`, `crates/hir-ty-pure`,
  `crates/hir-ty-share`, `crates/hir-ty-specialization`, `crates/hir-ty-suspend`,
  `crates/hir-ty-tailck`, `crates/hir-ty-ub`, `crates/hir-ty-unsafeck`, `crates/hir-ty-wfcheck`):
  hir-def and hir-expand, and blaze-query's fact kinds; the type checker is to ask through
  `Items` and `Source`.
- `crates/hir-ty-codegen`: hir-ty, blaze-codegen, blaze-stencil, blaze-query.
- `crates/blaze-items`: hir, hir-def, hir-ty, blaze-query.
- `crates/blaze-item-hash`: hir-def, hir-ty and its checkers, blaze-query.
- `crates/blaze-front`: hir-def, hir-ty, blaze-items, blaze-query.
- `crates/blaze-shader`: hir-def, hir-ty-mir.
- `crates/blaze-doc`: hir, hir-def.
- `crates/blaze-diagnostics`: its lint passes were deleted; what is left (the rendering of the
  type checker's errors, `lower/`, `checks/`, emit, print, levels) waits on hir and hir-ty, and on
  the request types blaze-api had.

## Waiting on the build graph off blaze-query

- `crates/blaze-units`: blaze-query's fact kinds.
- `crates/blaze-registry`, `crates/blaze-metadata`, `crates/blaze-tree`: blaze-units.
- `crates/blaze-usercode`: blaze-query, blaze-items.
- `crates/blaze-testsel`: blaze-usercode.
- `crates/blaze-macro-eval`: blaze-query, blaze-lower (deleted), hir-def, hir-ty-mir.
- `crates/blaze-global`: blaze-codegen, blaze-diagnostics, hir-ty.

## Waiting on code records (the back end without the daemon)

- `crates/blaze-codegen`: blaze-api, blaze-query, blaze-items.
- `crates/blaze-stencil` (the copy-and-patch stencils): blaze-query, blaze-lower, blaze-codegen,
  hir-ty-codegen.
- `crates/blaze-lazy` (compile on first call): its default feature `daemon` (plans, stubs, the
  fetcher) needs blaze-codegen, blaze-items and blaze-stencil; without `daemon` it needs only
  blaze-core, blaze-bytes, blaze-inproc and blaze-spin. Waits on being re-seated on code
  records: a stub calls the resolver, which reads the code record by its key, made on a miss by
  the program's helper.
- `crates/blaze-link`: blaze-live (deleted), blaze-codegen, blaze-items, blaze-lazy's `daemon`;
  the same re-seating as blaze-lazy.
- `crates/blaze-launch-host`: blaze-codegen, blaze-items, blaze-link; the same re-seating.
- `crates/blaze-launcher`: blaze-client (deleted), its request path to the daemon; the same
  re-seating, the launcher asking the program's helper.
- `crates/blaze-command`: the no-daemon request, on hir-ty, blaze-front, blaze-codegen, blaze-link,
  blaze-units.

## Waiting on a command line without the daemon

- `crates/blaze-cargo`: its daemon (serve, socket, inspect, resume, daemon client) was deleted;
  `cli.rs` still routes through them. Waits on the CLI made over blaze-command alone.
- `crates/blaze-certify`: the request certificate, on blaze-api's request types (deleted).
- `crates/blaze-thin`: the thin `blaze`, a round trip on the daemon's socket (blaze-api,
  blaze-client deleted) and blaze-certify.
- `crates/blaze-engine`: blaze-cargo, blaze-query, blaze-lower.
