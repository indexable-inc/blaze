---
status: plan
summary: The explorer's core moves to Rust compiled to wasm32 by blaze (`blaze-explore-core`), the crate the daemon, the build's live view and `blaze status` already share (`blaze_protocol::now`) plus the wire decoding and the trace's aggregation and layout; the page (indexable-inc/ix) keeps only the shell. The producer serves its own core over the session, so page and producer can never disagree on a schema. Each view is written from a question a person asks (is it stuck, what is slow, why did this rerun, where is X and what does it wait on), not from the tables a producer happens to have.
date: 2026-11-05
---
# The explorer's core, in wasm

Andrew, 2026-11-05, on the deployed explorer and a v0.20261105.1 daemon: Running now was 16
rows of `worker idle 52.0 s` with an empty crate column, it failed on a 64-bit id
(`BigInt exceeds integer number representation`), and Trace said `Pick a build first.` with
nothing on the page to pick one. It shows the tables the daemon has, not answers. The same day
the coordinator decided the core moves to Rust compiled to wasm32 by blaze, so the page, the
terminal and `blaze status` run one model and 64-bit ids are `u64` everywhere.

This plan sits on [explore-generic.md](explore-generic.md) (the protocol, the generic tables,
the `Source` trait in `ix_prof::explore`) and `build-view.md` (lane tui-redesign, landing beside this) (the build's live
model, `blaze_protocol::now`, and its fold). It replaces that plan's page section.

## The questions

Every view starts from a question, and says the answer in its first line:

| Question | Where | The answer's first line |
| --- | --- | --- |
| Is it stuck? | Running now, header | `compiling 488/570 · 1 of 16 busy · slow: nothing settled for 4m30s` (or `stalled`, naming what is in flight) |
| What is it doing? | Running now, busy | each busy worker's span chain as a breadcrumb, `core › codegen › copy_prop › core::ptr::is_freeze  4m30s` |
| What is everything waiting on? | Running now, blocked on | the blockers ranked by what they hold back, each with what it blocks |
| Is memory the problem? | every tab's header | `mem 20.1 of 24.0 GB, squeezed: mir over its share, 3 tasks held back` |
| Where is X and what does it wait on? | Graph | search `std build script`: its state, the thread under it, its inputs not done, the chain behind it |
| What was slow? | Trace, then Builds | the longest tasks and spans of the selected build, aggregated by name, then the timeline |
| Why did this rerun? | Builds | the build's reruns with the key part that changed (the profile's `misses` and `key_parts`) |
| What does this program do now (not blaze)? | Running now | the same view over ix-prof's tasks and open spans: no build, no phase, the same busy, blocked and memory lines |

A tab that needs a build picks one itself: the running build, or else the latest, with the
picker inline (time, command, duration, outcome) and a link to Builds. No state tells a person
to do something the page does not offer right there.

## The decisions

1. **One crate, `blaze-explore-core`**, a `cdylib` built by blaze for `wasm32-unknown-unknown`
   (blaze builds such cdylibs today: docs/platforms.md). It holds:
   - the wire: `hello`, answer and live headers in deterministic CBOR (blaze-protocol's codec)
     and the Arrow IPC tables (arrow-ipc), live streams kept across frames;
   - the live state: the generic tables folded into `blaze_protocol::now::Now` through the same
     `Fold` the daemon's look runs (build-view.md), so Running now and the terminal are one
     model. A program without the `blaze` layer gets a `Now` with no phase and its tasks'
     kinds as labels;
   - the trace: aggregation of up to 10M spans into levels of detail by time bucket and the
     layout of the visible window into instance buffers the page's WebGL renderer draws (rects:
     x, width, row, colour; labels: position and string id). The page never holds a span.

   It is sans-IO (`#![blaze::sans_io]`): bytes in, view models out, the clock handed in. Ids,
   hashes and `_ns` are `u64`/`i64` inside; a view model carries an id as a decimal string and a
   duration as a number of milliseconds, so JS never rounds one.
2. **The shell stays TypeScript** (ix packages/web): tabs, tables, pickers, routing, the canvas.
   It calls the core through a small hand-written ABI over linear memory (`alloc`, `free`,
   `feed(kind, ptr, len)`, `view(which, args) -> ptr, len` of CBOR), no wasm-bindgen. View models
   are small and display-ready; the shell decodes them with its CBOR reader in bigint mode.
3. **The producer serves its own core.** The release archive carries `explore-core.wasm` beside
   the binary (release.yaml builds it with blaze); `hello` names it (`core: {abi, sha256,
   bytes}`) and a `core` op serves its bytes, which the page checks against the hash and keeps
   in the Cache API by hash. A daemon and the core that reads its tables are built from one
   tree, so they cannot drift; the page's gate shrinks to the ABI (`explore_page.py` checks
   `abi`, not `schema`). A dev daemon with no core beside it answers `core` with an error naming
   the path it looked at and the `blaze build` that makes it. An ix-prof program passes its core
   in `explore::Config` (the bytes a blaze build of `blaze-explore-core` wrote) or serves none,
   and the page says the program serves no explorer core and which ix-prof release does.
4. **One home for the model.** `blaze_protocol::now` stays the model (lane tui-redesign owns it
   and its renderer; agreed 2026-11-05). `Fold` takes generic task records,
   `Fold::take(&TaskRecord { id: u64, kind: String, name: String, deps: Vec<u64> })` and
   `Fold::moved(id, now::State)` (`Waiting`, `Ready`, `Running`, `Done`, `Failed`, `Cancelled`);
   the plane's `Event::Node`/`Nodes`/`Moved` convert into them in one place (`Fold::take_event`),
   and the core converts the live feed's `tasks`/`task_edges` rows into them. `Task.id` is `u64`
   (the daemon's is `request << 32 | node`, explore-generic.md's ids), `Worker.task` names the
   task a thread works on, and `Phase::Running` is a program that is not a build. Nothing else
   reads the live tables.
5. **Pages older than the core keep working until the release.** The TS views stay until the
   first release whose daemon serves `core`; from then the page refuses a daemon without one:
   `this daemon (blaze X) predates the explorer core; run blaze update, then blaze explore`.
   No second model is kept beside it.

## The views

- **Running now.** Header: the question's line (phase, busy and idle, memory against the
  ceiling, downloads with bytes and rate, slow or stalled). Busy work: one row per busy worker,
  its chain as a breadcrumb (crate › step › pass › item), elapsed, expanding to the whole span
  stack and, when the sampler has them, its sampled stacks (the signal-free sampler, lane
  deadline-waits). Idle workers are one line (`15 idle`). Blocked on: the blockers, each with
  what it holds back and the first few behind it. Downloads. Threads that are no task's worker
  (the daemon's update check, stop guard, io loop) sit behind a toggle. A step links to its
  node in Graph and its spans in Trace.
- **Graph.** Search first. A selected task shows its state, the thread under it, its inputs
  not done, what it holds back, and the DAG around it by depth, each node once, the longest
  unfinished chain marked.
- **Trace.** The selected build (picked as above); first the longest tasks and spans by name
  (self and total time), then the timeline the core lays out.
- **Builds.** One row per build: time, command, duration, outcome, reruns. A build opens on
  why it reran: the units whose keys changed and the key part that changed.
- **SQL.** The questions above as ready queries, then the editor.
- **Views.** Each view says the question it answers in its title.

## Proof

- `blaze build -p blaze-explore-core --target wasm32-unknown-unknown` in CI, and the core's
  tests on the host: a live stream across frames and a reset; ids at 2^53 + 1 and 2^64 - 1
  through to the view model; the fold over a recorded daemon feed equals the daemon's own
  `Now` for the same records (the one-model claim, tested by comparing bytes).
- The trace layout: 10M synthetic spans aggregate and lay out a window in under 16 ms in the
  page (measured in the browser, spans named).
- On the Mac: a cold `blaze test` opened in the explorer answers each question in the table
  above, captured.

## Order of the work

1. The TS fixes ship first, alone: ids exact in the TS client, errors that name the message
   and field, the inline build picker (indexable-inc/ix).
2. This plan; `Fold`'s generic task input and `u64` ids, agreed with lane tui-redesign.
3. Done. `blaze-explore-core`: the wire and the live state, built for wasm32 by blaze; the
   daemon's source publishes `tasks` (upserted whole, `Rule::Upsert`) and `task_edges` from the
   plane (explore-generic.md step 5): the plane's one record conversion (`now::Records`) feeds
   both its fold and `blaze_explore::Tasks`; a task is `request<<32|node`, downloads are tasks of
   kind `download` with bytes as progress, and `open` gained `task`. The core folds those rows
   through `now::Fold`, and Running now (phase, tally, blockers, downloads with rate, memory,
   slow and stalled, busy rows with their task and stack) comes from its `Now`.
4. Done. The release ships the core (`explore-core.wasm` in each archive, beside `blaze`, which
   reads it from there); `hello` names it (`core: {abi, sha256, bytes}`, the ABI from the
   module's `ix.explore.abi` section); the `core` op answers a header and the bytes. A missing
   core is an error naming the path and the build command, answered to `core`.
5. The page loads the core and draws Running now and Graph from it.
6. The trace in the core; Trace, Builds, SQL and Views redone around their questions.
