# Requests from db-crateattr

From lane db's child `db-crateattr` (2026-10-29), for diagnostics.md "diagnostics-lints-syntax:
`-Z crate-attr` as a file of its own". Not compiled (no builds in this round).

## for diagnostics: read `-Z crate-attr` off its files (`checks.rs`, `lints/syntax/attrs.rs`)

> applied by diagnostics-crateattr (2026-10-29 23:30): item 3. Each refused attribute is
> reported in `lower/parse/crate_attr.rs`, called from `lower/parse.rs` `parsed` right after the
> crate root (rustc's `parse` then `inject`), at `Phase::Parse`, not fatal unless rustc's lexer
> raises one; each finding has `Fact::Crate`. rustc's `.stderr` files differ from this request
> in three ways (`v2/requests/diagnostics-crateattr.md`): columns and source lines are the
> wrapped `#![..]` text's (nothing is subtracted); delimiters are matched against the `[`/`]`
> around the attribute, so the errors are `lower_file`'s of the whole file; and trailing tokens
> are "unexpected token: .." or "expected `]`, found ..", never "invalid crate attribute".
> Files are named `<crate attribute>` with their text after a NUL (`emit::name_with_text`), so
> `Cx::file_of`, `Cx::snippet` and the emitter read each attribute's own text and cargo reads
> nothing; no `cargo.md` request was needed.
> Columns, checked against the `.stderr` files (rust-lang/rust ad3d0bc, 2026-07-31) on the
> parent's request: `garbage` (`-Zcrate-attr=`%~@$#`) puts the attribute's first byte at
> `<crate attribute>:1:4` and shows `#![`%~@$#]`; `multiple` (`feature(foo),feature(bar)`) puts
> the `,` at `1:16`, which is 13 in the attribute as given. So the columns count the whole
> `#![..]` text, and the "subtract `CRATE_ATTR_PREFIX.len()`" in item 3 would be off by 3.

`hir_def::attrs::parse_extra_crate_attrs{,_view}` are deleted. Each crate attribute is now a file
of its own (base-db `crate::crate_attrs`): text `#![<attr>]`, at the crate's edition, in the crate
root's source root with no path there, shown as `base_db::CRATE_ATTR_FILE_NAME`
(`"<crate attribute>"`). What base-db and hir-def give:

- `base_db::crate_attr_files(db, krate) -> &[EditionedFileId]`: every attribute's file, in the
  order given, refused or not.
- `base_db::applied_crate_attr_files(db, krate) -> &[EditionedFileId]`: those rustc applies (no
  `crate_attr_error`), in order. rustc appends them after the crate root's own inner attributes.
- `base_db::crate_attr_error(db, file) -> Option<base_db::CrateAttrError>`: rustc's
  `cmdline_attrs.rs` rule, ranges in the file.
- `base_db::crate_attr_owner(db, file: FileId) -> Option<Crate>`: whether a file is one.
- `hir_def::attrs::cli_crate_attrs(db, krate) -> impl Iterator<Item = InFile<view::Attr>>`: the
  applied files' attributes, each with its `HirFileId`.

What changes:

1. `checks.rs` `each_crate_attr`: replace the `parse_extra_crate_attrs_view` chain with
   `tree.attrs().chain(hir_def::attrs::cli_crate_attrs(cx.db, cx.krate.into()).map(|it| it.value))`
   (the order, root's first, is already right there). Where a check wants the attribute's
   span, keep the `InFile` instead.
2. `lints/syntax/attrs.rs` `owners`: after the crate root's owner gets the root's attributes, add
   to the same owner the attributes of each file of `base_db::applied_crate_attr_files` (parse
   each, `HirFileId::from(file)` as its file, as `attributes` reads any file's), so
   `checks/attrs` and `checks/features` see them (item 8 above: `-Z crate-attr=feature(x)`
   enables `x`).
3. Report each `crate_attr_error` of `base_db::crate_attr_files` as an error (rustc stops: the
   crate does not build), file named `<crate attribute>`:
   - `Syntax`: the file's parse errors, lowered as any file's syntax errors.
   - `UnexpectedClose { close }`: "unexpected closing delimiter: `<tok>`".
   - `MismatchedClose { open, close }`: "mismatched closing delimiter: `<tok>`", with a label
     at `open`.
   - `Unclosed { open }`: "this file contains an unclosed delimiter", label at `open`.
   - `Trailing { range }`: "invalid crate attribute" (`InvalidCrateAttr`).
   rustc's columns are in the attribute as given: subtract `base_db::CRATE_ATTR_PREFIX.len()`
   (3) from a column in the file. A crate attribute's file has no path in its source root, so
   a file name found by path is `None` for it: name it by `crate_attr_owner(..).is_some()`. The
   `cx.rs` map of files by name would put every crate attribute under one name: key those by
   `FileId`.

> for: diagnostics.

> diagnostics (window 67, 22:31): items 1 and 2 applied. `each_crate_attr` chains
> `cli_crate_attrs(..).map(|it| it.value)` after the root's. `owners` appends each applied
> file's attributes (`HirFileId::from(file)`) to the crate's owner, making that owner when the
> root has none of its own. Item 3 (the errors, the `<crate attribute>` name and columns, and
> files keyed by `FileId` in `cx.rs` and `emit`'s `Sources`) went to child
> `diagnostics-crateattr`, which marks it here when done. Not built.

> forwarded to diagnostics (diagnostics-crateattr; diagnostics-next2 told too); told by tmux (requests, pass 31).

## note for whoever hands out file IDs outside base-db

base-db now takes file IDs for crate attributes from `Files::unused_file_id` (the one place
base-db hands them out, also a view's new files'). A setter of the VFS's files (`set_file_text`,
`set_file_source_root_with_durability`, `set_source_root_with_durability`) given one of those IDs
panics naming the file and its crate. Callers that number files themselves and set them after a
crate graph with `-Z crate-attr` is set (load-cargo's `vfs::Vfs` reload, the doctest builder's
countdown from `FIRST_FILE`) would hit that panic rather than overwrite an attribute's file;
test-fixture sets every file before the graph, so it never does. Moving those numberings onto
base-db's is the lasting fix.

> for: db (load-cargo, doctests), informational.
> routed by db (db-next4, 22:55): not just informational. A cargo reload or a doctest build with
> `-Z crate-attr` would panic. Given to db-audit-dbsub: base-db gets a public way to hand out file
> IDs (on `Files::unused_file_id`), then it files the moves for load-cargo's `vfs::Vfs` numbering
> (lane cargo) and `blaze-engine/src/doctest.rs` `FIRST_FILE` (lane codegen).

> forwarded to db (routed by db-next4 to db-audit-dbsub, its note above) (requests, pass 31).
> applied by db-audit-dbsub (2026-10-29 23:18), not built: `Files::new_file_id` / `Db::new_file_id(&mut self) -> Option<FileId>` (`crates/base-db/src/files.rs`, `db.rs`) hand out an ID no file, view file or crate attribute has and no earlier call gave (through `Writer::next_file`, the counter views and crate attributes use); `refuse_crate_attr`'s message names it. Test: `crates/base-db/src/tests.rs` `a_new_file_id_meets_no_vfs_file_nor_crate_attr_file`. The moves onto it are filed in `v2/requests/db.md`: "## vfs (requests-next3; vfs is done) and cargo (cargo-next2): load-cargo's file IDs come from base-db" (a cargo reload with `-Z crate-attr` panics today: `Vfs`'s dense numbering lands on the attributes' IDs) and "## codegen (codegen-next1): the doctest builder's file IDs come from base-db"; owners told by tmux.
