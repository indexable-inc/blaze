---
status: plan
summary: lane mem's child mem-format: moving the front end's interners (intern, blaze-ids, span), syntax and item-tree columns, the lexer's columns and the per-call scratch of parse, lowering, token conversion and mbe onto crates/blaze-mem, with the allocation-budget tests that hold each to zero.
date: 2026-10-29
---
# mem-format: the front end's format crates onto blaze-mem

Parent: lane mem (docs/v2/plan/design/memory.md). Children: `mem-format-tokens` (parser, tt,
mbe, syntax-bridge), `mem-format-tree` (syntax, syntax-tree, hir-def's item tree),
`mem-format-hir` (hir-def's expression store and HIR). Each writes its own section below.

## interners and span (mem-format)

None of these crates was free, so each change is written out, code and budget tests, in
`v2/requests/mem-format.md` for its owner. Speed of light: `speed-of-light-format.md`.

- **`blaze-mem` (to mem, section 1).** `Column::claim(count) -> Block` and `Column::fill(&mut
  Block, value)`: a writer claims a run of indices with one `fetch_add` and fills them in
  order. That lets an interner that assigns IDs in per-shard blocks keep its exact assignment
  on a `Column`.
- **`intern` (to dedup-intern, section 2).** The store is type-erased, so its values go on one
  shared `Region` tagged `Tag::Intern` (`space.rs`, 64 GiB reserved), not a `Column<T>` per type.
  Tables, grown table headers and bump chunks are claimed from it; the model build
  (`blaze_check`) keeps boxes, as only std atomics can be cast from region memory. The `Symbol`
  ID column becomes one `Region` of `AtomicPtr` slots. It is not a `Column`, because a freed ID
  is reused at a chosen index. Tests: a hit and read and 100k misses allocate nothing; each slot
  is published once.
- **`blaze-ids` (to dedup-intern and the codec children, section 3).** `Sharded`'s values move
  onto a `Column<(ContentId, T)>` with per-shard `Block`s of 64. The probe tables stay boxed (see
  the speed-of-light doc). `ContentId::of_many_into` keeps its buffers in `scratch`. Tests: hits
  and reads allocate nothing; misses allocate only when a table doubles; a reserved interner
  stops at its size.
- **`span` (to db-expand-macros-next1, section 4).** `AstIdMap`'s build, body walk and relative
  maps put their buffers on the task's `Frame` and their radix sorts on kept `Scratch`. The
  index counters are one hash map per thread, cleared per build. Results are copied out once
  at exact length. Tests: a warm build allocates only its 3 (or 8 with fn bodies) result
  blocks; a warm body walk allocates only its ids. `map.rs` and `lines.rs` already build exact
  results, and their remaining sorts are lazy or of macro fragments only.
- **`const-ast`** allocates nothing outside its tests: nothing to move.
- **`hir-def`, applied here** (the paths were free once speed-hir-store was DONE):
  - `ExpressionStore::has_closures`, a request from speed-hir.
  - mem-db-def-hir's R10.1: a builder dropped without `finish` gives its scratch back (`Drop`).
    The collector's stacks are one per-thread pooled `Stacks`, so a warm `ExprCollector::new`
    plus `lower_path` of a plain path allocates nothing
    (`a_warm_plain_path_lookup_allocates_nothing`).
  - db-expand-sigs' signature lowering: `BodyLowering` (`Body`, `SourceMap`, `Whole`) is set
    on every signature entry point, and `ExpressionStore::block_scopes` gives a second
    lowering the first one's blocks.
  - Body lowering's syntax side (`lower*`, `expander.rs`, the store's tests) was handed to
    frontend-tree-move-def-lower on 10-29 for the move onto `syntax::view`.
  - The `hir.rs` and `expr_store/path.rs` size asserts fixed for a 4-byte `Name`
    (`intern::Symbol` is a `NonZeroU32`): `Label` 4, `RecordLitField` and `RecordFieldPat` 8,
    `Binding` 12, `Name` 4.
- **`intern`'s `Bump` stays.** It is not an `ArenaLocal`: under `blaze_check`, `blaze_mem::Arena`
  reaches the chunk cache, whose links are model atomics in process-wide statics. Answered in
  `v2/requests/mem.md`.

Memo values (`AstIdMap`, `SpanMap`, `ItemTree`, expression stores) stay global-allocator blocks
per lane mem's answer, until memos live in the image.

## tokens (mem-format-tokens)

All four crates are frontend-tokens', so each change is written out, code and budget tests, in
`v2/requests/mem-format-tokens.md`. A memoized `tt::TopSubtree` stays global-allocator blocks
(lane mem's answer); only per-call scratch moves.

- **Lexer (`parser/src/lexed_str.rs`).** The token and checkpoint columns are `lend_u32s`,
  declared checkpoints-first so they drop back in the order they were lent and each keeps its
  role. Test: a warm lex allocates nothing (was 2). A lex dropped on another thread warms that
  thread. `syntax::reparsing::lexes_alone` holds four `LexedStr`s (eight columns against a
  shelf of 4), so its last four allocate, as today.
- **Parse.** The event list, forward parents and output words were already on
  `per_thread_buffer!` shelves (requests-parser, frontend-tokens), with the warm-parse budgets
  at 0. Nothing to add.
- **Parser input (`parser/src/input.rs`).** The words go on their own `per_thread_buffer!`
  kind, so a warm `to_input` is free and the hand-rolled `Cell<Option<Input>>` caches in
  syntax-bridge and mbe can go. `into_words` copies out exactly: same count as today for
  `tree_words`, and the shelf stays warm.
- **mbe.** The matcher arena, both matches' bindings and the transcriber's nesting stack are
  `Vec<_, &Frame>` in one `frame::task(Tag::Expand)` per `expand_rules`. Nothing outlives the
  call, and no new lifetime is needed (the frame's and the input's shorten to one `'t`).
  `${concat}` text is `lend_string`. Test: a warm expansion of `$($e:tt)*` allocates the same
  for 10 trees as for 1000.
- **tt.** The walk stack is `Scratch<(u32, u32)>` (same count). The builder's five-column
  `SCRATCH` stays hand-rolled: it is a struct under one size cap, not a `Vec<T>`.
- **syntax-bridge.** `TtTreeSink`'s text is `lend_string` and its fragment starts are
  `lend_u32s`. `FlatTokens`' stack is a `SmallVec<[_; 4]>`.
- **Cost.** A lend and its return are two thread-local accesses; a task is two more. No hot
  loop takes one per token: one per lex, input, parse, expansion or conversion.

## tree (mem-format-tree)

Syntax side (`crates/syntax`, `crates/syntax-tree`). Not free when written, so every change is
in `v2/requests/mem-format-tree.md`; the item tree is `## tree items (mem-format-tree-items)`.

- **An unchanged file's parse.** `SourceFile::parse_file` of the same text is a kept parse
  handed out again: two `Parse` clones and two drops, all counts of a tree the kept slot holds.
  The allocation was in the drops: `Drop for Parse` sent every count to the drop thread over
  `std::sync::mpsc`, whose list channel allocates a block every 31 sends. Only a tree's last
  count frees anything, so only it is sent (`Arc::into_inner`, atomic); the others are a
  decrement in place. `reparse_to` compares the texts' addresses before their bytes. Budget:
  0, a hundred times over (`an_unchanged_file_parses_again_allocating_nothing`).
- **Lifetimes.** A `FlatTree` is a memo value (in the parse query's `Parse`): by lane mem's
  answer, one block, from the global allocator. Every column of one build, copy or edit is a
  temporary of that call: a per-thread set of buffers (`Buffers`, a `Cell` in a thread-local of
  `syntax-tree`'s, the builder's own as the item tree's `TreeBuilder` is), kept at the largest
  tree the thread built. Nothing in a tree is a process-long table, so no `RegionVec`/`Column`.
- **One block.** `flat/block.rs`: `Block` (one raw allocation, 8-aligned, columns placed past
  each other) and `Col<T>` (derefs to `[T]`), so the tree's ~150 column reads are unchanged and
  only its five constructors move to `FlatTree::pack`. Soundness: a column is a field of the
  tree whose block it is in; `FlatTree` and `Kept` implement `Drop`, so none can be moved out.
  The 8-aligned columns go first: the block has no padding and is exactly the bytes
  `heap_size` counted before. 12 boxes and `exact`'s second copy of each become one block and
  one copy.
- **Budgets.** Warm build 1 (the block), copy 2 (its text, its block), splice 1, token
  replacement 1 (`a_warm_build_copy_or_edit_allocates_its_block_alone`); an outline adds its
  `Lazy`, the parse its `Arc`. The lexer and parser's buffers are `crates/parser`'s
  (mem-format-tokens).
- **Speed of light.** The block's cost is one copy of the columns, about 6.5 bytes an element
  at memory bandwidth (about 0.3 ns an element, under 1% of a parse at about 40 ns an element),
  and it replaces `exact`'s copy, so the work is no more than before; what goes is about 20
  allocator calls a parse and 12 to 15 an edit. Prior art: rust-analyzer's rowan allocates a
  green node per node; Roslyn pools its builders' buffers per thread, as this does.
- **An edit's reparse.** Three `format!` strings become `blaze_mem::scratch::with_string`.
  The ancestors `Vec` and `vec![bool]` become two walks: an ancestor is parsed in full when it
  is below the outermost `fn` body. `first_tokens` is an iterator. A reparsed body, block or
  item is built over its range of the new text (`sourced_range`) instead of over a copy of
  that text. Budget: a warm block reparse allocates at most 3 (the node's block, the spliced
  block, the `Arc`) and a token edit at most 2 (`a_warm_reparse_allocates_its_trees_alone`).
  This holds once the lexer and parser input are on their shelves (mem-format-tokens).
- **Applied 2026-10-29** under frontend-tree's grant: `syntax-tree`'s `Cargo.toml`, `lib.rs`,
  `flat.rs` and `flat/block.rs`, and `syntax`'s `Cargo.toml`, `parsing.rs` and
  `parsing/reparsing.rs`. Not yet built or tested (this lane's brief forbids it).
- **The bits dedup is folded in** (`v2/requests/format.md`, "dedup: bits: syntax-tree flat.rs
  ..."). `rank_in`, `BitSlice`/`BitSliceMut` and `or_range_at` from `blaze_ids::bits` replace the
  local `rank_in` and `append_bits`. `Ranked`/`ranks_of` are not used, because their boxes would
  be allocations beyond the block. The ranks are filled into the block in place.

## hir (mem-format-hir)

hir-def's expression store and HIR (`expr_store.rs`, `expr_store/`, `hir.rs`, `hir/`). The
changes were written as exact requests in `v2/requests/mem-format-hir.md` while
speed-hir-store held the paths, and applied once it was done (see the end of this section).
This section is the design those requests implement.

### Lifetimes, and the allocator for each

| what | lives | allocator |
|---|---|---|
| a builder's columns, list stacks, forward and back pushes, arenas, hash maps and side vectors | one lowering | the per-thread pool of builder scratch (`expr_store.rs` `Scratch`, at most 4 sets, a set over 1 Mi elements freed): warm after a thread's first body |
| `finish`'s sort buffers and gathered side columns | one `finish` | the same pool (`FreezeScratch::sort`, `Packing`) |
| a finished store's `Copy` columns | the memo value | one slab for the HIR columns, now with the match arms, record fields, field patterns and names; one for the type columns; one for the five side columns (`hir/col.rs` `Col`) |
| the store's arenas (bindings, labels, lifetimes), its macro calls and its cold tables (paths, literals, asm, offset_of, generic args) | the memo value | one exact allocation each, none when empty |
| encode's gathered columns | one encode | the per-thread `EncodeScratch` (`expr_store/persist.rs`) |
| encode's words and names | one encode | per-thread spares of `Words` (`hir/persist.rs`, held: see below) |

A store, a body and a source map are memo values. Lane mem's ruling (`v2/requests/mem.md`) is
that a memo value has no revision-arena lifetime: salsa drops each one on its own when it is
recomputed. So they are owned, made once at their exact size, and their `Copy` columns are
packed into one block. Nothing here uses a revision arena or a task frame:
- A frame gains nothing over the pool for about 30 interleaved growing columns. A bump
  allocator wastes each column's old space when it grows, where the pool keeps each at its
  high-water mark.
- The pool is a type of hir-def's own in its own thread-local, which is lane mem's ruling
  for crate-owned scratch.
- So hir-def needs no `blaze-mem` dependency for this.

### Budgets, each held by a test

- Warm lowering: 0 (`a_warm_lowering_allocates_nothing`). Before this change, each of the 12
  per-store tables grew from empty for every body.
- `finish`: no buffer that grows with the body (`finishing_allocates_no_scratch`). Std's stable
  sort takes a heap buffer once a table outgrows 4 KB, about 150 entries. `finish` now uses two
  stable radix passes into a pooled buffer, low key then file. That gives the same permutation.
- A finished store without a source map is exactly its blocks
  (`a_warm_store_is_its_blocks_and_nothing_else`). For the fixture that is 8: the expression
  box, the column slab, the side slab, three arenas, the type slab and the type hygiene.
- Reading a store, a memo hit and a scope lookup: 0 (`expr_store/tests/alloc.rs`,
  speed-hir-store's).
- Warm encode: the same bytes as a cold one, no kept buffer grown, and fewer allocations than
  cold (`a_warm_encode_reuses_its_scratch`). What is left is `Words`, which starts empty on
  each encode until its per-thread spares land. Then a warm encode is 0 into an output with
  room, and names are 0 on warm `Words` (`names_are_one_index_per_text_on_warm_words`, in the
  held request).

### Speed of light

Lowering a body is a walk of its syntax tree that writes 5 to 12 bytes per node into warm,
pooled columns, so its bound is that walk. `finish` adds one copy of every column (memory
bandwidth: about 1 µs for a 100 KB store at 100 GB/s). A small allocation costs about 20 ns,
so the 8 to 12 allocations of a store are under 0.3 µs, against a lowering of tens of
microseconds for any body that has them, which is under 1%. The forward-map sorts are linear:
the radix survey skips the digits all keys share, the whole file key within one body, so for
range keys they are about 4 to 6 passes over 28-byte pairs.

What is left, and why:
- The five cold tables of `HirColumns` and the store's arenas are one allocation each.
  Packing them into one block needs offsets in place of pointers, and `Binding`/`Path` as
  plain records.
- Decode still copies once per column. Making it borrow the mapped bytes is speed-hir-store's
  queued "borrowed decode columns" (it needs frontend-deps-sigs' `Arc` owner of the image).
- The source maps' encode is next, once srcmap's schema 4 is settled.

### applied (mem-format-hir-apply)

Five of the six requests are in the tree (2026-10-29):
- the builder's tables on the pooled scratch, the side columns in one slab, and exact arenas
  (`expr_store.rs`);
- `finish`'s stable sorts in `FreezeScratch::sort` (`expr_store/source_map.rs`);
- the arms, record fields, field patterns and names in the HIR slab (`hir.rs`,
  `hir/columns.rs`);
- the three lowering budget tests;
- the store's and body's encode on `EncodeScratch` (`expr_store/persist.rs`).

`Words` on per-thread spares (`hir/persist.rs`) is held. frontend-deps-sigs, which owned that
file, is done, and the request waits on lane db's ok. hir-def has the `blaze-alloc-count`
dev-dependency and the test `#[global_allocator]` now. The adaptations are noted under each
section of `v2/requests/mem-format-hir.md`.

### forwarded sections (mem-format-fwd)

Lane `requests` forwarded 24 sections to mem-format, plus two later ones. Each now has a
`> mem-format-fwd` note under it. Applied (2026-10-29):
- **The store keeps its macro calls** (`v2/requests/codegen.md`, for hir-ty-mir's
  `failed_expansion`, `location.rs` and `type_name.rs`).
  - `ExpressionStore::macro_calls()` gives each call in lowering order, recorded whether or
    not the source map is.
  - It is a `ThinVec` on `ExpressionStore`, not in `expr_only`: 8 bytes, and no allocation
    without calls. A signature with only type macros has calls and no expressions.
  - It is persisted as `StoreRoot::macro_calls` (salsa id bits, like `block_scopes`; store
    and body schemas 6), because dependency signature stores are decoded and their anon
    consts reach MIR.
  - Moving the column through a dependency image's relocation table is
    `v2/requests/mem-format-fwd.md` F1 (`deps/signatures/table.rs`).
  - Test: `tests/body.rs` `round_trip`, for every fixture, checks the store's calls against
    the source map's.
- **`BindingPats::from_pushed` is `blaze_ids::radix::csr::group_into`** (`speed-hir-store.md`).
  Its cursors are kept in the pooled `FreezeScratch`, so it makes the same two exact
  allocations as before.
- **`hir/columns.rs` reads `ArithOp::ALL`** (`format.md`, dedup). This is the same order, so
  the codes are unchanged.
- **`underscore_import`'s dump says `(import)`** (`db-expand.md`).

Held: `hir/persist.rs` (`Read::seq` at its length, `Read::boxed`, `ModPath` segments in a
`SmallVec`, `Words` keys), together with `mem-format-hir.md` section 5. Open by design: a path's
resolved local kept at lowering (`speed-hir.md`). Every other section was already in the tree,
overtaken, or not mem-format's.

## tree items (mem-format-tree-items)

Filed as `v2/requests/mem-format-tree-items.md` (paths held by `frontend-deps-items`, which is
applying F of speed-hir-sol-items, and `frontend-tree`); needs `mem-db-def.md` H1 to H3 for
hir-def first.

- An `ItemTree` is one pointer to one block: a header of `u32` column ends, then the 31
  columns at their aligned offsets (`item_tree/block.rs`, written by a `columns!` macro). The
  header costs 4 bytes a column against the 16 of a `Box<[T]>`, so the tree shrinks from about
  500 bytes of fat pointers to 8. A tree with no values points at a static empty header and
  allocates nothing.
- The columns are private; same-named accessors keep every reader as it was (`attrs` is
  `attr_list()`, since `attrs(id)` exists). A tree is made only through `build`/`try_build`
  from its lengths, each column written through a sink branded to its build, and checked full
  before it is a tree, so a half-written block is never seen; an error or panic drops what was
  written.
- Lowering, assembly and decode allocate once a tree, not once a non-empty column. Decode is
  1 + one a `ModPath` with segments until `hir::persist`'s `Code for ModPath` stops collecting
  into a `Vec` (not this lane's path).
- Rejected: interning POD columns by content (a tree's columns would outlive it in the
  interner, which never frees: a leak an edit), and keeping one box a column (31 allocations
  a tree, which is what this replaces).
- The per-item design of the 20:45 tree (`item_sources` keeping every item's syntax, ids and
  anchors in one block, spans read in place from the anchors, a cached root cursor so a warm
  item lowering allocates only its block) is on record in the request but withdrawn: F replaces
  per-item queries with the file's last tree kept beside salsa. Of it, only `assemble` writing
  one block carries over.

## expand (mem-format-expand)

`mem-db-def-expand`'s F1 to F9 (format's types for hir-expand), settled on 2026-10-29 in
`v2/requests/mem-format-expand.md` and marked under each F. None was applied here: `crates/span`
is held whole by `frontend-tree-span`, and `tt`, `mbe` and `syntax-bridge` by `frontend-tokens`.
`frontend-tree-span` has applied the span half (2b, 3a, 4, in `map.rs`); the `tt`, `mbe` and
`syntax-bridge` half (1, 2a, 3b, 5 and the F7 addendum) is with `frontend-tokens`.

- **A tree in any allocator, one view type.** tt keeps a tree as one `u32` block, and the views
  (`TokenTreesView`, 16 bytes) borrow the tree. `TopSubtree<A = Global>` owns its block in `A`
  and derefs to a non-generic `Tree` (block pointer, layout, top spans, kept words), which every
  view and reader borrows. One small `unsafe` module (`storage/owned.rs`) states the
  invariant: the block is the owning `TopSubtree`'s, freed only in its `Drop`, written only
  through `&mut` it. Rejected: a generic `TokenTreesView<'a, A>`, which would make every
  iterator, matcher and parser input generic for one allocation per tree.
- **A task's tree keeps no parser words.** Each parse of a tree made and kept its `TreeWords`
  (four boxes), a cache for trees read many times, such as a macro's argument by its rules. A
  framed tree is parsed once, so it converts its view into the worker's `parser::Input` (the
  same tokens, by `view_tokens`' contract), and a framed expansion allocates nothing global
  from build to parse.
- **Output in the allocator given, temporaries in a task of their own.** mbe's `expand_rules_in`
  takes the output's allocator apart from the match's frame. The global wrapper passes `Global`
  and makes no copy, where F7 as filed copied a framed tree out. `expand_in` puts the output in
  the caller's frame and the match in a nested task, so the buffers' doubling never piles up in
  a query's frame.
- **The expansion's span map: the worker's map, copied exact.** The sink fills a per-thread
  `SpanMap` (cleared, capacity kept) and copies it out at its length, in place of F3's
  `from_columns_in`. That merges three requests on the same sink lines: F3, `frontend-tree-span`'s
  fragment spans, and the applied lent-buffer section. The fragment sorts use
  `sort_copy_by_key_in` with worker scratch, the same stable sort, so the order is unchanged.
- F9 was already true after tt's one-block rewrite; F8 asked nothing.
