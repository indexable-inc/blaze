---
status: plan
summary: lane vfs's speed of light inside the VFS (hashing a tree, commit and the journal, reads, diff, the watcher, collection): each component's work counted, its bound and the arithmetic, what the code does, and how it reaches 90% of the bound.
date: 2026-10-30
---
# Lane vfs: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). This is `crates/blaze-vfs/` inside the VFS: the
tree, the store, the journal, the watcher. What is outside every view (`Cover`, `Dirs`, `want`,
`Outside::flush`) is in [speed-of-light-outside.md](speed-of-light-outside.md), and is not
repeated here. The runtime paths' costs and benchmarks against git, Watchman and Unison are in
[speed-runtime.md](speed-runtime.md); this doc counts against the hardware instead.

## The machine

The target machines are in [hardware.md](../../design/hardware.md): EPYC Turin, Graviton 5 and
Apple M-series. Unless a section says otherwise, the figures are these, not yet measured:

| what | figure |
|---|---|
| clock | 3.5 GHz |
| L1 read, streaming | 64 B a cycle |
| L2 read, streaming | 32 B a cycle (about 110 GB/s) |
| one core from DRAM, streaming | about 25 GB/s |
| a socket from DRAM | about 400 GB/s (Turin, 12 channels of DDR5 at 70% of peak) |
| cores the daemon may use | 16 (a dev machine), 64 (a CI machine) |
| page-cache copy (`read`, `write`, `copy_file_range`) | about 10 GB/s a core |
| NVMe write | about 7 GB/s |
| system call on an open descriptor | 0.15 µs |
| system call that resolves a path | 0.4 µs |
| `mmap` plus first touch of a 4 KiB page | 0.25 µs a page |
| `mprotect` | 0.5 µs, plus a TLB shootdown of about 2 µs on each other core running the process |
| IPC | about 2 instructions a cycle for branchy code, about 4 for straight-line integer code |
| SHA-256 with SHA-NI or ARMv8 crypto | about 2 GB/s a core |
| BLAKE3 with AVX-512 | about 6 GB/s a core |

A cold cache line from DRAM is about 80 ns; one in L2 about 4 ns.

The reference workloads:
- **A checkout**: 100 000 files, 2 GB, about 10 000 directories (git's fan-out on a large
  repository), written as one `apply(empty, writes)` or one commit.
- **An edit**: one 10 KB file four directories deep, each directory about 32 entries.

## Hashing a tree: `tree.rs` (`hash_files`, `Tree::apply_all`, `rebuild`)

**The work.** Every byte of every new file goes through BLAKE3 once, for its
`"blaze v2 vfs file 1"` ID. Each file is one Merkle node: one ID, one 8-byte shape, one
`Bytes` (40 bytes) and one index slot. Each directory on a changed path is one more node, whose
hashed bytes are about 52 bytes an entry (mode code 4, name length 4, name about 12, child ID
32).

**The bound.** For the checkout:
- Hashing: 2 GB / (6 GB/s × 16 cores) = **21 ms** on a dev machine. On 64 cores it is
  5.2 ms, which is then DRAM's: 2 GB / 400 GB/s = 5 ms.
- Directories: 110 000 entries × 52 B = 5.7 MB, about 1 ms on one core.
- Nodes: 110 000 nodes, each one cold line for its index slot plus its column writes. That is
  about 110 000 × 80 ns = 9 ms if serial, and it overlaps the hashing only if it is spread out.

So the bound is about 21 ms on 16 cores, with the node writes in its shadow once they run beside
the hashing, and one node written per file plus one per directory.

**What the code did.** One core hashed every file, through three copies and two BLAKE3 passes
per byte:
- the watcher's read `Vec`;
- the op's `Vec`;
- an `Arc<[u8]>` copy in `stage`;
- the journal's batch, which hashed it again for the record checksum.

That is 2 GB / 6 GB/s × 2 = 667 ms of hashing plus about 3 × 2 GB / 10 GB/s = 600 ms of copies,
about **1.27 s**, 60 times the bound.

**What it does now.**
- An owned op's bytes become the file's own (`store::adopt`: the `Vec` moved into the store's
  `Bytes`, its buffer kept). `Vfs::commit_owned`, `Vfs::apply_owned` and the watcher's commit
  copy nothing. A borrowed `Vfs::commit` copies once, as before.
- `Tree::apply_all` hashes every new file of a commit at once, before the merge: one
  `ContentId::of_many_into` call (a SIMD lane a file), and from 1 MiB of new bytes on the
  `Jobs` the daemon gives (`Vfs::hash_on`), in runs of about 256 KiB. `rebuild` then interns
  each file by the ID already made (`Tree::file_with`). The IDs are the same either way.
- The journal no longer hashes the file again (below).

That is one BLAKE3 pass, on every core: 21 ms plus the hand-off of about 8 000 runs (a few
µs each on a pool), so **within 10%** once the daemon calls `hash_on`. Nodes stay one per file
plus one per changed directory, each directory built once per commit.

**What stands in the way.**
- The daemon has to call `vfs.hash_on(image::on_pool)` after `Vfs::restore`, and `mounts.rs`
  has to use `apply_owned` (queue 00877). Until then hashing is on one core: 333 ms.
- A single file larger than all the others hashes on one core. A 2 GB file is 333 ms against
  21 ms. BLAKE3's tree mode splits a file into 1 KiB chunks hashed independently. The design
  is a `ContentId::of_parallel` in `blaze-ids` (lane ids') that hashes subtrees of the chunk
  tree on `Jobs` and joins them, giving the same ID. It is not filed yet: no workload has a
  file over 64 MiB on a hot path.
- The interning (index probe, column writes) is serial in `rebuild`: about 9 ms beside a
  21 ms hash, so within the bound while hashing runs parallel. It would dominate on 64 cores
  (9 ms against 5 ms). There the design is to probe the index for every new ID in the hashing
  jobs, prefetching its slot, so `rebuild` only writes.

**What `proof` should measure.**
- The checkout's wall time on 16 and 64 cores against 2 GB / (6 GB/s × cores).
- BLAKE3 bytes per file byte, which should be 1.0.
- Bytes copied per file byte between the read and the store, which should be 0 for an owned
  commit.
- Nodes written per file.

## Commit and the journal: `journal.rs`, `lib.rs` (`commit`, `commit_owned`)

**The work.** A commit writes, durably, the bytes of the nodes it made: its new files and the
directories on their paths, and one head record. Group commit lets many commits share one
durable write.

**The bound.**
- The checkout: 2 GB / 7 GB/s = **286 ms** of NVMe, one durable write.
- The edit: 10 KB plus 4 directories × 32 entries × 52 B = 17 KB, which is 2.4 µs of NVMe. It
  is dominated by one flush to stable storage, about 20 µs on a drive with power-loss
  protection.
- One durable write per batch of commits, with no hashing beyond the IDs the tree has.

**What the code did.**
- Each file record's bytes were copied into the batch and hashed again for the checksum: 2 GB
  at 6 GB/s, 333 ms on one core.
- Each batch was an `Effect::Append` and then an `Effect::Sync`: two round trips and one
  `fsync`.
- Every collection asked the next flush to rewrite the whole file as what the heads reach. The
  daemon collects after every job (`serve/daemon.rs` `between()`), so every job rewrote the
  whole live state: 2 GB a job after a checkout.

**What it does now.**
- A file record's checksum is over its content ID, `checksum(FILE, id)`, not its bytes. The
  tree has the ID, so writing a record hashes nothing, and loading hashes the bytes once, for
  the ID it interns them by. The ID commits to every byte as a checksum over them would. The
  format is `BZJ4` under `"blaze v2 vfs journal record 2"`. A `BZJ3` file reads as torn at its
  first record, as the bump from `BZJ2` did.
- Compaction is amortized. After a collection the file is rewritten only once as many bytes
  were appended since the last rewrite as that rewrite held, or at once when a node of the
  file's base image was dropped. A rewrite of R bytes then follows at least R appended bytes
  and holds at most R + the appends, so the rewrites' bytes are at most twice the appends'.
  The file stays within about twice what the heads reach, plus one batch. Correctness holds
  as before: records of dropped nodes still make their nodes, and a head takes its latest
  record.
- Bytes written per checkout: 2 GB once, plus one copy into the batch (2 GB at 10 GB/s, 200 ms
  on one core).

**The gap.**
- The copy into the batch: 286 ms of NVMe plus 200 ms of copy is 70% over.
- The second round trip per batch: at an edit's 20 µs flush that is a whole extra effect and
  completion, about 5 µs on io_uring, so 25% over.

Both need effects in `blaze-core` (the coordinator's), filed as queue 00876:
- `Effect::AppendParts { file, parts: Vec<Bytes>, sync: bool, tag }`: a vectored append of the
  file records' own `Bytes` with the framing between them, so the journal copies nothing
  (`pwritev`, or io_uring's write with fixed buffers);
- with `sync: true`, one durable write per batch (`RWF_DSYNC`, or a write linked to an `fsync`
  in one submission), completing once.

With them a checkout is 286 ms of NVMe and an edit one flush: **within 10%**.

**What `proof` should measure.**
- Journal bytes written per commit, against the changed nodes' bytes; the ratio should be
  1.0, plus framing.
- Durable writes (`fsync` or `RWF_DSYNC`) and effect round trips per batch, which should be 1
  and 1.
- BLAKE3 bytes per journal byte written, which should be 0 for file records.
- Rewrite bytes over append bytes across a run that collects after every job, which should be
  at most 2.

## Reads: `reader.rs` (`Vfs::reader`, `Reader`, `Pinned`)

**The work.** A file read is a path lookup (one directory per component), then lending the
file's bytes.

**The bound.**
- Zero copies: the bytes are the store's, mapped from the image or in its heap.
- A lookup of a few cache lines: one for the root's index slot, and one or two a component
  for a search over its directory's names that touches only a compact column.

For the edit's 4-deep path that is about 6 to 10 lines, under 1 µs cold and about 40 ns warm.

**What the code does.**
- Zero copies. `Pinned` lends the bytes with no count. `Reader::file` takes one `Arc` count:
  one atomic increment, uncontended unless many threads read the same file.
- The lookup binary-searches each directory's `u32` name column. Each compare reads the name's
  span (`base_spans` or the span column) and then its bytes, so about 2 cold lines a compare.
  A 32-entry directory takes 5 compares, so about 10 lines a component and 40 for the edit's
  path, plus the root's slot.

That is about 4 times the bound's lines cold, and the same warm, since the lines stay in L2.

**The gap, and how to reach it.** Keep a `u64` name-prefix column beside each directory's
names: the name's first 8 bytes, big-endian, so integer order is byte order on the prefix.
Search on it, reading the name's bytes only for equal prefixes (rare: most names differ in
their first 8 bytes). A component is then log2(n) compares in 1 or 2 lines of the prefix
column plus one line to confirm the name: about 3 lines a component, 13 for the path, within
the bound's range.

It changes the store's entry columns and the image's tree section (`TREE_SCHEMA` `"vfs tree
1"` to 2, one more column), so it is a change of its own with a differential test of lookups
against the binary search over names. It is not in this change. Warm reads, the common case for
a build's repeated lookups, are already near the bound.

**Listings.** Module resolution lists directories constantly, so a listing has the same bound
as a read: no allocation, the names lent from the store's name table.
- The database's listings already meet it. blaze-query's `Tree` impl for `Reader` (`tree.rs`)
  lists through `Pinned::list`, which lends `&str` names under the pin.
- `Reader::list` builds owned names: one `Vec` a listing and one `String` a name. Its one
  caller is cold: blaze-cargo's `Mounted::walk` (`serve/unit/files.rs`), a walk of a mount's
  files. It also took a pin per directory and per file. `Pinned::held` (new) reads a file's
  bytes past the pin with one count, so the walk can run under one pin with lent names.
  That edit is lane cargo's (queue 00944); once it lands, `Reader::list` has no caller and
  goes.
- load-cargo's `Mounts::node` copied a whole directory's names (`Tree::list`) only to learn
  that the path is a directory. `Tree::read_dir`, which lends the names, answers that with
  none (queue 00945).
- Tests: `every_directory_lists_the_same_lent_owned_and_through_the_vfs` lists every
  directory of 8 random trees (names that sort around `/`) through `Pinned::list`,
  `Reader::list` and `Vfs::list`. They agree, and a file, a path under a file and a missing
  path are none through each. `a_pinned_listing_allocates_nothing` lists every directory of a
  200-file tree, under one pin and under a pin each: 0 allocations.

**What `proof` should measure.**
- Cache lines touched per lookup (perf `mem_load_retired.l2_miss` and `l1_miss` per call)
  over a checkout's paths.
- Bytes copied per file read, which should be 0.
- Allocations per read and per listing, which should be 0. `text.rs`'s allocation budgets
  already hold this for `Reader::text` and `Vfs::text`, and `a_pinned_listing_allocates_nothing`
  holds it for `Pinned::list`.

## Diff and views: `tree.rs` (`diff_into`), `lib.rs` (`open`, `close`, `view`)

**The work and its bound.** A diff visits only the subtrees whose IDs differ. Per visited
directory it needs one pass over the two entry lists, comparing node numbers, not names'
bytes, wherever the names are the same. Opening a view is one slot.

**What the code does.**
- It visits only differing subtrees: a shared child is skipped by node number before its path
  is built.
- Per directory, the merge compares names by interned number and reads name bytes only where
  the numbers differ.
- New: when both directories have the same name column (an edit changed only contents, the
  common case), the node and mode columns are compared 16 entries at a time (two 64-byte and
  one 16-byte compare), and only a differing entry is looked at. For the edit's 32-entry
  directories that is 2 compares each, about 4 lines.

So it is at the bound: nodes visited are the differing path's 4 directories and 1 file.
`open` is one push.

**What `proof` should measure.**
- Nodes visited per diff against the number of differing nodes, which should be 1.0.
- Time per diff of the edit against the checkout's root: about 100 ns warm.

## The watcher: `watch.rs`

**The work.** A batch of N changed paths, from one `Done::Changed`, becomes one commit of what
changed. Per path, the work is learning whether it is a file, a directory or gone, and a
file's bytes if it changed.

**The bound.**
- One effect and one completion per batch.
- One `statx` per path the event leaves in doubt, none for a removal or a directory event
  (the kernel's event says which).
- One read per changed file, and nothing more for a file whose stamp (size, mtime, inode)
  equals the one the watcher saved.

For N = 1 000 with R changed files: 1 round trip, at most N `statx` at 0.4 µs (0.4 ms), and R
reads.

**What the code does.**
- One batched `Effect::Stamp` for all N paths, then one `ReadAll` per changed file and one
  `Entries` per changed directory. That is 1 + R + L effects in two round trips.
- `Journaled` carries no kinds, so every told path is stamped: removals and directory events
  too.
- The commit is now owned: the bytes read are the file's own, not copied again (before, one
  copy each).

**Names are numbers.** Before, the watcher kept every path as a `String` and cloned it at each
step. Counted from the code:
- Every path told cost 1 allocation, its slot's `String`, even when its stamp showed it
  unchanged.
- A changed file read and written cost about 7: its slot, the `ReadAll` name, `taken`, the
  write, the op, the op list's `ops.clone()`, and the `ours` key.
- A listing cost 3 a directory: its name was cloned into the lists and into `Pending`, plus a
  `Box<str>` index key a name.
- Deletes were cloned again into the kept list, and each covered-delete check ran a
  `format!("{path}/")`.
- Every batch cost about 12 on top: the stamp buffer (`Arc<Names>` and its two buffers), the
  slots, a commit's 6 lists, a gone-directory list, and a `Box<str>` key from `Disks::of`
  on every commit, even for a head whose disk was there.

Now:
- Each name is interned once as it comes in, as a 4-byte `PathId` into one per-watcher table
  (`paths.rs`: one buffer of text, end offsets, a `HashTable` by `name_hash`). Slots, looks
  in flight, the commit's lists and `ours` hold IDs.
- Text is read only where a name leaves: an effect's `FileName`, an `Op`'s path, a
  `Watched`'s, each marked `// alloc:`.
- The table clears whenever nothing is in flight, keeping room for 4 096 names. Committed
  batches go back to a spare list of 4, with room for 4 096 slots each. The commit's lists
  live in one reused `Work`. The stamp buffer is reclaimed with `Arc::get_mut` once the
  driver drops its clone.
- Ancestors are compared against `dir` and a `/` in place (`cmp_below`, `is_below`), never
  joined. `Disks::of` finds a head's disk before it makes a key.

The counts now:
- A warm batch of unchanged paths, the journal completion and the stamps' completion, makes
  0 allocations.
- A changed file costs 2, both at the edge: its `ReadAll` name and its op's path.
- A commit costs its op list.

**Tests for this:**
- `watch/tests.rs` `the_watcher_does_what_the_reference_did` runs 3 × 30 seeded runs of 8
  rounds of random deletes, renames, chmods and writes. Each is followed live, after a
  restart, after a restart with the subtree lost, and after a restart with the journal reset.
  The watcher is checked against `watch/reference.rs`, the string watcher as it was. The
  effects, outputs, reads, heads, disks and idleness are the same.
- `a_warm_batch_allocates_only_the_names_its_reads_hand_the_driver` is the allocation
  budget per batch once warm: 0 when unchanged, and the read names alone when every file's
  stamp moved.
- `a_name_compares_with_a_directory_as_the_joined_string_does` checks that `cmp_below` and
  `is_below` agree with the joined string.
- `paths.rs` `a_warm_table_interns_without_allocating`.

**The gap, and what `Effect::Look` buys.** The request in `v2/requests/core.md` (lines
282-301) is the coordinator's:
- `Effect::Look { files, tag }`, completing once with `Done::Looked(Vec<Looked>)`: each path's
  stamp and, when it differs from the stamp sent with it, its bytes or entries;
- kinds in `Done::Changed`.

With it:
- a batch is **one effect and one completion**, where now it is 1 + R + L and two round
  trips: on io_uring, 2 to 5 µs a round trip;
- the stat of every removal and directory event goes, about 0.4 µs each;
- an unchanged file (a `touch`, a save of the same bytes) is never read, because the stamp
  saved with the path matches.

Without it the watcher stays at two round trips. That is 10 µs of latency on a batch whose
work is about 0.4 ms at N = 1 000, so within 10% for large batches. It is 2 to 3 times the
bound for a batch of one, the edit's case. The rest of the watcher's per-path work is within
the bound.

**What `proof` should measure.**
- Effects and completions per batch, which should be 1 and 1 once `Look` lands.
- `statx` calls per removal or directory event, which should be 0.
- Reads per unchanged file, which should be 0.
- Bytes copied between the read and the store, which should be 0.

## Collection: `lib.rs` (`collect`, `collect_if_due`), `tree.rs` (`Tree::collect`), `journal.rs` (`collected`)

**The work.** A collection marks what the heads, the open views and `keep` reach, then
publishes a table of only those nodes. Its cost is the walk and the copy of everything kept:
O(live), whatever it frees.

**The bound.** A constant amount of work per node collected, or per node made, amortized:
- a collection paid for by the garbage it frees;
- the journal's compaction paid for by the appends it replaces (above).

**What the code did.** `Vfs::collect` walks and copies every live node, image nodes included,
on every call. Every collection rewrote the journal. The daemon calls it after every job, so
after a checkout of 110 000 nodes a job that drops 100 nodes walked and copied 110 000: about
10 ms, and a 2 GB journal rewrite. That is about 1 000 times the work per node collected.

**What it does now.**
- `Vfs::collect_if_due` collects only once the tree made at least as many nodes since the last
  collection as that one kept. A collection then walks and copies at most kept + made ≤ 2 ×
  made nodes, so at most 2 node copies per node made. Memory stays within twice the live
  nodes.
- The journal is rewritten only as above.

`collect` keeps its meaning: it collects now, for a caller that needs what it drops gone.
**Within the bound** once the daemon calls `collect_if_due` in `between()` (queue 00877).

**What `proof` should measure.**
- Nodes copied by collections over nodes made, across a run of jobs; it should be at most 2.
- Rewrite bytes over append bytes, which should be at most 2.
- Peak nodes over live nodes, which should be at most 2.

## Tests in this change

In `crates/blaze-vfs/src/tests.rs` unless noted:
- `files_hashed_on_workers_are_their_own_ids`: a differential test of `hash_files` on worker
  threads and on one, against `ContentId::of` per file, over random sizes from empty to 2 MiB.
- `a_vfs_hashing_on_workers_makes_the_same_roots`: a `Vfs` with `hash_on` and an owned commit
  makes the roots and files of a plain one.
- `an_owned_commit_keeps_its_bytes_without_a_copy`: the store lends the very buffer an owned
  op gave, also after a rebase, and the root equals the borrowed commit's.
- `a_directory_whose_names_stayed_diffs_as_the_model`: a differential test of the column fast
  path against `model_diff`, over contents and mode changes.
- `collecting_after_every_commit_rewrites_the_journal_amortized`: 300 commits, each followed
  by a collection. The rewrites' bytes are at most twice the appends', and the file recovers
  the head and its files.
- `a_collection_if_due_waits_until_it_is_paid_for`.
- `a_damaged_file_record_is_refused`: a flipped kind, payload or checksum byte still ends
  recovery.
- `store/tests.rs` `adopted_bytes_are_the_vecs_own`: the reference for `store::adopt`, the one
  new `unsafe`. Its bytes are the `Vec`'s pointer, equal an `Arc` copy of them, and outlive the
  caller's handle.
