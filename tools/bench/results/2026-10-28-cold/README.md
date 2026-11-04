# Cold builds as the speed gate runs them: where the scripts phase goes

`blaze build` from an empty store with R2 reachable, the store a real directory under `/tmp`
(`BLAZE_CACHE=/tmp/...`, as `promote.py speed` has it with `WORK=/tmp/gate`), cranelift. ripgrep
and Andrew's anim case. Each run in a systemd
unit of its own, the store and target emptied before each, arms alternating.

ix VMs, arm64 Neoverse-V3: `coldfast-build` 16 vCPU, `coldfast-b4` 4 vCPU. The root file system
of an ix VM is a network volume: an fsync of a pack takes 0.6-0.9 s there, and pulling the 39 MB
of the standard library takes 0.25 s into tmpfs and 0.9-1.4 s into `/root`, so runs with the
store on disk (`abdisk`, `ab-4vcpu.txt`) overstate the store's writes against a runner's SSD.

The labels are experiment builds of main with a line per scheduler step (`TSTEP`, the start and
end of each step from the build's start); each adds to the one before:

| label | what |
|---|---|
| tstep | main (after #1273) |
| tstep2 | + #1280: the release's standard library fetched before the workspace loads, and the scripts' standard library waits for it |
| tstep3 | + #1283: the store readable in the build-script sandbox |
| tstep4 | + the scheduler's slots at cores instead of cores / 4 |
| tstep5 | + no pack hashing (`Store::checked` true, no `check_packs`); an upper bound only, not sound |

## Results (wall, seconds)

| change | ripgrep before | ripgrep after | anim before | anim after |
|---|---|---|---|---|
| #1280, 16 vCPU, store in `/tmp` | 10.80 / 11.16 / 11.95 | 10.10 / 10.54 / 10.30 | 35.95 / 36.07 | 32.99 / 33.86 |
| #1280, 16 vCPU, store on disk | 13.90 / 12.84 / 14.16 / 11.26 | 10.61 / 12.37 / 13.84 / 11.90 | | |
| #1280, 4 vCPU, store on disk | | | 64.08 / 63.57 | 62.32 / 62.29 |
| #1283, 16 vCPU | 9.98 / 9.91 / 10.90 | 8.23 / 9.12 / 9.12 | 33.26 / 35.62 | 30.23 / 30.91 |
| slots = cores (not landed) | 7.54 / 8.64 / 8.64 | 8.78 / 10.09 / 9.38 | 31.07 / 29.48 | 31.64 / 28.79 |
| no pack hashing (not landed) | 8.29 / 8.58 / 8.23 | 7.41 / 8.46 / 8.73 | 31.75 / 28.90 | 30.42 / 31.16 |

cargo on the same VM: ripgrep 4.22 s.

- #1280: before it, each build-script build's standard library came from the store only if
  the release's had been pulled by then; it was pulled after the workspace loaded, so the
  scripts' standard library was compiled (2.35 s, "compiling the standard library of build
  scripts"). After: 0.83-0.99 s, the time of the pull.
- #1283: the sandbox mounts an empty `/tmp`, so a store under `/tmp` was not there for the blaze a
  script runs as its `rustc` (anyhow's, num-traits' and rustix's probes): it made the standard
  library's frozen maps again on one thread. anyhow's run 3.4 s to 0.8 s, num-traits' and
  rustix's 6.3 and 6.1 s to 1.3 s each. On the runners the gate's store is under `/tmp`.
- More slots are not faster: the steps are not short of slots but wait on each other (below).
- Hashing packs (sha256 of each pack the first time a process reads from it, and the check of all
  packs each process starts) is 6.4% of all samples of the scripts phase (`perf -a`,
  `perf-ripgrep-scripts-phase.txt`) but within the noise of wall time: the machine is 75% idle
  then.

## Where ripgrep's 8 s goes (after both)

Loading the workspace 4.4 s, of which the build scripts 4.25 s: a first batch of script compiles
2.0 s (with the scripts' standard library, 0.9 s), a second of 3 compiles at 1.3-1.45 s, the runs
of anyhow's (0.75 s) and serde_core's (0.8 s) scripts; then compiling functions 2.3 s and
resolving 0.54 s. A script compile is a process of its own, about 0.4 s of it outside its logged
phases, its link 0.3-1.3 s.

## anim's scripts phase (`steps-16vcpu.txt`)

Two things set it. Linking document-features' proc macro takes 6.3 s (1.97 s to 8.28 s); and the
ready queue is first in, first out: proc-macro2's run (3.4 s) starts at 8.95 s, behind compiles
queued before it, and everything that needs proc-macro2 waits for it. Ordering ready steps by
the length of the path behind them is the scheduler's (sansio lane's) to do.

## Files

- `ab-16vcpu.txt`, `ab-4vcpu.txt`: each run's wall time and blaze's own summary line.
- `steps-16vcpu.txt`, `steps-4vcpu.txt`: the scripts phase step by step (start, end, length).
- `perf-ripgrep-scripts-phase.txt`: `perf record -a -g` over ripgrep's scripts phase, by
  command, by object and by symbol.
