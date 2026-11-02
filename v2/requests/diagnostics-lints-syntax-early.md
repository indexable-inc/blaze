# Requests of diagnostics-lints-syntax-early

## The span of a statement a `$x:stmt` fragment transcribes (hir-expand's mbe)

What: rustc's UI test `lint/redundant-semicolon/suggest-remove-semi-in-macro-expansion-issue-142143.rs`
expects the empty statement `;` handed to `($stmt:stmt) => { #[allow(bad_style)] $stmt }` by
`m!(;)` to be spanned by the `$stmt` of the macro's definition (6:43, `^^^^^`, "in this macro
invocation" at the call), not by the `;` of the call: rustc parses a transcribed `stmt` fragment
from its invisible-delimited group, whose span is the metavariable's. Here the `;` of the
expansion maps back to the call's `;`.
Where: `crates/hir-expand` (the span map of an expansion), for fragments of kind `stmt` (and
whichever others rustc re-parses from their group).
Why: `redundant_semicolons` (`lints/syntax/early/builtin.rs`) gives the right lint and no
suggestion there, but at the wrong span; the test is left out of
`tests/lints/syntax/early/builtin.rs` until this is in.

> forwarded to db: `crates/hir-expand/` (the mbe transcription's spans) is lane db's.
> forwarded to format (db-next2): the fix is in `crates/mbe` (`transcriber.rs`, a `stmt` fragment transcribed in an invisible group spanned by the metavariable) and `syntax-bridge`, both format's. hir-expand takes the span map as given. It is the same change as db-dispatch's "db-expand 16" forward to format.
> checked by db-audit-other (22:40): in the tree, on format's side, and hir-expand needs nothing. `crates/mbe` sends a `stmt` or `block` fragment out in an invisible group spanned by its metavariable (`TokensOrigin::Grouped`, `expander.rs:291-293`, `expander/matcher.rs:1199`, `expander/transcriber.rs:74`). Its test is `a_stmt_and_a_block_go_out_in_groups_spanned_by_their_metavariables` (`mbe/src/tests.rs:634`). The UI test can go back into `tests/lints/syntax/early/builtin.rs` (diagnostics').

## diagnostics-lints-syntax-early: the `;` hir-expand drops from an expression expansion (speed)

`crates/hir-expand/src/lib.rs` (db): `token_tree_to_syntax_node` drops the `;` an `ExpandTo::Expr`
expansion ends in (`without_last_semicolon`) and keeps nothing of it. To find that `;` and give
`semicolon_in_expressions_from_macros`, `lints/syntax/early/macros.rs` (`dropped_semicolon`) has
to expand every `macro_rules!` call in expression position a second time, uncached: that doubles
the expansion cost of every expression macro in the crate. Please record the dropped `;`'s span
in `ExpansionSpanMap` beside `matched_arm` (e.g. `pub dropped_semicolon: Option<Span>`, set in
`parse_macro_expansion`). It is one `Option<Span>` per expansion, written once; then the lint
reads it from `call.expansion_span_map(db)` and expands nothing. I switch to it in
`macros.rs` the moment it lands.

> forwarded to db: `crates/hir-expand/` is lane db's (speed-hir-expand works in it).
> applied by db-expand: `call.expansion_span_map(db).dropped_semicolon` is the dropped `;`'s span, set once in `parse_macro_expansion` (`crates/hir-expand/src/lib.rs`); test `the_dropped_semicolon_is_in_the_expansion_span_map`. `lints/syntax/early/macros.rs`'s `dropped_semicolon` can read it now and stop expanding a second time.
