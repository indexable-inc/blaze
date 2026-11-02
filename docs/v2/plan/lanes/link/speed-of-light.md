---
status: plan
summary: lane link's speed of light: the bound each component's work and the hardware set, the arithmetic, and how the design reaches 90% of it.
date: 2026-10-29
---
# Lane link: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). Each component has a section here, written by the
agent that owns it. Each edits only its own section.

## The machine

The target machines are in [hardware.md](../../design/hardware.md): EPYC Turin, Graviton 5 and
Apple M-series. Unless a section says otherwise, the figures are these, not yet measured:

| what | figure |
|---|---|
| clock | 3.5 GHz |
| L1 read, streaming | 64 B a cycle |
| L2 read, streaming | 32 B a cycle (about 110 GB/s) |
| one core from DRAM, streaming | about 25 GB/s |
| a socket from DRAM | about 400 GB/s (Turin, 12 channels of DDR5 at 70% of peak) |
| cores a link may use | 16 (a dev machine), 64 (a CI machine) |
| page-cache copy (`read`, `write`, `copy_file_range`) | about 10 GB/s a core |
| NVMe write | about 7 GB/s |
| system call on an open descriptor | 0.15 µs |
| system call that resolves a path | 0.4 µs |
| `mmap` plus first touch of a 4 KiB page | 0.25 µs a page |
| `mprotect` | 0.5 µs, plus a TLB shootdown of about 2 µs on each other core running the process |
| IPC | about 2 instructions a cycle for branchy code, about 4 for straight-line integer code |
| SHA-256 with SHA-NI or ARMv8 crypto | about 2 GB/s a core |
| BLAKE3 with AVX-512 | about 6 GB/s a core |

The reference workloads:
- **A full link of a large Rust binary** (ripgrep-sized to rustc-sized): 50 to 500 MB of input
  sections, 1 to 10 million relocations, 0.2 to 2 million symbols, and 100 MB of output.
- **An edit**: one function's body changes, so 1 to 10 functions are re-placed.

## ELF writer and readers: `crates/blaze-link/src/elf.rs`, `elf/` (speed-link-edge-sol-link)

**Workload.** A Linux x86-64 PIE of 100 MB: 200,000 named functions (80-byte names), 300,000
slid addresses, 210,000 FDEs in an 8 MB `.eh_frame`, 400 imports, and 300 MB of DWARF in a debug
build. Native inputs: the crt objects, `libc.so.6` (3,000 dynamic symbols), `libgcc_s`, and 20 MB
of C archives of which 500 members are pulled (60,000 symbols, 250,000 relocations, 3 MB of
string tables). The edit workload is one function's body changing.

The readers only hand out ranges and `Names` spans into the input; the writer is a pure function
of `Placed`. So every bound below is the memory the phase must touch, or its instruction count
where that is larger, spread over 16 cores. Budgets are the bound × 1.1. The figures are
arithmetic, not yet measured: this pass ran no benchmarks.

| phase | bound | where it is | what the core spends |
|---|---|---|---|
| parse | 0.1 ms over the pull rounds | near the bound | none: 19.3 and 19.4 made |
| resolve | 0.15 ms, 0.3 ms with collection | serial on a full link: about 0.7 ms registering, 3 ms collecting and defining; O(change) on an edit (see "Native inputs") | none |
| layout of synthetic sizes | O(1): sizes come from `Counts` | at the bound | none |
| synthesize `.rela.dyn` | 0.1 ms | near the bound | none |
| synthesize `.eh_frame_hdr` | 0.05 ms | near the bound (was 12 ms) | none |
| synthesize `.symtab`/`.strtab` | 0.1 ms | parallel (corrected 2026-10-30): one pointer chase a name | queue 00923 (a names column) |
| synthesize `.dynsym`/`.dynstr`/`.gnu.hash` | under 10 µs | at the bound | none |
| debug sections | 0 | at the bound: no copy (was 30 ms) | 19.2 made |
| relink one function | about 3 µs | O(changes) patched; kept tables handed back as `CowBytes`, no copy (corrected 2026-10-30) | none |

**Parse** (speed-sol2-link-inputs owns this paragraph and the row). About 24 MB is touched:
the headers, symbol tables, string tables and relocations of the pulled members, plus the
archive indexes. At 400 GB/s a socket that is 0.06 ms; the decoding is about 16 million
instructions, or 16M ÷ (3 × 3.5 GHz × 16) = 0.1 ms in all, so parse is compute-bound at 0.1 ms
spread over the pull rounds. Each round adds one fork and join of its reads (about 10 µs) and
one batch interning of its symbols; a C archive needs 3 to 6 rounds. The string-table NUL scan,
the last scalar per-byte loop, is now eight bytes a step: `elf/read/strings.rs` `nul_mask` builds
each 64-bit NUL bitmap with the SWAR zero-byte test and a multiply gather. `elf/` forbids unsafe
code, so this is the widest safe form. It is tested against the old byte loop on every byte
value at every position and 20,000 random chunks. The core no longer copies or names anything
per input (corrected 2026-10-30 from the code): `Input::bytes` is the caller's `Arc<[u8]>`,
taken as a count (19.3), and `read_misses` names a member only in its errors (19.4). What is
left per input is in "Native inputs: reading and pulling" below.

**Resolve.** 60,000 symbols with about two hash probes each at about 20 ns a miss is 2.4 ms of
one core, 0.15 ms on 16. Corrected 2026-10-30 (speed-sol2-link-inputs): only the first probe,
the batch interning of a round's symbols, is parallel; registering them in the graph, the
second probe, is serial (about 0.7 ms), and so are collection and the definitions after it
(about 3 ms). An edit that changes no native and no name's resolution builds no graph. See
"Native inputs: reading and pulling", "The resolve graph".

**Layout.** Every synthetic size is a function of `Counts`, which the readers fill while parsing,
so layout does constant work for each region.

**Synthesize.**
- `.rela.dyn`: 300,000 × 24 bytes = 7.2 MB written and 2.4 MB of addresses read, plus a radix
  sort when the addresses come unsorted (8 passes over 2.4 MB, read and write): about 45 MB ÷
  400 GB/s = 0.1 ms. The writer is parallel and radix-sorted already.
- `.eh_frame_hdr`: 210,000 entries of 8 bytes out, 8 MB of `.eh_frame` in. Reading is 8 MB ÷
  400 GB/s = 0.02 ms; decoding is about 40 instructions an FDE, 8.4M ÷ (3 × 3.5 GHz × 16) =
  0.05 ms. It was a serial gimli walk with a CIE hash map, about 12 ms. Now `elf/dynamic.rs`
  splits the section at every 4,096th FDE start the core placed (`Function::fde`), walks the
  pieces in parallel with a hand decoder (CIE versions 1, 3 and 4, augmentations `z`, `L`, `P`,
  `R` and `S`, a zero length ending the section, as gimli reads them), joins the pieces in
  parallel, skips the sort when the table is already sorted (it is when functions are placed in
  address order), and encodes the table in parallel. A split that doesn't land on a record
  start is an error naming the offset, never a quiet serial walk. It is tested against the old
  gimli walk on random sections of five CIE kinds with splits every 1, 7, 64 and n FDEs, with
  terminators in any piece, and with the first error reported in section order. Since
  2026-10-30 (A8, speed-sol2-link-writers) the walk is `unwind.rs` `scan`, which the Mach-O
  writer uses too: see "The shared `.eh_frame` scan" below.
- `.symtab`/`.strtab`: 200,000 × 24 bytes = 4.8 MB of symbols and 16 MB of names written, 16 MB
  of names read: about 37 MB ÷ 400 GB/s = 0.1 ms. Corrected 2026-10-30
  (speed-sol2-link-writers), from the code: `elf/image.rs` already fills the locals in
  parallel, one task per 4,096 functions at offsets from prefix sums of the name lengths
  (`name_counts`). The names are `Function::name: Option<Arc<str>>`, not `Class::Named`
  strings, so link.md item 11 (section names) doesn't touch them. What is left is a dependent
  load per name, to its own heap block. Queue 00923 asks the core for a contiguous names column
  in `Placed`, which makes the fill one parallel copy.
- `.dynsym`/`.dynstr`/`.gnu.hash`: 400 imports and a few exports, under 10 µs.
- Debug sections: 300 MB. The bound for the linker is zero: the bytes are already in the inputs
  and in the store. They were one serial copy into the tail buffer (300 MB at 10 GB/s = 30 ms),
  then a parallel copy into a part each (about 2 ms). Now (19.2) `File::debug` names where each
  of the core's sections goes, (offset, index into `Placed::debug`), and `elf/image.rs` copies
  none: `file.rs` lays and diffs them from the state's shared `lists.debug`, and an unchanged
  section is the same buffer, compared by address. `elf/tests.rs`
  `the_debug_sections_are_the_cores_not_copies` checks the file's bytes against the core's and
  that 1 MiB and 3 MiB of debug info allocate the same buffers, under 4 KiB apart.

**Incremental relink of one changed function.** What changes: the body (about 1 KB), its FDE
(about 40 bytes), its `.eh_frame_hdr` entry (8 bytes), its `.symtab` entry (24 bytes) and a few
relocations and slid addresses, about 2 KB in at most five 4 KiB blocks. The bound is writing
those: five block writes at 0.15 µs a call plus 0.4 µs of copying, about 3 µs. `file.rs` diffs
parts at the same offset by 4 KiB block as well as synthetic regions (19.1), so a moved
`.symtab` entry rewrites its block of the tail part, not the 20 MB part. With 19.5 `Placed`
carries a `Delta` (the functions, slid addresses, GOT cells and binds spliced, the address
ranges the core wrote, whether regions and names are unchanged, and the synthesis it is from),
and an incremental output's writer keeps its last `.rela.dyn`, `.eh_frame_hdr` and tail
(`Elf::last`, each table with the synthesis it was made in) and patches them when the delta is
from that synthesis, no region moved and the lists it reads were spliced at equal lengths.
Every patched table is the one a build of the same `Placed` makes, which the tests check
byte for byte against a fresh writer's build; where that can't be shown cheaply the table is
built.
- `.rela.dyn`: the `RELATIVE` run is kept in the order of `Placed::rebase` (only when the
  addresses came sorted, so entry `i` is `rebase[i]`'s). A patch rewrites the entries of each
  rebase splice and of each slid address whose 8 bytes a `written` range or a spliced local GOT
  cell covers, two binary searches a range, and checks each splice is in order with its
  neighbours, so a build would sort nothing. The binds and GOT cells (a few hundred) are built
  as before and `.dynamic` with them.
- `.eh_frame_hdr`: the table is kept when it came in address order, so it is sorted by FDE as
  well. For each `written` range in `.eh_frame` the records are walked again with the build's
  own walk (`unwind::rewalk`, given the CIEs the build met), from the last kept FDE before the range to the first record start past
  it where a kept FDE starts (or the section's end or its zero length): the walk a build makes
  from there. Its FDEs replace that span's, and the patch holds only when no CIE was written,
  the FDEs start where they did (so the build's parallel splits stay record starts), the table
  stays sorted, and every FDE a spliced function names is one of the table (checked once on
  the build, in parallel, for every function, where the build splits at all).
- `.symtab`/`.strtab`: per function the writer keeps its local's index, name offset and stored
  name length. A functions splice whose functions keep their name presence and name lengths
  rewrites only those locals and names in the kept tail; the exports, imports, `.shstrtab`
  and section headers are written whole (O(globals + sections)); any change of layout (tail
  offset, debug length, counts, name totals) builds the tail.

So a one-function relink does O(splices + written + globals) work in the tables.
Corrected 2026-10-30 (speed-sol2-link): `Writer::synthesize` returns `CowBytes` and
`File::parts` is `Vec<(u64, CowBytes)>` (`model.rs`), so a patched table is handed back as two
reference counts, not copied. The rest of a one-function relink is the core's, not this
writer's: the writer lists' splice and the debug rows, in "The core" below.

**Proof.** Each phase is timed on the workload above, a full link and a one-function edit, against
the bound × 1.1 in the table; the relink is also measured in bytes written, against the 2 KB.

**State of the art.** mold parses and synthesizes in parallel and copies debug sections with
parallel `memcpy` into a mapped output; it has no incremental relink. wild is parallel by design,
and incremental linking is its stated aim but is not done. lld builds `.eh_frame_hdr` serially
from its own `.eh_frame` parse. The MSVC incremental linker patches in place by padding functions
and calling through thunks, which changes the code; blaze patches without thunks, so the image is
the one a full link produces. With 19.1, 19.2 and 19.5, blaze's relink patches its tables in
O(change) (neither mold nor wild relinks incrementally), and the kept tables go back as shared
`CowBytes`, not copies (corrected 2026-10-30).

### The shared `.eh_frame` scan: `unwind.rs` `scan` (speed-sol2-link-writers)

**The work.** Both writers read the image's own `.eh_frame`: ELF for `.eh_frame_hdr`, Mach-O
for `__unwind_info`'s FDE column (A8). On the workload that is 8 MB holding 210,000 FDEs and
their CIEs. Each record is a length, a CIE pointer, a pointer-encoded start and a length:
about 40 instructions an FDE.

**The bound.** Reading 8 MB at 400 GB/s takes 0.02 ms. Decoding is 210,000 × 40 = 8.4M
instructions ÷ (3 × 3.5 GHz × 16) = 0.05 ms. So the bound is 0.05 ms, compute-bound, with a
budget of 0.055 ms.

**Today** (2026-10-30, written without a build). The section is cut at every 4,096th FDE start
the core placed (`fde_splits`, from `Function::fde`), about 52 pieces, and the pieces are
walked in parallel by one hand decoder. Each piece keeps its own CIEs. A CIE pointer to an
earlier piece is read where it points (`cie_at`) and listed. After the join, any such CIE that
no earlier piece met is an error at the smallest record naming it, so the first error is the
one walk's. The kept items are joined by a parallel copy (`concat`). Each piece is about 15 µs
on one core: 52 pieces are four rounds on 16 cores, about 0.06 ms, plus a join of 3.4 MB
(under 10 µs). That is about 1.2 times the bound, estimated, not measured. Mach-O was a serial
walk with a hash map of CIEs, about 1 ms. One rule is now shared: an FDE must name a CIE met
before it. Mach-O already required this, and `unwind::split` holds every native section to it,
so no image blaze writes breaks it. Patches walk again with `rewalk`, which takes the CIEs the
build met.

**The gap, and how it closes.** About 20%, from the join copy and the uneven last round. The
cut could follow the thread count as the rebase stream's does. Each piece could also write
straight into the output at an offset from a count pass, but its native FDEs are only known
once it is walked, so that would need one more pass, which costs as much as it saves.

**Proof.** `scan` timed alone on an 8 MB section of 210,000 FDEs at 1, 4 and 16 threads,
against 0.055 ms. Correctness: `elf/dynamic.rs`'s tests against the old gimli walk with splits
every 1, 7, 64 and n FDEs, `a_cie_pointer_to_no_cie_is_one_error_in_any_piece`, and on Mach-O
`the_fde_scan_reads_what_gimli_reads` and `live_fdes_are_the_records_fdes_in_order`.

### The debug line program and subprograms: `dwarf.rs` `sections` (speed-sol2-link-writers)

**The work.** With `Debug::Lines`, every link encodes one compilation unit covering 200,000
functions and about 2 million rows. It writes a subprogram per function (`.debug_info`, about
15 bytes each, 3 MB), the names (`.debug_str`, 16 MB), and a line sequence per function
(`.debug_line`, about 4 bytes a row plus 20 a function, 12 MB). It reads 32 MB of rows and
16 MB of names.

**The bound.** Memory: about 80 MB read and written ÷ 400 GB/s = 0.2 ms. Compute: about 15
instructions a row, 30M ÷ (3 × 3.5 GHz × 16) = 0.18 ms. These overlap, so the bound is about
0.2 ms, with a budget of 0.22 ms.

**Today** (2026-10-30, request 00913, written without a build). The encode was one serial pass,
about 10 ms. Now it is in pieces of 1,024 functions in address order. Each piece builds its
subprograms, its names and its line sequences in parallel. Each line sequence starts from the
program's initial state, and each subprogram depends only on where its name lands in
`.debug_str`, so a piece's bytes are the serial pass's bytes for it. The name offsets come from
a prefix sum over the pieces. The three sections are then joined by parallel copies. The first
error is the serial pass's: the files are checked first, then the unit's low address, then the
subprograms in order, then the line program's header, then the sequences in order. Estimated at
about 0.3 ms, 1.5 times the bound: the join writes the 31 MB a second time, which adds about
0.15 ms with its zero fill.

**The gap, and how it closes.** Every piece's `.debug_info` and `.debug_str` sizes can be
computed up front, so those two could be encoded straight into the output. `.debug_line`'s
can't: special opcodes vary in length, and sizing them first doubles the compute. The design
that reaches O(change) is one compilation unit and line program per chunk of 1,024 functions,
each padded with slack. An edit then re-encodes one chunk in place, and the core rewrites only
that byte range: about 10,000 rows, 40 µs on one core. It is not built. It changes the unit
layout that debuggers see (the bytes stay valid DWARF, gimli reads them), and it needs the core
to keep each chunk's offset across links.

**Proof.** `sections_in_pieces_are_the_serial_encodes_bytes` compares against the old serial
encode, kept in the tests as `sections_serial`. It covers 1 to 5,120 functions in shuffled
address order, at both address sizes, with a bad file in an early piece and a bad name in a
late one, checking the same bytes and the same first error. To time it: `sections` on 200,000
functions and 2 million rows at 16 threads, against 0.22 ms.

**FDEs outside `.eh_frame` (00913, part 3).** Neither is worth doing. On ELF, `build_hdr` lists
FDEs outside `.eh_frame` only from `placed.functions`. Moving native FDEs into `Class::Patch`
would mean listing every FDE of the patch region's frame runs. Native code rarely changes in a
live patch, so this would add a second walk to every patch for little gain. On Mach-O,
`__unwind_info` can name an FDE only by its offset in `__eh_frame`, so an FDE outside it can't
be found by the unwinder when the image is launched again from disk. That would trade
correctness for speed, which the rules forbid.

## Mach-O writer and readers: `crates/blaze-link/src/macho.rs`, `macho/` (speed-link-edge-sol-link)

**Workload.** The ELF workload built for arm64 macOS: a 100 MB image, 200,000 functions with
80-byte names, 300,000 rebases, 400 binds, 210,000 FDEs, and 300 MB of DWARF in `__DWARF`. The
native inputs are the same C archives plus `libSystem.tbd` (about 1 MB of YAML in about 40
documents, 30,000 names) and any dylibs. The files were held by the `requests-link-fuzz` agents
during this pass, so the changes below are filed as exact edits in
`v2/requests/requests-link-fuzz.md` (A1 to A5) and `v2/requests/link.md` (19.1 to 19.5) rather
than made. Budgets are the bound × 1.1; the figures are arithmetic, not yet measured.

| phase | bound | where it is | filed as |
|---|---|---|---|
| parse objects and archives | 0.1 ms over the pull rounds | near the bound | none: 19.3, 19.4 made |
| parse `.tbd` | 0.05 ms | about 0.8 ms (was 1.1 ms): documents in parallel (A6 made); the split, the plan's count and the name decode serial | queue (`Names` from a joined buffer) |
| resolve | 0.15 ms, 0.3 ms with collection | serial on a full link: about 0.7 ms registering, 3 ms collecting and defining; O(change) on an edit | none: see "Native inputs" |
| layout of synthetic sizes | O(1) | at the bound | none |
| export trie | under 10 µs for an executable | at the bound | none |
| rebases and binds | 0.02 ms | rebase opcodes and function starts in parallel pieces, about 0.03 ms; binds serial (400); chained (A2 made) O(slots) | none |
| `__unwind_info` | 0.06 ms | parallel scan, merge and regular pages (A8 made), about 0.1 ms; compressed pages serial; a relink patches it (19.5 made) | none |
| symbol table | 0.1 ms | locals parallel; `String` names | link.md item 11 |
| code signature | 3.2 ms (16 cores) | about 1.05 × the bound | none (A3 made) |
| `__DWARF` | 0 | at the bound: no copy | none (A5 made) |
| relink one function | about 15 µs | O(changes) hashed and patched; `__LINKEDIT` shared and written by changed page (corrected 2026-10-30) | none (19.5, A4 made) |

**Parse** (speed-sol2-link-inputs owns this paragraph and the two parse rows). Objects and
archives cost as in the ELF section; the core copies and names nothing per input (19.3 and 19.4
are made, corrected 2026-10-30 from the code).

**Parse `.tbd`.** `libSystem.tbd` is about 1 MB of YAML: about 33,000 lines in about 40
documents, of which about 30,000 names in the lists the target selects, about 600 KB of names
once decoded. The work, counted:
- Splitting the stream into documents (`documents`): one SWAR newline search and a few compares
  per line, about 25 cycles a line: 33,000 × 25 = 0.8M cycles, 0.24 ms of one core.
- Lexing and parsing each document onto the tape (`parse`): about a byte a cycle, 1M cycles,
  0.3 ms of one core, and about 40,000 nodes of 12 bytes (480 KB).
- Planning (`fold`): a sum of the selected names' lengths, about 2 ns a name: 0.06 ms.
- Decoding (`decode`): each name's core name built, hashed once, probed in `Seen` (a 256 KB
  table) and pushed into the arena, about 17 ns a name: 0.5 ms.

The memory touched is about 2.6 MB (the text, the tape, the arena), 0.01 ms at 400 GB/s, so the
bound is compute: 1M cycles of lexing ÷ 16 cores (0.02 ms) plus 30,000 × 17 ns ÷ 16 of
decoding (0.03 ms), **about 0.05 ms**, budget 0.055 ms. The figure here before (0.3 ms, bound
0.02 ms) counted only the parse; corrected 2026-10-30.

Today (A6 made by speed-sol2-link-inputs): a stream of several documents and at least 64 KiB
(`tbd.rs` `PARALLEL`) is parsed one document per rayon task, each onto its own tape, with its
`Doc` read there too, so libSystem's 40 documents take 0.3 ms ÷ 16 ≈ 0.02 ms. The tapes are
joined into one sized once: a fill of 480 KB (about 0.02 ms of one core) and a parallel copy that
moves each node's subtree end and scratch offset by where its tape lands. The tape, docs,
targets and first error are those of the serial loop, which smaller or single-document files
keep. So the read is about 0.24 + 0.04 + 0.06 + 0.5 ≈ 0.8 ms (1.1 ms before), 16 times the bound,
once per image: a native is read when it first comes on the line and kept after.

What stands between it and the bound, all in `tbd.rs` but the last step:
- **The split in chunks**: each of 16 chunks lists its newline count, its `---` and `...` lines,
  and the first line with content after each `...` and at its start; a serial join over those
  events (about two a document) gives the same documents and line numbers: about 0.015 ms.
- **The plan's count** done in each document's task (each selected list's name count and bytes),
  so `fold` only adds them up.
- **The decode in passes**, as `symbols.rs` `batch` interns: every planned name's core name
  hashed in parallel (one key buffer a worker); deduplicated in 64 shards by the hash's top bits,
  each shard in plan order, which gives the first of each name as the serial `Seen` does; then
  the kept names laid by prefix sums of their lengths and copied into the arena in parallel.
  The last step needs `model.rs` `Names` built from one joined buffer, whose `push` is the only
  way in today (queue, to speed-sol2-link: `Names::from_joined`).

Proof: `read` of the SDK's `libSystem.B.tbd` and of a seeded 1 MB stream of 40 documents, timed
on 16 cores against 0.055 ms; its allocations counted against O(documents), never O(names)
(`read_allocation_budget` holds the single-document path to a constant). The parallel parse is
held to the serial one by `documents_parse_alike_on_workers_and_in_turn` (600 seeded and
mutated streams, the tape, docs, targets and errors equal) and to `tbd/reference.rs` by
`a_large_stream_reads_as_the_reference_does`.

**Resolve** is the ELF figure, corrected the same way: see "Native inputs: reading and pulling".

**Synthesize.**
- Export trie: an executable exports a handful of symbols, under 10 µs. A dylib with 30,000
  exports sorts them by a radix key of the first eight name bytes, then builds the trie in one
  pass: about 0.05 ms.
- Rebases and binds: 300,000 rebases are 2.4 MB of addresses in and, as opcodes, under 1 MB out:
  about 0.02 ms in parallel. Since 2026-10-30 the stream is encoded in parallel pieces (see
  "Rebase opcodes and function starts" below; it was one serial stream, about 0.3 ms). With chained fixups
  (`LC_DYLD_CHAINED_FIXUPS`, pointer format `DYLD_CHAINED_PTR_64_OFFSET`) there is no rebase table:
  each slot carries its own fixup and the next slot's distance, written with the slot by
  `Writer::encode`, and the only table is a 2-byte page start for each of 25,600 pages, 51 KB.
  The core lays and diffs encoded slots (`file.rs` `changed_slots`), and `macho/chained.rs`
  (A2) is the Mach-O `encode` and the `LC_DYLD_CHAINED_FIXUPS` table, with
  `LC_DYLD_EXPORTS_TRIE` in place of `LC_DYLD_INFO_ONLY`. It is chosen as ld-prime chooses:
  macOS 12, iOS, tvOS and Mac Catalyst 15, watchOS 8, DriverKit 21 and every visionOS; other
  platforms and earlier targets keep the opcodes. rustc's defaults are 11.0 (arm64) and 10.12
  (x86-64), and blaze's is 11.0, so the default bytes are unchanged and both paths must be at
  the bound. Imports use `DYLD_CHAINED_IMPORT`, or `_ADDEND` when an addend doesn't fit the
  pointer's signed byte; a slot not 8-aligned, outside `__DATA_CONST`/`__DATA` or both slid
  and bound is an error. `macho/tests.rs` `chained_fixups_slide_and_bind_what_the_opcodes_do`
  decodes the chains by hand (the workspace's `object`, 0.37, reads none) against the opcodes
  of the same image at 11.0, and `a_bind_added_rewrites_two_slots` checks that a new bind
  changes its slot and its predecessor's only.
- `__unwind_info`: 200,000 functions, 800 KB of second-level entries: about 0.06 ms. It was a
  serial `.eh_frame` walk and a serial build, about 1 ms. A8 is made (2026-10-30,
  speed-sol2-link-writers): the split parallel scan from `elf/dynamic.rs` is now `unwind.rs`
  `scan`, which both writers use, and the merge and the regular second-level pages run in
  parallel. See "`__unwind_info` assembly" below. On a relink (19.5) `unwind::build_kept` keeps the last
  table's columns, rows and pages (`unwind::Last`, moved out of the full build, no copy but
  the table's bytes and the functions' addresses) and patches it for a `Delta` from its
  synthesis: it reads again only the `__eh_frame` records and `__compact_unwind` entries
  `written` overlaps, moves each changed FDE or entry in its column in place, and merges again
  only from each changed address to the first unchanged entry after it. It rewrites the rows,
  LSDA entries, pages (at most 4 KiB each) and first-level entries that changed, and the
  sentinel. It builds whole when a row would come or go, a row gains or loses its LSDA, a page
  would split differently, the common encodings or a personality change, a changed FDE or entry
  is not alone at its address or leaves its place in its column, a CIE's encoding or a record's
  length changes, a bind of a personality cell changes, or the last end shrinks. That is
  O(splices + entries written + pages touched × 1,021 rows). Corrected 2026-10-30: there is no
  800 KB copy, since the table is kept and returned as `CowBytes` (`unwind::kept`) and a patch
  writes only the pages it changes. `unwind.rs`'s tests hold each patch to
  `build`'s bytes: a moved function, a moved LSDA and the last FDE grown; a delta from another
  synthesis; an unequal splice; a lost row and a new LSDA; and random LSDA moves over the
  seeded images.
- Symbol table: 200,000 `nlist_64` of 16 bytes (3.2 MB) and 16 MB of names: about 0.1 ms
  memory-bound. The locals were one serial loop, 3 to 5 ms. Now `macho.rs` `locals_into` fills
  them in parallel, one task per 4,096 locals at offsets from prefix sums of the name lengths,
  and the first error is the first local's in order (A1). It is tested against the old loop on
  0 to 10,000 random names, both spellings, and an error in several chunks. The `exported`
  probe per function costs no hash when the set is empty, as in every executable, because
  hashbrown returns early from an empty table. What is left is the core's `Class::Named`
  `String`s, one pointer chase a name, which interning (link.md item 11) removes. Corrected
  2026-10-30 (speed-sol2-link-writers): the names are `Function::name: Option<Arc<str>>`, not
  `Class::Named` strings, and item 11 is about section names. The pointer chase per name is
  removed by a names column in `Placed`, queue 00923.
- Code signature: 100 MB ÷ 4 KiB = 25,600 pages. SHA-256 of a 4,096-byte page is 64 blocks plus
  one padding block, so the bound is 100 MB ÷ (2 GB/s × 16) × 65/64 = 3.1 ms × 1.016 = 3.2 ms
  (0.8 ms on 64 cores), budget 3.5 ms. `macho/sign.rs` hashes 64 pages a task in parallel, about
  1.05 × the bound. `macho.rs` copied the signature into one part per 4 KiB page so a relink
  rewrote only changed pages; now that `file.rs` diffs a part by block (19.1), the signature is
  one part with the same relink writes and no allocation per page (A3). The pages hash the
  chained slots laid over the regions (`sign::View::with_slots`).
- `__DWARF`: the bound is zero for the reason given in the ELF section. It was one 300 MB serial
  copy, 30 ms; now (A5) `image` lays the core's sections through `File::debug` at the same
  offsets and the signature hashes them where they are. `macho/tests.rs`
  `the_debug_sections_are_the_cores_not_copies` checks their bytes and that only the
  signature's hashes grow with them.

**Incremental relink of one changed function.** The ELF changes, plus the rebase or chained slot
words on the changed pages and the signature: about 2 KB in a few pages, and two to three pages
to rehash at 4 KiB ÷ 2 GB/s = 2 µs each, plus one code-directory slot each. The bound is about
15 µs. The writer regenerated every synthetic region (5 to 40 ms, most of it the `__DWARF` copy
and the serial symbol table), and the signature cache compared all 25,600 pages (about 0.5 ms).
Now, for an incremental output, `macho.rs` keeps its last tables (`Kept`) and uses them only for
a `Delta::from` equal to the synthesis they are of (a rollback brings back a state they are not
of), with the same regions:
- The rebase and bind opcodes, the trie and the function starts are the kept bytes when their
  lists have no splice (and, for binds and the trie, the names are the same).
- The symbol table's locals are patched in place (`patch_locals`) for equal-length function
  splices with names of the same length, local or not alike and still in order; otherwise it
  is built again. `__unwind_info` is patched by `unwind::build_kept`.
- The signature (A4) rehashes the pages that overlap the delta's `written` (which includes the
  synthetic regions' changed blocks), the pages of chained slots that changed, and the debug
  sections that aren't the same buffers. It compares only the headers' and `__LINKEDIT`'s
  pages, which are all it keeps, and keeps every other hash.

`the_signing_cache_writes_the_same_file` checks five relinks, at macOS 11 and 12, against an
uncached writer, and `a_relink_patches_the_symbol_table_as_a_full_build_writes_it` that a patch
keeps the kept buffer and gives a full build's bytes. Corrected 2026-10-30
(speed-sol2-link-writers), from the code: `macho.rs` `linkedit_part` clones the kept
`__LINKEDIT` (two reference counts) and writes only the pages whose bytes changed whenever the
kept tables stay in place at the same length. Only a `__LINKEDIT` that moved is a new buffer.
What a relink still spends is building `__LINKEDIT`'s new tables into a scratch buffer that is
kept across links, and diffing the kept chained slots (16 bytes a slot).

**Proof.** As for ELF: each phase timed on the workload, a full link and a one-function edit,
against the bound × 1.1, with the signature also timed alone on 25,600 pages against 3.5 ms on 16
cores, and the relink measured in pages hashed and bytes written.

**State of the art.** ld-prime (Apple's ld since Xcode 15) is parallel and emits chained fixups,
and signs in parallel; it has no incremental relink. lld's Mach-O port builds the export trie,
`__unwind_info` and the signature (`CodeSignatureSection`, parallel over pages) much as blaze does,
with serial symbol tables. mold's Mach-O port (sold) is parallel throughout. None relinks one
function in O(change); after 19.1, 19.5, A2 and A4 blaze does. A5, A6 and A8 are made; by the
arithmetic in the sections below, rebases and `__unwind_info` are within about 1.3 times their
bounds, not yet 10%, and the symbol table waits on queue 00923 (corrected 2026-10-30, which
named link.md item 11 here).

**Made, and filed.** 19.1, 19.2, 19.5, A2, A4 and A5 are made (requests-link-next-writers, not
yet built: the lane made them without a build, so the tests above are written and not yet run).
A6 and A8 are in `v2/requests/requests-link-fuzz.md`; both are made (A8 on 2026-10-30 by
speed-sol2-link-writers, written without a build, like the rest).

### Rebase opcodes and function starts: `macho/linkedit.rs` (speed-sol2-link-writers)

**The work.** With the opcodes (`LC_DYLD_INFO_ONLY`, every target before macOS 12, including
rustc's default of 11.0), 300,000 slid addresses (2.4 MB) become a rebase stream of under 1 MB.
Each address costs a compare with the one before and, at a gap, an opcode with a ULEB. The
binds are about 400. `LC_FUNCTION_STARTS` is 200,000 ULEB deltas, about 400 KB, from 1.6 MB of
addresses.

**The bound.** Memory: about 5 MB ÷ 400 GB/s = 0.013 ms. Compute: about 10 instructions an
address, (300,000 + 200,000) × 10 = 5M ÷ (3 × 3.5 GHz × 16) = 0.03 ms for both tables, 0.02 ms
of it rebases. The budget is 0.033 ms.

**Today** (2026-10-30, written without a build). Both tables were serial, about 0.3 ms and
0.2 ms. The duplicate check is a parallel `par_windows` with `find_first`, so the error is the
first one in order. The rebase stream is cut only at a gap, where a new run begins. The
encoder's cursor before a cut is the segment of the address before it, plus that address + 8,
so each piece starts from exactly the state the serial encoder would be in. Function starts
can be cut anywhere, since each piece starts from the address before it. There are as many
pieces as the pool has threads, at least 16,384 addresses each, so they run in one round. The
join takes the first error in order, then makes a parallel copy. The estimate is 0.03 ms plus
two fork-joins of about 5 µs, within about 1.3 times the bound. Binds stay serial: there are
400, under 5 µs.

**The gap, and how it closes.** The fork-joins are the gap. Running both tables (and the export
trie) as one rayon scope, rather than one after another, would hide one of them. That change is
in `macho.rs`'s `__LINKEDIT` build, which is mine, but it isn't made yet.

**Proof.** `rebases_in_pieces_are_the_one_streams_bytes` covers random runs, unaligned pointers,
shuffled input, a run up to a segment's end, and one run longer than every piece.
`a_rebase_error_is_the_first_in_order` covers a read-only segment, a hole in no segment, and a
pointer twice. `function_starts_in_pieces_are_the_one_streams_bytes` covers function starts.
All three run at 16 threads against the old serial encoders, and each needs a byte-identical
result. To time them: both tables on the workload at 16 threads, against 0.033 ms. ld-prime
and lld write the opcodes serially. ld-prime prefers chained fixups (A2 here), which have no
stream.

### `__unwind_info` assembly: `macho/unwind.rs` `full` (speed-sol2-link-writers)

**The work.** The table is built from 200,000 functions, 210,000 FDEs (from the scan above) and
the native `__compact_unwind` entries (a few thousand from C archives). It merges four sorted
columns (compact entries, FDEs, function starts, the ends of covered ranges) into about 200,000
rows, drops each row that repeats the encoding before it with no LSDA, counts the common
encodings, cuts pages (compressed: up to 1,021 entries and 256 encodings; regular: 511), and
writes 800 KB.

**The bound.** Corrected 2026-10-30: the 0.06 ms above counted only the bytes written, not the
scan or the merge. The scan takes 0.05 ms (see the ELF section). The merge is about 620,000
column entries at about 6 branchy instructions each: 3.7M ÷ (2 × 3.5 GHz × 16) = 0.033 ms.
Pages and writes take about 5 instructions a row, 0.005 ms. So the bound is about 0.09 ms, with
a budget of 0.1 ms.

**Today** (2026-10-30, A8 made, written without a build). These steps now run in parallel:
- the FDE column from the shared scan;
- the sorted and duplicate checks on it;
- the extend and sorted check of the ends column (the radix sort it may need is parallel from
  100,000 keys and skips shared digits);
- the merge, in as many pieces as the pool has threads, cut at function starts. Each piece
  starts from the row the piece before leaves last, computed directly (`Columns::tail`), so its
  rows are the single merge's rows. The first error is in order, and rows and LSDAs are joined
  by parallel copies;
- the non-DWARF encodings, found per piece of rows and counted in order;
- the regular pages, written in parallel, each into its own slice of the output.

A run of 511 DWARF-mode rows becomes a regular page directly (`dwarf_page`), with the same
result as the page walk. blaze's own code is all DWARF-mode FDEs, so on the workload nearly
every page is regular and parallel. Only the compressed pages are serial, because each stamps
the encodings' page-local indexes into a shared table. So is `compact`, over the native entries.
Both are O(native functions), about 0.02 ms here. The estimate is 0.06 ms for the scan plus
about 0.04 ms for the merge, plus about 6 fork-joins (30 µs), about 0.13 ms in all: 1.3 times the
bound. It was about 1 ms.

**The gap, and how it closes.** The fork-joins, mainly. The scan and the merge cut at the same
kind of boundary, a function start. Fusing them, so each piece walks its `.eh_frame` and merges
its own rows in one task, would remove two joins and the FDE column's copy. The compressed pages
could run in parallel with a stamp table per worker (`Encodings` is 256 indexes a page). That
matters only for images that are mostly native.

**Proof.** `rows_merged_in_pieces_are_the_one_merges` checks the merge at 16 threads against one
`merge`, at cuts amid runs the merge leaves out. `dwarf_runs_split_into_the_pages_the_walk_makes`
checks `dwarf_page` against `page` alone. `live_fdes_are_the_records_fdes_in_order` covers the
live FDEs. The existing image tests hold the whole table to gimli and to the patches. To time
it: `unwind::build` on the workload at 16 threads, against 0.1 ms. lld's `UnwindInfoSection`
is the reference for the format and the dedup rule. It is serial apart from its parallel
relocation pass. ld-prime is parallel, but its unwind assembly isn't documented. mold and wild
link ELF and have no `__unwind_info` (sold, mold's Mach-O port, is no longer developed).

## PE/COFF writer and readers (speed-sol2-link-pewasm)

Code: `crates/blaze-link/src/pe.rs`, `pe/coff.rs` (the readers), `pe/image.rs` (the tables and
headers). Not this section's: `pe.rs` `order` (fix-radix) and the sort in `pe/image.rs`
`exports` (fix-orderkeys).

**Workload.** The ELF section's program built for `x86_64-pc-windows-msvc`: a 100 MB image,
200,000 functions with unwind info (200,000 `.pdata` entries; the `UNWIND_INFO` records are
codegen's bytes, placed by the core), 300,000 rebased addresses (`DIR64`), and 400 imports from
10 DLLs. The CRT and UCRT objects are 500 COFF members pulled (60,000 symbols, 250,000
relocations). The import libraries are `kernel32.lib`-sized ones plus one umbrella of 30,000
import members (`windows.0.52.0.lib`, which `windows-targets` links). The exe exports one symbol
(`__blaze_live`); the DLL variant exports 10,000. The edit workload is one function's body
changing. Budgets are the bound × 1.1. The figures are arithmetic; this pass ran no benchmarks.

| phase | bound | where it is | what the core spends |
|---|---|---|---|
| COFF object read | 0.1 ms over the pull rounds (the ELF figure: the same bytes) | near the bound | P18 (queue 00930), P22 (00932) |
| import libraries | 0.15 ms: the member-header walk, serial by format | the parse and the grouping by DLL are parallel; the header and index walks are serial, about 0.6 ms | P22 (00932) |
| resolve | the ELF row | the core's | none |
| layout of synthetic sizes | O(1): sizes come from `Counts` | at the bound | none |
| base relocations | 0.02 ms | at the bound (was 3 of 16 cores busy) | none |
| `.pdata` (`.xdata` is codegen's) | 0.03 ms | at the bound (was one thread, about 2 ms) | none |
| import and export tables | 5 µs (400 imports); 0.05 ms (10,000 exports, the sort) | at the bound | none |
| debug directory | under 1 µs | at the bound; no PDB is written | none |
| relink one function | about 1 µs | O(changes): patched or kept tables | none |

**COFF object read.** Each object is read in place: one pass over its section headers (which
sizes every column) and one over its symbol table, names pushed once into its `Names`,
relocations and COMDAT members as runs of one column, section bytes as `Data::Input` ranges.
That is the ELF reader's shape and the same bytes, so the bound is the ELF row's 0.1 ms. The
core reads the pulled members in parallel. The reader is near the bound but for two things the
model forces. First, a long import member names only `__IMPORT_DESCRIPTOR_<lib>`, so the DLL is
looked up behind `Pe::descriptors`, a `Mutex`, once per member (P18, queue 00930). Second, each
import's `"<hint> <name>"` is a `String` of its own (P22, queue 00932).

**Import libraries.** An umbrella library is 30,000 members of about 100 bytes: 3 MB, or 0.01 ms
at socket bandwidth. Parsing a short import member is about 150 instructions, so 30,000 members
on 16 cores is 4.5M ÷ (3 × 3.5 GHz × 16) = 0.03 ms. The member headers are a chain, each giving
the next one's offset: 30,000 × 5 ns = 0.15 ms on one core, which is the bound. Today
(`pe/coff.rs` `import_library`) every member is parsed in place on every thread, its version made
there. The imports are then sorted by (DLL, member) in parallel, each DLL's `Shared` is built
from its run on its own thread, and the DLLs are put in the order each is first met. The error
given is the first in member order: the first member that fails to parse, unless adding an
earlier one fails. This was a serial pass of one hash and three pushes a member, about 60 ns,
or 1.8 ms. What stays serial is the member-header walk, twice in `archive` (once to size the
columns, once to read), and the index walk: 2 × 30,000 + 60,000 symbols at about 5 ns, so
0.6 ms, 4 times the bound. To reach 90% of it, walk the headers once, into the columns, and
read the index (a table of offsets) on every thread. This is open in this file. The library is
read once per daemon (an edit doesn't read it), so it costs full links only.

**Resolve** is the core's (see "Native inputs"). **Layout**: every synthetic size is a function
of `Counts` (`pe/image.rs` `synthetic`), constant work a region.

**Base relocations** (`pe/image.rs` `relocations`). 300,000 addresses read (2.4 MB), 0.6 MB of
entries and block headers written: 3 MB ÷ 400 GB/s = 0.008 ms, or about 10 instructions an
address = 3M ÷ (3 × 3.5 GHz × 16) = 0.02 ms. Today the sorted addresses are cut at page
boundaries into pieces of at least 8,192 addresses, about four a thread (`reloc_piece`). Each
piece is written on its own thread with a region cursor rather than a search per address. The
pieces are then copied in parallel into one zeroed buffer of the exact size. Before, pieces were
65,536 addresses, so 300,000 addresses kept 5 of 16 cores busy. The core's addresses come sorted;
a radix sort runs only when they don't.

**`.pdata`** (`pdata`). 200,000 functions read (about 10 MB of `Function`) and 2.4 MB of
entries written: 12 MB ÷ 400 GB/s = 0.03 ms. Today pieces of 4,096 functions are staged on every
thread into one zeroed buffer. Each piece is then merged with the objects' entries that sort
before the next piece, straight into its place in the output, and the overlap check is a
parallel `find_first` (the first overlap in order, the same error). The staging buffer and the
output are 2.4 MB each and stay in L3, so the DRAM traffic is the bound's. Functions out of
order (the core places them in order) are sorted and merged on one thread, the same bytes. It
was one serial pass, about 2 ms.

**Import and export tables** (`imports`, `exports`). 400 imports and 10 DLLs are linear: about
5 µs. 10,000 exports are linear plus fix-orderkeys' radix sort: about 0.05 ms. Each import's
name is parsed once and borrowed, and nothing in them allocates per entry.

**Debug directory** (`debug`). 28 + 24 bytes and the PDB's name, constant. blaze writes no PDB.
That is a missing feature (lld-link's PDB writer is the reference), not a speed gap; when one is
written it is a phase of its own.

**Relink one function.** An incremental output keeps its last `.pdata`, `.reloc`, import and
export directories (`Pe::last`, `image::Last`). On a `Placed::delta` from them with no region
moved, each kept table is patched as follows:
- `.pdata`: each spliced function's entry is written into its slot, then checked to sort strictly
  between its neighbours and overlap neither. That is 12 bytes and two reads a function, and the
  check proves a build makes the same bytes.
- `.reloc`: each spliced address's entry is written into its block, after these checks: the block
  is still its page; the address keeps its order among its neighbours; it is no repeat, and it
  makes none.
- The import directory is reused as it is when the names, libraries and GOT are the same.
- The export directory is reused with its time stamp rewritten when the names are the same and
  it names the image where it did.

A synthetic region may move when no other does, so each table also checks that its own region,
or (for `.reloc`) the synthetic layout, is where it was. Anything a patch can't show to be the
build's bytes is built: the same output, slower, never a different one. With the imports
checked, the O(GOT + imports) checks of a full synthesis are skipped too. So an edit is
O(spliced entries + regions): about 1 µs for one function. The tables go back as `CowBytes`, so
the core's block diff finds the pages that changed.

**Proof.**
- `incremental_tables_are_patched_to_a_builds_bytes` holds every table of a patched synthesis to
  a fresh build's bytes, and checks that each one counts as patched or reused. It also checks the
  rejections: a function moved out of order, an address onto its neighbour, changes from another
  synthesis.
- `pdata_in_pieces_is_the_sorted_merge` checks the pieced build on 12,365 functions (a third with
  no unwind info), interleaved with 1,000 unsorted objects' entries, against a sort of all the
  entries. It runs on one thread and on many, through a patch, and with functions out of order.
- `incremental_base_relocations_are_a_builds_bytes` covers patches within a page, onto a neighbour
  and back, and across a page.
- `base_relocations_are_the_same_bytes_in_any_number_of_pieces` and the allocation test are
  unchanged.

To time it: `Tables::of` on the workload at 16 threads, a full one against 0.05 ms and an edit
against 2 µs. The references: lld-link, which rebuilds both tables on every link (it accepts
`/INCREMENTAL` and links in full), writes `.reloc` on one thread and sorts `.pdata` with a
parallel sort; mold, for the design of pieces written in parallel at prefix-sum offsets; wild,
for its parallel writers. link.exe's `/INCREMENTAL` pads functions and patches thunks; blaze
patches the tables instead.

## Wasm writer and readers (speed-sol2-link-pewasm)

Code: `crates/blaze-link/src/wasm.rs`, `wasm/object.rs` (the reader), `wasm/reloc.rs` (relocation
apply), `wasm/code.rs` (the code header), `wasm/module.rs` (the module), `tests/wasm_imports.rs`.

**Workload.** The ELF section's program built for `wasm32-wasip1` (and `wasm32-unknown-unknown`,
the same without WASI imports): 200,000 named functions (80-byte names, 16 MB) in a 20 MB code
section, 600,000 type fixups, 2,000 distinct types, 8 MB of data segments, 40 WASI imports, and
`producers` and `target_features` from 300 pulled wasi-libc members. The edit workload is one
function's body changing.

| phase | bound | where it is | what the core spends |
|---|---|---|---|
| wasm object read | 0.01 ms (libc's members: about 1 MB of code) | near the bound | none |
| relocation apply | 0.12 ms (1 million fixups) | near the bound: one dynamic call a fixup | W11's batch (optional, the core's) |
| code section | 0.15 ms | near the bound (was about 25 ms on one thread) | none |
| name and custom sections | 0.1 ms | near the bound (was about 5 ms) | none |
| the module write | 0.4 ms | near the bound | none |
| relink one function | about 2 µs | the module is rewritten: 0.4 ms | open in `module.rs` (W12) |

**Wasm object read** (`wasm/object.rs`). Rust's code comes from codegen, not from objects, so the
reader sees only libc's members: about 1 MB of code and data, 0.01 ms. Each function is copied
once, behind the codegen header that the writer reads every code piece by. Segments and custom
sections are `Data::Input` ranges. The core reads the members in parallel, so this is near the
bound.

**Relocation apply** (`wasm/reloc.rs`, through `Writer::apply`). A fixup is a padded 5-byte LEB
or a 4-byte word: about 20 instructions, so 1 million on 16 cores is 20M ÷ (3 × 3.5 GHz × 16) =
0.12 ms. The core applies them per piece in parallel, with one dynamic call each. Batching them
per piece (W11) would save the call, which is a few percent.

**Code section.** 20 MB read and written: 40 MB ÷ 400 GB/s = 0.1 ms. Plus 200,000 headers
parsed in place (about 50 instructions each) and 600,000 fixups (about 10 each): 16M ÷ (3 × 3.5
GHz × 16) = 0.1 ms. Together that is about 0.15 ms. Today (`module.rs` `write`) the functions are
handled in pieces of 4,096 on every thread (`piece`). Each piece interns its types into its own
map, checks every fixup's place, and sizes its entries. The pieces' types are then interned in
order after the imports', a few per piece, so each type's index is where one pass in order first
meets it. That is the serial part: about 50 pieces × 100 types × 20 ns = 0.1 ms in the worst
case, and the doc's 10% allows it only when a piece meets few new types, as Rust code's do.
Each piece's bodies are copied and patched on its own thread into its place in one zeroed
buffer. `functions` reads the headers in parallel. Its placement checks (overlap, and stray code
between functions) run in parallel, and the first error in address order is the one given. It
no longer sorts: the core's functions come in address order, with `__wasm_call_ctors`'s region
first. Before, the code section took these serial passes:
- type interning: 200,000 functions and 600,000 fixups at about 20 ns, 16 ms;
- a radix sort of every function's 72-byte unit, about 5 ms;
- the per-function split, the checks and a memset of the code and data, about 4 ms.

**Name and custom sections.** 16 MB of names written once: 0.04 ms, plus 200,000 entries at
about 30 instructions each, 0.04 ms. Each piece writes its function names into place on its own
thread, from sizes `piece` counted. The globals' and segments' names and the merged `producers`
and `target_features` are small and serial. Custom sections' parts are copied as runs, at most
256 KiB a run. Before, the names were copied three times on one thread, about 5 ms.

**The module write** (`write`, `Layout`, `Run`). The output is 45 MB: written once (0.11 ms),
its sources read once (0.11 ms), and its pages first touched (11,000 pages × 0.25 µs ÷ 16 =
0.17 ms): about 0.4 ms. Today every size is known before a byte is written. The module is laid
out as runs: small sections and every header go into one `head` buffer in order, and the big
sections are `Run`s. Then one zeroed buffer of the exact size is allocated and every run is
written into it in parallel: the function section's type indices and the element segment's
slots per piece or per 65,536 slots, the code, the data, the custom parts and the names. Nothing
is allocated per function but a set of columns per piece. The serial work is O(types + imports
+ exports + segments + pieces).

**Relink one function** (W12). The bound is one body patched and the section sizes rewritten:
about 2 µs. Today the module is rewritten whole on every link, 0.4 ms at the bound of a full
write, then diffed by the core block by block. To reach the bound, `write` keeps the last module
and its per-function offsets, and patches when the `Placed::delta` is from it and these hold:
`same_names`, `same_regions`, and every spliced or written function keeps its size, type and
export. The model has what this needs now; it is open in `module.rs`.

**Proof.**
- `many_pieces_write_what_one_pass_would` builds 12,293 functions in four pieces. It holds the
  function, code, element and name sections to the command's own, with each extra function's
  entries after them as one pass in order writes them, on 1 thread and on 8.
- `uleb_runs_are_sized_exactly` checks the closed form of the element segment's size against
  summing every ULEB.
- `one_thread_and_many_write_the_same`, `the_same_image_is_the_same_module`, `a_command` (whose
  module `object` reads) and the error tests are unchanged.
- `a_module_is_written_in_a_constant_number_of_buffers` holds the allocations from 100 to 2,000
  functions.

To time it: `Writer::file` on the workload at 16 threads against 0.45 ms. The references:
wasm-ld, which writes each output section's body on its own thread (`parallelForEach` over
sections), but its code section on one; wild and mold, for runs at prefix-sum offsets in one
buffer. wasm-ld has no incremental mode.

## Native inputs: reading and pulling (speed-sol2-link-inputs)

Code: `crates/blaze-link/src/native.rs` (taking inputs, archive indexes, pull rounds, the
resolve graph, shared-library exports), and the readers it calls: `elf/read.rs`, `elf/read/`,
`macho/input.rs`, `macho/archive.rs`, `macho/dylib.rs`, `macho/tbd.rs` (its parse is in the
Mach-O section above). The workload is the ELF section's natives: the crt objects, `libc.so.6`
(3,000 dynamic symbols), 20 MB of C archives in 10 archives with about 30,000 index entries,
of which 500 members are pulled over about 4 rounds (60,000 symbols, of them about 40,000
globals; 250,000 relocations, about 150,000 distinct section-to-symbol references; 50,000
sections, 30,000 kept). On Mach-O, `libSystem.tbd` instead of `libc.so.6`. The figures are
counted off the code; nothing was run. A native is read when it first comes on the line and
kept for the image's life, so on an edit none of this runs but the resolve graph's O(change)
path.

| component | bound | today | within 10% |
|---|---|---|---|
| taking new inputs | the largest input's read | one task an input, bytes shared | yes |
| archive indexes | 30,000 × 7 ns, archives in parallel: 0.03 ms | the same, kept | yes |
| searching the archives | 5,000 × (3 + 5) ns ÷ 1: 0.04 ms | 5,000 × (3 + 10 × 5) ns: 0.27 ms | no: 10 probes a name |
| reading pulled members | in "Parse" above | one task a member, a round at a time | yes |
| the resolve graph, full link | about 4 ms ÷ 16: 0.3 ms | about 4 ms of one core | no: serial |
| the resolve graph, edit | O(names gained) | O(names gained) | yes |
| shared-library exports | 33,000 × 15 ns ÷ 16: 0.03 ms | 33,000 × 15 ns: 0.5 ms, once a library set | no: serial |
| Mach-O objects and archives | in "Parse" above | constant allocations an object | yes |
| dylibs | the trie's bytes once: 1 MB ÷ 1 B a cycle ÷ 16 | one walk, one task a dylib | yes for many dylibs; one large dylib is serial |

### Taking new inputs (`Stage::inputs`)

Work: for each native the change adds, one `Writer::read` and one `check_file` (a pass over
its sections, relocations, symbols and groups, O(items)), one FxHash probe of its `ContentId`
to find it twice on the line, and its place in the sorted line. An archive's read is its member
headers and index only (members are read when pulled): about 2,000 headers and 30,000 entries,
about 1 MB touched, 0.1 ms for the largest. The bound is the largest input's read, or the sum
÷ 16 where larger: reads are independent. Today the reads and checks run one rayon task an
input; the bytes are the caller's `Arc<[u8]>`, taken as a count, never copied (19.3); nothing
is formatted unless an input fails. At the bound. Proof: allocations per new input counted
(the reader's constant plus one `Arc<NativeFile>`), and `inputs` timed with 40 natives against
the largest read × 1.1.

### Archive indexes (`archive_index`, `indexes`, `Table`)

Work: for each searched archive, once per image, one FxHash of each index name (about 20 bytes,
3 ns) and one insert (one probe, about 4 ns in L2): 30,000 × 7 ns = 0.2 ms of work, archives
in parallel, the largest (about 10,000 entries) 0.07 ms. No linker that reads only the members
it pulls does less: lld interns each index name once as a lazy symbol. The index is kept
(`Read::Index`) and rebuilt only when the archive changes.

`Table` (also the exports' and a graph build's COMDAT groups) kept a `u64` hash column and a
`u32` value column, two cache lines a probe and 24 bytes a name at half full. Now (2026-10-30)
a slot is one `u64`, the hash's high half over the value: one line a probe and 16 bytes a name.
A probe starts at the hash's high bits, which the high half holds, so the table grows without
the names. Names whose hashes share the high half are told apart by `eq`, which
`names_whose_hashes_share_the_high_half_are_told_apart_by_name` tests across growth. Proof: the
index of a 30,000-entry archive timed against 0.2 ms × 1.1 on one core; 0 allocations a lookup.

### Searching the archives (`search`, the pull loop of `rebuild`)

Work: each pull round, each name newly strongly undefined (about 5,000 over the rounds) is
looked up in the searched archives in line order until one has it. A name no archive defines
(an import, or one defined later in the round) is probed in all 10. It used to be hashed
again for each archive: 5,000 × 10 × (3 + 5) ns = 0.4 ms. Now `search` hashes it once and
probes each index with that hash: 5,000 × (3 + 10 × 5) ns = 0.27 ms.
`a_name_is_searched_for_in_line_order_as_a_scan_of_the_indexes_finds_it` holds it to a scan of
the indexes over seeded archives, with 0 allocations a search.

The bound is one hash and one probe a name, 0.04 ms, as lld and mold look a name up once in
one symbol table. That takes one index for all the archives, built from the archives' kept
tables (their slots, without hashing again) in line order, first archive winning: 30,000
inserts, 0.15 ms, each time the set of searched archives changes. On a cold link that costs
about what it saves (0.23 ms), so it pays only where the graph is rebuilt more than once for
one set of archives. It is the next step if `proof` shows the search over its budget. Proof:
probes and hashes per searched name counted on the workload (1 hash; today up to 10 probes).

### Reading pulled members (`read`, `read_misses`)

Each round's members are read, checked, their `.eh_frame` split, their symbols interned (one
batch, sharded) and indexed (`Parsed::new`), one rayon task a member. A member is named only in
an error (19.4). The work and bound are the "Parse" figure; the rounds add one fork and join
each. Proof: as "Parse".

### The resolve graph (`register`, `aliases`, the `Marker` walk, the definitions)

Work on a full link, counted off `rebuild`:
- registering each loaded object's globals in link-line and round order: one slot probe
  (`FxHashMap<Sym, u32>`, a multiply and an L2 hit) and the strength rules: 40,000 × 15 ns =
  0.6 ms;
- weak aliases and the image beside: O(slots), 0.1 ms;
- collection: from the roots, each kept section's distinct references (150,000), each a
  `target` of two or three dependent loads: 150,000 × 10 ns = 1.5 ms; then `note_native` reads
  them again: 0.75 ms;
- what came and went: every section of every load checked, 50,000 × 5 ns = 0.25 ms; and every
  local and global defined again, 60,000 × 10 ns = 0.6 ms.

That is about 4 ms of one core. It ran with the slot map grown by doubling, which rehashes
every slot each time (about 0.3 ms more). Now `Parsed::globals` sizes it once for each round
and once for the seeds (`Build::reserve`).

The bound is that work ÷ 16, about 0.3 ms: mold resolves each file's symbols in parallel
against one concurrent table, the winner of a name chosen by a priority the file's place gives,
and marks live sections with a parallel walk over atomic flags. The rules to keep are lld's,
which depend on order only through the winner of each name and group: of equal strength, the
first load in line and round order; the first duplicate in that order is the error. So the
parallel form is:
- each round's slots numbered by sorting the round's `(sym, load, symbol)` keys (radix) and
  taking each sym's first;
- each slot's winner the least packed key (strength descending, load, symbol), a parallel
  reduction; a duplicate is a slot with two strong keys, the error the one first in load order;
- COMDAT groups the same, keyed by `(load, group)`;
- collection a parallel walk setting flat section and frame flags once with `fetch_or`. The
  marks are a set, and `rebuild` makes pieces in load order whatever order they were marked
  in, so the image is the same.

What stands in the way: the marks are `CowFlags`, copy-on-write chunks shared with the states
a stage was cloned from, so a parallel walk needs an atomic bitset for the build, copied into
the `CowFlags` once at its end. This is about 500 lines in `native.rs`, with a differential test
against the serial graph on seeded links (strengths, groups, commons, aliases, duplicates). It
is not written in this pass, in which nothing is compiled before the tree's integration: a
resolver that differs from lld's order in any case links a different program, so it should
land where it can be run against the serial one.

On an edit the graph is not rebuilt: `resolve` returns at once when no native and no
definition changed and no reference moved a name's outcome (`settled`), and `grow` resolves
only the names Rust newly refers to against the kept graph, marking only what they reach: O(names
gained + what they reach), at the bound. Proof: a full link's graph build timed on the workload
against 0.3 ms × 1.1 on 16 cores; an edit's `resolve` against O(gained) (a counter of slots
probed), and 0 graph builds for an edit that changes no native.

### Shared-library exports (`Exports::build`)

Work: when the line's shared libraries change (once per image), every exported name of every
library, flattened through re-exports, is hashed, looked up and, when new, copied into one
arena with its version: `libSystem.tbd`'s 30,000 names or `libc.so.6`'s 3,000, about 15 ns a
name, 0.5 ms for libSystem. It made two probes a name (a `find`, then an `insert`); now one
`insert` that compares names. The bound is the readers' own work: `tbd.rs` already builds an
index of the names (`Seen`) to deduplicate them, and the core could keep it (`Shared::index`,
request 7 from speed-link-edge-macho, queue 00004), so `Exports` would hash each imported name
once and probe each library in order: 400 imports × a few probes, under 10 µs, with no arena
copy. That is a `model.rs` field (core) plus every reader filling it (`tbd.rs`, `dylib.rs`,
`elf/read/shared.rs`, the PE import libraries). Proof: `Exports::build` timed on libSystem
against 0.5 ms today; with the index, the lookups counted per import.

### Mach-O objects and archives (`macho/input.rs`, `macho/archive.rs`)

As `speed-link-edge-macho-input.md` counts them: tables borrowed from the input, one pass over
the nlist table into columns sized once, atoms from one sort, every address-to-atom lookup a
binary search, relocations decoded in place, `__eh_frame` scanned once. An object read
allocates a constant number of times, plus one `format!` for each name without a leading
underscore past 255 bytes (none clang writes); an archive read a constant number (the
headers, members, index and arena). This is wild's shape (columns from header counts) and
mold's (tables read in place), so at the bound of their work, which is in "Parse". What remains
per section is the `Class::Named` name `macho.rs` `classify` formats (link.md item 11, queue
00006, the core's). Proof: `seeded_objects_read_as_the_per_section_model_reads_them` and the
allocation budgets those notes list.

### Dylibs (`macho/dylib.rs`)

A dylib's exports are read from its export trie (or its symbol table when it has none): one
walk of the trie's bytes, each node visited once (a `BitSet` of node offsets refuses a cycle),
each name one buffer (256 bytes inline) cut back to the edge's parent, the exports' column
sized once (one export per 6 trie bytes at most), and the names pushed into one arena that
starts at twice the trie's bytes and doubles past that, a few times a file at most. Its
allocations are constant but for those doublings. That is a byte a cycle or better:
a 1 MB trie is 0.3 ms of one core, and dylibs are read one task each. One large dylib is a
serial walk: the trie's subtrees are independent, so a walk splitting at the root's children
would divide it by their count, but the SDK ships `.tbd` stubs, not dylibs, for every system
library, so a large dylib on a link line is rare. Proof: `trie_differential` against the
reference walk, and a read of a 30,000-export trie timed against 0.3 ms.

## The core: stages, lists, file and table (speed-sol2-link)

Covers `crates/blaze-link/src/` outside the writers, the native inputs and PE/wasm: `lib.rs`,
`stage.rs`, `place.rs`, `write.rs`, `file.rs`, `state.rs`, `cow.rs`, `symbols.rs`, `intern.rs`,
`reloc.rs`, `commit.rs`, `table.rs`, `lazy.rs`, `premap.rs`, `sanitize.rs`, `live.rs` and
`target/`. The workload is the ELF section's: 200,000 functions, 300,000 slid addresses, 210,000
FDEs, 100 MB of image, and an edit that re-places 1 to 10 functions. Figures are arithmetic from
the code and "The machine"; nothing was run (2026-10-30). Each paragraph gives the work, the
bound, today's count, the gap, and what `proof` measures.

| component | bound | today | within 10% |
|---|---|---|---|
| stages of an edit (`inputs` to `sweep`, `place` incremental) | O(fresh pieces + their fixups), about 1 µs | O(change): every loop runs over `fresh`, `removed` or `touched` | yes |
| writer lists' splice (`Owned::splice`) | about 3 µs (k entries) | parallel copy of n entries, about 50 µs | no: needs a chunked `Placed` |
| the cells' rebase list | O(dirty cells) | O(dirty cells) (was O(slots)) | yes |
| stale functions (`update_lists`) | O(changed frame pieces) | parallel scan of n owners, about 0.13 ms | no: needs a reverse index |
| debug rows (`Stage::debug`) | O(changed lines) | parallel rebuild of every row, about 0.1 ms, then a serial encode | no: 00913 to the writers |
| lineage (full link) | one radix pass over 8-byte prefixes | the same | yes |
| file write, full link (`file.rs`) | 14 ms (100 MB at 7 GB/s) | the write plus one more copy, about 0.6 ms on 16 cores | yes on NVMe; 00009 to `core` removes the copy |
| live message (`live.rs`) | O(written pieces) | O(written), radix-sorted | yes |
| jump-table stubs (`table.rs`) | 4.8 MB written in parallel; a call is 2 dependent loads and a branch | the same; 4 instructions below 4,096 slots | yes |

**Stages of an edit.** The work is the fresh and removed pieces and their fixups: about 10
pieces and 200 fixups, so 1 µs. `stage.rs` `fixups`, `positions` and `sweep` sort their sets with
`blaze_ids::radix` (request 00146), and `place.rs` places fresh pieces by class and relays only
the ordered classes. Within the bound. Proof: count the pieces and fixups an edit touches,
against those it changed.

**Writer lists.** `Placed` hands each writer contiguous slices (`functions`, `rebase`, `got`,
`binds`). An edit changes k ≈ 10 of n = 200,000 functions and about 30 of 300,000 rebase
addresses. With slices, the least an edit can do is copy each list once: about 13 MB of
`Function`s and 2.4 MB of rebase, which is 40 µs at 400 GB/s. `Owned::splice` now finds the
dropped entries in parallel, plans the gaps with one binary search per new entry, and copies
the kept runs with `par_extend` above 16,384 entries. That is the copy at memory bandwidth, plus
one reference count per `Function::name` (about 60 µs ÷ 16). The serial walk it replaces is kept
under `#[cfg(test)]` as the reference of `a_planned_splice_gives_the_serial_lists_and_runs`. The
gap against an O(k) bound of about 3 µs is about 20×. Closing it needs a persistent chunked list
behind `Placed` (each writer reads chunks, an edit copies the 1 or 2 chunks it touches), which
changes every writer's interface; it is not done here. Proof: the time of `update_lists` on a
one-function edit at n = 200,000, and the bytes it copies.

**The cells' rebase list.** The cells piece rebases each filled slot. `write_cells` used to
rebuild the list whenever any cell changed, which is O(slots). Now `filled_slots_changed` rebuilds
it only when a slot is filled or emptied, which is O(dirty cells).
`the_cells_rebase_list_is_every_filled_slot_after_each_edit` checks the list after each edit.
Proof: a moved function's edit writes no rebase list.

**Stale functions.** `update_lists` finds the functions whose FDE or unwind piece was rewritten.
Today it scans every function's owner in parallel with a binary search per function over the
radix-sorted changed symbols: 200,000 × about 10 ns ÷ 16 = 0.13 ms. The bound is O(changed
frame pieces), using a map from a frame piece to its function. Not done: it adds a map that the
state would have to keep. Proof: the scan's time against the number of rewritten frame pieces.

**Debug rows.** With `Debug::Lines`, every function's line rows are rebuilt on each link. At
about 10 rows a function, that is 2 million rows of 16 bytes. Rebuilding is 32 MB, 80 µs at
socket bandwidth; the O(change) bound is the changed function's rows. `Stage::debug` now builds
rows in parallel. Each task of 1,024 functions numbers its files by first appearance, a serial
merge in task order gives them the numbers a serial pass would, and the first error in order is
the one returned. `line_tables_across_many_tasks_name_each_rows_file` reads the result back with
gimli. The encode (`dwarf::sections`) is parallel since 2026-10-30 (request 00913, made). The
chunked-CU design that would reach O(change) is written down, not built, in "The debug line
program and subprograms" above. Proof: rows built and
bytes encoded on a one-function edit.

**Lineage.** A full link hashes the image's input keys in byte order. `lineage_order` radix-sorts
on an 8-byte big-endian prefix and compares whole keys only within equal prefixes, so the bytes
hashed are those of a full sort (`the_lineage_order_is_the_keys_byte_order`). One pass over 16
bytes a key; within the bound. Proof: the time of the sort against `sort_unstable` at 2 million
keys.

**File write.** A full link writes 100 MB: 14 ms at 7 GB/s. `file.rs` copies each region once
more into the 8 MiB runs `Effect::WriteAt` takes: 100 MB at 10 GB/s a core is 0.6 ms on 16
cores, within 10% under direct I/O. Through the page cache it is a second pass of memory
bandwidth. `WriteShared` (request 00009, reassigned to `core`) writes from the region's buffers.
An edit writes only the paged ranges it changed. Proof: bytes copied per byte written, and the
wall time against the device's rate.

**Live message.** The message carries the written pieces, borrowed (`PatchRef`), and is encoded
once. Its code, data, frames and windows lists are radix-sorted by address (request 00906), so
the runtime's check is one pass. `a_patch_puts_its_frames_and_literals_in_the_patch_regions_and_ends_its_frames`
asserts the order. Proof: the message's bytes against the bytes written.

**Jump-table stubs.** A full link writes 200,000 stubs of 24 bytes in parallel: 4.8 MB, at the
bound. A call costs the two dependent loads (the cell, then the slot's entry) and a branch. On
AArch64, `write_stub` now uses 4 instructions below 4,096 slots and 5 below 2^21 (an `add …, lsl
#12`), and keeps the `movz`/`movk` form to 2^29 (request 00905). The stub stays 24 bytes, with
`brk #0` after its `br`. `an_aarch64_stub_reaches_any_of_2_pow_29_slots` interprets every form.
Proof: instructions retired a call through a slot, against a direct call plus two loads.

**Also applied (2026-10-30).**
- **00931, a correctness fix.** In a PE image, `Class::Tdata` is now an ordered class. A new
  thread-local in an incremental link lands before `.tls$ZZZ` (`_tls_end`), where a full link
  puts it, instead of past the end of the CRT's template.
- **00932.** `Shared::versions` names live in the `Shared`'s `names` arena, so there is no
  `String` per version.
- **00911.** `Names::append` and `Name::shifted`, for the `.tbd` decode on workers.

**Open, by design.** 00923, a names column in `Placed`, waits on the chunked `Placed` from
"Writer lists". With contiguous lists it would add an O(names) splice to every edit.

**Refused, with reasons (2026-10-30).**
- **00903, patch FDEs in `Class::Patch` for Mach-O and native ELF.** Both would lose unwinding
  when the incremental image is launched again from disk. Mach-O `__unwind_info` names an FDE
  only by its offset in `__eh_frame`, and `build_hdr` lists only core functions' FDEs outside
  `.eh_frame`.
- **00904, a W+X PE patch section.** It changes the W^X policy, which needs a maintainer's
  decision.
- **00907, `MAP_JIT` on macOS.** It needs hardware to check.

## blaze-splice, blaze-in-place, cranelift-object (speed-sol2-link)

**blaze-splice** is v1's splice of an executable, used by `blaze-engine`. An edit's bound is
O(patch): about 2 KB placed and written. ELF and PE are O(patch) apart from two costs. Each
splice writes the unwinder's search table again with the patch's frames added (O(FDEs), 1.7 MB
at 210,000 FDEs). Sections the appended code reaches are moved with `copy_within`. Wasm writes
the whole module again (O(module)). These gaps are over 10%, and hot_lint reports hundreds of
unannotated allocations. They are not worked: when it can't splice, the crate's caller links in
full, which is a fallback. v2's answer is blaze-link's incremental link above, so the long-term
design is to retire the crate rather than tune it. Proof, while it lives: `BLAZE_STATS`'s
`splice-us` phases against the patch's bytes.

**blaze-in-place** waits on the daemon's lock (one `open`, `flock` and two `stat`s, about 1 µs)
and copies an executable keeping its holes. The bound is the copy's data runs at the
page-cache rate, and it does that. Within the bound. Proof: bytes copied against the file's
allocated bytes.

**vendor/cranelift-object** is the object writer of v1's `hir-ty-codegen` and of `blaze-wasm`,
not on blaze-link's path, which takes `CompiledFunction`s. Its work per function is one append
of the bytes and a relocation record. `finish` passes once over the relocations, and `emit`
serializes, which is one more copy of every section. The bound is one copy, so the bytes are 2×
the bound. That is under 0.1% of the codegen that produced them, and the second copy is in the
vendored `object` crate's writer. Not worked. Proof: bytes copied per byte emitted.

## The live runtime: `crates/blaze-live/` (speed-link-edge-sol-live)

The runtime that takes patches is `blaze_live::slots::Live` (the jump table: a stub per
function loads the table cell, then its slot, then jumps). `blaze_live::Live` (v1, a jump
written over each replaced function's entry) is still used by blaze-engine, the demos and the
`live*` harnesses; it is measured here only for its stop. Figures are from "The machine",
except where a line says otherwise, and none is measured yet. `T` is the number of other cores
running the program; `8` where a total needs a number.

Workloads:
- **One-function edit**: about 200 B of code, 1 cell, 1 FDE (about 40 B), a literal or two,
  and a message of about 1 KiB.
- **1,000-function patch**: about 200 KB of code, 1,000 cells, 1,000 FDEs (about 40 KB),
  about 50 KB of data, and a message of about 300 KB. The program's table has `S` slots, one
  per function of the image; `S` = 100,000 is 800 KB.

### A patch's apply, from message to new code running

The bound, on Linux x86-64:
- **Read the message once**: `openat` (0.4 µs) + `statx` (0.15) + one `read` (0.15 +
  bytes ÷ 10 GB/s) + `close` (0.15) = 0.85 µs + 0.1 ns a byte. That is 0.95 µs for the edit
  and 31 µs for the 1,000-function patch. One read into a buffer sized by `statx` is the least
  that reads a file of unknown length once: without the size it takes a second `read` that
  returns 0, which costs the same. Checking and walking the bytes in memory is about
  300 KB ÷ 110 GB/s = 3 µs.
- **Code**: copy the bytes once, through a writable second mapping of the patch region: 0
  system calls, 0 shootdowns, 200 KB at about 50 GB/s = 4 µs. x86 needs no cache maintenance
  for code at addresses no core has run.
- **Data the patch writes in place** (new statics, relocated words, the image's data cells):
  in writable pages, 0 system calls.
- **Cells**: `C` relaxed 8-byte stores into a table the cell doesn't point at, then **one
  release store** of the cell, which is the switch. That is 1,000 stores, 1 to 8 µs depending
  on how much of the table is in cache.
- **Frames**: one `__register_frame` per patch's run of FDEs (libgcc reads the run when it
  first unwinds through it).
- **The answer, written once**: `openat` with `O_CREAT` (0.4) + `write` (0.15) + `close`
  (0.15) + `rename` (about 0.6, two paths). That is 1.3 µs, and it is the least that publishes
  a file whole.

Bound for the edit: 0.95 + 1.3 + about 0.1 = **2.4 µs**, budget **2.6 µs**. Bound for the
1,000-function patch: 31 + 3 + 4 + 8 + 1.3 = **47 µs**, budget **52 µs**. On AArch64 the
bound adds the cache maintenance the core asks for (`CTR_EL0`): nothing but `dsb ishst` and
`isb` when IDC and DIC are both set, or else one `dc cvau` and one `ic ivau` per 64-byte line
written, about 3,200 lines of each for 200 KB.

Where it was:
- The message took 5 system calls: a probe `read` after the sized one.
- Each patch stored into the standby table all the cells of every generation since that table
  last held the current one, about `4C` stores with a pool of 4 tables, all before the switch.
- The first three patches each copied a whole table (`S` × 8 B, 800 KB, about 80 µs) before
  their switch.
- On macOS every data span cost two `dladdr` calls, which scan the image's symbol table
  linearly: about 100 µs each with 100,000 symbols, so roughly 200 ms for a 1,000-span patch.
  On Windows every span cost a `VirtualQuery`.
- On AArch64 the per-line `dc` and `ic` loops ran even when `CTR_EL0` said they weren't needed.
- Read-only pages a patch writes (new FDEs in `.eh_frame`, literals in `.rodata`, RELRO and
  GOT words): one `mprotect` pair per run of pages, with a shootdown when write is taken away:
  1 µs + 2 µs × `T`, about 17 µs a run at `T` = 8. An edit touches about two runs, so about
  34 µs, 14 times the bound. On macOS each run of code or read-only data is a page swap
  (`mach_vm_allocate`, a copy of whole 16 KiB pages, `mach_vm_protect`, `mach_vm_remap`,
  `mach_vm_deallocate`, `sys_icache_invalidate`). On Windows each run is a `VirtualProtect`
  pair and a `FlushInstructionCache`.

What changed (speed-link-edge-sol-live, 2026-10-29):
- `slots.rs` reads the message with one sized `read` (4 system calls) into a buffer and a file
  name kept across patches, through `patch::message_file_into`.
- `switch.rs`: after each switch, `Switch::ready` brings a table to the current generation, or
  makes one while the pool isn't full (one at start). The next patch then stores only its own
  `C` cells before its switch, and the catch-up and the table copies run after it. Tested by
  `a_patch_stores_only_its_own_cells_before_the_switch`. After a rollback, the next patch still
  catches its table up before the switch.
- `unix.rs` `Segments`: the image's loaded segments are found once (`dl_iterate_phdr` on Linux,
  the Mach-O load commands on macOS), and each span is checked by binary search.
  `windows.rs` reuses one `VirtualQuery` per region.
- `linux.rs` `flush` honors `CTR_EL0` IDC and DIC (Linux hides DIC on Neoverse N1, erratum
  1542419).

Where it is (corrected by speed-sol2-link-live, 2026-10-30; it said the edit paid about 34 µs of
read-only runs, which no longer holds): request 19 is applied for ELF. An ELF patch's FDEs for
blaze functions go in `Class::Patch` after its code, ending in their own zero length, and are
written through the code region's alias, while new literals and RELRO pieces go in
`PatchData`, which is writable. So the one-function edit on Linux x86-64 writes no read-only
page and is at the bound, 2.4 µs. What can still write read-only pages, a changed word of an
existing RELRO or GOT piece or a native object's FDE in `.eh_frame`, now goes through a window
mapped twice, made once. That costs 0 system calls from then on, as "Read-only pages a patch
writes" below says. macOS and Windows are in that section too.

State of the art: Live++ and MSVC `/hotpatch` write a jump over the old entry and suspend every
thread to check its instruction pointer (v1's design). JITs on Apple Silicon (V8,
JavaScriptCore) switch W^X per thread with `pthread_jit_write_protect_np`, which needs
`MAP_JIT`, and `MAP_JIT` can't be mapped at a fixed address. Linux's own JITs write through a
second mapping, as here. Dioxus `subsecond` swaps a jump table too, but it reads the patch as a
whole shared library through `dlopen`.

### A call through a slot

The x86-64 stub is `mov r11, [rip+cell]; jmp [r11+disp32]`: two dependent loads and an
indirect jump. The bound for a table that switches all its slots in one store is one load more
than a PLT stub (`jmp [rip+slot]`): the cell, one line shared by every stub and so always in L1.
When the branch predictor knows the target, the loads only check its guess, so the bound is
**0 extra cycles** over a direct call. When it guesses wrong, the loads add 2 × 4 to 5 cycles
of L1 latency to the 15 to 20 cycles of the misprediction. Budget: no more than 0.5 cycles a
call over a direct call, measured in a predicted loop (0 × 1.1 is below what a loop can
resolve). x86-64 is at the bound. AArch64 takes 6 instructions (`adrp`, `ldr`, `movz`, `movk`,
`ldr`, `br`); request 10.3 of `link.md` asks for 4 (`adrp`, `ldr` of the cell, `ldr` of the
slot at a scaled offset, `br`), and that is the AArch64 bound, still 2 loads.

### v1's stop-the-world: `apply::write_stopped`

Work on Linux x86-64, per patch:
- **Per other thread**: a real-time signal by `tgkill`, the signal delivery, a compare-and-swap
  on the shared count, a futex wait and wake, `cpuid` to serialize (a VM exit under a
  hypervisor, about 1 to 2 µs) and `sigreturn`. That is about 3 to 10 µs of the stopping
  thread's time per thread.
- **Per patch**: `/proc/self/task` listed at least twice and `/proc/self/maps` read once (10 to
  50 µs for a large program), and an `mprotect` pair with a shootdown per run.
- **Retries**: up to 20 rounds, 500 µs apart, while any thread is inside the bytes a jump
  covers.

The least that makes a write over arbitrary prologues safe still interrupts every thread to
read its pc: one signal round trip per thread, sent to all at once, about 0.2 µs × `T` plus the
slowest handler. The state of the art, Linux's `text_poke_bp` (an `int3`, then three broadcasts
of a core-serializing IPI), costs tens of µs whatever the thread count, but only works where
the patch site is one instruction. The slots design needs neither: **0 per thread**, the cell's
store. So the fix is moving v1's users onto slots, not a faster stop. `slots::Live` now has
what they need (the macOS sampler, the directory `tiers()` reads, and `apply_as_they_come`), and
[request 20 of `link.md`](../../../../../v2/requests/link.md) asks for the daemon's side and the
users' switch, after which v1 is removed. Request 15's blind spots go with it.

### Rollback, and the answer written once

Going back from generation `g` to a kept `g'`: when a table of the pool holds `g'` (any of the
last three, with 4 tables), **0 slot stores and 1 release store** of the cell. Otherwise one
relaxed store per cell that differs between the two, undoing down to their common ancestor and
redoing up, then the cell. Then the answer: the same 4 system calls, one file, written once and
renamed into place. That is the bound and where the code is. Budget: 1.1 × (cells differing ×
store + 1.3 µs).

### The log of generations: reserved columns (speed-link-edge-mem-live)

The switch keeps every generation the program ran (`switch.rs` `Log`) and every lasting range
(`Lasting`) in columns. They were `Vec`s with room for 1,024 generations, 16,384 cells, 4,096
writes of code and 4,096 lasting ranges. Past that, a column doubled: one allocation, plus a
copy of the whole column, on the latency path of the patch that crossed the line. The
allocation-budget test only held below those counts. blaze-mem's `RegionVec` can't be used
(`v2/requests/mem-backend-live.md`, "refused by requests"): blaze-live is published and built
alone into user programs, and blaze-mem reserved about 264 GB, which broke `Live::open` under
`ulimit -v` and valgrind.

Now each column is a `slots/column.rs` `Column<T>`:
- At `Live::open` it reserves address space for its most rows (`mmap` with `PROT_NONE` and
  `MAP_NORESERVE`, or `VirtualAlloc` with `MEM_RESERVE`).
- It commits 256 KiB at a time as it grows (`mprotect` to read and write, or `MEM_COMMIT`), so
  rows never move and growing never calls the allocator.
- A full column refuses the patch before anything is written, with an error naming the column
  and its limit. Nothing falls back to the heap.

The `Range` columns became pairs of `u64` columns, and `kept` became one byte per generation.
Rows are 25 B per generation, 20 B per cell, 16 B per write of code and 25 B per lasting range.
The limits are `switch.rs` `Limits::LONG`; each column is rounded up to the 256 KiB step:

| column | rows | bytes per row | reserved |
|---|---|---|---|
| generation numbers | 2^20 | 8 | 8 MiB |
| parents, depths | 2^20 each | 4 | 4 MiB each |
| kept marks | 2^20 | 1 | 1 MiB |
| cell runs, code runs | 2^20 + 1 each | 4 | 4.25 MiB each |
| slots | 2^24 | 4 | 64 MiB |
| new targets, old targets | 2^24 each | 8 | 128 MiB each |
| code starts, code ends | 2^22 each | 8 | 32 MiB each |
| lasting starts, ends, generations | 2^20 each | 8 | 8 MiB each |
| lasting kinds | 2^20 | 1 | 1 MiB |

That is 25.5 MiB for the per-generation columns, 320 MiB for cells, 64 MiB for code and
25 MiB for lasting ranges: **434.5 MiB of address space in 15 mappings**. None of it is
memory until written. `Live::open` therefore needs `ulimit -v` (which counts reservations, in
KiB) to leave 444,928 KiB above the program's own mappings: a 1 GiB limit fits a program
whose own mappings are under about 590 MiB, and 4 GiB or unlimited always fits. Windows charges
only committed pages against the commit limit, and macOS doesn't enforce `RLIMIT_AS`.

How long the limits last:
- A one-function edit takes 1 generation, 1 cell, 1 write of code and up to 3 lasting ranges
  (the code, its FDE and the zero length after the FDE's run).
- So the lasting ranges fill first, after about 350,000 edits: an edit every 10 seconds for
  40 days.
- A 1,000-function patch takes about 1,000 cells, 1,000 writes and 3,000 lasting ranges, so
  about 350 of them fit.
- A full log refuses the patch, naming the column, and the program needs a restart. No program
  has come near this.

Commits are rare:
- A one-function edit writes 136 B across the 15 columns (25 B per generation, 20 for its cell,
  16 for its code and 75 for 3 lasting ranges). Each column commits once per 256 KiB it
  fills, so that is **one `mprotect` per 1,928 edits**.
- A 1,000-function patch writes about 111 KB: 0.42 `mprotect` per patch. Each call adds
  access to untouched pages, so no TLB shootdown, about 1 µs.

The first write to a committed page faults (a minor fault, about 0.25 µs with zeroing). That
is once per 30 one-function edits, but about 27 times for a 1,000-function patch: about 7 µs,
15% of its 47 µs bound. So after each switch, off the latency path, `Column::ahead` commits
and writes once each page that a next patch of the same size would push to. The next patch's
pushes then find their pages committed and resident, and its log work costs only the stores:
111 KB at about 50 GB/s is 2 µs, inside the bound's 10%. The commits move behind the switch
too.

Proof:
- `a_warm_patch_allocates_nothing` takes 9,000 warm patches of 4 cells under a budget of 0
  allocations. That is past the old 1,024 generations and 16,384 cells, and across a commit
  step of every cell column.
- `a_full_column_refuses_the_patch` sets small limits and checks that a full generation,
  cell, code or lasting column refuses the patch whole and names itself.
- `column.rs`'s model test checks `Column` against a `Vec` over seeded pushes, splices and
  truncations, across commit steps and up to the limit.
- `a_column_costs_memory_only_where_written` runs per platform: `mincore` on Linux and macOS,
  `VirtualQuery` and `QueryWorkingSetEx` on Windows. It checks that a reserved or committed
  column has no resident page until a row is written.

State of the art: wasmtime and V8 reserve each linear memory or heap up front, with guard
regions in GiB, and commit as it grows. This is the same technique sized for a program that
links it: hundreds of MiB, not GiB.

### The sampler's counters: `sample.rs`

The sampler reads no counter per sample. When it writes the profile (every 250 ms) it makes
`used` relaxed loads of `blaze_live_counters`, at most 65,536 × 8 B = 512 KiB. At 25 GB/s that
is 21 µs per 250 ms, 0.008% of a core, and it is the bound: each counter read once.

The program's side costs more. Level-2 code does a plain load, add and store per counter, and
the functions' counters are packed eight to a line, so two hot functions on two cores move one
line back and forth (about 50 to 100 ns each time, against about one cycle). The bound is about
one cycle an add: each function's counters on their own 128 bytes, and per-thread blocks when
one function is hot on many threads. That is codegen's (`hir-ty-codegen/tier.rs` `count`):
filed in `v2/requests/codegen.md`.

### Read-only pages a patch writes, per platform (speed-sol2-link-live)

The work is a run of pages the image maps read-only, or read and run, that a patch writes `B`
bytes into: a changed word of an existing RELRO or GOT piece, an FDE in `.eh_frame` or
`__eh_frame`, Windows `UNWIND_INFO`, or the table cell when its page is read-only. The bound
is `B` bytes of stores and 0 system calls, which a second, writable mapping of the pages gives,
as the code region already has on Linux. A TLB shootdown (2 µs × `T`) is the cost to avoid: it
comes whenever write access is taken away from pages other cores may have cached.

**Linux** (`slots/linux.rs` `Aliases`):
- Before: one `mprotect` pair per run per patch, 1 µs + 2 µs × `T`, about 17 µs at `T` = 8. A
  read-only cell page cost the same at every publish.
- Now: the first write into a read-only mapping moves a window of it onto a memory file
  mapped twice, with the protection it had (`map_twice`, as for the code region). The window
  is the 2 MiB-aligned block around the write, cut to the mapping and to the windows made
  before. Making it is `memfd_create`, `ftruncate`, two `mmap`s, a copy of at most 2 MiB
  (0.2 ms at 10 GB/s), an `mremap` over the window (one shootdown) and `close`. That happens
  once per window for the life of the program. Every write after it is atomic stores through
  the second mapping: **0 system calls, the bound**.
- A read-only cell page gets its window at start, so a publish is always one release store.
- What it costs: a window's file-backed pages become memory-file pages, up to 2 MiB each.
  RELRO pages are private copies already, so they cost nothing more.
- With request 19, a one-function edit writes no read-only page at all (frames go in the code
  region, literals in `PatchData`). Queue 00903 moves native objects' FDEs out of
  `.eh_frame` too, so no window of it is ever made.

**macOS** (`slots/macos.rs` `swap`):
- One page swap per merged run: `mach_vm_allocate`, `mach_vm_protect`, `mach_vm_remap` and
  `mach_vm_deallocate` (4 traps, about 1 µs each), plus a copy of whole 16 KiB pages (about
  0.3 µs each).
- The remap's overwrite costs a shootdown (2 µs × `T`), and `sys_icache_invalidate` costs
  about 1 µs a page (256 lines each of `dc` and `ic`).
- That is about 8 to 10 µs a run at `T` = 8. An edit has one run of code, plus one of
  `__eh_frame` while Mach-O FDEs stay there. Queue 00903 puts them after the code, which
  makes one run.
- Linux's bound is not open here:
  - Apple Silicon's kernel keeps executable pages outside `MAP_JIT` from being writable
    through a second mapping (.NET 7's W^X found double mapping unusable there and uses
    `MAP_JIT` with `pthread_jit_write_protect_np`, as V8 and JavaScriptCore do).
  - `MAP_JIT` can't be mapped at the image's fixed patch region.
- So for a fixed region the swap is the least there is, and the edit is about 4 × the 2.4 µs
  bound. What removes it is a `MAP_JIT` region the runtime maps where the kernel puts it and
  names to the linker, which then lays patches out there (queue 00907). A write is then a
  per-thread toggle, a copy and the cache maintenance, with no trap. It needs hardware to
  check before it is built.

**Windows** (`slots/windows.rs`):
- A `VirtualProtect` pair per run of pages of one protection: 2 calls (about 0.5 µs each) and
  a shootdown when write is taken away.
- `FlushInstructionCache` per run of code written: now skipped for patch code. x86-64 keeps
  instruction fetch coherent, and the bytes were never run, as on Linux.
- No second mapping of part of a loaded image exists: a `SEC_IMAGE` view can't be unmapped
  in part, and placeholders only replace placeholders.
- Queue 00904 links the patch code section read, write and execute (the trade Live++ makes)
  and puts a patch's `UNWIND_INFO` in it. `Os::write` already writes writable pages in place,
  so code then costs **0 system calls**. Until then it is 1 µs + 2 µs × `T` a run.

Proof:
- `read_only_pages_are_written_through_a_window_and_stay_read_only` checks, on Linux:
  - the bytes the program reads after writes into a read-only mapping;
  - one window per 2 MiB block, with no new window for a later write;
  - a release store through a window, seen at the original address;
  - the mapping still read-only in `/proc/self/maps`.
- `a_window_is_aligned_and_within_its_mapping_and_the_room_left` checks the window's bounds.
- `proof` should count the system calls of a warm patch that writes RELRO words (0 after the
  first) and time the macOS swap per run.

### Where a patch may write: the image's mappings (speed-sol2-link-live)

Before any write, each of a patch's `D` data spans is checked to be in the image and readable
(`Switch::apply`, through `Memory::in_image_all`), and each write finds its pages' protection.
The spans are sorted by then. The bound is a forward walk of the image's mappings, read once at
start, in step with the spans: `D + M` steps of a few cycles and 0 system calls. For the
1,000-function patch that is about 1,000 × 2 ns = 2 µs.

Where it was:
- On macOS, one `mach_vm_region` trap per span for the check and one per region change while
  writing. On Windows, one `VirtualQuery` per span for the check, one per region change while
  writing, and two per patch at publish (the current word and the cell). At about 0.3 to 1 µs
  each, a 1,000-function patch with about 1,000 data spans paid 0.3 to 1 ms, 6 to 20 × its
  47 µs bound.
- On Linux there were no system calls, but four binary searches a span (segments and
  `/proc/self/maps`, about 30 ns), about 30 µs, 60% of the bound.

Now:
- macOS (`Regions`) and Windows (`Regions`) read the image's regions and protections once, at
  start. Every swap and every `VirtualProtect` pair gives pages back the protection they had,
  so the table stays true. Lookups are binary searches: 0 system calls.
- Linux checks and writes with cursors that only move forward (`Segments::check_from`,
  `Maps::each_from`, `Maps::mapping_from`): `D + M` steps.
- The lasting-range check per write (`Lasting::hit`) looks at the last end first. The
  linker places each patch after the last one's code, so new code and the data after it start
  past every lasting range, and one hot load settles them. A write below takes the search; a
  rollback that sends the linker's cursor back (queue 00007 item a) does that until it is
  fixed. Before, it was a binary search through a column of up
  to 8 MiB: about 20 cold misses, around 1.6 µs the first time in a patch, 66% of the edit's
  bound late in a long session.
- What is left: the switch sorts the code spans, data spans and FDEs. That is linear on
  sorted input, but the linker sends them in piece order (queue 00906: sort them by address in
  `live.rs`, about 20 µs of a 1,000-function patch otherwise).

Proof:
- `a_forward_walk_of_the_maps_is_a_search_for_each` and
  `a_forward_check_of_the_segments_is_a_search_for_each` check the cursors against the
  searches.
- `a_lasting_hit_is_what_a_scan_finds` is a seeded model test of the fast path and the
  search.
- `regions_read_at_start_say_what_the_kernel_says` (macOS and Windows) checks the table
  against the kernel.
- `proof` should count system calls per span (0) and time the check per span of the
  1,000-function patch.

### The message, the answer and the poll: `patch.rs`, `slots.rs` (speed-sol2-link-live)

- `Message::view` checks a message in one pass over its headers and borrows every write from
  the buffer, so nothing is copied or allocated. That is the bound: each header read once,
  about 300 KB ÷ 110 GB/s = 3 µs for the 1,000-function patch.
- The read and the answer are the 4 + 4 system calls of the apply section's bound.
- `Live::poll` while nothing came is one acquire load of the event count. The watcher blocks
  in `poll`, `kevent` or `WaitForSingleObject` and wakes only for a message file.
- The message file's name is formatted into a kept `String`, once per event, not per poll.
- The bound is at the code. `proof` should time an empty `poll` (one load) and a message read
  per byte.

### The execution log: `slots/exec.rs` (speed-sol2-link-live)

- `__blaze_exec_arm`, in a forked child before its test, carries the prelude's marks into the
  test's region (one load per 8 slots, an `or` where any is set), then puts every slot's log
  stub back (one relaxed store per slot).
- With `S` slots that is `S` ÷ 8 loads and `S` stores of 8 bytes: 100,000 slots is 800 KB of
  stores, about 16 µs at 50 GB/s, the bound for re-arming every slot.
- The stores go to the log table only, which the forked child has to itself (copy-on-write),
  so they cost one minor fault per 4 KiB page touched: about 200 faults, 50 µs, once per test.
- A finer bound would re-arm only the slots the prelude called, read from the map's marks
  instead of storing all `S`. That trades a scan of `S` bytes for `S` ÷ 8 stores, a gain only
  when few slots were called. Left as is; at a test's fork it is not on an edit's path.
- `publish`, lazy compilation's, is 1 + `POOL` sequentially consistent stores, the bound for
  writing one slot into every table that may become current.
- Queue 00546 (per-process execution hooks for the fork server) is answered by this log, which
  the fork server already calls.

### A panic under `guard`: `slots/hook.rs` (speed-sol2-link-live)

- No cost until a guarded thread panics. Then the hook walks up to 256 frames with
  `_Unwind_Backtrace` (or `RtlCaptureStackBackTrace`), formats the reason once, and
  `Switch::tie` checks each frame's pc against each kept generation's code writes.
- That is cold, once per panic, and not held to a budget.

### v1: `apply.rs`, `format.rs`, `linux.rs`, `macos.rs`, `windows.rs` and the stops (speed-sol2-link-live)

v1 writes a jump over each replaced function's entry with every other thread stopped (the
section on `apply::write_stopped` above counts it). It is removed once its users move onto
`slots` (request 20, and the section in `v2/requests/requests.md` it points to), so it gets no
work beyond correctness here. Its tiering formats in `format.rs` (`Sample`, `Profile`,
`Tiers`) are written a few times a second and read by the daemon: not on an edit's path.
