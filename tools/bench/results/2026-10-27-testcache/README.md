# Test passes from the store on a fresh runner (lane testcache, #647)

What `BLAZE_TESTS=affected` does on the shape of a CI test job: a fresh target directory and a
fresh daemon, with a store that main's run filled (as `blaze-engine cache pull` gives a runner).

ix VM, arm64 Neoverse-V3, 16 vCPU, 16 GiB committed, cranelift, nightly-2026-10-25, main
2b97b978ed (#647 merged). Each step one `blaze test --no-fail-fast` with the project's
`blaze_cli.py` test arguments, timed whole (the build from the store and the tests), in a
systemd unit with `MemoryMax=12G`. `BLAZE_MANIFEST=off` throughout: with the build manifest
(#550) a fresh target with a warm store fails to build fd and ripgrep (`.md` inputs parsed as
Rust; reported to lane warmstate). The failing tests are the projects' own on this machine (the
test executable is not in `target/debug/deps`, #595, and the same fail outside the sandbox); a
failure is never kept, so they run every time.

Zero sandbox denials across all of it (about 2,560 tests): hermetic by default broke none of
these projects' tests.

## A one-function PR (`pr-shape.json`)

main: an empty store, `affected`, which keeps the passes. Then bench.py's `leaf` edit of each
project with `std::hint::black_box(1u32);` as the line it adds (bench.py's own `let _bench_edit
= 1u32;` compiles to the same code, and every pass stood after it), and three runs each on a
fresh target and daemon with main's store: `all` (no results, every test), `affected` (results
from main), `verify` (every test, and each pass kept checked).

| project | tests (pass / fail) | main, cold | PR, `all` | PR, `affected`: ran / cached | PR, `verify` |
|---|---|---|---|---|---|
| ripgrep | 123 / 320 | 16.2 s | 13.2 s | 18.9 s: 320 / 123 | 123 of 123 passed again |
| fd | 138 / 99 | 57.0 s | 43.0 s | 47.3 s: 99 / 138 | 138 of 138 passed again |
| just | 515 / 928 | 55.1 s | 32.8 s | 44.3 s: 1272 / 171 | 515 of 515 passed again |
| hyperfine | 51 / 54 | 52.7 s | 23.0 s | 36.8 s: 62 / 43 | 51 of 51 passed again |
| axum | 6 / 0 | 22.6 s | 13.1 s | 20.6 s: 6 / 0 | 6 of 6 passed again |
| cargo | 8 / 0 | 7.1 s | 4.8 s | 5.2 s: 0 / 8 | 8 of 8 passed again |
| nushell | 67 / 0 | 27.4 s | 17.2 s | 21.3 s: 42 / 25 | 67 of 67 passed again |

`verify` is clean everywhere: no cached pass failed when run.

The PR's `affected` run is slower than its `all` run on every project. Timed apart
(`split.json`, ABAB): axum's build under `affected` is 21.3 and 18.2 s against 15.6 and 11.5 s
under `all`, its tests 0.5 s either way; nushell's build 7.9 and 5.0 s against 7.2 and 4.3 s, its
tests 12.0 and 9.0 s against 14.3 and 10.2 s. The build's extra time is the reach hash the daemon
writes beside each test program (`tests-reach`, `hir_ty_codegen::tests_reach`): with
`BLAZE_STATS=1`, axum's two programs take 8.5 s and 3.6 s (`tests-reach-us`). It walks the MIR
of everything each test reaches, which a build from a warm store does not otherwise lower. On
these projects the tests themselves are short, so the walk costs more than the tests it skips.
That walk is what to make cheaper next.

## No edit: main's passes on a fresh runner (`fresh-runner.json`)

Cold (an empty store), then a fresh target and daemon with that store, both `affected`; before
the PR-shape runs, on the branch's tree. Every test that passed on the first run came from the
store on the second.

| project | tests (pass / fail) | cold | fresh runner: ran / cached |
|---|---|---|---|
| ripgrep | 123 / 320 | 59.0 s | 16.9 s: 320 / 123 |
| fd | 138 / 99 | 53.7 s | 50.5 s: 99 / 138 |
| just | 515 / 928 | 51.6 s | 36.4 s: 928 / 515 |
| hyperfine | 51 / 54 | 40.7 s | 24.3 s: 54 / 51 |
| tokei | 228 / 0 | 103.2 s | 70.6 s: 0 / 228 |
| axum | 6 / 0 | 26.9 s | 19.6 s: 0 / 6 |
| cargo | 8 / 0 | 8.4 s | 6.2 s: 0 / 8 |
| rust-analyzer | 7 / 0 | 6.5 s | 4.5 s: 0 / 7 |
| nushell | 67 / 0 | 28.8 s | 15.3 s: 0 / 67 |

The scripts that made these (run in the VM from blaze's checkout, `nix develop -c python3 ...`)
are `survey.py`, `survey2.py` and `split.py` here.

## The walk by stable names (`ab.json`, `ab.py`)

The same one-function PR, before and after the walk hashes a dependency's function by its stable
name and keeps what is below it in the store: two builds of one tree, differing in that change
alone. Each project has a store of its own that main's run fills; every PR run starts from a copy
of it, with a fresh target, daemon and `BLAZE_HOME`, `all` and `affected` in turn, twice. The
walk is `tests-reach-us` from the daemon's log, summed over the test programs.

| project | ran / cached | `all` | `affected` before | after | walk before | after |
|---|---|---|---|---|---|---|
| axum | 6 / 0 | 11.5 to 12.2 s | 19.1, 18.9 s | 13.3, 13.7 s | 6.25, 6.14 s | 0.94, 0.94 s |
| just | 1272 / 171 | 28.2 to 29.5 s | 38.8, 38.9 s | 32.4, 32.1 s | 7.52, 7.70 s | 0.46, 0.44 s |
| nushell | 42 / 25 | 17.2 to 18.9 s | 17.3, 19.4 s | 18.9, 17.0 s | 0.52, 0.53 s | 0.57, 0.57 s |

Which tests ran and which came from main is the same before and after. Main's run, which walks
everything once and keeps what is below each name, walks as long as before (axum 2.4 and 2.6 s).
`affected` still costs a little more than `all` here: just's 3.5 s is not the walk, which is
0.45 s now. Where it goes is not measured; the sandbox `affected` runs each test in is the first
thing to time. With a
store of its own, nushell's `affected` is as fast as `all`: the 4 s it lost in the table above
came from the shared store that every project's runs kept growing, which every fresh daemon
checks the packs of as it opens it (#685 makes that cheaper).
