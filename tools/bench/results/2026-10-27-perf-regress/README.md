# ripgrep bench regression, 2026-10-27 (lane perf)

The train job `linux x64 / bench (ripgrep)` regressed against baseline 42cb85270f:

| row | baseline | train |
|---|---|---|
| no-op | 0.027 s | 0.13 s |
| body edit | 0.28 s | 1.4-1.8 s |
| cold (llvm) | 24 s | 89 s |
| peak memory | | 3.2 GB |

## Machine

Everything here ran on ix VMs `perf5` and `perf6`: arm64 (Neoverse), 8 vCPU, 16 GiB committed. Every run was pinned to cores 0-3 (`taskset -c 0-3`) and used the cranelift backend unless a row says otherwise. x86_64 could not be placed (`vm.placement.memory_full`). The binaries are either CI's `blaze-linux-arm64` artifacts (`bin/<sha10>`) or release builds from source without llvm (`scripts/buildsrc.sh`; `base`, `noopfix`, `stdkey`). The CI train numbers are x86-64.

## What regressed, and where

| row | first bad | fix | before | after |
|---|---|---|---|---|
| body/sig edit (`roots`) | #441 238b9e9dab (`compile_time_only` filter per crate listing) | #696 (operator) | 82db94d173 body 1.906 s | 58c3e559a0 body 0.236 s |
| no-op | #408 da138e119c ("build is the whole build", so every build reaches `link::stand_ins`, which read the whole 155 MB executable) | #800 (mmap) | 56 ms | 35 ms |
| cold | #775: the store was keyed by the first toolchain a build script named, so std from `blaze-engine std` was never found and was compiled again in the daemon | #801 | 39.0 s, peak 5.4 GB | 20.2 s, peak 2.9 GB |
| cold load | #450 9bb16676c5 and onwards (no-rustc: build scripts built and probes answered by blaze child processes, each loading the sysroot again) | not fixed: see below | load 0.7 s (#441) | 5.4 s (#450), 9.4 s (main) |
| cold llvm | BLAZE_OPT=2 runs the thinlto O2 pipelines on every dependency function (31,885 functions, 145 s of LLVM CPU on CI); the baseline took dependencies from rustc | not fixed: needs a design decision | | |

The no-op range was measured per revision (`noop/<rev>/times.txt`): 0b48278e48 34 ms, 32ccb28b37 32 ms, da138e119c 66-99 ms, 58c3e559a0 53-55 ms.

## Files

- `res/<rev|tag>-cranelift.json` is `harness/bench.py ripgrep` output. `scripts/summ.py res/*.json` prints one line per run.
- `abnoop-{base,noopfix}-{1,2}.txt` hold three rounds of 30 no-ops each, alternating the two binaries.
- `noop/<rev>/` has a no-op client strace (`noop.strace`), syscall counts and timings.
- `cold/58c3e559a0/` has the client strace of a cold build, with the blaze child processes and their times (strace-inflated).
- `stdcheck/{base,stdkey}/` has worker logs of a cold build with and without #801 ("standard library: from the store").
- `mem/<rev>/` holds the `BLAZE_MEMORY` salsa tables (`tables/*.tsv`), RSS samples of every blaze process, and worker logs. `58c3e559a0` is main and `b6095a7b47` is main plus #801.
- `prof/`, `prof3/` and `noopperf/` hold earlier worker logs and perf reports from edits and no-ops.
- `scripts/` has the VM job scripts that produced all of this. Each job runs as a transient systemd unit through `job.sh` and `q.sh`.

## Memory (cold ripgrep, worker)

| | after load | after resolve | after build | after warm-up |
|---|---|---|---|---|
| main 58c3e559a0 (resident / live MB) | 282 / 91 | 1461 / 1239 | 3772 / 3270 | 3635 / 2814 |
| with #801 (resident / live MB) | 266 / 91 | 654 / 444 | 2511 / 2012 | 2198 / 1608 |

With #801, salsa's own tables account for 389 MB after the build:

| area | MB |
|---|---|
| codegen queries | 130 |
| bodies and syntax | 67 |
| name resolution | 58 |
| inference | 38 |
| MIR | 19 |
| other | 78 |

That leaves about 1.6 GB live outside what salsa counts. During the cold load, each build-script child holds 0.4-0.6 GB and anyhow's probe (`as_rustc`) holds 0.73 GB.
