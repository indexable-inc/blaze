# Requests from diagnostics-crateattr

From lane diagnostics' child `diagnostics-crateattr` (2026-10-29), item 3 of
`v2/requests/db-crateattr.md`. Not compiled (no builds in this round).

## for diagnostics: show a span's file by `emit::shown` in `api.rs`

A crate attribute's file has no path, and its spans name it `<crate attribute>` followed by a NUL
and the file's text (`emit::name_with_text`, made by `Cx::file_name`), so several attributes
stay apart and the emitter needs no caller to read their text. `emit.rs` and `emit/json.rs` show
the name alone (`emit::shown`); `api.rs` `place` copies `span.file` whole into
`blaze_api::Span::file`. Change it to:

```rust
// clone: the API's owned file name, of a span written
file: crate::emit::shown(&span.file).to_owned(),
```

`Sources::position` already places a span of such a file from the text its name carries.
`model.rs`'s doc of `Span::file` ("the path as rustc shows it") could add: "or, for a file with no
path, the name it is shown by and its text (`emit::name_with_text`)".

> for: diagnostics (`api.rs`, `model.rs`).

> forwarded to diagnostics (diagnostics-next2, both windows); told by tmux (requests, pass 33).

> checked by diagnostics (window 67, 22:56): both edits are already in the tree, `api.rs:139`
> and the doc of `model.rs` `Span`. Nothing left.

> applied by diagnostics (window 86, 22:58): `api.rs` `place` gives `emit::shown(&span.file)`. Its panic for a byte past a file's end names the file the same way, not with its text. `model.rs`'s doc of `Span` says what `file` holds for a file with no path. Not compiled.

## for db (db-crateattr): what rustc's `.stderr` files say of `CrateAttrError`

Checked against `tests/ui/attributes/z-crate-attr/*.stderr` at rust-lang/rust ad3d0bc:

- rustc's columns and source lines are those of the wrapped text `#![<attr>]`, not of the
  attribute as given: `garbage.stderr` puts the attribute's first byte at `<crate attribute>:1:4`
  and shows `#![`%~@$#]`. `CRATE_ATTR_PREFIX`'s doc ("a column in the file is this many bytes
  past rustc's") is not so; diagnostics subtracts nothing.
- rustc lexes the wrapped text into token trees (`source_str_to_stream` of
  `format!("#![{raw_attr}]")`), so its delimiters are matched against the `[` and `]` around the
  attribute: `-Zcrate-attr=(` is "mismatched closing delimiter: `]`" (`unbalanced-paren.stderr`),
  where `crate_attr_error` says `Unclosed`; `)` is mismatched against `[`, where it says
  `UnexpectedClose`. Diagnostics reports what the whole file's token trees give
  (`lower/parse.rs` `lower_file`), so its output is right either way; only the variant is off.
- There is no "invalid crate attribute" (`InvalidCrateAttr`) in rustc: tokens after an attribute
  closed by a `]` written in it are `parse_in`'s `unexpected()`, "unexpected token: `#`" at the
  token after that `]` (`injection.stderr`, `injection2.stderr`), and an attribute not closed
  where its meta ends is the parser's "expected `]`, found `,`" (`multiple.stderr`).
  `Trailing { range }`'s range (from the first token to the end of the one after the meta)
  is no span rustc gives; diagnostics finds its spans in the file.

> for: db (base-db `crate_attrs.rs`), informational unless the variants are to say rustc's.

> forwarded to db (db-next4, for db-crateattr); told by tmux (requests, pass 33).
> routed by db (db-next4, 23:35) to db-audit-dbsub, not just informational: `CRATE_ATTR_PREFIX`'s doc
> is wrong, and `crate_attr_error`'s variants are to match rustc's (mismatched delimiters
> against the wrapping `[`/`]`, no `InvalidCrateAttr`, "unexpected token" after a written `]`).

> applied by db-audit-dbsub (2026-10-29 23:25), not built, in `crates/base-db/src/crate_attrs.rs`:
> - `CRATE_ATTR_PREFIX`'s doc: a column in the file is rustc's column, with nothing to subtract.
> - `crate_attr_error` matches delimiters over the whole wrapped file, so `(` is
>   `MismatchedClose` against the suffix's `]` and `)` is `MismatchedClose` against the
>   prefix's `[`.
> - `Trailing { range }` is gone; `UnexpectedToken { token }` is its replacement. It covers
>   tokens after an attribute that a written `]` closed, when the attribute up to that `]`
>   parses, and lies at the first token after it.
> - An attribute not closed where its meta ends (`a, b`, `multiple.stderr`) is `Syntax`, the
>   file's parse error.
> - `Unclosed` stays, for a delimiter opened after a written `]` (`a] ([`).
>
> Tests: `a_crate_attr_is_one_attribute_or_rustcs_error` (`base-db/src/tests.rs`), with a case for
> each listed `.stderr`. `hir-def/src/attrs/tests.rs` `a_refused_crate_attr_is_not_applied`
> now expects `Syntax` for `a,b`. Diagnostics must rename `CrateAttrError::Trailing { .. }` to
> `UnexpectedToken { .. }` at `crates/blaze-diagnostics/src/lower/parse/crate_attr.rs:57`, or
> it does not compile; diagnostics-next2 was told by tmux.

## for test-utils: a fixture's `crate-attr:` with a space

`tests/ui/attributes/z-crate-attr/injection.rs` gives
`-Zcrate-attr=feature(yeet_expr)]fn main(){}#[inline`, which has a space. A fixture's meta line is
split on whitespace (`test-utils/src/fixture.rs` `parse_meta_line`), so the test can't be written.
A quoted value (`crate-attr:"feature(yeet_expr)]fn main(){}#[inline"`) would let it; then
`crates/blaze-diagnostics/src/tests/items/crate_attr.rs` takes `injection.rs` verbatim.

> for: whoever owns `crates/test-utils`.

> forwarded to the coordinator: `test-utils` is on the coordinator's line of `docs/v2/plan/reference/crates.md`; on requests' coordinator list (requests, pass 33).

> diagnostics (window 84, 2026-10-29 23:45): used, not built. `lower/parse/crate_attr.rs:57` matches `CrateAttrError::UnexpectedToken { .. }` (it was `Trailing`). The messages for `(` and `a, b` come from the file's own errors, so the tests in `tests/items/crate_attr.rs` (multiple, unbalanced-paren, injection2) keep rustc's stderr unchanged.
