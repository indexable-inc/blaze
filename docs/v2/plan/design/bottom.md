---
status: plan
summary: The item layer rebuilt from scratch, pull-based at item grain: a question about one item reads or makes one small record keyed from text, a crate is never resolved whole, a dependency is only records, and the type checker asks through two traits (Items, Source). Built beside the old layer and switched when hello-world passes on it.
date: 2026-11-15
---
# The bottom layer, from scratch

Andrew, 2026-11-15: "we can scrap everything imo and build the lints FOR this new design ...
delete old code and just start from scratch"; the order he gave for what matters: incremental
compile time (large crates and large files included), memory, cold compile, disk cleanup that
never blocks, disk use; "start with the right design and then iterate ... iterate fast".

## What it is judged by

Andrew's order, 2026-11-15, state of the art at the least on each, the first decides when two
trade:

1. Incremental compile being short, on large crates and large files too, **and after a
   restart with nothing running**; lints being fast ranks with it.
2. Getting records from remote (R2, peer to peer).
3. Memory.
4. Cold compile.
5. Disk cleanup that never blocks.
6. Disk use.

What each asks of this design:

- **After a restart.** No process, and the page cache is cold. A repeat check may read only a
  handful of pages: one certificate for the request, and nothing else when it stands. So a
  unit's records are packed (one file per unit or per pack, an index at its head), never one
  file per record; the file sweep is by stat against one small table; no source is re-hashed
  that a stat proves unchanged. A count of pages touched by a repeat is measured and bounded.
- **Lints.** A lint is a task like any other: keyed from the facts it reads of one item, its
  finding a record (rule 81). An edit re-runs the lints of the items whose facts changed and no
  others. A lint that needs a whole crate or the whole program reads per-item summary records,
  so it too is incremental. No lint walks a crate.
- **Remote.** A record holds nothing of the machine and is keyed by content, so any builder's
  record is any reader's. Pull at item grain must not become a request per item: records are
  published in packs with an index, a reader fetches the index once and then the byte ranges it
  needs in parallel; a signed statement and digest are checked before a byte is mapped. The
  same pack format serves R2 and a peer.
- **Memory.** Records are read in place; a task holds only what it pulled; the kernel's page
  cache is the only cache.

## Why

The inherited item layer (hir-def, hir-expand, base-db: 189k lines) resolves a crate whole,
names items by arena slots, keeps syntax trees alive, and lets any question about any item
fall into source text. On 2026-11-15 a cold `check --no-daemon` of hello-world cost 21 to 45 s
and 3.4 to 4.6 GiB because every item of `core`, `alloc` and `std` is resolved to answer
questions about a few hundred of them. The floor for that check is the work for the items
hello-world reaches.

## The design

1. **Pull, at item grain.** The driver asks one question (check this body; make this program).
   Answering it asks for records; a record that exists is read in place, one that does not is
   made by one small task, which asks the same way. Nothing is computed that no one asked for.
   Two askers of one record share one task (the store's flight).
2. **A record per fact per item**, keyed from text before the work (rule 65). Nothing is keyed
   or stored per crate except the crate's own small roots (its file list, its root module).
3. **Identity** is `blaze_id::ItemKey` (docs/v2/plan/design/identity.md): crate key, module
   path, kind, name, and a disambiguator. Inside a task an item is a small number local to that
   task; it is not `Portable`.
4. **Two traits** are everything the type checker may ask:
   - `Items`: any crate; every method reads records; no method can reach text.
   - `Source: Items`: the crate a task compiles; its methods take a `HasText` proof.
   A stage's entry point takes `&impl Items` or `&impl Source`; helpers behind it stay on the
   concrete database.
5. **A dependency is only its records.** A missing record is made by that crate's own task
   (which has its text) or is a typed stop naming the fact and the item; never a parse by the
   asker.
6. **A fork shares by content.** Two checkouts with equal text for an item find equal records.

## The records (the surface schema, first cut)

Each is one blaze-format record, read in place. `K(x)` is the item key of `x`.

| Record | Key from | Holds |
|---|---|---|
| `FileItems` | the file's bytes | the file's items in order: kind, name, the byte range of its signature and of its body, its attributes' tokens, `mod`/`use`/macro-call entries. No tree is kept. |
| `ModuleNames` | the module's `FileItems` keys and its parent's key | the names the module declares itself (before imports and macros), by namespace |
| `ModuleScope` | `ModuleNames` plus the scopes it imports from (by key) | the resolved scope: name, namespace, item key, visibility. Made by a fixpoint over the import group it is in (below). |
| `Expansion` | the call's tokens, the macro's definition key, the hygiene facts | the items or tokens one macro call makes |
| `Signature` | the item's signature text and the keys its paths resolve to | the lowered signature (types by item key) |
| `ImplHeader`, `ImplIndex` bucket | the impl's header text and resolved keys; per (crate, trait or self-type shape) bucket | trait ref, self type, generics; the impls of a bucket |
| `BodyFacts` | the body's text, the signatures it reads | what a body gives others: hidden types, const values, discriminants |
| `KeyIndex` page | the module's item keys | key to place, so `resolve` is one probe |

## Name resolution without resolving a crate

Globs and macros make a module's scope depend on other modules', possibly in a loop. The unit
of the fixpoint is the **import group**: the strongly connected set of modules that import from
each other by glob or whose macros are used across them. A group is found from `ModuleNames`
alone (which modules a module names in `use` and macro paths). Most modules are groups of one.
`ModuleScope` of a group is one task; a path that leaves the group reads the other group's
record. This design is to be reviewed by an outsider before it is built (it is the risk).

## How it is built and proven

- New crates beside the old: `blaze-id` (done), `blaze-surface` (the schema and the two
  traits), `blaze-file-items` (text to `FileItems`, on the existing parser), `blaze-scope`
  (names and scopes), `blaze-expand` (expansion records), then signatures and impls.
- The ladder: `fn main() {}` to a running binary through the new layer with the existing type
  checker, codegen and linker; then `println!`; then ripgrep; then reth and tempo.
- A question the new layer cannot answer yet is a typed stop `NotBuilt { method, item }`; the
  counted stops are the work list.
- The old layer is the referee: for every item of real `core`, `alloc`, `std`, each `Items`
  method answers equal through both, until the switch.
- The switch deletes, in one change: hir-def's name resolution and frozen pages, the daemon
  path and blaze-query, the old persistence forms, blaze-lints as it is. Lints are rebuilt on
  the new layer, a short list first (string errors, implied allocators, dropped errors,
  ambient reads, unnamed or unbounded waits, map-order output, a key ignoring an input,
  `todo!`).
- Until the switch the old layer is frozen: no fixes that are not needed to keep main's smoke.

## Floors, to report against (rule 67)

- Cold check of hello-world: the items reached (a few hundred), each parsed from one file
  record and resolved by one scope probe. Target: under 1 s and under 200 MiB with std's
  records fetched; today 21 to 45 s and 3.4 to 4.6 GiB.
- Repeat with nothing changed: one certificate look. Target: under 20 ms; today 2.2 to 3.3 s.
- Body edit: one `FileItems` (re-parse of one file), one body task. Target: under 100 ms.
