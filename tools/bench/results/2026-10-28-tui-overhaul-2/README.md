# Live view overhaul, piece 2: how busy the cores are, and cold from the start (lane tui)

A view that shows a slow build must show why, and "it runs on one core" or "it waits" is
one of the answers (Andrew's 100 s was one core). This piece adds:

- `Event::Cores { busy, of }`: the worker's CPU every 250 ms while it is at a step, in
  hundredths of a core of the cores it may use. It is said only when it moved by a tenth of a
  core, so an open step that waits says it once.
  - The CPU counted is the worker's own (`getrusage(RUSAGE_SELF)`), its reaped children's
    (`RUSAGE_CHILDREN`), and on Linux every live child and grandchild's from `/proc`.
    Without the children, compiling the standard library of build scripts read 0.0 of 8
    cores for 11 s: it runs in a child process.
  - The process's one "counts" ticker thread does it, and only while at a step. `at()`
    unparks it on going from waiting to a step.
- The live view draws `cores ▁▂▇█████▇▆▃▁ 7.4/8 busy` under the steps: the last 10 s, half a
  second a block.
- The end gives each slow step the cores it kept busy on average:
  `Took 33.9s: compiling the standard library of build scripts 12.2s (3.9/8 cores), ...`.
- The engine says why the build is cold as the workspace starts loading, when the store
  already says so (no store, empty, or another blaze wrote it last), so the header shows
  `· cold: the store is empty` from the first second, not at the end. This happens only once
  the toolchain is known: a store opened before that is none for good.

## Numbers (ix VM tui-r2d, arm64, 16 cpus, 16 GB; ripgrep 15.1.0; 8 cores via taskset)

Main (d1497be239) and this branch alternated run by run, each with a warm daemon of its own:

| ripgrep, median | main | this branch |
|---|---|---|
| no-op, worker's own time (`--events json`, Finished `at_ns`) | 1.93 ms | 1.94 ms |
| no-op, wall (`--events json`, 30 each) | 20.82 ms | 19.92 ms |
| edit, worker's own time | 113.25 ms | 112.53 ms |
| edit, wall (`--events json`, 20 each) | 133.87 ms | 133.72 ms |
| no-op on a terminal (live view drawing, 30 each) | 32.81 ms | 32.34 ms |
| edit on a terminal (20 each) | 144.00 ms | 143.55 ms |

`bench-events.sh` → `events.jsonl`, `bench-terminal.sh` → `terminal.jsonl`. A cold build
(`after/exits`) took 39.3 s with the toolchain download. The worker's own time on a no-op
moved 0.01 ms, under the 0.2 ms budget.

## What it found (cold ripgrep, `after/cold.txt`)

- The two standard-library compiles and the build scripts use about half the cores
  (3.9, 3.4 and 2.4 of 8 on average).
- The engine's standard library (`Compiling the standard library (once, kept in the store)`)
  keeps all 8 cores busy for about 5 s, then sits at 0.0 of 8 for about 2.5 s before
  codegen starts. The worker is waiting, probably on writing the store. Told coldfast
  (common.md).
- Storing the manifest after a warm build: 1.68 s at 0.6 of 8 cores.

`after/` holds every 20th frame of cold, edit and warm, and the screen left at the end
(`render.py`). Main's captures are in `../2026-10-28-tui-overhaul-1/before/`.
