---
status: plan
summary: blaze v2 lane fastcompile, ideas 9 and 10 (semantic edits, declared intent): crates/blaze-intent's edits by meaning applied to the flat tree in place, the byte-equality argument against the text edit, rename and path replacement (`blaze check --replace`) through the front end's resolution, what each edit reports changed, the agent API's Edit, WillTest and WillSave, the session's take-over and cancel rules, the plane's idle class requested from core, the hooks requested from ci, core, db and cargo, the tests, and the state of the art.
date: 2026-10-29
---
# fastcompile: semantic edits and declared intent (`crates/blaze-intent`)

Ideas 9 and 10 of [compile-time.md](../../design/compile-time.md). An agent says what it changes
by meaning (`Edit`), and what it is about to do (`WillTest`, `WillSave`), so the daemon
reparses only what changed and builds ahead what the next request needs. The code is in
`crates/blaze-intent`. The API's half is in `crates/blaze-api` (`intent.rs`,
`session/intent.rs`). What other crates must do is requested in `v2/requests/fastcompile.md`
under `fastcompile-intent <n>`.

## Edits by meaning

`Request::Edit { base, head, edits }` names items by path within a file: `Shape::area`,
`<Circle as Shape>::area`, `fn area@1`, a tuple field by its index (`blaze_intent::path`). The
edit kinds are:

- `ReplaceBody`, `ReplaceSignature` and `ReplaceItem`;
- `AddField` and `RemoveField`;
- `AddItem` and `RemoveItem` (an item, a field or a variant);
- `AddUse`;
- `Rename`;
- `Replace { from, to }`, which names no file: two def paths from a crate's name.

The daemon's job (`Work::Edit`) calls `blaze_intent::apply` on the view's lowered front end.

- Each edit finds its node in the file's flat tree and writes one splice
  (`file::Working::splice`). The tree is then reparsed only where the splice falls
  (`Parse::reparse`).
- The output is the VFS's `Op::Edit`s, one per splice, in file order, plus a renamed module's
  file moves. With them comes each touched item, with its `How`, its path, and its `ItemKey`
  in the view before the edit.
- The session commits the ops to `head` as a `Commit` would, after `Response::Edited`.

**Same bytes as the text edit.** A splice is a byte range and its replacement text, so the
bytes committed are exactly those of the text edit that replaces that range. The tests build
the equivalent text edit by hand for every kind and compare the VFS roots. Where an edit has
a choice to make (where to insert, how to indent, a trailing comma, a blank line), it takes
the file's own style and a fixed rule. The rule is the one a person would write, and the
tests hold it. The reparsed tree must equal a fresh parse of the new text: tree shape and
sorted errors are checked for every edit. Only error-free trees are handed to the front end
as seeds, since error order can differ.

**Changed items.** Each edit reports what it touched at item level.

- A body edit is `Body`. A signature edit is `Signature`. A field list edit is `Fields`.
- Replacing an item is `Whole`. `Added`, `Removed` and `Renamed` are what they say.
- A container whose member list changed (module, impl, trait, file) is `Scope`.

The test oracle diffs every item's own text, with its members cut out, before and after the
equivalent text edit. The set it finds equals the reported set for every kind. Keys come from
the base view, so invalidation can start from the named items (`blaze_items::resolve`), not
from a text diff.

**Rename resolves, never searches text.**

- The files searched are those of the definition's crate, plus every crate that depends on it
  if the definition is public. A `memchr` finder over the base text only prefilters them.
- Each identifier token with the old name is classified through `Semantics`: definitions,
  paths per namespace, method calls, fields, record shorthands, associated type bindings,
  `offset_of!`, `extern crate`.
- A shorthand (`Point { x }`, pattern or expression) is written out as `x: new` or `new: x`, so
  the binding keeps its name.
- An out-of-line module's file moves, `m.rs` and `m/` or `m/mod.rs`. A `#[path]` module's
  file stays where it is.
- **No-fallback.** When the old name occurs where the front end can't say what it means, the
  rename stops with an error naming `path:line`, and nothing is skipped. That covers a macro
  token tree, an unresolved path, a token a macro uses both for the item and for something
  else, and renaming an item that implements a trait method ("rename it in the trait").
  Renaming the trait method renames every impl of it and every call.

**Replace retargets by resolution, never by text** (`blaze_intent::replace`, `blaze check
--replace std::sync::Mutex=parking_lot::Mutex`). It is the tool for moving a type or path
(the locks and hasher ports); astlog stays the syntax tool.

- `from` and `to` are def paths: a crate's name, then the item's path in it, resolved from the
  crate's root (re-exports followed). Of several crates by one name, the one the view's crates
  depend on. A module, a macro, a builtin type, a path that names nothing, or a `to` missing a
  namespace `from` has, is an error.
- The files are those of the view's crates that can name `from` (its crate and those depending
  on it), prefiltered by a `memchr` finder for every name `from` goes by: its own, the path's
  last segment, and each a dependency outside the view exports it under
  (`hir_def::import_map`, so `use std::alias::Lock` is found). A file of such a crate outside
  the view that holds a name is an error.
- Each file is rewritten by `blaze_intent::retarget`, the rewrite the lints' fixes share: a
  `use` of `from` becomes an import of `to` of its own (its attributes and visibility kept, and
  under the old name when the new one differs: `use parking_lot::Mutex as Lock;`), the other
  leaves of its list staying; a glob of a module outside the view gets an import of `to` beside
  it when its scope names `from`; a path that qualifies `from` is rewritten up to its name,
  generic arguments kept.
- A request's replaces are made together in one pass over the same base: each file's retarget
  moves the items of all of them, so `use std::sync::{Mutex, RwLock};` under two replaces is
  one rewrite. An item given twice (by any of its paths), or a chain (one's `to` another's
  `from`), is an error.
- `to` is written as the crate naming it sees it: `crate::…` in its own crate, else under the
  name the crate's manifest gives the dependency, checked to resolve to `to` at the crate's
  root. A crate that has no such dependency is an error ("add it to the crate's manifest").
- A path through one of the view's own imports (`crate::locks::Mutex` where `locks` has
  `pub use std::sync::Mutex;`, `use lib::locks::Mutex;` in a dependent) stays: the import
  moves. So does a path of one segment bound by an import of the view.
- What one file's tree can't show is done here, after every name's token is classified
  through `Semantics` (macros descended): a path a macro expands from its input is rewritten
  at the input's range; a path of one segment reaching `from` through the prelude gets an
  import of `to` in its module; one reaching it through a chain of the view's globs gets the
  import beside the last glob of the chain, whose module is outside the view, with the glob's
  visibility, so every glob of it sees `to`.
- **No-fallback**, as rename: a name in a `macro_rules!` body or tokens no macro expanded, an
  unresolved or cfg'd-out occurrence, a `use` a macro expands, a path a macro builds from
  pieces of its input, a token a macro uses both for `from` and for something else, and a glob
  cycle each stop it with `path:line`. Nothing is written.
- `blaze check --replace` (`crates/blaze-cargo/src/cli/replace.rs`) opens the checkout's head, sends
  `Request::Edit` with one `Replace` a `--replace`, and writes the committed ops to the
  checkout's files (each read once, edited in memory, written whole through a file renamed
  over it); a path leaving the checkout or an edit past a file's end is refused before
  anything is written.

## Declared intent

`Request::WillTest { view, filter, targets, wait_ms }` and
`Request::WillSave { ops, head, test, targets, wait_ms }` are answered at once with
`Response::Announced { root }`. They start a job of no request through
`Builder::speculate`, which runs in the plane's idle class.

- **WillTest** (`Work::Ahead`) runs no test. It builds the test units holding a test the
  filter selects, using libtest's rule: the name contains a filter, or every test when there
  is no filter. The functions those tests reached when they last ran are compiled first, the
  ones more of them reached ahead of the rest (`blaze_intent::ahead::plan`).
- **WillSave** checks, or with `test` tests, the what-if of `ops` on `head`'s current root.

The session (`session/intent.rs`) keeps each intent's view open until its job finishes.

- **Taken over (promoted, once).**
  - A request that builds the intent's root incrementally takes it over: check, build,
    test, run, codegen inputs, trace, or a best-of candidate.
  - So does a commit to its head that makes its root, whether a commit request, an edit's
    commit, or the daemon's own watcher commit (`Session::head_moved`).
  - From then on the job runs at normal priority, and the request joins its in-flight nodes
    through the plane's single flight. The work is reused, not repeated.
- **Cancelled.** An intent not taken over is cancelled by:
  - a newer intent of the same connection;
  - a commit to its head that makes another root;
  - its connection's end;
  - `wait_ms` passing on the daemon's clock (`Session::tick`, `Session::deadline`).

  Cancelling a job frees its memory as any cancel does. A taken-over intent is never
  cancelled by these.
- **A request without intent behaves exactly as before.** Every new path starts from an
  intent in the table. With an empty table, no `speculate`, `promote` or extra `cancel` is
  ever called. The session's existing 200-seed simulation asserts this.

**Never delaying real work** is the plane's job (request 2). An idle request's task starts
only when no foreground task is ready and fits. When a foreground task is blocked only by
running idle tasks, the plane yields the newest of them. Without the yield, a running idle
task could hold a thread for up to one task's length.

## Hooks

| # | Owner | What | State |
|---|---|---|---|
| 1 | ci | API types, variants, `SCHEMA` 9, `Work::{Edit, Ahead}`, `Builder::{speculate, promote}`, session handling and tests, blaze-ui lines | written by fastcompile-intent (ci DONE) |
| 2 | core | idle class, `Promote`, `Yield` in the plane | requested |
| 3 | db | a parse seed taken by `EditionedFileId::parse`, keyed by `FileText` content and edition | requested |
| 4 | cargo | `Jobs`' `speculate`/`promote`, `Work::Edit` and `Work::Ahead`, `tick`/`deadline`/`head_moved`, `heads.rs`, the dependency | requested |

## Tests (beside the code)

- `blaze-intent/src/tests/edits.rs`: every edit kind against its hand-made text edit (same
  bytes and VFS root, same changed items by the oracle), a run of several edits in a row,
  fresh-parse equality, and the error cases.
- `blaze-intent/src/tests/rename.rs`: rename over a two-crate module graph. It covers a
  function across crates, a field with shorthands written out, a module with its file moved,
  an associated function followed by a body edit, a trait method reaching its impls and
  calls, and the refusals (a macro body, an impl of a trait method). Keys are checked
  through `blaze_items::resolve`.
- `blaze-intent/src/tests/replace.rs`: std's `Mutex` to parking_lot's over two crates of the
  view (`lib` and `app`) beside std and parking_lot outside it, with each import form: a
  `use`, a list, a rename, a glob, a block's glob, a qualified path, a path through a module, a
  re-export of the view and a glob of it, the prelude, an alias of the dependency's, a macro's
  input, and a struct of the same name that stays. The bytes equal hand-written text edits
  (and the VFS root theirs), seeds equal fresh parses, and in a front end of the new text every
  name that named std's lock names parking_lot's, the other stays, no `Unresolved*` or
  `TypeMismatch` diagnostic appears, and a second replace has nothing to move. Refusals: a
  crate without the dependency, a `macro_rules!` body (`lib.rs:4`), and bad def paths. Two
  replaces (`Mutex` and `RwLock`) sharing one `use` list make one rewrite of it; an item
  replaced twice and a chain are refused.
- `blaze-cargo/src/cli/fix.rs`: the `--replace` words, and ops written to files in order with
  a path outside and an edit past the end refused before writing.
- `path.rs`, `file.rs` and `ahead.rs` have their own unit tests.
- `blaze-api/src/session/tests.rs`:
  - an edit commits its reported ops once durable, and a failed, cancelled, silent or gone
    edit commits nothing;
  - intent take-over by request and by commit;
  - each cancel path (a newer intent, another root, the connection's end, the wait);
  - a 200-seed simulation of intents, commits, clock ticks, finishes and connections ending:
    each intent is taken over or cancelled at most once, never both, never after its end,
    and every view is freed;
  - the request-without-intent assertion.
- `blaze-api/src/frames/identity.rs` holds every new variant to the reference codec.

## State of the art

- **rust-analyzer.**
  - Its rename (`ide-db` `rename` and `search`) also classifies text matches through
    `Semantics`. It edits with `TextEdit`s through the client, and it falls back to text in
    macros. Here a macro occurrence it can't resolve is an error.
  - Its incremental reparse (`syntax::parsing::reparsing`) relexes a token or reparses a
    `{}` block. blaze-intent uses the same `Parse::reparse`, and hands the result to the
    front end instead of discarding it.
- **Roslyn.** `Renamer` and `Solution.WithDocumentSyntaxRoot` apply semantic edits to an
  immutable syntax tree. `GetSpeculativeSemanticModel` answers what-ifs without committing.
  blaze's what-if is a VFS root, so a speculative build and a real one share their results
  by content.
- **Unison.** It stores definitions by content hash, so a rename touches only names. blaze
  keeps Rust's text as the source of truth but reports changes by `ItemKey`, so invalidation
  also starts from definitions.
- **LSP `textDocument/willSave`.** The editor-side precedent for `WillSave`. It carries no
  content and nothing builds on it; `WillSave` carries the ops, so the daemon can build the
  exact root before it exists.
- **Speculative builds.** IDE background compilers and watch modes (rust-analyzer flycheck,
  dune watch) rebuild after a save, and CI merge queues build ahead of merge. Here the build
  starts before the save, in a strict idle class, and is promoted, not restarted, when the
  save lands.

## Known limits

- An occurrence in code the lowered graph doesn't analyse is an error rather than skipped.
  An example is `cfg(test)` code when the test crate isn't lowered. Lowering more of the
  graph removes these errors; skipping them would be a quiet wrong rename.
- Without request 3, the first query after an edit parses the edited file again. The bytes
  are still correct; only the cost idea 9 removes is paid.
- Without request 2, a speculative job runs as an ordinary weighted request. Until then the
  daemon must not implement `speculate` as `start`: it stops with an error naming the
  missing idle class.

## Speed of light

Nothing here was run: costs are counted from the code and the machine. The machine is that of
[link's speed of light](../link/speed-of-light.md#the-machine): 3.5 GHz, L2 streaming at
32 bytes a cycle (about 110 GB/s read, about 50 GB/s for a copy, which reads and writes), one
core from DRAM at about 25 GB/s, BLAKE3 at about 6 GB/s. A salsa memo hit is about 30 ns, as in
[share.md](share.md#speed-of-light). The lexer runs at about 1 GB/s
([mem/codegen.md](../mem/codegen.md)).

There are three hot paths:

- one edit applied to a file's flat tree, which should be bound by the bytes it changes;
- a rename, which should be bound by the references it finds;
- a declared intent, which should be bound by the work it starts and by how fast that work
  stops once dropped.

### Unit costs

| step | cost |
|---|---|
| copy or compare bytes held in L2, per byte moved | about 0.02 ns |
| lex and parse a block, and build its subtree | about 5 ns a byte (about 25 ns a token of about 5 bytes) |
| scan for a name (`memchr::memmem`), file bytes from DRAM | about 0.04 ns a byte |
| down from the root to the element at an offset: `covering_element`, a binary search of each level's children, about 10 levels | about 100 ns |
| one member of a container in a path step (find its `NAME` child, compare the text) | about 15 ns |
| resolve one occurrence of a name (`descend_into_macros_exact`, then `resolve_path`, `resolve_method_call` or `resolve_field`, warm) | about 1 µs |
| the validation walk of `Parse::errors`, per element | about 3 ns |
| a reached hash counted: read 32 bytes from DRAM, update its entry in an L2-resident table | about 6 ns |

### An edit

**The work.** Take a file of \(N\) bytes whose tree holds \(E \approx N/3\) elements, at about
6 bytes of columns each, so about \(2N\) bytes of tree. An edit names an item through a path
of containers with \(I\) members in all, and changes \(B\) bytes of a token, block or body.

- The item must be found: \(15I\) ns, without an index of the file's names.
- The new text and tree are a new immutable version: the base is the front end's, which salsa
  keeps. With one contiguous text and one flat tree per file
  ([decisions.md](../../process/decisions.md), #42), that is the text once (\(2N\) bytes
  moved) and the columns once (\(4N\) bytes moved). A persistent tree (rowan's green nodes,
  Roslyn's, tree-sitter's) copies only the path to the root. The flat tree pays \(6N\)
  instead, so that every read after it is a column load.
- The unit the edit falls in must be relexed and reparsed: \(5B\) ns.
- Each touched item is named and keyed: its path (\(15I\) ns for the siblings its steps
  count) and a few memo hits (about 0.5 µs).

So the bound for one edit in one file is

\[ T_{\text{edit}} \approx 30\,I + 0.02 \cdot 6N + 5B + 500 \ \text{ns}. \]

Take a 40 KiB workspace file (about 1,000 lines, 13,000 elements) with 100 top items, and a
`ReplaceBody` of a 1 KiB body. Then \(T_{\text{edit}} \approx 3 + 4.9 + 5 + 0.5 \approx
13.4\ \mu s\).

**What the code did before this section** (counted from the code):

- `Working` kept its own `String` of the whole text: a copy at open (\(2N\)) and a
  `replace_range` per splice (about \(N\) moved).
- `finish` made every seed whether anyone took it or not. That is a BLAKE3 of the file
  (\(N/6\) GB/s, 6.8 µs) and `errors()`, the validation walk of the whole tree (\(3E\) ns,
  about 40 µs).
- `item_at` found an item through `token_at_offset`, which scans every child at each level:
  about 1.2 µs per touched item for 400 top-level children.
- With `syntax`'s own excess (below, 4.9 µs), the total is about
  \(13.4 + 4.9 + 2.4 + 6.8 + 40 + 1.2 \approx 69\ \mu s\), five times the bound.

**What the code does now:**

- `Working` holds no text. `text()` is the tree's own source, which the reparse makes once
  (test `the_text_is_the_trees_never_a_copy`).
- Opening a file compares nothing when the view's bytes and the front end's text are the same
  memory (`begins_with`, then `Text::ptr_eq`).
- `Applied::seeds()` is lazy: the hash and the walk are paid only by a caller that seeds. The
  daemon takes only `edited`, so today it pays neither.
- `item_at` goes down by binary search (`covering_element`), about 100 ns.
- `open_id` is a hash lookup, not a scan of the files opened.
- The reads `text`, `bytes`, `to_now`, `to_base`, `root` and `base` allocate nothing (test
  `reads_and_position_maps_allocate_nothing`).

blaze-intent's own part is now the path, the naming and the ops: at the bound. What is left
over the bound is inside `syntax`'s `Parse::reparse`. It moves about \(12N\) bytes where the
bound is \(6N\): the new text, a compare of the new text's prefix and suffix against the old
(`assert!`), and the columns built in `Buffers` and copied again by `pack`. That is about
\(18.3\ \mu s\) against \(13.4\), 1.37 times. Request `fastcompile-sol-intent 1` asks format
to write the columns once and make the compare a debug assertion, which brings an edit to
within 10%.

### Several edits, and rename

A request with \(k\) splices in one file reparses \(k\) times, so it pays the \(6N\)-byte
version (today \(12N\)) \(k\) times where the bound pays it once. For \(k\) of 1 to 3, as
an agent's edits mostly are, the excess is \(k - 1\) versions: 5 to 10 µs at 40 KiB. A
rename writes every reference as a splice, so \(k\) is the number of references in the file.

**The rename's work.**

- Scan the bytes of every file of the crates that can see the item (\(\sum N_f\), from DRAM).
- Find the token at each match: \(H\) matches, 100 ns each.
- Resolve each identifier occurrence: \(h\) of them, 1 µs each.
- Write each file with references once: \(6N_f\) bytes moved, plus relexing a token per
  reference.

\[ T_{\text{rename}} \approx 0.04 \textstyle\sum N_f + 100\,H + 1000\,h + 0.12 \sum_{f \in F_h} N_f \ \text{ns}. \]

Take a public function renamed in a crate of 200 files of 20 KiB (4 MiB), with 15 references
in 5 files. Then \(T_{\text{rename}} \approx 168 + 1.5 + 15 + 12 \approx 197\ \mu s\). A file
already in L2 scans at about 110 GB/s, so a warm scan is about 38 µs.

**What the code did before:**

- Each file whose bytes held the name was walked token by token
  (`descendants_with_tokens`), about \(3E_f\) ns, or 21 µs for a 20 KiB file. That is 25
  times its scan.
- Each candidate file's path was copied into a `String`.
- Each reference paid a full reparse, \(12N_f\).
- Total: about \(168 + 105 + 15 + 74 \approx 362\ \mu s\).

**What the code does now:**

- Each match of the name goes straight to its token (`rename::ident_at`: `covering_element`
  over the match, an identifier whose name, `r#` cut, is exactly the match). So a file costs
  its scan and its matches, not its nodes.
- A non-overlapping search reaches every token, because a name never overlaps itself at a
  token's start (what precedes a token's start is no identifier character). The test
  `the_names_matches_reach_every_identifier_of_it_and_nothing_else` checks this against the
  walk, on a full parse and on an outline whose bodies are lazy.
- Paths are borrowed from `Files`, not copied.
- Total: about \(168 + 1.5 + 15 + 74 \approx 259\ \mu s\), 1.3 times the bound. All of the
  excess is the per-reference reparse, which `Parse::reparse_all` (request
  `fastcompile-sol-intent 1`) makes one call per file; the loop over `hits` in `rename.rs`
  becomes that call.

### Declared intent

**Announcing.** `WillTest` and `WillSave` are answered with `Announced` after
`Intents::add`. That cancels the connection's open intents (a scan of a few) and pushes one
entry, well under a microsecond. Everything else is the job it starts, through
`Builder::speculate`, in the plane's idle class (`blaze-graph` `plane.rs`). Nothing in the
request path waits on that job.

**Planning a `WillTest`** (`ahead::plan`). Take \(T\) tests, \(F\) filters, and \(R\)
reached hashes over the selected tests, \(D\) of them distinct.

- Matching costs one SIMD substring search per test and filter.
- Counting costs \(6R\) ns: each hash is read once and counted in a table.
- Ordering is a radix sort of \(D\) items.

Today `reached` is empty for every test, because blaze-testsel's record waits on codegen's
`ExecTable` (fastcompile-ident-select 7). So this matters only once that lands. The code is
at the bound:

- each filter's `memmem::Finder` is built once. `str::contains` builds a Two-Way searcher on
  every call, about 20 to 50 ns per test and filter;
- the count table is `blaze_ids::prehashed::ByHash`: a code hash is a hash already, so its
  first word indexes the table and nothing is hashed. FxHash over 32 bytes is about 2.5 ns
  more per reached hash, 40% over;
- the order (count descending, then hash) is a radix sort on the count and the hash's first
  four bytes, with the rare tied run compared in full (`hottest_first`, checked against the
  compare sort with forced ties). A compare sort costs about \(4 \log_2 D\) ns a hash, 64 ns
  at \(D = 50{,}000\); the radix sort costs about 10 ns.

For 2,000 tests reaching 1,000 functions each (\(R = 2 \cdot 10^6\), \(D = 5 \cdot 10^4\)),
that is about 13 ms, against about 21 ms before. The build it starts takes seconds.

**Stopping dropped work.** An intent not taken over is cancelled at the moment its cause
arrives:

- a newer intent, a commit to another root and the connection's end are each a scan of the
  open intents (`Intents::cancel_where`);
- the wait is checked on every daemon turn (`Session::tick`), and the loop wakes at
  `Session::deadline`.

The cancel itself is one store to the job's flag and a `Plane::cancel`, which skips every
node of the request that is not running.

The bound for the rest is one function's compile, 50 to 200 µs, since no design stops code
mid-function without checking for it inside the code. Today a running task sees the flag only
at its job's stage checks (`compile.rs`, `job.cancelled()`). Codegen's `stopped()` is polled
once per walk level, and only when a body check runs beside the build. The daemon also ignores
the plane's `Yield`. So a dropped speculative build holds its threads for up to one unit's
codegen (tens to hundreds of milliseconds), and a foreground task waiting for them waits that
long. That is 100 to 1,000 times the bound. Requests `fastcompile-sol-intent 3` (cargo: the
job's flag and `Yield` reach codegen's `stopped()`) and `4` (codegen: poll it per function)
bring it to the bound.

A cancelled idle task that nothing foreground waits for may run on. It uses only threads
nothing else wants, and what it compiles is stored by content, so a later root that shares it
reuses it.

### State of the art

- **Incremental reparse.** rust-analyzer (`syntax::parsing::reparsing`, rowan), Roslyn and
  tree-sitter (`ts_tree_edit`) relex a token or reparse a block, and rebuild only the path to
  the root of a persistent tree. blaze picks the same units (`Parse::reparse` is
  rust-analyzer's rule, plus lazy bodies) but pays one \(6N\) copy per file for a flat tree,
  whose every later read is a column load instead of a pointer chase. At 40 KiB the copy is
  about 5 µs, under a body's reparse. The edit's result also stays the front end's tree (a
  seed) where rust-analyzer's is discarded.
- **Rename.** rust-analyzer's `FindUsages` scans each file of the search scope for the name
  and maps each match to its token, then classifies it through `Semantics`. blaze-intent now
  does the same, with SIMD `memmem`. It beats rust-analyzer by refusing what it can't resolve
  instead of editing text in macros, and by scanning only the crates that can see the item. A
  trigram index of the workspace (zoekt, Sourcegraph) would make the scan proportional to the
  files that hold the name. That index is not built here; the scan runs at memory bandwidth.
- **Moving a path.** rust-analyzer has no "replace this path everywhere" assist; its
  structural search and replace (`ide-ssr`) matches a path pattern by resolution but edits
  only the matched paths, leaving `use` items, globs and the prelude to the user, and IntelliJ's
  "migrate" (Java's package and class migration maps) rewrites imports and qualified names
  together but by fully qualified name text. `blaze check --replace` resolves both ends, moves
  imports instead of paths through them, adds the import a glob or the prelude needed, and
  refuses what it can't place.
- **Speculation.** Watch modes and flycheck (rust-analyzer, dune, cargo-watch) build after a
  save, and merge queues build ahead of a merge. Here the build starts before the save, in an
  idle class, and is promoted, not restarted, when the save lands. With requests 3 and 4,
  dropping it gives every thread back within one function.
