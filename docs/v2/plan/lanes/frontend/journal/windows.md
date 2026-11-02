---
status: plan
summary: The Windows journal of changes (frontend-journal-windows) - the NTFS/ReFS USN journal read unprivileged from a saved USN and journal ID, overlapped on the completion port, with reference numbers made names through a lazy map of directories, removals told as Lost::Unnamed directories, and stamps and listings by handle and FileIdBothDirectoryInfo - compared with how Everything and watchman read the journal.
date: 2026-10-29
---
# The journal on Windows (frontend-journal-windows)

`crates/blaze-core/src/iocp/usn.rs` (the journal), `crates/blaze-core/src/iocp/stamp.rs`
(`stamps`, `entries`) and `crates/blaze-core/src/iocp.rs` (the driver's wiring) make the Windows
half of docs/v2/plan/design/restore.md's "Changed files come from the OS's journal": on a warm
restart the daemon asks the volume's USN journal what changed under each watched directory
since the cursor saved in its image, and hashes only those files.

## The cursor

`Cursor::Usn { volume, journal, next }`: the 64-bit serial number of the watched directory's
volume (`FILE_ID_INFO::VolumeSerialNumber`, which ReFS needs at 64 bits), the journal's
`UsnJournalID` (a journal deleted and made again has a new one), and the USN after the last
record the batch took. USNs are byte offsets into the journal's stream and only grow while the
ID stays.

## Starting

The directory is opened for its attributes (the "hint" handle every `OpenFileById` is relative
to); its normalized NT path, volume GUID path, file ID and volume flags are read from it. A
network share (`\Device\Mup\...`) or a volume without `FILE_SUPPORTS_USN_JOURNAL` (FAT, exFAT)
fails the effect, naming the directory and the file system. The volume is opened twice by its
GUID path with `FILE_TRAVERSE` only: synchronous for `FSCTL_QUERY_USN_JOURNAL`, overlapped and on
the port for the reads. `ERROR_JOURNAL_NOT_ACTIVE` fails, naming the `fsutil` command an
administrator runs to make one; blaze reads journals and never makes or resizes them.

| `since` | starts at | first batch |
| --- | --- | --- |
| `None` | `NextUsn` | at once: empty, `caught_up`, the cursor |
| same serial and ID, `FirstUsn <= next <= NextUsn` | `next` | after the history |
| same serial and ID, `next < FirstUsn` | `NextUsn` | at once: `(dir, Truncated)`, `caught_up` |
| same serial and ID, `next > NextUsn` | `NextUsn` | at once: `(dir, Reset)`, `caught_up` |
| another serial or ID, or another kind of cursor | `NextUsn` | at once: `(dir, Reset)`, `caught_up` |

## Reading

One read: `READ_USN_JOURNAL_DATA_V1 { StartUsn: next, ReasonMask, UsnJournalID, Min 2, Max 3 }`
into a 256 KiB buffer, as `METHOD_NEITHER` in place, so the request and the buffer live in the
boxed `Journal`, which the driver keeps in the request until the completion is taken (the
`Request` rule of `iocp.rs`). The history is read with `BytesToWaitFor = 0` until a read reaches
the `NextUsn` queried at the start (or returns nothing past its start); that batch says
`caught_up: true`, and every read after waits for one byte (`BytesToWaitFor = 1`), so live
changes come from the journal too and every batch's cursor is the exact USN after its last
record. A history batch is told every 4096 names; a live one whenever it names anything, or
once the journal moved 1 MiB past the last cursor told with nothing under the directory, so a
saved cursor stays well inside the journal's reach.

`ReasonMask` is every reason that changes data, names, attributes, security or links (overwrite,
extend, truncation, named streams, create, delete, EA, security, both rename halves, indexable,
basic info, hard links, compression, encryption, reparse points, streams, integrity). A close
record repeats its handle's reasons, each told by its own record already; only a removal
happens at the close, so a close is a change of names only when it says `FILE_DELETE`.

Errors of a read: `ERROR_HANDLE_EOF` is the end of what is there. `ERROR_JOURNAL_ENTRY_DELETED`
(the journal wrapped past `next`) queries again and goes on from the new `NextUsn` with `(dir,
Truncated)`. `ERROR_JOURNAL_DELETE_IN_PROGRESS`, `ERROR_JOURNAL_NOT_ACTIVE` or
`ERROR_INVALID_PARAMETER` query again: a journal made again (another ID) goes on from its
`NextUsn` with `(dir, Reset)`; no journal ends the watch with a last batch of `(dir, Reset)` and
a `Done::Failed` naming why, because there is nothing to reopen until an administrator makes
one, and waiting for that would be a quiet stall. `ERROR_INVALID_FUNCTION` fails naming Windows
10 1709. A record of a version not asked for, or running past the read, fails naming its byte.

## One read, never two: why unprivileged

`FSCTL_READ_USN_JOURNAL` is `FILE_ANY_ACCESS` as an ioctl, but NTFS refuses it unless the volume
handle was opened with read access, and opening a volume for reading takes Administrators. A
build tool must not need elevation, so blaze reads `FSCTL_READ_UNPRIVILEGED_USN_JOURNAL`
(Windows 10 1709 and later): any user who can open the volume for traversal may read it, and
NTFS strips each record's name (`FileNameLength` 0). Picking the privileged read where it is
allowed would be two code paths with different names for the same change and a test matrix
twice the size, and a quiet change of behavior with elevation, so blaze never tries it. Where
the unprivileged read is refused, the watch fails, naming why.

The parser still reads names where a record has one (`Names::record` takes `Option<&[u8]>`),
which the unit tests drive, so a later move to a named read changes only the request.

Proved on Windows only by a run as a non-administrator: GitHub's Windows runners are
elevated, so `a_journal_from_its_cursor_hears_what_changed_while_it_was_unwatched` there shows
the journal logic, not the access check. The first unelevated run checks that `FILE_TRAVERSE`
on the volume is enough; if the volume's DACL refuses it, the open fails naming the volume and
the access, never anything quieter.

## Reference numbers to names

Each record has its file's and its parent's reference numbers (64 bits in `USN_RECORD_V2`,
128 in `V3`, which ReFS needs; both are kept as `u128`, `FILE_ID_128` read little-endian, which
on NTFS is the same number), its attributes and its reasons. `Names` keeps an
`FxHashMap<u128, Place>` of the directories met, seeded with the watched directory's own ID:
`Under(name)` (an index into an interned `Vec<FileName>`), `Unnameable` (below a name that is not
UTF-8), `Outside`, or `Gone`. An unknown parent is asked once: `OpenFileById` (no access, share
all, backup semantics, the reparse point itself) and `GetFinalPathNameByHandleW` (normalized, NT
volume name, compared with the watched directory's NT path, so no mount point or drive letter is
looked up); outside, or refused, is kept as `Outside` and never asked again; gone since is
skipped and not kept.

| record | told as |
| --- | --- |
| a file made, written, its attributes or links changed | its name now (one `OpenFileById` per file per read) |
| a file removed, or a rename's old name | `(its directory, Lost::Unnamed)` |
| a rename's new name | its name now |
| a directory made or come in by rename | its name now, and kept in the map |
| a directory known renamed away or removed | its old name, and it and every directory under it forgotten |
| a directory not known renamed away from under the watched one | `(dir, Unnamed)`, and every directory under the watched one forgotten |
| a record in a directory below a name that is not UTF-8 | `(that name's directory, Lost::Rescan)` |
| the watched directory itself renamed or removed, or one above it moved | `(dir, RootChanged)` and the watch fails |
| anything with its parent outside | nothing (one map lookup) |

`Lost::Unnamed` (added to `crate::Lost` for this read) tells the VFS to list that directory's
entries again (`Effect::Entries`) and compare them with what it has: new names are taken whole,
missing ones dropped with what was under them, nothing deeper is rescanned. Without names a
removal can say no less.

## Stamps and entries

`stamps(root, names)`: one `CreateFileW(FILE_READ_ATTRIBUTES, share all, backup semantics, open
reparse point)` per name, `GetFileInformationByHandle` (file index, size) and
`GetFileInformationByHandleEx(FileBasicInfo)` (`LastWriteTime`, `ChangeTime`, attributes), plus
`FileAttributeTagInfo` only for a reparse point. Mode is made as `Stamp` says: a name-surrogate
reparse point (symbolic link, junction) `0o120777`, a directory `0o040755`, a file `0o100644`, or
`0o100444` when read-only, so a read-only flip moves the stamp the same way a chmod moves it on
Unix. A cloud placeholder or deduplicated file is a file, as `std::fs` treats it. Names Windows
cannot hold (`\`, `:`, wildcards, control characters) are `GONE`; an error other than not found
is `UNKNOWN`. Over 2048 names the sweep runs on up to 8 scoped threads, in chunks, as
`crate::fs::stamps` does.

`entries(dir, name)`: `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)`, 64 KiB of entries
per call into a buffer reused per thread. Its entries have the file ID, `ChangeTime`, size and
attributes (and the reparse tag where `EaSize` would be), so no entry is opened.
`FindFirstFileExW(FIND_FIRST_EX_LARGE_FETCH)` was the alternative, but its `WIN32_FIND_DATAW` has
neither a file ID nor a change time, so every entry would need a handle of its own: about
10 times the cost. A name that is not UTF-8 fails the listing, naming the directory, as on Unix.

## Speed of light

- History: the kernel copies records at memory speed (about 100 bytes each, 2500 per 256 KiB
  read, about 15 µs of copying per read); parsing is a bounds-checked walk of fixed offsets,
  about 10 ns per record. What dominates is naming: with names stripped, each distinct file
  changed under the directory costs one `OpenFileById` + `GetFinalPathNameByHandleW` + close,
  about 10 to 20 µs on NTFS, once per read (the `seen` set), and each directory once per watch
  (the map). Records outside the directory cost one hash lookup each. So 1 million journal
  records with 1000 files changed under the directory cost about 400 reads (6 ms of copying),
  10 ms of parsing and 10 to 20 ms of lookups. The floor is the lookups, which the unprivileged
  read forces; blaze is at it because it asks each file once per read and each directory once.
- Live: one read in flight per watch, completed on the port; no polling, no thread blocked.
- Stamps: one open, two or three queries and a close, about 5 to 15 µs a name; 100k names on 8
  threads in about 100 to 200 ms. A listing gives every entry of a directory in one call at
  under 1 µs an entry, so a sweep of whole directories through `Effect::Entries` is the faster
  path, and the VFS's restart on Windows needs neither: the journal names what changed.

## Compared with Everything and watchman

- **Everything (voidtools)** runs a service with administrator rights. It builds its index by
  enumerating the MFT (`FSCTL_ENUM_USN_DATA`), keeps the parent reference number of every file on
  the volume, and follows changes with the privileged `FSCTL_READ_USN_JOURNAL`, names included.
  It saves the journal ID and next USN with its database and, on start, reads on from there; a
  different ID means a full rescan of the volume. blaze keeps the same cursor (plus the volume
  serial), resumes the same way, and on a different ID rescans only the watched directory
  (`Lost::Reset`). blaze never enumerates the MFT: the VFS tree in the image already has every
  name, and the map holds only the directories the journal names, met lazily. Not needing
  elevation is what costs blaze the names, paid for as one lookup per changed file and
  `Lost::Unnamed` for removals.
- **watchman** on Windows watches with `ReadDirectoryChangesW` on a directory handle and does not
  read the USN journal. Its clocks do not survive a restart of the watchman server, so a restart
  is a full crawl of the tree. blaze resumes from the saved USN without any crawl, and its live
  changes come from the same journal as its history, so there is no gap between the two and no
  overflow (`ERROR_NOTIFY_ENUM_DIR`) to rescan after.

## Tests

- `iocp/usn.rs` (unit tests, a fake volume): a file written is named where it is once per read;
  a removal or an old name is its directory's `Unnamed`; outside directories are asked once and
  never named; a directory made and renamed names its files where they are now; named records
  are named from their parent with no lookup; a name that is not UTF-8 is its directory's
  `Rescan`; the watched directory moving, or one above it, ends the watch; a directory not known
  moving away forgets where directories are; batches are sorted and deduplicated; records of
  versions 2 and 3 parse, and short, oversized or other-version records are refused; a close
  tells only a removal.
- `iocp/stamp.rs` (unit tests): a listing entry's fields; a reparse point is a link only when it
  is a name surrogate; names Windows keeps out are gone.
- `iocp.rs` (Windows, the real volume): a journal from `Cursor::None` gives a cursor at once;
  files written, renamed and removed while unwatched, and a directory made, filled and renamed,
  are heard by a journal from the saved cursor (history batches, then caught up), nothing
  outside the directory is named, removals and old names are `Unnamed`, cursors only grow; a
  cursor with another journal ID, another volume or of another kind is `Reset`; stamps of a file,
  a directory, a missing name, a name under a missing directory, a junction and (where it can
  be made) a symbolic link, through `stamps` and `Effect::Stamp`, equal to `Effect::Entries`'
  stamps of the same names, and a write moving only its file's stamp.
