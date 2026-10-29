---
status: record
summary: Where blaze's time goes against cargo on three workspaces.
date: 2026-10-20
---
# Where blaze's time goes

Measurements of `blaze` against plain cargo on three pinned workspaces, what the daemon
says about its own time, profiles of the slow cases, and what was changed because of them.
Everything here was run on one machine: x86-64 Linux, 8 cores, 31 GB, Rust 1.99.0, LLVM 20.
What was measured and what is inferred from it are kept apart; inferences are marked.

## The harness

`harness/bench.py` (the edit loop that had this name is now `harness/editbench.py`) takes a
pinned checkout of ripgrep 15.1.0, cargo 0.92.0 or nushell 0.108.0, copies it, and under each
backend, with a store, a daemon and a home of its own, runs:

- **cold**: `blaze build` from nothing, then the wait until `blaze status` says the
  workspace is ready. Cargo's own cold build is started only after that, so the two do not
  share the cores.
- **no-op**: `blaze build` with nothing changed.
- **body**: a statement is added to one function of the program's own crate.
- **signature**: a method gets another parameter and every caller the argument.
- **leaf**: a statement is added to a function in a crate of the workspace that the others
  depend on.
- **generic**: a statement is added to a generic function that is used at many types.

Every edit is made and taken back three times, with plain cargo building the same source into
its own target directory after each, and after the first of each kind the program is run and
has to print what cargo's prints. For each phase the JSON has the wall time of the command
(the median for edits, with each sample and the slowest), cargo's time, the most memory the
daemon's workers held (`VmHWM` summed), who built it (the daemon, or cargo with the daemon's
stated reason), whether the edit was spliced or linked in full and why, how many functions
were compiled, and the daemon's own timings from `BLAZE_STATS`. An edit the daemon turns down
is measured once and reported as built by cargo; it is not retried, since each refusal has the
daemon load the workspace again.

    cd harness
    python3 bench.py ripgrep cargo nushell --out result.json        # both backends
    python3 bench.py cargo --backend llvm --work DIR --reference DIR2
    python3 bench.py --compare bench-baseline.json result.json --threshold 25

`--compare` exits 1 when a phase is slower, holds more memory or compiles more functions than
the baseline by more than the threshold, when the daemon built something in the baseline and
cargo builds it now, when a spliced edit is no longer spliced, when a phase is missing, or
when a program printed something else than cargo's.

`harness/bench-baseline.json` is ripgrep under both backends on the machine above. The CI job
`bench (ripgrep)` runs the same and compares at 200%, with `continue-on-error`; it has not
run yet at the time of writing. The runner is not this machine, so only the counts and the
splice/full-link/refusal fields mean something there until a baseline is taken on a runner.

How steady it is, measured: ripgrep's edits were within 20% over three separate runs of the
harness (body 0.124, 0.161, 0.154 s under Cranelift). The cold phase is not steady: 17.1,
19.8 and 27.1 s for ripgrep under Cranelift in three runs with nothing changed between them.
Cargo and nushell need about 12 GB of disk each per backend and 25 to 40 minutes, which is
why CI has ripgrep alone.

## What the harness measured

Seconds, `blaze` then plain cargo, on the branch before any fix. Edits are the median of
three; "first" is the first of the three where it differed much from the others.

| Cranelift | ripgrep | cargo | nushell |
|---|---|---|---|
| cold | 17.1 / 5.4 | 173.2 / 66.2 | 218.9 / 54.8 |
| no-op | 0.02 / 0.03 | 0.04 / 0.18 | 0.03 / 0.32 |
| body | 0.12 / 0.48, spliced | 0.39 / 5.40, spliced | 1.09 / 2.20, spliced |
| signature | 0.13 / 0.63, spliced | refused, 43.3 / 10.2 | refused, 11.2 / 9.6 |
| leaf | 0.08 / 0.84, spliced | 0.41 / 7.23 (first 41.9, full link) | refused, 40.3 / 8.5 |
| generic | 0.08 / 0.82, spliced | 35.7 / 7.8, full link | 1.27 / 10.3 (first 30.6, full link) |
| peak RSS | 2.1 GB | 8.4 GB | 10.2 GB |

| LLVM (`BLAZE_OPT=2 BLAZE_EMIT=fast`) | ripgrep | cargo | nushell |
|---|---|---|---|
| cold | 20.1 / 5.3 | 225.6 / 52.8 | 311.4 / 66.6 |
| no-op | 0.04 / 0.03 | 0.07 / 0.18 | 0.05 / 0.30 |
| body | 0.17 / 0.47, spliced | 0.37 / 5.00, spliced | 2.46 / 3.10, spliced |
| signature | 0.18 / 0.62, spliced | refused, 31.1 / 9.7 | refused, 11.8 / 8.4 |
| leaf | 0.09 / 0.88, spliced | 0.43 / 7.79 (first 26.7, full link) | refused, 34.1 / 8.6 |
| generic | 0.09 / 0.79, spliced | 31.1 / 14.3, full link | 2.14 / 10.6 (first 21.9, full link) |
| peak RSS | 2.1 GB | 7.9 GB | 9.3 GB |

The cold column is cargo's build through the wrapper plus the daemon's load and first build
beside it; the program is usable when cargo's part is done (5.4, 48 and 66 s under Cranelift),
and the rest is the daemon getting ready for the first edit.

What these say, before any profile:

- Where the daemon builds an edit and splices it, it is 2 to 14 times faster than cargo.
- A "refused" edit is one the daemon turned down with a reason and cargo built. The time is
  cargo's build plus the daemon's attempt, and after it the daemon loads the workspace again
  from the start, because what cargo linked is no longer what the daemon's objects are of
  (`Daemon::warm` in `crates/blaze-engine/src/cargo/daemon.rs` replaces the worker when the
  generation differs). On cargo that reload is about 150 s during which every build is cargo's.
- Cargo's signature edit (`GlobalContext::cli_unstable`, 113 places in 49 files) is refused
  with "`publish` has errors: frontend: mir: HasErrors". Nushell's signature and leaf edits are
  refused with the same for `MatchPattern::eq`, a derived `PartialEq` the frontend cannot
  lower in the cold build either.
- The first edit to the checkout after the daemon loaded with a full store compiles about half
  of all functions again and links in full (cargo leaf 109,223 functions, nushell generic
  104,924): the first build takes the checkout's code from the store, and a patch can only be
  made against code compiled in this process (`hydrate` in `CodegenUnit::compile`,
  `Base::outdated`).
- Cargo's generic edit (`GlobalContext::get`) takes 31 to 37 s every time, compiles 199 to 435
  functions, and still writes the whole object again.

The sections below take these one at a time.

## What the daemon says about its own time

With `BLAZE_STATS=1` the daemon already printed, for every build, `timeline-us:` (the steps
of an edit one after another: settle, read, apply, roots, check, reach, resolve, define,
object, write, link, and what is left between them), `phases:`, `backend:` (the producer's
time summed over threads; LLVM's split into lower, verify, optimize, emit, read), `store:`,
`link:` (spliced or not and why) and `edit-to-binary:`; and for the first build `load
workspace:`, `resolve:` and `cold:`. Two things could not be told from those and were added,
on the same switch and as lines of the same kind:

- `steps: infer …, mir …, monomorphize …, translate …, describe …, store read …, store write …`:
  what the compiling threads spent, summed over the threads, on the types of a body, its MIR,
  putting an instance's types into a generic body, turning MIR into the producer's IR, debug
  info (lines and variables of each function, and writing the sections), and reading and
  writing entries of the store. `infer` is only what had not been asked for before codegen got
  to it; after an edit the types are computed in the `check` step of the timeline, so `infer`
  is then near zero.
- `round N: TIME, started over: WHY` for every pass over the program whose object was thrown
  away, and `discarded=` in `timeline-us` for their sum. `phases:` only ever described the last
  round, so a build of four rounds showed three quarters of its time as "compile-rest".

The client, the daemon and the worker also record spans of every build on one clock, always
(`crates/blaze-engine/src/trace.rs`): the worker sends its own with its answer, the daemon adds its
own, and the client puts them together. With them:

- `BLAZE_STATS=1` makes `blaze build` print `blaze time: 402.1 ms: check 285.5 (71%),
  …` and a table of where the client's wall time went, step by step (client, daemon, worker),
  with `unaccounted` for moments no step had: the rows add up to the total.
- `BLAZE_TRACE=<path>` writes the spans as a Chrome trace (ui.perfetto.dev and
  chrome://tracing open it): to `path` if it ends in `.json`, else to a new file in the
  directory `path` for every build. Besides the steps it has a span for each rust-analyzer
  query the worker ran (infer, MIR, impls) and each step of code generation (translate,
  cranelift) of 0.2 ms or longer. The worker's
  reach, resolve, define, object, statics, setup and compile-rest are sums over threads laid
  end to end, so read them as one stretch.
- `harness/realbench.py` runs every warm build with `BLAZE_TRACE` and keeps the breakdown of
  each row's median run; `realbench.py --table FILE` prints it under the results.

The old `BLAZE_TRACE` switch, which printed the rust-analyzer queries an edit ran, is now
`BLAZE_QUERIES`.

`bench.py` puts these in the JSON as `steps_ms` (the timeline), `threads_s` (the `steps:` and
`backend:` lines) and `started_over` (the reasons).

On ripgrep's cold build under Cranelift (7.1 s of compiling on 8 threads, 23,387 functions)
the threads spent: MIR 2.9 s, translation 3.8 s, Cranelift 6.2 s, store writes 2.1 s, debug
info 1.2 s, monomorphization 0.5 s, types 0.3 s, store reads 0.07 s. The same for cargo and
nushell, and the profiles, are not in this file yet.

## Cargo's cold build, taken apart

All of this is cargo 0.92.0 under Cranelift, built by `blaze DIR --crate … --out … --link-cmd …`
as `harness/remote.py` does it (one process, no daemon, so that nothing but blaze is timed),
with cargo's own artifacts built beforehand. Seconds are blaze's alone.

### The store and debug info, each on and off

| | this head | before format 2 and debug info (27315323cb) |
| --- | --- | --- |
| store, debug info | 102.7 s (compiling 92.3 s) | 81.2 s (71.1 s), no debug info yet |
| store, `BLAZE_DEBUGINFO=0` | 88.2 s (77.7 s) | |
| no store, debug info | 74.6 s (64.7 s) | |
| no store, `BLAZE_DEBUGINFO=0` | 61.0 s (51.2 s) | 65.0 s (54.9 s) |

One run each after a run that is thrown away. The same four in the opposite order gave 109.9,
97.2, 77.6 and 65.0 s, and the old binary gave 81.2 and 89.7 s for the same thing twice, so a
difference under about 8 s between two rows is not one.

What it says: debug info costs 13 to 14 s with or without the store, and the store costs 27
to 28 s with or without debug info; the two add up. With neither, this head is not slower
than the old one (61.0 s against 65.0 s). The old store cost 16 s (81.2 against 65.0). So the
6 s that REMOTE.md could not place are the store: writing to it costs about 11 s more than
it did in format 1, and compiling itself got a few seconds faster, which hid part of it.

### The profile

`perf record -F 499 --call-graph fp` on a binary built with frame pointers and line tables,
of the first two rows of the table (128.0 s with the store, 84.4 s without, under perf; the
binary is of the head before the last merge, the code profiled is the same). 237.8 s of
processor time with the store, 167.3 s without.

**The build is one thread for most of its time.** Measured in 5 s windows, the process uses
more than two cores for 15 to 20 s of the 108 s of compiling and one core for the rest. The
thread that runs `CodegenUnit::compile` is on a processor for 84 s of those 108 s (54 s of
66 s without the store). Three things keep it there:

1. *Functions found on the way are compiled one at a time.* Each pass compiles the functions
   it knows of on every core (`in_parallel` over `compiled_root`) and then defines them one
   by one; a function that one of them asks for is appended to the same queue and reached by
   the same loop, which by then is past the parallel step. In the profile that is 18 s at the
   start of the build with one thread in `define_function`, 7.9 s of it lowering MIR and
   8.4 s compiling.
2. *Putting instances into the object is serial*: after the instances are compiled on every
   core (`compile_ahead`, the 15 to 20 s above), each one is stitched (relocations resolved
   to names: 3.5 s a round), described for the debugger (3.5 to 4.8 s a round) and kept in
   the store (14.6 s in the first round, 2.9 s after) on the one thread.
3. *The pass is made three times.* The first is thrown away after 60 s because 7 instances
   did not compile (`TypeId` of a type with a `char` const argument), the second after 18 s
   because a static turned out to be used by a function left to rustc. A later pass compiles
   nothing again but stitches, describes and keeps everything again: 11 to 12 s each on the
   one thread, 16 to 20 s by the clock.

**Debug info** is `ObjectCodegen::describe` on that one thread, 12 to 14 s over the three
passes: collecting the variables of each function (6 s), finding the line of each
instruction (3 s; the compiling threads spend another 7 s on source locations), and handing
the function to gimli (1.7 s). Writing the sections at the end is under a second. So it is
neither the line program nor the relocations nor the bytes in the store (the store grows by
140 MB, but the table above shows the cost is the same without a store); it is the work of
describing 180,000 instances, done three times, on one thread.

**The store** is, by processor time:

- SHA-256, 23.3 s, none of it without the store: the key of an entry is a hash of its name,
  and the names of instances are long (9.1 s, half of it on the one thread, since the key is
  computed when a compiling thread looks the entry up and again when the entry is kept);
  Cranelift's own cache of compiled functions, which blaze also points at the store, hashes
  every function's IR (7.4 s with what is hashed besides); a sealed pack is hashed for its
  name (3.1 s) and read back and checked (2.9 s).
- Finding which function of the program an entry's names stand for
  (`StableNames::functions_named`, 10.1 s on the one thread in the first pass). 6.8 s of
  that is listing every function of every crate once, and 3.9 s is asking for the same
  crate's id and root file again for every name.
- Writing the entries, 3.8 s, and copying them, about 4 s.

The checksums of the records do not show (0.3 s).

**Elsewhere:** Cranelift's verifier is 9.6 s of processor time (4%). It is what turns IR
that blaze got wrong into a function left to rustc instead of a crash or wrong code, so it
stays. Register allocation is 27 s, types 37 s, MIR 21 s; those are the work.

### What follows from it

Small, and not made at this head: compile the functions found on the way on every core
too; keep a crate's part of a name instead of asking for it again.

Larger, not started:

- *Do not start over.* A pass that finds instances it cannot compile, or a static it must
  leave to rustc, throws away a whole object to take out a handful of functions. On cargo
  the second and third pass are about 32 s of the 92 s. Taking the affected functions out of the object being built
  (or finding both conditions before anything is stitched: both are known from the compiled
  instances' requests, which the parallel step has already produced) would make the build
  one pass. Cargo's generic edit (31 to 37 s, the whole object written again) has not been
  profiled; that it is the same restarts is a guess from its log showing several rounds.
- *Stitch, describe and keep on every core.* These are per-function and independent until
  the bytes go into the object; today they wait for one thread. With the restarts gone this
  is the next 30 s on cargo.
- *One hash per entry.* The key computed at the lookup can be kept with the entry, and
  Cranelift's cache under the store's one could go: a build that finds an entry by name never
  asks Cranelift's, and one that does not has changed IR.

## Open at this head

Not done yet from the list this file is for:

- Profiles of the slowest edit of each kind, and of LLVM's cold build. Only cargo's cold
  build under Cranelift has been profiled.
- No fix has been made. Besides what the profile section lists: a refusal costs a whole
  reload, and the first edit after a start with a full store compiles half the program.
- The frontend errors behind the refused edits (`publish` in cargo after a signature edit is
  taken back, `MatchPattern::eq` in nushell from the start) have not been reduced to cases.
- The CI job has not run, and its baseline is from another machine than the runner.
