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
certificate, Arrow answers, the live `spans`/`counters`/`open` stream; [explore.md](../lanes/observe/explore.md))
and ix-prof's loopback HTTP (`/debug/pprof/*` and `/debug/pprof/live`; [ix-prof-format.md](../../../ix-prof-format.md)).
The page reads only the first, and decodes its live stream wrongly after the first frame.

## Decisions

1. **One serving side, in ix-prof.** `ix_prof::explore` (feature `explore`) is the WebTransport
   server: the certificate and its rotation, the link and token, the origin rule, the framing,
   `hello`, the request streams and the live stream. Everything in blaze-explore that is not
   about builds moves there unchanged in behavior (`cert.rs`, `link.rs`, `frame.rs`, the
   session and live loop of `server.rs`, `tiles.rs`). Any program gets it with
   `ix_prof::explore::serve(config)` or `IX_PROF_EXPLORE=1` beside `IX_PROF`.
2. **A program is a `Source`.** The server asks its source for the core tables and hands it
   any op it does not know:

   ```rust
   pub trait Source: Send + Sync + 'static {
       fn hello(&self) -> Hello;                       // name, version, layers
       fn live(&self, feed: &mut Feed) -> Frame;       // the next live frame (core tables + layer columns)
       fn spans(&self, range: Range) -> Result<Table, OpError>;
       fn op(&self, name: &str, args: &Value) -> Option<Result<Answer, OpError>>; // a layer's ops
   }
   ```

   ix-prof implements it over its `core::Recorder`. blaze-explore implements it for the daemon
   over blaze-profile's flight recorder, the plane and the history, and keeps only the `blaze`
   layer: the ops `builds`, `sql`, `views`, `view`, `world` and the layer's columns.
3. **One transport.** The page talks WebTransport only: requests are multiplexed and
   cancellable, and the pinned certificate rules out DNS rebinding without a Host check.
   ix-prof's HTTP `/debug/pprof/*` stays for `go tool pprof`, curl and `blaze pprof`; the
   page's HTTP live reader (`#live=`, never merged in indexable-inc/ix) is not built.
4. **Versions split.** `hello` carries `protocol` (the core's, owned by ix-prof, starting at 1)
   and `layers: [{name, version}]`. The blaze daemon's layer is `blaze`, versioned by
   blaze-profile's `schema::VERSION` (today 5). The page publishes what it reads at
   `https://ix.dev/explore/schemas.json` as `{"protocol": [..], "layers": {"blaze": [..]}}`
   (`reads` stays beside them until every released daemon sends `protocol`). An unknown layer
   is not a refusal: the page shows its columns as plain columns and its ops are not offered.
   An unknown `protocol` is a refusal that names which side is behind and the command
   (`blaze update`, or reload the page).
5. **The gate.** `.github/scripts/explore_page.py` refuses a release, and a pull request that
   moves `protocol` or a layer's version, when the deployed page does not read it. The page
   moves first (indexable-inc/ix), then blaze. Landed for the `blaze` layer on 2026-11-05.

## The core tables

Arrow, one long-lived IPC stream per table in the live feed as today (schema and dictionaries
once, then batches with dictionary deltas, end-of-stream and a new stream at the cap).

| table | live rule | columns |
|---|---|---|
| `threads` | upsert by (process, thread) | process, thread, name, process_name |
| `spans` | append (closed spans) | as today: process, thread, span, parent, depth, name, start_ns, dur_ns, ... |
| `open` | replace each tick | process, thread, depth, span, parent, name, start_ns, `task`?, `awaits`?, `owner`? |
| `tasks` | upsert by (process, task) | process, task, parent?, name, state, start_ns?, end_ns?, worker_thread?, waiting_on_task?, waiting_on?, progress_done?, progress_total?, progress_unit? |
| `task_edges` | append | process, task, needs (the task it cannot finish without) |
| `counters` | append | as today |

- `state` is one of `waiting` (needs something unfinished), `ready`, `running`, `done`,
  `failed`, `cancelled`. A task settled more than 60 s ago leaves the live set (one row with
  `end_ns`), so the Running now view stays bounded; the history keeps it.
- What a task is doing is not a column: it is the innermost `open` row whose `task` is the
  task's, joined on the page. One concept, one home.
- `waiting_on_task` names another task when the wait is on one (a download, a lock owner's
  task); `waiting_on` is the text the source has (`LLVM 21.1.8 download`, a path, a key).
- A layer adds columns after the core's, named `<layer>.<column>` (`blaze.kind`,
  `blaze.crate`, `blaze.unit`), and nullable.

## Producers

- **Any program (ix-prof).** A `tracing` span is a task when it has a `task` field (its label)
  or a `progress.total` field; `span.record("progress.done", n)` moves it; `Span::follows_from`
  adds a task edge; the existing `wait`/`on`/`owner` fields fill `awaits` and `waiting_on`. A
  task's worker is the thread it is entered on. No new macro.
- **The blaze daemon.** Tasks are the plane's nodes (`blaze_graph::plane`): id the node id,
  name the node's name (`<pkg> <target>`), `blaze.kind` its `NodeKind` (Script, ScriptRun,
  Codegen, Link, ...), state from `NodeState`, edges from the plane's deps, the crate and unit
  from the unit. The daemon loop, which owns the plane, publishes the changed nodes to
  `Observe` each tick a subscriber exists; the explorer thread never touches the plane. The
  worker is the thread whose open span carries the node (`OpenSpan.node`, added by lane
  fetch-progress). A build script run is held between its steps and in no thread's open
  stack, so it shows from the plane as `running` with no worker, which is the truth.
  Downloads (`blaze_fetch::downloads`, lane fetch-progress) are tasks with `progress_*` in
  bytes, and the task that waits on one names it in `waiting_on_task`. The download registry
  gives each entry a task id from its own range (high bit set), so ids never collide with
  nodes.

## The page (indexable-inc/ix, packages/web/src/lib/explore)

- A stateful decoder per live table (fixes today's: each frame after the first is not a whole
  IPC stream), feeding one store of the core tables keyed as above.
- Tabs: **Running now** (default when a source is live), **Graph**, **Trace**, then each
  layer's (`blaze`: Builds, SQL, Views).
- **Running now:** one row per task in `running` or `waiting`, longest first: name, the layer's
  label (`blaze.kind` and `blaze.crate` for the daemon), what it is doing (the innermost open
  span, and its own name chain on hover), for how long, its worker thread, a progress bar with
  done/total and rate when it has progress, and what it waits on (a link to that task).
  Threads with open spans but no task are listed below, so a program without tasks still
  shows what runs.
- **Graph:** the task DAG around a selected task: what it needs (upstream) and what needs it
  (downstream), each as a collapsible tree, states coloured, the longest unfinished chain
  marked. Search by name. Selecting a task in Running now opens it here.

## Order of the work

1. Gate the page against the daemon's schema (done: indexable-inc/ix#7597, blaze
   `explore_page.py`).
2. Move the serving side into `ix_prof::explore` with the `Source` trait; blaze-explore becomes
   the daemon's source. No protocol change; the session tests move with it.
3. `protocol` and `layers` in `hello`; the page and the gate read them.
4. `tasks`, `task_edges` and `open.task` in the core; ix-prof's producer and its test (a
   program with a task, a progress and a follows-from edge, read back through a session).
5. The daemon's producer from the plane and the downloads registry (with lane fetch-progress).
6. The page: the stateful decoder, Running now, Graph.

## Open questions

- Whether `Feed` stays per session (today's cursor per page) or becomes one log with positions,
  as ix-prof's HTTP stream does. Per session until a measurement says otherwise.
- The history of tasks for a finished build (a `tasks` Parquet table) is not in this plan; the
  Graph view of a finished build reads `Event::Nodes` once that is recorded.
