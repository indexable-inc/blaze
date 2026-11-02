---
status: plan
summary: blaze v2 vfs lane's design: the Merkle tree, heads and views, rebase, the journal with group commit, recovery and compaction, and the disk watcher as one input of transactions.
date: 2026-10-29
---
# blaze v2: the vfs lane's design

How [vfs.md](v2/plan/lanes/vfs.md) is built in `crates/blaze-vfs`, as of 2026-10-29. The tree, rebase and
journal are blaze-next's `blaze-vfs` (`git show origin/blaze-next:crates/blaze-vfs`) behind the
v2 interface; what changed from it is below.

## The tree

- A node is interned by its [`ContentId`](../crates/blaze-ids/src/lib.rs): a file's under
  `"blaze v2 vfs file 1"` (the hash of its bytes), a directory's under
  `"blaze v2 vfs directory 2"` (each entry's git mode code, length-prefixed name and child ID,
  sorted by name). A dense `NodeId` never leaves the crate: every public type names roots and
  files by content ID, so collection can renumber freely.
- **Modes** are git's: a directory entry carries `Mode` (regular 100644, executable 100755,
  symlink 120000; a directory is 040000), and the code is hashed into the directory, as git
  does. A file's own ID is its bytes alone, so a chmod is a new root and a `Modified` diff, and
  equal bytes in two modes are one node. `Write` sets the mode; `Edit` and `Rename` keep it. A
  symlink's bytes are its target: `read` gives the link itself, `follow` reads through every
  link on the path, a relative target taken from the link's directory. A target that is
  absolute, empty, climbs above the root, or makes more than 40 hops is `OpError::BadLink`;
  the tree never reaches outside itself.
- **Path order** is by component, as the tree is walked: `a/x` comes before `a.b`. A diff where
  a file and a directory swap gives the file's path first, then the paths under it.
- `lookup` and `list` take `""` for the root.

## Heads and views

- A head is a named root: a workspace, `branch/<name>`, `pr/<number>`. `commit` moves one by a
  transaction (rebased file by file when its base is stale); `set_head` moves one to any root
  the tree has (a checkout, a fork, a push); `remove_head` drops one (a deleted branch, a closed
  PR). Each is a journal record with a sequence number.
- A view is an open root: `open` is constant time (a slot in an arena) and keeps what the root
  reaches alive until `close`. A closed view's number is never given out again, so a stale
  `ViewId` panics instead of reading another view. A what-if is `apply` and then `open`.
- A CI job's checkout is `apply(empty, writes)` of the commit's files (each with git's mode),
  opened as a view or set as a head. `empty` is the empty directory's root, always known.
- The view arena is the one every crate uses: a `ViewId` is what `Vfs::open` gave.
- **Readers on other threads**: `Vfs::reader` is a cloneable `Send` and `Sync` handle that
  the query database reads as its `blaze_query::Tree` (the impl is in blaze-query, which
  depends on blaze-vfs). It follows docs/v2/plan/design/concurrency.md:
  - The tree and every reader share one store: append-only columns in chunks that never move
    (each node's ID and an 8-byte shape; each directory's entries as a `u32` name, a `u32`
    node and a mode byte; each file's bytes as one `Bytes`, the very buffer an owned write
    gave) and an open-addressed index
    keyed by the content ID's bits, grown by adding a level.
  - The `Vfs` is the only writer. It writes each node, children first, and releases it
    before anything names it, so a node is readable before the call that gives its ID
    returns. A read is in a task (`blaze_reclaim::enter`) and is acquire loads: an index
    probe for the root and a binary search of a name column per component, no lock.
    `file` returns the bytes' `Arc` with one count; `pin()` gives a `Pinned` that lends
    them with none.
  - A collection builds the next table and swaps it in with one release store; the old one
    is freed once every task that could read it has ended (blaze-reclaim), or with the last
    handle. A node an open view, head or `keep` reaches is kept.

## The journal

- Records: file, directory, head move, head removal, and sequence (below), each framed with
  magic `BZJ4`, a length, a kind and an 8-byte checksum under `"blaze v2 vfs journal record 2"`.
  A file record's checksum is over the kind and the file's content ID, not its bytes: writing
  one hashes nothing (the tree has the ID) and loading hashes the bytes once, for the ID it
  interns them by. A directory record gives each entry's git mode code (4 bytes) before its
  child's ID. A file of an older magic is read as torn at its first record, as with `BZJ3`.
- One effect in flight at a time (append, then sync; a cut; a rewrite), with commits made in
  between grouped into the next append, as in blaze-next.
- **Compaction**: `collect` drops unreached nodes from memory and, once as many bytes were
  appended since the last rewrite as that rewrite held (or a node of the file's base image was
  dropped), asks the next flush to replace the whole file in one `Effect::Write` with the nodes
  the heads reach and one record per head. A daemon collects after every job, so this keeps
  the rewrites' bytes within twice the appends' and the file within about twice what the
  heads reach; records of dropped nodes wait in the file until then. That write is atomic, so a crash
  leaves the old file or the new. Nothing is appended while it is in flight. A failed one, or
  a failed append or sync, is made again as a rewrite from the state then, which holds every
  commit recorded since. The rewrite ends with a sequence record, so the numbers of removals
  it drops are not given out again after recovery.
- Recovery loads a prefix: it stops at the first record that is not whole, fails its checksum
  or is malformed, and never looks for records past it, since a torn record's payload can be
  another journal whose records verify. It cuts the file there before the first append, so a
  record past the damage cannot load later once the node it named is made again. Malformed
  covers a sequence number at or above 2^63, which no journal reaches, so the next one never
  overflows, and a directory record whose names are not distinct single components in tree
  order. A head whose root is not loaded (only a forged file has one) is dropped, falling back
  to its latest earlier move, and the file is rewritten without it.

## The disk as input

[`Watcher`](../crates/blaze-vfs/src/watch.rs) is its own state machine with its own tags:

- Each `Done::Changed` is a batch. Every path in it is stat-ed and read at once (one round
  trip); the read counts only if the stat says a file. Batches commit in the order they were
  told, so a later read wins.
- Ops are made against the head as it is at commit time: a delete for a path gone, a write for
  a file whose bytes differ (bytes the head has already make nothing, so a file blaze wrote
  is not taken back), a delete of a file where a directory now is, and of any file above a
  file now there.
- A path whose head changed after the report, by anyone but this watcher, is not taken:
  `Watched::Conflict` names it and the head keeps its version.
- A watch starts with a scan: `Effect::List`, asked right after `Effect::Watch` so no change
  falls between them. Every file listed is read, and every file the head has that the listing
  doesn't is deleted, as one batch through the same conflict check; `Watched::Scanned` follows
  its commit. The scan's place in line is taken when the listing is asked, so a change told
  after it commits after it (a read of a file deleted since reads as empty, and the deletion's
  report then wins). A directory that can't be listed stops the watch, naming `""`.
- `lost` (the system dropped changes) is `Watched::Lost` and a rescan; a loss while a listing
  is in flight asks for one more after it. Every file is read on each scan: the drivers give
  no content hash, and `modified_ns` is not a safe skip (0 in the simulation, coarse on some
  file systems).
- Modes: a path stamped a symbolic link is read with `ReadLink`, never followed, and committed
  as a symlink whose bytes are its target, as git keeps one, whatever it points at (a
  directory, outside the tree, nothing); a link replaced by a file is regular. Otherwise a
  file the head has keeps its mode, a new one is regular, and a chmod alone is not seen
  (requested from core: v2/requests/vfs.md item 6). A path's mode counts in the conflict check.
  (Updated 2026-10-31.)
- Projection (writing a head's changes back to its directory) is not in this lane yet. Until it
  is, an agent's transaction and an editor's save of the same file race, and the disk's read
  wins unless the agent's commit lands after the report.
