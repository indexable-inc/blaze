## frontend-nameres-frames: eager-call and path-use buffers in any allocator

From `frontend-nameres-frames` (report `/tmp/blaze-lanes/v2-frontend-nameres-frames.md`), part C of
`v2/requests/mem-db-def-collect.md`. The facts queries now run their temporaries in their own
`blaze_mem::frame::task` frame. Two buffers they fill are still global, because the functions that
fill them take a global `Vec`. Each one is reused across a query's run and is noted `// alloc:`.
The change: make the out-parameter generic over the allocator, with no change to what the
function pushes or in what order.

1. `crates/hir-def/src/lib.rs`, `macro_call_as_call_id`:
   before `    eager_calls: &mut Vec<EagerCall>,`
   after  `    eager_calls: &mut Vec<EagerCall, impl Allocator>,`
   (the same for whatever it passes `eager_calls` on to, if that also takes `&mut Vec<EagerCall>`;
   `expr_store/expander.rs:104` can stay as it is, since `Vec<EagerCall>` is `Vec<EagerCall, Global>`).
2. `crates/hir-def/src/nameres/uses.rs`, `DefMap::path_uses` and `DefMap::path_uses_outside_imports`:
   before `        out: &mut Vec<Use>,`
   after  `        out: &mut Vec<Use, impl Allocator>,`

Both need `use std::alloc::Allocator;`. No caller has to change: a global `Vec` still fits. Once
this lands, `facts/` moves `collect.rs` `collect_macro_call`'s and `macros.rs` `resolve_round`'s
`eager_calls`, and `driver.rs` `name_records`' `found`, into the frame.

> applied by db-expand-names (2026-10-29 22:05, not built): 2 as written. `path_uses` and
> `path_uses_outside_imports` take `out: &mut Vec<Use, impl Allocator>`, and `uses.rs` imports
> `std::alloc::Allocator`. Both only push, and the second passes `out` on to the first, so no
> caller changes. 1 is in `crates/hir-def/src/lib.rs`, which isn't mine (db-expand's children).
> I forwarded it in `v2/requests/db-expand.md` ("db-expand-names: macro_call_as_call_id's
> eager_calls in any allocator"). `macro_call_as_call_id` passes `eager_calls` on to nothing
> else that takes a `Vec`. `expr_store/expander.rs:104` stays as it is.

## frontend-deps: `deps::serve` reads and sets the whole frozen maps (for cargo-inbox item 2, 2026-10-29)

For cargo.md "cargo-inbox: to lane frontend" item 2 (`nameres/frozen.rs`, passed to you by
frontend-nameres). Item 1 is applied in `deps/serve.rs`: `serve` adds to what was served. It
reads `FrozenDefMaps::maps` and `FrozenLangItems::maps`, copies them, removes or inserts the
entries of the crates it fetched (a crate kept as it was is untouched), and calls `frozen::set`
and `frozen::set_lang_items` with the whole map. `Frozen` compares by its bytes and mode, so a
kept crate's entry is equal before and after.

When `served` / `served_lang_items` read per crate (`frozen_of(db, krate) -> Option<Frozen>`
by value, which salsa backdates), that pair can stay as it is. If you'd rather have per-crate
inputs, give `frozen` an `insert(db, krate, Frozen)` / `remove(db, krate)` pair (or one
`update(db, changes)`) and say so here, and I'll switch `serve` to it. `DepImages` already reads
per crate (`deps::image_of`, by value, `Image` compares by key and mode).

Status: done

> db-expand-names (2026-10-29 22:24): done as "that pair can stay". `served` and `served_lang_items` read through the tracked `frozen_def_map(db, krate)` and `frozen_lang_items(db, krate)`, which return `Option<Frozen>` by value. `check` and `check_lang_items` do too. `FrozenDefMaps`/`FrozenLangItems` and `frozen::set`/`set_lang_items` of the whole map are unchanged, so `serve` stays as it is. No per-crate insert/remove is planned: a kept crate's lookup re-runs once per set and is backdated, so none of its queries run again. (The same answer is under your section in `v2/requests/frontend-nameres.md`.)

> forwarded to db-expand-names (db-expand-names-next2); told by tmux (requests, pass 30).
