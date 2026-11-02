---
status: plan
summary: Speed of the Mach-O `__unwind_info` builder (`crates/blaze-link/src/macho/unwind.rs`), which runs on every full and incremental link - lld's and ld64's way of building it, how the builder compared before the 2026-10-29 pass (allocations, passes, bytes per entry), what changed, and what it would take to shrink Rust code's tables further.
date: 2026-10-29
---
# `__unwind_info` (speed-link-edge-macho-unwind)

`unwind::build` makes `__unwind_info` from the placed image's `__eh_frame`, the C objects'
`__compact_unwind` and `Placed::functions`. `Writer::synthesize` calls it on every link and
again after each in-place relink, so its cost is paid on every body edit.

## The benchmark

lld's `UnwindInfoSection::finalize` (and ld64/ld-prime's `UnwindInfoAtom`, which writes the same
pages). Its steps:
1. One sorted array of entries.
2. Encoding frequencies counted in a hash map, with up to 127 common encodings.
3. Second-level pages split greedily: a compressed page holds 4-byte entries (a 24-bit function
   offset from the page's first entry and an 8-bit encoding index) in 4096 bytes, with up to
   256 encoding indexes between the common ones and the page's own. Where a compressed page
   would hold fewer than a regular page's 511 entries and more entries follow, the page is
   regular instead.
4. The output written once, at its final size.

mold and sold write the same format the same way. ld64 does not split pages differently.

## Before

| | per link | per function |
| --- | --- | --- |
| `__eh_frame` | gimli's full parse of every CIE and FDE (augmentation data, LSDA pointers, instructions), each FDE then inserted into a `BTreeMap` | one B-tree insertion |
| starts | every compact entry, FDE, function and range end pushed into one `Vec<Entry>` (24 bytes each), then sorted with `sort_by_key` | about 2 entries, one O(n log n) sort over all of them |
| binds | a `BTreeMap` of the binds in each `__compact_unwind` region, built per region | |
| common encodings | a `BTreeMap<u32, u64>` over every row, then every distinct encoding sorted. For Rust code every DWARF-mode row is a distinct encoding, so this was a sort of n entries. | one B-tree insertion |
| pages | regular pages only: 8 bytes an entry | 8 bytes |
| output | a `Vec` grown by `extend_from_slice` (exactly preallocated) | |

That was 6 passes over every entry and 3 heap structures with a node per entry.

## After

- **`__eh_frame`**: one linear scan of the records. Of each CIE it reads only what gimli checks:
  version 1, 3 or 4, the `z`, `L`, `P`, `R` and `S` augmentations, and the FDE pointer encoding.
  Of each FDE it reads only `pc_begin` and `pc_range`. CIEs go in a small `Vec` sorted by
  offset, binary-searched by each FDE's CIE pointer. FDEs go in one column of 24-byte entries,
  sorted only if `__eh_frame` was out of address order, then deduplicated so the first at an
  address wins. gimli is used only in the tests.
- **Starts**: four sorted columns (compact entries, FDEs, `Placed::functions`, range ends),
  each sorted only when it is not already in order (the core sorts `functions`), merged in one
  pass with a 4-way minimum. Merging consecutive equal encodings and checking offsets happen in
  that same pass. DWARF-mode compact entries find their FDE by binary search in the FDE
  column. Personality binds are found by binary search in `Placed::binds`, which the core sorts.
- **Common encodings**: an open-addressed table with Fibonacci hashing, 16-byte slots, at most
  half full. DWARF-mode encodings never enter it: each one names its own FDE, so it belongs to
  a single entry, is never common and is always new to its page. For Rust code the table is
  therefore a handful of entries. The same slot also records the encoding's page-local index,
  stamped with the page being laid out, so a lookup during page layout is one probe and no table
  is cleared per page. Only encodings with more than one entry are made common: one with a
  single entry is no smaller in the common table than in the page's own.
- **Pages**: split exactly as lld does: compressed where a compressed page holds at least 511
  entries or reaches the end, regular otherwise. A compressed entry is 4 bytes, plus 4 bytes for
  each encoding a page needs of its own.
- **Output**: sizes are settled first (page split, then section offsets), then everything is
  written in place into one zeroed buffer of exactly that size. The second pass over a
  compressed page repeats its page-local encoding choices with a fresh stamp.
- `bound` grows by 4 bytes (one compressed page header over a regular one), which proves an
  upper bound for either page kind. Its signature is unchanged.

| | per link | per function |
| --- | --- | --- |
| allocations | the FDE, compact-entry, end, row and LSDA columns, one table of distinct non-DWARF encodings, the page list (n/511 entries) | none |
| passes | `__eh_frame` scan, compact scan, one merge, one count, one page split, one write | |
| bytes per entry | compressed: 4 for an entry whose encoding is common or already used on its page, 8 for one that is new to its page; regular: 8 | |

For C and Objective-C objects, whose entries share a few compact encodings, the second level
is about 2 times smaller than before (4 bytes an entry instead of 8), as ld64's. For blaze's
own Rust code, where every function is described by its FDE, encodings are distinct, so pages
stay at 8 bytes an entry and lld's rule picks regular pages once a page has used up its 129 to
255 page-local indexes, exactly as ld64 and lld do for DWARF-mode functions. The build is
still one linear pass with no per-function allocation.

## What would beat it

Rust functions' tables would shrink to 4 bytes an entry only if codegen emitted compact
encodings (`UNWIND_ARM64_MODE_FRAME`/`FRAMELESS`, x86-64 `RBP_FRAME`) for the frames that have
one, as LLVM does for C, instead of an FDE for every function. Then most functions would share
a few common encodings and `__eh_frame` would shrink too. That change belongs to codegen and
the core's `Unwind` record, not this file.

An in-place relink still rebuilds the whole table. That is linear and allocation-free per
function; patching only the changed pages would need the core to say which functions moved.

## Tests

- The existing tests are kept, with a reader that decodes compressed pages as libunwind does.
- `the_new_tables_answer_as_the_regular_page_builder_did`: over 48 seeded images (up to 4000
  functions, FDEs out of order and duplicated, dead entries, personalities held by address or
  by bind, LSDAs, gaps past 24 bits, unsorted functions and binds), the old builder, kept in
  the test module, and the new one give the same entries, personalities, LSDA index and
  sentinel. Their lookups agree at every entry boundary, one byte either side of it, and the
  sentinel. Between boundaries the answer is constant. Errors are identical, the output is
  deterministic, within `bound` and at most 4 bytes longer than the old output.
- `the_fde_scan_reads_what_gimli_reads` (seeded) and
  `hand_built_records_scan_as_gimli_parses_them` (absolute, `udata4`, `zPLRS` with an indirect
  personality, version 3, a dead FDE, a duplicate, a bad CIE pointer and a bad version) check
  the scan against gimli's parse.
- `many_functions_span_pages` (compressed, 4 bytes an entry) and
  `a_page_past_24_bits_of_function_offsets_ends`.
