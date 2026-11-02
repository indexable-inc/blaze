---
status: plan
summary: blaze v2 lane cargo: the workspace load and unit graph as queries over a view, the standard library as one more crate graph, the daemon on the plane and the agent API, and the CLI as its client with cargo's flags, output and exit codes.
date: 2026-10-29
---
# blaze v2: lane cargo's design

Lane cargo's notes, as of 2026-10-29: [building.md](../../process/building.md) section 18, #1450, #805, #804, #86, #91, #298, #601.
Requests are in `v2/requests/cargo.md`; the shared types in
`crates/blaze-units/src/query.rs` (feature `query`).

## Parts and owners

| part | paths | child |
|---|---|---|
| the interface: view configuration, mounts, `Plan`, `StdUnits`, `Cbor<T>` | `crates/blaze-units/src/query.rs` | cargo (the lane) |
| the workspace load and the unit graph as queries | `crates/blaze-units/` but `query.rs` and `src/stdlib*`; `crates/blaze-tree/` | cargo-load |
| the standard library as a crate graph; target cfg without rustc | `crates/blaze-units/src/stdlib.rs`, `crates/blaze-units/src/stdlib/`, `crates/project-model/`, `library/` | cargo-std |
| the daemon on the plane, `blaze-core` and `blaze-api`; lowering units to the front end | `crates/blaze-cargo/src/serve.rs`, `crates/blaze-cargo/src/serve/`, `crates/load-cargo/` | cargo-daemon |
| the CLI as a client of the agent API | `crates/blaze-cargo/src/cli.rs`, `crates/blaze-cargo/src/cli/` | cargo-cli |

v1's code in these crates (`blaze-cargo/src/cargo/`, `socket*`, `plan.rs`, `load-cargo`'s
rust-analyzer load, `project-model`'s `ProjectWorkspace`) stays until integration deletes it; v2's
code calls none of it but pure helpers (flag parsing, cargo's message text), and nothing of v2
falls back to it.

## 1. A build is a view with a configuration

A request names a view (`blaze_api::ViewRef`); the session opens it in the VFS and the database
and hands the daemon's builder an `Opened`. The builder opens the same root again in the database
with a `BuildConfig` (`Database::open_with(view, &config.encode())`), the only thing
`Cx::config` reads:

- `invocation`: the cargo command line (`blaze_api::Cargo`, added in schema 5 by
  `v2/requests/cargo.md` request 3), or one the daemon makes of an agent's `Targets` (`-p`, `--profile`, `--target`,
  `-F`, `--all-features`, `--no-default-features`, cwd at the root, an empty environment but
  `PATH` and `HOME` of the daemon).
- `mounts`: where every directory cargo names is: the view at the client's checkout path, each
  fetched source where cargo keeps it, the standard library at `/rustc/<commit>/library`.
- `host` and `library`.

Only the load's first query reads the configuration; it keeps of the environment only the
variables cargo reads by name (`CARGO_*`, `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`, `RUSTDOCFLAGS`,
`CARGO_TARGET_*`, `CARGO_PROFILE_*`, `CARGO_BUILD_*`, `CARGO_HOME`, `HOME`), so no other value is
in a query's value, the image or the store. Build scripts read the rest of it as a traced input
(usercode), so `PATH` keys what read it, never a worker (#805). The whole environment lives in
memory for the request only and is never logged.

## 2. Every file through the database

The workspace load reads by absolute path, as cargo names it. A path's longest `Mount` gives its
origin: the view (`Cx::file`, `Cx::dir`) or another root (`Cx::file_at`, `Cx::dir_at`: request
2). Each read is an edge, so a result is keyed by exactly the files it read: an edit to a source
file reruns nothing of the load, a `Cargo.toml` edit reruns that manifest's queries and what
their values reach (early cutoff below that). A path under no mount fails the load naming it;
nothing reads the disk (no-fallback). blaze-manifest and blaze-metadata read through a `Files`
trait (request 1); the load's `Files` is over `Cx` and the mounts.

## 3. Sources

A registry package is its `.crate`, checked by the lock file's sha256; a git package its tree at
the locked commit; a sparse index file as served. Each is fetched once by the daemon, never by a
query: `Plan` answers `Needs(fetches)` with what it would read that no mount covers; the daemon
fetches them (IO tasks of the plane), checks each (sha256 for a `.crate`, path entries that would
escape refused), puts each into the VFS as a root of its own, keeps `at -> root` durable in its
journal, opens the view again with the new mounts and asks again. A source's root is a function
of its bytes, so every view and project with the same version shares one root and every result
keyed by it: the shared base layer. `--offline` and `--frozen` make a needed fetch an error naming
the package, as cargo's.

## 4. The queries of the load

Kinds in `0x6000..0x6800` (request 2): `PLAN` and `STD_UNITS` fixed in `query.rs`; cargo-load's
own in `0x6010..0x6400`, cargo-std's in `0x6400..0x6800`. Each value is a `Cbor<T>` with a
versioned schema. cargo-load splits the load so each query reads the narrowest thing: the
invocation (the configuration, narrowed), the config chain (`.cargo/config.toml` files from cwd up,
`$CARGO_HOME/config.toml`, `--config`, `CARGO_*`), one manifest read and normalized, the workspace
root and its members, the lock file, the resolve with features (blaze-resolve, guided by the lock),
the unit graph (blaze-units' port of cargo), and per unit what cargo passes and writes
(`UnitPlan`: `-C metadata` and `extra-filename` as cargo computes them, so file names are
cargo's; cfgs, check-cfg, rustflags, lints, `CARGO_*` variables, outputs in cargo's layout). The
standard library's units come from `StdUnits` and are merged as `-Zbuild-std` merges them
(`is_std`, every unit depending on the std roots its kind needs).

cargo's refusals stay cargo's, word for word; what blaze has not ported is an error naming it
(#601: artifact dependencies are read and resolved, and only a build that includes a unit with one
is refused, naming it).

As written, the resolve, the unit graph and the plans run in one function under `Plan` and
`Foreign`: the cutoffs are at the file queries, `ManifestQuery`, `WorkspaceQuery`, `ConfigChain`
and `Asking`, and the tests hold those.

What it measures against: rust-analyzer's salsa project loading and Buck2's DICE, which are
incremental per file read as this is. It goes finer than both, and than rustc's red-green graph,
in two ways. A `Cargo.toml` is kept as canonical TOML, so an edit to a comment or the layout stops
at that file's `ManifestQuery`. A target is found from its directory's listing, never by reading
it, so an edit to a source file reruns no query of the load. Cold, it reads no more files than
cargo's `Workspace::new`. The longest mount is a binary search per ancestor (O(depth · log
mounts)), file bytes are CBOR byte strings, and blaze-manifest runs over a snapshot of sorted path
columns. blaze-manifest stops at the first member manifest it lacks, so each round fetches every
sibling's: a workspace takes as many rounds as its members are deep. What is left: a package's
summary goes through JSON (request 19, with request 1), and the sibling fetch runs one after
another until `Cx::get_all` (request 20).

## 5. The standard library

`library/` (rust-lang/rust at `library/UPSTREAM`'s commit) is put into the VFS once as a root and
mounted at `/rustc/<commit>/library`, the paths rustc's own std has in panics and `file!()`. Its
workspace is loaded by the same queries, asked for the crates a build needs (`std`, or `core`
and `alloc` for `#![no_std]`; `proc_macro` for proc macros; `test` for test harnesses) with
`-Zbuild-std`'s features and the build's panic strategy, for each platform the build has (the host
for build scripts and proc macros, `--target`). std's build script runs sandboxed like any other.
The target's cfg, data layout and std's target facts come from blaze's own target table, never
`rustc --print` (no-rustc). Its results are keyed by the library's root, the target and the
profile's std settings, never the view: the shared base layer (#86).

Speed. A cache hit is the database's memo, so it runs none of this code and allocates nothing.
The measure for a miss is rustc's own `--print cfg`: about 20 ms, because it starts a process
and loads LLVM's target machine to list features. The data is compiled once per process into
per-target columns. Feature names are interned to bits of a 32-byte set; every recorded target
has at most 92 such names, and the set's limit of 256 is checked. Each feature's implications
and turn-offs are precomputed sets, as are the target spec's facts and every
`target_feature="…"` line. `target_cfg` is then a binary search for the triple, four word
operations per `-C target-feature`, and one sorted copy out, about a microsecond. Once set,
reads take no lock. The checks `StdUnits` makes of a loaded graph are O(units) with one
`FxHashMap` of package names. Not reached yet:
- The JSON (423 KB) is still parsed at the first call, about a millisecond per process. Reaching
  zero needs a build step that writes the table as `static` Rust data
  (`harness/answers.py --write` could emit it beside `rustc.json`).
- `StdUnits` decodes `Foreign`'s CBOR and writes it out again. Passing the bytes through needs a
  `Persist` for already-encoded CBOR in `query.rs`.

## 6. The daemon

`blaze serve` (`crates/blaze-cargo/src/serve*`): one process, one `blaze_core` driver, one loop:

- the agent API's `Server` (Unix socket, HTTP) and `Session`; the daemon is the session's
  `Builder`;
- the `Vfs` (journal, the workspace heads the CLI's checkouts are, their watchers);
- the `Database` (from its image at start, so nothing is loaded again: `Database::from_image_with`;
  `write_image` and `settle` after a cold build; `collect` between requests);
- the store (`blaze_cas::Store`);
- the plane: every job's queries, fetches, codegen units, links, build scripts and tests are its
  tasks, admitted against the memory budget, run on its worker pool.

A job asks `Plan`, fetches what it needs, then builds the units: the front end's inputs from the
units (load-cargo lowers `Units` to base-db's crate graph), build scripts and proc macros through
blaze-usercode, codegen through blaze-codegen's driver, linking through blaze-link, outputs placed
where `UnitPlan::outputs` says, and reports `Unit`, `Diagnostic`, `BuildScript`, `Built`. A
build's view is the checkout as saved: a CLI request names the head of its checkout, and the daemon
holds it until the head's watcher has seen every change made before the request (a cookie file
written by the daemon in the checkout's `target/` and waited for, as watchman does).

The front end's database is shared by reference, never cloned (decisions.md): the loop owns each
job's lowered front end, and a task that reads it holds a shared reference for its run. A build
script's output or a proc macro's load is a write, applied once the readers posted before it
have returned and before any reader is posted after it. Every write ready at that point is applied
as one, so a wave of build scripts is one stop; tasks that don't read the front end keep running.
A git fetch clones into a directory of its own under the daemon's state and is removed once read:
no lock, nothing written to the user's `$CARGO_HOME`.

## 7. The CLI

`blaze build|run|test|check|bench` and `cargo blaze <same>`: parse as cargo does (v1's
`cargo/cli.rs` rules), find or start the daemon (`blaze serve`, socket under blaze's home), commit
nothing (the watcher has the edits), send `Request::Build` / `Check` with `Targets::cargo`, and
print what cargo prints, on the stream cargo prints it, with cargo's exit codes:

- `Compiling foo v0.1.0 (/w/foo)` per `Response::Unit`, rustc's text per diagnostic, `warning:
  `foo` (lib) generated 2 warnings`, `error: could not compile `foo` (bin "foo") due to 1 previous
  error`, `Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.12s`; with
  `--message-format json*`, cargo's JSON messages on stdout (`compiler-message`,
  `compiler-artifact`, `build-script-executed`, `build-finished`); with `short`, rustc's short
  form of each diagnostic and cargo's own lines unchanged.
- `run`: builds, prints `Running `target/debug/foo``, and runs the program itself with the
  terminal, its arguments and the command's environment, exiting with its code.
- `test`/`bench`: builds the test targets, then runs each test executable (and the doc tests)
  as cargo does (`Running unittests src/lib.rs (target/debug/deps/foo-0123)`, `Doc-tests foo`),
  stopping at the first failing one without `--no-fail-fast`; exit 101 on a failure.
- blaze's own `test` flags, opt-in and never on cargo's command line: `--reached` (the tests
  an edit reached) and `--ub [--ub-seed N] [--ub-schedules K]` (each selected test in the MIR
  evaluator under `hir-ty-ub`, nothing compiled). Both are run by the daemon and sent with
  libtest's selection arguments; the CLI prints their `TestResult`s in libtest's format.
- exit codes: 0, 101 for a failed build or test (cargo's), 1 for cargo's usage errors.

The CLI reads no manifest itself: the daemon's load is the one load. It has no `unsafe`, and
`cli.rs` says `#![forbid(unsafe_code)]` for all of it (the crate can't: its sandboxes and sockets
need `unsafe`).

Fast: the CLI is a thin client, as buck2's and Bazel's clients are: what it does before the
request is parse the words, stat up from the directory for `Cargo.toml` and `.git` (one stat per
directory, `cargo.toml` looked for only on failure), stat `.cargo` once per directory outside the
checkout and read the config files without a stat, and connect (polling a just-started daemon's
socket every millisecond). Responses are handed over owned, so the build's artifacts are moved,
never copied. Each response is printed with one write, from one buffer the printer reuses. The
JSON messages are written straight into it in cargo's field order, with no `serde_json::Value`
tree between. Strings are escaped as serde_json escapes them: a 256-entry table, with runs that
need no escape copied whole, and with rustc's color codes stripped in the same pass for
`rendered`. sonic-rs's SIMD escaping is faster on long strings, but a cargo message is short and
mostly plain ASCII, so the table copy is the right tool. A source file a span points into is read
once, with its lines indexed as 4-byte offsets found by `memchr` (core's `match_indices('\n')`).
It is not mmapped, because a file edited while the build runs would fault (SIGBUS). A name seen
before costs one Fx hash of its directory and name, and one lookup, and allocates nothing.
Diagnostics are deduplicated by a 64-bit Fx hash of their text, as cargo's `DiagDedupe` does, and
units are found by a hash of their package, target and mode.
`--reached` groups its results in one Fx pass, and libtest's text is built in one preallocated
`String`.

## 8. State of the art, by part

What each part matches or beats (bounds and arithmetic: [speed-of-light.md](speed-of-light.md);
the rules: [principles.md](../../goals/principles.md), "Fast code, line by line"):

| Part | Measured against | How it matches or beats it |
|---|---|---|
| Workspace load | Buck2's DICE, rust-analyzer's salsa project load, rustc's red-green graph | an edge per file read, early cutoff on canonical TOML and on directory listings, so a source edit reruns no load query |
| Mounts and file reads | a kernel's mount hash | binary search per ancestor over sorted columns read in place; a hit allocates nothing |
| Resolve | cargo's resolver guided by the lock | the lock taken as the answer, the resolve rerun only when a manifest's dependencies change; index lines parsed lazily per matching version, as cargo's index cache |
| cargo tree (`blaze-tree`) | cargo's `ops/tree`, CSR graph traversal (GAP, Ligra) | CSR edges per kind built by counting sort, bitset visited and on-stack sets, specs matched once into bitsets over packages, one output buffer; the printed text is the query's value, so a repeat is a memo hit |
| Standard library | `-Zbuild-std` | the same pipeline, keyed by none of the view: one graph per target and profile in the shared base layer |
| Target table | rustc's built-in target specs | to match: the table compiled in, a lookup. Today `rustc.json` is parsed once per process (speed-of-light.md, the standard library) |
| Daemon start | Bazel's and buck2's servers | the database image mapped in place; nothing loaded again on start |
| Daemon's jobs (`serve/job.rs`) | `slotmap`'s and `thunderdome`'s generational arenas, a game engine's entity table | no hash lookup where an index works. A job is found by its `JobId`'s slot with one compare of the generation, so a late report for a cancelled or finished job is dropped, not a panic. An effect of the loop's own or a run of user code is a `blaze_mem::Slab` slot whose handle (slot and generation's low bits) its `Tag` carries, so a completion is an index and a compare. A node finds its job by binary search over the batches' first ids (one SoA column), and paths and checkouts are interned to `u32` once where they come in, each table after that a column. This replaces `FxHashMap<JobId, Job>` with an `.expect` per lookup, the `Tag`-keyed maps and `BTreeMap<u32, ..>` of nodes. The store's placement tags are its own, so they stay hashed |
| Heads | watchman's cookies | one cookie per request through the watcher; nothing walked |
| Front end per job | rust-analyzer's incremental database | one `Sync` database by reference, kept across jobs, new views applied as tree diffs; writes batched per wave |
| Fetch | cargo's HTTP/2 multiplexed downloads | one connection per registry, checked and unpacked in memory, mounted as content-addressed roots |
| CLI | buck2's and Bazel's thin clients; sonic-rs and simdjson for JSON strings; a dense-id table (an ECS) for units | one socket round trip; responses read in place (`ResponseRef`); units and packages found by the daemon's dense indexes; one write per burst the daemon sent (`Client::call_ref`'s held flag), lines in the order made; the printer allocates nothing per response once warm; strings escaped by an 8-byte SWAR scan, byte for byte serde_json's; `blaze-served` lines after `blaze-work`; `--reached` grouped by counting sort over the executable's index (speed-of-light.md, the CLI) |

Each child applies the rules to its code as written, not only to new code; the gaps left are in
speed-of-light.md, each with its owner.
