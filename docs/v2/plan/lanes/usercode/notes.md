---
status: plan
summary: Design notes of blaze v2's usercode lane: the sandbox's record and report, how a run's result is keyed and found, cargo's rule for what can't be traced, the warm pool, and how the pieces meet.
date: 2026-10-29
---
# blaze v2 usercode: design notes

2026-10-30: the wasm host (`blaze-macro-wasm`), `blaze-wasi`, rust-analyzer's server
(`proc-macro-srv`, `proc-macro-srv-cli`) and `proc-macro-api`'s client are deleted; every proc macro
runs on `blaze-macro-eval` ([dedup.md](../dedup.md#proc-macros-one-engine-blaze-macro-eval)). What
this says of them is the record of the plan at its date.

Notes of the `usercode` lane on [usercode.md](../usercode.md), as of 2026-10-29. Code:
`crates/blaze-usercode/` (the lane's own), the sandbox in `crates/blaze-cargo/src/sandbox*`,
build scripts in `crates/blaze-build-script/`, proc macros in `crates/blaze-expand/`,
`crates/proc-macro-api/`, `crates/proc-macro-srv/`, `crates/blaze-macro-wasm/`,
`crates/blaze-wasi/`.

## The sandbox, from a driver's side

A driver runs a spawn whose `Sandbox` is on through a `blaze_core::Confine`;
`blaze_usercode::confine::Launcher` is the one blaze gives it. It writes a
`confine::Spec` (JSON, unknown fields refused) into the run's scratch directory and has the
driver start `blaze sandbox --v2 SPEC` in place of the program. The launcher makes the
platform's sandbox (namespaces, overlays and seccomp on Linux; Seatbelt and an interposed
library on macOS; an AppContainer in a job object on Windows), runs the program with exactly
the spec's environment, and leaves a report (`trace::Report`, `trace::encode`): that the
sandbox could not be made (nothing ran), or that the program ran and, when it traces, what it
read. `Confine::traced` reads the report back; a report saying the sandbox was not made fails
the spawn, never a run outside one.

## The record

As the program runs, the sandbox appends one `trace::Line` per thing it sees to the run's live
record, before the call it notes goes on: `R` read or run, `L` looked at, `D` listed, `E` a
variable read by name (`*`: the whole environment, for a process whose reads by name can't be
seen or that walked `environ`), `T` untraceable (`clock`, `random`, `network`, `descriptor`),
`W` and `N` refused (no inputs: the refusal is the same each run), `X` the record is not whole.
Each line has a scope, the name the sandbox's marker gave the thread or process (a test, an
expansion), empty for every scope's. A warm process (a proc-macro server, a fork server) is
read after each answer, by scope.

`trace::assemble` turns lines into a `blaze_core::Traced`: sorted, each once, each file hashed
as it is when assembled (BLAKE3 of its bytes; `trace::DIRECTORY` for a directory;
`inputs::SPECIAL` for a pipe, socket or device, which is never read), each listing as the hash
of its sorted names, each variable with its value. `X` is untraceable as `descriptor`. A run
that read the whole environment has every variable and one more entry, `trace::ALL_ENV_NAME`,
whose value is the hash of the names, so a variable added later is one it would have seen.

## Keys

A run's `RunKey` is its code (the content ID of the binary or module) and what it was given
(`Given`: kind, arguments, the variables blaze sets, stdin, named parts such as a macro's input
tokens). Its result is kept under `RunKey::traced(traced)`: the key and every input read with
its value.

To find a result before running, the store keeps the run key's **shapes** (`index::Shape`):
which paths, listings and variables a run read, without values, in a chain of slots under the
run key. Finding is, for each shape, the values of its inputs now (`Shape::values`) and one get
of the key they make. A run key has one shape per distinct set of reads, usually one, so finding
does not slow down as results pile up. The store never replaces a value, so the chain grows a
slot at a time; a slot another daemon filled with another shape is skipped.

What a run reads in its own writable `OUT_DIR` (given empty) it wrote itself and is no input.
What its given key has already (a variable blaze set) is not traced again. The jobserver's
variables (`script::RUN_ONLY`) are of one run alone, as in cargo's fingerprint.

Where inputs are hashed now: `inputs::ViewInputs` reads a view's files through the query
database's `Cx`, so the query that checks a run depends on exactly what the run read, and the
machine's files (toolchain, the C compiler and headers: #1480) through `inputs::Files`, one
append-only table per process (`blaze_ids::Sharded`) of hashes by path and what `stat` said:
a lookup takes no lock, a file is hashed outside any lock, and an edit is a new entry under its
new stamp. `inputs::Now` is one check's view of it with an environment (`inputs::Env`, sorted);
`inputs::Disk` owns its environment and shares the table. A file that is there and can't be
read is an error naming it.

## The environment

The launcher records `E *` (the whole environment) for every traced run, because a process can
walk `environ` with no call the sandbox sees (Rust's `env::vars` on Linux, C's `environ`). So
today every passed-through variable is in every traced key: sound, but a shell that differs in
`SHLVL` or `OLDPWD` reruns build scripts and misses kept expansions. `E *` also covers what the C
library reads through its own internal calls (`TZ`, `LANG`, `LC_*`, `TMPDIR`, `MALLOC_*`, the
resolver's variables), which no interposer sees. Narrowing it takes three owned pieces
(v2/requests/usercode.md, request 6 revised): std calls a hook before it walks; the linker
marks an object whose only `environ` references are std's; and the interposer records `E *` for
an unmarked object and the C library's own reads by name, from the libc the toolchain pins.

## What can't be traced

A run that read the clock, randomness, the network or a descriptor it was given keeps cargo's
rule (`rule::widen`), added to what it was seen to read: its `rerun-if-changed` paths (a
directory meaning everything below it: a `trace::TREE` entry), or, when it printed neither
`rerun-if-changed` nor `rerun-if-env-changed`, its whole package without hidden directories and
`target` (a `trace::PACKAGE` entry), and its `rerun-if-env-changed` variables
(`blaze_build_script::watched`, made a `rule::CargoRule` by `CargoRule::of`). It is never kept
for longer than either rule would keep it.

## Runs

`run::Run` is a run that ends (a build script): from the store with its `OUT_DIR` written back,
or a spawn in a sandbox that traces, its `OUT_DIR` given empty and read back with every path
checked to stay inside (a link out of it is an error), kept when it succeeded. `run::Mode::Verify`
runs anyway and fails naming the run when the result differs from the one kept.
A build script proved pure (`hir_ty_pure`) is also kept under a key of its instance graph's
identity alone, found before it is compiled (`pure`, lane pure-scripts:
[design.md](../pure-scripts/design.md)).

`script::prepare` makes a build script's run from `blaze_build_script::Unit`: a variable cargo
sets from the unit (`Env::set_from_unit`) is given, one passed through from where the build
started is traced (#805); either way the value it saw is in its key. `script::inert` reads a
script that only prints constant directives (`blaze_build_script::elide`, with the compile
flags `elide::flags_allow` accepts) as its result without compiling or running it (#1296);
verify mode runs it as well and `script::verify_elided` compares.

## Speed

What each part matches or beats:

- **Keys** are Shake's constructive traces (*Build systems à la carte*), stored content-addressed
  as Bazel's action cache is, but keyed by what was read rather than what was declared. A shape
  chain makes finding cost one get per distinct set of reads, not per result kept. Bazel's
  action cache checks a digest of declared inputs. We check the actual reads, and as cheaply:
  - **A hit allocates nothing.** `index::find` reads the shape in place, from bytes borrowed
    from the store's guard; on Unix a stored name is the path's bytes, with no `PathBuf`.
  - **Each input is hashed into the result key as it is asked** (`trace::Keyer`). No `Traced`
    is built and none is encoded. The entries go through an 8 KiB stack buffer, so BLAKE3 hashes
    chunks side by side in SIMD lanes instead of one block per tiny update.
  - **The whole environment is merged in order**, from the sorted `Env` that `Inputs::env_all`
    lends. Its names hash is made on the stack.
  - **A shape whose directory is gone stops at that directory**, with no further `stat`s.
  - **What a run is given is hashed once, as it is given** (`Given::part`, `stdin`). A macro's
    tokens are never copied into the key.
  - **Behind the hit**, the kept value is decoded once and moved to the caller, not cloned.
- **Checking inputs** costs nothing inside a view, and three atomic loads for a path outside
  every view while the vfs watch over it has heard nothing, as watchman and Bazel's
  `--watchfs` answer from the watcher.
  - View files come from the query database, which the VFS watcher keeps current (watchman's
    model).
  - `inputs::Files` keys its table by two seeded FxHash passes and compares the stored path;
    BLAKE3 there cost ten times the rest of a check. It keeps each canonical outside path's last answer with its
    `blaze_vfs::Cover` and the generation read before it, under a sequence lock. A miss wants
    the path's parent watched. A path with a symlink, `.` or `..` in it is `stat`ed at every
    check, since a watch hears nothing of a link's target.
  - A changed file is hashed once per process, with BLAKE3's SIMD over 64 KiB reads, and never
    under a lock.
  - A path in a view is named for the database without a copy when it is already
    `/`-separated.
  - A directory's entries say what they are from the listing (`d_type`), so only a link costs
    a `stat`.
  - A package walk pushes and pops one path buffer. Its hash is a Merkle hash per directory,
    by `blaze_query::TreeHash` (each entry by its view content ID and git mode, a link not
    followed), so a tree walked on the disk and the same tree in a view hash alike.
- **Tracing** is unprivileged seccomp user notification on Linux, with
  `SECCOMP_USER_NOTIF_FLAG_CONTINUE` (the kernel runs the call itself once the path is noted, no
  ptrace stop and no emulation) and `SECCOMP_USER_NOTIF_FD_SYNC_WAKE_UP` (the supervisor is
  woken on the held thread's CPU and switched to directly). On macOS it is dyld interposing in the process itself,
  with no context switch a call. Each new line is one `write`. See
  [sandbox.md](sandbox.md).
- **Where it falls short:**
  - Outside paths until the daemon drives `blaze_vfs::Outside`: an unsettled table vouches
    for nothing, so each check `stat`s, as before.
  - The package walk of a script keyed by its whole package, outside a view. Inside one,
    `ViewInputs::tree` asks the view's own tree hash (`Cx::tree`, one query per directory), so
    a check of an unchanged package is O(1) and an edit hashes again only the directories
    above it.
- **Tests** fork from a server per binary, as AFL's fork server does: 0.1 to 0.5 ms a test on
  Linux, against the few milliseconds of spawning one process per test as nextest does. One
  global queue runs the longest first across every binary.
- **Workers**: warm processes are keyed like Bazel's persistent workers, but never by a
  passed-through variable (#805), so a `PATH` change keeps them.
- **Build scripts** that only print constants are read without compiling them (#1296), as
  Relax's no-op detection does, over a tokenizer of this crate's own.

## The warm pool

`pool::Pool` keeps sandboxed processes that serve many requests, found by `pool::WarmKey` (code,
program, arguments, directory, sandbox, the variables blaze sets; never one passed through), each
talking over a Unix socket the pool listens on (`pool::SOCKET`), its live record in a directory
it can't write (`pool::Pool::live`), so what its sandbox says it read is the sandbox's alone.
