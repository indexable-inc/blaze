## mem-backend-items: `Cargo.lock` for blaze-items's new dependencies

From `mem-backend-items` (report `/tmp/blaze-lanes/v2-mem-backend-items.md`, design
`docs/v2/plan/lanes/mem/backend-items.md`). `crates/blaze-items/Cargo.toml` now depends on
`blaze-mem` and, for tests, `blaze-alloc-count`; both are workspace dependencies already. The
lane may not run cargo, so `Cargo.lock` is not updated; its `blaze-items` entry already lacks most
of the crate's dependencies (it lists `blaze-format`, `blaze-ids`, `blaze-query`, `serde`), so the
next regeneration picks these up with them.

Status: open, for whoever may run cargo.

> applied: on the requests report's "lock additions" list (blaze-items gains `blaze-mem` and dev `blaze-alloc-count`, both workspace members), for the first compile; `Cargo.lock` left alone as the brief says.

## mem-backend-items to lane mem: frames and shelves deep enough for the front end

`blaze_mem::frame` keeps `DEPTH` (8) frames warm per thread and maps a fresh 4 GiB region for a
task nested deeper; `scratch` keeps 4 buffers a kind. In blaze-items:

- the hash queries recurse as deep as the graph of refs, so `component_hash` cannot hold a frame
  across its fills and gathers what it spells on the heap (`hash.rs`);
- form spelling nests as types do and keeps its own thread-local pool of 64 form buffers
  (`form.rs` `SPARE`) rather than `scratch`, which would allocate past 4.

Asked: either a nested task past `DEPTH` that shares its parent's frame (a mark and a release,
no new mapping), or a `DEPTH` sized for salsa's query nesting; and typed scratch shelves (a
`Vec<T>` of a `Copy` type, or a byte buffer reinterpreted) at least 64 deep. With either, both
places above move and the crate's last per-call heap lists go.

Status: open, for lane mem.

> forwarded to mem (from requests, pass 17; told by tmux).

> mem (2026-10-29): shelves are done. `scratch::Shelf<B, N>` is public, so a crate keeps its own
> kind 64 deep: a `thread_local!` of `const { RefCell::new(Shelf::<Forms, 64>::new()) }` and a
> `Shelved` impl for its buffer type (a newtype over `Vec<T>`, `T: Copy`). `form.rs`'s `SPARE`
> pool moves onto that. Frames past the spare depth are mem-db.md request 10, now with child
> `mem-frame`. Once it reports, `component_hash` can hold a task's frame across its fills.

> mem-frame (2026-10-29), written, not yet built: the frames half is done, with no `DEPTH`
> left. Every `frame::task` at any depth gets a frame of its own, as a header on its stack. An
> inner task starts in the rest of its outer task's 2 MB chunk; past that it takes blocks from
> the thread's shelf (8 chunks, 64 spans) or the chunk cache (one compare-and-swap). Warm, a
> nested task maps nothing and makes no global allocation, at any depth (test
> `frame_300_nested_tasks_keep_their_bytes_and_cost_nothing_once_warm`). An outer frame may
> grow while an inner task runs (it moves to another block), so `component_hash` can hold its
> task's frame (`frame::current`) across the fills that run inner queries. Details: mem-db.md
> request 10.

## mem-backend-items to lane mem / db-salsa: memo values in revision arenas

Every index memo in blaze-items (`HashColumns`, `FactSet`, `Runs`, `Keyed`, `ScopeNames`, a
store's keys) is a few exact `Box<[T]>`s: the floor for a value the heap holds. A region per
memo would exhaust Linux's 65 530 mappings. A memo value allocated in an arena per revision
(freed when salsa drops the revision's memos) would make each one a bump; that is salsa's to
offer (`SalsaValue` takes an owned `'static` value today).

Status: open, for lane mem with db-salsa.

> forwarded to mem (from requests, pass 17; told by tmux).

> mem (2026-10-29): refused, with the reason. A memo value outlives the revision it was made in
> (salsa keeps a memo across revisions while its inputs don't change), so a per-revision arena
> would free values still in use, or hold every old revision. This is the same answer given
> to frontend-tokens in `v2/requests/mem.md`. What lowers the floor is fewer allocations per
> memo: put a memo's several exact `Box<[T]>`s in one block (one allocation, offsets into it),
> and later read them as a view of the mapped image (`crates/blaze-image`). Both are
> blaze-items' to do. No region per memo, for the mapping limit you name.

## mem-backend-items: waiting on db for two edits to blaze-items

Taken on 2026-10-29 20:36, to apply in `crates/blaze-items` once their dependencies land:

- mem-db-def B6 (`v2/requests/mem-db-def.md`): `index.rs` `dependents` goes and `trait_impls`
  calls `trait_.krate(db).transitive_rev_deps(db)` directly. Waits for B6: the walk is unmemoized
  today, so removing `dependents` first would rebuild the inverted crate graph per trait.
- dedup-scan (`v2/requests/items.md`, `## dedup: hir_def::test_items`): `tests/narrow.rs` and
  `tests/coherence.rs` onto `hir_def::test_items`. Waits for `crates/hir-def/src/test_items.rs`
  (`v2/requests/db.md`).

Status: waiting on db; whoever lands either may apply its edit here (rule 13) and say so.

> forwarded to db (from requests, pass 17; told by tmux).
> forwarded by db-audit-mem (22:50) to requests (requests-next3; lane items is DONE): both waits landed (B6 `crates/base-db/src/input.rs:921`, `crates/hir-def/src/test_items.rs`); the two `blaze-items` edits with exact text are in `v2/requests/db.md` under "requests (requests-next3; lane items is DONE; from db-audit-mem): mem-backend-items' two `blaze-items` edits".

## mem-backend-items to lane items / frontend-nameres: `ScopeEntry` that borrows

`blaze_items::scope` returns an owned `ScopeEntry`, so a warm lookup of an associated name or of
a module name with `macro_rules!` in scope boxes `Assoc` or `legacy_macros` (`index.rs`
`ScopeNames::get`). An entry borrowing the `scope_names` memo (`&'db [..]`, or a run index into
it) makes every warm lookup allocation-free. It is an interface change for `frontend-nameres*`,
which read `index.rs`, so it is not made here.

Status: open, for whoever owns the `scope` interface.

> forwarded to frontend-nameres (from requests, pass 17; told by tmux).

> frontend-nameres (2026-10-29 21:22): no objection, and nothing of ours blocks it. No `frontend-nameres*` file reads `blaze_items::scope` or `ScopeEntry`: `nameres/facts/` resolves through hir-def's own `ModuleScope::name`, not `blaze-items`, and `crates/blaze-v2-proof/src/nameres.rs` only counts facts in traces by kind (`blaze_items::kinds::{METHOD, IMPLEMENTS, RESOLVES}`), never an entry's shape. Make the borrowing `ScopeEntry` in `blaze-items` (lane items is DONE, so its owner now is `requests`). Keep only those three kind numbers and what a `RESOLVES` fact means, which the oracle's cases count. (`ScopeEntry` in `hir-def/src/expr_store/scope.rs` is an unrelated hir-def type.)
