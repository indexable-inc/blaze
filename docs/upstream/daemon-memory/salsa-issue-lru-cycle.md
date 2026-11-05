# Draft: salsa issue, `lru` on a query with cycle handling throws `Cancelled::PropagatedPanic`

Written 2026-10-22 for [salsa-rs/salsa](https://github.com/salsa-rs/salsa); not filed yet. The
patch it proposes is `salsa-0.28.5-evicted-memo.patch.txt` beside it.

---

**Title:** An LRU-evicted memo of a query with cycle handling is taken for a panicked cycle head

**Version:** 0.28.5 (the same code is on master as of 2026-10-21).

**What happens.** Put `lru = N` on a tracked function that also has `cycle_result` or
`cycle_fn`. After a revision in which its memo was evicted, the first `fetch` of it that
re-executes throws `Cancelled::PropagatedPanic`, though nothing panicked. `Cancelled` is resumed
with `resume_unwind`, so no panic hook runs and a program that does not catch it exits with
status 101 and no message.

**Why.** Eviction (`evict_value_from_memo_for`) sets the memo's value to `None` and keeps its
revisions, so a later revision can deep-verify it and mark it verified at the current revision
without a value. When something then needs the value, `execute_maybe_iterate` asks
`MemoHeader::previous_iteration` about the old memo, which, for a memo verified in this
revision with `has_value == false`, assumes it is a cycle head that panicked earlier in the
revision and throws (`function/execute.rs`, the `if !has_value` branch with the "Propagating
panic for cycle head" warning).

The two cases can be told apart. The memo a panic leaves (`PoisonProvisionalIfPanicking`,
`QueryRevisions::fixpoint_initial`) is provisional (`verified_final == false`) and lists itself
in its cycle heads. An evicted memo was final when it was evicted, and `cycle_heads()` of a final
memo is empty. The origin cannot tell them apart: `fixpoint_initial` builds a
`QueryOrigin::derived` with no edges, as an executed query's is.

**Fix proposed.** In `previous_iteration`, a memo without a value that is not its own cycle
head is treated as an earlier revision's memo (`PreviousIteration` with
`IterationStamp::initial` and `reuse_as_provisional: false`), as a memo verified in an earlier
revision is. And `can_evict_value` refuses a provisional memo, whose value fixpoint iteration
reads. Both are in the attached patch (13 lines).

**How it was found.** rust-analyzer's `infer` and MIR queries have cycle handling. With
`lru = 2048` on `infer`, `mir_body` and `runtime_mir_body` (and 1,024 on their closure
queries) in [blaze](https://github.com/indexable-inc/blaze), a daemon that builds
rust-analyzer's MIR, nushell's cold build died: under lldb the throw was
`Cancelled::throw` from `MemoHeader::previous_iteration` inside `infer`'s `execute`, reached
from another query's `maybe_changed_after`. With the patch the same build and its edits finish,
and each program built behaves as cargo's.

**Not covered.** A regression test: one would evict a memo of a cycle-handling query in one
revision, deep-verify it in the next through a dependent, then fetch it. The patch was checked
by blaze's own suites, not salsa's.
