---
status: plan
summary: The live view of `blaze build`, `check` and `test`, redesigned. One typed model of a build as it runs now (`blaze_protocol::now::Now`: its phase, the daemon's memory against its ceiling and how pressed it is, busy and idle workers with their innermost spans, what the rest waits on and what each blocker holds back, downloads, the slowest so far, how long nothing moved), folded by one sans-IO core from the plane's graph records, the threads' open spans and the downloads, and read by the build's own client, `blaze status` and the explorer's daemon source. Drawn by one sans-IO renderer (`blaze_ui::board`) full screen on a terminal, as a few lines inline, or as a line every few seconds off one; snapshot-tested.
date: 2026-11-05
---
# The build's live view

Andrew, 2026-11-05: during a `blaze test` the view said

```text
 ⠼ blaze test  compiling 15m14s / build ████████▌░ 488/570  std build script 12m28s · libc build script 12m28s · std build script 12m28s +79
```

while 15 of 16 workers were idle and one job had spent 4.5 minutes in codegen of one function
(MIR `copy_prop` asking `is_freeze`). Every unit the line listed was parked behind that job; the
one thing running was not on the line. Earlier a cold build said `compiling 0/0` for minutes
while it was downloading LLVM, and nothing said whether it was stuck.

The view lists what has *begun*, timed from when it began. It does not know what is running on a
thread, what the rest waits for, or what a download is doing. `blaze build` and `blaze check`
have no live view at all: they print cargo's lines.

## What the view must answer

1. What phase the build is in and what it is really doing (downloading LLVM, resolving, waiting
   for its plan, compiling, linking, running tests).
2. How many workers are busy and how many idle, and for each busy one its innermost span: the
   kind, the item's path, the crate, for how long.
3. What the remaining work is blocked on: the few tasks everything else waits for, and for each
   how much it holds back.
4. Each download: bytes, rate, ETA.
5. The slowest tasks so far.
6. The daemon's memory against its ceiling and how pressed it is, always on screen. The same
   day a build reached 99 GiB, and blaze daemons together about 300 GiB, until the Mac
   panicked, with nothing on screen to say so.
7. Whether it has stopped: no progress for N seconds, naming what is in flight, and telling a
   slow job (a worker busy in one function) from a stall (nothing runs, nothing arrives).

## The model: one home

`blaze_protocol::now` (blaze-protocol owns `NodeKind`, `NodeState` and the graph records
already; every crate that speaks to a client depends on it, and it depends on nothing of
blaze's but blaze-serde). It holds the wire types and the fold:

```rust
pub struct Now {            // one build, as of `at_ms` after it began
    pub at_ms: u64,
    pub phase: Phase,       // Starting | Fetching | Resolving | Planning | Compiling | Linking | Testing | Finishing | Running
    pub stage: String,      // what the daemon says the stage waits on: `waits for its pinned native inputs, 1 of 1 layouts left`
    pub stall: Option<String>, // the daemon's top stall in its wait-for graph, in one line, while one lasts
    pub tally: Tally,       // tasks waiting, ready, running, done, failed, total
    pub workers: u32,       // the pool's workers; idle = workers - busy.len()
    pub busy: Vec<Busy>,    // each busy worker: its thread, its task, its innermost span
    pub blockers: Vec<Blocker>, // unfinished tasks with every input done, ranked by what they hold back
    pub downloads: Vec<Download>,
    pub slowest: Vec<Took>, // settled tasks, longest first
    pub quiet_ms: u64,      // since a task last settled, a download got a byte or a span closed
}
pub struct Span { pub name: String, pub item: Option<String>, pub krate: Option<String>, pub awaits: Option<String>, pub for_ms: u64 }
pub struct Busy { pub thread: String, pub task: Option<Task>, pub span: Span }
pub struct Task { pub id: u64, pub kind: String, pub name: String } // kind: the layer's label, a build's `NodeKind::name()`
pub struct Blocker { pub task: Task, pub running: bool, pub for_ms: u64, pub span: Option<Span>, pub holds: u32, pub chain: u32, pub behind: Vec<String> }

// The fold's input, generic so the explorer's wasm32 page folds any program's feed:
pub struct TaskRecord { pub id: u64, pub kind: String, pub name: String, pub deps: Vec<u64> }
pub enum State { Waiting, Ready, Running, Done, Failed, Cancelled } // From<&NodeState>
pub struct Worker { pub thread: String, pub task: Option<u64>, pub open: Vec<Span> }
```

`Fold` is the sans-IO core (`#![blaze::sans_io]`): `take(&TaskRecord)` adds or updates a task,
`moved(id, State)` moves one, and `take_event(&Event)` is the one place the plane's records
(`Event::Node`, `Event::Nodes`, `Event::Moved`) become those (ids widened to `u64`, the kind
`NodeKind::name()`). The clock comes as `advance_clock`, and `look(&mut self, &Look) -> Now`
builds the model from what it holds and the look's inputs (each worker thread's open spans with
its task's id, the downloads' meters, the stage's words). It keeps tasks in slots in arrival
order with an id-to-slot map that is asked, never walked, so no output depends on its order, and
at most `MOST_TASKS` (2^22) of them. It owns no thread, socket or clock, and builds for
`wasm32-unknown-unknown`.

- **Busy and idle.** A worker is busy when its thread is inside a span; its task is the plane node
  its `task` span carries (`OpenSpan::node`, lane fetch-progress), never matched by name.
- **Blockers.** The frontier is every unfinished task whose inputs are all done (running or
  ready). Walking the unfinished tasks in dependency order, each carries the set of frontier
  tasks it waits on (a bitset over the frontier, capped at 256 entries, the rest counted as
  one); a blocker's `holds` is how many unfinished tasks wait on it, `chain` the longest path of
  unfinished tasks behind it, `behind` the first few by name. Ranked by `holds`, then `chain`,
  then age. In Andrew's build the one blocker is `codegen core` (running, 4 min 30 s, in
  `copy_prop` of `core::ptr::is_freeze`), holding 82 and a chain of 41; the build scripts the
  old line listed appear under it as held, not as running.
- **Phase.** Running when no task has a build's kind (one of `NodeKind::ALL`'s names): a program
  that is not a build, whose tasks run with no phase. Otherwise Fetching while a download is in
  flight and no task runs; Resolving and Planning from the stage; then Compiling, Linking or
  Testing by the kinds of the running tasks (a test run over a link over a compile), the kinds
  told by name.
- **Memory.** The look carries what admission reads (`blaze_graph::admission::Pressure`:
  the footprint, the limit, the subsystem past its fair share) and how many ready tasks the
  plane holds back for memory; the fold keeps the build's peak and counts its releases.
- **Progress.** `quiet_ms` restarts when a task settles, a download gets a byte, or the caller
  says a span closed since its last look. A view says *stalled* only when no worker is busy and
  no download moves; with a busy worker it says *slow* and names that worker's span.
- **Lock stalls.** `stall` is the daemon's, not the fold's: its loop watchdog (the stop's guard,
  `serve::stop`) looks at the wait-for graph of named locks, signals and kernel waits
  (docs/v2/plan/design/named-locks.md) at each of its looks, and once a wait is older than
  `STALL_THRESHOLD` (2 s, far under the loop's own 60 s report and 120 s bound) puts the top
  deadlock or convoy's line there (`convoy: 61 behind thread 7 on lock flight.intern (oldest
  2400 ms); the head is running`) and says each new one once as a `blaze::stall` event (`kind`,
  `line`, `head`, `waiters`, `threads`, `oldest_ns`). The board shows it as a red row under the
  warning in the full and inline forms and at the end of the compact line, which `blaze status`
  prints; schema 44.

Readers, all of the same `Now`:

- the build's client: `Response::Doing` (lane fetch-progress, schema 41) carries a `Now` in place
  of its strings, about every 500 ms while the build runs;
- `blaze status`: each build under way (`Status::builds`) is its last `Now`, printed by the same
  renderer's compact form;
- the explorer: the page (indexable-inc/ix) runs the same crate compiled to wasm32, folding the
  live feed's `tasks`, `task_edges` and `open` rows (docs/v2/plan/design/explore-generic.md)
  through the same `Fold`, so its Running now and the terminal are one model, not two that
  agree by care. The daemon's `Source` publishes those rows from the plane as that plan says.

The model compiles to wasm32 (coordinator's decision, 2026-11-05): `blaze_protocol::now` is
sans-IO, depends on nothing but serde and blaze-serde, and a `wasm32-unknown-unknown` check of
blaze-protocol is part of its proof. Its name and place are `blaze_protocol::now` (`Now`,
`Fold`, `Look`, `Worker`, `TaskRecord`, `State`); the explorer lane builds its wasm reader on
these names, feeding any program's live feed through `take` and `moved`.

`blaze debug live` (`blaze_profile::live::Top`) stays the raw view of every thread; the build's
view is per build.

## The renderer

`blaze_ui::board` is sans-IO: a `Board` fed `Now`s and keys (`take`, `key`), with the time
injected, drawn into a ratatui `Buffer` (`render`) or into text (`line`). Three forms:

**Full screen** (a terminal at least 80 by 20, not CI; the alternate screen, so the scrollback
keeps only what stays: diagnostics as they come are printed after the view leaves, with the
summary). What it shows, top to bottom, each section only when it has rows:

```text
 blaze test  compiling                  487 of 570 tasks done · 12 in the last 30s   15m14s
 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━  3 running · 80 waiting
 3 of 16 workers busy · 13 idle
 memory  106.3 GB, no limit
 ⚠ slow: no task settled for 4m30s; 1 of 16 workers busy
 ⚠ longest: codegen core 1.0.0 lib (host), 4m30s, on blaze-worker-3 in core::ptr::is_freeze

 running 3, longest first
   4m30s  codegen     core 1.0.0 lib (host)             blaze-worker-3  core::ptr::is_freeze  core
   2m01s  test        blaze-ui 0.1.0 lib test (host)    waiting on the test process  blaze-worker-5
   31.0s  script-run  ring 0.17.8 build-script (host)   a process, on no worker

 proc macros  1 of 32 expanding
   serde_derive::Serialize  in blaze-api                                                1.2s

 waiting  80 on their inputs · 3 ready for a worker · 2 held back for memory
   codegen core 1.0.0 lib (host)   running 4m30s   holds 82  chain 41
     std build script, libc build script, alloc lib, +79

 downloads
   llvm-21.1.8-aarch64-apple-darwin.tar.xz  ━━━━━──────  34.0 of 410.0 MB  260 kB/s  24m10s left

 failed 1
   bodies foo 0.1.0 lib (host)

 slowest so far
   bodies hir-ty 1m12s · codegen syntax 48s · script-run ring 31s

 e expand · blaze explore for the graph · blaze debug live for every thread
```

Every number is labelled. A task's name is the unit it serves, as a person says it: package,
version, target and what it is built for (`object 0.36.7 build-script (host)`, `serde_derive
1.0.219 proc-macro (macro)`), so two units of one package never share a row's name.

Running and waiting stay apart. A worker is busy only while its innermost span is work: a
thread parked on what another computes is not counted, and a running task whose thread waits
says `waiting on` what its span awaits. A task no worker thread is inside says what it is (a
build script's or a test's process, a download, or a fiber parked). A pool job is named by the
innermost span under it that names an item or a crate, never by its closure's type
(`Pool::submit<...>::{{closure}}`); with none, it is `pool job`. The running section takes the
rows the rest leaves, one task a row, longest first, the rest counted (`… 12 more running`).

The proc macros have their own lane: the `blaze wasm expansion` threads are no pool workers and
serve every build at once, so they are counted apart (`1 of 32 expanding`), each busy one with
its macro and the crate it expands in.

The rate is honest: the tasks settled in the last 30 s, never an ETA for the build, whose
remaining work is not known until its plan settles. `releases` is the front ends' releases
since the build began (a daemon counter the fold takes the difference of). `held_back` stays 0
until the plane counts tasks it held back for memory rather than bytes.

`e` expands every section to all its rows (every busy worker, every blocker, the slowest ten)
and back; collapsed, each keeps to a few rows so the screen never scrolls. Keys are read in raw
mode by the driver, which turns Ctrl-C into the same cancel the signal gives.

The memory line is always there, colored by pressure (calm dim, rising yellow, squeezed and
over red and bold). Over the limit, it moves above the header.

**Inline** (a smaller terminal, or `BLAZE_UI=inline`): the same model in at most ten lines under
the scrollback: the header with the workers busy and the memory (`3 of 16 busy  mem 20.1/24.0
GB`), the stall line when there is one, the longest running tasks in the rows left, the first
blocker, the first download.

When the board ends, the scrollback keeps its summary: the build's time and tasks done, the
five slowest tasks and every failed one, then the tests' summary for `blaze test`. Under the
board, the test view draws only the tests' own rows: its header, build row and held row are the
board's.

**Compact** (off a terminal: CI, a pipe, an agent; `BLAZE_UI=plain`): one line when the phase or
the first blocker changes and otherwise every 10 s:

```text
[15m14s] compiling 487 of 570 done · 1 of 16 busy · 3 running: codegen core 1.0.0 lib (host) 4m30s, test blaze-ui 0.1.0 lib test (host) 2m01s waiting, script-run ring 0.17.8 build-script (host) 31.0s · mem 20.1/24.0 GB squeezed · on codegen core (core::ptr::is_freeze, 4m30s), holds 82 · slow: nothing settled for 4m30s
```

A change of pressure is also due a line, so a log says when memory went from calm to squeezed.

## Proof

- The fold's blocker ranking is hard to check by eye and its interface is stable: a few unit tests
  on small graphs (a chain, a diamond, Andrew's shape: one running codegen with 82 waiting behind
  it) and a Hegel property: every unfinished task waits on at least one blocker, and `holds` sums
  to at least the unfinished count minus the frontier.
- `blaze check -p blaze-protocol --target wasm32-unknown-unknown` passes: the model the page
  runs is the one the terminal runs.
- The renderer is snapshot-tested with fixed `Now`s, sizes and keys (`expect_file!` under
  `crates/blaze-ui/src/board/snapshots/`): full, expanded, inline, compact, a download, a stall,
  and drawing at every size from 0 by 0 never panics and is the same twice.
- On the Mac: a cold `blaze test -p blaze-ui` after deleting the store's LLVM shows the download
  and then the build, captured before and after.

## Order of the work

1. This spec, `blaze_protocol::now` (types and `Fold`) and `blaze_ui::board` with its snapshots.
2. With fetch-progress landed: `Response::Doing` carries `Now`; the daemon's look
   (`serve/job/doing.rs`) feeds the plane's records and the threads into one `Fold` per build;
   `Status::builds` keeps the last `Now`.
3. The driver: `cli/live.rs` grows the full-screen and inline forms for `build`, `check` and
   `test` (the test view's tests section sits under the board's), raw-mode keys, and the compact
   lines for the plain path.
4. The explorer's daemon source reads the same `Fold`.

The losing alternative: the client opens a second connection for `Request::Live` and folds the
raw open spans itself. It has no plane, so it cannot say what blocks what, and it would be a
second model of a build beside the daemon's.
