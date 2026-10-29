---
status: record
summary: Soak and robustness of the daemon.
date: 2026-10-20
---
# Soak and robustness of the daemon

Branch `blaze-soak`, from `blaze-security` c0dfb021cb merged with `blaze-collate` 8f21942505.
Everything here was run on one Linux x86-64 machine (8 cores, 31 GB, Rust 1.99.0, LLVM 20).
Where a sentence is something I measured it gives the command or the number; where it is
something I read off the code or concluded, it says so.

## What is in the branch

- `harness/soak.py`: the soak. One daemon, a seed, a state file it resumes from.
- `harness/soak/`: a small workspace with a build script that writes code, a proc macro and
  a program with unit and integration tests. It is the fifth workspace of the soak and the
  fixture of the rows.
- `harness/soakrows.py`: twenty-two file, toolchain and memory edge cases, one row each.
  `harness/SOAKROWS.md` is what it wrote last.
- `BLAZE_MEMORY_MAX`: a ceiling on what the daemon's workers hold, with eviction.
- Fixes for what the soak, the rows and the lock audit found (listed below with their tests).

## The soak

    BLAZE_MEMORY_MAX=6G BLAZE_BIN=.../blaze python3 soak.py --seed 101 --hours 3 \
        --worktrees 2 --work /dev/shm/soak-cl --sample 0.25

It copies ripgrep 15.1.0, fd v10.3.0, bat v0.26.0, axum axum-v0.8.6 (its `hello-world`
example server) and the build-script workspace into a work directory, each
as a git repository with `--worktrees` checkouts, plants a few marked blocks of code in each
(a struct, a constant, functions that call each other, a test), and starts one daemon. Each
step draws from the seed:

| step | weight | what it does |
|---|---|---|
| edit | 48 | one to three of: body, call, signature, field type, constant, function added or removed, file added or removed, an error planted or mended, a dependency's version in the lockfile, the package's version, its profile, its features |
| lock | 2 | the lockfile changed |
| macros | 6 | the build script, its input, or the proc macro edited |
| git | 12 | commit and checkout of another branch, rebase, stash and pop, a checkout that touches many files |
| worktree | 4 | a worktree added, or removed (and sometimes made again at the same path) |
| burst | 8 | 12 to 40 clients at once, `blaze build`/`test`/`run` and the direct client mixed, over several trees |
| cancel | 8 | clients sent SIGINT or SIGKILL at a random moment of their request |
| kill | 4 | the daemon or a worker sent SIGKILL, then the next request |
| collect | 4 | `blaze-engine cache gc --max` while builds run |

After a step, with probability `--sample`, each tree it touched is built or tested with
`blaze` and with cargo into an empty target directory, and the program's output or
the list of test results is compared. Every request records who built it (the daemon, or
cargo and the reason the daemon gave). Every step appends a line to `metrics.jsonl`: the
resident memory, descriptors and threads of the workers and of the daemon, the size of the
store, and each request's latency. A failure prints the seed, the step and the command that
runs it again, and keeps the two outputs, the status, the diffs of the trees, the logs of
the daemon and its workers and the objects the daemon linked under `failures/STEP`. Refusals
that are not of the ordinary kind (a link that failed, a worker that ended or did not
answer, a panic) are kept under `odd/STEP` the same way. `--resume` goes on from the state
file; `--steps N` with the same seed and arguments is the reproduction.

Two things about this machine shaped the runs. The root disk had 4 to 8 GB free, so the
work directories are on a 16 GB tmpfs, which holds two worktrees per workspace and not
more; the soak deletes target directories when the disk runs low. And a soak ran beside
the regression gates for part of the time, so latencies are not benchmark numbers.

The runs, all with seed 101 under Cranelift, `BLAZE_MEMORY_MAX=6G`, two worktrees per
workspace and a quarter of the steps compared with cargo
(`python3 soak.py --seed 101 --hours 2 --worktrees 2 --work /dev/shm/soak-cl --sample 0.25 --quiet`;
`python3 soak.py --report DIR` prints the tables these numbers come from):

| head | steps | hours | requests | built by the daemon | compared with cargo | failures | flaky | daemon kills |
|---|---|---|---|---|---|---|---|---|
| 1d70c30788 | 223 | 1.23 | 1156 | 215 | 241 | 2 | 4 | 6 |
| 260255fa9b | 345 | 2.00 | 1932 | 355 | 400 | 4 (steps 71, 137, 211, 265) | 10 | 9 |
| 9cef93dc14 | 215 | 1.17 | 1105 | 192 | 233 | 1 (step 137) | 5 | 6 |

That is about 4.4 hours and 783 steps of soak, all under Cranelift. No soak has run on the
head with the fix for step 137, and no soak has run under LLVM: the one LLVM attempt
(seed 202) stopped in its first comparison with an exception in the harness
(`blaze_cli.py`, `difference` given an exit code where it expects text), which I have not
fixed. A two and a half hour run under each backend on the fixed binaries was queued behind
the gates and was stopped before it began when this session was wound down.

Only about a sixth of the requests were built by the daemon. Most of the rest fell back to
cargo with "the daemon has not loaded this workspace", which is what the memory ceiling and
the daemon kills produce with ten checkouts under a 6 GB ceiling. So the soak exercises
loading, eviction and falling back much more than it exercises long-lived warm state.

Growth, in the 345-step run and the 215-step run (least-squares slope per 1000 steps;
measured over all workers and the daemon together):

| | 260255fa9b, 345 steps | 9cef93dc14, 215 steps |
|---|---|---|
| resident memory | 2101 MB first, 4728 last, 10056 most; +815 MB | 2101 first, 6083 last, 9007 most; +1830 MB |
| descriptors | 13 first, 33 last, 423 most; +82 | 13 first, 373 last, 373 most; +456 |
| threads | 16 first, 30 last, 131 most; -6 | 16 first, 60 last, 119 most; +18 |
| the daemon alone, memory | 15 MB first, 12 last, 28 most | 15 first, 13 last, 20 most |
| the daemon alone, descriptors | 6 first, 7 last, 15 most | 6 first, 10 last, 15 most |
| store | 33 MB first, 83 last, 1002 most | 33 first, 811 last, 811 most |

What I read from this, as inference: the daemon process itself does not grow. The totals
follow how many workers are alive when the sample is taken (median 3, most 10), so the
slopes of runs this short say more about where the run happened to end than about a leak;
the two runs disagree in sign on threads and by five times on descriptors. A run of
thousands of steps is needed before a slope means anything, and has not been done.

## What the soak found

Fixed, each with the test or row that holds it:

1. `blaze test` ran the test binary of a proc-macro crate without the toolchain's
   library directory on the loader's path: exit 101 where cargo gives 0. The path cargo
   sets (toolchain, profile and dependency directories) is now set. Row `tests of a proc
   macro`.
2. The doc tests of a proc-macro crate were not run where cargo runs them. Same row.
3. A worker panicked while loading a package that holds a file with no text. bat's tests
   create and delete a file while the daemon loads; a `.rs` file that is not UTF-8 is the
   same path. Such a file is no source of a crate. Row `names and text that are not UTF-8`.
4. Every `.lock().unwrap()` in `crates/blaze-engine` and the store's two locks: one build that
   unwound holding one made every later build of that worker panic, or turned the store off
   without saying so. See the audit. Tests
   `a_lock_a_thread_panicked_with_is_still_taken` and
   `a_thread_that_panics_with_the_store_stops_the_writer_for_the_build_and_no_longer`.
5. An interrupted build (`salsa::Cancelled`) was read as "the function is gone" by the
   outline and by `probe` in the patcher. It now goes on unwinding. Test
   `a_probe_that_is_interrupted_goes_on_unwinding_and_one_that_panics_finds_nothing`.
6. Workers were never released before their idle time, and the worker of a deleted
   worktree lived as long: 16.8 GB after eight steps. `BLAZE_MEMORY_MAX` and the check for
   a checkout that is gone. Rows `memory ceiling`, `no memory ceiling`,
   `worktree deleted and recreated`.
7. A link that failed said only "the link failed (exit status: 1)": the linker's output
   was thrown away. The last lines the linker said are now in the reason the client is
   given. Test `a_link_that_failed_says_what_the_linker_said_last`.
8. With that, the commonest refusal of the soak could be read. Two worktrees of one
   project are served by one worker, which reads the second as a view over the first and
   links its object against what rustc built in the second's own target directory. When
   the two were built by rustc from sources that differ, the second's libraries do not
   hold every generic instance the object refers to (`undefined symbol: <u8 as
   TryFrom<u64>>::try_from`, referenced by a function of a library crate), and every build
   of that worktree was refused and built by cargo for as long as the daemon lived. Never
   wrong output, but the daemon was of no use to that worktree. A checkout read as another
   whose link fails is now loaded by itself from then on: one refusal, then daemon builds.
   Row `worktrees whose libraries were built apart`.
9. The rest of the edge-case table below (symlinked root, stale-id panic reports, a daemon
   whose executable is gone, a workspace under a non-UTF-8 name).
10. Wrong output. Seed 101, Cranelift, step 211: `blaze run -p soak-app --features soak` in the
   build-script workspace printed `Reading/7 ... alpha 12` where cargo's cold build printed
   `Reading/18 ... alpha 23`: the output of another worktree. A worker that held
   several checkouts ran cargo for every one after the first in the first one's target
   directory, so that a dependency would be told one `OUT_DIR` and be one crate for all.
   Cargo names what it builds of a workspace's own packages by their place in the
   workspace, not by where the workspace is, so two worktrees shared the build script's
   output and the proc macro's library there; cargo took whichever was written last as
   fresh for a worktree whose files were older, and the daemon expanded the one
   worktree's code with the other's macro and table. Each checkout now runs cargo in its
   own target directory. What that costs: a dependency with a build script is a crate
   per checkout in such a worker instead of one (not measured separately; `worktrees.py`
   is 62 of 62 as before). Reduced to the row "worktrees built by other code" in
   `harness/soakrows.py`: two worktrees whose proc macro and build-script data differ,
   the one asked second having the older files. It is WRONG on the build before the fix
   (`Reading/5` for the worktree that should print `Reading/7`) and right after.

11. Wrong output. Seed 101, Cranelift, step 137, in every run of that seed: `blaze
   test` in fd reported `filter::time::tests::is_time_filter_applicable` failed where
   cargo's cold run passed it. The test sets a clock through a `cfg(test)`
   `thread_local!` and the code under test reads it. The cause is wider than the thread
   local. The soak builds with `CARGO_INCREMENTAL=0`, and built that way rustc makes
   local to its object every function that nothing outside the object calls (`t`, not
   `T`, in `nm`). The daemon's table of rustc's symbols read only what another object can
   name, so for such a function it found no symbol, took that to mean rustc has no copy,
   and compiled its own beside rustc's (`342 unsupported: no rustc symbol for function` of
   367 in the worker's log). What rustc compiled went on calling rustc's copy. Measured on
   the fd tree of that step with the build before the fix (`CARGO_INCREMENTAL=0
   CARGO_PROFILE_TEST_DEBUG=0 blaze test --bin fd`, wait for the daemon, edit,
   again): after a function was added to `filter/time.rs` the test above failed (blaze's
   copy of a function read blaze's copy of the thread local, rustc's test wrote
   rustc's), and an `assert_eq!(1 + 1, 3)` added to the body of a test was answered
   `Finished` by the daemon with the test still passing: the edit was lost. With the
   default incremental build none of this happens (the same two edits were right), which
   is why I could not make it happen outside the soak before. The table now also reads
   the functions an object keeps to itself, and such a function is left to rustc with the
   reason "rustc's object keeps the function to itself"; the machinery that already
   existed for functions left to rustc then finds its statics to be rustc's and refuses
   an edit of it ("is not compiled by blaze and rustc's copy is from before the edit,
   build with cargo"). After the fix the same commands give 137 passed after the added
   function, and the failing assertion is reported, built by cargo. What it costs: in a
   test program built without incremental compilation nearly every function is kept to
   an object, so an edit to one is built by cargo, stated and counted, until cargo's
   build is loaded again. Row `functions rustc keeps to an object` (WRONG before: "0 for
   101", the failing test not seen; right after) and test
   `a_function_kept_to_its_object_is_no_symbol`.

Not fixed:

- Seed 101, Cranelift, step 71 of the last run: during a burst of 31 clients the daemon's
  log says a worker of bat's second worktree, which the memory ceiling had just stopped,
  had printed `panicked at crates/span/src/ast_id.rs:852` (an id of a syntax node that the
  file no longer has). The comparison of that step was right, and the panic was caught
  (the worker ended by the daemon's SIGKILL, not by the panic), but a panic that is
  expected is not supposed to be printed and one that is not expected should not happen.
  The worker's log was written over when the worker started again, so I have the line and
  no backtrace. I infer it is a thread that asks for a removed function's syntax outside
  the `Probing` mark, like the two fixed in the rows. A worker's log should be kept across
  its restarts so the next one can be read; that is not done.
- A worker that is stopped by the ceiling while a client's request is on its way gives
  that client "the daemon's worker ended (signal: 9)" and cargo builds. Right output, and
  counted, but the reason reads like a crash. Seen at steps 71 and 88.

## Edge cases

    BLAZE_BIN=.../blaze python3 soakrows.py

Each row builds or tests with `blaze` and with cold cargo and compares, and most also
require that the daemon (not the fallback to cargo) did the last build. The table is in `harness/SOAKROWS.md`: 22 of 22 right under Cranelift on 4eaebf61f2; under
LLVM 21 of 21 on 9cef93dc14, with the newest row not yet run there.

| asked for | row | what it found |
|---|---|---|
| symlinked file, symlinked directory | `symlinked file`, `symlinked directory` | right from the start |
| symlinked workspace root | `symlinked workspace root` | the daemon loaded the link's path and looked crates up by their real path: the checkout failed to load on every request (cargo built, so the output was right, but the daemon never did). The workspace is now the path it links to, and rustc's output is matched by file identity where the two spellings differ |
| save by rename, save by truncate-and-write | rows of those names | right |
| changed twice within the timestamp granularity | `changed twice within the time stamp` | right: the daemon reads contents, not times |
| checkout that changes thousands of files | `checkout of thousands of files` (3000) | right output, but the worker printed a panic per removed function ("Can't find ast id"): the mark that says a panic is expected was not carried to the threads that lower functions. Fixed |
| case-only rename | `case-only rename` | same panic report, same fix |
| worktree deleted and recreated at the same path | `worktree deleted and recreated` | right; the worker of a deleted checkout used to stay until the idle time and is now stopped when its directory is gone |
| non-UTF-8 paths | `names and text that are not UTF-8`, `workspace under a name that is not UTF-8` | a file with no text (not UTF-8, or deleted while loading) panicked the worker; a workspace under a non-UTF-8 name panicked on an `assert_utf8`. The first is skipped as no source; the second is refused with the reason and cargo builds |
| workspace on another filesystem from the store | `store on another filesystem` | right |
| rustup switching the toolchain under a live daemon | `toolchain file switched` (1.99.0 to 1.94.0 and back) | right: the worker of the other toolchain is another worker |
| two toolchains in turn | `two toolchains in turn` | right |
| daemon binary replaced, version mismatch | `daemon binary replaced` | the new client is refused by the old daemon, which ends, and starts its own: right. A daemon whose executable was deleted and not replaced stayed forever; it now ends |
| memory | `memory ceiling`, `no memory ceiling` | below |
| (from the soak) | `tests of a proc macro` | the two bugs under "What the soak found" |
| (from the soak) | `worktrees whose libraries were built apart` | the link failure under "What the soak found" |
| (from the soak) | `a function that is another under test` | an attempt to reduce the one unexplained failure; it passes |

## Locks and cancellation

I read every `Mutex`, `RwLock`, `Condvar`, `Once`, `OnceLock` and `LazyLock` in `crates/blaze-engine`
and `crates/hir-ty/src/mir/codegen*`, and every place that catches an unwind. The tables are
what the code does, read, with a test where one is named; I did not edit
`crates/blaze-engine/src/socket.rs`.

How a build is cancelled. A worker process holds one database. Builds read it from threads
of their own; a write to it (an edit arrived) calls `trigger_cancellation`
(`Builds::settle`, `crates/blaze-engine/src/main.rs`), which makes every query on every reading
thread unwind with `salsa::Cancelled`. So "the holder is cancelled" and "the holder
panics" are the same event for a lock: the thread unwinds through whatever guard it holds,
and std marks the lock poisoned. A client that disappears is not seen by the worker at all:
the daemon's handler thread waits for the worker's answer, then fails to write it to the
closed socket and ends; the build itself runs to its end and its result is kept.

The rule the code now follows everywhere: no lock in `crates/blaze-engine` or the codegen store
is held across a query, so what it guards is whole between any two statements, and a
poisoned lock is taken again (`crate::lock`, `PoisonError::into_inner`). The one lock whose
contents may be half-done is the store's writer, which has its own rule below.

| Lock | Where | Held across | Holder cancelled or panics | Was | Now |
|---|---|---|---|---|---|
| `Store::sealed` (RwLock) | `codegen/store.rs` | map lookups and inserts, file reads | poisoned | every later `get`/`insert` treated the poison as "no store": the store was silently off for the life of the worker | taken again; a record read is still checked against its CRC. Test `a_thread_that_panics_with_the_store_stops_the_writer_for_the_build_and_no_longer` |
| `Store::writer` (Mutex) | `codegen/store.rs` | appends to the open pack | poisoned, possibly mid-record | same: store silently off | poison cleared, the writer is stopped as after a failed write (counted in `dropped`, said once), the open pack is cut at its last whole record when sealed, the next build writes again. Same test |
| `read` in `Store::read_packs` | `codegen/store.rs` | a `push` | cannot be poisoned by a query; scoped threads | fine | unchanged |
| `ASSEMBLED` (OnceLock<Mutex>) | `codegen/asm.rs` | map get/insert | poisoned | insert skipped: every `global_asm!` assembled again each build | taken again |
| `Entries::known`, `BANNED` | `codegen/cached.rs` | map ops | poisoned | already taken again | unchanged |
| `Tally::fallbacks` | `codegen/backend.rs` | map ops | poisoned | already taken again | unchanged |
| `Ahead` mutex + `idle` condvar | `codegen.rs` `compile_ahead` | queue ops; `compiled_instance` runs outside it | a thread unwinding sets `stopped` in a drop guard and wakes the rest, so no thread waits for a count that never drops | fine (the poisoned lock found earlier) | unchanged |
| `SELECTED`, `TARGET` x3, `TOOLCHAIN`, `FETCH_MISSING`, `STORE`, `LEAN`, `TALLY` (OnceLock/LazyLock) | codegen | initialisers read the environment or build an ISA; none runs a query | cannot be cancelled; a panicking `OnceLock` initialiser leaves it empty and the next caller runs it again; `LazyLock` would poison but its initialisers cannot panic | fine | unchanged |
| `DISABLED` (Once) | `codegen/llvm.rs` | one FFI call | cannot be cancelled | fine | unchanged |
| `Contents::crates`, `places`, `HASHES` | `blaze/content.rs` | map ops; `file_text` is read before the lock | poisoned | already taken again | unchanged |
| `Contents::cargo_home` (OnceLock) | `blaze/content.rs` | reads the environment | no query | fine | unchanged |
| `Listed::functions`, `Listed::built` | `blaze/main.rs` | map ops; the listing query runs outside | poisoned | `.lock().unwrap()`: one cancelled build while another thread held it made every later build of the worker panic | `crate::lock`. Test `a_lock_a_thread_panicked_with_is_still_taken` |
| `LINKED` (LazyLock<Mutex>) | `blaze/main.rs` | map ops; the link and the splice run outside | poisoned | `.lock().unwrap()` | `crate::lock` |
| `as_built` | `blaze/main.rs` | map ops | poisoned | `.lock().unwrap()` | `crate::lock` |
| outline's maps | `blaze/outline.rs` | map ops | poisoned | `.lock().unwrap()` | `crate::lock` |
| `sent` | `blaze/deps.rs` | an insert, from tokio tasks | a task that panics | the insert was skipped, so a unit that was pushed was pushed again next time | taken again |
| `Keeper::lazy` (RwLock), `Keeper::seen` (Mutex) | `blaze/remote/keeper.rs` | pointer swap, a digest | poisoned | lazy hydration refused for the life of the worker | taken again |
| `Keeper::steps` (OnceLock) | same | channel creation | no query | fine | unchanged |
| pack `OnceLock` in `remote.rs` | `blaze/remote.rs` | key list of a pack, fetched once | initialiser fails: stays empty, next caller fetches again | fine | unchanged |
| `Worker::child`, `checkouts`, `asked`; `Daemon::workers`, `asked` | `blaze/cargo/daemon.rs` | map ops, `kill`/`try_wait`; `Daemon::warm` holds `workers` while it stops one worker process and starts another (no wait on a build) | the daemon runs no query; a handler thread that panics | already taken again | unchanged |
| store `lock` file (flock) | `codegen/store.rs`, `remote.rs` | collection, format change | the process dies: the kernel drops the lock | fine (`storekills.py`) | unchanged |
| `daemon.sock.lock` (flock) | `blaze/cargo` | daemon start | same | fine | unchanged |

Cancellation points and what catches the unwind:

| Where | Catches | Then |
|---|---|---|
| `Program::sync` thread, `main.rs` | every unwind of a build | when `INTERRUPTING` is set the build is counted as interrupted and started again after the write; otherwise the client is told "a build panicked" and cargo builds |
| `outline`, `outline.rs` | a listed function the edit took away (asking for its body panics) | was: any unwind, a cancellation included, read as "a function is gone". Now `Cancelled` goes on unwinding. Its threads are marked `Probing` so that the expected panic is not reported; the mark was not carried to the threads `lower` spawns, which printed panics for every removed function (seen as "Can't find ast id" in the rows for a case-only rename and a 3000-file checkout) |
| `probe`, `codegen/patch.rs` | a query on something the base had and the edit removed | was: `Cancelled` swallowed too, so an interrupted patch read "the base does not have it" and went on until its next query. Now `Cancelled` goes on unwinding. Test `a_probe_that_is_interrupted_goes_on_unwinding_and_one_that_panics_finds_nothing` |
| `compile` around Cranelift, `codegen.rs` | a Cranelift panic on one function | the function is refused with the reason and counted; no query runs inside, so no cancellation. It does hold the store's locks through `Lookup`, which is how a poisoned store lock could arise |
| corpus build per crate, `main.rs` | a panic of one crate's build in a batch | written to the report as `panicked:` |

Left as it is, on purpose:
- `Worker::ask` reads the worker's answer with no timeout. A build has no upper bound, so there is no right number; a worker that hangs holds one daemon thread and its client until the client is interrupted. `BLAZE_MEMORY_MAX` and the idle time never stop a worker that is answering, so a hung worker is not reaped either. This is the one thing in the audit I would still change (a watchdog on a worker that makes no progress), and did not.
- A client that is killed does not cancel its build. The result is kept and the next client gets it, so the work is not lost, but nothing is saved either.
- `Daemon::warm` holds the map of workers while it kills one worker and spawns another (not timed; a status request waits for it).

## Memory

Measured before the ceiling (commit 1d70c30788, Cranelift, seed 101, the five workspaces in
two worktrees each): eight steps in, the daemon had ten workers that held 16.8 GB resident
together, 118 descriptors and 120 threads. The daemon process itself was 19 MB. Nothing was
released: a worker lived until `BLAZE_IDLE_SECS` (1800 s) without a request, and the worker
of a worktree that had been deleted lived as long. A worker is one process per workspace and
build configuration (`dev` and `test` are two), and holds the whole analysis of the
workspace and its dependencies: about 0.9 GB for the small build-script workspace and 1.3
to 2.5 GB for the others, as `blaze status` now prints per worker.

What was added. `BLAZE_MEMORY_MAX` (a size such as `6G`, or `off`; by default half of the
machine's memory) is the most the workers may have resident together. Once a second the
daemon sums what they hold, and while it is over, stops the worker that was asked longest
ago, never one that is answering a request and never the one asked last. It also stops a
worker none of whose checkouts exists any more. What is dropped is the whole worker, which
is the unit that can be dropped safely: the next request for that workspace is built by
cargo (the reason given is "the daemon has not loaded this workspace"), and the daemon
loads it again behind that build, taking stored code from the disk store.

Measured after: in the 215-step run on 9cef93dc14 the median of the resident memory samples was
5064 MB, the 95th percentile 6141 MB and the most 9007 MB, with 9 samples over 6144 MB; in
the 345-step run on 260255fa9b the median was 5034 MB, the 95th percentile 6091 MB, the most
10056 MB, with 14 samples over. Before the ceiling one sample of the same soak showed
16.8 GB over 10 workers, 118 descriptors, 120 threads and 19 MB in the daemon. The samples
over the ceiling are from between a worker growing and the daemon's next look, and from
loads of several workspaces at once; the ceiling is checked when a request arrives, not
continuously. The rows `memory ceiling` and `no memory ceiling` show an evicted workspace
built right afterwards. The cost is visible in the soak: requests for a stopped worker get
"the daemon's worker ended (signal: 9)" or "has not loaded this workspace" and are built by
cargo.

What I infer and did not measure: a single workspace larger than the ceiling is not helped,
since the one worker asked last is never stopped; and resident memory is read from
`/proc`, so the ceiling is only enforced on Linux (elsewhere the sum is zero and nothing is
evicted; I did not run it there).

## Validation

Everything here is on Linux x86-64, run locally; CI has not run on this branch. The branch
is from `blaze-collate` at 8f21942505 with `blaze-security` merged; it does not have
`blaze-collate-next` merged.

The last whole gate that passed under both backends was on 9cef93dc14, the commit before
the fix for step 137: all 21 commands under Cranelift and all 24 under LLVM exited 0,
including `remote.py` (117 of 117), `buildmodel.py` (194 of 194), `backends.py --levels 2f`
and `soakrows.py` (21 of 21 under each).

On 4eaebf61f2, which adds the fix for step 137:

- Cranelift: every command of the gate exited 0, `soakrows.py` 22 of 22 and `remote.py`
  117 of 117 among them, except `keys.py`, 44 of 46.
- LLVM: the gate was stopped in `fuzz/fuzz.py smoke`. The 18 commands before it exited 0,
  except `keys.py`, the same 44 of 46. `blaze_cli.py`, `buildmodel.py`, `backends.py`
  and `soakrows.py` did not run on this commit under LLVM.
- The two `keys.py` cases are the row "a cfg of RUSTFLAGS" in its two modes. The row sets
  `RUSTFLAGS`, which replaces the `-C link-dead-code` every harness gets from
  `difftest.py`; without it rustc keeps two functions of the fixture to one object, and
  since the fix those are counted as left to rustc where before they were quietly taken to
  have no copy. The head changes the row to keep `-C link-dead-code` beside its cfg. That
  change has not been seen to pass: both reruns were cut short.
- `cargo test -p blaze-engine --release kept_to_its_object` passes. The whole
  `cargo test --release -p hir-ty -p hir-def -p parser -p syntax -p blaze-engine` last passed on
  9cef93dc14 and was not rerun after the fix; nor was clippy, which was clean there.
- `cargo xtask tidy` fails on two `#[should_panic]` tests in `crates/blaze-engine/src/main.rs`
  that come from `blaze-collate`.
- `cargo fmt --all -- --check` was not confirmed clean on the head.

## What is left

Half done:

- The fix for step 137 (functions rustc keeps to an object) is in and its row passes, but
  the gate is not green on it: `keys.py` needs the rerun described above, the LLVM gate
  needs finishing, and tests, clippy and fmt need rerunning.
- That fix has a cost that is not measured. In a build where rustc keeps private functions
  to their object (`CARGO_INCREMENTAL=0`, or one codegen unit, without `-C link-dead-code`),
  an edit to such a function is refused and built by cargo. How often that happens in an
  ordinary `blaze` session with incremental on should be counted before this is
  merged. The better fix is to have the daemon own every caller of such a function, so it
  can replace the function and its callers together.
- No soak on the fixed head and none under LLVM.

Open, found by the soak and not fixed:

- Step 71 (seed 101, head 260255fa9b): a worker panicked at `span/ast_id.rs:852` while
  loading bat during a burst of 31 clients. The client fell back to cargo and the output was
  right. Not reduced, because the worker's log is overwritten when the worker restarts.
- Step 265 (same run): after the package version of ripgrep was edited, `blaze sync`
  printed a value that folds `env!("CARGO_PKG_VERSION")` from before the edit. It did not
  reproduce by hand on the later binary with one or two worktrees, and the rerun on
  9cef93dc14 ended at step 215, before reaching it. It may be the same cause as step 211
  (fixed); that is a guess.
- The LLVM soak's harness exception in `blaze_cli.py` `difference`.

Open, from the audit:

- `Worker::ask` reads the worker's answer with no time limit, so a worker that hangs holds
  its client for ever.
- `Daemon::warm` holds the map of workers while it stops and starts workers.
- The probing mark is not carried to every lowering thread.
- A worker's log is overwritten at restart, which loses the panic that ended it.
- A single workspace larger than the ceiling is not helped by the ceiling, and the ceiling
  only works on Linux.

To restart the soak (binaries from `cargo build -p blaze-engine --release`, and with
`--features llvm` for the second; `--resume` continues a work directory that was stopped):

    cd harness
    BLAZE_MEMORY_MAX=6G BLAZE_BIN=.../blaze python3 soak.py --seed 101 --hours 2.5 \
        --worktrees 2 --work /dev/shm/soak-cl --sample 0.25 --quiet
    BLAZE_BACKEND=llvm BLAZE_OPT=2 BLAZE_EMIT=fast BLAZE_MEMORY_MAX=6G BLAZE_BIN=.../blaze \
        python3 soak.py --seed 202 --hours 2.5 --worktrees 2 --work /dev/shm/soak-ll \
        --sample 0.25 --quiet

What I would do next, in order: rerun `keys.py` and the rest of the gate on the head; fix
the harness exception and run both soaks for several hours; count how often the new refusal
fires with incremental on; reduce step 71 by keeping worker logs across restarts.
