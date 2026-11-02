# Requests to diagnostics-speed-lints

## dedyn: cx.rs: keep `&dyn Files`, justified on the line

From `dedyn-diag` (report `/tmp/blaze-lanes/v2-dedyn-diag.md`), 2026-10-29, based on the files
as of 21:00.

`Files::name` is called once per span built (`Cx::file_name`), and every call already returns a
fresh `String`; the vtable call is noise beside that. It has three implementations (blaze-cargo's
`Names`, `NoSpans` in `checks/features/library.rs`, the tests' `FxHashMap`), and making `Cx`
generic over it would copy every pass of the crate per implementation. It stays, commented.

In `crates/blaze-diagnostics/src/cx.rs`, the field of `Cx`:

```rust
    files: &'a dyn Files,
```

becomes

```rust
    files: &'a dyn Files, // dyn: called once per span named, beside the String it returns; a generic `Cx` would copy every pass
```

and the parameter of `Cx::new`:

```rust
        files: &'a dyn Files,
```

becomes

```rust
        files: &'a dyn Files, // dyn: the caller's file names, read once per span named
```

> forwarded to diagnostics-speed-lints (window diagnostics-speed-lints-next1) (requests, pass 19; told by tmux).

## dedyn: lints.rs: `|it: &dyn HasName|` to a generic `fn`

In `body_item` (`crates/blaze-diagnostics/src/lints.rs`), before:

```rust
            let name = |it: &dyn HasName| it.name().map(|it| it.text().to_string());
```

after:

```rust
            fn name(it: &impl HasName) -> Option<String> {
                it.name().map(|it| it.text().to_string())
            }
```

Call sites (`name(&it)?`, `name(&it).unwrap_or_else(..)`) unchanged. One two-call instance per
AST type named (four or five).

> forwarded to diagnostics-speed-lints (requests, pass 19; told by tmux).

## dedyn: lints/names/items.rs: `|it: &dyn HasName|` to a generic `fn`

Also named by `diagnostics-lints-names`' brief; told too. In `item_id`
(`crates/blaze-diagnostics/src/lints/names/items.rs`), before:

```rust
    let name = |it: &dyn HasName| it.name().map_or_else(|| "_".to_owned(), |it| it.text().to_string());
```

after:

```rust
    fn name(it: &impl HasName) -> String {
        it.name().map_or_else(|| "_".to_owned(), |it| it.text().to_string())
    }
```

Call sites (`ast::Item::Fn(it) => name(it)`, ...) unchanged; `it` is already a reference. One
small instance per item kind.

> forwarded to diagnostics-speed-lints (requests, pass 19; told by tmux).

## dedyn: lints/syntax/check_cfg/unexpected.rs: `cargo_help`'s `&dyn Fn` to `&impl Fn`

Before:

```rust
    fn cargo_help(&self, diag: &mut Diag, arg: &dyn Fn(bool) -> String) {
```

after:

```rust
    fn cargo_help(&self, diag: &mut Diag, arg: &impl Fn(bool) -> String) {
```

Its two callers (`self.cargo_help(&mut diag, &arg)` for a name and for a value) pass different
closures: two instances of a dozen lines, on the path of a diagnostic being written.

> forwarded to diagnostics-speed-lints (requests, pass 19; told by tmux).

> dedyn-diag, 2026-10-29 (later): applied by dedyn-diag under the new rule (held files may be edited for the dyn change); see /tmp/blaze-lanes/v2-dedyn-diag.md. The `// dyn:` keeps proposed for `cx.rs` were replaced by a concrete `Files`: no `dyn` remains.

> diagnostics-speed-lints, 2026-10-29: checked in the tree: no `dyn HasName`, `dyn Fn(bool)` or `dyn Files` is left in `lints.rs`, `lints/` or `cx.rs`. All four done.

## From diagnostics-speed-lints: to the parent (`diagnostics`)

1. `tools/v2/hot_lint.py` cannot run: it imports `typed_input`, and no `typed_input.py` is in
   the tree any more (`ModuleNotFoundError`). Whoever moved it should restore it or point
   the import at its new place. Until then, no lane can recheck its findings.
2. For the speed-of-light section's last 10% (front end, not ours to change): the syntax walk
   (`lints::syntax::Walk::add`) costs about 10 ms of rowan cursors on the reference crate. It
   should move to the flat syntax view when that view can walk an expanded crate in pre-order
   with kinds and ranges. Nothing to do in diagnostics until then.

> forwarded to diagnostics (diagnostics-next1) for 2; answer to 1: `typed_input` is `python/typed-input/src/typed_input`, so run `PYTHONPATH=python/typed-input/src python3 tools/v2/hot_lint.py ..` (it runs; requests used it this pass) (requests, pass 21; told by tmux).
