# Build UI overhaul, piece 3b: the functions slow to compile, under their crate and at the end (lane tui)

Andrew (11:24Z): under each crate in flight, show the functions compiling longest
(`crate::path::fn 1.2s`, the top 3 over ~200 ms). The final summary lists the 5 slowest, and
`blaze why` lists them all.

- **`hir-ty-codegen/flight.rs`** (new): a guard beside the counters in `compiled_root` and
  `compiled_instance`. It is off unless the engine says someone hears the build
  (`flight::begin(heard)`). When on, it keeps each compile under way (its crate, its name as
  rustc spells it, since when) and the finished ones that took 200 ms or more (at most 256).
  Lane web's `watch.rs` and its timers are untouched.
- **Protocol** (appended at the end of `Event`): `Compiling { functions: Vec<Slow> }` and
  `Slowest { functions: Vec<Slow> }`, with `Slow { krate, name, ns }` and `SLOW_NS` = 200 ms.
  The engine's codegen counting thread, every 100 ms, says the functions compiling for 200 ms
  or more, up to 3 per crate and 24 in all. It says them only when which ones they are changes:
  each carries how long it had been compiling as of its record, and the view counts on from
  there. The engine says `Slowest` as the build ends. Golden bytes are pinned.
- **The view**: under each crate under way, its functions compiling longest, e.g.
  `│    wgpu::Device::new 1.30s` (tree and list), then
  `Slowest fns: <regex_syntax::unicode::SimpleCaseFolder>::new 0.21s` at the end (five, each
  name cut from the front to 48 characters so its end stays). `blaze why` gets a
  `slowest functions:` list of all of them.

On ripgrep, one function takes 200 ms or more (`after/why-cold.txt`, `after/cold.txt`):

```
        Took 31.6s: std of build scripts 12.1s (4.0/8 cores), std 8.98s (3.3/8), build scripts 4.01s (2.6/8)
     Slowest fns: <regex_syntax::unicode::SimpleCaseFolder>::new 0.21s
```

No function of ripgrep's crates compiles for 200 ms in codegen, so its live tree shows none.
The test `the_functions_slow_to_compile_show_under_their_crate_and_at_the_end` pins how they
show (anim's wgpu, as in Andrew's build).

## Numbers (ix VM tui-r2d, arm64, 16 cpus; 8 cores via taskset; ripgrep 15.1.0)

Piece 3d (c4308e2622) and this branch alternated run by run, each with a fresh home and store
for a cold build, and with a warm daemon of its own for the rest:

| ripgrep, median | piece 3d | this branch |
|---|---|---|
| cold build, worker's time (3 each, `bench-cold.sh` → `cold.jsonl`) | 32.03 s | 31.89 s |
| cold: codegen steps | 3.67 s | 3.38 s |
| cold: standard library steps | 20.06 s | 19.63 s |
| no-op, worker's time (30 each, `bench-events.sh` → `events.jsonl`) | 1.674 ms | 1.679 ms |
| no-op, worker's time, again (`events2.jsonl`) | 1.646 ms | 1.680 ms |
| edit, worker's time (20 each) | 110.04 ms | 111.74 ms |
| edit, worker's time, again | 110.71 ms | 110.26 ms |
| no-op, piped wall (30 each, `bench-piped.sh` → `piped.jsonl`) | 20.04 ms | 20.21 ms |
| edit, piped wall (20 each) | 130.60 ms | 129.71 ms |

The cold builds vary by 4 s from run to run on both sides. Naming each compile at its start
costs nothing that shows. The edit moved +1.7 ms in the first run and −0.45 ms in the second:
noise. The no-op's worker time moved 0.005 ms and 0.034 ms, under the 0.2 ms budget (a no-op
compiles nothing).
