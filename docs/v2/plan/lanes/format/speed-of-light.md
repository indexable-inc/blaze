---
status: plan
summary: lane format's speed of light: the bound each component's work and the hardware set, the arithmetic, and how the design reaches 90% of it.
date: 2026-10-29
---
# Lane format: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). Each component of lane format has a section here,
written by the agent that owns it.

## The machine

The target machines are in [hardware.md](../../design/hardware.md): EPYC Turin, Graviton 5 and
Apple M-series. Unless a section says otherwise, the figures are these, for one core, not yet
measured:

| what | figure |
|---|---|
| clock | 3.5 GHz (figures in ns use it) |
| L1 hit, L2 hit | 4 to 5 cycles, about 14 |
| L1 read, streaming | 64 B a cycle |
| L2 read, streaming | 32 B a cycle (about 110 GB/s) |
| one core from DRAM, streaming | about 25 GB/s |
| IPC | about 2 instructions a cycle for branchy pointer code, about 4 for straight-line integer code |
| vector ALU ports | 2 of 512 bits (Turin's Zen 5), 4 of 128 bits (Graviton 5's NEON) |
| `crc32q` | 3 cycles latency, 1 a cycle |
| BLAKE3, many inputs in lanes | about 26 cycles a block an input with AVX-512 (16 lanes) |

None of this has been measured yet: nothing compiles until every lane is done. `proof`'s budgets
measure each bound once it does.

## Content IDs: `crates/blaze-ids` (speed-sol2-format)

The BLAKE3 backends themselves (the fork's `hash_many`, the official crate) and their bounds are
[blake3-servil's speed of light](../blake3-servil/speed-of-light.md). This section covers what
blaze-ids builds around them. Estimates are ±50%, by counting instructions.

### `ContentId::of` and `Hasher` over one block

Work: find the domain's context key, then one compression of at most 64 bytes.
- **The compression** is 7 rounds, each two half-rounds of 4 `G` in one register, and each half
  is 8 dependent add, xor and rotate steps plus the diagonal shuffles. That is about 7 × 2 × 14 ≈
  200 cycles on its dependency chain, with nothing beside it to overlap. **Bound: about 210
  cycles (60 ns)** counting the input's load and the key.
- **Before (speed-sol2-format, from the code):** the key table hashed the domain's text with FNV,
  one dependent multiply a byte. Domains are 20 to 40 bytes, so that is 70 to 130 cycles before
  the compression starts, **1.3× to 1.6× the bound**.
- **Now:** the table is keyed by the domain's address and length (`home` in `lib.rs`): one
  multiply, a shift, one load and a pointer compare, about 10 cycles, so **1.05×, within 10%**.
  A slot compares by address and then by text, so one domain's text at two addresses takes two
  slots with the same key. The model test `model_colliding_domains_keep_their_own_keys` covers
  it.
- **The `Hasher`** of the fork keeps its 16 KiB stage uninitialised (`MaybeUninit`), so
  `Hasher::new` writes about 100 bytes, not 16 KiB. The official backend's `Hasher` is 1.9 KiB,
  but it serves only the secondary targets.

`proof` measures cycles a call to `ContentId::of` of 32 bytes in a known domain, and a cold
first use of a domain.

### `ContentId::of_many`: many short inputs (item hashes)

Work: for each input, \(\lceil len/64 \rceil\) compressions, the reads of its bytes and a 32-byte
store. Inputs are mostly under a chunk: item and key hashes of 8 to 200 bytes.
- **Bound on Turin:** 16 lanes in a 512-bit register. Each compression is about 26 cycles a block
  an input on the two vector ports, and a message transpose adds `log2(16)` × 16 / 2 = 32
  interleaves of two shuffles over 16 inputs, about 4 cycles an input. Reading the block and
  writing the ID are a few cycles more. **About 33 cycles a block an input, 9.5 ns.** On
  Graviton 5 the lanes are 4 a register, and the bound is blake3-servil's.
- **Before (from the code):** whole blocks went to the fork's `hash_many`, at 16 lanes with
  AVX-512 on the servil targets. But every last block (the only block of a short input) went to
  `compress_lanes::<8>` at the baseline features, SSE2 on x86-64: two registers a word and a
  rotate in 3 instructions, about 100 cycles a block an input. Its message was scattered word by
  word into the transposed array, and each block was copied into a zeroed stack block through a
  `memcpy` call. Together that was **about 4× the bound** for inputs of one block.
- **Now:**
  - `last_blocks` dispatches on the CPU to 16 lanes with AVX-512 and 8 with AVX2, through
    `#[target_feature]` wrappers.
  - Group 0 starts from a splat of the key, not a transposed copy of it.
  - Messages are transposed in registers (`columns`, `transpose`: `std::simd` interleaves), and
    so are the outputs (`rows_of`).
  - A last block is read 8 bytes at a time, ending on an overlapping read (`padded_words`).
  
  The estimate is about 40 cycles a block an input, **1.2×**. The rest is the variable-length
  read, a load a word plus a branch on the length. A masked load (`vmovdqu8` with a length mask)
  reads it in one instruction, but that needs an intrinsic and so `unsafe`, which
  [unsafe.md](../../design/unsafe.md) allows only for a measured gain. `proof` measures it first.
- **Tests** (`many/tests.rs`): register transposes in both directions against scalar ones at
  widths 16, 8, 4 and 1. The word read against a zero-padded copy at every length and offset.
  Every width and path (8, 16, dispatched, AVX-512, AVX2) alike over every group and member
  count. One lane against `blake3` itself at lengths 0 to 64.

`proof` measures cycles an input for `of_many` over 10⁵ inputs of 32, 64, 65 and 200 bytes, on
Turin and on Graviton 5.

### `crc32c` and `bits`

- **`crc32c`** runs three interleaved `crc32q` streams (`crc32x` on ARMv8) and joins them with
  a GF(2) shift by table, 32 lookups a block (`combine`). It is 24 bytes each 3 cycles, about 8 B a cycle (28 GB/s), at the instruction's bound and at
  one core's DRAM stream. Within 10%.
- **`bits`** is word loops over `u64` that LLVM vectorizes: popcount, and/or/xor of bit sets. It
  is at L1 or L2 bandwidth. Within 10%.

## Records, columns and tiers: `crates/blaze-format` (speed-sol2-format)

\(S\) is a buffer's bytes and \(r\) the number of records and references (`Rel`, `Slice`) in it.

### `open`, `get` and following a reference

Work: `open` must check every byte it will hand out: alignment, bounds, enum tags, `bool`s,
`char`s. That is a pass over the buffer, about \(r\) bounds checks and one tag check a narrow
field, **bound about \(S / 32\) cycles plus 3 a reference**. After that a `get` or a `follow`
needs only its own bounds check, **O(1), a few cycles**.
- **Before (from the code):** `open` checked everything deeply, and so did every `follow`:
  `raw::view` called `T::check`, which follows every reference reachable from the target. Reading
  each reference of a tree of depth \(d\) once cost about \(d × S\), not \(S\). A `Slice<Node>` of
  50 nodes, each with two references, re-checked all 100 leaves on `get` and again on each
  follow.
- **Now:** `Record` has `check_shallow` and `SHALLOW_PLAIN`. A follow checks only the target's
  own bytes and the references in place, not what they point to. `open` stays deep, so every
  reachable byte has been checked once before the first `get`. A record of plain fields and
  references is `SHALLOW_PLAIN`, and a follow of it is a bounds and alignment check only.
  **Within 10%.** The test `a_get_after_open_checks_only_what_it_reaches_directly` counts the
  checks: 200 on `open`, 0 on `get`, and 200 to follow every reference once.

`proof` measures `open` in bytes a cycle on a 1 MiB item-tree buffer, and `follow` in cycles.

### The writer and `bytes_of_slice`

`Writer::push`, `push_slice` and `push_rel` append to one `Vec<u8>` at its alignment, a store a
word. `bytes_of_slice` (added for queue 00127) gives a slice of records as bytes in place, so
hir-ty's column hashing takes no copy. The writer is at the store bound. Within 10%.

### Tiers (`tiers.rs`, `tiers/buffer.rs`)

- **Hot** columns are kept at the narrowest width that fits their values. A read is one load at
  a known offset. At the bound.
- **Warm** is SIMD-BP128 with frame of reference: 128 values a block, unpacked by shifts in
  vector registers, about 1 to 2 cycles for 4 values. That is near the L1 stream bound and
  within 10%.
- **Cold** chooses runs, deltas or a dictionary by exact size, then zstd level 9 with a
  per-kind dictionary. zstd's decode, about 1 to 1.5 GB/s, is all of the time. The column codec
  sorts once with `blaze_ids::radix::sort`. The dictionary decode collected into a `Vec` of
  unknown size through `Result`, which grew it about log₂ n times. It now fills one of
  exact capacity (speed-sol2-format). Within 10% of zstd's own bound.

### `leb` and `cursor`

- **`leb`** decodes a one-byte value at once. A longer one is a single 8-byte load, finds the
  stop bit with `trailing_zeros` and compacts the 7-bit groups. That is about 6 instructions
  for values of up to 8 bytes, against a bound of a load and a compact. Within 10%.
- **`cursor::Reader`** is one length compare and one load a read, with the error paths
  `#[cold]`. That is its bound ([dedup's cursor.md](../dedup/cursor.md)).

## The HIR expression store: `hir-def` `hir/`, `expr_store/` (speed-hir-store)

Layout and history: [speed-hir-store.md](speed-hir-store.md). Below, \(n\) is the number of HIR
nodes in a body (expressions, patterns, statements and type refs) and \(S\) the store's bytes.

### Lowering a body, `Body::of` (no source map)

Work: walk the body's syntax once and write one row per HIR node.
- **Bytes.** Each HIR node reads about 6 tree elements (4-byte words) and about 6 bytes of text,
  so about 30 bytes, and writes a 5-byte row, about 4 bytes of `extra` and list entries, and 4
  bytes on the list stack: about 45 bytes in all. The anchor's tree was just parsed or is kept
  per thread, so it is in L2. That is about 1.5 cycles a node at L2 bandwidth, and bytes are not
  the bound.
- **Instructions.** The minimum per node is about 5 to dispatch on the syntax kind, about 10 to
  find its children in the flat tree, about 5 to write the row, and about 3 for the list stack.
  Add a name interned for about one node in three (hash about 8 bytes and probe one table:
  about 25, so about 8 a node), and a hygiene lookup in the body's span map, which almost always
  has one context (about 5). That is about 40 instructions, at IPC 2 about **20 cycles, 5.7 ns
  a node**.
- **Fixed cost per body.** One salsa query and one exact allocation per non-empty column at
  `finish` (8 to 20 of them, about 30 ns each): about **1 µs**.
- **The bound** is \(5.7n\) ns + 1 µs per body. A build lowers bodies in parallel, one query
  each, so a crate graph's lowering is bound by total nodes over cores.
- **Where the design is.** The store side is at the bound: rows and CSR lists, per-thread
  pooled scratch with no allocation per node, and no hashing per node. Without a reader of the
  map, no source map is built, so no `AstPtr` and no table push either.
- **What stands in the way, in the walk** (estimated, to be measured first):
  1. Each `ast::*` step goes through a `SyntaxNode` handle. If a handle costs more than about
     10 instructions (a refcount, a parent pointer), it is the largest term. The fix is to
     lower from `(FlatTree, element index)` directly, with typed views over indices.
     Owners: speed-syntax (the views) and this lane (lowering).
     **Corrected (speed-sol2-format, 2026-10-30): done.** `lower.rs` takes `ast::Expr<'_>` and
     the other `ast` types as views, `Copy` over the flat tree and an element index, with no
     refcount (frontend-tree's move). A step costs the view's child search, about 10 to 20
     instructions, the bound's "find its children" term.
  2. `check_cfg` runs for every expression, statement, field and parameter. Since 2026-10-29
     it is one test when there is no attribute (`lower.rs` `may_have_attrs`: is the first child
     node an attribute? The grammar puts every `AnyAttr*` first). Before, it walked every
     child, and for a block every statement, through `dyn` and `expand_cfg_attr`.
  3. Interning a name hashes the token text again. The lexer could carry each identifier's
     hash, or a per-thread front cache could answer repeated names in a body without touching
     the shared table. Owner: the interner (format).
     **Still so (speed-sol2-format, 2026-10-30).** `AsName` (`hir-expand/src/name.rs`) calls
     `Name::new_root(text)`, which hashes again: about 6 instructions a word, small beside
     the probe. The probe of a name that isn't compiled in is about 1.9× its bound (the
     interner's section, "Interning a symbol"; queue 00934), so about 8 of the node's 40 are
     about 16.

  Once it compiles, the first step is `proof` measuring cycles per HIR node on the corpus.
  Anything above 22 cycles a node is a bug against this bound.

### Reading the store: `store.expr(e)`, `pat(p)`, `type_ref(t)`

- **Bound:** the bytes of the node, a 1-byte kind and a 4-byte data word in two columns, plus
  one `extra` read for a node of several words. That is 2 or 3 loads, L1 or L2 hits for a
  body being inferred: about 5 to 15 cycles.
- **Design:** exactly those loads and a `match` on the kind, so it is at the bound by
  construction. A whole-store scan reads \(5n\) bytes plus `extra`. The id-only scans
  (`expr_ids()`) read nothing, and borrowck's seven scans per body are filed to become one
  (`v2/requests/db.md`).

### `finish`: scratch to the store's columns

- **Bound:** write \(S\) once. Scratch is hot in L1 and L2, so the reads are nearly free, plus
  the allocations.
- **Design:** one copy per column, at the bound for \(S\). Since 2026-10-29 the `Copy`
  columns share one slab (`hir/col.rs`), so a body makes 1 allocation plus one per non-empty
  cold side table: about 4, not 8 to 20 (about 0.1 µs, not 0.5).
- **What stands in the way:** the side tables (names, literals) stay boxed, because their
  elements have destructors. Interning them into `u32` ids would let them join the slab.
  `TypeColumns` has its own slab. The path columns (`PathColumns`, since 2026-10-29 columns, not
  boxed values) are `Copy` but freeze to one box each, up to 12 a store, because lowering reads
  them back through the same views a frozen store has (`speed-hir-store.md`, "Still short").

### Encoding and decoding a store (`expr_store/persist.rs`)

- **Encode bound:** write \(S\) once, and hash it if its content id is taken. BLAKE3 costs
  about 0.6 cycles a byte with AVX-512 or NEON over large inputs.
- **Encode design:** each column is written as a `Slice`, which is a copy on little-endian
  targets. It is at the bound.
- **Decode bound:** unsafe.md requires bytes from outside the process to be hash-verified
  (0.6 cycles a byte) and bounds-checked before any view of them. The check (kinds below their
  count, list ranges inside their arrays) is about 1 cycle a 5-byte row, so about 0.2 cycles a
  byte. With views that borrow the buffer, the bound is about **0.8 cycles a byte**.
- **Decode design:** hash, then check in place, then copy each column out once. The copy adds
  about 0.1 cycles a byte plus one allocation per column, which is about 15% over the bound for
  a large store and more for a small one.
- **What stands in the way:** decode copies the columns, though since 2026-10-29 into one
  slab, not one allocation each. A store read in place from a crate image
  (`StoreBytes::InPlace`, most signature stores) could borrow its `Copy` columns instead.
  `Col` is the column type that can hold such a count, and `follow` already checks each
  `Slice`'s alignment. What's missing is the image's owner reaching `deps/signatures.rs` `read`
  as an `Arc` (frontend-deps-sigs, requested in `v2/requests/speed-hir-store.md`). Check also fuses into the hash
  pass, a 16 KiB block at a time, so the bytes are read from memory once.

### Expression scopes (`expr_store/scope.rs`)

- **Bound:** one pass over the expressions: read each kind (5 bytes), write its scope (4
  bytes), and push one entry per binding. About 10 instructions an expression, so 5 cycles.
- **Design:** one walk over the views into per-thread scratch columns, copied out exactly.
  Entries are grouped by scope, with a stable radix sort that is skipped when they are
  already grouped, which is every body without a `const` block inside a pattern. It is at the
  bound, estimated.

### An edit: what runs again

- **Bound:** for an edit inside body \(B\) of file \(F\), reparse what changed (speed-syntax),
  lower \(B\) again, and do \(O(1)\) for every other item of \(F\): compare a hash the parse
  carries.
- **Design:** a body is lowered from its own anchor's tree and is equal after any edit outside
  it, so nothing downstream reruns. But `body_syntax` runs for every body of \(F\), and each
  anchor is hashed whole (`tree_hash`, once per anchor per thread). So an edit costs
  \(O(|F|)\) hashing: \(|F| \times\) cycles a byte.
  - It was two SipHash-1-3 passes, about 3 cycles a byte: 1.2 M cycles, **340 µs**, for a
    400 KB file.
  - Since 2026-10-29 it is one BLAKE3 pass (`body.rs` `subtree_hash`): about 0.6 cycles a byte
    over a large anchor, so about **70 µs** for the same file. A single-function anchor under
    1 KiB gains little (one chunk, about 1.5 cycles a byte).
  - Signatures hash their trees the same way, filed in `v2/requests/speed-hir.md`.
- **Carried hashes:** `FlatTree::subtree_hash` keeps the hash of every node that costs 4 KiB
  or more to hash, made the first time it is asked for, and an incremental reparse carries it
  for the subtrees the edit does not change. So an unchanged anchor's `tree_hash` (`body.rs`)
  is one load (a popcount rank and a slot), and an edit costs the reparse plus hashing the
  anchors it changed, each once. They are made when asked, not while the tree is built: a
  parent's BLAKE3 pass is not made of its children's, so building them bottom-up would rehash
  every ancestor of an edit up to the root, the whole file, on every edit.
- **Source maps** are built only when something reads them. An IDE body read with its map is
  lowered twice, once by `Body::of` and once with the map. That is IDE-only, and within 2× of
  the bound for that body.

## The lexer: `LexedStr::new` (`crates/parser/src/lexed_str.rs`, `lexed_str/scan.rs`) (format-lexer)

\(n\) is the text's bytes and \(T\) its tokens, trivia included. Rust source has about 3.5
bytes a token: the token column is sized for 3, and whitespace runs and identifiers are short.
The token mix is about 40% whitespace, 30% identifiers and keywords, 22% punctuation, and 8%
literals, lifetimes and comments. These are estimates; `proof` measures them on the corpus
first, and every figure below scales with them.

### The work, and the bound

The output is a 4-byte token (kind and length) per token, plus a 4-byte checkpoint per 16,
so 4.25 bytes a token. That is data.md's budget, and the parser reads it in this form.

- **Bytes.**
  - The text, read once: \(n\).
  - The classes, written once and read back: \(0.5n\) each way. They are 32 bytes per 64 of
    text; the text and its classes are still in L1 or L2 when the token loop reads them.
    They live in the worker's scratch buffer (`blaze_mem::scratch`, design/memory.md), which
    stays committed and warm from one lex to the next, so they cost no allocation and no
    page faults.
  - The tokens: \(4.25T \approx 1.2n\).

  That is about \(2.7n\) of distinct bytes. From L2, at about 32 bytes a cycle, it is
  **0.08 cycles a byte**. From DRAM (a file read for the first time), at 25 GB/s, it is
  \(2.7 / 25\) ns, so **0.38 cycles a byte**. Memory is not the bound.
- **Instructions.**
  - **Classification.** On AVX-512 a 64-byte block takes about 14 vector ops and 4 stores,
    and turning whitespace into token starts takes about 10 scalar ops. That is about 28
    instructions per 64 bytes, **0.44 a byte**. On NEON it is about 80 per 64 bytes, 1.25 a
    byte.
  - **Each token.** The least a lexer can do works a block at a time. A fifth bitmap marks
    the bytes that start a token needing more than its run: digits, `/`, `#`, quotes, bytes of
    0x80 or more, and the rest of ASCII that is not punctuation. That costs about 6 more
    vector ops per 64 bytes, so classification is about **0.55 instructions a byte**. Every
    token between two special starts is then known to be simple, so each one costs about 9
    instructions:
    - the bit scan, the cleared bit and the length from the last start (3);
    - the first byte and its kind from a table (2);
    - pack and store into a 64-entry local buffer (3);
    - the loop (1).

    The column's capacity and the checkpoints are paid once a block, about 20 instructions,
    which is 0.3 a byte.
  - **Identifiers.** Each also needs a keyword test. `SyntaxKind::from_keyword_in` reads the
    identifier in place with one 8-byte load and probes one table slot, about 19 instructions
    (format-grammar's "Keyword lookup" below). Averaged over all tokens that is
    \(0.3 \times 19 = 5.7\).
  - In all, about 14.7 instructions a token, so \(14.7 / 3.5 = 4.2\) a byte. Adding 0.3 for
    each block and 0.55 for classification gives **5.05 instructions a byte**.
- **The critical path.** The only chain carried from one token to the next is finding the next
  token's start. Clearing the lowest bit of a register (`bits &= bits - 1`) makes it **1 cycle
  a token**, about 0.3 cycles a byte, which does not bind. Finding the start with a load at the
  last token's end would make it about 10 cycles a token, which binds (see below).
- **The bound.** Instructions bind: 5.05 a byte at IPC 4 is **about 1.26 cycles a byte, 0.35
  ns a byte, about 2.9 GB/s a core** on Turin. On Graviton 5 (5.9 instructions a byte with
  NEON's classification) it is about 1.5 cycles a byte.
  - A 100 KB file is about **35 µs**.
  - Files lex in parallel, one per task, so a cold build's lexing is total bytes over cores.
    The rustc tree's 120 MB of `.rs` would take about 0.42 s on one core, and 4.4 ms on 96.
  - Interning identifiers is not the lexer's: it happens later, in lowering.

**Corrected (speed-sol2-format, 2026-10-30, from the code).** This count leaves out three
things the output needs, so it is too low:
- the special tokens themselves: 8% of tokens at about 35 instructions, 0.8 a byte;
- the mispredicts on the simple-or-special branch, about 0.2 cycles a byte;
- the store of each token's start, which the checkpoints read, and the per-block window: about
  0.3 a byte more than the 0.3 above.

With them it is simd-lexer's count: **2.0 cycles a byte with AVX-512 VBMI**, 2.25 with AVX2 or
NEON, per kernel in [simd-lexer's speed of light](../simd-lexer/speed-of-light.md#the-bound).
That is the bound the benchmark `crates/parser/benches/lex.rs` holds the lexer to, and the one
both docs use. The 1.26 is kept above as the first count.

### Where the design is (2026-10-29; the next section has the code as of 2026-10-30)

The benchmark is simdjson. Stage 1 classifies 64 bytes at a time into bitmaps. Stage 2 walks
the structural bits with `trailing_zeros` and a cleared lowest bit, so the loop-carried chain
is one ALU op. Zig's tokenizer is the scalar bar.

As of 2026-10-29 the lexer has both stages:

- **Classification.** One vector pass gives four bitmaps:
  - identifier bytes;
  - whitespace, replaced in place by token starts (the first byte of an identifier or
    whitespace run, and every other byte);
  - `\n`;
  - `"` or `\`.
- **The token loop, `scan::Cursor` and `scan::token`.** The cursor keeps the current block's
  start bits in a register.
  - An identifier, whitespace or punctuation token takes three loads off the chain: its
    kind, its flags, and the byte after it. Add one `trailing_zeros`, one cleared bit and two
    branches that predict well. The branches are "needs special handling" (about 8% of
    tokens take it) and "the byte after may continue the token" (rare).
  - The kind is already the final `SyntaxKind`. Before, the fast path built a
    `rustc_lexer::TokenKind` that `extend_token` dispatched on again, and the next start was
    a load at the last token's end, the 10-cycle chain above.
- **Off the fast path, but still handled in `scan`:**
  - A line comment ends at the next `\n` bit.
  - A string ends at the next quote bit. If that bit is a `\`, the search skips it and the
    byte after it and goes on; the escapes are then checked once, on the contents alone, with
    the converter's `unescape_str`, and a string with an escape error goes to `rustc_lexer`, so
    its error text is unchanged.
  - A lifetime is `'` and an identifier run.
  - A plain decimal integer takes a byte or two of lookahead.

  Before, strings and lifetimes went to `rustc_lexer`, through a `Cursor`, UTF-8 decoding and,
  for a string, a second pass to unescape it.
- **The rest goes to `rustc_lexer`, one token each:** chars, raw, byte and C strings, strings
  with an escape error or a suffix, numbers with a base, fraction or suffix, block comments,
  and anything next to a byte of 0x80 or more. That is about 1% of tokens, at about 250
  instructions each: about 2.5 a token on average. `Cursor::seek` puts the cursor back after
  each.
- **Per token, the estimate is about 46 instructions against the minimum's 14.7.** The extra,
  about 31 instructions, is:
  - `LexedStr::push`'s work per token: the checkpoint test, the column's capacity, the
    running offset and the long-token test (7);
  - the per-token tests for special handling, for refilling the cursor's block and for the
    byte after the token (7);
  - the caller's test of the `Option` and of `IDENT` (3);
  - bounds checks (4);
  - the identifier's `&str` slice (1.2 on average);
  - address arithmetic (about 4);
  - the tokens `rustc_lexer` lexes (2.5).

  That is about 13.6 instructions a byte, **about 3.4 cycles a byte, 2.7× the bound**. Each
  term is an estimate good to about ±50%.

### Where the code is, 2026-10-30 (speed-sol2-format)

Items 1 and 2 below are in the code, though this section still listed them as to do:
- All five kernels and the scalar reference give the special-start bitmap (`classify.rs`,
  `SPECIAL`), with differential tests per kernel.
- `scan::simple` walks a block's start bits up to its first special start into a 64-token
  window and adds the block's checkpoints once.
- `scan::special` lexes numbers with fractions, exponents and suffixes, chars, escaped strings,
  raw strings and comments. A test holds it to at least 99% of the special tokens of the
  parser's sources.

One cost was left in the driver. After each special token, `simple` returned to `lex_rest`,
which lexed the token, pushed it through `LexedStr::push` and called `simple` again. The new
call re-derived the block's bits, zeroed a new 64-token window and truncated it, about 45
instructions on 8% of tokens. Now (speed-sol2-format) a special token that ends in its block
goes into the open window and the walk goes on. Only one that ends past the block closes the
window, goes through `LexedStr::push` and continues at the block it ends in. The test
`special_tokens_lex_alike_at_every_offset_of_a_window` (`differential.rs`) covers it, beside
the corpus, random and mutated differential tests. It lexes every kind of special token, alone
and in a row, at each of 130 offsets after four fillers, in every edition, and checks the
hashed lex's name hashes.

The count from the code, per token:
- **A simple token, about 15 instructions:** the bit scan, the add and the cleared bit (3), the
  length (1), the first byte's bounds check and load and its `FIRST` kind (3), the `IDENT`
  compare (1), pack (2), the window index and the two stores (3), the count (1) and the loop
  (1).
- **An identifier, about 20 more:** the length filter, `ident_window`, the `_` test and
  `strict_by_window` (fix-names' keyword hash).
- **A special token:** about 35, and 45 more for the 40% or so that end past their block.

That is about 24 instructions a token, or 6.9 a byte. Adding 0.6 a byte for the block and 0.53
for classification gives 8.0 instructions a byte. **That is about 2.2 cycles a byte on Turin,
1.1× the bound,** at the 10% line by a count good to ±50%. Before this change it was about 2.3.

What is left, about 3% to 5% each, in order of size:
1. **The start store a token:** select the checkpoint's token start from the block's bits
   instead. That is `pdep` and `tzcnt` on BMI2, or a broadword select elsewhere, about 4 a
   block, and saves 1 instruction a token.
2. **The first byte's bounds check:** read it from the block's 64 bytes, with a padded copy of
   the last block, at one check a block, saving 1 a token. `ident_window` is fix-names', so
   that is the only read this reaches.
3. **A special token past its block:** keep the window open across the boundary by making it
   hold the next block too.

`proof` measures before any of them. The benchmark fails above 2.2 cycles a byte with AVX-512,
which is 10% over the bound.

### What stands in the way, in the order to remove it once `proof` measures it

The estimate's error is larger than the gap between items, so the first measurement decides
the order. Anything above 1.4 cycles a byte on Turin (90% of the bound) is a bug against it.

1. **Done; found in the code by speed-sol2-format, 2026-10-30 (see the section above).**
   **Per-token work that belongs once a block, about 17 instructions a token.** This is
   `push`'s bookkeeping, the per-token tests and the caller's tests. Fix: the block form
   above. Add the fifth bitmap (special starts) to the three vector kernels and the scalar
   reference, using simdjson's nibble-table lookup. Then, per block, walk the start bits that
   come before the first special start in a loop with no test but the loop's own. Write the
   packed tokens into a local buffer, and extend the column and add the checkpoints once. A
   special start drops to `scan::token` as today.
   - The long-token test moves to the block: a token longer than 4 MiB spans a block with no
     start.
   - The byte-after test goes away, because every byte that can continue a run (`#`, `"`,
     `'`, 0x80 and up) is itself a special start.

   Owner: format-lexer. What stands in the way: it rewrites the vector kernels, and the lanes
   are not building yet (common.md). The kernels must first pass the every-path-against-scalar
   test on AVX-512, AVX2 and NEON hardware, so this lands right after the first build. It is
   not stacked blind on top of the cursor change.
2. **Done; found in the code by speed-sol2-format, 2026-10-30: `scan::number` and
   `lifetime_or_char`.** **The tokens still lexed by `rustc_lexer`, about 1% of tokens: about
   2.5 instructions a token on average.** Escaped strings are now in `scan` (2026-10-29). Suffixed and floating
   numbers and chars are what is left. Fix: a scalar scan of the fraction, the exponent and
   the suffix in `scan::special`, and a char's one code point or escape. Owner: format-lexer,
   with the first measurement of the mix.
3. **The keyword test. Done (2026-10-29).** format-grammar replaced the perfect hash, which
   was about 60 instructions an identifier, not the 15 this section first assumed. The new
   probe reads 8 bytes in place, and `ident_kind` now calls `from_keyword_in` on the file's
   text, at about 19 instructions. The bound above already counts it.
4. **Bounds checks, about 4 instructions a token.** The classes' sentinel bits already stop
   every search at the end. Removing the remaining `bytes[at]` and `bytes.get(end)` checks
   would need an unsafe read, so it waits for a measurement that shows it matters
   (docs/v2/plan/design/unsafe.md: unsafe only for measured speed). The block form removes
   most of them anyway, because a start bit is in bounds by construction.

With 1 and 2 done, the estimate is about 19.2 instructions a token: the bound's 14.7 plus
about 4.5 for the slice, address arithmetic and bounds checks in the inner loop. Adding the
block's cost and classification, that is 6.3 a byte, about 1.6 cycles a byte, or 80% of the
bound. With 4 as well, the estimate is about 15.2 a token and 5.2 a byte, about 1.3 cycles a
byte, which is within 10% of the bound.

### `break 'a: loop {}` (`grammar/expressions/atom.rs`)

This adds one lookahead compare per `break`, which is free. The labeled value is parsed once
by the shared `labeled_expr`.

## The grammar: tokens to events (`crates/parser/src/grammar/`) (format-grammar)

\(t\) is a non-trivia token: the grammar never sees trivia. In real source, at about 3.5 bytes
a token and 60% of tokens non-trivia, there is one \(t\) per 5.8 bytes. The parser's test
trees (`test_data/parser/{ok,inline/ok}`, counted 2026-10-29) have 11,413 nodes for 10,855
\(t\): **1.05 nodes a token**. Those files are denser than real code, at 3.0 bytes a \(t\).

### The work, and the bound

The output is an 8-byte `Event`: one `Token` per \(t\), and a `Start` and a `Finish` per node.
- **Bytes.** The grammar reads the input word of each token it looks at, 4 bytes, and writes
  \(8 + 1.05 \times 16 \approx 25\) bytes of events per \(t\). With the `Vec`'s doubling
  copies, that is about 35 bytes, or about 1 cycle per \(t\) at L2's 32 bytes a cycle. Memory
  is not the bound.
- **Instructions.** The least an LL(3) parser can do per \(t\):
  - for the token, about 7: load its kind, dispatch on it (2), push the event (3), and advance;
  - for its 1.05 nodes, about 8 each: push the `Start` (3), then write the kind into it and
    push the `Finish` (5);
  - about 2 more kind tests (2 instructions each) where one dispatch does not decide.

  That is about **20 instructions per \(t\)**.
- **Branches.** The dispatch depends on the data. Even a parser whose branches predict well
  mispredicts about once in 8 tokens, at about 15 cycles each: about 2 cycles per \(t\). This
  is an estimate, which `proof` measures first.
- **The bound.** 20 instructions at IPC 2, plus the mispredicts, is **about 12 cycles, 3.4
  ns, per \(t\)**. That is about 2.1 cycles a byte, or about 1.7 GB/s a core. A 100 KB file
  is about 17,000 \(t\), so **about 60 µs**. Files parse in parallel, one per task, so a cold
  build is bound by total \(t\) over cores.

### Where the design is (estimated, ±50%, nothing measured)

The grammar is rust-analyzer's recursive descent: one call and several kind tests per
production. A kind test (`p.at`, `p.nth`, `p.current`) costs about 12 instructions, not 2:
- `Parser::nth`'s step counter, a `Cell` load, compare, branch and store (5);
- `Input::kind`'s `words.get` bounds check (2);
- `SyntaxKind::from`'s range assert (2);
- the mask and the compare (3).

The `Cell` store between two tests also stops the compiler from reusing the kind it just
loaded, so a chain of `p.at(X) || p.at(Y)` reloads it every time. With about 8 kind tests per
\(t\), that is about 95 instructions. Add about 15 for the token and its nodes, and about 8 per
node for the `Marker`'s `DropBomb`: building a `Cow` message, defusing it, and testing it on
drop. In all, about **120 instructions per \(t\), about 5× the bound**.

No valid input allocates. Every `format!`, `String` and `to_owned` in `grammar/` is on an
error path (checked 2026-10-29), including the new `PolarityAndModifiers` message.

### What stands in the way, in order

1. **The kind test's overhead, about 10 of its 12 instructions** (`parser.rs`, `input.rs`; not
   format-grammar's, filed in `v2/requests/format.md`, "format-grammar: the parser's speed of
   light"):
   - Count steps where a stuck loop must pass (`start`, `error`, `err_recover`, the
     `expect` failures), not in `nth`. Every grammar loop that fails to consume either starts
     a marker or reports an error, so the guard still catches it; a fuzz target over the
     grammar checks that.
   - Pad `Input`'s words with 4 `EOF` words, so a kind read at `pos + 3` needs no `get`.
   - Have `Input` hold kinds that are valid by construction, so `kind()` needs no range
     assert.

   Then a kind test is a load and a compare, and adjacent tests share the load. That leaves
   about 8 tests × 3 + 15 + 8 ≈ 47 instructions per \(t\), about 2.3× the bound.
2. **The `DropBomb` per marker, about 8 instructions a node.** A `Marker` of a `u32` and a
   `bool`, with its own `Drop` that asserts the bool, does the same check in one byte
   (`parser.rs`, same request).
3. **The production structure.** Most of what remains is calls and tests that one table-driven
   dispatch per token would merge. That is the postorder rewrite of `grammar/` that
   [speed-syntax.md](speed-syntax.md) describes, which also removes `event::process`. Do it
   after `proof` measures where the remaining cycles go: the ±50% on the estimate is larger
   than the gap between this item's options.

Applied 2026-10-29 in format-grammar's files:
- `paths::path_for_qualifier` read `p.nth(2)` (a full kind test) for every path segment before
  testing for `::`. It now tests `::` first, which saves about 12 instructions per path, about
  3 per \(t\).
- The new productions (`GuardPat`, `&pin`, the range ban, `!` bounds, fn-pointer params,
  `unsafe mod`, `cfg` literals) add at most one kind test where they are possible, and none
  elsewhere: `ban_pat_range_if_ambiguous` reads the operator `pattern_single_r` already had.

Applied 2026-10-29 in `parser.rs` and `input.rs` (requests-parser, items 1 and 2 of
"What stands in the way"), not measured. Nothing is built until every lane is done.
- **Step counter.** It is a plain `u32` in `Parser`, checked in `start` and `error` (which
  every failed `expect` and every path of `err_recover` go through) and in the `EOF` branches
  of `bump_any` and `bump_remap`, which consume nothing. The limit and the message are the
  same (`PARSER_STEP_LIMIT`, "the parser seems stuck"), and it is reset on every consumed token.
  A kind test no longer touches it, which saves 5 instructions a test. Because nothing stores
  between two tests, `p.at(X) || p.at(Y)` loads the word once.
- **Pad and valid kinds.** Every `Input`, and every `parser::Tokens` view (`Tokens::new`
  checks this), ends in `Tokens::PAD` = 4 words of an `EOF` token, and `len()` leaves them out.
  `kind`, `contextual_kind` and `is_joint` at `pos + n` are a load from the slice for
  `pos ≤ len` and `n ≤ 3`. A composite token reads `pos + n + 2` only after `pos + n` and
  `pos + n + 1` matched real kinds, so it never reads past the pad. A kind is the word's 10
  low bits transmuted, with no range assert. That is sound because `Input`'s words come only
  from `word(SyntaxKind, …)` plus flag bits above both kind fields, and `Tokens::new` checks
  both fields of every word once, a branch-free pass at memory bandwidth (about 0.1 cycle a
  word). This removes the `get` and its `EOF` select (about 1 instruction) and the assert (2).
- **Result.** A kind test is about 5 instructions: add the offset, the slice's bounds check
  (a compare and a never-taken branch, one fused µop), load, mask, compare. Before, it was
  about 12. With about 8 tests per \(t\) and the shared loads, that is about 8 × 4.5 + 15 ≈
  **51 instructions per \(t\), about 2.5× the bound** (it was about 5×). The bounds check is
  the 0.2× between this and the 2.3× projected above. Removing it needs `get_unchecked`, a
  release check that `pos ≤ len` on every bump (one compare per token instead of one per
  test), and `n ≤ 3` asserted in `nth_at`. That is about 7 µops per \(t\), for `proof` to
  weigh once it has measured.
- **Marker** (item 2 above): `Marker` is a `u32` and a `bool` with its own `Drop` that asserts
  it, with the same message. Completing or abandoning a marker stores `true`, and the
  compiler folds the drop's test away, so a node pays nothing for it. That is about 8
  instructions a node less, which is why the 51 has no term for it. `drop_bomb` is no longer
  a dependency.
- **Termination.** `tests.rs` `termination::every_entry_point_finishes_on_every_input` runs
  every `TopEntryPoint` (full and as an outline), every `PrefixEntryPoint` and
  `Reparser::top_level_item` over 2,000 token sequences (`BLAZE_FUZZ_RUNS` changes the
  count), with a deadline. The sequences are windows of the parser's sources, changed as
  `blaze-fuzz` changes bytes, and made-up sequences. It is a property test, not a fuzz
  target: `fuzz/Cargo.toml` was being edited by another lane, and `blaze-fuzz`'s global
  allocator can't share a test binary with the parser's counting one.

## Keyword lookup (`crates/parser/src/syntax_kind/keywords.rs`) (format-grammar)

The lexer looks up every identifier, about 30% of tokens (format-lexer's section, item 3).
- **Work:** read the identifier, at most 8 bytes for a strict keyword, probe one table entry,
  and compare it.
- **Bound:** about **12 instructions**: a length filter (2), one 8-byte load and a mask (3),
  a multiply-shift hash (3), two loads (2), and the compares (2).
- **Before (2026-10-29):** `blaze_phf`, a byte loop over the identifier's tail, three 64×64
  multiplies to fold it, a bucket and a pilot load, and a byte-by-byte compare. About 60
  instructions for a 5-byte identifier, with a branch per byte.
- **Now:** the key is the identifier as a zero-padded little-endian word, and a multiplier
  found at compile time sends every strict keyword to its own slot among 256. A `const`
  search finds it and fails the build if there is none, so no runtime path handles a missing
  one. A lookup is then one slot load, one entry load, and the window, length and edition
  compares, with no data-dependent branch.
  - `SyntaxKind::from_keyword_in(text, start, len, edition)` reads the window with one 8-byte
    load from the file text, about **19 instructions**.
  - `from_keyword(&str)` cannot read past the identifier, so it ORs four 2-byte loads at
    offsets set by the length, about **24 instructions**.
  - `tables_give_what_the_match_gave` checks both against the old `match`, for every keyword
    and every edition, with variants of every keyword: each byte flipped, each byte doubled,
    the whole doubled, a prefix, a suffix, upper case, and the identifier mid-text and at the
    end of the text.
- **What stands in the way of the last 7 instructions:**
  - The lexer calls `from_keyword_in` (format-lexer, 2026-10-29); it was requested of
    format-lexer (`lexed_str.rs:559`).
  - The slot holds an index into the entries, which costs one load and one bounds check.
    Entries stored in the slots directly (4 KB instead of about 1 KB) remove both, at the cost
    of more L1 footprint. Whether that pays is for `proof` to measure.
- Contextual keywords (up to 15 bytes, looked up only by the parser's callers) keep
  `blaze_phf`.

## Interning a symbol: `crates/intern` (speed-sol2-format)

Counted by instructions from the code on 2026-10-30, ±50%, nothing measured. Layouts and their
tests are in [speed-span.md](speed-span.md) and
[speed-unsafe-front-intern.md](speed-unsafe-front-intern.md). The lexer hashes names while their
bytes are in registers ([simd-lexer, "Hashing names for the interner"](../simd-lexer/speed-of-light.md#hashing-names-for-the-interner-lexedstrnew_hashed)),
and the bridge passes that hash on (`syntax-bridge` `intern_name`), so "hash given" is the
usual case. Not mine and left alone: the compiled-in name sets as enums (fix-names) and the
symbol order keys (fix-orderkeys).

- **The work:** given the hash, find the one entry whose bytes equal the name; on a miss, take
  an ID and publish the bytes.
- **The bound:** one probe of one index: a group load, a 7-tag SWAR match, the entry's load and
  a compare of a short name. That is about 30 instructions and 3 dependent L1 or L2 loads.
  `name_hash` itself is about 6 a word, when the caller hasn't hashed already.
- **Today** (`Symbol::intern_hashed`, `Table::intern_hashed`, `arena::Store::find`):
  - A compiled-in name takes about 28: the perfect hash `symbols::COMPILED` (fold, pilot load,
    slot load, key compare). Within 10%.
  - A name made at run time misses the perfect hash first (about 18). In a fresh process it
    then finds the empty image index (3) and probes the arena (about 35): about 56, **1.9×**.
  - In a process restored from an image, a name in the image probes the image index (about 40
    through `get`'s range and liveness checks): about 58, **1.9×**. A name made since the
    restore misses that index too (about 20) before the arena: about 75, **2.5×**.
  - A miss inserts under the shard's lock (an uncontended acquire, a bump, a copy, a publish):
    about 1.3× a lock-free claim. As [speed-span.md](speed-span.md) says, that is worth changing
    only if `perf c2c` shows shard locks.
- **The gap:** three structures, each probed in turn: the compiled-in set, then the image's
  index read in place, then the run-time arena. The image index already holds the compiled-in
  values (`Table::validate` refuses one without them), so in a restored process the perfect hash
  in front pays only when most names are compiled in. Nobody has measured that share.
- **What stands in the way:** `symbol.rs`'s neighbours (`symbols.rs`, `lib.rs`) are being edited
  as this is written, and choosing an order needs that measurement. Queue 00934 asks for one
  index answering all three (or a tag per perfect-hash slot, so a miss costs about 10), with a
  model test against today's three over seeded names, fresh and restored.
- **What `proof` should measure:** intern hits per second for compiled-in and run-time names,
  fresh and restored, and the share of each in a real build's interning.

### Reading a symbol's text (`Symbol::as_str`)

- **Bound:** one load of a (pointer, length) by ID, about 5 instructions.
- **Today:** a compiled-in symbol costs about 5 (a range check, then `TEXTS[id - 1]`), at its
  bound. A run-time one costs about 20: the table's `OnceLock`, `Table::get`'s three ranges (the
  compiled-in IDs, the image's, the arena's) and then `Records::get`'s ID column and header.
  That is **4×**, though it is rarely on a hot path: `cmp_order` compares order keys first and
  reads text only on a tie. Queue 00934 asks for a text column by ID as well.

## Span maps and ast id maps: `crates/span` (speed-sol2-format)

The layouts are [speed-span-ast.md](speed-span-ast.md)'s, checked against the code on 2026-10-30.
Queue 00019 is done: every item is in `map.rs`.

- **`SpanMap::span_at_from` and `RealSpanMap::span_for_range_from`** (one per token mapped back,
  one per token of a macro input). The bound for an in-order walk is a compare with the hint's
  end and one load of a 20-byte span. Galloping from the caller's hint makes it two compares
  while tokens stay in one piece or move to the next: within 10%. A random lookup is a
  branch-free binary search over 4-byte ends, 16 to a line, again within 10% for maps that fit
  in L1.
- **Building an expansion's map:** two pushes a token into columns sized once
  (`with_capacity(parser_input.len())`), at the bound. `merge` is `O(n)` with no sort.
- **Mapping down:** a binary search of the lazy `by_anchor` permutation and then the run, where
  the bound is the same search. Within 10% once the permutation is built: `O(n log n)` once a
  map, and only on the IDE's path.
- **Ast id maps** (`ast_id.rs`): node to ID and ID to node are one multiply and two lines, the
  bound for content-derived IDs. Building reads each item's name through an `ast` cursor node,
  where the bound reads the flat arrays directly: not counted here, because
  mem-format-expand-next1 holds the file. [speed-span-ast.md](speed-span-ast.md) gives what a
  direct read would have to reproduce (`Impl::self_ty`, `trait_`, `MacroCall::path`).
- **`SyntaxContext`:** one salsa interned read, which is db-salsa's.

## Token trees: `crates/tt` (speed-sol2-format)

The layout is [speed-span-tt.md](speed-span-tt.md)'s: 8-byte nodes, 8-byte packed spans,
canonical tables.

- **Reading:** `TtIter::next` loads a node, dispatches on its two tag bits and loads a span and
  its part. That is about 15 instructions against a bound of about 12: within the count's error.
  Skipping a subtree is one add.
- **Building** (`finish`), per leaf: a node push, a span packed against the last-part cache
  (one compare, usually a hit) and a push. A punct takes about 25, at its bound.
  - **An ident or literal** also looks for its symbol among the last 16 (`Symbols::index`). That
    was an early-exit scan from the newest entry: a repeat close by took about 6, but a symbol
    new to the window scanned all 16, about 48. The ident took about 70 against a bound of about
    30, **2.3×** for a tree of mostly new names.
  - **Now** a full window is one 16-lane `std::simd` compare to a mask, and the highest set bit
    is the index. That costs about 6 whether the symbol is found or not, so an ident takes about
    35, within the count's error of its bound.
  - A window never holds one ID twice, since an ID is added only when the window lacks it, so
    the mask has at most one bit set and the numbering is exactly the scan's.
  - Test: `the_symbol_window_numbers_as_its_scan_did`, 40 seeds against the old scan.
- **A literal's suffix** (queue 00023): it is a `u32` in `Literal` and in the wire encoding. A
  node holds up to `MAX_SUFFIX`, 2^18 − 1 bytes, so 256 or more is fine. A suffix of 256 KiB or
  more still stops at `Node::literal`'s assert, which names the limit. The random tree tests now
  make suffixes of 256 to 555 bytes.
- **What `proof` should measure:** `finish` per leaf over the bridge's corpus, before and after
  the window compare.

## Syntax to token trees and back: `crates/syntax-bridge` (speed-sol2-format)

- **Syntax to tree** (`syntax_node_to_token_tree*`), per token: an in-order step of the flat
  tree (about 10; queue 00021 is done), `span_for` through the hint (about 6), interning an ident
  with the lexer's hash where the tree kept it, and `tt`'s push (about 25 to 35). That is about
  70 to 100 a token. The only part over its bound is interning, whose gap is the interner's
  (queue 00934).
- **Tree to syntax** (`token_tree_to_syntax_node`), per token:
  - `to_parser_input` (about 15, with a last-context cache before its map; read in place when
    the tree keeps its words);
  - the parse;
  - `TtTreeSink`: about 15 for text and span-map pushes into columns sized once, plus the tree
    builder.
  - The parse and the builder's gaps are the parse driver's and the flat tree's (queue 00927).
    The bridge's own work is within 10%.

## `macro_rules!`: `crates/mbe` (speed-sol2-format)

The layouts and their differential tests are [speed-span-mbe.md](speed-span-mbe.md)'s.

- **The matcher,** per step and position: the token is decoded once a step; a fork is a 24-byte
  copy; binding pushes a 20-byte cell. The bound is about the same: rustc's matcher copies 16
  bytes and bumps an `Rc`. Within 10%.
- **A fragment** (`expect_fragment`):
  - The parse reads a window of top-level trees, doubled until the parse provably read nothing
    past it. That is at most twice the final window, and the first window (8 trees) holds most
    fragments.
  - The input words are read in place when the tree keeps them. Otherwise they are copied into
    the thread's pooled `Input`, which allocates nothing when warm.
  - The fragment's cost is the parser's (queue 00927).
- **The transcriber:** a binding is one index into flat columns, and output goes straight into
  a builder sized for the template. That is `tt`'s push per token, at its bound.
- **What `proof` should measure:** `benchmark.rs`'s real-world macros, expansion time per output
  token.

## `const-ast` (speed-sol2-format)

`Tree` borrows four arrays, and a view is two words. A step skips children by their subtree end,
and nothing is copied per element: at its bound, nothing to change.

## Syntax validation (`crates/syntax/src/validation.rs`) (format-grammar)

- **Bound:** every node's kind read once, and one dispatch to the check for it, if it has
  one: about 4 instructions a node, plus what the checks do on the nodes they are for. The
  tree walk is syntax-tree's, so it is outside this bound.
- **Before:** 15 `can_cast` tests in a row on every node. Most nodes have no check, so they
  took all 15, about 30 instructions a node.
- **Now (2026-10-29):** one `match` on the kind, a jump table, at the bound. Each arm's type
  is a single kind; a `debug_assert` checks the kind against `AstNode::kind`.
- **What stands in the way:** the walk builds a `SyntaxNode` handle for every node, and a
  check casts with a cursor clone. Walking `(FlatTree, index)` directly is speed-syntax's
  typed views (`v2/requests/format.md`).
- **Corrected (speed-sol2-format, 2026-10-30, from the code):** that is done. `validate` and
  `validate_outline` walk `view::Node<'_>`, the tree and a number, with no handle a node, so
  the walk is the step bound of the flat tree's section. `validate_outline` compares a range
  only for a `BLOCK_EXPR`, after its kind. Within 10%.

## The parse driver: input, events and the sink (speed-sol2-format)

The passes from a file's tokens to the tree builder are `input.rs`, `event.rs`, `output.rs`,
`shortcuts.rs` and `outline.rs`. \(T\) is every token, trivia included, and \(t\) a non-trivia
token, as above. Counts are by instruction, ±50%, nothing measured.

### Building the input: `LexedStr::to_input` (`shortcuts.rs`)

Work: for each token, read its word, skip it if it is trivia, and write a 4-byte input word.
An identifier also needs its contextual keyword.
- **Bound:** about 6 instructions a token, plus about 15 an identifier with a window probe like
  the strict keywords'. That is about 10.5 a \(T\).
- **Before (from the code):** every token paid for `kind(i)` and `token_len(i)` (two index
  checks and the kind's range assert) and a `&str` slice of its text with its char-boundary
  checks, about 20 instructions. An identifier added about 50 for `keywords::contextual`'s
  `blaze_phf` probe. That is about 35 a \(T\), **3.3×**.
- **Now:** one pass over the token column (`LexedStr::kinds_and_lens`). Only an identifier's
  text is sliced, and a float's last byte read, so a token is about 12 and the average about
  27, **2.6×**. Test: `the_input_in_one_pass_is_the_input_token_by_token`, against the
  token-by-token walk in every edition.
- **What stands in the way:** the contextual probe is fix-names' (`keywords.rs`; queue
  **00926**). With a window probe it is about 15 a \(T\), 1.4×. What is left after that is
  `Input::push`'s capacity check a word, which `with_capacity` makes always predicted.

### Events to `Output`, and `Output` to the sink (`event::process`, `intersperse_trivia_outline`)

Work the output needs: none, beyond writing the tree's words, which is
[syntax-tree's bound](#the-flat-tree-syntax-tree-speed-sol2-format).
These are two passes over the parse that exist because the grammar writes preorder events with
forward parents.
- **`event::process`** is about 10 instructions an event (the load, the variant's dispatch, the
  `TOMBSTONE` test and a `Vec::push`), at 3.1 events per \(t\): about 31 per \(t\), 8 cycles
  at IPC 4.
- **`intersperse_trivia_outline`** decodes each word (about 6) and steps the builder's state
  (about 4). For each trivia token it reads its kind and length and slices its text, and it
  calls the sink once a step. An item also scans its leading trivia for doc comments to attach
  (`n_attached_trivias`).

Together they make the parse from tokens to tree about **3×** the grammar's bound, beside the
grammar's own 2.5×. **What stands in the way:** the grammar's postorder rewrite in
[speed-syntax.md](speed-syntax.md), which writes the builder's words directly and patches
forward parents in place. It rewrites `grammar/` (about 5,400 lines) and the sinks in `syntax`
and `syntax-tree`, so `proof` measures first; queue **00927** (lane format). It is not done
here: the estimate's ±50% is larger than the gap between its designs, and nothing can run.

### Outlines: `TopEntryPoint::parse_outline` and `lazy_block_len` (`outline.rs`, frontend-deps')

An outline parses items in full and skips each `fn` body after one scan of its tokens, for
balance and the few error recoveries that take a brace. Bodies are most of a dependency
crate's tokens.
- **Bound:** a body token's kind tested against the 9 kinds the scan watches. With 16 kinds a
  vector compare (`std::simd`), that is about 1 instruction a token; scalar it is about 5.
- **Before (from the code):** every token went through the `match`, with the next token's kind
  loaded and the attribute flag swapped, about 13 instructions.
- **Now:** a `WATCHED` table skips a token no arm watches when no attribute is pending, about
  9. The test `skipping_unwatched_tokens_keeps_every_answer` holds it to the old loop on random
  sequences from every start.
- **What stands in the way of the vector scan:** `Tokens` would have to expose its words as a
  slice. That changes `input.rs`, which frontend-tokens shares, and takes the 16-at-a-time
  loop's tests. It is next once `proof` shows outline bodies matter, at about 2× the scalar
  bound now.

### The reparser's parse (`Reparser`)

`Reparser::for_node` and `top_level_item` parse a block's or an item's tokens with the full
grammar and the same driver, so their cost per token is the sections above. The edit's bound,
lexing and parsing only the block that changed, is the tree part's
([syntax-tree and syntax](#reparsing-an-edit-syntaxsrcparsingreparsingrs-speed-sol2-format)).

### `rustc_lexer`'s share

`scan::special` decides at least 99% of the special tokens (its test), so `rustc_lexer` lexes
about 0.1% of tokens: errors, edition-split forms and non-ASCII outside strings and comments.
At about 250 instructions each that is about 0.25 a token, about 1% of the lexer. This is
dispatch by token class: both paths give the same tokens, and the differential tests check it.

## The flat tree: `syntax-tree` (speed-sol2-format)

The layout is in `flat.rs`'s module doc. \(e\) is an element (a node or a token); Rust has a
little over half tokens and about one element every two bytes. Counts are by instruction,
±50%, nothing measured.

### Memory per tree

About **6.5 bytes an element**:
- a word an element;
- a parent `u32` a node;
- a node bit and an eighth of a rank `u32`;
- an eighth of a start;
- side tables for the rare long or far values.

A tree keeps no copy of the text. That is the budget [tree-store's
speed of light](../tree-store/speed-of-light.md) and data.md's columns are held to, and the
least for O(1) kind, parent, sibling and subtree size with arithmetic ranges. Within 10%.

### Building: `FlatTreeBuilder` (`start_node`, `token`, `lazy`, `finish_node`, `finish`)

- **Bound:** a word an element with its fields packed (about 6), a start every eighth, a parent
  a node, and the node bits made once at `finish` (`node_bits`, a pass at L1 speed). That is
  about 8 instructions an \(e\).
- **The code:** about 38 instructions a token.
  - `FlatTreeBuilder::push`: the mark test, the open parent and the tag `match`.
  - `token`'s three asserts and its source check (a pointer compare when the text is the
    source's own slice).
  - `Packer::start`'s size assert and far-kind test.
  - `leaf`'s long and far tests, and the `Vec` push.

  A node costs about 45 over `start_node` and `finish_node`: two pushes and two pops, the size
  and the `keeps` test. **About 4.5× by count.** A warm build allocates nothing beyond the
  tree's block (the thread's `Buffers`).
- **What stands in the way:** most of it is the checks that make a tree valid whatever its
  caller sends, and they are cheap only when merged with the caller's. The builder's caller
  is the parse's sink, so the fix is the postorder parse of queue **00927**. It writes these
  words from the grammar, with the open stack and the starts as the grammar's own, so the
  per-call asserts become one check per production. Tightening `token` alone (one open-stack
  read for its assert and its parent) saves about 5 of the 38. That waits for 00927's shape.

### A step: kind, parent, siblings, children, range, text (`flat.rs`, `view.rs`, `cursor.rs`)

- **Kind and subtree end** are one load and a mask (plus a cold side-table search for the rare
  long values).
- **A token's parent** is a subtraction. A node's is a rank (one load and a popcount) and a
  load.
- **First child** is `at + 1` against the end. **Next sibling** is the end against the parent's
  end.
- **The range** is a mark and at most 7 lengths for the start, and the same again for a node's
  end. **The text** is that slice of the shared source.

Each is at its bound for this layout, within 10%:
- `view::Node<'t>` is the tree and a number, so a step is the arithmetic above.
- The handle API (`cursor::SyntaxNode`, `SyntaxToken`) adds a non-atomic count, increased on
  clone and decreased on drop, and one pointer read to its root. That is about 3 instructions a
  handle, and nothing is allocated.
- A lazy node's part is parsed once when a cursor first enters it, by whichever thread comes
  first (`OnceLock`).

The step that is not O(1) is `prev_sibling` and `last_child`: `child_containing` walks up and
across at once, which takes the fewer of the depth and the children before it.

### `subtree_hash`

- **Bound:** BLAKE3 over the subtree's text and its words, once a subtree, at BLAKE3's rate.
- **The code:** that. A node over 4 KiB of text and words (`KEEP`) keeps its hash in a slot,
  and an edit carries the slots of the subtrees it does not change (`kept_spliced`), so a
  hash after an edit hashes only the changed subtrees. Smaller nodes hash again when asked,
  at most four chunks, about 2 µs. Within 10%.

### `ast` casts and accessors (`crates/syntax/src/ast`)

A cast is one kind compare. An accessor (`ast::support::child`) walks the node's children to
the first of a kind, the least without a per-kind index. The views
(`syntax::view`) are the same walks on `(tree, number)` without a handle, at the step bound
above.

### Reparsing an edit (`syntax/src/parsing/reparsing.rs`) (speed-sol2-format)

- **Bound:** lex and parse only the smallest piece that parses alone (a token, a body, a block
  or an item), plus one copy of the tree's arrays with the piece's swapped in. The arrays must
  be copied because a tree is immutable, at 6.5 bytes an element at memory bandwidth: for a
  100 KB file about 260 KB, about 2.5 µs from L2.
  [Frontend's speed of light](../frontend/speed-of-light.md) has the same bound.
- **The code:** `FlatTree::splice_all` writes the new block in one pass.
  - The words, node parents and node bits between the splices are copied by range (the parents
    shifted by the elements added before them).
  - The side tables are merged in order.
  - The ancestors' sizes and the far parents a splice moves are patched after.

  Each column is a pass at about memory speed, estimated within 1.5× of one `memcpy` of the
  tree. Several edits at once (`reparse_all`) make one copy.
- **A full parse** happens when no piece passes the checks. How often that is on real edits is
  for `proof` to count: nothing counts it yet. It is the answer the incremental reparse is
  checked against, not a slower path to the same output.
- **What stands in the way of O(piece):** a tree in chunks (a rope of blocks), so an edit
  copies one chunk and a spine. That is a layout change for tree-store's images and every
  reader of `FlatTree`'s columns, against about 2.5 µs an edit now. It is not worth it until
  `proof` shows edits of large files bound by the copy.

### Tree blocks on huge pages (queue 00810)

Each tree is one block from the global allocator (`flat/block.rs`), on base pages. A huge-page
slab (size classes over `blaze_mem::Region::reserve_huge`, blocks returned to it when an edit
replaces a tree) would cut the TLB misses of walks over many trees. `block.rs` holds tree-store's
held mode, and tree-store is IN, so the slab waits for it. Queue 00810 stays open for lane
format, with this note. A walk of one tree is sequential, so it gains little; the gain is for
analyses that touch thousands of trees, which `proof` measures.

## The item tree: `hir-def` `item_tree*` (speed-hir-sol-items)

Layout and history: [speed-hir-items.md](speed-hir-items.md) (9 bytes an item plus a payload
record, a reused per-worker builder). The files are `frontend-deps`' while it builds dependency
images, so every change below is an exact edit in `v2/requests/frontend-deps.md`
("speed-hir-sol-items"), A to F. The file is frontend-deps-reparse's example
([frontend/speed-of-light.md](../frontend/speed-of-light.md), "frontend-deps-reparse"): \(I\) =
500 top-level items, 50 KB of text, about 10 KB of it signatures (bodies left out), \(A\) =
1 000 attributes, most of them doc comments, which the item tree skips. Cycle counts are
estimates counted from the code; nothing has been run.

### Lowering a file's outline (`lower_whole`, a warm builder)

Work: one row per item, from its signature's tokens. Bodies are never entered: a lazy `fn` body
is only tested for a leading `#!`.
- **Bytes.** Lowering reads about 10 KB of signatures and writes about 25 bytes an item (the
  9-byte row, its payload and its name), about 12 KB in all. At L2's 32 bytes a cycle that is
  about 700 cycles for the file, so bytes are not the bound.
- **Instructions, per item, at the minimum:**
  - about 15 to dispatch and find the name and visibility among the item's children;
  - about 30 to intern the name (hash about 8 bytes, probe one shard; the interner takes no
    lock on a hit);
  - about 25 for the ast id (one probe of `AstIdMap`);
  - about 15 to deduplicate the name within the item, and about 6 to write the row and payload;
  - about 10 to find that the item has no attributes.

  That is about 100 instructions, at IPC 2 about **50 cycles, 14 ns an item**. Each attribute
  the tree keeps adds a path interned (about 40 cycles); a skipped doc comment adds about 10.
- **Fixed cost per file:** one exact allocation per non-empty column at `finish`, at most 31
  (30 columns and, with edit C, the attribute index), about 30 ns each: about 1 µs.
- **The bound:** \(50I\) cycles + \(10A\) + 1 µs, so **about 11 µs** for the example file.
- **Where the code is.** About 170 cycles an item:
  - about 75 in `SyntaxNode` cursor scans: `name()`, `visibility()` and `attrs_with_doc()`
    each walk the item's children;
  - about 30 in attribute lowering's setup for an item with none (`collect_item_tree_attrs`);
  - about 30 to intern the name and 25 for the ast id;
  - about 10 for the three `Scoped` seals.

  Add, per file, a `radix::sort_copy_by_key` of the attribute owners at `finish`. Above its
  insertion-sort size it allocates a scratch the size of the owners and makes four passes. Yet
  the owners are already in order except the top level's, which is pushed first. In all that is
  about 100 000 cycles, **29 µs, about 38% of the bound**.
- **What stands in the way, and the exact change:**
  1. *The sort* (edit A): hold the top level's owner aside and push it last at `finish`. Every
     other owner is pushed right after its item's id is made, so they arrive in order, and an
     `assert!` in `add_attrs` checks it with one compare a push. This removes the allocation
     and the passes.
  2. *The scope table after a big item* (edit B): `Scoped::seal` drains an `FxIndexSet`, and a
     full drain clears the hash table, which writes every control byte at the table's
     high-water size. After one inline module with 1 000 names, every later sealed item on the
     worker writes about 1 KiB: about 32 cycles, about 20% of an item. With edit B, a table
     much larger than its entries is emptied one entry at a time instead. That is O(entries),
     with no allocation, and the capacity is kept.
  3. *The cursor scans* are the largest term. With frontend.md item 16's index views
     (frontend-tree) they are about 15 cycles. *The ast id* could be read by element index
     instead of a hash probe (span's `AstIdMap`, about 20 cycles).
  4. *Attribute setup* for an item with none: a first-child test, as the expression store's
     `may_have_attrs` does, saves about 25 cycles. It stays out of this round: inner attributes
     anywhere in a module's or an impl's list are collected today, and a first-child test must
     keep that.

  With 1 and 2 the code is about 160 cycles an item (40%). With 3 and 4, about 70 (70%). The
  remaining gap to 90% is interning: a per-thread front cache of recent names, which is the
  store section's item 3.

### A memo hit and the accessors

- **Memo hit** (`file_item_tree`, `block_item_tree_query`): salsa's lookup through the concrete
  `&Db`, a few loads and a revision compare, and no allocation (asserted:
  `items/tests.rs` `trees_asked_again_allocate_nothing`). Its cost is salsa's, lane db's.
- **Bound per accessor:** the bytes it returns, one or two loads from columns that a walk
  in item order streams through L1.
- **Where the code is.**
  - At the bound: `top_level_items`, `items`, `mod_kind`, `value_ns_ctor`, `use_tree` and
    `UseTree::kind`, `get_name`, `Index<NameId | PathId | RawVisibilityId>` and
    `import_alias`. Each is one or two bounds-checked loads, and a `Name` or `ModPath` comes back
    by reference or as a copy.
  - `index(id)`: three loads (kind, data word, ast id) and the payload, about 6 to 8 cycles.
    Packing the kind into the data word's top byte (data < 2^24 per kind) would make it 8 bytes
    and two loads, about 1 cycle less. That changes the persisted schema; it is listed for
    frontend-deps to weigh, not filed.
  - **`attrs(id)` is a binary search** over `attr_owners`: about 9 dependent probes for 500
    owners, with a mispredict on about half, so about 100 cycles. The def map collector asks it
    for every item: \(100I\) = 50 000 cycles, 14 µs a file, more than lowering the file's
    outline costs at its bound. **Edit C** makes it O(1) with a rank index in the style of
    the flat tree's node bits: per 64 items, one `u64` of which have attributes and the count
    before them (`AttrBlock`, 16 bytes per 64 items, 0.25 bytes an item, against the 4 bytes a
    per-item column would cost). The lookup is one load, a mask, a `popcnt` and the owner's
    load: about 10 cycles. The index is built from `attr_owners` at `finish`, at `assemble`
    and at `decode`, so the persisted bytes don't change.
  - `Use::expand`: for each leaf under a prefix, the joined path is interned, a `SmallVec`
    copy and one probe, about 40 cycles. The callback takes a `ModPath`, so this is the API's
    bound. A warm run allocates nothing (edit D asserts it).
- **Budgets** (edit D, `item_tree/tests.rs`): reading every item through every accessor is 0
  allocations, and `Use::expand` once its paths are interned is 0.

### Encoding and decoding (`persist.rs`)

- **Encode bound:** write the columns once, about 25 bytes an item plus the name table, then
  hash the buffer in the store (BLAKE3 at about 0.6 cycles a byte). That is about 15 KB, so
  about 10 000 cycles, 3 µs, for the example.
- **Encode design.** The columns are pushed as slices, at the bound. Three temporaries
  (kinds as `u8`, ast ids as `u32`, macro calls as records) are copied once more, about 5 bytes
  an item, plus 3 allocations, about 3%. The largest term is in `hir/persist.rs`, which is
  speed-hir-store's: `Words::name` allocates a `Box<str>` key for every distinct name, and both
  `name` and `symbol` look up and then insert, which hashes twice. At about 100 cycles a name
  that was 50 000 cycles, so encode was about 5 times its bound. Now `Words::name` hashes each
  name once into a table of indexes into `text` (`by_hash`, with `same_hash` chaining
  collisions) and allocates nothing a name: about 25 cycles a name, 12 500 cycles, so encode is
  about 10 000 + 12 500 cycles, **about 2.2× its bound** (by count, sol-all 2026-10-30). The rest is the probe per name; names interned once per tree, kept as `u32`s in
  the columns, would remove it.
- **Decode bound:** verify the hash (the store's, 0.6 cycles a byte), check every id (about 1
  cycle a row), copy each column once (0.1 cycles a byte), and intern each distinct name once.
  That is about 1 cycle a byte plus 40 cycles a name, **about 10 µs** for the example.
- **Decode design.**
  - The `Copy` columns are one `memcpy` and one allocation each, and the checks are one pass,
    both at the bound.
  - Off the bound: `kinds` and `macro_calls` are collected into a `Result<Box<[_]>, _>`, whose
    size hint is 0. They grow by doubling and shrink at the end, about \(\log_2 n + 2\)
    allocations each. Edit E collects them at their exact length.
    **Corrected (speed-sol2-format, 2026-10-30): done, and further.** `decode` takes every
    column's length first and builds the tree as one block (`ItemTree::try_build`), pushing
    `kinds` and `macro_calls` into it with their checks. One allocation a tree, not one a
    column.
  - `Read::seq` (`hir/persist.rs`) has the same doubling for the names, paths, visibilities,
    attributes and `cfg`s. `Code for ModPath` reads its segments into a `Vec` first, one
    allocation per path and per attribute path. Both are filed for speed-hir-store: an
    exact-size iterator for `seq`, and a `SmallVec<[Name; 8]>` for the segments.
  - With E and those two changes, decoding allocates once per non-empty column (edit D
    asserts this). A read in place (`ItemTreeRef`, the top level, kinds, ast ids and children)
    is already 0.
- **What stands in the way of 90%:** the name table is still re-interned on every read. Reading
  it in place needs symbols persisted as stable ids: frontend-image's interner, filed by that
  lane.

### Lowering a macro expansion's tree

The same `lower_whole`, over the expansion's parse. Each span lookup is one binary search of
the expansion's span map, about 50 cycles for a visibility path, a macro call or an attribute;
a plain item has none. The bound and the code's distance from it are those of the outline
lowering above, plus that search. The expansion's parse costs more than its tree.

### An edit: one item's rows change

- **Bound.** Re-lowering the file's outline is the bound of the whole-file design: **about
  11 µs** for the example. Re-lowering one item and reusing the others' rows needs, per
  unchanged item, a hash the parse carries (one load and a compare, about 5 cycles) and a copy
  of its rows with ids moved (about 10 cycles). That is \(15I\) = 7 500 cycles + one item's
  lowering (about 0.1 µs), **about 2.2 µs**. Both reuse every downstream memo the same way: the
  tree comes out equal when only bodies changed, and only the edited item's rows differ after
  a signature edit (asserted: `tests.rs` `an_edit_of_one_item_changes_only_its_rows`).
- **Where the code is.** Source files go through the per-item queries
  (`items.rs`: `item_sources`, then `item_fragment` per item, then `assemble`). By
  frontend-deps-reparse's own count, an edit costs:
  - `item_sources` about 120 µs;
  - the changed item about 0.9 µs;
  - `assemble` about 45 µs;
  - plus about 100 cycles an item for salsa to verify each unchanged `item_fragment` memo,
    about 14 µs.

  That is **about 180 µs**: 6 times the whole outline re-lowered by today's code (29 µs), and
  16 times its bound (11 µs).
- **A cold build** pays the same per item without the reuse:
  - `item_sources`, about 850 cycles;
  - a new `item_fragment` memo, about 300;
  - `Ctx::for_item`'s extras, about 600 (a `RealSpanMap` of three allocations, a pairs `Vec`,
    and about 7 exact columns per fragment);
  - the lowering, about 170;
  - `assemble`, about 20.

  That is about 1 950 cycles an item: **about 11 times `lower_whole`**, and about 10 allocations
  an item where `lower_whole` makes 31 a file. For a 100 000-item workspace, that is about 55 ms
  of a core against about 5 ms.
- **What stands in the way, and what it would take.** Per-item salsa granularity has a floor
  of about 250 cycles an item per edit (a tracked struct matched and a memo verified). That
  alone is more than re-lowering the item, about 170 cycles, so the per-item queries can at
  best tie the whole-file lowering. They beat it only once neither the queries nor the hashing
  run per item:
  1. the parse carries each top-level item's subtree hash, which an incremental reparse keeps
     for unchanged items (frontend-deps-reparse's item 4 plan; speed-syntax);
  2. one query per file reuses rows by that hash: its tree keeps a per-top-level-item key
     column, and the last tree of each file is kept beside salsa, keyed by content. The key is
     every input lowering reads, the item hash, the ids, the anchors and the crate's `cfg` set,
     so reuse misses nothing (AGENTS.md's cache-key rule). A cold file, with no last tree, is
     lowered whole.

  That reaches about 2.2 µs an edit and `lower_whole`'s cost cold. Until it is built,
  `lower_whole` for source files is the faster of the two designs on both paths. It is filed
  for frontend-deps to decide (request F), since frontend.md item 14 is its lane's. Edit F also
  carries a small step that holds under either design: `hash_full` feeds the 16-byte
  `subtree_hash` for subtrees of 4 KiB or more, which the flat tree keeps across edits, instead
  of their text. That helps large `macro_rules!` and macro calls.
- **Budgets** (edit D): on a warm builder, lowering a file allocates at most once per non-empty
  column. An attribute with an input allocates its `Box<AttrInput>` too (hir-expand's; shared
  inputs are requested in `v2/requests/db-expand.md`), so the fixture has none.
- **Corrected (speed-sol2-format, 2026-10-30):** edits A to D are in the code. A: no sort of
  the attribute owners (`lower.rs`: they are pushed in order). B: `Scoped::seal` pops a table
  much larger than its entries (`len * 16 < capacity`). C: `AttrBlock`, 16 bytes per 64 items.
  D: `reading_a_tree_allocates_nothing`, the `Use::expand` budget and the warm-builder budget
  in `item_tree/tests.rs`. E is in the decode note above. F stays frontend-deps' decision. So
  the outline lowering is about 160 cycles an item, where the section's "with 1 and 2" puts it.
  What is left is the cursor scans and attribute setup (its items 3 and 4) and interning (queue
  00934).

## hir-def: what the two sections above leave out (speed-sol2-format)

Counted from the code on 2026-10-30, ±50%, nothing measured. When I read them, `lower.rs`,
`path.rs` and `hir/type_ref.rs` had been modified in the last 20 minutes, so I changed nothing in
them. `hir/persist.rs` is frontend-deps-sigs'. I left `expr_store/persist.rs` alone too, as the
brief said item-hash-identity was live on it.

### Signatures: generics, where clauses, type refs (`expr_store/lower/generics.rs`, `hir/type_ref.rs`)

- **The work:** one row per generic parameter, bound, where clause and type ref, from the
  signature's tokens.
- **The bound:** as for a body's node, about 40 instructions a row, with a name interned for
  about one row in two.
- **The layout is at the bound:**
  - a type ref is a 1-byte kind and a 4-byte data word (`TYPE_REF_BYTES`, 12 a line), plus
    `extra` for the few kinds of several words;
  - lists and bounds are CSR columns;
  - paths are `PathColumns`: kind and data per path, `extra`, and rows of 20 bytes for generic
    arguments and bindings, all `const`-asserted.
- **The gap** is the body walk's: a view step a child, and interning. A signature is a few
  dozen rows, so the fixed costs dominate: one salsa query, `finish`'s allocations (one slab
  for the `Copy` columns, plus up to 12 boxes for the path columns: the store section's
  "`finish`"), and hashing the tree (`subtree_hash`, filed in `v2/requests/speed-hir.md`).
- **What `proof` should measure:** `ExpressionStore` lowering for signatures, per row and per
  signature, on a crate with many small items.

### Path columns: reading a path

- A path's kind and data word is two loads. A `Normal` path's segments are a `ModPath` by
  index, and their generic arguments are `slots` into `generic_args` and `args`.
- **The bound:** two loads and then the rows read, which is what the readers do. At the bound.
- What `finish` costs per store is in the store section's "`finish`": up to 12 boxes, because
  lowering reads paths back through the frozen views.

### Source maps (`expr_store/source_map.rs`)

- **Built** only when read (`Body::relative`, `with_source_map`): the body is lowered again
  with the map, then each table is sorted once at `finish`. No hash map anywhere.
- **ID to syntax** (`BackMap::get`): a dense row by ID (a `u16` kind and a range of two
  `u32`s), plus the file, which is one compare when the map has one run (almost every body)
  and a `partition_point` over run starts otherwise. That is about 8 instructions, at the
  bound.
- **Syntax to ID** (`SourceTable::get`): a binary search over `(key, id)` pairs. The bound is
  the same search over a column of keys alone; the pairs make the search's last steps cross
  one cache line more. That is about 1.2× by count, within the count's error, and only on the
  IDE's path. A split into two columns should wait until `proof` shows it matters.
- **Lowering twice for the map** is the store section's "Source maps": IDE-only, within 2×.

### A memo hit

- `Body::of`, `ExprScopes::of` and `file_item_tree` return their memo's value by reference
  (`returns(ref)`): no `Arc` count is written and nothing is cloned. Their cost is salsa's lookup
  through the concrete `&Db`, which is lane db's. Its handle reads are filed as queue 00886.

### An edit: `body_syntax` for every body of the file (queue 00845)

- **The bound:** an edit inside body \(F\) reruns only \(F\)'s queries.
- **Today:** `body_syntax(G)` reads its file's parse, span map and `AstIdMap`, so any edit
  reruns it for every body \(G\) of the file. Each run does `def_syntax`, an anchor lookup,
  two ancestor walks, `covering_element` and the per-anchor maps (built once an anchor and
  shared). That is a few µs a body, about 1 to 2 ms an edit for a file of 500 bodies. It
  backdates, so nothing downstream reruns.
- **What stands in the way:** the fix is a per-file query that gives each item's position-free
  syntax as a field of a tracked struct per item, as the item tree's per-item keys do
  (`item_tree/items.rs`). That changes the query graph. `body.rs` was edited 45 minutes before
  I looked, and the change can't be checked without a build: hir-firewall's
  `an_edit_of_each_function_reaches_only_it` is its test. Queue 00845 stays open for format.

### Reading persisted HIR (`hir/persist.rs`, frontend-deps-sigs')

- `Read::seq` still collects through a `Result` (`(0..len).map(T::get).collect()`), whose size
  hint is 0. So names, paths, visibilities, attributes and `cfg`s grow by doubling: about
  \(\log_2 n + 2\) allocations each, where the bound is one. Every `Box<[T]>`, `Vec`, `ThinVec`
  and `SmallVec` a persisted value holds is read through it (`Code` impls, `hir/persist.rs`
  around :740). `len` is read first, but `seq` is generic over `C: FromIterator`, so the fix is
  a `Vec`-specific path: `Vec::with_capacity(len)` and a push loop, which those four impls use
  (`into_boxed_slice` at exact capacity doesn't reallocate). Queue 00935, for
  frontend-deps-sigs, which holds `hir/persist.rs`.

### Queue settled here

- 00020: done. `SyntaxContext::from_u32` is safe, `id` checks the bound, and no `unsafe` is
  left in the persist files.
- 00570: done. `ScopeEntry::hygiene` is `pub`.
- 00131: refused to db. `TraitItems` and `ImplItems` are in `nameres/assoc.rs`, not format's.
- 00845: open, as above.
