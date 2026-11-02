---
status: plan
summary: Index of speed-link-edge's work on blaze-link's ELF and Mach-O writers and blaze-live, with where each part's before/after against mold, wild, ld-prime and lld is.
date: 2026-10-29
---
# speed-link: the linker's formats and the live runtime

Part of [link](../link.md). The core of `blaze-link` (model, placement, relocation, commit) is
lead `link`'s and not covered here; what the formats still need from it is in
`v2/requests/link.md` items 5 to 11.

| part | code | notes |
|---|---|---|
| ELF writer and readers | `crates/blaze-link/src/elf.rs`, `elf/` | [speed-link-edge-elf](speed-link-edge-elf.md) |
| Mach-O writer, signature, export trie | `crates/blaze-link/src/macho.rs`, `macho/{linkedit,sign,sha256,tests}.rs` | [speed-link-edge-macho](speed-link-edge-macho.md) |
| Mach-O objects and archives | `macho/input.rs`, `macho/archive.rs` | [speed-link-edge-macho-input](speed-link-edge-macho-input.md) |
| `.tbd` stubs and dylibs | `macho/tbd.rs`, `macho/dylib.rs` | [speed-link-edge-macho-dylib](speed-link-edge-macho-dylib.md) |
| compact unwind | `macho/unwind.rs` | [speed-link-edge-macho-unwind](speed-link-edge-macho-unwind.md) |
| live patch apply | `crates/blaze-live/` | [speed-link-edge-live](speed-link-edge-live.md) |

## Per phase, in one line each

- **Parse**: inputs are read in place at any alignment (`object`'s `unaligned` feature). Mach-O
  atoms are `Data::Input` ranges with names in one arena per object, sized once. `.tbd` files go
  through a byte lexer onto a flat tape, the way simdjson does. The ELF readers still copy section
  bytes and names until the model's zero-copy types reach ELF (`v2/requests/link.md` item 5).
- **Resolve** (what the readers feed): archive members are found by binary search over their
  offsets, and ELF members without an index are read in parallel. A name index on `Shared`
  (item 7) would make the work proportional to imports rather than exports. It is open.
- **Layout**: synthetic sizes come from counts, which is already wild's shape.
- **Copy and relocate**: these are the core's (lead `link`).
- **Synthesize**: each table is built once. ELF's string tables, `.gnu.hash` and `.rela.dyn` are
  built in parallel, as mold builds them. The Mach-O export trie is one pass with CSR nodes.
  Compact unwind uses compressed pages, as lld does. The code signature hashes pages with
  SHA-NI/ARMv8 in parallel and re-hashes only changed pages on an incremental link.
- **Live patch**: messages are decoded in place. Generations are kept as columns in a pool of
  tables. A slot write is a relaxed store, then one release store per cell. The kernel cost per
  patch is one `mprotect` pair per run of pages, or none for code dual-mapped on Linux.

What still falls short of the benchmarks, and why, is in each part's notes under "Not done"
or "What would beat it". Chained fixups (`v2/requests/link.md` item 6) and incremental ELF
synthesis are the biggest of those.
