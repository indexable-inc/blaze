---
status: plan
summary: The Mach-O part of blaze-link against ld-prime, lld's Mach-O port, mold/sold and wild, phase by phase, before and after the speed pass.
date: 2026-10-29
---
# speed-link-edge-macho: the Mach-O writer and readers

Paths: `crates/blaze-link/src/macho.rs` and `crates/blaze-link/src/macho/`. The readers have
their own notes, written by this lane's children:
[speed-link-edge-macho-input.md](speed-link-edge-macho-input.md) (objects, archives),
[speed-link-edge-macho-dylib.md](speed-link-edge-macho-dylib.md) (`.tbd`, dylibs) and
[speed-link-edge-macho-unwind.md](speed-link-edge-macho-unwind.md) (`__unwind_info`). This file
covers the writer (`macho.rs`, `linkedit.rs`, `sign.rs`, `sha256.rs`) and sums up all of them.

## The benchmarks

- **ld-prime** (Apple's 2023 rewrite of ld64): parallel parsing, parallel hashing of the
  signature, chained fixups by default for deployment targets of macOS 12 and later.
- **lld's Mach-O port**: `ExportSection` builds the trie once and settles offsets in passes,
  `UnwindInfoSection` builds compact unwind from sorted function starts, `CodeSignatureSection`
  hashes pages in parallel (`parallelFor` over 4 KiB pages) straight from the output buffer.
- **mold/sold**: everything parallel per input and per chunk, synthesized sections written in
  place into the mapped output.
- **wild**: zero-copy input reading and a two-phase layout built for incremental relinks.

## Phase by phase

| phase | before | after | against the benchmark |
|---|---|---|---|
| parse: objects | atoms found by two `Vec<u64>` per section and a linear scan per symbol for its size (quadratic per section); a `Vec` of relocations per section; `__eh_frame` pointers resolved by linear scans | one sorted column of atom bounds and starts per file, sizes and address→atom by binary search; nlists and relocations read in place; `__eh_frame` by binary search. On the zero-copy contract: an atom's bytes a range of the input (`Data::Input`), every name a `Name` in one `Names` arena sized once from the nlists, each Mach-O section's `SEG,SECT` pushed once and shared by its atoms, fixups one column (`Object::fixups`) with each atom's run a `Range<u32>`: no allocation per atom, per section or per symbol ([input notes](speed-link-edge-macho-input.md)). Left: a `String` per *named* Mach-O section (`__LD,__compact_unwind`, `#[link_section]`), `Class::Named`'s (`v2/requests/link.md` item 11) | as lld's `ObjFile` on passes and lookups, and as mold and wild on copies (none) |
| parse: archives | each index entry re-parsed its member's header; a `String` per member and per index name | members by binary search over header offsets; the index and members' names in one `Names` arena sized by one pass over the index | as lld |
| parse: where an input starts | a member not at a multiple of 8 (GNU-format archives, `llvm-ar`'s) was copied whole into an aligned buffer before it was read (`aligned`, one `unsafe` block) | read in place at any byte: the workspace builds `object` with `unaligned`, so its Mach-O structures are byte arrays, which arm64 and x86-64 load unaligned at no cost; `macho.rs` asserts at compile time that every structure the readers view has alignment 1. A universal file's object is read in its slice and its `Data::Input` ranges moved onto the whole file, so the core reads them in place too | as wild and mold: no copy of any input |
| parse: `.tbd`, dylibs | a parse tree of `String`s and `Vec`s per YAML node; at least 4 allocations and 6 char-decoding passes per kept symbol; a `String` per export of a dylib | `.tbd`: a byte lexer (8 bytes a step) onto a flat 12-byte tape, three passes, one hash and about 1.5 probes per kept name, the names straight into a `Names` arena sized once from the tape; dylibs: the export trie walked in place into an arena; no allocation per symbol in either ([dylib notes](speed-link-edge-macho-dylib.md)) | beats lld's and ld-prime's (LLVM's YAMLIO: a node tree and a `std::string` per symbol); beats mold's hand-written YAML-subset reader, the fastest in a linker, which has the same zero-copy scalars but a `vector` per node and a byte-at-a-time scan |
| resolve | the core's, fed `String` names | the core's, fed `Name`s of each file's arena and `Copy` symbols (`SharedSymbol` 16 bytes); a hash index per `Shared` (`v2/requests/link.md` item 7, which `tbd.rs`'s dedup table already is) is still open, so the core hashes every export of every library per link | as lld once item 7 lands: lookups proportional to the imports, not the exports |
| layout | `synthetic` sizes `__unwind_info` from counts only: no pass over the image | unchanged | as lld |
| copy | the core's | unchanged | the core's |
| relocate | every Mach-O relocation is read into an `Apply` the core applies; `Writer::apply` refuses the rest | unchanged | as lld: relocation types decoded once, at read |
| synthesize: signature | the whole file assembled into one new buffer (`vec![0; code_limit]` plus a copy of every region, the debug info and `__LINKEDIT`), then hashed in parallel per page; SHA-256 by the ARMv8 instructions or portable code | pages hashed where they are (`sign::View`: a page inside one region is borrowed, one across regions gathered into a 4 KiB stack buffer), 64 pages per parallel task; SHA-NI on x86-64 beside the ARMv8 path; an incremental image keeps the last signature's pages (`sign::Cache`) and hashes only the pages whose bytes differ (compared exactly, about 10 times faster than hashing them); the signature is handed back as one file part per 4 KiB, so the core rewrites only the parts that changed | matches lld and ld-prime on a full link (no assembled copy, parallel, hardware SHA); beats both on a relink, which re-sign every page |
| synthesize: export trie | a `Vec<u8>` per edge label, edges split by copying as each name was inserted, a `Vec` of edges per node; every name built as a `String` (`macho_name`) and copied into a `Vec<u8>` | built in one pass from the sorted names (`linkedit::trie`): a node's names are a range, its edges the runs of one next byte, each label a range of one name; 16-byte nodes and edges in two columns (CSR), nothing allocated per node or edge; names borrowed from the string table; byte for byte the same trie (a differential test against the old builder) | as lld and ld64: same structure, offsets settled in passes until no ULEB grows (usually two) |
| synthesize: binds, rebases | a `String` and a `Vec<u8>` per bind for its name; the rebase and function-start addresses copied and sorted every link | bind names borrowed from the string table (`Bind<'a>`); addresses used in place when already sorted, as the core lists them | as ld64's opcode writer; chained fixups not yet, see below |
| synthesize: symbol table | a `String` per local, defined and undefined symbol, sorted as `String`s, then copied into the string table; the indirect table found each `__got` cell with a linear search of every GOT cell (quadratic) | each name written once, straight into the string table, from the core's name (`spelled`: the underscore and the rest), sorted by a comparison of the Mach-O spellings without building them (`macho_order`); the string and nlist buffers sized up front; the indirect table a merge walk of the sorted GOT | as lld's `SymtabSection` |
| synthesize: `__unwind_info` | gimli's full parse of every CIE and FDE into a `BTreeMap`; every start in one `Vec` sorted whole; common encodings by a `BTreeMap`; regular second-level pages only (8 bytes an entry) | one linear scan of `__eh_frame` reading only each CIE's pointer encoding and each FDE's range; a one-pass merge of four already-sorted columns; an open-addressed encoding table; compressed pages split as lld splits them (4 bytes an entry); one buffer of the exact size, nothing allocated per function ([unwind notes](speed-link-edge-macho-unwind.md)) | as lld's `UnwindInfoSection` and ld64; Rust functions stay 8 bytes an entry (all DWARF mode) until codegen emits compact encodings, as with ld64 and lld |

## Not done, and what it would take

- **Chained fixups** (`LC_DYLD_CHAINED_FIXUPS`). The writer emits rebase and bind opcodes
  (`LC_DYLD_INFO_ONLY`), which is what ld64 and ld-prime emit for rustc's default deployment
  targets (macOS 11.0 on arm64, 10.12 on x86-64): chained fixups are their default only from
  macOS 12. A chain lives in the pointer slots themselves (each slot holds its target and the
  distance to the next slot of its page), so the writer must rewrite bytes the core placed,
  and an incremental relink must rewrite a slot's predecessor when the set of slots in a page
  changes. The contract has no way for a writer to do either: `v2/requests/link.md` item 6
  asks for it. With it, the chain is written as pieces are placed, page starts and the imports
  table in one pass from counts known up front, and a relink rewrites only the slots whose
  target or neighbour changed.
- **The debug info is copied once** into the `__DWARF` part: `File::parts` owns its bytes
  (`Vec<(u64, Vec<u8>)>`) and `Placed::debug` is borrowed, so one copy of every debug section
  is forced per link (the signature hashes them in place). A part that names a debug section
  by index instead of holding its bytes would remove it; mold writes them straight into the
  mapped output.
- **The symbol table and the trie are rebuilt on every relink**: `Writer::synthesize` and
  `Writer::file` are pure functions of `Placed`, which does not say what changed since the last
  link. Both are linear after one sort of the names (O(n log n) string compares, n the
  functions with names); a relink that changes no name could reuse them if `Placed` carried
  the previous link's names, which no benchmark linker does either.
