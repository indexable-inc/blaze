---
status: plan
summary: How blaze-link works as written by lane link (the core, the jump table, positions, live patching, native resolution, PE and wasm rules) and what it still lacks; nothing in it was built or run.
date: 2026-10-29
---
# link: notes on the in-place linker

Part of [link](../link.md). Code: `crates/blaze-link/` (the core, and the format writers
behind `model::Writer`: `elf*`, `macho*`, `pe*`, `wasm*`), `crates/blaze-live/` (the runtime
that takes patches) and `vendor/cranelift-object/`. Nothing here was built, checked or run (v2
rule: agents run nothing expensive); every claim is read off the code.

## A link

`Image::new(to, output)` makes an empty image; `Image::stage(Change)` applies a change to a
copy-on-write copy of the state (`CowVec`, `Arc` fields) and returns `Staged`, the effects to
run; `Image::commit(staged, effects)` adopts it once the caller ran them, `complete` takes the
caller's completions, `roll_back` goes back one commit. A stage is, in this order
(`stage.rs`):

1. removed functions, statics and natives go; added ones become pieces, in parallel, their
   symbols interned shard by shard (`Symbols`, 16 shards);
2. `native.rs` resolves every name from scratch, as lld would for the same inputs (below);
3. `needs` makes the GOT cells and PLT stubs the pieces to write need;
4. `sweep` counts references to shared pieces (data, CIEs, file names) and drops unused ones;
5. `place.rs` places (a full link lays everything out; an incremental one allocates);
6. `write.rs` relocates every piece that is new or refers to something that moved, in
   parallel, each core on a disjoint slice of its region's bytes;
7. `file.rs` has the writer synthesize its regions and headers, and makes the effects: a full
   link truncates and writes the whole file; an incremental one clones the old file (FICLONE)
   and writes only the dirty ranges, then `Sync`, `Mode 0o755` (a clone may not keep the
   mode), `Rename` over the output. The undo of a commit is the old bytes of exactly the
   ranges it wrote, clipped to the old length, and the old length.

A full link is the same whatever the core count or the order changes came in: each class's
pieces are sorted by rank, link-line order, then symbol key.

## Relocations

What codegen emits and what the linker makes of it (`reloc.rs`, `reloc::of_kind`). `S` is the
target, `A` the addend, `P` the place, `G` the target's GOT cell.

### Relocations codegen emits

| `RelocKind` | x86-64 | AArch64 | written as |
|---|---|---|---|
| `Abs64` | yes | yes | `S + A`, 8 bytes; rebased by the loader when `S` is in the image |
| `PcRel32` | yes | no | `X86Branch32`: `S + A - P`; a call to an import goes to its PLT stub |
| `GotPcRel32` | yes | no | `G + A - P`; the cell is made by `needs` |
| `Call26` | no | yes | `A64Call26`; out of range is an error naming the call |
| `AdrpPage21`, `PageOffset12` | no | yes | page and low 12 bits; the low-12 shift is read from the instruction |
| `GotPage21`, `GotPageOffset12` | no | yes | the same, of `G` |
| `TlsOffset` | yes | yes | `S + A` from the thread pointer, by the format's TLS variant |
| `WasmFunction`, `WasmTable`, `WasmMemory` | wasm32 only | | `wasm::codegen_apply(kind, in_code)`: in code `FUNCTION_INDEX_LEB`, `TABLE_INDEX_SLEB`, `MEMORY_ADDR_SLEB`; in data and statics the `_I32` forms |

Targets: `Target::Function(key)` is the function's jump-table slot when the image has a table
(incremental, not wasm), else its body; `Target::Direct(key)` is always the body;
`Target::Extern(name)` a name natives and Rust share; `Target::Weak(name)` the same, 0 when
nothing defines it; `Target::Static`, `Target::Data` codegen's pieces; `Target::Position(span)`
a `core::panic::Location` record ("Positions"). A value that does not fit its field is an
error naming the field, never a truncation.

## The jump table

An incremental image calls every function through a slot (`table.rs`, #275):

- `SymKey::Generation`: one 8-byte cell holding the address of the current cells.
- `SymKey::Cells`: one 8-byte entry per slot, the address of the function now in it.
- `SymKey::Stubs`: one stub per slot, what callers call. x86-64 (16 bytes):
  `mov r11, [rip+generation]; jmp [r11 + slot*8]`. AArch64 (24 bytes): `adrp x16; ldr x16`
  (the generation cell), `movz/movk x17` (the slot offset), `ldr x16, [x16, x17]; br x16`.
  Only call-clobbered registers no argument is in (`r11`; `x16`, `x17`).
- Capacity: `max(2n, n + 65536)` slots for the first link's `n` functions; a full table is an
  error asking for a link from scratch.

An incremental link writes changed entries, never moves a caller. A live patch writes a new
cells array and one store of its address into the generation cell switches every slot at
once; a rollback stores the old address.

`Change::slots` is `(slot, function)` pairs: the slot of the first runs the second. An edited
function has a new `InstanceKey` (its body changed) while its callers name the old one, so
codegen and the coordinator pass, for an edit, the old key in `removed`, the new function in
`added`, and `(old key, new key)` in `slots` (as codegen's `TieredUp::replaced` gives for a
tier-up). Removing a function empties its slot only where the slot still holds that body; a
removed function whose slot is left empty while something still calls it is an error naming
the caller. Slots need a table: `slots` on an image linked without one is an error.

## Positions

`line!()`, `file!()`, `column!()`, a panic's and a `#[track_caller]` function's `Location` are a
`Target::Position(SpanRef)` relocation, never bytes in code (`position.rs`). The linker makes a
24-byte record per span, as `core::panic::Location` is laid out: the file name's pointer at 0
and length at 8, `LINE` at 16, `COLUMN` at 20 (`position::LINE`, `COLUMN`), from the span table
`Change::spans` carries for the span's anchor (magic `BZSPANS1`). File names are NUL-terminated
shared pieces. A reformat changes only the table: each record of the anchor is rewritten in
place and no function is compiled or relocated again. DWARF line tables (`Debug::Lines`) come
from `CompiledFunction::lines` through the same tables (`dwarf.rs`).

## Live patching

`blaze-live` (`patch.rs`) is the protocol: a `Descriptor` (version 2, 112 bytes, with the
generation) exported as `__blaze_live`, and `Message::{Patch, RollBack, Restart}` files in the
live directory, `message_file(n)`, written under another name and renamed. `live.rs` makes a
`Patch` of an incremental link: base and new generation, the code and data writes, the new
cells, frames and Windows unwind entries; anything a running program can't take (a moved
static, a changed layout, new imports) is a `Restart` naming why. ELF frames are followed by a
4-byte zero inside the patch's writes (libgcc and libunwind read a run of FDEs to a terminator).
New code never reuses old code space: a running program may still be in it, and a rollback
returns to it. A panic under the runtime's guard rolls the newest generation with a frame in
its code back to its base and answers `RolledBack`; `Image::roll_back` follows. A speculative
patch commits while its borrow check (`Pending::finish`) runs beside it. On a refusal, the caller
completes any commit in flight and rolls back each commit made since the patch, one
`roll_back` per commit.

## Start and stop sections

`__start_X` and `__stop_X` (linkme, `#1496`) are `Special::Section { name: X, end }`: the bounds
of the `Class::Named` regions of that name; a reference to one when no input has the section is
an error naming the section. Named and init-array regions are ordered: an incremental link that
adds or removes a piece in one lays the region out again in place.

## Native inputs

`native.rs` loads objects in link-line order and pulls archive members for what is strongly
undefined, wherever the archive is on the line, to a fixed point (lld's rule). Then:

- COMDAT: of a name the first group wins; a group marked `largest` (`IMAGE_COMDAT_SELECT_LARGEST`)
  is replaced by a later one whose first section is larger, and candidates in a dropped group
  are forgotten.
- A strong definition beats a weak one beats a common; two strong ones are an error naming both
  inputs; a Rust definition and a strong native one are an error.
- `SymbolDef::Fallback` (COFF weak externals, `/ALTERNATENAME`): when nothing defines the name
  it is an alias of its fallback symbol, which must then be defined; `search` makes the name
  pull archive members; a fallback chain back to itself is an error.
- `.drectve` directives: `/INCLUDE` roots and requires its symbol; `/EXPORT:name=symbol` exports
  it (from an executable too, as link.exe does); `/FAILIFMISMATCH` with two values is an error
  naming both inputs; `/DEFAULTLIB` and `/NODEFAULTLIB` are followed as link.exe does, from
  every object and taken archive member: `target::locate` puts each library they ask for on
  the line after the archive that asked (transitively, found in `-L` then `LIB`), and the
  engine stops with an error naming the library and `archive(member)` when one a loaded
  member asks for is not on it.
- An archive member that reads as a `Shared` (an import library's member) adds its DLL, merged
  by name with the others of that DLL.
- Collection (`--gc-sections`): from the roots (Rust's references, the entry, exports, `_init`,
  `_fini`, retained and init/fini sections), through relocations; a kept group's sections live
  or go together; unwind sections and FDEs are kept for what they describe.
- What is left undefined: a writer's special, a shared library's export (an import), 0 when
  every reference is weak (executables), an undefined import of a shared image, else an error
  naming the symbol and who refers to it.

## PE

`Plan::got_by_library`: the GOT is the import address table. Import cells are sorted by DLL and
import, each DLL's run followed by an 8-byte zero piece (`SymKey::IatEnd`), local cells after;
the GOT is laid out again in place when it changes. `__imp_X` is `Special::Got(X)`: X's cell,
X resolved strongly. Every reference to an imported function other than through its cell goes
to its PLT stub (`jmp [rip+cell]`, link.exe's thunk); imported data other than through `__imp_`
is an error. The core writes nothing in import cells: the loader fills them. A DLL's entry is
`Writer::shared_entry` (`_DllMainCRTStartup`). `Writer::own` puts codegen's thread-locals in
`.tls$`. `Placed::symbol` lets the writer find `_tls_used`, `_load_config_used`.

## wasm

Code is reached by index (`Plan::indexed_code`): every class the writer makes executable, and
its executable synthetic regions (`__wasm_call_ctors`), are a code space of their own from 0,
apart from memory. A function's index is the number of imports (all wasm imports are
functions) plus its rank by code address; `FUNCTION_INDEX_*` get the index, `TABLE_INDEX_*` the
index + 1 (its slot), 0 for a weak undefined function; a call by index to an undefined one is an
error. An undefined name that some input says the wasm import of (`Object::imports` of a native
object, `Code::imports` of a codegen function, `Static::imports`) and that is not weak becomes a
function import (rustc's `--allow-undefined`); any other undefined name is an error.
`Placed::functions` lists native code pieces too. No jump table, no GOT, no DWARF, no live
patching (a live wasm output is an error).

## Resolution: cost

What resolving the native inputs and the names costs (`native.rs`, against the reader contract
in `model.rs`), read off the code; nothing was measured.

**The reader contract.** A reader borrows: a section's bytes are `Data::Input(range)` into the
input (or a member's slice of its archive), a name is `Name { at, len }` into one `Names` arena
per file, and relocations are one CSR column (`Object::fixups`, each section a `Range<u32>`
of it), as are COMDAT groups (`Object::grouped`). A reader makes bytes (`Data::Bytes`) only
where the format has none to borrow, such as wasm function bodies. `check_file` checks every index and
range once, when the file is read, so nothing after it checks bounds against the bytes.

**Once per object** (when its input is added, or its member is first pulled), in parallel
over the objects of a round. A same-bytes result whatever the core count comes from batching in input order:

- read and check: linear in its symbols, sections and fixups; `.eh_frame` split into records;
- interning: one `intern_wide` batch for every symbol, section and frame record of the round,
  and one `names_all` batch for named sections' names (both deterministic, see "Interning");
- columns (`Parsed`): a `Sym` per symbol, section and frame record (4 bytes each); per section
  the symbols its fixups refer to and the symbols it defines (CSR, 4 bytes an entry, each list
  deduplicated); the group of each section (4 bytes); per frame record its section, range,
  CIE and described function (24 bytes) and its fixups (CSR); the root sections; the named
  sections sorted by name.
- an archive's index (`Table`: open addressing over 64-bit hashes, the first member to define
  a name keeps it, so lookup order is link-line order) is built once, in parallel over the
  archives, and kept with the image in `Stage::objects` like the parsed objects.

An object is never read, interned or indexed again while its input stays on the line: a
stage that changes the natives reads only the inputs it adds. Removing an input drops its cached objects.

**A stage with no native change.** Rust's references are counted, not walked: `Refs` keeps,
per name (by `Symbols::dense`), how many of Rust's pieces refer to it strongly and weakly, and
per counted piece the names it referred to. A stage counts the pieces made since the last
resolution (their fixups) and uncounts the ones in the state's removal log since then
(`State::removed_since`). That is linear in what the change made and removed, never in the
image. Only names whose class (none, weak only, strong) changed go on, and each is checked
against what it resolved to (`settled`):

- nothing, for nearly all edits (a body changes, a name stays referred to): the stage keeps
  the resolution, sharing every column by `Arc`;
- grow, for a name Rust newly refers to: it is resolved against the kept graph (one hash
  lookup per name), and what it newly reaches is marked from that name alone. The bitsets are
  copy-on-write by chunk, so a mark costs the chunks it writes. Then only the new sections and
  frame records are made. The cost is the name plus what it reaches, plus a copy of the
  import list when it adds an import;
- rebuild, when that is not enough: the last reference to something native went (its sections
  may be collected), a weak-only reference became strong, or a gained name needs an archive
  member, an alias, a section-keeping special or a library not yet needed.

**A rebuild** builds the graph again from the cached columns, with no reading and no
interning. Registration, which applies groups, directives and strengths in link order, costs
O(globals of the loaded objects). Marking costs O(kept sections + their references), and
assigning definitions costs O(globals). It then diffs the new graph against the kept one by object. Only
sections, frame records and commons that came or went are made or removed, and only objects whose
sections changed or whose globals' winners changed have their symbols defined again. Exports
(the shared libraries' names, one `Table`) are kept when the libraries are the same `Arc`s,
and the import and library lists keep their `Arc` when they come out equal. The writer
compares those to decide whether to bind again.

**Ahead and behind.** mold and wild resolve symbols in parallel: every file inserts its
definitions into one concurrent map (mold), or prehashed names are bucketed and resolved per
bucket (wild). mold also marks `--gc-sections` liveness in parallel with atomic flags. Both
redo all of it, and the archive member fetch, on every link. lld resolves archives lazily:
an archive's symbols are lazy entries in the one symbol table, and a reference fetches the
member, so no archive is rescanned. Here an archive is a cached hash index, members are pulled
in rounds, and a round's reads run in parallel. Ahead: an edit that changes no native input and
no name's class costs what it made, not a link, and a gained name costs what it reaches. A
native change costs reading the new inputs plus a rebuild from columns, not reading everything
again. Behind:

- registration and marking in a rebuild are serial, where mold and wild do both in parallel. Marking could
  run as a parallel frontier over the CSR columns, and registration could resolve per name
  shard and then apply strengths in load order;
- a rebuild re-marks from every root, even when one root went;
- each native input's bytes are copied once into the state (`Input::bytes`, an `Arc<[u8]>`)
  rather than mapped;
- bytes a reader makes (wasm function bodies) are copied into the stage arena each time
  their section is made;
- `NativeFixup` is 40 bytes;
- there is no benchmark against mold, wild or lld on the same inputs.

**Rules this keeps that the code makes precise.** The "first" COMDAT group of a name is first
in load order: the link line, then each pull round with its members sorted by input and member.
A frame record's own fixups (an FDE's personality, LSDA and function pointers) are references:
they keep what they name and resolve it. Native wasm imports are `Object::imports`, keyed by
symbol (formerly `Symbol::import`). The same name imported two different ways, by two objects
or by an object and Rust, is an error naming both.

## State and stages: cost

What the state (`state.rs`, `cow.rs`, `symbols.rs`) and a stage (`stage.rs`, `live.rs`,
`commit.rs`) cost, read off the code; nothing was measured.

**Interning.** Every symbol, name and 32-byte id is interned once into a `u32`
(`Sym`, `NameId`, `IdNo`). `SymKey` is `Copy` and 20 bytes (a tag and `u32`s; `Got`/`Plt`
name the target `Sym`). `Symbols` is 64 shards of SwissTables (`hashbrown::HashTable<u32>`
over a `Vec` of keys and their hashes). A key is hashed once with a folded multiply (FxHash
words folded for names, the first 8 bytes for a content id), and the shard comes from hash
bits 32..38. Growing a shard never rehashes. A lookup is one hash, one 16-wide group probe and
one 20-byte compare. A batch (`intern_all`, `intern_wide`) is split by shard and each shard
interned in input order by one rayon task; below 4096 keys it runs serially and gives the same
ids. A `Sym` is `shard << 26 | index`; `Symbols::dense` maps it to `index * 64 + shard` for
per-symbol columns.

The benchmarks: mold interns names in one concurrent open-addressing map filled by every
input file at once; wild keeps a database of prehashed names with dense `u32` symbol ids,
filled in parallel by hash bucket. The ids here are deterministic whatever the thread count,
which a CAS-based concurrent map does not give. The cost is one partition pass per batch.

**Sizes.** A piece is columns, each a copy-on-write `CowVec` of 1024-element chunks:

| column | bytes | read by |
|---|---|---|
| `Hot` (addr, size, sym, region, class, align, flags) | 32 | every pass |
| `What` | 16 | stage, writer |
| `Sort` (rank, order) | 16 | full placement |
| lists: fixups, weak, defines (`ListRef` each 12) | 36 | relocation |
| loader: rebase, binds | 24 | writer, live |
| users | 4 | sweep |

That is 128 bytes per piece plus its lists. Per symbol there is a `Def` (16), a `SymCold`
(24: its referrer list, slot chain, seen flag) and the interner's key, hash and control byte.
The lists are CSR ranges into `CowList` arenas of 64 KiB chunks (a list never straddles two).
A write copies only the chunk it touches, and compaction runs only when the garbage outweighs
the live data. `Fixup` (`model.rs`) is 32 bytes; 24 is possible (a 32-bit `at`, the `minus`
symbol folded into a tag).

**An incremental stage** costs what it touches:

- Cloning the state is one `Arc` count per chunk: about 6 000 for a million pieces.
- Its sets (`IdSet`, `SymSet`) are bitsets with an insertion list: membership is one load,
  iteration costs the members, and clearing costs the highest id / 64 words.
- A function made costs its keys (one interning batch for the whole change), its lists pushed
  into the arenas, one chunk copy per column chunk first written (32 KiB for `Hot`), and one
  16 KiB page copy per region page first written (`CowBytes`). A region is never copied whole.
- Its bytes are borrowed from the change or allocated in the stage's `bumpalo` arena:
  stubs, CIEs, Windows unwind info and locations. There is no `Vec` per piece.
- Referrers come from per-symbol lists, so relocating again what refers to a changed symbol
  costs those referrers. A jump-table slot is a cell in a column: setting one is O(1), and the
  patch lists the dirty slots, not every slot.
- A span table is compared by its bytes before decoding; a changed one rewrites only its own
  positions (one lookup per span index).
- The live message is built from borrowed slices and encoded into one buffer of its exact
  length. A commit's effects are one queue cut by step counts, and the effects in flight are
  a range of consecutive tags plus a bitset.

**What is still O(image) per incremental link.** Some of it is mine:

- the cells piece's rebase list, rebuilt from `filled_cells()` (O(slots)) when a cell was set
  or emptied; the cells themselves are written 8 bytes per dirty slot;
- `State::compact` when it runs, amortised over the garbage it drops.

The rest is outside these files:

- a native rebuild, when a stage changes native inputs or a name's class ("Resolution: cost");
- the writer's lists (rebase, binds, GOT, functions, debug), spliced in one linear merge when
  they change ("Placement and writing: cost");
- the format writers' synthesis of headers and tables.

**Ahead and behind.** Neither mold nor wild patches an image in place: both relink from the
inputs, so a one-function edit costs them a full link (mold's published figure is about a
second or two for Chromium on many cores). Here it costs the function. Behind:

- Full-link throughput is unmeasured. mold's single concurrent map skips the partition pass.
- mold writes into a memory-mapped output; here regions are built from pages and written by
  effects.
- There is no benchmark yet against either on the same inputs.

`tests.rs` checks the dense symbol index (distinct, and a column under twice the count) and
that a clone's lists, referrers and cells stay as they were when the original rewrites them.
It also checks that removal leaves the referrer lists in order without the removed piece, and
that compaction keeps every live list and function lines. `cow.rs` and `symbols.rs` test chunk
sharing, paged bytes, and that parallel interning gives the serial ids.

## Placement and writing: cost

What placement (`place.rs`), relocation and the writer's lists (`write.rs`), the file's writes
(`file.rs`) and the tables they use (`reloc.rs`, `table.rs`, `unwind.rs`, `dwarf.rs`,
`position.rs`) cost, read off the code; nothing was measured.

**Per piece, full link.**

- Placement:
  - Each piece gets a 40-byte entry: a 32-byte key and its symbol. The key is the import
    address table cell, rank and link-line order, and an order-keeping prefix of the symbol's
    contents (its variant, then 7 bytes of its id or name).
  - Classes are a dense index. Each class's entries are radix sorted (two passes over 16-byte
    keys) in parallel with the other classes.
  - Only entries whose keys tie are compared through `Symbols::cmp`.
  - Then one pass writes each piece's 32-byte `Hot` row.
- `Counts` is a parallel sum over the pieces, kept in `State::lists` for the next link.
- Relocation:
  - Each region is one zeroed buffer, split into a disjoint slice per piece in address order.
    Each piece's bytes are copied in once.
  - A piece with more than 2048 fixups (sorted, non-overlapping, no format relocations) is
    split into jobs at fixup boundaries. Small pieces run in batches of at least 8.
- The writer's lists (rebased addresses, binds, GOT cells, functions) come from one parallel
  pass and a parallel sort.
- The file is written as runs of at most 8 MiB, cut at multiples of 8 MiB, so the core sends
  them past the page cache. The runs are built in parallel, each copying its bytes from the
  region buffers once.

**Per relocation.**

- Before the parallel loop, every symbol the written pieces refer to is gathered (a parallel
  fold), sorted, deduplicated and resolved once, in parallel, into a dense column.
  - Each symbol takes 16 bytes: its value, its kind in a byte, and the index of its GOT cells
    and PLT stub.
  - The column is laid out per symbol shard and allocated zeroed, so pages that no symbol uses
    are never touched.
- A fixup then costs:
  - one load for its target, and one for `minus` if it has one;
  - one more for the cell when its kind goes through a GOT or PLT (a key read only for the TLV
    kinds);
  - a match on the kind byte, and `reloc::apply`.
- `reloc::apply` returns `reloc::Bad` (`Copy`, at most 24 bytes). A message is built only when
  a fixup fails, and the error kept is the first in address order, whatever the thread count.
- Loader output is appended to per-batch buffers, so no piece allocates.
- `table::stubs` writes every jump-table stub into one buffer in parallel from a static
  template. `unwind.rs` keeps a record's at most 3 fields inline and finds CIEs by binary
  search. `dwarf.rs` writes DWARF 4 directly into one buffer per section, sized beforehand.

**Per incremental link.**

- Placement:
  - Only fresh pieces are placed, grouped by class and sorted by the same keys.
  - A region whose order the program observes is laid out again from one scan of the `Hot`
    column, shared by every such region in the link, and only when one changed.
  - `Counts` is the last link's totals plus the difference over the pieces written, made or
    removed.
- Relocation:
  - The column covers only the written pieces' targets.
  - The pieces are relocated into one scratch buffer, then written into the regions'
    copy-on-write pages.
  - The jump table writes only its set cells, 8 bytes each. Its rebase list is rebuilt from
    the filled cells, which is O(slots), only when a cell changed.
- Lists:
  - Each changed list is one linear merge: drop the entries whose piece (a bitset lookup) was
    written or removed, and merge in the new ones. A list nothing touched is shared.
  - GOT cells and binds are rebuilt from every piece only when the imports or the TLS layout
    changed.
  - The debug sections are reused unless a function was written, moved or removed, or a span
    table changed.
- The file:
  - It gets only what changed: pieces whose bytes or place differ, the cells set, the tails
    regions lost, and the 4 KiB blocks of synthetic regions and the headers that differ.
  - Ranges less than 4 KiB apart are merged into one write.
  - The undo is those ranges, not their bytes: a rollback reads the old bytes from the state
    the history keeps (`file::restore`).
- The commit clones the old file with `FICLONE`/`clonefile` and writes only the ranges, so the
  old image stays whole for a rollback.

**The benchmarks.**

- Full link: mold and wild write the output in parallel through a memory-mapped file. mold
  copies and relocates each output section's pieces across threads. wild resolves symbols
  into a dense table of ids before a single parallel write pass. Both run near memory
  bandwidth.
- Incremental link: the bar is writing only the changed pieces into a block-shared clone of
  the old file. Neither mold nor wild patches an image in place; wild lists incremental
  linking as a goal.

**Ahead and behind.**

Ahead:

- A one-function edit writes the function, its stub cell and the headers it changes. Its
  callers are not touched, because they call through the jump table.
- The old file costs no copy, because the clone shares its blocks.
- Placement and relocation give the same image whatever the thread count, which a
  first-come concurrent layout does not.

Behind:

- A full link copies the image twice: into the region buffers, then into the write runs.
  mold and wild write once, into the mapping. `v2/requests/link.md` item 13 (a write of
  shared bytes) removes the second copy.
- `Placed` hands writers contiguous slices. So a changed list is copied whole (O(list)
  `memcpy` per incremental link), and `file::finish` makes each region with a written page
  contiguous again (O(region)) before the writer synthesizes, which is O(image) in the
  writers. A paged `Placed` would make both O(change).
- Nothing is measured against mold or wild on the same inputs.

The tests are beside the code:

- `place.rs`: the radix sort against a comparison sort, and that symbol prefixes keep the
  content order.
- `write.rs`: job splitting, a spliced list equal to the list built again, and the value
  column.
- `file.rs`: range merging, runs, and the layout read against a whole-file buffer.
- `reloc.rs`, `table.rs`, `unwind.rs`, `dwarf.rs` (read back with `gimli`) and `position.rs`
  keep their tests and add new ones.

## Tests

`crates/blaze-link/src/tests.rs` links x86-64 ELF images end to end through a simulated disk
(it runs the effects on a sparse file) and reads them back: calls reach their callees; the image
is the same in any order; an incremental link clones, writes little, and moves only a slot
entry; a rollback restores the file byte for byte; a failed commit leaves the image as it was;
live descriptors and patches; positions and a reformat that rewrites only them;
`__start_`/`__stop_`; C and Rust calling each other by name; DWARF line tables. Each format's
writer has its own tests from hand-made `Placed` values. None was run.

## Limits

- The whole cells array (8 bytes per slot of capacity, about 512 KiB at the least capacity) is
  written on each incremental link of an image with a table; writing only changed entries is a
  smaller write the effects can already say.
- A Mach-O object's `-weak-lNAME` and `-weak_framework NAME` linker options are an error:
  `NativeKind` has no weak library, so the writer can't emit `LC_LOAD_WEAK_DYLIB` for one
  (link.md 25).
- An archive member is pulled even when a shared library given earlier defines the name.
- AArch64: no range-extension veneers; a call past ±128 MiB is an error naming it
  (`Class::Veneers` exists in the model but the core makes none).
- PE: no PDB (debuggers and dbghelp show addresses only), no delay-load imports, no
  `/guard:cf` tables (see the PE writer's report).
- wasm: no DWARF; a direct call to an undefined weak function is an error, where wasm-ld makes
  a trapping stub.
- Zero-copy reading of native inputs (`Data` borrowing the input's bytes) is requested by the
  speed lanes and not done: every reader still copies section bytes once.
