---
status: plan
summary: The build driver as a graph that does no IO: what is wrong with the serial driver and what replaces it.
date: 2026-10-28
---
# The build driver: a graph that does no IO

The compiler's core (parser, `hir-def`, `hir-expand`, `hir-ty`, MIR) is salsa queries that
read nothing but their inputs. The driver around it is not. Loading a workspace, running build
scripts, starting proc-macro servers, reading and writing the store, fetching from the remote,
linking and running tests are calls made one after the other from `blaze-engine`'s `main` and
from `blaze-cargo`, each doing its IO where it stands. This note says what replaces that and in
what order. Lane sansio does the work (2026-10-28).

## What is wrong with the driver as it is

- **Phases are serial.** A worker loads the workspace (cargo's metadata, every build script,
  every proc-macro server, every file into the vfs), then resolves names, then compiles, then
  writes the store, then links. Nothing of a later phase starts before the earlier one is over,
  though most of a later phase depends on a small part of the earlier one: `regex`'s names can
  be resolved while `ring`'s build script still runs.
- **Within a phase, the schedule is ad hoc.** Build scripts and proc macros have a ready queue
  of their own (`compile_time.rs`, a mutex, a condvar and a quarter of the cores). Names are
  resolved in a list ordered after dependencies, where a thread whose crate's dependencies are
  not done resolves them itself, one after the other (`resolve_all`). Codegen is a pool per
  crate. None of them knows about the others.
- **There is nothing to show.** A cold ripgrep says "Loading workspace 18.8s" and then one node
  "rg 31894 fns". A cold anim `--release` spent about 100 s on one core with no line in its log
  (coldfast's HANDOFF, 10:38Z). What a build waits for is not written down anywhere, so it can be
  neither drawn nor measured.
- **It cannot be tested without doing the IO.** The order things happen in, what is cached,
  what a failure stops, can only be checked by running a build.

## The design

### The graph

A build is a graph of nodes. A node is one piece of work with explicit inputs:

| Node | Its work | Waits for |
|---|---|---|
| `Manifest` | read a manifest and the lock file, resolve | nothing |
| `Fetch(pkg)` | the package's source, local or from a registry | `Manifest` |
| `Script(pkg)` / `ScriptRun(pkg)` | compile a build script / run it in the sandbox | what it is compiled with / the compiled script |
| `Macro(pkg)` | a proc macro's library, built and loaded | its dependencies' scripts and macros |
| `Std` | the standard library, from the store or compiled | nothing (fetched ahead) |
| `Names(crate)` | the crate's def map (salsa) | its sources, scripts, macros, dependencies' `Names` |
| `Bodies(crate)` | inference, borrowck, MIR of its functions (salsa) | its `Names` |
| `Codegen(unit)` | compile or take from the store a unit of functions | the unit's `Bodies` |
| `Store(unit)` | write what was compiled | its `Codegen` |
| `Link(program)` | spawn the linker | every `Codegen` it links, `Std` |
| `Test(binary)` | run a test executable | its `Link` |

A node's state is one of `Waiting` (some input is not done), `Ready`, `Running`, `Done` with
how (`Built`, `Cached { local | remote }`), `Failed`, or `Skipped` (an input failed: the node is
never started, and says which). States only move forward, and each move is recorded.

### The step function

The core is a state machine in its own crate, `blaze-graph`, with no IO in it:

    Machine::step(&mut self, input: Input) -> Vec<Output>

An `Input` is the build's start, or the completion of a request the machine made (with the
result and the time it finished, given to it: the machine reads no clock). An `Output` is a
request, or a transition (node, from, to, at). Requests are of two kinds:

- **IO**: read files, stat or watch paths, fetch a blob from the local store or the remote,
  write store entries, run a build script or a probe, spawn the linker, run a test binary.
- **Work**: run a node's CPU work, the salsa queries or codegen of a `Ready` node.

The machine does no fs, net, process, thread or clock call; a test checks its sources for
them. Given the same inputs it gives the same outputs, in the same order: of the ready nodes,
the one with the longest remaining path to the end of the build goes first, so the critical
path is never behind work off it; ties go by node id. That makes it testable with recorded
completions: a **trace** is the machine's configuration and the inputs of a real build, a line
of JSON each, as the executor keeps them when asked (`blaze_graph::trace`). The corpus under
`crates/blaze-graph/traces/` replays each one and checks the outputs against its `.out` file, so
any build can become a test.

### The executor

One executor per process performs the requests and feeds back completions:

- IO goes to a pool of threads (io_uring on Linux and kqueue on macOS later, behind the same
  interface; the machine does not change).
- Work goes to a pool of one thread per core. A salsa task runs on its own clone of the database
  (a snapshot); every ready node runs at once, as cores allow.
- Setting salsa inputs (a crate graph, a script's output, a file's text) needs the database
  with no snapshots alive. The machine knows what is running, so it asks for a `Commit` of the
  inputs that are ready when no `Work` holds a snapshot, and holds back the `Work` that needs
  them until it is done. In the first steps of the migration, as today, every input is
  committed before the first `Work` starts; what changes first is that the input-producing nodes
  run concurrently and the `Work` nodes in DAG order. Committing while work runs (an input per
  crate, or salsa sets that do not cancel unrelated queries) is a later step, measured.

### Events

The machine's transitions are the build's events. Each node is announced once with its kind,
name and inputs, and each state change is a record of `blaze-protocol` (#878's stream), so
`blaze build`'s live view, `blaze status`, `blaze watch` and `blaze why` show the graph itself:
what waits, runs, came from the store (local or remote), is done, and the critical path. Lane tui
renders them; nothing else writes build events for the nodes the machine has.

## Migration

Each step keeps behaviour identical (the harnesses stay green), is measured against main with
`harness/bench.py` on ripgrep, anim and cargo in an ix VM, and lands on its own. A cold build
must get faster and use the cores; a no-op or an edit must not get slower (no-op budget +0.2 ms).

1. `blaze-graph`: the graph, the states, the step function, the trace format and its replay
   tests, and the executor with its pools. Nothing uses it yet.
2. Build scripts and proc macros (`compile_time.rs`'s ready queue) run as `Script`, `ScriptRun`
   and `Macro` nodes, with their events.
3. Name resolution runs as `Names` nodes in DAG order instead of `resolve_all`'s list, with
   `Bodies` behind them.
4. The workspace load as nodes: the manifest and lock, fetches, file reads per source root.
5. Codegen units and store writes, so a unit's store write overlaps the next unit's compile.
6. Link and test runs (with lane hermetic).
7. Inputs committed while work runs (above), once 2 to 6 show where the barrier costs.

Lane coldfast finds and fixes the measured costs of a cold build; this lane owns the shape of the
driver. Where a fix of theirs and a step here touch the same code, the two say so in their
HANDOFFs first and one of them does it.

The rules are those of the rest of blaze: no rustc, no fallback (a node that cannot run fails
the build and names itself; nothing runs it another way), no `unwrap` on what another process
sent, builds and benches in ix VMs.
