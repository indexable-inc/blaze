---
status: plan
summary: lane frontend's speed of light: for each component, the bound its work and the hardware set, the arithmetic, how the design reaches 90% of it, and what stands in the way where it does not yet.
date: 2026-10-29
---
# Lane frontend: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). Each part of lane frontend has a section here,
written by the agent that owns it; a sibling adds its own section below rather than rewriting
the file.

Figures are for one core of the target machines (EPYC Turin, Graviton 5), from
[hardware.md](../../design/hardware.md), with the same constants as
[format's](../format/speed-of-light.md):
- about 3.5 GHz;
- an L1 hit in 4 to 5 cycles, L2 in about 14;
- streaming from L2 at about 32 bytes a cycle, DRAM at 20 to 30 GB/s;
- an IPC of about 2 for branchy code and about 4 for straight-line integer code;
- a mispredicted branch costs about 15 cycles.

Nothing has been measured: nothing may compile until every lane is done. `proof`'s budgets
measure each bound once it does. Instruction counts are estimates from the code as written.

## frontend-tokens: one flat token format (frontend.md item 4)

Design: [tokens.md](tokens.md). Below, \(t\) is a tree's tokens (a subtree counts as two, its
open and close), \(n\) its nodes, and \(s\) its distinct symbols.

### 1. Parser input from token trees (`syntax_bridge::to_parser_input*`)

Work: for each token, write one 4-byte word holding its kind, contextual kind, edition and
joint bit.

- **Bytes.** Per token it reads the 8-byte node and the 8-byte packed span (for the context's
  part index), then a parts entry, an L1 hit since a tree has a few. An ident also reads its
  8-byte symbol slot and, unless it is a declared keyword, one line of its text. It writes 4
  bytes. That is about 24 bytes a token. The trees converted were just built by an expansion or
  a call's lowering, so they are in L2 (a tree of 10 000 tokens is 160 KB). At 32 bytes a cycle
  that is **0.75 cycles a token**; from DRAM, 24 B / 25 GB/s ≈ 1 ns ≈ 3.4 cycles. Bytes are not
  the bound while the tree is in L2.
- **Instructions, the minimum.** About 3 to step and dispatch on the tag, 2 for the context (a
  shift and a load), 2 to compare it with the last, 2 to 5 for the kind (a table by char, by
  declared symbol, or by literal kind), and 3 to pack and store the word: about 14. That is
  straight-line work at an IPC near 4, so about 3.5 cycles. The tag branch is data-dependent:
  token kinds alternate and mispredict on perhaps 1 token in 10, which adds about 1.5 cycles.
  **Bound: about 5 cycles, 1.4 ns a token.**
- **Where the design is.** Estimated per token:

  | Step | Instructions |
  | --- | --- |
  | walk (close check against the stack's end, zip over the two columns, tag) | 8 |
  | callback's lifetime flag | 1 |
  | edition | 3 |
  | kind: punct | 4 |
  | kind: ident (underscore compare, raw bit, declared index, two table loads) | 8 |
  | push (capacity check, store, length) | 4 |
  | **total** | **about 22** |

  At an IPC of 3, plus the mispredicts, that is about **9 cycles: about 55% of the bound**.
- **Applied now.**
  - The walk keeps each open subtree's end on its stack, so no node is reloaded per token. It
    zips the node and span columns, so there is no bounds check and no top-node test per token
    (`tt` `for_each_token`).
  - The parser's conversion is compiled without the matcher's fragment tracking
    (`convert::<FRAGMENTS>`).
  - An identifier's text is read at most once, and never for a declared keyword
    (`Keywords::of`, which now also answers lifetimes).
- **What stands in the way of 90%.** The pass exists at all. The parser indexes lexer tokens,
  and the trees differ: a lifetime is `'` plus a name, a subtree is one node for two tokens, and
  groups without delimiters are no tokens. The pass is the conversion between those models. The
  component's real bound is zero conversion: the parser reads nodes through an input trait,
  with one tag decode a lookahead, as rustc's parser reads `TokenStream`s through its cursor.
  To remove it:
  1. Change the parser's input trait so that `nth(n)` is answered from a token index over the
     nodes, built once per tree in the walk above. That index is 4 bytes a token, but with no
     kind computed, so the kind work moves to the lookahead that needs it.
  2. Change the trait's other user, `syntax/`'s lexer-token parse, which is frontend-deps'.

  Owners: frontend-tokens (`parser/`) together with frontend-deps (`syntax/`). Until then, set
  this component's cost against the whole "parse an expansion" step: the parser (estimated 60
  to 100 instructions a token) plus `TtTreeSink` (about 40). The pass is about 15% of that.

### 2. The macro matcher's input (`mbe::expect_fragment`)

Work: the tokens of each fragment window converted as in (1), once per attempt.

- **Bound:** (1)'s 5 cycles a token, times the tokens each arm's matcher looks at. The window
  makes each attempt's cost linear in what it parses, with no quadratic work.
- **Where the design is:** at (1)'s 9 cycles a token. The input is one buffer per thread that is
  refilled (`MATCHER_INPUT`), so a window costs no allocation, where it cost five column
  allocations before.
- **What stands in the way:** as in (1).

### 3. Lexed text to token trees (`syntax_bridge::parse_to_token_tree*`)

Work: for each lexed token, a node and a packed span, and the symbol interned for an ident or a
literal.

- **Bytes.** It reads the lexer's 4-byte word and the token's text (about 4 bytes for an ident)
  and writes 16 bytes of columns. The builder also writes and reads back a scratch row: an
  8-byte node, a 20-byte span and an 8-byte symbol. That is about 100 bytes a token, at L2
  bandwidth about 3 cycles. The minimum is about 25 bytes, under 1 cycle.
- **Instructions, the minimum.** About 8 to dispatch the lexer kind, and about 5 to pack and
  store. About half the tokens are idents or literals, and each must be interned: a hash of
  about \(\lceil len/8 \rceil + 10\) instructions and a probe of the shared table, about 30, so
  about 15 averaged over all tokens. Total: **about 28 instructions, about 9 cycles a token.**
  The probe's cache misses are the bound beyond that.
- **Where the design is.**
  - Reading a token's text is O(1) now: `LexedTokens` keeps a running offset, where each token
    used to cost up to 16 steps from a checkpoint.
  - The builder's two phases cost about 20 more instructions a token: it pushes scratch rows,
    then `finish` packs each span against the last part.
  - That is about 50 instructions, about 17 cycles, **about 55% of the bound**.
- **What stands in the way, and what removes it.**
  1. **The scratch phase.** A lexed tree has one part (one anchor and context for every token),
     so its spans pack as they come. A direct writer for one-part sources writes the final
     columns in one pass, holding closing spans in their own column until `len` is known. That
     reaches the minimum's 5 instructions for packing. Owner: frontend-tokens, next.
  2. **Interning.** Interning hashes each identifier's text again after the lexer has read it.
     A per-thread front cache in the interner, or the lexer carrying each identifier's hash,
     halves that term. Owner: the interner's (format), the same item format's own notes name.

### 4. The proc-macro wire (`tt::TopSubtree::write_wire`, `read_wire`)

Work: writing is copying the columns, 16 bytes a leaf and 24 a subtree, plus the symbols'
text. Reading is the same copy plus the checks untrusted bytes need, and one intern a symbol.

- **Write.**
  - Bound: a memory copy, 16 bytes a leaf at L2's 32 bytes a cycle, so 0.5 cycles a leaf (DRAM:
    16 B / 25 GB/s ≈ 0.64 ns).
  - Design, applied: one `reserve`, then the node, span and wide-span columns appended as their
    bytes (`zerocopy` `IntoBytes` on the `#[repr(C)]` columns in `tt/src/storage/columns.rs`,
    with a little-endian host asserted at compile time). There is no zero fill. The parts, the
    symbols' ends and the header are small rows. That is the copy plus a row a symbol: **about
    90%**.
- **Read.**
  - Bound: the copy (0.5 cycles a leaf), plus the least an untrusted tree needs: a tag and
    bounds check a node (about 5 instructions), a part-order check a span (about 4), and one
    intern a distinct symbol (about 30 plus the probe). That is about **3 cycles a leaf** plus
    \(s\) interns.
  - Design, applied now:
    - Each column is decoded as fixed-size rows from a slice taken whole, with no length check
      a word.
    - The canonical check is by indices alone: a span's part is a used one or the next one,
      the parts are distinct, wide spans are in order and exactly the ones that don't pack, and
      each symbol index is the builder's window's. It no longer re-encodes and compares every
      span.
    - Node checks rebuild each node through its constructor and compare it, about 8
      instructions.

    - The node and span columns are viewed where they lie in the bytes (`zerocopy`
      `FromBytes`), checked there, and copied once into the tree. There is no decode a word, and
      no allocation for bytes that fail the checks.

    That is about 13 instructions a leaf, about 3.3 cycles: **about 90%**. Input must start at a
    4-byte boundary, and input that doesn't is refused.
- **A whole proc-macro call** costs two writes and two reads, against rustc's bridge, which
  crosses one token tree at a time through RPC handles at hundreds of cycles a token. Above the
  wire, the critical path is the macro itself.

## frontend-image: the image, its CRCs and the stable-ID interner (restore.md items 1–3)

Design: [image.md](image.md). Whole-machine figures are [db's](../db/speed-of-light.md):
Turin streams about 480 GB/s across a socket and about 30 GB/s on one core. A minor fault on
a page already in the page cache is about 0.25 µs, and Linux maps 16 pages around it
(fault-around), so about 0.5 µs for 64 KiB. CRC32C in hardware (`crc32q`, `crc32cx`) has a
throughput of one 8-byte instruction a cycle, so 8 bytes a cycle is about 28 GB/s on one core.
Below, \(G\) is an image's groups, \(S\) a section's bytes, and \(n\) the symbols.

### 1. Opening an image (`Image::open`)

Work: map the file, then read and check its spine. The spine is a 128-byte header, 64 bytes a
section and 4 bytes a CRC-table group. For a 4 GiB image of 12 sections it is about 900 bytes
(\(G = 65\,536\), so 4 CRC-table groups).

- **Bound.**
  - The least the kernel takes: `open`, `fstat`, `mmap` and `close`, about 2 µs, plus one
    fault for the first page, 0.25 µs.
  - The least integrity check is one CRC32C of 900 bytes: about 110 cycles, 0.03 µs.
  - Total: **about 2.3 µs**.
- **Where the design is.**
  - It checks the spine with BLAKE3, not a CRC: about 1.2 cycles a byte for one chunk, so
    0.3 µs. That hash is the image's name (the VFS journal and `intern::restored_from` key on
    it), so it is computed anyway.
  - The checked and bad bitmaps are \(2G/8\) bytes, 16 KiB, zeroed at about 64 bytes a cycle:
    0.07 µs.
  - It reads nothing else: `open_reads_only_the_spine` counts the bytes touched.
  - Total: about 2.7 µs, **about 85%** of the bound.
- **What stands in the way.** The BLAKE3 of the spine (0.3 µs). Checking the spine by CRC32C
  instead, with the hash stored as the image's name, removes it and reaches about 98%. It is
  left for now: 0.3 µs, once a restart, is noise against the restart budget in
  [activate.md](activate.md), and the hash binds the name to the bytes.

### 2. The first read of a group (`Image::range`, `slice`, `value`, `group`)

Work: bring 64 KiB from the page cache and CRC it once. After that, a read costs a bit test.

- **Bound.**
  - The fault: one, since groups are 64 KiB-aligned and fault-around maps exactly one group.
    About 0.5 µs.
  - The bytes: 64 KiB at 30 GB/s, 2.2 µs.
  - A CRC is the least that proves the bytes, and it is computed as they stream. Carry-less
    folding (`VPCLMULQDQ`) runs faster than memory, so streaming is the bound.
  - Total: **about 2.7 µs a group**.
- **Where the design is.**
  - Three `crc32q` streams of 4 KiB, joined by a 4-lookup GF(2) shift table, at the
    instruction's throughput: 8 bytes a cycle, 2.34 µs a group. That overlaps the stream, so
    the cost is max(2.2, 2.34) + 0.5 = 2.85 µs, **about 95%**.
  - The check leaves the group in L2 (64 KiB of a 1 MB L2), so the read that asked for it
    pays nothing more to bring it in.
- **A read once checked.** One relaxed load and a bit test a group spanned, about 3
  instructions on an L1-resident bitmap (16 KiB for 4 GiB): **about 100%** of a read's own
  cost.
- **What stands in the way.** Nothing on Linux. On Apple M-series (16 KiB pages, no
  fault-around) a group is 4 faults, about 3 µs more. A `madvise(MADV_WILLNEED)` of the image
  at map time is the fix, and is core's (`Effect::Map`).

### 3. Restoring the symbol table (`intern::restore_symbols`)

Work: check the symbol section whole (every value names symbols), then validate its columns
so every later read is in bounds. A symbol is about 12 bytes of text, a 4-byte start, an
8-byte order key, a live bit and two 5-byte index slots, about 34 bytes. So \(n = 10^6\) is
\(S \approx 34\) MB.

- **Bound.**
  - Bytes: 34 MB / 480 GB/s = 71 µs, with the CRC and the column checks fused into one pass.
  - Compute: about 25 instructions a symbol, so \(25 \times 10^6\) over (IPC 3 × 64 cores) is
    37 µs. Bytes are the bound.
  - **About 71 µs.**
- **Where the design was.** One core: 34 MB at 28 GB/s is 1.2 ms of CRC, plus
  \(25 \times 10^6\) instructions at IPC 3 is 2.4 ms of validation. About 3.6 ms, **2%**.
- **Applied now.**
  - `restore_symbols(image, jobs)` takes the daemon's pool (`blaze_image::Jobs`, the shape of
    `blaze_graph::pool`'s `run_jobs`).
  - The CRCs run in 1 MiB jobs (`Image::check`), about 37 µs each. Each column's validation
    runs in jobs of 16k entries, about 40 µs each.
  - The first failing chunk is the error, so many workers report what one would.
    `a_parallel_restore_is_the_serial_one` proves that.
  - Two parallel passes: CRC 34 MB, about 75 µs at the socket's bandwidth; validation reads
    about 26 MB of it again, about 54 µs. Plus about 10 µs of handing out jobs: **about
    140 µs, about 50%**.
- **What stands in the way, and what removes it.** The second pass. Fusing them per chunk of
  IDs removes it: a job checks the groups its starts, live bits and bytes fall in, then
  validates them while they are in its L2. `Image::check` is idempotent, so overlapping ranges
  cost a bit test. That reaches about 90%. Owner: frontend-image, next.

### 4. Interning and reading a symbol (`Symbol::intern`, `as_str`)

- **Bound** (as in frontend-tokens' section 3): a hash of about \(\lceil len/8 \rceil + 10\)
  instructions and one probe, about 30.
- **Where the design is.**
  - A symbol already in the image: the hash and one probe of the mapped index (a tag match in
    one 8-byte control word, then the bytes compared). About 35 instructions, **about 85%**.
  - A process with no image skips the mapped index with one compare.
  - A symbol new since the restart: both probes, about 60 instructions, **about 50%** for
    those. They are the few names an edit adds, not the bulk.
  - `as_str` of a compiled-in symbol is a bounds check and a load. For another symbol it is a
    `OnceLock` load, the base range check, one live bit and two adjacent starts, about 15
    instructions. The bound is the two starts and the pointer: **about 80%**.
  - `Symbol` is 4 bytes, down from 8, so everything that holds names is smaller.
- **What stands in the way.**
  1. The second probe for new symbols. One probe would need the mapped index to take inserts:
     copying it on the first one is O(index), which costs more.
  2. The lexer's text is hashed again, as frontend-tokens' section 3 says. For text lexed into
     token trees (`syntax_bridge::parse_to_token_tree*`) this is gone:
     `LexedStr::new_hashed` hashes each name while it reads the bytes, and
     `Symbol::intern_hashed` takes that hash. A compiled-in name (the declared symbols and the
     most frequent names of `library/` and `harness/`) is a lookup in a perfect hash keyed by
     the same hash, with no probe of the table
     ([simd-lexer's speed of light](../simd-lexer/speed-of-light.md#hashing-names-for-the-interner-lexedstrnew_hashed)).
     Lowering from the flat syntax tree still hashes each name. Owner: frontend-image, with
     frontend-tokens' lexer.

### 5. Writing an image (`Builder::finish_on`, `intern::write_symbols`)

Work: write \(B\) bytes through the page cache to NVMe, with a CRC each group.

- **Bound.** The device: \(B\) / 7 GB/s, so 0.6 s for 4 GiB. Everything else overlaps it or
  is smaller.
- **Where the design is.**
  - CRCs were one core: 4 GiB / 28 GB/s, 150 ms before the write. They are now
    `finish_on(jobs)`, 1 MiB a job, about 9 ms at the socket's bandwidth
    (`an_image_finished_on_workers_is_the_serial_one`).
  - A section built through `Builder::section`'s writer is written in place. One given to
    `Builder::add` is copied once more, 4 GiB at about 15 GB/s, 270 ms. The db and VFS
    requests ask for the writer.
  - With the writer, about 0.61 s, **about 98%**.
- **What stands in the way.**
  1. `Table::write` (the GC) runs on one core: for \(10^6\) symbols, about \(35 \times 10^6\)
     instructions (copying, plus one `value_hash` and an insert each kept value), about 3 ms.
     Parallel, the bound is about 0.1 ms. It runs at the daemon's stop, after the last
     answer. Taking `jobs` there too, with the starts as a prefix sum over chunks and the index
     built by group-partitioned inserts, removes it. Owner: frontend-image, next.
  2. The write itself is one `Effect::Write` (temp file, fsync, rename, directory fsync).
     That is 4 syscalls plus the data's writes, the minimum for an atomic replace.

## frontend-deps: dependencies from images (frontend.md item 5)

What is matched: rustc's `.rmeta` load, which maps each dependency's metadata and decodes it
lazily, checking only the SVH in its header. blaze also maps its images (`Reader::pinned`)
and reads parts in place, but it verifies each image's BLAKE3 before use, as the store
verifies every value. That check is the one cost rustc does not pay.

A session start with 300 dependencies, images averaging 150 KB (45 MB). Figures assumed, not
measured:
- **Keys**: per crate, the crate's fields and about 100 files as path and content ID
  (about 70 bytes each, so 7 KB). BLAKE3 runs at about 1 cycle a byte on short inputs, so
  that is about 2 µs a crate and 0.7 ms for 300, on one thread (`keys` walks dependencies
  first). The file content IDs are the VFS's, already made. `Base::of` and `portable` add
  one scan for each path, which is noise next to the hash.
- **Serve, hit**: one get per crate (`K`), a table lookup and a mapping, then BLAKE3 over the
  image at about 4 GB/s a core: 11 ms on one core, 1.4 ms on 8 (`ImageStore::for_each`).
  Opening is header and bounds checks (constant per crate), and `frozen::unreadable` is one
  pass over the def map part's file names. Nothing is copied.
- **Serve, crate with reads**: one get for `K` that misses, then one per slot up to the hit
  (one in practice), plus `reads_now` per shape. That stats and hashes each file the
  expansion read, the same work `blaze_expand`'s own cache does before it reuses an expansion.
  It is bounded by the files read; for a derive that reads nothing, the hash of an empty shape.
- **Bound**: the verification. 45 MB / (4 GB/s × 8 cores) ≈ 1.4 ms, against rustc's
  ≈ 0.3 ms to map and check headers (300 × 1 µs). Every later lookup is a load from the
  mapped part, as rustc's is. Avoiding the verification would mean trusting unverified bytes
  from a store, which AGENTS.md's review rules forbid, so the gap stays.

Store side: `make` per crate encodes the frozen def map and signatures, then scans the three
parts once for each machine path of at least 8 bytes (`held_by`, a first-byte scan with
`position`, about 0.3 cycles a byte for a 150 KB image). It runs once per new image, on
`for_each`'s cores.

## frontend-deps-reparse: reparse and re-lower only what an edit touches (frontend.md item 14)

Design: [reparse.md](reparse.md). An edit's bound is "recomputing only what changed, plus the
fixed latency of each stage". The figures are for a large file: \(F\) = 50 KB of text,
\(E\) = 30 000 tree elements, \(I\) = 500 top-level items, \(A\) = 1 000 attributes (most of them
doc comments), and item signatures (what lowering reads, bodies left out) of about 10 KB. A
keystroke changes one item.

State of the art: tree-sitter and Roslyn reparse an edit in microseconds by reusing every
untouched subtree in place; rustc re-lowers per HIR owner but reparses whole files;
rust-analyzer reparses a token or block in place and re-lowers the file's whole item tree.

### 1. The edit, from the previous parse's text (`syntax::parsing::edit_between`)

Work: find the common prefix and suffix of the old and new text.

- **Bound.** It reads both texts once: \(2F\) = 100 KB. Both are hot (the editor just wrote one,
  the last parse holds the other), so at L2's 32 bytes a cycle that is about 3 100 cycles,
  **0.9 µs** (from DRAM, 100 KB / 25 GB/s = 4 µs).
- **Design.** It compares 4 KiB blocks, then 64-byte blocks, then bytes, from each end. Each
  block is one `memcmp`, about 20 cycles to call against the 256 cycles that streaming 4 KiB
  of each text takes, so **about 92% of the bound**. Before this change it compared 64-byte
  blocks: one call per 4 cycles of streaming, about 20%.
- **What would remove the stage.** The VFS knows the edit (`blaze_vfs::Op::Edit`). If base-db
  passed the edit with the new text, nothing would be compared, but a database would then trust
  an edit log to match a tree. The diff is checked against the tree's own text, which is why the
  design keeps it: at 0.9 µs, it costs less than any other stage below.

### 2. The kept parse (`syntax::parsing::kept`)

Removed on 2026-10-29: db's D8 (`base-db/src/reparse.rs`) keeps the last parse, and
`parsing/kept.rs` is deleted. The figures below were for the deleted table.

Work: find the file's last parse, and keep the new one.

- **Bound:** one uncontended lock and one reference count each way, about 50 cycles.
- **Design:** a direct-mapped table of 128 slots, each a mutex holding one `(key, Parse)`. Taking
  the parse locks once and clones three `Arc`s; keeping it locks once and swaps. That is about
  100 cycles, under 0.05 µs. It is within noise of a keystroke's microseconds, so it is not a
  term worth tightening.

### 3. The splice (`FlatTree::replace_subtree`, under `incremental_reparse_onto`)

Work: the tree of the new text, which is the old tree with the reparsed node's subtree
replaced. Reparsing the node is the parser's cost (frontend-tokens' and speed-syntax's
sections).

- **Bound.** With a flat tree that an edit copies, the bound is copying its arrays: 6.5 bytes an
  element, read and written, \(2 \times 6.5E\) = 390 KB at 32 bytes a cycle. That is about
  12 000 cycles, **3.5 µs**. The input check compares the text around the node with the old
  text again, another 0.9 µs.
- **Design, as it stands.** `replace_subtree` repacks every element: `parent`, `end` and
  `repack`, about 35 instructions at an IPC of 2.5, so about 15 cycles an element. Then
  `Packer::finish` makes another pass for marks and ranks, about 5 cycles an element. That is
  about 600 000 cycles, **about 170 µs: 2% of the bound**, and the largest cost of a keystroke
  in a big file.
- **In the way, and what removes it.** The tree code is `syntax-tree`'s, and requests-subtree-hash
  is editing it now. The request is in `v2/requests/format.md` ("`FlatTree::replace_subtree` by
  copying, not repacking"):
  1. Copy the arrays, and patch only what the edit moves: the ancestors' sizes, the tokens that
     are direct children of an ancestor after the edit (their distance to their parent), the
     node parents after the edit (a compare and add each), the side tables' indices, the node
     bits (a shift), and the marks after the edit (read from the old marks). That is about 1.5
     cycles an element, **about 40 µs, about 9% of the bound**, because the marks and node
     parents after the edit are still rewritten.
  2. Reaching 90% needs unchanged items that are shared, not copied, as Roslyn's green nodes
     are: the top level's items as their own arrays under a spine, so that an edit copies one
     item and the spine. Its bound is then the item's size plus \(I\) spine words, under 1 µs.
     That is a layout change for format to weigh.

  Step 1 is applied (requests-subtree-hash, `v2/requests/format.md`): `replace_subtree` copies
  the arrays in three runs and patches what the edit moves, with `repack` gone. Not yet
  measured; step 2 remains.

### 4. Finding the items an edit changed (`hir_def::item_tree::items::item_keys`)

Work: on every edit of the file, tell each top-level item that reads what it read before
(syntax, ast ids, span anchors, the crate's `cfg` set) from one that does not. There is one
query per file (`file_item_tree_query`) and no salsa query per item. A per-item tracked struct
plus its memo costs about 250 cycles an item an edit, more than lowering the item (about 200),
so per-item queries could at best tie a whole-file lowering (about 166 µs an edit and 1 950
cycles an item cold by speed-hir-sol-items' count, against 29 µs and 170 cycles). Instead the
file's last tree is kept beside salsa (`KEPT`, 128 direct-mapped slots, as
`syntax::parsing::kept` keeps the last parse), with its text and a key per item.

- **Bound.** What changed is known from the text: the common prefix and suffix of the old and
  new text, 0.9 µs (stage 1's bound). Each item outside them reads what it did, but for its ast
  ids and anchors, which an edit elsewhere can renumber (a disambiguator). Comparing those as two
  arrays, the map's entries in the prefix as they are and in the suffix moved by the edit's
  length, is about 10 KB, about 0.3 µs. Then a compare per item to place it, about 5 cycles,
  0.7 µs. That makes **about 2 µs**.
- **Design, applied now.**
  - `common_ends` compares the kept text with the new one in 4 KiB `memcmp`s, then 64 bytes,
    then bytes: about 1 µs.
  - One walk of the items takes their ranges (about 10 cycles an item, 1.4 µs).
  - An item in the suffix, matched to the old item at its place moved by the edit, or in the
    prefix with two whole items after it there, keeps its old syntax fingerprint. A file's parse
    is a function of its text, and the parser reads an item from the item list's state, its own
    tokens and at most three tokens past its position, so such an item's syntax is the same.
    Only the items the edit touched, and the two before them, are hashed (about 850 cycles each:
    the signature walk, about 40 writes into one reused buffer and one BLAKE3 pass, with bodies
    left out). That is about 2 500 cycles, 0.7 µs.
  - The ast ids and anchors are compared in bulk (`Unchanged::touched`). The slot keeps the
    last file's `AstIdMap::entries` (`[start, end, kind, id]`, breadth first) and
    `RealSpanMap::anchors` (as `[start, id]`, in text order). One merge pass over each array
    takes the entries in the unchanged text from old and new (end within the prefix, or start
    in the suffix with the old ones moved back by the edit's length) and pairs them by place and
    kind, run by run (a breadth-first level is one run of rising starts). A pair counts only if
    its id is equal too. Any other entry marks the new item holding its start, found by a binary
    search of the items' ranges. That is about 3 cycles an entry, for about 600 entries and
    600 anchors: 3 600 cycles, **1 µs**. Copying the new arrays into the slot for the next edit
    is 14 KB, 0.15 µs.
  - An item `Unchanged` keeps and nothing marked copies its old ids and anchors (`Keys`, about
    25 bytes an item, 0.15 µs for all of them). Only the touched items, and those `Unchanged`
    doesn't keep, read the maps per item: one hash probe and one binary search, about 90 cycles
    each, so about 300 cycles for a keystroke. A renumbered id (a second `fn f` added before
    another) marks only the item holding it. A run that falls out of step with the other side
    only marks more items; it never makes a pair count wrongly.
  - The keys are compared from each end (about 10 cycles an item, 1.4 µs), and the top level's
    attributes are lowered again and compared (about 0.3 µs).

  That is 1 + 1.4 + 0.7 + 1.3 + 0.15 + 1.4 + 0.3, about **6 µs, about 33% of the bound**.
  Before the span slices, every item's ids and anchors were read on every edit (\(90 I\) =
  45 000 cycles, 13 µs), about 18 µs in all. Before that (one BLAKE3 fingerprint of every
  item's syntax, ids and anchors on every edit), it was about 850 cycles an item, 120 µs.
- **Applied (2026-10-29): what was in the way.** `AstIdMap::entries` and
  `RealSpanMap::anchors` are lent as slices (frontend-tree-span, `v2/requests/frontend-deps-items.md`),
  and the bulk compare above replaces the per-item reads.
- **What is left.** Two per-item passes: the walk that takes the items' ranges (1.4 µs) and
  comparing the keys from each end (1.4 µs). Both go once the prefix and suffix are placed as
  runs rather than item by item: the top-level items are the first breadth-first level of
  `entries`, so their ranges can be read from there. That gives **about 3 µs, about 70%**.

### 5. Lowering a changed item (`Ctx::lower_item`)

- **Bound:** the lowering itself: interning the item's names and paths and lowering its
  attributes and visibility, a few hundred instructions for a typical item, about 0.3 µs.
- **Design:** the same lowering as the whole file's, in the file's own context (its ast id map
  and span map, so no per-item map and no binary search of ids), on the worker's builder. Its
  `finish` makes one allocation, the tree's block (`item_tree/block.rs`: a header of column
  ends, then every column), and moves each column into it, about 60 cycles. That is about
  **0.35 µs, about 85% of the bound**, once per changed item. An edit inside a body changes no
  key and lowers nothing: the kept `Arc` is the result, which salsa compares by address
  (`Arc`'s `Eq` fast path) and backdates.
- **Applied (mem-format-tree-items):** before, `finish` made one allocation for each non-empty
  column, about 7 for a `fn`, about 300 cycles, 0.4 µs. What is left is the lowering itself.

### 6. Assembling the file's tree (`assemble`)

Only an edit that changes a key assembles: the kept rows of the unchanged items before and after
the changed ones, as two runs, and each changed item's rows.

- **Bound.** The output's plain columns: about 40 bytes an item, so \(40I\) = 20 KB written plus
  as much read, about 1 300 cycles, **0.4 µs**. Then one reference for each part's non-plain
  data. With attributes shared, that is \(A\) reference counts, about 20 cycles each,
  20 000 cycles, 6 µs.
- **Design.** The tree's one block is allocated once at every column's summed length (one
  allocation a tree, not one a column) and filled in one pass, moving
  each id with a compare and an add (about 2 cycles an element), about 10 000 cycles. Then each
  attribute is cloned: its `Box<AttrInput>` is an allocation, a copy of its literal or token
  tree, and a free, about 150 cycles. \(150A\) = 150 000 cycles, so the total is **about 45 µs,
  about 13% of the bound with shared attributes**.
- **Applied (10-30, counted, not built):** `Attr::input` is `Option<triomphe::Arc<AttrInput>>`
  (`hir-expand/src/attrs.rs`), so cloning an attribute is one reference count: the fill's
  10 000 cycles plus \(A\) counts at about 15 cycles, about 25 000 cycles, **about 7 µs, about
  85%**.
- **In the way, and what removes it.**
  1. `Attr::input` is a `Box`. The request is in `v2/requests/db-expand.md` ("`Attr::input`
     shared, `Option<Arc<AttrInput>>`"). My side is ready: inputs are built with `.into()`.
     With it, assembly is about 25 000 cycles, 7 µs, **about 85%**.
  2. The rest of that gap, and the 6 µs of reference counts, go if the tree keeps each part's
     attributes as one shared slice: an `Arc<[Attr]>` per fragment, found by binary search on its
     start. Assembly then takes \(I\) references, not \(A\), which gives about 2 µs. That is a
     change to `ItemTree`'s attribute column and its persisted form; it is mine and comes after
     item 4.

Against a whole-file lowering on every edit (`lower_whole`: 29 µs today, about 11 µs at its
bound by speed-hir-sol-items' count, then salsa comparing the equal tree to backdate it, about
10 µs): a keystroke in a body is about 6 µs here, with the span slices applied, and lowers
nothing; with the prefix and suffix placed as runs it is about 3 µs. A signature edit was
6 + 0.35 + 45 µs, worse than `lower_whole` until `Attr::input` was shared. Now it is
6 + 0.35 + 7, about 13 µs, and about 6 µs with the runs placed and the attributes kept as
shared slices.

Cold, a file is lowered whole (`lower_whole`) in one pass and keys nothing, so a file never
edited (every dependency) never pays for keys. Its first edit lowers it whole again and keys
every item: 29 µs plus about 940 cycles an item, about 160 µs once per file. A slot holds 32
bytes of key, about 25 of ids and anchors, about 30 of the maps' entries and anchors, and 112 of
marks per item, and a count of the text the keys were made of, for the at most 128 files edited
last.

### The keystroke, end to end

The stages above are mine. The stages between them are not: the reparse itself, and base-db's
`ast_id_map` and `real_span_map`, both \(O(F)\) passes. Mine, for a keystroke in a 50 KB file,
a body edit (most keystrokes) and, in brackets, one that changes an item's signature:

| Stage | Bound | Before | Now | With the removals |
| --- | --- | --- | --- | --- |
| edit | 0.9 µs | 4 µs | 1 µs | 1 µs |
| splice | 3.5 µs | 170 µs | 40 µs (copy in runs, stage 3 step 1) | under 1 µs shared |
| changed items (4) | 2 µs | 260 µs | 6 µs | 3 µs |
| changed item lowered (5) | 0 (0.3 µs) | 0.9 µs | 0 (0.35 µs) | 0 (0.35 µs) |
| assemble (6) | 0 (6 µs) | 45 µs | 0 (7 µs, `Attr::input` shared) | 0 (2 µs, attributes as shared slices) |
| **total** | **6 µs (13 µs)** | **480 µs** | **47 µs (54 µs)** | **5 µs (7 µs), with the splice shared and attributes as slices** |

The splice is the largest term left and is format's (stage 3). `Now` counts step 1 as applied
(`FlatTree::splice_all` copies the arrays in runs and patches what the edit moves), about 7.8×
the bound for a body edit. Step 2, sharing the unchanged items under a spine, is a layout change
of the flat tree and its persisted form, for format and Andrew to decide. Nothing has been
measured.
`proof`'s budgets time each row once things compile.

## frontend-tree: borrowed views over the flat tree (frontend.md item 16)

Design: [tree.md](tree.md). Below, \(e\) is a file's elements. Rust has about 6 bytes of source
per element, so a 100 KiB file is about \(e = 17\,000\).

### 1. A whole-file walk (`Node::descendants_with_tokens`, `preorder_with_tokens`)

**Bound.** The walk has to read each element's word once. These are 4-byte words in a single
array, streamed in order: \(4e\) bytes, which is 68 KB for the file above, held in L2. One step
is a load, a tag test, an increment and the 24-byte view it yields. That is about 4 instructions,
so 1 cycle at an IPC of 4. The bound is therefore \(e\) cycles, about 5 µs for the 100 KiB file.

**Design.** `Scan` reaches the bound. It walks `at..end` linearly with no stack, no refcount and
no allocation. A lazy node costs two more things:
- finding its part, a binary search over the file's lazy nodes (a few hundred at most, so about
  9 steps);
- an acquire load of the part's `OnceLock`.

That is about 40 cycles per lazy node. Lazy nodes are fewer than 1 in 50 elements, so this adds
under 1 cycle per element on average.

`preorder_with_tokens` also reads `end` of each node to emit its leave event, and that word is
already in L1. The rowan-style cursor does more work at each step:
- a `Cell` refcount increment and decrement;
- a branch to free the cursor;
- the root `Box` that `Tree::root` allocates per call;
- a `RefCell` map lookup per part.

That is about 3 times the bound before its allocations.

**Gap.** `text_range()` on a view recomputes the start from the nearest of every eighth start,
which reads up to 15 words: about 10 cycles, all L1 hits. A walk that asks every element's range
pays that per element, about 10 times the walk itself. `Node::descendants_with_ranges`
(sol-all, 2026-10-30) carries the running offset instead: a token's range is one add, and a
node's end is `FlatTree::node_end`, a fold over its own words when its subtree ends in the same
block of eight, else a kept start and up to seven lengths. Little over half the elements are
tokens, so that is about 1 cycle a token and about 2.5 a node, **about 1.7 cycles an element,
1.7× the bare walk** (estimated by count, not measured), against about 10× before. Its test
checks every element's range against `text_range` from every node of 300 random trees with lazy
nodes. What removes the rest: a node's end is the start of the element after its subtree, which
an in-order walk reaches anyway, so a walk that gives each node's range at its leave event
(`preorder_with_tokens` carrying the offset) is one add an element, at the bound.

### 2. Navigation (`parent`, `children`, siblings)

- **A token's parent** is a subtraction from its word, 1 load.
- **A node's parent** is its rank (one bitset word, one count word and a popcount) plus a load
  from the parents column: 3 loads, about 15 cycles counting the L1 hits. The bound is 1 load,
  because the parent has to be stored somewhere. The rank costs 2 loads to save 4 bytes per
  element, and that trade belongs to the tree's design ([flat.rs](../../../../../crates/syntax-tree/src/flat.rs)),
  not to the views.
- **Children** step by `end`: 1 load per child, however large the child's subtree. This meets
  the bound.
- **A typed accessor** (`support::child`) is a scan of children that compares kinds, which is the
  same as the bound for a lookup with no index.

### 3. The view itself

A view is 24 bytes: file tree, tree, part and element. It is `Copy`, and making one costs
nothing beyond the registers that hold it. A `NodeId` that is kept is 8 bytes. A smaller view,
16 bytes with the file tree found from the part, would need a load to walk back up out of a
part. That would make the common case, reading within one tree, slower to save stack space that
is already in registers.

### 4. Descendants that are only nodes

`descendants()` walks tokens and skips them, so it does \(e\) steps to produce \(n \approx 0.45e\)
nodes. The tree's node bitset would let it jump from node to node with one `tzcnt` per 64
elements, about \(n\) steps. That needs a `pub(crate)` accessor in `flat.rs`, which this lane
does not own; it is requested once callers that walk only nodes are on the build path. Until
then, a node walk runs at about \(e/n \approx 2\) times its bound.

Nothing has been measured. `crates/syntax/tests/view.rs` checks the allocation half, zero
allocations per walk. The cycle figures need `proof`'s timing budgets once the crates compile.

## frontend-journal: changed files from the OS's journal (restore.md)

The full arithmetic is in [journal.md](journal.md), "Speed of light", and in each platform's note
([macos](journal/macos.md), [windows](journal/windows.md), [linux](journal/linux.md)). For a warm
restart with N = 100 000 tracked files of which k changed:

- **macOS and Windows:** the work is O(k). There is one column search and one batched stamp
  per path the journal names, and a read and a hash only where the stamp moved. Nothing is
  done for an unchanged file, so this is at the bound (the k reads and hashes).
- **Linux:** the `statx` sweep is N·t/C, about 6.3 ms on 8 cores and 0.78 ms on 64. The
  watcher adds about 0.35 ms of serial work: one copy of the mapped column's names, decoding
  the saved stamps, and comparing N stamps. That is 6% on 8 cores, inside the bar. On 64 cores
  it is about 45% over. To close that, the stampers would compare against the mapped column in
  place and return only the indices that moved; that is open (see journal.md).

## frontend-tree-move-diag-lints: the early lint pass's walk on views

`blaze-diagnostics`' `Walk::add` (`lints/syntax.rs`) visits every node of every tree of the
crate once, in preorder, and writes three columns per node it keeps (a 24-byte view, a 2-byte
kind, a 1-byte flag).

- **Work.** Per node: one preorder step (`first_child`/`next_sibling`/`parent`, arithmetic on the
  flat tree's `u32` columns), one kind read, four kind tests (`AnyHasAttrs`, `Item`,
  `MACRO_CALL`, `Adt`), 27 bytes written. Per attribute owner, its children scanned for `ATTR`
  (`attrs_including_inner`): summed over the tree, at most one more pass over the elements.
  About 30 instructions a node.
- **Bound.** At 4 GHz and an IPC of 3, 2.5 ns a node; the 27 bytes written and about 16 read per
  node are 43 MB per million nodes, about 2 ms at 20 GB/s. So 2.5 ms per million nodes
  (a 1 MB crate has about 300,000), instruction-bound.
- **How it is reached.** No handle (the handle walk counted and dropped a root per step and
  per clone, about 4x the instructions), no allocation per node: the columns are sized once
  by `node_count`, a scan of the token flag of each element (one byte-wide pass, under 10% of
  the walk), so they never grow; the stack is reserved at 64. Casts test the kind before
  anything else. `text_range()` (the 10x-slow step frontend-tree names) is read only for the
  nodes that are skipped or expanded, not per node. `Walk::attrs` is an iterator over the tree
  (`hir_expand::attrs::cfg_attrs_view`): an owner with no `cfg_attr` allocates nothing.
- **Proof.** `walking_a_tree_allocates_only_its_columns`: four allocations for the walk of a
  tree at 4 and at 400 functions. The time bound is checked once the coordinator builds.
- **State of the art.** rustc's early lint pass walks `&'a ast::Item` references into its arena:
  no counts, a pointer chase a node. A view step is the same cost without the pointer chase,
  since the tree is columns.
