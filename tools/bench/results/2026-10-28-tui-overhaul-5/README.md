# Build UI overhaul, piece 3d: `blaze why` lists where the time went, as the live view names it (lane tui)

`blaze why` wrote the steps of the last build as one comma-joined line of about 600
characters. It named the steps differently from the live view and said nothing of the cores.
It now uses `Build::why_steps` in blaze-ui, the same model the terminal draws from. Each step
gets a line in the order it began, named as the steps line names it (with its count), with its
time and the cores the worker kept busy meanwhile. Time spent at no step shows as
`at no step`. The longest chain reads `a → b → c`, as at the end of the live view.

Steps under 10 ms and gaps under 50 ms are left out: they say nothing of where the time went.

Cold ripgrep, `before/why-cold.txt`:

```
the last `blaze build` that compiled anything took 32.8s
  compiled nothing again
  steps: loading workspace 0.09s, building build scripts and proc macros (0/14 steps) 0.00s, compiling the standard library of build scripts 10.6s, building build scripts and proc macros (14/14 steps) 3.83s, loading workspace 0.07s, resolving names (56/56 crates) 0.70s, storing the resolved names 0.03s, compiling ahead 0.00s, compiling the standard library (once, kept in the store) 9.90s, compiling rg 3.70s, checking 74 files (74/74 files) 0.24s, linking rg 0.37s, storing the manifest 2.90s, building build scripts and proc macros (14/14 steps) 0.14s
  cache: cold (the store is empty); 31256 compiled
```

`after/why-cold.txt`:

```
the last `blaze build` that compiled anything took 32.5s
  compiled nothing again
  steps:
    workspace                   0.41s  0.0/8 cores
    std of build scripts        11.8s  4.0/8 cores
    build scripts 14/14 steps   3.76s  2.8/8 cores
    workspace                   0.07s  0.0/8 cores
    names 56/56 crates          0.71s  2.1/8 cores
    store the resolved names    0.03s  2.9/8 cores
    std                         9.27s  3.1/8 cores
    codegen rg                  3.87s  4.9/8 cores
    check 74 files              0.31s  1.9/8 cores
    link rg                     0.11s  2.8/8 cores
    store the manifest          1.84s  1.0/8 cores
    at no step                  0.08s  0.0/8 cores
    build scripts 14/14 steps   0.08s  0.0/8 cores
  cache: cold (the store is empty); 31256 compiled
```

At a glance: neither std reaches half of the 8 cores, and storing the manifest takes 1.8 s
on one core.

## Numbers

Only `blaze why` changed; the build path, the worker and the live view are untouched. The
capture runs (`before/exits`, `after/exits`; ix VM tui-r2d, arm64, 8 cores via taskset) give
cold 37.46 s → 37.41 s, edit 0.262 s → 0.240 s and no-ops 20–25 ms on both sides. These are
single runs: the interleaved benches of pieces 3a and 3c measure the paths that render builds.
