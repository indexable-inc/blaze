# The build page: what it costs a build and the daemon (2026-10-28)

Lane web. `blaze ui` serves the build page (docs/web.md). The daemon makes each workspace's
program a Flecs world (`crates/blaze-web`) once the page is first served. Until then it keeps
only the records the world would be made from, gzipped.

## Method

ix VM, x86_64, 16 vCPU, 32 GiB committed (us-west-2), nothing else running. *Before* is the
functions' records change (#1272, `65b6dfcd39`), the base of this one; *after* is this branch
before `b9c829c357`, when the daemon fed every build's records into a Flecs world whether or not a
page was open. That is more work during a build than the branch does now (it keeps the records
gzipped until `blaze ui`), so the times are an upper bound for it. The final branch, merged with
main, was not timed: two ix VMs lost their disks during that run (ix, not blaze), and it was
measured for memory alone (below). Both are `cargo build --release -p blaze-engine` in `nix develop`. The project is anim
(indexable-inc/anim 92c64f7: 69 crates, 79,696 functions and instances, 149,538 calls).

The time is from `bench/results/2026-10-28-web-events/bench.py` with `input.json`. Each arm has
its own `BLAZE_HOME`, store and copy of anim, and the arms take turns throughout: 2 cold builds
each, then with the last cold's daemon 3 warm-ups, 30 no-op `blaze build`s and 8 body edits of
anim's `main`. Each is a fresh process timed whole from Python. `report.json` has every run.
No page is open, as for anyone who never runs `blaze ui`.

The memory is the daemon's `VmRSS`, in runs of their own on the same VM with a fresh home and
store. One run has no page: a cold build, then an edit, then `blaze ui`. The other has the page
open in headless Chromium from before the cold build through two edits
(`bench/results/2026-10-28-web-graph/live.py`).

## Numbers

anim, ms (wall time of `blaze build`):

| | before | after | difference |
|---|---|---|---|
| no-op, median of 30 | 6.81 | 5.73 | -1.08 |
| no-op, p90 | 8.21 | 6.85 | -1.36 |
| body edit, median of 8 | 1286.7 | 1294.2 | +7.5 (min 1266.6 vs 1267.6) |
| cold, median of 2 | 211,893 | 212,338 | +445 (0.2 %) |

No cost within noise: the no-op is faster in the after arm (the arms alternate, so this is
noise or the daemon's records leaving the waiting command's path), an edit's minimum is the
same, and the colds' ranges overlap (209.4-214.4 s vs 211.0-213.7 s).

The daemon's memory on anim:

| | before | after |
|---|---|---|
| no page, 5 s after the cold build | 30.6 MB | 51.7 MB |
| no page, 60 s after an edit | | 49.4 MB |
| 5 s after `blaze ui`, the world made from what was kept | | 171.8 MB |
| page open through the cold build and two edits | | 121 to 167 MB (two runs) |

With no page, the daemon keeps the cold build's 37 MB of functions' records gzipped: about
20 MB more than before. Kept as decoded values instead, they took the daemon to 97 MB, and an
always-made world took it to 121 to 167 MB. Once `blaze ui` has made the world, every query of
it over Flecs's REST took 1 to 1.5 ms (`Crate,(Calls,*)`, `Module`, `Function`, one page of
1,000), and all 79,696 functions came back in 49 ms. The page's snapshot of anim is 4.3 MB
gzipped.
