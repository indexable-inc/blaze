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
| `FileItems` | the file's bytes, the edition, the outline's version | the file's items in order: kind, name, the byte range of its signature and of its body, its attributes' tokens, `mod`/`use`/macro-call entries. No tree is kept. |
| `ModuleDecls` | the module's `FileItems`, the outcomes of the cfg predicates it mentions, the edition | the names the module declares itself, by namespace; an item under an attribute macro is not a declared name until it expands |
| `ModuleScope` | its own `ModuleDecls`; valid by its per-name read list | bindings by (namespace, name): target key, visibility, origin, state; and an `Exports` digest per visibility class |
| `Expansion` | the call's tokens, the macro definition's content (not its item key), the hygiene facts | the items or tokens one macro call makes |
| `Signature` | the item's signature text and the keys its paths resolve to | the lowered signature (types by item key) |
| `ImplHeader`, `ImplIndex` bucket | the impl's header text and resolved keys; per (crate, trait or self-type shape) bucket | trait ref, self type, generics; the impls of a bucket |
| `BodyFacts` | the body's text, the signatures it reads | what a body gives others: hidden types, const values, discriminants |
| `KeyIndex` page | the module's item keys | key to place, so `resolve` is one probe |
| `LangItems`, `CrateGraph`, `NoImplsOf` | the crate's roots | lang items across the graph; the crate graph; "crate C has no impl of T", so an impl lookup does not probe every crate |
| `Fields`, `Variants`, `AssocItems`, `Predicates`, `Supertraits`, `TypeAttrs` | each item's text and resolved keys | split from `Signature` so each is cut off on its own; `repr`, `non_exhaustive`, `fundamental`, stability, `#![feature]` |

## Name resolution without resolving a crate

Reviewed by an outsider on 2026-11-15; the first form of this section (an "import group" found
before resolution) was wrong: what `use a::b::*` names is itself a resolution, item macros add
`use`, `mod` and `#[macro_export]` only after they expand, and grouping by module invents
cycles that are not cycles of names (a child's `use crate::Y` with the root's `pub use
child::*`). No per-name, demand-driven resolver of Rust is known to exist; rustc and
rust-analyzer resolve a crate whole because expansion and imports interleave. This is the part
of the design that is new.

- **One `ModuleScope` record per module, read per name.** A reader asks `(module, namespace,
  name)` or the module's `Exports` digest for a visibility class (what a glob reads). Its read
  list holds exactly those, so a private fn added to a module moves no reader that did not
  ask for that name, and no glob outside it.
- **No key holds another record's key.** A scope is found by its own text facts and is valid
  by its read list (read-addressed), never keyed by the scopes it imports from: that cascade
  is what read-addressing removed.
- **A binding has states.** Target key, visibility, origin (declared, explicit import, glob,
  expanded), and `undetermined` or `ambiguous` as values, not errors. Explicit bindings are a
  layer below globs, so the fixpoint is well defined; an import waits until its answer is
  final; a macro path resolved early records the scope it hit and is checked against the final
  scope with rustc's ambiguity kinds, which is what makes any evaluation order agree with
  rustc.
- **Cycles are found at run time.** A glob read that reaches a module still being computed
  joins a strongly connected set, solved as a least fixpoint; each member is written as its
  own record naming its fellows. Most modules are alone.
- **The other records names need.** `ModuleDecls(M)` (keyed by the file's items, the outcomes
  of the cfg predicates it mentions, the edition); `TextualEntry(M)` (the `macro_rules!` scope
  at the `mod` line, with earlier `#[macro_use]` siblings); `ModuleFile(M)` (which file, with
  the negative reads of `foo.rs` and `foo/mod.rs`); one crate-root record (edition, `no_std`,
  preludes, the extern prelude with `extern crate self as`, `register_tool`); `MacroDef`
  (with a derive's helper attributes); hygiene per token in `Expansion` for `$crate`.
- **What an edit redoes** (estimates of the review, to be measured): a body edit, one
  `FileItems` and no scope; a private fn added, about three records; a `pub use x::*` at the
  root, the root's set and the modules that glob it; an edited `macro_rules!`, its N
  expansions and the scopes whose names changed.
- **The risk.** If real cyclic sets are large (a root that globs children that glob `super`;
  macro-heavy modules whose expansion feeds their own imports), pull per module collapses
  into a crate-wide task. The size distribution of glob cycles on std, reth and tempo is
  measured with the old layer's resolved maps before the pull producer is trusted.

**The producer (decided with Andrew, 2026-11-15 18:37).** Three grains were weighed: one
record per crate (any new name redoes the crate), per module updated incrementally, and
per-name pull from cold. The choice is the middle one. The first time a crate is seen, all its
modules are resolved together by the known fixpoint, which settles every name; each module's
scope is its own record with its per-name read list. After an edit only the modules whose read
lists no longer hold are run again (the edited module, and readers of a name that changed,
appeared or went), and a module in a glob cycle runs with its partners. The machine is per
module and asks for what it needs, so the same machine serves both "all modules of this crate"
and "these modules again". Pull from cold (resolving only the modules a program reaches) saves
work only for a crate never seen before and not fetched, and is the one part no Rust compiler
has built: it is not built unless a measured crate-wide first pass over item lists costs more
than about 10 ms on reth's or tempo's largest crate.

## What Relax does, and what is taken from it

Relax (makepad, MIT; read at commit f05459f on 2026-11-15, source only) is the compiler this
one is judged against. What its code does, by file:

- Names: collection is eager and the import fixpoint runs over the whole program's import list
  (`front/src/program.rs`); lookups are memoised per (module, name, namespace) and every binding
  change clears the memos of the whole program. So it resolves whole, as rustc does.
- It keeps a full AST of every file for the life of the process; positions are absolute byte
  offsets; a line inserted above a function changes that function's code key.
- Cache: one mmap'd file per crate version (a header, a sorted index, append-only payloads of
  raw in-memory structs, readable only by the same build). Keys hash absolute worktree paths,
  so the cache is not shareable across checkouts or machines; std is compiled per workspace.
- `check`: a crate's verdict is keyed by every file's text (one edit misses the crate); a
  signature edit drops every body verdict of the crate; a body with a warning is never stored.
- Its speed comes from elsewhere: a function is compiled when first called (stubs), a warm
  start maps the whole resolved program in place after a stat-only certificate, a check with
  unchanged inputs prints stored diagnostics with no load, and there is no link step.

Taken from it:

1. **Compile on first call.** `run` emits stubs and compiles a function when it is first hit,
   the rest in the background; `build` compiles what is reached. This is the largest part of
   Relax's lead on time to first run and is independent of the item layer.
2. **The early record.** A check whose certificate stands prints its stored findings and
   exits before anything is loaded (the certificate here already; the findings as a record).
3. **Stat stamps with a racy window**, hashing only files whose stat changed (here already).
4. **One mapped pack per crate with a sorted index** for a crate's records (the store's packs).
5. **Packed tokens** (4 bytes each) and LEB128 position tables for `FileItems` and expansions.
6. **Function bodies skipped at load**: an outline never parses a body; a body is parsed when
   its own task runs.
7. **A key that covers what a name can find**: its per-item name graph with a Merkle hash per
   strongly connected set is the same idea as a per-name read list; blanket impls are indexed
   under the trait's name.

Not taken: absolute positions and paths in keys, crate-grain verdicts, raw in-memory structs
as the stored form, whole-program memo flushes, ASTs kept alive, dependencies read from source.

## The commands: run, test, check, export

Andrew, 2026-11-15 18:45: "no blaze build and just blaze export ... and blaze run / test /
check that are fast"; "everything in JIT world".

- **`blaze run` and `blaze test`** never write an executable. Code is records; the launcher
  places them in the program's own process behind one slot word a function. A function not
  yet compiled is a stub in its slot: the first call asks the program's helper for the code
  record (found by key, or made then), which is published in the slot; what is likely to run
  next is compiled behind it. There is no link step. A test run is a program run.
- **Tiers.** The first code of a function is the fast tier, which calls through slots. A
  background tier optimizes hot functions across functions (inlining). Its record lists, in
  its read list, the code identity of every callee body it inlined, so an edit of a callee
  makes the caller's optimized record stale and the caller runs its fast-tier record until
  the tier makes it again. A better version of a function is one slot word written.
- **`blaze check`** makes no code.
- **`blaze export`** writes a native executable (Mach-O, ELF): every function the program
  reaches, at the optimized tier, no stubs, laid out as the launcher lays them out and written
  once, whole. It is not incremental and keeps nothing between runs: nobody edits through it.
  It comes after `run`; until then the command says it is not built.
- **Gone:** `blaze build`, the linker's patching of a file in place (kept slots in a file,
  spare room, growth bounds, unwind tables patched, a signature made again per edit), and
  compiling everything reachable before the first instruction runs.

## Measured so far

- `blaze_file_items::outline` (a file's text to its items, hashes and ranges; one thread, an
  M-series Mac, release, 2026-11-15, bodies parsed in full): `core` 299 files, 180,413 lines,
  13,443 items in 132.5 ms; `alloc` 64.2 ms; `std` 170.9 ms; all of `library/` (1.53 M lines,
  207,552 items) 1,671 ms; blaze's own crates (2.63 M lines, 321,674 items) 3,332 ms: about
  1 microsecond a line, 32 to 51 MB/s. On N cores it divides by N (a file is independent); with
  bodies skipped it falls further. 9 syntax errors in `core` and 1,531 in blaze's crates are
  syntax the outline does not read yet (it parses with no extensions): to fix.

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
