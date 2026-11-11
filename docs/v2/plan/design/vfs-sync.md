---
status: plan
summary: blaze v2 vfs - the team's source of truth lives on Cloudflare (a Rust Worker built by blaze, run as wasm, a Durable Object per head); multiplayer edits and merges are operations on the syntax tree, not text; jj-style changes with stable IDs, first-class conflicts and an operation log; a branch can stack on another and takes its updates automatically; Pando-style per-head ordering for thousands of agents; blaze builds straight from a synced head; git is only a mirror.
date: 2026-11-04
---
# blaze v2 vfs: synced, multiplayer, no git

2026-11-04. A plan and to-do list. The vfs ([../../../../v2-vfs.md](vfs.md)) already
has content-addressed nodes, named heads, constant-time views and a file-by-file rebase. This plan
adds three things:

1. **Sync.** Every machine on a team (the Mac daemon, ix VMs, CI, a browser, an
   agent) holds a replica of the same heads, so it can build without cloning anything.
2. **Multiplayer.** Several people and agents edit one head live and see each other's edits within
   a round trip.
3. **History.** Commits, branches and merge in the vfs itself, so a team does not need git.
4. **Edits on the syntax tree.** Operations and merges act on the front end's syntax tree (move
   this item, rename this binding, insert this match arm), not on byte ranges. Two people editing
   different items in one file never conflict, and a rename merges with an edit that uses the
   name.

**Cloudflare is the source of truth.** The heads, commits and nodes live in the Worker's Durable
Objects and R2. Every machine is a replica. git is a one-way mirror the Worker keeps for tools and
people that still want it (GitHub, code search, a plain `git clone`).

## The model

- **Nodes are immutable and named by content** (BLAKE3 `ContentId`, the file and directory domains).
  Syncing them never conflicts: a peer either has a node or doesn't.
- **A commit is a node too**: `{root, parents[], change, author, time, message}` under a new
  domain `"blaze v2 vfs commit 1"`. History is a DAG of commits, as in git. The hash is BLAKE3, not
  SHA-1.
- **A change is the unit of work, not a commit** (jj). A change has a stable change ID that
  survives every rewrite. Its current commit changes on every edit, rebase or amend. Agents and
  reviewers name changes, so "the change that adds X" stays the same thing while it is rebased a
  hundred times. There is no staging area: the working copy is the change's current commit.
- **A branch is a head that names a change**: `branch/<name>`. A **live head**, `live/<name>`, names
  a root that changes on every keystroke batch, the way a workspace head does today. Recording a
  live head makes a new commit of its change. See "Changes and stacks" below.
- **Ops are structural.** An op names a syntax node by a stable identity (its item path plus a
  node ID that survives edits around it, assigned by the front end's syntax tree) and says what
  happens to it: replace its text, insert a child at a position, remove it, move it, rename a
  binding across its uses. A plain text edit is the fallback op only for files the front end does
  not parse (Markdown, TOML, C), and it is typed as such, not silent. The parse lives in the front
  end's query crates (semantics-in-front-end). The vfs stores the op log; the tree's bytes stay
  the result of applying it, so every node is still a file with a `ContentId`.
- **The server orders each head.** One writer per head (its Durable Object) gives every move a
  sequence number. A client's move says which sequence it was made against. A stale move is rebased
  by transforming its ops over the moves since (a structural op on a node nobody touched applies
  unchanged; a node someone removed or moved is followed or refused naming the item) or refused
  with the item that conflicts. This is the multiplayer model of Figma and Linear
  (server-authoritative, the client rebases its pending ops). It is not a CRDT: the ops act on a
  tree that already exists. The vfs's `rebase` (file by file, edit ranges moved) is the base case
  for unparsed files.
- **Merge is three-way over syntax trees.** The base is the commits' lowest common ancestor. A path
  that only one side changed takes that side. A file both sides changed is merged item by item and
  node by node: each side's diff from the base, as structural ops, is applied to the other, so
  reordered imports, two new functions at the end of a file, or an edit inside a function the other
  side moved merge cleanly. What still clashes (both sides changed the same node differently)
  becomes a typed conflict entry naming the item, and `blaze check` refuses to build it
  (errors-name-the-thing). Line diff3 applies only to files the front end does not parse.

## Prior art, and what this takes from it

- **jj (Jujutsu).**
  - Changes with stable IDs.
  - The working copy is a commit.
  - Descendants rebase automatically when an ancestor is rewritten.
  - Conflicts are first-class: they are stored in commits, so a rebase never stops halfway.
  - An operation log records every repo operation, so any one can be undone. Concurrent
    operations are merged, not locked.

  We take all of it. jj still stores text and merges lines, and its backend is git. Here the
  backend is the vfs, and conflicts and merges are on syntax nodes.
- **OpenAI's Pando** (2026-11-02). It keeps the git protocol and rebuilds the server for Codex's
  load:
  - Each ref is ordered on its own, with no repository-wide order. The result is causal
    consistency: two branches read together may be from different moments.
  - Packs are immutable objects in blob storage, reused across every reader instead of built per
    fetch, and compacted in the background like an LSM tree.
  - A writer reads its own writes. A push costs O(1) metadata operations.
  - The design was checked by deterministic simulation and TLA+/Lean specs.

  We take all of it: a Durable Object per head is the per-ref order, and R2 packs are the reused
  packs. Pando keeps git's data model (text, commits, line merges) because its users wanted the
  git client. We don't need the git client, so we can change the model.
- **Pijul and Darcs.** Patches that commute, so the order of independent changes doesn't matter.
  Structural ops on distinct syntax nodes commute the same way. This plan gets that property
  without Pijul's text-graph state.
- **Sapling, Graphite and stacked diffs.** Stacks of small changes, restacked when a lower change
  moves, and landed one at a time. This is the "branch based on a branch" below.
- **Figma and Linear.** A server-ordered op log with clients rebasing their pending ops. This is
  the live layer.

## Changes and stacks: a branch on a branch takes its updates

- A change names its **base**: trunk, another branch, or another change. A branch whose base is
  another branch is a stack.
- **When a base moves, every change on it is rebased onto the new base automatically** (jj's
  descendant rebase), across heads. Each head's Durable Object subscribes to its base's moves. On a
  move it does a structural three-way merge of its change onto the new base, then moves its own
  head. The cascade runs down the stack one level at a time. Each level records which base version
  it is on (`base@seq`), so a reader always knows how fresh a stack is (Pando's causal
  consistency, made visible).
- **A rebase never fails.** What doesn't merge becomes a conflict entry stored in the change,
  naming the item, as in jj. The stack keeps moving, and the owner of the conflicted change (a
  person or an agent) is told which item conflicts. Changes above it stay on the conflicted
  version until it is resolved; they are never silently dropped.
- **Landing** a change onto trunk is a move of trunk's head through trunk's Durable Object, which
  is a merge queue of one. Structural ops on distinct nodes commute, so most landings need no
  rebuild of what they don't touch. blaze keys every result by its inputs, so testing the merged
  tree reuses everything the change didn't reach.
- **The operation log** of each head (every move, rebase, land and resolve, with who did it and
  why) lives in its Durable Object. `blaze undo` reverts any operation, as jj's does.

## At the scale of an AI team

The workload is a few people and thousands of agents. Each agent makes many small changes, stacked,
all rebased continually, all built and tested.

- **Nothing global.** The only ordering is per head (Pando). Agents on unrelated changes never wait
  on each other. A trunk landing waits only on trunk.
- **A change per task, a live head per agent.** An agent's head is a view: constant time to open, no
  clone, no checkout. Spawning a thousand agents on trunk is a thousand view slots, not a thousand
  checkouts.
- **Claims on items, not files.** An agent may claim the syntax nodes it intends to change (a lease
  with a time limit, held in the head's Durable Object). Another agent sees the claim before it
  edits, which avoids conflicts rather than resolving them. A claim is advisory: edits still merge
  or conflict as above.
- **Gate by the exact tree.** A head reaches trunk only if `blaze check` and `blaze test` passed on
  the exact root it lands as (keys-cover-every-input). Because results are content-keyed, the gate
  for a rebased change re-runs only what the rebase changed.
- **Reads served from the edge.** Packs are immutable and content-named, so Cloudflare's cache serves
  them to every agent. A cold agent fetches what its build reads, not the history.
- **Proof before production.** The Durable Object protocol (moves, cascades, claims, landing) gets
  a model in blaze-check's simulation and deterministic replay tests before any team relies on it,
  as Pando was checked.

## The server: blaze-sync, a Rust Worker built by blaze

platforms.md shows blaze building workers-rs Workers that answer the same way rustc's builds do. So
the server is Rust, built by blaze (blaze-builds-blaze), and it reuses the vfs's own code instead of
a TypeScript copy.

- **Verifying in wasm.** A sans-IO core crate, `blaze-vfs-wire`, holds the node encoding, the
  BLAKE3 domains, the commit format and the closure check. It is shared by blaze-vfs and the
  Worker, compiled to `wasm32-unknown-unknown`. The Worker re-hashes every node it receives and
  stores none that fails (the "store never trusts bytes" review rule). The gateway is TypeScript
  only because blaze could not build wasm32 when it was written; that reason is gone.
- **Storage.**
  - R2 holds node packs (the `BLZPACK1` layout of [remote-cache API.md](../../../../tools/remote-cache/API.md),
    keyed by `ContentId`).
  - D1 holds node locations.
  - A **Durable Object per (tenant, head)** holds the head's current root or commit, its sequence
    number, and the recent move log. It is also the WebSocket hub for that head.
- **Order of writes (durability).** A node pack is stored and its locations recorded before any
  head may name it. A head move is accepted only when everything its root reaches is stored (the
  closure check). The Durable Object's storage, not R2, is the authority on a head's sequence.
- **Auth.** The gateway's tokens and tenants as they are today: private tenants' HMAC namespaces,
  and signed heads for branches (Ed25519, the gateway's key list). Live heads are authenticated
  per WebSocket session rather than per move.
- **Protocol.** HTTP plus a WebSocket, one versioned schema in deterministic CBOR (no-mcp). The CLI
  and the daemon are clients of it.

| request | does |
|---|---|
| `POST /v/<tenant>/have {ids}` | which nodes are stored |
| `POST /v/<tenant>/put` (pack) | store nodes; every one re-hashed in wasm |
| `POST /v/<tenant>/get {ids}` | a pack of nodes |
| `GET /v/<tenant>/head/<name>` | root or commit, and its sequence |
| `PUT /v/<tenant>/head/<name>` | move it, given the sequence it was made against (412 when stale: rebase and retry) |
| `WS /v/<tenant>/live/<name>` | send ops, receive every move as `(seq, ops, root)`, in order |

## The client: a sans-IO sync core in blaze-vfs

- `Sync` is a state machine on an injected clock (sans-io-cores). It works out which nodes the
  server lacks (a walk from the new root that stops at any node the server already has), which to
  fetch, and how to rebase pending ops onto a move it receives. The daemon's driver does the HTTP
  and WebSocket IO.
- Fetching is lazy: a view opens on a remote root, and the walk fetches a directory's nodes when
  the walk reaches them. A build reads only what it touches, so `blaze build tenant/branch/main`
  on a fresh ix VM needs no clone.
- The journal gets a `synced` record per head: the last server sequence applied, and the ops not
  yet acknowledged. A restart resumes from it. It needs a byte-for-byte restore test
  (persisted-state-round-trips).
- **Projection** (writing a head's changes to a directory on disk) is not built yet
  ([../../../../v2-vfs.md](vfs.md), "The disk as input"). Without it, a remote edit
  reaches builds but not the editor's files. Projection is a prerequisite for multiplayer in an
  editor, and is slice 3 below.

## To do

Each slice ends with a measurement on the exact tree it ran on (measure-the-exact-tree).
Durability and deletion changes are proven before merge by a Hegel deterministic simulation
test whose oracle checks the invariant (dst-not-review, the `testing` skill), not by a review.

### Slice 1: wire core and Worker skeleton
- [ ] `crates/blaze-vfs-wire`: node and commit encoding, domains, closure check; no IO, `no_std` + alloc
- [ ] blaze-vfs uses it for hashing (one-concept-one-home; ban the old helpers with `.clippy.toml`)
- [ ] `tools/blaze-sync`: workers-rs Worker built with `blaze build --target wasm32-unknown-unknown`; `have`, `put`, `get`, re-hashing in wasm
- [ ] Test in workerd on real R2 and D1 bindings; a forged node is refused, naming its ID
- [ ] Done when: the blaze repo's whole tree uploads and reads back byte-identical, with the time and size measured

### Slice 2: heads and push/pull
- [ ] Durable Object per head: root, sequence, move log; `GET` and `PUT` head with the stale check
- [ ] Closure check before any head move (review before merge: durability)
- [ ] `Sync` core in blaze-vfs plus the daemon driver; `synced` journal record with a round-trip test
- [ ] `blaze push <tenant>/<head>` and `blaze pull`; lazy fetch when a view opens
- [ ] Done when: an ix VM with no git checkout runs `blaze build` and `blaze test` on blaze from a pushed head, the outputs match the Mac's, and the fetched bytes are measured against the tree's size

### Slice 3: projection
- [ ] Write a head's diffs to its directory; the watcher ignores bytes blaze wrote itself (it already skips bytes the head has)
- [ ] A race between an editor save and a remote edit becomes `Watched::Conflict`, never a silent loss
- [ ] Done when: an edit pushed from machine A shows up in machine B's files and B's daemon rebuilds

### Slice 4: live multiplayer
- [ ] WebSocket hub in the head's Durable Object: ops in, ordered `(seq, ops, root)` out
- [ ] The client keeps pending ops, rebases them on every move it receives, and reports a conflict with its path
- [ ] Presence (who has which file and cursor) as ephemeral messages, never stored
- [ ] Browser client: a compiler in the page reads a synced view, so a page edits and builds the same head (the
      in-browser compiler, `crates/blaze-wasm`, was deleted on 2026-11-09; git keeps it)
- [ ] Done when: two humans and one agent edit one live head and converge to the same root, and p50/p99 from keystroke to remote view are traced (slow-is-p0-traced)

### Slice 5: changes, stacks, merge, operation log
- [ ] Commit and change nodes; `blaze new`, `blaze log`, `blaze branch`, `blaze switch` (a switch is a view: constant time)
- [ ] Lowest common ancestor over the commit DAG (Durable Object or client side; measure both)
- [ ] Three-way tree merge plus diff3 text merge; first-class conflict entries stored in changes, refused by `blaze check` with the item named
- [ ] Bases and stacks: a head subscribes to its base's Durable Object and rebases itself on every move; `base@seq` recorded
- [ ] Operation log per head; `blaze undo`
- [ ] Landing through trunk's Durable Object, gated on check and test of the exact root (review before merge: durability)
- [ ] Model the cascade and landing protocol in blaze-check before relying on it
- [ ] Done when: a stack of 20 agent changes follows trunk through 100 landings with no stuck level, every conflict names its item, and blaze's own branches merge in the vfs with the same result git gives on a corpus of real merges, every difference explained

### Slice 5b: many agents
- [ ] Claims (leases on syntax nodes) in the head's Durable Object, shown to editors and agents
- [ ] Spawn N agent heads off trunk as views; measure time to first build with no clone
- [ ] Load test: 1,000 agent heads pushing at once; push and land latency p50/p99 from spans, with nothing cross-head on the path

### Slice 6: structural ops
- [ ] Stable syntax node identities in the front end (survive edits around them; deterministic, so every replica assigns the same)
- [ ] Op set: replace, insert child, remove, move, rename binding; a typed text op for unparsed files
- [ ] Transform rules for every pair of ops; unit tests here (hard logic, stable interface)
- [ ] Editors and agents send structural ops (an LSP-side translator turns keystrokes into ops on the node under the cursor)
- [ ] Structural three-way merge replaces diff3 for Rust files
- [ ] Done when: replayed concurrent histories (blaze's own merge corpus) produce fewer conflicts than git's line merge, each remaining one names an item, and every merged tree builds and passes its tests

### Slice 7: git mirror and GC
- [ ] Import a git repo (blobs to nodes, commits to commits, modes kept) to move a team onto the vfs
- [ ] The Worker mirrors every branch to a git remote one way (same SHA-1s on a round trip); pushes to the mirror are refused
- [ ] GC: mark from every head and branch per tenant and sweep packs; the gateway's lifecycle rules (review before merge: data deletion)
- [ ] Done when: blaze's own history imports, exports byte-identical, and blaze develops on the vfs for a week with git only as a mirror

## Open questions

- **One hash or two.** remote-cache.md picks SHA-256 because the TypeScript Worker could only hash
  with the runtime's SHA-256. A Rust Worker can verify BLAKE3. Should the CAS move to `ContentId`
  (one hash per concept), or should vfs nodes live in a separate namespace?
- **Durable Object memory** (128 MB) limits a server-side tree diff. The plan has the client walk
  the tree; measure before moving the diff to the server.
- **Node identity.** Stable IDs need either a stored ID per node (in the op log) or a deterministic
  match by item path and position. Stored IDs are sturdier, but every replica must agree on them.
  Pick by measuring both on the merge corpus.
- **Cascade cost.** A deep stack under a busy trunk rebases on every landing. Should it debounce
  (rebase at most every N seconds), or rebase lazily when someone reads the head? Measure in slice
  5b.
- **Claims.** Should a claim block other edits or only warn? The plan says warn. Change it only if
  the load test shows wasted agent work from conflicts.
- **Ordering of slices.** Slice 4 can ship with text ops and switch to structural ops in slice 6,
  or wait for slice 6. Shipping text ops first is faster to land, but the protocol must version
  the op set from day one.
