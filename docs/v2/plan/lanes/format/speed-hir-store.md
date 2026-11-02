---
status: plan
summary: The HIR expression store as columns (exprs, pats, type refs, source maps), lowering that allocates per body, and how it compares with Zig's ZIR/AIR, Carbon's SemIR and rustc's arena HIR.
date: 2026-10-29
---
# speed-hir-store: the HIR store against the state of the art

Code: `crates/hir-def/src/{hir.rs,hir/,expr_store.rs,expr_store/}`. The columns for expressions
and patterns are format-hir's (contract `format-hir-columns-api.md`); type refs, source maps,
lowering allocation and the budgets are this lane's pass 2 (`speed-hir-store-pass2.md`). Both
briefs live in `~/restart-2026-10-28/briefs/v2/`.

## Benchmarks

| system | node | lists | allocation | spans |
|---|---|---|---|---|
| Zig ZIR / AIR | `Inst`: 1-byte tag + 8-byte data (9 bytes, tags and data in separate arrays of a `MultiArrayList`) | `extra: []u32`, payload structs read out of it | one `ArrayList` per column, per file (ZIR) / per function (AIR) | ZIR: a source offset in the data word, relative to the decl; no line or column info |
| Carbon SemIR | 12 bytes: kind + two 32-bit args | value blocks: `u32` ranges into shared arrays | `ValueStore`s per file | a parse-node id per instruction, kept apart |
| rustc HIR | `hir::Expr<'hir>` 64 bytes on x86_64 (`HirId`, `ExprKind`, `Span`) | `&'hir [Expr]` slices | `DroplessArena`: one bump per crate, never freed | in every node; incremental hashes them relative to the owner |

## Before (commit `5179f7cb5e`'s parent)

- **Bytes**: `Expr` 48 (asserted), `Pat` 48 (largest variant `Record`: `Path` 24 +
  `Box<[_]>` 16 + `bool`; not asserted), `TypeRef` 24, `TypeBound` 16, `Path` 24, `Statement` 32,
  `MatchArm` 16, `Binding` 16, `Label` 8, in `Arena`s (`Vec`s) of those enums.
- **Allocations per body**: one `Box<[_]>` per non-empty child list (calls, method calls,
  blocks, matches, record literals, closures twice, tuples, arrays; tuple, or, record, slice twice
  and tuple-struct patterns), each built in a `Vec`/`SmallVec` first and shrunk; one `Box` per
  `&T` and fn-pointer type and one `ThinVec` per tuple type and bound list; one `Box` per path
  with generic arguments plus its `GenericArgs` boxes; and the growth reallocations of every
  arena, `ArenaMap` and hash map (about \(\log_2 n\) each).
- **Hashes**: lowering hashed every expression, pattern, label, type and lifetime into its
  forward source map (`FxHashMap<InFile<AstPtr>, _>` insert, with rehashes as it grew), and paths
  with hygiene into two more maps. `store[e]` was a plain index (0 hashes). An IDE lookup
  `node_expr` was one hash.

## After

- **Bytes per expression**: `expr_kind` (1) + `expr_data` (4) = **5 bytes**, `const` asserted
  \(\le 12\) in `hir/columns.rs`; the words each variant takes in `extra` are tabled in that
  module's doc. Patterns the same, 5 bytes. Type refs 5 bytes (`TypeColumns` in
  `hir/type_ref.rs`) with `RefType`/`ArrayType`/`FnType` in side tables, and paths in columns of
  their own (below). That beats ZIR's 9 and Carbon's 12 in the hot columns, and rustc's 64 by
  12x. The cost is decoding: a `store.expr(e)` view is two loads, a `match` on the tag and, for
  multi-word variants, a read of `extra`. ZIR pays the same for its payloads.
- **Statements** are rows too: `stmt_kind` (1) + `stmt_data` (4) = **5 bytes**, asserted, with
  a `let`'s 1 to 4 words in `extra` (Zig's trailing optionals). `foo();` takes 5 bytes,
  `let x = e;` 13, `let x: T = e else { .. };` at most 21. A typical body goes from a 32-byte
  `Statement` enum to about 9 bytes a statement, 3.5x fewer cache lines on every block walk. A
  block's statements are a run of rows (`Statements<'a>`, a `Copy` handle in `Expr::Block`). A
  local macro definition is a row and a `MacroDefId` in the `macro_defs` side table, where
  before it was boxed twice. On disk the rows are copied as they are (schema 4), not a 20-byte
  record per statement.
- **Lists**: CSR ranges into one array per element type (`expr_lists`, `pat_lists`, `arms`,
  `record_fields`, `field_pats`, `type_lists`, and for types `lists` and `bounds`), as ZIR's
  `extra` and Carbon's blocks.
- **Allocations per body**: lowering builds in per-thread pooled scratch columns, with children
  collected on a list stack (Zig AstGen's scratch pattern), so after a thread's first large body
  it allocates **nothing per node**. `finish` copies the `Copy` columns (kinds, data words,
  `extra`, id lists, statement rows, macro definitions) into **one slab together**
  (`hir/col.rs`, `Col<T>`). Each cold side table that isn't empty (names, literals, arms, record
  fields, and each of the path columns) gets one exact box, with no shrink reallocation. That is
  8 to 20 allocations a body down to 1 plus the non-empty side tables (typically 3 or 4). Decode
  uses the same slab. rustc's bump arena makes no allocation per body, but a body's store, unlike
  rustc's arena, is freed on its own when salsa evicts it. What still allocates per node:
  fn-pointer types (`FnType`'s two boxes), byte-string literals, `asm!` and `offset_of!`: all
  rare, cold side-table values.
- **Paths** (`expr_store/path.rs`, `PathColumns`): a path is a kind byte and a data word, 5
  bytes; a `Normal` path adds 4 `extra` words and a 4-byte slot per segment, and a segment with
  arguments a 20-byte row, 8 bytes an argument and 20 a binding. `Path<'a>` is a 16-byte `Copy`
  view, so copying a path copies no heap. Lowering appends to the scratch store's columns and
  allocates nothing per path, where before it allocated per `Normal` path, per segment-argument
  list, per binding list and per `for<..>`/`use<..>` list, and every `.clone()` deep-copied them:

  | path | before: bytes, allocations | after: bytes, allocations |
  |---|---|---|
  | `foo::bar` | 24, 0 | 21, 0 |
  | `Vec<u8>` | 112, 3 | 69, 0 |
  | `Iterator<Item = u32>` | 176, 3 | 89, 0 |
  | `<T as Trait<U>>::Assoc` | 6 allocations (two deep `Path`s, the trait's args cloned) | one 20-byte row and 2 args, 0 |

  Reading a `Normal` path's arguments is 4 dependent loads into dense columns that a body's paths
  share (data word, `extra`, slot, row), against a pointer chase to a separate heap object per
  level before. rustc's `hir::Path` arena-allocates its segments and a full `PathSegment` record
  per segment; these columns take 4 bytes a segment beyond the interned `ModPath`, plus rows only
  for segments with arguments. `TypeBound` is a 16-byte `Copy` record, its `for<..>` names and
  `use<..>` arguments runs into shared columns.
- **Hashes**: lowering hashes nothing per node. The forward source maps are pushed to and sorted
  once per body by `(file, start, end, kind)`, which is nearly sorted already because lowering is
  preorder. `store[e]` is still 0 hashes. An IDE lookup is a binary search
  (\(\log_2 n\) compares, no hash), and iteration order is deterministic.
- **Back maps** (id to syntax, `expr_store/source_map.rs`): per arena, dense columns of a `u16`
  kind (`u16::MAX` for no source) and two `u32` range ends, 10 bytes per id. Each file is
  stored once per run of ids (16 bytes a run), since preorder lowering gives each macro
  expansion a contiguous range of ids. Before, an `ArenaMap` slot of
  `Option<InFile<AstPtr<_>>>` took 24 bytes per id. A lookup is an index plus a binary search
  over the runs, which is one compare in a body without macros. `patterns_for_binding` is CSR,
  4 bytes a binding plus 4 a pattern, built with a counting sort. Before, it was a 32-byte
  `SmallVec<[PatId; 4]>` per binding. On disk a back map is 10 bytes per id plus 16 per run
  (was a 24-byte `Src` per id), read with one copy per column (schema 4).
- **Invalidation, finer than rustc's red-green graph**: the store holds no spans (positions live
  in the source map, relative to the body's anchor), and bodies are lowered from their own
  syntax (`Body::relative`). So a formatting or comment edit inside a body re-lowers it to an
  `==` store with equal encoded bytes, and salsa backdates it: inference, MIR and borrowck don't
  run again. rustc's incremental hashes spans relative to the owner, so the same edit changes the
  owner's HIR hash and re-runs `typeck`. An edit elsewhere in the file doesn't even re-lower,
  only moves the source map. This is the per-definition granularity of Unison's content-addressed
  code, with positions kept apart. `expr_store/tests/invalidation.rs` tests the three cases.

- **Expression scopes** (`scope.rs`, built for every body inference reads): one entries array
  with a range per scope, and a dense `ExprId`-indexed scope column (4 bytes, was 8 in an
  `ArenaMap` of `Option`). Per scope 17 bytes in columns, where before it was about 32 plus a
  `Box` per macro scope. Built in per-thread scratch and copied out exactly. Entries are grouped
  by scope with a stable radix sort at `finish`, which is skipped when they are already grouped.
  rust-analyzer's walk-order ranges let a `let` see a `const` pattern block's locals, where rustc
  doesn't.
- **`body_syntax`** (re-run for every body of a file on each edit of it) built the relative
  `AstIdMap` and span map once per body. It now builds them once per anchor, shared by its bodies:
  for an `impl` of \(M\) methods and \(n\) nodes, \(O(Mn) \to O(n + M)\) per edit.
- **Scans**: `expr_ids()` / `pat_ids()` give a store's ids without decoding, for the id-only
  whole-store scans in `hir-ty-borrowck` (request in `v2/requests/db.md`).
- **Paths no longer intern their `ModPath` a second time.** `ModPath` is a 16-byte `Copy` value
  with interned segments (speed-hir-expand), held directly in the path columns: one intern probe
  per lowered path instead of two, and one pointer fewer on every read.

## Still short, and what it takes

- **Signatures re-lower on every edit of their file.** `FunctionSignature::with_source_map` and
  the other signatures lower from the file's AST with absolute pointers, so any edit re-runs them
  and they backdate. Making signatures relative to an anchor, as bodies are, removes that
  (`signatures.rs`, speed-hir-defs).
- **`Option<Idx>` is 8 bytes.** `la_arena::Idx` has no niche, so `MatchArm` stays 16 bytes
  and `RefType` 16 (statements are rows since 2026-10-29: 5 bytes plus a `let`'s words). A raw
  index that can't be `u32::MAX` (a `NonMaxU32`-style niche) makes them 12/12 with no API
  change. That needs `la-arena` vendored (root
  `Cargo.toml`, coordinator).
- **Path columns are not in the slab.** `PathColumns` freezes to one exact box per non-empty
  column (3 for a store of bare paths only, up to 12), because lowering reads paths back through
  the same `&PathColumns` views that read a frozen store, and a slab needs the growing and frozen
  columns to be different types. If it shows in a profile, lowering reads the builder's paths
  through non-view accessors (4 call sites) and `PathColumns` becomes generic over `ColumnVec`,
  as `HirColumns` is (`expr_store/path.rs`).
- **Fn-pointer types** still box `FnType`'s `binder` and `params` per type (`hir/type_ref.rs`).
  Moving them to runs in the type columns changes every `&FnType` reader.
- **Decode borrows nothing from the pack.** A store read from the signature image copies its
  `Copy` columns into a fresh slab. To borrow them in place, `deps/signatures.rs` must name the
  image owner type and pass an `Arc` of it to `read` (`v2/requests/speed-hir-store.md`).
- **Bindings and labels** keep their arenas (16 and 8 bytes, flat records, no per-node heap).
