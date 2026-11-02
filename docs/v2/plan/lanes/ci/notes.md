---
status: plan
summary: Lane ci's design notes for blaze v2: how the agent API and the VM-as-CI service are built, and what they ask of other lanes.
date: 2026-10-29
---
# blaze v2: lane ci's notes

Notes on [ci.md](../ci.md) as built, 2026-10-29. Requests of other lanes are in
`v2/requests/ci.md`.

## The agent API (`crates/blaze-api`)

- **One schema** (`SCHEMA` 5 when written, 14 since requests-api's change of 2026-10-29: lane cargo's cargo-shaped targets, artifacts, units and
  diagnostics, and lane proof's `Work`, `CodegenInputs`, `Trace` and `Tables`), deterministic CBOR from
  `blaze_protocol::cbor`, one frame layout for the Unix socket and for HTTP bodies. JSON is a
  rendering (`json`), never the wire.
- **Layers, each sans-IO but the client**: `frames` (bytes to items, schema refused by name
  before the body is read), `http` (HTTP/1.1 both ways, strict limits), `server` (listens and
  talks through `blaze_core::Effect::{Listen, Recv, Send, Hangup}`, hands the session
  `Arrived`s and takes `Reply`s), `session` (what each request does), `client` (blocking, for
  the CLI and blaze-ci).
- **The session does no building.** Anything past the VFS and the database is a `Work` for the
  daemon's `build::Builder`: check, build, test, run, what an edit reaches, the oracle's verify,
  a snapshot. The daemon (coordinator, at integration) implements `Builder` over the plane and
  routes its reports back through `Session::report` and `Session::finish`.
- **Views**: a request names a view by head, root or what-if; the session resolves it, opens
  it in the VFS and the database for the job's life, and closes it at the end, except the
  database's view of each root's last incremental build (latest 64 roots), which `Why` reads:
  a database view's reruns are its own. `Open` keeps a
  root open for the connection's life, which is how a client (blaze-ci) keeps its checkouts
  from collection. A head never written reads as the empty tree, which is how a client learns
  the base of a first commit.
- **Best-of-N** runs every candidate as its own what-if job at once; the plane's single flight
  shares everything the candidates have in common. Ranking is deterministic.
- **HTTP is for remote clients and ix forks**: a bearer token on every request, compared in
  constant time. The Unix socket is permissioned by its file (mode 0600, the driver's).

## The VM as CI (`crates/blaze-ci`)

- **A client of the API**, in a process of its own (`blaze-ci serve`): it never writes the
  store, never builds, never hands a secret to the daemon. Only the daemon writes the store.
- **A checkout is a view**: each ref's last `Checkout { commit, root }` is kept, and a new
  commit's root is a what-if of the nearest known root plus `git diff` of the two commits
  (a PR starts from its base branch's root), held open on one pin connection. The default
  branch is also committed to the head `ci/<branch>` so it survives a daemon restart.
- **Check runs** are posted by the service itself through the GitHub App (JWT RS256 with
  `ring`, TLS with `rustls`, roots from the system), so required checks and `land.py` keep
  working with no Actions runner.
- **Latest wins**, per branch and PR (a newer push cancels the older one's requests) and for
  releases (`Train`: one running, one waiting, a newer tag replaces the waiting one).
- **The production oracle** samples views by a hash of the commit (deterministic) and asks the
  daemon to `Verify` them; a mismatch fails the `blaze/oracle` check naming the fact.
- **Snapshots and a standby**: after a green default-branch or release run, the daemon makes
  its state durable (`Request::Snapshot`) and the service runs the configured snapshot
  command; a standby polls the primary's status and, after enough misses, restores the newest
  snapshot and promotes it. The commands are configuration (argv templates), not guessed ix
  calls.
- **Webhooks** are trusted only after `X-Hub-Signature-256` checks in constant time; fork PRs
  run like any other because the daemon sandboxes all user code.

## Open questions

- Two arenas name an open view (`Vfs::open` and `Database::open`); `build::Opened` carries
  both until db drops its own (the vfs arena is the one, `docs/v2/plan/reference/crates.md`).
- Files carry git's mode (`blaze_vfs::Mode`): a checkout writes executables and symlinks as
  such; a submodule is refused naming it.
