# A promoted stack slot takes no room in the frame (astral-experiments/blaze#84)

ix VM `astral-ax4`: arm64 Neoverse-V3, 16 vCPU, 16 GB, kernel 7.2.11, the flake's toolchain,
Cranelift backend. `base` is main 7f4e04dcc8. `frames` is ad6ed4ebc2, the same commit plus
the change in `crates/hir-ty-codegen/promote.rs`: a slot whose stores and loads `promote`
replaced with SSA values in full gets size 0, because Cranelift reserves room for every
explicit slot, referenced or not. Ported from astral-experiments/blaze#84 by Charlie Marsh,
Apache-2.0/MIT. The half of #84 that shares slots between scopes was already on main (#561).

## Frame bytes

Every function in the objects that `harness/breakers.py` builds (`breakers`,
`breakers2015`, `breakers2021`, with the std they use), with frame sizes read from the
`objdump -d` prologues by `scripts/frames.py` (`scripts/measure-frames.sh`). Raw output:
`frames/{base,frames}.frames.json`.

| | base | frames |
|---|---|---|
| functions | 10,116 | 10,116 |
| total frame bytes | 783,776 | 480,320 (-38.7%) |
| `frame_of_array::run` | 112 | 16 |
| `static_in_static::nested` | 144 | 48 |
| `if_branch_temporaries::nested` (2021) | 128 | 80 |
| `array_in_tuple_moved_out::pair` | 448 | 352 |
| `const_format_args::pair` | 16 | 0 |
| `branch_frames::pair` | 64 | 32 |
| `branch_frames::nested` | 2,208 | 2,080 |
| `frame_of_array::of_a_call` | 50,128 | 50,048 |

`branch_frames` is #84's own case. It was run from a scratch tree only, and the PR does not
add it to breakers, because rustc's build of it aborts (status -6) on this host, so it can't be
a difftest case.

## ripgrep, bench.py

`harness/bench.py ripgrep --backend cranelift --runs 5`, four rounds: base then frames
for rounds 1 and 2, frames then base for rounds 3 and 4 (`scripts/bench.sh`,
`scripts/bench34.sh`). Raw output: `bench/{base,frames}-{1..4}.{json,log}`. `wrong` is empty
in every run.

| round | binary | cold s | cold load s | warm store s | runtime blaze s | runtime cargo debug s |
|---|---|---|---|---|---|---|
| 1 | base | 12.78 | 5.9 | 7.14 | 2.2469 | 0.3949 |
| 1 | frames | 14.80 | 8.3 | 5.72 | 2.2193 | 0.3807 |
| 2 | base | 14.17 | 5.6 | 7.28 | 2.2492 | 0.3956 |
| 2 | frames | 16.36 | 7.4 | 5.84 | 2.2199 | 0.3821 |
| 3 | frames | 16.93 | 7.6 | 6.31 | 2.2202 | 0.3826 |
| 3 | base | 17.79 | 10.6 | 6.42 | 2.2486 | 0.3826 |
| 4 | frames | 14.97 | 7.1 | 7.51 | 2.2199 | 0.3822 |
| 4 | base | 13.73 | 5.7 | 6.78 | 2.2485 | 0.3822 |

- **Runtime:** the blaze-built `rg` runs in 2.2200 s with the change against 2.2486 s
  without it (-1.3%, medians of 10 runs each round). That holds in all four rounds, and the
  min and max of the two never overlap. In rounds 3 and 4 the cargo debug binary, the same
  file under both, timed the same to 0.1 ms, so the difference is the code blaze emitted.
- **Edits** (body, signature, leaf, generic, struct, newfn) are within 1 ms of each other
  except one base outlier (round 4 body, 130 ms).
- **Cold build:** no conclusion. It ranges over 12.8-17.8 s for base alone, and the spread
  is the `load` step (5.6-10.6 s); compile and translate are the same. The warm-store
  rebuild is mixed too (5.7-7.5 s against 6.4-7.3 s).

A finding unrelated to this change: blaze's `rg` takes 2.25 s for the query that cargo's
debug build answers in 0.39 s and release in 0.04 s.

## Commands

```sh
# on the VM, both binaries built by scripts/build.sh into /root/ax/bin/{base,frames}
bash scripts/measure-frames.sh /root/ax/out/frames-breakers base frames
bash scripts/bench.sh   2026-10-28-astral-frames ripgrep base frames
bash scripts/bench34.sh 2026-10-28-astral-frames ripgrep frames base
```
