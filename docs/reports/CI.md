# CI: where the fast tier's time went, and the merge queue (2026-10-22)

What CI does is in [harness.md](../harness.md#ci). This is what I measured before changing it,
and why it changed as it did.

## What I measured

I pulled every run of `blaze.yaml` since 2026-10-20 (269 runs) with `gh api
.../actions/workflows/blaze.yaml/runs` and the jobs and steps of each, and read the logs of the
slowest jobs. The numbers below are for the runs after the fast and full tiers split
([#67](https://github.com/indexable-inc/blaze/pull/67), 03:52Z), unless they say otherwise.
Times are wall times from the run's start; "queue wait" is a job's creation to its start.

- **The fast tier on main: 13.3 minutes at the median, 20.4 at p90** (22 runs that built code,
  03:52Z to 20:13Z). From #97's merge at 20:13Z on, every run took 33 to 36 minutes (4 of 4).
- **Queue wait is nothing**: p50 0.1 minutes, p90 under 0.5, on every kind of runner. The
  organization is on GitHub Enterprise (500 concurrent jobs, 50 of them macOS) and the
  repository is public, so hosted runners cost nothing and are never short.
- **Building is not the long part.** sccache hit 75 to 97 percent of compilations on most runs
  (a few after a `Cargo.lock` change hit 0 to 8 percent); the `cargo build` step took 1.8 minutes
  at the median on Linux x86-64, 1.7 on AArch64 and 2.4 on macOS.
- **macOS `quick-2`: `norustc.py` took 1489 of the part's 1730 seconds** (run 39851906782), where
  it takes 66 to 70 seconds on Linux. The harness came with #97. On macOS its `BLAZE_HOME` is
  under `$TMPDIR` (`/var/folders/...`), the daemon's socket path inside it passes the 104 bytes
  macOS allows, so the daemon never listened: the cold build ran cargo 623 times (28 on Linux)
  and each scenario ran until the log was still. It records nothing there either: the baseline
  has counts for `linux-x86_64 cranelift` alone.
- **Linux x86-64 `quick-1`: `apt-get install valgrind gdb` stalls.** 24 of 929 runs of that step
  since 2026-10-21 took over two minutes, 230 minutes over one in all; the stalls are 8 to 12
  minutes each (11 min 41 s for 26 MB at 37 kB/s from azure.archive.ubuntu.com in one) and came
  in clusters, in every run from 17:28Z to 20:06Z. Of the 14 runs on main over 15 minutes since
  the split, 7 ended on such a job and 5 on macOS `quick-2`. The LLVM install (`apt.llvm.org`) in
  the toolchain step took over two minutes in 59 of 1693 runs.
- **The longest path when nothing stalls** is the macOS build (about 4 minutes, with the toolchain
  and LLVM from Homebrew) and then its slowest harness part (`quick-1` under Cranelift, 5.6
  minutes of harness and 0.7 of setup): about 11 minutes.
- **The Actions cache is over its 10 GB**: 10.7 GB in 1813 entries. rust-cache's entries are
  0.8 GB each (two per key for six keys of this workflow, more for others); sccache's are
  1.6 GB in about 1300 small entries, 0.5 GB of them written by pull requests, where nothing
  else can read them; Nix's from `nix-live.yaml` 1.2 GB, half from pull requests. The oldest
  sccache entry was 18 hours old: eviction, not the 7-day limit, is what clears it.
- **Failures.** No run was re-run. On main, 7 code runs in a row failed from 04:54Z to 17:22Z
  (12.5 hours), all on macOS `quick-1`, until the fix for #74 landed: lanes merged onto a red main
  and pushed, since nothing stopped them. One job failed in infrastructure (an artifact download
  on macOS). Pull requests: of 23 finished runs with code, 8 failed and 8 were cancelled by a
  newer push.
- **How main moved.** 82 merges on main's first-parent line on 2026-10-21 and 22; 727 of the
  commits since 2026-10-15 are Andrew's identity (the lanes), each pushed straight to main past
  the ruleset, which was in `evaluate`. The ruleset's log shows every such push as `bypass`, and
  the `github-actions[bot]` pushes (`pgo.yaml`'s pin, the bench numbers) as `fail`: they are
  refused once it is active.

## What changed

- `norustc.py` keeps its work directory under `/tmp` on macOS, as `blaze_cli.py`,
  `loadedits.py` and `headerpad.py` already did. Expected: macOS `quick-2` back to about 5
  minutes, the fast tier back to about 11.
- The harness jobs install their Ubuntu packages through `.github/actions/apt`, which keeps the
  `.deb` files in the Actions cache per runner image (written by main). A stall then costs the
  first run on a new image, not one run in thirty.
- sccache is read-only off main (`SCCACHE_GHA_RW_MODE`), as rust-cache already was.
- **The merge queue.** The ruleset is active with a `merge_queue` rule: merge commits, up to five
  groups built at once and five pull requests merged together, every entry green
  (`ALLGREEN`), a check timeout of 60 minutes, `ci` required. A pull request's own run is
  `fmt` alone, so it enters the queue within a minute and its landing waits for one fast tier,
  not two. Main moves only to a commit the fast tier passed on. Lanes land with
  `.github/scripts/land.py`, which opens the pull request, queues it and waits for the result.
  The repository now deletes a merged branch, allows auto-merge (a pull request waits for its
  checks and then queues itself), and titles a merge commit with the pull request's title.
- The workflows that commit to main check out with `BLAZE_BOT_TOKEN` when the secret is set,
  since the workflow's own token cannot pass the ruleset.

## What was not done, and why

- **Larger or self-hosted runners.** Nothing waits for a runner, and the longest path is two
  jobs of 4 and 6 minutes; a larger macOS runner (paid) would take perhaps 2 minutes off it. A
  self-hosted runner on a public repository runs the code of whoever opens a pull request, and
  with the queue only queued pull requests run the fast tier at all. Neither is worth it yet.
- **Skipping the run on main after a queue merge.** The queue moves main to the commit it
  tested, so that run tests it again; but it is the run that writes main's caches, which every
  queue entry reads, and its runners are free.
- **The cache's size.** GitHub sells more than 10 GB; with pull requests and queue entries no
  longer writing sccache, the space should go to main's entries. `nix-live.yaml`'s pull
  request entries (0.65 GB) are still written.

## 2026-10-26: a lean pull request and queue, the rest nightly

Measured from every `blaze.yaml` run of 2026-10-25 10:21Z to 10-27 00:33Z (the jobs, steps and
the logs of every failed job, with `gh api`):

- **Merge queue: 140 runs, 51 failed (37%).** Passing runs took 39.5 minutes at the median and
  45.4 at p90; failing ones 43.1 and 48.8. Pushes to main the same (40.0, 45.7). No job waited
  for a runner (0.1 minutes).
- **The longest path** was the macOS build (6.4 minutes) and then macOS `quick-2` (33 at the
  median, 37.6 at p90). `wholeprogram.py` alone took 644 to 1309 seconds of it and failed in
  none of the runs. The Linux `quick-2` parts took 20 to 23 minutes for the same reason.
- **Most failures were real breakage that sank whole groups.** A pull request's own run was
  `fmt` alone, so a broken change met the harnesses first in a queue group of up to five: of the
  126 pull requests in failed groups, 94 later landed unchanged. Repeated causes: `live_types.py`
  (36 jobs, in clusters), `norustc.py` (46), the consteval unit tests (30), `selects.py` (28),
  `renamedstatics.py --not-incremental` (22), `coldbuild.py` (10).
- **Flakes** (the same commit failed and passed): `testselect.py`'s "that test taken out of the
  sandbox" (about 15 Linux runs, pushes to main among them; #412), `aheadready.py` on macOS (6;
  #413), and the `git fetch` of rust-lang/rust in `borrowck` and `pinned-ui` refused with HTTP
  429 (26 jobs).
- **`nix build (x86_64-linux)`** in `nix-live.yaml` failed on every run: the flake's `dist` ran
  `build_release.py`, whose glibc 2.34 floor refused binaries linked against nixpkgs' glibc
  (2.39 and newer).

What changed: a pull request runs the fast tier on Linux x86-64 and must pass it to be queued;
the queue adds Linux AArch64 and macOS, each with the harnesses where the platform matters
(`FAST` in `harness/ci.py`); the harnesses are packed into parts of about four minutes; a push
to main only builds and runs the unit tests, which write the caches; `wholeprogram.py`,
`aheadready.py` on macOS and `nix-live.yaml`'s builds moved to the nightly, which opens an issue
per failed job; every harness gets a daemon home of its own; rustc's tests come from the Actions
cache; `testselect.py`'s flaky step is quarantined in the fast tier (#412); the flake's `dist`
checks its binaries with `build_release.py --nix`. The numbers after are in harness.md's CI
section once measured.

## 2026-10-27: a merge gate of minutes, and the release train

The queue still took 35 to 45 minutes a run after the change above, with no wait for a runner:
the builds took 2 to 5 minutes (sccache hits), the unit tests 3 to 6, and the rest was
harnesses, `quick-2` taking 20 to 30 minutes on each platform. Every entry of the queue paid
that, for failures the harnesses could as well report on main with the commits they came in.

What changed: the merge gate (`blaze.yaml`, the `ci` check) is the build and the unit tests on
Linux x86-64, `harness/imports.py` and `harness/rustcsites.py`; `cargo fmt --check` is gone,
and `fmt.yaml` formats main after each push. Everything else is the release train
(`train.yaml`), on each push to main, one at a time on the newest commit: the full tier on four
platforms, and `nix-live.yaml` (not blocking until it has been green a while). Its `core` part,
36 to 47 minutes as one job in the nightly of 2026-10-26, is packed into `core-1` ...
`core-N` by measured seconds, so that no part is longer than `wholeprogram.py` (19 minutes on
macOS). A green train releases main when it has changes since the last release, through
`pgo.yaml` and `release.yaml`; a red one opens or adds to one issue. The nightly schedules of
`blaze.yaml` and `nix-live.yaml` are gone; `tools/nix/units` was regenerated, after a week stale.
