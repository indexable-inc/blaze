---
status: plan
summary: lock policy (lane lock-policy, 2026-11-04, approved by Andrew) - and, since 2026-11-20, when an in-memory Lock earns its place (D179's exception, its lock record, the lint and ratchet, every lock in shipped code and its shape) - CI refuses any change to the lock files, naming each move, why and the fix; locally a build writes them but never silently; `blaze.lock` beside `Cargo.lock` pins the blaze release and its nightly, the index's newest publish time and every pinned native input's digests; `lock = required` refuses locally too and blaze's tree sets it; what is built and what is left.
date: 2026-11-04
---
# Lock policy

Lane lock-policy, 2026-11-04. The design Andrew approved that day, and what this lane built of
it. The user-facing description is docs/cargo.md, "Lock files"; this file says why and what is
left.

The last section, "A lock earns its place" (2026-11-20, D179), is about the other kind of lock:
the in-memory `Lock` and `ReadLock` of blaze-core, when one is allowed, how its waits are
recorded, and what each one in shipped code should become.

## The rules

1. **CI refuses.** With `CI` set (not empty, not `false`, not `0`) or `TF_BUILD` set, `--locked`
   is the default. A missing lock file, or a resolve that would change it, is an error naming
   each package added, removed or moved (old → new), why (which requirement of which package
   asked for it), and the fix (`blaze lock` locally, then commit). `--no-locked` opts out;
   `BLAZE_LOCK=write` opts out once for everything a harness starts (a typed `--locked` wins).
2. **Locally, never silent.** With no lock file, blaze resolves, writes `Cargo.lock` and says one
   line: `wrote Cargo.lock: 143 packages, index as of 2026-11-04T10:12Z (commit it)`. A change
   forced by a manifest edit says what moved, and stays minimal: the resolve keeps every
   locked package the manifests still allow, as cargo's does.
3. **The lock covers everything blaze pins:** the blaze release, the nightly it ships
   (`pins/toolchain`), every pinned native input's digests (`[workspace.metadata.blaze.native.*]`)
   and the index's snapshot time. Pinned inputs that disagree with the lock are a lock change.
4. **Opt-in strict mode:** `[workspace.metadata.blaze] lock = "required"` refuses locally too.
   blaze's root `Cargo.toml` sets it.

The decision (`crates/blaze-units/src/lock.rs`, `policy`), first row that applies:

| Asked | A change to a lock file |
|---|---|
| `--locked`/`--frozen` and `--no-locked` | error naming both flags |
| `--locked`/`--frozen` | refused |
| `--no-locked`, or `BLAZE_LOCK=write` | written, said |
| CI | refused |
| `lock = "required"` | refused |
| otherwise | written, said |

## Where the pins live: `blaze.lock`

A second file at the workspace root, `blaze.lock`, not `Cargo.lock`'s `[metadata]`:

- `Cargo.lock` stays byte for byte what cargo writes. cargo, `cargo metadata`, cargo-deny,
  cargo-audit, Dependabot and Renovate read and rewrite it; anything blaze adds would be theirs
  to drop or keep.
- cargo's `[metadata]` is a flat map of strings that `cargo update` copies through untouched. A
  pin there would go stale without a word the first time a person ran cargo, which is the silent
  drift this policy exists to stop. `blaze.lock` is written only by blaze, so what it says is what
  blaze last checked.
- Nested, typed tables (`[native.llvm.targets]`) read and diff better than encoded strings.
- One home for every pin that is not a crate (`one-concept-one-home`). #864's networked build
  steps (a step's output hash) belong here as `[step.<id>]` tables when they are built, not in a
  third file.

The format (version 1; unknown keys and other versions are errors naming the file, never
guessed at):

    # blaze.lock: what blaze builds this workspace with besides the crates in Cargo.lock.
    # blaze writes it (`blaze lock`); commit it. docs/cargo.md, "Lock files".
    version = 1

    [blaze]
    release = "0.20261104.0"
    toolchain = "nightly-2026-10-27"

    [index]
    newest = "2026-11-04T10:12:00Z"

    [native.llvm]
    pin = "pins/llvm.json"

    [native.llvm.targets]
    aarch64-apple-darwin = "e95fc3b4…"

Tables and keys are sorted, so the bytes depend on nothing but the pins
(`deterministic-output`); equality is structural, so a hand-edited spacing is no change.
A lock that would pin nothing is not written.

What each entry means, and the choices made:

- **`[blaze]`** is recorded only for a workspace that pins a blaze version
  (`[workspace.metadata.blaze] version = "0.YYYYMMDD.N"`), and only by that release, without
  `--ignore-pin` for this workspace. An unpinned workspace, or one pinned to a channel, gets no
  `[blaze]` at all, so it is not refused after every blaze upgrade; its toolchain follows the
  running blaze. Any other blaze (a dev build of the tree, another release under
  `--ignore-pin`) keeps the entry that is there, or writes none; an entry that names a
  release other than the pin is the refusal case, an error naming both and the fix (run
  `blaze lock` with the pinned release). `is_version` is the one rule for a release's
  version, shared with the launcher's pin (`blaze_units::lock::is_version`).
- **`[index] newest`** is the newest sparse-index `pubtime` among the packages the resolve took
  from an index. The load is a pure function of its inputs and owns no clock
  (`sans-io-cores`), so it cannot know when the index was fetched; the newest publish time it
  used is a deterministic lower bound of that, and is what "index as of" says. A resolve that
  took nothing from the index (everything locked) keeps the old value.
- **`[native.*]`** covers every input the root manifest declares and every target its pin file
  names, not only what this build needs, so the lock does not depend on the host or the
  packages built.

## Built (this lane)

- `crates/blaze-units/src/lock.rs`: the policy (with `BLAZE_LOCK`, `env_writes`), `blaze.lock`'s
  model, render and parse, the move lines with their reasons (`moves`, `pin_moves`), `decide`,
  `identity` (which blaze to record), `is_version`, and `after_resolve`, the one function every
  resolve that may write lock files calls: the daemon's load and blaze-metadata's commands
  (`metadata`, `remove`, `vendor`, `tree`). Its two tests: the decision table (with the
  identity rows), and `blaze.lock` round-tripping byte for byte.
- `crates/blaze-units/src/load/resolve.rs`: calls `after_resolve` through the load's snapshot; a
  refusal is the load's error.
- `crates/blaze-resolve/src/lockfile.rs`: `lockfile_change`, the text without the policy;
  `lockfile_to_write` and its `Locked` are gone, banned in `.clippy.toml`.
- `crates/blaze-metadata/src/lib.rs`: `Asked::lock` carries the same inputs as a build;
  `relock` and `vendored` lost their own `locked` flag.
- `crates/blaze-engine/src/pin.rs` and the update commands: the launcher's version rule is
  `blaze_units::lock::is_version`; the engine's copy is gone.
- `crates/blaze-units/src/load/native.rs`: `locked_pins`; reading a pin now checks every
  target's name and digest, not only the host's.
- `crates/blaze-units/src/load/invocation.rs`: `--no-locked`, `CI`/`TF_BUILD` (read apart from
  the variables cargo reads, since cargo's `CI` changes `incremental`), `BLAZE_LOCK`, the
  running release (`BuildConfig::blaze`).
- `crates/blaze-api`: `Request::Lock` and `Work::Lock` (schema 39); `crates/blaze-cargo`'s job
  writes the lock files and ends the job after the load, so `blaze lock` resolves, writes and
  stops, checking nothing.
- Harnesses: `BLAZE_LOCK=write` where children start (`harness/ci.py` `run_one`,
  `harness/smoke.py`, `harness/blaze_cli.py`, `harness/live_e2e.py`, `harness/live_latency.py`,
  `tools/pgo/pgo.py`, `.github/scripts/publish_crates.py`, the install and promote workflows,
  `crates/blaze-engine/tests/repro.rs`); builds of committed workspaces keep `--locked`.
- Root `Cargo.toml`: `lock = "required"`; `blaze.lock` with the LLVM pin, in the format `render`
  writes. `blaze lock && git diff --exit-code Cargo.lock blaze.lock` in the tree checks it.

## Left

- **The launcher does not honour `blaze.lock`** beyond a version pin, which it already runs. A
  channel-pinned or unpinned workspace records no release and runs whatever it gets; recording
  and running one for a channel is a launcher change, not a lock one.
- **Index fetch time.** "index as of" is the newest publish time used, not when the index was
  read. Recording the fetch time needs the sources layer to hand the load a timestamp as an
  input, keyed, which it does not.
- **blaze's tree at the next pin bump.** `blaze.lock` holds no `[blaze]` yet: the pinned
  0.20261030.1 predates this policy and dev builds record none. The first pin bump must run
  `blaze lock` with the new release and commit `blaze.lock`, or that release refuses the tree.

## A lock earns its place

State shared between workers is PUBLISHED, GATHERED or CLAIMED: the three shapes, and why, are
end-state.md D179, not restated here. bstd builds them as `bstd::sync::Published`,
`bstd::gathered::Gathered` and blaze-core's claim table. A `Lock` or `ReadLock` is the named
exception, and it earns its place three ways: it says at its construction which shape it could
not be and why, the gate counts it, and its contention is recorded so a lock that should be a
shape is found by its waits.

### The record

Every contended acquire, with or without a profile, ends in `blaze_meter::lock_waited` (one
call, `#[cold]`, from blaze-core `lock.rs` `Waiting::charge`): the lock's name, how it waited
(behind a holder, a reader behind a writer, a writer behind readers), its wait, and the holder
it waited behind: the holder's task (`blaze_meter::doing`, which bstd's pool sets at each task's
start and end) and its hold, measured only for a hold that made someone wait (from the first
waiter's arrival, or from its acquire when it had waited itself, then "whole"). It goes into the
thread's lock table (`blaze_meter::Locks`, 16 rows and an "other" row, nothing lost), which
`Meter::now` and so every task's `Used` reads; the same number adds `LockWaits` and
`LockWaitNs`, so the two always reconcile (`Used::locks_reconcile`). A lock wait blocks its
thread (a fiber never parks inside one, rule 27), so the thread is the worker the task is on
for the whole wait: the thread's table is the worker's, with no parameter on any acquire.

The uncontended path is unchanged: a take is the raw lock's compare-and-swap and the owner
stamp, a release a store and a load of `waiting`. All of the record is behind `lock_slow`,
`read_slow`, `write_slow` (`#[cold] #[inline(never)]`, entered only after the CAS failed),
`try_lock_for` after its failed try, `enqueue`, and `Meta::released`, entered only when
`waiting` is not zero. So acquires are not counted, and a lock never contended is not seen by
the record; counting acquires would be an add on every take, and constructions cannot be
counted at run time (`Lock::new` is `const`). The ratchet below counts locks instead.

### The `locks:` section

`blaze_meter::LocksReport { used, worker_ns }` over a command's summed `Used`:

```text
locks: 3 contended, 14 waits 41.2 ms = 0.4% of 9.8 s worker time
  store.plain.writer       9 waits 30.1 ms, longest 20.0 ms; most by `merge` 25.0 ms (hold up to 18.0 ms)
  sched.domain.core        4 waits 10.0 ms, longest 6.1 ms; 3 writers behind readers; most by `parse` 9.0 ms (hold at least 4.0 ms)
  other                    1 waits 1.1 ms
  never contended: not counted (counting acquires would cost the uncontended path)
  UNACCOUNTED: ...         (only when the table and the meter's lock waits differ)
```

The ten locks by total wait, each with its count, total, longest, the holder that caused the
most wait and that holder's longest hold; the total against worker time; and the
reconciliation. The `used:` line under each timed row also names its worst lock and holder.

### The exception, as a fact the tools check

The attribute, on the item, statement, struct-literal field, `static` or `const` holding the
construction:

```rust
#[blaze::lock_exception(tried = "gathered", why = "one writer at a time appends to one open file; per-worker segments need the store's merge (D159)")]
writer: Lock::new("store.plain.writer", driven),
```

`tried` is `"published"`, `"gathered"` or `"claimed"` (the shape it was tried as); `why` is
the reason that shape failed, a sentence, never empty.

The lint, for the blaze-lint writer (rule `lock-without-reason`): in shipped code (not under
`#[cfg(test)]`, not in `tests/`, `benches/` or `examples/`), a call of `blaze_core::lock::Lock::new`,
`Lock::unrecorded`, `Lock::probed`, `ReadLock::new`, `ReadLock::unrecorded`, bstd's
`sync::Lock::new`, `RwLock::new` or `RankedLock::new`, or a `SharedMap::new_in` (its shard
locks), whose nearest enclosing statement, field, `static`, `const` or item carries no
`#[blaze::lock_exception(..)]` with a known `tried` and a non-empty `why`, is a finding:
"`<name>`: a lock with no reason: say which D179 shape it could not be". It names the lock by
its first argument when that is a string literal. A constructor that forwards a caller's name
(bstd's `Lock::new`, `Once`, `Flight`, `Chan`, `Swap`, `RankedLock`) carries the attribute
once, at its own body, and its callers then need none for that type.

The ratchet, for the gate tool's writer: a column `locks_constructed` per crate, the count of
those calls in shipped code by the same rule as the lint, and `locks_without_reason` beside it.
`locks_constructed` may only fall; `locks_without_reason` falls to 0 and is then held there.

### Every lock in shipped code, and what it becomes

From the tree on 2026-11-20 (a grep for the constructors above outside tests; a name with
`test` in it, or under `#[cfg(test)]`, is left out). "Lock" means it stays, with the reason
given.

| Lock | Guards | Becomes |
|---|---|---|
| `bstd.io.world` (blaze-std io.rs, `Reach::Shared` and `Escape`) | the world: effects queue, completions, abandoned tags, rng, trace, changed map, record | GATHERED: a submission ring and a completion queue per worker cell (D171); the lock stays only around the driver's own state, taken by the one thread that drives it |
| `bstd.task.inject`, `bstd.task.inject.background`, `bstd.task.pinned` | the pool's injected and pinned work | not a lock: crossbeam's `Injector` (rule 12) for injection, the worker's own deque for pinned work (GATHERED) |
| `bstd.task.failed`, `bstd.task.broken`; `graph.exec.first_panic`, `graph.pool.stray_panic`, `graph.pool.scope_panic`, `graph.par.split_panic`, `graph.par.giving_way_panic` | the first failure of a scope | PUBLISHED: the first failure wins, the joiner reads it (`PublishOnce`, set once) |
| `graph.par.first_found` | the first result of a search | PUBLISHED |
| bstd `Once`'s, `Published`'s park (a `Lock<()>` beside a `Signal`) | nothing: a waiter's look and the wake | Lock (tried CLAIMED: it is the parking half of a claim; a lock over nothing for the condition variable, held only to look and wake) |
| bstd `Flight` table, `Chan` queue, `Swap` retired list, `RankedLock` | a flight's attempts by key; a bounded channel; values waiting for reclamation; a ranked value | `Flight`: CLAIMED (the claim table). `Chan`: a lock-free bounded queue (crossbeam, rule 12). `Swap`'s retired list: GATHERED (a limbo per worker, merged at the grace period). `RankedLock`: Lock, its reason the caller's |
| `bstd.hosting.held`, `.gate`, `.fakes`, `.exited` | the hosted program's state, its consent gate, fakes, its exit | `exited`: PUBLISHED. `gate`, `held`, `fakes`: Lock (tried GATHERED: one host thread and the guest change one state in place; becomes a machine fed by events, D170) |
| `bstd.video.videotoolbox.outbox`, bstd `local` pipe | frames handed from the encoder's thread; a local pipe's bytes | GATHERED, a queue to one owner (an SPSC ring) |
| `core.threads.inbox`, `core.threads.errands`, `core.threads.spawn_life`, `threads.two.baton` | a thread's inbox; the errand pool's queue; a spawn's life stage; a two-thread handoff | inbox and errands: GATHERED, a queue to one owner (an MPSC ring); spawn life: PUBLISHED (each stage set once); baton: Lock (tried PUBLISHED: it is passed back and forth by design) |
| `core.descriptors` | the cache of open descriptors, an LRU | GATHERED (a cache per worker; a file opened twice is cheap next to a shared LRU) |
| `core.fsevents.writing`, `.heard`, `.flushes`, `.trace` | the watcher's buffers, written by the kernel's callback thread and read by one reader | GATHERED, a queue to one owner |
| `core.host.resolve_answer` | one answer handed from a resolver thread | PUBLISHED |
| `core.footprint.entries`, `core.cgroup.readying`, `core.process.children`, `core.reaped.labels` | the footprint sampler's table; making a cgroup ready; children to reap; labels of reaped children | footprint and reaped: GATHERED; cgroup readying: CLAIMED (made once by the first asker); children: Lock (tried GATHERED: the reaper and spawners share one set the kernel's waits key) |
| `wait.parker.thread` (unrecorded) | the parked thread's handle | PUBLISHED (set once, when the thread parks first) |
| `graph.pool.inject`, `graph.pool.scope_waiters`, `sched.domain.core`, `sched.fiber.driver`, `sched.ids.free`, `sched.ids.domain_slot`, `sched.flights.index`, `graph.flights.shard`, `graph.runs.bucket_latch`, `graph.runs.grow`, `graph.exec.spare_pools` | blaze-graph's scheduler: injection, scope waiters, a domain's core, the fiber driver, free ids, flight indexes, run buckets | injection: crossbeam's `Injector`; free ids: GATHERED (a free list per worker); flight indexes and shards: CLAIMED (the claim table); `bucket_latch`: PUBLISHED; `grow`: Lock (tried CLAIMED: growing a table is one writer while readers continue, kept until the table is the claim table); `sched.domain.core`, `sched.fiber.driver`, `scope_waiters`, `spare_pools`: Lock until blaze-graph is replaced by bstd's pool, which deletes them |
| `store.plain.writer`, `store.plain.lane` and the store's `SharedMap`s (`table`, `lanes`, `by_thread`) | the store's one writer, its lanes, its key table | PUBLISHED records, GATHERED segment writers a worker, one owner for the head's flip (D159); `table`: CLAIMED |
| engine `SharedMap`s (`engine.heads...`, `engine.dependency heads...`, `engine.member heads...`, `engine.frozen heads`), `ladder.goals` | heads read or made once a request, their failures; the solver's goals | CLAIMED, each value PUBLISHED |
| `engine.status.at_end`, `.phase_mem`, `.slot` (an array), `.park`, `.failed`; `engine.bodies.largest`; `engine.names.output`; `scripts.completed-worker`; `run.line` | the status line's tables and slots; the largest body; a name task's output; a finished script; the run's output line | status tables and `largest`: GATHERED (merged at the tick, D157); `failed`, `names.output`, `completed-worker`: PUBLISHED; `run.line`: Lock (tried GATHERED: one terminal written by whoever finishes, becomes a queue to the line's owner) |
| `scripts.jobserver`, `a crate's proc-macro results of this command` (blaze-proc-macro) | the jobserver's tokens and running count; a crate's pending results | jobserver: an atomic count and the pipe (not a lock); pending results: GATHERED |
| `intern.arena.insert`, `hashcons.shard.writer` | inserting into an interner; a hash-cons shard's writer | CLAIMED (a value interned once, by whoever meets it first) |
| `memo.ids`, `memo.alone.held`, `memo.arena.drops` | memo ids; a single-flight's held set; deferred drops | ids: CLAIMED; held: CLAIMED; drops: GATHERED |
| `h3.tickets`, `http.mailbox.latch` | TLS session tickets; one reply handed over | tickets: Lock (tried GATHERED: a ticket taken by one connection must leave every worker's view; small and rare); mailbox: PUBLISHED |
| `world.checkout.files`, `scope` `tables`/`made` (`RwLock`) | a checkout's files; the resolver's tables | checkout: GATHERED; resolver tables: CLAIMED, each table PUBLISHED |
| `stdx.assert.sites`, `profile.hot.marks`, `profile.history.drained` | assertion sites seen; hot-path marks; drained history | GATHERED |
| blaze-profile's unrecorded locks (`profile.flight.*`, `profile.postmortem.*`, `profile.sampler.*`), ix-prof's (`ixprof.*`, `ix_prof.layer.extra`) | the recorder's rings, owners, samples, postmortem state; ix-prof's session, symbols, maps | rings, samples, owners: GATHERED (a ring per worker, D179's sampler); `*.failed`, `*.stopped`, `recovering`, `kept`, `recovered`, `reader`: PUBLISHED; symbol and image tables: CLAIMED; ix-prof's session and HTTP gate: Lock (tried PUBLISHED: started and stopped by the user while the sampler runs) |

### Corrections to D179's last paragraph, from reading the code

- **The `World` lock is two.** Besides `Reach::Shared`'s, a hosted run's escape world (io.rs
  `Escape`, made at its first effect off the hot path) is its own `bstd.io.world`; both go to
  the per-worker rings.
- **The engine's results are not yet only merged at a tick.** `engine_status` keeps its tables
  and an array of `engine.status.slot` locks, and `engine.bodies.largest` and
  `engine.names.output` are locks; they become GATHERED (and PUBLISHED) with D157's driver.
- **The meter has no lock.** Its counts are already per thread (blaze-meter's cells) and per
  worker (bstd's `Tally`), summed by the caller: it is GATHERED now; the lock record above is
  kept the same way.
- **The proc-macro host's state is a jobserver and a results table, not a condition variable.**
  The jobserver becomes an atomic count; the table is GATHERED.
- **Not named in D179, and the most locks:** blaze-graph's scheduler (pool injection, domains,
  free ids, flight indexes); it is deleted when the engine runs on bstd's pool, and until then
  injection and free ids move as the table says.
- **Interners are locks for insert today** (`intern.arena.insert`, `hashcons.shard.writer`):
  CLAIMED is their end, through the claim table.
