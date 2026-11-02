---
status: plan
summary: The Mach-O object and archive readers (`macho/input.rs`, `macho/archive.rs`) against mold, wild, ld-prime and lld; what they cost before and after, and what the contract still forces.
date: 2026-10-29
---

# Mach-O object and archive reading, at the speed of the fastest linkers

Paths: `crates/blaze-link/src/macho/input.rs`, `crates/blaze-link/src/macho/archive.rs`. Both
run on every cold link, once per C object and once per archive (and once per archive member a
link pulls), so a large crate graph runs them thousands of times (libc-like crates,
compiler-builtins, ring, openssl).

`n` is the object's nlist entries, `r` its relocations, `a` its atoms, `S` its Mach-O sections,
`b` the symbols that bound an atom in a section, `F` the FDEs of `__eh_frame`, `C` its CIEs,
`m` an archive's members, `k` its index entries.

## The benchmark

- **mold/sold** (`macho/input-files.cc`): the nlist, string and relocation tables are borrowed
  from the mapped file; subsections are split by sorting the symbols by address once; a
  relocation's atom is a binary search of the sorted subsection starts; one `Vec` of
  relocations per subsection, sized once.
- **wild**: section bytes and names are never copied; every per-object table is a column
  indexed by `u32`, preallocated from counts in the headers.
- **lld's Mach-O `ObjFile`**: `parseSymbols` sorts each section's symbol indexes by address
  once and splits subsections from that; `findContainingSubsection` is a binary search.
- **Apple's ld-prime**: objects parsed in parallel, one pass each, tables read in place.

So the target: tables borrowed, one pass over the symbol table into preallocated columns, atoms
from one sort, every address-to-atom lookup a binary search, relocations decoded in place with a
tag `match`, `__eh_frame` scanned once, allocations a constant per object plus what the contract
forces per atom and per symbol.

## Objects (`input.rs`)

| | before | after |
|---|---|---|
| nlist table | a `Vec<&Nlist>` of the table (8 B a symbol) | borrowed `&[Nlist]` from the file |
| name column | grown `Vec<&[u8]>` | `reserve_exact(n)`, one allocation |
| atom starts and bounds | two `Vec<u64>` per Mach-O section (`2S` allocations), each sorted | one `(section, offset)` sort for the file, then two columns of the whole file (`bounds`, `starts`), each section a `Range<u32>`/`first..first+atoms` of them: 3 allocations, 8 B a bound and 8 B an atom |
| a symbol's size | linear scan of its section's bounds: `O(b)` a symbol, `O(b²)` a section | binary search: `O(log b)` |
| atom of an offset | binary search of the section's starts | the same, over its range of the one column |
| nlist → symbol map | `Vec<Option<u32>>`, 8 B a symbol | `Vec<u32>` with a `u32::MAX` sentinel, 4 B |
| section symbols | `Vec<Option<u32>>`, 8 B an atom | `Vec<u32>`, 4 B |
| `symbols` | grown from empty | `with_capacity(n)` |
| relocation table | copied into a `Vec<RelocationInfo>` per section (one allocation and 12 B a relocation) | read in place: one branch-light scan for `R_SCATTERED`, each entry decoded where a reader takes it; the CPU picked once per section, not per relocation |
| decoding into `Apply` | `u8` tag `match` on `(r_type, r_pcrel, r_length)` | unchanged: already a tag match, no `dyn`, no `String` but on the error path |
| fixups | pushed into each atom's `Vec` (amortized growth: up to `log` reallocations an atom), then each atom's `Vec` sorted | pushed into one column sized from `r` up front, one stable sort by `(atom, offset)`, each atom's run collected with its exact length: one allocation an atom with fixups, none reallocated |
| `__eh_frame` covered fields | `Vec` of the atom's fixup offsets, `contains` per field: `O(F·r)` | sorted, binary search: `O(F log r)` |
| `__eh_frame` FDE → CIE | linear scan of the CIEs: `O(F·C)` | binary search (CIEs are seen in offset order): `O(F log C)` |
| `__eh_frame` pointer → section | linear scan of the sections per field: `O(F·S)` | sections sorted by address once, when the first uncovered pc-relative pointer needs it, then binary search: `O(F log S)` |
| `__eh_frame` error text | a `format!` of the section's name on every call, error or not | formatted only on an error |
| `LC_LINKER_OPTION` | a `Vec<&str>` then `join` | one `String`, sized from the command |

Passes over the nlist table: two (atom bounds, then symbols), as lld and mold make (the bounds
must be known before any symbol's atom and size). Each is a straight walk over the borrowed
table and the name column.

Error-path behaviour kept the same, with one deliberate addition: an object whose non-empty
sections overlap in address is refused when its `__eh_frame` has a pc-relative pointer without a
relocation ("sections X and Y overlap"), since which atom an address names would be ambiguous.
Before, the first section in file order was taken. clang and `ld -r` never write such objects.

### On the zero-copy contract (second pass, `speed-link-edge-macho-input2`)

`model.rs` now has the reader contract this lane asked for (`v2/requests/link.md` items 5, 5a,
7), and the reader is on it:

| per | before this pass | after |
|---|---|---|
| atom | a `String` clone of `SEG,SECT`, a `Vec<u8>` copy of its bytes, a `Vec<NativeFixup>` when it has fixups | nothing: `Section::name` is the Mach-O section's one `Name` (8 B, `Copy`), `Data::Input(offset + start..offset + end)` a range of the input (the section's `file_range` offset), `Section::fixups` a `Range<u32>` run of `Object::fixups` |
| Mach-O section | a `format!` of `SEG,SECT` | one push into the arena, through one reused 33-byte buffer |
| symbol | a `String` (`core_name`) | one `Name` pushed into the arena (`push_core_name`); a section symbol shares the one `""` name |
| object | the columns of the first pass | the same, plus the arena (sized once from the name column: every core name is at most a byte longer than its Mach-O name, every `SEG,SECT` at most 33 bytes, so it never grows) and one `Vec<NativeFixup>` collected from the sorted `(atom, fixup)` column |

Bytes copied per object: none (was every atom's, and the whole file when its slice wasn't
8-aligned, as an archive member's often isn't). `object` is built with feature `unaligned`, so
every Mach-O struct has alignment 1 and is read in place at any address; the reader's own
aligned copy is gone, as is `macho.rs`'s.

The kind (`SectionKind`) moves into a section's last atom rather than being cloned into each:
a `Class::Named` holds a `String`, so only the other atoms of a split named section clone it.

What still allocates per item, outside these files (filed with the parent
`speed-link-edge-macho`, in `v2/requests/speed-link-edge-macho.md`):

- `macho.rs` `push_core_name`: a `format!("\x01{text}")` for every name without a leading
  underscore, which is every assembler-local label clang keeps (`ltmp0`…, `l_…`). The fix is a
  push of two parts into one `Name` (`Names::push_parts(&["\x01", text])` in `model.rs`, or a
  scratch buffer the caller passes in).
- `macho.rs` `classify`: a `format!` for each `Class::Named` section (`__LD,__compact_unwind` in
  every C object, `__thread_init`, `#[link_section]` ones), and a clone per extra atom of a split
  one. The fix is a model change: `Class::Named`'s name as an interned id or `&'static str`/`Arc<str>`.

With those, an object read allocates a constant number of times (the columns, the arena, the
object's `sections`, `symbols` and `fixups`), as wild does; it already does for any object whose
symbols all start with `_` and that has no named sections.

## Archives (`archive.rs`)

| | before | after |
|---|---|---|
| `__.SYMDEF` / GNU `/` index | read in place through `object` | unchanged |
| members | listed in place; one `String` a member name (the contract's `Member::name`) | unchanged |
| index entry → member | `ArchiveFile::member(offset)` re-parsed the member's header (and its `#1/` name) for every index entry, then a binary search of the members' data offsets | binary search of the members' header offsets, taken in place from `ArchiveMember::header()` while listing: no parse per entry, `O(log m)` |
| index `Vec` | grown from empty | `with_capacity` from the iterator's exact `size_hint` |
| index names | one `String` each (`core_name`, the contract's `(String, u32)`) | second pass: `(Name, u32)` pairs (12 B an entry) into the archive's one `Names` arena, sized by one in-place pass over the index |
| member names | one `String` each | second pass: a `Name` into the same arena |

The core turns the index into an `FxHashMap<&str, u32>` once per archive (`native.rs`), so
lookup by name is one hash. After the second pass an archive read allocates a constant number
of times (`heads`, `members`, `index`, the arena; the arena may grow once for the members'
names), as mold keeps them; the `format!` of `push_core_name` for an index name without a
leading underscore is the one per-entry allocation left (see above).

## unsafe

Neither file has `unsafe`; each forbids it at module level (`#![forbid(unsafe_code)]`), as the
crate can't while `macho.rs` has one block. Every zero-copy view of the input is made after a
bounds check: `object`'s checked parsers for the headers and tables, `get(..)` for relocation
fields, `Cursor::take` for `__eh_frame`, a length check for archive members
([unsafe.md](../../design/unsafe.md)).

## Tests

- `input.rs`: every existing test kept. Added `seeded_objects_read_as_the_per_section_model_reads_them`:
  256 seeded objects (text, data, cstring and zerofill sections; global, local, `ltmp`, `L`, `l`
  and `N_ALT_ENTRY` symbols at random places, at section ends too; `BRANCH26`, extern and
  non-extern `UNSIGNED` relocations in a shuffled table) read by the reader and by a model of the
  old layout (a `Vec` of starts and bounds per section, linear scans, per-atom sort): the same
  atoms, alignments, bytes, fixups and symbols. And `eh_frame_fdes_find_their_cie_and_section_by_search`:
  two CIEs, three FDEs pointing into atoms of two sections, each `pc_begin` made explicit to the
  right atom.
- Second pass, `input.rs`: every test now reads names through `Object::name`, bytes through
  `Data::bytes(input)` and fixups through `Object::section_fixups`; the model keeps the old
  per-atom copy. The differential test also checks that the arena holds exactly each Mach-O
  section's name and each symbol's, and that the fixup runs tile `Object::fixups` in atom order.
  Added `every_input_range_holds_the_bytes_the_old_copy_held`: 256 seeded objects read from
  slices shifted by 0, 1, 3 and 7 bytes; every atom is `Data::Input` (or `Data::Zero` where the
  old one was), its range holds exactly the old copy's bytes, and the ranges are the same
  whatever the slice's alignment.
- `archive.rs`: added 64 seeded BSD archives (1 to 20 members, short and long names, up to 50
  index entries) whose index must map to the members written, and an index entry naming no
  member header refused. Second pass: names compared through `Archive::name`, and the arena must
  hold exactly the members' and the index's names.
