# Requests to db-expand-macros

## speed-hir-sol-expand: hot_lint and budgets

From `speed-hir-sol-expand` (child of `speed-hir`, round 2). Every path below is yours, so these
are edits for you to apply as you go. Each block names a file and gives the old text and the new
text; text, not line numbers, because `frontend-db-def` and you are editing these files now. A
block whose old text is gone (you got there first) is done. Line numbers in parentheses are from
`hot_lint` at 20:40, for finding the place only.

Why: `PYTHONPATH=python/typed-input/src python3 tools/v2/hot_lint.py '{"root": "."}'` finds 223
lines in `crates/hir-expand` (465 before `frontend-db-def` moved the crate to `&Db`). 71 are in
`builtin/derive_macro/reference.rs`, which is `#[cfg(test)] mod reference;`: the lint misses
`#[cfg(test)] mod x;` files, which I asked of the lint's owner in `v2/requests/speed.md`
("speed-hir-sol-expand: hot_lint skips files of `#[cfg(test)] mod x;`"). Nothing to do for those
here. The other 152 are below: 6 real fixes (blocks 1 to 6) and justify-comments for the rest.

The speed-of-light notes these come from: `docs/v2/plan/lanes/db/speed-of-light.md`, section
"hir-expand: macro expansion (speed-hir-sol-expand)".

> forwarded to db (db-expand-macros-next1, which holds these paths) (from requests, pass 16; told by tmux).
> checked by db-audit-dbsub-macros (2026-10-29 22:37): in the tree: blocks 1 to 8, each checked below (8's `budget_tests.rs`, `lib.rs:32-36`, the dev-dep, `a_derive_allocates_nothing_per_field` and `macro_expansion_tests/budgets.rs` too).

### 1. `crates/hir-expand/src/lib.rs`: an eager call's argument read in place (fix)

What: `macro_arg` of an eager call (`concat!`, `env!`, `option_env!`, `include*!`,
`compile_error!`) copied the already-expanded argument out of the interned `MacroCallLoc` into
its own memo: `eager.arg.clone()`, five boxed slices allocated and copied per eager call, and the
tree held twice for the revision. Now `macro_arg_considering_derives` reads it from the location,
and returns borrowed parts. Zero allocations and no memo for an eager call's argument.

Callers checked across the workspace: inside hir-expand the four below; `hir-def`
`expr_store/expander.rs:218` takes `.2` and still compiles (a `Span` by value); one outside caller
changes, `crates/blaze-diagnostics/src/lints/syntax/early/macros.rs:148`
(`expand(db, arg, call, *span)` becomes `expand(db, arg, call, span)`), filed in
`v2/requests/diagnostics.md` under `## speed-hir-sol-expand: macro_arg_considering_derives
returns its span by value`. Land both together.

Old:
```rust
/// This is just to ensure the types of [`MacroCallId::macro_arg_considering_derives`]
/// and [`MacroCallId::macro_arg`] are the same.
type MacroArgResult = (tt::TopSubtree, SyntaxFixupUndoInfo, Span);
```
New:
```rust
/// What [`MacroCallId::macro_arg`] keeps: the argument's tree, how to undo its fixups, and the
/// call's span.
type MacroArgResult = (tt::TopSubtree, SyntaxFixupUndoInfo, Span);

/// A macro call's argument as [`MacroCallId::macro_arg_considering_derives`] reads it: from the
/// memo of `macro_arg`, or for an eager call from its interned location.
pub type MacroArg<'db> = (&'db tt::TopSubtree, &'db SyntaxFixupUndoInfo, Span);

static NO_FIXUPS: SyntaxFixupUndoInfo = SyntaxFixupUndoInfo::NONE;
```

Old:
```rust
    #[expect(deprecated, reason = "we are `macro_arg_considering_derives`")]
    pub fn macro_arg_considering_derives<'db>(
        self,
        db: &'db Db,
        kind: &MacroCallKind,
    ) -> &'db MacroArgResult {
        match kind {
            // Get the macro arg for the derive macro
            MacroCallKind::Derive { derive_macro_id, .. } => derive_macro_id.macro_arg(db),
            // Normal macro arg
            _ => self.macro_arg(db),
        }
    }
```
New:
```rust
    #[expect(deprecated, reason = "we are `macro_arg_considering_derives`")]
    pub fn macro_arg_considering_derives<'db>(
        self,
        db: &'db Db,
        kind: &MacroCallKind,
    ) -> MacroArg<'db> {
        let (tt, undo_info, span) = match kind {
            // Get the macro arg for the derive macro
            MacroCallKind::Derive { derive_macro_id, .. } => derive_macro_id.macro_arg(db),
            MacroCallKind::FnLike { eager: Some(_), .. } => {
                let MacroCallKind::FnLike { eager: Some(eager), .. } = &self.loc(db).kind else {
                    unreachable!("`kind` is this call's own");
                };
                return (&eager.arg, &NO_FIXUPS, eager.span);
            }
            _ => self.macro_arg(db),
        };
        (tt, undo_info, *span)
    }
```
(It tests the `kind` it is given before reading the location, so a call that is not eager pays
nothing for it; `kind` is `self`'s at every caller. `eager: Some` is only built for
`BuiltInEager`, in `eager.rs`.)

In `fn macro_arg`, old:
```rust
        if let MacroCallLoc {
            def: MacroDefId { kind: MacroDefKind::BuiltInEager(..), .. },
            kind: MacroCallKind::FnLike { eager: Some(eager), .. },
            ..
        } = &loc
        {
            return (eager.arg.clone(), SyntaxFixupUndoInfo::NONE, eager.span);
        }
```
New:
```rust
        if let MacroCallKind::FnLike { eager: Some(_), .. } = &loc.kind {
            unreachable!("an eager call's argument is its location's, read by `macro_arg_considering_derives`");
        }
```

In `fn macro_expand`, old:
```rust
                let (macro_arg, undo_info, span) =
                    self.macro_arg_considering_derives(db, &loc.kind);
                let span = *span;
```
New:
```rust
                let (macro_arg, undo_info, span) =
                    self.macro_arg_considering_derives(db, &loc.kind);
```

In `fn expand_speculative`, old:
```rust
        let (_, _, span) = *self.macro_arg_considering_derives(db, &loc.kind);
```
New:
```rust
        let (_, _, span) = self.macro_arg_considering_derives(db, &loc.kind);
```

`expand_proc_macro` (`let (macro_arg, undo_info, _) = ...`, then `reverse_fixups(&mut tt,
undo_info)`, which takes `&SyntaxFixupUndoInfo`) and `HirFileId::syntax_context` (`.2.ctx`)
compile unchanged.

> forwarded to db (db-expand-macros-next1, which holds these paths) (from requests, pass 16; told by tmux).
> checked by db-audit-dbsub-macros (2026-10-29 22:37): in the tree at `crates/hir-expand/src/lib.rs:94` (`MacroArg`), `:115` (`NO_FIXUPS`), `:680-692` and `:710`.

### 2. `crates/hir-expand/src/mod_path.rs`: the path callback is generic, not `dyn` (fix)

What: `ModPath::from_src`, `from_tokens` and `convert_path` call `span_for_range` through a
vtable once per segment (`convert_path`, `let syn_ctx = span_for_range(...)`) and once per
`$crate`/`crate` keyword. Generic, the caller's closure (a span-map lookup) inlines. `?Sized`
keeps every caller compiling unchanged, including hir-def `item_tree/lower.rs`, which passes a
`&mut dyn FnMut` variable on (it can then drop its own `// dyn:` lines, once the hir-def side
wants to).

Three times (in `from_src`, `from_tokens` and `fn convert_path`), old:
```rust
        span_for_range: &mut dyn FnMut(::tt::TextRange) -> SyntaxContext,
```
(`convert_path`'s is indented 4, not 8.) New:
```rust
        span_for_range: &mut (impl FnMut(::tt::TextRange) -> SyntaxContext + ?Sized),
```

Old:
```rust
    Some(InternedSlice::from_header_and_slice((), segments).to_owned())
```
New:
```rust
    // alloc: none: `to_owned` makes the interned reference an owned handle, a pointer copy
    Some(InternedSlice::from_header_and_slice((), segments).to_owned())
```

Old:
```rust
        let segments: SegmentBuf = segments.into_iter().collect();
```
New:
```rust
        // alloc: none for up to 8 segments, gathered on the stack
        let segments: SegmentBuf = segments.into_iter().collect();
```

> forwarded to db (db-expand-macros-next1, which holds these paths) (from requests, pass 16; told by tmux).
> checked by db-audit-dbsub-macros (2026-10-29 22:37): in the tree at `crates/hir-expand/src/mod_path.rs:97`, `:106`, `:128` (and `:328`, `:412`), with the comments at `:43` and `:116`.

### 3. `crates/hir-expand/src/builtin/fn_macro.rs`: `line!()`/`column!()` without a `String` (fix)

What: every `line!()` and `column!()` (and so every `log`-style macro, `assert!` message helper,
`file!()`-and-`line!()` pair) allocated a `String` to intern `"{value}u32"`. A `u32` and its
suffix fit in 13 bytes on the stack.

In `fn place_expand`, old:
```rust
    let literal = tt::Literal {
        text_and_suffix: Symbol::intern(&format!("{value}u32")),
```
New:
```rust
    let mut text = [0u8; 13];
    let mut rest = &mut text[..];
    std::io::Write::write_fmt(&mut rest, format_args!("{value}u32"))
        .expect("a `u32`'s 10 digits and its suffix fit in 13 bytes");
    let len = 13 - rest.len();
    let literal = tt::Literal {
        text_and_suffix: Symbol::intern(
            std::str::from_utf8(&text[..len]).expect("digits and `u32` are ASCII"),
        ),
```

> forwarded to db (db-expand-macros-next1, which holds these paths) (from requests, pass 16; told by tmux).
> checked by db-audit-dbsub-macros (2026-10-29 22:37): in the tree at `crates/hir-expand/src/builtin/fn_macro.rs:253-258`.

### 4. `crates/hir-expand/src/name.rs` and `proc_macro.rs`: `Symbol` is `Copy` (fix)

`name.rs`, old:
```rust
        Name::new_symbol_root((*self.name).clone())
```
New:
```rust
        Name::new_symbol_root(*self.name)
```

`name.rs`, the doc comment of `Name` is stale (`Symbol` is a 4-byte id now, not a pointer). Old:
```rust
/// 8 bytes, `Copy`: the interned symbol's pointer. Equality and hashing are one pointer;
/// ordering compares text (pointers differ between runs), equal pointers first.
```
New:
```rust
/// 4 bytes, `Copy`: the interned symbol's id. Equality and hashing are one `u32`; ordering
/// compares text (ids differ between runs), equal ids first.
```

`proc_macro.rs`, old:
```rust
                        let name = crate::name::Name::new_symbol(it.name.clone(), def_site_ctx);
```
New:
```rust
                        let name = crate::name::Name::new_symbol(it.name, def_site_ctx);
```

> forwarded to db (db-expand-macros-next1, which holds these paths) (from requests, pass 16; told by tmux).
> checked by db-audit-dbsub-macros (2026-10-29 22:37): in the tree at `crates/hir-expand/src/name.rs:20`, `:342` and `proc_macro.rs:245`.

### 5. `crates/hir-expand/src/cfg_process.rs`: the callback's empty answers named once (fix)

What: eleven `(true, Vec::new())` / `(false, Vec::new())` answers. An empty `Vec` allocates
nothing, so these were never allocations; two constants say so once instead of eleven
justifications. The callback's `Vec<tt::Leaf>` return type is `syntax-bridge`'s
(`syntax_node_to_token_tree_modified`); the two answers that insert (`#[` and `]` around an
expanded `cfg_attr` attribute) still allocate one small `Vec` each, noted in "For speed-hir" of my
report for format.

Old:
```rust
struct ItemIsCfgedOut;
```
New:
```rust
struct ItemIsCfgedOut;

/// The callback's answers that insert no tokens, keeping or skipping the token.
// alloc: none, an empty `Vec`
const KEEP: (bool, Vec<tt::Leaf>) = (true, Vec::new());
// alloc: none, an empty `Vec`
const SKIP: (bool, Vec<tt::Leaf>) = (false, Vec::new());
```

Then, every `return (true, Vec::new());` becomes `return KEEP;` (6 of them), every
`return (false, Vec::new());` becomes `return SKIP;` (3), and the closure's last line
`        (true, Vec::new())` becomes `        KEEP`.

Justify-comments in the same file (append to the line):
- `    let mut attrs = Vec::new();` → `// alloc: one list per input, reused for every node with attributes`
- `} else if !in_attr && let Some(has_attrs) = ast::AnyHasAttrs::cast(node.clone()) {` → `// clone: a node handle, a non-atomic count`
- `node_attrs.clone().for_each(|attr| {` → `// clone: an iterator of node handles`
- `for event in preorder.clone() {` → `// clone: a cursor, for a cfg'd-out item only`
- `let insert_tokens = vec![tt::Leaf::Punct(tt::Punct {` → `// alloc: the `]` after an expanded attribute: the bridge's callback takes a `Vec``

> forwarded to db (db-expand-macros-next1, which holds these paths) (from requests, pass 16; told by tmux).
> checked by db-audit-dbsub-macros (2026-10-29 22:37): in the tree at `crates/hir-expand/src/cfg_process.rs:30-31` (`KEEP`/`SKIP`, `Inserted` since frontend-tokens' syntax-bridge change).

### 6. `crates/hir-expand/src/builtin/derive_macro.rs`: one allocation per field list (fix)

What: `parse_adt_from_syntax` grows `fields` by doubling, `log2(f)` allocations for a struct of
`f` fields. Reserving each list's length first makes it one for a struct.

In `fn push_fields`, old:
```rust
        Some(FieldList::RecordFieldList(it)) => {
            for field in it.fields() {
```
New:
```rust
        Some(FieldList::RecordFieldList(it)) => {
            fields.reserve(it.fields().count());
            for field in it.fields() {
```
Old:
```rust
        Some(FieldList::TupleFieldList(it)) => {
            for (i, field) in it.fields().enumerate() {
```
New:
```rust
        Some(FieldList::TupleFieldList(it)) => {
            fields.reserve(it.fields().count());
            for (i, field) in it.fields().enumerate() {
```

Justify-comments in the same file (append to the line):
- `    let mut fields = Vec::new();` → `// alloc: the shape's field list, one per derive (each list reserved in `push_fields`)`
- `    let mut variants = Vec::new();` → `// alloc: the shape's variant list, one per derive`
- the lifetimes' `.collect();` → `// alloc: one tree per lifetime parameter; none without generics`
- the type parameters' `.collect();` (after `AdtParam { name, const_ty, bounds }`) → `// alloc: one entry per type or const parameter; none without generics`
- `Some(w) => w.predicates().map(|it| to_tt(it.syntax())).collect(),` → `// alloc: one tree per where predicate`
- `None => Vec::new(),` and the `Vec::new()` of `associated_types` → `// alloc: none, an empty `Vec``
- the three `.map(|list| list.syntax().clone()),` → `// clone: a node handle, a non-atomic count`
- the associated types' `.collect()` → `// alloc: one tree per `T::Assoc` path, only with type parameters`
- `variants.push(Variant { name: name_token.clone(), ...` → `// clone: a copy (symbol and span)`
- `let adt = ast::Adt::cast(item.syntax().clone())` → `// clone: a node handle, a non-atomic count`
- `format!("`{trait_name}` can only be derived on `struct`s"),` → `// alloc: an error's message, on the error path`
- `|_, _| (true, Vec::new()),` (in `coerce_shared_target`) → `// alloc: none, an empty `Vec``
- In `coerce_pointee_expand` and its helpers (`(type_params[0].clone(), 0)`,
  `let mut new_predicates: Vec<ast::WherePred> = Vec::new();`, `bound.clone(),`,
  `ast::Struct::cast(edit.new_root().clone())`, `ast::Adt::Struct(strukt.clone())`,
  `ast::TypeBound::cast(edit.new_root().clone())`, `ast::WherePred::cast(edit.new_root().clone())`)
  → `// clone: `CoercePointee` only, rare: node handles and its edit's lists`

> forwarded to db (db-expand-macros-next1, which holds these paths) (from requests, pass 16; told by tmux).
> checked by db-audit-dbsub-macros (2026-10-29 22:37): in the tree at `crates/hir-expand/src/builtin/derive_macro.rs:537` and `:545`.

### 7. Justify-comments, the rest (append to the line, or put on the line above)

`crates/hir-expand/src/lib.rs`:
- `ExpandError { inner: Arc::new((kind, span)) }`, `ExpandError { inner: Arc::new((ExpandErrorKind::Other(msg.into()), span)) }`, `Self::new(span, ExpandErrorKind::Builtin(Box::new(error)))` → `// alloc: one per error, on the error path`
- each of the 12 `message: ...` lines of `ExpandErrorKind::render_to_string` → `// alloc: a diagnostic's text, at the edge`
- `ExpandError { inner: Arc::new((ExpandErrorKind::Mbe(mbe.inner.1.clone()), mbe.inner.0)) }` → `// alloc: clone: one per error, on the error path`
- `Self(AttrMacroAttrIdsRepr::ManyDerives(ids.iter().copied().collect()))` → `// alloc: the list a call's interned key owns, once per attribute call with several derives`
- `res.err = error.clone().or(res.err);` → `// clone: an `Option<ExpandError>`: an `Arc` count, only when the eager input had an error`
- `ast::Attr::cast(speculative_args.clone())`, `let item = ast::Item::cast(speculative_args.clone())?;` → `// clone: a node handle; speculative expansion is IDE-only`; the `.collect();` that ends `expand_speculative` → `// alloc: the mapped tokens; IDE-only`
- in `MacroCallLoc::to_node`, the four `...syntax().clone())`; in `MacroCallKind::arg`, `Some(it.token_tree()?.syntax().clone())`; `self.expanded.clone()`; `Some(attr.with_value(ast::Attr::cast(attr.value.clone())?))` → `// clone: a node handle, a non-atomic count`
- `InFile { file_id: range.file_id.into(), value: smallvec::smallvec![range.range] }` → replace `smallvec::smallvec![range.range]` with `smallvec::SmallVec::from_buf([range.range])` (inline, no allocation, and no `vec![]` for the lint)
- `return InFile::new(self.arg.file_id, smallvec::smallvec![]);` → `smallvec::smallvec![]` becomes `smallvec::SmallVec::new()`
- the `.collect(),` in `map_range_up_once` → `// alloc: none for one range (inline); IDE up-mapping`
- the two `.collect()` lines of `syntax_error_ranges` → `// alloc: the query's value, once per file`

`crates/hir-expand/src/eager.rs`:
- `pub type EagerCallBackFn<'a> = &'a mut dyn FnMut(` → `// dyn: one call per macro call the eager input holds, not per token`
- `resolver: &dyn Fn(&ModPath) -> Option<MacroDefId>,` and `macro_resolver: &dyn Fn(&ModPath) -> Option<MacroDefId>,` → `// dyn: one call per nested macro call`
- `let err = parse_err.clone().or(err);`, `error: err.clone(),`, `err: err.clone().or_else(|| err2.clone()),` → `// clone: an `Option<ExpandError>`: an `Arc` count, only on error`
- `eager: Some(Box::new(EagerCallInfo {` → `// alloc: the eager call's interned key, once per eager call`
- `let original = curr.value.clone();`, `syntax_node.clone(),`, `let expanded = editor.finish().new_root().clone();` → `// clone: a node handle, a non-atomic count`
- `let mut replacements = Vec::new();` → `// alloc: none unless a nested call is replaced`
- `format!("unresolved macro {}", path.display(db, edition)),` → `// alloc: an error's message, on the error path`

`crates/hir-expand/src/fixup.rs`: on the line above each `remove.insert(` and each
`append.insert(` in `fixup_syntax` (22 lines, from `remove.insert(node.clone().into());` to the
last `append.insert(node.into(), vec![`) → `// alloc: clone: error path: fixups run only where a syntax error touches the input (`has_syntax_error_in`)`

`crates/hir-expand/src/proc_macro.rs`:
- `fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool;`, `impl PartialEq for dyn ProcMacroExpander {`, `impl Eq for dyn ProcMacroExpander {}` → `// dyn: comparing expanders when a crate's macros are set, not per expansion`
- `pub expander: sync::Arc<dyn ProcMacroExpander>,` → `// dyn: the edge to user code (native or wasm), one call per proc-macro expansion`
- `Ok(it) => Arc::new(CrateProcMacros(Ok(it.into_boxed_slice()))),`, `Err(e) => Arc::new(CrateProcMacros(Err(e))),` → `// alloc: once per crate, when its proc macros are loaded`
- the `format!(` in `ProcMacros::get` (`"internal error: proc-macro index out of bounds ..."`) → `// alloc: an error's message, on the error path`
- the `.collect(),` of `ProcMacros::list` → `// alloc: once per crate's def map: its proc macros' names`
- `let current_dir = calling_crate.data(db).proc_macro_cwd.to_string();` → `// alloc: one path per proc-macro call, the `String` that `ProcMacroExpander::expand_in` takes (implemented in six crates); the expansion costs far more` (the `FIXME` above it can go: the answer is in the comment)
- `value: tt.clone(),` → `// clone: error path: an attribute macro that failed keeps its item`

`crates/hir-expand/src/span_map.rs`:
- `let mut pairs = vec![(syntax::TextSize::new(0), span::ROOT_ERASED_FILE_AST_ID)];` → `// alloc: the file's span map, once per parse of the file`

`crates/hir-expand/src/files.rs`:
- `self.with_value(self.value.clone())` (in `cloned`) → `// clone: what `cloned` is asked for`
- `return Some(InRealFile { file_id, value: self.value.borrow().clone() });` → `// clone: a node handle, a non-atomic count`

`crates/hir-expand/src/attrs.rs`:
- `stack.extend(meta.metas().map(|meta| (meta, top_attr.clone())));` → `// clone: a node handle, a non-atomic count`
- `let mut buf = String::new();` (in `unescape`) → `// alloc: none until the first escape (`reserve_exact` then)`

`crates/hir-expand/src/prettify_macro_expansion_.rs`:
- `dollar_crate.clone()`, `Some(replacement.clone())` → `// clone: a token handle; display of an expansion, at the edge`

`crates/hir-expand/src/builtin/attr_macro.rs`:
- `ExpandResult::ok(tt.clone())` (in `dummy_attr_expand`) and the two `return ExpandResult::ok(tt.clone());` → `// clone: the expansion is the input, written once`
- `tt::TtElement::Leaf(tt::Leaf::Ident(ident)) => Some(ident.clone()),` → `// clone: a copy (symbol and span); `#[global_allocator]`, once per crate`

`crates/hir-expand/src/builtin/fn_macro.rs` (skip any you have already marked):
- `ExpandError::other(span, format!("`{name}`: the call is not in its file's text"));`, both `format!("failed to load file `{path_str}`")` / `format!("recursive inclusion of `{path_str}`")` lines, `"UNRESOLVED_ENV_VAR".to_owned()` → `// alloc: an error's message, on the error path`
- `b.push(template.clone().into());` → `// clone: a copy (symbol and span)`
- `String::from_utf8(out).expect("escaped bytes are ASCII")` → `// alloc: none: the escaped buffer becomes the `String``
- `let text = included_bytes(db, arg_id, path.as_str()).map(String::from_utf8_lossy);` → `// alloc: none for a UTF-8 file, a borrowed `Cow``
- `let mut tt = tt.clone();` (in `gca_expand`) → `// clone: the expansion is the input, written once`

`crates/hir-expand/src/builtin/quote.rs`:
- `crate::tt::TtElement::Leaf(leaf) => builder.push(leaf.clone()),` → `// clone: a copy (symbol and span)`
- `self.clone().to_tokens(span, builder);` → `// clone: a leaf, a number or a `Copy` handle, as the doc above says`

> forwarded to db (db-expand-macros-next1, which holds these paths) (from requests, pass 16; told by tmux).
> checked by db-audit-dbsub-macros (2026-10-29 22:37): in the tree: hot_lint over `crates/hir-expand` finds 50 lines, all in the test-only `builtin/derive_macro/reference.rs`; `lib.rs:1750`, `:1754` take `SmallVec::from_buf`/`SmallVec::new()`.

### 8. Budgets: allocation-budget tests (new code, yours to add)

hir-expand has no counting allocator. Add:

`crates/hir-expand/Cargo.toml`, under `[dev-dependencies]`:
```toml
blaze-alloc-count.workspace = true
```
(the workspace dependency exists, root `Cargo.toml:118`.)

`crates/hir-expand/src/lib.rs`, after `mod prettify_macro_expansion_;`:
```rust
#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

#[cfg(test)]
mod budget_tests;
```

New file `crates/hir-expand/src/budget_tests.rs`:
```rust
//! Allocation budgets of hir-expand's hot reads: a `Name` from a symbol that exists, a
//! `ModPath` whose segment list exists, and span lookups, in order and scattered, each
//! allocating nothing; and a call's argument converted to tokens, a constant number. Each is
//! measured warm (its symbols and lists interned once before).

use intern::Symbol;
use span::{Edition, EditionedFileId, ErasedFileAstId, FileId, Span, SpanAnchor, SyntaxContext};
use syntax::{AstNode, SourceFile, TextRange, TextSize, ast};
use syntax_bridge::DocCommentDesugarMode;

use crate::{
    mod_path::{ModPath, PathKind},
    name::{AsName, Name},
    span_map::{ExpansionSpanMap, InOrder, RealSpanMap, SpanMap},
    tt,
};

fn span(range: TextRange) -> Span {
    Span {
        range,
        anchor: SpanAnchor {
            file_id: EditionedFileId::new(FileId::from_raw(7), Edition::CURRENT),
            ast_id: ErasedFileAstId::from_raw(0),
        },
        ctx: SyntaxContext::root(Edition::CURRENT),
    }
}

#[test]
fn a_name_of_an_existing_symbol_allocates_nothing() {
    let symbol = Symbol::intern("budget_name");
    let ident = tt::Ident {
        sym: symbol,
        span: span(TextRange::empty(TextSize::new(0))),
        is_raw: tt::IdentIsRaw::No,
    };
    let warm = (Name::new_root("budget_name"), Name::new_tuple_field(3), ident.as_name());
    let names = blaze_alloc_count::assert_at_most(0, "names of existing symbols", || {
        (
            Name::new_symbol_root(symbol),
            Name::new_root("budget_name"),
            Name::new_tuple_field(3),
            ident.as_name(),
        )
    });
    assert_eq!(names.0, warm.0);
    assert_eq!(names.1, warm.0);
    assert_eq!(names.2, warm.1);
    assert_eq!(names.3, warm.2);
}

#[test]
fn a_path_whose_segments_exist_allocates_nothing() {
    let [a, b, c] = ["budget_a", "budget_b", "budget_c"].map(Name::new_root);
    let warm = ModPath::from_segments(PathKind::Plain, [a, b, c]);
    let mut shorter = warm;
    shorter.pop_segment();
    let paths = blaze_alloc_count::assert_at_most(0, "paths whose segment lists exist", || {
        let mut path = ModPath::from_segments(PathKind::Plain, [a, b, c]);
        let popped = path.pop_segment();
        let mut again = path;
        again.push_segment(c);
        (path, popped, again, ModPath::from_kind(PathKind::Crate))
    });
    assert_eq!(paths.0, shorter);
    assert_eq!(paths.1, Some(c));
    assert_eq!(paths.2, warm);
    assert_eq!(paths.3.segments(), &[] as &[Name]);
}

#[test]
fn span_lookups_allocate_nothing() {
    let file_id = EditionedFileId::new(FileId::from_raw(7), Edition::CURRENT);
    let anchors: Box<[_]> = (0..32u32)
        .map(|i| (TextSize::new(i * 40), ErasedFileAstId::from_raw(i)))
        .collect();
    let real = RealSpanMap::from_file(file_id, anchors, TextSize::new(32 * 40));
    let mut expansion = ExpansionSpanMap::empty();
    let ranges: Vec<TextRange> =
        (0..320u32).map(|i| TextRange::new((i * 4).into(), (i * 4 + 3).into())).collect();
    for &range in &ranges {
        expansion.push(range.end(), span(range));
    }
    expansion.finish();
    for map in [SpanMap::RealSpanMap(&real), SpanMap::ExpansionSpanMap(&expansion)] {
        let expected: Vec<Span> = ranges.iter().map(|&range| map.span_for_range(range)).collect();
        let found = blaze_alloc_count::assert_at_most(0, "span lookups", || {
            let in_order = InOrder::new(map);
            let mut same = true;
            for (at, &range) in ranges.iter().enumerate() {
                same &= in_order.span_for_range(range) == expected[at];
                same &= map.span_for_range(range) == expected[at];
            }
            same
        });
        assert!(found, "the in-order lookup gives the plain lookup's spans");
    }
}

/// `macro_arg`'s conversion of a call's argument from syntax to tokens: a constant number of
/// allocations however long the argument is, since the builder is sized by the subtree's length.
#[test]
fn an_argument_costs_a_constant_number_of_allocations() {
    let used = |tokens: usize| {
        let text = format!("m!({});", "a + 1, ".repeat(tokens / 4));
        let file = SourceFile::parse(&text, Edition::CURRENT).tree();
        let arg = file.syntax().descendants().find_map(ast::TokenTree::cast).expect("the argument");
        let map = RealSpanMap::absolute(EditionedFileId::new(FileId::from_raw(7), Edition::CURRENT));
        let at = span(TextRange::empty(TextSize::new(0)));
        let run = || {
            syntax_bridge::syntax_node_to_token_tree(
                arg.syntax(),
                InOrder::new(SpanMap::RealSpanMap(&map)),
                at,
                DocCommentDesugarMode::Mbe,
            )
        };
        let warm = run();
        let (again, used) = blaze_alloc_count::measure(run);
        assert!(again == warm, "{tokens} tokens");
        used.allocations
    };
    let (small, big) = (used(16), used(1024));
    assert!(small <= 12, "an argument of 16 tokens: {small} allocations, budget 12");
    assert_eq!(big, small, "1024 tokens against 16: no allocation grows with the argument");
}
```

Append to `crates/hir-expand/src/builtin/derive_macro/tests.rs` (it has `item` and `call_site`):
```rust
/// A struct of `fields` fields with a field of each kind of type the derives read.
fn wide_struct(fields: usize) -> String {
    let fields: Vec<String> = (0..fields).map(|i| format!("f{i}: u{}", 8 << (i % 4))).collect();
    format!("struct Wide {{ {} }}", fields.join(", "))
}

/// `#[derive(Clone)]` of a struct, from its syntax to its expansion (the reparse of the input
/// tree before it is the bridge's): no allocation per field. What grows with the field count is
/// the output builder's columns doubling, about 4 columns × log2 of the growth, so going from 16
/// fields to 256 adds at most 48 allocations where one per field would add 240. Measured warm:
/// the field bindings' symbols are interned by the first run.
#[test]
fn a_derive_allocates_nothing_per_field() {
    let used = |fields: usize| {
        let text = wide_struct(fields);
        let (adt, map) = item(&text);
        let span = call_site();
        let run = || super::clone_derive(span, super::parse_adt_from_syntax(&adt, &map, span), false);
        let warm = run();
        let (again, used) = blaze_alloc_count::measure(run);
        assert!(again.value == warm.value && again.err.is_none(), "{fields} fields");
        used.allocations
    };
    let (small, big) = (used(16), used(256));
    assert!(small <= 64, "`Clone` of 16 fields: {small} allocations, budget 64");
    assert!(big <= small + 48, "`Clone` of 256 fields: {big} allocations, {small} for 16");
}
```

Memo hits need a crate and its macro calls, so that test goes in hir-def's
`macro_expansion_tests` (yours too). It needs hir-def's counting allocator, requested already in
`v2/requests/db.md` ("speed-hir-store: one counting allocator for hir-def's tests"). New file
`crates/hir-def/src/macro_expansion_tests/budgets.rs`, and `mod budgets;` beside the other `mod`
lines at the top of `macro_expansion_tests/mod.rs`:
```rust
//! Allocation budgets of the expansion queries' memo hits: once a call is expanded, reading its
//! expansion, argument, span map and ast id map again allocates nothing.

use super::*;

#[test]
fn expansion_memo_hits_allocate_nothing() {
    crate::nameres::ENABLE_BUILTIN_DERIVE_FAST_PATH.set(false);
    let db = TestDB::with_files(
        r#"
//- minicore: derive, clone
macro_rules! make { ($($t:tt)*) => { fn made() { $($t)* } } }
make!(let _x = 1;);
#[derive(Clone)]
struct S { a: u8, b: u16 }
"#,
    );
    let krate = db.fetch_test_crate();
    let def_map = crate_def_map(&db, krate);
    let calls: Vec<MacroCallId> =
        def_map.modules(&db).flat_map(|module| module.1.scope(&db).all_macro_calls()).collect();
    assert!(calls.len() >= 2, "the `macro_rules!` call and the derive: {calls:?}");
    let read = |call: MacroCallId| {
        let expansion = call.parse_macro_expansion(&db);
        let (arg, _, span) = call.macro_arg_considering_derives(&db, &call.loc(&db).kind);
        let file = HirFileId::from(call);
        let _ = file.span_map(&db);
        let _ = file.ast_id_map(&db);
        (std::ptr::from_ref(expansion), std::ptr::from_ref(arg), span)
    };
    let warm: Vec<_> = calls.iter().map(|&call| read(call)).collect();
    for (at, &call) in calls.iter().enumerate() {
        let hit = blaze_alloc_count::assert_at_most(0, "an expansion's memo hits", || read(call));
        assert!(hit == warm[at], "a hit gives the memo's own values");
    }
    crate::nameres::ENABLE_BUILTIN_DERIVE_FAST_PATH.set(true);
}
```

> applied by db-expand-macros: blocks 1 to 8.
> - Blocks 1 to 6 as written. In `cfg_process.rs` the `AnyHasAttrs::cast(node.clone())` comment is at the end of its line.
> - Block 7 as written, with two changes:
>   - In `fn_macro.rs`, the `let mut tt = tt.clone();` comment is only on `gca_expand`'s; the other four of that text are in `mod tests`.
>   - Three more lines `hot_lint` found are marked: `place_expand`'s error `format!`, now a `let message`; `ProcMacros::get`'s `format!(`; and `ProcMacros::list`'s `.collect()`.
> - Block 8 as written, with one change: `macro_expansion_tests/budgets.rs` takes its calls from `builtin_errors::macro_calls`, which walks the root file's `MacroCall`s and adds the scopes' derives, instead of `all_macro_calls` over each scope. Its assert is now that it found a fn-like call and a derive.
> - `hot_lint` over `crates/hir-expand` now finds only `builtin/derive_macro/reference.rs`.
> - Not compiled or run (no builds allowed).
> - Block 1 needs blaze-diagnostics' `macros.rs:148` change (`*span` becomes `span`) to land with it.
> - `budgets.rs` needs hir-def's counting allocator (`v2/requests/db.md`, "speed-hir-store: one counting allocator for hir-def's tests", items 2 to 4, db-next2's).

> forwarded to db (db-expand-macros-next1, which holds these paths) (from requests, pass 16; told by tmux).

## frontend-tree-span: `RealSpanMap::from_file` takes `&[(TextSize, ErasedFileAstId)]`

`v2/requests/mem-format-expand.md` section 4 is applied in `crates/span/src/map.rs`:
`RealSpanMap::from_file(file_id, pairs: &[(TextSize, ErasedFileAstId)], end)` (it reads the
pairs only; it used to take a `Box<[_]>`). hir-expand does not compile until these three
one-token changes are made:

- `crates/hir-expand/src/span_map.rs:169`: `&pairs` for `pairs.into_boxed_slice()`.
- `crates/hir-expand/src/span_map.rs:219`: `&anchors` for `anchors.into_boxed_slice()`.
- `crates/hir-expand/src/budget_tests.rs:77`: `&anchors` (and `anchors` can be a `Vec` or an
  array, no `Box<[_]>` needed).

Also new there, for E8 and E13: `RealSpanMap::absolute_span(file_id, range) -> Span` (the
absolute map's span with no map made), and `SpanMap<A: Allocator + Clone = Global>` with
`clear`, `reserve`, `capacity` and `copy_in(alloc)`; `finish` no longer shrinks.
`ExpansionSpanMap = span::SpanMap` needs no change.

> forwarded by db-expand-macros (2026-10-29 21:36) to db-mem-def, which holds `span_map.rs` (E8 rewrites this code) and `budget_tests.rs` (T1) now. Told by tmux, with the new `absolute_span` and `SpanMap<A>`.

## frontend-tokens: syntax-bridge's new signatures (mem-format-expand 2a and 5), the callers outside it

frontend-tokens applied `v2/requests/mem-format-expand.md` sections 2a and 5 in
`crates/syntax-bridge/src/lib.rs` (2026-10-29, not built). The call sites below no longer compile
until each makes the change given. The hir-expand ones are E2's and E3's in
`v2/requests/mem-db-def-expand.md`, which already ask for the same.

2a: `syntax_bridge::token_tree_to_syntax_node(tt: tt::SubtreeView<'_>, entry_point,
span_to_edition)` takes a view, so each caller passes `.view()` of the tree it passes today.
`reference::token_tree_to_syntax_node` keeps `&TopSubtree`. There's also
`token_tree_to_syntax_node_in(view, entry_point, span_to_edition, alloc) -> (Parse, SpanMap<A>)`
for E11.
- `crates/hir-expand/src/lib.rs:1157`: `syntax_bridge::token_tree_to_syntax_node(tt.view(), ..)`.
- `crates/hir-expand/src/fixup.rs:567`: the same.
- Not hir-expand's (requests: please route): `crates/hir-def/src/nameres/facts/tests.rs:1142`,
  `crates/hir-def/src/macro_expansion_tests/mod.rs:399` and `:443`,
  `crates/hir-ide/src/const_macro.rs:188`: `&x` becomes `x.view()` (or `.view()` added to what
  is passed).

5: the modified conversions take hashbrown maps in any allocator, and the callback returns an
inline `SmallVec`. All three types are new in syntax-bridge:
`syntax_bridge::Inserted = SmallVec<[tt::Leaf; 3]>`,
`syntax_bridge::Appended<K, A = Global> = hashbrown::HashMap<K, Inserted, FxBuildHasher, A>`,
`syntax_bridge::Removed<K, A = Global> = hashbrown::HashSet<K, FxBuildHasher, A>`.
`syntax_node_to_token_tree_modified<SpanMap, OnEvent, A: Allocator + Clone>(node, map, append:
Appended<SyntaxElement, A>, remove: Removed<SyntaxElement, A>, ..)` and
`call_input_modified<'t, SpanMap, OnEvent, A>(node, map, append: Appended<view::NodeId, A>,
remove: Removed<view::NodeId, A>, ..)`. `OnEvent` returns `(bool, Inserted)`.
- `crates/hir-expand/src/builtin/derive_macro.rs:1098`, `cfg_process.rs:359` and `:390`,
  `fixup.rs:532` and `:550`. An `FxHashMap::default()` passed as `append` becomes
  `Appended::<_, Global>::default()`, and an `FxHashSet::default()` passed as `remove` becomes
  `Removed::<_, Global>::default()`. Give `A` explicitly: a function's type parameter takes no
  default, so `Appended::default()` alone leaves `A` to infer. A map built and filled first is
  declared as `Appended<_>`/`Removed<_>` (the alias's default is `Global`). A callback's
  `Vec<tt::Leaf>` becomes `Inserted` (`smallvec![..]` for `vec![..]`, `Inserted::new()` for
  `Vec::new()`).

Also new and additive: `syntax_bridge::call_input_in(node: view::Node, map, call_site, mode,
alloc) -> tt::TopSubtree<A>`, a task's temporary tree (once `tt::TopSubtree<A>` from section 1
lands, which `frontend-tokens-alloc` is applying now). And
`mbe::DeclarativeMacro::expand_making` (the `${concat}` spans, task 17; see `db-expand.md`).

Status: applied (hir-expand); two outside callers to route

> applied by db-expand-macros (2026-10-29, not built), the hir-expand call sites and my two in `macro_expansion_tests/mod.rs`:
> - 2a: `.view()` is passed in `lib.rs` (`token_tree_to_syntax_node`), in `fixup.rs`'s test, and at `macro_expansion_tests/mod.rs` `IdentityWhenValidProcMacroExpander` and the `quote!` test.
> - 5: `SyntaxFixups` holds `Appended<K>`/`Removed<K>`, filled with `smallvec![..]`. `cfg_process.rs`'s callbacks return `Inserted` (`KEEP`/`SKIP` use `Inserted::new_const()`). `derive_macro.rs` passes `Removed::<_>::from_iter`, `Appended::<_, Global>::default()` and `Inserted::new()`. `fixup.rs`'s test callbacks return `Inserted::new()`.
> - These are also E2's and E3's syntax-bridge call sites in `mem-db-def-expand.md`; db-mem-def has been told.
> - Not mine, left for requests to route: `hir-def/src/nameres/facts/tests.rs:1142` and `hir-ide/src/const_macro.rs:188`.
