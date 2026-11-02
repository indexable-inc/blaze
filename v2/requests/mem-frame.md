# Requests of mem-frame (frames past the spare depth, `frame::measure`, NUMA and the cap)

mem-frame owned `crates/blaze-mem/src/{frame.rs, frame/cap.rs, chunks.rs}`, the NUMA parts of
`region.rs` and `os.rs`, and moved `blaze-graph` off `arena::{Bump, task, with, Cap}`. Nothing
here was built or run: the pass allowed no builds.

## 1. lane mem (`crates/blaze-mem/src/sync.rs`, `frame/cap.rs`, `model.rs`): the cap under the model checker

`blaze-graph` had a `blaze_check` model of `arena::Cap`
(`model_a_full_cap_cancels_one_of_two_stuck_holders`): two holders each holding part of a full
cap and asking for more. Exactly one is cancelled, the other goes on, and the total comes back
to 0. It was deleted with `arena::Cap`. `frame::Cap` (`frame/cap.rs`) uses `std::sync::{Mutex,
Condvar, atomic, mpsc}` directly, because blaze-mem's `sync` shim has only atomics and
`OnceLock`. Asked:

- `sync.rs` re-exports `Mutex`, `Condvar` (and `MutexGuard`, `PoisonError`) from `std` or
  `blaze_check::sync`, and `cap.rs` takes its lock, condvar and atomics from `crate::sync`.
  The spill channel can stay `std::sync::mpsc`: it only sends.
- `model.rs` gains the model above, ported onto `frame::Cap::reserve`/`Grant`, plus one where a
  thread that holds nothing waits behind a full cap and is let in once a grant drops (never
  cancelled).

> forwarded to mem (mem-next1); told by tmux (requests, pass 31).

> done by mem (2026-10-29): `crate::sync` now gives `Condvar`, `Mutex`, `MutexGuard` and
> `PoisonError` (std's, or blaze-check's under `cfg(blaze_check)`). `cap.rs` takes them and its
> atomics from there. `INSTALLED` stays std's `AtomicPtr`, since it is process-wide and outside
> any model. The spill channel stays `std::sync::mpsc`. `model.rs` gains
> `model_a_full_cap_cancels_one_of_two_stuck_holders` (no deadlock; at most one cancelled;
> every byte back) and `model_a_thread_holding_nothing_waits_and_is_never_cancelled`.

## 2. lane mem (`crates/blaze-mem/src/lib.rs` line 7): the frame row of the table

The row still says "a bump pointer per worker over a reserved [`Region`]". Asked, in its place:
"[`Frame`]: a bump pointer over a chain of 2 MB chunks and spans, a frame per task (an inner
task starts in the rest of its outer task's chunk), with [`Marker`]s and reset; [`frame::task`]
hands each task one".

> forwarded to mem (mem-next1); told by tmux (requests, pass 31).

> done by mem (2026-10-29): the row reads as asked.

## 3. lane mem (`crates/blaze-mem/src/tag.rs`): `task_ended` per task on a shared line

`frame::task`'s end calls `tag::task_ended(tag, peak)`, which does a `fetch_add` and a
`fetch_max` on the tag's line. That line is shared by every worker that ends a task of the
tag. Warm, it is the only shared write a task makes: about 40 cycles uncontended, and more when
many workers end tasks of one tag at once (salsa's nested queries, all `Tag::Infer`). It is a
speed note, not a bug. A per-thread sum added in batches, like `blaze_graph::arena`'s `Local`
(with the max kept per thread and folded in at a snapshot), would take it off the task path.

> forwarded to mem (mem-next1); told by tmux (requests, pass 31).

> done by mem (2026-10-29). `task_ended` writes no shared line when warm. The peak does a
> relaxed load and then `fetch_max` only when it rises. The bytes gather in a thread-local per
> tag and fold into `allocated` at 1 MiB, at a `snapshot` the thread takes, and when the thread
> ends. Another thread's bytes can reach the rate up to 1 MiB per tag late.

## 4. drive's cancelled work

`blaze_graph::exec::drive` reruns work that the frames' cap cancels (`frame::Cancelled`) on
the same thread, at once. Only a daemon scheduler with a budget installs a cap, so this
happens only when a `drive` build runs in the same process as such a scheduler. No change is
asked. A reader of `drive` should know that its work may run more than once.

> applied: nothing to apply: a note for readers of `blaze_graph::exec::drive`, no change asked (requests, pass 31).
