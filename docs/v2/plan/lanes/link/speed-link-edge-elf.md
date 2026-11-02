---
status: plan
summary: The ELF writer and readers of blaze-link against mold and wild, phase by phase, before and after speed-link-edge-elf.
date: 2026-10-29
---
# speed-link-edge-elf: the ELF format of blaze-link

Part of [link](../link.md). Code: `crates/blaze-link/src/elf.rs`, `crates/blaze-link/src/elf/`
(`read.rs` and `read/` the native readers, `dynamic.rs` the synthetic regions, `image.rs` the
headers and symbol table, `native.rs` `Writer::apply`). Nothing here was built or run (v2 rule);
the numbers are counts read off the code, not timings.

## The benchmarks

- **mold** (the fastest production ELF linker): one parallel task per input file for parsing,
  parallel symbol resolution over a concurrent hash map, `.gnu.hash` built in one pass with the
  symbols bucket-sorted, string tables laid out by prefix sums and filled in parallel, `.strtab`
  names not deduplicated, `.rela.dyn` sorted with a parallel sort.
- **wild** (the fastest incremental-minded ELF linker): zero-copy reads of mapped inputs (sections
  and names are slices of the file), a two-phase layout (sizes first from counts, then addresses,
  then parallel writing of every part straight into the output buffer), and a design aimed at
  incremental relinks.

## Phase by phase

| phase | mold / wild | before | after |
|---|---|---|---|
| parse | one task per file; sections and names borrowed from the map | one `Vec<u8>` per loaded section and one `String` per symbol and section name (model types); the 3 of 4 archive members not 8-aligned copied whole into a zeroed buffer first; a `String` clone of the section name per relocation section; fixups gathered in a temporary `Vec`, then copied | the alignment copy only when `object` needs it (no copy once the `unaligned` feature is in, request filed), and without zero-filling; fixups pushed straight into their section; no name clone; size symbols in an FxHash map. The per-section `Vec` and per-name `String` remain: they are the model's types (request 5 and 7 in `v2/requests/link.md`) |
| resolve (what the reader feeds) | archive symbol index read in place; members parsed only when pulled | members kept in a `BTreeMap` by start; each archive symbol parsed its member's header again; without a symbol table every member read one after another | members found by binary search on their start (no map); a member's run of symbols parses its header once; without a symbol table the members are read in parallel, in member order |
| layout (synthetic sizes) | exact sizes from counts before placement | `sizes` from `Counts`: O(1) per region, no allocation but the list | unchanged (already wild's shape) |
| copy | output buffer written in parallel | the core's (not ELF) | the core's |
| relocate (`Writer::apply` of `Apply::Native`) | a type `match` per relocation | a `(arch, r_type)` match, no allocation on success, `String` only for an error | unchanged (already at the bar; these types are rare) |
| synthesize | each table built once, in parallel where large | every table built **twice** per link (`synthesize`, then again inside `file`) and copied once more (`to_vec` of each); `Strings` hashed each name with SipHash and allocated a `String` per name; versions found by linear scans; `--as-needed` an O(libraries × imports) scan; exports bucket-ordered by a comparison sort; `.rela.dyn` built and sorted serially; `.eh_frame_hdr` parsed each FDE's CIE again; `.symtab`/`.strtab` built serially through the SipHash map, then copied into the file's tail | each table built once and moved, not copied: `file` reads them back from `Placed::contents` (`.gnu.version_r`'s `sh_info` from its `vn_next` chain); string tables FxHash-probed once each (entry API), borrowing the image's names; versions one FxHash probe per import; `--as-needed` one pass; `.gnu.hash` as mold builds it (hashes once, in parallel past 4,096 exports, then a stable counting sort into buckets: O(n), the bytes the stable sort gave); `.rela.dyn` read in parallel per chunk into a buffer of its exact size, sorted only when the input isn't (the core's is), with a parallel sort past 4,096, and encoded in parallel; `.eh_frame_hdr` takes each CIE from where the walk met it; `.symtab`/`.strtab` laid out by prefix sums over chunks of 4,096 functions and filled in parallel straight into the tail, which is allocated once at its exact size; section headers written in place |

Everything stays deterministic: parallel parts write disjoint, precomputed ranges; sorts are of
totally ordered items whose equal items are identical; the first error in input order is the
one reported (`dynamic::first_error`). One output change: `.strtab` no longer shares a name
between a function and an export of the same name (mold doesn't either); every other byte is as
before.

## After the zero-copy contract (speed-link-edge-elf-input)

The model took requests 5 and 7 (`Data::Input`, `Name` in one `Names` arena per file, fixups
and group members as runs of one column per object, versions by index), and the readers moved
onto it. Counted off the code, per input:

| input | before (after speed-link-edge-elf) | after |
|---|---|---|
| relocatable object | per loaded section one `Vec<u8>` copy of its bytes and one `String` name; per symbol one `String`; per relocated section one fixup `Vec`; per group one `Vec` of members and one `String` | **15 allocations, whatever its size**: the sections, symbols, fixups, groups and group-members columns, the names arena, the relocation-section order, and four buffers per string table (`.shstrtab`, `.strtab`: the NUL bitset, its rank counts, the string slots, the pushed strings). Each is sized once from the section headers. One or two more when relocations add symbols (`TLSLD`, `GOTPC*`, `SIZE*`), and the `SIZE*` map's own |
| archive with an index | per member one `String`, per index entry one `String` | **7 allocations**: the arena, the members, the index, and the index's string table (4), sized in a first pass over the member headers and the index |
| archive without one (lld's path) | per member one `String` name for errors, its defined names as `String`s | per member the offsets of its defined names and its `.strtab`'s 4 table buffers; the member is named only in an error. The arena grows by doubling here (log *n* reallocations), as `Names` has no `reserve` |
| shared object | per symbol one `String` name and one `Option<String>` version | per symbol nothing: the symbols, arena, versions (one `String` per distinct version, a few dozen in glibc) and slot columns, and `.dynstr`'s 4 table buffers |

- **Bytes per element.** A name is 8 bytes (`Name`) instead of a 24-byte `String` plus its heap
  block. A section's fixups are a `Range<u32>` (8 bytes) instead of a 24-byte `Vec`. Its data is
  a 16-byte range of the input instead of a heap copy as long as the section.
- **The arena** holds each string of a string table once, whole, the first time any offset in it
  is named. Every name is a suffix of it, so tail-merged tables (`.text` inside `.rela.text`)
  cost nothing extra, and a hostile table naming every offset of one long string still costs at
  most its own bytes. The arena is at most the string tables' bytes plus
  `_GLOBAL_OFFSET_TABLE_`. A string is found by the rank of the NUL that ends it (a bitset with
  a count per 64 bytes): direct indexing, no hash, so no input can make a lookup slow.
- **No alignment copy is left.** `object` is built with `unaligned` (asserted at compile time in
  `read.rs`), so an archive member is read in place at its 2-byte alignment.
- **Against mold and wild.** Sections and relocations are now read in place, as wild reads them.
  Names are copied once into the arena: one `memcpy` of each distinct string, at most the table's
  bytes, where mold keeps a `string_view` into the mapping. The extra work is one pass over each
  string table to build the NUL bitset, and the UTF-8 check of each string pushed, because
  `Names` is a `String`.

## Where it still falls short, and what it would take

- **The string-table pass is scalar**: one compare per byte. A 64-byte compare-and-movemask
  (`pcmpeqb`/`pmovmskb`, NEON `cmeq` and a narrowing shift, as simdjson finds structural
  characters) would build each bitset word in a few instructions instead of 64. That is about
  10× faster on the pass, which runs once per table byte. It would also have to be checked
  against the scalar loop as `blaze-cas`'s `chunk.rs` paths are. Mapping names straight from
  the input, with no arena at all, would need `Name` to be able to point into the file's bytes,
  which is a model change.
- **`Names::reserve`** would let the no-index archive path size its arena once, as the other
  paths do.
- **Parallel parsing.** The core calls `Writer::read` once per input in a loop and pulls archive
  members one at a time (request 7, item 3). The readers are pure functions of their bytes, so
  this is the core's change alone.
- **Incremental synthesis.** Each link reruns the tables whole. They are now linear and parallel,
  but an edit that moves one function should cost O(change) (wild's aim). It needs the previous
  link's tables and what changed passed in (request 7, item 5).
- **`.eh_frame_hdr` parsing** is still a sequential walk through gimli (length-prefixed
  records). A hand-written scan that reads only each FDE's length, CIE pointer and `pc_begin`
  would be faster. Better still, the core could hand over Rust functions' FDE addresses, which
  it knows (`Function::fde`), and leave only native FDEs to parse.
- **Packed relative relocations** (`DT_RELR`, `-z pack-relative-relocs`) would shrink `.rela.dyn`
  about 8× and the loader's work with it, as mold and lld offer. rustc's link line doesn't ask
  for it, so it isn't done; it would be an opt-in output change.

## Tests added (`elf/tests.rs`)

- `many_exports_hash_as_the_model_does`: 12,288 exports (past the parallel threshold):
  `.gnu.hash` equals the old stable-sort build byte for byte, `.dynsym` holds the exports in its
  bucket order, `.gnu.version` marks them global, and `object`'s hash lookup finds every one.
- `many_slid_addresses_are_relocated_as_the_model_does`: 12,305 slid addresses, given sorted and
  shuffled. `.rela.dyn` equals the sequential, sorted model both times, `DT_RELACOUNT` counts
  them, and a text relocation is still an error.
- `many_functions_are_named_in_the_symbol_table`: 8,292 functions over three chunks, some
  unnamed and one with an empty name. Read back with `object`, every one is in order with its
  name, value, size and section. `sh_info` is the first global, and the export and import
  follow.
- `eh_frame_hdr_indexes_fdes_of_every_cie`: 40 FDEs of two CIEs, out of order; the table holds
  all of them, sorted.
- `an_executable_reads_back` now also checks `.gnu.version_r`'s `sh_info`.

## Tests added by speed-link-edge-elf-input (`elf/read/tests.rs`)

- `agrees`, a reference that reads the same input straight through `object`'s ELF reader, as
  the reader did before it stopped copying, and checks:
  - each loaded section is a `Data::Input` holding exactly the slice the old copy held;
  - each section and symbol name is the string it used to allocate;
  - each group is its members;
  - each section has as many fixups as its relocation sections hold;
  - the fixup and group runs tile their columns;
  - the arena equals one built the slow way, by linear scans.

  It runs on an archive member read at its offset, and on:
  - `every_range_holds_what_the_copy_held_at_any_alignment`: two objects at every shift 0 to 7;
  - `relocation_sections_of_one_section_extend_its_run`: two relocation sections pointed at
    one section, in and out of the sections' order;
  - `many_groups_and_fixups_tile_their_columns`: 300 functions, each with its own section and
    group.
- `a_string_table_pushes_each_string_once`, `a_string_table_past_a_word_finds_its_strings`:
  tail-merged names, names inside a string that is not wholly UTF-8, a missing NUL, and
  strings across the bitset's words, named in reverse.
- `reading_allocates_nothing_per_symbol_section_or_fixup`: an object of 4 functions and one of
  400 make the same number of allocations, and so do archive indexes of 3 and 600 symbols.
