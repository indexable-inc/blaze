# blaze-next: the floor of an edit, for agents

2026-10-24. Status: design; the implementation is on the `blaze-next` branch. Written against
main `96538afe71`. Each claim is marked **read** (in a source we read, or measured by us),
**reported** (a document or someone says so, not re-checked here) or **unverified** (our
inference or an estimate). What was fetched and measured is in
[upstream/roadmap/](upstream/roadmap/SOURCE.md). The umbrella issue is
[#279](https://github.com/indexable-inc/blaze/issues/279).

blaze is ra-jit's new name; code paths below are still `ra-jit` where the rename has not reached
them.

## Short version

**The architecture.** blaze's source of truth is a content-addressed Merkle tree in the daemon,
not the files on disk: editors, agents, git checkouts and the CI runner write atomic multi-file
transactions into it, each producing a new root hash, and disk is one input and one projection.
A build pins one root. What changed is the diff of two roots, which skips equal subtrees by hash,
and that diff pushes invalidation through a compact dependency graph with early cutoff at
signatures, so an edit redoes only the functions whose inputs changed. The core that does this
(the VFS, invalidation, scheduling, the store's index, the protocol, the patch planner) is a set
of sans-IO state machines that take events and emit IO requests, run by a deterministic
in-memory backend in tests, by threads on macOS and by io_uring on Linux. Its state is an image
that a restart, a new worktree or an ix fork maps instead of rebuilding. Code reaches the running
program as a patch in place, before the borrow check and diagnostics finish, and is rolled back if
they fail. Agents drive all of it over a Unix socket (or HTTP off the machine) with structured
results, and lints run in the same pipeline, per function.

- **Agents are the users.** Edits come as whole-file writes and multi-file patches, in bursts,
  from many agents at once, and the result an agent waits for is structured (diagnostics with
  spans, tests run and skipped, run output). The latency that counts is edit tool call to
  structured result. Keystroke speculation matters little; what-if builds on candidate roots
  matter a lot ([below](#who-it-is-for-coding-agents)).
- **The floor.** A nushell body edit is 258 ms today (**read**). The stages it needs at the
  floor sum to about 20-30 ms ([the table](#the-floor-stage-by-stage)); the largest part of what
  is spent today is bookkeeping (noticing the edit, salsa revalidating, waiting for the borrow
  check, linking), not compiling.
- **The VFS is a Merkle tree, not overlays.** An "overlay" is a root that shares unchanged
  subtrees. Measured on nushell (2,240 files, 15.1 MB, one M5 Max core): hashing the whole tree
  with BLAKE3 is 10.5 ms, a three-file 233 KB transaction 135 us, and the diff of the two roots
  11 us (**read**).
- **Items by content above it.** Every function, type, trait, impl, const, static and macro is
  hashed by its form after expansion, resolution and inference, with what it refers to as slots
  and names in a separate map, as Unison does; a rename, a move, rustfmt or a reorder changes no
  hash. The code key takes callees' interfaces, not their bodies, so a body edit stops at its
  function ([below](#1b-items-by-content-the-layer-above-the-vfs)).
- **Isolation, settled.** An ix VM fork (copy-on-write memory and disk, 1.7 s at best,
  **reported**) is the boundary for untrusted code and whole jobs; a new root (microseconds, on
  any Mac) is the unit of speculation and sharing in one warm daemon. No `fork(2)` of the daemon
  as a middle tier ([below](#2-isolation-ix-vm-forks-and-roots)).
- **Sans-IO, and where io_uring pays.** IO is not where today's time goes: a polling pass over
  every nushell file is 1.8 ms and reading all of them 24.5 ms (**read**, Mac), and helix's
  129 s of "store read" thread time is decoding and interning, not syscalls (**read**). io_uring
  pays where IO is on the critical path with a cold page cache (a fresh ix fork, CI, VFS group
  commit). The main gain of sans-IO is deterministic simulation of the daemon.
- **Two concurrency planes.** Control and IO: shared-nothing state machines and messages,
  sharded by workspace. Compute: immutable snapshots read lock-free by a work-stealing pool, one
  committer for each new revision, since a front-end memo is about 8 ns of work and a message
  costs more than that ([below](#the-concurrency-model)).
- **No MCP.** The agent API is one versioned schema in deterministic CBOR over a Unix domain
  socket and HTTP; `cargo blaze` is a client of it and den wraps it as hub tools. One value has
  one encoding, so requests and results can be hashed and stored like everything else.
- **Delivery.** One integration branch, `blaze-next`, with one draft pull request, workstreams
  merged into it and main merged in often; it lands when the full gate and every differential
  harness pass on it ([below](#11-delivery-and-order)).

## Who it is for: coding agents

Andrew: "this is incremental compilation/run for LLMs, which do not have keystrokes." What that
changes against an IDE-shaped design such as rust-analyzer's:

| | human in an IDE | coding agent |
|---|---|---|
| an edit | a keystroke, one file, a stream | a whole-file write or a multi-file patch (`Write`, `Edit`, `MultiEdit`, `apply_patch`), in bursts |
| consistency | each keystroke a revision | a transaction is the unit: a build sees all of it or none of it |
| speculation | compile as you type | evaluate a patch before applying it; several candidates at once |
| concurrency | one person, one checkout | many agents, each on its own branch of the same repository |
| what comes back | squiggles, hover | structured diagnostics, tests run and skipped and why, failures with minimal output, run results |
| latency that counts | keystroke to squiggle | tool call to structured result, inside the agent's loop |
| trust | the person's own code | generated code: sandboxed, and results must be deterministic, as a flaky result misleads the agent |
| how changes arrive | the editor's `didChange`, a watcher | written into the VFS directly; no watcher on the agent's path |

How den's agents edit today (**read**, den `crates/den-claude/src/tool.rs`, `den-codex`):
through the agent's own `Write`, `Edit` and `MultiEdit` tools, or Codex's `apply_patch`, which
write to disk; den's `vfs_*` hub tools reach only den's own space of notes and skills
(den `docs/vfs.md`), and `code_run` runs `cargo run` (den `docs/code-lsp.md`). So today an agent's
edit reaches blaze through the disk and the client, and the first step is a socket call that
takes the same write as a transaction.

## Where an edit's time goes today, and the floor

nushell, a body edit of `main` (507 lines in one function), Cranelift, 16 pinned cores, from
[lean-frontend.md](research/lean-frontend.md) after its slices 2 to 4 (**read**):

| stage | today | what it is |
|---|---|---|
| notice | 35 ms | the cargo client starting and asking; the watcher's pass |
| apply | 21-23 ms | outlines lowered for the edited file |
| roots | about 6 ms | the build's crates named (was 40 ms before #252) |
| check | 155-200 ms | `TraitImpls::for_crate` verified crate by crate (37-75 ms, others waiting), inferring `main` (about 70 ms), MIR and code |
| build-rest | 35-76 ms | waiting for the borrow check of `main` on the diagnostics thread |
| refuse and link | 28 ms | the link is 14 ms |
| total | 258 ms | |

A signature edit (`coerce_bool` in nu-protocol) is 268-285 ms and asks the patch base about
106,256 instances; a warm restart with everything in the store is 35.4 s on nushell and 38.3 s on
helix (**read**, warm-start.md).

### The floor, stage by stage

The same edit made by an agent through the socket. Targets, **unverified** until each workstream
measures them:

| stage | floor | how |
|---|---|---|
| the transaction reaches the daemon | 0.3 ms | a socket request, no process started; hashing and path copy 135 us and the diff 11 us were measured for a larger transaction (**read**) |
| changed items found | 1-2 ms | the diff gives files; item hashes give the items; only changed items are lowered again |
| invalidation | under 1 ms | pushed from the changed items through the dependency graph; nothing is verified that the edit did not reach |
| infer, MIR and code for `main` | 15-25 ms | the work itself: one 507-line function. Inference without the tolerant path's overhead, Cranelift on one function |
| borrow check and diagnostics | 0 on the path | the patch goes out when code exists; diagnostics follow and an error rolls it back |
| patch | 1-2 ms | code mapped into the running program or appended behind the reply; no link on the path |
| structured result | 0.2 ms | one reply on the socket |
| **total** | **about 20-30 ms** | |

A signature edit's floor is proportional to its real dependents: the callers whose inference
result changes are inferred again and only instances whose MIR changed are compiled. A warm
restart's floor is mapping the image and diffing its root against the checkout: under a second,
no inference.

## 1. The VFS: a content-addressed Merkle tree

[#271](https://github.com/indexable-inc/blaze/issues/271). Andrew: "the VFS is always the source
of truth", and "would the VFS be a Merkle tree?"

### The model

- **Nodes are immutable and named by their hash.** A file node is the BLAKE3 hash of its bytes
  (files up to 1 MiB are one blob; larger ones are cut by content into chunks so an edit in the
  middle shares the rest, as FastCDC does). A directory node is the hash of its sorted entries
  (name, kind, child hash). The same tree has the same root everywhere.
- **A root is one hash**, and a write copies the path from the file to the root, sharing every
  other subtree. A head per workspace (agent, branch, checkout) names the root it is at.
- **There are no overlays.** An agent's branch, a what-if candidate, an edit transaction and a
  git commit are all roots; they share whatever they have in common by hash.
- **A build pins one root.** It is a consistent snapshot by construction: nothing written after
  the build began is in it, and no half of a transaction is.
- **What changed is the diff of two roots**, which descends only where hashes differ. That is
  what replaces the watcher's pass and what feeds invalidation
  ([section 4](#4-push-invalidation-per-function)).

**Hashing.** BLAKE3, not git's SHA-1: on nushell's tree it is 10.5 ms against 14.7 ms for git's
blob hashing (**read**, one thread), SHA-1 is broken, and BLAKE3's own tree mode allows a large
file to be hashed in parallel and verified by range. A git object id maps to a node through an
index in the store, so a checkout of commit X is a root built from git's objects: each blob read
and hashed once, then the mapping is kept (git's own tree format is not reused, as its ids are
SHA-1 or SHA-256 of a different encoding) (**unverified** cost beyond the hashing measured).

**What a write costs** (**read**, `measured/merkle-proto-nushell.txt`, one M5 Max core, a
throwaway prototype with `BTreeMap` directories and `Arc` nodes):

| | nushell |
|---|---|
| files, bytes, deepest path | 2,240, 15.1 MB, 8 |
| whole tree hashed with BLAKE3 | 9.8-10.8 ms |
| git blob SHA-1 of every file | 14.2-15.2 ms |
| a transaction of three files (233 KB: `main.rs`, `nu-protocol`'s `value/mod.rs`, `each.rs`), hashed and path-copied | 131-138 us, of which hashing 106 us |
| diff of the two roots | 10.7-11.1 us, 225 nodes visited |
| BLAKE3 throughput, one thread | 1.9-2.1 GiB/s |

So hashing per write is about 0.5 us per KB written, and a diff costs nodes visited along the
changed paths, not files in the tree. The in-memory representation for the hot path is not the
prototype's: nodes are interned to u32 ids in an arena (the hash stored once), directories are
sorted arrays of (name id, node id), and file bytes live in the store's mapped packs, so a
lookup is a binary search per path component (depth 8 on nushell) over memory that is already
mapped (**unverified** until built).

### Writes: transactions against a base

- `apply(base_root, ops) -> root`, where `ops` are whole-file writes, range edits (offset, length,
  bytes, as LSP's `didChange` and `str_replace` give), deletes and renames, atomically.
- A transaction names the root it was made against. On the head's root it applies as is. On an
  older root it is rebased file by file: a file nobody else changed takes the write; range edits
  that touch disjoint ranges merge; anything else is refused with the conflict (path, both
  versions' hashes), so the agent decides. A whole-file write over a file changed since its base
  is a conflict, not a silent overwrite (**unverified** that this is enough for agents; measured
  by the agent API's harness).
- Every root keeps its parent and the transaction that made it, so "what did this agent change"
  and undo are walks over roots, and the store keeps the history for as long as a root is live.

### Disk: one input, one projection

- **Input.** A watcher (FSEvents, inotify or fanotify, and today's stat pass where none) turns a
  write on disk into a transaction by the "disk" author, applied to the head. For ordering it
  uses Watchman's cookie: the client writes a cookie file and waits for its event, so a build
  sees every write before the request (**reported**, Watchman docs). On macOS FSEvents gives no
  ordering guarantee and a settle period is the workaround (**reported**, same page): one more
  reason agents write through the socket, not the disk.
- **Projection.** The root a workspace has checked out is written to its directory, file by file
  for the paths the diff names, so ordinary tools (`git`, `rg`, editors, build scripts) see
  files. A write blaze made itself is remembered by hash and not taken back in as an edit (today's
  `sent` map does this for the editor's text).
- **Reconciling.** Someone running `sed` on disk is a disk transaction like any other; if an
  agent's transaction on the same file arrived first and is not yet projected, the two are merged
  as above or the conflict is reported to both.
- **Mounts, optional.** For a root that is never materialized (a what-if candidate that a build
  script or test must read as files), a mount serves it read-only: FUSE on Linux, NFSv3 or FSKit
  (macOS 15.4 and later) on macOS, ProjFS on Windows, as EdenFS does (**reported**, EdenFS
  overview). ix already has a FUSE server in its buck2 fork: +0.9% wall on a kernel build in
  no-open mode, against +17% for a plain passthrough over `/dev/fuse` (**reported**, indexable's
  `fuse-input-tracing.md`). A mount is never on the agent's edit path.

### Many roots, one store

- Nodes go in the same content-addressed store as compiled functions, under content-only keys
  (the work of #105 and the warm-store lane), so a second worktree, another machine or an ix fork
  needs only the nodes it lacks: syncing a root is "which of these hashes are missing", then a
  pack of them.
- GC is reachability from live roots: heads, pinned builds, what-if roots an agent holds, and
  the roots of the last N transactions per workspace.
- Two agents on two branches of one repository are two heads in one daemon on a Mac, sharing
  every unchanged file, item and memo by hash; on ix, two forks of one seed share pages by
  copy-on-write instead ([section 2](#2-isolation-ix-vm-forks-and-roots)).

### What exists today

- **rust-analyzer's `vfs`** (**read**, `vendor/rust-analyzer/crates/vfs/src/lib.rs`): a flat map
  from `FileId` (an interned path) to bytes with a change log that salsa drains; no versions, no
  roots, no hashes; IO and watching are a separate `loader::Handle`. It stays as the front end's
  file table, fed from the Merkle VFS, until the front end reads nodes directly.
- **ra-jit's own** (**read**, `tree.rs`, `main.rs`): every watched file's mtime is stat-ed in a
  loop with 20 ms sleeps until a pass finds nothing new; `tree.rs` lists directories whose mtime
  moved; an `edit\t` request carries one file's whole text (`Request::Edit`); `content.rs` names
  crates by content so checkouts share crates. The heap profile's "ra-jit's own (vfs, symbol
  table, splice)" is 988 MB on nushell (**read**, lean-frontend.md); how much of it is file text
  was not measured (**unverified**: the source is 15 MB, so most of it is the symbol table and
  the splice's image).
- **den-vfs** (**read**, den `docs/vfs.md`): a sans-IO Loro CRDT space with per-file versions
  and hub tools, for notes and skills. Different job (merging concurrent prose); its sans-IO
  shape and its versioned path API are the model for blaze's API, not its storage.

### Prior art

| system | what it shows | how blaze differs |
|---|---|---|
| git trees | content-addressed directories, a commit is a root | blaze keeps git's model, hashes with BLAKE3, and builds roots from git objects without a checkout |
| EdenFS / Sapling | a virtual checkout served lazily from source control; FUSE, NFSv3, ProjFS; file hashes without reads (**reported**) | blaze's projection is a materialized directory by default; the mount is for roots nobody materializes |
| Google CitC / Piper | a workspace stores only what the developer changed, over the repository at a revision; most reads come from build and test (**reported**, CACM 2016) | the same idea per agent, inside one daemon or one ix fork |
| Bazel Skyframe | bottom-up invalidation must `stat()` every input unless told; change pruning resurrects nodes whose value did not change (**reported**, Bazel docs) | the root diff is the "told", and early cutoff is change pruning |
| REAPI `Directory` digests | Merkle input trees for remote execution (**reported**, `remote_execution.proto`) | the same shape; an edge adapter can speak it (#226) |
| Nix store, Unison | everything named by content, nothing mutated in place (**reported**) | blaze's store already is; the VFS joins it, and items follow Unison's hashing ([section 1b](#1b-items-by-content-the-layer-above-the-vfs)) |
| Watchman | cookies to order a query after every write before it (**reported**) | needed only for the disk input |

## 1b. Items by content: the layer above the VFS

[#344](https://github.com/indexable-inc/blaze/issues/344). Andrew: "A VFS layer but also a
'Unison' layer: VFS (Merkle tree) -> Unison, where EVERYTHING is proper CAS like Unison: every
function has its syntax tree and the things it depends on, anonymized, so a refactor would still
be cached."

The VFS says which files changed. This layer says which items changed, and what each item is
whatever it is called and wherever it is written, so a rename, a move to another file or module,
rustfmt or a reorder changes no hash and recompiles nothing. The claims behind this section, and
what it takes from Unison's hashing and codebase format and from den's `structure.md` and
`build.md`, are in [research/item-identity.md](research/item-identity.md).

### The model

- **An item** is a function, struct, enum, union, trait, impl, const, static, type alias or
  macro; a closure is part of the body it is written in. Its **form** is what it is once macros
  are expanded, `cfg` evaluated, names resolved and the body inferred: generics and bounds, the
  signature, the attributes that change code or linking (`repr`, `inline`, `track_caller`,
  `target_feature`, `no_mangle`, `export_name`, `link_section`, `used`, not docs or lints), and
  the body as MIR. Blocks and locals are numbered in the order a walk from the start block
  reaches them. Names, spans, comments, formatting and item order are not in it, and every item
  it refers to is a numbered **slot**, never a path.
- **Four hashes** fill the slots differently:

| hash | slots filled with | changes when | what it keys |
|---|---|---|---|
| local | nothing | the item's own form changes | the item-level diff of two roots |
| interface | interface hashes of what its signature refers to | what a dependent can see changes: a signature, a type's fields, variants and `repr`, a trait's items, an impl's header and associated types, a const's value | early cutoff |
| code | interface hashes of everything it refers to | its own form, or an interface it uses | compiled code, in the daemon and in the store |
| deep | deep hashes of everything | anything it reaches (Unison's hash) | test results, code with bodies inlined into it |

  Unison and loom's hash-rustc have only the deep one, which pins the bodies of dependencies and
  so gives exact cones and no cutoff, as den's `build.md` (3g) says of hash-rustc. blaze's code
  for a function depends on its callees' interfaces and not on their bodies, because a build
  inlines nothing across functions (**read**, [support.md](support.md)): a callee's body edit
  leaves every caller's code key as it was.
- **Cycles** hash as one component, as Unison's do: each member is hashed with the slots of
  the cycle blanked, the members are sorted by that, the component is hashed with each in-cycle
  slot as its member's place, and a member's hash is the component's and its index (**read**,
  upstream/unison/hashes.md, for Unison's rule). Members of a fully symmetric cycle tie and are
  ordered as found (**unverified** that this matters).
- **Names are a separate map**, per root, from path to item, like Unison's namespace and den's
  `ns` tree. A rename or a move changes the map and no hash. Diagnostics, backtraces, symbols,
  `type_name` and the agent API read the map.
- **An item is its place in its root; its hash is a key.** This is den's rule (ids in storage,
  hashes derived, `structure.md` section 2), not Unison's (references are hashes). Stored code
  refers to other items by slot, and the root that loads it binds the slots, as each definition
  in Unison's codebase carries a lookup array that a sync rewrites without touching the
  definition (**read**, upstream/unison/repoformat-v2.md).

### What each hash must hold so a hit is never wrong

A hit hands back what was computed for another item with the same key. Content is not identity:
`struct Meters(f64)` and `struct Feet(f64)` have one interface hash, and two equal `static`s
have one deep hash (**read**, the fixture below), as Unison's structural types are one type
unless declared `unique` (**read**). So whatever observes *which* item is bound by the root,
and whatever depends on items the form does not name is recorded and checked.

| what code depends on | where it is |
|---|---|
| name resolution: imports, globs, the prelude, shadowing, hygiene | its result: the slots in the form. Never the paths. A new item that shadows a glob import changes the resolution, so the form |
| method and impl selection in a body | its result, the callee and its generic arguments in MIR: in the form. Which items to look at again when an impl is added is invalidation's job (section 4: who read the impls of that trait for that self type) |
| inference, adjustments, coercions, closure kinds | in MIR: in the form |
| the layout of a type | its interface (fields, `repr`, generics, variants, discriminants) and the target. Target, backend, options and the build of blaze are the store key's prefix as today ([cache.md](cache.md), #326) |
| an instance of a generic body | the generic body's code key, the type arguments' interface hashes, and the answers monomorphizing got: the impl each obligation selected, each associated type and const it normalized, each `needs_drop` and auto-trait answer. They are kept with the entry and checked on a hit: a verifying trace, as Shake keeps (**reported**, den `build.md` section 2) |
| facts no named item holds: a `Drop` impl, `Copy`, `Send`, `Freeze` | the same answers, kept and checked |
| macros | the expansion is in the form. The macro's own hash is an edge for invalidation (expand again when it changes), not part of the key: an edit to a macro that expands the same keeps the code |
| `cfg` | the form is after `cfg`; `cfg!()` is a literal in it; the cfg set and features are in the key's prefix |
| consts | a const's interface holds its body, its value once evaluated; array lengths and const arguments are values in the types |
| `line!()`, `column!()`, `file!()`, panic `Location`, `#[track_caller]` | requests the code generator fills, outside the code's bytes (**read**, today's `DataRequest::Location`). The form holds the request; the root fills in the place |
| `module_path!()`, `type_name`, `TypeId`, symbol names | they observe a path or which item. `module_path!()` expands to a literal today (**read**), so moving a function that logs through `tracing` changes its hash: it becomes a request like `file!()`. `type_name` and `TypeId` become requests the root answers. Symbols are written from the name map at link |
| debug info: line tables, variable names, scopes | outside the form: a side table of the entry, moved on a hit as marks move lines today (**read**, `patch.rs`) |
| inlined code (the tier-up at level 2, `RA_JIT_INLINE` builds) | the deep hashes of the bodies built in |
| what a form could not write (no MIR, a type with an error, an anonymous constant) | the reason is in the dump, and the hash is pinned to the item's file and place in it, so it is never shared with another item and misses on a move (**read**, `of_crates`) |

### Measured

`ra-jit items` on both sides of 15 real commits of nushell, ripgrep and helix, picked by their
messages as renames, moves, rustfmt and refactors, with a fix or two as controls; dev-aws-8,
16 cores pinned, release build of `blaze-next-items` at `062d38fc52` (**read**). An item counts
when its crate has a changed file, or depends on such a crate in the checkout: the crates whose
keys today's store gives new names. "New" is a hash found nowhere among the parent's.

| commit | items counted | new code hashes | interface kept | deep kept | today: names kept | today: store keys kept | edit to result today |
|---|---|---|---|---|---|---|---|
| ripgrep `bb8172f` rustfmt, 40 files | 4,705 | 1 | 100% | 99.8% | 100% | 0% | 0.35-0.52 s |
| ripgrep `861f6d3` inline format args, 5 files | 1,691 | 1 | 100% | 99.4% | 100% | 0% | 1.9-2.1 s (`build.rs` changed: cargo) |
| ripgrep `756a945` `Cargo.toml` only, 10 files | 4,805 | 1 | 100% | 99.8% | 100% | 0% | |
| ripgrep `626b895` refactor `is_hidden` | 2,174 | 1 | 100% | 99.4% | 99.9% | 0% | |
| ripgrep `d47663b` a fix (control) | 2,760 | 2 | 100% | 99.0% | 100% | 0% | 0.18-0.22 s |
| helix `7fccb499d` cargo fmt | 2,902 | 0 | 100% | 100% | 100% | 0% | 0.09 s (0.48 s the first) |
| helix `5f5c5a30e` a dependency moved to the workspace | 5,687 | 0 | 100% | 100% | 100% | 0% | |
| helix `bd1b25b5c` `once_cell` to std, 22 files | 23,482 | 1,325 | 92.0% | 91.6% | 100% | 0% | 9.1-31.9 s (manifests: cargo) |
| helix `e19c0ea02` a `Debug` bound dropped | 5,848 | 1 | 100% | 100% | 100% | 0% | |
| helix `59f323140` `IndexMap` in `KeyTrieNode` | 2,109 | 115 | 92.4% | 91.6% | 98.5% | 0% | |
| nushell `f97cd76f2` std's `fetch_update` to `try_update` | 37,828 | 2 | 100% | 99.5% | 100% | 0% | 20.8-27.4 s (manifest: cargo) |
| nushell `f70ed723c` the parser split into 16 files | 10,543 | 46 | 100% | 96.7% | 98.0% | 0% | 0.88-1.39 s |
| nushell `728b88abf` `merge` moved to another crate | 7,626 | 8 | 99.8% | 99.7% | 99.6% | 0% | 0.45-3.68 s |
| nushell `686de561b` spread checks consolidated | 10,574 | 12 | 100% | 96.9% | 100% | 0% | |
| nushell `2529074fc` a fix (control) | 37,785 | 10 | 100% | 96.6% | 100% | 0% | (the parent's build failed) |

What changed, read item by item (**read**):

- **Pure layout changes keep every hash** but one: rustfmt of 40 files, `cargo fmt`, a `Cargo.toml`
  edit and a dependency moved to the workspace. The one is ripgrep's `generate_digits`, which
  reads the git revision `build.rs` passes, so it is new at every commit: a real input.
- **The parser split** (16 new files, 10,543 items) changed 46 code hashes: 38 call `log::trace!`,
  whose `module_path!()` is a string literal that a move changes (the table above makes it a
  request); 2 call `str::from_utf8`, which the old file resolved to `core::str::from_utf8` through
  `use std::str` and the new ones to `str`'s inherent function, a different item; 6 were edited
  by the commit. None changed for having moved.
- **The renames** of `fetch_update` are of a std function: the 2 callers now call another item.
- **Refactors that change types** (`once_cell` to std, `IndexMap`) change what uses those types,
  and nothing else: 88% and 91% of the code hashes kept.
- **Before normalizing the body's order**, ripgrep's rustfmt changed `hiargs::printer`: `return x`
  became `return x;`, which gives lowering one more local of type `!` and its storage marks. Bodies
  are now written in the order a walk from the start block reaches them, leaving out blocks it
  does not reach and the marks of locals nothing else uses.
- **Today's names already survive most of this inside one daemon**: an item's name is its file and
  its place in the file's table of items, which rustfmt and reorders keep; moves to another file
  lose them (the split: 2.0%). Today's store keys are all new in every crate counted, whatever the
  edit, because they name crates by their sources.
- **Missed forms**: nushell's newest commits have 3,952 items (10%) whose types have an error
  in blaze's front end, the June ones 1,994 without MIR and 1,359 with errors; ripgrep 28,
  helix 18. Their hashes are pinned to their place, so they never share wrongly and miss on a
  move. They are what the front end does not yet check, not the hash.

**Cost** (**read**): hashing is 3.2-4.2 us an item, 15-17 ms for ripgrep's 4,800 items, 80-103 ms
for helix's 25,000 and 120-157 ms for nushell's 34,000-41,000, on one thread: 0.13-0.25% of
computing the forms (8.2-8.5 s, 33-35 s and 79-94 s, the MIR of every item from nothing, on one
thread). On an edit only the edited files' items and the dependents whose interfaces changed are
hashed. **Size**: four 32-byte hashes an item, 5.2 MB for nushell's 40,605; the code hashes
alone, which are the store's keys, 1.3 MB.

The edit-to-result times are today's daemon, warm, with each commit applied and taken back
twice (`harness/commits.py`). Times with item keys wait for step 3 below; the commits whose
manifests or build scripts change go through cargo today and are not this layer's to speed up.

### The store's key, and main's keys

Today's key ([cache.md](cache.md)) names a function of the checkout by a hash of its crate's
sources and those of the crates of the checkout it depends on, its file and its place in the
file, so every edit gives new keys to all of the edited crate and its dependents (**read**).
The warm-store lane on main makes the keys content-only and adds the build of blaze to them
(#326); c18d96d6 ("store reuse") has not landed and was not found on any branch (**read**,
2026-10-25). The item hash goes under the same scheme, not beside it:

- **The key of a function of the checkout** becomes SHA-256 over today's prefix (backend,
  level, target, debug, overflow, blaze's build), the tag `item`, its code hash, and for an
  instance its type arguments' interface hashes. Dependencies keep rustc's stable crate id: they
  never change under a checkout.
- **The entry** keeps its code, relocations and unwind information as now. Its relocations
  name slots, bound at load by the root, instead of stable names; it adds the answers it was
  built with (the trace) and its debug side table.
- **Migration**: none is needed for soundness. An old entry is under a key nobody asks for
  again, and goes by the store's LRU. The entry's layout changes (slots, trace), so the store
  format's version moves and old packs are read as misses. Both kinds of key can live in one
  store, so main's format stays readable while the branch moves.

### What agents get

- **What-if roots and best-of-N candidates** share every item no candidate changed semantically.
  Four agents' renames of one helper are four name maps over one set of entries.
- **A refactor costs nothing to check.** An agent that moves a module or runs rustfmt gets its
  result from the store; the diff says "no item changed", and only the name map is new.
- **Early cutoff is free**: an edit whose item hash is unchanged stops at the diff; one whose
  interface hash is unchanged stops at the item.

### Plan

1. *(done on `blaze-next-items`)* `crates/blaze-items`: forms, the four hashes, cycles;
   `ra-jit items <checkout>`; `harness/items.py` and `harness/commits.py`.
2. Changed items from the VFS diff: a changed file's items are hashed again and compared by
   local hash; the push of section 4 starts from the items whose local hash changed and stops
   where the interface hash did not.
3. Code keys in the daemon: the patch base and marks keyed by code hash, so a rename or a move
   keeps the compiled code in a running daemon.
4. Slots in stored relocations, the name map, `type_name`, `TypeId` and `module_path!` as
   requests, debug info as a side table.
5. Instance keys with their traces; the store's key switched (above).

**Kill criterion.** Hashing has to cost under 5% of what computing the forms costs, and under
1 ms on an edit (it hashes the items of the edited files and the dependents whose interfaces
changed). If a measured refactor commit keeps under 90% of the code hashes of the items it does
not change semantically, the form is wrong somewhere and is fixed before anything keys on it.

## 2. Isolation: ix VM forks and roots

Andrew: "the ix fork ... CoW at VM level, or do we need to do overlays ourselves?" Both exist, at
different costs, and each does a job the other can't:

| | ix VM fork | a new root in one daemon |
|---|---|---|
| what is copied | memory, disk and running processes, copy-on-write (**reported**, ix docs) | nothing: the path from the edited files to the root |
| cost | 1.7 s per restore at best; 4.3 s for an 8 GiB seed, 35.8 s for 27 GiB touched on the older path; 17-42 ms to `exec` into a warm spare (**reported**, [ci-runner.md](ci-runner.md)) | 135 us for a three-file transaction (**read**) |
| limits | machines at once per plan (20 to 100, **reported**); a fork is charged the seed's whole memory allocation today (**reported**) | memory for the memos of what differs |
| where it runs | ix only | anywhere, the Mac included |
| isolates | everything: a test that deletes the disk, a proc macro that escapes its sandbox, a daemon crash | nothing beyond what the sandboxes give (#147, #257) |

**The division of labour.**

- **VM forks** for isolation boundaries and whole jobs: CI (one fork per push, #264), untrusted
  code, and each agent's long-lived workspace when agents run off the Mac. ix provides it; blaze
  builds nothing of it, only the daemon's readiness to be forked (quiesce, then snapshot).
- **New roots** for everything cheap: what-if builds, best-of-N candidates, several agents
  sharing one warm daemon locally, and the per-push checkout inside a fork (the fork's daemon
  applies the push as one transaction, not a disk checkout and a watcher).
- **No `fork(2)` of the daemon.** A multi-threaded process forks only the calling thread, with
  every other thread's locks held forever, so it would need a quiescent single-threaded point;
  and what it would buy is isolation of the compiler, which runs no untrusted code (proc macros
  and build scripts run in their own sandboxes, tests in theirs). Roots already give sharing.
  The `fork(2)` that is worth it is of a loaded test program, one per sandboxed test, estimated
  1-5 ms ([ci-runner.md](ci-runner.md), **unverified**), which is #257's.
- **Not built twice.** blaze does not build VM-level isolation; ix does not build per-root
  speculation. On ix the two compose: forks of one seed share pages by copy-on-write, and inside
  each fork roots share by hash. Kill criterion for roots-in-one-daemon: if N concurrent agent
  heads in one daemon use more memory than N forks would charge, or one head's build slows
  another's edit by more than 10%, agents move to forks.

## 3. A sans-IO core

[#272](https://github.com/indexable-inc/blaze/issues/272). Andrew: "redesign to be sans-IO, so
on Linux you could replace IO with an io_uring backend."

### The boundary

The core is synchronous state machines with no IO, clock or threads of their own:

    trait Core { fn step(&mut self, now: Instant, event: Event, out: &mut Effects); }

- **Events**: a transaction arrived (from the socket, the disk input, git), an IO completed (bytes
  of a range, an append done, an fsync done), a compute job finished, a timer fired, a client
  went away.
- **Effects**: read a range of a pack or a file, append to the VFS log, fsync, write a projected
  file, send a reply, spawn a process (a sandboxed test, a linker while one is needed), start a
  compute job, set a timer. Each carries a tag its completion comes back with.
- **What is core**: the VFS (roots, transactions, diffs), invalidation, the scheduler's decisions
  (what to compute, in which order, what to cancel when a newer root arrives), the store's index
  and GC, the protocol's codec and sessions, the patch planner (what to patch, roll back, or link).
- **What is not**: inference, MIR and code generation are compute, run on a pool and represented
  in the core as jobs whose results are events, so the simulator can reorder their completions.

ix's streams node is the in-house template (**read**, ix `coreio.rs`, `4a647c63ef`): a
`CoreIo` trait with `poll(sink)`, `read(at, tag)`, `submit()`, `park()`, `now_ns()` and
`offload(job)`, io_uring in production, a virtual clock and journal in its simulation, one
pinned event loop per core, message passing only at the edges. An earlier read for Andrew found
it "already mostly sans-IO" with pure `step` cores returning effects (**reported**, a den
session). TigerBeetle's `IO` (a `Completion` per operation, the same interface on Linux, Darwin
and its simulator) and quinn-proto ("contains no networking code and does not get any relevant
timestamps from the operating system") are the outside models (**reported**, fetched for
hyperion's `docs/upstream/sans-io/`).

### Backends

| backend | where | what it does |
|---|---|---|
| simulation | tests, CI, fuzzing | one thread, a seeded scheduler and virtual clock; completions reordered, delayed, failed; torn appends and crashes at any effect; a failure replays from its seed and shrinks to a minimal list of events, as ix's `dst-kit` does |
| threads | macOS default, portable | blocking reads and writes on a small pool, a `poll`-style loop for the sockets |
| io_uring | Linux | registered files for open packs and the VFS log, a registered buffer pool for reads, batched submission per turn, `SINGLE_ISSUER` and `DEFER_TASKRUN`, multishot accept and receive for the socket |
| ix | inside a VM | the Linux backend, plus a quiesce event before a snapshot so a fork starts at a clean turn |

What the simulation tests that nothing tests today: concurrent transactions from several agents
against builds in flight, a crash between a VFS append and its fsync, a client vanishing
mid-reply, a disk write racing an agent's write to the same file, every interleaving of a build
cancelled by a newer root.

### Where IO costs today, and where io_uring pays

| IO | measured | io_uring? |
|---|---|---|
| noticing an edit | 35 ms, most of it the client starting (**read**); one stat of every nushell file 1.8 ms on the Mac (**read**) | no: an agent's write through the socket removes it |
| reading every source file at load | 24.5 ms for nushell's 2,240 files on the Mac, warm cache (**read**) | a few ms saved by batching opens and reads, once per start (**unverified**) |
| "store read" on a warm start | helix 107-138 s of thread time over 64 threads; unpacking zstd blocks 19-44 s of it, the rest decoding and interning; 151k block reads at 32 MiB of cache (**read**, concurrent-reads.md) | no: CPU. At an estimated 5 us a `pread`, 151k reads are under a second of thread time (**unverified**). The image ([section 5](#5-state-as-an-image)) removes the decoding |
| store writes after a build from nothing | helix 21.2 s, longer than the compile (**read**, warm-start.md) | partly: what share is compression was not measured (**unverified**) |
| the socket | one thread per connection, a few requests a second | no |
| a fresh ix fork faulting in its store and image | not measured; a fork restores memory lazily and its first compile pays the faults (**reported**, ci-runner.md) | yes: batched reads of the hot set at queue depth instead of one fault at a time |
| the VFS log | does not exist yet | yes: group commit of transactions, one fsync per turn |

So io_uring is built because the core makes it cheap and because ix forks and CI read cold, and
it is measured against the threads backend on the same Linux host before it is the default
there. It does not speed up anything on today's critical path.

### The concurrency model

Andrew asked whether everything should be thread-per-core with message passing. Not
everything: two planes, each with the model its work wants.

**The control and IO plane** (socket requests, VFS transactions, the store's and the VFS's
persistence, spawning processes): shared-nothing state machines, each owning its state, talking
by messages, with one io_uring ring per IO thread on Linux. This is where sans-IO and determinism
come cheap. How many such threads is decided by load, not fixed at one per core: a transaction is
135 us (**read**), so one thread takes about 7,000 a second, and a few agents make tens
(**unverified** rate). The state is sharded by workspace from the start, so it spreads over cores
when a harness shows one thread saturated (an ix seed serving many forks, CI), and runs as one
shard on a Mac.

**The compute plane** (inference, MIR, codegen, lints, per item and per function): not
shared-nothing. The work is an irregular graph of very uneven tasks over a large database that is
read far more than written, and the measurements say why messages per unit of work would lose:

| | measured | what it means |
|---|---|---|
| a compiled function | nushell's own base: 21.2 s on 16 cores for about 165,752 functions, about 2 ms of thread time each (**read** for the inputs, **unverified** arithmetic) | a message per function (a few hundred ns across cores, **unverified**) is noise |
| a front-end memo | a signature edit walks 30.6M edges in about 245 ms of revalidation, about 8 ns an edge (**read** for the inputs) | a message per query or per memo would be tens of times the work |
| shared reads under a lock | helix's warm start: 571 s of "store read" thread time with the interner taking a shard's write lock to look up; 131 s once lookups took the read lock (**read**, warm-start.md); #244 found an interned value's shard lock the hottest of a signature edit | the cost is writes to shared lines, not sharing: reads must write nothing shared |
| one task holding the others | the first type check verified `TraitImpls::for_crate` crate after crate while other threads waited, 37-75 ms of a 258 ms edit (**read**, lean-frontend.md) | the scheduler must see tasks, not one thread's call stack |

So the compute plane is MVCC-shaped: message passing for mutation, shared immutable data for
reads.

- **A revision is an immutable snapshot.** The root, the item and instance tables and the results
  of the revision before are persistent structures that every worker reads without locks or
  reference-count traffic (ids into arenas, not `Arc`s on the hot path).
- **Work-stealing over per-item tasks.** The push from the diff makes tasks (lower an item, infer a
  body, build MIR, compile an instance, lint); a work-stealing scheduler of blaze's own runs them,
  each task declaring its inputs so the scheduler orders and cancels them (a newer root cancels
  what the older one no longer needs). rayon's pool is the baseline it must beat on the same tasks.
- **Results go to per-worker append-only buffers** and are published by message to one committer,
  which makes the next revision. Nothing writes shared state in place.
- **Interning** goes to per-worker arenas whose ids are globally stable (a worker's prefix and an
  index), with a sharded table that is read without writing for lookups of values that already
  exist, as the interner's fix and #244 did by hand.
- **A deterministic mode.** The same tasks run on a single-threaded seeded scheduler with the
  simulation backend; its results (code, diagnostics, test outcomes, hashed) must equal the
  parallel run's, as a harness step. Laying out functions in completion order (#200) is the kind of
  nondeterminism this makes a test failure.
- **Numbers to gather before the boundary is fixed** (a dev host, perf on nushell's edits): lock
  contention and queue time today, the cost of a cross-core message on the hosts, and the
  distribution of task sizes, so tasks smaller than a message stay inside their parent.

## 4. Push invalidation, per function

[#273](https://github.com/indexable-inc/blaze/issues/273). Today salsa pulls: at a new revision
every memo asked for is verified by walking its recorded inputs. On nushell revalidation is on the
stack in 38% of a body edit and 47% of a signature edit, and a signature edit walks 30.6M edges,
27.6M of them to memos already verified in that revision (**read**, lean-frontend.md).

- **From the diff, forward.** Changed files give changed items: each item (function, struct, impl,
  `use`, macro call) has a hash of its own text and of what it resolves to, as `outline.rs` and
  `Body::relative` already compute for body-only edits. Changed items give their dependents
  through a reverse index recorded when they were computed. Only those are marked.
- **Early cutoff.** A recomputed item whose result hash is the same stops the push: a body edit
  stops at its function's signature (which did not change); a signature edit stops at each caller
  whose inference result is the same; an instance stops when its MIR hash is the same, and a
  function when its code is the same.
- **Per function for signature edits.** The callers of a changed signature are found from the
  reverse index (who resolved a path or method to it), not from "every instance whose crate
  depends on nu-protocol" (today's 106,256). Each is re-inferred; only callers whose result changed
  continue.
- **The graph.** Item and instance ids are u32, edges in compressed sparse rows with a small
  per-revision delta, kept in the image so a restart has it. Salsa's records are 1.3 GB on nushell
  (**read**); 64M edges in CSR at one u32 each is about 256 MB, and at item granularity rather than
  query granularity there are far fewer edges (**unverified**).
- **Migration.** Salsa stays the front end's engine at first; the push layer decides which salsa
  inputs to set and which outputs to ask for, so salsa verifies only what the push reached. Then
  the codegen-side queries (`compiled_instance`, marks, places) move to the push graph; then the
  front end's per-item queries. This is the makepad Relax bet, done step by step with the
  differential harnesses green at each.

## 5. State as an image

[#274](https://github.com/indexable-inc/blaze/issues/274). A warm restart with everything in the
store is 35.4 s on nushell, and helix's "store read" is 129-131 s of thread time, mostly decoding
and interning (**read**). The fix is not to read faster but to not decode.

- The daemon's state for its roots (the Merkle VFS's node arena, the dependency graph, item and
  instance tables, the compiled-code index, the patch base's marks named by stable names, not salsa
  ids) is laid out in position-independent, versioned sections that are mapped and used in place.
- A restart maps the image, checks its root against the head (a diff, microseconds), and is ready;
  an edit since is the diff. A new worktree maps the same image and diffs its root. An ix fork
  already has the pages; the image makes a process restart inside it as cheap.
- Interned types become ids in the image's tables, so nothing is interned again at start.
- Kill criterion: the image must make nushell's warm restart under 1 s and the first edit after it
  as fast as any other; if mapping and validating costs more than rebuilding the part it covers,
  that part stays rebuilt.

## 6. Own the binary: patch in place

[#275](https://github.com/indexable-inc/blaze/issues/275). The splice already never links in
full for a reason of size ([architecture.md](architecture.md)); a body edit still spends 28 ms in
refuse and link, 14 ms of it the link (**read**).

- **On the edit path, nothing is linked.** The patch's code is placed and its references resolved
  by the patch planner (sans-IO), written into the running program by ra-jit-live where one runs,
  and appended to the executable behind the reply, so the reply does not wait for the file.
- **A patchable table**, as Relax keeps one: each function that has been patched is reached through
  a slot; the entry jump the splice writes today is one such slot. Generations of patches are kept,
  so a rollback restores the slots of a generation (#232).
- **Own layouts and live field edits** need blaze's own layouts for every crate, which is the
  no-rustc lane (#87, #85); designed in, built there. What `blaze-next` builds is the patch path
  that does not need it.

## 7. Speculation that fits agents

[#276](https://github.com/indexable-inc/blaze/issues/276).

- **What-if on a root.** An agent proposes a patch; blaze builds, checks or tests it on a new root
  without moving the head or writing to disk, and answers with the same structured result. Taking
  it is moving the head to that root (no second build).
- **Best-of-N.** N candidate roots at once share every unchanged node and memo; the scheduler runs
  their distinct work in parallel and cancels a candidate the agent dropped.
- **Patch before the checks finish.** Code goes to the program when it exists; the borrow check and
  diagnostics run beside it (on nushell the borrow check of `main` is about 100 ms and outlasts
  codegen by 35-76 ms, **read**). If they find an error, the patch is rolled back and the result
  says so. A running program never keeps code its source would not compile to.
- Keystroke-level speculation (compile likely states ahead of an editor) is not built: agents
  have no keystrokes.

## 8. The agent API

[#277](https://github.com/indexable-inc/blaze/issues/277). Andrew: "don't use MCP; it should be
like a Unix socket or HTTP."

- **Transport.** A Unix domain socket per user, permissioned by the socket file as today
  (`socket.rs`), and HTTP for remote use and ix forks. One documented, versioned schema.
- **Encoding: deterministic CBOR** (Andrew's decision), RFC 8949 section 4.2's core
  deterministic encoding: integers and lengths in their shortest form, definite lengths only,
  map keys sorted bytewise by their encoding, no floats where an integer will do (durations in
  nanoseconds) and the shortest float otherwise. The same bytes on the socket and over HTTP
  (`application/cbor`); JSON is only a rendering for people (`Accept: application/json`, the
  CLI's `--json`), never the wire. Why:
  - one value has one encoding, so a request, a transaction, a set of diagnostics or a test
    result can be hashed and kept in the content-addressed store under that hash, beside roots
    and compiled functions, and two daemons agree on it byte for byte;
  - byte strings carry hashes and file contents as they are, without base64;
  - it is compact and fast to parse.
- **The implementation.** minicbor, with structs encoded as maps keyed by small unsigned
  integers declared in ascending order: its encoder always writes integers and lengths in the
  shortest form (**read**, `minicbor` 2.3.0 `encoder.rs`), but it encodes a `HashMap` in
  iteration order and a `BTreeMap` in Rust's order, which for text keys is not RFC 8949's
  bytewise order of the encoded keys ("b" sorts before "aa" there), and writes every `f64` in
  eight bytes (**read**, same source). So the schema has no maps keyed by strings or hash
  iteration and no floats, and a small validator of blaze's own checks every message against
  section 4.2 (shortest heads, definite lengths, strictly increasing map keys, no
  non-shortest float). The daemon refuses a message that fails it, naming the first offence,
  rather than canonicalizing it, so a client that encodes wrong finds out. Tests: every
  message type round-trips decode to encode byte-identically, and the validator accepts
  everything the encoder writes, on random values. ciborium (serde) was not chosen: serde's
  maps and floats give no such control (**unverified** beyond the above); an encoder of our
  own stays possible behind the same validator.
- **Framing on the socket.** Each message is a length (four bytes, big-endian, at most 16 MiB
  as `socket::LONGEST_REQUEST` today) and then one CBOR item, a map of a request id, the schema
  version and the body. Request ids let a client pipeline several requests on one connection;
  replies and progress events (a test finished, a build stage done) carry the id they answer
  and may come in any order.
- **Calls.** `apply(base, ops) -> root` (an atomic multi-file transaction), `head(workspace)`,
  `check(root)`, `build(root)`, `test(root, filter)`, `run(root, args)`, `what_if(base, ops,
  then)` (apply on a root that is not the head, then check, build or test it), `drop(root)`,
  and queries: diagnostics, tests and why each ran or was skipped, callers of an item.
- **Results.** Diagnostics with rustc's fields (#222 writes them as rustc's JSON for cargo; the
  API carries the same fields in CBOR) with machine-applicable
  fixes as `ops` an agent can pass to `apply`; tests with outcome, duration, the reason a test was
  skipped (unchanged reach, #227) and output only for failures; run results with exit status and a
  bounded tail; timings per stage.
- **Clients.** `cargo blaze` becomes a thin client of the same socket. den wraps the API as hub
  tools on its side (`blaze_apply`, `blaze_check`, `blaze_test`, `blaze_what_if`), the way its
  `lsp_*` tools wrap language servers. No MCP server anywhere.
- **The number that counts** is edit tool call to structured result, measured end to end by a
  harness that drives the API as an agent would (a transaction, then a check or a test), including
  the client's start where a CLI is used. The watcher's latency stops mattering once agents write
  through the API.

## 9. Lints

[#278](https://github.com/indexable-inc/blaze/issues/278), P3. Andrew: "something LIKE clippy in
this too."

- **clippy can't be run**: it is a rustc driver, and blaze never runs rustc. `cargo ra-jit clippy`
  passes through to cargo today ([no-rustc.md](no-rustc.md)), which is one more such path.
- **Lints run in blaze's pipeline**, on its HIR, types and MIR, as part of the check, per function:
  an edit re-lints only the items the push reached, and the result is memoized like any other.
- **What to port first.** clippy at `43c0efca14` has 840 lints, 498 of them on by default:
  correctness 67, suspicious 86, style 159, complexity 146, perf 40 (**read**, counted from
  `declare_clippy_lint!`). Correctness and suspicious first (153 lints, the ones that find bugs),
  then the rest of the default set. 318 lint passes work on HIR after type checking, 53 on the
  syntax tree, and 20 lint files use MIR (**read**): almost all of it maps onto rust-analyzer's
  HIR and types, which already carry 80 diagnostic handlers in `ide-diagnostics` (**read**).
- **Parity**: a differential harness runs clippy and blaze on real crates (the bench set, then
  crates.io's top crates) and compares each lint's spans and messages, as `cargo ra-jit check` was
  compared field by field with rustc (#222). Lint config is honored as clippy's: `clippy.toml`,
  `#[allow]`/`#[warn]`/`#[deny]`/`#[expect]` of `clippy::...` and its groups, `-A/-W/-D` on the
  command line, `[lints.clippy]` in `Cargo.toml`.
- **Fixes**: 354 of clippy's 748 lint files offer a machine-applicable fix (**read**); blaze's are
  edit transactions, so an agent can apply them or try them as a what-if root.
- **Project lints**: a Rust lint API over the same IR, compiled by blaze and run in the proc-macro
  sandbox (#147), loaded per workspace. Recommended over a new query language (one more language to
  learn and to keep complete) and over Tern (a configuration language, not one for walking typed
  IR); a declarative layer can come later on top of the API if lints turn out to be mostly patterns.
- **Agent lints**: patterns coding agents get wrong (an `unwrap` in library code just written,
  `clone()` to quiet the borrow checker, a test that asserts nothing, `todo!()` left behind),
  opt-in by group, gathered from what agents' transactions are later rewritten to.
- In `blaze-next`: designed and stubbed (the per-function lint hook and the result shape); the
  port is its own lane, as it would otherwise hold the branch for months.

## 10. Efficiency: layout and budgets

- **Ids, not pointers.** Paths, names, items, instances, types and nodes are u32 ids into arenas;
  maps are keyed by id; nothing on the hot path hashes a string.
- **Nothing per file after lowering.** Syntax trees go once an item is lowered (the parse lru and
  [flat-syntax.md](flat-syntax.md) do most of it: 2.9% of the heap, **read**); file text lives in
  the store's mapped packs, once for every root.
- **The image is the memory.** What a restart needs is mapped, so resident memory is what the
  current roots touch.
- **Budgets, nushell** (worker after the base build: 5.8 GB sampled heap, 7.5 GB resident,
  **read**): salsa's records 1.3 GB to under 0.3 GB through the item-level graph; "ra-jit's own"
  988 MB measured apart and halved (**unverified** what it holds); a second agent's head under
  10% of the first's. Targets, **unverified**, each checked by `heap.py` as the workstream lands.

## 11. Delivery and order

Andrew chose one pull request: an integration branch, `blaze-next`, with one draft pull request
opened early and kept up to date, its body a checklist against this document with the current
numbers. Workstreams are short branches merged into `blaze-next` with `--no-ff`, never into main.
Main is merged into `blaze-next` at least daily and after each large lane lands (the rename, the
warm-store lane's content-only keys, #236's slices, #257/#263, #258). It lands through `land.py`
in a quiet window, once the full gate and every differential harness pass on it, with the new
path the default and the old one deletable or behind a switch.

The order inside the branch, by what the floor needs and what depends on what:

| # | workstream | needs | kill criterion or measurement |
|---|---|---|---|
| 1 | the sans-IO core crate, the simulation and threads backends, and the Merkle VFS on it (roots, transactions, diffs, persistence) | nothing | simulation finds no lost or half-applied transaction over 10^6 seeds; a transaction under 1 ms on nushell |
| 2 | the daemon takes transactions (socket and CLI), disk watching becomes one input producing them, builds pin roots | 1 | every edit harness passes with the watcher as the only input; notice to apply under 1 ms on the socket path |
| 2b | items by content: forms and the four hashes per item, changed items from the diff, code keys in the daemon, then slots, the name map and the store's key (section 1b) | 1 | hashing under 5% of the forms and under 1 ms an edit; a refactor commit keeps over 90% of the code hashes of what it does not change |
| 3 | invalidation driven by root diffs: changed items from the diff, the push layer in front of salsa | 2, 2b | a body edit's revalidation share from 38% to under 5%; a signature edit's instances asked from 106k to its callers |
| 3b | the compute plane: revisions as immutable snapshots, a work-stealing scheduler over per-item tasks with one committer, per-worker interning, the deterministic scheduler | 3 | beats rayon on the same tasks; the deterministic run's hashes equal the parallel run's; lock and queue time on an edit under 5% |
| 4 | the agent API: check, build, test, run and what-if on a root, structured results; `cargo blaze` a client | 2 | edit-to-result end to end on nushell; best-of-4 candidates in under twice one |
| 5 | diagnostics off the critical path with rollback; the patch path without a link on the reply | 3 | build-rest from 35-76 ms to 0; refuse-and-link from 28 ms to under 3 |
| 6 | the image: graph, VFS arena, code index, marks by stable name | 3 | nushell warm restart under 1 s; no rebuild before the first edit |
| 7 | the io_uring backend | 1 | faster than threads on a cold fork's start on the same host, or not the default |
| 8 | lints: the per-function hook and result shape (port as its own lane) | 3, 4 | stubbed in the branch |

Every step keeps the differential harnesses green (rustc is the reference, every program rustc
builds still builds and behaves the same). Numbers are taken on a dev host when one is reachable
and on the Mac for what fits; the io_uring backend needs a Linux host.

### How it meets the open lanes

- **#236, #253 (lean frontend)**: their slices are what workstream 3 builds on; merged in as they
  land. The push layer makes slice 2's firewall per instance general.
- **The warm-store lane (content-only keys, worktree sharing, #105)**: the VFS's nodes go under the
  same keys; workstream 1 uses its store API rather than a second one.
- **#257/#263 (sandboxed tests), #258 (no fallback for tests)**: the agent API's `test` reports
  what they decide; no change to them.
- **#264 (the CI runner)**: a push becomes one transaction in the fork's daemon instead of a
  checkout and a watcher pass.
- **The rename**: new crates are named `blaze-*` from the start.

## Not checked

- Every floor in the tables is a target, not a measurement, except the VFS's write, hash and diff
  costs and the IO timings, measured on a Mac prototype that is not the implementation.
- The Merkle prototype was run on one repository (nushell) on one machine.
- ix's numbers are its own documents', not re-measured; whether a fork's still-shared memory is
  charged per fork is not stated by ix.
- No agent harness exists yet, so "edit tool call to structured result" has no baseline beyond the
  daemon's own stage timeline.
- The item hashes were replayed on 15 commits of three projects, picked by their messages, not
  sampled; whether an equal code hash always means code that may be reused is argued (section
  1b's table), not tested against rustc: no harness yet builds with item keys and compares.
