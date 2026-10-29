---
status: record
summary: Linux AArch64 towards production: the gaps closed and left.
date: 2026-10-20
---
# Linux AArch64: towards production

Branch `blaze-aarch64-prod`, cut from `blaze-collate`. This file is kept accurate for the head
of the branch. "Native" below means GitHub's `ubuntu-24.04-arm` runner, where blaze itself is an
AArch64 process; "qemu" means an x86-64 host that builds for `aarch64-unknown-linux-gnu` and runs
the result under `qemu-aarch64`. The two are never mixed in one number.

## Where it stands

Work on this branch stopped here. This section says what is done, what is half done and what
would come next; the sections after it give the detail.

The head has `blaze-collate-next` at 1b022ff6f2 merged in. That merge has not had a whole gate:
the run on it was stopped before the end. What it showed, on an x86-64 Linux host:

- Cranelift: 21 of the 22 harnesses passed (the table at the end of this file).
  `blaze_cli.py` failed on one check of the daemon's life, "16 clients at once on 4 worktrees
  each run their own tree's program": 15 of 16 were right. That check came with the merge and
  was not looked into, so it is not known whether it fails every time. The five projects of
  that run (ripgrep, fd, bat, cargo, cargo-lib) were the same as cargo at every step.
- LLVM at -O2 with fast emit: the first nine harnesses passed (corpus, programs, stress,
  breakers, unwinds, splices, asmedits, includes, worktrees). The run was stopped in `keys.py`;
  the thirteen after it did not run.

The last head with a whole gate green under both backends is the commit before the merge of
`blaze-collate` 14ee89181b, that is 0a069156c8. On the merge of 3d47ed59bf the Cranelift leg was
green and the LLVM leg was stopped part way.

Nothing on this head has run natively. The last native run is on da50733d91
([run 39093559068](https://github.com/DioxusLabs/rustd/actions/runs/39093559068)), which is
this branch with 14ee89181b merged. Natively in that run:

- Cranelift harness job: every harness right except one row of `debuginfo.py`, which was the
  harness and is fixed (section 2). `storeguards.py` and `selects.py` passed, `remote.py` was
  right on every check, `buildmodel.py` 190 of 190.
- LLVM harness job: failed in `remote.py` on the pull that stops one pack short, which is
  fixed (section 1). Its `debuginfo.py` was all right.
- Both `projects` jobs failed in the harness itself, which killed a served program that had
  already ended; `blaze-collate-next` has a fix for that. Before that, with LLVM, the project
  `just` differed from cargo by one test in two steps (one test fewer in a step cargo built
  for both sides, one more in the next). Not looked into; it looks like the comparison and not
  the build, since the first of the two was cargo's build on both sides.

Done:

- fd's unit test that failed under blaze (section 3), on every platform.
- The pull that now and then stopped one pack short (section 1), on every platform.
- The `debuginfo.py` rows that were wrong natively: both were the harness (section 2).
- The worker that was stopped as idle while loading (section 1).
- `aarch64-unknown-linux-musl`, one attempt (section 4).

Half done:

- The gate on this head, as above.
- Native confirmation of this head: the workflow run on the push is the first.

Not done, and what I would do next, in this order:

1. Run the gate whole on this head and look into the "16 clients" check.
2. Read the native run of this head; the two fixes above have not been seen natively.
3. Look into the one-test difference in `just` natively.
4. `armv7-unknown-linux-gnueabihf` under qemu, one attempt. Not tried.
5. The causes of the musl failures.

Seen on Windows and left to the Windows work: `storeshared.py` with Cranelift fails with
`cache verify` saying a pack is corrupt because access to it is denied while daemons run.
`verify` counts a pack it may not read as corrupt; my guess, not checked, is that Windows
answers the same way for a pack that is being removed.

## What passes natively

From run 39093559068 (da50733d91), on `ubuntu-24.04-arm`, the Cranelift harness job. The LLVM
harness job of that run stopped at `remote.py`, which is last, with the same counts up to there.

| Harness | Native AArch64 |
|---|---|
| `difftest.py corpus --layouts` | MATCH, 825 of 825 functions from blaze, 0 of 903 layouts differ |
| `programs.py` | 22 of 22 |
| `stress.py` | 99 of 99, 0 restarts |
| `breakers.py` | 62 of 62, 7 open |
| `unwinds.py` | 8 of 8 |
| `splices.py` | 71 of 71 |
| `BLAZE_SPLICE=1 farsplices.py` | 6 of 6 |
| `asmedits.py` | 10 of 10 |
| `includes.py` | 9 of 9 |
| `worktrees.py` | 62 of 62 |
| `keys.py` | 46 of 46, 0 open |
| `debuginfo.py` | all right but the `.eh_frame` row of the corpus, which was the harness (section 2); perf named 95 frames in 109 samples |
| `buildmodel.py` | 190 of 190 as expected, 4 rows not run (see below) |
| `storekills.py --kills 100`, `storecorrupts.py --rounds 4`, `storefull.py`, `storeshared.py`, `storeguards.py`, `selects.py` | pass |
| `remote.py` with MinIO's arm64 binary | every check right with Cranelift; with LLVM one wrong, fixed since (section 1) |

`blaze_cli.py` did not finish natively in that run (see above). In the run before it, 39087233000
on 0a069156c8, all eight projects, the flag table and the daemon's life were the same as cargo
under both backends.

The fuzzer's smoke run is x86-64 only in the workflow: only x86-64 has been fuzzed.

## 1. Harnesses that had never run natively

### `buildmodel.py`

190 rows as expected, none wrong. The harness runs both backends itself, so it is in the
Cranelift leg only.

Two cases are marked as cases of an x86-64 host (`"on": "x86_64"` in their `case.json`) and print
"not run" on AArch64, once per backend:

- `cross_musl` builds for `x86_64-unknown-linux-musl`. It is there for another libc of the same
  machine, linked by `musl-gcc`, whose output runs on the host without an emulator. On an AArch64
  host that target is another machine.
- `rustflags_target_cpu` sets `RUSTFLAGS=-C target-cpu=x86-64-v3`, a processor name that only an
  x86-64 compiler knows.

One more row changes its meaning without saying so: `cross_aarch64` builds for
`aarch64-unknown-linux-gnu`, which on this host is the host, so natively it is a build with the
host named and not a cross build. No row builds for another machine from an AArch64 host yet.

### `blaze_cli.py`

The workflow has a `projects` job on `ubuntu-24.04-arm` for each backend. It runs every project
of the harness, the flag table and the daemon's life.

Natively, in run 39087233000, with both backends: ripgrep, fd, bat, cargo, hyperfine, just, axum
and nushell gave the same output and the same test results as cargo at every step, and the flag
table and the daemon's life passed.

Two things were wrong and are fixed:

- The flag case `blaze run --target` named `x86_64-unknown-linux-gnu`. It now names the
  host.
- The daemon stopped a worker as idle while the worker was still loading the workspace, when
  loading took longer than the idle time. The harness sets the idle time to 3 seconds, and with
  LLVM on the arm runner loading takes longer than that, so the worker was gone before it had
  built anything. A worker that is loading is now not idle, its idle time starts when it has
  loaded, and the daemon does not end while it has a worker.

### `remote.py`

The MinIO step of the workflow now runs on both Linux architectures; the image it takes the
binary from publishes both. SeaweedFS is not installed on either runner, so that half is left
out there as on x86-64.

Natively 91 of 91 checks are right under both backends in run 39087233000.

One check failed now and then on the runners, on either architecture and either backend:
"packs the store made one of are not fetched again". 24 packs of 64 KiB are pushed, another
machine pulls them, its store makes one pack of them, and a second pull should fetch nothing.
When it failed, the first pull had stopped one pack short with "incomplete frame". The cause:
a fetched pack is written to a file by a task that runs behind the fetch, and the file was
read again for unpacking before the last write had landed. The fetch now waits for the file
to be written whole first. A unit test pulls such a pack 500 times; on a loaded machine it
failed in 5 of 8 runs of 5,000 pulls without the fix and in none of 8 with it. The same fix
came in with `blaze-collate-next`, and the merge keeps that one. Not yet seen natively.

### `storeguards.py`

In the gate and the workflow since `blaze-security` was merged. It passes natively with
Cranelift (run 39093559068) and on x86-64 under both backends; natively with LLVM that run
shows it passing too, as it comes before `remote.py`.

## 2. `debuginfo.py`

Apart from the one row described below, all rows are right natively under both backends: the frames of each panic are cargo's,
`llvm-dwarfdump --verify` finds no errors, `.eh_frame` agrees with the function ranges for all
11,735 functions of the corpus (with Cranelift), gdb stops at the breakpoint, walks up the frames and steps by
line, and the six edits are spliced into the linked binary with the debug sections kept and still
sound after each.

Locals, with Cranelift: a debugger is told of 12,983 parameters and locals in the corpus, and
gdb prints 9 of 9 in the locals case. With LLVM: 0, as on x86-64 ("LLVM's frames are not read
back yet"). Nothing differs between AArch64 and x86-64 here; the gap is the same one.

With LLVM at -O2 one corpus panic has a frame fewer than cargo's because the optimizer inlined
it (447 frames against 448); the harness counts and allows that, as on x86-64.

perf, unwinding by `.eh_frame`, now runs: the workflow installs it and lets the user sample.
Natively with Cranelift it names 104 frames of blaze's functions in 108 samples, none without
a line, 28 samples unwound back to `main`. Natively with LLVM: 26 frames in 53 samples, none
without a line, 13 back to `main`.

One row was wrong natively now and then, under either backend: "corpus: .eh_frame and the
ranges" found 1 function with lines and no entry in `.eh_frame`. It was the harness. The
function was the naked `stabilized::arm::naked_add`, which has no entry and no lines, as it
should. The row of the line table that ends a sequence is at the first byte after the code it
ends, and when the linker put `naked_add` right there, the harness took that row for a line of
`naked_add`. It no longer counts rows that end a sequence. Not yet seen natively.

The perf row failed once on the x86-64 runner with LLVM ("NO LINE" for `runner::main`). That
was the harness: `main` of the corpus's runner is rustc's code, and rustc gives some of its
instructions line 0. The row no longer counts rustc's line 0 as a missing line of blaze's.

## 3. Real applications through `blaze`

The harness builds, runs and tests each project with `blaze` and with cargo, cold, warm,
after an edit, with the edit broken and with it mended, and compares output and the result of
every test by name.

Natively (run 39087233000, both backends): ripgrep, fd, bat and cargo are the same as cargo's
own build at every step. For cargo that run built cargo 0.99.0 and ran the unit tests of its
library (`cargo test --lib`).

fd was not the same in the run before: `filter::time::tests::is_time_filter_applicable` failed
in the test binary the daemon built and passed in cargo's. That is fixed. The test sets a thread-local clock and then calls a
function that reads it. In a test executable rustc gives a symbol other objects can link to
only to the functions another codegen unit calls; the rest are private to their object and are
run by rustc's test harness as rustc built them. blaze counted such a function as one rustc has
no copy of, so the thread-local it shares with a function blaze did compile was defined twice,
once by each side, and the test set one copy while the code read the other. blaze now reads the
private function symbols of rustc's objects too, leaves those functions to rustc, and treats
the statics they use as rustc's.

On x86-64 with this head, under both backends, ripgrep, fd, bat, `cargo` (0.92.0, the tests of
`cargo-platform`, as `blaze-collate` has it) and `cargo-lib` (0.99.0, the unit tests of cargo's
library) are the same as cargo's own build at all 14 steps each, tests included. The fd failure showed on x86-64 as
well before the fix, so it was not particular to AArch64.

Where the daemon hands a step to cargo rather than building it, the harness says so and it is
not a failure. That happens for the test binary of a package whose unit tests are linked with
its library as rustc built it (ripgrep after an edit, bat when warm), and in cargo itself for
an edit to a function blaze does not compile (`request_helper`).

## 4. Other targets, one attempt each

### `aarch64-unknown-linux-musl`

One attempt, under qemu on an x86-64 host, on an earlier head of this branch; the causes below
were not looked into.

What works as it is, under Cranelift: the corpus matches rustc (825 of 825 functions from blaze,
0 of 903 layouts differ), `breakers.py` 43 of 43, `unwinds.py` 8 of 8, `asmedits.py` 10 of 10,
`includes.py` 9 of 9, `farsplices.py` 6 of 6, `keys.py` 46 of 46. With LLVM the corpus matches
and `unwinds.py` is 8 of 8.

What breaks, in the order it showed:

1. `programs.py`: 9 of 22 under both backends. The first build of each program is right; a
   patched build prints different output.
2. `stress.py`: 38 of 99, with 61 restarts of the daemon.
3. `worktrees.py`: 12 of 62, different output after a checkout.
4. `splices.py` does not get as far as a count: the host's `strip` fails on the linked binary.

The common line is that code built from scratch is right and what comes after an edit is not,
which points at how a patch is linked into a statically linked musl executable and not at code
generation.

### `armv7-unknown-linux-gnueabihf`

Not tried.

## Local gate

Run on an x86-64 Linux host for the x86-64 target on this head, Cranelift first, then LLVM at
-O2 with fast emit. The run was stopped during the LLVM leg. The table is the Cranelift leg;
the LLVM leg had the same counts for the first nine rows and did not get further.

| Harness | Result |
|---|---|
| `difftest.py corpus --layouts` | MATCH, 822 of 822 functions from blaze, 0 of 893 layouts differ |
| `programs.py` | 22 of 22 |
| `stress.py` | 99 of 99, 0 restarts |
| `breakers.py` | 94 of 94, 6 open |
| `unwinds.py` | 8 of 8 |
| `splices.py` | 71 of 71 |
| `asmedits.py` | 10 of 10 |
| `includes.py` | 9 of 9 |
| `worktrees.py` | 62 of 62 |
| `keys.py` | 46 of 46, 0 open |
| `fuzz/fuzz.py smoke`, `fuzz/edits.py smoke` | 2 programs, 0 differences, 0 crashes; edits pass |
| `backends.py` (LLVM leg) | not run |
| `debuginfo.py` | all right; perf named 81 frames in 36 samples, none without a line |
| `buildmodel.py` (Cranelift leg, runs both backends) | 194 of 194 as expected |
| `storekills.py --kills 100`, `storecorrupts.py --rounds 4`, `storefull.py`, `storeshared.py`, `storeguards.py`, `selects.py` | pass |
| `remote.py` with MinIO | 117 of 117 |
| `blaze_cli.py ripgrep fd bat cargo cargo-lib` | the five projects same as cargo at every step; FAILED on one daemon check, "16 clients at once", 15 of 16 |

Nothing of this branch ran under qemu for this head.
