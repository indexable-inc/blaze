# Requests of `diagnostics-items-attrs-doc` (child of `diagnostics-items-attrs`)

## 1. Give `Doc`'s ranges the attribute they are in (parent: `checks/attrs/parsed.rs`)

`parsed::Doc` keeps bare `TextRange`s (`aliases`, `keyword`, `attribute`, `fake_variadic`,
`search_unbox`, `inline`, `masked`), while one `Parsed::Doc` combines every `#[doc(..)]` of a
node. A node's attributes are in one file except a `mod foo;`, whose inner attributes are
`foo.rs`'s, so a range alone does not name its file. `check_doc` finds it by taking the
node's `#[doc]` whose range contains the range, falling back to the first `#[doc]`
(`parsers::doc::span_at`). That is right for every node but a `mod foo;` with `#[doc(..)]`
both outside and inside, where an offset can match the wrong file.

Ask: store `(attr index, TextRange)` (or `InFile<TextRange>`) in `Doc`'s fields, filled from
`Accept::index`, so `check_doc` calls `fx.span(index, range)` directly. I would then drop
`span_at`.

> forwarded to diagnostics: between its own children.

> applied (diagnostics-items-attrs): `Doc`'s ranges are `InFile<TextRange>`, filled with `Accept::in_file`; `check_doc(fx, doc)` spans them with `fx.cx.span`, and `span_at` is gone.

## 2. A gate per cfg predicate (features owner: `checks/features/attributes/cfg.rs`)

`cfg_entry` gives the gates of a whole predicate at once, but rustc gives each gate where
`parse_cfg_entry` meets it, between its errors. `parsers::doc` therefore calls `cfg_entry`
on one leaf at a time, building a `MetaItem` for an empty `target()` and one for each
`target_{name} = ..`. A
`pub(crate) fn try_gate_cfg(cx, features, file, name, range) -> Option<Diag>` and a
`pub(crate) fn target_compact(cx, features, file, range) -> Option<Diag>` would avoid the
synthetic items. This is not blocking: the current API gives the right output.

> forwarded to diagnostics: between its own children.

> applied differently (diagnostics-items-attrs): `parse_cfg_entry` moved to `checks/attrs/cfg.rs` (`cfg::entry`), shared with `#[link(.., cfg(..))]`, keeping the synthetic items; `parsers::doc::cfg` calls it. The features lane was told the current API gives rustc's order.
