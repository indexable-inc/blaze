---
status: plan
summary: The Mach-O `.tbd` and dylib readers brought to the speed bar on 2026-10-29 — a SWAR lexer onto a flat tape of spans, lists filtered before decoding, names hashed once, the export trie walked with one name buffer — each compared with lld's TextAPI, mold and simdjson, and what the contract still costs.
date: 2026-10-29
---
# speed: Mach-O `.tbd` stubs and dylibs

Agent `speed-link-edge-macho-dylib` (child of `speed-link-edge-macho`), for lane
[link](../link.md). The code is `crates/blaze-link/src/macho/tbd.rs` and
`crates/blaze-link/src/macho/dylib.rs`; `macho.rs` calls them through
`tbd::read(name, text, arch, platform)` and `dylib::read(name, bytes, arch)`, unchanged.

Every Mach-O link reads the SDK's `usr/lib/libSystem.B.tbd`. On this machine's SDK
(CommandLineTools, macOS 26) it is 348 KB: 3,921 lines, 40 documents (libSystem and the
`libsystem_*` it re-exports), about 11,600 tokens, 205 flow lists that wrap over nearly every
line, 412 lines with a single-quoted name (`'$ld$previous$...'`), and no double-quoted ones. A
link reads it once per arch and keeps the names of the documents its target selects. Dylibs on
the link line are read through their export trie, or their symbol table when they have none.

## `.tbd`: the lexer and parser

**Benchmarks.**
- lld's Mach-O port reads `.tbd` with `llvm::MachO::TextAPIReader`. That reader runs LLVM's
  YAMLIO: a full YAML scanner and a tree of nodes, mapped into an `InterfaceFile` with a
  `std::string` per symbol and a `StringSet` to dedup them.
- ld-prime reads them through Apple's libtapi, which uses the same YAMLIO.
- mold/sold (`macho/yaml.cc`) has the fastest one in a linker. It is a hand-written YAML-subset
  tokenizer: `std::string_view` scalars (zero-copy), but a `std::vector` per sequence and
  mapping node and a byte-at-a-time scan.
- simdjson's two stages are the lexing benchmark: a SIMD structural index (64 bytes a step),
  then a tape of 8-byte words with each container's end written into its first word.

**Before** (as `link-macho-dylib` wrote it):
- Six passes over the bytes, all of them decoding UTF-8 a char at a time:
  - `lines()`;
  - a quote-aware comment strip;
  - a second quote-aware scan for flow depth;
  - joining wrapped lines into a `String`;
  - copying each mapping value (`value.to_owned()`, the whole symbol list for every list key);
  - the flow parser.
- Allocations:
  - per logical line, a `String`, grown once per wrapped line;
  - per scalar, a `String` inside `Value::Scalar`, 32 bytes plus the heap block;
  - per sequence or mapping, a `Vec`;
  - per block line, a `Line` clone, plus a `String` for each sequence item's rest;
  - per document, a `String` and a `Target` for each target.
- Every document was parsed into the tree, including the ones the output's target never folds
  in. Lists were decoded as they were found.
- Each kept name cost two allocations, its `core_name` `String` and a `clone()` into an
  `FxHashSet<String>`, and a full UTF-8 check in `core_name`.

**Changed.**
- **Three byte passes, SWAR.**
  - `documents` finds newlines eight bytes a step.
  - `logical_lines` scans each line once for quotes, the comment and the flow depth together.
    Outside quotes it searches eight bytes a step for a superset of the bytes it acts on: `x | 0x20`
    folds `[`/`{`, `]`/`}` and `\n`/`*` together, which is five compares a word where the eight
    bytes would take eight. A false hit (`*`, `\x02`, `\x03`, `\x07`) is stepped over. In quotes
    it searches for the closing quote, a backslash or the newline.
  - The flow parser finds the end of a plain scalar (`,` `]` `}`, `:` in a key) the same way.
  - ASCII never goes through `char` decoding; a byte of 0x80 or more is decoded only where
    Unicode whitespace could matter.
- **A flat tape, simdjson's layout.**
  - Each value is a 12-byte `Node`: a tag, then a scalar's offset and length, or a collection's
    end index and count. A subtree is skipped in one step.
  - Scalars are spans of the text. One `scratch` string holds the only copies: quoted scalars
    with an escape or `''`, and plain or quoted scalars that wrap a line (rare in tapi's
    output).
  - A logical line is a run of `Piece`s, the trimmed bytes of each physical line, read as if
    joined by one space, so wrapped symbol lists are never joined into a buffer.
  - No allocation per node, scalar or line: the tape (preallocated to `len / 16`), the line and
    piece buffers (reused across documents) and `scratch` are the only buffers of a read.
- **Lists filtered before decoding.**
  - Each document is checked as before (its version, install name and targets), since a
    malformed document is an error even when the output's target never uses it.
  - Its lists are only selected: `selects` compares each list's targets.
  - A selected list's keys and value types are checked, and its nodes and kind are recorded in
    a `Plan`, in the order the old reader added names, with the error order unchanged.
  - Then `decode` walks the plan once. Lists of other targets, and documents nothing
    re-exports, never have a name decoded or hashed.
- **Names hashed once, counts known up front.**
  - The plan counts the names, so `decode` sizes the symbols `Vec` and the dedup table once.
  - The table, `Seen`, is open addressing of `u32` indexes into the symbols (4 bytes a slot, at
    most half full). Each name is hashed once with `FxBuildHasher` and compared in place, with
    no second copy of the key.
  - The core name is built without allocating: `_x` is borrowed as `x`, and `\x01`-prefixed and
    Objective-C names are written into one reused buffer.
  - A name allocates only when it is new: one `String`, which `SharedSymbol::name` requires.
  - The equivalence with `super::core_name` is a test.
- Install names are looked up through one `FxHashMap` built once per file; this used to be a
  linear scan of the documents for each re-exported library.

**After, per kept symbol:** about 1 byte scanned per input byte in each of three passes, one
12-byte node, one hash, about 1.5 probes, and no allocation. The old reader made at least 4
allocations, about 56 bytes of tree, and 6 passes of char decoding. Compared with mold: the
same zero-copy scalars, no `vector` per node, and a word-at-a-time scan.

**The zero-copy contract (agent `speed-link-edge-macho-dylib2`, 2026-10-29).** `model.rs` now
gives `Shared` one `Names` arena and makes `SharedSymbol` a 16-byte `Copy` value holding an
8-byte `Name`. The reader moved onto it:
- While `check` plans the selected lists, it also bounds their names' bytes: each scalar's
  length plus one (for a `\x01`), or the prefix plus the scalar for each Objective-C prefix. It
  reads only the 12-byte nodes and never touches the text.
- `decode` sizes `Vec<SharedSymbol>`, `Names` and `Seen` once from the plan.
- A new name is one `Names::push`, which copies its bytes into the arena. `Seen` compares a
  candidate against `names.at(symbol.name)` in place.
- Per kept symbol: 16 bytes of `SharedSymbol` and the name's bytes in the arena, against the
  32-byte `String` plus a heap block before. No allocation per symbol: a read allocates the
  tape, the line and piece buffers, `scratch`, the symbols column, the arena and the set, each
  once.
- `Shared::versions` is empty: Mach-O has no symbol versions.

**Where it still falls short, and what it would take.**
1. Fixed by the contract (above): there is no longer a `String` per symbol.
2. The core then hashes every library symbol again, every link: `native.rs` builds an
   `FxHashMap<&str, _>` over all of them to look up the few hundred names the link imports. The
   reader's `Seen` table is already that index. The proposal (filed with the parent) is for
   `Shared` to carry it (`index: Vec<u32>`, same probe rule), and for the core to probe each
   library in order for each wanted name: the same first-library-wins answer, with work
   proportional to the imports rather than the exports. wild and mold resolve a dylib's
   exports through a hash table built once per file in the same way. As of the contract
   change, `model.rs` has no `Shared::index`, so `Seen` is still dropped at the end of a
   read. When the index lands, `read_documents` moves `Exports::seen.slots` into it; that
   needs `Seen` to hash with the probe rule the index states.
3. SWAR is 8 bytes a step where simdjson's stage 1 does 64. Lexing is no longer where the time
   goes (allocation and hashing per symbol are, until items 1 and 2), so it stays portable. An
   SSE2/NEON path behind run-time detection, with the scalar path kept and tested against it
   as `blaze-cas` does, would roughly quadruple the scan rate if profiles ever show the scan.

## Dylibs: the export trie and the symbol table

**Benchmarks.**
- lld's `TrieParser` walks the trie recursively and builds a `Twine`, then a `std::string`, per
  terminal.
- mold's `read_trie` recurses with a `std::string` prefix plus the label for each child: one
  allocation per edge.
- ld-prime is not public; Apple's dyld `MachOFile::forEachExportedSymbol` keeps one fixed
  buffer and a stack of offsets, and is the benchmark matched here.

**Before.** For each edge, a `Vec<u8>` clone of the prefix plus the label, and each node's
children collected into a `Vec` of their own. The nodes seen were an `FxHashSet<usize>` (a hash
per node). Classifying an export scanned every section.

**Changed.**
- One name buffer, cut back to an edge's parent length before its label is appended.
- One stack of 16-byte `Edge`s (label offset and length, child, parent length) for the whole
  walk. A node's edges are pushed and reversed in place, so they are all read before its first
  child is entered and the walk keeps its order and its error order.
- A bitset of one bit per trie byte for the nodes seen.
- Sections are sorted by address once, so an address is classified by binary search. When the
  non-empty sections overlap (no linker writes that) it scans them in load-command order, the
  old answer.
- The symbol-table path counts its exports before allocating the `Vec`.
- Per export (since the zero-copy contract): one `push_core_name` into the file's `Names`
  arena, with its UTF-8 check. The arena starts at twice the trie's bytes, or at the string
  table's size on the symbol-table path, which is an upper bound there. A name without a
  leading `_` still gets a short-lived `String` from `push_core_name` (in `macho.rs`, for its
  `\x01`). No linker writes such a name in an export trie.

**After.** Allocations per trie edge: none, where lld allocates once per terminal and mold once
per edge. Allocations per export: none beyond the arena's doubling, which happens at most a
few times per file (before the contract, one `String` of 24 bytes plus a heap block per
export). Bytes kept per export: 16 for the `SharedSymbol` plus the name. Bytes kept per
pending edge: 16. Lookups per node: one bit test.

## Tests

- `tbd.rs` keeps every test the old reader had, the parse-tree one included. The old reader is
  kept whole in `tests::reference` as the model, and the new reader is checked against it:
  - **The parse trees and full results agree** (install name, symbols in order, inputs, or the
    exact error string) for six targets over 600 seeded `.tbd` files:
    - versions 1 to 4, one to three documents;
    - re-exports in the file and out of it;
    - plain, single- and double-quoted names with `''`, `\x61` and `\u00e9` escapes;
    - flow lists on one line or wrapped with trailing comments, and block lists, nested or not;
    - `%` directives, CRLF line endings, missing `...`;
    - a third of the files with inserted or deleted bytes, to reach the error paths.
  - **Hand-written edge cases agree**: scalars that wrap a line, quoted scalars that wrap one,
    escape errors, flow maps, duplicate keys, tabs, keys that wrap.
  - **The real SDK `libSystem.B.tbd` agrees**, when the SDK is present.
- The SWAR searches agree with a byte loop from every offset of seeded noise. `candidate` is
  checked to cover every byte the line scan acts on.
- Core names agree with `super::core_name`.
- `Seen` grows correctly and rejects repeated names.
- `dylib.rs` keeps its tests and adds two more:
  - The old trie walk, kept in the tests as the model, gives the same result (or the same
    error) as the new one over 400 seeded tries: random shapes, labels and export kinds, one in
    three with a byte changed and one in three cut short.
  - The section lookup gives the same answer as a linear scan, over seeded overlapping and
    disjoint sections.
- Since the zero-copy contract, results are compared in an owned form (`tests::Owned`: the name
  as a `String`, through `Shared::symbol_name`). The reference models still produce that form.
  Every read in the differential tests also checks that the arena holds exactly its symbols'
  names, back to back, and that `versions` is empty. `tbd::tests::arena_sized_once` checks, over
  seeded files, that the plan's byte bound holds, so the arena never grows. These readers make
  no `Object`, so they have no `Data::Input` range to check.
