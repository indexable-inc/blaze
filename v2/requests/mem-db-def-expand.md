# Requests of `mem-db-def-expand` (child of `mem-db-def`, lane `mem`)

`crates/hir-expand/` onto `crates/blaze-mem`, against mem-db's contract (`docs/v2/plan/lanes/mem/db.md`,
"The contract"). Notes and the speed-of-light bound: `docs/v2/plan/lanes/mem/db-def-expand.md`.

Owners:
- **E1 to E14, T1, T2**: `crates/hir-expand/` belongs to `db-expand-macros` (window
  `db-expand-macros-next1`). T2 adds a file to hir-def's `macro_expansion_tests/`, which is
  `db-expand-next2`'s; the test is hir-expand's, so either may apply it.
- **F1 to F8**: types of lane `format` (`crates/tt/`, `crates/span/`, `crates/syntax-bridge/`,
  `crates/mbe/`, `crates/blaze-radix/` via `blaze-ids`). Format reports DONE, so the coordinator
  or format's next agent applies them. Lane mem's agent for those crates is `mem-format`
  (window `mem-format`, children `mem-format-tokens`, `mem-format-tree`): F6 is its request as
  filed, F7 builds on `mem-format-tokens`', and F3 leaves the sink's string to it.
- **X1, X2**: one line each in `crates/blaze-diagnostics/` (lane `diagnostics`), and the
  `ProcMacroExpander` implementations in `crates/load-cargo/` and `crates/test-fixture/`.

State of the tree on 2026-10-29 (read from `git diff crates/hir-expand`): already applied are
H1 to H3 (manifest lines, `#![feature(allocator_api)]`, the counting allocator), E7 (the
`RANGE_SORT` scratch), E9 (`line_starts` counted, then exact), and block 8's
`src/budget_tests.rs`. Still to apply: E1 to E6, E8, E10 to E14, T1, T2, F1 to F7, F9, X1, X2.
Order: F1 to F7 and F9 first (the types), then E2 to E6, E8, E10 to E14 (hir-expand against
them), then X1, X2 (callers outside), then T1, T2.

Filed elsewhere, assumed here:
- `v2/requests/mem-db-def.md` H1 (`blaze-mem`, `hashbrown` and `blaze-alloc-count` in hir-expand's
  manifest), H2 (`#![feature(allocator_api)]` in `crates/hir-expand/src/lib.rs`), H3 (the counting
  allocator in `src/lib.rs` under `#[cfg(test)]`).
- `blaze_mem::frame::task` (`crates/blaze-mem/src/frame.rs:453`), in the tree.
- `v2/requests/db-expand-macros.md`, section "speed-hir-sol-expand" (blocks 1 to 8): an eager
  call's argument read in place, the generic path callback, `line!`/`column!` without a `String`,
  the cfg callback's `KEEP`/`SKIP` constants, the derive's reserved field lists, the
  justification comments, and the budget tests in `src/budget_tests.rs` and
  `hir-def/src/macro_expansion_tests/budgets.rs`. Where a change below touches the same lines,
  it says which text it starts from. Apply those blocks first.

Two rules decide where each temporary goes (mem-db-def's rules 1 and 3):
- **A query body** (`#[salsa::tracked]`) opens `blaze_mem::frame::task(blaze_mem::Tag::Expand,
  |frame| ..)` at its top and passes `frame: &'f Frame` down. Not `frame::current`: salsa's
  `execute` opens no frame (`vendor/salsa` uses blaze-mem only for memo tables), so `current`
  would stop outside a task, and a nested query's miss would allocate into the outer query's
  frame. A task costs two thread-local reads once the thread has a frame at that depth. Every function that takes a `frame` is reached only from
  a query body or from one of the two entry points below that open their own task.
- **Code reached from outside a run** (`expand_speculative`, `find_derive_range`, eager
  expansion from hir-def's `macro_call_as_call_id`, span mapping through `ExpansionInfo` and
  `InFile`) never calls `current`. The two that build trees (`expand_speculative`,
  `find_derive_range`) run in a `frame::task(Tag::Expand, ..)` of their own. A task nested in a
  run gets a frame one level deeper (contract 1), so this is right inside or outside a query.
  The others use the worker's scratch (`blaze_mem::scratch`, or a `thread_local!`
  `Scratch<T>`/free list).

---

## E1. The tag of every hir-expand query (for `mem-db-salsa`'s tracked attribute)

Every tracked function in hir-expand runs under `Tag::Expand`. mem-db-salsa writes the
attribute syntax; the table is what it sets:

| query | file | tag | opens `frame::task` |
|---|---|---|---|
| `MacroCallId::parse_macro_expansion` | `src/lib.rs` | `Expand` | yes (E2) |
| `MacroCallId::macro_arg` | `src/lib.rs` | `Expand` | yes (E3) |
| `MacroCallId::expand_proc_macro` | `src/lib.rs` | `Expand` | no: its temporaries are the expander's (E4) |
| `proc_macro_span` (inner fn) | `src/lib.rs` | `Expand` | no temporaries |
| `HirFileId::ast_id_map` | `src/lib.rs` | `Expand` | no: `from_source` opens its own task (E6) |
| `HirFileId::syntax_error_ranges` | `src/lib.rs` | `Expand` | no: sort scratch from the worker (E7) |
| `real_span_map` | `src/span_map.rs` | `Expand` | yes (E8) |
| `AstId<ast::Macro>::decl_macro_expander` | `src/declarative.rs` | `Expand` | yes (E5) |
| `line_starts` | `src/builtin/fn_macro.rs` | `Expand` | no temporaries (E9) |
| `ProcMacros::get_for_crate` | `src/proc_macro.rs` | `Expand` | no temporaries |
| `may_have_session_macros` | `src/proc_macro.rs` | `Expand` | no temporaries |

`MacroCallId` (`#[salsa::interned]`) and `ProcMacros` (`#[salsa::input]`) are tables: their
storage is `Tag::Intern`/`Tag::Query` regions under contract 5, mem-db-salsa's.

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).
> applied by db-audit-mem (22:39): salsa M2 is in, so `mem_tag = Expand` on all ten tracked attributes: crates/hir-expand/src/lib.rs (`parse_macro_expansion`, `macro_arg`, `proc_macro_expansion`, `proc_macro_span`, `ast_id_map`, `syntax_error_ranges`), `span_map.rs` (`real_span_map`), `declarative.rs` (`decl_macro_expander`), `proc_macro.rs` (`get_for_crate`, `may_have_session_macros`). `line_starts` is gone (E9). Holder db-expand-macros-next2 cleared the lines.

## E2. `parse_macro_expansion`: one frame, the expansion tree a frame value

**What changes.** The expansion tree (`macro_expand`'s `Cow::Owned(tt)`) is a temporary: the
query parses it and drops it. Today it is a `tt::TopSubtree`, five boxed columns from the global
allocator per macro call (`nodes`, `spans`, `parts`, `symbols`, and `wide` when a span is wide),
each built from builder vectors (`storage.rs`, `TopSubtreeBuilder::finish`). After, it is a
`tt::TopSubtree<&'f Frame>` (F1), so an expansion costs its owned result and nothing else. An
expression expansion ending in `;` also built a second tree (`without_last_semicolon`); that tree
moves to the frame too.

Made: once per macro call expanded (per run). Frame from: the query body.

`crates/hir-expand/src/lib.rs`, after `type MacroArgResult = ...;` (or after block 1's `MacroArg`):
```rust
/// A macro's expansion tree in one run of [`MacroCallId::parse_macro_expansion`]: a proc macro's
/// memo or an eager call's input, read in place, or a tree built in the run's frame.
pub(crate) enum Expansion<'db, 'f> {
    Borrowed(&'db tt::TopSubtree),
    Framed(tt::TopSubtree<&'f Frame>),
}

impl Expansion<'_, '_> {
    pub(crate) fn view(&self) -> tt::SubtreeView<'_> {
        match self {
            Expansion::Borrowed(it) => it.view(),
            Expansion::Framed(it) => it.view(),
        }
    }
}
```
and in the `use` list: `use blaze_mem::{Frame, Tag, frame};`.

`parse_macro_expansion`, old:
```rust
        let _p = tracing::info_span!("parse_macro_expansion").entered();
        let loc = self.loc(db);
        let expand_to = loc.expand_to();
        let mbe::ValueResult { value: (tt, matched_arm), err } = self.macro_expand(db, loc);

        let (parse, mut rev_token_map, semicolon) = token_tree_to_syntax_node(db, &tt, expand_to);
        rev_token_map.matched_arm = matched_arm;
        let err = err.or_else(|| ignored_semicolon(db, loc, semicolon));

        ExpandResult { value: (parse, rev_token_map), err }
```
new:
```rust
        let _p = tracing::info_span!("parse_macro_expansion").entered();
        blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
            let loc = self.loc(db);
            let expand_to = loc.expand_to();
            let mbe::ValueResult { value: (tt, matched_arm), err } =
                self.macro_expand(db, frame, loc);

            let (parse, mut rev_token_map, semicolon) =
                token_tree_to_syntax_node(db, frame, tt.view(), expand_to);
            rev_token_map.matched_arm = matched_arm;
            let err = err.or_else(|| ignored_semicolon(db, loc, semicolon));

            ExpandResult { value: (parse, rev_token_map), err }
        })
```

`macro_expand`: signature old
```rust
    fn macro_expand<'db>(
        self,
        db: &'db Db,
        loc: &MacroCallLoc,
    ) -> ExpandResult<(Cow<'db, tt::TopSubtree>, MatchedArmIndex)> {
```
new
```rust
    fn macro_expand<'db, 'f>(
        self,
        db: &'db Db,
        frame: &'f Frame,
        loc: &MacroCallLoc,
    ) -> ExpandResult<(Expansion<'db, 'f>, MatchedArmIndex)> {
```
and in its body: `Cow::Borrowed(it)` becomes `Expansion::Borrowed(it)` (proc macro, eager input);
each expander call gains `frame` as its second argument (`id.decl_macro_expander(db,
loc.def.krate).expand(db, frame, arg, self, span)`, `it.expand(db, frame, self, arg, span)` for
`BuiltIn`, `BuiltInDerive`, `BuiltInEager`, `BuiltInAttr`), `expand_unimplemented_builtin_macro(span)`
becomes `expand_unimplemented_builtin_macro(frame, span)`, `fixup::reverse_fixups(&mut res.value,
undo_info)` is unchanged (F1 makes it generic), and the last line
`ExpandResult { value: (Cow::Owned(tt), matched_arm), err }` becomes
`ExpandResult { value: (Expansion::Framed(tt), matched_arm), err }`. `use std::borrow::Cow` goes if
nothing else uses it.

`expand_unimplemented_builtin_macro`, new:
```rust
fn expand_unimplemented_builtin_macro(
    frame: &Frame,
    span: Span,
) -> ExpandResult<tt::TopSubtree<&Frame>> {
    ExpandResult::new(
        tt::TopSubtree::empty_in(tt::DelimSpan::from_single(span), frame),
        ExpandError::other(span, "this built-in macro is not implemented"),
    )
}
```

`token_tree_to_syntax_node` and `without_last_semicolon`, new (the parse reads a view, F2):
```rust
pub(crate) fn token_tree_to_syntax_node(
    db: &Db,
    frame: &Frame,
    tt: tt::SubtreeView<'_>,
    expand_to: ExpandTo,
) -> (Parse<SyntaxNode>, ExpansionSpanMap, Option<Span>) {
    let entry_point = match expand_to { /* unchanged */ };
    let unterminated = match expand_to {
        ExpandTo::Expr => without_last_semicolon(frame, tt),
        _ => None,
    };
    let (tt, semicolon) = match &unterminated {
        Some((tt, semicolon)) => (tt.view(), Some(*semicolon)),
        None => (tt, None),
    };
    let (parse, map) =
        syntax_bridge::token_tree_to_syntax_node(tt, entry_point, &mut |ctx| ctx.edition(db));
    (parse, map, semicolon)
}

fn without_last_semicolon<'f>(
    frame: &'f Frame,
    tt: tt::SubtreeView<'_>,
) -> Option<(tt::TopSubtree<&'f Frame>, Span)> {
    let mut rest = tt.token_trees().iter();
    /* the loop unchanged */
    match last? {
        tt::TtElement::Leaf(tt::Leaf::Punct(punct)) if punct.char == ';' => Some((
            tt::TopSubtree::from_token_trees_in(tt.top_subtree().delimiter, before, frame),
            punct.span,
        )),
        _ => None,
    }
}
```
Every other caller of `token_tree_to_syntax_node` passes its frame and a view:
`expand_speculative` (E10) and `builtin/derive_macro.rs` `to_adt_syntax` (E11).

Test: T2 (b), a miss of `parse_macro_expansion` held to its owned result.

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).
> partly applied by db-mem-def (22:20; `lib.rs` handed over, read against task 17's `MadeIdents` and `proc_macro_expansion`): `parse_macro_expansion` runs in `frame::task(Tag::Expand, ..)`. `token_tree_to_syntax_node(db, frame, tt: SubtreeView, expand_to)` and `without_last_semicolon(frame, view)` build the semicolon split's tree in the frame (`from_token_trees_in`). The derive callers (`to_adt_syntax`, the `CoercePointee` target) wrap the parse in a task of their own until E11 gives them the frame. Not applied: `Expansion<'db, 'f>` and the expanders' `frame` argument. `macro_expand` keeps `Cow` until expanders return frame trees: the declarative one waits on mbe `expand_making_in` (filed in `v2/requests/db.md` for frontend-tokens, told by tmux), and the builtins on E11 and E12's frame halves.
> applied further by db-mem-def (22:34; mbe `expand_making_in` in): `macro_expand(db, frame, loc)` returns `Expansion<'db, 'f>`, and `Cow` is gone. A `macro_rules!` expansion is `Framed`, built in `parse_macro_expansion`'s frame, and its `MadeIdents` still reach `rev_token_map.made_idents` (task 17). Differs: `Expansion` has a third variant, `Owned(tt::TopSubtree)`, for the builtin, derive, eager and attribute expanders and `expand_unimplemented_builtin_macro`, until E11 and E12's frame halves make them `Framed`.

## E3. `macro_arg`: one frame for the attribute input's walk and fixups

**What changes.** The `FnLike` path's tree is the memo, owned (it stays `tt::TopSubtree`, made
exact by F9). The `Attr` path runs `attr_macro_input_to_token_tree`, whose temporaries are the
cfg callback's attribute list (`cfg_process.rs`, `let mut attrs = Vec::new();`, one per input,
grown by doubling), the callback's inserted leaves (`Vec::with_capacity(3)` and `vec![..]`, one per
expanded `cfg_attr` attribute, twice), and when the input has a syntax error the fixup maps
(`fixup.rs`, `FxHashMap<SyntaxElement, Vec<Leaf>>` and `FxHashSet<SyntaxElement>`, plus one `Vec`
per fixup). After, the list and the maps are frame values and the leaves inline (F5).
`SyntaxFixupUndoInfo.original` is part of the memo: built today in a `ThinVec` grown by
doubling and then `shrink_to_fit` (`fixup.rs`, `original.shrink_to_fit();`), after in a frame
vector copied once into a `ThinVec` of its exact length.

Made: once per attribute or derive call's argument (per run); the fixups only where a syntax
error touches the item (`has_syntax_error_in`). Frame from: the query body.

`macro_arg`, old first lines:
```rust
    fn macro_arg(self, db: &Db) -> MacroArgResult {
        let loc = self.loc(db);
```
new: the whole body moves into `blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| { .. })`, and the `Attr` arm's call becomes
```rust
        let (mut tt, undo_info) = attr_macro_input_to_token_tree(
            frame,
            db,
            item_node.syntax(),
            map,
            span,
            is_derive,
            censor_item_tree_attr_ids,
            loc.krate,
            fix_errors,
        );
```
(the early `return`s inside the closure return from the closure, which is the function's value).

`crates/hir-expand/src/cfg_process.rs` (starting from block 5's text):
- `use blaze_mem::Frame;` and `use smallvec::SmallVec;` (already imported).
- The callback's type, old
  `) -> impl FnMut(&mut PreorderWithTokens, &WalkEvent<SyntaxElement>) -> (bool, Vec<tt::Leaf>) {`
  new
  `) -> impl FnMut(&mut PreorderWithTokens, &WalkEvent<SyntaxElement>) -> (bool, syntax_bridge::Inserted) + 'f {`
  with `fn macro_input_callback<'f>(frame: &'f Frame, db: &'f Db, is_derive: bool,
  censor_item_tree_attr_ids: &'f [AttrId], krate: Crate, default_span: Span, span_map: SpanMap<'f>)`.
- Block 5's constants become
  ```rust
  const KEEP: (bool, syntax_bridge::Inserted) = (true, SmallVec::new_const());
  const SKIP: (bool, syntax_bridge::Inserted) = (false, SmallVec::new_const());
  ```
- `let mut attrs = Vec::new();` becomes `let mut attrs: Vec<AstAttrToProcess, &'f Frame> = Vec::new_in(frame);`
  (`attrs.clear()` keeps its room; one frame block per input however many nodes have attributes).
- `let mut insert_tokens = Vec::with_capacity(3);` becomes `let mut insert_tokens = syntax_bridge::Inserted::new();`
  (`#`, `!`, `[`: at most three, inline).
- `let insert_tokens = vec![tt::Leaf::Punct(tt::Punct { char: ']', .. })];` becomes
  `let insert_tokens = smallvec::smallvec![tt::Leaf::Punct(tt::Punct { char: ']', .. })];` (inline).
- `attr_macro_input_to_token_tree` takes `frame: &Frame` first, and:
  ```rust
      let fixups = if fix_errors {
          fixup::fixup_syntax(frame, span_map, node, span, DocCommentDesugarMode::ProcMacro)
      } else {
          fixup::SyntaxFixups::new_in(frame)
      };
  ```
  and passes `macro_input_callback(frame, db, ..)`.

`crates/hir-expand/src/fixup.rs`:
```rust
pub(crate) struct SyntaxFixups<'f> {
    pub(crate) append: syntax_bridge::Appended<&'f Frame>,
    pub(crate) remove: syntax_bridge::Removed<&'f Frame>,
    pub(crate) undo_info: SyntaxFixupUndoInfo,
}

impl<'f> SyntaxFixups<'f> {
    pub(crate) fn new_in(frame: &'f Frame) -> Self {
        SyntaxFixups {
            append: syntax_bridge::Appended::with_hasher_in(FxBuildHasher, frame),
            remove: syntax_bridge::Removed::with_hasher_in(FxBuildHasher, frame),
            undo_info: SyntaxFixupUndoInfo::NONE,
        }
    }
}
```
(`#[derive(Debug, Default)]` goes; `use rustc_hash::FxBuildHasher;` replaces `FxHashMap, FxHashSet`).
In `fixup_syntax(frame: &'f Frame, span_map, node, call_site, mode) -> SyntaxFixups<'f>`:
- `let mut append = FxHashMap::<SyntaxElement, _>::default();` and the `remove` line become
  `let SyntaxFixups { mut append, mut remove, .. } = SyntaxFixups::new_in(frame);`
- `let mut original = ThinVec::new();` becomes `let mut original: Vec<TopSubtree, &Frame> = Vec::new_in(frame);`
- every `vec![ .. ]` value of `append.insert(..)` becomes `smallvec::smallvec![ .. ]` (the
  longest is `for`'s three leaves, inline in `Inserted`).
- `original.shrink_to_fit();` and the result become:
  ```rust
      let needs_fixups = !append.is_empty() || !original.is_empty();
      let original = needs_fixups.then(|| {
          let mut owned = ThinVec::with_capacity(original.len());
          owned.extend(original.drain(..));
          owned
      });
      SyntaxFixups { append, remove, undo_info: SyntaxFixupUndoInfo { original } }
  ```
The trees in `original` stay global (`TopSubtree`): they are the memo's, owned.
Block 7's justification for fixup's `insert` lines is no longer needed once this lands: nothing
in them allocates globally.

The test module's `check` passes a frame: `blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| { .. })`
around its body, `super::fixup_syntax(frame, ..)`, and `|_, _| (true, syntax_bridge::Inserted::new())`.

Test: T2 (c), a miss of `macro_arg` for a derive is its tree's columns and salsa's run cost.

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).
> applied by db-mem-def (22:10; db-expand-macros-next2 handed over `cfg_process.rs` and `fixup.rs`): E3's frame values. `macro_input_callback(_handle)` takes `frame` first and keeps `attrs` in it, and the inserted leaves were already `Inserted`. `SyntaxFixups<'f, K>` has `Appended/Removed<K, &'f Frame>` and `new_in(frame)` in place of `Default` (no `Debug`), `fixup_syntax(_handle)(frame, ..)` with `original` in a frame vector copied once into an exact `ThinVec`. Differs: `macro_arg` is in `lib.rs`, which db-expand-macros holds, so `attr_macro_input_to_token_tree(_handle)` open `frame::task(Tag::Expand, ..)` themselves with their signatures unchanged. Moving the task up to `macro_arg` is the remaining step, once `lib.rs` is free. The fixup tests take a frame from `blaze_mem::Frame::new(Tag::Expand)`, so their bodies aren't re-indented. The `// alloc: clone:` notes on the `insert` lines stay, for the handle `.clone()`s.

## E4. `expand_proc_macro`: the expander's tree is the memo; `current_dir` borrowed

The memo is the tree the expander returns (owned, at the edge to user code). Its one hir-expand
temporary is `current_dir`, a `String` per proc-macro call (`proc_macro.rs`,
`calling_crate.data(db).proc_macro_cwd.to_string()`), made because `ProcMacroExpander::expand`
takes `current_dir: String`. After, it takes `&str` and the native expander reads it in place; the
IPC client copies it only where it writes it to the wire.

`crates/hir-expand/src/proc_macro.rs`: in `ProcMacroExpander::expand` and `expand_in`,
`current_dir: String,` becomes `current_dir: &str,`. In `CustomProcMacroExpander::expand`, old
```rust
                // FIXME: Can we avoid the string allocation here?
                let current_dir = calling_crate.data(db).proc_macro_cwd.to_string();
```
new
```rust
                let current_dir = calling_crate.data(db).proc_macro_cwd.as_str();
```
(block 7's justification for this line is then not needed). Implementations to change with it
are X2.

`reverse_fixups` on the memo (only with an error in the input) rebuilds it through
`transform_tt` (F1 keeps that on the global allocator for a `TopSubtree<Global>`): the memo's
columns once more, on the error path.

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:39): in the tree: db-apply-def's `ProcMacroCall { current_dir: &'a str, .. }` (crates/hir-expand/src/proc_macro.rs:36), filled from `proc_macro_cwd.as_str()` at :434.

## E5. `decl_macro_expander`: the definition's token trees in the frame

**What changes.** The `macro_rules!` body (or `macro`'s args and body) is converted to a
`tt::TopSubtree` only for `mbe::DeclarativeMacro::parse_macro_rules`/`parse_macro2` to read; it is
dropped when the expander is built. Today five global columns per tree, after none (F1). The
expander (`DeclarativeMacroExpander { mac, transparency, edition }`) is the memo, owned; its rules
are built by mbe (F7 makes them exact).

Made: once per macro definition (per run). Frame from: the query body.

`crates/hir-expand/src/declarative.rs`, `decl_macro_expander`'s body inside
`blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| { .. })`; each of its three `syntax_bridge::syntax_node_to_token_tree(`
becomes `syntax_bridge::syntax_node_to_token_tree_in(` with `frame` as its last argument. Nothing
else changes: `parse_macro_rules(&tt, ctx_edition)` and `parse_macro2(args.as_ref(), &body,
ctx_edition)` take `&TopSubtree<A>` (F1).

`DeclarativeMacroExpander::expand` gains the run's frame for mbe's matcher and transcriber
(F7), and returns the expansion in it:
```rust
    pub fn expand<'f>(
        &self,
        db: &Db,
        frame: &'f Frame,
        tt: &tt::TopSubtree,
        call_id: MacroCallId,
        span: Span,
    ) -> ExpandResult<(tt::TopSubtree<&'f Frame>, Option<u32>)> {
```
with `tt::TopSubtree::empty_in(tt::DelimSpan { open: span, close: span }, frame)` on the error arm
and `self.mac.expand_in(frame, db, tt, |s| { .. }, loc.kind.call_style(), span)`.
`expand_unhygienic` the same (`frame` second, `expand_in(frame, db, tt, |_| (), call_style, call_site)`).
Callers: `macro_expand` (E2), `expand_speculative` (E10), and X1.

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).
> applied by db-mem-def (22:10; `declarative.rs` handed over): first half. `decl_macro_expander` runs in `frame::task(Tag::Expand, ..)` and reads the definition as views (`AstId::to_view`, `expand_cfg_attr_view`, view `AnyHasAttrs`), with its trees made by `syntax_bridge::call_input_in(.., frame)`. frontend-tokens didn't add `syntax_node_to_token_tree_in`, because new code uses views. Not applied: `expand`/`expand_unhygienic` gaining `frame`, which waits on `lib.rs` (E2, E10).
> applied further by db-mem-def (22:34): `expand_making(db, frame, tt, call_id, span)` returns `TopSubtree<&'f Frame>` through `mbe::expand_making_in`, with `empty_in` on the error arm. Differs: `expand` stays global, because blaze-diagnostics' caller (`lints/syntax/early/macros.rs:152`, X1) isn't lane db's. Both share one `marker` helper for the hygiene. `expand_unhygienic` waits on E10's framed speculative expansion.
> applied by db-audit-mem (23:10; hir-expand free, db-expand holds nothing): the second half: `expand_unhygienic(db, frame, tt, call_style, call_site) -> ExpandResult<tt::TopSubtree<&'f Frame>>` through `mbe`'s `expand_in(frame, ..)`, with `empty_in` on the error arm (`crates/hir-expand/src/declarative.rs:105`). `expand` stays global for blaze-diagnostics' caller (X1, forwarded).

## E6. `ast_id_map`: no change in hir-expand

`HirFileId::ast_id_map`'s body is `AstIdMap::from_source(&self.parse_or_expand(db))`; the
temporaries are span's: `entries` grown by doubling and shrunk into the result, `found`
(bodies), the next-index map. `v2/requests/mem-format.md` ("What allocates today, per
`AstIdMap::from_source`", line 905 on) already moves them: `from_source` runs its walk in a
`frame::task(Tag::Parse, ..)` of its own and builds the result with `Box::from(&entries[..])`.
Inside this query run that task nests (contract 1: a frame one level deeper, reset on return),
so the body stays as it is and the bytes count under `Tag::Parse`, the ast ids' subsystem.
Made: once per file parse or expansion. F6 is that request; nothing more is asked.

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:39): nothing asked here; `AstIdMap::from_source` runs in its own `frame::task(Tag::Parse, ..)` (crates/span/src/ast_id.rs:1262).

## E7. `syntax_error_ranges`: exact already; the sort's scratch from the worker

The result is collected from a slice's `map` (an exact-size iterator), so the `Vec` is allocated
at its length and `into_boxed_slice` does not reallocate: owned and exact. The temporary is
`radix::sort_copy_by_key`'s scratch (`blaze-radix/src/lib.rs`, `Radix::AMBIENT.sort_copy_by_key`
runs with `&mut Vec::new()`), one global allocation per file with more than one error. After, the
worker's scratch, which `sort_copy_by_key_in` takes:

`crates/hir-expand/src/lib.rs`, after the imports:
```rust
thread_local! {
    /// The scratch of the radix sorts of ranges, kept by the worker.
    static RANGE_SORT: std::cell::RefCell<blaze_mem::scratch::Scratch<TextRange>> =
        const { std::cell::RefCell::new(blaze_mem::scratch::Scratch::new()) };
}
```
old
```rust
        radix::sort_copy_by_key(&mut ranges, |range| {
            radix::pack_32(range.start().into(), range.end().into())
        });
```
new
```rust
        RANGE_SORT.with_borrow_mut(|scratch| {
            radix::sort_copy_by_key_in(
                &mut ranges,
                |range| radix::pack_32(range.start().into(), range.end().into()),
                scratch.take(),
            )
        });
```
No nested use: the sort calls no query.

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).
> checked by db-mem-def (22:20): applied already (`RANGE_SORT`, `sort_copy_by_key_in`).

## E8. `real_span_map`: the anchor list in the frame, the map built exact

**What changes.** `pairs` is a temporary: `RealSpanMap::from_file` builds its own two columns
from it (`span/src/map.rs`, `from_file`, the two `.collect()`s at exact length) and drops it.
Today it is a `vec![..]` grown by doubling, `into_boxed_slice` (a shrink realloc), and a radix
sort's global scratch. After: a frame vector, the worker's sort scratch, and `from_file` reads a
slice (F4).

Made: once per file (per run). Frame from: the query body.

`crates/hir-expand/src/span_map.rs`:
```rust
thread_local! {
    /// The scratch of the anchor sort, kept by the worker.
    static ANCHOR_SORT: std::cell::RefCell<blaze_mem::scratch::Scratch<(syntax::TextSize, span::ErasedFileAstId)>> =
        const { std::cell::RefCell::new(blaze_mem::scratch::Scratch::new()) };
}
```
`real_span_map`'s body inside `blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| { .. })`, and:
- old `let mut pairs = vec![(syntax::TextSize::new(0), span::ROOT_ERASED_FILE_AST_ID)];`
  new
  ```rust
      let mut pairs = Vec::new_in(frame);
      pairs.push((syntax::TextSize::new(0), span::ROOT_ERASED_FILE_AST_ID));
  ```
  (block 7's justification of this line is not needed after this).
- old `radix::sort_copy_by_key(&mut pairs, |&(offset, _)| u32::from(offset));`
  new
  ```rust
      ANCHOR_SORT.with_borrow_mut(|scratch| {
          radix::sort_copy_by_key_in(&mut pairs, |&(offset, _)| u32::from(offset), scratch.take())
      });
  ```
- old `pairs.into_boxed_slice(),` new `&pairs,`.

The test `in_order_gives_the_spans_a_lookup_does` passes `&anchors` to `from_file` (F4).

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).
> applied by db-mem-def (21:55; db-expand-macros-next2 handed over `span_map.rs`): E8 in full, F4's `from_file(.., &[..], ..)` being in. Differs: the tracked `real_span_map` opens `frame::task(Tag::Expand, ..)` and calls `real_span_map_in(db, file, frame)`, which holds the body unchanged, so the body isn't re-indented.

## E9. `line_starts`: counted, then filled at its exact length

The result is `Box<[u32]>`, built today in `Vec::with_capacity(text.len() / 32 + 1)`: an estimate,
so a file with more lines than one per 32 bytes grows it by doubling, and every other file's
`into_boxed_slice` reallocates to shrink it. After, the newlines are counted first (one pass over
the bytes, vectorized by the compiler) and the vector is allocated once at its length:

`crates/hir-expand/src/builtin/fn_macro.rs`, old
```rust
    let text = text.text(db).as_bytes();
    let mut starts = Vec::with_capacity(text.len() / 32 + 1);
    starts.push(0);
```
new
```rust
    let text = text.text(db).as_bytes();
    let lines = text.iter().filter(|&&byte| byte == b'\n').count();
    // alloc: the query's value, once per file, at its exact length
    let mut starts = Vec::with_capacity(lines + 1);
    starts.push(0);
```

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).
> checked by db-mem-def (23:10): no longer applies. `line_starts` is gone: `line_and_column` reads `base_db::line_index`, a `span::LineIndex` (span's, not lane db's). The capacity estimate the request replaces isn't in the tree.

## E10. `expand_speculative`: its own task

IDE-only (completion), outside any query run. Its temporaries are the speculative argument tree
(five global columns), the attribute argument tree, the expansion tree and the fixups. After, the
body runs in `frame::task(Tag::Expand, |frame| { .. })`; the trees and fixups are frame values,
and what it returns (`SyntaxNode`, the mapped tokens' `Vec`) is owned.

`crates/hir-expand/src/lib.rs`, `expand_speculative`'s body becomes
`frame::task(Tag::Expand, |frame| { .. })` around the current body, with:
- each `syntax_bridge::syntax_node_to_token_tree(` becomes `syntax_bridge::syntax_node_to_token_tree_in(`
  with `frame` last (three places);
- `attr_macro_input_to_token_tree(db, ..)` becomes `attr_macro_input_to_token_tree(frame, db, ..)` (two places);
- the expansion's match: `pseudo_derive_attr_expansion(frame, &tt, attr_arg.as_ref()?, span)`,
  `.expand_unhygienic(db, frame, &tt, loc.kind.call_style(), span)`, `it.expand(db, frame, self, &tt, span)`
  for the builtins, `expand_unimplemented_builtin_macro(frame, span)`. The proc macro's arm
  returns the expander's global tree, so the match's value is an `Expansion`:
  `Expansion::Framed(..)` around each arm but the proc macro's, which is held in a local and
  returned as `Expansion::Borrowed(&local)`. Concretely:
  ```rust
          let proc_macro_tree;
          let mut speculative_expansion = match loc.def.kind {
              MacroDefKind::ProcMacro(ast, expander, _) => {
                  /* as now, binding the result */
                  proc_macro_tree = expander.expand(..);
                  proc_macro_tree.as_ref().map(Expansion::Borrowed)
              }
              MacroDefKind::BuiltInAttr(_, it) if it.is_derive() => {
                  pseudo_derive_attr_expansion(frame, &tt, attr_arg.as_ref()?, span)
                      .map(Expansion::Framed)
              }
              /* each other arm: `.map(Expansion::Framed)` */
          };
  ```
  `fixup::reverse_fixups(&mut speculative_expansion.value, &undo_info)` takes the
  `Expansion` (`Borrowed` with fixups to undo is copied into the frame first:
  `Expansion::Framed(tt::TopSubtree::from_subtree_in(it.view(), frame))`, then reversed; in
  `fixup.rs`, `pub(crate) fn reverse_fixups_expansion(frame, tt: &mut Expansion<'_, '_>, undo_info)`
  does that and calls `reverse_fixups`).
- `token_tree_to_syntax_node(db, frame, speculative_expansion.value.view(), expand_to)`.

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).
> partly applied by db-mem-def (22:20): the speculative parse runs in a task of its own (`token_tree_to_syntax_node(db, frame, view, ..)`). The body's handle trees stay global: they are of an edited tree, which no view reads, and syntax-bridge has no `_in` form for handles, because new code uses views. The framed expansion waits with E2's expanders. `attr_macro_input_to_token_tree_handle` already opens its own frame (E3).
> applied further by db-mem-def (23:00; `lib.rs`, `fixup.rs` handed over): the whole speculative expansion runs in one `frame::task`. Each builtin's, derive's and attribute's tree is `Expansion::Framed`, and the proc macro's is held in a local and read as `Borrowed`. `fixup::reverse_fixups_expansion(frame, ..)` copies a borrowed tree into the frame only when there are fixups to undo. Differs: the `macro_rules!` arm keeps `expand_unhygienic`'s global tree (`Expansion::Owned`), because `declarative.rs` is handed back. The speculative argument trees stay global handles (as at 22:20).
> applied by db-audit-mem (23:10; hir-expand free, db-expand holds nothing): the `macro_rules!` arm is framed too: `.expand_unhygienic(db, frame, &tt, ..).map(Expansion::Framed)` (`crates/hir-expand/src/lib.rs:1129`), so `Expansion::Owned` is gone (`lib.rs:101`, its `view` arm, and `fixup.rs`' `reverse_fixups_expansion` arm). Not built.

## E11. Builtin derives: the item's shape in the frame

**What changes.** Every `#[derive(..)]` of a builtin trait (the commonest macro call in real
code) runs `parse_adt` → `to_adt_syntax` + `parse_adt_from_syntax`. Its temporaries, all
dropped when the expansion is written:
- `to_adt_syntax`: the item reparsed from the token tree (`crate::token_tree_to_syntax_node`),
  a green tree and a `SpanMap` (two columns, F3 makes them the frame's: `SpanMap<&'f Frame>`);
- `parse_adt_from_syntax`: `fields: Vec<tt::Ident>`, `variants: Vec<Variant>` (doubling),
  `param_types: Vec<AdtParam>`, `lifetimes`, `where_clause`, `associated_types` (each a `Vec`
  of `tt::TopSubtree`, five global columns per tree, from `to_tt`);
- `BasicAdtInfo::bindings`, a `Vec<tt::Ident>` per pattern (`_self`, `_other`), per derive.
After, every one is a frame value (`Vec<_, &'f Frame>`, `tt::TopSubtree<&'f Frame>`), and the
derive's only global allocations are its expansion's... which E2 also puts in the frame. So a
builtin derive's miss allocates nothing beyond the parse of its result.

Made: per derive, per field, per generic parameter. Frame from: `macro_expand` (E2) or
`expand_speculative` (E10), through `BuiltinDeriveExpander::expand`.

`crates/hir-expand/src/builtin/derive_macro.rs`:
- `register_builtin!`'s `expander()` returns
  `fn(&Db, &'f Frame, Span, &tt::TopSubtree) -> ExpandResult<tt::TopSubtree<&'f Frame>>` (written
  `for<'f> fn(&Db, &'f Frame, Span, &tt::TopSubtree) -> ExpandResult<tt::TopSubtree<&'f Frame>>`),
  and each `*_expand(db, span, tt)` takes `frame` second and calls `parse_adt(db, frame, tt, span)`.
- `BuiltinDeriveExpander::expand(&self, db, frame: &'f Frame, id, tt, span) -> ExpandResult<tt::TopSubtree<&'f Frame>>`;
  `clone_expand_with(db, frame, span, tt, true)`, `self.expander()(db, frame, span, tt)`,
  `expanded.map(|it| as_const_impls(frame, it))`.
- `as_const_impls(frame, expanded: tt::TopSubtree<&'f Frame>) -> tt::TopSubtree<&'f Frame>` ends
  `builder.build_in(frame)`.
- the shape types carry the frame:
  ```rust
  struct AdtShape<'f> {
      kind: AdtKind,
      variants: Vec<Variant, &'f Frame>,
      fields: Vec<tt::Ident, &'f Frame>,
  }

  struct BasicAdtInfo<'f> {
      name: tt::Ident,
      shape: AdtShape<'f>,
      param_types: Vec<AdtParam<'f>, &'f Frame>,
      lifetimes: Vec<(tt::TopSubtree<&'f Frame>, tt::TopSubtree<&'f Frame>), &'f Frame>,
      where_clause: Vec<tt::TopSubtree<&'f Frame>, &'f Frame>,
      associated_types: Vec<tt::TopSubtree<&'f Frame>, &'f Frame>,
      packed: bool,
      copy_bounds: bool,
  }

  struct AdtParam<'f> {
      name: tt::TopSubtree<&'f Frame>,
      const_ty: Option<tt::TopSubtree<&'f Frame>>,
      bounds: Option<tt::TopSubtree<&'f Frame>>,
  }
  ```
  (`#[derive(Debug)]` stays on `AdtShape`; `Vec<T, A>` is `Debug` for any `A`).
- `fn bindings(&self, suffix: &str) -> Vec<tt::Ident, &'f Frame>`:
  `let mut out = Vec::with_capacity_in(self.shape.fields.len(), *self.shape.fields.allocator());`.
- `parse_adt(db, frame, tt, call_site)`: `let (adt, tm) = to_adt_syntax(db, frame, tt, call_site)?;`
  `parse_adt_from_syntax(frame, &adt, &tm, call_site)`.
- `push_fields(fields: &mut Vec<tt::Ident, &'f Frame>, ..)` (block 6's `reserve` lines stay).
- `parse_adt_from_syntax<'f>(frame: &'f Frame, adt, tm: &span::SpanMap<&'f Frame>, call_site) -> Result<BasicAdtInfo<'f>, ExpandError>`:
  `let mut fields = Vec::new_in(frame); let mut variants = Vec::new_in(frame);`;
  `to_tt` becomes `syntax_bridge::syntax_node_to_token_tree_in(node, tm, call_site, DocCommentDesugarMode::ProcMacro, frame)`;
  each `.collect()` of `lifetimes`, `param_types`, `where_clause` and `associated_types` becomes
  `.collect_in(frame)` (`Iterator::collect_in`, `allocator_api`'s), and `None => Vec::new()` /
  `Vec::new()` become `Vec::new_in(frame)`; `tt::TopSubtree::empty(..)` becomes
  `tt::TopSubtree::empty_in(.., frame)` (two places).
- `to_adt_syntax(db, frame, tt, call_site) -> Result<(ast::Adt, span::SpanMap<&'f Frame>), ExpandError>`:
  `let (parsed, tm, _) = crate::token_tree_to_syntax_node_in(db, frame, tt.view(), crate::ExpandTo::Items);`
  where `token_tree_to_syntax_node_in` in `lib.rs` is E2's `token_tree_to_syntax_node` with the
  span map built in `frame` (`syntax_bridge::token_tree_to_syntax_node_in`, F3); E2's own call
  keeps the global map, which is the memo.
- `write_derive(frame, span, f)` ends `b.build_skip_top_subtree_in(frame)`; `empty(span)` becomes
  `empty(frame, span)` with `tt::TopSubtree::empty_in(.., frame)`; every
  `tt::TopSubtree::empty(tt::DelimSpan { open: .., close: .. })` in the file becomes `empty_in(.., frame)`.
- the derives that read the item directly (`from_derive`, `coerce_shared_target`,
  `coerce_pointee_expand`): their `SpanMap` and trees the same way; their `SyntaxEditor` edits are
  syntax's (global green nodes), rare (`CoercePointee`, `CoerceShared`).
- `quote!` inside this file builds with `TopSubtreeBuilder`, whose working vectors are already the
  worker's (`tt/src/storage.rs`, `Scratch::take`); each `quote!` used as a value
  (`ExpandResult::ok(quote! {..})`) becomes `quote_in!(frame, span => ..)` (F8's macro, below in
  `builtin/quote.rs`).
- The test module (`derive_macro/tests.rs`): `item(..)` and each `*_derive(span, parse_adt_from_syntax(..))`
  run inside `blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| ..)` and pass `frame`; the
  expected trees compare by view (`a.view() == b.view()` through `TokenTreesView: PartialEq`,
  which tt has).

`crates/hir-expand/src/builtin/quote.rs`: next to `quote!`,
```rust
/// [`quote!`], the tree built in `frame`: an expansion, which its query parses and drops.
#[macro_export]
macro_rules! quote_in {
    ($frame:expr, $span:ident=> $($tt:tt)* ) => {
        {
            let mut builder = $crate::tt::TopSubtreeBuilder::new($crate::tt::Delimiter {
                kind: $crate::tt::DelimiterKind::Invisible,
                open: $span,
                close: $span,
            });
            #[allow(unused)]
            let builder_ref = &mut builder;
            $crate::builtin::quote::__quote!($span builder_ref $($tt)*);
            builder.build_skip_top_subtree_in($frame)
        }
    }
}
pub use quote_in;
```
(the body is `quote!`'s, whose last line is `builder.build_skip_top_subtree()`; read
`quote.rs:118` before applying, and keep the two the same but the last line). `ToTokenTree for
&crate::tt::TopSubtree` becomes `impl<A: core::alloc::Allocator> ToTokenTree for &crate::tt::TopSubtree<A>`.

Test: T1 (c), a derive's shape and expansion in the frame make no global allocation.

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).
> partly applied by db-mem-def (22:10; `builtin/quote.rs` handed over): `quote_in!` next to `quote!` (same body, last line `build_skip_top_subtree_in($frame)`), and `ToTokenTree for &TopSubtree<A>` for any allocator. The derive shapes in the frame wait on `lib.rs`. `token_tree_to_syntax_node_in` (E2) and every `*_expand`'s return type are read there, and `parse_adt_from_syntax` reads handles, so its trees need `call_input_in` on views, a move frontend-tree makes one function at a time.
> partly applied by db-mem-def (23:00; `derive_macro.rs` handed over): every derive's expansion is in the frame. `expander()` is `for<'f> fn(&Db, &'f Frame, Span, &TopSubtree) -> ExpandResult<TopSubtree<&'f Frame>>`, every `*_expand` takes `frame` second and every `*_derive`/`from_derive` takes it first, and `write_derive`, `empty`, `as_const_impls`, `derivable`, `expand_simple_derive`, `expand_reborrow_marker_derive` and `write_coerce_pointee_impls` build in it. `partial_eq_derive`'s StructuralPartialEq and `write_partial_eq_body` are as they were. Not applied: the shapes in the frame (`AdtShape<'f>`, `BasicAdtInfo<'f>`, `AdtParam<'f>`). `parse_adt_from_syntax` reads `ast::Adt` handles, which no view can be made from, and syntax-bridge has no `_in` form for handles: it waits on the function moving to `view::Adt`. `derive_macro/tests.rs`'s six call sites (frame first) wait on that file being handed over.
> applied further by db-mem-def (23:10; `derive_macro/tests.rs` handed over): the tests pass the frame. `Derive` is `for<'f> fn(&'f Frame, ..) -> (old, new in the frame)`, `assert_same` compares trees in any two allocators, and `reference_from`'s refusals copy the new tree out. `partial_eq_writes_structural_partial_eq_first` takes a frame, with its assertions as they were.
> unblocked (db-expand, db-expand-next2, 2026-10-29 23:29): `v2/requests/db-expand.md` (11) is applied. `parse_adt_from_syntax` reads a `view::Adt<'_>` through `&impl SpanMapper`, so the shapes in the frame can be built with `call_input_in`. Queued in db-expand, with T1 (d) after it, for a new child. It starts once `db-expand-derive` has finished (12) (the `CoercePointee` token build, also in `derive_macro.rs`) and the binder fix, so two agents don't edit the file at once.
> routed by db-expand (23:44) to `db-expand-pointee` (brief `~/restart-2026-10-28/briefs/v2/db-expand-pointee.md`): E11's shapes in the frame first, then T1 (d). db-expand-derive is done.

## E12. Fn-like and attribute builtins: expansions in the frame, text in the worker's string

All fn-like, eager and attribute builtins take `frame: &'f Frame` second and return
`ExpandResult<tt::TopSubtree<&'f Frame>>`: the `register_builtin!` signatures in
`builtin/fn_macro.rs` (both macros) and `builtin/attr_macro.rs`, each `*_expand` function, each
`quote! {span => ..}` used as an expansion becoming `quote_in!(frame, span => ..)`, each
`tt::TopSubtree::empty(..)` becoming `empty_in(.., frame)`, each
`tt::TopSubtree::invisible_from_leaves(span, [..])` becoming `invisible_from_leaves_in(span, [..], frame)`,
and each expansion that is its input (`tt.clone()` in `dummy_attr_expand`, `gca_expand`, the
attribute arms block 7 lists) becoming `tt::TopSubtree::from_subtree_in(tt.view(), frame)` (one
frame copy instead of five global columns). `pseudo_derive_attr_expansion(frame, tt, args, call_site)`
likewise. Their callers are E2 and E10.

Strings built to be interned or quoted, per call, moved to the worker's string
(`blaze_mem::scratch::with_string`, which lends it empty and keeps its capacity):
- `concat_expand`: `let mut text = String::new();` and the rest of the loop and result inside
  `scratch::with_string(|text| { .. })`, the last line `ExpandResult { value: quote_in!(frame, span => #text_ref), err }`
  with `let text_ref: &str = text;` (quote implements `ToTokenTree` for `&str`); `missing` stays
  (error path, block 7).
- `concat_bytes_expand`: `let mut bytes = Vec::new();` → `scratch::with_bytes(|bytes| { .. })` around the loop and the result.
- `assert_expand`: `let mut message = String::with_capacity(..)` → `scratch::with_string`, the
  `group` closure reading `message.as_deref()` inside it (the expansion is built inside the closure).
- `format_args_nl_of_literal`: `let mut joined = String::with_capacity(..)` → `scratch::with_string(|joined| { ..; Symbol::intern(joined) })`.
- `escape_ascii` (`concat_bytes`'s literal, `fn_macro.rs`, `let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 8);`
  then `String::from_utf8(out)`): the escaped text is interned by `tt::Literal::new_no_suffix`, so
  it becomes `scratch::with_string(|out| { escape into out; tt::Literal::new_no_suffix(out, ..) })`
  at its caller; `escape_ascii` takes `out: &mut String`.
- `attr_macro.rs:281`/`fn_macro.rs:1759` (`texts: Vec<String>`) are tests.

Made: per builtin call. Test: T1 (d).

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).
> applied by db-mem-def (21:55; `fn_macro.rs` handed over by db-expand-macros-next2, db-apply-def done): the string half. `concat_expand`, `concat_bytes_expand`, `assert_expand` and `format_args_nl_of_literal` take `scratch::lend_string()`/`lend_bytes()` (the same shelf as `with_*`, so the long loops aren't re-indented) and read the text as `&str` at the `quote!`. `escape_ascii(bytes, out: &mut Vec<u8>) -> &str` writes bytes, as it does today, into a `with_bytes` buffer at both callers (`concat_bytes`'s `byte_str`, `include_bytes_expand`), and the test reuses one `out`. The frame half (`frame` second, `quote_in!`, `empty_in`, `from_subtree_in`) waits on F1–F4's `TopSubtree<A>`. `attr_macro.rs` untouched.
> not applied by db-mem-def (22:10): E12's frame half (`frame` second, `quote_in!`, `empty_in`, `from_subtree_in`) changes the builtins' signatures, which `macro_expand`/`expand_speculative` in `lib.rs` call: waits on `lib.rs`. `quote_in!` is in. `attr_macro.rs` was not handed over.
> applied by db-mem-def (23:00; `fn_macro.rs`, `attr_macro.rs`, `lib.rs` handed over): the frame half. Every fn-like, eager and attribute builtin takes `frame` second and returns `TopSubtree<&'f Frame>`, as does `pseudo_derive_attr_expansion(frame, ..)` (`quote_in!`, `empty_in`, `invisible_from_leaves_in`, `build_in`, `from_subtree_in` for the input-as-expansion arms). `include!` reads the file by `call_input_in(parse.root(), .., frame)`. `macro_expand`'s builtin arms are `Expansion::Framed`, and the attribute arm's `reverse_fixups` is generic over the allocator. The `format_args_nl!` literal test runs in a frame.

## E13. Eager expansion: the argument's span map is the worker's

`expand_eager_macro_input` is reached from hir-def's `macro_call_as_call_id`
(`hir-def/src/lib.rs:1654`), which runs in def collection, body lowering and `hir::Semantics`
(outside a run), so it takes no frame (rule 1). Its temporary is `arg_map`, an
`ExpansionSpanMap` built per eager call and dropped after the argument's tree is made (two
global columns grown by doubling, then `finish`'s shrink under `debug_assertions`). It recurses
(a nested eager call makes its own), so one thread-local map would be borrowed twice; a free list
of maps, as deep as eager calls nest, is the worker's:

`crates/hir-expand/src/eager.rs`:
```rust
thread_local! {
    /// The span maps of eager inputs being expanded, one per nesting level, kept emptied.
    static ARG_MAPS: std::cell::RefCell<Vec<ExpansionSpanMap>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A span map of the worker's, empty, given back when dropped.
struct ArgMap(Option<ExpansionSpanMap>);

impl ArgMap {
    fn take() -> ArgMap {
        let map = ARG_MAPS.with_borrow_mut(Vec::pop).unwrap_or_else(ExpansionSpanMap::empty);
        ArgMap(Some(map))
    }
}

impl std::ops::Deref for ArgMap {
    type Target = ExpansionSpanMap;
    fn deref(&self) -> &ExpansionSpanMap {
        self.0.as_ref().expect("held until dropped")
    }
}

impl std::ops::DerefMut for ArgMap {
    fn deref_mut(&mut self) -> &mut ExpansionSpanMap {
        self.0.as_mut().expect("held until dropped")
    }
}

impl Drop for ArgMap {
    fn drop(&mut self) {
        if let Some(mut map) = self.0.take() {
            map.clear();
            // A thread that is ending: the map is freed.
            let _ = ARG_MAPS.try_with(|maps| maps.borrow_mut().push(map));
        }
    }
}
```
old `let mut arg_map = ExpansionSpanMap::empty();` new `let mut arg_map = ArgMap::take();`; the
`eager_macro_recur(db, arg_exp_map, &mut arg_map, ..)` call passes `&mut arg_map` (deref);
`arg_map.finish()` under `debug_assertions` stays (F3 makes it check order without shrinking);
`SpanMap::ExpansionSpanMap(&arg_map)` → `SpanMap::ExpansionSpanMap(&*arg_map)`.
`ExpansionSpanMap::clear` is F3.

`replacements` (a `Vec` only when a nested call is replaced) and `Box::new(EagerCallInfo { .. })`
(the interned key, owned) stay, justified by block 7. The `&dyn` callbacks are not this request's:
dedyn's agents may make them concrete or generic, and E13 applies on top of whatever they become.

Made: per eager call. Test: T1 (b) for `clear`'s reuse; T2 (a) covers the IDE path through hits.

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).
> applied by db-mem-def (22:10; `eager.rs` handed over): E13 as written (`ARG_MAPS`, `ArgMap::take`, `&*arg_map`), and `ArgMap`/`take` `pub(crate)` for T1 (b).

## E14. Hit paths: span mapping and attribute lookups (rule 1)

Reached by `hir::Semantics` on every IDE request, by diagnostics and by lowering, outside or
inside a run. None calls `current`; each allocates nothing warm:
- `map_node_range_up`, `map_node_range_up_rooted(_span)`, `span_for_offset`, `resolve_span`
  (`lib.rs`), `InFile`'s upmapping in `files.rs`: binary searches of the memo's columns and an
  `ast_id_map` memo read. Nothing to change; T1 (a) and T2 (a) hold them at 0.
- `ExpansionInfo::new`: a memo read and node handles. 0.
- `ExpansionInfo::map_range_up_once`: a `SmallVec<[TextRange; 1]>`, inline for one range (block 7's
  `from_buf`). 0 for one range.
- `ExpansionInfo::map_range_down`: `ExpansionSpanMap::ranges_with_span` collects a `Vec<u32>` per
  call (`span/src/map.rs`, `.collect::<Vec<u32>>()`): F2 lends the worker's `u32`s instead; the
  first downmap of a map also builds its `by_anchor` index once (owned by the memo, exact). So 0
  after the first. `map_range_down_exact`: 0 already.
- `AttrId::find_derive_range` (`attrs.rs`): diagnostics and `BuiltinDeriveImplLoc::source`; it
  converts the derive's token tree to a `tt::TopSubtree` (five global columns) to split it at its
  commas. After, the body runs in `frame::task(Tag::Expand, |frame| { .. })` and the tree is
  `syntax_bridge::syntax_node_to_token_tree_in(.., frame)`; `parse_path_comma_token_tree` takes
  `args: &'a tt::TopSubtree<A>` (generic, F1). The `RealSpanMap::absolute` it fakes is two
  one-element boxes: they become `RealSpanMap::absolute_ref(file_id)`, a `&'static`-free view
  (F4: `SpanMap::Absolute(EditionedFileId)` in hir-expand's `SpanMap` enum instead, see below),
  so the lookup allocates nothing.

  `crates/hir-expand/src/span_map.rs`'s `SpanMap` gains the absolute case, which every
  `RealSpanMap::absolute(..)` + `SpanMap::RealSpanMap(&..)` pair in hir-expand
  (`find_derive_range`, `expand_speculative`'s `RealSpanMap::absolute(span.anchor.file_id)`) uses
  instead:
  ```rust
      /// The spans of a file relative to its root, no anchors: what a lookup that needs only
      /// offsets uses.
      Absolute(span::EditionedFileId),
  ```
  with `SpanMap::span_for_range`'s arm
  `Self::Absolute(file_id) => span::RealSpanMap::absolute_span(*file_id, range)` and `InOrder`'s
  the same (F4's `absolute_span`).
- `AttrId::find_attr_range(_with_source)`: syntax walks, 0. `Attr::string_value_unescape`: an
  owned `Cow` only for a string with escapes, returned to the caller (block 7).
- `ModPath::from_src`/`from_tt` (`mod_path.rs`): segments in an inline `SmallVec<[Name; 8]>`, the
  list interned (0 when it exists). The `SmallVec` stays: these run inside callbacks that call
  the database again (`span_for_range`), which can re-enter path conversion, so a thread-local
  buffer would be borrowed twice; a path of more than 8 segments spills, once.
- `hygiene.rs`: `MarkCache` is on the stack; `apply_mark` interns contexts (`Tag::Intern`, 0
  warm). Nothing to change.

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).
> applied by db-audit-mem (22:39): `AttrId::find_derive_range` (crates/hir-expand/src/attrs.rs:647) finds the derive through `find_attr_range_view`, builds its tree with `syntax_bridge::call_input_in(.., frame)` in `frame::task(Tag::Expand, ..)`, and maps spans with a private `AbsoluteSpans` mapper over F4's `RealSpanMap::absolute_span` (no boxes); `parse_path_comma_token_tree` takes `&tt::Tree`, so any allocator's tree derefs to it. Differs: no `SpanMap::Absolute` variant, since hir-def matches `SpanMap` exhaustively (`expr_store/expander.rs:80`) and the mapper needs no variant; `expand_speculative`'s `RealSpanMap::absolute` (lib.rs:992, IDE-only, passed as a `SpanMap` to the fixup walk) stays. The other bullets need nothing (F2 applied by frontend-tree-span).

## M. What each memoized result is (contract 4)

Every memo below is owned (global allocator, lane mem's ruling) and built once at its exact
length from temporaries in the run's frame or the worker's scratch. None is a frame value.

| query | memo | built from | growth it replaces | by |
|---|---|---|---|---|
| `parse_macro_expansion` | `Parse<SyntaxNode>` | the expansion tree (frame, E2) through the parser's events | none in hir-expand: the green tree is `syntax`'s (rowan's node cache), out of this slice | E2 |
| `parse_macro_expansion` | `ExpansionSpanMap` (`ends`, `spans`, fragment columns) | the sink's columns in the worker's scratch, copied once at their length (`from_columns_in`) | `SpanMap::with_capacity(tokens)` sized by input tokens, then `finish`'s `shrink_to_fit` of `ends` and `spans` (a realloc each whenever punctuation glued) | F3 |
| `macro_arg` | `tt::TopSubtree` (5 columns) | `TopSubtreeBuilder`, whose working vectors are the worker's `Scratch`; `finish` writes each column at its length | `nodes`, `spans` are exact today; `parts` and `wide` grow by doubling from empty and `symbols` is reserved at an upper bound, each then shrunk by `into_boxed_slice` | F9 |
| `macro_arg` | `SyntaxFixupUndoInfo.original: Option<ThinVec<TopSubtree>>` | a frame vector of the replaced subtrees, drained once into `ThinVec::with_capacity(len)`; `None` when no fixup | `ThinVec::new()` grown by doubling, then `shrink_to_fit` | E3 |
| `macro_arg` | `Span` | copied | none | — |
| `expand_proc_macro` | `ExpandResult<tt::TopSubtree>` | the expander's result (user code at the edge); trees the native server builds go through `TopSubtreeBuilder` | as `macro_arg`'s tree | F9 |
| `decl_macro_expander` | `DeclarativeMacroExpander { mac, transparency, edition }`; `mac.rules: Box<[Rule]>` | the definition's tree in the frame (E5); rules collected in a frame vector, boxed at their count | `rules: Vec::new()` pushed per rule, then `into_boxed_slice` (`mbe/src/lib.rs:175`, `:215`) | E5, F7 |
| `ast_id_map` | `AstIdMap` (`entries: Box<[Entry]>`, its index) | `entries`, `found` and the next-index map in the frame; `Box::<[Entry]>::from(&entries[..])` | `vec![..]` grown by doubling, then `into_boxed_slice` (`span/src/ast_id.rs:1205`, `:1259`) | E6, F6 |
| `real_span_map` | `RealSpanMap` (two columns) | `pairs` in the frame, sorted with the worker's scratch; `from_file` collects its columns from the slice at exact length | `vec![..]` grown by doubling and `into_boxed_slice` (a shrink) before `from_file` read it; the radix sort's global scratch | E8, F4 |
| `syntax_error_ranges` | `Box<[TextRange]>` | an exact-size `map(..).collect()`, then `into_boxed_slice` (no realloc); sort scratch the worker's | the sort's global scratch (applied) | E7 |
| `line_starts` | `Box<[u32]>` | newlines counted, then `Vec::with_capacity(lines + 1)` (applied) | `text.len() / 32 + 1` estimate: doubling past it, a shrink under it | E9 |
| `proc_macro_span` | `Span` | copied | none | — |
| `ProcMacros::get_for_crate` | `Option<Arc<CrateProcMacros>>` | an `Arc` clone of the input's value | none (a refcount bump) | — |
| `may_have_session_macros` | `bool` | — | none | — |

The span maps an IDE downmap builds lazily (`by_anchor`, F2) belong to the memo they index and
are built once at exact length, so they follow the same rule.

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:39): a table; each row is its E/F section's, settled there.

## T1. Budget tests in `crates/hir-expand/src/budget_tests.rs` (append to block 8's file)

```rust
use blaze_mem::{Tag, frame};

/// Mapping up out of an expansion's span map and back down, warm: the first downmap builds the
/// map's index by anchor and grows the worker's `u32`s once; after that nothing allocates.
#[test]
fn span_mapping_is_free_once_warm() {
    let mut map = ExpansionSpanMap::empty();
    let ranges: Vec<TextRange> =
        (0..256u32).map(|i| TextRange::new((i * 4).into(), (i * 4 + 3).into())).collect();
    for &range in &ranges {
        map.push(range.end(), span(range));
    }
    map.finish();
    let probe = |map: &ExpansionSpanMap| {
        let mut found = 0usize;
        for &range in ranges.iter().step_by(17) {
            found += crate::map_node_range_up_rooted_span(map, range).is_some() as usize;
            found += map.ranges_with_span(span(range)).count();
            found += map.ranges_with_span_exact(span(range)).count();
        }
        found
    };
    let warm = probe(&map);
    let again = blaze_alloc_count::assert_at_most(0, "warm span mapping", || probe(&map));
    assert_eq!(again, warm);
}

/// An eager input's span map taken from the worker and given back: the second take reuses the
/// first's columns.
#[test]
fn an_eager_span_map_is_reused() {
    let fill = |map: &mut ExpansionSpanMap| {
        for i in 1..=64u32 {
            map.push((i * 4).into(), span(TextRange::new((i * 4 - 4).into(), (i * 4).into())));
        }
    };
    {
        let mut first = crate::eager::ArgMap::take();
        fill(&mut first);
    }
    blaze_alloc_count::assert_at_most(0, "a reused eager span map", || {
        let mut again = crate::eager::ArgMap::take();
        fill(&mut again);
        again.iter().count()
    });
}

/// An expression expansion's tokens before its last `;`, in the run's frame: no global
/// allocation, and the tokens are the input's.
#[test]
fn the_tokens_before_a_semicolon_are_framed() {
    let text = "a + b ;";
    let tt = syntax_bridge::parse_to_token_tree_static_span(
        Edition::CURRENT,
        span(TextRange::empty(TextSize::new(0))),
        text,
    )
    .expect("tokens");
    frame::task(Tag::Expand, |frame| {
        let (before, semicolon) =
            blaze_alloc_count::assert_at_most(0, "the tokens before a `;`", || {
                crate::without_last_semicolon(frame, tt.view())
            })
            .expect("ends in `;`");
        assert_eq!(before.token_trees().len(), 3);
        assert_eq!(semicolon, span(TextRange::empty(TextSize::new(0))));
    });
}

/// A builtin derive of a struct with generics, from its input's syntax to its expansion, in the
/// frame: nothing from the global allocator but what the reparse of the input makes (its green
/// tree, syntax's), measured by itself and subtracted.
#[test]
fn a_derive_is_framed() {
    let text = "struct S<'a, T: Clone, const N: usize> where T: Copy { a: &'a T, b: [u8; N], c: T::Assoc }";
    frame::task(Tag::Expand, |frame| {
        let (adt, map) = crate::builtin::derive_macro::tests::item_in(frame, text);
        let call_site = crate::builtin::derive_macro::tests::call_site();
        let run = || {
            let info = crate::builtin::derive_macro::parse_adt_from_syntax(frame, &adt, &map, call_site);
            crate::builtin::derive_macro::clone_derive(frame, call_site, info, false)
        };
        let warm = run();
        let (again, used) = blaze_alloc_count::measure(run);
        assert!(again.value.view() == warm.value.view());
        assert_eq!(used.allocations, 0, "a derive's shape and expansion: {used:?}");
    });
}

/// `concat!` of literals, warm: the text is the worker's string and the expansion the frame's;
/// the literal's symbol exists after the first run.
#[test]
fn concat_is_framed() {
    let call = span(TextRange::empty(TextSize::new(0)));
    let tt = syntax_bridge::parse_to_token_tree_static_span(Edition::CURRENT, call, r#""a", 1, 'b', true"#)
        .expect("tokens");
    frame::task(Tag::Expand, |frame| {
        let warm = crate::builtin::fn_macro::concat_text_in(frame, &tt, call);
        let again = blaze_alloc_count::assert_at_most(0, "a warm `concat!`", || {
            crate::builtin::fn_macro::concat_text_in(frame, &tt, call)
        });
        assert!(again.view() == warm.view());
    });
}
```
What the test names need, in hir-expand:
- `eager::ArgMap` and `ArgMap::take` are `pub(crate)`; `without_last_semicolon` and
  `map_node_range_up_rooted_span` are visible from a child module already.
- `builtin/derive_macro.rs`: `pub(crate) mod tests;` under `#[cfg(test)]` (it is `mod tests;`),
  `parse_adt_from_syntax` and `clone_derive` `pub(crate)`; in `derive_macro/tests.rs`,
  `pub(crate) fn call_site()` and a framed `item`:
  ```rust
  pub(crate) fn item_in<'f>(frame: &'f blaze_mem::Frame, text: &str) -> (ast::Adt, span::SpanMap<&'f blaze_mem::Frame>) {
      let tt = syntax_bridge::parse_to_token_tree_static_span(span::Edition::CURRENT, call_site(), text)
          .expect("tokens");
      let (parse, map) = syntax_bridge::token_tree_to_syntax_node_in(
          tt.view(),
          syntax_bridge::TopEntryPoint::MacroItems,
          &mut |_| span::Edition::CURRENT,
          frame,
      );
      let adt = parse.syntax_node().descendants().find_map(ast::Adt::cast).expect("an item");
      (adt, map)
  }
  ```
  The existing `item` stays for the other tests. `item_in` returns a handle only because
  `parse_adt_from_syntax` takes `&ast::Adt` today (as `item` does); when frontend-tree moves
  that function to `view::Adt<'_>`, `item_in` returns `(Parse<SyntaxNode>, SpanMap<..>)` and the
  test finds the item by `parse.root().descendants().find_map(view::Adt::cast)`.
- `concat_expand` has no `MacroCallId`-free core today (`args::exprs(db, tt)` takes the
  database). If `args::exprs` reads `db` (it does: `fn_macro/args.rs`), `concat_text_in` is the
  loop and result of `concat_expand` split out as
  `pub(crate) fn concat_text_in<'f>(frame: &'f Frame, tt: &tt::TopSubtree, open: Span) -> tt::TopSubtree<&'f Frame>`
  over literals only, which `concat_expand` calls after `args::exprs`. If that split is more than
  the owner wants, the test moves to T2 with a `concat!` call and the same assertion on
  `parse_macro_expansion`'s miss.

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).
> partly applied by db-mem-def (22:10): (a) `span_mapping_is_free_once_warm` and (b) `an_eager_span_map_is_reused`, appended. (c), (d) and (e) (semicolon, derive, `concat!` in the frame) wait on E2, E11 and E12's frame half.
> partly applied by db-mem-def (22:20): (c) `the_tokens_before_a_semicolon_are_framed` appended, now that `without_last_semicolon` takes a frame.
> forwarded by db-audit-mem (22:56) to db-expand-views (holds `crates/hir-expand/` since 22:45; told by tmux): (d) `a_derive_is_framed` waits on `v2/requests/db-expand.md` (10) (`derive_macro.rs` onto views, still open), which E11's framed shapes also need; (e) `concat_is_framed` needs `concat_text_in` split out of `concat_expand` (`builtin/fn_macro.rs:867`), a change to the owner's code that file's holder decides.
> partly applied by db-expand-views (23:06): (e) `concat_is_framed` appended. `args::exprs` reads the database (editions, in `mbe::expect_fragment`) and its parser input allocates, so the split takes parsed arguments: `concat_text_in(frame, &[args::Arg], open)` is `concat_expand`'s loop and result, unchanged, called after `args::exprs`. The test calls `builtin::concat_of_literals_in(frame, &tt, open)` (`#[cfg(test)]`), which splits at top-level commas (`args::comma_separated`, `#[cfg(test)]`) and calls the same `concat_text_in`, and it also checks that there is no error and that the expansion prints `"a1btrue"`. (d) `a_derive_is_framed` still waits on `v2/requests/db-expand.md` (10), which is not applied (its view move changes output; split requested there). Not run (no builds); hot lint clean on the changed files.
> not applied by db-expand-derive (23:27): (d) `a_derive_is_framed`. Its view half is in now: `v2/requests/db-expand.md` (11) moved `parse_adt_from_syntax` to `(adt: view::Adt<'_>, tm: &impl SpanMapper, call_site)` (with `parse_item`/`adt_of`, `CoercePointee` in `derive_macro/editor.rs`). It still waits on E11's shapes in the frame (`AdtShape<'f>`, `BasicAdtInfo<'f>`, `AdtParam<'f>`, `parse_adt_from_syntax(frame, ..)` building its trees with `call_input_in`), which nothing has applied. Without them the test's `used.allocations == 0` fails, since the shape's `Vec`s and trees are global. E11's blocker (handles) is gone, so it can be applied now in `derive_macro.rs`. When (d) goes in, `item_in` returns `(Parse<SyntaxNode>, SpanMap<&'f Frame>)`, and the test finds the item with `parse.root().descendants().find_map(view::Adt::cast)` and passes it by value, as the note above says.

## T2. Budget tests in `crates/hir-def/src/macro_expansion_tests/budgets.rs` (append to block 8's file)

They need a crate with macro calls, so they sit beside block 8's memo-hit test (hir-def's
counting allocator: mem-db-def H3).

```rust
/// (a) Mapping into and out of an expansion through `ExpansionInfo`, as `hir::Semantics` does on
/// every IDE request: after one round (the downmap index built, the worker's `u32`s grown), a
/// second allocates nothing.
#[test]
fn warm_span_mapping_allocates_nothing() {
    let db = TestDB::with_files(
        r#"
macro_rules! make { ($($t:tt)*) => { fn made() { $($t)* } } }
make!(let _x = 1 + 2;);
"#,
    );
    let krate = db.fetch_test_crate();
    let def_map = crate_def_map(&db, krate);
    let call = def_map
        .modules(&db)
        .flat_map(|module| module.1.scope(&db).all_macro_calls())
        .next()
        .expect("the `make!` call");
    let round = || {
        let info = hir_expand::ExpansionInfo::new(&db, call);
        let expanded = info.expanded();
        let range = expanded.value.text_range();
        let up = info.map_node_range_up(&db, range);
        let at = info.span_for_offset(&db, range.start());
        let (arg, _, _) = call.macro_arg_considering_derives(&db, &call.loc(&db).kind);
        let first = arg.token_trees().first_span().expect("an argument token");
        let down = info.map_range_down(first).map_or(0, |it| it.value.count());
        (up.map(|it| it.0), at.0, down)
    };
    let warm = round();
    let again = blaze_alloc_count::assert_at_most(0, "a warm span mapping round", round);
    assert_eq!(again, warm);
}

/// A new id for the call `call` is: its location one expansion deeper, so its argument and
/// expansion are computed again (a miss) while the file's parse, span map, ast ids and the
/// macro's definition stay warm. Interned before measuring.
fn fresh(db: &TestDB, call: MacroCallId) -> MacroCallId {
    let loc = call.loc(db);
    loc.def.make_call(db, loc.krate, loc.kind.clone(), loc.ctxt, loc.macro_depth + 1)
}

/// What the argument tree of `call` owns: one allocation per non-empty column, counted as a
/// copy of it costs.
fn arg_columns(db: &TestDB, call: MacroCallId) -> u64 {
    let (tree, _, _) = call.macro_arg_considering_derives(db, &call.loc(db).kind);
    blaze_alloc_count::measure(|| tree.clone()).1.allocations
}

/// (b) A miss of `parse_macro_expansion` (with its argument's `macro_arg`) allocates salsa's run
/// cost, the argument tree and the expansion's parse and span map, nothing else: the expansion
/// tree and the semicolon split are the run's frame's. The parse's cost is measured by parsing
/// the same tokens outside the database, warm; salsa's run cost by the miss of a call that
/// expands to nothing, less what that one owns.
#[test]
fn an_expansion_miss_allocates_its_result_alone() {
    let db = TestDB::with_files(
        r#"
macro_rules! make { ($($t:tt)*) => { fn made() { $($t)* } } }
macro_rules! nothing { () => {} }
make!(let _x = 1 + 2; let _y = [3, 4, 5]; let _z = (6, 7););
nothing!();
"#,
    );
    let krate = db.fetch_test_crate();
    let def_map = crate_def_map(&db, krate);
    let calls: Vec<MacroCallId> =
        def_map.modules(&db).flat_map(|module| module.1.scope(&db).all_macro_calls()).collect();
    let [make, nothing] = calls[..] else { panic!("two calls: {calls:?}") };
    let span = make.macro_arg_considering_derives(&db, &make.loc(&db).kind).2;
    let parse_cost = |tokens: &tt::TopSubtree| {
        let run = || {
            syntax_bridge::token_tree_to_syntax_node(
                tokens.view(),
                syntax_bridge::TopEntryPoint::MacroItems,
                &mut |_| span::Edition::CURRENT,
            )
        };
        let _warm = run();
        blaze_alloc_count::measure(run).1.allocations
    };
    let text = make.parse_macro_expansion(&db).value.0.root().text();
    let tokens = syntax_bridge::parse_to_token_tree_static_span(span::Edition::CURRENT, span, text)
        .expect("the expansion's tokens");
    let empty = tt::TopSubtree::empty(tt::DelimSpan::from_single(span));
    let (make, nothing) = (fresh(&db, make), fresh(&db, nothing));
    let (_, base) = blaze_alloc_count::measure(|| make.parse_macro_expansion(&db).value.1.iter().count());
    let (_, run) = blaze_alloc_count::measure(|| nothing.parse_macro_expansion(&db).value.1.iter().count());
    let salsa = run.allocations.saturating_sub(parse_cost(&empty) + arg_columns(&db, nothing));
    let owned = parse_cost(&tokens) + arg_columns(&db, make);
    assert!(
        base.allocations <= salsa + owned,
        "a miss: {} allocations; salsa's run {salsa}, its argument, parse and map {owned}",
        base.allocations,
    );
}

/// (c) A miss of `macro_arg` for a derive's attribute call is the argument tree's columns and
/// salsa's run cost (the same query for a derive of a unit struct, less its tree): the cfg
/// walk's attribute list and inserted leaves are the frame's.
#[test]
fn a_derive_argument_miss_allocates_its_tree_alone() {
    let db = TestDB::with_files(
        r#"
//- minicore: derive, clone
#[derive(Clone)]
#[cfg_attr(all(), allow(unused))]
struct Wide { #[cfg(all())] a: u8, b: u16, #[cfg(any())] c: u32, d: (u8, u8) }
#[derive(Clone)]
struct Unit;
"#,
    );
    let krate = db.fetch_test_crate();
    let def_map = crate_def_map(&db, krate);
    let attrs: Vec<MacroCallId> = def_map
        .modules(&db)
        .flat_map(|module| module.1.scope(&db).all_macro_calls())
        .filter_map(|call| match call.loc(&db).kind {
            hir_expand::MacroCallKind::Derive { derive_macro_id, .. } => Some(derive_macro_id),
            _ => None,
        })
        .collect();
    let [wide, unit] = attrs[..] else { panic!("two derive attributes: {attrs:?}") };
    let (wide, unit) = (fresh(&db, wide), fresh(&db, unit));
    let (_, run) = blaze_alloc_count::measure(|| {
        let _ = unit.macro_arg_considering_derives(&db, &unit.loc(&db).kind);
    });
    let salsa = run.allocations.saturating_sub(arg_columns(&db, unit));
    let (_, miss) = blaze_alloc_count::measure(|| {
        let _ = wide.macro_arg_considering_derives(&db, &wide.loc(&db).kind);
    });
    let owned = arg_columns(&db, wide);
    assert!(
        miss.allocations <= salsa + owned,
        "`macro_arg` of a derive: {} allocations; salsa's run {salsa}, its tree {owned}",
        miss.allocations,
    );
}
```
`MacroDefId::make_call` and `MacroCallLoc`'s fields are public (`hir-expand/src/lib.rs`); a call
interned with `macro_depth + 1` is a distinct id (the depth is part of `MacroCallLoc`'s key), so
its queries run fresh. `all_macro_calls` and `crate_def_map` are what block 8's test uses. The
`all_macro_calls` of a derive gives the `Derive` call; its `derive_macro_id` is the attribute call
whose `macro_arg` runs the cfg walk.

---

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).
> partly applied by db-mem-def (22:10): (c) `a_derive_argument_miss_allocates_its_tree_alone` with `fresh` and `arg_columns`, appended. It lists calls with the file's `macro_calls` and the derive fast path off, as block 8's test does, and orders the two derives by position. (a) and (b) wait on E2 and E14 (`lib.rs`).
> applied by db-audit-mem (22:55): (a) `warm_span_mapping_allocates_nothing` and (b) `an_expansion_miss_allocates_its_result_alone` in `crates/hir-def/src/macro_expansion_tests/budgets.rs`, now that E2 is in and E14 asks nothing of these paths. Differences: calls listed with the file's `macro_calls` helper, as (c) does; (a) reads views (`expanded_view`, `map_range_down_view`), not `SyntaxNode`/`SyntaxToken` handles. With (c), T2 is complete; not built (no builds in this pass).

## F. Requests of lane `format`

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:39): F1-F9 below, each forwarded and settled by format's lanes.

### F1. `tt::TopSubtree<A: Allocator = Global>`

`crates/tt/src/storage.rs`: the columns are `Box<[T], A>` (`nodes`, `spans`, `wide`, `parts`,
`symbols`); `words: WordsCache` stays global (owned by the memo's tree, made on first use) or
is `None` for a framed tree (it is a cache of the wire form, never asked of an expansion tree).
Needed, all keeping the current `Global` ones as they are:
- `TopSubtreeBuilder::build_in<A: Allocator>(self, alloc: A) -> TopSubtree<A>` and
  `build_skip_top_subtree_in`; `finish` writes the columns with `Box::new_uninit_slice_in(len, alloc)`
  (or `Vec::with_capacity_in(len, alloc)` filled, then `into_boxed_slice`: exact, no realloc).
- `TopSubtree::empty_in(span: DelimSpan, alloc: A)`, `from_token_trees_in(delimiter, view, alloc)`,
  `from_subtree_in(view, alloc)`, `invisible_from_leaves_in(span, leaves, alloc)`.
- `view()`, `token_trees()`, `iter()`, `top_subtree()`, `set_top_subtree_delimiter_kind`,
  `set_top_subtree_delimiter_span` on every `TopSubtree<A>` (they read the columns as slices);
  `PartialEq` between any two allocators by view.
- `transform_tt<A: Allocator + Clone>(tt: &mut TopSubtree<A>, ..)` rebuilding in `tt`'s allocator.
- `syntax_bridge::syntax_node_to_token_tree_in(node, map, span, mode, alloc) -> TopSubtree<A>`
  (`convert_tokens` building with `build_in`).
- `mbe::DeclarativeMacro::parse_macro_rules(tt: &tt::TopSubtree<A>, ..)` and `parse_macro2(args:
  Option<&tt::TopSubtree<A>>, body: &tt::TopSubtree<A>, ..)`: they read `tt.iter()` only.
Why: an expansion's tree, a definition's tree, a derive's pieces and the speculative trees are
temporaries of one run (E2, E5, E10 to E12); framed they cost a bump each instead of up to five
global allocations and frees.

> mem-format-expand: forwarded to frontend-tokens (holds `crates/tt`, `syntax-bridge`, `mbe`), restated as `v2/requests/mem-format-expand.md` section 1: tt now keeps a tree as one block, and views borrow `&TopSubtree`, so `TopSubtree<A>` owns its block in `A` and views borrow a non-generic `tt::Tree` (16-byte views kept); `build_in`, `build_skip_top_subtree_in`, `empty_in`, `invisible_from_leaves_in`, `from_token_trees_in`, `from_subtree_in`, `syntax_node_to_token_tree_in`, `call_input_in`, generic `transform_tt` and `parse_macro_rules`/`parse_macro2` as asked; a framed tree keeps no `TreeWords` (its parse converts into the worker's input).

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:39): in the tree: crates/tt/src/storage.rs:511 (`empty_in`, `invisible_from_leaves_in`, `from_token_trees_in`), `syntax_bridge::call_input_in`.

### F2. Readers take views; `ranges_with_span` lends the worker's `u32`s

- `syntax_bridge::token_tree_to_syntax_node(tt: tt::SubtreeView<'_>, entry_point, span_to_edition)`:
  its first line is `let buffer = tt.view().strip_invisible();`, which becomes
  `let buffer = tt.strip_invisible();`. Callers outside hir-expand pass `tt.view()`.
- `span::SpanMap::ranges_with_span` (`span/src/map.rs`): old
  ```rust
          let mut found = by_anchor[first..last]
              .iter()
              .copied()
              .filter(|it| key(it)[3] >= end)
              // alloc: the one documented above; IDE token descent only, never a build's path
              .collect::<Vec<u32>>();
          radix::sort(&mut found);
          found.into_iter().map(move |it| self.range_of(it))
  ```
  new
  ```rust
          let mut found = blaze_mem::scratch::lend_u32s();
          found.extend(by_anchor[first..last].iter().copied().filter(|it| key(it)[3] >= end));
          found.sort_unstable();
          (0..found.len()).map(move |at| self.range_of(found[at]))
  ```
  (`Lent<Vec<u32>>` derefs to the vector and goes back to the thread's shelf when the iterator is
  dropped; the run is short, a comparison sort of `u32`s in place allocates nothing where
  `radix::sort` takes a scratch vector). The doc line "and one allocation" becomes "and nothing
  allocated once the worker's buffer has grown". `blaze-mem.workspace = true` in
  `crates/span/Cargo.toml`.

> mem-format-expand: forwarded, `v2/requests/mem-format-expand.md` section 2: the parse's view to frontend-tokens (2a, with the ten callers outside hir-expand), `ranges_with_span` to frontend-tree-span (2b, which holds all of `crates/span`; blaze-mem is in its manifest already), each with a budget test.
> mem-format-expand (21:40): 2b applied by frontend-tree-span (`crates/span/src/map.rs`, test `ranges_with_span_allocates_nothing_warm`); 2a waits on frontend-tokens.

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).

### F3. `span::SpanMap`: exact columns, built from the worker's, reusable, and in an allocator

- `SpanMap<A: Allocator = Global>`: `ends: Vec<TextSize, A>`, `spans: Vec<Span, A>`,
  `fragment_starts`/`fragment_ends` likewise.
- `finish` stops calling `shrink_to_fit` (it keeps its order check; the fragment sorts move
  to `radix::sort_copy_by_key_in` with a `thread_local!` `Scratch<TextSize>`, as E8's, since
  `sort_copy_by_key` allocates its scratch globally on every call with two or more fragments); the
  sink (`syntax-bridge`, `TtTreeSink`) pushes into the worker's columns (a `thread_local!`
  `Scratch<TextSize>` and `Scratch<Span>`, or one per sink from a free list as eager's maps) and
  `TtTreeSink::finish` builds the map with `SpanMap::from_columns_in(ends: &[TextSize], spans:
  &[Span], fragment_starts: &[TextSize], fragment_ends: &[TextSize], alloc)`, each column
  `Vec::with_capacity_in(len, alloc)` + `extend_from_slice`: exact, no realloc. Today the sink's
  `SpanMap::with_capacity(tokens)` is sized by input tokens and `finish` shrinks it whenever
  punctuation was glued (fewer pieces than tokens), a realloc per expansion.
  (`TtTreeSink`'s text `String` is `mem-format-tokens.md`'s, line 803 on; not refiled.)
- `pub fn clear(&mut self)`: empties every column and `by_anchor`, keeping capacity (E13).
- `syntax_bridge::token_tree_to_syntax_node_in(tt: SubtreeView<'_>, entry, span_to_edition, alloc) -> (Parse<SyntaxNode>, SpanMap<A>)`
  (E11's reparse of a derive's item).
- `span_at`, `span_at_from`, `spans_for_range`, `ranges_with_span(_exact)`, `iter` and `MacroFragments`
  on every `SpanMap<A>`.
Why: `parse_macro_expansion`'s span map is a memo (contract 4: exact, not shrunk), the derive's
and eager's are temporaries.

> mem-format-expand: forwarded, `v2/requests/mem-format-expand.md` section 3, merged with frontend-tree-span's open `push_fragment(range, span)` request (`v2/requests/frontend-tokens.md`) and the applied sink section of `mem-format-tokens.md`: `SpanMap<A>` with the new `fragment_spans` column, `finish` without shrink and its sorts in the worker's scratch, `clear`, and `copy_in(alloc)` in place of `from_columns_in` (3a, frontend-tree-span); the sink fills the worker's kept map and copies it out exact, its open fragments carrying their span (3b, frontend-tokens).
> mem-format-expand (21:40): 3a applied by frontend-tree-span (`SpanMap<A>`, `clear`, `reserve`, `capacity`, `copy_in`, `sort_fragments`; test `a_copied_map_is_exact_and_finishing_allocates_nothing_warm`, under `salsa`); 3b waits on frontend-tokens, told it can go ahead.

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).

### F4. `RealSpanMap`: `from_file` reads a slice; absolute spans without a map

- `RealSpanMap::from_file(file_id, pairs: &[(TextSize, ErasedFileAstId)], end)`: it builds its
  own columns from `pairs` and drops it (E8). The one call outside hir-expand's
  `real_span_map` is block 8's test, which passes `&anchors`.
- `pub fn absolute_span(file_id: EditionedFileId, range: TextRange) -> Span`: the span
  `RealSpanMap::absolute(file_id).span_for_range(range)` gives, computed without the two boxes
  (E14).

> mem-format-expand: forwarded to frontend-tree-span (holds all of `crates/span`), as filed, with the code and test, in `v2/requests/mem-format-expand.md` section 4; `within`/`without` take their pairs from a worker `Scratch`; the three hir-expand callers (`span_map.rs:169`, `:219`, `budget_tests.rs:77`) each pass a slice, db-expand-macros'.
> mem-format-expand (21:40): applied by frontend-tree-span (`from_file(&[_])`, `ANCHOR_PAIRS`, `absolute_span`; test `absolute_spans_need_no_map`); the three hir-expand callers are filed in `v2/requests/db-expand-macros.md`, and hir-expand doesn't compile until they pass slices.

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).

### F5. `syntax_node_to_token_tree_modified` without per-call maps and vectors

`crates/syntax-bridge/src/lib.rs`:
```rust
/// The leaves a conversion's callback inserts at one event: at most three, inline.
pub type Inserted = smallvec::SmallVec<[tt::Leaf; 3]>;
/// The leaves to append after an element, by element.
pub type Appended<A> = hashbrown::HashMap<SyntaxElement, Inserted, rustc_hash::FxBuildHasher, A>;
/// The elements to leave out.
pub type Removed<A> = hashbrown::HashSet<SyntaxElement, rustc_hash::FxBuildHasher, A>;

pub fn syntax_node_to_token_tree_modified<SpanMap, OnEvent, A: Allocator + Clone>(
    node: &SyntaxNode,
    map: SpanMap,
    append: Appended<A>,
    remove: Removed<A>,
    call_site: Span,
    mode: DocCommentDesugarMode,
    on_enter: OnEvent,
) -> tt::TopSubtree
where
    SpanMap: SpanMapper,
    OnEvent: FnMut(&mut PreorderWithTokens, &WalkEvent<SyntaxElement>) -> (bool, Inserted),
```
`Converter`'s `append`/`remove` fields take the same types; `current_leaves: VecDeque<tt::Leaf>`
(allocated at the first inserted leaf, per conversion) becomes a `SmallVec<[tt::Leaf; 4]>` with a
read index (reset when drained), inline for what one event inserts. Callers outside hir-expand
(`derive_macro.rs`'s `coerce_shared_target`, the fixup test) pass `Inserted::new()`. Manifest:
`hashbrown.workspace = true`, `smallvec.workspace = true` in `crates/syntax-bridge/Cargo.toml`
(hashbrown's `nightly` feature is mem-db's root request).
Why: E3, the attribute and derive inputs' walk, per call.

> mem-format-expand: forwarded to frontend-tokens, `v2/requests/mem-format-expand.md` section 5, widened to the view converter added since (`call_input_modified`, `ViewConverter`): `Appended<K, A>`/`Removed<K, A>` keyed by `SyntaxElement` or `NodeId`, `Inserted`, `current_leaves` a `SmallVec` popped with `remove(0)`; hashbrown's `nightly` is in the root already; callers listed, budget test given.

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:39): in the tree: crates/syntax-bridge/src/lib.rs:28-32 (`Inserted`, `Appended<K, A>`, `Removed<K, A>`), :220 generic in `A`.

### F6. `AstIdMap::from_source`: as `mem-format` filed it

`v2/requests/mem-format.md`, the `AstIdMap::from_source` section (line 905 on): the walk in its
own `frame::task(Tag::Parse, ..)`, `AstIdMap::new(&entries, bodies)` boxing at the exact length.
That is everything E6 needs; no `from_source_in` is asked.

> mem-format-expand: merged into `v2/requests/mem-format.md` section 4 (mem-format's own `AstIdMap` request), which is with frontend-tree-span (it holds `crates/span/src/ast_id.rs`, item 1 of its brief). Nothing more to file.

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:39): in the tree: crates/span/src/ast_id.rs:1250-1262 (`from_source` in `frame::task(Tag::Parse, ..)`).

### F7. `mbe`: the matcher and transcriber in the frame; rules exact

Builds on `v2/requests/mem-format-tokens.md` ("an expansion's matcher and transcriber in the
task's frame"), which splits `expander::expand_rules` into a wrapper that opens
`frame::task(Tag::Expand, ..)` and `expand_rules_in(frame, ..)` with the arena, matches and
nesting stack in `frame`. Asked on top of it: `expand_rules_in` returns its output in the frame
it is given, `ExpandResult<(tt::TopSubtree<&'t Frame>, MatchedArmIndex)>` (the transcriber's
builder ends `build_in(frame)`, F1), and is reached as
`DeclarativeMacro::expand_in<'f>(&self, frame: &'f Frame, db, tt: &tt::TopSubtree, marker,
call_style, call_site) -> ExpandResult<(tt::TopSubtree<&'f Frame>, MatchedArmIndex)>` (E5). The
wrapper `expand`, for callers outside a run, copies the framed tree out before its task ends:
`expand_rules_in(..).map(|(tt, arm)| (tt::TopSubtree::from_subtree(tt.view()), arm))`, one
exact copy (F9) where today the builder's output is the result.
`parse_macro_rules`/`parse_macro2` build `rules` (today `Vec::new()` grown per rule,
"alloc: the rules, once per rule") in a frame vector and box them at their exact length
(`Box::from_iter` over an exact-size drain), the memo's.

> mem-format-expand: merged into `v2/requests/mem-format-tokens.md`, the addendum to "an expansion's matcher and transcriber in the task's frame" (that section is applied, `Buffers` rename included). `expand_rules_in(frame, out: A, ..) -> TopSubtree<A>` and `transcribe(.., frame, out, ..)` build the output in the allocator given, so the global wrapper passes `Global` and makes no copy (F7's copy-out dropped); `expand_in(frame, ..)` runs the match in a nested task with the output in the caller's frame; rules collected in a frame and boxed exact. Forwarded to frontend-tokens, after section 1 of `mem-format-expand.md`.

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).

### F8. `blaze-radix`'s scratch

None: `sort_copy_by_key_in(items, key, scratch: &mut Vec<T>)` exists (`blaze-radix/src/lib.rs`);
E7 and E8 pass a `Scratch<T>`'s vector. Listed so nobody files it.

> mem-format-expand: applied (nothing to change: checked `crates/blaze-radix/src/lib.rs:196`, `sort_copy_by_key_in` exists as F8 says; blaze-radix's holders, fix-radix and its children, are DONE).

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).

### F9. `tt`'s `finish`: every column at its exact length

`crates/tt/src/storage.rs`, `TopSubtreeBuilder::finish` (line 870): `nodes` and `spans` are
already allocated at their length. The encoder's three tables are not:
- `Table::new()` (line 206): `parts.list` is `Vec::new()`, grown by doubling from its first
  part (capacity 4 for 1 part), and `parts.map` an `FxHashMap` allocated past 8 parts, both per
  tree;
- `Encoder::new` (line 267): `wide` is `Vec::new()` grown by doubling; `symbols.list` is
  `Vec::with_capacity(symbols.len().min(len))`, an upper bound (repeats in the window share an
  index, so the list is shorter);
- `finish` then calls `into_boxed_slice` on each, a shrink realloc whenever capacity exceeds
  length, which for `parts` and `symbols` is nearly every tree.

Change: the encoder's buffers join the builder's worker `Scratch` (line 674):
```rust
struct Scratch {
    nodes: Vec<Node>,
    spans: Vec<Span>,
    closes: Vec<Span>,
    symbols: Vec<Symbol>,
    unclosed: Vec<u32>,
    parts: Vec<CompressedSpanPart>,
    part_map: FxHashMap<CompressedSpanPart, u32>,
    wide: Vec<WideSpan>,
    out_symbols: Vec<Symbol>,
}
```
(`give_back` clears the four new ones too, under the same `KEEP` bound). `TopSubtreeBuilder`
holds them from `Scratch::take` in `with_capacity`; `Encoder` borrows them
(`Encoder<'s> { parts: Table<'s, CompressedSpanPart>, last_part, wide: &'s mut Vec<WideSpan>,
symbols: Symbols<'s> }`, `Table<'s, T> { list: &'s mut Vec<T>, map: &'s mut FxHashMap<T, u32> }`,
`Symbols<'s> { list: &'s mut Vec<Symbol> }`; `index` unchanged). `finish` (and F1's `finish_in`)
writes each column once from its slice:
```rust
fn exact_in<T: Copy, A: Allocator>(items: &[T], alloc: A) -> Box<[T], A> {
    let mut out = Vec::with_capacity_in(items.len(), alloc);
    out.extend_from_slice(items);
    out.into_boxed_slice()
}
```
(`Vec::with_capacity_in(len, ..)` then `into_boxed_slice` at `len == capacity` does not
reallocate; an empty slice allocates nothing.) `Symbol` is `Copy` in `intern` (read before
applying; if it is only `Clone`, `extend_from_slice` takes `Clone` with the same cost). The
`// alloc:` comments on lines 207 and 271 go; `finish`'s output columns keep theirs.
Why: `macro_arg`'s and `expand_proc_macro`'s memo trees (M), and every expansion tree built in
a frame under F1, which today costs up to three reallocations per tree on top of its columns.
Test: hir-expand's block 8 `an_argument_costs_a_constant_number_of_allocations` lowers to
`small <= 5` (one per non-empty column, no shrink); and in `crates/tt/src/storage/tests.rs`
(format's; tt's counting allocator is `src/lib.rs:22`):
```rust
/// A tree of 64 idents over 8 symbols and one span part: its four non-empty columns, each
/// allocated once at its length, nothing grown or shrunk (warm: the builder's scratch is grown).
#[test]
fn a_built_tree_allocates_its_columns_alone() {
    let span = Span {
        range: TextRange::at(TextSize::new(0), TextSize::new(3)),
        anchor: anchor(1, 0),
        ctx: SyntaxContext::root(Edition::CURRENT),
    };
    let symbols: Vec<Symbol> = (0..8).map(|i| Symbol::intern(&format!("budget_{i}"))).collect();
    let build = || {
        let mut builder = TopSubtreeBuilder::new(Delimiter::invisible_spanned(span));
        for i in 0..64usize {
            builder.push(Leaf::Ident(Ident { sym: symbols[i % 8], span, is_raw: IdentIsRaw::No }));
        }
        builder.build()
    };
    let warm = build();
    let (again, used) = blaze_alloc_count::measure(build);
    assert!(again == warm);
    // nodes, spans, parts, symbols; no wide span; no shrink.
    assert!(used.allocations <= 4, "{used:?}");
}
```

> mem-format-expand: already true in the code, nothing to apply. tt's one-block rewrite (after F9 was filed) keeps the encoder's parts table, wide spans and symbols in the builder's per-thread `Scratch` (`storage.rs:878`), and `from_columns` allocates the tree's one block at its exact length. `storage/tests.rs:846` ("building a tree with a warm builder", at most 1) is the budget F9's test asked for. Whether hir-expand's block-8 budget (`small <= 12`) can be lowered is db-expand-macros' to see when it is built.

---

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).

## X. Outside hir-expand and format

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:39): X1 forwarded (below), X2 covered by `ProcMacroCall`.

### X1. `crates/blaze-diagnostics/src/lints/syntax/early/macros.rs` (lane `diagnostics`)

`dropped_semicolon` calls `DeclarativeMacroExpander::expand`, which takes a frame (E5). Old
```rust
    let expanded = def.decl_macro_expander(db, loc.def.krate).expand(db, arg, call, *span);
    if expanded.err.is_some() {
        return None;
    }
    match expanded.value.0.token_trees().iter().last()? {
```
new (the lint runs in a batch task or none; it opens its own, which nests):
```rust
    blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
        let expanded = def.decl_macro_expander(db, loc.def.krate).expand(db, frame, arg, call, *span);
        if expanded.err.is_some() {
            return None;
        }
        match expanded.value.0.token_trees().iter().last()? {
            /* the arms unchanged */
        }
    })
```
(`*span` becomes `span` if block 1 of `db-expand-macros.md` has landed, as that block's request to
diagnostics says.)

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).
> forwarded by db-audit-mem (22:39): to diagnostics in `v2/requests/db.md` ("diagnostics (diagnostics-next2; from db-audit-mem): `dropped_semicolon` expands in a frame"; told by tmux): it was not in `diagnostics.md`. Restated against the tree: `expand` stays global (E5), so the lint calls the framed `expand_making(db, frame, ..)` inside `frame::task(Tag::Expand, ..)`.

### X2. `ProcMacroExpander::expand(.., current_dir: &str)` (E4)

The implementations: `crates/load-cargo/src/lib.rs:913` (`Expander`, whose
`proc_macro_api::ProcMacro::expand(.., current_dir, ..)` takes a `String` for the wire: it passes
`current_dir.to_owned()` there, `// alloc: the request's text, at the edge to the server`), and
the nine in `crates/test-fixture/src/lib.rs` (916, 939, 975, 999, 1036, 1075, 1120, 1151, 1205),
whose parameter is unused: `_: &str`.

> db-expand-macros (owner of `crates/hir-expand/`), 2026-10-29:
> - **Applied:**
>   - H1 to H3, see `v2/requests/mem-db-def.md`.
>   - speed-hir-sol-expand's blocks 1 to 8, which E2 and later start from.
>   - E7: `RANGE_SORT`, the worker's `Scratch<TextRange>` in `lib.rs`, with `sort_copy_by_key_in`.
>   - E9: `line_starts` counts the newlines, then allocates once at the exact length.
> - **Waiting, because what they program against doesn't exist yet:**
>   - F1, `tt::TopSubtree<A>` with `empty_in`, `from_token_trees_in`, `from_subtree_in` and `invisible_from_leaves_in`. That holds E2, E5, E10, E11, E12 and E14's `find_derive_range`.
>   - F2 to F6: views for the parse and `ranges_with_span`, `ExpansionSpanMap::clear`, `from_file(&[..])`, `RealSpanMap::absolute_span`, `syntax_node_to_token_tree_in` and `token_tree_to_syntax_node_in`, and `AstIdMap::from_source_in`. These hold E3, E6, E8, E10, E13 and E14.
>   - E12's string half names `blaze_mem::scratch::with_string`/`with_bytes`, which `scratch.rs` doesn't have. It has `with::<B: Shelved>`, `with_path` and `Scratch<T>`. Say which to use.
>   - T1 and T2 assert what those changes make true: framed trees, `ArgMap`, a downmap that no longer allocates, and frame-only misses. They'd fail or not compile before them, so they land with them.
> - **E4 isn't applied, because X2 is incomplete.** Changing `ProcMacroExpander::expand`/`expand_in` to `current_dir: &str` has to land together with every implementation. Besides `load-cargo` and `test-fixture` there are nine more:
>   - `blaze-macro-wasm/src/lib.rs`, `blaze-expand/src/lib.rs`, `blaze-wasm/src/pack.rs` and `pack/record.rs`;
>   - `hir-ide/src/const_macro.rs` and `tests/derived_impls.rs`;
>   - `hir-def/src/nameres/facts/tests.rs`, `nameres/tests/macros.rs` and `macro_expansion_tests/mod.rs` (that last one is mine).
>
>   Name their owners in X2 and I'll change the trait and my test's implementation in the same landing.
> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).

> not applied by db-mem-def (21:31): waits on format's F1 to F4 (`tt::TopSubtree<A>`, `empty_in` and the other `_in` constructors, `SpanMap::clear`, `RealSpanMap::absolute_span`; none in `crates/tt` or `crates/span` yet), filed through `mem-format-expand.md` with frontend-tokens and frontend-tree-span. `frame::current` and `scratch::with_string`/`with_bytes` exist now, which unblocks E12's string half and E8's anchor list; db-expand-macros-next2 holds `crates/hir-expand/` and is told (tmux 21:31). E4's `current_dir` is now db-apply-def's `ProcMacroCall` (`proc_macro.rs`), whose implementations are filed in `usercode.md`, `cargo.md` and `coordinator.md`, so X2 is covered. X1 is filed in `diagnostics.md` (`fn dropped_semicolon`). E7 and E9 were applied by db-expand-macros.
