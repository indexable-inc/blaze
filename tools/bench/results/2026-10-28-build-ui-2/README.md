# Build UI round 2: every step named, the live view a tree of the crates (2026-10-28)

Lane tui. The build now says every step it takes with a count (`Event::Progress`), the live view
draws the crate graph (lane sansio's `Event::Node`/`Moved`), and a build that took over a second
ends saying why (`Took`, `Cache`). This compares main at the merge base with the change, on the
same machine.

## Method

ix VM `tui-r2b` (arm64, 32 vCPUs, 16 GB), ripgrep 15.1.0, every build pinned to 8 cores
(`taskset -c 0-7`). A cold build on all 32 cores ran the VM out of memory. Each arm has its
own `BLAZE_HOME` and its own store (`XDG_CACHE_HOME`), so it has its own daemon.

- `bench.sh`: both arms' daemons are warm on their own copy of ripgrep. 20 body edits and 30
  no-ops per arm, alternated run by run with the order flipped every other pair. Each run
  records its wall time and the build's own time (the `Finished` record's `at_ns`, from
  `--events json`). The output is in `interleaved.jsonl`.
- `bench-cold.sh`: per arm and round (ABAB): a cold build with an empty store, 5 edits and 10
  no-ops. The output is in `cold.jsonl`. It was taken one revision before the final one, which
  changed only how progress counts are said (one thread for the process instead of one per
  step). That revision's per-step thread is what cost the no-op 0.58 ms below.
- `captures.sh`: a cold build, a body edit and a second checkout over the same store (a warm
  store with a cold daemon), each under `script` at 110 columns. Then the edit piped, `blaze why`,
  `blaze status`, a no-op with `--events json`, and five timed no-ops. `render.py` plays the
  terminal captures back as text. The results are in `before/` (main) and `after/` (this change).

## Numbers

ripgrep, ms, median (quartiles):

| | main | change |
|---|---|---|
| no-op, wall, 30 alternated | 19.85 (19.66–20.09) | 19.68 (19.50–19.80) |
| no-op, the build's own | 1.86 | 1.88 |
| body edit, wall, 20 alternated | 130.32 (129.67–132.07) | 131.07 (129.73–135.26) |
| body edit, the build's own | 110.56 | 111.54 |
| cold, empty store, 2 rounds | 25.86, 25.87 s | 25.90, 26.20 s |

The revision before the last spawned a thread for each progress count. Because a no-op re-runs
the build-script driver, that thread cost a no-op +0.58 ms (19.41 → 19.99 ms). One thread now
says every count of the process and waits while there is none.

## What the captures show

- Cold. main said `Loaded workspace in 16.8s` and nothing about what loading was. The change
  says `Compiled the standard library of build scripts in 11.9s` and then `Built build scripts
  and proc macros in 3.58s`. While it runs it shows `3/14 steps · longest anyhow, compiling its
  build script`, and then the tree of the crates being resolved and compiled. The last lines
  are `Took 33.2s: compiling the standard library (once, kept in the store) 20.6s, compiling
  functions 3.97s, building build scripts and proc macros 3.71s` and `Cache cold: the store
  is empty`.
- Warm store, cold daemon. main ended with `Compiled 1 fn in 1 crate, 31219 kept` and
  `Linked rg in 2.71s`, which included the 1.7 s of storing the manifest after the link. The change
  says `Built from the store: 31219 fns found, 1 compiled` and names `Stored the manifest in 3.83s`
  (1.73 s in an earlier run): storing the manifest is the largest step of that build.
- Edit. The line is unchanged (`Rebuilt 1 fn: search (crates/core/main.rs edited); 0.18s`).
  `blaze why` now lists the edit's steps: the 14-step build-script driver takes 0.07 s of the
  0.11 s. That happens on main too, and is reported to perf/coldfast in common.md.
