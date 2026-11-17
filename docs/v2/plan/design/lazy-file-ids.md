---
status: plan
summary: A file of a head is stamped when the watcher sees it and hashed only when a build first reads it, streamed in a 64 KiB buffer on the pool; its bytes go to the store once and the tree, the journal and the image name it by content ID, so no file size is bounded by a 32-bit record and the main thread never hashes.
date: 2026-11-13
---
# Lazy file IDs

## The floor

For a checkout of N files of which a build reads R:

- a file no build reads costs one stat on a rescan, and nothing on a journal event that does
  not name it;
- a file a build reads costs one read and one hash, streamed through a bounded buffer, and its
  bytes written to the store once;
- a cache directory (`CACHEDIR.TAG`) costs one stat and one 43-byte read of its tag
  (`crates/blaze-vfs/src/watch/view.rs`, built).

## What it costs today

- The watcher reads every file it finds new or changed, whole (`Effect::ReadAll`), whether a
  build will read it or not: N reads where the floor is R.
- Each file is held whole in memory: the read's `Vec`, then the tree's `Bytes` in the store
  table, then a copy framed into the VFS journal's `FILE` record (`frame_file`, whose 32-bit
  length bounds a file at `MOST_FILE_BYTES`). Committed files are inline `Bytes`; files of a
  mapped image are content IDs in the injected `Texts` store.
- `commit_pending` hashes with `hash_files`, which goes to the pool only when a commit has
  more than one file and at least 1 MiB in all (`crates/blaze-vfs/src/tree.rs:52`): one big
  file is hashed on the thread that commits, the daemon's main thread.

## The end state

1. **A file node may be unhashed.** The tree gains a node kind `Unread { stamp, size, mode }`:
   the watcher commits a new or changed file as it, after a stamp alone, with no read. Its
   directory's ID covers the stamp (a stamped node's identity is its stamp, the domain kept
   apart from content IDs) until it is hashed.
2. **A read hashes it once.** `Reader::file` (and blaze-query's `Tree` input) on an `Unread`
   node asks the driver for `Effect::HashFile { file, stamp }`. The driver runs it on the pool:
   open, `pread` 64 KiB at a time into one buffer, `blake3::Hasher::update` per buffer under
   the VFS file domain, and the same buffers written to the store's pack as they go; fstat
   after. A stamp that moved meanwhile fails the read with the file named (the watcher's
   `VERIFIES` rule, moved to the driver). The completion is `(ContentId, size)`; the head's
   node becomes `File { id }` in a commit of its own. The main thread never hashes and never
   holds a file's bytes.
3. **The buffer is 64 KiB.** blake3 hashes 1 KiB chunks, up to 16 at once with AVX-512 (8 with
   AVX2, 4 with NEON: `MAX_SIMD_DEGREE`), and its own `update_reader` copies through 65536
   bytes (`blake3-1.8.x/src/io.rs`, `copy_wide`). 64 KiB is four full 16-chunk SIMD rounds, a
   power of two so every update lands on a subtree boundary, and small enough to sit in L2
   per worker; larger buffers buy nothing the hasher can use from one thread. Files over
   4 MiB may be split across workers by blake3's subtree hashing (its `hazmat` API), so one
   huge file is not one worker's alone: both of blaze-ids' `b3` backends must then expose
   the chaining value of a subtree.
4. **No file size is bounded.** The journal's `FILE` record holds `(path, ContentId, size)`,
   not the bytes; the bytes are the store's, written before the record (the record names
   only what is durable). `MOST_FILE_BYTES`, `OpError::TooLarge` and `Watched::TooLarge`
   go, and so does the `frame_file` bound.
5. **The image names files by ID.** The VFS image's tree section gains the `Unread` node
   (stamp, size, mode) beside `File` (content ID, size, mode); a reader of an image of the old
   layout reads every node as `File` (tree schema bump, with a round-trip test and a test of
   the previous layout, rule persisted-state-round-trips).

## Readers it touches

- `crates/blaze-vfs`: `tree.rs` (node kind, `hash_files` goes), `journal.rs` (`FILE` record by
  ID), `image.rs` (node kind), `reader.rs` (`file` asks for the hash), `watch.rs` (commits
  `Unread` nodes; reads only what a build reads).
- `blaze-core`: `Effect::HashFile` and its completion in every driver (uring, kqueue, iocp,
  threads, sim), streaming to the store's pack writer.
- blaze-query's VFS input (`Tree` reads): a read of an `Unread` file waits on its hash, a span
  naming the file (rule every-wait-says-what).
- The daemon's settle and image writer, and `blaze-cas`'s `Texts` (the bytes' one home).

## Cost against the floor

Every operation above is at the floor: a stamp per file seen, one read and one hash per file
read, in a 64 KiB buffer, the bytes written once. Not reached by what is built today: N reads
for R, a whole-file copy held in memory and framed into the journal, and the main thread
hashing a commit of one large file.
