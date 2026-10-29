# Live view overhaul, piece 1: header, steps strip, what held the build up (lane tui)

Andrew (11:35Z): a glance at the view must say what blaze is doing, whether it is stuck, how
far along it is, why it is slow, and at the end why it took that long. This piece is the
renderer only (`crates/blaze-ui`); the worker writes no new event and does no new work.

- A header: `blaze build · cold: <why>` and the time so far at the right.
- A steps strip: every kind of step so far, `✓` done or `●` under way, with its count and
  time (`✓ workspace 0.14s  ✓ build scripts 14/14 steps 2.69s  ✓ std 17.8s  ● codegen 0.44s`);
  the first done ones fold into `✓ N more` when the terminal is narrow.
- The end, for a build over 1 s:
  - `Took` names each step of a different thing apart. Main merged the standard library
    of build scripts (11.9 s) and the one kept in the store (9.6 s) into one
    "compiling the standard library (once, kept in the store) 20.4s".
  - `Chain a → b → c 1.90s` says what held the build up, from the graph's own
    `Event::Critical` (blaze-graph). It is absent when the graph doesn't say it. The live
    footer's "longest chain" summed each crate's busy time, but blaze compiles every
    crate's functions at once, so it said 24.6 s of a 27.9 s build. It's gone.
  - `warning: 1.50s with no step named, after names` for any stretch over 1 s with no
    step under way. A phase with no name is a bug, and the warning is a line no transcript
    has, so `harness/experience.py` fails on it.
  - `` `blaze why` says more ``.
- `harness/blaze_cli.py` STATUS accepts `Chain` and the `blaze why` line.

## Numbers (ix VM tui-r2d, arm64, 16 cpus, 16 GB; ripgrep 15.1.0; 8 cores via taskset)

Interleaved, on a terminal (`script`, 110 columns) so that the live view draws, each side with
a warm daemon of its own, alternated run by run (`bench.sh`, `interleaved.jsonl`):

| ripgrep, wall | main d1497be239 | this branch |
|---|---|---|
| edit (20 each), median | 145.77 ms | 144.07 ms |
| no-op (30 each), median | 33.88 ms | 33.34 ms |

Single captures (`captures.sh`, `*/exits`): cold 42.1 s (main) and 40.3 s (branch), warm
second checkout 5.37 s and 6.56 s. The warm gap is the worker storing the manifest (1.50 s
on main, 2.80 s here), which this renderer-only change does not touch.

## Captures

`before/` is main, `after/` this branch: `cold.txt`, `edit.txt`, `warm.txt` are every 40th frame
of the live region and what stayed on the screen (`render.py` replays the `script` capture),
`edit.piped`, `why.txt` and `status.txt` as printed.
