---
status: plan
summary: lane firewall's bound — what a body edit costs at best (validating every memo once plus executing the edited body's queries), how the harness measures against it, and where the time goes.
date: 2026-10-30
---
# Speed of light: a body edit

An edit of function `F` in a crate of `B` bodies whose requests touch `M` memos can do no less
than:

- **execute `F`'s own queries**: about a cold pass's share per body, `C / B`, where `C` is a
  cold pass (every query executed once). A body's check is 11 µs or more
  ([`../frontend/fast-bodies-tasks.md`](../frontend/fast-bodies-tasks.md)), and its file's
  firewalls (reparse, `AstIdMap`, item tree) add the file's share, which incremental reparse
  bounds by the edited item ([`../frontend/reparse.md`](../frontend/reparse.md));
- **validate every other memo once**: what a pass in a revision that changed nothing costs,
  `V`. Salsa's deep verify of a memo whose inputs are unchanged is a load of its verified-at
  revision and a walk of its edges: about 20 to 50 ns a memo on one core, so `V ≈ M × 35 ns`.
  Nothing faster is correct without a dependency index that skips unread subgraphs, which is
  the red-green graph's own cost model (rustc's `try_mark_green` is the same walk).

The bound for one edit is `V + C / B`, and the harness's edit is two passes (the edit and its
undo), so **bound = 2 × (V + C / B)**. A leak adds the leaked queries' execution, which grows
with the crate rather than with `F`: a leak of `body_syntax` per body of the file adds
`(bodies in the file) × its cost`, which the bench shows as a ratio above 1.

## The bench

`crates/hir-firewall/benches/edit.rs` makes one crate of 200 functions in 10 files, times the
harness's setup (a cold and a warm pass), a warm pass alone (`V`), and every seventh function's
edit and undo; it prints the time per edit against `2 × (V + C / B)` and the keys executed and
validated per edit, with the verdict `within 10% of the bound` or `OVER`. Every pass of the
harness is logged (one event per memo touched, behind a lock), and so is `V`: the overhead is in
both sides of the ratio.

## The machine (speed-sol2-small)

As [link/speed-of-light.md](../link/speed-of-light.md)'s "The machine": 3.5 GHz, about 2
instructions a cycle for branchy code, DRAM about 110 ns, BLAKE3 about 6 GB/s a core, a system
call 0.15 µs. Nothing here is measured. Reference workloads:

- **the bench crate**: 200 functions in 10 files, 20 to a file, each body about 60 syntax nodes;
- **a large crate**: 10,000 bodies in 200 files, 50 to a file, about 40 memos touched per body's
  requests (check, code hash, their inputs), so `M` ≈ 400,000.

## The edit, counted (speed-sol2-small)

| | the bench crate | a large crate |
|---|---|---|
| `V` = `M` × 35 ns | 8,000 memos: 0.28 ms | 400,000 memos: 14 ms |
| `C / B` | 11 µs and the file's share | the same |
| this bound, `2 × (V + C / B)` | 0.58 ms | 28 ms |
| the leak today (00845): every body of the edited file runs `body_syntax` again | 20 × ~0.75 µs (an anchor's `tree_hash` and walk) × 2 passes: 30 µs | 50 × ~1.5 µs × 2: 0.15 ms |
| today against this bound | ~1.05×, within 10% | ~1.01× |

So against this lane's bound the one known leak is within 10% on both workloads; it grows past
10% only for a file of many large bodies in a small crate, where `V` is small. The harness reports
it as a leak either way, so it is fixed at its cause (00845, lane format: `body_syntax` keyed per
item, not by the file's parse).

**What this bound leaves out.** The principles' bound for an edit is recomputing only what
changed plus each stage's fixed latency: here `C / B` and the edited file's reparse, about 11 to
20 µs. `V` is not work the edit needs; it is what salsa's pull model pays to learn that nothing
else changed (a deep verify of every memo a build's requests reach). On a large crate `V` is
14 ms against ~20 µs of real work, three orders of magnitude off. A body edit reaches the
principles' bound only when the requests themselves are pruned: a push of the changed inputs to
the memos that read them (lane dbsp, incremental view maintenance, in progress) or durability
levels that let a revision skip the unchanged files' memos (lane db). Both are other lanes', not
done; this harness is how either one proves itself: its `touched` count per edit drops from `M`
to the edited body's memos.

**The harness's own overhead.** `Db::logged()` and the fixture database take a lock per salsa
event even outside `Db::log` (`crates/base-db/src/db.rs`, the event hook), and `Harness::pass`
pushes each event into a `Vec` and records it. That is test infrastructure, never a build's
path, and it is on both sides of the bench's ratio, so the verdict holds, but the absolute
times above are about twice a build's. An `AtomicBool` that `Db::log` sets, read before the lock,
makes events outside a log one relaxed load: queued to lane db (00891).

## What `proof` should measure (speed-sol2-small)

- Per edit, exactly: keys executed (the edited body's queries plus the exceptions E1 to E10,
  nothing else; the harness's `Report::is_clean`), keys touched (today `M`; the target once
  pruning lands is the edited body's), and code hashes changed (1).
- Per edit, timed on a database with no event hook: the edit's wall time against
  `2 × (V + C / B)` today (the bench's verdict) and against `C / B` plus the reparse once
  pruning lands.

## Where the time goes, and how the harness stays near the bound

- One pass validates each memo once: the requests are the ones a build makes, and each key's
  memo is verified at most once a revision.
- Classifying a key costs only for keys executed again (a few per edit): a set lookup in `W`
  first, and the decode of the key's ids only when it is in `W`.
- The codegen walk describes each instance once per pass and does not descend into a
  dependency's instances, which no edit of the crate changes.
