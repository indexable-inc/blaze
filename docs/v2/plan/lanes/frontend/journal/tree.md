---
status: plan
summary: blaze v2 opens the VFS tree in place from the daemon's image (frontend-journal-tree) - the store's mapped base generation, files as image ranges checked by page group on first read, a journal that starts with a base record naming the image and holds only the tail, crash-safe across the image's rename and the journal's rewrite - its speed of light, and how it compares with git's index and Sapling's treestate.
date: 2026-10-29
---
# The VFS tree opened in place from the image (frontend-journal-tree)

Part of [restore](../../../design/restore.md): "The VFS tree itself is in the mapped image, not
rebuilt from a journal read whole." Written on 2026-10-29. Nothing has been built or run
(this lane may not build). Every claim below is what the code is written to do, and the test
named for it is what proves it once it runs. The code is in `crates/blaze-vfs/src/`:
`store.rs` (the mapped base), `image.rs` (writing and opening the sections), `journal.rs` (the
base record), and `tree.rs`, `reader.rs` and `rebase.rs` (reads that can fail).

## What a start does

1. The daemon maps its image file (`Effect::Map`) and hands the `Mapping` to
   `Vfs::restore(journal, tags, journal_bytes, Some(mapping))`.
2. `Image::open` reads the spine: the header, the section table, and the CRC table's top CRCs.
   The `vfs tree` section is checked whole (every page group's CRC32C), then
   `Base::check` walks it once. That walk checks that every span, entry, file range and index
   place is in bounds; that each directory's entries are sorted and name only earlier nodes,
   so a walk of the tree ends; and that each mode matches its child's kind. Together these
   are what make the reads safe.
3. The store's first generation is that section, used in place: node IDs, shapes, entry
   columns, file places, names and both open-addressed indexes (by content ID and by name).
   Nodes made afterwards are appended to the store's own columns with numbers after the
   base's. A lookup probes the base's index first, then the appended one.
4. The journal's first record is a base record naming the image: the BLAKE3 of the tree
   section's columns and header, and its node count. The records after it are only what the
   image lacks: nodes made since, and one record per head. Each loads over the mapped tree.
5. No file's bytes are read. A file of the image is a range of the `vfs blobs` section. Its
   page groups are checked by CRC32C the first time anything reads it (`Mapped::read`), and
   `Reader::file` and `Tree::owned` give a `blaze_bytes::Bytes` that holds the image, so
   nothing is copied from the disk to the parse tree.

The image holds nodes, never heads. A base record is only written by a rewrite, and a rewrite
records every head, so the journal's tail always names every head. A head removed after the
image was written therefore never comes back from it, and an image paired with another VFS's
journal adds only content-addressed nodes.

## Moving the journal onto a new image, crash-safe at every step

`Vfs::write_image(&mut Builder)` writes the three sections and stages the image in the journal.
The daemon writes the whole file with `Effect::Write`, which is temp, fsync, rename. On
`Done::Written`, `Vfs::image_durable(name)` makes the next flush rewrite the journal as
`BASE(name, nodes)` plus the tail. The image records `based_on`, the image the journal started
from when it was written (or none). On open, `restore` decides which case it is in:

| journal's first record   | image                     | result                                           |
|--------------------------|---------------------------|--------------------------------------------------|
| `BASE(name, nodes)` of it | any                       | `Used`: tree mapped, tail loaded                 |
| the image's `based_on`   | the stop came between the rename and the rewrite | `Resumed`: tree mapped, whole journal loaded over it, rewritten at the next flush |
| anything else            |                           | `Unnamed`: journal loaded alone, and said so     |

The `Resumed` case is sound only if every node the old journal names is in the new image. The
old journal names its own records' nodes, which the tree had when the image was written, and
nodes of its base image. So an image is written only while the journal is `settled`: no node of
its base has been dropped by a collection since its last rewrite. Otherwise `write_image` is
`ImageError::Busy` until the rewrite that a collection asks for is durable. A rewrite that
fails starting onto a new image is retried from it. A collection renumbers the base's,
the staged image's and the in-flight image's node sets.

## Corruption is named, never read past

- **A tree group fails:** the image is unused. `Recovered.image` is `ImageUse::Corrupt(Corrupt
  { section: VFS_TREE, group, .. })`, and the journal is loaded alone. With a base record and
  no image, its heads whose roots only the image held are dropped and counted in
  `heads_dropped`.
- **A blob group fails:** that file's reads are `Err(Corrupt { section: VFS_BLOBS, group })` in
  `Reader::file`, `Tree::bytes`, `Vfs::read` and `follow`, and `OpError::Corrupt` in a commit
  that needs the bytes. It is never `None` and never stale bytes. `Vfs::corrupt()` names the
  first such group. Writing the same bytes again (the watcher's rescan, a checkout) mends the
  node in place (`Tree::repair`), with every node keeping its number. `write_image` refuses
  (`ImageError::Corrupt`) while a corrupt file is unmended.
- **The disks section:** if it is unusable, the watchers rescan, and the reason is in
  `ImageUse::Used { disks: Some(why) }`.

## Speed of light

What a start costs, per node of the tree (a node is a file or a directory):

- Bytes in the tree section: a 32-byte ID, an 8-byte shape, about 1 entry of 9 bytes (name,
  node, mode), 16 bytes of file place for a file, 8 bytes of index places at most half full,
  and about 8 bytes of name. That is about 80 bytes. 1M nodes make about 80 MB.
- Checking it whole: CRC32C at about 8 bytes a cycle on one core, about 3-4 ms for 80 MB.
  `Image::check` can spread it over the pool (frontend-image). `Base::check` is one pass at
  memory bandwidth, a few ms. Page faults: about 20k 4 KiB pages, or about 40 2 MiB pages if
  the mapping is huge.
- Blobs: 0 bytes. The journal: its tail (head records and nodes since the image), typically
  kilobytes.

What it replaces: `Vfs::recover` over the whole journal interns every file again, so it hashes
every file's bytes with BLAKE3 (about 1 GB/s per core with no SIMD tree mode, more with
parallelism). For a 10 GB tree that is seconds, and it touches every page of every file. The
mapped start is bounded by the tree's metadata, not its contents. The one O(tree) term left is
the whole check of the tree section. Making it lazy per group would need `Base::check`'s
structural guarantees per group (a directory's children are earlier nodes, so possibly in
another group). That is deliberately not done.

## Benchmark against git's index and Sapling's treestate

None of this was run. The numbers below are estimates from each design, to be replaced by the
benchmark named after them.

- **git's index** (`.git/index`): a sorted flat list of entries (stat data, object ID, path;
  v4 prefix-compresses paths). Every command reads and parses it whole into `cache_entry`
  structs, threaded since `index.threads` and the IEOT extension. It is O(entries) per
  command, at roughly 10-50 ms for 100k-400k entries. It holds no contents: those are zlib'd
  objects, inflated on read. The cache-tree extension stores per-directory tree IDs, but it
  is invalidated by any change under that directory and rebuilt by `write-tree`.
- **Sapling's treestate and Mercurial's dirstate-v2**: the closest analogue. A tree of nodes
  in an append-only data file is mapped and read in place, lazily. A small docket names the
  data file and its used length. The data file is rewritten when its unreachable bytes pass a
  ratio. blaze's base record plays the docket's role (it names the image by hash, and the
  rewrite happens on collection). blaze differs in three ways. The image also holds every
  file's bytes, so a start reads no file and a build reads them from the same mapping with
  no copy. Every page group is CRC-checked, with the bad one named; the docket designs trust
  the bytes. And the base switch is crash-safe in both orders (the `Resumed` case), where a
  docket is a single atomic rename.
- **To measure** (a benchmark for the speed lane, once blaze builds blaze-vfs): a checkout of
  the Linux kernel (about 80k files) and of Chromium (about 400k). Measure:
  1. `Vfs::restore` from an image plus an empty tail;
  2. `Vfs::recover` of the same tree's whole journal;
  3. `git status` with a warm index (`git update-index --refresh` excluded);
  4. `sl status` with a warm treestate.

  Report wall time, bytes read (`/proc/self/io`), and page faults. Expected: (1) is
  milliseconds and reads no blob page; (2) is seconds; (3) and (4) are tens of milliseconds.

## Which test proves what

All are in `crates/blaze-vfs/src/image/tests.rs` unless noted.

| claim | test |
|---|---|
| a restored VFS answers as one recovered from a journal that never had an image (heads, sequences, every file and mode, every listing, every pairwise diff), across random histories with collections, branches, removals and restarts, and keeps doing so after more commits | `an_image_and_its_journal_restore_what_the_journal_alone_recovers` (64 seeds; `BLAZE_DST_SEEDS`) |
| a stop at every step of write image, commits, rename, durable, flush, submit, rewrite done, and later commits, over two images with a removal and a collection between them, loads what was durable; the next flush then gives `Used`; `write_image` is `Busy` while base nodes are dropped | `a_stop_at_any_step_of_moving_onto_an_image_loads_what_was_durable` (8 seeds, every place) |
| a start reads the journal's tail only and no blob group; a read checks exactly the groups of that file; the tree is checked whole | `a_start_reads_the_journals_tail_and_no_file_until_one_is_read` |
| a corrupt blob group is named in the read's `Err` and by `Vfs::corrupt`, other files still read, `write_image` refuses, and writing the bytes again mends it | `a_corrupt_file_is_named_by_its_group_and_mended_by_writing_it_again` |
| a corrupt tree group leaves the image unused, says so, and counts the dropped heads; an unrelated image is `Unnamed`; a non-image is `Unopened` | `a_corrupt_tree_leaves_the_image_unused_and_says_so` |
| an image brings nodes, never heads | `an_image_brings_nodes_and_never_heads` |
| `Base::check` refuses each malformed base | `store/tests.rs`: `a_well_formed_base_passes_its_check`, `a_malformed_base_is_refused` |

## Open

- **The daemon's start and stop** (cargo's `serve/daemon.rs`) still call `Vfs::recover` and
  write no VFS sections, so a start loads the journal alone. Asked in `v2/requests/cargo.md`
  with the exact edits. blaze-vfs's own wiring (`lib.rs`, `Cargo.toml`, `tests.rs`) and
  blaze-query's `Tree::file` are in.
- **A repair is in memory only.** The journal does not record the mended file, because its
  base holds the node. A restart before the next image finds the group corrupt again, and
  the watcher's rescan mends it again. Recording the mended file's bytes in the tail would
  make that durable. A `FILE` record of a base node already loads as a repair (`Tree::file`).