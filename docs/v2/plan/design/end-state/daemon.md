---
status: plan
summary: End state for the daemon, the client protocol, observability and proof: the daemon is a cache of evidence and pages; the certificate check is run by the client; each request is a sans-IO state machine; work starts only with a typed cause.
date: 2026-11-12
---
# End state: the daemon, the client protocol, observability, and proof

Tree read: `/Volumes/Projects/blaze-int-1112` (2026-11-12). Status words: **measured** (with where from), **read** (from code, this session), **memory** (a system cited from what I know of it, its source not opened this session), **design** (not run).

## 1. What this area must do, and the invariant

1. Answer a request (`build`, `run`, `check`, `test`, an agent's query) with what a build of the files as saved gives, in time proportional to what changed.
2. Keep evidence current (a watcher, pool pages, warm engine rows) so that the check and the redo are cheap; own the one writer of the store and the one memory budget.
3. Fail one request on a panic or an error, naming the item and the rule; never the process, never another request.
4. Record, for every build, what it did and the cause of each piece of work, so `why`, `slow` and `explain` are sums over rows.
5. Prove the above by simulation with oracles, by count bounds on real projects, and by a measurement table on a quiet machine, for a named commit.

**The invariant (derivability):** every answer the daemon gives equals the answer a fresh process gives from the store and the disk alone. The daemon holds nothing that is the only copy of a fact an answer rests on. It is a cache of evidence and of pages; killing it at any instant changes cost and never an answer.

## 2. How the best systems do it

- **git: racy-git and fsmonitor** (memory: `Documentation/technical/racy-git.txt`, `fsmonitor--daemon`). The index keeps each file's stat tuple; "unchanged" is a stat compare, and a file whose mtime is not older than the index's own write is hashed instead of trusted. A daemon is optional: when one runs it answers only "which paths changed since token T" and git stats those. **Take:** the check belongs to the client, the daemon supplies evidence for the same check, and the racy rule makes stat evidence sound. **Not:** nothing.
- **Relax** (read: `compiler/src/cache/collected.rs` `certificate`, `restore_collected`; `front/src/crate_image.rs:734-740`, the stat fields `size, mtime_ns, ctime_ns, ino` and its "racy" case). One certificate, one process, mapped arrays, 52.5 ms (measured, brief). **Take:** a no-op needs no daemon. **Not:** no watcher, so its check is O(files) for ever, and no shared scheduler or memory budget; at reth's size that is the wrong default.
- **watchman** (memory: clock tokens, `since` queries, cookie files as the sync barrier). **Take:** a persistent token per watch and a barrier before answering; on macOS FSEvents' event id survives a daemon restart, on Windows the USN. **Not:** its query language and triggers.
- **Buck2** (memory: `buckd`, DICE, the event log behind `buck2 log what-ran`). **Take:** every unit of work emits one typed event with its reason, and tools are readers of that log. **Not:** the graph that lives only in the daemon's memory: a restart is a cold graph (blaze: restart 191.6 s, daemon 56-75 GiB, measured, brief).
- **FoundationDB simulation and TigerBeetle's VOPR** (memory: Zhou et al., SIGMOD 2021; TigerBeetle's `docs/` on the VOPR and static allocation). **Take:** the real code, whole, under one seeded simulator with faults, run continuously, each failure a seed; buffers of fixed size allocated once. **Not:** hand-written fakes beside the real code.
- **bors, "the not rocket science rule"** (memory: Hoare). **Take for how the work is run:** the tree everyone builds on is only ever a tree that passed; a batch that fails is dropped, never fixed forward in the shared tree.

## 3. blaze today (read from code unless marked)

**Shape.** `Daemon::turn_untagged` (`serve/daemon.rs:2146`) is one loop: poll the driver, then `turn_after_wait` (`:2226`) runs completions, the plane's step, observe/watch/say, then `between` (`:3377`: collect, ask the image, settle the database on the image), `settle_turn`, `collect_turn`. Requests are `Jobs<S>` (`serve/job.rs:3547`), one struct of about fifty fields shared by every job (placements, runs, tickets, settling, recordings, roots, leases). `Jobs::begin` (`job.rs:4029`) asks `answer_again` (`:4369`) and otherwise makes a `Job` and plans and walks from the roots. `keep_answer` (`:4753`) keeps a `noop::LastAnswer` in memory. The thin `blaze` (`crates/blaze-thin`) answers a routed subset and execs the engine for the rest.

**Smells under rule 66.**
- *Fast path over a slow default; two mechanisms for one job.* `job/noop.rs` (1,193 lines, live) and `job/certificate.rs` (658 lines, `#![allow(dead_code)]` at `:29`, wired nowhere) both decide "is this request answered". The live one is the exception to a walk.
- *Special-case lists.* `noop::Unkept` (`noop.rs:327`: None, Failed, Made, Planned, Said, Looked, Outputs, Uncounted) and `NotReplayed` (`:413`, ten cases, among them `NotAlone` and `Disturbed`); `answer_again` refuses the daemon's first build (`job.rs:4380`). `blaze-thin/src/route.rs:3-9` reads only `build`/`run` with `-q`/`-r`; any other word execs the engine, and `handover.rs` passes a half-read answer to it: a second client for the first one's gaps.
- *A per-process key.* An answer is kept "only of a build that walked every unit and made nothing" (`noop.rs:5-6`), in memory, per daemon. Measured (brief): after a cold build the fast answer appears on the fifth run; 160-700 ms before.
- *I/O mixed with logic.* `job.rs:1377`, `:1539`, `:1546`, `:3039` call `std::fs` and `:3154` reads `SystemTime::now()` inside the "sans-IO" job core; `keep_answer` reads process-global tables (`blaze_vfs::outside::take_file_looks`, `:4758`). `#[blaze::nonblocking]` on `turn_after_wait` is commented out (`daemon.rs:2224`).
- *Polling.* The idle loop wakes every 50 ms and every 1 ms while tasks run (`daemon.rs:2155-2162`).
- *Unbounded or unowned state.* `Jobs` holds maps for the daemon's life (`fetch_failed`, `native_laid`, `settling`, `recordings`); the after-answer work is driven by a counter of quiet turns (`quiet_between_requests`, `daemon.rs:3364`), not by what it must not delay.
- *One concept, two homes.* Two wire schemas: `blaze_protocol::SCHEMA = 2` (`blaze-protocol/src/lib.rs:45`, `Message`) and `blaze_api::SCHEMA = 53` (`blaze-api/src/lib.rs:110`, `Envelope`), the second bumped for every field.
- *Why is reconstructed, not recorded.* `account.rs:5-8` says the account was added after three bugs were each found by hand; refusal reasons were dropped until `Jobs::refused` and a span were added (`job.rs:4095-4106`). `history.rs:781` makes an `ArrowWriter` per file per flush with zstd (`:720`); `:1335` wraps the flush in `catch_unwind` after `byte array offset overflow` panics.

**Numbers (measured 2026-11-12, from the brief and `end-state.md`).** A repeated hello-world `run`: 1,162 jobs, 1,195 admissions. 425 ms of loop CPU inside "loop turn: completions" with one completion costing 240 ms (the comment at `daemon.rs:2250` records it). The next request waited 1.7 s for the previous build's store commit. reth no-op 2.0-2.8 s. The profile's writer allocated 128 GiB cumulatively in one build. `blaze explain memory` empty for a daemon with no memory rows. Revision suite on a loaded machine: 7 of 40. Test targets of three crates did not compile on main.

## 4. The end state

### 4.1 What the daemon is for

Four things, none needed for a correct answer: (a) **evidence**: watches that keep each checkout's leaf table current, so a check costs O(what changed) instead of O(files); (b) **pages**: the store's buffer pool and the engine's sealed groups, so a miss starts warm; (c) **one writer and one budget**: the store has one committer and the machine has one footprint ceiling, so concurrent requests are admitted by one scheduler; (d) **work ahead**: a save is built before it is asked for. One daemon per home because a home is one store with one writer. Several homes on a machine share nothing, so admission reads the kernel's footprint and pressure, never its own forecast alone. A restart loses (a)-(d)'s warmth and nothing else: no image is decoded, no database is settled.

### 4.2 Records on disk

- **Certificate** (store record, content only, as `certificate.rs` has it, with outputs by content hash and no stamp): key `(shape, root id, blaze id, n)` in a bounded chain; reads are groups, variables, outside files, probes; value is outputs, unit record keys, `said` relocated. Written by **the build that did the work**, from the union of its units' recorded reads and the root id taken after its own writes are in the view. Not by a later walk that made nothing. A build with a step blaze could not trace writes none and says which unit.
- **Leaf table** (local, one per checkout, in the home; not content addressed): sorted `(path, content id, size, mtime_ns, ctime_ns, ino, mode)` for every file and directory the watch covers, the output stamps, the watch's persistent token, and the instant it was written. One writer (the daemon's watcher), replaced by rename. It is the persisted form of the view, and the only local evidence; `Stamp` leaves the certificate for it.
- **Slot** (local): checkout and shape to the last certificate key, so a miss names what moved.

### 4.3 One check, two sources of evidence

`blaze-certify`: a sans-IO crate that links no compiler. `holds(certificate, now, looks) -> Ok | Failed{input, readers}` as today. `Looks` is filled from the leaf table plus one of: **`Since(token)`** asked of a live daemon (it flushes the watch, then answers the set of paths changed since the token, or "token not covered"); or a **stat sweep** of the leaf table when no daemon holds the token. A stat is trusted only when the tuple is equal and both times are older than the table's write instant by more than the file system's timestamp grain; otherwise the file is hashed (git's racy rule). A negative probe is the stat of the nearest existing directory. A file system without nanosecond times or ctime gives no stat evidence, and the request says so.

**Should the client answer a no-op without the daemon? Yes.** The client is the one place the request check runs: it reads the slot, the certificate and the leaf table, takes evidence, hashes its own environment's named variables (they are its own, not shipped), stats the outputs, and on `Ok` prints `said` and for `run` becomes the program. This is sound because (1) a certificate exists only for a fully traced build; (2) every read has evidence that is either a watch continuous since the token with a flush barrier, or a non-racy equal stat, or a fresh hash; (3) the certificate is read only once durable, and the store orders a certificate after everything it names; (4) the tool identity is the client's own. It is not a fallback: both sources feed one function and give one answer; simulation holds them equal (I1). On `Failed` the client sends `Build` with the failure; the daemon runs the same `holds` itself as its first state (it serves agents and saves that have no client) and starts the walk at `readers`.

### 4.4 The request machine

`blaze-request`: a crate that depends on ids, the protocol and `blaze-certify`; one machine **per request**, no shared struct. `step(&mut self, now, Event) -> Effects`.

States: `Barrier` (watch flush asked) → `Certify` → `Answered` | `Place(outputs)` | `Walk{from}` → `Running{outstanding}` → `Said` (answer with the driver) → `Durable{ticket}` → `Certified` → gone. Ends: `Failed(error)`, `Cancelled`, `Panicked{state, event}`.
Events: `Flushed(token)`, `Evidence`, `RecordRead`, `UnitDone(Held | Made | Error | Panic)`, `Committed(ticket)`, `Cancel`, `Deadline`.
Effects: `Flush`, `ReadRecord`, `Stat`, `StartUnit{cause}`, `Put`, `Say`, `Commit`.

Rules of the machine: `Certify` issues no `StartUnit`, opens no plan and no admission. `Walk` visits `walk_from(failed)` and nothing else. `StartUnit` takes a `Cause` (4.6); there is no other way to start work. A request never waits on another request's `Durable`: reads see the committer's pending puts; durability orders only what a crash keeps. The driver calls each `step` under one catch: a panic drops that machine and says `Panicked` to its client; shared state is reached only through effects, so nothing is left half changed (rule 20). The driver (`serve/daemon.rs`, thin) owns sockets, the watcher, the store, the plane, and waits on a deadline with no periodic wake (rules 14, 18, 44).

**Settle** is not a phase of the loop. It is three tasks on the plane, each with a span: the store's group commit; the certificate's put after its ticket; the profile's seal. Collection and cold compression are scheduled tasks that a request preempts at a record boundary. The image and "settle the database on the image" go with level 3 of `end-state.md`.

### 4.5 The protocol and the client

One crate, one `Message{schema, id, body}` in deterministic CBOR over the Unix socket (rule 33). Requests: `Since`, `Build`, `Cancel`, `Follow`, `Explain`, `Status`, `Stop`. Answers are a stream of the same typed records the profile stores (one span stream), so a client's view of a build and the history's cannot differ. Fields are added by key and old readers skip unknown keys in events; `schema` changes only when a meaning changes. `blaze` is the whole client (arguments, the check, printing; no compiler); `blaze-engine` is only the daemon. A client whose version differs from the daemon's starts the engine beside it and the old daemon hands the socket over after its running requests end.

### 4.6 What every build records

- **One `requests` row always**, even on a hit: id, certificate key, verdict, and on a miss the `Failed` variant with the named input; the instants of each state. A hit writes this one fixed row and flushes nothing.
- **Cause by construction.** The function that decides whether a record holds returns `Held | Moved(read, was, now)`. `Cause` is `First{nearest key's differing part} | Moved(read) | Evicted(by) | Forced | Left(prior failure)`; there is no unexplained constructor, so "unexplained" is zero by type and the account is a sum. `why` reads rows; it reconstructs nothing.
- **Loop time.** The driver times every `step` and adds it to a folded counter keyed `(state, event kind)`; a step over 1 ms is also a span with its event. No loop CPU can be outside a name.
- **Waits** name what and who holds it (a ticket number, a lock's holder). **Reads outside a view** name the reader.
- **Memory** (rule 50): a build's row cannot be closed without footprint at start, peak and end and the top owners at peak; an empty `explain memory` is a gate failure, not an empty table.
- **Cost.** Rows are fixed width, written into per-thread rings and from there into append-only segments from buffers allocated once (one encode buffer, one compression context for the daemon's life), bounded by bytes. Parquet is made by the compactor when idle or when `blaze debug sql` asks. A full ring drops with a count in the account. Budget: under 1% of the build's CPU and a fixed footprint, held by the profile gate on reth.
- **Alarms** are the account's proportions (work against change) and are part of each real-project check's pass condition.

### 4.7 The proof pyramid

- **L0, every snapshot, the runner, minutes:** `cargo check --workspace --all-targets` and `blaze check` of the same. This is the gate that keeps test targets compiling; a snapshot that fails it is not a tree.
- **L1, with L0:** property tests of `blaze-certify` and `blaze-request` against a model build (files, units with read sets, outputs as a hash of reads), with planted bugs (`Plant::Ignores*` exists).
- **L2, with L0 and continuously on spare cores:** the request machine with the real store and VFS under `sim::Sim`, seeds drawing edits, torn saves, restarts, crashes, failed syncs, lost and late watch events, concurrent requests and injected panics. Oracles: **I1** answer equals a fresh process's answer on the same files, for both evidence sources; **I2** a hit starts 0 units, 0 admissions, 0 plans, and steps at most a constant; **I3** a durable certificate implies its closure is durable; **I4** the request after a build that passed is a hit; **I5** no request waits on another's commit (virtual time bound); **I6** units started are within `walk_from(failed)`; **I7** a panic fails exactly its request and others answer as without it; **I8** every account balances. I4 would have caught the fifth-run answer, I2 the 1,162 jobs, I5 the 1.7 s, I7 the history thread.
- **L3, per snapshot, real processes, counts only:** the revision suite's real driver for what only a kernel shows, bounded by jobs, admissions, units, rows run and verified, bytes written. Counts do not depend on load, so 7 of 40 on a busy machine cannot happen; `max_ms` leaves the gate.
- **L4, twice a day on reserved cores, a named commit:** `real_smoke.py`, then the edit-loop table on reth and tempo (no-op, comment, body edit, first edit, edit after restart, warm footprint), each row with its account's alarms at zero and the profile gate passing. Times live only here.

### 4.8 How the work is run

Critical of today: lanes write code they cannot compile into one directory; one runner finds every error serially; `ready_check.py` exists to detect that someone is still typing in a file; freezes cut snapshots; `certificate.rs` was written whole before the loop's design was settled and sits dead; eight lanes put hunks in one 9,494-line file; a day ended with no clean test table and no clean measurement table. The cause is one: nothing is small enough to check alone, so nothing is checked until everything is merged.

1. **A design gate before code.** A ledger row cannot be picked until it has: the invariant, the machine or record, the oracle that will check it, the number it moves with its baseline on a named commit, and a review by a reader outside the work. No baseline, no code.
2. **Interfaces by one mind, compiled.** The coordinator writes and compiles the types of a design (events, effects, records, traits) and lands them; builders fill bodies behind them in their own files.
3. **Builders compile their crate.** Cores are small crates without the compiler (`blaze-certify`, `blaze-request`), so `cargo check -p <crate> --all-targets` and its unit tests take seconds; a builder runs them through the VM's queue with a cap. Rule 35 ("lanes only write") is the measured cause of the lost day and should be narrowed to "lanes do not run the whole build or any measurement".
4. **A worktree per lane and a merge queue.** The integration tree advances only by batches that passed L0-L2; a failing batch is bisected and dropped back to its lane. No freeze, no ready lists.
5. **Reserved cores for measurement** (a cpuset on `blaze-vm`), so the L4 table is made on schedule whatever lanes are doing, and a missing table for the head stops new batches.

Rules met: 3 (one check, no second route, a miss names its input), 11 (machines and rows in fixed buffers; a request's frame), 14 (two sans-IO crates, thin driver), 21 (the certificate's reads are its key's inputs), 25 (records by content; `said` relocated), 43 (I1-I8), 50 (footprint rows mandatory), 51 (certificates and profile segments are records), 65 (certificate groups carry per-unit readers; nothing per crate or per process).

## 5. What is deleted, and what survives

**Deleted:** `serve/job/noop.rs` whole (`Answers`, `LastAnswer`, `Unkept`, `NotReplayed`, `Vouches`, `Recording`); `Jobs::answer_again`, `keep_answer`, `Jobs::refused`, `recordings`; `Jobs<S>` as a shared struct and `job.rs` as a file; `Daemon::between`, `quiet_turns`, `QUIET_BEFORE_BETWEEN`, `ask_image`, `let_go_of_image`, `serve/image.rs` (with level 3); the 1 ms and 50 ms wakes; `blaze-thin/src/route.rs`, `handover.rs` and the engine's CLI; `blaze_api::Envelope` and its `SCHEMA`; `certificate::Stamp` in the record; `account.rs`'s `ran_uncaused`/`by_unexplained` as measured columns; the per-flush `ArrowWriter` and the `catch_unwind` at `history.rs:1335`; `max_ms` in the gate; `ready_check.py` and the freeze.
**Survives:** `certificate.rs`'s record, `holds`, `walk_from` and plants (moved to `blaze-certify`); `serve/heads.rs`'s flush barrier; `settle.rs`'s `Policy` as the scheduler of idle tasks; `blaze-core`'s `sim::Sim`, drivers and deadline wait; the flight recorder's rings; `Ledger::did(cause)`; the DuckDB views; the revision cases and `spec.rs`; `real_smoke.py`, `ran.py`.

## 6. Order of building

Before step 1 (half a day): L0 in the runner, and the baseline tables on a named commit of the quiet machine.

| Step | What | Judged by (measured how) |
|---|---|---|
| 1 | `blaze-certify` and `blaze-request` with `Certify` as the first state, daemon side; the certificate written by the build that worked; `noop.rs` deleted; I1-I6 in simulation | hello-world repeated `run`: 0 jobs and 0 admissions (from 1,162 and 1,195), a hit on the second run (from the fifth), daemon side under 5 ms; reth no-op under 100 ms (from 2.0-2.8 s); wait on the previous commit 0 (from 1.7 s). `requests` rows and `blaze explain`, quiet VM |
| 2 | Every request a machine; `Jobs` and `job.rs` dissolved; step attribution; I7 | unnamed loop CPU 0 ms (from 425 ms); longest step under 1 ms at p99; one injected panic fails one request |
| 3 | The leaf table, `Since`, the stat sweep; the client runs the check; `route.rs`, `handover.rs`, `Envelope` deleted | hello-world no-op wall under 10 ms with a daemon and under 20 ms with none (Relax 52.5 ms); reth with nothing changed and no daemon: the sweep's time, expected under 200 ms (from 191.6 s restart) |
| 4 | Causes by construction; mandatory memory rows; fixed-buffer segments, Parquet at compaction | reth cold: writer's cumulative allocation under 1 GiB (from 128 GiB); profile under 1% CPU by an on/off pair on a reth body edit; `explain memory` non-empty; unexplained 0 on reth and tempo |
| 5 | Settle as plane tasks; idle wakes removed | an idle daemon's wakeups per second 0; a request arriving during collection waits under 1 ms |

## 7. Unsound, unknown, or to be measured first

1. What does the watch flush cost on macOS (FSEvents) and Linux? If it is over a few ms, a small checkout is checked faster by the sweep. Measure both on hello-world and reth before step 3 picks which evidence a client asks for first.
2. How many files does reth's certificate cover, and what does the sweep cost on the Mac and the VM? Count the groups' leaves; time the stats.
3. Does `Placement.key`'s store value equal the placed file's bytes for a patched image (`certificate.md`, "State", item 1)? Read `serve/work.rs`; if not, an output's evidence is its stamp in the leaf table only.
4. Between the check and `exec` of the output another build may replace it. Is open-then-fstat-then-spawn enough on macOS, which has no `fexecve`? Write the race as an L2 case first.
5. How many units of reth and tempo run a step blaze cannot trace (so no certificate)? One query over `misses`.
6. Is a hit from the committer's pending puts sound across a crash in every order? I3 with torn commits decides; until it passes, a certificate is read only once durable.
7. Can segments plus compaction to Parquet stay under 1%, or is reusing one `ArrowWriter` enough? Measure the simple reuse first; keep the simpler one that meets the budget.
8. Fanotify has no persistent token: after a daemon restart on Linux the sweep is the only evidence. Is that sweep acceptable at tempo's size, or is a kept journal needed?
9. A machine with several homes: should the ceiling be machine-wide (a lease file) or is kernel pressure enough? Run two daemons on tempo and reth together.

## 8. How to cut it

**One mind (the coordinator, compiled and landed first):** the types of `blaze-certify` (`Certificate`, `Looks`, `Failed`, the leaf table's layout) and of `blaze-request` (`State`, `Event`, `Effect`, `Cause`); the one protocol `Message` and its bodies; the oracle statements I1-I8; the `requests` row. About five files.

**Pieces, each whole in at most a few files, each with its crate's check:** (a) `blaze-certify`: codec, `holds`, the racy rule, properties; (b) the leaf table's writer in the watcher and its reader; (c) `blaze-request`: the transitions of `Barrier`/`Certify`/`Walk`; (d) the same crate: `Running`/`Said`/`Durable`/`Certified`; (e) the certificate's assembly from unit records' reads; (f) the driver in `serve/daemon.rs` mapping effects to the store, plane and sockets (needs `daemon.rs` by function, one builder); (g) the simulation harness and model build for I1-I8; (h) the client: arguments, the check, printing; (i) `Cause` threaded through `Ledger` and the engine's validity function; (j) the profile's segment writer and the compactor; (k) L0 and the merge queue in the runner. Deleting `noop.rs` and the `Jobs` fields is the coordinator's, in the same batch as (c)-(f), so the tree compiles at every snapshot (rule 24).
