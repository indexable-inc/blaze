---
status: plan
summary: blaze v2: data-oriented layout and compression in every stage.
date: 2026-10-29
---
# Data-oriented everywhere

Struct of arrays is not only for syntax and HIR: every stage's long-lived data is columns of
small integers over `u32` indices, laid out the same in memory and on disk (section 5).

**Encoding rules** (every lane):
- **Tag plus one word**: a node is a `u8` kind in one column and a `u32` data word in another;
  variants that need more put it in a shared `extra: [u32]` array and store its index (Zig's
  encoding). No Rust `enum` with payloads in a hot array.
- **Lists as CSR**: children, arguments, edges and projections are `(start, len)` ranges into
  one flat array per kind, never `Vec<Vec<_>>` or a `Vec` per node.
- **Hot and cold split**: what a pass reads every time sits in the main columns; what it reads
  rarely (spans, docs, attributes, diagnostics data) sits in side tables keyed by the index.
- **Flags as bitsets**: booleans and small sets (type flags such as "has inference variables",
  "has params"; live locals; reached functions) are bit columns, tested 512 at a time with
  AVX-512 or 128 with NEON.
- **Names as symbol IDs**, never `String`; no `Box`, `Arc` or `Option<Box<_>>` in long-lived data.
- **Size budgets**, checked by `const` asserts beside each type: a token 4 bytes, a syntax node
  8, a HIR expression 12, a MIR statement 16, a memo 24 plus 4 per edge, a relocation 16.
- **Batch by kind**: a phase runs over every item of one kind at once (hash every changed
  body in one BLAKE3 batch, lex every file, collect every signature of a crate), and tiny
  per-function tasks are grouped so the scheduler's cost per task stays below the task's work.

**By stage:**

| data | layout | lane |
|---|---|---|
| tokens, syntax, item trees, HIR bodies (exprs, pats, stmts) | tag and data columns, CSR children, spans in a side table | format |
| types | one interned table: kind column, CSR generic arguments, a flags bitset column; a type is a `u32` | db (`vendor/rustc_type_ir`) |
| inference | union-find in flat `parent` and `rank` arrays in the task's arena; obligations in a column queue | db |
| trait solving | goals and results interned as `u32`s; the solver's cache an open-addressed table on them | db (`vendor/rustc_next_trait_solver`) |
| MIR | locals as columns (type, mutability); blocks as ranges into one statement array; statements as tag and data; places as a base plus a CSR projection list | codegen (`hir-ty-mir`) |
| borrow check | facts as columnar relations (Polonius-style); liveness and initialization as dense bitset dataflow per block | db (`hir-ty-borrowck`) |
| query database | memos as columns (key hash, fingerprint, verified-at, value offset); edges as one CSR array per memo table (today salsa's edges are 0.8 GB of nushell's 12.5 GB, #734) | db |
| item hashes and facts | columns per hash kind; facts as `(kind, subject, object)` triples sorted for range scans; negative-fact indexes as sorted arrays | items |
| compiled functions and relocations | code bytes in one buffer per unit; relocations as columns (offset, kind, target), sorted by target so a moved function's callers are one range | codegen, link |
| linker symbols | columns (name, section, offset, size); placement by prefix sums over sizes | link |
| store index | the open-addressed table of section 8, keys and places in separate arrays | store |

`proof-perf` reports bytes per token, node, expression, statement and memo, memory per
thousand lines, and cache misses per edit (`perf stat`), against these budgets.

**Compression follows the columns** (format, store). Columns of one type compress far better
than rows, and zero-copy holds by tier:
- **hot**: plain fixed-width columns, mapped and used in place, each at the narrowest width
  its range allows (`u8`, `u16` or `u32`, recorded in the schema);
- **warm**: bit-packed with a frame of reference (each value as its offset from the column's
  minimum, in as many bits as the range needs), still random-access (element *i* is a shift and
  a mask) and unpacked 128 or more at a time with SIMD, so it is read in place too;
- **cold**: a codec per column: run-length or entropy coding for tag columns, delta plus
  bit-packing for sorted and monotonic columns (CSR offsets, sorted relocations and facts, the
  store's keys), dictionaries for repeated values, then zstd with a dictionary per kind;
  decoded when the page is read in.
The store picks the tier by how recently a record was read, and moves records between tiers
in the background. Every encoding round-trips exactly, which the proof checks.
