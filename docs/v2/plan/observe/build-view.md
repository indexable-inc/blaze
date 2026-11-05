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
    pub phase: Phase,       // Starting | Fetching | Resolving | Planning | Compiling | Linking | Testing | Finishing
    pub stage: String,      // what the daemon says the stage waits on: `waits for its pinned native inputs, 1 of 1 layouts left`
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
pub struct Task { pub id: u32, pub kind: NodeKind, pub name: String }
pub struct Blocker { pub task: Task, pub running: bool, pub for_ms: u64, pub span: Option<Span>, pub holds: u32, pub chain: u32, pub behind: Vec<String> }
```

`Fold` is the sans-IO core (`#![blaze::sans_io]`): it takes the plane's records
(`Event::Node`, `Event::Nodes`, `Event::Moved`) as they come and the clock as `advance_clock`,
and `look(&mut self, workers, downloads, stage) -> Now` builds the model from what it holds and
the look's inputs (each worker thread's open spans with their plane node, the downloads' meters,
the stage's words). It owns no thread, socket or clock.

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
- **Phase.** Fetching while a download is in flight and no task runs; Resolving and Planning from
  the stage; then Compiling, Linking or Testing by the kinds of the running tasks (a test run
  over a link over a compile).
- **Memory.** The look carries what admission reads (`blaze_graph::admission::Pressure`:
  the footprint, the limit, the subsystem past its fair share) and how many ready tasks the
  plane holds back for memory; the fold keeps the build's peak and counts its releases.
- **Progress.** `quiet_ms` restarts when a task settles, a download gets a byte, or the caller
  says a span closed since its last look. A view says *stalled* only when no worker is busy and
  no download moves; with a busy worker it says *slow* and names that worker's span.

Readers, all of the same `Now`:

- the build's client: `Response::Doing` (lane fetch-progress, schema 41) carries a `Now` in place
  of its strings, about every 500 ms while the build runs;
- `blaze status`: each build under way (`Status::builds`) is its last `Now`, printed by the same
  renderer's compact form;
- the explorer: the daemon's `Source` (docs/v2/plan/observe/explore-generic.md) publishes its
  `tasks` and `task_edges` rows from the same `Fold`'s tasks, so the page's Running now and
  the terminal agree.

`blaze debug live` (`blaze_profile::live::Top`) stays the raw view of every thread; the build's
view is per build.

## The renderer

`blaze_ui::board` is sans-IO: a `Board` fed `Now`s and keys (`take`, `key`), with the time
injected, drawn into a ratatui `Buffer` (`render`) or into text (`line`). Three forms:

**Full screen** (a terminal at least 80 by 20, not CI; the alternate screen, so the scrollback
keeps only what stays: diagnostics as they come are printed after the view leaves, with the
summary). What it shows, top to bottom, each section only when it has rows:

```text
 blaze test  compiling                                          488 of 570 tasks   15m14s
 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━──────────  1 running · 81 waiting
 ⚠ slow: no task settled for 4m30s; 1 of 16 workers busy

 workers  1 busy · 15 idle
   blaze-worker-3  codegen  core::ptr::is_freeze  core  copy_prop > is_freeze      4m30s

 blocked on
   codegen core                 running 4m30s   holds 82  chain 41
     std build script, libc build script, alloc lib, +79

 downloads
   llvm-21.1.8-aarch64-apple-darwin.tar.xz  ━━━━━──────  34.0 of 410.0 MB  260 kB/s  24m10s left

 slowest so far
   bodies hir-ty 1m12s · codegen syntax 48s · script-run ring 31s

 e expand · blaze explore for the graph · blaze debug live for every thread
```

`e` expands every section to all its rows (every busy worker, every blocker, the slowest ten)
and back; collapsed, each keeps to a few rows so the screen never scrolls. Keys are read in raw
mode by the driver, which turns Ctrl-C into the same cancel the signal gives.

The memory line is always there, colored by pressure (calm dim, rising yellow, squeezed and
over red and bold). Over the limit, it moves above the header.

**Inline** (a smaller terminal, or `BLAZE_UI=inline`): the same model in at most six lines under
the scrollback: the header with the memory (`mem 20.1/24.0 GB`), the stall line when there is one, the busy workers' spans (two), the
first blocker, the first download.

**Compact** (off a terminal: CI, a pipe, an agent; `BLAZE_UI=plain`): one line when the phase or
the first blocker changes and otherwise every 10 s:

```text
[15m14s] compiling 488/570 · 1/16 busy · mem 20.1/24.0 GB squeezed · on codegen core (core::ptr::is_freeze, 4m30s), holds 82 · slow: nothing settled for 4m30s
```

A change of pressure is also due a line, so a log says when memory went from calm to squeezed.

## Proof

- The fold's blocker ranking is hard to check by eye and its interface is stable: a few unit tests
  on small graphs (a chain, a diamond, Andrew's shape: one running codegen with 82 waiting behind
  it) and a Hegel property: every unfinished task waits on at least one blocker, and `holds` sums
  to at least the unfinished count minus the frontier.
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
