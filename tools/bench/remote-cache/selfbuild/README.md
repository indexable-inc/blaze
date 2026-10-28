# blaze building its own tree: cargo, blaze cold, blaze warm, blaze from the remote

What it measures, for blaze's repository at a main commit and three pull request shapes (`a` no
edit, `b` one function body, `body.patch`: `Symbol::intern` in `intern`, which most of the fork
reaches; `c` one signature, `sig.patch`: `Symbol::empty` and its three callers), the build job
(`build -p blaze-engine --release --features llvm`) and the test job (the unit tests `harness/unittests.py`
runs) of the merge gate:

| arm | what |
|---|---|
| `cargo` | cargo with main's target directory, as CI's cache gives it |
| `cold` | `blaze` with an empty store |
| `warm` | `blaze` with a copy of the store main's cold run left: the most a remote can give |
| `remote` | `blaze` with an empty store that `blaze-engine cache pull` fills from the gateway first, the pull counted in the time |
| `fill` | `blaze` on a store kept across runs: a run the machine runs out of memory for keeps the packs it sealed, so repeating it ends with main's store (then pushed, as cold `a 1` is) |

Each blaze run is a fresh CI job: a fresh copy of the tree at `/root/rb/b`, an empty target
directory, a new `BLAZE_HOME`, `BLAZE_STATS=1` and a trace. Results are rows of
`/root/rb/results.tsv`; the measured ones are in `../selfbuild.tsv`.

## Running it

In an ix VM (fleet-ops), as root, with the tree of the commit under test cloned at `/root/rb/src`,
these files and the two patches in `/root/rb`, and the gateway's token, signing key and trust in
`/root/.rcache-{token,sign,pub}` (mode 600, never printed):

1. `bash step0.sh`: cargo builds the blaze to measure with (into `/root/rb/bin`) and warms the
   cargo arm's target directory (`/root/rb/c`) and test binaries.
2. `bash driver.sh "cold a 1 build" "cold a 1 test" "cargo a 1 build" ...`: one systemd unit per
   run, so its cgroup's `memory.peak` is that run's, pinned to 4 CPUs (a GitHub runner's) and
   capped at 13 GB. Page cache is dropped before each run. Alternate the arms.
3. Where a cold run does not fit: `bash fill.sh build test` instead of the cold `a 1` runs, then
   the matrix in `specs.txt`.
4. `bash xfer.sh STORE PROJECT ROUNDS`: push one store and pull it into empty ones, timed.

`/root/rb/bench.env` (`KEY=VALUE` lines) is added to every blaze arm's environment.

## Machines, and what did not fit (2026-10-27)

- Never raise `vm.swappiness` in these VMs: every VM that swapped (zram or a swap file) was
  wedged or killed by its host within minutes.
- A cold blaze build of blaze does not fit 16 GB: the worker loads (158 s, nearly all of it the
  build scripts), then compiling 131,167 roots takes it from 4 to 15 GB in 90 s and the kernel kills
  it, with `BLAZE_LRU` lowered and no spare worker too. Where it finished (32 GB) its peak was
  28.3 GB. The merge gate's runners (`ubuntu-24.04`, 16 GB) meet the same limit on a cold store.
- x86_64 VMs and committed arm64 VMs above 16 GB could not be placed that day (ix capacity), and
  the 16 GB VM that could was killed by its host at about 8 GB touched, so the warm and remote arms
  wait for main's store to be made on a larger machine.
