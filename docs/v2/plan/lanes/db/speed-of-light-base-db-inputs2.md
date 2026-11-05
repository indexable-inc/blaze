---
status: plan
summary: speed of light for base-db's reparse on an edit (db's D8) and its lookup of a path another source root names (a `#[path]` module outside its package), with the arithmetic, how the code reaches it, and the gaps left.
date: 2026-10-29
---
# Speed of light: reparse on an edit and paths across source roots (`crates/base-db/`)

Nothing here was run: costs are counted from the code. The unit costs are those of
[speed-of-light-infer-relate.md](speed-of-light-infer-relate.md): an L1 hit is about 3 cycles,
an L2 hit about 16, and a salsa memo hit about 100. Streaming reads of a file's text from L2 run
at about 32 bytes a cycle.

## Reparse on an edit (`reparse.rs`, `files.rs` `write_text`, `EditionedFileId::parse`)

### The work

An edit of a file of n bytes changes k bytes, in one item of t tokens. The file's tree has
N tokens.
- The setter must find the edit. That takes the common prefix and suffix of the two texts, and
  so a read of about 2(n − k) bytes. Without a diff from the editor, no setter can read less.
- The parse must lex and parse the changed item (about t tokens) and splice it into the tree.
- A cache hit of `parse` must do nothing.

### What the code does

- **Setter.** `TextEdit::between` compares 32 bytes at a time: one `[u8; 32]` equality, which
  compiles to two 16-byte vector compares on NEON and SSE2 and one on AVX2. Only the block
  where the texts differ is looked into, a `u64` lane at a time through `trailing_zeros` and
  `leading_zeros`. That is 2(n − k) bytes at about 32 bytes a cycle: for a 100 KB file,
  about 6k cycles (2 µs). The same setter already hashes the new text with BLAKE3 (about
  6 bytes a cycle with SIMD, so about 17k cycles), so the edit costs under a third of what
  the setter pays anyway. The setter also makes one `Arc<Slot>` per text.
- **Parse on a miss.** Finding the kept parse is up to `KEPT` = 4 acquire loads of
  `OnceLock`s and one `Weak::upgrade`, which is an atomic increment: under 50 cycles, with no
  lock. Then `Parse::reparse_onto` parses the smallest token, body, block or item that holds
  the edit (about 60 cycles a token) and splices it in. A whole parse of the same 100 KB file
  is about 30k tokens × 60, so about 1.8M cycles. The reparse is about t × 60 plus the
  splice.
- **Cache hit.** Salsa returns the memo, and nothing here runs. `a_cache_hit_of_parse_allocates_nothing`
  holds it to zero allocations.
- **Memory.** A slot is about 260 bytes: the `Arc` header, the text's `Arc<str>`, the 32-byte
  content id and four `OnceLock<Kept>`. A slot holds its parse weakly, so it keeps no tree that
  salsa's LRU let go of. A file edited twice without a parse keeps the text last parsed
  (one extra text per file, until its next parse) so that the next reparse is one edit
  from it.
- **Debug builds** also make a whole parse for each reparse and compare the two
  (`assert_whole`). That is the check the property test relies on; release builds skip it.

### Gaps

- `FlatTree::replace_subtree` (format's, `crates/syntax-tree`) copies the tree's arrays on a
  splice. That is O(N) at memory bandwidth: for 30k tokens at about 16 bytes each, about 15k
  cycles. It is well under the whole parse it replaces, but above the t-token floor for a
  large file. Reaching that floor needs a tree that shares unchanged runs (a rope of
  arrays), which is format's to decide.
- How far the reparse goes is decided by format's `parsing::reparsing` (the token, then the
  body, block or item). An edit that no such element holds is a whole parse.

## A path another source root names (`named.rs`, `SourceDatabase::resolve_path`)

### The work

A `#[path]` module or an `include!` outside its package must find the file by its absolute
path among every source root's files, and must record a read the next edit can invalidate. The
floor is one hash of the path, one probe of a table, and a read of the input that answers.

### What the code does

- **Lookup.** The anchor's own root is checked first, as before: one `FileSet` probe. When it
  misses, the code does the following.
  - It joins the path, which allocates a `VfsPath`.
  - It computes `hash_of` the path, an FxHash at about 1 cycle per 8 bytes.
  - It reads the path's bucket input: one salsa input read, about 20 to 50 cycles.
  - It calls `Table::find`, which is acquire loads with no lock: one or two cache misses, about
    100 to 200 cycles.
  - It reads the file's `FileSourceRootInput` and source root, and probes that root's
    `FileSet`.

  In all, about 500 cycles. Only a lookup that its own root misses pays this.
- **Setters.** `Named::note` costs O(1) a path: one table insert, or one atomic swap when a
  path moves to another file. `Named::publish` hashes the new paths of a setter in one
  `ContentId::of_many` batch (BLAKE3's many-input SIMD). It then rehashes only the buckets
  that changed, each from its count, its four `u64` lane sums and its move count: O(1) a
  bucket, whatever the bucket holds.
- **Invalidation.** A hit reads the found file's source root, so an edit or removal there
  reruns the reader. A miss reads its bucket's content id, so a root that later names the
  path (or gives it to another file) reruns the reader. Only the readers of that one bucket,
  1/64 of the cross-root lookups, rerun.

### Gap

The joined `VfsPath` is an allocation of about 50 to 100 cycles, roughly 15% of the lookup.
Removing it means hashing and comparing the anchor's directory and the relative path piece by
piece, with `..` resolved as the pieces are read. It is left for now because only a lookup that
its own root misses pays it: a `#[path]` or `include!` outside its package, which is rare.
