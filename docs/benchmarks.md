# Benchmarks

The README's benchmark tables come from here. Each is a matrix over three platforms, macOS
arm64, Linux aarch64 and Linux x86-64, each measured on a dedicated machine of its own, on real
projects at pinned commits, against plain cargo on the same machine, and written back to the
README by a bot commit. A platform's numbers compare with each other and with its own history,
not with another platform's: the machines differ in more than their architecture. This page
says how, so that anyone can check it or run it again.

## What is measured

`harness/bench.py` copies each project's pinned checkout and builds it two ways side by side:
`cargo ra-jit build` (the daemon) and plain `cargo build` into a target directory of its own.
Both build the same source after every edit, one after the other, so they see the same machine
state.

| Phase | What happens | ra-jit | cargo |
|---|---|---|---|
| Cold | build from an empty target directory and an empty ra-jit store; crates are already downloaded | the build returns (cargo builds the first time) and then the daemon loads the workspace in the background; both are reported | `cargo build` |
| Cold, warm store | after everything else, `--cold-runs` times: a second copy of the project with an empty target directory and a daemon of its own, but the store the measured copy filled with its cold build and edits (as a second checkout of the same project, or a teammate's store, would have) | build and daemon ready, as cold | |
| Nothing changed | build again with no edit | | |
| Edit a function | one statement is added to a function in the main crate | | incremental |
| Edit a widely used function | one statement in a function many places call: in a library crate of the workspace when there is one (ripgrep's `grep-matcher`, axum's `axum-core`, nushell's `nu-path`, bat's library), otherwise in a module the rest of the crate uses (fd, hyperfine, just) | | incremental |
| Change a signature | a method gets another parameter and every caller passes it | | incremental |
| Edit a generic function | a statement in a generic function used at many types (ripgrep and nushell) | | incremental |
| Add a function | a new function in the main crate, called from the function of "Edit a function" | | incremental |
| Add a struct field | a struct gets one more field (`_bench_edit: ()`) and every place that builds it sets it; in a library crate where there is one (nushell's `nu-protocol::Alias`, built in `nu-parser`) | | incremental |
| Tests | (`--tests`) the project's test suite, or the crates of it named in `PROJECTS`, after the edits: `cargo ra-jit test` and `cargo test`, run twice each, the second timed; every test has to end the same way under both | | |
| Runtime | (ripgrep) the program ra-jit linked, cargo's debug build and cargo's release build run the same search over the toolchain's own library source | | |

The edits do not change what the program does. After the first edit of each kind the program
ra-jit linked is run and has to print what cargo's prints (exit status, stdout and stderr); in
the runtime phase all three programs have to print the same. A run in which any program differs
is not published (`benchreport.py` refuses it and the workflow fails).

Each edit is made and taken back `--runs` times (5 in CI). The README shows the median of the
edits; this page also shows the fastest and slowest run of each side, and the raw JSON keeps
every sample, the time of taking each edit back, who built each step (the daemon, or cargo and
why), whether the daemon spliced the change into the program or linked it again, how many
functions it compiled, the memory of both sides (below) and the daemon's own timeline of each
build.
Cold is `--cold-runs` runs of each (5 in CI), each with a new target directory, home and store;
the store size after it is kept too.

An edit the daemon hands to cargo (a fallback: ra-jit always builds what cargo would) is measured
once, marked `(c)` in the README, and shows about cargo's time.

## Memory and disk

Every build in the table is also measured for memory, on both sides, and so is the disk each
side leaves behind. The README shows a summary under the time table; this page has all of it.

| Number | ra-jit | cargo |
|---|---|---|
| Peak, each phase and edit | the most `cargo ra-jit build`, the daemon and every process under either held at once: cargo when the client hands it the build, its rustc, build scripts and linker, the daemon's workers and what they run | the most `cargo build`, rustc, build scripts and the linker held at once |
| Largest process | the peak of the largest of those processes on its own | the same |
| Daemon between edits | what the daemon and its workers hold once it is ready, after nothing changed, after each kind of edit and at the end of the run, with the number of builds so far: memory that grows with every edit shows here | |
| On disk | the target directory, the store and ra-jit's home | the target directory |
| Cold, warm store | the peak, and what the daemon holds once ready | |
| Runtime | the peak of the program ra-jit linked while it runs | the peak of cargo's debug and release programs |

`harness/runbench.py` (runtime parity) adds the peak of each program while it runs, the size of
each binary, and the peak of each build: `cargo build --release` against ra-jit's whole-program
LLVM build.

**How.** One helper, `harness/memory.py`, measures all of it, and every harness that reports
memory uses it. It combines two readings:

- **The exact peak of one process.** When the harness waits for a command (`wait4`), the
  kernel reports the largest resident size any single process of what it waited for reached
  (`ru_maxrss`; KiB on Linux, bytes on macOS). This is exact, but it is one process: cargo's 16
  rustc at once count as the largest of them.
- **The sum over a tree, sampled.** A thread reads what every process under the command, and
  under the daemon, holds every 10 ms (`RA_JIT_MEMORY_INTERVAL`) and keeps the largest sum. On
  Linux that is the resident size, from `/proc/PID/statm`, with the children of each thread in
  `/proc/PID/task/*/children`. On macOS it is the physical footprint, from
  `proc_pid_rusage`, with `proc_listchildpids`: what Activity Monitor shows, the memory a
  process owns, still counted when macOS has compressed it or swapped it out. Resident size
  would not do on a Mac: when memory is short macOS compresses the pages of a process that is
  idle, and the daemon between two edits read 119 MB on nushell on a Mac whose memory was full,
  where it held 7 GB a minute before. The kernel also keeps each process's peak footprint,
  which the sampler reads as the largest of that process. Each process it saw stays watched,
  so the daemon, which the first build starts and which then outlives it, is followed with its
  workers.

The peak reported is the sum, and never less than the peak of the largest process. What the
daemon holds between edits is one reading of its tree when it is idle.

**Error.** The sum can miss a rise shorter than 10 ms and a process that lives less than
10 ms (a short `cc` or build script): it reads low by at most what such a process held. On
Linux it can also read high: a page of a shared library that several processes have mapped
counts once in each (resident size, not proportional size). That is mostly libLLVM and
`librustc_driver`, so it inflates cargo's peak with many rustc at once more than ra-jit's,
whose daemon is one process with a few workers; proportional size costs a walk of every
mapping per reading. The footprint macOS counts leaves those pages out. A Linux number and a
macOS number are therefore not the same count; within one machine both sides are.

**Cost.** The sampling thread wakes every 10 ms and reads a few dozen small files or calls;
while it runs, Python switches threads every 0.1 ms instead of 5 ms, so the end of a timed
command is seen within that. On the MacBook of the macOS results, a no-op `cargo build` of ra-jit itself took a
median 322 ms without it and 319 ms with it (30 runs each, taking turns): no cost the noise
shows. Two runs of the Mac's measurement gave the same peaks within 1%.

**Disk** is the blocks the files use, a file with several hard links once
(`memory.disk_mb`). On Linux each block is counted once however many files share it: a
copy the file system made by sharing the blocks (a reflink, which `std::fs::copy` makes on
XFS and Btrfs, and which ra-jit's copies of rustc's objects and libraries are) takes no
more disk than the one file, and is counted so, where `du` counts it once per file. The
kernel says which blocks are shared (`FIEMAP`); where it does not, a file is counted as
`du` counts it. On macOS a clone is counted as `du` counts it, once per file.

The history (`rss_mb`) still has the daemon's workers' high-water marks summed, as before; the
history's memory alerts check the new numbers too (below).

Each platform's memory tables are with the rest of its numbers, under [Results](#results).

## Projects

Pinned by tag and commit in `harness/bench.py` (`PROJECTS`); the clone has to be at that commit
or the run stops.

| Project | Tag | Commit | Run to check the output |
|---|---|---|---|
| ripgrep | 15.1.0 | `af60c2de9d85` | `rg -n --no-heading --sort path "fn main" ...` |
| fd | v10.3.0 | `d38148f0aabd` | `fd --extension rs main src` |
| bat | v0.26.0 | `c734087e1d15` | `bat --plain --color=never rustfmt.toml` |
| hyperfine | v1.19.0 | `12fec4209864` | `hyperfine --help` |
| just | 1.43.0 | `b7e7c46495b3` | `just --list --unsorted` |
| axum | axum-v0.8.6 | `c1bb9c3e9062` | built (`-p example-hello-world`), not run: it is a server |
| nushell | 0.108.0 | `da141be11b3a` | `nu -n -c "[3 1 2] \| sort \| str join ','"` |
| helix | 25.07.1 | `a05c151bb6e8` | `hx --version` (grammars are not built: `HELIX_DISABLE_AUTO_GRAMMAR_BUILD=1`) |
| bevy | v0.20.0 | `b3bdd75becaf` | built (`--example breakout`), not run: it opens a window |
| zed | v1.24.4 | `aed37547d854` | `zed --help`; not in the CI run, see below |

zed (1.7 million lines in 249 crates, 1,291 crates with its dependencies) is in `PROJECTS` and
every edit of it builds, but it is not measured yet: ra-jit's daemon does not fit it in 123 GB.
On a c8i.16xlarge (2026-10-21, ra-jit `2b2f2dcd5b`) the daemon loaded the workspace (20 s),
its 1,756,350 symbols (26 s) and resolved the crates (25 s), then went from 20 GB to more
than 111 GB in 40 seconds and was stopped at a 112 GB limit before it was ready; cargo alone
builds it in a few minutes on the same machine. Its build needs `nix develop .#bench-zed`.

`cargo` 0.92.0 is in `PROJECTS` too but not in the CI run (it doubles the run's length).

## Fairness

- Same machine, same source, same moment: the two builds alternate after every edit.
- Same toolchain on both sides: Rust 1.99.0 (`harness/toolchain`), from the flake's
  `devShells.bench`, which also pins LLVM 20 for the LLVM tier.
- Cargo is warm and incremental: the dev profile's default (`CARGO_INCREMENTAL` unset).
- Same linker: the toolchain's default (`cc`, which runs GNU ld on the benchmark machine). No mold,
  no lld, no sccache or other `RUSTC_WRAPPER` on either side (bench.py removes it).
- Crates are downloaded before the cold build on both sides; downloading is not timed.
- ra-jit's store starts empty for each project and backend and for every cold run. "Cold, warm
  store" is the one phase that starts with a store, and it is reported apart. On a machine
  the store is one for every project, so the Store column is what a project adds to a store
  that has nothing of it. `store_added_mb` in the results is what it adds to one that has
  what the projects before it in the run stored: the disk of its entries whose keys none of
  them had (a pack's size shared among its entries by the length of their values).
- The machine runs nothing else: it is a dedicated instance used only by this workflow.

## Tiers

Each project is measured with each code producer ra-jit has: Cranelift (the default, in the
README) and LLVM (`RA_JIT_BACKEND=llvm RA_JIT_OPT=2 RA_JIT_EMIT=fast`). Cranelift is measured
with ra-jit as it is installed and released, without the `llvm` feature (`RA_JIT_BIN`), and LLVM
with a build that has it (`RA_JIT_LLVM_BIN`): a binary that links libLLVM loads it each time it
starts, about 6 ms on the benchmark machine, and every `cargo ra-jit` starts it once. A new tier
is one more entry in `BACKENDS` in `harness/bench.py`; the report makes a table for every
backend in the result.

## The fastest setups

With `--setups` (all of them in the nightly run, `--setups-on ripgrep,nushell`), every step plain
cargo builds is built again by each of the fastest ways to build Rust there already are, each
into a target directory of its own, right after plain cargo, on the same source:

| Setup | What it is |
|---|---|
| mold | cargo with mold 3.0 as the linker (`-C link-arg=-fuse-ld=mold`) |
| wild | cargo with wild 0.10 as the linker (`-C link-arg=-fuse-ld=wild`) |
| `-Zthreads=8` | rustc's parallel front end, on the same 1.99.0 (`RUSTC_BOOTSTRAP=1`) |
| nightly | plain cargo on the nightly below, to tell the nightly's own difference from Cranelift's |
| cg_clif | rustc_codegen_cranelift (`-Zcodegen-backend=cranelift`), on the nightly: it ships with nightlies only |
| cg_clif + wild + `-Zthreads=8` | all three at once, the fastest cargo build there is |
| `cargo check` | builds no code, so there is nothing to run after it: the floor of what rustc does |

The flags go in through `--config target.'cfg(all())'.rustflags`, which cargo adds to each
project's own (helix sets `--cfg tokio_unstable` there; `RUSTFLAGS` would replace it).
`-Zshare-generics` is not a column: it is already on in every debug build. The nightly is
`rust-bin.nightly."2026-10-09"` with `rustc-codegen-cranelift-preview`, from the flake
(`RA_JIT_NIGHTLY`). A setup that cannot build a project is reported as failed and not timed
after that.

Hot-patching a running program is compared on its own (`harness/hotpatch.py`): a small program
that calls one function every millisecond through `subsecond::call`, edited 30 times by each of
Dioxus's `dx serve --hotpatch` (subsecond, which patches the running program on Linux),
`cargo ra-jit build` and incremental `cargo build`, the last two followed by starting the program
again (ra-jit patches a running program on macOS only). Each edit is timed from writing the file
to the program running the new code. It is a small program on purpose: subsecond needs the
program to call through `subsecond::call` and to keep running, which a command-line tool like
ripgrep does not. The restart flows stop the program before building, as `cargo watch` does.

On Linux (dev-aws-7, 16 cores of a c7i.16xlarge, the bench shell), 30 edits each, milliseconds
from the save to the new code running ([raw JSON](benchmarks/hotpatch/linux-x86_64.json)):

| Tool | median | p90 | min | max | of which the build |
|---|---:|---:|---:|---:|---:|
| subsecond (`dx serve --hotpatch`), patched, 0 restarts | 289 | 463 | 283 | 535 | |
| `cargo ra-jit build`, then a restart (all 30 built by the daemon and spliced) | **40** | 72 | 39 | 78 | 39 |
| `cargo build`, then a restart | 361 | 413 | 353 | 427 | 360 |

On a program this small, starting it again is nearly free, so ra-jit's build plus a restart
beats subsecond's patch of the running program; a program with state to rebuild after a
restart would lose that. On a MacBook Pro (M5 Max, 18 cores, on AC, other work running: a load
of 7 to 28), the same 30 edits each ([raw JSON](benchmarks/hotpatch/macbook.json)):

| Tool | median | p90 | min | max | of which the build |
|---|---:|---:|---:|---:|---:|
| subsecond (`dx serve --hotpatch`), patched, 0 restarts | 346 | 417 | 320 | 421 | |
| `cargo ra-jit build`, then a restart (all 30 built by the daemon) | 469 | 527 | 410 | 841 | 299 |
| `cargo build`, then a restart | 520 | 593 | 486 | 651 | 365 |

On macOS ra-jit also patches a running program without a restart: a program that links
`ra-jit-live` (`harness/live`, as small as this one) takes an edit in a median of 12.5 ms (p90
28.0, p99 48.9) when it goes through `cargo ra-jit build`, and 4.0 ms (p90 11.6, p99 13.8) when an
editor sends it over the daemon's socket (`harness/live_latency.py --program probe cargo socket`,
30 edits each, same MacBook, [raw JSON](benchmarks/live/macbook-probe-cargo.json)). That is the
flow to set against subsecond's; the restart flow above is what ra-jit does where it cannot
patch, today Linux.

## The machines

Every result records its machine and platform: the name it was given (`RA_JIT_BENCH_MACHINE`),
the CPU, the cores the run could use (it may be pinned with `taskset`) and the machine's total,
memory, OS, kernel, rustc, C compiler, linker, LLVM version and the load average when it
started (below and in the JSON). The platforms, their runners' labels and how a result is
filed are `harness/benchplatforms.py`.

Each platform is meant to run on a dedicated machine that runs nothing else, registered as a
self-hosted GitHub Actions runner. A shared runner is no use: GitHub's hosted macOS arm64 runner
(`macos-15`) is a 3-core VM with 7 GB of memory, less than nushell's daemon holds, with
neighbours that move the numbers. The three:

| Platform | Runner labels | Machine | Why |
|---|---|---|---|
| Linux x86-64 | `self-hosted, linux, x64, bench-x86` | `dev-aws-bench-x86` (i-01666717f0de39201, us-west-2a), resized to m8a.4xlarge: 16 AMD EPYC 9R45 (Zen 5) cores, one thread per core, 64 GiB | already launched for it, on demand, ttl a year, tagged `ix-dev-purpose=ra-jit-bench`; launched as a c8a.4xlarge (32 GiB), too little for nushell's daemon (15 GB) beside cargo |
| Linux aarch64 | `self-hosted, linux, ARM64, bench-arm64` | a new `dev-aws-bench-arm64`, m8g.4xlarge: 16 Graviton4 (Neoverse V2) cores, 64 GiB, on demand | the same core count and memory as the x86 machine; a cloud VM is not bare metal, but its cores are not shared with other tenants |
| macOS arm64 | `self-hosted, macOS, ARM64, bench-macos` | a Mac mini M4 Pro (14 cores, 64 GB) kept for it, plugged in, not sleeping (`sudo pmset -a sleep 0`), or an EC2 mac-m4pro.metal host if it has to be in AWS | bare metal, and the only way to measure macOS: a MacBook in use is busy with other work |

`harness/benchrunner.sh PLATFORM MACHINE` sets a runner up: it checks the platform is the
machine's, installs the runner in `~/actions-runner`, registers it with the platform's labels and
runs it at boot (on Linux as user `andrew` from a user systemd unit,
`ra-jit-bench-runner.service`, lingering; on macOS as the runner's launchd service), and its
`.env` names the machine for the results. It needs Nix with flakes on the machine:

```
scp harness/benchrunner.sh HOST:
gh api -X POST repos/indexable-inc/ra-jit/actions/runners/registration-token --jq .token \
  | ssh HOST bash benchrunner.sh linux-aarch64 "'AWS m8g.4xlarge (dev-aws-bench-arm64)'"
```

Then name the platform in the repository variable `BENCH_RUNNERS`, comma-separated, which turns
its jobs on: `gh variable set BENCH_RUNNERS -R indexable-inc/ra-jit -b linux-x86-64,linux-aarch64,macos-arm64`.
A platform it does not name is skipped by every run rather than left waiting for a runner that
is not there, and the README keeps its last numbers, under their machine, date and commit.
(`BENCH_RUNNER=true`, from before there were platforms, still means Linux x86-64.)

Until the runners exist the numbers are measured by hand with the same harness, pinned to 16
cores with `taskset` so that cargo and the daemon have the 16 cores of the machines above. The
edits: on 2026-10-22, Linux x86-64 on `dev-aws-8` (a c7a.48xlarge, AMD EPYC 9R14, idle and
claimed for the run) and Linux aarch64 on a c8g.16xlarge (Graviton4, Neoverse-V2) launched for
it; macOS arm64's are the MacBook Pro (M5 Max) run of 2026-10-21, made while it was busy with
other work (its load average is in the README), so its times are noisier than the others'. The
runtime, each benchmark run 10 times on 2026-10-23: Linux x86-64 on `dev-aws-11` (a bare-metal
c8a.metal-24xl, AMD EPYC 9R45, idle and claimed for the run) and Linux aarch64 on a c8g.8xlarge
(Graviton4) launched for it. macOS arm64's runtime is one run on GitHub's `macos-15` runner, a
shared virtual M1 with 3 cores and 7 GiB, whose runs of a benchmark have varied by up to 17×
(eza, which reads the disk), so the README marks every benchmark whose runs varied by more than
1.5× with ≈. The first published run (2026-10-20) was on `dev-aws-11` too.

## When it runs and what it commits

`.github/workflows/bench-readme.yaml` runs on every push to main that touches code (not docs,
the site or the README), every night at 09:17 UTC, and by hand (`gh workflow run bench-readme
--repo indexable-inc/ra-jit`, with `-f force=true` to write the README even when nothing moved).
One run at a time; a push during a run waits, and a newer push replaces the waiting one. A run
has a job per registered platform, on its runner, at the same time, and then one job that
publishes them all.

That job hands each platform's result to `harness/benchreport.py`, which compares it with the
stored one of the same platform (`docs/benchmarks/<platform>.json`, e.g. `linux-x86-64.json`;
`runtime-<platform>.json` for the runtime). It replaces the stored result, and writes the
README's tables and this page's results from the newest stored result of every platform, only
when:

- a median of either side, in any phase, moved by more than 10% and more than 50 ms,
- a step the daemon built is now built by cargo, or the other way round,
- a peak of either side moved by more than 10% and 100 MB,
- a project or backend was added or removed, or
- the stored run is more than 24 hours old (so the commit the README names stays within a day
  of main).

It then commits those files to main as `github-actions[bot]` with `[skip ci]` in the message (so
the commit starts no other workflow), once for every platform, rebasing onto main if main moved
during the run. It pushes with the secret `RA_JIT_BOT_TOKEN`, a repository admin's token, which
the ruleset on main lets past its merge queue; the job's own `GITHUB_TOKEN` is refused
([harness.md](harness.md#ci)). `bench-bun.yaml` does the same for bun, nightly, on the same runners: bun takes
hours, so it has a workflow and a stored result of its own (`docs/benchmarks/bun-<platform>.json`
and `runtime-bun-<platform>.json`, written by `benchreport.py --name bun`), but it is a project like
the others in the README's tables and charts and in their geometric means. Each table folds a
platform's bun result into that platform's own (`benchreport.py`'s `stored_all`), whichever
workflow wrote last, and a footnote names bun's machine, commit and date; its every number is
under "macOS arm64, bun" and so on below.

## History and regressions

Every run of the workflow appends one line per platform to
`docs/benchmarks/history/<platform>.jsonl` (`harness/benchhistory.py build`), whether or not the
README changed: the medians, spreads, who built each step, memory and store size of each project
and phase, the platform, the machine and the run's link. The site's
[/bench](https://ra-jit.ix.dev/bench) page charts those files, a platform and machine at a time,
and the workflow starts the site's deploy after it commits.

`benchhistory.py check` then compares the new line with the median of the last seven from the
same machine with the same cores. A number regressed when it is above that median by more than
its noise band and by more than a floor: the band is 10% or three times the spread the history
shows (1.4826 times the median absolute deviation), whichever is larger, and the floors are
50 ms for times, 100 MB for memory and 50 MB for the store and disk. Memory checked is
ra-jit's: its peak in each phase, what the daemon holds after each phase, once ready and at the
end of the run, and its disk; cargo's memory is charted, not checked. A step the daemon built that cargo
builds now is a regression whatever its time. On a regression the workflow opens an issue
labelled `bench-regression` with a table of what moved (or comments on the one already open),
writes the table to the run's summary and fails.

Live edits are checked the same way. `.github/workflows/live-latency.yaml` runs every night on
main on a GitHub macOS arm64 runner (`macos-15`), times socket, burst and replay edits of the
four live programs (`harness/live_latency.py`, [live.md](live.md#latency-under-typing)), appends
one line for all of them to `docs/benchmarks/history/macos-arm64-live.jsonl` and checks the p50
and p90 of each program and mode (floor 1 ms). A shared runner is noisier than the benchmark
machine; the band widens with the spread its own history shows.

## Run it yourself

On any of the platforms with Nix (or with Rust 1.99.0, rust-src, LLVM 20 and Python 3 installed):

```
nix develop .#bench
cargo build -p ra-jit --release
CARGO_TARGET_DIR=target-llvm cargo build -p ra-jit --release --features llvm
python3 harness/bench.py ripgrep fd --runs 5 --out /tmp/bench.json   # any of the projects
python3 harness/benchreport.py /tmp/bench.json --check               # what would change
```

`--backend cranelift` measures one tier. `--cold-runs 5` repeats the cold builds, `--setups
all --setups-on ripgrep,nushell` adds the setups above, `--tests` the test suites, and
`--reference DIR` puts cargo's target directories outside the work directory.
`python3 harness/hotpatch.py --edits 30` times hot-patching (needs `dx`: the bench shell has it). Projects are cloned once into `RA_JIT_PROJECTS`
(default `~/ra-jit-projects`). The whole CI set (seven projects, both tiers, five runs) took 38
minutes on 16 Zen 5 cores, plus the build of ra-jit. It needs about 20 GB of memory (nushell)
and 30 GB of disk.
`python3 harness/bench.py --compare OLD.json NEW.json` lists what got slower or holds more
memory. On macOS give `--work` a short directory (`--work /tmp/bench`): the daemon's socket
path has to fit in 104 bytes, which a work directory under `$TMPDIR` does not leave room for.
`python3 harness/benchreport.py RESULT` publishes a run as its platform's (the machine it ran
on): `RA_JIT_BENCH_MACHINE` names that machine in the tables, and `taskset -c 0-15` before
`python3` gives it the 16 cores of the benchmark machines. `benchreport.py --render` writes
every table and chart again from the stored results.

## Caveats

- Live edits (the patch applied to a running program) work on macOS on Apple silicon only and are
  measured there by `harness/live_latency.py`, per keystroke under typing as well as one edit at
  a time: [live.md](live.md#latency-under-typing) has the numbers from a MacBook, and
  `history/macos-arm64-live.jsonl` the runs.
- One machine, one OS. The numbers on a laptop will differ; the ratios are what carries over.
- Cold is the least precise number here: five runs at most, each minutes long on the large
  projects.
- The setups compared are measured on ripgrep and nushell only (`--setups-on`): seven more
  builds of every step on every project would make the nightly run too long for one machine.
- An edit is a small, fixed change. A larger change (many functions, a new type) costs more;
  `harness/agents.py` makes random edits of the kinds coding agents make.
- axum's output is not checked (it is a server); its build is.
- ripgrep's runtime is one short workload (tens of milliseconds, over 16 threads), so it says more
  about start-up and directory walking than about generated code; the program ra-jit linked has
  had the edits spliced into it and taken back, as a program in use would.
- Cold for `cargo ra-jit build` is cargo's own build (the daemon is not loaded yet), so it matches
  plain cargo; the daemon then loads the workspace in the background ("Daemon ready after").
- The C compiler comes from Nix (gcc 16) with the wrapper's hardening turned off, as a
  distribution's gcc behaves; it is the same for both sides. Nix's linker wrapper runs
  `realpath` for each library directory of the shell on every link cargo makes (ra-jit's links
  do not go through it), so the bench shell holds only what the measured projects need, 72
  directories (a link through `cc` costs 0.23 s); zed's libraries are in `nix develop
  .#bench-zed`. With them in the bench shell (282 directories) every cargo link cost 0.85 s, which
  made cargo and subsecond look 0.6 s slower. A result records the count (`library_dirs`).

## What the 2026-10-21 run found

The results below come from two runs on dev-aws-7 (16 cores of a c7i.16xlarge, ra-jit
`2b2f2dcd5b`): ripgrep to nushell on both tiers in one, bevy and helix on Cranelift with one
cold run each in the second, since helix with its test targets needs more memory than the
machine has. ra-jit loses or fails on these:

- **Doctests.** Without `-q`, `cargo ra-jit test` builds the doctests of every library itself, in
  a process that loads the workspace each time (about 2 s per library), on every run: ripgrep's
  suite takes 27.8 s with nothing changed against 4.3 s under `cargo test`, nushell's 12.4 s
  against 2.5 s. With `-q` cargo hands the doctests to rustdoc and ripgrep's run takes 4.9 s
  against 4.1 s. fd, hyperfine and just, whose libraries have few or no doctests, are faster than
  cargo after an edit and the same with nothing changed. In ripgrep, nushell and axum the edited
  library is one whose unit tests the daemon leaves to rustc, so that edit is cargo's build.
- **axum's `example-testing`.** The test binary the daemon builds crashes (SIGSEGV) in
  `the_real_deal`, which serves axum on a TCP socket and calls it with hyper's client: tokio's
  `Registration::readiness` reads a null `Arc<ScheduledIo>` from `TcpListener::accept`. Five of
  the six tests pass; cargo's binary passes all six. Seen on Linux and on macOS.
- **bevy's breakout.** The daemon cannot type `(apply_velocity, move_paddle,
  check_for_collisions).chain()` (the method comes from impls bevy generates for tuples of
  systems), and `cargo ra-jit build` fails at the first edit instead of handing the build to
  cargo. Cold and no-op are measured; the edits are not.
- **helix.** After a 31 s cold build (the same as cargo's) the daemon takes 638 s to be ready
  (636 to 641 s over five cold runs) and holds 98 GB; with a warm store it is no faster (681 s).
  With the test targets loaded too it was killed for memory on a 123 GB machine. Its edits, once
  ready, are 10x to 48x faster than cargo's.
  nushell, a larger workspace, is ready 33 s after its build, at 16 GB.
- **zed** (1.7M lines, 1,291 crates) does not fit: the daemon needs more than 111 GB.
- **Cold.** `cargo ra-jit build` from nothing is cargo's build plus the start of the daemon:
  6.29 s against 5.56 s on ripgrep, and the daemon is ready 6.2 s later.
- **No-op** on ripgrep: 33 ms against cargo's 30 ms. This run's Cranelift column used the
  binary built with the `llvm` feature, which spends about 6 ms loading libLLVM at every start;
  the bench now measures that column with ra-jit as released (19 ms on ripgrep in
  [results.md](results.md#warm-edits-on-linux-x86-64-where-the-time-goes-and-build-scripts-2026-10-20)).

Two commits after `2b2f2dcd5b` (`20708cd2da`, `c8f58ac7d9`) stop the daemon from running build
scripts again that cargo just ran, which roughly halves the edits of fd and hyperfine (fd's
function edit 88 to 43 ms on another machine, same page). The next run of the workflow
measures them here.

## Earlier one-off measurements

Edit, then build, on a workspace of 200 generated crates (`harness/genws.py`), Linux x86-64,
8 CPUs, both warm and incremental, same linker, median of three. Measured once by hand before
this page existed; not updated by CI.

| Edit | cargo | ra-jit | |
|---|---:|---:|---:|
| A function in the main crate | 1.76 s | 0.42 s | 4.2x |
| A function in a crate everything depends on | 6.33 s | 0.60 s | 10.6x |
| Functions in 30 crates at once | 6.26 s | 0.62 s | 10.1x |
| A new function | 2.09 s | 0.56 s | 3.7x |
| A signature used in every crate | 13.87 s | 10.87 s | 1.3x |

## Results

<!-- bench:start -->
Every number of the newest run of each platform, then of each project measured on its own on it (bun, by the `bench-bun` workflow), under its own machine. Each platform is a machine of its own: its numbers compare with each other and with its own history, not with another platform's.

### macOS arm64

ra-jit [`84760c3f51`](https://github.com/indexable-inc/ra-jit/commit/84760c3f517a76f6e8a3aa41ccd81fc3f26b87c7), 2026-10-21T21:03:27Z, Rust 1.99.0, 3 runs of each edit, measured by hand. Raw: [benchmarks/macos-arm64.json](benchmarks/macos-arm64.json).

| Machine | |
|---|---|
| Name | MacBook Pro (Mac17,6), macOS 27.0.1, with other work running |
| CPU | Apple M5 Max |
| Cores | 18 |
| Memory (GiB) | 128 |
| OS | macOS |
| Kernel | 27.0.0 |
| rustc | rustc 1.99.0 (b940084d7 2026-09-28) |
| C compiler | Apple clang version 21.0.0 (clang-2100.3.34.2) |
| Linker | - |
| LLVM | - |
| Incremental | profile default (on for dev) |
| Compiler cache | none |
| Load average at the start | 48.0, 46.1, 48.4 |

#### Edits, Cranelift, macOS arm64

Median, then the fastest and slowest run. Built: who built the edit (daemon, or cargo and why). Link: the change was spliced into the program, or it was linked again.

| Project | Edit | cargo | ra-jit | | cargo runs | ra-jit runs | Built | Link |
|---|---|---:|---:|---:|---|---|---|---|
| ripgrep 15.1.0 | Edit a function | 993 ms | 126 ms | 7.9x | 646 ms to 1.11 s | 125 ms to 191 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a widely used function | 1.31 s | 159 ms | 8.2x | 1.02 s to 4.28 s | 104 ms to 160 ms | daemon | spliced |
| ripgrep 15.1.0 | Change a signature | 828 ms | 175 ms | 4.7x | 785 ms to 1.36 s | 102 ms to 305 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a generic function | 1.93 s | 100 ms | 19x | 1.27 s to 1.95 s | 92 ms to 157 ms | daemon | spliced |
| ripgrep 15.1.0 | Add a function | 1.81 s | 248 ms | 7.3x | 706 ms to 2.86 s | 104 ms to 271 ms | daemon | spliced |
| ripgrep 15.1.0 | Add a struct field | 1.88 s | 191 ms | 9.8x | 1.35 s to 1.96 s | 94 ms to 298 ms | daemon | spliced |
| ripgrep 15.1.0 | Nothing changed | 67 ms | 54 ms | 1.2x | 63 ms to 72 ms | 50 ms to 61 ms | daemon |  |
| fd 10.3.0 | Edit a function | 630 ms | 109 ms | 5.8x | 611 ms to 1.29 s | 99 ms to 140 ms | daemon | spliced |
| fd 10.3.0 | Edit a widely used function | 734 ms | 102 ms | 7.2x | 691 ms to 1.01 s | 87 ms to 105 ms | daemon | spliced |
| fd 10.3.0 | Change a signature | 681 ms | 113 ms | 6.0x | 563 ms to 835 ms | 100 ms to 205 ms | daemon | spliced |
| fd 10.3.0 | Add a function | 712 ms | 126 ms | 5.7x | 593 ms to 745 ms | 106 ms to 228 ms | daemon | spliced |
| fd 10.3.0 | Add a struct field | 1.09 s | 127 ms | 8.6x | 894 ms to 1.31 s | 113 ms to 143 ms | daemon | spliced |
| fd 10.3.0 | Nothing changed | 144 ms | 49 ms | 2.9x | 85 ms to 369 ms | 48 ms to 84 ms | daemon |  |
| nushell 0.108.0 | Edit a function | 1.81 s | 454 ms | 4.0x | 1.66 s to 1.89 s | 383 ms to 713 ms | daemon | spliced |
| nushell 0.108.0 | Edit a widely used function | 14.39 s | 850 ms | 17x | 12.49 s to 15.24 s | 840 ms to 861 ms | daemon | spliced |
| nushell 0.108.0 | Change a signature | 10.28 s | 716 ms | 14x | 9.54 s to 12.34 s | 694 ms to 867 ms | daemon | spliced |
| nushell 0.108.0 | Edit a generic function | 20.61 s | 2.12 s | 9.7x | 9.59 s to 26.96 s | 986 ms to 2.50 s | daemon | full link: a program too large for frames in front of its own: one is 0x1000020 bytes in |
| nushell 0.108.0 | Add a function | 2.47 s | 444 ms | 5.6x | 1.74 s to 2.86 s | 353 ms to 480 ms | daemon | spliced |
| nushell 0.108.0 | Add a struct field | 25.84 s | 2.51 s | 10x | 23.58 s to 30.94 s | 2.21 s to 3.54 s | daemon | full link: a program too large for frames in front of its own: one is 0x1000008 bytes in |
| nushell 0.108.0 | Nothing changed | 407 ms | 45 ms | 9.0x | 347 ms to 433 ms | 41 ms to 56 ms | daemon |  |

#### Cold, Cranelift, macOS arm64

From an empty target directory, crates already downloaded, one run. The empty store fetches the standard library as a release's first build does. `cargo ra-jit build` is cargo's own build (the daemon is not loaded yet); the daemon then loads the workspace and builds it once, and edits are fast from then on. Warm store: the same in another checkout of the same commit, with the store the first one filled.

| Project | `cargo build` | `cargo ra-jit build` | Daemon ready, empty store | Daemon ready, warm store | Functions compiled | Daemon memory | Store |
|---|---:|---:|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | 9.83 s | 12.08 s | 14.68 s | 17.62 s | 18,437 | 2,360 MB | 72 MB |
| fd 10.3.0 | 9.17 s | 11.57 s | 12.62 s | 13.16 s | 8,478 | 1,946 MB | 32 MB |
| nushell 0.108.0 | 49.69 s | 77.23 s | 92.67 s | 139.83 s | 167,893 | 16,137 MB | 1,121 MB |

#### Memory and disk, Cranelift, macOS arm64

Peak: the most every process doing the work held at once, sampled every 10 ms and never below the largest process, median of the runs. Largest process: the peak of the one process that held the most, as the kernel kept it: `wait4`'s for the command's tree, and each process's peak sampled (on macOS the kernel's peak footprint of each, counting for the daemon only a peak it passed during the step). ra-jit is the daemon, its workers and what they run, and the `cargo ra-jit build` with any cargo it ran; cargo is cargo itself, rustc, build scripts and the linker. Daemon after: what the daemon and its workers hold once the step is over ([how](#memory-and-disk)).

| Project | Step | cargo, peak | ra-jit, peak | cargo, largest process | ra-jit, largest process | Daemon after |
|---|---|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | Cold, until the daemon is ready | 541 MB | 2.4 GB | 356 MB | 2.2 GB | 2.2 GB |
| ripgrep 15.1.0 | Edit a function | 235 MB | 2.9 GB | 235 MB | 1.9 GB | 2.9 GB |
| ripgrep 15.1.0 | Edit a widely used function | 243 MB | 2.9 GB | 243 MB | 1.9 GB | 2.8 GB |
| ripgrep 15.1.0 | Change a signature | 235 MB | 2.9 GB | 235 MB | 1.9 GB | 2.8 GB |
| ripgrep 15.1.0 | Edit a generic function | 237 MB | 2.9 GB | 237 MB | 1.9 GB | 2.8 GB |
| ripgrep 15.1.0 | Add a function | 234 MB | 2.9 GB | 234 MB | 1.9 GB | 2.8 GB |
| ripgrep 15.1.0 | Add a struct field | 286 MB | 2.9 GB | 286 MB | 1.9 GB | 2.8 GB |
| ripgrep 15.1.0 | Nothing changed | 40 MB | 2.8 GB | 40 MB | 1.9 GB | 2.8 GB |
| fd 10.3.0 | Cold, until the daemon is ready | 897 MB | 1.9 GB | 389 MB | 1.8 GB | 1.8 GB |
| fd 10.3.0 | Edit a function | 212 MB | 2.6 GB | 212 MB | 1.6 GB | 2.6 GB |
| fd 10.3.0 | Edit a widely used function | 199 MB | 2.6 GB | 192 MB | 1.6 GB | 2.6 GB |
| fd 10.3.0 | Change a signature | 226 MB | 2.6 GB | 226 MB | 1.6 GB | 2.6 GB |
| fd 10.3.0 | Add a function | 211 MB | 2.6 GB | 211 MB | 1.6 GB | 2.6 GB |
| fd 10.3.0 | Add a struct field | 258 MB | 2.6 GB | 258 MB | 1.6 GB | 2.6 GB |
| fd 10.3.0 | Nothing changed | 48 MB | 2.5 GB | 48 MB | 1.6 GB | 2.5 GB |
| nushell 0.108.0 | Cold, until the daemon is ready | 2.4 GB | 16.1 GB | 1.1 GB | 15.3 GB | 11.5 GB |
| nushell 0.108.0 | Edit a function | 1.1 GB | 12.5 GB | 1.1 GB | 11.5 GB | 12.5 GB |
| nushell 0.108.0 | Edit a widely used function | 1.1 GB | 12.5 GB | 1.1 GB | 11.5 GB | 12.5 GB |
| nushell 0.108.0 | Change a signature | 1.1 GB | 12.5 GB | 1.1 GB | 11.5 GB | 12.5 GB |
| nushell 0.108.0 | Edit a generic function | 1.1 GB | 13.2 GB | 1.1 GB | 11.3 GB | 12.2 GB |
| nushell 0.108.0 | Add a function | 1.1 GB | 12.5 GB | 1.1 GB | 11.5 GB | 12.5 GB |
| nushell 0.108.0 | Add a struct field | 1.1 GB | 13.3 GB | 1.1 GB | 11.4 GB | 12.2 GB |
| nushell 0.108.0 | Nothing changed | 156 MB | 12.1 GB | 156 MB | 11.2 GB | 12.1 GB |

The daemon between builds, and what each side keeps on disk once the edits are done (each block once, [how](#memory-and-disk)). After the first build: once the step after the cold build (nothing changed) is done; growth is from then to the end, over every edit made and taken back. Warm store: the cold start of another checkout with the store the first one filled.

| Project | Daemon ready | After the first build | At the end | Growth | Warm store: peak, ready | cargo target | ra-jit target | Store | Home |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | 2.2 GB | 2.8 GB | 2.8 GB after 39 builds | +23 MB | 2.3 GB, 2.0 GB | 416 MB | 526 MB | 72 MB | 0 MB |
| fd 10.3.0 | 1.8 GB | 2.5 GB | 2.6 GB after 33 builds | +50 MB | 1.9 GB, 1.6 GB | 355 MB | 427 MB | 33 MB | 0 MB |
| nushell 0.108.0 | 11.5 GB | 12.1 GB | 12.5 GB after 39 builds | +367 MB | 16.5 GB, 11.7 GB | 5.3 GB | 5.8 GB | 1.1 GB | 1 MB |

#### How fast the program runs, Cranelift, macOS arm64

The same command run by the program ra-jit linked and by cargo's debug and release builds, in turn. Median, then fastest and slowest; where measured, then the program's peak resident memory while it ran (median of the runs, as the kernel kept it) and the size of the program.

| Project | Command | ra-jit | cargo debug | cargo release |
|---|---|---:|---:|---:|
| ripgrep 15.1.0 | `rg -uuu --sort path -c \w+_unchecked\(\|unsafe \{ <the toolchain's library source>` | 1.01 s (676 ms to 1.43 s); 12 MB, 28.5 MB | 753 ms (679 ms to 1.07 s); 8 MB, 15.6 MB | 167 ms (129 ms to 308 ms); 6 MB, 6.6 MB |

### macOS arm64, bun

ra-jit [`27077156cc`](https://github.com/indexable-inc/ra-jit/commit/27077156ccbd476079d261e529fc25b3225f0ace), 2026-10-23T05:22:33Z, Rust 1.99.0, 3 runs of each edit, measured by hand. Raw: [benchmarks/bun-macos-arm64.json](benchmarks/bun-macos-arm64.json).

| Machine | |
|---|---|
| Name | - |
| CPU | Apple M5 Max |
| Cores | 18 |
| Memory (GiB) | 128 |
| OS | macOS |
| Kernel | 27.0.0 |
| rustc | rustc 1.100.0-nightly (574ff7d98 2026-09-14) |
| C compiler | clang version 21.1.8 |
| Linker | - |
| LLVM | 23.1.3 |
| Incremental | profile default (on for dev) |
| Compiler cache | none |
| Load average at the start | 8.5, 10.0, 13.9 |

#### Edits, Cranelift, macOS arm64, bun

Median, then the fastest and slowest run. Built: who built the edit (daemon, or cargo and why). Link: the change was spliced into the program, or it was linked again.

| Project | Edit | cargo | ra-jit | | cargo runs | ra-jit runs | Built | Link |
|---|---|---:|---:|---:|---|---|---|---|
| bun 1.4.3 | Edit a function | 7.55 s | 362 ms | 21x | 6.96 s to 7.71 s | 305 ms to 1.07 s | daemon | spliced |
| bun 1.4.3 | Edit a widely used function | 16.01 s | 457 ms | 35x | 15.53 s to 16.95 s | 454 ms to 604 ms | daemon | spliced |
| bun 1.4.3 | Change a signature | 7.26 s | 823 ms | 8.8x | 6.30 s to 8.27 s | 566 ms to 573.75 s | daemon | full link: no executable to add to |
| bun 1.4.3 | Edit a generic function | 14.91 s | 568 ms | 26x | 13.09 s to 15.59 s | 523 ms to 692 ms | daemon | spliced |
| bun 1.4.3 | Add a function | 11.10 s | 558 ms | 20x | 10.77 s to 11.71 s | 557 ms to 566 ms | daemon | spliced |
| bun 1.4.3 | Nothing changed | 199 ms | 122 ms | 1.6x | 137 ms to 214 ms | 90 ms to 190 ms | daemon |  |

#### Cold, Cranelift, macOS arm64, bun

From an empty target directory, crates already downloaded, one run. The empty store fetches the standard library as a release's first build does. `cargo ra-jit build` is cargo's own build (the daemon is not loaded yet); the daemon then loads the workspace and builds it once, and edits are fast from then on. Warm store: the same in another checkout of the same commit, with the store the first one filled.

| Project | `cargo build` | `cargo ra-jit build` | Daemon ready, empty store | Daemon ready, warm store | Functions compiled | Daemon memory | Store |
|---|---:|---:|---:|---:|---:|---:|---:|
| bun 1.4.3 | 83.29 s | 79.49 s | 81.44 s | 64.31 s | 0 | 13,684 MB | 2,654 MB |

#### Memory and disk, Cranelift, macOS arm64, bun

Peak: the most every process doing the work held at once, sampled every 10 ms and never below the largest process, median of the runs. Largest process: the peak of the one process that held the most, as the kernel kept it: `wait4`'s for the command's tree, and each process's peak sampled (on macOS the kernel's peak footprint of each, counting for the daemon only a peak it passed during the step). ra-jit is the daemon, its workers and what they run, and the `cargo ra-jit build` with any cargo it ran; cargo is cargo itself, rustc, build scripts and the linker. Daemon after: what the daemon and its workers hold once the step is over ([how](#memory-and-disk)).

| Project | Step | cargo, peak | ra-jit, peak | cargo, largest process | ra-jit, largest process | Daemon after |
|---|---|---:|---:|---:|---:|---:|
| bun 1.4.3 | Cold, until the daemon is ready | 3.1 GB | 13.7 GB | 2.7 GB | 11.8 GB | 11.8 GB |
| bun 1.4.3 | Edit a function | 2.1 GB | 26.1 GB | 2.1 GB | 26.0 GB | 26.1 GB |
| bun 1.4.3 | Edit a widely used function | 2.1 GB | 27.7 GB | 2.1 GB | 27.7 GB | 27.7 GB |
| bun 1.4.3 | Change a signature | 2.1 GB | 27.7 GB | 2.1 GB | 27.7 GB | 27.7 GB |
| bun 1.4.3 | Edit a generic function | 2.3 GB | 27.7 GB | 2.3 GB | 27.7 GB | 27.7 GB |
| bun 1.4.3 | Add a function | 2.2 GB | 27.8 GB | 2.2 GB | 27.7 GB | 27.8 GB |
| bun 1.4.3 | Nothing changed | 98 MB | 26.1 GB | 98 MB | 26.0 GB | 26.0 GB |

The daemon between builds, and what each side keeps on disk once the edits are done (each block once, [how](#memory-and-disk)). After the first build: once the step after the cold build (nothing changed) is done; growth is from then to the end, over every edit made and taken back. Warm store: the cold start of another checkout with the store the first one filled.

| Project | Daemon ready | After the first build | At the end | Growth | Warm store: peak, ready | cargo target | ra-jit target | Store | Home |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| bun 1.4.3 | 11.8 GB | 26.0 GB | 27.8 GB after 33 builds | +1.7 GB | 13.8 GB, 12.0 GB | 6.0 GB | 8.6 GB | 2.6 GB | 103 MB |

#### How fast the program runs, Cranelift, macOS arm64, bun

The same command run by the program ra-jit linked and by cargo's debug and release builds, in turn. Median, then fastest and slowest; where measured, then the program's peak resident memory while it ran (median of the runs, as the kernel kept it) and the size of the program.

| Project | Command | ra-jit | cargo debug | cargo release |
|---|---|---:|---:|---:|
| bun 1.4.3 | `bun -e const t = new Bun.Transpiler({ loader: 'ts', minifyWhitespace: true }); const fs = require('fs'); const sources = ['http2', 'net', '_http_server', 'fs', 'child_process'].map(n => fs.readFileSync('src/js/node/' + n + '.ts', 'utf8')); let n = 0; for (let i = 0; i < 20; i++) for (const s of sources) n += t.transformSync(s).length; console.log(n)` | 1.33 s (1.08 s to 1.55 s); 74 MB, 478.6 MB | 846 ms (720 ms to 935 ms); 54 MB, 245.9 MB | 113 ms (108 ms to 151 ms); 41 MB, 103.1 MB |

### Linux aarch64

ra-jit [`c93e530e47`](https://github.com/indexable-inc/ra-jit/commit/c93e530e474c4de2d67a7a9ec49fd5581894efb8), 2026-10-23T21:59:50Z, Rust 1.99.0, 5 runs of each edit, measured by hand. Raw: [benchmarks/linux-aarch64.json](benchmarks/linux-aarch64.json).

| Machine | |
|---|---|
| Name | AWS c8g.16xlarge, pinned to 16 cores with taskset |
| CPU | Neoverse-V2 |
| Cores | 16 |
| Memory (GiB) | 124 |
| OS | Linux |
| Kernel | 6.18.53 |
| rustc | rustc 1.99.0 (b940084d7 2026-09-28) |
| C compiler | gcc (GCC) 16.2.0 |
| Linker | GNU ld (GNU Binutils) 2.46 |
| LLVM | 20.1.8 |
| Incremental | profile default (on for dev) |
| Compiler cache | none |
| Load average at the start | 7.6, 3.2, 1.2 |

#### Edits, Cranelift, Linux aarch64

Median, then the fastest and slowest run. Built: who built the edit (daemon, or cargo and why). Link: the change was spliced into the program, or it was linked again.

| Project | Edit | cargo | ra-jit | | cargo runs | ra-jit runs | Built | Link |
|---|---|---:|---:|---:|---|---|---|---|
| ripgrep 15.1.0 | Edit a function | 1.18 s | 62 ms | 19x | 1.17 s to 1.19 s | 62 ms to 75 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a widely used function | 1.54 s | 57 ms | 27x | 1.53 s to 1.57 s | 57 ms to 94 ms | daemon | spliced |
| ripgrep 15.1.0 | Change a signature | 1.34 s | 101 ms | 13x | 1.34 s to 1.35 s | 101 ms to 141 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a generic function | 1.51 s | 55 ms | 27x | 1.50 s to 1.52 s | 55 ms to 55 ms | daemon | spliced |
| ripgrep 15.1.0 | Add a function | 1.20 s | 69 ms | 17x | 1.20 s to 1.20 s | 68 ms to 71 ms | daemon | spliced |
| ripgrep 15.1.0 | Add a struct field | 1.72 s | 64 ms | 27x | 1.72 s to 1.75 s | 64 ms to 79 ms | daemon | spliced |
| ripgrep 15.1.0 | Nothing changed | 25 ms | 18 ms | 1.4x | 24 ms to 25 ms | 18 ms to 21 ms | daemon |  |
| fd 10.3.0 | Edit a function | 1.27 s | 58 ms | 22x | 1.27 s to 1.29 s | 57 ms to 135 ms | daemon | spliced |
| fd 10.3.0 | Edit a widely used function | 1.24 s | 35 ms | 35x | 1.23 s to 1.26 s | 35 ms to 47 ms | daemon | spliced |
| fd 10.3.0 | Change a signature | 1.29 s | 64 ms | 20x | 1.28 s to 1.30 s | 63 ms to 176 ms | daemon | spliced |
| fd 10.3.0 | Add a function | 1.28 s | 62 ms | 21x | 1.27 s to 1.29 s | 61 ms to 98 ms | daemon | spliced |
| fd 10.3.0 | Add a struct field | 1.53 s | 51 ms | 30x | 1.52 s to 1.57 s | 51 ms to 83 ms | daemon | spliced |
| fd 10.3.0 | Nothing changed | 34 ms | 17 ms | 2.0x | 34 ms to 34 ms | 17 ms to 18 ms | daemon |  |
| bat 0.26.0 | Edit a function | 1.62 s | 56 ms | 29x | 1.61 s to 1.63 s | 55 ms to 76 ms | daemon | spliced |
| bat 0.26.0 | Edit a widely used function | 1.92 s | 55 ms | 35x | 1.91 s to 1.92 s | 55 ms to 56 ms | daemon | spliced |
| bat 0.26.0 | Change a signature | 1.87 s | 57 ms | 33x | 1.87 s to 1.88 s | 57 ms to 89 ms | daemon | spliced |
| bat 0.26.0 | Add a function | 1.62 s | 62 ms | 26x | 1.62 s to 1.62 s | 60 ms to 65 ms | daemon | spliced |
| bat 0.26.0 | Add a struct field | 2.22 s | 58 ms | 38x | 2.21 s to 2.26 s | 57 ms to 62 ms | daemon | spliced |
| bat 0.26.0 | Nothing changed | 65 ms | 17 ms | 3.8x | 65 ms to 65 ms | 17 ms to 18 ms | daemon |  |
| hyperfine 1.19.0 | Edit a function | 753 ms | 38 ms | 20x | 743 ms to 769 ms | 38 ms to 45 ms | daemon | spliced |
| hyperfine 1.19.0 | Edit a widely used function | 797 ms | 59 ms | 14x | 793 ms to 799 ms | 59 ms to 61 ms | daemon | spliced |
| hyperfine 1.19.0 | Change a signature | 806 ms | 93 ms | 8.7x | 803 ms to 808 ms | 93 ms to 101 ms | daemon | spliced |
| hyperfine 1.19.0 | Add a function | 751 ms | 41 ms | 18x | 746 ms to 753 ms | 41 ms to 42 ms | daemon | spliced |
| hyperfine 1.19.0 | Add a struct field | 886 ms | 78 ms | 11x | 879 ms to 903 ms | 77 ms to 79 ms | daemon | spliced |
| hyperfine 1.19.0 | Nothing changed | 44 ms | 17 ms | 2.6x | 44 ms to 44 ms | 17 ms to 18 ms | daemon |  |
| just 1.43.0 | Edit a function | 1.51 s | 43 ms | 35x | 1.49 s to 1.52 s | 42 ms to 51 ms | daemon | spliced |
| just 1.43.0 | Edit a widely used function | 1.53 s | 71 ms | 22x | 1.51 s to 1.54 s | 70 ms to 73 ms | daemon | spliced |
| just 1.43.0 | Change a signature | 1.50 s | 84 ms | 18x | 1.49 s to 1.52 s | 84 ms to 89 ms | daemon | spliced |
| just 1.43.0 | Add a function | 1.62 s | 63 ms | 26x | 1.62 s to 1.63 s | 62 ms to 64 ms | daemon | spliced |
| just 1.43.0 | Add a struct field | 3.00 s | 136 ms | 22x | 2.97 s to 3.27 s | 131 ms to 202 ms | daemon | spliced |
| just 1.43.0 | Nothing changed | 43 ms | 17 ms | 2.5x | 43 ms to 43 ms | 17 ms to 17 ms | daemon |  |
| axum 0.8.6 | Edit a function | 803 ms | 51 ms | 16x | 801 ms to 810 ms | 51 ms to 56 ms | daemon | spliced |
| axum 0.8.6 | Edit a widely used function | 1.37 s | 43 ms | 32x | 1.36 s to 1.37 s | 43 ms to 52 ms | daemon | spliced |
| axum 0.8.6 | Add a function | 805 ms | 52 ms | 15x | 801 ms to 818 ms | 52 ms to 55 ms | daemon | spliced |
| axum 0.8.6 | Nothing changed | 73 ms | 18 ms | 4.1x | 73 ms to 85 ms | 18 ms to 19 ms | daemon |  |
| nushell 0.108.0 | Edit a function | 7.20 s | 323 ms | 22x | 7.18 s to 7.24 s | 320 ms to 341 ms | daemon | spliced |
| nushell 0.108.0 | Edit a widely used function | 11.36 s | 295 ms | 39x | 11.28 s to 11.39 s | 290 ms to 311 ms | daemon | spliced |
| nushell 0.108.0 | Change a signature | 11.32 s | 371 ms | 31x | 11.26 s to 11.37 s | 354 ms to 653 ms | daemon | spliced |
| nushell 0.108.0 | Edit a generic function | 13.50 s | 239 ms | 56x | 13.45 s to 13.68 s | 235 ms to 264 ms | daemon | spliced |
| nushell 0.108.0 | Add a function | 7.17 s | 327 ms | 22x | 7.13 s to 7.22 s | 324 ms to 345 ms | daemon | spliced |
| nushell 0.108.0 | Add a struct field | 15.42 s | 388 ms | 40x | 15.38 s to 15.72 s | 360 ms to 416 ms | daemon | spliced |
| nushell 0.108.0 | Nothing changed | 192 ms | 21 ms | 9.1x | 189 ms to 193 ms | 21 ms to 22 ms | daemon |  |
| helix 25.07.1 | Edit a function | 6.84 s | 82 ms | 83x | 6.80 s to 6.84 s | 81 ms to 57.53 s | daemon | full link: no executable to add to |
| helix 25.07.1 | Edit a widely used function | 8.20 s | 95 ms | 86x | 8.16 s to 8.23 s | 94 ms to 96 ms | daemon | spliced |
| helix 25.07.1 | Change a signature | 7.59 s | 267 ms | 28x | 7.58 s to 7.64 s | 250 ms to 464 ms | daemon | spliced |
| helix 25.07.1 | Add a function | 6.87 s | 142 ms | 48x | 6.82 s to 6.89 s | 139 ms to 152 ms | daemon | spliced |
| helix 25.07.1 | Add a struct field | 14.92 s | 350 ms | 43x | 14.82 s to 15.54 s | 347 ms to 365 ms | daemon | spliced |
| helix 25.07.1 | Nothing changed | 92 ms | 18 ms | 5.1x | 92 ms to 92 ms | 18 ms to 102 ms | daemon |  |
| bevy 0.20.0 | Edit a function | 22.81 s | 884 ms | 26x | 22.68 s to 22.85 s | 883 ms to 1.15 s | daemon | spliced |
| bevy 0.20.0 | Edit a widely used function | 32.13 s | 352 ms | 91x | 32.03 s to 32.22 s | 349 ms to 442 ms | daemon | spliced |
| bevy 0.20.0 | Add a function | 22.85 s | 945 ms | 24x | 22.75 s to 22.96 s | 941 ms to 1.16 s | daemon | spliced |
| bevy 0.20.0 | Nothing changed | 217 ms | 24 ms | 9.0x | 215 ms to 235 ms | 23 ms to 24 ms | daemon |  |

#### Cold, Cranelift, Linux aarch64

From an empty target directory, crates already downloaded, median of 5 runs, each from nothing. The empty store fetches the standard library as a release's first build does. `cargo ra-jit build` is cargo's own build (the daemon is not loaded yet); the daemon then loads the workspace and builds it once, and edits are fast from then on. Warm store: the same in another checkout of the same commit, with the store the first one filled.

| Project | `cargo build` | `cargo ra-jit build` | Daemon ready, empty store | Daemon ready, warm store | Functions compiled | Daemon memory | Store |
|---|---:|---:|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | 6.11 s (6.05 s to 6.12 s) | 6.31 s (6.29 s to 6.34 s) | 6.86 s (6.85 s to 6.90 s) | 6.67 s (6.66 s to 6.72 s) | 0 | 2,095 MB | 18 MB |
| fd 10.3.0 | 32.35 s (32.08 s to 32.36 s) | 32.51 s (32.42 s to 32.54 s) | 33.33 s (33.24 s to 33.36 s) | 32.86 s (32.77 s to 32.94 s) | 0 | 1,820 MB | 17 MB |
| bat 0.26.0 | 12.98 s (12.88 s to 13.09 s) | 13.10 s (12.91 s to 13.22 s) | 14.98 s (14.79 s to 15.03 s) | 13.90 s (13.78 s to 14.43 s) | 0 | 2,222 MB | 31 MB |
| hyperfine 1.19.0 | 6.21 s (6.21 s to 6.25 s) | 6.44 s (6.40 s to 6.46 s) | 7.52 s (7.22 s to 7.54 s) | 6.96 s (6.94 s to 7.24 s) | 0 | 2,042 MB | 16 MB |
| just 1.43.0 | 9.09 s (8.98 s to 9.14 s) | 9.01 s (8.94 s to 9.20 s) | 10.36 s (10.29 s to 10.55 s) | 9.69 s (9.65 s to 10.14 s) | 0 | 2,556 MB | 22 MB |
| axum 0.8.6 | 8.91 s (8.87 s to 8.91 s) | 9.21 s (9.17 s to 9.23 s) | 11.09 s (11.05 s to 11.37 s) | 10.13 s (10.04 s to 10.87 s) | 0 | 2,380 MB | 18 MB |
| nushell 0.108.0 | 46.63 s (46.45 s to 46.92 s) | 41.82 s (41.52 s to 42.24 s) | 47.69 s (47.39 s to 48.12 s) | 45.02 s (44.76 s to 46.73 s) | 0 | 11,547 MB | 84 MB |
| helix 25.07.1 | 42.28 s (41.58 s to 42.57 s) | 37.54 s (37.38 s to 37.61 s) | 40.76 s (40.59 s to 41.02 s) | 40.94 s (40.81 s to 40.98 s) | 0 | 10,612 MB | 362 MB |
| bevy 0.20.0 | 119.12 s (117.34 s to 119.93 s) | 101.44 s (100.21 s to 101.73 s) | 126.30 s (124.98 s to 133.39 s) | 124.32 s (123.69 s to 131.26 s) | 0 | 32,608 MB | 311 MB |

#### Memory and disk, Cranelift, Linux aarch64

Peak: the most every process doing the work held at once, sampled every 10 ms and never below the largest process, median of the runs. Largest process: the peak of the one process that held the most, as the kernel kept it: `wait4`'s for the command's tree, and each process's peak sampled (on macOS the kernel's peak footprint of each, counting for the daemon only a peak it passed during the step). ra-jit is the daemon, its workers and what they run, and the `cargo ra-jit build` with any cargo it ran; cargo is cargo itself, rustc, build scripts and the linker. Daemon after: what the daemon and its workers hold once the step is over ([how](#memory-and-disk)).

| Project | Step | cargo, peak | ra-jit, peak | cargo, largest process | ra-jit, largest process | Daemon after |
|---|---|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | Cold, until the daemon is ready | 1.9 GB | 2.5 GB | 416 MB | 1.7 GB | 1.5 GB |
| ripgrep 15.1.0 | Edit a function | 464 MB | 1.7 GB | 274 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Edit a widely used function | 488 MB | 1.6 GB | 286 MB | 1.6 GB | 1.6 GB |
| ripgrep 15.1.0 | Change a signature | 464 MB | 1.7 GB | 271 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Edit a generic function | 486 MB | 1.6 GB | 281 MB | 1.6 GB | 1.6 GB |
| ripgrep 15.1.0 | Add a function | 463 MB | 1.6 GB | 275 MB | 1.6 GB | 1.6 GB |
| ripgrep 15.1.0 | Add a struct field | 506 MB | 1.6 GB | 331 MB | 1.6 GB | 1.6 GB |
| ripgrep 15.1.0 | Nothing changed | 35 MB | 1.8 GB | 35 MB | 1.7 GB | 1.7 GB |
| fd 10.3.0 | Cold, until the daemon is ready | 2.2 GB | 3.3 GB | 598 MB | 1.7 GB | 1.7 GB |
| fd 10.3.0 | Edit a function | 520 MB | 1.5 GB | 304 MB | 1.4 GB | 1.5 GB |
| fd 10.3.0 | Edit a widely used function | 513 MB | 1.5 GB | 304 MB | 1.4 GB | 1.5 GB |
| fd 10.3.0 | Change a signature | 523 MB | 1.5 GB | 304 MB | 1.4 GB | 1.5 GB |
| fd 10.3.0 | Add a function | 521 MB | 1.5 GB | 304 MB | 1.4 GB | 1.5 GB |
| fd 10.3.0 | Add a struct field | 548 MB | 1.5 GB | 304 MB | 1.4 GB | 1.5 GB |
| fd 10.3.0 | Nothing changed | 57 MB | 1.6 GB | 57 MB | 1.5 GB | 1.6 GB |
| bat 0.26.0 | Cold, until the daemon is ready | 3.4 GB | 5.8 GB | 460 MB | 2.2 GB | 1.9 GB |
| bat 0.26.0 | Edit a function | 672 MB | 2.0 GB | 460 MB | 1.9 GB | 1.9 GB |
| bat 0.26.0 | Edit a widely used function | 670 MB | 2.0 GB | 460 MB | 1.9 GB | 1.9 GB |
| bat 0.26.0 | Change a signature | 655 MB | 2.0 GB | 460 MB | 1.9 GB | 1.9 GB |
| bat 0.26.0 | Add a function | 672 MB | 1.9 GB | 460 MB | 1.9 GB | 1.9 GB |
| bat 0.26.0 | Add a struct field | 688 MB | 2.0 GB | 460 MB | 1.9 GB | 1.9 GB |
| bat 0.26.0 | Nothing changed | 58 MB | 2.0 GB | 58 MB | 2.0 GB | 2.0 GB |
| hyperfine 1.19.0 | Cold, until the daemon is ready | 2.2 GB | 2.5 GB | 344 MB | 2.0 GB | 1.9 GB |
| hyperfine 1.19.0 | Edit a function | 361 MB | 1.8 GB | 223 MB | 1.7 GB | 1.7 GB |
| hyperfine 1.19.0 | Edit a widely used function | 371 MB | 1.7 GB | 239 MB | 1.6 GB | 1.7 GB |
| hyperfine 1.19.0 | Change a signature | 372 MB | 1.7 GB | 243 MB | 1.6 GB | 1.7 GB |
| hyperfine 1.19.0 | Add a function | 362 MB | 1.7 GB | 226 MB | 1.6 GB | 1.7 GB |
| hyperfine 1.19.0 | Add a struct field | 385 MB | 1.7 GB | 272 MB | 1.6 GB | 1.7 GB |
| hyperfine 1.19.0 | Nothing changed | 79 MB | 1.8 GB | 79 MB | 1.7 GB | 1.8 GB |
| just 1.43.0 | Cold, until the daemon is ready | 3.0 GB | 3.6 GB | 606 MB | 2.2 GB | 1.8 GB |
| just 1.43.0 | Edit a function | 434 MB | 2.1 GB | 319 MB | 2.0 GB | 2.0 GB |
| just 1.43.0 | Edit a widely used function | 422 MB | 2.0 GB | 344 MB | 2.0 GB | 2.0 GB |
| just 1.43.0 | Change a signature | 422 MB | 2.0 GB | 342 MB | 2.0 GB | 2.0 GB |
| just 1.43.0 | Add a function | 432 MB | 2.0 GB | 361 MB | 1.9 GB | 2.0 GB |
| just 1.43.0 | Add a struct field | 640 MB | 2.0 GB | 597 MB | 2.0 GB | 2.0 GB |
| just 1.43.0 | Nothing changed | 79 MB | 2.1 GB | 79 MB | 2.0 GB | 2.1 GB |
| axum 0.8.6 | Cold, until the daemon is ready | 2.1 GB | 3.1 GB | 507 MB | 2.4 GB | 2.0 GB |
| axum 0.8.6 | Edit a function | 399 MB | 1.9 GB | 230 MB | 1.9 GB | 1.9 GB |
| axum 0.8.6 | Edit a widely used function | 397 MB | 1.9 GB | 318 MB | 1.8 GB | 1.9 GB |
| axum 0.8.6 | Add a function | 400 MB | 1.9 GB | 232 MB | 1.8 GB | 1.9 GB |
| axum 0.8.6 | Nothing changed | 83 MB | 2.0 GB | 83 MB | 2.0 GB | 2.0 GB |
| nushell 0.108.0 | Cold, until the daemon is ready | 4.5 GB | 7.5 GB | 2.0 GB | 5.5 GB | 5.2 GB |
| nushell 0.108.0 | Edit a function | 2.4 GB | 7.6 GB | 2.0 GB | 7.5 GB | 7.5 GB |
| nushell 0.108.0 | Edit a widely used function | 2.3 GB | 7.5 GB | 2.0 GB | 7.4 GB | 7.5 GB |
| nushell 0.108.0 | Change a signature | 2.3 GB | 7.5 GB | 2.0 GB | 7.4 GB | 7.5 GB |
| nushell 0.108.0 | Edit a generic function | 2.4 GB | 7.6 GB | 2.0 GB | 7.4 GB | 7.5 GB |
| nushell 0.108.0 | Add a function | 2.4 GB | 7.6 GB | 2.0 GB | 7.5 GB | 7.5 GB |
| nushell 0.108.0 | Add a struct field | 2.4 GB | 7.6 GB | 2.0 GB | 7.4 GB | 7.5 GB |
| nushell 0.108.0 | Nothing changed | 126 MB | 8.0 GB | 126 MB | 7.9 GB | 8.0 GB |
| helix 25.07.1 | Cold, until the daemon is ready | 3.5 GB | 5.8 GB | 1.5 GB | 4.1 GB | 3.7 GB |
| helix 25.07.1 | Edit a function | 1.8 GB | 6.6 GB | 1.5 GB | 6.6 GB | 6.6 GB |
| helix 25.07.1 | Edit a widely used function | 1.8 GB | 6.6 GB | 1.5 GB | 6.5 GB | 6.6 GB |
| helix 25.07.1 | Change a signature | 1.7 GB | 6.6 GB | 1.5 GB | 6.5 GB | 6.6 GB |
| helix 25.07.1 | Add a function | 1.8 GB | 6.6 GB | 1.5 GB | 6.5 GB | 6.6 GB |
| helix 25.07.1 | Add a struct field | 1.8 GB | 6.6 GB | 1.5 GB | 6.5 GB | 6.6 GB |
| helix 25.07.1 | Nothing changed | 148 MB | 10.2 GB | 148 MB | 10.1 GB | 10.2 GB |
| bevy 0.20.0 | Cold, until the daemon is ready | 6.8 GB | 9.3 GB | 6.3 GB | 9.2 GB | 8.1 GB |
| bevy 0.20.0 | Edit a function | 6.7 GB | 17.8 GB | 6.3 GB | 17.6 GB | 17.7 GB |
| bevy 0.20.0 | Edit a widely used function | 6.7 GB | 17.8 GB | 6.3 GB | 17.6 GB | 17.7 GB |
| bevy 0.20.0 | Add a function | 6.7 GB | 17.8 GB | 6.3 GB | 17.6 GB | 17.7 GB |
| bevy 0.20.0 | Nothing changed | 240 MB | 32.9 GB | 240 MB | 32.7 GB | 33.0 GB |

The daemon between builds, and what each side keeps on disk once the edits are done (each block once, [how](#memory-and-disk)). After the first build: once the step after the cold build (nothing changed) is done; growth is from then to the end, over every edit made and taken back. Warm store: the cold start of another checkout with the store the first one filled.

| Project | Daemon ready | After the first build | At the end | Growth | Warm store: peak, ready | cargo target | ra-jit target | Store | Home |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | 1.5 GB | 1.7 GB | 1.6 GB after 65 builds | −130 MB | 2.5 GB, 1.6 GB | 404 MB | 502 MB | 16 MB | 0 MB |
| fd 10.3.0 | 1.7 GB | 1.6 GB | 1.5 GB after 55 builds | −139 MB | 3.2 GB, 1.5 GB | 678 MB | 786 MB | 15 MB | 0 MB |
| bat 0.26.0 | 1.9 GB | 2.0 GB | 1.9 GB after 55 builds | −97 MB | 5.8 GB, 1.8 GB | 980 MB | 1.2 GB | 28 MB | 0 MB |
| hyperfine 1.19.0 | 1.9 GB | 1.8 GB | 1.7 GB after 55 builds | −98 MB | 2.5 GB, 1.9 GB | 340 MB | 418 MB | 14 MB | 0 MB |
| just 1.43.0 | 1.8 GB | 2.1 GB | 2.0 GB after 55 builds | −101 MB | 3.6 GB, 1.8 GB | 664 MB | 764 MB | 20 MB | 0 MB |
| axum 0.8.6 | 2.0 GB | 2.0 GB | 1.9 GB after 35 builds | −124 MB | 3.0 GB, 1.9 GB | 405 MB | 439 MB | 15 MB | 0 MB |
| nushell 0.108.0 | 5.2 GB | 8.0 GB | 7.5 GB after 65 builds | −471 MB | 9.5 GB, 5.0 GB | 5.3 GB | 5.5 GB | 81 MB | 1 MB |
| helix 25.07.1 | 3.7 GB | 10.2 GB | 6.6 GB after 55 builds | −3.7 GB | 6.1 GB, 3.7 GB | 3.7 GB | 4.3 GB | 357 MB | 1 MB |
| bevy 0.20.0 | 8.1 GB | 33.0 GB | 17.7 GB after 35 builds | −15.3 GB | 8.7 GB, 7.3 GB | 17.5 GB | 15.7 GB | 241 MB | 1 MB |

#### How fast the program runs, Cranelift, Linux aarch64

The same command run by the program ra-jit linked and by cargo's debug and release builds, in turn. Median, then fastest and slowest; where measured, then the program's peak resident memory while it ran (median of the runs, as the kernel kept it) and the size of the program.

| Project | Command | ra-jit | cargo debug | cargo release |
|---|---|---:|---:|---:|
| ripgrep 15.1.0 | `rg -uuu --sort path -c \w+_unchecked\(\|unsafe \{ <the toolchain's library source>` | 1.10 s (1.10 s to 1.10 s); 50 MB, 200.1 MB | 1.08 s (1.08 s to 1.09 s); 50 MB, 51.4 MB | 54 ms (54 ms to 54 ms); 50 MB, 31.6 MB |

#### Edits, LLVM, Linux aarch64

Median, then the fastest and slowest run. Built: who built the edit (daemon, or cargo and why). Link: the change was spliced into the program, or it was linked again.

| Project | Edit | cargo | ra-jit | | cargo runs | ra-jit runs | Built | Link |
|---|---|---:|---:|---:|---|---|---|---|
| ripgrep 15.1.0 | Edit a function | 1.20 s | 72 ms | 17x | 1.20 s to 1.21 s | 71 ms to 88 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a widely used function | 1.56 s | 67 ms | 23x | 1.56 s to 1.57 s | 66 ms to 105 ms | daemon | spliced |
| ripgrep 15.1.0 | Change a signature | 1.36 s | 116 ms | 12x | 1.36 s to 1.38 s | 115 ms to 152 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a generic function | 1.53 s | 66 ms | 23x | 1.53 s to 1.57 s | 66 ms to 67 ms | daemon | spliced |
| ripgrep 15.1.0 | Add a function | 1.21 s | 78 ms | 15x | 1.20 s to 1.21 s | 77 ms to 78 ms | daemon | spliced |
| ripgrep 15.1.0 | Add a struct field | 1.72 s | 75 ms | 23x | 1.72 s to 1.77 s | 75 ms to 84 ms | daemon | spliced |
| ripgrep 15.1.0 | Nothing changed | 25 ms | 28 ms | 0.9x | 25 ms to 25 ms | 27 ms to 29 ms | daemon |  |
| fd 10.3.0 | Edit a function | 1.28 s | 69 ms | 19x | 1.28 s to 1.29 s | 66 ms to 142 ms | daemon | spliced |
| fd 10.3.0 | Edit a widely used function | 1.24 s | 49 ms | 25x | 1.23 s to 1.26 s | 47 ms to 56 ms | daemon | spliced |
| fd 10.3.0 | Change a signature | 1.30 s | 74 ms | 18x | 1.29 s to 1.30 s | 73 ms to 188 ms | daemon | spliced |
| fd 10.3.0 | Add a function | 1.28 s | 72 ms | 18x | 1.27 s to 1.29 s | 71 ms to 106 ms | daemon | spliced |
| fd 10.3.0 | Add a struct field | 1.55 s | 61 ms | 25x | 1.54 s to 1.56 s | 61 ms to 68 ms | daemon | spliced |
| fd 10.3.0 | Nothing changed | 34 ms | 27 ms | 1.3x | 34 ms to 34 ms | 27 ms to 27 ms | daemon |  |
| bat 0.26.0 | Edit a function | 1.62 s | 65 ms | 25x | 1.62 s to 1.63 s | 64 ms to 84 ms | daemon | spliced |
| bat 0.26.0 | Edit a widely used function | 1.93 s | 66 ms | 29x | 1.92 s to 1.94 s | 65 ms to 67 ms | daemon | spliced |
| bat 0.26.0 | Change a signature | 1.87 s | 67 ms | 28x | 1.87 s to 1.90 s | 66 ms to 97 ms | daemon | spliced |
| bat 0.26.0 | Add a function | 1.62 s | 71 ms | 23x | 1.61 s to 1.63 s | 70 ms to 72 ms | daemon | spliced |
| bat 0.26.0 | Add a struct field | 2.23 s | 71 ms | 31x | 2.21 s to 2.27 s | 70 ms to 73 ms | daemon | spliced |
| bat 0.26.0 | Nothing changed | 65 ms | 27 ms | 2.4x | 65 ms to 66 ms | 27 ms to 28 ms | daemon |  |
| hyperfine 1.19.0 | Edit a function | 746 ms | 56 ms | 13x | 743 ms to 751 ms | 56 ms to 58 ms | daemon | spliced |
| hyperfine 1.19.0 | Edit a widely used function | 794 ms | 68 ms | 12x | 785 ms to 796 ms | 67 ms to 69 ms | daemon | spliced |
| hyperfine 1.19.0 | Change a signature | 796 ms | 104 ms | 7.7x | 793 ms to 810 ms | 103 ms to 112 ms | daemon | spliced |
| hyperfine 1.19.0 | Add a function | 749 ms | 59 ms | 13x | 748 ms to 765 ms | 58 ms to 61 ms | daemon | spliced |
| hyperfine 1.19.0 | Add a struct field | 877 ms | 87 ms | 10x | 874 ms to 890 ms | 85 ms to 88 ms | daemon | spliced |
| hyperfine 1.19.0 | Nothing changed | 44 ms | 26 ms | 1.7x | 44 ms to 44 ms | 26 ms to 27 ms | daemon |  |
| just 1.43.0 | Edit a function | 1.50 s | 53 ms | 28x | 1.50 s to 1.51 s | 51 ms to 67 ms | daemon | spliced |
| just 1.43.0 | Edit a widely used function | 1.53 s | 80 ms | 19x | 1.52 s to 1.54 s | 80 ms to 81 ms | daemon | spliced |
| just 1.43.0 | Change a signature | 1.51 s | 103 ms | 15x | 1.50 s to 1.52 s | 102 ms to 107 ms | daemon | spliced |
| just 1.43.0 | Add a function | 1.63 s | 72 ms | 23x | 1.60 s to 1.63 s | 71 ms to 73 ms | daemon | spliced |
| just 1.43.0 | Add a struct field | 2.97 s | 140 ms | 21x | 2.96 s to 3.13 s | 137 ms to 202 ms | daemon | spliced |
| just 1.43.0 | Nothing changed | 43 ms | 26 ms | 1.7x | 43 ms to 43 ms | 25 ms to 27 ms | daemon |  |
| axum 0.8.6 | Edit a function | 805 ms | 62 ms | 13x | 801 ms to 819 ms | 60 ms to 64 ms | daemon | spliced |
| axum 0.8.6 | Edit a widely used function | 1.37 s | 52 ms | 26x | 1.36 s to 1.38 s | 52 ms to 61 ms | daemon | spliced |
| axum 0.8.6 | Add a function | 814 ms | 62 ms | 13x | 805 ms to 817 ms | 61 ms to 66 ms | daemon | spliced |
| axum 0.8.6 | Nothing changed | 74 ms | 27 ms | 2.7x | 73 ms to 74 ms | 27 ms to 28 ms | daemon |  |
| nushell 0.108.0 | Edit a function | 7.18 s | 332 ms | 22x | 7.17 s to 7.22 s | 330 ms to 508 ms | daemon | spliced |
| nushell 0.108.0 | Edit a widely used function | 11.30 s | 291 ms | 39x | 11.28 s to 11.41 s | 290 ms to 298 ms | daemon | spliced |
| nushell 0.108.0 | Change a signature | 11.36 s | 349 ms | 33x | 11.25 s to 11.41 s | 343 ms to 640 ms | daemon | spliced |
| nushell 0.108.0 | Edit a generic function | 13.40 s | 457 ms | 29x | 13.39 s to 13.71 s | 447 ms to 490 ms | daemon | spliced |
| nushell 0.108.0 | Add a function | 7.18 s | 335 ms | 21x | 7.15 s to 7.19 s | 334 ms to 356 ms | daemon | spliced |
| nushell 0.108.0 | Add a struct field | 15.40 s | 367 ms | 42x | 15.34 s to 15.62 s | 362 ms to 387 ms | daemon | spliced |
| nushell 0.108.0 | Nothing changed | 184 ms | 30 ms | 6.1x | 183 ms to 185 ms | 30 ms to 5.70 s | daemon |  |
| helix 25.07.1 | Edit a function | 6.89 s | 91 ms | 76x | 6.85 s to 6.94 s | 90 ms to 59.30 s | daemon | full link: no executable to add to |
| helix 25.07.1 | Edit a widely used function | 8.26 s | 104 ms | 79x | 8.19 s to 8.27 s | 103 ms to 105 ms | daemon | spliced |
| helix 25.07.1 | Change a signature | 7.64 s | 275 ms | 28x | 7.62 s to 7.69 s | 264 ms to 466 ms | daemon | spliced |
| helix 25.07.1 | Add a function | 6.85 s | 145 ms | 47x | 6.83 s to 6.91 s | 142 ms to 154 ms | daemon | spliced |
| helix 25.07.1 | Add a struct field | 14.98 s | 395 ms | 38x | 14.95 s to 15.58 s | 395 ms to 397 ms | daemon | spliced |
| helix 25.07.1 | Nothing changed | 93 ms | 27 ms | 3.4x | 92 ms to 93 ms | 27 ms to 91 ms | daemon |  |
| bevy 0.20.0 | Edit a function | 22.79 s | 875 ms | 26x | 22.54 s to 23.64 s | 848 ms to 1.23 s | daemon | spliced |
| bevy 0.20.0 | Edit a widely used function | 32.11 s | 351 ms | 91x | 31.81 s to 32.47 s | 349 ms to 440 ms | daemon | spliced |
| bevy 0.20.0 | Add a function | 22.99 s | 899 ms | 26x | 22.59 s to 23.27 s | 896 ms to 1.12 s | daemon | spliced |
| bevy 0.20.0 | Nothing changed | 209 ms | 32 ms | 6.5x | 206 ms to 229 ms | 32 ms to 104 ms | daemon |  |

#### Cold, LLVM, Linux aarch64

From an empty target directory, crates already downloaded, median of 5 runs, each from nothing. The empty store fetches the standard library as a release's first build does. `cargo ra-jit build` is cargo's own build (the daemon is not loaded yet); the daemon then loads the workspace and builds it once, and edits are fast from then on. Warm store: the same in another checkout of the same commit, with the store the first one filled.

| Project | `cargo build` | `cargo ra-jit build` | Daemon ready, empty store | Daemon ready, warm store | Functions compiled | Daemon memory | Store |
|---|---:|---:|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | 6.52 s (6.44 s to 6.63 s) | 6.49 s (6.48 s to 6.51 s) | 7.08 s (7.06 s to 7.09 s) | 6.79 s (6.75 s to 6.82 s) | 0 | 2,194 MB | 17 MB |
| fd 10.3.0 | 33.05 s (33.02 s to 33.20 s) | 32.54 s (32.42 s to 32.73 s) | 33.40 s (33.28 s to 33.59 s) | 32.85 s (32.82 s to 33.25 s) | 0 | 1,991 MB | 17 MB |
| bat 0.26.0 | 13.86 s (13.59 s to 14.04 s) | 13.12 s (13.02 s to 13.20 s) | 15.09 s (14.98 s to 15.16 s) | 14.04 s (14.00 s to 14.53 s) | 0 | 2,410 MB | 30 MB |
| hyperfine 1.19.0 | 6.34 s (6.30 s to 6.39 s) | 6.47 s (6.45 s to 6.49 s) | 7.33 s (7.31 s to 7.35 s) | 6.80 s (6.74 s to 7.30 s) | 0 | 2,149 MB | 15 MB |
| just 1.43.0 | 9.83 s (9.67 s to 10.01 s) | 9.12 s (9.11 s to 9.27 s) | 10.52 s (10.52 s to 10.68 s) | 9.73 s (9.66 s to 10.23 s) | 0 | 2,524 MB | 21 MB |
| axum 0.8.6 | 9.06 s (8.99 s to 9.09 s) | 9.31 s (9.28 s to 9.34 s) | 11.27 s (11.25 s to 11.30 s) | 10.24 s (10.21 s to 11.05 s) | 0 | 2,495 MB | 17 MB |
| nushell 0.108.0 | 51.20 s (50.53 s to 51.30 s) | 42.29 s (42.11 s to 42.50 s) | 48.32 s (47.93 s to 48.39 s) | 44.70 s (44.64 s to 47.08 s) | 0 | 11,293 MB | 78 MB |
| helix 25.07.1 | 44.90 s (44.55 s to 45.22 s) | 38.08 s (37.78 s to 38.19 s) | 41.42 s (41.12 s to 41.53 s) | 41.09 s (41.00 s to 41.28 s) | 0 | 9,515 MB | 352 MB |
| bevy 0.20.0 | 123.18 s (122.06 s to 123.66 s) | 101.55 s (100.65 s to 101.68 s) | 126.12 s (125.16 s to 133.06 s) | 124.08 s (123.64 s to 131.58 s) | 0 | 16,409 MB | 305 MB |

#### Memory and disk, LLVM, Linux aarch64

Peak: the most every process doing the work held at once, sampled every 10 ms and never below the largest process, median of the runs. Largest process: the peak of the one process that held the most, as the kernel kept it: `wait4`'s for the command's tree, and each process's peak sampled (on macOS the kernel's peak footprint of each, counting for the daemon only a peak it passed during the step). ra-jit is the daemon, its workers and what they run, and the `cargo ra-jit build` with any cargo it ran; cargo is cargo itself, rustc, build scripts and the linker. Daemon after: what the daemon and its workers hold once the step is over ([how](#memory-and-disk)).

| Project | Step | cargo, peak | ra-jit, peak | cargo, largest process | ra-jit, largest process | Daemon after |
|---|---|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | Cold, until the daemon is ready | 2.0 GB | 4.0 GB | 413 MB | 1.8 GB | 1.7 GB |
| ripgrep 15.1.0 | Edit a function | 464 MB | 1.8 GB | 273 MB | 1.7 GB | 1.8 GB |
| ripgrep 15.1.0 | Edit a widely used function | 489 MB | 1.8 GB | 285 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Change a signature | 464 MB | 1.8 GB | 271 MB | 1.7 GB | 1.8 GB |
| ripgrep 15.1.0 | Edit a generic function | 485 MB | 1.8 GB | 283 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Add a function | 465 MB | 1.8 GB | 276 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Add a struct field | 497 MB | 1.8 GB | 329 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Nothing changed | 50 MB | 1.9 GB | 50 MB | 1.8 GB | 1.9 GB |
| fd 10.3.0 | Cold, until the daemon is ready | 2.3 GB | 3.4 GB | 592 MB | 1.8 GB | 1.8 GB |
| fd 10.3.0 | Edit a function | 520 MB | 1.7 GB | 304 MB | 1.5 GB | 1.6 GB |
| fd 10.3.0 | Edit a widely used function | 513 MB | 1.7 GB | 304 MB | 1.5 GB | 1.6 GB |
| fd 10.3.0 | Change a signature | 523 MB | 1.7 GB | 304 MB | 1.5 GB | 1.6 GB |
| fd 10.3.0 | Add a function | 521 MB | 1.7 GB | 304 MB | 1.5 GB | 1.6 GB |
| fd 10.3.0 | Add a struct field | 548 MB | 1.7 GB | 304 MB | 1.5 GB | 1.6 GB |
| fd 10.3.0 | Nothing changed | 57 MB | 1.8 GB | 57 MB | 1.6 GB | 1.7 GB |
| bat 0.26.0 | Cold, until the daemon is ready | 3.3 GB | 7.0 GB | 460 MB | 2.2 GB | 1.9 GB |
| bat 0.26.0 | Edit a function | 672 MB | 2.2 GB | 460 MB | 2.0 GB | 2.1 GB |
| bat 0.26.0 | Edit a widely used function | 670 MB | 2.2 GB | 460 MB | 2.0 GB | 2.1 GB |
| bat 0.26.0 | Change a signature | 655 MB | 2.2 GB | 460 MB | 2.0 GB | 2.1 GB |
| bat 0.26.0 | Add a function | 672 MB | 2.2 GB | 460 MB | 2.0 GB | 2.1 GB |
| bat 0.26.0 | Add a struct field | 688 MB | 2.2 GB | 460 MB | 2.0 GB | 2.1 GB |
| bat 0.26.0 | Nothing changed | 79 MB | 2.2 GB | 79 MB | 2.1 GB | 2.2 GB |
| hyperfine 1.19.0 | Cold, until the daemon is ready | 2.1 GB | 2.7 GB | 345 MB | 2.1 GB | 2.1 GB |
| hyperfine 1.19.0 | Edit a function | 361 MB | 1.9 GB | 222 MB | 1.8 GB | 1.9 GB |
| hyperfine 1.19.0 | Edit a widely used function | 370 MB | 1.9 GB | 238 MB | 1.7 GB | 1.8 GB |
| hyperfine 1.19.0 | Change a signature | 371 MB | 1.9 GB | 241 MB | 1.7 GB | 1.8 GB |
| hyperfine 1.19.0 | Add a function | 361 MB | 1.9 GB | 224 MB | 1.7 GB | 1.8 GB |
| hyperfine 1.19.0 | Add a struct field | 386 MB | 1.9 GB | 270 MB | 1.7 GB | 1.8 GB |
| hyperfine 1.19.0 | Nothing changed | 79 MB | 1.9 GB | 79 MB | 1.8 GB | 1.9 GB |
| just 1.43.0 | Cold, until the daemon is ready | 3.1 GB | 4.2 GB | 605 MB | 2.3 GB | 1.9 GB |
| just 1.43.0 | Edit a function | 434 MB | 2.1 GB | 320 MB | 2.0 GB | 2.1 GB |
| just 1.43.0 | Edit a widely used function | 422 MB | 2.1 GB | 344 MB | 1.9 GB | 2.0 GB |
| just 1.43.0 | Change a signature | 422 MB | 2.1 GB | 342 MB | 1.9 GB | 2.0 GB |
| just 1.43.0 | Add a function | 432 MB | 2.1 GB | 360 MB | 1.9 GB | 2.0 GB |
| just 1.43.0 | Add a struct field | 638 MB | 2.1 GB | 594 MB | 2.0 GB | 2.0 GB |
| just 1.43.0 | Nothing changed | 81 MB | 2.2 GB | 81 MB | 2.1 GB | 2.2 GB |
| axum 0.8.6 | Cold, until the daemon is ready | 2.1 GB | 3.2 GB | 506 MB | 2.5 GB | 2.2 GB |
| axum 0.8.6 | Edit a function | 399 MB | 2.2 GB | 230 MB | 2.0 GB | 2.1 GB |
| axum 0.8.6 | Edit a widely used function | 397 MB | 2.2 GB | 319 MB | 2.0 GB | 2.1 GB |
| axum 0.8.6 | Add a function | 400 MB | 2.2 GB | 234 MB | 2.0 GB | 2.1 GB |
| axum 0.8.6 | Nothing changed | 83 MB | 2.2 GB | 83 MB | 2.1 GB | 2.2 GB |
| nushell 0.108.0 | Cold, until the daemon is ready | 4.6 GB | 7.8 GB | 2.0 GB | 5.5 GB | 5.2 GB |
| nushell 0.108.0 | Edit a function | 2.4 GB | 7.5 GB | 2.0 GB | 7.4 GB | 7.5 GB |
| nushell 0.108.0 | Edit a widely used function | 2.3 GB | 7.5 GB | 2.0 GB | 7.3 GB | 7.4 GB |
| nushell 0.108.0 | Change a signature | 2.4 GB | 7.5 GB | 2.0 GB | 7.3 GB | 7.4 GB |
| nushell 0.108.0 | Edit a generic function | 2.4 GB | 7.5 GB | 2.0 GB | 7.3 GB | 7.4 GB |
| nushell 0.108.0 | Add a function | 2.4 GB | 7.5 GB | 2.0 GB | 7.3 GB | 7.5 GB |
| nushell 0.108.0 | Add a struct field | 2.4 GB | 7.5 GB | 2.0 GB | 7.3 GB | 7.5 GB |
| nushell 0.108.0 | Nothing changed | 126 MB | 8.4 GB | 126 MB | 8.2 GB | 8.3 GB |
| helix 25.07.1 | Cold, until the daemon is ready | 3.4 GB | 6.0 GB | 1.5 GB | 4.2 GB | 3.8 GB |
| helix 25.07.1 | Edit a function | 1.8 GB | 6.7 GB | 1.5 GB | 6.5 GB | 6.6 GB |
| helix 25.07.1 | Edit a widely used function | 1.8 GB | 6.6 GB | 1.5 GB | 6.5 GB | 6.6 GB |
| helix 25.07.1 | Change a signature | 1.7 GB | 6.7 GB | 1.5 GB | 6.5 GB | 6.6 GB |
| helix 25.07.1 | Add a function | 1.8 GB | 6.7 GB | 1.5 GB | 6.5 GB | 6.6 GB |
| helix 25.07.1 | Add a struct field | 1.8 GB | 6.7 GB | 1.5 GB | 6.5 GB | 6.6 GB |
| helix 25.07.1 | Nothing changed | 159 MB | 9.6 GB | 159 MB | 9.5 GB | 9.6 GB |
| bevy 0.20.0 | Cold, until the daemon is ready | 6.8 GB | 9.5 GB | 6.3 GB | 9.3 GB | 8.2 GB |
| bevy 0.20.0 | Edit a function | 6.7 GB | 17.7 GB | 6.3 GB | 17.5 GB | 17.6 GB |
| bevy 0.20.0 | Edit a widely used function | 6.7 GB | 17.7 GB | 6.3 GB | 17.4 GB | 17.6 GB |
| bevy 0.20.0 | Add a function | 6.7 GB | 17.7 GB | 6.3 GB | 17.5 GB | 17.6 GB |
| bevy 0.20.0 | Nothing changed | 320 MB | 16.2 GB | 320 MB | 16.0 GB | 16.2 GB |

The daemon between builds, and what each side keeps on disk once the edits are done (each block once, [how](#memory-and-disk)). After the first build: once the step after the cold build (nothing changed) is done; growth is from then to the end, over every edit made and taken back. Warm store: the cold start of another checkout with the store the first one filled.

| Project | Daemon ready | After the first build | At the end | Growth | Warm store: peak, ready | cargo target | ra-jit target | Store | Home |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | 1.7 GB | 1.9 GB | 1.7 GB after 65 builds | −148 MB | 3.2 GB, 1.8 GB | 404 MB | 502 MB | 15 MB | 1 MB |
| fd 10.3.0 | 1.8 GB | 1.7 GB | 1.6 GB after 55 builds | −88 MB | 3.4 GB, 1.6 GB | 678 MB | 784 MB | 15 MB | 1 MB |
| bat 0.26.0 | 1.9 GB | 2.2 GB | 2.1 GB after 55 builds | −62 MB | 5.8 GB, 1.8 GB | 980 MB | 1.2 GB | 28 MB | 0 MB |
| hyperfine 1.19.0 | 2.1 GB | 1.9 GB | 1.8 GB after 55 builds | −95 MB | 2.7 GB, 2.1 GB | 340 MB | 417 MB | 13 MB | 0 MB |
| just 1.43.0 | 1.9 GB | 2.2 GB | 2.0 GB after 55 builds | −164 MB | 3.7 GB, 1.8 GB | 664 MB | 760 MB | 19 MB | 0 MB |
| axum 0.8.6 | 2.2 GB | 2.2 GB | 2.1 GB after 35 builds | −116 MB | 3.1 GB, 2.0 GB | 405 MB | 439 MB | 15 MB | 0 MB |
| nushell 0.108.0 | 5.2 GB | 8.3 GB | 7.5 GB after 65 builds | −889 MB | 8.4 GB, 5.0 GB | 5.3 GB | 5.5 GB | 75 MB | 1 MB |
| helix 25.07.1 | 3.8 GB | 9.6 GB | 6.6 GB after 55 builds | −3.1 GB | 6.1 GB, 3.8 GB | 3.7 GB | 4.3 GB | 348 MB | 1 MB |
| bevy 0.20.0 | 8.2 GB | 16.2 GB | 17.6 GB after 35 builds | +1.4 GB | 8.8 GB, 7.5 GB | 17.5 GB | 15.7 GB | 221 MB | 1 MB |

#### How fast the program runs, LLVM, Linux aarch64

The same command run by the program ra-jit linked and by cargo's debug and release builds, in turn. Median, then fastest and slowest; where measured, then the program's peak resident memory while it ran (median of the runs, as the kernel kept it) and the size of the program.

| Project | Command | ra-jit | cargo debug | cargo release |
|---|---|---:|---:|---:|
| ripgrep 15.1.0 | `rg -uuu --sort path -c \w+_unchecked\(\|unsafe \{ <the toolchain's library source>` | 1.09 s (1.09 s to 1.09 s); 57 MB, 197.9 MB | 1.08 s (1.08 s to 1.08 s); 57 MB, 51.4 MB | 54 ms (54 ms to 55 ms); 57 MB, 31.6 MB |

### Linux aarch64, bun

ra-jit [`ac1ee2dd68`](https://github.com/indexable-inc/ra-jit/commit/ac1ee2dd682b7d321ba75a67924aaf95609572f0), 2026-10-23T08:43:38Z, Rust 1.99.0, 3 runs of each edit, measured by hand. Raw: [benchmarks/bun-linux-aarch64.json](benchmarks/bun-linux-aarch64.json).

| Machine | |
|---|---|
| Name | AWS c8g.24xlarge (dev-aws-eph-rajit-arm64bun), pinned to 16 cores with taskset |
| CPU | Neoverse-V2 |
| Cores | 16 |
| Memory (GiB) | 185 |
| OS | Linux |
| Kernel | 6.18.53 |
| rustc | rustc 1.100.0-nightly (574ff7d98 2026-09-14) |
| C compiler | gcc (GCC) 16.2.0 |
| Linker | GNU ld (GNU Binutils) 2.46 |
| LLVM | 23.1.3 |
| Incremental | profile default (on for dev) |
| Compiler cache | none |
| Load average at the start | 6.9, 5.2, 3.0 |

#### Edits, Cranelift, Linux aarch64, bun

Median, then the fastest and slowest run. Built: who built the edit (daemon, or cargo and why). Link: the change was spliced into the program, or it was linked again.

| Project | Edit | cargo | ra-jit | | cargo runs | ra-jit runs | Built | Link |
|---|---|---:|---:|---:|---|---|---|---|
| bun 1.4.3 | Edit a function | 10.63 s | 486 ms | 22x | 10.59 s to 10.65 s | 483 ms to 497 ms | daemon | spliced |
| bun 1.4.3 | Edit a widely used function | 19.64 s | 477 ms | 41x | 19.63 s to 19.66 s | 477 ms to 480 ms | daemon | spliced |
| bun 1.4.3 | Change a signature | 10.67 s | 629 ms | 17x | 10.66 s to 10.68 s | 611 ms to 725 ms | daemon | spliced |
| bun 1.4.3 | Edit a generic function | 19.97 s | 503 ms | 40x | 19.96 s to 20.03 s | 501 ms to 508 ms | daemon | spliced |
| bun 1.4.3 | Add a function | 14.75 s | 735 ms | 20x | 14.74 s to 15.13 s | 731 ms to 738 ms | daemon | spliced |
| bun 1.4.3 | Nothing changed | 90 ms | 21 ms | 4.3x | 90 ms to 90 ms | 21 ms to 22 ms | daemon |  |

#### Cold, Cranelift, Linux aarch64, bun

From an empty target directory, crates already downloaded, one run. The empty store fetches the standard library as a release's first build does. `cargo ra-jit build` is cargo's own build (the daemon is not loaded yet); the daemon then loads the workspace and builds it once, and edits are fast from then on. Warm store: the same in another checkout of the same commit, with the store the first one filled.

| Project | `cargo build` | `cargo ra-jit build` | Daemon ready, empty store | Daemon ready, warm store | Functions compiled | Daemon memory | Store |
|---|---:|---:|---:|---:|---:|---:|---:|
| bun 1.4.3 | 89.03 s | 87.68 s | 91.44 s | 90.72 s | 0 | 36,189 MB | 92 MB |

#### Memory and disk, Cranelift, Linux aarch64, bun

Peak: the most every process doing the work held at once, sampled every 10 ms and never below the largest process, median of the runs. Largest process: the peak of the one process that held the most, as the kernel kept it: `wait4`'s for the command's tree, and each process's peak sampled (on macOS the kernel's peak footprint of each, counting for the daemon only a peak it passed during the step). ra-jit is the daemon, its workers and what they run, and the `cargo ra-jit build` with any cargo it ran; cargo is cargo itself, rustc, build scripts and the linker. Daemon after: what the daemon and its workers hold once the step is over ([how](#memory-and-disk)).

| Project | Step | cargo, peak | ra-jit, peak | cargo, largest process | ra-jit, largest process | Daemon after |
|---|---|---:|---:|---:|---:|---:|
| bun 1.4.3 | Cold, until the daemon is ready | 4.5 GB | 15.8 GB | 3.2 GB | 12.2 GB | 12.3 GB |
| bun 1.4.3 | Edit a function | 3.5 GB | 27.7 GB | 3.2 GB | 27.6 GB | 27.7 GB |
| bun 1.4.3 | Edit a widely used function | 3.6 GB | 27.7 GB | 3.2 GB | 27.6 GB | 27.7 GB |
| bun 1.4.3 | Change a signature | 3.5 GB | 27.7 GB | 3.2 GB | 27.6 GB | 27.7 GB |
| bun 1.4.3 | Edit a generic function | 3.6 GB | 27.7 GB | 3.2 GB | 27.6 GB | 27.7 GB |
| bun 1.4.3 | Add a function | 3.5 GB | 27.7 GB | 3.2 GB | 27.6 GB | 27.7 GB |
| bun 1.4.3 | Nothing changed | 60 MB | 27.7 GB | 60 MB | 27.6 GB | 27.7 GB |

The daemon between builds, and what each side keeps on disk once the edits are done (each block once, [how](#memory-and-disk)). After the first build: once the step after the cold build (nothing changed) is done; growth is from then to the end, over every edit made and taken back. Warm store: the cold start of another checkout with the store the first one filled.

| Project | Daemon ready | After the first build | At the end | Growth | Warm store: peak, ready | cargo target | ra-jit target | Store | Home |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| bun 1.4.3 | 12.3 GB | 27.7 GB | 27.7 GB after 33 builds | +55 MB | 15.9 GB, 12.2 GB | 5.6 GB | 6.6 GB | 88 MB | 1 MB |

#### How fast the program runs, Cranelift, Linux aarch64, bun

The same command run by the program ra-jit linked and by cargo's debug and release builds, in turn. Median, then fastest and slowest; where measured, then the program's peak resident memory while it ran (median of the runs, as the kernel kept it) and the size of the program.

| Project | Command | ra-jit | cargo debug | cargo release |
|---|---|---:|---:|---:|
| bun 1.4.3 | `bun -e const t = new Bun.Transpiler({ loader: 'ts', minifyWhitespace: true }); const fs = require('fs'); const sources = ['http2', 'net', '_http_server', 'fs', 'child_process'].map(n => fs.readFileSync('src/js/node/' + n + '.ts', 'utf8')); let n = 0; for (let i = 0; i < 20; i++) for (const s of sources) n += t.transformSync(s).length; console.log(n)` | 3.51 s (3.51 s to 3.52 s); 105 MB, 601.0 MB | 2.03 s (2.03 s to 2.03 s); 105 MB, 408.4 MB | 189 ms (188 ms to 189 ms); 105 MB, 318.7 MB |

#### Edits, LLVM, Linux aarch64, bun

Median, then the fastest and slowest run. Built: who built the edit (daemon, or cargo and why). Link: the change was spliced into the program, or it was linked again.

| Project | Edit | cargo | ra-jit | | cargo runs | ra-jit runs | Built | Link |
|---|---|---:|---:|---:|---|---|---|---|
| bun 1.4.3 | Edit a function | 10.66 s | 493 ms | 22x | 10.65 s to 10.67 s | 490 ms to 508 ms | daemon | spliced |
| bun 1.4.3 | Edit a widely used function | 19.66 s | 483 ms | 41x | 19.59 s to 19.74 s | 481 ms to 494 ms | daemon | spliced |
| bun 1.4.3 | Change a signature | 10.72 s | 609 ms | 18x | 10.67 s to 10.74 s | 608 ms to 714 ms | daemon | spliced |
| bun 1.4.3 | Edit a generic function | 19.97 s | 518 ms | 39x | 19.92 s to 20.04 s | 518 ms to 529 ms | daemon | spliced |
| bun 1.4.3 | Add a function | 14.80 s | 711 ms | 21x | 14.79 s to 15.09 s | 710 ms to 721 ms | daemon | spliced |
| bun 1.4.3 | Nothing changed | 90 ms | 32 ms | 2.8x | 90 ms to 90 ms | 31 ms to 43 ms | daemon |  |

#### Cold, LLVM, Linux aarch64, bun

From an empty target directory, crates already downloaded, one run. The empty store fetches the standard library as a release's first build does. `cargo ra-jit build` is cargo's own build (the daemon is not loaded yet); the daemon then loads the workspace and builds it once, and edits are fast from then on. Warm store: the same in another checkout of the same commit, with the store the first one filled.

| Project | `cargo build` | `cargo ra-jit build` | Daemon ready, empty store | Daemon ready, warm store | Functions compiled | Daemon memory | Store |
|---|---:|---:|---:|---:|---:|---:|---:|
| bun 1.4.3 | 94.97 s | 89.03 s | 92.67 s | 90.46 s | 0 | 34,829 MB | 60 MB |

#### Memory and disk, LLVM, Linux aarch64, bun

Peak: the most every process doing the work held at once, sampled every 10 ms and never below the largest process, median of the runs. Largest process: the peak of the one process that held the most, as the kernel kept it: `wait4`'s for the command's tree, and each process's peak sampled (on macOS the kernel's peak footprint of each, counting for the daemon only a peak it passed during the step). ra-jit is the daemon, its workers and what they run, and the `cargo ra-jit build` with any cargo it ran; cargo is cargo itself, rustc, build scripts and the linker. Daemon after: what the daemon and its workers hold once the step is over ([how](#memory-and-disk)).

| Project | Step | cargo, peak | ra-jit, peak | cargo, largest process | ra-jit, largest process | Daemon after |
|---|---|---:|---:|---:|---:|---:|
| bun 1.4.3 | Cold, until the daemon is ready | 4.5 GB | 15.8 GB | 3.2 GB | 12.1 GB | 12.2 GB |
| bun 1.4.3 | Edit a function | 3.5 GB | 27.3 GB | 3.2 GB | 27.1 GB | 27.2 GB |
| bun 1.4.3 | Edit a widely used function | 3.6 GB | 27.3 GB | 3.2 GB | 27.1 GB | 27.2 GB |
| bun 1.4.3 | Change a signature | 3.5 GB | 27.3 GB | 3.2 GB | 27.1 GB | 27.2 GB |
| bun 1.4.3 | Edit a generic function | 3.5 GB | 27.3 GB | 3.2 GB | 27.1 GB | 27.2 GB |
| bun 1.4.3 | Add a function | 3.5 GB | 27.3 GB | 3.2 GB | 27.1 GB | 27.2 GB |
| bun 1.4.3 | Nothing changed | 105 MB | 27.2 GB | 105 MB | 27.1 GB | 27.2 GB |

The daemon between builds, and what each side keeps on disk once the edits are done (each block once, [how](#memory-and-disk)). After the first build: once the step after the cold build (nothing changed) is done; growth is from then to the end, over every edit made and taken back. Warm store: the cold start of another checkout with the store the first one filled.

| Project | Daemon ready | After the first build | At the end | Growth | Warm store: peak, ready | cargo target | ra-jit target | Store | Home |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| bun 1.4.3 | 12.2 GB | 27.2 GB | 27.2 GB after 33 builds | +38 MB | 15.7 GB, 11.6 GB | 5.6 GB | 6.4 GB | 56 MB | 1 MB |

#### How fast the program runs, LLVM, Linux aarch64, bun

The same command run by the program ra-jit linked and by cargo's debug and release builds, in turn. Median, then fastest and slowest; where measured, then the program's peak resident memory while it ran (median of the runs, as the kernel kept it) and the size of the program.

| Project | Command | ra-jit | cargo debug | cargo release |
|---|---|---:|---:|---:|
| bun 1.4.3 | `bun -e const t = new Bun.Transpiler({ loader: 'ts', minifyWhitespace: true }); const fs = require('fs'); const sources = ['http2', 'net', '_http_server', 'fs', 'child_process'].map(n => fs.readFileSync('src/js/node/' + n + '.ts', 'utf8')); let n = 0; for (let i = 0; i < 20; i++) for (const s of sources) n += t.transformSync(s).length; console.log(n)` | 1.90 s (1.89 s to 1.90 s); 131 MB, 538.2 MB | 2.03 s (2.03 s to 2.06 s); 131 MB, 408.4 MB | 189 ms (188 ms to 190 ms); 131 MB, 318.7 MB |

### Linux x86-64

ra-jit [`42cb85270f`](https://github.com/indexable-inc/ra-jit/commit/42cb85270f3822819deed0d8788b6f1180b4137a), 2026-10-24T02:03:22Z, Rust 1.99.0, 5 runs of each edit, measured by hand. Raw: [benchmarks/linux-x86-64.json](benchmarks/linux-x86-64.json).

| Machine | |
|---|---|
| Name | AWS c8a.metal-24xl (dev-aws-3), pinned to 16 cores with taskset |
| CPU | AMD EPYC 9R45 96-Core Processor |
| Cores | 16 |
| Memory (GiB) | 188 |
| OS | Linux |
| Kernel | 6.18.53 |
| rustc | rustc 1.99.0 (b940084d7 2026-09-28) |
| C compiler | gcc (GCC) 16.2.0 |
| Linker | GNU ld (GNU Binutils) 2.46 |
| LLVM | 20.1.8 |
| Incremental | profile default (on for dev) |
| Compiler cache | none |
| Load average at the start | 1.8, 14.6, 17.4 |

#### Edits, Cranelift, Linux x86-64

Median, then the fastest and slowest run. Built: who built the edit (daemon, or cargo and why). Link: the change was spliced into the program, or it was linked again.

| Project | Edit | cargo | ra-jit | | cargo runs | ra-jit runs | Built | Link |
|---|---|---:|---:|---:|---|---|---|---|
| ripgrep 15.1.0 | Edit a function | 497 ms | 56 ms | 8.9x | 492 ms to 499 ms | 54 ms to 64 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a widely used function | 758 ms | 50 ms | 15x | 740 ms to 771 ms | 50 ms to 84 ms | daemon | spliced |
| ripgrep 15.1.0 | Change a signature | 611 ms | 88 ms | 6.9x | 605 ms to 615 ms | 88 ms to 124 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a generic function | 746 ms | 50 ms | 15x | 730 ms to 759 ms | 49 ms to 50 ms | daemon | spliced |
| ripgrep 15.1.0 | Add a function | 487 ms | 66 ms | 7.4x | 475 ms to 491 ms | 64 ms to 68 ms | daemon | spliced |
| ripgrep 15.1.0 | Add a struct field | 878 ms | 60 ms | 15x | 872 ms to 922 ms | 58 ms to 73 ms | daemon | spliced |
| ripgrep 15.1.0 | Nothing changed | 22 ms | 16 ms | 1.4x | 22 ms to 23 ms | 15 ms to 16 ms | daemon |  |
| fd 10.3.0 | Edit a function | 490 ms | 52 ms | 9.4x | 480 ms to 495 ms | 51 ms to 63 ms | daemon | spliced |
| fd 10.3.0 | Edit a widely used function | 465 ms | 35 ms | 13x | 459 ms to 480 ms | 34 ms to 36 ms | daemon | spliced |
| fd 10.3.0 | Change a signature | 505 ms | 56 ms | 9.0x | 496 ms to 531 ms | 55 ms to 138 ms | daemon | spliced |
| fd 10.3.0 | Add a function | 482 ms | 54 ms | 8.9x | 480 ms to 488 ms | 54 ms to 58 ms | daemon | spliced |
| fd 10.3.0 | Add a struct field | 684 ms | 46 ms | 15x | 668 ms to 708 ms | 45 ms to 68 ms | daemon | spliced |
| fd 10.3.0 | Nothing changed | 31 ms | 15 ms | 2.1x | 30 ms to 32 ms | 15 ms to 15 ms | daemon |  |
| bat 0.26.0 | Edit a function | 490 ms | 49 ms | 10x | 481 ms to 497 ms | 49 ms to 67 ms | daemon | spliced |
| bat 0.26.0 | Edit a widely used function | 723 ms | 48 ms | 15x | 692 ms to 758 ms | 47 ms to 49 ms | daemon | spliced |
| bat 0.26.0 | Change a signature | 689 ms | 50 ms | 14x | 650 ms to 695 ms | 49 ms to 74 ms | daemon | spliced |
| bat 0.26.0 | Add a function | 497 ms | 56 ms | 8.9x | 492 ms to 507 ms | 55 ms to 57 ms | daemon | spliced |
| bat 0.26.0 | Add a struct field | 940 ms | 55 ms | 17x | 923 ms to 990 ms | 54 ms to 57 ms | daemon | spliced |
| bat 0.26.0 | Nothing changed | 59 ms | 15 ms | 3.9x | 59 ms to 60 ms | 15 ms to 16 ms | daemon |  |
| hyperfine 1.19.0 | Edit a function | 370 ms | 34 ms | 11x | 366 ms to 377 ms | 33 ms to 34 ms | daemon | spliced |
| hyperfine 1.19.0 | Edit a widely used function | 404 ms | 49 ms | 8.2x | 395 ms to 409 ms | 49 ms to 50 ms | daemon | spliced |
| hyperfine 1.19.0 | Change a signature | 418 ms | 75 ms | 5.6x | 417 ms to 420 ms | 73 ms to 82 ms | daemon | spliced |
| hyperfine 1.19.0 | Add a function | 375 ms | 36 ms | 10x | 369 ms to 381 ms | 35 ms to 42 ms | daemon | spliced |
| hyperfine 1.19.0 | Add a struct field | 480 ms | 63 ms | 7.6x | 466 ms to 484 ms | 59 ms to 65 ms | daemon | spliced |
| hyperfine 1.19.0 | Nothing changed | 41 ms | 15 ms | 2.7x | 41 ms to 41 ms | 14 ms to 15 ms | daemon |  |
| just 1.43.0 | Edit a function | 697 ms | 40 ms | 17x | 684 ms to 710 ms | 40 ms to 45 ms | daemon | spliced |
| just 1.43.0 | Edit a widely used function | 712 ms | 62 ms | 11x | 696 ms to 725 ms | 61 ms to 63 ms | daemon | spliced |
| just 1.43.0 | Change a signature | 706 ms | 74 ms | 9.5x | 674 ms to 710 ms | 73 ms to 78 ms | daemon | spliced |
| just 1.43.0 | Add a function | 788 ms | 59 ms | 13x | 774 ms to 800 ms | 58 ms to 60 ms | daemon | spliced |
| just 1.43.0 | Add a struct field | 1.82 s | 129 ms | 14x | 1.77 s to 1.94 s | 121 ms to 173 ms | daemon | spliced |
| just 1.43.0 | Nothing changed | 40 ms | 15 ms | 2.7x | 40 ms to 41 ms | 14 ms to 15 ms | daemon |  |
| axum 0.8.6 | Edit a function | 418 ms | 50 ms | 8.4x | 415 ms to 420 ms | 48 ms to 53 ms | daemon | spliced |
| axum 0.8.6 | Edit a widely used function | 864 ms | 44 ms | 20x | 818 ms to 875 ms | 43 ms to 47 ms | daemon | spliced |
| axum 0.8.6 | Add a function | 407 ms | 50 ms | 8.1x | 404 ms to 419 ms | 49 ms to 52 ms | daemon | spliced |
| axum 0.8.6 | Nothing changed | 72 ms | 16 ms | 4.5x | 70 ms to 75 ms | 15 ms to 20 ms | daemon |  |
| nushell 0.108.0 | Edit a function | 1.23 s | 262 ms | 4.7x | 1.19 s to 1.27 s | 253 ms to 281 ms | daemon | spliced |
| nushell 0.108.0 | Edit a widely used function | 4.74 s | 271 ms | 17x | 4.71 s to 4.77 s | 267 ms to 275 ms | daemon | spliced |
| nushell 0.108.0 | Change a signature | 4.77 s | 350 ms | 14x | 4.73 s to 4.81 s | 342 ms to 540 ms | daemon | spliced |
| nushell 0.108.0 | Edit a generic function | 6.37 s | 265 ms | 24x | 6.32 s to 6.65 s | 264 ms to 280 ms | daemon | spliced |
| nushell 0.108.0 | Add a function | 1.24 s | 266 ms | 4.7x | 1.19 s to 1.29 s | 253 ms to 293 ms | daemon | spliced |
| nushell 0.108.0 | Add a struct field | 8.11 s | 355 ms | 23x | 8.00 s to 8.22 s | 348 ms to 386 ms | daemon | spliced |
| nushell 0.108.0 | Nothing changed | 171 ms | 18 ms | 9.5x | 166 ms to 173 ms | 17 ms to 20 ms | daemon |  |
| helix 25.07.1 | Edit a function | 2.12 s | 79 ms | 27x | 2.11 s to 2.16 s | 78 ms to 78.08 s | daemon | full link: no executable to add to |
| helix 25.07.1 | Edit a widely used function | 3.25 s | 116 ms | 28x | 3.18 s to 3.30 s | 112 ms to 118 ms | daemon | spliced |
| helix 25.07.1 | Change a signature | 2.75 s | 275 ms | 10.0x | 2.70 s to 2.83 s | 262 ms to 640 ms | daemon | spliced |
| helix 25.07.1 | Add a function | 2.11 s | 144 ms | 15x | 2.06 s to 2.18 s | 132 ms to 148 ms | daemon | spliced |
| helix 25.07.1 | Add a struct field | 8.80 s | 253 ms | 35x | 8.57 s to 9.16 s | 238 ms to 259 ms | daemon | spliced |
| helix 25.07.1 | Nothing changed | 81 ms | 16 ms | 5.1x | 80 ms to 82 ms | 16 ms to 203 ms | daemon |  |
| bevy 0.20.0 | Edit a function | 3.09 s | 914 ms | 3.4x | 2.88 s to 3.16 s | 879 ms to 979 ms | daemon | spliced |
| bevy 0.20.0 | Edit a widely used function | 11.53 s | 726 ms | 16x | 11.05 s to 11.64 s | 618 ms to 746 ms | daemon | spliced |
| bevy 0.20.0 | Add a function | 3.17 s | 966 ms | 3.3x | 3.12 s to 3.25 s | 952 ms to 1.18 s | daemon | spliced |
| bevy 0.20.0 | Nothing changed | 178 ms | 19 ms | 9.4x | 177 ms to 185 ms | 19 ms to 523 ms | daemon |  |

#### Cold, Cranelift, Linux x86-64

From an empty target directory, crates already downloaded, median of 5 runs, each from nothing. The empty store fetches the standard library as a release's first build does. `cargo ra-jit build` is cargo's own build (the daemon is not loaded yet); the daemon then loads the workspace and builds it once, and edits are fast from then on. Warm store: the same in another checkout of the same commit, with the store the first one filled.

| Project | `cargo build` | `cargo ra-jit build` | Daemon ready, empty store | Daemon ready, warm store | Functions compiled | Daemon memory | Store |
|---|---:|---:|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | 4.65 s (4.59 s to 4.72 s) | 5.07 s (5.06 s to 5.12 s) | 5.62 s (5.61 s to 5.67 s) | 5.31 s (5.24 s to 5.38 s) | 0 | 2,051 MB | 92 MB |
| fd 10.3.0 | 25.77 s (25.60 s to 25.88 s) | 26.21 s (26.08 s to 26.27 s) | 26.77 s (26.63 s to 26.82 s) | 26.24 s (26.21 s to 26.71 s) | 0 | 1,677 MB | 92 MB |
| bat 0.26.0 | 9.89 s (9.80 s to 9.96 s) | 13.01 s (12.93 s to 13.04 s) | 14.09 s (14.00 s to 14.11 s) | 12.70 s (12.51 s to 12.96 s) | 0 | 2,158 MB | 243 MB |
| hyperfine 1.19.0 | 4.63 s (4.59 s to 4.70 s) | 7.20 s (7.09 s to 7.29 s) | 8.01 s (7.90 s to 8.10 s) | 6.57 s (6.52 s to 6.97 s) | 0 | 1,952 MB | 90 MB |
| just 1.43.0 | 6.64 s (6.55 s to 6.68 s) | 9.13 s (8.88 s to 9.22 s) | 10.20 s (9.95 s to 10.30 s) | 9.02 s (8.93 s to 9.31 s) | 0 | 2,381 MB | 99 MB |
| axum 0.8.6 | 6.92 s (6.88 s to 6.95 s) | 8.55 s (8.51 s to 8.69 s) | 9.89 s (9.85 s to 10.03 s) | 8.82 s (8.66 s to 9.33 s) | 0 | 2,194 MB | 94 MB |
| nushell 0.108.0 | 34.66 s (33.86 s to 35.07 s) | 35.09 s (34.70 s to 35.40 s) | 39.02 s (38.41 s to 39.38 s) | 36.21 s (35.84 s to 37.46 s) | 0 | 11,002 MB | 290 MB |
| helix 25.07.1 | 29.70 s (29.48 s to 30.05 s) | 30.72 s (30.44 s to 30.79 s) | 33.91 s (33.62 s to 34.23 s) | 34.62 s (34.45 s to 35.52 s) | 0 | 9,504 MB | 554 MB |
| bevy 0.20.0 | 82.40 s (81.28 s to 83.34 s) | 70.78 s (70.33 s to 71.22 s) | 89.54 s (89.18 s to 95.03 s) | 98.10 s (97.87 s to 98.40 s) | 0 | 27,046 MB | 1,682 MB |

#### Memory and disk, Cranelift, Linux x86-64

Peak: the most every process doing the work held at once, sampled every 10 ms and never below the largest process, median of the runs. Largest process: the peak of the one process that held the most, as the kernel kept it: `wait4`'s for the command's tree, and each process's peak sampled (on macOS the kernel's peak footprint of each, counting for the daemon only a peak it passed during the step). ra-jit is the daemon, its workers and what they run, and the `cargo ra-jit build` with any cargo it ran; cargo is cargo itself, rustc, build scripts and the linker. Daemon after: what the daemon and its workers hold once the step is over ([how](#memory-and-disk)).

| Project | Step | cargo, peak | ra-jit, peak | cargo, largest process | ra-jit, largest process | Daemon after |
|---|---|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | Cold, until the daemon is ready | 2.4 GB | 4.6 GB | 423 MB | 1.7 GB | 1.6 GB |
| ripgrep 15.1.0 | Edit a function | 529 MB | 1.8 GB | 295 MB | 1.7 GB | 1.8 GB |
| ripgrep 15.1.0 | Edit a widely used function | 536 MB | 1.8 GB | 306 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Change a signature | 527 MB | 1.8 GB | 291 MB | 1.7 GB | 1.8 GB |
| ripgrep 15.1.0 | Edit a generic function | 532 MB | 1.8 GB | 299 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Add a function | 529 MB | 1.8 GB | 295 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Add a struct field | 555 MB | 1.8 GB | 350 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Nothing changed | 29 MB | 1.9 GB | 29 MB | 1.8 GB | 1.9 GB |
| fd 10.3.0 | Cold, until the daemon is ready | 2.7 GB | 5.2 GB | 554 MB | 1.7 GB | 1.6 GB |
| fd 10.3.0 | Edit a function | 606 MB | 1.6 GB | 373 MB | 1.4 GB | 1.5 GB |
| fd 10.3.0 | Edit a widely used function | 598 MB | 1.5 GB | 372 MB | 1.4 GB | 1.5 GB |
| fd 10.3.0 | Change a signature | 610 MB | 1.6 GB | 371 MB | 1.4 GB | 1.5 GB |
| fd 10.3.0 | Add a function | 604 MB | 1.6 GB | 370 MB | 1.4 GB | 1.5 GB |
| fd 10.3.0 | Add a struct field | 634 MB | 1.5 GB | 372 MB | 1.4 GB | 1.5 GB |
| fd 10.3.0 | Nothing changed | 101 MB | 1.6 GB | 101 MB | 1.5 GB | 1.6 GB |
| bat 0.26.0 | Cold, until the daemon is ready | 3.8 GB | 7.9 GB | 518 MB | 2.0 GB | 1.9 GB |
| bat 0.26.0 | Edit a function | 751 MB | 2.0 GB | 521 MB | 1.8 GB | 2.0 GB |
| bat 0.26.0 | Edit a widely used function | 742 MB | 2.0 GB | 514 MB | 1.8 GB | 1.9 GB |
| bat 0.26.0 | Change a signature | 740 MB | 2.0 GB | 518 MB | 1.8 GB | 2.0 GB |
| bat 0.26.0 | Add a function | 750 MB | 2.0 GB | 518 MB | 1.8 GB | 2.0 GB |
| bat 0.26.0 | Add a struct field | 765 MB | 2.0 GB | 518 MB | 1.8 GB | 2.0 GB |
| bat 0.26.0 | Nothing changed | 136 MB | 2.1 GB | 136 MB | 1.9 GB | 2.1 GB |
| hyperfine 1.19.0 | Cold, until the daemon is ready | 2.6 GB | 9.8 GB | 362 MB | 1.9 GB | 1.9 GB |
| hyperfine 1.19.0 | Edit a function | 425 MB | 1.7 GB | 245 MB | 1.6 GB | 1.7 GB |
| hyperfine 1.19.0 | Edit a widely used function | 427 MB | 1.6 GB | 260 MB | 1.5 GB | 1.6 GB |
| hyperfine 1.19.0 | Change a signature | 434 MB | 1.6 GB | 265 MB | 1.5 GB | 1.6 GB |
| hyperfine 1.19.0 | Add a function | 421 MB | 1.6 GB | 248 MB | 1.5 GB | 1.6 GB |
| hyperfine 1.19.0 | Add a struct field | 451 MB | 1.6 GB | 295 MB | 1.5 GB | 1.6 GB |
| hyperfine 1.19.0 | Nothing changed | 146 MB | 1.7 GB | 146 MB | 1.6 GB | 1.7 GB |
| just 1.43.0 | Cold, until the daemon is ready | 3.4 GB | 8.7 GB | 628 MB | 2.1 GB | 1.8 GB |
| just 1.43.0 | Edit a function | 538 MB | 1.9 GB | 355 MB | 1.8 GB | 1.9 GB |
| just 1.43.0 | Edit a widely used function | 525 MB | 1.9 GB | 362 MB | 1.8 GB | 1.9 GB |
| just 1.43.0 | Change a signature | 531 MB | 1.9 GB | 359 MB | 1.8 GB | 1.9 GB |
| just 1.43.0 | Add a function | 542 MB | 1.9 GB | 379 MB | 1.8 GB | 1.9 GB |
| just 1.43.0 | Add a struct field | 648 MB | 1.9 GB | 617 MB | 1.8 GB | 1.9 GB |
| just 1.43.0 | Nothing changed | 146 MB | 2.0 GB | 146 MB | 1.9 GB | 2.0 GB |
| axum 0.8.6 | Cold, until the daemon is ready | 2.5 GB | 5.6 GB | 504 MB | 2.2 GB | 2.0 GB |
| axum 0.8.6 | Edit a function | 466 MB | 1.9 GB | 253 MB | 1.7 GB | 1.9 GB |
| axum 0.8.6 | Edit a widely used function | 450 MB | 1.9 GB | 338 MB | 1.7 GB | 1.9 GB |
| axum 0.8.6 | Add a function | 466 MB | 1.9 GB | 258 MB | 1.7 GB | 1.9 GB |
| axum 0.8.6 | Nothing changed | 172 MB | 2.0 GB | 172 MB | 1.9 GB | 2.0 GB |
| nushell 0.108.0 | Cold, until the daemon is ready | 4.7 GB | 8.8 GB | 2.4 GB | 5.5 GB | 5.3 GB |
| nushell 0.108.0 | Edit a function | 2.7 GB | 7.2 GB | 2.4 GB | 7.1 GB | 7.2 GB |
| nushell 0.108.0 | Edit a widely used function | 2.7 GB | 7.2 GB | 2.4 GB | 7.0 GB | 7.2 GB |
| nushell 0.108.0 | Change a signature | 2.7 GB | 7.2 GB | 2.4 GB | 7.0 GB | 7.2 GB |
| nushell 0.108.0 | Edit a generic function | 2.7 GB | 7.2 GB | 2.4 GB | 7.0 GB | 7.2 GB |
| nushell 0.108.0 | Add a function | 2.7 GB | 7.2 GB | 2.4 GB | 7.0 GB | 7.2 GB |
| nushell 0.108.0 | Add a struct field | 2.7 GB | 7.2 GB | 2.4 GB | 7.0 GB | 7.2 GB |
| nushell 0.108.0 | Nothing changed | 175 MB | 7.7 GB | 175 MB | 7.5 GB | 7.7 GB |
| helix 25.07.1 | Cold, until the daemon is ready | 3.9 GB | 8.7 GB | 1.8 GB | 4.1 GB | 3.9 GB |
| helix 25.07.1 | Edit a function | 2.1 GB | 6.0 GB | 1.8 GB | 5.9 GB | 6.0 GB |
| helix 25.07.1 | Edit a widely used function | 2.1 GB | 6.0 GB | 1.8 GB | 5.8 GB | 5.9 GB |
| helix 25.07.1 | Change a signature | 2.1 GB | 6.0 GB | 1.8 GB | 5.8 GB | 5.9 GB |
| helix 25.07.1 | Add a function | 2.1 GB | 6.0 GB | 1.8 GB | 5.8 GB | 5.9 GB |
| helix 25.07.1 | Add a struct field | 2.1 GB | 6.0 GB | 1.8 GB | 5.8 GB | 5.9 GB |
| helix 25.07.1 | Nothing changed | 177 MB | 9.3 GB | 177 MB | 9.1 GB | 9.3 GB |
| bevy 0.20.0 | Cold, until the daemon is ready | 8.5 GB | 9.6 GB | 8.1 GB | 9.4 GB | 8.4 GB |
| bevy 0.20.0 | Edit a function | 8.5 GB | 28.2 GB | 8.1 GB | 27.9 GB | 15.0 GB |
| bevy 0.20.0 | Edit a widely used function | 8.5 GB | 20.0 GB | 8.1 GB | 19.8 GB | 20.0 GB |
| bevy 0.20.0 | Add a function | 8.5 GB | 15.2 GB | 8.1 GB | 15.0 GB | 15.2 GB |
| bevy 0.20.0 | Nothing changed | 259 MB | 27.5 GB | 259 MB | 27.2 GB | 28.0 GB |

The daemon between builds, and what each side keeps on disk once the edits are done (each block once, [how](#memory-and-disk)). After the first build: once the step after the cold build (nothing changed) is done; growth is from then to the end, over every edit made and taken back. Warm store: the cold start of another checkout with the store the first one filled.

| Project | Daemon ready | After the first build | At the end | Growth | Warm store: peak, ready | cargo target | ra-jit target | Store | Home |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | 1.6 GB | 1.9 GB | 1.7 GB after 65 builds | −162 MB | 4.5 GB, 1.7 GB | 407 MB | 724 MB | 91 MB | 0 MB |
| fd 10.3.0 | 1.6 GB | 1.6 GB | 1.5 GB after 55 builds | −58 MB | 5.2 GB, 1.6 GB | 571 MB | 940 MB | 90 MB | 0 MB |
| bat 0.26.0 | 1.9 GB | 2.1 GB | 2.0 GB after 55 builds | −120 MB | 7.9 GB, 1.8 GB | 973 MB | 1.8 GB | 129 MB | 0 MB |
| hyperfine 1.19.0 | 1.9 GB | 1.7 GB | 1.6 GB after 55 builds | −135 MB | 8.2 GB, 1.8 GB | 342 MB | 1.0 GB | 88 MB | 0 MB |
| just 1.43.0 | 1.8 GB | 2.0 GB | 1.9 GB after 55 builds | −135 MB | 7.5 GB, 1.8 GB | 667 MB | 1.4 GB | 97 MB | 0 MB |
| axum 0.8.6 | 2.0 GB | 2.0 GB | 1.9 GB after 35 builds | −107 MB | 5.1 GB, 2.0 GB | 404 MB | 733 MB | 91 MB | 1 MB |
| nushell 0.108.0 | 5.3 GB | 7.7 GB | 7.2 GB after 65 builds | −504 MB | 8.3 GB, 5.1 GB | 5.3 GB | 7.5 GB | 185 MB | 1 MB |
| helix 25.07.1 | 3.9 GB | 9.3 GB | 5.9 GB after 55 builds | −3.4 GB | 8.0 GB, 3.8 GB | 3.7 GB | 5.5 GB | 543 MB | 1 MB |
| bevy 0.20.0 | 8.4 GB | 28.0 GB | 15.2 GB after 35 builds | −12.8 GB | 11.2 GB, 10.1 GB | 17.5 GB | 17.7 GB | 1.5 GB | 1 MB |

#### How fast the program runs, Cranelift, Linux x86-64

The same command run by the program ra-jit linked and by cargo's debug and release builds, in turn. Median, then fastest and slowest; where measured, then the program's peak resident memory while it ran (median of the runs, as the kernel kept it) and the size of the program.

| Project | Command | ra-jit | cargo debug | cargo release |
|---|---|---:|---:|---:|
| ripgrep 15.1.0 | `rg -uuu --sort path -c \w+_unchecked\(\|unsafe \{ <the toolchain's library source>` | 1.08 s (1.08 s to 1.09 s); 101 MB, 200.7 MB | 1.07 s (1.07 s to 1.08 s); 101 MB, 53.2 MB | 27 ms (26 ms to 29 ms); 101 MB, 30.7 MB |

#### Edits, LLVM, Linux x86-64

Median, then the fastest and slowest run. Built: who built the edit (daemon, or cargo and why). Link: the change was spliced into the program, or it was linked again.

| Project | Edit | cargo | ra-jit | | cargo runs | ra-jit runs | Built | Link |
|---|---|---:|---:|---:|---|---|---|---|
| ripgrep 15.1.0 | Edit a function | 504 ms | 64 ms | 7.9x | 499 ms to 510 ms | 64 ms to 78 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a widely used function | 772 ms | 58 ms | 13x | 762 ms to 779 ms | 57 ms to 93 ms | daemon | spliced |
| ripgrep 15.1.0 | Change a signature | 623 ms | 101 ms | 6.2x | 619 ms to 628 ms | 99 ms to 138 ms | daemon | spliced |
| ripgrep 15.1.0 | Edit a generic function | 746 ms | 65 ms | 11x | 733 ms to 756 ms | 64 ms to 66 ms | daemon | spliced |
| ripgrep 15.1.0 | Add a function | 504 ms | 71 ms | 7.1x | 498 ms to 509 ms | 71 ms to 73 ms | daemon | spliced |
| ripgrep 15.1.0 | Add a struct field | 889 ms | 73 ms | 12x | 879 ms to 929 ms | 71 ms to 79 ms | daemon | spliced |
| ripgrep 15.1.0 | Nothing changed | 23 ms | 24 ms | 1.0x | 23 ms to 24 ms | 23 ms to 24 ms | daemon |  |
| fd 10.3.0 | Edit a function | 484 ms | 59 ms | 8.2x | 482 ms to 587 ms | 58 ms to 70 ms | daemon | spliced |
| fd 10.3.0 | Edit a widely used function | 450 ms | 43 ms | 10x | 439 ms to 453 ms | 43 ms to 44 ms | daemon | spliced |
| fd 10.3.0 | Change a signature | 487 ms | 64 ms | 7.6x | 484 ms to 496 ms | 63 ms to 146 ms | daemon | spliced |
| fd 10.3.0 | Add a function | 488 ms | 63 ms | 7.7x | 486 ms to 513 ms | 62 ms to 69 ms | daemon | spliced |
| fd 10.3.0 | Add a struct field | 672 ms | 52 ms | 13x | 668 ms to 690 ms | 52 ms to 54 ms | daemon | spliced |
| fd 10.3.0 | Nothing changed | 32 ms | 23 ms | 1.4x | 32 ms to 33 ms | 22 ms to 23 ms | daemon |  |
| bat 0.26.0 | Edit a function | 484 ms | 57 ms | 8.5x | 472 ms to 496 ms | 56 ms to 75 ms | daemon | spliced |
| bat 0.26.0 | Edit a widely used function | 704 ms | 56 ms | 13x | 691 ms to 726 ms | 55 ms to 56 ms | daemon | spliced |
| bat 0.26.0 | Change a signature | 684 ms | 58 ms | 12x | 652 ms to 706 ms | 57 ms to 81 ms | daemon | spliced |
| bat 0.26.0 | Add a function | 484 ms | 62 ms | 7.8x | 480 ms to 498 ms | 60 ms to 64 ms | daemon | spliced |
| bat 0.26.0 | Add a struct field | 952 ms | 63 ms | 15x | 923 ms to 994 ms | 62 ms to 66 ms | daemon | spliced |
| bat 0.26.0 | Nothing changed | 60 ms | 23 ms | 2.6x | 59 ms to 61 ms | 22 ms to 23 ms | daemon |  |
| hyperfine 1.19.0 | Edit a function | 378 ms | 49 ms | 7.7x | 371 ms to 386 ms | 49 ms to 52 ms | daemon | spliced |
| hyperfine 1.19.0 | Edit a widely used function | 402 ms | 57 ms | 7.1x | 399 ms to 417 ms | 56 ms to 58 ms | daemon | spliced |
| hyperfine 1.19.0 | Change a signature | 418 ms | 85 ms | 4.9x | 412 ms to 423 ms | 84 ms to 94 ms | daemon | spliced |
| hyperfine 1.19.0 | Add a function | 375 ms | 52 ms | 7.2x | 366 ms to 395 ms | 51 ms to 53 ms | daemon | spliced |
| hyperfine 1.19.0 | Add a struct field | 478 ms | 72 ms | 6.6x | 468 ms to 488 ms | 69 ms to 91 ms | daemon | spliced |
| hyperfine 1.19.0 | Nothing changed | 41 ms | 23 ms | 1.8x | 40 ms to 42 ms | 22 ms to 26 ms | daemon |  |
| just 1.43.0 | Edit a function | 691 ms | 49 ms | 14x | 687 ms to 702 ms | 47 ms to 52 ms | daemon | spliced |
| just 1.43.0 | Edit a widely used function | 716 ms | 70 ms | 10x | 697 ms to 722 ms | 67 ms to 72 ms | daemon | spliced |
| just 1.43.0 | Change a signature | 708 ms | 91 ms | 7.8x | 679 ms to 719 ms | 91 ms to 92 ms | daemon | spliced |
| just 1.43.0 | Add a function | 773 ms | 65 ms | 12x | 760 ms to 803 ms | 62 ms to 70 ms | daemon | spliced |
| just 1.43.0 | Add a struct field | 1.80 s | 135 ms | 13x | 1.78 s to 1.92 s | 128 ms to 187 ms | daemon | spliced |
| just 1.43.0 | Nothing changed | 42 ms | 22 ms | 1.9x | 40 ms to 42 ms | 21 ms to 26 ms | daemon |  |
| axum 0.8.6 | Edit a function | 413 ms | 60 ms | 6.9x | 405 ms to 419 ms | 59 ms to 61 ms | daemon | spliced |
| axum 0.8.6 | Edit a widely used function | 845 ms | 52 ms | 16x | 832 ms to 865 ms | 52 ms to 56 ms | daemon | spliced |
| axum 0.8.6 | Add a function | 413 ms | 61 ms | 6.8x | 398 ms to 420 ms | 58 ms to 63 ms | daemon | spliced |
| axum 0.8.6 | Nothing changed | 73 ms | 24 ms | 3.0x | 71 ms to 74 ms | 23 ms to 26 ms | daemon |  |
| nushell 0.108.0 | Edit a function | 1.23 s | 269 ms | 4.6x | 1.17 s to 1.26 s | 264 ms to 281 ms | daemon | spliced |
| nushell 0.108.0 | Edit a widely used function | 4.66 s | 264 ms | 18x | 4.62 s to 4.75 s | 263 ms to 289 ms | daemon | spliced |
| nushell 0.108.0 | Change a signature | 4.68 s | 323 ms | 14x | 4.67 s to 4.73 s | 321 ms to 540 ms | daemon | spliced |
| nushell 0.108.0 | Edit a generic function | 6.41 s | 470 ms | 14x | 6.25 s to 6.45 s | 463 ms to 522 ms | daemon | spliced |
| nushell 0.108.0 | Add a function | 1.24 s | 281 ms | 4.4x | 1.21 s to 1.26 s | 275 ms to 311 ms | daemon | spliced |
| nushell 0.108.0 | Add a struct field | 8.13 s | 350 ms | 23x | 8.06 s to 8.35 s | 340 ms to 392 ms | daemon | spliced |
| nushell 0.108.0 | Nothing changed | 172 ms | 25 ms | 6.9x | 168 ms to 173 ms | 24 ms to 9.76 s | daemon |  |
| helix 25.07.1 | Edit a function | 2.16 s | 86 ms | 25x | 2.09 s to 2.74 s | 86 ms to 74.39 s | daemon | full link: no executable to add to |
| helix 25.07.1 | Edit a widely used function | 3.22 s | 125 ms | 26x | 3.17 s to 3.31 s | 124 ms to 128 ms | daemon | spliced |
| helix 25.07.1 | Change a signature | 2.77 s | 270 ms | 10x | 2.71 s to 2.81 s | 267 ms to 592 ms | daemon | spliced |
| helix 25.07.1 | Add a function | 2.16 s | 142 ms | 15x | 2.12 s to 2.17 s | 141 ms to 150 ms | daemon | spliced |
| helix 25.07.1 | Add a struct field | 8.72 s | 257 ms | 34x | 8.56 s to 9.11 s | 243 ms to 267 ms | daemon | spliced |
| helix 25.07.1 | Nothing changed | 77 ms | 24 ms | 3.2x | 75 ms to 83 ms | 23 ms to 203 ms | daemon |  |
| bevy 0.20.0 | Edit a function | 3.21 s | 891 ms | 3.6x | 3.03 s to 3.55 s | 862 ms to 930 ms | daemon | spliced |
| bevy 0.20.0 | Edit a widely used function | 11.79 s | 700 ms | 17x | 11.49 s to 13.77 s | 589 ms to 730 ms | daemon | spliced |
| bevy 0.20.0 | Add a function | 3.23 s | 986 ms | 3.3x | 3.19 s to 3.28 s | 951 ms to 1.21 s | daemon | spliced |
| bevy 0.20.0 | Nothing changed | 192 ms | 28 ms | 6.9x | 189 ms to 214 ms | 26 ms to 311 ms | daemon |  |

#### Cold, LLVM, Linux x86-64

From an empty target directory, crates already downloaded, median of 5 runs, each from nothing. The empty store fetches the standard library as a release's first build does. `cargo ra-jit build` is cargo's own build (the daemon is not loaded yet); the daemon then loads the workspace and builds it once, and edits are fast from then on. Warm store: the same in another checkout of the same commit, with the store the first one filled.

| Project | `cargo build` | `cargo ra-jit build` | Daemon ready, empty store | Daemon ready, warm store | Functions compiled | Daemon memory | Store |
|---|---:|---:|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | 5.17 s (5.13 s to 5.20 s) | 5.44 s (5.29 s to 5.53 s) | 6.02 s (5.87 s to 6.11 s) | 5.64 s (5.55 s to 5.77 s) | 0 | 2,028 MB | 63 MB |
| fd 10.3.0 | 26.53 s (26.32 s to 27.07 s) | 26.57 s (26.52 s to 27.06 s) | 27.14 s (27.10 s to 27.64 s) | 26.64 s (26.51 s to 26.68 s) | 0 | 1,763 MB | 63 MB |
| bat 0.26.0 | 10.90 s (10.87 s to 10.98 s) | 18.53 s (18.20 s to 18.55 s) | 19.65 s (19.31 s to 19.67 s) | 18.82 s (18.76 s to 19.12 s) | 0 | 2,266 MB | 212 MB |
| hyperfine 1.19.0 | 4.89 s (4.84 s to 4.90 s) | 8.72 s (8.60 s to 8.84 s) | 9.57 s (9.45 s to 9.69 s) | 8.84 s (8.72 s to 9.15 s) | 0 | 1,894 MB | 61 MB |
| just 1.43.0 | 7.76 s (7.66 s to 7.94 s) | 10.70 s (10.61 s to 10.94 s) | 11.81 s (11.73 s to 12.06 s) | 11.07 s (10.99 s to 11.36 s) | 0 | 2,309 MB | 69 MB |
| axum 0.8.6 | 7.11 s (7.03 s to 7.12 s) | 9.37 s (9.25 s to 9.55 s) | 10.76 s (10.64 s to 10.94 s) | 9.93 s (9.88 s to 10.44 s) | 0 | 2,238 MB | 64 MB |
| nushell 0.108.0 | 41.94 s (41.76 s to 42.10 s) | 43.84 s (43.38 s to 44.45 s) | 47.69 s (47.22 s to 48.30 s) | 54.79 s (54.54 s to 56.17 s) | 0 | 10,743 MB | 251 MB |
| helix 25.07.1 | 35.05 s (34.29 s to 35.44 s) | 33.78 s (33.59 s to 33.95 s) | 37.06 s (36.88 s to 37.25 s) | 38.30 s (37.73 s to 38.66 s) | 0 | 9,389 MB | 730 MB |
| bevy 0.20.0 | 94.40 s (92.39 s to 95.08 s) | 76.84 s (76.40 s to 77.06 s) | 95.68 s (95.26 s to 97.95 s) | 99.87 s (98.97 s to 99.96 s) | 0 | 13,047 MB | 811 MB |

#### Memory and disk, LLVM, Linux x86-64

Peak: the most every process doing the work held at once, sampled every 10 ms and never below the largest process, median of the runs. Largest process: the peak of the one process that held the most, as the kernel kept it: `wait4`'s for the command's tree, and each process's peak sampled (on macOS the kernel's peak footprint of each, counting for the daemon only a peak it passed during the step). ra-jit is the daemon, its workers and what they run, and the `cargo ra-jit build` with any cargo it ran; cargo is cargo itself, rustc, build scripts and the linker. Daemon after: what the daemon and its workers hold once the step is over ([how](#memory-and-disk)).

| Project | Step | cargo, peak | ra-jit, peak | cargo, largest process | ra-jit, largest process | Daemon after |
|---|---|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | Cold, until the daemon is ready | 2.4 GB | 5.2 GB | 434 MB | 1.7 GB | 1.7 GB |
| ripgrep 15.1.0 | Edit a function | 531 MB | 1.8 GB | 295 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Edit a widely used function | 537 MB | 1.8 GB | 306 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Change a signature | 532 MB | 1.8 GB | 296 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Edit a generic function | 534 MB | 1.8 GB | 300 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Add a function | 528 MB | 1.8 GB | 295 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Add a struct field | 551 MB | 1.8 GB | 349 MB | 1.6 GB | 1.7 GB |
| ripgrep 15.1.0 | Nothing changed | 101 MB | 1.9 GB | 101 MB | 1.7 GB | 1.8 GB |
| fd 10.3.0 | Cold, until the daemon is ready | 2.6 GB | 5.8 GB | 554 MB | 1.7 GB | 1.7 GB |
| fd 10.3.0 | Edit a function | 606 MB | 1.6 GB | 372 MB | 1.4 GB | 1.6 GB |
| fd 10.3.0 | Edit a widely used function | 594 MB | 1.6 GB | 368 MB | 1.4 GB | 1.6 GB |
| fd 10.3.0 | Change a signature | 611 MB | 1.6 GB | 372 MB | 1.4 GB | 1.6 GB |
| fd 10.3.0 | Add a function | 606 MB | 1.6 GB | 372 MB | 1.4 GB | 1.6 GB |
| fd 10.3.0 | Add a struct field | 632 MB | 1.6 GB | 371 MB | 1.4 GB | 1.6 GB |
| fd 10.3.0 | Nothing changed | 136 MB | 1.7 GB | 136 MB | 1.5 GB | 1.7 GB |
| bat 0.26.0 | Cold, until the daemon is ready | 3.8 GB | 8.9 GB | 520 MB | 2.1 GB | 2.0 GB |
| bat 0.26.0 | Edit a function | 750 MB | 2.2 GB | 520 MB | 1.9 GB | 2.1 GB |
| bat 0.26.0 | Edit a widely used function | 746 MB | 2.2 GB | 517 MB | 1.9 GB | 2.1 GB |
| bat 0.26.0 | Change a signature | 742 MB | 2.2 GB | 521 MB | 1.9 GB | 2.1 GB |
| bat 0.26.0 | Add a function | 750 MB | 2.2 GB | 520 MB | 1.9 GB | 2.1 GB |
| bat 0.26.0 | Add a struct field | 764 MB | 2.2 GB | 517 MB | 1.9 GB | 2.1 GB |
| bat 0.26.0 | Nothing changed | 146 MB | 2.3 GB | 146 MB | 2.0 GB | 2.2 GB |
| hyperfine 1.19.0 | Cold, until the daemon is ready | 2.6 GB | 11.7 GB | 360 MB | 2.0 GB | 1.9 GB |
| hyperfine 1.19.0 | Edit a function | 421 MB | 1.8 GB | 246 MB | 1.6 GB | 1.7 GB |
| hyperfine 1.19.0 | Edit a widely used function | 426 MB | 1.7 GB | 259 MB | 1.5 GB | 1.7 GB |
| hyperfine 1.19.0 | Change a signature | 429 MB | 1.7 GB | 264 MB | 1.5 GB | 1.7 GB |
| hyperfine 1.19.0 | Add a function | 422 MB | 1.7 GB | 248 MB | 1.5 GB | 1.7 GB |
| hyperfine 1.19.0 | Add a struct field | 450 MB | 1.7 GB | 293 MB | 1.5 GB | 1.7 GB |
| hyperfine 1.19.0 | Nothing changed | 146 MB | 1.8 GB | 146 MB | 1.6 GB | 1.8 GB |
| just 1.43.0 | Cold, until the daemon is ready | 3.4 GB | 10.7 GB | 631 MB | 2.1 GB | 1.8 GB |
| just 1.43.0 | Edit a function | 539 MB | 2.0 GB | 356 MB | 1.8 GB | 2.0 GB |
| just 1.43.0 | Edit a widely used function | 534 MB | 2.0 GB | 360 MB | 1.8 GB | 2.0 GB |
| just 1.43.0 | Change a signature | 530 MB | 2.0 GB | 360 MB | 1.8 GB | 2.0 GB |
| just 1.43.0 | Add a function | 542 MB | 2.0 GB | 378 MB | 1.8 GB | 2.0 GB |
| just 1.43.0 | Add a struct field | 663 MB | 2.0 GB | 623 MB | 1.8 GB | 2.0 GB |
| just 1.43.0 | Nothing changed | 172 MB | 2.1 GB | 172 MB | 1.9 GB | 2.1 GB |
| axum 0.8.6 | Cold, until the daemon is ready | 2.5 GB | 6.3 GB | 508 MB | 2.4 GB | 2.2 GB |
| axum 0.8.6 | Edit a function | 468 MB | 2.0 GB | 253 MB | 1.8 GB | 1.9 GB |
| axum 0.8.6 | Edit a widely used function | 454 MB | 2.0 GB | 338 MB | 1.8 GB | 1.9 GB |
| axum 0.8.6 | Add a function | 468 MB | 2.0 GB | 255 MB | 1.8 GB | 1.9 GB |
| axum 0.8.6 | Nothing changed | 175 MB | 2.1 GB | 175 MB | 1.9 GB | 2.1 GB |
| nushell 0.108.0 | Cold, until the daemon is ready | 4.7 GB | 10.2 GB | 2.4 GB | 5.6 GB | 5.4 GB |
| nushell 0.108.0 | Edit a function | 2.7 GB | 7.5 GB | 2.4 GB | 7.2 GB | 7.4 GB |
| nushell 0.108.0 | Edit a widely used function | 2.7 GB | 7.4 GB | 2.4 GB | 7.1 GB | 7.3 GB |
| nushell 0.108.0 | Change a signature | 2.7 GB | 7.4 GB | 2.4 GB | 7.1 GB | 7.3 GB |
| nushell 0.108.0 | Edit a generic function | 2.7 GB | 7.4 GB | 2.4 GB | 7.1 GB | 7.3 GB |
| nushell 0.108.0 | Add a function | 2.7 GB | 7.4 GB | 2.4 GB | 7.2 GB | 7.3 GB |
| nushell 0.108.0 | Add a struct field | 2.8 GB | 7.4 GB | 2.4 GB | 7.1 GB | 7.3 GB |
| nushell 0.108.0 | Nothing changed | 177 MB | 7.9 GB | 177 MB | 7.6 GB | 7.8 GB |
| helix 25.07.1 | Cold, until the daemon is ready | 3.9 GB | 9.8 GB | 1.8 GB | 4.1 GB | 3.9 GB |
| helix 25.07.1 | Edit a function | 2.1 GB | 6.1 GB | 1.8 GB | 5.8 GB | 6.0 GB |
| helix 25.07.1 | Edit a widely used function | 2.1 GB | 6.0 GB | 1.8 GB | 5.8 GB | 5.9 GB |
| helix 25.07.1 | Change a signature | 2.1 GB | 6.0 GB | 1.8 GB | 5.8 GB | 6.0 GB |
| helix 25.07.1 | Add a function | 2.1 GB | 6.0 GB | 1.8 GB | 5.8 GB | 5.9 GB |
| helix 25.07.1 | Add a struct field | 2.1 GB | 6.0 GB | 1.8 GB | 5.8 GB | 5.9 GB |
| helix 25.07.1 | Nothing changed | 238 MB | 9.4 GB | 238 MB | 9.1 GB | 9.4 GB |
| bevy 0.20.0 | Cold, until the daemon is ready | 8.5 GB | 9.7 GB | 8.1 GB | 9.4 GB | 8.3 GB |
| bevy 0.20.0 | Edit a function | 8.5 GB | 12.3 GB | 8.1 GB | 12.0 GB | 12.4 GB |
| bevy 0.20.0 | Edit a widely used function | 8.5 GB | 19.4 GB | 8.1 GB | 19.1 GB | 19.8 GB |
| bevy 0.20.0 | Add a function | 8.5 GB | 15.0 GB | 8.1 GB | 14.7 GB | 15.0 GB |
| bevy 0.20.0 | Nothing changed | 344 MB | 12.7 GB | 344 MB | 12.3 GB | 12.6 GB |

The daemon between builds, and what each side keeps on disk once the edits are done (each block once, [how](#memory-and-disk)). After the first build: once the step after the cold build (nothing changed) is done; growth is from then to the end, over every edit made and taken back. Warm store: the cold start of another checkout with the store the first one filled.

| Project | Daemon ready | After the first build | At the end | Growth | Warm store: peak, ready | cargo target | ra-jit target | Store | Home |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| ripgrep 15.1.0 | 1.7 GB | 1.8 GB | 1.7 GB after 65 builds | −137 MB | 5.3 GB, 1.7 GB | 407 MB | 599 MB | 61 MB | 0 MB |
| fd 10.3.0 | 1.7 GB | 1.7 GB | 1.6 GB after 55 builds | −76 MB | 5.8 GB, 1.7 GB | 571 MB | 784 MB | 61 MB | 1 MB |
| bat 0.26.0 | 2.0 GB | 2.2 GB | 2.1 GB after 55 builds | −92 MB | 9.0 GB, 1.9 GB | 973 MB | 1.3 GB | 98 MB | 0 MB |
| hyperfine 1.19.0 | 1.9 GB | 1.8 GB | 1.7 GB after 55 builds | −77 MB | 11.5 GB, 2.0 GB | 342 MB | 595 MB | 59 MB | 0 MB |
| just 1.43.0 | 1.8 GB | 2.1 GB | 2.0 GB after 55 builds | −119 MB | 10.8 GB, 1.9 GB | 667 MB | 947 MB | 66 MB | 0 MB |
| axum 0.8.6 | 2.2 GB | 2.1 GB | 1.9 GB after 35 builds | −132 MB | 6.2 GB, 2.1 GB | 404 MB | 525 MB | 62 MB | 1 MB |
| nushell 0.108.0 | 5.4 GB | 7.8 GB | 7.3 GB after 65 builds | −493 MB | 10.6 GB, 5.0 GB | 5.3 GB | 5.6 GB | 146 MB | 1 MB |
| helix 25.07.1 | 3.9 GB | 9.4 GB | 5.9 GB after 55 builds | −3.4 GB | 10.0 GB, 3.8 GB | 3.7 GB | 4.8 GB | 718 MB | 1 MB |
| bevy 0.20.0 | 8.3 GB | 12.6 GB | 15.0 GB after 35 builds | +2.4 GB | 10.8 GB, 10.1 GB | 17.5 GB | 16.3 GB | 717 MB | 1 MB |

#### How fast the program runs, LLVM, Linux x86-64

The same command run by the program ra-jit linked and by cargo's debug and release builds, in turn. Median, then fastest and slowest; where measured, then the program's peak resident memory while it ran (median of the runs, as the kernel kept it) and the size of the program.

| Project | Command | ra-jit | cargo debug | cargo release |
|---|---|---:|---:|---:|
| ripgrep 15.1.0 | `rg -uuu --sort path -c \w+_unchecked\(\|unsafe \{ <the toolchain's library source>` | 1.08 s (1.08 s to 1.08 s); 101 MB, 199.8 MB | 1.08 s (1.08 s to 1.08 s); 101 MB, 53.2 MB | 27 ms (26 ms to 28 ms); 101 MB, 30.7 MB |

### Linux x86-64, bun

ra-jit [`961bb3105a`](https://github.com/indexable-inc/ra-jit/commit/961bb3105a8e90e0ac33488c803f5513f075bb87), 2026-10-23T15:45:27Z, Rust 1.99.0, 3 runs of each edit, measured by hand. Raw: [benchmarks/bun-linux-x86-64.json](benchmarks/bun-linux-x86-64.json).

| Machine | |
|---|---|
| Name | AWS c8a.24xlarge (dev-aws-eph-rajit-x86edits), pinned to 16 cores with taskset |
| CPU | AMD EPYC 9R45 |
| Cores | 16 |
| Memory (GiB) | 185 |
| OS | Linux |
| Kernel | 6.18.53 |
| rustc | rustc 1.100.0-nightly (574ff7d98 2026-09-14) |
| C compiler | gcc (GCC) 16.2.0 |
| Linker | GNU ld (GNU Binutils) 2.46 |
| LLVM | 23.1.3 |
| Incremental | profile default (on for dev) |
| Compiler cache | none |
| Load average at the start | 12.4, 6.4, 2.7 |

#### Edits, Cranelift, Linux x86-64, bun

Median, then the fastest and slowest run. Built: who built the edit (daemon, or cargo and why). Link: the change was spliced into the program, or it was linked again.

| Project | Edit | cargo | ra-jit | | cargo runs | ra-jit runs | Built | Link |
|---|---|---:|---:|---:|---|---|---|---|
| bun 1.4.3 | Edit a function | 9.11 s | 451 ms | 20x | 9.10 s to 9.23 s | 443 ms to 534 ms | daemon | spliced |
| bun 1.4.3 | Edit a widely used function | 16.51 s | 430 ms | 38x | 16.48 s to 16.75 s | 420 ms to 439 ms | daemon | spliced |
| bun 1.4.3 | Change a signature | 9.22 s | 556 ms | 17x | 9.20 s to 9.23 s | 543 ms to 649 ms | daemon | spliced |
| bun 1.4.3 | Edit a generic function | 16.76 s | 452 ms | 37x | 16.74 s to 16.80 s | 450 ms to 453 ms | daemon | spliced |
| bun 1.4.3 | Add a function | 14.54 s | 648 ms | 22x | 14.51 s to 14.73 s | 641 ms to 657 ms | daemon | spliced |
| bun 1.4.3 | Nothing changed | 72 ms | 18 ms | 4.0x | 72 ms to 72 ms | 17 ms to 33 ms | daemon |  |

#### Cold, Cranelift, Linux x86-64, bun

From an empty target directory, crates already downloaded, one run. The empty store fetches the standard library as a release's first build does. `cargo ra-jit build` is cargo's own build (the daemon is not loaded yet); the daemon then loads the workspace and builds it once, and edits are fast from then on. Warm store: the same in another checkout of the same commit, with the store the first one filled.

| Project | `cargo build` | `cargo ra-jit build` | Daemon ready, empty store | Daemon ready, warm store | Functions compiled | Daemon memory | Store |
|---|---:|---:|---:|---:|---:|---:|---:|
| bun 1.4.3 | 71.44 s | 69.88 s | 73.07 s | 72.59 s | 0 | 32,580 MB | 100 MB |

#### Memory and disk, Cranelift, Linux x86-64, bun

Peak: the most every process doing the work held at once, sampled every 10 ms and never below the largest process, median of the runs. Largest process: the peak of the one process that held the most, as the kernel kept it: `wait4`'s for the command's tree, and each process's peak sampled (on macOS the kernel's peak footprint of each, counting for the daemon only a peak it passed during the step). ra-jit is the daemon, its workers and what they run, and the `cargo ra-jit build` with any cargo it ran; cargo is cargo itself, rustc, build scripts and the linker. Daemon after: what the daemon and its workers hold once the step is over ([how](#memory-and-disk)).

| Project | Step | cargo, peak | ra-jit, peak | cargo, largest process | ra-jit, largest process | Daemon after |
|---|---|---:|---:|---:|---:|---:|
| bun 1.4.3 | Cold, until the daemon is ready | 4.5 GB | 14.1 GB | 3.0 GB | 10.9 GB | 10.5 GB |
| bun 1.4.3 | Edit a function | 3.3 GB | 24.5 GB | 3.0 GB | 24.3 GB | 24.5 GB |
| bun 1.4.3 | Edit a widely used function | 3.3 GB | 24.5 GB | 3.0 GB | 24.3 GB | 24.4 GB |
| bun 1.4.3 | Change a signature | 3.3 GB | 24.5 GB | 3.0 GB | 24.3 GB | 24.5 GB |
| bun 1.4.3 | Edit a generic function | 3.3 GB | 24.5 GB | 3.0 GB | 24.3 GB | 24.4 GB |
| bun 1.4.3 | Add a function | 3.3 GB | 24.5 GB | 3.0 GB | 24.3 GB | 24.5 GB |
| bun 1.4.3 | Nothing changed | 56 MB | 25.6 GB | 56 MB | 25.5 GB | 25.5 GB |

The daemon between builds, and what each side keeps on disk once the edits are done (each block once, [how](#memory-and-disk)). After the first build: once the step after the cold build (nothing changed) is done; growth is from then to the end, over every edit made and taken back. Warm store: the cold start of another checkout with the store the first one filled.

| Project | Daemon ready | After the first build | At the end | Growth | Warm store: peak, ready | cargo target | ra-jit target | Store | Home |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| bun 1.4.3 | 10.5 GB | 25.5 GB | 24.5 GB after 33 builds | −1.0 GB | 14.0 GB, 10.5 GB | 5.7 GB | 6.6 GB | 96 MB | 1 MB |

#### How fast the program runs, Cranelift, Linux x86-64, bun

The same command run by the program ra-jit linked and by cargo's debug and release builds, in turn. Median, then fastest and slowest; where measured, then the program's peak resident memory while it ran (median of the runs, as the kernel kept it) and the size of the program.

| Project | Command | ra-jit | cargo debug | cargo release |
|---|---|---:|---:|---:|
| bun 1.4.3 | `bun -e const t = new Bun.Transpiler({ loader: 'ts', minifyWhitespace: true }); const fs = require('fs'); const sources = ['http2', 'net', '_http_server', 'fs', 'child_process'].map(n => fs.readFileSync('src/js/node/' + n + '.ts', 'utf8')); let n = 0; for (let i = 0; i < 20; i++) for (const s of sources) n += t.transformSync(s).length; console.log(n)` | 1.40 s (1.40 s to 1.40 s); 108 MB, 591.4 MB | 961 ms (960 ms to 962 ms); 100 MB, 397.9 MB | 136 ms (136 ms to 136 ms); 100 MB, 314.0 MB |

#### Edits, LLVM, Linux x86-64, bun

Median, then the fastest and slowest run. Built: who built the edit (daemon, or cargo and why). Link: the change was spliced into the program, or it was linked again.

| Project | Edit | cargo | ra-jit | | cargo runs | ra-jit runs | Built | Link |
|---|---|---:|---:|---:|---|---|---|---|
| bun 1.4.3 | Edit a function | 9.06 s | 462 ms | 20x | 9.04 s to 9.14 s | 455 ms to 536 ms | daemon | spliced |
| bun 1.4.3 | Edit a widely used function | 16.50 s | 438 ms | 38x | 16.49 s to 16.55 s | 437 ms to 439 ms | daemon | spliced |
| bun 1.4.3 | Change a signature | 9.16 s | 526 ms | 17x | 9.13 s to 9.21 s | 525 ms to 618 ms | daemon | spliced |
| bun 1.4.3 | Edit a generic function | 16.86 s | 454 ms | 37x | 16.71 s to 16.95 s | 453 ms to 465 ms | daemon | spliced |
| bun 1.4.3 | Add a function | 14.49 s | 617 ms | 23x | 14.41 s to 14.75 s | 614 ms to 630 ms | daemon | spliced |
| bun 1.4.3 | Nothing changed | 72 ms | 26 ms | 2.8x | 72 ms to 74 ms | 23 ms to 26 ms | daemon |  |

#### Cold, LLVM, Linux x86-64, bun

From an empty target directory, crates already downloaded, one run. The empty store fetches the standard library as a release's first build does. `cargo ra-jit build` is cargo's own build (the daemon is not loaded yet); the daemon then loads the workspace and builds it once, and edits are fast from then on. Warm store: the same in another checkout of the same commit, with the store the first one filled.

| Project | `cargo build` | `cargo ra-jit build` | Daemon ready, empty store | Daemon ready, warm store | Functions compiled | Daemon memory | Store |
|---|---:|---:|---:|---:|---:|---:|---:|
| bun 1.4.3 | 78.33 s | 71.93 s | 74.95 s | 73.11 s | 0 | 31,245 MB | 68 MB |

#### Memory and disk, LLVM, Linux x86-64, bun

Peak: the most every process doing the work held at once, sampled every 10 ms and never below the largest process, median of the runs. Largest process: the peak of the one process that held the most, as the kernel kept it: `wait4`'s for the command's tree, and each process's peak sampled (on macOS the kernel's peak footprint of each, counting for the daemon only a peak it passed during the step). ra-jit is the daemon, its workers and what they run, and the `cargo ra-jit build` with any cargo it ran; cargo is cargo itself, rustc, build scripts and the linker. Daemon after: what the daemon and its workers hold once the step is over ([how](#memory-and-disk)).

| Project | Step | cargo, peak | ra-jit, peak | cargo, largest process | ra-jit, largest process | Daemon after |
|---|---|---:|---:|---:|---:|---:|
| bun 1.4.3 | Cold, until the daemon is ready | 4.5 GB | 13.9 GB | 3.0 GB | 10.6 GB | 10.3 GB |
| bun 1.4.3 | Edit a function | 3.3 GB | 24.2 GB | 3.0 GB | 24.0 GB | 24.1 GB |
| bun 1.4.3 | Edit a widely used function | 3.3 GB | 24.2 GB | 3.0 GB | 24.0 GB | 24.1 GB |
| bun 1.4.3 | Change a signature | 3.3 GB | 24.2 GB | 3.0 GB | 24.0 GB | 24.1 GB |
| bun 1.4.3 | Edit a generic function | 3.3 GB | 24.2 GB | 3.0 GB | 24.0 GB | 24.1 GB |
| bun 1.4.3 | Add a function | 3.3 GB | 24.2 GB | 3.0 GB | 24.0 GB | 24.2 GB |
| bun 1.4.3 | Nothing changed | 100 MB | 25.2 GB | 100 MB | 24.9 GB | 25.1 GB |

The daemon between builds, and what each side keeps on disk once the edits are done (each block once, [how](#memory-and-disk)). After the first build: once the step after the cold build (nothing changed) is done; growth is from then to the end, over every edit made and taken back. Warm store: the cold start of another checkout with the store the first one filled.

| Project | Daemon ready | After the first build | At the end | Growth | Warm store: peak, ready | cargo target | ra-jit target | Store | Home |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| bun 1.4.3 | 10.3 GB | 25.1 GB | 24.2 GB after 33 builds | −952 MB | 13.8 GB, 10.4 GB | 5.7 GB | 6.5 GB | 64 MB | 1 MB |

#### How fast the program runs, LLVM, Linux x86-64, bun

The same command run by the program ra-jit linked and by cargo's debug and release builds, in turn. Median, then fastest and slowest; where measured, then the program's peak resident memory while it ran (median of the runs, as the kernel kept it) and the size of the program.

| Project | Command | ra-jit | cargo debug | cargo release |
|---|---|---:|---:|---:|
| bun 1.4.3 | `bun -e const t = new Bun.Transpiler({ loader: 'ts', minifyWhitespace: true }); const fs = require('fs'); const sources = ['http2', 'net', '_http_server', 'fs', 'child_process'].map(n => fs.readFileSync('src/js/node/' + n + '.ts', 'utf8')); let n = 0; for (let i = 0; i < 20; i++) for (const s of sources) n += t.transformSync(s).length; console.log(n)` | 916 ms (916 ms to 917 ms); 135 MB, 530.5 MB | 961 ms (961 ms to 962 ms); 135 MB, 397.9 MB | 135 ms (135 ms to 136 ms); 135 MB, 314.0 MB |

<!-- bench:end -->
