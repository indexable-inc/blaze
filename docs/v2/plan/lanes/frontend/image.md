---
status: plan
summary: blaze v2's daemon state is one mapped image (crates/blaze-image) opened by one BLAKE3 of its spine, every 64 KiB group checked by a hardware CRC32C on first touch and named when corrupt, with symbols and interned values as mapped tables whose u32 IDs survive restarts and a GC that never renumbers a live one.
date: 2026-10-29
---
# One mapped image, stable IDs, lazy integrity (frontend-image)

The first three bullets of [restore.md](../../design/restore.md) and item 11 of
[frontend.md](../../design/frontend.md): the state is one mapped image used in place; IDs are
stable across restarts; integrity is checked lazily, and a corrupt part is dropped, recomputed
and named; interned values are freed by the image's GC.

## What exists

- **`crates/blaze-image`**: the file format and its reader and writer, with no IO of its own
  (the write is `blaze_core::Effect::Write`: temp file, fsync, rename, directory fsync).
  - The *spine* fills group 0: magic, layout schema, the BLAKE3 of the spine, the file length,
    the group size (64 KiB by default, 4 KiB to 16 MiB), the section table (ID, owner's schema,
    offset, length, groups), and the CRCs of the CRC table's own groups. Opening reads the
    spine and nothing else: one hash over a few KiB.
  - Sections start on group boundaries, so a group belongs to one section and a section's
    corruption never takes a neighbor with it.
  - The *CRC table* (one CRC32C a group) is last. Its groups are checked against the spine, so
    the spine's hash covers every byte through two levels.
  - `Image::group`, `range`, `slice::<T>` and `value::<T>` check each group they touch the
    first time, with one relaxed atomic bit a group, so a group already checked costs one
    load. A failure is `Corrupt { section, group, cause }`, remembered so later reads fail the
    same way, and its message names the section ("the database's memos (section 0x100),
    group 7, is corrupt: its CRC32C is ..., not ...").
  - CRC32C uses SSE4.2 `crc32q` on x86-64 and ARMv8 `crc32cx` on aarch64. It runs three
    interleaved streams of 4 KiB joined by a GF(2) shift table (the method of Intel's
    `crc_pcl`), with slicing-by-8 as the portable path, checked against a bitwise reference.
- **`intern`**: `Symbol` is a `NonZeroU32` ID.
  - IDs `1..=STATIC` are the compiled-in texts (the declared names, then each list's), so
    `sym::self_` is a constant and its text is one load.
  - Every other symbol is in the table of `crates/intern/src/table.rs`: a *base* restored from
    the image and read in place (texts, starts, order keys, live bits, a SwissTable-style
    index of 8-tag groups, the free list) and an *overlay* for what this process makes (the
    lock-free arena store it had, plus a column from ID to record).
  - `restore_symbols` checks the symbol section whole (every value depends on it) and
    validates its columns in O(IDs + slots). The section's schema includes every compiled-in
    text, so a blaze with another symbol list refuses the section instead of misreading IDs.
  - `write_symbols(&Live, ..)` is the GC: it keeps what the other sections marked, each at its
    own ID, and frees the rest. A freed ID goes to the next process's new values, lowest first,
    never to a live value, and nothing is renumbered.
  - `Mapped<T>` is the same table for slices of a plain record (`SectionId::interned(tag)`),
    for what `Interned<T>` becomes once persisted values name it.
- **`blaze-format`**: `Sym(u32)` is a symbol in a persisted value. `Record::symbols` walks
  every `Sym` a value reaches (inline, through `Rel`, through `Slice`, through a record that
  holds a slice of itself); `symbols_of::<R>` and `Persist::symbols` are what an owner marks
  live.
- **`blaze-graph/src/history.rs`**: the plane's history (`Estimates::encode`) as the image's
  `PLANE_HISTORY` section instead of a file of its own.

## Hooks asked of other lanes

- **db** (`v2/requests/db.md`, "frontend-image: the daemon image on `blaze-image`"):
  - move `blaze-query`'s hand-laid image onto sections;
  - turn the "image is corrupt" panics into `Corrupt`, with the key recomputed and the error
    reported;
  - check `intern::restored_from()` before using sections;
  - mark symbols at `write_image`;
  - store `Sym` instead of text in persisted HIR and solver values;
  - make salsa interned values and item IDs mapped tables with the same GC.
- **cargo** (`v2/requests/cargo.md`, "frontend-image: one daemon image"): restore symbols
  first, keep the plane history in the image, and write one image.

## The state of the art, and where this stands

- **CRC32C throughput.** Intel's `crc_pcl` (three-way interleaved `crc32q`, about 3 bytes a
  cycle) and the ARMv8 `crc32cx` equivalent; this is the same method. A 64 KiB group is about
  20 µs at 3 GB/s single-stream, and only touched groups pay it.
- **Opening in place.** LMDB maps its B-tree and reads in place with no load step, and its
  meta page is the one thing read at open. Here the spine plays that role, and lazy per-group
  CRCs give what LMDB leaves to the filesystem.
- **Mapped hash index.** SwissTable (Abseil, hashbrown) groups of tag bytes, here read only
  and at most half full, so a probe ends within one or two groups.
- **Stable IDs across restarts.** rustc's incremental cache maps `DefPathHash` to IDs by
  hashing and remapping on load. Here no remapping happens: the ID in a persisted value is the
  index into the mapped table. The cost is the GC's free list, which keeps IDs dense.

## What each test proves

- **`crates/blaze-image/src/tests.rs`**
  - `open_reads_only_the_spine`: opening touches the spine's bytes and none of a section's
    (bytes touched are counted).
  - `a_read_checks_only_its_groups`: a read checks the groups it covers and no others.
  - `every_corrupted_group_is_found_and_named`: each group of every section, corrupted alone,
    fails on first touch with its own section and group, and only reads of that group fail.
  - `padding_is_checked`, `a_corrupt_crc_table_group_is_named`, `a_changed_spine_is_refused`:
    no byte of the file is outside a check.
  - `a_torn_write_is_the_old_image_or_refused`: every truncation and every prefix of a new
    image over an old one opens as the old image or fails, never as a mix.
  - `an_image_written_over_in_place_never_reads_old_bytes`: an image written over in place
    never reads old bytes as new.
  - `first_reads_race`: racing first reads of one group agree.
  - `crc.rs` tests: the hardware and portable CRC32C equal the bitwise reference on every
    length, offset and group size, and on the known vectors.
- **`crates/intern/src/table/tests.rs`**
  - `ids_are_equal_after_write_and_reopen`: three generations of write and restore keep every
    ID, text and order key.
  - `gc_never_renumbers_a_live_value`: eight generations with random marks. Live values keep
    their IDs, freed IDs read as nothing, new values take freed IDs lowest first and never a
    live one.
  - `a_freed_value_made_again_takes_a_new_id` and `a_lost_id_panics`: reading a freed ID is a
    loud bug, never an empty value.
  - `a_restore_never_accepts_a_table_it_cannot_read`: 4000 random corruptions of a section,
    with CRCs recomputed so only the validator stands. Each is refused or gives a table whose
    every read is in bounds and UTF-8.
  - `a_corrupt_table_is_dropped_whole`: one flipped byte fails the restore, naming the symbol
    section.
  - `a_restored_table_interns_from_many_threads`: no ID is given twice.
  - `value_hash_is_deterministic_and_spreads`: the index hash is the format's, the same in
    every process.
- **`crates/intern/src/symbol.rs`**
  - `every_compiled_in_text_interns_to_itself`: every compiled-in text interns to its constant
    ID.
  - `a_restore_after_interning_is_refused` and `another_symbol_list_is_refused`: a table that
    already gave IDs, or an image from a blaze with another symbol list, is never restored
    over.
- **`crates/intern/src/mapped.rs`**: values keep their IDs across an image and are read
  aligned (16-byte records), and a section of another element type is refused.
- **`crates/blaze-format/src/tests.rs`**: `symbols_are_found_wherever_a_value_holds_them`.
- **`crates/blaze-graph/src/history.rs`**: the history survives an image, and a corrupt,
  missing or undecodable one is refused, naming the section.

## Costs written down

- Restoring symbols is eager: the whole section's CRC plus one pass over starts, live bits,
  the free list and the index, roughly 1 ms per 100k symbols. It is paid once, because every
  value names symbols. A corrupt symbol section therefore means a cold start of everything
  that names symbols, said in the error.
- A symbol made in this process costs a 16-byte header and an 8-byte column slot beside its
  text. A restored one costs 12 bytes (start, order key), a live bit and about 5 to 10 bytes
  of index.
- `Symbol` is 4 bytes, down from 8. `as_str` of a compiled-in symbol is one load; of another,
  a `OnceLock` load, a range check, one live bit and two starts.
