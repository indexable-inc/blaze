---
status: plan
summary: The explorer for any program that emits blaze telemetry, not just the blaze daemon. ix-prof gains the one serving side (feature `explore`, the WebTransport server, certificate, link, token and live stream moved out of blaze-explore) behind a `Source` trait; the protocol speaks of generic things (processes, threads, open spans, tasks with state, progress and a "waiting on", task edges); build steps, crates and build scripts are the `blaze` layer on top, and the daemon is one `Source`. Covers the tables, versions and the gate that keeps ix.dev/explore from falling behind, the page's Running now and Graph views, and the order of the work.
date: 2026-11-05
---
# The explorer for anything that emits blaze telemetry

Andrew, 2026-11-05: `blaze explore` should show what is running right now, and the same page
should open for any Rust program instrumented with ix-prof, not only for the blaze daemon. The
protocol and the views talk about processes, threads, open spans and tasks; build steps,
crates and build scripts are one layer on top.

Today there are two servers. blaze-explore (the daemon's: WebTransport on 127.0.0.1, a pinned
certificate, Arrow answers, the live `spans`/`counters`/`open` stream; [explore.md](explore.md))
and ix-prof's loopback HTTP (`/debug/pprof/*` and `/debug/pprof/live`; [ix-prof-format.md](../../../ix-prof-format.md)).
The page reads only the first, and decodes its live stream wrongly after the first frame.

## Decisions

1. **One serving side, in ix-prof.** `ix_prof::explore` (feature `explore`, off by default) is
   the WebTransport server: the certificate and its rotation, the link and token, the origin
   rule, the framing, `hello`, the request streams and the live stream. Any program gets it with
   `ix_prof::explore::serve(config)` or `IX_PROF_EXPLORE=1` beside `IX_PROF`; that variable in a
   program built without the feature is an error naming the feature. tokio stays in the driver
   module.
2. **A sans-IO session core.** Hello ordering, the token check, the in-flight limit, the live
   emit rules, certificate rotation and task eviction are `explore::session`, a state machine on
   an injected clock (`#![blaze::sans_io]`). Today they sit in async code in blaze-explore's
   `server.rs`; the move puts them in the core rather than carrying them over. The driver does
   the QUIC, the streams and the blocking pool.
3. **A program is a `Source`**, the driver's seam:

   ```rust
   pub trait Source: Send + Sync + 'static {
       fn hello(&self) -> Hello;                          // name, version, layers, processes and their clocks
       fn tick(&self, at_ns: i64, into: &mut Tick);       // this tick's rows of the core tables, as batches
       fn op(&self, name: &str, args: &Value, stop: &Stop) -> Option<Result<Answer, OpError>>; // a layer's ops
   }
   ```

   The core encodes the batches; a source never writes IPC. `stop` is set when the page stops
   the request (blaze's `sql` kills DuckDB on it). ix-prof implements `Source` over its
   `core::Recorder`; blaze-explore implements it for the daemon over blaze-profile's flight
   recorder, the plane and the history, and keeps only the `blaze` layer: the ops `builds`,
   `sql`, `views`, `view`, `spans`, `tiles`.
4. **One log per source, a position per session.** `Recorder::take_live` drains, so two pages
   reading it each would steal each other's rows. Every source's ticks go into ix-prof's
   `live::Log` (retention budget, GAP when a position falls out of it); each session reads from
   its own position. A tick ends with a TICK frame, and the page applies a tick's `tasks`,
   `task_edges` and `open` together on it. A session that joins late first gets a snapshot of
   the live set (threads, open, tasks and their edges), then the log from that tick.
5. **One transport for the page**, WebTransport, once its feed carries everything
   `/debug/pprof/live` does (`cpu_samples`, `frames`, `stacks`, `alloc_events`, gaps). Then
   ix-prof's HTTP live stream and its second encoder go; the `/debug/pprof/*` snapshots stay for
   `go tool pprof`, curl and `blaze pprof`. If Safari's WebTransport turns out to lack
   `serverCertificateHashes` (the probe has not been run there), the HTTP stream stays as
   Safari's route instead.
6. **Versions split.** `hello` carries `protocol` (the core's, owned by ix-prof, starting at 1)
   and `layers: [{name, version}]`. The blaze daemon's layer is `blaze`, versioned by
   blaze-profile's `schema::VERSION` (5 today). Adding a nullable column or a table changes no
   version; the page ignores columns and tables it does not know. An unknown layer is not a
   refusal: its columns show as plain columns and its ops are not offered. An unknown `protocol`
   is a refusal naming which side is behind and the command (`blaze update`, or reload).
7. **Gates both ways.** The page publishes what it reads at `https://ix.dev/explore/schemas.json`
   (`{"protocol": [..], "layers": {"blaze": [..]}}`, with `reads` for the `blaze` layer until
   every released daemon sends `protocol`). blaze's `.github/scripts/explore_page.py` refuses a
   release, and a pull request that moves a version, the page does not read (landed for the
   `blaze` layer, 2026-11-05). In indexable-inc/ix, a test refuses dropping a `protocol` or
   layer version that a released blaze or ix-prof still sends, since programs pin old ix-prof.

## Ids

- A **process** is keyed by its session id (ix-prof's `session_id`, blake3 of pid, start
  times and a nonce), never its pid, which build scripts reuse. `hello` lists each process with
  its pid, name and its monotonic clock's offset from the source's, since time namespaces and
  VMs move `CLOCK_MONOTONIC`.
- A **task** id is never reused within a session. The daemon's plane reuses node slots across
  builds (a free list with generations, `blaze_graph::plane`), so its task id is
  `request << 32 | node`, not the node id.
- Every reference to a task (`parent`, `needs`, `waiting_on_task`) is a (process, task) pair, so
  a client waiting on the daemon, or the daemon on a build script, can be said.
- One set of names and types for both producers: `thread` u64, `span` u64, `name`,
  `thread_name`. ix-prof's `open_spans` becomes `open`.

## The core tables

Arrow; names and `waiting_on` are Utf8, not dictionaries (unique strings would hit the live
dictionary cap and restart the streams).

| table | live rule | columns |
|---|---|---|
| `threads` | upsert by (process, thread) | process, thread, thread_name |
| `spans` | append (closed spans) | process, thread, span, parent, depth, name, start_ns, dur_ns, ... |
| `open` | replace each tick | process, thread, depth, span, parent, name, start_ns, task?, awaits?, owner? |
| `tasks` | upsert by (process, task), whole rows | process, task, parent_process?, parent?, name, state, outcome?, error?, start_ns?, end_ns?, waiting_on_process?, waiting_on_task?, waiting_on?, progress_done?, progress_total?, progress_unit? |
| `task_edges` | append; evicted with their task | process, task, needs_process, needs (the task it cannot finish without) |
| `counters`, `cpu_samples`, `frames`, `stacks`, `alloc_events` | as ix-prof's stream | as ix-prof-format.md section 3 |

- `state` is `waiting` (needs something unfinished), `ready`, `running` or `settled`;
  `outcome` (for `settled`) is the source's word (`built`, `cached`, `cached remote`, `failed`,
  `skipped`, `cancelled` for the daemon) and `error` its message. A row is whole: a null clears.
- A settled task leaves the live set 60 s after its `end_ns`, on the source's `at_ns`, with a
  last row; its edges go with it. The history keeps what the source keeps.
- `open.task` is the nearest task among the span and its ancestors, so the page's join finds
  what a task is doing (its innermost open span), not the task's own span. Which thread works
  for a task is read from `open`, not stored on the task.
- A layer's columns keep their own names (`crate`, `unit`, `kind`, as the history and its
  DuckDB views name them) and carry Arrow field metadata `ix.layer = <layer>`; core columns
  have none.

## Producers

- **Any program (ix-prof).** A `tracing` span is a task when it has a `task` field (its id,
  as ix-prof's `poll` spans already carry a future's address) or a `progress.total` field (its
  id is then the span's); its name is the task's. `span.record("progress.done", n)` moves it;
  `Span::follows_from` adds a task edge; the `wait`/`on`/`owner` fields fill `awaits` and
  `waiting_on`. No new macro.
- **The blaze daemon.** Tasks are the plane's nodes (`blaze_graph::plane`): name the node's
  (`<pkg> <target>`), layer columns `kind` (`NodeKind`), `crate`, `unit`; state and outcome from
  `NodeState`; edges from the plane's deps. The loop, which owns the plane, keeps a
  materialized, versioned task table in `Observe` (so a late session gets its snapshot) and
  updates it each tick a reader exists; the explorer thread never touches the plane. The thread
  working on a node is the one whose open span carries it (`OpenSpan.node`, lane
  fetch-progress). A build script run is held between its steps and in no thread's open stack,
  so it shows as `running` with no thread under it, which is the truth. Downloads
  (`blaze_fetch::downloads`, lane fetch-progress) are tasks with `progress_*` in bytes, ids from
  their own range, and the node waiting on one names it in `waiting_on_task`. Sandboxed build
  scripts and proc macros have no network, so they do not serve: their telemetry reaches the
  daemon over the pipe it already has and joins its `Source` as their own processes.

## The page (indexable-inc/ix, packages/web/src/lib/explore)

Superseded by [explore-core.md](explore-core.md) (2026-11-05): the page's decoding, live state
and trace layout move to `blaze-explore-core` in wasm, and the views are written from questions.
What follows is what the TS page shipped (indexable-inc/ix#7612).


- A stateful decoder per live table (fixes today's: each frame after the first is not a whole
  IPC stream), reset at the end-of-stream marker, feeding one store of the core tables, applied
  on TICK.
- Tabs: **Running now** (default when a source is live), **Graph**, **Trace**, then each
  layer's (`blaze`: Builds, SQL, Views).
- **Running now:** one row per task that is `running`, or `waiting` on something that is itself
  running (a download, a lock), longest first: name, the layer's label (`kind` and `crate` for
  the daemon), what it is doing (the innermost open span, the chain on hover), for how long, the
  thread under it, a progress bar with done/total and rate when it has progress, and what it
  waits on (a link to that task). Counts of `waiting` and `ready` tasks above, not rows: at a
  build's start thousands are waiting. Threads with open spans and no task are listed below, so
  a program without tasks still shows what runs.
- **Graph:** the DAG around a selected task, each node drawn once: what it needs (upstream) and
  what needs it (downstream), by depth, states coloured, the longest unfinished chain marked.
  Search by name. Selecting a task in Running now opens it here.

## Order of the work

1. Gate the page against the daemon's schema (done: indexable-inc/ix#7597, blaze
   `explore_page.py`; ix.dev serves `{"reads":[1,2,3,4,5]}` since 2026-11-05 10:08 UTC).
2. `ix_prof::explore`: the session core and its driver, moved from blaze-explore with the
   `Source` trait and one log per source; blaze-explore becomes the daemon's source. The session
   tests move with it, and the core gets a test on a virtual clock.
3. `protocol`, `layers` and processes in `hello`; the page and both gates read them.
4. `tasks`, `task_edges` and `open.task` in the core; ix-prof's producer, with a test that reads
   a task, its progress and a follows-from edge back through a session.
5. The daemon's producer from the plane and the downloads registry (with lane fetch-progress).
6. The page: the stateful decoder, Running now, Graph (shipped in TS, ix#7612; the rest is
   [explore-core.md](explore-core.md)).
7. The feed carries samples, frames, stacks and allocations; the HTTP live stream goes.
