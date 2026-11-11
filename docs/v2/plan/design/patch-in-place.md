---
status: plan
summary: What a link after a one-function edit costs, step by step, and the design that makes it cost the edit. Built - the link as a patch of its file (ranges written, a Merkle digest of the file), link inputs kept beside the database and brought forward by the build's delta, function, static and span rows kept per image with one keyed Merkle digest as the link's id, and the spans that count each. Not built - the address and perf maps as patches, the writer's tables built by block, a store that takes a patch, the patch regions' room out of the file (designed here).
date: 2026-11-10
---
# Patch in place

Measured 2026-11-10 on `fn main() { println!("hi {}", 1); }` (about 14,000 reachable
functions) after a one-line body edit: `unit link` 0.30 to 0.37 s, `link inputs` 21 ms in one
run and 650 ms in others, `dev run commit` twice at 110 to 130 ms. The linker's stage was
already a splice. What cost the program was around it. This is the design as built; the
numbers after it are the main lane's to measure.

## The edit path, step by step

| Step | Span | Cost after a one-function edit |
|---|---|---|
| Span tables the code names | `link inputs` | the functions the delta lists are read for their anchors; the workspace's named bodies are each one memo verification (`verified`); the tables whose positions moved are encoded (`made`). A library's bodies are not asked. One call a file the tables name, to check its name. |
| Native inputs | `link delta` | one `stat` a library, as before |
| Function, static and span rows | `link delta` (`rows`, `hashed`) | the delta's functions hashed (`filled_id`); a search of the build's sorted lists for each |
| The link's id | `link delta` | a leaf and about `log2(n)` nodes for each changed row (`blaze_ids::keyed`) |
| Names | `link delta` | every name the image exports (`Built::symbols`): a handful for a program, not its functions |
| Slots of a live image | `link delta` | the patchable (workspace) functions, each a search |
| What running code assumes | `live assumed` | the workspace's items (`crate_hashes` columns), as before |
| Stage and commit | `link patch` | the linker's: the new code, its callers, the cell, and the writer's tables (see Not built) |
| Whole file for the store | `link copy` | the file's length, until the store takes a patch |
| Maps | `link tables` | nothing unless the target directory keeps layout profiles or a `perf` directory; then the whole map (see Not built) |

## Why `link inputs` varied

The guess was that `Storage::end_epoch` frees the `positions` values a link did not read.
The code refutes it: `end_epoch` lets go only of functions declared `lru`
(`Function::EVICTABLE`, `Vtable::unread`), and `positions`, `anchors` and
`declaration_location` are not. What the old `tables` did every link was fetch those memos
for every item of the build, std's included, and encode every table again. A fetch in a new
view takes the cold path (`fetch_cold`, blaze-query `db/function.rs`): one compare when
nothing of the row's durability was written since it was verified, else a summary test, else
a walk of its edges (`verify_claimed`). So the cost was 14,000 rows times whichever of
those the view allowed: 21 ms when every library row passed on the compare, hundreds when a
write since their last verification sent each to its edges. Which write does that between
two warm edits is not decided from the code; the fix makes it not matter and makes it
visible. A library's owner is never fetched while `Storage::durable_view` is the one the
tables were made in, and when it is not, the span's `whole` says "a library input was written
since the last link".

## Design

1. **Link inputs are kept beside the database** (`crates/hir-ty-codegen/v2/linked.rs`,
   `Kept`): per program, found by `Built::id` or `Built::delta.base`, four programs a
   process. Kept: which anchors each function and static names, a count of namers per
   anchor, each anchor's owner, the id of each table's encoding, and a stamp per owner that
   is not durable. No table's bytes are kept. A link applies the delta's functions and
   statics to the counts, encodes the tables named anew, and asks the database again only
   for the watched owners (`positions_stamp`, `declared_stamp`: memos whose one input is
   the positions, so they run only when those are found again). `LinkInputs` is a change of
   a snapshot (`serial`, `base`, `tables`, `removed`); a consumer that holds neither asks
   for every table (`HirFrontend::span_tables`).
2. **File names** are the job's, not the database's, and are in the encodings. The kept
   state holds the name it used for each file and checks each again every link; a name that
   differs makes every table again, said (`whole`).
3. **The delta is the driver's** (`Built::delta`, `blaze-codegen` `driver/delta.rs`). The
   link's rows (`crates/blaze-cargo/src/serve/unit/codegen/kept.rs`, `Rows`) take it when
   the image's last link was of the delta's base: only the listed functions are hashed
   (`filled_id`), found in the build's sorted lists by search. Else every row is made again
   and `link delta`'s `whole` says why: no link yet, another base (a link failed since),
   the build has no base (`Fresh::why`), or the build holds folded groups' bodies, which
   are of no walked instance and so in no delta.
4. **The link's id** (`Rows::given`) is the build's id, the root of a keyed Merkle set over
   every function row, static row and span table id, and the natives. The set
   (`crates/blaze-ids/src/keyed.rs`) is a tree over the keys' bits: empty subtree zeros,
   one entry its leaf, more the hash of the two halves, the root bound to the count. Its
   shape is of the set alone, so the root kept through edits is the root made whole, and
   two sets with one root are the same set by the hash's collision resistance. A sum of
   entry hashes (what `BuiltId` is) updates as cheaply and is not that: `given` relies on
   `BuiltId` only for what the rows do not hold (names, flags), and on the tree for code.
5. **The linker's change is a delta** already: debug names and span tables are upserts
   (`Stage::apply`, `Stage::spans`). So a delta link gives it the names of the delta's
   functions and the changed tables, not all of them, and the cold functions among those
   it adds (all of them when the image is laid out whole).
6. **A refused stage takes the rows back** (`Rows::undo`), so the kept rows always say
   what the image holds and the next delta is against them.
7. The stage places the delta in the room the image kept; a change with no room is linked
   from scratch, named (`Full::NoRoom`, `Full::NoThunk`). The commit is a patch
   (`Image::patch`): the ranges written and the file's identity, a Merkle tree over 4 KiB
   blocks (`blaze_link::digest`), which is also the store's key of the file
   (`compile::linked_key`). The file a client runs is never written in place: clone,
   `pwrite` the ranges, rename. A patched clone is not synced; `Image::verify` refuses a
   torn file (`Refusal::Torn`) for whoever patches a file it did not just write.
8. Live: a dev executable's link is committed into the daemon's code memory
   (`Loaded::patch` -> `Image::commit_into`); the running program switches slots in one
   atomic store. New code never goes where old code was; a changed signature or layout is
   a restart with the reason (`changed_assumptions`).

## Not built, and why

- **Address map and perf map as patches.** Each is one file per build id, written whole
  (`layout.rs` `write_map`, `perf.rs` `write_map`), only when the target directory keeps
  profiles or a `perf` directory. A patch needs a format of fixed-size records in any order
  with tombstones (48 bytes a function: address, size, identity), the last id's file cloned
  and the edit's records written, compacted when half are tombstones; readers sort on load.
  The perf map's nodes hold names and edges and need a log, not slots.
- **The writer's tables by block.** `file.rs` rebuilds each synthetic region (symbols,
  names, unwind header) every stage and compares it with the last in 4 KiB blocks. Building
  only the blocks that can have changed is a change of each format's writer. What each
  costs is now said: a stage's `link write` events sum the bytes and ranges it writes by
  region, writer's part and debug section (`shape::record_writes`).
- **One link for two paths**: built 2026-11-10 ([debug-side-file.md](debug-side-file.md)).
  `artifact::made` lists a program with no hash in its name once, the profile directory's
  file is its copy (`uplifted`), and an image's build id is of its file name, not its path.
- **`blaze why`** names `Full::NoRoom`, `NoThunk`, `Output`, `Layout` on the link line. Why
  the rows or the inputs were made whole is on the spans only (`whole`); a line needs a
  field in `blaze_api::LinkHow`.
- **A store that takes a patch**, and the dev run's whole-file copy: the store lane's.
- **A test binary** still reads every span table (`Generated::spans`) for its execution
  table and its tests' selection.

## The patch room out of the file (designed 2026-11-10, not built)

Hello world's dev executable is 162,021,720 bytes, and at least 134,217,728 of them are the
room of `Class::Patch` and `Class::PatchData` (64 MiB each, `place.rs` `patch_room`). The
room is zeros the digest never hashes (its tree is sparse) and a clone never copies; it
costs where the file is made whole (`Image::file`, `Filed::written`'s `link copy`, a store
put of `Written::Linked`): 162 MB allocated, zeroed and written. Expected once built: about
27.8 MB (162,021,720 less 134,217,728, less nothing else: each used part is rounded up to
64 KiB), a few MB less with the debug info beside it. Measure with `ls -l`, `du`, and
`readelf -lW` (two more `LOAD`s, `MemSiz` past `FileSiz` on the two that end in a patch
region).

- **Why it is not a change of the writers alone.** Every writer gives a region the offset
  `addr - base` (`elf.rs` `offset`), and three things rest on that: a dev run's code memory
  holds the program "as the linker lays out its file (a file offset is an offset here)"
  (`blaze-inproc/src/lib.rs`), the start hook takes an overlay only for "one of the image's
  executable segments at the same file offset", and a live patch names code by that offset
  (`blaze_live::patch`, hashed by the running program). New code of a live patch goes into
  the patch region's room, which every run already maps from the code memory. A file
  without the room has no offset for it.
- **The layout.** Addresses and the order of regions stay. A patch region's unused pages are
  a hole: the file holds its used part rounded up to `max(64 KiB, the next power of two)`,
  and what follows in address order follows it in the file. `offset(region) = addr - base -
  holes before it`, holes whole pages, so offsets stay congruent to addresses. Each hole
  splits its segment: the part before ends with memory past its file bytes (ELF `p_memsz >
  p_filesz`, PE `VirtualSize > SizeOfRawData`), the part after is a segment of its own.
  Growth past the kept extent doubles it and moves what follows in the file: the core
  already writes a region whose offset moved whole (`file.rs` `dirty`), so it is a rewrite
  of the bytes after the hole, amortized over the doubling, never a link from scratch.
- **Mach-O.** A segment of `vmsize > filesize` is zero-filled only when writable, and on
  arm64 an executable page must be in the signed file: `__TEXT`-like patch code can't be
  zero fill. The used part is file bytes (signed); the room is address space the image
  reserves with a `VM_PROT_NONE` segment of no file bytes, as a dev run's reservations are.
- **Who runs code in the room.** Only a dev run, and it maps the room from the daemon's code
  memory, not from the file. A program started from the file runs the used part, which is
  file bytes; its room is never executed.
- **Two coordinates.** The code memory and the live patch keep `addr - base` (the *image
  offset*); the file gets the compact one. `Image::commit_into`, `commit_into_patch` and
  `roll_back_into` translate each write from file to image offset by the holes; `Image::
  patch`, `file`, `verify` and the store's derive ranges stay in file offsets; the digest is
  of the compact file, its length the real one, and the holes are in the headers it hashes.
  `blaze-inproc`'s hook checks a code overlay against the segment's address, not its file
  offset, and `host::write_executable` writes the compact file from the image.
- **Why not in this pass.** It changes the three writers' headers, the dev run's rule
  (`elf/image.rs` `dev_run`: "is all file bytes"), the code memory's contract and the start
  hook's check in running programs, on three systems. Written without a build or a run, a
  mistake is a program that does not start; it needs the linker's tests and a dev run at
  each step.

## Tests

- `crates/blaze-ids/src/keyed.rs`: the refinement
  `a_keyed_set_kept_by_edits_is_the_set_made_whole`, control
  `the_oracle_catches_a_removed_entry_left_in_the_root`, counter
  `one_entry_set_hashes_its_path`.
- `crates/hir-ty-codegen/v2/linked/tests.rs`: the refinement
  `the_link_inputs_kept_are_the_tables_made_whole` (reformat, body edit, function gone or
  back, another table named), control `the_oracle_catches_a_moved_body_not_encoded_again`,
  counter `a_body_edit_among_many_makes_its_own_tables`.
- `crates/blaze-cargo/src/serve/unit/codegen/kept/tests.rs`: the refinement
  `the_rows_kept_by_deltas_are_the_rows_made_whole` (rows, span tables, id and the lists of
  what changed against rows made whole), controls
  `the_oracle_catches_a_row_not_refreshed_for_a_caller` and
  `the_oracle_catches_a_given_id_not_updated_for_a_removed_function`, the undo
  `rows_taken_back_are_the_rows_before`, counter `a_body_edit_among_many_hashes_the_edit`
  (100 and 10,000 functions).
- `crates/blaze-link/src/tests/patch.rs` and `digest.rs`: the file's patch and digest, as
  before (`a_patched_file_behaves_as_a_fresh_link` and its controls).
