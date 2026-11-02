---
status: plan
summary: mem-db-def-hir's plan to take crates/hir's IDE hit paths (Semantics, descend_into_macros, resolve_path, to_def, the accessors, display) off the general allocator with per-thread shelves and caches carried across requests, SymbolCollector onto its query's frame, the budget tests and the speed-of-light bound.
date: 2026-10-29
---
# mem-db-def-hir: `crates/hir` onto blaze-mem

Part of `mem-db-def` under `mem-db` (`docs/v2/plan/lanes/mem/db.md`, whose contract this follows).
Path: `crates/hir/`, held by db-next2. Every change is a request:
`v2/requests/mem-db-def-hir.md`, Parts R1 to R12. This note gives the design and the reasons.

## What the slice is

`hir` has no query of its own: no `#[salsa::tracked]` function. Its code runs in two places.

- **IDE requests.** A request makes a `Semantics`, asks it to descend tokens into macros,
  resolve paths, map syntax to definitions and read types, and drops it. None of this is in a
  query run, so there is no frame: per-call buffers are per-thread shelves, and the request's
  caches must not start empty each time.
- **ide-db's symbol queries.** `SymbolCollector` runs inside `module_symbols`,
  `library_symbols` and `extern_prelude_symbols`. Its temporaries take the frame those queries
  open, tagged `Tag::Nameres`; its result (`Box<[FileSymbol]>`) is the memo, made once at size.

## The design

1. **Shelves of this crate's own (R1).** `blaze_mem::scratch::Shelved` is implemented for a
   newtype per buffer kind: the descend walk's groups, tokens and derive calls, display's fields,
   variants and clauses, `lint_attrs`' output. `scratch::with` lends one empty buffer per call
   and takes it back emptied. A callback that calls back into `Semantics` gets another buffer, so
   nothing is aliased and no `RefCell` is borrowed twice. Buffers whose elements carry `'db`
   (`Clause<'db>`, `ExpansionInfo<'db>`) are kept as their `'static` type and moved through
   `recycle`, which is std's in-place `collect`: safe code, the same allocation, and checked by a
   test.
2. **Caches carried from one request to the next (R2).** A `Semantics` rebuilt every `DynMap` it
   read (`child_by_source`, the main cost of `to_def`) and grew six maps from empty. Now
   `SemanticsImpl::new` takes what the thread's last `Semantics` left, and `Drop` leaves it
   back. The tables that are a function of the database alone stay whole while the nonce and
   revision are the same: dyn maps, a file's modules, include files and macro call ids. The
   others keep only their storage: the root-to-file table (so the "nodes of this `Semantics`
   only" check is unchanged) and the expansion table (`ExpansionInfo<'db>`, held in a vector
   indexed by a `'static` map so the storage carries). The database can't change revision while
   a `Semantics` borrows it, so the revision read at `Drop` is the one the tables were built at.
3. **A walk with no heap of its own (R3, R4).** `descend_into_macros_impl`'s stack of
   `(file, SmallVec)` becomes two flat shelves: groups `(file, count)` and the tokens. The order
   is unchanged: a group's tokens are walked first to last, and groups pushed meanwhile are
   walked after it, last pushed first. The derive branch's per-derive `to_owned` goes to a shelf
   of call ids. `kmerge_by` of the at most two iterators `token_at_offset` yields becomes a
   two-way merge with kmerge's exact tie rule: the iterator that gave the last item stays first.
   A test checks it against `kmerge_by`.
4. **Iterator forms of the accessors (R6).** `declarations`, `impl_defs`, `legacy_macros`,
   `fields`, `variants`, `items` and `modules` each collected into a `Vec` that callers only
   iterate. Every table they read lives for `'db`, so `*_iter` forms borrow the database and
   allocate nothing. The `Vec` forms stay, defined through them. An iterator is used instead of
   the `for_each_*` closure the brief allowed, because it keeps `break`, `?` and `return` in
   the 126 callers. `Module::children` stops collecting with no change to its signature.
5. **No re-lowered store per path (R10).** `resolve_path` lowered the path again into a whole
   `ExpressionStore` only so a type anchor could be lowered. It now finishes the store only when
   a type anchor or a use tree's qualifier reads it, on top of mem-format-hir's pooled builder
   scratch. The `Resolver`'s scope vector, `traits_in_scope`'s set and `names_in_scope`'s map
   are mem-db-def-resolve's (asked in R10).

6. **A completion search in search order (R10b).** `query_external_importables` dedups with
   a per-worker set and hands items out of a lent vector in the order the import map's search
   finds them. Draining a reused hash set would give an order that depends on the set's past
   capacity: buffers reused across requests must never decide an output order.

## What stays and why

- `signature_anon_consts_cache` is per request. Its values are `AnonConstId<'db>` maps, made
  only for expressions in signatures.
- Functions that return a collection (`rename_conflicts`, `get_unsafe_ops`, `derive_helper`,
  `record_*_fields`, the `descend_into_macros*` `SmallVec<[_; 1]>` results) return the caller's
  result. One descendant is inline.
- `term_search`'s field vectors that are zipped against parameters keep the `Vec` form.

## Budget tests

`crates/hir/src/tests/budget.rs`, written in full in R12. Each body runs twice, with a fresh
`Semantics` each time as an IDE makes them. Then a third `Semantics` is made, used and dropped
under a budget of 0: `descend_into_macros` (also into a `macro_rules!` call), `resolve_path` of
an expression path and a type path, `to_def` of a struct, a field and a module, and every
accessor's iterator form. Two further tests check that `kmerge2` matches `kmerge_by` (ties
included) and that `recycle` keeps the allocation. The tests find what they ask about through
`syntax::view`, once and outside the measured calls, and keep it as ranges and pointers. They
make a handle only where `Semantics`' API still takes one, and stop doing so once frontend-tree
moves `Semantics` to views.

## Speed of light

The work of a warm request is the walk itself: hash lookups and pointer steps over the flat
syntax tree (`crates/syntax-tree`, which allocates no cursor). For one `descend_into_macros`:
one span-map lookup, about \( d \) macro-call lookups for \( d \) levels of nesting, and one
`map_range_down` per expansion. Each is one or two cache-missing loads, about 5 to 20 ns. So
the bound is about \( 20 + 40d \) ns, with no allocation.

What this removes is all overhead on top of that walk:

- **Allocation.** Before, there was one `Vec` growth for the stack, \( \log_2 n \) growths for
  each of six cache maps per request, and one heap per `kmerge_by`. Each costs 15 to 25 ns
  with mimalloc, plus its free.
- **Rebuilding every `DynMap` a request reads.** `child_by_source` walks a container's items:
  microseconds for a module of a few hundred items, per request, per container.

After the change, a warm request makes 0 allocations and rebuilds no `DynMap`. Its cost is
the hash lookups (FxHash, one multiply per word) and the loads they miss on, which is the bound.
Taking the carry is two thread-local accesses and six map moves (about 50 bytes each), about
10 ns. On a miss (a new revision, or a thread's first `Semantics`), the cost is the tables
rebuilt once, as before.

`SymbolCollector`: the bound is writing each symbol once (about 64 bytes) and hashing it once.
Before, the `FxIndexSet` grew by doubling (\( 2n \) element moves and \( \log_2 n \)
allocations), and a new cache map was made per module. On the frame, growth is a bump, often
extended in place. Dedup is one `HashTable<u32>` probe per symbol, without a clone. The result
is one exact allocation, at memory bandwidth: about 64 bytes per symbol, so microseconds for a
module of a thousand symbols.

The fastest known way to do this is to keep the per-container source maps for as long as the
revision holds (IntelliJ keeps its PSI caches until a modification count changes), and make
the per-request walk read-only. The carry does that per thread, with no lock and no shared
write.
