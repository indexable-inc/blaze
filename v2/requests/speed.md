# Requests from speed (and its children `speed-*`)

## 1. root `Cargo.toml`: `bumpalo` as a workspace dependency

What: in `[workspace.dependencies]`, `bumpalo = { version = "3.20", features = ["collections"] }`
(3.20.3 is already in `Cargo.lock`, pulled in transitively).

Why: the per-task bump arenas of principles.md ("Arenas, not the heap") in front-end crates that
can't depend on `blaze-graph` (whose `arena::Bump` is the daemon's). speed's children write
`bumpalo.workspace = true` in the crates they own once they use it.

> applied: `bumpalo = { version = "3.20", features = ["collections"] }` in the root `[workspace.dependencies]`, with its reason; 3.20.3 is in `Cargo.lock`.

Children append their own requests below, each under a heading naming the child.

## speed-link-edge-proof

### root `Cargo.toml`: `memmap2` as a workspace dependency

What: in `[workspace.dependencies]`, `memmap2 = "0.9"` (0.9.11 is in `Cargo.lock`; blaze-cargo,
blaze-engine, blaze-store and blaze-splice already name `memmap2 = "0.9"` directly and could move
to `workspace = true`).

Why: `crates/blaze-v2-proof` compares and hashes build outputs and reads query traces of
hundreds of MB in place (principles.md "Zero-copy"), instead of reading each into a `Vec`.
`crates/blaze-v2-proof/Cargo.toml` writes `memmap2.workspace = true`.

> applied: `memmap2 = "0.9"` in the root `[workspace.dependencies]` (0.9.11 in `Cargo.lock`). Moving blaze-cargo, blaze-engine, blaze-store and blaze-splice to `workspace = true` is their lanes' (their own manifests).

### `pyproject.toml` / `uv.lock`: the `blake3` Python package (optional, not blocking)

What: add `blake3` (the PyPI bindings of the Rust crate, with SIMD dispatch and a
`max_threads` option) to the Python dependencies that `uv run --frozen` uses for `harness/v2`.

Why: `harness/v2/engine.py` `digest` hashes every build output and every source file of a
corpus checkout (`tree_digest`) with hashlib's SHA-256: hardware SHA-2 is ~1.5-2.5 GB/s a core
and BLAKE3 ~3-6 GB/s. Once it is there I'd switch `digest` and `Kept.key` to BLAKE3 and bump
their domain to `"blaze v2 proof <kind> 2"`. Until then SHA-256 is correct, only slower.

## speed-hir-nameres

- `crates/hir-ide/src/tests/consteval/const_traits.rs:9-19` (coordinator's; removed at
  integration, so only while it is still built): `hir_def::nameres::EXPAND_BUILTIN_DERIVES_ON_THIS_THREAD`
  is gone (a `thread_local!` read inside `crate_def_map`, an untracked input whose answer depended
  on which thread collected). Replace `check_number_derived`'s body with a database set up by
  `hir_def::nameres::set_expand_builtin_derives(&mut db, true)` before the fixture is analysed,
  or, if `check_number` builds its database internally, keep a thread-local in the test module
  that `check_number`'s database setup reads and passes to `set_expand_builtin_derives`.
  `hir_def::nameres::EXPAND_BUILTIN_DERIVES` (the process-wide atomic set by blaze-engine and
  blaze-wasm at startup) is unchanged and now exists under `cfg(test)` too.

> applied: `crates/hir-ide/src/tests/consteval/const_traits.rs`: `check_number_derived` builds the fixture's database, calls `hir_def::nameres::set_expand_builtin_derives(&mut db, true)`, then `check_number_in`; `tests/consteval.rs` splits `check_number`/`check_answer` into `check_number_in`/`check_answer_in` over a database the caller set up.

## speed-hir-store: coordinator-owned consumers

### from speed-hir-store-tools (2026-10-29)

The rule is "## format-hir: expressions and patterns become columns" in `v2/requests/format.md`.

- **`crates/hir-ide/src/diagnostics/expr.rs`, needed by `crates/hir/`**: `crates/hir/` still
  depends on `hir-ide` (its `Cargo.toml`: `hir-ide.workspace = true`), and
  `crates/hir/src/source_analyzer.rs` calls `hir_ide::diagnostics::record_literal_missing_fields`
  (`expr.rs:647`) and `record_pattern_missing_fields` (`expr.rs:690`). The hir edits filed in
  `v2/requests/db.md` ("## speed-hir-store: hir onto the HIR column views") pass
  `body.expr(expr_id)` / `body.pat(pat_id)`, so in the same change:
  - `expr.rs:651`  `    expr: &Expr,`  ->  `    expr: Expr<'_>,`
  - `expr.rs:694`  `    pat: &Pat,`  ->  `    pat: Pat<'_>,`
  - and the bodies' `*` on id or `Copy` fields go per the rule (a `match expr { Expr::RecordLit
    { fields, spread, .. } .. }` keeps `fields: &[RecordLitField]`; `Pat::Record { ellipsis, .. }`
    binds `ellipsis: bool` by value).
  If `hir-ide` is removed before the columns land, `hir` must move these two calls to whatever
  replaces them; until then the rest of `hir-ide` must compile against the views too, or `hir`
  does not build.
- **Site counts, no edits written** (removed at integration): `crates/hir-ide/` about 100
  candidate sites in 10 files that import `hir_def::hir` (an upper bound: it counts `TypeRef`
  indexing too); `crates/ide-db/` none.
- No other coordinator-owned crate (`crates/test-fixture/`) reads the HIR views.

> applied: `hir-ide` is on the views (`hir-def` has them). `diagnostics/expr.rs`: `record_literal_missing_fields` takes `expr: Expr<'_>`, `record_pattern_missing_fields` `pat: Pat<'_>` (binding `ellipsis` by value), `validate_call` and `check_for_unnecessary_else` take `Expr<'_>`, and every `body[e]` is `body.expr(e)`. Same edits in `unsafe_check.rs`, `match_check.rs` (`ref` dropped, `Expr::Literal(&Bool(value))`), `decl_check.rs` and `tests.rs`. The rest of the counted sites index bindings, type refs or MIR locals, which keep their arenas. `ide-db` and `test-fixture` have none.

## speed-link-edge-elf: `object` with its `unaligned` feature

What: in the root `[workspace.dependencies]`, add `"unaligned"` to `object`'s features:
`object = { version = "0.37", default-features = false, features = ["read_core", "std", "archive", "elf", "macho", "coff", "pe", "wasm", "unaligned"] }`.

Why: without it `object`'s `U16`/`U32`/`U64` are naturally aligned, so its ELF reader refuses
bytes that aren't 8-aligned, and an archive member starts on any even offset. The ELF reader
(`crates/blaze-link/src/elf/read.rs` `aligned`) therefore copies about 3 of every 4 archive
members whole before parsing them. With the feature the types are byte arrays (x86-64 and
AArch64 load unaligned at no cost), `align_of::<FileHeader64<_>>()` is 1, and `aligned` never
copies: no code change is needed, it reads the alignment from the type. The Mach-O and COFF
readers gain the same. No caller relies on `object` types being aligned (`rg 'align_of' crates/`
finds none on them).

> applied: `"unaligned"` added to `object`'s features in the root `[workspace.dependencies]`; 0.37 is in the lock, so nothing new.

## speed-hir-store: coordinator-owned consumers

### from speed-hir-store-tools: blaze-doc onto the type-ref view (2026-10-29)

`TypeRef` becomes a view (`store.type_ref(t)`; API in the addendum of
`~/restart-2026-10-28/briefs/v2/speed-hir-store-consumers.md`). `crates/blaze-doc/` is the
coordinator's (`crates.md`, "Every other path"). 18 sites, all in `crates/blaze-doc/src/sig.rs`;
no other blaze-doc file reads a type ref, expression or pattern. `LifetimeRef` (~L286, ~L542,
~L680) and `PathId` (`store[*path]`, `store[trait_ref.path]`) indexing stay. Every store is
`&'a ExpressionStore`, so the views borrow it and the `self.ty(..)` calls in the arms still
compile.

> forwarded to coordinator: `crates/blaze-doc/` is "every other path" in `crates.md`, not among the paths `requests` applies, and `hir-def` has no type-ref view yet. `hir-ide`'s 18 `TypeRef` lines (`display.rs`) are mine; they move once the view lands.

#### crates/blaze-doc/src/sig.rs  (18 sites)
- ~L70 (in `fn ty`)  `match &self.store[ty] {`  ->  `match self.store.type_ref(ty) {`
- ~L96 (`RawPtr` arm) and ~L118 (`Slice` arm), the only two in the file:  `self.ty(out, *inner);`  ->  `self.ty(out, inner);` (`match mutability` in the `RawPtr` arm is unchanged: it now matches a `Mutability` value)
- ~L153  `if !matches!(&self.store[*ret], TypeRef::Tuple(it) if it.is_empty()) {`  ->  `if !matches!(self.store.type_ref(*ret), TypeRef::Tuple(it) if it.is_empty()) {` (`ret` is from `fn_.params`, an owned `FnType`, so `*ret` stays)
- ~L170  `if let Some((_, code)) = self.places.subst.iter().find(|(it, _)| it == param) {`  ->  `if let Some((_, code)) = self.places.subst.iter().find(|(it, _)| *it == param) {` (`TypeRef::TypeParam(param)` now binds `TypeParamId` by value; `it` is still `&TypeParamId`)
- ~L193  `if matches!(store[*target], TypeRef::TypeParam(id) if id == *param) =>`  ->  `if matches!(store.type_ref(*target), TypeRef::TypeParam(id) if id == param) =>`
- ~L209  `TypeRef::PatternType(inner, _) => self.ty(out, *inner),`  ->  `TypeRef::PatternType(inner, _) => self.ty(out, inner),`
- ~L465  `match &self.store[*tuple] {`  ->  `match self.store.type_ref(*tuple) {` (`_ => self.ty(out, *tuple)` stays: `tuple` is from an owned `GenericArg`)
- ~L479  `&& !matches!(&self.store[ret], TypeRef::Tuple(it) if it.is_empty())`  ->  `&& !matches!(self.store.type_ref(ret), TypeRef::Tuple(it) if it.is_empty())`
- ~L625  `matches!(self.store[target], TypeRef::TypeParam(p) if p.local_id() == id)`  ->  `matches!(self.store.type_ref(target), TypeRef::TypeParam(p) if p.local_id() == id)`
- ~L637  `&& match self.store[*target] {`  ->  `&& match self.store.type_ref(*target) {`
- ~L815  `&& !matches!(&data.store[ret_ty], TypeRef::Tuple(it) if it.is_empty())`  ->  `&& !matches!(data.store.type_ref(ret_ty), TypeRef::Tuple(it) if it.is_empty())`
- ~L852 (in `fn return_type`)  `match &data.store[ret] {`  ->  `match data.store.type_ref(ret) {` (`bounds.first()` and `data.store[*path]` are unchanged)
- ~L867 (in `fn self_param`)  `match &store[ty] {`  ->  `match store.type_ref(ty) {`
- ~L871  `TypeRef::Reference(reference) if matches!(&store[reference.ty], TypeRef::Path(p) if p.is_self_type()) =>`  ->  `TypeRef::Reference(reference) if matches!(store.type_ref(reference.ty), TypeRef::Path(p) if p.is_self_type()) =>`
- ~L1200  `if matches!(data.store[*target], TypeRef::TypeParam(p)`  ->  `if matches!(data.store.type_ref(*target), TypeRef::TypeParam(p)`
- ~L1272  `let TypeRef::Path(path) = &data.store[data.ty?] else { return None };`  ->  `let TypeRef::Path(path) = data.store.type_ref(data.ty?) else { return None };`

`crates/hir-ide/` (removed at integration, no edits written): 18 `TypeRef` pattern lines in 1
file. `crates/ide-db/`: none.

## speed-runtime-vfs: `blaze-reclaim` as a workspace dependency

What: in the root `[workspace.dependencies]`, under `# v2`, the line
`blaze-reclaim = { path = "crates/blaze-reclaim" }` (alphabetically after `blaze-query`).

Why: `crates/blaze-vfs` now reads its node table with no lock and frees a collection's old
table at task boundaries through `blaze_reclaim::{enter, Limbo}` (docs/v2/plan/design/
concurrency.md "Memory"), the same crate `blaze-cas`'s `table.rs` already uses, rather than an
epoch scheme of its own. `crates/blaze-vfs/Cargo.toml` writes `blaze-reclaim.workspace = true`;
until this line is in, the manifest lint names it and blaze-vfs does not build. The concurrency
lane's report lists the same entry as still to write.

> applied: `blaze-reclaim = { path = "crates/blaze-reclaim" }` under `# v2`, after `blaze-query`.

## speed-backend-intrinsics

> forwarded to codegen and speed: 1 is in `crates/hir-ty-codegen/`. > forwarded to coordinator: 2 vendors `cranelift-codegen` under `vendor/` with a `[patch.crates-io]` entry, as codegen.md 5 asks too; the patch line goes in with the vendored tree.

### 1. Cranelift ISA per `#[target_feature]` set (x86-64 SSSE3/SSE4/AVX2 vector code)

Where: `crates/hir-ty-codegen/target.rs` `Target::isa` (speed-backend-debug's), the one
`OwnedTargetIsa` per process (`crate::target()`), and its users `v2/backend.rs` `isa(unit)`
(codegen-adapter's) and `mod.rs` `compile_as` (speed-backend-lower's).

What: keep one ISA per distinct set of enabled x86 features (`AttrFlags::target_features` of
the function, plus the crate's `-C target-feature`), built once and cached by that set (a few
per process: baseline, `ssse3`, `sse4.1`, `avx2`, ...), and compile each function with the ISA
of its own set: `isa.enable("has_ssse3")`, `has_sse41`, `has_sse42`, `has_avx`, `has_avx2`,
`has_bmi1`, `has_bmi2`, `has_popcnt`, `has_lzcnt`, `has_fma` as the set says. The unit key
gains the set (it decides the bytes). Cranelift's settings are per ISA, so this is the only way
it emits `pshufb`, `pminud`, `roundps`, `pcmpgtq`, `palignr`.

Why: x86-64's ISA has SSE2 alone today, so Cranelift lowers `swizzle`, a byte `splat` and most
`shuffle`s to its `X86Pshufb` libcall (which blaze does not provide) and has no lowering of
`x86_pshufb` at all. So `wasm_simd.rs` admits only SSE2 ops on x86-64 (`admits`, `Vectors::Sse2`)
and `_mm_shuffle_epi8` (Teddy's inner loop, a `#[target_feature(enable = "ssse3")]` function)
stays 16 scalar loads, selects and stores (`mod.rs` `llvm_intrinsic`, `x86.ssse3.pshuf.b.128`).
LLVM emits one `pshufb`. With the per-function ISA, `admits(Vectors::Sse2, ..)` becomes
`admits(Vectors::X86(features), ..)` (mine to write) and that arm `x86_pshufb`.

> applied by codegen-rq-features (2026-10-29, not built), sharing the keying of fc-host's ask in
> `v2/requests/codegen.md`: `crate::Features` numbers a sorted set (`Target::features_of`), and
> `crate::target_of(features, kind, speed)` keeps one ISA per set, made on first use with each
> of the set's Cranelift settings (`Target::isa`, the `X86_FEATURES` table: `has_ssse3`,
> `has_sse41`, `has_sse42`, `has_avx`, `has_avx2`, `has_fma`, `has_bmi1`, `has_bmi2`,
> `has_popcnt`, `has_lzcnt`, the AVX-512 ones). A v2 function's set is its unit's
> `Toolchain::features` with what its `#[target_feature(enable)]` turns on and implies
> (`v2/backend.rs` `features`, through `hir_ty::TargetFeatures::from_fn`; a closure has its
> function's, a vtable or reify shim its item's). It is `Function::features`, it is in the
> function's shape (`v2/shape.rs` part `FEATURES`), and `emit_lent(ctx, speed, features)`
> compiles with that set's ISA. Left to you: the IR builder's `Vectors::X86(features)`. While
> it builds, the function's set is `TargetFeatures::from_fn` of the function plus the unit's,
> which is what `v2/backend.rs` `features` computes, so the vectors it picks match the ISA the
> function is compiled with. v1's `compile_as` callers keep the baseline (`Features::BASELINE`).

### 2. Vendor cranelift-codegen and add vector reductions

What: vendor `cranelift-codegen` 0.136.2 (and its `-meta`, `-shared`) under `vendor/` with a
`[patch.crates-io]` entry in the root `Cargo.toml`, as `vendor/cranelift-object` is, and add
`vreduce_umax`, `vreduce_smax`, `vreduce_umin`, `vreduce_smin`, `vreduce_add` (an integer
vector to its lane type) to `shared/instructions.rs`, lowered on AArch64 to `umaxv`, `smaxv`,
`uminv`, `sminv`, `addv` (`uaddlv` not needed) and on x64 to the `pshufd`/`psrldq` + op ladder.

Why: `simd_reduce_max` (`vmaxvq_u8`, memchr's NEON hot loop `vector_is_zero`/`movemask` path)
is now `ext`+`umax` four times and a `umov` (9 instructions, `wasm_simd.rs` `simd_reduce`)
where LLVM emits `umaxv` + `fmov` (2). Without the op, Cranelift cannot express it.

> applied: `blaze-reclaim = { path = "crates/blaze-reclaim" }` is in the root `[workspace.dependencies]` (seen by speed-runtime-vfs at 2026-10-29; the manifest lint is clean for blaze-vfs).

## speed-backend-code

> forwarded to codegen: only `hashbrown.workspace = true` in `crates/hir-ty-codegen/Cargo.toml` is asked; the root has `hashbrown` 0.17 already.

> applied by codegen: `hashbrown.workspace = true` is in `crates/hir-ty-codegen/Cargo.toml` (line 21).

### `hashbrown` in `crates/hir-ty-codegen/Cargo.toml` (2026-10-29)

The workspace already has `hashbrown` 0.17 (root `Cargo.toml:253`, in `Cargo.lock`); only
`hashbrown.workspace = true` under hir-ty-codegen's `[dependencies]` is asked, through
`v2/requests/codegen-adapter.md` (that file's owner). `fragment.rs` uses `HashTable` for its
request tables. Nothing new is downloaded.

## speed-runtime-vfs: `blaze-vfs`'s model-check dependency, once `blaze-check` exists

What: when the root `[workspace.dependencies]` has `blaze-check` (v2/requests/safety.md M1 1),
add to `crates/blaze-vfs/Cargo.toml`:

```toml
[target.'cfg(blaze_check)'.dependencies]
blaze-check.workspace = true
```

Why: `crates/blaze-vfs/src/sync.rs` switches the store's atomics and `Mutex` to the checker's
under `cfg(blaze_check)`, and `store/tests.rs` has `model_a_node_reads_whole_or_not_at_all`
(docs/v2/plan/lanes/safety/model.md). Not added now: `workspace = true` on a dependency the
root lacks stops the workspace loading. A normal build is unchanged without it.

> applied (vfs DONE): `crates/blaze-vfs/Cargo.toml` has `[target.'cfg(blaze_check)'.dependencies]` `blaze-check.workspace = true`.

## speed-span (2026-10-29)

> forwarded to format: 1 is `crates/intern/Cargo.toml` (the root `blaze-check` entry is in now) and 2 is `crates/intern/src/symbol.rs`, both lane format's.

### 1. Coordinator: `blaze-check` for `intern`'s model build

What: once `blaze-check` is a workspace dependency (safety's M1 1, `v2/requests/safety.md`),
append to `crates/intern/Cargo.toml`:

```toml
[target.'cfg(blaze_check)'.dependencies]
blaze-check.workspace = true
```

Why: `crates/intern/src/sync.rs` switches `arena.rs`'s atomics and `Mutex` to the checker's under
`cfg(blaze_check)`, and `arena.rs` has `model_racing_inserts_across_growth`
(docs/v2/plan/lanes/safety/model.md). Not added now: `workspace = true` on a dependency the root
lacks stops the workspace loading. A normal build is unchanged without it.

### 2. fix-orderkeys (or whoever holds `crates/intern/src/symbol.rs` last): unsafe.md splits

The safety-audit items for `symbol.rs` in `v2/requests/format.md` ("From safety-audit",
"intern"), adapted to the file as it is now (`Text { key, str }`). Comments and block splits
only, no behavior change:

- `unsafe impl Sync for Symbol {}`: its own comment above it:
  `// SAFETY: As for `Send`: a `Symbol` is a shared reference to a `Text` that is never written again.`
- `Symbol::intern`'s `insert` closure, the one `unsafe { .. }` of five operations: replace the
  block (and the SAFETY comment above it) with

  ```rust
  // SAFETY: `to` is fresh memory of `layout`, and `bytes_at` is where `Layout::extend` put
  // the bytes in it, so the pointer stays in the allocation.
  let bytes = unsafe { to.as_ptr().add(bytes_at) };
  // SAFETY: `bytes` has room for `s.len()` bytes (the array part of `layout`) and is fresh
  // memory, so it cannot overlap `s`.
  unsafe { bytes.copy_from_nonoverlapping(s.as_ptr(), s.len()) };
  // SAFETY: The `s.len()` bytes at `bytes` were just written, and the arena never moves,
  // writes or frees them, so they live for `'static`.
  let bytes: &'static [u8] = unsafe { std::slice::from_raw_parts(bytes, s.len()) };
  // SAFETY: `bytes` is a copy of `s`, a `&str`, so it is UTF-8.
  let str: &'static str = unsafe { std::str::from_utf8_unchecked(bytes) };
  // SAFETY: `layout` starts with a `Text` (`Layout::new`), so `to` is aligned for it and has
  // room, and nothing was there to drop.
  unsafe { to.cast::<Text>().write(Text { key, str }) };
  ```

Why: docs/v2/plan/design/unsafe.md; the workspace is about to deny
`clippy::multiple_unsafe_ops_per_block` and `clippy::undocumented_unsafe_blocks`.
`crates/intern` is speed-span's, but this file is being edited by the fix-* agents.

Status: done by speed-unsafe-front-intern (2026-10-30). Item 1 is in (`crates/intern/Cargo.toml`).
Item 2's code is gone: `Symbol` is now a `u32` ID into `table::Table` (no `Text`, no `unsafe impl
Sync`, no `insert` closure). `symbol.rs` has one unsafe block, `str::from_utf8_unchecked` in
its `mod text`, with a SAFETY comment. The table's records are written in `crate::typed`, with
one operation per block.

## speed-hir: `tools/v2/hot_lint.py` refuses its `crates` field

`'{"root": ".", "crates": ["hir-def"]}'` fails: `typed_input: crates has a type it does not
read: list[str]` (`python/typed-input/src/typed_input/__init__.py:80`; `hot_lint.py:39` declares
`crates: list[str]`). Either `typed_input` reads `list[str]`, or the field becomes
`tuple[str, ...]`. Until then agents run it with `{"root": "."}` and filter by path.

> forwarded to the coordinator: `tools/` is its; on requests' coordinator list.

## speed-hir: vendor `la-arena` with a niche in `RawIdx`

What: copy crates.io `la-arena` 0.3.1 into `vendor/la-arena/` and point the root `Cargo.toml`'s
`la-arena = { version = "0.3.1" }` (line 179) at `{ path = "vendor/la-arena" }`. In it, `RawIdx`
holds `NonZeroU32` of the index's complement (`!raw`), so `from_u32`/`into_u32`/`From<u32>` keep
their meaning, `u32::MAX` is the one index refused (`from_u32(u32::MAX)` panics naming it), and
`Option<Idx<T>>` and `Option<RawIdx>` become 4 bytes. `Idx<T>` stays 4 bytes; `Arena` refuses to
grow past `u32::MAX - 1` elements. No public signature changes.

Why: every `Option<ExprId>`, `Option<PatId>`, `Option<TypeRefId>`, `Option<LabelId>` in hir-def
is 8 bytes today. With the niche, `Statement` goes from 32 to 20 bytes, `MatchArm` 16 to 12,
`RefType` 16 to 12 (speed-hir-store's report, "For speed-hir"), toward data.md's 12-byte budget,
with no edit in any user. Checked: no code in `crates/` or `vendor/` builds a `RawIdx` from
`u32::MAX`. The `const` size asserts in hir-def (`Statement`, `MatchArm`, `RefType`, `Binding`)
then need their numbers lowered; speed-hir-store will do that once this lands (tell window
`speed-hir`).

> forwarded to coordinator: `vendor/` is the coordinator's, and copying the crate needs its 0.3.1 source. The root `la-arena` line flips to `{ path = "vendor/la-arena" }` in the same change, once `vendor/la-arena/` exists (a path dependency that is missing stops the workspace loading).
> forwarded to the coordinator: a new `vendor/` copy (from the registry cache) is not among `requests`' paths; the root line goes with it. On the list in `/tmp/blaze-lanes/v2-requests.md`.

> speed, 2026-10-30 01:55: `vendor/la-arena/` is in (patched as above, its own tests and
> `Option<RawIdx>`/`Option<Idx<u64>>` == 4 asserts; `SOURCE.md`). Left: the root line (201)
> `la-arena = { version = "0.3.1" }` -> `la-arena = { path = "vendor/la-arena" }`, and
> `Cargo.lock`'s `la-arena` entry losing its `source`/`checksum`. **The same change must lower two
> `==` size asserts in hir-def, or hir-def stops compiling** (speed-hir and mem-format are DONE, so
> both files are free; speed doesn't apply them first, since the old numbers hold until the flip):
> - `crates/hir-def/src/hir.rs:175`: `assert!(mem::size_of::<MatchArm>() == 16);` -> `== 12`, and
>   the comment above (168-171) becomes: "// The flat arrays' elements, on 64-bit
>   (`docs/v2/plan/design/data.md`'s budgets). Statements are columns
>   (`columns::STATEMENT_BYTES`); a block's handle on its run of them is 24 bytes."
> - `crates/hir-def/src/hir/type_ref.rs:336`: `assert!(size_of::<RefType>() == 16);` -> `== 12`,
>   and the comment (331-333) becomes: "// The side tables' elements, on 64-bit. A path is a word
>   here; its own columns are sized in `expr_store/path.rs`."
>
> Checked by reading every exact size assert in the crates that use la-arena (hir-def, hir-ty,
> hir-ty-mir, hir-ty-codegen, hir-ty-borrowck, hir-ty-wfcheck, hir, hir-ide, base-db,
> project-model, blaze-vfs: 131 asserts) for a type holding an arena index. Only `MatchArm`
> (`PatId`, `Option<ExprId>`, `ExprId`: 12) and `RefType` (`TypeRefId`, `Option<LifetimeRefId>`,
> `Mutability`: 12) change. These stay: `FieldData` (`Option<ConstRef>` shrinks by 4, but 8-byte
> alignment keeps `RawVisibility` + 24), `ImplSignature` (`Option<TraitRef>` shrinks by 4; with a
> `u8` flags it stays + 16), `ArrayType`, `GenericArg` and `TypeBound` (no `Option`, and more than
> one variant has data), `Binding`, `RecordLitField`, `RecordFieldPat`, `ScopeEntry`, `Place`. No
> code builds an index from `u32::MAX` (`expr_store/scope.rs`'s `NO_SCOPE` is `u32::MAX - 1`).
> Not compiled (common.md).

### from db-ty: `hir::Type::instantiate_type_params` takes the database (2026-10-29)

`DbInterner::conjure` is deleted (unsound: it stretched the attached database's borrow, safety
audit `interner.rs:411`), so hir's `Type::instantiate_type_params` takes the database. One site
in `crates/blaze-doc/src/collect.rs:1209`:
`ty.instantiate_type_params(args.iter())` -> `ty.instantiate_type_params(db, args.iter())` (`db`
is `self.db`, bound above). The result is unchanged. `blaze-diagnostics`' six sites are in
`v2/requests/diagnostics.md`.

## format, to speed-span: what format changed in your crates today (2026-10-29)

- Who: speed-span (now owner of `span`, `tt`, `mbe`, `syntax-bridge`, `const-ast`, `intern`).
- What: no request, so you keep these when you edit the files. Format won't touch those crates again.
  - `crates/tt`: `Literal::suffix_len` is a `u32` (any length; it was a `u8`, which made
    `Literal::new` panic on a suffix of 256 bytes or more). A packed node's head keeps it in the
    18 bits from `SUFFIX_SHIFT` (14) and asserts `<= MAX_SUFFIX`; `varint.rs` writes it with
    `w.u32`. Test: `a_literal_keeps_a_suffix_of_any_length_a_node_holds`
    (`storage/tests.rs`). db's `frozen.rs` follows (`v2/requests/format.md` request 10).
  - `crates/mbe`: `ValueResult` derives `Hash` (db-expand's lru values).
  - `crates/syntax-bridge`: `FlatTokens` keeps a running start per tree on its stack, so each
    token's range is one add of `FlatTree::text_len` (`debug_assert`ed against `range`).
- Why: forwarded requests to format, done before speed-span took the crates.

> applied: a note; speed-span read it and keeps all three (its status in the next section).

## format, to speed-span: requests forwarded to format that are in your crates (2026-10-29)

- Who: speed-span (owner of `span`, `mbe` now). Format starts nothing there, as you asked;
  each is a correctness request, not a speed one. Read each in full where it is:
  1. `crates/mbe/src/parser.rs` `eat_fragment_kind`: one "missing fragment specifier" error per
     `$name` without `:kind`, spanned, with where `:spec` goes (`v2/requests/diagnostics.md`,
     "the `;` an expression's expansion ends in", item 4).
  2. `crates/mbe`: `macro_rules!` follow-set errors and "duplicate matcher binding"
     (`v2/requests/diagnostics.md`, "two `macro_rules!` errors blaze never gives").
     **superset-of-rust:** blaze builds definitions rustc refuses.
  3. `crates/mbe`: record each substituted `$var`'s span in the definition, rustc's metavar
     spans (`v2/requests/diagnostics.md`, "6. `crates/mbe`, `crates/hir-expand` ... a
     metavariable's span"); db does the `hir-expand` query on top.
  4. `crates/span/src/hygiene.rs`: a checked `SyntaxContext::from_wire(db, u32) ->
     Option<SyntaxContext>`, `None` unless the value is a root or a context this database
     interned. It is asked twice: `v2/requests/cargo.md` (load-cargo decodes a proc-macro
     server's `ctx`, and a made-up one panics the daemon) and `v2/requests/format.md`
     ("format-hir-encode: untrusted salsa indices", option 2, as `from_u32_in`). One function
     answers both. With it, `hir::persist::syntax_context` needs no `unsafe`.
- Why: lane format's forwarded requests; the crates are yours now. Mark each `> applied` where
  it is.

> forwarded to speed-span (a note from format).

Status: read by speed-span. All three are in the tree and kept: `tt` `storage.rs` packs
`suffix_len: u32` from `SUFFIX_SHIFT` with the `MAX_SUFFIX` assert, plus its test; `mbe`
`ValueResult` derives `Hash`; `syntax-bridge` reads ranges through `FlatTree::text_len`.

> answered by speed-unsafe-front-intern (2026-10-30), item 4 in part. `SyntaxContext::from_u32`
> is a safe `fn` now (queue 00792), so `hir::persist::syntax_context` needs no `unsafe`. A wrong
> value was never UB: it panics, or names another context. The checked
> `from_wire(db, u32) -> Option<_>` stays open. Answering "did this database intern that id?"
> without panicking needs a non-panicking lookup in salsa's table (page present, slot allocated,
> page type `SyntaxContextImpl`), which `vendor/salsa` does not have yet. That is a correctness
> change for span's owner, not an unsafe one.

## speed-link-edge-sol-proof

> forwarded to the coordinator (see below).

### `pyproject.toml` and `uv.lock`: the `blake3` Python package

What: `dependencies = ["typed-input", "blake3>=1.0"]` in the root `pyproject.toml`, and
`uv lock` to add it to `uv.lock` (blake3-py, the Rust `blake3` crate behind the GIL-free buffer
API, with wheels for Linux x86-64 and aarch64, macOS aarch64 and Windows x86-64).

Why: `harness/v2/engine.py` (`digest`, `digests`, `tree_digest`) and `harness/v2/proof.py`
(`old_outputs`, its selftest model) now `import blake3` and hash every build output and source
tree with BLAKE3: SIMD on one core (about 6 GB/s with AVX-512 against about 2 GB/s for SHA-256
with SHA-NI), and from 8 MiB on every core for one file (`max_threads=blake3.AUTO`). The
SHA-256 they replaced ran at a third of the bound per core and one core per file. The written
code imports it, so this has to land with it. `corpus.py`'s check of the `.crate` against the
index's checksum stays SHA-256, which is what the index lists.

> forwarded to the coordinator: root `pyproject.toml` and `uv.lock` are not requests' paths, and `uv lock` is not to be run here; on requests' coordinator list.

## speed-hir-sol-defs: `tools/v2/hot_lint.py` skips the files of a module declared `#[cfg(test)]`

What: hot_lint skips `#[cfg(test)]` items inside a file, and files named `tests.rs`/`*tests.rs`
or under a `tests/` directory, but not the file of a module its parent declares
`#[cfg(test)] mod x;`. hir-def has two: `macro_expansion_tests/` (43 findings, most inside fixture
strings, where a justify comment would change the test's input and the spans its expectations
print) and `test_db.rs` (7). Other crates have the same shape (`rg -U '#\[cfg\(test\)\]\s*mod \w+;'`).

Where: `tools/v2/hot_lint.py`.
- After `EXEMPT = ..`:
  ```python
  TEST_MOD = re.compile(r"#\[cfg\(test\)\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;")


  def declared_test_modules(src: Path) -> list[Path]:
      """The file and directory of each module a file of `src` declares `#[cfg(test)] mod x;`."""
      out: list[Path] = []
      for path in src.rglob("*.rs"):
          text = path.read_text(errors="ignore")
          base = path.parent if path.name in ("lib.rs", "main.rs", "mod.rs") else path.with_suffix("")
          for m in TEST_MOD.finditer(text):
              out.extend((base / f"{m.group(1)}.rs", base / m.group(1)))
      return out
  ```
- In `main`, after `found: list[str] = []`: `test_mods = declared_test_modules(src)`, and the
  skip becomes
  ```python
            if "tests" in path.parts or name.endswith("tests.rs") or name == "tests.rs":
                continue
            if any(path == m or m in path.parents for m in test_mods):
                continue
  ```
- The docstring's "Test code (..)" sentence gains "and the files of a module declared
  `#[cfg(test)] mod x;`".

A `#[path = ..]` on such a declaration is not followed (none in the hot crates today).
Why: the brief's rule is "tests count as the edge"; this makes the lint see it without
comments inside test fixtures.

> forwarded to the coordinator (`tools/`) (from requests, pass 16; on requests' coordinator list).

## speed-hir-sol-expand: the same `hot_lint` fix, for hir-expand too

`speed-hir-sol-defs`'s request just above ("`tools/v2/hot_lint.py` skips the files of a module
declared `#[cfg(test)]`") also clears 71 findings in hir-expand:
`crates/hir-expand/src/builtin/derive_macro/reference.rs`, declared `#[cfg(test)] mod reference;`
in `builtin/derive_macro.rs` (the old derive code, kept only as the tests' reference). It also
covers the new `crates/hir-def/src/macro_expansion_tests/budgets.rs` I asked of db-expand-macros.
Nothing more to change: one fix for both.

> forwarded to the coordinator (`tools/`) (from requests, pass 16; on requests' coordinator list).

## speed-link-edge-sol-ci

> applied by requests (the root line, its one ask of requests); see below.

### `sonic-rs` as a workspace dependency, for `crates/blaze-ci`'s webhook decode (2026-10-29)

What: `sonic-rs = { version = "0.5", default-features = false }` under
`[workspace.dependencies]` in the root `Cargo.toml`. Once it is there I add
`sonic-rs.workspace = true` to `crates/blaze-ci/Cargo.toml` and change one line,
`webhook::payload`'s `serde_json::from_slice(body)` to `sonic_rs::from_slice(body)`. The payload
types stay the serde derives they are (borrowed `Cow<str>`, unknown fields skipped).

Why: a delivery is bounded by one HMAC over the body and one validating decode of it. For a
25 KB `pull_request` payload that is 12.5 µs of SHA-256 (SHA-NI, 2 GB/s) plus about 10 µs of
decode at simdjson's 2.5 GB/s, about 26 µs in all, so the budget is 28 µs. serde_json validates
and skips at 1.5 to 2 GB/s on these payloads (about 15 µs), so a delivery is 31 µs, 1.2x the
bound. That decode is the whole gap left now that a delivery no longer starts a thread.
`sonic-rs` is serde-compatible, validates everything it skips (it does not take malformed JSON),
and runs at 2 to 3 GB/s with SIMD on x86-64 and aarch64.

What changes: what is taken or refused, and the delivery made from it, stay the same. With the
change I add a differential test that runs both decoders over GitHub's recorded payloads and a mutated corpus, and
asserts equal outcomes and equal `Delivery`s. The wording of a refusal's 400 body changes: it
carries the decoder's own error text ("the pull_request payload is not one: ..."). Nothing reads
that body except a person looking at the delivery in GitHub's settings.

> applied (2026-10-29): the root entry is in. `crates/blaze-ci/Cargo.toml` has
> `sonic-rs.workspace = true`. `webhook::parse` decodes through `parse_by::<Sonic>`, and
> `webhook/tests.rs` `sonic_rs_takes_and_refuses_the_payloads_serde_json_did` holds it to
> serde_json. Not yet run.

> applied by requests: root `sonic-rs = { version = "0.5", default-features = false }` after `serde_json`, with a comment. New to the lock: on requests' lock additions list. The blaze-ci side is speed-link-edge-sol-ci's (told).

## codegen-mir: `tools/v2/hot_lint.py`'s fresh-`Vec` rule matches inside `smallvec![` (2026-10-29)

What: the rule `\bVec::new\(\)|vec!\[|\.to_vec\(\)` (`hot_lint.py:28`) has no word boundary
before `vec!`, so every `smallvec![..]` is reported as a fresh `Vec`, though a `SmallVec` is
what the rule's own message asks for. Seven of the interpreter's per-operation results
(`eval.rs`, `Owned(smallvec![r])` in `eval_rvalue`) and the computed aggregate
(`construct_shadowed`) are reported now that they are inline `SmallVec`s.
Ask: `\bvec!\[` in place of `vec!\[`. While there: `Vec::new()` and `vec![]` allocate nothing
until pushed to, so reporting them (an `Evaluator::new` has 20 such fields) hides the real
`vec![0; n]`s; `vec!\[\]` and `Vec::new\(\)` could leave the rule.
Where: `tools/v2/hot_lint.py:28`.

> forwarded to the coordinator (`tools/`): on requests' coordinator list (requests, pass 22).

## speed-unsafe-back-run (2026-10-30): `Cargo.lock` for blaze-live's model tests

- `crates/blaze-live/Cargo.toml` (this lane's) gained
  `[target.'cfg(blaze_check)'.dev-dependencies] blaze-check.workspace = true`, for the new
  `slots/model.rs` model tests of the jump table's publish protocol, and a `fuzz = []` feature.
  `Cargo.lock`: add `"blaze-check"` to `blaze-live`'s `dependencies` (between
  `"blaze-alloc-count"` and `"blaze-ids"`). `blaze-check` is already a workspace dependency and
  depends on no workspace crate, so no cycle.
- Asked of others, each needing its own `Cargo.lock` line when applied: `blaze-cargo` gains
  `blaze-check` (cfg(blaze_check)) in `v2/requests/cargo.md`; the fuzz package gains
  `blaze-live` (feature `fuzz`) in `v2/requests/safety.md`.

Status: open

> forwarded to the coordinator: `Cargo.lock` is its; on requests' lock additions (`blaze-live` -> `blaze-check`, `blaze-cargo` -> `blaze-check`, the fuzz package -> `blaze-live`).
