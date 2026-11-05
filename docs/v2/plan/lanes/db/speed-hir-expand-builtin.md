---
status: plan
summary: Builtin macros (hir-expand `builtin/`) brought to the speed bar - cached quote symbols, one output buffer per derive, no tree clones or per-token strings in fn-like macros - with what each costs before and after against rustc's builtin_macros.
date: 2026-10-29
---

# speed-hir-expand-builtin: builtin derives, fn-like and attribute macros

Paths: `crates/hir-expand/src/builtin.rs`, `crates/hir-expand/src/builtin/` (`quote.rs`,
`derive_macro.rs` with `derive_macro/reference.rs` and `derive_macro/tests.rs`, `fn_macro.rs`,
`attr_macro.rs`).

## The benchmark

- Derives: rustc's `rustc_builtin_macros::deriving` (`generic/mod.rs`). It reads the item's
  AST, which it already has, and builds the impl's AST directly: no token trees, no reparse. It
  still interns a `format!`ed name per field binding (`__self_0`, `__arg1_0`) and allocates one
  `P<Expr>` per node.
- `format_args!`: rustc parses the template into a `FormatArgs` AST node right away
  (`rustc_builtin_macros::format`) and lowers it in `rustc_ast_lowering::format`. Here the
  expansion is `builtin # format_args (args)`, which hir-def lowers, so the expansion's only
  cost is copying the arguments once.
- `quote!`: the `quote` crate's `quote_spanned!` writes into one `TokenStream` and interns
  through the proc-macro bridge, which caches per call.

## Before and after

| | before | after |
|---|---|---|
| identifier in `quote!` | `Symbol::intern(stringify!(..))`: a hash and a table probe per token per expansion | a `static OnceLock<Symbol>` per call site: one acquire load |
| `Name`/`Symbol` spliced as an identifier | interned again from its text | the symbol as it is (`Name` is never stored with `r#`) |
| `#tree`/`#string` splice | moved in, so callers cloned trees and strings to splice them | taken by reference, nothing cloned |
| string literal | `escape_default().to_smolstr()`, then intern | interned as it is when nothing needs escaping; otherwise the kept runs are copied whole |
| derive output | one `quote!` per field, per variant, per bound and per arm, each its own builder and a `TopSubtree` (five boxed slices) copied into the next one out: about 4 to 6 trees per field, 100+ per derive of a ten-field struct | one `TopSubtreeBuilder` per derive (`quote_into!`, `Group`/`Emit` splices): one tree, built once |
| field bindings (`{name}_self`, `{name}_other`, `f{i}`) | a `format!` String and an intern each, again in each derive's loop | an intern from a 64-byte stack buffer, once per field per derive (`bindings`) |
| `derives_copy` / `repr(packed)` | `attr.syntax().text().to_string()` per attribute | the attribute's tokens read in place, word by word |
| shape | `Vec<(Name, Vec<(Ident, Span)>)>` per variant | one field list for all variants with `u32` ranges per variant (CSR) |
| `format_args!`, `asm!`, `global_asm!`, `naked_asm!`, `pattern_type!`, `unreachable!` | `tt.clone()` (five boxed slices copied), the delimiter changed, then copied again into the output | `WithDelimiter` over the call's tree: copied once into the output |
| `format_args_nl!` of a literal | clone, then `set_token`, which flattens the whole tree into a `Vec<TokenTree>` and rebuilds it, then copied again | the arguments written once with the one literal replaced; one `String` sized for the new text |
| `assert!` | a `format!` message, its condition in a second `String`, and the panic call built as its own tree and copied in | one message buffer sized from the condition; the panic call written in the outer builder |
| `option_env!` | a nested `quote!` tree for the value | written in place |
| `concat!` | `format_to!` per number, and a `Cow` String per escaped string literal | `push_str` of the text; escaped literals unescaped straight into the output |
| `include_bytes!` | `escape_ascii().to_string()`: the `Display` formatter, byte by byte, into a growing String | runs of kept bytes copied whole into a buffer sized for the file, escapes from a table |
| `include_str!` | `from_utf8_lossy(..).into_owned()` copied the file, then escaped char by char | escaped from the file's bytes where they are, kept runs copied whole |
| `line!`, `column!`, `file!`, `module_path!` | the same text interned (and for `module_path!`, escaped) on every call | a `static OnceLock<Symbol>` each |
| `#[derive]` pseudo-expansion | a builder grown as it went | sized up front from the argument count |

Expansions run in parallel as plane tasks. The only shared writes on their paths are the
interner's own, plus a `OnceLock` filled once per call site and read with one acquire load after.

## Where it still differs from rustc

- The derives still parse their input tree into syntax (`token_tree_to_syntax_node`), once per
  derive. rustc reads its AST in place, but here the input is the item after `cfg` and `cfg_attr`
  processing, which the source AST behind `ast_id` is not, so reading the source would be wrong.
  The fix that remains: one parse shared by every derive of one `#[derive(..)]`, which all get
  the same input. That needs a query in `crates/hir-expand/src/db.rs`, the parent's file (see the
  report).
- `CoercePointee` still edits where clauses with `SyntaxEditor`. It is a rare derive, and its
  calls stay on `SyntaxEditor`'s public API (`with_ast_node`, `GetOrCreateWhereClause`).
- A literal written in `quote!` (`"\n"`, `0`) is still interned per expansion. They are rare in
  the expanders; identifiers, the common case, are cached.

## Proof

- `quote.rs` tests: `quote_into!` with `Group`/`Emit` against nested `quote!`s (`Debug`,
  `Display`, `==`); cached symbols equal `Symbol::intern`; `str_literal` equals
  `escape_default` over every char up to U+00FF and mixed runs; `Name`/`Symbol` idents equal the
  old re-interning.
- `derive_macro/tests.rs`: every derive over a table of items (unit, tuple and record structs,
  generics with bounds and associated types, packed, enums with discriminants and `#[default]`,
  unions, raw and non-ASCII names) against `derive_macro/reference.rs` (the old nested-`quote!`
  code, test-only): the same trees, `Debug`, `Display` and errors.
- `fn_macro.rs` tests: `parenthesized` and `WithDelimiter` against the old clone-and-change;
  `format_args_nl_of_literal` against the old `set_token` path for plain, escaped, raw, suffixed
  and non-string templates; `escape_ascii` against `<[u8]>::escape_ascii` over all 256 bytes and
  a seeded mix; `write_assert_condition` appended after the head.
