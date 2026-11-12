---
status: plan
summary: What a link after a one-function edit costs, step by step, and the design that makes it cost the edit. Built - the link as a patch of its file (ranges written, a Merkle digest of the file), link inputs kept beside the database and brought forward by the build's delta, function, static and span rows kept per image with one keyed Merkle digest as the link's id, and the spans that count each. Not built - the address and perf maps as patches, the writer's tables built by block, the patch regions' room out of the file's length (the compact file, designed here; the room already costs no bytes written, hashed, copied or stored, written 2026-11-11 and not yet built or run).
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
| Span tables the code names | `link inputs` | the functions the delta lists are read for their anchors; the bodies whose positions the engine's changed set names since the last link have their kept tables encoded again (`verified`, `made`). No body is asked for being watched. One call a file the tables name, to check its name. |
| Native inputs | `link delta` | one `stat` a library, as before |
| Function, static and span rows | `link delta` (`rows`, `hashed`) | the delta's functions hashed (`filled_id`); a search of the build's sorted lists for each |
| The link's id | `link delta` | a leaf and about `log2(n)` nodes for each changed row (`blaze_ids::keyed`) |
| Names | `link delta` | every name the image exports (`Built::symbols`): a handful for a program, not its functions |
| Slots of a live image | `link delta` | the patchable (workspace) functions, each a search |
| What running code assumes | `live assumed` | the workspace's items (`crate_hashes` columns), as before |
| Stage and commit | `link patch` | the linker's: the new code, its callers, the cell, and the writer's tables (see Not built) |
| The file for the store | `link copy` | nothing for an edit the store derives; else the bytes of the file's runs, never its length |
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
   anchor, each anchor's owner, the id of each table's encoding, and the kept tables of each
   owner that is not durable (`watched`). No table's bytes are kept. A link applies the
   delta's functions and statics to the counts, encodes the tables named anew, and encodes
   again the kept tables of the watched owners the engine names as changed since the view
   the tables are of (`positions::changed_since`, `declaration_location::changed_since`:
   the changed set, walked from what was written). Until 2026-11-11 it asked a stamp memo
   of every watched owner each link (`positions_stamp`): 4,841 memos verified and 1.9 ms
   for a one-line edit of hello world, since std's bodies read `MEDIUM` inputs and are
   watched; now 0 and 0.17 ms (`link inputs`, 30 edits each). `LinkInputs` is a change of
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
   Built for the edit path's place 2026-11-11 (`Store::place_patched`, `work::Made::Patch`):
   the link hands the loop the ranges it wrote, and the loop, having looked that the path
   still holds the file it placed there, clones that file beside it, writes the ranges and
   renames it on. Before, the image's runs were read whole every link (`link runs`, 21 MB
   for hello world) and written whole at each path. Each place asks for its file's
   write-back as it ends and waits for none, so the next clone finds the file clean.
8. Live: a dev executable's link is committed into the daemon's code memory
   (`Loaded::patch` -> `Image::commit_into`); the running program switches slots in one
   atomic store. New code never goes where old code was; a changed signature or layout is
   a restart with the reason (`changed_assumptions`).

## `.rela.dyn` by slot (ELF, 2026-11-11)

A one-function edit takes the old body's constants out of the slid addresses in one place
and puts the new body's in at another (the patch room). In a table packed in address order
every entry between the two moves: 303 KB of hello world's 442 KB `.rela.dyn` was written
each edit, 95% of the patch, of the store's record of it and of each place's writes.

A kept table (`elf/dynamic.rs`, `KeptRela`) now holds each slid address's `RELATIVE` entry
in a slot that stays its own: an address gone leaves its slot zeroed (`R_*_NONE` at offset
0), an address added takes the lowest empty slot or one more at the run's end, and the
binds' and GOT cells' entries follow the run. `DT_RELACOUNT` is the slots before the first
empty one, so what it counts is all `RELATIVE`; a `RELATIVE` past it and an `R_*_NONE` are
taken by the general path of glibc's and musl's loaders (`elf_machine_rela`, `do_relocs`).
Only an output a dynamic loader relocates is kept so (a shared library, or an executable
with an interpreter). A table made whole (a first link, a link from scratch, a delta of
another synthesis, a run whose empty slots outgrow the region's room) is in address order
with no empty slot, as before. So a patched file's `.rela.dyn` is a history's, not the clean
link's bytes; what a loader applies of it is the same set of entries
(`elf/tests.rs`, `relinks_as_loaded`).

Mach-O needs none of this on a platform with chained fixups (macOS 12 on,
`macho/chained.rs`): a slid slot holds its target and the distance to the next slot of its
16 KiB page, so a changed slot rewrites its 8 bytes and its predecessor's. A platform
without them keeps the rebase opcodes, which `linkedit::rebases` makes whole each link:
not changed here, and not measured (no macOS run in this lane).

## Not built, and why

- **Address map and perf map as patches.** Each is one file per build id, written whole
  (`layout.rs` `write_map`, `perf.rs` `write_map`), only when the target directory keeps
  profiles or a `perf` directory. A patch needs a format of fixed-size records in any order
  with tombstones (48 bytes a function: address, size, identity), the last id's file cloned
  and the edit's records written, compacted when half are tombstones; readers sort on load.
  The perf map's nodes hold names and edges and need a log, not slots.
- **A stage that costs the edit.** Measured 2026-11-12 on hello world (12,071 functions, 30
  one-line body edits, a Linux VM, tree a9325666d): `link patch` is 9.5 ms, of which
  `link stage: lists` 2.9 (each of the core's four lists is copied whole by
  `Owned::splice`, and where a symbol changed every function is scanned for it), `apply the change`
  2.0, `the writer's tables` 1.25 (`.eh_frame_hdr` is made whole: its patch asks for
  `Delta::same_regions`, which no edit that places new code keeps), `the writer's file` 0.8
  (the symbol table, made whole for the same reason) and `dev run commit` 1.2 beside it.
  Each is a pass over the image. The lists need a structure a splice does not copy (blocks
  shared between links, as `CowBytes` is for bytes); the two tables need the slot treatment
  `.rela.dyn` got.
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
- **A test binary** still reads every span table (`Generated::spans`) for its execution
  table and its tests' selection.

## The patch room (2026-11-11; written, not built or run)

Hello world's dev executable is 162,021,720 bytes long, and at least 134,217,728 of them are
the room of `Class::Patch` and `Class::PatchData` (64 MiB each, `place.rs` `patch_room`).

**The design chosen, for ELF, Mach-O and PE alike: the room is a hole.** Addresses, the order
of regions and `offset = addr - base` stay, so the room has file offsets (a splice writes the
code it adds there, a dev run's code memory holds it there, and a live patch names it there)
and no bytes: no step writes, reads, hashes, copies or stores a byte of room nothing used. A
splice fills only what it uses, as before. Every step that made the file at its length is
changed to work on the runs of it that hold bytes:

| Where the room cost | Was, per link that writes the whole file | Now |
|---|---|---|
| The file for the store (`link copy`, `Filed::written`) | the file's length allocated, zeroed and copied | `Written::Runs`: the patch's writes moved, or `Image::file_runs`; the span's `bytes` are the runs' |
| The store's record (`Put`) | the file's length hashed for its content id and chunked (`CHUNK_FROM`) into the pack | the recipe of the runs over the base of holes (`blaze_cas::derived::holes`, 16 KiB of zeros put once): `Kind::Derived`, as an edit's is |
| The store's file of it (`placed/`) and a place from it | the file's length written twice (where it goes and as the store's file), then written back | the base's clone cut to the length (holes) with the runs written; a place of it is a clone, which keeps holes |
| A dev run's code memory (`Image::commit_into`, `write_into`) | the gaps of a file written whole filled with zeros and published: the room's length of memory in the object and an instruction-cache pass over it | `CodeMemory::zero` fills only pages something wrote since the object was made |
| The Mach-O signature (`sign.rs`) | every page of the room gathered and hashed (SHA-256 of 128 MiB) | a page nothing is in takes the zero page's hash (`View::hole`) |

The place of the build that made the file (`Store::place_made`, the file's runs into a file
of holes), the commit's own file (`Truncate`, then `WriteAt` of the extents) and the digest
(a sparse Merkle tree) never touched the room.

What this leaves: the file's *length* is still its addresses' (`ls -l` says 162 MB, `du`
says the content), each segment's file size still spans the hole (`readelf -l` shows the room
as file bytes of `LOAD`s), and Mach-O's signature still holds 32 bytes a 4 KiB page of room
(about 1 MiB). A tool that copies without looking for holes (`cat`, an upload) writes the
zeros. The key of a link is unchanged: what decides the layout (`Output::incremental`) was in
it already.

**Why not the room as memory only (`p_filesz = 0`, `vmsize > filesize`).** The room is in the
middle of its segment (code, room, stubs; data, room, thread-locals), so it splits each into
three; on ELF the room of the code would be anonymous executable memory, which SELinux's
`execmem` refuses where a mapping of a file's hole is ordinary; on Mach-O an executable page
must be in the signed file. And it would save nothing the hole does not: the kernel reads no
disk for a hole.

**Why not no room in the file's offsets (the compact file, below).** It is the better file:
its length is its content. It needs two coordinates (the file's offset and the image's),
segments split at each hole, and what follows a room moved in the file when the room's used
part doubles. It is still the design to build, with the linker's tests and a dev run at each
step; nothing here is in its way, since every consumer above already takes runs.

**Release.** `Output::incremental` stays on for every native profile. `blaze_lazy::allow`
decides only live patching; the room is what makes the next `blaze build` of a release
profile a splice of its file (`Linking::Spliced`), which no profile refuses, and what lets an
image nothing changed keep its file byte for byte (`relink`'s `unchanged`). A release file
without room is a decision of release edit time against a file that copies whole anywhere;
it is not made here.

**`strip`.** Not honoured in the writer (ledger D44's lane). `strip --strip-debug` on an
image fails to load because the tool lays the file out again by its section headers: the
relro `PT_LOAD` ends in memory past its file bytes with file-backed segments after it, and
every offset is an address, neither of which it keeps (read from the writer, `elf/image.rs`;
not run).

### The compact file (designed 2026-11-10, not built)

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

- The room: `crates/blaze-cas/src/tests/place_crash.rs`, the invariant
  `an_image_put_as_its_runs_over_holes_is_placed_writing_its_runs_and_none_of_its_room` and
  its control `the_oracle_catches_an_image_put_whole_with_its_room`;
  `crates/blaze-cas/src/derived.rs` `a_file_of_runs_over_the_base_of_holes_is_a_recipe_of_its_runs_alone`;
  `crates/blaze-inproc/src/tests.rs` `zeroing_fills_only_the_pages_that_were_written`;
  `crates/blaze-link/src/macho/sign.rs` `a_view_with_room_signs_as_the_assembled_file`.

- `crates/blaze-ids/src/keyed.rs`: the refinement
  `a_keyed_set_kept_by_edits_is_the_set_made_whole`, control
  `the_oracle_catches_a_removed_entry_left_in_the_root`, counter
  `one_entry_set_hashes_its_path`.
- `crates/hir-ty-codegen/v2/linked/tests.rs`: the refinement
  `the_link_inputs_kept_are_the_tables_made_whole` (reformat, body edit, function gone or
  back, another table named), control `the_oracle_catches_a_moved_body_not_encoded_again`,
  counter `a_body_edit_among_many_makes_its_own_tables` (100 and 2,000 functions) and its
  control `the_oracle_catches_a_link_that_asks_of_every_body`.
- `crates/blaze-cargo/src/serve/unit/codegen/kept/tests.rs`: the refinement
  `the_rows_kept_by_deltas_are_the_rows_made_whole` (rows, span tables, id and the lists of
  what changed against rows made whole), controls
  `the_oracle_catches_a_row_not_refreshed_for_a_caller` and
  `the_oracle_catches_a_given_id_not_updated_for_a_removed_function`, the undo
  `rows_taken_back_are_the_rows_before`, counter `a_body_edit_among_many_hashes_the_edit`
  (100 and 10,000 functions).
- `crates/blaze-link/src/tests/patch.rs` and `digest.rs`: the file's patch and digest, as
  before (`a_patched_file_behaves_as_a_fresh_link` and its controls).
- `crates/blaze-link/src/elf/tests.rs`: `.rela.dyn` by slot,
  `a_relink_from_another_synthesis_is_built_and_one_of_another_length_takes_a_slot`, the
  counter `an_edit_that_moves_slid_addresses_writes_their_entries_alone` (1,000 and 16,000
  functions) and its control `the_oracle_catches_a_table_made_whole_for_an_address_moved`.
