---
status: plan
summary: The macOS journal of changes (frontend-journal-macos) - an FSEvents stream started sinceWhen a saved event ID, checked against the volume store's UUID, with history, HistoryDone, dropped events, rescans, root changes and ID wraps told as named Lost subtrees - and how it compares with watchman's FSEvents watcher, which never resumes across a restart.
date: 2026-10-29
---
# The journal on macOS (frontend-journal-macos)

`crates/blaze-core/src/fsevents.rs` (`Stream::journal`) and `crates/blaze-core/src/kqueue.rs`
(`Effect::Journal`, `Effect::Stamp`, `Effect::Entries`) make the macOS half of
docs/v2/plan/design/restore.md's "Changed files come from the OS's journal": on a warm restart
the daemon asks FSEvents what changed under each watched directory since the cursor saved in
its image, and hashes only those files.

## The cursor

`Cursor::FsEvents { event, volume }`: the highest event ID a batch told, and the UUID of the
watched directory's volume's event store (`FSEventsCopyUUIDForDevice` of `stat`'s `st_dev`,
read with `CFUUIDGetUUIDBytes` and released). Event IDs are system-wide and only grow, so the
ID alone would do on one boot of one store; the UUID says it is still that store. fseventsd
gives a store a new UUID when it discards one (a volume erased, its `.fseventsd` removed, a
store too damaged to read), and then old IDs mean nothing on it.

## Starting

1. `FSEventsGetCurrentEventId()` is read *before* the stream is made.
2. A cursor of this store with `event` at most that ID resumes: the stream starts
   `sinceWhen = event`, and fseventsd tells every event after it from its store, then
   `kFSEventStreamEventFlagHistoryDone`, then live events. Batches before HistoryDone say
   `caught_up: false`; its batch and every one after say `caught_up: true`. History and
   HistoryDone may come in one callback, so that batch holds history and is caught up.
3. `Cursor::None` starts `sinceWhen` the ID read in step 1, not `kFSEventStreamEventIdSinceNow`:
   an event between the read and the stream's start is in its history, never in the gap. Its
   first batch is sent before the stream starts (so before anything it hears): empty, at that
   ID, `caught_up: false`.
4. Anything else (another UUID, an ID past now so the system's IDs wrapped or the store was
   reset, a cursor of another system's kind) starts as `Cursor::None` and its first batch names
   the watched directory `Lost::Reset`: whoever watches sweeps it (stamps, then entries of
   directories whose stamp moved), the only place a full look happens.

Flags are `kFSEventStreamCreateFlagFileEvents | NoDefer | WatchRoot` with latency 0, as the
plain watch's, on a serial dispatch queue of the stream's own.

## Each batch

| FSEvents says | Told as |
| --- | --- |
| `HistoryDone` (0x10) | `caught_up: true` from this batch on; its path and ID are ignored |
| `EventIdsWrapped` (0x08) | the watched directory `Lost::Reset`; the cursor restarts at this ID |
| `UserDropped`, `KernelDropped` | the watched directory `Lost::Dropped` |
| `RootChanged`, `Mount`, `Unmount` | the watched directory `Lost::RootChanged` |
| `MustScanSubDirs` | that path `Lost::Rescan` (the watched directory for it, an ancestor of it, or a path no `FileName` names) |
| anything else under the directory | its name in `files`, sorted, once |

The cursor is the batch's highest ID (never lower than the last told, except after a wrap).
fseventsd hands a stream its events in ID order, so every event of the watched paths below
that ID is in this batch or an earlier one: the contract of `Effect::Journal`. A batch naming
nothing is still sent when it moves the cursor or flips `caught_up`, so a quiet daemon's saved
cursor keeps up with the system's.

Flushes use the watch's cookies. A history can hold cookie events of an earlier stream that
had the same cookie directory (same process ID and stream number), so a journal acts on a
cookie only once caught up; a flush asked during the history is told after it, which is what
a flush means there anyway.

## Against watchman

Watchman's FSEvents watcher starts its streams `kFSEventStreamEventIdSinceNow` and keeps its
own clock (`c:<start>:<pid>:<root>:<tick>`), which a restart invalidates: a query with an old
clock is a fresh instance and the root is crawled whole. It uses an event ID only inside one
run: with `fsevents_try_resync` on, after dropped events it remakes the stream from the last
good ID instead of recrawling, and `MustScanSubDirs` recrawls that directory. Here the event ID
and the store's UUID are the clock and live in the daemon's image, so a restart resumes where
it stopped and nothing is crawled; a store that changed is told as `Lost::Reset`, never
guessed at. Reading the current ID before the stream is made also closes the window
`SinceNow` leaves open between a crawl and the stream's start.

## Proved by

- `fsevents::tests::a_journal_resumes_from_its_cursor_with_what_changed_while_it_was_stopped`:
  a journal from `Cursor::None` gives a cursor at once; a live change moves it; a file written
  while no stream runs is told by a new journal from that cursor, in its history, before or
  with HistoryDone; nothing from before the cursor is told again; names are sorted, once, and
  under the directory.
- `fsevents::tests::a_cursor_of_another_store_or_past_now_is_lost_as_reset`: another UUID, an ID
  past now, a USN cursor and a simulation cursor each give `Lost::Reset` of the directory and a
  cursor of this store at now.
- `kqueue::tests::a_journal_resumes_from_its_cursor_through_the_driver`: the same through
  `Effect::Journal` and `Effect::Unwatch`; a batch does not end it in `in_flight`.
- `kqueue::tests::stamps_and_entries_are_the_file_systems`: `Effect::Stamp` and
  `Effect::Entries` on the kqueue driver equal `fs::stamps` and `fs::entries` (a file, a
  directory, a link not followed, a missing name, a name under a file).
