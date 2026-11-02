---
status: plan
summary: mem-db's plan to move the query database and the front end (blaze-query, salsa, base-db, hir-expand, hir-def, hir, hir-ty, the solver crates, borrowck, wfcheck) onto crates/blaze-mem; the contract every change programs against, and where each crate's changes are filed.
date: 2026-10-29
---
# mem-db: the database and the front end onto blaze-mem

The paths belong to lane `db` and its children, which are not done, so every change here is a
request: `v2/requests/mem-db.md` (blaze-query, the solver cache, the requests to lane mem and
the root manifest) and one file per part, written by mem-db's children:

| part | paths | request file |
|---|---|---|
| `mem-db` | `crates/blaze-query/`, `crates/hir-ty/src/next_solver/interner/tls_cache.rs` | `v2/requests/mem-db.md` |
| `mem-db-salsa` | `vendor/salsa/`, `vendor/salsa-macros/`, `vendor/salsa-macro-rules/` | `v2/requests/mem-db-salsa.md` |
| `mem-db-infer` | `crates/hir-ty/` except `src/next_solver/` | `v2/requests/mem-db-infer.md` |
| `mem-db-solver` | `crates/hir-ty/src/next_solver/` except `interner/tls_cache.rs`, `vendor/rustc_type_ir/`, `vendor/rustc_next_trait_solver/` | `v2/requests/mem-db-solver.md` |
| `mem-db-def` | `crates/base-db/`, `crates/hir-expand/`, `crates/hir/`, `crates/hir-def/` except format's files (`expr_store.rs`, `expr_store/`, `hir.rs`, `hir/`, `item_tree.rs`, `item_tree/`, which are `mem-format`'s) | `v2/requests/mem-db-def.md` |
| `mem-db-checks` | `crates/hir-ty-borrowck/`, `crates/hir-ty-wfcheck/` | `v2/requests/mem-db-checks.md` |

Some parts split again, each with a request file of its own: `mem-db-infer-{ctx,expr,closure,lower,method,eval}.md`,
`mem-db-def-{expand,hir,items,resolve,collect}.md` and `mem-db-checks-{regions,check}.md`, all
under `v2/requests/`. The index by crate, and the budget test that holds each change, is the
last section of `v2/requests/mem-db.md` ("Per crate").

## The contract

Every change in every part programs against these, so the pieces fit.

1. **One frame per query run.** The two executors (blaze-query's `Database::execute`,
   `crates/blaze-query/src/lib.rs`, and salsa's `function/execute.rs`) run each execution of a
   query's function inside `blaze_mem::frame::task(tag, |_| ..)`, the tag the query's. A query
   asked inside a run that misses runs in a task of its own (the frame one level deeper), so
   an inner reset never frees what the outer run holds. A fixpoint iteration (salsa's cycle
   handling) is one task per iteration: what an iteration kept for the next is owned, not
   framed.
2. **Deep code reaches the run's frame through `blaze_mem::frame::current(|frame| ..)`**
   (requested of lane mem in `v2/requests/mem-db.md`, request 1). It hands the innermost
   task's frame to the closure and stops, naming the caller, when the thread is in no task:
   never a second path that allocates globally (no-fallback). A query function's body opens
   one `current` at its top and threads `&'f Frame` into the context structs it builds
   (`InferenceContext`, the lowering contexts, the solver's search graph), so their
   temporaries are `Vec<T, &'f Frame>` (`Vec::new_in(frame)`), frame slices
   (`frame.alloc_slice_copy`, `alloc_slice_fill_with`, `alloc_slice_from_iter`) and, for maps
   and sets, `hashbrown::HashMap<K, V, FxBuildHasher, &'f Frame>`
   (`HashMap::with_hasher_in(FxBuildHasher, frame)`; root request 3 turns on hashbrown's
   `nightly` feature, which makes its `A` parameter `core::alloc::Allocator`). A crate that
   names `Vec<T, A>` adds `#![feature(allocator_api)]`.
   The one exception is a context made and dropped many times within one run, such as the trait
   solver's `InferCtxt` per canonical goal (thousands per `infer`) and its search graph's
   storage. Code holding `&Frame` can't roll back, so those would pile up in the frame until the
   run ends. They take storage the worker keeps instead, emptied with its capacity kept and
   given back when the context ends, as upstream's `obtain_canonicalizer_state` does
   (`v2/requests/mem-db-solver.md`, "The one design decision").
   Nested runs each have a frame of their own. Sharing one frame through markers would be
   unsound (`v2/requests/mem-db.md` request 10, which asks lane mem to make deep nesting cheap).
3. **The tag is the query's subsystem** (`blaze_mem::Tag`): base-db's file and parse inputs
   `Parse`; hir-expand `Expand`; hir-def's name resolution, scopes, signatures and body
   lowering `Nameres`, its item tree `Parse`; hir-ty's inference, lowering, method resolution,
   layout, const eval and trait solving `Infer`, its MIR queries `Mir`; borrowck `Mir`;
   wfcheck `Infer`; a query with none of these (blaze-query's own, salsa's tests) `Query`.
   blaze-query gives `Query` a `const TAG: Tag = Tag::Query` that a query overrides; salsa
   gives `function::Configuration` a `const MEM_TAG: Tag = Tag::Query`, set by the tracked
   attribute `mem_tag = <a Tag variant, or a path to a Tag constant>`
   (`v2/requests/mem-db-salsa.md` M1 and M2).
4. **A memoized result is not a frame value.** It outlives the run, so it is owned: lane mem's
   ruling in `v2/requests/mem.md` ("frontend-tokens", answer 2) is that a memo's value comes
   from the general-purpose allocator until memos persist into the image, since each memo has
   its own end (recomputed to a new value, evicted) and no lifetime group an arena could free.
   Building it: temporaries in the frame, the result made once at the end at its exact size
   (`Box<[T]>`/`ThinVec` from an `ExactSizeIterator` over the frame vector, never a `Vec` grown
   by doubling and shrunk). The memo's slot is already on a `blaze_mem::Region`
   (`vendor/salsa/src/table/memo.rs`); blaze-query's memo values sit in its segments.
5. **Interned and table storage is a reserved region**, `Column`/`AppendVec`/`RegionVec`
   tagged `Tag::Query` (salsa's ingredient and page tables, blaze-query's `Rows` and maps) or
   `Tag::Intern` (interned values).
6. **Per-call scratch on hit paths** (memo verification, dependency walks, revalidation,
   cycle checks) is a buffer the worker keeps: `blaze_mem::scratch::{with_u32s, with_u64s,
   with_bytes}`, or a `scratch::Scratch<T>` in thread-local worker state for a typed one.
   These paths make no allocation at all, so their budget tests assert 0.
7. **Budget tests** use `blaze-alloc-count` (`assert_at_most`), in a crate whose tests install
   `#[global_allocator] static ALLOC: blaze_alloc_count::Counting`: zero for a memo hit, a
   revalidation that changes nothing and the warm steady state of a re-check (the second
   identical re-check after an edit); a miss is held to its owned result's allocations alone.

## Speed

A frame allocation is an align, an add and a compare (about 2 ns, `crates/blaze-mem/src/frame.rs`),
against mimalloc's 15 to 25 ns for a small allocation and its free; a run's temporaries are
freed in one store. `frame::task` is two thread-local accesses a run, against the run itself
(hundreds of nanoseconds for the smallest query). So the bound for a query run's temporaries is
their bytes written once to warm memory, and the executor's wrapper costs under 1% of the
cheapest run.

The solver cache's replace (`v2/requests/mem-db.md` request 4) is bounded by the generation it
writes: about 2.5 KB, some 40 cache lines, roughly 100 ns to warm L2. Taking the slot is an
array pop from the table's cache (one compare-and-swap per 64), and retiring the old one is a
push of 8 bytes into the limbo's reserved column. Together that is under 10 ns, against today's
`Box::new` and the second box `blaze_reclaim::Limbo` makes (two allocations of 20 to 40 ns each,
and a `Vec` that grows). So the replace stays within 10% of writing the generation.

A revalidation in a reused view (request 6) is bounded by the loads `verify` already does: one
result line per edge. Opening the view becomes a pop from the spare list plus the `reset` done
earlier, off the path, in place of three `Map`s that cost two `mmap`s each and a shard array.
