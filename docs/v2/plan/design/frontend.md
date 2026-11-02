---
status: plan
summary: blaze v2: what rust-analyzer's front end got wrong for a compiler, and what v2 does instead: incremental name resolution, indexed impls, one token format, signature-only dependencies, mapped files, no IDE-only work.
date: 2026-10-29
---
# The front end: rust-analyzer's design, fixed

blaze's front end came from rust-analyzer, which was built for an IDE: answer one question about
one file fast, and recompute the rest later. A compiler asks about everything, keeps the answers
and wants an edit to cost only what it changes. Each item below is a design choice that made
sense for the IDE and costs blaze, followed by what v2 does instead.

1. **Name resolution reruns for a whole crate on any item change.** `crate_def_map` is one
   fixpoint over every module, so adding a `use` or an item in one file reruns resolution for
   the crate and invalidates everything that read it. v2 resolves per module, with facts:
   - each import and glob records the names it read and the names it found missing (negative
     facts, [items.md](../lanes/items.md));
   - an edit reruns only the modules whose facts changed, and the fixpoint reruns only over
     the strongly connected set of glob imports the change touches.
2. **Trait impls are collected per crate.** `TraitImpls::for_crate` is one value, so adding
   any impl invalidates every trait query in every dependent. v2 indexes impls by (trait,
   simplified self type) as facts. A query reads only the buckets it asks about, and an empty
   bucket is a negative fact.
3. **Inherent impls and method candidates are crate-wide.** The same fix: indexed by
   simplified self type, read per bucket.
4. **Tokens are converted back and forth.** A macro call goes from syntax to token trees (with
   span maps) to the macro and back to syntax, then is parsed again, on every expansion. v2
   uses one flat token format shared by the lexer, the parser, `macro_rules!` and proc macros.
   Spans are side tables, so nothing is converted and an expansion's output is parsed straight
   from its tokens.
5. **Every file of every dependency is parsed and lowered fully.** rust-analyzer holds the
   sysroot and every dependency in memory as text and syntax trees. v2 reads dependencies'
   signatures only, from the store or the global cache. A dependency's body is read only
   when its code must be generated (generic, `#[inline]`, or not in the cache) or const
   evaluated.
6. **File text is a `String` per file, kept forever,** with a line index built per file. v2
   maps files from the VFS, read in place. Line and column are computed only when a
   diagnostic or debug info needs them, from a newline bitmap built with SIMD on first use.
7. **Work only the IDE needs is done anyway.** Body source maps, syntax-to-HIR maps, IDE
   symbol indexes and hover data are built on every lowering. v2 builds only what a build
   reads. Source maps are kept as span side tables, read only when a diagnostic needs them.
8. **Everything is `Arc`'d.** Arc-per-node trees, `Arc<[T]>` and `triomphe` everywhere, plus
   interners behind `DashMap` locks. v2 uses arenas, `u32` IDs and lock-free interners
   ([data.md](data.md), [concurrency.md](concurrency.md)).
9. **Cancellation is a panic.** salsa cancels by unwinding (`Cancelled`), so every query must
   be unwind-safe, and cancelling costs an unwind through deep stacks. v2 cancels
   cooperatively: the plane stops scheduling a cancelled request's tasks, and a running task
   sees the flag at its next query call and unwinds to its own task boundary, where it is
   caught once (`base_db::Db::run_all`, `base_db::Shared`). Nothing is caught per query, and
   a query never returns a `Result` for it: a cancel comes once per edit made during a build,
   and a hit pays no branch for it.
10. **Parallelism is bolted on.** salsa handles are per thread, and parallel work forks
    databases. v2's database is `Sync` and all parallel work is plane tasks (decided).
11. **Interned values are never freed.** rust-analyzer's interners grow for the life of the
    process. v2's interners are collected with the rest of the image (GC by reachability from
    live views).
12. **Early cutoff breaks under memory caps.** The LRU caps evict values, and salsa can only
    backdate by comparing values. v2 puts fingerprints on edges, so an evicted value still cuts
    off ([db.md](../lanes/db.md)).
13. **Workspace loading shells out.** `cargo metadata` and `rustc --print` run as processes on
    every load. v2 loads the workspace as queries over its own view of the manifests and lock
    files (lane cargo).
14. **Item trees are rebuilt per file on any edit to the file.** v2 reparses only the items an
    edit touches (lane format's item reparse) and re-lowers only those items.
15. **Every query goes through `&dyn HirDatabase`.** A virtual call per query blocks inlining,
    so even a memo hit can't compile down to a few loads. v2's database is one concrete type,
    passed as `&Db` everywhere, so query calls are static and a hit inlines.
16. **A rowan-style handle API over the flat tree.** `SyntaxNode` handles with an `Arc` to the
    tree, cloned per node, keep the old callers working. Callers move to the flat tree's index
    API (`u32` node IDs, kind and range columns, CSR children), and the handle layer is
    deleted.

Each item has an owner: 1 to 3 and 11 go to db with items, 4 to 6 and 14 go to format with
db's expansion, 7 to format with diagnostics, 8 to everyone, 9 and 10 to db with core, 12 to
db, 13 to cargo, 15 to db with every front-end lane, 16 to format with every caller. Lane `frontend` makes sure each is done, building what no lane has started.
