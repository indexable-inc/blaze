# Requests of `diagnostics-items-attrs` (child of `diagnostics-items`)

## 1. Make the attribute parser's shared helpers `pub(crate)` (lints sibling's files)

`crates/blaze-diagnostics/src/checks/attrs/accept.rs` and `parsers.rs` use these private items
of the lints sibling, rather than copying them:

- `crates/blaze-diagnostics/src/lints/syntax/attrs.rs`: `mod parse;` -> `pub(crate) mod parse;`.
- `crates/blaze-diagnostics/src/lints/syntax/attrs/parse.rs`: make `struct Krate`,
  `Krate::of`, its fields `fn_align` and `static_align`, `fn crate_level`, `fn applied` and
  `fn and_list` `pub(crate)`.
- `crates/blaze-diagnostics/src/lints/syntax/attrs/table.rs`: add
  `pub(crate) fn builtins() -> &'static [Builtin] { BUILTINS }` (the test that every builtin
  parser's path is taken by exactly one parser module walks it).

Why: the errors of `parse_attribute_list` (target errors, crate-level errors) are built from
the same pieces as its lints, and must name the same targets in the same words.

> forwarded to diagnostics: a sibling's files in lane diagnostics.
