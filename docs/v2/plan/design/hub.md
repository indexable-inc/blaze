---
status: plan
summary: The hub keeps warm daemons for many agent lanes. A lane is a root (a BLAKE3 Merkle tree) an agent reads and changes through one API with no checkout; every change is one transaction to a new root, checked at once on a warm machine, the verdict awaited by root. Git stays the origin and the hub is an index of it, every object known by its content id and its git id. Trust, routing, what to measure, adoption, and the order of building.
date: 2026-11-11
---
# The hub

Agents are the users. An agent's loop is: read a few files, change some, ask "does it check, do the
tests it reaches pass", read the first error with its place and its fix. The hub makes each of those
one call whose cost follows the edit, on a machine that is already warm, for many lanes at once.
The bar is makepad's devhub (nine lanes, 300 jobs an hour, 50 to 200 ms to ship an edit, native
builds counted per lane); the aim is to be better in kind: no checkout to ship, results shared by
code identity across lanes and machines, every answer about a named root, a shared machine that is
safe.

## What exists, and is extended

- **The daemon's API** (`crates/blaze-api/src/lib.rs`): schema 50, deterministic CBOR in frames,
  Unix socket and `POST /api` with a bearer token. `Request::Commit`, `Check`, `Build`, `Test`,
  `Open`, `Reach`, `WillSave`, `Status`; `ViewRef::{Head, Root, WhatIf}`; diagnostics streamed as
  `Report`s ([structured-output.md](structured-output.md)); `Worked::misses` and `why`. One daemon
  per `BLAZE_HOME` already serves every checkout as a head (`blaze-cargo/src/serve/heads.rs`).
  The hub adds lanes, owners and a ledger in front of it; a job is the daemon's own request.
- **The VFS** (`crates/blaze-vfs`, [vfs.md](vfs.md)): a Merkle tree by `ContentId` (BLAKE3), heads,
  views, `apply(base, ops)`, a journal with a DST. A file's id is `FILE_DOMAIN`; a directory is one
  flat node (`DIR_DOMAIN`), as git's is. [vfs-sync.md](vfs-sync.md) plans a `no_std` wire core for
  nodes and a sync by `have`/`put`; none of it is built.
- **blaze-ci** (`crates/blaze-ci`, [ci.md](ci.md)): a webhook to a what-if view per pull request on
  a warm daemon, check runs posted; deployed nowhere, and it runs fork pull requests on the shared
  daemon (`service.rs` ignores `from_fork`). It becomes the hub's git and GitHub edge.
- **The store** (`crates/blaze-cas`, [store.md](store.md), [store-pool.md](store-pool.md)): keys are
  BLAKE3 of exact inputs, every read verified, one writer, a read-through remote (`remote.rs`,
  loopback only) and a signed global cache (`crates/blaze-global`). Test selection
  (`crates/blaze-testsel`), sandboxes (`crates/blaze-usercode`), admission by footprint
  (`blaze-graph/src/admission.rs`) exist. Nothing is named lane, hub or tenant.

## The tree (built: `crates/blaze-hub/src/tree.rs`)

A checkout is a root hash. **A file** is BLAKE3's tree hash of its bytes under the VFS's own
`FILE_DOMAIN`, so it is one object in the hub, the VFS and the store. Because that hash is a binary
tree over 1 KiB chunks, a range of a file can be sent with the hashes beside its path and verified
without the rest (Bao's verified streaming); that is how a file past one frame is written and read
(step 3; until then a file past 8 MiB is refused by name). Fixed BLAKE3 chunks, not content-defined
ones: source files are small and an edit resends kilobytes, a second chunk hash would give a file
two identities, and a verified range needs the file's own tree. Content-defined chunks buy
deduplication of shifted large binaries, which a lane does not edit.

**A directory** maps names to (kind, id) and is a search tree whose shape is a function of its
names: a name whose hash has `5(l+1)` trailing zero bits ends a node of layer `l` (a Merkle search
tree, as Dolt's prolly trees and Bluesky's repositories use). Nodes hold 32 entries on average, a
directory of 100,000 entries is four layers, one changed entry makes at most two new nodes a layer,
and the same entries give the same root however the directory was made. Every node read is held to
the encoding first: layer and count bounds, names in order, no entry inside a node that would have
ended it, a child's layer and last name as its parent says. Today a change rebuilds each directory
on its path from its entries (few new nodes, work linear in that directory); splicing only the
touched nodes is step 2, when blaze-vfs takes this node format in place of `DIR_DOMAIN` 2.

**The root names the exact tree** (rule measure-the-exact-tree) for every job, measurement and
answer. The daemon's keys stay below it and never hold it: a query is keyed by what it read (a
file's id, then item hashes, then a function's code identity, then the store's record key), so two
roots that share a file share everything derived from it, and file tree, items, code identities and
compiled records are one content-addressed graph of which the root is the top.

Against others: git's trees are flat (a big directory is rehashed whole), hashed with SHA-1, with no
verified ranges and no object without a repository. Sapling and EdenFS keep git's or Mercurial's
flat manifests and add lazy fetch and a mounted file system; the hub needs neither, an agent reads
by path. Unison hashes definitions, not files: blaze has that one level up, in code identity
([code-identity.md](code-identity.md)), and this tree is what feeds it.

## Lanes and the agent API (built: `hub.rs`, `wire.rs`; no driver yet)

A lane is a name, an owner, a base root and a root. `Open{lane, base}` stores a root: milliseconds,
whatever the repository's size. `Fork{from, lane}` copies a root: two approaches side by side.
The calls, one schema (`wire::SCHEMA`), deterministic CBOR over a Unix socket or HTTP, JSON by the
same types; `blaze hub ls|status|inspect <lane>` and every other command are clients of it:

| call | answer |
|---|---|
| `Read{lane, path, at, len}` | at most 1 MiB of the file, its size, id and mode, and the root |
| `List{lane, path, after}` | at most 1024 entries in name order, `more` |
| `Change{lane, on, ops}` | every op (write, range edit, delete, rename) or none; the new root and the eager check's job |
| `Job{lane, at, kind, task, targets}` | `Queued`, then `Started` (warm, or cold and why), diagnostics, `Ended` |
| `Ls`, `Status`, `Inspect{lane}` | names and counts for all; root, last roots, jobs and outcomes for the owner |

`on` is the root the change was written against: a lane that moved refuses it naming both roots.
Every change starts a check of the new root by itself; asking for that check joins it, and a newer
root's job takes an older one's place. A job names the task it is for and the root it judges, and
ends in one outcome (rule every-agent-has-a-task); the hub, not the request, names the view. Errors
name the path, the op's index and the rule. To come on the same schema: `Search` (bounded matches
with their place), diagnostics for the edited files first then the rest, each a `Report` with its
fix, and `Test` of only what the change reaches. A patch is `Op::Edit`; a unified diff is parsed by
the client into edits.

A local directory with a watcher that ships its changes as `Change`s is the compatibility path,
built after the API (step 5). A mounted file system for people is out of scope.

## Git is the origin (built: `git.rs`; the protocol and export are not)

**The invariant: the hub is an index derived from git and deterministic builds. Turning it off
loses warmth, never work.** Its test: build a second hub from the remote alone and compare every
root and every job's verdict by root.

- One table gives every file and directory two names, its content id and its git object id (SHA-1,
  or SHA-256 where the repository uses it). A commit's tree is imported once per tree id, each
  object checked against its git id first; an export hashes only what a change touched.
- Rules: modes `100644`, `100755`, `120000`, `40000` are the tree's kinds and any other is refused;
  git's order (a directory sorts as `name/`) is written on export; a tree that is not the bytes git
  writes for its entries is refused, so every tree round-trips byte for byte; no empty directory;
  a root holds the repository's form of a file, so an LFS pointer stays a pointer and
  `.gitattributes` filters (line endings) apply only where files reach a working directory; a
  submodule is refused by name until an entry can hold a commit (step 4).
- Export: a lane's root becomes a commit on its base (author the lane's owner, the task in the
  message), pushed to the company's own remote as an ordinary branch; verdicts are posted as commit
  statuses. A tool that needs a `.git` gets one synthesized from the same table.
- Reading: the hub serves git's smart protocol, so a stock `git clone` or `git fetch` of a lane
  works, the commit and objects made from the root on demand; beside it a native fetch by root.

## Sync by root (specified; step 5)

Between machines, and for the local directory: equal roots, nothing to do. Else the sender offers
the root, the receiver walks from it and answers the ids it lacks in batches (`missing(hashes)`,
the shape of Bazel's `FindMissingBlobs`), the sender sends those, every object verified against its
id before use, a root accepted only once its closure is held. A one-file save is the changed file
and one node a layer, sent with the root: one round trip, under 50 ms on a LAN. With no common
base the same descent works, a round a layer, because the shape is canonical: range-based set
reconciliation would save nothing a history-independent tree does not already give. A lane's
`missing` is answered only from its base and its own objects, never from another lane's.

## Routing and memory

One daemon serves every lane of a repository from one store, so two lanes never compile a function
twice (results are keyed by code identity, not by lane). A job goes to the machine that holds the
lane's base in memory, else on disk, else the one with the most headroom, which first pulls records
from the shared store: a cold start is a download. Admission is by footprint (rule
one-memory-measure): a lane's charge is what its jobs add over the base, and a lane idle past its
bound is retired. Every job that starts is counted warm or cold with the reason (no machine had the
base, moved for load, evicted, daemon restart, other targets, other toolchain), per lane.

## Trust

- Only the hub's own sandboxed builders write the shared store; a lane's machine and every client
  read. Every read is verified by hash, and one result in N is recomputed and compared.
  **The guarantee: a cache hit is byte-identical to a fresh build.**
- Build scripts and proc macros are always sandboxed; on the hub tests are too, and a test that
  must run outside the sandbox is refused there, not run.
- A lane is read and changed only by its owner. No call names an object by id, so the shared store
  is not an oracle for another lane's uncommitted bytes; `ls` shows names and counts only. Secrets
  are a lane's, given to its own sandboxed jobs, never written to the store or a log.
- Clients are authenticated (Unix peer or bearer token today, single sign-on for a company) and
  access per repository mirrors the git host's. Everything decoded has a bound: frames, ops, file
  sizes, node layers and entries, names, depth, a lane's lifetime writes.
- Self-hosted in the company's own cloud account: no source leaves its network. An audit log holds
  every job and every store write.
- Fork pull requests never run on the shared daemon ([ci.md](ci.md)).

## Before landing

A branch is a lane whose base is the target branch: the hub checks its root, posts the verdict as
a required status, and `land.py` merges only a root the hub passed. Cold jobs and their reasons are
on the lane's row.

## What it must show

Time from change to first diagnostic (p50, p99); jobs an hour; share of jobs served warm, and the
cold ones by reason; memory per lane; bytes sent per change; time to open and to fork a lane.

## Adoption, each step useful alone

1. `blaze build` and `blaze test` in place of cargo, no server.
2. A shared store in the company's cloud: a cold start is a download.
3. The hub for agent lanes.
4. The hub's check as a required status on pull requests.

## Order of building

1. **The lane, the root, the transaction, git's ids, the schema** (`crates/blaze-hub`, on bstd).
   Check: its tests (the model test with its planted bugs, the git round trip).
2. **The driver in the daemon**: the hub's socket and HTTP, lanes as VFS heads, blaze-vfs on this
   node format with spliced updates, objects in the store's pool, the lane table restored after a
   restart (round-trip test). Check: change to first diagnostic on a warm ix VM, under 100 ms.
3. **Jobs for agents**: diagnostics for edited files first, reached tests, `Search`, files in
   verified ranges. Check: two lanes making the same edit compile its functions once.
4. **The git edge**: blaze-ci imports commits and exports lanes, statuses, the smart protocol,
   submodules. Check: the invariant's test, a hub rebuilt from the remote has the same roots.
5. **Machines**: sync by root, the router, the shared store's builders, sampled recomputation, the
   local-directory path. Check: jobs an hour and the warm share on two machines.
6. **Companies**: single sign-on, the git host's permissions, the audit log, the required status.
