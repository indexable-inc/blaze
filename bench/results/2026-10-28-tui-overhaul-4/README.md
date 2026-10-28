# Build UI overhaul, piece 3c: a function count that adds up, and a `Took` line that fits (lane tui)

Two misleading things found in piece 2's and 3a's captures of a cold ripgrep build:

- **The now-line could read more functions done than there are:** `Compiling rg 3.05s ·
  31268/5380 fns`. A crate whose total is not known yet (the standard library, mid-codegen)
  counted in the done and not in the total. Earlier in the same build the line said no count
  at all, because every known total was still 0. Now, while any crate's total is unknown, the
  line counts what is done: `Compiling rg 1.01s · 7592 fns`.
- **The end's `Took` line was ~200 characters and wrapped on a 110-column terminal.** It now
  names each step as the steps line above it does, and says "cores" once:

```
before:  Took 34.2s: compiling the standard library of build scripts 11.0s (4.4/8 cores), compiling the standard library (once, kept in the store) 9.57s (3.0/8 cores), building build scripts and proc macros 4.82s (2.2/8 cores)
after:   Took 34.6s: std of build scripts 11.2s (4.3/8 cores), std 9.36s (3.2/8), build scripts 5.08s (2.1/8)
```

`before/` and `after/` hold the frames of a cold build, an edit and a warm build on a
110-column terminal (`captures.sh`, rendered by `render.py`), plus `blaze why`.

## Numbers (ix VM tui-r2d, arm64, 16 cpus; 8 cores via taskset)

Piece 3a (d8a57135) and this branch alternated run by run on a terminal (`script`), each with
a warm daemon of its own (`bench-terminal.sh` → `terminal.jsonl`):

| ripgrep, median wall | piece 3a | this branch |
|---|---|---|
| no-op on a terminal (30 each) | 35.19 ms | 35.38 ms |
| edit on a terminal (20 each) | 152.44 ms | 152.21 ms |

The worker is unchanged. The client looks at each crate once more per frame, which is within
the noise.
