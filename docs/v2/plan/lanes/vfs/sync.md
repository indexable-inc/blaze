---
status: plan
summary: blaze v2 vfs - the team's source of truth lives on Cloudflare (a Rust Worker built by blaze, run as wasm, a Durable Object per head); multiplayer edits and merges are operations on the syntax tree, not text; commits, branches and merge in the vfs; blaze builds straight from a synced head; git is only a mirror.
date: 2026-11-04
---
# blaze v2 vfs: synced, multiplayer, no git

2026-11-04. A plan and to-do list. The vfs ([../../../../v2-vfs.md](../../../../v2-vfs.md)) already
has content-addressed nodes, named heads, constant-time views and a file-by-file rebase. This plan
adds three things:

1. **Sync.** Every machine on a team (the Mac daemon, ix VMs, CI, a browser running blaze-wasm, an
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
- **A commit is a node too**: `{root, parents[], author, time, message}` under a new domain
  `"blaze v2 vfs commit 1"`. History is a DAG of commits, as in git. The hash is BLAKE3, not SHA-1.
- **A branch is a head that names a commit**: `branch/<name>`. A **live head**, `live/<name>`, names a
  root that changes on every keystroke batch, the way a workspace head does today. A commit
  snapshots a live head onto a branch.
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
  - R2 holds node packs (the `BLZPACK1` layout of [remote-cache API.md](../../../../../tools/remote-cache/API.md),
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
  ([../../../../v2-vfs.md](../../../../v2-vfs.md), "The disk as input"). Without it, a remote edit
  reaches builds but not the editor's files. Projection is a prerequisite for multiplayer in an
  editor, and is slice 3 below.

## To do

Each slice ends with a measurement on the exact tree it ran on (measure-the-exact-tree).
Durability and deletion changes get a review before merge.

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
- [ ] Browser client: blaze-wasm's `Compiler` reads a synced view, so a page edits and builds the same head
- [ ] Done when: two humans and one agent edit one live head and converge to the same root, and p50/p99 from keystroke to remote view are traced (slow-is-p0-traced)

### Slice 5: commits, branches, merge
- [ ] Commit node; `blaze commit`, `blaze log`, `blaze branch`, `blaze switch` (a switch is a view: constant time)
- [ ] Lowest common ancestor over the commit DAG (Durable Object or client side; measure both)
- [ ] Three-way tree merge plus diff3 text merge; typed conflict entries that `blaze check` refuses
- [ ] Review: a "PR" is `pr/<n>`, a branch head plus its merge target; CI builds the view at that merge
- [ ] Done when: blaze's own branches merge in the vfs with the same result git gives on a corpus of real merges, every difference explained

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
- **Ordering of slices.** Slice 4 can ship with text ops and switch to structural ops in slice 6,
  or wait for slice 6. Shipping text ops first is faster to land, but the protocol must version
  the op set from day one.
