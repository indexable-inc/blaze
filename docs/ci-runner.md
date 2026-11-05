---
status: plan
summary: A CI runner on ix VMs: a warm blaze VM per repository and a fork per branch. Design, nothing built.
date: 2026-10-24
---
# A CI runner of our own: a warm blaze VM per repository, a fork per branch

2026-10-24. Status: design, nothing built. Written against blaze main `d4f6027e82` and ix
`4a647c63ef`. Each claim is marked **read** (in a source we read, or measured by us),
**reported** (a document or someone says so, not re-checked here) or **unverified** (our
inference or an estimate). What was fetched is in [upstream/ci-runner/](upstream/ci-runner/SOURCE.md).
The umbrella issue is [#264](https://github.com/indexable-inc/blaze/issues/264).

Andrew's requirements: our own runner, warm per repository, on our VM product, working with git;
as fast as possible ("I don't know if RE will be the best protocol; maybe we want to control
it"); tests sandboxed to remove nondeterminism; never fall back to cargo; a cache that works
across branches and worktrees. And, settling the concurrency question: "our VM DOES support CoW
... we would not load multiple branches into the main blaze, so people would be forced into our
fork VM".

## Short version

- **The VM is ix** (indexable's product, [ix.dev](https://ix.dev/docs/machines)): every machine
  is 100 vCPUs, 256 GiB of memory and 1 PiB of disk; a snapshot is the machine's memory, disk
  and devices; a snapshot restores as many times as you like, each restore a new machine that
  "shares the snapshot's bytes and writes its own changes beside them", running processes
  included (**read**, public docs). ix already runs CI jobs as forks of a warm VM
  (`crates/ci/fast`) and has seen a buck2 daemon's in-memory graph survive the fork (**reported**).
- **The design.** One persistent VM per repository (the *seed*) runs a blaze-engine daemon warm on
  `main`. A push forks the seed's latest snapshot; the fork checks out the pushed tree in place,
  so the daemon sees the difference as an edit, builds, runs the affected tests each in a
  sandbox, reports a GitHub check and is thrown away, or kept as a snapshot for that branch's
  next push. blaze stays one branch per daemon; many branches are many forks.
- **Fork, not edit, is the long pole.** A nushell edit is 258 to 277 ms in a live daemon
  (**read**); an ix fork is 1.7 s for a seed restore (**reported**) and 4 to 36 s for seeds with
  8 to 27 GiB of touched memory on the older fork path (**reported**). So forks are made ahead:
  a few running *spares* per repository take the fork off the push's path (`vm.exec` into one:
  17 to 42 ms, **reported**).
- **Protocol: none on the hot path.** Inside a VM the daemon, its store and the tests share a
  file system and memory; between the seed and its forks the transfer is ix's copy-on-write
  fork; across machines it is blaze's existing remote (sealed, content-addressed packs of
  thousands of functions each, signed manifests). REAPI's action cache has no batch lookup
  (**read**): 165,752 per-function lookups at a gRPC unary median of 112 us is 18.6 s serial,
  against about ten pack fetches for the same 64 MB (**unverified** arithmetic). REAPI stays an edge
  adapter for teams with a Bazel cache ([#226](https://github.com/indexable-inc/blaze/issues/226), P3); no `Execution`.
- **Sandbox: `blaze sandbox` per test executable** ([#257](https://github.com/indexable-inc/blaze/issues/257)):
  every test, by default; user, mount, network, IPC and PID namespaces, read-only mounts,
  overlays, seccomp-notify recording of every path each test touches, by test; about 8 ms per
  executable on Linux x86-64 (**measured**, [cargo.md](cargo.md#sandboxed-tests-an-experiment-opt-in)). The fork is the boundary around
  the whole job. A VM fork per test is 100 times dearer and capped by account quotas; the cheap
  copy-on-write fork per test is a process fork from a loaded test program (**unverified**,
  estimated 1 to 5 ms).
- **Expected:** a body-edit PR on nushell or ripgrep green in about 3 to 8 s from push, against
  13 to 30 min for nushell's recent GitHub Actions runs and 5 to 11 min for ripgrep's (**read** for
  Actions, **unverified** estimate for ours).
- **Slices**, ranked: R1 one seed and one job end to end on ripgrep; R2 test results in the
  store, keyed by content; R3 every test sandboxed and flakes named; R4 spares and branch
  snapshots; R5 packs flow from forks back to the store; R6 GC by live roots; R7 shards over
  forks; R8 no rustc in the seed; R9 the REAPI edge. Blockers per slice are below.

## What ix gives

### The public offering (ix.dev, fetched 2026-10-24)

From [docs/machines](https://ix.dev/docs/machines), [docs/snapshots](https://ix.dev/docs/snapshots),
[docs/sdk](https://ix.dev/docs/sdk), [docs/networking](https://ix.dev/docs/networking),
[docs/api](https://ix.dev/docs/api) and [docs/pricing](https://ix.dev/docs/pricing), all **read**:

- **Shape.** "Every ix machine is the same size, and the size is large: 100 vCPUs, 256 GiB of
  memory and 1 PiB of disk." Each has its own kernel. `--cpus` picks 2 to 64 to pack more on a
  host. A fresh machine starts with 8 GiB plugged and grows (`VM_LIMITS.freshMemoryGiB`,
  `packages/web/src/lib/vm-limits.ts`).
- **Snapshot.** "A snapshot is a machine's memory, disk and devices at one moment. Taking one
  pauses the machine briefly; it keeps running afterwards." The call returns once the snapshot
  can be restored.
- **Fork.** "Restore a snapshot as many times as you like. Each restore is a new machine that
  starts from the snapshot, and the original keeps running. The new machine shares the
  snapshot's bytes and writes its own changes beside them." "Its memory and running processes
  come with it, as well as its disk." `machine.fork()` is snapshot and restore in one call.
- **What a fork does not take.** "A restore does not honor `--env`, `--ipv4`, `--group` or
  `--cpus`. The new machine takes the original's shape." A systemd image gets a new machine id;
  "everything else on the disk, SSH host keys included, is the original's". A fork keeps its
  architecture and, if pinned, its CPU class.
- **API.** A binary RPC over WebTransport (`https://api.ix.dev/rpc`); the operations the runner
  needs are `vm.snapshot`, `vm.vcs.restore`, `vm.exec` (a live `Process` with streamed
  stdout/stderr), `vm.fs.*`, `vm.delete`, `vm.port_forward`; SDKs in Rust, TypeScript and
  Python. Machines can be `ephemeral` or carry a `ttl` the platform enforces.
- **Networking.** Private groups (`<name>.ix.internal`, tunnels that end inside the machines);
  outbound traffic is free. A fork is not in its original's group unless added (`addMember`).
- **Price.** CPU $0.0288 per vCPU-hour *of CPU time used*, memory $0.0108 per GiB-hour *in
  use*, disk $0.101 per GiB-month; "a snapshot bills as disk, for the bytes it does not share
  with your other machines"; "identical disk bytes are stored once across all your machines";
  "memory the host caches for the machine is free". Whether a fork's memory that is still shared
  with its snapshot counts as "in use" for each fork is not stated (**unverified**).
- **How many.** Plans allow 20, 50 or 100 machines at once (public pricing). Usage tiers in
  `billing/catalog.json` cap concurrent machines at 2 to 100 and creates at 6 to 600 a minute
  (**read**, internal).

### What ix's own documents add (internal, `indexable-inc/ix`)

- **How a fork shares memory.** A restore maps a per-host *donor* (one sealed memfd per
  snapshot) `MAP_PRIVATE` with userfaultfd, resumes after the hot set, and faults the rest in
  from S3 lazily (`docs/design/lazy-memory-restore.md`, **reported**). Forks on the donor's host
  share its pages copy-on-write; fork placement is "same-host first (memory shared
  copy-on-write), then same-AZ hosts with lazy memory from S3" (`docs/ci/fast-jobs.md`,
  **reported**). Snapshots are chained: a record stores only the pages dirtied since its parent,
  three hops at most, then folded (`docs/design/chained-vm-snapshots-2026-10-09.md`, **reported**).
  The disk fork is a metadata-only copy-on-write fork of a segvol volume
  (`docs/design/fork-open-local-2026-10-08.md`, **reported**).
- **How long.** Seeds of ix's CI runners: 1.1 s capture, 1.7 s restore (`docs/design/ci-dogfood-ix-vms.md`
  section 7, prod 2026-10-16). A fork at the worker about 250 ms; `vm.exec` into a running fork
  17 to 42 ms; an 8 GiB seed forks in about 4.3 s; a seed with 27 GiB plugged and touched took
  35.8 s (19.0 s cut, 16.8 s restore) on the older fork path; a second cut of a seed is
  incremental (2.3 s against 40.2 s) (`docs/ci/fast-jobs.md`). A live snapshot pauses a 100-vCPU
  VM for a median 334 ms today, target under 1 ms (`docs/design/live-snapshot-pause-under-1ms-2026-10-09.md`).
  The target for a restore is a p50 of about 500 ms (`docs/design/golden-restore-1s-2026-10-08.md`,
  an estimate there). All **reported**.
- **What bites.** A fresh fork restores memory lazily, so its first compile paid the page
  faults: 5.2 s against 3.3 s on the second run, fixed by a warm-up run in each spare. Promoting
  a job's VM to be the next seed made every fork one disk generation deeper until segvol refused
  them; seeds are now refreshed in place. Plugged memory never shrinks and a fork is charged the
  seed's whole plug at admission ("no node can admit a VM that commits 104286 MiB" for a 100 GiB
  seed). A guest disk read and wrote about 40 MB/s on that deploy, so the hot build state lived
  on tmpfs, inside the memory snapshot. arm64 guests have no `/dev/kvm`; x86 guests have nested
  KVM. All from `docs/ci/fast-jobs.md` and `ci-dogfood-ix-vms.md`, **reported**.
- **CI already built on it.** `crates/ci/fast`: "a job is a fork of its lineage's last green
  VM"; the push webhook reaches ix, a running spare fork of the seed is claimed, the whole job
  is one `vm.exec`, the verdict goes back as a check run, and green on the default branch
  refreshes the seed (**read**, module docs). `crates/ci/github-events` parses `push` and
  `pull_request` and marks a fork's pull request untrusted (**read**). Measured on prod: a
  one-line ripgrep change with `cargo build --release` in a warm fork was green 5.9 s after
  `git push`; GitHub Actions' floor for a no-op job on an already-connected runner is 7.4 s
  (**reported**, `fast-jobs.md`). ix's CF5 seed kept a buck2 daemon running, and "`buck2 status`
  in the fork answers from the seed's daemon id, and the test reuses its DICE" (**reported**).
- **A sandbox crate.** `crates/ix/sandbox` (namespaces, Landlock, cgroups, rlimits, seals)
  (**read**, file list). ix decided against remote execution for its own CI after measuring a
  192-thread box: width beyond one box buys about 5% (`docs/ci/remote-execution.md`, **reported**).

## What blaze gives, and what is per checkout

- **A warm daemon is where the speed is.** nushell: ready in 52.1 s from nothing, a warm restart
  37.2 s that reused nothing, cargo 46.3 s beside it; in a live daemon a body edit 258 ms and a
  signature edit 277 ms ([research/lean-frontend.md](research/lean-frontend.md), **read**).
  ripgrep: a first build 14.5 s, an edit 0.09 s ([research/warm-start.md](research/warm-start.md),
  **read**). A fork carries the live daemon, so the runner never pays the start.
- **The store.** Keys hash the store format, the bytes of the blaze executable and the
  toolchain's commit (`Identity` and `program()` in `crates/blaze-store/src/lib.rs`, **read**).
  A seed therefore pins one blaze binary; a new binary is a new seed. A crate of the checkout
  is named by a hash of its source, so an edit renames the edited crate and its dependents
  ([cache.md](cache.md), **read**); agent c18d96d6 is making a warm start reuse the store and
  the keys content-only and independent of path and branch (**reported**). This design takes
  that as given.
- **Workers.** A worker is keyed by repository, toolchain, target, features and `cfg(test)`
  ([architecture.md](architecture.md), **read**). It can hold several checkouts (`--also`), but
  per Andrew the runner never uses that: a daemon holds one branch, and branches are forks.
- **The first build.** The daemon builds the base itself (`lib.rs`, **read**), as
  `BLAZE_RUSTC_BASE=0` asked for before the wrapper went.
- **Per checkout today.** `tests-reach` and `tests-passed` live beside each test program in the
  target directory (`cargo/affected.rs`, **read**). Affected selection runs everything when cargo
  built the program or `tests-reach` is missing ([#227](https://github.com/indexable-inc/blaze/issues/227),
  **read**). Integration tests are rerun when a binary's size or modification time changed
  (`affected.rs` lines 174 to 187, **read**). In a fork that is mostly fine (the fork inherits the
  seed's target directory and times), but none of it is shared between branches.
- **GC.** `StoreDir::collect` evicts sealed packs least recently used (by modification time)
  down to `BLAZE_CACHE_MAX`, by default a tenth of the free disk between 256 MB and 32 GB, after
  sealing, merging small packs and removing temporaries (`crates/blaze-store/src/lib.rs`,
  **read**). `blaze-engine cache gc` runs it; `blaze-engine cache gc --remote` takes old lines off remote
  manifests and removes unlisted objects (`crates/blaze-cargo/src/remote/collect.rs`, **read**).
  Neither knows what is live.

## The design

### The seed: one VM per repository, warm on main

The seed is an ix machine, persistent, of the repository's architecture. It holds:

- a partial clone of the repository (`--filter=blob:none`, unshallowed: ix measured a new commit
  fetched in 0.40 s that way against 5 to 8 s shallow, **reported**), checked out at one stable
  path;
- the pinned blaze binary and toolchain, and the target directory and store on tmpfs or the
  page cache, so they are inside the memory snapshot (the guest disk was 40 MB/s, above);
- one blaze-engine daemon for the checkout, started with `BLAZE_TESTS=affected`
  and the daemon's idle exit off (a fork resumes with a clock that jumped, and an idle timer must
  not end the daemon the job came for: **unverified** what the daemon does today);
- after a build of `main`'s tests: the daemon's own base, the test programs built, and the test
  results recorded.

Then it takes a snapshot. That snapshot, not the running seed, is what jobs fork. The seed never
runs code from a pull request.

**Advancing main.** On each push to `main` the seed fetches, checks out the new tree in place,
and asks the daemon for a build of the program and the test programs: main's new commits are an
edit like any other. It runs the affected tests (main's results are what branches reuse), then
takes a new snapshot. A chained snapshot stores only the pages dirtied since the last one and a
live snapshot pauses the guest briefly (334 ms median today at 100 vCPUs, **reported**), so a
refresh costs about the edit plus a pause. Pushes that arrive during a refresh are batched into
the next one, as ix's seeds do at most once a minute (**reported**). The seed follows main green
or red: its job is to hold main's compiled state, and a red main is reported on main's commit.

**Starting over.** The seed is rebuilt from nothing (and its spares and branch snapshots
dropped) when the blaze binary or the toolchain changes (the store's identity), when the
worker's key changes (target, features), when the daemon's memory passes a budget (workers grow
with edits, [#261](https://github.com/indexable-inc/blaze/issues/261)), and once a day as a
backstop. A rebuild boots a fresh VM beside the old seed and swaps when ready; jobs keep forking
the old snapshot meanwhile. Its cost is the cold start (52 s on nushell today), and a warm start
on the store gets cheaper with #41/#216 and c18d96d6's work.

### A job is an edit in a fork

1. **Hear about it.** GitHub's `push` and `pull_request` webhooks to the ix-runners App
   (`crates/ci/github-events`, about 1.0 s after the push, **reported**). Later, the runner as a
   git remote too (below).
2. **Pick the warm state.** Candidates: the seed's latest snapshot (main at `M`) and, for a
   branch pushed before, that branch's own last snapshot (at `B`). The tree to test is `T`: the
   pushed commit, or for a pull request GitHub's test merge with its base, which is what will land.
   The cost of a candidate `W` is what the daemon must redo for `git diff W T`: the workspace
   crates whose files changed, plus their dependents in the crate graph, with a manifest or lock
   file change counting as a reload of the crates it touches. The runner keeps the crate graph of
   `M` to compute this without a daemon. Usually: the branch's own snapshot for a follow-up push,
   the seed for a new branch. A branch snapshot built by another blaze binary, or older than the
   seed's last rebuild, is not a candidate. When a branch is far behind main, `M` usually wins,
   because the test merge `T` contains main.
3. **Fork.** Claim a running spare of `W` (a fork made ahead and warmed, below), or restore `W`
   if none is free. The fork is ephemeral with a TTL, gets a job-scoped GitHub token and no other
   credential.
4. **Check out and edit.** `git fetch` the commit, `git checkout --detach T` in place at the seed's
   path. Only changed files are written; the daemon finds them at the next build request
   (`tree.rs`) or, better, the runner hands it the changed paths from `git diff --name-status`.
5. **Build and test.** `blaze test` with `BLAZE_TESTS=affected`: only the edited
   functions are compiled and patched; each test program runs the tests whose code or recorded
   inputs changed, each in its sandbox; the rest are skipped because the same key passed before,
   on main or on this branch.
6. **Report.** One check run per job on the commit, with a line per failed test and a summary
   such as "built as an edit of main at abc1234: 3 crates changed, 41 tests run, 1,127 skipped
   (passed on the same code)". The plumbing is `crates/ci/runners/src/notices.rs`'s.
7. **End.** Ship any new packs (below). Then either delete the fork, or snapshot it as the
   branch's warm state and delete the machine: a stopped snapshot bills only as disk for the bytes
   it does not share with the seed's snapshot (public pricing), so keeping every live branch warm
   costs disk, not memory.

Why the fork and not a daemon per branch inside one VM: the daemon's state for a large workspace
is gigabytes (nushell 5.2 GB resident when ready, 7.5 GB after its own base, 11.2 GB at peak:
**read**), so a second daemon per branch costs a second cold start and a second copy; a fork costs
neither, starts with the loaded workspace and the salsa memos, and gives each job its own kernel.
blaze needs no change to its one-branch model for this, which is the point: the product does the
branching.

### Fork latency against the edit budget

| step | time | mark |
|---|---|---|
| webhook after `git push` | about 1.0 s | reported (ix) |
| claim a running spare, first `vm.exec` | 17 to 42 ms | reported (ix) |
| restore a snapshot when no spare is free | 1.7 s (seed restore), 4.3 s (8 GiB seed, older path), 35.8 s (27 GiB touched, older path) | reported (ix) |
| fetch one commit into a partial clone | 0.40 s | reported (ix) |
| a nushell body or signature edit in the live daemon | 258 to 277 ms | read |
| the same, first compile after a fresh restore without warm-up | about 1.6x (5.2 s against 3.3 s for ix's test) | reported (ix) |

So an unclaimed fork costs five to a hundred edits. Spares take it off the path: each repository
keeps `k` running forks of the seed's latest snapshot (and of the hottest branch snapshots), each
having run a warm-up build so its working set is faulted in. An idle spare costs memory and
almost no CPU (idle vCPUs halt; CPU bills by time used). `k` follows the push rate: one or two for
a quiet repository, more while a merge queue is busy. When the seed takes a new snapshot, spares
of the old one still serve (they are only one main commit behind, which the job's diff absorbs)
and are replaced as they are used.

### Where new compiled functions go

A fork's daemon writes what it compiled to its store, on its own copy-on-write disk, so it
vanishes with the fork unless shipped. The packs are sealed, named by the SHA-256 of their bytes
and checked on fetch, and the keys are content-only after c18d96d6's work, so a pack made on any
branch serves every branch with the same code. Shipping is the existing remote: at job end the
fork runs `blaze-engine cache push` to the repository's remote (an S3 prefix or a directory on a shared
volume), off the push's path; the seed pulls lazily (`BLAZE_REMOTE_LAZY`) at its next refresh,
and so do new forks.

Content addressing makes the bytes safe; it does not make the key-to-code mapping safe. A pack's
name proves the bytes were not changed, not that the code under a key is what that key's source
compiles to. A fork runs untrusted build scripts, proc macros and tests next to its daemon, and
code that escapes the sandbox could write a well-formed pack that maps a key to other code, which
every branch would then link. So only trusted lineages push: the seed always, forks of pull
requests from the repository itself (members), never forks of pull requests from forked
repositories (`trusted = false` in `github-events`). A manifest stays signed (`BLAZE_REMOTE_SIGN`)
by the runner, never by the fork. Later, if compiling one function is deterministic (**unverified**;
whole-program layout is not, [#200](https://github.com/indexable-inc/blaze/issues/200)), the
seed can check a sample of pushed entries by compiling them again.

### Memory with many forks

- Pages a fork never writes stay the donor's, shared on that host. What a job dirties is the
  edited crates' memos and code, the test programs' patched parts and what the tests allocate.
  We have not measured it; we expect hundreds of megabytes for a body edit and a few gigabytes for
  a signature edit in a core crate, against a 7.5 GB nushell daemon (**unverified**). R1 measures
  it (`/proc/meminfo` and the fork's dirty pages before and after).
- Admission charges each fork the seed's whole plugged memory today and plugged memory never
  shrinks (**reported**). So the seed is kept small: its plug is sized to the daemon's peak plus
  the store and target directory in memory, grown before the snapshot and not beyond; the
  spare count per host is bounded by host memory over plug; and branch warm states are snapshots
  (disk), not running machines. If ix starts charging forks for their own dirty pages rather than
  the plug, `k` can grow.
- Forks placed on the donor's host share memory; forks elsewhere fault from S3 and are slower
  for their first build (above). The runner asks for same-host placement for spares.
- Account quotas bound the fan-out: 20 to 100 machines at once and 120 to 600 creates a minute on
  the larger tiers (**read**). A repository's spares, jobs and shards count against them.

### Git

- **Webhooks first.** ix's App ingress already receives `push` and `pull_request`, records a run
  and posts check runs (`crates/ci/fast`, `crates/ci/runners`): blaze's runner is a job script
  and a seed recipe on that, not a second ingress.
- **The runner as a git remote, second.** For agents (den) that push many times a minute, the
  webhook's second is the largest fixed cost left. A `git push ix-ci HEAD` to a bare repository
  the runner serves (git over SSH into the control plane, which forwards the pack into the fork)
  starts the job when the pack arrives and pushes on to GitHub behind it. Unverified which ix
  service would serve it.
- **Worktrees.** In a fork the job checks out in place at the seed's path, so nothing in the
  daemon's state names a second path. A developer's own worktrees on their machine share the
  store as today; across machines, the remote and content-only keys share it.
- **Status.** Check runs per job; a test's failure as an annotation at its source line; a link to
  the job's full log (`machine.logs`).

### Fan-out, later

When a job's tests take longer than a few seconds, it forks itself after the build: shard `i`
runs a slice of the test programs in fork `i`, which starts with the built programs in memory, and
the dispatcher merges the verdicts. ix measured shards of 40 to 100 ms but a wave of 9.3 to 9.9 s
dominated by restores on the older fork path (**reported**), so sharding pays only for suites
measured in tens of seconds, or once spares of the job itself are cheap.

## Protocol: why none on the hot path

The question was REAPI v2 (CAS, ActionCache, Execution over gRPC) or our own. Where data moves:

| boundary | what moves | transport |
|---|---|---|
| inside the fork: daemon, store, test runner | functions, programs, verdicts | memory and the file system; the store's packs are local files |
| seed to fork | everything: the daemon's heap, the store, the target directory, the checkout | ix's copy-on-write fork; nothing is copied |
| fork to store (trusted lineages) | new packs, at job end | `blaze-engine cache push` to the repository's remote |
| runner to fork | the job, its output | ix RPC: `vm.exec` with streamed output |
| machine to machine, cold | packs and signed manifests | blaze's remote: S3, a directory, https |

The numbers that decide it:

- **Per-request cost.** gRPC's own benchmark run for Rust: a unary ping-pong median of 112 us, a
  streaming ping-pong 67 to 70 us (nrc/grpc-benchmark, **reported**, file in upstream/); grpc-io
  reports C++ unary about 200 us and Go about 300 us on GCE (**reported**). A tonic server does
  about 47k unary requests a second per core (**reported**), about 21 us of server CPU each. An
  in-process read of a store entry is a hash probe and a copy of a few hundred bytes in a mapped
  pack, about a microsecond plus a page fault when cold (**unverified**).
- **Many tiny values.** nushell has 165,752 functions; its store after zstd is 64 MB
  ([research/disk.md](research/disk.md), **read**), about 400 bytes a function (**unverified**
  arithmetic). REAPI's ActionCache has only unary `GetActionResult` and `UpdateActionResult`
  (**read**, `remote_execution.proto`): one lookup per function is 165,752 calls, 18.6 s at
  112 us each serially, or about 3.5 s of server CPU however wide (**unverified** arithmetic).
  The CAS batches (`BatchReadBlobs`, `FindMissingBlobs`), but under the server's size limit
  (NativeLink's default message is 4 MiB, **reported** in #226), and still needs the AC to map a
  key to a blob. blaze's packs already batch: a pack is sealed at 4 MB or more (`SEAL_AT`) and
  small ones are merged (`merge_small`, **read**), so one pack holds thousands of functions and
  the same 64 MB is on the order of ten objects (**unverified** arithmetic).
- **What a build actually waits on.** helix's warm start read 118,698 entries with "store read"
  at 129 to 131 s of thread time over 64 threads (#216, [research/warm-start.md](research/warm-start.md),
  **read**): the cost is turning records back into the daemon's values (interning types), not
  moving bytes. A protocol adds its round trips on top of that and removes none of it.
- **The edit budget.** An edit is 258 ms. A warm fork's edit reads nothing from any store: the
  memos are in the daemon's heap. There is no request to make faster.
- **Remote execution.** A function compiles in milliseconds, so an `Execute` per function loses to
  compiling it (#226 says the same). An `Execute` per test needs the action, the command and every
  input `Directory` in the CAS first (**read**, proto) and a worker that materializes the test
  program; the sandbox runs the test executable against the program already in the page cache
  in about 8 ms.
  ix measured that more width than one big box buys about 5% for its own CI (**reported**).

So: no RPC inside the fork, ix's fork between the seed and its forks, and the existing pack remote
between machines. "Our own protocol" here means blaze's pack and manifest format, which is
already ours; the one new message is a job's report (verdicts per test key), which travels in the
job's output over `vm.exec`. REAPI is worth one thing: letting a team that already runs
NativeLink or BuildBuddy hold blaze's packs, as an `BLAZE_REMOTE=grpc://` place mapping a pack
to a CAS blob and a manifest to an AC entry, exactly as #226 lays out. That is P3 and outside the
runner. A read-only REAPI view for Bazel users, and `Execution`, are not planned.

## Sandboxing

### Each test

The sandbox is [#257](https://github.com/indexable-inc/blaze/issues/257)'s `blaze sandbox`,
landing first in [PR #263](https://github.com/indexable-inc/blaze/pull/263) (**read**, issue
and PR): new user, mount, network, IPC and PID namespaces; every mount recursively read-only; the
workspace and target directory under overlayfs with a private upper layer; fresh `/tmp`,
`/var/tmp` and `/dev/shm`; loopback only; a seccomp user-notification filter on every
path-taking syscall whose supervisor, outside the sandbox, records each path and its digest when
the test ends. blaze writes the test harness ([#88](https://github.com/indexable-inc/blaze/issues/88)),
so the harness marks, on each thread, which test it is running, and the supervisor files each
path under that test. Every test runs sandboxed, by default, one sandbox per test executable
with libtest's threads inside: a launch costs about 8 ms on Linux x86-64 (9 ms recording reads),
and a 600-test library's test phase 37 ms sandboxed against 18 ms bare, where a sandbox per
test cost ten times as much (**measured**, `harness/sandboxbench.py`;
[cargo.md](cargo.md#sandboxed-tests-an-experiment-opt-in)). macOS has the same sandbox from Seatbelt
and an interposer.

Build scripts and proc macros go through the same `blaze sandbox`
([#147](https://github.com/indexable-inc/blaze/issues/147)), with what they read recorded into
their keys the same way.

### Can a test's sandbox be a copy-on-write fork?

| primitive | per test | what it gives | use |
|---|---|---|---|
| a thread in libtest's process | about 0 | nothing beyond Rust's own rules | not in CI |
| `blaze sandbox`: exec under namespaces and seccomp-notify, one per test executable | about 8 ms a launch, shared by its tests (measured) | a private file system view, no network, recorded inputs by test | every test, the default |
| process fork from a loaded test program, into new namespaces | about 1 to 5 ms (unverified) | the same, with the program's loaded memory shared copy-on-write | a follow-up to #257 |
| an ix VM fork per test | 250 ms at the worker to 1.7 s or more (reported) | its own kernel | tests that need root, KVM (x86 only) or kernel state; shards |

A VM fork per test is a hundred times the namespace sandbox and runs into the machine quotas, so
it is the exception, asked for per test. The cheap copy-on-write fork is one level down: the test
program, which blaze links and whose harness blaze writes, starts once, loads, and before
starting any thread forks a child per test with `clone3` into fresh namespaces (pre-made network
namespaces from a pool, since making one costs more than the rest, **unverified**). The child
shares the parent's loaded pages copy-on-write and skips exec and dynamic loading, which is most of
the 8 ms (**unverified**). The overlay's upper layer is the file system's copy-on-write of the
workspace. With one sandbox per test executable, this matters only for a test that must be
alone in its process.

The fork itself is the sandbox around the whole job: a pull request's code runs only in a
throwaway kernel with no credentials but a job-scoped token, and nothing it writes outlives the
job except the report and, for trusted lineages, packs.

### What a test result is keyed by

A test's verdict is kept under the hash of: the blaze identity and target; the test's reach hash
from `tests-reach` (its code and every function it reaches, by code identity); its attributes;
the fixed environment; the content (not the size and time) of each `CARGO_BIN_EXE_*` program it
runs; and the digests of every path its sandbox recorded, including paths found absent. A pass is
an ordinary entry of the store, beside the compiled functions and as content-keyed as they are,
so it travels in the same packs through the remote, and a test that passed on main is skipped on
every branch whose code and inputs for it are the same. A failure is never a reason to skip; a
failing test runs until it passes. Done in `cargo/results.rs`
([cargo.md](cargo.md#which-tests-run) says what of it v2 has): the key is found in two steps
(the paths a passing run read are kept under the rest of the key, the pass under those paths with
their contents), and `tests-reach` stays beside the program, written by the daemon at each build.

### Nondeterminism and flakes

The sandbox removes the file system, the network, other tests and the environment as sources of
difference. It does not remove the clock, threads or, by default, randomness (#257: a seeded
`getrandom` is opt-in, a virtual clock opt-in, a deterministic scheduler out of scope).

- **The runner's default.** It sets the test's seed to a hash of its key (#257's
  `BLAZE_TEST_SEED` per test), so a rerun of the same key sees the same randomness and the seed
  is printed on failure.
- **A flake is defined by the key.** Two different verdicts under one key are nondeterminism, by
  construction, since everything the sandbox can see was the same. The store keeps a mark beside
  a pass for a failure under the same key; a test with both is marked flaky, never skipped again,
  and the command warns with where it passed (done). `BLAZE_TESTS=verify` runs the tests kept as
  passed and names each that failed (done). Not done: rerunning a failure three times in fresh
  sandboxes before calling it, and a report on main's issue tracker.
- **Who it blocks.** A flaky test whose key the pull request did not change does not fail the pull
  request; it is main's bug, reported on main. A flaky test whose key the pull request changed
  fails it.
- **Looking for flakes.** Nightly, the seed reruns main's tests under varied seeds and thread
  counts in a fork and reports keys whose verdicts differ.

## Never falling back to cargo

The runner's rule: any job in which blaze prints `blaze: bug: <reason>; building with cargo` (the
line #258 added) fails, as a blaze bug with the reason and the issue, and never goes green through
cargo. It never sets `BLAZE_TESTS=all` or `BLAZE_SANDBOX=off` on its own. What is missing for
that rule to hold on real repositories, from [no-rustc.md](no-rustc.md) and the open issues
(**read**):

- [#258](https://github.com/indexable-inc/blaze/issues/258) (P1): test edits went to cargo on
  ripgrep and helix-core; three causes, fixes in flight.
- [#91](https://github.com/indexable-inc/blaze/issues/91) (no-rustc K): the cold build is blaze's
  own, the only path since `BLAZE_RUSTC_BASE` went with the wrapper.
- [#86](https://github.com/indexable-inc/blaze/issues/86), [#87](https://github.com/indexable-inc/blaze/issues/87),
  [#89](https://github.com/indexable-inc/blaze/issues/89), [#90](https://github.com/indexable-inc/blaze/issues/90)
  (F, G, I, J): std, every crate, build scripts and proc macros by blaze. Until they land the seed's
  warm-up still uses rustc's std rlibs, proc macros and parts of build scripts. That is counted
  rustc use in the seed, not a fallback in a job, and it is the reason the seed is built once and
  forked many times.
- [#92](https://github.com/indexable-inc/blaze/issues/92) (L): doctests; [#93](https://github.com/indexable-inc/blaze/issues/93) (M):
  the wrapper and the fallback removed.
- Edit refusals that end in cargo today: [#262](https://github.com/indexable-inc/blaze/issues/262)
  (cdylib-only packages), [#260](https://github.com/indexable-inc/blaze/issues/260),
  [#233](https://github.com/indexable-inc/blaze/issues/233), [#224](https://github.com/indexable-inc/blaze/issues/224),
  [#242](https://github.com/indexable-inc/blaze/issues/242).
- In the affected path: a program cargo built, or a missing `tests-reach`, runs everything (#227).
  In the runner that becomes a failed job naming why.

Each red job of this kind is a count the runner keeps per repository, and those counts only go down.

## GC

What is kept: everything reachable from a live root, then the rest by last use within a budget.

- **Roots.** The seed's current snapshot; each live branch's warm snapshot (a branch is live while
  it has an open pull request or was pushed in the last 14 days); each running spare. Each root
  writes a root manifest: the packs its daemon's build used and the test-result keys of its last
  run. A root manifest is the same kind of object as the remote's `manifests/<project>`, so the
  remote's collector can read it.
- **The local store** inside a seed or fork needs no root logic: the fork is thrown away, and the
  seed's store is bounded by `BLAZE_CACHE_MAX` as today.
- **The repository's remote.** `blaze-engine cache gc --remote` keeps every pack named by a live root
  manifest and applies its age rules only to the rest (a change to `remote/collect.rs`, which today
  ages manifests' lines regardless of roots). Results rows the same.
- **ix's side.** A branch snapshot is deleted when its branch stops being live; a deleted fork frees
  its disk at once; snapshots bill as disk for unshared bytes, so the budget is a disk budget per
  repository.
- **The daemon's memory.** Workers grow with edits (#261); the seed's restart onto its own store is
  the GC for that, and it is cheap only once #41/#216 land.

## Expected latency

nushell and ripgrep, a pull request with a body edit in one or two files of one crate, and one
with a signature edit in a crate many depend on (nushell's `nu-protocol`). All of our column is
**unverified** estimate built from the measured parts above; GitHub's column is **read** from the
projects' own recent runs (2026-10-23/24).

| | ripgrep, body edit | nushell, body edit | nushell, signature edit in `nu-protocol` |
|---|---|---|---|
| webhook, claim a spare, fetch, checkout | 1.5 s | 1.5 s | 1.5 s |
| build the program and the test programs as an edit | 0.2 to 0.5 s | 0.5 to 2 s | 1 to 10 s (#165: 0.5 to 17 s after a signature edit with an error) |
| affected tests, sandboxed, skipped by key elsewhere | 0.5 to 2 s | 1 to 4 s | 3 to 20 s |
| report | 0.5 s | 0.5 s | 0.5 s |
| **push to green, warm spare** | **about 3 to 4 s** | **about 4 to 8 s** | **about 6 to 32 s** |
| no spare free: add a restore | +1.7 s or more | +1.7 to 36 s | +1.7 to 36 s |
| GitHub Actions, whole pull request run | 5.3 to 11.0 min wall (10 runs; the stable Linux test job 3.5 min in run 39981832373) | 13.4 to 29.7 min wall (3 runs; a fourth took 53 min; the Linux job 4.9 min in run 40045770215, with Swatinem/rust-cache) | same |

A lock file change reloads the changed crates and builds them from the store or from source: tens
of seconds on nushell (**unverified**). A new blaze binary is a new seed: one cold start (52 s on
nushell) per repository, made beside the old seed. Actions runs include their OS matrix; ours is
Linux only until the runner has macOS and Windows forks, which ix does not have.

## Build plan

Ranked by what each unblocks. Blockers named; ix rollout state is ix's.

1. **R1. One seed, one job, end to end, on ripgrep then nushell.** A seed recipe (clone, pinned
   blaze, daemon warm on main, idle exit off, store and target in memory,
   snapshot), and a `crates/ci/fast` job script: fork, fetch, check out in place, `blaze
   test` under `affected`, check run, delete. Fails the job on any `building with cargo` line.
   Measures: push to green, fork time, the fork's dirty memory, whether the daemon survives the
   fork (clock jump, sockets, idle timer). Blockers: #258; ix restores reliable for a seed of the
   daemon's size (the guest-binary reload failures in `fast-jobs.md`); c18d96d6's store work for
   anything beyond one seed.
2. **R2. Test results in the store, by content.** `tests-passed` and `tests-reach` move into a
   results table keyed as above; `CARGO_BIN_EXE_*` by content. A test that passed on main is
   skipped on a branch. Blockers: c18d96d6 (content-only keys); #227's item 1 (a reach hash per
   function in the store, so a fork never compiles to hash).
3. **R3. Every test sandboxed, flakes named.** #257 phase 1 (PR #263), then phase 2 (every test),
   the per-key seed, the rerun-three-times rule and the check's flake line. Blocker: #263.
4. **R4. Spares and branch snapshots.** `k` warmed spares per repository on the donor's host,
   branch snapshots at job end, the warm-state choice by crate-level diff cost. Blockers: ix
   admission charging forks the seed's plug; same-host placement for spares.
5. **R5. Packs back to the store.** `blaze-engine cache push` at job end for trusted lineages, signed
   manifests by the runner, lazy pulls in the seed and new forks. Blocker: c18d96d6.
6. **R6. GC by live roots.** Root manifests from the seed, branch snapshots and spares;
   `cache gc --remote` keeps what they name; branch snapshots deleted with their branches.
7. **R7. Shards over forks** for suites that take tens of seconds. Blocker: cheap job-level forks.
8. **R8. No rustc in the seed.** #147 for build scripts and proc macros in the sandbox, then
   no-rustc F, G, I, J, K and M. Long; the runner's counts of rustc use in seeds track it.
9. **R9. The REAPI edge** ([#226](https://github.com/indexable-inc/blaze/issues/226), P3): packs
   and manifests on a team's NativeLink or BuildBuddy. Independent of the runner.

Also worth doing after R4: the runner as a git remote for agents, and the per-test process fork
of the sandbox section, each measured against the step it replaces.

## Open questions

- Does ix bill a fork's memory that is still shared with its snapshot? The public page says
  memory "in use" bills and host-cached memory is free; it does not say (**unverified**).
- How large is a nushell job's dirty memory? R1 measures it; the spare count follows from it.
- macOS and Windows: ix has neither, so the runner is Linux-only (x86-64 and arm64) until it does.
