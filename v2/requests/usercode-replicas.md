# Requests from usercode-replicas

## usercode-replicas 1: cargo: accept `K` connections per binary on its test socket (`crates/blaze-cargo/src/serve/unit/test.rs`)

From `usercode-replicas` (child of `usercode`), 2026-10-29, for `usercode-fast-tests` 7. Bound
and arithmetic: `docs/v2/plan/lanes/usercode/speed-of-light.md`, "Tests", "Replicas of a fork
server".

What the queue now does (`crates/blaze-usercode/src/test/queue.rs`): for binary `b` it emits one
`Effect::Listen` on `<dir>/t{b}` as before, and takes every `Done::Accepted` on it as one
replica's connection, up to `K`. `K` is 1 until the first hello; that hello sets it
(`K = ceil(C × F / (F + T_median))`, clamped to `[1, min(C, tests)]`, with `C` = `Config::jobs`).
The queue sends `Frame::Replicate { count: K - 1 }` to the first connection, and emits
`Effect::Unlisten` once a hello came and `K` connections are in, or when the server ends. Each
connection says its own `Hello` (same tests, its own pid). The wire version is 3.

The request, exactly:
1. Pass every `Done::Accepted` for a test socket's listen tag to `Queue::done`, however many
   come, until the queue emits `Unlisten` for it. Don't close the listener after the first
   accept, and don't drop a second accept on the same path.
2. Keep each accepted connection open until the queue emits `Hangup` for it, and pass its reads,
   sends and closes to `Queue::done` as now. Up to `K ≤ Config::jobs` connections per binary
   are open at once.
3. `Config::jobs` stays the machine's cores (`available_parallelism`). It is `C` above.

Cargo's answer to request 7 (`crates/blaze-core/src/lib.rs:208`: the driver's listen completes
once per connection until `Unlisten`, and cargo passes the queue's effects straight through)
means all three already hold, so I expect no code change. Please confirm against the current
`serve/unit/test.rs`, or name what breaks.

> forwarded to cargo (requests, pass 20; told by tmux).

## usercode-replicas 2: codegen: the fork server's wire is version 3 (`crates/blaze-engine/src/test_harness.rs`)

From `usercode-replicas`, 2026-10-29. Informational, no change expected. The harness text still
comes from `crates/blaze-usercode/src/test/{wire,fork_server}.rs` and still ends in
`fork_server::serve(tests, socket)`. The wire is now version 3: `Hello` carries `fork: u64` (the
server's measured fork, in nanoseconds) and there is a new frame, `Replicate { count: u32 }`.
That supersedes "The wire version is 2" in `usercode-fast-tests` 3. If the harness pins or
checks the version anywhere outside that text, make it 3.

No sandbox change: answered by usercode under `usercode-fast-tests` 7.

> forwarded to codegen (codegen-next1, `crates/blaze-engine/`) (requests, pass 20; told by tmux).
