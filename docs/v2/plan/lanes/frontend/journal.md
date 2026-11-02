---
status: plan
summary: frontend-journal - a warm restart takes what changed from the OS's journal (FSEvents from a saved event ID, NTFS's USN from a saved USN, Linux's statx sweep of the saved stamp column) and reads and hashes only files whose stamp moved; lost journals are named and rescan only their subtree. The VFS side (blaze-vfs's watcher and disk column), its speed of light, how it compares with git, watchman, Everything and Bazel, and which test proves what.
date: 2026-10-29
---
# Changed files come from the OS's journal (frontend-journal)

[restore.md](../../design/restore.md), "Changed files come from the OS's journal". Per
platform, the drivers' side is in [journal/macos.md](journal/macos.md) (FSEvents),
[journal/windows.md](journal/windows.md) (the USN journal) and [journal/linux.md](journal/linux.md)
(the `statx` sweep and fanotify). This note is the shared shape and the VFS side.

## The interface (blaze-core, sans-IO)

`crates/blaze-core/src/journal.rs` holds the types; every driver and the simulation implement
the effects.

- `Effect::Journal { dir, since: Cursor, tag }` completes with `Done::Journaled` batches: the
  names under `dir` that changed (sorted, once each), the subtrees it lost and why
  (`(FileName, Lost)`), the cursor after the batch, and `caught_up` once all history from
  before the watch has been told. A cursor is never past a change that is not in its batch or
  an earlier one.
- `Cursor` is `FsEvents { event, volume }` (the event ID and the UUID of the volume's event store,
  `FSEventsCopyUUIDForDevice`), `Usn { volume, journal, next }`, `Sim { journal, next }`, or
  `None`. Encoded in 25 bytes for the image.
- `Lost` is named, never silent: `NoJournal` (Linux after a restart), `Reset` (another store
  UUID, wrapped IDs, a new USN journal ID, a cursor of another kind), `Truncated`, `Dropped`,
  `Rescan` (`MustScanSubDirs`), `Unnamed` (one directory's names changed but the journal does not
  say which: NTFS's unprivileged read) and `RootChanged`.
- `Effect::Stamp { files: Arc<Names>, tag }` gives `Done::Stamped(Vec<Stamp>)`: inode, size,
  mtime, ctime and mode of every name, in order. `Names` is one buffer of NUL-terminated names,
  so a driver hands it to the system with no allocation per name; it is shared, so the asker
  keeps it for its comparison without a copy.
- `Effect::Entries { dir, tag }` gives one directory's names with their stamps.

## The VFS side (blaze-vfs)

- **The column** (`crates/blaze-vfs/src/disk.rs`): for each head, the watched directory, its
  `Cursor`, the head's root when the column last held all of it, and every tracked path's
  `Stamp`, sorted bytewise. It is saved as the image's `VFS_DISKS` section (written and opened by
  `crates/blaze-vfs/src/image.rs`) and read in place on a start (`Base`): names NUL-terminated
  as `Names` keeps them, ends, 36-byte stamps. Everything is checked once when it is opened, so
  reads after never fail. What changes later goes into an overlay, and the next image merges
  them.
- **The watcher** (`crates/blaze-vfs/src/watch.rs`):
  - `Watcher::start(&Vfs, ..)` asks the journal from the saved cursor. With no cursor (a first
    start, or Linux) it rescans the directory, which with a saved column is a sweep of stamps,
    not a read of every file.
  - Every look is stamps first. A path whose stamp equals the saved one, and which the head has,
    is never read or hashed. The rest are read after their stamp was taken, so a change during
    the read moves the stamp and is seen again.
  - A rescan of a lost subtree stamps every path the column and the head have under it. It
    lists only directories whose stamp moved, or that are new, and reads only files that are
    new or moved. Nothing outside the subtree is looked at. `Lost::Unnamed` lists the one
    directory alone and diffs it against the column.
  - A move is not a read. A name new to a batch whose stamp (inode, size, mtime, type) is that of
    a file the batch found gone is that file moved: `Op::Rename` when the source goes in the same
    commit, else its bytes from the tree. A directory renamed while blaze was down reads none of
    its files.
  - A batch commits as one transaction once every look in it is in, in the order the journal
    told them. Its stamps and the cursor after it are saved then, so the saved cursor never
    covers an uncommitted change. A path the head changed since the report is a
    `Watched::Conflict`; the head keeps its own.
- **The tree** is mapped from the image, not rebuilt: `frontend-journal-tree`
  ([journal/tree.md](journal/tree.md) once written), `crates/blaze-vfs/src/{image,store,tree}.rs`.
- **The daemon's hooks** are lane cargo's: `heads.rs` filters the new completions
  (`Done::Journaled`, `Done::Entries`) and passes `&Vfs` to `Watcher::start`. The request is in
  `v2/requests/cargo.md`, "frontend-journal: checkouts resume from the OS's journal". The
  image's start and stop hooks for the VFS sections are `frontend-journal-tree`'s request.

## Speed of light

A warm restart with N tracked files of which k changed while blaze was down. The work that
must be done: learn which k changed, and read and hash those k. Everything per unchanged file
is overhead.

- **macOS and Windows: O(k).** The journal names the k paths (its cost is in the platform
  notes: fseventsd's history replay; about 10 ms of USN parsing per million records plus one
  name lookup per changed file). Per named path, the watcher does one binary search of the
  mapped column (log2 N ≈ 17 probes, ~1–2 µs cold), one stamp in a batched `Effect::Stamp`,
  and a read and a BLAKE3 hash only if the stamp moved. Nothing is done for the N − k
  unchanged files. The bound is the k reads and hashes, and this is it.
- **Linux: N stamps, then O(k).** No journal survives a restart, so every tracked name must be
  `statx`ed. That is N·t/C ≈ 6.3 ms on 8 cores and 0.78 ms on 64 for N = 100 000
  ([journal/linux.md](journal/linux.md)). The watcher's own serial work for an unchanged tree:
  - one copy of the column's names into the shared `Names` (~3 MB at ~30 GB/s, ~0.1 ms) plus
    its starts (0.4 MB);
  - decoding the saved stamps (3.6 MB, ~0.15 ms);
  - comparing N 40-byte stamps after the sweep (~0.1 ms, no allocation and no tree lookup for
    an equal stamp).

  That is about 0.35 ms, 6% of the 8-core bound, which meets the 10% bar there. Two things
  that used to be there are gone: a per-name walk building the buffer (~25 ns a name, 2.5 ms)
  and a second copy of it for the effect.
- **Where it is still off:** on 64 cores the serial 0.35 ms is about 45% of the 0.78 ms bound.
  Reaching it needs the serial part to be O(k):
  - the sweep reads names from the mapped column in place, with no copy;
  - the stampers compare against the saved stamps and return only the indices that moved.

  That changes `Effect::Stamp`'s contract in every driver, so it is listed as open in
  `/tmp/blaze-lanes/v2-frontend-journal.md`, not done here.

## Compared with the state of the art

- **git** keeps each file's stat data (ctime, mtime, inode, size, mode) in its index and, with
  `core.preloadIndex`, `lstat`s every tracked path on up to 20 threads by whole path. That is
  the Linux design here. The sweep is per core, `statx` relative to an opened directory (no
  path walk from the root per name), and compares in place against a mapped column instead of
  a parsed index. With `core.fsmonitor`, git asks a daemon (watchman, or its own on macOS and
  Windows) for changes since a token, but the token does not survive the daemon's restart:
  that is a full `lstat` again. Here the FSEvents ID and the USN are saved, so a restart on
  macOS or Windows looks at no unchanged file at all.
- **watchman** starts its FSEvents streams `SinceNow`, and its clocks die with the server, so a
  restart is a full crawl ([journal/macos.md](journal/macos.md)). On Windows it uses
  `ReadDirectoryChangesW`, not the USN journal ([journal/windows.md](journal/windows.md)).
- **Everything** resumes from a saved USN journal ID and USN, as here, but rescans the whole
  volume when the journal ID changes and needs administrator rights. Here a new journal ID
  rescans only the watched directory, by stamps (`Lost::Reset`), and the read is unprivileged.
- **Bazel** with `--watchfs` drops what it watched on a server restart and re-stats every file
  of the build's inputs. That matches the Linux path here and loses to the macOS and Windows
  ones.

## Proved by

All with the simulation driver (`blaze_core::sim`, whose `Journal::{None, FsEvents, Usn}`
behaves as each system across a crash), in `crates/blaze-vfs/src/watch/tests.rs`:

- `a_first_start_reads_the_directory_once`: a cold start reads every file once and ends
  `Scanned`.
- `a_restart_with_nothing_changed_reads_and_commits_nothing`: on every system and 20 seeds, no
  read and no commit. With a journal nothing is stamped. Linux sweeps every tracked name once,
  and only those.
- `what_changed_while_down_is_read_and_what_moved_is_not`: edits, a new directory, a delete, a
  rename, a directory renamed and a chmod while down. Only the edited, new and chmodded files
  are read (the chmod makes no op), everything commits as one transaction, and with a journal
  no unnamed path is stamped.
- `a_file_renamed_over_another_is_not_read`: a rename over a tracked file moves its node.
- `a_lost_journal_is_named_and_only_its_subtree_is_swept`: a reset store or journal, a
  truncated one, and `MustScanSubDirs` on one subtree are each named. Only what changed is
  read, and nothing outside the lost subtree is stamped.
- `with_no_journal_a_saved_cursor_is_named_lost_and_swept`: a cursor from macOS on a system
  with none is `Lost::NoJournal`.
- `an_unnamed_change_lists_its_directory_alone`: `Lost::Unnamed` lists one directory, stamps
  nothing, reads only the new file, moves the renamed one and drops a removed subtree.
- `a_cursor_is_saved_only_with_what_it_covers`: the cursor moves only with a commit, and a
  restart from it hears nothing again.
- `the_head_follows_the_disk_across_restarts`: random edits, deletes, renames and chmods over
  100 seeds and 4 restarts on each system. The head is always the disk, and no file is read
  that was not written, chmodded or renamed onto a tracked path.

In `crates/blaze-vfs/src/disk.rs`: the column reopens in place and merges later changes; a
directory's children are found by search; a cut or corrupt section is refused, naming why;
and `a_sweep_of_a_mapped_column_is_its_names_as_they_are` checks that the one-copy sweep
equals the merging walk. The drivers' own tests are listed in each platform note.
