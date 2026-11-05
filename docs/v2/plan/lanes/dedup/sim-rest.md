---
status: plan
summary: dedup-sim-rest (2026-10-29): the last 20 cross-crate clone and near groups of 15+ lines in the semantic duplicate report. Seven are copies, each given one owner (hir-def's expression and pattern kinds, hir-ty's struct tail, stdx's thread-local lend, rustc_ast_ir's integer widths, hir-def's version parse, syntax's precedence, hir-expand's macro kind, hir-ty-codegen's proc-macro list, blaze-resolve's block_on); the rest compute different things, or are covered by earlier requests.
date: 2026-10-29
---
# dedup-sim-rest: the rest of the similar-code groups

Part of [dedup](../dedup.md). The input was `/tmp/similar-rest.json`: the cross-crate `clone`
and `near` groups of 15+ lines from the 2026-10-29 22:57 run of `tools/similar` that no other
child owned. Edits made here: `crates/hir-def/src/hir/columns.rs`, `hir.rs` and
`expr_store/persist.rs` (format's, which is done), and `crates/hir-ty/src/layout.rs` (items',
which is done). Everything else is a request.

## Per primitive

| Primitive | Owner | Copies that go |
| --- | --- | --- |
| Expression and pattern kind tags | `hir_def::hir::{ExprKind, PatKind}` (the columns' bytes) | blaze-diagnostics `lints/bodies.rs` `ExprTag`/`PatTag`; hir-def `expr_store/persist.rs` `ExprKind`/`PatKind` |
| rustc's `struct_tail_raw` | `hir_ty::layout::struct_tail_erasing_lifetimes` | hir-ty `consts/valtree.rs` `tail`, hir-ty-borrowck `regions.rs` `tail` |
| A value lent to a thread-local for a call | `stdx::lend` | hir-def `hir/persist.rs` and hir-ty `check_cache/ids.rs` `relocating` bodies |
| Integer and float widths | `rustc_ast_ir`'s `bit_width` | blaze-diagnostics `panics.rs` `scalar_kind`, hir-ty `hir_eval.rs` `kind_of` tables |
| rustc's `parse_version` | `hir_def::attrs::stability::RustcVersion::parse` | blaze-diagnostics `checks/attrs/parsers.rs`, `checks/features/library.rs` |
| rustc's `ExprPrecedence` questions | `syntax::view::prec` | blaze-diagnostics `type_mismatch::needs_parens`, blaze-lints `needless_borrow::unambiguous` lists |
| `MacroDefKind` to `MacroKind` | `hir_expand::MacroDefKind::macro_kind` | hir-expand `MacroCallId::kind`, hir `Macro::kind` matches |
| The proc-macro list's source text | `hir_ty_codegen::v2::proc_macro_list::list_text` | blaze-engine `macro_list.rs` `text` |
| One-thread executor | `blaze_resolve::block_on` | blaze-registry `lib.rs` and `tests/query.rs` |

The shared tag fixes a real drift: `ExprTag` had no `UnsafeBinderCast`. The shared precedence
fixes two lists that disagree with rustc: blaze-lints called `a..b` unambiguous, and diagnostics
left out `continue`, `become` and `do yeet`. The shared tail follows pattern types as rustc does,
and stops at an infinitely representable struct where valtree's copy looped forever.

## Speed of light

- Kind tags: `ExprKind::of` is a `match` on the decoded variant, which compiles to the
  discriminant, one load and one table lookup per node, the same as `ExprTag::of`.
  `ExpressionStore`'s read view now reads a kind as `ALL[byte]`, a bounds-checked load, in place of
  a `from_u32` that did the same.
- Struct tail: one step per struct or tuple level, with the representability query (salsa-kept,
  one lookup) per struct. The recursion became a loop, so there's no stack per level.
- `stdx::lend`: two `RefCell` borrows and two moves of the table per scope, the same as both
  copies.
- Widths, version parse, precedence, macro kind: each the same constant-time `match` or parse
  as before. `Macro::kind` reads the cached `MacroId::definition` in place of a lookup of the
  macro's loc, both one salsa read.
- Proc-macro list: one `String` per proc-macro crate, written once, as before.

## Not copies

- `float_cc` (blaze-stencil) and `float_predicate` (hir-ty-codegen LLVM): Cranelift's `FloatCC`
  into two backends' types (the stencil's `FloatCc`, LLVM's `FloatPredicate`).
- `Def` (diagnostics `print/paths.rs`) and `Item` (hir-def `deps/signatures/key.rs`): different
  variant sets for different uses (a printed path's definition; a signature query's record with
  `Fields` and `Variants`).
- `Impls` (blaze-doc) and `SourceConfigDef` (blaze-manifest): unrelated schemas.
- `is_def_kind` (diagnostics stability) and `is_definition` (hir-def stability): every node of
  rustc's `DefId` tree (modules, `use`, generic params, macros) against only the definitions that
  can hold a block. The generated `can_cast`s are the syntax crate's codegen.
- `module` (blaze-items `ItemId`, hir-def's `HasModule` impls, `signatures/key.rs` `Item`): one
  `match` per id enum, each its own type.
- `ObligationCauseCode` (hir) and hir-ty's: `hir`'s facade copy with its own ids (`GenericDef`),
  joined by one `From`, as every `hir` diagnostic type is.
- `Test` (blaze-engine harness) and v2's `roots/harness.rs`: two harnesses (v2 writes a listing
  digest and serves the fork server; v1 marks each test for the sandbox).
- `parse_version` in blaze-link `target.rs`: the `MACOSX_DEPLOYMENT_TARGET` grammar packed as
  `xxxx.yy.zz` nibbles, not rustc's.
- `SourceId` `fmt` (blaze-manifest and blaze-source): two `Display`s of two `SourceId` types.
  The real copy is the type, requested as lane cargo's "one `SourceId`", after its request 1.
