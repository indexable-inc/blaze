---
status: plan
summary: dedup-scan (2026-10-29): the copies of the copy-paste scan and its identifier-normalized rerun that no other dedup lane owns, each given one implementation (stdx::edit_distance, blaze_core::create_process, hir_def::test_items, blaze_query::query_kinds!, TerminatorKind::all_successors, hir-ty-mir's noop_landing_pads, blaze_codegen::unwind::FrameRule and others), written as requests to their owners, with the diagnostics, cargo and code-identity runs in the three child notes.
date: 2026-10-29
---
# dedup-scan: the rest of the scan

Part of [dedup](../dedup.md). Children: [diagnostics against blaze-lints](scan-diag.md),
[the cargo crates against blaze-units](scan-cargo.md), [code identity](scan-ident.md).

The scan (`/tmp/dedup-scan.py`) finds runs of equal lines; its rerun with every identifier that is
not a keyword spelled `I` (`/tmp/dedup-scan-norm.py`) also finds copies that were renamed. Most
normalized hits are `match` tables or struct literals with the same shape and different meaning;
each one below was read on both sides. Only `crates/blaze-core/src/create_process.rs` (and one
`mod` line) is written here; the rest are exact edits in `v2/requests/<owner>.md` under
`## dedup: …`, since every owner was running.

## Per primitive

### Edit distance: `stdx::edit_distance`

Six copies (blaze-manifest, blaze-resolve, blaze-diagnostics twice, hir-expand, hir-ty-matchck).
Picked: rustc's `rustc_span::edit_distance`, ported whole (`edit_distance`,
`edit_distance_with_substrings`, `find_best_match_for_name{,_with_substrings}`,
`find_best_match_for_names`), since every suggestion must be the one rustc makes; cargo's
lowercased variant and `closest`/`closest_msg` in `stdx::edit_distance::cargo`. Generic over the
candidate (`AsRef<str>`, or a projection with `_by`) so no caller copies names into a `Vec<&str>`.
Tests: every copy's, plus proptests against rustc's code kept verbatim as the reference.
Applied 2026-10-30 (queue request 00215): the six copies are deleted and every caller uses `stdx`.

Speed of light: it runs on the error path only, O(n·m) in code points with one rotating row
buffer of `3·(m+1)` and no other allocation; ASCII inputs take a byte path, and the
case-insensitive compare allocates nothing. Matches rustc's bound; nothing faster is wanted on an
error path.

### Windows command line and environment block: `blaze_core::create_process`

Two copies: blaze-core's driver (`iocp/process.rs`) and the build-script sandbox
(`blaze-cargo/src/sandbox/windows.rs`). They split back to the same arguments but differed in
bytes: core quoted an argument with a quote, newline or vertical tab; the sandbox quoted only
empty arguments and ones with a space or tab. Core sorted variable names in ASCII upper case, the
sandbox in Unicode simple upper case per code unit. Picked: std's `Command` rules (the sandbox's)
for both, since Windows' ordinal compare upper-cases per code unit and the runtime splits on space
and tab only. The work is on UTF-16 code units, so its tests run on every platform; the `OsString`
wrappers are `cfg(windows)`. Tests: both copies', plus a proptest that the C runtime's split
(written into the test) reads the arguments back from this line and from core's old one, and that
the block equals core's old one on ASCII names.

Speed of light: one pass over each argument into one buffer sized up front; the block is one
O(k log k) sort of the k variables and one buffer, without the hash map or the O(k²) `retain` the
copies had. Spawning a process costs far more.

### Fixture lookups in tests: `hir_def::test_items`

Six test modules each had `declarations`, `impls` and an exactly-one `function`/`type_alias`/
`static_item`/`constant` lookup. One copy on `&base_db::Db`, as frontend-db's one concrete
database. The twelve `TestDB`s are frontend-db's work (`Db::for_tests()`) and not repeated here.
Test-only; no speed bound.

### Query kind tables: `blaze_query::query_kinds!`

hir-def's resolver and hir-ty each had the same `kinds!` macro, `NAMES` table and `name`. One
macro next to `QueryKind`; numbers and names unchanged. Speed of light: `name` is one bounds check
and one load from a table built at compile time, as before.

### MIR successors: `TerminatorKind::all_successors`

Four copies agree on every terminator (borrowck's `cfg.rs`, hir-ty-mir's `optimize.rs`,
blaze-items' `form.rs` pushing them in reverse; `blaze-items/src/tests/form.rs` is a deliberate
reference and stays). One method beside `successors` and `unwind`; blaze-items' CodeHash walk
pushes it reversed, the same order, so no hash changes. Speed of light: an inline `SmallVec` of
two, no allocation for all but `SwitchInt` with more targets.

### No-op landing pads: `hir_ty_mir::mir::noop_landing_pads`

hir-ty-codegen's `resumes_only` was the fixpoint of hir-ty-mir's `remove_noop_landing_pads`
(rustc's `RemoveNoopLandingPads`). The set is split out of the pass and codegen calls it. Same
cost as before: a fixpoint over the cleanup blocks, linear per round.

### Small ones

- `FrameRule`: hir-ty-codegen's and blaze-stencil's `Rule` become one enum in
  `blaze_codegen::unwind` with stencil's `RaSignState` last, so persisted rules decode the same.
- `RelocKind` tags: blaze-premap's `reloc_tag` is `blaze_codegen::content::kind_tag`; the same
  bytes go into the key.
- `SelectError`: blaze-testsel's is blaze-usercode's, token for token.
- `ARITH_OPS`: two persisted-code tables become `ArithOp::ALL` in syntax, in declaration order.
- `block_may_start_with_inner_attr`: three copies (syntax's ast and view, hir-def's docs) become
  the view's with an `inner_docs` flag.
- `rust_files`: seven test copies become `test_utils::rust_files`, sorted, panicking on a
  directory it can't read (one copy silently skipped it).
- `describe(SourceId)` in blaze-registry and blaze-units: passed to the cargo child.

## Kept apart on purpose

- Codegen `Policy` fixtures in `plan.rs`, `driver/tests.rs` and `blaze-lazy`: different arch,
  CPU and frame pointers.
- The SIMD shim in `hir-ty-mir/tests/mir/machine.rs` and `hir-ty-ub/tests/ub/main.rs`: ub's sits
  in a `concat!` and must be a literal.
- blaze-diagnostics' test `CORE`s: minicore has no `rustc_diagnostic_item`s; the fix is to add
  them there, not to share these.
- `blaze-engine`'s `Resolver` and `blaze-wasm/src/names.rs`: the bridge to rustc-built symbols,
  to be deleted under no-rustc, not merged.
- The two stability target filters (`parsers/stability.rs` `visited`, `features/library.rs`
  `is_definition`) and hir-def's `is_definition`: different rustc rules.
- `HistoryError` and `RestoreError`, `Effect::tag` and `DeclOp::target`, stencil's `ty_of` and
  wasm SIMD's `Op::of`, blaze-doc's kind names and the protocol's step names: same shape,
  different meaning.
- hasher feeds, RNGs and SplitMix: dedup-codec's.
- `vendor/` runs between rustc's own crates.
